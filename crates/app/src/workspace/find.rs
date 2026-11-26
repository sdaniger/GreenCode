//! ⌘F: find in the active thread, or in the diff panel when it has focus.
use super::*;

#[derive(Clone, Copy, PartialEq)]
pub(super) enum Target {
    Thread,
    Diff,
}

struct Bar {
    field: Entity<InputState>,
    target: Target,
    _sub: Subscription,
}

#[derive(Default)]
pub(super) struct Find {
    bar: Option<Bar>,
    query: String,
    /// Ascending line (diff) or item (thread) indices that contain the query.
    hits: Vec<usize>,
    cur: usize,
    pub diff_scroll: UniformListScrollHandle,
}

/// Indices of the lines that contain `query`, ignoring case.
fn matches<'a>(lines: impl Iterator<Item = (usize, &'a str)>, query: &str) -> Vec<usize> {
    if query.is_empty() {
        return vec![];
    }
    let query = query.to_lowercase();
    lines
        .filter(|(_, line)| line.to_lowercase().contains(&query))
        .map(|(i, _)| i)
        .collect()
}

pub(super) fn item_text(item: &Item) -> Option<&str> {
    match item {
        Item::User(text) => Some(text.as_ref()),
        Item::Assistant(text) => Some(text),
        _ => None,
    }
}

impl Find {
    pub(super) fn is_open(&self) -> bool {
        self.bar.is_some()
    }

    pub(super) fn close(&mut self) {
        self.bar = None;
        self.query.clear();
        self.hits.clear();
        self.cur = 0;
    }

    fn step(&mut self, forward: bool) {
        let n = self.hits.len();
        if n > 0 {
            self.cur = if forward { (self.cur + 1) % n } else { (self.cur + n - 1) % n };
        }
    }

}

impl Workspace {
    pub(super) fn open_find(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let target = if self.show_diff && self.diff_focus.contains_focused(window, cx) {
            Target::Diff
        } else {
            Target::Thread
        };
        if let Some(bar) = self.find.bar.as_ref().filter(|b| b.target == target) {
            bar.field.update(cx, |s, cx| {
                s.focus(window, cx);
                s.select_all(window, cx);
            });
            return;
        }
        let field = cx.new(|cx| {
            InputState::new(window, cx).placeholder(match target {
                Target::Thread => "スレッド内を検索",
                Target::Diff => "差分内を検索",
            })
        });
        let _sub = cx.subscribe_in(&field, window, |this, _, ev: &InputEvent, _, cx| match ev {
            InputEvent::Change => {
                this.refind(true, cx);
                this.find_reveal(cx);
            }
            InputEvent::PressEnter { shift, .. } => this.find_step(!*shift, cx),
            _ => {}
        });
        field.update(cx, |s, cx| s.focus(window, cx));
        self.find.query.clear();
        self.find.hits.clear();
        self.find.cur = 0;
        self.find.bar = Some(Bar { field, target, _sub });
        cx.notify();
    }

    /// Recomputes the matches against the current content; `reset` starts again from the first one.
    fn refind(&mut self, reset: bool, cx: &App) {
        let Some((target, query)) = self
            .find
            .bar
            .as_ref()
            .map(|b| (b.target, b.field.read(cx).value().to_string()))
        else {
            return;
        };
        let hits = match target {
            Target::Thread => matches(
                self.thread().items.iter().enumerate().filter_map(|(i, it)| item_text(it).map(|t| (i, t))),
                &query,
            ),
            Target::Diff => matches(self.diff.iter().enumerate().map(|(i, l)| (i, l.as_ref())), &query),
        };
        let f = &mut self.find;
        // Selecting all and pressing a key can re-emit Change with the same text; keep the match.
        let reset = reset && f.query != query;
        f.query = query;
        f.cur = if reset { 0 } else { f.cur.min(hits.len().saturating_sub(1)) };
        f.hits = hits;
    }

    fn find_step(&mut self, forward: bool, cx: &mut Context<Self>) {
        self.refind(false, cx);
        self.find.step(forward);
        self.find_reveal(cx);
    }

    fn find_reveal(&mut self, cx: &mut Context<Self>) {
        let f = &self.find;
        if let (Some(&ix), Some(bar)) = (f.hits.get(f.cur), &f.bar) {
            match bar.target {
                Target::Thread => self.list.scroll_to_reveal_item(ix),
                Target::Diff => f.diff_scroll.scroll_to_item(ix, ScrollStrategy::Center),
            }
        }
        cx.notify();
    }

    pub(super) fn close_find(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let target = self.find.bar.as_ref().map(|b| b.target);
        self.find.close();
        match target {
            Some(Target::Diff) => window.focus(&self.diff_focus, cx),
            _ => self.input.update(cx, |s, cx| s.focus(window, cx)),
        }
        cx.notify();
    }

    pub(super) fn find_bar(&self, target: Target, pal: Pal, cx: &mut Context<Self>) -> Option<Div> {
        let f = &self.find;
        let bar = f.bar.as_ref().filter(|b| b.target == target)?;
        let count = if bar.field.read(cx).value().is_empty() {
            String::new()
        } else if f.hits.is_empty() {
            "0 件".into()
        } else {
            format!("{} / {}", f.cur + 1, f.hits.len())
        };
        Some(
            div()
                .flex()
                .items_center()
                .gap_2()
                .px_3()
                .py_1()
                .border_b_1()
                .border_color(pal.line)
                .child(div().flex_1().child(Input::new(&bar.field)))
                .child(div().text_xs().text_color(pal.muted).child(SharedString::from(count)))
                .child(chip("find-prev", "↑", pal).on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.find_step(false, cx))))
                .child(chip("find-next", "↓", pal).on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.find_step(true, cx))))
                .child(chip("find-close", "✕", pal).on_click(cx.listener(|this, _: &ClickEvent, window, cx| this.close_find(window, cx)))),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::{matches, Find, Item};

    #[test]
    fn matching_ignores_case_and_wraps() {
        let lines = ["Alpha", "beta", "ALPHA beta", "gamma"];
        let hits = matches(lines.iter().copied().enumerate(), "alpha");
        assert_eq!(hits, [0, 2]);
        assert!(matches(lines.iter().copied().enumerate(), "").is_empty());
        let mut f = Find { hits, ..Default::default() };
        f.step(true);
        assert_eq!(f.cur, 1);
        f.step(true);
        assert_eq!(f.cur, 0);
        f.step(false);
        assert_eq!(f.cur, 1);
    }

    #[test]
    fn message_limit_counts_characters() {
        assert!(!super::super::over_limit("あ".repeat(120_000).as_str()));
        assert!(super::super::over_limit("あ".repeat(120_001).as_str()));
    }

    #[gpui_kit::test]
    async fn cmd_f_steps_through_loaded_items_and_escape_closes(
        cx: &mut gpui_kit::TestAppContext,
    ) {
        let (ws, cx) = super::super::tests::open(cx);
        ws.update(cx, |ws, _| {
            ws.threads[0].items = vec![
                Item::User("alpha".into()),
                Item::Assistant("beta alpha".into()),
            ];
            ws.list.reset(2);
        });
        cx.simulate_keystrokes("cmd-f");
        cx.simulate_input("alpha");
        ws.read_with(cx, |ws, _| assert_eq!(ws.find.hits, [0, 1]));
        cx.simulate_keystrokes("enter");
        ws.read_with(cx, |ws, _| assert_eq!(ws.find.cur, 1));
        cx.simulate_keystrokes("enter");
        ws.read_with(cx, |ws, _| assert_eq!(ws.find.cur, 0));
        cx.simulate_keystrokes("shift-enter");
        ws.read_with(cx, |ws, _| assert_eq!(ws.find.cur, 1));
        cx.simulate_keystrokes("escape");
        ws.read_with(cx, |ws, _| assert!(!ws.find.is_open()));
    }

    #[gpui_kit::test]
    async fn send_shortcut_enter_sends_or_inserts_a_newline(cx: &mut gpui_kit::TestAppContext) {
        let (ws, cx) = super::super::tests::open(cx);
        // Default: Enter sends.
        cx.simulate_input("one");
        cx.simulate_keystrokes("enter");
        ws.read_with(cx, |ws, cx| assert!(ws.input.read(cx).value().is_empty()));
        ws.read_with(cx, |ws, _| {
            assert!(matches!(ws.thread().items.last(), Some(Item::User(text)) if text.as_ref() == "one"))
        });

        // ⌘Enter mode: Enter is a newline; ⌘Enter sends.
        ws.update(cx, |ws, _| {
            ws.threads[0].status = super::super::Status::Idle;
            ws.threads[0].queue.clear();
        });
        ws.update_in(cx, |ws, window, cx| {
            ws.run(super::super::Act::SendShortcut, window, cx)
        });
        cx.simulate_input("two");
        cx.simulate_keystrokes("enter");
        cx.run_until_parked();
        ws.read_with(cx, |ws, cx| assert_eq!(ws.input.read(cx).value().to_string(), "two\n"));
        cx.simulate_keystrokes("cmd-enter");
        ws.read_with(cx, |ws, cx| assert!(ws.input.read(cx).value().is_empty()));
        ws.read_with(cx, |ws, _| {
            assert!(matches!(ws.thread().items.last(), Some(Item::User(text)) if text.as_ref() == "two"))
        });
    }

    #[gpui_kit::test]
    async fn over_limit_message_is_refused_and_the_draft_stays(cx: &mut gpui_kit::TestAppContext) {
        let (ws, cx) = super::super::tests::open(cx);
        let long = "あ".repeat(super::super::MAX_MESSAGE_CHARS + 1);
        ws.update_in(cx, |ws, window, cx| {
            ws.input
                .update(cx, |s, cx| super::super::set_input(s, long.clone(), window, cx));
            ws.send(window, cx);
        });
        ws.read_with(cx, |ws, cx| {
            assert_eq!(
                ws.input.read(cx).value().chars().count(),
                super::super::MAX_MESSAGE_CHARS + 1
            );
            assert!(ws
                .thread()
                .items
                .iter()
                .any(|item| matches!(item, Item::Notice(text) if text.contains("120,000"))));
            assert!(!ws.thread().items.iter().any(|item| matches!(item, Item::User(_))));
        });
    }
}
