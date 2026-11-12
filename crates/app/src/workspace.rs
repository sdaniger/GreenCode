use crate::thread_view::{self, Item, Pal};
use gpui_kit::component::{
    input::{Input, InputEvent, InputState, Textarea, TextareaState},
    ActiveTheme, Theme,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use greencode_core::{
    config::Config,
    permission::{Mode, Perms},
    session::{Meta, Session},
    skills, worktree, Command, Decision, Event, Usage,
};
use std::{ops::Range, path::PathBuf, sync::Arc};
use tokio::sync::{mpsc, oneshot};

#[derive(Clone, Copy, PartialEq)]
enum Status {
    Idle,
    Running,
    /// Blocked on an approval.
    Waiting,
}

struct Thread {
    meta: Meta,
    /// `None` until the first prompt: an idle thread has no backend.
    cmd: Option<mpsc::UnboundedSender<Command>>,
    items: Vec<Item>,
    /// Only the visible thread and running ones keep their items in memory.
    loaded: bool,
    status: Status,
    pending: Option<(SharedString, SharedString, oneshot::Sender<Decision>)>,
    usage: Usage,
}

impl Thread {
    fn new(meta: Meta, loaded: bool) -> Thread {
        Thread { meta, cmd: None, items: vec![], loaded, status: Status::Idle, pending: None, usage: Usage::default() }
    }

    fn project(&self) -> PathBuf {
        self.meta.repo.clone().unwrap_or_else(|| self.meta.cwd.clone())
    }
}

#[derive(Clone)]
enum Act {
    NewThread,
    NewWorktree,
    OpenProject,
    ToggleDiff,
    Delete,
    Mode(Mode),
    Model(String),
    Backend(String),
    Thread(String),
}

pub struct Workspace {
    cfg: Arc<Config>,
    perms: Arc<Perms>,
    rt: tokio::runtime::Runtime,
    threads: Vec<Thread>,
    active: usize,
    input: Entity<TextareaState>,
    list: ListState,
    focus: FocusHandle,
    show_diff: bool,
    diff: Vec<SharedString>,
    palette: Option<(Entity<InputState>, Subscription)>,
    /// Slash commands of the active project.
    commands: Vec<String>,
    /// Tracked files of a project, for `@` completion.
    files: Option<(PathBuf, Vec<String>)>,
    _subs: Vec<Subscription>,
}

fn mode_label(m: Mode) -> &'static str {
    match m {
        Mode::Ask => "毎回確認",
        Mode::AutoEdit => "編集を自動許可",
        Mode::Full => "すべて許可",
    }
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

impl Workspace {
    pub fn new(cfg: Config, cfg_error: Option<String>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().expect("tokio runtime");
        let perms = Arc::new(Perms::new(cfg.mode));
        let input = cx.new(|cx| {
            TextareaState::new(window, cx)
                .auto_grow(1, 8)
                .submit_on_enter(true)
                .placeholder("メッセージを入力   / コマンド   @ ファイル")
        });
        let subs = vec![cx.subscribe_in(&input, window, |this, _, ev: &InputEvent, window, cx| match ev {
            InputEvent::PressEnter { shift: false, .. } => this.send(window, cx),
            InputEvent::Change => cx.notify(),
            _ => {}
        })];
        let mut cwd = std::env::current_dir().unwrap_or_default();
        if cwd.parent().is_none() {
            cwd = greencode_core::user_home(); // launched from Finder
        }
        let mut threads = vec![Thread::new(Meta::new(cwd, cfg.model.clone(), "native".into()), true)];
        threads.extend(Session::list().into_iter().map(|m| Thread::new(m, false)));
        if let Some(e) = cfg_error {
            threads[0].items.push(Item::Error(format!("設定を読み込めません: {e}").into()));
        }
        let mut this = Workspace {
            cfg: Arc::new(cfg),
            perms,
            rt,
            list: ListState::new(threads[0].items.len(), ListAlignment::Bottom, px(600.)),
            threads,
            active: 0,
            focus: cx.focus_handle(),
            input,
            show_diff: false,
            diff: vec![],
            palette: None,
            commands: vec![],
            files: None,
            _subs: subs,
        };
        this.activate(0, window, cx);
        this
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
        self.active = ix;
        let t = &mut self.threads[ix];
        if !t.loaded {
            // A thread that was never prompted has no session file yet.
            if Session::open(&t.meta.id).is_some() {
                match Session::load(&t.meta.id) {
                    Ok((_, messages)) => t.items = thread_view::items_from(&messages),
                    Err(e) => t.items = vec![Item::Error(format!("{e:#}").into())],
                }
            }
            t.loaded = true;
        }
        self.list.reset(t.items.len());
        let mut commands = vec!["compact".to_string()];
        commands.extend(skills::commands(&t.meta.cwd));
        self.commands = commands;
        self.refresh_diff(cx);
        self.input.update(cx, |s, cx| s.focus(window, cx));
        cx.notify();
    }

    fn select(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        if ix >= self.threads.len() || ix == self.active {
            return;
        }
        let old = &mut self.threads[self.active];
        if old.status == Status::Idle {
            old.items = Vec::new();
            old.loaded = false;
        }
        self.activate(ix, window, cx);
    }

    fn new_thread(&mut self, project: PathBuf, in_worktree: bool, window: &mut Window, cx: &mut Context<Self>) {
        let cur = &self.thread().meta;
        let mut meta = Meta::new(project.clone(), cur.model.clone(), cur.backend.clone());
        if in_worktree {
            match worktree::create(&project, &meta.id) {
                Ok(path) => {
                    meta.cwd = path;
                    meta.repo = Some(project);
                }
                Err(e) => return self.fail(format!("worktree を作成できません: {e:#}")),
            }
        }
        self.threads.insert(0, Thread::new(meta, true));
        self.active += 1;
        self.select(0, window, cx);
    }

    fn refresh_diff(&mut self, cx: &mut Context<Self>) {
        if !self.show_diff {
            return;
        }
        let cwd = self.thread().meta.cwd.clone();
        cx.spawn(async move |this, cx| {
            let diff = cx.background_executor().spawn(async move { worktree::diff(&cwd) }).await;
            this.update(cx, |this, cx| {
                this.diff = diff.lines().map(|l| SharedString::from(l.to_string())).collect();
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn send(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.input.read(cx).value().trim().to_string();
        if text.is_empty() || self.thread().status != Status::Idle {
            return;
        }
        self.input.update(cx, |s, cx| s.set_value("", window, cx));
        let (cfg, perms, handle) = (self.cfg.clone(), self.perms.clone(), self.rt.handle().clone());
        let t = &mut self.threads[self.active];
        if t.cmd.is_none() {
            let (tx, mut rx) = greencode_core::spawn(&handle, cfg, t.meta.clone(), perms);
            t.cmd = Some(tx);
            let id = t.meta.id.clone();
            cx.spawn(async move |this, cx| {
                while let Some(ev) = rx.recv().await {
                    if this.update(cx, |this, cx| this.on_event(&id, ev, cx)).is_err() {
                        break;
                    }
                }
            })
            .detach();
        }
        if t.meta.title.is_empty() {
            t.meta.title = text.chars().take(60).collect();
        }
        t.items.push(Item::User(text.clone().into()));
        t.status = Status::Running;
        let _ = t.cmd.as_ref().unwrap().send(Command::Prompt(text));
        let n = t.items.len();
        self.list.splice(n - 1..n - 1, 1);
        self.list.scroll_to_end();
        cx.notify();
    }

    fn on_event(&mut self, id: &str, ev: Event, cx: &mut Context<Self>) {
        let Some(ix) = self.threads.iter().position(|t| t.meta.id == id) else { return };
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
                t.items.push(Item::Tool { id, name, input, output: None, is_error: false, open: false });
                changed = Some((n, true));
            }
            Event::ToolEnd { id, output, is_error } => {
                changed = thread_view::finish_tool(&mut t.items, &id, output, is_error).map(|i| (i, false));
            }
            Event::Permission { tool, summary, reply } => {
                t.pending = Some((tool.into(), summary.into(), reply));
                t.status = Status::Waiting;
            }
            Event::Usage(u) => t.usage = u,
            Event::Notify(s) => {
                t.items.push(Item::Notice(s.into()));
                changed = Some((n, true));
            }
            Event::Error(s) => {
                t.items.push(Item::Error(s.into()));
                changed = Some((n, true));
            }
            Event::Done => {
                t.status = Status::Idle;
                t.pending = None;
                if visible {
                    self.refresh_diff(cx);
                }
            }
        }
        if let (true, Some((i, new))) = (visible, changed) {
            self.list.splice(if new { i..i } else { i..i + 1 }, 1);
        }
        cx.notify();
    }

    fn answer(&mut self, d: Decision, cx: &mut Context<Self>) {
        let t = &mut self.threads[self.active];
        if let Some((_, _, reply)) = t.pending.take() {
            let _ = reply.send(d);
            t.status = Status::Running;
            cx.notify();
        }
    }

    fn stop(&mut self) {
        if let Some(cmd) = &self.thread().cmd {
            let _ = cmd.send(Command::Cancel);
        }
    }

    // ---- completion ----

    fn files(&mut self) -> &[String] {
        let project = self.thread().meta.cwd.clone();
        if self.files.as_ref().is_none_or(|(p, _)| *p != project) {
            let out = std::process::Command::new("git").arg("-C").arg(&project).arg("ls-files").output();
            let list = out.map(|o| String::from_utf8_lossy(&o.stdout).lines().map(String::from).collect()).unwrap_or_default();
            self.files = Some((project, list));
        }
        &self.files.as_ref().unwrap().1
    }

    /// Completions for the word being typed: `/command` at the start, `@file` anywhere.
    fn suggestions(&mut self, cx: &App) -> Vec<String> {
        let value = self.input.read(cx).value();
        let word = value.rsplit(char::is_whitespace).next().unwrap_or("");
        if let Some(q) = value.strip_prefix('/').filter(|q| !q.contains(char::is_whitespace)) {
            self.commands.iter().filter(|c| c.starts_with(q)).take(6).map(|c| format!("/{c}")).collect()
        } else if let Some(q) = word.strip_prefix('@') {
            let q = q.to_lowercase();
            self.files().iter().filter(|f| f.to_lowercase().contains(&q)).take(6).map(|f| format!("@{f}")).collect()
        } else {
            vec![]
        }
    }

    fn complete(&mut self, s: &str, window: &mut Window, cx: &mut Context<Self>) {
        let value = self.input.read(cx).value().to_string();
        let start = value.rfind(char::is_whitespace).map_or(0, |i| i + 1);
        // `@path` is only a marker while typing; the model gets the plain path.
        let new = format!("{}{} ", &value[..start], s.strip_prefix('@').unwrap_or(s));
        self.input.update(cx, |st, cx| st.set_value(new, window, cx));
        cx.notify();
    }

    // ---- palette and actions ----

    fn acts(&self) -> Vec<(String, Act)> {
        let mut v = vec![
            ("新規スレッド  ⌘N".to_string(), Act::NewThread),
            ("新規スレッド（worktree で分離）  ⇧⌘N".into(), Act::NewWorktree),
            ("プロジェクトを開く…  ⌘O".into(), Act::OpenProject),
            ("差分パネルを切り替え  ⌘D".into(), Act::ToggleDiff),
            ("このスレッドを削除".into(), Act::Delete),
        ];
        for m in [Mode::Ask, Mode::AutoEdit, Mode::Full] {
            v.push((format!("権限: {}", mode_label(m)), Act::Mode(m)));
        }
        for m in self.cfg.model_list() {
            v.push((format!("モデル: {m}"), Act::Model(m)));
        }
        for b in self.backends() {
            v.push((format!("バックエンド: {b}"), Act::Backend(b)));
        }
        for t in &self.threads {
            if !t.meta.title.is_empty() {
                v.push((format!("スレッド: {}", t.meta.title), Act::Thread(t.meta.id.clone())));
            }
        }
        v
    }

    fn backends(&self) -> Vec<String> {
        std::iter::once("native".to_string()).chain(self.cfg.backends.iter().map(|b| b.name.clone())).collect()
    }

    fn filtered_acts(&self, cx: &App) -> Vec<(String, Act)> {
        let q = self.palette.as_ref().map(|(p, _)| p.read(cx).value().to_lowercase()).unwrap_or_default();
        self.acts().into_iter().filter(|(label, _)| label.to_lowercase().contains(&q)).take(12).collect()
    }

    fn toggle_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.palette.take().is_some() {
            self.input.update(cx, |s, cx| s.focus(window, cx));
        } else {
            let state = cx.new(|cx| InputState::new(window, cx).placeholder("コマンド、モデル、スレッドを検索"));
            let sub = cx.subscribe_in(&state, window, |this, _, ev: &InputEvent, window, cx| match ev {
                InputEvent::PressEnter { .. } => {
                    if let Some((_, act)) = this.filtered_acts(cx).into_iter().next() {
                        this.run(act, window, cx);
                    }
                }
                InputEvent::Change => cx.notify(),
                _ => {}
            });
            state.update(cx, |s, cx| s.focus(window, cx));
            self.palette = Some((state, sub));
        }
        cx.notify();
    }

    fn run(&mut self, act: Act, window: &mut Window, cx: &mut Context<Self>) {
        if self.palette.is_some() {
            self.toggle_palette(window, cx);
        }
        match act {
            Act::NewThread => self.new_thread(self.thread().project(), false, window, cx),
            Act::NewWorktree => self.new_thread(self.thread().project(), true, window, cx),
            Act::OpenProject => {
                let rx = cx.prompt_for_paths(PathPromptOptions { files: false, directories: true, multiple: false, prompt: None });
                cx.spawn_in(window, async move |this, cx| {
                    if let Ok(Ok(Some(paths))) = rx.await {
                        if let Some(p) = paths.into_iter().next() {
                            this.update_in(cx, |this, window, cx| this.new_thread(p, false, window, cx)).ok();
                        }
                    }
                })
                .detach();
            }
            Act::ToggleDiff => {
                self.show_diff = !self.show_diff;
                self.refresh_diff(cx);
            }
            Act::Delete => {
                // The session file goes; a worktree and its branch are left on disk on purpose.
                let t = self.threads.remove(self.active);
                Session::delete(&t.meta.id);
                if self.threads.is_empty() {
                    self.threads.push(Thread::new(Meta::new(t.project(), t.meta.model, "native".into()), true));
                }
                self.activate(self.active.min(self.threads.len() - 1), window, cx);
            }
            Act::Mode(m) => self.perms.set_mode(m),
            Act::Model(m) => {
                let t = &mut self.threads[self.active];
                if let Some(cmd) = &t.cmd {
                    let _ = cmd.send(Command::Model(m.clone()));
                }
                t.meta.model = m;
            }
            Act::Backend(b) => {
                let t = &mut self.threads[self.active];
                // An external agent owns its own history, so the backend is fixed once a thread has started.
                if t.cmd.is_none() && t.items.is_empty() {
                    t.meta.backend = b;
                } else {
                    self.fail("バックエンドは新規スレッドでのみ変更できます");
                }
            }
            Act::Thread(id) => {
                if let Some(ix) = self.threads.iter().position(|t| t.meta.id == id) {
                    self.select(ix, window, cx);
                }
            }
        }
        cx.notify();
    }

    fn cycle<T: PartialEq + Clone>(list: &[T], cur: &T) -> T {
        let i = list.iter().position(|x| x == cur).map_or(0, |i| (i + 1) % list.len());
        list[i].clone()
    }

    fn on_key(&mut self, ev: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let k = &ev.keystroke;
        let (cmd, shift) = (k.modifiers.platform, k.modifiers.shift);
        match k.key.as_str() {
            "escape" if self.palette.is_some() => self.toggle_palette(window, cx),
            "escape" if self.thread().pending.is_some() => self.answer(Decision::Deny, cx),
            "escape" if self.thread().status == Status::Running => self.stop(),
            "enter" if cmd && self.thread().pending.is_some() => {
                self.answer(if shift { Decision::Always } else { Decision::Allow }, cx)
            }
            "tab" if !shift && self.palette.is_none() => match self.suggestions(cx).into_iter().next() {
                Some(s) => self.complete(&s, window, cx),
                None => return,
            },
            "n" if cmd => self.run(if shift { Act::NewWorktree } else { Act::NewThread }, window, cx),
            "o" if cmd => self.run(Act::OpenProject, window, cx),
            "d" if cmd => self.run(Act::ToggleDiff, window, cx),
            "k" if cmd => self.toggle_palette(window, cx),
            d if cmd && d.len() == 1 && ("1"..="9").contains(&d) => {
                self.select(d.parse::<usize>().unwrap() - 1, window, cx)
            }
            _ => return,
        }
        cx.stop_propagation();
    }

    // ---- rendering ----

    fn sidebar(&self, pal: Pal, cx: &mut Context<Self>) -> impl IntoElement {
        // Threads grouped by project, in order of first appearance.
        let mut groups: Vec<(PathBuf, Vec<usize>)> = Vec::new();
        for (i, t) in self.threads.iter().enumerate() {
            let p = t.project();
            match groups.iter_mut().find(|(g, _)| *g == p) {
                Some((_, v)) => v.push(i),
                None => groups.push((p, vec![i])),
            }
        }
        let mut body = div().id("threads").flex_1().overflow_y_scroll().px_2().pb_2();
        for (project, ixs) in groups {
            let name = project.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| project.display().to_string());
            body = body.child(div().px_2().pt_3().pb_1().text_xs().font_weight(FontWeight::MEDIUM).text_color(pal.muted).child(SharedString::from(name)));
            for i in ixs {
                let t = &self.threads[i];
                let title: SharedString = if t.meta.title.is_empty() { "新しいスレッド".into() } else { t.meta.title.clone().into() };
                let dot = match t.status {
                    Status::Running => Some(pal.accent),
                    Status::Waiting => Some(pal.warn),
                    Status::Idle => None,
                };
                body = body.child(
                    div()
                        .id(("thread", i))
                        .flex()
                        .items_center()
                        .gap_2()
                        .px_2()
                        .py_1()
                        .rounded_md()
                        .cursor_pointer()
                        .when(i == self.active, |d| d.bg(pal.selected))
                        .when(i != self.active, |d| d.hover(|s| s.bg(pal.hover)))
                        .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| this.select(i, window, cx)))
                        .child(div().flex_1().truncate().child(title))
                        .when(t.meta.repo.is_some(), |d| d.child(div().text_xs().text_color(pal.muted).child("worktree")))
                        .when_some(dot, |d, c| d.child(div().size_2().rounded_full().bg(c))),
                );
            }
        }
        div()
            .w(px(248.))
            .h_full()
            .flex()
            .flex_col()
            .flex_shrink_0()
            .bg(pal.sidebar)
            .border_r_1()
            .border_color(pal.line)
            // Room for the traffic lights; the strip also drags the window.
            .child(div().h(px(44.)).on_mouse_down(MouseButton::Left, |_, window, _| window.start_window_move()))
            .child(
                div().px_2().child(
                    chip("new", "＋ 新規スレッド", pal)
                        .text_sm()
                        .py_1()
                        .on_click(cx.listener(|this, _: &ClickEvent, window, cx| this.run(Act::NewThread, window, cx))),
                ),
            )
            .child(body)
            .child(
                div().p_2().border_t_1().border_color(pal.line).child(
                    chip("open", "プロジェクトを開く…", pal)
                        .on_click(cx.listener(|this, _: &ClickEvent, window, cx| this.run(Act::OpenProject, window, cx))),
                ),
            )
    }

    fn item(&mut self, ix: usize, cx: &mut Context<Self>) -> AnyElement {
        let pal = Pal::new(cx.theme().is_dark());
        let mono = cx.theme().mono_font_family.clone();
        let Some(item) = self.threads[self.active].items.get(ix) else { return div().into_any_element() };
        let toggle = cx.listener(move |this, _: &ClickEvent, _, cx| {
            if let Some(Item::Tool { open, .. }) = this.threads[this.active].items.get_mut(ix) {
                *open = !*open;
                this.list.splice(ix..ix + 1, 1);
                cx.notify();
            }
        });
        thread_view::render_item(ix, item, pal, mono, toggle)
    }

    fn composer(&mut self, pal: Pal, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let suggestions = self.suggestions(cx);
        let t = self.thread();
        let status = t.status;
        let used = t.usage.context();
        let usage = (used > 0).then(|| format!("{:.1}k トークン · キャッシュ {}%", used as f64 / 1000., t.usage.cache_read * 100 / used));
        let (model, backend) = (t.meta.model.clone(), t.meta.backend.clone());
        let models = self.cfg.model_list();
        let backends = self.backends();
        let mode = self.perms.mode();
        let can_send = status == Status::Idle;
        let _ = window;

        let mut col = div().flex().flex_col().gap_2().px_6().pb_4().pt_1();
        if let Some((tool, summary, _)) = &t.pending {
            let button = |id: &'static str, label: &'static str, d: Decision, primary: bool, cx: &mut Context<Self>| {
                div()
                    .id(id)
                    .px_3()
                    .py_1()
                    .rounded_md()
                    .text_sm()
                    .cursor_pointer()
                    .when(primary, |b| b.bg(pal.accent).text_color(gpui_kit::white()))
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
                    .bg(pal.hover)
                    .child(div().text_sm().child(SharedString::from(format!("{tool} の実行を許可しますか？"))))
                    .child(div().font_family(cx.theme().mono_font_family.clone()).text_xs().text_color(pal.muted).max_h(px(120.)).overflow_hidden().child(summary.clone()))
                    .child(
                        div()
                            .flex()
                            .gap_2()
                            .child(button("allow", "許可  ⌘⏎", Decision::Allow, true, cx))
                            .child(button("always", "常に許可  ⇧⌘⏎", Decision::Always, false, cx))
                            .child(button("deny", "拒否  esc", Decision::Deny, false, cx)),
                    ),
            );
        }
        if !suggestions.is_empty() {
            let mut row = div().flex().flex_wrap().gap_1();
            for (i, s) in suggestions.into_iter().enumerate() {
                let text = s.clone();
                row = row.child(
                    chip(("suggest", i), s, pal)
                        .when(i == 0, |c| c.bg(pal.hover).text_color(pal.fg))
                        .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| this.complete(&text, window, cx))),
                );
            }
            col = col.child(row.child(div().text_xs().text_color(pal.muted).py_0p5().child("tab で補完")));
        }
        col.child(
            div()
                .flex()
                .flex_col()
                .rounded_xl()
                .border_1()
                .border_color(pal.line)
                .bg(pal.hover)
                .child(div().px_2().pt_1().child(Textarea::new(&self.input).appearance(false)))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_1()
                        .px_2()
                        .pb_1p5()
                        .child(chip("model", model.clone(), pal).on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                            this.run(Act::Model(Self::cycle(&models, &model)), window, cx)
                        })))
                        .child(chip("backend", backend.clone(), pal).on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                            this.run(Act::Backend(Self::cycle(&backends, &backend)), window, cx)
                        })))
                        .child(chip("mode", mode_label(mode), pal).on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                            this.run(Act::Mode(Self::cycle(&[Mode::Ask, Mode::AutoEdit, Mode::Full], &mode)), window, cx)
                        })))
                        .child(div().flex_1())
                        .when_some(usage, |d, u| d.child(div().text_xs().text_color(pal.muted).child(SharedString::from(u))))
                        .when(status == Status::Running, |d| d.child(div().text_xs().text_color(pal.muted).child("実行中")))
                        .child(
                            div()
                                .id("send")
                                .ml_2()
                                .px_3()
                                .py_0p5()
                                .rounded_md()
                                .text_sm()
                                .cursor_pointer()
                                .when(can_send, |b| b.bg(pal.accent).text_color(gpui_kit::white()).child("送信"))
                                .when(!can_send, |b| b.bg(pal.selected).child("停止  esc"))
                                .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                                    if can_send { this.send(window, cx) } else { this.stop() }
                                })),
                        ),
                ),
        )
    }

    fn main_panel(&mut self, pal: Pal, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let composer = self.composer(pal, window, cx).into_any_element();
        let t = self.thread();
        let title: SharedString = if t.meta.title.is_empty() { "新しいスレッド".into() } else { t.meta.title.clone().into() };
        let path = SharedString::from(t.meta.cwd.display().to_string());
        let body = if t.items.is_empty() {
            div()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .text_color(pal.muted)
                .child("何を作りますか？")
                .into_any_element()
        } else {
            list(self.list.clone(), cx.processor(|this: &mut Self, ix: usize, _, cx| this.item(ix, cx))).flex_1().into_any_element()
        };
        div()
            .flex_1()
            .min_w_0()
            .h_full()
            .flex()
            .flex_col()
            .bg(pal.panel)
            .child(
                div()
                    .h(px(44.))
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .gap_3()
                    .px_6()
                    .border_b_1()
                    .border_color(pal.line)
                    .on_mouse_down(MouseButton::Left, |_, window, _| window.start_window_move())
                    .child(div().font_weight(FontWeight::MEDIUM).truncate().child(title))
                    .child(div().flex_1().text_xs().text_color(pal.muted).truncate().child(path))
                    .child(chip("palette", "⌘K", pal).on_click(cx.listener(|this, _: &ClickEvent, window, cx| this.toggle_palette(window, cx))))
                    .child(
                        chip("diff", "差分  ⌘D", pal)
                            .when(self.show_diff, |c| c.bg(pal.selected).text_color(pal.fg))
                            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| this.run(Act::ToggleDiff, window, cx))),
                    ),
            )
            .child(body)
            .child(composer)
    }

    fn diff_panel(&self, pal: Pal, cx: &mut Context<Self>) -> impl IntoElement {
        let mono = cx.theme().mono_font_family.clone();
        let body = if self.diff.is_empty() {
            div().flex_1().flex().items_center().justify_center().text_sm().text_color(pal.muted).child("変更はありません").into_any_element()
        } else {
            uniform_list(
                "diff",
                self.diff.len(),
                cx.processor(move |this: &mut Self, range: Range<usize>, _, _| {
                    range
                        .map(|i| {
                            let l = &this.diff[i];
                            let color = if l.starts_with("+++") || l.starts_with("---") || l.starts_with("diff ") {
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
                            div().px_3().whitespace_nowrap().text_color(color).child(l.clone())
                        })
                        .collect::<Vec<_>>()
                }),
            )
            .flex_1()
            .font_family(mono)
            .text_xs()
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
                    .child("未コミットの変更"),
            )
            .child(body)
    }

    fn palette_view(&self, pal: Pal, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let (state, _) = self.palette.as_ref()?;
        let mut rows = div().flex().flex_col().p_1();
        for (i, (label, act)) in self.filtered_acts(cx).into_iter().enumerate() {
            rows = rows.child(
                div()
                    .id(("act", i))
                    .px_3()
                    .py_1()
                    .rounded_md()
                    .cursor_pointer()
                    .when(i == 0, |d| d.bg(pal.selected))
                    .hover(|s| s.bg(pal.selected))
                    .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| this.run(act.clone(), window, cx)))
                    .child(SharedString::from(label)),
            );
        }
        Some(
            div().absolute().inset_0().flex().justify_center().pt(px(110.)).child(
                div()
                    .w(px(540.))
                    .h_auto()
                    .flex()
                    .flex_col()
                    .self_start()
                    .rounded_xl()
                    .border_1()
                    .border_color(pal.line)
                    .bg(pal.panel.opacity(0.97))
                    .shadow_lg()
                    .child(div().p_2().border_b_1().border_color(pal.line).child(Input::new(state).appearance(false)))
                    .child(rows),
            ),
        )
    }
}

impl Render for Workspace {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // The kit paints the window surface with this color; clear it so the blur shows through.
        if cx.theme().tokens.background.a != 0. {
            Theme::global_mut(cx).tokens.background = gpui_kit::transparent_black().into();
        }
        let pal = Pal::new(cx.theme().is_dark());
        let sidebar = self.sidebar(pal, cx).into_any_element();
        let main = self.main_panel(pal, window, cx).into_any_element();
        let diff = self.show_diff.then(|| self.diff_panel(pal, cx).into_any_element());
        let palette = self.palette_view(pal, cx).map(|p| p.into_any_element());
        div()
            .size_full()
            .flex()
            .text_color(pal.fg)
            .text_sm()
            .track_focus(&self.focus)
            .capture_key_down(cx.listener(Self::on_key))
            .child(sidebar)
            .child(main)
            .children(diff)
            .children(palette)
    }
}

#[cfg(test)]
mod tests {
    // No glob import: with test-support, `gpui_kit::*` brings a `test` macro that shadows `#[test]`.
    use super::{Item, Status, Workspace};
    use gpui_kit::{Entity, TestAppContext, VisualTestContext};
    use greencode_core::{config::Config, Decision, Event};
    use tokio::sync::oneshot;

    fn open(cx: &mut TestAppContext) -> (Entity<Workspace>, &mut VisualTestContext) {
        let home = std::env::temp_dir().join(format!("greencode-ui-test-{}", std::process::id()));
        std::env::set_var("GREENCODE_HOME", &home);
        cx.update(gpui_kit::init);
        let (ws, cx) = cx.add_window_view(|window, cx| Workspace::new(Config::default(), None, window, cx));
        cx.run_until_parked();
        (ws, cx)
    }

    fn ask(ws: &Entity<Workspace>, cx: &mut VisualTestContext) -> oneshot::Receiver<Decision> {
        let (reply, rx) = oneshot::channel();
        ws.update(cx, |ws, cx| {
            let id = ws.thread().meta.id.clone();
            ws.on_event(&id, Event::Permission { tool: "bash".into(), summary: "ls".into(), reply }, cx);
        });
        cx.run_until_parked();
        rx
    }

    #[gpui_kit::test]
    async fn approvals_answer_from_the_keyboard(cx: &mut TestAppContext) {
        let (ws, cx) = open(cx);
        for (keys, expected) in [("cmd-enter", Decision::Allow), ("cmd-shift-enter", Decision::Always), ("escape", Decision::Deny)] {
            let mut rx = ask(&ws, cx);
            assert!(ws.read_with(cx, |ws, _| ws.thread().status == Status::Waiting));
            cx.simulate_keystrokes(keys);
            assert_eq!(rx.try_recv().ok(), Some(expected), "{keys}");
            assert!(ws.read_with(cx, |ws, _| ws.thread().pending.is_none()));
        }
    }

    #[gpui_kit::test]
    async fn threads_palette_and_completion(cx: &mut TestAppContext) {
        let (ws, cx) = open(cx);
        // A never-saved thread survives switching away and back.
        cx.simulate_keystrokes("cmd-n");
        assert_eq!(ws.read_with(cx, |ws, _| (ws.threads.len(), ws.active)), (2, 0));
        cx.simulate_keystrokes("cmd-2");
        cx.simulate_keystrokes("cmd-1");
        assert!(ws.read_with(cx, |ws, _| ws.active == 0 && ws.threads.iter().all(|t| t.items.is_empty())));

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
        assert_eq!(ws.read_with(cx, |ws, cx| ws.input.read(cx).value().to_string()), "/compact ");

        // Streamed text extends the last assistant item instead of adding rows.
        ws.update(cx, |ws, cx| {
            let id = ws.thread().meta.id.clone();
            for part in ["Hel", "lo"] {
                ws.on_event(&id, Event::Text(part.into()), cx);
            }
            ws.on_event(&id, Event::Done, cx);
            assert!(matches!(&ws.thread().items[..], [Item::Assistant(t)] if t == "Hello"));
        });
    }
}
