//! Devin sign-in: the native app.devin.ai code flow (`devin`), the Windsurf
//! enterprise flow that mints a seat key (`devin-windsurf`), and adopting the
//! official CLI's stored key (`devin-cli`). No refresh: seats and session JWTs
//! are long-lived, so `refreshes` stays false.

use super::{browser, pkce, Callback, Cred, Start};
use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Map};
use std::path::PathBuf;

const AUTHORIZE: &str = "https://app.devin.ai/auth/cli/continue";
const TOKEN: &str = "https://api.devin.ai/auth/cli/token";
const WINDSURF_AUTHORIZE: &str = "https://windsurf.com/editor/signin";
/// Both loopback ports are the ones the official client registers.
const CALLBACK_PORT: u16 = 59653;
const WINDSURF_CALLBACK_PORT: u16 = 59654;
/// `credential.expires` fallback: a year, for tokens without an `exp` claim.
const FALLBACK_MS: u64 = 31_536_000_000;

async fn callback(port: u16) -> Result<Callback> {
    match Callback::bind(port, "127.0.0.1", "/callback").await {
        Ok(callback) => Ok(callback),
        Err(_) => Callback::bind(0, "127.0.0.1", "/callback").await,
    }
}

pub async fn login() -> Result<Start> {
    let callback = callback(CALLBACK_PORT).await?;
    let (verifier, challenge) = pkce()?;
    let state = crate::provider::devin::uuid_v4();
    let url = format!(
        "{AUTHORIZE}?{}",
        url::form_urlencoded::Serializer::new(String::new())
            .extend_pairs([
                ("response_type", "code"),
                ("redirect_uri", callback.redirect_uri.as_str()),
                ("state", state.as_str()),
                ("code_challenge", challenge.as_str()),
                ("code_challenge_method", "S256"),
                ("prompt", "select_account"),
            ])
            .finish()
    );
    Ok(browser(url, callback, move |redirect, _| async move {
        let code = redirect.code()?.to_string();
        let body = json!({"code": code, "code_verifier": verifier});
        let v = super::post_json(TOKEN, &body, &[("Accept", "application/json")]).await?;
        let token = v["token"].as_str().filter(|t| !t.is_empty()).ok_or_else(|| anyhow!("Devin のトークン応答に token がありません"))?;
        let mut oauth = super::OAuth {
            access: token.into(),
            refresh: Some(token.into()),
            expires: Some(super::jwt_expiry(token).unwrap_or_else(|| super::now_ms() + FALLBACK_MS)),
            extra: Map::from_iter([
                ("apiEndpoint".into(), json!("https://api.devin.ai")),
                ("enterpriseUrl".into(), json!("https://app.devin.ai")),
            ]),
        };
        // Identity labels the seat; a failure here must not fail the sign-in.
        if let Some(identity) = crate::provider::devin::identity(token).await {
            oauth.extra.extend(identity);
        }
        Ok(Cred::OAuth(oauth))
    }))
}

pub async fn login_windsurf() -> Result<Start> {
    let callback = callback(WINDSURF_CALLBACK_PORT).await?;
    let (verifier, challenge) = pkce()?;
    let state = crate::provider::devin::uuid_v4();
    let url = format!(
        "{WINDSURF_AUTHORIZE}?{}",
        url::form_urlencoded::Serializer::new(String::new())
            .extend_pairs([
                ("response_type", "code"),
                ("redirect_uri", callback.redirect_uri.as_str()),
                ("state", state.as_str()),
                ("code_challenge", challenge.as_str()),
                ("code_challenge_method", "S256"),
                ("prompt", "login"),
                ("redirect_parameters_type", "query"),
            ])
            .finish()
    );
    Ok(browser(url, callback, move |redirect, _| async move {
        let code = redirect.code()?.to_string();
        let key = crate::provider::devin::windsurf_exchange(&code, &verifier).await?;
        Ok(Cred::Key(key))
    }))
}

/// Adopts the `windsurf_api_key` the official Devin CLI persists; local only.
pub async fn login_cli() -> Result<Start> {
    let key = read_cli_key(&cli_paths()).await?;
    Ok(Start { url: "https://app.devin.ai".into(), code: None, paste: None, done: Box::pin(async move { Ok(Cred::Key(key)) }) })
}

/// Credential files in probe order: Windows APPDATA, XDG, `~/.local/share`, macOS Library.
fn cli_paths() -> Vec<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let win = std::env::consts::OS == "windows";
    let mut dirs: Vec<PathBuf> = Vec::new();
    if win {
        dirs.extend(std::env::var_os("APPDATA").map(PathBuf::from));
    }
    dirs.extend(std::env::var_os("XDG_DATA_HOME").map(PathBuf::from));
    if let Some(home) = &home {
        dirs.push(home.join(".local").join("share"));
        if !win {
            dirs.push(home.join("Library").join("Application Support"));
        }
    }
    let mut paths: Vec<PathBuf> = dirs.into_iter().map(|dir| dir.join("devin").join("credentials.toml")).collect();
    paths.dedup();
    paths
}

/// The seat key from the first existing candidate; missing files are skipped.
async fn read_cli_key(candidates: &[PathBuf]) -> Result<String> {
    for file in candidates {
        let text = match std::fs::read_to_string(file) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => bail!("Devin CLI の認証情報（{}）を読めません: {e}", file.display()),
        };
        return parse_cli_credentials(&text, file);
    }
    let probed: Vec<String> = candidates.iter().map(|p| p.display().to_string()).collect();
    bail!(
        "Devin CLI の認証情報が見つかりません（確認した場所: {}）。`devin auth login` を実行してから再試行してください",
        probed.join(", ")
    )
}

fn parse_cli_credentials(text: &str, file: &std::path::Path) -> Result<String> {
    let parsed: toml::Value = toml::from_str(text)
        .with_context(|| format!("Devin CLI の認証情報（{}）が正しい TOML ではありません", file.display()))?;
    let key = parsed.get("windsurf_api_key").and_then(|v| v.as_str()).unwrap_or("").trim();
    if key.is_empty() {
        bail!(
            "Devin CLI の認証情報（{}）に windsurf_api_key がありません。`devin auth login` を実行してから再試行してください",
            file.display()
        );
    }
    if let Some(server) = parsed.get("api_server_url").and_then(|v| v.as_str()).filter(|s| !s.is_empty()) {
        let host = url::Url::parse(server).ok().and_then(|u| u.host_str().map(str::to_string)).unwrap_or_default();
        let known = ["server.codeium.com", "server.enterprise.windsurf.com"];
        if !known.contains(&host.as_str()) {
            bail!(
                "Devin CLI の認証情報（{}）は未対応の API サーバー「{server}」を指しています（対応: {}）",
                file.display(),
                known.join(", ")
            );
        }
    }
    Ok(key.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    /// Serves `answers` in order and records request heads and bodies.
    async fn serve(answers: Vec<(u16, Vec<u8>)>) -> (String, Arc<std::sync::Mutex<Vec<(String, Vec<u8>)>>>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let log = seen.clone();
        tokio::spawn(async move {
            for (status, reply) in answers {
                let (mut s, _) = listener.accept().await.unwrap();
                let mut req = Vec::new();
                let mut buf = [0u8; 4096];
                loop {
                    let n = s.read(&mut buf).await.unwrap();
                    if n == 0 {
                        break;
                    }
                    req.extend_from_slice(&buf[..n]);
                    if let Some(split) = req.windows(4).position(|w| w == b"\r\n\r\n") {
                        let head = String::from_utf8_lossy(&req[..split]).to_string();
                        let len = head
                            .lines()
                            .find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length:").and_then(|v| v.trim().parse::<usize>().ok()))
                            .unwrap_or(0);
                        if req.len() >= split + 4 + len {
                            log.lock().unwrap().push((head, req[split + 4..split + 4 + len].to_vec()));
                            break;
                        }
                    }
                }
                let body = if reply.is_empty() { b"{}".to_vec() } else { reply };
                let _ = s
                    .write_all(
                        format!(
                            "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                            body.len()
                        )
                        .as_bytes(),
                    )
                    .await;
                let _ = s.write_all(&body).await;
            }
        });
        (format!("http://{addr}"), seen)
    }

    #[test]
    fn cli_credentials_require_a_key_and_a_known_host() {
        let file = std::path::Path::new("/tmp/devin.toml");
        assert_eq!(parse_cli_credentials("windsurf_api_key = \"sk-ws-1\"\n", file).unwrap(), "sk-ws-1");
        assert!(parse_cli_credentials("windsurf_api_key = \"k\"\napi_server_url = \"https://server.codeium.com\"\n", file).is_ok());
        assert!(parse_cli_credentials("windsurf_api_key = \"\"\n", file).is_err());
        assert!(parse_cli_credentials("other = 1\n", file).is_err());
        assert!(parse_cli_credentials("windsurf_api_key = \"k\"\napi_server_url = \"https://evil.example\"\n", file).is_err());
        assert!(parse_cli_credentials("not toml [", file).is_err());
    }

    #[tokio::test]
    async fn cli_login_reads_the_first_existing_file() {
        let dir = std::env::temp_dir().join(format!("greencode-devin-{}", std::process::id()));
        let missing = dir.join("nope").join("credentials.toml");
        let file = dir.join("devin").join("credentials.toml");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, "windsurf_api_key = \"sk-ws-cli\"\n").unwrap();
        assert_eq!(read_cli_key(&[missing, file]).await.unwrap(), "sk-ws-cli");
        assert!(read_cli_key(&[dir.join("absent.toml")]).await.is_err());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn token_expiry_uses_the_jwt_claim() {
        use base64::Engine as _;
        let payload = super::super::URL_SAFE_NO_PAD.encode(r#"{"exp":2000000000}"#);
        assert_eq!(super::super::jwt_expiry(&format!("a.{payload}.s")), Some(2_000_000_000_000));
        assert_eq!(super::super::jwt_expiry("plain-key"), None);
    }

    #[tokio::test]
    async fn windsurf_exchange_decodes_the_prefixed_seat_key() {
        // Field 1 = "gsk-ws-abc" (prefixed wire form); RPCs want the leading `g` dropped.
        let mut reply = Vec::new();
        reply.extend_from_slice(&[0x0a, 0x0a]);
        reply.extend_from_slice(b"gsk-ws-abc");
        let (origin, seen) = serve(vec![(200, reply)]).await;
        let key = crate::provider::devin::windsurf_exchange_at(&origin, "code-1", "verifier-1").await.unwrap();
        assert_eq!(key, "sk-ws-abc");
        let seen = seen.lock().unwrap();
        let head = &seen[0].0;
        assert!(head.contains("/exa.seat_management_pb.SeatManagementService/ExchangePKCEAuthorizationCode"), "{head}");
        let lower = head.to_ascii_lowercase();
        assert!(lower.contains("content-type: application/proto"), "{head}");
        assert!(lower.contains("connect-protocol-version: 1"), "{head}");
        // Request: field 1 code, field 2 verifier.
        let body = &seen[0].1;
        assert_eq!(&body[..2], &[0x0a, 6]);
        assert_eq!(&body[2..8], b"code-1");
        assert_eq!(&body[8..10], &[0x12, 10]);
        assert_eq!(&body[10..], b"verifier-1");
    }
}
