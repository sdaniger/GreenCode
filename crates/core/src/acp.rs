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
}

impl Acp {
    pub async fn start(command: &[String], env: &std::collections::BTreeMap<String, String>, meta: &Meta, perms: Arc<Perms>) -> Result<Acp> {
        let saved = Session::runtime_for_backend(&meta.id, &meta.backend)?;
        let (rpc, incoming, mut out_rx) = Rpc::spawn_with_output(command, &env.iter().map(|(k, v)| (k.clone(), v.clone())).collect::<Vec<_>>(), &meta.cwd)?;
        let init = rpc.request("initialize", json!({
            "protocolVersion": 1,
            "clientCapabilities": {"fs": {"readTextFile": true, "writeTextFile": true}},
        }))
        .await?;
        let (session_id, r) = match open_session(&rpc, &saved, &init, meta).await {
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
                open_session(&rpc, &saved, &init, meta).await?
            }
            Err(err) => return Err(err),
        };
        Session::save_runtime(&meta.id, &json!({"backend":meta.backend,"sessionId":session_id}))?;
        let mut acp = Acp { rpc, incoming, session_id, cwd: meta.cwd.clone(), perms, meta: meta.clone(), log: vec![], state: r, prompt_text:None, commands:None, idle_inc:Default::default() };
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

    async fn handle(&mut self, inc: Incoming, tx: &EventTx) {
        match inc {
            Incoming::Notification { method, params } if method == "session/update" => {
                let u = &params["update"];
                match u["sessionUpdate"].as_str().unwrap_or("") {
                    "agent_message_chunk" => {
                        if let Some(t) = u["content"]["text"].as_str() {
                            self.text(t);
                            let _ = tx.send(Event::Text(t.into()));
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
                                    _ => c["content"]["text"].as_str().map(String::from),
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
        if let Some(key) = self.perms.check(tool, params, &self.cwd) {
            let (reply, rx) = oneshot::channel();
            let _ = tx.send(Event::Permission { tool:tool.into(), summary:file.into(), reply });
            match rx.await.unwrap_or(Decision::Deny) {
                Decision::Allow => {}
                Decision::Always => self.perms.always(key),
                Decision::Deny => return Err("the user denied this file access".into()),
            }
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
            match self.perms.check(tool, &input, &self.cwd) {
                None => Decision::Allow,
                Some(key) => {
                    let (reply, rx) = oneshot::channel();
                    let _ = tx.send(Event::Permission {
                        tool: call["title"].as_str().unwrap_or("tool").into(),
                        summary: crate::permission::summary(call["title"].as_str().filter(|_| tool != "bash").unwrap_or(tool), &call["rawInput"]),
                        reply,
                    });
                    match rx.await.unwrap_or(Decision::Deny) {
                        Decision::Always => { self.perms.always(key); Decision::Always }
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
            self.meta.title = prompt.chars().take(60).collect();
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
            let params = json!({"sessionId": self.session_id, "prompt": [{"type": "text", "text": text}]});
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

async fn open_session(rpc: &Arc<Rpc>, saved: &Value, init: &Value, meta: &Meta) -> Result<(String, Value)> {
    if let Some(id) = saved["sessionId"].as_str() {
        if init["agentCapabilities"]["loadSession"] != true {
            anyhow::bail!("this ACP agent cannot resume sessions; start a new thread rather than silently losing context");
        }
        let r = rpc.request("session/load", json!({"sessionId":id,"cwd":meta.cwd,"mcpServers":[]})).await?;
        Ok((id.to_string(), r))
    } else {
        if Session::open(&meta.id).is_some() {
            anyhow::bail!("this thread predates saved ACP session handles; start a new thread to preserve its transcript");
        }
        let r = rpc.request("session/new", json!({"cwd": meta.cwd, "mcpServers": []})).await?;
        let id = r["sessionId"].as_str().ok_or_else(|| anyhow!("agent returned no sessionId"))?.to_string();
        Ok((id, r))
    }
}
