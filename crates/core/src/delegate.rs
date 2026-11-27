//! Cross-harness orchestration (T3 Code's orchestrator MCP): a thread on any backend starts child threads on another
//! backend, model or linked machine, creates and messages other threads, asks for secrets and manages schedules.
//! Each child is its own GreenCode thread (`Meta::parent` names the parent) with the backend's own history.
//! External parents (ACP, Pi, OMP) reach these tools over a loopback MCP endpoint owned by this process;
//! native parents use `task` items with `backend`.
use crate::{
    config::Config,
    permission::{Mode, Perms},
    session::{Meta, Session, View},
    Block, Command, Event, EventTx, Role,
};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{Arc, LazyLock, Mutex},
    time::Duration,
};
use tokio::sync::{mpsc, oneshot, watch};

/// Longest waiting call; MCP clients time tool calls out, and a timeout does not cancel the child.
const WAIT_LIMIT: Duration = Duration::from_secs(45);

/// Told to the parent agent once: with the tool list alone, agents that defer MCP tools never find them.
pub const INSTRUCTIONS: &str = "## GreenCode delegation
- `delegate_task` (GreenCode MCP) starts a child agent on any harness or model GreenCode runs: `native` (provider/model), `codex`, `claude`, `opencode`, `hermes`, `pi`, `omp` and other ACP agents, or on a linked machine (`target.environmentId`). Call `orchestrator_capabilities` once before the first delegation. Use it when the user asks for another provider, model or harness, a separate checkout (`isolation:\"worktree\"`), or delegated parallel work; for your own model use your native subagent tool. The child sees only `task`, not this conversation.
- Default `mode:\"async\"`: the child's completion wakes this thread with its result. Do not poll `task_status` or sleep; continue other work and end your turn when none is left. `mode:\"wait\"` only when this turn needs the result now (at most 45 s; a timeout does not cancel the child).
- `task_cancel` stops a child you replace or no longer need. Never fall back to another provider or CLI when the requested one is unavailable; say so.
- Other threads: `create_threads` (new top-level threads, not delegation), `thread_list`, `thread_read`, `thread_send`, `thread_wait`, `thread_interrupt`, `thread_update` (rename). Credentials: `request_secret`, never in chat. Recurring work: `schedule_task`. Long background work: `background_wait`.";

const CHILD_PREFACE: &str = "<greencode_delegated_task>\nYou are a child agent started by another agent through GreenCode. Do this task yourself; start no sub-agents unless it says to. Your final message is the only result the parent reads, so end with a concise, complete answer.\n</greencode_delegated_task>\n\n";

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Outcome {
    Completed(String),
    Failed(String),
    Cancelled,
}

impl Outcome {
    fn status(&self) -> &'static str {
        match self {
            Outcome::Completed(_) => "completed",
            Outcome::Failed(_) => "failed",
            Outcome::Cancelled => "cancelled",
        }
    }
    fn text(&self) -> &str {
        match self {
            Outcome::Completed(s) | Outcome::Failed(s) => s,
            Outcome::Cancelled => "cancelled",
        }
    }
}

/// What a thread's host registered in `spawn`; used for MCP calls, row events, injected turns and wake-ups.
struct Sink {
    events: mpsc::WeakUnboundedSender<Event>,
    cmd: mpsc::WeakUnboundedSender<Command>,
    cfg: Arc<Config>,
    perms: Arc<Perms>,
    meta: Meta,
    wake: Arc<tokio::sync::Notify>,
    busy: Arc<watch::Sender<bool>>,
    /// Messages from other threads, delivered as this thread's next turn.
    inbox: Vec<String>,
}

/// Returned to `spawn`: `wake` fires when finished results or messages wait for an idle turn; `busy` mirrors its turns.
pub(crate) struct Host {
    pub wake: Arc<tokio::sync::Notify>,
    pub busy: Arc<watch::Sender<bool>>,
}

struct Task {
    parent: String,
    name: String,
    backend: String,
    model: String,
    /// A delegated child, shown as a row in its parent; threads made by `create_threads` or resumed by `thread_send` are not.
    child: bool,
    cmd: Option<mpsc::UnboundedSender<Command>>,
    done: watch::Receiver<Option<Outcome>>,
    /// Completion wakes the parent (MCP `mode: async`); native `task` items wait in the turn instead.
    wake: bool,
    delivered: bool,
    /// Live child events until a viewer (the desktop app) takes them; approvals go to the viewer once taken.
    view: Option<mpsc::UnboundedReceiver<Event>>,
    viewed: bool,
    cancel_requested: bool,
    worktree: Option<(PathBuf, String)>,
    /// Row events go here for native `task` items; MCP children use the parent's registered sink.
    events: Option<EventTx>,
}

/// A task as saved in `delegations.json`, so results and statuses survive a restart.
#[derive(Serialize, Deserialize)]
struct Record {
    parent: String,
    name: String,
    backend: String,
    model: String,
    child: bool,
    wake: bool,
    delivered: bool,
    worktree: Option<(PathBuf, String)>,
    outcome: Option<Outcome>,
}

static SINKS: LazyLock<Mutex<HashMap<String, Sink>>> = LazyLock::new(Default::default);
static TASKS: LazyLock<Mutex<HashMap<String, Task>>> = LazyLock::new(|| Mutex::new(load_records()));
/// MCP bearer token -> parent thread id.
static TOKENS: LazyLock<Mutex<HashMap<String, String>>> = LazyLock::new(Default::default);
static ENDPOINT: tokio::sync::OnceCell<String> = tokio::sync::OnceCell::const_new();

fn records_path() -> PathBuf {
    crate::home().join("delegations.json")
}

fn load_records() -> HashMap<String, Task> {
    let records: HashMap<String, Record> = std::fs::read(records_path()).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
    records.into_iter().map(|(id, r)| {
        // A task that was running when GreenCode stopped has no process left.
        let outcome = r.outcome.unwrap_or_else(|| Outcome::Failed("interrupted: GreenCode stopped while this task ran".into()));
        let task = Task {
            parent: r.parent, name: r.name, backend: r.backend, model: r.model, child: r.child, cmd: None,
            done: watch::channel(Some(outcome)).1, wake: r.wake, delivered: r.delivered, view: None, viewed: false,
            cancel_requested: false, worktree: r.worktree, events: None,
        };
        (id, task)
    }).collect()
}

fn persist(tasks: &HashMap<String, Task>) {
    // Unit tests use fake parents; only real runs (and integration tests with their own home) write the file.
    if cfg!(test) { return; }
    let records: HashMap<&String, Record> = tasks.iter().map(|(id, t)| (id, Record {
        parent: t.parent.clone(), name: t.name.clone(), backend: t.backend.clone(), model: t.model.clone(), child: t.child,
        wake: t.wake, delivered: t.delivered, worktree: t.worktree.clone(), outcome: t.done.borrow().clone(),
    })).collect();
    // Results may quote private code, so owner-only. A failed save only loses records across a restart.
    let _ = serde_json::to_vec(&records).map(|data| crate::remote::write_private(&records_path(), &data));
}

/// Called by `spawn` for every thread it runs, so orchestration tools know the thread's settings, UI and agent.
pub(crate) fn register(meta: &Meta, events: &EventTx, cmd: &mpsc::UnboundedSender<Command>, cfg: Arc<Config>, perms: Arc<Perms>) -> Host {
    let wake = Arc::new(tokio::sync::Notify::new());
    let busy = Arc::new(watch::channel(false).0);
    SINKS.lock().unwrap().insert(meta.id.clone(), Sink {
        events: events.downgrade(), cmd: cmd.downgrade(), cfg, perms, meta: meta.clone(), wake: wake.clone(), busy: busy.clone(), inbox: vec![],
    });
    // Results that finished while GreenCode was closed are delivered once the parent runs again.
    if TASKS.lock().unwrap().values().any(|t| t.parent == meta.id && t.wake && !t.delivered && t.done.borrow().is_some()) {
        wake.notify_one();
    }
    Host { wake, busy }
}

pub(crate) fn update(id: &str, cfg: Option<Arc<Config>>, model: Option<&str>) {
    if let Some(sink) = SINKS.lock().unwrap().get_mut(id) {
        if let Some(cfg) = cfg { sink.cfg = cfg; }
        if let Some(model) = model { sink.meta.model = model.to_string(); }
    }
}

pub(crate) fn unregister(id: &str) {
    SINKS.lock().unwrap().remove(id);
    TOKENS.lock().unwrap().retain(|_, parent| parent != id);
}

fn parent_events(parent: &str) -> Option<EventTx> {
    SINKS.lock().unwrap().get(parent).and_then(|s| s.events.upgrade())
}

fn first_line(text: &str, limit: usize) -> String {
    text.lines().find(|l| !l.trim().is_empty()).unwrap_or("").trim().chars().take(limit).collect()
}

fn row(task: &Task, id: &str, state: &str, detail: &str) -> Event {
    let label = if task.model.is_empty() || task.model == "default" { task.backend.clone() } else { format!("{} · {}", task.backend, task.model) };
    Event::Remote {
        command: "subagent".into(),
        data: json!({"name": task.name, "agent": label, "state": state, "detail": first_line(detail, 80), "task": id, "thread": id}),
    }
}

/// A child's row update in its parent; top-level threads have none.
fn send_row(id: &str, state: &str, detail: &str) {
    let tasks = TASKS.lock().unwrap();
    let Some(task) = tasks.get(id).filter(|t| t.child) else { return };
    let ev = row(task, id, state, detail);
    let target = task.events.clone().or_else(|| parent_events(&task.parent));
    drop(tasks);
    if let Some(tx) = target { let _ = tx.send(ev); }
}

fn rank(mode: Mode) -> u8 {
    match mode { Mode::Plan => 0, Mode::Ask => 1, Mode::AutoEdit => 2, Mode::Full => 3 }
}

/// The mode a child runs in: the parent's unless a narrower one is asked for.
fn child_mode(parent: Mode, requested: Option<&str>) -> Result<Mode> {
    let Some(r) = requested.filter(|r| *r != "inherit") else { return Ok(parent) };
    let mode: Mode = serde_json::from_value(json!(r)).map_err(|_| anyhow::anyhow!("unknown mode `{r}`: inherit, plan, ask, auto-edit or full"))?;
    anyhow::ensure!(rank(mode) <= rank(parent), "mode `{r}` is broader than this thread's; a child cannot have more rights than its parent");
    Ok(mode)
}

/// The backends a child can run on, with known models; `native` models are `provider/model`.
pub fn capabilities(cfg: &Config, parent: &Meta) -> Value {
    let mut native: std::collections::BTreeSet<String> = cfg.models.iter().cloned().collect();
    native.insert(cfg.model.clone());
    for (name, p) in &cfg.providers {
        for model in &p.models { native.insert(format!("{name}/{model}")); }
    }
    let cached = crate::cached_project_backend_models(&parent.cwd);
    let mut backends = vec![json!({"backend": "native", "models": native, "default_model": cfg.model})];
    for b in &cfg.backends {
        if cfg.policy.as_ref().is_some_and(|p| !p.backend_allowed(&b.name)) { continue; }
        let mut models: Vec<String> = cached.get(&b.name).cloned().unwrap_or_default();
        if let Some(extra) = cfg.backend_overrides.get(&b.name) { models.extend(extra.models.iter().cloned()); }
        backends.push(json!({"backend": b.name, "models": models, "default_model": "default"}));
    }
    let environments: Vec<Value> = environments().into_iter().map(|e| json!({"environmentId": e.id, "url": e.url})).collect();
    json!({"current": {"backend": parent.backend, "model": parent.model}, "backends": backends,
           "modes": ["inherit", "plan", "ask", "auto-edit", "full"], "environments": environments,
           "note": "Pass `target.backend` and optionally `target.model` and `target.mode` (never broader than yours). Models listed for external backends are the ones seen so far; that backend's own model ids work too. On a linked machine (`target.environmentId`) also pass `target.cwd`, a project directory there."})
}

#[derive(Default)]
pub struct Request {
    pub backend: String,
    pub model: Option<String>,
    pub task: String,
    pub name: Option<String>,
    pub isolated: bool,
    /// Narrower than the parent's mode; `None` inherits it.
    pub mode: Option<Mode>,
    /// A new top-level thread (`create_threads`) instead of a child.
    pub top_level: bool,
}

fn new_task(parent: &str, name: String, backend: String, model: String, child: bool, wake: bool, events: Option<EventTx>) -> Task {
    Task {
        parent: parent.into(), name, backend, model, child, cmd: None, done: watch::channel(None).1, wake, delivered: false,
        view: None, viewed: false, cancel_requested: false, worktree: None, events,
    }
}

/// Starts a child (or, with `top_level`, a new thread) and returns its id, which is also the task id.
pub fn start(parent: &Meta, cfg: Arc<Config>, mode: Mode, req: Request, wake: bool, events: Option<EventTx>) -> Result<String> {
    let backend = crate::antigravity::canonical_backend(req.backend.trim()).to_string();
    if backend != "native" && !cfg.backends.iter().any(|b| b.name == backend) {
        let known: Vec<_> = std::iter::once("native").chain(cfg.backends.iter().map(|b| b.name.as_str())).collect();
        bail!("unknown backend `{backend}`; available: {}", known.join(", "));
    }
    if let Some(policy) = &cfg.policy {
        anyhow::ensure!(policy.backend_allowed(&backend), "backend `{backend}` is blocked by the managed policy");
    }
    let model = match req.model.as_deref().map(str::trim).filter(|m| !m.is_empty()) {
        Some(m) if backend == "native" && !m.contains('/') => bail!("native models are `provider/model`, got `{m}`"),
        Some(m) => m.to_string(),
        None if backend == "native" => cfg.model.clone(),
        None => "default".into(),
    };
    anyhow::ensure!(!req.task.trim().is_empty(), "`task` must not be empty");
    let mode = match req.mode {
        Some(m) if rank(m) > rank(mode) => bail!("a child cannot have a broader mode than its parent"),
        Some(m) => m,
        None => mode,
    };
    let name = {
        let tasks = TASKS.lock().unwrap();
        // Finished tasks keep their result, but their names free up, as with native sub-agents.
        let taken = |n: &str| tasks.values().any(|t| t.parent == parent.id && t.name == n && t.cmd.is_some());
        match req.name.as_deref().map(str::trim).filter(|n| !n.is_empty()) {
            Some(n) if taken(n) && !req.top_level => bail!("task name `{n}` is already in use in this thread"),
            Some(n) => n.to_string(),
            None if req.top_level => first_line(&req.task, 50),
            None => (1..).map(|i| format!("{backend}{i}")).find(|n| !taken(n)).unwrap(),
        }
    };
    let mut meta = Meta::new(parent.cwd.clone(), model.clone(), backend.clone());
    meta.subagent_model = cfg.subagent_model.clone();
    meta.repo = parent.repo.clone();
    let worktree = if req.isolated {
        let path = crate::worktree::create(&parent.cwd, &crate::home().join("subagent-worktrees"), &meta.id)?;
        meta.repo = Some(parent.repo.clone().unwrap_or_else(|| parent.cwd.clone()));
        meta.cwd = path.clone();
        Some((path, format!("{}{}", crate::worktree::Options::default().prefix, meta.id)))
    } else {
        None
    };
    let prompt = if req.top_level {
        meta.title = first_line(&name, 60);
        req.task.clone()
    } else {
        meta.title = format!("↳ {name}: {}", first_line(&req.task, 60));
        meta.parent = Some(parent.id.clone());
        let place = worktree.as_ref().map(|(p, b)| format!("Work only in the isolated worktree {} (branch {b}).\n\n", p.display())).unwrap_or_default();
        format!("{CHILD_PREFACE}{place}{}", req.task)
    };
    let mut task = new_task(&parent.id, name, backend, model, !req.top_level, wake && !req.top_level, events);
    task.worktree = worktree;
    let opened = req.top_level.then(|| meta.clone());
    let id = launch(meta, prompt, cfg, mode, task)?;
    if let Some(meta) = opened { announce(&parent.id, &meta); }
    Ok(id)
}

/// Tells the caller's UI that `meta`'s thread now runs here, so it can list it and follow its events.
fn announce(caller: &str, meta: &Meta) {
    if let Some(tx) = parent_events(caller) {
        let _ = tx.send(Event::Remote { command: "thread_opened".into(), data: json!({"meta": meta}) });
    }
}

/// Runs `prompt` as the next turn of `meta`'s thread in a process-local agent, tracked as task `meta.id`.
fn launch(meta: Meta, prompt: String, cfg: Arc<Config>, mode: Mode, task: Task) -> Result<String> {
    let id = meta.id.clone();
    // Children never ask with broader rights than their parent has now.
    let (cmd, rx) = crate::spawn(&tokio::runtime::Handle::current(), cfg, meta, Arc::new(Perms::new(mode)));
    cmd.send(Command::Prompt(prompt.clone()))?;
    track(id.clone(), cmd, rx, task, Some(prompt));
    Ok(id)
}

/// Follows a task's events until its turn ends, then records the outcome and wakes the parent if asked.
/// `prompt` reaches a viewer first, so one that already shows the thread's history shows the turn's prompt too.
fn track(child: String, cmd: mpsc::UnboundedSender<Command>, mut rx: mpsc::UnboundedReceiver<Event>, mut task: Task, prompt: Option<String>) {
    let (view_tx, view_rx) = mpsc::unbounded_channel();
    if let Some(text) = prompt { let _ = view_tx.send(Event::Remote { command: "injected_prompt".into(), data: json!({"text": text}) }); }
    let (done_tx, done) = watch::channel(None);
    task.cmd = Some(cmd);
    task.done = done;
    task.view = Some(view_rx);
    {
        let mut tasks = TASKS.lock().unwrap();
        tasks.insert(child.clone(), task);
        persist(&tasks);
    }
    send_row(&child, "running", "");
    tokio::spawn(async move {
        let mut error = None;
        let mut text = String::new();
        let mut finished = false;
        // Approvals and questions go to the viewer once one took the child's events, else to the parent's UI.
        let route = |view_tx: &EventTx| {
            let tasks = TASKS.lock().unwrap();
            let task = tasks.get(&child)?;
            if task.viewed { Some(view_tx.clone()) } else { task.events.clone().or_else(|| parent_events(&task.parent)) }
        };
        while let Some(ev) = rx.recv().await {
            let ev = match ev {
                Event::Permission { tool, summary, reply } => {
                    let name = TASKS.lock().unwrap().get(&child).map(|t| t.name.clone()).unwrap_or_default();
                    // Unanswerable (no live UI): the dropped reply denies.
                    if let Some(tx) = route(&view_tx) {
                        let _ = tx.send(Event::Permission { tool: format!("{name}: {tool}"), summary, reply });
                    }
                    continue;
                }
                Event::Dialog { request, reply } => {
                    match route(&view_tx) {
                        Some(tx) => { let _ = tx.send(Event::Dialog { request, reply }); }
                        None => { let _ = reply.send(json!({"cancelled": true})); }
                    }
                    continue;
                }
                ev => ev,
            };
            match &ev {
                Event::ToolStart { name, input, .. } => {
                    let arg = ["path", "command", "pattern", "url", "query"].iter().find_map(|k| input[*k].as_str());
                    send_row(&child, "running", &arg.map_or(name.clone(), |a| format!("{name} {a}")));
                }
                Event::Text(s) => text.push_str(s),
                Event::Error(e) => error = Some(e.clone()),
                _ => {}
            }
            let done = matches!(ev, Event::Done { .. });
            let _ = view_tx.send(ev);
            if done { finished = true; break; }
        }
        let cancelled = TASKS.lock().unwrap().get(&child).is_some_and(|t| t.cancel_requested);
        let outcome = if cancelled {
            Outcome::Cancelled
        } else if let Some(e) = error {
            Outcome::Failed(e)
        } else if !finished {
            Outcome::Failed("the child thread ended before finishing its turn".into())
        } else {
            let mut result = last_reply(&child).filter(|t| !t.trim().is_empty()).unwrap_or(text);
            if let Some((path, branch)) = TASKS.lock().unwrap().get(&child).and_then(|t| t.worktree.clone()) {
                result.push_str(&format!("\n\n[worktree {}, branch {branch}]\n{}", path.display(), crate::worktree::summary(&path)));
            }
            Outcome::Completed(result)
        };
        let wake = {
            let mut tasks = TASKS.lock().unwrap();
            let Some(task) = tasks.get_mut(&child) else { return };
            // Dropping the last sender ends the child's `spawn` loop and its backend process.
            task.cmd = None;
            task.view = None;
            task.wake && !task.delivered
        };
        let state = match &outcome { Outcome::Completed(_) => "done", Outcome::Failed(_) => "failed", Outcome::Cancelled => "cancelled" };
        send_row(&child, state, outcome.text());
        done_tx.send_replace(Some(outcome));
        persist(&TASKS.lock().unwrap());
        if wake {
            let parent = TASKS.lock().unwrap().get(&child).map(|t| t.parent.clone()).unwrap_or_default();
            // A permit is stored, so a parent mid-turn picks it up once idle.
            if let Some(sink) = SINKS.lock().unwrap().get(&parent) { sink.wake.notify_one(); }
        }
    });
}

fn last_reply(id: &str) -> Option<String> {
    let (_, messages) = Session::load(id).ok()?;
    let last = messages.iter().rev().find(|m| m.role == Role::Assistant)?;
    Some(last.content.iter().filter_map(|b| if let Block::Text { text } = b { Some(text.as_str()) } else { None }).collect::<Vec<_>>().join("\n"))
}

/// Waits for a task; `None` when `limit` passes first or the task is unknown.
pub async fn wait(id: &str, limit: Option<Duration>) -> Option<Outcome> {
    let mut done = TASKS.lock().unwrap().get(id)?.done.clone();
    let finished = async { done.wait_for(Option::is_some).await.ok().and_then(|o| o.clone()) };
    match limit { Some(l) => tokio::time::timeout(l, finished).await.ok()?, None => finished.await }
}

/// Runs a child to completion for a native `task` item; dropping the future cancels the child.
pub async fn run(parent: &Meta, cfg: Arc<Config>, mode: Mode, req: Request, events: &EventTx, mut steer: Option<mpsc::UnboundedReceiver<Value>>) -> Result<String> {
    // Cancels the child when the parent's turn drops this future; a no-op once it finished.
    struct Guard(String);
    impl Drop for Guard { fn drop(&mut self) { let _ = cancel(&self.0); } }
    let id = start(parent, cfg, mode, req, false, Some(events.clone()))?;
    let _guard = Guard(id.clone());
    let finished = wait(&id, None);
    tokio::pin!(finished);
    let outcome = loop {
        tokio::select! {
            o = &mut finished => break o,
            Some(v) = async { match steer.as_mut() { Some(rx) => rx.recv().await, None => std::future::pending().await } } => {
                if let Some(text) = v["message"].as_str() { let _ = self::steer(&id, text); }
            }
        }
    };
    if let Some(task) = TASKS.lock().unwrap().get_mut(&id) { task.delivered = true; }
    match outcome.context("delegated task vanished")? {
        Outcome::Completed(text) => Ok(text),
        Outcome::Failed(e) => bail!("{e}"),
        Outcome::Cancelled => bail!("cancelled by the user"),
    }
}

/// Stops a running task; finished tasks keep their result.
pub fn cancel(id: &str) -> Result<&'static str> {
    let mut tasks = TASKS.lock().unwrap();
    let task = tasks.get_mut(id).with_context(|| format!("unknown task `{id}`"))?;
    if let Some(outcome) = task.done.borrow().as_ref() { return Ok(outcome.status()); }
    task.cancel_requested = true;
    if let Some(cmd) = &task.cmd { let _ = cmd.send(Command::Cancel); }
    Ok("cancel_requested")
}

/// Sends a mid-turn instruction to a running child.
pub fn steer(id: &str, text: &str) -> Result<()> {
    let tasks = TASKS.lock().unwrap();
    let task = tasks.get(id).with_context(|| format!("unknown task `{id}`"))?;
    anyhow::ensure!(task.done.borrow().is_none(), "task `{}` has finished; delegate a new task instead", task.name);
    task.cmd.as_ref().context("task is not running")?.send(Command::Steer(text.to_string()))?;
    Ok(())
}

/// The running task in `parent` called `name`, for `/to NAME` and row buttons.
pub fn find(parent: &str, name: &str) -> Option<String> {
    TASKS.lock().unwrap().iter().find(|(_, t)| t.parent == parent && t.name == name && t.cmd.is_some()).map(|(id, _)| id.clone())
}

/// The child's live events for a viewer; from then on its approvals and questions go to the viewer.
pub fn take_view(id: &str) -> Option<mpsc::UnboundedReceiver<Event>> {
    let mut tasks = TASKS.lock().unwrap();
    let task = tasks.get_mut(id)?;
    let rx = task.view.take()?;
    task.viewed = true;
    Some(rx)
}

/// The settings of a thread running in this process, which may have no session file before its first turn ends.
pub fn running_meta(id: &str) -> Option<Meta> {
    SINKS.lock().unwrap().get(id).map(|s| s.meta.clone())
}

pub fn is_running(id: &str) -> bool {
    TASKS.lock().unwrap().get(id).is_some_and(|t| t.cmd.is_some())
}

/// Whether `parent` has async children still running or results not yet delivered.
pub(crate) fn outstanding(parent: &str) -> bool {
    TASKS.lock().unwrap().values().any(|t| t.parent == parent && t.wake && !t.delivered)
}

/// The finished async results and messages for `parent` not yet delivered, as one message; marks them delivered.
pub fn take_wake(parent: &str) -> Option<String> {
    let mut parts = Vec::new();
    {
        let mut tasks = TASKS.lock().unwrap();
        let mut ids: Vec<String> = tasks.iter().filter(|(_, t)| t.parent == parent && t.wake && !t.delivered && t.done.borrow().is_some()).map(|(id, _)| id.clone()).collect();
        ids.sort();
        if !ids.is_empty() {
            let mut results = Vec::new();
            for id in ids {
                let task = tasks.get_mut(&id).unwrap();
                task.delivered = true;
                let outcome = task.done.borrow().clone().unwrap();
                results.push(format!("### {} ({} · {}, taskId {id}) {}\n{}", task.name, task.backend, task.model, outcome.status(), outcome.text()));
            }
            persist(&tasks);
            parts.push(format!("[GreenCode] Delegated task results:\n\n{}", results.join("\n\n")));
        }
    }
    if let Some(sink) = SINKS.lock().unwrap().get_mut(parent) {
        parts.append(&mut sink.inbox);
    }
    (!parts.is_empty()).then(|| parts.join("\n\n"))
}

fn status_json(id: &str, waited_out: bool) -> Result<Value> {
    let mut tasks = TASKS.lock().unwrap();
    let Some(task) = tasks.get_mut(id) else {
        let (meta, _) = Session::load(id).with_context(|| format!("unknown task `{id}`"))?;
        anyhow::ensure!(meta.parent.is_some(), "`{id}` is not a delegated task");
        return Ok(json!({"taskId": id, "childThreadId": id, "status": "interrupted", "backend": meta.backend, "model": meta.model,
            "summary": last_reply(id), "note": "No record of this task is left; its child thread's last reply is shown."}));
    };
    let outcome = task.done.borrow().clone();
    if outcome.is_some() && !task.delivered {
        task.delivered = true;
        persist(&tasks);
    }
    let task = &tasks[id];
    let mut v = json!({
        "taskId": id, "childThreadId": id, "name": task.name, "backend": task.backend, "model": task.model,
        "status": outcome.as_ref().map_or("running", Outcome::status),
        "summary": outcome.as_ref().map(|o| o.text().to_string()),
    });
    if let Some((path, branch)) = &task.worktree { v["worktree"] = json!({"path": path, "branch": branch}); }
    if outcome.is_none() {
        v["waitTimedOut"] = json!(waited_out);
        if task.wake { v["next"] = json!("Completion wakes this thread. Do not wait on it or poll it. Continue other work, and end your turn when none is left."); }
    }
    Ok(v)
}

// ---- linked machines: other GreenCode instances reached through their `greencode-cli serve` endpoint ----

#[derive(Clone, Serialize, Deserialize)]
struct Environment {
    id: String,
    url: String,
    token: String,
    #[serde(default)]
    label: String,
}

fn environments_path() -> PathBuf {
    crate::home().join("environments.json")
}

fn environments() -> Vec<Environment> {
    std::fs::read(environments_path()).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

fn save_environments(list: &[Environment]) -> Result<()> {
    // Holds bearer tokens, so owner-only.
    crate::remote::write_private(&environments_path(), &serde_json::to_vec_pretty(list)?)
}

fn environment(id: &str) -> Result<Environment> {
    environments().into_iter().find(|e| e.id == id).with_context(|| format!("unknown environment `{id}`; see `environment_links`"))
}

/// One tool of the linked machine's MCP server; its text result parsed as JSON when it is JSON.
async fn remote_call(env: &Environment, tool: &str, args: Value, timeout: Duration) -> Result<Value> {
    let body = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": tool, "arguments": args}});
    let res: Value = reqwest::Client::new().post(format!("{}/mcp", env.url.trim_end_matches('/')))
        .bearer_auth(&env.token).json(&body).timeout(timeout).send().await
        .with_context(|| format!("environment `{}` is not reachable", env.id))?
        .error_for_status().with_context(|| format!("environment `{}` refused the request; link it again if its token changed", env.id))?
        .json().await?;
    if let Some(e) = res.get("error") { bail!("{}", e["message"].as_str().unwrap_or("remote error")); }
    let text = res["result"]["content"][0]["text"].as_str().unwrap_or_default();
    if res["result"]["isError"] == true { bail!("{text}"); }
    Ok(serde_json::from_str(text).unwrap_or_else(|_| Value::String(text.into())))
}

/// A delegated child on a linked machine, followed by long-polling its `wait_thread`; tracked like a local child.
async fn start_remote(parent: &Meta, env: Environment, target: &Value, task: &str, title: Option<&str>) -> Result<String> {
    let backend = target["backend"].as_str().unwrap_or("native").to_string();
    let cwd = target["cwd"].as_str().map(str::to_string).unwrap_or_else(|| parent.cwd.to_string_lossy().into_owned());
    let mut args = json!({"cwd": cwd, "prompt": format!("{CHILD_PREFACE}{task}"), "backend": backend, "wait": false});
    if let Some(model) = target["model"].as_str() { args["model"] = json!(model); }
    let started = remote_call(&env, "start_thread", args, Duration::from_secs(30)).await?;
    let id = started["id"].as_str().context("the linked machine returned no thread id")?.to_string();
    let name = title.map(str::to_string).unwrap_or_else(|| format!("{backend}@{}", env.id));
    let model = target["model"].as_str().unwrap_or("default").to_string();
    let (cmd, mut crx) = mpsc::unbounded_channel::<Command>();
    let (etx, erx) = mpsc::unbounded_channel::<Event>();
    let (remote_id, env_id) = (id.clone(), env.id.clone());
    tokio::spawn(async move {
        loop {
            tokio::select! {
                c = crx.recv() => match c {
                    Some(Command::Cancel) => { let _ = remote_call(&env, "stop_thread", json!({"id": remote_id}), Duration::from_secs(10)).await; }
                    Some(_) => { let _ = etx.send(Event::Notify("a task on a linked machine takes no instructions".into())); }
                    None => break,
                },
                r = remote_call(&env, "wait_thread", json!({"id": remote_id, "timeout_secs": 30}), Duration::from_secs(60)) => {
                    match r {
                        Ok(v) if v["running"] == true => continue,
                        Ok(v) => { let _ = etx.send(Event::Text(v["text"].as_str().unwrap_or_default().into())); }
                        Err(e) => { let _ = etx.send(Event::Error(format!("{e:#}"))); }
                    }
                    let _ = etx.send(Event::Done { waiting_for_delegates: false });
                    break;
                }
            }
        }
    });
    track(id.clone(), cmd, erx, new_task(&parent.id, name, format!("{backend}@{env_id}"), model, true, true, None), None);
    Ok(id)
}

// ---- other threads ----

fn sink_state(id: &str) -> Option<(Option<mpsc::UnboundedSender<Command>>, Option<EventTx>, Arc<watch::Sender<bool>>)> {
    SINKS.lock().unwrap().get(id).map(|s| (s.cmd.upgrade(), s.events.upgrade(), s.busy.clone()))
}

fn thread_state(id: &str) -> &'static str {
    if is_running(id) || sink_state(id).is_some_and(|(_, _, busy)| *busy.borrow()) { "running" } else { "idle" }
}

fn project_of(meta: &Meta) -> PathBuf {
    meta.repo.clone().unwrap_or_else(|| meta.cwd.clone())
}

fn thread_list(caller: &Meta, args: &Value) -> Value {
    let project = args["project"].as_str().map(PathBuf::from).unwrap_or_else(|| project_of(caller));
    let all = args["allProjects"] == true;
    let title = args["titleContains"].as_str().map(str::to_lowercase);
    let statuses: Vec<&str> = args["statuses"].as_array().map(|a| a.iter().filter_map(Value::as_str).collect()).unwrap_or_default();
    let cursor = args["cursor"].as_u64().unwrap_or(0) as usize;
    let limit = args["limit"].as_u64().unwrap_or(20).clamp(1, 100) as usize;
    let mut metas = Session::list();
    metas.sort_by(|a, b| b.created.cmp(&a.created));
    let rows: Vec<Value> = metas.into_iter()
        .filter(|m| all || project_of(m) == project)
        .filter(|m| args["includeSubagents"] == true || m.parent.is_none())
        .filter(|m| title.as_ref().is_none_or(|t| m.title.to_lowercase().contains(t)))
        .map(|m| { let status = thread_state(&m.id); (m, status) })
        .filter(|(_, s)| statuses.is_empty() || statuses.contains(s))
        .skip(cursor).take(limit + 1)
        .map(|(m, status)| json!({"threadId": m.id, "title": m.title, "backend": m.backend, "model": m.model, "cwd": m.cwd,
            "parentThreadId": m.parent, "status": status, "created": m.created}))
        .collect();
    let more = rows.len() > limit;
    json!({"threads": &rows[..rows.len().min(limit)], "nextCursor": more.then_some(cursor + limit)})
}

async fn thread_read(args: &Value, cfg: &Config) -> Result<Value> {
    let id = args["threadId"].as_str().context("missing `threadId`")?;
    let max = args["maxMessages"].as_u64().unwrap_or(20);
    let mut v = crate::mcp_server::call("read_thread", &json!({"id": id, "max_messages": max}), cfg.clone()).await?;
    v["status"] = json!(thread_state(id));
    // Reading a finished child's result delivers it, as `task_status` does.
    let mut tasks = TASKS.lock().unwrap();
    if let Some(task) = tasks.get_mut(id).filter(|t| !t.delivered && t.done.borrow().is_some()) {
        task.delivered = true;
        persist(&tasks);
    }
    Ok(v)
}

async fn thread_send(caller: &Meta, cfg: Arc<Config>, mode: Mode, args: &Value) -> Result<Value> {
    let id = args["threadId"].as_str().context("missing `threadId`")?;
    let text = args["message"].as_str().filter(|t| !t.trim().is_empty()).context("missing `message`")?;
    anyhow::ensure!(id != caller.id, "this is your own thread; just continue");
    let how = args["mode"].as_str().unwrap_or("auto");
    anyhow::ensure!(["auto", "queue", "steer", "restart"].contains(&how), "unknown mode `{how}`: auto, queue, steer or restart");
    let message = format!("[GreenCode] Message from thread \"{}\" ({}):\n{text}", caller.title, caller.id);
    if is_running(id) {
        match how {
            "auto" | "steer" => {
                steer(id, &message)?;
                return Ok(json!({"threadId": id, "delivered": "steer"}));
            }
            "queue" => bail!("this thread runs as a task here; use mode `steer`, or `thread_wait` and then send"),
            _ => {
                cancel(id)?;
                wait(id, Some(Duration::from_secs(10))).await.context("the running turn did not stop in time")?;
            }
        }
    }
    if let Some((Some(cmd), events, busy)) = sink_state(id) {
        let running = *busy.borrow();
        if running && matches!(how, "auto" | "steer") {
            if let Some(tx) = &events { let _ = tx.send(Event::Remote { command: "injected_prompt".into(), data: json!({"text": message, "steer": true}) }); }
            cmd.send(Command::Steer(message))?;
            return Ok(json!({"threadId": id, "delivered": "steer"}));
        }
        if running && how == "restart" { let _ = cmd.send(Command::Cancel); }
        let mut sinks = SINKS.lock().unwrap();
        let sink = sinks.get_mut(id).context("the thread stopped")?;
        sink.inbox.push(message);
        sink.wake.notify_one();
        return Ok(json!({"threadId": id, "delivered": if running && how == "queue" { "queued" } else { "started" }}));
    }
    // Not running anywhere in this process: resume it here, never with broader rights than the caller's.
    anyhow::ensure!(!crate::handoff::is_open(id), "this thread is open in a terminal session");
    let (meta, _) = Session::load(id)?;
    let own = Session::view(id)?.map_or(cfg.mode, |v| v.mode);
    let mode = if rank(own) < rank(mode) { own } else { mode };
    let child = meta.parent.as_deref() == Some(caller.id.as_str());
    let name = meta.title.trim_start_matches("↳ ").split(':').next().unwrap_or_default().to_string();
    let task = new_task(&caller.id, name, meta.backend.clone(), meta.model.clone(), child, child, None);
    launch(meta.clone(), message, cfg, mode, task)?;
    if !child { announce(&caller.id, &meta); }
    Ok(json!({"threadId": id, "taskId": id, "delivered": "started"}))
}

async fn thread_wait(args: &Value) -> Result<Value> {
    let id = args["threadId"].as_str().context("missing `threadId`")?;
    let limit = args["timeoutMs"].as_f64().map_or(WAIT_LIMIT, |ms| Duration::from_millis(ms.max(0.0) as u64).min(WAIT_LIMIT));
    let finished = if is_running(id) {
        wait(id, Some(limit)).await.is_some()
    } else if let Some((_, _, busy)) = sink_state(id) {
        let mut rx = busy.subscribe();
        let idle = tokio::time::timeout(limit, rx.wait_for(|b| !*b)).await.is_ok();
        idle
    } else {
        Session::load(id)?;
        true
    };
    let mut v = json!({"threadId": id, "status": thread_state(id), "lastReply": last_reply(id)});
    if let Some(outcome) = TASKS.lock().unwrap().get(id).and_then(|t| t.done.borrow().clone()) {
        v["lastReply"] = json!(outcome.text());
    }
    if !finished { v["next"] = json!("Still running. End your turn if nothing else is left; a delegated task's completion wakes you."); }
    Ok(v)
}

fn thread_interrupt(caller: &Meta, args: &Value) -> Result<Value> {
    let id = args["threadId"].as_str().context("missing `threadId`")?;
    anyhow::ensure!(id != caller.id, "this is your own thread");
    if let Some(child) = TASKS.lock().unwrap().get(id).filter(|t| t.cmd.is_some()).map(|t| t.child) {
        anyhow::ensure!(!child, "`{id}` is a delegated task; use `task_cancel`");
        cancel(id)?;
        return Ok(json!({"threadId": id, "interrupted": true}));
    }
    if let Some((Some(cmd), _, busy)) = sink_state(id) {
        if *busy.borrow() {
            cmd.send(Command::Cancel)?;
            return Ok(json!({"threadId": id, "interrupted": true}));
        }
    }
    Ok(json!({"threadId": id, "interrupted": false, "note": "no running turn"}))
}

fn thread_update(caller: &Meta, args: &Value) -> Result<Value> {
    let id = args["threadId"].as_str().unwrap_or(&caller.id).to_string();
    anyhow::ensure!(args["action"].as_str().is_none_or(|a| a == "rename"), "only `action:\"rename\"` is supported");
    let title: String = args["title"].as_str().map(str::trim).filter(|t| !t.is_empty()).context("missing `title`")?.chars().take(60).collect();
    let mut view = match Session::view(&id)? {
        Some(v) => v,
        None => serde_json::from_value::<View>(json!({"meta": Session::load(&id)?.0}))?,
    };
    view.meta.title = title.clone();
    Session::save_view(&view)?;
    let target = SINKS.lock().unwrap().get_mut(&id).map(|s| { s.meta.title = title.clone(); s.events.upgrade() });
    let ev = || Event::Remote { command: "thread_renamed".into(), data: json!({"id": id, "title": title}) };
    if let Some(tx) = parent_events(&caller.id) { let _ = tx.send(ev()); }
    if let Some(Some(tx)) = target.filter(|_| id != caller.id) { let _ = tx.send(ev()); }
    Ok(json!({"threadId": id, "title": title}))
}

/// A request the desktop app answers (schedules); other hosts cancel it.
async fn ask_ui(caller: &str, request: Value) -> Result<Value> {
    let tx = parent_events(caller).context("this thread has no UI")?;
    let (reply, rx) = oneshot::channel();
    tx.send(Event::Dialog { request, reply }).map_err(|_| anyhow::anyhow!("this thread has no UI"))?;
    let answer = rx.await.map_err(|_| anyhow::anyhow!("no user can answer here"))?;
    if answer["cancelled"] == true { bail!("scheduled tasks are kept by the GreenCode desktop app, and this thread runs elsewhere"); }
    if let Some(e) = answer["error"].as_str() { bail!("{e}"); }
    Ok(answer)
}

fn tools() -> Vec<Value> {
    let s = json!({"type": "string"});
    let n = json!({"type": "number"});
    let b = json!({"type": "boolean"});
    let env = json!({"type": "string", "description": "A linked machine from `environment_links`; default: this one."});
    let tool = |name: &str, description: &str, properties: Value, required: &[&str]| json!({"name": name, "description": description, "inputSchema": {"type": "object", "properties": properties, "required": required}});
    vec![
        tool("orchestrator_capabilities", "List backends (harnesses), models, modes and linked machines for `delegate_task`.", json!({}), &[]),
        tool("delegate_task", "Start one child agent on any backend/model with only `task` (no parent history). Separate checkout per child: `isolation:'worktree'`. Keep `taskId` for `task_status` and `task_cancel`. No secrets in `task`.", json!({
            "task": s, "title": s,
            "target": {"type": "object", "properties": {
                "backend": {"type": "string", "description": "From `orchestrator_capabilities`; default: yours."},
                "model": {"type": "string", "description": "`provider/model` for native; the backend's model id otherwise."},
                "mode": {"enum": ["inherit", "plan", "ask", "auto-edit", "full"], "description": "Default inherit. Never broader than yours."},
                "environmentId": env, "cwd": {"type": "string", "description": "With `environmentId`: a project directory on that machine."}}},
            "isolation": {"enum": ["shared", "worktree"], "description": "'shared' (default): your checkout. 'worktree': a new git worktree from HEAD; uncommitted edits stay out."},
            "mode": {"enum": ["async", "wait"], "description": "Default 'async': completion wakes this thread. 'wait' only if this turn needs the result."},
            "timeoutMs": {"type": "number", "description": "Mode 'wait' only. At most 45000. A timeout does not cancel the child."}}), &["task"]),
        tool("task_status", "Read one delegated task you started. Do not call it on a running task before its completion wakes you. Reading a finished result marks it delivered.", json!({"taskId": s}), &["taskId"]),
        tool("task_cancel", "Stop one delegated task by `taskId`. A finished task keeps its result.", json!({"taskId": s, "reason": s}), &["taskId"]),
        tool("create_threads", "Create new top-level threads in this checkout, each starting with its `prompt`. Not delegation: nothing wakes you. Own worktree: `worktree:true`.", json!({
            "threads": {"type": "array", "minItems": 1, "maxItems": 20, "items": {"type": "object", "properties": {
                "prompt": s, "title": s, "worktree": b,
                "target": {"type": "object", "properties": {"backend": s, "model": s, "mode": {"enum": ["inherit", "plan", "ask", "auto-edit", "full"]}}}}, "required": ["prompt"]}}}), &["threads"]),
        tool("thread_list", "List threads newest first in this project (or `project`, or `allProjects`). Page with `cursor`.", json!({
            "project": s, "allProjects": b, "titleContains": s, "includeSubagents": b,
            "statuses": {"type": "array", "items": {"enum": ["running", "idle"]}}, "cursor": n, "limit": n, "environmentId": env}), &[]),
        tool("thread_read", "Read any thread's recent messages by id. Reading a child's final result marks it delivered.", json!({"threadId": s, "maxMessages": n, "environmentId": env}), &["threadId"]),
        tool("thread_update", "Rename a thread (`title`). Omit `threadId` for this thread.", json!({"threadId": s, "action": {"enum": ["rename"]}, "title": s}), &["title"]),
        tool("thread_send", "Send a message to another thread. It never runs with broader modes than yours. `mode:'auto'` starts an idle thread or steers a running turn; `queue`: new turn after the current one; `steer`: mid-turn update; `restart`: interrupt and start again. A send to a finished child thread starts it again and its completion wakes you.", json!({
            "threadId": s, "message": s, "mode": {"enum": ["auto", "queue", "steer", "restart"]}, "environmentId": env}), &["threadId", "message"]),
        tool("thread_wait", "Wait for a thread's running turn to finish (at most 45 s). Idle: at once. With `next` in the result, end your turn instead of calling again.", json!({"threadId": s, "timeoutMs": n, "environmentId": env}), &["threadId"]),
        tool("thread_interrupt", "Interrupt a top-level thread's running turn. No effect when idle. For a delegate, use `task_cancel`.", json!({"threadId": s, "environmentId": env}), &["threadId"]),
        tool("request_secret", "Ask the user for one credential in a private card. Open it, don't offer it. Returns a file read only inside the command that needs it, e.g. `tool --token \"$(cat FILE)\"`. Never print it or put it in a task.", json!({"name": {"type": "string", "description": "Letters, digits and _, e.g. GITHUB_TOKEN."}, "reason": s}), &["name", "reason"]),
        tool("schedule_task", "Create or update recurring work that runs in a new thread of this project while the GreenCode app runs. `scheduledTaskId` updates only given fields; `enabled=false` pauses. Report the next run time.", json!({
            "scheduledTaskId": s, "prompt": s, "title": s, "enabled": b,
            "schedule": {"type": "object", "description": "{type:'interval', everyMs} (at least 60000) or {type:'fixed_time', timeOfDay:'HH:MM', weekdays?:[0-6, 0 is Sunday]}.", "properties": {
                "type": {"enum": ["interval", "fixed_time"]}, "everyMs": n, "timeOfDay": s, "weekdays": {"type": "array", "items": n}}},
            "target": {"type": "object", "properties": {"backend": s, "model": s}}}), &[]),
        tool("list_scheduled_tasks", "List scheduled tasks of this project (or `allProjects`). In a scheduled run, do not call this to find your task; do the prompt.", json!({"allProjects": b}), &[]),
        tool("delete_scheduled_task", "Delete one scheduled task by `scheduledTaskId`. To pause instead, use `schedule_task` with `enabled=false`.", json!({"scheduledTaskId": s}), &["scheduledTaskId"]),
        tool("background_wait", "Keep this thread shown as working while background work you started runs (pass its id or a short description once started). Not for work left running (dev server, watcher). `waiting=false` clears. Doesn't stop anything.", json!({"waiting": b, "taskId": s, "description": s}), &[]),
        tool("workflow_runs", "List GreenCode workflow runs of this project, newest first.", json!({"limit": n}), &[]),
        tool("workflow_stop", "Stop a running GreenCode workflow run by `runId`.", json!({"runId": s, "reason": s}), &["runId"]),
        tool("environment_links", "List linked machines (other GreenCode instances) and whether each answers now, with their projects. Pass one's `environmentId` to `delegate_task` (`target.environmentId`) or thread tools.", json!({}), &[]),
        tool("environment_link", "Link another machine running `greencode-cli serve`. Pass `url` the user gave; the token is asked from the user in a private card, never in chat.", json!({"url": s, "environmentId": s, "hint": s}), &["url"]),
        tool("environment_unlink", "Forget a linked machine.", json!({"environmentId": s}), &["environmentId"]),
    ]
}

async fn call(parent: &str, name: &str, args: &Value) -> Result<Value> {
    let (cfg, perms, meta) = {
        let sinks = SINKS.lock().unwrap();
        let sink = sinks.get(parent).context("the parent thread is no longer running in GreenCode")?;
        (sink.cfg.clone(), sink.perms.clone(), sink.meta.clone())
    };
    // Thread tools aimed at a linked machine go to its own MCP server.
    if let Some(env) = args["environmentId"].as_str().filter(|_| name.starts_with("thread_")) {
        let env = environment(env)?;
        let id = args["threadId"].clone();
        let (tool, remote_args) = match name {
            "thread_list" => ("list_threads", json!({})),
            "thread_read" => ("read_thread", json!({"id": id, "max_messages": args["maxMessages"].as_u64().unwrap_or(20)})),
            "thread_send" => ("send_message", json!({"id": id, "text": args["message"], "wait": false})),
            "thread_wait" => ("wait_thread", json!({"id": id, "timeout_secs": args["timeoutMs"].as_u64().map_or(45, |ms| (ms / 1000).min(45))})),
            "thread_interrupt" => ("stop_thread", json!({"id": id})),
            _ => bail!("`{name}` does not take `environmentId`"),
        };
        return remote_call(&env, tool, remote_args, Duration::from_secs(60)).await;
    }
    // Some clients send nested objects as JSON text.
    let object = |v: &Value| match v { Value::String(s) => serde_json::from_str(s).unwrap_or(Value::Null), v => v.clone() };
    match name {
        "orchestrator_capabilities" => Ok(capabilities(&cfg, &meta)),
        "delegate_task" => {
            anyhow::ensure!(meta.parent.is_none(), "a delegated child cannot delegate further; do the work yourself");
            let target = object(&args["target"]);
            let task = args["task"].as_str().context("missing `task`")?;
            let id = if let Some(env) = target["environmentId"].as_str() {
                start_remote(&meta, environment(env)?, &target, task, args["title"].as_str()).await?
            } else {
                let req = Request {
                    backend: target["backend"].as_str().unwrap_or(&meta.backend).to_string(),
                    model: target["model"].as_str().map(str::to_string),
                    task: task.to_string(),
                    name: args["title"].as_str().map(str::to_string),
                    isolated: match args["isolation"].as_str() { None | Some("shared") => false, Some("worktree") => true, Some(o) => bail!("unknown isolation `{o}`") },
                    mode: Some(child_mode(perms.mode(), target["mode"].as_str())?),
                    top_level: false,
                };
                start(&meta, cfg, perms.mode(), req, true, None)?
            };
            if args["mode"].as_str() == Some("wait") {
                let limit = args["timeoutMs"].as_f64().map_or(WAIT_LIMIT, |ms| Duration::from_millis(ms.max(0.0) as u64).min(WAIT_LIMIT));
                let finished = wait(&id, Some(limit)).await.is_some();
                return status_json(&id, !finished);
            }
            status_json(&id, false)
        }
        "task_status" => status_json(args["taskId"].as_str().context("missing `taskId`")?, false),
        "task_cancel" => {
            let id = args["taskId"].as_str().context("missing `taskId`")?;
            anyhow::ensure!(TASKS.lock().unwrap().get(id).is_some_and(|t| t.parent == parent), "task `{id}` was not started by this thread");
            Ok(json!({"taskId": id, "status": cancel(id)?}))
        }
        "create_threads" => {
            let threads = object(&args["threads"]);
            let threads = threads.as_array().filter(|t| (1..=20).contains(&t.len())).context("`threads` takes 1 to 20 items")?;
            let mut created = Vec::new();
            for t in threads {
                let target = object(&t["target"]);
                let req = Request {
                    backend: target["backend"].as_str().unwrap_or(&meta.backend).to_string(),
                    model: target["model"].as_str().map(str::to_string),
                    task: t["prompt"].as_str().context("each thread needs a `prompt`")?.to_string(),
                    name: t["title"].as_str().map(str::to_string),
                    isolated: t["worktree"] == true,
                    mode: Some(child_mode(perms.mode(), target["mode"].as_str())?),
                    top_level: true,
                };
                match start(&meta, cfg.clone(), perms.mode(), req, false, None) {
                    Ok(id) => created.push(json!({"threadId": id, "status": "running"})),
                    Err(e) => created.push(json!({"status": "failed", "error": format!("{e:#}")})),
                }
            }
            Ok(json!({"threads": created}))
        }
        "thread_list" => Ok(thread_list(&meta, args)),
        "thread_read" => thread_read(args, &cfg).await,
        "thread_send" => thread_send(&meta, cfg, perms.mode(), args).await,
        "thread_wait" => thread_wait(args).await,
        "thread_interrupt" => thread_interrupt(&meta, args),
        "thread_update" => thread_update(&meta, args),
        "request_secret" => {
            let tx = parent_events(parent).context("this thread has no UI to ask")?;
            let name = args["name"].as_str().context("missing `name`")?;
            match crate::secrets::ask(&tx, name, args["reason"].as_str().unwrap_or_default()).await? {
                Some(path) => Ok(json!({"status": "provided", "delivery": "file", "file": path,
                    "note": "Read it only inside the command that needs it, e.g. \"$(cat FILE)\". Never print it, write it elsewhere or pass it to a task."})),
                None => Ok(json!({"status": "declined", "note": "The user chose not to; don't ask again."})),
            }
        }
        "schedule_task" | "list_scheduled_tasks" | "delete_scheduled_task" => {
            let action = match name { "schedule_task" => "save", "list_scheduled_tasks" => "list", _ => "delete" };
            let mut request = args.clone();
            if !request.is_object() { request = json!({}); }
            request["method"] = json!("schedule");
            request["action"] = json!(action);
            request["schedule"] = object(&args["schedule"]);
            request["target"] = object(&args["target"]);
            request["project"] = json!(project_of(&meta));
            ask_ui(parent, request).await
        }
        "background_wait" => {
            let waiting = args["waiting"].as_bool().unwrap_or(true);
            let label = args["description"].as_str().or(args["taskId"].as_str()).unwrap_or("background work");
            let tx = parent_events(parent).context("this thread has no UI")?;
            let _ = tx.send(Event::Remote { command: "background_wait".into(), data: json!({"waiting": waiting, "label": label}) });
            Ok(json!({"waiting": waiting}))
        }
        "workflow_runs" => {
            let limit = args["limit"].as_u64().unwrap_or(20).clamp(1, 100) as usize;
            let runs = crate::workflow::Runtime::new(cfg, project_of(&meta)).runs();
            Ok(json!(runs.iter().take(limit).map(|r| json!({"runId": r.id, "workflow": r.workflow, "status": r.status, "step": r.current})).collect::<Vec<_>>()))
        }
        "workflow_stop" => {
            let id = args["runId"].as_str().context("missing `runId`")?;
            let run = crate::workflow::Runtime::new(cfg, project_of(&meta)).cancel(id, args["reason"].as_str())?;
            Ok(json!({"runId": run.id, "status": run.status}))
        }
        "environment_links" => {
            let mut out = Vec::new();
            for env in environments() {
                let projects = remote_call(&env, "list_projects", json!({}), Duration::from_secs(3)).await;
                out.push(json!({"environmentId": env.id, "url": env.url, "label": env.label, "reachable": projects.is_ok(),
                    "projects": projects.as_ref().ok(), "error": projects.as_ref().err().map(|e| format!("{e:#}"))}));
            }
            Ok(json!({"environments": out}))
        }
        "environment_link" => {
            let url = args["url"].as_str().context("pass `url`: the address `greencode-cli serve` shows on that machine; ask the user for it")?;
            let parsed = url::Url::parse(url).context("`url` must be an http(s) address")?;
            anyhow::ensure!(matches!(parsed.scheme(), "http" | "https"), "`url` must be an http(s) address");
            let id = args["environmentId"].as_str().map(str::to_string)
                .unwrap_or_else(|| parsed.host_str().unwrap_or("remote").chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect());
            if let Ok(env) = environment(&id) {
                if remote_call(&env, "list_projects", json!({}), Duration::from_secs(3)).await.is_ok() { return Ok(json!({"status": "linked", "environmentId": id})); }
            }
            let tx = parent_events(parent).context("this thread has no UI to ask")?;
            let hint = args["hint"].as_str().unwrap_or_default();
            let message = format!("{url} の GreenCode リモートトークン（そのマシンの greencode-cli serve が表示）を入力してください。{hint}");
            let Some(token) = crate::secrets::ask_value(&tx, &format!("マシンをリンク: {id}"), &message).await? else {
                return Ok(json!({"status": "declined"}));
            };
            let env = Environment { id: id.clone(), url: url.trim_end_matches('/').to_string(), token, label: hint.to_string() };
            let projects = remote_call(&env, "list_projects", json!({}), Duration::from_secs(10)).await?;
            let mut list = environments();
            list.retain(|e| e.id != id);
            list.push(env);
            save_environments(&list)?;
            Ok(json!({"status": "linked", "environmentId": id, "projects": projects}))
        }
        "environment_unlink" => {
            let id = args["environmentId"].as_str().context("missing `environmentId`")?;
            let mut list = environments();
            let before = list.len();
            list.retain(|e| e.id != id);
            save_environments(&list)?;
            Ok(json!({"environmentId": id, "unlinked": list.len() < before}))
        }
        "list_projects" | "search_threads" => crate::mcp_server::call(name, args, (*cfg).clone()).await,
        other => bail!("unknown tool `{other}`"),
    }
}

/// One JSON-RPC request from the parent `parent`'s agent.
pub async fn handle(parent: &str, req: Value) -> Option<Value> {
    let id = req.get("id").cloned()?;
    let result = match req["method"].as_str().unwrap_or("") {
        "initialize" => json!({"protocolVersion": req["params"]["protocolVersion"].as_str().unwrap_or("2025-06-18"), "capabilities": {"tools": {}}, "serverInfo": {"name": "GreenCode", "version": env!("CARGO_PKG_VERSION")}, "instructions": INSTRUCTIONS}),
        "ping" => json!({}),
        "tools/list" => {
            let mut all = tools();
            all.extend(crate::mcp_server::tools().as_array().into_iter().flatten().filter(|t| matches!(t["name"].as_str(), Some("list_projects" | "search_threads"))).cloned());
            json!({"tools": all})
        }
        "tools/call" => match call(parent, req["params"]["name"].as_str().unwrap_or(""), &req["params"]["arguments"]).await {
            Ok(value) => json!({"content": [{"type": "text", "text": value.to_string()}]}),
            Err(e) => json!({"content": [{"type": "text", "text": format!("{e:#}")}], "isError": true}),
        },
        _ => return Some(json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32601, "message": "Method not found"}})),
    };
    Some(json!({"jsonrpc": "2.0", "id": id, "result": result}))
}

/// The loopback MCP URL, started on first use, and a bearer token that acts as `parent`.
pub async fn endpoint(parent: &str) -> Result<(String, String)> {
    let url = ENDPOINT.get_or_try_init(|| async {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let url = format!("http://{}/mcp", listener.local_addr()?);
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                tokio::spawn(serve(stream));
            }
        });
        anyhow::Ok(url)
    }).await?.clone();
    let token = crate::remote::random_hex()?;
    TOKENS.lock().unwrap().insert(token.clone(), parent.to_string());
    Ok((url, token))
}

async fn serve(mut stream: tokio::net::TcpStream) -> Result<()> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    loop {
        let mut data = Vec::new();
        let header_end = loop {
            let mut buf = [0u8; 8192];
            let n = stream.read(&mut buf).await?;
            if n == 0 { return Ok(()); }
            data.extend_from_slice(&buf[..n]);
            if let Some(i) = data.windows(4).position(|w| w == b"\r\n\r\n") { break i + 4; }
            anyhow::ensure!(data.len() < 64 * 1024, "headers too large");
        };
        let head = String::from_utf8_lossy(&data[..header_end]).to_string();
        let mut lines = head.lines();
        let mut first = lines.next().unwrap_or("").split_whitespace();
        let (method, path) = (first.next().unwrap_or(""), first.next().unwrap_or(""));
        let header = |name: &str| head.lines().skip(1).filter_map(|l| l.split_once(':')).find(|(k, _)| k.trim().eq_ignore_ascii_case(name)).map(|(_, v)| v.trim().to_string());
        let len: usize = header("content-length").and_then(|v| v.parse().ok()).unwrap_or(0);
        anyhow::ensure!(len <= 4 * 1024 * 1024, "body too large");
        while data.len() < header_end + len {
            let mut buf = vec![0u8; header_end + len - data.len()];
            let n = stream.read(&mut buf).await?;
            if n == 0 { return Ok(()); }
            data.extend_from_slice(&buf[..n]);
        }
        let parent = header("authorization").and_then(|v| v.strip_prefix("Bearer ").map(str::to_string)).and_then(|t| TOKENS.lock().unwrap().get(&t).cloned());
        let (status, body) = match (method, path, parent) {
            (_, _, None) => ("401 Unauthorized", r#"{"error":"unauthorized"}"#.to_string()),
            ("POST", "/mcp", Some(parent)) => match serde_json::from_slice::<Value>(&data[header_end..header_end + len]) {
                Ok(req) => match handle(&parent, req).await { Some(v) => ("200 OK", v.to_string()), None => ("202 Accepted", String::new()) },
                Err(_) => ("200 OK", json!({"jsonrpc":"2.0","id":null,"error":{"code":-32700,"message":"Parse error"}}).to_string()),
            },
            // No server-initiated stream (Streamable HTTP allows refusing GET).
            _ => ("405 Method Not Allowed", String::new()),
        };
        let response = format!("HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}", body.len());
        stream.write_all(response.as_bytes()).await?;
    }
}

/// `greencode-cli mcp-relay`: stdio MCP for agents without HTTP MCP, forwarding each line to the endpoint.
pub async fn relay() -> Result<()> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    let url = std::env::var("GREENCODE_DELEGATE_URL").context("GREENCODE_DELEGATE_URL is not set")?;
    let token = std::env::var("GREENCODE_DELEGATE_TOKEN").context("GREENCODE_DELEGATE_TOKEN is not set")?;
    let client = reqwest::Client::new();
    let out = Arc::new(tokio::sync::Mutex::new(tokio::io::stdout()));
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    let mut tasks = tokio::task::JoinSet::new();
    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() { continue; }
        let (client, url, token, out) = (client.clone(), url.clone(), token.clone(), out.clone());
        // Concurrent, so `task_cancel` is not stuck behind a waiting `delegate_task`.
        tasks.spawn(async move {
            let id = serde_json::from_str::<Value>(&line).ok().and_then(|v| v.get("id").cloned());
            let body = match client.post(&url).bearer_auth(&token).header("content-type", "application/json").body(line).send().await {
                Ok(r) => r.text().await.unwrap_or_default(),
                Err(e) => id.map(|id| json!({"jsonrpc":"2.0","id":id,"error":{"code":-32603,"message":format!("GreenCode is not reachable: {e}")}}).to_string()).unwrap_or_default(),
            };
            if !body.trim().is_empty() {
                let mut out = out.lock().await;
                let _ = out.write_all(format!("{}\n", body.trim()).as_bytes()).await;
                let _ = out.flush().await;
            }
        });
    }
    while tasks.join_next().await.is_some() {}
    Ok(())
}

/// `greencode-cli` beside this executable, for the stdio relay.
fn relay_program() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let name = if cfg!(windows) { "greencode-cli.exe" } else { "greencode-cli" };
    if exe.file_name().is_some_and(|n| n == name) { return Some(exe); }
    let sibling = exe.parent()?.join(name);
    sibling.is_file().then_some(sibling)
}

/// The GreenCode MCP server entry for an ACP `session/new`, or `None` for children and unregistered threads.
pub async fn acp_server(meta: &Meta, init: &Value) -> Option<Value> {
    if meta.parent.is_some() || !SINKS.lock().unwrap().contains_key(&meta.id) { return None; }
    let (url, token) = endpoint(&meta.id).await.ok()?;
    if init["agentCapabilities"]["mcpCapabilities"]["http"] == true {
        return Some(json!({"type": "http", "name": "greencode", "url": url, "headers": [{"name": "Authorization", "value": format!("Bearer {token}")}]}));
    }
    let program = relay_program()?;
    Some(json!({"name": "greencode", "command": program, "args": ["mcp-relay"],
        "env": [{"name": "GREENCODE_DELEGATE_URL", "value": url}, {"name": "GREENCODE_DELEGATE_TOKEN", "value": token}]}))
}

/// A Pi/OMP extension that registers the delegation tools against this process's endpoint.
pub async fn pi_extension(meta: &Meta) -> Option<String> {
    if meta.parent.is_some() || !SINKS.lock().unwrap().contains_key(&meta.id) { return None; }
    let (url, token) = endpoint(&meta.id).await.ok()?;
    let specs = Value::Array(tools());
    Some(format!(r#"const URL = {url};
const TOKEN = {token};
const SPECS = {specs};
const GUIDE = {guide};
let seq = 0;
export default function (pi: any) {{
  SPECS.forEach((spec: any, i: number) => pi.registerTool({{
    name: spec.name,
    label: spec.name,
    description: spec.description,
    parameters: spec.inputSchema,
    ...(i === 0 ? {{ promptGuidelines: GUIDE.split("\n- ").slice(1) }} : {{}}),
    async execute(_id: string, params: any, signal: any) {{
      const res = await fetch(URL, {{ method: "POST", signal, headers: {{ "content-type": "application/json", authorization: `Bearer ${{TOKEN}}` }},
        body: JSON.stringify({{ jsonrpc: "2.0", id: ++seq, method: "tools/call", params: {{ name: spec.name, arguments: params ?? {{}} }} }}) }});
      const body: any = await res.json();
      const text = body.error ? body.error.message : (body.result?.content ?? []).map((c: any) => c.text).join("\n");
      if (body.error || body.result?.isError) throw new Error(text);
      return {{ content: [{{ type: "text", text }}], details: {{}} }};
    }},
  }}));
}}
"#, url = json!(url), token = json!(token), guide = json!(INSTRUCTIONS)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wake_batches_finished_results_once() {
        let (_tx, done) = watch::channel(Some(Outcome::Completed("result A".into())));
        let (_tx2, running) = watch::channel(None);
        let mut tasks = TASKS.lock().unwrap();
        let base = |name: &str, done: watch::Receiver<Option<Outcome>>| Task {
            parent: "wake-parent".into(), name: name.into(), backend: "codex".into(), model: "gpt".into(), child: true, cmd: None, done,
            wake: true, delivered: false, view: None, viewed: false, cancel_requested: false, worktree: None, events: None,
        };
        tasks.insert("t-a".into(), base("a", done));
        tasks.insert("t-b".into(), base("b", running));
        drop(tasks);
        let text = take_wake("wake-parent").expect("finished result");
        assert!(text.contains("result A") && !text.contains("### b"), "{text}");
        assert!(take_wake("wake-parent").is_none(), "delivered results must not wake twice");
        assert!(outstanding("wake-parent"), "the running async task is still outstanding");
        TASKS.lock().unwrap().retain(|_, t| t.parent != "wake-parent");
    }
}
