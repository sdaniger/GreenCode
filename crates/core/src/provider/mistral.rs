//! Mistral Vibe browser sign-in, as in the Vibe CLI and OMP (`mistral-vibe.ts`):
//! POST /api/vibe/sign-in with a PKCE challenge, poll until approved, then exchange for an API key
//! billed against the plan's Vibe quota. The key is stored like any Mistral API key.

use anyhow::{anyhow, bail, Result};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::time::Duration;

const ORIGIN: &str = "https://console.mistral.ai";
/// The Vibe CLI's poll cadence and failure budget.
const POLL_SECS: u64 = 3;
const MAX_FAILURES: u32 = 3;
/// Console `error_code` when the chosen workspace may not mint API keys.
const WORKSPACE_DENIED: i64 = 9004;
// shortcut: local cap instead of parsing `expires_at` (no date crate); the console also answers 410/"expired".
const MAX_WAIT_SECS: u64 = 15 * 60;

#[derive(Debug)]
pub struct SignIn {
    pub url: String,
    process_id: String,
    poll_url: String,
    verifier: String,
}

pub async fn start() -> Result<SignIn> { start_at(ORIGIN).await }

pub async fn start_at(origin: &str) -> Result<SignIn> {
    let mut bytes = [0u8; 64];
    getrandom::fill(&mut bytes).map_err(|e| anyhow!("random generation failed: {e}"))?;
    let verifier = URL_SAFE_NO_PAD.encode(bytes);
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    let (status, v) = post(&format!("{origin}/api/vibe/sign-in"), json!({"code_challenge": challenge, "code_challenge_method": "S256"})).await?;
    if !status.is_success() { bail!("Mistral サインインを開始できません: HTTP {status}") }
    let text = |k: &str| v[k].as_str().filter(|s| !s.is_empty()).map(str::to_string);
    let (Some(process_id), Some(url), Some(poll_url)) = (text("process_id"), text("sign_in_url"), text("poll_url")) else { bail!("invalid sign-in response") };
    // The poll URL comes from the server; only ever send requests back to the console API.
    Ok(SignIn { url: under(origin, "", &url)?, poll_url: under(origin, "/api", &poll_url)?, process_id, verifier })
}

pub async fn finish(sign_in: SignIn) -> Result<String> { finish_at(ORIGIN, sign_in, Duration::from_secs(1)).await }

/// `unit` is the length of one "second" of waiting, so tests can run fast.
pub async fn finish_at(origin: &str, sign_in: SignIn, unit: Duration) -> Result<String> {
    let client = reqwest::Client::new();
    let (mut waited, mut failures) = (0, 0);
    let token = loop {
        if waited >= MAX_WAIT_SECS { bail!("Mistral サインインの期限が切れました。/login からやり直してください") }
        tokio::time::sleep(unit * POLL_SECS as u32).await;
        waited += POLL_SECS;
        let resp = client.get(&sign_in.poll_url).timeout(Duration::from_secs(30)).send().await;
        let status = resp.as_ref().map(|r| r.status().as_u16()).ok();
        match status {
            Some(410) => bail!("Mistral サインインの期限が切れました。/login からやり直してください"),
            Some(429) => { failures = 0; continue }
            _ => {}
        }
        let answer = match resp { Ok(r) if r.status().is_success() => r.json::<Value>().await.ok(), _ => None };
        let Some(v) = answer.filter(|v| v["status"].is_string()) else {
            failures += 1;
            if failures < MAX_FAILURES { continue }
            bail!("Mistral サインインの状態を取得できません{}", status.map(|s| format!(": HTTP {s}")).unwrap_or_default())
        };
        failures = 0;
        match v["status"].as_str().unwrap() {
            "pending" => {}
            "completed" => break v["exchange_token"].as_str().filter(|t| !t.is_empty()).ok_or_else(|| anyhow!("sign-in completed without an exchange token"))?.to_string(),
            "expired" => bail!("Mistral サインインの期限が切れました。/login からやり直してください"),
            "denied" => bail!("Mistral サインインが拒否されました"),
            "error" => bail!("{}", v["message"].as_str().unwrap_or("Mistral サインインに失敗しました")),
            other => bail!("unknown sign-in state \"{other}\""),
        }
    };
    let url = format!("{origin}/api/vibe/sign-in/{}/exchange", url::form_urlencoded::byte_serialize(sign_in.process_id.as_bytes()).collect::<String>());
    let (status, v) = post(&url, json!({"exchange_token": token, "code_verifier": sign_in.verifier})).await?;
    if status.as_u16() == 403 && v["error_code"].as_i64() == Some(WORKSPACE_DENIED) {
        bail!("この Mistral ワークスペースでは Vibe を使えません。/login でやり直してブラウザで別のワークスペースを選ぶか、管理者に確認してください")
    }
    if !status.is_success() { bail!("API キーへの交換に失敗しました: HTTP {status}") }
    v["api_key"].as_str().filter(|k| !k.is_empty()).map(str::to_string).ok_or_else(|| anyhow!("exchange response has no api_key"))
}

async fn post(url: &str, body: Value) -> Result<(reqwest::StatusCode, Value)> {
    let resp = reqwest::Client::new().post(url).json(&body).timeout(Duration::from_secs(30)).send().await?;
    let status = resp.status();
    Ok((status, resp.json().await.unwrap_or(Value::Null)))
}

/// `value` when it is on `origin` at or below `base` (segment-aware: `/api` rejects `/api-x`).
fn under(origin: &str, base: &str, value: &str) -> Result<String> {
    let (want, url) = (url::Url::parse(origin)?, url::Url::parse(value).map_err(|_| anyhow!("unexpected URL {value}"))?);
    let path = url.path();
    if url.origin() == want.origin() && (base.is_empty() || path == base || path.starts_with(&format!("{base}/"))) { Ok(url.into()) }
    else { bail!("unexpected URL {value}") }
}
