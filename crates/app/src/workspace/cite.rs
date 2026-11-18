//! "Cite in composer": select text in an assistant reply, add an optional comment, and it lands in the draft as a quote.

use super::*;
use gpui_kit::base::TextSelection;

pub(super) struct CiteForm {
    quote: SharedString,
    field: Entity<InputState>,
    _sub: Subscription,
}

/// `> ` before each quoted line, then `↑ comment` when there is one.
fn quote_block(text: &str, comment: &str) -> String {
    let mut out = text
        .lines()
        .map(|l| format!("> {l}"))
        .collect::<Vec<_>>()
        .join("\n");
    let comment = comment.trim();
    if !comment.is_empty() {
        out.push_str("\n↑ ");
        out.push_str(comment);
    }
    out
}

/// At most six lines, with an ellipsis when cut.
fn preview(text: &str) -> String {
    let mut lines = text.lines().take(7).collect::<Vec<_>>();
    if lines.len() > 6 {
        lines.truncate(6);
        lines.push("…");
    }
    lines.join("\n")
}

impl Workspace {
    pub(super) fn open_cite(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = TextSelection::selected_text(window, cx);
        if text.trim().is_empty() {
            self.notice("引用するテキストを回答から選択してください");
            return;
        }
        let field = cx.new(|cx| InputState::new(window, cx).placeholder("コメント（任意）⏎"));
        let sub = cx.subscribe_in(&field, window, |this, _, ev: &InputEvent, window, cx| {
            if let InputEvent::PressEnter { .. } = ev {
                this.submit_cite(window, cx)
            }
        });
        field.update(cx, |s, cx| s.focus(window, cx));
        self.cite_form = Some(CiteForm {
            quote: preview(&text).into(),
            field,
            _sub: sub,
        });
        // The full selection is read again on submit; keep it until then.
        self.cite_text = text;
        cx.notify();
    }

    pub(super) fn close_cite_form(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.cite_form = None;
        self.cite_text.clear();
        self.input.update(cx, |s, cx| s.focus(window, cx));
        cx.notify();
    }

    pub(super) fn submit_cite(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(form) = &self.cite_form else { return };
        let comment = form.field.read(cx).value().to_string();
        let block = quote_block(&self.cite_text, &comment);
        let mut draft = self.input.read(cx).value().to_string();
        if !draft.is_empty() {
            draft.push_str("\n\n");
        }
        draft.push_str(&block);
        TextSelection::clear(window, cx);
        self.cite_form = None;
        self.cite_text.clear();
        self.input.update(cx, |s, cx| {
            super::set_input(s, draft, window, cx);
            s.focus(window, cx);
        });
        cx.notify();
    }

    pub(super) fn cite_form_view(
        &self,
        pal: Pal,
        cx: &mut Context<Self>,
    ) -> Option<impl IntoElement> {
        let form = self.cite_form.as_ref()?;
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
            .child("引用を追加")
            .child(
                div()
                    .text_xs()
                    .text_color(pal.muted)
                    .child(form.quote.clone()),
            )
            .child(Input::new(&form.field))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_1()
                    .child(div().flex_1())
                    .child(
                        chip("cite-cancel", "キャンセル  esc", pal).on_click(cx.listener(
                            |this, _: &ClickEvent, window, cx| this.close_cite_form(window, cx),
                        )),
                    )
                    .child(chip("cite-add", "追加  ⏎", pal).on_click(cx.listener(
                        |this, _: &ClickEvent, window, cx| this.submit_cite(window, cx),
                    ))),
            );
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

#[cfg(test)]
mod tests {
    use super::super::tests::open;
    use super::quote_block;
    use gpui_kit::TestAppContext;

    #[test]
    fn quote_block_prefixes_lines_and_appends_the_comment() {
        assert_eq!(quote_block("a\nb", "why?"), "> a\n> b\n↑ why?");
        assert_eq!(quote_block("a\nb", ""), "> a\n> b");
    }

    #[gpui_kit::test]
    async fn cite_without_a_selection_only_notifies(cx: &mut TestAppContext) {
        let (ws, cx) = open(cx);
        ws.update_in(cx, |ws, window, cx| {
            ws.run(super::super::Act::Cite, window, cx);
            assert!(ws.cite_form.is_none());
        });
    }
}
