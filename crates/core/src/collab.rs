//! `/collab`: share a live interactive session through a relay so others can watch (view) or drive (control).
//!
//! GreenCode's own wire protocol (the relay is a plain HTTP/1.1 server; `ws(s)://` in `collab.relayUrl`
//! is used as `http(s)://`):
//! - `POST /v1/rooms` `{"name","mode"}` → `{"room","host","viewer"}`
//! - `PUT /v1/rooms/<room>/rows` (host token) `{"rows":[Value,...]}` appends transcript rows
//! - `GET /v1/rooms/<room>/rows?after=N&viewer=<token>` → `{"rows":[{"seq","event"}],"done":bool}`
//! - `POST /v1/rooms/<room>/prompts?viewer=<token>` (control rooms) `{"text"}`
//! - `GET /v1/rooms/<room>/prompts?after=N` (host token) → `{"prompts":[{"id","text"}]}`
//! - `DELETE /v1/rooms/<room>` (host token) closes the room
//! - `GET /rooms/<room>` is the browser viewer; `GET /v1/rooms` lists open rooms (no secrets).
use crate::{
    config::{collab::AutoStart, Config},
    localserver::{self, Request, Response},
    redact::Redactor,
};
use anyhow::{bail, Context, Result};
use serde::Deserialize;
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::net::TcpListener;

const MAX_ROWS: usize = 20_000;
const MAX_PROMPTS: usize = 200;
const MAX_PROMPT_TEXT: usize = 8_000;

/// `ws://host` / `wss://host` / `http://host` / `https://host` → an HTTP base without a trailing slash.
pub fn http_base(url: &str) -> Result<String> {
    let url = url.trim().trim_end_matches('/');
    let (scheme, rest) = url.split_once("://").context("URL needs a scheme (wss:// or https://)")?;
    let rest = rest.trim_end_matches('/');
    if rest.is_empty() || rest.contains([' ', '\t', '\n']) {
        bail!("invalid URL `{url}`");
    }
    let scheme = match scheme.to_ascii_lowercase().as_str() {
        "ws" | "http" => "http",
        "wss" | "https" => "https",
        other => bail!("unsupported scheme `{other}` (use wss://, ws://, https:// or http://)"),
    };
    Ok(format!("{scheme}://{rest}"))
}

/// The browser UI base: explicit `webUrl`, else `relayUrl`. An explicit `http://` URL must be loopback.
pub fn web_base(relay: Option<&str>, explicit: Option<&str>) -> Result<String> {
    let explicit = explicit.map(str::trim).filter(|u| !u.is_empty());
    if let Some(url) = explicit {
        let base = http_base(url)?;
        if base.starts_with("http://") && !is_loopback(&base) {
            bail!("an http:// web UI is only allowed on localhost; use https:// for `{url}`");
        }
        return Ok(base);
    }
    http_base(relay.context("collab.relay_url is not set")?)
}

fn is_loopback(base: &str) -> bool {
    let host = base.split_once("://").map_or(base, |(_, rest)| rest).split(['/', ':']).next().unwrap_or_default();
    matches!(host, "localhost" | "[::1]" | "::1") || host.parse::<std::net::IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

/// Name shown to participants: `collab.displayName`, else `$USER`.
pub fn display_name(cfg: &Config) -> String {
    cfg.collab_opts
        .collab
        .display_name
        .as_deref()
        .map(|s| s.chars().filter(|c| !c.is_control()).collect::<String>().trim().to_owned())
        .filter(|s| !s.is_empty())
        .or_else(|| std::env::var("USER").ok().or_else(|| std::env::var("USERNAME").ok()))
        .unwrap_or_else(|| "greencode".into())
}

#[derive(Clone, Debug)]
pub struct Room {
    pub room: String,
    pub host: String,
    pub viewer: String,
}

fn client() -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder().timeout(Duration::from_secs(30)).build()?)
}

pub fn viewer_link(web: &str, room: &Room) -> String {
    format!("{web}/rooms/{}?viewer={}", room.room, room.viewer)
}

async fn create_room(base: &str, name: &str, mode: AutoStart) -> Result<Room> {
    let mode = if mode == AutoStart::Control { "control" } else { "view" };
    let response = client()?
        .post(format!("{base}/v1/rooms"))
        .json(&json!({"name": name, "mode": mode}))
        .send()
        .await
        .with_context(|| format!("relay {base} is unreachable"))?;
    ensure_status(response.status())?;
    let value: Value = response.json().await?;
    let field = |name: &str| value[name].as_str().map(str::to_owned).with_context(|| format!("relay response has no {name}"));
    Ok(Room { room: field("room")?, host: field("host")?, viewer: field("viewer")? })
}

fn ensure_status(status: reqwest::StatusCode) -> Result<()> {
    if status.is_success() {
        return Ok(());
    }
    bail!("relay answered {status}")
}

async fn publish_rows(base: &str, room: &Room, rows: &[Value]) -> Result<()> {
    let response = client()?.put(format!("{base}/v1/rooms/{}/rows", room.room))
        .bearer_auth(&room.host)
        .json(&json!({"rows": rows}))
        .send().await?;
    ensure_status(response.status())
}

/// The transcript rows after `after`, and whether the host closed the room.
/// `base + path` with query parameters (reqwest's `query` feature is off in this workspace).
fn get_url(base: &str, path: &str, params: &[(&str, &str)]) -> Result<reqwest::Url> {
    let mut url = url::Url::parse(&format!("{base}{path}")).context("invalid URL")?;
    if !params.is_empty() {
        url.query_pairs_mut().extend_pairs(params);
    }
    Ok(url)
}

pub async fn rows(base: &str, room: &str, viewer: &str, after: usize) -> Result<(Vec<Value>, bool)> {
    let response = client()?
        .get(get_url(base, &format!("/v1/rooms/{room}/rows"), &[("after", &after.to_string()), ("viewer", viewer)])?)
        .send().await?;
    ensure_status(response.status())?;
    let value: Value = response.json().await?;
    Ok((value["rows"].as_array().cloned().unwrap_or_default(), value["done"].as_bool().unwrap_or(false)))
}

pub async fn submit_prompt(base: &str, room: &str, viewer: &str, text: &str) -> Result<()> {
    let response = client()?
        .post(get_url(base, &format!("/v1/rooms/{room}/prompts"), &[("viewer", viewer)])?)
        .json(&json!({"text": text}))
        .send().await?;
    ensure_status(response.status())
}

pub async fn prompts(base: &str, room: &Room, after: usize) -> Result<Vec<Value>> {
    let response = client()?
        .get(get_url(base, &format!("/v1/rooms/{}/prompts", room.room), &[("after", &after.to_string())])?)
        .bearer_auth(&room.host)
        .send().await?;
    ensure_status(response.status())?;
    let value: Value = response.json().await?;
    Ok(value["prompts"].as_array().cloned().unwrap_or_default())
}

async fn close_room(base: &str, room: &Room) {
    let _ = client()
        .ok()
        .map(|client| client.delete(format!("{base}/v1/rooms/{}", room.room)).bearer_auth(&room.host).send());
}

// ---- local registry: `greencode-cli collab list` ------------------------------------------------

pub fn registry_path() -> PathBuf {
    crate::home().join("collab-rooms.json")
}

#[derive(Deserialize, serde::Serialize, Clone, Debug, PartialEq)]
pub struct Entry {
    pub room: String,
    pub thread: String,
    pub name: String,
    pub mode: String,
    pub link: String,
    pub since: u64,
}

fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// Rooms this machine hosts, oldest first. Entries for rooms the relay no longer has stay until the host exits.
pub fn registry() -> Vec<Entry> {
    std::fs::read(registry_path()).ok().and_then(|bytes| serde_json::from_slice(&bytes).ok()).unwrap_or_default()
}

// shortcut: read-modify-write without a cross-process lock; two hosts starting in the same instant could drop an entry.
fn update_registry(change: impl FnOnce(&mut Vec<Entry>)) {
    let mut entries = registry();
    change(&mut entries);
    let _ = std::fs::create_dir_all(crate::home());
    let _ = std::fs::write(registry_path(), serde_json::to_vec(&entries).unwrap_or_default());
}

fn record(entry: Entry) {
    update_registry(|entries| {
        entries.retain(|e| e.room != entry.room && e.thread != entry.thread);
        entries.push(entry);
    });
}

fn unrecord(room: &str) {
    update_registry(|entries| entries.retain(|e| e.room != room));
}

/// A hosted room: publishes rows in order from one background task and yields viewer prompts.
pub struct Host {
    pub base: String,
    pub link: String,
    pub room: Room,
    rows: tokio::sync::mpsc::UnboundedSender<Value>,
}

impl Host {
    /// Opens a room for `thread` and publishes its saved transcript.
    pub async fn start(cfg: &Config, thread: &str, mode: AutoStart) -> Result<Host> {
        if mode == AutoStart::Off {
            bail!("collab.auto_start is off");
        }
        let base = http_base(cfg.collab_opts.collab.relay_url.as_deref().context("collab.relay_url is not set")?)?;
        let name = display_name(cfg);
        let room = create_room(&base, &name, mode).await?;
        let web = web_base(Some(&base), cfg.collab_opts.collab.web_url.as_deref())?;
        let link = viewer_link(&web, &room);
        let (rows, mut rx) = tokio::sync::mpsc::unbounded_channel::<Value>();
        let publisher = Publisher { base: base.clone(), room: room.clone() };
        tokio::spawn(async move {
            let mut batch = Vec::new();
            while let Some(row) = rx.recv().await {
                batch.push(row);
                while batch.len() < 100 {
                    match rx.try_recv() {
                        Ok(row) => batch.push(row),
                        Err(_) => break,
                    }
                }
                if publisher.publish(&batch).await.is_err() {
                    break;
                }
                batch.clear();
            }
        });
        // The transcript a viewer opening the link sees.
        if let Ok((meta, messages)) = crate::session::Session::load(thread) {
            let _ = rows.send(json!({"type": "history", "title": meta.title, "model": meta.model, "messages": messages}));
        }
        Ok(Host { base, link, room, rows })
    }

    /// Queue one event for viewers; order is kept by the publisher task.
    pub fn publish(&self, event: Value) {
        let _ = self.rows.send(event);
    }

    pub async fn next_prompts(&self, after: usize) -> Result<Vec<Value>> {
        prompts(&self.base, &self.room, after).await
    }

    pub async fn stop(self) {
        close_room(&self.base, &self.room).await;
    }
}

/// Hosts `thread` and publishes it to the local registry (`greencode-cli collab list`) until stopped.
pub async fn start_hosting(cfg: &Config, thread: &str, mode: AutoStart) -> Result<Host> {
    let host = Host::start(cfg, thread, mode).await?;
    let mode = if mode == AutoStart::Control { "control" } else { "view" };
    record(Entry { room: host.room.room.clone(), thread: thread.to_owned(), name: display_name(cfg), mode: mode.into(), link: host.link.clone(), since: now() });
    Ok(host)
}

/// Removes a stopped room from the registry and tells the relay.
pub async fn stop_hosting(host: Host) {
    unrecord(&host.room.room);
    host.stop().await;
}

struct Publisher {
    base: String,
    room: Room,
}

impl Publisher {
    async fn publish(&self, rows: &[Value]) -> Result<()> {
        publish_rows(&self.base, &self.room, rows).await
    }
}

// ---- relay server -------------------------------------------------------------------------------

struct RoomState {
    name: String,
    mode: AutoStart,
    host: String,
    viewer: String,
    rows: Vec<Value>,
    prompts: Vec<Value>,
    done: bool,
}

type Shared = Arc<Mutex<HashMap<String, RoomState>>>;

/// Serves the relay until the listener closes.
pub async fn serve(listener: TcpListener) -> Result<()> {
    let rooms: Shared = Arc::default();
    localserver::serve(listener, 4 * 1024 * 1024, move |request| {
        let rooms = rooms.clone();
        async move { handle(rooms, request).await }
    })
    .await
}

fn secret() -> Result<String> {
    crate::remote::random_hex()
}

fn parse<T: for<'de> Deserialize<'de>>(body: &[u8]) -> Result<T> {
    serde_json::from_slice(body).context("invalid JSON")
}

async fn handle(rooms: Shared, request: Request) -> Response {
    let (method, path) = (request.method.as_str(), request.path.as_str());
    if method == "GET" && path == "/health" {
        return Response::json(json!({"ok": true}));
    }
    if method == "POST" && path == "/v1/rooms" {
        #[derive(Deserialize)]
        struct Create { name: Option<String>, mode: Option<String> }
        let Ok(create) = parse::<Create>(&request.body) else { return Response::error("400 Bad Request", "invalid JSON") };
        let mode = match create.mode.as_deref().unwrap_or("view") {
            "view" => AutoStart::View,
            "control" => AutoStart::Control,
            _ => return Response::error("400 Bad Request", "mode must be view or control"),
        };
        let (Ok(room), Ok(host), Ok(viewer)) = (secret(), secret(), secret()) else {
            return Response::error("500 Internal Server Error", "entropy unavailable");
        };
        let room = room[..16].to_owned();
        rooms.lock().unwrap().insert(room.clone(), RoomState {
            name: create.name.unwrap_or_default().chars().take(80).collect(),
            mode,
            host,
            viewer,
            rows: Vec::new(),
            prompts: Vec::new(),
            done: false,
        });
        let state = rooms.lock().unwrap();
        let entry = &state[&room];
        let (host, viewer) = (entry.host.clone(), entry.viewer.clone());
        drop(state);
        return Response::json(json!({"room": room, "host": host, "viewer": viewer}));
    }
    if method == "GET" && path == "/v1/rooms" {
        let list: Vec<Value> = rooms.lock().unwrap().iter().map(|(room, state)| json!({
            "room": room, "name": state.name, "mode": if state.mode == AutoStart::Control { "control" } else { "view" },
        })).collect();
        return Response::json(json!(list));
    }
    let Some(rest) = path.strip_prefix("/v1/rooms/") else {
        if let Some(room) = path.strip_prefix("/rooms/") {
            let room = room.trim_end_matches('/');
            let exists = rooms.lock().unwrap().contains_key(room);
            return if exists { Response::html(VIEWER_HTML) } else { Response::error("404 Not Found", "room not found") };
        }
        return Response::error("404 Not Found", "not found");
    };
    let (room, action) = rest.split_once('/').unwrap_or((rest, ""));
    let mut state = rooms.lock().unwrap();
    let Some(room_state) = state.get_mut(room) else { return Response::error("404 Not Found", "room not found") };
    match (method, action) {
        ("PUT", "rows") => {
            if request.token() != Some(room_state.host.as_str()) { return Response::error("401 Unauthorized", "host token required"); }
            #[derive(Deserialize)]
            struct Rows { rows: Vec<Value> }
            let Ok(rows) = parse::<Rows>(&request.body) else { return Response::error("400 Bad Request", "invalid JSON") };
            for row in rows.rows {
                room_state.rows.push(row);
            }
            if room_state.rows.len() > MAX_ROWS {
                room_state.rows.drain(..room_state.rows.len() - MAX_ROWS);
            }
            Response::json(json!({"ok": true}))
        }
        ("GET", "rows") => {
            if request.query("viewer").filter(|v| *v == room_state.viewer).is_none() { return Response::error("401 Unauthorized", "viewer token required"); }
            let after = request.query("after").and_then(|v| v.parse().ok()).unwrap_or(0usize);
            let rows: Vec<Value> = room_state.rows.iter().enumerate().skip(after).map(|(i, row)| json!({"seq": i + 1, "event": row})).collect();
            Response::json(json!({"rows": rows, "done": room_state.done}))
        }
        ("POST", "prompts") => {
            if room_state.mode != AutoStart::Control { return Response::error("403 Forbidden", "this room is view-only"); }
            if request.query("viewer").filter(|v| *v == room_state.viewer).is_none() { return Response::error("401 Unauthorized", "viewer token required"); }
            #[derive(Deserialize)]
            struct Prompt { text: String }
            let Ok(prompt) = parse::<Prompt>(&request.body) else { return Response::error("400 Bad Request", "invalid JSON") };
            let text = prompt.text.trim();
            if text.is_empty() || text.len() > MAX_PROMPT_TEXT { return Response::error("400 Bad Request", "empty or too long"); }
            let id = room_state.prompts.len() + 1;
            room_state.prompts.push(json!({"id": id, "text": text}));
            if room_state.prompts.len() > MAX_PROMPTS { room_state.prompts.remove(0); }
            Response::json(json!({"id": id}))
        }
        ("GET", "prompts") => {
            if request.token() != Some(room_state.host.as_str()) { return Response::error("401 Unauthorized", "host token required"); }
            let after = request.query("after").and_then(|v| v.parse().ok()).unwrap_or(0usize);
            let prompts: Vec<Value> = room_state.prompts.iter().filter(|p| p["id"].as_u64().unwrap_or(0) > after as u64).cloned().collect();
            Response::json(json!({"prompts": prompts}))
        }
        ("DELETE", "") => {
            if request.token() != Some(room_state.host.as_str()) { return Response::error("401 Unauthorized", "host token required"); }
            room_state.done = true;
            Response::json(json!({"ok": true}))
        }
        _ => Response::error("404 Not Found", "not found"),
    }
}

const VIEWER_HTML: &str = r#"<!doctype html>
<html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width, initial-scale=1">
<title>GreenCode collab</title>
<style>body{font:15px/1.5 system-ui,sans-serif;margin:0;background:#101214;color:#e6e6e6}
header{padding:10px 16px;border-bottom:1px solid #333;display:flex;gap:12px;align-items:center}
main{padding:12px 16px;white-space:pre-wrap;font-family:ui-monospace,monospace;font-size:13px}
.row{padding:6px 0;border-bottom:1px solid #1e2226}.user{color:#8ab4f8}.assistant{color:#cfe8cf}
.tool{color:#f0c674}.error{color:#ff7b72}.think{color:#9aa0a6;font-style:italic}
form{display:flex;gap:8px;padding:10px 16px;border-top:1px solid #333;position:sticky;bottom:0;background:#101214}
input{flex:1;padding:8px;background:#1b1f23;border:1px solid #333;color:#eee;border-radius:6px}button{padding:8px 14px}
#state{color:#9aa0a6}</style></head><body>
<header><strong id="title">GreenCode collab</strong><span id="state">connecting…</span></header>
<main id="log"></main>
<form id="prompt" hidden><input id="text" placeholder="Ask or steer this session…" autocomplete="off"><button>Send</button></form>
<script>
const room = location.pathname.split("/").pop();
const viewer = new URLSearchParams(location.search).get("viewer") || "";
const log = document.getElementById("log"), state = document.getElementById("state"), form = document.getElementById("prompt");
let after = 0, mode = "view";
function add(text, cls) { const d = document.createElement("div"); d.className = "row " + (cls||""); d.textContent = text;
  log.appendChild(d); while (log.children.length > 3000) log.removeChild(log.firstChild); window.scrollTo(0, document.body.scrollHeight); }
function render(row) {
  const e = row.event || {};
  if (e.type === "history") {
    document.getElementById("title").textContent = e.title || ("GreenCode · " + (e.model || ""));
    for (const m of e.messages || []) for (const b of m.content || []) {
      if (b.text && b.type === "text") add((m.role === "user" ? "> " : "") + b.text, m.role);
      else if (b.type === "tool_use") add(b.name + " " + JSON.stringify(b.input), "tool");
      else if (b.type === "tool_result") add(typeof b.content === "string" ? b.content : JSON.stringify(b.content), "tool");
      else if (b.type === "thinking") add(b.thinking, "think");
    }
  } else if (e.type === "text") add(e.text, "assistant");
  else if (e.type === "thinking") add(e.text, "think");
  else if (e.type === "tool_start") add(e.name + " " + JSON.stringify(e.input), "tool");
  else if (e.type === "tool_end") add(e.output, "tool");
  else if (e.type === "error") add("error: " + e.text, "error");
  else if (e.type === "done") add("— turn finished —", "think");
}
async function poll() {
  try {
    const r = await fetch(`/v1/rooms/${room}/rows?after=${after}&viewer=${encodeURIComponent(viewer)}`);
    if (!r.ok) { state.textContent = r.status === 401 ? "invalid viewer link" : "room closed"; return; }
    const v = await r.json();
    for (const row of v.rows) { after = row.seq; render(row); }
    state.textContent = v.done ? "closed" : "live";
    if (v.done) return;
  } catch (_) { state.textContent = "reconnecting…"; }
  setTimeout(poll, 1000);
}
fetch(`/v1/rooms`).then(r => r.json()).then(list => {
  const found = list.find(x => x.room === room);
  if (found && found.mode === "control") form.hidden = false;
}).catch(() => {});
form.addEventListener("submit", async (e) => {
  e.preventDefault(); const input = document.getElementById("text"); const text = input.value.trim();
  if (!text) return; input.value = "";
  const r = await fetch(`/v1/rooms/${room}/prompts?viewer=${encodeURIComponent(viewer)}`, {method: "POST", headers: {"content-type": "application/json"}, body: JSON.stringify({text})});
  if (!r.ok) add("prompt not accepted: " + r.status, "error");
});
poll();
</script></body></html>"#;

/// Hosting state the TUI keeps: the room plus its prompt cursor.
pub struct Hosting {
    pub host: Host,
    pub prompts_seen: usize,
    pub redactor: Redactor,
}

/// Starts hosting `thread` if `collab.auto_start` asks for it; `None` when off.
pub async fn auto_host(cfg: &Config, thread: &str) -> Result<Option<Hosting>> {
    auto_host_mode(cfg, thread, cfg.collab_opts.collab.auto_start).await
}

/// The same with an explicit mode (`/collab start view|control`).
pub async fn auto_host_mode(cfg: &Config, thread: &str, mode: AutoStart) -> Result<Option<Hosting>> {
    if mode == AutoStart::Off {
        return Ok(None);
    }
    let host = start_hosting(cfg, thread, mode).await?;
    Ok(Some(Hosting { host, prompts_seen: 0, redactor: Redactor::from_config(cfg)? }))
}

/// Redacts one event for the relay.
pub fn redacted(redactor: &Redactor, event: Value) -> Value {
    let mut event = event;
    redactor.json(&mut event);
    event
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::collab::CollabCfg;

    fn cfg_with(url: &str, web: Option<&str>, mode: AutoStart) -> Config {
        let mut cfg = Config::default();
        cfg.collab_opts.collab = CollabCfg { relay_url: Some(url.into()), web_url: web.map(str::to_owned), display_name: Some("Ada Lovelace".into()), auto_start: mode };
        cfg
    }

    #[test]
    fn schemes_web_base_and_names() {
        assert_eq!(http_base("wss://relay.example.com/").unwrap(), "https://relay.example.com");
        assert_eq!(http_base("ws://127.0.0.1:9").unwrap(), "http://127.0.0.1:9");
        assert!(http_base("relay.example.com").is_err());
        assert!(http_base("ftp://x").is_err());
        assert!(web_base(None, Some("http://relay.example.com")).is_err());
        assert_eq!(web_base(None, Some("http://localhost:8080/ui")).unwrap(), "http://localhost:8080/ui");
        assert_eq!(web_base(Some("https://relay.example.com"), Some("/ui")).unwrap_err().to_string(), "URL needs a scheme (wss:// or https://)");
        let cfg = cfg_with("wss://relay.example.com", None, AutoStart::View);
        assert_eq!(display_name(&cfg), "Ada Lovelace");
    }

    #[tokio::test]
    async fn relay_rooms_scopes_and_control_prompts() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(serve(listener));
        let client = reqwest::Client::new();
        let create: Value = client.post(format!("{base}/v1/rooms")).json(&json!({"name": "Ada", "mode": "control"})).send().await.unwrap().json().await.unwrap();
        let room = create["room"].as_str().unwrap().to_string();
        let host = create["host"].as_str().unwrap().to_string();
        let viewer = create["viewer"].as_str().unwrap().to_string();

        // Host rows: host token only; viewer polls with its own token.
        assert_eq!(client.put(format!("{base}/v1/rooms/{room}/rows")).json(&json!({"rows":[{"type":"text","text":"hi"}]})).send().await.unwrap().status(), 401);
        let put = client.put(format!("{base}/v1/rooms/{room}/rows")).bearer_auth(&host).json(&json!({"rows":[{"type":"text","text":"hi"}]})).send().await.unwrap();
        assert_eq!(put.status(), 200);
        let page = client.get(format!("{base}/rooms/{room}")).send().await.unwrap();
        assert_eq!(page.status(), 200);
        assert!(page.text().await.unwrap().contains("GreenCode collab"));
        let rows: Value = client.get(format!("{base}/v1/rooms/{room}/rows?after=0&viewer={viewer}")).send().await.unwrap().json().await.unwrap();
        assert_eq!(rows["rows"][0]["event"]["text"], "hi");
        assert_eq!(rows["done"], false);

        // Control prompts reach the host; view-only rooms refuse them.
        let prompt = client.post(format!("{base}/v1/rooms/{room}/prompts?viewer={viewer}")).json(&json!({"text":"run the tests"})).send().await.unwrap();
        assert_eq!(prompt.status(), 200);
        let seen: Value = client.get(format!("{base}/v1/rooms/{room}/prompts?after=0")).bearer_auth(&host).send().await.unwrap().json().await.unwrap();
        assert_eq!(seen["prompts"][0]["text"], "run the tests");
        assert_eq!(client.post(format!("{base}/v1/rooms/{room}/prompts?viewer={viewer}")).json(&json!({"text":"x"})).bearer_auth(&viewer).send().await.unwrap().status(), 200);

        let view: Value = client.post(format!("{base}/v1/rooms")).json(&json!({"mode": "view"})).send().await.unwrap().json().await.unwrap();
        let view_room = view["room"].as_str().unwrap();
        let view_viewer = view["viewer"].as_str().unwrap();
        assert_eq!(client.post(format!("{base}/v1/rooms/{view_room}/prompts?viewer={view_viewer}")).json(&json!({"text":"x"})).send().await.unwrap().status(), 403);

        let closed = client.delete(format!("{base}/v1/rooms/{room}")).bearer_auth(&host).send().await.unwrap();
        assert_eq!(closed.status(), 200);
        let rows: Value = client.get(format!("{base}/v1/rooms/{room}/rows?after=1&viewer={viewer}")).send().await.unwrap().json().await.unwrap();
        assert_eq!(rows["done"], true);
    }

    #[tokio::test]
    async fn host_publishes_its_transcript_and_registry() {
        let home = std::env::temp_dir().join(format!("greencode-collab-home-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();
        // This test binary has other tests; only this one touches the collab registry path.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(serve(listener));
        let cfg = cfg_with(&format!("ws://{}", base.trim_start_matches("http://")), None, AutoStart::Control);
        let meta = crate::session::Meta::new(home.clone(), "test/model".into(), "native".into());
        crate::session::Session::create(&meta).unwrap().append(&crate::Message::user("hello relay")).unwrap();
        let host = Host::start(&cfg, &meta.id, AutoStart::Control).await.unwrap();
        let link = host.link.clone();
        assert!(link.contains("/rooms/") && link.contains("viewer="), "{link}");
        host.publish(json!({"type":"text","text":"streamed"}));
        tokio::time::sleep(Duration::from_millis(300)).await;
        let (published, _) = rows(&host.base, &host.room.room, &host.room.viewer, 0).await.unwrap();
        assert_eq!(published[0]["event"]["type"], "history");
        assert!(published.iter().any(|r| r["event"]["text"] == "streamed"), "{published:?}");
        assert!(rows(&host.base, &host.room.room, "wrong", 0).await.is_err(), "a viewer token is required");
        let _ = std::fs::remove_dir_all(&home);
        host.stop().await;
    }
}
