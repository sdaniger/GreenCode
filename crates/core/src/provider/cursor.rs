//! Cursor: the agent CLI's RunSSE/BidiAppend transport over HTTP/1, in pure
//! external-executor mode (OMP `providers/cursor.ts`, `wire/cursor.ts`).
//!
//! Every GreenCode tool is advertised to Cursor as an MCP tool. A tool call arrives
//! as an `McpArgs` exec frame: it becomes a `Block::ToolUse` and the frame is answered
//! with the handoff text, ending the turn; the result rides the next request as
//! history. History blobs are kept client-side and served over the KV frames.

mod proto;

use super::{limit, Provider, Reply, Request, Wire};
use crate::{Block, BoxFut, Message, Role, ToolDef, Usage};
use anyhow::{anyhow, bail, Context, Result};
use proto::{fields, frame, json_from_value, json_value, trailer_error, vbytes, vnum, vstr, Frames, Val, Wr};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap};
use std::time::Duration;

const RUN: &str = "/agent.v1.AgentService/RunSSE";
const APPEND: &str = "/aiserver.v1.BidiService/BidiAppend";
const MODELS: &str = "/agent.v1.AgentService/GetUsableModels";
/// The released Cursor CLI build this client mirrors; the service gates features on it.
const CLIENT_VERSION: &str = "cli-2026.09.02-c22c1a3";
/// External-executor mode runs no native Cursor tool, so only the MCP calls are allowed.
const ALLOWED_TOOLS: &str = "mcp_tool_call,get_mcp_tools_tool_call";
/// MCP server name every advertised tool belongs to.
const PROVIDER: &str = "greencode";
/// OMP's `cursor-external-tool-handoff.md`, verbatim: the frame must be answered for the
/// turn to continue, and the model is told the result arrives in the next request.
const HANDOFF: &str = "Tool call received and handed off to the external client for execution. Do not retry or call it again; end the turn. The result will be provided in the next request.";
const MAX_TOOL_ID: usize = 64;
const CONNECT_COMPRESSED: u8 = 0x01;
const CONNECT_END_STREAM: u8 = 0x02;

pub(crate) struct Cursor {
    http: reqwest::Client,
    base: String,
    key: String,
    wire: Wire,
}

impl Cursor {
    pub(crate) fn new(base: impl Into<String>, key: String, wire: Wire) -> Self {
        Self { http: reqwest::Client::new(), base: base.into().trim_end_matches('/').into(), key, wire }
    }

    fn post(&self, path: &str, content_type: &str, request_id: &str, streaming: bool) -> reqwest::RequestBuilder {
        let req = self
            .http
            .post(format!("{}{path}", self.base))
            .header("authorization", format!("Bearer {}", self.key))
            .header("content-type", content_type)
            .header("connect-protocol-version", "1")
            .header("x-cursor-client-type", "cli")
            .header("x-cursor-client-version", CLIENT_VERSION)
            .header("x-ghost-mode", "true")
            .header("x-request-id", request_id)
            // No gzip: compressed frames would need a decoder we do not carry.
            .header("accept-encoding", "identity");
        if streaming { req.header("x-cursor-streaming", "true").header("x-cursor-allowed-tools", ALLOWED_TOOLS) } else { req }
    }

    async fn write(&self, request_id: &str, seqno: u64, data: &[u8]) -> Result<()> {
        let mut rid = Wr::default();
        rid.str(1, request_id);
        let mut w = Wr::default();
        w.msg(2, &rid);
        w.varint(3, seqno);
        w.bytes(4, data);
        let resp = self
            .post(APPEND, "application/proto", request_id, false)
            .body(w.0)
            .send()
            .await
            .with_context(|| format!("POST {APPEND}"))?;
        let status = resp.status();
        if !status.is_success() {
            let headers = resp.headers().clone();
            let text = resp.text().await.unwrap_or_default();
            return Err(limit::http_error(status, &headers, &text));
        }
        Ok(())
    }

    /// Sends one `AgentClientMessage`, numbered in arrival order.
    async fn send(&self, request_id: &str, seq: &mut u64, message: Vec<u8>) -> Result<()> {
        self.write(request_id, *seq, &message).await?;
        *seq += 1;
        Ok(())
    }

    async fn run(&self, req: Request<'_>, on_text: &(dyn Fn(&str) + Send + Sync)) -> Result<Reply> {
        let mut store = Store::default();
        let (root, action) = history(&req, &mut store);
        let request_id = uuid();
        let tools: Vec<Vec<u8>> = req.tools.iter().map(mcp_tool).collect();
        let rules = rule(&req.system).into_iter().collect::<Vec<_>>();

        let mut body = Wr::default();
        body.str(1, &request_id);
        let mut resp = self
            .post(RUN, "application/connect+proto", &request_id, true)
            .body(frame(0, &body.0))
            .send()
            .await
            .with_context(|| format!("POST {RUN}"))?;
        let status = resp.status();
        if !status.is_success() {
            let headers = resp.headers().clone();
            let text = resp.text().await.unwrap_or_default();
            return Err(limit::http_error(status, &headers, &text));
        }

        let mut turn = Turn::default();
        let mut frames = Frames::default();
        let mut seq = 0u64;
        // RunSSE carries only the request id; the run itself is the first append.
        self.send(&request_id, &mut seq, run_request(&req, &root, &action)).await?;
        let mut first = self.wire.timeouts.first;
        let mut ended = false;
        loop {
            let budget = first.take().or(self.wire.timeouts.idle);
            let chunk = match budget {
                Some(budget) => tokio::time::timeout(budget, resp.chunk())
                    .await
                    .map_err(|_| anyhow!("モデルの応答が {} 秒止まりました", budget.as_secs()))??,
                None => resp.chunk().await?,
            };
            let Some(chunk) = chunk else { break };
            for (flags, payload) in frames.push(&chunk)? {
                if flags & CONNECT_COMPRESSED != 0 {
                    bail!("Cursor の圧縮された応答には対応していません");
                }
                if flags & CONNECT_END_STREAM != 0 {
                    if let Some(err) = trailer_error(&payload) {
                        return Err(err);
                    }
                    ended = true;
                    continue;
                }
                self.handle(&payload, &mut turn, &mut store, &request_id, &mut seq, &tools, &rules, on_text).await?;
            }
            if ended {
                break;
            }
        }
        if !turn.ended {
            bail!("Cursor の応答が turn_ended の前に終了しました");
        }
        Ok(turn.reply())
    }

    #[allow(clippy::too_many_arguments)]
    async fn handle(
        &self,
        payload: &[u8],
        turn: &mut Turn,
        store: &mut Store,
        request_id: &str,
        seq: &mut u64,
        tools: &[Vec<u8>],
        rules: &[Vec<u8>],
        on_text: &(dyn Fn(&str) + Send + Sync),
    ) -> Result<()> {
        for (no, val) in fields(payload)? {
            let Val::Bytes(body) = val else { continue };
            match no {
                1 => turn.update(body, on_text)?,
                2 => {
                    let msg = exec_reply(body, turn, tools, rules)?;
                    self.send(request_id, seq, msg).await?;
                }
                4 => {
                    let msg = kv_reply(body, store)?;
                    self.send(request_id, seq, msg).await?;
                }
                7 => self.send(request_id, seq, query_reply(body)).await?,
                _ => {}
            }
        }
        Ok(())
    }
}

impl Provider for Cursor {
    fn complete<'a>(&'a self, req: Request<'a>, on_text: &'a (dyn Fn(&str) + Send + Sync)) -> BoxFut<'a, Result<Reply>> {
        Box::pin(async move { self.run(req, on_text).await })
    }
}

/// This account's usable models (`GetUsableModels`), sorted and de-duplicated.
pub(crate) async fn models(base: &str, key: &str) -> Result<Vec<String>> {
    let req = reqwest::Client::new()
        .post(format!("{}{MODELS}", base.trim_end_matches('/')))
        .header("authorization", format!("Bearer {key}"))
        .header("content-type", "application/proto")
        .header("connect-protocol-version", "1")
        .header("x-cursor-client-type", "cli")
        .header("x-cursor-client-version", CLIENT_VERSION)
        .header("x-ghost-mode", "true")
        .header("x-request-id", uuid())
        .header("accept-encoding", "identity")
        .body(Vec::new())
        .timeout(Duration::from_secs(30))
        .send()
        .await
        .with_context(|| format!("POST {MODELS}"))?;
    let status = req.status();
    if !status.is_success() {
        let headers = req.headers().clone();
        let text = req.text().await.unwrap_or_default();
        return Err(limit::http_error(status, &headers, &text));
    }
    let bytes = req.bytes().await?;
    let mut out = std::collections::BTreeSet::new();
    for (no, val) in fields(&bytes)? {
        let Val::Bytes(model) = val else { continue };
        if no != 1 {
            continue;
        }
        let f = fields(model)?;
        let id = vstr(&f, 1).unwrap_or("").trim();
        if !id.is_empty() {
            out.insert(id.to_string());
        }
    }
    Ok(out.into_iter().collect())
}

// ---- history ----

/// History blobs by id, served to Cursor through the KV frames.
#[derive(Default)]
struct Store {
    blobs: HashMap<Vec<u8>, Vec<u8>>,
}

impl Store {
    fn put(&mut self, data: &[u8]) -> Vec<u8> {
        let id = Sha256::digest(data).to_vec();
        self.blobs.insert(id.clone(), data.to_vec());
        id
    }
}

fn history(req: &Request<'_>, store: &mut Store) -> (Vec<Vec<u8>>, Vec<u8>) {
    let messages = req.messages;
    let active = match messages.last() {
        Some(m) if m.role == Role::User && prompt_parts(m).is_some() => Some(m),
        _ => None,
    };
    // A trailing message of tool results is a resume: its results must stay in history.
    let end = if active.is_some() { messages.len() - 1 } else { messages.len() };

    let mut ids = Vec::new();
    let system = req.system.trim();
    let head = if system.is_empty() { "You are a helpful assistant." } else { system };
    ids.push(store.put(json!({"role": "system", "content": head}).to_string().as_bytes()));

    let mut entries = Vec::new();
    // Tool names by sanitized id, from the calls the history issued.
    let mut names: HashMap<String, String> = HashMap::new();
    for m in &messages[..end] {
        if m.role != Role::Assistant {
            continue;
        }
        for b in &m.content {
            if let Block::ToolUse { id, name, .. } = b {
                names.insert(tool_id(id), name.clone());
            }
        }
    }
    for m in &messages[..end] {
        match m.role {
            Role::User => {
                for b in &m.content {
                    if let Block::ToolResult { tool_use_id, content, is_error } = b {
                        let id = tool_id(tool_use_id);
                        let Some(name) = names.get(&id) else {
                            let prefix = if *is_error { "[Tool Error]" } else { "[Tool Result]" };
                            let text = format!("{prefix}\n{}", if content.is_empty() { "(empty result)" } else { content });
                            entries.push(json!({"role": "assistant", "content": [{"type": "text", "text": text}]}));
                            continue;
                        };
                        let mut part = json!({"type": "tool-result", "toolName": name, "toolCallId": id, "result": content});
                        if *is_error {
                            part["isError"] = json!(true);
                        }
                        entries.push(json!({"role": "tool", "id": id, "content": [part]}));
                    }
                }
                if let Some((text, images)) = prompt_parts(m) {
                    let mut content: Vec<Value> = Vec::new();
                    if !text.is_empty() {
                        content.push(json!({"type": "text", "text": text}));
                    }
                    for (mime, data) in images {
                        content.push(json!({"type": "image", "image": format!("data:{mime};base64,{data}"), "mediaType": mime}));
                    }
                    entries.push(json!({"role": "user", "content": content}));
                }
            }
            Role::Assistant => {
                // A step ends when text/thinking follows the calls that started it, as in OMP.
                let mut steps: Vec<Vec<Value>> = Vec::new();
                let mut current: Vec<Value> = Vec::new();
                let mut calls = false;
                let mut seen = std::collections::HashSet::new();
                for b in &m.content {
                    match b {
                        Block::ToolUse { id, name, input } => {
                            let id = tool_id(id);
                            if !seen.insert(id.clone()) {
                                continue;
                            }
                            current.push(json!({"type": "tool-call", "toolCallId": id, "toolName": name, "args": input}));
                            calls = true;
                        }
                        Block::Text { text } if !text.trim().is_empty() => {
                            if calls {
                                steps.push(std::mem::take(&mut current));
                                calls = false;
                            }
                            current.push(json!({"type": "text", "text": text}));
                        }
                        Block::Other(v) if v["type"] == "thinking" => {
                            if let Some(text) = v["thinking"].as_str().filter(|t| !t.is_empty()) {
                                if calls {
                                    steps.push(std::mem::take(&mut current));
                                    calls = false;
                                }
                                current.push(json!({"type": "reasoning", "text": text}));
                            }
                        }
                        _ => {}
                    }
                }
                if !current.is_empty() {
                    steps.push(current);
                }
                for step in steps {
                    entries.push(json!({"role": "assistant", "content": step}));
                }
            }
        }
    }
    // With history, Cursor replaces the system entries with its own prompt; the
    // client carries the `<rules>` entry (OMP builds the same one).
    if !entries.is_empty() && !system.is_empty() {
        ids.push(store.put(json!({"role": "user", "content": format!("<rules>\n{system}\n</rules>")}).to_string().as_bytes()));
    }
    for entry in entries {
        ids.push(store.put(entry.to_string().as_bytes()));
    }

    let action = match active {
        Some(m) => {
            let (text, images) = prompt_parts(m).unwrap_or_default();
            let mut um = Wr::default();
            um.str(1, &text);
            um.str(2, &uuid());
            if !images.is_empty() {
                let mut ctx = Wr::default();
                for (mime, data) in images {
                    let mut img = Wr::default();
                    img.str(2, &uuid());
                    img.str(7, &mime);
                    img.bytes(8, data.as_bytes());
                    ctx.bytes(1, &img.0);
                }
                um.bytes(3, &ctx.0);
            }
            let mut uma = Wr::default();
            uma.bytes(1, &um.0);
            let mut action = Wr::default();
            action.bytes(1, &uma.0);
            action.0
        }
        None => {
            let mut action = Wr::default();
            action.bytes(2, &[]);
            action.0
        }
    };
    (ids, action)
}

/// `(text, images)` of a user message when it carries a prompt.
fn prompt_parts(m: &Message) -> Option<(String, Vec<(String, String)>)> {
    let mut text = Vec::new();
    let mut images = Vec::new();
    for b in &m.content {
        match b {
            Block::Text { text: t } => text.push(t.as_str()),
            Block::Other(v) if v["type"] == "image" => {
                if let (Some(data), Some(mime)) = (v["source"]["data"].as_str(), v["source"]["media_type"].as_str()) {
                    if !data.is_empty() {
                        images.push((mime.to_string(), data.to_string()));
                    }
                }
            }
            _ => {}
        }
    }
    let text = text.join("\n");
    let text = text.trim();
    if text.is_empty() && images.is_empty() {
        return None;
    }
    Some((text.to_string(), images))
}

/// Cursor's tool-call id charset; the same id is used for the replayed result.
fn tool_id(id: &str) -> String {
    let id: String = id
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '_' })
        .take(MAX_TOOL_ID)
        .collect();
    if id.is_empty() { uuid() } else { id }
}

fn uuid() -> String {
    let mut b = [0u8; 16];
    let _ = getrandom::fill(&mut b);
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!("{}-{}-{}-{}-{}", &h[..8], &h[8..12], &h[12..16], &h[16..20], &h[20..32])
}

// ---- request encoding ----

fn run_request(req: &Request<'_>, root: &[Vec<u8>], action: &[u8]) -> Vec<u8> {
    let max_mode = req.model.ends_with("-max") || req.model.ends_with("-xhigh");
    let mut state = Wr::default();
    for id in root {
        state.bytes(1, id);
    }
    let mut details = Wr::default();
    details.str(1, req.model);
    details.str(3, req.model);
    details.str(4, req.model);
    if max_mode {
        details.bool(7, true);
    }
    let mut requested = Wr::default();
    requested.str(1, req.model);
    requested.bool(2, max_mode);
    let mut run = Wr::default();
    run.msg(1, &state);
    run.bytes(2, action);
    run.msg(3, &details);
    run.msg(9, &requested);
    run.str(5, &uuid());
    let mut client = Wr::default();
    client.msg(1, &run);
    client.0
}

fn mcp_tool(tool: &ToolDef) -> Vec<u8> {
    let mut w = Wr::default();
    w.str(1, &tool.name);
    w.str(2, &tool.description);
    w.bytes(3, &json_value(&tool.schema));
    w.str(4, PROVIDER);
    w.str(5, &tool.name);
    w.0
}

fn rule(system: &str) -> Option<Vec<u8>> {
    let system = system.trim();
    if system.is_empty() {
        return None;
    }
    let mut w = Wr::default();
    w.str(1, "/greencode/system-prompt/0.mdc");
    w.str(2, system);
    // CursorRuleType.global as an empty message.
    let mut kind = Wr::default();
    kind.bytes(1, &[]);
    w.msg(3, &kind);
    w.varint(4, 2); // CursorRuleSource.USER
    Some(w.0)
}

fn exec_client(id: u32, exec_id: &str, field: u32, body: &[u8]) -> Vec<u8> {
    let mut exec = Wr::default();
    exec.varint(1, u64::from(id));
    if !exec_id.is_empty() {
        exec.str(15, exec_id);
    }
    exec.bytes(field, body);
    let mut client = Wr::default();
    client.msg(2, &exec);
    client.0
}

fn exec_throw(id: u32, exec_id: &str, error: &str) -> Vec<u8> {
    let mut throw = Wr::default();
    throw.varint(1, u64::from(id));
    throw.str(2, error);
    if !exec_id.is_empty() {
        throw.str(4, "exec_error");
    }
    let mut control = Wr::default();
    control.msg(2, &throw);
    let mut client = Wr::default();
    client.msg(5, &control);
    client.0
}

fn kv_client(id: u32, field: u32, body: &[u8]) -> Vec<u8> {
    let mut kv = Wr::default();
    kv.varint(1, u64::from(id));
    kv.bytes(field, body);
    let mut client = Wr::default();
    client.msg(3, &kv);
    client.0
}

fn mcp_result(id: u32, exec_id: &str, text: &str) -> Vec<u8> {
    let mut item = Wr::default();
    let mut text_part = Wr::default();
    text_part.str(1, text);
    item.msg(1, &text_part);
    let mut success = Wr::default();
    success.msg(1, &item);
    let mut result = Wr::default();
    result.msg(1, &success);
    exec_client(id, exec_id, 11, &result.0)
}

fn mcp_rejected(id: u32, exec_id: &str, reason: &str) -> Vec<u8> {
    let mut rejected = Wr::default();
    rejected.str(1, reason);
    let mut result = Wr::default();
    result.msg(3, &rejected);
    exec_client(id, exec_id, 11, &result.0)
}

fn mcp_state(id: u32, exec_id: &str, tools: &[Vec<u8>], wanted: &[String]) -> Result<Vec<u8>> {
    let mut groups: BTreeMap<String, Vec<&[u8]>> = BTreeMap::new();
    for tool in tools {
        let f = fields(tool)?;
        let provider = vstr(&f, 4).unwrap_or(PROVIDER).trim().to_string();
        let provider = if provider.is_empty() { PROVIDER.to_string() } else { provider };
        if !wanted.is_empty() && !wanted.iter().any(|w| w == &provider) {
            continue;
        }
        groups.entry(provider).or_default().push(tool);
    }
    let mut success = Wr::default();
    for (name, list) in groups {
        let mut server = Wr::default();
        server.str(1, &name);
        server.str(2, &name);
        for tool in list {
            server.bytes(5, tool);
        }
        server.str(7, "connected");
        success.msg(1, &server);
    }
    let mut result = Wr::default();
    result.msg(1, &success);
    Ok(exec_client(id, exec_id, 36, &result.0))
}

/// One exec frame answered; the caller sends the returned message back through BidiAppend.
fn exec_reply(body: &[u8], turn: &mut Turn, tools: &[Vec<u8>], rules: &[Vec<u8>]) -> Result<Vec<u8>> {
    let f = fields(body)?;
    let id = vnum(&f, 1).unwrap_or(0) as u32;
    let exec_id = vstr(&f, 15).unwrap_or("").to_string();
    for (no, val) in &f {
        let Val::Bytes(args) = val else { continue };
        // exec_id and span_context are plain fields on the frame, not the oneof member.
        if *no == 15 || *no == 19 {
            continue;
        }
        match *no {
            10 => {
                let mut ctx = Wr::default();
                for rule in rules {
                    ctx.bytes(2, rule);
                }
                for tool in tools {
                    ctx.bytes(7, tool);
                }
                let mut success = Wr::default();
                success.msg(1, &ctx);
                let mut result = Wr::default();
                result.msg(1, &success);
                return Ok(exec_client(id, &exec_id, 10, &result.0));
            }
            11 => {
                let a = fields(args)?;
                let approval = vnum(&a, 7).unwrap_or(0) != 0;
                let name = vstr(&a, 5).filter(|s| !s.is_empty()).or_else(|| vstr(&a, 1)).unwrap_or("").to_string();
                let call_id = vstr(&a, 3).unwrap_or("").to_string();
                if approval {
                    return Ok(mcp_rejected(id, &exec_id, &format!("ツール \"{name}\" は確認なしでは実行できません")));
                }
                let mut input = serde_json::Map::new();
                for (n, v) in &a {
                    let (Val::Bytes(entry), 2) = (v, *n) else { continue };
                    let e = fields(entry)?;
                    let key = vstr(&e, 1).unwrap_or("");
                    if let Some(value) = vbytes(&e, 2) {
                        if let Ok(value) = json_from_value(value) {
                            input.insert(key.into(), value);
                        }
                    }
                }
                let call_id = tool_id(&call_id);
                if !turn.calls.iter().any(|(existing, _, _)| existing == &call_id) {
                    turn.calls.push((call_id, name, Value::Object(input)));
                }
                return Ok(mcp_result(id, &exec_id, HANDOFF));
            }
            36 => {
                let wanted: Vec<String> = fields(args)?
                    .into_iter()
                    .filter_map(|(n, v)| match v {
                        Val::Bytes(b) if n == 1 => Some(String::from_utf8_lossy(b).into_owned()),
                        _ => None,
                    })
                    .collect();
                return mcp_state(id, &exec_id, tools, &wanted);
            }
            2..=54 => {
                return Ok(exec_throw(id, &exec_id, &format!("Cursor のネイティブツール（フィールド {no}）には対応していません")));
            }
            _ => {}
        }
    }
    Ok(exec_throw(id, &exec_id, "未知の exec メッセージです"))
}

fn kv_reply(body: &[u8], store: &mut Store) -> Result<Vec<u8>> {
    let f = fields(body)?;
    let id = vnum(&f, 1).unwrap_or(0) as u32;
    for (no, val) in &f {
        let Val::Bytes(args) = val else { continue };
        match *no {
            2 => {
                let args = fields(args)?;
                let blob_id = vbytes(&args, 1).unwrap_or(&[]);
                let mut result = Wr::default();
                if let Some(data) = store.blobs.get(blob_id) {
                    result.bytes(1, data);
                }
                return Ok(kv_client(id, 2, &result.0));
            }
            3 => {
                let args = fields(args)?;
                if let (Some(blob_id), Some(data)) = (vbytes(&args, 1), vbytes(&args, 2)) {
                    store.blobs.insert(blob_id.to_vec(), data.to_vec());
                }
                return Ok(kv_client(id, 3, &[]));
            }
            _ => {}
        }
    }
    Ok(kv_client(id, 3, &[]))
}

/// Cursor's interactive permission gates: hosted web tools are approved, questions and
/// mode switches are refused with a reason. VM setup is left unanswered, as in OMP.
fn query_reply(body: &[u8]) -> Vec<u8> {
    let f = match fields(body) {
        Ok(f) => f,
        Err(_) => return Vec::new(),
    };
    let id = vnum(&f, 1).unwrap_or(0) as u32;
    let Some((no, _)) = f.iter().find(|(n, v)| *n >= 2 && matches!(v, Val::Bytes(_))) else {
        return Vec::new();
    };
    let no = *no;
    let approved = || {
        let mut inner = Wr::default();
        inner.bytes(1, &[]);
        inner.0
    };
    let rejected = |reason: &str| {
        let mut inner = Wr::default();
        inner.str(1, reason);
        inner.0
    };
    let payload = match no {
        2 | 5 | 6 | 9 => approved(),
        3 => rejected("対話的な質問には対応していません"),
        4 => rejected("モードの切り替えには対応していません"),
        7 => rejected("プランファイルには対応していません"),
        8 => return Vec::new(),
        _ => approved(),
    };
    let mut response = Wr::default();
    response.varint(1, u64::from(id));
    response.bytes(no, &payload);
    let mut client = Wr::default();
    client.msg(6, &response);
    client.0
}

// ---- updates ----

#[derive(Default)]
struct Turn {
    thinking: String,
    text: String,
    /// (id, tool name, args), in arrival order.
    calls: Vec<(String, String, Value)>,
    usage: Usage,
    ended: bool,
}

impl Turn {
    fn update(&mut self, body: &[u8], on_text: &(dyn Fn(&str) + Send + Sync)) -> Result<()> {
        for (no, val) in fields(body)? {
            let Val::Bytes(inner) = val else { continue };
            match no {
                1 => {
                    if let Some(text) = vstr(&fields(inner)?, 1).filter(|t| !t.is_empty()) {
                        self.text.push_str(text);
                        on_text(text);
                    }
                }
                4 => self.thinking.push_str(vstr(&fields(inner)?, 1).unwrap_or("")),
                14 => {
                    let f = fields(inner)?;
                    let get = |n| vnum(&f, n).unwrap_or(0);
                    let (input, output, read, write) = (get(1), get(2), get(3), get(4));
                    if input > 0 || output > 0 || read > 0 || write > 0 {
                        self.usage = Usage {
                            input: input.saturating_sub(read + write),
                            output,
                            cache_read: read,
                            cache_write: write,
                        };
                    }
                    self.ended = true;
                }
                _ => {}
            }
        }
        Ok(())
    }

    fn reply(self) -> Reply {
        let mut blocks = Vec::new();
        if !self.thinking.is_empty() {
            blocks.push(Block::Other(json!({"type": "thinking", "thinking": self.thinking})));
        }
        if !self.text.is_empty() {
            blocks.push(Block::Text { text: self.text });
        }
        for (id, name, input) in self.calls {
            blocks.push(Block::ToolUse { id, name, input });
        }
        let stop = if blocks.iter().any(|b| matches!(b, Block::ToolUse { .. })) { "tool_use" } else { "end_turn" };
        Reply { blocks, usage: self.usage, stop: stop.into() }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::localserver::{serve, Request as HttpRequest, Response};
    use std::sync::{Arc, Mutex};

    fn server_message(field: u32, body: &[u8]) -> Vec<u8> {
        let mut w = Wr::default();
        w.bytes(field, body);
        frame(0, &w.0)
    }

    fn exec(id: u32, field: u32, args: &[u8]) -> Vec<u8> {
        let mut w = Wr::default();
        w.varint(1, u64::from(id));
        w.str(15, &format!("exec-{id}"));
        w.bytes(field, args);
        w.0
    }

    fn text_update(field: u32, text: &str) -> Vec<u8> {
        let mut inner = Wr::default();
        inner.str(1, text);
        let mut update = Wr::default();
        update.bytes(field, &inner.0);
        server_message(1, &update.0)
    }

    /// Serves the scripted RunSSE body and records every request.
    async fn mock(stream_body: Vec<u8>) -> (String, Arc<Mutex<Vec<HttpRequest>>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = seen.clone();
        tokio::spawn(async move {
            let _ = serve(listener, 1 << 24, move |req: HttpRequest| {
                let log = log.clone();
                let body = stream_body.clone();
                async move {
                    let run = req.path.contains("RunSSE");
                    log.lock().unwrap().push(req);
                    if run {
                        Response::new("200 OK", "application/connect+proto", body)
                    } else {
                        Response::new("200 OK", "application/proto", Vec::new())
                    }
                }
            })
            .await;
        });
        (origin, seen)
    }

    #[tokio::test]
    async fn streamed_text_and_mcp_calls_become_blocks_and_are_answered() {
        let mut args = Wr::default();
        let mut entry = Wr::default();
        entry.str(1, "path");
        entry.bytes(2, &json_value(&json!("/tmp/x")));
        args.str(1, "read");
        args.msg(2, &entry);
        args.str(3, "call|1");
        args.str(4, PROVIDER);
        args.str(5, "read");
        let mut ended = Wr::default();
        ended.varint(1, 100);
        ended.varint(2, 20);
        ended.varint(3, 30);
        ended.varint(4, 10);
        let mut turn_ended = Wr::default();
        turn_ended.bytes(14, &ended.0);
        let stream = [
            text_update(1, "Hello "),
            text_update(4, "hmm"),
            server_message(2, &exec(1, 10, &[])),
            server_message(2, &exec(2, 11, &args.0)),
            server_message(1, &turn_ended.0),
            frame(CONNECT_END_STREAM, &[]),
        ]
        .concat();
        let (origin, seen) = mock(stream).await;
        let cursor = Cursor::new(origin, "tok".into(), Wire::default());
        let messages = [Message::user("hi")];
        let tools = [ToolDef { name: "read".into(), description: "read a file".into(), schema: json!({"type": "object"}) }];
        let streamed = Mutex::new(String::new());
        let on_text = |t: &str| streamed.lock().unwrap().push_str(t);
        let req = Request { model: "claude-opus-5-high", system: "sys", messages: &messages, tools: &tools, max_tokens: 1000, live: None };
        let reply = cursor.complete(req, &on_text).await.unwrap();
        assert_eq!(streamed.into_inner().unwrap(), "Hello ");
        assert_eq!(reply.stop, "tool_use");
        assert_eq!((reply.usage.input, reply.usage.output, reply.usage.cache_read, reply.usage.cache_write), (60, 20, 30, 10));
        assert_eq!(reply.blocks.len(), 3, "{:?}", reply.blocks);
        assert!(matches!(&reply.blocks[0], Block::Other(v) if v["thinking"] == "hmm"));
        assert!(matches!(&reply.blocks[1], Block::Text { text } if text == "Hello "));
        match &reply.blocks[2] {
            Block::ToolUse { id, name, input } => {
                assert_eq!((id.as_str(), name.as_str()), ("call_1", "read"));
                assert_eq!(input["path"], "/tmp/x");
            }
            other => panic!("expected a tool call, got {other:?}"),
        }

        let seen = seen.lock().unwrap();
        let run = &seen[0];
        assert!(run.path.contains("RunSSE"));
        let lower = run.headers.iter().map(|(k, v)| format!("{}: {}", k.to_ascii_lowercase(), v)).collect::<Vec<_>>().join("\n");
        assert!(lower.contains("x-cursor-allowed-tools: mcp_tool_call"), "{lower}");
        assert!(lower.contains("x-cursor-streaming: true"), "{lower}");
        let request = fields(&run.body[5..]).unwrap();
        assert_eq!(run.body[0], 0);
        assert!(!vstr(&request, 1).unwrap_or("").is_empty(), "a BidiRequestId");

        // Every reply arrived as a BidiAppend with a fresh sequence number.
        let appends: Vec<_> = seen.iter().filter(|r| r.path.contains("BidiAppend")).collect();
        assert_eq!(appends.len(), 3, "request_context, mcp result and the KV-less turn");
        let mut seqnos = Vec::new();
        let mut saw_tools = false;
        let mut saw_handoff = false;
        for append in appends {
            let f = fields(&append.body).unwrap();
            seqnos.push(vnum(&f, 3).unwrap());
            let data = vbytes(&f, 4).unwrap();
            let agent = fields(data).unwrap();
            let Some(message) = vbytes(&agent, 2) else { continue };
            let exec = fields(message).unwrap();
            // The frame's own exec id is echoed so a streamed exec can be matched.
            assert!(vstr(&exec, 15).unwrap_or("").starts_with("exec-"), "exec id missing");
            if let Some(result) = vbytes(&exec, 10) {
                let result = fields(result).unwrap();
                let success = fields(vbytes(&result, 1).unwrap()).unwrap();
                let ctx = fields(vbytes(&success, 1).unwrap()).unwrap();
                let tools: Vec<&[u8]> = ctx.iter().filter_map(|(n, v)| match v {
                    Val::Bytes(b) if *n == 7 => Some(*b),
                    _ => None,
                }).collect();
                assert_eq!(tools.len(), 1);
                assert_eq!(vstr(&fields(tools[0]).unwrap(), 1), Some("read"));
                assert_eq!(vstr(&fields(tools[0]).unwrap(), 4), Some(PROVIDER));
                saw_tools = true;
            }
            if let Some(result) = vbytes(&exec, 11) {
                let result = fields(result).unwrap();
                let success = fields(vbytes(&result, 1).unwrap()).unwrap();
                let item = fields(vbytes(&success, 1).unwrap()).unwrap();
                let text = vstr(&fields(vbytes(&item, 1).unwrap()).unwrap(), 1).unwrap();
                assert_eq!(text, HANDOFF);
                saw_handoff = true;
            }
        }
        assert_eq!(seqnos, [0, 1, 2]);
        assert!(saw_tools && saw_handoff);
    }

    #[tokio::test]
    async fn end_stream_errors_are_reported() {
        let trailer = br#"{"error":{"code":"resource_exhausted","message":"no"}}"#;
        let (origin, _) = mock(frame(CONNECT_END_STREAM, trailer)).await;
        let cursor = Cursor::new(origin, "tok".into(), Wire::default());
        let messages = [Message::user("hi")];
        let req = Request { model: "claude-opus-5-high", system: "", messages: &messages, tools: &[], max_tokens: 10, live: None };
        let err = cursor.complete(req, &|_| {}).await.unwrap_err().to_string();
        assert!(err.contains("resource_exhausted") && err.contains("no"), "{err}");
    }

    #[tokio::test]
    async fn history_round_trips_calls_and_results_through_blobs() {
        let mut store = Store::default();
        let messages = [
            Message::user("first"),
            Message {
                role: Role::Assistant,
                content: vec![
                    Block::Text { text: "looking".into() },
                    Block::ToolUse { id: "call|1".into(), name: "read".into(), input: json!({"path": "a"}) },
                ],
            },
            Message { role: Role::User, content: vec![Block::ToolResult { tool_use_id: "call|1".into(), content: "ok".into(), is_error: false }] },
            Message::user("second"),
        ];
        let req = Request { model: "m", system: "rules", messages: &messages, tools: &[], max_tokens: 10, live: None };
        let (root, action) = history(&req, &mut store);
        // system, rules, user, assistant, tool — the active prompt rides in the action.
        assert_eq!(root.len(), 5);
        let decoded: Vec<Value> = root.iter().map(|id| serde_json::from_slice(&store.blobs[id]).unwrap()).collect();
        assert_eq!(decoded[0]["role"], "system");
        assert_eq!(decoded[1]["role"], "user");
        assert!(decoded[1]["content"].as_str().unwrap().starts_with("<rules>"));
        assert_eq!(decoded[2]["content"][0]["text"], "first");
        assert_eq!(decoded[3]["content"][1]["type"], "tool-call");
        assert_eq!(decoded[3]["content"][1]["toolCallId"], "call_1");
        assert_eq!(decoded[4]["role"], "tool");
        assert_eq!(decoded[4]["content"][0]["toolName"], "read");
        let f = fields(&action).unwrap();
        assert!(vbytes(&f, 1).is_some(), "a user_message_action");
    }
}
