//! Workflows page: durable runs started by `greencode-cli workflow` or a trigger.
//! The engine itself lives in `greencode_core::workflow`; this page only reads run files and steers them.
use super::*;
use greencode_core::workflow::{self, Definition, Run, RunStatus, StepState};

pub(super) struct Workflows {
    pub(super) defs: workflow::Definitions,
    pub(super) runs: Vec<Run>,
    /// 0 = Needs you, 1 = Running, 2 = All.
    pub(super) tab: usize,
    pub(super) error: Option<String>,
}

impl Workflows {
    pub(super) fn load() -> Self {
        let paths = workflow::paths();
        Self {
            defs: workflow::load_definitions(&paths),
            runs: workflow::all_runs(&paths),
            tab: 0,
            error: None,
        }
    }

    pub(super) fn reload(&mut self) {
        let paths = workflow::paths();
        self.defs = workflow::load_definitions(&paths);
        self.runs = workflow::all_runs(&paths);
    }

    fn count(&self, tab: usize) -> usize {
        self.runs
            .iter()
            .filter(|r| match tab {
                0 => r.needs_you(),
                1 => r.auto_pending(),
                _ => true,
            })
            .count()
    }

    fn visible(&self) -> Vec<&Run> {
        self.runs
            .iter()
            .filter(|r| match self.tab {
                0 => r.needs_you(),
                1 => r.auto_pending(),
                _ => true,
            })
            .collect()
    }
}

fn step_label(state: StepState) -> &'static str {
    match state {
        StepState::Pending => "待機",
        StepState::Running => "実行中",
        StepState::Waiting => "回答待ち",
        StepState::Done => "完了",
        StepState::Failed => "失敗",
    }
}

fn trigger_label(def: &Definition) -> Option<String> {
    if let Some(every) = &def.trigger.every {
        Some(format!("定期: {every} ごと"))
    } else if let Some(daily) = &def.trigger.daily {
        Some(format!("毎日 {daily}"))
    } else {
        def.trigger
            .webhook
            .as_ref()
            .map(|secret| format!("Webhook: http://127.0.0.1:{}/workflows/{secret}", workflow::WEBHOOK_PORT))
    }
}

impl Workspace {
    pub(super) fn toggle_workflows(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.workflows.take().is_some() {
            self.input.update(cx, |s, cx| s.focus(window, cx));
            cx.notify();
            return;
        }
        self.workflows = Some(Workflows::load());
        self.poll_workflows(cx);
        cx.notify();
    }

    /// Refreshes the open page every two seconds; ends when the page closes.
    fn poll_workflows(&self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(std::time::Duration::from_secs(2))
                    .await;
                let open = this
                    .update(cx, |this, cx| match &mut this.workflows {
                        Some(w) => {
                            w.reload();
                            cx.notify();
                            true
                        }
                        None => false,
                    })
                    .unwrap_or(false);
                if !open {
                    break;
                }
            }
        })
        .detach();
    }

    /// Starts schedules, webhooks and due runs while the app runs; no-op until Labs → Workflows is on.
    pub(super) fn start_workflow_engine(&mut self, _cx: &mut Context<Self>) {
        if cfg!(test) || self.workflow_engine.is_some() || self.ui["labs_workflows"] != true {
            return;
        }
        let cfg = self.cfg.clone();
        let workdir = self.thread().project();
        let task = self.rt.spawn(greencode_core::workflow::orchestrate(
            cfg,
            workdir,
            Some(greencode_core::workflow::WEBHOOK_PORT),
        ));
        self.workflow_engine = Some(task.abort_handle());
    }

    pub(super) fn stop_workflow_engine(&mut self) {
        if let Some(task) = self.workflow_engine.take() {
            task.abort();
        }
    }

    fn advance_workflow(&mut self, id: String, cx: &mut Context<Self>) {
        let rt = workflow::Runtime::new(self.cfg.clone(), self.thread().project());
        let task = self.rt.spawn(async move {
            rt.advance_run(&id)
                .await
                .map(|_| ())
                .map_err(|e| format!("{e:#}"))
        });
        cx.spawn(async move |this, cx| {
            let error = task.await.unwrap_or_else(|e| Err(e.to_string()));
            this.update(cx, |this, cx| {
                if let Err(e) = error {
                    this.fail(e);
                }
                if let Some(w) = &mut this.workflows {
                    w.reload();
                    w.error = None;
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    pub(super) fn workflow_answer(&mut self, id: &str, step: &str, choice: &str, cx: &mut Context<Self>) {
        let rt = workflow::Runtime::new(self.cfg.clone(), self.thread().project());
        match rt.answer(id, step, choice) {
            Ok(_) => {
                self.notice(format!("回答しました: {choice}"));
                self.advance_workflow(id.to_string(), cx);
            }
            Err(e) => self.fail(format!("回答できません: {e:#}")),
        }
        if let Some(w) = &mut self.workflows {
            w.reload();
        }
        cx.notify();
    }

    pub(super) fn workflow_resume(&mut self, id: &str, cx: &mut Context<Self>) {
        let rt = workflow::Runtime::new(self.cfg.clone(), self.thread().project());
        match rt.resume(id) {
            Ok(_) => {
                self.notice("実行を再開しました");
                self.advance_workflow(id.to_string(), cx);
            }
            Err(e) => self.fail(format!("再開できません: {e:#}")),
        }
        if let Some(w) = &mut self.workflows {
            w.reload();
        }
        cx.notify();
    }

    pub(super) fn workflow_cancel(&mut self, id: &str, cx: &mut Context<Self>) {
        let rt = workflow::Runtime::new(self.cfg.clone(), self.thread().project());
        match rt.cancel(id, Some("ユーザーがキャンセル")) {
            Ok(_) => self.notice(format!("実行 {id} をキャンセルしました")),
            Err(e) => self.fail(format!("キャンセルできません: {e:#}")),
        }
        if let Some(w) = &mut self.workflows {
            w.reload();
        }
        cx.notify();
    }

    pub(super) fn workflows_view(&self, pal: Pal, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let view = self.workflows.as_ref()?;
        let enabled = self.ui["labs_workflows"] == true;
        let mut col = div()
            .id("workflows-page")
            .w(px(760.))
            .max_h(px(640.))
            .flex()
            .flex_col()
            .gap_2()
            .p_4()
            .overflow_y_scroll()
            .self_start()
            .rounded_xl()
            .border_1()
            .border_color(pal.line)
            .bg(pal.card)
            .shadow_lg()
            .child(div().flex().items_center().gap_2().text_xl().font_weight(FontWeight::MEDIUM).child("ワークフロー"))
            .child(div().text_xs().text_color(pal.muted).child(format!(
                "{} の実行はこのマシンで進みます（GreenCode の起動中のみ）",
                workflow::paths().workflows.display()
            )));
        if !enabled {
            col = col.child(
                div()
                    .rounded_lg()
                    .border_1()
                    .border_color(pal.warn)
                    .p_2()
                    .text_xs()
                    .child("定期実行、Webhook、再試行を使うには、設定でワークフローを有効にしてください。"),
            );
            col = col.child(
                control_button("workflow-open-settings", "自動化の設定を開く", pal)
                    .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                        this.workflows = None;
                        this.run(Act::SettingsTab(WORKFLOW_SETTINGS), window, cx)
                    })),
            );
        }
        let mut tabs = div().flex().items_center().gap_1();
        for (i, label) in ["あなたの対応", "実行中", "すべて"].into_iter().enumerate() {
            tabs = tabs.child(
                chip(("workflow-tab", i), format!("{label}  {}", view.count(i)), pal)
                    .when(view.tab == i, |c| c.bg(pal.selected).text_color(pal.fg))
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                        if let Some(w) = &mut this.workflows {
                            w.tab = i;
                        }
                        cx.notify();
                    })),
            );
        }
        col = col.child(tabs);
        if !view.defs.errors.is_empty() {
            col = col.child(
                div()
                    .rounded_lg()
                    .border_1()
                    .border_color(pal.removed)
                    .p_2()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .children(view.defs.errors.iter().map(|(path, error)| {
                        div().text_xs().text_color(pal.removed).child(format!("{}: {error}", path.display()))
                    })),
            );
        }
        let visible = view.visible();
        if visible.is_empty() {
            col = col.child(div().py_4().text_xs().text_color(pal.muted).child(match view.tab {
                0 => "対応が必要な実行はありません。",
                1 => "実行中のワークフローはありません。",
                _ => "実行はまだありません。greencode-cli workflow start で開始できます。",
            }));
        }
        for run in visible {
            let mut card = div()
                .rounded_lg()
                .border_1()
                .border_color(pal.line)
                .p_2()
                .flex()
                .flex_col()
                .gap_1()
                .child(
                    div()
                        .flex()
                        .items_baseline()
                        .gap_2()
                        .child(div().font_weight(FontWeight::MEDIUM).child(format!("{} v{}", run.workflow, run.version)))
                        .child(div().text_xs().text_color(status_color(run.status, pal)).child(run.status.label()))
                        .child(div().flex_1())
                        .child(div().text_xs().text_color(pal.muted).child(run.id.clone())),
                );
            if let Some(question) = &run.question {
                card = card.child(div().text_sm().child(question.title.clone()));
                let mut answers = div().flex().flex_wrap().gap_1();
                for (i, option) in question.options.iter().enumerate() {
                    let (id, step, choice) = (run.id.clone(), question.step.clone(), option.clone());
                    answers = answers.child(
                        chip(format!("workflow-answer-{}-{i}", run.id), option.clone(), pal)
                            .bg(pal.accent)
                            .text_color(pal.card)
                            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                                this.workflow_answer(&id, &step, &choice, cx)
                            })),
                    );
                }
                card = card.child(answers);
            }
            if let Some(error) = &run.error {
                card = card.child(
                    div()
                        .text_xs()
                        .text_color(pal.removed)
                        .child(error.lines().next().unwrap_or("").to_string()),
                );
            }
            let done = run.steps.iter().filter(|s| s.status == StepState::Done).count();
            if !run.steps.is_empty() {
                card = card.child(div().text_xs().text_color(pal.muted).child(format!(
                    "ステップ {done}/{} 完了{}",
                    run.steps.len(),
                    run.steps
                        .last()
                        .map(|s| format!(" · {} {}", s.id, step_label(s.status)))
                        .unwrap_or_default()
                )));
            }
            let mut actions = div().flex().items_center().gap_1();
            if run.status == RunStatus::Paused {
                let id = run.id.clone();
                actions = actions.child(chip(format!("workflow-resume-{}", run.id), "再開", pal).on_click(
                    cx.listener(move |this, _: &ClickEvent, _, cx| this.workflow_resume(&id, cx)),
                ));
            }
            if !run.finished() {
                let id = run.id.clone();
                actions = actions.child(
                    chip(format!("workflow-cancel-{}", run.id), "キャンセル", pal)
                        .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| this.workflow_cancel(&id, cx))),
                );
            }
            card = card.child(actions);
            col = col.child(card);
        }
        let triggers: Vec<_> = view.defs.workflows.iter().filter_map(|def| {
            trigger_label(def).map(|label| format!("{} — {label}", def.name))
        }).collect();
        if !triggers.is_empty() {
            col = col.child(
                div()
                    .pt_1()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(div().text_xs().text_color(pal.muted).child("トリガー"))
                    .children(triggers.into_iter().map(|line| {
                        div().text_xs().text_color(pal.muted).child(line)
                    })),
            );
        }
        col = col.child(
            div()
                .flex()
                .items_center()
                .gap_1()
                .child(div().flex_1())
                .child(chip("workflow-refresh", "更新", pal).on_click(cx.listener(
                    |this, _: &ClickEvent, _, cx| {
                        if let Some(w) = &mut this.workflows {
                            w.reload();
                        }
                        cx.notify();
                    },
                )))
                .child(chip("workflow-close", "閉じる  esc", pal).on_click(
                    cx.listener(|this, _: &ClickEvent, window, cx| this.toggle_workflows(window, cx)),
                )),
        );
        Some(
            div()
                .absolute()
                .inset_0()
                .flex()
                .justify_center()
                .pt(px(80.))
                .child(col),
        )
    }
}

fn status_color(status: RunStatus, pal: Pal) -> Hsla {
    match status {
        RunStatus::Waiting => pal.warn,
        RunStatus::Paused => pal.warn,
        RunStatus::Done => pal.added,
        RunStatus::Failed | RunStatus::Cancelled => pal.removed,
        RunStatus::Running => pal.accent,
    }
}
