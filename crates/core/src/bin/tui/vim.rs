//! Modal (Vim) prompt editing for the composer: Normal, Insert and Visual modes with the
//! motions, operators, text objects, registers and undo listed in OMP's `tui.vimMode`.
use super::input::Editor;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum VimMode {
    #[default]
    Insert,
    Normal,
    Visual,
    VisualLine,
}

impl VimMode {
    /// The status-line label: `text` spells the mode, `icon` a glyph, `none` hides it.
    pub(super) fn label(self, display: greencode_core::config::VimModeDisplay) -> Option<&'static str> {
        use greencode_core::config::VimModeDisplay::*;
        Some(match (display, self) {
            (None, _) => return Option::None,
            (Text, VimMode::Insert) => "-- INSERT --",
            (Text, VimMode::Normal) => "-- NORMAL --",
            (Text, VimMode::Visual) => "-- VISUAL --",
            (Text, VimMode::VisualLine) => "-- VISUAL LINE --",
            (Icon, VimMode::Insert) => "I",
            (Icon, VimMode::Normal) => "N",
            (Icon, VimMode::Visual) => "V",
            (Icon, VimMode::VisualLine) => "V",
        })
    }
}

/// What the composer should do with a consumed key.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum VimOutcome {
    /// Fully handled; the composer is redrawn.
    Handled,
    /// Not a Vim key: pass it to the ordinary composer bindings.
    Pass,
    /// Normal mode wanted to submit the prompt.
    Submit,
}

#[derive(Default)]
pub(super) struct Vim {
    pub mode: VimMode,
    /// Count prefix and a pending operator (`d`, `c`, `y`), in key order.
    pending: String,
    register: String,
    register_linewise: bool,
    undo: Vec<(String, usize)>,
    /// Where a Visual selection started.
    anchor: usize,
}

impl Vim {
    pub fn new(enabled: bool) -> Self {
        let mut vim = Self::default();
        if enabled {
            vim.mode = VimMode::Normal;
        }
        vim
    }

    pub fn mode_label(&self, display: greencode_core::config::VimModeDisplay) -> Option<&'static str> {
        self.mode.label(display)
    }

    /// Applies one key. `journal` records the previous (text, cursor) so `u` can restore it.
    pub fn key(&mut self, key: KeyEvent, editor: &mut Editor, journal: bool) -> VimOutcome {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        if self.mode == VimMode::Insert {
            if key.code == KeyCode::Esc {
                self.mode = VimMode::Normal;
                // Insert leaves the cursor where typing stopped, as Vim does.
                let at = if editor.cursor() > 0 { super::input::prev_boundary(editor.text(), editor.cursor()) } else { 0 };
                editor.set(editor.text().to_owned(), at);
                self.pending.clear();
                return VimOutcome::Handled;
            }
            return VimOutcome::Pass;
        }
        if ctrl || alt {
            return VimOutcome::Pass;
        }
        let KeyCode::Char(c) = key.code else {
            if key.code != KeyCode::Esc {
                return VimOutcome::Pass;
            }
            self.pending.clear();
            self.mode = VimMode::Normal;
            return VimOutcome::Handled;
        };
        if c.is_ascii_digit() && !(c == '0' && self.pending.is_empty()) {
            self.pending.push(c);
            return VimOutcome::Handled;
        }
        if let Some(outcome) = self.pending_key(c, editor, journal) {
            return outcome;
        }
        if self.mode != VimMode::Normal {
            return self.visual(c, editor);
        }
        self.normal(c, editor, journal)
    }

    /// Finishes a pending operator (`dw`, `di(`, `yy`) or `gg`; a bare count falls through to the command.
    fn pending_key(&mut self, c: char, editor: &mut Editor, journal: bool) -> Option<VimOutcome> {
        let count = self.count();
        match (self.operator(), c) {
            (Some(_), 'i' | 'a') if !self.pending.ends_with(['i', 'a']) => {
                self.pending.push(c);
                Some(VimOutcome::Handled)
            }
            (Some(op), motion) => {
                let span = self.motion_span(motion, editor, count);
                self.pending.clear();
                let Some(span) = span else { return Some(VimOutcome::Handled) };
                let outcome = self.apply_operator(op, span, editor, journal);
                self.register_linewise = matches!(motion, 'd' | 'c' | 'y');
                Some(outcome)
            }
            (None, c) if self.pending.starts_with('g') => {
                self.pending.clear();
                if c == 'g' {
                    editor.set(editor.text().to_owned(), 0);
                    return Some(VimOutcome::Handled);
                }
                None
            }
            _ => None,
        }
    }

    fn normal(&mut self, c: char, editor: &mut Editor, journal: bool) -> VimOutcome {
        match c {
            'i' => {
                self.pending.clear();
                self.mode = VimMode::Insert;
                VimOutcome::Handled
            }
            'a' => {
                self.pending.clear();
                self.mode = VimMode::Insert;
                let at = super::input::next_boundary(editor.text(), editor.cursor());
                editor.set(editor.text().to_owned(), at);
                VimOutcome::Handled
            }
            'v' | 'V' => {
                self.pending.clear();
                self.anchor = editor.cursor();
                self.mode = if c == 'v' { VimMode::Visual } else { VimMode::VisualLine };
                VimOutcome::Handled
            }
            'h' | 'j' | 'k' | 'l' | '0' | '$' | '^' | 'w' | 'b' | 'e' | 'G' => {
                let count = self.count();
                self.pending.clear();
                if let Some(at) = self.motion(c, editor, count) {
                    editor.set(editor.text().to_owned(), at);
                }
                VimOutcome::Handled
            }
            'g' => {
                self.pending.push('g');
                VimOutcome::Handled
            }
            'x' | 'D' | 'C' => {
                let count = self.count();
                self.pending.clear();
                match self.command_span(c, editor, count) {
                    Some(span) => self.apply_operator(c, span, editor, journal),
                    None => VimOutcome::Handled,
                }
            }
            'd' | 'c' | 'y' => {
                if self.pending.ends_with(c) {
                    // `dd`, `cc`, `yy` act on whole lines; `yy` alone only yanks.
                    let count = self.count();
                    self.pending.clear();
                    let span = self.line_span(editor, count);
                    let outcome = self.apply_operator(c, span, editor, journal);
                    self.register_linewise = true;
                    return outcome;
                }
                self.pending.push(c);
                VimOutcome::Handled
            }
            'p' | 'P' => {
                let count = self.count().unwrap_or(1);
                self.pending.clear();
                if !self.register.is_empty() {
                    self.journal(editor, journal);
                    let buffer = editor.text();
                    let line = Editor::line_span(buffer, Editor::line_of(buffer, editor.cursor()));
                    // Linewise text goes above (`P`) or below (`p`) the cursor's line, never inside it.
                    let (at, text, cursor) = if self.register_linewise {
                        let body = self.register.trim_end_matches('\n');
                        if c == 'P' {
                            (line.start, format!("{body}\n").repeat(count), line.start)
                        } else if line.end < buffer.len() {
                            (line.end + 1, format!("{body}\n").repeat(count), line.end + 1)
                        } else {
                            (line.end, format!("\n{body}").repeat(count), line.end + 1)
                        }
                    } else {
                        let at = if c == 'p' { super::input::next_boundary(buffer, editor.cursor()) } else { editor.cursor() };
                        (at, self.register.repeat(count), at)
                    };
                    let mut buffer = buffer.to_owned();
                    buffer.insert_str(at.min(buffer.len()), &text);
                    editor.set(buffer, cursor);
                }
                VimOutcome::Handled
            }
            'u' => {
                let count = self.count().unwrap_or(1);
                self.pending.clear();
                for _ in 0..count {
                    let Some((text, cursor)) = self.undo.pop() else { break };
                    editor.set(text, cursor);
                }
                VimOutcome::Handled
            }
            _ => {
                self.pending.clear();
                VimOutcome::Handled
            }
        }
    }

    fn visual(&mut self, c: char, editor: &mut Editor) -> VimOutcome {
        match c {
            'y' | 'd' | 'c' => {
                let start = self.anchor.min(editor.text().len());
                let span = Range::new(start.min(editor.cursor()), start.max(super::input::next_boundary(editor.text(), editor.cursor())));
                self.mode = VimMode::Normal;
                self.pending.clear();
                if c == 'y' {
                    self.register = editor.text()[span.start..span.end].to_owned();
                    self.register_linewise = false;
                    let at = span.start;
                    editor.set(editor.text().to_owned(), at);
                    return VimOutcome::Handled;
                }
                if c == 'c' {
                    self.mode = VimMode::Insert;
                }
                // The journal entry precedes the change; visual deletes are undoable.
                let mut buf = editor.text().to_owned();
                self.undo.push((buf.clone(), editor.cursor()));
                buf.replace_range(span.start..span.end, "");
                editor.set(buf, span.start);
                VimOutcome::Handled
            }
            'h' | 'j' | 'k' | 'l' | '0' | '$' | '^' | 'w' | 'b' | 'e' | 'G' => {
                if let Some(at) = self.motion(c, editor, self.count()) {
                    editor.set(editor.text().to_owned(), at);
                }
                VimOutcome::Handled
            }
            'v' | 'V' | 'g' => {
                self.mode = match (self.mode, c) {
                    (VimMode::Visual, 'v') | (VimMode::VisualLine, 'V') => VimMode::Normal,
                    (_, 'V') => VimMode::VisualLine,
                    (_, 'v') => VimMode::Visual,
                    _ => self.mode,
                };
                VimOutcome::Handled
            }
            _ => VimOutcome::Handled,
        }
    }

    fn count(&self) -> Option<usize> {
        let digits: String = self.pending.chars().filter(char::is_ascii_digit).collect();
        digits.parse().ok().filter(|n: &usize| *n > 0)
    }

    fn operator(&self) -> Option<char> {
        self.pending.chars().find(|c| matches!(c, 'd' | 'c' | 'y'))
    }


    fn journal(&mut self, editor: &Editor, record: bool) {
        if record {
            self.undo.push((editor.text().to_owned(), editor.cursor()));
            if self.undo.len() > 100 {
                self.undo.remove(0);
            }
        }
    }

    /// Cursor target for a motion, clamped to the buffer.
    fn motion(&self, c: char, editor: &Editor, count: Option<usize>) -> Option<usize> {
        let text = editor.text();
        let mut at = editor.cursor();
        let times = count.unwrap_or(1);
        match c {
            'h' => for _ in 0..times { at = super::input::prev_boundary(text, at); },
            'l' => for _ in 0..times { at = super::input::next_boundary(text, at).min(text.len()); },
            '0' => at = Editor::line_span(text, Editor::line_of(text, at)).start,
            '^' => {
                let line = Editor::line_span(text, Editor::line_of(text, at));
                at = line.start + text[line.clone()].len() - text[line].trim_start().len();
            }
            '$' => {
                let line = Editor::line_span(text, Editor::line_of(text, at));
                at = line.end;
            }
            'j' | 'k' => {
                let lines = Editor::lines(text);
                let line = Editor::line_of(text, at) as isize + if c == 'j' { times as isize } else { -(times as isize) };
                let line = line.clamp(0, lines.len().saturating_sub(1) as isize) as usize;
                let column = at - Editor::line_span(text, Editor::line_of(text, at)).start;
                let span = Editor::line_span(text, line);
                at = (span.start + column).min(span.end);
            }
            'w' => for _ in 0..times { at = word_forward(text, at); },
            'b' => for _ in 0..times { at = word_back(text, at); },
            'e' => for _ in 0..times { at = word_end(text, at); },
            'G' => {
                let total = Editor::lines(text).len();
                let line = count.unwrap_or(total).min(total).max(1) - 1;
                at = Editor::line_span(text, line).start;
            }
            _ => return None,
        }
        Some(at.min(text.len()))
    }

    /// Span an operator applies to the next motion, including text objects (`iw`, `a(`, `ip`).
    fn motion_span(&self, motion: char, editor: &Editor, count: Option<usize>) -> Option<Range> {
        let text = editor.text();
        let at = editor.cursor();
        if let Some(object) = self.pending.chars().last().filter(|c| matches!(c, 'i' | 'a')) {
            let inner = object == 'i';
            if let Some(span) = text_object(text, at, motion, inner) {
                return Some(span);
            }
            return None;
        }
        // `dd` and friends arrive here as the operator char repeating.
        if matches!(motion, 'd' | 'c' | 'y') {
            return Some(self.line_span(editor, count));
        }
        let target = self.motion(motion, editor, count)?;
        let (from, to) = (at.min(target), at.max(target));
        Some(match motion {
            // Inclusive motions take the character under the cursor.
            '$' | 'e' | 'l' => Range::new(from, super::input::next_boundary(text, to)),
            _ => Range::new(from, to),
        })
    }

    /// Span `x`, `D` and `C` cover.
    fn command_span(&self, c: char, editor: &Editor, count: Option<usize>) -> Option<Range> {
        let text = editor.text();
        let at = editor.cursor();
        let times = count.unwrap_or(1);
        Some(match c {
            'x' => {
                let mut end = at;
                for _ in 0..times {
                    end = super::input::next_boundary(text, end);
                }
                Range::new(at, end)
            }
            'D' => Range::new(at, Editor::line_span(text, Editor::line_of(text, at)).end),
            'C' => Range::new(at, Editor::line_span(text, Editor::line_of(text, at)).end),
            _ => return None,
        })
    }

    fn line_span(&self, editor: &Editor, count: Option<usize>) -> Range {
        let text = editor.text();
        let first = Editor::line_of(text, editor.cursor());
        let last = (first + count.unwrap_or(1) - 1).min(Editor::lines(text).len().saturating_sub(1));
        let start = Editor::line_span(text, first).start;
        let mut end = Editor::line_span(text, last).end;
        if end < text.len() {
            end += 1; // the trailing newline moves with the lines
        }
        Range::new(start, end)
    }

    fn apply_operator(&mut self, op: char, span: Range, editor: &mut Editor, record: bool) -> VimOutcome {
        let mut buffer = editor.text().to_owned();
        let span = Range::new(span.start.min(buffer.len()), span.end.min(buffer.len()));
        if span.start == span.end && op != 'p' {
            self.pending.clear();
            return VimOutcome::Handled;
        }
        self.journal(editor, record);
        self.register = buffer[span.start..span.end].to_owned();
        // Callers that act on whole lines mark the register linewise afterwards.
        self.register_linewise = false;
        match op {
            'y' => {
                editor.set(buffer, span.start);
            }
            'c' => {
                self.mode = VimMode::Insert;
                buffer.replace_range(span.start..span.end, "");
                editor.set(buffer, span.start);
            }
            _ => {
                buffer.replace_range(span.start..span.end, "");
                editor.set(buffer, span.start);
            }
        }
        self.pending.clear();
        VimOutcome::Handled
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Range {
    pub start: usize,
    pub end: usize,
}

impl Range {
    fn new(start: usize, end: usize) -> Self {
        Self { start, end }
    }
}

/// Vim's word classes: blank, keyword characters, and runs of other punctuation.
fn char_class(c: char) -> u8 {
    if c.is_whitespace() {
        0
    } else if c.is_alphanumeric() || c == '_' {
        1
    } else {
        2
    }
}

/// `w`: past the rest of the current word (or punctuation run), then past blanks.
fn word_forward(text: &str, at: usize) -> usize {
    let at = at.min(text.len());
    let mut chars = text[at..].char_indices().map(|(i, c)| (at + i, char_class(c))).peekable();
    if let Some(&(_, first)) = chars.peek().filter(|&&(_, class)| class != 0) {
        while chars.next_if(|&(_, class)| class == first).is_some() {}
    }
    while chars.next_if(|&(_, class)| class == 0).is_some() {}
    chars.peek().map_or(text.len(), |&(i, _)| i)
}

fn word_back(text: &str, at: usize) -> usize {
    let mut i = at;
    let step = |i: usize| text[..i].chars().next_back().map_or(0, |c| i - c.len_utf8());
    while i > 0 && !text[..i].chars().next_back().is_some_and(|c| c.is_alphanumeric() || c == '_') { i = step(i); }
    while i > 0 && text[..i].chars().next_back().is_some_and(|c| c.is_alphanumeric() || c == '_') { i = step(i); }
    i
}

fn word_end(text: &str, at: usize) -> usize {
    let mut i = super::input::next_boundary(text, at);
    while i < text.len() && text[i..].chars().next().is_some_and(|c| c.is_whitespace()) { i = super::input::next_boundary(text, i); }
    while i < text.len() {
        let next = super::input::next_boundary(text, i);
        if !text[i..next].chars().all(|c| c.is_alphanumeric() || c == '_') { break; }
        i = next;
    }
    super::input::prev_boundary(text, i)
}

/// `iw`, `aw`, `i(`, `a(`, `i"`, `ip`, `ap` around the cursor.
fn text_object(text: &str, at: usize, object: char, inner: bool) -> Option<Range> {
    match object {
        'w' => {
            // The run of one character class under the cursor; `aw` also takes the blanks after it.
            let here = char_class(text[at..].chars().next()?);
            let start = text[..at].char_indices().rev().take_while(|&(_, c)| char_class(c) == here).last().map_or(at, |(i, _)| i);
            let mut end = text[at..].char_indices().find(|&(_, c)| char_class(c) != here).map_or(text.len(), |(i, _)| at + i);
            if !inner {
                end = text[end..].char_indices().find(|&(_, c)| !c.is_whitespace()).map_or(text.len(), |(i, _)| end + i);
            }
            Some(Range::new(start, end))
        }
        '(' | ')' | '[' | ']' | '{' | '}' | '<' | '>' | '"' | '\'' | '`' => {
            let (open, close) = match object {
                '(' | ')' => ('(', ')'),
                '[' | ']' => ('[', ']'),
                '{' | '}' => ('{', '}'),
                '<' | '>' => ('<', '>'),
                '"' => ('"', '"'),
                '\'' => ('\'', '\''),
                _ => ('`', '`'),
            };
            // The cursor may sit on the opening delimiter itself.
            let start = text[..super::input::next_boundary(text, at)].rfind(open)?;
            let after = start + open.len_utf8();
            let end = after + text[after..].find(close)?;
            Some(if inner { Range::new(start + open.len_utf8(), end) } else { Range::new(start, end + close.len_utf8()) })
        }
        'p' => {
            let first = Editor::line_of(text, at);
            let last = if inner { first } else { first + 1 };
            let start = Editor::line_span(text, first).start;
            let end = Editor::line_span(text, last.min(Editor::lines(text).len().saturating_sub(1))).end;
            Some(Range::new(start, end))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vim() -> (Vim, Editor) {
        (Vim::new(true), Editor::default())
    }

    fn press(vim: &mut Vim, editor: &mut Editor, keys: &str) {
        for c in keys.chars() {
            // Insert mode passes plain keys to the composer, which types them.
            if vim.key(KeyEvent::from(KeyCode::Char(c)), editor, true) == VimOutcome::Pass {
                editor.insert(&c.to_string());
            }
        }
    }

    fn press_key(vim: &mut Vim, editor: &mut Editor, code: KeyCode) {
        vim.key(KeyEvent::from(code), editor, true);
    }

    #[test]
    fn starts_in_normal_and_insert_types_at_the_cursor() {
        let (mut vim, mut editor) = vim();
        editor.replace("hello world".into());
        press_key(&mut vim, &mut editor, KeyCode::Esc);
        assert_eq!(vim.mode, VimMode::Normal);
        press(&mut vim, &mut editor, "0w");
        assert_eq!(editor.cursor(), 6);
        press(&mut vim, &mut editor, "i");
        assert_eq!(vim.mode, VimMode::Insert);
        press(&mut vim, &mut editor, "X");
        assert_eq!(editor.text(), "hello Xworld");
        press_key(&mut vim, &mut editor, KeyCode::Esc);
        assert_eq!(vim.mode, VimMode::Normal);
        assert_eq!(editor.cursor(), 6, "Escape steps back onto the last typed character");
    }

    #[test]
    fn counts_operators_and_text_objects_edit_like_vim() {
        let (mut vim, mut editor) = vim();
        editor.replace("one two three four".into());
        press_key(&mut vim, &mut editor, KeyCode::Esc);
        press(&mut vim, &mut editor, "02w");
        assert!(editor.text()[editor.cursor()..].starts_with("three"));
        press(&mut vim, &mut editor, "dw");
        assert_eq!(editor.text(), "one two four");
        press(&mut vim, &mut editor, "2x");
        assert_eq!(editor.text(), "one two ur");
        press(&mut vim, &mut editor, "0");
        press(&mut vim, &mut editor, "D");
        assert_eq!(editor.text(), "");
        editor.replace("say (this is) fine".into());
        press(&mut vim, &mut editor, "0");
        press(&mut vim, &mut editor, "f");
        assert_eq!(vim.mode, VimMode::Normal, "unknown keys stay in normal mode");
        press(&mut vim, &mut editor, "w");
        press(&mut vim, &mut editor, "di(");
        assert_eq!(editor.text(), "say () fine");
        press(&mut vim, &mut editor, "da(");
        assert_eq!(editor.text(), "say  fine");
    }

    #[test]
    fn linewise_yank_delete_paste_and_undo_round_trip() {
        let (mut vim, mut editor) = vim();
        editor.replace("alpha\nbeta\ngamma".into());
        press_key(&mut vim, &mut editor, KeyCode::Esc);
        press(&mut vim, &mut editor, "ggyy");
        assert_eq!(editor.text(), "alpha\nbeta\ngamma");
        press(&mut vim, &mut editor, "jdd");
        assert_eq!(editor.text(), "alpha\ngamma");
        press(&mut vim, &mut editor, "p");
        assert_eq!(editor.text(), "alpha\ngamma\nbeta", "a linewise put lands below the last line");
        press(&mut vim, &mut editor, "u");
        assert_eq!(editor.text(), "alpha\ngamma");
        press(&mut vim, &mut editor, "0G");
        assert_eq!(Editor::line_of(&editor.text().clone(), editor.cursor()), 1, "G with no count goes to the last line");
    }

    #[test]
    fn visual_selections_yank_and_delete() {
        let (mut vim, mut editor) = vim();
        editor.replace("keep this drop that".into());
        press_key(&mut vim, &mut editor, KeyCode::Esc);
        press(&mut vim, &mut editor, "0wvwww");
        assert_eq!(vim.mode, VimMode::Visual);
        press(&mut vim, &mut editor, "y");
        assert_eq!(vim.mode, VimMode::Normal);
        press(&mut vim, &mut editor, "$v0d");
        assert_eq!(editor.text(), "");
        press(&mut vim, &mut editor, "P");
        assert!(editor.text().contains("this"));
    }

    #[test]
    fn mode_label_respects_the_display_setting() {
        use greencode_core::config::VimModeDisplay;
        assert_eq!(VimMode::Normal.label(VimModeDisplay::Text), Some("-- NORMAL --"));
        assert_eq!(VimMode::Normal.label(VimModeDisplay::Icon), Some("N"));
        assert_eq!(VimMode::Insert.label(VimModeDisplay::None), None);
        assert_eq!(Vim::new(true).mode_label(VimModeDisplay::Icon), Some("N"));
        assert_eq!(Vim::new(false).mode, VimMode::Insert);
    }
}
