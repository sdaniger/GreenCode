//! Amazon Bedrock: Converse over `bedrock-runtime` (SigV4 or a Bedrock API key), and the OpenAI
//! Responses wire that Bedrock Mantle serves under `/openai/v1`.
use super::aws_credentials;
use super::aws_eventstream::{self, Frame};
use super::aws_sigv4::{self, Credentials};
use super::{send, Provider, Reply, Request, StreamTimeouts, Wire};
use crate::{Block, BoxFut, Role, Usage};
use anyhow::{anyhow, bail, Result};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};

const USER_AGENT: &str = concat!("GreenCode/", env!("CARGO_PKG_VERSION"));
const EVENTSTREAM: &str = "application/vnd.amazon.eventstream";

/// Bedrock validates that a request whose history carries tool blocks also names a tool; ephemeral
/// turns (`/btw`, auto-replies) have none, so the request names this one and the reply drops it.
const NO_TOOLS: &str = "__no_tools__";

pub struct Bedrock {
    pub http: reqwest::Client,
    pub base: String,
    /// A Bedrock API key (bearer); empty means SigV4 with ambient AWS credentials.
    pub key: String,
    pub effort: Option<String>,
    pub wire: Wire,
}

pub struct Mantle {
    pub http: reqwest::Client,
    pub base: String,
    pub key: String,
    pub effort: Option<String>,
    pub wire: Wire,
}

impl Bedrock {
    pub fn new(http: reqwest::Client, base: String, key: String, effort: Option<String>) -> Self {
        Self { http, base, key, effort, wire: Wire::default() }
    }

    pub fn with_wire(mut self, wire: Wire) -> Self {
        self.wire = wire;
        self
    }

    /// The Converse request and whether the no-tools sentinel was injected.
    pub fn body(&self, req: &Request) -> (Value, bool) {
        // Bedrock gates cache points on the model; only Claude accepts them today, and sending one
        // to Nova/Qwen/DeepSeek is a ValidationException rather than an ignored field.
        let cache = self.wire.cache_retention.caching() && req.model.to_ascii_lowercase().contains("claude");
        let ttl = (self.wire.cache_retention == crate::config::provider::CacheRetention::Long).then_some("1h");
        let cache_point = || {
            let mut point = json!({"cachePoint": {"type": "default"}});
            if let Some(ttl) = ttl {
                point["cachePoint"]["ttl"] = json!(ttl);
            }
            point
        };

        let mut messages: Vec<Value> = Vec::new();
        let mut has_tools = false;
        for message in req.messages {
            let role = match message.role {
                Role::User => "user",
                Role::Assistant => "assistant",
            };
            let mut blocks = Vec::new();
            for block in &message.content {
                let converted = match block {
                    Block::Text { text } if !text.trim().is_empty() => Some(json!({"text": text})),
                    Block::Text { .. } => None,
                    Block::ToolUse { id, name, input } => {
                        has_tools = true;
                        Some(json!({"toolUse": {"toolUseId": id, "name": name, "input": input}}))
                    }
                    Block::ToolResult { tool_use_id, content, is_error } => {
                        has_tools = true;
                        Some(json!({"toolResult": {
                            "toolUseId": tool_use_id,
                            "content": [{"text": content}],
                            "status": if *is_error { "error" } else { "success" },
                        }}))
                    }
                    Block::Other(v) if v["type"] == "image" && v["source"]["type"] == "base64" => {
                        let format = match v["source"]["media_type"].as_str().unwrap_or("") {
                            "image/jpeg" | "image/jpg" => Some("jpeg"),
                            "image/png" => Some("png"),
                            "image/gif" => Some("gif"),
                            "image/webp" => Some("webp"),
                            _ => None,
                        };
                        format.map(|format| json!({"image": {"format": format, "source": {"bytes": v["source"]["data"]}}}))
                    }
                    // Only a model that streamed a signature (Claude) accepts signed reasoning back;
                    // unsigned reasoning replayed as `reasoningContent` is rejected by Nova et al.
                    Block::Other(v) if v["type"] == "thinking" => {
                        let text = v["thinking"].as_str().unwrap_or("");
                        match v["signature"].as_str().filter(|s| !s.is_empty()) {
                            Some(signature) => Some(json!({"reasoningContent": {"reasoningText": {"text": text, "signature": signature}}})),
                            None if !text.is_empty() => Some(json!({"text": text})),
                            None => None,
                        }
                    }
                    Block::Other(_) => None,
                };
                if let Some(converted) = converted {
                    blocks.push(converted);
                }
            }
            if blocks.is_empty() {
                continue;
            }
            // Bedrock rejects two consecutive same-role messages, which a skipped empty reply or a
            // run of tool results would otherwise produce.
            match messages.last_mut().filter(|last| last["role"] == role) {
                Some(last) => last["content"].as_array_mut().unwrap().extend(blocks),
                None => messages.push(json!({"role": role, "content": blocks})),
            }
        }

        let mut body = json!({"messages": messages, "inferenceConfig": {}});
        if !req.system.trim().is_empty() {
            let mut system = vec![json!({"text": req.system})];
            if cache {
                system.push(cache_point());
            }
            body["system"] = json!(system);
        }
        if cache {
            // The final user boundary is the checkpoint that survives the next turn's growth.
            if let Some(last) = body["messages"].as_array_mut().and_then(|m| m.last_mut()).filter(|m| m["role"] == "user") {
                last["content"].as_array_mut().unwrap().push(cache_point());
            }
        }
        body["inferenceConfig"]["maxTokens"] = json!(req.max_tokens);
        for (key, value) in [("temperature", self.wire.sampling.temperature), ("topP", self.wire.sampling.top_p)] {
            if let Some(value) = crate::config::provider::Sampling::value(value) {
                body["inferenceConfig"][key] = json!(value);
            }
        }

        let mut tools: Vec<Value> = req
            .tools
            .iter()
            .map(|tool| {
                let mut spec = json!({"name": tool.name, "inputSchema": {"json": tool.schema}});
                // Bedrock rejects an explicitly empty description.
                if !tool.description.is_empty() {
                    spec["description"] = json!(tool.description);
                }
                json!({"toolSpec": spec})
            })
            .collect();
        let sentinel = tools.is_empty() && has_tools;
        if sentinel {
            tools.push(json!({"toolSpec": {
                "name": NO_TOOLS,
                "description": "Placeholder required by Bedrock validation. Do not call; answer with text.",
                "inputSchema": {"json": {"type": "object", "properties": {}}},
            }}));
        }
        if !tools.is_empty() {
            body["toolConfig"] = json!({"tools": tools, "toolChoice": {"auto": {}}});
        }
        if let Some(fields) = self.additional(req) {
            body["additionalModelRequestFields"] = fields;
        }
        (body, sentinel)
    }

    /// `additionalModelRequestFields.thinking` / `.reasoning`: the Converse way to ask for reasoning.
    fn additional(&self, req: &Request) -> Option<Value> {
        if self.wire.external_thinking {
            return None;
        }
        let effort = self.effort.as_deref()?;
        let id = req.model.to_ascii_lowercase();
        if id.contains("claude") {
            let version = claude_version(&id);
            // Opus 4.6 and 4.7+ think adaptively; everything older takes an explicit budget.
            let adaptive = version.is_some_and(|(major, minor)| (major, minor) >= (4, 7) || (major, minor) == (4, 6) && id.contains("opus"));
            if adaptive {
                return Some(json!({"thinking": {"type": "adaptive", "display": "summarized"}, "output_config": {"effort": effort}}));
            }
            let budget = self.wire.thinking.budget(effort)?;
            return Some(json!({"thinking": {"type": "enabled", "budget_tokens": budget, "display": "summarized"}}));
        }
        // Converse expresses these models' reasoning as an OpenAI-style effort level instead.
        if id.contains("gpt-") || id.contains("grok") || id.contains("kimi-k3") {
            return Some(json!({"reasoning": {"effort": effort}}));
        }
        // Nova/Qwen/Llama/Mistral take no thinking field we can shape without their catalog rules;
        // an unsupported budget block is a hard 400, so leave them alone.
        None
    }
}

impl Mantle {
    pub fn new(http: reqwest::Client, base: String, key: String, effort: Option<String>) -> Self {
        Self { http, base, key, effort, wire: Wire::default() }
    }

    pub fn with_wire(mut self, wire: Wire) -> Self {
        self.wire = wire;
        self
    }
}

impl Provider for Bedrock {
    fn append_only(&self) -> bool {
        self.wire.append_only
    }

    fn complete<'a>(&'a self, req: Request<'a>, on_text: &'a (dyn Fn(&str) + Send + Sync)) -> BoxFut<'a, Result<Reply>> {
        Box::pin(async move {
            let (body, sentinel) = self.body(&req);
            let region = region_for(req.model, aws_credentials::ambient(None), &self.base);
            let url = converse_url(&runtime_base(&self.base, &region), req.model)?;
            let auth = Auth { service: "bedrock", region, bearer: bearer(&self.key) };
            let resp = post(&self.http, &url, EVENTSTREAM, &serde_json::to_vec(&body)?, &auth, &self.wire.retry).await?;
            let mut acc = Acc { sentinel, ..Default::default() };
            frames(resp, self.wire.timeouts, |frame| {
                acc.feed(&frame, on_text)?;
                Ok(false)
            })
            .await?;
            acc.finish()
        })
    }
}

impl Provider for Mantle {
    fn append_only(&self) -> bool {
        self.wire.append_only
    }

    fn complete<'a>(&'a self, req: Request<'a>, on_text: &'a (dyn Fn(&str) + Send + Sync)) -> BoxFut<'a, Result<Reply>> {
        Box::pin(async move {
            let body = super::responses::body(
                &super::responses::Shape {
                    model: req.model,
                    system: req.system,
                    messages: req.messages,
                    tools: req.tools,
                    effort: self.effort.as_deref().filter(|_| !self.wire.external_thinking),
                    codex: false,
                },
                &self.wire,
            );
            let region = region_for(req.model, aws_credentials::ambient(None), &self.base);
            let base = resolve_base(&self.base, &region);
            let url = format!("{}/responses", base.trim_end_matches('/'));
            let auth = Auth { service: "bedrock-mantle", region, bearer: bearer(&self.key) };
            let resp = post(&self.http, &url, "text/event-stream", &serde_json::to_vec(&body)?, &auth, &self.wire.retry).await?;
            let mut acc = super::responses::Acc::default();
            super::sse(resp, self.wire.timeouts, |data| {
                if data == "[DONE]" {
                    return Ok(true);
                }
                if let Ok(ev) = serde_json::from_str::<Value>(data) {
                    if let Some(text) = acc.feed(&ev)? {
                        on_text(&text);
                    }
                }
                Ok(false)
            })
            .await?;
            Ok(acc.finish())
        })
    }
}

/// One request's signing identity: a bearer token when configured, otherwise SigV4 credentials.
struct Auth {
    service: &'static str,
    region: String,
    bearer: Option<String>,
}

/// The Bedrock API key, from the provider config or `AWS_BEARER_TOKEN_BEDROCK`.
fn bearer(key: &str) -> Option<String> {
    let from_env = std::env::var("AWS_BEARER_TOKEN_BEDROCK").ok();
    let tokens = [key.trim(), from_env.as_deref().unwrap_or("").trim()];
    tokens.iter().find(|token| !token.is_empty()).map(|token| token.to_string())
}

/// POSTs a JSON body with retries; a signed request rejected with 401/403 retries once with
/// credentials re-read from disk, which repairs rotated session keys in `~/.aws/credentials`.
async fn post(http: &reqwest::Client, url: &str, accept: &str, body: &[u8], auth: &Auth, retry: &crate::config::provider::Retry) -> Result<reqwest::Response> {
    let parsed = url::Url::parse(url).map_err(|e| anyhow!("Bedrock の URL が不正です（{url}）: {e}"))?;
    let host = parsed.host_str().ok_or_else(|| anyhow!("Bedrock の URL にホストがありません（{url}）"))?.to_string();
    let host = match parsed.port() {
        Some(port) => format!("{host}:{port}"),
        None => host,
    };
    let path = parsed.path().to_string();
    let query = parsed.query().map(str::to_string);
    let mut signed_headers = BTreeMap::new();
    signed_headers.insert("content-type".to_string(), "application/json".to_string());
    signed_headers.insert("accept".to_string(), accept.to_string());
    signed_headers.insert("user-agent".to_string(), USER_AGENT.to_string());
    let build = |credentials: &Option<Credentials>| {
        let request = http.post(url).header("content-type", "application/json").header("accept", accept).header(reqwest::header::USER_AGENT, USER_AGENT).body(body.to_vec());
        match (&auth.bearer, credentials) {
            (Some(token), _) => request.bearer_auth(token),
            (None, Some(credentials)) => {
                let signed = aws_sigv4::sign("POST", &host, &path, query.as_deref(), &signed_headers, body, &auth.region, auth.service, credentials, &aws_sigv4::amz_date(unix_ms()));
                let mut request = request
                    .header("x-amz-date", signed.date)
                    .header("x-amz-content-sha256", signed.content_sha256)
                    .header(reqwest::header::AUTHORIZATION, signed.authorization);
                if let Some(token) = signed.session_token {
                    request = request.header("x-amz-security-token", token);
                }
                request
            }
            // Never reached: credentials are resolved before the first attempt.
            (None, None) => request,
        }
    };
    if auth.bearer.is_some() {
        return send(retry, || build(&None)).await;
    }
    let mut credentials = Some(aws_credentials::resolve(None).await?);
    match send(retry, || build(&credentials)).await {
        Ok(resp) => Ok(resp),
        Err(error) if rejected(&error) => {
            aws_credentials::invalidate(&aws_credentials::profile(None));
            credentials = Some(aws_credentials::resolve(None).await?);
            send(retry, || build(&credentials)).await
        }
        Err(error) => Err(error),
    }
}

/// Whether an error is an auth rejection whose credentials are worth re-reading once.
fn rejected(error: &anyhow::Error) -> bool {
    let text = error.to_string();
    text.starts_with("HTTP 401") || text.starts_with("HTTP 403")
}

/// Reads eventstream frames from the body under the same first-event/idle watchdogs as SSE.
async fn frames(mut resp: reqwest::Response, timeouts: StreamTimeouts, mut on: impl FnMut(Frame) -> Result<bool>) -> Result<()> {
    let mut decoder = aws_eventstream::Decoder::default();
    let mut first = timeouts.first;
    loop {
        let budget = first.take().or(timeouts.idle);
        let chunk = match budget {
            Some(budget) => tokio::time::timeout(budget, resp.chunk())
                .await
                .map_err(|_| anyhow!("model stream stayed silent for {}s", budget.as_secs()))??,
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
        bail!("Bedrock のストリームがメッセージの途中で終了しました");
    }
    Ok(())
}

/// One content block while it streams.
enum Partial {
    Text(String),
    Thinking { text: String, signature: String },
    Tool { id: String, name: String, json: String },
}

#[derive(Default)]
struct Acc {
    blocks: BTreeMap<u64, Partial>,
    usage: Usage,
    stop: String,
    sentinel: bool,
    /// Indices carrying the injected placeholder, whose calls are not real.
    dropped: BTreeSet<u64>,
}

impl Acc {
    fn feed(&mut self, frame: &Frame, on_text: &dyn Fn(&str)) -> Result<()> {
        let kind = frame.headers.get(":message-type").map(String::as_str).unwrap_or("");
        let payload: Value = if frame.payload.is_empty() {
            json!({})
        } else {
            serde_json::from_slice(&frame.payload).unwrap_or(Value::Null)
        };
        if kind == "exception" || kind == "error" {
            let code = frame.headers.get(":exception-type").or_else(|| frame.headers.get(":error-code")).map(String::as_str).unwrap_or("Exception");
            let message = payload["message"].as_str().map(str::to_string).unwrap_or_else(|| String::from_utf8_lossy(&frame.payload).into_owned());
            bail!("Bedrock {code}: {message}");
        }
        if kind != "event" {
            return Ok(());
        }
        let n = |v: &Value| v.as_u64().unwrap_or(0);
        let index = n(&payload["contentBlockIndex"]);
        match frame.headers.get(":event-type").map(String::as_str).unwrap_or("") {
            "contentBlockStart" => {
                if let Some(tool) = payload["start"]["toolUse"].as_object() {
                    let name = tool["name"].as_str().unwrap_or("");
                    if self.sentinel && name == NO_TOOLS {
                        self.dropped.insert(index);
                    } else {
                        self.blocks.insert(index, Partial::Tool {
                            id: tool["toolUseId"].as_str().unwrap_or("").into(),
                            name: name.into(),
                            json: String::new(),
                        });
                    }
                }
            }
            "contentBlockDelta" => {
                let delta = &payload["delta"];
                if let Some(text) = delta["text"].as_str().filter(|t| !t.is_empty()) {
                    match self.blocks.entry(index).or_insert_with(|| Partial::Text(String::new())) {
                        Partial::Text(current) => {
                            current.push_str(text);
                            on_text(text);
                        }
                        _ => {}
                    }
                } else if let Some(input) = delta["toolUse"]["input"].as_str() {
                    if let Some(Partial::Tool { name, json, .. }) = self.blocks.get_mut(&index) {
                        json.push_str(input);
                        // `edit.streamingAbort`: stop reading a reply whose streamed edit already fails.
                        if let Some(reason) = crate::tools::stream_edit_abort(name, json) {
                            bail!(reason);
                        }
                    }
                } else if let Some(reasoning) = delta["reasoningContent"].as_object() {
                    let block = self.blocks.entry(index).or_insert_with(|| Partial::Thinking { text: String::new(), signature: String::new() });
                    if let Partial::Thinking { text, signature } = block {
                        if let Some(delta) = reasoning["text"].as_str() {
                            text.push_str(delta);
                        }
                        if let Some(delta) = reasoning["signature"].as_str() {
                            signature.push_str(delta);
                        }
                    }
                }
            }
            "messageStop" => {
                let reason = payload["stopReason"].as_str().unwrap_or("");
                self.stop = match reason {
                    "end_turn" | "stop_sequence" => "end_turn",
                    "max_tokens" | "model_context_window_exceeded" => "max_tokens",
                    // A sentinel-only request must not read as a tool turn: no real tool exists.
                    "tool_use" if self.sentinel => "end_turn",
                    "tool_use" => "tool_use",
                    "guardrail_intervened" => bail!("Bedrock のガードレールにより応答がブロックされました"),
                    "content_filtered" => bail!("Bedrock のコンテンツフィルタにより応答が除外されました"),
                    other => bail!("Bedrock が応答を生成できませんでした（stop reason: {other}）"),
                }
                .into();
            }
            "metadata" => {
                let usage = &payload["usage"];
                let cache_read = n(&usage["cacheReadInputTokens"]);
                let cache_write = n(&usage["cacheWriteInputTokens"]);
                // `inputTokens` counts the whole prompt, cached spans included.
                self.usage = Usage {
                    input: n(&usage["inputTokens"]).saturating_sub(cache_read).saturating_sub(cache_write),
                    output: n(&usage["outputTokens"]),
                    cache_read,
                    cache_write,
                };
            }
            _ => {}
        }
        Ok(())
    }

    fn finish(self) -> Result<Reply> {
        let mut blocks = Vec::new();
        for (index, partial) in self.blocks {
            if self.dropped.contains(&index) {
                continue;
            }
            match partial {
                Partial::Text(text) if !text.is_empty() => blocks.push(Block::Text { text }),
                Partial::Text(_) => {}
                Partial::Thinking { text, signature } if !text.is_empty() || !signature.is_empty() => {
                    blocks.push(Block::Other(json!({"type": "thinking", "thinking": text, "signature": signature})));
                }
                Partial::Thinking { .. } => {}
                Partial::Tool { id, name, json } => {
                    let input = if json.trim().is_empty() {
                        json!({})
                    } else {
                        match serde_json::from_str::<Value>(&json) {
                            Ok(v) if v.is_object() => v,
                            _ => json!({"__invalid_json": json}),
                        }
                    };
                    blocks.push(Block::ToolUse { id, name, input });
                }
            }
        }
        let uses_tools = blocks.iter().any(|b| matches!(b, Block::ToolUse { .. }));
        let stop = if self.stop.is_empty() {
            if uses_tools { "tool_use" } else { "end_turn" }
        } else if uses_tools && self.stop == "end_turn" {
            "tool_use"
        } else {
            &self.stop
        };
        Ok(Reply { blocks, usage: self.usage, stop: stop.into() })
    }
}

/// Bedrock's regional runtime host; the region segment is re-derived so signing and routing agree.
fn runtime_base(base: &str, region: &str) -> String {
    let Ok(mut url) = url::Url::parse(base) else { return base.to_string() };
    let regional = url.host_str().is_some_and(|host| {
        host.strip_prefix("bedrock-runtime.")
            .and_then(|rest| rest.strip_suffix(".amazonaws.com"))
            .is_some_and(|segment| !segment.is_empty() && !segment.contains('.'))
    });
    if regional {
        let _ = url.set_host(Some(&format!("bedrock-runtime.{region}.amazonaws.com")));
    }
    url.to_string()
}

/// The catalog spells Mantle's host with a `{region}` template; a literal base is kept as written.
fn resolve_base(base: &str, region: &str) -> String {
    base.replace("{region}", region)
}

/// `{base}/model/{id}/converse-stream`, keeping any path prefix and query a gateway base carries.
fn converse_url(base: &str, model: &str) -> Result<String> {
    let mut url = url::Url::parse(base).map_err(|e| anyhow!("Bedrock の base_url が不正です（{base}）: {e}"))?;
    let prefix = url.path().trim_end_matches('/').to_string();
    url.set_path(&format!("{prefix}/model/{}/converse-stream", aws_sigv4::encode(model)));
    Ok(url.to_string())
}

/// Cross-region inference profiles are only servable from their own geo.
const GEOS: &[(&str, &str)] = &[
    ("us", "us-east-1"),
    ("us-gov", "us-gov-west-1"),
    ("eu", "eu-west-1"),
    ("apac", "ap-southeast-1"),
    ("au", "ap-southeast-2"),
    ("jp", "ap-northeast-1"),
];

fn geo(model: &str) -> Option<&'static str> {
    let prefix = model.split_once('.')?.0;
    GEOS.iter().find(|(geo, _)| *geo == prefix).map(|(geo, _)| *geo)
}

fn serves(region: &str, geo: &str) -> bool {
    match geo {
        "us-gov" => region.starts_with("us-gov-"),
        "us" => region.starts_with("us-") && !region.starts_with("us-gov-"),
        "eu" => region.starts_with("eu-"),
        "apac" => region.starts_with("ap-"),
        "au" => region == "ap-southeast-2" || region == "ap-southeast-4",
        "jp" => region == "ap-northeast-1" || region == "ap-northeast-3",
        _ => false,
    }
}

/// The region one Converse request signs and routes with.
fn region_for(model: &str, ambient: Option<String>, base: &str) -> String {
    if let Some(region) = arn_region(model) {
        return region;
    }
    if let Some(geo) = geo(model) {
        if let Some(region) = ambient.filter(|region| serves(region, geo)) {
            return region;
        }
        return GEOS.iter().find(|(candidate, _)| *candidate == geo).map(|(_, region)| *region).unwrap_or("us-east-1").into();
    }
    ambient.or_else(|| region_from_host(base)).unwrap_or_else(|| "us-east-1".into())
}

fn arn_region(model: &str) -> Option<String> {
    let parts: Vec<&str> = model.split(':').take(4).collect();
    (parts.len() == 4 && parts[0] == "arn" && parts[2] == "bedrock" && !parts[3].is_empty()).then(|| parts[3].to_string())
}

fn region_from_host(base: &str) -> Option<String> {
    let host = url::Url::parse(base).ok()?.host_str()?.to_string();
    let region = host.strip_prefix("bedrock-runtime.").or_else(|| host.strip_prefix("bedrock-mantle."))?;
    let region = region.strip_suffix(".amazonaws.com").or_else(|| region.strip_suffix(".api.aws"))?;
    (!region.is_empty() && !region.contains('.') && region != "{region}").then(|| region.to_string())
}

/// `claude-opus-4-6` → `(4, 6)`; older ids spell it `claude-3-7-sonnet`.
pub fn claude_version(id: &str) -> Option<(u32, u32)> {
    let rest = id.split("claude-").nth(1)?;
    let mut parts = rest.split('-');
    let first = parts.next()?;
    if let Ok(major) = first.parse::<u32>() {
        return Some((major, parts.next().and_then(|p| p.parse().ok()).unwrap_or(0)));
    }
    let major = parts.next()?.parse().ok()?;
    Some((major, parts.next().and_then(|p| p.parse().ok()).unwrap_or(0)))
}

fn unix_ms() -> u128 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |elapsed| elapsed.as_millis())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Message, ToolDef};

    fn bedrock(effort: Option<&str>, wire: Wire) -> Bedrock {
        Bedrock { http: reqwest::Client::new(), base: "https://bedrock-runtime.us-east-1.amazonaws.com".into(), key: "k".into(), effort: effort.map(String::from), wire }
    }

    fn request<'a>(model: &'a str, messages: &'a [Message], tools: &'a [ToolDef]) -> Request<'a> {
        Request { model, system: "be brief", messages, tools, max_tokens: 4096, live: None }
    }

    fn tool(name: &str) -> ToolDef {
        ToolDef { name: name.into(), description: "does a thing".into(), schema: json!({"type": "object", "properties": {}}) }
    }

    #[test]
    fn converse_body_carries_tool_results_in_one_user_message() {
        let messages = [
            Message::user("read it"),
            Message { role: Role::Assistant, content: vec![Block::ToolUse { id: "call_1".into(), name: "read".into(), input: json!({"path": "a"}) }] },
            Message { role: Role::User, content: vec![Block::ToolResult { tool_use_id: "call_1".into(), content: "contents".into(), is_error: false }] },
            Message::user("and now summarize"),
        ];
        let client = bedrock(None, Wire::default());
        let (body, sentinel) = client.body(&request("us.anthropic.claude-opus-5-5", &messages, &[tool("read")]));
        let wire = body["messages"].as_array().unwrap();
        assert_eq!(wire.len(), 3, "the tool result and the following text are one user turn: {body}");
        assert_eq!(wire[1]["content"][0]["toolUse"]["toolUseId"], "call_1");
        assert_eq!(wire[2]["content"][0]["toolResult"]["status"], "success");
        assert_eq!(wire[2]["content"][1]["text"], "and now summarize");
        assert!(!sentinel);
        assert_eq!(body["system"][0]["text"], "be brief");
        // Claude accepts cache points, so the system and the final user turn carry one by default.
        assert_eq!(body["system"][1]["cachePoint"]["type"], "default");
        assert_eq!(wire[2]["content"][2]["cachePoint"]["type"], "default");
        assert_eq!(body["toolConfig"]["tools"][0]["toolSpec"]["name"], "read");
        assert_eq!(body["toolConfig"]["tools"][0]["toolSpec"]["inputSchema"]["json"]["type"], "object");
        assert_eq!(body["inferenceConfig"]["maxTokens"], 4096);
        assert!(body.get("additionalModelRequestFields").is_none(), "no effort, no reasoning block");
    }

    #[test]
    fn a_tool_history_without_tools_gets_the_validation_sentinel() {
        let messages = [
            Message::user("q"),
            Message { role: Role::Assistant, content: vec![Block::ToolUse { id: "call_1".into(), name: "read".into(), input: json!({}) }] },
            Message { role: Role::User, content: vec![Block::ToolResult { tool_use_id: "call_1".into(), content: "x".into(), is_error: true }] },
        ];
        let (body, sentinel) = bedrock(None, Wire::default()).body(&request("amazon.nova-pro", &messages, &[]));
        assert!(sentinel);
        assert_eq!(body["toolConfig"]["tools"][0]["toolSpec"]["name"], NO_TOOLS);
        assert_eq!(body["messages"][2]["content"][0]["toolResult"]["status"], "error");
        assert!(body["system"].as_array().is_none() || body["system"][0]["text"] == "be brief");
        // Nova is not Claude: no cache point may be sent.
        assert!(body["system"].as_array().is_some_and(|system| system.len() == 1));
        assert!(body["messages"][2]["content"].as_array().unwrap().len() == 1);
    }

    #[test]
    fn thinking_follows_the_model_generation() {
        let messages = [Message::user("hi")];
        let adaptive = bedrock(Some("high"), Wire::default()).body(&request("us.anthropic.claude-opus-5-5", &messages, &[])).0;
        assert_eq!(adaptive["additionalModelRequestFields"]["thinking"]["type"], "adaptive");
        assert_eq!(adaptive["additionalModelRequestFields"]["thinking"]["display"], "summarized");
        assert_eq!(adaptive["additionalModelRequestFields"]["output_config"]["effort"], "high");

        let budget = bedrock(Some("high"), Wire::default()).body(&request("anthropic.claude-3-7-sonnet-20250219-v1:0", &messages, &[])).0;
        assert_eq!(budget["additionalModelRequestFields"]["thinking"]["type"], "enabled");
        assert_eq!(budget["additionalModelRequestFields"]["thinking"]["budget_tokens"], 16384);

        let effort = bedrock(Some("medium"), Wire::default()).body(&request("openai.gpt-5.6-terra", &messages, &[])).0;
        assert_eq!(effort["additionalModelRequestFields"]["reasoning"]["effort"], "medium");

        let none = bedrock(None, Wire::default()).body(&request("us.anthropic.claude-opus-5-5", &messages, &[])).0;
        assert!(none.get("additionalModelRequestFields").is_none());
    }

    #[test]
    fn regions_follow_the_inference_profile_geo() {
        assert_eq!(region_for("eu.anthropic.claude-sonnet-5-5", Some("us-west-2".into()), ""), "eu-west-1");
        assert_eq!(region_for("eu.anthropic.claude-sonnet-5-5", Some("eu-central-1".into()), ""), "eu-central-1");
        assert_eq!(region_for("us.anthropic.claude-opus-5-5", Some("us-gov-west-1".into()), ""), "us-east-1");
        assert_eq!(region_for("global.anthropic.claude-opus-5-5", Some("ap-northeast-1".into()), ""), "ap-northeast-1");
        assert_eq!(region_for("arn:aws:bedrock:eu-west-3:123:inference-profile/x", Some("us-east-1".into()), ""), "eu-west-3");
        assert_eq!(region_for("anthropic.claude-opus-5-5", None, "https://bedrock-runtime.eu-west-2.amazonaws.com"), "eu-west-2");
        assert_eq!(region_for("anthropic.claude-opus-5-5", None, "https://proxy.test"), "us-east-1");
        assert_eq!(runtime_base("https://bedrock-runtime.us-east-1.amazonaws.com", "eu-west-1"), "https://bedrock-runtime.eu-west-1.amazonaws.com/");
        // A gateway base keeps its own host.
        assert_eq!(runtime_base("https://proxy.test/aws", "eu-west-1"), "https://proxy.test/aws");
    }

    #[test]
    fn the_request_url_keeps_gateway_prefixes_and_escapes_the_model_id() {
        assert_eq!(
            converse_url("https://bedrock-runtime.eu-west-1.amazonaws.com", "us.anthropic.claude-opus-5-5").unwrap(),
            "https://bedrock-runtime.eu-west-1.amazonaws.com/model/us.anthropic.claude-opus-5-5/converse-stream"
        );
        assert_eq!(
            converse_url("https://gw.test/aws?token=1", "anthropic.claude-3-7-sonnet-v1:0").unwrap(),
            "https://gw.test/aws/model/anthropic.claude-3-7-sonnet-v1%3A0/converse-stream?token=1"
        );
    }

    /// A local server answering one eventstream body, and asserting the request it got.
    async fn converse_server(chunks: Vec<Vec<u8>>) -> (String, std::sync::Arc<std::sync::Mutex<Option<crate::localserver::Request>>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let seen = std::sync::Arc::new(std::sync::Mutex::new(None));
        let captured = seen.clone();
        tokio::spawn(async move {
            let _ = crate::localserver::serve(listener, 1 << 20, move |request| {
                let captured = captured.clone();
                let chunks = chunks.clone();
                async move {
                    *captured.lock().unwrap() = Some(request);
                    crate::localserver::Response::new("200 OK", EVENTSTREAM, chunks.concat())
                }
            })
            .await;
        });
        (base, seen)
    }

    #[tokio::test]
    async fn a_converse_stream_becomes_blocks_with_usage() {
        let mut body = Vec::new();
        let mut event = |headers: Vec<(&str, &str)>, payload: Value| {
            let headers: Vec<(&str, &str)> =
                headers.into_iter().chain([(":message-type", "event"), (":content-type", "application/json")]).collect();
            body.extend_from_slice(&aws_eventstream::encode(&headers, payload.to_string().as_bytes()));
        };
        event(vec![(":event-type", "messageStart")], json!({"role": "assistant"}));
        event(vec![(":event-type", "contentBlockStart")], json!({"contentBlockIndex": 0, "start": {"toolUse": {"toolUseId": "call_1", "name": "read"}}}));
        event(vec![(":event-type", "contentBlockDelta")], json!({"contentBlockIndex": 0, "delta": {"toolUse": {"input": "{\"path\":"}}}));
        event(vec![(":event-type", "contentBlockDelta")], json!({"contentBlockIndex": 0, "delta": {"toolUse": {"input": "\"a.rs\"}"}}}));
        event(vec![(":event-type", "contentBlockDelta")], json!({"contentBlockIndex": 1, "delta": {"text": "hello"}}));
        event(vec![(":event-type", "contentBlockDelta")], json!({"contentBlockIndex": 2, "delta": {"reasoningContent": {"text": "because", "signature": "sig"}}}));
        event(vec![(":event-type", "contentBlockStop")], json!({"contentBlockIndex": 0}));
        event(vec![(":event-type", "contentBlockStop")], json!({"contentBlockIndex": 1}));
        event(vec![(":event-type", "contentBlockStop")], json!({"contentBlockIndex": 2}));
        event(vec![(":event-type", "messageStop")], json!({"stopReason": "tool_use"}));
        event(vec![(":event-type", "metadata")], json!({"usage": {"inputTokens": 100, "outputTokens": 10, "cacheReadInputTokens": 40, "cacheWriteInputTokens": 5}}));
        // Split the body across chunk boundaries to exercise the decoder's buffering.
        let cut = body.len() / 3;
        let (base, seen) = converse_server(vec![body[..cut].to_vec(), body[cut..2 * cut].to_vec(), body[2 * cut..].to_vec()]).await;

        let client = Bedrock { http: reqwest::Client::new(), base, key: "api-key".into(), effort: None, wire: Wire::default() };
        let messages = [Message::user("read a.rs")];
        let tools = [tool("read")];
        let streamed = std::sync::Mutex::new(String::new());
        let reply = client
            .complete(request("us.anthropic.claude-opus-5-5", &messages, &tools), &|text| streamed.lock().unwrap().push_str(text))
            .await
            .expect("converse stream");

        assert_eq!(streamed.into_inner().unwrap(), "hello");
        assert_eq!(reply.stop, "tool_use");
        assert_eq!(reply.usage.input, 55);
        assert_eq!(reply.usage.cache_read, 40);
        assert_eq!(reply.usage.cache_write, 5);
        assert!(reply.blocks.iter().any(|b| matches!(b, Block::Text { text } if text == "hello")));
        assert!(reply.blocks.iter().any(|b| matches!(b, Block::ToolUse { id, name, input }
            if id == "call_1" && name == "read" && input["path"] == "a.rs")));
        assert!(reply.blocks.iter().any(|b| matches!(b, Block::Other(v)
            if v["type"] == "thinking" && v["thinking"] == "because" && v["signature"] == "sig")));

        let seen = seen.lock().unwrap().take().expect("the server saw a request");
        assert_eq!(seen.method, "POST");
        assert_eq!(seen.path, "/model/us.anthropic.claude-opus-5-5/converse-stream");
        assert_eq!(seen.header("authorization"), Some("Bearer api-key"));
        assert_eq!(seen.header("accept"), Some(EVENTSTREAM));
        assert_eq!(seen.header("content-type"), Some("application/json"));
        let sent: Value = serde_json::from_slice(&seen.body).unwrap();
        assert_eq!(sent["messages"][0]["content"][0]["text"], "read a.rs");
        assert_eq!(sent["toolConfig"]["tools"][0]["toolSpec"]["name"], "read");
    }

    #[tokio::test]
    async fn an_exception_frame_fails_the_turn_in_japanese() {
        let frame = aws_eventstream::encode(
            &[(":message-type", "exception"), (":exception-type", "ThrottlingException")],
            br#"{"message":"slow down"}"#,
        );
        let (base, _) = converse_server(vec![frame]).await;
        let client = Bedrock { http: reqwest::Client::new(), base, key: "api-key".into(), effort: None, wire: Wire::default() };
        let messages = [Message::user("hi")];
        let error = client.complete(request("amazon.nova-pro", &messages, &[]), &|_| {}).await.unwrap_err().to_string();
        assert!(error.contains("ThrottlingException") && error.contains("slow down"), "{error}");
    }

    /// A local server answering one SSE body.
    async fn sse_server(events: Vec<Value>) -> (String, std::sync::Arc<std::sync::Mutex<Option<crate::localserver::Request>>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let seen = std::sync::Arc::new(std::sync::Mutex::new(None));
        let captured = seen.clone();
        tokio::spawn(async move {
            let _ = crate::localserver::serve(listener, 1 << 20, move |request| {
                let captured = captured.clone();
                let events = events.clone();
                async move {
                    *captured.lock().unwrap() = Some(request);
                    let body: String = events.iter().map(|event| format!("data: {event}\n\n")).collect();
                    crate::localserver::Response::new("200 OK", "text/event-stream", body)
                }
            })
            .await;
        });
        (base, seen)
    }

    #[tokio::test]
    async fn mantle_speaks_responses_and_replaces_the_region_placeholder() {
        let (base, seen) = sse_server(vec![
            json!({"type": "response.created", "response": {"id": "r1"}}),
            json!({"type": "response.output_text.delta", "delta": "terra"}),
            json!({"type": "response.completed", "response": {"usage": {"input_tokens": 12, "output_tokens": 3}}}),
        ])
        .await;
        let client = Mantle {
            http: reqwest::Client::new(),
            base: format!("{base}/openai/v1"),
            key: "bedrock-key".into(),
            effort: Some("high".into()),
            wire: Wire::default(),
        };
        let messages = [Message::user("hi")];
        let reply = client.complete(request("openai.gpt-5.6-terra", &messages, &[]), &|_| {}).await.expect("mantle stream");
        assert!(reply.blocks.iter().any(|b| matches!(b, Block::Text { text } if text == "terra")));
        assert_eq!(reply.usage.input, 12);

        let seen = seen.lock().unwrap().take().unwrap();
        assert_eq!(seen.path, "/openai/v1/responses");
        assert_eq!(seen.header("authorization"), Some("Bearer bedrock-key"));
        let sent: Value = serde_json::from_slice(&seen.body).unwrap();
        assert_eq!(sent["model"], "openai.gpt-5.6-terra");
        assert_eq!(sent["reasoning"]["effort"], "high");
        assert_eq!(sent["stream"], true);
    }

    #[test]
    fn the_region_template_only_touches_the_region_segment() {
        // The `{region}` substitution is the only templating the catalog uses; a literal host is kept.
        assert_eq!(resolve_base("https://bedrock-mantle.{region}.api.aws/openai/v1", "eu-west-1"), "https://bedrock-mantle.eu-west-1.api.aws/openai/v1");
        assert_eq!(resolve_base("https://proxy.test/openai/v1", "eu-west-1"), "https://proxy.test/openai/v1");
    }

    #[test]
    fn mantle_regions_come_from_the_configured_endpoint() {
        // The Mantle preset carries the region in its host, so a US endpoint signs us-east-1.
        assert_eq!(region_for("openai.gpt-5.6-terra", None, "https://bedrock-mantle.us-east-1.api.aws/openai/v1"), "us-east-1");
        // `{region}` is unresolved until the base is rendered, so the ambient region decides.
        assert_eq!(region_for("openai.gpt-5.6-terra", None, "https://bedrock-mantle.{region}.api.aws/openai/v1"), "us-east-1");
        assert_eq!(region_for("openai.gpt-5.6-terra", Some("eu-west-1".into()), "https://bedrock-mantle.{region}.api.aws/openai/v1"), "eu-west-1");
    }
}
