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
}

impl Acp {
    pub async fn start(command: &[String], meta: &Meta, perms: Arc<Perms>) -> Result<Acp> {
        let (rpc, incoming) = Rpc::spawn(command, &[], &meta.cwd)?;
        rpc.request("initialize", json!({
            "protocolVersion": 1,
            "clientCapabilities": {"fs": {"readTextFile": true, "writeTextFile": true}},
        }))
        .await?;
        // shortcut: always a new agent session; a reopened thread shows its history but the agent does not remember it. Use `session/load` when the agent advertises it.
        let r = rpc.request("session/new", json!({"cwd": meta.cwd, "mcpServers": []})).await?;
        let session_id = r["sessionId"].as_str().ok_or_else(|| anyhow!("agent returned no sessionId"))?.to_string();
        Ok(Acp { rpc, incoming, session_id, cwd: meta.cwd.clone(), perms, meta: meta.clone(), log: vec![] })
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
                    kind @ ("tool_call" | "tool_call_update") => {
                        let id = u["toolCallId"].as_str().unwrap_or("").to_string();
                        if kind == "tool_call" {
                            let name = u["title"].as_str().unwrap_or("tool").to_string();
                            let input = if u["rawInput"].is_object() { u["rawInput"].clone() } else { json!({}) };
                            self.log.push(Block::ToolUse { id: id.clone(), name: name.clone(), input: input.clone() });
                            let _ = tx.send(Event::ToolStart { id: id.clone(), name, input });
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
                    _ => {}
                }
            }
            Incoming::Notification { .. } => {}
            Incoming::Request { id, method, params } => {
                let result = match method.as_str() {
                    "session/request_permission" => Ok(self.permission(&params, tx).await),
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
                    m => Err(format!("method `{m}` is not supported")),
                };
                let _ = self.rpc.respond(id, result).await;
            }
        }
    }

    async fn permission(&self, params: &Value, tx: &EventTx) -> Value {
        let decision = if self.perms.mode() == Mode::Full {
            Decision::Allow
        } else {
            let (reply, rx) = oneshot::channel();
            let call = &params["toolCall"];
            let _ = tx.send(Event::Permission {
                tool: call["title"].as_str().unwrap_or("tool").into(),
                summary: if call["rawInput"].is_null() { String::new() } else { call["rawInput"].to_string() },
                reply,
            });
            rx.await.unwrap_or(Decision::Deny)
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

    fn save(&mut self, prompt: &str) {
        let session = Session::open(&self.meta.id).or_else(|| {
            self.meta.title = prompt.chars().take(60).collect();
            Session::create(&self.meta).ok()
        });
        if let Some(s) = session {
            let _ = s.append(&Message::user(prompt));
            if !self.log.is_empty() {
                let _ = s.append(&Message { role: Role::Assistant, content: std::mem::take(&mut self.log) });
            }
        }
    }
}

impl Backend for Acp {
    fn prompt<'a>(&'a mut self, text: String, tx: &'a EventTx) -> BoxFut<'a, Result<()>> {
        Box::pin(async move {
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
            self.save(&text);
            result.map(|_| ())
        })
    }

    fn cancel<'a>(&'a mut self) -> BoxFut<'a, ()> {
        Box::pin(async move {
            let _ = self.rpc.notify("session/cancel", json!({"sessionId": self.session_id})).await;
        })
    }
}
