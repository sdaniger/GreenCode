use anyhow::{anyhow, bail, Result};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
const REDIRECT: &str = "https://console.anthropic.com/oauth/code/callback";
const TOKEN_USER_AGENT: &str = "axios/1.7.9";

/// Sent on every subscription request; without them the API rejects OAuth tokens.
pub const BETAS: &str = "claude-code-20250219,oauth-2025-04-20";
pub const SYSTEM_PREFIX: &str = "You are Claude Code, Anthropic's official CLI for Claude.";
/// Bare tool names are billed as a third-party app; `mcp__name` is accepted.
pub const TOOL_PREFIX: &str = "mcp__";
/// Used when no `claude` executable is installed.
const CLAUDE_CODE_VERSION: &str = "2.1.322";

pub fn path() -> std::path::PathBuf {
    crate::home().join("anthropic-oauth.json")
}

/// Adopts an existing Claude Code sign-in (`~/.claude/.credentials.json`) so a user already
/// logged in through that CLI does not repeat the browser flow. Returns the tokens when found.
pub fn import_claude_code() -> Result<Option<StoredTokens>> {
    import_claude_code_at(&crate::user_home().join(".claude/.credentials.json"))
}

/// `import_claude_code` against an explicit path.
pub fn import_claude_code_at(path: &std::path::Path) -> Result<Option<StoredTokens>> {
    if !path.is_file() { return Ok(None); }
    let value: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(path)?)?;
    let oauth = &value["claudeAiOauth"];
    let (Some(access), Some(refresh)) = (oauth["accessToken"].as_str(), oauth["refreshToken"].as_str()) else { return Ok(None) };
    // Claude Code stores milliseconds; this file stores seconds.
    let expires_at = oauth["expiresAt"].as_u64().map_or(0, |ms| ms / 1000);
    // The refresh token is not copied: refreshing rotates it and would sign Claude Code out.
    let _ = refresh;
    Ok(Some(StoredTokens { access_token: access.to_string(), refresh_token: String::new(), expires_at }))
}

/// Writes the imported credentials. A real login (one with a refresh token) is never replaced;
/// a previously borrowed token (empty refresh token) is, so re-importing renews it.
#[cfg(unix)]
pub fn import_into(path: &std::path::Path) -> Result<Option<StoredTokens>> {
    import_into_from(path, &crate::user_home().join(".claude/.credentials.json"))
}

#[cfg(unix)]
pub fn import_into_from(path: &std::path::Path, source: &std::path::Path) -> Result<Option<StoredTokens>> {
    let borrowed = match std::fs::read_to_string(path) {
        Ok(text) => serde_json::from_str::<StoredTokens>(&text).is_ok_and(|t| t.refresh_token.is_empty()),
        Err(_) => true,
    };
    if path.exists() && !borrowed { return Ok(None); }
    let Some(tokens) = import_claude_code_at(source)? else { return Ok(None) };
    tokens.save(path)?;
    Ok(Some(tokens))
}

/// A valid access token, refreshed when it expires within a minute.
pub async fn access_token(path: &std::path::Path) -> Result<String> {
    #[cfg(unix)]
    return Ok(StoredTokens::load_or_refresh(path).await?.access_token);
    #[cfg(not(unix))]
    { let _ = path; bail!("Anthropic OAuth credential storage requires Unix") }
}

/// `claude-cli/<version> (external, cli)`. The API rejects versions far behind the current release,
/// so the installed Claude Code's version is preferred over the pinned one. Detected once per process.
pub async fn user_agent() -> &'static str {
    static UA: tokio::sync::OnceCell<String> = tokio::sync::OnceCell::const_new();
    UA.get_or_init(|| async {
        let detected = async {
            let exe = crate::config::executable("claude")?;
            let out = tokio::process::Command::new(exe).arg("--version").stdin(std::process::Stdio::null()).output();
            let out = tokio::time::timeout(std::time::Duration::from_secs(5), out).await.ok()?.ok()?;
            let version = String::from_utf8_lossy(&out.stdout).split_whitespace().next()?.to_string();
            version.starts_with(|c: char| c.is_ascii_digit()).then_some(version)
        }.await;
        format!("claude-cli/{} (external, cli)", detected.as_deref().unwrap_or(CLAUDE_CODE_VERSION))
    }).await
}

pub struct Login {
    verifier: String,
    state: String,
}

#[derive(Deserialize)]
pub struct Tokens {
    pub access_token: String,
    pub refresh_token: String,
    pub expires_in: u64,
}

#[derive(Serialize, Deserialize)]
pub struct StoredTokens {
    pub access_token: String,
    pub refresh_token: String,
    pub expires_at: u64,
}

impl StoredTokens {
    #[cfg(unix)]
    pub async fn load_or_refresh(path: &std::path::Path) -> Result<Self> {
        Self::load_or_refresh_at(path, "https://platform.claude.com/v1/oauth/token").await
    }

    #[cfg(unix)]
    async fn load_or_refresh_at(path: &std::path::Path, endpoint: &str) -> Result<Self> {
        use std::os::unix::fs::OpenOptionsExt;
        let parent = path.parent().ok_or_else(|| anyhow!("credential path has no parent"))?;
        std::fs::create_dir_all(parent)?;
        let lock_path = path.with_extension("lock");
        let lock = std::fs::OpenOptions::new().write(true).create(true).truncate(false).mode(0o600).open(lock_path)?;
        loop {
            match lock.try_lock() {
                Ok(()) => break,
                Err(std::fs::TryLockError::WouldBlock) => tokio::time::sleep(std::time::Duration::from_millis(50)).await,
                Err(std::fs::TryLockError::Error(e)) => return Err(e.into()),
            }
        }
        let tokens: Self = serde_json::from_reader(std::fs::File::open(path)?)?;
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_secs();
        if tokens.expires_at > now.saturating_add(60) { return Ok(tokens); }
        anyhow::ensure!(!tokens.refresh_token.is_empty(), "the imported Claude Code token expired; run /login or `greencode-cli --anthropic-login` (the Claude Code login is left untouched)");
        let refreshed = tokens.refresh_at(endpoint).await?;
        refreshed.save(path)?;
        Ok(refreshed)
    }

    pub fn new(tokens: Tokens) -> Result<Self> {
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_secs();
        Ok(Self {
            access_token: tokens.access_token,
            refresh_token: tokens.refresh_token,
            expires_at: now.checked_add(tokens.expires_in).ok_or_else(|| anyhow!("token expiry overflow"))?,
        })
    }

    async fn refresh_at(&self, endpoint: &str) -> Result<Self> {
        let response = reqwest::Client::new().post(endpoint)
            .timeout(std::time::Duration::from_secs(15))
            .header(reqwest::header::USER_AGENT, TOKEN_USER_AGENT)
            .json(&serde_json::json!({"grant_type":"refresh_token", "client_id":CLIENT_ID,
                "refresh_token":self.refresh_token}))
            .send().await?.error_for_status()?;
        let tokens: Tokens = response.json().await?;
        if tokens.access_token.is_empty() || tokens.refresh_token.is_empty() || tokens.expires_in == 0 {
            bail!("token response is incomplete");
        }
        Self::new(tokens)
    }

    pub async fn refresh(&self) -> Result<Self> {
        self.refresh_at("https://platform.claude.com/v1/oauth/token").await
    }

    #[cfg(unix)]
    pub fn save(&self, path: &std::path::Path) -> Result<()> {
        self.save_inner(path, false)
    }

    #[cfg(unix)]
    pub fn save_new(&self, path: &std::path::Path) -> Result<()> {
        self.save_inner(path, true)
    }

    #[cfg(unix)]
    fn save_inner(&self, path: &std::path::Path, new: bool) -> Result<()> {
        use std::{io::Write, os::unix::fs::OpenOptionsExt};
        let parent = path.parent().ok_or_else(|| anyhow!("credential path has no parent"))?;
        std::fs::create_dir_all(parent)?;
        let tmp = parent.join(format!(".anthropic-oauth-{}", random()?));
        let result = (|| -> Result<()> {
            let mut file = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&tmp)?;
            serde_json::to_writer(&mut file, self)?;
            file.flush()?;
            file.sync_all()?;
            if new {
                std::fs::hard_link(&tmp, path)?;
                std::fs::remove_file(&tmp)?;
            } else {
                std::fs::rename(&tmp, path)?;
            }
            Ok(())
        })();
        if result.is_err() { let _ = std::fs::remove_file(&tmp); }
        result
    }
}

fn random() -> Result<String> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|e| anyhow!("random generation failed: {e}"))?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

impl Login {
    pub fn new() -> Result<Self> {
        Ok(Self { verifier: random()?, state: random()? })
    }

    pub fn url(&self) -> String {
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(self.verifier.as_bytes()));
        let mut url = url::Url::parse("https://claude.ai/oauth/authorize").unwrap();
        url.query_pairs_mut().extend_pairs([
            ("code", "true"), ("client_id", CLIENT_ID), ("response_type", "code"),
            ("redirect_uri", REDIRECT), ("scope", "org:create_api_key user:profile user:inference"),
            ("code_challenge", &challenge), ("code_challenge_method", "S256"), ("state", &self.state),
        ]);
        url.into()
    }

    pub async fn exchange(self, input: &str) -> Result<Tokens> {
        self.exchange_at(input, "https://platform.claude.com/v1/oauth/token").await
    }

    /// Exchanges the pasted code and stores the credentials, replacing any previous ones.
    pub async fn exchange_and_save(self, input: &str, path: &std::path::Path) -> Result<()> {
        let tokens = StoredTokens::new(self.exchange(input).await?)?;
        #[cfg(unix)]
        return tokens.save(path);
        #[cfg(not(unix))]
        { let _ = (tokens, path); bail!("Anthropic OAuth credential storage requires Unix") }
    }

    async fn exchange_at(self, input: &str, endpoint: &str) -> Result<Tokens> {
        let (code, state) = input.trim().split_once('#').ok_or_else(|| anyhow!("authorization code is missing state"))?;
        if code.is_empty() || state != self.state { bail!("authorization code or state is invalid"); }
        let tokens: Tokens = reqwest::Client::new().post(endpoint)
            .timeout(std::time::Duration::from_secs(15))
            .header(reqwest::header::USER_AGENT, TOKEN_USER_AGENT)
            .json(&serde_json::json!({"grant_type":"authorization_code", "client_id":CLIENT_ID,
                "code":code, "state":state, "redirect_uri":REDIRECT, "code_verifier":self.verifier}))
            .send().await?.error_for_status()?.json().await?;
        if tokens.access_token.is_empty() || tokens.refresh_token.is_empty() || tokens.expires_in == 0 { bail!("token response is incomplete"); }
        Ok(tokens)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn exchange_sends_pkce_and_rejects_expired_response() {
        use std::io::{Read, Write};
        let login = Login::new().unwrap();
        let input = format!("code#{}", login.state);
        let verifier = login.verifier.clone();
        let server = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!("http://{}", server.local_addr().unwrap());
        let thread = std::thread::spawn(move || {
            let (mut stream, _) = server.accept().unwrap();
            let mut request = Vec::new();
            let mut buf = [0; 1024];
            loop {
                let n = stream.read(&mut buf).unwrap();
                assert!(n > 0);
                request.extend_from_slice(&buf[..n]);
                if let Some(end) = request.windows(4).position(|w| w == b"\r\n\r\n") {
                    let headers = String::from_utf8_lossy(&request[..end]).to_lowercase();
                    assert!(headers.lines().any(|l| l == "user-agent: axios/1.7.9"));
                    let length: usize = headers.lines().find_map(|l| l.strip_prefix("content-length: ")).unwrap().parse().unwrap();
                    if request.len() >= end + 4 + length {
                        let body: serde_json::Value = serde_json::from_slice(&request[end + 4..end + 4 + length]).unwrap();
                        assert_eq!(body["code"], "code");
                        assert_eq!(body["code_verifier"], verifier);
                        break;
                    }
                }
            }
            let body = r#"{"access_token":"access","refresh_token":"refresh","expires_in":0}"#;
            write!(stream, "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body).unwrap();
        });
        assert!(login.exchange_at(&input, &endpoint).await.is_err());
        thread.join().unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn concurrent_refresh_reuses_rotated_token() {
        use std::io::{Read, Write};
        let dir = std::env::temp_dir().join(format!("greencode-oauth-lock-{}", random().unwrap()));
        let path = dir.join("credentials.json");
        StoredTokens { access_token: "old".into(), refresh_token: "old-refresh".into(), expires_at: 0 }.save(&path).unwrap();
        let server = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!("http://{}", server.local_addr().unwrap());
        let thread = std::thread::spawn(move || {
            let (mut stream, _) = server.accept().unwrap();
            let mut request = [0; 4096];
            let n = stream.read(&mut request).unwrap();
            assert!(String::from_utf8_lossy(&request[..n]).to_lowercase().contains("user-agent: axios/1.7.9\r\n"));
            let body = r#"{"access_token":"new","refresh_token":"rotated","expires_in":3600}"#;
            write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body).unwrap();
        });
        let (a, b) = tokio::join!(StoredTokens::load_or_refresh_at(&path, &endpoint), StoredTokens::load_or_refresh_at(&path, &endpoint));
        assert_eq!(a.unwrap().refresh_token, "rotated");
        assert_eq!(b.unwrap().refresh_token, "rotated");
        thread.join().unwrap();
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn refresh_parses_rotated_token() {
        use std::io::{Read, Write};
        let server = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = server.local_addr().unwrap();
        let thread = std::thread::spawn(move || {
            let (mut stream, _) = server.accept().unwrap();
            let mut request = [0; 4096];
            let _ = stream.read(&mut request).unwrap();
            let body = r#"{"access_token":"new-access","refresh_token":"new-refresh","expires_in":3600}"#;
            write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body).unwrap();
        });
        let old = StoredTokens { access_token: "old-access".into(), refresh_token: "old-refresh".into(), expires_at: 0 };
        let new = old.refresh_at(&format!("http://{address}")).await.unwrap();
        assert_eq!(new.access_token, "new-access");
        assert_eq!(new.refresh_token, "new-refresh");
        thread.join().unwrap();
    }

    #[tokio::test]
    async fn pkce_url_and_state_validation() {
        let login = Login::new().unwrap();
        let url = url::Url::parse(&login.url()).unwrap();
        let pairs: std::collections::HashMap<_, _> = url.query_pairs().into_owned().collect();
        assert_eq!(pairs["code_challenge"], URL_SAFE_NO_PAD.encode(Sha256::digest(login.verifier.as_bytes())));
        assert_eq!(pairs["state"], login.state);
        assert!(login.exchange("code#wrong-state").await.is_err());
    }

    #[cfg(unix)]
    #[test]
    fn claude_code_credentials_are_imported_without_the_refresh_token() {
        let dir = std::env::temp_dir().join(format!("greencode-claude-import-{}-{}", std::process::id(), std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(".credentials.json");
        std::fs::write(&path, r#"{"claudeAiOauth":{"accessToken":"oat","refreshToken":"rotating","expiresAt":1794170039771}}"#).unwrap();
        let tokens = import_claude_code_at(&path).unwrap().unwrap();
        assert_eq!(tokens.access_token, "oat");
        assert_eq!(tokens.expires_at, 1794170039);
        assert!(tokens.refresh_token.is_empty(), "the rotating refresh token must not be copied");
        assert!(import_claude_code_at(&dir.join("missing.json")).unwrap().is_none());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[cfg(unix)]
    #[test]
    fn reimport_renews_a_borrowed_token_but_never_replaces_a_real_login() {
        let dir = std::env::temp_dir().join(format!("greencode-claude-reimport-{}-{}", std::process::id(), std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
        std::fs::create_dir_all(&dir).unwrap();
        let (source, target) = (dir.join("source.json"), dir.join("anthropic-oauth.json"));
        let write = |token: &str| std::fs::write(&source, format!(r#"{{"claudeAiOauth":{{"accessToken":"{token}","refreshToken":"rotating","expiresAt":1794170039771}}}}"#)).unwrap();
        write("first");
        assert_eq!(import_into_from(&target, &source).unwrap().unwrap().access_token, "first");
        write("second");
        assert_eq!(import_into_from(&target, &source).unwrap().unwrap().access_token, "second");
        assert_eq!(serde_json::from_slice::<StoredTokens>(&std::fs::read(&target).unwrap()).unwrap().access_token, "second");
        // A browser login carries its own refresh token and must survive.
        StoredTokens { access_token: "real".into(), refresh_token: "r".into(), expires_at: u64::MAX }.save(&target).unwrap();
        assert!(import_into_from(&target, &source).unwrap().is_none());
        assert_eq!(serde_json::from_slice::<StoredTokens>(&std::fs::read(&target).unwrap()).unwrap().access_token, "real");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn credentials_are_saved_privately_and_replaced() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("greencode-oauth-test-{}", random().unwrap()));
        let path = dir.join("credentials.json");
        let tokens = StoredTokens::new(Tokens { access_token: "access".into(), refresh_token: "refresh".into(), expires_in: 3600 }).unwrap();
        tokens.save(&path).unwrap();
        let saved: StoredTokens = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(saved.refresh_token, "refresh");
        assert!(saved.expires_at > std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs());
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        let other = StoredTokens { access_token: "other".into(), refresh_token: "other".into(), expires_at: 1 };
        assert!(other.save_new(&path).is_err());
        let saved: StoredTokens = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(saved.refresh_token, "refresh");
        tokens.save(&path).unwrap();
        std::fs::remove_dir_all(dir).unwrap();
    }
}
