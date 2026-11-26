//! First-run welcome page: detected agents and recent Claude Code / Codex / Pi projects to import.

use super::*;
use greencode_core::agents::AGENTS;

pub(super) struct Welcome {
    projects: Vec<import::Project>,
    checked: Vec<bool>,
    /// (imported, total) while an import runs.
    importing: Option<(usize, usize)>,
}

#[derive(Clone)]
pub(super) enum WelcomeAct {
    Open,
    Toggle(usize),
    All(bool),
    Start,
    Install(&'static str),
    Login(&'static str),
}

/// Fresh installs have no saved thread; anyone who has used GreenCode, or finished this page, skips it.
pub(super) fn first_run(ui: &Value, saved_threads: usize) -> bool {
    ui["welcome_done"] != true && saved_threads == 0
}

impl Workspace {
    pub(super) fn welcome_act(&mut self, act: WelcomeAct, window: &mut Window, cx: &mut Context<Self>) {
        match act {
            WelcomeAct::Open => self.open_welcome(cx),
            WelcomeAct::Toggle(i) => {
                if let Some(c) = self.welcome.as_mut().and_then(|w| w.checked.get_mut(i)) {
                    *c = !*c;
                }
            }
            WelcomeAct::All(on) => {
                if let Some(w) = self.welcome.as_mut() {
                    w.checked.iter_mut().for_each(|c| *c = on);
                }
            }
            WelcomeAct::Start => self.start_welcome(cx),
            // Installing and signing in only uncovers the terminal; setup is not done until 始める.
            WelcomeAct::Install(name) => {
                self.welcome = None;
                self.install_agent(name, window, cx);
            }
            WelcomeAct::Login(name) => {
                let command = if name == "claude" { "claude auth login" } else { "codex login" };
                self.welcome = None;
                self.settings = None;
                if !self.terminal_visible {
                    self.open_terminal(window, cx);
                }
                if self.active_terminal().is_some() {
                    if cfg!(test) {
                        self.active_terminal_mut().unwrap().lines.push(format!("$ {command}").into());
                    } else {
                        self.write_terminal(format!("{command}\n").into_bytes());
                    }
                }
            }
        }
        cx.notify();
    }

    pub(super) fn open_welcome(&mut self, cx: &mut Context<Self>) {
        self.settings = None;
        self.welcome = Some(Welcome { projects: vec![], checked: vec![], importing: None });
        self.refresh_agent_versions(cx);
        if cfg!(test) {
            return;
        }
        let task = cx.background_executor().spawn(async { import::scan_projects() });
        cx.spawn(async move |this, cx| {
            let projects = task.await;
            this.update(cx, |this, cx| {
                let known: Vec<_> = this.threads.iter().map(|t| t.meta.cwd.clone()).collect();
                if let Some(w) = this.welcome.as_mut() {
                    // Git repositories with several conversations start checked, unless already a project here.
                    w.checked = projects.iter().map(|p| p.sessions.len() >= 3 && p.dir.join(".git").exists() && !known.contains(&p.dir)).collect();
                    w.projects = projects;
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    fn finish_welcome(&mut self) {
        self.welcome = None;
        self.ui["welcome_done"] = json!(true);
        self.save_ui();
    }

    fn start_welcome(&mut self, cx: &mut Context<Self>) {
        let Some(w) = self.welcome.as_mut().filter(|w| w.importing.is_none()) else { return };
        let found: Vec<_> = w.projects.iter().zip(&w.checked).filter(|(_, on)| **on).flat_map(|(p, _)| p.sessions.clone()).collect();
        if found.is_empty() {
            return self.finish_welcome();
        }
        w.importing = Some((0, found.len()));
        let model = if self.thread().meta.backend == "native" { self.thread().meta.model.clone() } else { self.cfg.model.clone() };
        cx.spawn(async move |this, cx| {
            let mut failed = 0;
            for f in found {
                let model = model.clone();
                let result = cx.background_executor().spawn(async move { import::import_found(&f, &model) }).await;
                let ok = this
                    .update(cx, |this, cx| {
                        if let Ok(meta) = &result {
                            this.threads.insert(0, Thread::new(meta.clone(), false, this.cfg.mode));
                            this.active += 1;
                        }
                        if let Some((done, _)) = this.welcome.as_mut().and_then(|w| w.importing.as_mut()) {
                            *done += 1;
                        }
                        cx.notify();
                    })
                    .is_ok();
                failed += result.is_err() as usize;
                if !ok {
                    return;
                }
            }
            this.update(cx, |this, cx| {
                this.finish_welcome();
                if failed > 0 {
                    this.notice(format!("{failed} 件の会話をインポートできませんでした"));
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    pub(super) fn welcome_view(&self, pal: Pal, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let w = self.welcome.as_ref()?;
        let button = |id: &'static str, label: String, act: WelcomeAct, cx: &mut Context<Self>| {
            chip(id, label, pal)
                .border_1()
                .border_color(pal.line)
                .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| this.welcome_act(act.clone(), window, cx)))
        };
        let ready = |name: &str| matches!(self.agent_versions.get(name), Some(Some(_)));
        let mut sorted: Vec<_> = AGENTS.iter().collect();
        sorted.sort_by_key(|a| (!ready(a.name), a.name));
        let agent_row = |ix: usize, a: &'static greencode_core::agents::AgentInfo, cx: &mut Context<Self>| {
            let status = match self.agent_versions.get(a.name) {
                Some(Some(v)) => format!("準備完了 {v}"),
                Some(None) => "未インストール".into(),
                None => "確認中…".into(),
            };
            let mut r = div().flex().items_center().gap_2().min_h(px(30.)).child(div().flex_1().child(a.name)).child(div().text_xs().text_color(pal.muted).child(status));
            if ready(a.name) {
                if matches!(a.name, "claude" | "codex") {
                    r = r.child(button("wl", "ログイン".into(), WelcomeAct::Login(a.name), cx).id(("welcome-login", ix)));
                }
            } else {
                r = r.child(button("wi", "インストール".into(), WelcomeAct::Install(a.name), cx).id(("welcome-install", ix)));
            }
            r
        };
        let (found, more): (Vec<_>, Vec<_>) = sorted.into_iter().partition(|a| ready(a.name));
        let any_ready = !found.is_empty();
        let section = |title: &'static str| div().mt_5().pb_1().font_weight(FontWeight::SEMIBOLD).child(title);
        let mut page = div()
            .w_full()
            .max_w(px(720.))
            .mx_auto()
            .px_8()
            .py_10()
            .child(div().text_2xl().font_weight(FontWeight::SEMIBOLD).child("GreenCode へようこそ"))
            .child(div().text_color(pal.muted).child("このコンピューターで見つかったエージェントとプロジェクトです。始めるにはエージェントが1つあれば十分です。"))
            .child(section(if any_ready { "エージェント" } else { "エージェントを設定" }))
            .children(found.iter().enumerate().map(|(i, a)| agent_row(i, a, cx)))
            .child(section("さらにエージェントを追加"))
            .children(more.iter().enumerate().map(|(i, a)| agent_row(i + 100, a, cx)))
            .child(section("プロジェクト"));
        if w.projects.is_empty() {
            page = page.child(div().text_color(pal.muted).child("過去30日に Claude Code / Codex / Pi で使われたプロジェクトは見つかりませんでした"));
        } else {
            page = page.child(
                div()
                    .flex()
                    .gap_2()
                    .child(button("wa", "すべて選択".into(), WelcomeAct::All(true), cx))
                    .child(button("wn", "選択解除".into(), WelcomeAct::All(false), cx)),
            );
        }
        for (i, p) in w.projects.iter().enumerate() {
            let mark = if w.checked[i] { "☑" } else { "☐" };
            page = page.child(
                div()
                    .id(("welcome-project", i))
                    .flex()
                    .gap_2()
                    .py_1()
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| this.welcome_act(WelcomeAct::Toggle(i), window, cx)))
                    .child(mark)
                    .child(div().flex_1().truncate().child(p.dir.display().to_string()))
                    .child(div().text_xs().text_color(pal.muted).child(format!("{} 件の会話", p.sessions.len()))),
            );
        }
        let start = match (w.importing, any_ready) {
            (Some((done, total)), _) => format!("インポート中… {done}/{total}"),
            (None, true) => "始める".into(),
            (None, false) => "エージェントなしで始める".into(),
        };
        page = page.child(div().mt_6().flex().child(button("ws", start, WelcomeAct::Start, cx).bg(pal.accent).text_color(pal.accent_fg)));
        Some(div().absolute().inset_0().bg(pal.panel).occlude().child(div().id("welcome").size_full().overflow_y_scroll().child(page)))
    }
}

#[cfg(test)]
mod tests {
    // Not `use super::*`: that imports `gpui_kit::*`'s `test` macro, which shadows `#[test]`.
    use super::first_run;
    use crate::workspace::Workspace;
    use gpui_kit::AppContext;
    use greencode_core::config::Config;
    use serde_json::json;

    #[gpui_kit::test]
    async fn fresh_home_shows_welcome_and_existing_home_does_not(cx: &mut gpui_kit::TestAppContext) {
        let (ws, cx) = super::super::tests::open_fresh(cx);
        ws.read_with(cx, |ws, _| assert!(ws.welcome.is_some()));
        // A finished page, or any saved thread, skips it.
        assert!(!first_run(&json!({"welcome_done": true}), 0));
        assert!(!first_run(&json!({}), 1));
        ws.update(cx, |ws, cx| {
            ws.finish_welcome();
            cx.notify();
        });
        ws.read_with(cx, |ws, _| {
            assert!(ws.welcome.is_none());
            assert_eq!(ws.ui["welcome_done"], true);
        });
        // The saved flag is what a later launch reads: the same home opens without the page.
        let again = cx.update(|window, cx| {
            cx.new(|cx| Workspace::new(Config::load().unwrap(), None, window, cx))
        });
        again.read_with(cx, |ws, _| assert!(ws.welcome.is_none()));
    }
}
