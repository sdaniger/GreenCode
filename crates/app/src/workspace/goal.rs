//! `/goal` pause and resume, the goal line above the composer, and `/clear` (a fresh session in the same thread).

use super::*;

/// The prompt that starts each automatic goal continuation.
pub(super) fn goal_continuation(goal: &str) -> String {
    format!("{GOAL_CONTINUE}{goal}\nWhen it is completely done and verified, end your reply with the line GOAL MET.")
}

impl Workspace {
    /// `/goal pause` stops the automatic continuations (a running turn finishes); `/goal resume` starts them again.
    pub(super) fn goal_pause(&mut self, pause: bool, cx: &mut Context<Self>) {
        let ix = self.active;
        let Some((goal, _)) = self.threads[ix].goal.clone() else {
            return self.notice("ゴールは設定されていません。/goal 完了条件 で設定します");
        };
        if self.threads[ix].goal_paused == pause {
            if pause {
                self.drop_goal_continuations(ix);
            }
            return self.notice(if pause { "ゴールはすでに一時停止中です" } else { "ゴールは一時停止していません" });
        }
        self.threads[ix].goal_paused = pause;
        if pause {
            self.drop_goal_continuations(ix);
            self.notice("ゴールを一時停止しました。/goal resume で再開します");
        } else {
            if self.threads[ix].status == Status::Idle {
                self.threads[ix].queue_held = false;
                self.start_prompt(goal_continuation(&goal), cx);
            }
            self.notice("ゴールを再開しました");
        }
        cx.notify();
    }

    /// The line above the composer while a goal is set.
    pub(super) fn goal_row(&self, pal: Pal, cx: &mut Context<Self>) -> Option<Div> {
        let t = self.thread();
        let (goal, turns) = t.goal.as_ref()?;
        let paused = t.goal_paused;
        let action = |id: &'static str, label: &'static str, command: &'static str| {
            chip(id, label, pal).on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                this.command(command, true, window, cx);
            }))
        };
        Some(
            div()
                .flex()
                .items_center()
                .gap_2()
                .text_xs()
                .text_color(pal.muted)
                .child(
                    div().flex_1().truncate().child(SharedString::from(format!(
                        "ゴール{}: {goal} · 自動継続 {turns}/20",
                        if paused { "（一時停止中）" } else { "" }
                    ))),
                )
                .child(if paused {
                    action("goal-resume", "再開", "/goal resume")
                } else {
                    action("goal-pause", "一時停止", "/goal pause")
                })
                .child(action("goal-clear", "解除", "/goal clear")),
        )
    }

    /// `/clear`: the thread and its messages stay, but the next message starts a session that does not see them.
    pub(super) fn clear_session(&mut self, cx: &mut Context<Self>) {
        let t = &self.threads[self.active];
        if t.status != Status::Idle {
            return self.fail("実行中は /clear できません");
        }
        let id = t.meta.id.clone();
        if Session::open(&id).is_none() {
            return self.notice("まだ会話がないため、すでに新しいセッションです");
        }
        if let Err(e) = Session::reset(&id).and_then(|_| Session::clear_runtime(&id)) {
            return self.fail(format!("セッションを新しくできません: {e:#}"));
        }
        let t = &mut self.threads[self.active];
        t.cmd = None;
        t.models.clear();
        t.config_options.clear();
        t.usage = Default::default();
        t.items.push(Item::Notice(thread_view::NEW_SESSION.into()));
        self.list.reset(self.threads[self.active].items.len());
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use crate::workspace::tests::open;
    use gpui_kit::TestAppContext;

    #[gpui_kit::test]
    async fn goal_pause_stops_continuations_and_resume_restarts_them(cx: &mut TestAppContext) {
        let (ws, cx) = open(cx);
        cx.update(|window, cx| {
            ws.update(cx, |ws, cx| {
                let pal = crate::thread_view::Pal::for_theme(crate::thread_view::ThemeId::Light, false);
                assert!(ws.goal_row(pal, cx).is_none());
                ws.threads[0].goal = Some(("tests pass".into(), 3));
                assert!(ws.goal_row(pal, cx).is_some());
                assert!(ws.command("/goal pause", false, window, cx));
                assert!(ws.thread().goal_paused && ws.thread().goal.is_some());
                // A paused goal is not a goal named "pause".
                assert_eq!(ws.thread().goal.as_ref().unwrap().0, "tests pass");
                ws.threads[0].queue.push_back(format!("{}x", super::super::GOAL_CONTINUE));
                assert!(ws.command("/goal pause", false, window, cx));
                assert!(ws.thread().queue.is_empty());
                assert!(ws.command("/goal resume", false, window, cx));
                assert!(!ws.thread().goal_paused);
                assert!(matches!(ws.thread().status, super::super::Status::Running));
                assert!(ws.thread().items.iter().any(|i| matches!(i, super::super::Item::User(t) if t.contains("tests pass"))));
            })
        });
    }

    #[gpui_kit::test]
    async fn clear_keeps_the_thread_but_ends_the_session(cx: &mut TestAppContext) {
        let (ws, cx) = open(cx);
        cx.update(|window, cx| {
            ws.update(cx, |ws, cx| {
                let id = ws.thread().meta.id.clone();
                let session = greencode_core::session::Session::create(&ws.thread().meta).unwrap();
                session.append(&greencode_core::Message::user("old")).unwrap();
                let threads = ws.threads.len();
                assert!(ws.command("/clear", false, window, cx));
                assert_eq!(ws.threads.len(), threads);
                assert!(greencode_core::session::Session::load(&id).unwrap().1.is_empty());
                assert!(matches!(ws.thread().items.last(), Some(super::super::Item::Notice(n)) if n.contains("新しいセッション")));
                let reloaded = crate::thread_view::session_items(&id).unwrap();
                assert!(matches!(reloaded.as_slice(), [super::super::Item::User(_), super::super::Item::Notice(_)]));
            })
        });
    }
}
