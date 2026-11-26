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
    pub wire: super::Wire,
}

/// api.openai.com's reasoning models reject explicit sampling parameters; compatible endpoints usually accept them.
fn accepts_sampling(first_party: bool, model: &str) -> bool {
    if !first_party {
        return true;
    }
    let id = model.to_ascii_lowercase();
    !["gpt-5", "gpt-6", "o1", "o3", "o4", "codex"].iter().any(|prefix| id.starts_with(prefix))
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

/// Which OpenAI wire this client speaks. The provider *kind* is not enough: a Kimi provider set
/// to the Anthropic format goes through the Messages API, and Responses models need the Responses
/// wire even when configured as a plain OpenAI provider.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lane {
    /// OpenAI's Responses API on api.openai.com (or a Responses-capable catalog model).
    Responses,
    /// Any server speaking Chat Completions.
    Chat,
    /// ChatGPT subscription backend; `codex::Codex` handles it.
    Codex,
}

/// Responses models are identified by id, not by endpoint, so a compatible proxy works too.
pub fn responses_capable(model: &str) -> bool {
    let id = model.to_ascii_lowercase();
    ["gpt-5", "gpt-6", "o1", "o3", "o4", "codex"].iter().any(|p| id.starts_with(p))
}

pub fn lane(base: &str, model: &str, kind: &str) -> Lane {
    if kind == "codex" {
        return Lane::Codex;
    }
    // Only the official endpoint is known to implement `/responses`; compatible servers keep Chat.
    let first_party = reqwest::Url::parse(base).is_ok_and(|url| url.host_str() == Some("api.openai.com"));
    if first_party && responses_capable(model) {
        Lane::Responses
    } else {
        Lane::Chat
    }
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
        if let Some(effort) = self.effort.as_ref().filter(|_| first_party && !self.wire.external_thinking) {
            body["reasoning_effort"] = json!(effort);
        }
        if accepts_sampling(first_party, req.model) {
            use crate::config::provider::Sampling;
            let sampling = &self.wire.sampling;
            for (key, value) in [("temperature", sampling.temperature), ("top_p", sampling.top_p)] {
                if let Some(value) = Sampling::value(value) {
                    body[key] = json!(value);
                }
            }
            if let Some(value) = Sampling::value(sampling.presence_penalty) {
                body["presence_penalty"] = json!(value);
            }
            // The OpenAI API itself rejects these; OpenAI-compatible servers commonly accept them.
            if !first_party {
                for (key, value) in [
                    ("top_k", sampling.top_k),
                    ("min_p", sampling.min_p),
                    ("repetition_penalty", sampling.repetition_penalty),
                ] {
                    if let Some(value) = Sampling::value(value) {
                        body[key] = json!(value);
                    }
                }
            }
        }
        // `tier.*` already excludes endpoints that pick their own serving path (Fireworks, Copilot).
        if let Some(tier) = self.wire.service_tier.and_then(|t| t.wire()) {
            body["service_tier"] = json!(tier);
        }
        // Fast models are their own serving path and ignore a tier.
        if self.wire.fireworks_tier == crate::config::provider::FireworksTier::Priority && !req.model.ends_with("-fast") {
            body["service_tier"] = json!("priority");
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
        // `cacheRetention` on the Chat lane: the legacy parameter, or explicit mode with no writes.
        if first_party {
            let retention = self.wire.cache_retention;
            if retention == crate::config::provider::CacheRetention::None {
                if super::responses::modern(req.model) {
                    body["prompt_cache_options"] = json!({"mode": "explicit"});
                } else {
                    body.as_object_mut().unwrap().remove("prompt_cache_key");
                }
            } else if let Some(options) = super::responses::retention(req.model, &self.wire) {
                match options.get("prompt_cache_retention") {
                    Some(value) => body["prompt_cache_retention"] = value.clone(),
                    None => {
                        if let Some(ttl) = options.get("ttl") {
                            body["prompt_cache_options"] = json!({"mode": "implicit", "ttl": ttl});
                        }
                    }
                }
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
    fn append_only(&self) -> bool {
        self.wire.append_only
    }

    fn live_steering(&self, model: &str) -> bool {
        self.wire.websockets == crate::config::provider::Websockets::On
            && lane(&self.base, model, "openai") == Lane::Responses
            && model.to_ascii_lowercase().starts_with("gpt-6")
    }

    fn complete<'a>(&'a self, req: Request<'a>, on_text: &'a (dyn Fn(&str) + Send + Sync)) -> BoxFut<'a, Result<Reply>> {
        Box::pin(async move {
            if lane(&self.base, req.model, "openai") == Lane::Responses {
                let body = super::responses::body(
                    &super::responses::Shape {
                        model: req.model,
                        system: req.system,
                        messages: req.messages,
                        tools: req.tools,
                        effort: self.effort.as_deref(),
                        codex: false,
                    },
                    &self.wire,
                );
                // `prompt_cache_options` already carries the retention; `body` adds it for this lane.
                if self.wire.websockets == crate::config::provider::Websockets::On {
                    let shown = std::sync::atomic::AtomicBool::new(false);
                    let guarded = |t: &str| {
                        shown.store(true, std::sync::atomic::Ordering::Relaxed);
                        on_text(t);
                    };
                    let headers = [("authorization", format!("Bearer {}", self.key))];
                    match super::responses::complete(&self.base, "responses", &headers, &body, &self.wire, req.live, &guarded).await {
                        Ok(reply) => return Ok(reply),
                        Err(error) if shown.load(std::sync::atomic::Ordering::Relaxed) => return Err(error),
                        Err(_) => {}
                    }
                }
                return responses_sse(&self.http, &self.base, &self.key, &body, &self.wire, on_text).await;
            }
            let body = self.body(&req);
            let identity = if self.copilot { super::copilot::headers(req.messages) } else { Vec::new() };
            let resp = send(&self.wire.retry, || {
                let r = self.http.post(format!("{}/chat/completions", self.base)).bearer_auth(&self.key).json(&body);
                identity.iter().fold(r, |r, (k, v)| r.header(*k, v))
            }).await?;
            let mut acc = Acc::default();
            sse(resp, self.wire.timeouts, |data| {
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

/// The Responses API over SSE: the same events the socket carries, framed as `data:` lines.
async fn responses_sse(
    http: &reqwest::Client,
    base: &str,
    key: &str,
    body: &Value,
    wire: &super::Wire,
    on_text: &(dyn Fn(&str) + Send + Sync),
) -> Result<Reply> {
    let mut payload = body.clone();
    payload["stream"] = json!(true);
    let resp = send(&wire.retry, || http.post(format!("{base}/responses")).bearer_auth(key).json(&payload)).await?;
    let mut acc = super::responses::Acc::default();
    sse(resp, wire.timeouts, |data| {
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
            // `edit.streamingAbort`: stop reading a reply whose streamed edit already fails its preview.
            if let Some(reason) = crate::tools::stream_edit_abort(&e.1, &e.2) {
                bail!(reason);
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::provider::{FireworksTier, Sampling};

    fn client(base: &str, sampling: Sampling) -> OpenAi {
        OpenAi {
            http: reqwest::Client::new(),
            base: base.into(),
            key: "k".into(),
            copilot: false,
            effort: None,
            wire: super::super::Wire { sampling, ..Default::default() },
        }
    }

    fn body(client: &OpenAi, model: &str) -> Value {
        client.body(&Request { model, system: "", messages: &[], tools: &[], max_tokens: 100, live: None })
    }

    #[test]
    fn streaming_abort_stops_a_broken_edit_preview_early() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "one\n").unwrap();
        let mut cfg = crate::config::Config::default();
        let chunk = |args: &str| {
            json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c1","function":{"name":"edit","arguments":args}}]}}]})
        };
        let missing = "{\"path\":\"a.txt\",\"old_string\":\"three\",\"new_string\":\"x\"}";
        // The flag off: the accumulator accepts the same call and the tool fails later.
        cfg.file_opts.edit.streaming_abort = false;
        crate::tools::set_stream_guard(dir.path(), &cfg);
        assert!(Acc::default().feed(&chunk(missing)).is_ok());
        // The flag on: the first complete-but-failing argument delta aborts the stream.
        cfg.file_opts.edit.streaming_abort = true;
        crate::tools::set_stream_guard(dir.path(), &cfg);
        let error = Acc::default().feed(&chunk(missing)).unwrap_err();
        assert!(error.to_string().contains("not in"), "{error}");
        assert!(Acc::default().feed(&chunk("{\"path\":\"a.txt\",\"old_string\":\"one\",\"new_string\":\"two\"}")).is_ok());
        // Non-edit tools stream untouched, and an incomplete edit fragment is not a failure yet.
        assert!(Acc::default().feed(&chunk("")).is_ok());
        assert!(Acc::default()
            .feed(&json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c2","function":{"name":"bash","arguments":"nope"}}]}}]}))
            .is_ok());
    }

    #[test]
    fn sampling_params_are_omitted_when_unset() {
        let unset = body(&client("https://example.test/v1", Sampling::default()), "any-model");
        for key in ["temperature", "top_p", "top_k", "min_p", "presence_penalty", "repetition_penalty"] {
            assert!(unset.get(key).is_none(), "{key} must be omitted at -1");
        }
        let set = Sampling { temperature: 0.2, top_p: 0.9, top_k: 40., min_p: 0.05, presence_penalty: 1., repetition_penalty: 1.1 };
        let payload = body(&client("https://example.test/v1", set), "any-model");
        for (key, want) in [
            ("temperature", 0.2),
            ("top_p", 0.9),
            ("top_k", 40.),
            ("min_p", 0.05),
            ("presence_penalty", 1.),
            ("repetition_penalty", 1.1),
        ] {
            assert_eq!(payload[key], want, "{key}");
        }
        // api.openai.com's reasoning models reject every sampling parameter.
        let sampling = Sampling { temperature: 0.2, top_k: 40., ..Sampling::default() };
        let reasoning = body(&client("https://api.openai.com/v1", sampling.clone()), "gpt-6-sol");
        assert!(reasoning.get("temperature").is_none() && reasoning.get("top_k").is_none());
        // Classic OpenAI models keep the standard parameters but not the compatible-server extras.
        let classic = body(&client("https://api.openai.com/v1", sampling), "gpt-4.1");
        assert_eq!(classic["temperature"], 0.2);
        assert!(classic.get("top_k").is_none());
    }

    #[test]
    fn fireworks_priority_sets_service_tier_except_for_fast_models() {
        let mut client = client("https://api.fireworks.ai/inference/v1", Sampling::default());
        client.wire.fireworks_tier = FireworksTier::Priority;
        assert_eq!(body(&client, "accounts/fireworks/models/llama")["service_tier"], "priority");
        assert!(body(&client, "accounts/fireworks/models/llama-fast").get("service_tier").is_none());
        client.wire.fireworks_tier = FireworksTier::Standard;
        assert!(body(&client, "accounts/fireworks/models/llama").get("service_tier").is_none());
    }

    #[test]
    fn service_tier_and_thinking_flags_reach_the_request_body() {
        let mut c = client("https://api.openai.com/v1", Sampling::default());
        c.wire.service_tier = Some(crate::config::provider::ServiceTier::Priority);
        assert_eq!(body(&c, "gpt-6-sol")["service_tier"], "priority");
        c.wire.service_tier = Some(crate::config::provider::ServiceTier::None);
        assert!(body(&c, "gpt-6-sol").get("service_tier").is_none());
        // `externalThinking` turns provider reasoning off; the scratchpad moves to the `think` tool.
        let mut external = client("https://api.openai.com/v1", Sampling::default());
        external.effort = Some("high".into());
        external.wire.external_thinking = true;
        assert!(body(&external, "gpt-6-sol").get("reasoning_effort").is_none());
        external.wire.external_thinking = false;
        assert_eq!(body(&external, "gpt-6-sol")["reasoning_effort"], "high");
    }

    #[test]
    fn lane_picks_responses_only_for_the_official_endpoint() {
        assert_eq!(lane("https://api.openai.com/v1", "gpt-6-sol", "openai"), Lane::Responses);
        assert_eq!(lane("https://api.openai.com/v1", "gpt-4o-mini", "openai"), Lane::Chat);
        // A proxy serving the same model ids keeps Chat Completions: `/responses` is not guaranteed.
        assert_eq!(lane("https://proxy.test/v1", "gpt-6-sol", "openai"), Lane::Chat);
        assert_eq!(lane("https://proxy.test/v1", "gpt-6-sol", "codex"), Lane::Codex);
        assert!(responses_capable("gpt-6-sol") && responses_capable("o3-mini"));
        assert!(!responses_capable("gpt-4o"));
    }

    /// The Responses lane over a local socket: `response.create`, deltas, completion.
    #[tokio::test]
    async fn responses_lane_streams_over_the_websocket() {
        use futures_util::{SinkExt, StreamExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_async(tcp).await.unwrap();
            let create: Value = serde_json::from_str(socket.next().await.unwrap().unwrap().to_text().unwrap()).unwrap();
            assert_eq!(create["type"], "response.create");
            assert_eq!(create["model"], "gpt-6-sol");
            assert_eq!(create["stream"], true);
            assert_eq!(create["prompt_cache_options"]["ttl"], "12h");
            for event in [
                json!({"type": "response.created", "response": {"id": "resp_1"}}),
                json!({"type": "response.output_text.delta", "delta": "hello"}),
                json!({"type": "response.completed", "response": {"id": "resp_1", "usage": {"input_tokens": 7, "output_tokens": 2}}}),
            ] {
                socket.send(tokio_tungstenite::tungstenite::Message::text(event.to_string())).await.unwrap();
            }
        });
        let mut client = OpenAi {
            http: reqwest::Client::new(),
            base: format!("http://127.0.0.1:{port}"),
            key: "k".into(),
            copilot: false,
            effort: None,
            wire: super::super::Wire::default(),
        };
        client.wire.websockets = crate::config::provider::Websockets::On;
        client.wire.cache_retention = crate::config::provider::CacheRetention::Long;
        // Force the Responses lane: the classification reads the endpoint, and this one is local.
        let req = Request { model: "gpt-6-sol", system: "sys", messages: &[crate::Message::user("hi")], tools: &[], max_tokens: 100, live: None };
        let body = super::super::responses::body(
            &super::super::responses::Shape { model: req.model, system: req.system, messages: req.messages, tools: req.tools, effort: None, codex: false },
            &client.wire,
        );
        let url = super::super::ws::socket_url(&format!("{}/responses", client.base));
        let mut ws = super::super::ws::Ws::connect(&url, &[("authorization", "Bearer k".into())]).await.unwrap();
        let text = std::sync::Mutex::new(String::new());
        let reply = super::super::responses::stream(&mut ws, &body, &client.wire, None, &|t| text.lock().unwrap().push_str(t))
            .await
            .expect("responses stream");
        assert_eq!(text.into_inner().unwrap(), "hello");
        assert_eq!(reply.usage.input, 7);
        server.await.unwrap();
    }

    #[test]
    fn responses_body_forwards_retention_and_cache_key() {
        let wire = |retention, append_only| super::super::Wire {
            cache_retention: retention,
            append_only,
            ..Default::default()
        };
        let messages = [crate::Message::user("hi")];
        let shape = |system| super::super::responses::Shape { model: "gpt-6-sol", system, messages: &messages, tools: &[], effort: None, codex: false };
        let cached = super::super::responses::body(&shape("sys"), &wire(crate::config::provider::CacheRetention::Long, false));
        assert_eq!(cached["prompt_cache_options"]["ttl"], "12h");
        assert!(cached.get("prompt_cache_key").is_some(), "the shared prefix key rides along");
        let off = super::super::responses::body(&shape("sys"), &wire(crate::config::provider::CacheRetention::None, false));
        assert_eq!(off["prompt_cache_options"]["mode"], "explicit");
        assert!(off.get("prompt_cache_key").is_none());
        // Retention reaches the Chat lane too, as the legacy OpenAI parameter.
        let mut client = client("https://api.openai.com/v1", Sampling::default());
        client.wire.cache_retention = crate::config::provider::CacheRetention::Short;
        assert_eq!(body(&client, "gpt-4.1")["prompt_cache_retention"], "in_memory");
    }
}
