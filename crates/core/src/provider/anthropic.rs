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
    pub wire: super::Wire,
}

/// `extendedContext`: Anthropic's 1M-token context window.
const EXTENDED_CONTEXT_BETA: &str = "context-1m-2025-08-07";

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
        // `cache_control` with a TTL is the whole retention mechanism on Anthropic; `none` drops
        // every breakpoint so the request is billed (and stored) without prompt caching.
        let breakpoint = super::anthropic_breakpoint(&self.wire);
        let mut system = json!({"type": "text", "text": req.system});
        let mut body = json!({
            "model": req.model,
            "max_tokens": req.max_tokens,
            "stream": true,
            "messages": req.messages,
        });
        if !breakpoint.is_null() {
            system["cache_control"] = breakpoint.clone();
            // Automatic breakpoint on the last block: the whole append-only history is cached.
            body["cache_control"] = breakpoint.clone();
        }
        body["system"] = json!([system]);
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
        if !breakpoint.is_null() {
            if let Some((i, message)) = req.messages.iter().enumerate().rev().filter(|(_, m)| m.role == crate::Role::User).nth(1) {
                if let Some(j) = message.content.iter().rposition(|b| matches!(b, Block::Text { .. } | Block::ToolResult { .. })) {
                    body["messages"][i]["content"][j]["cache_control"] = breakpoint;
                }
            }
        }
        // Anthropic accepts these three; `-1` leaves its default.
        for (key, value) in [
            ("temperature", self.wire.sampling.temperature),
            ("top_p", self.wire.sampling.top_p),
            ("top_k", self.wire.sampling.top_k),
        ] {
            if let Some(value) = crate::config::provider::Sampling::value(value) {
                body[key] = json!(value);
            }
        }
        if self.wire.external_thinking {
            // No native reasoning: the model keeps its scratchpad in the `think` tool instead.
        } else if let Some(e) = &self.effort {
            body["output_config"] = json!({"effort": e});
            // The API requires the budget to stay below `max_tokens`.
            let budget = self.wire.thinking.budget(e).map(|tokens| tokens.min(req.max_tokens.saturating_sub(1)));
            if first_party && !self.wire.omit_thinking {
                if let Some(budget) = budget.filter(|tokens| *tokens >= 1024) {
                    body["thinking"] = json!({"type": "enabled", "budget_tokens": budget});
                }
            }
        }
        if first_party && self.wire.service_tier == Some(crate::config::provider::ServiceTier::Priority) {
            body["speed"] = json!("fast");
        }
        if first_party && self.wire.server_side_fallback && FALLBACK_MODELS.contains(&req.model) {
            body["fallbacks"] = json!("default");
        }
        body
    }
}

impl Provider for Anthropic {
    fn append_only(&self) -> bool {
        self.wire.append_only
    }

    fn complete<'a>(&'a self, req: Request<'a>, on_text: &'a (dyn Fn(&str) + Send + Sync)) -> BoxFut<'a, Result<Reply>> {
        Box::pin(async move {
            let body = self.body(&req);
            let auth = match &self.oauth {
                Some(path) => Some((anthropic_oauth::access_token(path).await?, anthropic_oauth::user_agent().await)),
                None => None,
            };
            let resp = send(&self.wire.retry, || {
                let mut r = self.http.post(format!("{}/v1/messages", self.base)).header("anthropic-version", "2026-10-21").json(&body);
                // `extendedContext`: the 1M-token window is a beta header, only first-party asks for it.
                let extended = (self.wire.extended_context && self.base.contains("api.anthropic.com")).then_some(EXTENDED_CONTEXT_BETA);
                let fallback = body.get("fallbacks").is_some().then_some("server-side-fallback-2026-07-01");
                r = match &auth {
                    Some((token, ua)) => r.bearer_auth(token).header(reqwest::header::USER_AGENT, *ua).header("x-app", "cli")
                        .header("anthropic-beta", [Some(BETAS), fallback, extended].into_iter().flatten().collect::<Vec<_>>().join(",")),
                    None => {
                        let r = r.header("x-api-key", &self.key);
                        match [fallback, extended].into_iter().flatten().collect::<Vec<_>>().join(",") {
                            betas if betas.is_empty() => r,
                            betas => r.header("anthropic-beta", betas),
                        }
                    }
                };
                r
            })
            .await?;
            let mut acc = Acc::default();
            sse(resp, self.wire.timeouts, |data| {
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
                    "input_json_delta" => {
                        json.push_str(d["partial_json"].as_str().unwrap_or(""));
                        // `edit.streamingAbort`: stop reading a reply whose streamed edit already fails.
                        if let Some(reason) = crate::tools::stream_edit_abort(block["name"].as_str().unwrap_or(""), json) {
                            bail!(reason);
                        }
                    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::provider::ThinkingBudgets;

    fn client(effort: Option<&str>, thinking: ThinkingBudgets) -> Anthropic {
        Anthropic {
            http: reqwest::Client::new(),
            base: "https://api.anthropic.com".into(),
            key: "k".into(),
            oauth: None,
            effort: effort.map(String::from),
            wire: super::super::Wire { thinking, ..Default::default() },
        }
    }

    fn body(client: &Anthropic, model: &str, max_tokens: u32) -> Value {
        client.body(&Request { model, system: "", messages: &[], tools: &[], max_tokens, live: None })
    }

    #[test]
    fn tier_and_thinking_flags_shape_the_messages_request() {
        let mut c = client(Some("high"), ThinkingBudgets::default());
        c.wire.service_tier = Some(crate::config::provider::ServiceTier::Priority);
        let fast = body(&c, "claude-opus-5-5", 8000);
        assert_eq!(fast["speed"], "fast");
        assert_eq!(fast["thinking"]["budget_tokens"], 7999);
        c.wire.omit_thinking = true;
        let omitted = body(&c, "claude-opus-5-5", 8000);
        assert!(omitted.get("thinking").is_none());
        assert_eq!(omitted["output_config"]["effort"], "high");
        c.wire.external_thinking = true;
        let external = body(&c, "claude-opus-5-5", 8000);
        assert!(external.get("output_config").is_none() && external.get("thinking").is_none());
        // A proxy is not first party: priority and thinking budgets stay off there.
        let mut proxy = client(Some("high"), ThinkingBudgets::default());
        proxy.base = "https://claude-proxy.test".into();
        proxy.wire.service_tier = Some(crate::config::provider::ServiceTier::Priority);
        let proxied = body(&proxy, "claude-opus-5-5", 8000);
        assert!(proxied.get("speed").is_none() && proxied.get("thinking").is_none());
    }

    #[test]
    fn configured_effort_sends_its_thinking_budget() {
        let thinking = ThinkingBudgets { high: 4000, max: 60_000, ..Default::default() };
        let payload = body(&client(Some("high"), thinking), "claude-opus-5-5", 32_000);
        assert_eq!(payload["thinking"]["budget_tokens"], 4000);
        // A budget that would exceed `max_tokens` is clamped below it.
        let payload = body(&client(Some("max"), thinking), "claude-opus-5-5", 10_000);
        assert_eq!(payload["thinking"]["budget_tokens"], 9_999);
        // Unset effort or an unknown level sends no budget.
        assert!(body(&client(None, thinking), "claude-opus-5-5", 32_000).get("thinking").is_none());
        assert!(body(&client(Some("weird"), thinking), "claude-opus-5-5", 32_000).get("thinking").is_none());
    }

    #[test]
    fn anthropic_sampling_params_are_omitted_when_unset() {
        let mut client = client(None, ThinkingBudgets::default());
        let unset = body(&client, "claude-opus-5-5", 1000);
        for key in ["temperature", "top_p", "top_k"] {
            assert!(unset.get(key).is_none(), "{key} must be omitted at -1");
        }
        client.wire.sampling = crate::config::provider::Sampling {
            temperature: 0.,
            top_p: 0.9,
            top_k: 20.,
            ..Default::default()
        };
        let set = body(&client, "claude-opus-5-5", 1000);
        assert_eq!(set["temperature"], 0.);
        assert_eq!(set["top_p"], 0.9);
        assert_eq!(set["top_k"], 20.);
        assert!(set.get("min_p").is_none(), "Anthropic has no min_p");
    }

    #[test]
    fn cache_retention_sets_the_breakpoint_ttl() {
        use crate::config::provider::CacheRetention;
        let with = |retention| {
            let mut c = client(None, ThinkingBudgets::default());
            c.wire.cache_retention = retention;
            c
        };
        // The default five-minute window carries no explicit `ttl`.
        let auto = body(&with(CacheRetention::Auto), "claude-opus-5", 100);
        assert_eq!(auto["system"][0]["cache_control"], json!({"type": "ephemeral"}));
        assert_eq!(auto["cache_control"], json!({"type": "ephemeral"}));
        let long = body(&with(CacheRetention::Long), "claude-opus-5", 100);
        assert_eq!(long["system"][0]["cache_control"], json!({"type": "ephemeral", "ttl": "1h"}));
        assert_eq!(long["cache_control"]["ttl"], "1h");
        // `none` drops every breakpoint: the request is neither cached nor billed for cache writes.
        let off = body(&with(CacheRetention::None), "claude-opus-5", 100);
        assert!(off["system"][0].get("cache_control").is_none());
        assert!(off.get("cache_control").is_none());
    }

    #[test]
    fn retention_never_pins_an_old_message_when_caching_is_off() {
        use crate::config::provider::CacheRetention;
        let messages = [crate::Message::user("first"), crate::Message::user("second")];
        let mut c = client(None, ThinkingBudgets::default());
        c.wire.cache_retention = CacheRetention::None;
        let payload = c.body(&Request { model: "claude-opus-5", system: "sys", messages: &messages, tools: &[], max_tokens: 100, live: None });
        let text = serde_json::to_string(&payload["messages"]).unwrap();
        assert!(!text.contains("cache_control"), "no breakpoint belongs in history: {text}");
        c.wire.cache_retention = CacheRetention::Short;
        let payload = c.body(&Request { model: "claude-opus-5", system: "sys", messages: &messages, tools: &[], max_tokens: 100, live: None });
        let text = serde_json::to_string(&payload["messages"]).unwrap();
        assert!(text.contains("cache_control"), "the pin returns with caching on: {text}");
    }
}
