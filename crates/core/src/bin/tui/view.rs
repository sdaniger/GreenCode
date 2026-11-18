use super::{input::{plain, rows, Editor}, theme::Theme, App, Pending, Picker, KEYS};
use greencode_core::config::TuiCfg;
use greencode_core::permission::Mode;
use ratatui::{
    layout::{Position, Rect}, style::{Modifier, Style}, text::{Line, Span},
    widgets::{Block, BorderType, Borders}, Frame,
};
use std::{ops::Range, time::Instant};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Kind { User, Assistant, Thinking, Tool, ToolOk, ToolError, Steer, Notice, Error }

impl Kind {
    fn label(self) -> &'static str {
        match self {
            Self::User => "› ", Self::Assistant => "", Self::Thinking => "◇ ",
            Self::Tool => "▸ ", Self::ToolOk => "✓ ", Self::Steer => "↪ ",
            Self::Notice => "· ", Self::Error | Self::ToolError => "✗ ",
        }
    }

    fn is_tool(self) -> bool { matches!(self, Self::Tool | Self::ToolOk | Self::ToolError) }

    fn visible(self, hide_tools: bool, hide_thinking: bool) -> bool {
        match self {
            Self::Tool | Self::ToolOk => !hide_tools,
            Self::Thinking => !hide_thinking,
            _ => true,
        }
    }

    fn style(self, theme: &Theme) -> Style {
        let (color, modifier) = match self {
            Self::User => (theme.user, Modifier::BOLD),
            Self::Assistant => (theme.assistant, Modifier::empty()),
            Self::Thinking => (theme.thinking, Modifier::ITALIC),
            Self::Tool => (theme.tool, Modifier::empty()),
            Self::ToolOk => (theme.success, Modifier::empty()),
            Self::Steer => (theme.heading, Modifier::empty()),
            Self::Notice => (theme.muted, Modifier::empty()),
            Self::Error | Self::ToolError => (theme.error, Modifier::empty()),
        };
        let bg = match self {
            Self::User => theme.user_bg,
            Self::Tool => theme.tool_bg,
            Self::ToolOk => theme.tool_ok_bg,
            Self::ToolError => theme.tool_error_bg,
            _ => ratatui::style::Color::Reset,
        };
        Style::default().fg(color).bg(bg).add_modifier(modifier)
    }
}

struct Entry {
    kind: Kind,
    text: String,
    preview_end: usize,
    ranges: Vec<Range<usize>>,
}

impl Entry {
    fn display_end(&self, expanded: bool) -> usize {
        if expanded { self.text.len() } else { self.preview_end }
    }
}

#[derive(Default)]
pub(super) struct Log {
    entries: Vec<Entry>,
    streaming: bool,
    width: u16,
    pub viewport: usize,
    expand_tools: bool,
}

impl Log {
    pub fn clear(&mut self) {
        self.entries.clear();
        self.streaming = false;
    }

    fn set_tools_expanded(&mut self, expanded: bool) {
        if self.expand_tools == expanded { return; }
        self.expand_tools = expanded;
        for entry in &mut self.entries {
            if entry.kind.is_tool() && entry.preview_end < entry.text.len() { entry.ranges.clear(); }
        }
    }

    pub fn add(&mut self, kind: Kind, text: String) {
        let text = if kind == Kind::Assistant { text } else {
            match plain(&text) { std::borrow::Cow::Owned(clean) => clean, _ => text }
        };
        let preview_end = if kind.is_tool() {
            text.grapheme_indices(true).take(241).enumerate()
                .find(|&(i, (_, grapheme))| i == 240 || grapheme == "\n")
                .map_or(text.len(), |(_, (at, _))| at)
        } else { text.len() };
        self.entries.push(Entry { kind, text, preview_end, ranges: Vec::new() });
        self.streaming = false;
    }

    pub fn delta(&mut self, text: &str) {
        if self.streaming {
            let entry = self.entries.last_mut().unwrap();
            entry.text.push_str(text);
            entry.ranges.clear();
        } else {
            self.add(Kind::Assistant, text.to_owned());
            self.streaming = true;
        }
    }

    pub fn end_delta(&mut self) { self.streaming = false; }

    fn draw(&mut self, frame: &mut Frame<'_>, area: Rect, scroll: &mut usize, theme: &Theme, cfg: &TuiCfg) -> bool {
        self.set_tools_expanded(cfg.expand_tools);
        let width = area.width.saturating_sub(3).max(1);
        self.viewport = usize::from(area.height);
        let mut count = 0;
        let (hide_tools, hide_thinking) = (cfg.hide_tools, cfg.hide_thinking);
        for entry in &mut self.entries {
            if !entry.kind.visible(hide_tools, hide_thinking) {
                if width != self.width { entry.ranges.clear(); }
                continue;
            }
            if width != self.width || entry.ranges.is_empty() {
                if entry.kind == Kind::Assistant {
                    if let std::borrow::Cow::Owned(text) = plain(&entry.text) { entry.text = text; }
                    entry.preview_end = entry.text.len();
                }
                let end = entry.display_end(self.expand_tools);
                rows(&entry.text[..end], width, &mut entry.ranges);
            }
            count += entry.ranges.len() + 1;
        }
        self.width = width;
        *scroll = (*scroll).min(count.saturating_sub(self.viewport));
        let end = count.saturating_sub(*scroll);
        let start = end.saturating_sub(self.viewport);
        let mut index = 0;
        for entry in self.entries.iter().filter(|e| e.kind.visible(hide_tools, hide_thinking)) {
            let next = index + entry.ranges.len() + 1;
            if next <= start { index = next; continue; }
            if index >= end { break; }
            for (i, range) in entry.ranges.iter().enumerate().skip(start.saturating_sub(index)).take(end.saturating_sub(index.max(start))) {
                let text = &entry.text[range.clone()];
                let prefix = if i == 0 { entry.kind.label() } else if entry.kind.label().is_empty() { "" } else { "  " };
                let mut style = entry.kind.style(theme);
                if entry.kind == Kind::Assistant {
                    if text.starts_with('#') { style = style.fg(theme.heading).add_modifier(Modifier::BOLD); }
                    else if text.starts_with("```") { style = style.fg(theme.code).add_modifier(Modifier::BOLD); }
                }
                let clipped = i + 1 == entry.ranges.len() && entry.display_end(self.expand_tools) < entry.text.len();
                let rect = Rect::new(area.x, area.y + (index + i - start) as u16, area.width, 1);
                frame.buffer_mut().set_style(rect, style);
                frame.render_widget(Line::from(vec![Span::raw(prefix), Span::raw(text), Span::raw(if clipped { "…" } else { "" })]).style(style), rect);
            }
            index = next;
        }
        count != 0
    }
}

fn line(frame: &mut Frame<'_>, area: Rect, offset: u16, line: Line<'_>) {
    if offset < area.height && area.width > 0 {
        frame.render_widget(line, Rect::new(area.x, area.y + offset, area.width, 1));
    }
}

pub(super) fn draw(frame: &mut Frame<'_>, app: &mut App<'_>) {
    let area = frame.area();
    if area.width < 12 || area.height < 10 {
        if let Some(Pending::Permission { shown, .. }) = app.pending.front_mut() { *shown = false; }
        line(frame, area, 0, Line::raw("Resize terminal"));
        return;
    }
    let theme = &app.theme;
    let accent = Style::default().fg(theme.accent);
    let soft = Style::default().fg(theme.muted);
    let dim = Style::default().fg(theme.dim);
    let pad = u16::from(!app.tui.tight);
    let x = area.x + pad;
    let width = area.width - 2 * pad;
    line(frame, area, 0, Line::from(vec![
        Span::raw(if pad == 0 { "" } else { " " }), Span::styled("◆ GreenCode", accent.add_modifier(Modifier::BOLD)),
        Span::styled("  native", dim), Span::styled(format!("  ·  {}", env!("CARGO_PKG_VERSION")), dim),
    ]));
    line(frame, Rect::new(x, area.y + 1, width, 1), 0, Line::styled(plain(&app.meta.cwd.to_string_lossy()).into_owned(), soft));

    let footer = area.bottom() - 2;
    let pending = app.pending.front_mut();
    let waiting = pending.is_some();
    let (prompt, prompt_scroll, mut approval_shown, editor, secret) = match pending {
        Some(Pending::Permission { label, offset, shown, .. }) => (Some(label.as_str()), Some(offset), Some(shown), None, false),
        Some(Pending::Dialog { dialog, input, offset, .. }) => (Some(dialog.prompts[dialog.answers.len()].as_str()), Some(offset), None, Some(&*input), dialog.secret()),
        None => (None, None, None, Some(&app.input), false),
    };
    let prompt = prompt.map(plain);
    let mut prompt_rows = Vec::new();
    if let Some(text) = &prompt { rows(text, width - 2, &mut prompt_rows); }
    let available = area.height - 8;
    let mut input_rows = Vec::new();
    let mut rendered = None;
    if let Some(editor) = editor { rendered = Some(composer_text(editor, secret)); }
    if let Some((text, _)) = &rendered { rows(text, width - 4, &mut input_rows); }
    // The question and its input together must leave at least one row of history on a short terminal.
    let input_height = input_rows.len().min(8).min(usize::from(if waiting { (available / 2).max(1) } else { available })) as u16;
    let prompt_height = prompt_rows.len().min(usize::from(available - input_height)).min(14) as u16;
    let compose_height = 2 + prompt_height + if editor.is_none() { 1 } else { input_height };
    let compose_top = footer - compose_height;
    let compose = Rect::new(x, compose_top, width, compose_height);
    let title = if approval_shown.is_some() { " Approval required " } else if waiting { " Question " } else if app.menu.is_some() { " Filter " } else { " Prompt " };
    let border = Style::default().fg(if waiting { theme.warning } else { theme.border });
    frame.render_widget(Block::new().borders(Borders::ALL).border_type(BorderType::Rounded).border_style(border).title(title), compose);
    let inside = Rect::new(x + 1, compose_top + 1, width - 2, compose_height - 2);
    if let Some(text) = &prompt {
        let last = prompt_rows.len().saturating_sub(usize::from(prompt_height));
        let offset = prompt_scroll.map_or(0, |p| { *p = (*p).min(last); *p });
        for (n, range) in prompt_rows.iter().skip(offset).take(prompt_height.into()).enumerate() {
            line(frame, inside, n as u16, Line::styled(&text[range.clone()], theme.text));
        }
        if let Some(shown) = approval_shown.take() { *shown = offset == last; }
    }
    if let Some((text, cursor)) = &rendered {
        let cursor_row = input_rows.partition_point(|r| r.start <= *cursor).saturating_sub(1);
        let first = (cursor_row + 1).saturating_sub(usize::from(input_height));
        for (i, range) in input_rows.iter().enumerate().skip(first).take(input_height.into()) {
            let mark = if i == 0 { "› " } else { "  " };
            line(frame, inside, prompt_height + (i - first) as u16, Line::from(vec![Span::styled(mark, border), Span::styled(&text[range.clone()], theme.text)]));
        }
        let column = text[input_rows[cursor_row].start..*cursor].width().min(usize::from(inside.width - 3));
        frame.set_cursor_position(Position::new(inside.x + 2 + column as u16, inside.y + prompt_height + (cursor_row - first) as u16));
    } else {
        let label = if prompt_rows.len() > usize::from(prompt_height) {
            "↑↓ View details · y Allow · a Always · n Deny"
        } else { "y Allow · a Always · n Deny" };
        line(frame, inside, prompt_height, Line::styled(label, border));
    }

    let mut history_end = compose_top;
    if !waiting && (app.menu.is_some() || app.has_slash_prefix()) {
        let matches = app.matches();
        let visible = matches.len().max(1).min(app.tui.autocomplete_max_visible)
            .min(usize::from(history_end.saturating_sub(area.y + 5)));
        if visible > 0 {
            history_end -= visible as u16;
            let menu = Rect::new(x + 1, history_end, width - 2, visible as u16);
            let selected = app.selected.min(matches.len().saturating_sub(1));
            let start = selected.saturating_sub(visible - 1);
            if matches.is_empty() { line(frame, menu, 0, Line::styled("No matches", dim)); }
            for (i, (name, detail)) in matches.iter().enumerate().skip(start).take(visible) {
                let mark = if i == selected { "❯ " } else { "  " };
                let prefix = if app.menu.is_some() { "" } else { "/" };
                let name = plain(name.lines().next().unwrap_or_default());
                let detail = plain(detail);
                if i == selected {
                    frame.buffer_mut().set_style(Rect::new(menu.x, menu.y + (i - start) as u16, menu.width, 1), Style::default().bg(theme.selection_bg));
                }
                line(frame, menu, (i - start) as u16, Line::from(vec![
                    Span::styled(format!("{mark}{prefix}{name}"), if i == selected { accent.add_modifier(Modifier::BOLD) } else { soft }),
                    Span::styled(format!("  {detail}"), dim),
                ]));
            }
        }
    }
    if history_end > area.y + 3 {
        history_end -= 1;
        let mut status = String::new();
        if let Some(start) = app.busy {
            let now = Instant::now().saturating_duration_since(start);
            let animation = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
            let symbol = if app.cancelling { "…" } else { animation[(now.as_millis() / 100 % 10) as usize] };
            let activity = if app.fetching_review.is_some() { "Fetching review diff" }
                else if app.cancelling { "Stopping" } else if waiting { "Awaiting your response" } else { "Working" };
            status = format!("{symbol} {activity} · {}s", now.as_secs());
        }
        if !app.queue.is_empty() { status.push_str(&format!("  ·  {} {}", app.queue.len(), if app.held { "held (/queue run)" } else { "queued" })); }
        if app.scroll > 0 { status.push_str("  ·  PgDn to return to latest"); }
        let agents = app.agents.values().filter(|(state, _)| state == "running").count();
        if agents > 0 { status.push_str(&format!("  ·  {agents} sub-agent(s) /agents")); }
        line(frame, Rect::new(x, history_end, width, 1), 0, Line::styled(status, accent));
    }
    let history = Rect::new(x, area.y + 3, width, history_end.saturating_sub(area.y + 3));
    if !app.log.draw(frame, history, &mut app.scroll, theme, &app.tui) {
        line(frame, history, 0, Line::styled("Type a prompt or / for commands.", soft));
    }

    let mode = match app.perms.mode() { Mode::Ask => "ask", Mode::AutoEdit => "auto-edit", Mode::Full => "full", Mode::Plan => "plan" };
    let effort = app.cfg.effort.as_deref().unwrap_or("default");
    let mut stats = format!("  ·  {mode}  ·  thinking {}", plain(effort));
    if app.tui.show_usage {
        let usage = app.usage;
        stats.push_str(&format!("  ·  ctx {}  ↑{} ↓{} cached {}", app.last_context, usage.input, usage.output, usage.cache_read));
    }
    let meta = Rect::new(x, footer, width, 2);
    frame.buffer_mut().set_style(meta, Style::default().bg(theme.status_bg));
    line(frame, meta, 0, Line::from(vec![Span::styled(plain(&app.meta.model), accent), Span::styled(stats, soft)]));
    let keys = if waiting { "Esc Deny / Cancel · ↑↓ Scroll question" } else if app.menu.is_some() {
        match app.menu.as_ref().map(|m| &m.kind) {
            Some(Picker::Model) => "Select model  ·  ↑↓ Navigate · Enter Select · Esc Back",
            Some(Picker::Session) => "Select session  ·  ↑↓ Navigate · Enter Select · Esc Back",
            Some(Picker::History) => "Search history · ↑↓ Navigate · Enter Restore to draft · Esc Back",
            Some(Picker::Settings) => "Terminal preferences · ↑↓ Navigate · Enter Edit · Esc Back",
            Some(Picker::Review(_)) => "Review target · ↑↓ Navigate · Enter Select · Esc Back",
            Some(Picker::ReviewBase(_)) => "Select base branch · ↑↓ Navigate · Enter Review · Esc Back",
            Some(Picker::ReviewCommit(_)) => "Select commit · ↑↓ Navigate · Enter Review · Esc Back",
            _ => "↑↓ Navigate · Enter Select · Esc Back",
        }
    } else { KEYS };
    line(frame, meta, 1, Line::styled(keys, dim));
}

fn composer_text(editor: &Editor, secret: bool) -> (std::borrow::Cow<'_, str>, usize) {
    if !secret { return (std::borrow::Cow::Borrowed(editor.text()), editor.cursor()); }
    let text: String = editor.text().graphemes(true).map(|g| if g == "\n" { '\n' } else { '•' }).collect();
    let cursor = editor.text()[..editor.cursor()].graphemes(true).map(|g| if g == "\n" { 1 } else { '•'.len_utf8() }).sum();
    (std::borrow::Cow::Owned(text), cursor)
}
