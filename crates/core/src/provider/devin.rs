//! Devin (Codeium Cascade): Connect-protocol protobuf RPCs over HTTP.
//!
//! OMP (`providers/devin.ts`, `wire/devin.ts`, `discovery/devin-proto.ts`) talks to
//! Cascade with generated protobuf codecs; the workspace has no protobuf runtime, so
//! the handful of messages used here are hand-encoded below.

use super::{limit, Provider, Reply, Request, Wire};
use crate::{config::provider::Sampling, Block, BoxFut, Message, Role, ToolDef, Usage};
use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::time::Duration;

/// Cascade's Connect host; presets point here too.
pub(crate) const BASE: &str = "https://server.codeium.com";
const CHAT: &str = "/exa.api_server_pb.ApiServerService/GetChatMessage";
const AUTH: &str = "/exa.auth_pb.AuthService/GetUserJwt";
const CLI_CONFIGS: &str = "/exa.api_server_pb.ApiServerService/GetCliModelConfigs";
const STATUS: &str = "/exa.seat_management_pb.SeatManagementService/GetUserStatus";
const EXCHANGE: &str = "/exa.seat_management_pb.SeatManagementService/ExchangePKCEAuthorizationCode";

const CONNECT_COMPRESSED: u8 = 0x02 >> 1;
const CONNECT_END_STREAM: u8 = 0x02;
const MAX_FRAME: usize = 16 * 1024 * 1024;
/// Cascade rejects `temperature: 0` with an opaque `invalid_argument`.
const TEMPERATURE_FLOOR: f64 = 0.0001;
/// OpenAI-routed models reject replay ids past this cap or outside `[A-Za-z0-9_-]`.
const TOOL_ID_MAX: usize = 40;

pub struct Devin {
    pub http: reqwest::Client,
    pub base: String,
    /// The stored seat key; rides in `Metadata.apiKey`, not a bearer header.
    pub key: String,
    pub wire: Wire,
    /// Cascade thread id, reused so the server keeps one conversation. Per client instance,
    /// like OMP's per-session `conversationId`.
    cascade: String,
}

impl Devin {
    pub fn new(base: String, key: String, wire: Wire) -> Self {
        Self { http: reqwest::Client::new(), base, key, wire, cascade: uuid_v4() }
    }

    async fn user_jwt(&self) -> Result<(String, Option<String>)> {
        let mut req = Wr::default();
        if !self.key.is_empty() {
            req.bytes(1, &metadata(&self.key, "", false));
        }
        let bytes = unary_post(
            &self.http,
            &format!("{}{AUTH}", self.base),
            &req.0,
            "application/proto",
            Duration::from_secs(30),
        )
        .await
        .context("Devin の認証に失敗しました")?;
        let f = fields(&bytes)?;
        let jwt = vstr(&f, 1).unwrap_or("").trim().to_string();
        if jwt.is_empty() {
            bail!("Devin の認証情報を取得できませんでした。`/login devin` でサインインし直してください");
        }
        let custom = vstr(&f, 2).unwrap_or("").trim().trim_end_matches('/').to_string();
        Ok((jwt, (!custom.is_empty()).then_some(custom)))
    }

    async fn run(&self, req: Request<'_>, on_text: &(dyn Fn(&str) + Send + Sync)) -> Result<Reply> {
        let (jwt, custom) = self.user_jwt().await?;
        let base = custom.unwrap_or_else(|| self.base.clone());
        let md = metadata(&self.key, &jwt, false);
        let body = chat_request(&req, &self.cascade, &md, &self.wire);
        let mut resp = self
            .http
            .post(format!("{base}{CHAT}"))
            .header("content-type", "application/connect+proto")
            .header("connect-protocol-version", "1")
            .header("accept-encoding", "identity")
            .header("user-agent", "connect-go/1.18.1 (go1.26.3)")
            // No `connect-accept-encoding`: compressed frames would need a gzip decoder we do not carry.
            .body(connect_frame(&body, 0))
            .send()
            .await
            .with_context(|| format!("POST {base}{CHAT}"))?;
        let status = resp.status();
        if !status.is_success() {
            let headers = resp.headers().clone();
            let text = resp.text().await.unwrap_or_default();
            return Err(limit::http_error(status, &headers, &text));
        }
        let mut acc = Acc::default();
        let mut frames = Frames::default();
        let mut first = self.wire.timeouts.first;
        loop {
            let budget = first.take().or(self.wire.timeouts.idle);
            let chunk = match budget {
                Some(budget) => tokio::time::timeout(budget, resp.chunk())
                    .await
                    .map_err(|_| anyhow!("モデルの応答が {} 秒止まりました", budget.as_secs()))??,
                None => resp.chunk().await?,
            };
            let Some(chunk) = chunk else { break };
            let mut ended = false;
            for (flags, payload) in frames.push(&chunk)? {
                if flags & CONNECT_COMPRESSED != 0 {
                    bail!("Devin の圧縮された応答には対応していません");
                }
                if flags & CONNECT_END_STREAM != 0 {
                    if let Some(err) = trailer_error(&payload) {
                        return Err(err);
                    }
                    ended = true;
                    continue;
                }
                if let Some(text) = acc.feed(&payload)? {
                    on_text(&text);
                }
            }
            if ended {
                break;
            }
        }
        Ok(acc.finish())
    }
}

impl Provider for Devin {
    fn complete<'a>(
        &'a self,
        req: Request<'a>,
        on_text: &'a (dyn Fn(&str) + Send + Sync),
    ) -> BoxFut<'a, Result<Reply>> {
        Box::pin(async move { self.run(req, on_text).await })
    }
}

// ---- request building ----

fn chat_request(req: &Request<'_>, cascade: &str, md: &[u8], wire: &Wire) -> Vec<u8> {
    let mut w = Wr::default();
    w.bytes(1, md);
    w.str(2, req.system);
    for p in prompts(req.messages, cascade) {
        w.bytes(3, &p);
    }
    w.varint(7, 5); // ChatMessageRequestType::CASCADE
    let temperature = Sampling::value(wire.sampling.temperature).unwrap_or(1.0).max(TEMPERATURE_FLOOR);
    // Devin's CLI sends topP as a float32 widened to double; round the same way.
    let top_p = Sampling::value(wire.sampling.top_p).unwrap_or(0.95) as f32 as f64;
    let mut cfg = Wr::default();
    cfg.varint(1, 1); // numCompletions
    cfg.varint(2, u64::from(req.max_tokens.max(1)));
    cfg.varint(3, 400); // maxNewlines, as the native client sends
    cfg.double(5, temperature);
    cfg.varint(7, 40); // topK
    cfg.double(8, top_p);
    w.bytes(8, &cfg.0);
    for tool in req.tools {
        w.bytes(10, &tool_def(tool));
    }
    // The catalog's per-model `supportsParallelToolCalls` is not kept by this workspace; OMP's
    // compat default is "not supported", so requests stay sequential.
    w.bool(11, true);
    let mut choice = Wr::default();
    choice.str(1, "auto");
    w.bytes(12, &choice.0);
    let mut cache = Wr::default();
    cache.varint(1, 1); // CacheControlType::EPHEMERAL
    w.bytes(13, &cache.0);
    w.str(16, cascade);
    w.varint(20, 1); // ConversationalPlannerMode::DEFAULT
    w.str(21, req.model);
    w.str(22, &uuid_v4()); // executionId
    w.0
}

fn tool_def(tool: &ToolDef) -> Vec<u8> {
    let mut w = Wr::default();
    w.str(1, &tool.name);
    w.str(2, &tool.description);
    w.str(3, &serde_json::to_string(&tool.schema).unwrap_or_else(|_| "{}".into()));
    w.0
}

fn chat_tool_call(id: &str, name: &str, input: &Value) -> Vec<u8> {
    let mut w = Wr::default();
    w.str(1, id);
    w.str(2, name);
    w.str(3, &input.to_string());
    w.0
}

fn prompt(message_id: &str, source: u64, text: &str) -> Wr {
    let mut w = Wr::default();
    w.str(1, message_id);
    w.varint(2, source);
    w.str(3, text);
    w
}

fn image(v: &Value) -> Option<Vec<u8>> {
    let data = v["source"]["data"].as_str().filter(|d| !d.is_empty())?;
    let mime = v["source"]["media_type"].as_str().unwrap_or("image/jpeg");
    let mut w = Wr::default();
    w.str(1, data);
    w.str(2, mime);
    Some(w.0)
}

/// Maps history onto Cascade's USER / SYSTEM / TOOL channel rows (`buildChatMessagePrompts`).
fn prompts(messages: &[Message], cascade: &str) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    for (index, m) in messages.iter().enumerate() {
        let seed = |role: &str| format!("{cascade}\0{index}\0{role}");
        match m.role {
            Role::User => {
                let (mut text, mut images, mut results) = (String::new(), Vec::new(), Vec::new());
                for b in &m.content {
                    match b {
                        Block::Text { text: t } => text.push_str(t),
                        Block::ToolResult { tool_use_id, content, is_error } => {
                            results.push((tool_id(tool_use_id), content.clone(), *is_error))
                        }
                        Block::Other(v) if v["type"] == "image" => images.extend(image(v)),
                        _ => {}
                    }
                }
                for (id, content, is_error) in results {
                    let mut w = prompt(
                        &deterministic_uuid(&format!("{cascade}\0{index}\0tool\0{id}")),
                        4, // TOOL
                        &content,
                    );
                    w.str(7, &id);
                    w.bool(9, is_error);
                    out.push(w.0);
                }
                if !text.is_empty() || !images.is_empty() {
                    let mut w = prompt(&deterministic_uuid(&seed("user")), 1, &text);
                    for img in images {
                        w.bytes(10, &img);
                    }
                    out.push(w.0);
                }
            }
            Role::Assistant => {
                let (mut text, mut thinking, mut signature) = (String::new(), String::new(), String::new());
                let mut calls = Vec::new();
                for b in &m.content {
                    match b {
                        Block::Text { text: t } => text.push_str(t),
                        Block::ToolUse { id, name, input } => {
                            calls.push(chat_tool_call(&tool_id(id), name, input))
                        }
                        Block::Other(v) if v["type"] == "thinking" => {
                            thinking.push_str(v["thinking"].as_str().unwrap_or(""));
                            if signature.is_empty() {
                                signature = v["signature"].as_str().unwrap_or("").to_string();
                            }
                        }
                        _ => {}
                    }
                }
                if text.is_empty() && thinking.is_empty() && signature.is_empty() && calls.is_empty() {
                    continue;
                }
                let mut w = prompt(&deterministic_uuid(&seed("assistant")), 2, &text);
                for call in calls {
                    w.bytes(6, &call);
                }
                w.str(11, &thinking);
                w.str(12, &signature);
                out.push(w.0);
            }
        }
    }
    out
}

/// `[A-Za-z0-9_-]{0,40}`: Cascade forwards ids to OpenAI-routed backends.
fn tool_id(id: &str) -> String {
    id.chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '_' })
        .take(TOOL_ID_MAX)
        .collect()
}

/// A UUID-shaped id from a seed (not RFC 4122 nibbles), stable across turns.
fn deterministic_uuid(seed: &str) -> String {
    let hex = format!("{:x}", Sha256::digest(seed.as_bytes()));
    format!("{}-{}-{}-{}-{}", &hex[..8], &hex[8..12], &hex[12..16], &hex[16..20], &hex[20..32])
}

pub(crate) fn uuid_v4() -> String {
    let mut b = [0u8; 16];
    let _ = getrandom::fill(&mut b);
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!("{}-{}-{}-{}-{}", &h[..8], &h[8..12], &h[12..16], &h[16..20], &h[20..32])
}

// ---- streaming ----

fn connect_frame(payload: &[u8], flags: u8) -> Vec<u8> {
    let mut out = Vec::with_capacity(5 + payload.len());
    out.push(flags);
    out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    out.extend_from_slice(payload);
    out
}

/// Incremental Connect-protocol frame decoder (1 flag byte + u32 length + payload).
#[derive(Default)]
struct Frames {
    buf: Vec<u8>,
}

impl Frames {
    fn push(&mut self, chunk: &[u8]) -> Result<Vec<(u8, Vec<u8>)>> {
        self.buf.extend_from_slice(chunk);
        let mut out = Vec::new();
        let mut off = 0;
        while self.buf.len() - off >= 5 {
            let flags = self.buf[off];
            let len = u32::from_be_bytes([self.buf[off + 1], self.buf[off + 2], self.buf[off + 3], self.buf[off + 4]]) as usize;
            if len > MAX_FRAME {
                bail!("Devin の応答フレームが大きすぎます（{len} バイト）");
            }
            if self.buf.len() - off < 5 + len {
                break;
            }
            out.push((flags, self.buf[off + 5..off + 5 + len].to_vec()));
            off += 5 + len;
        }
        self.buf.drain(..off);
        Ok(out)
    }
}

/// The end-of-stream trailer's `{error:{code,message}}`, as a user-facing error.
fn trailer_error(payload: &[u8]) -> Option<anyhow::Error> {
    let v: Value = serde_json::from_slice(payload).ok()?;
    let err = &v["error"];
    let code = err["code"].as_str().unwrap_or("");
    let message = err["message"].as_str().unwrap_or("");
    if code.is_empty() && message.is_empty() {
        return None;
    }
    let mut text = format!("Devin がリクエストを拒否しました");
    if !code.is_empty() {
        text.push_str(&format!("（{code}）"));
    }
    if !message.is_empty() {
        text.push_str(&format!(": {message}"));
    }
    if let Some(details) = err["details"].as_array() {
        let summary: Vec<String> = details
            .iter()
            .filter_map(|d| {
                let kind = d["type"].as_str().unwrap_or("");
                let value = d["value"].as_str().or_else(|| d["debug"].as_str()).unwrap_or("");
                (!kind.is_empty() || !value.is_empty()).then(|| format!("{kind}: {value}"))
            })
            .take(8)
            .collect();
        if !summary.is_empty() {
            text.push_str(&format!(" [{}]", summary.join("; ")));
        }
    }
    Some(anyhow!(text))
}

#[derive(Default)]
struct Acc {
    thinking: String,
    signature: String,
    text: String,
    /// (id, name, accumulated arguments JSON), in arrival order.
    calls: Vec<(String, String, String)>,
    active: Option<usize>,
    usage: Usage,
    stop: Option<u64>,
}

impl Acc {
    /// Consumes one `GetChatMessageResponse`; returns the text delta, if any.
    fn feed(&mut self, payload: &[u8]) -> Result<Option<String>> {
        let mut delta = None;
        for (no, val) in fields(payload)? {
            match (no, &val) {
                (3, Val::Bytes(b)) => {
                    let text = String::from_utf8_lossy(b).into_owned();
                    if !text.is_empty() {
                        self.text.push_str(&text);
                        delta = Some(text);
                    }
                }
                (9, Val::Bytes(b)) => self.thinking.push_str(&String::from_utf8_lossy(b)),
                (10, Val::Bytes(b)) => self.signature = String::from_utf8_lossy(b).into_owned(),
                (5, Val::Varint(v)) if *v != 0 => self.stop = Some(*v),
                (6, Val::Bytes(b)) => self.tool_call(b)?,
                (7, Val::Bytes(b)) => {
                    let f = fields(b)?;
                    self.usage.input = vnum(&f, 2).unwrap_or(0);
                    self.usage.output = vnum(&f, 3).unwrap_or(0);
                    self.usage.cache_write = vnum(&f, 4).unwrap_or(0);
                    self.usage.cache_read = vnum(&f, 5).unwrap_or(0);
                }
                _ => {}
            }
        }
        Ok(delta)
    }

    fn tool_call(&mut self, bytes: &[u8]) -> Result<()> {
        let f = fields(bytes)?;
        let id = vstr(&f, 1).unwrap_or("").to_string();
        let name = vstr(&f, 2).unwrap_or("").to_string();
        let args = vstr(&f, 3).unwrap_or("");
        let index = match (id.is_empty(), self.active) {
            (true, Some(index)) => index,
            (true, None) => return Ok(()),
            (false, _) => match self.calls.iter().position(|(call_id, _, _)| *call_id == id) {
                Some(index) => index,
                None => {
                    self.calls.push((id, String::new(), String::new()));
                    self.calls.len() - 1
                }
            },
        };
        self.active = Some(index);
        let call = &mut self.calls[index];
        if !name.is_empty() {
            call.1 = name;
        }
        if args.is_empty() {
            return Ok(());
        }
        // Arguments arrive either as growing snapshots or as deltas (OMP checks both).
        if args.starts_with(&call.2) {
            call.2 = args.to_string();
        } else {
            call.2.push_str(args);
        }
        Ok(())
    }

    fn finish(self) -> Reply {
        let mut blocks = Vec::new();
        if !self.thinking.is_empty() || !self.signature.is_empty() {
            let mut v = json!({"type": "thinking", "thinking": self.thinking});
            if !self.signature.is_empty() {
                v["signature"] = json!(self.signature);
            }
            blocks.push(Block::Other(v));
        }
        if !self.text.is_empty() {
            blocks.push(Block::Text { text: self.text });
        }
        for (id, name, args) in &self.calls {
            let input = if args.trim().is_empty() {
                json!({})
            } else {
                match serde_json::from_str::<Value>(args) {
                    Ok(v) if v.is_object() => v,
                    _ => json!({"__invalid_json": args}),
                }
            };
            blocks.push(Block::ToolUse { id: id.clone(), name: name.clone(), input });
        }
        let stop = if !self.calls.is_empty() {
            "tool_use"
        } else if self.stop == Some(3) {
            "max_tokens"
        } else {
            "end_turn"
        };
        Reply { blocks, usage: self.usage, stop: stop.into() }
    }
}

// ---- unary RPCs ----

/// A Devin unary Connect call: `application/proto` request, bare protobuf answer.
pub(crate) async fn unary_post(
    http: &reqwest::Client,
    url: &str,
    body: &[u8],
    content_type: &str,
    timeout: Duration,
) -> Result<Vec<u8>> {
    let resp = http
        .post(url)
        .header("content-type", content_type)
        .header("connect-protocol-version", "1")
        .header("accept", "*/*")
        .header("accept-encoding", "identity")
        .timeout(timeout)
        .body(body.to_vec())
        .send()
        .await
        .with_context(|| format!("POST {url}"))?;
    let status = resp.status();
    let headers = resp.headers().clone();
    let bytes = resp.bytes().await?;
    if !status.is_success() {
        return Err(limit::http_error(status, &headers, &String::from_utf8_lossy(&bytes)));
    }
    Ok(bytes.to_vec())
}

/// Windsurf's PKCE exchange: an authorization code for the seat's raw API key.
pub(crate) async fn windsurf_exchange(code: &str, verifier: &str) -> Result<String> {
    windsurf_exchange_at(BASE, code, verifier).await
}

pub(crate) async fn windsurf_exchange_at(base: &str, code: &str, verifier: &str) -> Result<String> {
    let mut w = Wr::default();
    w.str(1, code);
    w.str(2, verifier);
    // Any other content type is answered with 200 and an empty body.
    let bytes = unary_post(
        &reqwest::Client::new(),
        &format!("{}{EXCHANGE}", base.trim_end_matches('/')),
        &w.0,
        "application/proto",
        Duration::from_secs(30),
    )
    .await
    .context("Windsurf の認可コードを交換できませんでした")?;
    let key = vstr(&fields(&bytes)?, 1).unwrap_or("").trim();
    if key.is_empty() {
        bail!("Windsurf の交換応答に API キーがありません");
    }
    // The exchange answers the `g`-prefixed wire form (`gsk-ws-…`); RPCs want the raw key.
    Ok(match key.strip_prefix("gsk-") {
        Some(rest) => format!("sk-{rest}"),
        None => key.to_string(),
    })
}

/// The account a seat key belongs to: email, user id, and the organization the token is scoped to.
pub(crate) async fn identity(api_key: &str) -> Option<serde_json::Map<String, Value>> {
    if api_key.is_empty() {
        return None;
    }
    let mut req = Wr::default();
    req.bytes(1, &metadata(api_key, "", false));
    let bytes = unary_post(
        &reqwest::Client::new(),
        &format!("{BASE}{STATUS}"),
        &req.0,
        "application/proto",
        Duration::from_secs(30),
    )
    .await
    .ok()?;
    let f = fields(&bytes).ok()?;
    let status = vbytes(&f, 1).map(fields).transpose().ok()??;
    let plan_status = vbytes(&status, 13).map(fields).transpose().ok().flatten();
    let plan = vbytes(&f, 2)
        .map(fields)
        .transpose()
        .ok()
        .flatten()
        .or_else(|| {
            plan_status
                .as_ref()
                .and_then(|p| vbytes(p, 1).map(fields).transpose().ok().flatten())
        });
    let devin = plan.as_ref().and_then(|p| vbytes(p, 33).map(fields).transpose().ok().flatten());
    let team = vstr(&status, 5).unwrap_or("").trim().to_string();
    let org = devin.as_ref().and_then(|d| vstr(d, 4)).unwrap_or("").trim().to_string();
    let mut extra = serde_json::Map::new();
    for (key, value) in [
        ("email", vstr(&status, 7).unwrap_or("").trim()),
        ("accountId", vstr(&status, 36).unwrap_or("").trim()),
        ("orgId", if org.is_empty() { team.as_str() } else { org.as_str() }),
        ("orgName", devin.as_ref().and_then(|d| vstr(d, 8)).unwrap_or("").trim()),
    ] {
        if !value.is_empty() {
            extra.insert(key.into(), json!(value));
        }
    }
    extra.insert("apiEndpoint".into(), json!("https://api.devin.ai"));
    extra.insert("enterpriseUrl".into(), json!("https://app.devin.ai"));
    Some(extra)
}

/// This account's chat model uids (`GetCliModelConfigs`), sorted and de-duplicated.
pub(crate) async fn models(base: &str, api_key: &str) -> Result<Vec<String>> {
    let mut req = Wr::default();
    req.bytes(1, &metadata(api_key, "", true));
    let bytes = unary_post(
        &reqwest::Client::new(),
        &format!("{}{CLI_CONFIGS}", base.trim_end_matches('/')),
        &req.0,
        "application/proto",
        Duration::from_secs(30),
    )
    .await?;
    let mut out = std::collections::BTreeSet::new();
    for (no, val) in fields(&bytes)? {
        let Val::Bytes(config) = val else { continue };
        if no != 1 {
            continue;
        }
        let f = fields(config)?;
        if vnum(&f, 4).unwrap_or(0) != 0 {
            continue; // disabled
        }
        let uid = vstr(&f, 22).unwrap_or("").trim().to_string();
        if uid.is_empty() || uid.contains("-sidekick-") {
            continue;
        }
        let info = vbytes(&f, 23).map(fields).transpose()?;
        if let Some(info) = &info {
            let display = vnum(info, 22).unwrap_or(0);
            let router = vnum(info, 25).unwrap_or(0) != 0;
            // Quick-review (4) and internal-default (6) slots are hidden by the native client too;
            // routers need `AssignModel`, which this port does not speak.
            if router || matches!(display, 4 | 6) {
                continue;
            }
        }
        out.insert(uid);
    }
    Ok(out.into_iter().collect())
}

// ---- protobuf encoding ----

/// `Metadata`: the request identity Cascade gates behavior on.
pub(crate) fn metadata(api_key: &str, user_jwt: &str, discovery: bool) -> Vec<u8> {
    let (ide, version) = if discovery { ("chisel", "0.0.0-dev") } else { ("devin-cli", "3000.12.57") };
    let mut w = Wr::default();
    w.str(1, ide);
    w.str(7, version);
    if !discovery {
        w.str(28, "chisel"); // ideType; unlocks router assignment and the CLI model surface
    }
    w.str(12, "chisel");
    w.str(2, version);
    w.str(3, &credential(api_key));
    w.str(4, "en");
    w.str(5, os());
    w.str(21, user_jwt);
    w.0
}

/// JWT-shaped seat tokens go out with the native prefix; other keys (legacy `sk-ws-`) as stored.
fn credential(api_key: &str) -> String {
    let key = api_key.trim();
    if key.is_empty() || key.starts_with("devin-session-token$") {
        return key.into();
    }
    if key.split('.').count() == 3 {
        format!("devin-session-token${key}")
    } else {
        key.into()
    }
}

fn os() -> &'static str {
    match std::env::consts::OS {
        "macos" => "darwin",
        "windows" => "windows",
        _ => "linux",
    }
}

#[derive(Default)]
struct Wr(Vec<u8>);

impl Wr {
    fn key(&mut self, no: u32, wire: u8) {
        put_varint(&mut self.0, u64::from(no) << 3 | u64::from(wire));
    }

    fn varint(&mut self, no: u32, v: u64) {
        if v == 0 {
            return;
        }
        self.key(no, 0);
        put_varint(&mut self.0, v);
    }

    fn bool(&mut self, no: u32, v: bool) {
        if v {
            self.varint(no, 1);
        }
    }

    fn double(&mut self, no: u32, v: f64) {
        if v == 0.0 {
            return;
        }
        self.key(no, 1);
        self.0.extend_from_slice(&v.to_le_bytes());
    }

    fn bytes(&mut self, no: u32, v: &[u8]) {
        if v.is_empty() {
            return;
        }
        self.key(no, 2);
        put_varint(&mut self.0, v.len() as u64);
        self.0.extend_from_slice(v);
    }

    fn str(&mut self, no: u32, v: &str) {
        self.bytes(no, v.as_bytes());
    }
}

fn put_varint(out: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        out.push(v as u8 | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

fn get_varint(b: &mut &[u8]) -> Option<u64> {
    let (mut out, mut shift) = (0u64, 0);
    loop {
        let (&byte, rest) = b.split_first()?;
        *b = rest;
        out |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Some(out);
        }
        shift += 7;
        if shift >= 64 {
            return None;
        }
    }
}

enum Val<'a> {
    Varint(u64),
    Bytes(&'a [u8]),
}

/// All length-delimited and varint fields; fixed32/fixed64 fields are skipped.
fn fields(mut b: &[u8]) -> Result<Vec<(u32, Val<'_>)>> {
    let mut out = Vec::new();
    while !b.is_empty() {
        let key = get_varint(&mut b).ok_or_else(|| anyhow!("protobuf: truncated field key"))?;
        let no = (key >> 3) as u32;
        match (key & 7) as u8 {
            0 => {
                let v = get_varint(&mut b).ok_or_else(|| anyhow!("protobuf: truncated varint"))?;
                out.push((no, Val::Varint(v)));
            }
            1 => {
                if b.len() < 8 {
                    bail!("protobuf: truncated fixed64");
                }
                b = &b[8..];
            }
            2 => {
                let len = get_varint(&mut b).ok_or_else(|| anyhow!("protobuf: truncated length"))? as usize;
                if b.len() < len {
                    bail!("protobuf: truncated bytes field");
                }
                let (v, rest) = b.split_at(len);
                out.push((no, Val::Bytes(v)));
                b = rest;
            }
            5 => {
                if b.len() < 4 {
                    bail!("protobuf: truncated fixed32");
                }
                b = &b[4..];
            }
            other => bail!("protobuf: unsupported wire type {other}"),
        }
    }
    Ok(out)
}

fn vbytes<'a>(f: &[(u32, Val<'a>)], no: u32) -> Option<&'a [u8]> {
    f.iter().find_map(|(n, v)| match v {
        Val::Bytes(b) if *n == no => Some(*b),
        _ => None,
    })
}

fn vstr<'a>(f: &[(u32, Val<'a>)], no: u32) -> Option<&'a str> {
    vbytes(f, no).and_then(|b| std::str::from_utf8(b).ok())
}

fn vnum(f: &[(u32, Val<'_>)], no: u32) -> Option<u64> {
    f.iter().find_map(|(n, v)| match v {
        Val::Varint(x) if *n == no => Some(*x),
        _ => None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn framed(parts: &[(u8, Vec<u8>)]) -> Vec<u8> {
        parts.iter().flat_map(|(flags, payload)| connect_frame(payload, *flags)).collect()
    }

    /// Serves each reply in order, recording the request head and body.
    async fn serve(replies: Vec<Vec<u8>>) -> (String, Arc<Mutex<Vec<(String, Vec<u8>)>>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = seen.clone();
        tokio::spawn(async move {
            for reply in replies {
                let (mut s, _) = listener.accept().await.unwrap();
                let mut req = Vec::new();
                let mut buf = [0u8; 4096];
                let (head, body) = loop {
                    let n = s.read(&mut buf).await.unwrap();
                    if n == 0 {
                        break (String::new(), Vec::new());
                    }
                    req.extend_from_slice(&buf[..n]);
                    if let Some(split) = req.windows(4).position(|w| w == b"\r\n\r\n") {
                        let head = String::from_utf8_lossy(&req[..split]).to_string();
                        let len = head
                            .lines()
                            .find_map(|l| {
                                l.to_ascii_lowercase()
                                    .strip_prefix("content-length:")
                                    .and_then(|v| v.trim().parse::<usize>().ok())
                            })
                            .unwrap_or(0);
                        if req.len() >= split + 4 + len {
                            break (head, req[split + 4..split + 4 + len].to_vec());
                        }
                    }
                };
                log.lock().unwrap().push((head, body));
                let reply = reply;
                s.write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: application/proto\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                        reply.len()
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
                s.write_all(&reply).await.unwrap();
            }
        });
        (format!("http://{addr}"), seen)
    }

    fn msg(parts: &[(u32, &str)]) -> Vec<u8> {
        let mut w = Wr::default();
        for (no, v) in parts {
            w.str(*no, v);
        }
        w.0
    }

    #[tokio::test]
    async fn stream_decodes_text_tools_usage_and_sends_the_cli_request() {
        let (origin, seen) = serve(vec![msg(&[(1, "jwt-1")]), {
            let mut usage = Wr::default();
            usage.varint(2, 10);
            usage.varint(3, 5);
            usage.varint(5, 7);
            let mut text = Wr::default();
            text.str(3, "Hello ");
            text.bytes(7, &usage.0);
            let mut more = Wr::default();
            more.str(3, "world");
            more.varint(5, 3);
            let mut call = Wr::default();
            let mut args = Wr::default();
            args.str(1, "call|1");
            args.str(2, "bash");
            args.str(3, "{\"command\":");
            call.bytes(6, &args.0);
            let mut next = Wr::default();
            let mut more_args = Wr::default();
            more_args.str(1, "call|1");
            more_args.str(3, "\"ls\"}");
            next.bytes(6, &more_args.0);
            next.varint(5, 10);
            let mut thinking = Wr::default();
            thinking.str(9, "hmm");
            thinking.str(10, "sig-1");
            framed(&[(0, thinking.0), (0, text.0), (0, more.0), (0, call.0), (0, next.0), (2, b"{}".to_vec())])
        }])
        .await;

        let devin = Devin::new(origin, "seat-key".into(), Wire::default());
        let messages = vec![
            Message::user("hi"),
            Message {
                role: Role::Assistant,
                content: vec![
                    Block::Other(json!({"type": "thinking", "thinking": "old", "signature": "s0"})),
                    Block::ToolUse { id: "call|0".into(), name: "bash".into(), input: json!({"command": "pwd"}) },
                ],
            },
            Message {
                role: Role::User,
                content: vec![Block::ToolResult { tool_use_id: "call|0".into(), content: "/tmp".into(), is_error: false }],
            },
        ];
        let tools = vec![ToolDef { name: "bash".into(), description: "run".into(), schema: json!({"type": "object"}) }];
        let streamed = Mutex::new(String::new());
        let on_text = |t: &str| streamed.lock().unwrap().push_str(t);
        let req = Request { model: "swe-1-6", system: "sys", messages: &messages, tools: &tools, max_tokens: 1000, live: None };
        let reply = devin.complete(req, &on_text).await.unwrap();

        assert_eq!(streamed.into_inner().unwrap(), "Hello world");
        assert_eq!(reply.stop, "tool_use");
        assert_eq!((reply.usage.input, reply.usage.output, reply.usage.cache_read), (10, 5, 7));
        assert_eq!(reply.blocks.len(), 3, "{:?}", reply.blocks);
        let thinking = match &reply.blocks[0] {
            Block::Other(v) => v,
            other => panic!("expected a thinking block, got {other:?}"),
        };
        assert_eq!(thinking["type"], "thinking");
        assert_eq!((thinking["thinking"].as_str(), thinking["signature"].as_str()), (Some("hmm"), Some("sig-1")));
        assert!(matches!(&reply.blocks[1], Block::Text { text } if text == "Hello world"));
        match &reply.blocks[2] {
            Block::ToolUse { id, name, input } => {
                // Response ids pass through untouched; history replays are sanitized.
                assert_eq!((id.as_str(), name.as_str()), ("call|1", "bash"));
                assert_eq!(input["command"], "ls");
            }
            other => panic!("expected a tool call, got {other:?}"),
        }

        let seen = seen.lock().unwrap();
        assert!(seen[0].0.starts_with("POST /exa.auth_pb.AuthService/GetUserJwt"), "{}", seen[0].0);
        let head = &seen[1].0;
        assert!(head.starts_with("POST /exa.api_server_pb.ApiServerService/GetChatMessage"), "{head}");
        let lower = head.to_ascii_lowercase();
        assert!(lower.contains("content-type: application/connect+proto"), "{head}");
        assert!(lower.contains("connect-protocol-version: 1"), "{head}");
        assert!(!lower.contains("connect-accept-encoding"), "compressed answers would not decode");

        // The request is one Connect frame: 1 flag byte + u32 length, then the protobuf.
        assert_eq!(seen[1].1[0], 0, "a plain, uncompressed frame");
        let body = fields(&seen[1].1[5..]).unwrap();
        assert_eq!(vstr(&body, 2), Some("sys"));
        assert_eq!(vstr(&body, 21), Some("swe-1-6"));
        assert_eq!(vnum(&body, 7), Some(5));
        assert_eq!(vnum(&body, 20), Some(1));
        let md = fields(vbytes(&body, 1).unwrap()).unwrap();
        assert_eq!(vstr(&md, 3), Some("seat-key"));
        assert_eq!((vstr(&md, 1), vstr(&md, 28)), (Some("devin-cli"), Some("chisel")));
        assert_eq!(vstr(&md, 21), Some("jwt-1"));
        let prompts: Vec<_> = body.iter().filter(|(no, _)| *no == 3).map(|(_, v)| fields(match v { Val::Bytes(b) => b, _ => unreachable!() }).unwrap()).collect();
        assert_eq!(prompts.len(), 3, "user, assistant and tool rows");
        assert_eq!(vnum(&prompts[0], 2), Some(1));
        assert_eq!(vnum(&prompts[1], 2), Some(2));
        // The replayed tool call is sanitized the same way the result id is.
        assert_eq!(vstr(&fields(vbytes(&prompts[1], 6).unwrap()).unwrap(), 1), Some("call_0"));
        assert_eq!(vnum(&prompts[2], 2), Some(4));
        assert_eq!(vstr(&prompts[2], 7), Some("call_0"));
        let cfg = fields(vbytes(&body, 8).unwrap()).unwrap();
        assert_eq!(vnum(&cfg, 2), Some(1000));
        assert_eq!(vnum(&cfg, 7), Some(40));
    }

    #[tokio::test]
    async fn end_stream_errors_are_reported_and_empty_jwt_fails() {
        let (origin, _) = serve(vec![msg(&[(1, "jwt")]), framed(&[(2, br#"{"error":{"code":"invalid_argument","message":"boom"}}"#.to_vec())])]).await;
        let devin = Devin::new(origin, "k".into(), Wire::default());
        let req = Request { model: "swe-1-6", system: "", messages: &[Message::user("hi")], tools: &[], max_tokens: 10, live: None };
        let err = devin.complete(req, &|_| {}).await.unwrap_err().to_string();
        assert!(err.contains("invalid_argument") && err.contains("boom"), "{err}");

        let (origin, _) = serve(vec![vec![]]).await;
        let devin = Devin::new(origin, "k".into(), Wire::default());
        let req = Request { model: "swe-1-6", system: "", messages: &[], tools: &[], max_tokens: 10, live: None };
        let text = devin.complete(req, &|_| {}).await.unwrap_err().to_string();
        assert!(text.contains("サインイン"), "{text}");
    }

    #[tokio::test]
    async fn discovery_lists_enabled_chat_uids_only() {
        let mut disabled = Wr::default();
        disabled.str(22, "gone");
        disabled.bool(4, true);
        let mut router = Wr::default();
        router.str(22, "adaptive");
        let mut info = Wr::default();
        info.bool(25, true);
        router.bytes(23, &info.0);
        let (origin, seen) = serve(vec![{
            let mut w = Wr::default();
            w.bytes(1, &disabled.0);
            w.bytes(1, &router.0);
            w.bytes(1, &msg(&[(22, "swe-1-6")]));
            w.bytes(1, &msg(&[(22, "swe-1-6")]));
            w.0
        }])
        .await;
        assert_eq!(models(&origin, "key").await.unwrap(), ["swe-1-6"]);
        let seen = seen.lock().unwrap();
        let md = fields(&fields(&seen[0].1).unwrap().into_iter().find_map(|(no, v)| match v {
            Val::Bytes(b) if no == 1 => Some(b),
            _ => None,
        }).unwrap()).unwrap();
        assert_eq!((vstr(&md, 1), vstr(&md, 28)), (Some("chisel"), None), "the dev-channel identity unlocks the full list");
    }

    #[test]
    fn ids_and_metadata_follow_the_native_shapes() {
        assert_eq!(tool_id("a|b c"), "a_b_c");
        assert_eq!(tool_id(&"x".repeat(50)).len(), TOOL_ID_MAX);
        let a = deterministic_uuid("cascade\00\0user");
        assert_eq!(a, deterministic_uuid("cascade\00\0user"));
        assert_ne!(a, deterministic_uuid("cascade\01\0user"));
        assert_eq!(a.len(), 36);
        assert_ne!(uuid_v4(), uuid_v4());
        assert_eq!(uuid_v4().len(), 36);
        let md = metadata("a.b.c", "", false);
        assert_eq!(vstr(&fields(&md).unwrap(), 3), Some("devin-session-token$a.b.c"));
        let md = metadata("sk-ws-1", "", false);
        assert_eq!(vstr(&fields(&md).unwrap(), 3), Some("sk-ws-1"));
        assert_eq!(os(), match std::env::consts::OS { "macos" => "darwin", "windows" => "windows", _ => "linux" });
    }
}
