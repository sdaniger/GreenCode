//! Thread history, workspace focus cycling, sidebar undo, and the ⌘Q guard.
use super::*;
use std::time::{Duration, Instant};

#[derive(Default)]
pub(super) struct Nav {
    back: Vec<String>,
    forward: Vec<String>,
    /// Set while `navigate` changes the thread, so `select` does not record the move again.
    jump: bool,
}

impl Nav {
    pub(super) fn visit(&mut self, from: &str) {
        if std::mem::take(&mut self.jump) {
            return;
        }
        self.back.push(from.into());
        self.forward.clear();
    }

    fn back(&mut self, current: &str) -> Option<String> {
        let to = self.back.pop()?;
        self.forward.push(current.into());
        self.jump = true;
        Some(to)
    }

    fn forward(&mut self, current: &str) -> Option<String> {
        let to = self.forward.pop()?;
        self.back.push(current.into());
        self.jump = true;
        Some(to)
    }
}

/// A sidebar action whose previous value `undo_sidebar` can restore.
pub(super) enum SidebarAction {
    Pin { id: String, value: bool },
    Archive { id: String, value: bool },
    Snooze { id: String, value: u64 },
}

#[derive(Default)]
pub(super) struct Undo {
    action: Option<(SidebarAction, Instant)>,
}

impl Undo {
    pub(super) fn record(&mut self, action: SidebarAction) {
        self.action = Some((action, Instant::now()));
    }

    fn take(&mut self) -> Option<SidebarAction> {
        let (action, at) = self.action.take()?;
        (at.elapsed() <= Duration::from_secs(5)).then_some(action)
    }
}

#[derive(Default)]
pub(super) struct QuitGuard {
    last: Option<Instant>,
    held_since: Option<Instant>,
}

#[derive(Clone, Copy, PartialEq)]
enum Area {
    Sidebar,
    Conversation,
    Composer,
    Panel,
}

pub(super) fn quit_mode_label(value: Option<&str>) -> &'static str {
    match value {
        Some("direct") => "直接",
        Some("hold") => "長押し",
        _ => "2回押し",
    }
}

impl Workspace {
    pub(super) fn navigate(&mut self, back: bool, window: &mut Window, cx: &mut Context<Self>) {
        let current = self.thread().meta.id.clone();
        let target = if back {
            self.nav.back(&current)
        } else {
            self.nav.forward(&current)
        };
        let Some(id) = target else {
            return self.notice(if back {
                "戻る履歴はありません"
            } else {
                "進む履歴はありません"
            });
        };
        match self.threads.iter().position(|t| t.meta.id == id) {
            Some(ix) if ix != self.active => self.select(ix, window, cx),
            _ => {
                self.nav.jump = false;
                self.notice("移動先のスレッドが見つかりません");
            }
        }
    }

    fn focus_areas(&self) -> Vec<Area> {
        let mut areas = vec![Area::Sidebar, Area::Conversation, Area::Composer];
        if (self.terminal_visible && self.active_terminal().is_some()) || self.show_diff {
            areas.push(Area::Panel);
        }
        areas
    }

    fn area_focused(&self, area: Area, window: &Window, cx: &App) -> bool {
        match area {
            Area::Sidebar => self.sidebar_focus.is_focused(window),
            Area::Conversation => self.conversation_focus.is_focused(window),
            Area::Composer => self.input.read(cx).focus_handle(cx).is_focused(window),
            Area::Panel => match self.active_terminal().filter(|_| self.terminal_visible) {
                Some(term) => term.input.read(cx).focus_handle(cx).is_focused(window),
                None => self.diff_focus.is_focused(window),
            },
        }
    }

    fn focus_area(&mut self, area: Area, window: &mut Window, cx: &mut Context<Self>) {
        match area {
            Area::Sidebar => window.focus(&self.sidebar_focus, cx),
            Area::Conversation => window.focus(&self.conversation_focus, cx),
            Area::Composer => self.input.update(cx, |s, cx| s.focus(window, cx)),
            Area::Panel => {
                if self.terminal_visible {
                    if let Some(term) = self.active_terminal() {
                        term.input.update(cx, |s, cx| s.focus(window, cx));
                    }
                } else if self.show_diff {
                    window.focus(&self.diff_focus, cx);
                }
            }
        }
        cx.notify();
    }

    /// F6 / Shift+F6: move to the next (or previous) visible workspace area, wrapping around.
    pub(super) fn cycle_focus(&mut self, forward: bool, window: &mut Window, cx: &mut Context<Self>) {
        let areas = self.focus_areas();
        let next = match areas.iter().position(|a| self.area_focused(*a, window, cx)) {
            Some(i) if forward => (i + 1) % areas.len(),
            Some(i) => (i + areas.len() - 1) % areas.len(),
            None if forward => 0,
            None => areas.len() - 1,
        };
        self.focus_area(areas[next], window, cx);
    }

    /// ⌘Z after pin, archive or snooze; only within five seconds of the action.
    pub(super) fn undo_sidebar(&mut self) -> bool {
        let Some(action) = self.undo.take() else {
            return false;
        };
        let saved = match action {
            SidebarAction::Pin { id, value } => self
                .threads
                .iter_mut()
                .find(|t| t.meta.id == id)
                .map(|t| {
                    t.pinned = value;
                    t.save()
                }),
            SidebarAction::Archive { id, value } => self
                .threads
                .iter_mut()
                .find(|t| t.meta.id == id)
                .map(|t| {
                    t.archived = value;
                    t.save()
                }),
            SidebarAction::Snooze { id, value } => self
                .threads
                .iter_mut()
                .find(|t| t.meta.id == id)
                .map(|t| {
                    t.snoozed = value;
                    t.save()
                }),
        };
        let Some(saved) = saved else {
            return false;
        };
        if let Err(e) = saved {
            self.fail(e);
            return true;
        }
        self.notice("最後のサイドバー操作を元に戻しました");
        true
    }

    /// ⌘Q: `direct`, `hold`, or two presses within 500 ms, per `ui.json` `quit_mode`.
    pub(super) fn request_quit(&mut self, held: bool, cx: &mut Context<Self>) {
        let now = Instant::now();
        match self.ui["quit_mode"].as_str().unwrap_or("double") {
            "direct" => cx.quit(),
            "hold" => {
                const HOLD: Duration = Duration::from_millis(1200);
                if self
                    .quit
                    .held_since
                    .is_some_and(|since| held && now.duration_since(since) >= HOLD)
                {
                    cx.quit();
                } else if !held {
                    self.quit.held_since = Some(now);
                    self.notice("⌘Q を長押しすると終了します");
                }
            }
            _ => {
                // Keyboard repeat from one held press is not a second press.
                if held {
                    return;
                }
                let second = self
                    .quit
                    .last
                    .is_some_and(|at| now.duration_since(at) <= Duration::from_millis(500));
                self.quit.last = Some(now);
                if second {
                    cx.quit();
                } else {
                    self.notice("もう一度 ⌘Q で終了");
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    // No glob import: it would bring in gpui-kit's `test` macro, which shadows `#[test]`.
    use crate::workspace::tests::open;
    use gpui_kit::TestAppContext;
    use std::time::{Duration, Instant};

    #[gpui_kit::test]
    async fn sidebar_action_undoes_within_five_seconds(cx: &mut TestAppContext) {
        let (ws, cx) = open(cx);
        ws.update_in(cx, |ws, window, cx| {
            ws.run(crate::workspace::Act::Pin, window, cx)
        });
        assert!(ws.read_with(cx, |ws, _| ws.thread().pinned));
        ws.update(cx, |ws, _| assert!(ws.undo_sidebar()));
        assert!(!ws.read_with(cx, |ws, _| ws.thread().pinned));

        // The same action is past its window, so the input keeps its own undo.
        ws.update_in(cx, |ws, window, cx| {
            ws.run(crate::workspace::Act::Pin, window, cx)
        });
        ws.update(cx, |ws, _| {
            if let Some((_, at)) = ws.undo.action.as_mut() {
                *at = Instant::now() - Duration::from_secs(6);
            }
        });
        assert!(!ws.update(cx, |ws, _| ws.undo_sidebar()));
        assert!(ws.read_with(cx, |ws, _| ws.thread().pinned));
    }

    #[gpui_kit::test]
    async fn navigation_back_and_forward_restores_threads(cx: &mut TestAppContext) {
        let (ws, cx) = open(cx);
        let first = ws.read_with(cx, |ws, _| ws.thread().meta.id.clone());
        ws.update_in(cx, |ws, window, cx| {
            let project = ws.thread().project();
            ws.new_thread(project, false, window, cx);
        });
        let second = ws.read_with(cx, |ws, _| ws.thread().meta.id.clone());
        assert_ne!(first, second);
        ws.update_in(cx, |ws, window, cx| ws.navigate(true, window, cx));
        assert_eq!(ws.read_with(cx, |ws, _| ws.thread().meta.id.clone()), first);
        ws.update_in(cx, |ws, window, cx| ws.navigate(false, window, cx));
        assert_eq!(ws.read_with(cx, |ws, _| ws.thread().meta.id.clone()), second);
    }

    #[gpui_kit::test]
    async fn f6_cycles_visible_areas(cx: &mut TestAppContext) {
        let (ws, cx) = open(cx);
        // The composer starts focused: F6 goes to the sidebar, then conversation, then wraps.
        ws.update_in(cx, |ws, window, cx| ws.cycle_focus(true, window, cx));
        assert!(cx.update(|window, cx| ws.read_with(cx, |ws, _| ws.sidebar_focus.is_focused(window))));
        ws.update_in(cx, |ws, window, cx| ws.cycle_focus(true, window, cx));
        assert!(cx.update(|window, cx| ws.read_with(cx, |ws, _| ws.conversation_focus.is_focused(window))));
        ws.update_in(cx, |ws, window, cx| ws.cycle_focus(false, window, cx));
        assert!(cx.update(|window, cx| ws.read_with(cx, |ws, _| ws.sidebar_focus.is_focused(window))));
    }
}
