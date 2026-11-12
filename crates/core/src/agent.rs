//! The native agent loop.

use crate::{
    config::Config,
    context,
    lua::Lua,
    permission::{self, Perms},
    provider::{self, Provider, Request},
    session::{Meta, Session},
    skills,
    tools::{self, mcp::Mcp},
    Backend, Block, BoxFut, Decision, Event, EventTx, Message, Role, ToolDef,
};
use anyhow::{bail, Result};
use futures_util::future::join_all;
use serde_json::{json, Value};
use std::{path::PathBuf, sync::Arc};
use tokio::sync::{mpsc, oneshot};

const COMPACT_PROMPT: &str = "Stop here and do not call tools. Write a summary of this conversation that lets you continue the work \
without the history: the user's goals, decisions made, files touched, the current state, and the next steps.";

pub struct Agent {
    cfg: Arc<Config>,
    provider: Arc<dyn Provider>,
    model: String,
    cwd: PathBuf,
    system: String,
    pub messages: Vec<Message>,
    meta: Meta,
    session: Option<Session>,
    perms: Arc<Perms>,
    lua: Arc<Lua>,
    mcp: Arc<Mcp>,
    subagent: bool,
    /// Context size of the last request, for the compaction trigger.
    context: u64,
}

async fn ask(tx: &EventTx, tool: &str, summary: String) -> Decision {
    let (reply, rx) = oneshot::channel();
    let _ = tx.send(Event::Permission { tool: tool.into(), summary, reply });
    rx.await.unwrap_or(Decision::Deny)
}

impl Agent {
    pub fn new(cfg: Arc<Config>, meta: &Meta, perms: Arc<Perms>) -> Result<Agent> {
        let (provider, model) = provider::make(&cfg, &meta.model)?;
        Ok(Self::with_provider(cfg, meta, perms, provider, model))
    }

    /// Resumes from the session file when one exists.
    pub fn with_provider(cfg: Arc<Config>, meta: &Meta, perms: Arc<Perms>, provider: Arc<dyn Provider>, model: String) -> Agent {
        let messages = Session::load(&meta.id).map(|(_, m)| m).unwrap_or_default();
        Agent {
            cfg,
            provider,
            model,
            system: context::system_prompt(&meta.cwd),
            messages,
            session: Session::open(&meta.id),
            lua: Arc::new(Lua::new(&meta.cwd)),
            mcp: Arc::new(Mcp::load(&meta.cwd)),
            cwd: meta.cwd.clone(),
            meta: meta.clone(),
            perms,
            subagent: false,
            context: 0,
        }
    }

    fn tools(&self) -> Vec<ToolDef> {
        let mut t = tools::defs(self.subagent, &self.mcp.names());
        t.extend(self.lua.tools());
        t
    }

    fn push(&mut self, m: Message) {
        if !self.subagent {
            if self.session.is_none() {
                if let Some(Block::Text { text }) = m.content.first() {
                    self.meta.title = text.chars().take(60).collect();
                }
                self.session = Session::create(&self.meta).ok();
            }
            if let Some(s) = &self.session {
                let _ = s.append(&m);
            }
        }
        self.messages.push(m);
    }

    /// A cancelled turn can leave tool calls without results, which the API rejects.
    fn repair(&mut self) {
        let Some(Message { role: Role::Assistant, content }) = self.messages.last() else { return };
        let results: Vec<Block> = content
            .iter()
            .filter_map(|b| match b {
                Block::ToolUse { id, .. } => {
                    Some(Block::ToolResult { tool_use_id: id.clone(), content: "interrupted by the user".into(), is_error: true })
                }
                _ => None,
            })
            .collect();
        if !results.is_empty() {
            self.push(Message { role: Role::User, content: results });
        }
    }

    fn notes(&self, tx: &EventTx) {
        for n in self.lua.take_notes() {
            let _ = tx.send(Event::Notify(n));
        }
    }

    async fn request(&self, messages: &[Message], tx: &EventTx) -> Result<provider::Reply> {
        let tools = self.tools();
        let req = Request { model: &self.model, system: &self.system, messages, tools: &tools, max_tokens: self.cfg.max_tokens };
        let reply = self
            .provider
            .complete(req, &|t| {
                let _ = tx.send(Event::Text(t.to_string()));
            })
            .await?;
        let _ = tx.send(Event::Usage(reply.usage));
        Ok(reply)
    }

    /// Replaces the history with a model-written summary.
    async fn compact(&mut self, tx: &EventTx) -> Result<()> {
        if self.messages.is_empty() {
            return Ok(());
        }
        let _ = tx.send(Event::Notify("Compacting conversation…".into()));
        let mut messages = self.messages.clone();
        messages.push(Message::user(COMPACT_PROMPT));
        let (sink, _) = mpsc::unbounded_channel(); // the summary is not shown as a reply
        let reply = self.request(&messages, &sink).await?;
        let summary: Vec<&str> = reply.blocks.iter().filter_map(|b| if let Block::Text { text } = b { Some(text.as_str()) } else { None }).collect();
        if summary.is_empty() {
            bail!("compaction produced no summary");
        }
        let summary = format!("Summary of the conversation so far:\n{}", summary.join("\n"));
        if let Some(s) = &self.session {
            s.append(&json!({ "compact": summary }))?;
        }
        self.messages = vec![Message::user(summary)];
        self.context = 0;
        Ok(())
    }

    pub async fn run_turn(&mut self, prompt: String, tx: &EventTx) -> Result<()> {
        self.repair();
        self.push(Message::user(prompt));
        loop {
            if self.context > self.cfg.compact_at {
                self.compact(tx).await?;
            }
            // A `context` hook rewrites what is sent, not what is stored.
            let rewritten = match self.lua.has_hook("context") {
                true => self.lua.hook("context", json!({ "messages": self.messages })).await,
                false => Value::Null,
            };
            let rewritten: Option<Vec<Message>> = rewritten.get("messages").and_then(|m| serde_json::from_value(m.clone()).ok());
            let reply = self.request(rewritten.as_deref().unwrap_or(&self.messages), tx).await?;
            self.notes(tx);
            self.context = reply.usage.context() + reply.usage.output;
            if reply.blocks.is_empty() {
                bail!("the model returned an empty reply (stop reason: {})", reply.stop);
            }
            self.push(Message { role: Role::Assistant, content: reply.blocks.clone() });
            if reply.stop == "refusal" {
                bail!("the model declined this request");
            }
            let uses: Vec<(&String, &String, &Value)> = reply
                .blocks
                .iter()
                .filter_map(|b| if let Block::ToolUse { id, name, input } = b { Some((id, name, input)) } else { None })
                .collect();
            if uses.is_empty() {
                if reply.stop == "max_tokens" {
                    let _ = tx.send(Event::Notify("Reply was cut off at max_tokens.".into()));
                }
                return Ok(());
            }
            let results = if reply.stop == "max_tokens" {
                // The last call's arguments may be cut off; run none of them.
                uses.iter()
                    .map(|(id, ..)| Block::ToolResult {
                        tool_use_id: (*id).clone(),
                        content: "not run: the reply hit max_tokens; retry with smaller calls".into(),
                        is_error: true,
                    })
                    .collect()
            } else {
                join_all(uses.iter().map(|(id, name, input)| self.exec(id, name, (*input).clone(), tx))).await
            };
            self.push(Message { role: Role::User, content: results });
        }
    }

    async fn exec(&self, id: &str, name: &str, mut input: Value, tx: &EventTx) -> Block {
        let _ = tx.send(Event::ToolStart { id: id.into(), name: name.into(), input: input.clone() });
        let result = self.exec_inner(name, &mut input, tx).await;
        self.notes(tx);
        let (output, is_error) = match result {
            Ok(s) => (s, false),
            Err(e) => (format!("{e:#}"), true),
        };
        let output = context::truncate(output, id);
        let _ = tx.send(Event::ToolEnd { id: id.into(), output: output.clone(), is_error });
        Block::ToolResult { tool_use_id: id.into(), content: output, is_error }
    }

    async fn exec_inner(&self, name: &str, input: &mut Value, tx: &EventTx) -> Result<String> {
        if input.get("__invalid_json").is_some() {
            bail!("INVALID_JSON: the tool arguments were not a valid JSON object; send the call again");
        }
        let hook = self.lua.hook("tool_call", json!({"tool": name, "input": input})).await;
        if let Some(reason) = hook["deny"].as_str() {
            bail!("blocked by an extension: {reason}");
        }
        if hook["input"].is_object() {
            *input = hook["input"].clone();
        }
        if let Some(key) = self.perms.check(name, input, &self.cwd) {
            match ask(tx, name, permission::summary(name, input)).await {
                Decision::Allow => {}
                Decision::Always => self.perms.always(key),
                Decision::Deny => bail!("the user denied this action"),
            }
        }
        let mut result = match name {
            "read" | "write" | "edit" | "bash" | "memory" => tools::run(name, input, &self.cwd).await,
            "task" if !self.subagent => self.task(input["prompt"].as_str().unwrap_or("").to_string(), tx).await,
            "mcp" => self.mcp.run(input, &self.cwd).await,
            _ => self.lua.tool(name, input.clone()).await.map_err(anyhow::Error::msg),
        };
        if self.lua.has_hook("tool_result") {
            let payload = match &result {
                Ok(s) => json!({"tool": name, "input": input, "output": s, "is_error": false}),
                Err(e) => json!({"tool": name, "input": input, "output": format!("{e:#}"), "is_error": true}),
            };
            if let Some(out) = self.lua.hook("tool_result", payload).await["output"].as_str() {
                result = result.map(|_| out.to_string());
            }
        }
        result
    }

    /// Sub-agent: same tools minus `task`, empty history, nothing persisted.
    /// Boxed because it re-enters `run_turn`.
    fn task<'a>(&'a self, prompt: String, tx: &'a EventTx) -> BoxFut<'a, Result<String>> {
        Box::pin(async move {
            let mut sub = Agent {
                cfg: self.cfg.clone(),
                provider: self.provider.clone(),
                model: self.model.clone(),
                cwd: self.cwd.clone(),
                system: self.system.clone(),
                messages: vec![],
                meta: self.meta.clone(),
                session: None,
                perms: self.perms.clone(),
                lua: self.lua.clone(),
                mcp: self.mcp.clone(),
                subagent: true,
                context: 0,
            };
            let (stx, mut srx) = mpsc::unbounded_channel();
            let result = {
                let run = sub.run_turn(prompt, &stx);
                tokio::pin!(run);
                loop {
                    tokio::select! {
                        r = &mut run => break r,
                        // Only approvals and notices surface; the sub-agent's chatter stays out of the thread.
                        Some(ev) = srx.recv() => if matches!(ev, Event::Permission { .. } | Event::Notify(_)) {
                            let _ = tx.send(ev);
                        }
                    }
                }
            };
            result?;
            let text = sub.messages.last().into_iter().flat_map(|m| &m.content).filter_map(|b| match b {
                Block::Text { text } => Some(text.as_str()),
                _ => None,
            });
            Ok(text.collect::<Vec<_>>().join("\n"))
        })
    }
}

impl Backend for Agent {
    fn prompt<'a>(&'a mut self, text: String, tx: &'a EventTx) -> BoxFut<'a, Result<()>> {
        Box::pin(async move {
            if self.lua.untrusted().is_some() {
                let dir = self.lua.untrusted().unwrap();
                if ask(tx, "load project extensions", dir.display().to_string()).await != Decision::Deny {
                    self.lua.trust();
                }
            }
            if let Err(e) = self.lua.refresh().await {
                let _ = tx.send(Event::Notify(format!("Extension error: {e}")));
            }
            let mut prompt = text;
            if let Some(rest) = prompt.strip_prefix('/') {
                let (name, args) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
                if name == "compact" {
                    return self.compact(tx).await;
                } else if let Some(body) = skills::command(&self.cwd, name) {
                    prompt = body.replace("$ARGUMENTS", args.trim());
                } else if self.lua.commands().iter().any(|c| c == name) {
                    let r = self.lua.command(name, args.trim()).await;
                    self.notes(tx);
                    match r.map_err(anyhow::Error::msg)? {
                        Some(p) => prompt = p,
                        None => return Ok(()),
                    }
                }
            }
            let hook = self.lua.hook("prompt_submit", json!({ "prompt": prompt })).await;
            if let Some(reason) = hook["deny"].as_str() {
                bail!("blocked by an extension: {reason}");
            }
            if let Some(p) = hook["prompt"].as_str() {
                prompt = p.to_string();
            }
            self.run_turn(prompt, tx).await
        })
    }
}
