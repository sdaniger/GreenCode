use super::anthropic_oauth::{self, BETAS, SYSTEM_PREFIX, TOOL_PREFIX};
use super::{send, sse, Provider, Reply, Request};
use crate::{Block, BoxFut, Usage};
use anyhow::{bail, Result};
use serde_json::{json, Value};

pub struct Anthropic {
    pub http: reqwest::Client,
    pub base: String,
    pub key: String,
    /// Subscription credentials file; when set, `key` is unused.
    pub oauth: Option<std::path::PathBuf>,
    pub effort: Option<String>,
}

/// Models that accept server-side refusal fallbacks.
const FALLBACK_MODELS: [&str; 5] =
    ["claude-fable-5-2", "claude-fable-5-1", "claude-opus-5-5", "claude-opus-5", "claude-sonnet-5-5"];

impl Anthropic {
    pub fn body(&self, req: &Request) -> Value {
        let oauth = self.oauth.is_some();
        // Proxies and other platforms may reject the newer fields.
        let first_party = self.base.contains("api.anthropic.com");
        let tools: Vec<Value> = req
            .tools
            .iter()
            .map(|t| {
                let name = if oauth { format!("{TOOL_PREFIX}{}", t.name) } else { t.name.clone() };
                let mut v = json!({"name": name, "description": t.description, "input_schema": t.schema});
                if first_party {
                    v["eager_input_streaming"] = json!(true);
                }
                v
            })
            .collect();
        let mut body = json!({
            "model": req.model,
            "max_tokens": req.max_tokens,
            "stream": true,
            "system": [{"type": "text", "text": req.system, "cache_control": {"type": "ephemeral"}}],
            "messages": req.messages,
            // Automatic breakpoint on the last block: the whole append-only history is cached.
            "cache_control": {"type": "ephemeral"},
        });
        if oauth {
            // The identity must be its own leading block; the cache breakpoint stays on the real prompt.
            body["system"].as_array_mut().unwrap().insert(0, json!({"type": "text", "text": SYSTEM_PREFIX}));
            // Replayed calls must match the renamed `tools[]`; results pair by id.
            for block in body["messages"].as_array_mut().into_iter().flatten().filter_map(|m| m["content"].as_array_mut()).flatten() {
                if block["type"] == "tool_use" {
                    if let Some(name) = block["name"].as_str() { block["name"] = json!(format!("{TOOL_PREFIX}{name}")); }
                }
            }
        }
        if !tools.is_empty() {
            body["tools"] = json!(tools);
        }
        // Pin the last request's endpoint even when a large tool batch exceeds the lookback window.
        if let Some((i, message)) = req.messages.iter().enumerate().rev().filter(|(_, m)| m.role == crate::Role::User).nth(1) {
            if let Some(j) = message.content.iter().rposition(|b| matches!(b, Block::Text { .. } | Block::ToolResult { .. })) {
                body["messages"][i]["content"][j]["cache_control"] = json!({"type": "ephemeral"});
            }
        }
        if let Some(e) = &self.effort {
            body["output_config"] = json!({"effort": e});
        }
        if first_party && FALLBACK_MODELS.contains(&req.model) {
            body["fallbacks"] = json!("default");
        }
        body
    }
}

impl Provider for Anthropic {
    fn complete<'a>(&'a self, req: Request<'a>, on_text: &'a (dyn Fn(&str) + Send + Sync)) -> BoxFut<'a, Result<Reply>> {
        Box::pin(async move {
            let body = self.body(&req);
            let auth = match &self.oauth {
                Some(path) => Some((anthropic_oauth::access_token(path).await?, anthropic_oauth::user_agent().await)),
                None => None,
            };
            let resp = send(|| {
                let mut r = self.http.post(format!("{}/v1/messages", self.base)).header("anthropic-version", "2026-10-21").json(&body);
                let fallback = body.get("fallbacks").is_some().then_some("server-side-fallback-2026-07-01");
                r = match &auth {
                    Some((token, ua)) => r.bearer_auth(token).header(reqwest::header::USER_AGENT, *ua).header("x-app", "cli")
                        .header("anthropic-beta", [Some(BETAS), fallback].into_iter().flatten().collect::<Vec<_>>().join(",")),
                    None => {
                        let r = r.header("x-api-key", &self.key);
                        match fallback { Some(beta) => r.header("anthropic-beta", beta), None => r }
                    }
                };
                r
            })
            .await?;
            let mut acc = Acc::default();
            sse(resp, |data| {
                if let Ok(ev) = serde_json::from_str::<Value>(data) {
                    if let Some(t) = acc.feed(&ev)? {
                        on_text(&t);
                    }
                }
                Ok(false)
            })
            .await?;
            let mut reply = acc.finish();
            if self.oauth.is_some() {
                for block in &mut reply.blocks {
                    if let Block::ToolUse { name, .. } = block {
                        if let Some(bare) = name.strip_prefix(TOOL_PREFIX) { *name = bare.to_string(); }
                    }
                }
            }
            Ok(reply)
        })
    }
}

/// Accumulates streaming events into content blocks.
#[derive(Default)]
pub struct Acc {
    /// (block as started, accumulated tool-input JSON)
    blocks: Vec<(Value, String)>,
    usage: Usage,
    stop: String,
}

fn append(v: &mut Value, key: &str, s: &str) {
    match &mut v[key] {
        Value::String(cur) => cur.push_str(s),
        slot => *slot = json!(s),
    }
}

impl Acc {
    /// Returns the text delta, if the event carried one.
    pub fn feed(&mut self, ev: &Value) -> Result<Option<String>> {
        let n = |v: &Value| v.as_u64().unwrap_or(0);
        match ev["type"].as_str().unwrap_or("") {
            "message_start" => {
                let u = &ev["message"]["usage"];
                self.usage.input = n(&u["input_tokens"]);
                self.usage.cache_read = n(&u["cache_read_input_tokens"]);
                self.usage.cache_write = n(&u["cache_creation_input_tokens"]);
            }
            "content_block_start" => self.blocks.push((ev["content_block"].clone(), String::new())),
            "content_block_delta" => {
                let idx = n(&ev["index"]) as usize;
                let Some((block, json)) = self.blocks.get_mut(idx) else { return Ok(None) };
                let d = &ev["delta"];
                match d["type"].as_str().unwrap_or("") {
                    "text_delta" => {
                        let t = d["text"].as_str().unwrap_or("");
                        append(block, "text", t);
                        return Ok(Some(t.to_string()));
                    }
                    "input_json_delta" => json.push_str(d["partial_json"].as_str().unwrap_or("")),
                    "thinking_delta" => append(block, "thinking", d["thinking"].as_str().unwrap_or("")),
                    "signature_delta" => block["signature"] = d["signature"].clone(),
                    _ => {}
                }
            }
            "message_delta" => {
                if let Some(s) = ev["delta"]["stop_reason"].as_str() {
                    self.stop = s.to_string();
                }
                self.usage.output = n(&ev["usage"]["output_tokens"]);
            }
            "error" => bail!("API error: {}", ev["error"]),
            _ => {}
        }
        Ok(None)
    }

    pub fn finish(self) -> Reply {
        let mut blocks = Vec::new();
        for (v, json) in self.blocks {
            match v["type"].as_str() {
                // Empty text blocks are rejected when replayed.
                Some("text") => {
                    let text = v["text"].as_str().unwrap_or("").to_string();
                    if !text.is_empty() {
                        blocks.push(Block::Text { text });
                    }
                }
                Some("tool_use") => {
                    // With eager input streaming the API does not validate the JSON for us.
                    let input = if json.trim().is_empty() {
                        json!({})
                    } else {
                        match serde_json::from_str::<Value>(&json) {
                            Ok(v) if v.is_object() => v,
                            _ => json!({"__invalid_json": json}),
                        }
                    };
                    blocks.push(Block::ToolUse {
                        id: v["id"].as_str().unwrap_or("").into(),
                        name: v["name"].as_str().unwrap_or("").into(),
                        input,
                    });
                }
                _ => blocks.push(Block::Other(v)),
            }
        }
        Reply { blocks, usage: self.usage, stop: self.stop }
    }
}
