mod advanced;
mod agents;
mod attach;
mod checkpoint;
mod cite;
mod files;
mod find;
mod git;
mod goal;
mod links;
mod login;
#[cfg(target_os = "macos")]
mod media;
mod mentions;
mod models;
mod nav;
mod providers;
mod prs;
mod queue;
mod repos;
mod registry;
mod schedule;
mod secrets;
mod settle;
mod snapshot;
mod storage;
mod suggest;
mod terminal;
mod textgen;
mod themes;
mod usage;
mod voice;
mod webhooks;
mod welcome;
mod workflows;

use crate::thread_view::{self, Item, Pal, ThemeId};
use cite::CiteForm;
use files::FilesPanel;
use gpui_kit::base::FocusTrapElement as _;
use gpui_kit::component::{
    input::{Input, InputEvent, InputState, Textarea, TextareaState},
    ActiveTheme, Theme,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use greencode_core::{
    config::{Config, ProviderCfg},
    hermes, import,
    permission::{Mode, Perms},
    session::{AutoSettle, LinkedPr, Meta, Session, View},
    skills, worktree, Command, Decision, Event, Usage,
    update::{self, Channel},
};
use login::{method_label, AccountForm, DeviceForm, DeviceKind, KeyForm};
use models::ModelPicker;
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashSet, VecDeque};
#[cfg(unix)]
use std::os::{
    fd::{FromRawFd, OwnedFd},
    unix::process::CommandExt,
};
use std::{ops::Range, path::PathBuf, sync::Arc};
use terminal::{Term, TerminalSet};
use themes::Choice;
use tokio::io::AsyncWriteExt;
use tokio::sync::{mpsc, oneshot};

#[derive(Clone, Copy, PartialEq)]
enum Status {
    Idle,
    Running,
    /// Blocked on an approval.
    Waiting,
}

struct SubRow {
    name: String,
    agent: String,
    state: String,
    detail: String,
    /// A delegated child thread (another harness), which can be opened while it runs.
    task: Option<String>,
}

/// An agent's command and event channels, as `greencode_core::spawn` returns them.
type AgentLink = (
    mpsc::UnboundedSender<Command>,
    mpsc::UnboundedReceiver<Event>,
);

/// Starts a thread's agent. Tests swap it for one that lives on the test's thread: the test scheduler rejects
/// events woken from tokio's worker threads.
type AgentSpawner = fn(
    &mut Context<Workspace>,
    &tokio::runtime::Handle,
    Arc<Config>,
    Meta,
    Arc<Perms>,
) -> AgentLink;

fn spawn_agent(
    _: &mut Context<Workspace>,
    rt: &tokio::runtime::Handle,
    cfg: Arc<Config>,
    meta: Meta,
    perms: Arc<Perms>,
) -> AgentLink {
    greencode_core::spawn(rt, cfg, meta, perms)
}

struct Thread {
    meta: Meta,
    /// `None` until the first prompt: an idle thread has no backend.
    cmd: Option<mpsc::UnboundedSender<Command>>,
    items: Vec<Item>,
    /// Only the visible thread and running ones keep their items in memory.
    loaded: bool,
    /// A transcript exists on disk; unlike an unsent draft, this thread is real history.
    has_session: bool,
    status: Status,
    pending: Option<(SharedString, SharedString, oneshot::Sender<Decision>)>,
    approvals: VecDeque<(SharedString, SharedString, oneshot::Sender<Decision>)>,
    usage: Usage,
    /// All usage this thread has reported, including compaction, advisor and sub-agent calls.
    spent: Usage,
    /// Dollars spent since the app started, for models with a `prices` entry.
    cost: f64,
    models: Vec<String>,
    commands: Vec<String>,
    thinking: String,
    thinking_levels: Vec<String>,
    pending_thinking: Option<String>,
    opencode_effort: Option<String>,
    /// The ACP agent advertises mid-turn steering (`_session/steering`).
    steering: bool,
    config_options: Vec<Value>,
    pending_options: Vec<(String, Value)>,
    dialog: Option<(Value, oneshot::Sender<Value>)>,
    /// Dialogs that arrived while `dialog` was open, answered in order.
    dialogs: VecDeque<(Value, oneshot::Sender<Value>)>,
    queue: VecDeque<String>,
    subs: Vec<SubRow>,
    /// Background work the agent said it is waiting on (`background_wait`); the sidebar keeps showing it as working.
    bg_wait: Option<String>,
    /// Last prompt sent, for the retry chip after a failed turn.
    last_prompt: Option<String>,
    /// Unix time a provider's usage limit resets, from the failed turn; 0 when none.
    reset_at: u64,
    queue_held: bool,
    draft: String,
    /// Images pasted or dropped into the composer; shown as previews and sent as paths with the next prompt.
    images: Vec<PathBuf>,
    /// When the running turn was sent, for the live row's timer.
    started: Option<std::time::Instant>,
    perms: Arc<Perms>,
    pinned: bool,
    archived: bool,
    /// Unix time until which the thread is hidden from the sidebar.
    snoozed: u64,
    auto_settle: AutoSettle,
    linked_pr: Option<LinkedPr>,
    linked_pr_manual: Option<LinkedPr>,
    last_user_at: u64,
    /// `/goal`: what "done" means, and how many automatic continuations it has used.
    goal: Option<(String, u8)>,
    /// `/goal pause`: automatic continuations are off until `/goal resume`.
    goal_paused: bool,
    /// "Suggest next prompt": the predicted next message, shown in the empty composer.
    suggestion: Option<String>,
    /// The agent was started only to learn its slash commands; no prompt was sent, so the thread still counts as unstarted.
    warm: bool,
    /// Silent control commands in flight whose `Done` must not end a prompt.
    warmups: u8,
    /// A first `/command` held until discovery finishes: (text, generation, backend it was typed for).
    deferred: Option<(String, u64, String)>,
    /// Commands an RPC backend advertises as GreenCode's own mapping of terminal built-ins (`source: greencode`), not extensions.
    mapped: Vec<String>,
    /// Extension `setStatus` / `setWidget` output of an RPC backend, shown above the composer.
    statuses: BTreeMap<String, String>,
    widgets: BTreeMap<String, Vec<String>>,
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// `HH:MM` in local time, for a limit reset shown to the user.
fn clock_local(at: u64) -> String {
    let t = (at as i64 + schedule::utc_offset()).rem_euclid(86400);
    format!("{:02}:{:02}", t / 3600, t % 3600 / 60)
}

/// Project files of a directory that is not a git repo: hidden entries, `node_modules` and `target` skipped; at most 5,000 files, 8 levels deep.
fn walk_files(dir: &std::path::Path, prefix: &str, depth: usize, out: &mut Vec<String>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') || out.len() >= 5000 {
            continue;
        }
        let Ok(kind) = e.file_type() else { continue };
        if kind.is_dir() {
            if depth < 8 && !matches!(name.as_str(), "node_modules" | "target") {
                walk_files(&e.path(), &format!("{prefix}{name}/"), depth + 1, out);
            }
        } else if kind.is_file() {
            out.push(format!("{prefix}{name}"));
        }
    }
}

fn term_style(line: &str) -> String {
    let mut style = String::new();
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        if c != '\x1b' || chars.next() != Some('[') {
            continue;
        }
        let mut seq = String::from("\x1b[");
        for c in chars.by_ref() {
            seq.push(c);
            if ('@'..='~').contains(&c) {
                if c == 'm' {
                    let params = &seq[2..seq.len() - 1];
                    if params.is_empty() || params.split(';').next() == Some("0") {
                        style.clear();
                    }
                    if !params.is_empty() && params != "0" {
                        style.push_str(&seq);
                    }
                }
                break;
            }
        }
    }
    style
}

/// Appends PTY output to scrollback, retaining SGR colors and handling carriage returns and backspace.
fn term_feed(lines: &mut Vec<SharedString>, open: &mut bool, chunk: &str) {
    let mut line = if *open {
        lines.pop().map(|l| l.to_string()).unwrap_or_default()
    } else {
        String::new()
    };
    let mut chars = chunk.chars().peekable();
    *open = true;
    while let Some(c) = chars.next() {
        match c {
            '\n' => {
                let style = term_style(&line);
                lines.push(std::mem::replace(&mut line, style).into());
            }
            '\r' => {
                if chars.peek() != Some(&'\n') {
                    line = term_style(&line);
                }
            }
            '\u{8}' => {
                line.pop();
            }
            '\u{1b}' => match chars.next() {
                // CSI: parameters, then one final byte in @..~.
                Some('[') => {
                    let mut seq = String::from("\x1b[");
                    for c in chars.by_ref() {
                        seq.push(c);
                        if ('@'..='~').contains(&c) {
                            if c == 'm' {
                                line.push_str(&seq);
                            }
                            break;
                        }
                    }
                }
                // OSC (window title and the like): up to BEL or ESC \.
                Some(']') => {
                    while let Some(c) = chars.next() {
                        if c == '\u{7}' {
                            break;
                        }
                        if c == '\u{1b}' {
                            chars.next();
                            break;
                        }
                    }
                }
                // Charset selection takes one more byte; other two-byte escapes are complete.
                Some('(' | ')') => {
                    chars.next();
                }
                _ => {}
            },
            c if c.is_control() && c != '\t' => {}
            c => line.push(c),
        }
    }
    lines.push(line.into());
    terminal::trim_scrollback(lines);
}

/// Size of the terminal drawer's PTY and screen, in character cells.
const TERM_SIZE: (u16, u16) = (14, 110);

/// The bytes a terminal program expects for a key, while a full-screen program has the terminal.
fn term_key(k: &Keystroke) -> Option<Vec<u8>> {
    if k.modifiers.platform {
        return None;
    }
    let bytes: &[u8] = match k.key.as_str() {
        "enter" => b"\r",
        "escape" => b"\x1b",
        "backspace" => b"\x7f",
        "tab" => b"\t",
        "space" => b" ",
        "up" => b"\x1b[A",
        "down" => b"\x1b[B",
        "right" => b"\x1b[C",
        "left" => b"\x1b[D",
        "home" => b"\x1b[H",
        "end" => b"\x1b[F",
        "pageup" => b"\x1b[5~",
        "pagedown" => b"\x1b[6~",
        "delete" => b"\x1b[3~",
        key if k.modifiers.control => {
            return (key.len() == 1 && key.as_bytes()[0].is_ascii_lowercase())
                .then(|| vec![key.as_bytes()[0] & 0x1f])
        }
        key => {
            return k
                .key_char
                .clone()
                .or_else(|| (key.chars().count() == 1).then(|| key.to_string()))
                .map(String::into_bytes)
        }
    };
    Some(bytes.to_vec())
}

fn new_screen() -> vt100::Parser<terminal::Replies> {
    let cells = |n: u16| std::num::NonZero::new(n).expect("terminal size");
    vt100::Parser::new_with_callbacks(
        cells(TERM_SIZE.0),
        cells(TERM_SIZE.1),
        0,
        terminal::Replies::default(),
    )
}

fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Decodes `chunk` after `pending`, keeping an incomplete UTF-8 suffix for the next read.
fn take_utf8(pending: &mut Vec<u8>, chunk: &[u8]) -> String {
    pending.extend_from_slice(chunk);
    let mut out = String::new();
    loop {
        match std::str::from_utf8(pending) {
            Ok(text) => {
                out.push_str(text);
                pending.clear();
                break;
            }
            Err(e) => {
                let valid = e.valid_up_to();
                out.push_str(std::str::from_utf8(&pending[..valid]).expect("valid prefix"));
                match e.error_len() {
                    Some(len) => {
                        out.push('\u{FFFD}');
                        pending.drain(..valid + len);
                    }
                    None => {
                        pending.drain(..valid);
                        break;
                    }
                }
            }
        }
    }
    out
}

fn terminal_output(
    mut stdout: impl std::io::Read,
    tx: mpsc::UnboundedSender<String>,
) {
    let mut buf = [0u8; 8192];
    let mut pending = Vec::new();
    while let Ok(n) = stdout.read(&mut buf) {
        if n == 0 {
            break;
        }
        let text = take_utf8(&mut pending, &buf[..n]);
        if !text.is_empty() && tx.send(text).is_err() {
            return;
        }
    }
    if !pending.is_empty() {
        let _ = tx.send(String::from_utf8_lossy(&pending).into_owned());
    }
}

#[cfg(unix)]
fn terminal_command(
    cwd: &std::path::Path,
    launch: Option<&greencode_core::handoff::Launch>,
) -> std::io::Result<(
    tokio::process::Command,
    Arc<OwnedFd>,
    std::fs::File,
    tokio::fs::File,
)> {
    let mut master = -1;
    let mut slave = -1;
    let mut size = libc::winsize {
        ws_row: TERM_SIZE.0,
        ws_col: TERM_SIZE.1,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    if unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut size,
        )
    } == -1
    {
        return Err(std::io::Error::last_os_error());
    }
    let master = unsafe { std::fs::File::from_raw_fd(master) };
    let slave = unsafe { std::fs::File::from_raw_fd(slave) };
    let reader = master.try_clone()?;
    let writer = tokio::fs::File::from_std(master.try_clone()?);
    let mut command = if let Some(launch) = launch {
        let program = launch.argv.first().ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "empty handoff command")
        })?;
        let mut command = std::process::Command::new(program);
        command
            .args(&launch.argv[1..])
            .envs(&launch.env)
            .env("PATH", &launch.path_env);
        command
    } else {
        let default = if cfg!(target_os = "macos") {
            Some("/bin/zsh".to_string())
        } else {
            std::env::var("SHELL").ok()
        };
        let shell = terminal::resolve_shell(
            std::env::var("GREENCODE_TERMINAL_SHELL").ok().as_deref(),
            default.as_deref(),
        );
        let mut command = std::process::Command::new(&shell);
        if cfg!(target_os = "macos") {
            command.arg("-l");
        }
        if let Some(features) =
            terminal::fish_features(&shell, std::env::var("fish_features").ok().as_deref())
        {
            command.env("fish_features", features);
        }
        command.arg("-i");
        command
    };
    command
        .current_dir(cwd)
        .env("TERM", "xterm-256color")
        .stdin(slave.try_clone()?)
        .stdout(slave.try_clone()?)
        .stderr(slave);
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 || libc::ioctl(0, libc::TIOCSCTTY as _, 0) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut command = tokio::process::Command::from(command);
    command.kill_on_drop(true);
    Ok((command, Arc::new(master.into()), reader, writer))
}

impl Thread {
    fn new(meta: Meta, loaded: bool, mode: Mode) -> Thread {
        let has_session = Session::open(&meta.id).is_some();
        let view = Session::view(&meta.id).unwrap_or_default().unwrap_or(View {
            meta,
            draft: String::new(),
            queue: vec![],
            pinned: false,
            archived: false,
            mode,
            snoozed: 0,
            auto_settle: AutoSettle::Follow,
            linked_pr: None,
            linked_pr_manual: None,
            last_user_at: 0,
        });
        Thread {
            meta: view.meta,
            cmd: None,
            items: vec![],
            loaded,
            has_session,
            status: Status::Idle,
            pending: None,
            approvals: VecDeque::new(),
            usage: Usage::default(),
            spent: Usage::default(),
            cost: 0.,
            models: vec![],
            commands: vec![],
            thinking: String::new(),
            thinking_levels: vec![],
            pending_thinking: None,
            opencode_effort: None,
            steering: false,
            config_options: vec![],
            pending_options: vec![],
            dialog: None,
            dialogs: VecDeque::new(),
            queue: view.queue.into(),
            subs: vec![],
            bg_wait: None,
            last_prompt: None,
            reset_at: 0,
            queue_held: true,
            images: Vec::new(),
            started: None,
            draft: view.draft,
            perms: Arc::new(Perms::new(view.mode)),
            pinned: view.pinned,
            archived: view.archived,
            snoozed: view.snoozed,
            auto_settle: view.auto_settle,
            linked_pr: view.linked_pr,
            linked_pr_manual: view.linked_pr_manual,
            last_user_at: view.last_user_at,
            goal: None,
            goal_paused: false,
            suggestion: None,
            warm: false,
            warmups: 0,
            deferred: None,
            mapped: vec![],
            statuses: BTreeMap::new(),
            widgets: BTreeMap::new(),
        }
    }

    fn prompt_command(&mut self, text: String) -> Command {
        let mut controls: Vec<Value> = std::mem::take(&mut self.pending_options)
            .into_iter()
            .map(|(id, value)| json!({"type":"set_config_option","configId":id,"value":value}))
            .collect();
        if let Some(level) = self.pending_thinking.take() {
            controls.push(json!({"type":"set_thinking_level","level":level}));
        }
        if controls.is_empty() {
            Command::Prompt(text)
        } else {
            Command::PromptWithOptions(text, controls)
        }
    }

    /// An agent exists for a real conversation, not just for command discovery.
    fn live(&self) -> bool {
        self.cmd.is_some() && !self.warm
    }

    /// No message was sent and nothing lasting was set: the thread is only an empty composer.
    /// Such threads are not history; the sidebar hides them and startup only restores the remembered one.
    fn unstarted(&self) -> bool {
        self.meta.title.is_empty()
            && !self.has_session
            && self.items.is_empty()
            && self.queue.is_empty()
            && self.status == Status::Idle
            && self.bg_wait.is_none()
            && !self.pinned
            && !self.archived
            && self.snoozed == 0
            && self.auto_settle == AutoSettle::Follow
            && self.linked_pr.is_none()
            && self.linked_pr_manual.is_none()
            && self.last_user_at == 0
            && self.goal.is_none()
    }

    fn save(&self) -> Result<(), String> {
        if self.meta.title.is_empty()
            && self.draft.is_empty()
            && self.queue.is_empty()
            && !self.pinned
            && !self.archived
            && self.snoozed == 0
            && self.auto_settle == AutoSettle::Follow
            && self.linked_pr.is_none()
            && self.linked_pr_manual.is_none()
            && self.last_user_at == 0
            && Session::view(&self.meta.id)
                .map_err(|e| format!("スレッドを保存できません: {e:#}"))?
                .is_none()
        {
            return Ok(());
        }
        Session::save_view(&View {
            meta: self.meta.clone(),
            draft: self.draft.clone(),
            queue: self.queue.iter().cloned().collect(),
            pinned: self.pinned,
            archived: self.archived,
            mode: self.perms.mode(),
            snoozed: self.snoozed,
            auto_settle: self.auto_settle,
            linked_pr: self.linked_pr.clone(),
            linked_pr_manual: self.linked_pr_manual.clone(),
            last_user_at: self.last_user_at,
        })
        .map_err(|e| format!("スレッドを保存できません: {e:#}"))
    }

    fn project(&self) -> PathBuf {
        self.meta
            .repo
            .clone()
            .unwrap_or_else(|| self.meta.cwd.clone())
    }
}

#[derive(Clone, Debug, PartialEq)]
struct GitEntry {
    index: char,
    worktree: char,
    path: String,
    original: Option<String>,
}

fn parse_git_status(text: &str) -> Vec<GitEntry> {
    let mut records = text.split('\0');
    let mut entries = Vec::new();
    while let Some(line) = records.next() {
        let bytes = line.as_bytes();
        if bytes.len() < 4 || bytes[2] != b' ' {
            continue;
        }
        let original =
            if bytes[0] == b'R' || bytes[0] == b'C' || bytes[1] == b'R' || bytes[1] == b'C' {
                records.next().map(str::to_string)
            } else {
                None
            };
        entries.push(GitEntry {
            index: bytes[0] as char,
            worktree: bytes[1] as char,
            path: line[3..].to_string(),
            original,
        });
    }
    entries
}

#[derive(Clone)]
enum Act {
    SecretAdd,
    SecretRemove(usize),
    SecretTest,
    /// 詳細設定: edit the picked `config.toml` key, or reset it to the default.
    SettingPick(String),
    SettingSave,
    SettingReset,
    RuntimeCategory(usize),
    /// 動作設定: turn a native tool on (`true`) or off.
    RuntimeTool(&'static str, bool),
    /// Sets a `config.toml` key to this text, or resets it with `None`.
    SettingWrite(String, Option<String>),
    NewThread,
    NewWorktree,
    OpenProject,
    ImportHermes(hermes::Profile),
    ImportHermesFolder,
    ToggleDiff,
    ToggleFiles,
    PullRequests,
    ToggleGit,
    Delete,
    /// Deletes a thread picked in the sidebar, without switching to it first.
    DeleteThread(String),
    Mode(Mode),
    Model(String),
    SubagentModel(Option<String>),
    Backend(String),
    Thread(String),
    Rpc(Value),
    Pin,
    Archive,
    AutoSettle(AutoSettle),
    LinkPr(Option<u64>),
    ToggleArchived,
    ResumeQueue,
    MoveQueue(usize, bool),
    EditQueue(usize),
    Steer,
    AddProvider,
    RegistryOpen,
    RegistryInstall(Box<greencode_core::registry::Entry>),
    RegistryRemove(String),
    LoginProvider(String),
    /// Browser or device sign-in of a provider in `greencode_core::login::PROVIDERS`.
    SignIn(String),
    OpenCodeLogin,
    HermesSetup(String),
    CopyMcp,
    Cite,
    Usage,
    UsageHistory,
    Workflows,
    AnthropicLogin,
    #[cfg(unix)]
    ImportLogins,
    CopilotLogin,
    ChatGptLogin,
    MistralLogin,
    AntigravityLogin,
    AntigravityInstall,
    AntigravityAuthType(String),
    Logout(String),
    SwitchAccount(String, String),
    SaveAccount(String),
    ImportSession(import::Found),
    ImportSessionFiles,
    Undo,
    Fork,
    Export,
    Attach,
    Sandbox,
    Effort(Option<String>),
    /// Moves to the next (true) or previous effort level, wrapping around.
    EffortStep(bool),
    OpenCodeEffort(Option<String>),
    OpenFile(PathBuf),
    Revoke(String),
    Theme(Choice),
    ThemeMode(themes::ThemeMode),
    CycleThemeMode,
    ImportTheme,
    ExportTheme,
    EditTheme,
    PickBackground,
    ClearBackground,
    BackgroundOpacity(i16),
    /// A model of any backend; switches the thread's backend, or opens a new thread when it has already started.
    BackendModel(String, String),
    RefreshBackendModels,
    Update(bool, Channel),
    Shell(String),
    Terminal,
    Handoff,
    HandoffAdopt,
    InstallAgent(&'static str),
    Welcome(welcome::WelcomeAct),
    Stage(String),
    Unstage(Vec<String>),
    GitReview,
    Stash,
    /// Copies a `greencode://new` link for the draft, with the repository and model when true.
    CopyPromptLink(bool),
    CopyThreadLink,
    CopyThreadId,
    /// Copies one user or assistant message's text.
    CopyMessage(usize),
    /// Copies the last assistant reply as Markdown.
    CopyLastReply,
    /// Opens the `greencode://` link on the clipboard.
    OpenPromptLink,
    Unstash(usize),
    Snooze(u64),
    WakeAll,
    /// Cancels the reminder at this index of `ui["reminders"]`.
    Unschedule(usize),
    CopyWebhook(String),
    DeleteWebhook(String),
    NewScratch,
    SaveProjectDefault,
    /// Flips a boolean UI setting.
    Toggle(&'static str),
    /// Enter sends (default) or ⌘Enter sends and Enter inserts a newline.
    SendShortcut,
    /// ⌘Q: direct, hold, or two presses.
    CycleQuitMode,
    /// Text generation model for titles, commits and PRs; `None` = off.
    TextModel(Option<String>),
    CycleTextModel,
    RegenerateTitle,
    GenerateCommit,
    GeneratePr,
    CycleVoiceProvider,
    /// Voice settings button: `language`, `mic` or `test`.
    VoiceAction(&'static str),
    /// SnapShot (macOS): turn the capture shortcut on or off, include app text, change the shortcut.
    SnapshotToggle,
    SnapshotText,
    SnapshotShortcut,
    SetLearning(bool),
    /// Turns one native `ext` tool on or off.
    ToggleExt(&'static str),
    /// Automatic compaction: summary → snap → off.
    CycleCompaction,
    Prompt(&'static str),
    AddRepo,
    AddRepoPath(PathBuf),
    RemoveRepo(PathBuf),
    ToggleWorktreeRepo(PathBuf),
    DiffRepo(Option<PathBuf>),
    RepoCommit(PathBuf),
    RepoPush(PathBuf),
    CloneProject,
    NewProject,
    PublishProject,
    /// Switches the diff panel between uncommitted, base-branch and single-commit sources.
    DiffSource(greencode_core::git::DiffSource),
    DiffCommit(String),
    PickCommit,
    PinProject(PathBuf),
    ArchiveProject(PathBuf),
    SetCloneRoot,
    /// Asks for a text setting in the palette's search box, prefilled with its value.
    EditText(git::TextPrompt),
    CycleWorktreeSubmodules,
    /// Fetches and fast-forwards the default branch now, even when the setting is off.
    SyncDefaultBranch,
    /// Runs the value the palette is asking for (clone URL, project name, branch prefix).
    SubmitText,
    /// Squash-merges a PR, or turns auto-merge on; `true` = auto.
    MergePr(u64, bool),
    Quote,
    /// Rewinds to the prompt this many turns before the last one, for editing.
    Rewind(usize),
    /// Moves the active thread up (true) or down within its project and pin group.
    Move(bool),
    /// Sends this prompt to the active thread, queued while it is running.
    Ask(String),
    /// Side chat: asks a question in a copy of the thread, leaving the thread itself untouched.
    Side(String),
    /// Quotes the side chat's last answer into its parent thread's composer.
    BringBack,
    /// Steers the parent thread with the side answer's `<inject>` block (`btw.inject`).
    InjectBtw,
    /// Shows what the turn this many turns before the last one changed, in the diff panel.
    TurnDiff(usize),
    BashDiff,
    RestartAgent,
    ToggleDisabled(&'static str, String),
    ToolVisibility,
    Triage,
    /// Copies a 24-hour read-only guest link for the active thread. Needs a running `serve`.
    GuestLink,
    Retry,
    /// Sends the last prompt again once the provider's usage limit resets.
    ResumeAtReset,
    Skill(String),
    Speak,
    /// Default permission mode of new threads.
    DefaultMode(Mode),
    /// Makes the current thread's model the default for new threads.
    DefaultModel,
    Settings,
    /// Shows one section of the settings screen.
    SettingsTab(&'static str),
    Bind,
    Unbind(String),
    ClearProjectDefault,
}

/// Claude subscription sign-in: the browser shows a code that is pasted here.
struct OAuthForm {
    /// Taken when the exchange starts; `None` = request in flight.
    login: Option<greencode_core::provider::anthropic_oauth::Login>,
    url: SharedString,
    field: Entity<InputState>,
    /// The running exchange; esc aborts it.
    pending: Option<tokio::task::AbortHandle>,
    _sub: Subscription,
}

/// Custom-provider form: name, base URL, API key, models.
struct ProviderForm {
    fields: [Entity<InputState>; 4],
    anthropic: bool,
    error: Option<SharedString>,
    _subs: Vec<Subscription>,
}

enum UpdateStatus {
    Unchecked,
    Checking,
    Installing,
    Checked(SharedString),
    Installed(SharedString),
    Failed(SharedString),
}

impl UpdateStatus {
    fn message(&self) -> SharedString {
        match self {
            Self::Unchecked => "更新状況は未確認です。".into(),
            Self::Checking => "更新を確認中…".into(),
            Self::Installing => "更新を確認・適用中…".into(),
            Self::Checked(message) | Self::Installed(message) | Self::Failed(message) => message.clone(),
        }
    }

    fn locked(&self) -> bool {
        matches!(self, Self::Checking | Self::Installing | Self::Installed(_))
    }
}

pub struct Workspace {
    cfg: Arc<Config>,
    cfg_error: Option<String>,
    threads: Vec<Thread>,
    active: usize,
    input: Entity<TextareaState>,
    list: ListState,
    focus: FocusHandle,
    /// F6 targets: the thread sidebar, the conversation, and the diff panel.
    sidebar_focus: FocusHandle,
    conversation_focus: FocusHandle,
    diff_focus: FocusHandle,
    find: find::Find,
    nav: nav::Nav,
    undo: nav::Undo,
    quit: nav::QuitGuard,
    show_diff: bool,
    /// (thread id, first item) of work groups the user expanded.
    expanded_groups: std::collections::HashSet<(String, usize)>,
    files_panel: Option<FilesPanel>,
    pr_panel: Option<prs::PrPanel>,
    diff: Vec<SharedString>,
    diff_generation: u64,
    /// What the diff panel shows: uncommitted, against the base branch, or one commit.
    diff_source: greencode_core::git::DiffSource,
    /// Newest commits of the diff repository, for the commit picker.
    diff_commits: Vec<(String, String)>,
    /// The palette is listing commits for the diff panel.
    picking_commit: bool,
    /// The palette is asking for a text value.
    text_prompt: Option<git::TextPrompt>,
    /// When each repository's default branch was last synced.
    synced: std::collections::HashMap<PathBuf, std::time::Instant>,
    palette: Option<(Entity<InputState>, Subscription)>,
    model_picker: Option<ModelPicker>,
    login_palette: bool,
    registry_entries: Option<Vec<(String, Act)>>,
    palette_accounts: Vec<(String, Act)>,
    palette_logins: Vec<(String, Act)>,
    palette_effort: Vec<&'static str>,
    palette_skills: Vec<String>,
    palette_scroll: ScrollHandle,
    settings_scroll: ScrollHandle,
    settings_focus: FocusHandle,
    /// Index into `SETTINGS_TABS`; opening settings starts at 一般.
    settings_tab: usize,
    show_update_panel: bool,
    update_channel: Channel,
    update_status: UpdateStatus,
    /// Highlighted palette row; ↑/↓ move it, Enter runs it.
    palette_ix: usize,
    /// `provider/model` ids reported by the providers' `/models` endpoints.
    fetched_models: Vec<String>,
    /// Models of the external backends (Pi, OpenCode, ...), from the cache and refreshed in the background.
    backend_models: std::collections::BTreeMap<String, Vec<String>>,
    backend_options: std::collections::BTreeMap<String, Vec<Value>>,
    /// OpenCode's models and probe state are isolated by project.
    project_models: std::collections::BTreeMap<PathBuf, Vec<String>>,
    probed: std::collections::BTreeSet<(String, PathBuf)>,
    /// Persistent UI settings (`ui.json`): theme, follow-up behavior, keybindings, project defaults, stash.
    ui: Value,
    /// Files in `~/.greencode/themes`, reloaded on import, save and CLI polling.
    custom_themes: Vec<(String, greencode_core::theme::Theme)>,
    theme_edit: Option<themes::ThemeEdit>,
    voice_recording: Option<voice::VoiceRecording>,
    /// The SnapShot helper process; aborting it releases the shortcut.
    snapshot: Option<tokio::task::AbortHandle>,
    webhooks: Arc<std::sync::Mutex<Vec<greencode_core::webhook::Hook>>>,
    webhook_server: Option<tokio::task::AbortHandle>,
    /// Runs schedules, webhooks and due workflow runs while Labs → Workflows is on.
    workflow_engine: Option<tokio::task::AbortHandle>,
    /// Settings screen: inputs for a new keybinding (key, command) while it is open.
    settings: Option<([Entity<InputState>; 2], Vec<Subscription>)>,
    secrets: secrets::Secrets,
    advanced: advanced::Advanced,
    connection_name: Option<(Entity<InputState>, Subscription)>,
    /// Connections tab: executable, arguments, environment, backend model id and provider model id.
    connection_fields: Option<([Entity<InputState>; 5], Vec<Subscription>)>,
    storage: storage::Storage,
    repos: repos::RepoState,
    agent_versions: std::collections::BTreeMap<&'static str, Option<String>>,
    agent_versions_generation: u64,
    welcome: Option<welcome::Welcome>,
    /// Position in the sent prompts while ↑/↓ recall walks them.
    recall: Option<usize>,
    terminals: TerminalSet,
    handoffs: HashSet<String>,
    handoff_diverged: HashSet<String>,
    terminal_visible: bool,
    /// Current branch and local branches of the active project.
    branch: String,
    branches: Vec<String>,
    /// Open pull requests of the active project (number, title, head branch), from `gh`.
    prs: Vec<(u64, String, String)>,
    git_status: Vec<GitEntry>,
    show_git: bool,
    /// Thread ids matching the palette's `?text` search.
    found: Vec<String>,
    /// Bumped for every palette search; a result only lands when it is still current.
    search_gen: u64,
    /// Bumped for every composer edit; only the last save timer writes the draft.
    draft_save_gen: u64,
    /// A streaming text flush is already scheduled (~30 fps).
    text_flush_pending: bool,
    /// When the importable sessions were last discovered, per project.
    import_checked: Option<(PathBuf, std::time::Instant)>,
    /// Bumped for every background git read; an older read does not overwrite a newer one.
    git_generation: u64,
    /// `work_groups` of the active thread, reused for every visible row.
    groups_cache: std::cell::RefCell<(String, usize, (bool, bool), std::rc::Rc<Vec<Option<usize>>>)>,
    /// Antigravity runtime install shown on its button instead of in the chat: running, then done.
    antigravity_install: Option<&'static str>,
    provider_form: Option<ProviderForm>,
    queue_edit: Option<queue::QueueEdit>,
    oauth_form: Option<OAuthForm>,
    key_form: Option<KeyForm>,
    account_form: Option<AccountForm>,
    cite_form: Option<CiteForm>,
    /// Full selected text behind `cite_form`.
    cite_text: String,
    usage: Option<usage::UsageView>,
    workflows: Option<workflows::Workflows>,
    device_form: Option<DeviceForm>,
    /// Slash commands of the active project.
    commands: Vec<String>,
    /// Text an extension asked the composer to show (`set_editor_text`); applied on the next render, which has the window.
    editor_text: Option<String>,
    /// Held `/command`s whose discovery ended: (thread id, text, backend). Dispatched on the next render, which has the window.
    released: Vec<(String, String, String)>,
    defer_gen: u64,
    agent_spawner: AgentSpawner,
    /// Tracked files of a project, for `@` completion.
    files: Option<(PathBuf, Vec<String>)>,
    /// Skills of the project for `$`/`/` completion, re-read when stale.
    mentions: mentions::Cache,
    hermes_profiles: Vec<hermes::Profile>,
    /// Claude Code and Codex sessions of the active project, refreshed when the palette opens.
    importable: Vec<import::Found>,
    show_archived: bool,
    _subs: Vec<Subscription>,
    // Drop terminals first so their pipes close before the runtime waits for blocking I/O.
    rt: tokio::runtime::Runtime,
}

fn extension_text(text: &str) -> String {
    let mut chars = text.chars().peekable();
    let mut output = String::with_capacity(text.len());
    while let Some(c) = chars.next() {
        if c == '\x1b' && chars.peek() == Some(&'[') {
            chars.next();
            for c in chars.by_ref() {
                if ('@'..='~').contains(&c) {
                    break;
                }
            }
        } else {
            output.push(c);
        }
    }
    output
}

/// Sections of the settings screen, in sidebar order; icons must be among gpui-kit's bundled defaults.
const VOICE_SETTINGS: &str = "音声入力";
const WORKFLOW_SETTINGS: &str = "自動化";
const RUNTIME_SETTINGS: &str = "動作設定";
const SETTINGS_TABS: [(&str, Ic); 13] = [
    ("一般", Ic::Settings2),
    ("外観", Ic::Palette),
    ("入力と会話", Ic::FileText),
    (VOICE_SETTINGS, Ic::Mic),
    ("モデルと権限", Ic::Settings),
    ("接続", Ic::Network),
    ("プロジェクト", Ic::Folder),
    (WORKFLOW_SETTINGS, Ic::Calendar),
    ("ツール", Ic::SquareTerminal),
    ("認証情報", Ic::EyeOff),
    ("ショートカットとファイル", Ic::File),
    ("詳細設定", Ic::Settings2),
    (RUNTIME_SETTINGS, Ic::LayoutDashboard),
];

/// Slash commands handled by the GUI in native threads, for completion.
const BUILTINS: [&str; 54] = [
    "skill",
    "webhook",
    "webhook-secret",
    "webhook-rotate",
    "schedule",
    "schedule!",
    "side",
    "btw",
    "model",
    "models",
    "new",
    "clear",
    "mode",
    "plan",
    "status",
    "help",
    "diff",
    "provider",
    "login",
    "logout",
    "undo",
    "retry",
    "fork",
    "export",
    "import",
    "attach",
    "effort",
    "sandbox",
    "settings",
    "init",
    "review",
    "commit",
    "learn",
    "usage",
    "usage-limits",
    "usage-history",
    "update",
    "goal",
    "terminal",
    "theme",
    "stash",
    "rename",
    "snooze",
    "branch",
    "push",
    "pr",
    "scratch",
    "to",
    "titleprompt",
    "voice",
    "clone",
    "project",
    "publish",
    "sync",
];

/// T3's composer limit; longer drafts stay in the composer.
const MAX_MESSAGE_CHARS: usize = 120_000;

fn over_limit(text: &str) -> bool {
    text.chars().count() > MAX_MESSAGE_CHARS
}

const HELP: &str = "/model [provider/model], /models  モデルを選ぶ（引数なしで一覧）
/new  新規スレッド   /clear  同じスレッドで新しいセッション（履歴は表示したまま）
/mode ask|auto-edit|full|plan  権限モード（/plan で読み取り専用のプランモードを切り替え）
/undo  最後のターンを取り消し、write・edit で変えたファイルを戻す   /retry  直前のプロンプトを再送
/fork  スレッドを複製   /export  Markdown に書き出し   /import  Claude Code / Codex セッションを開く
/attach  ファイルを添付（画像は ⌘V で貼り付け可。ウィンドウへのドラッグ＆ドロップも可）
/effort [low|medium|high]  思考の強さ   /sandbox  bash の書き込み制限
/theme [名前|system|light|dark]  テーマ・外観の切替（⌘⌥A 一覧、⌘⌥⇧A 外観を切替）
/settings  設定画面（⌘,）   !コマンド  シェルコマンドを直接実行
/voice 音声入力（設定・言語・辞書・マイク）。⌘⇧D で録音、esc で破棄
/compact [snap|summary]  会話を圧縮（snap は画像に退避、モデル呼び出しなし）
/to NAME 指示  サブエージェントに指示
/status  モデル・権限・トークン使用量
/usage  サブスクリプションの利用上限   /usage-limits  上限をメッセージで表示   /usage-history  コスト・トークンの履歴
/diff  差分パネル
/login [プロバイダ名]  プロバイダにログイン（APIキー / Claude サブスクリプション / vibe で Mistral Vibe）
/logout [プロバイダ名]  保存したキー・サブスクリプションを削除   /provider  プロバイダを追加
/skill [名前]  スキルを実行（引数なしで一覧）   /learn 教訓  プロジェクトの学習として保存
/init  AGENTS.md を作成
/review  未コミットの変更をレビュー   /commit  変更をコミット
/branch 名前  ブランチ作成   /push  プッシュ   /pr  プルリクエスト作成（gh）
/goal 完了条件  達成まで自動で続行（/goal pause・resume・clear）
/terminal  ターミナル（⌘J）   /stash  下書きを退避・復元（⌘S）
/side 質問（/btw も可）  スレッドを変えずに質問するサイドチャット。回答はパレットの「持ち帰る」で元のスレッドへ
/schedule 30m プロンプト  定期実行（m・h・d、起動中のみ）。/schedule で一覧、/schedule clear で全削除
/schedule HH:MM|+30m|+2h プロンプト  指定時刻に送信（アプリ起動中）。/schedule で予約を一覧・取り消し
/update [check] [stable|main]  GitHub の安定版または main のコミットビルドを確認・適用
/webhook プロンプト  Webhook を作成（/webhook で一覧）   /webhook-secret ID SECRET 署名を設定   /webhook-rotate ID URL を更新
/rename 名前  スレッド名変更   /snooze [時間]  スレッドを一時的に隠す   /scratch  プロジェクトなしで開始
/titleprompt 指示  スレッド名を付けるときの指示（設定 → 入力と会話）
/clone [owner/name|URL]  リポジトリをクローン（引数なしで入力欄）   /project [名前]  新規プロジェクト   /publish  GitHub に非公開で公開   /sync  既定ブランチを今すぐ同期
↑↓ 送信済みプロンプトの呼び出し · ⌥↑ キュー末尾を編集 · ⇧⌘A 権限切り替え · ⌘⏎ 送信して次の下書きへ
⌘K パレット（? で始めると会話の全文検索）· （↑↓ で選択） · ⌘N 新規 · ⌘O プロジェクト · ⌘D 差分 · / コマンド · $ スキル · @ ファイル・スレッド · # PR · esc 停止";

const INIT_PROMPT: &str = "Explore this project and write AGENTS.md at its root: what it is, how to build, run and test it, the layout, and the conventions a coding agent must follow. Keep it short and specific to this repository. If AGENTS.md already exists, improve it instead of replacing it.";

const COMMIT_PROMPT: &str = "Commit the current changes: inspect `git status` and `git diff`, stage what belongs together, and write a concise commit message in the style of this repository's history. Do not push.";

/// Ends the prompt `/goal` sends; also marks it in the queue.
const GOAL_SUFFIX: &str = "(Goal mode: keep working until this is completely done and verified. Only then end your reply with the line GOAL MET.)";
/// Starts each automatic goal continuation in the queue.
const GOAL_CONTINUE: &str = "Continue working toward the goal: ";

const REVIEW_PROMPT: &str = "Review the uncommitted changes in this project (`git status`, `git diff`, and untracked files). Report bugs, regressions, security problems and missing tests, most serious first, each with file and line. Do not modify any files.";

/// Providers listed in settings even without a key; the rest of the presets appear once they have one.
const PRESET_SHOWN: [&str; 6] = [
    "anthropic",
    "openai",
    "openrouter",
    "google",
    "ollama",
    "lmstudio",
];

/// `gh pr list --json number,title,headRefName` output as (number, title, head branch).
fn parse_prs(json: &[u8]) -> Vec<(u64, String, String)> {
    let list: Vec<Value> = serde_json::from_slice(json).unwrap_or_default();
    list.iter()
        .filter_map(|p| {
            Some((
                p["number"].as_u64()?,
                p["title"].as_str()?.to_string(),
                p["headRefName"].as_str()?.to_string(),
            ))
        })
        .collect()
}

/// How much of the agent's tool activity the transcript shows (T3's "Tool calls in chat").
#[derive(Clone, Copy, PartialEq)]
enum ToolVisibility {
    Show,
    Successful,
    All,
    Zen,
}

/// What an extension tool does, for the settings screen; core's `CATALOG` text is the model's English.
fn ext_hint(name: &str) -> Option<&'static str> {
    Some(match name {
        "hread" => "行番号つきで読む",
        "hedit" => "行番号を指定して編集",
        "ast" => "構文単位で検索・置換",
        "eval" => "Python / JS を続けて実行",
        "debug" => "デバッガ",
        "advisor" => "別のモデルに意見を聞く",
        "learn" => "プロジェクトの学びを残す",
        "ask" => "ユーザーに質問する",
        "secret" => "認証情報を非公開で尋ねる",
        "checkpoint" => "途中の地点を記録",
        "rewind" => "記録した地点まで戻す",
        "new_context" => "引き継ぎメモから再開",
        "recall" => "過去のセッションを検索",
        "wait" => "プロセスの終了を待つ",
        "manage_skill" => "スキルを保存",
        "find" => "説明から該当ファイルを探す",
        "github" => "gh で Issue・PR・CI を読む",
        "dsm" => "バイナリを逆アセンブル",
        "tts" => "テキストを音声ファイルに",
        "ratchet" => "基準と比べて改善を繰り返す",
        "archive" => "プロンプトとセッションの履歴",
        "vault" => "Obsidian のノート",
        "security" => "セキュリティ診断の計画と実行",
        "report_issue" => "ツールの不具合を報告",
        _ => return None,
    })
}

fn tool_visibility_label(v: ToolVisibility) -> &'static str {
    match v {
        ToolVisibility::Show => "すべて表示",
        ToolVisibility::Successful => "成功したものは隠す",
        ToolVisibility::All => "すべて隠す",
        ToolVisibility::Zen => "最後の回答だけ表示",
    }
}

fn mode_label(m: Mode) -> &'static str {
    match m {
        Mode::Ask => "毎回確認",
        Mode::AutoEdit => "編集を自動許可",
        Mode::Full => "すべて許可",
        Mode::Plan => "読み取りのみ",
    }
}

/// Plain text for `say`, as the core's speech module builds it.
fn speech_text(markdown: &str) -> String {
    greencode_core::speech::plain(markdown)
}

fn spawn_external(program: &str, args: &[&str]) {
    if !cfg!(test) {
        #[cfg(not(target_os = "macos"))]
        if program == "/usr/bin/open" {
            match args {
                ["-a", "Visual Studio Code", path] => { let _ = open::with_detached(path, "code"); }
                ["-R", path] => {
                    if let Some(dir) = std::path::Path::new(path).parent() {
                        let _ = open::that_detached(dir);
                    }
                }
                ["-t", target] | [target] => { let _ = open::that_detached(target); }
                _ => {}
            }
            return;
        }
        let _ = std::process::Command::new(program).args(args).spawn();
    }
}

/// A macOS notification, only while GreenCode is not the active app.
fn os_notify(text: &str, cx: &App) {
    if cfg!(target_os = "macos") && cx.active_window().is_none() {
        spawn_external(
            "/usr/bin/osascript",
            &[
                "-e",
                "on run argv",
                "-e",
                "display notification (item 1 of argv) with title \"GreenCode\"",
                "-e",
                "end run",
                text,
            ],
        );
    }
}

/// Fills the composer with the caret after the text; `set_value` leaves a multi-line input's caret at the start.
fn set_input(
    s: &mut TextareaState,
    text: impl Into<SharedString>,
    window: &mut Window,
    cx: &mut Context<TextareaState>,
) {
    let text = text.into();
    let end = text.len();
    s.set_value(text, window, cx);
    s.set_selected_range(end..end, cx);
}

fn rpc_backend(cfg: &Config, name: &str) -> bool {
    cfg.backends
        .iter()
        .any(|b| b.name == name && b.protocol != greencode_core::config::Protocol::Acp)
}

/// Backends whose live channel carries RPC commands as well as steering.
fn live_commands(cfg: &Config, backend: &str) -> bool {
    backend == "native" || rpc_backend(cfg, backend)
}

/// Whether a message sent during a run joins that run instead of waiting for it.
fn can_steer(cfg: &Config, t: &Thread) -> bool {
    t.steering || live_commands(cfg, &t.meta.backend)
}

/// Completions of a thread: what the GUI handles itself plus what its backend advertises.
fn command_list(cfg: &Config, t: &Thread) -> Vec<String> {
    let mut commands = vec!["compact".to_string()];
    if t.meta.backend == "native" {
        commands.extend(BUILTINS.map(String::from));
        commands.extend(
            skills::commands(&t.meta.cwd, cfg)
                .into_iter()
                .chain(t.commands.iter().cloned()),
        );
    } else {
        commands.extend(t.commands.iter().chain(&t.mapped).cloned());
        if t.meta.backend == "opencode" {
            commands.extend(skills::opencode_commands(&t.meta.cwd, cfg));
        }
        commands.push("models".into());
        commands.push("clear".into());
    }
    if rpc_backend(cfg, &t.meta.backend) {
        commands
            .extend(["model", "thinking", "clear", "stats", "export", "goal"].map(String::from));
    }
    let mut seen = HashSet::new();
    commands.retain(|command| seen.insert(command.clone()));
    commands
}

fn chip(id: impl Into<ElementId>, label: impl Into<SharedString>, pal: Pal) -> Stateful<Div> {
    div()
        .id(id)
        .px_2()
        .py_0p5()
        .rounded_md()
        .text_xs()
        .text_color(pal.muted)
        .cursor_pointer()
        .hover(|s| s.bg(pal.hover).text_color(pal.fg))
        .child(label.into())
}

fn control_button(id: impl Into<ElementId>, label: impl Into<SharedString>, pal: Pal) -> gpui_kit::base::Button {
    let label = label.into();
    gpui_kit::base::Button::new(id)
        .accessibility_label(label.clone())
        .flex_shrink_0()
        .px_2()
        .py_1()
        .rounded_md()
        .text_xs()
        .text_color(pal.fg)
        .cursor_pointer()
        .hover(|s| s.bg(pal.hover).text_color(pal.fg))
        .child(label)
}

/// An on/off switch in the palette's colors; the caller supplies `on_change`.
fn switch(id: impl Into<ElementId>, label: impl Into<SharedString>, on: bool, pal: Pal) -> gpui_kit::base::Switch {
    gpui_kit::base::Switch::new(id)
        .accessibility_label(label)
        .checked(on)
        .flex_shrink_0()
        .w(px(32.))
        .h(px(18.))
        .p(px(2.))
        .flex()
        .items_center()
        .when(on, |d| d.justify_end())
        .rounded_full()
        .cursor_pointer()
        .bg(if on { pal.accent } else { pal.fg.opacity(0.2) })
        .child(div().size(px(14.)).rounded_full().bg(hsla(0., 0., 1., 1.)))
}

fn settings_row(label: SharedString, hint: &str, control: impl IntoElement, pal: Pal) -> Div {
    div()
        .flex()
        .flex_wrap()
        .items_center()
        .justify_between()
        .gap_x_8()
        .gap_y_3()
        .min_h(px(44.))
        .child(
            div()
                .flex_1()
                .min_w(px(148.))
                .flex()
                .flex_col()
                .gap_1p5()
                .text_base()
                .font_weight(FontWeight::MEDIUM)
                .child(label)
                .when(!hint.is_empty(), |d| {
                    d.child(div().text_sm().font_weight(FontWeight::NORMAL).text_color(pal.muted).child(hint.to_string()))
                }),
        )
        .child(control)
}

fn settings_select(
    id: impl Into<ElementId>,
    label: &str,
    current: &str,
    options: Vec<(&'static str, Vec<Act>)>,
    pal: Pal,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) -> impl IntoElement {
    use gpui_kit::component::{
        IndexPath, Sizable as _,
        select::{SearchableVec, Select, SelectEvent, SelectState},
    };
    type Options = Vec<(&'static str, Vec<Act>)>;
    type State = (Entity<SelectState<SearchableVec<&'static str>>>, Subscription, Options);

    let index = options.iter().position(|(option, _)| *option == current);
    let selected = index.map(|i| options[i].0);
    let index = index.map(IndexPath::new);
    let workspace = cx.weak_entity();
    let entry = window.use_keyed_state(id, cx, |window, cx: &mut Context<State>| {
        let items = SearchableVec::new(options.iter().map(|(label, _)| *label).collect::<Vec<_>>());
        let select = cx.new(|cx| SelectState::new(items, index, window, cx).searchable(false));
        let entry = cx.weak_entity();
        let subscription = window.subscribe(&select, cx, move |_, event, window, cx| {
            let SelectEvent::Confirm(Some(value)) = event else { return };
            let Some(entry) = entry.upgrade() else { return };
            let Some((_, acts)) = entry.read(cx).2.iter().find(|(label, _)| label == value) else { return };
            let acts = acts.clone();
            _ = workspace.update(cx, |this, cx| {
                for act in acts {
                    this.run(act, window, cx);
                }
            });
        });
        (select, subscription, Vec::new())
    });
    let select = entry.update(cx, |entry, _| {
        entry.2 = options;
        entry.0.clone()
    });
    if select.read(cx).selected_value().copied() != selected {
        select.update(cx, |state, cx| state.set_selected_index(index, window, cx));
    }
    div().flex_shrink_0().w(px(200.)).child(
        Select::new(&select)
            .accessibility_label(label.to_owned())
            .placeholder(current.to_owned())
            .bg(pal.panel)
            .text_color(pal.fg)
            .border_color(pal.line),
    )
}

/// Settings content laid out as in T3 Code: titled sections, each a bordered card of divided rows.
struct SettingsPage {
    pal: Pal,
    groups: Vec<(&'static str, Vec<AnyElement>)>,
}

impl SettingsPage {
    fn section(mut self, title: &'static str) -> Self {
        self.groups.push((title, Vec::new()));
        self
    }

    fn child(mut self, element: impl IntoElement) -> Self {
        if self.groups.is_empty() {
            self.groups.push(("", Vec::new()));
        }
        self.groups.last_mut().unwrap().1.push(element.into_any_element());
        self
    }

    fn children<E: IntoElement>(mut self, elements: impl IntoIterator<Item = E>) -> Self {
        for element in elements {
            self = self.child(element);
        }
        self
    }

    fn build(self) -> Div {
        let pal = self.pal;
        div().flex().flex_col().gap_10().children(self.groups.into_iter().map(move |(title, rows)| {
            div()
                .flex()
                .flex_col()
                .gap_3()
                .when(!title.is_empty(), |d| {
                    d.child(
                        div()
                            .px_1()
                            .text_lg()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(pal.fg)
                            .child(title),
                    )
                })
                .child(
                    div()
                        .rounded_xl()
                        .border_1()
                        .border_color(pal.line)
                        .bg(pal.card)
                        .overflow_hidden()
                        .children(rows.into_iter().enumerate().map(move |(i, row)| {
                            div()
                                .px_6()
                                .py_5()
                                .when(i > 0, |d| d.border_t_1().border_color(pal.line))
                                .child(row)
                        })),
                )
        }))
    }
}

fn theme_preview(pal: Pal) -> Div {
    let line = |w: f32| {
        div().w(relative(w)).h_1().rounded_full().bg(pal.muted.opacity(0.33))
    };
    div()
        .size_full()
        .flex()
        .bg(pal.panel)
        .child(
            div().w(relative(0.24)).h_full().min_w_0().bg(pal.sidebar)
                .border_r_1().border_color(pal.line).p_2()
                .flex().flex_col().gap_2()
                .child(div().w(relative(0.72)).h_1p5().rounded_full().bg(pal.accent))
                .child(line(0.83)).child(line(0.65)).child(line(0.76)),
        )
        .child(
            div().flex_1().min_w_0().p_2().flex().flex_col().gap_2()
                .child(line(0.37))
                .child(
                    div().w(relative(0.68)).ml_auto().h_4().rounded_md().bg(pal.selected),
                )
                .child(line(0.78))
                .child(line(0.58))
                .child(
                    div().mt_auto().h_5().rounded_md().border_1().border_color(pal.line)
                        .bg(pal.card).flex().items_center().justify_end().px_1()
                        .child(div().size_2p5().rounded_sm().bg(pal.accent)),
                ),
        )
}

fn appearance_preview(system: bool, light: Pal, dark: Pal) -> Div {
    let frame = div().w_full().h(px(112.)).flex().rounded_md().overflow_hidden();
    let half = |pal: Pal, right: bool| {
        div().relative().w(relative(0.5)).h_full().overflow_hidden().child(
            div().absolute().top_0().h_full().w(relative(2.))
                .when(right, |d| d.right_0())
                .when(!right, |d| d.left_0())
                .child(theme_preview(pal)),
        )
    };
    if system {
        frame.child(half(light, false)).child(half(dark, true))
    } else {
        frame.child(theme_preview(light))
    }
}

fn theme_settings(ws: &Workspace, pal: Pal, window: &Window, cx: &mut Context<Workspace>) -> Div {
    let tile = |id: String, label: String, selected: bool, act: Act, cx: &mut Context<Workspace>| {
        gpui_kit::base::Button::new((ElementId::from("theme"), SharedString::from(id)))
            .accessibility_label(SharedString::from(label))
            .aria_toggled(if selected { accesskit::Toggled::True } else { accesskit::Toggled::False })
            .min_w_0()
            .p_2()
            .rounded_lg()
            .border_1()
            .border_color(if selected { pal.accent } else { pal.line })
            .bg(if selected { pal.selected } else { pal.card })
            .text_color(pal.fg)
            .cursor_pointer()
            .hover(|s| s.bg(pal.hover))
            .focus(|s| s.border_color(pal.fg))
            .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                this.run(act.clone(), window, cx)
            }))
    };
    let label = |text: String, selected: bool| {
        div().flex().items_center().gap_1p5().min_w_0().text_sm()
            .font_weight(FontWeight::MEDIUM)
            .child(div().min_w_0().truncate().child(text))
            .when(selected, |d| {
                d.child(div().flex_shrink_0().text_color(pal.accent).child(Ic::Check))
            })
    };
    let heading = |text| div().text_base().font_weight(FontWeight::SEMIBOLD).child(text);
    let light_pal = ws.theme_pal(&ws.theme_slot(false), false);
    let dark_pal = ws.theme_pal(&ws.theme_slot(true), true);
    let mode = ws.theme_mode();
    let modes = div().grid().grid_cols(3).gap_3().children(themes::ThemeMode::ALL.map(|m| {
        tile(
            format!("mode-{}", m.id()),
            m.hint().to_string(),
            m == mode,
            Act::ThemeMode(m),
            cx,
        )
        .child(
            div().w_full().flex().flex_col().items_center().gap_2()
                .child(appearance_preview(
                    m == themes::ThemeMode::System,
                    if m == themes::ThemeMode::Dark { dark_pal } else { light_pal },
                    dark_pal,
                ))
                .child(label(m.hint().to_string(), m == mode)),
        )
    }));
    let slot_dark = mode.uses_dark_slot(Workspace::window_is_dark(window));
    let selected = ws.theme_slot(slot_dark);
    let choices = themes::choices(&ws.custom_themes)
        // Keep the selected theme visible if it was set from an older build or a command.
        .filter(|choice| *choice == selected || choice.dark(&ws.custom_themes, slot_dark) == slot_dark);
    let grid = div().grid().grid_cols(2).gap_2().children(choices.map(|choice| {
        let is_selected = choice == selected;
        let swatch = ws.theme_pal(&choice, slot_dark);
        let name = match choice {
            Choice::Builtin(ThemeId::Light) if !slot_dark => "標準".into(),
            Choice::Builtin(ThemeId::Dark) if slot_dark => "標準".into(),
            _ => choice.label(&ws.custom_themes),
        };
        tile(
            format!("slot-{slot_dark}-{}", choice.id()),
            name.clone(),
            is_selected,
            Act::Theme(choice),
            cx,
        )
        .child(
            div().w_full().min_w_0().flex().items_center().gap_2()
                .child(
                    div().size_7().flex_shrink_0().flex().items_center()
                        .justify_center().rounded_full().bg(swatch.sidebar)
                        .border_1().border_color(swatch.muted)
                        .child(
                            div().size_5().flex().items_center().justify_center()
                                .rounded_full().bg(swatch.card)
                                .child(div().size_2p5().rounded_full().bg(swatch.accent)),
                        ),
                )
                .child(label(name, is_selected)),
        )
    }));
    let configured = ws.ui["background_image"].as_str().map(|p| p.to_string());
    let opacity = (ws.background_opacity() * 100.).round() as u32;
    div()
        .flex().flex_col().gap_3().pb_6()
        .child(heading("外観モード"))
        .child(modes)
        .child(heading("配色テーマ").pt_2())
        .when(mode == themes::ThemeMode::System, |d| {
            let appearance = if slot_dark { "ダーク" } else { "ライト" };
            d.child(div().text_sm().text_color(pal.muted).child(format!(
                "現在は{appearance}。ここで選んだ配色は、システムが{appearance}のときに使われます。"
            )))
        })
        .child(grid)
        .child(heading("カスタムテーマ").pt_2())
        .child(
            div().flex().flex_wrap().items_center().gap_1p5()
                .child(chip("theme-import", "読み込む…（VS Code / GreenCode JSON）", pal).on_click(
                    cx.listener(|this, _: &ClickEvent, window, cx| this.run(Act::ImportTheme, window, cx)),
                ))
                .child(chip("theme-export", "書き出す…", pal).on_click(
                    cx.listener(|this, _: &ClickEvent, window, cx| this.run(Act::ExportTheme, window, cx)),
                ))
                .child(chip("theme-edit", "編集…", pal).on_click(
                    cx.listener(|this, _: &ClickEvent, window, cx| this.run(Act::EditTheme, window, cx)),
                )),
        )
        .child(heading("背景画像").pt_2())
        .child(
            div().flex().items_center().gap_2()
                .child(div().flex_1().min_w_0().truncate().text_color(if configured.is_some() { pal.fg } else { pal.muted }).child(
                    configured.clone().unwrap_or_else(|| "未設定（JPEG / PNG / WebP、16 MB まで）".into()),
                ))
                .child(chip("bg-pick", "選択…", pal).on_click(
                    cx.listener(|this, _: &ClickEvent, window, cx| this.run(Act::PickBackground, window, cx)),
                ))
                .when(configured.is_some(), |d| {
                    d.child(chip("bg-clear", "クリア", pal).on_click(
                        cx.listener(|this, _: &ClickEvent, window, cx| this.run(Act::ClearBackground, window, cx)),
                    ))
                }),
        )
        .when(configured.is_some(), |d| {
            d.child(
                div().flex().items_center().gap_2()
                    .child(div().flex_1().child(format!("背景の不透明度: {opacity}%")))
                    .child(chip("bg-minus", "−10%", pal).on_click(
                        cx.listener(|this, _: &ClickEvent, window, cx| this.run(Act::BackgroundOpacity(-10), window, cx)),
                    ))
                    .child(chip("bg-plus", "+10%", pal).on_click(
                        cx.listener(|this, _: &ClickEvent, window, cx| this.run(Act::BackgroundOpacity(10), window, cx)),
                    )),
            )
        })
}

type Ic = gpui_kit::assets::IconName;

/// A 26px square, borderless button for toolbars; `icon` is an icon or a text glyph.
fn icon_button(id: impl Into<ElementId>, icon: impl IntoElement, pal: Pal) -> Stateful<Div> {
    div()
        .id(id)
        .size(px(26.))
        .flex()
        .flex_shrink_0()
        .items_center()
        .justify_center()
        .rounded_md()
        .text_sm()
        .text_color(pal.muted)
        .cursor_pointer()
        .hover(|s| s.bg(pal.hover).text_color(pal.fg))
        .child(icon)
}

fn project_name(project: &std::path::Path) -> String {
    project
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| project.display().to_string())
}

/// A path shown relative to the project when it lies inside it (macOS reports `/private/tmp` for `/tmp`).
fn relative_to_project(path: &str, project: &std::path::Path) -> String {
    let p = std::path::Path::new(path);
    let canonical = project.canonicalize().ok();
    let rel = p
        .strip_prefix(project)
        .ok()
        .or_else(|| canonical.as_deref().and_then(|c| p.strip_prefix(c).ok()))
        .map(|r| r.display().to_string());
    rel.filter(|r| !r.is_empty())
        .unwrap_or_else(|| path.to_string())
}

/// Small rounded square with the first two letters of the project name.
fn avatar(name: &str, pal: Pal) -> Div {
    div()
        .size(px(18.))
        .flex_shrink_0()
        .rounded(px(5.))
        .bg(pal.selected)
        .text_size(px(9.))
        .flex()
        .items_center()
        .justify_center()
        .child(SharedString::from(
            name.chars().take(2).collect::<String>().to_uppercase(),
        ))
}

/// `今`, `3m`, `2h`, `14d`.
fn age(created: u64) -> String {
    let secs = now().saturating_sub(created);
    match secs {
        0..=59 => "今".to_string(),
        60..=3599 => format!("{}m", secs / 60),
        3600..=86399 => format!("{}h", secs / 3600),
        _ => format!("{}d", secs / 86400),
    }
}

/// Seconds east of UTC in the local zone, so session dates follow the user's calendar.
fn utc_offset() -> i64 {
    #[cfg(unix)]
    unsafe {
        let t = now() as libc::time_t;
        let mut tm: libc::tm = std::mem::zeroed();
        if !libc::localtime_r(&t, &mut tm).is_null() {
            return tm.tm_gmtoff as i64;
        }
    }
    // shortcut: Windows dates sessions in UTC; read GetTimeZoneInformation if that shows up wrong.
    0
}

/// Day number of `secs` in a zone `offset` seconds east of UTC.
fn local_day(secs: u64, offset: i64) -> u32 {
    ((secs as i64 + offset).max(0) / 86400) as u32
}

/// Sidebar heading for a session started on local day `day`, seen on local day `today`.
fn month_heading(day: u32, today: u32) -> String {
    let civil = greencode_core::usage_history::civil;
    let ((y, m, _), (ty, tm, _)) = (civil(day), civil(today));
    if (y, m) == (ty, tm) {
        "今月".into()
    } else if y == ty {
        format!("{m}月")
    } else {
        format!("{y}年{m}月")
    }
}

impl Workspace {
    pub fn new(
        cfg: Config,
        cfg_error: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        Theme::sync_system_appearance(Some(window), cx);
        cx.observe_window_appearance(window, |this, window, cx| {
            if this.theme_mode() == themes::ThemeMode::System {
                this.apply_theme(window, cx);
            }
        })
        .detach();
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("tokio runtime");
        let input = cx.new(|cx| {
            TextareaState::new(window, cx)
                .auto_grow(1, 8)
                .submit_on_enter(true)
                .placeholder("メッセージを入力")
        });
        let subs =
            vec![cx.subscribe_in(
                &input,
                window,
                |this, _, ev: &InputEvent, window, cx| match ev {
                    InputEvent::PressEnter { secondary, shift } => {
                        // In ⌘Enter mode the textarea does not submit on Enter, so it inserted the newline already.
                        if !*shift && !(this.ui["send_shortcut"] == "cmd+enter" && !*secondary) {
                            this.send(window, cx);
                        }
                    }
                    InputEvent::Change => {
                        let t = &mut this.threads[this.active];
                        t.draft = this.queue_edit.as_ref().map_or_else(
                            || this.input.read(cx).value().to_string(),
                            |edit| edit.draft.clone(),
                        );
                        if !t.draft.is_empty() {
                            t.suggestion = None;
                        }
                        // Typing `/` is the first sign the user wants commands: learn the backend's before the first prompt.
                        if t.draft.starts_with('/') {
                            this.discover_commands(cx);
                        }
                        this.queue_draft_save(cx);
                        cx.notify();
                    }
                    // Leaving the composer (palette, thread switch, app deactivation) writes the draft at once.
                    InputEvent::Blur => {
                        let t = &this.threads[this.active];
                        if let Err(e) = t.save() {
                            this.fail(e);
                        }
                    }
                    _ => {}
                },
            )];
        let ui: Value = std::fs::read(greencode_core::home().join("ui.json"))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_else(|| json!({}));
        let mut cwd = std::env::current_dir().unwrap_or_default();
        if cwd.parent().is_none() {
            cwd = greencode_core::user_home(); // launched from Finder
        }
        let mut meta = Meta::new(cwd, cfg.model.clone(), "native".into());
        meta.subagent_model = cfg.subagent_model.clone();
        // Reopen the thread that was showing when the app last closed. Empty composers from
        // earlier launches are not history, so only the remembered one keeps its draft.
        let mut saved = Session::list();
        let saved_count = saved.len();
        let restored = ui["last_thread"]
            .as_str()
            .and_then(|id| saved.iter().position(|m| m.id == id))
            .map(|ix| saved.remove(ix));
        // A session saved without a view has no permission mode of its own; the current default applies.
        let restored_view = restored
            .as_ref()
            .is_some_and(|m| Session::view(&m.id).ok().flatten().is_some());
        let fresh = restored.is_none();
        let mut threads = vec![Thread::new(restored.unwrap_or(meta), fresh, cfg.mode)];
        threads.extend(
            saved
                .into_iter()
                .map(|m| Thread::new(m, false, cfg.mode))
                // Empty composers are not history; only the remembered thread keeps its draft.
                .filter(|t| !t.unstarted()),
        );
        if let Some(e) = &cfg_error {
            threads[0]
                .items
                .push(Item::Error(format!("設定を読み込めません: {e}").into()));
        }
        let hermes_profiles = match hermes::discover() {
            Ok(profiles) => profiles,
            Err(e) => {
                threads[0].items.push(Item::Error(
                    format!("Hermesプロファイルを検出できません: {e:#}").into(),
                ));
                vec![]
            }
        };
        let mut this = Workspace {
            cfg: Arc::new(cfg),
            cfg_error,
            rt,
            list: ListState::new(threads[0].items.len(), ListAlignment::Bottom, px(600.)),
            threads,
            active: 0,
            focus: cx.focus_handle(),
            sidebar_focus: cx.focus_handle(),
            conversation_focus: cx.focus_handle(),
            diff_focus: cx.focus_handle(),
            find: Default::default(),
            nav: Default::default(),
            undo: Default::default(),
            quit: Default::default(),
            input,
            show_diff: false,
            expanded_groups: Default::default(),
            files_panel: None,
            pr_panel: None,
            diff: vec![],
            diff_generation: 0,
            diff_source: greencode_core::git::DiffSource::default(),
            diff_commits: vec![],
            picking_commit: false,
            text_prompt: None,
            synced: Default::default(),
            palette: None,
            model_picker: None,
            login_palette: false,
            registry_entries: None,
            palette_accounts: Vec::new(),
            palette_logins: Vec::new(),
            palette_effort: Vec::new(),
            palette_skills: Vec::new(),
            palette_scroll: ScrollHandle::default(),
            settings_scroll: ScrollHandle::default(),
            settings_focus: cx.focus_handle(),
            settings_tab: 0,
            show_update_panel: false,
            update_channel: Channel::default(),
            update_status: UpdateStatus::Unchecked,
            palette_ix: 0,
            fetched_models: vec![],
            found: vec![],
            search_gen: 0,
            draft_save_gen: 0,
            text_flush_pending: false,
            import_checked: None,
            git_generation: 0,
            groups_cache: Default::default(),
            antigravity_install: None,
            ui,
            custom_themes: greencode_core::theme::load_all(),
            theme_edit: None,
            voice_recording: None,
            snapshot: None,
            webhooks: Arc::new(std::sync::Mutex::new(vec![])),
            webhook_server: None,
            workflow_engine: None,
            settings: None,
            secrets: secrets::Secrets::new(window, cx),
            advanced: advanced::Advanced::new(window, cx),
            connection_name: None,
            connection_fields: None,
            repos: Default::default(),
            storage: storage::Storage {
                field: cx
                    .new(|cx| InputState::new(window, cx).placeholder("絶対パス（空欄は既定）")),
                error: None,
                candidates: vec![],
                busy: false,
            },
            agent_versions: Default::default(),
            agent_versions_generation: 0,
            welcome: None,
            recall: None,
            terminals: TerminalSet::default(),
            handoffs: HashSet::new(),
            handoff_diverged: HashSet::new(),
            terminal_visible: false,
            branch: String::new(),
            branches: vec![],
            prs: vec![],
            git_status: vec![],
            show_git: false,
            backend_models: greencode_core::cached_backend_models(),
            backend_options: if cfg!(test) {
                Default::default()
            } else {
                greencode_core::cached_backend_options()
            },
            project_models: Default::default(),
            probed: Default::default(),
            provider_form: None,
            queue_edit: None,
            oauth_form: None,
            key_form: None,
            account_form: None,
            cite_form: None,
            cite_text: String::new(),
            usage: None,
            workflows: None,
            device_form: None,
            commands: vec![],
            editor_text: None,
            released: vec![],
            defer_gen: 0,
            agent_spawner: spawn_agent,
            files: None,
            mentions: Default::default(),
            hermes_profiles,
            importable: vec![],
            show_archived: false,
            _subs: subs,
        };
        if let Some(policy) = &this.cfg.policy {
            for (key, value, locked) in [
                (
                    "suggest_next",
                    policy
                        .settings
                        .suggest_next
                        .as_ref()
                        .map(|s| json!(s.value)),
                    policy
                        .settings
                        .suggest_next
                        .as_ref()
                        .is_some_and(|s| s.locked),
                ),
                (
                    "voice_provider",
                    policy
                        .settings
                        .voice_provider
                        .as_ref()
                        .map(|s| json!(s.value)),
                    policy
                        .settings
                        .voice_provider
                        .as_ref()
                        .is_some_and(|s| s.locked),
                ),
                (
                    "sandbox",
                    policy
                        .settings
                        .sandbox
                        .as_ref()
                        .filter(|s| s.locked)
                        .map(|s| json!(s.value)),
                    true,
                ),
                (
                    "mode",
                    policy
                        .settings
                        .default_mode
                        .as_ref()
                        .filter(|s| s.locked)
                        .map(|s| json!(s.value)),
                    true,
                ),
            ] {
                if let Some(value) = value {
                    if locked || this.ui.get(key).is_none() {
                        this.ui[key] = value;
                    }
                }
            }
        }
        // Settings chosen in earlier runs.
        let submit_on_enter = this.ui["send_shortcut"] != "cmd+enter";
        this.input
            .update(cx, |s, cx| s.set_submit_on_enter(submit_on_enter, cx));
        this.apply_theme(window, cx);
        this.watch_environment_theme(window, cx);
        // Scheduled prompts are checked twice a minute while the app runs. Not in UI tests, which drive time themselves.
        if !cfg!(test) {
            cx.spawn(async move |this, cx| loop {
                cx.background_executor()
                    .timer(std::time::Duration::from_secs(30))
                    .await;
                if this
                    .update(cx, |this, cx| {
                        this.auto_settle(now());
                        this.run_due_schedules(now(), cx);
                        this.run_due_reminders(now(), cx)
                    })
                    .is_err()
                {
                    break;
                }
            })
            .detach();
        }
        if !cfg!(test) {
            cx.spawn(async move |this, cx| loop {
                cx.background_executor()
                    .timer(std::time::Duration::from_secs(600))
                    .await;
                if this
                    .update(cx, |this, cx| {
                        this.refresh_thread_prs(cx);
                        this.sync_default_branch(false, cx)
                    })
                    .is_err()
                {
                    break;
                }
            })
            .detach();
        }
        // Threads moved by hand keep their place; ones the saved order does not know stay newest first on top.
        let order: Vec<&str> = this.ui["order"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .collect();
        this.threads[1..].sort_by_key(|t| order.iter().position(|id| *id == t.meta.id));
        if welcome::first_run(&this.ui, saved_count) {
            this.open_welcome(cx);
        }
        advanced::apply_ui(Arc::make_mut(&mut this.cfg), &this.ui);
        if let Some(mode) = serde_json::from_value::<Mode>(this.ui["mode"].clone())
            .ok()
            .filter(|m| {
                this.cfg
                    .policy
                    .as_ref()
                    .is_none_or(|p| p.check_mode(*m).is_ok())
            })
        {
            Arc::make_mut(&mut this.cfg).mode = mode;
            if (fresh || !restored_view) && this.threads[0].items.is_empty() {
                this.threads[0].perms.set_mode(mode);
            }
        }
        if fresh {
            if let Some(model) = this.ui["model"].as_str() {
                this.threads[0].meta.model = model.into();
            }
        }
        if let Some(on) = this.ui["sandbox"].as_bool() {
            greencode_core::tools::SANDBOX.store(on, std::sync::atomic::Ordering::Relaxed);
        }
        let location = this.ui["worktree_dir"].as_str().unwrap_or("").to_string();
        this.storage
            .field
            .update(cx, |s, cx| s.set_value(location, window, cx));
        if !cfg!(test) && this.ui["auto_cleanup"] == true {
            this.cleanup(false, true, cx);
        }
        this.activate(0, window, cx);
        this.refresh_thread_prs(cx);
        this.start_webhooks(cx);
        this.start_workflow_engine(cx);
        this.snapshot_start(window, cx);
        this.refresh_models(cx);
        this
    }

    /// Writes the composer draft after a short pause instead of on every keystroke.
    fn queue_draft_save(&mut self, cx: &mut Context<Self>) {
        self.draft_save_gen = self.draft_save_gen.wrapping_add(1);
        let generation = self.draft_save_gen;
        let id = self.thread().meta.id.clone();
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(300))
                .await;
            this.update(cx, |this, cx| {
                if this.draft_save_gen != generation {
                    return;
                }
                let saved = this
                    .threads
                    .iter()
                    .find(|t| t.meta.id == id)
                    .map(Thread::save);
                if let Some(Err(e)) = saved {
                    this.fail(e);
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn save_ui(&mut self) {
        let mut saved_ui = self.ui.clone();
        if let Some(p) = &self.cfg.policy {
            let original: Value = std::fs::read(greencode_core::home().join("ui.json"))
                .ok()
                .and_then(|b| serde_json::from_slice(&b).ok())
                .unwrap_or_else(|| json!({}));
            for (key, locked) in [
                (
                    "mode",
                    p.settings.default_mode.as_ref().is_some_and(|s| s.locked),
                ),
                (
                    "sandbox",
                    p.settings.sandbox.as_ref().is_some_and(|s| s.locked),
                ),
                (
                    "suggest_next",
                    p.settings.suggest_next.as_ref().is_some_and(|s| s.locked),
                ),
                (
                    "voice_provider",
                    p.settings.voice_provider.as_ref().is_some_and(|s| s.locked),
                ),
            ] {
                if locked {
                    if let Some(value) = original.get(key) {
                        saved_ui[key] = value.clone();
                    } else if let Some(object) = saved_ui.as_object_mut() {
                        object.remove(key);
                    }
                }
            }
        }
        let saved = std::fs::create_dir_all(greencode_core::home()).and_then(|_| {
            std::fs::write(
                greencode_core::home().join("ui.json"),
                serde_json::to_vec_pretty(&saved_ui).unwrap_or_default(),
            )
        });
        if let Err(e) = saved {
            self.fail(format!("設定を保存できません: {e}"));
        }
    }

    /// Remembers the thread to reopen on the next launch, writing only when it changes.
    fn remember_thread(&mut self) {
        let id = self.thread().meta.id.clone();
        if self.ui["last_thread"].as_str() == Some(id.as_str()) {
            return;
        }
        self.ui["last_thread"] = json!(id);
        self.save_ui();
    }

    /// Runs `command` in the active project off the UI thread; output goes to the terminal drawer or the thread.
    fn run_shell(&mut self, command: String, to_terminal: bool, cx: &mut Context<Self>) {
        let (cwd, id) = (
            self.thread().meta.cwd.clone(),
            self.thread().meta.id.clone(),
        );
        let target = self.active_terminal().map(|t| (cwd.clone(), t.number));
        cx.spawn(async move |this, cx| {
            let shown = command.clone();
            let out = cx
                .background_executor()
                .spawn(async move {
                    std::process::Command::new("sh")
                        .arg("-c")
                        .arg(&command)
                        .current_dir(cwd)
                        .stdin(std::process::Stdio::null())
                        .output()
                })
                .await;
            let text = match out {
                Ok(o) => format!(
                    "$ {shown}\n{}{}",
                    String::from_utf8_lossy(&o.stdout),
                    String::from_utf8_lossy(&o.stderr)
                ),
                Err(e) => format!("$ {shown}\n{e}"),
            };
            this.update(cx, |this, cx| {
                if to_terminal {
                    if let Some((cwd, number)) = target {
                        if let Some(term) = this.terminals.by_number_mut(&cwd, number) {
                            term.open = false;
                            term.lines
                                .extend(text.lines().map(|l| SharedString::from(l.to_string())));
                            terminal::trim_scrollback(&mut term.lines);
                        }
                    }
                } else if let Some(ix) = this.threads.iter().position(|t| t.meta.id == id) {
                    this.threads[ix].items.push(Item::Notice(
                        text.chars().take(6000).collect::<String>().into(),
                    ));
                    if ix == this.active {
                        this.list.reset(this.threads[ix].items.len());
                    }
                }
                this.refresh_git(cx);
                this.refresh_diff(cx);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Current and local branches of the active project; empty outside a repository.
    fn refresh_git(&mut self, cx: &mut Context<Self>) {
        self.refresh_repos(cx);
        let cwd = self.thread().meta.cwd.clone();
        self.git_generation = self.git_generation.wrapping_add(1);
        let generation = self.git_generation;
        // Three git processes can take a while on a large repository; keep the UI responsive.
        cx.spawn(async move |this, cx| {
            let (branch, branches, status) = cx
                .background_executor()
                .spawn(async move {
                    let git = |args: &[&str]| {
                        std::process::Command::new("git")
                            .arg("-C")
                            .arg(&cwd)
                            .args(args)
                            .stderr(std::process::Stdio::null())
                            .output()
                            .ok()
                            .filter(|o| o.status.success())
                            .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
                            .unwrap_or_default()
                    };
                    (
                        git(&["branch", "--show-current"]).trim().to_string(),
                        git(&["branch", "--format=%(refname:short)"]),
                        git(&["status", "--porcelain=v1", "-z", "--untracked-files=all"]),
                    )
                })
                .await;
            this.update(cx, |this, cx| {
                if this.git_generation != generation {
                    return;
                }
                this.branch = branch;
                this.branches = branches.lines().map(String::from).collect();
                this.git_status = parse_git_status(&status);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn git_path_action(&mut self, paths: Vec<String>, stage: bool, cx: &mut Context<Self>) {
        let cwd = self.thread().meta.cwd.clone();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    let mut command = std::process::Command::new("git");
                    command
                        .current_dir(cwd)
                        .arg("--literal-pathspecs")
                        .arg(if stage { "add" } else { "reset" })
                        .arg("--")
                        .args(paths);
                    command.output()
                })
                .await;
            this.update(cx, |this, cx| {
                match result {
                    Ok(output) if output.status.success() => {
                        this.refresh_git(cx);
                        this.refresh_diff(cx);
                    }
                    Ok(output) => {
                        this.fail(String::from_utf8_lossy(&output.stderr).trim().to_string())
                    }
                    Err(e) => this.fail(format!("Git 操作に失敗しました: {e}")),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn active_terminal(&self) -> Option<&Term> {
        self.terminals.active(&self.thread().meta.cwd).filter(|t| {
            t.private
                .as_ref()
                .is_none_or(|id| *id == self.thread().meta.id)
        })
    }
    fn active_terminal_mut(&mut self) -> Option<&mut Term> {
        let cwd = self.thread().meta.cwd.clone();
        let id = self.thread().meta.id.clone();
        self.terminals
            .active_mut(&cwd)
            .filter(|t| t.private.as_ref().is_none_or(|owner| *owner == id))
    }

    fn write_terminal(&self, bytes: Vec<u8>) {
        let Some(stdin) = self.active_terminal().and_then(|t| t.stdin.clone()) else {
            return;
        };
        self.rt.spawn(async move {
            let mut stdin = stdin.lock().await;
            let _ = stdin.write_all(&bytes).await;
            let _ = stdin.flush().await;
        });
    }

    fn stop_terminal(&mut self) {
        let cwd = self.thread().meta.cwd.clone();
        if let Some(mut term) = self.terminals.remove_active(&cwd) {
            if let Err(e) = term.save(&cwd) {
                self.fail(format!("ターミナルの出力を保存できません: {e}"));
            }
            #[cfg(unix)]
            if let Some(process) = term.process.take() {
                self.rt.spawn(async move {
                    let _ = process.lock().await.kill().await;
                });
            }
            #[cfg(windows)]
            drop(term.process.take());
        }
        if self.active_terminal().is_none() {
            self.terminal_visible = false;
        }
    }

    fn open_terminal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.terminal_visible {
            self.terminal_visible = false;
            self.input.update(cx, |s, cx| s.focus(window, cx));
        } else {
            self.terminal_visible = true;
            if self.active_terminal().is_none() {
                self.new_terminal(window, cx);
            }
        }
        cx.notify();
    }
    fn handoff(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let meta = self.thread().meta.clone();
        if self.cfg.policy.is_some()
            || self.cfg.sandbox
            || greencode_core::tools::SANDBOX.load(std::sync::atomic::Ordering::Relaxed)
        {
            self.fail("管理ポリシーまたはサンドボックスが有効な間は端末 UI に引き継げません");
            return;
        }
        if self.thread().perms.mode() == Mode::Plan {
            self.fail("プランモードでは端末 UI に引き継げません");
            return;
        }
        // shortcut: require a verified empty process tree before supporting session handoff on Windows.
        if cfg!(windows) {
            self.fail("Windows では Pi / OMP の端末 UI に引き継げません");
            return;
        }
        if self.handoffs.contains(&meta.id) || greencode_core::handoff::is_open(&meta.id) {
            self.fail("端末 UI への引き継ぎが未完了です");
            return;
        }
        if self.thread().status != Status::Idle {
            self.fail("実行中のターンが終わってから引き継いでください");
            return;
        }
        self.handoffs.insert(meta.id.clone());
        let reply = if let Some(cmd) = &self.thread().cmd {
            let (tx, rx) = oneshot::channel();
            if cmd.send(Command::Handoff(tx)).is_err() {
                self.handoffs.remove(&meta.id);
                self.fail("エージェントを停止できません");
                return;
            }
            Some(rx)
        } else {
            None
        };
        let cfg = self.cfg.clone();
        cx.spawn_in(window, async move |this, cx| {
            let result = match reply {
                Some(rx) => rx
                    .await
                    .unwrap_or_else(|_| Err("エージェントの停止を確認できません".into())),
                None => Ok(()),
            };
            if let Err(e) = result {
                let _ = this.update_in(cx, |this, _, cx| {
                    this.handoffs.remove(&meta.id);
                    this.fail(format!("引き継げません: {e}"));
                    cx.notify();
                });
                return;
            }
            let result =
                greencode_core::handoff::prepare(&cfg, &meta).map_err(|e| format!("{e:#}"));
            let _ = this.update_in(cx, |this, window, cx| {
                if let Some(t) = this.threads.iter_mut().find(|t| t.meta.id == meta.id) {
                    t.cmd = None;
                }
                match result {
                    Ok(launch) => this.start_terminal(Some((launch, meta)), window, cx),
                    Err(e) => {
                        this.handoffs.remove(&meta.id);
                        this.fail(format!("引き継げません: {e}"));
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn adopt_handoff(&mut self, cx: &mut Context<Self>) {
        let meta = self.thread().meta.clone();
        if !self.handoff_diverged.contains(&meta.id) || self.thread().dialog.is_some() {
            self.fail("安全に確認できる分岐した引き継ぎがありません");
            return;
        }
        let (tx, rx) = oneshot::channel();
        self.threads[self.active].dialog = Some((
            json!({
                "method": "confirm",
                "title": "端末側の履歴を採用しますか？",
                "message": "現在の会話をバックアップし、端末側の履歴で置き換えます。既存の取り消しポイントは引き継がれません。"
            }),
            tx,
        ));
        let cfg = self.cfg.clone();
        cx.spawn(async move |this, cx| {
            let confirmed = rx.await.ok().is_some_and(|v| v["confirmed"] == true);
            let result = confirmed
                .then(|| greencode_core::handoff::adopt(&cfg, &meta).map_err(|e| format!("{e:#}")));
            let _ = this.update(cx, |this, cx| {
                if let Some(ix) = this.threads.iter().position(|t| t.meta.id == meta.id) {
                    this.threads[ix].status = Status::Idle;
                    if let Some(result) = result {
                        match result {
                            Ok(n) => {
                                this.handoff_diverged.remove(&meta.id);
                                this.threads[ix].loaded = false;
                                if ix == this.active {
                                    if let Ok((_, messages)) = Session::load(&meta.id) {
                                        this.threads[ix].items = thread_view::items_from(&messages);
                                        this.threads[ix].loaded = true;
                                        this.list.reset(this.threads[ix].items.len());
                                    }
                                }
                                this.notice(format!("端末側の履歴 {n} 件を採用しました"));
                            }
                            Err(e) => this.fail(format!("履歴を採用できません: {e}")),
                        }
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn new_terminal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.start_terminal(None, window, cx);
    }

    fn start_terminal(
        &mut self,
        launch: Option<(greencode_core::handoff::Launch, Meta)>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let cwd = launch.as_ref().map_or_else(
            || self.thread().meta.cwd.clone(),
            |(launch, _)| launch.cwd.clone(),
        );
        let number = self.terminals.next_number(&cwd);
        let mut process = None;
        let mut stdin = None;
        let mut master = None;
        let mut receiver = None;
        #[allow(unused_mut)]
        let mut private_kill = None;
        #[allow(unused_mut)]
        let mut private_wait: Option<tokio::task::JoinHandle<Result<std::process::ExitStatus, String>>> = None;
        if !cfg!(test) || launch.is_some() {
            #[cfg(unix)]
            let (fd, stdout, input) = {
                let launched = terminal_command(&cwd, launch.as_ref().map(|(launch, _)| launch))
                    .and_then(|(mut command, fd, stdout, input)| {
                        // tokio::process requires a runtime even on the GPUI main thread.
                        let _guard = self.rt.enter();
                        Ok((command.spawn()?, fd, stdout, input))
                    });
                let (child, fd, stdout, input) = match launched {
                    Ok(pty) => pty,
                    Err(e) => {
                        if let Some((_, meta)) = &launch {
                            let _ = greencode_core::handoff::abandon(meta);
                            self.handoffs.remove(&meta.id);
                        }
                        self.fail(format!("PTY を起動できません: {e}"));
                        return;
                    }
                };
                if launch.is_some() {
                    let (kill, rx) = oneshot::channel();
                    private_kill = Some(kill);
                    private_wait = Some(self.rt.spawn(terminal::supervise(child, rx)));
                } else {
                    process = Some(Arc::new(tokio::sync::Mutex::new(child)));
                }
                (fd, stdout, input)
            };
            #[cfg(windows)]
            let (fd, stdout, input) = match terminal::windows_pty(&self.rt, &cwd, TERM_SIZE) {
                Ok((child, fd, stdout, input)) => {
                    process = Some(child);
                    (fd, stdout, input)
                }
                Err(e) => {
                    self.fail(format!("PTY を起動できません: {e}"));
                    return;
                }
            };
            master = Some(fd);
            stdin = Some(Arc::new(tokio::sync::Mutex::new(input)));
            let (tx, rx) = mpsc::unbounded_channel();
            self.rt.spawn_blocking(move || terminal_output(stdout, tx));
            receiver = Some(rx);
        }
        let state = cx.new(|cx| InputState::new(window, cx).placeholder("PTY に入力して ⏎"));
        let stdin_for_input = stdin.clone();
        let handle = self.rt.handle().clone();
        let private = launch.is_some();
        let sub = cx.subscribe_in(
            &state,
            window,
            move |_this, state, ev: &InputEvent, window, cx| {
                if private && matches!(ev, InputEvent::Change) {
                    let text = state.read(cx).value().to_string();
                    if !text.is_empty() {
                        state.update(cx, |s, cx| s.set_value("", window, cx));
                        if let Some(stdin) = stdin_for_input.clone() {
                            handle.spawn(async move {
                                let _ = stdin.lock().await.write_all(text.as_bytes()).await;
                            });
                        }
                    }
                } else if !private && matches!(ev, InputEvent::PressEnter { .. }) {
                    let text = state.read(cx).value().to_string();
                    state.update(cx, |s, cx| s.set_value("", window, cx));
                    if let Some(stdin) = stdin_for_input.clone() {
                        handle.spawn(async move {
                            let mut stdin = stdin.lock().await;
                            let _ = stdin.write_all(text.as_bytes()).await;
                            let _ = stdin.write_all(b"\r").await;
                            let _ = stdin.flush().await;
                        });
                    }
                }
            },
        );
        let mut term = Term::new(number, state, sub, process, stdin, master, &cwd);
        if launch.is_some() {
            term.input.update(cx, |s, cx| s.focus(window, cx));
        }
        if let (Some((_, meta)), Some(kill)) = (&launch, private_kill) {
            term = term.into_private(meta.id.clone(), kill, [220, 220, 220], [0, 0, 0]);
        }
        self.terminals.insert(cwd.clone(), term);
        self.terminal_visible = true;
        if let Some(mut rx) = receiver {
            cx.spawn(async move |this, cx| {
                while let Some(text) = rx.recv().await {
                    if this
                        .update(cx, |this, cx| {
                            if let Some(term) = this.terminals.by_number_mut(&cwd, number) {
                                let reply = term.feed(&text);
                                if !reply.is_empty() {
                                    if let Some(stdin) = term.stdin.clone() {
                                        this.rt.spawn(async move {
                                            let _ = stdin.lock().await.write_all(&reply).await;
                                        });
                                    }
                                }
                                cx.notify();
                            }
                        })
                        .is_err()
                    {
                        break;
                    }
                }
            })
            .detach();
        }
        if let (Some((_, meta)), Some(wait)) = (launch, private_wait) {
            let cfg = self.cfg.clone();
            let finish_meta = meta.clone();
            let task = self.rt.spawn(async move {
                let status = wait.await.map_err(|e| e.to_string()).and_then(|r| r);
                match status {
                    Ok(status) => Ok((
                        status,
                        greencode_core::handoff::finish(&cfg, &finish_meta)
                            .map_err(|e| format!("{e:#}")),
                    )),
                    Err(e) => Err(e),
                }
            });
            cx.spawn(async move |this, cx| {
                let result = task.await.unwrap_or_else(|e| Err(e.to_string()));
                let _ = this.update(cx, |this, cx| {
                    if result.is_ok() { this.handoffs.remove(&meta.id); }
                    if let Some(ix) = this.threads.iter().position(|t| t.meta.id == meta.id) {
                        this.threads[ix].cmd = None;
                        this.threads[ix].loaded = false;
                        if ix == this.active {
                            if let Ok((_, messages)) = Session::load(&meta.id) {
                                this.threads[ix].items = thread_view::items_from(&messages);
                                this.threads[ix].loaded = true;
                                this.list.reset(this.threads[ix].items.len());
                            }
                        }
                    }
                    match result {
                        Ok((_, Ok(greencode_core::handoff::Outcome::Unchanged))) => this.notice("端末 UI から戻りました"),
                        Ok((_, Ok(greencode_core::handoff::Outcome::Appended(n)))) => this.notice(format!("端末 UI から {n} 件を取り込みました")),
                        Ok((_, Ok(greencode_core::handoff::Outcome::Diverged(reason)))) => {
                            this.handoff_diverged.insert(meta.id.clone());
                            this.fail(format!("引き継ぎが分岐しました: {reason}。パレットから端末側の履歴を採用できます"));
                        }
                        Ok((_, Err(e))) | Err(e) => this.fail(format!("引き継ぎを完了できません: {e}")),
                    }
                    cx.notify();
                });
            }).detach();
        }
        cx.notify();
    }

    /// Asks every provider that has credentials (or needs none) for its models. Failures just leave the list shorter.
    fn refresh_models(&mut self, cx: &mut Context<Self>) {
        // No network from UI tests: the deterministic test scheduler rejects wake-ups from tokio threads.
        if cfg!(test) {
            return;
        }
        for (name, p) in &self.cfg.providers {
            if !greencode_core::auth::status(name, p).signed_in() {
                continue;
            }
            let (cfg, name) = (self.cfg.clone(), name.clone());
            let task = self
                .rt
                .spawn(async move { greencode_core::provider::list_models(&cfg, &name).await });
            cx.spawn(async move |this, cx| {
                let Ok(Ok(models)) = task.await else { return };
                let _ = this.update(cx, |this, cx| {
                    for m in models {
                        if !this.fetched_models.contains(&m) {
                            this.fetched_models.push(m);
                        }
                    }
                    cx.notify();
                });
            })
            .detach();
        }
    }

    fn models_for_project(
        &self,
        project: &std::path::Path,
    ) -> std::collections::BTreeMap<String, Vec<String>> {
        let mut models = self.backend_models.clone();
        models.remove("opencode");
        if let Some(ids) = self.project_models.get(project) {
            models.insert("opencode".into(), ids.clone());
        }
        for (name, over) in &self.cfg.backend_overrides {
            let list = models.entry(name.clone()).or_default();
            for model in &over.models {
                if !list.contains(model) {
                    list.push(model.clone());
                }
            }
        }
        models
    }

    fn load_project_models(&mut self) {
        let project = self.thread().project();
        self.project_models
            .entry(project.clone())
            .or_insert_with(|| {
                greencode_core::cached_project_backend_models(&project)
                    .remove("opencode")
                    .unwrap_or_default()
            });
    }

    /// Asks external backends for their models: all of them when `force`, otherwise those with nothing cached.
    fn refresh_backend_models(&mut self, force: bool, cx: &mut Context<Self>) {
        self.load_project_models();
        // No agent processes from UI tests.
        if cfg!(test) {
            return;
        }
        let cwd = self.thread().project();
        let cached = self.models_for_project(&cwd);
        for b in &self.cfg.backends {
            let scope = if b.name == "opencode" {
                cwd.clone()
            } else {
                PathBuf::new()
            };
            // Models cached from the legacy Antigravity server miss most of agy's list; probing upgrades it.
            let legacy = b.name == greencode_core::antigravity::BACKEND && greencode_core::antigravity::is_legacy_runtime(b);
            let known = self.probed.contains(&(b.name.clone(), scope.clone()))
                || (!legacy
                    && cached.get(&b.name).is_some_and(|models| !models.is_empty())
                    && self.backend_options.contains_key(&b.name)
                    // Caches written before model names were kept have no model option.
                    && (b.protocol != greencode_core::config::Protocol::Acp || self.cached_model_option(&b.name).is_some()));
            // Config resolves installed agents to an absolute path; skip the ones that are not installed.
            if (known && !force)
                || !b.command.first().is_some_and(|c| {
                    std::path::Path::new(c).is_absolute() && std::path::Path::new(c).exists()
                })
            {
                continue;
            }
            self.probed.insert((b.name.clone(), scope));
            let (cfg, name, cwd) = (self.cfg.clone(), b.name.clone(), cwd.clone());
            let task = self.rt.spawn({
                let name = name.clone();
                let cwd = cwd.clone();
                async move { greencode_core::backend_models(cfg, &name, cwd).await }
            });
            cx.spawn(async move |this, cx| {
                let Ok(models) = task.await else { return };
                let _ = this.update(cx, |this, cx| {
                    if !models.is_empty() {
                        if name == "opencode" {
                            this.project_models.insert(cwd, models);
                        } else {
                            this.backend_models.insert(name.clone(), models);
                        }
                    }
                    if let Some(options) = greencode_core::cached_backend_options().remove(&name) {
                        this.backend_options.insert(name, options);
                    }
                    cx.notify();
                });
            })
            .detach();
        }
    }

    /// Configured models first, then everything the providers report.
    fn models(&self) -> Vec<String> {
        let mut v = self.cfg.model_list();
        for model in &self.fetched_models {
            if !v.contains(model) {
                v.push(model.clone());
            }
        }
        let disabled = self.disabled("disabled_providers");
        v.retain(|m| {
            m.split_once('/').is_some_and(|(name, _)| {
                self.cfg
                    .policy
                    .as_ref()
                    .is_none_or(|p| p.provider_allowed(name))
                    && !disabled.contains(&name.to_string())
            })
        });
        v
    }

    fn update_app(&mut self, check_only: bool, channel: Channel, cx: &mut Context<Self>) {
        if self.update_status.locked() {
            if !self.show_update_panel {
                self.notice(self.update_status.message());
                cx.notify();
            }
            return;
        }
        self.update_channel = channel;
        self.update_status = if check_only { UpdateStatus::Checking } else { UpdateStatus::Installing };
        cx.notify();
        let configured = self.cfg.update_repo.clone();
        let task = self.rt.spawn(async move {
            let repo = update::repository(configured.as_deref(), None).map_err(|e| e.to_string())?;
            let executable = (!check_only).then(std::env::current_exe).transpose().map_err(|e| e.to_string())?;
            update::run(&repo, channel, executable.as_deref()).await.map_err(|e| e.to_string())
        });
        cx.spawn(async move |this, cx| {
            let result = task.await.map_err(|e| e.to_string()).and_then(|result| result);
            this.update(cx, |this, cx| {
                let url = result.as_ref().ok().filter(|result| !result.installed).map(|result| &result.url);
                this.update_status = match &result {
                    Ok(result) if result.installed => UpdateStatus::Installed(format!(
                        "{} から {} に更新しました。再起動すると新しい版を使います。",
                        result.current, result.latest
                    ).into()),
                    Ok(result) if result.available => UpdateStatus::Checked(format!(
                        "更新あり：{} → {}", result.current, result.latest
                    ).into()),
                    Ok(result) => {
                        let note = result.note.as_deref().map(|note| format!(" {note}")).unwrap_or_default();
                        UpdateStatus::Checked(format!(
                            "更新なし：現在のバージョンは {} です。{note}", result.current
                        ).into())
                    }
                    Err(error) => {
                        let action = if check_only { "更新の確認" } else { "更新" };
                        UpdateStatus::Failed(format!("{action}に失敗しました。{error}").into())
                    }
                };
                if !this.show_update_panel {
                    let message = this.update_status.message();
                    if matches!(this.update_status, UpdateStatus::Failed(_)) {
                        this.fail(message);
                    } else if let Some(url) = url {
                        this.notice(format!("{message} · {url}"));
                    } else {
                        this.notice(message);
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn notice(&mut self, text: impl Into<SharedString>) {
        let t = &mut self.threads[self.active];
        t.items.push(Item::Notice(text.into()));
        self.list.reset(t.items.len());
    }

    fn cancel_sub(&mut self, name: &str, cx: &mut Context<Self>) {
        // A delegated child runs on after its parent's turn, so it is stopped directly.
        if let Some(task) = greencode_core::delegate::find(&self.thread().meta.id, name) {
            let _ = greencode_core::delegate::cancel(&task);
            return;
        }
        if self.thread().status == Status::Running {
            if let Some(cmd) = &self.thread().cmd {
                let _ = cmd.send(Command::Remote(json!({"type":"sub_cancel","name":name})));
                return;
            }
        }
        self.notice("実行中のサブエージェントを停止できません");
        cx.notify();
    }

    fn steer_sub(&mut self, name: &str, text: &str, cx: &mut Context<Self>) {
        if let Some(task) = greencode_core::delegate::find(&self.thread().meta.id, name) {
            match greencode_core::delegate::steer(&task, text) {
                Ok(()) => self.notice(format!("→ {name}: {text}")),
                Err(e) => self.notice(format!("{name} に指示できません: {e:#}")),
            }
            cx.notify();
            return;
        }
        if self.thread().status == Status::Running {
            if let Some(cmd) = &self.thread().cmd {
                let _ = cmd.send(Command::Remote(
                    json!({"type":"sub_steer","name":name,"message":text}),
                ));
                self.notice(format!("→ {name}: {text}"));
                cx.notify();
                return;
            }
        }
        self.notice("実行中のサブエージェントに指示できません");
        cx.notify();
    }

    /// Shows a delegated child thread, following its live events while it runs.
    fn open_child(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let meta = match self.threads.iter().find(|t| t.meta.id == id) {
            Some(t) => t.meta.clone(),
            None => match Session::load(id) {
                Ok((meta, _)) => meta,
                Err(e) => match greencode_core::delegate::running_meta(id) {
                    Some(meta) => meta,
                    None => return self.fail(format!("スレッドを開けません: {e:#}")),
                },
            },
        };
        let ix = self.attach_thread(meta, cx);
        self.select(ix, window, cx);
    }

    /// Lists `meta`'s thread if it is new and follows a delegated run's live events; returns its index.
    fn attach_thread(&mut self, meta: Meta, cx: &mut Context<Self>) -> usize {
        let id = meta.id.clone();
        let ix = match self.threads.iter().position(|t| t.meta.id == id) {
            Some(ix) => ix,
            None => {
                let mode = self.thread().perms.mode();
                self.threads.insert(0, Thread::new(meta, false, mode));
                self.active += 1;
                0
            }
        };
        if let Some(mut rx) = greencode_core::delegate::take_view(&id) {
            self.threads[ix].status = Status::Running;
            cx.spawn(async move |this, cx| {
                while let Some(ev) = rx.recv().await {
                    if this.update(cx, |this, cx| this.on_event(&id, ev, cx)).is_err() {
                        break;
                    }
                }
            })
            .detach();
        }
        cx.notify();
        ix
    }

    /// Opens the palette filtered to `query`.
    /// Reloads the open pull requests in the background; the palette and header use the last result meanwhile.
    fn refresh_prs(&mut self, cx: &mut Context<Self>) {
        if cfg!(test) || self.branch.is_empty() {
            return;
        }
        let cwd = self.thread().meta.cwd.clone();
        let id = self.thread().meta.id.clone();
        cx.spawn(async move |this, cx| {
            let out = cx
                .background_executor()
                .spawn(async move {
                    std::process::Command::new("sh")
                        .arg("-c")
                        .arg("gh pr list --json number,title,headRefName --limit 30")
                        .current_dir(cwd)
                        .stdin(std::process::Stdio::null())
                        .stderr(std::process::Stdio::null())
                        .output()
                })
                .await;
            let prs = out
                .ok()
                .filter(|o| o.status.success())
                .map(|o| parse_prs(&o.stdout))
                .unwrap_or_default();
            this.update(cx, |this, cx| {
                if this.thread().meta.id == id {
                    this.prs = prs;
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    fn open_palette(&mut self, query: &str, window: &mut Window, cx: &mut Context<Self>) {
        if self.palette.is_none() {
            self.toggle_palette(window, cx);
        }
        if let Some((state, _)) = &self.palette {
            state.update(cx, |s, cx| s.set_value(query.to_string(), window, cx));
        }
    }

    /// Slash commands the GUI handles itself in native threads. False: not one of them.
    fn builtin(&mut self, text: &str, window: &mut Window, cx: &mut Context<Self>) -> bool {
        self.command(text, false, window, cx)
    }

    /// `any_backend`: run GUI commands even where the backend has commands of the same name (keybindings).
    fn command(
        &mut self,
        text: &str,
        any_backend: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let backend = self.thread().meta.backend.clone();
        let native = backend == "native";
        let Some(rest) = text.strip_prefix('/') else {
            return false;
        };
        let (name, arg) = rest
            .split_once(char::is_whitespace)
            .map_or((rest, ""), |(n, a)| (n, a.trim()));
        // The backend's own command of the same name wins over the GUI's, except for keybindings.
        if !any_backend
            && (self.thread().commands.iter().any(|command| command == name)
                || (native && skills::command(&self.thread().meta.cwd, &self.cfg, name).is_some())
                || (backend == "opencode"
                    && skills::opencode_commands(&self.thread().meta.cwd, &self.cfg)
                        .iter()
                        .any(|command| command == name)))
        {
            return false;
        }
        // `/clear` starts a new session in this thread; the backend's own command of that name returned above.
        if !any_backend && !native && name == "clear" {
            self.clear_session(cx);
            return true;
        }
        // `/new` opens a new thread. A real extension command of that name returned above.
        if !any_backend
            && !native
            && rpc_backend(&self.cfg, &backend)
            && name == "new"
        {
            self.run(Act::NewThread, window, cx);
            return true;
        }
        // `/goal` only needs prompts and turn ends, which Pi/OMP RPC provide; their own `/goal` is terminal-only.
        if !any_backend
            && !native
            && !matches!(name, "model" | "models")
            && !(name == "goal" && rpc_backend(&self.cfg, &backend))
        {
            return false;
        }
        match name {
            "to" if native => match arg.split_once(char::is_whitespace) {
                Some((name, text)) if !name.is_empty() && !text.trim().is_empty() => {
                    self.steer_sub(name, text.trim(), cx)
                }
                _ => self.notice("使い方: /to NAME 指示"),
            },
            "webhook" | "webhook-secret" | "webhook-rotate"
                if self
                    .cfg
                    .policy
                    .as_ref()
                    .is_some_and(|p| p.limits.webhooks == Some(false)) =>
            {
                self.notice("管理ポリシーで Webhook は無効です")
            }
            "webhook" if arg.is_empty() => self.open_palette("Webhook: ", window, cx),
            "webhook" => self.create_webhook(arg, cx),
            "webhook-secret" => self.webhook_secret(arg),
            "webhook-rotate" => self.rotate_webhook(arg.trim(), cx),
            "model" if arg.is_empty() => self.open_model_picker(window, cx),
            "models" if arg.is_empty() => self.open_model_picker(window, cx),
            "model" => match arg.split_once('/') {
                Some((provider, model))
                    if self.cfg.providers.contains_key(provider) && !model.is_empty() =>
                {
                    self.run(Act::Model(arg.into()), window, cx)
                }
                _ => self.fail(format!(
                    "`{arg}` は使えません。provider/model の形式で、provider は次のいずれか: {}",
                    self.cfg
                        .providers
                        .keys()
                        .cloned()
                        .collect::<Vec<_>>()
                        .join(", ")
                )),
            },
            "new" => self.run(Act::NewThread, window, cx),
            "clear" => self.clear_session(cx),
            "undo" => self.run(Act::Undo, window, cx),
            "fork" => self.run(Act::Fork, window, cx),
            "attach" => self.run(Act::Attach, window, cx),
            "sandbox" => self.run(Act::Sandbox, window, cx),
            "retry" => self.run(Act::Retry, window, cx),
            "mode" => match arg {
                "ask" => self.run(Act::Mode(Mode::Ask), window, cx),
                "auto-edit" => self.run(Act::Mode(Mode::AutoEdit), window, cx),
                "full" => self.run(Act::Mode(Mode::Full), window, cx),
                "plan" => self.run(Act::Mode(Mode::Plan), window, cx),
                _ => self.fail("使い方: /mode ask | auto-edit | full | plan"),
            },
            "plan" => {
                let next = if self.thread().perms.mode() == Mode::Plan {
                    Mode::Ask
                } else {
                    Mode::Plan
                };
                self.run(Act::Mode(next), window, cx);
                self.notice(format!("権限: {}", mode_label(next)));
            }
            "terminal" => self.run(Act::Terminal, window, cx),
            "stash" => self.run(Act::Stash, window, cx),
            "skill" if arg.is_empty() => self.open_palette("スキル: ", window, cx),
            "skill" => self.run(Act::Skill(arg.into()), window, cx),
            "palette" => self.toggle_palette(window, cx),
            "scratch" => self.run(Act::NewScratch, window, cx),
            "schedule!" => {
                self.schedule_once(arg, true);
            }
            "schedule" if self.schedule_once(arg, false) => {}
            "schedule"
                if arg.is_empty()
                    && self.ui["reminders"]
                        .as_array()
                        .is_some_and(|l| !l.is_empty()) =>
            {
                self.open_palette("予約: ", window, cx);
            }
            "schedule" => {
                let (interval, prompt) = arg.split_once(' ').unwrap_or((arg, ""));
                let secs = interval
                    .strip_suffix('m')
                    .map(|n| (n, 60))
                    .or(interval.strip_suffix('h').map(|n| (n, 3600)))
                    .or(interval.strip_suffix('d').map(|n| (n, 86400)))
                    .and_then(|(n, unit)| Some(n.parse::<u64>().ok().filter(|n| *n > 0)? * unit));
                match (arg, secs) {
                    ("", _) => {
                        let list: Vec<String> = self.ui["schedules"].as_array().into_iter().flatten()
                            .map(|s| {
                                let when = s["time"].as_str().map_or_else(|| format!("{} 分ごと", s["every"].as_u64().unwrap_or(0) / 60), |t| format!("定刻 {t}"));
                                let paused = if s["enabled"] == false { "（停止中）" } else { "" };
                                format!("{when}{paused} · {} · {}", s["project"].as_str().unwrap_or(""), s["prompt"].as_str().unwrap_or(""))
                            }).collect();
                        self.notice(if list.is_empty() { "スケジュールはありません。/schedule 30m プロンプト で追加（m・h・d）、/schedule clear で全削除".to_string() } else { list.join("\n") });
                    }
                    ("clear", _) => { self.ui["schedules"] = json!([]); self.save_ui(); self.notice("スケジュールをすべて削除しました"); }
                    (_, Some(secs)) if !prompt.trim().is_empty() => {
                        if !self.ui["schedules"].is_array() { self.ui["schedules"] = json!([]); }
                        let entry = json!({"every": secs, "prompt": prompt.trim(), "project": self.thread().project(), "next": now() + secs});
                        self.ui["schedules"].as_array_mut().unwrap().push(entry);
                        self.save_ui();
                        self.notice(format!("{} 分ごとに、このプロジェクトの新しいスレッドで実行します（GreenCode の起動中のみ）", secs / 60));
                    }
                    _ => self.fail("使い方: /schedule 30m プロンプト（m・h・d）、/schedule（一覧）、/schedule clear"),
                }
            }
            "side" | "btw" if !arg.is_empty() => self.run(Act::Side(arg.into()), window, cx),
            "rename" if !arg.is_empty() => {
                self.threads[self.active].meta.title = arg.chars().take(60).collect();
                if let Err(e) = self.thread().save() {
                    self.fail(e);
                }
            }
            "snooze" => self.run(
                Act::Snooze(arg.parse::<u64>().unwrap_or(1) * 3600),
                window,
                cx,
            ),
            "branch" if !arg.is_empty() => {
                self.run_shell(format!("git switch -c {}", shell_quote(arg)), false, cx)
            }
            "push" => self.run_shell("git push -u origin HEAD".into(), false, cx),
            "commit" if self.text_model().is_some() => self.generate_git(false, cx),
            "pr" if self.text_model().is_some() => self.generate_git(true, cx),
            "pr" => self.run_shell("gh pr create --fill".into(), false, cx),
            "titleprompt" => self.set_title_instructions(arg),
            "goal" => match arg {
                "" => {
                    let text = match &self.thread().goal {
                        Some((g, n)) => format!(
                            "ゴール{}: {g}（自動継続 {n} 回）",
                            if self.thread().goal_paused { "（一時停止中）" } else { "" }
                        ),
                        None => "ゴールは設定されていません。/goal 完了条件 で設定します".into(),
                    };
                    self.notice(text)
                }
                "clear" => {
                    let ix = self.active;
                    self.threads[ix].goal = None;
                    self.threads[ix].goal_paused = false;
                    self.drop_goal_continuations(ix);
                    self.notice("ゴールを解除しました")
                }
                "pause" => self.goal_pause(true, cx),
                "resume" => self.goal_pause(false, cx),
                goal => {
                    self.threads[self.active].goal = Some((goal.to_string(), 0));
                    self.threads[self.active].goal_paused = false;
                    let prompt = format!("{goal}\n\n{GOAL_SUFFIX}");
                    if self.thread().status == Status::Idle {
                        self.threads[self.active].queue_held = false;
                        self.start_prompt(prompt, cx);
                    }
                    // Next in line, so the running turn's goal continuation does not go first.
                    else {
                        self.threads[self.active].queue.push_front(prompt);
                        if let Some(edit) = &mut self.queue_edit {
                            edit.index += 1;
                        }
                    }
                }
            },
            "learn" if !arg.is_empty() => {
                let prompt = format!("Save this reusable lesson with the ext learn tool after asking for approval: {arg}");
                self.start_prompt(prompt, cx);
            }
            "update" => {
                let mut args = arg.split_whitespace().peekable();
                let check = args.next_if_eq(&"check").is_some();
                let channel = args.next().map(str::parse).transpose();
                match (channel, args.next()) {
                    (Ok(channel), None) => self.run(Act::Update(check, channel.unwrap_or_default()), window, cx),
                    _ => {
                        self.fail("使い方: /update [check] [stable|main]");
                        cx.notify();
                    }
                }
            }
            "export" => self.run(Act::Export, window, cx),
            "import" => self.import_session_paths(vec![PathBuf::from(arg)], window, cx),
            "effort" => self.run(
                Act::Effort((!arg.is_empty() && arg != "default").then(|| arg.to_string())),
                window,
                cx,
            ),
            "settings" => self.run(Act::Settings, window, cx),
            "theme" => {
                if arg.is_empty() {
                    self.open_palette("テーマ: ", window, cx);
                } else if let Some(choice) = Choice::parse(arg) {
                    if let Choice::Custom(slug) = &choice {
                        if !self.custom_themes.iter().any(|(s, _)| s == slug) {
                            self.fail(format!(
                                "テーマ「{slug}」がありません。~/.greencode/themes に JSON を置くか、パレットの「テーマを読み込む…」を使ってください"
                            ));
                            cx.notify();
                            return true;
                        }
                    }
                    self.run(Act::Theme(choice), window, cx);
                } else if let Some(mode) = themes::ThemeMode::parse(arg) {
                    self.run(Act::ThemeMode(mode), window, cx);
                } else {
                    self.fail("使い方: /theme [名前 | system | auto]");
                }
            }
            "diff" => self.run(Act::ToggleDiff, window, cx),
            "clone" => {
                if arg.is_empty() {
                    self.run(Act::CloneProject, window, cx);
                } else {
                    self.start_clone(arg.to_string(), cx);
                }
            }
            "project" => {
                if arg.is_empty() {
                    self.run(Act::NewProject, window, cx);
                } else {
                    self.start_new_project(arg.to_string(), window, cx);
                }
            }
            "publish" => self.publish_project(cx),
            "sync" => self.sync_default_branch(true, cx),
            "provider" => self.run(Act::AddProvider, window, cx),
            "login" => {
                if arg.split_whitespace().count() > 1 {
                    self.fail("使い方: /login [プロバイダ名]。APIキーは入力画面に入力してください");
                } else if arg.is_empty() {
                    self.open_palette("", window, cx);
                    self.login_palette = true;
                    cx.notify();
                } else if arg == "claude" {
                    self.run(Act::AnthropicLogin, window, cx);
                } else if arg == "copilot" || arg == "github-copilot" {
                    self.run(Act::CopilotLogin, window, cx);
                } else if arg == "chatgpt" || arg == "codex" {
                    self.run(Act::ChatGptLogin, window, cx);
                } else if arg == "vibe" || arg == "mistral-vibe" {
                    self.run(Act::MistralLogin, window, cx);
                } else if greencode_core::antigravity::is_alias(arg) {
                    self.run(Act::AntigravityLogin, window, cx);
                } else {
                    self.run(Act::LoginProvider(arg.into()), window, cx);
                }
            }
            "logout" => {
                if arg.is_empty() {
                    self.open_palette("ログアウト: ", window, cx);
                } else {
                    self.run(
                        Act::Logout(greencode_core::antigravity::canonical_backend(arg).into()),
                        window,
                        cx,
                    );
                }
            }
            "status" => {
                let t = self.thread();
                let cost = if t.cost > 0. {
                    format!("\nコスト: ${:.4}（起動後）", t.cost)
                } else {
                    String::new()
                };
                let text = format!("モデル: {}\nバックエンド: {}\n権限: {}\n思考の強さ: {}\nサンドボックス: {}\nプロジェクト: {}\nコンテキスト: {} トークン（入力 {} · キャッシュ読込 {} · 出力 {}）{cost}",
                    t.meta.model, t.meta.backend, mode_label(t.perms.mode()), self.cfg.effort.as_deref().unwrap_or("既定"),
                    if greencode_core::tools::SANDBOX.load(std::sync::atomic::Ordering::Relaxed) { "オン" } else { "オフ" },
                    t.project().display(), t.usage.context(), t.usage.input, t.usage.cache_read, t.usage.output);
                self.notice(text);
            }
            "help" => self.notice(HELP),
            "voice" => self.voice_command(arg, cx),
            "usage" => self.run(Act::Usage, window, cx),
            "usage-limits" => self.usage_limits_notice(cx),
            "usage-history" => self.run(Act::UsageHistory, window, cx),
            _ => return false,
        }
        cx.notify();
        true
    }

    fn thread(&self) -> &Thread {
        &self.threads[self.active]
    }

    fn fail(&mut self, e: impl std::fmt::Display) {
        let t = &mut self.threads[self.active];
        t.items.push(Item::Error(e.to_string().into()));
        self.list.reset(t.items.len());
    }

    /// Shows thread `ix`, loading it from disk if needed.
    fn activate(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        if self.active != ix {
            if let Some(panel) = &mut self.files_panel {
                panel.close_preview();
            }
        }
        self.active = ix;
        // Items are reloaded or replaced here; the grouping cache may no longer match.
        *self.groups_cache.get_mut() = Default::default();
        let t = &mut self.threads[ix];
        if !t.loaded {
            // A thread that was never prompted has no session file yet.
            if Session::open(&t.meta.id).is_some() {
                match thread_view::session_items(&t.meta.id) {
                    Ok(items) => t.items = items,
                    Err(e) => t.items = vec![Item::Error(e.into())],
                }
            }
            t.loaded = true;
        }
        self.list.reset(t.items.len());
        self.commands = command_list(&self.cfg, t);
        self.refresh_git(cx);
        self.recall = None;
        self.refresh_diff(cx);
        self.sync_default_branch(false, cx);
        let draft = self.thread().draft.clone();
        if self.terminal_visible && self.active_terminal().is_none() {
            self.new_terminal(window, cx);
        }
        self.input.update(cx, |s, cx| {
            set_input(s, draft, window, cx);
            s.focus(window, cx);
        });
        self.remember_thread();
        cx.notify();
    }

    fn select(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        if ix >= self.threads.len() || ix == self.active {
            return;
        }
        let from = self.threads[self.active].meta.id.clone();
        self.nav.visit(&from);
        self.finish_queue_edit(false, window, cx);
        let active = self.active;
        self.threads[active].draft = self
            .editor_text
            .take()
            .unwrap_or_else(|| self.input.read(cx).value().to_string());
        let saved = self.threads[active].save();
        if let Err(e) = saved {
            self.fail(e);
        }
        if self.threads[active].status == Status::Idle {
            self.threads[active].items = Vec::new();
            self.threads[active].loaded = false;
        }
        self.activate(ix, window, cx);
    }

    /// Deletes thread `ix` and its session/view. A worktree and branch stay on disk on purpose.
    fn delete_thread(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(t) = self.threads.get(ix) else {
            return;
        };
        if t.status != Status::Idle {
            self.fail("実行を停止してからスレッドを削除してください");
            cx.notify();
            return;
        }
        let removed_active = ix == self.active;
        let t = self.threads.remove(ix);
        Session::delete(&t.meta.id);
        if self.threads.is_empty() {
            let mut meta = Meta::new(t.project(), t.meta.model, "native".into());
            meta.subagent_model = self.cfg.subagent_model.clone();
            self.threads.push(Thread::new(meta, true, self.cfg.mode));
        }
        if ix < self.active {
            self.active -= 1;
        }
        if removed_active {
            self.active = self.active.min(self.threads.len() - 1);
            self.activate(self.active, window, cx);
        } else {
            cx.notify();
        }
    }

    fn new_thread(
        &mut self,
        project: PathBuf,
        in_worktree: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let cur = &self.thread().meta;
        let mut meta = Meta::new(project.clone(), cur.model.clone(), cur.backend.clone());
        meta.subagent_model = cur.subagent_model.clone();
        // A project's saved default wins over the selections of the thread being viewed.
        let default = self.ui["projects"][project.to_string_lossy().as_ref()].clone();
        if let (Some(model), Some(backend)) =
            (default["model"].as_str(), default["backend"].as_str())
        {
            (meta.model, meta.backend) = (model.into(), backend.into());
        }
        let chosen = serde_json::from_value(default["mode"].clone()).unwrap_or(self.cfg.mode);
        let mode = if self.cfg.policy.as_ref().is_some_and(|p| {
            p.check_mode(chosen).is_err()
                || p.settings.default_mode.as_ref().is_some_and(|s| s.locked)
        }) {
            self.cfg.mode
        } else {
            chosen
        };
        if in_worktree {
            match self.make_worktree(&project, &meta.id) {
                Ok(path) => {
                    meta.cwd = path;
                    meta.repo = Some(project);
                }
                Err(e) => return self.fail(e),
            }
        }
        self.threads.insert(0, Thread::new(meta, true, mode));
        self.active += 1;
        self.select(0, window, cx);
    }

    fn refresh_diff(&mut self, cx: &mut Context<Self>) {
        self.diff_generation = self.diff_generation.wrapping_add(1);
        let generation = self.diff_generation;
        self.diff.clear();
        if !self.show_diff && self.files_panel.is_none() {
            return;
        }
        let scope = self.diff_scope();
        let source = self.diff_source_scope();
        let id = self.thread().meta.id.clone();
        cx.spawn(async move |this, cx| {
            let diff = cx
                .background_executor()
                .spawn(async move {
                    if matches!(&source.source, greencode_core::git::DiffSource::Uncommitted) {
                        scope.diff()
                    } else {
                        source.diff()
                    }
                })
                .await;
            this.update(cx, |this, cx| {
                if this.apply_diff(generation, &id, &diff) {
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    fn apply_diff(&mut self, generation: u64, id: &str, diff: &str) -> bool {
        if self.diff_generation != generation || self.thread().meta.id != id {
            return false;
        }
        self.diff = diff
            .lines()
            .map(|l| SharedString::from(l.to_string()))
            .collect();
        true
    }

    /// A first `/command` must be matched against the backend's catalog, which is still arriving: hold it until discovery ends.
    /// Returns whether it was held. A timeout restores the draft rather than guessing command precedence.
    fn defer_until_discovered(&mut self, text: &str, cx: &mut Context<Self>) -> bool {
        self.discover_commands(cx);
        self.defer_gen += 1;
        let gen = self.defer_gen;
        let t = &mut self.threads[self.active];
        // Only a command the GUI would handle itself can be shadowed by the catalog; any other reaches the backend either way.
        let name = text
            .strip_prefix('/')
            .and_then(|rest| rest.split_whitespace().next())
            .unwrap_or("");
        let gui_owned =
            t.meta.backend == "native" || matches!(name, "model" | "models" | "new" | "clear");
        if !(gui_owned && t.warm && t.warmups > 0 && t.commands.is_empty()) {
            return false;
        }
        t.deferred = Some((text.to_string(), gen, t.meta.backend.clone()));
        let id = t.meta.id.clone();
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_secs(10))
                .await;
            this.update(cx, |this, cx| this.release_deferred(&id, Some(gen), cx))
                .ok();
        })
        .detach();
        true
    }

    /// `gen` identifies a timed-out deferral; `None` releases after discovery.
    fn release_deferred(&mut self, id: &str, gen: Option<u64>, cx: &mut Context<Self>) {
        let Some(ix) = self.threads.iter().position(|t| t.meta.id == id) else {
            return;
        };
        let t = &mut self.threads[ix];
        if t.deferred
            .as_ref()
            .is_some_and(|d| gen.is_none_or(|g| g == d.1))
        {
            let (text, _, backend) = t.deferred.take().unwrap();
            if gen.is_some() {
                t.items.push(Item::Error(
                    "コマンド一覧の取得がタイムアウトしました。入力を復元しました。".into(),
                ));
                if ix == self.active {
                    self.list.reset(t.items.len());
                }
                self.restore_text(ix, text, cx);
            } else {
                self.released.push((id.to_string(), text, backend));
                cx.notify();
            }
        }
    }

    /// Puts text back in a thread's composer without overwriting what is there.
    fn restore_text(&mut self, ix: usize, text: String, cx: &mut Context<Self>) {
        let current = if ix == self.active {
            self.editor_text
                .clone()
                .unwrap_or_else(|| self.input.read(cx).value().to_string())
        } else {
            self.threads[ix].draft.clone()
        };
        let next = if current.trim().is_empty() {
            text
        } else {
            format!("{current}\n{text}")
        };
        self.threads[ix].draft = next.clone();
        if ix == self.active {
            self.editor_text = Some(next);
        }
        cx.notify();
    }

    /// Dispatches a released `/command` against the catalog that arrived, unless its thread or backend changed meanwhile.
    fn submit_released(
        &mut self,
        id: &str,
        text: String,
        backend: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(ix) = self.threads.iter().position(|t| t.meta.id == id) else {
            return;
        };
        if ix != self.active || self.threads[ix].meta.backend != backend {
            return self.restore_text(ix, text, cx);
        }
        self.submit(text, window, cx);
    }

    /// Drops an agent that was only started for discovery, before its blank thread changes backend. The agent may still
    /// write runtime state for the old backend under this id (Pi `get_state`, ACP session start), which the next
    /// backend would reject, so the thread continues under a fresh id. A thread with a session file keeps its id.
    fn retire_warm_agent(&mut self, ix: usize) {
        let t = &mut self.threads[ix];
        if !t.warm {
            return;
        }
        (t.cmd, t.warm, t.warmups, t.commands, t.steering) = (None, false, 0, vec![], false);
        if Session::open(&t.meta.id).is_some() {
            return;
        }
        let old = std::mem::replace(
            &mut t.meta.id,
            Meta::new(PathBuf::new(), String::new(), String::new()).id,
        );
        match t.save() {
            Ok(()) => Session::delete(&old),
            Err(e) => self.fail(e),
        }
    }

    /// Applies a thinking level to the config, ui.json, and every live agent.
    fn set_effort(&mut self, level: Option<String>) {
        self.notice(format!(
            "思考の強さ: {}",
            level.as_deref().unwrap_or("既定")
        ));
        self.ui["effort"] = json!(level);
        self.save_ui();
        Arc::make_mut(&mut self.cfg).effort = level;
        for t in &self.threads {
            if let Some(cmd) = &t.cmd {
                let _ = cmd.send(Command::Config(self.cfg.clone()));
            }
        }
    }

    fn send(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.queue_edit.is_some() {
            self.finish_queue_edit(true, window, cx);
            return;
        }
        if self.handoffs.contains(&self.thread().meta.id)
            || greencode_core::handoff::is_open(&self.thread().meta.id)
        {
            self.fail("端末 UI への引き継ぎが未完了です");
            return;
        }
        let text = self.input.read(cx).value().trim().to_string();
        if over_limit(&text) {
            self.notice(format!(
                "メッセージは 120,000 文字までです（現在 {} 文字）。下書きはそのまま残しています",
                text.chars().count()
            ));
            return;
        }
        if self
            .thread()
            .dialog
            .as_ref()
            .is_some_and(|(request, _)| request["method"] == "ask")
        {
            self.answer_ask((!text.is_empty()).then_some(text), cx);
            self.input.update(cx, |s, cx| set_input(s, "", window, cx));
            return;
        }
        if self.thread().dialog.as_ref().is_some_and(|(request, _)| request["method"] == "secret") {
            // The secret is typed into the dialog's own masked field, not the composer.
            return self.secret_save(window, cx);
        }
        if let Some((request, _)) = &self.thread().dialog {
            let value = if request["method"] == "confirm" {
                // Only an explicit answer confirms or refuses; a draft typed meanwhile stays in the composer.
                match text.to_lowercase().as_str() {
                    "y" | "yes" | "はい" => json!({"confirmed": true}),
                    "n" | "no" | "いいえ" => json!({"confirmed": false}),
                    _ => {
                        // Enter already put a newline in the composer; keep the draft as typed.
                        self.input
                            .update(cx, |s, cx| set_input(s, &text, window, cx));
                        return self
                            .notice("確認待ちです。y / n で答えるか、esc で取り消してください");
                    }
                }
            } else {
                json!({"value": text})
            };
            // Clear while the dialog is still open so the answer never becomes the thread's draft.
            self.input.update(cx, |s, cx| set_input(s, "", window, cx));
            self.answer_dialog(value, cx);
            return;
        }
        if text.is_empty() && self.thread().images.is_empty() {
            return;
        }
        // A message held for command discovery is still in flight: keep this one in the composer.
        if self.thread().deferred.is_some() {
            return;
        }
        self.input.update(cx, |s, cx| set_input(s, "", window, cx));
        self.threads[self.active].draft.clear();
        self.threads[self.active].suggestion = None;
        if text.starts_with('/') && self.defer_until_discovered(&text, cx) {
            return;
        }
        self.submit(text, window, cx);
    }

    /// Runs a message the user submitted: GUI commands first, then `!shell`, otherwise a prompt.
    fn submit(&mut self, text: String, window: &mut Window, cx: &mut Context<Self>) {
        if self.builtin(&text, window, cx) {
            return;
        }
        // `!command` runs in the project without involving the model.
        if let Some(command) = text
            .strip_prefix('!')
            .map(str::trim)
            .filter(|c| !c.is_empty())
        {
            return self.run_shell(command.to_string(), false, cx);
        }
        self.threads[self.active].last_user_at = now();
        self.recall = None;
        if text.trim() == "/commit"
            && self.thread().meta.backend == "native"
            && !self.cfg.integration_opts.commit.enabled
        {
            self.notice("commit.enabled = false のため /commit は無効です".to_string());
            return;
        }
        let text = match text.as_str() {
            "/init" | "/review" | "/commit"
                if self.thread().meta.backend == "native"
                    && skills::command(&self.thread().meta.cwd, &self.cfg, &text[1..]).is_none() =>
            {
                match text.as_str() {
                    "/init" => INIT_PROMPT,
                    "/review" => REVIEW_PROMPT,
                    _ => COMMIT_PROMPT,
                }
                .to_string()
            }
            _ => text,
        };
        // `@thread:ID` and `#N` carry a short reference block; a provider command is sent verbatim.
        let text = if text.starts_with('/') {
            text
        } else {
            self.expand_mentions(text)
        };
        // Attached images ride along as paths, which the agent turns into image blocks. Taken here, after the
        // GUI command and `!shell` branches, so those leave the images in the composer for the next prompt.
        let mut text = text;
        for path in std::mem::take(&mut self.threads[self.active].images) {
            if !text.is_empty() {
                text.push(' ');
            }
            text.push_str(&path.display().to_string());
        }
        // Follow-up behavior "steer": a message sent during a run goes into that run when the agent supports it.
        if self.ui["follow_up_steer"] == true
            && self.thread().status == Status::Running
            && can_steer(&self.cfg, self.thread())
        {
            if let Some(cmd) = &self.thread().cmd {
                let _ = cmd.send(Command::Steer(text.clone()));
            }
            self.threads[self.active]
                .items
                .push(Item::User(text.into()));
            if let Err(e) = self.thread().save() {
                self.fail(e);
            }
            self.list.reset(self.thread().items.len());
            cx.notify();
            return;
        }
        if self.thread().status != Status::Idle {
            self.threads[self.active].queue.push_back(text);
            if let Err(e) = self.thread().save() {
                self.fail(e);
            }
            cx.notify();
            return;
        }
        self.threads[self.active].queue_held = false;
        self.start_prompt(text, cx);
    }

    /// `/schedule` and agents' `schedule_task`: starts every due scheduled prompt in a new background thread of its project.
    fn run_due_schedules(&mut self, now: u64, cx: &mut Context<Self>) {
        let mut due = vec![];
        for s in self.ui["schedules"].as_array_mut().into_iter().flatten() {
            let (Some(prompt), Some(project)) = (s["prompt"].as_str(), s["project"].as_str()) else {
                continue;
            };
            if s["enabled"] == false || s["next"].as_u64().unwrap_or(0) > now {
                continue;
            }
            let weekdays: Vec<u64> = s["weekdays"].as_array().into_iter().flatten().filter_map(Value::as_u64).collect();
            let next = match (s["every"].as_u64().filter(|e| *e > 0), s["time"].as_str()) {
                (Some(every), _) => now + every,
                (None, Some(time)) => match schedule::next_fixed(now, time, &weekdays, schedule::utc_offset()) {
                    Some(next) => next,
                    None => continue,
                },
                (None, None) => continue,
            };
            let title = s["title"].as_str().unwrap_or(prompt).chars().take(50).collect::<String>();
            let target = (s["backend"].as_str().map(str::to_string), s["model"].as_str().map(str::to_string));
            due.push((prompt.to_string(), PathBuf::from(project), title, target));
            s["next"] = json!(next);
        }
        if due.is_empty() {
            return;
        }
        self.save_ui();
        for (prompt, project, title, (backend, model)) in due {
            let backend = backend.unwrap_or_else(|| "native".into());
            let model = model.unwrap_or_else(|| if backend == "native" { self.cfg.model.clone() } else { "default".into() });
            let mut meta = Meta::new(project, model, backend);
            meta.subagent_model = self.cfg.subagent_model.clone();
            meta.title = format!("⏱ {title}");
            self.threads
                .insert(0, Thread::new(meta, true, self.cfg.mode));
            self.active += 1;
            self.start_in(0, prompt, cx);
        }
    }

    /// Starts the thread's agent (idempotent) and forwards its events to `on_event`.
    fn ensure_agent(&mut self, ix: usize, cx: &mut Context<Self>) {
        if self.threads[ix].cmd.is_some() {
            return;
        }
        let (mut cfg, perms, handle) = (
            self.cfg.clone(),
            self.threads[ix].perms.clone(),
            self.rt.handle().clone(),
        );
        if self.threads[ix].opencode_effort.is_some() {
            Arc::make_mut(&mut cfg).opencode_effort = self.threads[ix].opencode_effort.clone();
        }
        let spawn = self.agent_spawner;
        let meta = self.threads[ix].meta.clone();
        let (tx, mut rx) = spawn(cx, &handle, cfg, meta, perms);
        let t = &mut self.threads[ix];
        t.cmd = Some(tx);
        t.warmups = 0;
        t.steering = false;
        let id = t.meta.id.clone();
        cx.spawn(async move |this, cx| {
            while let Some(ev) = rx.recv().await {
                if this
                    .update(cx, |this, cx| this.on_event(&id, ev, cx))
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
    }

    /// Learns the active thread's slash commands before its first prompt, without a model call or a session file.
    fn discover_commands(&mut self, cx: &mut Context<Self>) {
        let ix = self.active;
        let t = &self.threads[ix];
        if self.handoffs.contains(&t.meta.id) || greencode_core::handoff::is_open(&t.meta.id) {
            return;
        }
        if t.cmd.is_some() || self.cfg_error.is_some() {
            return;
        }
        if let Some(policy) = &self.cfg.policy {
            if policy.check_mode(t.perms.mode()).is_err()
                || !policy.backend_allowed(&t.meta.backend)
            {
                return;
            }
        }
        // Native asks for its Lua/file commands; every external backend reports them while initialising.
        let control = if t.meta.backend == "native" {
            "get_commands"
        } else {
            "get_state"
        };
        self.ensure_agent(ix, cx);
        let t = &mut self.threads[ix];
        t.warm = true;
        t.warmups += 1;
        let _ = t
            .cmd
            .as_ref()
            .unwrap()
            .send(Command::Remote(json!({"type": control})));
    }

    fn start_prompt(&mut self, text: String, cx: &mut Context<Self>) {
        self.start_in(self.active, text, cx)
    }

    /// Palette `リセット時に再開`: queues the failed prompt as a reminder for the provider's limit reset.
    fn resume_at_reset(&mut self) {
        let t = self.thread();
        let (at, id, prompt) = (t.reset_at, t.meta.id.clone(), t.last_prompt.clone());
        let Some(prompt) = prompt.filter(|_| at > now()) else {
            self.notice("再開できるプロンプトまたはリセット時刻がありません");
            return;
        };
        if !self.ui["reminders"].is_array() {
            self.ui["reminders"] = json!([]);
        }
        self.ui["reminders"].as_array_mut().unwrap().push(json!({"thread": id, "at": at, "prompt": prompt}));
        self.save_ui();
        self.notice(format!("{} にこのスレッドへ再送します（GreenCode の起動中のみ）", clock_local(at)));
    }

    /// Sends `text` as the next turn of thread `ix`, starting its agent if needed.
    fn start_in(&mut self, ix: usize, text: String, cx: &mut Context<Self>) {
        // The delegated child's backend is owned by its parent's task; a second one would fork its history.
        if greencode_core::delegate::is_running(&self.threads[ix].meta.id) {
            self.restore_text(ix, text, cx);
            self.fail("委任タスクの実行中です。親スレッドの「指示」から送ってください");
            return;
        }
        if self.handoffs.contains(&self.threads[ix].meta.id)
            || greencode_core::handoff::is_open(&self.threads[ix].meta.id)
        {
            self.restore_text(ix, text, cx);
            self.fail("端末 UI への引き継ぎが未完了です");
            return;
        }
        self.threads[ix].last_prompt = Some(text.clone());
        self.threads[ix].reset_at = 0;
        if let Some(error) = &self.cfg_error {
            self.fail(format!("設定を読み込めません: {error}"));
            return;
        }
        if let Some(policy) = &self.cfg.policy {
            if policy.check_mode(self.threads[ix].perms.mode()).is_err()
                || !policy.backend_allowed(&self.threads[ix].meta.backend)
            {
                self.fail(format!(
                    "管理ポリシーで制限されています（{}）",
                    policy.organization
                ));
                return;
            }
        }
        self.record_checkpoint(ix);
        self.ensure_agent(ix, cx);
        let t = &mut self.threads[ix];
        t.warm = false;
        t.subs.clear();
        t.bg_wait = None;
        if t.meta.title.is_empty() {
            t.meta.title = thread_view::without_image_paths(&text).chars().take(60).collect();
        }
        let start = t.items.len();
        t.items.push(Item::User(text.clone().into()));
        // Show the images the prompt attaches, as the agent will send them.
        let images = greencode_core::context::attachments(&text, &t.project());
        t.items.extend(images.iter().filter_map(|b| match b {
            greencode_core::Block::Other(block) => thread_view::image_item(block),
            _ => None,
        }));
        t.status = Status::Running;
        t.started = Some(std::time::Instant::now());
        let n = t.items.len();
        let saved = t.save();
        if let Err(e) = saved {
            t.status = Status::Idle;
            self.fail(e);
            cx.notify();
            return;
        }
        let command = t.prompt_command(text);
        let _ = t.cmd.as_ref().unwrap().send(command);
        if ix == self.active {
            // Every row pushed above is new: the user message plus one preview per attached image.
            self.list.splice(start..start, n - start);
            self.list.scroll_to_end();
        }
        cx.notify();
    }

    fn on_event(&mut self, id: &str, ev: Event, cx: &mut Context<Self>) {
        let notify = self.ui["notify_off"] != true;
        let persist = matches!(&ev, Event::Done { .. })
            || matches!(&ev, Event::Remote { command, .. } if command == "get_state" || command == "acp_state");
        let is_text = matches!(&ev, Event::Text(_));
        let done = matches!(&ev, Event::Done { .. });
        // Orchestration requests that concern the workspace, not this thread's transcript.
        let ev = match ev {
            Event::Dialog { request, reply } if request["method"] == "schedule" => {
                let _ = reply.send(self.schedule_request(&request));
                return;
            }
            Event::Remote { command, data } if command == "thread_opened" => {
                if let Ok(meta) = serde_json::from_value::<Meta>(data["meta"].clone()) {
                    self.attach_thread(meta, cx);
                }
                return;
            }
            Event::Remote { command, data } if command == "thread_renamed" => {
                if let Some(t) = self.threads.iter_mut().find(|t| data["id"] == t.meta.id.as_str()) {
                    t.meta.title = data["title"].as_str().unwrap_or_default().to_string();
                }
                cx.notify();
                return;
            }
            ev => ev,
        };
        let Some(ix) = self.threads.iter().position(|t| t.meta.id == id) else {
            return;
        };
        let visible = ix == self.active;
        let t = &mut self.threads[ix];
        let n = t.items.len();
        // Index of the item that changed, and whether it is new.
        let mut changed = None;
        match ev {
            Event::Text(s) => match t.items.last_mut() {
                Some(Item::Assistant(text)) => {
                    text.push_str(&s);
                    changed = Some((n - 1, false));
                }
                _ => {
                    t.items.push(Item::Assistant(s));
                    changed = Some((n, true));
                }
            },
            Event::ToolStart { id, name, input } => {
                // A repeated id carries the call's full arguments (ACP sends a stub first).
                let known = t
                    .items
                    .iter()
                    .rposition(|i| matches!(i, Item::Tool { id: i, .. } if *i == id));
                if let Some(ix) = known {
                    if let Item::Tool { input: old, .. } = &mut t.items[ix] {
                        *old = input;
                    }
                    changed = Some((ix, false));
                } else {
                    t.items.push(Item::Tool {
                        id,
                        name,
                        input,
                        output: None,
                        is_error: false,
                        open: false,
                    });
                    changed = Some((n, true));
                }
            }
            Event::ToolEnd {
                id,
                output,
                is_error,
            } => {
                changed = thread_view::finish_tool(&mut t.items, &id, output, is_error)
                    .map(|i| (i, false));
            }
            Event::Permission {
                tool,
                summary,
                reply,
            } => {
                if notify {
                    os_notify(&format!("承認待ち: {tool}"), cx);
                }
                t.snoozed = 0; // an approval brings a snoozed thread back
                let approval = (tool.into(), summary.into(), reply);
                if t.pending.is_none() {
                    t.pending = Some(approval);
                } else {
                    t.approvals.push_back(approval);
                }
                t.status = Status::Waiting;
            }
            Event::Usage(u) => {
                t.cost += self.cfg.cost(&t.meta.model, &u).unwrap_or(0.);
                t.usage = u;
                t.spent.add(u);
            }
            Event::UsageAux(u) => {
                t.cost += self.cfg.cost(&t.meta.model, &u).unwrap_or(0.);
                t.spent.add(u);
            }
            Event::Thinking(s) => {
                // It arrives after the reply's text has streamed; show it above that text.
                let at = if matches!(t.items.last(), Some(Item::Assistant(_))) {
                    n - 1
                } else {
                    n
                };
                t.items.insert(at, Item::Thinking(s.into()));
                changed = Some((at, true));
            }
            Event::Image(block) => {
                if let Some(item) = thread_view::image_item(&block) {
                    t.items.push(item);
                    changed = Some((n, true));
                }
            }
            Event::Dialog { request, reply } => {
                // `method: secret` is answered here (masked input); other secret requests belong to the terminal.
                if request["secret"] == true && request["method"] != "secret" {
                    let _ = reply.send(json!({"cancelled":true}));
                    t.items.push(Item::Notice(
                        "秘密情報の入力はエージェント本体のターミナルで行ってください".into(),
                    ));
                    changed = Some((n, true));
                } else {
                    let prefill = request["prefill"].as_str().map(str::to_string);
                    // An `editor` prefill goes into an empty composer; a draft in progress is never overwritten.
                    if let Some(text) = prefill.filter(|_| {
                        if visible {
                            self.editor_text
                                .as_deref()
                                .unwrap_or(&self.input.read(cx).value())
                                .trim()
                                .is_empty()
                        } else {
                            t.draft.trim().is_empty()
                        }
                    }) {
                        t.draft = text.clone();
                        if visible {
                            self.editor_text = Some(text);
                        }
                    }
                    if notify {
                        os_notify(&format!("質問: {}", t.meta.title), cx);
                    }
                    if t.dialog.is_some() {
                        t.dialogs.push_back((request, reply));
                    } else {
                        t.dialog = Some((request, reply));
                    }
                    t.status = Status::Waiting;
                }
            }
            Event::Remote { command, data } => {
                match command.as_str() {
                    "rate_limit" => {
                        let at = data["reset_at"].as_u64().unwrap_or(0);
                        t.reset_at = at;
                        let text = if at > now() {
                            format!("利用上限に達しました。{} にリセットされます。パレットの「リセット時に再開」で予約できます", clock_local(at))
                        } else {
                            "利用上限に達しました。しばらくしてから再試行してください".to_string()
                        };
                        t.items.push(Item::Notice(text.into()));
                        changed = Some((n, true));
                    }
                    "usage_update" => {
                        if let Some(used) = data["used"].as_u64() {
                            t.usage.input = used;
                        }
                        if let Some(amount) = data["cost"]["amount"].as_f64() {
                            t.cost = amount;
                        }
                    }
                    "dialog_closed" => {
                        if t.dialog
                            .as_ref()
                            .is_some_and(|(request, _)| request["id"] == data["id"])
                        {
                            t.dialog = t.dialogs.pop_front();
                            t.status = if t.pending.is_some() || t.dialog.is_some() {
                                Status::Waiting
                            } else {
                                Status::Running
                            };
                        }
                    }
                    // Finished delegated tasks or messages from other threads woke this idle thread: they are its next turn.
                    // `injected_prompt`: a turn another thread started here.
                    "delegate_wake" | "injected_prompt" => {
                        if t.loaded || command == "delegate_wake" {
                            t.items.push(Item::User(data["text"].as_str().unwrap_or("").to_string().into()));
                            changed = Some((n, true));
                        }
                        if data["steer"] != true {
                            t.status = Status::Running;
                        }
                    }
                    "background_wait" => {
                        t.bg_wait = (data["waiting"] == true).then(|| data["label"].as_str().unwrap_or("").to_string());
                    }
                    "subagent" => {
                        // A cancel reports only name and state; the row keeps its agent.
                        if let (Some(name), Some(state)) =
                            (data["name"].as_str(), data["state"].as_str())
                        {
                            let detail = data["detail"].as_str().unwrap_or("");
                            let task = data["task"].as_str().map(str::to_string);
                            if let Some(existing) = t.subs.iter_mut().find(|s| s.name == name) {
                                existing.state = state.into();
                                existing.detail = detail.into();
                                if let Some(agent) = data["agent"].as_str() {
                                    existing.agent = agent.into();
                                }
                                if task.is_some() {
                                    existing.task = task;
                                }
                            } else if let Some(agent) = data["agent"].as_str() {
                                t.subs.push(SubRow {
                                    name: name.into(),
                                    agent: agent.into(),
                                    state: state.into(),
                                    detail: detail.into(),
                                    task,
                                });
                            }
                        }
                    }
                    "acp_state" => {
                        t.steering = data["steering"] == true;
                        t.config_options = data["configOptions"]
                            .as_array()
                            .cloned()
                            .unwrap_or_default();
                        if data["configOptions"].is_array() {
                            let relevant =
                                greencode_core::cacheable_picker_options(&t.config_options);
                            if !cfg!(test) {
                                let _ = greencode_core::cache_backend_options(
                                    &t.meta.backend,
                                    &relevant,
                                );
                            }
                            self.backend_options
                                .insert(t.meta.backend.clone(), relevant);
                        }
                        if let Some(cmd) = &t.cmd {
                            t.pending_options.retain(|(id, value)| {
                            if t.config_options.iter().any(|o| o["id"].as_str() == Some(id.as_str())) {
                                cmd.send(Command::Remote(json!({"type":"set_config_option","configId":id,"value":value}))).is_err()
                            } else { true }
                        });
                        }
                        t.models =
                            greencode_core::remote_models("acp_state", &data).unwrap_or_default();
                        if !t.models.is_empty() {
                            if t.meta.backend == "opencode" {
                                self.project_models.insert(t.project(), t.models.clone());
                            } else {
                                self.backend_models
                                    .insert(t.meta.backend.clone(), t.models.clone());
                            }
                        }
                        if let Some(model) = data["models"]["currentModelId"].as_str() {
                            t.meta.model = model.into();
                        }
                        if let Some(option) = data["configOptions"]
                            .as_array()
                            .and_then(|v| v.iter().find(|o| o["category"] == "model"))
                        {
                            if let Some(model) = option["currentValue"].as_str() {
                                t.meta.model = model.into();
                            }
                        }
                    }
                    "get_available_models" => {
                        t.models = greencode_core::remote_models("get_available_models", &data)
                            .unwrap_or_default();
                        if !t.models.is_empty() {
                            if t.meta.backend == "opencode" {
                                self.project_models.insert(t.project(), t.models.clone());
                            } else {
                                self.backend_models
                                    .insert(t.meta.backend.clone(), t.models.clone());
                            }
                        }
                    }
                    "get_available_thinking_levels" => {
                        t.thinking_levels = data["levels"]
                            .as_array()
                            .into_iter()
                            .flatten()
                            .filter_map(|v| v.as_str().map(String::from))
                            .collect()
                    }
                    "get_state" => {
                        if let (Some(provider), Some(model)) = (
                            data["model"]["provider"].as_str(),
                            data["model"]["id"].as_str(),
                        ) {
                            t.meta.model = format!("{provider}/{model}");
                        }
                        t.thinking = data["thinkingLevel"].as_str().unwrap_or("").into();
                    }
                    "get_commands" | "get_available_commands" | "available_commands_update" => {
                        let native = t.meta.backend == "native";
                        let (mapped, real): (Vec<&Value>, Vec<&Value>) = data["commands"]
                            .as_array()
                            .into_iter()
                            .flatten()
                            .filter(|c| !(native && c["source"] == "builtin"))
                            .partition(|c| c["source"] == "greencode");
                        let names = |list: Vec<&Value>| {
                            let mut names: Vec<String> = list
                                .into_iter()
                                .filter_map(|c| {
                                    c.as_str()
                                        .or_else(|| c["name"].as_str())
                                        .map(|name| name.trim_start_matches('/').to_string())
                                })
                                .filter(|name| !name.is_empty())
                                .collect();
                            names.sort();
                            names.dedup();
                            names
                        };
                        (t.commands, t.mapped) = (names(real), names(mapped));
                        if visible {
                            self.commands = command_list(&self.cfg, t);
                        }
                    }
                    "get_session_stats" | "export_html" => {
                        t.items.push(Item::Notice(
                            serde_json::to_string_pretty(&data)
                                .unwrap_or_default()
                                .into(),
                        ));
                        changed = Some((n, true));
                    }
                    "copy_text" => {
                        if let Some(text) = data["text"].as_str().filter(|s| !s.is_empty()) {
                            cx.write_to_clipboard(gpui_kit::ClipboardItem::new_string(
                                text.to_string(),
                            ));
                            t.items.push(Item::Notice(
                                "最後の返信をクリップボードにコピーしました".into(),
                            ));
                            changed = Some((n, true));
                        } else {
                            t.items
                                .push(Item::Error("コピーできる返信がありません".into()));
                            changed = Some((n, true));
                        }
                    }
                    "compact" | "new_session" => {
                        t.items.push(Item::Notice(
                            if command == "compact" {
                                "会話を圧縮しました"
                            } else {
                                "新しいエージェントセッションを開始しました"
                            }
                            .into(),
                        ));
                        changed = Some((n, true));
                    }
                    "notify" | "command_output" => {
                        let text = data["message"]
                            .as_str()
                            .or_else(|| data["text"].as_str())
                            .unwrap_or("");
                        if !text.is_empty() {
                            t.items.push(if data["notifyType"] == "error" {
                                Item::Error(text.to_string().into())
                            } else {
                                Item::Notice(text.to_string().into())
                            });
                            changed = Some((n, true));
                        }
                    }
                    "setStatus" => {
                        let key = data["statusKey"].as_str().unwrap_or("").to_string();
                        match data["statusText"].as_str().filter(|s| !s.is_empty()) {
                            Some(text) => {
                                t.statuses.insert(key, extension_text(text));
                            }
                            None => {
                                t.statuses.remove(&key);
                            }
                        }
                    }
                    "setWidget" => {
                        let key = data["widgetKey"].as_str().unwrap_or("").to_string();
                        let lines: Vec<String> = data["widgetLines"]
                            .as_array()
                            .into_iter()
                            .flatten()
                            .filter_map(|l| l.as_str().map(extension_text))
                            .collect();
                        if lines.is_empty() {
                            t.widgets.remove(&key);
                        } else {
                            t.widgets.insert(key, lines);
                        }
                    }
                    // Unsent text is kept: the extension's text fills an empty composer and is appended otherwise.
                    "set_editor_text" => {
                        let text = data["text"].as_str().unwrap_or("");
                        let current = if visible {
                            self.editor_text
                                .clone()
                                .unwrap_or_else(|| self.input.read(cx).value().to_string())
                        } else {
                            t.draft.clone()
                        };
                        if !text.is_empty() {
                            let next = if current.trim().is_empty() {
                                text.to_string()
                            } else {
                                format!("{current}\n{text}")
                            };
                            t.draft = next.clone();
                            if visible {
                                self.editor_text = Some(next);
                            }
                        }
                    }
                    _ => {}
                }
            }
            Event::Notify(s) => {
                t.items.push(Item::Notice(s.into()));
                changed = Some((n, true));
            }
            Event::Error(s) => {
                // Discovery is best effort: a backend that cannot start reports it again with the first prompt.
                if t.warm && t.warmups > 0 {
                    return;
                }
                t.queue_held = true;
                t.snoozed = 0;
                t.items.push(Item::Error(s.into()));
                changed = Some((n, true));
            }
            Event::Done { .. } => {
                // The end of a silent discovery command, not of a prompt.
                if t.warmups > 0 {
                    t.warmups -= 1;
                    if t.warmups == 0 {
                        self.release_deferred(id, None, cx);
                    }
                    cx.notify();
                    return;
                }
                self.files = None; // the turn may have created files for `@` completion and the files panel
                if notify {
                    os_notify(&format!("完了: {}", t.meta.title), cx);
                }
                // Goal mode: keep going until the agent declares the goal met. Stopping holds the queue, which pauses it.
                if let Some((goal, turns)) = t.goal.clone().filter(|_| !t.goal_paused) {
                    let met = t
                        .items
                        .iter()
                        .rev()
                        .find_map(|i| {
                            if let Item::Assistant(a) = i {
                                Some(a.contains("GOAL MET"))
                            } else {
                                None
                            }
                        })
                        .unwrap_or(false);
                    if met || turns >= 20 {
                        t.goal = None;
                        t.items.push(Item::Notice(
                            if met {
                                "ゴールを達成しました"
                            } else {
                                "ゴール: 自動継続の上限（20回）に達したため停止しました"
                            }
                            .into(),
                        ));
                        changed = Some((n, true));
                        self.drop_goal_continuations(ix);
                    } else if !t.queue.front().is_some_and(|q| q.ends_with(GOAL_SUFFIX)) {
                        // A `/goal` sent during this turn is already next; it starts the goal itself.
                        t.goal = Some((goal.clone(), turns + 1));
                        if visible {
                            if let Some(edit) = &mut self.queue_edit {
                                edit.index += 1;
                            }
                        }
                        t.queue.push_front(goal::goal_continuation(&goal));
                    }
                }
                let t = &mut self.threads[ix];
                t.status = Status::Idle;
                t.suggestion = None;
                t.pending = None;
                t.approvals.clear();
                t.dialog = None;
                t.dialogs.clear();
                if visible {
                    self.refresh_diff(cx);
                }
                self.request_suggestion(ix, cx);
                self.request_title(ix, false, cx);
            }
        }
        if visible {
            match changed {
                // Streaming text updates at most ~30 times a second instead of once per chunk.
                Some(_) if is_text => {
                    if !self.text_flush_pending {
                        self.text_flush_pending = true;
                        cx.spawn(async move |this, cx| {
                            cx.background_executor()
                                .timer(std::time::Duration::from_millis(33))
                                .await;
                            this.update(cx, |this, cx| {
                                this.text_flush_pending = false;
                                let n = this.thread().items.len();
                                if n > 0 {
                                    this.list.splice(n - 1..n, 1);
                                    cx.notify();
                                }
                            })
                            .ok();
                        })
                        .detach();
                    }
                }
                Some((i, new)) => {
                    self.list.splice(if new { i..i } else { i..i + 1 }, 1);
                }
                // The stream can end between two flushes; show its last chunk now.
                None if done && !self.threads[ix].items.is_empty() => {
                    let n = self.threads[ix].items.len();
                    self.list.splice(n - 1..n, 1);
                }
                None => {}
            }
        }
        let next = if self.threads[ix].status == Status::Idle && !self.threads[ix].queue_held {
            self.threads[ix].queue.pop_front()
        } else {
            None
        };
        if let Some(text) = next {
            if visible {
                self.queue_removed(0);
            }
            // Same path as a typed prompt (checkpoint for /undo, last prompt for retry), without touching the composer.
            self.start_in(ix, text, cx);
            if visible {
                self.list.reset(self.threads[ix].items.len());
                self.list.scroll_to_end();
            }
        }
        if persist {
            if let Err(e) = self.threads[ix].save() {
                self.fail(e);
            }
        }
        cx.notify();
    }

    /// Removes queued goal continuations, keeping an open queue edit pointed at its item.
    fn drop_goal_continuations(&mut self, ix: usize) {
        let t = &mut self.threads[ix];
        if ix == self.active {
            if let Some(edit) = &mut self.queue_edit {
                let removed_before = t
                    .queue
                    .iter()
                    .take(edit.index)
                    .filter(|q| q.starts_with(GOAL_CONTINUE))
                    .count();
                if t.queue
                    .get(edit.index)
                    .is_some_and(|q| q.starts_with(GOAL_CONTINUE))
                {
                    edit.removed = true;
                }
                edit.index -= removed_before;
            }
        }
        t.queue.retain(|q| !q.starts_with(GOAL_CONTINUE));
    }

    fn answer(&mut self, d: Decision, cx: &mut Context<Self>) {
        let t = &mut self.threads[self.active];
        if let Some((_, _, reply)) = t.pending.take() {
            let _ = reply.send(d);
            t.pending = t.approvals.pop_front();
            t.status = if t.pending.is_some() || t.dialog.is_some() {
                Status::Waiting
            } else {
                Status::Running
            };
            cx.notify();
        }
    }
    fn select_ask_option(&mut self, label: &str, cx: &mut Context<Self>) {
        let Some((request, _)) = self.threads[self.active].dialog.as_mut() else {
            return;
        };
        let index = request["_gc_answers"].as_array().map_or(0, Vec::len);
        // Native `ask` and Claude's AskUserQuestion send `multiSelect`; older callers send `multi`.
        let q = &request["questions"][index];
        let multi = q["multiSelect"] == true || q["multi"] == true;
        if !request["_gc_selected"].is_array() {
            request["_gc_selected"] = json!([]);
        }
        let selected = request["_gc_selected"].as_array_mut().unwrap();
        if let Some(i) = selected.iter().position(|v| v == label) {
            selected.remove(i);
        } else {
            if !multi {
                selected.clear();
            }
            selected.push(json!(label));
        }
        cx.notify();
    }

    fn answer_ask(&mut self, custom: Option<String>, cx: &mut Context<Self>) {
        let t = &mut self.threads[self.active];
        let Some((request, _)) = t.dialog.as_mut() else {
            return;
        };
        if !request["_gc_answers"].is_array() {
            request["_gc_answers"] = json!([]);
        }
        let index = request["_gc_answers"].as_array().unwrap().len();
        let Some(question) = request["questions"].get(index) else {
            return;
        };
        let Some(id) = question["id"].as_str().map(str::to_string) else {
            return;
        };
        let selected = request["_gc_selected"].take();
        let mut answer =
            json!({"id":id,"selectedOptions":selected.as_array().cloned().unwrap_or_default()});
        if let Some(text) = custom {
            answer["customInput"] = json!(text);
        }
        request["_gc_answers"].as_array_mut().unwrap().push(answer);
        if request["_gc_answers"].as_array().unwrap().len()
            == request["questions"].as_array().map_or(0, Vec::len)
        {
            let answers = request["_gc_answers"].take();
            self.answer_dialog(json!({"answers":answers}), cx);
        } else {
            cx.notify();
        }
    }

    fn answer_dialog(&mut self, value: Value, cx: &mut Context<Self>) {
        let t = &mut self.threads[self.active];
        if let Some((_, reply)) = t.dialog.take() {
            let _ = reply.send(value);
            t.dialog = t.dialogs.pop_front();
            t.status = if t.pending.is_some() || t.dialog.is_some() {
                Status::Waiting
            } else {
                Status::Running
            };
            cx.notify();
        }
    }

    fn stop(&mut self) {
        self.threads[self.active].queue_held = true;
        if let Some(cmd) = &self.thread().cmd {
            let _ = cmd.send(Command::Cancel);
        }
    }

    // ---- completion ----

    fn files(&mut self) -> &[String] {
        let project = self.thread().meta.cwd.clone();
        if self.files.as_ref().is_none_or(|(p, _)| *p != project) {
            let out = std::process::Command::new("git")
                .arg("-C")
                .arg(&project)
                .args([
                    "ls-files",
                    "-z",
                    "--cached",
                    "--others",
                    "--exclude-standard",
                ])
                .output();
            let mut list: Vec<_> = out
                .ok()
                .filter(|o| o.status.success())
                .map(|o| {
                    String::from_utf8_lossy(&o.stdout)
                        .split('\0')
                        .filter(|p| !p.is_empty())
                        .map(String::from)
                        .collect()
                })
                .unwrap_or_default();
            // Not a git repo (T3 lists these too): walk the directory.
            if list.is_empty() {
                walk_files(&project, "", 0, &mut list);
            }
            list.sort();
            list.dedup();
            self.files = Some((project, list));
        }
        &self.files.as_ref().unwrap().1
    }

    /// Completions for the word being typed: `/command`, `/skill`, `$skill`, `@file`, `@thread`, `#PR`.
    fn suggestions(&mut self, cx: &App) -> Vec<String> {
        self.completions(cx)
    }

    /// Tab in the composer or the terminal field. The inputs bind Tab to indent, so `capture_action` routes here too.
    /// Returns whether it did something.
    fn tab(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        // The terminal's field sends whole lines, so the shell never sees a tab: hand it the typed text with the tab.
        if self.terminal_visible
            && self.palette.is_none()
            && self.active_terminal().is_some_and(|t| {
                t.stdin.is_some() && t.input.read(cx).focus_handle(cx).is_focused(window)
            })
        {
            let input = self.active_terminal().unwrap().input.clone();
            let mut bytes = input.read(cx).value().to_string().into_bytes();
            bytes.push(b'\t');
            input.update(cx, |s, cx| s.set_value("", window, cx));
            self.write_terminal(bytes);
            return true;
        }
        let modal = self.model_picker.is_some()
            || self.palette.is_some()
            || self.provider_form.is_some()
            || self.oauth_form.is_some()
            || self.key_form.is_some()
            || self.account_form.is_some()
            || self.cite_form.is_some()
            || self.usage.is_some()
            || self.workflows.is_some()
            || self.device_form.is_some()
            || self.theme_edit.is_some()
            || self.settings.is_some();
        if modal {
            return false;
        }
        if self.suggestion_ready(cx) {
            self.accept_suggestion(window, cx);
            return true;
        }
        match self.suggestions(cx).into_iter().next() {
            Some(s) => {
                self.complete(&s, window, cx);
                true
            }
            None => false,
        }
    }

    fn complete(&mut self, s: &str, window: &mut Window, cx: &mut Context<Self>) {
        if self.complete_mention(s, window, cx) {
            return;
        }
        let value = self.input.read(cx).value().to_string();
        let start = value
            .rmatch_indices(char::is_whitespace)
            .next()
            .map_or(0, |(i, space)| i + space.len());
        // `@path` is only a marker while typing; the model gets the plain path.
        let new = format!("{}{} ", &value[..start], s.strip_prefix('@').unwrap_or(s));
        self.input
            .update(cx, |st, cx| set_input(st, new, window, cx));
        cx.notify();
    }

    // ---- palette and actions ----

    fn acts(&self) -> Vec<(String, Act)> {
        let mut v = vec![
            ("新規スレッド  ⌘N".to_string(), Act::NewThread),
            (
                "新規スレッド（worktree で分離）  ⇧⌘N".into(),
                Act::NewWorktree,
            ),
            ("プロジェクトを開く…  ⌘O".into(), Act::OpenProject),
            ("リポジトリをクローン…".into(), Act::CloneProject),
            ("新規プロジェクトを作成…".into(), Act::NewProject),
            ("GitHub に公開…".into(), Act::PublishProject),
            ("差分: 未コミット".into(), Act::DiffSource(greencode_core::git::DiffSource::Uncommitted)),
            ("差分: ベースブランチとの差分".into(), Act::DiffSource(greencode_core::git::DiffSource::Base)),
            ("差分: コミットを選ぶ…".into(), Act::PickCommit),
            ("既定ブランチを今すぐ同期".into(), Act::SyncDefaultBranch),
            (
                format!(
                    "設定: 既定ブランチを最新に保つ（{}）",
                    if self.ui["sync_default_branch"] == true { "オン" } else { "オフ" }
                ),
                Act::Toggle("sync_default_branch"),
            ),
            (
                format!(
                    "設定: ホスト別フォルダにクローン（ghq・{}）",
                    if self.ui["clone_host_folders"] == true { "オン" } else { "オフ" }
                ),
                Act::Toggle("clone_host_folders"),
            ),
            (
                format!(
                    "設定: マージ時にエージェントのクレジットを除去（{}）",
                    if self.ui["pr_strip_credits"] == true { "オン" } else { "オフ" }
                ),
                Act::Toggle("pr_strip_credits"),
            ),
            ("ようこそページを開く".into(), Act::Welcome(welcome::WelcomeAct::Open)),
            (
                "Claude Code / Codex セッションをファイルから開く…".into(),
                Act::ImportSessionFiles,
            ),
            (
                "Hermesプロファイルをフォルダからインポート…".into(),
                Act::ImportHermesFolder,
            ),
            ("差分パネルを切り替え  ⌘D".into(), Act::ToggleDiff),
            ("ファイルパネル  ⇧⌘E".into(), Act::ToggleFiles),
            ("このスレッドを削除".into(), Act::Delete),
            (
                if self.thread().pinned {
                    "ピン留めを解除"
                } else {
                    "スレッドをピン留め"
                }
                .into(),
                Act::Pin,
            ),
            (
                if self.thread().archived {
                    "スレッドを復元"
                } else {
                    "スレッドをアーカイブ"
                }
                .into(),
                Act::Archive,
            ),
            (
                format!(
                    "自動保管: 設定に従う{}",
                    if self.thread().auto_settle == AutoSettle::Follow {
                        "（選択中）"
                    } else {
                        ""
                    }
                ),
                Act::AutoSettle(AutoSettle::Follow),
            ),
            (
                format!(
                    "自動保管: しない{}",
                    if self.thread().auto_settle == AutoSettle::Never {
                        "（選択中）"
                    } else {
                        ""
                    }
                ),
                Act::AutoSettle(AutoSettle::Never),
            ),
            ("PR のリンクを解除".into(), Act::LinkPr(None)),
            ("アーカイブの表示を切り替え".into(), Act::ToggleArchived),
            ("キューを再開".into(), Act::ResumeQueue),
            ("入力中の指示を実行中エージェントに送信".into(), Act::Steer),
            ("カスタムプロバイダを追加…".into(), Act::AddProvider),
            (
                "エージェントを追加（ACP レジストリ）…".into(),
                Act::RegistryOpen,
            ),
            (
                "他のアプリから GreenCode を使う（MCP）".into(),
                Act::CopyMcp,
            ),
            ("プロンプトリンクをコピー".into(), Act::CopyPromptLink(false)),
            (
                "プロンプトリンクをコピー（リポジトリとモデル付き）".into(),
                Act::CopyPromptLink(true),
            ),
            ("スレッドリンクをコピー".into(), Act::CopyThreadLink),
            ("スレッドIDをコピー".into(), Act::CopyThreadId),
            (
                "最後の返答を Markdown でコピー".into(),
                Act::CopyLastReply,
            ),
            ("プロンプトリンクを開く…".into(), Act::OpenPromptLink),
            ("GreenCode を更新…".into(), Act::Update(false, Channel::default())),
            ("設定…  ⌘,".into(), Act::Settings),
            ("利用状況（サブスクリプション上限）".into(), Act::Usage),
            ("使用量の履歴".into(), Act::UsageHistory),
            ("ワークフロー（実行・回答・再開）".into(), Act::Workflows),
            ("選択範囲を引用  ⇧⌘C".into(), Act::Cite),
            ("スレッドを上へ移動  ⌥⌘↑".into(), Act::Move(true)),
            ("スレッドを下へ移動  ⌥⌘↓".into(), Act::Move(false)),
            (
                "サイドチャットの回答を元のスレッドへ持ち帰る".into(),
                Act::BringBack,
            ),
            (
                "/btw の <inject> を元のスレッドへ送る".into(),
                Act::InjectBtw,
            ),
            ("ターミナルを切り替え  ⌘J".into(), Act::Terminal),
            ("Pi / OMP の端末 UI に引き継ぐ".into(), Act::Handoff),
            ("端末側の分岐履歴を採用…".into(), Act::HandoffAdopt),
            ("Git: 変更一覧を切り替え".into(), Act::ToggleGit),
            ("下書きをスタッシュ／復元  ⌘S".into(), Act::Stash),
            (
                "プロジェクトなしで新規スレッド  ⌥⌘N".into(),
                Act::NewScratch,
            ),
            ("スヌーズ: 1時間".into(), Act::Snooze(3600)),
            ("スヌーズ: 3時間".into(), Act::Snooze(3 * 3600)),
            (
                "スヌーズ: 明日まで（24時間）".into(),
                Act::Snooze(24 * 3600),
            ),
            ("スヌーズ中のスレッドをすべて起こす".into(), Act::WakeAll),
            (
                "このプロジェクトの既定として現在のモデル・権限を保存".into(),
                Act::SaveProjectDefault,
            ),
            (
                format!(
                    "設定: 実行中の送信 = {}（切り替え）",
                    if self.ui["follow_up_steer"] == true {
                        "ステア"
                    } else {
                        "キュー"
                    }
                ),
                Act::Toggle("follow_up_steer"),
            ),
            (
                format!(
                    "設定: / メニューにスキルを表示 = {}（切り替え）",
                    if self.ui["slash_skills_off"] == true { "オフ" } else { "オン" }
                ),
                Act::Toggle("slash_skills_off"),
            ),
            (
                format!(
                    "設定: ツール呼び出し（{}）",
                    tool_visibility_label(self.tool_visibility())
                ),
                Act::ToolVisibility,
            ),
            (
                "設定を開く: ui.json（キーバインド・プロジェクト既定）".into(),
                Act::OpenFile(greencode_core::home().join("ui.json")),
            ),
            (
                "Git: 変更をコミット（エージェント）".into(),
                Act::Prompt(COMMIT_PROMPT),
            ),
            (
                "Git: プッシュ".into(),
                Act::Shell("git push -u origin HEAD".into()),
            ),
            (
                "Git: プルリクエストを作成（gh）".into(),
                if self.text_model().is_some() { Act::GeneratePr } else { Act::Shell("gh pr create --fill".into()) },
            ),
            ("コミットメッセージを生成してコミット".into(), Act::GenerateCommit),
            ("タイトルを再生成".into(), Act::RegenerateTitle),
            ("次の自動作成用モデルに切り替え".into(), Act::CycleTextModel),
            ("最後のターンを取り消す（ファイルも戻す）".into(), Act::Undo),
            ("このターンの bash 変更の差分".into(), Act::BashDiff),
            ("エージェントセッションを再起動".into(), Act::RestartAgent),
            ("問題を調査（トリアージ）".into(), Act::Triage),
            ("ゲストリンクを作成（24時間・読み取り専用）".into(), Act::GuestLink),
            ("最後の返答を読み上げ".into(), Act::Speak),
            ("スレッドを複製（フォーク）".into(), Act::Fork),
            ("会話をMarkdownでエクスポート".into(), Act::Export),
            ("ファイルを添付…".into(), Act::Attach),
            (
                format!(
                    "サンドボックス（bash の書き込み制限）を{}",
                    if greencode_core::tools::SANDBOX.load(std::sync::atomic::Ordering::Relaxed) {
                        "オフにする"
                    } else {
                        "オンにする"
                    }
                ),
                Act::Sandbox,
            ),
            (
                "設定を開く: config.toml".into(),
                Act::OpenFile(greencode_core::home().join("config.toml")),
            ),
            (
                "設定を開く: mcp.json（MCPサーバー）".into(),
                Act::OpenFile(greencode_core::home().join("mcp.json")),
            ),
            (
                "設定を開く: allow（許可ルール）".into(),
                Act::OpenFile(greencode_core::home().join("allow")),
            ),
        ];
        if self.thread().reset_at > now() {
            v.push(("リセット時に再開".into(), Act::ResumeAtReset));
        }
        for mode in themes::ThemeMode::ALL {
            let suffix = if mode == self.theme_mode() { " ✓" } else { "" };
            v.push((
                format!("外観: {}{suffix}", mode.hint()),
                Act::ThemeMode(mode),
            ));
        }
        v.push((
            "外観を切り替え: システム → ライト → ダーク  ⌘⌥⇧A".into(),
            Act::CycleThemeMode,
        ));
        let (light, dark) = (self.theme_slot(false), self.theme_slot(true));
        for choice in themes::choices(&self.custom_themes) {
            let suffix = if choice == light && choice == dark {
                " ✓"
            } else if choice == light {
                " ✓ライト"
            } else if choice == dark {
                " ✓ダーク"
            } else {
                ""
            };
            let label = match &choice {
                Choice::Builtin(t) => t.palette_label().to_string(),
                Choice::Custom(_) => format!("テーマ: {}", choice.label(&self.custom_themes)),
            };
            v.push((format!("{label}{suffix}"), Act::Theme(choice)));
        }
        v.push((
            "テーマ: 読み込む…（VS Code / GreenCode JSON）".into(),
            Act::ImportTheme,
        ));
        v.push(("テーマ: 書き出す…".into(), Act::ExportTheme));
        v.push(("テーマ: 編集…".into(), Act::EditTheme));
        v.push(("背景画像を選択…".into(), Act::PickBackground));
        if self.background_image().is_some() {
            v.push(("背景画像をクリア".into(), Act::ClearBackground));
            v.push((
                format!(
                    "背景の不透明度: {}%（+10%）",
                    (self.background_opacity() * 100.).round() as u32
                ),
                Act::BackgroundOpacity(10),
            ));
        }
        v.extend(greencode_core::agents::AGENTS.iter().map(|a| {
            (
                format!("エージェントをインストール/更新: {}", a.name),
                Act::InstallAgent(a.name),
            )
        }));
        v.extend(self.prs.iter().map(|(number, _, _)| {
            (
                format!("PR をリンク: #{number}"),
                Act::LinkPr(Some(*number)),
            )
        }));
        v.extend(self.reminder_rows());
        v.extend(self.webhook_rows());
        if self.thread().meta.backend == "native" {
            let prompts = self.thread().items.iter().rev().filter_map(|i| {
                if let Item::User(u) = i {
                    Some(u)
                } else {
                    None
                }
            });
            for (back, prompt) in prompts.clone().enumerate() {
                v.push((
                    format!(
                        "このターンの差分: {}",
                        prompt.chars().take(50).collect::<String>()
                    ),
                    Act::TurnDiff(back),
                ));
            }
            for (back, prompt) in prompts.enumerate().skip(1) {
                v.push((
                    format!(
                        "ここまで巻き戻して編集: {}",
                        prompt.chars().take(50).collect::<String>()
                    ),
                    Act::Rewind(back),
                ));
            }
        }
        for profile in &self.hermes_profiles {
            v.push((
                format!("Hermesプロファイルをインポート: {}", profile.name),
                Act::ImportHermes(profile.clone()),
            ));
        }
        for found in &self.importable {
            v.push((
                format!("{}のセッションをインポート: {}", found.source, found.title),
                Act::ImportSession(found.clone()),
            ));
        }
        for (n, title, branch) in &self.prs {
            let title: String = title.chars().take(50).collect();
            v.push((
                format!("PR #{n} をレビュー（エージェント）: {title}"),
                Act::Ask(prs::review_prompt(*n)),
            ));
            if *branch == self.branch {
                v.push((
                    format!("PR #{n}: チェック状況"),
                    Act::Shell(format!("gh pr checks {n}")),
                ));
                v.push((
                    format!("PR #{n}: ブラウザで開く"),
                    Act::Shell(format!("gh pr view {n} --web")),
                ));
                v.push((
                    format!("PR #{n}: マージ（squash）"),
                    Act::Shell(format!("gh pr merge {n} --squash")),
                ));
            } else {
                v.push((
                    format!("PR #{n} をチェックアウト: {title}"),
                    Act::Shell(format!("gh pr checkout {n}")),
                ));
            }
        }
        for b in self.branches.iter().filter(|b| **b != self.branch) {
            v.push((
                format!("Git: ブランチを切り替え: {b}"),
                Act::Shell(format!("git switch {}", shell_quote(b))),
            ));
        }
        for (i, text) in self.ui["stash"]
            .as_array()
            .into_iter()
            .flatten()
            .enumerate()
        {
            v.push((
                format!(
                    "スタッシュを復元: {}",
                    text.as_str()
                        .unwrap_or("")
                        .chars()
                        .take(50)
                        .collect::<String>()
                ),
                Act::Unstash(i),
            ));
        }
        v.extend(self.palette_accounts.iter().cloned());
        if !self.palette_effort.is_empty() {
            v.push(("思考の強さ: 既定".into(), Act::Effort(None)));
        }
        for &level in &self.palette_effort {
            v.push((
                format!("思考の強さ: {level}"),
                Act::Effort(Some(level.into())),
            ));
        }
        v.push((
            "思考の強さを上げる  ⌘⌥.".into(),
            Act::EffortStep(true),
        ));
        v.push((
            "思考の強さを下げる  ⌘⌥,".into(),
            Act::EffortStep(false),
        ));
        for key in self.thread().perms.allowed() {
            v.push((format!("許可ルールを削除: {key}"), Act::Revoke(key)));
        }
        for &m in greencode_core::config::backend_modes(&self.thread().meta.backend) {
            v.push((format!("権限: {}", mode_label(m)), Act::Mode(m)));
        }
        // One picker for every backend, as in T3 Code: the thread's own models switch in place, the others pick the backend too.
        let t = self.thread();
        for m in self.models() {
            if t.meta.backend == "native" {
                v.push((format!("モデル: {m}"), Act::Model(m)));
            } else {
                v.push((
                    format!("モデル: native · {m}"),
                    Act::BackendModel("native".into(), m),
                ));
            }
        }
        for (backend, models) in &self.models_for_project(&t.project()) {
            if self
                .cfg
                .policy
                .as_ref()
                .is_some_and(|p| !p.backend_allowed(backend))
            {
                continue;
            }
            if *backend == t.meta.backend && !t.models.is_empty() {
                continue;
            }
            for m in models {
                v.push((
                    format!("モデル: {backend} · {m}"),
                    Act::BackendModel(backend.clone(), m.clone()),
                ));
            }
        }
        v.extend(
            self.palette_skills
                .iter()
                .map(|name| (format!("スキル: {name}"), Act::Skill(name.clone()))),
        );
        v.push((
            "外部エージェントのモデル一覧を再取得".into(),
            Act::RefreshBackendModels,
        ));
        if self.thread().meta.backend == "native" {
            let current = self.thread().meta.subagent_model.as_ref();
            v.push((
                format!(
                    "サブエージェント既定モデル: 親と同じ{}",
                    if current.is_none() {
                        "（選択中）"
                    } else {
                        ""
                    }
                ),
                Act::SubagentModel(None),
            ));
            let mut models = self.models();
            for model in self.cfg.subagent_model.iter().chain(current) {
                if !models.contains(model) {
                    models.push(model.clone());
                }
            }
            for model in models {
                v.push((
                    format!(
                        "サブエージェント既定モデル: {model}{}",
                        if current == Some(&model) {
                            "（選択中）"
                        } else {
                            ""
                        }
                    ),
                    Act::SubagentModel(Some(model)),
                ));
            }
        }
        for m in &self.thread().models {
            v.push((format!("モデル: {m}"), Act::Model(m.clone())));
        }
        // Keep the conversation's model ahead of the writing model in unfiltered search.
        let writing_model = self.text_model();
        v.push(("自動作成に使うモデル: オフ".into(), Act::TextModel(None)));
        for m in self.models() {
            let chosen = if writing_model.as_deref() == Some(m.as_str()) { "（選択中）" } else { "" };
            v.push((format!("自動作成に使うモデル: {m}{chosen}"), Act::TextModel(Some(m))));
        }
        if rpc_backend(&self.cfg, &self.thread().meta.backend) {
            for level in &self.thread().thinking_levels {
                v.push((
                    format!("思考の深さ: {level}"),
                    Act::Rpc(json!({"type":"set_thinking_level","level":level})),
                ));
            }
            v.push((
                "モデルとコマンドを再取得".into(),
                Act::Rpc(json!({"type":"get_available_models"})),
            ));
            v.push(("会話を圧縮".into(), Act::Rpc(json!({"type":"compact"}))));
            v.push((
                "使用量の詳細".into(),
                Act::Rpc(json!({"type":"get_session_stats"})),
            ));
        }
        for option in &self.thread().config_options {
            if option["category"] == "model" {
                continue;
            }
            for item in option["options"].as_array().into_iter().flatten() {
                let items = item["options"]
                    .as_array()
                    .cloned()
                    .unwrap_or_else(|| vec![item.clone()]);
                for item in items {
                    if let Some(value) = item.get("value") {
                        v.push((format!("{}: {}",option["name"].as_str().unwrap_or("設定"),item["name"].as_str().unwrap_or("")),Act::Rpc(json!({"type":"set_config_option","configId":option["id"],"value":value}))));
                    }
                }
            }
        }
        for b in self.backends() {
            let name = if b == "native" { "標準" } else { &b };
            v.push((format!("接続先: {name}"), Act::Backend(b)));
        }
        for t in &self.threads {
            if !t.meta.title.is_empty() {
                v.push((
                    format!("スレッド: {}", t.meta.title),
                    Act::Thread(t.meta.id.clone()),
                ));
            }
        }
        v.extend(self.repo_acts());
        v.extend(self.project_acts());
        v
    }

    /// Backends offered in the pickers; `ui["disabled_backends"]` hides one without removing its config.
    fn backends(&self) -> Vec<String> {
        let disabled = self.disabled("disabled_backends");
        std::iter::once("native".to_string())
            .chain(
                self.cfg
                    .backends
                    .iter()
                    .filter(|b| {
                        self.cfg
                            .policy
                            .as_ref()
                            .is_none_or(|p| p.backend_allowed(&b.name))
                            && !disabled.contains(&b.name)
                    })
                    .map(|b| b.name.clone()),
            )
            .collect()
    }

    /// `ui["tool_calls"]` with the older boolean as a fallback.
    fn tool_visibility(&self) -> ToolVisibility {
        match self.ui["tool_calls"].as_str() {
            Some("successful") => ToolVisibility::Successful,
            Some("all") => ToolVisibility::All,
            Some("zen") => ToolVisibility::Zen,
            Some(_) => ToolVisibility::Show,
            None if self.ui["hide_tools"] == true => ToolVisibility::All,
            None => ToolVisibility::Show,
        }
    }

    fn cycle_tool_visibility(&mut self) {
        let next = match self.tool_visibility() {
            ToolVisibility::Show => "successful",
            ToolVisibility::Successful => "all",
            ToolVisibility::All => "zen",
            ToolVisibility::Zen => "show",
        };
        self.ui["tool_calls"] = json!(next);
        self.ui["hide_tools"] = json!(next != "show");
        self.save_ui();
    }

    /// Names listed in one of the `ui` arrays of hidden entries.
    fn disabled(&self, key: &str) -> Vec<String> {
        self.ui[key]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(String::from)
            .collect()
    }

    /// Toggles a name in `ui[key]` and saves.
    pub(super) fn toggle_disabled(&mut self, key: &str, name: &str) {
        let mut list = self.disabled(key);
        match list.iter().position(|n| n == name) {
            Some(i) => {
                list.remove(i);
            }
            None => list.push(name.to_string()),
        }
        self.ui[key] = json!(list);
        self.save_ui();
    }

    fn refresh_palette_rows(&mut self) {
        schedule::utc_offset();
        let mut rows = Vec::new();
        for (name, p) in &self.cfg.providers {
            let method = greencode_core::auth::status(name, p);
            if !matches!(
                method,
                greencode_core::auth::Method::StoredKey
                    | greencode_core::auth::Method::Subscription
            ) {
                continue;
            }
            rows.push((
                format!("ログアウト: {name}（{}）", method_label(&method)),
                Act::Logout(name.clone()),
            ));
            rows.push((
                format!("アカウントとして保存: {name}…"),
                Act::SaveAccount(name.clone()),
            ));
            if let Ok((active, all)) = greencode_core::auth::accounts(name) {
                if all.len() >= 2 {
                    for account in all.into_iter().filter(|a| active.as_deref() != Some(a)) {
                        rows.push((
                            format!("アカウント切替: {name} → {account}"),
                            Act::SwitchAccount(name.clone(), account),
                        ));
                    }
                }
            }
        }
        let ag = greencode_core::antigravity::status();
        if ag.signed_in {
            rows.push((
                format!(
                    "ログアウト: antigravity（{}）",
                    greencode_core::antigravity::auth_type_label(&ag.auth_type)
                ),
                Act::Logout("antigravity".into()),
            ));
        }
        self.palette_accounts = rows;
        self.palette_logins = self.login_rows();
        self.palette_skills = greencode_core::skills::list(&self.thread().meta.cwd, &self.cfg)
            .into_iter()
            .map(|skill| skill.name)
            .collect();
        self.palette_effort =
            greencode_core::provider::effort_levels(&self.cfg, &self.thread().meta.model);
    }

    fn filtered_acts(&self, cx: &App) -> Vec<(String, Act)> {
        let q = self
            .palette
            .as_ref()
            .map(|(p, _)| p.read(cx).value().to_lowercase())
            .unwrap_or_default();
        // A palette prompt is a text box: its only row submits the current value.
        if let Some(prompt) = self.text_prompt {
            return vec![(format!("⏎ {}: {}", prompt.action(), q), Act::SubmitText)];
        }
        // The commit picker lists the whole first-parent history, not the usual twelve rows.
        if self.picking_commit {
            return self
                .diff_commits
                .iter()
                .filter(|(sha, subject)| format!("コミット: {sha} {subject}").to_lowercase().contains(&q))
                .map(|(sha, subject)| (format!("コミット: {sha} {subject}"), Act::DiffCommit(sha.clone())))
                .collect();
        }
        if let Some(rows) = self
            .registry_entries
            .as_ref()
            .or_else(|| self.login_palette.then_some(&self.palette_logins))
        {
            return rows
                .iter()
                .filter(|(label, _)| label.to_lowercase().contains(&q))
                .cloned()
                .collect();
        }
        // `?text` searches inside conversations instead of labels.
        if q.starts_with('?') {
            return self
                .threads
                .iter()
                .filter(|t| self.found.contains(&t.meta.id))
                .take(12)
                .map(|t| {
                    (
                        format!("スレッド: {}", t.meta.title),
                        Act::Thread(t.meta.id.clone()),
                    )
                })
                .collect();
        }
        // Prefix matches first, so "モデル: " lists models ahead of labels that merely contain it.
        let (mut first, mut rest): (Vec<_>, Vec<_>) = self
            .acts()
            .into_iter()
            .filter(|(label, _)| label.to_lowercase().contains(&q))
            .partition(|(label, _)| label.to_lowercase().starts_with(&q));
        first.append(&mut rest);
        first.truncate(12);
        first
    }

    fn toggle_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.palette.take().is_some() {
            self.login_palette = false;
            self.registry_entries = None;
            self.text_prompt = None;
            self.picking_commit = false;
            self.input.update(cx, |s, cx| s.focus(window, cx));
        } else {
            self.refresh_prs(cx);
            self.refresh_palette_rows();
            let state = cx.new(|cx| {
                InputState::new(window, cx)
                    .placeholder("コマンド、モデル、スレッドを検索（?語句 で会話の全文検索）")
            });
            let sub =
                cx.subscribe_in(
                    &state,
                    window,
                    |this, _, ev: &InputEvent, window, cx| match ev {
                        InputEvent::PressEnter { .. } => {
                            if let Some((_, act)) =
                                this.filtered_acts(cx).into_iter().nth(this.palette_ix)
                            {
                                this.run(act, window, cx);
                            }
                        }
                        InputEvent::Change => {
                            this.palette_ix = 0;
                            this.palette_scroll.set_offset(point(px(0.), px(0.)));
                            let q = this
                                .palette
                                .as_ref()
                                .map(|(p, _)| p.read(cx).value().to_string())
                                .unwrap_or_default();
                            // Full-text search reads every transcript: debounce it and keep it off the UI thread.
                            this.search_gen = this.search_gen.wrapping_add(1);
                            let generation = this.search_gen;
                            this.found.clear();
                            if let Some(text) = q
                                .strip_prefix('?')
                                .map(str::trim)
                                .filter(|t| t.chars().count() >= 2)
                            {
                                let text = text.to_string();
                                cx.spawn(async move |this, cx| {
                                    cx.background_executor()
                                        .timer(std::time::Duration::from_millis(180))
                                        .await;
                                    let found = cx
                                        .background_executor()
                                        .spawn(async move { Session::search(&text) })
                                        .await;
                                    this.update(cx, |this, cx| {
                                        if this.search_gen == generation {
                                            this.found = found;
                                            cx.notify();
                                        }
                                    })
                                    .ok();
                                })
                                .detach();
                            }
                            cx.notify()
                        }
                        _ => {}
                    },
                );
            state.update(cx, |s, cx| s.focus(window, cx));
            self.palette_ix = 0;
            self.palette_scroll.set_offset(point(px(0.), px(0.)));
            self.found.clear();
            self.search_gen = self.search_gen.wrapping_add(1);
            self.refresh_backend_models(false, cx);
            // Discovery runs a CLI and scans transcripts; keep the palette snappy and reuse the result briefly.
            let project = self.thread().project();
            let due = self.import_checked.as_ref().is_none_or(|(p, at)| {
                p != &project || at.elapsed() >= std::time::Duration::from_secs(30)
            });
            if due {
                if self
                    .import_checked
                    .as_ref()
                    .is_none_or(|(p, _)| p != &project)
                {
                    self.importable.clear();
                }
                self.import_checked = Some((project.clone(), std::time::Instant::now()));
                let discover = project.clone();
                cx.spawn(async move |this, cx| {
                    let found = cx
                        .background_executor()
                        .spawn(async move { import::discover(&discover) })
                        .await;
                    this.update(cx, |this, cx| {
                        if this.thread().project() == project {
                            this.importable = found;
                            cx.notify();
                        }
                    })
                    .ok();
                })
                .detach();
            }
            self.palette = Some((state, sub));
        }
        cx.notify();
    }

    fn import_session_paths(
        &mut self,
        paths: Vec<PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let model = if self.thread().meta.backend == "native" {
            self.thread().meta.model.clone()
        } else {
            self.cfg.model.clone()
        };
        for path in paths {
            match import::import_file(&path, &model) {
                Ok(meta) => {
                    self.threads
                        .insert(0, Thread::new(meta, false, self.cfg.mode));
                    self.active += 1;
                    self.select(0, window, cx);
                }
                Err(e) => self.fail(format!("インポートできません: {e:#}")),
            }
        }
    }

    fn run(&mut self, act: Act, window: &mut Window, cx: &mut Context<Self>) {
        if (self.handoffs.contains(&self.thread().meta.id)
            || greencode_core::handoff::is_open(&self.thread().meta.id))
            && matches!(
                &act,
                Act::Rpc(_)
                    | Act::Model(_)
                    | Act::Backend(_)
                    | Act::BackendModel(_, _)
                    | Act::Steer
                    | Act::ResumeQueue
                    | Act::Retry
                    | Act::ResumeAtReset
            )
        {
            self.fail("端末 UI への引き継ぎが未完了です");
            return;
        }
        if let Some(policy) = &self.cfg.policy {
            let blocked = match &act {
                Act::Mode(m) => policy.check_mode(*m).is_err(),
                Act::DefaultMode(m) => {
                    policy.check_mode(*m).is_err()
                        || policy
                            .settings
                            .default_mode
                            .as_ref()
                            .is_some_and(|s| s.locked)
                }
                Act::Sandbox => policy.settings.sandbox.as_ref().is_some_and(|s| s.locked),
                Act::Toggle("suggest_next") => policy
                    .settings
                    .suggest_next
                    .as_ref()
                    .is_some_and(|s| s.locked),
                Act::CycleVoiceProvider => policy
                    .settings
                    .voice_provider
                    .as_ref()
                    .is_some_and(|s| s.locked),
                Act::Backend(name) | Act::BackendModel(name, _) => !policy.backend_allowed(name),
                _ => false,
            };
            if blocked {
                self.notice(format!(
                    "管理ポリシーで制限されています（{}）",
                    policy.organization
                ));
                return;
            }
        }
        // The palette's text is lost when it closes; take it before that.
        if matches!(act, Act::SubmitText) {
            let value = self
                .palette
                .as_ref()
                .map(|(input, _)| input.read(cx).value().to_string())
                .unwrap_or_default();
            let prompt = self.text_prompt.take();
            if self.palette.is_some() {
                self.toggle_palette(window, cx);
            }
            if let Some(prompt) = prompt {
                self.submit_prompt(prompt, value, window, cx);
            }
            return;
        }
        if self.palette.is_some() {
            self.toggle_palette(window, cx);
        }
        match act {
            Act::RegistryOpen => self.open_registry(window, cx),
            Act::RegistryInstall(entry) => self.install_registry(*entry, cx),
            Act::RegistryRemove(id) => self.remove_registry(id, cx),
            Act::CopyWebhook(id) => self.copy_webhook(&id, cx),
            Act::DeleteWebhook(id) => self.delete_webhook(&id),
            Act::Update(check, channel) => self.update_app(check, channel, cx),
            Act::CopilotLogin => self.start_device_login(DeviceKind::Copilot, window, cx),
            Act::ChatGptLogin => self.start_device_login(DeviceKind::ChatGpt, window, cx),
            Act::MistralLogin => self.start_device_login(DeviceKind::Mistral, window, cx),
            Act::AntigravityLogin => self.start_device_login(DeviceKind::Antigravity, window, cx),
            Act::AntigravityInstall => self.install_antigravity(cx),
            Act::AntigravityAuthType(kind) => {
                match greencode_core::antigravity::set_auth_type(&kind) {
                    Ok(()) => {
                        self.notice(format!(
                            "Antigravity の認証方式を {} に変更しました。",
                            greencode_core::antigravity::auth_type_label(&kind)
                        ));
                        cx.notify();
                    }
                    Err(e) => self.fail(format!("{e:#}")),
                }
            }
            Act::CopyPromptLink(with_repo) => self.copy_prompt_link(with_repo, cx),
            Act::CopyThreadLink => self.copy_thread_link(cx),
            Act::CopyThreadId => self.copy_thread_id(cx),
            Act::CopyMessage(ix) => {
                let text = self
                    .thread()
                    .items
                    .get(ix)
                    .and_then(find::item_text)
                    .map(str::to_string);
                match text {
                    Some(text) => {
                        cx.write_to_clipboard(gpui_kit::ClipboardItem::new_string(text));
                        self.notice("メッセージをコピーしました");
                    }
                    None => self.notice("コピーできるメッセージではありません"),
                }
            }
            Act::CopyLastReply => {
                let text = self.thread().items.iter().rev().find_map(|item| match item {
                    Item::Assistant(text) if !text.trim().is_empty() => Some(text.clone()),
                    _ => None,
                });
                match text {
                    Some(text) => {
                        cx.write_to_clipboard(gpui_kit::ClipboardItem::new_string(text));
                        self.notice("最後の返答を Markdown でコピーしました");
                    }
                    None => self.notice("コピーできる返答がありません"),
                }
            }
            Act::OpenPromptLink => self.open_link_from_clipboard(window, cx),
            Act::CopyMcp => {
                let sibling = std::env::current_exe()
                    .ok()
                    .map(|exe| exe.with_file_name("greencode-cli"))
                    .filter(|p| p.is_file());
                let cli = sibling.or_else(|| {
                    std::env::var_os("PATH").and_then(|path| {
                        std::env::split_paths(&path)
                            .map(|dir| dir.join("greencode-cli"))
                            .find(|p| p.is_file())
                    })
                });
                match cli.and_then(|p| p.canonicalize().ok()) {
                    Some(cli) => {
                        let path = cli.to_string_lossy().replace('\'', "'\\''");
                        let command = format!("claude mcp add greencode -- '{path}' mcp");
                        cx.write_to_clipboard(gpui_kit::ClipboardItem::new_string(command.clone()));
                        self.notice(format!("コピーしました: {command}"));
                    }
                    None => self.fail("greencode-cli が見つかりません。インストールするか PATH に追加してください"),
                }
            }
            Act::AnthropicLogin
                if self
                    .cfg
                    .providers
                    .get("anthropic")
                    .is_some_and(|p| p.api_key.is_some() && !p.stored) =>
            {
                self.fail("anthropic の api_key が config.toml にあるためサブスクリプションを使えません。config.toml から削除してください");
            }
            // Borrows an existing CLI sign-in; the rotating refresh token stays with that CLI.
            #[cfg(unix)]
            Act::ImportLogins => {
                let claude = greencode_core::provider::anthropic_oauth::import_into(
                    &greencode_core::provider::anthropic_oauth::path(),
                );
                let codex = greencode_core::provider::codex::import_into(
                    &greencode_core::provider::codex::path(),
                );
                let mut done = Vec::new();
                match claude {
                    Ok(Some(_)) => done.push("Claude Code"),
                    Ok(None) => {}
                    Err(e) => self.fail(format!(
                        "Claude Code ログインの取り込みに失敗しました: {e:#}"
                    )),
                }
                match codex {
                    Ok(Some(_)) => done.push("Codex CLI"),
                    Ok(None) => {}
                    Err(e) => {
                        self.fail(format!("Codex CLI ログインの取り込みに失敗しました: {e:#}"))
                    }
                }
                if done.is_empty() {
                    self.notice("取り込めるログインが見つかりませんでした（~/.claude/.credentials.json または ~/.codex/auth.json）");
                } else {
                    self.notice(format!(
                        "{} のログインを取り込みました（トークンが切れたら再度取り込んでください）",
                        done.join("・")
                    ));
                    self.refresh_models(cx);
                }
                cx.notify();
            }
            Act::AnthropicLogin => match greencode_core::provider::anthropic_oauth::Login::new() {
                Ok(login) => {
                    let url = login.url();
                    spawn_external("/usr/bin/open", &[&url]);
                    let field = cx.new(|cx| {
                        InputState::new(window, cx)
                            .placeholder("認証コードを貼り付けて ⏎")
                            .masked(true)
                    });
                    let sub =
                        cx.subscribe_in(&field, window, |this, _, ev: &InputEvent, window, cx| {
                            if let InputEvent::PressEnter { .. } = ev {
                                this.submit_oauth(window, cx)
                            }
                        });
                    field.update(cx, |s, cx| s.focus(window, cx));
                    self.oauth_form = Some(OAuthForm {
                        login: Some(login),
                        url: url.into(),
                        field,
                        pending: None,
                        _sub: sub,
                    });
                }
                Err(e) => self.fail(e),
            },
            Act::OpenCodeLogin => {
                if cfg!(target_os = "macos") {
                    spawn_external("/usr/bin/osascript", &["-e", "tell application \"Terminal\"", "-e", "activate", "-e", "do script \"PATH=\\\"$HOME/.opencode/bin:$HOME/.local/bin:$PATH\\\" opencode auth login\"", "-e", "end tell"]);
                } else {
                    self.fail("ターミナルで opencode auth login を実行してください");
                }
            }
            Act::HermesSetup(backend) => self.hermes_setup(&backend, window, cx),
            Act::NewThread => self.new_thread(self.thread().project(), false, window, cx),
            Act::NewWorktree => self.new_thread(self.thread().project(), true, window, cx),
            Act::ImportHermes(profile) => self.import_hermes(profile, window, cx),
            Act::ImportHermesFolder => {
                let rx = cx.prompt_for_paths(PathPromptOptions {
                    files: false,
                    directories: true,
                    multiple: false,
                    prompt: Some("Hermesプロファイルをインポート".into()),
                });
                cx.spawn_in(window, async move |this, cx| {
                    if let Ok(Ok(Some(paths))) = rx.await {
                        if let Some(path) = paths.into_iter().next() {
                            this.update_in(cx, |this, window, cx| {
                                match path
                                    .to_str()
                                    .ok_or_else(|| {
                                        "プロファイルのパスがUTF-8ではありません".to_string()
                                    })
                                    .and_then(|source| {
                                        hermes::resolve(source).map_err(|e| format!("{e:#}"))
                                    }) {
                                    Ok(profile) => this.import_hermes(profile, window, cx),
                                    Err(e) => this.fail(e),
                                }
                                cx.notify();
                            })
                            .ok();
                        }
                    }
                })
                .detach();
            }
            Act::OpenProject => {
                let rx = cx.prompt_for_paths(PathPromptOptions {
                    files: false,
                    directories: true,
                    multiple: false,
                    prompt: None,
                });
                cx.spawn_in(window, async move |this, cx| {
                    let selection = rx.await.map_err(|e| e.to_string())
                        .and_then(|paths| paths.map_err(|e| format!("{e:#}")));
                    this.update_in(cx, |this, window, cx| match selection {
                        Ok(paths) => {
                            if let Some(path) = paths.and_then(|paths| paths.into_iter().next()) {
                                this.new_thread(path, false, window, cx);
                            }
                        }
                        Err(err) => {
                            this.fail(format!("フォルダ選択を開けません: {err}"));
                            this.run(Act::EditText(git::TextPrompt::OpenProject), window, cx);
                        }
                    }).ok();
                })
                .detach();
            }
            Act::ToggleFiles => self.toggle_files(window, cx),
            Act::PullRequests => self.toggle_prs(window, cx),
            Act::ToggleDiff => {
                self.show_diff = !self.show_diff;
                if self.show_diff {
                    self.files_panel = None;
                    self.pr_panel = None;
                }
                self.refresh_diff(cx);
            }
            Act::Delete => self.delete_thread(self.active, window, cx),
            Act::DeleteThread(id) => {
                if let Some(ix) = self.threads.iter().position(|t| t.meta.id == id) {
                    self.delete_thread(ix, window, cx);
                }
            }
            Act::AutoSettle(value) => self.threads[self.active].auto_settle = value,
            Act::LinkPr(number) => {
                self.threads[self.active].linked_pr_manual = number.map(|number| LinkedPr {
                    number,
                    state: "OPEN".into(),
                    changed_at: 0,
                });
                self.refresh_thread_prs(cx);
            }
            Act::Mode(m) => self.thread().perms.set_mode(m),
            Act::Move(up) => {
                let (a, t) = (self.active, self.thread());
                let same = |o: &Thread| {
                    o.project() == t.project() && o.pinned == t.pinned && o.archived == t.archived
                };
                let other = if up {
                    self.threads[..a].iter().rposition(same)
                } else {
                    self.threads[a + 1..]
                        .iter()
                        .position(same)
                        .map(|i| a + 1 + i)
                };
                if let Some(b) = other {
                    self.threads.swap(a, b);
                    self.active = b;
                    self.ui["order"] = json!(self
                        .threads
                        .iter()
                        .map(|t| t.meta.id.as_str())
                        .collect::<Vec<_>>());
                    self.save_ui();
                }
            }
            Act::Pin => {
                let t = &mut self.threads[self.active];
                t.pinned = !t.pinned;
                let action = nav::SidebarAction::Pin {
                    id: t.meta.id.clone(),
                    value: !t.pinned,
                };
                self.undo.record(action);
            }
            Act::Archive => {
                let t = &mut self.threads[self.active];
                t.archived = !t.archived;
                let action = nav::SidebarAction::Archive {
                    id: t.meta.id.clone(),
                    value: !t.archived,
                };
                self.undo.record(action);
            }
            Act::ToggleArchived => self.show_archived = !self.show_archived,
            Act::ResumeQueue => {
                self.threads[self.active].queue_held = false;
                if self.thread().status == Status::Idle {
                    self.send_queue_now(0, window, cx);
                }
            }
            Act::MoveQueue(i, up) => {
                let queue = &mut self.threads[self.active].queue;
                let other = if up {
                    i.checked_sub(1)
                } else {
                    i.checked_add(1)
                };
                if let Some(other) = other.filter(|&other| i < queue.len() && other < queue.len()) {
                    queue.swap(i, other);
                    if let Some(edit) = &mut self.queue_edit {
                        if edit.index == i {
                            edit.index = other;
                        } else if edit.index == other {
                            edit.index = i;
                        }
                    }
                }
            }
            Act::EditQueue(i) => self.edit_queue(i, window, cx),
            Act::Steer => {
                let text = self.input.read(cx).value().trim().to_string();
                if text.is_empty()
                    && self.thread().status == Status::Running
                    && can_steer(&self.cfg, self.thread())
                    && !self.thread().queue.is_empty()
                {
                    self.send_queue_now(0, window, cx);
                } else if !text.is_empty()
                    && self.thread().status == Status::Running
                    && can_steer(&self.cfg, self.thread())
                {
                    if let Some(cmd) = &self.thread().cmd {
                        let _ = cmd.send(Command::Steer(text.clone()));
                    }
                    self.threads[self.active]
                        .items
                        .push(Item::User(text.into()));
                    self.list.reset(self.thread().items.len());
                    self.input.update(cx, |s, cx| set_input(s, "", window, cx));
                } else {
                    self.fail("この接続先では実行中に追加指示を送信できません");
                }
            }
            Act::Shell(command) => self.run_shell(command, false, cx),
            Act::Terminal => self.open_terminal(window, cx),
            Act::Handoff => self.handoff(window, cx),
            Act::HandoffAdopt => self.adopt_handoff(cx),
            Act::InstallAgent(name) => self.install_agent(name, window, cx),
            Act::Welcome(a) => self.welcome_act(a, window, cx),
            Act::ToggleGit => {
                self.show_git = !self.show_git;
                self.refresh_git(cx);
            }
            Act::Stage(path) => self.git_path_action(vec![path], true, cx),
            Act::Unstage(paths) => self.git_path_action(paths, false, cx),
            Act::GitReview => self.run(Act::Prompt(REVIEW_PROMPT), window, cx),
            Act::Usage => self.toggle_usage(window, cx),
            Act::UsageHistory => self.open_usage_history(cx),
            Act::Workflows => self.toggle_workflows(window, cx),
            Act::SecretAdd => self.secret_add(window, cx),
            Act::SecretRemove(i) => self.secret_remove(i),
            Act::SecretTest => self.secret_test(cx),
            Act::Cite => self.open_cite(window, cx),
            Act::SettingsTab(name) => {
                let Some(tab) = SETTINGS_TABS.iter().position(|(label, _)| *label == name) else {
                    return;
                };
                if self.settings.is_none() {
                    self.run(Act::Settings, window, cx);
                }
                self.settings_tab = tab;
                self.settings_scroll = ScrollHandle::default();
                if matches!(name, "詳細設定" | RUNTIME_SETTINGS) {
                    self.advanced_load();
                }
            }
            Act::SettingPick(key) => self.advanced_pick(key, window, cx),
            Act::SettingSave => self.advanced_save(false, window, cx),
            Act::SettingReset => self.advanced_save(true, window, cx),
            Act::RuntimeCategory(category) => {
                self.advanced.runtime_category = category;
                self.advanced.key = None;
                self.advanced.outcome = None;
                self.settings_scroll.set_offset(point(px(0.), px(0.)));
            }
            Act::RuntimeTool(name, on) => self.advanced_tool(name, on, window, cx),
            Act::SettingWrite(key, value) => {
                self.advanced.key = None;
                self.advanced_write(&key, value);
            }
            Act::Settings => {
                if self.settings.take().is_some() {
                    self.connection_name = None;
                    self.connection_fields = None;
                    self.input.update(cx, |s, cx| s.focus(window, cx));
                } else {
                    let mut subs = vec![];
                    let fields = ["キー（例: cmd-shift-p）", "コマンド（例: /plan）"].map(|hint| {
                        let state = cx.new(|cx| InputState::new(window, cx).placeholder(hint));
                        subs.push(cx.subscribe_in(
                            &state,
                            window,
                            |this, _, ev: &InputEvent, window, cx| {
                                if let InputEvent::PressEnter { .. } = ev {
                                    this.run(Act::Bind, window, cx)
                                }
                            },
                        ));
                        state
                    });
                    let field = cx.new(|cx| InputState::new(window, cx));
                    let sub = cx.subscribe_in(&field, window, |this, _, ev: &InputEvent, _, cx| {
                        if let InputEvent::PressEnter { .. } = ev {
                            this.save_connection_name(cx);
                        }
                    });
                    self.connection_name = Some((field, sub));
                    let selected = self.connection_selection();
                    let display = self.connection_display_name(&selected);
                    self.connection_name
                        .as_ref()
                        .unwrap()
                        .0
                        .update(cx, |s, cx| s.set_value(display, window, cx));
                    let hints = [
                        "実行ファイル（~ 可、空欄は既定）",
                        "追加の引数（スペース区切り）",
                        "環境変数 KEY=VALUE（スペース区切り）",
                        "追加するモデルID",
                        "追加するモデルID",
                    ];
                    let mut field_subs = vec![];
                    let mut create = |i: usize| {
                        let state = cx.new(|cx| InputState::new(window, cx).placeholder(hints[i]));
                        field_subs.push(cx.subscribe_in(
                            &state,
                            window,
                            move |this, _, ev: &InputEvent, window, cx| {
                                if let InputEvent::PressEnter { .. } = ev {
                                    this.save_connection_field(i, window, cx)
                                }
                            },
                        ));
                        state
                    };
                    self.connection_fields = Some((std::array::from_fn(&mut create), field_subs));
                    self.fill_connection_fields(window, cx);
                    self.settings = Some((fields, subs));
                    self.settings_tab = 0;
                    self.settings_scroll = ScrollHandle::default();
                    self.settings_focus.focus(window, cx);
                    self.refresh_agent_versions(cx);
                }
            }
            Act::DefaultMode(mode) => {
                Arc::make_mut(&mut self.cfg).mode = mode;
                self.ui["mode"] = json!(mode);
                self.save_ui();
            }
            Act::DefaultModel => {
                let model = self.thread().meta.model.clone();
                if self.thread().meta.backend == "native" {
                    Arc::make_mut(&mut self.cfg).model = model.clone();
                    self.ui["model"] = json!(model);
                    self.save_ui();
                } else {
                    self.fail("既定モデルにできるのはネイティブスレッドのモデルです。外部エージェントはプロジェクト既定で指定します");
                }
            }
            Act::Bind => {
                let Some((fields, _)) = &self.settings else {
                    return;
                };
                let [key, command] = fields
                    .each_ref()
                    .map(|f| f.read(cx).value().trim().to_lowercase());
                if key.is_empty() || !command.starts_with('/') {
                    self.fail("キーと、/ で始まるコマンドを入力してください");
                    self.settings = None;
                    cx.notify();
                    return;
                }
                let fields = fields.clone();
                for f in &fields {
                    f.update(cx, |s, cx| s.set_value("", window, cx));
                }
                if !self.ui["keys"].is_object() {
                    self.ui["keys"] = json!({});
                }
                self.ui["keys"][key] = json!(command);
                self.save_ui();
            }
            Act::Unbind(key) => {
                if let Some(keys) = self.ui["keys"].as_object_mut() {
                    keys.remove(&key);
                }
                self.save_ui();
            }
            Act::ClearProjectDefault => {
                let key = self.thread().project().to_string_lossy().into_owned();
                if let Some(projects) = self.ui["projects"].as_object_mut() {
                    projects.remove(&key);
                }
                self.save_ui();
            }
            Act::Quote => {
                let last = self.thread().items.iter().rev().find_map(|i| {
                    if let Item::Assistant(a) = i {
                        Some(a.clone())
                    } else {
                        None
                    }
                });
                match last {
                    Some(text) => {
                        let mut value = self.input.read(cx).value().to_string();
                        if !value.is_empty() && !value.ends_with('\n') {
                            value.push('\n');
                        }
                        value.push_str(
                            &text.lines().map(|l| format!("> {l}\n")).collect::<String>(),
                        );
                        self.input.update(cx, |s, cx| {
                            set_input(s, value + "\n", window, cx);
                            s.focus(window, cx);
                        });
                    }
                    None => self.fail("引用できる回答がまだありません"),
                }
            }
            Act::Ask(text) => {
                if self.thread().status == Status::Idle {
                    self.threads[self.active].queue_held = false;
                    self.start_prompt(text, cx);
                } else {
                    self.threads[self.active].queue.push_back(text);
                }
            }
            act @ (Act::AddRepo
            | Act::AddRepoPath(_)
            | Act::RemoveRepo(_)
            | Act::ToggleWorktreeRepo(_)
            | Act::DiffRepo(_)
            | Act::RepoCommit(_)
            | Act::RepoPush(_)) => self.repo_act(act, window, cx),
            act @ (Act::CloneProject
            | Act::NewProject
            | Act::PublishProject
            | Act::DiffSource(_)
            | Act::DiffCommit(_)
            | Act::PickCommit
            | Act::PinProject(_)
            | Act::ArchiveProject(_)
            | Act::SetCloneRoot
            | Act::EditText(_)
            | Act::CycleWorktreeSubmodules
            | Act::SyncDefaultBranch) => self.git_act(act, window, cx),
            Act::MergePr(number, auto) => self.merge_pr(number, auto, cx),
            Act::SubmitText => {}
            Act::Prompt(text) => {
                if self.thread().status == Status::Idle {
                    self.threads[self.active].queue_held = false;
                    self.start_prompt(text.to_string(), cx);
                } else {
                    self.threads[self.active].queue.push_back(text.to_string());
                }
            }
            Act::Stash => {
                let draft = self.input.read(cx).value().trim().to_string();
                let count = self.ui["stash"].as_array().map_or(0, Vec::len);
                if !draft.is_empty() {
                    match self.ui["stash"].as_array_mut() {
                        Some(list) => list.push(json!(draft)),
                        None => self.ui["stash"] = json!([draft]),
                    }
                    self.save_ui();
                    self.input.update(cx, |s, cx| set_input(s, "", window, cx));
                    self.threads[self.active].draft.clear();
                } else if count == 1 {
                    return self.run(Act::Unstash(0), window, cx);
                } else if count > 1 {
                    self.open_palette("スタッシュを復元", window, cx);
                    return;
                } else {
                    self.fail("スタッシュは空です。下書きを入力して ⌘S で退避します");
                }
            }
            Act::Unstash(i) => {
                if let Some(list) = self.ui["stash"].as_array_mut().filter(|l| i < l.len()) {
                    let text = list.remove(i).as_str().unwrap_or("").to_string();
                    self.save_ui();
                    self.input.update(cx, |s, cx| {
                        set_input(s, text, window, cx);
                        s.focus(window, cx);
                    });
                }
            }
            Act::Snooze(secs) => {
                let t = &mut self.threads[self.active];
                let action = nav::SidebarAction::Snooze {
                    id: t.meta.id.clone(),
                    value: t.snoozed,
                };
                t.snoozed = now() + secs;
                self.undo.record(action);
                // Move on to the newest thread that is still visible.
                if let Some(ix) = (0..self.threads.len()).find(|i| {
                    *i != self.active
                        && self.threads[*i].snoozed <= now()
                        && !self.threads[*i].archived
                }) {
                    if let Err(e) = self.thread().save() {
                        self.fail(e);
                    }
                    self.select(ix, window, cx);
                }
            }
            Act::WakeAll => {
                for t in &mut self.threads {
                    if t.snoozed != 0 {
                        t.snoozed = 0;
                        let _ = t.save();
                    }
                }
            }
            Act::Unschedule(i) => self.unschedule(i),
            Act::NewScratch => {
                let dir = greencode_core::home().join("scratch").join(format!(
                    "{:x}",
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_micros()
                ));
                match std::fs::create_dir_all(&dir) {
                    Ok(()) => self.new_thread(dir, false, window, cx),
                    Err(e) => self.fail(format!("作業フォルダを作成できません: {e}")),
                }
            }
            Act::SaveProjectDefault => {
                let t = self.thread();
                let (key, value) = (
                    t.project().to_string_lossy().into_owned(),
                    json!({"model": t.meta.model, "backend": t.meta.backend, "mode": t.perms.mode()}),
                );
                if !self.ui["projects"].is_object() {
                    self.ui["projects"] = json!({});
                }
                self.ui["projects"][key] = value;
                self.save_ui();
                self.notice("このプロジェクトの新規スレッドは、現在のモデル・バックエンド・権限で始まります");
            }
            Act::SnapshotToggle => self.snapshot_toggle(window, cx),
            Act::SnapshotText => self.snapshot_toggle_text(window, cx),
            Act::SnapshotShortcut => self.snapshot_cycle_shortcut(window, cx),
            Act::TextModel(m) => self.set_text_model(m),
            Act::CycleTextModel => self.cycle_text_model(),
            Act::RegenerateTitle => self.request_title(self.active, true, cx),
            Act::GenerateCommit => self.generate_git(false, cx),
            Act::GeneratePr => self.generate_git(true, cx),
            Act::CycleVoiceProvider => {
                // Local transcription uses whisper-cli when it and a model are installed.
                let local = greencode_core::voice::local_available();
                self.ui["voice_provider"] = json!(voice::next_provider(self.ui["voice_provider"].as_str(), local));
                self.save_ui();
            }
            Act::VoiceAction(what) => self.voice_action(what, cx),
            Act::ToggleExt(name) => {
                let off = &mut Arc::make_mut(&mut self.cfg).ext_off;
                let now_off = !off.iter().any(|o| o == name);
                if now_off {
                    off.push(name.into())
                } else {
                    off.retain(|o| o != name)
                }
                self.ui["ext_off"] = json!(off);
                self.save_ui();
                self.notice(format!(
                    "拡張ツール {name} を{}にしました（新しく開始するエージェントから反映）",
                    if now_off { "オフ" } else { "オン" }
                ));
            }
            Act::CycleCompaction => {
                let cfg = Arc::make_mut(&mut self.cfg);
                let next = match (cfg.compaction_enabled, cfg.compaction.as_str()) {
                    (false, _) => "summary",
                    (true, "snap") => "off",
                    _ => "snap",
                };
                cfg.compaction_enabled = next != "off";
                if next != "off" {
                    cfg.compaction = next.into();
                }
                self.ui["compaction"] = json!(next);
                self.save_ui();
            }
            Act::SetLearning(auto) => {
                Arc::make_mut(&mut self.cfg).learning.auto = auto;
                self.ui["learning_auto"] = json!(auto);
                self.save_ui();
                self.notice(if auto { "自己改善: 自動学習をオンにしました（Lesson行をローカル保存。追加リクエストなし）" } else { "自己改善: 自動学習をオフにしました（ext/learn は利用できます）" });
            }
            Act::Toggle(key) => {
                self.ui[key] = json!(self.ui[key] != true);
                self.save_ui();
                self.list.reset(self.thread().items.len());
                if key == "labs_workflows" {
                    if self.ui[key] == true {
                        self.start_workflow_engine(cx);
                        self.notice("ワークフローを有効にしました（設定 → 自動化）");
                    } else {
                        self.stop_workflow_engine();
                        self.notice("ワークフローをオフにしました。実行は保持され、オンに戻すと続きから進みます");
                    }
                }
            }
            Act::SendShortcut => {
                let cmd_enter = self.ui["send_shortcut"] != "cmd+enter";
                self.ui["send_shortcut"] = json!(if cmd_enter { "cmd+enter" } else { "enter" });
                self.input
                    .update(cx, |s, cx| s.set_submit_on_enter(!cmd_enter, cx));
                self.save_ui();
                self.notice(if cmd_enter {
                    "送信キー: ⌘Enter で送信（Enter は改行）"
                } else {
                    "送信キー: Enter で送信（Shift+Enter は改行）"
                });
            }
            Act::CycleQuitMode => {
                let next = match self.ui["quit_mode"].as_str() {
                    Some("direct") => "hold",
                    Some("hold") => "double",
                    _ => "direct",
                };
                self.ui["quit_mode"] = json!(next);
                self.save_ui();
                self.notice(format!(
                    "⌘Q の終了方法: {}",
                    nav::quit_mode_label(Some(next))
                ));
            }
            Act::RefreshBackendModels => {
                self.refresh_backend_models(true, cx);
                self.notice("外部エージェントのモデル一覧を取得しています…");
            }
            Act::BackendModel(backend, model) => {
                if self.thread().meta.backend == backend {
                    return self.run(Act::Model(model), window, cx);
                }
                // An agent owns its history, so a started thread cannot change backend: continue in a new one.
                let started = self.thread().live()
                    || Session::open(&self.thread().meta.id).is_some()
                    || self
                        .thread()
                        .items
                        .iter()
                        .any(|item| !matches!(item, Item::Notice(_)));
                if started {
                    let (cwd, repo) = (
                        self.thread().meta.cwd.clone(),
                        self.thread().meta.repo.clone(),
                    );
                    let pending = self.input.read(cx).value().to_string();
                    let history = Session::handoff(&self.thread().meta.id, 24_000);
                    self.new_thread(self.thread().project(), false, window, cx);
                    (
                        self.threads[self.active].meta.cwd,
                        self.threads[self.active].meta.repo,
                    ) = (cwd, repo);
                    self.threads[self.active].draft = pending.clone();
                    match history {
                        Ok((messages, omitted)) if !messages.is_empty() => {
                            let mut draft = String::from("以前の会話（参考情報）:\n");
                            if omitted {
                                draft.push_str("[会話の一部は省略されています]\n");
                            }
                            for message in messages {
                                draft.push_str(&format!("\n{:?}:\n", message.role));
                                for block in message.content {
                                    if let greencode_core::Block::Text { text } = block {
                                        draft.push_str(&text);
                                        draft.push('\n');
                                    }
                                }
                            }
                            draft.push_str("\n続きの依頼: ");
                            draft.push_str(&pending);
                            self.threads[self.active].draft = draft;
                            self.notice("保存済み会話を下書きに用意しました。内容を確認してから送信してください。実行中の発言や画像・ツール情報は含まれない場合があります。");
                        }
                        _ => self.notice(
                            "新しいスレッドを作成しました。保存済み会話を引き継げませんでした。",
                        ),
                    }
                }
                if let Some((text, ..)) = self.threads[self.active].deferred.take() {
                    self.restore_text(self.active, text, cx);
                }
                self.retire_warm_agent(self.active);
                let t = &mut self.threads[self.active];
                (t.meta.backend, t.meta.model, t.models) = (backend, model, vec![]);
                if let Err(e) = t.save() {
                    self.fail(e);
                }
                self.activate(self.active, window, cx);
            }
            Act::Undo => self.run(Act::Rewind(0), window, cx),
            Act::BashDiff => self.bash_diff(cx),
            // T3's voice output: macOS `say` reads the last reply.
            Act::Speak => match self.thread().items.iter().rev().find_map(|i| {
                if let Item::Assistant(text) = i {
                    Some(text.clone())
                } else {
                    None
                }
            }) {
                Some(text) => {
                    let cfg = self.cfg.clone();
                    let rt = self.rt.handle().clone();
                    rt.spawn(async move {
                        let spoken = if cfg.services_opts.speech.enhanced {
                            greencode_core::speech::enhanced(&cfg, &text).await
                        } else {
                            speech_text(&text)
                        };
                        greencode_core::speech::speak_voice(
                            &cfg.services_opts.speech.voice,
                            cfg.services_opts.speech.speed,
                            &spoken,
                        );
                    });
                }
                None => self.notice("読み上げる返答がまだありません"),
            },
            // T3's `$skill`: load the skill's instructions into the draft so the agent can follow them.
            Act::Skill(name) => {
                match greencode_core::skills::body(&self.thread().meta.cwd, &self.cfg, &name) {
                    Some(body) if !body.is_empty() => {
                        let mut draft = self.input.read(cx).value().to_string();
                        if !draft.is_empty() && !draft.ends_with('\n') {
                            draft.push('\n');
                        }
                        draft.push_str(&format!("スキル `{name}` に従ってください:\n\n{body}\n"));
                        self.input.update(cx, |s, cx| {
                            set_input(s, draft, window, cx);
                            s.focus(window, cx);
                        });
                        self.notice(format!("スキル「{name}」を下書きに読み込みました"));
                    }
                    _ => self.fail(format!("スキル「{name}」が見つかりません")),
                }
            }
            Act::Retry => match self.thread().last_prompt.clone() {
                Some(prompt) if self.thread().status == Status::Idle => {
                    self.start_prompt(prompt, cx)
                }
                _ => self.notice("再試行できるプロンプトがありません"),
            },
            Act::ResumeAtReset => self.resume_at_reset(),
            Act::ToolVisibility => {
                self.cycle_tool_visibility();
                self.list.reset(self.thread().items.len());
                cx.notify();
            }
            // T3's "Triage an issue": a scratch thread that investigates GreenCode itself.
            Act::Triage => {
                let dir = greencode_core::home().join("scratch").join(format!(
                    "triage-{:x}",
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map_or(0, |d| d.as_nanos())
                ));
                match std::fs::create_dir_all(&dir) {
                    Ok(()) => {
                        self.new_thread(dir, false, window, cx);
                        let prompt = greencode_core::triage::prompt("");
                        self.input.update(cx, |s, cx| {
                            set_input(s, prompt, window, cx);
                            s.focus(window, cx);
                        });
                        self.notice("症状を書き足して送信してください。");
                    }
                    Err(e) => self.fail(format!("作業フォルダを作成できません: {e}")),
                }
            }
            // T3's "Share with guest": copy a read-only link that expires after 24 hours.
            Act::GuestLink => {
                let id = self.thread().meta.id.clone();
                match greencode_core::remote::share_link(&id) {
                    Ok(url) => {
                        cx.write_to_clipboard(gpui_kit::ClipboardItem::new_string(url));
                        self.notice("ゲストリンクをコピーしました（24時間有効・読み取り専用）");
                    }
                    Err(e) => self.fail(format!("ゲストリンクを作成できません: {e:#}")),
                }
            }
            Act::ToggleDisabled(key, name) => {
                self.toggle_disabled(key, &name);
                self.refresh_models(cx);
                cx.notify();
            }
            // T3's "Restart agent session": keep the transcript, start a fresh provider session next turn.
            Act::RestartAgent => {
                let t = &self.threads[self.active];
                if t.status != Status::Idle {
                    self.fail("実行中はエージェントを再起動できません");
                    cx.notify();
                    return;
                }
                let (id, backend) = (t.meta.id.clone(), t.meta.backend.clone());
                self.threads[self.active].cmd = None;
                let cleared = Session::clear_runtime(&id);
                self.threads[self.active].models.clear();
                self.threads[self.active].config_options.clear();
                match cleared {
                    Ok(()) => self.notice(format!("{backend} のセッションを再起動しました。次のメッセージで新しいセッションが始まります。")),
                    Err(e) => self.fail(format!("セッションを再起動できません: {e:#}")),
                }
                cx.notify();
            }
            Act::TurnDiff(back) => match Session::turn_diff(&self.thread().meta.id, back) {
                Ok(diff) if diff.is_empty() => {
                    self.notice("このターンで write・edit が変更したファイルはありません")
                }
                Ok(diff) => {
                    // Shown until the panel next refreshes to the uncommitted changes.
                    self.show_diff = true;
                    self.files_panel = None;
                    self.pr_panel = None;
                    self.diff_generation = self.diff_generation.wrapping_add(1);
                    self.diff = diff
                        .lines()
                        .map(|l| SharedString::from(l.to_string()))
                        .collect();
                }
                Err(e) => self.fail(format!("差分を作れません: {e:#}")),
            },
            Act::Rewind(back) => {
                let t = &self.threads[self.active];
                if t.status != Status::Idle {
                    self.fail("取り消しは、実行中でないスレッドでのみ使えます");
                    cx.notify();
                    return;
                }
                let external = t.meta.backend != "native";
                match Session::open(&t.meta.id).map(|_| Session::rewind(&t.meta.id, back)) {
                    Some(Ok(Some(prompt))) => {
                        let t = &mut self.threads[self.active];
                        // The running agent keeps the history in memory; make it reload from the file.
                        if external {
                            // An external agent owns its history, so the truncated transcript starts a fresh session.
                            t.cmd = None;
                            let _ = Session::clear_runtime(&t.meta.id);
                            t.models.clear();
                            t.config_options.clear();
                        } else if let Some(cmd) = &t.cmd {
                            let _ = cmd.send(Command::Config(self.cfg.clone()));
                        }
                        (t.loaded, t.draft) = (false, prompt);
                        self.activate(self.active, window, cx);
                        // `bash` changes are not in the tool-level restore; a checkpoint covers them.
                        match self.restore_checkpoint(back) {
                            Some(n) if n > 0 => {
                                self.notice(format!("bash で変更された {n} ファイルも戻しました"))
                            }
                            _ => {}
                        }
                    }
                    Some(Err(e)) => self.fail(format!("取り消せません: {e:#}")),
                    _ => self.fail("取り消すターンがありません"),
                }
            }
            Act::Side(question) => {
                let parent = self.thread().meta.id.clone();
                // The session file is copied as it stands, so the parent may keep running.
                match (self.thread().meta.backend == "native" && Session::open(&parent).is_some())
                    .then(|| Session::fork(&parent))
                {
                    Some(Ok(mut meta)) => {
                        meta.title =
                            format!("side: {}", question.chars().take(50).collect::<String>());
                        let side = Thread::new(meta, false, self.thread().perms.mode());
                        if let Err(e) = side.save() {
                            self.fail(e);
                        }
                        if !self.ui["side"].is_object() {
                            self.ui["side"] = json!({});
                        }
                        self.ui["side"][side.meta.id.as_str()] = json!(parent);
                        self.save_ui();
                        self.threads.insert(0, side);
                        self.active += 1;
                        self.select(0, window, cx);
                        let question = if self.cfg.interaction_opts.btw.inject {
                            format!("{question}\n\n{}", greencode_core::btw::DRAFT_INSTRUCTION)
                        } else {
                            question
                        };
                        self.start_prompt(question, cx);
                    }
                    Some(Err(e)) => self.fail(format!("サイドチャットを開けません: {e:#}")),
                    None => self.fail("サイドチャットは、会話のあるネイティブスレッドで使えます"),
                }
            }
            Act::InjectBtw => {
                let parent = self.ui["side"][self.thread().meta.id.as_str()]
                    .as_str()
                    .and_then(|id| self.threads.iter().position(|t| t.meta.id == id));
                let answer = self.thread().items.iter().rev().find_map(|i| {
                    if let Item::Assistant(a) = i {
                        Some(a.clone())
                    } else {
                        None
                    }
                });
                let block = parent.zip(answer).and_then(|(ix, answer)| {
                    greencode_core::btw::inject_block(&answer, self.cfg.interaction_opts.btw.inject).map(|t| (ix, t))
                });
                match block {
                    Some((ix, text)) => {
                        self.select(ix, window, cx);
                        // Sent as a steer so it joins a running turn instead of waiting for it.
                        self.run(Act::Ask(text), window, cx);
                    }
                    None => self.fail("このサイドチャットには送れる <inject> ブロックがありません"),
                }
            }
            Act::BringBack => {
                let parent = self.ui["side"][self.thread().meta.id.as_str()]
                    .as_str()
                    .and_then(|id| self.threads.iter().position(|t| t.meta.id == id));
                let answer = self.thread().items.iter().rev().find_map(|i| {
                    if let Item::Assistant(a) = i {
                        Some(a.clone())
                    } else {
                        None
                    }
                });
                match (parent, answer) {
                    (Some(ix), Some(answer)) => {
                        let title = self.thread().meta.title.clone();
                        self.select(ix, window, cx);
                        let mut value = self.input.read(cx).value().to_string();
                        if !value.is_empty() && !value.ends_with('\n') {
                            value.push('\n');
                        }
                        value.push_str(&format!(
                            "{title}\n{}\n",
                            answer
                                .lines()
                                .map(|l| format!("> {l}\n"))
                                .collect::<String>()
                        ));
                        self.input.update(cx, |s, cx| {
                            set_input(s, value, window, cx);
                            s.focus(window, cx);
                        });
                    }
                    (None, _) => self.fail(
                        "このスレッドはサイドチャットではないか、元のスレッドがもうありません",
                    ),
                    (_, None) => self.fail("持ち帰る回答がまだありません"),
                }
            }
            Act::Fork => {
                let t = &self.threads[self.active];
                match (t.meta.backend == "native" && Session::open(&t.meta.id).is_some())
                    .then(|| Session::fork(&t.meta.id))
                {
                    Some(Ok(meta)) => {
                        self.threads
                            .insert(0, Thread::new(meta, false, self.cfg.mode));
                        self.active += 1;
                        self.select(0, window, cx);
                    }
                    Some(Err(e)) => self.fail(format!("複製できません: {e:#}")),
                    None => self.fail("複製できるのは、会話のあるネイティブスレッドだけです"),
                }
            }
            Act::Export => {
                let id = self.thread().meta.id.clone();
                let dir = greencode_core::home().join("exports");
                let path = dir.join(format!("{id}.md"));
                match Session::markdown(&id).and_then(|md| {
                    Ok(std::fs::create_dir_all(&dir).and_then(|_| std::fs::write(&path, md))?)
                }) {
                    Ok(()) => {
                        self.notice(format!("エクスポートしました: {}", path.display()));
                        spawn_external("/usr/bin/open", &["-R", &path.to_string_lossy()]);
                    }
                    Err(e) => self.fail(format!("エクスポートできません: {e:#}")),
                }
            }
            Act::Attach => {
                let rx = cx.prompt_for_paths(PathPromptOptions {
                    files: true,
                    directories: false,
                    multiple: true,
                    prompt: None,
                });
                cx.spawn_in(window, async move |this, cx| {
                    if let Ok(Ok(Some(paths))) = rx.await {
                        this.update_in(cx, |this, window, cx| this.attach_paths(paths, window, cx))
                            .ok();
                    }
                })
                .detach();
            }
            Act::Sandbox => {
                let on = !greencode_core::tools::SANDBOX
                    .fetch_xor(true, std::sync::atomic::Ordering::Relaxed);
                self.ui["sandbox"] = json!(on);
                self.save_ui();
                self.notice(if on {
                    "サンドボックス: オン（bash はプロジェクト・一時フォルダ以外に書き込めません）"
                } else {
                    "サンドボックス: オフ"
                });
            }
            Act::Effort(level) => self.set_effort(level),
            Act::EffortStep(up) => {
                let levels =
                    greencode_core::provider::effort_levels(&self.cfg, &self.thread().meta.model);
                if levels.is_empty() {
                    self.notice("このモデルには思考の強さの段階がありません");
                } else {
                    let current = self
                        .cfg
                        .effort
                        .as_deref()
                        .and_then(|value| levels.iter().position(|level| *level == value));
                    let next = match current {
                        Some(i) if up => levels[(i + 1) % levels.len()],
                        Some(i) => levels[(i + levels.len() - 1) % levels.len()],
                        None if up => levels[0],
                        None => levels[levels.len() - 1],
                    };
                    self.set_effort(Some(next.to_string()));
                }
            }
            Act::OpenCodeEffort(level) => {
                let t = &mut self.threads[self.active];
                if t.status != Status::Idle {
                    self.fail("実行中は思考の強さを変更できません");
                    cx.notify();
                    return;
                }
                if t.opencode_effort != level {
                    t.opencode_effort = level;
                    t.cmd = None;
                }
            }
            Act::OpenFile(path) => {
                // TextEdit refuses a missing file.
                if !path.exists() {
                    if let Err(e) = std::fs::create_dir_all(greencode_core::home())
                        .and_then(|_| std::fs::write(&path, ""))
                    {
                        self.fail(e);
                    }
                }
                spawn_external("/usr/bin/open", &["-t", &path.to_string_lossy()]);
            }
            Act::Revoke(key) => {
                // Every thread loaded its own copy of the rules.
                let failed = self
                    .threads
                    .iter()
                    .filter_map(|t| t.perms.revoke(&key).err())
                    .next_back();
                match failed {
                    Some(e) => self.fail(format!("許可ルールを削除できません: {e}")),
                    None => self.notice(format!("許可ルールを削除しました: {key}")),
                }
            }
            Act::Theme(choice) => self.set_theme(choice, window, cx),
            Act::ThemeMode(mode) => self.set_theme_mode(mode, window, cx),
            Act::CycleThemeMode => {
                let mode = self.theme_mode().cycle();
                self.set_theme_mode(mode, window, cx);
            }
            Act::ImportTheme => {
                let rx = cx.prompt_for_paths(PathPromptOptions {
                    files: true,
                    directories: false,
                    multiple: false,
                    prompt: Some("テーマを読み込む（VS Code / GreenCode JSON）".into()),
                });
                cx.spawn_in(window, async move |this, cx| {
                    if let Ok(Ok(Some(paths))) = rx.await {
                        if let Some(path) = paths.first() {
                            this.update_in(cx, |this, window, cx| {
                                this.import_theme(path, window, cx)
                            })
                            .ok();
                        }
                    }
                })
                .detach();
            }
            Act::ExportTheme => self.export_theme(window),
            Act::EditTheme => self.open_theme_editor(window, cx),
            Act::PickBackground => {
                let rx = cx.prompt_for_paths(PathPromptOptions {
                    files: true,
                    directories: false,
                    multiple: false,
                    prompt: Some("背景画像（JPEG / PNG / WebP、16 MB まで）".into()),
                });
                cx.spawn_in(window, async move |this, cx| {
                    if let Ok(Ok(Some(paths))) = rx.await {
                        if let Some(path) = paths.first() {
                            this.update_in(cx, |this, window, cx| {
                                match greencode_core::theme::check_background(path) {
                                    Ok(()) => {
                                        this.ui["background_image"] =
                                            json!(path.to_string_lossy());
                                        this.save_ui();
                                        this.apply_theme(window, cx);
                                        this.notice(format!(
                                            "背景画像を設定しました（不透明度 {}%）",
                                            (this.background_opacity() * 100.).round() as u32
                                        ));
                                    }
                                    Err(e) => this.fail(format!("背景画像を設定できません: {e:#}")),
                                }
                                cx.notify();
                            })
                            .ok();
                        }
                    }
                })
                .detach();
            }
            Act::ClearBackground => {
                if let Some(object) = self.ui.as_object_mut() {
                    object.remove("background_image");
                }
                self.save_ui();
                self.apply_theme(window, cx);
                self.notice("背景画像をクリアしました");
                cx.notify();
            }
            Act::BackgroundOpacity(delta) => {
                let percent = ((self.background_opacity() * 100.).round() as i16 + delta)
                    .clamp(0, 100);
                self.ui["background_opacity"] = json!(percent);
                self.save_ui();
                self.apply_theme(window, cx);
                cx.notify();
            }
            Act::ImportSessionFiles => {
                let rx = cx.prompt_for_paths(PathPromptOptions {
                    files: true,
                    directories: false,
                    multiple: true,
                    prompt: Some("セッションをインポート".into()),
                });
                cx.spawn_in(window, async move |this, cx| {
                    if let Ok(Ok(Some(paths))) = rx.await {
                        this.update_in(cx, |this, window, cx| {
                            this.import_session_paths(paths, window, cx)
                        })
                        .ok();
                    }
                })
                .detach();
            }
            Act::ImportSession(found) => {
                // The source tool's model is not a native provider/model; keep the current native one.
                let model = if self.thread().meta.backend == "native" {
                    self.thread().meta.model.clone()
                } else {
                    self.cfg.model.clone()
                };
                match import::import_found(&found, &model) {
                    Ok(meta) => {
                        self.threads
                            .insert(0, Thread::new(meta, false, self.cfg.mode));
                        self.active += 1;
                        self.select(0, window, cx);
                    }
                    Err(e) => self.fail(format!("セッションをインポートできません: {e:#}")),
                }
            }
            Act::AddProvider => {
                let mut subs = vec![];
                let fields = [
                    ("名前（例: my-llm）", false),
                    ("Base URL（groq など組み込み名なら空欄可）", false),
                    ("APIキー（不要なら空欄）", true),
                    ("モデル（カンマ区切り）", false),
                ]
                .map(|(hint, masked)| {
                    let state =
                        cx.new(|cx| InputState::new(window, cx).placeholder(hint).masked(masked));
                    subs.push(cx.subscribe_in(
                        &state,
                        window,
                        |this, _, ev: &InputEvent, window, cx| {
                            if let InputEvent::PressEnter { .. } = ev {
                                this.submit_provider(window, cx)
                            }
                        },
                    ));
                    state
                });
                fields[0].update(cx, |s, cx| s.focus(window, cx));
                self.provider_form = Some(ProviderForm {
                    fields,
                    anthropic: false,
                    error: None,
                    _subs: subs,
                });
            }
            Act::Logout(name) => self.logout(&name),
            Act::SwitchAccount(provider, account) => self.switch_account(&provider, &account, cx),
            Act::SaveAccount(provider) => self.open_account_form(provider, window, cx),
            Act::LoginProvider(name) if name == "github-copilot" => {
                self.run(Act::CopilotLogin, window, cx)
            }
            Act::LoginProvider(name) if name == "chatgpt" => {
                self.run(Act::ChatGptLogin, window, cx)
            }
            Act::SignIn(name) => {
                if let Some(kind) = DeviceKind::login(&name) {
                    self.start_device_login(kind, window, cx);
                }
            }
            // Sign-in only (no API key to paste): go straight to the browser.
            Act::LoginProvider(name)
                if DeviceKind::login(&name).is_some()
                    && greencode_core::config::preset(&name).is_some_and(|p| p.env.is_empty()) =>
            {
                self.run(Act::SignIn(name), window, cx)
            }
            Act::LoginProvider(name) if self.cfg.providers.contains_key(&name) => {
                if greencode_core::auth::status(&name, &self.cfg.providers[&name])
                    == greencode_core::auth::Method::Keyless
                {
                    self.notice(format!("{name} はキー不要です。"));
                } else {
                    self.open_key_form(name, window, cx);
                }
            }
            Act::LoginProvider(name) => {
                self.run(Act::AddProvider, window, cx);
                if let Some(form) = &mut self.provider_form {
                    form.anthropic = greencode_core::config::preset(&name)
                        .is_some_and(|p| p.kind == "anthropic");
                    form.fields[0].update(cx, |s, cx| s.set_value(name, window, cx));
                }
            }
            Act::Model(m) => {
                let t = &mut self.threads[self.active];
                if t.meta.backend == "opencode" && t.opencode_effort.is_some() && t.meta.model != m
                {
                    if t.status != Status::Idle {
                        self.fail("実行中はモデルを変更できません");
                        cx.notify();
                        return;
                    }
                    t.cmd = None;
                    t.meta.model = m;
                    if let Err(e) = t.save() {
                        self.fail(e);
                    }
                    cx.notify();
                    return;
                }
                if let Some(cmd) = &t.cmd {
                    if cmd.send(Command::Model(m.clone())).is_err() {
                        self.fail("モデルを変更できません: エージェントが終了しています");
                        cx.notify();
                        return;
                    }
                    if t.meta.backend != "native" {
                        cx.notify();
                        return;
                    }
                }
                t.meta.model = m;
                if let Err(e) = t.save() {
                    self.fail(e);
                }
            }
            Act::SubagentModel(model) => {
                let t = &mut self.threads[self.active];
                if t.meta.backend == "native" {
                    if let Some(cmd) = &t.cmd {
                        let _ = cmd.send(Command::SubagentModel(model.clone()));
                    }
                    t.meta.subagent_model = model;
                }
            }
            Act::Backend(b) => {
                let t = &mut self.threads[self.active];
                // An external agent owns its own history, so the backend is fixed once a thread has started.
                if !t.live() && t.items.is_empty() {
                    if let Some((text, ..)) = t.deferred.take() {
                        self.restore_text(self.active, text, cx);
                    }
                    self.retire_warm_agent(self.active);
                    let t = &mut self.threads[self.active];
                    t.meta.backend = b;
                    t.meta.model = if t.meta.backend == "native" {
                        self.cfg.model.clone()
                    } else {
                        "default".into()
                    };
                    self.commands = command_list(&self.cfg, &self.threads[self.active]);
                } else {
                    // Same as picking another backend's model: continue in a new thread.
                    let model = if b == "native" {
                        self.cfg.model.clone()
                    } else {
                        "default".into()
                    };
                    return self.run(Act::BackendModel(b, model), window, cx);
                }
            }
            Act::Thread(id) => {
                if let Some(ix) = self.threads.iter().position(|t| t.meta.id == id) {
                    self.select(ix, window, cx);
                }
            }
            Act::Rpc(value) => {
                let busy = self.thread().status != Status::Idle
                    && !live_commands(&self.cfg, &self.thread().meta.backend);
                let t = &mut self.threads[self.active];
                let option_id = (value["type"] == "set_config_option")
                    .then(|| value["configId"].as_str())
                    .flatten()
                    .map(str::to_string);
                if value["type"] == "set_thinking_level" && t.cmd.is_none() {
                    t.pending_thinking = value["level"].as_str().map(str::to_string);
                // A busy backend without a live channel would run it after the turn and report a second
                // `Done`, which the GUI would take for the end of the next prompt.
                } else if let Some(id) = option_id.filter(|id| {
                    busy || t.cmd.is_none()
                        || !t
                            .config_options
                            .iter()
                            .any(|o| o["id"].as_str() == Some(id.as_str()))
                }) {
                    t.pending_options.retain(|(existing, _)| existing != &id);
                    t.pending_options.push((id, value["value"].clone()));
                } else if busy {
                    self.fail("実行中のため使えません。ターンが終わってからもう一度選んでください");
                    return;
                } else {
                    self.ensure_agent(self.active, cx);
                    let t = &mut self.threads[self.active];
                    if t.status == Status::Idle {
                        t.status = Status::Running;
                    }
                    if let Some(cmd) = &t.cmd {
                        let _ = cmd.send(Command::Remote(value));
                    }
                }
            }
        }
        if let Err(e) = self.thread().save() {
            self.fail(e);
        }
        cx.notify();
    }

    fn close_provider_form(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.provider_form = None;
        self.input.update(cx, |s, cx| s.focus(window, cx));
        cx.notify();
    }

    fn submit_provider(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(form) = &self.provider_form else {
            return;
        };
        let [name, base_url, key, models] = form
            .fields
            .each_ref()
            .map(|f| f.read(cx).value().trim().to_string());
        let models: Vec<String> = models
            .split(',')
            .map(str::trim)
            .filter(|m| !m.is_empty())
            .map(String::from)
            .collect();
        let first = models.first().map(|m| format!("{name}/{m}"));
        let p = ProviderCfg {
            kind: if form.anthropic {
                "anthropic"
            } else {
                "openai"
            }
            .into(),
            base_url,
            api_key: (!key.is_empty()).then_some(key),
            models,
            ..Default::default()
        };
        match Arc::make_mut(&mut self.cfg).add_provider(&name, p) {
            Ok(()) => {
                self.push_config();
                self.close_provider_form(window, cx);
                self.refresh_models(cx);
                let t = &mut self.threads[self.active];
                t.items.push(Item::Notice(
                    format!("プロバイダ「{name}」を追加しました。").into(),
                ));
                self.list.reset(t.items.len());
                if let Some(model) = first.filter(|_| self.thread().meta.backend == "native") {
                    self.run(Act::Model(model), window, cx);
                }
            }
            Err(e) => {
                self.provider_form.as_mut().unwrap().error = Some(format!("{e:#}").into());
                cx.notify();
            }
        }
    }

    /// Appends a file path to the composer; image paths are attached when the prompt is sent.
    fn insert_path(&mut self, path: &std::path::Path, window: &mut Window, cx: &mut Context<Self>) {
        let mut value = self.input.read(cx).value().to_string();
        if !value.is_empty() && !value.ends_with(char::is_whitespace) {
            value.push(' ');
        }
        value.push_str(&format!("{} ", path.display()));
        self.input
            .update(cx, |s, cx| set_input(s, value, window, cx));
        cx.notify();
    }

    fn insert_terminal_output(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(term) = self.active_terminal() else {
            return;
        };
        let output = term
            .lines
            .iter()
            .rev()
            .take(14)
            .rev()
            .map(|line| terminal::plain(line))
            .collect::<Vec<_>>()
            .join("\n");
        if output.trim().is_empty() {
            return;
        }
        let mut draft = self.input.read(cx).value().to_string();
        if !draft.is_empty() {
            draft.push_str("\n\n");
        }
        draft.push_str("ターミナル出力:\n");
        draft.push_str(&output);
        self.input.update(cx, |s, cx| {
            set_input(s, draft, window, cx);
            s.focus(window, cx);
        });
        cx.notify();
    }

    fn insert_terminal_selection(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(term) = self.active_terminal() else {
            return;
        };
        let Some((start, end)) = term.selection else {
            return;
        };
        let (a, b) = (start.min(end), start.max(end));
        if b >= term.lines.len() {
            return;
        }
        let output = term.lines[a..=b]
            .iter()
            .map(|line| terminal::plain(line))
            .collect::<Vec<_>>()
            .join("\n");
        let text = format!(
            "ターミナル {} の {}-{} 行目:\n```\n{}\n```",
            term.number,
            a + 1,
            b + 1,
            output
        );
        let mut draft = self.input.read(cx).value().to_string();
        if !draft.is_empty() {
            draft.push_str("\n\n");
        }
        draft.push_str(&text);
        self.input.update(cx, |s, cx| {
            set_input(s, draft, window, cx);
            s.focus(window, cx);
        });
        cx.notify();
    }

    /// Saves a clipboard image as a file and puts its path in the composer; files copied in Finder are
    /// attached like a drop. False: the clipboard holds neither.
    fn paste_image(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let Some(entry) = cx.read_from_clipboard().and_then(|item| {
            item.into_entries()
                .find(|e| matches!(e, ClipboardEntry::Image(_) | ClipboardEntry::ExternalPaths(_)))
        }) else {
            return false;
        };
        let image = match entry {
            ClipboardEntry::ExternalPaths(paths) => {
                self.attach_paths(paths.paths().to_vec(), window, cx);
                return true;
            }
            ClipboardEntry::Image(image) => image,
            _ => return false,
        };
        let (ext, bytes) = match image.format {
            ImageFormat::Png => ("png", image.bytes),
            ImageFormat::Jpeg => ("jpg", image.bytes),
            ImageFormat::Gif => ("gif", image.bytes),
            ImageFormat::Webp => ("webp", image.bytes),
            // Safari and Preview often copy only TIFF; models take PNG.
            ImageFormat::Tiff | ImageFormat::Bmp => {
                match greencode_core::image::to_png("image/tiff", &image.bytes) {
                    Some(png) => ("png", png),
                    None => {
                        self.fail("クリップボードの画像を読み込めません".to_string());
                        return true;
                    }
                }
            }
            _ => return false,
        };
        let dir = greencode_core::home().join("attachments");
        let path = dir.join(format!("{:x}.{ext}", image.id));
        match std::fs::create_dir_all(&dir).and_then(|_| std::fs::write(&path, &bytes)) {
            Ok(()) => self.attach_image(path, cx),
            Err(e) => self.fail(format!("画像を保存できません: {e}")),
        }
        true
    }

    /// Pastes a clipboard image, or text of at least 32 KiB (unless ⇧ is held), as an attachment. False: let the input paste it.
    fn paste_attachment(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        self.paste_image(window, cx)
            || (!window.modifiers().shift && self.paste_large_text(window, cx))
    }

    /// Attaches a clipboard text of at least 32 KiB as a file instead of filling the composer. False: not applicable.
    fn paste_large_text(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) else {
            return false;
        };
        if text.len() < 32_768 {
            return false;
        }
        let dir = greencode_core::home().join("pastes");
        let stamp = now();
        let saved = std::fs::create_dir_all(&dir).and_then(|_| {
            let mut n = 0;
            loop {
                let name = if n == 0 {
                    format!("paste-{stamp}.txt")
                } else {
                    format!("paste-{stamp}-{n}.txt")
                };
                let path = dir.join(name);
                match std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&path)
                {
                    Ok(mut f) => {
                        std::io::Write::write_all(&mut f, text.as_bytes())?;
                        return Ok(path);
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => n += 1,
                    Err(e) => return Err(e),
                }
            }
        });
        match saved {
            Ok(path) => {
                self.insert_path(
                    std::path::Path::new(&format!("@{}", path.display())),
                    window,
                    cx,
                );
                self.notice(format!(
                    "大きな貼り付け（{} KB）をファイルとして添付しました",
                    text.len() / 1024
                ));
            }
            Err(e) => {
                self.fail(format!("貼り付けを保存できません: {e}"));
                return false;
            }
        }
        true
    }

    fn hermes_setup(
        &mut self,
        backend: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let command = greencode_core::hermes::setup_command(backend);
        self.settings = None;
        if !self.terminal_visible {
            self.open_terminal(window, cx);
        }
        if self.active_terminal().is_some() {
            if cfg!(test) {
                self.active_terminal_mut()
                    .unwrap()
                    .lines
                    .push(format!("$ {command}").into());
            } else {
                self.write_terminal(format!("{command}\n").into_bytes());
            }
        }
        cx.notify();
    }

    fn import_hermes(
        &mut self,
        profile: hermes::Profile,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match hermes::import(&profile, Arc::make_mut(&mut self.cfg)) {
            Ok(backend) => {
                self.settings = None;
                self.push_config();
                self.new_thread(self.thread().project(), false, window, cx);
                let t = &mut self.threads[self.active];
                t.meta.backend = backend;
                t.meta.model = "default".into();
                t.items.push(Item::Notice(format!("Hermes「{}」をインポートしました。元のプロファイルの設定・認証・スキル・メモリを使用します。", profile.name).into()));
                self.list.reset(t.items.len());
                self.refresh_backend_models(false, cx);
            }
            Err(e) => self.fail(format!("Hermesプロファイルをインポートできません: {e:#}")),
        }
    }

    /// ↑/↓ recall of sent prompts, while the composer is empty or still shows an unedited recalled one.
    /// False: the key should move the caret as usual.
    fn recall(&mut self, up: bool, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if self.palette.is_some()
            || self.model_picker.is_some()
            || self.provider_form.is_some()
            || self.oauth_form.is_some()
            || self.key_form.is_some()
            || self.account_form.is_some()
            || self.cite_form.is_some()
            || self.usage.is_some()
            || self.workflows.is_some()
            || self.device_form.is_some()
            || self.terminal_visible
            || self.theme_edit.is_some()
            || self.settings.is_some()
        {
            return false;
        }
        let prompts: Vec<SharedString> = self
            .thread()
            .items
            .iter()
            .filter_map(|i| {
                if let Item::User(u) = i {
                    Some(u.clone())
                } else {
                    None
                }
            })
            .collect();
        let value = self.input.read(cx).value().to_string();
        let current = self
            .recall
            .filter(|i| prompts.get(*i).is_some_and(|p| p.as_ref() == value));
        if !(value.is_empty() || current.is_some()) || prompts.is_empty() {
            self.recall = None;
            return false;
        }
        self.recall = match (up, current) {
            (true, None) => Some(prompts.len() - 1),
            (true, Some(i)) => Some(i.saturating_sub(1)),
            (false, Some(i)) if i + 1 < prompts.len() => Some(i + 1),
            (false, Some(_)) => None,
            (false, None) => return false,
        };
        let text = self
            .recall
            .map(|i| prompts[i].to_string())
            .unwrap_or_default();
        self.input
            .update(cx, |s, cx| set_input(s, text, window, cx));
        true
    }

    fn cycle<T: PartialEq + Clone>(list: &[T], cur: &T) -> T {
        let i = list
            .iter()
            .position(|x| x == cur)
            .map_or(0, |i| (i + 1) % list.len());
        list[i].clone()
    }

    fn on_key(&mut self, ev: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let k = &ev.keystroke;
        let (cmd, shift) = (k.modifiers.platform, k.modifiers.shift);
        // User keybindings from ui.json: {"keys": {"cmd-shift-p": "/plan"}}.
        let combo = format!(
            "{}{}{}{}{}",
            if cmd { "cmd-" } else { "" },
            if k.modifiers.control { "ctrl-" } else { "" },
            if k.modifiers.alt { "alt-" } else { "" },
            if shift { "shift-" } else { "" },
            k.key
        );
        if let Some(bound) = self.ui["keys"][combo.as_str()].as_str().map(String::from) {
            if self.command(&bound, true, window, cx) {
                cx.stop_propagation();
            }
            return;
        }
        // A full-screen terminal program gets every key as it is typed, not a line at a time.
        if self.terminal_visible
            && self
                .active_terminal()
                .is_some_and(|t| t.raw() && t.input.read(cx).focus_handle(cx).is_focused(window))
        {
            if let Some(bytes) = terminal::tui_key(
                k,
                self.active_terminal()
                    .unwrap()
                    .screen
                    .screen()
                    .application_cursor(),
            ) {
                self.write_terminal(bytes);
                cx.stop_propagation();
                return;
            }
        }
        if self.terminal_visible
            && k.modifiers.shift
            && matches!(k.key.as_str(), "pageup" | "pagedown" | "home" | "end")
            && (!matches!(k.key.as_str(), "home" | "end") || cmd)
            && self.active_terminal().is_some_and(|t| {
                !t.raw() && t.input.read(cx).focus_handle(cx).is_focused(window)
            })
        {
            let term = self.active_terminal().unwrap();
            match k.key.as_str() {
                "pageup" => term.scroll_by_page(false),
                "pagedown" => term.scroll_by_page(true),
                "home" => term.scroll_to_top(),
                _ => term.scroll.scroll_to_bottom(),
            }
            cx.notify();
            cx.stop_propagation();
            return;
        }
        match k.key.as_str() {
            "escape" if self.model_picker.is_some() => self.close_model_picker(window, cx),
            "down" | "up" if self.model_picker.is_some() => {
                self.move_model_picker(k.key == "down", cx)
            }
            "escape" if self.voice_recording.is_some() => self.cancel_voice(cx),
            "escape" if self.palette.is_some() => self.toggle_palette(window, cx),
            "down" | "up" if self.palette.is_some() => {
                let n = self.filtered_acts(cx).len().max(1);
                self.palette_ix = (self.palette_ix + if k.key == "down" { 1 } else { n - 1 }) % n;
                if self.login_palette || self.registry_entries.is_some() || self.picking_commit {
                    self.palette_scroll.scroll_to_item(self.palette_ix);
                }
                cx.notify();
            }
            "escape" if self.usage.is_some() => self.toggle_usage(window, cx),
            "escape" if self.workflows.is_some() => self.toggle_workflows(window, cx),
            key if self.usage.is_some()
                && self.palette.is_none()
                && !(cmd || k.modifiers.control || k.modifiers.alt)
                && self.input.read(cx).value().is_empty()
                && self.usage_key(key, cx) =>
            {
                cx.stop_propagation()
            }
            "escape" if self.find.is_open() => self.close_find(window, cx),
            "escape" if self.pr_panel.is_some() && self.palette.is_none() => {
                self.toggle_prs(window, cx)
            }
            "escape" if self.key_form.is_some() => self.close_key_form(window, cx),
            "escape" if self.account_form.is_some() => self.close_account_form(window, cx),
            "escape" if self.cite_form.is_some() => self.close_cite_form(window, cx),
            "escape" if self.device_form.is_some() => self.close_device_form(window, cx),
            "escape" if self.oauth_form.is_some() => self.close_oauth_form(window, cx),
            "escape" if self.provider_form.is_some() => self.close_provider_form(window, cx),
            "escape" if self.theme_edit.is_some() => self.close_theme_editor(window, cx),
            "escape" if self.settings.is_some() => self.run(Act::Settings, window, cx),
            "," if cmd && k.modifiers.alt => self.run(Act::EffortStep(false), window, cx),
            "." if cmd && k.modifiers.alt => self.run(Act::EffortStep(true), window, cx),
            "," if cmd => self.run(Act::Settings, window, cx),
            "q" if cmd => self.request_quit(ev.is_held, cx),
            "f6" => self.cycle_focus(shift, window, cx),
            // ctrl-c, ctrl-d, ctrl-z and the like go to the terminal's foreground program as control bytes.
            // Only while the terminal's input has focus: the composer keeps its own ctrl-a, ctrl-e and so on.
            key if k.modifiers.control
                && !cmd
                && self.terminal_visible
                && self.active_terminal().is_some_and(|t| t.stdin.is_some())
                && key.len() == 1
                && key.as_bytes()[0].is_ascii_lowercase()
                && self
                    .active_terminal()
                    .is_some_and(|t| t.input.read(cx).focus_handle(cx).is_focused(window)) =>
            {
                self.write_terminal(vec![key.as_bytes()[0] & 0x1f]);
            }
            "up" | "down" if cmd && k.modifiers.alt => {
                self.run(Act::Move(k.key == "up"), window, cx)
            }
            "escape"
                if self.thread().suggestion.is_some()
                    && self.thread().pending.is_none()
                    && self.thread().dialog.is_none() =>
            {
                self.dismiss_suggestion(cx)
            }
            "escape" if self.thread().dialog.is_some() => {
                if self.thread().dialog.as_ref().is_some_and(|(request, _)| request["method"] == "secret") {
                    self.secret_refuse(window, cx)
                } else {
                    self.answer_dialog(json!({"cancelled":true}), cx)
                }
            }
            "escape" if self.thread().pending.is_some() => self.answer(Decision::Deny, cx),
            "escape" if self.thread().status == Status::Running => self.stop(),
            "enter" if cmd && self.thread().pending.is_some() => self.answer(
                if shift {
                    Decision::Always
                } else {
                    Decision::Allow
                },
                cx,
            ),
            "enter" if cmd && shift => self.run(Act::Steer, window, cx),
            // Start this thread in the background and move on to the next draft.
            "enter"
                if cmd
                    && self.palette.is_none()
                    && self.thread().items.is_empty()
                    && !self.input.read(cx).value().trim().is_empty() =>
            {
                let project = self.thread().project();
                self.send(window, cx);
                self.new_thread(project, false, window, cx);
            }
            "up" if k.modifiers.alt
                && self.palette.is_none()
                && self.input.read(cx).value().is_empty() =>
            {
                if let Some(i) = self.thread().queue.len().checked_sub(1) {
                    self.edit_queue(i, window, cx);
                }
            }
            "a" if cmd && shift && !k.modifiers.alt => {
                let mode = self.thread().perms.mode();
                self.run(
                    Act::Mode(Self::cycle(
                        greencode_core::config::backend_modes(&self.thread().meta.backend),
                        &mode,
                    )),
                    window,
                    cx,
                )
            }
            "m" if cmd && shift && self.palette.is_none() => self.open_model_picker(window, cx),
            "t" if cmd
                && self.terminal_visible
                && self
                    .active_terminal()
                    .is_some_and(|t| t.input.read(cx).focus_handle(cx).is_focused(window)) =>
            {
                self.new_terminal(window, cx)
            }
            "w" if cmd
                && self.terminal_visible
                && self
                    .active_terminal()
                    .is_some_and(|t| t.input.read(cx).focus_handle(cx).is_focused(window)) =>
            {
                self.stop_terminal();
                cx.notify();
            }
            "j" if cmd => self.run(Act::Terminal, window, cx),
            "s" if cmd && self.palette.is_none() => self.run(Act::Stash, window, cx),
            "tab" if !shift && self.tab(window, cx) => cx.stop_propagation(),
            "n" if cmd && k.modifiers.alt => self.run(Act::NewScratch, window, cx),
            "n" if cmd => self.run(
                if shift {
                    Act::NewWorktree
                } else {
                    Act::NewThread
                },
                window,
                cx,
            ),
            "o" if cmd => self.run(Act::OpenProject, window, cx),
            "d" if cmd && shift => self.toggle_voice(window, cx),
            "d" if cmd => self.run(Act::ToggleDiff, window, cx),
            "e" if cmd && shift => self.run(Act::ToggleFiles, window, cx),
            "c" if cmd && shift && self.palette.is_none() => self.run(Act::Cite, window, cx),
            "a" if cmd && k.modifiers.alt && shift => {
                self.run(Act::CycleThemeMode, window, cx)
            }
            "a" if cmd && k.modifiers.alt => self.open_palette("テーマ: ", window, cx),
            "k" if cmd => self.toggle_palette(window, cx),
            "z" if cmd && !shift => {
                if self.undo_sidebar() {
                    cx.stop_propagation();
                }
            }
            "z" if cmd && shift && self.palette.is_none() => self.run(Act::Undo, window, cx),
            "f" if cmd && self.palette.is_none() && self.settings.is_none() => {
                self.open_find(window, cx)
            }
            "[" if cmd && self.palette.is_none() && self.settings.is_none() => {
                self.navigate(true, window, cx)
            }
            "]" if cmd && self.palette.is_none() && self.settings.is_none() => {
                self.navigate(false, window, cx)
            }
            // Text pastes fall through to the input.
            "v" if cmd
                && self.model_picker.is_none()
                && self.palette.is_none()
                && self.provider_form.is_none()
                && self.oauth_form.is_none()
                && self.key_form.is_none()
                && self.account_form.is_none()
                && self.cite_form.is_none()
                && self.usage.is_none()
                && self.device_form.is_none()
                && self.theme_edit.is_none() =>
            {
                if !self.paste_attachment(window, cx) {
                    return;
                }
            }
            // The picker's rows carry these shortcuts; otherwise they switch threads.
            d if cmd && d.len() == 1 && ("1"..="9").contains(&d) && self.model_picker.is_some() => {
                self.pick_model_shortcut(d.parse::<usize>().unwrap() - 1, window, cx)
            }
            d if cmd && d.len() == 1 && ("1"..="9").contains(&d) => {
                self.select(d.parse::<usize>().unwrap() - 1, window, cx)
            }
            _ => return,
        }
        cx.stop_propagation();
    }

    // ---- rendering ----

    /// One sidebar row. `offset` (the local UTC offset) switches to the dated look: a status dot and the day of month.
    fn thread_row(&self, i: usize, offset: Option<i64>, pal: Pal, cx: &mut Context<Self>) -> Stateful<Div> {
        let t = &self.threads[i];
        let id = t.meta.id.clone();
        let title: SharedString = if t.meta.title.is_empty() {
            "新しいスレッド".into()
        } else {
            t.meta.title.clone().into()
        };
        let dot = match t.status {
            Status::Running => Some(pal.accent),
            Status::Waiting => Some(pal.warn),
            Status::Idle if t.bg_wait.is_some() => Some(pal.accent),
            Status::Idle => None,
        };
        div()
            .id(("thread", i))
            .group("thread-row")
            .flex()
            .items_center()
            .gap_2()
            .px_2()
            .py_1()
            .rounded_md()
            .cursor_pointer()
            .when(i == self.active, |d| d.bg(pal.selected))
            .when(i != self.active, |d| d.hover(|s| s.bg(pal.hover)))
            .on_click(
                cx.listener(move |this, _: &ClickEvent, window, cx| this.select(i, window, cx)),
            )
            .child(match offset {
                Some(_) => div().size_1p5().flex_shrink_0().rounded_full().bg(dot.unwrap_or(pal.muted.opacity(0.5))),
                None => avatar(&project_name(&t.project()), pal),
            })
            .child(div().flex_1().truncate().text_sm().child(title))
            .children(
                t.linked_pr_manual
                    .as_ref()
                    .or(t.linked_pr.as_ref())
                    .map(|pr| {
                        div()
                            .text_xs()
                            .text_color(if pr.state == "MERGED" {
                                pal.accent
                            } else {
                                pal.muted
                            })
                            .child(format!(
                                "#{}{}",
                                pr.number,
                                if pr.state == "CLOSED" { " ✕" } else { "" }
                            ))
                    }),
            )
            .when(t.meta.repo.is_some(), |d| {
                d.child(div().text_xs().text_color(pal.muted).child("⑂"))
            })
            .when(t.pinned, |d| {
                d.child(div().text_xs().text_color(pal.muted).child("•"))
            })
            .child(
                gpui_kit::base::Button::new(("thread-delete", i))
                    .accessibility_label("スレッドを削除")
                    .tab_stop(false)
                    .flex_shrink_0()
                    .size(px(18.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded_md()
                    .text_xs()
                    .text_color(pal.muted)
                    .cursor_pointer()
                    .invisible()
                    .group_hover("thread-row", |s| s.visible())
                    .hover(|s| s.bg(pal.hover).text_color(pal.fg))
                    .child("×")
                    .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                        cx.stop_propagation();
                        this.run(Act::DeleteThread(id.clone()), window, cx);
                    })),
            )
            .child(match (offset, dot) {
                (Some(o), _) => div().text_xs().text_color(pal.muted).child(SharedString::from(format!(
                    "{}日",
                    greencode_core::usage_history::civil(local_day(t.meta.created, o)).2
                ))),
                (None, Some(c)) => div().size_2().rounded_full().bg(c),
                (None, None) => div()
                    .text_xs()
                    .text_color(pal.muted)
                    .child(SharedString::from(age(t.meta.created))),
            })
    }

    fn sidebar(&self, pal: Pal, cx: &mut Context<Self>) -> impl IntoElement {
        // Threads grouped by project, in order of first appearance; archived ones are listed apart.
        let mut groups: Vec<(PathBuf, Vec<usize>)> = Vec::new();
        let mut settled: Vec<usize> = Vec::new();
        for (i, t) in self.threads.iter().enumerate() {
            // An empty composer is not history; its row appears once a message is sent.
            if t.unstarted() {
                continue;
            }
            // Snoozed threads stay hidden until their time, unless they need attention.
            if t.snoozed > now() && i != self.active && t.status != Status::Waiting {
                continue;
            }
            if t.archived && i != self.active {
                settled.push(i);
                continue;
            }
            let p = t.project();
            // An archived project drops out of the sidebar; the thread being viewed stays reachable.
            if self.project_archived(&p) && i != self.active {
                continue;
            }
            match groups.iter_mut().find(|(g, _)| *g == p) {
                Some((_, v)) => v.push(i),
                None => groups.push((p, vec![i])),
            }
        }
        let pinned = self.pinned_projects();
        groups.sort_by_key(|(p, _)| pinned.iter().position(|q| q == p).unwrap_or(usize::MAX));
        let mut body = div()
            .id("threads")
            .flex_1()
            .overflow_y_scroll()
            .px_2()
            .pb_2();
        // Hermes-style by default: one list by start date under month headings, pinned threads first.
        let by_date = self.ui["sidebar_group"].as_str() != Some("project");
        let offset = by_date.then(utc_offset);
        let mut sections: Vec<(AnyElement, Vec<usize>)> = Vec::new();
        if let Some(o) = offset {
            let today = local_day(now(), o);
            let mut ixs: Vec<usize> = groups.iter().flat_map(|(_, v)| v.iter().copied()).collect();
            ixs.sort_by_key(|&i| (!self.threads[i].pinned, std::cmp::Reverse(self.threads[i].meta.created)));
            let mut last = None;
            for i in ixs {
                let t = &self.threads[i];
                let heading =
                    if t.pinned { "ピン留め".to_string() } else { month_heading(local_day(t.meta.created, o), today) };
                if last.as_ref() != Some(&heading) {
                    let el = div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .px_2()
                        .pt_3()
                        .pb_1()
                        .text_xs()
                        .text_color(pal.muted)
                        .child(SharedString::from(heading.clone()))
                        .child(div().flex_1().h(px(1.)).bg(pal.line));
                    sections.push((el.into_any_element(), Vec::new()));
                    last = Some(heading);
                }
                sections.last_mut().unwrap().1.push(i);
            }
        }
        for (project, ixs) in groups.into_iter().filter(|_| !by_date) {
            let mut ixs = ixs;
            ixs.sort_by_key(|i| !self.threads[*i].pinned);
            let (pinned_now, archived_now) =
                (self.project_pinned(&project), self.project_archived(&project));
            let pin_act = Act::PinProject(project.clone());
            let pin_label = if pinned_now { "📌" } else { "ピン" };
            let archive_act = Act::ArchiveProject(project.clone());
            let archive_label = if archived_now { "復元" } else { "保管" };
            let pin_id = format!("project-pin-{}", project.display());
            let archive_id = format!("project-archive-{}", project.display());
            let header = div()
                .flex()
                .items_center()
                .gap_1()
                .px_2()
                .pt_3()
                .pb_1()
                .text_xs()
                .font_weight(FontWeight::MEDIUM)
                .text_color(pal.muted)
                .child(div().flex_1().truncate().child(SharedString::from(format!(
                    "{}{}",
                    project_name(&project),
                    if archived_now { "（保管済み）" } else { "" }
                ))))
                .child(chip(pin_id, pin_label, pal).on_click(
                    cx.listener(move |this, _: &ClickEvent, window, cx| {
                        this.run(pin_act.clone(), window, cx)
                    }),
                ))
                .child(chip(archive_id, archive_label, pal).on_click(
                    cx.listener(move |this, _: &ClickEvent, window, cx| {
                        this.run(archive_act.clone(), window, cx)
                    }),
                ));
            sections.push((header.into_any_element(), ixs));
        }
        for (header, ixs) in sections {
            body = body.child(header);
            // Delegated child threads sit indented under their parent when both are listed here.
            let is_child = |i: &usize| {
                let parent = self.threads[*i].meta.parent.as_deref();
                parent.is_some_and(|p| ixs.iter().any(|j| self.threads[*j].meta.id == p))
            };
            for &i in ixs.iter().filter(|i| !is_child(i)) {
                body = body.child(self.thread_row(i, offset, pal, cx));
                let id = self.threads[i].meta.id.as_str();
                for &c in ixs.iter().filter(|c| self.threads[**c].meta.parent.as_deref() == Some(id)) {
                    body = body.child(self.thread_row(c, offset, pal, cx).ml_4());
                }
            }
        }
        if !settled.is_empty() {
            let open = self.show_archived;
            body = body.child(
                div()
                    .id("settled")
                    .flex()
                    .items_center()
                    .gap_1()
                    .px_2()
                    .mt_3()
                    .py_1()
                    .rounded_md()
                    .text_xs()
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(pal.muted)
                    .cursor_pointer()
                    .hover(|s| s.bg(pal.hover))
                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                        this.show_archived = !this.show_archived;
                        cx.notify();
                    }))
                    .child(
                        div()
                            .flex_1()
                            .child(SharedString::from(format!("保管済み {}", settled.len()))),
                    )
                    .child(if open { "▾" } else { "▸" }),
            );
            if open {
                for i in settled {
                    body = body.child(self.thread_row(i, offset, pal, cx));
                }
            }
        }
        div()
            .w(px(224.))
            .h_full()
            .flex()
            .flex_col()
            .flex_shrink_0()
            .bg(pal.sidebar)
            .border_r_1()
            .border_color(pal.line)
            // Room for the traffic lights; the strip also drags the window.
            .child(
                div()
                    .h(px(44.))
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .pl(px(78.))
                    .text_sm()
                    .font_weight(FontWeight::MEDIUM)
                    .on_mouse_down(MouseButton::Left, |_, window, _| window.start_window_move())
                    .child("GreenCode"),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_1()
                    .px_2()
                    .pb_1()
                    .child(
                        div()
                            .id("search")
                            .flex_1()
                            .flex()
                            .items_center()
                            .gap_2()
                            .px_2()
                            .h(px(26.))
                            .rounded_md()
                            .bg(pal.hover)
                            .text_xs()
                            .text_color(pal.muted)
                            .cursor_pointer()
                            .hover(|s| s.bg(pal.selected))
                            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                this.open_palette("?", window, cx)
                            }))
                            .child(Ic::Search)
                            .child("検索"),
                    )
                    .child(
                        control_button("open", "フォルダを開く", pal).on_click(cx.listener(
                            |this, _: &ClickEvent, window, cx| {
                                this.run(Act::OpenProject, window, cx)
                            },
                        )),
                    )
                    .child(control_button("new", "＋ 新規", pal)
                        .accessibility_label("新しいスレッド")
                        .on_click(cx.listener(
                            |this, _: &ClickEvent, window, cx| this.run(Act::NewThread, window, cx),
                        ))),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .px_4()
                    .pt_2()
                    .text_xs()
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(pal.muted)
                    .child(div().flex_1().child("スレッド"))
                    .child(chip("sidebar-group", if by_date { "日付順" } else { "プロジェクト別" }, pal).on_click(
                        cx.listener(move |this, _: &ClickEvent, _, cx| {
                            this.ui["sidebar_group"] = json!(if by_date { "project" } else { "date" });
                            this.save_ui();
                            cx.notify();
                        }),
                    )),
            )
            .child(body)
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_1()
                    .p_2()
                    .border_t_1()
                    .border_color(pal.line)
                    .child(
                        control_button("settings", "設定", pal).on_click(cx.listener(
                            |this, _: &ClickEvent, window, cx| this.run(Act::Settings, window, cx),
                        )),
                    )
                    .child(icon_button("prs", "PR", pal).on_click(cx.listener(
                        |this, _: &ClickEvent, window, cx| {
                            this.run(Act::PullRequests, window, cx);
                        },
                    )))
                    .child(
                        icon_button("usage", Ic::ChartPie, pal).on_click(cx.listener(
                            |this, _: &ClickEvent, window, cx| {
                                this.run(Act::Usage, window, cx);
                            },
                        )),
                    ),
            )
            .track_focus(&self.sidebar_focus)
    }

    /// Resets the conversation list after work rows change height, keeping the row at the top in place.
    fn reset_work_rows(&mut self) {
        let anchor = self.list.logical_scroll_top();
        self.list.reset(self.thread().items.len());
        self.list.scroll_to(anchor);
    }

    /// Collapses every expanded work group in the active thread.
    fn collapse_work(&mut self, cx: &mut Context<Self>) {
        let id = self.thread().meta.id.clone();
        self.expanded_groups.retain(|(thread, _)| *thread != id);
        self.reset_work_rows();
        cx.notify();
    }

    fn item(&mut self, ix: usize, cx: &mut Context<Self>) -> AnyElement {
        let pal = self.pal(cx);
        let mono = cx.theme().mono_font_family.clone();
        let t = &self.threads[self.active];
        let Some(item) = t.items.get(ix) else {
            return div().into_any_element();
        };
        // T3's Tool calls in chat: show, hide successful, hide all, or Zen (also hide interim replies).
        let visibility = self.tool_visibility();
        if let Item::Tool {
            output, is_error, ..
        } = item
        {
            let hidden = match visibility {
                ToolVisibility::Show => false,
                ToolVisibility::Successful => output.is_some() && !*is_error,
                ToolVisibility::All | ToolVisibility::Zen => true,
            };
            if hidden {
                return div().into_any_element();
            }
        }
        if visibility == ToolVisibility::Zen
            && matches!(item, Item::Assistant(_))
            && ix + 1 < self.threads[self.active].items.len()
        {
            return div().into_any_element();
        }
        let toggle = cx.listener(move |this, _: &ClickEvent, _, cx| {
            if let Some(Item::Tool { open, .. }) = this.threads[this.active].items.get_mut(ix) {
                *open = !*open;
                this.list.splice(ix..ix + 1, 1);
                cx.notify();
            }
        });
        let workspace = cx.weak_entity();
        let open_link = move |url: &SharedString, _: &ClickEvent, window: &mut Window, cx: &mut App| {
            if let Ok(handled) =
                workspace.update(cx, |ws, cx| ws.preview_link(url, window, cx))
            {
                if !handled {
                    cx.open_url(url);
                }
            }
        };
        let column = || div().w_full().max_w(px(760.)).px_6().py_1p5();
        let centred = |d: Div| div().w_full().flex().justify_center().child(d);
        // Finished work (tools, thinking) collapses into one row; a click expands it.
        // The grouping only depends on item kinds, so one computation serves every visible row.
        let running = t.status == Status::Running;
        // `classic_activity`: the earlier running view, where the current tools stay listed one by one.
        let live = self.ui["classic_activity"] != true;
        let groups: std::rc::Rc<Vec<Option<usize>>> = if visibility == ToolVisibility::Show {
            let mut cache = self.groups_cache.borrow_mut();
            if cache.0 == t.meta.id && cache.1 == t.items.len() && cache.2 == (running, live) {
                cache.3.clone()
            } else {
                let groups = std::rc::Rc::new(thread_view::work_groups(&t.items, running, live));
                *cache = (t.meta.id.clone(), t.items.len(), (running, live), groups.clone());
                groups
            }
        } else {
            Default::default()
        };
        let group = groups.get(ix).copied().flatten();
        let mut header = None;
        // An expanded group can be longer than the window, so the header is out of reach by its end.
        // A footer repeats the collapse control after the last row.
        let mut footer = None;
        if let Some(start) = group {
            let key = (t.meta.id.clone(), start);
            let footer_key = key.clone();
            let expanded = self.expanded_groups.contains(&key);
            if start == ix {
                let n = (start..t.items.len())
                    .take_while(|&i| groups[i] == Some(start))
                    .filter(|&i| matches!(t.items[i], Item::Tool { .. }))
                    .count();
                let errors = (start..t.items.len())
                    .take_while(|&i| groups[i] == Some(start))
                    .filter(|&i| matches!(t.items[i], Item::Tool { is_error: true, .. }))
                    .count();
                let label = if errors == 0 {
                    format!("作業内容 · ツール {n} 件")
                } else {
                    format!("作業内容 · ツール {n} 件 · エラー {errors}")
                };
                let chevron = if expanded { "‹" } else { "›" };
                // The running turn's work is one live row naming the current step, as in T3 Code.
                let row = if live && thread_view::live_start(&t.items, running) == Some(start) {
                    let mut trailing = format!("ツール {n} 件");
                    if let Some(started) = t.started {
                        trailing += &format!(" · {}", thread_view::elapsed(started.elapsed().as_secs()));
                    }
                    thread_view::live_row(
                        ("work", ix),
                        thread_view::live_label(&t.items),
                        format!("{trailing} {chevron}"),
                        pal,
                    )
                } else {
                    div()
                        .id(("work", ix))
                        .px_2()
                        .py_0p5()
                        .rounded_md()
                        .text_sm()
                        .text_color(pal.muted)
                        .child(SharedString::from(format!("{label} {chevron}")))
                };
                header = Some(centred(
                    column().py_0p5().child(
                        row.cursor_pointer()
                            .hover(|s| s.bg(pal.hover))
                            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                                if !this.expanded_groups.remove(&key) {
                                    this.expanded_groups.insert(key.clone());
                                }
                                this.reset_work_rows();
                                cx.notify();
                            })),
                    ),
                ));
            }
            if !expanded {
                return match header {
                    Some(h) => h.into_any_element(),
                    None => div().into_any_element(),
                };
            }
            // The last row of the group carries the way out, next to where the reading ends.
            if groups.get(ix + 1).copied().flatten() != Some(start) {
                let key = footer_key;
                footer = Some(centred(
                    column().py_0p5().child(
                        div()
                            .id(("work-collapse", ix))
                            .flex()
                            .items_center()
                            .justify_center()
                            .gap_2()
                            .px_2()
                            .py_1()
                            .rounded_md()
                            .text_xs()
                            .text_color(pal.muted)
                            .cursor_pointer()
                            .hover(|s| s.bg(pal.hover).text_color(pal.fg))
                            .child("折りたたむ ‹")
                            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                                this.expanded_groups.remove(&key);
                                this.reset_work_rows();
                                cx.notify();
                            })),
                    ),
                ));
            }
        }
        // Prompts of an idle native thread can be taken back for editing, which rewinds the thread to them.
        let edit = (matches!(item, Item::User(_))
            && t.meta.backend == "native"
            && t.status == Status::Idle)
            .then(|| {
                let back = t.items[ix + 1..]
                    .iter()
                    .filter(|i| matches!(i, Item::User(_)))
                    .count();
                div().flex().justify_end().px_6().child(
                    chip(("edit-prompt", ix), "ここから編集し直す", pal).on_click(cx.listener(
                        move |this, _: &ClickEvent, window, cx| {
                            this.run(Act::Rewind(back), window, cx)
                        },
                    )),
                )
            });
        // A finished turn ends with the files it changed.
        let ends_turn = matches!(item, Item::Assistant(_))
            && match t.items.get(ix + 1) {
                Some(next) => matches!(next, Item::User(_)),
                None => t.status != Status::Running,
            };
        let changes = if ends_turn {
            thread_view::turn_changes(&t.items, ix)
        } else {
            vec![]
        };
        let card = (!changes.is_empty()).then(|| {
            let (added, removed) = changes.iter().fold((0, 0), |a, c| (a.0 + c.1, a.1 + c.2));
            let mono = mono.clone();
            let project = t.project();
            let mut card = div()
                .w_full()
                .flex()
                .flex_col()
                .rounded_xl()
                .border_1()
                .border_color(pal.line)
                .bg(pal.card)
                .child(
                    div()
                        .flex()
                        .items_center()
                        .justify_between()
                        .px_3()
                        .py_2()
                        .text_sm()
                        .child(
                            div()
                                .flex()
                                .gap_2()
                                .child(SharedString::from(format!(
                                    "変更されたファイル {}",
                                    changes.len()
                                )))
                                .child(
                                    div()
                                        .text_color(pal.added)
                                        .child(SharedString::from(format!("+{added}"))),
                                )
                                .child(
                                    div()
                                        .text_color(pal.removed)
                                        .child(SharedString::from(format!("−{removed}"))),
                                ),
                        )
                        .child(chip(("changes-diff", ix), "差分を開く", pal).on_click(
                            cx.listener(|this, _: &ClickEvent, window, cx| {
                                if !this.show_diff {
                                    this.run(Act::ToggleDiff, window, cx);
                                }
                            }),
                        )),
                );
            for (path, a, r) in &changes {
                card = card.child(
                    div()
                        .flex()
                        .items_center()
                        .justify_between()
                        .gap_2()
                        .px_3()
                        .py_1()
                        .border_t_1()
                        .border_color(pal.line)
                        .child(
                            div()
                                .font_family(mono.clone())
                                .text_xs()
                                .truncate()
                                .child(SharedString::from(relative_to_project(path, &project))),
                        )
                        .child(
                            div()
                                .flex()
                                .gap_2()
                                .text_xs()
                                .child(
                                    div()
                                        .text_color(pal.added)
                                        .child(SharedString::from(format!("+{a}"))),
                                )
                                .child(
                                    div()
                                        .text_color(pal.removed)
                                        .child(SharedString::from(format!("−{r}"))),
                                ),
                        ),
                );
            }
            centred(column().child(card))
        });
        // T3 offers a retry after a failed turn, and a resume when the provider reported a limit reset.
        let retry =
            (matches!(item, Item::Error(_))
                && ix + 1 == t.items.len()
                && t.status == Status::Idle
                && t.last_prompt.is_some())
            .then(|| {
                div()
                    .flex()
                    .items_center()
                    .justify_end()
                    .gap_2()
                    .px_6()
                    .child(
                        chip(("retry", ix), "再試行", pal).on_click(cx.listener(
                            |this, _: &ClickEvent, window, cx| this.run(Act::Retry, window, cx),
                        )),
                    )
                    .children((t.reset_at > now()).then(|| {
                        chip(("resume-at-reset", ix), "リセット時に再開", pal).on_click(
                            cx.listener(|this, _: &ClickEvent, window, cx| {
                                this.run(Act::ResumeAtReset, window, cx)
                            }),
                        )
                    }))
            });
        let copy = matches!(item, Item::User(_) | Item::Assistant(_)).then(|| {
            div().flex().justify_end().px_6().child(
                chip(("copy-message", ix), "コピー", pal).on_click(cx.listener(
                    move |this, _: &ClickEvent, window, cx| {
                        this.run(Act::CopyMessage(ix), window, cx)
                    },
                )),
            )
        });
        // Before the first tool or reply arrives, the turn still shows it is working.
        let thinking = (running
            && live
            && ix + 1 == t.items.len()
            && !matches!(item, Item::Tool { .. } | Item::Thinking(_) | Item::Assistant(_)))
        .then(|| {
            let trailing = t
                .started
                .map(|s| thread_view::elapsed(s.elapsed().as_secs()))
                .unwrap_or_default();
            centred(column().py_0p5().child(thread_view::live_row(
                ("thinking", ix),
                "考え中".into(),
                trailing,
                pal,
            )))
        });
        div()
            .children(header)
            .child(thread_view::render_item(ix, item, pal, mono, toggle, open_link))
            .children(thinking)
            .children(copy)
            .children(edit)
            .children(card)
            .children(retry)
            .children(footer)
            .into_any_element()
    }

    fn composer(
        &mut self,
        pal: Pal,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        if self.queue_edit.as_ref().is_some_and(|e| e.removed) {
            self.finish_queue_edit(false, window, cx);
        }
        if let Some(text) = self.editor_text.take() {
            cx.defer_in(window, move |this, window, cx| {
                this.input
                    .update(cx, |s, cx| set_input(s, text, window, cx))
            });
        }
        if !self.released.is_empty() {
            let released = std::mem::take(&mut self.released);
            cx.defer_in(window, move |this, window, cx| {
                for (id, text, backend) in released {
                    this.submit_released(&id, text, &backend, window, cx);
                }
            });
        }
        // A secret request is answered in its own masked field, so focus that instead of the composer.
        if self.thread().dialog.as_ref().is_some_and(|(request, _)| request["method"] == "secret") {
            let entry = self.secrets.entry.clone();
            if !entry.read(cx).focus_handle(cx).is_focused(window) {
                cx.defer_in(window, move |this, window, cx| {
                    if this.thread().dialog.as_ref().is_some_and(|(request, _)| request["method"] == "secret") {
                        entry.update(cx, |s, cx| s.focus(window, cx));
                    }
                });
            }
        }
        let has_selection = gpui_kit::base::TextSelection::has_selection(window, cx);
        let suggestions = self.suggestions(cx);
        let t = self.thread();
        let status = t.status;
        let used = t.usage.context();
        let billed = t.spent.context();
        let cost = if t.cost > 0. {
            format!(" · ${:.2}", t.cost)
        } else {
            String::new()
        };
        // ACP reports a context size per turn without a spend counter, so the token count alone is shown.
        let usage = (used > 0).then(|| {
            let cache = (billed > 0)
                .then(|| format!(" · キャッシュ {}%", t.spent.cache_read * 100 / billed))
                .unwrap_or_default();
            format!("{:.1}k トークン{cache}{cost}", used as f64 / 1000.)
        });
        let (model, backend) = (self.model_label(), t.meta.backend.clone());
        let mode_chips = self.mode_chips(pal, cx);
        let backend_name = if backend == "native" {
            "標準".into()
        } else {
            self.connection_display_name(&backend)
        };
        let mode = self.thread().perms.mode();
        let can_send = status == Status::Idle
            || !self.input.read(cx).value().trim().is_empty()
            || t.dialog.is_some()
            || self.queue_edit.is_some();
        let send_label = if self.queue_edit.is_some() {
            "変更を保存"
        } else if t.dialog.is_some() {
            "回答を送信"
        } else if status == Status::Idle {
            "送信"
        } else if status == Status::Running
            && self.ui["follow_up_steer"] == true
            && can_steer(&self.cfg, t)
        {
            "実行中に送信"
        } else {
            "送信を予約"
        };

        let branch = self.branch.clone();
        let mut col = div().flex().flex_col().gap_2().px_6().pb_4().pt_1();
        let extension_ui: Vec<String> = t
            .widgets
            .values()
            .flatten()
            .cloned()
            .chain(
                (!t.statuses.is_empty())
                    .then(|| t.statuses.values().cloned().collect::<Vec<_>>().join(" · ")),
            )
            .collect();
        if !extension_ui.is_empty() {
            col = col.child(
                div()
                    .flex()
                    .flex_col()
                    .text_xs()
                    .text_color(pal.muted)
                    .children(extension_ui.into_iter().map(SharedString::from)),
            );
        }
        if let Some(row) = self.goal_row(pal, cx) {
            col = col.child(row);
        }
        if let Some((request, _)) = &t.dialog {
            let mut dialog = div()
                .flex()
                .flex_col()
                .gap_2()
                .p_3()
                .rounded_lg()
                .bg(pal.card)
                .child(SharedString::from(
                    request["title"]
                        .as_str()
                        .unwrap_or("エージェントからの質問")
                        .to_string(),
                ))
                .child(SharedString::from(
                    request["message"].as_str().unwrap_or("").to_string(),
                ));
            if request["method"] == "ask" {
                let index = request["_gc_answers"].as_array().map_or(0, Vec::len);
                let question = &request["questions"][index];
                dialog = dialog
                    .child(SharedString::from(
                        question["header"].as_str().unwrap_or("").to_string(),
                    ))
                    .child(SharedString::from(
                        question["question"].as_str().unwrap_or("").to_string(),
                    ));
                for (i, option) in question["options"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .enumerate()
                {
                    let Some(label) = option["label"].as_str() else {
                        continue;
                    };
                    let value = label.to_string();
                    let selected = request["_gc_selected"]
                        .as_array()
                        .is_some_and(|s| s.iter().any(|v| v == label));
                    let title = format!(
                        "{}{}{}",
                        if selected { "✓ " } else { "" },
                        label,
                        option["description"]
                            .as_str()
                            .map(|d| format!(" — {d}"))
                            .unwrap_or_default()
                    );
                    dialog =
                        dialog.child(chip(("ask-option", i), title, pal).on_click(cx.listener(
                            move |this, _: &ClickEvent, _, cx| this.select_ask_option(&value, cx),
                        )));
                }
                dialog = dialog
                    .child(chip("ask-next", "次へ / 送信", pal).on_click(
                        cx.listener(|this, _: &ClickEvent, window, cx| this.send(window, cx)),
                    ))
                    .child(
                        div()
                            .text_xs()
                            .text_color(pal.muted)
                            .child("選択肢を選ぶか入力欄で自由記入 · esc でキャンセル"),
                    );
            } else {
                for (i, option) in request["options"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .enumerate()
                {
                    let value = option.to_string();
                    dialog = dialog.child(chip(("dialog-option", i), value.clone(), pal).on_click(
                        cx.listener(move |this, _: &ClickEvent, _, cx| {
                            this.answer_dialog(json!({"value":value}), cx)
                        }),
                    ));
                }
                if request["method"] == "confirm" {
                    dialog = dialog
                        .child(chip("dialog-yes", "はい", pal).on_click(cx.listener(
                            |this, _: &ClickEvent, _, cx| {
                                this.answer_dialog(json!({"confirmed":true}), cx)
                            },
                        )))
                        .child(chip("dialog-no", "いいえ", pal).on_click(cx.listener(
                            |this, _: &ClickEvent, _, cx| {
                                this.answer_dialog(json!({"confirmed":false}), cx)
                            },
                        )));
                } else if request["method"] == "secret" {
                    dialog = dialog
                        .child(Input::new(&self.secrets.entry))
                        .child(chip("secret-save", "保存", pal).on_click(cx.listener(
                            |this, _: &ClickEvent, window, cx| this.secret_save(window, cx),
                        )))
                        .child(chip("secret-refuse", "拒否", pal).on_click(cx.listener(
                            |this, _: &ClickEvent, window, cx| this.secret_refuse(window, cx),
                        )))
                        .child(
                            div()
                                .text_xs()
                                .text_color(pal.muted)
                                .child("この欄の値は会話にも下書きにも残りません · esc で拒否"),
                        );
                } else {
                    dialog = dialog.child(
                        div()
                            .text_xs()
                            .text_color(pal.muted)
                            .child("入力欄で回答して送信 · esc でキャンセル"),
                    );
                }
            }
            col = col.child(dialog);
        }
        for (i, text) in t.queue.iter().enumerate() {
            let editing = self
                .queue_edit
                .as_ref()
                .is_some_and(|e| e.index == i && !e.removed);
            let mut row = div().flex().flex_wrap().gap_2().child(
                chip(("queue-now", i), "今すぐ送信", pal).on_click(cx.listener(
                    move |this, _: &ClickEvent, window, cx| this.send_queue_now(i, window, cx),
                )),
            );
            if i > 0 {
                row = row.child(chip(("queue-up", i), "上へ", pal).on_click(cx.listener(
                    move |this, _: &ClickEvent, window, cx| {
                        this.run(Act::MoveQueue(i, true), window, cx)
                    },
                )));
            }
            if i + 1 < t.queue.len() {
                row = row.child(chip(("queue-down", i), "下へ", pal).on_click(cx.listener(
                    move |this, _: &ClickEvent, window, cx| {
                        this.run(Act::MoveQueue(i, false), window, cx)
                    },
                )));
            }
            if editing {
                row = row
                    .child(chip(("queue-save", i), "保存 ✓", pal).on_click(cx.listener(
                        |this, _: &ClickEvent, window, cx| this.finish_queue_edit(true, window, cx),
                    )))
                    .child(
                        chip(("queue-cancel", i), "キャンセル", pal).on_click(cx.listener(
                            |this, _: &ClickEvent, window, cx| {
                                this.finish_queue_edit(false, window, cx)
                            },
                        )),
                    );
            } else {
                row = row.child(chip(("queue-edit", i), "編集", pal).on_click(cx.listener(
                    move |this, _: &ClickEvent, window, cx| this.run(Act::EditQueue(i), window, cx),
                )));
            }
            row = row
                .child(chip(("queue-remove", i), "削除", pal).on_click(cx.listener(
                    move |this, _: &ClickEvent, window, cx| this.remove_queue(i, window, cx),
                )))
                .child(
                    chip(("queue-draft", i), "下書きへ", pal).on_click(cx.listener(
                        move |this, _: &ClickEvent, window, cx| {
                            this.move_queue_to_composer(i, window, cx)
                        },
                    )),
                );
            col = col.child(
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .p_3()
                    .rounded_lg()
                    .border_1()
                    .border_color(if editing { pal.accent } else { pal.line })
                    .bg(pal.card)
                    .child(div().text_xs().text_color(pal.muted).child("未送信"))
                    .child(SharedString::from(text.clone()))
                    .child(row),
            );
        }
        if !t.subs.is_empty() {
            let mut header = div()
                .flex()
                .items_center()
                .justify_between()
                .child("サブエージェント");
            if t.subs.iter().all(|s| s.state != "running") {
                header = header.child(chip("subs-close", "閉じる", pal).on_click(cx.listener(
                    |this, _: &ClickEvent, _, cx| {
                        this.threads[this.active]
                            .subs
                            .retain(|s| s.state == "running");
                        cx.notify();
                    },
                )));
            }
            let mut card = div()
                .flex()
                .flex_col()
                .gap_2()
                .p_3()
                .rounded_lg()
                .border_1()
                .border_color(pal.line)
                .bg(pal.card)
                .child(header);
            for (i, sub) in t.subs.iter().enumerate() {
                let color = match sub.state.as_str() {
                    "running" => pal.accent,
                    "done" => pal.added,
                    "failed" => pal.removed,
                    _ => pal.muted,
                };
                let mut row = div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .text_sm()
                    .child(div().text_color(color).child("●"))
                    .child(
                        div()
                            .flex()
                            .child(SharedString::from(sub.name.clone()))
                            .child(
                                div()
                                    .text_color(pal.muted)
                                    .child(SharedString::from(format!(" ({})", sub.agent))),
                            ),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_xs()
                            .text_color(pal.muted)
                            .truncate()
                            .child(SharedString::from(sub.detail.clone())),
                    );
                if let Some(task) = sub.task.clone() {
                    row = row.child(chip(("sub-open", i), "開く", pal).on_click(cx.listener(
                        move |this, _: &ClickEvent, window, cx| this.open_child(&task, window, cx),
                    )));
                }
                if sub.state == "running" {
                    let name = sub.name.clone();
                    let cancel_name = name.clone();
                    row = row
                        .child(chip(("sub-steer", i), "指示", pal).on_click(cx.listener(
                            move |this, _: &ClickEvent, window, cx| {
                                this.input.update(cx, |s, cx| {
                                    set_input(s, format!("/to {name} "), window, cx);
                                    s.focus(window, cx);
                                });
                                cx.notify();
                            },
                        )))
                        .child(chip(("sub-cancel", i), "停止", pal).on_click(cx.listener(
                            move |this, _: &ClickEvent, _, cx| this.cancel_sub(&cancel_name, cx),
                        )));
                }
                card = card.child(row);
            }
            col = col.child(card);
        }
        if t.queue_held && !t.queue.is_empty() {
            col = col.child(
                chip("resume-queue", "キューを再開", pal).on_click(cx.listener(
                    |this, _: &ClickEvent, window, cx| this.run(Act::ResumeQueue, window, cx),
                )),
            );
        }
        if let Some((tool, summary, _)) = &t.pending {
            let button = |id: &'static str,
                          label: &'static str,
                          d: Decision,
                          primary: bool,
                          cx: &mut Context<Self>| {
                div()
                    .id(id)
                    .px_3()
                    .py_1()
                    .rounded_md()
                    .text_sm()
                    .cursor_pointer()
                    .when(primary, |b| b.bg(pal.accent).text_color(pal.accent_fg))
                    .when(!primary, |b| b.bg(pal.hover).hover(|s| s.bg(pal.selected)))
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| this.answer(d, cx)))
                    .child(label)
            };
            col = col.child(
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .p_3()
                    .rounded_lg()
                    .border_1()
                    .border_color(pal.line)
                    .bg(pal.card)
                    .child(
                        div()
                            .text_sm()
                            .child(SharedString::from(format!("{tool} の実行を許可しますか？"))),
                    )
                    .child(
                        div()
                            .font_family(cx.theme().mono_font_family.clone())
                            .text_xs()
                            .text_color(pal.muted)
                            .max_h(px(120.))
                            .overflow_hidden()
                            .child(summary.clone()),
                    )
                    .child(
                        div()
                            .flex()
                            .gap_2()
                            .child(button("allow", "許可  ⌘⏎", Decision::Allow, true, cx))
                            .child(button(
                                "always",
                                "常に許可  ⇧⌘⏎",
                                Decision::Always,
                                false,
                                cx,
                            ))
                            .child(button("deny", "拒否  esc", Decision::Deny, false, cx)),
                    ),
            );
        }
        if let Some(row) = self.suggestion_row(pal, cx) {
            col = col.child(row);
        }
        if !suggestions.is_empty() {
            let mut row = div().flex().flex_wrap().gap_1();
            for (i, s) in suggestions.into_iter().enumerate() {
                let label = self.completion_label(&s);
                let text = s.clone();
                row = row.child(
                    chip(("suggest", i), label, pal)
                        .when(i == 0, |c| c.bg(pal.hover).text_color(pal.fg))
                        .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                            this.complete(&text, window, cx)
                        })),
                );
            }
            col = col.child(
                row.child(
                    div()
                        .text_xs()
                        .text_color(pal.muted)
                        .py_0p5()
                        .child("tab で補完"),
                ),
            );
        }
        let images = self.thread().images.clone();
        let has_images = !images.is_empty();
        let thumbs = images.into_iter().enumerate().map(|(i, path)| {
            let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            div()
                .relative()
                .size(px(64.))
                .rounded_md()
                .overflow_hidden()
                .border_1()
                .border_color(pal.line)
                .child(img(path).size_full().object_fit(ObjectFit::Cover))
                .child(
                    gpui_kit::base::Button::new(("draft-image-remove", i))
                        .accessibility_label(SharedString::from(format!("{name} を外す")))
                        .absolute()
                        .top_0p5()
                        .right_0p5()
                        .size(px(18.))
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded_full()
                        .bg(pal.card)
                        .text_xs()
                        .text_color(pal.fg)
                        .cursor_pointer()
                        .hover(|s| s.bg(pal.hover))
                        .child("×")
                        .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                            let images = &mut this.threads[this.active].images;
                            if i < images.len() {
                                images.remove(i);
                            }
                            cx.notify();
                        })),
                )
        });
        col.child(
            div()
                .flex()
                .flex_col()
                .rounded_2xl()
                .border_1()
                .border_color(pal.line)
                .bg(pal.card)
                .when(has_images, |d| {
                    d.child(div().flex().flex_wrap().gap_2().px_3().pt_2().children(thumbs))
                })
                .child(div().px_3().pt_2().child(
                    Textarea::new(&self.input).appearance(false).on_paste({
                        let me = cx.entity();
                        move |_, window, cx| {
                            me.update(cx, |this, cx| this.paste_attachment(window, cx))
                        }
                    }),
                ))
                .child(
                    div()
                        .flex()
                        .flex_wrap()
                        .items_center()
                        .gap_1()
                        .px_2()
                        .pb_2()
                        .child(control_button("attach", "添付", pal).on_click(cx.listener(
                            |this, _: &ClickEvent, window, cx| {
                                this.command("/attach", false, window, cx);
                            },
                        )))
                        .child(
                            control_button(
                                "voice",
                                if self.voice_recording.is_some() {
                                    "録音を停止"
                                } else {
                                    "音声入力"
                                },
                                pal,
                            )
                            .on_click(cx.listener(
                                |this, _: &ClickEvent, window, cx| this.toggle_voice(window, cx),
                            )),
                        )
                        .when(has_selection, |d| {
                            d.child(chip("cite", "引用", pal).on_click(cx.listener(
                                |this, _: &ClickEvent, window, cx| this.run(Act::Cite, window, cx),
                            )))
                        })
                        .child(div().flex_1())
                        .when_some(usage, |d, u| {
                            d.child(
                                div()
                                    .text_xs()
                                    .text_color(pal.muted)
                                    .child(SharedString::from(u)),
                            )
                        })
                        .when(status != Status::Idle, |d| {
                            d.child(
                                div().text_xs().text_color(pal.muted).child(
                                    if status == Status::Waiting { "確認待ち" } else { "実行中" },
                                ),
                            )
                        })
                        .child(control_button("model", "", pal)
                            .accessibility_label(format!("モデルを選択: {model}"))
                            .max_w(px(220.))
                            .flex()
                            .items_center()
                            .gap_1p5()
                            .children(models::harness_icon(&backend, 14., pal.fg))
                            .child(div().min_w_0().truncate().child(format!("{model} ▾")))
                            .on_click(cx.listener(
                                |this, _: &ClickEvent, window, cx| this.open_model_picker(window, cx),
                            )))
                        .children(mode_chips)
                        .when(status != Status::Idle, |d| {
                            d.child(control_button("stop", "停止", pal).on_click(cx.listener(
                                |this, _: &ClickEvent, _, _| this.stop(),
                            )))
                        })
                        .child(
                            control_button("send", send_label, pal)
                                .disabled(!can_send)
                                .when(can_send, |b| b.bg(pal.accent).text_color(pal.accent_fg))
                                .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                    this.send(window, cx);
                                })),
                        ),
                ),
        )
        .child(
            div()
                .flex()
                .items_center()
                .gap_2()
                .px_2()
                .text_xs()
                .text_color(pal.muted)
                .child(control_button("backend", format!("接続先: {backend_name} ▾"), pal)
                    .max_w(px(200.))
                    .truncate()
                    .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                        if !this.thread().live() && this.thread().items.is_empty() {
                            this.open_palette("接続先: ", window, cx);
                        } else {
                            this.open_model_picker(window, cx);
                        }
                    })))
                .child(div().flex_1())
                .when(!branch.is_empty(), |d| {
                    d.child(div().truncate().child(format!("⎇ {branch}")))
                })
                .child(control_button("mode", format!("権限: {} ▾", mode_label(mode)), pal)
                    .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                        this.open_palette("権限: ", window, cx);
                    }))),
        )
    }

    fn main_panel(
        &mut self,
        pal: Pal,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let composer = self.composer(pal, window, cx).into_any_element();
        let t = self.thread();
        let title: SharedString = if t.meta.title.is_empty() {
            "新しいスレッド".into()
        } else {
            t.meta.title.clone().into()
        };
        let project = project_name(&t.project());
        let path = SharedString::from(if self.branch.is_empty() {
            String::new()
        } else {
            format!(
                "⎇ {}{}",
                self.branch,
                self.prs
                    .iter()
                    .find(|p| p.2 == self.branch)
                    .map(|p| format!("  PR #{}", p.0))
                    .unwrap_or_default()
            )
        });
        let empty = t.items.is_empty();
        let mono = cx.theme().mono_font_family.clone();
        let terminal = if self.terminal_visible {
            self.active_terminal().map(|term| {
                let cwd = self.thread().meta.cwd.clone();
                let (terms, active) = &self.terminals.projects[&cwd];
                let mut tabs = div().flex().gap_1().items_center();
                for (i, tab) in terms.iter().enumerate() {
                    if tab
                        .private
                        .as_ref()
                        .is_some_and(|id| *id != self.thread().meta.id)
                    {
                        continue;
                    }
                    tabs = tabs.child(
                        chip(
                            ("terminal-tab", i),
                            format!("ターミナル {}", tab.number),
                            pal,
                        )
                        .when(i == *active, |c| c.bg(pal.selected).text_color(pal.fg))
                        .on_click(cx.listener(
                            move |this, _: &ClickEvent, _, cx| {
                                let cwd = this.thread().meta.cwd.clone();
                                if let Some((_, active)) = this.terminals.projects.get_mut(&cwd) {
                                    *active = i;
                                }
                                cx.notify();
                            },
                        )),
                    );
                }
                tabs = tabs
                    .child(chip("terminal-new", "＋", pal).on_click(cx.listener(
                        |this, _: &ClickEvent, window, cx| this.new_terminal(window, cx),
                    )))
                    .child(chip("terminal-close", "×", pal).on_click(cx.listener(
                        |this, _: &ClickEvent, _, cx| {
                            this.stop_terminal();
                            cx.notify();
                        },
                    )));
                let size = term.screen.screen().size();
                let number = term.number;
                let view = cx.entity().downgrade();
                let resize_cwd = cwd.clone();
                let mut lines = div()
                    .id("terminal-lines")
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .track_scroll(&term.scroll)
                    .flex()
                    .flex_col()
                    .font_family(mono)
                    .text_xs()
                    .text_color(pal.fg)
                    .child(
                        canvas(
                            move |bounds, window, cx| {
                                let style = window.text_style();
                                let font_size = style.font_size.to_pixels(window.rem_size());
                                let fonts = cx.text_system();
                                let font = fonts.resolve_font(&style.font());
                                let Ok(width) = fonts.advance(font, font_size, '0') else {
                                    return;
                                };
                                if width.width <= px(0.) || window.line_height() <= px(0.) {
                                    return;
                                }
                                let cols = ((bounds.size.width / width.width) as u16).max(20);
                                let rows =
                                    ((bounds.size.height / window.line_height()) as u16).max(5);
                                if (rows, cols) != (size.0.get(), size.1.get()) {
                                    cx.defer(move |cx| {
                                        let _ = view.update(cx, |this, cx| {
                                            if let Some(term) =
                                                this.terminals.by_number_mut(&resize_cwd, number)
                                            {
                                                term.resize(rows, cols);
                                                cx.notify();
                                            }
                                        });
                                    });
                                }
                            },
                            |_, _, _, _| {},
                        )
                        .absolute()
                        .size_full(),
                    );
                let screen = term.screen.screen();
                if term.raw() {
                    let (cursor_row, cursor_col) = screen.cursor_position();
                    for r in 0..size.0.get() {
                        let cursor = (r == cursor_row).then_some(cursor_col);
                        let mut row = div().flex().whitespace_nowrap();
                        for (text, fg, bg, bold) in
                            terminal::screen_row(screen, r, cursor, pal.fg, pal.panel)
                        {
                            row = row.child(
                                div()
                                    .when_some(fg, |d, c| d.text_color(c))
                                    .when_some(bg, |d, c| d.bg(c))
                                    .when(bold, |d| d.font_weight(FontWeight::BOLD))
                                    .child(text),
                            );
                        }
                        lines = lines.child(row);
                    }
                } else {
                    for line in &term.previous {
                        lines =
                            lines.child(div().text_color(pal.muted).child(terminal::plain(line)));
                    }
                    if !term.previous.is_empty() {
                        lines = lines.child(div().text_color(pal.muted).child("— 前回の出力 —"));
                    }
                    for (i, line) in term.lines.iter().enumerate() {
                        let selected = term
                            .selection
                            .is_some_and(|(a, b)| (a.min(b)..=a.max(b)).contains(&i));
                        let mut row = div()
                            .id(("terminal-line", i))
                            .flex()
                            .flex_col()
                            .cursor_pointer()
                            .when(selected, |d| d.bg(pal.selected));
                        for wrapped in terminal::wrap_runs(
                            terminal::parse_sgr_line(line),
                            size.1.get() as usize,
                        ) {
                            let mut part = div().w_full().flex();
                            for (text, fg, bg, bold) in wrapped {
                                part = part.child(
                                    div()
                                        .when_some(fg, |d, c| d.text_color(c))
                                        .when_some(bg, |d, c| d.bg(c))
                                        .when(bold, |d| d.font_weight(FontWeight::BOLD))
                                        .child(text),
                                );
                            }
                            row = row.child(part);
                        }
                        lines = lines.child(row.on_click(cx.listener(
                            move |this, ev: &ClickEvent, _, cx| {
                                if let Some(term) = this.active_terminal_mut() {
                                    let start = if ev.modifiers().shift {
                                        term.selection.map_or(i, |(a, _)| a)
                                    } else {
                                        i
                                    };
                                    term.selection = Some((start, i));
                                }
                                cx.notify();
                            },
                        )));
                    }
                }
                div()
                    .h(px(260.))
                    .flex_shrink_0()
                    .flex()
                    .flex_col()
                    .px_6()
                    .py_2()
                    .border_t_1()
                    .border_color(pal.line)
                    .child(tabs)
                    .child(
                        div()
                            .flex()
                            .gap_1()
                            .child(
                                chip("terminal-context", "表示中の出力を下書きに追加", pal)
                                    .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                        this.insert_terminal_output(window, cx)
                                    })),
                            )
                            .child(chip("terminal-select-all", "すべて選択", pal).on_click(
                                cx.listener(|this, _: &ClickEvent, _, cx| {
                                    if let Some(term) = this.active_terminal_mut() {
                                        term.selection = term.lines.len().checked_sub(1).map(|n| (0, n));
                                    }
                                    cx.notify();
                                }),
                            ))
                            .child(chip("terminal-copy", "コピー", pal).on_click(cx.listener(
                                |this, _: &ClickEvent, _, cx| {
                                    if let Some(term) = this.active_terminal() {
                                        cx.write_to_clipboard(gpui_kit::ClipboardItem::new_string(
                                            term.copy_text(),
                                        ));
                                    }
                                },
                            )))
                            .child(chip("terminal-latest", "最新へ", pal).on_click(
                                cx.listener(|this, _: &ClickEvent, _, cx| {
                                    if let Some(term) = this.active_terminal() {
                                        term.scroll.scroll_to_bottom();
                                    }
                                    cx.notify();
                                }),
                            ))
                            .child(
                                chip("terminal-selection", "選択を下書きに追加", pal).on_click(
                                    cx.listener(|this, _: &ClickEvent, window, cx| {
                                        this.insert_terminal_selection(window, cx)
                                    }),
                                ),
                            ),
                    )
                    .child(lines)
                    .child(div().pt_1().child(Input::new(&term.input)))
                    .into_any_element()
            })
        } else {
            None
        };
        let (body, composer) = if empty {
            let hero = div()
                .flex_1()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap_3()
                .child(div().text_xl().font_weight(FontWeight::SEMIBOLD).child("新しいスレッド"))
                .child(div().text_sm().text_color(pal.muted).child("ファイルは @、コマンドは / で探せます"))
                .child(div().w_full().max_w(px(640.)).child(composer))
                .into_any_element();
            (hero, None)
        } else {
            let list = list(
                self.list.clone(),
                cx.processor(|this: &mut Self, ix: usize, _, cx| this.item(ix, cx)),
            )
            .flex_1()
            .into_any_element();
            // An expanded group can be far longer than the window; keep a way to close it on screen.
            let id = &self.thread().meta.id;
            let expanded = self.tool_visibility() == ToolVisibility::Show
                && self.expanded_groups.iter().any(|(thread, _)| thread == id);
            let collapse = expanded.then(|| {
                div()
                    .absolute()
                    .right_4()
                    .bottom_2()
                    .child(
                        gpui_kit::base::Button::new("collapse-work")
                            .accessibility_label("作業内容を折りたたむ")
                            .px_3()
                            .py_1()
                            .rounded_full()
                            .border_1()
                            .border_color(pal.line)
                            .bg(pal.card)
                            .text_xs()
                            .text_color(pal.muted)
                            .cursor_pointer()
                            .hover(|s| s.bg(pal.hover).text_color(pal.fg))
                            .child("折りたたむ ‹")
                            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                this.collapse_work(cx)
                            })),
                    )
            });
            let body = div()
                .relative()
                .flex_1()
                .min_h_0()
                .flex()
                .flex_col()
                .child(list)
                .children(collapse)
                .into_any_element();
            (
                body,
                Some(
                    div()
                        .w_full()
                        .flex()
                        .justify_center()
                        .child(div().w_full().max_w(px(760.)).child(composer)),
                ),
            )
        };
        div()
            .flex_1()
            .min_w_0()
            .h_full()
            .flex()
            .flex_col()
            .bg(pal.panel)
            .track_focus(&self.conversation_focus)
            .child(
                div()
                    .h(px(44.))
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .gap_3()
                    .px_6()
                    .when(!empty, |d| d.border_b_1().border_color(pal.line))
                    .on_mouse_down(MouseButton::Left, |_, window, _| window.start_window_move())
                    .child(avatar(&project, pal))
                    .child(
                        div()
                            .text_sm()
                            .text_color(pal.muted)
                            .child(SharedString::from(project.clone())),
                    )
                    .child(div().text_sm().text_color(pal.muted).child("/"))
                    .child(
                        div()
                            .text_sm()
                            .font_weight(FontWeight::MEDIUM)
                            .truncate()
                            .child(title),
                    )
                    .child(div().flex_1())
                    .when(!path.is_empty(), |d| {
                        d.child(
                            div()
                                .text_xs()
                                .text_color(pal.muted)
                                .truncate()
                                .child(path.clone()),
                        )
                    })
                    .child(
                        gpui_kit::base::Button::new("show-updates")
                            .accessibility_label("アップデート")
                            .aria_expanded(self.show_update_panel)
                            .flex_shrink_0()
                            .px_2()
                            .py_1()
                            .rounded_md()
                            .border_1()
                            .border_color(if self.show_update_panel { pal.accent } else { pal.line })
                            .hover(|s| s.bg(pal.hover))
                            .focus(|s| s.border_color(pal.fg))
                            .child("アップデート")
                            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                this.show_update_panel = !this.show_update_panel;
                                cx.notify();
                            })),
                    )
                    .child(icon_button("palette", "⌘K", pal).on_click(cx.listener(
                        |this, _: &ClickEvent, window, cx| this.toggle_palette(window, cx),
                    )))
                    .child(
                        icon_button("git", "⎇", pal)
                            .when(self.show_git, |c| c.bg(pal.selected).text_color(pal.fg))
                            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                this.run(Act::ToggleGit, window, cx)
                            })),
                    )
                    .child(
                        icon_button("files", Ic::File, pal)
                            .when(self.files_panel.is_some(), |c| {
                                c.bg(pal.selected).text_color(pal.fg)
                            })
                            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                this.run(Act::ToggleFiles, window, cx)
                            })),
                    )
                    .child(
                        icon_button("diff", Ic::PanelRight, pal)
                            .when(self.show_diff, |c| c.bg(pal.selected).text_color(pal.fg))
                            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                this.run(Act::ToggleDiff, window, cx)
                            })),
                    )
                    .child(
                        icon_button("terminal", Ic::SquareTerminal, pal).on_click(cx.listener(
                            |this, _: &ClickEvent, window, cx| this.run(Act::Terminal, window, cx),
                        )),
                    ),
            )
            .when(self.show_update_panel, |d| {
                d.child(div().px_6().py_3().border_b_1().border_color(pal.line).bg(pal.card)
                    .child(self.update_controls(pal, cx)))
            })
            .children(self.find_bar(find::Target::Thread, pal, cx))
            .child(body)
            .children(terminal)
            .children(composer)
    }

    fn git_panel(&self, pal: Pal, cx: &mut Context<Self>) -> impl IntoElement {
        let mut panel = div()
            .w(px(300.))
            .h_full()
            .flex_shrink_0()
            .flex()
            .flex_col()
            .p_3()
            .gap_2()
            .border_l_1()
            .border_color(pal.line)
            .bg(pal.panel)
            .child(
                div()
                    .flex()
                    .gap_2()
                    .child("Git の変更")
                    .child(chip("git-refresh", "更新", pal).on_click(cx.listener(
                        |this, _: &ClickEvent, _, cx| {
                            this.refresh_git(cx);
                            cx.notify();
                        },
                    )))
                    .child(chip("git-review", "レビュー", pal).on_click(cx.listener(
                        |this, _: &ClickEvent, window, cx| this.run(Act::GitReview, window, cx),
                    ))),
            );
        for (i, entry) in self.git_status.iter().enumerate() {
            let mut row = div()
                .flex()
                .items_center()
                .gap_2()
                .child(div().flex_1().truncate().child(entry.path.clone()));
            if entry.worktree != ' ' || entry.index == '?' {
                let path = entry.path.clone();
                row = row.child(chip(format!("stage-{i}"), "+", pal).on_click(cx.listener(
                    move |this, _: &ClickEvent, window, cx| {
                        this.run(Act::Stage(path.clone()), window, cx)
                    },
                )));
            }
            if entry.index != ' ' && entry.index != '?' {
                let mut paths = vec![entry.path.clone()];
                if entry.index == 'R' {
                    if let Some(original) = &entry.original {
                        paths.push(original.clone());
                    }
                }
                row = row.child(chip(format!("unstage-{i}"), "−", pal).on_click(cx.listener(
                    move |this, _: &ClickEvent, window, cx| {
                        this.run(Act::Unstage(paths.clone()), window, cx)
                    },
                )));
            }
            panel = panel.child(row);
        }
        panel
    }

    fn diff_panel(&self, pal: Pal, cx: &mut Context<Self>) -> impl IntoElement {
        let mono = cx.theme().mono_font_family.clone();
        let body = if self.diff.is_empty() {
            div()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .text_sm()
                .text_color(pal.muted)
                .child("変更はありません")
                .into_any_element()
        } else {
            uniform_list(
                "diff",
                self.diff.len(),
                cx.processor(move |this: &mut Self, range: Range<usize>, _, _| {
                    range
                        .map(|i| {
                            let l = &this.diff[i];
                            let color = if l.starts_with("+++")
                                || l.starts_with("---")
                                || l.starts_with("diff ")
                            {
                                pal.fg
                            } else if l.starts_with("@@") {
                                pal.accent
                            } else if l.starts_with('+') {
                                pal.added
                            } else if l.starts_with('-') {
                                pal.removed
                            } else {
                                pal.muted
                            };
                            div()
                                .px_3()
                                .whitespace_nowrap()
                                .text_color(color)
                                .child(l.clone())
                        })
                        .collect::<Vec<_>>()
                }),
            )
            .flex_1()
            .font_family(mono)
            .text_xs()
            .track_scroll(&self.find.diff_scroll)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, window, cx| window.focus(&this.diff_focus, cx)),
            )
            .into_any_element()
        };
        div()
            .w(px(440.))
            .h_full()
            .flex()
            .flex_col()
            .flex_shrink_0()
            .overflow_hidden()
            .bg(pal.panel)
            .border_l_1()
            .border_color(pal.line)
            .track_focus(&self.diff_focus)
            .child(
                div()
                    .h(px(44.))
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .px_3()
                    .border_b_1()
                    .border_color(pal.line)
                    .font_weight(FontWeight::MEDIUM)
                    .child("差分"),
            )
            .child(self.diff_source_row(pal, cx))
            .children(self.repo_picker(pal, cx))
            .children(self.find_bar(find::Target::Diff, pal, cx))
            .child(body)
    }

    fn palette_view(&self, pal: Pal, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let (state, _) = self.palette.as_ref()?;
        let mut rows = div().id("palette-rows").flex().flex_col().p_1().when(
            self.login_palette || self.registry_entries.is_some() || self.picking_commit,
            |d| {
                d.max_h(px(420.))
                    .overflow_y_scroll()
                    .track_scroll(&self.palette_scroll)
            },
        );
        for (i, (label, act)) in self.filtered_acts(cx).into_iter().enumerate() {
            rows = rows.child(
                div()
                    .id(("act", i))
                    .px_3()
                    .py_1()
                    .rounded_md()
                    .cursor_pointer()
                    .when(i == self.palette_ix, |d| d.bg(pal.selected))
                    .hover(|s| s.bg(pal.selected))
                    .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                        this.run(act.clone(), window, cx)
                    }))
                    .child(SharedString::from(label)),
            );
        }
        Some(
            div()
                .absolute()
                .inset_0()
                .flex()
                .justify_center()
                .pt(px(110.))
                .child(
                    div()
                        .w(px(540.))
                        .h_auto()
                        .flex()
                        .flex_col()
                        .self_start()
                        .rounded_xl()
                        .border_1()
                        .border_color(pal.line)
                        .bg(pal.card)
                        .shadow_lg()
                        .when(self.login_palette, |d| {
                            d.child(div().px_3().pt_3().child("プロバイダを選択してログイン"))
                        })
                        .when(self.registry_entries.is_some(), |d| {
                            d.child(div().px_3().pt_3().child("ACP レジストリ"))
                        })
                        .when_some(self.text_prompt, |d, prompt| {
                            d.child(div().px_3().pt_3().child(prompt.hint()))
                        })
                        .when(self.picking_commit, |d| {
                            d.child(div().px_3().pt_3().child("差分に表示するコミット（新しい100件）"))
                        })
                        .child(
                            div()
                                .p_2()
                                .border_b_1()
                                .border_color(pal.line)
                                .child(Input::new(state).appearance(false)),
                        )
                        .child(rows),
                ),
        )
    }

    fn update_controls(&self, pal: Pal, cx: &mut Context<Self>) -> Div {
        use gpui_kit::base::Button;

        if !cfg!(target_os = "macos") {
            return div().text_color(pal.muted).child("アプリ内からの更新は macOS のみ対応しています。");
        }

        let locked = self.update_status.locked();
        let channel = self.update_channel;
        let button = |id: &'static str, label: &'static str, highlight| {
            Button::new(id)
                .accessibility_label(label)
                .disabled(locked)
                .px_3()
                .py_1()
                .rounded_md()
                .border_1()
                .border_color(if highlight { pal.accent } else { pal.line })
                .bg(if highlight { pal.selected } else { pal.panel })
                .text_color(if locked { pal.muted } else { pal.fg })
                .hover(|s| s.bg(pal.hover))
                .focus(|s| s.border_color(pal.fg))
                .child(label)
        };
        let message = self.update_status.message();
        div()
            .flex()
            .flex_col()
            .gap_3()
            .child(div().flex().flex_wrap().items_center().gap_2().child("更新チャネル").children(
                [
                    (Channel::Stable, "update-stable", "安定版 (stable)"),
                    (Channel::Main, "update-main", "コミット版 (main)"),
                ].map(|(choice, id, label)| {
                    let selected = choice == channel;
                    button(id, label, selected)
                        .aria_toggled(if selected { accesskit::Toggled::True } else { accesskit::Toggled::False })
                        .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                            if !this.update_status.locked() && this.update_channel != choice {
                                this.update_channel = choice;
                                this.update_status = UpdateStatus::Unchecked;
                                cx.notify();
                            }
                        }))
                }),
            ))
            .child(div().flex().flex_wrap().gap_2().children(
                [("update-check", "更新を確認", true), ("update-install", "今すぐ更新", false)]
                    .map(|(id, label, check)| button(id, label, !check)
                        .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                            this.update_app(check, channel, cx);
                        }))),
            ))
            .child(div().id("update-status").role(accesskit::Role::Status).aria_label(message.clone())
                .text_sm().text_color(if matches!(self.update_status, UpdateStatus::Failed(_)) { pal.warn } else { pal.muted })
                .child(message))
    }

    /// The settings screen: every row changes its setting on click and saves at once.
    fn settings_view(&self, pal: Pal, window: &mut Window, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let (fields, _) = self.settings.as_ref()?;
        let t = self.thread();
        let row = |label, hint: &str, control: AnyElement| settings_row(label, hint, control, pal);
        let n = std::cell::Cell::new(0usize);
        let next_id = || {
            n.set(n.get() + 1);
            n.get()
        };
        let button = |value: String, act: Act, cx: &mut Context<Self>| {
            control_button(("setting", next_id()), value, pal)
                .max_w(px(320.))
                .truncate()
                .border_1()
                .border_color(pal.line)
                .bg(pal.panel)
                .text_color(pal.fg)
                .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                    this.run(act.clone(), window, cx)
                }))
                .into_any_element()
        };
        let toggle = |label: &str, hint: &str, on: bool, act: Act, cx: &mut Context<Self>| {
            let click = cx.listener(move |this, _: &ClickEvent, window, cx| this.run(act.clone(), window, cx));
            row(
                label.to_string().into(),
                hint,
                switch(("setting-switch", next_id()), label.to_string(), on, pal)
                    .on_change(move |_, event, window, cx| click(event, window, cx))
                    .into_any_element(),
            )
        };
        let mut choice = |label: &'static str, options: Vec<(&'static str, Vec<Act>)>, current: Option<usize>, cx: &mut Context<Self>| {
            let value = current.and_then(|i| options.get(i)).map_or("カスタム", |(value, _)| *value);
            let id = (ElementId::from("setting-choice"), SharedString::from(label));
            settings_select(id, label, value, options, pal, window, cx).into_any_element()
        };
        // Options of an action that steps through them in this order: an option repeats the step until it is reached.
        let cycle = |labels: &[&'static str], current: usize, act: Act| {
            labels
                .iter()
                .enumerate()
                .map(|(i, label)| (*label, vec![act.clone(); (i + labels.len() - current) % labels.len()]))
                .collect::<Vec<_>>()
        };
        let settings = self.cfg.policy.as_ref().map(|p| &p.settings);
        let policy_hint = |locked: bool, hint: &'static str| if locked { "組織のポリシーで固定" } else { hint };
        let banner = self.cfg.policy.as_ref().map(|p| {
            let mut line = div().flex().gap_2().items_center().child(format!(
                "{} の管理ポリシーが適用されています",
                p.organization
            ));
            if let Some(url) = &p.support_url {
                let url = url.clone();
                line = line.child(
                    chip("policy-support", "サポート", pal)
                        .on_click(move |_, _, _| spawn_external("/usr/bin/open", &[&url])),
                );
            }
            line
        });

        let tab = self.settings_tab.min(SETTINGS_TABS.len() - 1);
        // Full-window page as in T3 Code: a sidebar of sections, then the section's cards.
        let nav = div()
            .w(px(264.))
            .h_full()
            .flex()
            .flex_col()
            .flex_shrink_0()
            .bg(pal.sidebar)
            .border_r_1()
            .border_color(pal.line)
            // Room for the traffic lights; the strip also drags the window.
            .child(
                div()
                    .h(px(44.))
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .pl(px(78.))
                    .font_weight(FontWeight::MEDIUM)
                    .on_mouse_down(MouseButton::Left, |_, window, _| window.start_window_move())
                    .child("設定"),
            )
            .child(div().id("settings-nav").min_h_0().overflow_y_scroll().flex().flex_col().gap_1().px_3().pt_2().children(
                SETTINGS_TABS.iter().enumerate().map(|(i, &(name, icon))| {
                    gpui_kit::base::Button::new(("settings-tab", i))
                        .accessibility_label(name)
                        .aria_toggled(if i == tab { accesskit::Toggled::True } else { accesskit::Toggled::False })
                        .flex()
                        .items_center()
                        .justify_start()
                        .gap_3()
                        .px_3()
                        .py_2p5()
                        .text_base()
                        .rounded_lg()
                        .cursor_pointer()
                        .hover(|s| s.bg(pal.hover))
                        .text_color(if i == tab { pal.fg } else { pal.muted })
                        .when(i == tab, |d| d.bg(pal.selected))
                        .child(icon)
                        .child(name)
                        .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                            this.run(Act::SettingsTab(name), window, cx)
                        }))
                }),
            ))
            .child(div().flex_1())
            .child(
                gpui_kit::base::Button::new("settings-back")
                    .accessibility_label("設定を閉じる")
                    .m_3()
                    .flex()
                    .items_center()
                    .gap_3()
                    .px_3()
                    .py_2p5()
                    .text_base()
                    .rounded_lg()
                    .text_color(pal.muted)
                    .cursor_pointer()
                    .hover(|s| s.bg(pal.hover).text_color(pal.fg))
                    .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                        this.run(Act::Settings, window, cx)
                    }))
                    .child(Ic::ArrowLeft)
                    .child("設定を閉じる")
                    .child(div().flex_1())
                    .child(div().text_xs().child("esc")),
            );
        let title = div()
            .pb_8()
            .text_2xl()
            .font_weight(FontWeight::SEMIBOLD)
            .child(SETTINGS_TABS[tab].0);
        let name = SETTINGS_TABS[tab].0;
        let mut col = SettingsPage { pal, groups: Vec::new() };
        let macos = cfg!(target_os = "macos");
        let text = |key: &str, empty: &str| {
            self.ui[key].as_str().filter(|s| !s.is_empty()).unwrap_or(empty).to_string()
        };
        match name {
            "一般" => {
                let quit = match self.ui["quit_mode"].as_str() {
                    Some("direct") => 1,
                    Some("hold") => 2,
                    _ => 0,
                };
                col = col
                    .section("通知と終了")
                    .child(toggle("デスクトップ通知", "バックグラウンドで作業が完了したときや確認が必要なとき", self.ui["notify_off"] != true, Act::Toggle("notify_off"), cx))
                    .child(row(
                        "アプリの終了".into(),
                        "⌘Q を押したとき",
                        choice("アプリの終了", cycle(&["2回押す", "すぐに終了", "長押し"], quit, Act::CycleQuitMode), Some(quit), cx),
                    ))
                    .section("画面の撮影")
                    .child(toggle(
                        "ウィンドウを撮影して添付",
                        if macos { "" } else { "macOS のみ" },
                        macos && self.ui["snapshot"] == true,
                        Act::SnapshotToggle,
                        cx,
                    ))
                    .child(toggle(
                        "ウィンドウ内のテキストも添付",
                        "",
                        self.ui["snapshot_text"] == true,
                        Act::SnapshotText,
                        cx,
                    ))
                    .child(row(
                        "ショートカット".into(),
                        "",
                        button(snapshot::shortcut_label(&self.ui).into(), Act::SnapshotShortcut, cx),
                    ))
                    .section("スレッドの自動保管")
                    .child(toggle(
                        "7 日間使っていない",
                        "",
                        self.ui["auto_settle_off"] != true,
                        Act::Toggle("auto_settle_off"),
                        cx,
                    ))
                    .child(toggle(
                        "PR がマージされた",
                        "",
                        self.ui["settle_merged_off"] != true,
                        Act::Toggle("settle_merged_off"),
                        cx,
                    ))
                    .child(toggle(
                        "PR がクローズされた",
                        "",
                        self.ui["settle_closed"] == true,
                        Act::Toggle("settle_closed"),
                        cx,
                    ))
                    .child(toggle(
                        "PR があるスレッドは除く",
                        "",
                        self.ui["settle_keep_pr"] == true,
                        Act::Toggle("settle_keep_pr"),
                        cx,
                    ))
                    .section("その他")
                    .child(row(
                        "リモートアクセス".into(),
                        "greencode-cli serve",
                        chip("remote-copy", "コピー", pal)
                            .border_1()
                            .border_color(pal.line)
                            .bg(pal.panel)
                            .text_color(pal.fg)
                            .on_click(|_, _, cx| {
                                cx.write_to_clipboard(gpui_kit::ClipboardItem::new_string("greencode-cli serve".into()))
                            })
                            .into_any_element(),
                    ))
                    .child(row(
                        "ようこそページ".into(),
                        "",
                        button("開く".into(), Act::Welcome(welcome::WelcomeAct::Open), cx),
                    ))
            }
            "入力と会話" => {
                let tools = match self.tool_visibility() {
                    ToolVisibility::Show => 0,
                    ToolVisibility::Successful => 1,
                    ToolVisibility::All => 2,
                    ToolVisibility::Zen => 3,
                };
                let steer = self.ui["follow_up_steer"] == true;
                let classic = self.ui["classic_activity"] == true;
                col = col.section("進行中の作業").child(row(
                    "表示方法".into(),
                    "",
                    choice(
                        "進行中の作業の表示方法",
                        cycle(&["1 行にまとめる", "ツールを並べる"], classic as usize, Act::Toggle("classic_activity")),
                        Some(classic as usize),
                        cx,
                    ),
                ));
                let cmd_enter = self.ui["send_shortcut"] == "cmd+enter";
                let suggest_locked = settings.and_then(|s| s.suggest_next.as_ref()).is_some_and(|s| s.locked);
                col = col
                    .section("作業の表示")
                    .child(row(
                        "作業の履歴".into(),
                        "ファイルの読み書きやコマンドの実行結果",
                        choice("作業の履歴", cycle(&["すべて", "失敗のみ", "非表示", "回答だけ"], tools, Act::ToolVisibility), Some(tools), cx),
                    ))
                    .section("入力")
                    .child(row(
                        "送信キー".into(),
                        "",
                        choice("送信キー", cycle(&["Enter", "⌘Enter"], cmd_enter as usize, Act::SendShortcut), Some(cmd_enter as usize), cx),
                    ))
                    .child(row(
                        "作業中に送信したとき".into(),
                        "実行中に送信できない接続先では、次の送信として予約します。",
                        choice(
                            "作業中に送信したとき",
                            cycle(&["送信を予約", "実行中に送信"], steer as usize, Act::Toggle("follow_up_steer")),
                            Some(steer as usize),
                            cx,
                        ),
                    ))
                    .child(toggle(
                        "/ の候補にスキルを表示",
                        "",
                        self.ui["slash_skills_off"] != true,
                        Act::Toggle("slash_skills_off"),
                        cx,
                    ))
                    .child(toggle(
                        "返信のあとに次の入力を提案",
                        policy_hint(suggest_locked, "Tab キーで候補を下書きに入れる"),
                        self.ui["suggest_next"] == true,
                        Act::Toggle("suggest_next"),
                        cx,
                    ))
                    .section("名前と文章の自動作成")
                    .child(row(
                        "使用するモデル".into(),
                        "スレッド名、コミット文、PR の説明、ブランチ名に使用",
                        control_button(
                            "text-model-setting",
                            format!("{} ▾", self.text_model().as_deref().unwrap_or("オフ")),
                            pal,
                        )
                        .max_w(px(320.))
                        .truncate()
                        .border_1()
                        .border_color(pal.line)
                        .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                            this.open_palette("自動作成に使うモデル: ", window, cx);
                        }))
                        .into_any_element(),
                    ))
                    .child(row(
                        "スレッド名を付けるときの指示".into(),
                        "",
                        button(text("title_instructions", "なし"), Act::EditText(git::TextPrompt::TitleInstructions), cx),
                    ))
            }
            VOICE_SETTINGS => {
                let voice_locked = settings.and_then(|s| s.voice_provider.as_ref()).is_some_and(|s| s.locked);
                col = col.section("文字起こし")
                    .child(row(
                        "文字起こし先".into(),
                        policy_hint(voice_locked, "音声を処理するサービスを選びます。"),
                        button(
                            voice::provider_label(self.ui["voice_provider"].as_str()).into(),
                            Act::CycleVoiceProvider,
                            cx,
                        ),
                    ))
                    .child(row(
                        "言語".into(),
                        "",
                        button(voice::language_label(&self.ui), Act::VoiceAction("language"), cx),
                    ))
                    .child(toggle(
                        "英語に翻訳",
                        "",
                        self.ui["voice_translate"] == true,
                        Act::Toggle("voice_translate"),
                        cx,
                    ))
                    .child(toggle(
                        "「えー」「um」を除く",
                        "",
                        self.ui["voice_fillers"] == true,
                        Act::Toggle("voice_fillers"),
                        cx,
                    ))
                    .child(row(
                        "マイク".into(),
                        if macos { "" } else { "macOS のみ" },
                        div()
                            .flex()
                            .gap_1()
                            .child(button(voice::mic_label(&self.ui), Act::VoiceAction("mic"), cx))
                            .child(button("テスト".into(), Act::VoiceAction("test"), cx))
                            .into_any_element(),
                    ))
                    .child(row(
                        "辞書".into(),
                        "/voice dict で編集",
                        div().text_sm().child(voice::dict_label(&self.ui)).into_any_element(),
                    ))
                    .section("文字起こしの仕上げ")
                    .child(toggle(
                        "読みやすく整える",
                        "文字起こしの結果を選んだモデルに送ります。",
                        self.ui["voice_cleanup"] == true,
                        Act::Toggle("voice_cleanup"),
                        cx,
                    ))
                    .child(row(
                        "整形モデル".into(),
                        "",
                        button(
                            text("voice_cleanup_model", "スレッドのモデル"),
                            Act::EditText(git::TextPrompt::VoiceCleanupModel),
                            cx,
                        ),
                    ))
                    .child(row(
                        "整形の追加指示".into(),
                        "",
                        button(
                            text("voice_cleanup_instructions", "なし"),
                            Act::EditText(git::TextPrompt::VoiceInstructions),
                            cx,
                        ),
                    ))
                    .child(row(
                        "訂正の合図".into(),
                        "",
                        button(text("voice_correction", "なし"), Act::EditText(git::TextPrompt::VoiceCorrection), cx),
                    ))
            }
            "モデルと権限" => {
                let efforts = [None, Some("low"), Some("medium"), Some("high")];
                let effort = efforts.iter().position(|e| self.cfg.effort.as_deref() == *e);
                let modes = [Mode::Ask, Mode::AutoEdit, Mode::Full, Mode::Plan];
                let mode_choices = |act: fn(Mode) -> Act| {
                    modes
                        .iter()
                        .map(|m| (mode_label(*m), vec![act(*m)]))
                        .collect::<Vec<_>>()
                };
                let mode_locked = settings.and_then(|s| s.default_mode.as_ref()).is_some_and(|s| s.locked);
                let sandbox_locked = settings.and_then(|s| s.sandbox.as_ref()).is_some_and(|s| s.locked);
                col = col
                    .section("モデル")
                    .child(row(
                        "新しいスレッドの既定モデル".into(),
                        &self.cfg.model,
                        button("現在のモデルを使う".into(), Act::DefaultModel, cx),
                    ))
                    .child(row(
                        "回答前の検討量".into(),
                        "対応するモデルで使用",
                        choice(
                            "回答前の検討量",
                            efforts
                                .iter()
                                .zip(["既定", "低", "中", "高"])
                                .map(|(e, label)| (label, vec![Act::Effort(e.map(String::from))]))
                                .collect(),
                            effort,
                            cx,
                        ),
                    ))
                    .section("権限")
                    .child(row(
                        "新しいスレッド".into(),
                        policy_hint(mode_locked, ""),
                        choice("新しいスレッドの権限", mode_choices(Act::DefaultMode), modes.iter().position(|m| *m == self.cfg.mode), cx),
                    ))
                    .child(row(
                        "現在のスレッド".into(),
                        "",
                        choice("現在のスレッドの権限", mode_choices(Act::Mode), modes.iter().position(|m| *m == t.perms.mode()), cx),
                    ))
                    .child(toggle(
                        "シェルの書き込み先を制限",
                        policy_hint(sandbox_locked, "プロジェクトと一時フォルダなどに限定（macOS）"),
                        greencode_core::tools::SANDBOX.load(std::sync::atomic::Ordering::Relaxed),
                        Act::Sandbox,
                        cx,
                    ))
                    .section("常に許可")
                    .children(
                        t.perms
                            .allowed()
                            .is_empty()
                            .then(|| div().text_xs().text_color(pal.muted).child("なし")),
                    )
                    .children(t.perms.allowed().into_iter().map(|key| {
                        row(key.clone().into(), "", button("削除".into(), Act::Revoke(key), cx))
                    }))
            }
            "ツール" => {
                let compaction = if !self.cfg.compaction_enabled {
                    2
                } else if self.cfg.compaction == "snap" {
                    1
                } else {
                    0
                };
                col = col
                    .section("長い会話の処理")
                    .child(row(
                        "会話履歴の整理".into(),
                        "履歴が長くなったとき、続きを進めるために内容を短くまとめます。",
                        choice("会話履歴の整理", cycle(&["要約", "引き継ぎメモ", "オフ"], compaction, Act::CycleCompaction), Some(compaction), cx),
                    ))
                    .child(toggle(
                        "作業で得た知識を保存",
                        "今後のスレッドで再利用します。",
                        self.cfg.learning.auto,
                        Act::SetLearning(!self.cfg.learning.auto),
                        cx,
                    ))
                    .section("拡張ツール")
                    .children(
                        greencode_core::tools::ext::CATALOG
                            .iter()
                            .map(|(name, about)| {
                                let on = !self.cfg.ext_off.iter().any(|o| o == name);
                                toggle(name, ext_hint(name).unwrap_or(about), on, Act::ToggleExt(name), cx)
                            })
                            .collect::<Vec<_>>(),
                    )
            }
            "プロジェクト" => {
                let project = t.project();
                let default = &self.ui["projects"][project.to_string_lossy().as_ref()];
                let saved = match (default["model"].as_str(), default["backend"].as_str()) {
                    (Some(model), Some(backend)) => format!("{backend} · {model}"),
                    _ => "未設定".into(),
                };
                col = col
                    .section("このプロジェクトの既定")
                    .child(row(
                        project.display().to_string().into(),
                        &saved,
                        div()
                            .flex()
                            .gap_1()
                            .child(button("今の設定を保存".into(), Act::SaveProjectDefault, cx))
                            .child(button("解除".into(), Act::ClearProjectDefault, cx))
                            .into_any_element(),
                    ))
                    .section("ワークツリー")
                    .child(
                        div()
                            .text_xs()
                            .text_color(pal.muted)
                            .child(format!("保存先: {}", self.worktree_base().display())),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(div().flex_1().child(Input::new(&self.storage.field)))
                            .child(chip("storage-save", "保存", pal).on_click(cx.listener(
                                |this, _: &ClickEvent, _, cx| this.save_worktree_dir(cx),
                            ))),
                    )
                    .children(
                        self.storage.error.as_ref().map(|e| {
                            div().text_xs().text_color(gpui_kit::red()).child(e.clone())
                        }),
                    )
                    .child(toggle(
                        "自動クリーンアップ",
                        "",
                        self.ui["auto_cleanup"] == true,
                        Act::Toggle("auto_cleanup"),
                        cx,
                    ))
                    .child(
                        chip(
                            "storage-scan",
                            if self.storage.busy {
                                "確認中…"
                            } else {
                                "今すぐクリーンアップ"
                            },
                            pal,
                        )
                        .on_click(cx.listener(
                            |this, _: &ClickEvent, _, cx| this.cleanup(false, false, cx),
                        )),
                    );
                let removable: Vec<_> = self
                    .storage
                    .candidates
                    .iter()
                    .filter(|(_, c)| c.reason == "変更なし・保管済み")
                    .collect();
                let total: u64 = self
                    .storage
                    .candidates
                    .iter()
                    .map(|(_, c)| c.size_bytes)
                    .sum();
                for (_, c) in &self.storage.candidates {
                    col = col.child(div().text_xs().text_color(pal.muted).child(format!(
                        "{} · {} · {} bytes",
                        c.path.display(),
                        c.reason,
                        c.size_bytes
                    )));
                }
                if !self.storage.candidates.is_empty() {
                    col = col.child(div().text_xs().child(format!("合計 {total} bytes")));
                }
                if !removable.is_empty() {
                    let bytes: u64 = removable.iter().map(|(_, c)| c.size_bytes).sum();
                    col =
                        col.child(
                            chip(
                                "storage-remove",
                                format!("{} 件を削除（{} bytes）", removable.len(), bytes),
                                pal,
                            )
                            .on_click(cx.listener(
                                |this, _: &ClickEvent, _, cx| this.cleanup(true, false, cx),
                            )),
                        );
                }
                let prefix = self.ui["worktree_prefix"].as_str().unwrap_or("greencode/").to_string();
                let submodules = match self.ui["worktree_submodules"].as_str() {
                    Some("top") => 1,
                    Some("skip") => 2,
                    _ => 0,
                };
                col = col
                    .section("ソース管理")
                    .child(row(
                        "クローン先".into(),
                        "",
                        button(self.clone_root().display().to_string(), Act::SetCloneRoot, cx),
                    ))
                    .child(toggle(
                        "ホストごとのフォルダに分ける",
                        "github.com/owner/repo",
                        self.ui["clone_host_folders"] == true,
                        Act::Toggle("clone_host_folders"),
                        cx,
                    ))
                    .child(toggle(
                        "既定ブランチを自動で同期",
                        "1 時間ごと",
                        self.ui["sync_default_branch"] == true,
                        Act::Toggle("sync_default_branch"),
                        cx,
                    ))
                    .child(row(
                        "既定ブランチを今すぐ同期".into(),
                        "",
                        button("同期".into(), Act::SyncDefaultBranch, cx),
                    ))
                    .child(row(
                        "ブランチ接頭辞".into(),
                        "",
                        button(
                            if prefix.is_empty() { "なし".into() } else { prefix },
                            Act::EditText(git::TextPrompt::WorktreePrefix),
                            cx,
                        ),
                    ))
                    .child(row(
                        "サブモジュール".into(),
                        "",
                        choice(
                            "サブモジュール",
                            cycle(&["再帰的", "トップレベルのみ", "スキップ"], submodules, Act::CycleWorktreeSubmodules),
                            Some(submodules),
                            cx,
                        ),
                    ))
                    .child(toggle(
                        "マージ時にエージェントのクレジットを除く",
                        "",
                        self.ui["pr_strip_credits"] == true,
                        Act::Toggle("pr_strip_credits"),
                        cx,
                    ))
                    .section("プロジェクト");
                for project in self.known_projects() {
                    let (pinned, archived) = (self.project_pinned(&project), self.project_archived(&project));
                    let state = if archived {
                        "保管済み"
                    } else if pinned {
                        "ピン留め中"
                    } else {
                        ""
                    };
                    col = col.child(row(
                        project.display().to_string().into(),
                        state,
                        div()
                            .flex()
                            .gap_1()
                            .child(button(
                                if pinned { "ピン解除".into() } else { "ピン留め".into() },
                                Act::PinProject(project.clone()),
                                cx,
                            ))
                            .child(button(
                                if archived { "復元".into() } else { "保管".into() },
                                Act::ArchiveProject(project),
                                cx,
                            ))
                            .into_any_element(),
                    ));
                }
            }
            "認証情報" => col = self.secrets_section(col, pal, cx),
            "詳細設定" => col = self.advanced_section(col, pal, cx),
            RUNTIME_SETTINGS => col = self.runtime_section(col, pal, window, cx),
            WORKFLOW_SETTINGS => {
                let workflows = greencode_core::workflow::paths().workflows;
                col = col
                    .section("ワークフロー")
                    .child(toggle(
                        "ワークフローを有効にする",
                        "定期実行、Webhook、失敗時の再試行",
                        self.ui["labs_workflows"] == true,
                        Act::Toggle("labs_workflows"),
                        cx,
                    ))
                    .child(row("一覧".into(), "", button("開く".into(), Act::Workflows, cx)))
                    .child(row(
                        "定義フォルダ".into(),
                        &workflows.display().to_string(),
                        button("開く".into(), Act::OpenFile(workflows), cx),
                    ))
            }
            "ショートカットとファイル" => {
                col = col.section("カスタムショートカット");
                for (key, command) in self.ui["keys"].as_object().into_iter().flatten() {
                    col = col.child(row(
                        key.clone().into(),
                        command.as_str().unwrap_or(""),
                        button("削除".into(), Act::Unbind(key.clone()), cx),
                    ));
                }
                col = col
                    .child(
                        div()
                            .flex()
                            .gap_2()
                            .items_center()
                            .pt_1()
                            .child(div().flex_1().child(Input::new(&fields[0])))
                            .child(div().flex_1().child(Input::new(&fields[1])))
                            .child(button("追加".into(), Act::Bind, cx)),
                    )
                    .section("設定ファイル");
                for (file, about) in [("config.toml", ""), ("mcp.json", "MCP サーバー"), ("ui.json", "")] {
                    col = col.child(row(
                        file.into(),
                        about,
                        button("開く".into(), Act::OpenFile(greencode_core::home().join(file)), cx),
                    ));
                }
            }
            // 外観 and 接続 lay out their own content below.
            _ => {}
        }
        let content = if name == "接続" {
            // The connection list and detail scroll on their own, so this page fills the height.
            div()
                .flex_1()
                .min_h_0()
                .flex()
                .flex_col()
                .px_8()
                .pb_6()
                .child(title)
                .children(banner)
                .child(div().flex_1().min_h_0().child(self.connections_view(pal, cx)))
                .into_any_element()
        } else {
            div()
                .id("settings-body")
                .flex_1()
                .min_h_0()
                .overflow_y_scroll()
                .track_scroll(&self.settings_scroll)
                .child(
                    div()
                        .w_full()
                        .max_w(px(840.))
                        .mx_auto()
                        .px_10()
                        .pb_16()
                        .child(title)
                        .children(banner.map(|b| b.pb_4()))
                        .when(name == "外観", |d| d.child(theme_settings(self, pal, window, cx)))
                        .child(col.build()),
                )
                .into_any_element()
        };
        Some(
            div()
                .absolute()
                .inset_0()
                .flex()
                .bg(pal.panel)
                .occlude()
                .child(nav)
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .h_full()
                        .flex()
                        .flex_col()
                        // Drag strip level with the sidebar's title row.
                        .child(
                            div()
                                .h(px(44.))
                                .flex_shrink_0()
                                .on_mouse_down(MouseButton::Left, |_, window, _| {
                                    window.start_window_move()
                                }),
                        )
                        .child(content),
                )
                .focus_trap("settings-focus", &self.settings_focus),
        )
    }

    fn provider_form_view(&self, pal: Pal, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let form = self.provider_form.as_ref()?;
        let mut col = div()
            .w(px(540.))
            .flex()
            .flex_col()
            .gap_2()
            .p_3()
            .self_start()
            .rounded_xl()
            .border_1()
            .border_color(pal.line)
            .bg(pal.card)
            .shadow_lg()
            .child("プロバイダを追加（APIキーは config.toml に平文保存）");
        for (i, state) in form.fields.iter().enumerate() {
            let input = Input::new(state);
            col = col.child(if i == 2 { input.mask_toggle() } else { input });
        }
        let kind = if form.anthropic {
            "API形式: Anthropic 互換"
        } else {
            "API形式: OpenAI 互換"
        };
        col = col.child(
            div()
                .flex()
                .items_center()
                .justify_between()
                .child(chip("provider-kind", kind, pal).on_click(cx.listener(
                    |this, _: &ClickEvent, _, cx| {
                        if let Some(f) = &mut this.provider_form {
                            f.anthropic = !f.anthropic;
                        }
                        cx.notify();
                    },
                )))
                .child(
                    div()
                        .flex()
                        .gap_1()
                        .child(chip("provider-cancel", "キャンセル  esc", pal).on_click(
                            cx.listener(|this, _: &ClickEvent, window, cx| {
                                this.close_provider_form(window, cx)
                            }),
                        ))
                        .child(chip("provider-add", "追加  ⏎", pal).on_click(cx.listener(
                            |this, _: &ClickEvent, window, cx| this.submit_provider(window, cx),
                        ))),
                ),
        );
        if let Some(e) = &form.error {
            col = col.child(div().text_xs().text_color(gpui_kit::red()).child(e.clone()));
        }
        Some(
            div()
                .absolute()
                .inset_0()
                .flex()
                .justify_center()
                .pt(px(110.))
                .child(col),
        )
    }

    fn oauth_form_view(&self, pal: Pal, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let form = self.oauth_form.as_ref()?;
        let col = div()
            .w(px(540.))
            .flex()
            .flex_col()
            .gap_2()
            .p_3()
            .self_start()
            .rounded_xl()
            .border_1()
            .border_color(pal.line)
            .bg(pal.card)
            .shadow_lg()
            .child("Claude サブスクリプションでログイン")
            .child(div().text_xs().text_color(pal.muted).child("ブラウザで承認し、表示されたコードを貼り付けてください。ブラウザが開かない場合は次の URL を開いてください:"))
            .child(div().text_xs().text_color(pal.muted).child(form.url.clone()))
            .child(Input::new(&form.field).mask_toggle())
            .child(div().flex().gap_1()
                .child(chip("oauth-cancel", "キャンセル  esc", pal).on_click(cx.listener(|this, _: &ClickEvent, window, cx| this.close_oauth_form(window, cx))))
                .child(chip("oauth-submit", if form.login.is_some() { "ログイン  ⏎" } else { "確認中…" }, pal).on_click(cx.listener(|this, _: &ClickEvent, window, cx| this.submit_oauth(window, cx)))));
        Some(
            div()
                .absolute()
                .inset_0()
                .flex()
                .justify_center()
                .pt(px(110.))
                .child(col),
        )
    }
}

impl Render for Workspace {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // The kit paints the window surface with this color; clear it so the blur shows through.
        if cx.theme().tokens.background.a != 0. {
            Theme::global_mut(cx).tokens.background = gpui_kit::transparent_black().into();
        }
        let pal = self.pal(cx);
        let background = self.background_layer();
        let sidebar = self.sidebar(pal, cx).into_any_element();
        let main = self.main_panel(pal, window, cx).into_any_element();
        let diff = self
            .show_diff
            .then(|| self.diff_panel(pal, cx).into_any_element());
        let pr_panel = self.pr_panel_view(pal, cx).map(|p| p.into_any_element());
        let git = self
            .show_git
            .then(|| self.git_panel(pal, cx).into_any_element());
        let palette = self.palette_view(pal, cx).map(|p| p.into_any_element());
        let model_picker = self
            .model_picker_view(pal, cx)
            .map(|p| p.into_any_element());
        let settings = self.settings_view(pal, window, cx).map(|p| p.into_any_element());
        let theme_editor = self
            .theme_editor_view(pal, cx)
            .map(|p| p.into_any_element());
        let provider_form = self
            .provider_form_view(pal, cx)
            .map(|p| p.into_any_element());
        let oauth_form = self.oauth_form_view(pal, cx).map(|p| p.into_any_element());
        let key_form = self.key_form_view(pal, cx).map(|p| p.into_any_element());
        let welcome = self.welcome_view(pal, cx).map(|p| p.into_any_element());
        let account_form = self
            .account_form_view(pal, cx)
            .map(|p| p.into_any_element());
        let cite_form = self.cite_form_view(pal, cx).map(|p| p.into_any_element());
        let usage = self.usage_view(pal, cx).map(|p| p.into_any_element());
        let workflows = self.workflows_view(pal, cx).map(|p| p.into_any_element());
        let device_form = self.device_form_view(pal, cx).map(|p| p.into_any_element());
        let unobscured = model_picker.is_none()
            && settings.is_none()
            && palette.is_none()
            && provider_form.is_none()
            && oauth_form.is_none()
            && key_form.is_none()
            && welcome.is_none()
            && account_form.is_none()
            && cite_form.is_none()
            && usage.is_none()
            && workflows.is_none()
            && device_form.is_none()
            && theme_editor.is_none();
        let files_panel = self.files_panel_view(pal, unobscured, cx);
        div()
            .size_full()
            .flex()
            .text_color(pal.fg)
            .text_sm()
            .track_focus(&self.focus)
            .capture_key_down(cx.listener(Self::on_key))
            .on_drop(cx.listener(|this, paths: &ExternalPaths, window, cx| {
                this.attach_paths(paths.paths().to_vec(), window, cx)
            }))
            // The composer binds the arrow keys to caret movement, so recall hooks its actions rather than the keys.
            .capture_action(cx.listener(
                |this, _: &gpui_kit::component::input::MoveUp, window, cx| {
                    if this.recall(true, window, cx) {
                        cx.stop_propagation()
                    }
                },
            ))
            // Tab is bound to indent in every input, so it never reaches `on_key` while one has focus.
            .capture_action(cx.listener(
                |this, _: &gpui_kit::component::input::IndentInline, window, cx| {
                    if this.tab(window, cx) {
                        cx.stop_propagation()
                    }
                },
            ))
            .capture_action(cx.listener(
                |this, _: &gpui_kit::component::input::MoveDown, window, cx| {
                    if this.recall(false, window, cx) {
                        cx.stop_propagation()
                    }
                },
            ))
            // ⌘F: the inputs bind it to their own search; the app's find bar takes it over.
            .capture_action(cx.listener(
                |this, _: &gpui_kit::component::input::Search, window, cx| {
                    let raw_terminal = this.terminal_visible
                        && this.active_terminal().is_some_and(|t| {
                            t.raw() && t.input.read(cx).focus_handle(cx).is_focused(window)
                        });
                    if !raw_terminal && this.palette.is_none() && this.settings.is_none() {
                        this.open_find(window, cx);
                        cx.stop_propagation();
                    }
                },
            ))
            // ⌘[ / ⌘]: inputs bind them to outdent/indent; history navigation takes them over.
            .capture_action(cx.listener(
                |this, _: &gpui_kit::component::input::Outdent, window, cx| {
                    if this.palette.is_none() && this.settings.is_none() {
                        this.navigate(true, window, cx);
                        cx.stop_propagation();
                    }
                },
            ))
            .capture_action(cx.listener(
                |this, _: &gpui_kit::component::input::Indent, window, cx| {
                    if this.palette.is_none() && this.settings.is_none() {
                        this.navigate(false, window, cx);
                        cx.stop_propagation();
                    }
                },
            ))
            // ⌘Z still undoes typing when no sidebar action is waiting.
            .capture_action(cx.listener(
                |this, _: &gpui_kit::component::input::Undo, _window, cx| {
                    if this.undo_sidebar() {
                        cx.stop_propagation();
                    }
                },
            ))
            .children(background)
            .child(sidebar)
            .child(main)
            .children(git)
            .children(diff)
            .children(files_panel)
            .children(pr_panel)
            .children(model_picker)
            .children(settings)
            .children(palette)
            .children(provider_form)
            .children(oauth_form)
            .children(key_form)
            .children(welcome)
            .children(account_form)
            .children(cite_form)
            .children(usage)
            .children(workflows)
            .children(device_form)
            .children(theme_editor)
    }
}

#[cfg(test)]
mod tests {
    // No glob import: with test-support, `gpui_kit::*` brings a `test` macro that shadows `#[test]`.
    use super::{AgentLink, Item, Status, Workspace};
    use gpui_kit::{AppContext, Context, Entity, TestAppContext, VisualTestContext};
    use greencode_core::{
        config::Config, permission::Perms, session::Meta, Command, Decision, Event,
    };
    use serde_json::json;
    use tokio::sync::oneshot;

    #[gpui_kit::test]
    async fn handoff_blocks_rpc_and_keeps_the_draft(cx: &mut TestAppContext) {
        let (ws, cx) = open(cx);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        ws.update(cx, |ws, _| {
            ws.threads[0].cmd = Some(tx);
            ws.handoffs.insert(ws.thread().meta.id.clone());
        });
        cx.simulate_input("keep this");
        cx.simulate_keystrokes("enter");
        cx.update(|window, cx| {
            ws.update(cx, |ws, cx| {
                ws.run(super::Act::Rpc(json!({"type":"get_state"})), window, cx)
            })
        });
        assert!(rx.try_recv().is_err());
        assert_eq!(
            ws.read_with(cx, |ws, cx| ws.input.read(cx).value().to_string()),
            "keep this\n"
        );
    }

    #[gpui_kit::test]
    async fn handoff_is_blocked_in_plan_mode(cx: &mut TestAppContext) {
        let (ws, cx) = open(cx);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        ws.update(cx, |ws, _| {
            ws.threads[0].perms =
                std::sync::Arc::new(Perms::new(greencode_core::permission::Mode::Plan));
            ws.threads[0].cmd = Some(tx);
        });
        cx.update(|window, cx| ws.update(cx, |ws, cx| ws.run(super::Act::Handoff, window, cx)));
        assert!(rx.try_recv().is_err());
        assert!(ws.read_with(cx, |ws, _| ws.handoffs.is_empty()));
    }

    #[gpui_kit::test]
    async fn voice_shortcut_opens_settings_when_off(cx: &mut TestAppContext) {
        let (ws, cx) = open(cx);
        ws.update(cx, |ws, _| ws.ui["voice_provider"] = json!("off"));
        cx.simulate_keystrokes("cmd-shift-d");
        ws.read_with(cx, |ws, _| {
            assert!(ws.settings.is_some());
            assert_eq!(super::SETTINGS_TABS[ws.settings_tab].0, super::VOICE_SETTINGS);
            assert!(ws.voice_recording.is_none());
            assert!(matches!(ws.thread().items.last(), Some(Item::Notice(text)) if text.as_ref() == "音声入力を使うには、文字起こし先を選んでください"));
        });
    }

    #[gpui_kit::test]
    async fn webhook_commands_manage_hooks(cx: &mut TestAppContext) {
        let (ws, cx) = open(cx);
        cx.update(|window, cx| {
            ws.update(cx, |ws, cx| {
                assert!(ws.command("/webhook Review {{body.action}}", false, window, cx));
                let hook = ws.ui["webhooks"][0].clone();
                let id = hook["id"].as_str().unwrap();
                assert_eq!(id.len(), 24);
                assert_eq!(hook["thread"], ws.thread().meta.id);
                assert_eq!(ws.webhook_rows().len(), 2);
                assert!(ws.command(&format!("/webhook-secret {id} secret"), false, window, cx));
                assert_eq!(ws.ui["webhooks"][0]["secret"], "secret");
                assert_eq!(
                    ws.webhooks.lock().unwrap()[0].secret.as_deref(),
                    Some("secret")
                );
                assert!(ws.command("/webhook", false, window, cx));
                assert_eq!(ws.filtered_acts(cx).len(), 2);
                ws.run(super::Act::DeleteWebhook(id.into()), window, cx);
                assert!(ws.ui["webhooks"].as_array().unwrap().is_empty());
            })
        });
    }

    #[gpui_kit::test]
    async fn workflows_page_opens_and_closes_with_escape(cx: &mut TestAppContext) {
        let (ws, cx) = open(cx);
        cx.update(|window, cx| {
            ws.update(cx, |ws, cx| ws.run(super::Act::Workflows, window, cx));
        });
        ws.read_with(cx, |ws, _| assert!(ws.workflows.is_some()));
        cx.simulate_keystrokes("escape");
        ws.read_with(cx, |ws, _| assert!(ws.workflows.is_none()));
    }

    fn tool(name: &str, input: serde_json::Value) -> Item {
        Item::Tool {
            id: "t".into(),
            name: name.into(),
            input,
            output: Some("ok".into()),
            is_error: false,
            open: false,
        }
    }

    #[test]
    fn sessions_are_headed_by_local_month() {
        // 2026-11-26 00:30 JST is still 2026-11-25 in UTC; 2026-10-31 23:00 UTC is November in JST.
        let (jst, now) = (9 * 3600, 1_795_620_600);
        let today = super::local_day(now, jst);
        assert_eq!(greencode_core::usage_history::civil(today), (2026, 11, 26));
        assert_eq!(super::month_heading(super::local_day(1_793_487_600, jst), today), "今月");
        assert_eq!(super::month_heading(super::local_day(1_793_487_600, 0), today), "10月");
        assert_eq!(super::month_heading(super::local_day(1_764_000_000, jst), today), "2025年11月");
    }

    #[test]
    fn finished_tool_runs_collapse_into_work_groups() {
        let user = || Item::User("go".into());
        let items = [
            user(),
            tool("read", json!({})),
            tool("read", json!({})),
            Item::Assistant("done".into()),
        ];
        assert_eq!(
            crate::thread_view::work_groups(&items, false, true),
            [None, Some(1), Some(1), None]
        );
        // A running turn collapses finished work too, and its trailing tools form the live group.
        assert_eq!(
            crate::thread_view::work_groups(&items, true, true),
            [None, Some(1), Some(1), None]
        );
        assert_eq!(
            crate::thread_view::work_groups(&items[..3], true, true),
            [None, Some(1), Some(1)]
        );
        // The classic view keeps the running turn's newest group open.
        assert_eq!(crate::thread_view::work_groups(&items, true, false), [None; 4]);
        assert_eq!(crate::thread_view::work_groups(&items[..3], true, false), [None; 3]);
        assert_eq!(crate::thread_view::live_start(&items[..3], true), Some(1));
        assert_eq!(crate::thread_view::live_start(&items, true), None);
        assert_eq!(crate::thread_view::elapsed(83), "1m 23s");
        // A single tool is not worth collapsing.
        let single = [
            user(),
            tool("read", json!({})),
            Item::Assistant("done".into()),
        ];
        assert_eq!(crate::thread_view::work_groups(&single, false, true), [None; 3]);
        // Interim assistant text between tools joins the group; the final answer does not.
        let interim = [
            user(),
            tool("read", json!({})),
            Item::Assistant("hm".into()),
            tool("read", json!({})),
            Item::Assistant("done".into()),
        ];
        assert_eq!(
            crate::thread_view::work_groups(&interim, false, true),
            [None, Some(1), Some(1), Some(1), None]
        );
    }

    #[gpui_kit::test]
    async fn collapsing_work_clears_only_the_active_thread(cx: &mut TestAppContext) {
        let (ws, cx) = open(cx);
        let id = ws.read_with(cx, |ws, _| ws.thread().meta.id.clone());
        ws.update(cx, |ws, cx| {
            ws.expanded_groups.insert((id.clone(), 0));
            ws.expanded_groups.insert((id.clone(), 4));
            ws.expanded_groups.insert(("other".into(), 2));
            ws.collapse_work(cx);
            assert!(!ws.expanded_groups.iter().any(|(thread, _)| *thread == id));
            assert!(ws.expanded_groups.contains(&("other".to_string(), 2)));
        });
    }

    #[test]
    fn turn_changes_merge_edits_and_count_writes() {
        let items = [
            Item::User("old".into()),
            tool("write", json!({"path": "old.rs", "content": "x\n"})),
            Item::Assistant("a".into()),
            Item::User("go".into()),
            tool(
                "edit",
                json!({"path": "a.rs", "old_string": "a\nb", "new_string": "c"}),
            ),
            tool(
                "edit",
                json!({"file_path": "a.rs", "old_string": "d", "new_string": "e\nf"}),
            ),
            tool("write", json!({"path": "b.rs", "content": "1\n2\n3"})),
            tool("read", json!({"path": "c.rs"})),
            // opencode's spelling; the unchanged first line is not counted.
            tool(
                "edit",
                json!({"path": "d.rs", "oldString": "print(1)", "newString": "print(1)\nprint(2)"}),
            ),
            tool(
                "ext",
                json!({"action": "call", "tool": "hedit", "args": {"path": "e.rs", "tag": "AAAA", "ops": [{"op": "replace", "start": 1, "end": 2, "text": "x"}]}}),
            ),
            Item::Assistant("done".into()),
        ];
        assert_eq!(
            crate::thread_view::turn_changes(&items, 10),
            [
                ("a.rs".to_string(), 3, 3),
                ("b.rs".to_string(), 3, 0),
                ("d.rs".to_string(), 1, 0),
                ("e.rs".to_string(), 1, 2)
            ]
        );
    }

    #[gpui_kit::test]
    async fn agent_install_palette_sends_command_to_terminal(cx: &mut TestAppContext) {
        let (ws, cx) = open(cx);
        cx.simulate_keystrokes("cmd-k");
        cx.simulate_input("エージェントをインストール/更新: codex");
        cx.simulate_keystrokes("enter");
        ws.read_with(cx, |ws, _| {
            assert!(ws.terminal_visible);
            assert!(ws
                .active_terminal()
                .unwrap()
                .lines
                .iter()
                .any(|line| line.as_ref() == "$ npm install -g @openai/codex"));
        });
    }

    #[gpui_kit::test]
    async fn policy_blocks_mode_changes(cx: &mut TestAppContext) {
        use super::{Act, Mode};
        use std::sync::Arc;
        let (ws, cx) = open(cx);
        ws.update(cx, |ws, _| {
            Arc::make_mut(&mut ws.cfg).policy = Some(
                greencode_core::policy::Policy::parse(
                    r#"{"schemaVersion":1,"organization":"Test","limits":{"maxMode":"auto-edit"}}"#,
                )
                .unwrap(),
            );
        });
        ws.update(cx, |ws, _| ws.thread().perms.set_mode(Mode::AutoEdit));
        cx.update(|window, cx| ws.update(cx, |ws, cx| ws.run(Act::Mode(Mode::Full), window, cx)));
        assert_eq!(
            ws.read_with(cx, |ws, _| ws.thread().perms.mode()),
            Mode::AutoEdit
        );
    }

    #[gpui_kit::test]
    async fn terminal_output_is_added_to_draft_without_sending(cx: &mut TestAppContext) {
        let (ws, cx) = open(cx);
        cx.simulate_input("investigate");
        cx.update(|window, cx| {
            ws.update(cx, |ws, cx| {
                ws.open_terminal(window, cx);
                ws.active_terminal_mut()
                    .unwrap()
                    .lines
                    .extend((0..16).map(|n| format!("line {n}").into()));
            })
        });
        cx.update(|window, cx| ws.update(cx, |ws, cx| ws.insert_terminal_output(window, cx)));
        ws.read_with(cx, |ws, cx| {
            let draft = ws.input.read(cx).value().to_string();
            assert_eq!(
                draft,
                format!(
                    "investigate\n\nターミナル出力:\n{}",
                    (2..16)
                        .map(|n| format!("line {n}"))
                        .collect::<Vec<_>>()
                        .join("\n")
                )
            );
            assert!(ws.thread().items.is_empty());
        });
    }

    #[gpui_kit::test]
    async fn terminal_shortcuts_require_terminal_input_focus(cx: &mut TestAppContext) {
        let (ws, cx) = open(cx);
        cx.update(|window, cx| {
            ws.update(cx, |ws, cx| {
                ws.open_terminal(window, cx);
                ws.input.update(cx, |input, cx| input.focus(window, cx));
            })
        });
        cx.simulate_keystrokes("cmd-t");
        cx.simulate_keystrokes("cmd-w");
        ws.read_with(cx, |ws, _| {
            assert_eq!(ws.terminals.projects[&ws.thread().meta.cwd].0.len(), 1)
        });
        cx.update(|window, cx| {
            ws.update(cx, |ws, cx| {
                ws.active_terminal()
                    .unwrap()
                    .input
                    .update(cx, |input, cx| input.focus(window, cx));
            })
        });
        cx.simulate_keystrokes("cmd-t");
        ws.read_with(cx, |ws, _| {
            assert_eq!(ws.terminals.projects[&ws.thread().meta.cwd].0.len(), 2)
        });
        cx.update(|window, cx| {
            ws.update(cx, |ws, cx| {
                ws.active_terminal()
                    .unwrap()
                    .input
                    .update(cx, |input, cx| input.focus(window, cx));
            })
        });
        cx.simulate_keystrokes("cmd-w");
        ws.read_with(cx, |ws, _| {
            assert_eq!(ws.terminals.projects[&ws.thread().meta.cwd].0.len(), 1)
        });
    }

    #[gpui_kit::test]
    async fn the_inputs_indent_action_completes_a_command(cx: &mut TestAppContext) {
        // On a real keyboard Tab arrives as the input's own indent action, not as a key event.
        let (ws, cx) = open(cx);
        cx.simulate_input("/co");
        cx.dispatch_action(gpui_kit::component::input::IndentInline);
        assert_eq!(
            ws.read_with(cx, |ws, cx| ws.input.read(cx).value().to_string()),
            "/compact "
        );
    }

    #[cfg(unix)]
    #[gpui_kit::test]
    async fn tab_in_the_terminal_field_reaches_the_shell(cx: &mut TestAppContext) {
        use std::io::Read;
        use std::os::fd::FromRawFd;
        let (ws, cx) = open(cx);
        let (mut read_end, write_end) = {
            let mut fds = [0; 2];
            assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
            unsafe {
                (
                    std::fs::File::from_raw_fd(fds[0]),
                    std::fs::File::from_raw_fd(fds[1]),
                )
            }
        };
        cx.update(|window, cx| {
            ws.update(cx, |ws, cx| {
                ws.open_terminal(window, cx);
                let term = ws.active_terminal_mut().unwrap();
                term.stdin = Some(std::sync::Arc::new(tokio::sync::Mutex::new(
                    tokio::fs::File::from_std(write_end),
                )));
                term.input.update(cx, |input, cx| {
                    input.focus(window, cx);
                    input.set_value("git sta", window, cx);
                });
            })
        });
        cx.simulate_keystrokes("tab");
        ws.read_with(cx, |ws, cx| {
            assert_eq!(
                ws.active_terminal()
                    .unwrap()
                    .input
                    .read(cx)
                    .value()
                    .to_string(),
                ""
            )
        });
        let mut got = [0u8; 8];
        let n = read_end.read(&mut got).unwrap();
        assert_eq!(&got[..n], b"git sta\t");
    }

    #[gpui_kit::test]
    async fn terminals_are_project_scoped_persisted_and_selectable(cx: &mut TestAppContext) {
        let (ws, cx) = open(cx);
        cx.update(|window, cx| {
            ws.update(cx, |ws, cx| {
                let project = ws.thread().meta.cwd.clone();
                ws.open_terminal(window, cx);
                ws.active_terminal_mut().unwrap().lines =
                    (0..501).map(|n| format!("line {n}").into()).collect();
                ws.new_terminal(window, cx);
                ws.active_terminal_mut()
                    .unwrap()
                    .lines
                    .push("\x1b[31msecond\x1b[0m".into());
                assert_eq!(ws.active_terminal().unwrap().number, 2);
                ws.threads[ws.active].meta.cwd = project.join("other-project");
                ws.new_terminal(window, cx);
                ws.active_terminal_mut().unwrap().lines.push("other".into());
                assert_eq!(ws.active_terminal().unwrap().lines[0].as_ref(), "other");
                ws.threads[ws.active].meta.cwd = project.clone();
                assert_eq!(
                    super::terminal::plain(&ws.active_terminal().unwrap().lines[0]),
                    "second"
                );
                ws.active_terminal_mut().unwrap().selection = Some((0, 0));
                ws.insert_terminal_selection(window, cx);
                assert_eq!(
                    ws.input.read(cx).value(),
                    "ターミナル 2 の 1-1 行目:\n```\nsecond\n```"
                );
                ws.terminals.projects.get_mut(&project).unwrap().1 = 0;
                ws.stop_terminal();
                ws.terminals = Default::default();
                ws.new_terminal(window, cx);
                let previous = &ws.active_terminal().unwrap().previous;
                assert_eq!(previous.len(), 501);
                assert_eq!(previous[0].as_ref(), "line 0");
                assert_eq!(previous[500].as_ref(), "line 500");
                let term = ws.active_terminal_mut().unwrap();
                term.feed("\x1b[3");
                term.feed("1mred\nstill red\x1b[0m");
                assert_eq!(term.lines.len(), 2);
                let runs = super::terminal::parse_sgr_line(&term.lines[1]);
                assert_eq!(runs[0].0, "still red");
                assert!(runs[0].1.is_some());
            })
        });
    }

    #[test]
    fn terminal_reads_merged_stream() {
        use std::io::Write;
        let (reader, mut writer) = std::io::pipe().unwrap();
        writer.write_all(b"output").unwrap();
        drop(writer);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        super::terminal_output(reader, tx);
        assert_eq!(rx.blocking_recv().unwrap(), "output");
        assert!(rx.blocking_recv().is_none());
    }

    #[test]
    fn git_status_handles_renames_and_unusual_names() {
        let entries = super::parse_git_status("R  new name\0old name\0?? line\nbreak\0 M file\0");
        assert_eq!(
            entries.iter().map(|e| e.path.as_str()).collect::<Vec<_>>(),
            ["new name", "line\nbreak", "file"]
        );
        assert_eq!(entries[0].index, 'R');
        assert_eq!(entries[0].original.as_deref(), Some("old name"));
    }

    /// GREENCODE_HOME is process-wide, so tests that share the process must not overlap.
    static HOME: std::sync::Mutex<()> = std::sync::Mutex::new(());
    thread_local! {
        /// Held until the test's thread exits.
        static HOME_GUARD: std::cell::RefCell<Option<std::sync::MutexGuard<'static, ()>>> = const { std::cell::RefCell::new(None) };
    }

    pub(super) fn open_fresh(cx: &mut TestAppContext) -> (Entity<Workspace>, &mut VisualTestContext) {
        HOME_GUARD.with(|g| {
            if g.borrow().is_none() {
                *g.borrow_mut() = Some(HOME.lock().unwrap_or_else(|e| e.into_inner()));
            }
        });
        static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let home = std::env::temp_dir().join(format!(
            "greencode-ui-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::env::set_var("GREENCODE_HOME", &home);
        std::env::set_var("GEMINI_HOME", home.join("gemini"));
        std::env::set_var("GREENCODE_POLICY", home.join("policy.json"));
        cx.update(gpui_kit::init);
        // Loaded from the empty test home: defaults plus the built-in providers, as at a first launch.
        let (ws, cx) = cx.add_window_view(|window, cx| {
            Workspace::new(Config::load().unwrap(), None, window, cx)
        });
        ws.update(cx, |ws, _| ws.agent_spawner = answering_agent);
        cx.run_until_parked();
        (ws, cx)
    }

    /// `open_fresh` without the first-run welcome page covering the window.
    pub(super) fn open(cx: &mut TestAppContext) -> (Entity<Workspace>, &mut VisualTestContext) {
        let (ws, cx) = open_fresh(cx);
        ws.update(cx, |ws, _| ws.welcome = None);
        (ws, cx)
    }

    /// A discovery command (`get_commands`, `get_state`) that an agent answers by ending it.
    fn is_discovery(command: &Command) -> bool {
        matches!(command, Command::Remote(v) if matches!(v["type"].as_str(), Some("get_commands" | "get_state")))
    }

    /// A test agent on the test's own thread, since the test scheduler rejects events woken from tokio's workers. It is a
    /// backend with no commands to report: it ends each discovery command and holds every prompt.
    fn answering_agent(
        cx: &mut Context<Workspace>,
        _: &tokio::runtime::Handle,
        _: std::sync::Arc<Config>,
        _: Meta,
        _: std::sync::Arc<Perms>,
    ) -> AgentLink {
        let (tx, mut commands) = tokio::sync::mpsc::unbounded_channel();
        let (events, rx) = tokio::sync::mpsc::unbounded_channel();
        cx.spawn(async move |_, _| {
            while let Some(command) = commands.recv().await {
                if is_discovery(&command) {
                    let _ = events.send(Event::Done { waiting_for_delegates: false });
                }
            }
        })
        .detach();
        (tx, rx)
    }

    /// Like `answering_agent`, but a backend whose discovery never finishes.
    fn silent_agent(
        cx: &mut Context<Workspace>,
        _: &tokio::runtime::Handle,
        _: std::sync::Arc<Config>,
        _: Meta,
        _: std::sync::Arc<Perms>,
    ) -> AgentLink {
        let (tx, mut commands) = tokio::sync::mpsc::unbounded_channel();
        let (events, rx) = tokio::sync::mpsc::unbounded_channel();
        cx.spawn(async move |_, _| {
            let _events = events;
            while commands.recv().await.is_some() {}
        })
        .detach();
        (tx, rx)
    }

    fn ask(ws: &Entity<Workspace>, cx: &mut VisualTestContext) -> oneshot::Receiver<Decision> {
        let (reply, rx) = oneshot::channel();
        ws.update(cx, |ws, cx| {
            let id = ws.thread().meta.id.clone();
            ws.on_event(
                &id,
                Event::Permission {
                    tool: "bash".into(),
                    summary: "ls".into(),
                    reply,
                },
                cx,
            );
        });
        cx.run_until_parked();
        rx
    }

    #[gpui_kit::test]
    async fn approvals_answer_from_the_keyboard(cx: &mut TestAppContext) {
        let (ws, cx) = open(cx);
        for (keys, expected) in [
            ("cmd-enter", Decision::Allow),
            ("cmd-shift-enter", Decision::Always),
            ("escape", Decision::Deny),
        ] {
            let mut rx = ask(&ws, cx);
            assert!(ws.read_with(cx, |ws, _| ws.thread().status == Status::Waiting));
            cx.simulate_keystrokes(keys);
            assert_eq!(rx.try_recv().ok(), Some(expected), "{keys}");
            assert!(ws.read_with(cx, |ws, _| ws.thread().pending.is_none()));
        }
    }

    #[gpui_kit::test]
    async fn theme_matches_window_on_startup(cx: &mut TestAppContext) {
        use gpui_kit::component::{ActiveTheme, Theme, ThemeMode};
        let (_, cx) = open(cx);
        cx.update(|window, cx| {
            Theme::change(ThemeMode::Dark, None, cx);
            let expected_dark = matches!(
                window.appearance(),
                gpui_kit::WindowAppearance::Dark | gpui_kit::WindowAppearance::VibrantDark
            );
            let _ws = cx.new(|cx| Workspace::new(Config::default(), None, window, cx));
            assert_eq!(cx.theme().is_dark(), expected_dark);
        });
    }

    #[gpui_kit::test]
    async fn restart_reopens_the_thread_that_was_showing(cx: &mut TestAppContext) {
        use greencode_core::permission::Mode;

        let (ws, cx) = open(cx);
        let id = ws.read_with(cx, |ws, _| ws.thread().meta.id.clone());
        let model = ws.read_with(cx, |ws, _| ws.thread().meta.model.clone());
        // A draft is enough to save the thread, as when the app closes with unsent text.
        ws.update(cx, |ws, _| {
            ws.threads[0].draft = "keep me".into();
            ws.threads[0].perms.set_mode(Mode::Plan);
            ws.threads[0].save().unwrap();
            // Global defaults from an earlier run must not override the restored thread.
            ws.ui["mode"] = json!("ask");
            ws.ui["model"] = json!("openai/other");
            ws.save_ui();
        });
        let saved: serde_json::Value =
            serde_json::from_slice(&std::fs::read(greencode_core::home().join("ui.json")).unwrap())
                .unwrap();
        assert_eq!(saved["last_thread"], json!(id));
        let saved_ids: Vec<String> = greencode_core::session::Session::list()
            .into_iter()
            .map(|m| m.id)
            .collect();
        assert!(saved_ids.contains(&id));

        cx.update(|window, cx| {
            let again = cx.new(|cx| Workspace::new(Config::default(), None, window, cx));
            again.read_with(cx, |ws, _| {
                // The same saved threads, with the remembered one shown and no fresh one added.
                let ids: Vec<String> = ws.threads.iter().map(|t| t.meta.id.clone()).collect();
                assert_eq!(ids.len(), saved_ids.len());
                assert!(saved_ids.iter().all(|id| ids.contains(id)));
                assert_eq!(ws.threads[0].meta.id, id);
                assert_eq!(ws.active, 0);
                assert_eq!(ws.threads[0].draft, "keep me");
                assert_eq!(ws.threads[0].meta.model, model);
                assert_eq!(ws.threads[0].perms.mode(), Mode::Plan);
                assert_eq!(ws.cfg.mode, Mode::Ask);
            });
        });
    }

    #[gpui_kit::test]
    async fn restart_replaces_a_thread_with_nothing_saved(cx: &mut TestAppContext) {
        let (ws, cx) = open(cx);
        let id = ws.read_with(cx, |ws, _| ws.thread().meta.id.clone());
        // The fresh thread has no view, so `last_thread` alone must not bring back an empty shell.
        assert!(!greencode_core::home()
            .join("threads")
            .join(format!("{id}.json"))
            .exists());
        let saved_ids: Vec<String> = greencode_core::session::Session::list()
            .into_iter()
            .map(|m| m.id)
            .collect();
        assert!(!saved_ids.contains(&id));

        cx.update(|window, cx| {
            let again = cx.new(|cx| Workspace::new(Config::default(), None, window, cx));
            again.read_with(cx, |ws, _| {
                assert_ne!(ws.threads[0].meta.id, id);
                assert!(!saved_ids.contains(&ws.threads[0].meta.id));
                assert!(ws.threads.iter().all(|t| t.meta.id != id));
            });
        });
    }

    #[gpui_kit::test]
    async fn restart_drops_unstarted_threads_but_keeps_the_last_draft(cx: &mut TestAppContext) {
        let (ws, cx) = open(cx);
        let first = ws.read_with(cx, |ws, _| ws.thread().meta.id.clone());
        cx.simulate_input("draft one");
        // A second thread with a draft but no sent message is still an empty composer.
        cx.simulate_keystrokes("cmd-n");
        cx.simulate_input("draft two");
        cx.simulate_keystrokes("cmd-2");
        assert_eq!(ws.read_with(cx, |ws, _| ws.thread().meta.id.clone()), first);

        cx.update(|window, cx| {
            let again = cx.new(|cx| Workspace::new(Config::default(), None, window, cx));
            again.read_with(cx, |ws, _| {
                // The saved second draft is not history; the remembered thread keeps its own draft.
                assert_eq!(ws.threads.len(), 1);
                assert_eq!(ws.threads[0].meta.id, first);
                assert_eq!(ws.threads[0].draft, "draft one");
            });
        });
    }

    #[gpui_kit::test]
    async fn deleting_threads_keeps_the_viewed_thread_and_replaces_the_last_one(
        cx: &mut TestAppContext,
    ) {
        use greencode_core::session::Session;
        let (ws, cx) = open(cx);
        let cwd = ws.read_with(cx, |ws, _| ws.thread().meta.cwd.clone());
        let ids: Vec<String> = ["kept", "removed"]
            .iter()
            .map(|title| {
                let mut meta = Meta::new(cwd.clone(), "m".into(), "native".into());
                meta.title = title.to_string();
                Session::create(&meta).unwrap();
                ws.update(cx, |ws, _| {
                    let t = super::Thread::new(meta.clone(), false, Default::default());
                    t.save().unwrap();
                    ws.threads.push(t);
                });
                meta.id
            })
            .collect();

        // View the second saved thread; deleting the one above it must keep the view and fix the index.
        cx.simulate_keystrokes("cmd-3");
        assert_eq!(ws.read_with(cx, |ws, _| ws.active), 2);
        cx.update(|window, cx| {
            ws.update(cx, |ws, cx| {
                ws.run(super::Act::DeleteThread(ids[0].clone()), window, cx)
            })
        });
        ws.read_with(cx, |ws, _| {
            assert_eq!(ws.active, 1);
            assert_eq!(ws.thread().meta.id, ids[1]);
            assert!(ws.threads.iter().all(|t| t.meta.id != ids[0]));
        });
        assert!(Session::open(&ids[0]).is_none());
        assert!(Session::view(&ids[0]).unwrap().is_none());
        assert!(Session::view(&ids[1]).unwrap().is_some());

        // A running thread refuses deletion.
        ws.update(cx, |ws, _| ws.threads[1].status = Status::Running);
        cx.update(|window, cx| {
            ws.update(cx, |ws, cx| {
                ws.run(super::Act::DeleteThread(ids[1].clone()), window, cx)
            })
        });
        ws.update(cx, |ws, _| {
            assert_eq!(ws.threads.len(), 2);
            assert!(matches!(
                ws.thread().items.last(),
                Some(Item::Error(e)) if e.contains("実行を停止してからスレッドを削除してください")
            ));
            ws.threads[1].status = Status::Idle;
        });

        // Deleting the viewed thread selects the neighbour now at that index.
        cx.update(|window, cx| {
            ws.update(cx, |ws, cx| {
                ws.run(super::Act::DeleteThread(ids[1].clone()), window, cx)
            })
        });
        ws.read_with(cx, |ws, _| {
            assert_eq!((ws.threads.len(), ws.active), (1, 0));
            assert!(ws.threads.iter().all(|t| t.meta.id != ids[1]));
        });
        assert!(Session::open(&ids[1]).is_none());
        assert!(Session::view(&ids[1]).unwrap().is_none());

        // Deleting the last thread leaves a fresh composer rather than an empty list.
        let last = ws.read_with(cx, |ws, _| ws.thread().meta.id.clone());
        cx.update(|window, cx| ws.update(cx, |ws, cx| ws.run(super::Act::Delete, window, cx)));
        ws.read_with(cx, |ws, _| {
            assert_eq!((ws.threads.len(), ws.active), (1, 0));
            assert_ne!(ws.thread().meta.id, last);
        });
    }

    #[gpui_kit::test]
    async fn diff_refresh_invalidates_previous_results_and_clears_display(cx: &mut TestAppContext) {
        let (ws, cx) = open(cx);
        ws.update(cx, |ws, cx| {
            ws.show_diff = true;
            ws.diff.push("old thread".into());
            let previous = ws.diff_generation;
            ws.refresh_diff(cx);
            assert!(ws.diff.is_empty());
            assert_ne!(ws.diff_generation, previous);
            let id = ws.thread().meta.id.clone();
            assert!(!ws.apply_diff(previous, &id, "stale"));
            assert!(ws.apply_diff(ws.diff_generation, &id, "current"));
            assert_eq!(ws.diff[0].as_ref(), "current");
            let pending = ws.diff_generation;
            ws.show_diff = false;
            ws.refresh_diff(cx);
            assert_ne!(ws.diff_generation, pending);
        });
    }

    #[gpui_kit::test]
    async fn threads_palette_and_completion(cx: &mut TestAppContext) {
        let (ws, cx) = open(cx);
        // A never-saved thread survives switching away and back.
        cx.simulate_keystrokes("cmd-n");
        assert_eq!(
            ws.read_with(cx, |ws, _| (ws.threads.len(), ws.active)),
            (2, 0)
        );
        cx.simulate_keystrokes("cmd-2");
        cx.simulate_keystrokes("cmd-1");
        assert!(ws.read_with(cx, |ws, _| ws.active == 0
            && ws.threads.iter().all(|t| t.items.is_empty())));

        // Palette: filter, then Enter runs the first match.
        cx.simulate_keystrokes("cmd-k");
        cx.simulate_input("差分");
        cx.simulate_keystrokes("enter");
        assert!(ws.read_with(cx, |ws, _| ws.show_diff && ws.palette.is_none()));
        cx.simulate_keystrokes("cmd-k");
        cx.simulate_keystrokes("escape");
        assert!(ws.read_with(cx, |ws, _| ws.palette.is_none()));

        // Tab completes a slash command in the composer.
        cx.simulate_input("/co");
        cx.simulate_keystrokes("tab");
        assert_eq!(
            ws.read_with(cx, |ws, cx| ws.input.read(cx).value().to_string()),
            "/compact "
        );

        // Streamed text extends the last assistant item instead of adding rows.
        ws.update(cx, |ws, cx| {
            let id = ws.thread().meta.id.clone();
            for part in ["Hel", "lo"] {
                ws.on_event(&id, Event::Text(part.into()), cx);
            }
            ws.on_event(&id, Event::Done { waiting_for_delegates: false }, cx);
            assert!(matches!(&ws.thread().items[..], [Item::Assistant(t)] if t == "Hello"));
            // A repeated tool id (ACP's stub first, full arguments later) updates the row instead of adding one.
            ws.on_event(&id, Event::ToolStart { id: "c1".into(), name: "shell".into(), input: json!({"cwd": "/x"}) }, cx);
            ws.on_event(&id, Event::ToolStart { id: "c1".into(), name: "shell".into(), input: json!({"command": "ls", "cwd": "/x"}) }, cx);
            assert!(matches!(&ws.thread().items[1..], [Item::Tool { input, .. }] if input["command"] == "ls"));
        });
    }

    #[gpui_kit::test]
    async fn subagent_model_palette_saves_selection_and_sends_it_to_the_running_thread(
        cx: &mut TestAppContext,
    ) {
        use greencode_core::{session::Session, Command};
        let (ws, cx) = open(cx);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        ws.update(cx, |ws, _| {
            std::sync::Arc::make_mut(&mut ws.cfg)
                .models
                .push("openai/gpt-6-luna".into());
            ws.threads[ws.active].cmd = Some(tx);
        });
        let parent = ws.read_with(cx, |ws, _| ws.thread().meta.model.clone());
        cx.simulate_input("draft to keep");
        cx.simulate_keystrokes("cmd-k");
        cx.simulate_input("サブエージェント既定モデル: openai/gpt-6-luna");
        cx.simulate_keystrokes("enter");
        assert!(
            matches!(rx.try_recv(), Ok(Command::SubagentModel(Some(model))) if model == "openai/gpt-6-luna")
        );
        ws.read_with(cx, |ws, _| {
            assert_eq!(ws.thread().meta.model, parent);
            let view = Session::view(&ws.thread().meta.id).unwrap().unwrap();
            assert_eq!(
                view.meta.subagent_model.as_deref(),
                Some("openai/gpt-6-luna")
            );
            assert_eq!(view.draft, "draft to keep");
        });
        cx.simulate_keystrokes("cmd-n");
        assert_eq!(
            ws.read_with(cx, |ws, _| ws.thread().meta.subagent_model.clone())
                .as_deref(),
            Some("openai/gpt-6-luna")
        );
        cx.simulate_input("another draft");
        cx.simulate_keystrokes("cmd-k");
        cx.simulate_input("サブエージェント既定モデル: 親と同じ");
        cx.simulate_keystrokes("enter");
        ws.read_with(cx, |ws, _| {
            assert!(ws.thread().meta.subagent_model.is_none());
            assert!(Session::view(&ws.thread().meta.id)
                .unwrap()
                .unwrap()
                .meta
                .subagent_model
                .is_none());
        });
    }

    #[gpui_kit::test]
    async fn project_model_worktree_and_unicode_completion(cx: &mut TestAppContext) {
        let (ws, cx) = open(cx);
        let project = greencode_core::home().join("project-picker");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(project.join("README.md"), "fixture\n").unwrap();
        for args in [
            vec!["init", "-q"],
            vec!["add", "README.md"],
            vec![
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.invalid",
                "commit",
                "-qm",
                "fixture",
            ],
        ] {
            assert!(std::process::Command::new("git")
                .arg("-C")
                .arg(&project)
                .args(args)
                .status()
                .unwrap()
                .success());
        }

        cx.simulate_keystrokes("cmd-o");
        assert!(cx.did_prompt_for_paths());
        cx.simulate_path_prompt_response(|options| {
            assert!(options.directories && !options.files && !options.multiple);
            Some(vec![project.clone()])
        });
        cx.run_until_parked();
        assert_eq!(
            ws.read_with(cx, |ws, _| ws.thread().meta.cwd.clone()),
            project
        );

        ws.update(cx, |ws, _| {
            std::sync::Arc::make_mut(&mut ws.cfg)
                .models
                .push("openai/test-model".into())
        });
        cx.simulate_keystrokes("cmd-k");
        cx.simulate_input("openai/test-model");
        cx.simulate_keystrokes("enter");
        assert_eq!(
            ws.read_with(cx, |ws, _| ws.thread().meta.model.clone()),
            "openai/test-model"
        );

        cx.simulate_input("確認\u{3000}@REA");
        cx.simulate_keystrokes("tab");
        assert_eq!(
            ws.read_with(cx, |ws, cx| ws.input.read(cx).value().to_string()),
            "確認\u{3000}README.md "
        );

        cx.simulate_keystrokes("cmd-shift-n");
        ws.read_with(cx, |ws, _| {
            let meta = &ws.thread().meta;
            assert_eq!(meta.repo.as_ref(), Some(&project));
            assert_ne!(meta.cwd, project);
            assert_eq!(
                std::fs::read_to_string(meta.cwd.join("README.md")).unwrap(),
                "fixture\n"
            );
            assert_eq!(meta.model, "openai/test-model");
        });
    }

    #[gpui_kit::test]
    async fn composer_draft_is_written_after_a_pause(cx: &mut TestAppContext) {
        use greencode_core::session::Session;
        let (ws, cx) = open(cx);
        let id = ws.read_with(cx, |ws, _| ws.thread().meta.id.clone());
        cx.simulate_input("typed");
        // No write per keystroke.
        assert!(Session::view(&id).unwrap().is_none());
        cx.executor()
            .advance_clock(std::time::Duration::from_millis(300));
        cx.run_until_parked();
        assert_eq!(Session::view(&id).unwrap().unwrap().draft, "typed");
    }

    #[gpui_kit::test]
    async fn drafts_modes_and_held_queues_survive_thread_switches(cx: &mut TestAppContext) {
        use greencode_core::{permission::Mode, session::Session, Command};
        let (ws, cx) = open(cx);
        let first = ws.read_with(cx, |ws, _| ws.thread().meta.id.clone());
        cx.simulate_input("draft one");
        cx.simulate_keystrokes("cmd-n");
        cx.simulate_input("draft two");
        cx.simulate_keystrokes("cmd-2");
        assert_eq!(
            ws.read_with(cx, |ws, cx| ws.input.read(cx).value().to_string()),
            "draft one"
        );
        assert_eq!(Session::view(&first).unwrap().unwrap().draft, "draft one");
        ws.update(cx, |ws, _| ws.thread().perms.set_mode(Mode::Ask));
        cx.simulate_keystrokes("cmd-1");
        assert_eq!(
            ws.read_with(cx, |ws, _| ws.thread().perms.mode()),
            Mode::Full
        );

        let (commands, mut rx) = tokio::sync::mpsc::unbounded_channel();
        ws.update(cx, |ws, _| {
            let t = &mut ws.threads[ws.active];
            t.cmd = Some(commands);
            t.status = Status::Running;
            t.queue_held = false;
        });
        cx.simulate_keystrokes("enter");
        ws.update(cx, |ws, cx| {
            assert_eq!(ws.thread().queue.front().unwrap(), "draft two");
            ws.stop();
            let id = ws.thread().meta.id.clone();
            ws.on_event(&id, Event::Done { waiting_for_delegates: false }, cx);
            assert_eq!(ws.thread().queue.len(), 1);
            assert_eq!(Session::view(&id).unwrap().unwrap().queue, ["draft two"]);
        });
        assert!(matches!(rx.try_recv(), Ok(Command::Cancel)));
        assert!(rx.try_recv().is_err());
        cx.update(|window, cx| ws.update(cx, |ws, cx| ws.run(super::Act::ResumeQueue, window, cx)));
        assert!(matches!(rx.try_recv(),Ok(Command::Prompt(t)) if t == "draft two"));
        assert!(ws.read_with(cx, |ws, _| ws.thread().queue.is_empty()));
    }

    #[gpui_kit::test]
    async fn queue_reordering_persists_without_sending(cx: &mut TestAppContext) {
        use greencode_core::session::Session;
        let (ws, cx) = open(cx);
        ws.update(cx, |ws, _| {
            ws.threads[0]
                .queue
                .extend(["one".into(), "two".into(), "three".into()])
        });
        cx.update(|window, cx| {
            ws.update(cx, |ws, cx| {
                ws.run(super::Act::MoveQueue(2, true), window, cx);
                ws.run(super::Act::MoveQueue(0, false), window, cx);
                ws.run(super::Act::MoveQueue(0, true), window, cx);
                ws.run(super::Act::MoveQueue(2, false), window, cx);
                assert_eq!(
                    Session::view(&ws.thread().meta.id).unwrap().unwrap().queue,
                    ["three", "one", "two"]
                );
                assert!(ws.thread().queue_held);
            })
        });
    }

    #[gpui_kit::test]
    async fn queue_edit_preserves_existing_draft(cx: &mut TestAppContext) {
        let (ws, cx) = open(cx);
        ws.update(cx, |ws, _| {
            ws.threads[0].queue.extend(["one".into(), "two".into()])
        });
        cx.simulate_input("draft");
        cx.update(|window, cx| {
            ws.update(cx, |ws, cx| ws.run(super::Act::EditQueue(0), window, cx))
        });
        assert_eq!(
            ws.read_with(cx, |ws, cx| ws.input.read(cx).value().to_string()),
            "one"
        );
        cx.update(|window, cx| {
            ws.update(cx, |ws, cx| {
                ws.input
                    .update(cx, |s, cx| s.set_value("changed", window, cx));
                ws.finish_queue_edit(true, window, cx);
                assert_eq!(ws.thread().queue[0], "changed");
                assert_eq!(ws.input.read(cx).value(), "draft");
                ws.edit_queue(1, window, cx);
                ws.input
                    .update(cx, |s, cx| s.set_value("discard", window, cx));
                ws.finish_queue_edit(false, window, cx);
                assert_eq!(ws.thread().queue[1], "two");
                assert_eq!(ws.input.read(cx).value(), "draft");
                ws.input.update(cx, |s, cx| s.set_value("", window, cx));
                ws.edit_queue(0, window, cx);
                ws.input
                    .update(cx, |s, cx| s.set_value("recovered", window, cx));
                ws.remove_queue(0, window, cx);
                assert_eq!(ws.input.read(cx).value(), "recovered");
                assert!(ws.queue_edit.is_none());
            })
        });
    }

    #[gpui_kit::test]
    async fn rewind_works_for_external_backends(cx: &mut TestAppContext) {
        let (ws, cx) = open(cx);
        let id = ws.read_with(cx, |ws, _| ws.thread().meta.id.clone());
        // Two turns of a Pi thread, plus a saved provider session.
        let mut meta =
            greencode_core::session::Meta::new(std::env::temp_dir(), "default".into(), "pi".into());
        meta.id = id.clone();
        let session = greencode_core::session::Session::create(&meta).unwrap();
        session
            .append(&greencode_core::Message::user("first"))
            .unwrap();
        session
            .append(&greencode_core::Message {
                role: greencode_core::Role::Assistant,
                content: vec![greencode_core::Block::Text { text: "one".into() }],
            })
            .unwrap();
        session
            .append(&greencode_core::Message::user("second"))
            .unwrap();
        session
            .append(&greencode_core::Message {
                role: greencode_core::Role::Assistant,
                content: vec![greencode_core::Block::Text { text: "two".into() }],
            })
            .unwrap();
        greencode_core::session::Session::save_runtime(
            &id,
            &json!({"backend":"pi","sessionId":"old"}),
        )
        .unwrap();
        ws.update(cx, |ws, _| ws.threads[0].meta.backend = "pi".into());
        ws.update_in(cx, |ws, window, cx| ws.run(super::Act::Undo, window, cx));
        ws.read_with(cx, |ws, _| {
            assert_eq!(
                ws.thread().draft,
                "second",
                "the last prompt comes back for editing"
            );
            assert!(
                greencode_core::session::Session::runtime(&id)
                    .unwrap()
                    .is_null(),
                "the provider session is dropped"
            );
            assert!(ws.thread().cmd.is_none());
        });
    }

    #[gpui_kit::test]
    async fn webhook_rotate_replaces_the_url(cx: &mut TestAppContext) {
        let (ws, cx) = open(cx);
        ws.update_in(cx, |ws, window, cx| {
            ws.command("/webhook review the diff", false, window, cx)
        });
        let old = ws.read_with(cx, |ws, _| {
            ws.ui["webhooks"][0]["id"].as_str().unwrap().to_string()
        });
        ws.update_in(cx, |ws, window, cx| {
            ws.command(&format!("/webhook-rotate {old}"), false, window, cx)
        });
        ws.read_with(cx, |ws, _| {
            let new = ws.ui["webhooks"][0]["id"].as_str().unwrap();
            assert_ne!(new, old, "the old URL stops working");
            assert_eq!(ws.ui["webhooks"][0]["prompt"], "review the diff");
        });
    }

    #[gpui_kit::test]
    async fn skill_command_loads_the_skill_body(cx: &mut TestAppContext) {
        let (ws, cx) = open(cx);
        let dir = std::env::temp_dir().join(format!(
            "greencode-skill-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(dir.join(".greencode/skills/review")).unwrap();
        std::fs::write(
            dir.join(".greencode/skills/review/SKILL.md"),
            "---\nname: review\ndescription: d\n---\nCheck the diff first.\n",
        )
        .unwrap();
        ws.update(cx, |ws, _| ws.threads[0].meta.cwd = dir.clone());
        ws.update_in(cx, |ws, window, cx| {
            ws.command("/skill review", false, window, cx)
        });
        ws.read_with(cx, |ws, cx| {
            let draft = ws.input.read(cx).value().to_string();
            assert!(
                draft.contains("スキル `review`") && draft.contains("Check the diff first."),
                "{draft}"
            );
            assert!(!draft.contains("description: d"), "frontmatter is stripped");
        });
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[gpui_kit::test]
    async fn retry_resends_the_last_prompt(cx: &mut TestAppContext) {
        let (ws, cx) = open(cx);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        ws.update(cx, |ws, _| {
            let t = &mut ws.threads[0];
            t.cmd = Some(tx);
            t.items.push(Item::Error("boom".into()));
            t.last_prompt = Some("fix it".into());
        });
        ws.update_in(cx, |ws, window, cx| ws.run(super::Act::Retry, window, cx));
        let sent = rx.try_recv().expect("a command is sent");
        assert!(matches!(sent, greencode_core::Command::Prompt(text) if text == "fix it"));
        assert!(ws.read_with(cx, |ws, _| matches!(
            ws.thread().items.last(),
            Some(Item::User(_))
        )));
    }

    #[gpui_kit::test]
    async fn tool_visibility_cycles_and_hides_rows(cx: &mut TestAppContext) {
        let (ws, cx) = open(cx);
        ws.update(cx, |ws, _| {
            ws.ui["tool_calls"] = json!("show");
            assert!(matches!(ws.tool_visibility(), super::ToolVisibility::Show));
            ws.cycle_tool_visibility();
            assert!(matches!(
                ws.tool_visibility(),
                super::ToolVisibility::Successful
            ));
            assert_eq!(ws.ui["hide_tools"], json!(true));
            ws.cycle_tool_visibility();
            ws.cycle_tool_visibility();
            assert!(matches!(ws.tool_visibility(), super::ToolVisibility::Zen));
            ws.cycle_tool_visibility();
            assert!(matches!(ws.tool_visibility(), super::ToolVisibility::Show));
            assert_eq!(ws.ui["hide_tools"], json!(false));
            // The older boolean still selects "hide all".
            ws.ui["tool_calls"] = json!(null);
            ws.ui["hide_tools"] = json!(true);
            assert!(matches!(ws.tool_visibility(), super::ToolVisibility::All));
        });
    }

    #[gpui_kit::test]
    async fn disabled_backends_and_providers_leave_the_pickers(cx: &mut TestAppContext) {
        let (ws, cx) = open(cx);
        ws.update(cx, |ws, _| {
            std::sync::Arc::make_mut(&mut ws.cfg)
                .backends
                .push(greencode_core::config::AcpCfg {
                    name: "pi".into(),
                    command: vec!["pi".into()],
                    protocol: greencode_core::config::Protocol::Pi,
                    env: Default::default(),
                });
            assert!(ws.backends().contains(&"pi".to_string()));
            ws.toggle_disabled("disabled_backends", "pi");
            assert!(
                !ws.backends().contains(&"pi".to_string()),
                "a disabled backend leaves the picker"
            );
            ws.toggle_disabled("disabled_backends", "pi");
            assert!(ws.backends().contains(&"pi".to_string()));
            assert!(ws.models().iter().any(|m| m.starts_with("anthropic/")));
            ws.toggle_disabled("disabled_providers", "anthropic");
            assert!(
                !ws.models().iter().any(|m| m.starts_with("anthropic/")),
                "a disabled provider leaves the model list"
            );
        });
    }

    #[gpui_kit::test]
    async fn restart_agent_forgets_the_provider_session(cx: &mut TestAppContext) {
        let (ws, cx) = open(cx);
        let id = ws.read_with(cx, |ws, _| ws.thread().meta.id.clone());
        greencode_core::session::Session::save_runtime(
            &id,
            &json!({"backend":"native","sessionId":"old"}),
        )
        .unwrap();
        ws.update_in(cx, |ws, window, cx| {
            ws.run(super::Act::RestartAgent, window, cx)
        });
        assert!(
            greencode_core::session::Session::runtime(&id)
                .unwrap()
                .is_null(),
            "runtime handle is dropped"
        );
        ws.read_with(cx, |ws, _| {
            assert!(matches!(ws.thread().items.last(), Some(Item::Notice(_))))
        });
    }

    #[gpui_kit::test]
    async fn rewind_restores_bash_changes_from_a_checkpoint(cx: &mut TestAppContext) {
        let (ws, cx) = open(cx);
        let dir = std::env::temp_dir().join(format!(
            "greencode-ckpt-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        for args in [
            &["init", "-q"][..],
            &["config", "user.email", "a@b"][..],
            &["config", "user.name", "a"][..],
        ] {
            std::process::Command::new("git")
                .arg("-C")
                .arg(&dir)
                .args(args)
                .output()
                .unwrap();
        }
        std::fs::write(dir.join("a.txt"), "one\n").unwrap();
        std::process::Command::new("git")
            .arg("-C")
            .arg(&dir)
            .args(["add", "."])
            .output()
            .unwrap();
        std::process::Command::new("git")
            .arg("-C")
            .arg(&dir)
            .args(["commit", "-qm", "init"])
            .output()
            .unwrap();
        ws.update(cx, |ws, _| ws.threads[0].meta.cwd = dir.clone());
        ws.update(cx, |ws, _| ws.record_checkpoint(0));
        // A `bash` command changes the file after the checkpoint.
        std::fs::write(dir.join("a.txt"), "one\nbash\n").unwrap();
        ws.update_in(cx, |ws, _, cx| ws.bash_diff(cx));
        assert!(ws.read_with(cx, |ws, _| ws.diff.iter().any(|l| l.contains("+bash"))));
        let restored = ws.update(cx, |ws, _| ws.restore_checkpoint(0));
        assert_eq!(restored, Some(1));
        assert_eq!(std::fs::read_to_string(dir.join("a.txt")).unwrap(), "one\n");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn speech_text_drops_code_and_markdown() {
        let text = super::speech_text(
            "# Title\n\nUse `cargo test` now.\n```\nlet x = 1;\n```\n- next step",
        );
        assert!(
            text.contains("Title")
                && text.contains("Use cargo test now.")
                && text.contains("next step"),
            "{text}"
        );
        assert!(!text.contains("let x") && !text.contains('`') && !text.contains('#'));
    }

    #[test]
    fn utf8_split_across_reads_is_not_mangled() {
        let bytes = "日本語テキスト".as_bytes();
        let mut pending = Vec::new();
        let mut text = String::new();
        for byte in bytes {
            text.push_str(&super::take_utf8(&mut pending, &[*byte]));
        }
        assert_eq!(text, "日本語テキスト");
        assert!(pending.is_empty());
        // Invalid bytes still surface, and the decoder recovers afterwards.
        let mut pending = Vec::new();
        assert_eq!(
            super::take_utf8(&mut pending, &[b'a', 0xff, b'b']),
            "a\u{FFFD}b"
        );
        assert_eq!(super::take_utf8(&mut pending, "日".as_bytes()), "日");
    }

    #[gpui_kit::test]
    async fn queue_message_moves_to_an_empty_composer(cx: &mut TestAppContext) {
        let (ws, cx) = open(cx);
        ws.update(cx, |ws, _| {
            ws.threads[0].queue.extend(["one".into(), "two".into()])
        });
        cx.simulate_input("draft");
        cx.update(|window, cx| {
            ws.update(cx, |ws, cx| {
                ws.move_queue_to_composer(0, window, cx);
                assert_eq!(
                    ws.thread().queue.len(),
                    2,
                    "a non-empty composer keeps the queue"
                );
                ws.input.update(cx, |s, cx| s.set_value("", window, cx));
                ws.move_queue_to_composer(1, window, cx);
                assert_eq!(ws.thread().queue.len(), 1);
                assert_eq!(ws.thread().queue[0], "one");
                assert_eq!(ws.input.read(cx).value(), "two");
            })
        });
    }

    #[gpui_kit::test]
    async fn queue_send_now_starts_idle_thread(cx: &mut TestAppContext) {
        let (ws, cx) = open(cx);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        ws.update(cx, |ws, _| {
            ws.threads[0].cmd = Some(tx);
            ws.threads[0]
                .queue
                .extend(["first".into(), "second".into()]);
        });
        cx.update(|window, cx| ws.update(cx, |ws, cx| ws.send_queue_now(1, window, cx)));
        assert!(
            matches!(rx.try_recv(), Ok(greencode_core::Command::Prompt(text)) if text == "second")
        );
        ws.read_with(cx, |ws, _| {
            assert_eq!(ws.thread().queue.len(), 1);
            assert!(ws.thread().status == super::Status::Running);
        });
    }

    #[gpui_kit::test]
    async fn queue_send_now_steers_running_native_thread(cx: &mut TestAppContext) {
        let (ws, cx) = open(cx);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        ws.update(cx, |ws, _| {
            let t = &mut ws.threads[0];
            t.meta.backend = "native".into();
            t.status = super::Status::Running;
            t.cmd = Some(tx);
            t.queue.push_back("new direction".into());
        });
        cx.update(|window, cx| ws.update(cx, |ws, cx| ws.send_queue_now(0, window, cx)));
        assert!(
            matches!(rx.try_recv(), Ok(greencode_core::Command::Steer(text)) if text == "new direction")
        );
        ws.read_with(cx, |ws, _| {
            assert!(ws.thread().queue.is_empty());
            assert!(matches!(ws.thread().items.last(), Some(super::Item::User(text)) if text.as_ref() == "new direction"));
        });
    }

    #[gpui_kit::test]
    async fn queue_send_now_steers_running_opencode_thread(cx: &mut TestAppContext) {
        let (ws, cx) = open(cx);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        ws.update(cx, |ws, cx| {
            let id = ws.threads[0].meta.id.clone();
            ws.on_event(
                &id,
                Event::Remote {
                    command: "acp_state".into(),
                    data: json!({"steering": true}),
                },
                cx,
            );
            let t = &mut ws.threads[0];
            t.meta.backend = "opencode".into();
            t.status = super::Status::Running;
            t.cmd = Some(tx);
            t.queue.push_back("new direction".into());
        });
        cx.update(|window, cx| ws.update(cx, |ws, cx| ws.send_queue_now(0, window, cx)));
        assert!(
            matches!(rx.try_recv(), Ok(greencode_core::Command::Steer(text)) if text == "new direction")
        );
        ws.read_with(cx, |ws, _| {
            assert!(ws.thread().queue.is_empty());
            assert!(matches!(ws.thread().items.last(), Some(super::Item::User(text)) if text.as_ref() == "new direction"));
        });
    }

    #[gpui_kit::test]
    async fn subagent_progress_is_transient_and_updates_by_name(cx: &mut TestAppContext) {
        let (ws, cx) = open(cx);
        ws.update(cx, |ws, cx| {
            let id = ws.thread().meta.id.clone();
            assert!(greencode_core::session::Session::view(&id)
                .unwrap()
                .is_none());
            for data in [
                json!({"name":"a","agent":"task","state":"running","detail":"read x"}),
                json!({"name":"a","agent":"task","state":"done","detail":"finished"}),
                json!({"name":"b","agent":"scout","state":"running","detail":""}),
            ] {
                ws.on_event(
                    &id,
                    Event::Remote {
                        command: "subagent".into(),
                        data,
                    },
                    cx,
                );
            }
            let rows = &ws.thread().subs;
            assert_eq!(rows.len(), 2);
            assert_eq!(
                (
                    rows[0].name.as_str(),
                    rows[0].agent.as_str(),
                    rows[0].state.as_str(),
                    rows[0].detail.as_str()
                ),
                ("a", "task", "done", "finished")
            );
            assert_eq!(
                (
                    rows[1].name.as_str(),
                    rows[1].agent.as_str(),
                    rows[1].state.as_str(),
                    rows[1].detail.as_str()
                ),
                ("b", "scout", "running", "")
            );
            assert!(greencode_core::session::Session::view(&id)
                .unwrap()
                .is_none());
        });
    }

    #[gpui_kit::test]
    async fn subagent_controls_and_slash_command(cx: &mut TestAppContext) {
        let (ws, cx) = open(cx);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        ws.update(cx, |ws, cx| {
            let t = &mut ws.threads[0];
            t.meta.backend = "native".into();
            t.status = Status::Running;
            t.cmd = Some(tx);
            ws.steer_sub("a", "first", cx);
            ws.cancel_sub("a", cx);
        });
        assert!(
            matches!(rx.try_recv(), Ok(greencode_core::Command::Remote(v)) if v == json!({"type":"sub_steer","name":"a","message":"first"}))
        );
        assert!(
            matches!(rx.try_recv(), Ok(greencode_core::Command::Remote(v)) if v == json!({"type":"sub_cancel","name":"a"}))
        );
        cx.update(|window, cx| ws.update(cx, |ws, cx| {
            assert!(ws.command("/to a hello", false, window, cx));
            assert!(ws.thread().queue.is_empty());
            assert!(matches!(ws.thread().items.last(), Some(Item::Notice(text)) if text.as_ref() == "→ a: hello"));
        }));
        assert!(
            matches!(rx.try_recv(), Ok(greencode_core::Command::Remote(v)) if v == json!({"type":"sub_steer","name":"a","message":"hello"}))
        );
        assert!(rx.try_recv().is_err());
    }

    #[gpui_kit::test]
    async fn new_prompt_clears_subagent_rows(cx: &mut TestAppContext) {
        let (ws, cx) = open(cx);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        ws.update(cx, |ws, cx| {
            ws.threads[0].cmd = Some(tx);
            ws.threads[0].subs.push(super::SubRow {
                name: "a".into(),
                agent: "task".into(),
                state: "done".into(),
                detail: "done".into(),
                task: None,
            });
            ws.start_prompt("next".into(), cx);
            assert!(ws.thread().subs.is_empty());
        });
        assert!(
            matches!(rx.try_recv(), Ok(greencode_core::Command::Prompt(text)) if text == "next")
        );
    }

    #[gpui_kit::test]
    async fn orchestration_events_open_child_threads_and_save_schedules(cx: &mut TestAppContext) {
        let (ws, cx) = open(cx);
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        ws.update(cx, |ws, cx| {
            let parent = ws.thread().meta.id.clone();
            let project = ws.thread().project();
            let mut child = Meta::new(project.clone(), "default".into(), "codex".into());
            child.parent = Some(parent.clone());
            child.title = "↳ kid: say hi".into();
            let child_id = child.id.clone();
            let opened = Event::Remote { command: "thread_opened".into(), data: json!({"meta": child}) };
            ws.on_event(&parent, opened, cx);
            let ix = ws.threads.iter().position(|t| t.meta.id == child_id).expect("child listed");
            assert_eq!(ws.threads[ix].meta.parent.as_deref(), Some(parent.as_str()));
            assert_eq!(ws.threads.iter().filter(|t| t.meta.id == child_id).count(), 1);
            let request = json!({"method": "schedule", "action": "save", "prompt": "nightly", "project": project,
                "schedule": {"type": "interval", "everyMs": 3_600_000}});
            ws.on_event(&parent, Event::Dialog { request, reply: reply_tx }, cx);
            let saved = &ws.ui["schedules"][0];
            assert_eq!((saved["prompt"].as_str(), saved["every"].as_u64()), (Some("nightly"), Some(3600)));
        });
        let reply = reply_rx.await.expect("schedule request answered");
        assert!(reply["scheduledTaskId"].is_string(), "{reply}");
    }

    #[gpui_kit::test]
    async fn clearing_untitled_thread_state_is_persisted(cx: &mut TestAppContext) {
        use greencode_core::session::Session;
        let (ws, cx) = open(cx);
        ws.update(cx, |ws, _| {
            let t = &mut ws.threads[0];
            t.draft = "draft".into();
            t.queue.push_back("queued".into());
            t.save().unwrap();
            t.draft.clear();
            t.queue.clear();
            t.save().unwrap();
            let view = Session::view(&t.meta.id).unwrap().unwrap();
            assert!(view.draft.is_empty());
            assert!(view.queue.is_empty());
        });
    }

    #[gpui_kit::test]
    async fn extension_dialog_and_parallel_approvals_are_not_lost(cx: &mut TestAppContext) {
        let (ws, cx) = open(cx);
        let mut first = ask(&ws, cx);
        let mut second = ask(&ws, cx);
        cx.simulate_keystrokes("cmd-enter");
        assert_eq!(first.try_recv().unwrap(), Decision::Allow);
        assert!(ws.read_with(cx, |ws, _| ws.thread().pending.is_some()));
        cx.simulate_keystrokes("escape");
        assert_eq!(second.try_recv().unwrap(), Decision::Deny);
        let (reply, mut rx) = oneshot::channel();
        ws.update(cx, |ws, cx| {
            let id = ws.thread().meta.id.clone();
            ws.on_event(
                &id,
                Event::Dialog {
                    request: json!({"method":"input","title":"Question"}),
                    reply,
                },
                cx,
            );
        });
        cx.simulate_input("operator answer");
        cx.simulate_keystrokes("enter");
        assert_eq!(rx.try_recv().unwrap()["value"], "operator answer");
    }

    #[gpui_kit::test]
    async fn omp_ask_returns_multiple_answers(cx: &mut TestAppContext) {
        let (ws, cx) = open(cx);
        let (reply, mut rx) = oneshot::channel();
        ws.update(cx, |ws, cx| {
            let id = ws.thread().meta.id.clone();
            ws.on_event(&id, Event::Dialog { request: json!({"method":"ask","questions":[
                {"id":"a","question":"Choose","multi":true,"options":[{"label":"one"},{"label":"two"}]},
                {"id":"b","question":"Explain","options":[]}
            ]}), reply }, cx);
            ws.select_ask_option("one", cx);
            ws.select_ask_option("two", cx);
            ws.answer_ask(None, cx);
        });
        cx.simulate_input("details");
        cx.simulate_keystrokes("enter");
        assert_eq!(
            rx.try_recv().unwrap()["answers"],
            json!([
                {"id":"a","selectedOptions":["one","two"]},
                {"id":"b","selectedOptions":[],"customInput":"details"}
            ])
        );
    }

    #[gpui_kit::test]
    async fn timed_out_dialog_closes_only_matching_request(cx: &mut TestAppContext) {
        let (ws, cx) = open(cx);
        let (reply, _rx) = oneshot::channel();
        ws.update(cx, |ws, cx| {
            let id = ws.thread().meta.id.clone();
            ws.on_event(
                &id,
                Event::Dialog {
                    request: json!({"id":"current","method":"input"}),
                    reply,
                },
                cx,
            );
            ws.on_event(
                &id,
                Event::Remote {
                    command: "dialog_closed".into(),
                    data: json!({"id":"old"}),
                },
                cx,
            );
            assert!(ws.thread().dialog.is_some());
            ws.on_event(
                &id,
                Event::Remote {
                    command: "dialog_closed".into(),
                    data: json!({"id":"current"}),
                },
                cx,
            );
            assert!(ws.thread().dialog.is_none());
            assert!(matches!(ws.thread().status, super::Status::Running));
        });
    }

    #[gpui_kit::test]
    async fn hermes_import_opens_a_separate_thread_and_preserves_drafts(cx: &mut TestAppContext) {
        let (ws, cx) = open(cx);
        let source = greencode_core::home().join("fixture/profiles/ui-hermes");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::write(source.join("config.yaml"), "model: fixture\n").unwrap();
        ws.update(cx, |ws, _| {
            ws.hermes_profiles = vec![greencode_core::hermes::Profile {
                name: "ui-hermes".into(),
                home: source.clone(),
            }]
        });
        cx.simulate_input("keep this draft");
        let previous = ws.read_with(cx, |ws, _| ws.thread().meta.id.clone());
        cx.simulate_keystrokes("cmd-k");
        cx.simulate_input("Hermesプロファイルをインポート: ui-hermes");
        cx.simulate_keystrokes("enter");
        ws.read_with(cx, |ws, _| {
            assert_eq!(ws.thread().meta.backend, "hermes:ui-hermes");
            assert_eq!(ws.thread().meta.model, "default");
            assert_ne!(ws.thread().meta.id, previous);
            assert!(ws.thread().cmd.is_none());
            assert_eq!(
                ws.threads
                    .iter()
                    .find(|t| t.meta.id == previous)
                    .unwrap()
                    .draft,
                "keep this draft"
            );
            assert_eq!(
                ws.cfg
                    .backends
                    .iter()
                    .filter(|b| b.name == "hermes:ui-hermes")
                    .count(),
                1
            );
        });
        assert_eq!(
            std::fs::read_to_string(source.join("config.yaml")).unwrap(),
            "model: fixture\n"
        );
        assert_eq!(
            greencode_core::hermes::imported()
                .unwrap()
                .iter()
                .filter(|p| p.name == "ui-hermes")
                .count(),
            1
        );
        cx.simulate_keystrokes("cmd-k");
        cx.simulate_input("Hermesプロファイルをフォルダからインポート");
        cx.simulate_keystrokes("enter");
        assert!(cx.did_prompt_for_paths());
        cx.simulate_path_prompt_response(|options| {
            assert!(options.directories && !options.files && !options.multiple);
            Some(vec![source])
        });
        cx.run_until_parked();
        assert_eq!(
            ws.read_with(cx, |ws, _| ws.thread().meta.backend.clone()),
            "hermes:ui-hermes"
        );
        assert_eq!(
            ws.read_with(cx, |ws, _| ws
                .cfg
                .backends
                .iter()
                .filter(|b| b.name == "hermes:ui-hermes")
                .count()),
            1
        );
    }

    #[gpui_kit::test]
    async fn worktree_thread_model_switch_and_file_completion(cx: &mut TestAppContext) {
        let (ws, cx) = open(cx);
        let repo = std::env::temp_dir().join(format!("greencode-ui-repo-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&repo);
        std::fs::create_dir_all(repo.join("src")).unwrap();
        std::fs::write(repo.join("src/lib.rs"), "").unwrap();
        for args in [
            &["init", "-q"][..],
            &["add", "."],
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "commit",
                "-qm",
                "init",
            ],
        ] {
            assert!(std::process::Command::new("git")
                .arg("-C")
                .arg(&repo)
                .args(args)
                .status()
                .unwrap()
                .success());
        }
        ws.update_in(cx, |ws, window, cx| {
            ws.new_thread(repo.clone(), true, window, cx)
        });
        ws.read_with(cx, |ws, _| {
            let t = ws.thread();
            assert_eq!(t.meta.repo.as_ref(), Some(&repo));
            assert!(t.meta.cwd.join("src/lib.rs").exists() && t.meta.cwd != repo);
            assert_eq!(t.project(), repo);
        });

        // `@` completes to a tracked file of the thread's checkout.
        cx.simulate_input("see @lib");
        cx.simulate_keystrokes("tab");
        assert_eq!(
            ws.read_with(cx, |ws, cx| ws.input.read(cx).value().to_string()),
            "see src/lib.rs "
        );

        // Model and permission mode change through the same actions the palette and chips use.
        ws.update_in(cx, |ws, window, cx| {
            ws.run(super::Act::Model("openai/x".into()), window, cx);
            ws.run(
                super::Act::Mode(greencode_core::permission::Mode::Full),
                window,
                cx,
            );
            assert_eq!(ws.thread().meta.model, "openai/x");
            assert!(ws.thread().perms.mode() == greencode_core::permission::Mode::Full);
            // Deleting the thread keeps the worktree on disk.
            let cwd = ws.thread().meta.cwd.clone();
            ws.run(super::Act::Delete, window, cx);
            assert!(cwd.exists() && ws.threads.len() == 1);
        });
    }

    #[gpui_kit::test]
    async fn multi_repo_checkpoint_restores_each_repository_and_names_the_plain_folder(
        cx: &mut TestAppContext,
    ) {
        let (ws, cx) = open(cx);
        let root = std::env::temp_dir().join(format!(
            "greencode-ui-multi-{}-{}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ));
        let (api, web) = (root.join("shop/api"), root.join("shop/web"));
        for repo in [&api, &web] {
            std::fs::create_dir_all(repo).unwrap();
            std::fs::write(repo.join("f"), "one\n").unwrap();
            for args in [
                &["init", "-q"][..],
                &["add", "."][..],
                &["-c", "user.name=t", "-c", "user.email=t@t", "commit", "-qm", "init"][..],
            ] {
                assert!(std::process::Command::new("git")
                    .arg("-C")
                    .arg(repo)
                    .args(args)
                    .output()
                    .unwrap()
                    .status
                    .success());
            }
        }
        ws.update_in(cx, |ws, window, cx| ws.new_thread(root.join("shop"), false, window, cx));
        cx.run_until_parked();
        assert_eq!(ws.read_with(cx, |ws, _| ws.thread_repos()), vec![api.clone(), web.clone()]);

        ws.update(cx, |ws, cx| ws.refresh_repos(cx));
        cx.run_until_parked();
        ws.read_with(cx, |ws, _| {
            let acts = ws.repo_acts();
            assert!(acts.iter().any(|(label, _)| label == "リポジトリを追加…"));
            assert_eq!(acts.iter().filter(|(label, _)| label.starts_with("Worktree 対象 ✓")).count(), 2);
        });

        // The diff pane groups both repositories and narrows to the picked one.
        std::fs::write(api.join("g"), "x").unwrap();
        std::fs::write(web.join("g"), "x").unwrap();
        let grouped = ws.read_with(cx, |ws, _| ws.diff_scope().diff());
        assert!(grouped.contains("## api") && grouped.contains("b/api/g"));
        assert!(grouped.contains("## web") && grouped.contains("b/web/g"));
        ws.update_in(cx, |ws, window, cx| {
            ws.run(super::Act::DiffRepo(Some(api.clone())), window, cx)
        });
        let only_api = ws.read_with(cx, |ws, _| ws.diff_scope().diff());
        assert!(only_api.contains("## api") && !only_api.contains("## web"));
        ws.update_in(cx, |ws, window, cx| ws.run(super::Act::DiffRepo(None), window, cx));

        ws.update(cx, |ws, _| ws.record_checkpoint(0));
        ws.read_with(cx, |ws, _| {
            let entry = &ws.ui["checkpoints"][ws.thread().meta.id.as_str()][0];
            assert!(entry[api.to_string_lossy().as_ref()].is_string());
            assert!(entry[web.to_string_lossy().as_ref()].is_string());
        });
        std::fs::write(web.join("f"), "two\n").unwrap();
        assert_eq!(ws.update(cx, |ws, _| ws.restore_checkpoint(0)), Some(1));
        assert_eq!(std::fs::read_to_string(web.join("f")).unwrap(), "one\n");
        assert_eq!(std::fs::read_to_string(api.join("f")).unwrap(), "one\n");
        // The project folder itself holds no snapshot; the rewind names it instead of failing.
        assert!(ws.read_with(cx, |ws, _| matches!(
            ws.thread().items.last(),
            Some(Item::Notice(text)) if text.contains("shop")
        )));
        std::fs::remove_dir_all(root).unwrap();
    }

    /// `Config::default()` has no providers; `Config::load` adds the presets.
    fn with_presets(ws: &mut Workspace) {
        let providers = &mut std::sync::Arc::make_mut(&mut ws.cfg).providers;
        for &greencode_core::config::Preset { name, kind, url, env, .. } in
            greencode_core::config::PRESETS
        {
            providers
                .entry(name.into())
                .or_insert(greencode_core::config::ProviderCfg {
                    kind: kind.into(),
                    base_url: url.into(),
                    api_key_env: env.into(),
                    ..Default::default()
                });
        }
    }

    #[gpui_kit::test]
    async fn login_opens_key_form_for_a_known_provider(cx: &mut TestAppContext) {
        let (ws, cx) = open(cx);
        ws.update_in(cx, |ws, window, cx| {
            with_presets(ws);
            assert!(ws.command("/login openai", false, window, cx));
            assert_eq!(ws.key_form.as_ref().unwrap().provider, "openai");
            assert!(ws.provider_form.is_none());
            ws.close_key_form(window, cx);
            assert!(ws.command("/login brand-new", false, window, cx));
            assert!(ws.key_form.is_none());
            assert_eq!(
                ws.provider_form.as_ref().unwrap().fields[0]
                    .read(cx)
                    .value(),
                "brand-new"
            );
        });
    }

    #[gpui_kit::test]
    async fn login_picker_filters_presets_and_opens_custom_form(cx: &mut TestAppContext) {
        let (ws, cx) = open(cx);
        ws.update_in(cx, |ws, window, cx| {
            with_presets(ws);
            assert!(ws.command("/login", false, window, cx));
            assert!(ws.login_palette);
            let rows = ws.filtered_acts(cx);
            assert!(rows.iter().any(|(_, act)| matches!(act, super::Act::OpenCodeLogin)));
            assert!(rows.iter().any(|(label, _)| label.contains("カスタム")));
            assert!(rows.iter().any(|(label, act)| label.starts_with("OpenAI")
                && matches!(act, super::Act::LoginProvider(n) if n == "openai")));
            assert!(rows.iter().all(|(label, _)| label != "github-copilot" && label != "chatgpt"));
            assert_eq!(rows.iter().filter(|(_, act)| matches!(act, super::Act::CopilotLogin)).count(), 1);
            assert_eq!(rows.iter().filter(|(_, act)| matches!(act, super::Act::ChatGptLogin)).count(), 1);
            assert!(rows.iter().all(|(_, act)| !matches!(act, super::Act::LoginProvider(n) if n == "github-copilot" || n == "chatgpt")));
            ws.palette.as_ref().unwrap().0.update(cx, |s, cx| s.set_value("anthropic", window, cx));
            let rows = ws.filtered_acts(cx);
            assert_eq!(rows.len(), 1);
            ws.run(rows[0].1.clone(), window, cx);
            assert!(!ws.login_palette && ws.palette.is_none());
            assert_eq!(ws.key_form.as_ref().unwrap().provider, "anthropic");
            ws.close_key_form(window, cx);
            ws.command("/login", false, window, cx);
            ws.run(super::Act::AddProvider, window, cx);
            assert!(ws.provider_form.as_ref().unwrap().fields[0].read(cx).value().is_empty());
        });
    }

    #[gpui_kit::test]
    async fn login_copilot_opens_device_form_and_escape_closes_it(cx: &mut TestAppContext) {
        let (ws, cx) = open(cx);
        ws.update_in(cx, |ws, window, cx| {
            with_presets(ws);
            assert!(ws.command("/login", false, window, cx));
            assert!(ws
                .filtered_acts(cx)
                .iter()
                .any(|(label, act)| label.starts_with("GitHub Copilot")
                    && matches!(act, super::Act::CopilotLogin)));
            ws.run(
                super::Act::LoginProvider("github-copilot".into()),
                window,
                cx,
            );
            assert!(ws.key_form.is_none());
            assert!(ws.device_form.as_ref().is_some_and(|f| f.code.is_none()));
            ws.close_device_form(window, cx);
            assert!(ws.command("/login copilot", false, window, cx));
            assert!(ws.device_form.as_ref().is_some_and(|f| f.code.is_none()));
            ws.close_device_form(window, cx);
            assert!(ws.command("/login vibe", false, window, cx));
            assert!(ws
                .device_form
                .as_ref()
                .is_some_and(|f| f.kind == super::DeviceKind::Mistral && f.code.is_none()));
            assert!(ws
                .login_rows()
                .iter()
                .any(|(label, act)| label.starts_with("Mistral Vibe")
                    && matches!(act, super::Act::MistralLogin)));
        });
        cx.simulate_keystrokes("escape");
        assert!(ws.read_with(cx, |ws, _| ws.device_form.is_none()));
    }

    #[gpui_kit::test]
    async fn login_antigravity_opens_device_form_and_escape_closes_it(cx: &mut TestAppContext) {
        let (ws, cx) = open(cx);
        ws.update_in(cx, |ws, window, cx| {
            with_presets(ws);
            assert!(ws
                .login_rows()
                .iter()
                .any(|(label, act)| label.starts_with("Google Antigravity")
                    && matches!(act, super::Act::AntigravityLogin)));
            assert!(ws.command("/login antigravity", false, window, cx));
            assert!(ws
                .device_form
                .as_ref()
                .is_some_and(|f| f.kind == super::DeviceKind::Antigravity && f.code.is_none()));
        });
        cx.simulate_keystrokes("escape");
        assert!(ws.read_with(cx, |ws, _| ws.device_form.is_none()));
        ws.update_in(cx, |ws, window, cx| {
            assert!(ws.command("/login agy", false, window, cx));
            assert!(ws
                .device_form
                .as_ref()
                .is_some_and(|f| f.kind == super::DeviceKind::Antigravity));
            ws.close_device_form(window, cx);
            assert!(ws.command("/login google", false, window, cx));
            assert_eq!(ws.key_form.as_ref().unwrap().provider, "google");
            assert!(ws.key_form_view(super::Pal::new(true), cx).is_some());
            ws.close_key_form(window, cx);
        });
    }

    #[gpui_kit::test]
    async fn login_routes_sign_in_providers_to_the_browser_flow(cx: &mut TestAppContext) {
        let (ws, cx) = open(cx);
        ws.update_in(cx, |ws, window, cx| {
            with_presets(ws);
            // Sign-in only (no key env): straight to the browser sign-in.
            assert!(ws.command("/login devin-windsurf", false, window, cx));
            assert!(ws
                .device_form
                .as_ref()
                .is_some_and(|f| f.kind == super::DeviceKind::Login("devin-windsurf")));
            ws.close_device_form(window, cx);
            // A key also works: the key form offers the browser sign-in next to the field.
            assert!(ws.command("/login cursor", false, window, cx));
            assert_eq!(ws.key_form.as_ref().unwrap().provider, "cursor");
            ws.close_key_form(window, cx);
            ws.run(super::Act::SignIn("cursor".into()), window, cx);
            assert!(ws
                .device_form
                .as_ref()
                .is_some_and(|f| f.kind == super::DeviceKind::Login("cursor")));
        });
    }

    #[gpui_kit::test]
    async fn key_form_saves_replaces_and_logout_removes(cx: &mut TestAppContext) {
        let (ws, cx) = open(cx);
        ws.update_in(cx, |ws, window, cx| {
            with_presets(ws);
            let auth = greencode_core::home().join("auth.json");
            for key in ["k1", "k2"] {
                assert!(ws.command("/login openai", false, window, cx));
                ws.key_form
                    .as_ref()
                    .unwrap()
                    .field
                    .update(cx, |s, cx| s.set_value(key, window, cx));
                ws.submit_key(window, cx);
                assert!(ws.key_form.is_none());
                assert_eq!(ws.cfg.providers["openai"].api_key.as_deref(), Some(key));
                assert!(std::fs::read_to_string(&auth).unwrap().contains(key));
            }
            assert!(!std::fs::read_to_string(&auth).unwrap().contains("k1"));
            assert!(ws.command("/logout openai", false, window, cx));
            assert_eq!(ws.cfg.providers["openai"].api_key, None);
            assert!(!std::fs::read_to_string(&auth).unwrap().contains("k2"));
        });
    }

    #[gpui_kit::test]
    async fn switch_account_row_switches_the_active_credential(cx: &mut TestAppContext) {
        let (ws, cx) = open(cx);
        ws.update_in(cx, |ws, window, cx| {
            with_presets(ws);
            let cfg = std::sync::Arc::make_mut(&mut ws.cfg);
            greencode_core::auth::set_key(cfg, "openai", "k1").unwrap();
            ws.refresh_palette_rows();
            assert!(!ws
                .acts()
                .iter()
                .any(|(l, _)| l.starts_with("アカウント切替")));
            let cfg = std::sync::Arc::make_mut(&mut ws.cfg);
            greencode_core::auth::save_as(cfg, "openai", "work").unwrap();
            greencode_core::auth::set_key(cfg, "openai", "k2").unwrap();
            greencode_core::auth::save_as(cfg, "openai", "personal").unwrap();
            ws.refresh_palette_rows();
            let acts = ws.acts();
            assert!(acts
                .iter()
                .any(|(l, _)| l == "アカウントとして保存: openai…"));
            let (_, act) = acts
                .into_iter()
                .find(|(l, _)| l == "アカウント切替: openai → work")
                .expect("switch row");
            ws.run(act, window, cx);
            assert_eq!(ws.cfg.providers["openai"].api_key.as_deref(), Some("k1"));
            assert!(ws
                .login_rows()
                .iter()
                .any(|(l, _)| l.starts_with("openai") && l.contains("· work")));
        });
    }

    #[gpui_kit::test]
    async fn palette_reads_skill_files_on_open_not_during_filtering(cx: &mut TestAppContext) {
        let (ws, cx) = open(cx);
        let skill = greencode_core::home().join("skills/palette-snapshot/SKILL.md");
        std::fs::create_dir_all(skill.parent().unwrap()).unwrap();
        std::fs::write(
            &skill,
            "---\nname: palette-snapshot\ndescription: palette test\n---\nText\n",
        )
        .unwrap();
        ws.update_in(cx, |ws, window, cx| {
            ws.open_palette("スキル: palette-snapshot", window, cx);
            std::fs::remove_file(&skill).unwrap();
            assert!(ws
                .filtered_acts(cx)
                .iter()
                .any(|(label, _)| label == "スキル: palette-snapshot"));
            ws.toggle_palette(window, cx);
            ws.open_palette("スキル: palette-snapshot", window, cx);
            assert!(ws.filtered_acts(cx).is_empty());
        });
    }

    #[gpui_kit::test]
    async fn login_picker_scrolls_with_keyboard_and_resets(cx: &mut TestAppContext) {
        let (ws, cx) = open(cx);
        ws.update_in(cx, |ws, window, cx| {
            ws.command("/login", false, window, cx);
        });
        cx.run_until_parked();
        cx.simulate_keystrokes("up");
        cx.run_until_parked();
        ws.read_with(cx, |ws, cx| {
            assert_eq!(ws.palette_ix + 1, ws.filtered_acts(cx).len());
            assert!(ws.palette_scroll.offset().y < gpui_kit::px(0.));
            assert!(ws.palette_scroll.bottom_item() >= ws.palette_ix);
        });
        cx.simulate_input("anthropic");
        cx.run_until_parked();
        ws.read_with(cx, |ws, _| {
            assert_eq!(ws.palette_scroll.offset().y, gpui_kit::px(0.))
        });
        cx.simulate_keystrokes("escape");
        ws.update_in(cx, |ws, window, cx| {
            ws.command("/login", false, window, cx);
        });
        cx.run_until_parked();
        ws.read_with(cx, |ws, _| {
            assert_eq!(ws.palette_ix, 0);
            assert_eq!(ws.palette_scroll.offset().y, gpui_kit::px(0.));
        });
    }

    #[gpui_kit::test]
    async fn custom_provider_form_validates_saves_and_selects_the_model(cx: &mut TestAppContext) {
        let (ws, cx) = open(cx);
        cx.simulate_keystrokes("cmd-k");
        cx.simulate_input("カスタムプロバイダ");
        cx.simulate_keystrokes("enter");
        assert!(ws.read_with(cx, |ws, _| ws.provider_form.is_some()));
        cx.simulate_input("my llm");
        cx.simulate_keystrokes("enter");
        assert!(ws.read_with(cx, |ws, _| ws
            .provider_form
            .as_ref()
            .unwrap()
            .error
            .is_some()));
        cx.update(|window, cx| {
            ws.update(cx, |ws, cx| {
                let fields = ws.provider_form.as_ref().unwrap().fields.clone();
                for (field, value) in
                    fields
                        .iter()
                        .zip(["my-llm", "http://localhost:9/v1", "secret", "a, b"])
                {
                    field.update(cx, |s, cx| s.set_value(value, window, cx));
                }
            })
        });
        cx.simulate_keystrokes("enter");
        ws.read_with(cx, |ws, _| {
            assert!(ws.provider_form.is_none());
            assert_eq!(ws.thread().meta.model, "my-llm/a");
            assert!(ws.cfg.model_list().contains(&"my-llm/b".to_string()));
            assert!(matches!(ws.thread().items.last(), Some(Item::Notice(_))));
        });
        let saved = std::fs::read_to_string(greencode_core::home().join("config.toml")).unwrap();
        assert!(
            saved.contains("[providers.my-llm]") && saved.contains("api_key = \"secret\""),
            "{saved}"
        );
        cx.simulate_keystrokes("cmd-k");
        cx.simulate_input("カスタムプロバイダ");
        cx.simulate_keystrokes("enter");
        cx.simulate_keystrokes("escape");
        assert!(ws.read_with(cx, |ws, _| ws.provider_form.is_none()));
    }

    #[gpui_kit::test]
    async fn imported_session_opens_as_a_native_thread_with_its_transcript(
        cx: &mut TestAppContext,
    ) {
        let (ws, cx) = open(cx);
        let file = greencode_core::home().join("claude.jsonl");
        std::fs::create_dir_all(greencode_core::home()).unwrap();
        std::fs::write(&file, [
            json!({"type":"user","message":{"role":"user","content":"hello"}}).to_string(),
            json!({"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"hi"}]}}).to_string(),
        ].join("\n")).unwrap();
        let found = |path: std::path::PathBuf| greencode_core::import::Found {
            source: "Claude Code",
            title: "hello".into(),
            origin: greencode_core::import::Origin::File(path),
        };
        cx.update(|window, cx| {
            ws.update(cx, |ws, cx| {
                ws.run(super::Act::ImportSession(found(file.clone())), window, cx)
            })
        });
        ws.read_with(cx, |ws, _| {
            assert_eq!((ws.threads.len(), ws.active), (2, 0));
            assert_eq!((ws.thread().meta.title.as_str(), ws.thread().meta.backend.as_str()), ("hello", "native"));
            assert!(matches!(&ws.thread().items[..], [Item::User(u), Item::Assistant(a)] if u == "hello" && a == "hi"));
        });
        cx.update(|window, cx| {
            ws.update(cx, |ws, cx| {
                ws.run(
                    super::Act::ImportSession(found("/nonexistent.jsonl".into())),
                    window,
                    cx,
                )
            })
        });
        assert!(ws.read_with(cx, |ws, _| ws.threads.len() == 2
            && matches!(ws.thread().items.last(), Some(Item::Error(_)))));
    }

    #[gpui_kit::test]
    async fn import_command_opens_session_as_thread(cx: &mut TestAppContext) {
        let (ws, cx) = open(cx);
        let file = greencode_core::home().join("codex.jsonl");
        std::fs::create_dir_all(greencode_core::home()).unwrap();
        std::fs::write(&file, [
            json!({"type":"user","message":{"role":"user","content":"resume me"}}).to_string(),
            json!({"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"ready"}]}}).to_string(),
        ].join("\n")).unwrap();
        cx.update(|window, cx| {
            ws.update(cx, |ws, cx| {
                ws.command(&format!("/import {}", file.display()), true, window, cx)
            })
        });
        ws.read_with(cx, |ws, _| {
            assert_eq!(ws.threads.len(), 2);
            assert_eq!(ws.thread().meta.title, "resume me");
        });
    }

    #[gpui_kit::test]
    async fn fork_and_undo_commands_run_in_the_gui(cx: &mut TestAppContext) {
        let (ws, cx) = open(cx);
        ws.update_in(cx, |ws, window, cx| {
            // An empty thread cannot be forked: the GUI says so instead of sending "/fork" to the model.
            assert!(ws.command("/fork", false, window, cx));
            assert!(
                ws.command("/undo", false, window, cx) && ws.command("/retry", false, window, cx)
            );
            assert!(
                ws.thread()
                    .items
                    .iter()
                    .all(|i| !matches!(i, Item::User(_)))
                    && ws.thread().status == Status::Idle
            );
        });
    }

    #[gpui_kit::test]
    async fn slash_commands_and_model_picker(cx: &mut TestAppContext) {
        let (ws, cx) = open(cx);
        ws.update(cx, |ws, _| {
            let cfg = std::sync::Arc::make_mut(&mut ws.cfg);
            cfg.providers.insert("openai".into(), Default::default());
            ws.fetched_models = vec!["openai/alpha".into(), "openai/beta".into()];
        });
        let send = |text: &str, cx: &mut VisualTestContext| {
            cx.simulate_input(text);
            cx.simulate_keystrokes("enter");
        };
        // /model opens the dedicated picker; filtering and Enter select a fetched model without closing it.
        send("/model", cx);
        assert!(ws.read_with(cx, |ws, _| ws.model_picker.is_some()
            && ws.palette.is_none()));
        cx.simulate_input("openai/beta");
        cx.simulate_keystrokes("enter");
        assert!(ws.read_with(cx, |ws, _| ws.model_picker.is_some()
            && ws.thread().meta.model == "openai/beta"));
        cx.simulate_keystrokes("escape");
        send("/model openai/anything-typed", cx);
        assert_eq!(
            ws.read_with(cx, |ws, _| ws.thread().meta.model.clone()),
            "openai/anything-typed"
        );
        send("/model nope/x", cx);
        assert!(ws.read_with(cx, |ws, _| ws.thread().meta.model
            == "openai/anything-typed"
            && matches!(ws.thread().items.last(), Some(Item::Error(_)))));
        send("/mode full", cx);
        assert!(ws.read_with(cx, |ws, _| ws.thread().perms.mode()
            == greencode_core::permission::Mode::Full));
        for command in ["/status", "/help"] {
            send(command, cx);
            assert!(
                ws.read_with(cx, |ws, _| matches!(
                    ws.thread().items.last(),
                    Some(Item::Notice(_))
                ) && ws.thread().status == Status::Idle),
                "{command}"
            );
        }
        send("/diff", cx);
        assert!(ws.read_with(cx, |ws, _| ws.show_diff));
        cx.simulate_input("/st");
        cx.simulate_keystrokes("tab");
        assert_eq!(
            ws.read_with(cx, |ws, cx| ws.input.read(cx).value().to_string()),
            "/status "
        );
        cx.simulate_keystrokes("cmd-a backspace");
        send("/new", cx);
        assert_eq!(ws.read_with(cx, |ws, _| ws.threads.len()), 2);
    }

    #[gpui_kit::test]
    async fn learn_command_starts_approval_prompt(cx: &mut TestAppContext) {
        let (ws, cx) = open(cx);
        cx.simulate_input("/learn use cargo test");
        cx.simulate_keystrokes("enter");
        assert!(ws.read_with(cx, |ws, _| ws
            .thread()
            .items
            .iter()
            .any(|item| matches!(item, Item::User(text) if text.contains("ext learn")))));
    }

    #[gpui_kit::test]
    async fn model_shortcut_and_external_selection_wait_for_confirmation(cx: &mut TestAppContext) {
        let (ws, cx) = open(cx);
        cx.simulate_keystrokes("cmd-shift-m");
        assert!(ws.read_with(cx, |ws, _| ws.model_picker.is_some()
            && ws.palette.is_none()));
        cx.simulate_keystrokes("escape");
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        ws.update(cx, |ws, _| {
            ws.threads[0].meta.backend = "pi".into();
            ws.threads[0].meta.model = "old/model".into();
            ws.threads[0].cmd = Some(tx);
        });
        cx.update(|window, cx| {
            ws.update(cx, |ws, cx| {
                ws.run(super::Act::Model("new/model".into()), window, cx)
            })
        });
        assert!(matches!(rx.try_recv(), Ok(greencode_core::Command::Model(m)) if m == "new/model"));
        assert_eq!(
            ws.read_with(cx, |ws, _| ws.thread().meta.model.clone()),
            "old/model"
        );
        ws.update(cx, |ws, cx| {
            let id = ws.thread().meta.id.clone();
            ws.on_event(
                &id,
                Event::Remote {
                    command: "get_state".into(),
                    data: json!({"model":{"provider":"new","id":"model"}}),
                },
                cx,
            );
        });
        assert_eq!(
            ws.read_with(cx, |ws, _| ws.thread().meta.model.clone()),
            "new/model"
        );
    }

    #[gpui_kit::test]
    async fn undo_fork_export_plan_thinking_cost_and_search(cx: &mut TestAppContext) {
        use greencode_core::{permission::Mode, session::Session, Usage};
        let (ws, cx) = open(cx);
        let meta = ws.read_with(cx, |ws, _| ws.thread().meta.clone());
        let file = greencode_core::home().join("edited.txt");
        let session = Session::create(&meta).unwrap();
        for line in [
            json!({"role":"user","content":[{"type":"text","text":"first unicorn"}]}),
            json!({"role":"assistant","content":[{"type":"text","text":"ok"}]}),
            json!({"role":"user","content":[{"type":"text","text":"second"}]}),
            json!({"checkpoint":{"path":file,"before":"old"}}),
            json!({"role":"assistant","content":[{"type":"text","text":"done"}]}),
        ] {
            session.append(&line).unwrap();
        }
        std::fs::write(&file, "new").unwrap();

        // Undo drops the last turn, restores the file and hands the prompt back for editing.
        cx.update(|window, cx| ws.update(cx, |ws, cx| ws.run(super::Act::Undo, window, cx)));
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "old");
        ws.read_with(cx, |ws, cx| {
            assert_eq!(ws.input.read(cx).value().to_string(), "second");
            assert!(matches!(&ws.thread().items[..], [Item::User(u), Item::Assistant(a)] if u == "first unicorn" && a == "ok"));
        });
        cx.simulate_keystrokes("cmd-a backspace");

        // Rewinding two turns restores files changed in either and hands back the older prompt.
        let forked = Session::fork(&meta.id).unwrap();
        let session = Session::open(&forked.id).unwrap();
        for line in [
            json!({"role":"user","content":[{"type":"text","text":"second"}]}),
            json!({"checkpoint":{"path":file,"before":"old"}}),
            json!({"role":"user","content":[{"type":"text","text":"third"}]}),
            json!({"checkpoint":{"path":file,"before":"mid"}}),
        ] {
            session.append(&line).unwrap();
        }
        std::fs::write(&file, "new").unwrap();
        // Per-turn diff: "second" took the file from old to mid, "third" from mid to what is on disk.
        let second = Session::turn_diff(&forked.id, 1).unwrap();
        assert!(
            second.contains("-old") && second.contains("+mid") && !second.contains("+new"),
            "{second}"
        );
        let third = Session::turn_diff(&forked.id, 0).unwrap();
        assert!(third.contains("-mid") && third.contains("+new"), "{third}");
        assert_eq!(Session::turn_diff(&forked.id, 2).unwrap(), "");
        assert_eq!(Session::rewind(&forked.id, 5).unwrap(), None);
        assert_eq!(
            Session::rewind(&forked.id, 1).unwrap().as_deref(),
            Some("second")
        );
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "old");
        assert_eq!(Session::load(&forked.id).unwrap().1.len(), 2);

        // A side chat is a copy titled after the question; bringing it back quotes its answer into the parent.
        cx.update(|window, cx| {
            ws.update(cx, |ws, cx| {
                ws.run(super::Act::Side("why?".into()), window, cx)
            })
        });
        ws.read_with(cx, |ws, _| {
            assert!(ws.thread().meta.title == "side: why?" && ws.thread().meta.id != meta.id);
            assert!(matches!(&ws.thread().items[..], [Item::User(_), Item::Assistant(_), Item::User(q)] if q == "why?"), "{}", ws.thread().items.len());
            assert_eq!(ws.ui["side"][ws.thread().meta.id.as_str()], meta.id.as_str());
        });
        cx.update(|window, cx| {
            ws.update(cx, |ws, cx| {
                ws.threads[ws.active]
                    .items
                    .push(Item::Assistant("because".into()));
                ws.run(super::Act::BringBack, window, cx);
            })
        });
        ws.read_with(cx, |ws, cx| {
            assert_eq!(ws.thread().meta.id, meta.id);
            assert_eq!(
                ws.input.read(cx).value().to_string(),
                "side: why?\n> because\n\n"
            );
        });
        cx.simulate_keystrokes("cmd-a backspace");

        cx.update(|window, cx| ws.update(cx, |ws, cx| ws.run(super::Act::Export, window, cx)));
        let exported = std::fs::read_to_string(
            greencode_core::home()
                .join("exports")
                .join(format!("{}.md", meta.id)),
        )
        .unwrap();
        assert!(exported.contains("## User\n\nfirst unicorn"), "{exported}");

        // `?text` in the palette searches conversation contents.
        cx.simulate_keystrokes("cmd-k");
        cx.simulate_input("?UNICORN");
        // Search is debounced and runs in the background.
        cx.executor()
            .advance_clock(std::time::Duration::from_millis(200));
        cx.run_until_parked();
        // The thread and its side chat copy both contain the text.
        assert_eq!(ws.read_with(cx, |ws, cx| ws.filtered_acts(cx).len()), 2);
        cx.simulate_keystrokes("escape");

        cx.simulate_input("/plan");
        cx.simulate_keystrokes("enter");
        assert!(ws.read_with(cx, |ws, _| ws.thread().perms.mode() == Mode::Plan));

        ws.update(cx, |ws, cx| {
            std::sync::Arc::make_mut(&mut ws.cfg).prices.insert(meta.model.clone(), vec![10.0, 20.0]);
            let n = ws.thread().items.len();
            ws.on_event(&meta.id, Event::Text("answer".into()), cx);
            ws.on_event(&meta.id, Event::Thinking("because".into()), cx);
            ws.on_event(&meta.id, Event::Usage(Usage { input: 100_000, output: 50_000, ..Default::default() }), cx);
            assert!(matches!(&ws.thread().items[n..], [Item::Thinking(t), Item::Assistant(a)] if t == "because" && a == "answer"));
            assert!((ws.thread().cost - 2.0).abs() < 1e-9);
            ws.thread().perms.always("bash:ls".into());
        });
        cx.update(|window, cx| {
            ws.update(cx, |ws, cx| {
                ws.run(super::Act::Revoke("bash:ls".into()), window, cx)
            })
        });
        assert!(ws.read_with(cx, |ws, _| !ws
            .thread()
            .perms
            .allowed()
            .contains(&"bash:ls".to_string())));

        cx.update(|window, cx| ws.update(cx, |ws, cx| ws.run(super::Act::Fork, window, cx)));
        ws.read_with(cx, |ws, _| {
            assert_eq!((ws.threads.len(), ws.active), (3, 0));
            assert!(
                ws.thread().meta.id != meta.id
                    && ws.thread().meta.title.ends_with("(fork)")
                    && ws.thread().items.len() == 2
            );
        });
    }

    #[gpui_kit::test]
    async fn discovered_models_are_available_from_other_threads(cx: &mut TestAppContext) {
        let (ws, cx) = open(cx);
        ws.update(cx, |ws, cx| {
            ws.threads[0].meta.backend = "pi".into();
            let id = ws.thread().meta.id.clone();
            ws.on_event(
                &id,
                Event::Remote {
                    command: "get_available_models".into(),
                    data: json!({"models":[{"provider":"p","id":"m"}]}),
                },
                cx,
            );
            ws.threads[0].meta.backend = "native".into();
            assert!(ws
                .acts()
                .iter()
                .any(|(label, _)| label == "モデル: pi · p/m"));
            ws.threads[0].meta.backend = "acp".into();
            ws.on_event(
                &id,
                Event::Remote {
                    command: "acp_state".into(),
                    data: json!({"models":{"availableModels":[{"modelId":"a"}]}}),
                },
                cx,
            );
            ws.threads[0].meta.backend = "native".into();
            assert!(ws
                .acts()
                .iter()
                .any(|(label, _)| label == "モデル: acp · a"));
        });
    }

    #[gpui_kit::test]
    async fn backend_change_on_a_started_thread_opens_a_new_thread(cx: &mut TestAppContext) {
        let (ws, cx) = open(cx);
        ws.update(cx, |ws, cx| {
            let id = ws.thread().meta.id.clone();
            ws.on_event(&id, Event::Text("hi".into()), cx);
            ws.on_event(&id, Event::Done { waiting_for_delegates: false }, cx);
        });
        cx.update(|window, cx| {
            ws.update(cx, |ws, cx| {
                ws.run(super::Act::Backend("pi".into()), window, cx)
            })
        });
        ws.read_with(cx, |ws, _| {
            assert_eq!(
                (ws.threads.len(), ws.thread().meta.backend.as_str()),
                (2, "pi")
            );
            assert!(!ws
                .thread()
                .items
                .iter()
                .any(|i| matches!(i, Item::Error(_))));
        });
    }

    #[gpui_kit::test]
    async fn one_picker_lists_every_backends_models_and_selects_backend_with_model(
        cx: &mut TestAppContext,
    ) {
        let (ws, cx) = open(cx);
        ws.update(cx, |ws, _| {
            ws.backend_models.insert(
                "pi".into(),
                vec!["openai/gpt-x".into(), "anthropic/claude-y".into()],
            );
            ws.project_models
                .insert(ws.thread().project(), vec!["zen/big".into()]);
        });
        // A fresh thread takes the backend and model in place.
        cx.simulate_keystrokes("cmd-k");
        cx.simulate_input("モデル: pi");
        assert_eq!(
            ws.read_with(cx, |ws, cx| ws
                .filtered_acts(cx)
                .into_iter()
                .map(|a| a.0)
                .collect::<Vec<_>>()),
            [
                "モデル: pi · openai/gpt-x",
                "モデル: pi · anthropic/claude-y"
            ]
        );
        cx.simulate_keystrokes("down enter");
        ws.read_with(cx, |ws, _| {
            assert_eq!(
                (
                    ws.threads.len(),
                    ws.thread().meta.backend.as_str(),
                    ws.thread().meta.model.as_str()
                ),
                (1, "pi", "anthropic/claude-y")
            );
        });
        // Same backend: only the model changes. Native models stay reachable from an external thread.
        cx.update(|window, cx| {
            ws.update(cx, |ws, cx| {
                ws.run(
                    super::Act::BackendModel("pi".into(), "openai/gpt-x".into()),
                    window,
                    cx,
                )
            })
        });
        assert!(
            ws.read_with(cx, |ws, cx| ws.thread().meta.model == "openai/gpt-x" && {
                let _ = cx;
                ws.acts()
                    .iter()
                    .any(|a| a.0 == "モデル: native · anthropic/claude-opus-5-5")
            })
        );
        // A thread with history keeps its backend; the other backend's model opens a new thread in the same project.
        ws.update(cx, |ws, cx| {
            ws.threads[0].meta.cwd = "/tmp/example-worktree".into();
            ws.threads[0].meta.repo = Some("/tmp/example-repo".into());
            let id = ws.thread().meta.id.clone();
            ws.on_event(&id, Event::Text("hi".into()), cx);
            ws.on_event(&id, Event::Done { waiting_for_delegates: false }, cx);
        });
        cx.simulate_input("unsent request");
        cx.update(|window, cx| {
            ws.update(cx, |ws, cx| {
                ws.run(
                    super::Act::BackendModel("opencode".into(), "zen/big".into()),
                    window,
                    cx,
                )
            })
        });
        ws.read_with(cx, |ws, _| {
            assert_eq!((ws.threads.len(), ws.active), (2, 0));
            assert_eq!((ws.thread().meta.backend.as_str(), ws.thread().meta.model.as_str()), ("opencode", "zen/big"));
            assert_eq!(ws.threads[1].meta.backend, "pi");
            assert_eq!(ws.thread().meta.cwd, std::path::PathBuf::from("/tmp/example-worktree"));
            assert_eq!(ws.thread().meta.repo, Some(std::path::PathBuf::from("/tmp/example-repo")));
            assert_eq!(ws.thread().draft, "unsent request");
            assert!(matches!(ws.thread().items.last(), Some(Item::Notice(text)) if text.contains("引き継げませんでした")));
        });
        cx.update(|window, cx| {
            ws.update(cx, |ws, cx| {
                ws.run(
                    super::Act::BackendModel("pi".into(), "other".into()),
                    window,
                    cx,
                )
            })
        });
        assert_eq!(ws.read_with(cx, |ws, _| ws.threads.len()), 2);
        assert_eq!(greencode_core::remote_models("acp_state", &json!({"models":{"availableModels":[{"modelId":"a"}]},"configOptions":[{"category":"model","options":[{"value":"b"}]}]})).unwrap(), ["b"]);
        assert_eq!(
            greencode_core::remote_models(
                "get_available_models",
                &json!({"models":[{"provider":"p","id":"m"}]})
            )
            .unwrap(),
            ["p/m"]
        );
    }

    #[gpui_kit::test]
    async fn external_commands_accept_string_and_object_entries(cx: &mut TestAppContext) {
        let (ws, cx) = open(cx);
        ws.update(cx, |ws, cx| {
            let id = ws.thread().meta.id.clone();
            ws.threads[0].meta.backend = "pi".into();
            ws.on_event(
                &id,
                Event::Remote {
                    command: "get_commands".into(),
                    data: json!({"commands":["/alpha", {"name":"beta"}, {"name":"alpha"}]}),
                },
                cx,
            );
            assert_eq!(ws.thread().commands, ["alpha", "beta"]);
            assert!(ws.commands.iter().any(|command| command == "alpha"));
        });
    }

    #[gpui_kit::test]
    async fn harness_commands_reach_backend_without_gui_interception(cx: &mut TestAppContext) {
        let (ws, cx) = open(cx);
        for backend in ["opencode", "claude", "pi", "omp"] {
            ws.update(cx, |ws, _| {
                let t = &mut ws.threads[0];
                t.meta.backend = backend.into();
                t.commands = vec!["model".into(), "review".into()];
                t.status = Status::Running;
                t.queue.clear();
                ws.ui["follow_up_steer"] = json!(false);
            });
            cx.simulate_input("/model");
            cx.simulate_keystrokes("enter");
            assert_eq!(
                ws.read_with(cx, |ws, _| ws.thread().queue.back().cloned()),
                Some("/model".into())
            );
        }
    }

    #[gpui_kit::test]
    async fn settings_toggle_ext_tools_and_compaction(cx: &mut TestAppContext) {
        let (ws, cx) = open(cx);
        ws.update_in(cx, |ws, window, cx| {
            ws.run(super::Act::ToggleExt("rewind"), window, cx);
            assert_eq!(ws.cfg.ext_off, vec!["rewind".to_string()]);
            assert_eq!(ws.ui["ext_off"], json!(["rewind"]));
            ws.run(super::Act::ToggleExt("rewind"), window, cx);
            assert!(ws.cfg.ext_off.is_empty());
            ws.run(super::Act::CycleCompaction, window, cx);
            assert!(ws.cfg.compaction_enabled && ws.cfg.compaction == "snap");
            ws.run(super::Act::CycleCompaction, window, cx);
            assert!(!ws.cfg.compaction_enabled && ws.ui["compaction"] == "off");
            ws.run(super::Act::CycleCompaction, window, cx);
            assert!(ws.cfg.compaction_enabled && ws.cfg.compaction == "summary");
        });
    }

    #[gpui_kit::test]
    async fn goal_order_goal_clear_and_queued_confirm_dialogs(cx: &mut TestAppContext) {
        let (ws, cx) = open(cx);
        let id = ws.read_with(cx, |ws, _| ws.thread().meta.id.clone());
        // `/goal` during a turn goes next; that turn's end does not put a continuation before it.
        ws.update(cx, |ws, _| {
            ws.threads[0].status = Status::Running;
            ws.ui["follow_up_steer"] = json!(false);
        });
        cx.simulate_input("/goal tests pass");
        cx.simulate_keystrokes("enter");
        ws.update(cx, |ws, cx| {
            ws.threads[0].queue_held = false;
            ws.on_event(&id, Event::Done { waiting_for_delegates: false }, cx);
            let t = ws.thread();
            assert!(
                !t.queue.iter().any(|q| q.starts_with(super::GOAL_CONTINUE)),
                "{:?}",
                t.queue
            );
            assert!(t
                .items
                .iter()
                .any(|i| matches!(i, Item::User(u) if u.starts_with("tests pass"))));
        });
        // `/goal clear` also drops a continuation already queued.
        ws.update(cx, |ws, _| {
            let t = &mut ws.threads[0];
            t.queue.push_back(format!("{}x", super::GOAL_CONTINUE));
            t.goal = Some(("x".into(), 1));
        });
        ws.update_in(cx, |ws, window, cx| {
            assert!(ws.command("/goal clear", false, window, cx))
        });
        ws.read_with(cx, |ws, _| {
            assert!(ws.thread().goal.is_none() && ws.thread().queue.is_empty())
        });
        // Two confirms arrive together: both are kept, and Enter on an unrelated draft answers neither.
        let (a, mut ra) = oneshot::channel();
        let (b, mut rb) = oneshot::channel();
        ws.update(cx, |ws, cx| {
            ws.on_event(
                &id,
                Event::Dialog {
                    request: json!({"method":"confirm","title":"one"}),
                    reply: a,
                },
                cx,
            );
            ws.on_event(
                &id,
                Event::Dialog {
                    request: json!({"method":"confirm","title":"two"}),
                    reply: b,
                },
                cx,
            );
        });
        cx.simulate_input("draft");
        cx.simulate_keystrokes("enter");
        ws.read_with(cx, |ws, cx| {
            assert_eq!(ws.thread().dialog.as_ref().unwrap().0["title"], "one");
            assert_eq!(ws.input.read(cx).value().to_string(), "draft");
        });
        assert!(ra.try_recv().is_err());
        ws.update(cx, |ws, cx| {
            ws.answer_dialog(json!({"confirmed": true}), cx)
        });
        assert_eq!(ra.try_recv().unwrap(), json!({"confirmed": true}));
        ws.read_with(cx, |ws, _| {
            assert_eq!(ws.thread().dialog.as_ref().unwrap().0["title"], "two")
        });
        assert!(rb.try_recv().is_err());
    }

    #[gpui_kit::test]
    async fn goal_runs_in_pi_and_omp_threads(cx: &mut TestAppContext) {
        let (ws, cx) = open(cx);
        for backend in ["pi", "omp"] {
            ws.update(cx, |ws, _| {
                let t = &mut ws.threads[0];
                t.meta.backend = backend.into();
                t.commands = vec!["model".into()];
                t.status = Status::Running;
                t.queue.clear();
                t.goal = None;
                ws.ui["follow_up_steer"] = json!(false);
            });
            cx.simulate_input("/goal tests pass");
            cx.simulate_keystrokes("enter");
            ws.read_with(cx, |ws, _| {
                assert_eq!(
                    ws.thread().goal,
                    Some(("tests pass".into(), 0)),
                    "{backend}"
                );
                assert!(
                    ws.thread()
                        .queue
                        .back()
                        .is_some_and(|q| q.starts_with("tests pass") && q.contains("GOAL MET")),
                    "{backend}"
                );
            });
        }
    }

    #[gpui_kit::test]
    async fn typing_a_slash_starts_command_discovery_without_a_prompt(cx: &mut TestAppContext) {
        let (ws, cx) = open(cx);
        ws.update(cx, |ws, _| {
            ws.threads[0].meta.backend = "pi".into();
            ws.agent_spawner = silent_agent;
        });
        cx.simulate_input("/");
        ws.read_with(cx, |ws, _| {
            let t = ws.thread();
            assert!(t.cmd.is_some() && t.warm && t.warmups == 1);
            assert!(t.items.is_empty() && t.last_prompt.is_none());
        });
        // A thread only warmed up for its commands is still unstarted: it may switch backend in place.
        cx.update(|window, cx| {
            ws.update(cx, |ws, cx| {
                ws.run(super::Act::Backend("acp".into()), window, cx)
            })
        });
        ws.read_with(cx, |ws, _| {
            assert_eq!(ws.threads.len(), 1);
            assert!(ws.thread().cmd.is_none() && !ws.thread().warm && ws.thread().warmups == 0);
        });
    }

    #[gpui_kit::test]
    async fn discovery_ends_on_the_agents_done_and_leaves_the_thread_unstarted(
        cx: &mut TestAppContext,
    ) {
        let (ws, cx) = open(cx);
        ws.update(cx, |ws, _| ws.threads[0].meta.backend = "pi".into());
        cx.simulate_input("/");
        ws.read_with(cx, |ws, _| {
            let t = ws.thread();
            assert!(
                t.cmd.is_some()
                    && t.warm
                    && t.warmups == 0
                    && t.items.is_empty()
                    && t.last_prompt.is_none()
            );
        });
    }

    #[gpui_kit::test]
    async fn first_command_the_gui_does_not_own_is_not_held_for_discovery(cx: &mut TestAppContext) {
        let (ws, cx) = open(cx);
        ws.update(cx, |ws, _| {
            let t = &mut ws.threads[0];
            (t.meta.backend, t.status) = ("pi".into(), Status::Running);
            ws.agent_spawner = silent_agent;
            ws.ui["follow_up_steer"] = json!(false);
        });
        cx.simulate_input("/review");
        cx.simulate_keystrokes("enter");
        ws.read_with(cx, |ws, _| {
            assert!(ws.thread().warmups == 1 && ws.thread().deferred.is_none());
            assert_eq!(ws.thread().queue.back().cloned(), Some("/review".into()));
        });
    }

    #[gpui_kit::test]
    async fn timed_out_command_waits_for_discovery_on_retry(cx: &mut TestAppContext) {
        let (ws, cx) = open(cx);
        ws.update(cx, |ws, _| {
            let t = &mut ws.threads[0];
            (t.meta.backend, t.status) = ("pi".into(), Status::Running);
            ws.agent_spawner = silent_agent;
            ws.ui["follow_up_steer"] = json!(false);
        });
        cx.simulate_input("/model");
        cx.simulate_keystrokes("enter");
        assert!(ws.read_with(cx, |ws, _| ws.thread().deferred.is_some()));
        cx.executor()
            .advance_clock(std::time::Duration::from_secs(11));
        cx.run_until_parked();
        ws.read_with(cx, |ws, cx| {
            assert!(ws.thread().deferred.is_none());
            assert_eq!(ws.input.read(cx).value().to_string(), "/model");
        });
        cx.simulate_keystrokes("enter");
        ws.read_with(cx, |ws, _| {
            assert!(ws.thread().deferred.is_some() && ws.model_picker.is_none())
        });
    }

    #[gpui_kit::test]
    async fn discovery_done_does_not_end_a_running_prompt(cx: &mut TestAppContext) {
        let (ws, cx) = open(cx);
        ws.update(cx, |ws, cx| {
            let id = ws.thread().meta.id.clone();
            ws.threads[0].status = Status::Running;
            ws.threads[0].warmups = 1;
            ws.on_event(&id, Event::Done { waiting_for_delegates: false }, cx);
            assert!(ws.thread().status == Status::Running);
            ws.on_event(&id, Event::Done { waiting_for_delegates: false }, cx);
            assert!(ws.thread().status == Status::Idle);
        });
    }

    #[gpui_kit::test]
    async fn native_discovered_commands_join_the_builtin_completions(cx: &mut TestAppContext) {
        let (ws, cx) = open(cx);
        ws.update(cx, |ws, cx| {
            let id = ws.thread().meta.id.clone();
            ws.on_event(
                &id,
                Event::Remote {
                    command: "get_commands".into(),
                    data: json!({"commands":[{"name":"deploy"}, "model"]}),
                },
                cx,
            );
            let has = |name: &str| ws.commands.iter().filter(|c| *c == name).count();
            assert_eq!((has("deploy"), has("model"), has("review")), (1, 1, 1));
        });
    }

    #[gpui_kit::test]
    async fn first_slash_command_waits_for_the_backends_catalog(cx: &mut TestAppContext) {
        let (ws, cx) = open(cx);
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        ws.update(cx, |ws, _| {
            let t = &mut ws.threads[0];
            (t.meta.backend, t.cmd, t.warm, t.warmups, t.status) =
                ("pi".into(), Some(tx), true, 1, Status::Running);
            ws.ui["follow_up_steer"] = json!(false);
        });
        cx.simulate_input("/model");
        cx.simulate_keystrokes("enter");
        ws.read_with(cx, |ws, cx| {
            // Held, not swallowed by the GUI model picker and not sent yet.
            assert!(
                ws.thread().deferred.is_some()
                    && ws.thread().queue.is_empty()
                    && ws.palette.is_none()
            );
            assert!(ws.input.read(cx).value().is_empty());
        });
        ws.update(cx, |ws, cx| {
            let id = ws.thread().meta.id.clone();
            ws.on_event(
                &id,
                Event::Remote {
                    command: "get_commands".into(),
                    data: json!({"commands":["model"]}),
                },
                cx,
            );
            ws.on_event(&id, Event::Done { waiting_for_delegates: false }, cx);
            assert!(ws.thread().deferred.is_none());
        });
        cx.run_until_parked();
        // The backend advertises /model, so it reaches the backend instead of the GUI picker.
        ws.read_with(cx, |ws, _| {
            assert_eq!(ws.thread().queue.back().cloned(), Some("/model".into()));
            assert!(ws.palette.is_none() && ws.released.is_empty());
        });
    }

    #[gpui_kit::test]
    async fn held_slash_command_returns_to_the_composer_when_the_backend_changes(
        cx: &mut TestAppContext,
    ) {
        let (ws, cx) = open(cx);
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        ws.update(cx, |ws, _| {
            let t = &mut ws.threads[0];
            (t.meta.backend, t.cmd, t.warm, t.warmups) = ("pi".into(), Some(tx), true, 1);
            t.deferred = Some(("/model".into(), 1, "pi".into()));
        });
        // A timer of another deferral does nothing.
        ws.update(cx, |ws, cx| {
            let id = ws.thread().meta.id.clone();
            ws.release_deferred(&id, Some(99), cx);
        });
        assert!(ws.read_with(cx, |ws, _| ws.thread().deferred.is_some()));
        cx.update(|window, cx| {
            ws.update(cx, |ws, cx| {
                ws.run(super::Act::Backend("acp".into()), window, cx)
            })
        });
        cx.run_until_parked();
        ws.read_with(cx, |ws, cx| {
            assert!(ws.thread().deferred.is_none() && ws.thread().cmd.is_none());
            assert_eq!(ws.input.read(cx).value().to_string(), "/model");
        });
    }

    #[gpui_kit::test]
    async fn discovery_timeout_restores_command_without_dispatch(cx: &mut TestAppContext) {
        let (ws, cx) = open(cx);
        ws.update(cx, |ws, cx| {
            let id = ws.thread().meta.id.clone();
            ws.threads[0].deferred = Some(("/model".into(), 1, "pi".into()));
            ws.release_deferred(&id, Some(1), cx);
            assert!(ws.released.is_empty());
        });
        cx.run_until_parked();
        assert_eq!(
            ws.read_with(cx, |ws, cx| ws.input.read(cx).value().to_string()),
            "/model"
        );
    }

    #[gpui_kit::test]
    async fn released_slash_command_is_not_sent_to_a_different_backend(cx: &mut TestAppContext) {
        let (ws, cx) = open(cx);
        ws.update(cx, |ws, cx| {
            let id = ws.thread().meta.id.clone();
            ws.threads[0].status = Status::Running;
            ws.threads[0].meta.backend = "opencode".into();
            ws.threads[0].deferred = Some(("/review".into(), 1, "pi".into()));
            ws.release_deferred(&id, None, cx);
        });
        cx.run_until_parked();
        ws.read_with(cx, |ws, cx| {
            assert!(ws.thread().queue.is_empty() && ws.thread().deferred.is_none());
            assert_eq!(ws.input.read(cx).value().to_string(), "/review");
        });
    }

    #[gpui_kit::test]
    async fn warm_agent_runtime_does_not_follow_the_thread_to_another_backend(
        cx: &mut TestAppContext,
    ) {
        use greencode_core::session::Session;
        let (ws, cx) = open(cx);
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let old = ws.update(cx, |ws, _| {
            let t = &mut ws.threads[0];
            (t.meta.backend, t.cmd, t.warm, t.warmups) = ("pi".into(), Some(tx), true, 0);
            t.draft = "keep me".into();
            // What Pi's `get_state` / ACP session start write before any prompt.
            Session::save_runtime(&t.meta.id, &json!({"backend":"pi","sessionFile":"/tmp/x"}))
                .unwrap();
            t.meta.id.clone()
        });
        cx.update(|window, cx| {
            ws.update(cx, |ws, cx| {
                ws.run(super::Act::Backend("acp".into()), window, cx)
            })
        });
        ws.read_with(cx, |ws, _| {
            let t = ws.thread();
            assert_ne!(t.meta.id, old);
            assert_eq!(
                (t.meta.backend.as_str(), t.cmd.is_none(), t.draft.as_str()),
                ("acp", true, "keep me")
            );
            // The old backend's state is gone and the new id has none, so the next backend starts clean.
            assert!(Session::runtime(&old).unwrap().is_null());
            assert!(Session::runtime_for_backend(&t.meta.id, "acp")
                .unwrap()
                .is_null());
        });
    }

    #[gpui_kit::test]
    async fn copy_text_event_writes_the_clipboard(cx: &mut TestAppContext) {
        let (ws, cx) = open(cx);
        ws.update(cx, |ws, cx| {
            let id = ws.thread().meta.id.clone();
            ws.on_event(
                &id,
                Event::Remote {
                    command: "copy_text".into(),
                    data: json!({"text":"last reply"}),
                },
                cx,
            );
            assert_eq!(
                cx.read_from_clipboard().and_then(|c| c.text()),
                Some("last reply".into())
            );
            assert!(matches!(ws.thread().items.last(), Some(Item::Notice(_))));
            ws.on_event(
                &id,
                Event::Remote {
                    command: "copy_text".into(),
                    data: json!({"text":""}),
                },
                cx,
            );
            assert!(matches!(ws.thread().items.last(), Some(Item::Error(_))));
        });
    }

    #[gpui_kit::test]
    async fn extension_editor_text_never_overwrites_a_draft(cx: &mut TestAppContext) {
        let (ws, cx) = open(cx);
        ws.update(cx, |ws, cx| {
            let id = ws.thread().meta.id.clone();
            ws.on_event(
                &id,
                Event::Remote {
                    command: "set_editor_text".into(),
                    data: json!({"text":"prefilled"}),
                },
                cx,
            );
            cx.notify();
        });
        cx.run_until_parked();
        assert_eq!(
            ws.read_with(cx, |ws, cx| ws.input.read(cx).value().to_string()),
            "prefilled"
        );
        cx.simulate_input(" mine");
        ws.update(cx, |ws, cx| {
            let id = ws.thread().meta.id.clone();
            ws.on_event(
                &id,
                Event::Remote {
                    command: "set_editor_text".into(),
                    data: json!({"text":"more"}),
                },
                cx,
            );
            cx.notify();
        });
        cx.run_until_parked();
        assert_eq!(
            ws.read_with(cx, |ws, cx| ws.input.read(cx).value().to_string()),
            "prefilled mine\nmore"
        );
        ws.update(cx, |ws, cx| {
            let id = ws.thread().meta.id.clone();
            ws.on_event(
                &id,
                Event::Remote {
                    command: "set_editor_text".into(),
                    data: json!({"text":"more"}),
                },
                cx,
            );
            cx.notify();
        });
        cx.run_until_parked();
        assert_eq!(
            ws.read_with(cx, |ws, cx| ws.input.read(cx).value().to_string()),
            "prefilled mine\nmore\nmore"
        );
        ws.update(cx, |ws, cx| {
            let id = ws.thread().meta.id.clone();
            for text in ["first", "second"] {
                ws.on_event(
                    &id,
                    Event::Remote {
                        command: "set_editor_text".into(),
                        data: json!({"text":text}),
                    },
                    cx,
                );
            }
        });
        cx.run_until_parked();
        assert_eq!(
            ws.read_with(cx, |ws, cx| ws.input.read(cx).value().to_string()),
            "prefilled mine\nmore\nmore\nfirst\nsecond"
        );
    }

    #[gpui_kit::test]
    async fn extension_status_and_widgets_are_tracked_per_thread(cx: &mut TestAppContext) {
        let (ws, cx) = open(cx);
        ws.update(cx, |ws, cx| {
            let id = ws.thread().meta.id.clone();
            ws.on_event(&id, Event::Remote { command: "setStatus".into(), data: json!({"statusKey":"k","statusText":"\u{1b}[38;2;1;2;3mbusy\u{1b}[0m 日本語"}) }, cx);
            ws.on_event(&id, Event::Remote { command: "setWidget".into(), data: json!({"widgetKey":"w","widgetLines":["\u{1b}[31ma\u{1b}[0m","b"]}) }, cx);
            assert_eq!(ws.thread().statuses["k"], "busy 日本語");
            assert_eq!(ws.thread().widgets["w"], ["a", "b"]);
            ws.on_event(&id, Event::Remote { command: "setStatus".into(), data: json!({"statusKey":"k"}) }, cx);
            ws.on_event(&id, Event::Remote { command: "setWidget".into(), data: json!({"widgetKey":"w"}) }, cx);
            assert!(ws.thread().statuses.is_empty() && ws.thread().widgets.is_empty());
        });
    }

    #[gpui_kit::test]
    async fn mapped_new_opens_a_thread_but_an_extension_new_reaches_the_backend(
        cx: &mut TestAppContext,
    ) {
        let (ws, cx) = open(cx);
        ws.update(cx, |ws, cx| {
            let id = ws.thread().meta.id.clone();
            ws.threads[0].meta.backend = "pi".into();
            ws.on_event(&id, Event::Remote { command: "get_commands".into(), data: json!({"commands":[{"name":"new","source":"greencode"},{"name":"copy","source":"greencode"}]}) }, cx);
            assert!(ws.thread().commands.is_empty());
            assert!(ws.commands.iter().any(|c| c == "new") && ws.commands.iter().any(|c| c == "copy"));
        });
        cx.update(|window, cx| {
            ws.update(cx, |ws, cx| assert!(ws.command("/new", false, window, cx)))
        });
        assert_eq!(ws.read_with(cx, |ws, _| ws.threads.len()), 2);
        ws.update(cx, |ws, cx| {
            let id = ws.thread().meta.id.clone();
            ws.threads[ws.active].meta.backend = "pi".into();
            ws.on_event(
                &id,
                Event::Remote {
                    command: "get_commands".into(),
                    data: json!({"commands":[{"name":"new","source":"extension"}]}),
                },
                cx,
            );
        });
        cx.update(|window, cx| {
            ws.update(cx, |ws, cx| assert!(!ws.command("/new", false, window, cx)))
        });
        assert_eq!(ws.read_with(cx, |ws, _| ws.threads.len()), 2);
    }

    #[gpui_kit::test]
    async fn native_custom_commands_win_over_gui_builtins_but_compact_stays_runtime(
        cx: &mut TestAppContext,
    ) {
        let (ws, cx) = open(cx);
        ws.update(cx, |ws, cx| {
            let id = ws.thread().meta.id.clone();
            ws.threads[0].status = Status::Running;
            ws.ui["follow_up_steer"] = json!(false);
            let data = json!({"commands":[{"name":"compact","source":"builtin"},{"name":"model","source":"extension"}]});
            ws.on_event(&id, Event::Remote { command: "get_commands".into(), data }, cx);
            // The runtime's own `compact` is not a custom command, so it is not recorded as one.
            assert_eq!(ws.thread().commands, ["model"]);
        });
        cx.simulate_input("/model");
        cx.simulate_keystrokes("enter");
        assert_eq!(
            ws.read_with(cx, |ws, _| ws.thread().queue.back().cloned()),
            Some("/model".into())
        );
        // Keybindings still reach the GUI action.
        cx.update(|window, cx| {
            ws.update(cx, |ws, cx| assert!(ws.command("/model", true, window, cx)))
        });
    }

    #[gpui_kit::test]
    async fn opencode_commands_are_available_from_project_files(cx: &mut TestAppContext) {
        let (ws, cx) = open(cx);
        let dir =
            std::env::temp_dir().join(format!("greencode-opencode-command-{}", std::process::id()));
        std::fs::create_dir_all(dir.join(".opencode/commands/team")).unwrap();
        std::fs::write(
            dir.join(".opencode/commands/review.md"),
            "Review $ARGUMENTS",
        )
        .unwrap();
        std::fs::write(
            dir.join(".opencode/commands/team/test.md"),
            "Test $ARGUMENTS",
        )
        .unwrap();
        ws.update(cx, |ws, _| {
            ws.threads[0].meta.cwd = dir.clone();
            ws.threads[0].meta.backend = "opencode".into();
        });
        ws.update_in(cx, |ws, window, cx| ws.activate(0, window, cx));
        assert!(ws.read_with(cx, |ws, _| ws
            .commands
            .iter()
            .any(|command| command == "review")
            && ws.commands.iter().any(|command| command == "team/test")));
        ws.update(cx, |ws, _| ws.threads[0].status = Status::Running);
        cx.simulate_input("/team/test \"two words\"");
        cx.simulate_keystrokes("enter");
        assert_eq!(
            ws.read_with(cx, |ws, _| ws.thread().queue.back().cloned()),
            Some("/team/test \"two words\"".into())
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui_kit::test]
    async fn switching_backend_preserves_saved_session_without_visible_messages(
        cx: &mut TestAppContext,
    ) {
        let (ws, cx) = open(cx);
        let id = ws.read_with(cx, |ws, _| ws.thread().meta.id.clone());
        ws.update(cx, |ws, _| {
            ws.threads[0].meta.title = "saved".into();
            greencode_core::session::Session::create(&ws.thread().meta).unwrap();
            ws.thread().save().unwrap();
        });
        cx.update(|window, cx| {
            ws.update(cx, |ws, cx| {
                ws.run(
                    super::Act::BackendModel("pi".into(), "test".into()),
                    window,
                    cx,
                )
            })
        });
        ws.read_with(cx, |ws, _| {
            assert_eq!(ws.threads.len(), 2);
            assert_eq!(ws.threads[1].meta.id, id);
            assert_eq!(ws.threads[1].meta.backend, "native");
            assert_eq!(
                greencode_core::session::Session::view(&id)
                    .unwrap()
                    .unwrap()
                    .meta
                    .backend,
                "native"
            );
            assert_eq!(ws.thread().meta.backend, "pi");
        });
    }

    #[gpui_kit::test]
    async fn switching_backend_prepares_saved_history_as_draft(cx: &mut TestAppContext) {
        use greencode_core::session::Session;
        let (ws, cx) = open(cx);
        ws.update(cx, |ws, _| {
            let t = &mut ws.threads[0];
            let session = Session::create(&t.meta).unwrap();
            session
                .append(&greencode_core::Message::user("original request"))
                .unwrap();
            t.items.push(Item::User("original request".into()));
        });
        cx.simulate_input("pending request");
        cx.update(|window, cx| {
            ws.update(cx, |ws, cx| {
                ws.run(
                    super::Act::BackendModel("pi".into(), "test".into()),
                    window,
                    cx,
                )
            })
        });
        ws.read_with(cx, |ws, cx| {
            let draft = ws.input.read(cx).value().to_string();
            assert!(draft.contains("original request") && draft.contains("続きの依頼"));
            assert!(draft.ends_with("pending request"));
            assert_eq!(ws.threads[1].draft, "pending request");
            assert_eq!(
                Session::view(&ws.thread().meta.id).unwrap().unwrap().draft,
                draft
            );
            assert!(ws.thread().cmd.is_none());
        });
    }

    #[gpui_kit::test]
    async fn recall_stash_goal_snooze_keybindings_and_project_defaults(cx: &mut TestAppContext) {
        use greencode_core::permission::Mode;
        let (ws, cx) = open(cx);
        let value = |cx: &mut VisualTestContext| {
            ws.read_with(cx, |ws, cx| ws.input.read(cx).value().to_string())
        };
        ws.update(cx, |ws, _| {
            let t = &mut ws.threads[0];
            t.items.extend([
                Item::User("first".into()),
                Item::Assistant("ok".into()),
                Item::User("second".into()),
            ]);
            ws.list.reset(3);
        });
        // ↑/↓ walk the sent prompts; moving past the newest clears the composer.
        for (key, expected) in [
            ("up", "second"),
            ("up", "first"),
            ("down", "second"),
            ("down", ""),
        ] {
            cx.simulate_keystrokes(key);
            assert_eq!(value(cx), expected, "{key}");
        }
        // ⌘S stashes the draft, and restores it into an empty composer; the stash survives in ui.json.
        cx.simulate_input("draft to park");
        cx.simulate_keystrokes("cmd-s");
        assert_eq!(value(cx), "");
        assert!(
            std::fs::read_to_string(greencode_core::home().join("ui.json"))
                .unwrap()
                .contains("draft to park")
        );
        cx.simulate_keystrokes("cmd-s");
        assert_eq!(value(cx), "draft to park");
        cx.simulate_keystrokes("cmd-a backspace");

        // ⌥↑ opens the most recently queued message for editing without removing it.
        ws.update(cx, |ws, _| {
            ws.threads[0]
                .queue
                .extend(["queued one".to_string(), "queued two".to_string()])
        });
        cx.simulate_keystrokes("alt-up");
        assert_eq!(value(cx), "queued two");
        assert_eq!(ws.read_with(cx, |ws, _| ws.thread().queue.len()), 2);
        cx.update(|window, cx| ws.update(cx, |ws, cx| ws.finish_queue_edit(false, window, cx)));
        cx.simulate_keystrokes("cmd-a backspace");
        ws.update(cx, |ws, _| ws.threads[0].queue.clear());

        cx.simulate_input("/rename Shipping work");
        cx.simulate_keystrokes("enter");
        assert_eq!(
            ws.read_with(cx, |ws, _| ws.thread().meta.title.clone()),
            "Shipping work"
        );

        // A user keybinding runs a command; ⇧⌘A cycles the permission mode.
        ws.update(cx, |ws, _| ws.ui["keys"] = json!({"ctrl-p": "/plan"}));
        cx.simulate_keystrokes("ctrl-p");
        assert!(ws.read_with(cx, |ws, _| ws.thread().perms.mode() == Mode::Plan));
        cx.simulate_keystrokes("cmd-shift-a");
        assert!(ws.read_with(cx, |ws, _| ws.thread().perms.mode() == Mode::Ask));

        // Goal mode queues a continuation after each turn until the agent reports GOAL MET.
        ws.update(cx, |ws, cx| {
            let id = ws.thread().meta.id.clone();
            ws.threads[0].goal = Some(("tests pass".into(), 0));
            ws.on_event(&id, Event::Text("still working".into()), cx);
            ws.on_event(&id, Event::Done { waiting_for_delegates: false }, cx);
            assert!(
                ws.thread()
                    .queue
                    .front()
                    .is_some_and(|q| q.contains("tests pass"))
                    && ws.thread().goal.as_ref().unwrap().1 == 1
            );
            ws.on_event(&id, Event::Text(" ... GOAL MET".into()), cx);
            ws.on_event(&id, Event::Done { waiting_for_delegates: false }, cx);
            assert!(ws.thread().goal.is_none() && ws.thread().queue.is_empty());
        });

        // A project default applies to new threads there, whatever the current thread uses.
        cx.update(|window, cx| {
            ws.update(cx, |ws, cx| {
                ws.run(super::Act::Model("openai/x".into()), window, cx);
                ws.run(super::Act::Mode(Mode::Full), window, cx);
                ws.run(super::Act::SaveProjectDefault, window, cx);
                ws.run(super::Act::Model("openai/y".into()), window, cx);
                ws.run(super::Act::Mode(Mode::Ask), window, cx);
                ws.run(super::Act::Toggle("hide_tools"), window, cx);
            })
        });
        cx.simulate_keystrokes("cmd-n");
        ws.read_with(cx, |ws, _| {
            assert_eq!(
                (ws.threads.len(), ws.thread().meta.model.as_str()),
                (2, "openai/x")
            );
            assert!(ws.thread().perms.mode() == Mode::Full && ws.ui["hide_tools"] == true);
        });
        // Snoozing hides the thread and moves to another one; an approval request wakes it.
        cx.update(|window, cx| {
            ws.update(cx, |ws, cx| ws.run(super::Act::Snooze(3600), window, cx))
        });
        ws.update(cx, |ws, cx| {
            assert!(ws.active == 1 && ws.threads[0].snoozed > super::now());
            let id = ws.threads[0].meta.id.clone();
            let (reply, _rx) = oneshot::channel();
            ws.on_event(
                &id,
                Event::Permission {
                    tool: "bash".into(),
                    summary: "ls".into(),
                    reply,
                },
                cx,
            );
            assert_eq!(ws.threads[0].snoozed, 0);
        });
    }

    #[gpui_kit::test]
    async fn advanced_settings_edit_any_config_key_and_reload(cx: &mut TestAppContext) {
        let (ws, cx) = open(cx);
        cx.simulate_keystrokes("cmd-,");
        let config = greencode_core::home().join("config.toml");
        cx.update(|window, cx| {
            ws.update(cx, |ws, cx| {
                ws.run(super::Act::SettingsTab("詳細設定"), window, cx);
                // Credentials are listed but never editable here.
                assert!(ws.advanced.rows.iter().any(|r| r.key == "tui.symbol_preset" && r.writable));
                assert!(!ws.advanced.rows.iter().any(|r| r.writable && r.key.ends_with("api_key")));
                ws.run(super::Act::SettingPick("tui.symbol_preset".into()), window, cx);
                assert_eq!(ws.advanced.value.read(cx).value().as_ref(), "unicode");
                ws.advanced.value.update(cx, |s, cx| s.set_value("ascii", window, cx));
                ws.run(super::Act::SettingSave, window, cx);
                assert_eq!(ws.cfg.tui.symbol_preset, greencode_core::config::SymbolPreset::Ascii);
                assert!(ws.advanced.rows.iter().any(|r| r.key == "tui.symbol_preset" && r.set));
                // An invalid value is refused and leaves the file alone.
                ws.advanced.value.update(cx, |s, cx| s.set_value("sideways", window, cx));
                ws.run(super::Act::SettingSave, window, cx);
                assert!(ws.advanced.outcome.as_ref().is_some_and(|(ok, _)| !ok));
                ws.run(super::Act::SettingReset, window, cx);
                assert_eq!(ws.cfg.tui.symbol_preset, greencode_core::config::SymbolPreset::Unicode);
                assert_eq!(ws.advanced.value.read(cx).value().as_ref(), "unicode");
            })
        });
        assert!(!std::fs::read_to_string(&config).unwrap_or_default().contains("symbol_preset"));
    }

    #[gpui_kit::test]
    async fn runtime_settings_edit_native_settings_and_tools(cx: &mut TestAppContext) {
        let (ws, cx) = open(cx);
        cx.simulate_keystrokes("cmd-,");
        cx.update(|window, cx| {
            ws.update(cx, |ws, cx| {
                ws.run(super::Act::SettingsTab(super::RUNTIME_SETTINGS), window, cx);
                // Only list values that GreenCode accepts for the native setting.
                assert_eq!(ws.advanced.choices["tui.symbol_preset"], ["unicode", "nerd", "ascii"]);
                ws.run(super::Act::SettingWrite("tui.symbol_preset".into(), Some("ascii".into())), window, cx);
                assert_eq!(ws.cfg.tui.symbol_preset, greencode_core::config::SymbolPreset::Ascii);
                ws.run(super::Act::SettingWrite("tui.symbol_preset".into(), None), window, cx);
                assert_eq!(ws.cfg.tui.symbol_preset, greencode_core::config::SymbolPreset::Unicode);
                // A built-in tool goes through the allowlist; an `ext` one through the ツール tab's switch.
                ws.run(super::Act::RuntimeTool("grep", false), window, cx);
                assert!(!greencode_core::tools::tool_on(&ws.cfg.tools, "grep") && greencode_core::tools::tool_on(&ws.cfg.tools, "read"));
                ws.run(super::Act::RuntimeTool("ast", false), window, cx);
                assert!(ws.cfg.ext_off.iter().any(|t| t == "ast"));
                // The GUI's own overrides survive the reload a config edit triggers.
                ws.run(super::Act::RuntimeTool("grep", true), window, cx);
                assert!(ws.cfg.tools.is_empty() && ws.cfg.ext_off.iter().any(|t| t == "ast"));
            })
        });
    }

    #[gpui_kit::test]
    async fn settings_screen_changes_and_persists_settings(cx: &mut TestAppContext) {
        use greencode_core::permission::Mode;
        let (ws, cx) = open(cx);
        let id = ws.read_with(cx, |ws, _| ws.thread().meta.id.clone());
        cx.simulate_keystrokes("cmd-,");
        assert!(ws.read_with(cx, |ws, _| ws.settings.is_some()));
        // A keybinding typed into the two fields is saved and works at once.
        cx.update(|window, cx| {
            ws.update(cx, |ws, cx| {
                let fields = ws.settings.as_ref().unwrap().0.clone();
                fields[0].update(cx, |s, cx| s.set_value("ctrl-g", window, cx));
                fields[1].update(cx, |s, cx| s.set_value("/diff", window, cx));
                ws.run(super::Act::Bind, window, cx);
                ws.run(super::Act::DefaultMode(Mode::Full), window, cx);
                ws.run(super::Act::Model("openai/z".into()), window, cx);
                ws.run(super::Act::DefaultModel, window, cx);
                ws.run(super::Act::Toggle("notify_off"), window, cx);
            })
        });
        cx.simulate_keystrokes("escape");
        assert!(ws.read_with(cx, |ws, _| ws.settings.is_none()));
        cx.simulate_keystrokes("ctrl-g");
        assert!(ws.read_with(cx, |ws, _| ws.show_diff));
        let saved: serde_json::Value =
            serde_json::from_slice(&std::fs::read(greencode_core::home().join("ui.json")).unwrap())
                .unwrap();
        assert_eq!(
            saved,
            json!({"keys": {"ctrl-g": "/diff"}, "last_thread": id, "mode": "full", "model": "openai/z", "notify_off": true})
        );
        // The next launch starts from the saved defaults.
        cx.update(|window, cx| {
            let again = cx.new(|cx| Workspace::new(Config::default(), None, window, cx));
            again.read_with(cx, |ws, _| {
                assert!(
                    ws.cfg.mode == Mode::Full
                        && ws.cfg.model == "openai/z"
                        && ws.thread().meta.model == "openai/z"
                );
                assert!(ws.thread().perms.mode() == Mode::Full);
            });
        });
        cx.update(|window, cx| {
            ws.update(cx, |ws, cx| {
                ws.run(super::Act::Unbind("ctrl-g".into()), window, cx)
            })
        });
        assert!(ws.read_with(cx, |ws, _| ws.ui["keys"].as_object().unwrap().is_empty()));
        // A text setting opens the palette over the settings screen and saves what is typed.
        cx.update(|window, cx| {
            ws.update(cx, |ws, cx| {
                ws.run(super::Act::Settings, window, cx);
                ws.run(super::Act::EditText(super::git::TextPrompt::VoiceCorrection), window, cx)
            })
        });
        cx.simulate_input("訂正");
        cx.simulate_keystrokes("enter");
        ws.read_with(cx, |ws, _| {
            assert_eq!(ws.ui["voice_correction"], "訂正");
            assert!(ws.palette.is_none() && ws.settings.is_some());
        });
    }

    #[gpui_kit::test]
    async fn project_picker_cancel_and_manual_folder_path(cx: &mut TestAppContext) {
        let (ws, cx) = open(cx);
        let project = greencode_core::home().join("My Project");
        std::fs::create_dir_all(&project).unwrap();
        let project = project.canonicalize().unwrap();
        let file = project.join("not a folder");
        std::fs::write(&file, "").unwrap();
        let original = ws.read_with(cx, |ws, _| ws.thread().meta.id.clone());

        cx.simulate_keystrokes("cmd-o");
        cx.simulate_path_prompt_response(|_| None);
        cx.run_until_parked();
        ws.read_with(cx, |ws, _| {
            assert!(ws.palette.is_none());
            assert_eq!(ws.thread().meta.id, original);
        });

        for path in [&file, &project] {
            cx.update(|window, cx| ws.update(cx, |ws, cx| {
                ws.run(super::Act::EditText(super::git::TextPrompt::OpenProject), window, cx)
            }));
            cx.simulate_input(&path.to_string_lossy());
            cx.simulate_keystrokes("enter");
            ws.read_with(cx, |ws, _| {
                if path == &file {
                    assert_eq!(ws.thread().meta.id, original);
                    assert!(matches!(ws.thread().items.last(), Some(Item::Error(_))));
                } else {
                    assert_eq!(ws.thread().meta.cwd, project);
                }
            });
        }
    }

    #[gpui_kit::test]
    async fn pull_requests_offer_review_checkout_and_merge(cx: &mut TestAppContext) {
        let (ws, cx) = open(cx);
        let prs = super::parse_prs(br#"[{"number":7,"title":"Fix","headRefName":"fix"},{"number":9,"title":"Other","headRefName":"other"},{"title":"no number"}]"#);
        assert_eq!(prs.len(), 2);
        ws.update(cx, |ws, _| {
            ws.prs = prs;
            ws.branch = "fix".into();
        });
        let shells: Vec<String> = ws.read_with(cx, |ws, _| {
            ws.acts()
                .into_iter()
                .filter_map(|(_, a)| {
                    if let super::Act::Shell(c) = a {
                        Some(c)
                    } else {
                        None
                    }
                })
                .collect()
        });
        // Checks and merge are only for the current branch's PR; other PRs can be checked out.
        for want in [
            "gh pr checks 7",
            "gh pr merge 7 --squash",
            "gh pr checkout 9",
        ] {
            assert!(shells.iter().any(|c| c == want), "{want}: {shells:?}");
        }
        assert!(!shells
            .iter()
            .any(|c| c == "gh pr merge 9 --squash" || c == "gh pr checkout 7"));
        let reviews = ws.read_with(cx, |ws, _| {
            ws.acts()
                .into_iter()
                .filter(|(_, a)| matches!(a, super::Act::Ask(p) if p.contains("gh pr diff")))
                .count()
        });
        assert_eq!(reviews, 2);
    }

    #[gpui_kit::test]
    async fn threads_move_within_their_group_and_keep_the_order_after_restart(
        cx: &mut TestAppContext,
    ) {
        use greencode_core::session::{Meta, Session};
        let (ws, cx) = open(cx);
        let cwd = ws.read_with(cx, |ws, _| ws.thread().meta.cwd.clone());
        let ids: Vec<String> = ["a", "b"]
            .iter()
            .map(|title| {
                let mut meta = Meta::new(cwd.clone(), "m".into(), "native".into());
                meta.title = title.to_string();
                Session::create(&meta).unwrap();
                ws.update(cx, |ws, _| {
                    ws.threads
                        .push(super::Thread::new(meta.clone(), false, Default::default()))
                });
                meta.id
            })
            .collect();
        // The last thread moves up past its neighbour, and stops at the top of the group.
        cx.update(|window, cx| {
            ws.update(cx, |ws, cx| {
                ws.active = 2;
                ws.run(super::Act::Move(true), window, cx);
                assert_eq!(
                    (
                        ws.active,
                        ws.threads[1].meta.id.as_str(),
                        ws.threads[2].meta.id.as_str()
                    ),
                    (1, ids[1].as_str(), ids[0].as_str())
                );
                ws.run(super::Act::Move(false), window, cx);
                ws.run(super::Act::Move(false), window, cx);
                assert_eq!(ws.active, 2);
                ws.run(super::Act::Move(true), window, cx);
            })
        });
        cx.update(|window, cx| {
            let again = cx.new(|cx| Workspace::new(Config::default(), None, window, cx));
            again.read_with(cx, |ws, _| {
                assert_eq!(
                    ws.threads[1..]
                        .iter()
                        .map(|t| t.meta.id.as_str())
                        .collect::<Vec<_>>(),
                    [ids[1].as_str(), ids[0].as_str()]
                )
            });
        });
    }

    #[test]
    fn terminal_scrollback_is_plain_text_across_chunk_boundaries() {
        let (mut lines, mut open) = (vec![], false);
        // A colored prompt split over two chunks, a progress line redrawn with `\r`, a title sequence and a backspace.
        for chunk in [
            "\u{1b}[1;32muser\u{1b}[0m $ ls\r\na.txt  b",
            ".txt\r\n10%\r50%\r100%\r\n",
            "\u{1b}]0;title\u{7}$ ab\u{8}c",
        ] {
            super::term_feed(&mut lines, &mut open, chunk);
        }
        assert_eq!(
            lines
                .iter()
                .map(|l| super::terminal::plain(l))
                .collect::<Vec<_>>(),
            ["user $ ls", "a.txt  b.txt", "100%", "$ ac"]
        );
        assert!(open);
    }

    #[gpui_kit::test]
    async fn schedule_due_reminders_start_idle_threads_or_queue_and_unschedule(
        cx: &mut TestAppContext,
    ) {
        let (ws, cx) = open(cx);
        cx.simulate_input("/schedule +1h later prompt");
        cx.simulate_keystrokes("enter");
        cx.simulate_input("/schedule 09:00 morning prompt");
        cx.simulate_keystrokes("enter");
        ws.update(cx, |ws, cx| {
            assert_eq!(ws.ui["reminders"].as_array().unwrap().len(), 2);
            let rows = ws.reminder_rows();
            assert!(
                rows[1].0.starts_with("予約: 09:00 morning")
                    && matches!(rows[1].1, super::Act::Unschedule(1))
            );
            ws.unschedule(1);
            assert_eq!(ws.ui["reminders"].as_array().unwrap().len(), 1);
            // Due while idle: started as the user's own prompt.
            let n = super::now();
            ws.ui["reminders"][0]["at"] = json!(n - 1);
            ws.run_due_reminders(n, cx);
            assert!(ws.ui["reminders"].as_array().unwrap().is_empty());
            assert!(ws
                .thread()
                .items
                .iter()
                .any(|i| matches!(i, super::Item::User(u) if &**u == "later prompt")));
            // Due while running: queued.
            let id = ws.thread().meta.id.clone();
            ws.ui["reminders"] = json!([{"thread": id, "at": n - 1, "prompt": "queued prompt"}]);
            ws.run_due_reminders(n, cx);
            assert!(ws.ui["reminders"].as_array().unwrap().is_empty());
            assert_eq!(
                ws.thread().queue.front().map(String::as_str),
                Some("queued prompt")
            );
        });
    }

    #[gpui_kit::test]
    async fn schedule_bang_installs_a_launchd_job_and_unschedule_removes_it(
        cx: &mut TestAppContext,
    ) {
        let (ws, cx) = open(cx);
        let dir = std::env::temp_dir().join(format!("greencode-agents-{}", std::process::id()));
        ws.update(cx, |ws, _| {
            ws.ui["launch_agents_dir"] = json!(dir.to_string_lossy());
            ws.ui["cli_path"] = json!("/fake/greencode-cli");
        });
        cx.simulate_input("/schedule! +30m test");
        cx.simulate_keystrokes("enter");
        // No session file yet: refused, nothing stored or installed.
        assert_eq!(
            ws.read_with(cx, |ws, _| ws.ui["reminders"]
                .as_array()
                .map_or(0, |l| l.len())),
            0
        );
        assert!(!dir.exists() || std::fs::read_dir(&dir).unwrap().next().is_none());
        let session = ws.read_with(cx, |ws, _| {
            greencode_core::session::Session::path(&ws.thread().meta.id)
        });
        std::fs::create_dir_all(session.parent().unwrap()).unwrap();
        std::fs::write(&session, "").unwrap();
        cx.simulate_input("/schedule! +30m test");
        cx.simulate_keystrokes("enter");
        let label = ws
            .read_with(cx, |ws, _| {
                ws.ui["reminders"][0]["label"].as_str().map(String::from)
            })
            .expect("labelled reminder");
        let plist = dir.join(format!("{label}.plist"));
        let text = std::fs::read_to_string(&plist).unwrap();
        assert!(
            label.starts_with("dev.greencode.schedule.")
                && text.contains(&label)
                && text.contains("<string>test</string>")
                && text.contains("/fake/greencode-cli")
        );
        // A second prompt at the same time must get its own job and `.done` marker.
        cx.simulate_input("/schedule! +30m second");
        cx.simulate_keystrokes("enter");
        let second = ws
            .read_with(cx, |ws, _| {
                ws.ui["reminders"][1]["label"].as_str().map(String::from)
            })
            .expect("second label");
        assert_ne!(label, second);
        assert!(dir.join(format!("{second}.plist")).exists());
        ws.update(cx, |ws, _| ws.unschedule(1));
        ws.update(cx, |ws, _| ws.unschedule(0));
        assert!(!plist.exists() && !dir.join(format!("{second}.plist")).exists());
        assert_eq!(
            ws.read_with(cx, |ws, _| ws.ui["reminders"].as_array().unwrap().len()),
            0
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[gpui_kit::test]
    async fn scheduled_prompts_run_in_new_background_threads_when_due(cx: &mut TestAppContext) {
        let (ws, cx) = open(cx);
        cx.simulate_input("/schedule 30m check the build");
        cx.simulate_keystrokes("enter");
        cx.simulate_input("/schedule soon nothing");
        cx.simulate_keystrokes("enter");
        let (next, first) = ws.read_with(cx, |ws, _| {
            (
                ws.ui["schedules"][0]["next"].as_u64().unwrap(),
                ws.thread().meta.id.clone(),
            )
        });
        assert_eq!(
            ws.read_with(cx, |ws, _| ws.ui["schedules"].as_array().unwrap().len()),
            1
        );
        ws.update(cx, |ws, cx| {
            // Not due yet: nothing starts. Once due, a new thread runs it and the visible thread stays put.
            ws.run_due_schedules(next - 1, cx);
            assert_eq!(ws.threads.len(), 1);
            ws.run_due_schedules(next, cx);
            assert_eq!((ws.threads.len(), ws.thread().meta.id == first), (2, true));
            assert!(
                ws.threads[0].status == Status::Running
                    && matches!(&ws.threads[0].items[..], [Item::User(p)] if p == "check the build")
            );
            assert_eq!(ws.ui["schedules"][0]["next"], next + 1800);
            ws.run_due_schedules(next, cx);
            assert_eq!(ws.threads.len(), 2);
        });
    }

    #[gpui_kit::test]
    async fn idle_threads_settle_after_a_week_unless_turned_off(cx: &mut TestAppContext) {
        use greencode_core::session::Meta;
        let (ws, cx) = open(cx);
        ws.update(cx, |ws, _| {
            let cwd = ws.thread().meta.cwd.clone();
            for title in ["old", "busy"] {
                let mut meta = Meta::new(cwd.clone(), "m".into(), "native".into());
                meta.title = title.into();
                ws.threads
                    .push(super::Thread::new(meta, false, Default::default()));
            }
            ws.threads[2].status = Status::Running;
            let created = ws.threads[1].meta.created;
            ws.ui["auto_settle_off"] = json!(true);
            ws.auto_settle(created + 8 * 86400);
            assert!(!ws.threads[1].archived);
            ws.ui["auto_settle_off"] = json!(false);
            ws.auto_settle(created + 6 * 86400);
            assert!(!ws.threads[1].archived);
            ws.auto_settle(created + 8 * 86400);
            assert!(ws.threads[1].archived && !ws.threads[2].archived && !ws.threads[0].archived);
        });
    }

    #[gpui_kit::test]
    async fn full_screen_terminal_programs_use_the_screen_and_raw_keys(cx: &mut TestAppContext) {
        use gpui_kit::Keystroke;
        let (ws, cx) = open(cx);
        cx.update(|window, cx| {
            ws.update(cx, |ws, cx| {
                ws.open_terminal(window, cx);
                let term = ws.active_terminal_mut().unwrap();
                term.feed("$ vim\r\n");
                // Entering the alternate screen: output is positioned on the grid and kept out of the scrollback.
                term.feed("\u{1b}[?1049h\u{1b}[2;3Hhi\u{1b}[1;1H");
                let screen = term.screen.screen();
                assert!(screen.alternate_screen());
                assert_eq!(screen.rows(0, 110).nth(1).unwrap(), "  hi");
                assert_eq!(screen.cursor_position(), (0, 0));
                term.feed("\u{1b}[?1049l$ ");
                assert!(!term.screen.screen().alternate_screen());
                assert_eq!(
                    term.lines.iter().map(|l| l.as_ref()).collect::<Vec<_>>(),
                    ["$ vim", "$ "]
                );
            })
        });
        let key = |s: &'static str| super::term_key(&Keystroke::parse(s).unwrap());
        assert_eq!(key("escape").unwrap(), b"\x1b");
        assert_eq!(key("up").unwrap(), b"\x1b[A");
        assert_eq!(key("ctrl-c").unwrap(), [3]);
        assert_eq!(key("j").unwrap(), b"j");
        assert_eq!(key("cmd-j"), None);
    }

    #[gpui_kit::test]
    async fn terminal_resize_preserves_wide_rows(cx: &mut TestAppContext) {
        let (ws, cx) = open(cx);
        cx.update(|window, cx| {
            ws.update(cx, |ws, cx| {
                ws.open_terminal(window, cx);
                let term = ws.active_terminal_mut().unwrap();
                term.resize(14, 160);
                let (rows, cols) = term.screen.screen().size();
                assert_eq!((rows.get(), cols.get()), (14, 160));
                let line = "x".repeat(150);
                term.feed(&line);
                term.resize(14, 160);
                assert_eq!(term.screen.screen().rows(0, 160).next().unwrap(), line);
                assert_eq!(term.screen.screen().cursor_position(), (0, 150));
            })
        });
    }

    #[gpui_kit::test]
    async fn usage_panel_opens_renders_and_closes_on_escape(cx: &mut TestAppContext) {
        let (ws, cx) = open(cx);
        ws.update_in(cx, |ws, window, cx| {
            ws.run(super::Act::Usage, window, cx);
            assert!(ws.usage.as_ref().unwrap().reports.is_none());
            ws.usage.as_mut().unwrap().reports = Some(vec![
                Ok(greencode_core::usage::Report {
                    provider: "Claude".into(),
                    plan: Some("max".into()),
                    limits: vec![greencode_core::usage::Limit {
                        label: "5時間".into(),
                        used: 85.,
                        resets_at: Some(super::now() + 7200),
                    }],
                }),
                Err("boom".into()),
            ]);
            cx.notify();
        });
        cx.run_until_parked();
        cx.simulate_keystrokes("escape");
        ws.read_with(cx, |ws, _| assert!(ws.usage.is_none()));
    }

    #[gpui_kit::test]
    async fn usage_history_tab_and_period_keys(cx: &mut TestAppContext) {
        use super::usage::UsageTab;
        let (ws, cx) = open(cx);
        ws.update_in(cx, |ws, window, cx| {
            ws.run(super::Act::UsageHistory, window, cx);
            let u = ws.usage.as_mut().unwrap();
            assert!(u.tab == UsageTab::Cost && u.days == 30);
            u.history = Some(vec![greencode_core::usage_history::Row {
                day: (super::now() / 86400) as u32,
                provider: "claude".into(),
                model: "claude-sonnet-4-5".into(),
                input: 1_000,
                output: 500,
                cache_read: 2_000,
                cache_write: 0,
                reported: 0.,
            }]);
            cx.notify();
        });
        cx.run_until_parked();
        cx.simulate_keystrokes("t");
        ws.read_with(cx, |ws, _| assert!(ws.usage.as_ref().unwrap().tab == UsageTab::Tokens));
        cx.simulate_keystrokes("3");
        ws.read_with(cx, |ws, _| assert_eq!(ws.usage.as_ref().unwrap().days, 92));
        cx.simulate_keystrokes("1");
        ws.read_with(cx, |ws, _| assert_eq!(ws.usage.as_ref().unwrap().days, 7));
        cx.simulate_keystrokes("c");
        ws.read_with(cx, |ws, _| assert!(ws.usage.as_ref().unwrap().tab == UsageTab::Cost));
        cx.simulate_keystrokes("escape");
        ws.read_with(cx, |ws, _| assert!(ws.usage.is_none()));
    }

    #[gpui_kit::test]
    async fn pull_requests_panel_toggles_and_closes_the_files_panel(cx: &mut TestAppContext) {
        use super::Act;
        let (ws, cx) = open(cx);
        ws.update_in(cx, |ws, window, cx| {
            ws.run(Act::ToggleFiles, window, cx);
            assert!(ws.files_panel.is_some());
            ws.run(Act::PullRequests, window, cx);
            assert!(ws.pr_panel.is_some() && ws.files_panel.is_none() && !ws.show_diff);
            ws.run(Act::ToggleFiles, window, cx);
            assert!(ws.files_panel.is_some() && ws.pr_panel.is_none());
            ws.run(Act::PullRequests, window, cx);
            ws.run(Act::ToggleDiff, window, cx);
            assert!(ws.show_diff && ws.pr_panel.is_none());
            ws.run(Act::PullRequests, window, cx);
            assert!(ws.pr_panel.is_some() && !ws.show_diff);
            ws.run(Act::PullRequests, window, cx);
            assert!(ws.pr_panel.is_none());
        });
    }

    #[gpui_kit::test]
    async fn files_panel_browses_the_project_tree(cx: &mut TestAppContext) {
        use super::{files::Row, Act};
        let dir = std::env::temp_dir().join(format!("greencode-files-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("a")).unwrap();
        std::fs::write(dir.join("a/b.txt"), "b\n").unwrap();
        std::fs::write(dir.join("c.rs"), "fn main() {}\nlet x = 1;\n").unwrap();
        // 1x1 PNG.
        std::fs::write(
            dir.join("p.png"),
            [
                0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0, 0, 0, 0x0d, 0x49, 0x48, 0x44,
                0x52, 0, 0, 0, 1, 0, 0, 0, 1, 8, 6, 0, 0, 0, 0x1f, 0x15, 0xc4, 0x89, 0, 0, 0, 0x0d,
                0x49, 0x44, 0x41, 0x54, 0x78, 0x9c, 0x63, 0xf8, 0xff, 0xff, 0x3f, 0, 5, 0xfe, 2,
                0xfe, 0xa7, 0x35, 0x81, 0x84, 0, 0, 0, 0, 0x49, 0x45, 0x4e, 0x44, 0xae, 0x42, 0x60,
                0x82,
            ],
        )
        .unwrap();
        std::fs::write(dir.join("n.md"), "# hi\nbody\n").unwrap();
        assert!(std::process::Command::new("git")
            .arg("-C")
            .arg(&dir)
            .arg("init")
            .output()
            .unwrap()
            .status
            .success());
        let git = |args: &[&str]| {
            assert!(std::process::Command::new("git")
                .arg("-C")
                .arg(&dir)
                .args(["-c", "user.name=t", "-c", "user.email=t@t"])
                .args(args)
                .output()
                .unwrap()
                .status
                .success())
        };
        git(&["add", "."]);
        git(&["commit", "-m", "init"]);
        std::fs::write(dir.join("a/b.txt"), "b\nchanged\n").unwrap();
        let (ws, cx) = open(cx);
        ws.update_in(cx, |ws, window, cx| {
            ws.threads[0].meta.cwd = dir.clone();
            ws.show_diff = true;
            ws.run(Act::ToggleFiles, window, cx);
            assert!(ws.files_panel.is_some() && !ws.show_diff);
            let paths = ws.files().to_vec();
            let names = |ws: &Workspace| super::files::tree_rows(&paths, &ws.files_panel.as_ref().unwrap().expanded).iter().map(|r| match r {
                Row::Dir { path, .. } | Row::File { path, .. } => path.clone(),
            }).collect::<Vec<_>>();
            assert_eq!(names(ws), ["a", "c.rs", "n.md", "p.png"]);
            ws.files_panel.as_mut().unwrap().expanded.insert("a".into());
            assert_eq!(names(ws), ["a", "a/b.txt", "c.rs", "n.md", "p.png"]);
            ws.open_file("c.rs", cx);
            let Some((path, super::files::Content::Text(lines))) = ws.files_panel.as_ref().unwrap().open.clone() else { panic!("text") };
            assert_eq!(path, "c.rs");
            assert_eq!(lines, ["fn main() {}", "let x = 1;"]);
            ws.open_file("p.png", cx);
            assert!(matches!(&ws.files_panel.as_ref().unwrap().open, Some((_, super::files::Content::Image(p))) if p.ends_with("p.png")));
            ws.open_file("n.md", cx);
            assert!(matches!(&ws.files_panel.as_ref().unwrap().open, Some((_, super::files::Content::Markdown { source, .. })) if source.as_ref() == "# hi\nbody"));
        });
        cx.run_until_parked();
        ws.read_with(cx, |ws, _| assert!(!ws.show_diff && !ws.diff.is_empty()));
        ws.update_in(cx, |ws, window, cx| {
            ws.run(Act::ToggleFiles, window, cx);
            assert!(ws.files_panel.is_none());
        });
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[gpui_kit::test]
    async fn large_text_paste_becomes_a_file_attachment(cx: &mut TestAppContext) {
        let (ws, cx) = open(cx);
        let value = |ws: &Entity<Workspace>, cx: &mut VisualTestContext| {
            ws.read_with(cx, |ws, cx| ws.input.read(cx).value().to_string())
        };
        cx.write_to_clipboard(gpui_kit::ClipboardItem::new_string("y".repeat(10)));
        cx.simulate_keystrokes("cmd-v");
        assert_eq!(value(&ws, cx), "yyyyyyyyyy");
        assert!(!greencode_core::home().join("pastes").exists());
        cx.simulate_keystrokes("cmd-a backspace");
        cx.write_to_clipboard(gpui_kit::ClipboardItem::new_string("x".repeat(40_000)));
        cx.simulate_keystrokes("cmd-v");
        let text = value(&ws, cx);
        let path = text
            .trim()
            .strip_prefix('@')
            .unwrap_or_else(|| panic!("{}", &text[..text.len().min(80)]));
        assert!(path.contains("pastes/paste-"), "{text}");
        assert_eq!(std::fs::metadata(path).unwrap().len(), 40_000);
    }

    #[gpui_kit::test]
    async fn tiff_clipboard_image_is_pasted_as_png(cx: &mut TestAppContext) {
        let (ws, cx) = open(cx);
        let mut tiff = std::io::Cursor::new(Vec::new());
        image::RgbImage::new(2, 2)
            .write_to(&mut tiff, image::ImageFormat::Tiff)
            .unwrap();
        cx.write_to_clipboard(gpui_kit::ClipboardItem::from(gpui_kit::Image::from_bytes(
            gpui_kit::ImageFormat::Tiff,
            tiff.into_inner(),
        )));
        cx.simulate_keystrokes("cmd-v");
        let (text, images) =
            ws.read_with(cx, |ws, cx| (ws.input.read(cx).value().to_string(), ws.thread().images.clone()));
        assert_eq!(text, "", "the path stays out of the composer");
        assert_eq!(images.len(), 1);
        assert!(images[0].extension().is_some_and(|e| e == "png"));
        assert!(std::fs::read(&images[0]).unwrap().starts_with(b"\x89PNG"));
        let shown = super::thread_view::without_image_paths(&format!("見て {}", images[0].display()));
        assert_eq!(shown, "見て");
    }

    #[test]
    fn theme_palettes_have_accessible_contrast_and_legacy_compat() {
        use super::{Pal, ThemeId};

        assert_eq!(ThemeId::from_json(&json!(null)), ThemeId::System);
        assert_eq!(ThemeId::from_json(&json!(false)), ThemeId::Light);
        assert_eq!(ThemeId::from_json(&json!(true)), ThemeId::Dark);
        for t in ThemeId::ALL {
            assert_eq!(ThemeId::from_json(&t.to_json()), t);
            assert_eq!(ThemeId::parse(t.id()), Some(t));
            assert_eq!(ThemeId::parse(t.label()), Some(t));
        }

        let composite = |fg: gpui_kit::Hsla, bg: gpui_kit::Hsla| -> [f32; 3] {
            let f: gpui_kit::Rgba = fg.into();
            let b: gpui_kit::Rgba = bg.into();
            let a = f.a;
            [
                f.r * a + b.r * (1. - a),
                f.g * a + b.g * (1. - a),
                f.b * a + b.b * (1. - a),
            ]
        };
        let lum = |c: [f32; 3]| -> f32 {
            let lin = |v: f32| {
                if v <= 0.03928 {
                    v / 12.92
                } else {
                    ((v + 0.055) / 1.055).powf(2.4)
                }
            };
            0.2126 * lin(c[0]) + 0.7152 * lin(c[1]) + 0.0722 * lin(c[2])
        };
        let contrast = |fg: gpui_kit::Hsla, bg: gpui_kit::Hsla| -> f32 {
            let l1 = lum(composite(fg, bg));
            let l2 = lum(composite(bg, gpui_kit::white()));
            let (hi, lo) = if l1 >= l2 { (l1, l2) } else { (l2, l1) };
            (hi + 0.05) / (lo + 0.05)
        };

        for t in [
            ThemeId::Emerald,
            ThemeId::EmeraldLight,
            ThemeId::Paper,
            ThemeId::Sand,
            ThemeId::Graphite,
            ThemeId::Light,
            ThemeId::Dark,
        ] {
            let pal = Pal::for_theme(t, false);
            assert!(contrast(pal.fg, pal.panel) >= 10.0, "{t:?} fg/panel");
            assert!(contrast(pal.fg, pal.sidebar) >= 10.0, "{t:?} fg/sidebar");
            assert!(contrast(pal.fg, pal.card) >= 9.0, "{t:?} fg/card");
            assert!(contrast(pal.muted, pal.panel) >= 4.2, "{t:?} muted/panel");
            let min_accent = if matches!(t, ThemeId::Light | ThemeId::Dark) {
                4.0
            } else {
                4.5
            };
            assert!(
                contrast(pal.accent, pal.panel) >= min_accent,
                "{t:?} accent/panel"
            );
            let min_accent_fg = if matches!(t, ThemeId::Light | ThemeId::Dark) {
                3.0
            } else {
                5.0
            };
            assert!(
                contrast(pal.accent_fg, pal.accent) >= min_accent_fg,
                "{t:?} accent_fg/accent"
            );
        }
    }

    #[gpui_kit::test]
    async fn themes_switch_persist_and_support_slash_command(cx: &mut TestAppContext) {
        use super::{themes, Choice, Pal, ThemeId};
        use gpui_kit::component::ActiveTheme;

        let (ws, cx) = open(cx);
        cx.update(|window, cx| {
            ws.update(cx, |ws, cx| {
                ws.run(
                    super::Act::Theme(Choice::Builtin(ThemeId::Emerald)),
                    window,
                    cx,
                );
                assert!(cx.theme().is_dark());
                assert_eq!(ws.theme_choice(false), Choice::Builtin(ThemeId::Emerald));
                assert_eq!(
                    ws.pal(cx).accent,
                    Pal::for_theme(ThemeId::Emerald, true).accent
                );
            });
        });

        let saved: serde_json::Value =
            serde_json::from_slice(&std::fs::read(greencode_core::home().join("ui.json")).unwrap())
                .unwrap();
        assert_eq!(saved["theme_light"], json!("emerald"));

        cx.simulate_input("/theme paper");
        cx.simulate_keystrokes("enter");
        ws.read_with(cx, |ws, cx| {
            assert!(!cx.theme().is_dark());
            assert_eq!(ws.theme_choice(false), Choice::Builtin(ThemeId::Paper));
            assert_eq!(
                ws.pal(cx).panel,
                Pal::for_theme(ThemeId::Paper, false).panel
            );
        });

        cx.simulate_input("/theme");
        cx.simulate_keystrokes("enter");
        assert!(ws.read_with(cx, |ws, cx| {
            ws.palette
                .as_ref()
                .is_some_and(|p| p.0.read(cx).value() == "テーマ: ")
        }));
        cx.simulate_keystrokes("escape");

        cx.update(|window, cx| {
            ws.update(cx, |ws, cx| {
                ws.run(super::Act::ThemeMode(themes::ThemeMode::Dark), window, cx);
                ws.run(super::Act::Theme(Choice::Builtin(ThemeId::Graphite)), window, cx);
                assert_eq!(ws.theme_choice(false), Choice::Builtin(ThemeId::Graphite));
                ws.run(super::Act::ThemeMode(themes::ThemeMode::System), window, cx);
            });
        });

        let saved: serde_json::Value =
            serde_json::from_slice(&std::fs::read(greencode_core::home().join("ui.json")).unwrap())
                .unwrap();
        assert_eq!(saved["theme_dark"], json!("graphite"));
        assert_eq!(saved["theme_light"], json!("paper"));

        cx.update(|window, cx| {
            let restored = cx.new(|cx| Workspace::new(Config::default(), None, window, cx));
            restored.read_with(cx, |ws, cx| {
                assert!(!cx.theme().is_dark());
                assert_eq!(ws.theme_choice(false), Choice::Builtin(ThemeId::Paper));
                assert_eq!(ws.theme_choice(true), Choice::Builtin(ThemeId::Graphite));
            });

            std::fs::write(greencode_core::home().join("ui.json"), r#"{"theme":true}"#).unwrap();
            let legacy_dark = cx.new(|cx| Workspace::new(Config::default(), None, window, cx));
            legacy_dark.read_with(cx, |ws, cx| {
                assert!(cx.theme().is_dark());
                assert_eq!(ws.theme_choice(false), Choice::Builtin(ThemeId::Dark));
                assert_eq!(ws.theme_mode(), themes::ThemeMode::Dark);
            });

            std::fs::create_dir_all(greencode_core::home().join("themes")).unwrap();
            std::fs::write(
                greencode_core::home().join("themes/night.json"),
                r##"{"name":"Night","appearance":"dark","panel":"#101820","accent":"#00FF00"}"##,
            )
            .unwrap();
            std::fs::write(
                greencode_core::home().join("ui.json"),
                r#"{"theme_mode":"system","theme_light":"night"}"#,
            )
            .unwrap();
            let custom = cx.new(|cx| Workspace::new(Config::default(), None, window, cx));
            custom.read_with(cx, |ws, cx| {
                assert_eq!(ws.theme_choice(false), Choice::Custom("night".into()));
                let expected: gpui_kit::Hsla = gpui_kit::rgba(0x00FF00FF).into();
                assert_eq!(ws.pal(cx).accent, expected);
            });
        });
    }

    #[gpui::test]
    async fn project_pins_keep_order_and_archive_moves_them_out(cx: &mut TestAppContext) {
        let (ws, cx) = open(cx);
        let (a, b) = (
            greencode_core::home().join("a"),
            greencode_core::home().join("b"),
        );
        ws.update(cx, |ws, _| {
            ws.toggle_project("pinned_projects", &a);
            ws.toggle_project("pinned_projects", &b);
            assert_eq!(ws.pinned_projects(), vec![a.clone(), b.clone()]);
            ws.toggle_project("pinned_projects", &a);
            assert_eq!(ws.pinned_projects(), vec![b.clone()]);
            ws.toggle_project("archived_projects", &b);
            assert!(ws.project_archived(&b));
            // Pinned projects lead the known list; the active thread's project follows.
            assert_eq!(ws.known_projects().first(), Some(&b));
            assert!(ws.project_acts().iter().any(|(label, _)| label.starts_with("プロジェクトを復元: b")));

            // Worktree branch prefixes are normalized when read; defaults survive an empty ui.json.
            assert_eq!(ws.worktree_options().prefix, "greencode/");
            ws.ui["worktree_prefix"] = serde_json::json!("feat");
            assert_eq!(ws.worktree_options().prefix, "feat/");
            ws.ui["worktree_submodules"] = serde_json::json!("skip");
            assert_eq!(
                ws.worktree_options().submodules,
                greencode_core::worktree::Submodules::Skip
            );
        });
    }

    #[gpui_kit::test]
    async fn text_generation_model_cycles_through_the_picker_and_off(cx: &mut TestAppContext) {
        let (ws, cx) = open(cx);
        ws.update_in(cx, |ws, _, _| {
            let models = ws.models();
            assert!(!models.is_empty());
            ws.set_text_model(None);
            ws.cycle_text_model();
            assert_eq!(ws.text_model(), Some(models[0].clone()));
            // A model that left the picker cycles back to off instead of staying selected.
            ws.set_text_model(Some("missing/model".into()));
            ws.cycle_text_model();
            assert_eq!(ws.text_model(), None);
            ws.set_text_model(None);
        });
        cx.run_until_parked();
    }
}

#[cfg(test)]
mod walk_tests {
    #[test]
    fn walk_lists_plain_directories_without_hidden_or_build_dirs() {
        let dir = std::env::temp_dir().join(format!("greencode-walk-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        for d in ["src", "target", "node_modules", ".git"] {
            std::fs::create_dir_all(dir.join(d)).unwrap();
            std::fs::write(dir.join(d).join("x.rs"), "").unwrap();
        }
        std::fs::write(dir.join("hello.txt"), "").unwrap();
        let mut out = vec![];
        super::walk_files(&dir, "", 0, &mut out);
        out.sort();
        assert_eq!(out, ["hello.txt", "src/x.rs"]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
