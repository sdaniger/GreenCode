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
use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    sync::{Arc, Mutex},
};
use tokio::sync::{mpsc, oneshot, OwnedSemaphorePermit, Semaphore};
use std::future::Future;

const COMPACT_PROMPT: &str = "Stop here and do not call tools. Write a summary of this conversation that lets you continue the work \
without the history: the user's goals, decisions made, files touched, the current state, and the next steps. Do not copy workspace instruction or memory snapshots; current files are supplied separately.";

const READ_PREVIEW_GUIDE: &str = "\nAn old read may show an excerpt and a `full output:` path under GreenCode's spill directory. \
If you need omitted information, call `read` on that exact saved path before answering. \
The saved result is available even if the original file changed or was deleted.";

enum Method {
    Summary,
    Snap,
}

/// Context surgery an `ext` call asked for; applied once the round's tool calls finish.
enum Reset {
    Rewind(usize, String),
    Fresh(String),
}

struct JobOutcome {
    name: String,
    agent: String,
    result: Result<String, String>,
}
struct TaskSpec {
    name: String,
    def: crate::subagents::AgentDef,
    prompt: String,
    model: Option<String>,
    isolated: bool,
    schema: Option<Value>,
}
struct SubCtl {
    steer: mpsc::UnboundedSender<Value>,
    cancel: Arc<tokio::sync::Notify>,
}
struct SubRegistration {
    subs: Arc<Mutex<HashMap<String, SubCtl>>>,
    name: String,
    cancel: Arc<tokio::sync::Notify>,
}
impl Drop for SubRegistration {
    fn drop(&mut self) {
        let mut subs = self.subs.lock().unwrap();
        if subs
            .get(&self.name)
            .is_some_and(|ctl| Arc::ptr_eq(&ctl.cancel, &self.cancel))
        {
            subs.remove(&self.name);
        }
    }
}
static WORKTREE_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

enum WorktreeState {
    PristineCandidate,
    ShellUsed,
    Completed,
}

struct TaskWorktree {
    repo: PathBuf,
    path: PathBuf,
    branch: String,
    initial: String,
    state: Mutex<WorktreeState>,
    tx: EventTx,
}

impl Drop for TaskWorktree {
    fn drop(&mut self) {
        let Ok(state) = self.state.get_mut() else {
            return;
        };
        let result = match state {
            WorktreeState::Completed => return,
            WorktreeState::ShellUsed => Err(anyhow::anyhow!("worktree may still be in use")),
            WorktreeState::PristineCandidate => crate::worktree::remove_pristine_task(
                &self.repo,
                &self.path,
                &self.branch,
                &self.initial,
            ),
        };
        if let Err(e) = result {
            let _ = self.tx.send(Event::Notify(format!(
                "Task worktree cleanup incomplete for {} ({}): {e:#}",
                self.path.display(),
                self.branch
            )));
        }
    }
}

#[derive(Clone, Copy)]
enum Review { Local, Remote }

pub struct Agent {
    cfg: Arc<Config>,
    provider: Arc<dyn Provider>,
    model: String,
    cwd: PathBuf,
    /// Fixed: workspace text is sent separately so an unchanged head stays cacheable.
    system: String,
    /// First user message of every request; held outside the transcript so resume reuses it byte for byte.
    prompt_context: String,
    /// Snapshot `prompt_context` was built from, to detect workspace-file changes.
    latest_context: String,
    pub messages: Vec<Message>,
    meta: Meta,
    session: Option<Session>,
    snap_source: Option<String>,
    perms: Arc<Perms>,
    lua: Arc<Lua>,
    mcp: Arc<Mcp>,
    subagent: bool,
    review: Option<Review>,
    task_worktree: Option<Arc<TaskWorktree>>,
    /// Concurrency slots shared by the main agent and every sub-agent below it.
    slots: Arc<Semaphore>,
    /// The slot a running sub-agent holds; it is lent back while the agent only waits for its own children.
    slot: Mutex<Option<OwnedSemaphorePermit>>,
    allowed: Option<Vec<String>>,
    submit_schema: Option<Value>,
    submitted: Mutex<Option<Value>>,
    jobs: std::sync::Mutex<tokio::task::JoinSet<JobOutcome>>,
    /// Context size of the last request, for the compaction trigger.
    context: u64,
    started: bool,
    /// Images from this round's computer/read calls, attached after the tool results.
    images: std::sync::Mutex<Vec<Value>>,
    steer_tx: mpsc::UnboundedSender<Value>,
    steer_rx: Mutex<mpsc::UnboundedReceiver<Value>>,
    subs: Arc<Mutex<HashMap<String, SubCtl>>>,
    pending: Mutex<Vec<String>>,
    depth: u8,
    rules: Arc<Vec<crate::rules::Rule>>,
    /// Rules already applied, so each fires once.
    fired: Mutex<HashSet<String>>,
    /// Index of the assistant message that called `checkpoint`.
    checkpoint: Mutex<Option<usize>>,
    reset: Mutex<Option<Reset>>,
}

fn broad_output(name: &str) -> bool {
    matches!(name, "bash" | "grep" | "glob" | "web_fetch")
}

async fn ask(tx: &EventTx, tool: &str, summary: String) -> Decision {
    let (reply, rx) = oneshot::channel();
    let _ = tx.send(Event::Permission {
        tool: tool.into(),
        summary,
        reply,
    });
    rx.await.unwrap_or(Decision::Deny)
}

impl Agent {
    pub fn new(cfg: Arc<Config>, meta: &Meta, perms: Arc<Perms>) -> Result<Agent> {
        let (provider, model) = provider::make(&cfg, &meta.model)?;
        Ok(Self::with_provider(cfg, meta, perms, provider, model))
    }

    /// Resumes from the session file when one exists.
    pub fn with_provider(
        cfg: Arc<Config>,
        meta: &Meta,
        perms: Arc<Perms>,
        provider: Arc<dyn Provider>,
        model: String,
    ) -> Agent {
        let messages = Session::load(&meta.id).map(|(_, m)| m).unwrap_or_default();
        let (steer_tx, steer_rx) = mpsc::unbounded_channel();
        let saved = Session::runtime(&meta.id).unwrap_or_default();
        let current = context::session_context(&meta.cwd, &cfg);
        let prompt_context = saved["prompt_context"]
            .as_str()
            .filter(|_| saved["backend"] == "native")
            .map(str::to_owned)
            .unwrap_or_else(|| context::wrap_context(&current));
        let latest_context = saved["context_source"]
            .as_str()
            .map(str::to_owned)
            .unwrap_or(current);
        let context = saved["context"].as_u64().unwrap_or(0);
        let slots = Arc::new(Semaphore::new(cfg.task.slots()));
        let mut system = cfg.system_prompt.as_deref().unwrap_or_else(|| context::system_prompt()).to_owned();
        if let Some(text) = &cfg.append_system_prompt {
            if !system.is_empty() { system.push_str("\n\n"); }
            system.push_str(text);
        }
        let mcp = Arc::new(Mcp::load_with_project(&meta.cwd, cfg.mcp_project_config));
        let rules = Arc::new(if cfg.rules_enabled { crate::rules::load(&meta.cwd) } else { Vec::new() });
        Agent {
            cfg,
            provider,
            model,
            system,
            prompt_context,
            latest_context,
            messages,
            session: Session::open(&meta.id),
            snap_source: Session::snap_source(&meta.id),
            lua: Arc::new(Lua::new(&meta.cwd)),
            mcp,
            cwd: meta.cwd.clone(),
            meta: meta.clone(),
            perms,
            subagent: false,
            review: None,
            task_worktree: None,
            slots,
            slot: Default::default(),
            allowed: None,
            submit_schema: None,
            submitted: Default::default(),
            jobs: Default::default(),
            context,
            started: false,
            images: Default::default(),
            steer_tx,
            steer_rx: Mutex::new(steer_rx),
            subs: Default::default(),
            pending: Default::default(),
            depth: 0,
            rules,
            fired: Default::default(),
            checkpoint: Default::default(),
            reset: Default::default(),
        }
    }

    fn tools(&self) -> Vec<ToolDef> {
        if self.cfg.no_tools || matches!(self.review, Some(Review::Remote)) { return Vec::new(); }
        let mut t = tools::defs(!self.can_task(), &self.mcp.names());
        if let Some(task) = t.iter_mut().find(|tool| tool.name == "task") {
            self.cfg.task.adjust_ad(task);
        }
        // Lua tools first, so the `tools` limit in config.toml applies to them too.
        if self.allowed.is_none() {
            t.extend(self.lua.tools());
        }
        if !self.cfg.tools.is_empty() {
            t.retain(|tool| {
                self.cfg.tools.iter().any(|name| name == &tool.name) || tool.name == "submit"
            });
        }
        if let Some(allowed) = &self.allowed {
            t.retain(|tool| allowed.contains(&tool.name));
        }
        if let Some(i) = t.iter().position(|tool| tool.name == "ext") {
            match tools::ext::def(&self.cfg.ext_off) {
                Some(ext) => t[i] = ext,
                None => {
                    t.remove(i);
                }
            }
        }
        if let Some(schema) = &self.submit_schema {
            t.push(ToolDef {
                name: "submit".into(),
                description: "Return your final result. Call this once when done.".into(),
                schema: schema.clone(),
            });
        }
        for tool in &mut t {
            tool.schema.sort_all_objects();
        }
        t.sort_by(|a, b| a.name.cmp(&b.name));
        t
    }

    /// Whether this agent may call `task`. Agent discovery reads the disk, so it only runs when agents are disabled.
    fn can_task(&self) -> bool {
        let task = &self.cfg.task;
        self.depth < task.depth_limit()
            && (task.disabled_agents.is_empty()
                || task.can_spawn(self.depth, &crate::subagents::discover(&self.cwd)))
    }

    /// A sub-agent that only waits for its own children lends its slot back, so they can run even with one slot.
    async fn lend_slot<T>(&self, lend: bool, wait: impl Future<Output = T>) -> T {
        let held = lend && self.slot.lock().unwrap().take().is_some();
        let out = wait.await;
        if held {
            let permit = self.slots.clone().acquire_owned().await.expect("semaphore closed");
            *self.slot.lock().unwrap() = Some(permit);
        }
        out
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

    fn handle_control(&self, v: Value) {
        match v["type"].as_str() {
            Some("steer") => {
                if let Some(text) = v["message"].as_str().filter(|s| !s.is_empty()) {
                    self.pending.lock().unwrap().push(text.to_string());
                }
            }
            Some("sub_steer") => {
                if let (Some(name), Some(message)) = (
                    v["name"].as_str(),
                    v["message"].as_str().filter(|s| !s.is_empty()),
                ) {
                    if let Some(ctl) = self.subs.lock().unwrap().get(name) {
                        let _ = ctl.steer.send(json!({"type":"steer","message":message}));
                    }
                }
            }
            Some("sub_cancel") => {
                if let Some(ctl) = v["name"].as_str().and_then(|name| {
                    self.subs
                        .lock()
                        .unwrap()
                        .get(name)
                        .map(|ctl| ctl.cancel.clone())
                }) {
                    ctl.notify_one();
                }
            }
            _ => {}
        }
    }

    fn drain_control(&self) {
        let mut rx = self.steer_rx.lock().unwrap();
        while let Ok(v) = rx.try_recv() {
            self.handle_control(v);
        }
    }

    async fn pump(&self, wake_on_steer: bool) {
        loop {
            if wake_on_steer && !self.pending.lock().unwrap().is_empty() {
                return;
            }
            let v = std::future::poll_fn(|cx| self.steer_rx.lock().unwrap().poll_recv(cx)).await;
            let Some(v) = v else {
                return std::future::pending().await;
            };
            self.handle_control(v);
        }
    }

    fn steered(&mut self) -> bool {
        self.drain_control();
        let pending = std::mem::take(&mut *self.pending.lock().unwrap());
        let pushed = !pending.is_empty();
        for text in pending {
            self.push(Message::user(text));
        }
        pushed
    }

    /// A cancelled turn can leave tool calls without results, which the API rejects.
    fn repair(&mut self) {
        let Some(Message {
            role: Role::Assistant,
            content,
        }) = self.messages.last()
        else {
            return;
        };
        let results: Vec<Block> = content
            .iter()
            .filter_map(|b| match b {
                Block::ToolUse { id, .. } => Some(Block::ToolResult {
                    tool_use_id: id.clone(),
                    content: "interrupted by the user".into(),
                    is_error: true,
                }),
                _ => None,
            })
            .collect();
        if !results.is_empty() {
            self.push(Message {
                role: Role::User,
                content: results,
            });
        }
    }

    /// Slash commands this agent runs, in execution order (first source wins). Project extension
    /// commands appear only once the project is trusted, since `lua` loads nothing before that.
    fn advertise(&self, tx: &EventTx) {
        let mut seen = std::collections::HashSet::new();
        let commands: Vec<Value> = [
            ("builtin", vec!["compact".to_string()]),
            ("command", skills::commands(&self.cwd)),
            ("extension", self.lua.commands()),
        ]
        .into_iter()
        .flat_map(|(source, names)| names.into_iter().map(move |name| (source, name)))
        .filter(|(_, name)| seen.insert(name.clone()))
        .map(|(source, name)| json!({"name": name, "source": source}))
        .collect();
        let _ = tx.send(Event::Remote {
            command: "get_commands".into(),
            data: json!({"commands": commands}),
        });
    }

    async fn refresh_extensions(&self, tx: &EventTx) {
        if self.cfg.extensions_enabled {
            if let Err(e) = self.lua.refresh().await {
                let _ = tx.send(Event::Notify(format!("Extension error: {e}")));
            }
        }
    }

    fn notes(&self, tx: &EventTx) {
        for n in self.lua.take_notes() {
            let _ = tx.send(Event::Notify(n));
        }
    }

    fn save_context(&self) -> Result<()> {
        if !self.subagent {
            let mut saved = Session::runtime(&self.meta.id)?;
            saved["backend"] = json!("native");
            saved["context"] = json!(self.context);
            saved["prompt_context"] = json!(self.prompt_context);
            saved["context_source"] = json!(self.latest_context);
            Session::save_runtime(&self.meta.id, &saved)?;
        }
        Ok(())
    }

    /// `GREENCODE_DUMP_REQUESTS=<dir>` appends one line per request with its component sizes.
    fn dump_request(&self, system: &str, tools: &[ToolDef], messages: &[Message]) {
        let Ok(dir) = std::env::var("GREENCODE_DUMP_REQUESTS") else {
            return;
        };
        let size = |v: serde_json::Value| v.to_string().len();
        let line = json!({
            "system": system.len(),
            "tools": size(json!(tools.iter().map(|t| json!({"name": t.name, "description": t.description, "schema": t.schema})).collect::<Vec<_>>())),
            "messages": size(json!(messages)),
            "count": messages.len(),
            "calls": messages.iter().rev().skip(1).find(|m| m.role == Role::Assistant).map(|m| m.content.iter().filter_map(|b| match b {
                Block::ToolUse { name, input, .. } => Some(json!({"name": name, "in": input.to_string().len()})),
                _ => None,
            }).collect::<Vec<_>>()),
            "last": messages.last().map(|m| json!({"role": format!("{:?}", m.role), "blocks": m.content.iter().map(|b| match b {
                Block::Text { text } => json!({"t": "text", "n": text.len()}),
                Block::ToolUse { name, input, .. } => json!({"t": "use", "name": name, "n": input.to_string().len()}),
                Block::ToolResult { content, .. } => json!({"t": "result", "n": content.len()}),
                Block::Other(v) => json!({"t": v["type"].as_str().unwrap_or("other"), "n": v.to_string().len()}),
            }).collect::<Vec<_>>()})),
        });
        let dir = std::path::Path::new(&dir);
        let _ = std::fs::create_dir_all(dir).and_then(|_| {
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(dir.join("requests.jsonl"))
                .and_then(|mut f| std::io::Write::write_all(&mut f, format!("{line}\n").as_bytes()))
        });
    }

    fn can_preview_old_reads(&self) -> bool {
        self.cfg.old_read_previews && !self.cfg.no_tools
            && !matches!(self.review, Some(Review::Remote))
            && !self.lua.has_hook("context")
            && (self.cfg.tools.is_empty() || self.cfg.tools.iter().any(|name| name == "read"))
            && self.allowed.as_ref().is_none_or(|allowed| allowed.iter().any(|name| name == "read"))
    }

    /// Summary and sub-agent calls count toward usage, not the main thread's displayed context.
    async fn request(
        &self,
        messages: &[Message],
        summarize: bool,
        tx: &EventTx,
    ) -> Result<provider::Reply> {
        let tools = if summarize { Vec::new() } else { self.tools() };
        // Workspace snapshots must not become part of the generated summary.
        let mut all = Vec::with_capacity(messages.len() + 1);
        if !summarize && !self.prompt_context.is_empty() {
            all.push(Message::user(self.prompt_context.clone()));
        }
        all.extend(context::model_messages(messages)?);
        let start = usize::from(!self.prompt_context.is_empty());
        let recovery_prompt = (!summarize && self.can_preview_old_reads()
            && context::preview_old_reads(&mut all[start..]))
            .then(|| format!("{}{READ_PREVIEW_GUIDE}", self.system));
        let system = recovery_prompt.as_deref().unwrap_or(&self.system);
        self.dump_request(system, &tools, &all);
        let req = Request {
            model: &self.model,
            system,
            messages: &all,
            tools: &tools,
            max_tokens: self.cfg.max_tokens,
        };
        let reply = self
            .provider
            .complete(req, &|t| {
                let _ = tx.send(Event::Text(t.to_string()));
            })
            .await?;
        let _ = tx.send(if summarize || self.subagent {
            Event::UsageAux(reply.usage)
        } else {
            Event::Usage(reply.usage)
        });
        Ok(reply)
    }

    async fn compact(&mut self, tx: &EventTx, method: Method) -> Result<()> {
        context::archive_computer_images(&mut self.messages)?;
        let mut history: Vec<_> = self
            .messages
            .iter()
            .filter(|m| !context::is_workspace_update(m))
            .cloned()
            .collect();
        if history.is_empty() {
            return Ok(());
        }
        let split = crate::snap::split_point(&history, crate::snap::KEEP_CHARS);
        if matches!(method, Method::Snap) {
            let edge = self.snap_edge().ok_or_else(|| anyhow::anyhow!("snap compaction needs a model that reads images (Anthropic or OpenAI); use /compact summary"))?;
            let start = usize::from(self.snap_source.is_some());
            let archived = if split >= start {
                crate::snap::serialize(&history[start..split])
            } else {
                String::new()
            };
            if split == 0 || archived.chars().count() < crate::snap::MIN_ARCHIVE_CHARS {
                let _ = tx.send(Event::Notify("Nothing worth archiving yet".into()));
                return Ok(());
            }
            let source = match &self.snap_source {
                Some(old) => format!("{old}\n{archived}"),
                None => archived,
            };
            let tail = &history[split..];
            let keep_latest_prompt = !tail.iter().any(|m| {
                m.role == Role::User && m.content.iter().any(|b| matches!(b, Block::Text { .. }))
            });
            let archive = crate::snap::archive(&source, edge, keep_latest_prompt);
            if archive.appendix_len > 12_000 {
                let _ = tx.send(Event::Notify(
                    "History is mostly text the frames cannot show; summarizing instead".into(),
                ));
                return Box::pin(self.compact(tx, Method::Summary)).await;
            }
            if let Some(s) = &self.session {
                s.append(&json!({"compact": archive.text, "frames": archive.frames, "source": archive.source}))?;
                for m in tail {
                    s.append(m)?;
                }
            }
            let count = archive.frames.len();
            self.messages = vec![crate::snap::message(&archive.text, &archive.frames)];
            self.messages.extend_from_slice(tail);
            self.snap_source = Some(archive.source);
            self.rebase_context();
            self.context = 0;
            *self.checkpoint.get_mut().unwrap() = None;
            self.save_context()?;
            let _ = tx.send(Event::Notify(format!(
                "Archived {} messages into {count} image frames (no model call)",
                split - start
            )));
            return Ok(());
        }
        // Keep exact user/assistant turns if only old reads need shortening and no rebase is pending.
        if split == 0 && self.context <= self.compact_at() && history.len() == self.messages.len()
            && self.can_preview_old_reads() && context::preview_old_reads(&mut history)
        {
            let _ = tx.send(Event::Notify("Keeping exact history; older reads are already previewed for the model.".into()));
            return Ok(());
        }
        let _ = tx.send(Event::Notify("Compacting conversation…".into()));
        // Summarize only the old span and replay the recent tail verbatim.
        let mut messages = history;
        let tail = if split == 0 {
            Vec::new()
        } else {
            messages.split_off(split)
        };
        messages.push(Message::user(COMPACT_PROMPT));
        let (sink, _) = mpsc::unbounded_channel(); // the summary is not shown as a reply
        let reply = self.request(&messages, true, &sink).await?;
        let _ = tx.send(Event::UsageAux(reply.usage));
        let summary: Vec<&str> = reply
            .blocks
            .iter()
            .filter_map(|b| {
                if let Block::Text { text } = b {
                    Some(text.as_str())
                } else {
                    None
                }
            })
            .collect();
        if summary.is_empty() {
            bail!("compaction produced no summary");
        }
        let summary = format!(
            "This is an automatically generated checkpoint of the conversation so far, not a message from the user. \
Continue the work from this summary and the messages that follow it.\n\n<compacted-summary>\n{}\n</compacted-summary>",
            summary.join("\n")
        );
        if let Some(s) = &self.session {
            s.append(&json!({ "compact": summary }))?;
            for m in &tail {
                s.append(m)?;
            }
        }
        self.messages = vec![Message::user(summary)];
        self.messages.extend(tail);
        self.snap_source = None;
        self.rebase_context();
        self.context = 0;
        *self.checkpoint.get_mut().unwrap() = None;
        self.save_context()?;
        Ok(())
    }

    fn rebase_context(&mut self) {
        self.latest_context = context::session_context(&self.cwd, &self.cfg);
        self.prompt_context = context::wrap_context(&self.latest_context);
    }

    fn snap_edge(&self) -> Option<u32> {
        let (name, _) = self.meta.model.split_once('/')?;
        crate::snap::frame_edge(self.cfg.providers.get(name)?, &self.model)
    }

    async fn compact_auto(&mut self, tx: &EventTx) -> Result<()> {
        let method = if self.cfg.compaction == "snap" && self.snap_edge().is_some() {
            Method::Snap
        } else {
            Method::Summary
        };
        self.compact(tx, method).await
    }

    fn compact_at(&self) -> u64 {
        let cfg = &self.cfg;
        if !cfg.model_compact_at_enabled { return cfg.compact_at; }
        let model = &self.meta.model;
        cfg.model_compact_at.get(model).copied().filter(|&n| n != 0).or_else(|| {
            cfg.model_compact_at.iter().filter(|&(_, &n)| n != 0)
                .filter_map(|(key, &n)| key.strip_suffix('*').filter(|prefix| model.starts_with(prefix)).map(|prefix| (prefix.len(), n)))
                .max_by_key(|&(len, _)| len).map(|(_, n)| n)
        }).unwrap_or(cfg.compact_at)
    }

    pub async fn run_turn(&mut self, prompt: String, tx: &EventTx) -> Result<()> {
        let mut tool_calls = 0u32;
        self.repair();
        context::archive_computer_images(&mut self.messages)?;
        // Never replace a newly submitted requirement with a lossy summary.
        let compact_at = self.compact_at();
        if self.cfg.compaction_enabled && self.context > compact_at {
            self.compact_auto(tx).await?;
        }
        // A steer left over from a cancelled turn keeps its place before the new prompt.
        self.steered();
        let mut message = Message::user(prompt);
        if self.review.is_none() {
            let images = context::attachments(
                match &message.content[0] {
                    Block::Text { text } => text,
                    _ => "",
                },
                &self.cwd,
            );
            message.content.extend(images);
        }
        self.push(message);
        loop {
            if self.cfg.compaction_enabled && (self.cfg.compaction_mid_turn || self.subagent)
                && self.context > compact_at
            {
                self.compact_auto(tx).await?;
            }
            self.deliver(false).await;
            self.steered();
            // A `context` hook rewrites what is sent, not what is stored.
            let rewritten = match self.lua.has_hook("context") {
                true => {
                    self.lua
                        .hook("context", json!({ "messages": self.messages }))
                        .await
                }
                false => Value::Null,
            };
            let rewritten: Option<Vec<Message>> = rewritten
                .get("messages")
                .and_then(|m| serde_json::from_value(m.clone()).ok());
            let reply = self
                .request(rewritten.as_deref().unwrap_or(&self.messages), false, tx)
                .await?;
            self.notes(tx);
            self.context = reply.usage.context() + reply.usage.output;
            if reply.blocks.is_empty() {
                bail!(
                    "the model returned an empty reply (stop reason: {})",
                    reply.stop
                );
            }
            for block in &reply.blocks {
                if let Block::Other(v) = block {
                    if let Some(thinking) = v["thinking"]
                        .as_str()
                        .filter(|t| v["type"] == "thinking" && !t.trim().is_empty())
                    {
                        let _ = tx.send(Event::Thinking(thinking.to_string()));
                    }
                }
            }
            self.push(Message {
                role: Role::Assistant,
                content: reply.blocks.clone(),
            });
            self.fire_rules(&reply.blocks, tx);
            self.save_context()?;
            if reply.stop == "refusal" {
                bail!("the model declined this request");
            }
            let uses: Vec<(&String, &String, &Value)> = reply
                .blocks
                .iter()
                .filter_map(|b| {
                    if let Block::ToolUse { id, name, input } = b {
                        Some((id, name, input))
                    } else {
                        None
                    }
                })
                .collect();
            tool_calls = tool_calls.saturating_add(uses.len() as u32);
            if self.cfg.max_tool_calls > 0 && tool_calls > self.cfg.max_tool_calls {
                let error = format!(
                    "tool call limit reached: {} calls requested, maximum is {}",
                    tool_calls, self.cfg.max_tool_calls
                );
                // Answer the calls here; `repair` would otherwise tell the model the user interrupted them.
                let results = uses
                    .iter()
                    .map(|(id, _, _)| Block::ToolResult {
                        tool_use_id: (*id).clone(),
                        content: error.clone(),
                        is_error: true,
                    })
                    .collect();
                self.push(Message {
                    role: Role::User,
                    content: results,
                });
                self.save_context()?;
                bail!("{error}");
            }
            if uses.is_empty() {
                if reply.stop == "max_tokens" {
                    let _ = tx.send(Event::Notify("Reply was cut off at max_tokens.".into()));
                }
                if self.deliver(true).await {
                    // The follow-up reply would otherwise run on from this one.
                    let _ = tx.send(Event::Text("\n\n".into()));
                    continue;
                }
                // Sub-agents report to their parent, and plan mode cannot save memory anyway.
                if self.cfg.learning.enabled
                    && !self.cfg.no_tools
                    && self.cfg.learning.auto
                    && !self.subagent
                    && self.perms.mode() != permission::Mode::Plan
                {
                    if let Err(e) = self.auto_learn(tx).await {
                        let _ = tx.send(Event::Notify(format!("自動学習を保存できません: {e:#}")));
                    }
                }
                return Ok(());
            }
            let results =
                if reply.stop == "max_tokens" {
                    // The last call's arguments may be cut off; run none of them.
                    uses.iter()
                        .map(|(id, ..)| Block::ToolResult {
                            tool_use_id: (*id).clone(),
                            content: "not run: the reply hit max_tokens; retry with smaller calls"
                                .into(),
                            is_error: true,
                        })
                        .collect()
                } else {
                    // shortcut: large batches may exceed 2 KiB to leave room for recovery paths.
                    let noisy = uses
                        .iter()
                        .filter(|(_, name, _)| broad_output(name))
                        .count();
                    let preview = (context::OUTPUT_PREVIEW / noisy.max(1)).max(512);
                    join_all(uses.iter().map(|(id, name, input)| {
                        self.exec(id, name, (*input).clone(), preview, tx)
                    }))
                    .await
                };
            // Screenshots follow the tool results as image blocks, which both wire formats accept in a user turn.
            let mut results: Vec<Block> = results;
            results.extend(self.images.lock().unwrap().drain(..).map(|block| {
                let _ = tx.send(Event::Image(block.clone()));
                Block::Other(block)
            }));
            if let Some(reset) = self.reset.get_mut().unwrap().take() {
                // The round's tool results are dropped with the messages they answer.
                self.apply_reset(reset)?;
                continue;
            }
            self.push(Message {
                role: Role::User,
                content: results,
            });
            if self.submitted.lock().unwrap().is_some() {
                return Ok(());
            }
        }
    }

    fn apply_reset(&mut self, reset: Reset) -> Result<()> {
        *self.checkpoint.get_mut().unwrap() = None;
        match reset {
            Reset::Rewind(at, report) => {
                if let Some(s) = &self.session {
                    s.append(&json!({"truncate": at, "keep_user_messages": true}))?;
                }
                crate::session::rewind_exploration(&mut self.messages, at);
                self.context = 0;
                self.push(Message::user(format!(
                    "Rewound to the checkpoint. Report of the exploration since then:\n{report}"
                )));
                self.save_context()?;
            }
            Reset::Fresh(handoff) => {
                let text = format!("The context was restarted at your request; this handoff is not a message from the user. Continue from it.\n\n<handoff>\n{handoff}\n</handoff>");
                if let Some(s) = &self.session {
                    s.append(&json!({"compact": text}))?;
                }
                self.messages = vec![Message::user(text)];
                self.snap_source = None;
                self.rebase_context();
                self.context = 0;
                self.save_context()?;
            }
        }
        Ok(())
    }

    async fn exec(
        &self,
        id: &str,
        name: &str,
        mut input: Value,
        preview: usize,
        tx: &EventTx,
    ) -> Block {
        let _ = tx.send(Event::ToolStart {
            id: id.into(),
            name: name.into(),
            input: input.clone(),
        });
        let result = self.exec_inner(name, &mut input, tx).await;
        self.notes(tx);
        let (output, is_error) = match result {
            Ok(s) => (s, false),
            Err(e) => (format!("{e:#}"), true),
        };
        let (output, _) = if !is_error && broad_output(name) {
            context::preview(output, id, preview)
        } else {
            context::truncate(output, id)
        };
        let _ = tx.send(Event::ToolEnd {
            id: id.into(),
            output: output.clone(),
            is_error,
        });
        Block::ToolResult {
            tool_use_id: id.into(),
            content: output,
            is_error,
        }
    }

    async fn exec_inner(&self, name: &str, input: &mut Value, tx: &EventTx) -> Result<String> {
        if matches!(self.review, Some(Review::Remote)) { bail!("tools are disabled for remote reviews"); }
        if self.cfg.no_tools { bail!("tools are disabled by no_tools"); }
        if !self.cfg.tools.is_empty()
            && name != "submit"
            && !self.cfg.tools.iter().any(|tool| tool == name)
        {
            bail!("tool `{name}` is disabled in config.toml");
        }
        if (name == "task" && !self.can_task())
            || (self.subagent
                && (name != "submit" || self.submit_schema.is_none())
                && self
                    .allowed
                    .as_ref()
                    .is_some_and(|allowed| !allowed.iter().any(|tool| tool == name)))
        {
            bail!("tool `{name}` is not available to this agent");
        }
        if input.get("__invalid_json").is_some() {
            bail!("INVALID_JSON: the tool arguments were not a valid JSON object; send the call again");
        }
        let hook = self
            .lua
            .hook("tool_call", json!({"tool": name, "input": input}))
            .await;
        if let Some(reason) = hook["deny"].as_str() {
            bail!("blocked by an extension: {reason}");
        }
        if hook["input"].is_object() {
            *input = hook["input"].clone();
        }
        // `ext` calls are judged as the built-in tool they behave like: approval, plan mode and undo.
        let (pname, pinput) = tools::ext::effective(name, input);
        let pname = pname.as_str();
        if !(name == "submit" && self.submit_schema.is_some())
            && self.perms.mode() == permission::Mode::Plan
            && !permission::read_only(pname, &pinput)
        {
            bail!("plan mode is on: nothing may be changed. Investigate with read-only tools, present your plan, and wait for the user to leave plan mode.");
        }
        if let Some(key) = if name == "submit" && self.submit_schema.is_some() {
            None
        } else {
            self.perms.check(pname, &pinput, &self.cwd)
        } {
            match ask(tx, pname, permission::summary(pname, &pinput)).await {
                Decision::Allow => {}
                Decision::Always => self.perms.always(key),
                Decision::Deny => bail!("the user denied this action"),
            }
        }
        // Remember what the file held so `undo` can put it back.
        if let (true, Some(session), Some(path)) = (
            matches!(pname, "write" | "edit"),
            &self.session,
            pinput["path"].as_str(),
        ) {
            let path = permission::resolve(&self.cwd, path);
            let before = match std::fs::read(&path) {
                Ok(bytes) => String::from_utf8(bytes).ok().map(Some),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Some(None),
                Err(_) => None,
            };
            if let Some(before) = before {
                session.append(&json!({"checkpoint": {"path": path, "before": before}}))?;
            }
        }
        let mut result = match name {
            "read" => {
                let path = permission::resolve(&self.cwd, tools::str_arg(input, "path")?);
                if let Some(image) = context::read_image(&path)? {
                    self.images.lock().unwrap().push(image);
                    Ok(format!("Image: {}", path.display()))
                } else {
                    tools::run(name, input, &self.cwd).await
                }
            }
            name if tools::BUILTIN.contains(&name) => {
                // Shell descendants can outlive the task, even when `background` was not set.
                if name == "bash" {
                    if let Some(tree) = &self.task_worktree {
                        let mut state = tree.state.lock().unwrap();
                        if matches!(*state, WorktreeState::PristineCandidate) {
                            *state = WorktreeState::ShellUsed;
                        }
                    }
                }
                tools::run(name, input, &self.cwd).await
            }
            "computer" => {
                let (text, image) = tools::computer::run(input).await?;
                if let Some(data) = image {
                    let image = context::screenshot(&data)?;
                    let path = image["path"].as_str().unwrap();
                    let text = format!("{text}\n[screenshot: {path}]");
                    self.images.lock().unwrap().push(image);
                    Ok(text)
                } else {
                    Ok(text)
                }
            }
            "submit" if self.submit_schema.is_some() => self.submit(input),
            "task" if self.can_task() => self.task(input, tx).await,
            "ext" => self.ext(input, tx).await,
            "mcp" => self.mcp.run(input, &self.cwd).await,
            _ => self
                .lua
                .tool(name, input.clone())
                .await
                .map_err(anyhow::Error::msg),
        };
        if self.lua.has_hook("tool_result") {
            let payload = match &result {
                Ok(s) => json!({"tool": name, "input": input, "output": s, "is_error": false}),
                Err(e) => {
                    json!({"tool": name, "input": input, "output": format!("{e:#}"), "is_error": true})
                }
            };
            if let Some(out) = self.lua.hook("tool_result", payload).await["output"].as_str() {
                result = result.map(|_| out.to_string());
            }
        }
        result
    }

    async fn deliver(&mut self, wait: bool) -> bool {
        let first = if wait {
            let waiting = !self.jobs.lock().unwrap().is_empty();
            let woke = self
                .lend_slot(waiting, async {
                    tokio::select! {
                        biased;
                        _ = self.pump(true) => None,
                        result = std::future::poll_fn(|cx| self.jobs.lock().unwrap().poll_join_next(cx)) => Some(result),
                    }
                })
                .await;
            match woke {
                None => return true,
                Some(result) => result,
            }
        } else {
            self.jobs.lock().unwrap().try_join_next()
        };
        let mut next = first;
        let mut delivered = false;
        while let Some(outcome) = next {
            delivered = true;
            let message = match outcome {
                Ok(job) => match job.result {
                    Ok(result) => format!(
                        "Background task \"{}\" ({}) finished:\n{result}",
                        job.name, job.agent
                    ),
                    Err(error) => format!(
                        "Background task \"{}\" ({}) failed:\n{error}",
                        job.name, job.agent
                    ),
                },
                Err(error) => format!("Background task failed:\n{error}"),
            };
            self.push(Message::user(message));
            next = self.jobs.lock().unwrap().try_join_next();
        }
        delivered
    }

    /// A rule applies once when the model's reply (text or tool arguments) matches its trigger; its body reaches the model as the next user message.
    fn fire_rules(&self, blocks: &[Block], tx: &EventTx) {
        if self.rules.is_empty() {
            return;
        }
        let written: String = blocks
            .iter()
            .map(|b| match b {
                Block::Text { text } => text.clone(),
                Block::ToolUse { input, .. } => input.to_string(),
                _ => String::new(),
            })
            .collect::<Vec<_>>()
            .join("\n");
        let mut fired = self.fired.lock().unwrap();
        for rule in self.rules.iter().filter(|r| r.trigger.is_match(&written)) {
            if fired.insert(rule.name.clone()) {
                self.pending.lock().unwrap().push(format!(
                    "Rule \"{}\" applies to what you just wrote:\n{}",
                    rule.name, rule.body
                ));
                let _ = tx.send(Event::Notify(format!("ルール適用: {}", rule.name)));
            }
        }
    }

    async fn ext(&self, input: &Value, tx: &EventTx) -> Result<String> {
        let tool = input["tool"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("missing string argument `tool`"))?;
        let args = if input["args"].is_object() {
            input["args"].clone()
        } else {
            json!({})
        };
        let text = |key: &str| {
            args[key]
                .as_str()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .ok_or_else(|| anyhow::anyhow!("{tool} requires a non-empty `{key}`"))
        };
        if input["action"] == "call" && self.cfg.ext_off.iter().any(|off| off == tool) {
            bail!("ext tool `{tool}` is turned off in settings")
        }
        match input["action"].as_str() {
            Some("help") => tools::ext::help(tool),
            Some("call") if tool == "ask" => self.ask_user(&args, tx).await,
            Some("call") if tool == "checkpoint" => {
                *self.checkpoint.lock().unwrap() = Some(self.messages.len().saturating_sub(1));
                Ok("Checkpoint set. Explore, then call rewind with a concise report.".into())
            }
            Some("call") if tool == "rewind" => {
                let report = text("report")?;
                let at = self.checkpoint.lock().unwrap().ok_or_else(|| {
                    anyhow::anyhow!("no checkpoint is active; call checkpoint first")
                })?;
                *self.reset.lock().unwrap() = Some(Reset::Rewind(at, report));
                Ok("Rewinding to the checkpoint.".into())
            }
            Some("call") if tool == "new_context" => {
                *self.reset.lock().unwrap() = Some(Reset::Fresh(text("handoff")?));
                Ok("Restarting the context from the handoff.".into())
            }
            Some("call") if tool == "learn" => {
                if !self.cfg.learning.enabled {
                    bail!("learning is disabled in config.toml")
                }
                let raw = args["text"]
                    .as_str()
                    .ok_or_else(|| anyhow::anyhow!("learn requires `text`"))?
                    .trim();
                if raw.is_empty() {
                    bail!("learn requires non-empty `text`")
                }
                let text: String = raw
                    .chars()
                    .take(
                        self.cfg
                            .learning
                            .max_chars
                            .clamp(1, crate::context::MEMORY_LIMIT),
                    )
                    .collect();
                let input = json!({"scope": args["scope"].as_str().unwrap_or("project"), "action": "add", "text": text});
                tools::run("memory", &input, &self.cwd).await
            }
            Some("call") if tool == "advisor" => self.advise(&args, tx).await,
            Some("call") => tools::ext::run(tool, &args, &self.cwd).await,
            _ => bail!("ext: action must be help or call"),
        }
    }

    /// Questions in the GUI's `ask` dialog format; the answers come back as JSON.
    async fn ask_user(&self, args: &Value, tx: &EventTx) -> Result<String> {
        let questions = args["questions"]
            .as_array()
            .filter(|q| !q.is_empty())
            .ok_or_else(|| anyhow::anyhow!("ask requires a non-empty `questions` array"))?;
        let mut list = Vec::new();
        for (i, q) in questions.iter().enumerate() {
            let question = q["question"]
                .as_str()
                .filter(|s| !s.trim().is_empty())
                .ok_or_else(|| anyhow::anyhow!("every question needs a `question` string"))?;
            let options: Vec<Value> = q["options"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .map(|label| json!({"label": label}))
                .collect();
            list.push(json!({"id": format!("q{}", i + 1), "header": "", "question": question, "options": options, "multiSelect": q["multi"] == true}));
        }
        let (reply, rx) = oneshot::channel();
        let _ = tx.send(Event::Dialog {
            request: json!({"method": "ask", "title": "エージェントからの質問", "questions": list}),
            reply,
        });
        let answer = rx
            .await
            .map_err(|_| anyhow::anyhow!("no user can answer here"))?;
        if answer["cancelled"] == true || answer.get("answers").is_none() {
            bail!("the user dismissed the question")
        }
        Ok(answer["answers"].to_string())
    }

    /// One tool-less request to `advisor_model` with the tail of this conversation. Its usage is reported like any other model call.
    async fn advise(&self, args: &Value, tx: &EventTx) -> Result<String> {
        let question = args["question"]
            .as_str()
            .filter(|q| !q.trim().is_empty())
            .ok_or_else(|| anyhow::anyhow!("missing string argument `question`"))?;
        let spec = self.cfg.advisor_model.as_deref()
            .or_else(|| self.cfg.model_roles.get("advisor").map(String::as_str))
            .ok_or_else(|| anyhow::anyhow!("advisor is not configured: set advisor_model = \"provider/model\" in config.toml"))?;
        let (provider, model) = provider::make(&self.cfg, spec)?;
        let digest = crate::snap::serialize(&self.messages);
        let digest: String = digest
            .chars()
            .skip(digest.chars().count().saturating_sub(24_000))
            .collect();
        let messages = [Message::user(format!("{digest}\n\nQuestion: {question}"))];
        let system = "You advise another coding agent. Read its conversation and answer its question briefly: concrete mistakes, risks and the next step. You have no tools.";
        let reply = provider
            .complete(
                Request {
                    model: &model,
                    system,
                    messages: &messages,
                    tools: &[],
                    max_tokens: 2000,
                },
                &|_| {},
            )
            .await?;
        let _ = tx.send(Event::UsageAux(reply.usage));
        let text: Vec<&str> = reply
            .blocks
            .iter()
            .filter_map(|b| {
                if let Block::Text { text } = b {
                    Some(text.as_str())
                } else {
                    None
                }
            })
            .collect();
        if text.is_empty() {
            bail!("the advisor returned no text");
        }
        Ok(text.join("\n"))
    }

    async fn auto_learn(&self, tx: &EventTx) -> Result<()> {
        let max_chars = self
            .cfg
            .learning
            .max_chars
            .clamp(1, crate::context::MEMORY_LIMIT);
        // Without a dedicated learning model, lessons come from the reply itself: no extra request.
        let spec = self
            .cfg
            .learning
            .model
            .as_deref()
            .or_else(|| self.cfg.model_roles.get("learning").map(String::as_str));
        let text = if let Some(spec) = spec {
            let (provider, model) = provider::make(&self.cfg, spec)?;
            let digest = crate::snap::serialize(&self.messages);
            let tail: String = digest
                .chars()
                .skip(digest.chars().count().saturating_sub(12_000))
                .collect();
            let prompt = format!("Extract at most one durable coding lesson from this completed conversation. Return either NO_LESSON or one concise fact under {} characters, with no bullets or preamble.\n\n{}", max_chars, tail);
            let messages = [Message::user(prompt)];
            let reply = provider.complete(Request { model: &model, system: "You distill only reusable project facts. Never include secrets, credentials, tokens, personal data, or temporary task details.", messages: &messages, tools: &[], max_tokens: 300 }, &|_| {}).await?;
            let _ = tx.send(Event::UsageAux(reply.usage));
            reply
                .blocks
                .iter()
                .find_map(|b| {
                    if let Block::Text { text } = b {
                        Some(text.trim())
                    } else {
                        None
                    }
                })
                .map(|text| text.trim_matches('`').trim().to_string())
                .filter(|text| !text.is_empty() && !text.eq_ignore_ascii_case("NO_LESSON"))
        } else {
            self.messages.last().and_then(|message| {
                message.content.iter().find_map(|block| {
                    let Block::Text { text } = block else {
                        return None;
                    };
                    text.lines().find_map(|line| {
                        let line = line.trim();
                        line.strip_prefix("Lesson:")
                            .or_else(|| line.strip_prefix("Durable lesson:"))
                            .map(str::trim)
                            .filter(|text| !text.is_empty())
                            .map(str::to_string)
                    })
                })
            })
        };
        let Some(text) = text else { return Ok(()) };
        let text: String = text.chars().take(max_chars).collect();
        let input = json!({"scope": self.cfg.learning.scope, "action": "add", "text": text});
        if let Some(key) = self.perms.check("memory", &input, &self.cwd) {
            match ask(tx, "memory", permission::summary("memory", &input)).await {
                Decision::Deny => bail!("the user denied this action"),
                Decision::Always => self.perms.always(key),
                Decision::Allow => {}
            }
        }
        tools::run("memory", &input, &self.cwd).await.map(|_| ())
    }

    fn submit(&self, input: &Value) -> Result<String> {
        let schema = self.submit_schema.as_ref().unwrap();
        let object = input
            .as_object()
            .ok_or_else(|| anyhow::anyhow!("submit: arguments must be an object"))?;
        if let Some(required) = schema["required"].as_array() {
            for field in required.iter().filter_map(Value::as_str) {
                if !object.contains_key(field) {
                    bail!("submit: missing required field `{field}`");
                }
            }
        }
        if let Some(properties) = schema["properties"].as_object() {
            for (field, property) in properties {
                if let (Some(value), Some(kind)) = (object.get(field), property["type"].as_str()) {
                    let valid = match kind {
                        "string" => value.is_string(),
                        "number" => value.is_number(),
                        "integer" => value.as_i64().is_some() || value.as_u64().is_some(),
                        "boolean" => value.is_boolean(),
                        "array" => value.is_array(),
                        "object" => value.is_object(),
                        _ => continue,
                    };
                    if !valid {
                        bail!("submit: field `{field}` must be a {kind}");
                    }
                }
            }
        }
        *self.submitted.lock().unwrap() = Some(input.clone());
        Ok("Result recorded.".into())
    }

    fn task<'a>(&'a self, input: &'a Value, tx: &'a EventTx) -> BoxFut<'a, Result<String>> {
        Box::pin(async move {
            let tasks = input["tasks"]
                .as_array()
                .ok_or_else(|| anyhow::anyhow!("task requires `tasks` array"))?;
            if tasks.is_empty() || tasks.len() > 8 {
                bail!("task requires 1 to 8 tasks");
            }
            let defs: Vec<_> = crate::subagents::discover(&self.cwd)
                .into_iter()
                .filter(|d| self.cfg.task.agent_enabled(&d.name))
                .collect();
            let mut specs = Vec::new();
            for (index, item) in tasks.iter().enumerate() {
                let prompt = item["prompt"]
                    .as_str()
                    .filter(|s| !s.trim().is_empty())
                    .ok_or_else(|| anyhow::anyhow!("task requires a non-empty `prompt`"))?;
                let agent = match item.get("agent") {
                    None => "task",
                    Some(v) => v
                        .as_str()
                        .ok_or_else(|| anyhow::anyhow!("task `agent` must be a string"))?,
                };
                if !self.cfg.task.agent_enabled(agent) {
                    bail!("agent `{agent}` is disabled by task.disabled_agents");
                }
                let def = defs
                    .iter()
                    .find(|d| d.name == agent)
                    .cloned()
                    .ok_or_else(|| {
                        anyhow::anyhow!(
                            "unknown agent `{agent}`; available: {}",
                            defs.iter()
                                .map(|d| d.name.as_str())
                                .collect::<Vec<_>>()
                                .join(", ")
                        )
                    })?;
                // Names address running tasks (steer, stop, GUI rows), so they must not collide with earlier calls.
                let taken = |n: &str, specs: &[TaskSpec]| {
                    specs.iter().any(|s| s.name == n) || self.subs.lock().unwrap().contains_key(n)
                };
                let name = match item.get("name") {
                    None => (index + 1..)
                        .map(|i| format!("{agent}{i}"))
                        .find(|n| !taken(n, &specs))
                        .unwrap(),
                    Some(v) => v
                        .as_str()
                        .filter(|s| !s.trim().is_empty())
                        .ok_or_else(|| anyhow::anyhow!("task `name` must be a non-empty string"))?
                        .to_string(),
                };
                if taken(&name, &specs) {
                    bail!("task name `{name}` is already in use");
                }
                let model = match item.get("model") {
                    Some(v) => {
                        let spec = v.as_str().ok_or_else(|| {
                            anyhow::anyhow!("task `model` must be a provider/model string")
                        })?;
                        self.cfg
                            .task
                            .check_call_model(spec)
                            .map_err(anyhow::Error::msg)?;
                        Some(spec.to_string())
                    }
                    None => self
                        .cfg
                        .task
                        .agent_model(&def)
                        .or_else(|| self.meta.subagent_model.clone())
                        .or_else(|| self.cfg.model_roles.get("task").cloned()),
                };
                let isolated = match item.get("isolated") {
                    Some(v) => v
                        .as_bool()
                        .ok_or_else(|| anyhow::anyhow!("task `isolated` must be a boolean"))?,
                    None => false,
                };
                if isolated && !self.cfg.task.isolation.enabled {
                    bail!("task isolation is disabled by task.isolation.enabled");
                }
                let schema = match item.get("schema") {
                    Some(_) if !self.cfg.task.structured => {
                        bail!("task `schema` is disabled by task.structured")
                    }
                    Some(v) if v.is_object() && v["type"] == "object" => Some(v.clone()),
                    Some(_) => bail!("task `schema` must be a JSON Schema with type object"),
                    None => None,
                };
                let context = input["context"].as_str().unwrap_or("");
                let prompt = if context.is_empty() {
                    prompt.to_string()
                } else {
                    format!("{context}\n\n---\n\n{prompt}")
                };
                specs.push(TaskSpec {
                    name,
                    def,
                    prompt,
                    model,
                    isolated,
                    schema,
                });
            }
            let background = match input.get("background") {
                Some(v) => v
                    .as_bool()
                    .ok_or_else(|| anyhow::anyhow!("task `background` must be a boolean"))?,
                None => false,
            };
            if background && !self.cfg.task.background {
                bail!("background tasks are disabled by task.background");
            }
            let names = specs.iter().map(|s| s.name.clone()).collect::<Vec<_>>();
            let mut foreground = tokio::task::JoinSet::new();
            for spec in specs {
                let (steer_tx, steer_rx) = mpsc::unbounded_channel();
                let cancel = Arc::new(tokio::sync::Notify::new());
                {
                    let mut subs = self.subs.lock().unwrap();
                    anyhow::ensure!(
                        !subs.contains_key(&spec.name),
                        "task name `{}` is already in use",
                        spec.name
                    );
                    subs.insert(
                        spec.name.clone(),
                        SubCtl {
                            steer: steer_tx.clone(),
                            cancel: cancel.clone(),
                        },
                    );
                }
                let registration = SubRegistration {
                    subs: self.subs.clone(),
                    name: spec.name.clone(),
                    cancel: cancel.clone(),
                };
                let sub = Agent {
                    cfg: self.cfg.clone(),
                    provider: self.provider.clone(),
                    model: self.model.clone(),
                    cwd: self.cwd.clone(),
                    system: self.system.clone(),
                    prompt_context: self.prompt_context.clone(),
                    latest_context: String::new(),
                    messages: vec![],
                    meta: self.meta.clone(),
                    session: None,
                    snap_source: None,
                    perms: self.perms.clone(),
                    lua: self.lua.clone(),
                    mcp: self.mcp.clone(),
                    subagent: true,
                    review: None,
                    task_worktree: self.task_worktree.clone(),
                    slots: self.slots.clone(),
                    slot: Default::default(),
                    allowed: spec.def.tools.clone(),
                    submit_schema: spec.schema.clone(),
                    submitted: Default::default(),
                    jobs: Default::default(),
                    context: 0,
                    started: true,
                    images: Default::default(),
                    steer_tx,
                    steer_rx: Mutex::new(steer_rx),
                    subs: Default::default(),
                    pending: Default::default(),
                    depth: self.depth + 1,
                    rules: self.rules.clone(),
                    fired: Default::default(),
                    checkpoint: Default::default(),
                    reset: Default::default(),
                };
                let tx = tx.clone();
                let limit = self.cfg.task.runtime();
                let future = async move {
                    let run = async {
                        let permit = sub
                            .slots
                            .clone()
                            .acquire_owned()
                            .await
                            .expect("semaphore closed");
                        *sub.slot.lock().unwrap() = Some(permit);
                        let _ = tx.send(Event::Notify(format!(
                            "▸ {} ({}) started",
                            spec.name, spec.def.name
                        )));
                        Self::sub_event(&tx, &spec, "running", "");
                        let running = Self::run_task(sub, &spec, &tx);
                        match limit {
                            Some(limit) => tokio::time::timeout(limit, running)
                                .await
                                .unwrap_or_else(|_| {
                                    Err(anyhow::anyhow!("timed out after {} ms", limit.as_millis()))
                                }),
                            None => running.await,
                        }
                        .map_err(|e| format!("{e:#}"))
                    };
                    let result = tokio::select! {
                        biased;
                        _ = cancel.notified() => Err("cancelled by the user".into()),
                        result = run => result,
                    };
                    let (state, detail) = match &result {
                        Ok(text) => ("done", text.as_str()),
                        Err(error) if error == "cancelled by the user" => {
                            ("cancelled", error.as_str())
                        }
                        Err(error) => ("failed", error.as_str()),
                    };
                    Self::sub_event(&tx, &spec, state, detail);
                    drop(registration);
                    match &result {
                        Ok(_) => {
                            let _ = tx.send(Event::Notify(format!("✓ {} finished", spec.name)));
                        }
                        Err(error) => {
                            let _ = tx.send(Event::Notify(format!(
                                "✗ {} failed: {}",
                                spec.name,
                                error.lines().next().unwrap_or("")
                            )));
                        }
                    }
                    JobOutcome {
                        name: spec.name,
                        agent: spec.def.name,
                        result,
                    }
                };
                if background {
                    self.jobs.lock().unwrap().spawn(future);
                } else {
                    foreground.spawn(future);
                }
            }
            if background {
                return Ok(format!("Started in the background: {}. Their results arrive in a later message; do not wait for them with tools.", names.join(", ")));
            }
            let waited = self
                .lend_slot(true, async {
                    let mut outcomes = Vec::new();
                    loop {
                        let job = tokio::select! {
                            job = foreground.join_next() => job,
                            _ = self.pump(false) => unreachable!(),
                        };
                        match job {
                            Some(job) => outcomes.push(
                                job.map_err(|e| anyhow::anyhow!("sub-agent crashed: {e}"))?,
                            ),
                            None => break,
                        }
                    }
                    Ok::<_, anyhow::Error>(outcomes)
                })
                .await;
            let mut outcomes = waited?;
            outcomes.sort_by_key(|job| {
                names
                    .iter()
                    .position(|name| *name == job.name)
                    .unwrap_or(usize::MAX)
            });
            let all_failed = outcomes.iter().all(|job| job.result.is_err());
            let single = outcomes.len() == 1;
            let result = outcomes
                .into_iter()
                .map(|job| match job.result {
                    Ok(text) if single => text,
                    Ok(text) => format!("## {} ({})\n{text}", job.name, job.agent),
                    Err(error) => format!("## {} ({}) failed\n{error}", job.name, job.agent),
                })
                .collect::<Vec<_>>()
                .join("\n\n");
            if all_failed {
                bail!("{result}");
            }
            Ok(result)
        })
    }

    fn sub_event(tx: &EventTx, spec: &TaskSpec, state: &str, detail: &str) {
        let detail: String = detail
            .lines()
            .next()
            .unwrap_or("")
            .trim()
            .chars()
            .take(80)
            .collect();
        let _ = tx.send(Event::Remote {
            command: "subagent".into(),
            data: json!({
                "name": spec.name, "agent": spec.def.name, "state": state, "detail": detail
            }),
        });
    }

    fn sub_forward(tx: &EventTx, spec: &TaskSpec, ev: Event) {
        if let Event::ToolStart { name, input, .. } = &ev {
            let arg = ["path", "command", "pattern", "url", "query"]
                .iter()
                .find_map(|key| input[*key].as_str());
            let detail = match arg {
                Some(arg) => format!("{name} {arg}"),
                None => name.clone(),
            };
            Self::sub_event(tx, spec, "running", &detail);
        }
        if matches!(
            ev,
            Event::Permission { .. } | Event::Notify(_) | Event::UsageAux(_)
        ) {
            let _ = tx.send(ev);
        }
    }

    fn run_task<'a>(
        mut sub: Agent,
        spec: &'a TaskSpec,
        tx: &'a EventTx,
    ) -> BoxFut<'a, Result<String>> {
        Box::pin(async move {
            if let Some(spec) = &spec.model {
                let (name, model) =
                    crate::subagents::model_spec(spec).map_err(anyhow::Error::msg)?;
                if !sub.cfg.providers.contains_key(name)
                    && sub
                        .meta
                        .model
                        .split_once('/')
                        .is_some_and(|(p, _)| p == name)
                {
                    sub.model = model.to_string();
                } else {
                    (sub.provider, sub.model) = provider::make(&sub.cfg, spec)?;
                }
                sub.meta.model.clone_from(spec);
            }
            let worktree = if spec.isolated {
                let id = format!(
                    "sub-{}-{}",
                    std::process::id(),
                    WORKTREE_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                );
                let initial = crate::worktree::head(&sub.cwd)?;
                let path = crate::worktree::create(
                    &sub.cwd,
                    &crate::home().join("subagent-worktrees"),
                    &id,
                )?;
                let tree = Arc::new(TaskWorktree {
                    repo: sub.meta.cwd.clone(),
                    path,
                    branch: format!("greencode/{id}"),
                    initial,
                    state: Mutex::new(WorktreeState::PristineCandidate),
                    tx: tx.clone(),
                });
                sub.task_worktree = Some(tree.clone());
                sub.cwd = tree.path.clone();
                sub.lua = Arc::new(Lua::new(&tree.path));
                sub.mcp = Arc::new(Mcp::load_with_project(&tree.path, sub.cfg.mcp_project_config));
                Some(tree)
            } else {
                None
            };
            let (stx, mut srx) = mpsc::unbounded_channel();
            // The role rides in the first user message: the child's system and context prefix stay
            // byte-identical to the parent's, so the provider cache is shared.
            // The shared context names the parent's directory; an isolated child must not write there.
            let place = worktree.as_ref().map(|tree| format!("\nYour working directory is the isolated worktree {}: use relative paths or paths under it, never the parent's directory.", tree.path.display())).unwrap_or_default();
            let role = format!(
                "# Role: {}\n{}{}{place}",
                spec.name,
                spec.def.prompt,
                if spec.schema.is_some() {
                    "\nFinish by calling submit with the result; do not answer in plain text."
                } else {
                    ""
                }
            );
            let prompt = format!("{role}\n\n---\n\n{}", spec.prompt);
            let result = {
                let run = sub.run_turn(prompt, &stx);
                tokio::pin!(run);
                loop {
                    tokio::select! {
                        r = &mut run => break r,
                        Some(ev) = srx.recv() => Self::sub_forward(tx, spec, ev),
                    }
                }
            };
            while let Ok(ev) = srx.try_recv() {
                Self::sub_forward(tx, spec, ev);
            }
            result?;
            let mut text =
                if spec.schema.is_some() {
                    let submitted = sub.submitted.lock().unwrap();
                    serde_json::to_string(submitted.as_ref().ok_or_else(|| {
                        anyhow::anyhow!("sub-agent finished without calling submit")
                    })?)?
                } else {
                    sub.messages
                        .last()
                        .into_iter()
                        .flat_map(|m| &m.content)
                        .filter_map(|b| match b {
                            Block::Text { text } => Some(text.as_str()),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join("\n")
                };
            if let Some(tree) = worktree {
                *tree.state.lock().unwrap() = WorktreeState::Completed;
                text.push_str(&format!(
                    "\n\n[worktree {}, branch {}]\n{}",
                    tree.path.display(),
                    tree.branch,
                    crate::worktree::summary(&tree.path)
                ));
            }
            Ok(text)
        })
    }
}

impl Backend for Agent {
    fn live(&self) -> Option<mpsc::UnboundedSender<Value>> {
        Some(self.steer_tx.clone())
    }

    /// Dropping the turn future stops foreground sub-agents; background jobs must be aborted explicitly.
    fn cancel<'a>(&'a mut self, tx: &'a EventTx) -> BoxFut<'a, Result<()>> {
        self.review = None;
        let mut jobs = std::mem::take(self.jobs.get_mut().unwrap());
        // Dropped futures never report back, so their rows would stay "running" in the GUI.
        for name in std::mem::take(&mut *self.subs.lock().unwrap()).into_keys() {
            let _ = tx.send(Event::Remote {
                command: "subagent".into(),
                data: json!({"name": name, "state": "cancelled", "detail": ""}),
            });
        }
        Box::pin(async move {
            jobs.shutdown().await;
            Ok(())
        })
    }

    fn set_subagent_model(&mut self, model: Option<String>) -> Result<()> {
        if let Some(session) = &self.session {
            let mut meta = self.meta.clone();
            meta.subagent_model = model.clone();
            session.append(&json!({"meta": meta}))?;
        }
        self.meta.subagent_model = model;
        Ok(())
    }

    /// Never asks for approval: only global and already-trusted extensions are loaded here.
    fn control<'a>(&'a mut self, command: Value, tx: &'a EventTx) -> BoxFut<'a, Result<()>> {
        Box::pin(async move {
            if command["type"] == "review_mode" {
                self.review = match command["scope"].as_str() {
                    Some("local") => Some(Review::Local),
                    Some("remote") => Some(Review::Remote),
                    _ => bail!("invalid native review scope"),
                };
                return Ok(());
            }
            if !matches!(
                command["type"].as_str(),
                Some("get_commands" | "get_available_commands")
            ) {
                bail!("this backend does not support native RPC commands");
            }
            self.refresh_extensions(tx).await;
            self.advertise(tx);
            Ok(())
        })
    }

    fn prompt<'a>(&'a mut self, text: String, tx: &'a EventTx) -> BoxFut<'a, Result<()>> {
        Box::pin(async move {
            // A cancelled turn can leave tool calls without results; fix them before anything is appended.
            self.repair();
            if let Some(dir) = self.cfg.extensions_enabled.then(|| self.lua.untrusted()).flatten() {
                if ask(tx, "load project extensions", dir.display().to_string()).await
                    != Decision::Deny
                {
                    self.lua.trust();
                }
            }
            self.refresh_extensions(tx).await;
            self.advertise(tx);
            let saved = Session::runtime(&self.meta.id)?;
            if saved["backend"] == "native" && self.prompt_context.is_empty() {
                self.prompt_context = saved["prompt_context"]
                    .as_str()
                    .map(str::to_owned)
                    .unwrap_or_else(|| context::context_message(&self.cwd, &self.cfg));
            }
            // Workspace files changing mid-session append a snapshot; the system prompt and earlier
            // history stay untouched, so the provider's cached prefix survives.
            let current = context::session_context(&self.cwd, &self.cfg);
            if current != self.latest_context {
                self.push(Message::user(context::update_message(&current)));
                self.latest_context = current;
                self.save_context()?;
            }
            if !self.started {
                self.started = true;
                self.lua
                    .hook("session_start", json!({ "cwd": self.cwd }))
                    .await;
                self.notes(tx);
            }
            let mut prompt = text;
            if let Some(rest) = prompt.strip_prefix('/') {
                let (name, args) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
                if name == "compact" {
                    return match args.trim() {
                        "" => self.compact_auto(tx).await,
                        "snap" => self.compact(tx, Method::Snap).await,
                        "summary" => self.compact(tx, Method::Summary).await,
                        _ => bail!("usage: /compact [snap|summary]"),
                    };
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
            let hook = self
                .lua
                .hook("prompt_submit", json!({ "prompt": prompt }))
                .await;
            if let Some(reason) = hook["deny"].as_str() {
                bail!("blocked by an extension: {reason}");
            }
            if let Some(p) = hook["prompt"].as_str() {
                prompt = p.to_string();
            }
            let result = self.run_turn(prompt, tx).await;
            self.review = None;
            result
        })
    }
}
