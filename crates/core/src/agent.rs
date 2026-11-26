//! The native agent loop.

use crate::{
    config::{
        agent_loop::{InterruptMode, SleepPrevention, SteeringMode, UnexpectedStopDetection},
        compaction::Method,
        Config,
    },
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
    /// Another harness: the task runs as a delegated child thread instead of a native sub-agent.
    backend: Option<String>,
    isolated: bool,
    schema: Option<Value>,
    /// Resolved, ceiling-clamped reasoning effort for this spawn.
    effort: Option<String>,
    /// Ordered steps: the subagent gets the first, then each next one after it finishes the previous.
    steps: Vec<String>,
    /// The model this spawn actually runs on (its own, the agent override, or the parent's), for the badge.
    resolved_model: String,
}
/// `task.agentIdleTtlMs`: where a named subagent's transcript waits on disk while it is not running.
fn parked_file(session: &str, depth: u8, name: &str) -> PathBuf {
    let safe: String = name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || "-_.".contains(c) { c } else { '_' })
        .collect();
    crate::home().join("parked").join(session).join(format!("{depth}-{safe}.json"))
}

fn park_subagent(sub: &Agent, name: &str) -> Result<()> {
    let file = parked_file(&sub.meta.id, sub.depth, name);
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(&file, serde_json::to_vec(&json!({"agent": sub.agent, "messages": sub.messages}))?)?;
    Ok(())
}

/// How a finished named agent is found again among the live ones kept by `task.agentIdleTtlMs = 0`.
fn resident_key(depth: u8, name: &str) -> String {
    format!("{depth}-{name}")
}

/// Takes a parked transcript for `name`; one parked under another agent definition is stale and dropped.
fn take_parked(session: &str, depth: u8, name: &str, agent: &str) -> Option<Vec<Message>> {
    let file = parked_file(session, depth, name);
    let value: Value = serde_json::from_slice(&std::fs::read(&file).ok()?).ok()?;
    let _ = std::fs::remove_file(&file);
    (value["agent"].as_str() == Some(agent))
        .then(|| serde_json::from_value(value["messages"].clone()).ok())
        .flatten()
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

/// `task.isolation.apply|merge|commits`: apply a completed isolated task's changes, or keep a patch.
async fn integrate_isolated(cfg: &crate::config::Config, spec: &TaskSpec, tree: &TaskWorktree) -> Result<String> {
    let patch = crate::worktree::capture_patch(&tree.path)?;
    let id = tree.path.file_name().and_then(|n| n.to_str()).unwrap_or("task");
    let keep_patch = |patch: &str| -> Option<std::path::PathBuf> {
        let file = crate::worktree::patch_file(&cfg.worktree.dir(), id);
        std::fs::create_dir_all(cfg.worktree.dir()).ok()?;
        std::fs::write(&file, patch).ok()?;
        Some(file)
    };
    if !cfg.task.isolation.apply {
        let file = keep_patch(&patch).ok_or_else(|| anyhow::anyhow!("cannot write the isolated patch under {}", cfg.worktree.dir().display()))?;
        return Ok(format!(
            "\n\n[worktree {}, branch {}]\n{}\nPatch kept in {}",
            tree.path.display(),
            tree.branch,
            crate::worktree::summary(&tree.path),
            file.display()
        ));
    }
    let message = match cfg.task.isolation.commits {
        crate::subagents::IsolationCommits::Generic => format!("task {}: isolated changes", spec.name),
        crate::subagents::IsolationCommits::Ai => {
            let model = spec.model.clone().unwrap_or_else(|| cfg.model.clone());
            crate::textgen::commit_message(cfg, &model, &patch)
                .await
                .unwrap_or_else(|_| format!("task {}: isolated changes", spec.name))
        }
    };
    let applied = if cfg.task.isolation.merge == crate::subagents::IsolationMerge::Patch {
        crate::worktree::apply_patch(&tree.repo, &patch)
    } else {
        match crate::worktree::commit_all(&tree.path, &message) {
            Ok(true) => crate::worktree::merge_branch(&tree.repo, &tree.branch, &message),
            Ok(false) => Ok(()),
            Err(e) => Err(e),
        }
    };
    if let Err(e) = applied {
        let kept = keep_patch(&patch).map(|file| format!("; changes kept in {}", file.display())).unwrap_or_default();
        return Err(anyhow::anyhow!("{e:#}{kept}"));
    }
    crate::worktree::remove_integrated(&tree.repo, &tree.path, &tree.branch)?;
    Ok(format!("\n\n[isolated worktree {} applied to the parent checkout; branch {} removed]", tree.path.display(), tree.branch))
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
    /// Sub-agent definition name; `None` for the main agent. Drives per-agent task settings.
    agent: Option<String>,
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
    /// `task.agentIdleTtlMs` 0: finished named agents kept in memory, so messaging one again continues its history.
    resident: Arc<Mutex<HashMap<String, Vec<Message>>>>,
    pending: Mutex<Vec<String>>,
    last_tool_call: Option<String>,
    tool_call_repeats: u32,
    depth: u8,
    /// Time-Traveling Stream Rules: patterns matched against the model's own streaming output.
    ttsr: Arc<crate::rules::Ttsr>,
    /// Passive advisor reviews and the prewalk hand-off.
    assist: crate::assist::Assist,
    /// Spend of this agent's own requests in USD, when their models have `prices` entries.
    spent: Mutex<f64>,
    /// The `cost.warn` notice already fired.
    cost_warned: std::sync::atomic::AtomicBool,
    /// Steering this request can carry mid-response (Codex/Responses WebSocket).
    live: provider::LiveSteer,
    /// Which fallback model this agent runs, and which primaries are cooling down.
    fallback: tokio::sync::Mutex<provider::fallback::State>,
    /// Warmers for the last request's prompt cache, and whether a turn is running.
    warming: provider::warm::Gates,
    warmer: Mutex<Option<tokio::task::JoinHandle<()>>>,
    /// Index of the assistant message that called `checkpoint`.
    checkpoint: Mutex<Option<usize>>,
    reset: Mutex<Option<Reset>>,
    /// An idle compaction already ran since the last prompt; the next prompt re-arms it.
    idle_done: bool,
    /// Memory backend for this session (`memory.backend`); `None` when off.
    memory: Option<Arc<dyn crate::memory::Backend>>,
    /// Completed user turns, for the backend's `retain_due` cadence.
    memory_turns: u32,
    /// Automatic recall runs on the first turn only.
    memory_recalled: bool,
    /// A background summary started by `compaction.asyncEnabled`, with the message count it covers.
    speculative: Option<Speculative>,
    /// Completion reminders sent for the current prompt, and mid-run nudge accounting.
    todo_reminders: u64,
    todo_mutations: u32,
    todo_nudges: u32,
    todo_nudge_gap: u32,
}

/// The latest todo list the model sent, in call order; native keeps it only in the transcript.
fn todo_items(messages: &[Message]) -> Vec<(String, bool)> {
    messages
        .iter()
        .rev()
        .find_map(|message| {
            message.content.iter().rev().find_map(|block| match block {
                Block::ToolUse { name, input, .. } if name == "todo" => Some(
                    input["todos"]
                        .as_array()
                        .map(|todos| {
                            todos
                                .iter()
                                .map(|todo| (todo["text"].as_str().unwrap_or("").to_string(), todo["done"] == true))
                                .collect()
                        })
                        .unwrap_or_default(),
                ),
                _ => None,
            })
        })
        .unwrap_or_default()
}

/// A background summary started by `compaction.asyncEnabled`.
struct Speculative {
    handle: tokio::task::JoinHandle<Result<String>>,
    /// Message count the summary covers; the rest stays verbatim.
    split: usize,
}

/// True when a text block holds nothing but a tool-call markup, so it is not also sent as prose.
fn b_text_is_call_only(block: &Block) -> bool {
    match block {
        Block::Text { text } => {
            let trimmed = text.trim();
            trimmed.is_empty()
                || trimmed.starts_with("<tool_call>")
                || trimmed.starts_with("<function_calls>")
                || trimmed.starts_with("<|tool_call")
                || trimmed.starts_with("```tool_code")
                || trimmed.starts_with("<｜tool▁calls▁begin｜>")
                || trimmed.starts_with("<|start|>")
                || trimmed.starts_with("<minimax:tool_call>")
                || trimmed.starts_with("<invoke")
        }
        _ => false,
    }
}

fn todo_tool_enabled(cfg: &Config) -> bool {
    !cfg.no_tools && (cfg.tools.is_empty() || cfg.tools.iter().any(|tool| tool == "todo"))
}

fn incomplete_todos(messages: &[Message]) -> Vec<String> {
    todo_items(messages).into_iter().filter(|(_, done)| !done).map(|(text, _)| text).collect()
}

fn broad_output(name: &str) -> bool {
    matches!(name, "bash" | "grep" | "glob" | "web_fetch")
}

const LOOP_REMINDER: &str = "Your previous reply repeated itself in a loop and was discarded. Continue the task without repeating that block.";
const TOOL_CALL_REMINDER: &str = "Your reasoning only wrote planning headers and never called a tool. Stop planning and call a tool now.";
const CONTINUE_REMINDER: &str = "You stopped without a visible reply. Continue the task: call a tool or give the final answer.";
const CUT_OFF_REMINDER: &str = "Your reply hit the output token limit and was cut off. Continue exactly where it stopped, without repeating or summarizing what you already wrote.";
const UNEXPECTED_STOP_REMINDER: &str = "You said you would continue with a tool call or action but stopped. The task is incomplete: call the next tool now and do not repeat prior analysis. If no tool is needed, give the final answer now.";
/// OMP's `unexpected-stop` judge question, criteria kept as examples.
const STOP_JUDGE: &str = "A coding agent ended its turn with the final message below. Did it stop unexpectedly? \
A turn that finishes the request, answers it, or asks about something the request left open ends normally, even when it says \
what it would do next. Asking permission for work the request already asked for is an unexpected stop.\n\
Unexpected stops:\n- \"I should do the same for the JS eval worker. Doing that now.\"\n- \"Let me run the tests next.\"\n- \"I'll fix that now.\"\n\
- request \"fix the failing test\", message \"Found the bug. Should I fix it?\"\n\
Not unexpected stops:\n- \"I've completed the task.\"\n- \"The fix is done and tests pass.\"\n\
- \"Option 1 is simpler. If you want it, I'll start by finding where the flag is set.\"\n\
- request \"explain the crash, don't change anything\", message \"The cache is stale. Should I fix it?\"\n\
Reply with one word: yes or no.";

/// `one-at-a-time` leaves the rest queued for the next request.
fn steering_batch(queued: &mut Vec<String>, mode: SteeringMode) -> Vec<String> {
    let take = match mode {
        SteeringMode::All => queued.len(),
        SteeringMode::OneAtATime => usize::from(!queued.is_empty()),
    };
    queued.drain(..take).collect()
}

fn call_key(name: &str, input: &Value) -> String {
    format!("{name}\0{input}")
}

/// Exempt tool names, including one run as the command of a `bash` call.
/// shortcut: only the first command word is matched, so a prefixed exempt command (`cd x && …`) still counts.
fn exempt_call(name: &str, input: &Value, exempt: &[String]) -> bool {
    let listed = |word: &str| exempt.iter().any(|tool| tool == word);
    listed(name)
        || (name == "bash"
            && input["command"]
                .as_str()
                .and_then(|command| command.split_whitespace().next())
                .is_some_and(listed))
}

/// Whether this call reaches the cross-turn repeat threshold; exempt calls leave the run untouched.
fn call_repeats(
    last: &mut Option<String>,
    repeats: &mut u32,
    name: &str,
    input: &Value,
    guard: &crate::config::agent_loop::ToolCallLoopGuardCfg,
) -> bool {
    if !guard.enabled || exempt_call(name, input, &guard.exempt_tools) {
        return false;
    }
    let key = call_key(name, input);
    if last.as_deref() == Some(key.as_str()) {
        *repeats = repeats.saturating_add(1);
    } else {
        *last = Some(key);
        *repeats = 1;
    }
    if *repeats >= guard.threshold {
        *repeats = 0;
        return true;
    }
    false
}

/// Many consecutive Markdown planning headers and no tool call: a Gemini reasoning stream that
/// never converged. Threshold matches OMP's loop guard.
const PLAN_HEADER_LIMIT: usize = 64;

fn plan_header_runaway(blocks: &[Block]) -> bool {
    let mut headers = 0;
    for text in blocks.iter().filter_map(|b| match b {
        Block::Text { text } => Some(text.as_str()),
        Block::Other(v) if v["type"] == "thinking" => v["thinking"].as_str(),
        _ => None,
    }) {
        for line in text.lines() {
            let line = line.trim_start();
            if line.starts_with('#') {
                headers += 1;
                if headers >= PLAN_HEADER_LIMIT {
                    return true;
                }
            }
        }
    }
    false
}

/// Counts Markdown header lines across streamed chunks (a chunk boundary can split one), so the
/// same threshold can cut a planning-only Gemini reply before it finishes.
struct PlanHeaderCount {
    headers: usize,
    line_start: bool,
}

impl Default for PlanHeaderCount {
    fn default() -> Self {
        Self { headers: 0, line_start: true }
    }
}

impl PlanHeaderCount {
    /// Appends the chunk and returns true once the consecutive header run reaches the limit.
    fn feed(&mut self, chunk: &str) -> bool {
        for ch in chunk.chars() {
            if ch == '\n' {
                self.line_start = true;
            } else if self.line_start {
                if ch == '#' {
                    self.headers += 1;
                    self.line_start = false;
                    if self.headers >= PLAN_HEADER_LIMIT {
                        return true;
                    }
                } else if !ch.is_whitespace() {
                    self.line_start = false;
                }
            }
        }
        false
    }
}

fn reply_repeats(blocks: &[Block], check_content: bool) -> bool {
    blocks.iter().any(|block| match block {
        Block::Text { text } => check_content && repeats(text),
        Block::Other(v) => v["thinking"].as_str().is_some_and(repeats),
        _ => false,
    })
}

/// OMP's verbatim tail shape: the same cycle at least three times, 180+ chars (1024+ for long cycles).
fn repeats(text: &str) -> bool {
    let all: Vec<char> = text.chars().collect();
    if all.len() < 180 {
        return false;
    }
    let chars = &all[all.len().saturating_sub(4096)..];
    let n = chars.len();
    for period in 1..=n / 3 {
        let need = if period > 64 { 1024 } else { 180 };
        let suffix = &chars[n - period..];
        if suffix.iter().all(|c| c.is_whitespace()) {
            continue;
        }
        let mut runs = 0;
        let mut end = n;
        while end >= period && &chars[end - period..end] == suffix {
            runs += 1;
            end -= period;
        }
        if runs >= 3 && period * runs >= need {
            return true;
        }
    }
    false
}

/// Thinking-only replies do not count as a visible message.
fn visible(blocks: &[Block]) -> bool {
    blocks.iter().any(|block| match block {
        Block::Text { text } => !text.trim().is_empty(),
        Block::Other(v) => v["type"] != "thinking" && !v.is_null(),
        _ => true,
    })
}

fn sleep_flags(mode: SleepPrevention) -> &'static [&'static str] {
    match mode {
        SleepPrevention::Off => &[],
        SleepPrevention::Idle => &["-i"],
        SleepPrevention::Display => &["-i", "-d"],
        SleepPrevention::System => &["-i", "-d", "-s"],
    }
}

/// Holds the macOS sleep assertion for one turn; dropping the turn releases it.
struct SleepHold(Option<std::process::Child>);

impl SleepHold {
    fn start(mode: SleepPrevention) -> Self {
        let flags = sleep_flags(mode);
        if !cfg!(target_os = "macos") || flags.is_empty() {
            return Self(None);
        }
        let pid = std::process::id().to_string();
        Self(
            std::process::Command::new("caffeinate")
                .args(flags)
                .args(["-w", &pid])
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .ok(),
        )
    }
}

impl Drop for SleepHold {
    fn drop(&mut self) {
        if let Some(mut child) = self.0.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// Marks a running turn; warming that only bridges tool-call gaps stops when the turn ends.
struct ActiveGuard(Arc<std::sync::atomic::AtomicBool>);

impl Drop for ActiveGuard {
    fn drop(&mut self) {
        self.0.store(false, std::sync::atomic::Ordering::Relaxed);
    }
}

/// Automatic compaction documents live next to the session they belong to.
fn save_handoff(id: &str, document: &str) -> Result<PathBuf> {
    let file = Session::path(id);
    let dir = file.parent().unwrap_or_else(|| std::path::Path::new("."));
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default();
    let path = dir.join(format!("handoff-{stamp}.md"));
    std::fs::write(&path, document)?;
    Ok(path)
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
        crate::tools::configure(&cfg);
        crate::tools::set_stream_guard(&meta.cwd, &cfg);
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
        let system = context::system_prompt(&cfg, &meta.model);
        let lua = Arc::new(Lua::new(&meta.cwd, &cfg.integration_opts));
        // `mcp.shared`: one process per server across this process's agents and eval kernels.
        let mcp = if cfg.integration_opts.mcp.shared {
            tools::mcp::shared(&meta.cwd, cfg.mcp_project_config, &cfg.integration_opts.mcp)
        } else {
            Arc::new(Mcp::load_with_project(&meta.cwd, cfg.mcp_project_config, &cfg.integration_opts.mcp))
        };
        let ttsr = Arc::new({
            let ttsr = &cfg.ttsr_opts.ttsr;
            let judge = match ttsr.judge {
                crate::config::ttsr::Judge::Off => None,
                crate::config::ttsr::Judge::Auto => cfg.model_roles.get("judge").cloned(),
                crate::config::ttsr::Judge::On => {
                    Some(cfg.model_roles.get("judge").cloned().unwrap_or_else(|| meta.model.clone()))
                }
            };
            let rules = if cfg.rules_enabled && ttsr.enabled {
                crate::rules::load(&meta.cwd, ttsr)
            } else {
                Vec::new()
            };
            crate::rules::Ttsr::new(ttsr.clone(), rules, judge)
        });
        let memory = crate::memory::backend(&cfg, &meta.cwd);
        let assist = {
            let mut rt = crate::assist::Assist::new();
            rt.set_provider(provider.clone());
            rt
        };
        Agent {
            cfg,
            provider: provider.clone(),
            model,
            system,
            prompt_context,
            latest_context,
            messages,
            session: Session::open(&meta.id),
            snap_source: Session::snap_source(&meta.id),
            lua,
            mcp,
            cwd: meta.cwd.clone(),
            meta: meta.clone(),
            perms,
            subagent: false,
            review: None,
            agent: None,
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
            resident: Default::default(),
            pending: Default::default(),
            last_tool_call: None,
            tool_call_repeats: 0,
            depth: 0,
            ttsr,
            spent: Mutex::new(0.),
            cost_warned: Default::default(),
            live: Default::default(),
            fallback: Default::default(),
            warming: Default::default(),
            warmer: Default::default(),
            checkpoint: Default::default(),
            reset: Default::default(),
            idle_done: false,
            speculative: None,
            memory,
            memory_turns: 0,
            memory_recalled: false,
            todo_reminders: 0,
            todo_mutations: 0,
            todo_nudges: 0,
            todo_nudge_gap: 12,
            assist,
        }
    }

    fn tools(&self) -> Vec<ToolDef> {
        if self.cfg.no_tools || matches!(self.review, Some(Review::Remote)) { return Vec::new(); }
        let mut t = tools::defs_for(&self.cfg, !self.can_task(), &self.mcp.names());
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
            let mut off = self.cfg.ext_off.clone();
            off.extend(self.cfg.extra_tools_opts.off());
            match tools::ext::def(&self.cfg, &off) {
                Some(ext) => t[i] = ext,
                None => {
                    t.remove(i);
                }
            }
        }
        if self.cfg.provider_opts.external_thinking {
            t.push(tools::think_def());
        }
        // `goal.enabled`: the hidden `goal` tool exists only while goal mode is on, and only for the main agent.
        if self.cfg.modes.goal.enabled && self.agent.is_none() {
            t.push(crate::goal::tool_def());
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

    /// `tool_commands`: the commands this turn mounts, for `tools.commandDocs`.
    fn mounted_commands(&self) -> Vec<crate::discovery::Mounted> {
        let mut dynamic: Vec<(String, String)> = Vec::new();
        for server in self.mcp.names() {
            dynamic.push((format!("{server}__tool"), format!("{server}__tool {{...}}: one MCP tool; `--help` shows its schema.")));
        }
        if self.allowed.is_none() {
            for tool in self.lua.tools() {
                dynamic.push((tool.name.clone(), crate::discovery::help_text(&tool)));
            }
        }
        let dynamic: Vec<(&str, String)> = dynamic.iter().map(|(name, help)| (name.as_str(), help.clone())).collect();
        let ext = tools::ext::def(&self.cfg, &self.cfg.ext_off);
        let ext: Vec<ToolDef> = ext.into_iter().collect();
        crate::discovery::catalog(&ext, &["ext"], &dynamic)
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
                    // While a steered response streams, the message rides the provider connection.
                    if self.cfg.provider_opts.openai_live_steering && self.provider.live_steering(&self.model) {
                        self.live.push(text.to_string());
                    } else {
                        self.pending.lock().unwrap().push(text.to_string());
                    }
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

    /// Waits for one steering message, handling other control messages on the way.
    async fn pump_steer(&self) {
        loop {
            let v = std::future::poll_fn(|cx| self.steer_rx.lock().unwrap().poll_recv(cx)).await;
            let Some(v) = v else {
                return std::future::pending().await;
            };
            let steer = v["type"] == "steer"
                && v["message"].as_str().is_some_and(|text| !text.is_empty());
            self.handle_control(v);
            if steer {
                return;
            }
        }
    }

    fn steered(&mut self) -> bool {
        self.drain_control();
        let mut queued = std::mem::take(&mut *self.pending.lock().unwrap());
        let batch = steering_batch(&mut queued, self.cfg.loop_opts.steering_mode);
        if !queued.is_empty() {
            *self.pending.lock().unwrap() = queued;
        }
        let pushed = !batch.is_empty();
        for text in batch {
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
            ("command", skills::commands(&self.cwd, &self.cfg)),
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
            match crate::lua::hook_event(&n) {
                // `statusLine.showHookStatus`: a hook ran; the TUI shows it under the status line.
                Some(ev) => { let _ = tx.send(Event::Remote { command: "hook".into(), data: json!({ "event": ev }) }); }
                None => { let _ = tx.send(Event::Notify(n)); }
            }
        }
    }

    /// Switches only after the target provider has accepted the configuration.
    fn switch_model(&mut self, spec: &str, tx: &EventTx) -> Result<()> {
        let (name, model) = spec.split_once('/').filter(|(name, model)| !name.is_empty() && !model.is_empty())
            .ok_or_else(|| anyhow::anyhow!("model must be `provider/model`, got `{spec}`"))?;
        // An injected provider can serve other models under its own name, not a different provider.
        let injected = !self.cfg.providers.contains_key(name)
            && self.meta.model.split_once('/').is_some_and(|(current, _)| current == name);
        let (provider, model) = if injected {
            (self.provider.clone(), model.to_owned())
        } else {
            provider::make(&self.cfg, spec)?
        };
        let mut meta = self.meta.clone();
        meta.model = spec.to_owned();
        if let Some(session) = &self.session {
            session.append(&json!({"meta": meta}))?;
        }
        self.provider = provider;
        self.model = model;
        self.meta = meta;
        let (_, data) = crate::assist::switched_event(spec);
        let _ = tx.send(Event::Remote { command: "model".into(), data });
        Ok(())
    }

    /// One primary update landed: hand prewalk off at the first real edit/write, then start a due review.
    fn assist_update(&mut self, wrote: bool, tx: &EventTx) -> Result<()> {
        if let Some(spec) = crate::assist::prewalk_take(&mut self.assist, &self.messages, wrote) {
            self.switch_model(&spec, tx)?;
            let (note, _) = crate::assist::switched_event(&spec);
            let _ = tx.send(Event::Notify(note.clone()));
            self.push(Message::user(note));
            self.save_context()?;
        }
        crate::assist::review_now(&mut self.assist, &self.cfg, &self.messages, tx);
        Ok(())
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

    /// Which system text `snapcompact.system_prompt` renders as frames, with the frame edge that
    /// model accepts.
    fn frame_text(&self) -> Option<(crate::config::compaction::SnapSystemPrompt, u32)> {
        if self.cfg.compaction_opts.snapcompact.system_prompt == crate::config::compaction::SnapSystemPrompt::None {
            return None;
        }
        Some((self.cfg.compaction_opts.snapcompact.system_prompt, self.snap_edge()?))
    }

    /// Whether the model is known to edit exactly from images, so read results stay text.
    fn edits_from_images(&self) -> bool {
        let edits = |name: &str| self.allowed.as_ref().is_none_or(|a| a.iter().any(|t| t == name))
            && (self.cfg.tools.is_empty() || self.cfg.tools.iter().any(|t| t == name));
        edits("edit") || edits("write")
    }

    /// The owned dialect in force, when `tools_format` asks for one.
    fn dialect(&self) -> Option<&'static str> {
        self.cfg.context_opts.tools_format.dialect()
    }

    /// `features.unexpectedStopDetection: smart`: asks the `judge` role (else the session model) whether a
    /// text-only final reply stopped short of `request`. Errors and timeouts count as a normal end.
    async fn stopped_early(&self, request: &str, blocks: &[Block]) -> bool {
        let text: String = blocks.iter().filter_map(|b| match b {
            Block::Text { text } => Some(text.as_str()),
            _ => None,
        }).collect::<Vec<_>>().join("\n");
        // The stop is said at the end; a long answer keeps only its tail.
        let tail: String = text.chars().skip(text.chars().count().saturating_sub(4000)).collect();
        let prompt = format!("Request:\n{request}\n\nFinal message:\n{tail}");
        let spec = crate::assist::judge_spec(&self.cfg).unwrap_or_else(|| self.meta.model.clone());
        // Room for a reasoning model to think before its one word; Anthropic drops thinking below 1024.
        let judged = crate::textgen::generate(&self.cfg, &spec, STOP_JUDGE, &prompt, 1024);
        match tokio::time::timeout(std::time::Duration::from_secs(15), judged).await {
            Ok(Ok(answer)) => answer.trim().to_ascii_lowercase().starts_with("yes"),
            _ => false,
        }
    }

    fn can_preview_old_reads(&self) -> bool {
        // Rewriting old reads would move the cached prefix (`provider.appendOnlyContext`).
        self.cfg.old_read_previews && !self.provider.append_only() && !self.cfg.no_tools
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
        let (tools, descriptors) = if summarize {
            (Vec::new(), None)
        } else {
            let mut tools = self.tools();
            let inline = self.cfg.provider_opts.inline_tool_descriptors.enabled(
                self.meta.model.split_once('/').map_or("", |(p, _)| p),
                &self.model,
            );
            if inline {
                // Render first: the prompt copy is the only one that keeps the text.
                let text = tools::descriptors(&tools);
                tools::strip_descriptions(&mut tools);
                (tools, Some(text))
            } else {
                (tools, None)
            }
        };
        // Workspace snapshots must not become part of the generated summary.
        let mut all = Vec::with_capacity(messages.len() + 1);
        if !summarize && !self.prompt_context.is_empty() {
            all.push(Message::user(self.prompt_context.clone()));
        }
        all.extend(context::model_messages(messages, &self.cfg.file_opts)?);
        let start = usize::from(!summarize && !self.prompt_context.is_empty());
        for notice in crate::image::prepare(&self.cfg, &self.meta.model, &mut all[start..]).await {
            let _ = tx.send(Event::Notify(notice));
        }
        // `tool_commands`: mounted commands appear in the prompt per `tools.commandDocs`.
        let mount_docs = (!summarize && crate::discovery::mounts(&self.cfg))
            .then(|| crate::discovery::docs_section(&self.cfg, &self.mounted_commands()))
            .unwrap_or_default();
        let system_text = format!("{}{mount_docs}", self.system);
        let recovery_prompt = (!summarize && self.can_preview_old_reads()
            && context::preview_old_reads(&mut all[start..]))
            .then(|| format!("{}{READ_PREVIEW_GUIDE}", self.system));
        // An owned tool-call dialect (`tools_format`) replaces provider-native tools: the catalog and
        // syntax go into the prompt, and the reply's textual calls are parsed back below.
        let dialect = (!summarize && !tools.is_empty()).then(|| self.dialect()).flatten();
        // `inlineToolDescriptors`: descriptor text ships once, in the prompt, not in the schemas.
        let inline_body = descriptors.map(|list| {
            format!(
                "{}\n# Tool descriptors\nThe provider tool schemas carry no descriptions; read them here.{list}",
                recovery_prompt.as_deref().unwrap_or(&system_text)
            )
        });
        let dialect_body = dialect.map(|name| crate::dialect::instructions(name, &tools));
        let mut system = dialect_body
            .as_deref()
            .or(inline_body.as_deref())
            .unwrap_or_else(|| recovery_prompt.as_deref().unwrap_or(&system_text))
            .to_string();
        // `snapcompact.systemPrompt`: selected system text ships as frames attached to the first user
        // message instead of as text.
        if let Some((mode, edge)) = self.frame_text() {
            let shape = crate::snap::Shape::parse(&self.cfg.compaction_opts.snapcompact.shape)
                .unwrap_or(crate::snap::Shape { cell_w: 8, cell_h: 10 });
            let (kept, images) = crate::snap::frame_sections(&system, mode, edge, shape);
            if !images.is_empty() {
                system = kept;
                if let Some(first) = all.first_mut() {
                    first.content.extend(images.iter().map(|data| Block::Other(json!({"type":"image", "source":{"type":"base64", "media_type":"image/png", "data":data}}))));
                }
            }
        }
        // The dialect model reads the catalog from the prompt, so the wire carries no tool schemas.
        let tools: &[ToolDef] = if dialect.is_some() { &[] } else { &tools };
        // `snapcompact.toolResults`: accumulated large tool output ships as frames, newest included.
        if self.cfg.compaction_opts.snapcompact.tool_results {
            if let Some(edge) = self.snap_edge() {
                let shape = crate::snap::Shape::parse(&self.cfg.compaction_opts.snapcompact.shape).unwrap_or(crate::snap::Shape { cell_w: 8, cell_h: 10 });
                if let Some(notice) = crate::snap::image_tool_results(&mut all[start..], edge, shape, self.edits_from_images()) {
                    let _ = tx.send(Event::Notify(notice));
                }
            }
        }
        // `secrets.enabled`: configured secrets and credential-shaped tokens never leave for the provider.
        let obf = self.cfg.services_extra_opts.secrets.enabled.then(|| crate::secrets::Obfuscator::from_config(&self.cfg));
        let system = match &obf {
            Some(obf) => std::borrow::Cow::Owned(obf.obfuscate(&system)),
            None => std::borrow::Cow::Borrowed(system.as_str()),
        };
        if let Some(obf) = &obf {
            obf.obfuscate_messages(&mut all);
        }
        self.dump_request(&system, &tools, &all);
        let spent = *self.spent.lock().unwrap();
        if let Some(reason) = self.cfg.provider_opts.cost.refusal(spent) {
            bail!("{reason}");
        }
        // A live-capable transport carries typed-ahead messages into the running response.
        let live = (self.cfg.provider_opts.openai_live_steering
            && !summarize
            && self.provider.live_steering(&self.model))
        .then_some(&self.live);
        // In-band tool calls can end with the model writing the result itself: stop at the marker, or (when
        // `abort_on_fabricated_result` is off) let it finish and cut the invented continuation.
        let fabricated = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stop = std::sync::Arc::new(tokio::sync::Notify::new());
        let abort = !summarize && self.cfg.loop_opts.abort_on_fabricated_result;
        // `model.loopGuard.toolCallReminder`: cut a Gemini stream that only writes planning headers.
        let plan_guard = !summarize
            && self.cfg.loop_opts.loop_guard.enabled
            && self.cfg.loop_opts.loop_guard.tool_call_reminder
            && self.model.to_ascii_lowercase().contains("gemini");
        let plan = std::sync::Arc::new(tokio::sync::Notify::new());
        let headers = std::sync::Arc::new(std::sync::Mutex::new(PlanHeaderCount::default()));
        let recent = std::sync::Mutex::new(String::new());
        let stream = obf.as_ref().map(|_| std::sync::Mutex::new(crate::secrets::StreamDeobfuscator::default()));
        let on_text = |t: &str| {
            let text = match (&obf, &stream) {
                (Some(obf), Some(stream)) => stream.lock().unwrap().push(t, obf),
                _ => t.to_string(),
            };
            let _ = tx.send(Event::Text(text));
            if plan_guard && headers.lock().unwrap().feed(t) {
                plan.notify_one();
            }
            if abort && !fabricated.load(std::sync::atomic::Ordering::Relaxed) {
                let mut window = recent.lock().unwrap();
                window.push_str(t);
                if tools::fabricated::marker_at(&window).is_some() {
                    fabricated.store(true, std::sync::atomic::Ordering::Relaxed);
                    stop.notify_one();
                }
                // Only the last few chunks can start a marker that spans them.
                let cut = window.len().saturating_sub(64);
                if cut > 0 {
                    if let Some((at, _)) = window.char_indices().nth(cut) {
                        window.drain(..at);
                    }
                }
            }
        };
        // TTSR: a rule matching the stream cancels the request, so partial output never reaches the user.
        let ttsr = (!summarize && self.ttsr.interrupting()).then_some(&*self.ttsr);
        let hit = std::sync::atomic::AtomicBool::new(false);
        let on_text = |t: &str| {
            on_text(t);
            if let Some(ttsr) = ttsr {
                ttsr.push_delta(t);
            }
        };
        if let Some(ttsr) = ttsr {
            ttsr.reset_stream();
            ttsr.try_clear_hit();
        }
        let started = crate::telemetry::millis();
        let mut used: Option<String> = None;
        let mut reply = {
            // `retry.fallbackChains`: a provider error continues on a configured fallback model.
            let template = provider::fallback::Template {
                system: &system,
                messages: &all,
                tools: &tools,
                max_tokens: self.cfg.max_tokens,
                live,
            };
            let roles = provider::fallback::roles(&self.cfg, &self.meta.model, self.agent.as_deref(), self.subagent);
            let build = |spec: &str, effort: Option<&str>| provider::make_with_effort(&self.cfg, spec, effort);
            let attempt = || async {
                let mut state = self.fallback.lock().await;
                provider::fallback::request(
                    &self.cfg,
                    &self.provider,
                    &self.model,
                    &self.meta.model,
                    &roles,
                    self.subagent,
                    &mut state,
                    &template,
                    &on_text,
                    &build,
                    tx,
                )
                .await
            };
            // A spent saved reset unblocks the turn: retry the request once (`codexResets` / `claudeResets`).
            let fut = crate::resets::with_redeem(&self.cfg, &self.meta.model, None, attempt);
            tokio::pin!(fut);
            let interrupted = async {
                match ttsr {
                    Some(ttsr) => ttsr.wait_hit().await,
                    None => std::future::pending().await,
                }
            };
            let planning = async {
                if plan_guard { plan.notified().await } else { std::future::pending().await }
            };
            tokio::select! {
                biased;
                _ = interrupted => {
                    hit.store(true, std::sync::atomic::Ordering::Relaxed);
                    let ttsr = ttsr.expect("only an interrupting stream wakes this branch");
                    let names = ttsr.take_hit_names();
                    let partial = match self.cfg.ttsr_opts.ttsr.context_mode {
                        crate::config::ttsr::ContextMode::Keep => ttsr.partial(),
                        crate::config::ttsr::ContextMode::Discard => {
                            ttsr.reset_stream();
                            String::new()
                        }
                    };
                    for warning in ttsr.warnings(&names) {
                        self.pending.lock().unwrap().push(warning);
                    }
                    for name in &names {
                        let _ = tx.send(Event::Notify(format!("ルール適用: {name}")));
                    }
                    provider::Reply {
                        blocks: (!partial.trim().is_empty()).then_some(Block::Text { text: partial }).into_iter().collect(),
                        usage: Default::default(),
                        stop: "ttsr_interrupt".into(),
                    }
                }
                _ = stop.notified() => {
                    bail!(
                        "stopped the model: it started writing a fabricated tool result mid-turn \
                         (tools.abort_on_fabricated_result; turn it off to discard the continuation instead)"
                    );
                }
                _ = planning => {
                    // Planning only: the turn continues with a reminder instead of this reply.
                    provider::Reply { blocks: Vec::new(), usage: Default::default(), stop: "plan_headers".into() }
                }
                outcome = &mut fut => {
                    let outcome = outcome?;
                    used = Some(outcome.spec);
                    outcome.reply
                }
            }
        };
        if let (Some(obf), Some(stream)) = (&obf, &stream) {
            let tail = stream.lock().unwrap().finish(obf);
            if !tail.is_empty() {
                let _ = tx.send(Event::Text(tail));
            }
            obf.deobfuscate_reply(&mut reply);
        }
        if !hit.load(std::sync::atomic::Ordering::Relaxed) {
            self.ttsr.reset_stream();
        }
        // `discard` dropped the streamed text before it could be committed; `keep` committed it above.
        if reply.stop == "ttsr_interrupt" {
            self.ttsr.reset_stream();
        }
        if !summarize && tools::fabricated::strip_blocks(&mut reply.blocks) {
            let _ = tx.send(Event::Notify(
                "discarded a fabricated tool result the model wrote inline".into(),
            ));
        }
        // An owned dialect spells its calls in the reply text; turn them into native tool calls so the
        // rest of the loop is unchanged.
        if let Some(name) = dialect {
            let calls = crate::dialect::parse(
                name,
                &reply.blocks.iter().filter_map(|b| match b {
                    Block::Text { text } => Some(text.as_str()),
                    _ => None,
                }).collect::<String>(),
            );
            if !calls.is_empty() {
                reply.blocks.retain(|b| !matches!(b, Block::Text { .. }) || b_text_is_call_only(b));
                for (i, (tool, args)) in calls.into_iter().enumerate() {
                    reply.blocks.push(Block::ToolUse { id: format!("dialect-{}-{i}", &reply.stop), name: tool, input: args });
                }
                reply.stop = "tool_use".into();
            }
        }
        // A message the transport could not carry waits for the next tool boundary instead.
        for text in self.live.take() {
            self.pending.lock().unwrap().push(text);
        }
        let used = used.unwrap_or_else(|| self.meta.model.clone());
        if !summarize && used == self.meta.model {
            self.refresh_warming(&system, &all, &tools, &reply);
        }
        let (provider, model) = used.split_once('/').unwrap_or(("greencode", self.model.as_str()));
        crate::telemetry::request(&self.cfg, &used, started, &reply.usage);
        crate::usage_history::record(provider, model, &reply.usage);
        let price = self.cfg.cost(&used, &reply.usage).unwrap_or(0.);
        let spent = {
            let mut spent = self.spent.lock().unwrap();
            *spent += price;
            *spent
        };
        let warned = self.cost_warned.load(std::sync::atomic::Ordering::Relaxed);
        if let Some(warning) = self.cfg.provider_opts.cost.warning(spent, warned) {
            self.cost_warned.store(true, std::sync::atomic::Ordering::Relaxed);
            let _ = tx.send(Event::Notify(warning));
        }
        // An open goal counts the thread's own usage against its token budget.
        if !summarize && !self.subagent && self.cfg.modes.goal.enabled {
            crate::goal::count_tokens(&Some(self.meta.id.clone()), reply.usage.input + reply.usage.output);
        }
        let _ = tx.send(if summarize || self.subagent {
            Event::UsageAux(reply.usage)
        } else {
            Event::Usage(reply.usage)
        });
        Ok(reply)
    }

    /// Starts the warmer for this request: it refreshes the entry shortly before it expires, and
    /// flips `warming.bridge` when compacting beats refreshing.
    fn refresh_warming(&self, system: &str, messages: &[Message], tools: &[ToolDef], reply: &provider::Reply) {
        let (pname, model) = self
            .meta
            .model
            .split_once('/')
            .map_or(("greencode", self.model.as_str()), |(p, m)| (p, m));
        let plan = provider::warm::Plan::resolve(&self.cfg, pname, model, self.provider_oauth());
        if let Some(handle) = self.warmer.lock().unwrap().take() {
            handle.abort();
        }
        if !plan.enabled {
            return;
        }
        let snap = provider::warm::Snapshot {
            model: model.to_string(),
            system: system.to_string(),
            messages: messages.to_vec(),
            tools: tools.to_vec(),
            max_tokens: self.cfg.max_tokens,
            prompt_tokens: reply.usage.context(),
            price: self.cfg.prices.get(&self.meta.model).map(|p| {
                // One cached read of the whole prompt: the cheapest possible refresh.
                p.get(2).or(p.first()).copied().unwrap_or(0.) * reply.usage.context() as f64 / 1e6
            }),
        };
        let generation = self.warming.generation.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
        *self.warmer.lock().unwrap() = provider::warm::spawn(
            self.provider.clone(),
            snap,
            plan,
            self.warming.clone(),
            generation,
        );
    }

    /// Whether this agent's provider signs in with a subscription (a one-hour cache tier).
    fn provider_oauth(&self) -> bool {
        self.meta
            .model
            .split_once('/')
            .and_then(|(pname, _)| self.cfg.providers.get(pname).map(|p| provider::oauth(pname, p).is_some()))
            .unwrap_or(false)
    }

    /// `auto` marks a maintenance-triggered compaction, which may save its document to disk.
    async fn compact(&mut self, tx: &EventTx, method: Method, auto: bool) -> Result<()> {
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
        let split = crate::snap::split_point(&history, self.cfg.compaction_opts.maintenance.keep_chars());
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
            let shape = crate::snap::Shape::parse(&self.cfg.compaction_opts.snapcompact.shape)?;
            let archive = crate::snap::archive(&source, edge, keep_latest_prompt, shape);
            if archive.appendix_len > 12_000 {
                let _ = tx.send(Event::Notify(
                    "History is mostly text the frames cannot show; summarizing instead".into(),
                ));
                return Box::pin(self.compact(tx, Method::Summary, auto)).await;
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
        if split == 0 && self.context <= self.compact_at()? && history.len() == self.messages.len()
            && self.can_preview_old_reads() && context::preview_old_reads(&mut history)
        {
            let _ = tx.send(Event::Notify("Keeping exact history; older reads are already previewed for the model.".into()));
            return Ok(());
        }
        let _ = tx.send(Event::Notify("Compacting conversation…".into()));
        let mut messages = history;
        let tail = if split == 0 {
            Vec::new()
        } else {
            messages.split_off(split)
        };
        // `compaction.remoteStreamingV2Enabled`: the provider compacts server-side and hands back an
        // opaque retained item instead of a written summary.
        if self.cfg.compaction_opts.maintenance.remote_streaming_v2 && messages.is_empty()
            && crate::provider::compact::supported(&self.cfg, &self.meta.model)
        {
            let checkpoint = crate::provider::compact::compact(&self.cfg, &self.meta.model, &self.system, &tail).await?;
            self.push(checkpoint);
            self.snap_source = None;
            self.rebase_context();
            self.context = 0;
            *self.checkpoint.get_mut().unwrap() = None;
            self.save_context()?;
            return Ok(());
        }
        // Summarize only the old span and replay the recent tail verbatim.
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
        self.apply_summary(tx, summary.join("\n"), tail, auto)
    }

    /// Replaces the transcript with `document` plus the verbatim `tail`; shared by every summary path.
    fn apply_summary(&mut self, tx: &EventTx, document: String, tail: Vec<Message>, auto: bool) -> Result<()> {
        if auto && !self.subagent && self.cfg.compaction_opts.maintenance.handoff_save_to_disk {
            match save_handoff(&self.meta.id, &document) {
                Ok(path) => { let _ = tx.send(Event::Notify(format!("Saved handoff document to {}", path.display()))); }
                Err(e) => { let _ = tx.send(Event::Notify(format!("Cannot save the handoff document: {e:#}"))); }
            }
        }
        // `compaction.experimentalContextManagement`: the summary also lands in durable notes, and the
        // raw history stays searchable on disk.
        if !self.subagent && self.cfg.compaction_opts.maintenance.experimental_context_management {
            match crate::notebook::record(&self.meta.id, &document, &self.messages) {
                Ok(path) => { let _ = tx.send(Event::Notify(format!("Notes and raw history kept in {}", path.display()))); }
                Err(e) => { let _ = tx.send(Event::Notify(format!("Cannot keep notes: {e:#}"))); }
            }
        }
        let summary = format!(
            "This is an automatically generated checkpoint of the conversation so far, not a message from the user. \
Continue the work from this summary and the messages that follow it.\n\n<compacted-summary>\n{document}\n</compacted-summary>"
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

    /// Starts a background summary of the old span, covering the same split the foreground path uses.
    fn start_speculative(&mut self) {
        let split = crate::snap::split_point(&self.messages, self.cfg.compaction_opts.maintenance.keep_chars());
        if split == 0 {
            return;
        }
        let mut messages = self.messages[..split].to_vec();
        messages.push(Message::user(COMPACT_PROMPT));
        let provider = self.provider.clone();
        let model = self.model.clone();
        let system = self.system.clone();
        let tools = self.tools();
        let max_tokens = self.cfg.max_tokens;
        let handle = tokio::spawn(async move {
            let req = Request { model: &model, system: &system, messages: &messages, tools: &tools, max_tokens, live: None };
            let reply = provider.complete(req, &|_: &str| {}).await?;
            let document = reply
                .blocks
                .iter()
                .filter_map(|b| match b {
                    Block::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n");
            if document.trim().is_empty() {
                anyhow::bail!("the background summary came back empty");
            }
            Ok(document)
        });
        self.speculative = Some(Speculative { handle, split });
    }

    fn rebase_context(&mut self) {
        self.latest_context = context::session_context(&self.cwd, &self.cfg);
        self.prompt_context = context::wrap_context(&self.latest_context);
    }

    fn snap_edge(&self) -> Option<u32> {
        let (name, _) = self.meta.model.split_once('/')?;
        crate::snap::frame_edge(self.cfg.providers.get(name)?, &self.model)
    }

    /// `contextPromotion.enabled`: move an overflowing thread to the wider-context model instead of
    /// compacting, keeping the transcript.
    async fn promote(&mut self, tx: &EventTx) -> Result<bool> {
        let Some(target) = self.cfg.context_opts.context_promotion.target_for(&self.meta.model) else {
            return Ok(false);
        };
        let (provider, model) = provider::make(&self.cfg, target)?;
        if let Some(session) = &self.session {
            let mut meta = Session::load(&self.meta.id)?.0;
            meta.model = target.to_string();
            session.append(&json!({"meta": meta}))?;
        }
        self.meta.model = target.to_string();
        self.model = model;
        self.provider = provider;
        // The promoted model's window is larger, so the next threshold must be recomputed from scratch.
        self.context = 0;
        self.save_context()?;
        let _ = tx.send(Event::Notify(format!(
            "Context overflow: promoted to {target} instead of compacting (contextPromotion)"
        )));
        Ok(true)
    }

    async fn compact_auto(&mut self, tx: &EventTx, auto: bool) -> Result<()> {
        if self.promote(tx).await? {
            return Ok(());
        }
        // `compaction.asyncEnabled`: a summary already prepared in the background is used as is.
        if let Some(spec) = self.speculative.take() {
            match spec.handle.await {
                Ok(Ok(document)) => {
                    let split = spec.split.min(self.messages.len());
                    let tail = self.messages.split_off(split);
                    return self.apply_summary(tx, document, tail, auto);
                }
                Ok(Err(e)) => {
                    let _ = tx.send(Event::Notify(format!("Background summary failed: {e:#}")));
                }
                Err(e) => {
                    let _ = tx.send(Event::Notify(format!("Background summary stopped: {e}")));
                }
            }
        }
        let order = &self.cfg.compaction_opts.maintenance.method_order;
        let method = crate::config::compaction::method(order, &self.cfg.compaction, self.snap_edge().is_some())?;
        self.compact(tx, method, auto).await
    }

    fn compact_at(&self) -> Result<u64> {
        let cfg = &self.cfg;
        if let Some(threshold) = self.agent.as_deref().and_then(|name| cfg.task.compaction_threshold(name)) {
            return Ok(threshold);
        }
        let model = &self.meta.model;
        let limit = cfg.model_compact_at_enabled.then(|| {
            cfg.model_compact_at.get(model).copied().filter(|&n| n != 0).or_else(|| {
                cfg.model_compact_at.iter().filter(|&(_, &n)| n != 0)
                    .filter_map(|(key, &n)| key.strip_suffix('*').filter(|prefix| model.starts_with(prefix)).map(|prefix| (prefix.len(), n)))
                    .max_by_key(|&(len, _)| len).map(|(_, n)| n)
            })
        }).flatten();
        cfg.compaction_opts.maintenance.threshold(limit, cfg.compact_at)
    }

    pub async fn run_turn(&mut self, mut prompt: String, tx: &EventTx) -> Result<()> {
        // An `eval` cell's `agent()`/`workpool()` runs subagents as children of this turn.
        tools::set_agent_context(&self.meta, self.perms.mode(), tx);
        self.warming.active.store(true, std::sync::atomic::Ordering::Relaxed);
        let _active = ActiveGuard(self.warming.active.clone());
        self.ttsr.begin_prompt();
        let mut tool_calls = 0u32;
        let _sleep = SleepHold::start(self.cfg.loop_opts.power.sleep_prevention);
        // Browser tabs this project owns freeze once the turn settles.
        let _browser = tools::browser::Settle::new(self.cwd.clone());
        let mut loop_retries = 0u8;
        let mut stop_retries = 0u8;
        let mut cut_retries = 0u8;
        // `smart`: the reply to a stop nudge is the final answer, so it is not judged again.
        let mut stop_nudged = false;
        let request: String = prompt.chars().take(2000).collect();
        self.repair();
        self.todo_reminders = 0;
        self.todo_mutations = 0;
        self.todo_nudges = 0;
        self.todo_nudge_gap = 12;
        // An append-only prefix must not move either: old screenshots stay as they were sent.
        if !self.provider.append_only() {
            context::archive_computer_images(&mut self.messages)?;
        }
        // Never replace a newly submitted requirement with a lossy summary.
        let compact_at = if self.cfg.compaction_enabled { self.compact_at()? } else { 0 };
        if self.cfg.compaction_enabled && self.context > compact_at {
            self.compact_auto(tx, true).await?;
        }
        // A steer left over from a cancelled turn keeps its place before the new prompt.
        self.steered();
        // Automatic recall runs once per session; the backend's setting decides whether it is on.
        if !self.memory_recalled {
            if let Some(memory) = self.memory.clone() {
                self.memory_recalled = true;
                let query = crate::memory::recall_query(&prompt, &self.messages);
                match memory.recall(&query, crate::memory::RECALL_LIMIT).await {
                    Ok(hits) => {
                        let block = crate::memory::recall_block(&hits);
                        if !block.is_empty() {
                            prompt = format!("{block}\n\n{prompt}");
                        }
                    }
                    Err(e) => {
                        let _ = tx.send(Event::Notify(format!("Memory recall failed: {e:#}")));
                    }
                }
            }
        }
        // `defaultThinkingLevel = auto`: this turn's prompt picks the effort, clamped by its ceiling.
        if let Some(effort) = provider::thinking::resolve(&self.cfg, &prompt, &self.meta.model) {
            self.provider = provider::make_with_effort(&self.cfg, &self.meta.model, Some(&effort))?.0;
        }
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
        // Standalone magic keywords append a hidden notice ahead of the turn's other reminders.
        if let Some(Block::Text { text }) = message.content.first_mut() {
            let prompt = text.clone();
            crate::magic_keywords::apply(text, &prompt, self.cfg.interaction_opts.magic_keywords.enabled);
        }
        // `always` gets one delegation reminder on the first user message of a main-agent session.
        if self.cfg.task.eager == crate::subagents::Eager::Always
            && self.agent.is_none()
            && self.depth == 0
            && self.messages.is_empty()
        {
            if let Some(Block::Text { text }) = message.content.first_mut() {
                text.push_str("\n\n");
                text.push_str(crate::subagents::EAGER_REMINDER);
            }
        }
        if self.cfg.bash_opts.todo.eager != crate::config::bash::Eager::Default
            && self.agent.is_none()
            && self.messages.is_empty()
            && todo_tool_enabled(&self.cfg)
            && self.perms.mode() != permission::Mode::Plan
        {
            if let Some(Block::Text { text }) = message.content.first_mut() {
                let prompt = text.trim_end();
                if !prompt.ends_with(['?', '!']) {
                    // shortcut: no forced tool_choice; `always` only strengthens the wording.
                    text.push_str("\n\n");
                    text.push_str(if self.cfg.bash_opts.todo.eager == crate::config::bash::Eager::Always {
                        "<system-reminder>Start this turn with the `todo` tool: break the work into tasks before you act.</system-reminder>"
                    } else {
                        "<system-reminder>Consider the `todo` tool first: for multi-step work, list the tasks before you act.</system-reminder>"
                    });
                }
            }
        }
        // Passive advisor and prewalk start on the new prompt; judgement failures stay silent.
        crate::assist::open(
            &mut self.assist,
            &self.cfg,
            &mut message,
            self.agent.is_none() && self.depth == 0,
            tx,
        )
        .await;
        self.push(message);
        // Soft request budget: subagents only; the main agent owns the budget for its children.
        let mut limits = self.agent.as_deref().map(|name| {
            let budget = self.cfg.task.soft_budget(name);
            (budget, crate::subagents::SoftBudget::new(budget, self.cfg.task.soft_request_budget_notice))
        });
        let mut requests = 0u64;
        let mut force_requested = false;
        loop {
            // `mcp.notifications`: debounced resource updates join the conversation before the model call.
            for update in self.mcp.take_updates(self.cfg.integration_opts.mcp.notifications) {
                self.push(Message::user(format!("<system-reminder>{update}</system-reminder>")));
            }
            if let Some((limit, guard)) = limits.as_mut() {
                match guard.step(requests) {
                    crate::subagents::BudgetStep::Run => {}
                    crate::subagents::BudgetStep::Notice => {
                        self.push(Message::user(crate::subagents::budget_notice(requests, *limit)));
                    }
                    crate::subagents::BudgetStep::Force => {
                        self.push(Message::user(crate::subagents::budget_force(requests, *limit)));
                        force_requested = true;
                    }
                    crate::subagents::BudgetStep::Exceeded => {
                        bail!("soft request budget exceeded ({requests} requests; budget {limit})")
                    }
                }
            }
            // `compaction.asyncEnabled`: prepare the summary once the context nears the threshold, so
            // crossing it costs no model call.
            if self.cfg.compaction_opts.maintenance.async_enabled
                && self.speculative.is_none()
                && self.context * 5 >= compact_at * 4
            {
                self.start_speculative();
            }
            if self.cfg.compaction_enabled && (self.cfg.compaction_mid_turn || self.subagent)
                && self.context > compact_at
            {
                self.compact_auto(tx, true).await?;
            } else if self.cfg.compaction_enabled
                && self.warming.bridge.swap(false, std::sync::atomic::Ordering::Relaxed)
            {
                // The warmer found a long tool call where compacting is cheaper than the cache miss.
                self.compact_auto(tx, true).await?;
            }
            if let Some(note) = crate::assist::gate(&mut self.assist, &self.cfg, tx).await {
                self.push(Message::user(note));
                self.save_context()?;
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
            let mut reply = self
                .request(rewritten.as_deref().unwrap_or(&self.messages), false, tx)
                .await?;
            // `edit.recoverInlineEdits`: a payload written as plain text runs as a real edit call.
            crate::tools::edit::recover_inline(&self.cfg, &mut reply.blocks);
            requests += 1;
            self.notes(tx);
            self.context = reply.usage.context() + reply.usage.output;
            if reply.stop == "ttsr_interrupt" {
                // The warning is already queued; a `keep` partial reply stays as context for the retry.
                if let Some(Block::Text { text }) = reply.blocks.first() {
                    self.push(Message {
                        role: Role::Assistant,
                        content: vec![Block::Text { text: text.clone() }],
                    });
                }
                self.save_context()?;
                continue;
            }
            if self.cfg.loop_opts.loop_guard.enabled
                && reply_repeats(&reply.blocks, self.cfg.loop_opts.loop_guard.check_assistant_content)
            {
                if loop_retries >= 2 {
                    bail!("the model's reply kept repeating; stopped before committing it");
                }
                loop_retries += 1;
                let _ = tx.send(Event::Notify("Loop detected in the reply; retrying without it.".into()));
                self.push(Message::user(LOOP_REMINDER));
                self.save_context()?;
                continue;
            }
            // `model.loopGuard.toolCallReminder`: a Gemini reasoning stream that only writes
            // planning headers never reaches the user; replace it with a reminder to act.
            if self.cfg.loop_opts.loop_guard.enabled
                && self.cfg.loop_opts.loop_guard.tool_call_reminder
                && self.model.to_ascii_lowercase().contains("gemini")
                && reply.blocks.iter().all(|b| !matches!(b, Block::ToolUse { .. }))
                && (reply.stop == "plan_headers" || plan_header_runaway(&reply.blocks))
                && loop_retries < 2
            {
                loop_retries += 1;
                let _ = tx.send(Event::Notify("Only planning headers from Gemini; asking for a tool call.".into()));
                self.push(Message::user(TOOL_CALL_REMINDER));
                self.save_context()?;
                continue;
            }
            if !visible(&reply.blocks) {
                if self.cfg.loop_opts.features.unexpected_stop_detection
                    == UnexpectedStopDetection::None
                    || stop_retries >= 3
                {
                    bail!(
                        "the model returned an empty reply (stop reason: {})",
                        reply.stop
                    );
                }
                stop_retries += 1;
                let _ = tx.send(Event::Notify("No visible reply; asking the model to continue.".into()));
                self.push(Message::user(CONTINUE_REMINDER));
                self.save_context()?;
                continue;
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
            // Instructions steered into the response are part of the transcript the model answered.
            for text in self.live.take_sent() {
                self.push(Message::user(text));
            }
            // `extensionRenderers.assistantText`: the hook may redraw the reply; the session keeps it as written.
            if crate::discovery::renders_assistant(&self.cfg, self.lua.has_hook("render_assistant")) {
                let text: String = reply.blocks.iter().filter_map(|b| match b {
                    Block::Text { text } => Some(text.as_str()),
                    _ => None,
                }).collect::<Vec<_>>().join("");
                if !text.trim().is_empty() {
                    let reply_text = self.lua.hook("render_assistant", json!({"text": text})).await;
                    if let Some(shown) = reply_text["text"].as_str().filter(|shown| *shown != text) {
                        let _ = tx.send(Event::Remote { command: "render_assistant".into(), data: json!({"text": shown}) });
                    }
                }
            }
            let blocked = self.rule_matches(&reply.blocks, tx);
            self.judge_rules(&reply, tx).await;
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
            if force_requested && !uses.is_empty() {
                bail!(
                    "soft request budget exceeded ({requests} requests; budget {}): the sub-agent kept calling tools after the forced stop",
                    limits.as_ref().map_or(0, |(limit, _)| *limit)
                );
            }
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
            let guard = self.cfg.loop_opts.tool_call_loop_guard.clone();
            let mut loops = HashSet::new();
            for (id, name, input) in &uses {
                if !blocked.contains_key(*id) && call_repeats(&mut self.last_tool_call, &mut self.tool_call_repeats, name, input, &guard) {
                    loops.insert(id.as_str());
                }
            }
            if uses.is_empty() {
                let stops = self.cfg.loop_opts.features.unexpected_stop_detection;
                if reply.stop == "max_tokens" {
                    if stops != UnexpectedStopDetection::None && cut_retries < 3 {
                        cut_retries += 1;
                        let _ = tx.send(Event::Notify("Reply was cut off at max_tokens; continuing.".into()));
                        self.push(Message::user(CUT_OFF_REMINDER));
                        self.save_context()?;
                        continue;
                    }
                    let _ = tx.send(Event::Notify("Reply was cut off at max_tokens.".into()));
                }
                match crate::assist::settle_final(&mut self.assist, &self.cfg, &self.messages, tx).await {
                    crate::assist::Verdict::Wake(note) => {
                        self.push(Message::user(note));
                        self.save_context()?;
                        continue;
                    }
                    crate::assist::Verdict::Cards(cards) => {
                        for card in cards {
                            self.push(Message::user(card));
                        }
                        self.save_context()?;
                    }
                    crate::assist::Verdict::Quiet => {}
                }
                if self.deliver(true).await {
                    // The follow-up reply would otherwise run on from this one.
                    let _ = tx.send(Event::Text("\n\n".into()));
                    continue;
                }
                if self.agent.is_none()
                    && self.cfg.bash_opts.todo.reminders
                    && todo_tool_enabled(&self.cfg)
                    && self.perms.mode() != permission::Mode::Plan
                    && self.todo_reminders < self.cfg.bash_opts.todo.reminders_max
                {
                    let pending = incomplete_todos(&self.messages);
                    if !pending.is_empty() {
                        self.todo_reminders += 1;
                        self.push(Message::user(format!(
                            "<system-reminder>\nYou stopped with {} incomplete todo item(s):\n{}\n\nPlease continue working on these tasks or mark them complete if finished.\n(Reminder {}/{})\n</system-reminder>",
                            pending.len(),
                            pending.iter().map(|text| format!("- {text}")).collect::<Vec<_>>().join("\n"),
                            self.todo_reminders,
                            self.cfg.bash_opts.todo.reminders_max
                        )));
                        self.save_context()?;
                        continue;
                    }
                }
                if stops == UnexpectedStopDetection::Smart
                    && self.perms.mode() != permission::Mode::Plan
                    && !matches!(reply.stop.as_str(), "max_tokens" | "ttsr_interrupt")
                    && !std::mem::take(&mut stop_nudged)
                    && self.stopped_early(&request, &reply.blocks).await
                {
                    stop_nudged = true;
                    let _ = tx.send(Event::Notify("The reply stopped before the work was done; continuing.".into()));
                    self.push(Message::user(UNEXPECTED_STOP_REMINDER));
                    self.save_context()?;
                    continue;
                }
                // one-at-a-time leaves queued steers for a follow-up request.
                if self.steered() {
                    let _ = tx.send(Event::Text("\n\n".into()));
                    continue;
                }
                // Sub-agents report to their parent, and plan mode cannot save memory anyway.
                if self.cfg.learning.enabled
                    && !self.cfg.no_tools
                    && self.cfg.learning.auto
                    && tool_calls >= self.cfg.integration_opts.autolearn.min_tool_calls
                    && !self.subagent
                    && self.perms.mode() != permission::Mode::Plan
                {
                    if let Err(e) = self.auto_learn(tx).await {
                        let _ = tx.send(Event::Notify(format!("自動学習を保存できません: {e:#}")));
                    }
                }
                self.remember(tx);
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
                    let threshold = guard.threshold;
                    let agent = &*self;
                    // Speculative execution: validated local reads start before dispatch and their results
                    // replace re-reading the file; discarding one costs a single read.
                    let calls: Vec<(String, Value)> = uses
                        .iter()
                        .filter(|(id, ..)| !loops.contains(id.as_str()))
                        .map(|(id, .., input)| ((*id).clone(), (*input).clone()))
                        .collect();
                    let warmed = tools::speculate::prefetch(&calls, &self.cwd, &self.cfg).await;
                    let batch = join_all(uses.iter().map(|(id, name, input)| {
                        let skipped = loops.contains(id.as_str());
                        let warmed = warmed.get(id.as_str()).cloned();
                        let refused = blocked.get(*id).cloned();
                        async move {
                            let not_run = if skipped {
                                Some(format!(
                                    "Loop guard: this exact `{name}` call repeated {threshold} times in a row and was not run. Take a different step or answer the user."
                                ))
                            } else {
                                refused.map(|warning| format!("{warning}\nThis call was not run; adjust it or take another approach."))
                            };
                            if let Some(content) = not_run {
                                // Shown like a failed call, so the user sees what was stopped and why.
                                let _ = tx.send(Event::ToolStart { id: (*id).clone(), name: (*name).clone(), input: (*input).clone() });
                                let _ = tx.send(Event::ToolEnd { id: (*id).clone(), output: content.clone(), is_error: true });
                                return Block::ToolResult { tool_use_id: (*id).clone(), content, is_error: true };
                            }
                            agent.exec(id, name, (*input).clone(), preview, tx, warmed).await
                        }
                    }));
                    if self.cfg.loop_opts.interrupt_mode == InterruptMode::Immediate {
                        tokio::pin!(batch);
                        tokio::select! {
                            results = &mut batch => results,
                            _ = self.pump_steer() => {
                                // shortcut: dropping the batch aborts in-flight tools; a side effect already started can still land.
                                uses.iter().map(|(id, ..)| Block::ToolResult {
                                    tool_use_id: (*id).clone(),
                                    content: "not run: interrupted by a steering message".into(),
                                    is_error: true,
                                }).collect()
                            }
                        }
                    } else {
                        batch.await
                    }
                };
            // Screenshots follow the tool results as image blocks, which both wire formats accept in a user turn.
            let mut results: Vec<Block> = results;
            results.extend(self.images.lock().unwrap().drain(..).map(|block| {
                let _ = tx.send(Event::Image(block.clone()));
                Block::Other(block)
            }));
            // Mid-run todo nudges count successful shelf-changing calls since the last `todo` call.
            let touched_todo = uses.iter().any(|(_, name, _)| name.as_str() == "todo");
            if touched_todo {
                self.todo_mutations = 0;
                self.todo_nudges = 0;
                self.todo_nudge_gap = 12;
            } else {
                let mutated = results
                    .iter()
                    .filter(|block| match block {
                        Block::ToolResult { tool_use_id, is_error: false, .. } => uses.iter().any(|(id, name, _)| {
                            *id == tool_use_id && matches!(name.as_str(), "bash" | "eval" | "edit" | "write")
                        }),
                        _ => false,
                    })
                    .count() as u32;
                self.todo_mutations = self.todo_mutations.saturating_add(mutated);
                let mode = self.cfg.bash_opts.todo.mid_run_nudges;
                let capped = mode == crate::config::bash::MidRunNudges::Capped;
                let gap = if mode == crate::config::bash::MidRunNudges::Backoff { self.todo_nudge_gap } else { 12 };
                if mode != crate::config::bash::MidRunNudges::Off
                    && (!capped || self.todo_nudges < 2)
                    && self.todo_mutations >= gap
                    && self.agent.is_none()
                    && todo_tool_enabled(&self.cfg)
                    && self.perms.mode() != permission::Mode::Plan
                {
                    let pending = incomplete_todos(&self.messages);
                    if !pending.is_empty() {
                        self.todo_mutations = 0;
                        self.todo_nudges += 1;
                        if mode == crate::config::bash::MidRunNudges::Backoff {
                            self.todo_nudge_gap = (self.todo_nudge_gap * 2).min(192);
                        }
                        results.push(Block::Text {
                            text: format!(
                                "<system-reminder>\n{} todo item(s) are still incomplete; update the list with the `todo` tool before continuing.\n</system-reminder>",
                                pending.len()
                            ),
                        });
                    }
                }
            }
            if let Some(reset) = self.reset.get_mut().unwrap().take() {
                // The round's tool results are dropped with the messages they answer.
                self.apply_reset(reset)?;
                continue;
            }
            let changed_file = |id: &str| {
                uses.iter().any(|(uid, name, input)| {
                    let (name, input) = tools::ext::effective(name, input);
                    uid.as_str() == id && matches!(name.as_str(), "edit" | "write") && !input["path"].as_str().unwrap_or("").contains("://")
                })
            };
            let wrote = results.iter().any(|block| {
                matches!(block, Block::ToolResult { tool_use_id, is_error: false, .. } if changed_file(tool_use_id))
            });
            self.push(Message {
                role: Role::User,
                content: results,
            });
            self.assist_update(wrote, tx)?;
            // shortcut: the session journal keeps the original text, so a resumed thread shows it again until the next pass;
            // upgrade when a session rewrite exists.
            let maintenance = &self.cfg.compaction_opts.maintenance;
            context::prune_results(&mut self.messages, maintenance.supersede_reads, maintenance.drop_useless);
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
        warmed: Option<tools::speculate::Warm>,
    ) -> Block {
        // `tool_commands`: a mounted command line is the tool it mounts, before hooks and approval.
        let rewritten = (name == "bash")
            .then(|| input["command"].as_str())
            .flatten()
            .and_then(|command| self.mounted_call(command));
        let (name, input) = match &rewritten {
            Some((rewritten, input)) => (rewritten.as_str(), input.clone()),
            None => (name, input),
        };
        let mut input = input;
        let _ = tx.send(Event::ToolStart {
            id: id.into(),
            name: name.into(),
            input: input.clone(),
        });
        let started = crate::telemetry::millis();
        let result = self.exec_inner(name, &mut input, tx, warmed).await;
        self.notes(tx);
        let (output, is_error) = match result {
            Ok(s) => (s, false),
            Err(e) => (format!("{e:#}"), true),
        };
        crate::telemetry::span("tool", started, crate::telemetry::millis(), !is_error, &[("tool.name", name.to_string())]);
        crate::telemetry::count("tool.calls", 1);
        let limits = context::Limits::new(&self.cfg);
        let (output, _) = if !is_error && broad_output(name) {
            context::preview_with(output, id, preview, &limits)
        } else {
            context::truncate_with(output, id, &limits)
        };
        // `spotlight.experiment.enabled`: broad output is shaped, and each shaping is logged to the session.
        let (output, shaped) = crate::discovery::shape(&self.cfg, name, &output);
        if let (Some(log), Some(session)) = (shaped, &self.session) {
            let _ = session.append(&log);
        }
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

    async fn exec_inner(
        &self,
        name: &str,
        input: &mut Value,
        tx: &EventTx,
        warmed: Option<tools::speculate::Warm>,
    ) -> Result<String> {
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
        // `extensionHandlers.toolCallTimeoutMs`: a hook that never returns is abandoned, not waited on.
        let hook = match tokio::time::timeout(
            crate::discovery::hook_timeout(&self.cfg),
            self.lua.hook("tool_call", json!({"tool": name, "input": input, "mode": self.perms.mode()})),
        )
        .await
        {
            Ok(hook) => hook,
            Err(_) => return Err(anyhow::anyhow!("extension tool_call handler timed out after {} ms", crate::discovery::hook_timeout(&self.cfg).as_millis())),
        };
        if let Some(reason) = hook["deny"].as_str() {
            bail!("blocked by an extension: {reason}");
        }
        if hook["input"].is_object() {
            *input = hook["input"].clone();
        }
        // `extensionHandlers.modeControl`: the hook may switch plan/vibe/full mode.
        if let Some(mode) = crate::discovery::mode_request(&self.cfg, &hook) {
            if mode != self.perms.mode() {
                self.perms.set_mode(mode);
                let _ = tx.send(Event::Notify(format!("Extension switched mode to {mode:?}")));
            }
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
        if !(name == "submit" && self.submit_schema.is_some()) {
            match self.perms.verdict(pname, &pinput, &self.cwd) {
                permission::Verdict::Deny(reason) => bail!("{reason}"),
                permission::Verdict::Ask(approval) => {
                    let summary = match &approval.reason {
                        Some(reason) => format!("{reason}\n{}", permission::summary(pname, &pinput)),
                        None => permission::summary(pname, &pinput),
                    };
                    match ask(tx, pname, summary).await {
                        Decision::Allow => {}
                        Decision::Always if approval.remember => self.perms.always(approval.key),
                        Decision::Always => {}
                        Decision::Deny => bail!("the user denied this action"),
                    }
                }
                permission::Verdict::Allow => {}
            }
        }
        // Remember what the file held so `undo` can put it back.
        if let (true, Some(session), Some(path)) = (
            matches!(pname, "write" | "edit"),
            &self.session,
            pinput["path"].as_str().filter(|path| !path.contains("://")),
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
            "write" if input["path"].as_str().is_some_and(|path| path.starts_with("out://")) => {
                if self.agent.is_some() {
                    bail!("out:// replies are not available to sub-agents");
                }
                if !self.cfg.bash_opts.out.enabled {
                    bail!("out:// is disabled by out.enabled");
                }
                let text = tools::str_arg(input, "content")?;
                anyhow::ensure!(text.len() <= 64 * 1024, "out:// replies are capped at 64 KiB");
                let _ = tx.send(Event::Text(text.to_string()));
                Ok("replied to the user mid-task".into())
            }
            "read" => {
                // `read path.png?q=question` asks a vision model about the image instead of returning it.
                let raw = tools::str_arg(input, "path")?;
                let (raw, question) = raw.split_once("?q=").map_or((raw, None), |(path, question)| (path, Some(question)));
                let path = permission::resolve(&self.cwd, raw);
                if let Some(question) = question {
                    let Some((media, bytes)) = context::read_image_bytes(&path)? else {
                        bail!("`?q=` works on images (png, jpg, gif, webp): {}", path.display());
                    };
                    return crate::image::ask(&self.cfg, &self.meta.model, media, &bytes, question).await;
                }
                if let Some(image) = context::read_image(&path)? {
                    self.images.lock().unwrap().push(image);
                    Ok(format!("Image: {}", path.display()))
                } else {
                    // A speculative read for exactly this selector replaces the second file read.
                    match warmed.filter(|w| Some(&w.key) == tools::speculate::key(&self.cwd, input).as_ref()) {
                        Some(warm) => Ok(warm.text),
                        None => tools::run_with(name, input, &self.cwd, &self.cfg).await,
                    }
                }
            }
            name if name == "memory" => {
                let saved = self.save_memory(input).await?;
                if self.memory.is_some() {
                    return Ok(saved);
                }
                // `memory.backend = off`: MEMORY.md edits work, but nothing is recalled later.
                Ok(format!(
                    "{saved}\n(automatic memory is off; set `memory.backend` to local, mnemopi, hindsight or sharpshooter to recall this in later sessions)"
                ))
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
                tools::run_with(name, input, &self.cwd, &self.cfg).await
            }
            "goal" => crate::goal::apply(Some(&self.meta.id), input),
            "generate_image" => {
                let path = tools::media::run(&self.cfg, input, &self.cwd).await?;
                match context::read_image(&path)? {
                    Some(image) => {
                        self.images.lock().unwrap().push(image);
                        Ok(format!("Image: {}", path.display()))
                    }
                    None => Ok(format!("wrote {}", path.display())),
                }
            }
            "computer" => {
                let (text, image) = tools::computer::run(&self.cfg, input).await?;
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
            "browser" => {
                // A saved screenshot joins the conversation as an image, like a computer capture.
                let (text, path) = tools::browser::run(&self.cfg, &self.cwd, input).await?;
                if let Some(path) = path {
                    if let Some(image) = context::read_image(&path)? {
                        self.images.lock().unwrap().push(image);
                        Ok(format!("{text}\nImage: {}", path.display()))
                    } else {
                        Ok(text)
                    }
                } else {
                    Ok(text)
                }
            }
            "submit" if self.submit_schema.is_some() => self.submit(input),
            "task" if self.can_task() => self.task(input, tx).await,
            "ext" => self.ext(input, tx).await,
            "mcp" => self.mcp.run(input, &self.cwd).await,
            name if tools::eval_is_tool(name) => tools::eval_call_tool(name, input, &self.cfg).await,
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
        // `extensionRenderers.transcriptPresenters`: full, one compact row, or hidden. The session and
        // model context keep the block as written; this only reaches the terminal.
        if crate::discovery::presents_transcript(&self.cfg, self.lua.has_hook("present_transcript")) {
            let payload = json!({"tool": name, "input": input, "is_error": result.is_err()});
            let reply = self.lua.hook("present_transcript", payload).await;
            if let Some(mode @ ("full" | "compact" | "hidden")) = reply["mode"].as_str() {
                let _ = tx.send(Event::Remote {
                    command: "present_transcript".into(),
                    data: json!({"mode": mode, "text": reply["text"]}),
                });
            }
        }
        // `todo.backend` mirrors the list to its extension; a configured but missing backend is an error.
        if name == "todo" && result.is_ok() {
            if let Err(error) = crate::todos::mirror(&self.lua, &self.cfg, &self.cwd, input).await {
                result = Err(error);
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

    /// Matches the completed reply against the rules, queues warnings and reports them. Returns the tool
    /// calls TTSR refused to run (id -> error result).
    fn rule_matches(&self, blocks: &[Block], tx: &EventTx) -> HashMap<String, String> {
        if !self.ttsr.has_rules() {
            return HashMap::new();
        }
        let matches = self.ttsr.match_reply(blocks, true);
        for name in &matches.names {
            let _ = tx.send(Event::Notify(format!("ルール適用: {name}")));
        }
        self.pending.lock().unwrap().extend(matches.warnings);
        matches.blocked
    }

    /// `ttsr.judge`: asks the judge model about `question` rules and warns when it says yes.
    async fn judge_rules(&self, reply: &provider::Reply, tx: &EventTx) {
        let Some(judge) = self.ttsr.judge() else { return };
        let candidates = self.ttsr.judged_candidates(&reply.blocks);
        if candidates.is_empty() {
            return;
        }
        let content = crate::rules::Ttsr::judged_content(&reply.blocks);
        match crate::rules::judge(judge, &self.cfg, &content, &candidates).await {
            Ok(names) => {
                for name in &names {
                    let _ = tx.send(Event::Notify(format!("ルール適用（判定）: {name}")));
                }
                self.pending.lock().unwrap().extend(self.ttsr.warnings(&names));
            }
            Err(e) => {
                let _ = tx.send(Event::Notify(format!("Rules judge failed: {e:#}")));
            }
        }
    }

    /// `tool_commands`: the tool a mounted command line names, with its arguments, or `None` for a
    /// normal shell line. `--help` becomes the tool's `help` action so the schema arrives on demand.
    fn mounted_call(&self, command: &str) -> Option<(String, Value)> {
        if !crate::discovery::mounts(&self.cfg) {
            return None;
        }
        // `server__tool`: one MCP tool; its schema stays behind `mcp` search.
        if let Some(server) = self.mcp.names().into_iter().find(|s| command.trim().starts_with(&format!("{s}__"))) {
            let word = command.trim().split_whitespace().next()?.to_string();
            let tool = word.strip_prefix(&format!("{server}__"))?.to_string();
            let (_, args) = crate::discovery::parse(command, std::slice::from_ref(&word))?;
            return Some(match args.get("__help") {
                Some(_) => ("mcp".into(), json!({"action": "search", "query": tool})),
                None => ("mcp".into(), json!({"action": "call", "server": server, "tool": tool, "args": args})),
            });
        }
        // Rarely used tools stay out of the prompt and are reached by name; top-level ones are not remounted.
        let mut names: Vec<String> = tools::ext::CATALOG
            .iter()
            .filter(|(name, _)| !self.cfg.ext_off.iter().any(|off| off == name))
            .map(|(name, _)| (*name).to_string())
            .filter(|name| !self.cfg.tools.iter().any(|top| top == name))
            .collect();
        if self.allowed.is_none() {
            names.extend(self.lua.tools().iter().map(|tool| tool.name.clone()));
        }
        let (bare, args) = crate::discovery::parse(command, &names)?;
        Some(match args.get("__help") {
            Some(_) => ("ext".into(), json!({"action": "help", "tool": bare})),
            None => ("ext".into(), json!({"action": "call", "tool": bare, "args": args})),
        })
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
        if input["action"] == "call"
            && (self.cfg.ext_off.iter().any(|off| off == tool) || self.cfg.extra_tools_opts.off().iter().any(|off| off == tool))
        {
            bail!("ext tool `{tool}` is turned off in settings")
        }
        match input["action"].as_str() {
            Some("help") => tools::ext::help(tool),
            // `dev.autoqa`: the first report asks for consent; a denial switches reporting off.
            Some("call") if tool == "report_issue" => {
                if !crate::discovery::autoqa_enabled(&self.cfg) {
                    bail!("Auto QA reporting is disabled by dev.autoqa")
                }
                if !crate::home().join("autoqa/consent").is_file() {
                    let decision = ask(tx, "report_issue", "Send this report to the GreenCode maintainers? Denying turns Auto QA off until you enable dev.autoqa again.".into()).await;
                    if decision == Decision::Deny {
                        let _ = crate::config::settings::edit_global("dev.autoqa", Some(&["false".into()]));
                        bail!("reporting declined; Auto QA disabled until dev.autoqa is re-enabled")
                    }
                    let _ = std::fs::create_dir_all(crate::home().join("autoqa"));
                    let _ = std::fs::write(crate::home().join("autoqa/consent"), b"granted
");
                }
                tools::ext::run_with(Some(&self.cfg), tool, &args, &self.cwd).await
            }
            Some("call") if tool == "ask" => self.ask_user(&args, tx).await,
            Some("call") if tool == "secret" => self.ask_secret(&text("name")?, &text("reason")?, tx).await,
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
                self.save_memory(&input).await
            }
            Some("call") if tool == "advisor" => self.advise(&args, tx).await,
            Some("call") => tools::ext::run_with(Some(self.cfg.as_ref()), tool, &args, &self.cwd).await,
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

    /// A private dialog for one credential; only where it was stored goes back to the model.
    async fn ask_secret(&self, name: &str, reason: &str, tx: &EventTx) -> Result<String> {
        let path = anyhow::Context::with_context(crate::secrets::ask(tx, name, reason).await?, || format!("the user declined to provide {name}"))?;
        Ok(format!("Saved. Use env var ${name} in your next bash call, or read the file {}. Do not print it.", path.display()))
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
        // `tier.advisor`: a concrete choice applies to the advisor model's family; inherit keeps the main tiers.
        let mut cfg = (*self.cfg).clone();
        if let Some((pname, bare)) = spec.split_once('/') {
            if let Some(p) = cfg.providers.get(pname) {
                if let Some(family) = crate::config::provider::tier_family(pname, &p.kind, &p.base_url, bare) {
                    cfg.provider_opts.tier.override_family(family, cfg.provider_opts.tier.advisor);
                }
            }
        }
        let (provider, model) = provider::make(&cfg, spec)?;
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
                    live: None,
                },
                &|_| {},
            )
            .await?;
        crate::usage_history::record(spec.split_once('/').map_or("greencode", |p| p.0), &model, &reply.usage);
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

    /// Adds the backend's developer instructions (mental models, project decisions) once, at session start.
    async fn remember_instructions(&mut self, tx: &EventTx) {
        let Some(memory) = self.memory.clone() else { return };
        match memory.instructions().await {
            Ok(text) if !text.trim().is_empty() => {
                self.system.push_str("\n\n");
                self.system.push_str(text.trim());
            }
            Ok(_) => {}
            Err(e) => {
                let _ = tx.send(Event::Notify(format!("Memory instructions unavailable: {e:#}")));
            }
        }
    }

    /// Retains the conversation at a turn boundary when the backend says one is due; the request
    /// runs off the turn so a slow server does not delay the reply.
    fn remember(&mut self, tx: &EventTx) {
        let Some(memory) = self.memory.clone() else { return };
        self.memory_turns += 1;
        let turns = self.memory_turns;
        if !memory.retain_due(turns) {
            return;
        }
        let Some(text) = memory.transcript(&self.messages) else { return };
        let session = self.meta.id.clone();
        let tx = tx.clone();
        tokio::spawn(async move {
            if let Err(e) = memory.retain(&session, &text).await {
                let _ = tx.send(Event::Notify(format!("Memory retention failed: {e:#}")));
            }
        });
    }

    async fn auto_learn(&self, tx: &EventTx) -> Result<()> {
        if self.cfg.memory_opts.memory.backend == crate::config::memory::MemoryBackend::Off {
            return Ok(());
        }
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
            .or_else(|| self.cfg.model_roles.get("learning").map(String::as_str))
            // `autolearn.autoContinue` spends one private request even without a learning model.
            .or_else(|| {
                self.cfg
                    .integration_opts
                    .autolearn
                    .auto_continue
                    .then_some(self.meta.model.as_str())
            });
        let text = if let Some(spec) = spec {
            let (provider, model) = provider::make(&self.cfg, spec)?;
            let digest = crate::snap::serialize(&self.messages);
            let tail: String = digest
                .chars()
                .skip(digest.chars().count().saturating_sub(12_000))
                .collect();
            let prompt = format!("Extract at most one durable coding lesson from this completed conversation. Return either NO_LESSON or one concise fact under {} characters, with no bullets or preamble.\n\n{}", max_chars, tail);
            let messages = [Message::user(prompt)];
            let reply = provider.complete(Request { model: &model, system: "You distill only reusable project facts. Never include secrets, credentials, tokens, personal data, or temporary task details.", messages: &messages, tools: &[], max_tokens: 300, live: None }, &|_| {}).await?;
            crate::usage_history::record(spec.split_once('/').map_or("greencode", |p| p.0), &model, &reply.usage);
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
        match self.perms.verdict("memory", &input, &self.cwd) {
            permission::Verdict::Deny(reason) => bail!("{reason}"),
            permission::Verdict::Ask(approval) => {
                match ask(tx, "memory", permission::summary("memory", &input)).await {
                    Decision::Deny => bail!("the user denied this action"),
                    Decision::Always if approval.remember => self.perms.always(approval.key),
                    Decision::Always | Decision::Allow => {}
                }
            }
            permission::Verdict::Allow => {}
        }
        self.save_memory(&input).await.map(|_| ())
    }

    /// One `memory` tool operation: the active backend's bank when it has one, else the `MEMORY.md` files.
    async fn save_memory(&self, input: &Value) -> Result<String> {
        // With memory off nothing is ever recalled, so a write would be silently lost.
        anyhow::ensure!(
            self.cfg.memory_opts.memory.backend != crate::config::memory::MemoryBackend::Off,
            "memory is off (memory.backend = off); turn it on to save memories"
        );
        if let Some(out) = self.memory.as_ref().and_then(|memory| memory.tool(input)) {
            return out.await;
        }
        tools::run("memory", input, &self.cwd).await
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
            // `task.batch = false` restores the flat single-spawn shape: the item's fields sit at the top level.
            let flat;
            let tasks: &[Value] = if self.cfg.task.batch {
                input["tasks"]
                    .as_array()
                    .ok_or_else(|| anyhow::anyhow!("task requires `tasks` array"))?
            } else {
                anyhow::ensure!(
                    input.get("tasks").is_none(),
                    "task.batch is off: pass one spawn's fields at the top level (`prompt`, `agent`, ...), not `tasks`"
                );
                flat = [input.clone()];
                &flat
            };
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
                let backend = match item.get("backend") {
                    None => None,
                    Some(v) => {
                        let b = v.as_str().filter(|s| !s.trim().is_empty()).ok_or_else(|| anyhow::anyhow!("task `backend` must be a backend name"))?;
                        anyhow::ensure!(self.cfg.task.per_call_model == crate::subagents::PerCallModel::On, "a per-task `backend` is disabled by task.per_call_model");
                        anyhow::ensure!(item.get("schema").is_none(), "task `schema` needs a native sub-agent; drop `backend` or `schema`");
                        Some(b.to_string())
                    }
                };
                let model = match item.get("model") {
                    // Another harness's own model id, checked by that harness.
                    Some(v) if backend.is_some() => Some(v.as_str().ok_or_else(|| anyhow::anyhow!("task `model` must be a string"))?.to_string()),
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
                    None if backend.is_some() => None,
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
                let hint = if self.cfg.task.enable_effort {
                    match item.get("effort") {
                        None | Some(Value::Null) => None,
                        Some(v) => {
                            let level = v.as_str().filter(|s| crate::subagents::Effort::parse(s).is_some())
                                .ok_or_else(|| anyhow::anyhow!("task `effort` must be one of minimal, low, medium, high, xhigh, max"))?;
                            Some(level.to_string())
                        }
                    }
                } else {
                    anyhow::ensure!(item.get("effort").is_none(), "a per-task `effort` is disabled by task.enableEffort");
                    None
                };
                let target = model.as_deref().unwrap_or(&self.meta.model);
                let effort = self.cfg.task.effective_effort(&provider::effort_levels(&self.cfg, target), hint, self.cfg.effort.as_deref());
                let schema = match item.get("schema") {
                    Some(_) if !self.cfg.task.structured => {
                        bail!("task `schema` is disabled by task.structured")
                    }
                    Some(v) if v.is_object() && v["type"] == "object" => Some(v.clone()),
                    Some(_) => bail!("task `schema` must be a JSON Schema with type object"),
                    None => None,
                };
                let context = input["context"].as_str().unwrap_or("");
                // `task.steps`: ordered items in place of one prompt; they stream as the subagent finishes each.
                let steps = if self.cfg.task.steps {
                    match item.get("steps") {
                        None => Vec::new(),
                        Some(v) => v.as_array().map(|list| {
                            list.iter().map(|step| step.as_str().map(str::trim).filter(|s| !s.is_empty()).map(str::to_string))
                                .collect::<Option<Vec<_>>>()
                        }).flatten().ok_or_else(|| anyhow::anyhow!("task `steps` must be an array of non-empty strings"))?,
                    }
                } else {
                    anyhow::ensure!(item.get("steps").is_none(), "task `steps` is disabled by task.steps");
                    Vec::new()
                };
                let prompt = if context.is_empty() {
                    prompt.to_string()
                } else {
                    format!("{context}\n\n---\n\n{prompt}")
                };
                specs.push(TaskSpec {
                    name,
                    def,
                    prompt,
                    resolved_model: model.clone().unwrap_or_else(|| self.meta.model.clone()),
                    model,
                    backend,
                    isolated,
                    schema,
                    effort,
                    steps,
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
                // Per-agent advisor: `off` removes the ext tool, `on` keeps the global one, anything else is its model.
                let mut sub_cfg = (*self.cfg).clone();
                if !sub_cfg.task.enable_lsp {
                    // `task.enableLsp`: keep subagent edits cheap; the `lsp` tool itself stays available.
                    sub_cfg.integration_opts.lsp.format_on_write = false;
                    sub_cfg.integration_opts.lsp.diagnostics_on_write = false;
                    sub_cfg.integration_opts.lsp.diagnostics_on_edit = false;
                }
                if let Some(choice) = self.cfg.task.agent_advisor.get(&spec.def.name) {
                    match choice.as_str() {
                        "off" => {
                            if !sub_cfg.ext_off.iter().any(|tool| tool == "advisor") { sub_cfg.ext_off.push("advisor".into()); }
                            sub_cfg.advisor_model = None;
                            sub_cfg.model_roles.remove("advisor");
                            sub_cfg.assist_opts.advisor.enabled = false;
                        }
                        "on" => {}
                        model => sub_cfg.advisor_model = Some(model.to_string()),
                    }
                }
                let sub_cfg_for_assist = sub_cfg.clone();
                let sub = Agent {
                    cfg: Arc::new(sub_cfg),
                    provider: self.provider.clone(),
                    model: self.model.clone(),
                    cwd: self.cwd.clone(),
                    // Sub-agents keep the model's own delegation policy, not the main agent's `task.eager` nudge.
                    system: match crate::subagents::eager_guidance(self.cfg.task.eager) {
                        Some(guidance) => self.system.replace(guidance, "").trim_end().to_string(),
                        None => self.system.clone(),
                    },
                    prompt_context: self.prompt_context.clone(),
                    latest_context: String::new(),
                    // `task.agentIdleTtlMs`: a named agent messaged again continues its transcript, from memory
                    // when it is kept live (0) or from disk once it was parked.
                    messages: if self.cfg.task.agent_idle_ttl_ms != 0 {
                        take_parked(&self.meta.id, self.depth + 1, &spec.name, &spec.def.name)
                    } else {
                        self.resident.lock().unwrap().remove(&resident_key(self.depth + 1, &spec.name))
                    }
                    .unwrap_or_default(),
                    meta: self.meta.clone(),
                    session: None,
                    snap_source: None,
                    perms: self.perms.clone(),
                    lua: self.lua.clone(),
                    mcp: self.mcp.clone(),
                    subagent: true,
                    review: None,
                    agent: Some(spec.def.name.clone()),
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
                    resident: self.resident.clone(),
                    pending: Default::default(),
                    last_tool_call: None,
                    tool_call_repeats: 0,
                    depth: self.depth + 1,
                    ttsr: Arc::new(self.ttsr.fork()),
                    checkpoint: Default::default(),
                    reset: Default::default(),
                    idle_done: false,
                    speculative: None,
                    // Memory retention runs once, in the parent that owns the conversation.
                    memory: None,
                    memory_turns: 0,
                    memory_recalled: true,
                    spent: Mutex::new(0.),
                    cost_warned: Default::default(),
                    live: Default::default(),
                    fallback: Default::default(),
                    warming: Default::default(),
                    warmer: Default::default(),
                    todo_reminders: 0,
                    todo_mutations: 0,
                    todo_nudges: 0,
                    todo_nudge_gap: 12,
                    assist: {
                        let mut rt = crate::assist::Assist::new();
                        rt.set_provider(self.provider.clone());
                        crate::assist::arm_subagent(&mut rt, &sub_cfg_for_assist, &spec.def);
                        rt
                    },
                };
                let tx = tx.clone();
                let limit = self.cfg.task.runtime();
                // `task.workerThreads`: the inline fallback names the job when a worker lane is gone.
                let (fallback_name, fallback_agent) = (spec.name.clone(), spec.def.name.clone());
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
                        let running = match &spec.backend {
                            Some(backend) => {
                                // Its own thread on that harness; the definition's prompt is advice there, not an enforced tool list.
                                let task = if spec.def.name == "task" { spec.prompt.clone() } else { format!("{}\n\n{}", spec.def.prompt, spec.prompt) };
                                let req = crate::delegate::Request { backend: backend.clone(), model: spec.model.clone(), task, name: Some(spec.name.clone()), isolated: spec.isolated, ..Default::default() };
                                let (steer, events) = (sub.steer_rx.into_inner().unwrap(), tx.clone());
                                Box::pin(async move { crate::delegate::run(&sub.meta, sub.cfg.clone(), sub.perms.mode(), req, &events, Some(steer)).await })
                            }
                            None => Self::run_task(sub, &spec, &tx),
                        };
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
                        name: spec.name.clone(),
                        agent: spec.def.name.clone(),
                        result,
                    }
                };
                // `task.workerThreads`: past the main-thread limit, subagents leave the caller's thread.
                let live = self.subs.lock().unwrap().len();
                let max = self.cfg.task.worker_threads;
                let job = async move {
                    match crate::workers::run(max, live, future).await {
                        Ok(job) => job,
                        Err(_) => JobOutcome {
                            name: fallback_name,
                            agent: fallback_agent,
                            result: Err("sub-agent worker stopped".into()),
                        },
                    }
                };
                if background {
                    self.jobs.lock().unwrap().spawn(job);
                } else {
                    foreground.spawn(job);
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

    /// The first line of a text event, for a one-line subagent row.
    fn last_text(ev: &Event) -> String {
        match ev {
            Event::Text(text) => text.lines().next().unwrap_or("").trim().chars().take(80).collect(),
            _ => String::new(),
        }
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
                "name": spec.name, "agent": spec.def.name, "state": state, "detail": detail,
                "model": spec.resolved_model
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
            if let Some(model_spec) = &spec.model {
                let (name, model) =
                    crate::subagents::model_spec(model_spec).map_err(anyhow::Error::msg)?;
                let same_parent = !sub.cfg.providers.contains_key(name)
                    && sub
                        .meta
                        .model
                        .split_once('/')
                        .is_some_and(|(p, _)| p == name);
                if same_parent {
                    if let Some(effort) = &spec.effort {
                        sub.provider = provider::make_with_effort(&sub.cfg, &sub.meta.model, Some(effort))?.0;
                    }
                    sub.model = model.to_string();
                } else {
                    (sub.provider, sub.model) =
                        provider::make_with_effort(&sub.cfg, model_spec, spec.effort.as_deref())?;
                }
                sub.meta.model.clone_from(model_spec);
            } else if let Some(effort) = &spec.effort {
                sub.provider = provider::make_with_effort(&sub.cfg, &sub.meta.model, Some(effort))?.0;
            }
            if spec.effort.is_some() {
                // Nested spawns inherit the effort this agent runs at.
                Arc::make_mut(&mut sub.cfg).effort = spec.effort.clone();
            }
            // `tier.subagent`: inherit keeps the parent's live per-family tiers; a value applies to
            // whichever family this sub-agent's model belongs to.
            let choice = sub.cfg.provider_opts.tier.subagent;
            if let Some((pname, bare)) = sub.meta.model.split_once('/') {
                if let Some(p) = sub.cfg.providers.get(pname) {
                    if let Some(family) = crate::config::provider::tier_family(pname, &p.kind, &p.base_url, bare) {
                        let mut cfg = (*sub.cfg).clone();
                        cfg.provider_opts.tier.override_family(family, choice);
                        sub.provider = provider::make_with_effort(&cfg, &sub.meta.model, spec.effort.as_deref())?.0;
                        sub.cfg = Arc::new(cfg);
                    }
                }
            }
            let worktree = if spec.isolated {
                let id = format!(
                    "sub-{}-{}",
                    std::process::id(),
                    WORKTREE_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                );
                let source = sub.cwd.clone();
                let initial = crate::worktree::head(&source)?;
                let options = crate::worktree::Options {
                    clone: sub.cfg.worktree.clone,
                    backend: sub.cfg.isolation_opts.isolation.backend,
                    ..crate::worktree::Options::default()
                };
                let path = crate::worktree::create_with(&source, &sub.cfg.worktree.dir(), &id, &options)?;
                let tree = Arc::new(TaskWorktree {
                    repo: source,
                    path,
                    branch: format!("greencode/{id}"),
                    initial,
                    state: Mutex::new(WorktreeState::PristineCandidate),
                    tx: tx.clone(),
                });
                sub.task_worktree = Some(tree.clone());
                sub.cwd = tree.path.clone();
                sub.lua = Arc::new(Lua::new(&tree.path, &sub.cfg.integration_opts));
                sub.mcp = Arc::new(Mcp::load_with_project(&tree.path, sub.cfg.mcp_project_config, &sub.cfg.integration_opts.mcp));
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
            // `task.steps`: the first step goes in the opening prompt, later batches follow each finished step.
            let batches = crate::subagents::step_batches(&spec.steps);
            let opening = if spec.steps.is_empty() { spec.prompt.clone() } else { batches[0].clone() };
            let mut prompt = format!("{role}\n\n---\n\n{opening}");
            let mut next = 1;
            let started = std::time::Instant::now();
            let mut probes = 0;
            let mut probe_pending = false;
            // `task.completionProbe`: ask for a rough estimate at 2, 5, 10 and 30 minutes, then hourly.
            let probe_enabled = sub.cfg.task.completion_probe;
            let steer_tx = sub.steer_tx.clone();
            loop {
                let result = {
                    let run = sub.run_turn(prompt, &stx);
                    tokio::pin!(run);
                    loop {
                        let probe = probe_enabled && !probe_pending;
                        tokio::select! {
                            r = &mut run => break r,
                            Some(ev) = srx.recv() => {
                                if probe_pending && matches!(ev, Event::Text(_)) {
                                    probe_pending = false;
                                    Self::sub_event(tx, spec, "estimate", &Self::last_text(&ev));
                                }
                                Self::sub_forward(tx, spec, ev)
                            }
                            _ = tokio::time::sleep(std::time::Duration::from_secs(30)), if probe => {
                                if crate::subagents::probe_due(started.elapsed(), probes) {
                                    probes += 1;
                                    probe_pending = true;
                                    let _ = steer_tx.send(json!({"type": "steer", "message": crate::subagents::PROBE}));
                                }
                            }
                        }
                    }
                };
                while let Ok(ev) = srx.try_recv() {
                    Self::sub_forward(tx, spec, ev);
                }
                result?;
                let Some(batch) = batches.get(next) else { break };
                next += 1;
                prompt = batch.clone();
            }
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
                match integrate_isolated(&sub.cfg, spec, &tree).await {
                    Ok(note) => text.push_str(&note),
                    Err(e) => text.push_str(&format!(
                        "\n\n[worktree {}, branch {}]\n{}\nIsolation was not applied: {e:#}. The worktree and its branch are kept.",
                        tree.path.display(),
                        tree.branch,
                        crate::worktree::summary(&tree.path)
                    )),
                }
            }
            // `task.agentIdleTtlMs`: the run's `Agent` is dropped here either way; >0 parks the transcript on
            // disk, 0 keeps it in memory so the agent's history survives until the process exits.
            // shortcut: parking happens when the run ends, not after the TTL, because no idle subagent object is kept.
            if sub.cfg.task.agent_idle_ttl_ms != 0 {
                let _ = park_subagent(&sub, &spec.name);
            } else {
                sub.resident.lock().unwrap().insert(resident_key(sub.depth, &spec.name), sub.messages.clone());
            }
            Ok(text)
        })
    }
}

impl Backend for Agent {
    fn live(&self) -> Option<mpsc::UnboundedSender<Value>> {
        Some(self.steer_tx.clone())
    }

    /// Waits out the idle timeout once the thread sits above the idle token threshold.
    fn idle<'a>(&'a mut self) -> BoxFut<'a, Result<()>> {
        Box::pin(async move {
            let maintenance = &self.cfg.compaction_opts.maintenance;
            if self.idle_done || !self.cfg.compaction_enabled || !maintenance.idle_due(self.context) {
                return std::future::pending().await;
            }
            tokio::time::sleep(maintenance.idle_delay()).await;
            Ok(())
        })
    }

    fn handle_idle<'a>(&'a mut self, tx: &'a EventTx) -> BoxFut<'a, Result<()>> {
        Box::pin(async move {
            self.idle_done = true;
            let due = self.cfg.compaction_enabled && self.cfg.compaction_opts.maintenance.idle_due(self.context);
            if due {
                self.compact_auto(tx, true).await?;
            }
            Ok(())
        })
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
            if command["type"] == "prewalk" {
                // `/prewalk [restart|MODEL|off]`: `restart` also returns to the planning model.
                if let Some(spec) = command["restart"].as_str().filter(|s| !s.is_empty()) {
                    self.switch_model(spec, tx)?;
                }
                let target = command["target"].as_str().unwrap_or("");
                let armed = (!target.is_empty())
                    .then(|| crate::assist::resolve_target(&self.cfg, target).unwrap_or_else(|| target.to_string()));
                self.assist.set_prewalk(armed);
                return Ok(());
            }
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
            self.idle_done = false;
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
                if !self.cfg.integration_opts.lsp.lazy {
                    let cwd = self.cwd.clone();
                    tokio::spawn(async move { crate::tools::lsp::warm(&cwd).await });
                }
                self.mcp
                    .warm(&self.cwd, self.cfg.integration_opts.mcp.startup_timeout_ms)
                    .await;
                self.lua
                    .hook("session_start", json!({ "cwd": self.cwd }))
                    .await;
                self.notes(tx);
                self.remember_instructions(tx).await;
            }
            let mut prompt = text;
            if let Some(rest) = prompt.strip_prefix('/') {
                let (name, args) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
                if name == "compact" {
                    return match args.trim() {
                        "" => self.compact_auto(tx, false).await,
                        "snap" => self.compact(tx, Method::Snap, false).await,
                        "summary" => self.compact(tx, Method::Summary, false).await,
                        _ => bail!("usage: /compact [snap|summary]"),
                    };
                } else if name == "ratchet" && self.cfg.extra_tools_opts.ratchet.enabled {
                    prompt = tools::ratchet::prelude(args.trim());
                } else if let Some(body) = skills::command(&self.cwd, &self.cfg, name) {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::agent_loop::ToolCallLoopGuardCfg;

    #[test]
    fn subagent_events_carry_the_resolved_model_for_the_badge() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let def = crate::subagents::AgentDef {
            name: "task".into(), description: String::new(), tools: None, model: None,
            prompt: String::new(), bundled: true, prewalk: None,
        };
        let spec = TaskSpec {
            name: "fix-login".into(), def, prompt: String::new(),
            model: None, backend: None, isolated: true, schema: None, effort: None,
            steps: Vec::new(), resolved_model: "anthropic/claude-sonnet-4".into(),
        };
        Agent::sub_event(&tx, &spec, "running", "edit src/auth.rs\nsecond line");
        match rx.try_recv().unwrap() {
            Event::Remote { command, data } => {
                assert_eq!(command, "subagent");
                assert_eq!(data["model"], "anthropic/claude-sonnet-4");
                assert_eq!(data["detail"], "edit src/auth.rs");
            }
            other => panic!("unexpected event {other:?}"),
        }
    }

    #[test]
    fn steering_mode_delivers_all_or_one() {
        let mut queued = vec!["first".to_string(), "second".into(), "third".into()];
        assert_eq!(steering_batch(&mut queued, SteeringMode::OneAtATime), ["first"]);
        assert_eq!(queued, ["second", "third"]);
        assert_eq!(steering_batch(&mut queued, SteeringMode::OneAtATime), ["second"]);
        assert_eq!(steering_batch(&mut queued, SteeringMode::All), ["third"]);
        assert!(queued.is_empty());
        assert!(steering_batch(&mut queued, SteeringMode::OneAtATime).is_empty());
    }

    #[test]
    fn tool_call_guard_trips_at_threshold_and_skips_exempt_tools() {
        let guard = ToolCallLoopGuardCfg {
            enabled: true,
            threshold: 3,
            exempt_tools: vec!["wait".into(), "mcp__peers_wait".into()],
        };
        let (mut last, mut repeats) = (None, 0);
        let call = |last: &mut Option<String>, repeats: &mut u32, name: &str, input: Value| {
            call_repeats(last, repeats, name, &input, &guard)
        };
        let read = json!({"path": "src/main.rs"});
        assert!(!call(&mut last, &mut repeats, "read", read.clone()));
        assert!(!call(&mut last, &mut repeats, "read", read.clone()));
        assert!(call(&mut last, &mut repeats, "read", read.clone()));
        assert!(!call(&mut last, &mut repeats, "read", read.clone()));
        assert!(!call(&mut last, &mut repeats, "read", json!({"path": "src/lib.rs"})));
        for _ in 0..10 {
            assert!(!call(&mut last, &mut repeats, "wait", json!({})));
            assert!(!call(&mut last, &mut repeats, "bash", json!({"command": "mcp__peers_wait --id 1"})));
        }
        let disabled = ToolCallLoopGuardCfg { enabled: false, ..guard.clone() };
        for _ in 0..10 {
            assert!(!call_repeats(&mut last, &mut repeats, "read", &read, &disabled));
        }
    }

    #[test]
    fn stream_guard_detects_verbatim_tails_only_when_enabled() {
        let looped = vec![Block::Text { text: "let me check the file.\n".repeat(20) }];
        assert!(reply_repeats(&looped, true));
        assert!(!reply_repeats(&looped, false));
        let thinking = vec![Block::Other(json!({"type": "thinking", "thinking": "waiting ".repeat(40)}))];
        assert!(reply_repeats(&thinking, true));
        let distinct = (0..30).map(|i| format!("unique line {i} with differing words\n")).collect::<String>();
        assert!(!repeats(&distinct));
        assert!(repeats(&"x".repeat(500)));
    }

    #[test]
    fn isolated_changes_apply_or_are_kept_as_a_patch() {
        let root = std::env::temp_dir().join(format!("greencode-isolate-agent-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let repo = root.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let run = |args: &[&str]| {
            let out = std::process::Command::new("git").arg("-C").arg(&repo)
                .args(["-c", "user.name=t", "-c", "user.email=t@example.com"]).args(args).output().unwrap();
            assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        };
        run(&["init", "-q"]);
        std::fs::write(repo.join("tracked"), "one").unwrap();
        run(&["add", "."]);
        run(&["commit", "-qm", "initial"]);

        let def = crate::subagents::bundled().into_iter().find(|d| d.name == "task").unwrap();
        let spec = TaskSpec { name: "iso".into(), def, prompt: String::new(), resolved_model: String::new(), model: None, backend: None, isolated: true, schema: None, effort: None, steps: Vec::new() };
        let mut cfg = crate::config::Config::default();
        cfg.worktree.base = Some(root.join("trees").to_string_lossy().into_owned());
        let tree = |id: &str, path: std::path::PathBuf| TaskWorktree {
            repo: repo.clone(), path, branch: format!("greencode/{id}"), initial: String::new(),
            state: Mutex::new(WorktreeState::Completed), tx: mpsc::unbounded_channel().0,
        };
        let rt = tokio::runtime::Runtime::new().unwrap();

        cfg.task.isolation.apply = false;
        let kept = crate::worktree::create(&repo, &cfg.worktree.dir(), "kept").unwrap();
        std::fs::write(kept.join("tracked"), "two").unwrap();
        let note = rt.block_on(integrate_isolated(&cfg, &spec, &tree("kept", kept.clone()))).unwrap();
        let patch = crate::worktree::patch_file(&cfg.worktree.dir(), "kept");
        assert!(note.contains("kept") && patch.exists());
        assert!(std::fs::read_to_string(&patch).unwrap().contains("two"));
        assert_eq!(std::fs::read_to_string(repo.join("tracked")).unwrap(), "one");
        assert!(kept.exists(), "the worktree is retained");

        cfg.task.isolation.apply = true;
        let applied = crate::worktree::create(&repo, &cfg.worktree.dir(), "applied").unwrap();
        std::fs::write(applied.join("tracked"), "three").unwrap();
        rt.block_on(integrate_isolated(&cfg, &spec, &tree("applied", applied.clone()))).unwrap();
        assert_eq!(std::fs::read_to_string(repo.join("tracked")).unwrap(), "three");
        assert!(!applied.exists(), "an applied worktree is removed");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn sleep_prevention_adds_lower_levels() {
        assert!(sleep_flags(SleepPrevention::Off).is_empty());
        assert_eq!(sleep_flags(SleepPrevention::Idle), ["-i"]);
        assert_eq!(sleep_flags(SleepPrevention::Display), ["-i", "-d"]);
        assert_eq!(sleep_flags(SleepPrevention::System), ["-i", "-d", "-s"]);
    }
}
