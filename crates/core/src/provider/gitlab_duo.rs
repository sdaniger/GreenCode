//! GitLab Duo Chat (`gitlab-duo`): one GitLab credential minted into a proxied AI-gateway
//! token, then the request routed by alias to Anthropic's Messages API or an OpenAI wire
//! (OMP `providers/gitlab-duo.ts`, routes from `compat/rules/runtime/behavior.kdl`).

use super::{
    anthropic, anthropic_oauth, openai, responses, sse, Provider, Reply, Request, Wire,
};
use crate::{Block, BoxFut};
use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    sync::{LazyLock, Mutex},
    time::{Duration, Instant},
};

const ANTHROPIC_PROXY: &str = "https://cloud.gitlab.com/ai/v1/proxy/anthropic";
const OPENAI_PROXY: &str = "https://cloud.gitlab.com/ai/v1/proxy/openai/v1";
const DIRECT_ACCESS_TTL: Duration = Duration::from_secs(25 * 60);

/// Public Duo aliases and the upstream model each one runs. The order is the model picker's.
pub const MODELS: &[(&str, &str)] = &[
    ("duo-chat-opus-4-6", "claude-opus-4-6"),
    ("duo-chat-sonnet-4-6", "claude-sonnet-4-6"),
    ("duo-chat-opus-4-5", "claude-opus-4-5-20251101"),
    ("duo-chat-sonnet-4-5", "claude-sonnet-4-5-20250929"),
    ("duo-chat-haiku-4-5", "claude-haiku-4-5-20251001"),
    ("duo-chat-gpt-5-1", "gpt-5.1-2025-11-13"),
    ("duo-chat-gpt-5-2", "gpt-5.2-2025-12-11"),
    ("duo-chat-gpt-5-mini", "gpt-5-mini-2025-08-07"),
    ("duo-chat-gpt-5-codex", "gpt-5-codex"),
    ("duo-chat-gpt-5-2-codex", "gpt-5.2-codex"),
];

/// Where one alias's request goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Route {
    Anthropic,
    Responses,
    Chat,
}

pub fn route(model: &str) -> Route {
    let id = model.to_ascii_lowercase();
    let claude = ["duo-chat-opus-", "duo-chat-sonnet-", "duo-chat-haiku-", "claude-opus-", "claude-sonnet-", "claude-haiku-"];
    if claude.iter().any(|prefix| id.starts_with(prefix)) {
        return Route::Anthropic;
    }
    if matches!(id.as_str(), "duo-chat-gpt-5-codex" | "duo-chat-gpt-5-2-codex" | "gpt-5-codex" | "gpt-5.2-codex") {
        return Route::Responses;
    }
    Route::Chat
}

/// The upstream model id for an alias, or for an upstream id already.
pub fn upstream(model: &str) -> Option<&'static str> {
    MODELS.iter().find(|(alias, _)| *alias == model).or_else(|| MODELS.iter().find(|(_, id)| *id == model)).map(|(_, id)| *id)
}

/// A minted gateway token and the headers GitLab wants beside it.
#[derive(Clone, Debug)]
struct Direct {
    token: String,
    headers: Vec<(String, String)>,
}

/// Keyed by the GitLab access token; a sign-in or refresh clears it.
static CACHE: LazyLock<Mutex<HashMap<String, (Instant, Direct)>>> = LazyLock::new(Default::default);

pub fn clear_cache() {
    CACHE.lock().unwrap().clear();
}

pub struct GitlabDuo {
    pub http: reqwest::Client,
    /// GitLab itself, for `direct_access`.
    pub base: String,
    /// OAuth access token, or a PAT from `GITLAB_TOKEN`.
    pub key: String,
    pub effort: Option<String>,
    pub wire: Wire,
    /// Overridable so tests can point both proxies at a local server.
    pub proxy_anthropic: String,
    pub proxy_openai: String,
}

impl GitlabDuo {
    pub fn new(http: reqwest::Client, base: String, key: String, effort: Option<String>, wire: Wire) -> Self {
        Self { http, base, key, effort, wire, proxy_anthropic: ANTHROPIC_PROXY.into(), proxy_openai: OPENAI_PROXY.into() }
    }

    /// The gateway token, cached for 25 minutes per access token.
    async fn direct_access(&self) -> Result<Direct> {
        if let Some((at, direct)) = CACHE.lock().unwrap().get(&self.key) {
            if at.elapsed() < DIRECT_ACCESS_TTL {
                return Ok(direct.clone());
            }
        }
        let url = format!("{}/api/v4/ai/third_party_agents/direct_access", self.base.trim_end_matches('/'));
        let resp = self.http.post(&url).bearer_auth(&self.key).timeout(Duration::from_secs(30))
            .json(&json!({"feature_flags": {"DuoAgentPlatformNext": true}}))
            .send().await.with_context(|| format!("POST {url}"))?;
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        if status.as_u16() == 403 {
            bail!("GitLab Duo が有効になっていません。GitLab アカウントで Duo を有効にするか、管理者に確認してください: {body}");
        }
        if !status.is_success() {
            bail!("GitLab Duo の direct_access に失敗しました（HTTP {status}）: {}", body.chars().take(500).collect::<String>());
        }
        let v: Value = serde_json::from_str(&body).context("GitLab Duo の direct_access 応答が JSON ではありません")?;
        let token = v["token"].as_str().filter(|t| !t.is_empty())
            .ok_or_else(|| anyhow!("GitLab Duo の direct_access 応答にトークンがありません"))?;
        let headers = v["headers"].as_object()
            .ok_or_else(|| anyhow!("GitLab Duo の direct_access 応答にヘッダーがありません"))?
            .iter()
            .filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_string())))
            .collect();
        let direct = Direct { token: token.into(), headers };
        CACHE.lock().unwrap().insert(self.key.clone(), (Instant::now(), direct.clone()));
        Ok(direct)
    }

    fn anthropic_body(&self, req: &Request<'_>, model: &str) -> Value {
        // Built as an API-key client: the proxy leg's identity edits are applied below.
        let client = anthropic::Anthropic {
            http: self.http.clone(),
            base: self.proxy_anthropic.clone(),
            key: String::new(),
            oauth: None,
            effort: None,
            wire: self.wire.clone(),
        };
        let shaped = Request { model, system: req.system, messages: req.messages, tools: req.tools, max_tokens: req.max_tokens, live: req.live };
        // The Duo Claude aliases run in budget mode; the effort itself never reaches the proxy.
        let mut body = client.body(&shaped);
        let system = body["system"].as_array_mut().expect("body always has a system block");
        system.insert(0, json!({"type": "text", "text": anthropic_oauth::SYSTEM_PREFIX}));
        // OAuth requests bill as Claude Code, whose tool names carry the `mcp__` prefix; replays must match.
        if let Some(tools) = body["tools"].as_array_mut() {
            for tool in tools {
                if let Some(name) = tool["name"].as_str() {
                    tool["name"] = json!(format!("{}{name}", anthropic_oauth::TOOL_PREFIX));
                }
            }
        }
        for block in body["messages"].as_array_mut().into_iter().flatten().filter_map(|m| m["content"].as_array_mut()).flatten() {
            if block["type"] == "tool_use" {
                if let Some(name) = block["name"].as_str() {
                    block["name"] = json!(format!("{}{name}", anthropic_oauth::TOOL_PREFIX));
                }
            }
        }
        // The Duo Claude aliases run in budget mode (OMP `thinking-mode "budget"`).
        let budget = match self.effort.as_deref() {
            Some("minimal") => Some(1024),
            Some(level) => self.wire.thinking.budget(level),
            None => None,
        };
        if let Some(budget) = budget.map(|b| b.min(req.max_tokens.saturating_sub(1))).filter(|b| *b >= 1024) {
            body["thinking"] = json!({"type": "enabled", "budget_tokens": budget});
            // The API rejects sampling parameters alongside a thinking budget.
            for key in ["temperature", "top_p", "top_k"] {
                body.as_object_mut().unwrap().remove(key);
            }
        }
        body
    }

    async fn anthropic(&self, req: &Request<'_>, model: &str, direct: &Direct, on_text: &(dyn Fn(&str) + Send + Sync)) -> Result<Reply> {
        let body = self.anthropic_body(req, model);
        let url = format!("{}/v1/messages?beta=true", self.proxy_anthropic.trim_end_matches('/'));
        let user_agent = anthropic_oauth::user_agent().await;
        let direct_headers = direct.headers.clone();
        let resp = super::send(&self.wire.retry, || {
            let mut r = self.http.post(&url)
                .header("accept", "application/json")
                .header("user-agent", user_agent)
                .header("anthropic-beta", anthropic_oauth::BETAS)
                .header("anthropic-dangerous-direct-browser-access", "true")
                .header("anthropic-version", "2023-06-01")
                .bearer_auth(&direct.token)
                .header("x-app", "cli")
                .json(&body);
            for (k, v) in &direct_headers {
                r = r.header(k, v);
            }
            r
        })
        .await?;
        let mut acc = anthropic::Acc::default();
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
        for block in &mut reply.blocks {
            if let Block::ToolUse { name, .. } = block {
                if let Some(bare) = name.strip_prefix(anthropic_oauth::TOOL_PREFIX) {
                    *name = bare.to_string();
                }
            }
        }
        Ok(reply)
    }

    async fn responses(&self, req: &Request<'_>, model: &str, direct: &Direct, on_text: &(dyn Fn(&str) + Send + Sync)) -> Result<Reply> {
        let shape = responses::Shape {
            model,
            system: req.system,
            messages: req.messages,
            tools: req.tools,
            effort: self.effort.as_deref(),
            codex: false,
        };
        let body = responses::body(&shape, &self.wire);
        let url = format!("{}/responses", self.proxy_openai.trim_end_matches('/'));
        let direct_headers = direct.headers.clone();
        let resp = super::send(&self.wire.retry, || {
            let mut r = self.http.post(&url).bearer_auth(&direct.token).json(&body);
            for (k, v) in &direct_headers {
                r = r.header(k, v);
            }
            r
        })
        .await?;
        let mut acc = responses::Acc::default();
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
    }

    async fn chat(&self, req: &Request<'_>, model: &str, direct: &Direct, on_text: &(dyn Fn(&str) + Send + Sync)) -> Result<Reply> {
        let client = openai::OpenAi {
            http: self.http.clone(),
            base: self.proxy_openai.clone(),
            key: String::new(),
            copilot: false,
            effort: None,
            wire: self.wire.clone(),
        };
        let mut body = client.body(&Request { model, system: req.system, messages: req.messages, tools: req.tools, max_tokens: req.max_tokens, live: req.live });
        // OMP's `duo-chat-gpt-5*` rule: the fronts reject explicit sampling params.
        for key in ["temperature", "top_p", "top_k", "min_p", "repetition_penalty", "presence_penalty", "frequency_penalty"] {
            body.as_object_mut().unwrap().remove(key);
        }
        if let Some(effort) = &self.effort {
            body["reasoning_effort"] = json!(effort);
        }
        // GPT-5 deployments take `max_completion_tokens`; `max_tokens` is a 400 there.
        if let Some(max) = body.as_object_mut().unwrap().remove("max_tokens") {
            body["max_completion_tokens"] = max;
        }
        let url = format!("{}/chat/completions", self.proxy_openai.trim_end_matches('/'));
        let direct_headers = direct.headers.clone();
        let resp = super::send(&self.wire.retry, || {
            let mut r = self.http.post(&url).bearer_auth(&direct.token).json(&body);
            for (k, v) in &direct_headers {
                r = r.header(k, v);
            }
            r
        })
        .await?;
        let mut acc = openai::Acc::default();
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
    }
}

impl Provider for GitlabDuo {
    fn append_only(&self) -> bool {
        self.wire.append_only
    }

    fn complete<'a>(&'a self, req: Request<'a>, on_text: &'a (dyn Fn(&str) + Send + Sync)) -> BoxFut<'a, Result<Reply>> {
        Box::pin(async move {
            let model = upstream(req.model)
                .ok_or_else(|| anyhow!("GitLab Duo のモデル `{}` は未対応です", req.model))?;
            let direct = self.direct_access().await?;
            match route(req.model) {
                Route::Anthropic => self.anthropic(&req, model, &direct, on_text).await,
                Route::Responses => self.responses(&req, model, &direct, on_text).await,
                Route::Chat => self.chat(&req, model, &direct, on_text).await,
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};

    const ANTHROPIC_SSE: &str = concat!(
        "data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":11}}}\n\n",
        "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"t1\",\"name\":\"mcp__read\"}}\n\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"path\\\":\\\"a\\\"}\"}}\n\n",
        "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"},\"usage\":{\"output_tokens\":3}}\n\n",
    );

    struct Recorded {
        path: String,
        headers: Vec<(String, String)>,
        body: String,
    }

    async fn read_request(socket: &mut TcpStream) -> Option<Recorded> {
        let mut data = Vec::new();
        let mut buf = [0u8; 4096];
        loop {
            let n = socket.read(&mut buf).await.ok()?;
            if n == 0 {
                return None;
            }
            data.extend_from_slice(&buf[..n]);
            let Some(split) = data.windows(4).position(|w| w == b"\r\n\r\n") else { continue };
            let head = String::from_utf8_lossy(&data[..split]).to_string();
            let length = head.lines().find_map(|line| {
                line.to_ascii_lowercase().strip_prefix("content-length:").and_then(|v| v.trim().parse::<usize>().ok())
            }).unwrap_or(0);
            if data.len() < split + 4 + length {
                continue;
            }
            let mut lines = head.lines();
            let path = lines.next().unwrap_or_default().split_whitespace().nth(1).unwrap_or_default().to_string();
            return Some(Recorded {
                path,
                headers: lines.filter_map(|line| line.split_once(':').map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string()))).collect(),
                body: String::from_utf8_lossy(&data[split + 4..split + 4 + length]).to_string(),
            });
        }
    }

    async fn respond(socket: &mut TcpStream, status: &str, content_type: &str, body: &str) {
        let head = format!("HTTP/1.1 {status}\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\n", body.len());
        let _ = socket.write_all(format!("{head}\r\n{body}").as_bytes()).await;
        let _ = socket.shutdown().await;
    }

    #[test]
    fn aliases_route_and_map_to_upstream_ids() {
        assert_eq!(route("duo-chat-opus-4-6"), Route::Anthropic);
        assert_eq!(route("duo-chat-haiku-4-5"), Route::Anthropic);
        assert_eq!(route("duo-chat-gpt-5-codex"), Route::Responses);
        assert_eq!(route("duo-chat-gpt-5-2-codex"), Route::Responses);
        assert_eq!(route("duo-chat-gpt-5-1"), Route::Chat);
        assert_eq!(route("claude-opus-4-6"), Route::Anthropic);
        assert_eq!(route("gpt-5-codex"), Route::Responses);
        assert_eq!(upstream("duo-chat-opus-4-5"), Some("claude-opus-4-5-20251101"));
        assert_eq!(upstream("duo-chat-gpt-5-mini"), Some("gpt-5-mini-2025-08-07"));
        assert_eq!(upstream("claude-opus-4-6"), Some("claude-opus-4-6"));
        assert_eq!(upstream("nope"), None);
        // Every alias the picker lists resolves.
        assert!(MODELS.iter().all(|(alias, _)| upstream(alias).is_some()));
    }

    /// A distinct GitLab token per test: the direct-access cache is global and keyed by it.
    fn client(origin: &str, key: &str, effort: Option<&str>) -> GitlabDuo {
        let mut client = GitlabDuo::new(reqwest::Client::new(), origin.into(), key.into(), effort.map(String::from), Wire::default());
        client.proxy_anthropic = format!("{origin}/proxy/anthropic");
        client.proxy_openai = format!("{origin}/proxy/openai/v1");
        client
    }

    /// `direct_access`, then one Anthropic turn: the proxy must see the minted token, GitLab's
    /// headers, the Claude Code identity and the upstream model id.
    #[tokio::test]
    async fn direct_access_mints_the_token_and_the_anthropic_leg_carries_it() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
        let log = std::sync::Arc::new(Mutex::new(Vec::<Recorded>::new()));
        let server = {
            let log = log.clone();
            tokio::spawn(async move {
                for _ in 0..3 {
                    let (mut socket, _) = listener.accept().await.unwrap();
                    let request = read_request(&mut socket).await.unwrap();
                    let direct = request.path.contains("direct_access");
                    log.lock().unwrap().push(request);
                    if direct {
                        respond(&mut socket, "200 OK", "application/json",
                            r#"{"token":"direct-token","headers":{"x-gitlab-shared-secret":"s3cr3t"}}"#).await;
                    } else {
                        respond(&mut socket, "200 OK", "text/event-stream", ANTHROPIC_SSE).await;
                    }
                }
            })
        };
        let client = client(&origin, "gl-token-anthropic", Some("high"));
        let tools = [crate::ToolDef { name: "read".into(), description: "r".into(), schema: json!({"type": "object"}) }];
        let messages = [crate::Message::user("hi")];
        let text = std::sync::Mutex::new(String::new());
        let one = Request { model: "duo-chat-opus-4-6", system: "sys", messages: &messages, tools: &tools, max_tokens: 20_000, live: None };
        let reply = client.complete(one, &|t| text.lock().unwrap().push_str(t)).await.unwrap();
        assert!(text.lock().unwrap().is_empty());
        assert!(matches!(&reply.blocks[0], Block::ToolUse { name, .. } if name == "read"), "{:?}", reply.blocks);
        assert_eq!(reply.stop, "tool_use");

        // A second request reuses the cached direct-access token: only one more HTTP call.
        let two = Request { model: "duo-chat-opus-4-6", system: "sys", messages: &messages, tools: &[], max_tokens: 4000, live: None };
        client.complete(two, &|_| {}).await.unwrap();
        server.await.unwrap();

        let requests = log.lock().unwrap();
        assert_eq!(requests.len(), 3, "{:?}", requests.iter().map(|r| &r.path).collect::<Vec<_>>());
        let direct = &requests[0];
        assert_eq!(direct.path, "/api/v4/ai/third_party_agents/direct_access");
        assert_eq!(direct.headers.iter().find(|(k, _)| k == "authorization").map(|(_, v)| v.as_str()), Some("Bearer gl-token-anthropic"));
        assert_eq!(serde_json::from_str::<Value>(&direct.body).unwrap(), json!({"feature_flags": {"DuoAgentPlatformNext": true}}));

        let proxied = &requests[1];
        assert_eq!(proxied.path, "/proxy/anthropic/v1/messages?beta=true");
        let header = |name: &str| proxied.headers.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str());
        assert_eq!(header("authorization"), Some("Bearer direct-token"));
        assert_eq!(header("x-gitlab-shared-secret"), Some("s3cr3t"));
        assert_eq!(header("anthropic-version"), Some("2023-06-01"));
        assert_eq!(header("anthropic-dangerous-direct-browser-access"), Some("true"));
        assert_eq!(header("x-app"), Some("cli"));
        assert!(header("anthropic-beta").unwrap().contains("claude-code-20250219"));
        let body: Value = serde_json::from_str(&proxied.body).unwrap();
        assert_eq!(body["model"], "claude-opus-4-6");
        assert_eq!(body["system"][0]["text"], anthropic_oauth::SYSTEM_PREFIX);
        assert_eq!(body["tools"][0]["name"], "mcp__read");
        assert_eq!(body["thinking"]["budget_tokens"], 16384);
        assert!(body.get("output_config").is_none());
    }

    /// The GPT aliases: codex goes to Responses, the rest to chat completions, each with the
    /// upstream model id and the minted token, and one direct-access call for both.
    #[tokio::test]
    async fn the_gpt_aliases_go_to_the_right_openai_wire() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
        let log = std::sync::Arc::new(Mutex::new(Vec::<Recorded>::new()));
        let server = {
            let log = log.clone();
            tokio::spawn(async move {
                for _ in 0..3 {
                    let (mut socket, _) = listener.accept().await.unwrap();
                    let request = read_request(&mut socket).await.unwrap();
                    let direct = request.path.contains("direct_access");
                    log.lock().unwrap().push(request);
                    if direct {
                        respond(&mut socket, "200 OK", "application/json", r#"{"token":"direct-token","headers":{}}"#).await;
                    } else {
                        respond(&mut socket, "200 OK", "text/event-stream", "data: [DONE]\n\n").await;
                    }
                }
            })
        };
        let client = client(&origin, "gl-token-openai", Some("low"));
        let messages = [crate::Message::user("hi")];
        for model in ["duo-chat-gpt-5-codex", "duo-chat-gpt-5-1"] {
            client.complete(Request { model, system: "s", messages: &messages, tools: &[], max_tokens: 100, live: None }, &|_| {}).await.unwrap();
        }
        server.await.unwrap();
        let requests = log.lock().unwrap();
        assert_eq!(requests.len(), 3);
        assert_eq!(requests[1].path, "/proxy/openai/v1/responses");
        assert_eq!(requests[2].path, "/proxy/openai/v1/chat/completions");
        for (request, expected) in [(&requests[1], "gpt-5-codex"), (&requests[2], "gpt-5.1-2025-11-13")] {
            assert_eq!(serde_json::from_str::<Value>(&request.body).unwrap()["model"], expected);
            assert_eq!(request.headers.iter().find(|(k, _)| k == "authorization").map(|(_, v)| v.as_str()), Some("Bearer direct-token"));
        }
    }

    /// 403 means Duo is not enabled for the account: a dedicated Japanese message, and no retry.
    #[tokio::test]
    async fn a_forbidden_direct_access_says_duo_is_not_enabled() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            read_request(&mut socket).await.unwrap();
            respond(&mut socket, "403 Forbidden", "application/json", r#"{"message":"403 Forbidden"}"#).await;
        });
        let client = client(&origin, "gl-token-403", None);
        clear_cache();
        let messages = [crate::Message::user("hi")];
        let err = client.complete(Request { model: "duo-chat-gpt-5-1", system: "", messages: &messages, tools: &[], max_tokens: 100, live: None }, &|_| {}).await.unwrap_err();
        assert!(err.to_string().contains("GitLab Duo が有効になっていません"), "{err}");
    }
}
