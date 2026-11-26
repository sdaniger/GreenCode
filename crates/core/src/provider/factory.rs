//! Factory Droid's LLM proxy: one WorkOS bearer, four upstream wires picked by model family
//! (OMP `providers/factory-droid.ts`, `catalog/src/wire/factory-droid.ts`).
//!
//! | family | path | models |
//! |---|---|---|
//! | Chat Completions | `/api/llm/o/v1/chat/completions` | Kimi, GLM, DeepSeek, Qwen, MiniMax, Mistral, Nemotron |
//! | Responses | `/api/llm/o/v1/responses` | GPT, Grok |
//! | Messages | `/api/llm/a/v1/messages` | Claude |
//! | Gemini `generateContent` | `/api/llm/g/v1/generate` | Gemini |
//!
//! The three wires GreenCode already speaks are reused; a Gemini-routed model reports that it is
//! unsupported instead of sending a body the proxy would reject.

use super::{anthropic, openai, responses, Provider, Reply, Request, Wire};
use crate::BoxFut;
use anyhow::{bail, Result};
use serde_json::Value;

/// Reported to Factory's API; the identity of the CLI this port mirrors.
const CLIENT_VERSION: &str = "1.7.1";
const DROID_PREFIX: &str = "You are Droid, an AI software engineering agent built by Factory.";

/// The wire one model id is served over, from `api-routes provider="factory-droid"`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WireKind {
    Chat,
    Responses,
    Messages,
    Gemini,
}

/// Which wire serves `model`: registry ids first, then the family prefix, else chat completions.
pub fn wire_for(model: &str) -> WireKind {
    let id = model.to_ascii_lowercase();
    const MESSAGES: [&str; 21] = [
        "claude-fable-5.2", "claude-fable-5.1", "claude-fable-5", "claude-opus-5-5", "claude-opus-5-5-fast",
        "claude-opus-5", "claude-opus-5-fast", "claude-opus-4-8", "claude-opus-4-8-fast", "claude-opus-4-7",
        "claude-opus-4-6", "claude-opus-4-5-20251101", "claude-sonnet-5-5", "claude-sonnet-5",
        "claude-sonnet-4-6", "claude-sonnet-4-5-20250929", "claude-haiku-5-5", "claude-haiku-4-5-20251001",
        "atlas-07-21", "aster-07-15", "minimax-m2.7",
    ];
    if MESSAGES.contains(&id.as_str()) {
        return WireKind::Messages;
    }
    // The proxy serves these through its own native generateContent client.
    if ["gemini-", "garnet-"].iter().any(|p| id.starts_with(p)) {
        return WireKind::Gemini;
    }
    // Muse Spark 1.3 is the one non-GPT/Grok id on the Responses route.
    if id == "muse-spark-1.3" || id.starts_with("gpt-") || id.starts_with("grok-") {
        return WireKind::Responses;
    }
    WireKind::Chat
}

/// The Factory upstream router for a model, sent as `x-api-provider`.
///
/// shortcut: each family's first rotation entry is pinned. The native client narrows this per
/// account region and live routing; upgrade when a gated or regional route must be selected.
pub fn upstream_for(model: &str) -> &'static str {
    let id = model.to_ascii_lowercase();
    match id.as_str() {
        "glm-5.3-flash" | "glm-5.3" | "kimi-k3.1-pro" | "kimi-k3" | "qwen3.8-max" | "beam-1.0"
        | "deepseek-v4.1-flash" | "deepseek-v4.1-flash-1017" | "deepseek-v4-flash-0731"
        | "deepseek-v4-pro" | "minimax-m3" | "minimax-m2.7" => "fireworks",
        "glm-5.2" | "glm-5.2-fast" | "nemotron-3-ultra" | "nemotron-3.5-super" | "muse-spark-open"
        | "muse-spark-1.2" | "inkling" => "baseten",
        "mistral-large-4" | "mistral-medium-3.5" => "mistral",
        "garnet-07-15" | "muse-spark-1.3" => "google",
        _ if id.starts_with("grok-") => "xai",
        _ if id.starts_with("gemini-") => "google",
        _ if id.starts_with("gpt-") => "openai",
        _ if id.starts_with("claude-") || id.starts_with("atlas-") || id.starts_with("aster-") => "anthropic",
        _ => "fireworks",
    }
}

/// Base URL for a residency region; the wire's own client appends its path.
pub fn base_for(kind: WireKind, region: Option<&str>) -> String {
    let host = if region == Some("eu") { "https://api.eu.factory.ai" } else { "https://api.factory.ai" };
    match kind {
        WireKind::Chat | WireKind::Responses => format!("{host}/api/llm/o/v1"),
        WireKind::Messages => format!("{host}/api/llm/a"),
        WireKind::Gemini => format!("{host}/api/llm/g/v1"),
    }
}

/// The model roster the native CLI ships: Factory has no model-listing endpoint, so discovery
/// cannot supply one (OMP `providers/factory-droid.kdl` `seed` rows).
pub const MODELS: &[&str] = &[
    "claude-fable-5.2", "claude-fable-5.1", "claude-fable-5", "claude-opus-5-5", "claude-opus-5-5-fast",
    "claude-opus-5", "claude-opus-5-fast", "claude-opus-4-8", "claude-opus-4-8-fast", "claude-opus-4-7",
    "claude-opus-4-6", "claude-opus-4-5-20251101", "claude-sonnet-5-5", "claude-sonnet-5",
    "claude-sonnet-4-6", "claude-sonnet-4-5-20250929", "claude-haiku-5-5", "claude-haiku-4-5-20251001",
    "gpt-6-astra", "gpt-6-astra-ultrafast", "gpt-6.1-sol", "gpt-6.1-sol-ultrafast", "gpt-6.1-terra",
    "gpt-6-sol", "gpt-6-sol-fast", "gpt-6-luna", "gpt-6-luna-fast", "gpt-5.6-sol", "gpt-5.6-sol-fast",
    "gpt-5.6-terra", "gpt-5.6-luna", "gpt-5.5", "gpt-5.5-fast", "gpt-5.5-pro", "gpt-5.4", "gpt-5.4-fast",
    "gpt-5.4-mini", "gpt-5.4-mini-fast", "gpt-5.3-codex", "gpt-5.3-codex-fast", "gpt-5.2",
    "gemini-3.1-pro-preview", "gemini-3.8-flash", "gemini-3.7-flash", "gemini-3.6-flash",
    "gemini-3.5-flash", "gemini-3-flash-preview", "grok-4.7", "grok-4.6", "grok-4.5",
    "muse-spark-1.3", "muse-spark-1.2", "muse-spark-open", "inkling", "mistral-large-4",
    "mistral-medium-3.5", "glm-5.3-flash", "glm-5.3", "glm-5.2", "glm-5.2-fast", "kimi-k3.1-pro",
    "kimi-k3", "qwen3.8-max", "nemotron-3.5-super", "nemotron-3-ultra", "beam-1.0",
    "deepseek-v4.1-flash-1017", "deepseek-v4.1-flash", "deepseek-v4-flash-0731", "deepseek-v4-pro",
    "minimax-m3", "minimax-m2.7", "atlas-07-21", "aster-07-15", "garnet-07-15",
];

/// Stable `x-session-id` per credential, so one sign-in's turns share Factory's usage attribution.
fn session_id(key: &str) -> String {
    use sha2::{Digest, Sha256};
    format!("{:x}", Sha256::digest(key.as_bytes()))[..32].to_string()
}

/// A WorkOS bearer plus the account scope stored with it.
pub struct Factory {
    pub http: reqwest::Client,
    pub key: String,
    pub org_id: Option<String>,
    pub region: Option<String>,
    pub effort: Option<String>,
    pub wire: Wire,
    /// Stable per-process id for `x-assistant-message-id`.
    message_id: std::sync::atomic::AtomicU64,
}

impl Factory {
    pub fn new(http: reqwest::Client, key: String, extra: &serde_json::Map<String, Value>, effort: Option<String>, wire: Wire) -> Factory {
        let text = |k: &str| extra.get(k).and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_string);
        Factory {
            http,
            key,
            org_id: text("org_id"),
            region: text("region"),
            effort,
            wire,
            message_id: Default::default(),
        }
    }

    /// Identity headers every Factory request carries. No `x-api-key`, SDK telemetry or Claude Code
    /// app headers: the proxy reads none of them.
    fn headers(&self, model: &str) -> Vec<(&'static str, String)> {
        let mut headers: Vec<(&'static str, String)> = vec![
            ("user-agent", format!("factory-cli/{CLIENT_VERSION}")),
            ("x-client-version", CLIENT_VERSION.into()),
            ("x-factory-client", "cli".into()),
            ("x-factory-request-surface", "cli_tui".into()),
            ("x-api-provider", upstream_for(model).to_string()),
            ("x-provider-routing-source", "registry_default".into()),
            ("x-session-id", session_id(&self.key)),
            ("x-assistant-message-id", format!("{:016x}", self.message_id.fetch_add(1, std::sync::atomic::Ordering::Relaxed))),
            ("accept", "application/json".into()),
        ];
        if let Some(org) = &self.org_id {
            headers.push(("x-factory-org-id", org.clone()));
        }
        headers
    }

    /// The caller's system prompt behind the Droid identity sentence, never replacing it.
    fn system(system: &str) -> String {
        if system.trim().is_empty() { DROID_PREFIX.to_string() } else { format!("{DROID_PREFIX}\n\n{system}") }
    }

    fn post(&self, url: String, headers: &[(&'static str, String)], body: &Value) -> reqwest::RequestBuilder {
        let mut r = self.http.post(url).bearer_auth(&self.key).json(body);
        for (k, v) in headers {
            r = r.header(*k, v.as_str());
        }
        r
    }
}

impl Provider for Factory {
    fn append_only(&self) -> bool {
        self.wire.append_only
    }

    fn complete<'a>(&'a self, req: Request<'a>, on_text: &'a (dyn Fn(&str) + Send + Sync)) -> BoxFut<'a, Result<Reply>> {
        Box::pin(async move {
            let kind = wire_for(req.model);
            if kind == WireKind::Gemini {
                bail!("{} は Factory の Gemini 経路で配信されるため、このバージョンでは使えません。別のモデルを選んでください", req.model)
            }
            let system = Self::system(req.system);
            let req = Request { system: &system, ..req };
            let base = base_for(kind, self.region.as_deref());
            let headers = self.headers(req.model);
            let retry = &self.wire.retry;
            match kind {
                WireKind::Messages => {
                    // `oauth: None` keeps x-api-key/bearer switching on the compatible-host path.
                    let client = anthropic::Anthropic {
                        http: self.http.clone(),
                        base: base.clone(),
                        key: self.key.clone(),
                        oauth: None,
                        effort: self.effort.clone(),
                        wire: self.wire.clone(),
                    };
                    let body = messages_body(&client, &req);
                    let resp = super::send(retry, || {
                        self.post(format!("{base}/v1/messages"), &headers, &body)
                            .header("anthropic-version", "2026-10-21")
                    }).await?;
                    let mut acc = anthropic::Acc::default();
                    super::sse(resp, self.wire.timeouts, |data| {
                        if data == "[DONE]" { return Ok(true) }
                        if let Ok(ev) = serde_json::from_str::<Value>(data) {
                            if let Some(t) = acc.feed(&ev)? { on_text(&t) }
                        }
                        Ok(false)
                    }).await?;
                    Ok(acc.finish())
                }
                WireKind::Responses => {
                    let body = responses::body(&responses::Shape {
                        model: req.model,
                        system: req.system,
                        messages: req.messages,
                        tools: req.tools,
                        effort: self.effort.as_deref(),
                        codex: false,
                    }, &self.wire);
                    let resp = super::send(retry, || self.post(format!("{base}/responses"), &headers, &body)).await?;
                    let mut acc = responses::Acc::default();
                    super::sse(resp, self.wire.timeouts, |data| {
                        if data == "[DONE]" { return Ok(true) }
                        if let Ok(ev) = serde_json::from_str::<Value>(data) {
                            if let Some(t) = acc.feed(&ev)? { on_text(&t) }
                        }
                        Ok(false)
                    }).await?;
                    Ok(acc.finish())
                }
                WireKind::Chat | WireKind::Gemini => {
                    let client = openai::OpenAi {
                        http: self.http.clone(),
                        base: base.clone(),
                        key: self.key.clone(),
                        copilot: false,
                        effort: self.effort.clone(),
                        wire: self.wire.clone(),
                    };
                    let body = client.body(&req);
                    let resp = super::send(retry, || self.post(format!("{base}/chat/completions"), &headers, &body)).await?;
                    let mut acc = openai::Acc::default();
                    super::sse(resp, self.wire.timeouts, |data| {
                        if data == "[DONE]" { return Ok(true) }
                        if let Ok(ev) = serde_json::from_str::<Value>(data) {
                            if let Some(t) = acc.feed(&ev)? { on_text(&t) }
                        }
                        Ok(false)
                    }).await?;
                    Ok(acc.finish())
                }
            }
        })
    }
}

/// A Messages body through Factory's proxy: no strict tools (proxied routes compile them into a
/// grammar the upstream may reject as oversized).
fn messages_body(client: &anthropic::Anthropic, req: &Request) -> Value {
    let mut body = client.body(req);
    if let Some(tools) = body["tools"].as_array_mut() {
        for tool in tools {
            if let Some(o) = tool.as_object_mut() { o.remove("strict"); }
        }
    }
    body
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Message, ToolDef};
    use serde_json::json;

    fn factory(region: Option<&str>, org: Option<&str>) -> Factory {
        let extra = serde_json::Map::from_iter(
            org.map(|o| ("org_id".to_string(), json!(o))).into_iter()
                .chain(region.map(|r| ("region".to_string(), json!(r)))),
        );
        Factory::new(reqwest::Client::new(), "workos-token".into(), &extra, Some("high".into()), Wire::default())
    }

    #[test]
    fn the_registry_route_decides_each_models_wire() {
        for model in ["claude-opus-5", "claude-haiku-4-5-20251001", "atlas-07-21", "minimax-m2.7"] {
            assert_eq!(wire_for(model), WireKind::Messages, "{model}");
        }
        for model in ["gpt-6-sol", "gpt-5.4-mini", "grok-4.7", "muse-spark-1.3"] {
            assert_eq!(wire_for(model), WireKind::Responses, "{model}");
        }
        for model in ["glm-5.3", "kimi-k3", "deepseek-v4-pro", "minimax-m3", "inkling", "mistral-medium-3.5"] {
            assert_eq!(wire_for(model), WireKind::Chat, "{model}");
        }
        for model in ["gemini-3.8-flash", "garnet-07-15", "gemini-3-pro-image-preview"] {
            assert_eq!(wire_for(model), WireKind::Gemini, "{model}");
        }
        // An id outside the roster keeps the proxy's own default route.
        assert_eq!(wire_for("some-future-model"), WireKind::Chat);
    }

    #[test]
    fn routing_pins_each_familys_first_upstream() {
        assert_eq!(upstream_for("glm-5.3"), "fireworks");
        assert_eq!(upstream_for("glm-5.2"), "baseten");
        assert_eq!(upstream_for("mistral-large-4"), "mistral");
        assert_eq!(upstream_for("muse-spark-1.3"), "google");
        assert_eq!(upstream_for("claude-opus-5"), "anthropic");
        assert_eq!(upstream_for("gpt-6-sol"), "openai");
        assert_eq!(upstream_for("grok-4.7"), "xai");
    }

    #[test]
    fn residency_chooses_the_host_and_the_wire_path() {
        assert_eq!(base_for(WireKind::Messages, None), "https://api.factory.ai/api/llm/a");
        assert_eq!(base_for(WireKind::Messages, Some("eu")), "https://api.eu.factory.ai/api/llm/a");
        assert_eq!(base_for(WireKind::Chat, Some("eu")), "https://api.eu.factory.ai/api/llm/o/v1");
        assert_eq!(base_for(WireKind::Responses, Some("global")), "https://api.factory.ai/api/llm/o/v1");
    }

    #[test]
    fn every_request_carries_the_cli_identity_headers() {
        let f = factory(Some("eu"), Some("fac-1"));
        let headers = f.headers("claude-opus-5");
        let get = |name: &str| headers.iter().find(|(k, _)| *k == name).map(|(_, v)| v.as_str());
        assert_eq!(get("user-agent"), Some("factory-cli/1.7.1"));
        assert_eq!(get("x-client-version"), Some(CLIENT_VERSION));
        assert_eq!(get("x-factory-client"), Some("cli"));
        assert_eq!(get("x-factory-request-surface"), Some("cli_tui"));
        assert_eq!(get("x-factory-org-id"), Some("fac-1"));
        assert_eq!(get("x-api-provider"), Some("anthropic"));
        assert_eq!(get("x-provider-routing-source"), Some("registry_default"));
        assert_eq!(get("accept"), Some("application/json"));
        assert_eq!(get("x-api-key"), None, "the proxy reads no API-key header");
        assert_eq!(get("authorization"), None, "auth is added by the request builder");
        let session = get("x-session-id").unwrap();
        assert_eq!(session.len(), 32);
        assert_ne!(get("x-assistant-message-id"), Some(session), "the message id is its own value");
        // Without a stored organization the header is omitted, not empty.
        assert!(factory(None, None).headers("glm-5.3").iter().all(|(k, _)| *k != "x-factory-org-id"));
    }

    #[test]
    fn the_droid_identity_precedes_the_callers_system_prompt() {
        assert_eq!(Factory::system(""), DROID_PREFIX);
        assert_eq!(Factory::system("  "), DROID_PREFIX);
        assert_eq!(Factory::system("be brief"), format!("{DROID_PREFIX}\n\nbe brief"));
    }

    #[test]
    fn messages_bodies_drop_strict_tools_and_keep_the_identity_prefix() {
        let f = factory(Some("eu"), None);
        let client = anthropic::Anthropic {
            http: reqwest::Client::new(),
            base: base_for(WireKind::Messages, Some("eu")),
            key: f.key.clone(),
            oauth: None,
            effort: None,
            wire: Wire::default(),
        };
        let messages = [Message::user("hello")];
        let tools = [ToolDef { name: "read".into(), description: "d".into(), schema: json!({"type": "object"}) }];
        let body = messages_body(&client, &Request {
            model: "claude-opus-5",
            system: &Factory::system("sys"),
            messages: &messages,
            tools: &tools,
            max_tokens: 100,
            live: None,
        });
        // The Messages client wraps the system prompt in a cache breakpoint block.
        let expected = format!("{DROID_PREFIX}\n\nsys");
        assert_eq!(body["system"][0]["text"], expected.as_str());
        assert_eq!(body["tools"][0]["name"], "read");
        assert!(body["tools"][0].get("strict").is_none(), "proxied routes reject strict schemas");
        assert_eq!(body["stream"], true);
    }
}
