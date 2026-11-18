//! GreenCode core: agent loop, providers, tools, sessions. No GUI dependencies.

pub mod acp;
pub mod agent;
pub mod agents;
pub mod antigravity;
pub mod auth;
pub mod checkpoint;
pub mod config;
pub mod context;
pub mod handoff;
pub mod hermes;
pub mod import;
pub mod jsonrpc;
pub mod launchd;
pub mod lua;
pub mod mcp_server;
pub mod permission;
pub mod pi;
pub mod policy;
pub mod provider;
pub mod remote;
pub mod update;
pub mod registry;
pub mod rules;
pub mod session;
pub mod snap;
pub mod subagents;
pub mod suggest;
pub mod skills;
pub mod tools;
pub mod usage;
pub mod webhook;
pub mod voice;
pub mod worktree;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{future::Future, path::PathBuf, pin::Pin, sync::Arc};
use tokio::sync::{mpsc, oneshot};

pub type BoxFut<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;
pub type EventTx = mpsc::UnboundedSender<Event>;

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    User,
    Assistant,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: Vec<Block>,
}

impl Message {
    pub fn user(text: impl Into<String>) -> Self {
        Message { role: Role::User, content: vec![Block::Text { text: text.into() }] }
    }
}

fn is_false(b: &bool) -> bool {
    !*b
}

/// Anthropic wire format. Blocks we don't interpret (thinking, fallback, ...) are
/// kept verbatim in `Other` because the API requires them replayed unchanged.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Block {
    Text {
        text: String,
    },
    ToolUse {
        id: String,
        name: String,
        input: Value,
    },
    ToolResult {
        tool_use_id: String,
        content: String,
        #[serde(default, skip_serializing_if = "is_false")]
        is_error: bool,
    },
    #[serde(untagged)]
    Other(Value),
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ToolDef {
    pub name: String,
    pub description: String,
    pub schema: Value,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Usage {
    /// Uncached input tokens.
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
}

impl Usage {
    pub fn context(&self) -> u64 {
        self.input + self.cache_read + self.cache_write
    }

    pub fn add(&mut self, other: Usage) {
        self.input += other.input;
        self.output += other.output;
        self.cache_read += other.cache_read;
        self.cache_write += other.cache_write;
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Decision {
    Allow,
    Always,
    Deny,
}

#[derive(Debug)]
pub enum Event {
    Text(String),
    /// The model's visible reasoning for the reply it just gave.
    Thinking(String),
    /// An image block (a `computer` screenshot) that was added to the conversation.
    Image(Value),
    ToolStart { id: String, name: String, input: Value },
    ToolEnd { id: String, output: String, is_error: bool },
    Permission { tool: String, summary: String, reply: oneshot::Sender<Decision> },
    Usage(Usage),
    /// Usage of an auxiliary call (compaction, advisor, sub-agent); counted, but not the thread's context size.
    UsageAux(Usage),
    Notify(String),
    Error(String),
    /// Native provider state, command results, and progress. Not model context.
    Remote { command: String, data: Value },
    Dialog { request: Value, reply: oneshot::Sender<Value> },
    /// The turn is over (success, error or cancel).
    Done,
}

pub enum Command {
    Prompt(String),
    /// Apply backend controls before sending this prompt.
    PromptWithOptions(String, Vec<Value>),
    Cancel,
    Model(String),
    SubagentModel(Option<String>),
    /// Replaces the thread's config, e.g. after a provider was added.
    Config(Arc<config::Config>),
    Steer(String),
    /// Pi/OMP RPC command; never passed to the language model.
    Remote(Value),
    /// Persists the backend's session handle and stops its process; answers `Ok` only once that process is dead.
    /// The thread then refuses prompts so nothing writes the session while a terminal UI owns it; reopen it with a
    /// new `spawn` afterwards. Refused (`Err`, nothing stopped) while a turn is running.
    Handoff(oneshot::Sender<Result<(), String>>),
}

pub trait Backend: Send {
    fn prompt<'a>(&'a mut self, text: String, tx: &'a EventTx) -> BoxFut<'a, anyhow::Result<()>>;
    /// Wait for one unsolicited backend event; processing happens separately so cancellation cannot lose it.
    fn idle<'a>(&'a mut self) -> BoxFut<'a, anyhow::Result<()>> {
        Box::pin(std::future::pending())
    }
    fn handle_idle<'a>(&'a mut self, _tx: &'a EventTx) -> BoxFut<'a, anyhow::Result<()>> {
        Box::pin(async { Ok(()) })
    }
    fn cancel<'a>(&'a mut self, _tx: &'a EventTx) -> BoxFut<'a, anyhow::Result<()>> {
        Box::pin(async { Ok(()) })
    }
    fn live(&self) -> Option<mpsc::UnboundedSender<Value>> { None }
    fn set_subagent_model(&mut self, _model: Option<String>) -> anyhow::Result<()> {
        anyhow::bail!("sub-agent model selection is managed by this external backend")
    }
    fn control<'a>(&'a mut self, _command: Value, _tx: &'a EventTx) -> BoxFut<'a, anyhow::Result<()>> {
        Box::pin(async { anyhow::bail!("this backend does not support native RPC commands") })
    }
    /// Persists whatever a later process needs to resume this session, stops the backend's process and returns only
    /// once it is dead. Backends that cannot guarantee that refuse.
    fn handoff<'a>(&'a mut self, _tx: &'a EventTx) -> BoxFut<'a, anyhow::Result<()>> {
        Box::pin(async { anyhow::bail!("this backend does not support session handoff") })
    }
}

/// `~/.greencode`, or `$GREENCODE_HOME`.
pub fn home() -> PathBuf {
    std::env::var_os("GREENCODE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| user_home().join(".greencode"))
}

pub fn user_home() -> PathBuf {
    std::env::home_dir().unwrap_or_else(|| PathBuf::from("."))
}

/// Merge a per-model OpenCode reasoning override into its inline configuration.
pub fn opencode_effort_env(model: &str, effort: Option<&str>, existing: Option<&str>) -> Option<String> {
    let effort = effort?;
    let (provider, id) = model.split_once('/').filter(|(p, m)| !p.is_empty() && !m.is_empty())?;
    let mut config = match existing { Some(text) => serde_json::from_str::<Value>(text).ok()?, None => json!({}) };
    if !config.is_object() { return None; }
    fn merge(target: &mut Value, patch: Value) {
        match (target, patch) {
            (Value::Object(target), Value::Object(patch)) => {
                for (key, value) in patch { merge(target.entry(key).or_insert(Value::Null), value); }
            }
            (target, patch) => *target = patch,
        }
    }
    merge(&mut config, json!({"provider":{(provider):{"models":{(id):{"options":{"reasoningEffort":effort}}}}}}));
    Some(config.to_string())
}

/// Makes OpenCode ask for shell and edit calls so GreenCode's own permission mode (and its approval prompt) decides.
/// OpenCode allows both by default, which would let `ask` mode run anything unprompted. A `permission` block the user already set is kept.
pub fn opencode_permission_env(existing: Option<&str>) -> Option<String> {
    let mut config = match existing { Some(text) => serde_json::from_str::<Value>(text).ok()?, None => json!({}) };
    let object = config.as_object_mut()?;
    object.entry("permission").or_insert_with(|| json!({"bash": "ask", "edit": "ask"}));
    Some(config.to_string())
}

async fn make_backend(
    cfg: &Arc<config::Config>,
    meta: &session::Meta,
    perms: &Arc<permission::Perms>,
) -> anyhow::Result<Box<dyn Backend>> {
    let bname = antigravity::canonical_backend(&meta.backend);
    if let Some(policy) = &cfg.policy {
        policy.check_mode(perms.mode())?;
        anyhow::ensure!(policy.backend_allowed(bname), "管理ポリシーで制限されています（{}）", policy.organization);
    }
    if bname == "native" {
        return Ok(Box::new(agent::Agent::new(cfg.clone(), meta, perms.clone())?));
    }
    let b = cfg
        .backends
        .iter()
        .find(|b| b.name == bname)
        .ok_or_else(|| anyhow::anyhow!("unknown backend `{}`", meta.backend))?;
    if bname == antigravity::BACKEND && !antigravity::is_runtime_installed(b) {
        anyhow::bail!("Google Antigravity ランタイムが未インストールです。/login antigravity または 設定 → 接続（CLI では greencode-cli auth login antigravity）を実行してください");
    }
    if bname.starts_with("hermes:") && config::executable("hermes").is_none() {
        anyhow::bail!("Hermes is not installed; install Hermes with ACP support and run `hermes acp --check`");
    }
    match b.protocol {
        config::Protocol::Acp => {
            let opencode = b.name == "opencode" || b.command.first().and_then(|c| std::path::Path::new(c).file_name()).is_some_and(|name| name == "opencode");
            // The override targets one `provider/model`; until the thread's model is known the backend starts without it.
            let process_env = std::env::var("OPENCODE_CONFIG_CONTENT").ok();
            let overridden = opencode.then(|| {
                let existing = b.env.get("OPENCODE_CONFIG_CONTENT").cloned().or(process_env);
                let with_effort = opencode_effort_env(&meta.model, cfg.opencode_effort.as_deref(), existing.as_deref()).or(existing);
                opencode_permission_env(with_effort.as_deref())
            }).flatten();
            let mut env = if bname == antigravity::BACKEND {
                antigravity::runtime_env(&b.env, true)
            } else {
                b.env.clone()
            };
            if let Some(value) = overridden { env.insert("OPENCODE_CONFIG_CONTENT".into(), value); }
            Ok(Box::new(acp::Acp::start(&b.command, &env, meta, perms.clone()).await?))
        }
        protocol => Ok(Box::new(pi::Pi::start(&b.command, protocol, meta, perms.clone()).await?)),
    }
}

const HANDED_OFF: &str = "this thread was handed off to a terminal session; reopen the thread once it ends";

/// Only Pi/OMP threads can be stopped for a terminal session; decided before any process is started.
fn handoff_supported(cfg: &config::Config, meta: &session::Meta) -> bool {
    cfg.backends.iter().any(|b| b.name == meta.backend && b.protocol != config::Protocol::Acp)
}

/// Starts an idle backend so its session handle exists, then has it persist that handle and stop.
async fn handoff_backend(
    backend: &mut Option<Box<dyn Backend>>,
    cfg: &Arc<config::Config>,
    meta: &session::Meta,
    perms: &Arc<permission::Perms>,
    tx: &EventTx,
) -> anyhow::Result<()> {
    if backend.is_none() {
        anyhow::ensure!(handoff_supported(cfg, meta), "this backend does not support session handoff");
        *backend = Some(make_backend(cfg, meta, perms).await?);
    }
    backend.as_mut().unwrap().handoff(tx).await
}

/// Model ids carried by an external backend's state event (`acp_state`, or Pi's `get_available_models`).
pub fn remote_models(command: &str, data: &Value) -> Option<Vec<String>> {
    fn values(list: &Value, key: &str, out: &mut Vec<String>) {
        for item in list.as_array().into_iter().flatten() {
            if let Some(value) = item[key].as_str() { out.push(value.to_string()); }
            if item["options"].is_array() { values(&item["options"], key, out); }
        }
    }
    let strings = |list: &Value, key: &str| { let mut out = Vec::new(); values(list, key, &mut out); out };
    match command {
        // Agents that expose a model config option list their models there.
        "acp_state" => Some(match data["configOptions"].as_array().and_then(|v| v.iter().find(|o| o["category"] == "model")) {
            Some(option) => strings(&option["options"], "value"),
            None => strings(&data["models"]["availableModels"], "modelId"),
        }),
        "get_available_models" => Some(data["models"].as_array().into_iter().flatten()
            .filter_map(|m| Some(format!("{}/{}", m["provider"].as_str()?, m["id"].as_str()?))).collect()),
        _ => None,
    }
}

fn backend_models_file() -> PathBuf {
    home().join("backend-models.json")
}

fn backend_options_file() -> PathBuf {
    home().join("backend-options.json")
}

/// Cached reasoning and fast-mode options reported by external backends.
pub fn cached_backend_options() -> std::collections::BTreeMap<String, Vec<Value>> {
    let mut cache: std::collections::BTreeMap<String, Vec<Value>> = read_backend_models(&backend_options_file()).and_then(|v| serde_json::from_value(v).ok()).unwrap_or_default();
    for options in cache.values_mut() { *options = cacheable_picker_options(options); }
    cache
}

/// Store the latest relevant ACP option definitions, without session-specific values.
pub fn cache_backend_options(backend: &str, options: &[Value]) -> std::io::Result<()> {
    let path = backend_options_file();
    let parent = path.parent().ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "missing cache directory"))?;
    std::fs::create_dir_all(parent)?;
    let lock = std::fs::File::create(parent.join("backend-options.lock"))?;
    lock.lock()?;
    let mut cache = match std::fs::read(&path) {
        Ok(bytes) => serde_json::from_slice::<std::collections::BTreeMap<String, Vec<Value>>>(&bytes)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Default::default(),
        Err(e) => return Err(e),
    };
    for existing in cache.values_mut() { *existing = cacheable_picker_options(existing); }
    cache.insert(backend.to_string(), cacheable_picker_options(options));
    let tmp = parent.join(format!("backend-options.{}.tmp", std::process::id()));
    std::fs::write(&tmp, serde_json::to_vec(&cache)?)?;
    std::fs::rename(tmp, path)
}

/// Options that can be controlled from the model picker.
pub fn picker_backend_option(option: &Value) -> bool {
    matches!(option["category"].as_str(), Some("reasoning" | "effort" | "thought" | "thinking" | "thought_level" | "thinking_level" | "reasoning_effort"))
        || matches!(option["id"].as_str(), Some("reasoning_effort" | "effort" | "thinking" | "thinking_level" | "reasoning"))
        || (option["category"] == "model_config" && matches!(option["id"].as_str(), Some("fast" | "fast-mode")))
}

/// Picker option definitions are shared between sessions, but their current values are not.
pub fn cacheable_picker_options(options: &[Value]) -> Vec<Value> {
    options.iter().filter(|o| picker_backend_option(o)).cloned().map(|mut option| {
        if let Some(object) = option.as_object_mut() { object.remove("currentValue"); }
        option
    }).collect()
}

/// Models each external backend reported the last time it was asked.
pub fn cached_backend_models() -> std::collections::BTreeMap<String, Vec<String>> {
    read_backend_models(&backend_models_file()).map(|v| v.as_object().unwrap().iter()
        .filter_map(|(k, v)| serde_json::from_value(v.clone()).ok().map(|models| (k.clone(), models))).collect()).unwrap_or_default()
}

/// OpenCode models belong to the project that reported them; legacy unscoped entries are ignored.
pub fn cached_project_backend_models(project: &std::path::Path) -> std::collections::BTreeMap<String, Vec<String>> {
    project_backend_models(&backend_models_file(), project)
}

fn project_backend_models(path: &std::path::Path, project: &std::path::Path) -> std::collections::BTreeMap<String, Vec<String>> {
    let Some(cache) = read_backend_models(path) else { return Default::default() };
    let mut models: std::collections::BTreeMap<String, Vec<String>> = cache.as_object().unwrap().iter()
        .filter_map(|(k, v)| serde_json::from_value(v.clone()).ok().map(|models| (k.clone(), models))).collect();
    models.remove("opencode");
    if let Some(value) = project.to_str().and_then(|p| cache.get("_projects")?.get(p)?.get("opencode").cloned()) {
        if let Ok(ids) = serde_json::from_value(value) { models.insert("opencode".into(), ids); }
    }
    models
}

fn read_backend_models(path: &std::path::Path) -> Option<Value> {
    let value: Value = serde_json::from_slice(&std::fs::read(path).ok()?).ok()?;
    value.is_object().then_some(value)
}

fn save_backend_models(path: &std::path::Path, backend: &str, project: &std::path::Path, models: &[String]) -> std::io::Result<()> {
    use std::io::{Error, ErrorKind};
    let parent = path.parent().ok_or_else(|| Error::new(ErrorKind::InvalidInput, "missing cache directory"))?;
    std::fs::create_dir_all(parent)?;
    let lock = std::fs::File::create(parent.join("backend-models.lock"))?;
    lock.lock()?;
    let mut cache = match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice::<Value>(&bytes).ok().filter(Value::is_object)
            .ok_or_else(|| Error::new(ErrorKind::InvalidData, "invalid backend model cache"))?,
        Err(e) if e.kind() == ErrorKind::NotFound => json!({}),
        Err(e) => return Err(e),
    };
    if backend == "opencode" {
        let project = project.to_str().ok_or_else(|| Error::new(ErrorKind::InvalidInput, "non-UTF-8 project path"))?;
        let root = cache.as_object_mut().unwrap().entry("_projects").or_insert_with(|| json!({})).as_object_mut()
            .ok_or_else(|| Error::new(ErrorKind::InvalidData, "invalid project cache"))?;
        let entry = root.entry(project).or_insert_with(|| json!({})).as_object_mut()
            .ok_or_else(|| Error::new(ErrorKind::InvalidData, "invalid project cache entry"))?;
        entry.insert(backend.into(), json!(models));
    } else {
        cache.as_object_mut().unwrap().insert(backend.into(), json!(models));
    }
    let tmp = parent.join(format!("backend-models.{}.tmp", std::process::id()));
    std::fs::write(&tmp, serde_json::to_vec(&cache)?)?;
    std::fs::rename(tmp, path)
}

pub(crate) fn cache_models_for_backend(backend: &str, cwd: &std::path::Path, models: &[String]) {
    let _ = save_backend_models(&backend_models_file(), backend, cwd, models);
}

pub(crate) fn remove_backend_cache_entry(home_dir: &std::path::Path, backend: &str) {
    let models_path = home_dir.join("backend-models.json");
    if models_path.exists() {
        if let Ok(lock) = std::fs::File::create(home_dir.join("backend-models.lock")) {
            let _ = lock.lock();
            if let Ok(bytes) = std::fs::read(&models_path) {
                if let Ok(mut cache) = serde_json::from_slice::<Value>(&bytes) {
                    if cache.as_object_mut().and_then(|o| o.remove(backend)).is_some() {
                        let tmp = home_dir.join(format!("backend-models.{}.tmp", std::process::id()));
                        if let Ok(out) = serde_json::to_vec(&cache) {
                            if std::fs::write(&tmp, out).is_ok() {
                                let _ = std::fs::rename(tmp, &models_path);
                            }
                        }
                    }
                }
            }
        }
    }
    let options_path = home_dir.join("backend-options.json");
    if options_path.exists() {
        if let Ok(lock) = std::fs::File::create(home_dir.join("backend-options.lock")) {
            let _ = lock.lock();
            if let Ok(bytes) = std::fs::read(&options_path) {
                if let Ok(mut cache) = serde_json::from_slice::<Value>(&bytes) {
                    if cache.as_object_mut().and_then(|o| o.remove(backend)).is_some() {
                        let tmp = home_dir.join(format!("backend-options.{}.tmp", std::process::id()));
                        if let Ok(out) = serde_json::to_vec(&cache) {
                            if std::fs::write(&tmp, out).is_ok() {
                                let _ = std::fs::rename(tmp, &options_path);
                            }
                        }
                    }
                }
            }
        }
    }
}

/// Asks an external backend (Pi, OpenCode, any ACP agent) which models it offers and caches the answer.
/// It starts the agent on a throwaway thread and sends no prompt.
pub async fn backend_models(cfg: Arc<config::Config>, backend: &str, cwd: PathBuf) -> Vec<String> {
    let backend = antigravity::canonical_backend(backend);
    let meta = session::Meta::new(cwd.clone(), "default".into(), backend.into());
    let perms = Arc::new(permission::Perms::new(permission::Mode::Ask));
    let (tx, mut rx) = spawn(&tokio::runtime::Handle::current(), cfg, meta.clone(), perms);
    let _ = tx.send(Command::Remote(serde_json::json!({"type": "get_state"})));
    let mut models = vec![];
    let _ = tokio::time::timeout(std::time::Duration::from_secs(60), async {
        while let Some(ev) = rx.recv().await {
            match ev {
                Event::Remote { command, data } => {
                    if let Some(found) = remote_models(&command, &data).filter(|m| !m.is_empty()) { models = found; }
                    if command == "acp_state" {
                        if let Some(options) = data["configOptions"].as_array() {
                            let relevant: Vec<_> = options.iter().filter(|o| picker_backend_option(o)).cloned().collect();
                            let _ = cache_backend_options(backend, &relevant);
                        }
                    }
                    if command == "get_available_thinking_levels" {
                        if let Some(levels) = data["levels"].as_array() {
                            let options: Vec<_> = levels.iter().filter_map(|v| v.as_str().map(|level| json!({"value":level}))).collect();
                            let _ = cache_backend_options(backend, &[json!({"id":"thinking_level","category":"thought_level","name":"Reasoning","options":options})]);
                        }
                    }
                }
                Event::Permission { reply, .. } => { let _ = reply.send(Decision::Deny); }
                Event::Dialog { reply, .. } => { let _ = reply.send(serde_json::json!({"cancelled": true})); }
                Event::Done => break,
                _ => {}
            }
        }
    })
    .await;
    drop(tx); // ends the thread and its agent process
    session::Session::delete(&meta.id);
    let _ = std::fs::remove_dir_all(home().join("provider-sessions").join(&meta.id));
    if !models.is_empty() {
        let _ = save_backend_models(&backend_models_file(), backend, &cwd, &models);
    }
    models
}

/// Runs one thread. The backend is created on the first prompt, so idle threads cost nothing.
pub fn spawn(
    rt: &tokio::runtime::Handle,
    mut cfg: Arc<config::Config>,
    mut meta: session::Meta,
    perms: Arc<permission::Perms>,
) -> (mpsc::UnboundedSender<Command>, mpsc::UnboundedReceiver<Event>) {
    let (ctx, mut crx) = mpsc::unbounded_channel();
    let (tx, erx) = mpsc::unbounded_channel();
    rt.spawn(async move {
        let mut backend: Option<Box<dyn Backend>> = None;
        let mut queue = std::collections::VecDeque::new();
        let mut handed_off = false;
        loop {
            let cmd = match queue.pop_front() {
                Some(c) => c,
                None => {
                    if let Some(b) = backend.as_mut() {
                        enum Wait { Command(Option<Command>), Event(anyhow::Result<()>) }
                        let next = tokio::select! {
                            c = crx.recv() => Wait::Command(c),
                            result = b.idle() => Wait::Event(result),
                        };
                        match next {
                            Wait::Command(Some(c)) => c,
                            Wait::Command(None) => break,
                            Wait::Event(Ok(())) => {
                                if let Err(e) = b.handle_idle(&tx).await {
                                    let _ = tx.send(Event::Error(format!("{e:#}")));
                                    backend = None;
                                }
                                continue;
                            }
                            Wait::Event(Err(e)) => {
                                let _ = tx.send(Event::Error(format!("{e:#}")));
                                backend = None;
                                continue;
                            }
                        }
                    } else {
                        match crx.recv().await { Some(c) => c, None => break }
                    }
                }
            };
            match cmd {
                Command::Cancel => {}
                Command::Config(new) => {
                    let opencode_effort = cfg.opencode_effort.clone();
                    cfg = new;
                    if opencode_effort.is_some() { Arc::make_mut(&mut cfg).opencode_effort = opencode_effort; }
                    // Rebuilt from the session file on the next prompt, like a model switch.
                    if meta.backend == "native" { backend = None; }
                }
                Command::SubagentModel(model) => {
                    if meta.backend != "native" {
                        let _ = tx.send(Event::Error("sub-agent model selection is managed by this external backend".into()));
                        continue;
                    }
                    if let Some(b) = backend.as_mut() {
                        if let Err(e) = b.set_subagent_model(model.clone()) {
                            let _ = tx.send(Event::Error(format!("{e:#}")));
                            continue;
                        }
                    }
                    meta.subagent_model = model;
                }
                // Native history lives in the session file, so the backend is simply rebuilt.
                Command::Model(m) => {
                    if handed_off {
                        let _ = tx.send(Event::Error(HANDED_OFF.into()));
                        continue;
                    }
                    if meta.backend != "native" && backend.is_none() {
                        match make_backend(&cfg, &meta, &perms).await {
                            Ok(b) => backend = Some(b),
                            Err(e) => { let _ = tx.send(Event::Error(format!("{e:#}"))); continue; }
                        }
                    }
                    if let Some(b) = backend.as_mut().filter(|_| meta.backend != "native") {
                        let result = b.control(serde_json::json!({"type":"select_model","model":m}), &tx).await;
                        match result {
                            Ok(()) => meta.model = m,
                            Err(e) => { let _ = tx.send(Event::Error(format!("{e:#}"))); }
                        }
                    } else {
                        meta.model = m;
                        backend = None;
                    }
                }
                Command::Handoff(reply) => {
                    let result = if handed_off { Ok(()) } else { handoff_backend(&mut backend, &cfg, &meta, &perms, &tx).await };
                    if result.is_ok() {
                        backend = None;
                        handed_off = true;
                    }
                    let _ = reply.send(result.map_err(|e| format!("{e:#}")));
                }
                Command::Prompt(_) | Command::PromptWithOptions(_, _) | Command::Steer(_) | Command::Remote(_) => {
                    if handed_off {
                        let _ = tx.send(Event::Error(HANDED_OFF.into()));
                        let _ = tx.send(Event::Done);
                        continue;
                    }
                    if let Some(policy) = &cfg.policy {
                        if let Err(e) = policy.check_mode(perms.mode()) {
                            let _ = tx.send(Event::Error(format!("{e:#}")));
                            let _ = tx.send(Event::Done);
                            continue;
                        }
                    }
                    if backend.is_none() {
                        match make_backend(&cfg, &meta, &perms).await {
                            Ok(b) => backend = Some(b),
                            Err(e) => {
                                let _ = tx.send(Event::Error(format!("{e:#}")));
                                let _ = tx.send(Event::Done);
                                continue;
                            }
                        }
                    }
                    let b = backend.as_mut().unwrap();
                    if let Command::PromptWithOptions(_, options) = &cmd {
                        let mut error = None;
                        for option in options {
                            if let Err(e) = b.control(option.clone(), &tx).await {
                                error = Some(e);
                                break;
                            }
                        }
                        if let Some(e) = error {
                            let _ = tx.send(Event::Error(format!("{e:#}")));
                            let _ = tx.send(Event::Done);
                            continue;
                        }
                    }
                    let live = if matches!(&cmd,Command::Prompt(_) | Command::PromptWithOptions(_, _) | Command::Steer(_)) { b.live() } else { None };
                    let (cancelled, failed) = {
                        let fut = match cmd {
                            Command::Prompt(text) | Command::PromptWithOptions(text, _) | Command::Steer(text) => b.prompt(text,&tx),
                            Command::Remote(value) => b.control(value,&tx),
                            _ => unreachable!(),
                        };
                        tokio::pin!(fut);
                        loop {
                            tokio::select! {
                                r = &mut fut => {
                                    match r {
                                        Ok(()) => break (false,false),
                                        Err(e) => { let _ = tx.send(Event::Error(format!("{e:#}"))); break (false,true); }
                                    }
                                }
                                c = crx.recv() => match c {
                                    Some(Command::Cancel) | None => break (true,false),
                                    Some(Command::Steer(message)) if live.is_some() => {
                                        let _ = live.as_ref().unwrap().send(serde_json::json!({"type": "steer", "message": message}));
                                    }
                                    Some(Command::Remote(value)) if live.is_some() => { let _ = live.as_ref().unwrap().send(value); }
                                    Some(Command::Handoff(reply)) => {
                                        let _ = reply.send(Err("a turn is still running; wait for it to finish or cancel it before handing off".into()));
                                    }
                                    Some(c) => queue.push_back(c),
                                },
                            }
                        }
                    };
                    if cancelled {
                        if let Err(e) = b.cancel(&tx).await {
                            let _ = tx.send(Event::Error(format!("{e:#}")));
                            backend = None;
                        }
                    }
                    if failed { backend = None; }
                    let _ = tx.send(Event::Done);
                }
            }
        }
    });
    (ctx, erx)
}

pub fn event_json(event: &Event) -> Value {
    match event {
        Event::Text(text) => json!({"type":"text","text":text}),
        Event::Thinking(text) => json!({"type":"thinking","text":text}),
        Event::Image(_) => json!({"type":"image"}),
        Event::ToolStart { id, name, input } => json!({"type":"tool_start","id":id,"name":name,"input":input}),
        Event::ToolEnd { id, output, is_error } => json!({"type":"tool_end","id":id,"output":output,"is_error":is_error}),
        Event::Permission { tool, summary, .. } => json!({"type":"permission","tool":tool,"summary":summary}),
        Event::Usage(u) => json!({"type":"usage","input":u.input,"output":u.output,"cache_read":u.cache_read,"cache_write":u.cache_write}),
        Event::UsageAux(u) => json!({"type":"usage_aux","input":u.input,"output":u.output,"cache_read":u.cache_read,"cache_write":u.cache_write}),
        Event::Remote { command, data } => json!({"type":"remote","command":command,"data":data}),
        Event::Dialog { request, .. } => json!({"type":"dialog","request":request}),
        Event::Notify(text) => json!({"type":"notify","text":text}),
        Event::Error(text) => json!({"type":"error","text":text}),
        Event::Done => json!({"type":"done"}),
    }
}

#[cfg(test)]
mod backend_model_tests {
    use super::*;

    #[test]
    fn project_cache_migrates_without_leaking_or_losing_legacy_data() {
        let dir = std::env::temp_dir().join(format!("greencode-model-cache-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("backend-models.json");
        std::fs::write(&path, br#"{"opencode":["legacy"],"pi":["shared"],"future":{"keep":true}}"#).unwrap();
        let a = std::path::Path::new("/project/a");
        let b = std::path::Path::new("/project/b");
        assert!(!project_backend_models(&path, a).contains_key("opencode"));
        save_backend_models(&path, "opencode", a, &["a/model".into()]).unwrap();
        save_backend_models(&path, "opencode", b, &["b/model".into()]).unwrap();
        assert_eq!(project_backend_models(&path, a)["opencode"], ["a/model"]);
        assert_eq!(project_backend_models(&path, b)["opencode"], ["b/model"]);
        assert_eq!(project_backend_models(&path, a)["pi"], ["shared"]);
        let saved = read_backend_models(&path).unwrap();
        assert_eq!(saved["opencode"], json!(["legacy"]));
        assert_eq!(saved["future"]["keep"], true);
        std::fs::write(&path, b"{invalid").unwrap();
        assert!(save_backend_models(&path, "opencode", a, &["x".into()]).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"{invalid");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn grouped_acp_model_options_are_flattened() {
        let state = json!({"configOptions":[{"category":"model","options":[
            {"value":"plain"}, {"name":"group","options":[{"value":"nested"}, {"options":[{"value":"deep"}]}]}
        ]}]});
        assert_eq!(remote_models("acp_state", &state).unwrap(), ["plain", "nested", "deep"]);
    }
}
