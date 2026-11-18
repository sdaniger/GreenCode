//! Token-protected HTTP access to local threads.
use std::{collections::HashMap, path::PathBuf, sync::{Arc, Mutex}, time::Duration};
use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::{io::{AsyncReadExt, AsyncWriteExt}, net::{TcpListener, TcpStream}, sync::{oneshot, watch}};
use crate::{config::Config, permission::Perms, session::{Meta, Session}, Command, Decision, Event};

struct Turn {
    events: Mutex<Vec<Value>>,
    changed: watch::Sender<usize>,
    permission: Mutex<Option<oneshot::Sender<Decision>>>,
    done: Mutex<bool>,
}
struct State { token: String, cfg: Arc<Config>, turns: Mutex<HashMap<String, Arc<Turn>>> }

/// Loads the existing secret or creates a new owner-only secret.
pub fn token(rotate: bool) -> Result<String> {
    let path = crate::home().join("remote-token");
    if !rotate {
        match std::fs::read_to_string(&path) {
            Ok(s) if s.len() == 32 && s.bytes().all(|b| b.is_ascii_hexdigit()) => return Ok(s),
            Ok(_) => bail!("invalid remote token file"),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {},
            Err(e) => return Err(e.into()),
        }
    }
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).map_err(|e| anyhow::anyhow!("random token: {e}"))?;
    let value: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    std::fs::create_dir_all(crate::home())?;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    { use std::os::unix::fs::OpenOptionsExt; options.mode(0o600); }
    use std::io::Write;
    let mut file = options.open(path)?;
    #[cfg(unix)]
    { use std::os::unix::fs::PermissionsExt; file.set_permissions(std::fs::Permissions::from_mode(0o600))?; }
    file.write_all(value.as_bytes())?;
    Ok(value)
}

/// Serves until the listener is closed or the process exits.
pub async fn serve(listener: TcpListener, token: String, cfg: Config) -> Result<()> {
    let state = Arc::new(State { token, cfg: Arc::new(cfg), turns: Mutex::new(HashMap::new()) });
    loop {
        let (stream, _) = listener.accept().await?;
        let state = state.clone();
        tokio::spawn(async move { let _ = handle(stream, state).await; });
    }
}

#[derive(Deserialize)]
struct Create { cwd: PathBuf, prompt: String, backend: Option<String>, model: Option<String> }
#[derive(Deserialize)]
struct Prompt { text: String }
#[derive(Deserialize)]
struct Approval { decision: String }

fn start(state: &Arc<State>, meta: Meta, text: String) -> Result<(), &'static str> {
    if text.trim().is_empty() { return Err("empty prompt"); }
    if !meta.cwd.is_dir() { return Err("cwd must be a directory"); }
    if let Some(policy) = &state.cfg.policy {
        if policy.check_mode(state.cfg.mode).is_err() || !policy.backend_allowed(&meta.backend) { return Err("blocked by policy"); }
    }
    let mut turns = state.turns.lock().unwrap();
    if turns.get(&meta.id).is_some_and(|t| !*t.done.lock().unwrap()) { return Err("turn already running"); }
    let (changed, _) = watch::channel(0);
    let turn = Arc::new(Turn { events: Mutex::new(Vec::new()), changed, permission: Mutex::new(None), done: Mutex::new(false) });
    turns.insert(meta.id.clone(), turn.clone());
    let (tx, mut rx) = crate::spawn(&tokio::runtime::Handle::current(), state.cfg.clone(), meta, Arc::new(Perms::new(state.cfg.mode)));
    let _ = tx.send(Command::Prompt(text));
    tokio::spawn(async move {
        let _tx = tx;
        while let Some(event) = rx.recv().await {
            let value = crate::event_json(&event);
            let done = matches!(event, Event::Done);
            match event {
                Event::Permission { reply, .. } => *turn.permission.lock().unwrap() = Some(reply),
                Event::Dialog { reply, .. } => { let _ = reply.send(json!({"cancelled": true})); },
                _ => {}
            }
            if done { *turn.done.lock().unwrap() = true; }
            let mut events = turn.events.lock().unwrap();
            events.push(value);
            turn.changed.send_replace(events.len());
            if done { break; }
        }
        // The agent can exit without `Event::Done` (crash, channel closed): never leave the turn "running".
        finish_turn(&turn, "agent exited before finishing the turn");
    });
    Ok(())
}

/// Marks a turn finished and publishes one terminal error, unless it already ended.
fn finish_turn(turn: &Turn, text: &str) {
    let mut done = turn.done.lock().unwrap();
    if *done { return; }
    *done = true;
    let mut events = turn.events.lock().unwrap();
    events.push(json!({"type":"error","text":text}));
    turn.changed.send_replace(events.len());
}

async fn response(stream: &mut TcpStream, status: &str, content_type: &str, body: &[u8]) -> Result<()> {
    stream.write_all(format!("HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\nX-Content-Type-Options: nosniff\r\nCache-Control: no-store\r\nReferrer-Policy: no-referrer\r\n\r\n", body.len()).as_bytes()).await?;
    stream.write_all(body).await?;
    Ok(())
}
async fn reply(stream: &mut TcpStream, status: &str, value: Value) -> Result<()> {
    response(stream, status, "application/json", value.to_string().as_bytes()).await
}

async fn handle(mut stream: TcpStream, state: Arc<State>) -> Result<()> {
    let mut data = Vec::new();
    let mut buf = [0; 4096];
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    let (header_end, len) = loop {
        let n = tokio::time::timeout_at(deadline, stream.read(&mut buf)).await??;
        if n == 0 { bail!("incomplete request"); }
        data.extend_from_slice(&buf[..n]);
        if data.len() > 1_048_576 { bail!("request too large"); }
        if let Some(pos) = data.windows(4).position(|x| x == b"\r\n\r\n") {
            let end = pos + 4;
            let headers = std::str::from_utf8(&data[..pos])?;
            let mut lengths = headers.lines().skip(1).filter_map(|line| line.split_once(':')).filter(|(k, _)| k.eq_ignore_ascii_case("content-length"));
            let len = lengths.next().map(|(_, v)| v.trim().parse::<usize>()).transpose()?.unwrap_or(0);
            if lengths.next().is_some() || headers.lines().any(|line| line.to_ascii_lowercase().starts_with("transfer-encoding:")) || len > 1_000_000 { bail!("invalid request length"); }
            if data.len() >= end + len { break (end, len); }
        }
    };
    let headers = std::str::from_utf8(&data[..header_end])?;
    let mut lines = headers.lines();
    let mut request = lines.next().context("missing request")?.split_whitespace();
    let method = request.next().context("missing method")?;
    let target = request.next().context("missing target")?;
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    let mut hosts = lines.clone().filter_map(|line| line.split_once(':')).filter(|(k, _)| k.eq_ignore_ascii_case("host"));
    let host = hosts.next().map(|(_, v)| v.trim());
    if hosts.next().is_some() { return reply(&mut stream, "400 Bad Request", json!({"error":"invalid host"})).await; }
    let mut origins = lines.clone().filter_map(|line| line.split_once(':')).filter(|(k, _)| k.eq_ignore_ascii_case("origin"));
    if let Some((_, origin)) = origins.next() {
        let valid = url::Url::parse(origin.trim()).ok().is_some_and(|url| {
            matches!(url.scheme(), "http" | "https")
                && url.username().is_empty() && url.password().is_none()
                && url.path() == "/" && url.query().is_none() && url.fragment().is_none()
                && host.is_some_and(|host| url::Url::parse(&format!("{}://{host}/", url.scheme())).ok().is_some_and(|h| h.origin() == url.origin()))
        });
        if !valid || origins.next().is_some() { return reply(&mut stream, "403 Forbidden", json!({"error":"invalid origin"})).await; }
    }
    let authenticated = url::form_urlencoded::parse(query.as_bytes()).any(|(k, v)| k == "token" && v == state.token)
        || lines.clone().filter_map(|line| line.split_once(':')).any(|(k, v)| k.eq_ignore_ascii_case("authorization") && v.trim().strip_prefix("Bearer ") == Some(state.token.as_str()));
    if !authenticated { return reply(&mut stream, "401 Unauthorized", json!({"error":"unauthorized"})).await; }
    if method == "POST" && path == "/mcp" {
        let result = match serde_json::from_slice(&data[header_end..header_end + len]) {
            Ok(req) => crate::mcp_server::handle_with_config(req, Ok((*state.cfg).clone())).await,
            Err(_) => Some(json!({"jsonrpc":"2.0","id":null,"error":{"code":-32700,"message":"Parse error"}})),
        };
        return match result {
            Some(value) => reply(&mut stream, "200 OK", value).await,
            None => response(&mut stream, "202 Accepted", "application/json", b"").await,
        };
    }
    if method == "GET" && path == "/" { return response(&mut stream, "200 OK", "text/html; charset=utf-8", include_bytes!("remote.html")).await; }
    if method == "GET" && path == "/api/threads" { return reply(&mut stream, "200 OK", json!(Session::list())).await; }
    if method == "POST" && path == "/api/threads" {
        let create: Create = match serde_json::from_slice(&data[header_end..header_end+len]) { Ok(v) => v, Err(_) => return reply(&mut stream, "400 Bad Request", json!({"error":"invalid JSON"})).await };
        let backend = create.backend.unwrap_or_else(|| "native".into());
        let model = create.model.unwrap_or_else(|| if backend == "native" { state.cfg.model.clone() } else { "default".into() });
        let mut meta = Meta::new(create.cwd, model, backend);
        meta.subagent_model = state.cfg.subagent_model.clone();
        return match start(&state, meta.clone(), create.prompt) {
            Ok(()) => reply(&mut stream, "201 Created", json!(meta)).await,
            Err(e) => reply(&mut stream, "400 Bad Request", json!({"error":e})).await,
        };
    }
    let parts: Vec<_> = path.trim_matches('/').split('/').collect();
    if parts.len() >= 3 && parts[0] == "api" && parts[1] == "threads" {
        let id = parts[2];
        if method == "GET" && parts.len() == 3 {
            return match Session::load(id) {
                Ok((meta, messages)) => reply(&mut stream, "200 OK", json!({"meta":meta,"messages":messages})).await,
                Err(_) => reply(&mut stream, "404 Not Found", json!({"error":"thread not found"})).await,
            };
        }
        if method == "POST" && parts.len() == 4 && parts[3] == "prompt" {
            let prompt: Prompt = match serde_json::from_slice(&data[header_end..header_end+len]) { Ok(v) => v, Err(_) => return reply(&mut stream, "400 Bad Request", json!({"error":"invalid JSON"})).await };
            let running = state.turns.lock().unwrap().get(id).is_some_and(|t| !*t.done.lock().unwrap());
            if running { return reply(&mut stream, "409 Conflict", json!({"error":"turn already running"})).await; }
            let meta = match Session::load(id) { Ok((meta, _)) => meta, Err(_) => return reply(&mut stream, "404 Not Found", json!({"error":"thread not found"})).await };
            return match start(&state, meta, prompt.text) {
                Ok(()) => reply(&mut stream, "202 Accepted", json!({"ok":true})).await,
                Err(e) => reply(&mut stream, if e == "turn already running" { "409 Conflict" } else { "400 Bad Request" }, json!({"error":e})).await,
            };
        }
        if parts.len() == 4 {
            let turn = state.turns.lock().unwrap().get(id).cloned();
            let Some(turn) = turn else { return reply(&mut stream, "404 Not Found", json!({"error":"turn not found"})).await };
            if method == "POST" && parts[3] == "approve" {
                let approval: Approval = match serde_json::from_slice(&data[header_end..header_end+len]) { Ok(v) => v, Err(_) => return reply(&mut stream, "400 Bad Request", json!({"error":"invalid JSON"})).await };
                let decision = match approval.decision.as_str() { "allow" => Decision::Allow, "always" => Decision::Always, "deny" => Decision::Deny, _ => return reply(&mut stream, "400 Bad Request", json!({"error":"invalid decision"})).await };
                let sender = turn.permission.lock().unwrap().take();
                return if sender.is_some_and(|sender| sender.send(decision).is_ok()) { reply(&mut stream, "200 OK", json!({"ok":true})).await } else { reply(&mut stream, "409 Conflict", json!({"error":"no pending permission"})).await };
            }
            if method == "GET" && parts[3] == "events" {
                stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-store\r\nConnection: close\r\nX-Content-Type-Options: nosniff\r\nReferrer-Policy: no-referrer\r\n\r\n").await?;
                let mut changed = turn.changed.subscribe();
                let mut index = 0;
                loop {
                    let events = { let events = turn.events.lock().unwrap(); events[index..].to_vec() };
                    for event in events {
                        index += 1;
                        stream.write_all(format!("data: {event}\n\n").as_bytes()).await?;
                        if event["type"] == "done" { return Ok(()); }
                    }
                    if changed.changed().await.is_err() { return Ok(()); }
                }
            }
        }
    }
    reply(&mut stream, "404 Not Found", json!({"error":"not found"})).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_closed_channel_finishes_the_turn_once() {
        let (changed, _) = watch::channel(0);
        let turn = Turn { events: Mutex::new(Vec::new()), changed, permission: Mutex::new(None), done: Mutex::new(false) };
        finish_turn(&turn, "agent exited before finishing the turn");
        finish_turn(&turn, "second");
        assert!(*turn.done.lock().unwrap());
        let events = turn.events.lock().unwrap();
        assert_eq!(events.len(), 1, "only one terminal event");
        assert_eq!(events[0]["type"], "error");
    }
}
