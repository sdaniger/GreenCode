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
}

pub fn convert(system: &str, messages: &[Message]) -> Vec<Value> {
    let mut out = vec![json!({"role": "system", "content": system})];
    for m in messages {
        let mut text = String::new();
        let mut calls = Vec::new();
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
                Block::Other(_) => {}
            }
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

impl Provider for OpenAi {
    fn complete<'a>(&'a self, req: Request<'a>, on_text: &'a (dyn Fn(&str) + Send + Sync)) -> BoxFut<'a, Result<Reply>> {
        Box::pin(async move {
            let mut body = json!({
                "model": req.model,
                "messages": convert(req.system, req.messages),
                "stream": true,
                "stream_options": {"include_usage": true},
            });
            if !req.tools.is_empty() {
                body["tools"] = req
                    .tools
                    .iter()
                    .map(|t| json!({"type": "function", "function": {"name": t.name, "description": t.description, "parameters": t.schema}}))
                    .collect();
            }
            let resp = send(|| self.http.post(format!("{}/chat/completions", self.base)).bearer_auth(&self.key).json(&body)).await?;
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
            let cached = ev["usage"]["prompt_tokens_details"]["cached_tokens"].as_u64().unwrap_or(0);
            self.usage = Usage {
                input: n("prompt_tokens").saturating_sub(cached),
                output: n("completion_tokens"),
                cache_read: cached,
                cache_write: 0,
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
