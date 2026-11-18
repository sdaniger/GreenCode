//! "Suggest next prompt" (Labs): the empty composer offers a predicted follow-up; tab fills it, esc dismisses it.

use super::*;

impl Workspace {
    /// Asks for a prediction after a native thread's turn. The thread keeps it until the user types, sends or dismisses it.
    pub(super) fn request_suggestion(&mut self, ix: usize, cx: &mut Context<Self>) {
        // No network from UI tests.
        if cfg!(test) || self.ui["suggest_next"] != true {
            return;
        }
        let t = &self.threads[ix];
        if t.meta.backend != "native" {
            return;
        }
        if !t.queue.is_empty() || (ix == self.active && !self.input.read(cx).value().is_empty()) {
            return;
        }
        let (id, model, cfg) = (t.meta.id.clone(), t.meta.model.clone(), self.cfg.clone());
        let task = self.rt.spawn(async move {
            let (_, messages) = greencode_core::session::Session::load(&id).ok()?;
            greencode_core::suggest::suggest_next(&cfg, &model, &messages)
                .await
                .ok()
                .map(|s| (id, s))
        });
        cx.spawn(async move |this, cx| {
            let Ok(Some((id, text))) = task.await else {
                return;
            };
            let _ = this.update(cx, |this, cx| {
                let empty = this.input.read(cx).value().is_empty();
                let active = this.active;
                if let Some((ix, t)) = this
                    .threads
                    .iter_mut()
                    .enumerate()
                    .find(|(_, t)| t.meta.id == id)
                {
                    // Only while the thread is still waiting for the user, and for the visible one only while the composer is empty.
                    if t.status == Status::Idle && t.queue.is_empty() && (ix != active || empty) {
                        t.suggestion = Some(text).filter(|s| !s.is_empty());
                        cx.notify();
                    }
                }
            });
        })
        .detach();
    }

    /// True when tab should fill the composer with the suggestion.
    pub(super) fn suggestion_ready(&self, cx: &App) -> bool {
        self.thread().suggestion.is_some() && self.input.read(cx).value().is_empty()
    }

    pub(super) fn accept_suggestion(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(text) = self.threads[self.active].suggestion.take() {
            self.input
                .update(cx, |s, cx| super::set_input(s, text, window, cx));
        }
        cx.notify();
    }

    pub(super) fn dismiss_suggestion(&mut self, cx: &mut Context<Self>) {
        self.threads[self.active].suggestion = None;
        cx.notify();
    }

    /// The muted row above the composer.
    pub(super) fn suggestion_row(&self, pal: Pal, cx: &App) -> Option<Div> {
        let text = self
            .thread()
            .suggestion
            .as_ref()
            .filter(|_| self.input.read(cx).value().is_empty())?;
        Some(
            div()
                .text_xs()
                .text_color(pal.muted)
                .truncate()
                .child(SharedString::from(format!(
                    "提案: {text}   tab で入力 · esc で閉じる"
                ))),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::open;
    use gpui_kit::{TestAppContext, VisualTestContext};

    #[gpui_kit::test]
    async fn tab_fills_the_suggestion_and_escape_dismisses_it(cx: &mut TestAppContext) {
        let (ws, cx) = open(cx);
        let value = |cx: &mut VisualTestContext| {
            ws.read_with(cx, |ws, cx| ws.input.read(cx).value().to_string())
        };
        ws.update(cx, |ws, cx| {
            ws.threads[ws.active].suggestion = Some("テストを足して".into());
            cx.notify();
        });
        cx.run_until_parked();
        cx.simulate_keystrokes("tab");
        assert_eq!(value(cx), "テストを足して");
        assert!(ws.read_with(cx, |ws, _| ws.thread().suggestion.is_none()));

        ws.update_in(cx, |ws, window, cx| {
            ws.input.update(cx, |s, cx| s.set_value("", window, cx))
        });
        ws.update(cx, |ws, cx| {
            ws.threads[ws.active].suggestion = Some("テストを足して".into());
            cx.notify();
        });
        cx.simulate_keystrokes("escape");
        assert!(ws.read_with(cx, |ws, _| ws.thread().suggestion.is_none()));
        assert_eq!(value(cx), "");

        // Typing replaces it; the composer text stays.
        ws.update(cx, |ws, cx| {
            ws.threads[ws.active].suggestion = Some("x".into());
            cx.notify();
        });
        cx.simulate_input("abc");
        assert!(ws.read_with(cx, |ws, _| ws.thread().suggestion.is_none()));
        assert_eq!(value(cx), "abc");
    }
}
