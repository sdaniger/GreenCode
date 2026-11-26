//! What a thread shows: items built from core events or a saved session.

use std::sync::atomic::{AtomicBool, Ordering};
use gpui_kit::component::{
    text::{TextView, TextViewStyle},
    Theme,
};
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

fn hex(c: u32) -> Hsla {
    rgb(c).into()
}

/// Built-in UI color themes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ThemeId {
    System,
    Emerald,
    EmeraldLight,
    Paper,
    Sand,
    Graphite,
    Light,
    Dark,
}

impl ThemeId {
    pub const ALL: [ThemeId; 8] = [
        Self::System,
        Self::Emerald,
        Self::EmeraldLight,
        Self::Paper,
        Self::Sand,
        Self::Graphite,
        Self::Light,
        Self::Dark,
    ];

    pub fn id(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::Emerald => "emerald",
            Self::EmeraldLight => "emerald-light",
            Self::Paper => "paper",
            Self::Sand => "sand",
            Self::Graphite => "graphite",
            Self::Light => "light",
            Self::Dark => "dark",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::System => "システム",
            Self::Emerald => "エメラルド",
            Self::EmeraldLight => "エメラルド ライト",
            Self::Paper => "ペーパー",
            Self::Sand => "サンド",
            Self::Graphite => "グラファイト",
            Self::Light => "ライト",
            Self::Dark => "ダーク",
        }
    }

    pub fn palette_label(self) -> &'static str {
        match self {
            Self::System => "テーマ: システムに合わせる",
            Self::Emerald => "テーマ: エメラルド（GreenCode ダーク）",
            Self::EmeraldLight => "テーマ: エメラルド ライト（GreenCode ライト）",
            Self::Paper => "テーマ: ペーパー（ウォームライト）",
            Self::Sand => "テーマ: サンド（ナチュラルモダン）",
            Self::Graphite => "テーマ: グラファイト（マットダーク）",
            Self::Light => "テーマ: ライト（ニュートラル）",
            Self::Dark => "テーマ: ダーク（ニュートラル）",
        }
    }

    pub fn from_json(v: &Value) -> Self {
        match v {
            Value::Bool(false) => Self::Light,
            Value::Bool(true) => Self::Dark,
            Value::String(s) => Self::parse(s).unwrap_or(Self::System),
            _ => Self::System,
        }
    }

    pub fn to_json(self) -> Value {
        match self {
            Self::System => Value::Null,
            other => Value::String(other.id().into()),
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "system" | "auto" | "システム" => Some(Self::System),
            "emerald" | "emerald-dark" | "greencode" | "エメラルド" => Some(Self::Emerald),
            "emerald-light" | "エメラルドライト" | "エメラルド ライト" => {
                Some(Self::EmeraldLight)
            }
            "paper" | "ivory" | "ペーパー" => Some(Self::Paper),
            "sand" | "linen" | "stone" | "サンド" => Some(Self::Sand),
            "graphite" | "charcoal" | "グラファイト" => Some(Self::Graphite),
            "light" | "ライト" => Some(Self::Light),
            "dark" | "ダーク" => Some(Self::Dark),
            _ => None,
        }
    }

    pub fn resolve(self, window_dark: bool) -> Self {
        match self {
            Self::System => {
                if window_dark {
                    Self::Dark
                } else {
                    Self::Light
                }
            }
            other => other,
        }
    }

    pub fn is_dark(self, window_dark: bool) -> bool {
        matches!(
            self.resolve(window_dark),
            Self::Dark | Self::Emerald | Self::Graphite
        )
    }
}

/// Resolved UI color palette for the active theme.
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
    pub accent_fg: Hsla,
    pub added: Hsla,
    pub removed: Hsla,
    pub warn: Hsla,
}

impl Pal {
    pub fn for_theme(theme: ThemeId, window_dark: bool) -> Pal {
        match theme.resolve(window_dark) {
            ThemeId::Light => {
                let fg = hsla(0., 0., 0., 0.88);
                Pal {
                    sidebar: hsla(0., 0., 0.975, 1.),
                    panel: hsla(0., 0., 1., 1.),
                    card: hsla(0., 0., 0.96, 1.),
                    fg,
                    muted: fg.opacity(0.60),
                    line: fg.opacity(0.08),
                    hover: fg.opacity(0.06),
                    selected: fg.opacity(0.10),
                    accent: hex(0x1D68D8),
                    accent_fg: gpui_kit::white(),
                    added: hsla(140. / 360., 0.6, 0.32, 1.),
                    removed: hsla(0., 0.7, 0.45, 1.),
                    warn: hsla(35. / 360., 0.95, 0.42, 1.),
                }
            }
            ThemeId::Dark => {
                let fg = hsla(0., 0., 1., 0.92);
                Pal {
                    sidebar: hsla(0., 0., 0.035, 1.),
                    panel: hsla(0., 0., 0.035, 1.),
                    card: hsla(0., 0., 0.09, 1.),
                    fg,
                    muted: fg.opacity(0.52),
                    line: fg.opacity(0.08),
                    hover: fg.opacity(0.06),
                    selected: fg.opacity(0.10),
                    accent: hsla(211. / 360., 1., 0.56, 1.),
                    accent_fg: gpui_kit::white(),
                    added: hsla(140. / 360., 0.6, 0.6, 1.),
                    removed: hsla(0., 0.7, 0.66, 1.),
                    warn: hsla(35. / 360., 0.95, 0.5, 1.),
                }
            }
            ThemeId::Emerald => Pal {
                sidebar: hex(0x08120F),
                panel: hex(0x0C1814),
                card: hex(0x132620),
                fg: hex(0xE6F2ED),
                muted: hex(0x85A698),
                line: hsla(158. / 360., 0.38, 0.55, 0.14),
                hover: hsla(158. / 360., 0.45, 0.52, 0.08),
                selected: hsla(158. / 360., 0.52, 0.48, 0.16),
                accent: hex(0x18BA83),
                accent_fg: hex(0x051F16),
                added: hex(0x34D399),
                removed: hex(0xF07167),
                warn: hex(0xF2A93B),
            },
            ThemeId::EmeraldLight => Pal {
                sidebar: hex(0xEAF3EE),
                panel: hex(0xF7FBF9),
                card: hex(0xEDF5F0),
                fg: hex(0x0F241C),
                muted: hex(0x4A6B5D),
                line: hsla(158. / 360., 0.35, 0.22, 0.11),
                hover: hsla(158. / 360., 0.40, 0.28, 0.065),
                selected: hsla(158. / 360., 0.48, 0.32, 0.12),
                accent: hex(0x067A55),
                accent_fg: gpui_kit::white(),
                added: hex(0x067A55),
                removed: hex(0xC0352E),
                warn: hex(0xB4690E),
            },
            ThemeId::Paper => {
                let fg = hex(0x1F1D1A);
                Pal {
                    sidebar: hex(0xEFECE4),
                    panel: hex(0xF8F6F0),
                    card: hex(0xF1EEE6),
                    fg,
                    muted: hex(0x69645B),
                    line: fg.opacity(0.09),
                    hover: fg.opacity(0.055),
                    selected: fg.opacity(0.095),
                    accent: hex(0xAD482B),
                    accent_fg: gpui_kit::white(),
                    added: hex(0x2D6A4F),
                    removed: hex(0xB53832),
                    warn: hex(0xB06B14),
                }
            }
            ThemeId::Sand => {
                let fg = hex(0x1C2021);
                Pal {
                    sidebar: hex(0xE7E4DC),
                    panel: hex(0xF3F1EC),
                    card: hex(0xEAE7DF),
                    fg,
                    muted: hex(0x5E6464),
                    line: fg.opacity(0.095),
                    hover: fg.opacity(0.06),
                    selected: fg.opacity(0.10),
                    accent: hex(0x2A585D),
                    accent_fg: gpui_kit::white(),
                    added: hex(0x2E6F52),
                    removed: hex(0xAE3E38),
                    warn: hex(0xAD6C16),
                }
            }
            ThemeId::Graphite => {
                let fg = hex(0xE8E5DE);
                Pal {
                    sidebar: hex(0x161615),
                    panel: hex(0x1C1C1A),
                    card: hex(0x252522),
                    fg,
                    muted: hex(0x9A968D),
                    line: fg.opacity(0.09),
                    hover: fg.opacity(0.06),
                    selected: fg.opacity(0.11),
                    accent: hex(0xD48E50),
                    accent_fg: hex(0x181614),
                    added: hex(0x68B384),
                    removed: hex(0xDE6F68),
                    warn: hex(0xDF9F45),
                }
            }
            ThemeId::System => unreachable!("resolved above"),
        }
    }

    #[cfg(test)]
    pub fn new(dark: bool) -> Pal {
        Self::for_theme(if dark { ThemeId::Dark } else { ThemeId::Light }, dark)
    }

    /// Projects this palette onto `gpui-kit` global theme colors so inputs, markdown, and scrollbars match.
    pub fn sync_kit_theme(&self, theme: &mut Theme, dark: bool) {
        let c = &mut theme.colors;
        c.background = gpui_kit::transparent_black();
        c.foreground = self.fg;
        c.muted = self.card;
        c.muted_foreground = self.muted;
        c.secondary = self.card;
        c.secondary_hover = self.hover;
        c.secondary_foreground = self.fg;
        c.border = self.line;
        c.input = self.line;
        c.caret = self.accent;
        c.ring = self.accent.opacity(0.45);
        c.selection = self.accent.opacity(if dark { 0.28 } else { 0.20 });
        c.primary = self.accent;
        c.primary_hover = self.accent.opacity(0.9);
        c.primary_foreground = self.accent_fg;
        c.accent = self.hover;
        c.accent_foreground = self.fg;
        c.link = self.accent;
        c.link_hover = self.accent.opacity(0.85);
        c.link_active = self.accent;
        c.popover = self.card;
        c.popover_foreground = self.fg;
        c.sidebar = self.sidebar;
        c.sidebar_foreground = self.fg;
        c.sidebar_border = self.line;
        c.list = self.panel;
        c.list_hover = self.hover;
        c.list_active = self.selected;
        c.list_active_border = self.accent.opacity(0.5);
        c.scrollbar_thumb = self.fg.opacity(0.18);
        c.scrollbar_thumb_hover = self.fg.opacity(0.32);
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

pub const NEW_SESSION: &str = "── 新しいセッション ──";

/// What a saved thread shows: every message, with a marker where `/clear` started a new session.
pub fn session_items(id: &str) -> Result<Vec<Item>, String> {
    let (_, messages, resets) = greencode_core::session::Session::load_all(id).map_err(|e| format!("{e:#}"))?;
    let (mut items, mut from) = (Vec::new(), 0);
    for at in resets {
        items.extend(items_from(&messages[from..at]));
        items.push(Item::Notice(NEW_SESSION.into()));
        from = at;
    }
    items.extend(items_from(&messages[from..]));
    Ok(items)
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
