mod display;
mod input;
mod keymap;
mod review;
mod startup;
mod theme;
mod view;
mod vim;

use crate::dialog::Dialog;
use anyhow::{bail, Context, Result};
use crossterm::{
    event::{self, DisableBracketedPaste, EnableBracketedPaste, Event as Input, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    execute,
    terminal::{self, EnterAlternateScreen, LeaveAlternateScreen},
};
use greencode_core::{
    config::{settings, Config, DoubleEscape, FollowUp, LoopMode, OnOff, TitleGenerator, TitleSpinner, TuiCfg, WorktreeScope},
    permission::{Mode, Perms}, session::{Meta, Session}, update, Block, Command,
    Decision, Event, Message, Role, Usage,
};
use input::{Edit, Editor};
use keymap::{Action, Keymap};
use ratatui::{backend::CrosstermBackend, Terminal};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, VecDeque}, io::{self, Stdout, Write}, path::{Path, PathBuf},
    sync::Arc, time::{Duration, Instant},
};
use tokio::{runtime::Handle, sync::{mpsc, oneshot}};
use view::{Kind, Log};

const COMMANDS: &[(&str, &str)] = &[
    ("help", "Keys and slash commands"),
    ("model", "Select a model"),
    ("thinking", "Set reasoning effort"),
    ("mode", "Set permission mode (ask / auto-edit / full / plan)"),
    ("plan", "Toggle read-only plan mode"),
    ("goal", "Show, pause, resume or clear this session's goal: /goal [pause|resume|clear|TOKEN_BUDGET N]"),
    ("new", "Start a thread"),
    ("resume", "Resume a saved thread"),
    ("fork", "Branch off the current thread (summarize it first with branchSummary)"),
    ("add-dir", "Add an extra workspace root: /add-dir PATH"),
    ("remove-dir", "Remove an extra workspace root: /remove-dir PATH"),
    ("dirs", "List the workspace roots in force"),
    ("session", "Show the current thread ID"),
    ("wt", "Check out this repository into a worktree and continue there: /wt [BRANCH_NAME]"),
    ("export", "Save this transcript as local HTML: /export [PATH]"),
    ("compact", "Compact context (summary / snap)"),
    ("ratchet", "Hill-climb an eval flow with the ratchet tool: /ratchet GOAL"),
    ("skill", "Load a skill into the composer"),
    ("skillful", "Toggle listing skills in the system prompt for this session"),
    ("role", "Assign a model role: /role NAME provider/model (or /role NAME - to clear)"),
    ("prewalk", "Plan on this model, then hand off at the first edit: /prewalk [restart|MODEL|off]"),
    ("agents", "List sub-agent activity"),
    ("instances", "Other GreenCode instances in this folder and their sub-agents"),
    ("message", "Message another instance or agent: /message SESSION_OR_ALL TEXT"),
    ("jobs", "Background jobs and their live widgets"),
    ("to", "Steer an active sub-agent: /to NAME TEXT"),
    ("stop", "Cancel an active sub-agent: /stop NAME"),
    ("queue", "List or run held follow-ups: /queue [run|clear]"),
    ("loop", "Repeat a prompt: /loop [--while CMD|--until CMD] PROMPT (or /loop stop)"),
    ("exit", "Quit GreenCode"),
    ("history", "Search prompt history"),
    ("hotkeys", "Show active application shortcuts"),
    ("settings", "Edit terminal preferences"),
    ("review", "Review a Git diff: /review [PR_OR_MR_URL] [FOCUS]"),
    ("collab", "Share this session on collab.relayUrl: /collab [start [view|control]|stop|link]"),
    ("share", "Upload an encrypted transcript: /share [THREAD]"),
];
const KEYS: &str = "Enter Send · Alt+Enter Newline · Tab Complete · Esc Stop · PgUp/PgDn Scroll · /hotkeys";
const TUI_SETTINGS: &[&str] = &[
    "ask.notify", "ask.timeout", "auto_resume", "autocomplete_max_visible", "color_blind_mode", "colorful_markdown", "composer.prediction_alternatives",
    "composer.predictions", "composer.shape", "composer.token_rate", "cycle_order", "display.cache_miss_marker",
    "display.smooth_streaming", "double_escape_action", "emoji_autocomplete", "expand_thinking_blocks",
    "expand_tools", "follow_up", "git.enabled", "hide_thinking", "hide_tools", "ime_safe_cursor", "input.bare_exit_on_empty_session",
    "input.bare_slash_commands", "keybindings", "loop.condition_timeout_ms", "loop.mode", "max_fps", "mcp.render_markdown_results",
    "motion", "notify.completion", "notify.error", "paste.large_menu_threshold", "plan.autosave", "plan.autosave_dir",
    "prose_only_thinking", "read.render_markdown", "read.tool_result_preview", "recall_cleared_drafts", "session.worktree_scope",
    "show_hardware_cursor", "show_terminal_progress", "show_turn_time", "show_turn_usage", "show_usage", "skills.enable_skill_commands",
    "spinner_interval", "symbol_preset", "terminal.program_status", "theme", "theme_dark", "theme_light", "tight",
    "title.generator", "title.refresh_after_prompts", "title_state", "title_spinner", "update.auto", "update.channel", "update.check",
];

struct Screen {
    terminal: Terminal<CrosstermBackend<Stdout>>,
    progress: bool,
    program: Option<&'static str>,
    title: String,
    program_status: bool,
}

impl Screen {
    fn open(show_cursor: bool, program_status: bool, mouse: bool) -> Result<Self> {
        terminal::enable_raw_mode()?;
        let result = (|| {
            execute!(io::stdout(), EnterAlternateScreen, EnableBracketedPaste)?;
            if mouse { execute!(io::stdout(), crossterm::event::EnableMouseCapture)?; }
            if !show_cursor { execute!(io::stdout(), crossterm::cursor::Hide)?; }
            Terminal::new(CrosstermBackend::new(io::stdout()))
                .map(|terminal| Self { terminal, progress: false, program: None, title: String::new(), program_status })
        })();
        if result.is_err() { Self::restore(); }
        Ok(result?)
    }

    fn set_progress(&mut self, running: bool) -> io::Result<()> {
        if self.progress != running {
            let mut out = io::stdout().lock();
            out.write_all(if running { b"\x1b]9;4;3\x1b\\" } else { b"\x1b]9;4;0\x1b\\" })?;
            out.flush()?;
            self.progress = running;
        }
        Ok(())
    }

    fn set_program_status(&mut self, status: &'static str) -> io::Result<()> {
        if !self.program_status || self.program == Some(status) { return Ok(()); }
        let mut out = io::stdout().lock();
        write!(out, "\x1b]7501;{status}\x07")?;
        out.flush()?;
        self.program = Some(status);
        Ok(())
    }

    fn set_title(&mut self, title: &str) -> io::Result<()> {
        if self.title == title { return Ok(()); }
        let safe: String = title.chars().filter(|c| !c.is_control()).collect();
        let mut out = io::stdout().lock();
        write!(out, "\x1b]2;{safe}\x07")?;
        out.flush()?;
        self.title = title.to_owned();
        Ok(())
    }

    fn restore() {
        let _ = execute!(io::stdout(), crossterm::event::DisableMouseCapture, DisableBracketedPaste, LeaveAlternateScreen, crossterm::cursor::Show);
        let _ = terminal::disable_raw_mode();
    }
}

impl Drop for Screen {
    fn drop(&mut self) {
        let _ = self.set_progress(false);
        let _ = self.set_program_status("done");
        let _ = self.set_title("GreenCode");
        Self::restore();
    }
}

fn notify(category: OnOff, body: &str) {
    if !category.on() { return; }
    let body: String = body.chars().filter(|c| !c.is_control()).take(200).collect();
    let mut out = io::stdout().lock();
    let _ = write!(out, "\x1b]777;notify;GreenCode;{body}\x07");
    let _ = out.flush();
}

fn animation_interval(cfg: &TuiCfg) -> u64 {
    cfg.spinner_interval.unwrap_or(match cfg.motion {
        greencode_core::config::Motion::Full => 100,
        greencode_core::config::Motion::Reduced => 200,
        greencode_core::config::Motion::None => 0,
    })
}

fn mode_name(mode: Mode) -> &'static str {
    match mode { Mode::Ask => "ask", Mode::AutoEdit => "auto-edit", Mode::Full => "full", Mode::Plan => "plan" }
}

fn title_spinner(kind: TitleSpinner, elapsed: Duration, interval: u64) -> &'static str {
    const BRAILLE: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
    const PULSE: [&str; 8] = ["🌑", "🌒", "🌓", "🌔", "🌕", "🌖", "🌗", "🌘"];
    const DOTS: [&str; 4] = ["·", "•", "∙", "●"];
    const LINE: [&str; 4] = ["-", "\\", "|", "/"];
    let frames: &[&str] = match kind { TitleSpinner::Braille => &BRAILLE, TitleSpinner::Pulse => &PULSE, TitleSpinner::Dots => &DOTS, TitleSpinner::Line => &LINE };
    if interval == 0 { return frames[0]; }
    frames[(elapsed.as_millis() / u128::from(interval)) as usize % frames.len()]
}

/// `startup.setupWizard`: a short first-run flow that chooses the default model and writes it to config.toml.
struct Wizard {
    choices: Vec<String>,
    selected: usize,
}

impl Wizard {
    fn new(cfg: &Config) -> Self {
        // Models of providers that already have a key or base URL; the preset list is always a valid choice.
        let mut choices: Vec<String> = cfg.model_list();
        choices.retain(|model| !model.trim().is_empty());
        choices.truncate(8);
        Self { choices, selected: 0 }
    }

    fn selected_model(&self) -> Option<&str> {
        self.choices.get(self.selected).map(String::as_str)
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Picker { Model, Session, Mode, Effort, Skill, History, Settings, Paste, Review(String), ReviewBase(String), ReviewCommit(String) }

#[derive(Clone, Copy, PartialEq, Eq)]
enum Submit { Default, Steer, Queue }

struct Choice {
    value: String,
    detail: String,
}

struct Menu {
    kind: Picker,
    choices: Vec<Choice>,
    saved_draft: Editor,
}

enum Pending {
    Permission { label: String, reply: oneshot::Sender<Decision>, offset: usize, shown: bool, since: Instant },
    Dialog { dialog: Dialog, reply: oneshot::Sender<Value>, input: Editor, offset: usize, since: Instant },
}

impl Pending {
    fn closed(&self) -> bool {
        match self {
            Self::Permission { reply, .. } => reply.is_closed(),
            Self::Dialog { reply, .. } => reply.is_closed(),
        }
    }
}

#[derive(Clone)]
struct LoopState {
    prompt: String,
    /// Shell command and whether the loop continues while it succeeds (`--while`) or until it does (`--until`).
    condition: Option<(String, bool)>,
    /// Set while the extra `/compact` turn between iterations is running.
    compacting: bool,
    /// False until the first prompt is sent (a condition is checked before it too).
    started: bool,
}

struct App<'a> {
    rt: &'a Handle,
    cfg: Arc<Config>,
    tui: TuiCfg,
    persist_settings: bool,
    theme: theme::Theme,
    keymap: Keymap,
    meta: Meta,
    perms: Arc<Perms>,
    tx: mpsc::UnboundedSender<Command>,
    rx: mpsc::UnboundedReceiver<Event>,
    log: Log,
    input: Editor,
    vim: vim::Vim,
    /// Idle-recap bookkeeping: when the session last went idle and whether it showed its recap.
    idle_since: Instant,
    recap_shown: bool,
    menu: Option<Menu>,
    selected: usize,
    scroll: usize,
    history: Vec<String>,
    // The submitted history length at each clear anchors that draft in chronological recall.
    cleared_drafts: VecDeque<(usize, String)>,
    recalled: Option<usize>,
    draft: String,
    busy: Option<Instant>,
    fetching_review: Option<tokio::task::JoinHandle<Result<(String, String)>>>,
    cancelling: bool,
    held: bool,
    failed: bool,
    queue: VecDeque<String>,
    pending: VecDeque<Pending>,
    usage: Usage,
    turn_usage: Option<Usage>,
    last_context: u64,
    commands: Vec<String>,
    agents: BTreeMap<String, (String, String, String)>,
    pre_plan: Mode,
    exit: bool,
    external_edit: bool,
    git: Option<(String, bool)>,
    last_tok_rate: Option<f64>,
    streamed: u64,
    last_escape: Option<Instant>,
    bare_confirm: Option<String>,
    loop_state: Option<LoopState>,
    loop_check: Option<tokio::task::JoinHandle<Result<bool>>>,
    paste: Option<String>,
    tool_names: BTreeMap<String, String>,
    update_rx: Option<mpsc::UnboundedReceiver<String>>,
    title_job: Option<tokio::task::JoinHandle<Result<String>>>,
    generated_title: bool,
    /// The last plan the `todo` tool wrote; a replacement refreshes the title when `title.refresh_on_replan`.
    last_plan: Option<String>,
    /// When `task.cross_instance` last polled the folder inbox.
    inbox_at: Instant,
    // Image waiting for a draw to place it, and the rows the history area must reserve for it.
    inline_image: Option<(String, Vec<u8>)>,
    inline_rows: u16,
    inline_emitted: bool,
    /// Speaks the assistant's output when `speech.enabled`; `None` when off.
    voice: Option<greencode_core::speech::Vocalizer>,
    prompts: u32,
    predicting: Option<tokio::task::JoinHandle<Result<Vec<String>>>>,
    recap_job: Option<tokio::task::JoinHandle<Result<String>>>,
    predictions: Vec<String>,
    prediction_index: usize,
    collab: Option<greencode_core::collab::Hosting>,
    collab_prompts: Option<mpsc::UnboundedReceiver<String>>,
    sharing: Option<tokio::task::JoinHandle<Result<String>>>,
    /// Latest hook notice and when it arrived; shown under the status line while `show_hook_status`.
    hook_status: Option<(String, Instant)>,
    /// `tui.mouse`: the screen row where view drew the first pinned agent, for click hit-testing.
    pin_top: Option<u16>,
    /// `tui.mouse`: the pinned agent a click focused; its row stays highlighted.
    agent_focus: Option<String>,
    /// `display.advisorCollapsedLines`: this turn's advisor notes, shown as one collapsible card.
    advisor_notes: Vec<String>,
    /// Codex reset celebration: `Some` draws the fireworks overlay until Escape.
    fireworks: Option<Instant>,
    /// The command of the running bash call, for the shell visualization box.
    tool_command: Option<String>,
    /// `spelling.*`: the checker is connected in the background after the first draw.
    spell: greencode_core::spelling::Spell,
    typos: Vec<std::ops::Range<usize>>,
    /// Cached completion `(word range, word)` for the draft, shown as ghost text and accepted with Tab.
    completion: Option<(std::ops::Range<usize>, String)>,
    /// Input the spelling state was computed for, so it is recomputed only when it changes.
    spell_key: Option<(String, usize)>,
    /// `startup.showSplash`: when the start began; the splash draws for a short moment.
    splash: Option<Instant>,
    /// `startup.setupWizard`: the step the wizard shows; `None` once dismissed or done.
    wizard: Option<Wizard>,
    /// `marketplace.autoUpdate`: startup update notices.
    marketplace_rx: Option<mpsc::UnboundedReceiver<String>>,
    /// Background `swiftc` build and connect of the spelling helper.
    spell_job: Option<tokio::task::JoinHandle<Result<Box<dyn greencode_core::spelling::Checker>>>>,
    /// `tui.renderSvg`/`renderEmbeds`: a figure being fetched and rasterized for the inline slot.
    figures: Option<tokio::task::JoinHandle<Option<(String, Vec<u8>)>>>,
}

pub(super) fn run(rt: &Handle, cfg: Config, meta: Meta, persist_settings: bool) -> Result<()> {
    // Claims this pane for the browser tool: tabs opened by a nested session stay out of it.
    std::env::set_var("GREENCODE_TERMINAL_OWNER_PID", std::process::id().to_string());
    let theme = theme::Theme::load_preferred(&cfg.tui)?;
    let keymap = Keymap::load(&cfg.tui.keybindings)?;
    let tui = cfg.tui.clone();
    let (show_cursor, mouse) = (tui.show_hardware_cursor, tui.display.mouse);
    let program_status = tui.terminal.program_status;
    // `startup.quiet` silences startup status messages; `startup.changelogMode = hidden` does too.
    let quiet = cfg.interaction_opts.startup.quiet
        || cfg.interaction_opts.startup.changelog_mode == greencode_core::config::interaction::ChangelogMode::Hidden;
    let update_rx = if (tui.update.check || tui.update.auto) && !quiet {
        let (tx, rx) = mpsc::unbounded_channel();
        let (repo, channel, install) = (cfg.update_repo.clone(), tui.update.channel, tui.update.auto);
        rt.spawn(async move {
            let Ok(repo) = update::repository(repo.as_deref(), None) else { return };
            let executable = install.then(std::env::current_exe).transpose().ok().flatten();
            if let Ok(result) = update::run(&repo, channel, executable.as_deref()).await {
                let text = if result.installed {
                    format!("GreenCode {} installed; restart to use it.", result.latest)
                } else if result.available {
                    format!("GreenCode {} is available; run `greencode-cli update` to install it.", result.latest)
                } else {
                    String::new()
                };
                let _ = tx.send(text);
            }
        });
        Some(rx)
    } else { None };
    let git = tui.git.enabled.then(|| git_status(&meta.cwd)).flatten();
    let vim_mode = tui.vim_mode;
    let voice = greencode_core::speech::Vocalizer::new(&cfg);
    let (spelling, startup, home) = (
        cfg.interaction_opts.spelling.clone(),
        cfg.interaction_opts.startup.clone(),
        greencode_core::home(),
    );
    let marketplace = startup.marketplace_auto_update;
    let cfg = Arc::new(cfg);
    let perms = Arc::new(Perms::new(cfg.mode));
    let (tx, rx) = greencode_core::spawn(rt, cfg.clone(), meta.clone(), perms.clone());
    let mut app = App {
        rt, cfg, tui, persist_settings, theme, keymap, meta, perms, tx, rx,
        log: Log::default(), input: Editor::default(), vim: vim::Vim::new(vim_mode),
        idle_since: Instant::now(), recap_shown: true, menu: None, selected: 0, scroll: 0,
        history: Vec::new(), recalled: None, draft: String::new(), busy: None, cancelling: false,
        cleared_drafts: VecDeque::new(),
        fetching_review: None,
        held: false, failed: false, queue: VecDeque::new(), pending: VecDeque::new(),
        usage: Usage::default(), turn_usage: None, last_context: 0, commands: Vec::new(), agents: BTreeMap::new(),
        pre_plan: Mode::Ask, exit: false, external_edit: false,
        git, last_tok_rate: None, streamed: 0, last_escape: None, bare_confirm: None,
        loop_state: None, loop_check: None, paste: None, tool_names: BTreeMap::new(), update_rx,
        title_job: None, generated_title: false, last_plan: None, inbox_at: Instant::now(),
        voice, prompts: 0, predicting: None, recap_job: None, predictions: Vec::new(), prediction_index: 0,
        collab: None, collab_prompts: None, sharing: None, hook_status: None, pin_top: None, agent_focus: None, advisor_notes: Vec::new(), fireworks: None, tool_command: None, figures: None,
        spell: greencode_core::spelling::Spell::new(spelling), typos: Vec::new(), completion: None, spell_key: None,
        splash: startup::splash_due(&startup).then(Instant::now), wizard: None,
        marketplace_rx: None, spell_job: None,
        inline_image: None, inline_rows: 0, inline_emitted: false,
    };
    // `startup.setupWizard`: onboarding is due until the stored setup version reaches this build's.
    app.wizard = startup::wizard_due(&startup, &home).then(|| Wizard::new(&app.cfg));
    if marketplace != greencode_core::config::interaction::MarketplaceUpdate::Off && !quiet {
        let (tx, rx) = mpsc::unbounded_channel();
        app.marketplace_rx = Some(rx);
        rt.spawn(async move {
            if let Some(message) = marketplace_updates(marketplace).await {
                let _ = tx.send(message);
            }
        });
    }
    if app.spell.enabled() {
        app.spell_job = Some(rt.spawn_blocking(greencode_core::spelling::connect));
    }
    app.load_display()?;
    // `plan.defaultOnStartup` enters plan mode at the start of every new session, unless a saved thread resumes.
    if app.cfg.modes.startup_plan() && app.history.is_empty() {
        app.perms.set_mode(Mode::Plan);
        app.pre_plan = app.cfg.mode;
    }
    app.start_collab();
    // `task.crossInstance`: other instances in this folder list this session and can message it.
    if app.cfg.task.cross_instance {
        let _ = greencode_core::instance::register(&app.meta.cwd, &app.meta.id, &[]);
    }
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| { Screen::restore(); previous(info); }));
    let mut screen = Screen::open(show_cursor, program_status, mouse)?;
    let mut dirty = true;
    let mut tick = Instant::now();
    let mut last_draw: Option<Instant> = None;
    while !app.exit {
        if app.cfg.task.cross_instance && app.inbox_at.elapsed() >= Duration::from_secs(1) {
            app.inbox_at = Instant::now();
            let running: Vec<String> = app.agents.iter().filter(|(_, (state, _, _))| state == "running").map(|(name, _)| name.clone()).collect();
            let _ = greencode_core::instance::register(&app.meta.cwd, &app.meta.id, &running);
            for message in greencode_core::instance::take_inbox(&app.meta.cwd, &app.meta.id) {
                let text = format!("Message from {}: {}", message.from, message.text);
                app.log.add(Kind::User, text.clone());
                let result = if app.busy.is_none() && !app.held { app.prompt(text) } else { app.queue.push_back(text); Ok(()) };
                if let Err(error) = result { app.log.add(Kind::Error, format!("{error:#}")); }
            }
            dirty = true;
        }
        for _ in 0..256 {
            match app.rx.try_recv() {
                Ok(ev) => { app.event(ev); dirty = true; }
                Err(mpsc::error::TryRecvError::Empty) => break,
                Err(mpsc::error::TryRecvError::Disconnected) => bail!("agent runtime stopped"),
            }
        }
        if app.fetching_review.as_ref().is_some_and(tokio::task::JoinHandle::is_finished) {
            let job = app.fetching_review.take().unwrap();
            app.busy = None;
            let result = rt.block_on(job).context("Remote review fetch stopped").and_then(|result| result)
                .and_then(|(label, prompt)| app.start_review(label, prompt, "remote"));
            if let Err(error) = result { app.log.add(Kind::Error, format!("{error:#}")); }
            dirty = true;
        }
        if app.loop_check.as_ref().is_some_and(tokio::task::JoinHandle::is_finished) {
            let job = app.loop_check.take().unwrap();
            let result = rt.block_on(job).context("Loop condition check stopped").and_then(|result| result);
            if let Err(error) = app.loop_checked(result) { app.log.add(Kind::Error, format!("{error:#}")); }
            dirty = true;
        }
        if app.figures.as_ref().is_some_and(|job| job.is_finished()) {
            if let Some(Some((media, bytes))) = app.figures.take().and_then(|job| rt.block_on(job).ok()) {
                app.inline_image = Some((media, bytes));
                app.inline_emitted = false;
            }
            dirty = true;
        }
        if app.title_job.as_ref().is_some_and(tokio::task::JoinHandle::is_finished) {
            let job = app.title_job.take().unwrap();
            match rt.block_on(job) {
                Ok(Ok(title)) => app.set_title(greencode_core::title_icons::head(app.tui.title.icons, &app.meta.cwd, &title)),
                Ok(Err(error)) => app.log.add(Kind::Notice, format!("Title generation failed: {error:#}")),
                Err(error) => app.log.add(Kind::Notice, format!("Title generation stopped: {error}")),
            }
            dirty = true;
        }
        if app.sharing.as_ref().is_some_and(tokio::task::JoinHandle::is_finished) {
            let job = app.sharing.take().unwrap();
            match rt.block_on(job) {
                Ok(Ok(link)) => app.log.add(Kind::Notice, format!("Share link (key is in the #fragment):\n{link}")),
                Ok(Err(error)) => app.log.add(Kind::Error, format!("Share failed: {error:#}")),
                Err(error) => app.log.add(Kind::Error, format!("Share stopped: {error}")),
            }
            dirty = true;
        }
        if let Some(mut rx) = app.collab_prompts.take() {
            let mut prompts = Vec::new();
            while let Ok(text) = rx.try_recv() { prompts.push(text); }
            for text in prompts {
                app.log.add(Kind::User, format!("[collab] {text}"));
                let result = if app.busy.is_none() && !app.held { app.prompt(text) } else { app.queue.push_back(text); Ok(()) };
                if let Err(error) = result { app.log.add(Kind::Error, format!("{error:#}")); }
            }
            app.collab_prompts = Some(rx);
            dirty = true;
        }
        if app.predicting.as_ref().is_some_and(tokio::task::JoinHandle::is_finished) {
            let job = app.predicting.take().unwrap();
            match rt.block_on(job) {
                Ok(Ok(mut list)) => { list.truncate(4); app.predictions = list; }
                Ok(Err(_)) | Err(_) => app.predictions.clear(),
            }
            dirty = true;
        }
        if let Some(rx) = app.update_rx.as_mut() {
            let mut messages = Vec::new();
            while let Ok(text) = rx.try_recv() { messages.push(text); }
            for text in messages.into_iter().filter(|text| !text.is_empty()) {
                app.log.add(Kind::Notice, text);
                dirty = true;
            }
        }
        // `spelling.*`: the helper connects off the UI thread; its words and completion start applying once ready.
        if app.spell_job.as_ref().is_some_and(tokio::task::JoinHandle::is_finished) {
            let job = app.spell_job.take().unwrap();
            match rt.block_on(job) {
                Ok(Ok(checker)) => { app.spell.use_checker(checker); app.spell_key = None; }
                Ok(Err(error)) => app.log.add(Kind::Notice, format!("Spell checking unavailable: {error:#}")),
                Err(error) => app.log.add(Kind::Notice, format!("Spell checking stopped: {error}")),
            }
            dirty = true;
        }
        app.refresh_spelling();
        if let Some(rx) = app.marketplace_rx.as_mut() {
            let mut messages = Vec::new();
            while let Ok(text) = rx.try_recv() { messages.push(text); }
            for text in messages {
                app.log.add(Kind::Notice, text);
                dirty = true;
            }
        }
        // `startup.showSplash`: hold the splash for a short moment, then the normal screen takes over.
        if app.splash.is_some_and(|at| at.elapsed() >= Duration::from_millis(900)) {
            app.splash = None;
            dirty = true;
        }
        app.schedule_recap();
        if app.recap_job.as_ref().is_some_and(|job| job.is_finished()) {
            let job = app.recap_job.take().unwrap();
            match rt.block_on(job) {
                Ok(Ok(text)) => app.log.add(Kind::Notice, format!("Recap\n{text}")),
                Ok(Err(error)) => app.log.add(Kind::Error, format!("Recap failed: {error:#}")),
                Err(_) => {}
            }
            dirty = true;
        }
        let before = app.pending.len();
        app.pending.retain(|p| !p.closed());
        dirty |= before != app.pending.len();
        if app.expire_pending() { dirty = true; }
        if app.busy.is_some() && tick.elapsed() >= Duration::from_millis(100) {
            dirty = true;
            tick = Instant::now();
        }
        dirty |= app.log.reveal();
        let frame = (app.tui.max_fps > 0).then(|| Duration::from_millis((1000 / u64::from(app.tui.max_fps)).max(1)));
        if dirty && frame.is_none_or(|frame| last_draw.is_none_or(|at| at.elapsed() >= frame)) {
            screen.set_progress(app.tui.show_terminal_progress && app.busy.is_some() && app.pending.is_empty())?;
            let mut inline = None;
            screen.terminal.draw(|frame| inline = view::draw(frame, &mut app))?;
            app.emit_inline(inline)?;
            screen.set_title(&app.terminal_title())?;
            screen.set_program_status(app.program_status())?;
            last_draw = Some(Instant::now());
            dirty = false;
        }
        if event::poll(Duration::from_millis(25))? {
            match event::read()? {
                Input::Key(key) if key.kind != KeyEventKind::Release => {
                    if let Err(err) = app.key(key) { app.log.add(Kind::Error, format!("{err:#}")); }
                    if std::mem::take(&mut app.external_edit) {
                        let editor = ["VISUAL", "EDITOR"].into_iter().filter_map(|name| std::env::var(name).ok())
                            .find(|value| !value.trim().is_empty()).or_else(|| cfg!(windows).then(|| "notepad".into()));
                        if let Some(editor) = editor {
                            drop(screen);
                            let edited = edit_draft(editor.trim(), &app.meta.cwd, app.input.text());
                            screen = Screen::open(show_cursor, program_status, mouse)?;
                            match edited {
                                Ok(text) => {
                                    if text != app.input.text() {
                                        app.input.replace(text);
                                        app.recalled = None;
                                        app.selected = 0;
                                    }
                                }
                                Err(err) => app.log.add(Kind::Error, format!("{err:#}")),
                            }
                        } else {
                            app.log.add(Kind::Notice, "Set $VISUAL or $EDITOR to edit the draft externally.".into());
                        }
                    }
                }
                Input::Resize(_, _) => app.log.resized(app.tui.display.resize_scrollback),
                Input::Mouse(mouse) if app.tui.display.mouse => {
                    match mouse.kind {
                        crossterm::event::MouseEventKind::ScrollUp => app.scroll = app.scroll.saturating_add(3),
                        crossterm::event::MouseEventKind::ScrollDown => app.scroll = app.scroll.saturating_sub(3),
                        // Clicking a pinned agent row focuses that agent's transcript.
                        crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left) => app.click(mouse.row),
                        _ => {}
                    }
                }
                Input::Paste(_) if app.wizard.is_some() => (),
                Input::Paste(text) => {
                    match app.pending.front_mut() {
                        Some(Pending::Dialog { input, .. }) => input.insert(&text),
                        Some(Pending::Permission { .. }) => (),
                        None => app.paste(text),
                    }
                }
                _ => (),
            }
            dirty = true;
        }
    }
    Ok(())
}

/// The `startup.setupWizard` screen: a title, the choices with the selected one marked, and a hint.
fn wizard_rows(wizard: &Wizard) -> Option<Vec<String>> {
    let mut rows = vec!["Welcome to GreenCode".to_string(), String::new(), "Choose the default model (↑↓, Enter; Esc skips):".to_string()];
    if wizard.choices.is_empty() {
        rows.push("No models configured; add a provider with /model or config.toml.".to_string());
    }
    for (i, model) in wizard.choices.iter().enumerate() {
        rows.push(format!("{} {model}", if i == wizard.selected { ">" } else { " " }));
    }
    Some(rows)
}

/// `marketplace.autoUpdate`: check the registry at startup; `auto` also installs the newer versions.
async fn marketplace_updates(mode: greencode_core::config::interaction::MarketplaceUpdate) -> Option<String> {
    use greencode_core::registry;
    let installed = registry::installed();
    if installed.is_empty() {
        return None;
    }
    let entries = registry::parse(&registry::fetch().await.ok()?).ok()?;
    let plan = registry::update_plan(mode, registry::outdated(&installed, &entries));
    if plan.entries.is_empty() {
        return None;
    }
    if !plan.install {
        let names: Vec<&str> = plan.entries.iter().map(|entry| entry.name.as_str()).collect();
        return Some(format!("Plugin updates available: {} · `greencode-cli agents install ID`", names.join(", ")));
    }
    let mut installed_now = Vec::new();
    let mut failed = Vec::new();
    for entry in &plan.entries {
        match registry::install(entry).await {
            Ok(_) => installed_now.push(entry.name.clone()),
            Err(error) => failed.push(format!("{}: {error:#}", entry.name)),
        }
    }
    let mut message = String::new();
    if !installed_now.is_empty() {
        message.push_str(&format!("Updated plugins: {}", installed_now.join(", ")));
    }
    if !failed.is_empty() {
        if !message.is_empty() { message.push_str(" · "); }
        message.push_str(&format!("Plugin update failed: {}", failed.join("; ")));
    }
    Some(message)
}

fn edit_draft(editor: &str, cwd: &std::path::Path, draft: &str) -> Result<String> {
    let mut builder = tempfile::Builder::new();
    builder.prefix("greencode-editor-");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        builder.permissions(std::fs::Permissions::from_mode(0o700));
    }
    let temp = builder.tempdir()?;
    let file = std::path::absolute(temp.path())?.join("draft.md");
    std::fs::write(&file, draft)?;
    #[cfg(not(windows))]
    let mut child = {
        let mut cmd = std::process::Command::new("sh");
        cmd.arg("-c").arg(format!("{editor} \"$1\"")).arg("sh").arg(&file);
        cmd
    };
    #[cfg(windows)]
    let mut child = {
        use std::os::windows::process::CommandExt;
        let mut cmd = std::process::Command::new("cmd.exe");
        cmd.args(["/d", "/s", "/c"]).raw_arg(format!("\"{editor} \"{}\"\"", file.display()));
        cmd
    };
    let status = child.current_dir(cwd).status().context("Cannot launch external editor")?;
    anyhow::ensure!(status.success(), "External editor exited with {status}; draft kept");
    let mut text = std::fs::read_to_string(file).context("Cannot read edited draft; original draft kept")?;
    if text.ends_with('\n') {
        text.pop();
        if text.ends_with('\r') { text.pop(); }
    }
    Ok(text)
}

impl App<'_> {
    fn send(&self, command: Command) -> Result<()> {
        self.tx.send(command).map_err(|_| anyhow::anyhow!("agent runtime stopped"))
    }

    fn load_display(&mut self) -> Result<()> {
        self.commands = greencode_core::skills::commands(&self.meta.cwd, &self.cfg);
        let Some(_) = Session::open(&self.meta.id) else { return Ok(()) };
        let (_, messages) = Session::load(&self.meta.id)?;
        self.last_context = Session::runtime(&self.meta.id)?["context"].as_u64().unwrap_or_default();
        for Message { role, content } in messages {
            for block in content {
                match block {
                    Block::Text { text } if !greencode_core::context::is_update(&text) => {
                        if role == Role::User {
                            self.history.push(text.clone());
                            self.log.add(Kind::User, text);
                        } else { self.log.add(Kind::Assistant, text); }
                    }
                    Block::ToolUse { name, input, .. } => self.log.add(Kind::Tool, format!("{name} {input}")),
                    Block::ToolResult { content, is_error, .. } => self.tool_result(None, content, is_error),
                    Block::Other(v) if v["type"] == "thinking" => {
                        if let Some(text) = v["thinking"].as_str() {
                            let text = if self.tui.prose_only_thinking { prose_only(text) } else { text.to_owned() };
                            self.log.add(Kind::Thinking, text);
                        }
                    }
                    _ => (),
                }
            }
        }
        Ok(())
    }

    fn switch(&mut self, meta: Meta) -> Result<()> {
        anyhow::ensure!(self.busy.is_none() && self.queue.is_empty(), "Wait for the turn or clear held prompts with /queue clear before switching threads");
        anyhow::ensure!(meta.backend == "native", "This TUI runs native threads; use --text for external threads");
        let (tx, rx) = greencode_core::spawn(self.rt, self.cfg.clone(), meta.clone(), self.perms.clone());
        (self.tx, self.rx, self.meta) = (tx, rx, meta);
        self.history.clear();
        self.cleared_drafts.clear();
        self.recalled = None;
        self.draft.clear();
        self.log.clear();
        self.agents.clear();
        self.usage = Usage::default();
        self.last_context = 0;
        self.scroll = 0;
        self.held = false;
        self.loop_state = None;
        if let Some(job) = self.loop_check.take() { job.abort(); }
        if let Some(job) = self.title_job.take() { job.abort(); }
        if let Some(job) = self.predicting.take() { job.abort(); }
        self.predictions.clear();
        self.prompts = 0;
        self.generated_title = false;
        self.bare_confirm = None;
        self.git = self.tui.git.enabled.then(|| git_status(&self.meta.cwd)).flatten();
        self.stop_collab();
        self.load_display()?;
        self.start_collab();
        Ok(())
    }

    fn prompt(&mut self, text: String) -> Result<()> {
        self.start_prompt(Command::Prompt(text.clone()), text)
    }

    fn start_prompt(&mut self, command: Command, display: String) -> Result<()> {
        self.send(command)?;
        self.log.add(Kind::User, display);
        self.scroll = 0;
        self.busy = Some(Instant::now());
        self.turn_usage = None;
        self.advisor_notes.clear();
        self.cancelling = false;
        self.failed = false;
        self.streamed = 0;
        if let Some(job) = self.predicting.take() { job.abort(); }
        self.predictions.clear();
        self.prediction_index = 0;
        self.prompts = self.prompts.saturating_add(1);
        Ok(())
    }

    fn steer(&mut self, text: String) -> Result<()> {
        self.send(Command::Steer(text.clone()))?;
        self.log.add(Kind::Steer, text);
        self.scroll = 0;
        Ok(())
    }

    fn cancel(&mut self) -> Result<()> {
        if let Some(job) = self.fetching_review.take() {
            job.abort();
            self.busy = None;
            self.log.add(Kind::Notice, "Remote review fetch cancelled".into());
            return Ok(());
        }
        self.loop_state = None;
        if let Some(job) = self.loop_check.take() { job.abort(); }
        if self.busy.is_some() && !self.cancelling {
            if let Some(voice) = self.voice.as_mut() { voice.clear(); }
            greencode_core::speech::stop();
            self.send(Command::Cancel)?;
            self.cancelling = true;
            self.held = !self.queue.is_empty();
            self.pending.clear();
        }
        Ok(())
    }

    /// `tui.renderSvg` / `tui.renderEmbeds`: rasterize the newest reply's first figure onto the
    /// transcript's inline-image slot. Local files and public URLs only; needs a graphics terminal.
    fn show_figures(&mut self) {
        let display = &self.tui.display;
        if (!display.render_svg && !display.render_embeds)
            || greencode_core::image::protocol().is_none() || !self.tui.terminal.show_images
        {
            return;
        }
        let Some(text) = self.log.last_assistant().map(str::to_string) else { return };
        let Some(figure) = greencode_core::figure::figures(&text, display.render_svg, display.render_embeds).into_iter().next() else { return };
        let cwd = self.meta.cwd.clone();
        if let Some(job) = self.figures.take() { job.abort(); }
        self.figures = Some(self.rt.spawn(async move {
            let png = |source: &str| greencode_core::figure::svg_png(source, 800).map(|png| ("image/png".to_string(), png));
            if figure.src.starts_with('<') {
                return png(&figure.src);
            }
            let (media, bytes) = greencode_core::figure::load(&figure.src, &cwd).await.ok()?;
            if greencode_core::figure::is_svg(&figure.src, &bytes) {
                png(&String::from_utf8_lossy(&bytes))
            } else {
                Some((media, bytes))
            }
        }));
    }

    /// Keeps the newest image for the next draw; the terminal protocol decides whether it can be shown.
    fn attach_image(&mut self, image: serde_json::Value) {
        let label = image["path"].as_str().map(|path| format!("Image: {path}")).unwrap_or_else(|| "Image attached".into());
        self.log.add(Kind::Notice, label);
        let Some((media, bytes)) = greencode_core::context::image_bytes(&image) else { return };
        let (media, bytes) = (media.to_string(), bytes);
        if !self.tui.terminal.show_images {
            return;
        }
        self.inline_image = Some((media, bytes));
        self.inline_emitted = false;
    }

    /// Writes the image escape at `rect` after a draw; the terminal keeps it until those cells change.
    fn emit_inline(&mut self, rect: Option<ratatui::layout::Rect>) -> Result<()> {
        let Some(rect) = rect else { return Ok(()) };
        if self.inline_emitted {
            return Ok(());
        }
        let Some((escape, rows)) = self.inline_payload(rect.width) else {
            self.inline_image = None;
            return Ok(());
        };
        self.inline_rows = rows;
        let mut out = io::stdout().lock();
        execute!(out, crossterm::cursor::MoveTo(rect.x, rect.y))?;
        out.write_all(&escape)?;
        out.flush()?;
        self.inline_emitted = true;
        Ok(())
    }

    /// Bytes that draw the pending image over `cols` cells, plus the rows it needs.
    fn inline_payload(&self, cols: u16) -> Option<(Vec<u8>, u16)> {
        let protocol = greencode_core::image::protocol()?;
        let (media, bytes) = self.inline_image.as_ref()?;
        greencode_core::image::inline_escape(protocol, media, bytes, cols)
    }

    fn tool_result(&mut self, id: Option<&str>, output: String, error: bool) {
        // `pty.notify`: a console command that moved to the background is now waiting on the user.
        if output.contains("moved to the background, pid") || output.contains("started in the background, pid") {
            notify(self.tui.notify.console, "A console command is waiting on you");
        }
        let name = id.and_then(|id| self.tool_names.remove(id)).unwrap_or_default();
        let kind = if error { Kind::ToolError } else { Kind::ToolOk };
        let plain_text = !output.trim_start().starts_with('{') && !output.trim_start().starts_with('[');
        let markdown = !error && ((name == "read" && self.tui.read.render_markdown)
            || (name.starts_with("mcp") && self.tui.mcp.render_markdown_results && plain_text));
        if !error && name == "bash" && self.tui.display.shell_visualization {
            let command = self.tool_command.take().unwrap_or_default();
            let lines = display::shell_box(&command, &output, 80, self.tui.symbol_preset == greencode_core::config::SymbolPreset::Ascii);
            self.log.add_full(kind, lines.join("\n"), false);
            return;
        }
        if markdown || (name == "read" && self.tui.read.tool_result_preview) {
            self.log.add_full(kind, output, markdown);
        } else {
            self.log.add(kind, output);
        }
    }

    fn event(&mut self, event: Event) {
        self.publish_collab(&event);
        match event {
            Event::Text(text) => {
                if let Some(voice) = self.voice.as_mut() { voice.feed(false, &text); }
                self.streamed += text.chars().count() as u64;
                self.log.delta(&text, self.tui.display.smooth_streaming);
            }
            Event::Thinking(text) => {
                if let Some(voice) = self.voice.as_mut() { voice.feed(true, &text); }
                let text = if self.tui.prose_only_thinking { prose_only(&text) } else { text };
                self.log.add(Kind::Thinking, text);
            }
            Event::Image(image) => self.attach_image(image),
            Event::ToolStart { id, name, input } => {
                if name == "bash" {
                    self.tool_command = Some(input["command"].as_str().unwrap_or_default().to_string());
                }
                if name == "todo" {
                    let plan = input["todos"].to_string();
                    if replan_refreshes(self.cfg.tui.title.refresh_on_replan, self.last_plan.as_deref(), &plan) {
                        self.spawn_title();
                    }
                    self.last_plan = Some(plan);
                }
                self.tool_names.insert(id, name.clone());
                self.log.add(Kind::Tool, format!("{name} {input}"));
            }
            Event::ToolEnd { id, output, is_error } => self.tool_result(Some(&id), output, is_error),
            Event::Permission { tool, summary, reply } if !self.cancelling => {
                self.pending.push_back(Pending::Permission { label: format!("Allow {tool}?\n{summary}"), reply, offset: 0, shown: false, since: Instant::now() });
                notify(self.tui.ask.notify, &format!("Approval needed: {tool}"));
            }
            Event::Dialog { request, reply } if !self.cancelling => {
                if let Some(dialog) = Dialog::new(request) {
                    self.pending.push_back(Pending::Dialog { dialog, reply, input: Editor::default(), offset: 0, since: Instant::now() });
                    notify(self.tui.ask.notify, "Waiting for your answer");
                } else { let _ = reply.send(json!({"cancelled": true})); }
            }
            Event::Usage(usage) => {
                self.last_context = usage.context() + usage.output;
                self.usage.add(usage);
                if self.busy.is_some() { self.turn_usage.get_or_insert_default().add(usage); }
            }
            Event::UsageAux(usage) => self.usage.add(usage),
            Event::Notify(text) => {
                if text.starts_with("Compacting conversation") {
                    self.log.compacted();
                }
                if text.contains("reset") && self.tui.display.codex_reset_fireworks {
                    self.fireworks = Some(Instant::now());
                }
                // `display.advisorCollapsedLines`: advisor notes collect into one collapsible card.
                if let Some(note) = text.strip_prefix("advisor (") {
                    self.advisor_notes.push(format!("advisor ({note}"));
                    let card = display::advisor_preview(&self.advisor_notes.join("\n"), self.tui.display.advisor_collapsed_lines);
                    self.log.set_advisor(card);
                } else {
                    self.log.add(Kind::Notice, text);
                }
            }
            Event::Error(error) => {
                self.failed = true;
                notify(self.tui.notify.error, "GreenCode stopped with an error");
                self.log.add(Kind::Error, error);
            }
            Event::Remote { command, data } if command == "hook" => {
                // `statusLine.showHookStatus`: a real hook run, not a guess from the config.
                let event = data["event"].as_str().unwrap_or("hook");
                self.hook_status = Some((format!("hook · {event}"), Instant::now()));
            }
            Event::Remote { command, data } if command == "get_commands" => {
                if let Some(commands) = data["commands"].as_array() {
                    self.commands = commands.iter().filter_map(|c| c["name"].as_str().map(str::to_owned)).collect();
                }
            }
            Event::Remote { command, data } if command == "present_transcript" => {
                self.log.present(data["mode"].as_str().unwrap_or("compact"), data["text"].as_str());
            }
            Event::Remote { command, data } if command == "render_assistant" => {
                if let Some(text) = data["text"].as_str() {
                    self.log.redraw_assistant(text);
                }
            }
            Event::Remote { command, data } if command == "subagent" => {
                if let (Some(name), Some(state)) = (data["name"].as_str(), data["state"].as_str()) {
                    // `task.showResolvedModelBadge` / `display.subagentLivePreview` read these off the event.
                    self.agents.insert(name.into(), (state.into(), data["detail"].as_str().unwrap_or_default().into(), data["model"].as_str().unwrap_or_default().into()));
                }
            }
            // Finished delegated tasks or messages from other threads woke the idle thread: they are its next turn.
            // A steer from another thread joins the running turn instead.
            Event::Remote { command, data } if command == "delegate_wake" || command == "injected_prompt" => {
                self.log.add(Kind::User, data["text"].as_str().unwrap_or_default().into());
                if data["steer"] != true {
                    self.busy = Some(Instant::now());
                    (self.turn_usage, self.cancelling, self.failed) = (None, false, false);
                }
            }
            Event::Done => {
                if let Some(mut voice) = self.voice.take() {
                    if voice.enhanced() {
                        let text = voice.take();
                        if !text.trim().is_empty() {
                            let (speech_cfg, rt) = (self.cfg.clone(), self.rt);
                            let (voice_name, speed) = (voice.voice().0.to_string(), voice.voice().1);
                            rt.spawn(async move {
                                let spoken = greencode_core::speech::enhanced(&speech_cfg, &text).await;
                                greencode_core::speech::speak_voice(&voice_name, speed, &spoken);
                            });
                        }
                    } else if !self.failed && !self.cancelling {
                        voice.finish();
                    }
                    self.voice = greencode_core::speech::Vocalizer::new(&self.cfg);
                }
                self.log.finish_turn(&self.tui);
                if let Some(text) = self.log.last_assistant() {
                    let text = text.to_owned();
                    self.spell.learn(&text);
                }
                self.pending.clear();
                let started = self.busy.take();
                let usage = self.turn_usage.take();
                let success = !self.failed && !self.cancelling;
                if let Some(started) = started.filter(|_| success) {
                    use std::fmt::Write as _;
                    let mut summary = String::new();
                    if let Some(u) = usage.filter(|_| self.tui.show_turn_usage) {
                        let _ = write!(summary, "Turn tokens: ↑{} ↓{} cached {}", u.context(), u.output, u.cache_read);
                    }
                    if self.tui.show_turn_time {
                        if !summary.is_empty() { summary.push_str(" · "); }
                        let _ = write!(summary, "Turn time: {:.1}s", started.elapsed().as_secs_f64());
                    }
                    if !summary.is_empty() { self.log.add(Kind::Notice, summary); }
                    let seconds = started.elapsed().as_secs_f64();
                    if let Some(u) = usage.filter(|_| seconds > 0.05) {
                        self.last_tok_rate = Some(u.output as f64 / seconds);
                    }
                }
                self.idle_since = Instant::now();
                self.recap_shown = false;
                if success {
                    notify(self.tui.notify.completion, "Turn complete");
                    if self.meta.title.is_empty() {
                        if let Ok((meta, _)) = Session::load(&self.meta.id) {
                            if !meta.title.is_empty() { self.meta.title = meta.title; }
                        }
                    }
                    self.show_figures();
                    if self.tui.display.cache_miss_marker {
                        if let Some(u) = usage.filter(|u| u.cache_read == 0 && u.context() > 0) {
                            self.log.add(Kind::Notice, format!("— prompt cache miss ({} input tokens) —", u.context()));
                        }
                    }
                    self.git = self.tui.git.enabled.then(|| git_status(&self.meta.cwd)).flatten();
                    self.schedule_title();
                    self.schedule_prediction();
                }
                if self.cancelling { self.log.add(Kind::Notice, "Interrupted".into()); }
                self.held |= (self.failed || self.cancelling) && !self.queue.is_empty();
                self.cancelling = false;
                if !self.held && !self.queue.is_empty() {
                    // `follow_up_mode = all` sends the whole queue as one turn's steers ahead of its prompt.
                    let batch: Vec<String> = match self.cfg.interaction_opts.follow_up_mode {
                        greencode_core::config::interaction::FollowUpDrain::All => self.queue.drain(..).collect(),
                        greencode_core::config::interaction::FollowUpDrain::OneAtATime => self.queue.pop_front().into_iter().collect(),
                    };
                    let mut batch = batch.into_iter();
                    if let Some(first) = batch.next() {
                        for rest in batch {
                            if let Err(error) = self.steer(rest) { self.log.add(Kind::Error, format!("{error:#}")); }
                        }
                        if let Err(error) = self.prompt(first) { self.log.add(Kind::Error, format!("{error:#}")); }
                    }
                }
                if !self.held && self.queue.is_empty() {
                    if let Err(error) = self.loop_after_turn() {
                        self.loop_state = None;
                        self.log.add(Kind::Error, format!("Loop stopped: {error:#}"));
                    }
                    if success && self.loop_state.is_none() {
                        if let Some(next) = self.goal_continuation() {
                            if let Err(error) = self.prompt(next) { self.log.add(Kind::Error, format!("{error:#}")); }
                        }
                    }
                }
            }
            _ => (),
        }
    }

    fn submit(&mut self, action: Submit) -> Result<()> {
        anyhow::ensure!(self.fetching_review.is_none(), "Wait for the review diff or press Esc to cancel");
        if self.menu.is_some() { return self.choose(); }
        let mut text = self.input.take();
        if self.tui.emoji_autocomplete { text = input::emoji(&text); }
        if text.trim().is_empty() { return Ok(()); }
        self.recalled = None;
        self.scroll = 0;
        self.predictions.clear();
        // The image was shown for the turn that attached it; a new prompt frees its rows.
        self.inline_image = None;
        self.inline_emitted = false;
        self.inline_rows = 0;
        let trimmed = text.trim();
        if self.tui.input.bare_exit_on_empty_session && self.history.is_empty()
            && matches!(trimmed.to_ascii_lowercase().as_str(), "exit" | "quit" | "q") {
            return self.quit();
        }
        // Only local commands are recognized here; extension and skill commands go to the native engine.
        if let Some(slash) = text.strip_prefix('/').filter(|_| !text.contains('\n')) {
            let (cmd, arg) = slash.trim_end().split_once(char::is_whitespace).unwrap_or((slash.trim_end(), ""));
            if let Some(name) = cmd.strip_prefix("skill:") {
                if self.cfg.tui.skills.enable_skill_commands && arg.trim().is_empty() {
                    return self.local("skill", name);
                }
            }
            if !matches!(cmd, "compact" | "ratchet") && COMMANDS.iter().any(|(name, _)| *name == cmd) {
                return self.local(cmd, arg.trim());
            }
        }
        if self.tui.input.bare_slash_commands && !trimmed.contains(char::is_whitespace) {
            if let Some(cmd) = bare_command(trimmed) {
                if !self.history.is_empty() && self.bare_confirm.as_deref() != Some(trimmed) {
                    self.bare_confirm = Some(trimmed.to_owned());
                    self.log.add(Kind::Notice, format!("Press Enter again to run /{cmd}"));
                    return Ok(());
                }
                self.bare_confirm = None;
                if matches!(cmd, "compact" | "ratchet") {
                    self.history.push(text.clone());
                    return self.prompt(format!("/{trimmed}"));
                }
                return self.local(cmd, "");
            }
        }
        self.bare_confirm = None;
        self.spell.learn(&text);
        self.history.push(text.clone());
        if self.busy.is_none() && !self.held { self.prompt(text) }
        else if self.busy.is_some() && !self.cancelling && (action == Submit::Steer
            || (action == Submit::Default && matches!(self.tui.follow_up, FollowUp::Steer))) { self.steer(text) }
        else { self.queue.push_back(text); Ok(()) }
    }

    fn recalled_text(&self, mut at: usize) -> &str {
        for (before, text) in &self.cleared_drafts {
            match at.cmp(before) {
                std::cmp::Ordering::Less => break,
                std::cmp::Ordering::Equal => return text,
                std::cmp::Ordering::Greater => at -= 1,
            }
        }
        &self.history[at]
    }

    fn recall(&mut self, older: bool) {
        let count = self.history.len() + self.cleared_drafts.len();
        if count == 0 { return; }
        if older {
            if self.recalled.is_none() { self.draft = self.input.take(); }
            let at = self.recalled.unwrap_or(count).saturating_sub(1);
            self.recalled = Some(at);
            self.input.replace(self.recalled_text(at).to_owned());
        } else if let Some(at) = self.recalled {
            self.recalled = (at + 1 < count).then_some(at + 1);
            let text = match self.recalled {
                Some(i) => self.recalled_text(i).to_owned(),
                None => std::mem::take(&mut self.draft),
            };
            self.input.replace(text);
        }
    }

    fn matches(&self) -> Vec<(&str, &str)> {
        if let Some(menu) = &self.menu {
            let query = self.input.text().to_lowercase();
            if menu.kind == Picker::History {
                return self.history.iter().rev().filter(|text| query.is_empty() || text.to_lowercase().contains(&query))
                    .map(|text| (text.as_str(), "")).collect();
            }
            return menu.choices.iter().filter(|c| query.split_whitespace().all(|q| {
                c.value.to_lowercase().contains(q) || c.detail.to_lowercase().contains(q)
            })).map(|c| (c.value.as_str(), c.detail.as_str())).collect();
        }
        let Some(prefix) = self.input.text().strip_prefix('/').filter(|s| !s.contains(char::is_whitespace)) else { return Vec::new() };
        COMMANDS.iter().copied().filter(|(name, _)| name.starts_with(prefix) && (*name != "skill" || self.cfg.skills_enabled))
            .chain(self.commands.iter().filter(|name| name.starts_with(prefix) && !COMMANDS.iter().any(|(cmd, _)| cmd == name)).map(|name| (name.as_str(), "Project command or extension")))
            .collect()
    }

    fn has_slash_prefix(&self) -> bool {
        self.input.text().strip_prefix('/').is_some_and(|s| !s.contains(char::is_whitespace))
    }

    fn close_menu(&mut self) {
        if let Some(menu) = self.menu.take() { self.input = menu.saved_draft; }
        self.selected = 0;
    }

    fn choose(&mut self) -> Result<()> {
        let matches = self.matches();
        let Some((choice, _)) = matches.get(self.selected.min(matches.len().saturating_sub(1))) else { return Ok(()) };
        let choice = choice.to_string();
        if let Some(menu) = self.menu.take() {
            self.input = menu.saved_draft;
            self.selected = 0;
            let command = match menu.kind {
                Picker::Settings => {
                    let current = toml::Value::try_from(&self.tui)?;
                    let value = toml_path(&current, &choice).map(toml_text).unwrap_or_default();
                    self.input.replace(format!("/settings {choice} {value}").trim_end().to_owned());
                    self.recalled = None;
                    return Ok(());
                }
                Picker::Paste => {
                    let text = self.paste.take().unwrap_or_default();
                    let wrapped = match choice.as_str() {
                        "code block" => format!("```\n{text}\n```"),
                        "XML tags" => format!("<paste>\n{text}\n</paste>"),
                        _ => {
                            let unique = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos());
                            let path = std::env::temp_dir().join(format!("greencode-paste-{unique}.txt"));
                            std::fs::write(&path, &text)?;
                            format!("@{}", path.display())
                        }
                    };
                    self.input.insert(&wrapped);
                    self.recalled = None;
                    return Ok(());
                }
                Picker::Model => "model", Picker::Session => "resume", Picker::Mode => "mode", Picker::Effort => "thinking", Picker::Skill => "skill",
                Picker::History => {
                    self.input.replace(choice);
                    self.recalled = None;
                    return Ok(());
                }
                Picker::Review(focus) => {
                    match choice.as_str() {
                        "uncommitted" => self.run_review(review::Target::Working, &focus)?,
                        "base branch" => self.open_review_refs(focus, false)?,
                        "specific commit" => self.open_review_refs(focus, true)?,
                        _ => unreachable!(),
                    }
                    return Ok(());
                }
                Picker::ReviewBase(focus) => return self.run_review(review::Target::Base(&choice), &focus),
                Picker::ReviewCommit(focus) => return self.run_review(review::Target::Commit(&choice), &focus),
            };
            self.local(command, &choice)
        } else {
            self.input.replace(format!("/{choice} "));
            self.selected = 0;
            Ok(())
        }
    }

    fn key(&mut self, key: KeyEvent) -> Result<()> {
        self.idle_since = Instant::now();
        // `startup.showSplash`: any key skips the rest of the moment; the key still edits and submits.
        self.splash = None;
        // `startup.setupWizard`: the first-run flow owns the keyboard until it is finished or skipped.
        if self.wizard_key(key)? {
            return Ok(());
        }
        // The reset celebration stays until Escape, then the turn continues normally.
        if key.code == KeyCode::Esc && self.fireworks.take().is_some() {
            return Ok(());
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
        let action = self.keymap.action(key);
        let edit = match action { Some(Action::Edit(edit)) => Some(edit), _ => None };
        if let Some(pending) = self.pending.front_mut() {
            match pending {
                Pending::Permission { shown, offset, .. } => {
                    let decision = match key.code {
                        KeyCode::Down | KeyCode::PageDown => { *offset = offset.saturating_add(1); return Ok(()); }
                        KeyCode::Up | KeyCode::PageUp => { *offset = offset.saturating_sub(1); return Ok(()); }
                        KeyCode::Char('y') if *shown && key.modifiers.is_empty() => Decision::Allow,
                        KeyCode::Char('a') if *shown && key.modifiers.is_empty() => Decision::Always,
                        KeyCode::Char('n') | KeyCode::Esc => Decision::Deny,
                        KeyCode::Char('c') if ctrl => Decision::Deny,
                        _ => return Ok(()),
                    };
                    if let Some(Pending::Permission { reply, .. }) = self.pending.pop_front() { let _ = reply.send(decision); }
                }
                Pending::Dialog { dialog, input, offset, .. } => {
                    match key.code {
                        KeyCode::PageDown | KeyCode::Down => { *offset = offset.saturating_add(1); return Ok(()); }
                        KeyCode::PageUp | KeyCode::Up => { *offset = offset.saturating_sub(1); return Ok(()); }
                        _ => (),
                    }
                    let escape = key.code == KeyCode::Esc || (ctrl && key.code == KeyCode::Char('c'));
                    if key.code == KeyCode::Enter && !alt && !shift && !ctrl {
                        dialog.answers.push(input.take());
                        *offset = 0;
                    }
                    if escape || dialog.answers.len() == dialog.prompts.len() {
                        if let Some(Pending::Dialog { dialog, reply, .. }) = self.pending.pop_front() {
                            let _ = reply.send(if escape { json!({"cancelled":true}) } else { dialog.finish() });
                        }
                    } else { input.key(key, edit); }
                }
            }
            return Ok(());
        }
        if !matches!(key.code, KeyCode::Tab) { self.predictions.clear(); }
        if self.menu.is_none() && self.pending.is_empty() {
            match self.vim.key(key, &mut self.input, true) {
                vim::VimOutcome::Handled => {
                    self.selected = 0;
                    // `spelling.autocorrect` also applies to insert-mode typing handled by vim.
                    if matches!(key.code, KeyCode::Char(_)) { self.autocorrect(); }
                    return Ok(());
                }
                vim::VimOutcome::Submit => return self.submit(Submit::Default),
                vim::VimOutcome::Pass => {}
            }
        }
        if let Some(action) = action.filter(|a| !matches!(a, Action::Edit(_))
            && (self.menu.is_none() || matches!(a, Action::Model | Action::History))) {
            return match action {
                Action::Model => { self.open_picker(Picker::Model); Ok(()) }
                Action::History => { self.open_picker(Picker::History); Ok(()) }
                Action::NextModel => self.cycle_model(false),
                Action::PrevModel => self.cycle_model(true),
                Action::Plan => self.local("plan", ""),
                Action::Thinking => self.cycle_effort(),
                Action::ExpandTools => { self.tui.expand_tools = !self.tui.expand_tools; Ok(()) }
                Action::ToggleTools => { self.tui.hide_tools = !self.tui.hide_tools; Ok(()) }
                Action::ToggleThinking => { self.tui.hide_thinking = !self.tui.hide_thinking; Ok(()) }
                Action::ExternalEditor => { self.external_edit = true; Ok(()) }
                Action::Edit(_) => unreachable!(),
                Action::FollowUp => self.submit(Submit::Queue),
                Action::Dequeue => {
                    if self.queue.is_empty() { return Ok(()); }
                    anyhow::ensure!(self.input.text().is_empty(), "Clear the draft before recalling a queued prompt");
                    if let Some(text) = self.queue.pop_back() { self.input.replace(text); }
                    if self.queue.is_empty() { self.held = false; }
                    self.recalled = None;
                    self.selected = 0;
                    Ok(())
                }
            };
        }
        match key.code {
            KeyCode::Esc if self.menu.is_some() => self.close_menu(),
            KeyCode::Esc if self.busy.is_some() => { self.last_escape = None; self.cancel()?; }
            KeyCode::Esc if self.tui.double_escape_action != DoubleEscape::None && self.input.text().is_empty() => {
                let double = self.last_escape.take().is_some_and(|at| at.elapsed() < Duration::from_millis(500));
                if !double { self.last_escape = Some(Instant::now()); return Ok(()); }
                if let Some(picker) = escape_picker(self.tui.double_escape_action) { self.open_picker(picker); }
            }
            KeyCode::Char('c') if ctrl && self.busy.is_some() => self.cancel()?,
            KeyCode::Char('c') if ctrl && self.menu.is_some() => self.close_menu(),
            KeyCode::Char('c') if ctrl && !self.input.text().is_empty() => {
                let text = self.input.take();
                if self.tui.recall_cleared_drafts {
                    if self.cleared_drafts.len() == 100 { self.cleared_drafts.pop_front(); }
                    self.cleared_drafts.push_back((self.history.len(), text));
                }
                self.draft.clear();
                self.recalled = None;
                self.selected = 0;
            }
            KeyCode::Char('d' | 'c') if ctrl && self.input.text().is_empty() && self.busy.is_none() => self.quit()?,
            KeyCode::Enter if alt || shift => self.input.insert("\n"),
            KeyCode::Char('j') if ctrl => self.input.insert("\n"),
            KeyCode::Char('s') if ctrl => self.submit(Submit::Steer)?,
            KeyCode::Enter => self.submit(Submit::Default)?,
            KeyCode::PageUp => self.scroll = self.scroll.saturating_add(self.log.viewport.saturating_sub(1).max(1)),
            KeyCode::PageDown => self.scroll = self.scroll.saturating_sub(self.log.viewport.saturating_sub(1).max(1)),
            KeyCode::Tab if self.accept_completion() => {}
            KeyCode::Tab if self.accept_prediction() => {}
            KeyCode::Tab if self.menu.is_some() || self.has_slash_prefix() => self.choose()?,
            KeyCode::Up | KeyCode::Down if key.modifiers.is_empty() && (self.menu.is_some() || self.has_slash_prefix()) => {
                let count = self.matches().len();
                self.selected = match key.code {
                    KeyCode::Up => self.selected.saturating_sub(1),
                    _ => (self.selected + 1).min(count.saturating_sub(1)),
                };
            }
            _ if self.menu.is_none() && matches!(edit, Some(Edit::Up | Edit::Down))
                && (!self.input.text().contains('\n') || self.recalled.is_some()) => self.recall(edit == Some(Edit::Up)),
            _ => {
                if self.input.key(key, edit) {
                    self.selected = 0;
                    self.recalled = None;
                    if matches!(key.code, KeyCode::Char(_)) { self.autocorrect(); }
                }
            }
        }
        Ok(())
    }

    fn quit(&mut self) -> Result<()> {
        anyhow::ensure!(self.busy.is_none(), "Press Esc to stop the active turn before exiting");
        anyhow::ensure!(self.queue.is_empty(), "Follow-ups are still held; use /queue run or /queue clear");
        self.exit = true;
        Ok(())
    }

    /// `/collab`: a live room for this session, reachable through `collab.relayUrl`.
    fn collab_command(&mut self, arg: &str) -> Result<()> {
        let (cmd, mode) = arg.split_once(char::is_whitespace).unwrap_or((arg, ""));
        match cmd {
            "" | "start" => {
                if let Some(hosting) = &self.collab {
                    self.log.add(Kind::Notice, format!("Collab link: {}", hosting.host.link));
                    return Ok(());
                }
                let mode = match mode {
                    "control" => greencode_core::config::collab::AutoStart::Control,
                    "view" | "" => greencode_core::config::collab::AutoStart::View,
                    other => bail!("Use /collab start [view|control], found `{other}`"),
                };
                let cfg = self.cfg.clone();
                let thread = self.meta.id.clone();
                let result = self.rt.block_on(async {
                    tokio::time::timeout(Duration::from_secs(15), greencode_core::collab::auto_host_mode(&cfg, &thread, mode)).await
                });
                match result {
                    Ok(Ok(Some(hosting))) => {
                        let link = hosting.host.link.clone();
                        self.collab = Some(hosting);
                        self.log.add(Kind::Notice, format!("Collab link: {link}"));
                        self.poll_collab_prompts();
                    }
                    Ok(Ok(None)) => self.log.add(Kind::Notice, "Set collab.relayUrl to host a collab room".into()),
                    Ok(Err(error)) => return Err(error),
                    Err(_) => bail!("The collab relay did not answer within 15s"),
                }
            }
            "stop" => self.stop_collab(),
            "link" => match &self.collab {
                Some(hosting) => self.log.add(Kind::Notice, hosting.host.link.clone()),
                None => self.log.add(Kind::Notice, "This session is not being shared".into()),
            },
            other => bail!("Use /collab [start [view|control]|stop|link], found `{other}`"),
        }
        Ok(())
    }

    /// Hosts the thread when `collab.autoStart` asks for it; failures only warn.
    fn start_collab(&mut self) {
        if self.collab.is_some() || self.cfg.collab_opts.collab.auto_start == greencode_core::config::collab::AutoStart::Off {
            return;
        }
        let cfg = self.cfg.clone();
        let thread = self.meta.id.clone();
        let result = self.rt.block_on(async {
            tokio::time::timeout(Duration::from_secs(15), greencode_core::collab::auto_host(&cfg, &thread)).await
        });
        match result {
            Ok(Ok(Some(hosting))) => {
                let link = hosting.host.link.clone();
                self.collab = Some(hosting);
                self.log.add(Kind::Notice, format!("Collab link: {link}"));
                self.poll_collab_prompts();
            }
            Ok(Ok(None)) => {}
            Ok(Err(error)) => self.log.add(Kind::Error, format!("Collab auto start failed: {error:#}")),
            Err(_) => self.log.add(Kind::Error, "Collab auto start timed out after 15s".into()),
        }
    }

    fn stop_collab(&mut self) {
        self.collab_prompts = None;
        if let Some(hosting) = self.collab.take() {
            self.rt.block_on(async {
                let _ = tokio::time::timeout(Duration::from_secs(5), greencode_core::collab::stop_hosting(hosting.host)).await;
            });
        }
    }

    /// Forwards one event to viewers, redacted.
    fn publish_collab(&self, event: &Event) {
        if let Some(hosting) = &self.collab {
            let value = greencode_core::collab::redacted(&hosting.redactor, greencode_core::event_json(event));
            hosting.host.publish(value);
        }
    }

    /// Polls control-room prompts into the app; a single background task drives the receiver.
    fn poll_collab_prompts(&mut self) {
        let Some(hosting) = &self.collab else { return };
        let (tx, rx) = mpsc::unbounded_channel();
        let mut after = hosting.prompts_seen;
        let (base, room) = (hosting.host.base.clone(), hosting.host.room.clone());
        self.rt.spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(2)).await;
                match greencode_core::collab::prompts(&base, &room, after).await {
                    Ok(prompts) => {
                        for prompt in prompts {
                            after = prompt["id"].as_u64().map_or(after, |id| id as usize);
                            if let Some(text) = prompt["text"].as_str() {
                                if tx.send(text.to_owned()).is_err() { return; }
                            }
                        }
                    }
                    Err(_) => return,
                }
            }
        });
        self.collab_prompts = Some(rx);
    }

    fn paste(&mut self, text: String) {
        let threshold = self.tui.paste.large_menu_threshold;
        if threshold == 0 || text.lines().count() < threshold as usize {
            self.input.insert(&text);
            self.selected = 0;
            self.recalled = None;
            return;
        }
        self.paste = Some(text);
        self.menu = Some(Menu {
            kind: Picker::Paste,
            choices: vec![
                Choice { value: "code block".into(), detail: "Wrap the paste in a fenced code block".into() },
                Choice { value: "XML tags".into(), detail: "Wrap the paste in <paste> tags".into() },
                Choice { value: "save to file".into(), detail: "Write it to a temp file and insert an @path".into() },
            ],
            saved_draft: std::mem::take(&mut self.input),
        });
        self.selected = 0;
    }

    /// Splash logo rows while `startup.showSplash` is showing.
    fn splash_lines(&self) -> Option<Vec<String>> {
        self.splash.map(|_| startup::splash_lines())
    }

    /// Wizard rows while `startup.setupWizard` onboarding is due.
    fn wizard_lines(&self) -> Option<Vec<String>> {
        wizard_rows(self.wizard.as_ref()?)
    }

    /// Recomputes typo underlines and the completion whenever the draft changes.
    fn refresh_spelling(&mut self) {
        if self.wizard.is_some() || !self.spell.enabled() {
            return;
        }
        if self.spell_key.as_ref().is_some_and(|(text, cursor)| *cursor == self.input.cursor() && text == self.input.text()) {
            return;
        }
        self.spell_key = Some((self.input.text().to_owned(), self.input.cursor()));
        let text = self.input.text();
        self.typos = self.spell.typos(text);
        self.completion = self.spell.completion(text, self.input.cursor());
    }

    /// Tab in the composer accepts the word completion before slash-command selection.
    fn accept_completion(&mut self) -> bool {
        if self.menu.is_some() || self.has_slash_prefix() {
            return false;
        }
        let Some(completion) = self.completion.take() else { return false };
        let Some((text, cursor)) = greencode_core::spelling::apply_completion(self.input.text(), self.input.cursor(), &completion) else { return false };
        self.input.set(text, cursor);
        self.selected = 0;
        self.recalled = None;
        true
    }

    /// `spelling.autocorrect`: rewrite the word a boundary just completed with a confident correction.
    fn autocorrect(&mut self) {
        let Some((range, word)) = self.spell.autocorrect(self.input.text(), self.input.cursor()) else { return };
        let cursor = self.input.cursor();
        let mut text = self.input.text().to_owned();
        let delta = word.len() as isize - range.len() as isize;
        text.replace_range(range, &word);
        self.input.set(text, (cursor as isize + delta).max(0) as usize);
    }

    /// `startup.setupWizard`: Up/Down choose a model, Enter writes it and records completion, Esc skips.
    fn wizard_key(&mut self, key: KeyEvent) -> Result<bool> {
        let Some(wizard) = self.wizard.as_mut() else { return Ok(false) };
        match key.code {
            KeyCode::Up => wizard.selected = wizard.selected.saturating_sub(1),
            KeyCode::Down => wizard.selected = (wizard.selected + 1).min(wizard.choices.len().saturating_sub(1)),
            KeyCode::Esc => self.finish_wizard(false)?,
            KeyCode::Enter => self.finish_wizard(true)?,
            _ => {}
        }
        Ok(true)
    }

    fn finish_wizard(&mut self, apply: bool) -> Result<()> {
        let Some(wizard) = self.wizard.take() else { return Ok(()) };
        let home = greencode_core::home();
        let config = home.join("config.toml");
        let model = apply.then(|| wizard.selected_model().map(str::to_owned)).flatten();
        startup::finish(&home, &config, model.as_deref())?;
        let switched = model.filter(|model| model != &self.meta.model);
        if let Some(model) = switched.as_deref() {
            self.local("model", model)?;
        }
        let message = match switched {
            Some(model) => format!("Default model set to {model}; /settings changes preferences."),
            None => "Setup skipped; /settings changes preferences.".to_string(),
        };
        self.log.add(Kind::Notice, message);
        self.spell_key = None;
        Ok(())
    }

    fn accept_prediction(&mut self) -> bool {
        if self.predictions.is_empty() || self.menu.is_some() { return false; }
        let current = self.input.text();
        if !current.is_empty() && !self.predictions.iter().any(|p| p == current) { return false; }
        let index = self.prediction_index % self.predictions.len();
        let text = self.predictions[index].clone();
        self.input.replace(text);
        self.prediction_index = (index + 1) % self.predictions.len();
        self.selected = 0;
        true
    }

    /// The ghost completion shown when the draft is empty.
    fn prediction(&self) -> Option<&str> {
        self.predictions.first().filter(|_| self.input.text().is_empty() && self.busy.is_none()).map(String::as_str)
    }

    fn expire_pending(&mut self) -> bool {
        let timeout = self.tui.ask.timeout;
        if timeout == 0 { return false; }
        let expired = self.pending.front().is_some_and(|pending| match pending {
            Pending::Permission { since, .. } | Pending::Dialog { since, .. } => since.elapsed().as_secs() >= timeout,
        });
        if !expired { return false; }
        match self.pending.pop_front() {
            Some(Pending::Permission { reply, .. }) => {
                let _ = reply.send(Decision::Deny);
                self.log.add(Kind::Notice, "Approval timed out; denied".into());
            }
            Some(Pending::Dialog { dialog, reply, .. }) => {
                let mut dialog = dialog;
                dialog.answers.resize(dialog.prompts.len(), String::new());
                let _ = reply.send(dialog.finish());
                self.log.add(Kind::Notice, "Question timed out; submitted current answers".into());
            }
            None => {}
        }
        true
    }

    fn autosave_plan(&mut self) {
        if !self.tui.plan.autosave { return; }
        if let Err(error) = self.save_plan() {
            self.log.add(Kind::Error, format!("Plan autosave failed: {error:#}"));
        }
    }

    fn save_plan(&mut self) -> Result<()> {
        let Some(text) = self.log.last_assistant().filter(|text| !text.trim().is_empty()) else {
            anyhow::bail!("no assistant plan to save");
        };
        let configured = self.tui.plan.autosave_dir.trim();
        let dir = if configured.is_empty() {
            self.meta.cwd.join(".greencode/plans")
        } else if configured == "~" {
            greencode_core::user_home()
        } else if let Some(rest) = configured.strip_prefix("~/") {
            greencode_core::user_home().join(rest)
        } else {
            PathBuf::from(configured)
        };
        let dir = if dir.is_absolute() { dir } else { self.meta.cwd.join(dir) };
        std::fs::create_dir_all(&dir)?;
        let path = dir.join(format!("{}.md", self.meta.id));
        std::fs::write(&path, text)?;
        self.log.add(Kind::Notice, format!("Plan saved to {}", path.display()));
        Ok(())
    }

    /// `tui.mouse`: a click on a pinned agent row focuses it, or on a transcript tool card toggles it.
    fn click(&mut self, row: u16) {
        let pinned = self.pin_top.filter(|top| row >= *top)
            .and_then(|top| display::pinned_hit(&self.agents, self.tui.display.pinned_agents, usize::from(row - top)));
        if let Some(name) = pinned {
            self.agent_focus = (self.agent_focus.as_deref() != Some(name.as_str())).then_some(name);
            return;
        }
        self.log.toggle_at(row);
    }

    /// The status line fitted to `width`, from the current model, mode, context and cost.
    fn status_row(&self, width: usize) -> greencode_core::status_line::Row {
        let model = self.meta.model.split_once('/').map_or(self.meta.model.as_str(), |(_, m)| m);
        let cost = self.cfg.cost(model, &self.usage);
        let (git, git_dirty) = self.git.as_ref().map_or((None, false), |(branch, dirty)| (Some(branch.as_str()), *dirty));
        let info = greencode_core::status_line::Info {
            model: &self.meta.model, mode: mode_name(self.perms.mode()), effort: self.cfg.effort.as_deref().unwrap_or("default"),
            context: self.last_context,
            context_window: self.cfg.compaction_opts.maintenance.context_window.or(Some(self.cfg.compact_at)),
            cost, git: git.map(|branch| (branch, git_dirty)), queue: self.queue.len(),
            agents: self.agents.values().filter(|(state, _, _)| state == "running").count(),
            session: &self.session_name(), turn_tokens: self.turn_usage.map(|u| (u.input + u.output, u.cache_read)),
            scroll: self.scroll > 0,
        };
        greencode_core::status_line::compose(&self.tui.status_line, &info, width)
    }

    /// The session's display name: its title, else the directory name, else the id prefix.
    fn session_name(&self) -> String {
        if !self.meta.title.is_empty() { return self.meta.title.clone(); }
        let dir = self.meta.cwd.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        if !dir.is_empty() { return dir; }
        self.meta.id.chars().take(8).collect()
    }

    fn set_title(&mut self, title: String) {
        let title = title.trim();
        if !title.is_empty() { self.meta.title = title.to_owned(); }
        self.generated_title = true;
    }

    fn schedule_title(&mut self) {
        if !title_due(self.cfg.tui.title.generator, self.prompts, self.cfg.tui.title.refresh_after_prompts) { return; }
        self.spawn_title();
    }

    /// Starts a title generation now, replacing one in flight only when it already finished.
    fn spawn_title(&mut self) {
        if self.title_job.is_some() { return; }
        let model = greencode_core::textgen::title_model(&self.cfg, &self.meta.model, self.cfg.tui.title.generator)
            .unwrap_or_else(|| self.meta.model.clone());
        let Some(user) = self.history.last().cloned() else { return; };
        let assistant = self.log.last_assistant().unwrap_or_default().to_owned();
        let cfg = self.cfg.clone();
        self.title_job = Some(self.rt.spawn(async move {
            greencode_core::textgen::title(&cfg, &model, "", &user, &assistant).await
        }));
    }

    fn schedule_prediction(&mut self) {
        if !self.tui.composer.predictions || self.predicting.is_some() { return; }
        let Some(user) = self.history.last().cloned() else { return; };
        let assistant = self.log.last_assistant().unwrap_or_default().to_owned();
        let (cfg, model) = (self.cfg.clone(), self.meta.model.clone());
        let count = if self.tui.composer.prediction_alternatives { 4 } else { 1 };
        self.predicting = Some(self.rt.spawn(async move {
            greencode_core::textgen::next_prompt(&cfg, &model, &user, &assistant, count).await
        }));
    }

    /// Starts the idle recap once the session has sat idle for `recap.idle_seconds`.
    fn schedule_recap(&mut self) {
        let idle = self.idle_since.elapsed();
        if self.recap_job.is_some()
            || !self.cfg.interaction_opts.recap.due(idle, self.busy.is_some() || self.pending.len() > 0, self.queue.is_empty() == false, self.recap_shown)
        {
            return;
        }
        let Some(user) = self.history.last().cloned() else { return; };
        let assistant = self.log.last_assistant().unwrap_or_default().to_owned();
        if assistant.trim().is_empty() { return; }
        self.recap_shown = true;
        let (cfg, model) = (self.cfg.clone(), self.meta.model.clone());
        self.recap_job = Some(self.rt.spawn(async move {
            greencode_core::textgen::recap(&cfg, &model, &user, &assistant).await
        }));
    }

    fn token_rate(&self) -> Option<f64> {
        if let Some(started) = self.busy {
            let seconds = started.elapsed().as_secs_f64();
            if self.streamed > 0 && seconds > 0.3 {
                return Some((self.streamed as f64 / 4.0) / seconds);
            }
        }
        self.last_tok_rate
    }

    fn program_status(&self) -> &'static str {
        if !self.pending.is_empty() { "waiting" }
        else if self.busy.is_some() { "working" }
        else if self.failed { "failed" }
        else { "done" }
    }

    fn terminal_title(&self) -> String {
        let name = if self.meta.title.is_empty() {
            self.meta.cwd.file_name().map_or_else(|| "GreenCode".into(), |n| n.to_string_lossy().into_owned())
        } else { self.meta.title.clone() };
        if !self.tui.title_state { return format!("{name} — GreenCode"); }
        let glyph = match self.busy {
            Some(started) => title_spinner(self.tui.title_spinner, started.elapsed(), animation_interval(&self.tui)),
            None if !self.pending.is_empty() => "!",
            None => ">",
        };
        format!("{glyph} {name} — GreenCode")
    }

    /// `goal.continuation_modes`: an open goal starts the next interactive turn here.
    fn goal_continuation(&self) -> Option<String> {
        let goal = greencode_core::goal::Goal::load(&self.meta.id)?;
        goal.continues(&self.cfg.modes, greencode_core::config::modes::RunMode::Interactive)
    }

    fn loop_after_turn(&mut self) -> Result<()> {
        let Some(state) = self.loop_state.clone() else { return Ok(()) };
        let Some((command, until)) = state.condition else {
            return self.loop_checked(Ok(true));
        };
        let cwd = self.meta.cwd.clone();
        let timeout = self.cfg.tui.loop_ui.condition_timeout_ms;
        self.loop_check = Some(self.rt.spawn(async move {
            shell_condition(cwd, command, timeout).await.map(|success| success != until)
        }));
        Ok(())
    }

    /// The next loop iteration once the condition passed (or after the between-iteration compaction).
    fn loop_checked(&mut self, result: Result<bool>) -> Result<()> {
        let keep = result?;
        let Some(mut state) = self.loop_state.clone() else { return Ok(()) };
        if !keep {
            self.loop_state = None;
            self.log.add(Kind::Notice, "Loop condition finished; loop stopped".into());
            return Ok(());
        }
        if state.compacting {
            state.compacting = false;
            let prompt = state.prompt.clone();
            self.loop_state = Some(state);
            return self.prompt(prompt);
        }
        if !state.started {
            state.started = true;
            let prompt = state.prompt.clone();
            self.loop_state = Some(state);
            return self.prompt(prompt);
        }
        match self.cfg.tui.loop_ui.mode {
            LoopMode::Prompt => {
                let prompt = state.prompt.clone();
                self.loop_state = Some(state);
                self.prompt(prompt)?;
            }
            LoopMode::Compact => {
                state.compacting = true;
                self.loop_state = Some(state);
                self.prompt("/compact".into())?;
            }
            LoopMode::Reset => {
                let prompt = state.prompt.clone();
                let mut meta = Meta::new(self.meta.cwd.clone(), self.meta.model.clone(), "native".into());
                meta.subagent_model = self.cfg.subagent_model.clone();
                self.switch(meta)?;
                self.loop_state = Some(state);
                self.prompt(prompt)?;
            }
        }
        Ok(())
    }

    fn set_loop(&mut self, arg: &str) -> Result<()> {
        let arg = arg.trim();
        if arg.is_empty() {
            let text = match &self.loop_state {
                Some(state) => format!("Loop: {}", state.prompt),
                None => "Usage: /loop [--while CMD|--until CMD] PROMPT · /loop stop".into(),
            };
            self.log.add(Kind::Notice, text);
            return Ok(());
        }
        if matches!(arg, "stop" | "off") {
            self.loop_state = None;
            if let Some(job) = self.loop_check.take() { job.abort(); }
            self.log.add(Kind::Notice, "Loop stopped".into());
            return Ok(());
        }
        anyhow::ensure!(self.loop_state.is_none(), "A loop is already running; use /loop stop first");
        anyhow::ensure!(self.busy.is_none() && self.queue.is_empty(), "Wait for the active turn before starting a loop");
        let (condition, prompt) = parse_loop_args(arg)?;
        let prompt = if prompt.is_empty() {
            self.history.last().cloned().context("Give /loop a prompt or send one first")?
        } else { prompt };
        self.loop_state = Some(LoopState { prompt: prompt.clone(), condition, compacting: false, started: false });
        if self.loop_state.as_ref().is_some_and(|state| state.condition.is_some()) {
            self.loop_after_turn()?;
        } else {
            self.loop_checked(Ok(true))?;
        }
        self.log.add(Kind::Notice, "Loop started; Esc stops it".into());
        Ok(())
    }

    /// `/prewalk [restart|MODEL|off]`: arm the hand-off for this thread, or turn it off.
    fn prewalk_command(&mut self, arg: &str) -> Result<()> {
        let cfg = &self.cfg.assist_opts.prewalk;
        let (from, into) = (cfg.from.as_str(), cfg.into.as_str());
        let target = match arg.trim() {
            "" | "on" => into.to_string(),
            "restart" => {
                // Back to planning on `from`, and hand off to `into` again at the next edit.
                let model = greencode_core::assist::resolve_target(&self.cfg, from).context("prewalk.from names no known model")?;
                self.send(Command::Remote(json!({"type": "prewalk", "restart": model, "target": into})))?;
                self.log.add(Kind::Notice, format!("Prewalk restarted on {model}; hands off to {into}"));
                return Ok(());
            }
            "off" => {
                self.send(Command::Remote(json!({"type": "prewalk", "target": ""})))?;
                self.log.add(Kind::Notice, "Prewalk off".into());
                return Ok(());
            }
            spec => greencode_core::assist::resolve_target(&self.cfg, spec)
                .with_context(|| format!("Prewalk needs a model or role, got `{spec}`"))?,
        };
        self.send(Command::Remote(json!({"type": "prewalk", "target": target.clone()})))?;
        self.log.add(Kind::Notice, format!("Prewalk armed: {target}"));
        Ok(())
    }

    fn open_picker(&mut self, kind: Picker) {
        if self.menu.as_ref().is_some_and(|menu| menu.kind == kind) { return; }
        self.close_menu();
        let choice = |value: String| Choice { value, detail: String::new() };
        let choices = match kind {
            Picker::Model => cycle_models(&self.cfg, &self.tui).into_iter().map(|value| Choice { detail: if value == self.meta.model { "current".into() } else { String::new() }, value }).collect(),
            Picker::Session => Session::list().into_iter()
                .filter(|m| m.backend == "native" && same_scope(m, &self.meta, self.tui.session.worktree_scope) && Session::open(&m.id).is_some())
                .filter(|m| tree_filter_keeps(self.tui.tree_filter_mode, m))
                .map(|m| Choice { value: m.id, detail: if m.title.is_empty() { m.model } else { m.title } }).collect(),
            Picker::Effort => std::iter::once("default").chain(greencode_core::provider::effort_levels(&self.cfg, &self.meta.model))
                .map(|s| choice(s.into())).collect(),
            Picker::Mode => ["ask", "auto-edit", "full", "plan"].into_iter().map(|s| choice(s.into())).collect(),
            Picker::Skill => greencode_core::skills::list(&self.meta.cwd, &self.cfg).into_iter().map(|s| choice(s.name)).collect(),
            Picker::History => Vec::new(),
            Picker::Paste => Vec::new(),
            Picker::Settings => {
                let current = toml::Value::try_from(&self.tui).expect("TUI preferences are TOML values");
                TUI_SETTINGS.iter().map(|&key| Choice {
                    value: key.into(), detail: toml_path(&current, key).map(toml_text).unwrap_or_else(|| "unset".into()),
                }).collect()
            }
            Picker::Review(_) => [
                ("base branch", "Compare HEAD with the merge base"),
                ("uncommitted", "Review staged and unstaged changes to tracked files"),
                ("specific commit", "Review a recent commit"),
            ].into_iter().map(|(value, detail)| Choice { value: value.into(), detail: detail.into() }).collect(),
            Picker::ReviewBase(_) | Picker::ReviewCommit(_) => unreachable!(),
        };
        self.menu = Some(Menu { kind, choices, saved_draft: std::mem::take(&mut self.input) });
    }

    fn open_review_refs(&mut self, focus: String, commits: bool) -> Result<()> {
        let choices = review::choices(&self.meta.cwd, commits)?;
        let kind = if commits { Picker::ReviewCommit(focus) } else { Picker::ReviewBase(focus) };
        self.menu = Some(Menu { kind, choices, saved_draft: std::mem::take(&mut self.input) });
        Ok(())
    }

    fn run_review(&mut self, target: review::Target<'_>, focus: &str) -> Result<()> {
        anyhow::ensure!(self.busy.is_none() && self.queue.is_empty(), "Wait for the turn and clear held prompts before reviewing");
        let (label, prompt) = review::prompt(&self.meta.cwd, target, focus)?;
        self.start_review(label, prompt, "local")
    }

    fn start_review(&mut self, label: String, prompt: String, scope: &str) -> Result<()> {
        let mode = self.perms.mode();
        self.set_mode(Mode::Plan)?;
        let command = Command::PromptWithOptions(prompt, vec![json!({"type":"review_mode","scope":scope})]);
        if let Err(err) = self.start_prompt(command, label) {
            self.perms.set_mode(mode);
            return Err(err);
        }
        if mode != Mode::Plan {
            self.pre_plan = mode;
            self.log.add(Kind::Notice, "Review uses plan mode; use /plan to restore the previous mode after it finishes.".into());
        }
        Ok(())
    }

    fn cycle_model(&mut self, reverse: bool) -> Result<()> {
        let models = cycle_models(&self.cfg, &self.tui);
        let index = models.iter().position(|m| m == &self.meta.model);
        let index = match index {
            Some(i) if reverse => (i + models.len() - 1) % models.len(),
            Some(i) => (i + 1) % models.len(),
            None => 0,
        };
        if models[index] != self.meta.model { self.local("model", &models[index])?; }
        Ok(())
    }

    fn cycle_effort(&mut self) -> Result<()> {
        let levels = greencode_core::provider::effort_levels(&self.cfg, &self.meta.model);
        anyhow::ensure!(!levels.is_empty(), "This model does not expose reasoning levels");
        let next = self.cfg.effort.as_deref().and_then(|e| levels.iter().position(|x| *x == e)).map_or(0, |i| i + 1);
        self.local("thinking", levels.get(next).copied().unwrap_or("default"))
    }

    fn set_mode(&self, mode: Mode) -> Result<()> {
        anyhow::ensure!(self.busy.is_none(), "Wait for the active turn before changing permissions");
        if let Some(policy) = &self.cfg.policy {
            anyhow::ensure!(!policy.settings.default_mode.as_ref().is_some_and(|s| s.locked && s.value != mode), "Permission mode is locked by policy ({})", policy.organization);
            policy.check_mode(mode)?;
        }
        self.perms.set_mode(mode);
        Ok(())
    }

    fn set_preference(&mut self, arg: &str) -> Result<()> {
        let (name, value) = arg.split_once(char::is_whitespace).context("Use /settings KEY VALUE or /settings KEY default")?;
        anyhow::ensure!(TUI_SETTINGS.contains(&name), "Unknown terminal preference `{name}`");
        let key = format!("tui.{name}");
        let value = value.trim();
        anyhow::ensure!(!value.is_empty(), "Missing value for `{name}`; use `default` to reset");
        let values: Vec<String> = if name == "cycle_order" {
            value.split_whitespace().map(str::to_owned).collect()
        } else { vec![value.to_owned()] };
        let text = toml::to_string(&BTreeMap::from([("tui", &self.tui)]))?;
        let (edited, next_values) = if value == "default" {
            (settings::reset_text(&text, &key)?.unwrap_or(text), None)
        } else {
            (settings::set_text(&text, &key, &values)?, Some(values.as_slice()))
        };
        let next: TuiCfg = toml::from_str::<toml::Table>(&edited)?.remove("tui")
            .map(toml::Value::try_into).transpose()?.unwrap_or_default();
        let theme_changed = next.theme != self.tui.theme || next.theme_dark != self.tui.theme_dark || next.theme_light != self.tui.theme_light;
        let theme = theme_changed.then(|| theme::Theme::load_preferred(&next)).transpose()?;
        let keymap = (next.keybindings != self.tui.keybindings).then(|| Keymap::load(&next.keybindings)).transpose()?;
        if self.persist_settings { settings::edit_global(&key, next_values)?; }
        if let Some(theme) = theme { self.theme = theme; }
        if let Some(keymap) = keymap { self.keymap = keymap; }
        self.tui = next;
        self.log.add(Kind::Notice, format!("{key} updated ({})", if self.persist_settings { "saved" } else { "this launch only" }));
        Ok(())
    }

    /// Extra workspace roots in force for this session.
    fn roots(&self) -> Vec<PathBuf> {
        self.cfg.context_opts.workspace.roots(&self.meta.cwd)
    }

    /// Stores extra workspace roots, persisting them when settings persist, and reconfigures the agent.
    fn set_roots(&mut self, roots: Vec<PathBuf>) -> Result<()> {
        let mut cfg = (*self.cfg).clone();
        cfg.context_opts.workspace.additional_directories = roots.clone();
        if self.persist_settings {
            let values: Vec<String> = roots.iter().map(|p| p.display().to_string()).collect();
            settings::edit_global("workspace.additional_directories", Some(&values))?;
        }
        self.cfg = Arc::new(cfg);
        self.send(Command::Config(self.cfg.clone()))?;
        Ok(())
    }

    fn local(&mut self, command: &str, arg: &str) -> Result<()> {
        match command {
            "help" => {
                let mut help = format!("{KEYS}\nCtrl+S Steer · Ctrl+D Quit\n{}", self.keymap.help());
                for (name, detail) in COMMANDS { help.push_str(&format!("\n/{name}  {detail}")); }
                self.log.add(Kind::Notice, help);
            }
            "hotkeys" => self.log.add(Kind::Notice, self.keymap.help()),
            "history" => self.open_picker(Picker::History),
            "settings" if arg.is_empty() => self.open_picker(Picker::Settings),
            "settings" => self.set_preference(arg)?,
            "review" => {
                anyhow::ensure!(self.busy.is_none() && self.queue.is_empty(), "Wait for the turn and clear held prompts before reviewing");
                if let Some((remote, focus)) = review::Remote::parse(arg)? {
                    let focus = focus.to_owned();
                    self.fetching_review = Some(self.rt.spawn(async move { remote.prompt(&focus).await }));
                    self.scroll = 0;
                    self.busy = Some(Instant::now());
                } else {
                    self.open_picker(Picker::Review(arg.into()));
                }
            }
            "model" if arg.is_empty() => self.open_picker(Picker::Model),
            "model" => {
                anyhow::ensure!(self.busy.is_none(), "Model changes are available between turns");
                let (provider, model) = arg.split_once('/').context("Use /model provider/model")?;
                anyhow::ensure!(!model.is_empty() && self.cfg.providers.contains_key(provider), "Unknown provider/model `{arg}`");
                if let Some(policy) = &self.cfg.policy { anyhow::ensure!(policy.provider_allowed(provider), "Provider blocked by policy ({})", policy.organization); }
                if let Some(mut view) = Session::view(&self.meta.id)? {
                    view.meta.model = arg.into();
                    Session::save_view(&view)?;
                } else if let Some(session) = Session::open(&self.meta.id) {
                    let mut meta = Session::load(&self.meta.id)?.0;
                    meta.model = arg.into();
                    session.append(&json!({"meta": meta}))?;
                }
                self.send(Command::Model(arg.into()))?;
                self.meta.model = arg.into();
            }
            "thinking" if arg.is_empty() => self.open_picker(Picker::Effort),
            "thinking" => {
                anyhow::ensure!(self.busy.is_none(), "Thinking changes are available between turns");
                let levels = greencode_core::provider::effort_levels(&self.cfg, &self.meta.model);
                anyhow::ensure!(arg == "default" || levels.contains(&arg), "Supported levels: default{}", levels.iter().fold(String::new(), |s, l| s + ", " + l));
                let mut cfg = (*self.cfg).clone();
                cfg.effort = (arg != "default").then(|| arg.into());
                self.cfg = Arc::new(cfg);
                self.send(Command::Config(self.cfg.clone()))?;
            }
            "mode" if arg.is_empty() => self.open_picker(Picker::Mode),
            "mode" => {
                let next = parse_mode(arg)?;
                if self.perms.mode() == Mode::Plan && next != Mode::Plan { self.autosave_plan(); }
                self.set_mode(next)?;
            }
            "plan" if !self.cfg.modes.plan.enabled => bail!("Plan mode is disabled by plan.enabled"),
            "plan" => {
                let mode = self.perms.mode();
                if mode == Mode::Plan { self.autosave_plan(); self.set_mode(self.pre_plan)?; }
                else { self.set_mode(Mode::Plan)?; self.pre_plan = mode; }
            }
            "new" => {
                let mut meta = Meta::new(self.meta.cwd.clone(), self.meta.model.clone(), "native".into());
                meta.subagent_model = self.cfg.subagent_model.clone();
                self.switch(meta)?;
            }
            "resume" if arg.is_empty() => self.open_picker(Picker::Session),
            "resume" => self.switch(Session::load(arg)?.0)?,
            "fork" => {
                anyhow::ensure!(self.busy.is_none() && self.queue.is_empty(), "Wait for the turn and clear held prompts before forking");
                // `branchSummary.enabled`: save a summary of the branch being left beside the new one.
                let note = if self.cfg.context_opts.branch_summary.enabled {
                    match greencode_core::branch::write(&self.meta.id) {
                        Ok(path) => format!(" · branch summary in {}", path.display()),
                        Err(e) => format!(" · branch summary failed: {e}"),
                    }
                } else { String::new() };
                let meta = Session::fork(&self.meta.id)?;
                self.log.add(Kind::Notice, format!("Forked to {}{note}", meta.id));
                self.switch(meta)?;
            }
            "add-dir" => {
                anyhow::ensure!(!arg.trim().is_empty(), "usage: /add-dir PATH");
                let path = greencode_core::permission::resolve(&self.meta.cwd, arg.trim());
                let mut roots = self.cfg.context_opts.workspace.additional_directories.clone();
                if !roots.contains(&path) {
                    roots.push(path.clone());
                    self.set_roots(roots)?;
                }
                self.log.add(Kind::Notice, format!("Added workspace root {}", path.display()));
            }
            "remove-dir" => {
                anyhow::ensure!(!arg.trim().is_empty(), "usage: /remove-dir PATH");
                let path = greencode_core::permission::resolve(&self.meta.cwd, arg.trim());
                let mut roots = self.cfg.context_opts.workspace.additional_directories.clone();
                let before = roots.len();
                roots.retain(|p| *p != path);
                anyhow::ensure!(roots.len() != before, "{} is not an extra workspace root", path.display());
                self.set_roots(roots)?;
                self.log.add(Kind::Notice, format!("Removed workspace root {}", path.display()));
            }
            "dirs" => {
                let mut roots = vec![self.meta.cwd.display().to_string()];
                roots.extend(self.roots().iter().map(|p| p.display().to_string()));
                self.log.add(Kind::Notice, roots.join("\n"));
            }
            "session" => self.log.add(Kind::Notice, format!("{} · {}\n{}", self.meta.id, self.meta.model, self.meta.cwd.display())),
            "wt" => {
                anyhow::ensure!(self.busy.is_none() && self.queue.is_empty(), "Wait for the turn before switching checkouts");
                let repo = &self.meta.cwd;
                anyhow::ensure!(greencode_core::worktree::is_repo(repo), "`/wt` needs a git repository ({})", repo.display());
                let id = if arg.trim().is_empty() { self.meta.id.clone() } else { arg.trim().to_owned() };
                // `worktree.cleanSource` decides whether the source checkout is reset after its changes move over.
                let tree = greencode_core::worktree::create_session(repo, &self.cfg.worktree, &id, self.cfg.isolation_opts.isolation.backend)?;
                std::env::set_current_dir(&tree.path)?;
                let mut meta = Meta::new(tree.path.clone(), self.meta.model.clone(), "native".into());
                meta.subagent_model = self.cfg.subagent_model.clone();
                self.switch(meta)?;
                self.log.add(Kind::Notice, format!("Worktree {} (branch {}); /exit to leave it there", tree.path.display(), tree.branch));
            }
            "export" => {
                anyhow::ensure!(self.busy.is_none(), "Wait for the turn before exporting");
                let path = crate::html::export(&self.meta.id, (!arg.is_empty()).then(|| self.meta.cwd.join(arg)))?;
                self.log.add(Kind::Notice, format!("Exported to {}", path.display()));
            }
            "role" => {
                let (name, spec) = arg.split_once(char::is_whitespace).context("Use /role NAME provider/model, or /role NAME - to clear")?;
                let spec = spec.trim();
                let spec = (spec != "-").then_some(spec);
                let storage = self.cfg.provider_opts.model_role_storage;
                let path = greencode_core::config::roles::save(&self.meta.cwd, storage, name.trim(), spec)?;
                let cfg = std::sync::Arc::make_mut(&mut self.cfg);
                match spec {
                    Some(spec) => { cfg.model_roles.insert(name.trim().into(), spec.into()); }
                    None => { cfg.model_roles.remove(name.trim()); }
                }
                self.send(Command::Config(self.cfg.clone()))?;
                self.log.add(Kind::Notice, format!("{} saved to {}", name.trim(), path.display()));
            }
            "goal" => {
                anyhow::ensure!(self.cfg.modes.goal.enabled, "Goal mode is disabled by goal.enabled");
                let action = arg.trim();
                let session = self.meta.id.clone();
                let text = match action {
                    "" => match greencode_core::goal::Goal::load(&session) {
                        Some(goal) => goal.line(self.cfg.modes.goal.status_in_footer),
                        None => "No goal is set; start one with the `goal` tool or `/goal TOKEN_BUDGET N`".into(),
                    },
                    "pause" | "resume" | "complete" | "drop" =>
                        greencode_core::goal::apply(Some(&session), &serde_json::json!({"action": action}))?,
                    other => {
                        // `/goal TOKEN_BUDGET N`: budget the current goal, or start one on the last user message.
                        let rest = other.strip_prefix("TOKEN_BUDGET").map(str::trim)
                            .or_else(|| other.strip_prefix("budget").map(str::trim))
                            .context("usage: /goal [pause|resume|complete|drop|TOKEN_BUDGET N]")?;
                        let budget: u64 = rest.parse().context("TOKEN_BUDGET takes a number")?;
                        let session = self.meta.id.clone();
                        let mut goal = greencode_core::goal::Goal::load(&session)
                            .or_else(|| self.history.last().map(|objective| greencode_core::goal::Goal {
                                objective: objective.clone(), status: Default::default(), tokens_used: 0, token_budget: None,
                            }))
                            .context("no goal yet: set one with the `goal` tool first")?;
                        goal.token_budget = (budget > 0).then_some(budget);
                        goal.save(&session)?;
                        goal.line(true)
                    }
                };
                self.log.add(Kind::Notice, format!("Goal\n{text}"));
            }
            "instances" => {
                anyhow::ensure!(self.cfg.task.cross_instance, "Cross-instance messaging is disabled by task.crossInstance");
                let report = greencode_core::instance::report(&self.meta.cwd, &self.meta.id);
                let list = report["instances"].as_array().cloned().unwrap_or_default();
                let text = if list.is_empty() { "No other GreenCode instance is running in this folder".into() } else {
                    list.iter().map(|i| format!("{} (pid {}) {}", i["session"].as_str().unwrap_or(""), i["pid"], i["agents"].as_array().map(|a| a.iter().filter_map(|v| v.as_str()).collect::<Vec<_>>().join(", ")).unwrap_or_default())).collect::<Vec<_>>().join("\n")
                };
                self.log.add(Kind::Notice, text);
            }
            "message" => {
                anyhow::ensure!(self.cfg.task.cross_instance, "Cross-instance messaging is disabled by task.crossInstance");
                let (to, text) = arg.split_once(char::is_whitespace).context("usage: /message SESSION_OR_ALL TEXT")?;
                greencode_core::instance::send(&self.meta.cwd, &self.meta.id, to.trim(), text.trim())?;
                self.log.add(Kind::Notice, format!("Sent to {}", to.trim()));
            }
            "skillful" => {
                // Session-only, like OMP: the setting stays as configured.
                let cfg = std::sync::Arc::make_mut(&mut self.cfg);
                cfg.provider_opts.skillful = !cfg.provider_opts.skillful;
                let on = cfg.provider_opts.skillful;
                self.send(Command::Config(self.cfg.clone()))?;
                self.log.add(Kind::Notice, format!("Skills in prompt: {}", if on { "on" } else { "off" }));
            }
            "skill" if !self.cfg.skills_enabled => bail!("Skills are disabled"),
            "skill" if arg.is_empty() => self.open_picker(Picker::Skill),
            "skill" => self.input.replace(greencode_core::skills::body(&self.meta.cwd, &self.cfg, arg).context("Unknown skill")?),
            "jobs" => {
                // Each running job: its caption, its derived widget, and the log path to read.
                let text = greencode_core::tools::jobwidget::sheet();
                let text = if text.is_empty() { "No background jobs are running".into() } else { text };
                self.log.add(Kind::Notice, text);
            }
            "agents" => {
                let text = if self.agents.is_empty() { "No sub-agent activity in this TUI yet".into() } else {
                    self.agents.iter().map(|(name, (state, detail, model))| format!("{name}: {state} {detail}{}", if model.is_empty() { String::new() } else { format!(" [{model}]") })).collect::<Vec<_>>().join("\n")
                };
                self.log.add(Kind::Notice, text);
            }
            "to" | "stop" => {
                let (name, message) = if command == "to" { arg.split_once(char::is_whitespace).context("Use /to NAME TEXT")? } else { (arg, "") };
                anyhow::ensure!(self.busy.is_some() && self.agents.get(name).is_some_and(|(state, _, _)| state == "running"), "No active sub-agent named `{name}`");
                self.send(Command::Remote(json!({"type": if command == "to" { "sub_steer" } else { "sub_cancel" }, "name":name, "message":message})))?;
            }
            "queue" => match arg {
                "run" if self.busy.is_none() => {
                    if let Some(text) = self.queue.pop_front() { self.held = false; self.prompt(text)?; }
                }
                "run" => bail!("Wait for the active turn"),
                "clear" => { self.queue.clear(); self.held = false; }
                "" => self.log.add(Kind::Notice, if self.queue.is_empty() { "No queued prompts".into() } else {
                    self.queue.iter().enumerate().map(|(i, text)| format!("{}: {text}", i + 1)).collect::<Vec<_>>().join("\n")
                }),
                _ => bail!("Use /queue [run|clear]"),
            },
            "prewalk" => self.prewalk_command(arg)?,
            "loop" => self.set_loop(arg)?,
            "collab" => self.collab_command(arg)?,
            "share" => {
                let thread = if arg.is_empty() { self.meta.id.clone() } else { arg.to_owned() };
                anyhow::ensure!(self.sharing.is_none(), "A share upload is already running");
                let cfg = self.cfg.clone();
                self.sharing = Some(self.rt.spawn(async move { greencode_core::share::publish(&cfg, &thread).await }));
            }
            "exit" => self.quit()?,
            _ => unreachable!(),
        }
        Ok(())
    }
}

fn cycle_models(cfg: &Config, tui: &TuiCfg) -> Vec<String> {
    let available = cfg.model_list();
    let mut out = Vec::new();
    for role in &tui.cycle_order {
        let spec = if role == "default" { &cfg.model } else if let Some(spec) = cfg.model_roles.get(role) { spec } else { continue };
        if available.contains(spec) && !out.contains(spec) { out.push(spec.clone()); }
    }
    for model in available {
        if !out.contains(&model) { out.push(model); }
    }
    out
}

/// A plan replacement (not the first plan) refreshes the title only when `title.refreshOnReplan` is on.
fn replan_refreshes(refresh: bool, previous: Option<&str>, plan: &str) -> bool {
    refresh && previous.is_some_and(|old| old != plan)
}

fn title_due(generator: TitleGenerator, prompts: u32, every: u32) -> bool {
    if prompts <= 1 {
        generator != TitleGenerator::FirstMessage
    } else {
        greencode_core::textgen::refresh_title(prompts, every, true)
    }
}

fn escape_picker(action: DoubleEscape) -> Option<Picker> {
    match action {
        DoubleEscape::Rewind => Some(Picker::History),
        DoubleEscape::Tree => Some(Picker::Session),
        DoubleEscape::None => None,
    }
}

/// `treeFilterMode`: which sessions the tree picker shows by default.
fn tree_filter_keeps(mode: greencode_core::config::TreeFilterMode, meta: &Meta) -> bool {
    use greencode_core::config::TreeFilterMode::*;
    match mode {
        Default => !meta.title.starts_with("side: ") && (meta.parent.is_none() || !meta.title.is_empty()),
        NoTools => meta.title.starts_with("side: ") == false && meta.parent.is_none(),
        UserOnly => !meta.title.starts_with("side: ") && meta.parent.is_none(),
        LabeledOnly => !meta.title.is_empty(),
        All => true,
    }
}

fn bare_command(word: &str) -> Option<&'static str> {
    COMMANDS.iter().find(|(name, _)| *name == word).map(|(name, _)| *name)
        .or_else(|| (word == "compact").then_some("compact"))
}

fn toml_path<'a>(value: &'a toml::Value, path: &str) -> Option<&'a toml::Value> {
    path.split('.').try_fold(value, |table, key| table.get(key))
}

fn toml_text(value: &toml::Value) -> String {
    match value {
        toml::Value::Array(items) => items.iter().map(toml_text).collect::<Vec<_>>().join(" "),
        toml::Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

fn same_scope(candidate: &Meta, current: &Meta, scope: WorktreeScope) -> bool {
    if scope == WorktreeScope::Directory { return candidate.cwd == current.cwd; }
    match &current.repo {
        Some(repo) => candidate.repo.as_ref() == Some(repo) || (candidate.repo.is_none() && &candidate.cwd == repo),
        None => candidate.cwd == current.cwd,
    }
}

fn git_status(cwd: &Path) -> Option<(String, bool)> {
    let output = std::process::Command::new("git").arg("-C").arg(cwd).args(["rev-parse", "--abbrev-ref", "HEAD"]).output().ok()?;
    let branch = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if !output.status.success() || branch.is_empty() { return None; }
    let dirty = std::process::Command::new("git").arg("-C").arg(cwd).args(["status", "--porcelain"]).output()
        .ok().is_some_and(|output| !output.stdout.is_empty());
    Some((branch, dirty))
}

/// Keep prose and inline code; replace fenced code blocks with a single ellipsis line.
fn prose_only(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut fenced = false;
    for line in text.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") {
            if !fenced { out.push_str("…\n"); }
            fenced = !fenced;
        } else if !fenced && !trimmed.starts_with('#') {
            out.push_str(line);
            out.push('\n');
        }
    }
    out.trim_end().to_owned()
}

async fn shell_condition(cwd: PathBuf, command: String, timeout_ms: u64) -> Result<bool> {
    let mut child = tokio::process::Command::new("sh")
        .arg("-c").arg(&command).current_dir(&cwd)
        .stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn().with_context(|| format!("Cannot run loop condition `{command}`"))?;
    if timeout_ms == 0 {
        return child.wait().await.map(|status| status.success()).context("Cannot wait for loop condition");
    }
    match tokio::time::timeout(Duration::from_millis(timeout_ms), child.wait()).await {
        Ok(status) => Ok(status.context("Cannot wait for loop condition")?.success()),
        Err(_) => {
            let _ = child.kill().await;
            Err(anyhow::anyhow!("loop condition timed out after {timeout_ms} ms"))
        }
    }
}

/// `/loop [--while|--until CMD] [prompt]`; the condition is one shell word, quoted when it has spaces.
fn parse_loop_args(arg: &str) -> Result<(Option<(String, bool)>, String)> {
    let mut rest = arg.trim();
    let mut condition = None;
    if rest.starts_with("--") {
        let (flag, tail) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
        let until = match flag {
            "--while" => false,
            "--until" => true,
            _ => bail!("Unknown /loop flag {flag}"),
        };
        let (command, remainder) = shell_word(tail.trim_start())?;
        anyhow::ensure!(!command.trim().is_empty(), "{flag} needs a shell command");
        condition = Some((command.trim().to_owned(), until));
        rest = remainder.trim_start();
        anyhow::ensure!(!rest.starts_with("--"), "Use only one of --while or --until");
    }
    Ok((condition, rest.trim().to_owned()))
}

fn shell_word(input: &str) -> Result<(String, &str)> {
    match input.chars().next() {
        Some(quote @ ('\'' | '"')) => {
            let rest = &input[quote.len_utf8()..];
            let end = rest.find(quote).context("Unterminated quote in /loop condition")?;
            Ok((rest[..end].to_owned(), &rest[end + quote.len_utf8()..]))
        }
        Some(_) => {
            let end = input.find(char::is_whitespace).unwrap_or(input.len());
            Ok((input[..end].to_owned(), &input[end..]))
        }
        None => Ok((String::new(), "")),
    }
}

pub(super) fn parse_mode(mode: &str) -> Result<Mode> {
    Ok(match mode {
        "ask" => Mode::Ask, "auto-edit" => Mode::AutoEdit, "full" => Mode::Full, "plan" => Mode::Plan,
        _ => bail!("Unknown mode `{mode}`; choose ask, auto-edit, full, or plan"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wizard_rows_mark_the_selection_and_explain_an_empty_catalog() {
        let wizard = Wizard { choices: vec!["a/one".into(), "b/two".into()], selected: 1 };
        let rows = wizard_rows(&wizard).unwrap();
        assert!(rows.iter().any(|row| row == "  a/one"));
        assert!(rows.iter().any(|row| row == "> b/two"));
        assert_eq!(wizard.selected_model(), Some("b/two"));
        assert!(Wizard { choices: Vec::new(), selected: 0 }.selected_model().is_none());
        let empty = wizard_rows(&Wizard { choices: Vec::new(), selected: 0 }).unwrap();
        assert!(empty.iter().any(|row| row.contains("No models configured")));
    }

    #[test]
    fn replan_refresh_only_replaces_an_existing_plan() {
        assert!(!replan_refreshes(true, None, "[{todo}]"), "the first plan is not a replacement");
        assert!(!replan_refreshes(false, Some("old"), "new"), "title.refreshOnReplan off keeps the title");
        assert!(!replan_refreshes(true, Some("same"), "same"), "an unchanged plan does not regenerate");
        assert!(replan_refreshes(true, Some("old"), "new"));
    }

    #[test]
    fn cycle_order_lists_roles_before_remaining_models() {
        let mut cfg = Config::default();
        cfg.model = "a/one".into();
        cfg.models = vec!["b/two".into(), "c/three".into()];
        cfg.model_roles.insert("smol".into(), "c/three".into());
        let mut tui = TuiCfg::default();
        tui.cycle_order = vec!["smol".into(), "default".into()];
        assert_eq!(cycle_models(&cfg, &tui), ["c/three", "a/one", "b/two"]);
        tui.cycle_order = vec!["missing".into()];
        assert_eq!(cycle_models(&cfg, &tui), ["a/one", "b/two", "c/three"]);
    }

    #[test]
    fn resume_scope_expands_from_one_worktree_to_its_repository() {
        let repo = PathBuf::from("/repo");
        let current = {
            let mut meta = Meta::new(PathBuf::from("/repo/worktree"), "a/one".into(), "native".into());
            meta.repo = Some(repo.clone());
            meta
        };
        let main = Meta::new(repo.clone(), "a/one".into(), "native".into());
        let sibling = {
            let mut meta = Meta::new(PathBuf::from("/repo/other"), "a/one".into(), "native".into());
            meta.repo = Some(repo);
            meta
        };
        let outside = Meta::new(PathBuf::from("/elsewhere"), "a/one".into(), "native".into());
        assert!(!same_scope(&main, &current, WorktreeScope::Directory));
        assert!(same_scope(&main, &current, WorktreeScope::Repo));
        assert!(same_scope(&sibling, &current, WorktreeScope::Repo));
        assert!(!same_scope(&outside, &current, WorktreeScope::Repo));
    }

    #[test]
    fn title_generation_toggles_follow_generator_and_refresh_interval() {
        assert!(!title_due(TitleGenerator::FirstMessage, 1, 0));
        assert!(title_due(TitleGenerator::Fork, 1, 0));
        assert!(title_due(TitleGenerator::Tiny, 1, 0));
        assert!(title_due(TitleGenerator::FirstMessage, 4, 4));
        assert!(!title_due(TitleGenerator::Fork, 3, 4));
        assert!(!title_due(TitleGenerator::Fork, 4, 0));
    }

    #[test]
    fn double_escape_opens_rewind_or_tree_and_none_does_nothing() {
        assert_eq!(escape_picker(DoubleEscape::Rewind), Some(Picker::History));
        assert_eq!(escape_picker(DoubleEscape::Tree), Some(Picker::Session));
        assert_eq!(escape_picker(DoubleEscape::None), None);
    }

    #[test]
    fn bare_command_names_are_exact_including_compact() {
        assert_eq!(bare_command("model"), Some("model"));
        assert_eq!(bare_command("compact"), Some("compact"));
        assert_eq!(bare_command("ratchet"), Some("ratchet"));
        assert_eq!(bare_command("mod"), None);
        assert_eq!(bare_command("/model"), None);
    }

    #[test]
    fn loop_arguments_parse_quoted_conditions_and_prompt() {
        assert_eq!(parse_loop_args("--while 'test -f done' keep going").unwrap(),
            (Some(("test -f done".into(), false)), "keep going".into()));
        assert_eq!(parse_loop_args("--until \"make test\"").unwrap(),
            (Some(("make test".into(), true)), String::new()));
        assert_eq!(parse_loop_args("fix it").unwrap(), (None, "fix it".into()));
        assert!(parse_loop_args("--while").is_err());
        assert!(parse_loop_args("--bogus x").is_err());
        assert!(parse_loop_args("--while 'unterminated").is_err());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn loop_condition_times_out_and_reports_success() {
        let cwd = std::env::temp_dir();
        let started = Instant::now();
        let error = shell_condition(cwd.clone(), "sleep 5".into(), 50).await.unwrap_err();
        assert!(error.to_string().contains("timed out"), "{error:#}");
        assert!(started.elapsed() < Duration::from_secs(3));
        assert!(shell_condition(cwd, "true".into(), 1000).await.unwrap());
        assert!(!shell_condition(std::env::temp_dir(), "false".into(), 1000).await.unwrap());
    }
}
