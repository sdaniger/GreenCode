use super::{display, input::{plain, rows, Editor}, theme::Theme, App, Pending, Picker, KEYS};
use greencode_core::config::{ComposerShape, SymbolPreset, TuiCfg};
use greencode_core::permission::Mode;
use ratatui::{
    layout::{Position, Rect}, style::{Color, Modifier, Style}, text::{Line, Span},
    widgets::{Block, BorderType, Borders}, Frame,
};
use std::{ops::Range, time::Instant};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Kind { User, Assistant, Thinking, Tool, ToolOk, ToolError, Steer, Notice, Error }

/// Glyphs for transcript labels, the menu cursor and the title bar, per symbol preset.
pub(super) struct Symbols {
    pub header: &'static str,
    pub menu: &'static str,
    user: &'static str,
    thinking: &'static str,
    tool: &'static str,
    ok: &'static str,
    steer: &'static str,
    notice: &'static str,
    error: &'static str,
}

impl Symbols {
    pub fn new(preset: SymbolPreset) -> Self {
        match preset {
            SymbolPreset::Unicode => Self {
                header: "◆", menu: "❯ ", user: "› ", thinking: "◇ ", tool: "▸ ", ok: "✓ ",
                steer: "↪ ", notice: "· ", error: "✗ ",
            },
            SymbolPreset::Nerd => Self {
                header: "◆", menu: "\u{f054} ", user: "\u{f120} ", thinking: "\u{f0eb} ", tool: "\u{f0ad} ",
                ok: "\u{f00c} ", steer: "\u{f112} ", notice: "\u{f111} ", error: "\u{f00d} ",
            },
            SymbolPreset::Ascii => Self {
                header: "#", menu: "> ", user: "> ", thinking: "? ", tool: "+ ", ok: "v ",
                steer: "-> ", notice: "- ", error: "x ",
            },
        }
    }
}

impl Kind {
    fn label(self, symbols: &Symbols) -> &'static str {
        match self {
            Self::User => symbols.user, Self::Assistant => "", Self::Thinking => symbols.thinking,
            Self::Tool => symbols.tool, Self::ToolOk => symbols.ok, Self::Steer => symbols.steer,
            Self::Notice => symbols.notice, Self::Error | Self::ToolError => symbols.error,
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

    fn style(self, theme: &Theme, color_blind: bool) -> Style {
        let success = if color_blind { Color::Blue } else { theme.success };
        let (color, modifier) = match self {
            Self::User => (theme.user, Modifier::BOLD),
            Self::Assistant => (theme.assistant, Modifier::empty()),
            Self::Thinking => (theme.thinking, Modifier::ITALIC),
            Self::Tool => (theme.tool, Modifier::empty()),
            Self::ToolOk => (success, Modifier::empty()),
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
    markdown: bool,
    done: bool,
    /// `[react: X]` extracted from the reply this turn, shown on the user line.
    badge: Option<String>,
    /// `extensionRenderers.transcriptPresenters`: a presenter hid this block.
    hidden: bool,
    /// `tui.mouse`: a click on this entry toggled its card open.
    open: bool,
    /// `display.advisorCollapsedLines`: this entry is the advisor card, replaced as notes arrive.
    advisor: bool,
}

impl Entry {
    fn display_end(&self, expanded_tools: bool, expand_thinking: bool) -> usize {
        if expanded_tools || self.open { return self.text.len(); }
        if self.kind == Kind::Thinking && self.done && !expand_thinking {
            let max = self.text.char_indices().nth(400).map_or(self.text.len(), |(i, _)| i);
            return self.preview_end.min(max);
        }
        self.preview_end
    }
}

#[derive(Default, Clone, Copy, PartialEq)]
enum Rewrap {
    #[default]
    All,
    Last,
    None,
}

#[derive(Default)]
pub(super) struct Log {
    entries: Vec<Entry>,
    /// The transcript line index of the last user message, for its reaction badge.
    last_user: Option<usize>,
    /// `[react: X]` extracted from the reply that just finished; `None` clears it at the next prompt.
    reaction: Option<String>,
    /// `display.collapseCompacted` hides entries recorded before this divider index.
    compacted_at: Option<usize>,
    /// Which entries a settled resize may rewrap on the next draw.
    rewrap: Rewrap,
    streaming: bool,
    pending: String,
    width: u16,
    pub viewport: usize,
    expand_tools: bool,
    expand_thinking: bool,
    /// `tui.mouse`: screen rows drawn from a tool entry, for click-to-toggle.
    hits: Vec<(u16, usize)>,
}

impl Log {
    /// Adds the divider after a compaction; earlier entries collapse when `collapse_compacted` is on.
    pub fn compacted(&mut self) {
        self.add(Kind::Notice, "— context compacted —".into());
        self.compacted_at = Some(self.entries.len() - 1);
    }

    /// A settled resize: `rebuild` rewraps everything, `append` keeps settled rows and rewraps the
    /// streaming entry, `preserve` keeps every break.
    pub fn resized(&mut self, mode: greencode_core::config::ResizeScrollback) {
        use greencode_core::config::ResizeScrollback;
        self.rewrap = match mode {
            ResizeScrollback::Rebuild => Rewrap::All,
            ResizeScrollback::Append => Rewrap::Last,
            ResizeScrollback::Preserve => Rewrap::None,
        };
    }

    pub fn clear(&mut self) {
        self.compacted_at = None;
        self.entries.clear();
        self.pending.clear();
        self.streaming = false;
    }

    /// `tui.mouse`: the entry a click at screen `row` toggled, if it landed on a tool card.
    pub fn toggle_at(&mut self, row: u16) -> bool {
        let Some(&(_, index)) = self.hits.iter().find(|(y, _)| *y == row) else { return false };
        let Some(entry) = self.entries.get_mut(index) else { return false };
        entry.open = !entry.open;
        entry.ranges.clear();
        self.rewrap = Rewrap::All;
        true
    }

    fn set_tools_expanded(&mut self, expanded: bool) {
        if self.expand_tools == expanded { return; }
        self.expand_tools = expanded;
        for entry in &mut self.entries {
            if entry.kind.is_tool() && entry.preview_end < entry.text.len() { entry.ranges.clear(); }
        }
    }

    fn set_thinking_expanded(&mut self, expanded: bool) {
        if self.expand_thinking == expanded { return; }
        self.expand_thinking = expanded;
        for entry in &mut self.entries {
            if entry.kind == Kind::Thinking { entry.ranges.clear(); }
        }
    }

    pub fn add(&mut self, kind: Kind, text: String) {
        self.flush();
        if kind == Kind::User {
            self.reaction = None;
            self.last_user = Some(self.entries.len());
        }
        let text = if kind == Kind::Assistant { text } else {
            match plain(&text) { std::borrow::Cow::Owned(clean) => clean, _ => text }
        };
        let preview_end = if kind.is_tool() {
            text.grapheme_indices(true).take(241).enumerate()
                .find(|&(i, (_, grapheme))| i == 240 || grapheme == "\n")
                .map_or(text.len(), |(_, (at, _))| at)
        } else { text.len() };
        self.entries.push(Entry { kind, text, preview_end, ranges: Vec::new(), markdown: false, done: false, badge: None, hidden: false, open: false, advisor: false });
        self.streaming = false;
    }

    /// `display.advisorCollapsedLines`: one advisor card per turn, collapsed to its configured lines.
    pub fn set_advisor(&mut self, text: String) {
        self.flush();
        if let Some(entry) = self.entries.last_mut().filter(|e| e.advisor) {
            entry.text = text;
            entry.preview_end = entry.text.len();
            entry.ranges.clear();
        } else {
            self.entries.push(Entry {
                kind: Kind::Notice, preview_end: text.len(), text, ranges: Vec::new(),
                markdown: false, done: false, badge: None, hidden: false, open: false, advisor: true,
            });
        }
        self.rewrap = Rewrap::All;
    }

    /// A tool result that keeps its whole text and can be styled as Markdown.
    /// A transcript presenter redrew the newest entry: hidden, one compact row, or full.
    pub fn present(&mut self, mode: &str, text: Option<&str>) {
        let Some(entry) = self.entries.last_mut() else { return };
        match mode {
            "hidden" => entry.hidden = true,
            "full" => {
                entry.hidden = false;
                entry.preview_end = entry.text.len();
            }
            _ => {
                if let Some(text) = text {
                    entry.text = text.to_string();
                    entry.preview_end = entry.text.len();
                }
            }
        }
        self.rewrap = Rewrap::All;
    }

    /// `extensionRenderers.assistantText`: redraw the newest reply; the session keeps the original.
    pub fn redraw_assistant(&mut self, text: &str) {
        let Some(entry) = self.entries.iter_mut().rev().find(|e| e.kind == Kind::Assistant) else { return };
        entry.text = text.to_string();
        entry.preview_end = entry.text.len();
        entry.ranges.clear();
        self.rewrap = Rewrap::All;
    }

    pub fn add_full(&mut self, kind: Kind, text: String, markdown: bool) {
        self.add(kind, text);
        if let Some(entry) = self.entries.last_mut() {
            entry.preview_end = entry.text.len();
            entry.markdown = markdown;
        }
    }

    pub fn delta(&mut self, text: &str, smooth: bool) {
        if smooth {
            if !self.streaming {
                self.entries.push(Entry { kind: Kind::Assistant, text: String::new(), preview_end: 0, ranges: Vec::new(), markdown: false, done: false, badge: None, hidden: false, open: false, advisor: false });
                self.streaming = true;
            }
            self.pending.push_str(text);
        } else {
            self.append_stream(text);
        }
    }

    fn append_stream(&mut self, text: &str) {
        if !self.streaming {
            self.entries.push(Entry { kind: Kind::Assistant, text: String::new(), preview_end: 0, ranges: Vec::new(), markdown: false, done: false, badge: None, hidden: false, open: false, advisor: false });
            self.streaming = true;
        }
        if let Some(entry) = self.entries.last_mut() {
            entry.text.push_str(text);
            entry.preview_end = entry.text.len();
            entry.ranges.clear();
        }
    }

    /// Add a frame's worth of buffered streamed text. Returns whether anything was revealed.
    pub fn reveal(&mut self) -> bool {
        if self.pending.is_empty() { return false; }
        let at = self.pending.char_indices().nth(24).map_or(self.pending.len(), |(i, _)| i);
        let rest = self.pending.split_off(at);
        let chunk = std::mem::replace(&mut self.pending, rest);
        self.append_stream(&chunk);
        true
    }

    pub fn flush(&mut self) {
        if self.pending.is_empty() { return; }
        let text = std::mem::take(&mut self.pending);
        self.append_stream(&text);
    }

    pub fn finish_turn(&mut self, cfg: &greencode_core::config::TuiCfg) {
        self.flush();
        self.streaming = false;
        let display = &cfg.display;
        for entry in &mut self.entries {
            entry.done = true;
            if entry.kind != Kind::Assistant { continue; }
            if display.reactions {
                let (badge, text) = greencode_core::reactions::extract(&entry.text);
                self.reaction = badge;
                entry.text = text;
            }
            if display.render_mermaid {
                entry.text = greencode_core::mermaid::render_fences(&entry.text);
            }
            if !matches!(display.auto_graph, greencode_core::config::AutoGraph::Off) {
                if let Some(chart) = chart_block(&entry.text, cfg) {
                    entry.text.push_str(&chart);
                }
            }
            entry.preview_end = entry.text.len();
            entry.ranges.clear();
        }
        if let (Some(badge), Some(user)) = (self.reaction.take(), self.last_user) {
            if let Some(entry) = self.entries.get_mut(user) {
                entry.badge = Some(badge);
            }
        }
    }

    pub fn last_assistant(&self) -> Option<&str> {
        self.entries.iter().rev().find(|e| e.kind == Kind::Assistant).map(|e| e.text.as_str())
    }

    fn draw(&mut self, frame: &mut Frame<'_>, area: Rect, scroll: &mut usize, theme: &Theme, cfg: &TuiCfg, symbols: &Symbols) -> bool {
        self.set_tools_expanded(cfg.expand_tools);
        self.set_thinking_expanded(cfg.expand_thinking_blocks);
        let width = area.width.saturating_sub(3).max(1);
        self.viewport = usize::from(area.height);
        let mut count = 0;
        let (hide_tools, hide_thinking) = (cfg.hide_tools, cfg.hide_thinking);
        let collapsed_at = self.compacted_at.filter(|_| cfg.display.collapse_compacted);
        let resized = width != self.width;
        let last = self.entries.len().saturating_sub(1);
        for (index, entry) in self.entries.iter_mut().enumerate() {
            if collapsed_at.is_some_and(|at| index < at) { continue; }
            let may_rewrap = match self.rewrap {
                Rewrap::All => true,
                Rewrap::Last => index == last,
                Rewrap::None => false,
            };
            if !entry.kind.visible(hide_tools, hide_thinking) {
                if resized && may_rewrap { entry.ranges.clear(); }
                continue;
            }
            if (resized && may_rewrap) || entry.ranges.is_empty() {
                if entry.kind == Kind::Assistant {
                    if let std::borrow::Cow::Owned(text) = plain(&entry.text) { entry.text = text; }
                    entry.preview_end = entry.text.len();
                }
                let end = entry.display_end(self.expand_tools, self.expand_thinking);
                rows(&entry.text[..end], width, &mut entry.ranges);
            }
            count += entry.ranges.len() + 1;
        }
        self.width = width;
        self.rewrap = Rewrap::All;
        *scroll = (*scroll).min(count.saturating_sub(self.viewport));
        let end = count.saturating_sub(*scroll);
        let start = end.saturating_sub(self.viewport);
        self.hits.clear();
        let mut index = 0;
        for (entry_index, entry) in self.entries.iter().enumerate()
            .filter(|(i, _)| collapsed_at.is_none_or(|at| *i >= at))
            .filter(|(_, e)| !e.hidden && e.kind.visible(hide_tools, hide_thinking)) {
            let next = index + entry.ranges.len() + 1;
            if next <= start { index = next; continue; }
            if index >= end { break; }
            for (i, range) in entry.ranges.iter().enumerate().skip(start.saturating_sub(index)).take(end.saturating_sub(index.max(start))) {
                let text = &entry.text[range.clone()];
                let label = entry.kind.label(symbols);
                let prefix: std::borrow::Cow<'_, str> = match (i, &entry.badge) {
                    (0, Some(badge)) => format!("{badge} {label}").into(),
                    _ if i == 0 => label.into(),
                    _ if label.is_empty() => "".into(),
                    _ => "  ".into(),
                };
                let mut style = entry.kind.style(theme, cfg.color_blind_mode);
                if entry.kind == Kind::Assistant || entry.markdown {
                    style = markdown_style(style, text, theme, cfg.colorful_markdown);
                }
                let clipped = i + 1 == entry.ranges.len() && entry.display_end(self.expand_tools, self.expand_thinking) < entry.text.len();
                let rect = Rect::new(area.x, area.y + (index + i - start) as u16, area.width, 1);
                if i == 0 && entry.kind.is_tool() {
                    self.hits.push((rect.y, entry_index));
                }
                // `tui.textSizing`: an H1 heading is drawn at 2x through Kitty's OSC 66.
                let sized = if text.starts_with("# ") {
                    display::sized_heading(text, 2, cfg.display.text_sizing && kitty_terminal())
                } else {
                    text.to_string()
                };
                frame.buffer_mut().set_style(rect, style);
                frame.render_widget(Line::from(vec![Span::raw(prefix), Span::raw(sized), Span::raw(if clipped { "…" } else { "" })]).style(style), rect);
            }
            index = next;
        }
        count != 0
    }
}

/// Whether headings should be sent scaled: text sizing needs Kitty's OSC 66, not a plain VT.
fn kitty_terminal() -> bool {
    std::env::var_os("KITTY_WINDOW_ID").is_some()
        || std::env::var("TERM").is_ok_and(|term| term.contains("kitty") || term.contains("ghostty"))
}

fn markdown_style(style: Style, text: &str, theme: &Theme, colorful: bool) -> Style {
    if text.starts_with('#') && colorful { style.fg(theme.heading).add_modifier(Modifier::BOLD) }
    else if text.starts_with("```") && colorful { style.fg(theme.code).add_modifier(Modifier::BOLD) }
    else if text.starts_with('#') { style.add_modifier(Modifier::BOLD) }
    else { style }
}

/// The live spinner frame for the activity label.
pub(super) fn glyph(preset: SymbolPreset) -> &'static str {
    match preset { SymbolPreset::Ascii => "* ", SymbolPreset::Nerd => "\u{f0e7} ", SymbolPreset::Unicode => "● " }
}

fn line(frame: &mut Frame<'_>, area: Rect, offset: u16, line: Line<'_>) {
    if offset < area.height && area.width > 0 {
        frame.render_widget(line, Rect::new(area.x, area.y + offset, area.width, 1));
    }
}

pub(super) fn draw(frame: &mut Frame<'_>, app: &mut App<'_>) -> Option<Rect> {
    let area = frame.area();
    // `startup.showSplash`: logo and version for a short moment before the session screen.
    if let Some(lines) = app.splash_lines() {
        let top = (area.height / 2).saturating_sub(lines.len() as u16 / 2);
        for (n, text) in lines.iter().enumerate() {
            line(frame, area, top + n as u16, Line::from(Span::styled(text.clone(), Style::default().fg(app.theme.accent).add_modifier(Modifier::BOLD))));
        }
        return None;
    }
    // `startup.setupWizard`: a short first-run flow that chooses the default model.
    if let Some(rows) = app.wizard_lines() {
        let top = (area.height / 2).saturating_sub(rows.len() as u16 / 2);
        for (n, text) in rows.iter().enumerate() {
            line(frame, area, top + n as u16, Line::raw(text.clone()));
        }
        return None;
    }
    if area.width < 12 || area.height < 10 {
        if let Some(Pending::Permission { shown, .. }) = app.pending.front_mut() { *shown = false; }
        line(frame, area, 0, Line::raw("Resize terminal"));
        return None;
    }
    let theme = &app.theme;
    let symbols = Symbols::new(app.tui.symbol_preset);
    let accent = Style::default().fg(theme.accent);
    let soft = Style::default().fg(theme.muted);
    let dim = Style::default().fg(theme.dim);
    let pad = u16::from(!app.tui.tight);
    let x = area.x + pad;
    let width = area.width - 2 * pad;
    line(frame, area, 0, Line::from(vec![
        Span::raw(if pad == 0 { "" } else { " " }), Span::styled(format!("{} GreenCode", symbols.header), accent.add_modifier(Modifier::BOLD)),
        Span::styled("  native", dim), Span::styled(format!("  ·  {}", env!("CARGO_PKG_VERSION")), dim),
    ]));
    line(frame, Rect::new(x, area.y + 1, width, 1), 0, Line::styled(plain(&app.meta.cwd.to_string_lossy()).into_owned(), soft));

    let footer = area.bottom() - 2;
    let ghost = app.prediction().map(|text| plain(text).into_owned());
    let session = app.session_name();
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
    let input_rows_height = input_rows.len().min(8).min(usize::from(if waiting { (available / 2).max(1) } else { available })) as u16;
    // An extra empty row keeps macOS IME preedit off the bottom border.
    let input_height = input_rows_height + u16::from(app.tui.ime_safe_cursor);
    let prompt_height = prompt_rows.len().min(usize::from(available - input_height)).min(14) as u16;
    let plain_shape = matches!(app.tui.composer.shape, ComposerShape::Plain);
    let frame_rows = if plain_shape { 0 } else { 2 };
    let compose_height = frame_rows + prompt_height + if editor.is_none() { 1 } else { input_height };
    let compose_top = footer - compose_height;
    let compose = Rect::new(x, compose_top, width, compose_height);
    let title = if approval_shown.is_some() { " Approval required ".to_string() } else if waiting { " Question ".to_string() } else if app.menu.is_some() { " Filter ".to_string() }
        else { " Prompt ".to_string() };
    let border = Style::default().fg(if waiting { theme.warning } else if app.tui.status_line.session_accent {
        Color::Indexed(16 + greencode_core::status_line::session_color(&session))
    } else { theme.border });
    if !plain_shape {
        let border_type = if app.tui.composer.shape == ComposerShape::Box { BorderType::Double } else { BorderType::Rounded };
        frame.render_widget(Block::new().borders(Borders::ALL).border_type(border_type).border_style(border).title(title), compose);
    }
    let inside = if plain_shape { compose } else { Rect::new(x + 1, compose_top + 1, width - 2, compose_height - 2) };
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
            let caret = if app.tui.symbol_preset == SymbolPreset::Ascii { "> " } else { "› " };
            let mark = if i == 0 { caret } else { "  " };
            let mut spans = vec![Span::styled(mark, border)];
            // `spelling.typoDetection`: misspelled prose words are underlined; dialogs and secrets never are.
            let typos: &[Range<usize>] = if waiting || secret { &[] } else { &app.typos };
            let mut at = range.start;
            for typo in typos.iter().filter(|t| t.end > range.start && t.start < range.end) {
                let (from, to) = (typo.start.max(range.start), typo.end.min(range.end));
                if from > at { spans.push(Span::styled(&text[at..from], theme.text)); }
                spans.push(Span::styled(&text[from..to], Style::default().fg(theme.error).add_modifier(Modifier::UNDERLINED)));
                at = to;
            }
            if at < range.end { spans.push(Span::styled(&text[at..range.end], theme.text)); }
            // `spelling.autocomplete`: the rest of the suggested word follows the caret as ghost text; Tab accepts it.
            if !waiting && !secret && i == cursor_row && *cursor >= range.end {
                if let Some((word_range, word)) = app.completion.as_ref().filter(|(word_range, _)| word_range.end == *cursor) {
                    spans.push(Span::styled(word.get(word_range.len()..).unwrap_or_default().to_owned(), Style::default().fg(theme.dim).add_modifier(Modifier::ITALIC)));
                }
            }
            line(frame, inside, prompt_height + (i - first) as u16, Line::from(spans));
        }
        if let Some(ghost) = &ghost {
            line(frame, inside, prompt_height, Line::from(vec![
                Span::styled(if app.tui.symbol_preset == SymbolPreset::Ascii { "> " } else { "› " }, border),
                Span::styled(ghost.clone(), dim),
            ]));
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
                let mark = if i == selected { symbols.menu } else { "  " };
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
        // Pinned live agents, hook notices and the shimmering activity label sit above the status row.
        let pinned = display::pinned_rows(&app.agents, app.tui.display.pinned_agents, app.tui.display.subagent_live_preview, app.cfg.task.show_resolved_model_badge, usize::from(width));
        let hook = app.hook_status.as_ref().filter(|_| app.tui.status_line.show_hook_status)
            .filter(|(_, at)| at.elapsed().as_secs() < 10).map(|(text, _)| text.as_str());
        let activity = app.busy.map(|started| {
            let label = if app.cancelling { format!("Stopping · {}s", started.elapsed().as_secs()) }
                else if waiting { "Awaiting your response".to_string() }
                else { format!("Working · {}s", started.elapsed().as_secs()) };
            let tick = (started.elapsed().as_millis() / 100) as usize;
            (label, display::shimmer_at(&"Working", tick, app.tui.display.shimmer, app.tui.motion))
        });
        let reserved = pinned.len() as u16 + u16::from(hook.is_some()) + u16::from(activity.is_some());
        if history_end.saturating_sub(reserved) > area.y + 3 {
            history_end -= reserved;
            let mut base = history_end;
            app.pin_top = Some(base);
            for (n, row) in pinned.iter().enumerate() {
                // `tui.mouse`: the focused agent's pinned row reads in reverse.
                let focused = app.agent_focus.is_some()
                    && display::pinned_hit(&app.agents, app.tui.display.pinned_agents, n) == app.agent_focus;
                let style = if focused { soft.bg(theme.selection_bg) } else { soft };
                line(frame, Rect::new(x, base, width, 1), 0, Line::styled(row.clone(), style));
                base += 1;
            }
            if let Some(text) = hook {
                line(frame, Rect::new(x, base, width, 1), 0, Line::styled(plain(text).into_owned(), dim));
                base += 1;
            }
            if let Some((label, shimmer)) = activity {
                let mut spans = vec![Span::styled(glyph(app.tui.symbol_preset), accent)];
                match shimmer.filter(|_| label.starts_with("Working")) {
                    Some((from, to)) => {
                        let chars: Vec<char> = label.chars().collect();
                        let (a, b) = (from.min(chars.len()), to.min(chars.len()));
                        spans.push(Span::styled(chars[..a].iter().collect::<String>(), accent));
                        spans.push(Span::styled(chars[a..b].iter().collect::<String>(), Style::default().fg(theme.heading).add_modifier(Modifier::BOLD)));
                        spans.push(Span::styled(chars[b..].iter().collect::<String>(), accent));
                    }
                    None => spans.push(Span::styled(label, accent)),
                }
                if !app.queue.is_empty() {
                    spans.push(Span::styled(format!("  ·  {} queued", app.queue.len()), soft));
                }
                line(frame, Rect::new(x, base, width, 1), 0, Line::from(spans));
            }
        }
        let row = app.status_row(usize::from(width));
        let session_fg = if app.tui.status_line.session_accent {
            Color::Indexed(16 + greencode_core::status_line::session_color(&session))
        } else { theme.accent };
        let mut spans = vec![Span::styled(row.left.clone(), Style::default().fg(theme.muted))];
        for &(glyph, filled) in &row.gauge {
            spans.push(Span::styled(glyph.to_string(), if filled { Style::default().fg(session_fg) } else { dim }));
        }
        spans.push(Span::styled(row.right.clone(), Style::default().fg(theme.muted)));
        line(frame, Rect::new(x, history_end, width, 1), 0, Line::from(spans));
    }
    let mut history = Rect::new(x, area.y + 3, width, history_end.saturating_sub(area.y + 3));
    // The pending image takes rows off the bottom of history so its placement is not overdrawn.
    let mut inline = None;
    if app.inline_image.is_some() {
        if let Some((_, rows)) = app.inline_payload(history.width) {
            let rows = rows.min(history.height);
            if rows > 0 {
                history.height -= rows;
                inline = Some(Rect::new(history.x, history.bottom(), history.width, rows));
            }
        }
    }
    if !app.log.draw(frame, history, &mut app.scroll, theme, &app.tui, &symbols) {
        line(frame, history, 0, Line::styled("Type a prompt or / for commands.", soft));
    }

    // Everything the status row already shows (model, mode, thinking, ctx) stays out of this line.
    let mut stats = String::new();
    if let Some(label) = app.vim.mode_label(app.tui.vim_mode_display) {
        stats.push_str(&format!("  ·  {label}"));
    }
    if app.tui.composer.token_rate {
        if let Some(rate) = app.token_rate() { stats.push_str(&format!("  ·  {rate:.0} tok/s")); }
    }
    // `goal.statusInFooter`: the session goal and its token budget ride in the status line.
    if app.cfg.modes.goal.enabled && app.cfg.modes.goal.status_in_footer {
        if let Some(goal) = greencode_core::goal::Goal::load(&app.meta.id) {
            if goal.status != greencode_core::goal::Status::Complete {
                stats.push_str(&format!("  ·  {}", plain(&goal.line(true))));
            }
        }
    }
    if app.tui.show_usage {
        let usage = app.usage;
        stats.push_str(&format!("  ·  ctx {}  ↑{} ↓{} cached {}", app.last_context, usage.input, usage.output, usage.cache_read));
    }
    let meta = Rect::new(x, footer, width, 1);
    let status_bg = if app.tui.status_line.transparent { Color::Reset } else { theme.status_bg };
    frame.buffer_mut().set_style(meta, Style::default().bg(status_bg));
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
    // The old model/mode/ctx footer is gone: the status row above carries it. Its extras ride here.
    let hints = if stats.is_empty() { keys.to_string() } else { format!("{}  ·  {keys}", stats.trim_start_matches("  ·  ")) };
    line(frame, Rect::new(x, footer, width, 1), 0, Line::styled(hints, dim));
    if app.fireworks.is_some() {
        let top = Rect::new(area.x, area.y, area.width, (area.height / 3).max(3));
        let tick = app.fireworks.map_or(0, |at| at.elapsed().as_millis() / 100) as usize;
        for (x, y, glyph) in display::fireworks(tick, top.width, top.height) {
            let cell = Rect::new(top.x + x, top.y + y, 1, 1);
            frame.buffer_mut().set_string(cell.x, cell.y, glyph.to_string(), Style::default().fg(theme.warning));
        }
    }
    inline
}

/// A bar chart appended below the reply's last numeric table on graphics terminals.
fn chart_block(text: &str, cfg: &TuiCfg) -> Option<String> {
    if greencode_core::image::protocol().is_none() {
        return None;
    }
    let table = greencode_core::graph::last_numeric_table(text)?;
    let ascii = cfg.symbol_preset == SymbolPreset::Ascii;
    let lines = greencode_core::graph::chart(&table, 48, ascii);
    if lines.is_empty() { return None; }
    Some(format!("\n\n```\n{}\n```", lines.join("\n")))
}

fn composer_text(editor: &Editor, secret: bool) -> (std::borrow::Cow<'_, str>, usize) {
    if !secret { return (std::borrow::Cow::Borrowed(editor.text()), editor.cursor()); }
    let text: String = editor.text().graphemes(true).map(|g| if g == "\n" { '\n' } else { '•' }).collect();
    let cursor = editor.text()[..editor.cursor()].graphemes(true).map(|g| if g == "\n" { 1 } else { '•'.len_utf8() }).sum();
    (std::borrow::Cow::Owned(text), cursor)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn symbol_presets_and_color_blind_mode_change_glyphs_and_success_color() {
        let unicode = Symbols::new(SymbolPreset::Unicode);
        let nerd = Symbols::new(SymbolPreset::Nerd);
        let ascii = Symbols::new(SymbolPreset::Ascii);
        assert_eq!(Kind::User.label(&unicode), "› ");
        assert_eq!(Kind::ToolOk.label(&ascii), "v ");
        assert_eq!(Kind::Error.label(&ascii), "x ");
        assert_eq!(Kind::Error.label(&nerd), "\u{f00d} ");
        assert_eq!(Kind::User.label(&ascii).width(), 2);
        let theme = Theme::default();
        assert_eq!(Kind::ToolOk.style(&theme, false).fg, Some(Color::Green));
        assert_eq!(Kind::ToolOk.style(&theme, true).fg, Some(Color::Blue));
        assert_eq!(Kind::Error.style(&theme, true).fg, Some(Color::LightRed));
    }

    #[test]
    fn click_hit_row_toggles_the_tool_card_it_belongs_to() {
        let mut log = Log::default();
        log.add(Kind::ToolOk, "a long tool result\nthat spans several\nrows here".into());
        log.add(Kind::Assistant, "plain reply".into());
        // Draw registers the first row each tool card occupies; rows without one never toggle.
        log.hits = vec![(4, 0)];
        assert!(!log.toggle_at(5));
        assert!(log.toggle_at(4));
        assert!(log.entries[0].open);
        assert_eq!(log.entries[0].display_end(false, false), log.entries[0].text.len());
        assert!(log.toggle_at(4));
        assert!(!log.entries[0].open);
        assert!(log.entries[0].display_end(false, false) < log.entries[0].text.len());
        assert!(!log.entries[1].open);
    }

    #[test]
    fn advisor_notes_replace_one_collapsed_card() {
        let mut log = Log::default();
        log.set_advisor(display::advisor_preview("one\ntwo\nthree", 1));
        log.set_advisor(display::advisor_preview("one\ntwo\nthree\nfour", 1));
        assert_eq!(log.entries.len(), 1);
        assert!(log.entries[0].advisor);
        assert_eq!(log.entries[0].text, "one\n… 3 more note lines");
    }

    #[test]
    fn smooth_streaming_buffers_until_revealed_and_thinking_collapses_after_the_turn() {
        let mut log = Log::default();
        log.delta("hello", true);
        assert_eq!(log.last_assistant(), Some(""));
        log.flush();
        assert_eq!(log.last_assistant(), Some("hello"));

        log.add(Kind::Thinking, "x".repeat(500));
        log.finish_turn(&TuiCfg::default());
        assert_eq!(log.entries[1].display_end(false, false), 400);
        assert_eq!(log.entries[1].display_end(false, true), 500);
    }
}
