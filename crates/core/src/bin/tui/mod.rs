mod input;
mod keymap;
mod review;
mod theme;
mod view;

use crate::dialog::Dialog;
use anyhow::{bail, Context, Result};
use crossterm::{
    event::{self, DisableBracketedPaste, EnableBracketedPaste, Event as Input, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    execute,
    terminal::{self, EnterAlternateScreen, LeaveAlternateScreen},
};
use greencode_core::{
    config::{settings, Config, FollowUp, TuiCfg}, permission::{Mode, Perms}, session::{Meta, Session}, Block, Command,
    Decision, Event, Message, Role, Usage,
};
use input::{Edit, Editor};
use keymap::{Action, Keymap};
use ratatui::{backend::CrosstermBackend, Terminal};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, VecDeque}, io::{self, Stdout, Write}, sync::Arc, time::{Duration, Instant},
};
use tokio::{runtime::Handle, sync::{mpsc, oneshot}};
use view::{Kind, Log};

const COMMANDS: &[(&str, &str)] = &[
    ("help", "Keys and slash commands"),
    ("model", "Select a model"),
    ("thinking", "Set reasoning effort"),
    ("mode", "Set permission mode (ask / auto-edit / full / plan)"),
    ("plan", "Toggle read-only plan mode"),
    ("new", "Start a thread"),
    ("resume", "Resume a saved thread"),
    ("fork", "Branch off the current thread"),
    ("session", "Show the current thread ID"),
    ("export", "Save this transcript as local HTML: /export [PATH]"),
    ("compact", "Compact context (summary / snap)"),
    ("skill", "Load a skill into the composer"),
    ("agents", "List sub-agent activity"),
    ("to", "Steer an active sub-agent: /to NAME TEXT"),
    ("stop", "Cancel an active sub-agent: /stop NAME"),
    ("queue", "List or run held follow-ups: /queue [run|clear]"),
    ("exit", "Quit GreenCode"),
    ("history", "Search prompt history"),
    ("hotkeys", "Show active application shortcuts"),
    ("settings", "Edit terminal preferences"),
    ("review", "Review a Git diff: /review [PR_OR_MR_URL] [FOCUS]"),
];
const KEYS: &str = "Enter Send · Alt+Enter Newline · Tab Complete · Esc Stop · PgUp/PgDn Scroll · /hotkeys";
const TUI_SETTINGS: &[&str] = &[
    "auto_resume", "autocomplete_max_visible", "expand_tools", "follow_up", "hide_thinking",
    "hide_tools", "keybindings", "recall_cleared_drafts", "show_terminal_progress", "show_turn_time", "show_turn_usage",
    "show_usage", "theme", "tight",
];

struct Screen {
    terminal: Terminal<CrosstermBackend<Stdout>>,
    progress: bool,
}

impl Screen {
    fn open() -> Result<Self> {
        terminal::enable_raw_mode()?;
        let result = (|| {
            execute!(io::stdout(), EnterAlternateScreen, EnableBracketedPaste)?;
            Terminal::new(CrosstermBackend::new(io::stdout())).map(|terminal| Self { terminal, progress: false })
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

    fn restore() {
        let _ = execute!(io::stdout(), DisableBracketedPaste, LeaveAlternateScreen, crossterm::cursor::Show);
        let _ = terminal::disable_raw_mode();
    }
}

impl Drop for Screen {
    fn drop(&mut self) {
        let _ = self.set_progress(false);
        Self::restore();
    }
}

#[derive(PartialEq, Eq)]
enum Picker { Model, Session, Mode, Effort, Skill, History, Settings, Review(String), ReviewBase(String), ReviewCommit(String) }

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
    Permission { label: String, reply: oneshot::Sender<Decision>, offset: usize, shown: bool },
    Dialog { dialog: Dialog, reply: oneshot::Sender<Value>, input: Editor, offset: usize },
}

impl Pending {
    fn closed(&self) -> bool {
        match self {
            Self::Permission { reply, .. } => reply.is_closed(),
            Self::Dialog { reply, .. } => reply.is_closed(),
        }
    }
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
    agents: BTreeMap<String, (String, String)>,
    pre_plan: Mode,
    exit: bool,
    external_edit: bool,
}

pub(super) fn run(rt: &Handle, cfg: Config, meta: Meta, persist_settings: bool) -> Result<()> {
    let theme = theme::Theme::load(cfg.tui.theme.as_deref())?;
    let keymap = Keymap::load(&cfg.tui.keybindings)?;
    let tui = cfg.tui.clone();
    let cfg = Arc::new(cfg);
    let perms = Arc::new(Perms::new(cfg.mode));
    let (tx, rx) = greencode_core::spawn(rt, cfg.clone(), meta.clone(), perms.clone());
    let mut app = App {
        rt, cfg, tui, persist_settings, theme, keymap, meta, perms, tx, rx,
        log: Log::default(), input: Editor::default(), menu: None, selected: 0, scroll: 0,
        history: Vec::new(), recalled: None, draft: String::new(), busy: None, cancelling: false,
        cleared_drafts: VecDeque::new(),
        fetching_review: None,
        held: false, failed: false, queue: VecDeque::new(), pending: VecDeque::new(),
        usage: Usage::default(), turn_usage: None, last_context: 0, commands: Vec::new(), agents: BTreeMap::new(),
        pre_plan: Mode::Ask, exit: false, external_edit: false,
    };
    app.load_display()?;
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| { Screen::restore(); previous(info); }));
    let mut screen = Screen::open()?;
    let mut dirty = true;
    let mut tick = Instant::now();
    while !app.exit {
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
        let before = app.pending.len();
        app.pending.retain(|p| !p.closed());
        dirty |= before != app.pending.len();
        if app.busy.is_some() && tick.elapsed() >= Duration::from_millis(100) {
            dirty = true;
            tick = Instant::now();
        }
        if dirty {
            screen.set_progress(app.tui.show_terminal_progress && app.busy.is_some() && app.pending.is_empty())?;
            screen.terminal.draw(|frame| view::draw(frame, &mut app))?;
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
                            screen = Screen::open()?;
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
                Input::Paste(text) => {
                    match app.pending.front_mut() {
                        Some(Pending::Dialog { input, .. }) => input.insert(&text),
                        Some(Pending::Permission { .. }) => (),
                        None => { app.input.insert(&text); app.selected = 0; app.recalled = None; }
                    }
                }
                _ => (),
            }
            dirty = true;
        }
    }
    Ok(())
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
        self.commands = greencode_core::skills::commands(&self.meta.cwd);
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
                    Block::ToolResult { content, is_error, .. } => self.tool_result(content, is_error),
                    Block::Other(v) if v["type"] == "thinking" => {
                        if let Some(text) = v["thinking"].as_str() { self.log.add(Kind::Thinking, text.to_owned()); }
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
        self.load_display()
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
        self.cancelling = false;
        self.failed = false;
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
        if self.busy.is_some() && !self.cancelling {
            self.send(Command::Cancel)?;
            self.cancelling = true;
            self.held = !self.queue.is_empty();
            self.pending.clear();
        }
        Ok(())
    }

    fn tool_result(&mut self, output: String, error: bool) {
        self.log.add(if error { Kind::ToolError } else { Kind::ToolOk }, output);
    }

    fn event(&mut self, event: Event) {
        match event {
            Event::Text(text) => self.log.delta(&text),
            Event::Thinking(text) => self.log.add(Kind::Thinking, text),
            Event::Image(_) => self.log.add(Kind::Notice, "Image attached to session (not rendered here)".into()),
            Event::ToolStart { name, input, .. } => self.log.add(Kind::Tool, format!("{name} {input}")),
            Event::ToolEnd { output, is_error, .. } => self.tool_result(output, is_error),
            Event::Permission { tool, summary, reply } if !self.cancelling => {
                self.pending.push_back(Pending::Permission { label: format!("Allow {tool}?\n{summary}"), reply, offset: 0, shown: false });
            }
            Event::Dialog { request, reply } if !self.cancelling => {
                if let Some(dialog) = Dialog::new(request) {
                    self.pending.push_back(Pending::Dialog { dialog, reply, input: Editor::default(), offset: 0 });
                } else { let _ = reply.send(json!({"cancelled": true})); }
            }
            Event::Usage(usage) => {
                self.last_context = usage.context() + usage.output;
                self.usage.add(usage);
                if self.busy.is_some() { self.turn_usage.get_or_insert_default().add(usage); }
            }
            Event::UsageAux(usage) => self.usage.add(usage),
            Event::Notify(text) => self.log.add(Kind::Notice, text),
            Event::Error(error) => { self.failed = true; self.log.add(Kind::Error, error); }
            Event::Remote { command, data } if command == "get_commands" => {
                if let Some(commands) = data["commands"].as_array() {
                    self.commands = commands.iter().filter_map(|c| c["name"].as_str().map(str::to_owned)).collect();
                }
            }
            Event::Remote { command, data } if command == "subagent" => {
                if let (Some(name), Some(state)) = (data["name"].as_str(), data["state"].as_str()) {
                    self.agents.insert(name.into(), (state.into(), data["detail"].as_str().unwrap_or_default().into()));
                }
            }
            Event::Done => {
                self.log.end_delta();
                self.pending.clear();
                let started = self.busy.take();
                let usage = self.turn_usage.take();
                if let Some(started) = started.filter(|_| !self.failed && !self.cancelling) {
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
                }
                if self.cancelling { self.log.add(Kind::Notice, "Interrupted".into()); }
                self.held |= (self.failed || self.cancelling) && !self.queue.is_empty();
                self.cancelling = false;
                if !self.held {
                    if let Some(text) = self.queue.pop_front() {
                        if let Err(error) = self.prompt(text) { self.log.add(Kind::Error, format!("{error:#}")); }
                    }
                }
            }
            _ => (),
        }
    }

    fn submit(&mut self, action: Submit) -> Result<()> {
        anyhow::ensure!(self.fetching_review.is_none(), "Wait for the review diff or press Esc to cancel");
        if self.menu.is_some() { return self.choose(); }
        let text = self.input.take();
        if text.trim().is_empty() { return Ok(()); }
        self.recalled = None;
        self.scroll = 0;
        // Only local commands are recognized here; extension and skill commands go to the native engine.
        if let Some(slash) = text.strip_prefix('/').filter(|_| !text.contains('\n')) {
            let (cmd, arg) = slash.trim_end().split_once(char::is_whitespace).unwrap_or((slash.trim_end(), ""));
            if cmd != "compact" && COMMANDS.iter().any(|(name, _)| *name == cmd) {
                return self.local(cmd, arg.trim());
            }
        }
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
                    self.input.replace(format!("/settings {choice} {}", current.get(&choice).map_or_else(|| "default".into(), ToString::to_string)));
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
            KeyCode::Esc if self.busy.is_some() => self.cancel()?,
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
                if self.input.key(key, edit) { self.selected = 0; self.recalled = None; }
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

    fn open_picker(&mut self, kind: Picker) {
        if self.menu.as_ref().is_some_and(|menu| menu.kind == kind) { return; }
        self.close_menu();
        let choice = |value: String| Choice { value, detail: String::new() };
        let choices = match kind {
            Picker::Model => self.cfg.model_list().into_iter().map(|value| Choice { detail: if value == self.meta.model { "current".into() } else { String::new() }, value }).collect(),
            Picker::Session => Session::list().into_iter().filter(|m| m.backend == "native" && m.cwd == self.meta.cwd && Session::open(&m.id).is_some())
                .map(|m| Choice { value: m.id, detail: if m.title.is_empty() { m.model } else { m.title } }).collect(),
            Picker::Effort => std::iter::once("default").chain(greencode_core::provider::effort_levels(&self.cfg, &self.meta.model))
                .map(|s| choice(s.into())).collect(),
            Picker::Mode => ["ask", "auto-edit", "full", "plan"].into_iter().map(|s| choice(s.into())).collect(),
            Picker::Skill => greencode_core::skills::list(&self.meta.cwd).into_iter().map(|s| choice(s.name)).collect(),
            Picker::History => Vec::new(),
            Picker::Settings => {
                let current = toml::Value::try_from(&self.tui).expect("TUI preferences are TOML values");
                TUI_SETTINGS.iter().map(|&key| Choice {
                    value: key.into(), detail: current.get(key).map_or_else(|| "default".into(), ToString::to_string),
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
        let models = self.cfg.model_list();
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
        let values = [value.to_owned()];
        let text = toml::to_string(&BTreeMap::from([("tui", &self.tui)]))?;
        let (edited, next_values) = if value == "default" {
            (settings::reset_text(&text, &key)?.unwrap_or(text), None)
        } else {
            (settings::set_text(&text, &key, &values)?, Some(values.as_slice()))
        };
        let next: TuiCfg = toml::from_str::<toml::Table>(&edited)?.remove("tui")
            .map(toml::Value::try_into).transpose()?.unwrap_or_default();
        let theme = (next.theme != self.tui.theme).then(|| theme::Theme::load(next.theme.as_deref())).transpose()?;
        let keymap = (next.keybindings != self.tui.keybindings).then(|| Keymap::load(&next.keybindings)).transpose()?;
        if self.persist_settings { settings::edit_global(&key, next_values)?; }
        if let Some(theme) = theme { self.theme = theme; }
        if let Some(keymap) = keymap { self.keymap = keymap; }
        self.tui = next;
        self.log.add(Kind::Notice, format!("{key} updated ({})", if self.persist_settings { "saved" } else { "this launch only" }));
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
            "mode" => self.set_mode(parse_mode(arg)?)?,
            "plan" => {
                let mode = self.perms.mode();
                if mode == Mode::Plan { self.set_mode(self.pre_plan)?; }
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
                self.switch(Session::fork(&self.meta.id)?)?;
            }
            "session" => self.log.add(Kind::Notice, format!("{} · {}\n{}", self.meta.id, self.meta.model, self.meta.cwd.display())),
            "export" => {
                anyhow::ensure!(self.busy.is_none(), "Wait for the turn before exporting");
                let path = crate::html::export(&self.meta.id, (!arg.is_empty()).then(|| self.meta.cwd.join(arg)))?;
                self.log.add(Kind::Notice, format!("Exported to {}", path.display()));
            }
            "skill" if !self.cfg.skills_enabled => bail!("Skills are disabled"),
            "skill" if arg.is_empty() => self.open_picker(Picker::Skill),
            "skill" => self.input.replace(greencode_core::skills::body(&self.meta.cwd, arg).context("Unknown skill")?),
            "agents" => {
                let text = if self.agents.is_empty() { "No sub-agent activity in this TUI yet".into() } else {
                    self.agents.iter().map(|(name, (state, detail))| format!("{name}: {state} {detail}")).collect::<Vec<_>>().join("\n")
                };
                self.log.add(Kind::Notice, text);
            }
            "to" | "stop" => {
                let (name, message) = if command == "to" { arg.split_once(char::is_whitespace).context("Use /to NAME TEXT")? } else { (arg, "") };
                anyhow::ensure!(self.busy.is_some() && self.agents.get(name).is_some_and(|(state, _)| state == "running"), "No active sub-agent named `{name}`");
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
            "exit" => self.quit()?,
            _ => unreachable!(),
        }
        Ok(())
    }
}

pub(super) fn parse_mode(mode: &str) -> Result<Mode> {
    Ok(match mode {
        "ask" => Mode::Ask, "auto-edit" => Mode::AutoEdit, "full" => Mode::Full, "plan" => Mode::Plan,
        _ => bail!("Unknown mode `{mode}`; choose ask, auto-edit, full, or plan"),
    })
}
