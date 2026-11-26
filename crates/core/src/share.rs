//! `/share`: snapshot a thread, redact secrets, encrypt it, and upload it for a viewer link.
//!
//! Link format: `<server>/<id>#<key>` where `<key>` is the base64url 32-byte AES-256-GCM key; the server
//! stores only the ciphertext (`GCS1` magic + 12-byte nonce + ciphertext), so it cannot read a share.
//!
//! Wire protocol (GreenCode's own):
//! - `POST /v1/blobs` (bearer token) raw ciphertext → `{"id"}`
//! - `GET /v1/blobs/<id>` raw ciphertext, public (the id is random and the key is never sent to the server)
//! - `GET /<id>` viewer page that decrypts in the browser with WebCrypto
//! - `GET /v1/gists/<id>` gist content when `share.store = gist` and this server proxies GitHub;
//!   the gist file holds the base64url ciphertext, and links are `<base>/gist:<gist-id>#<key>`
use crate::{
    config::{collab::ShareStore, Config},
    localserver::{self, Request, Response},
    redact::Redactor,
    session::Session,
};
use aes_gcm::{
    aead::{Aead, KeyInit},
    Aes256Gcm, Nonce,
};
use anyhow::{bail, Context, Result};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use serde_json::{json, Value};
use std::{path::Path, path::PathBuf, time::Duration};
use tokio::net::TcpListener;

const MAGIC: &[u8; 4] = b"GCS1";
const MAX_BLOB: usize = 32 * 1024 * 1024;

pub fn http_base(url: &str) -> Result<String> {
    crate::collab::http_base(url)
}

fn server_base(cfg: &Config) -> Result<String> {
    let base = http_base(cfg.collab_opts.share.server_url.as_deref().context("share.server_url is not set")?)?;
    if base.starts_with("http://") && !base.trim_start_matches("http://").starts_with("127.0.0.1")
        && !base.trim_start_matches("http://").starts_with("localhost")
        && !base.trim_start_matches("http://").starts_with("[::1]") {
        bail!("share.server_url must be https:// (http:// is only for localhost)");
    }
    Ok(base)
}

/// Plain JSON snapshot of one thread: meta, messages and the runtime's context size.
pub fn snapshot(id: &str) -> Result<Value> {
    let (meta, messages) = Session::load(id).with_context(|| format!("thread {id} not found"))?;
    let runtime = Session::runtime(id).unwrap_or(Value::Null);
    Ok(json!({"meta": meta, "messages": messages, "runtime": runtime, "shared_at": now()}))
}

fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// Encrypts `plain` with a fresh key; returns `(payload, base64url key)`.
pub fn encrypt(plain: &[u8]) -> Result<(Vec<u8>, String)> {
    let mut key = [0u8; 32];
    getrandom::fill(&mut key).map_err(|e| anyhow::anyhow!("random key: {e}"))?;
    let mut nonce = [0u8; 12];
    getrandom::fill(&mut nonce).map_err(|e| anyhow::anyhow!("random nonce: {e}"))?;
    let cipher = Aes256Gcm::new_from_slice(&key).map_err(|e| anyhow::anyhow!("{e}"))?;
    let mut payload = MAGIC.to_vec();
    payload.extend_from_slice(&nonce);
    payload.extend(cipher.encrypt(Nonce::from_slice(&nonce), plain).map_err(|e| anyhow::anyhow!("encrypt: {e}"))?);
    Ok((payload, URL_SAFE_NO_PAD.encode(key)))
}

pub fn decrypt(payload: &[u8], key: &str) -> Result<Vec<u8>> {
    let key = URL_SAFE_NO_PAD.decode(key.trim()).context("invalid share key")?;
    let cipher = Aes256Gcm::new_from_slice(&key).map_err(|_| anyhow::anyhow!("invalid share key length"))?;
    let nonce = payload.get(4..16).context("share blob is too short")?;
    cipher.decrypt(Nonce::from_slice(nonce), &payload[16..]).map_err(|_| anyhow::anyhow!("cannot decrypt: wrong key or corrupt blob"))
}

fn client() -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder().timeout(Duration::from_secs(120)).build()?)
}

/// Upload token: `$GREENCODE_SHARE_TOKEN`, else the stored `SHARE_TOKEN` secret.
fn token() -> Result<String> {
    std::env::var("GREENCODE_SHARE_TOKEN")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .or_else(|| crate::secrets::env().into_iter().find(|(name, _)| name == "SHARE_TOKEN").map(|(_, v)| v))
        .context("set GREENCODE_SHARE_TOKEN (or save SHARE_TOKEN with the secrets tool) to upload shares")
}

fn github_token() -> Result<String> {
    std::env::var("GITHUB_TOKEN")
        .or_else(|_| std::env::var("GH_TOKEN"))
        .ok()
        .filter(|v| !v.trim().is_empty())
        .context("share.store = gist needs GITHUB_TOKEN with the gist scope")
}

/// Publishes `thread` and returns the viewer link.
pub async fn publish(cfg: &Config, thread: &str) -> Result<String> {
    publish_value(cfg, snapshot(thread)?).await
}

/// The same for an in-memory snapshot (redacted here, so callers cannot forget).
pub async fn publish_value(cfg: &Config, mut value: Value) -> Result<String> {
    if cfg.collab_opts.share.redact_secrets {
        Redactor::from_config(cfg)?.json(&mut value);
    }
    let plain = serde_json::to_vec(&value)?;
    let (payload, key) = encrypt(&plain)?;
    let base = server_base(cfg)?;
    let body = match cfg.collab_opts.share.store {
        ShareStore::Blob => format!("{base}/{}", upload_blob(&base, &payload).await?),
        ShareStore::Gist => format!("{base}/gist:{}", upload_gist(&payload).await?),
    };
    Ok(format!("{body}#{key}"))
}

async fn upload_blob(base: &str, payload: &[u8]) -> Result<String> {
    let response = client()?
        .post(format!("{base}/v1/blobs"))
        .bearer_auth(token()?)
        .header("content-type", "application/octet-stream")
        .body(payload.to_vec())
        .send()
        .await
        .with_context(|| format!("share server {base} is unreachable"))?;
    ensure(response.status())?;
    let value: Value = response.json().await?;
    value["id"].as_str().map(str::to_owned).context("share server returned no id")
}

fn ensure(status: reqwest::StatusCode) -> Result<()> {
    if status.is_success() {
        return Ok(());
    }
    let text = match status.as_u16() {
        401 => " (share token rejected)".to_string(),
        _ => String::new(),
    };
    bail!("share server answered {status}{text}")
}

async fn upload_gist(payload: &[u8]) -> Result<String> {
    let body = json!({
        "description": "GreenCode share (encrypted; open the link to read)",
        "public": false,
        "files": { "greencode-share.b64": { "content": URL_SAFE_NO_PAD.encode(payload) } },
    });
    let response = client()?
        .post("https://api.github.com/gists")
        .bearer_auth(github_token()?)
        .header("user-agent", "greencode")
        .json(&body)
        .send()
        .await
        .context("api.github.com is unreachable")?;
    ensure(response.status())?;
    let value: Value = response.json().await?;
    value["id"].as_str().map(str::to_owned).context("GitHub returned no gist id")
}

/// Fetches and decrypts a link produced by `publish` (blob or gist).
pub async fn open(link: &str) -> Result<Value> {
    let (body, key) = link.split_once('#').context("a share link needs a #key fragment")?;
    let url = url::Url::parse(body).context("invalid share link")?;
    let id = url.path().trim_start_matches('/');
    if id.is_empty() {
        bail!("share link has no id");
    }
    let client = client()?;
    let origin = &url[..url::Position::BeforePath];
    let payload = match id.strip_prefix("gist:") {
        Some(gist) => base64_gist(client.get(format!("{origin}/v1/gists/{gist}")).send().await?).await?,
        None => {
            let response = client.get(format!("{origin}/v1/blobs/{id}")).send().await?;
            ensure(response.status())?;
            response.bytes().await?.to_vec()
        }
    };
    let plain = decrypt(&payload, key)?;
    serde_json::from_slice(&plain).context("share payload is not JSON")
}

/// Serves uploaded blobs and the decryption page until the listener closes.
pub async fn serve(listener: TcpListener, dir: PathBuf, token: String) -> Result<()> {
    std::fs::create_dir_all(&dir)?;
    let state = std::sync::Arc::new((dir, token));
    localserver::serve(listener, MAX_BLOB, move |request| {
        let state = state.clone();
        async move { handle(state, request).await }
    })
    .await
}

async fn handle(state: std::sync::Arc<(PathBuf, String)>, request: Request) -> Response {
    let (dir, token) = &*state;
    let (method, path) = (request.method.as_str(), request.path.as_str());
    if method == "POST" && path == "/v1/blobs" {
        if request.token() != Some(token.as_str()) {
            return Response::error("401 Unauthorized", "share token required");
        }
        let id = match crate::remote::random_hex() {
            Ok(id) => id[..16].to_string(),
            Err(_) => return Response::error("500 Internal Server Error", "entropy unavailable"),
        };
        let path = dir.join(&id);
        if let Err(e) = write_private(&path, &request.body) {
            return Response::error("500 Internal Server Error", &format!("cannot store blob: {e}"));
        }
        return Response::json(json!({"id": id}));
    }
    if method == "GET" && path.starts_with("/v1/blobs/") {
        let id = &path["/v1/blobs/".len()..];
        return match read_blob(dir, id) {
            Some(bytes) => Response::new("200 OK", "application/octet-stream", bytes),
            None => Response::error("404 Not Found", "no such share"),
        };
    }
    if method == "GET" && path.starts_with("/v1/gists/") {
        let id = &path["/v1/gists/".len()..];
        return match fetch_gist(id).await {
            Ok(text) => Response::new("200 OK", "application/json", serde_json::to_vec(&json!({"content": text})).unwrap_or_default()),
            Err(e) => Response::error("502 Bad Gateway", &format!("{e:#}")),
        };
    }
    if method == "GET" && path != "/" && !path.starts_with("/v1/") {
        let id = path.trim_start_matches('/');
        let exists = match id.strip_prefix("gist:") {
            Some(gist) => !gist.is_empty() && gist.bytes().all(|b| b.is_ascii_hexdigit()),
            None => read_blob(dir, id).is_some(),
        };
        return if exists { Response::html(VIEWER_HTML) } else { Response::error("404 Not Found", "no such share") };
    }
    Response::error("404 Not Found", "not found")
}

fn valid_id(id: &str) -> bool {
    id.len() == 16 && id.bytes().all(|b| b.is_ascii_hexdigit())
}

fn read_blob(dir: &Path, id: &str) -> Option<Vec<u8>> {
    if !valid_id(id) {
        return None;
    }
    std::fs::read(dir.join(id)).ok()
}

fn write_private(path: &Path, data: &[u8]) -> Result<()> {
    crate::remote::write_private(path, data)
}

async fn fetch_gist(id: &str) -> Result<String> {
    if !id.bytes().all(|b| b.is_ascii_hexdigit()) || id.is_empty() {
        bail!("invalid gist id");
    }
    let token = github_token()?;
    let value: Value = client()?.get(format!("https://api.github.com/gists/{id}")).bearer_auth(token).header("user-agent", "greencode")
        .send().await?.error_for_status()?.json().await?;
    value["files"].as_object().and_then(|files| files.values().next()).and_then(|file| file["content"].as_str())
        .map(str::to_owned).context("gist has no file content")
}

/// A `{"content": "<base64url ciphertext>"}` gist response, decoded.
async fn base64_gist(response: reqwest::Response) -> Result<Vec<u8>> {
    ensure(response.status())?;
    let value: Value = response.json().await?;
    let content = value["content"].as_str().context("gist response has no content")?;
    URL_SAFE_NO_PAD.decode(content.trim()).context("gist content is not base64url")
}

const VIEWER_HTML: &str = r#"<!doctype html>
<html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width, initial-scale=1">
<title>GreenCode share</title>
<style>body{font:15px/1.5 system-ui,sans-serif;margin:0;background:#101214;color:#e6e6e6}
header{padding:10px 16px;border-bottom:1px solid #333}main{padding:12px 16px}pre{white-space:pre-wrap;word-break:break-word;font:13px/1.5 ui-monospace,monospace;background:#16191c;padding:12px;border-radius:8px}
h1{font-size:16px;margin:0}.meta{color:#9aa0a6;font-size:13px}.turn{border-left:3px solid #333;padding-left:10px;margin:12px 0}
.user{border-color:#8ab4f8}.assistant{border-color:#6cc46c}.tool{color:#f0c674;font-size:13px;white-space:pre-wrap;background:#16191c;padding:8px;border-radius:6px}</style></head><body>
<header><h1 id="title">GreenCode share</h1><div class="meta" id="meta">decrypting…</div></header><main id="content"></main>
<script>
const id = location.pathname.split("/").pop();
const key = location.hash.slice(1);
const enc = new TextEncoder();
function b64url(s) { s = s.replace(/-/g, "+").replace(/_/g, "/"); while (s.length % 4) s += "="; return Uint8Array.from(atob(s), c => c.charCodeAt(0)); }
function el(tag, cls, text) { const e = document.createElement(tag); if (cls) e.className = cls; if (text !== undefined) e.textContent = text; return e; }
async function run() {
  const meta = document.getElementById("meta"), content = document.getElementById("content");
  if (!key) { meta.textContent = "This link is missing its #key fragment."; return; }
  let payload;
  if (id.startsWith("gist:")) {
    const r = await fetch(`/v1/gists/${id.slice(5)}`);
    if (!r.ok) { meta.textContent = "This server cannot fetch the gist (" + r.status + ")."; return; }
    const g = await r.json();
    payload = b64url(g.content.trim());
  } else {
    const r = await fetch(`/v1/blobs/${id}`);
    if (!r.ok) { meta.textContent = r.status === 404 ? "No such share." : "Cannot fetch this share (" + r.status + ")."; return; }
    payload = new Uint8Array(await r.arrayBuffer());
  }
  let data;
  try {
    const nonce = payload.slice(4, 16);
    const ciphertext = payload.slice(16);
    const cryptoKey = await crypto.subtle.importKey("raw", b64url(key), "AES-GCM", false, ["decrypt"]);
    data = JSON.parse(new TextDecoder().decode(await crypto.subtle.decrypt({name: "AES-GCM", iv: nonce}, cryptoKey, ciphertext)));
  } catch (e) { meta.textContent = "Cannot decrypt: wrong key or corrupt share."; return; }
  const m = data.meta || {};
  document.getElementById("title").textContent = m.title || "GreenCode share";
  meta.textContent = [m.model, m.cwd, m.created ? new Date(m.created * 1000).toLocaleString() : ""].filter(Boolean).join(" · ") + " · encrypted on the server";
  if (!data.messages) { content.appendChild(el("pre", "", JSON.stringify(data, null, 2))); return; }
  for (const message of data.messages) {
    const box = el("div", "turn " + (message.role || ""));
    for (const block of message.content || []) {
      if (block.type === "text") box.appendChild(el("div", "", (message.role === "user" ? "> " : "") + block.text));
      else if (block.type === "thinking") box.appendChild(el("div", "meta", block.thinking));
      else if (block.type === "tool_use") box.appendChild(el("pre", "tool", block.name + " " + JSON.stringify(block.input, null, 2)));
      else if (block.type === "tool_result") {
        const text = typeof block.content === "string" ? block.content : JSON.stringify(block.content, null, 2);
        box.appendChild(el("pre", "tool", text));
      }
    }
    content.appendChild(box);
  }
  if (data.runtime && data.runtime.context) meta.textContent += " · context " + data.runtime.context;
}
run();
</script></body></html>"#;

/// `greencode-cli share serve`: stores blobs under `dir` and prints the viewer base.
pub async fn serve_cli(host: &str, port: u16, dir: PathBuf) -> Result<()> {
    let listener = TcpListener::bind((host, port)).await.with_context(|| format!("cannot listen on {host}:{port}"))?;
    let token = serve_token()?;
    let addr = localserver::address(&listener);
    println!("Share server on http://{addr}");
    println!("Upload token: {token}");
    serve(listener, dir, token).await
}

/// The upload token for a local server: `$GREENCODE_SHARE_TOKEN`, else a stored one.
pub fn serve_token() -> Result<String> {
    if let Some(token) = std::env::var("GREENCODE_SHARE_TOKEN").ok().filter(|v| !v.trim().is_empty()) {
        return Ok(token);
    }
    let path = crate::home().join("share-token");
    let token = match std::fs::read_to_string(&path) {
        Ok(text) if text.trim().len() == 32 && text.trim().bytes().all(|b| b.is_ascii_hexdigit()) => text.trim().to_owned(),
        _ => {
            let token = crate::remote::random_hex()?;
            crate::remote::write_private(&path, token.as_bytes())?;
            token
        }
    };
    // Uploads from this process use it without further setup.
    unsafe { std::env::set_var("GREENCODE_SHARE_TOKEN", &token) };
    Ok(token)
}

/// Default directory shared blobs live in.
pub fn store_dir() -> PathBuf {
    crate::home().join("shares")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::collab::{CollabCfg, ShareCfg};

    fn home(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("greencode-share-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn encryption_round_trips_and_rejects_a_wrong_key() {
        let (payload, key) = encrypt(b"{\"hello\":1}").unwrap();
        assert_eq!(&payload[..4], MAGIC);
        assert_eq!(decrypt(&payload, &key).unwrap(), b"{\"hello\":1}");
        let (_, other) = encrypt(b"x").unwrap();
        assert!(decrypt(&payload, &other).is_err());
        assert!(decrypt(b"short", &key).is_err());
    }

    #[tokio::test]
    async fn publish_uploads_an_encrypted_redacted_blob_and_open_decrypts_it() {
        let dir = home("blobs");
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let base = format!("http://{addr}");
        let token = "test-share-token";
        let server = tokio::spawn(serve(listener, dir.clone(), token.into()));
        unsafe { std::env::set_var("GREENCODE_SHARE_TOKEN", token) };
        let mut cfg = Config::default();
        cfg.collab_opts.collab = CollabCfg::default();
        cfg.collab_opts.share = ShareCfg { server_url: Some(base.clone()), store: ShareStore::Blob, redact_secrets: true };
        let snapshot = json!({
            "meta": {"id": "t1", "title": "Demo", "model": "test/model"},
            "messages": [{"role": "user", "content": [{"type": "text", "text": "token sk-abcdefghijklmnopqrstuvwxyz and ordinary words"}]}],
        });
        let link = publish_value(&cfg, snapshot).await.unwrap();
        let (body, key) = link.split_once('#').unwrap();
        assert!(body.starts_with(&format!("{base}/")), "{link}");
        assert_eq!(key.len(), 43, "32 bytes base64url without padding");
        let page = reqwest::Client::new().get(body).send().await.unwrap();
        assert_eq!(page.status(), 200);
        assert!(page.text().await.unwrap().contains("crypto.subtle"));
        let value = open(&link).await.unwrap();
        let text = value.to_string();
        assert!(text.contains("ordinary words"));
        assert!(!text.contains("sk-abcdefghijklmnopqrstuvwxyz"), "redacted before upload: {text}");
        // The server only ever held ciphertext.
        let id = body.rsplit('/').next().unwrap();
        let stored = std::fs::read(dir.join(id)).unwrap();
        assert!(!String::from_utf8_lossy(&stored).contains("ordinary words"));
        assert_eq!(reqwest::Client::new().post(format!("{base}/v1/blobs")).body("x").send().await.unwrap().status(), 401);
        std::fs::remove_dir_all(&dir).ok();
        server.abort();
    }

    #[test]
    fn links_and_servers_need_https_except_loopback() {
        let mut cfg = Config::default();
        cfg.collab_opts.share.server_url = Some("http://share.example.com".into());
        assert!(server_base(&cfg).is_err());
        cfg.collab_opts.share.server_url = Some("http://localhost:8080".into());
        assert_eq!(server_base(&cfg).unwrap(), "http://localhost:8080");
        cfg.collab_opts.share.server_url = None;
        assert!(server_base(&cfg).unwrap_err().to_string().contains("share.server_url is not set"));
    }
}
