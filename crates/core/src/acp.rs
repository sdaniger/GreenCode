//! Drives an external agent CLI (Claude Code, Codex, ...) over the Agent Client Protocol.

use crate::{
    jsonrpc::{Incoming, Rpc},
    permission::{resolve, Mode, Perms},
    session::{Meta, Session},
    Backend, Block, BoxFut, Decision, Event, EventTx, Message, Role,
};
use anyhow::{anyhow, Result};
use serde_json::{json, Value};
use std::{path::PathBuf, sync::Arc};
use tokio::sync::{mpsc, oneshot};

pub struct Acp {
    rpc: Arc<Rpc>,
    incoming: mpsc::UnboundedReceiver<Incoming>,
    session_id: String,
    cwd: PathBuf,
    perms: Arc<Perms>,
    meta: Meta,
    /// What the agent did this turn, saved so the thread can be shown again later.
    log: Vec<Block>,
    state: Value,
    prompt_text: Option<String>,
    /// Latest slash commands the agent advertised; re-sent because the UI may subscribe after they arrive.
    commands: Option<Value>,
    /// Messages taken off the wire (by `idle`, or requests seen during startup) awaiting `handle_idle`.
    idle_inc: std::collections::VecDeque<Incoming>,
    /// The agent accepts images in prompts (`promptCapabilities.image`).
    images: bool,
    /// GreenCode's MCP server was attached and its usage guide is not sent yet; it goes before the next prompt.
    guide: bool,
}

impl Acp {
    pub async fn start(command: &[String], env: &std::collections::BTreeMap<String, String>, meta: &Meta, perms: Arc<Perms>) -> Result<Acp> {
        let saved = Session::runtime_for_backend(&meta.id, &meta.backend)?;
        // `auto` names the sandbox as the runtime's fallback; GreenCode also restarts the runtime on it when
        // production fails while the thread opens. `production`/`sandbox` never fall back.
        let Some(sandbox) = env.get("CODE_ASSIST_ENDPOINT_FALLBACK") else {
            return Self::start_env(command, env, &saved, meta, perms).await;
        };
        let mut backup = env.clone();
        backup.insert("CODE_ASSIST_ENDPOINT".into(), sandbox.clone());
        backup.remove("CODE_ASSIST_ENDPOINT_FALLBACK");
        let first = match Self::start_env(command, env, &saved, meta, perms.clone()).await {
            Ok(acp) => return Ok(acp),
            Err(err) => err,
        };
        if !endpoint_failure(&first) {
            return Err(first);
        }
        Self::start_env(command, &backup, &saved, meta, perms)
            .await
            .map_err(|err| err.context(format!("production endpoint failed first: {first:#}")))
    }

    async fn start_env(command: &[String], env: &std::collections::BTreeMap<String, String>, saved: &Value, meta: &Meta, perms: Arc<Perms>) -> Result<Acp> {
        // `op://` values in `[[backends]] env` are read here so an error names the backend.
        let who = format!("backend {}", meta.backend);
        let env = env.iter().map(|(k, v)| Ok((k.clone(), crate::secrets::resolve(v, &who)?))).collect::<Result<Vec<_>>>()?;
        let command = if crate::antigravity::is_alias(&meta.backend) { crate::antigravity::command_for(command, &env.iter().cloned().collect()) } else { command.to_vec() };
        let (rpc, incoming, mut out_rx) = Rpc::spawn_with_output(&command, &env, &meta.cwd)?;
        let init = rpc.request("initialize", json!({
            "protocolVersion": 1,
            "clientCapabilities": {"fs": {"readTextFile": true, "writeTextFile": true}},
        }))
        .await?;
        let images = init["agentCapabilities"]["promptCapabilities"]["image"] == true;
        let mut mcp = stdio_mcp_servers(&meta.cwd);
        let delegation = crate::delegate::acp_server(meta, &init).await;
        let guide = delegation.is_some();
        mcp.extend(delegation);
        let (session_id, r) = match open_session(&rpc, saved, &init, meta, &mcp).await {
            Ok(opened) => {
                if crate::antigravity::is_alias(&meta.backend) {
                    let _ = crate::antigravity::mark_signed_in(true, None);
                }
                opened
            }
            Err(err) if crate::antigravity::is_auth_required(&err) => {
                let preferred = if crate::antigravity::is_alias(&meta.backend) {
                    crate::antigravity::status().auth_type
                } else {
                    "oauth-personal".into()
                };
                let Some(method_id) = crate::antigravity::select_method_id(&init["authMethods"], &preferred) else {
                    return Err(err);
                };
                let auth_fut = rpc.request("authenticate", json!({"methodId": method_id}));
                tokio::pin!(auth_fut);
                let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(15);
                let mut out_open = true;
                loop {
                    tokio::select! {
                        res = &mut auth_fut => {
                            res?;
                            break;
                        }
                        maybe_line = out_rx.recv(), if out_open => {
                            match maybe_line {
                                Some(line) if crate::antigravity::extract_auth_url(&line).is_some() => {
                                    if crate::antigravity::is_alias(&meta.backend) {
                                        let _ = crate::antigravity::mark_signed_in(false, None);
                                    }
                                    anyhow::bail!("Google Antigravity にサインインしていません。/login antigravity を実行するか、設定 → 接続 からサインインしてください");
                                }
                                Some(_) => {}
                                None => out_open = false,
                            }
                        }
                        _ = tokio::time::sleep_until(deadline) => {
                            if crate::antigravity::is_alias(&meta.backend) {
                                let _ = crate::antigravity::mark_signed_in(false, None);
                            }
                            anyhow::bail!("Google Antigravity にサインインしていません。/login antigravity を実行するか、設定 → 接続 からサインインしてください");
                        }
                    }
                }
                if crate::antigravity::is_alias(&meta.backend) {
                    let _ = crate::antigravity::mark_signed_in(true, Some(&method_id));
                }
                open_session(&rpc, saved, &init, meta, &mcp).await?
            }
            Err(err) => return Err(err),
        };
        Session::save_runtime(&meta.id, &json!({"backend":meta.backend,"sessionId":session_id}))?;
        let mut acp = Acp { rpc, incoming, session_id, cwd: meta.cwd.clone(), perms, meta: meta.clone(), log: vec![], state: r, prompt_text:None, commands:None, idle_inc:Default::default(), images, guide };
        // Agents advertise slash commands in a notification sent right after the session/new|load response, and
        // session/load also replays its transcript (which the UI already has). Collect until the stream goes
        // quiet so those updates are neither missed nor mistaken for the first turn's output; keep only state.
        let (sink, _) = mpsc::unbounded_channel();
        let quiet = std::time::Duration::from_millis(250);
        let _ = tokio::time::timeout(std::time::Duration::from_secs(3), async {
            while let Ok(Some(inc)) = tokio::time::timeout(quiet, acp.incoming.recv()).await {
                let state = is_state_update(&inc);
                // Requests need the user's answer, so they wait for `handle_idle` instead of going to the sink.
                if state { acp.handle(inc, &sink).await; } else if matches!(inc, Incoming::Request { .. }) { acp.idle_inc.push_back(inc); }
            }
        }).await;
        if !meta.model.is_empty() && meta.model != "default" {
            let (tx, _) = mpsc::unbounded_channel();
            acp.control(json!({"type":"select_model","model":meta.model}), &tx).await?;
        }
        Ok(acp)
    }

    fn text(&mut self, t: &str) {
        match self.log.last_mut() {
            Some(Block::Text { text }) => text.push_str(t),
            _ => self.log.push(Block::Text { text: t.into() }),
        }
    }

    /// The prompt as ACP content blocks: image paths become image blocks only when the agent advertised support;
    /// otherwise the path stays in the text, as before.
    fn prompt_content(&self, text: &str) -> Vec<Value> {
        let mut blocks = vec![json!({"type": "text", "text": text})];
        if !self.images {
            return blocks;
        }
        for block in crate::context::attachments(text, &self.cwd) {
            let Block::Other(v) = &block else { continue };
            if let (Some(data), Some(mime)) = (v["source"]["data"].as_str(), v["source"]["media_type"].as_str()) {
                blocks.push(json!({"type": "image", "data": data, "mimeType": mime}));
            }
        }
        blocks
    }

    async fn handle(&mut self, inc: Incoming, tx: &EventTx) {
        match inc {
            Incoming::Notification { method, params } if method == "session/update" => {
                let u = &params["update"];
                match u["sessionUpdate"].as_str().unwrap_or("") {
                    "agent_message_chunk" => {
                        if let Some(t) = u["content"]["text"].as_str() {
                            self.text(t);
                            let _ = tx.send(Event::Text(t.into()));
                        } else if let Some(placeholder) = placeholder(&u["content"]) {
                            self.text(&placeholder);
                            let _ = tx.send(Event::Text(placeholder));
                        }
                    }
                    "agent_thought_chunk" => {
                        if let Some(t) = u["content"]["text"].as_str() {
                            let _ = tx.send(Event::Thinking(t.into()));
                        }
                    }
                    kind @ ("tool_call" | "tool_call_update") => {
                        let id = u["toolCallId"].as_str().unwrap_or("").to_string();
                        if kind == "tool_call" {
                            let name = u["title"].as_str().unwrap_or("tool").to_string();
                            let input = if u["rawInput"].is_object() { u["rawInput"].clone() } else { json!({}) };
                            self.log.push(Block::ToolUse { id: id.clone(), name: name.clone(), input: input.clone() });
                            let _ = tx.send(Event::ToolStart { id: id.clone(), name, input });
                        } else if u["rawInput"].as_object().is_some_and(|o| !o.is_empty()) {
                            // The call opens with a stub input (opencode: only `cwd`); the real arguments arrive in an update.
                            let stored = self.log.iter_mut().rev().find_map(|b| match b {
                                Block::ToolUse { id: i, name, input } if *i == id => Some((name.clone(), input)),
                                _ => None,
                            });
                            if let Some((name, input)) = stored {
                                if *input != u["rawInput"] {
                                    *input = u["rawInput"].clone();
                                    let _ = tx.send(Event::ToolStart { id: id.clone(), name, input: input.clone() });
                                }
                            }
                        }
                        let status = u["status"].as_str().unwrap_or("");
                        if status == "completed" || status == "failed" {
                            let parts: Vec<String> = u["content"]
                                .as_array()
                                .into_iter()
                                .flatten()
                                .filter_map(|c| match c["type"].as_str() {
                                    Some("diff") => Some(format!("edited {}", c["path"].as_str().unwrap_or(""))),
                                    _ => c["content"]["text"].as_str().map(String::from).or_else(|| placeholder(&c["content"])),
                                })
                                .collect();
                            let (output, is_error) = (parts.join("\n"), status == "failed");
                            self.log.push(Block::ToolResult { tool_use_id: id.clone(), content: output.clone(), is_error });
                            let _ = tx.send(Event::ToolEnd { id, output, is_error });
                        }
                    }
                    "config_option_update" => {
                        self.state["configOptions"] = u["configOptions"].clone();
                        let _ = tx.send(Event::Remote { command:"acp_state".into(), data:self.state.clone() });
                    }
                    "available_commands_update" => {
                        let data = json!({"commands":u["availableCommands"]});
                        self.commands = Some(data.clone());
                        let _ = tx.send(Event::Remote { command:"get_commands".into(), data });
                    }
                    "usage_update" => {
                        let _ = tx.send(Event::Remote { command:"usage_update".into(), data:u.clone() });
                    }
                    _ => {}
                }
            }
            Incoming::Notification { .. } => {}
            Incoming::Request { id, method, params } => {
                let result = match method.as_str() {
                    "session/request_permission" => Ok(self.permission(&params, tx).await),
                    "fs/read_text_file" | "fs/write_text_file" => self.file_request(&method, &params, tx).await,
                    m => Err(format!("method `{m}` is not supported")),
                };
                let _ = self.rpc.respond(id, result).await;
            }
        }
    }

    async fn file_request(&self, method: &str, params: &Value, tx: &EventTx) -> Result<Value, String> {
        let file = params["path"].as_str().filter(|p| !p.is_empty()).ok_or("missing file path")?;
        let tool = if method == "fs/read_text_file" { "read" } else { "write" };
        if tool == "write" && self.perms.mode() == Mode::Plan { return Err("plan mode is on: writes are refused".into()); }
        match self.perms.verdict(tool, params, &self.cwd) {
            crate::permission::Verdict::Deny(reason) => return Err(reason),
            crate::permission::Verdict::Ask(ask) => {
                let (reply, rx) = oneshot::channel();
                let _ = tx.send(Event::Permission { tool:tool.into(), summary:file.into(), reply });
                match rx.await.unwrap_or(Decision::Deny) {
                    Decision::Allow => {}
                    Decision::Always if ask.remember => self.perms.always(ask.key),
                    Decision::Always => {}
                    Decision::Deny => return Err("the user denied this file access".into()),
                }
            }
            crate::permission::Verdict::Allow => {}
        }
        match method {
                    "fs/read_text_file" => {
                        let path = resolve(&self.cwd, params["path"].as_str().unwrap_or(""));
                        std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display())).map(|s| {
                            let start = params["line"].as_u64().unwrap_or(1).max(1) as usize - 1;
                            let content = match params["limit"].as_u64() {
                                None if start == 0 => s,
                                limit => s.lines().skip(start).take(limit.unwrap_or(u64::MAX) as usize).collect::<Vec<_>>().join("\n"),
                            };
                            json!({"content": content})
                        })
                    }
                    "fs/write_text_file" => {
                        let path = resolve(&self.cwd, params["path"].as_str().unwrap_or(""));
                        std::fs::write(&path, params["content"].as_str().unwrap_or(""))
                            .map(|_| Value::Null)
                            .map_err(|e| format!("{}: {e}", path.display()))
                    }
                    _ => unreachable!(),
        }
    }

    async fn permission(&self, params: &Value, tx: &EventTx) -> Value {
        let call = &params["toolCall"];
        // Same rules as the native tools: the mode, "always allow" keys and the project boundary decide.
        let path = call["rawInput"]["path"].as_str().or_else(|| call["locations"][0]["path"].as_str()).unwrap_or("");
        let (tool, input) = match call["kind"].as_str() {
            Some("execute") => ("bash", call["rawInput"].clone()),
            Some("edit" | "delete" | "move") => ("write", json!({"path": path})),
            Some("read" | "search") => ("read", json!({"path": path})),
            _ => (call["title"].as_str().unwrap_or("tool"), call["rawInput"].clone()),
        };
        let decision = if self.perms.mode() == Mode::Plan && !matches!(tool, "read") {
            Decision::Deny
        } else {
            match self.perms.verdict(tool, &input, &self.cwd) {
                crate::permission::Verdict::Allow => Decision::Allow,
                crate::permission::Verdict::Deny(_) => Decision::Deny,
                crate::permission::Verdict::Ask(ask) => {
                    let (reply, rx) = oneshot::channel();
                    let _ = tx.send(Event::Permission {
                        tool: call["title"].as_str().unwrap_or("tool").into(),
                        summary: crate::permission::summary(call["title"].as_str().filter(|_| tool != "bash").unwrap_or(tool), &call["rawInput"]),
                        reply,
                    });
                    match rx.await.unwrap_or(Decision::Deny) {
                        Decision::Always if ask.remember => { self.perms.always(ask.key); Decision::Always }
                        d => d,
                    }
                }
            }
        };
        let wanted: &[&str] = match decision {
            Decision::Always => &["allow_always", "allow_once"],
            Decision::Allow => &["allow_once"],
            Decision::Deny => &["reject_once"],
        };
        let options = params["options"].as_array().cloned().unwrap_or_default();
        let pick = wanted.iter().find_map(|k| options.iter().find(|o| o["kind"] == *k));
        match pick {
            Some(o) => json!({"outcome": {"outcome": "selected", "optionId": o["optionId"]}}),
            None => json!({"outcome": {"outcome": "cancelled"}}),
        }
    }

    fn save(&mut self) -> Result<()> {
        let Some(prompt) = self.prompt_text.take() else { return Ok(()); };
        let session = match Session::open(&self.meta.id) {
            Some(s) => s,
            None => {
            // Delegated children arrive titled `↳ NAME: …`; only untitled threads take the prompt.
            if self.meta.title.is_empty() {
                self.meta.title = prompt.chars().take(60).collect();
            }
            Session::create(&self.meta)?
            }
        };
        session.append(&Message::user(prompt))?;
            if !self.log.is_empty() {
                session.append(&Message { role: Role::Assistant, content: std::mem::take(&mut self.log) })?;
            }
        Ok(())
    }
}

impl Backend for Acp {
    fn prompt<'a>(&'a mut self, text: String, tx: &'a EventTx) -> BoxFut<'a, Result<()>> {
        Box::pin(async move {
            while !self.idle_inc.is_empty() { self.handle_idle(tx).await?; } // parked idle message must not be lost to a turn that starts first
            let text = if text.trim_start().starts_with('/') { text.trim_start().to_string() } else { text };
            self.prompt_text = Some(text.clone());
            let _ = tx.send(Event::Remote { command: "acp_state".into(), data: self.state.clone() });
            if let Some(data) = &self.commands { let _ = tx.send(Event::Remote { command: "get_commands".into(), data: data.clone() }); }
            let rpc = self.rpc.clone();
            let mut prompt = self.prompt_content(&text);
            // Agents that defer MCP tools would not otherwise know delegation exists; once per agent process.
            if !text.starts_with('/') && std::mem::take(&mut self.guide) {
                prompt.insert(0, json!({"type": "text", "text": crate::delegate::INSTRUCTIONS}));
            }
            let params = json!({"sessionId": self.session_id, "prompt": prompt});
            let fut = rpc.request("session/prompt", params);
            tokio::pin!(fut);
            let result = loop {
                tokio::select! {
                    r = &mut fut => break r,
                    Some(inc) = self.incoming.recv() => self.handle(inc, tx).await,
                }
            };
            // Updates that were queued behind the final response.
            while let Ok(inc) = self.incoming.try_recv() {
                self.handle(inc, tx).await;
            }
            self.save()?;
            result.map(|_| ())
        })
    }

    fn cancel<'a>(&'a mut self, tx: &'a EventTx) -> BoxFut<'a, Result<()>> {
        Box::pin(async move {
            self.rpc.notify("session/cancel", json!({"sessionId": self.session_id})).await?;
            tokio::time::timeout(std::time::Duration::from_secs(10), async {
                let (sink,_) = mpsc::unbounded_channel();
                while self.rpc.pending_requests() > 0 {
                    tokio::select! {
                        Some(inc) = self.incoming.recv() => self.handle(inc, &sink).await,
                        _ = tokio::time::sleep(std::time::Duration::from_millis(10)) => {}
                    }
                }
                while let Ok(inc) = self.incoming.try_recv() { self.handle(inc, &sink).await; }
            }).await.map_err(|_| anyhow!("ACP agent did not acknowledge cancellation within 10 seconds"))?;
            self.save()?;
            let _ = tx.send(Event::Notify("実行を停止しました".into()));
            Ok(())
        })
    }

    fn control<'a>(&'a mut self, command: Value, tx: &'a EventTx) -> BoxFut<'a, Result<()>> {
        Box::pin(async move {
            if command["type"] == "get_state" {
                // Pick up advertisements that arrived while idle (e.g. right after session/new).
                while let Ok(inc) = self.incoming.try_recv() { self.handle(inc, tx).await; }
                let _ = tx.send(Event::Remote { command:"acp_state".into(), data:self.state.clone() });
                if let Some(data) = &self.commands { let _ = tx.send(Event::Remote { command:"get_commands".into(), data:data.clone() }); }
                return Ok(());
            }
            if command["type"] == "set_config_option" {
                let r = self.rpc.request("session/set_config_option", json!({"sessionId":self.session_id,"configId":command["configId"],"value":command["value"]})).await?;
                self.state["configOptions"] = r["configOptions"].clone();
                let _ = tx.send(Event::Remote { command:"acp_state".into(), data:self.state.clone() });
                return Ok(());
            }
            let model = match command["type"].as_str() {
                Some("select_model") => command["model"].as_str().ok_or_else(|| anyhow!("missing model"))?.to_string(),
                Some("set_model") => format!("{}/{}", command["provider"].as_str().unwrap_or(""), command["modelId"].as_str().unwrap_or("")),
                _ => anyhow::bail!("this ACP agent does not expose that RPC command"),
            };
            let option = self.state["configOptions"].as_array().and_then(|options| options.iter().find(|o| o["category"] == "model"));
            if let Some(option) = option {
                let r = self.rpc.request("session/set_config_option", json!({"sessionId":self.session_id,"configId":option["id"],"value":model})).await?;
                self.state["configOptions"] = r["configOptions"].clone();
            } else {
                self.rpc.request("session/set_model", json!({"sessionId":self.session_id,"modelId":model})).await?;
                self.state["models"]["currentModelId"] = json!(model);
            }
            let _ = tx.send(Event::Remote { command: "acp_state".into(), data: self.state.clone() });
            Ok(())
        })
    }

    /// Cancel-safe: only awaits `recv` and parks the message in `idle_inc` with no await in between;
    /// `handle_idle` then processes it outside any `select!`.
    fn idle<'a>(&'a mut self) -> BoxFut<'a, Result<()>> {
        Box::pin(async move {
            if !self.idle_inc.is_empty() { return Ok(()); }
            match self.incoming.recv().await {
                Some(inc) => { self.idle_inc.push_back(inc); Ok(()) }
                None => std::future::pending().await,
            }
        })
    }

    fn handle_idle<'a>(&'a mut self, tx: &'a EventTx) -> BoxFut<'a, Result<()>> {
        Box::pin(async move {
            // Between turns only state changes (and agent requests, which must be answered) matter;
            // message chunks would pollute the next turn's saved log.
            if let Some(inc) = self.idle_inc.pop_front().filter(|i| matches!(i, Incoming::Request { .. }) || is_state_update(i)) {
                self.handle(inc, tx).await;
            }
            Ok(())
        })
    }
}

fn is_state_update(inc: &Incoming) -> bool {
    matches!(inc, Incoming::Notification { method, params } if method == "session/update"
        && matches!(params["update"]["sessionUpdate"].as_str(), Some("available_commands_update" | "config_option_update")))
}

async fn open_session(rpc: &Arc<Rpc>, saved: &Value, init: &Value, meta: &Meta, mcp: &[Value]) -> Result<(String, Value)> {
    if let Some(id) = saved["sessionId"].as_str() {
        if init["agentCapabilities"]["loadSession"] != true {
            anyhow::bail!("this ACP agent cannot resume sessions; start a new thread rather than silently losing context");
        }
        let r = rpc.request("session/load", json!({"sessionId":id,"cwd":meta.cwd,"mcpServers":mcp})).await?;
        Ok((id.to_string(), r))
    } else {
        if !Session::can_start_external(&meta.id)? {
            anyhow::bail!("this thread predates saved ACP session handles; start a new thread to preserve its transcript");
        }
        let r = rpc.request("session/new", json!({"cwd": meta.cwd, "mcpServers": mcp})).await?;
        let id = r["sessionId"].as_str().ok_or_else(|| anyhow!("agent returned no sessionId"))?.to_string();
        Ok((id, r))
    }
}

/// A failure worth retrying on another Antigravity endpoint: connection trouble, a 5xx, or a dead runtime.
fn endpoint_failure(err: &anyhow::Error) -> bool {
    let text = format!("{err:#}").to_ascii_lowercase();
    ["process exited", "connection", "connect", "eof", "timed out", "timeout", "unavailable"]
        .iter()
        .any(|needle| text.contains(needle))
        || (500..=599).any(|code| text.contains(&format!(" {code}")) || text.contains(&format!("{code} ")))
}

/// A non-text content block as something the transcript can show; images and audio have no text form.
fn placeholder(content: &Value) -> Option<String> {
    Some(match content["type"].as_str()? {
        "image" => "[画像]".into(),
        "audio" => "[音声]".into(),
        "resource_link" => format!(
            "[リンク: {}]({})",
            content["name"].as_str().or(content["title"].as_str()).unwrap_or("リソース"),
            content["uri"].as_str().unwrap_or("")
        ),
        "resource" => content["resource"]["uri"].as_str().map_or_else(|| "[リソース]".into(), |uri| format!("[リソース: {uri}]")),
        _ => return None,
    })
}

/// GreenCode's stdio MCP servers (`mcp.json` and the project's `.mcp.json`) in ACP's `mcpServers` shape.
/// HTTP servers are left out: ACP agents only receive them when they advertise `mcpCapabilities.http`.
fn stdio_mcp_servers(cwd: &std::path::Path) -> Vec<Value> {
    let mut out: Vec<Value> = vec![];
    for path in [crate::home().join("mcp.json"), cwd.join(".mcp.json")] {
        let Ok(text) = std::fs::read_to_string(&path) else { continue };
        let Ok(v) = serde_json::from_str::<Value>(&text) else { continue };
        for (name, server) in v["mcpServers"].as_object().into_iter().flatten() {
            let Some(command) = server["command"].as_str() else { continue };
            let args: Vec<&str> = server["args"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
            let env: Vec<Value> = server["env"].as_object().into_iter().flatten().map(|(k, v)| json!({"name": k, "value": v.as_str().unwrap_or("")})).collect();
            out.retain(|s| s["name"] != name.as_str());
            out.push(json!({"name": name, "command": command, "args": args, "env": env}));
        }
    }
    out
}
