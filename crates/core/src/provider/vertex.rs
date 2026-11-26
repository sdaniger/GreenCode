//! Google Vertex AI's Gemini wire (`providers/google-vertex.ts`): `generateContent` over SSE.
//!
//! Two auth paths, as in OMP: `GOOGLE_CLOUD_API_KEY` (or a configured key) sends `x-goog-api-key`
//! to the global/regional publisher endpoint; otherwise an ADC access token goes in `Authorization`
//! on the project-scoped endpoint. ADC tokens come from `GOOGLE_CLOUD_ACCESS_TOKEN` /
//! `CLOUDSDK_AUTH_ACCESS_TOKEN`, or the gcloud ADC file's `authorized_user` /
//! `external_account_authorized_user` refresh token.
//!
//! shortcut: `service_account` (RS256 JWT), impersonated service accounts and the GCE metadata
//! server are not ported — no RSA signer here. Upgrade when a keyless non-gcloud deployment must work.
use super::{send, sse, Provider, Reply, Request};
use crate::{Block, BoxFut, Message, Role, Usage};
use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Map, Value};
use std::sync::Mutex;

const OAUTH_TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
const API_VERSION: &str = "v1beta1";
const DEFAULT_BASE: &str = "https://aiplatform.googleapis.com";
/// Seconds of slack: a token this close to expiring is re-minted instead of sent.
const SKEW_MS: u64 = 60_000;

pub struct Vertex {
    pub http: reqwest::Client,
    /// Host root; the default resolves the per-location host, any other value is used verbatim.
    pub base: String,
    /// `GOOGLE_CLOUD_API_KEY` when non-empty: express mode, no project needed.
    pub key: String,
    pub project: Option<String>,
    pub location: Option<String>,
    /// A minted access token; bypasses ADC resolution (and the cache) when set.
    pub token: Option<String>,
    /// Token endpoint override for ADC refreshes; tests point it at a mock.
    pub token_url: String,
    /// ADC file override; defaults to the gcloud application-default path.
    pub adc: Option<std::path::PathBuf>,
    cache: Mutex<Option<(String, Option<u64>)>>,
    pub effort: Option<String>,
    pub wire: super::Wire,
}

impl Vertex {
    pub fn new(http: reqwest::Client, base: String, key: String, effort: Option<String>, wire: super::Wire) -> Self {
        Vertex {
            http,
            base,
            key,
            project: None,
            location: None,
            token: None,
            token_url: OAUTH_TOKEN_URL.into(),
            adc: None,
            cache: Mutex::new(None),
            effort,
            wire,
        }
    }

    fn env(names: &[&str]) -> Option<String> {
        names.iter().find_map(|name| std::env::var(name).ok().filter(|v| !v.trim().is_empty()))
    }

    fn project(&self) -> Result<String> {
        self.project.clone().or_else(|| Self::env(&["GOOGLE_CLOUD_PROJECT", "GCP_PROJECT", "GCLOUD_PROJECT"]))
            .ok_or_else(|| anyhow!("google-vertex にはプロジェクト ID が必要です。GOOGLE_CLOUD_PROJECT を設定してください"))
    }

    fn location(&self) -> Result<String> {
        self.location.clone().or_else(|| Self::env(&["GOOGLE_VERTEX_LOCATION", "GOOGLE_CLOUD_LOCATION", "VERTEX_LOCATION"]))
            .ok_or_else(|| anyhow!("google-vertex にはリージョンが必要です。GOOGLE_VERTEX_LOCATION を設定してください"))
    }

    /// `https://{location}-aiplatform.googleapis.com`, with `global`/`eu`/`us` on their own hosts.
    fn host(&self, location: &str) -> String {
        if self.base != DEFAULT_BASE && !self.base.is_empty() {
            return self.base.clone();
        }
        match location {
            "global" => "https://aiplatform.googleapis.com".into(),
            "eu" | "us" => format!("https://aiplatform.{location}.rep.googleapis.com"),
            other => format!("https://{other}-aiplatform.googleapis.com"),
        }
    }

    /// An access token, from the env override, the cache, or the ADC file.
    pub async fn access_token(&self) -> Result<String> {
        if let Some(token) = &self.token {
            return Ok(token.clone());
        }
        if let Some((token, expires)) = self.cache.lock().unwrap().clone() {
            if expires.is_none_or(|at| at > crate::login::now_ms() + SKEW_MS) {
                return Ok(token);
            }
        }
        let token = self.adc_token().await?;
        let (access, expires) = token;
        *self.cache.lock().unwrap() = Some((access.clone(), expires));
        Ok(access)
    }

    fn adc_path(&self) -> std::path::PathBuf {
        if let Some(path) = &self.adc {
            return path.clone();
        }
        if let Ok(path) = std::env::var("GOOGLE_APPLICATION_CREDENTIALS") {
            return path.into();
        }
        let home = std::env::var_os("HOME").map(std::path::PathBuf::from).unwrap_or_default();
        home.join(".config/gcloud/application_default_credentials.json")
    }

    /// `(access_token, expires_at_ms)` from the ADC file's refresh flow.
    async fn adc_token(&self) -> Result<(String, Option<u64>)> {
        let path = self.adc_path();
        let text = std::fs::read_to_string(&path).with_context(|| {
            format!("google-vertex の認証情報がありません（{}）。`gcloud auth application-default login` を実行するか、GOOGLE_CLOUD_ACCESS_TOKEN を設定してください", path.display())
        })?;
        let creds: Value = serde_json::from_str(&text).with_context(|| format!("invalid ADC file {}", path.display()))?;
        let form: Vec<(&str, String)> = match creds["type"].as_str() {
            Some("authorized_user") => vec![
                ("grant_type", "refresh_token".into()),
                ("client_id", string(&creds, "client_id")?),
                ("client_secret", string(&creds, "client_secret")?),
                ("refresh_token", string(&creds, "refresh_token")?),
            ],
            Some("external_account_authorized_user") => vec![
                ("grant_type", "refresh_token".into()),
                ("refresh_token", string(&creds, "refresh_token")?),
            ],
            Some(other) => bail!("google-vertex: 未対応の ADC 資格情報です（{other}）。`gcloud auth application-default login` でユーザー資格情報を作成してください"),
            None => bail!("google-vertex: ADC ファイルに type がありません"),
        };
        // Workforce pools authenticate the refresh with the client id/secret as HTTP Basic.
        let basic = (creds["type"] == "external_account_authorized_user").then(|| {
            let raw = format!("{}:{}", creds["client_id"].as_str().unwrap_or(""), creds["client_secret"].as_str().unwrap_or(""));
            format!("Basic {}", base64::Engine::encode(&base64::engine::general_purpose::STANDARD, raw))
        });
        let url = creds["token_url"].as_str().filter(|_| creds["type"] == "external_account_authorized_user").unwrap_or(&self.token_url);
        let pairs: Vec<(&str, &str)> = form.iter().map(|(k, v)| (*k, v.as_str())).collect();
        let auth = basic.as_deref().map(|b| ("Authorization", b));
        let v = crate::login::post_form(url, &pairs, auth.as_slice()).await?;
        let access = v["access_token"].as_str().filter(|s| !s.is_empty())
            .ok_or_else(|| anyhow!("google-vertex: トークン回答に access_token がありません"))?;
        Ok((access.into(), crate::login::expires_in(v["expires_in"].as_u64())))
    }

    fn body(&self, req: &Request) -> Value {
        let mut body = json!({
            "contents": contents(req.messages),
            "safetySettings": [
                {"category": "HARM_CATEGORY_HATE_SPEECH", "threshold": "OFF"},
                {"category": "HARM_CATEGORY_DANGEROUS_CONTENT", "threshold": "OFF"},
                {"category": "HARM_CATEGORY_SEXUALLY_EXPLICIT", "threshold": "OFF"},
                {"category": "HARM_CATEGORY_HARASSMENT", "threshold": "OFF"},
            ],
        });
        if !req.system.is_empty() {
            body["systemInstruction"] = json!({"parts": [{"text": req.system}]});
        }
        if !req.tools.is_empty() {
            let calls: Vec<Value> = req.tools.iter().map(|t| {
                json!({"name": t.name, "description": t.description, "parametersJsonSchema": t.schema})
            }).collect();
            body["tools"] = json!([{"functionDeclarations": calls}]);
        }
        let mut gen = json!({"maxOutputTokens": req.max_tokens});
        if self.wire.sampling.temperature >= 0. {
            gen["temperature"] = json!(self.wire.sampling.temperature);
        }
        if self.wire.sampling.top_p >= 0. {
            gen["topP"] = json!(self.wire.sampling.top_p);
        }
        body["generationConfig"] = gen;
        body
    }

    fn url(&self, model: &str) -> Result<String> {
        let path = if !self.key.is_empty() {
            format!("{API_VERSION}/publishers/google/models/{model}:streamGenerateContent?alt=sse")
        } else {
            let (project, location) = (self.project()?, self.location()?);
            return Ok(format!(
                "{}/{API_VERSION}/projects/{project}/locations/{location}/publishers/google/models/{model}:streamGenerateContent?alt=sse",
                self.host(&location).trim_end_matches('/')
            ));
        };
        // Express mode has no project; an ambient region still picks its host, else the global one.
        let location = self.location().unwrap_or_else(|_| "global".into());
        Ok(format!("{}/{}", self.host(&location).trim_end_matches('/'), path))
    }
}

fn string(v: &Value, key: &str) -> Result<String> {
    v[key].as_str().map(str::to_string).ok_or_else(|| anyhow!("ADC ファイルに {key} がありません"))
}

/// Messages as `contents`: assistant calls and results pair by name, thinking replays as thought parts.
fn contents(messages: &[Message]) -> Value {
    let mut names: std::collections::BTreeMap<&str, &str> = Default::default();
    for m in messages {
        for b in &m.content {
            if let Block::ToolUse { id, name, .. } = b {
                names.insert(id.as_str(), name.as_str());
            }
        }
    }
    let mut out: Vec<Value> = Vec::new();
    let mut push = |role: &str, parts: Vec<Value>| {
        if parts.is_empty() {
            return;
        }
        match out.last_mut() {
            Some(last) if last["role"] == role => {
                last["parts"].as_array_mut().unwrap().extend(parts);
            }
            _ => out.push(json!({"role": role, "parts": parts})),
        }
    };
    for m in messages {
        let role = match m.role {
            Role::User => "user",
            Role::Assistant => "model",
        };
        let mut parts = Vec::new();
        for b in &m.content {
            match b {
                Block::Text { text } if !text.trim().is_empty() => parts.push(json!({"text": text})),
                Block::ToolUse { name, input, .. } => {
                    // Gemini 3 wants a signature on calls it made; this bypass sentinel is the
                    // documented public-API value for replayed history the client cannot sign.
                    parts.push(json!({"functionCall": {"name": name, "args": input},
                        "thoughtSignature": "skip_thought_signature_validator"}));
                }
                Block::ToolResult { tool_use_id, content, is_error } => {
                    let name = names.get(tool_use_id.as_str()).copied().unwrap_or("tool");
                    let response = if *is_error { json!({"error": content}) } else { json!({"output": content}) };
                    parts.push(json!({"functionResponse": {"name": name, "response": response}}));
                }
                Block::Other(v) if v["type"] == "thinking" => {
                    if let Some(text) = v["thinking"].as_str().filter(|t| !t.trim().is_empty()) {
                        parts.push(json!({"text": text, "thought": true}));
                    }
                }
                Block::Other(v) if v["type"] == "image" => {
                    if let (Some(mime), Some(data)) = (v["source"]["media_type"].as_str(), v["source"]["data"].as_str()) {
                        parts.push(json!({"inlineData": {"mimeType": mime, "data": data}}));
                    }
                }
                _ => {}
            }
        }
        push(role, parts);
    }
    Value::Array(out)
}

impl Provider for Vertex {
    fn complete<'a>(&'a self, req: Request<'a>, on_text: &'a (dyn Fn(&str) + Send + Sync)) -> BoxFut<'a, Result<Reply>> {
        Box::pin(async move {
            let body = self.body(&req);
            let url = self.url(req.model)?;
            let token = if self.key.is_empty() { Some(self.access_token().await?) } else { None };
            let resp = send(&self.wire.retry, || {
                let r = self.http.post(&url).json(&body);
                match &token {
                    Some(token) => r.bearer_auth(token),
                    None => r.header("x-goog-api-key", &self.key),
                }
            })
            .await?;
            let mut acc = Acc::default();
            sse(resp, self.wire.timeouts, |data| {
                if data == "[DONE]" {
                    return Ok(true);
                }
                if let Ok(v) = serde_json::from_str::<Value>(data) {
                    if let Some(t) = acc.feed(&v)? {
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
    thinking: String,
    calls: Vec<Block>,
    usage: Usage,
    stop: String,
}

impl Acc {
    pub fn feed(&mut self, v: &Value) -> Result<Option<String>> {
        if let Some(message) = v["error"]["message"].as_str() {
            bail!("Google API error: {message}");
        }
        if let Some(u) = v["usageMetadata"].as_object() {
            self.usage = google_usage(u);
        }
        let Some(candidates) = v["candidates"].as_array() else { return Ok(None) };
        let mut delta = String::new();
        for candidate in candidates {
            if let Some(reason) = candidate["finishReason"].as_str() {
                self.stop = match reason {
                    "MAX_TOKENS" => "max_tokens",
                    "SAFETY" | "PROHIBITED_CONTENT" | "BLOCKLIST" | "SPII" | "RECITATION" => "refusal",
                    _ => "end_turn",
                }.into();
            }
            for part in candidate["content"]["parts"].as_array().into_iter().flatten() {
                if let Some(call) = part.get("functionCall") {
                    self.calls.push(Block::ToolUse {
                        id: call["id"].as_str().unwrap_or("").into(),
                        name: call["name"].as_str().unwrap_or("").into(),
                        input: call["args"].clone(),
                    });
                    self.stop = "tool_use".into();
                } else if let Some(text) = part["text"].as_str() {
                    if part["thought"] == true {
                        self.thinking.push_str(text);
                    } else if !text.is_empty() {
                        self.text.push_str(text);
                        delta.push_str(text);
                    }
                }
            }
        }
        Ok((!delta.is_empty()).then_some(delta))
    }

    pub fn finish(self) -> Reply {
        let mut blocks = Vec::new();
        if !self.thinking.is_empty() {
            blocks.push(Block::Other(json!({"type": "thinking", "thinking": self.thinking})));
        }
        if !self.text.is_empty() {
            blocks.push(Block::Text { text: self.text });
        }
        blocks.extend(self.calls);
        let stop = if self.stop.is_empty() { "end_turn".into() } else { self.stop };
        Reply { blocks, usage: self.usage, stop }
    }
}

/// `promptTokenCount` includes the cached part; output is candidates plus thoughts.
fn google_usage(u: &Map<String, Value>) -> Usage {
    let n = |k: &str| u.get(k).and_then(Value::as_u64).unwrap_or(0);
    let candidates = n("candidatesTokenCount");
    let thinking = n("thoughtsTokenCount");
    let total = n("totalTokenCount");
    let prompt = match n("promptTokenCount") {
        0 => total.saturating_sub(candidates + thinking),
        prompt => prompt,
    };
    let cache_read = n("cachedContentTokenCount").min(prompt);
    Usage { input: prompt - cache_read, output: candidates + thinking, cache_read, cache_write: 0 }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::localserver::{serve, Request as Http};
    use crate::provider::Request;
    use crate::ToolDef;

    fn client(base: &str, key: &str) -> Vertex {
        let mut v = Vertex::new(reqwest::Client::new(), base.into(), key.into(), Some("high".into()), crate::provider::Wire::default());
        v.project = Some("proj".into());
        v.location = Some("us-central1".into());
        v
    }

    async fn server(answer: &'static str) -> (String, tokio::task::JoinHandle<Http>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let (tx, rx) = tokio::sync::oneshot::channel();
        let tx = Mutex::new(Some(tx));
        tokio::spawn(async move {
            let _ = serve(listener, 1 << 20, move |req: Http| {
                let tx = tx.lock().unwrap().take();
                async move {
                    if let Some(tx) = tx {
                        let _ = tx.send(req);
                    }
                    crate::localserver::Response::new("200 OK", "text/event-stream", answer)
                }
            }).await;
        });
        (origin, tokio::spawn(async move { rx.await.unwrap() }))
    }

    fn sample() -> Value {
        json!({"candidates": [{"content": {"parts": [{"text": "hi"}]}, "finishReason": "STOP"}],
            "usageMetadata": {"promptTokenCount": 10, "cachedContentTokenCount": 4, "candidatesTokenCount": 3, "thoughtsTokenCount": 2}})
    }

    #[tokio::test]
    async fn adc_request_carries_the_bearer_and_project_path() {
        let (origin, recorded) = server(format!("data: {}\n", sample()).leak()).await;
        let mut v = client(&origin, "");
        v.token = Some("tok".into());
        let tools = [ToolDef { name: "t".into(), description: "d".into(), schema: json!({"type": "object"}) }];
        let reply = v.complete(
            Request { model: "gemini-3.1-pro-preview", system: "sys", messages: &[Message::user("yo")], tools: &tools, max_tokens: 100, live: None },
            &|_| {},
        ).await.unwrap();
        assert!(matches!(&reply.blocks[0], Block::Text { text } if text == "hi"));
        assert_eq!(reply.usage, Usage { input: 6, output: 5, cache_read: 4, cache_write: 0 });
        assert_eq!(reply.stop, "end_turn");
        let req = recorded.await.unwrap();
        assert!(req.path.contains("/v1beta1/projects/proj/locations/us-central1/publishers/google/models/gemini-3.1-pro-preview:streamGenerateContent"));
        assert_eq!(req.header("authorization"), Some("Bearer tok"));
        assert!(req.header("x-goog-api-key").is_none());
        let body: Value = serde_json::from_slice(&req.body).unwrap();
        assert_eq!(body["generationConfig"]["maxOutputTokens"], 100);
        assert_eq!(body["tools"][0]["functionDeclarations"][0]["parametersJsonSchema"]["type"], "object");
        assert_eq!(body["contents"][0]["parts"][0]["text"], "yo");
    }

    #[tokio::test]
    async fn api_key_uses_express_path_without_project() {
        let (origin, recorded) = server("data: [DONE]\n").await;
        let v = client(&origin, "gk");
        let _ = v.complete(Request { model: "gemini-3-flash", system: "", messages: &[Message::user("x")], tools: &[], max_tokens: 10, live: None }, &|_| {}).await.unwrap();
        let req = recorded.await.unwrap();
        assert!(req.path.starts_with("/v1beta1/publishers/google/models/gemini-3-flash:streamGenerateContent"));
        assert_eq!(req.query("alt"), Some("sse"));
        assert_eq!(req.header("x-goog-api-key"), Some("gk"));
        assert!(req.header("authorization").is_none());
    }

    #[tokio::test]
    async fn authorized_user_adc_refreshes_through_the_token_endpoint() {
        let (origin, recorded) = server(r#"{"access_token":"at","expires_in":3600}"#).await;
        let dir = std::env::temp_dir().join(format!("gc-vertex-{}.json", crate::login::random().unwrap()));
        std::fs::write(&dir, r#"{"type":"authorized_user","client_id":"cid","client_secret":"sec","refresh_token":"rt"}"#).unwrap();
        let mut v = client("http://unused", "");
        v.token_url = format!("{origin}/token");
        v.adc = Some(dir.clone());
        assert_eq!(v.access_token().await.unwrap(), "at");
        // Second call is cached: the mock only answered once.
        assert_eq!(v.access_token().await.unwrap(), "at");
        let req = recorded.await.unwrap();
        assert_eq!(req.path, "/token");
        let form = String::from_utf8(req.body).unwrap();
        assert!(form.contains("grant_type=refresh_token") && form.contains("client_id=cid") && form.contains("refresh_token=rt"));
        std::fs::remove_file(dir).ok();
    }

    #[test]
    fn tool_results_pair_by_name_and_thinking_replays_as_thought() {
        let messages = [
            Message { role: Role::Assistant, content: vec![
                Block::ToolUse { id: "c1".into(), name: "shell".into(), input: json!({"cmd": "ls"}) },
            ] },
            Message { role: Role::User, content: vec![Block::ToolResult { tool_use_id: "c1".into(), content: "ok".into(), is_error: false }] },
            Message { role: Role::Assistant, content: vec![Block::Other(json!({"type": "thinking", "thinking": "hm"}))] },
        ];
        let v = contents(&messages);
        assert_eq!(v[0]["parts"][0]["functionCall"]["name"], "shell");
        assert_eq!(v[1]["parts"][0]["functionResponse"]["name"], "shell");
        assert_eq!(v[2]["parts"][0]["thought"], true);
    }
}
