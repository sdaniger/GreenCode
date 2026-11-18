use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use std::{borrow::Cow, ops::Range};
use unicode_segmentation::{GraphemeCursor, UnicodeSegmentation};
use unicode_width::UnicodeWidthStr;

/// Never pass terminal controls from tool results, pasted text, or model output to the backend.
pub(super) fn plain(text: &str) -> Cow<'_, str> {
    if !text.chars().any(|c| c.is_control() && c != '\n') {
        return Cow::Borrowed(text);
    }
    let mut clean = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\r' if chars.peek() == Some(&'\n') => (),
            '\r' | '\n' => clean.push('\n'),
            '\t' => clean.push_str("    "),
            c if !c.is_control() => clean.push(c),
            _ => (),
        }
    }
    Cow::Owned(clean)
}

/// Byte ranges into display text, always aligned on grapheme boundaries.
pub(super) fn rows(text: &str, width: u16, out: &mut Vec<Range<usize>>) {
    out.clear();
    let width = usize::from(width.max(1));
    let (mut start, mut columns) = (0, 0);
    for (at, grapheme) in text.grapheme_indices(true) {
        if grapheme == "\n" {
            out.push(start..at);
            start = at + 1;
            columns = 0;
            continue;
        }
        let n = grapheme.width();
        if columns != 0 && columns + n > width {
            out.push(start..at);
            start = at;
            columns = 0;
        }
        columns += n;
    }
    out.push(start..text.len());
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Edit {
    Up, Down, Left, Right, WordLeft, WordRight, LineStart, LineEnd,
    Backspace, Delete, DeleteWordLeft, DeleteWordRight, DeleteToStart, DeleteToEnd,
}

#[derive(Default)]
pub(super) struct Editor {
    text: String,
    cursor: usize,
}

impl Editor {
    pub fn text(&self) -> &str { &self.text }
    pub fn cursor(&self) -> usize { self.cursor }

    pub fn replace(&mut self, text: String) {
        self.text = match plain(&text) { Cow::Borrowed(_) => text, Cow::Owned(clean) => clean };
        self.cursor = self.text.len();
    }

    pub fn take(&mut self) -> String {
        self.cursor = 0;
        std::mem::take(&mut self.text)
    }

    pub fn insert(&mut self, text: &str) {
        let text = plain(text);
        self.text.insert_str(self.cursor, &text);
        self.align_cursor(self.cursor + text.len());
    }

    fn prev(&self) -> usize {
        self.text[..self.cursor].grapheme_indices(true).next_back().map_or(0, |(at, _)| at)
    }

    fn next(&self) -> usize {
        self.cursor + self.text[self.cursor..].graphemes(true).next().map_or(0, str::len)
    }

    fn align_cursor(&mut self, at: usize) {
        // Pasting or deleting can join a cluster across the splice.
        let mut cursor = GraphemeCursor::new(at, self.text.len(), true);
        self.cursor = if cursor.is_boundary(&self.text, 0).unwrap() {
            at
        } else {
            cursor.next_boundary(&self.text, 0).unwrap().unwrap_or(self.text.len())
        };
    }

    fn word(&self, left: bool) -> usize {
        fn distance<'a>(graphemes: impl Iterator<Item = &'a str>) -> usize {
            let mut kind = None;
            let mut bytes = 0;
            for g in graphemes {
                let first = g.chars().next().unwrap();
                if first.is_whitespace() {
                    if kind.is_some() { break; }
                } else {
                    let word = first.is_alphanumeric() || first == '_';
                    if *kind.get_or_insert(word) != word { break; }
                }
                bytes += g.len();
            }
            bytes
        }
        if left { self.cursor - distance(self.text[..self.cursor].graphemes(true).rev()) }
        else { self.cursor + distance(self.text[self.cursor..].graphemes(true)) }
    }

    fn line_start(&self) -> usize { self.text[..self.cursor].rfind('\n').map_or(0, |i| i + 1) }
    fn line_end(&self) -> usize { self.text[self.cursor..].find('\n').map_or(self.text.len(), |i| self.cursor + i) }

    fn vertical(&mut self, up: bool) {
        let start = self.line_start();
        let end = self.line_end();
        let target = if up && start > 0 {
            let end = start - 1;
            self.text[..end].rfind('\n').map_or(0, |i| i + 1)..end
        } else if !up && end < self.text.len() {
            let start = end + 1;
            start..self.text[start..].find('\n').map_or(self.text.len(), |i| start + i)
        } else { return };
        let mut column = self.text[start..self.cursor].width();
        self.cursor = target.start;
        for g in self.text[target].graphemes(true) {
            let Some(remaining) = column.checked_sub(g.width()) else { break };
            column = remaining;
            self.cursor += g.len();
        }
    }

    pub fn key(&mut self, key: KeyEvent, action: Option<Edit>) -> bool {
        use Edit::*;
        let range = match action {
            Some(Left) => { self.cursor = self.prev(); return false; }
            Some(Right) => { self.cursor = self.next(); return false; }
            Some(WordLeft) => { self.cursor = self.word(true); return false; }
            Some(WordRight) => { self.cursor = self.word(false); return false; }
            Some(LineStart) => { self.cursor = self.line_start(); return false; }
            Some(LineEnd) => { self.cursor = self.line_end(); return false; }
            Some(Up | Down) => { self.vertical(action == Some(Up)); return false; }
            Some(Backspace) => self.prev()..self.cursor,
            Some(Delete) => self.cursor..self.next(),
            Some(DeleteWordLeft) => self.word(true)..self.cursor,
            Some(DeleteWordRight) => self.cursor..self.word(false),
            Some(DeleteToStart) => self.line_start()..self.cursor,
            Some(DeleteToEnd) => self.cursor..self.line_end(),
            None => {
                let modified = key.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER);
                if let (false, KeyCode::Char(c)) = (modified, key.code) {
                    self.insert(c.encode_utf8(&mut [0; 4]));
                    return true;
                }
                return false;
            }
        };
        if range.is_empty() { return false; }
        let at = range.start;
        self.text.replace_range(range, "");
        self.align_cursor(at);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paste_and_edit_keep_clusters_and_remove_terminal_controls() {
        let mut e = Editor::default();
        e.insert("A\r\n日本👩‍💻\x1b\x07");
        e.key(KeyEvent::from(KeyCode::Backspace), Some(Edit::Backspace));
        e.key(KeyEvent::from(KeyCode::Left), Some(Edit::Left));
        e.insert("語");
        assert_eq!(e.text(), "A\n日語本");
        let mut lines = Vec::new();
        rows(e.text(), 4, &mut lines);
        assert_eq!(lines.iter().map(|r| &e.text()[r.clone()]).collect::<Vec<_>>(), ["A", "日語", "本"]);
    }

    #[test]
    fn deleting_a_line_break_does_not_leave_the_cursor_inside_a_cluster() {
        let mut e = Editor::default();
        e.replace("a\n\u{0301}b".into());
        e.key(KeyEvent::from(KeyCode::Home), Some(Edit::LineStart));
        e.key(KeyEvent::from(KeyCode::Backspace), Some(Edit::Backspace));
        e.insert("x");
        assert_eq!(e.text(), "a\u{0301}xb");
    }
}
