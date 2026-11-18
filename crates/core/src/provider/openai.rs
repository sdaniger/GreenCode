use super::{send, sse, Provider, Reply, Request};
use crate::{Block, BoxFut, Message, Role, Usage};
use anyhow::{bail, Result};
use serde_json::{json, Value};
use std::collections::BTreeMap;

/// Any server speaking the OpenAI Chat Completions API.
pub struct OpenAi {
    pub http: reqwest::Client,
    pub base: String,
    pub key: String,
    /// Send GitHub Copilot's identity headers.
    pub copilot: bool,
    /// `reasoning_effort`, sent to api.openai.com only.
    pub effort: Option<String>,
}

pub fn convert(system: &str, messages: &[Message]) -> Vec<Value> {
    let mut out = vec![json!({"role": "system", "content": system})];
    for m in messages {
        let mut text = String::new();
        let mut calls = Vec::new();
        let mut images = Vec::new();
        for b in &m.content {
            match b {
                Block::Text { text: t } => {
                    if !text.is_empty() {
                        text.push('\n');
                    }
                    text.push_str(t);
                }
                Block::ToolUse { id, name, input } => calls.push(json!({
                    "id": id, "type": "function",
                    "function": {"name": name, "arguments": input.to_string()},
                })),
                Block::ToolResult { tool_use_id, content, .. } => {
                    out.push(json!({"role": "tool", "tool_call_id": tool_use_id, "content": content}))
                }
                Block::Other(v) if v["type"] == "image" && v["source"]["type"] == "base64" => images.push(json!({"type": "image_url", "image_url": {
                    "url": format!("data:{};base64,{}", v["source"]["media_type"].as_str().unwrap_or("image/jpeg"), v["source"]["data"].as_str().unwrap_or(""))}})),
                Block::Other(_) => {}
            }
        }
        // After the tool messages: they must directly follow the assistant's calls.
        if !images.is_empty() {
            out.push(json!({"role": "user", "content": images}));
        }
        match m.role {
            Role::User if !text.is_empty() => out.push(json!({"role": "user", "content": text})),
            Role::Assistant if !text.is_empty() || !calls.is_empty() => {
                let mut v = json!({"role": "assistant", "content": text});
                if !calls.is_empty() {
                    v["tool_calls"] = json!(calls);
                }
                out.push(v);
            }
            _ => {}
        }
    }
    out
}

impl OpenAi {
    pub fn body(&self, req: &Request) -> Value {
        let mut body = json!({
            "model": req.model,
            "messages": convert(req.system, req.messages),
            "stream": true,
            "stream_options": {"include_usage": true},
        });
        let host = reqwest::Url::parse(&self.base).ok().and_then(|u| u.host_str().map(String::from));
        let first_party = host.as_deref() == Some("api.openai.com");
        // DeepSeek rejects more output tokens than it supports and caches prefixes automatically.
        let deepseek = host.as_deref().is_some_and(|h| h == "api.deepseek.com" || h.ends_with(".deepseek.com"));
        let max_tokens = if deepseek { req.max_tokens.min(8_192) } else { req.max_tokens };
        body[if first_party { "max_completion_tokens" } else { "max_tokens" }] = json!(max_tokens);
        if let Some(effort) = self.effort.as_ref().filter(|_| first_party) {
            body["reasoning_effort"] = json!(effort);
        }
        if first_party {
            let version = req.model.strip_prefix("gpt-").unwrap_or("").split('-').next().unwrap_or("");
            let mut parts = version.split('.').map(|p| p.parse::<u32>().unwrap_or(0));
            let (major, minor) = (parts.next().unwrap_or(0), parts.next().unwrap_or(0));
            if major >= 6 || (major == 5 && minor >= 6) {
                // Keep the shared instructions reusable even when conversation suffixes diverge.
                body["prompt_cache_options"] = json!({"mode": "implicit"});
                if !req.system.is_empty() {
                    body["messages"][0]["content"] = json!([{"type": "text", "text": req.system,
                        "prompt_cache_breakpoint": {"mode": "explicit"}}]);
                }
            } else {
                body["prompt_cache_key"] = json!("greencode");
            }
        }
        if !req.tools.is_empty() {
            body["tools"] = req
                .tools
                .iter()
                .map(|t| json!({"type": "function", "function": {"name": t.name, "description": t.description, "parameters": t.schema}}))
                .collect();
        }
        body
    }
}

impl Provider for OpenAi {
    fn complete<'a>(&'a self, req: Request<'a>, on_text: &'a (dyn Fn(&str) + Send + Sync)) -> BoxFut<'a, Result<Reply>> {
        Box::pin(async move {
            let body = self.body(&req);
            let identity = if self.copilot { super::copilot::headers(req.messages) } else { Vec::new() };
            let resp = send(|| {
                let r = self.http.post(format!("{}/chat/completions", self.base)).bearer_auth(&self.key).json(&body);
                identity.iter().fold(r, |r, (k, v)| r.header(*k, v))
            }).await?;
            let mut acc = Acc::default();
            sse(resp, |data| {
                if data == "[DONE]" {
                    return Ok(true);
                }
                if let Ok(ev) = serde_json::from_str::<Value>(data) {
                    if let Some(t) = acc.feed(&ev)? {
                        on_text(&t);
                    }
                }
                Ok(false)
            })
            .await?;
            Ok(acc.finish())
        })
    }
}

#[derive(Default)]
pub struct Acc {
    text: String,
    /// `deepseek-reasoner` streams its chain of thought here instead of `content`.
    reasoning: String,
    /// index -> (id, name, arguments)
    calls: BTreeMap<u64, (String, String, String)>,
    usage: Usage,
    stop: String,
}

impl Acc {
    pub fn feed(&mut self, ev: &Value) -> Result<Option<String>> {
        if !ev["error"].is_null() {
            bail!("API error: {}", ev["error"]);
        }
        if let Some(u) = ev["usage"].as_object() {
            let n = |k: &str| u.get(k).and_then(Value::as_u64).unwrap_or(0);
            let cached = ev["usage"]["prompt_tokens_details"]["cached_tokens"].as_u64()
                .or_else(|| ev["usage"]["prompt_cache_hit_tokens"].as_u64()).unwrap_or(0);
            let written = ev["usage"]["prompt_tokens_details"]["cache_write_tokens"].as_u64().unwrap_or(0);
            self.usage = Usage {
                input: n("prompt_tokens").saturating_sub(cached).saturating_sub(written),
                output: n("completion_tokens"),
                cache_read: cached,
                cache_write: written,
            };
        }
        let choice = &ev["choices"][0];
        if let Some(r) = choice["finish_reason"].as_str() {
            self.stop = match r {
                "tool_calls" => "tool_use",
                "length" => "max_tokens",
                "content_filter" => "refusal",
                _ => "end_turn",
            }
            .into();
        }
        for c in choice["delta"]["tool_calls"].as_array().into_iter().flatten() {
            let e = self.calls.entry(c["index"].as_u64().unwrap_or(0)).or_default();
            if let Some(id) = c["id"].as_str() {
                e.0 = id.into();
            }
            if let Some(name) = c["function"]["name"].as_str() {
                e.1.push_str(name);
            }
            if let Some(args) = c["function"]["arguments"].as_str() {
                e.2.push_str(args);
            }
        }
        if let Some(r) = choice["delta"]["reasoning_content"].as_str() {
            self.reasoning.push_str(r);
        }
        match choice["delta"]["content"].as_str() {
            Some(t) if !t.is_empty() => {
                self.text.push_str(t);
                Ok(Some(t.to_string()))
            }
            _ => Ok(None),
        }
    }

    pub fn finish(self) -> Reply {
        let mut blocks = Vec::new();
        if !self.reasoning.is_empty() {
            blocks.push(Block::Other(json!({"type": "thinking", "thinking": self.reasoning})));
        }
        if !self.text.is_empty() {
            blocks.push(Block::Text { text: self.text });
        }
        for (_, (id, name, args)) in self.calls {
            let input = if args.trim().is_empty() {
                json!({})
            } else {
                match serde_json::from_str::<Value>(&args) {
                    Ok(v) if v.is_object() => v,
                    _ => json!({"__invalid_json": args}),
                }
            };
            blocks.push(Block::ToolUse { id, name, input });
        }
        Reply { blocks, usage: self.usage, stop: self.stop }
    }
}
