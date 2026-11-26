//! `greencode-cli stream`: publish a live transcript to a stream server viewers watch in a browser.
//!
//! Wire protocol (GreenCode's own):
//! - `POST /v1/streams/<user>/rows` (bearer token) NDJSON, one event JSON per line; server assigns `seq`
//! - `GET /v1/streams/<user>/rows?after=N` → `{"rows":[{"seq","event"}],"next":N}`
//! - `GET /<user>` is the browser viewer, polling the rows endpoint
use crate::{
    config::Config,
    localserver::{self, Request, Response},
    redact::Redactor,
};
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::{
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::net::TcpListener;

/// `stream.server_url`, normalized like the relay URL.
pub fn base(cfg: &Config) -> Result<String> {
    let base = crate::collab::http_base(cfg.collab_opts.stream.server_url.as_deref().context("stream.server_url is not set")?)?;
    let rest = base.split_once("://").map(|(_, r)| r).unwrap_or_default();
    let host = rest.split(['/', ':']).next().unwrap_or_default();
    if base.starts_with("http://") && !matches!(host, "localhost" | "127.0.0.1" | "[::1]") {
        bail!("stream.server_url must be https:// (http:// is only for localhost)");
    }
    Ok(base)
}

/// Sanitized viewer name: `collab.displayName`, else `$USER`.
pub fn user(cfg: &Config) -> String {
    let name: String = crate::collab::display_name(cfg)
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '-' })
        .collect();
    let name = name.trim_matches('-').to_lowercase();
    if name.is_empty() { "greencode".into() } else { name }
}

fn token() -> Result<String> {
    std::env::var("GREENCODE_STREAM_TOKEN")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .or_else(|| crate::secrets::env().into_iter().find(|(name, _)| name == "STREAM_TOKEN").map(|(_, v)| v))
        .context("set GREENCODE_STREAM_TOKEN (or save STREAM_TOKEN with the secrets tool) to stream")
}

fn client() -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder().timeout(Duration::from_secs(60)).build()?)
}

/// Redacted session rows for a thread, in order: history first, then each message block as its own row.
pub fn rows_for(redactor: &Redactor, thread: &str) -> Result<Vec<Value>> {
    let (meta, messages) = crate::session::Session::load(thread).with_context(|| format!("thread {thread} not found"))?;
    let mut row = json!({"type": "history", "title": meta.title, "model": meta.model, "messages": messages});
    redactor.json(&mut row);
    Ok(vec![row])
}

async fn publish(base: &str, user: &str, rows: &[Value]) -> Result<()> {
    let body = rows.iter().map(Value::to_string).collect::<Vec<_>>().join("\n");
    let response = client()?
        .post(format!("{base}/v1/streams/{user}/rows"))
        .bearer_auth(token()?)
        .header("content-type", "application/x-ndjson")
        .body(body)
        .send()
        .await
        .with_context(|| format!("stream server {base} is unreachable"))?;
    if !response.status().is_success() {
        bail!("stream server answered {} (set GREENCODE_STREAM_TOKEN to its token)", response.status());
    }
    Ok(())
}

/// Publishes the saved transcript, then (with `follow`) polls the session file and streams new rows.
pub async fn run(cfg: &Config, thread: &str, follow: bool, poll: Duration) -> Result<()> {
    let base = base(cfg)?;
    let user = user(cfg);
    let redactor = Redactor::from_config(cfg)?;
    publish(&base, &user, &rows_for(&redactor, thread)?).await?;
    println!("Streaming to {base}/{user}");
    if !follow {
        return Ok(());
    }
    let mut seen = std::fs::metadata(crate::session::Session::path(thread)).map(|m| m.len()).unwrap_or(0);
    loop {
        tokio::time::sleep(poll).await;
        let Ok(metadata) = std::fs::metadata(crate::session::Session::path(thread)) else { continue };
        if metadata.len() <= seen {
            continue;
        }
        let mut file = std::fs::File::open(crate::session::Session::path(thread))?;
        use std::io::{Read, Seek};
        file.seek(std::io::SeekFrom::Start(seen))?;
        let mut text = String::new();
        file.read_to_string(&mut text)?;
        seen = metadata.len();
        let mut rows = Vec::new();
        for line in text.lines() {
            let Ok(mut value) = serde_json::from_str::<Value>(line) else { continue };
            // Session lines are Message (or bookkeeping); publish message-shaped rows only.
            if value.get("role").is_none() {
                continue;
            }
            redactor.json(&mut value);
            rows.push(json!({"type": "message", "message": value}));
        }
        if rows.is_empty() {
            continue;
        }
        publish(&base, &user, &rows).await?;
    }
}

/// Serves streams and the browser viewer until the listener closes.
pub async fn serve(listener: TcpListener, dir: PathBuf, token: String) -> Result<()> {
    std::fs::create_dir_all(&dir)?;
    let state = std::sync::Arc::new((dir, token));
    localserver::serve(listener, 8 * 1024 * 1024, move |request| {
        let state = state.clone();
        async move { handle(state, request).await }
    })
    .await
}

fn valid_user(user: &str) -> bool {
    !user.is_empty() && user.len() <= 64 && user.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

fn rows_path(dir: &Path, user: &str) -> PathBuf {
    dir.join(format!("{user}.jsonl"))
}

fn read_rows(dir: &Path, user: &str, after: usize) -> Vec<Value> {
    let text = std::fs::read_to_string(rows_path(dir, user)).unwrap_or_default();
    text.lines().enumerate().skip(after).filter_map(|(i, line)| {
        Some(json!({"seq": i + 1, "event": serde_json::from_str::<Value>(line).ok()?}))
    }).collect()
}

async fn handle(state: std::sync::Arc<(PathBuf, String)>, request: Request) -> Response {
    let (dir, token) = &*state;
    let (method, path) = (request.method.as_str(), request.path.as_str());
    if let Some(rest) = path.strip_prefix("/v1/streams/") {
        let (user, action) = rest.split_once('/').unwrap_or((rest, ""));
        if !valid_user(user) {
            return Response::error("400 Bad Request", "invalid stream name");
        }
        match (method, action) {
            ("POST", "rows") => {
                if request.token() != Some(token.as_str()) {
                    return Response::error("401 Unauthorized", "stream token required");
                }
                use std::io::Write;
                let mut file = match std::fs::OpenOptions::new().create(true).append(true).open(rows_path(dir, user)) {
                    Ok(file) => file,
                    Err(e) => return Response::error("500 Internal Server Error", &format!("cannot store rows: {e}")),
                };
                let mut count = 0;
                for line in std::str::from_utf8(&request.body).unwrap_or_default().lines() {
                    let Ok(value) = serde_json::from_str::<Value>(line) else {
                        return Response::error("400 Bad Request", "rows must be JSON, one per line");
                    };
                    if writeln!(file, "{value}").is_err() {
                        return Response::error("500 Internal Server Error", "cannot write rows");
                    }
                    count += 1;
                }
                return Response::json(json!({"ok": true, "count": count}));
            }
            ("GET", "rows") => {
                        let after = request.query("after").and_then(|v| v.parse().ok()).unwrap_or(0usize);
                let rows = read_rows(dir, user, after);
                let next = rows.last().and_then(|r| r["seq"].as_u64()).unwrap_or(after as u64);
                return Response::json(json!({"rows": rows, "next": next}));
            }
            _ => return Response::error("404 Not Found", "not found"),
        }
    }
    if method == "GET" && path != "/" && !path.starts_with("/v1/") {
        let user = path.trim_start_matches('/');
        if valid_user(user) && rows_path(dir, user).exists() {
            return Response::html(VIEWER_HTML);
        }
        return Response::error("404 Not Found", "no such stream");
    }
    Response::error("404 Not Found", "not found")
}

const VIEWER_HTML: &str = r#"<!doctype html>
<html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width, initial-scale=1">
<title>GreenCode stream</title>
<style>body{font:15px/1.5 system-ui,sans-serif;margin:0;background:#0e1113;color:#e6e6e6}
header{position:sticky;top:0;background:#0e1113;border-bottom:1px solid #2a2f34;padding:10px 16px;display:flex;gap:12px;align-items:center}
main{padding:12px 16px;font:13px/1.55 ui-monospace,monospace;white-space:pre-wrap;word-break:break-word}
.user{color:#8ab4f8}.assistant{color:#d8e8d8}.tool{color:#f0c674}.think{color:#8b949e;font-style:italic}.error{color:#ff7b72}
.row{padding:5px 0;border-bottom:1px solid #171b1e}#state{color:#8b949e;font-size:13px}</style></head><body>
<header><strong id="title">GreenCode stream</strong><span id="state">connecting…</span></header><main id="log"></main>
<script>
const user = location.pathname.split("/").pop();
const log = document.getElementById("log"), state = document.getElementById("state");
let after = 0;
function add(text, cls) { if (!text) return; const d = document.createElement("div"); d.className = "row " + (cls || ""); d.textContent = text;
  log.appendChild(d); while (log.children.length > 4000) log.removeChild(log.firstChild); window.scrollTo(0, document.body.scrollHeight); }
function render(e) {
  if (e.type === "history") {
    document.getElementById("title").textContent = "GreenCode · " + (e.title || e.model || user);
    for (const m of e.messages || []) for (const b of m.content || []) {
      if (b.type === "text") add((m.role === "user" ? "> " : "") + b.text, m.role);
      else if (b.type === "thinking") add(b.thinking, "think");
      else if (b.type === "tool_use") add(b.name + " " + JSON.stringify(b.input), "tool");
      else if (b.type === "tool_result") add(typeof b.content === "string" ? b.content : JSON.stringify(b.content), "tool");
    }
  } else if (e.type === "message") {
    const m = e.message || {};
    for (const b of m.content || []) {
      if (b.type === "text") add((m.role === "user" ? "> " : "") + b.text, m.role);
      else if (b.type === "thinking") add(b.thinking, "think");
      else if (b.type === "tool_use") add(b.name + " " + JSON.stringify(b.input), "tool");
      else if (b.type === "tool_result") add(typeof b.content === "string" ? b.content : JSON.stringify(b.content), "tool");
    }
  } else if (e.type === "text") add(e.text, "assistant");
  else if (e.type === "tool_start") add(e.name + " " + JSON.stringify(e.input), "tool");
  else if (e.type === "tool_end") add(e.output, e.is_error ? "error" : "tool");
  else if (e.type === "error") add("error: " + e.text, "error");
}
async function poll() {
  try {
    const r = await fetch(`/v1/streams/${user}/rows?after=${after}`);
    if (!r.ok) { state.textContent = "no such stream"; return; }
    const v = await r.json();
    for (const row of v.rows) { after = row.seq; render(row.event); }
    state.textContent = "live";
  } catch (_) { state.textContent = "reconnecting…"; }
  setTimeout(poll, 1000);
}
poll();
</script></body></html>"#;

/// `greencode-cli stream serve`: stores streams under `dir` and prints the viewer base.
pub async fn serve_cli(host: &str, port: u16, dir: PathBuf) -> Result<()> {
    let listener = TcpListener::bind((host, port)).await.with_context(|| format!("cannot listen on {host}:{port}"))?;
    let token = serve_token()?;
    let addr = localserver::address(&listener);
    println!("Stream server on http://{addr}");
    println!("Stream token: {token}");
    serve(listener, dir, token).await
}

/// The token a local stream server requires; reused by publishing from this process.
pub fn serve_token() -> Result<String> {
    if let Some(token) = std::env::var("GREENCODE_STREAM_TOKEN").ok().filter(|v| !v.trim().is_empty()) {
        return Ok(token);
    }
    let path = crate::home().join("stream-token");
    let token = match std::fs::read_to_string(&path) {
        Ok(text) if text.trim().len() == 32 && text.trim().bytes().all(|b| b.is_ascii_hexdigit()) => text.trim().to_owned(),
        _ => {
            let token = crate::remote::random_hex()?;
            crate::remote::write_private(&path, token.as_bytes())?;
            token
        }
    };
    unsafe { std::env::set_var("GREENCODE_STREAM_TOKEN", &token) };
    Ok(token)
}

pub fn store_dir() -> PathBuf {
    crate::home().join("streams")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::collab::StreamCfg;

    #[tokio::test]
    async fn publisher_server_and_viewer_round_trip_with_redaction() {
        let dir = std::env::temp_dir().join(format!("greencode-stream-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let base = format!("http://{addr}");
        let token = "stream-test-token";
        let server = tokio::spawn(serve(listener, dir.clone(), token.into()));
        unsafe { std::env::set_var("GREENCODE_STREAM_TOKEN", token) };
        let mut cfg = Config::default();
        cfg.collab_opts.stream = StreamCfg { server_url: Some(base.clone()), redact_patterns: vec!["internal-[0-9]+".into()] };
        cfg.collab_opts.collab.display_name = Some("Ada Lovelace".into());
        let redactor = Redactor::new(&cfg.collab_opts.stream.redact_patterns).unwrap();
        let message = json!({"role": "user", "content": [{"type": "text", "text": "internal-42 sk-abcdefghijklmnopqrstuvwxyz"}]});
        let mut row = message;
        redactor.json(&mut row);
        assert_eq!(row["content"][0]["text"], "[redacted] [redacted]");
        publish(&base, &user(&cfg), &[row]).await.unwrap();
        let rows: Value = reqwest::Client::new().get(format!("{base}/v1/streams/ada-lovelace/rows?after=0")).send().await.unwrap().json().await.unwrap();
        assert_eq!(rows["rows"][0]["seq"], 1);
        assert_eq!(rows["next"], 1);
        assert_eq!(rows["rows"][0]["event"]["content"][0]["text"], "[redacted] [redacted]");
        let page = reqwest::Client::new().get(format!("{base}/ada-lovelace")).send().await.unwrap();
        assert_eq!(page.status(), 200);
        assert!(page.text().await.unwrap().contains("GreenCode stream"));
        assert_eq!(reqwest::Client::new().post(format!("{base}/v1/streams/ada-lovelace/rows")).body("{}").send().await.unwrap().status(), 401);
        // A name with an illegal byte is refused before any file is named after it.
        assert_eq!(client().unwrap().post(format!("{base}/v1/streams/bad name/rows")).bearer_auth(token).body("{}").send().await.unwrap().status(), 400);
        assert!(base_for(&"http://127.0.0.1:1").is_ok());
        std::fs::remove_dir_all(&dir).ok();
        server.abort();
    }

    fn base_for(url: &str) -> Result<String> {
        let mut cfg = Config::default();
        cfg.collab_opts.stream.server_url = Some(url.into());
        base(&cfg)
    }

    #[test]
    fn external_stream_servers_must_be_https() {
        let mut cfg = Config::default();
        cfg.collab_opts.stream.server_url = Some("http://stream.example.com".into());
        assert!(base(&cfg).is_err());
        cfg.collab_opts.stream.server_url = Some("wss://stream.example.com".into());
        assert_eq!(base(&cfg).unwrap(), "https://stream.example.com");
        cfg.collab_opts.stream.server_url = None;
        assert!(base(&cfg).is_err());
        cfg.collab_opts.collab.display_name = Some("Ada Lovelace".into());
        assert_eq!(user(&cfg), "ada-lovelace");
    }
}
