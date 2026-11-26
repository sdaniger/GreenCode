//! Kiro: the Kiro CLI's runtime (`AmazonCodeWhispererStreamingService.GenerateAssistantResponse`),
//! AWS JSON 1.0 answered with an `application/vnd.amazon.eventstream` stream.
//!
//! Kiro takes one user turn per request plus a history of alternating user/assistant turns, with
//! tool results riding on the user turn that answers them; our `Message` list is folded into that.
use super::aws_eventstream::{Decoder, Frame};
use super::{send, Provider, Reply, Request, StreamTimeouts, Wire};
use crate::{Block, BoxFut, Role, Usage};
use anyhow::{anyhow, bail, Result};
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;

pub const TARGET: &str = "AmazonCodeWhispererStreamingService.GenerateAssistantResponse";
/// The profile listing the sign-in uses; same client identity, another operation.
pub const LIST_PROFILES: &str = "AmazonCodeWhispererService.ListAvailableProfiles";
const CLI_VERSION: &str = "3.2.0";
const UA_BASE: &str = "aws-sdk-rust/1.6.0 ua/2.1";
const RUST_LANG: &str = "lang/rust/1.94.1";
const CLI_APP: &str = "app/AmazonQ-For-CLI";
/// The CLI shares content by default; `true` opts out, which is all we ever want.
const OPT_OUT: &str = "true";
/// `ksk_` marks a raw API key, which the runtime only routes as one with `tokentype: API_KEY`.
pub const API_KEY_PREFIX: &str = "ksk_";
pub const DEFAULT_REGION: &str = "us-east-1";
/// Per-tool-result cap; past it the runtime rejects the whole request.
const TOOL_RESULT_MAX_BYTES: usize = 800_000;
const HISTORICAL_TOOL_DESCRIPTION: &str = "Tool used in conversation history.";
const CONTINUATION_PROMPT: &str = "Please proceed with the task.";
const HISTORICAL_IMAGE_PLACEHOLDER: &str = "(image omitted from history)";

/// The runtime host a region's credential routes to.
pub fn runtime_base(region: &str) -> String {
    format!("https://runtime.{region}.kiro.dev/")
}

/// `KIRO_API_REGION` when valid, else the default; an API key with neither is a runtime 403.
pub fn api_region() -> String {
    std::env::var("KIRO_API_REGION").ok().filter(|region| is_region(region)).unwrap_or_else(|| DEFAULT_REGION.into())
}

/// An AWS region id, safe to place in a host name.
pub fn is_region(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() >= 9
        && bytes[0].is_ascii_lowercase()
        && bytes[1].is_ascii_lowercase()
        && bytes[2] == b'-'
        && value.contains('-')
        && value.rsplit('-').next().is_some_and(|zone| !zone.is_empty() && zone.bytes().all(|b| b.is_ascii_digit()))
}

/// Whether `key` is a raw Kiro key rather than an OAuth access token.
pub fn is_api_key(key: &str) -> bool {
    key.trim().starts_with(API_KEY_PREFIX)
}

/// The `region` of an `arn:aws:codewhisperer:<region>:<account>:profile/<id>` ARN.
pub fn profile_region(arn: &str) -> Option<String> {
    // arn:aws:codewhisperer:<region>:<account>:profile/<id>
    let parts: Vec<&str> = arn.split(':').collect();
    let [arn_kind, partition, service, region, account, rest] =
        [parts.first()?, parts.get(1)?, parts.get(2)?, parts.get(3)?, parts.get(4)?, parts.get(5)?];
    let id = rest.strip_prefix("profile/")?;
    (*arn_kind == "arn" && matches!(*partition, "aws" | "aws-us-gov") && *service == "codewhisperer"
        && account.bytes().all(|b| b.is_ascii_digit()) && is_region(region)
        && !id.is_empty() && id.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-')))
        .then(|| region.to_string())
}

/// CLI 3.2.0's wire identity; the runtime gates on it, so it is not ours to shorten.
pub fn service_headers(operation: &str, token: &str, attempt: u32, max_attempts: u32) -> Vec<(&'static str, String)> {
    let (api, features) = match operation {
        TARGET => ("api/codewhispererstreaming/0.1.19636", "F"),
        LIST_PROFILES => ("api/codewhispererruntime/0.1.19636", "F"),
        _ => ("api/kirocontrolplanebearer/0.1.0", "F,C"),
    };
    let base = format!("{UA_BASE} {api} os/{} {RUST_LANG}", os());
    let mut headers = vec![
        ("content-type", "application/x-amz-json-1.0".to_string()),
        ("x-amz-target", operation.to_string()),
        ("user-agent", format!("{base} md/appVersion-{CLI_VERSION} {CLI_APP}")),
        ("x-amz-user-agent", format!("{base} m/{features} {CLI_APP}")),
        ("x-amzn-codewhisperer-optout", OPT_OUT.to_string()),
    ];
    if operation == TARGET {
        headers.push(("x-amzn-kiro-attempt", format!("{attempt};max={max_attempts}")));
        // We never fall back to another model server-side.
        headers.push(("x-amzn-kiro-fallback", "0;max=0".to_string()));
    }
    headers.push(("authorization", format!("Bearer {token}")));
    if token.starts_with(API_KEY_PREFIX) {
        headers.push(("tokentype", "API_KEY".to_string()));
    }
    headers.push(("amz-sdk-request", format!("attempt={attempt}; max={max_attempts}")));
    headers.push(("amz-sdk-invocation-id", uuid_v4()));
    headers.push(("accept", "*/*".to_string()));
    headers.push(("accept-encoding", "gzip".to_string()));
    headers
}

pub fn os() -> &'static str {
    match std::env::consts::OS {
        "macos" => "macos",
        "linux" => "linux",
        "windows" => "windows",
        _ => "other",
    }
}

/// A v4-shaped id; the runtime only needs each attempt to look unique.
pub fn uuid_v4() -> String {
    let mut b = [0u8; 16];
    let _ = getrandom::fill(&mut b);
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!("{}-{}-{}-{}-{}", &h[..8], &h[8..12], &h[12..16], &h[16..20], &h[20..32])
}

pub struct Kiro {
    pub http: reqwest::Client,
    /// Runtime base for the credential's region, trailing slash included.
    pub base: String,
    /// The OAuth access token or `ksk_` API key.
    pub key: String,
    /// `profileArn` the account routes with; Builder ID logins have none.
    pub profile: Option<String>,
    pub wire: Wire,
}

impl Kiro {
    pub fn new(http: reqwest::Client, base: String, key: String, profile: Option<String>, wire: Wire) -> Self {
        Self { http, base, key, profile, wire }
    }
}

impl Provider for Kiro {
    fn append_only(&self) -> bool {
        self.wire.append_only
    }

    fn complete<'a>(&'a self, req: Request<'a>, on_text: &'a (dyn Fn(&str) + Send + Sync)) -> BoxFut<'a, Result<Reply>> {
        Box::pin(async move {
            let payload = body(&req, self.profile.as_deref(), &uuid_v4())?;
            let bytes = serde_json::to_vec(&payload)?;
            let max = self.wire.retry.max_retries.saturating_add(1);
            let attempt = std::sync::atomic::AtomicU32::new(1);
            let resp = send(&self.wire.retry, || {
                let n = attempt.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let mut request = self.http.post(&self.base).body(bytes.clone());
                for (name, value) in service_headers(TARGET, &self.key, n, max) {
                    request = request.header(name, value);
                }
                request
            })
            .await?;
            let mut acc = Acc::default();
            frames(resp, self.wire.timeouts, |frame| {
                acc.feed(frame, on_text)?;
                Ok(false)
            })
            .await?;
            acc.finish()
        })
    }
}

/// Reads framed events until the body ends; a body that stops inside a frame is a truncated stream.
async fn frames(mut resp: reqwest::Response, timeouts: StreamTimeouts, mut on: impl FnMut(Frame) -> Result<bool>) -> Result<()> {
    let mut decoder = Decoder::default();
    let mut first = timeouts.first;
    loop {
        // The first-event budget covers the first chunk only; later gaps use the idle budget.
        let budget = first.take().or(timeouts.idle);
        let chunk = match budget {
            Some(budget) => tokio::time::timeout(budget, resp.chunk())
                .await
                .map_err(|_| anyhow!("Kiro のストリームが {}s 途切れました", budget.as_secs()))??,
            None => resp.chunk().await?,
        };
        let Some(chunk) = chunk else { break };
        for frame in decoder.push(&chunk)? {
            if on(frame)? {
                return Ok(());
            }
        }
    }
    if decoder.pending() {
        bail!("Kiro のストリームがメッセージの途中で終了しました");
    }
    Ok(())
}

/// One folded turn.
enum Turn {
    User(UserTurn),
    Assistant(AssistantTurn),
}

#[derive(Default)]
struct UserTurn {
    texts: Vec<String>,
    results: Vec<Value>,
    images: Vec<Value>,
}

#[derive(Default)]
struct AssistantTurn {
    text: String,
    tools: Vec<Value>,
}

/// Folds our messages into alternating turns, merging runs of one role the way the CLI does.
fn fold(messages: &[crate::Message]) -> Vec<Turn> {
    let mut turns: Vec<Turn> = Vec::new();
    for message in messages {
        match message.role {
            Role::User => {
                let mut turn = match turns.pop() {
                    Some(Turn::User(turn)) => turn,
                    Some(other) => {
                        turns.push(other);
                        UserTurn::default()
                    }
                    None => UserTurn::default(),
                };
                for block in &message.content {
                    match block {
                        Block::Text { text } if !text.trim().is_empty() => turn.texts.push(text.clone()),
                        Block::ToolResult { tool_use_id, content, is_error } => {
                            let text = if content.trim().is_empty() { HISTORICAL_IMAGE_PLACEHOLDER.to_string() } else { truncate(content) };
                            turn.results.push(json!({
                                "content": [{"text": text}],
                                "status": if *is_error { "error" } else { "success" },
                                "toolUseId": tool_use_id,
                            }));
                        }
                        Block::Other(v) if v["type"] == "image" => {
                            if let Some(image) = image(v) {
                                turn.images.push(image);
                            }
                        }
                        _ => {}
                    }
                }
                turns.push(Turn::User(turn));
            }
            Role::Assistant => {
                let mut text = String::new();
                let mut tools = Vec::new();
                for block in &message.content {
                    match block {
                        Block::Text { text: piece } => text.push_str(piece),
                        Block::ToolUse { id, name, input } => {
                            tools.push(json!({"name": name, "toolUseId": id, "input": input}))
                        }
                        _ => {}
                    }
                }
                if text.is_empty() && tools.is_empty() {
                    continue;
                }
                match turns.pop() {
                    Some(Turn::Assistant(mut last)) => {
                        last.text.push_str(&text);
                        last.tools.extend(tools);
                        turns.push(Turn::Assistant(last));
                    }
                    other => {
                        if let Some(other) = other {
                            turns.push(other);
                        }
                        turns.push(Turn::Assistant(AssistantTurn { text, tools }));
                    }
                }
            }
        }
    }
    turns
}

/// The Kiro request: current user turn, alternating history, declared tools and the account's profile.
pub fn body(req: &Request, profile: Option<&str>, conversation_id: &str) -> Result<Value> {
    let mut turns = fold(req.messages);
    // Kiro always answers a user turn; a trailing assistant turn means the caller replays our reply.
    if !matches!(turns.last(), Some(Turn::User(_))) {
        turns.push(Turn::User(UserTurn::default()));
    }
    let Some(Turn::User(current)) = turns.pop() else { unreachable!("a user turn was just ensured") };
    let system = (!req.system.trim().is_empty()).then_some(req.system);

    let mut tools: Vec<Value> = req.tools.iter().map(tool).collect();
    // Kiro rejects history tool uses whose tool is not declared; retired tools get a placeholder.
    let mut declared: std::collections::BTreeSet<String> = req.tools.iter().map(|tool| tool.name.clone()).collect();
    for turn in &turns {
        let Turn::Assistant(assistant) = turn else { continue };
        for call in &assistant.tools {
            let Some(name) = call["name"].as_str() else { continue };
            if declared.insert(name.to_string()) {
                tools.push(json!({"toolSpecification": {
                    "name": name,
                    "description": HISTORICAL_TOOL_DESCRIPTION,
                    "inputSchema": {"json": {"type": "object", "properties": {}}},
                }}));
            }
        }
    }

    let mut history = Vec::with_capacity(turns.len());
    for (index, turn) in turns.into_iter().enumerate() {
        history.push(match turn {
            Turn::Assistant(assistant) => {
                let mut message = json!({"content": assistant.text});
                if !assistant.tools.is_empty() {
                    message["toolUses"] = json!(assistant.tools);
                }
                json!({"assistantResponseMessage": message})
            }
            Turn::User(turn) => {
                let mut message = json!({
                    "content": content(&turn, index, system),
                    "modelId": req.model,
                    "origin": "KIRO_CLI",
                });
                if !turn.results.is_empty() {
                    message["userInputMessageContext"] = json!({"toolResults": turn.results});
                }
                json!({"userInputMessage": message})
            }
        });
    }

    let mut user = json!({
        "content": content(&current, history.len(), system),
        "modelId": req.model,
        "origin": "KIRO_CLI",
    });
    if !current.images.is_empty() {
        user["images"] = json!(current.images);
    }
    if !current.results.is_empty() || !tools.is_empty() {
        let mut context = Map::new();
        if !current.results.is_empty() {
            context.insert("toolResults".into(), json!(current.results));
        }
        if !tools.is_empty() {
            context.insert("tools".into(), json!(tools));
        }
        user["userInputMessageContext"] = Value::Object(context);
    }

    let mut state = json!({
        "chatTriggerType": "MANUAL",
        "agentTaskType": "vibe",
        "conversationId": conversation_id,
        "currentMessage": {"userInputMessage": user},
    });
    if !history.is_empty() {
        state["history"] = json!(history);
    }
    let mut request = json!({"agentMode": "vibe", "conversationState": state});
    if let Some(profile) = profile {
        request["profileArn"] = json!(profile);
    }
    Ok(request)
}

fn tool(tool: &crate::ToolDef) -> Value {
    let mut spec = json!({"name": tool.name, "inputSchema": {"json": tool.schema}});
    if !tool.description.is_empty() {
        spec["description"] = json!(tool.description);
    }
    json!({"toolSpecification": spec})
}

/// Kiro's image block; formats it does not name are dropped rather than failing the whole turn.
fn image(v: &Value) -> Option<Value> {
    let format = match v["source"]["media_type"].as_str()?.to_ascii_lowercase().as_str() {
        "image/jpeg" | "image/jpg" => "jpeg",
        "image/png" => "png",
        "image/gif" => "gif",
        "image/webp" => "webp",
        _ => return None,
    };
    Some(json!({"format": format, "source": {"bytes": v["source"]["data"]}}))
}

/// A user turn's content: its messages joined, the system prompt on the very first turn only.
fn content(turn: &UserTurn, index: usize, system: Option<&str>) -> String {
    let mut content = turn.texts.join("\n\n");
    if content.is_empty() && !turn.images.is_empty() {
        content = HISTORICAL_IMAGE_PLACEHOLDER.into();
    }
    if index == 0 {
        if let Some(system) = system {
            content = match content.is_empty() {
                true => system.to_string(),
                false => format!("{system}\n\n{content}"),
            };
        }
    }
    if content.trim().is_empty() && turn.results.is_empty() {
        CONTINUATION_PROMPT.into()
    } else {
        content
    }
}

/// Caps one tool result at the runtime's per-result limit, cutting on a UTF-8 boundary.
fn truncate(text: &str) -> String {
    if text.len() <= TOOL_RESULT_MAX_BYTES {
        return text.to_string();
    }
    let marker = |kept: usize| format!(
        "[Tool result truncated: only the first {kept} of {} UTF-8 bytes were sent. Re-run the tool with a narrower scope if the omitted part is needed.]",
        text.len()
    );
    let mut cut = TOOL_RESULT_MAX_BYTES.saturating_sub(marker(TOOL_RESULT_MAX_BYTES).len() + 2);
    while !text.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}\n\n{}", &text[..cut], marker(cut))
}

/// One tool call while its argument JSON streams in.
struct PendingTool {
    name: String,
    fragments: String,
    snapshot: Option<Value>,
    done: bool,
}

#[derive(Default)]
struct Acc {
    /// Text and thinking in arrival order; tool calls live in `tools`.
    blocks: Vec<Block>,
    /// Set while the last block is a thinking block the next reasoning event extends.
    thinking: bool,
    /// Set while the last block is a text block the next text event extends.
    text_open: bool,
    tools: BTreeMap<String, PendingTool>,
    order: Vec<String>,
    usage: Usage,
    stop: Option<String>,
    refusal: Option<Value>,
    /// Text or a tool call reached the consumer; reasoning alone is not a usable reply.
    visible: bool,
}

impl Acc {
    fn feed(&mut self, frame: Frame, on_text: &(dyn Fn(&str) + Send + Sync)) -> Result<()> {
        let message_type = frame.headers.get(":message-type").map(String::as_str);
        if matches!(message_type, Some("exception") | Some("error")) {
            let payload: Value = serde_json::from_slice(&frame.payload).unwrap_or_default();
            let code = frame
                .headers
                .get(":exception-type")
                .or_else(|| frame.headers.get(":error-code"))
                .cloned()
                .or_else(|| payload["code"].as_str().map(str::to_string))
                .unwrap_or_else(|| "KIRO_EVENTSTREAM_ERROR".into());
            let detail = payload["message"].as_str().or_else(|| frame.headers.get(":error-message").map(String::as_str));
            bail!("Kiro: {code}{}", detail.map(|detail| format!(": {detail}")).unwrap_or_default());
        }
        let Some(event) = frame.headers.get(":event-type").filter(|_| message_type == Some("event")) else {
            return Ok(());
        };
        let payload: Value = serde_json::from_slice(&frame.payload)
            .map_err(|_| anyhow!("Kiro が壊れた {event} イベントを返しました"))?;
        match event.as_str() {
            "initial-response" | "metadataEvent" => {
                if let Some(reason) = payload["stopReason"].as_str() {
                    self.stop = Some(reason.into());
                }
                if payload["stopDetails"]["refusal"].is_object() {
                    self.refusal = Some(payload["stopDetails"]["refusal"].clone());
                }
                if payload["tokenUsage"].is_object() {
                    let count = |key: &str| payload["tokenUsage"][key].as_u64().unwrap_or(0);
                    self.usage = Usage {
                        input: count("uncachedInputTokens"),
                        output: count("outputTokens"),
                        cache_read: count("cacheReadInputTokens"),
                        cache_write: count("cacheWriteInputTokens"),
                    };
                }
            }
            "reasoningContentEvent" => {
                let text = payload["text"].as_str().unwrap_or("");
                let signature = payload["signature"].as_str().unwrap_or("");
                if text.is_empty() && signature.is_empty() {
                    return Ok(());
                }
                if !self.thinking {
                    self.blocks.push(Block::Other(json!({"type": "thinking", "thinking": ""})));
                    self.thinking = true;
                    self.text_open = false;
                }
                let Some(Block::Other(block)) = self.blocks.last_mut() else { unreachable!() };
                let joined = format!("{}{text}", block["thinking"].as_str().unwrap_or(""));
                block["thinking"] = json!(joined);
                if !signature.is_empty() {
                    block["signature"] = json!(signature);
                }
            }
            "assistantResponseEvent" => {
                let text = payload["content"]
                    .as_str()
                    .ok_or_else(|| anyhow!("Kiro の assistantResponseEvent に content がありません"))?;
                if text.is_empty() || (text.trim().is_empty() && !self.visible) {
                    return Ok(());
                }
                self.visible = true;
                if !self.text_open {
                    self.blocks.push(Block::Text { text: String::new() });
                    self.text_open = true;
                }
                if let Some(Block::Text { text: current }) = self.blocks.last_mut() {
                    current.push_str(text);
                }
                on_text(text);
            }
            "toolUseEvent" => self.tool(&payload)?,
            _ => {}
        }
        Ok(())
    }

    fn tool(&mut self, payload: &Value) -> Result<()> {
        let id = payload["toolUseId"].as_str().filter(|id| !id.is_empty())
            .ok_or_else(|| anyhow!("Kiro の toolUseEvent に toolUseId がありません"))?;
        self.text_open = false;
        self.thinking = false;
        if self.tools.get(id).is_some_and(|tool| tool.done) {
            bail!("Kiro のツール {id} は完了後にデータを受け取りました");
        }
        if !self.tools.contains_key(id) {
            let name = payload["name"].as_str().filter(|name| !name.is_empty())
                .ok_or_else(|| anyhow!("Kiro のツール {id} が名前なしで始まりました"))?;
            self.visible = true;
            self.tools.insert(id.into(), PendingTool {
                name: name.into(),
                fragments: String::new(),
                snapshot: None,
                done: false,
            });
            self.order.push(id.into());
        }
        let tool = self.tools.get_mut(id).expect("inserted above");
        match &payload["input"] {
            Value::String(fragment) if !fragment.is_empty() => tool.fragments.push_str(fragment),
            Value::Object(_) => tool.snapshot = Some(payload["input"].clone()),
            _ => {}
        }
        if payload["stop"] == json!(true) {
            let args = tool.snapshot.take().or_else(|| serde_json::from_str(tool.fragments.trim()).ok());
            if !args.as_ref().is_some_and(Value::is_object) {
                bail!("Kiro のツール {id} が壊れた JSON 引数で完了しました");
            }
            tool.done = true;
        }
        Ok(())
    }

    fn finish(mut self) -> Result<Reply> {
        let filtered = self.stop.as_deref() == Some("CONTENT_FILTERED") || self.refusal.is_some();
        let truncated = matches!(self.stop.as_deref(), Some("MAX_TOKENS") | Some("LENGTH"));
        if filtered {
            let refusal = self.refusal.unwrap_or_default();
            let label = refusal["category"].as_str().map(|category| format!("拒否（{category}）")).unwrap_or_else(|| "拒否".into());
            bail!("Kiro {label}: {}", refusal["explanation"].as_str().unwrap_or("コンテンツが Kiro 側でフィルタされました"));
        }
        // A truncated or filtered turn's unfinished calls carry cut-off arguments; dropping them beats running them.
        for id in self.order {
            let Some(tool) = self.tools.remove(&id) else { continue };
            let input = match tool.snapshot.or_else(|| serde_json::from_str(tool.fragments.trim()).ok()) {
                Some(input) if input.is_object() => input,
                _ if truncated => continue,
                _ => bail!("Kiro のツール {id} が壊れた JSON 引数で終了しました"),
            };
            self.blocks.push(Block::ToolUse { id, name: tool.name, input });
        }
        self.blocks.retain(|block| !matches!(block, Block::Other(v) if v["type"] == "thinking" && v["thinking"].as_str().unwrap_or("").is_empty()));
        if !self.visible {
            bail!("Kiro のストリームが表示できる出力なしで終了しました");
        }
        let tool_use = self.blocks.iter().any(|block| matches!(block, Block::ToolUse { .. }));
        Ok(Reply {
            blocks: self.blocks,
            usage: self.usage,
            // Truncation wins over an emitted tool call: its arguments may be cut off.
            stop: match (truncated, tool_use) {
                (true, _) => "max_tokens".into(),
                (false, true) => "tool_use".into(),
                _ => "end_turn".into(),
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::super::aws_eventstream;
    use super::*;
    use crate::{Message, Role, ToolDef};
    use serde_json::json;

    fn request<'a>(model: &'a str, messages: &'a [Message], tools: &'a [ToolDef]) -> Request<'a> {
        Request { model, system: "be brief", messages, tools, max_tokens: 1024, live: None }
    }

    fn tool() -> ToolDef {
        ToolDef { name: "read".into(), description: "read a file".into(), schema: json!({"type": "object", "properties": {"path": {"type": "string"}}}) }
    }

    #[test]
    fn the_body_folds_tool_results_onto_the_user_turn_and_names_the_profile() {
        let messages = [
            Message::user("read a.rs"),
            Message { role: Role::Assistant, content: vec![
                Block::Text { text: "sure".into() },
                Block::ToolUse { id: "call_1".into(), name: "read".into(), input: json!({"path": "a.rs"}) },
            ]},
            Message { role: Role::User, content: vec![Block::ToolResult {
                tool_use_id: "call_1".into(), content: "fn main() {}".into(), is_error: false,
            }]},
            Message { role: Role::Assistant, content: vec![Block::ToolUse {
                id: "call_2".into(), name: "retired".into(), input: json!({}),
            }]},
            Message::user("thanks"),
        ];
        let tools = [tool()];
        let body = body(&request("auto", &messages, &tools), Some("arn:aws:codewhisperer:eu-central-1:1:profile/P"), "conv").unwrap();
        assert_eq!(body["agentMode"], "vibe");
        assert_eq!(body["profileArn"], "arn:aws:codewhisperer:eu-central-1:1:profile/P");
        let state = &body["conversationState"];
        assert_eq!(state["chatTriggerType"], "MANUAL");
        assert_eq!(state["agentTaskType"], "vibe");
        assert_eq!(state["conversationId"], "conv");
        // The last user turn is the current message; the system prompt rides on the first turn only.
        let current = &state["currentMessage"]["userInputMessage"];
        assert_eq!(current["content"], "thanks");
        assert_eq!(current["modelId"], "auto");
        assert_eq!(current["origin"], "KIRO_CLI");
        let history = state["history"].as_array().unwrap();
        assert_eq!(history.len(), 4);
        assert_eq!(history[0]["userInputMessage"]["content"], "be brief\n\nread a.rs");
        assert_eq!(history[1]["assistantResponseMessage"]["content"], "sure");
        assert_eq!(history[1]["assistantResponseMessage"]["toolUses"][0]["toolUseId"], "call_1");
        let results = &history[2]["userInputMessage"]["userInputMessageContext"]["toolResults"];
        assert_eq!(results[0]["toolUseId"], "call_1");
        assert_eq!(results[0]["status"], "success");
        assert_eq!(results[0]["content"][0]["text"], "fn main() {}");
        assert_eq!(history[3]["assistantResponseMessage"]["toolUses"][0]["name"], "retired");
        // The declared read tool plus a placeholder for the retired one.
        let declared = current["userInputMessageContext"]["tools"].as_array().unwrap();
        assert_eq!(declared.len(), 2);
        assert_eq!(declared[0]["toolSpecification"]["name"], "read");
        assert_eq!(declared[1]["toolSpecification"]["name"], "retired");
        assert_eq!(declared[1]["toolSpecification"]["inputSchema"]["json"]["properties"], json!({}));
    }

    #[test]
    fn an_oversized_tool_result_is_cut_on_a_character_boundary() {
        let huge = "あ".repeat(400_000);
        let cut = truncate(&huge);
        assert!(cut.len() <= TOOL_RESULT_MAX_BYTES + 200, "{}", cut.len());
        assert!(cut.contains("of 1200000 UTF-8 bytes"));
        assert!(cut.starts_with("ああ"));
    }

    #[tokio::test]
    async fn the_stream_becomes_blocks_with_usage_and_the_cli_headers() {
        let arn = "arn:aws:codewhisperer:eu-central-1:1:profile/P";
        let body = stream(&[
            event("initial-response", json!({"requestId": "r1"})),
            event("reasoningContentEvent", json!({"text": "think", "signature": "sig"})),
            event("assistantResponseEvent", json!({"content": "hel"})),
            event("assistantResponseEvent", json!({"content": "lo"})),
            event("toolUseEvent", json!({"toolUseId": "call_1", "name": "read", "input": "{\"path\":"})),
            event("toolUseEvent", json!({"toolUseId": "call_1", "input": "\"a.rs\"}", "stop": true})),
            event("metadataEvent", json!({
                "stopReason": "TOOL_USE",
                "tokenUsage": {"uncachedInputTokens": 100, "outputTokens": 10, "cacheReadInputTokens": 40, "cacheWriteInputTokens": 5},
            })),
        ]);
        let (base, seen) = server(body).await;
        let mut wire = Wire::default();
        wire.retry.max_retries = 0;
        let client = Kiro::new(reqwest::Client::new(), base, "ksk_key".into(), Some(arn.into()), wire);
        let messages = [Message::user("read a.rs")];
        let tools = [tool()];
        let streamed = std::sync::Mutex::new(String::new());
        let reply = client.complete(request("auto", &messages, &tools), &|text| streamed.lock().unwrap().push_str(text)).await.unwrap();
        assert_eq!(*streamed.lock().unwrap(), "hello");
        assert_eq!(reply.stop, "tool_use");
        assert_eq!(reply.usage, Usage { input: 100, output: 10, cache_read: 40, cache_write: 5 });
        assert!(reply.blocks.iter().any(|b| matches!(b, Block::Text { text } if text == "hello")));
        assert!(reply.blocks.iter().any(|b| matches!(b, Block::Other(v)
            if v["type"] == "thinking" && v["thinking"] == "think" && v["signature"] == "sig")));
        assert!(reply.blocks.iter().any(|b| matches!(b, Block::ToolUse { id, name, input }
            if id == "call_1" && name == "read" && input["path"] == "a.rs")));

        let seen = seen.lock().unwrap().take().expect("the server saw a request");
        assert_eq!(seen.method, "POST");
        assert_eq!(seen.path, "/");
        assert_eq!(seen.header("x-amz-target"), Some(TARGET));
        assert_eq!(seen.header("content-type"), Some("application/x-amz-json-1.0"));
        assert_eq!(seen.header("authorization"), Some("Bearer ksk_key"));
        assert_eq!(seen.header("tokentype"), Some("API_KEY"));
        assert_eq!(seen.header("x-amzn-codewhisperer-optout"), Some("true"));
        assert_eq!(seen.header("x-amzn-kiro-fallback"), Some("0;max=0"));
        assert!(seen.header("user-agent").is_some_and(|a| a.contains("api/codewhispererstreaming/0.1.19636") && a.contains("md/appVersion-3.2.0")), "{:?}", seen.header("user-agent"));
        assert_eq!(seen.header("amz-sdk-request"), Some("attempt=1; max=1"));
        assert_eq!(seen.header("accept"), Some("*/*"));
        let sent: Value = serde_json::from_slice(&seen.body).unwrap();
        assert_eq!(sent["profileArn"], arn);
        assert_eq!(sent["conversationState"]["currentMessage"]["userInputMessage"]["content"], "be brief\n\nread a.rs");
        assert_eq!(sent["conversationState"]["currentMessage"]["userInputMessage"]["userInputMessageContext"]["tools"][0]["toolSpecification"]["name"], "read");
    }

    #[tokio::test]
    async fn a_refusal_is_reported_in_japanese_with_its_category() {
        let body = stream(&[
            event("assistantResponseEvent", json!({"content": "no"})),
            event("metadataEvent", json!({
                "stopReason": "CONTENT_FILTERED",
                "stopDetails": {"refusal": {"category": "SAFETY", "explanation": "blocked"}},
            })),
        ]);
        let (base, _) = server(body).await;
        let client = Kiro::new(reqwest::Client::new(), base, "sk_oauth".into(), None, Wire::default());
        let messages = [Message::user("hi")];
        let error = client.complete(request("auto", &messages, &[]), &|_| {}).await.unwrap_err().to_string();
        assert!(error.contains("拒否（SAFETY）") && error.contains("blocked"), "{error}");
    }

    #[tokio::test]
    async fn a_truncated_turn_drops_an_unfinished_tool_and_stops_at_max_tokens() {
        let body = stream(&[
            event("assistantResponseEvent", json!({"content": "partial"})),
            event("toolUseEvent", json!({"toolUseId": "call_1", "name": "read", "input": "{\"path\":"})),
            event("metadataEvent", json!({"stopReason": "MAX_TOKENS"})),
        ]);
        let (base, _) = server(body).await;
        let client = Kiro::new(reqwest::Client::new(), base, "sk_oauth".into(), None, Wire::default());
        let messages = [Message::user("hi")];
        let reply = client.complete(request("auto", &messages, &[]), &|_| {}).await.unwrap();
        assert_eq!(reply.stop, "max_tokens");
        assert!(!reply.blocks.iter().any(|b| matches!(b, Block::ToolUse { .. })));
    }

    #[tokio::test]
    async fn a_corrupt_frame_fails_the_turn() {
        let mut frame = aws_eventstream::encode(&[(":message-type", "event"), (":event-type", "assistantResponseEvent")], br#"{"content":"x"}"#);
        let last = frame.len() - 1;
        frame[last] ^= 0xff;
        let (base, _) = server(frame).await;
        let client = Kiro::new(reqwest::Client::new(), base, "sk_oauth".into(), None, Wire::default());
        let messages = [Message::user("hi")];
        let error = client.complete(request("auto", &messages, &[]), &|_| {}).await.unwrap_err().to_string();
        assert!(error.contains("eventstream"), "{error}");
    }

    #[tokio::test]
    async fn an_exception_frame_names_its_code_and_message() {
        let frame = aws_eventstream::encode(
            &[(":message-type", "exception"), (":exception-type", "ThrottlingError")],
            br#"{"message":"slow down"}"#,
        );
        let (base, _) = server(frame).await;
        let client = Kiro::new(reqwest::Client::new(), base, "sk_oauth".into(), None, Wire::default());
        let messages = [Message::user("hi")];
        let error = client.complete(request("auto", &messages, &[]), &|_| {}).await.unwrap_err().to_string();
        assert!(error.contains("ThrottlingError") && error.contains("slow down"), "{error}");
    }

    fn event(kind: &str, payload: Value) -> Vec<u8> {
        aws_eventstream::encode(&[(":message-type", "event"), (":event-type", kind)], payload.to_string().as_bytes())
    }

    fn stream(frames: &[Vec<u8>]) -> Vec<u8> {
        frames.concat()
    }

    async fn server(body: Vec<u8>) -> (String, std::sync::Arc<std::sync::Mutex<Option<crate::localserver::Request>>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}/", listener.local_addr().unwrap());
        let seen = std::sync::Arc::new(std::sync::Mutex::new(None));
        let captured = seen.clone();
        tokio::spawn(async move {
            let _ = crate::localserver::serve(listener, 1 << 20, move |request| {
                let captured = captured.clone();
                let body = body.clone();
                async move {
                    *captured.lock().unwrap() = Some(request);
                    crate::localserver::Response::new("200 OK", "application/vnd.amazon.eventstream", body)
                }
            })
            .await;
        });
        (base, seen)
    }
}
