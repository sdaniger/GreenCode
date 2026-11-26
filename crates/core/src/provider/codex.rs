//! ChatGPT (Codex) subscription: device-flow sign-in and the Responses API it serves.

use super::{send, sse, Provider, Reply, Request};
use crate::BoxFut;
use anyhow::{anyhow, bail, Result};
use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{path::{Path, PathBuf}, time::{Duration, SystemTime, UNIX_EPOCH}};

pub const BASE: &str = "https://chatgpt.com/backend-api/codex";
const ORIGIN: &str = "https://auth.openai.com";
const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";

#[derive(Debug)]
pub struct Device {
    pub user_code: String,
    pub verification_uri: String,
    device_auth_id: String,
    interval: u64,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Tokens {
    pub access: String,
    pub refresh: String,
    pub expires_at: u64,
}

pub fn path() -> PathBuf { crate::home().join("chatgpt-oauth.json") }
#[derive(Clone, Debug)]
pub struct ModelInfo {
    pub id: String,
    pub efforts: Vec<String>,
}

fn models_cache_path() -> PathBuf { crate::user_home().join(".codex/models_cache.json") }

/// Reads the Codex CLI's public model catalog; no credentials are read.
pub fn models() -> Vec<ModelInfo> { models_at(&models_cache_path()) }

pub fn models_at(path: &Path) -> Vec<ModelInfo> {
    let Ok(text) = std::fs::read_to_string(path) else { return vec![] };
    let Ok(value) = serde_json::from_str::<Value>(&text) else { return vec![] };
    value["models"].as_array().into_iter().flatten().filter_map(|model| {
        let id = model["slug"].as_str().filter(|id| !id.is_empty())?;
        if model["supported_in_api"].as_bool() == Some(false) || model["visibility"].as_str() == Some("hide") { return None; }
        let efforts = model["supported_reasoning_levels"].as_array().into_iter().flatten()
            .filter_map(|level| level["effort"].as_str().map(String::from)).collect();
        Some(ModelInfo { id: id.into(), efforts })
    }).collect()
}

pub fn efforts(model: &str) -> Option<Vec<String>> {
    models().into_iter().find(|info| info.id.eq_ignore_ascii_case(model)).map(|info| info.efforts)
}

#[cfg(test)]
mod catalog_tests {
    use super::*;

    #[test]
    fn reads_api_models_and_reasoning_levels_without_hidden_entries() {
        let dir = std::env::temp_dir().join(format!("greencode-codex-models-{}", std::process::id()));
        let path = dir.join("models.json");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&path, r#"{"models":[{"slug":"gpt-6.1-sol","supported_in_api":true,"visibility":"list","supported_reasoning_levels":[{"effort":"low"},{"effort":"max"},{"effort":"ultra"}]},{"slug":"hidden","supported_in_api":true,"visibility":"hide"},{"slug":"local","supported_in_api":false}]}"#).unwrap();
        let models = models_at(&path);
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].id, "gpt-6.1-sol");
        assert_eq!(models[0].efforts, ["low", "max", "ultra"]);
        let _ = std::fs::remove_dir_all(dir);
    }
}


/// Adopts an existing Codex CLI sign-in (`~/.codex/auth.json`, `auth_mode: chatgpt`).
pub fn import_codex_cli() -> Result<Option<Tokens>> {
    import_codex_cli_at(&crate::user_home().join(".codex/auth.json"))
}

/// `import_codex_cli` against an explicit path.
pub fn import_codex_cli_at(path: &Path) -> Result<Option<Tokens>> {
    if !path.is_file() { return Ok(None); }
    let value: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(path)?)?;
    if value["auth_mode"].as_str() != Some("chatgpt") { return Ok(None); }
    let Some(access) = value["tokens"]["access_token"].as_str() else { return Ok(None) };
    // The refresh token is deliberately not copied: refreshing it would rotate the credential and
    // sign the Codex CLI out. The borrowed access token is used until its own `exp`.
    let expires_at = jwt_expiry(access).unwrap_or_else(|| now() + 1800);
    Ok(Some(Tokens { access: access.to_string(), refresh: String::new(), expires_at }))
}

/// Writes the imported credentials. A real login (one with a refresh token) is never replaced;
/// a previously borrowed token (empty refresh token) is, so re-importing renews it.
#[cfg(unix)]
pub fn import_into(path: &Path) -> Result<Option<Tokens>> {
    import_into_from(path, &crate::user_home().join(".codex/auth.json"))
}

#[cfg(unix)]
pub fn import_into_from(path: &Path, source: &Path) -> Result<Option<Tokens>> {
    let borrowed = match std::fs::read_to_string(path) {
        Ok(text) => serde_json::from_str::<Tokens>(&text).is_ok_and(|t| t.refresh.is_empty()),
        Err(_) => true,
    };
    if path.exists() && !borrowed { return Ok(None); }
    let Some(tokens) = import_codex_cli_at(source)? else { return Ok(None) };
    save(path, &tokens)?;
    Ok(Some(tokens))
}

fn now() -> u64 { SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs()) }

async fn post_json(url: &str, body: &Value) -> Result<reqwest::Response> {
    Ok(reqwest::Client::new().post(url).timeout(Duration::from_secs(30)).json(body).send().await?)
}

async fn check(resp: reqwest::Response) -> Result<Value> {
    let status = resp.status();
    let text = resp.text().await?;
    if !status.is_success() { bail!("HTTP {status}: {text}") }
    Ok(serde_json::from_str(&text)?)
}

pub async fn start() -> Result<Device> { start_at(ORIGIN).await }

pub async fn start_at(origin: &str) -> Result<Device> {
    let v = check(post_json(&format!("{origin}/api/accounts/deviceauth/usercode"), &json!({"client_id": CLIENT_ID})).await?).await?;
    let (Some(device_auth_id), Some(user_code)) = (v["device_auth_id"].as_str(), v["user_code"].as_str()) else { bail!("invalid device code response") };
    let interval = v["interval"].as_u64().or_else(|| v["interval"].as_str().and_then(|s| s.trim().parse().ok())).unwrap_or(5);
    Ok(Device { user_code: user_code.into(), verification_uri: format!("{origin}/codex/device"), device_auth_id: device_auth_id.into(), interval })
}

pub async fn finish(d: Device) -> Result<Tokens> { finish_at(ORIGIN, d, Duration::from_secs(1)).await }

/// `unit` is the length of one "second" of waiting, so tests can run fast.
pub async fn finish_at(origin: &str, d: Device, unit: Duration) -> Result<Tokens> {
    let step = d.interval.max(1) + 3;
    for poll in 0..120 {
        tokio::time::sleep(unit * (if poll == 0 { step.min(5) } else { step }) as u32).await;
        let resp = post_json(&format!("{origin}/api/accounts/deviceauth/token"), &json!({"device_auth_id": d.device_auth_id, "user_code": d.user_code})).await?;
        if matches!(resp.status().as_u16(), 403 | 404) { continue; }
        let v = check(resp).await?;
        let (Some(code), Some(verifier)) = (v["authorization_code"].as_str(), v["code_verifier"].as_str()) else { bail!("invalid device token response") };
        return token_request(origin, &[
            ("grant_type", "authorization_code"),
            ("client_id", CLIENT_ID),
            ("code", code),
            ("code_verifier", verifier),
            ("redirect_uri", "https://auth.openai.com/deviceauth/callback"),
        ], None).await;
    }
    bail!("Device flow timed out")
}

async fn token_request(origin: &str, form: &[(&str, &str)], old_refresh: Option<&str>) -> Result<Tokens> {
    let body = url::form_urlencoded::Serializer::new(String::new()).extend_pairs(form).finish();
    let resp = reqwest::Client::new().post(format!("{origin}/oauth/token"))
        .timeout(Duration::from_secs(30))
        .header("Content-Type", "application/x-www-form-urlencoded")
        .header("Accept", "application/json")
        .body(body).send().await?;
    let v = check(resp).await?;
    let access = v["access_token"].as_str().filter(|s| !s.is_empty()).ok_or_else(|| anyhow!("token response has no access_token"))?;
    let refresh = v["refresh_token"].as_str().filter(|s| !s.is_empty()).or(old_refresh).ok_or_else(|| anyhow!("token response has no refresh_token"))?;
    Ok(Tokens { access: access.into(), refresh: refresh.into(), expires_at: now() + v["expires_in"].as_u64().unwrap_or(3600) })
}

#[cfg(unix)]
pub fn save(path: &Path, t: &Tokens) -> Result<()> {
    use std::{io::Write, os::unix::fs::OpenOptionsExt};
    let parent = path.parent().ok_or_else(|| anyhow!("credential path has no parent"))?;
    std::fs::create_dir_all(parent)?;
    let mut random = [0u8; 32];
    getrandom::fill(&mut random).map_err(|e| anyhow!("random generation failed: {e}"))?;
    let tmp = parent.join(format!(".chatgpt-oauth-{}", base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(random)));
    let result = (|| -> Result<()> {
        let mut file = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&tmp)?;
        serde_json::to_writer_pretty(&mut file, t)?;
        file.flush()?;
        file.sync_all()?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    })();
    if result.is_err() { let _ = std::fs::remove_file(&tmp); }
    result
}

#[cfg(not(unix))]
pub fn save(_path: &Path, _t: &Tokens) -> Result<()> {
    bail!("ChatGPT OAuth credential storage requires Unix")
}

/// A valid access token, refreshed (and the rotated tokens saved) when it expires within a minute.
pub async fn access_token(path: &Path) -> Result<String> { access_token_at(ORIGIN, path).await }

#[cfg(unix)]
pub async fn access_token_at(origin: &str, path: &Path) -> Result<String> {
    use std::os::unix::fs::OpenOptionsExt;
    let parent = path.parent().ok_or_else(|| anyhow!("credential path has no parent"))?;
    std::fs::create_dir_all(parent)?;
    let lock = std::fs::OpenOptions::new().write(true).create(true).truncate(false).mode(0o600).open(path.with_extension("lock"))?;
    loop {
        match lock.try_lock() {
            Ok(()) => break,
            Err(std::fs::TryLockError::WouldBlock) => tokio::time::sleep(Duration::from_millis(50)).await,
            Err(std::fs::TryLockError::Error(e)) => return Err(e.into()),
        }
    }
    let text = std::fs::read_to_string(path).map_err(|_| anyhow!("chatgpt: not signed in; run /login chatgpt"))?;
    let t: Tokens = serde_json::from_str(&text).map_err(|_| anyhow!("{} is not valid; delete it and run /login chatgpt", path.display()))?;
    if t.expires_at > now().saturating_add(60) { return Ok(t.access); }
    anyhow::ensure!(!t.refresh.is_empty(), "the imported Codex CLI token expired; sign in again with /login chatgpt (the Codex CLI login is left untouched)");
    let fresh = token_request(origin, &[("grant_type", "refresh_token"), ("client_id", CLIENT_ID), ("refresh_token", &t.refresh)], Some(&t.refresh)).await?;
    save(path, &fresh)?;
    Ok(fresh.access)
}

#[cfg(not(unix))]
pub async fn access_token_at(_origin: &str, _path: &Path) -> Result<String> {
    bail!("ChatGPT OAuth credential storage requires Unix")
}

/// The access token's own `exp` (seconds), so a borrowed token is not refreshed early.
fn jwt_expiry(token: &str) -> Option<u64> {
    let payload = token.split('.').nth(1)?.trim_end_matches('=');
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(payload).ok()?;
    serde_json::from_slice::<Value>(&bytes).ok()?.get("exp")?.as_u64()
}

fn jwt_claims(token: &str) -> Option<Value> {
    let payload = token.split('.').nth(1)?.trim_end_matches('=');
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(payload).ok()?;
    serde_json::from_slice::<Value>(&bytes).ok()?.get("https://api.openai.com/auth").cloned()
}

/// (account id, data residency) from the access token's claims.
pub fn account(access: &str) -> (Option<String>, Option<String>) {
    let Some(c) = jwt_claims(access) else { return (None, None) };
    let text = |k: &str| c[k].as_str().filter(|s| !s.is_empty()).map(str::to_string);
    (text("chatgpt_account_id"), text("chatgpt_data_residency").or_else(|| text("chatgpt_compute_residency")))
}

pub struct Codex {
    pub http: reqwest::Client,
    pub base: String,
    pub path: PathBuf,
    pub effort: Option<String>,
    session: String,
    wire: super::Wire,
}

impl Codex {
    pub fn new(base: String, path: PathBuf, effort: Option<String>) -> Codex {
        let mut bytes = [0u8; 16];
        let _ = getrandom::fill(&mut bytes);
        let session = bytes.iter().map(|b| format!("{b:02x}")).collect();
        Codex { http: reqwest::Client::new(), base, path, effort, session, wire: super::Wire::default() }
    }

    /// Applies the provider request settings resolved from `config.toml`.
    pub fn with_wire(mut self, wire: super::Wire) -> Codex {
        self.wire = wire;
        self
    }

    pub fn body(&self, req: &Request) -> Value {
        super::responses::body(
            &super::responses::Shape {
                model: req.model,
                system: req.system,
                messages: req.messages,
                tools: req.tools,
                effort: self.effort.as_deref(),
                codex: true,
            },
            &self.wire,
        )
    }

    /// ChatGPT backend headers; `state` is the rotating session id.
    fn headers(&self, token: &str, account: Option<&str>, residency: Option<&str>, session: &str) -> Vec<(&'static str, String)> {
        let mut headers = vec![
            ("authorization", format!("Bearer {token}")),
            ("originator", "greencode".into()),
            ("User-Agent", concat!("GreenCode/", env!("CARGO_PKG_VERSION")).into()),
            ("session_id", session.to_string()),
        ];
        if let Some(account) = account {
            headers.push(("ChatGPT-Account-ID", account.to_string()));
        }
        if let Some(residency) = residency {
            headers.push(("x-openai-internal-codex-residency", residency.to_string()));
        }
        headers
    }
}

impl Provider for Codex {
    fn append_only(&self) -> bool {
        self.wire.append_only
    }

    fn live_steering(&self, model: &str) -> bool {
        // Steering is a GPT-6 Responses feature and needs the socket it rides on.
        self.wire.websockets == crate::config::provider::Websockets::On
            && model.to_ascii_lowercase().starts_with("gpt-6")
    }

    fn complete<'a>(&'a self, req: Request<'a>, on_text: &'a (dyn Fn(&str) + Send + Sync)) -> BoxFut<'a, Result<Reply>> {
        Box::pin(async move {
            let body = self.body(&req);
            let token = access_token(&self.path).await?;
            let (account, residency) = account(&token);
            let headers = self.headers(&token, account.as_deref(), residency.as_deref(), &self.session);
            // The socket carries steering and skips the per-turn HTTP handshake; SSE stays the fallback.
            if self.wire.websockets == crate::config::provider::Websockets::On {
                let shown = std::sync::atomic::AtomicBool::new(false);
                let guarded = |t: &str| {
                    shown.store(true, std::sync::atomic::Ordering::Relaxed);
                    on_text(t);
                };
                match super::responses::complete(&self.base, "responses", &headers, &body, &self.wire, req.live, &guarded).await {
                    Ok(reply) => return Ok(reply),
                    // A partial answer was already shown; retrying would duplicate it.
                    Err(error) if shown.load(std::sync::atomic::Ordering::Relaxed) => return Err(error),
                    Err(_) => {}
                }
            }
            let resp = send(&self.wire.retry, || {
                let mut r = self.http.post(format!("{}/responses", self.base))
                    .bearer_auth(&token)
                    .header("originator", "greencode")
                    .header("User-Agent", concat!("GreenCode/", env!("CARGO_PKG_VERSION")))
                    .header("session_id", &self.session)
                    .header("Accept", "text/event-stream")
                    .json(&body);
                if let Some(a) = &account { r = r.header("ChatGPT-Account-ID", a); }
                if let Some(v) = &residency { r = r.header("x-openai-internal-codex-residency", v); }
                r
            }).await?;
            let mut acc = super::responses::Acc::default();
            sse(resp, self.wire.timeouts, |data| {
                if let Ok(ev) = serde_json::from_str::<Value>(data) {
                    if let Some(t) = acc.feed(&ev)? { on_text(&t); }
                }
                Ok(false)
            }).await?;
            Ok(acc.finish())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codex_cli_credentials_are_borrowed_without_the_refresh_token() {
        let dir = std::env::temp_dir().join(format!("greencode-codex-import-{}-{}", std::process::id(), std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("auth.json");
        // A JWT-shaped access token whose payload carries exp = 2_000_000_000.
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(br#"{"exp":2000000000}"#);
        std::fs::write(&path, format!(r#"{{"auth_mode":"chatgpt","tokens":{{"access_token":"h.{payload}.s","refresh_token":"rotating"}}}}"#)).unwrap();
        let tokens = import_codex_cli_at(&path).unwrap().unwrap();
        assert_eq!(tokens.expires_at, 2_000_000_000);
        assert!(tokens.refresh.is_empty(), "the rotating refresh token must not be copied");
        // Re-importing renews a borrowed token; a real login (non-empty refresh) is kept.
        let target = dir.join("chatgpt-oauth.json");
        assert!(import_into_from(&target, &path).unwrap().is_some());
        assert!(import_into_from(&target, &path).unwrap().is_some(), "borrowed tokens can be re-imported");
        save(&target, &Tokens { access: "real".into(), refresh: "r".into(), expires_at: u64::MAX }).unwrap();
        assert!(import_into_from(&target, &path).unwrap().is_none());
        assert_eq!(serde_json::from_slice::<Tokens>(&std::fs::read(&target).unwrap()).unwrap().access, "real");
        assert!(import_codex_cli_at(&dir.join("missing.json")).unwrap().is_none());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn codex_request_pins_a_stable_prefix_key_and_no_cache_options() {
        use crate::config::provider::{AppendOnly, CacheRetention};
        let client = |retention, append_only| Codex::new(BASE.into(), path(), None).with_wire(super::super::Wire {
            cache_retention: retention,
            append_only,
            ..Default::default()
        });
        fn req<'a>(messages: &'a [crate::Message], system: &'a str) -> Request<'a> {
            Request { model: "gpt-6-sol", system, messages, tools: &[], max_tokens: 300, live: None }
        }
        let messages = [crate::Message::user("hi")];
        let body = client(CacheRetention::Long, false).body(&req(&messages, "sys"));
        assert_eq!(body["stream"], true);
        assert_eq!(body["store"], false);
        // The Codex backend takes no cache options; the prefix key alone pins the cache lane.
        assert!(body.get("prompt_cache_options").is_none());
        assert_eq!(body["prompt_cache_key"], "greencode");
        // Append-only pins the key to the shared prefix, so a growing transcript keeps the same cache lane.
        let a = client(CacheRetention::Auto, true).body(&req(&messages, "sys"));
        let b = client(CacheRetention::Auto, true).body(&req(&[crate::Message::user("different")], "sys"));
        assert_eq!(a["prompt_cache_key"], b["prompt_cache_key"]);
        // `none` sends no cache markers at all.
        let none = client(CacheRetention::None, false).body(&req(&messages, "sys"));
        assert!(none.get("prompt_cache_options").is_none());
        assert!(none.get("prompt_cache_key").is_none(), "no cache marker at all: {none}");
        assert!(AppendOnly::Auto.enabled("codex", "gpt-6-sol"));
    }
}
