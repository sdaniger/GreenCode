//! Token-protected HTTP access to local threads.
use std::{collections::HashMap, net::TcpStream as StdTcpStream, path::PathBuf, sync::{Arc, Mutex}, time::Duration};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::{io::{AsyncReadExt, AsyncWriteExt}, net::{TcpListener, TcpStream}, sync::{mpsc, oneshot, watch}};
use crate::{config::Config, permission::Perms, session::{Meta, Session}, Command, Decision, Event};

struct Turn {
    events: Mutex<Vec<Value>>,
    changed: watch::Sender<usize>,
    permission: Mutex<Option<oneshot::Sender<Decision>>>,
    dialog: Mutex<Option<oneshot::Sender<Value>>>,
    /// Dropped when the turn ends so the engine can exit.
    cancel: Mutex<Option<mpsc::UnboundedSender<Command>>>,
}

impl Turn {
    fn is_done(&self) -> bool {
        self.events.lock().unwrap().last().is_some_and(|e| e["type"] == "done")
    }
}

/// A read-only link to one thread. Requests are guest messages waiting for the owner to send or dismiss them.
#[derive(Clone, Serialize, Deserialize)]
struct Guest { token: String, thread: String, expires: u64, requests: Vec<GuestRequest> }
#[derive(Clone, Serialize, Deserialize)]
struct GuestRequest { id: String, text: String, at: u64 }
struct State {
    token: String,
    cfg: Arc<Config>,
    turns: Mutex<HashMap<String, Arc<Turn>>>,
    guests: Mutex<Vec<Guest>>,
    guest_changes: watch::Sender<()>,
}

const GUEST_TTL: u64 = 24 * 3600;
const GUEST_REQUESTS: usize = 20;
const GUEST_TEXT: usize = 8000;

fn now() -> u64 { std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs()) }
fn guests_path() -> PathBuf { crate::home().join("guest-links.json") }

pub(crate) fn random_hex() -> Result<String> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).map_err(|e| anyhow::anyhow!("random token: {e}"))?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

/// Replaces `path` with `data`, readable by the owner only.
pub(crate) fn write_private(path: &std::path::Path, data: &[u8]) -> Result<()> {
    std::fs::create_dir_all(crate::home())?;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    { use std::os::unix::fs::OpenOptionsExt; options.mode(0o600); }
    use std::io::Write;
    let mut file = options.open(path)?;
    #[cfg(unix)]
    { use std::os::unix::fs::PermissionsExt; file.set_permissions(std::fs::Permissions::from_mode(0o600))?; }
    file.write_all(data)?;
    Ok(())
}

/// Runs `f` on the live guest links (expired ones are dropped first) and saves the result.
fn with_guests<R>(state: &State, f: impl FnOnce(&mut Vec<Guest>) -> R) -> R {
    let mut guests = state.guests.lock().unwrap();
    guests.retain(|g| g.expires > now());
    let result = f(&mut guests);
    state.guest_changes.send_replace(());
    // A failed save only loses links across a restart; the live links keep working.
    let _ = serde_json::to_vec(&*guests).map(|data| write_private(&guests_path(), &data));
    result
}

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
    let value = random_hex()?;
    write_private(&path, value.as_bytes())?;
    Ok(value)
}

/// Serves until the listener is closed or the process exits.
pub async fn serve(listener: TcpListener, token: String, cfg: Config) -> Result<()> {
    let guests = std::fs::read(guests_path()).ok().and_then(|d| serde_json::from_slice::<Vec<Guest>>(&d).ok()).unwrap_or_default();
    let state = Arc::new(State {
        token, cfg: Arc::new(cfg), turns: Mutex::new(HashMap::new()), guests: Mutex::new(guests),
        guest_changes: watch::channel(()).0,
    });
    with_guests(&state, |_| ());
    if let Ok(addr) = listener.local_addr() {
        let host = if addr.ip().is_unspecified() { format!("127.0.0.1:{}", addr.port()) } else { addr.to_string() };
        let _ = write_private(&crate::home().join("remote-url"), format!("http://{host}").as_bytes());
    }
    loop {
        let (stream, _) = listener.accept().await?;
        let state = state.clone();
        tokio::spawn(async move { let _ = handle(stream, state).await; });
    }
}

/// The address of the running `serve`, written when it starts. Loopback is used for a
/// wildcard bind; `share_link` replaces it with the LAN address when local access is on.
pub fn remote_url() -> Result<url::Url> {
    let text = std::fs::read_to_string(crate::home().join("remote-url")).context("Start `greencode-cli serve` first")?;
    let url = url::Url::parse(text.trim()).context("invalid remote-url file")?;
    if url.scheme() != "http" {
        bail!("remote-url must be an http URL");
    }
    Ok(url)
}

fn lan_address() -> Option<String> {
    if !crate::service::lan() {
        return None;
    }
    let socket = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("8.8.8.8:80").ok()?;
    let ip = socket.local_addr().ok()?.ip();
    (!ip.is_loopback() && !ip.is_unspecified()).then(|| ip.to_string())
}

/// Creates a read-only guest link through the running `serve`. Used by the desktop palette.
pub fn share_link(thread: &str) -> Result<String> {
    use std::io::{Read, Write};
    Session::load(thread).with_context(|| format!("thread {thread} not found"))?;
    let token = token(false).context("Start `greencode-cli serve` first (no remote token)")?;
    let mut url = remote_url()?;
    if let Some(lan) = lan_address() {
        if url.host_str().and_then(|h| h.parse::<std::net::IpAddr>().ok()).is_some_and(|ip| ip.is_loopback()) {
            url.set_host(Some(&lan)).context("invalid LAN address")?;
        }
    }
    let host = url.host_str().context("remote-url has no host")?;
    let port = url.port_or_known_default().unwrap_or(80);
    let addr = url.socket_addrs(|| None)?.into_iter().next().context("remote-url has no address")?;
    let mut stream = StdTcpStream::connect_timeout(&addr, Duration::from_millis(800)).context("GreenCode remote server is not running")?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    let request = format!(
        "POST /api/threads/{thread}/share HTTP/1.1\r\nHost: {host}:{port}\r\nAuthorization: Bearer {token}\r\nContent-Type: application/json\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
    );
    stream.write_all(request.as_bytes())?;
    let mut response = String::new();
    stream.read_to_string(&mut response)?;
    let (head, body) = response.split_once("\r\n\r\n").context("invalid response from GreenCode")?;
    let status = head.lines().next().and_then(|line| line.split_whitespace().nth(1)).and_then(|s| s.parse::<u16>().ok()).unwrap_or(500);
    let value: Value = serde_json::from_str(body).unwrap_or_else(|_| json!({"error": body.trim()}));
    if !(200..300).contains(&status) {
        bail!("{}", value["error"].as_str().unwrap_or("share failed"));
    }
    value["url"].as_str().map(str::to_owned).context("share response had no URL")
}

#[derive(Deserialize)]
struct Create { cwd: PathBuf, prompt: String, backend: Option<String>, model: Option<String> }
#[derive(Deserialize)]
struct Prompt { text: String }
#[derive(Deserialize)]
struct Approval { decision: String }
#[derive(Deserialize)]
struct Answer { reply: Value }
#[derive(Deserialize, Default)]
struct Share { ttl_secs: Option<u64> }

fn start(state: &Arc<State>, meta: Meta, text: String) -> Result<(), &'static str> {
    if text.trim().is_empty() { return Err("empty prompt"); }
    if !meta.cwd.is_dir() { return Err("cwd must be a directory"); }
    let mode = Session::view(&meta.id).map_err(|_| "cannot read thread permissions")?
        .map_or(state.cfg.mode, |view| view.mode);
    if let Some(policy) = &state.cfg.policy {
        if policy.check_mode(mode).is_err() || !policy.backend_allowed(&meta.backend) { return Err("blocked by policy"); }
    }
    let mut turns = state.turns.lock().unwrap();
    if turns.get(&meta.id).is_some_and(|t| !t.is_done()) { return Err("turn already running"); }
    let (changed, _) = watch::channel(0);
    let (tx, mut rx) = crate::spawn(&tokio::runtime::Handle::current(), state.cfg.clone(), meta.clone(), Arc::new(Perms::new(mode)));
    let turn = Arc::new(Turn { events: Mutex::new(Vec::new()), changed, permission: Mutex::new(None), dialog: Mutex::new(None), cancel: Mutex::new(Some(tx.clone())) });
    turns.insert(meta.id.clone(), turn.clone());
    let _ = tx.send(Command::Prompt(text));
    tokio::spawn(async move {
        let _tx = tx;
        while let Some(event) = rx.recv().await {
            let value = crate::event_json(&event);
            let done = matches!(event, Event::Done { .. });
            match event {
                Event::Permission { reply, .. } => *turn.permission.lock().unwrap() = Some(reply),
                Event::Dialog { reply, .. } => *turn.dialog.lock().unwrap() = Some(reply),
                _ => {}
            }
            let mut events = turn.events.lock().unwrap();
            events.push(value);
            turn.changed.send_replace(events.len());
            if done { break; }
        }
        // The agent can exit without `Event::Done` (crash, channel closed): never leave the turn "running".
        finish_turn(&turn, "agent exited before finishing the turn");
        turn.cancel.lock().unwrap().take();
    });
    Ok(())
}

/// A closed engine channel still has to terminate every event stream.
fn finish_turn(turn: &Turn, text: &str) {
    let mut events = turn.events.lock().unwrap();
    if events.last().is_some_and(|e| e["type"] == "done") { return; }
    events.push(json!({"type":"error","text":text}));
    events.push(json!({"type":"done"}));
    turn.changed.send_replace(events.len());
}

fn send_prompt(state: &Arc<State>, id: &str, text: String) -> (&'static str, Value) {
    let Ok((meta, _)) = Session::load(id) else { return ("404 Not Found", json!({"error":"thread not found"})) };
    match start(state, meta, text) {
        Ok(()) => ("202 Accepted", json!({"ok":true})),
        Err(e) => (if e == "turn already running" { "409 Conflict" } else { "400 Bad Request" }, json!({"error":e})),
    }
}

/// Streams a turn's events until it is done. Guests never see approval or question prompts.
async fn stream_events(stream: &mut TcpStream, turn: &Turn, guest: bool) -> Result<()> {
    stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-store\r\nConnection: close\r\nX-Content-Type-Options: nosniff\r\nReferrer-Policy: no-referrer\r\n\r\n").await?;
    let mut changed = turn.changed.subscribe();
    let mut index = 0;
    loop {
        let events = { let events = turn.events.lock().unwrap(); events[index..].to_vec() };
        for event in events {
            index += 1;
            if guest && matches!(event["type"].as_str(), Some("permission" | "dialog")) { continue; }
            stream.write_all(format!("data: {event}\n\n").as_bytes()).await?;
            if event["type"] == "done" { return Ok(()); }
        }
        if changed.changed().await.is_err() { return Ok(()); }
    }
}

async fn wait_for_guest_access_to_end(state: &State, token: &str, expires: u64) {
    let mut changes = state.guest_changes.subscribe();
    let elapsed = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
    let deadline = tokio::time::sleep(Duration::from_secs(expires).saturating_sub(elapsed));
    tokio::pin!(deadline);
    loop {
        if !state.guests.lock().unwrap().iter().any(|g| g.token == token) { return; }
        tokio::select! {
            _ = &mut deadline => return,
            _ = changes.changed() => {}
        }
    }
}

async fn guest(stream: &mut TcpStream, state: &Arc<State>, method: &str, rest: &str, body: &[u8]) -> Result<()> {
    let (token, tail) = rest.split_once('/').unwrap_or((rest, ""));
    let guest = state.guests.lock().unwrap().iter()
        .find(|g| g.token == token && g.expires > now()).map(|g| (g.thread.clone(), g.expires));
    let Some((thread, expires)) = guest else { return reply(stream, "404 Not Found", json!({"error":"link expired or revoked"})).await };
    match (method, tail) {
        ("GET", "") => response(stream, "200 OK", "text/html; charset=utf-8", include_bytes!("guest.html")).await,
        ("GET", "thread") => match Session::load(&thread) {
            Ok((meta, messages)) => reply(stream, "200 OK", json!({"title":meta.title,"model":meta.model,"messages":messages})).await,
            Err(_) => reply(stream, "404 Not Found", json!({"error":"thread not found"})).await,
        },
        ("GET", "events") => {
            let turn = state.turns.lock().unwrap().get(&thread).cloned();
            match turn.filter(|t| !t.is_done()) {
                Some(turn) => tokio::select! {
                    biased;
                    _ = wait_for_guest_access_to_end(state, token, expires) => Ok(()),
                    result = stream_events(stream, &turn, true) => result,
                },
                None => reply(stream, "404 Not Found", json!({"error":"no running turn"})).await,
            }
        }
        ("POST", "request") => {
            let Ok(Prompt { text }) = serde_json::from_slice(body) else { return reply(stream, "400 Bad Request", json!({"error":"invalid JSON"})).await };
            let text = text.trim().to_string();
            if text.is_empty() || text.len() > GUEST_TEXT { return reply(stream, "400 Bad Request", json!({"error":"empty or too long"})).await }
            let id = random_hex()?[..8].to_string();
            let added = with_guests(state, |guests| guests.iter_mut().find(|g| g.token == token).filter(|g| g.requests.len() < GUEST_REQUESTS).map(|g| g.requests.push(GuestRequest { id, text, at: now() })).is_some());
            if added { reply(stream, "202 Accepted", json!({"ok":true})).await } else { reply(stream, "429 Too Many Requests", json!({"error":"too many pending requests"})).await }
        }
        _ => reply(stream, "404 Not Found", json!({"error":"not found"})).await,
    }
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
    if let Some(rest) = path.strip_prefix("/guest/") { return guest(&mut stream, &state, method, rest, &data[header_end..header_end + len]).await; }
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
            let (status, value) = send_prompt(&state, id, prompt.text);
            return reply(&mut stream, status, value).await;
        }
        if method == "POST" && parts.len() == 4 && parts[3] == "share" {
            if Session::load(id).is_err() { return reply(&mut stream, "404 Not Found", json!({"error":"thread not found"})).await; }
            let body = &data[header_end..header_end+len];
            let share: Share = if body.is_empty() { Share::default() } else { match serde_json::from_slice(body) { Ok(v) => v, Err(_) => return reply(&mut stream, "400 Bad Request", json!({"error":"invalid JSON"})).await } };
            let (token, expires) = (random_hex()?, now() + share.ttl_secs.unwrap_or(GUEST_TTL).min(GUEST_TTL));
            with_guests(&state, |guests| {
                guests.retain(|g| g.thread != id);
                guests.push(Guest { token: token.clone(), thread: id.to_string(), expires, requests: Vec::new() });
            });
            let host = host.unwrap_or("127.0.0.1");
            return reply(&mut stream, "201 Created", json!({"token":token,"url":format!("http://{host}/guest/{token}"),"expires":expires})).await;
        }
        if method == "POST" && parts.len() == 4 && parts[3] == "unshare" {
            let revoked = with_guests(&state, |guests| { let n = guests.len(); guests.retain(|g| g.thread != id); n - guests.len() });
            return reply(&mut stream, "200 OK", json!({"revoked":revoked})).await;
        }
        if method == "GET" && parts.len() == 4 && parts[3] == "requests" {
            let requests: Vec<_> = state.guests.lock().unwrap().iter().filter(|g| g.thread == id && g.expires > now()).flat_map(|g| g.requests.clone()).collect();
            return reply(&mut stream, "200 OK", json!(requests)).await;
        }
        if method == "POST" && parts.len() == 6 && parts[3] == "requests" {
            let (status, value) = with_guests(&state, |guests| {
                let request = guests.iter_mut().filter(|g| g.thread == id)
                    .find_map(|g| g.requests.iter().position(|r| r.id == parts[4]).map(|i| (&mut g.requests, i)));
                let Some((requests, index)) = request else {
                    return ("404 Not Found", json!({"error":"request not found"}));
                };
                let (status, value) = match parts[5] {
                    "send" => send_prompt(&state, id, requests[index].text.clone()),
                    "dismiss" => ("200 OK", json!({"ok":true})),
                    _ => return ("404 Not Found", json!({"error":"not found"})),
                };
                if status.starts_with('2') { requests.remove(index); }
                (status, value)
            });
            return reply(&mut stream, status, value).await;
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
            if method == "POST" && parts[3] == "answer" {
                let answer: Answer = match serde_json::from_slice(&data[header_end..header_end+len]) { Ok(v) => v, Err(_) => return reply(&mut stream, "400 Bad Request", json!({"error":"invalid JSON"})).await };
                let sender = turn.dialog.lock().unwrap().take();
                return if sender.is_some_and(|sender| sender.send(answer.reply).is_ok()) { reply(&mut stream, "200 OK", json!({"ok":true})).await } else { reply(&mut stream, "409 Conflict", json!({"error":"no pending question"})).await };
            }
            if method == "POST" && parts[3] == "cancel" {
                // Pending prompts are answered first so the agent is not left waiting on a person.
                if let Some(sender) = turn.permission.lock().unwrap().take() { let _ = sender.send(Decision::Deny); }
                if let Some(sender) = turn.dialog.lock().unwrap().take() { let _ = sender.send(json!({"cancelled": true})); }
                let sent = {
                    let mut cancel = turn.cancel.lock().unwrap();
                    let sent = cancel.as_ref().is_some_and(|cancel| cancel.send(Command::Cancel).is_ok());
                    if sent { cancel.take(); }
                    sent
                };
                return if sent { reply(&mut stream, "200 OK", json!({"ok":true})).await } else { reply(&mut stream, "409 Conflict", json!({"error":"no running turn"})).await };
            }
            if method == "GET" && parts[3] == "events" {
                return stream_events(&mut stream, &turn, false).await;
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
        let turn = Turn { events: Mutex::new(Vec::new()), changed, permission: Mutex::new(None), dialog: Mutex::new(None), cancel: Mutex::new(None) };
        finish_turn(&turn, "agent exited before finishing the turn");
        finish_turn(&turn, "second");
        assert!(turn.is_done());
        let events = turn.events.lock().unwrap();
        assert_eq!(events.as_slice(), [
            json!({"type":"error","text":"agent exited before finishing the turn"}),
            json!({"type":"done"}),
        ]);
    }
}
