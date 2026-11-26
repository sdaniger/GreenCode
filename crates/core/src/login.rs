//! Browser and device-code sign-in for providers that offer one (OMP's subscription logins).
//!
//! A provider module exposes `pub async fn login() -> Result<Start>` and, when its tokens expire,
//! `pub async fn refresh(cred: OAuth) -> Result<OAuth>`; both are wired into `start` and `refresh` below.
//! The GUI and `greencode-cli auth login` drive every flow the same way: show `url` (and `code`),
//! feed a pasted redirect into `paste` when the browser cannot return here, await `done`, then `auth::save`.

use crate::BoxFut;
use anyhow::{anyhow, bail, Context, Result};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use std::{future::Future, time::Duration};
use tokio::sync::mpsc;

pub mod kimi;
pub mod openrouter;
pub mod xai;
pub mod zai;
pub mod muse;
pub mod factory;
pub mod devin;
pub mod kiro;
pub mod snowflake;
pub mod gitlab;
pub mod cursor;
#[cfg(test)]
mod mock;

/// A signed-in OAuth credential as kept in auth.json.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct OAuth {
    pub access: String,
    pub refresh: Option<String>,
    /// Unix milliseconds; `None` never expires.
    pub expires: Option<u64>,
    /// Provider-specific fields (account id, org id, region, minted key…), stored next to the token.
    pub extra: Map<String, Value>,
}

impl OAuth {
    /// Expired or within a minute of expiring.
    pub fn due(&self) -> bool {
        self.expires.is_some_and(|at| at <= now_ms() + 60_000)
    }
}

/// What a finished sign-in produced.
#[derive(Debug)]
pub enum Cred {
    /// A durable API key (OpenRouter, Z.AI and Muse mint one).
    Key(String),
    OAuth(OAuth),
}

/// A sign-in in progress.
pub struct Start {
    /// Page the user opens: a device verification page or an authorize URL.
    pub url: String,
    /// Device code the user types on `url`; `None` for browser redirects.
    pub code: Option<String>,
    /// Accepts the pasted redirect URL (or bare code) when the browser cannot return here.
    pub paste: Option<mpsc::UnboundedSender<String>>,
    /// Resolves once the user approved; the caller stores the result with `auth::save`.
    pub done: BoxFut<'static, Result<Cred>>,
}

/// Providers with a browser or device sign-in.
pub const PROVIDERS: &[&str] = &[
    "kimi-code", "openrouter", "xai-oauth", "zai-coding-plan", "muse-code", "factory-droid",
    "devin", "devin-cli", "devin-windsurf", "kiro", "snowflake",
    "gitlab-duo", "gitlab-duo-agent", "cursor",
];

pub fn supports(name: &str) -> bool {
    PROVIDERS.contains(&name)
}

pub async fn start(name: &str) -> Result<Start> {
    let start = start_flow(name).await?;
    // The URL is opened automatically, so a server must not be able to hand us `file:` or an app scheme.
    if !openable(&start.url) {
        bail!("{name} のログイン URL が https ではありません: {}", start.url);
    }
    Ok(start)
}

/// `https`, or `http` to a loopback host (local test servers).
fn openable(url: &str) -> bool {
    let Ok(url) = url::Url::parse(url) else { return false };
    match url.scheme() {
        "https" => true,
        "http" => matches!(url.host(), Some(url::Host::Ipv4(ip)) if ip.is_loopback())
            || matches!(url.host(), Some(url::Host::Ipv6(ip)) if ip.is_loopback())
            || url.host_str() == Some("localhost"),
        _ => false,
    }
}

async fn start_flow(name: &str) -> Result<Start> {
    match name {
        "kimi-code" => kimi::login().await,
        "openrouter" => openrouter::login().await,
        "xai-oauth" => xai::login().await,
        "zai-coding-plan" => zai::login().await,
        "muse-code" => muse::login().await,
        "factory-droid" => factory::login().await,
        "devin" => devin::login().await,
        "devin-cli" => devin::login_cli().await,
        "devin-windsurf" => devin::login_windsurf().await,
        "kiro" => kiro::login().await,
        "snowflake" => snowflake::login().await,
        "gitlab-duo" => gitlab::login().await,
        "gitlab-duo-agent" => gitlab::login_agent().await,
        "cursor" => cursor::login().await,
        _ => bail!("{name} にはブラウザでのログインがありません。API キーを設定してください"),
    }
}

/// Providers whose OAuth tokens expire and are refreshed before requests.
pub fn refreshes(name: &str) -> bool {
    matches!(name, "kimi-code" | "xai-oauth" | "factory-droid" | "kiro" | "snowflake" | "gitlab-duo" | "gitlab-duo-agent" | "cursor")
}

pub async fn refresh(name: &str, cred: OAuth) -> Result<OAuth> {
    match name {
        "kimi-code" => kimi::refresh(cred).await,
        "xai-oauth" => xai::refresh(cred).await,
        "factory-droid" => factory::refresh(cred).await,
        "kiro" => kiro::refresh(cred).await,
        "snowflake" => snowflake::refresh(cred).await,
        "gitlab-duo" => gitlab::refresh(cred).await,
        "gitlab-duo-agent" => gitlab::refresh_agent(cred).await,
        "cursor" => cursor::refresh(cred).await,
        _ => bail!("{name} のトークンは更新できません。/login {name} でもう一度ログインしてください"),
    }
}

/// The stored OAuth credential of `name`, refreshed and saved first when it is about to expire.
pub async fn fresh(name: &str) -> Result<OAuth> {
    // One refresh at a time: services that rotate refresh tokens reject the second use of the old one.
    static LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    let _guard = LOCK.lock().await;
    let cred = crate::auth::oauth(name).ok_or_else(|| anyhow!("{name} にサインインしていません。/login {name} を実行してください"))?;
    if !cred.due() { return Ok(cred) }
    let cred = refresh(name, cred).await?;
    crate::auth::update_oauth(name, &cred)?;
    Ok(cred)
}

pub fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

/// `expires_in` seconds from now, as unix milliseconds.
pub fn expires_in(seconds: Option<u64>) -> Option<u64> {
    seconds.map(|s| now_ms().saturating_add(s.saturating_mul(1000)))
}

/// The `exp` claim of a JWT access token, as unix milliseconds.
pub fn jwt_expiry(token: &str) -> Option<u64> {
    jwt_claims(token)?.get("exp")?.as_u64().map(|s| s.saturating_mul(1000))
}

/// Payload of a JWT, unverified.
pub fn jwt_claims(token: &str) -> Option<Map<String, Value>> {
    let payload = token.split('.').nth(1)?;
    serde_json::from_slice(&URL_SAFE_NO_PAD.decode(payload.trim_end_matches('=')).ok()?).ok()
}

/// 32 random bytes, base64url.
pub fn random() -> Result<String> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|e| anyhow!("random generation failed: {e}"))?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

/// PKCE verifier and its S256 challenge.
pub fn pkce() -> Result<(String, String)> {
    let verifier = random()?;
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    Ok((verifier, challenge))
}

/// An RFC 8628 device authorization in progress.
#[derive(Clone, Debug)]
pub struct Device {
    pub user_code: String,
    /// `verification_uri_complete` when the service sends it, else `verification_uri`.
    pub url: String,
    pub device_code: String,
    pub interval: u64,
    pub expires_in: u64,
}

/// Requests a device code: POSTs `form` to `url` (form-encoded) with `headers`.
pub async fn device(url: &str, form: &[(&str, &str)], headers: &[(&str, &str)]) -> Result<Device> {
    let v = post_form(url, form, headers).await?;
    let text = |k: &str| v[k].as_str().filter(|s| !s.is_empty()).map(str::to_string);
    let (Some(device_code), Some(user_code)) = (text("device_code"), text("user_code")) else {
        bail!("invalid device code response: {v}")
    };
    let url = text("verification_uri_complete").or_else(|| text("verification_uri")).or_else(|| text("verification_url"))
        .ok_or_else(|| anyhow!("device code response has no verification URL"))?;
    Ok(Device {
        user_code,
        url,
        device_code,
        interval: v["interval"].as_u64().unwrap_or(5).max(1),
        expires_in: v["expires_in"].as_u64().unwrap_or(900),
    })
}

/// Polls `token_url` until the device is approved and returns the token response.
/// `form` holds the client fields; the grant type and device code are added here.
pub async fn poll(device: &Device, token_url: &str, form: &[(&str, &str)], headers: &[(&str, &str)]) -> Result<Value> {
    poll_every(device, token_url, form, headers, Duration::from_secs(1)).await
}

/// `poll` where one "second" of waiting lasts `unit`, so tests run fast.
pub async fn poll_every(device: &Device, token_url: &str, form: &[(&str, &str)], headers: &[(&str, &str)], unit: Duration) -> Result<Value> {
    let mut interval = device.interval as f64;
    let mut waited = 0.0;
    let mut fields = form.to_vec();
    fields.push(("grant_type", "urn:ietf:params:oauth:grant-type:device_code"));
    fields.push(("device_code", &device.device_code));
    while waited < device.expires_in as f64 {
        tokio::time::sleep(unit.mul_f64(interval)).await;
        waited += interval;
        let v = match post_form(token_url, &fields, headers).await {
            Ok(v) => v,
            // Pending answers often come back as HTTP 400 with an OAuth error body.
            Err(e) => match e.downcast_ref::<HttpError>() {
                Some(HttpError { body, .. }) => serde_json::from_str(body).map_err(|_| e)?,
                None => return Err(e),
            },
        };
        if v.get("access_token").and_then(Value::as_str).is_some_and(|t| !t.is_empty()) {
            return Ok(v);
        }
        match v["error"].as_str() {
            Some("authorization_pending") => {}
            // RFC 8628 §3.5: slow down by five seconds; never poll faster than before.
            Some("slow_down") => interval = v["interval"].as_u64().map_or(interval + 5.0, |i| (i as f64).max(interval)),
            Some("access_denied") => bail!("ログインが拒否されました"),
            Some("expired_token") => break,
            Some(e) => bail!("device sign-in failed: {e}: {}", v["error_description"].as_str().unwrap_or("")),
            None => bail!("invalid device token response: {v}"),
        }
    }
    bail!("ログインの有効期限が切れました。もう一度お試しください")
}

/// A non-success HTTP answer; `body` is the response text.
#[derive(Debug)]
pub struct HttpError {
    pub status: reqwest::StatusCode,
    pub body: String,
}

impl std::fmt::Display for HttpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "HTTP {}: {}", self.status, self.body.chars().take(500).collect::<String>())
    }
}

impl std::error::Error for HttpError {}

/// POSTs a form and returns the JSON answer; non-success statuses are `HttpError`.
pub async fn post_form(url: &str, form: &[(&str, &str)], headers: &[(&str, &str)]) -> Result<Value> {
    let body = url::form_urlencoded::Serializer::new(String::new()).extend_pairs(form).finish();
    let mut r = reqwest::Client::new().post(url).timeout(Duration::from_secs(30))
        .header("Accept", "application/json")
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(body);
    for (k, v) in headers { r = r.header(*k, *v); }
    json_answer(r.send().await.with_context(|| format!("POST {url}"))?).await
}

/// POSTs JSON and returns the JSON answer; non-success statuses are `HttpError`.
pub async fn post_json(url: &str, body: &Value, headers: &[(&str, &str)]) -> Result<Value> {
    let mut r = reqwest::Client::new().post(url).timeout(Duration::from_secs(30)).header("Accept", "application/json").json(body);
    for (k, v) in headers { r = r.header(*k, *v); }
    json_answer(r.send().await.with_context(|| format!("POST {url}"))?).await
}

async fn json_answer(resp: reqwest::Response) -> Result<Value> {
    let status = resp.status();
    let body = resp.text().await?;
    if !status.is_success() { return Err(HttpError { status, body }.into()) }
    serde_json::from_str(&body).with_context(|| format!("invalid JSON: {}", body.chars().take(200).collect::<String>()))
}

/// The query of the browser's final redirect: `code`, `state`, or the provider's own fields.
#[derive(Debug, Default)]
pub struct Redirect {
    pub query: std::collections::BTreeMap<String, String>,
    /// Typed or pasted by the user rather than delivered to the loopback listener,
    /// which any local process or web page can reach.
    pub pasted: bool,
}

impl Redirect {
    pub fn get(&self, key: &str) -> Option<&str> {
        self.query.get(key).map(String::as_str)
    }

    /// Fails unless the redirect carries the `state` sent with the authorize URL. A pasted bare
    /// code has no state to compare, but a loopback hit must always match (CSRF).
    pub fn check_state(&self, expected: &str) -> Result<()> {
        match self.get("state") {
            Some(got) if got == expected => Ok(()),
            None if self.pasted => Ok(()),
            _ => bail!("ログインの状態が一致しません。もう一度お試しください"),
        }
    }

    /// `code`, failing on an OAuth `error` answer.
    pub fn code(&self) -> Result<&str> {
        if let Some(e) = self.get("error") {
            bail!("ログインに失敗しました: {e} {}", self.get("error_description").unwrap_or(""));
        }
        self.get("code").filter(|c| !c.is_empty()).ok_or_else(|| anyhow!("リダイレクトに認可コードがありません"))
    }

    /// Parses a pasted redirect URL, a bare query, or a bare code (`code#state` included).
    pub fn parse(text: &str) -> Redirect {
        let text = text.trim();
        let query = match url::Url::parse(text) {
            Ok(url) => url.query().or(url.fragment()).unwrap_or("").to_string(),
            Err(_) => {
                // A bare query names an OAuth field; anything else (`abc==` included) is the code itself.
                let query = parse_query(text.trim_start_matches('?'));
                if ["code", "error", "state"].iter().any(|k| query.contains_key(*k)) {
                    return Redirect { query, pasted: true };
                }
                let (code, state) = text.split_once('#').unwrap_or((text, ""));
                let mut query = std::collections::BTreeMap::from([("code".to_string(), code.to_string())]);
                if !state.is_empty() { query.insert("state".into(), state.into()); }
                return Redirect { query, pasted: true };
            }
        };
        Redirect { query: parse_query(&query), pasted: true }
    }
}

fn parse_query(query: &str) -> std::collections::BTreeMap<String, String> {
    url::form_urlencoded::parse(query.as_bytes()).into_owned().collect()
}

/// A loopback listener for an OAuth redirect.
pub struct Callback {
    listener: Option<tokio::net::TcpListener>,
    /// `http://{host}:{port}{path}`, sent as `redirect_uri`.
    pub redirect_uri: String,
}

impl Callback {
    /// Listens on `127.0.0.1:port` (`0` picks a free port). `host` names it in `redirect_uri`
    /// (`localhost` or `127.0.0.1`, whatever the client registered).
    pub async fn bind(port: u16, host: &str, path: &str) -> Result<Callback> {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", port)).await
            .with_context(|| format!("ポート {port} を使えません。ほかのアプリが使用中でないか確認してください"))?;
        let port = listener.local_addr()?.port();
        Ok(Callback { listener: Some(listener), redirect_uri: format!("http://{host}:{port}{path}") })
    }

    /// No listener: the redirect goes to an app scheme (`zcode://`, `vscode://`) the user copies back.
    pub fn manual(redirect_uri: &str) -> Callback {
        Callback { listener: None, redirect_uri: redirect_uri.into() }
    }

    /// The first redirect that reaches the listener or is pasted, whichever comes first.
    pub async fn wait(self, mut paste: mpsc::UnboundedReceiver<String>) -> Result<Redirect> {
        let served = async {
            match self.listener {
                Some(listener) => serve_once(listener).await,
                None => std::future::pending().await,
            }
        };
        tokio::select! {
            got = served => got,
            Some(text) = paste.recv() => Ok(Redirect::parse(&text)),
            // Ten minutes, like OMP: an abandoned sign-in must not hold the port forever.
            _ = tokio::time::sleep(Duration::from_secs(600)) => bail!("ログインの有効期限が切れました。もう一度お試しください"),
        }
    }
}

/// Answers requests until one carries `code` or `error`, then returns it. Other requests
/// (favicon, preconnects) get a 404 and leave the listener waiting.
async fn serve_once(listener: tokio::net::TcpListener) -> Result<Redirect> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    loop {
        let (mut stream, _) = listener.accept().await?;
        let mut buf = vec![0u8; 8192];
        // An idle connection must not block the real redirect behind it.
        let n = match tokio::time::timeout(Duration::from_secs(5), stream.read(&mut buf)).await {
            Ok(Ok(n)) => n,
            _ => continue,
        };
        let head = String::from_utf8_lossy(&buf[..n]);
        let target = head.lines().next().and_then(|l| l.split_whitespace().nth(1)).unwrap_or("/");
        let redirect = target.split_once('?')
            .map(|(_, q)| Redirect { query: parse_query(q), pasted: false })
            .filter(|r| r.get("code").is_some() || r.get("error").is_some());
        let Some(redirect) = redirect else {
            let _ = stream.write_all(b"HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\nconnection: close\r\n\r\n").await;
            continue;
        };
        let page = if redirect.get("error").is_none() { "ログインしました。このタブを閉じて GreenCode に戻ってください。" } else { "ログインできませんでした。GreenCode に戻ってください。" };
        let html = format!("<!doctype html><meta charset=utf-8><title>GreenCode</title><p style=\"font:16px system-ui;margin:3em\">{page}</p>");
        let _ = stream.write_all(format!("HTTP/1.1 200 OK\r\ncontent-type: text/html; charset=utf-8\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{html}", html.len()).as_bytes()).await;
        return Ok(redirect);
    }
}

/// A browser sign-in: `url` is opened, the redirect arrives at `callback` or is pasted, and `finish` turns it into a credential.
pub fn browser<F>(url: String, callback: Callback, finish: impl FnOnce(Redirect, String) -> F + Send + 'static) -> Start
where
    F: Future<Output = Result<Cred>> + Send + 'static,
{
    let (tx, rx) = mpsc::unbounded_channel();
    let redirect_uri = callback.redirect_uri.clone();
    Start {
        url,
        code: None,
        paste: Some(tx),
        done: Box::pin(async move { finish(callback.wait(rx).await?, redirect_uri).await }),
    }
}

/// A device sign-in: shows `device.user_code` on `device.url`, then `finish` polls and builds the credential.
pub fn device_start<F>(device: Device, finish: impl FnOnce(Device) -> F + Send + 'static) -> Start
where
    F: Future<Output = Result<Cred>> + Send + 'static,
{
    Start { url: device.url.clone(), code: Some(device.user_code.clone()), paste: None, done: Box::pin(finish(device)) }
}

/// The standard token answer (`access_token`, `refresh_token`, `expires_in`) as a credential.
pub fn standard(v: &Value) -> Result<OAuth> {
    let access = v["access_token"].as_str().filter(|s| !s.is_empty()).ok_or_else(|| anyhow!("token response has no access_token"))?;
    Ok(OAuth {
        access: access.into(),
        refresh: v["refresh_token"].as_str().filter(|s| !s.is_empty()).map(str::to_string),
        expires: expires_in(v["expires_in"].as_u64()).or_else(|| jwt_expiry(access)),
        extra: Map::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pasted_redirects_parse_urls_queries_and_bare_codes() {
        let r = Redirect::parse("zcode://zai-auth/callback?code=abc&state=xyz");
        assert_eq!((r.get("code"), r.get("state")), (Some("abc"), Some("xyz")));
        assert_eq!(Redirect::parse("code=c1&state=s").code().unwrap(), "c1");
        let r = Redirect::parse(" c2#s2 ");
        assert_eq!((r.code().unwrap(), r.get("state")), ("c2", Some("s2")));
        assert!(Redirect::parse("http://127.0.0.1/cb?error=access_denied").code().is_err());
        // Base64 padding is part of a bare code, not a query.
        assert_eq!(Redirect::parse("abc123==").code().unwrap(), "abc123==");
    }

    #[test]
    fn state_must_match_unless_a_bare_code_was_pasted() {
        assert!(Redirect::parse("http://x/cb?code=c&state=s").check_state("s").is_ok());
        assert!(Redirect::parse("http://x/cb?code=c&state=evil").check_state("s").is_err());
        assert!(Redirect::parse("c").check_state("s").is_ok());
        let hit = Redirect { query: parse_query("code=c"), pasted: false };
        assert!(hit.check_state("s").is_err());
    }

    #[test]
    fn only_https_or_loopback_urls_are_opened() {
        assert!(openable("https://example.com/device"));
        assert!(openable("http://127.0.0.1:1234/auth"));
        assert!(openable("http://localhost:1234/auth"));
        assert!(!openable("http://example.com/device"));
        assert!(!openable("file:///Applications/Calculator.app"));
        assert!(!openable("-a Calculator"));
    }

    #[tokio::test]
    async fn callback_returns_the_redirect_or_a_paste() {
        let cb = Callback::bind(0, "127.0.0.1", "/callback").await.unwrap();
        let uri = cb.redirect_uri.clone();
        let (_tx, rx) = mpsc::unbounded_channel();
        let wait = tokio::spawn(cb.wait(rx));
        // A stray request without a code is ignored rather than ending the sign-in.
        assert_eq!(reqwest::get(format!("{uri}?v=1")).await.unwrap().status(), 404);
        let page = reqwest::get(format!("{uri}?code=xyz&state=s")).await.unwrap().text().await.unwrap();
        assert!(page.contains("ログインしました"));
        assert_eq!(wait.await.unwrap().unwrap().code().unwrap(), "xyz");

        let (tx, rx) = mpsc::unbounded_channel();
        tx.send("vscode://x/auth?code=pasted".into()).unwrap();
        assert_eq!(Callback::manual("vscode://x/auth").wait(rx).await.unwrap().code().unwrap(), "pasted");
    }

    #[tokio::test]
    async fn device_poll_waits_through_pending_and_slow_down() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/token", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let answers = [
                (400, r#"{"error":"authorization_pending"}"#),
                (200, r#"{"error":"slow_down","interval":1}"#),
                (200, r#"{"access_token":"tok","refresh_token":"r","expires_in":3600}"#),
            ];
            let mut bodies = vec![];
            for (status, body) in answers {
                let (mut s, _) = listener.accept().await.unwrap();
                // Headers and body may arrive in separate reads.
                let mut req = Vec::new();
                let mut buf = vec![0u8; 4096];
                while !String::from_utf8_lossy(&req).contains("device_code=dc") {
                    let n = s.read(&mut buf).await.unwrap();
                    assert!(n > 0, "request ended early");
                    req.extend_from_slice(&buf[..n]);
                }
                bodies.push(String::from_utf8_lossy(&req).to_string());
                s.write_all(format!("HTTP/1.1 {status} X\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
            }
            bodies
        });
        let device = Device { user_code: "U".into(), url: "u".into(), device_code: "dc".into(), interval: 1, expires_in: 60 };
        let v = poll_every(&device, &url, &[("client_id", "cid")], &[], Duration::from_millis(1)).await.unwrap();
        let cred = standard(&v).unwrap();
        assert_eq!((cred.access.as_str(), cred.refresh.as_deref()), ("tok", Some("r")));
        assert!(cred.expires.unwrap() > now_ms() + 3_000_000);
        let bodies = server.await.unwrap();
        assert!(bodies[0].contains("device_code=dc") && bodies[0].contains("client_id=cid") && bodies[0].contains("grant-type%3Adevice_code"));
    }
}
