//! The OpenAI Responses wire (ChatGPT/Codex and api.openai.com): one request body, one event
//! accumulator, and the WebSocket transport both providers use.
use super::ws::{socket_url, Ws};
use super::Wire;
use crate::{Block, Message, Role, ToolDef, Usage};
use anyhow::{bail, Result};
use serde_json::{json, Value};

/// Everything one Responses request carries; `codex` adds the ChatGPT backend's fixed choices.
pub struct Shape<'a> {
    pub model: &'a str,
    pub system: &'a str,
    pub messages: &'a [Message],
    pub tools: &'a [ToolDef],
    pub effort: Option<&'a str>,
    pub codex: bool,
}

/// Stable cache key for one agent's system prompt and tool set, so a growing transcript keeps the
/// same routing key; `None` when append-only is off (the key then says nothing about the prefix).
fn cache_key(shape: &Shape, wire: &Wire) -> Option<String> {
    // `none` sends no cache markers at all, the key included.
    if !wire.cache_retention.caching() {
        return None;
    }
    if !wire.append_only {
        return Some("greencode".into());
    }
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(shape.system.as_bytes());
    for tool in shape.tools {
        hasher.update(tool.name.as_bytes());
        hasher.update(tool.description.as_bytes());
        hasher.update(tool.schema.to_string().as_bytes());
    }
    Some(format!("{:x}", hasher.finalize())[..16].to_string())
}

pub fn body(shape: &Shape, wire: &Wire) -> Value {
    let mut input = Vec::new();
    let cache_instructions = !shape.codex
        && !shape.system.is_empty()
        && wire.cache_retention.caching()
        && modern(shape.model);
    if cache_instructions {
        // GPT-6.2+ puts top-level instructions after all cacheable input.
        input.push(json!({
            "type": "message",
            "role": "developer",
            "content": [{
                "type": "input_text",
                "text": shape.system,
                "prompt_cache_breakpoint": {"mode": "explicit"},
            }],
        }));
    }
    for m in shape.messages {
        let mut content = Vec::new();
        for b in &m.content {
            match (b, &m.role) {
                (Block::Text { text }, Role::User) => content.push(json!({"type": "input_text", "text": text})),
                (Block::Text { text }, Role::Assistant) => input.push(json!({"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": text}]})),
                (Block::ToolUse { id, name, input: args }, _) => input.push(json!({"type": "function_call", "call_id": id, "name": name, "arguments": args.to_string()})),
                (Block::ToolResult { tool_use_id, content, .. }, _) => input.push(json!({"type": "function_call_output", "call_id": tool_use_id, "output": content})),
                // `compaction.remoteStreamingV2Enabled`: the retained server-side compaction item
                // must be replayed unchanged for the provider to keep its context.
                (Block::Other(v), _) if v["type"] == "compaction" => input.push(v.clone()),
                (Block::Other(v), _) if v["type"] == "codex_reasoning" => {
                    let mut item = v["item"].clone();
                    if let Some(o) = item.as_object_mut() { o.remove("id"); }
                    input.push(item);
                }
                (Block::Other(v), Role::User) if v["type"] == "image" && v["source"]["type"] == "base64" => content.push(json!({"type": "input_image",
                    "image_url": format!("data:{};base64,{}", v["source"]["media_type"].as_str().unwrap_or("image/jpeg"), v["source"]["data"].as_str().unwrap_or(""))})),
                _ => {}
            }
        }
        if !content.is_empty() { input.push(json!({"type": "message", "role": "user", "content": content})); }
    }
    let tools: Vec<Value> = shape.tools.iter()
        .map(|t| json!({"type": "function", "name": t.name, "description": t.description, "parameters": t.schema, "strict": false}))
        .collect();
    // `omitThinking` drops the summary field; the reasoning items themselves still stream.
    let mut reasoning = if wire.omit_thinking { json!({}) } else { json!({"summary": "auto"}) };
    if wire.external_thinking {
        reasoning["effort"] = json!("none");
    } else if let Some(effort) = shape.effort.or(shape.codex.then_some("medium")) {
        reasoning["effort"] = json!(effort);
    }
    let mut body = json!({
        "model": shape.model,
        "input": input,
        "tools": tools,
        "tool_choice": "auto",
        "parallel_tool_calls": true,
        "store": false,
        "stream": true,
        "text": {"verbosity": wire.verbosity.as_str()},
        "include": ["reasoning.encrypted_content"],
        "reasoning": reasoning,
    });
    if !cache_instructions {
        body["instructions"] = json!(shape.system);
    }
    if let Some(key) = cache_key(shape, wire) {
        body["prompt_cache_key"] = json!(key);
    }
    if let Some(tier) = wire.service_tier.and_then(|t| t.wire()) {
        body["service_tier"] = json!(tier);
    }
    if !shape.codex {
        if let Some(options) = retention(shape.model, wire) {
            body["prompt_cache_options"] = options;
        }
    }
    body
}

/// `cacheRetention` as the Responses API spells it: `ttl` from GPT-5.6, `prompt_cache_retention` before.
pub fn retention(model: &str, wire: &Wire) -> Option<Value> {
    use crate::config::provider::CacheRetention;
    let retention = wire.cache_retention;
    if retention == CacheRetention::None {
        // Explicit mode without breakpoints performs no cache writes at all.
        return modern(model).then(|| json!({"mode": "explicit"}));
    }
    // The lifetime is a request-level option on newer models; earlier ones take `prompt_cache_retention`.
    let ttl = match retention {
        CacheRetention::Short => Some("30m"),
        CacheRetention::Long => Some("12h"),
        CacheRetention::Auto | CacheRetention::None => None,
    };
    if modern(model) {
        ttl.map(|ttl| json!({"mode": "implicit", "ttl": ttl}))
    } else {
        retention.prompt_cache_retention().map(|r| json!({"prompt_cache_retention": r}))
    }
}

/// GPT-5.6 and later take `prompt_cache_options`; the boundary is the model id, not the endpoint.
pub fn modern(model: &str) -> bool {
    let id = model.to_ascii_lowercase();
    let version = id.strip_prefix("gpt-").unwrap_or("").split('-').next().unwrap_or("");
    let mut parts = version.split('.').map(|p| p.parse::<u32>().unwrap_or(0));
    let (major, minor) = (parts.next().unwrap_or(0), parts.next().unwrap_or(0));
    major >= 6 || (major == 5 && minor >= 6)
}

/// Accumulates streaming events into content blocks.
#[derive(Default)]
pub struct Acc {
    text: String,
    item_text: String,
    reasoning: Vec<Block>,
    calls: Vec<Block>,
    usage: Usage,
    max_tokens: bool,
}

impl Acc {
    pub fn text(&self) -> &str {
        &self.text
    }

    /// Returns the text delta, if the event carried one.
    pub fn feed(&mut self, ev: &Value) -> Result<Option<String>> {
        match ev["type"].as_str().unwrap_or("") {
            "response.output_text.delta" => {
                let d = ev["delta"].as_str().unwrap_or("");
                self.text.push_str(d);
                return Ok((!d.is_empty()).then(|| d.to_string()));
            }
            "response.output_item.done" => {
                let item = &ev["item"];
                match item["type"].as_str() {
                    Some("function_call") => self.calls.push(Block::ToolUse {
                        id: item["call_id"].as_str().unwrap_or("").into(),
                        name: item["name"].as_str().unwrap_or("").into(),
                        input: item["arguments"].as_str().and_then(|a| serde_json::from_str(a).ok()).unwrap_or_else(|| json!({})),
                    }),
                    Some("reasoning") => self.reasoning.push(Block::Other(json!({"type": "codex_reasoning", "item": item}))),
                    Some("message") => {
                        for part in item["content"].as_array().into_iter().flatten() {
                            if let Some(t) = part["text"].as_str() { self.item_text.push_str(t); }
                        }
                    }
                    _ => {}
                }
            }
            "response.completed" | "response.incomplete" => {
                let r = &ev["response"];
                let u = &r["usage"];
                let cached = u["input_tokens_details"]["cached_tokens"].as_u64().unwrap_or(0);
                self.usage.add(Usage {
                    input: u["input_tokens"].as_u64().unwrap_or(0).saturating_sub(cached),
                    output: u["output_tokens"].as_u64().unwrap_or(0),
                    cache_read: cached,
                    cache_write: 0,
                });
                self.max_tokens = r["incomplete_details"]["reason"] == "max_output_tokens";
            }
            "response.failed" => bail!("{}", ev["response"]["error"]["message"].as_str().unwrap_or("response failed")),
            "error" => bail!("{}", ev["message"].as_str().or_else(|| ev["error"]["message"].as_str()).unwrap_or("stream error")),
            _ => {}
        }
        Ok(None)
    }

    pub fn finish(self) -> super::Reply {
        let mut blocks = self.reasoning;
        let text = if self.text.is_empty() { self.item_text } else { self.text };
        if !text.is_empty() { blocks.push(Block::Text { text }); }
        let stop = if !self.calls.is_empty() { "tool_use" } else if self.max_tokens { "max_tokens" } else { "end_turn" };
        blocks.extend(self.calls);
        super::Reply { blocks, usage: self.usage, stop: stop.into() }
    }
}

/// Forwards only text the client has not shown yet, so a steered continuation does not repeat itself.
fn forward_new(total: &str, shown: &mut String, on_text: &dyn Fn(&str)) {
    let fresh = total.strip_prefix(shown.as_str()).unwrap_or(total);
    if !fresh.is_empty() {
        on_text(fresh);
        shown.push_str(fresh);
    }
}

/// Streams one response over the socket, sending `response.steer` for messages typed mid-flight.
pub async fn stream(
    ws: &mut Ws,
    request: &Value,
    wire: &Wire,
    live: Option<&super::LiveSteer>,
    on_text: &(dyn Fn(&str) + Send + Sync),
) -> Result<super::Reply> {
    // The socket lane wraps the request body in an event; an explicit type is already the frame.
    let frame = match request.get("type") {
        Some(_) => request.clone(),
        None => {
            let mut frame = request.clone();
            frame["type"] = json!("response.create");
            frame
        }
    };
    ws.send_json(&frame).await?;
    let mut acc = Acc::default();
    let mut shown = String::new();
    let mut current: Option<String> = None;
    // Continuations this socket still owes us: one per accepted steer, plus the one the
    // `steered` incomplete status announces.
    let mut flowing = 0u32;
    loop {
        // A steer needs the running response's id, which `response.created` supplies.
        if let (Some(steer), Some(id)) = (live, current.as_deref()) {
            for text in steer.take() {
                ws.send_json(&json!({"type": "response.steer", "previous_response_id": id, "input": text})).await?;
            }
        }
        let Some(text) = ws.next_text(wire.timeouts.idle).await? else {
            bail!("the WebSocket closed before the response finished");
        };
        let Ok(ev) = serde_json::from_str::<Value>(&text) else { continue };
        match ev["type"].as_str().unwrap_or("") {
            "response.created" => {
                // A continuation restates the whole answer; `shown` keeps only its new tail on screen.
                if current.replace(ev["response"]["id"].as_str().unwrap_or("").to_string()).is_some() {
                    acc = Acc::default();
                }
            }
            "response.steer.accepted" => flowing += 1,
            "response.output_text.delta" => {
                let _ = acc.feed(&ev)?;
                forward_new(acc.text(), &mut shown, on_text);
            }
            "response.completed" | "response.incomplete" => {
                let steered = ev["response"]["incomplete_details"]["reason"] == "steered";
                if steered {
                    // This response ended *because* of a steer; its continuation replaces it.
                    flowing = flowing.saturating_sub(1);
                }
                acc.feed(&ev)?;
                if flowing > 0 || steered {
                    // A continuation restates the whole answer and its usage replaces this one's.
                    acc = Acc::default();
                    continue;
                }
                return Ok(acc.finish());
            }
            _ => {
                let _ = acc.feed(&ev)?;
            }
        }
    }
}

/// Streams `body` over a fresh socket at `{base}/{path}`; auth rides in `headers`.
pub async fn complete(
    base: &str,
    path: &str,
    headers: &[(&str, String)],
    request: &Value,
    wire: &Wire,
    live: Option<&super::LiveSteer>,
    on_text: &(dyn Fn(&str) + Send + Sync),
) -> Result<super::Reply> {
    let url = socket_url(&format!("{}/{path}", base.trim_end_matches('/')));
    let mut ws = Ws::connect(&url, headers).await?;
    stream(&mut ws, request, wire, live, on_text).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::provider::{CacheRetention, Websockets};

    fn shape<'a>(messages: &'a [Message]) -> Shape<'a> {
        Shape { model: "gpt-6-sol", system: "sys", messages, tools: &[], effort: Some("high"), codex: false }
    }

    fn wire(retention: CacheRetention, append_only: bool) -> Wire {
        Wire { cache_retention: retention, append_only, websockets: Websockets::Auto, ..Default::default() }
    }

    #[test]
    fn thinking_and_tier_flags_shape_the_responses_body() {
        let messages = [Message::user("hi")];
        let mut w = wire(CacheRetention::Auto, false);
        w.omit_thinking = true;
        let omitted = body(&shape(&messages), &w);
        assert!(omitted["reasoning"].get("summary").is_none());
        w.omit_thinking = false;
        assert_eq!(body(&shape(&messages), &w)["reasoning"]["summary"], "auto");
        // `externalThinking` forces reasoning off and sends the top tier when configured.
        w.external_thinking = true;
        w.service_tier = Some(crate::config::provider::ServiceTier::Priority);
        let external = body(&shape(&messages), &w);
        assert_eq!(external["reasoning"]["effort"], "none");
        assert_eq!(external["service_tier"], "priority");
        w.service_tier = None;
        assert!(body(&shape(&messages), &w).get("service_tier").is_none());
    }

    #[test]
    fn body_keeps_codex_fields_and_drops_effort_when_unset() {
        let messages = [Message::user("hi")];
        let codex = body(&Shape { effort: None, codex: true, ..shape(&messages) }, &wire(CacheRetention::Auto, false));
        assert_eq!(codex["stream"], true);
        assert_eq!(codex["store"], false);
        assert_eq!(codex["reasoning"]["effort"], "medium");
        assert_eq!(codex["prompt_cache_key"], "greencode");
        assert_eq!(codex["instructions"], "sys");
        assert_eq!(codex["input"][0]["role"], "user");
        let standard = body(&Shape { effort: None, ..shape(&messages) }, &wire(CacheRetention::Auto, false));
        assert!(standard["reasoning"].get("effort").is_none());
        assert_eq!(standard["reasoning"]["summary"], "auto");
        let effort = body(&shape(&messages), &wire(CacheRetention::Auto, false));
        assert_eq!(effort["reasoning"]["effort"], "high");
    }

    #[test]
    fn responses_cache_keeps_instructions_before_growing_history() {
        let w = wire(CacheRetention::Auto, true);
        let first = [Message::user("first question")];
        let first_shape = Shape { model: "gpt-6.2-sol", ..shape(&first) };
        let initial = body(&first_shape, &w);
        let continued = [Message::user("first question"), Message::user("follow-up")];
        let next = body(&Shape { messages: &continued, ..first_shape }, &w);
        let separate = body(&Shape { messages: &[Message::user("unrelated question")], ..first_shape }, &w);
        assert!(initial.get("instructions").is_none(), "instructions must not be repeated after the cached input");
        assert_eq!(initial["input"][0], json!({
            "type": "message",
            "role": "developer",
            "content": [{"type": "input_text", "text": "sys", "prompt_cache_breakpoint": {"mode": "explicit"}}],
        }));
        assert_eq!(initial["input"][0], separate["input"][0]);
        assert_eq!(initial["input"].as_array().unwrap(), &next["input"].as_array().unwrap()[..2]);
        assert_eq!(next["input"][2]["content"][0]["text"], "follow-up");
        assert!(next.get("prompt_cache_options").is_none(), "automatic caching stays at its implicit default");
    }

    #[test]
    fn responses_keeps_legacy_and_disabled_caching_wire_shape() {
        let messages = [Message::user("question")];
        let legacy = body(&Shape { model: "gpt-5.5", ..shape(&messages) }, &wire(CacheRetention::Auto, true));
        let disabled = body(&shape(&messages), &wire(CacheRetention::None, true));
        for payload in [&legacy, &disabled] {
            assert_eq!(payload["instructions"], "sys");
            assert_eq!(payload["input"][0]["role"], "user");
        }
        assert_eq!(disabled["prompt_cache_options"]["mode"], "explicit");
        assert!(disabled["input"][0]["content"][0].get("prompt_cache_breakpoint").is_none());
    }

    #[test]
    fn append_only_makes_the_cache_key_stable_and_specific() {
        let messages = [Message::user("hi")];
        let a = body(&shape(&messages), &wire(CacheRetention::Auto, true));
        let b = body(&Shape { messages: &[Message::user("another")], ..shape(&messages) }, &wire(CacheRetention::Auto, true));
        let key = a["prompt_cache_key"].as_str().unwrap();
        assert_eq!(key.len(), 16);
        assert_eq!(b["prompt_cache_key"], json!(key), "history changes must not move the shared prefix key");
        let other = body(&Shape { system: "different", ..shape(&messages) }, &wire(CacheRetention::Auto, true));
        assert_ne!(other["prompt_cache_key"], json!(key));
    }

    #[test]
    fn retention_maps_to_ttl_or_the_legacy_parameter() {
        assert!(retention("gpt-6-sol", &wire(CacheRetention::Auto, false)).is_none());
        assert_eq!(retention("gpt-6-sol", &wire(CacheRetention::Long, false)).unwrap()["ttl"], "12h");
        assert_eq!(retention("gpt-6-sol", &wire(CacheRetention::Short, false)).unwrap()["ttl"], "30m");
        assert_eq!(retention("gpt-6-sol", &wire(CacheRetention::None, false)).unwrap()["mode"], "explicit");
        let legacy = retention("gpt-4.1", &wire(CacheRetention::Long, false)).unwrap();
        assert_eq!(legacy["prompt_cache_retention"], "24h");
        assert_eq!(retention("gpt-4.1", &wire(CacheRetention::Short, false)).unwrap()["prompt_cache_retention"], "in_memory");
        assert!(retention("gpt-4.1", &wire(CacheRetention::Auto, false)).is_none());
    }

    #[test]
    fn forward_new_skips_the_repeated_continuation() {
        let mut shown = String::new();
        let out = std::cell::RefCell::new(String::new());
        let push = |t: &str| out.borrow_mut().push_str(t);
        for total in ["Plan: A", "Plan: A, then B", "Plan: A, then B"] {
            forward_new(total, &mut shown, &push);
        }
        assert_eq!(out.into_inner(), "Plan: A, then B");
    }

    /// The queued steer goes out after `response.created`; the continuation's full text is deduplicated.
    #[tokio::test]
    async fn steering_continues_inside_the_same_request() {
        use futures_util::{SinkExt, StreamExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_async(tcp).await.unwrap();
            let create: Value = serde_json::from_str(socket.next().await.unwrap().unwrap().to_text().unwrap()).unwrap();
            assert_eq!(create["type"], "response.create");
            for v in [
                json!({"type": "response.created", "response": {"id": "resp_1"}}),
                json!({"type": "response.output_text.delta", "delta": "first half"}),
            ] {
                socket.send(tokio_tungstenite::tungstenite::Message::text(v.to_string())).await.unwrap();
            }
            let steer: Value = loop {
                let text = socket.next().await.unwrap().unwrap();
                let v: Value = serde_json::from_str(text.to_text().unwrap()).unwrap();
                if v["type"] == "response.steer" { break v; }
            };
            assert_eq!(steer["previous_response_id"], "resp_1");
            assert_eq!(steer["input"], "keep it small");
            for v in [
                json!({"type": "response.steer.accepted", "steer": {"id": "steer_1", "previous_response_id": "resp_1"}}),
                json!({"type": "response.incomplete", "response": {"id": "resp_1", "usage": {"input_tokens": 10, "output_tokens": 2}, "incomplete_details": {"reason": "steered"}}}),
                json!({"type": "response.created", "response": {"id": "resp_2"}}),
                json!({"type": "response.output_text.delta", "delta": "first half and the rest"}),
                json!({"type": "response.completed", "response": {"id": "resp_2", "usage": {"input_tokens": 12, "output_tokens": 9}}}),
            ] {
                socket.send(tokio_tungstenite::tungstenite::Message::text(v.to_string())).await.unwrap();
            }
        });
        let live = super::super::LiveSteer::default();
        live.push("keep it small".into());
        let mut ws = Ws::connect(&format!("ws://127.0.0.1:{port}/responses"), &[]).await.unwrap();
        let shown = std::sync::Mutex::new(String::new());
        let request = json!({"type": "response.create", "model": "gpt-6-sol", "stream": true});
        let reply = stream(&mut ws, &request, &Wire::default(), Some(&live), &|t| shown.lock().unwrap().push_str(t)).await.expect("steered response");
        assert_eq!(shown.into_inner().unwrap(), "first half and the rest");
        let text: String = reply.blocks.iter().filter_map(|b| match b {
            Block::Text { text } => Some(text.as_str()),
            _ => None,
        }).collect();
        assert_eq!(text, "first half and the rest");
        assert_eq!(reply.usage.input, 12);
        server.await.unwrap();
    }
}
