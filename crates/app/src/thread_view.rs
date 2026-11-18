//! What a thread shows: items built from core events or a saved session.

use std::sync::atomic::{AtomicBool, Ordering};
use gpui_kit::component::text::{TextView, TextViewStyle};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use greencode_core::{context, permission, Block, Message, Role};
use serde_json::Value;

static WRAP_CODE: AtomicBool = AtomicBool::new(true);

pub enum Item {
    User(SharedString),
    Assistant(String),
    Tool { id: String, name: String, input: Value, output: Option<String>, is_error: bool, open: bool },
    Notice(SharedString),
    Error(SharedString),
    Thinking(SharedString),
    Image(std::sync::Arc<Image>),
}

/// For every item, `Some(start)` when it belongs to a collapsible work group starting at `start`.
/// A group is a run of tools/thinking (plus interim assistant text between them) that a later
/// assistant reply of the same turn follows; the newest group stays open while the thread runs.
pub fn work_groups(items: &[Item], running: bool) -> Vec<Option<usize>> {
    let mut out = vec![None; items.len()];
    let mut groups: Vec<(usize, usize)> = vec![];
    let mut turn = 0;
    while turn < items.len() {
        let mut end = turn + 1;
        while end < items.len() && !matches!(items[end], Item::User(_)) { end += 1; }
        let Some(last) = (turn..end).rev().find(|&i| matches!(items[i], Item::Assistant(_))) else { turn = end; continue };
        let mut i = turn;
        while i < last {
            if !matches!(items[i], Item::Tool { .. } | Item::Thinking(_)) { i += 1; continue }
            let start = i;
            let mut stop = i;
            while i < last && matches!(items[i], Item::Tool { .. } | Item::Thinking(_) | Item::Assistant(_)) {
                if !matches!(items[i], Item::Assistant(_)) { stop = i; }
                i += 1;
            }
            if stop > start { groups.push((start, stop)); }
        }
        turn = end;
    }
    if running { groups.pop(); }
    for (start, stop) in groups {
        for slot in &mut out[start..=stop] { *slot = Some(start); }
    }
    out
}

fn text<'a>(input: &'a Value, keys: &[&str]) -> &'a str {
    keys.iter().find_map(|k| input[*k].as_str()).unwrap_or("")
}

/// `(added, removed)` lines of a replacement; lines the old and new text share at both ends are not counted.
fn changed_lines(old: &str, new: &str) -> (usize, usize) {
    let (old, new): (Vec<_>, Vec<_>) = (old.lines().collect(), new.lines().collect());
    let head = old.iter().zip(&new).take_while(|(a, b)| a == b).count();
    let tail = old[head..].iter().rev().zip(new[head..].iter().rev()).take_while(|(a, b)| a == b).count();
    (new.len() - head - tail, old.len() - head - tail)
}

/// Files changed by the turn ending at `end`: `(path, added, removed)` line counts, merged by path.
pub fn turn_changes(items: &[Item], end: usize) -> Vec<(String, usize, usize)> {
    let mut changes: Vec<(String, usize, usize)> = vec![];
    let end = end.min(items.len().saturating_sub(1));
    let begin = items.get(..=end).map_or(0, |s| s.iter().rposition(|i| matches!(i, Item::User(_))).map_or(0, |p| p + 1));
    for item in items.get(begin..=end).unwrap_or(&[]) {
        let Item::Tool { name, input, output: Some(_), is_error: false, .. } = item else { continue };
        let args = if name == "ext" { &input["args"] } else { input };
        let (added, removed) = match name.as_str() {
            // External agents spell the arguments differently (`newString`, `new_str`, ...).
            "edit" => changed_lines(text(input, &["old_string", "oldString", "old_str", "oldText"]), text(input, &["new_string", "newString", "new_str", "newText"])),
            "write" => (text(input, &["content", "file_text", "text"]).lines().count(), 0),
            "ext" if input["tool"] == "hedit" => {
                let ops = args["ops"].as_array().map_or(&[][..], |o| o.as_slice());
                let span = |o: &Value| { let start = o["start"].as_u64().unwrap_or(0); o["end"].as_u64().unwrap_or(start).saturating_sub(start) as usize + 1 };
                (ops.iter().map(|o| o["text"].as_str().map_or(0, |t| t.lines().count())).sum(),
                 ops.iter().filter(|o| o["op"] != "insert").map(span).sum())
            }
            _ => continue,
        };
        let Some(path) = args["path"].as_str().or_else(|| args["file_path"].as_str()) else { continue };
        match changes.iter_mut().find(|c| c.0 == path) {
            Some(c) => { c.1 += added; c.2 += removed; }
            None => changes.push((path.to_string(), added, removed)),
        }
    }
    changes
}

/// A displayable image from an image block; formats the UI cannot decode are skipped.
pub fn image_item(block: &Value) -> Option<Item> {
    let (media, bytes) = greencode_core::context::image_bytes(block)?;
    let format = match media {
        "image/png" => ImageFormat::Png,
        "image/jpeg" => ImageFormat::Jpeg,
        "image/gif" => ImageFormat::Gif,
        "image/webp" => ImageFormat::Webp,
        _ => return None,
    };
    Some(Item::Image(std::sync::Arc::new(Image::from_bytes(format, bytes))))
}

/// Colors derived from the system appearance; solid, as in T3 Code.
#[derive(Clone, Copy)]
pub struct Pal {
    pub sidebar: Hsla,
    pub panel: Hsla,
    /// Composer, bubbles and popovers.
    pub card: Hsla,
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
            sidebar: if dark { hsla(0., 0., 0.035, 1.) } else { hsla(0., 0., 0.975, 1.) },
            panel: if dark { hsla(0., 0., 0.035, 1.) } else { hsla(0., 0., 1., 1.) },
            card: if dark { hsla(0., 0., 0.09, 1.) } else { hsla(0., 0., 0.96, 1.) },
            fg,
            muted: fg.opacity(0.5),
            line: fg.opacity(0.08),
            hover: fg.opacity(0.06),
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
        if m.content.iter().any(|b| matches!(b, Block::Text { text } if context::is_update(text))) { continue; }
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
                Block::Other(v) if v["type"] == "thinking" => {
                    if let Some(t) = v["thinking"].as_str().filter(|t| !t.trim().is_empty()) { items.push(Item::Thinking(t.to_string().into())); }
                }
                Block::Other(v) => items.extend(image_item(v)),
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
    let row = || div().w_full().max_w(px(760.)).px_6().py_1p5();
    let wrap = |d: Div| div().w_full().flex().justify_center().child(d).into_any_element();
    match item {
        Item::User(text) => wrap(row()
            .flex()
            .justify_end()
            .child(div().max_w(relative(0.8)).px_4().py_2().rounded_2xl().bg(pal.card).child(text.clone()))),
        Item::Assistant(text) => {
            let mut code_style = StyleRefinement::default();
            if !WRAP_CODE.load(Ordering::Relaxed) {
                code_style.overflow.x = Some(Overflow::Scroll);
                code_style.text.white_space = Some(WhiteSpace::Nowrap);
            }
            let label = if WRAP_CODE.load(Ordering::Relaxed) { "折返し" } else { "横スクロール" };
            let view = TextView::markdown(("md", ix), text.clone()).selectable(true).style(TextViewStyle::default().code_block(code_style)).code_block_actions(move |code, _, _cx| {
                let source = code.code().to_string();
                let label = label.to_string();
                div().flex().gap_1()
                    .child(div().id("copy-code").px_2().py_0p5().rounded_md().text_xs().text_color(pal.muted).cursor_pointer().hover(|s| s.bg(pal.hover).text_color(pal.fg))
                        .child("コピー").on_click(move |_, _, cx| cx.write_to_clipboard(ClipboardItem::new_string(source.clone()))))
                    .child(div().id("toggle-code-wrap").px_2().py_0p5().rounded_md().text_xs().text_color(pal.muted).cursor_pointer().hover(|s| s.bg(pal.hover))
                        .child(label).on_click(move |_, _, cx| { WRAP_CODE.fetch_xor(true, Ordering::Relaxed); cx.refresh_windows(); }))
            });
            wrap(row().child(view))
        }
        Item::Tool { name, input, output, is_error, open, .. } => {
            let (mark, color) = match (output.is_some(), *is_error) {
                (false, _) => ("○", pal.muted),
                (true, false) => ("✓", pal.added),
                (true, true) => ("✕", pal.removed),
            };
            let summary = clip(&permission::summary(name, input), 90);
            wrap(row().py_0p5()
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
                .when(*open, |d| d.child(tool_detail(name, input, output.as_deref(), pal, mono))))
        }
        Item::Notice(text) => wrap(row().text_sm().text_color(pal.muted).child(text.clone())),
        Item::Error(text) => wrap(row().text_sm().text_color(pal.removed).child(text.clone())),
        Item::Thinking(text) => wrap(row().py_0p5().text_xs().italic().text_color(pal.muted).child(text.clone())),
        // A fixed height keeps the virtualized list stable while the image decodes.
        Item::Image(image) => wrap(row().child(img(image.clone()).h(px(260.)).max_w(relative(0.9)).object_fit(ObjectFit::Contain).rounded_md())),
    }
}
