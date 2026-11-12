//! What a thread shows: items built from core events or a saved session.

use gpui_kit::component::text::TextView;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use greencode_core::{permission, Block, Message, Role};
use serde_json::Value;

pub enum Item {
    User(SharedString),
    Assistant(String),
    Tool { id: String, name: String, input: Value, output: Option<String>, is_error: bool, open: bool },
    Notice(SharedString),
    Error(SharedString),
}

/// Colors derived from the system appearance. Panels are translucent so the
/// blurred desktop shows through, as in native macOS apps.
#[derive(Clone, Copy)]
pub struct Pal {
    pub sidebar: Hsla,
    pub panel: Hsla,
    pub fg: Hsla,
    pub muted: Hsla,
    pub line: Hsla,
    pub hover: Hsla,
    pub selected: Hsla,
    pub accent: Hsla,
    pub added: Hsla,
    pub removed: Hsla,
    pub warn: Hsla,
}

impl Pal {
    pub fn new(dark: bool) -> Pal {
        let fg = if dark { hsla(0., 0., 1., 0.92) } else { hsla(0., 0., 0., 0.88) };
        Pal {
            sidebar: if dark { hsla(0., 0., 0.08, 0.35) } else { hsla(0., 0., 0.96, 0.45) },
            panel: if dark { hsla(0., 0., 0.12, 0.86) } else { hsla(0., 0., 1., 0.86) },
            fg,
            muted: fg.opacity(0.5),
            line: fg.opacity(0.1),
            hover: fg.opacity(0.05),
            selected: fg.opacity(0.1),
            accent: hsla(211. / 360., 1., if dark { 0.56 } else { 0.5 }, 1.),
            added: hsla(140. / 360., 0.6, if dark { 0.6 } else { 0.32 }, 1.),
            removed: hsla(0., 0.7, if dark { 0.66 } else { 0.45 }, 1.),
            warn: hsla(35. / 360., 0.95, 0.5, 1.),
        }
    }
}

pub fn items_from(messages: &[Message]) -> Vec<Item> {
    let mut items: Vec<Item> = Vec::new();
    for m in messages {
        for b in &m.content {
            match b {
                Block::Text { text } if m.role == Role::User => items.push(Item::User(text.clone().into())),
                Block::Text { text } => items.push(Item::Assistant(text.clone())),
                Block::ToolUse { id, name, input } => items.push(Item::Tool {
                    id: id.clone(),
                    name: name.clone(),
                    input: input.clone(),
                    output: None,
                    is_error: false,
                    open: false,
                }),
                Block::ToolResult { tool_use_id, content, is_error } => {
                    finish_tool(&mut items, tool_use_id, content.clone(), *is_error);
                }
                Block::Other(_) => {}
            };
        }
    }
    items
}

/// Records a tool's result; returns the item's index.
pub fn finish_tool(items: &mut [Item], tool_id: &str, out: String, err: bool) -> Option<usize> {
    let ix = items.iter().rposition(|i| matches!(i, Item::Tool { id, .. } if id == tool_id))?;
    if let Item::Tool { output, is_error, .. } = &mut items[ix] {
        *output = Some(out);
        *is_error = err;
    }
    Some(ix)
}

fn clip(s: &str, max: usize) -> String {
    let line = s.lines().next().unwrap_or("");
    if line.chars().count() > max || s.lines().count() > 1 {
        format!("{}…", line.chars().take(max).collect::<String>())
    } else {
        line.to_string()
    }
}

/// The body of an expanded tool row: an inline diff for edits, otherwise the output.
fn tool_detail(name: &str, input: &Value, output: Option<&str>, pal: Pal, mono: SharedString) -> Div {
    let mut body = div().flex().flex_col().font_family(mono).text_xs().pl_5().pt_1();
    let line = |text: &str, color: Hsla| div().text_color(color).child(SharedString::from(text.to_string()));
    if name == "edit" {
        for l in input["old_string"].as_str().unwrap_or("").lines().take(40) {
            body = body.child(line(&format!("- {l}"), pal.removed));
        }
        for l in input["new_string"].as_str().unwrap_or("").lines().take(40) {
            body = body.child(line(&format!("+ {l}"), pal.added));
        }
    } else if name == "write" {
        for l in input["content"].as_str().unwrap_or("").lines().take(40) {
            body = body.child(line(&format!("+ {l}"), pal.added));
        }
    }
    for l in output.unwrap_or("").lines().take(60) {
        body = body.child(line(l, pal.muted));
    }
    body
}

/// Renders one item. `toggle` is attached to tool rows to expand them.
pub fn render_item(
    ix: usize,
    item: &Item,
    pal: Pal,
    mono: SharedString,
    toggle: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> AnyElement {
    let row = div().px_6().py_1p5().w_full();
    match item {
        Item::User(text) => row
            .flex()
            .justify_end()
            .child(div().max_w(relative(0.8)).px_3().py_1p5().rounded_xl().bg(pal.selected).child(text.clone()))
            .into_any_element(),
        Item::Assistant(text) => row.child(TextView::markdown(("md", ix), text.clone()).selectable(true)).into_any_element(),
        Item::Tool { name, input, output, is_error, open, .. } => {
            let (mark, color) = match (output.is_some(), *is_error) {
                (false, _) => ("○", pal.muted),
                (true, false) => ("✓", pal.added),
                (true, true) => ("✕", pal.removed),
            };
            let summary = clip(&permission::summary(name, input), 90);
            row.py_0p5()
                .child(
                    div()
                        .id(("tool", ix))
                        .flex()
                        .gap_2()
                        .items_center()
                        .px_2()
                        .py_0p5()
                        .rounded_md()
                        .text_sm()
                        .cursor_pointer()
                        .hover(|s| s.bg(pal.hover))
                        .on_click(toggle)
                        .child(div().text_color(color).child(mark))
                        .child(div().font_family(mono.clone()).child(SharedString::from(name.clone())))
                        .child(div().font_family(mono.clone()).text_color(pal.muted).truncate().child(SharedString::from(summary))),
                )
                .when(*open, |d| d.child(tool_detail(name, input, output.as_deref(), pal, mono)))
                .into_any_element()
        }
        Item::Notice(text) => row.text_sm().text_color(pal.muted).child(text.clone()).into_any_element(),
        Item::Error(text) => row.text_sm().text_color(pal.removed).child(text.clone()).into_any_element(),
    }
}
