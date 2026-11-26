//! `compaction.experimentalContextManagement`: persistent notes plus a searchable raw history that
//! outlive a compacted context window. Files live next to the session they belong to.
use anyhow::{Context, Result};
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::{Block, Message, Role};

/// Notes are capped so an unbounded session cannot grow them without limit.
pub const NOTES_LIMIT: usize = 16 * 1024;
/// Characters of each raw message kept in `history.jsonl`; the full text stays in the session log.
const RAW_LIMIT: usize = 4_000;

/// `<home>/sessions/<id>` — beside the session's own `.jsonl`.
pub fn dir(id: &str) -> Result<PathBuf> {
    crate::session::check_id(id)?;
    Ok(crate::home().join("sessions").join(id))
}

pub fn notes_path(id: &str) -> Result<PathBuf> {
    Ok(dir(id)?.join("notes.md"))
}

pub fn history_path(id: &str) -> Result<PathBuf> {
    Ok(dir(id)?.join("history.jsonl"))
}

/// Appends `document` to the notes, dropping the oldest lines past `NOTES_LIMIT`.
pub fn append_notes(path: &Path, document: &str) -> Result<()> {
    let mut text = std::fs::read_to_string(path).unwrap_or_default();
    text.push_str(&format!("\n## Summary\n{}\n", document.trim()));
    if text.len() > NOTES_LIMIT {
        // Keep the newest entries whole: cut at a section boundary, then at a line boundary.
        let cut = text.len() - NOTES_LIMIT;
        let start = text[cut..].find("\n## ").map(|i| cut + i + 1).or_else(|| text[cut..].find('\n').map(|i| cut + i + 1));
        if let Some(start) = start {
            text = text[start..].to_string();
        }
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, text).with_context(|| path.display().to_string())
}

/// Appends one JSON line per message so `grep` can search the raw history across windows.
pub fn append_history(path: &Path, messages: &[Message]) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(path)
        .with_context(|| path.display().to_string())?;
    for message in messages {
        let line = serde_json::json!({
            "role": match message.role { Role::User => "user", Role::Assistant => "assistant" },
            "text": text_of(message),
        });
        writeln!(f, "{line}")?;
    }
    Ok(())
}

/// Flat text of one message, bounded so one huge tool result cannot dominate the history file.
fn text_of(message: &Message) -> String {
    let mut out = String::new();
    for block in &message.content {
        match block {
            Block::Text { text } => out.push_str(text),
            Block::ToolUse { name, input, .. } => out.push_str(&format!("\n@call:{name}({input})")),
            Block::ToolResult { content, .. } => out.push_str(&format!("\n<out>{content}</out>")),
            Block::Other(_) => {}
        }
        if out.chars().count() >= RAW_LIMIT {
            break;
        }
    }
    out.chars().take(RAW_LIMIT).collect()
}

/// Notes and raw history for one compaction: `notes.md` grows by `document`, `history.jsonl` gains
/// the messages being replaced. Returns the notes path.
pub fn record(id: &str, document: &str, replaced: &[Message]) -> Result<PathBuf> {
    let notes = notes_path(id)?;
    append_notes(&notes, document)?;
    append_history(&history_path(id)?, replaced)?;
    Ok(notes)
}

/// Reads the notes back for the context block that survives a compaction.
pub fn notes(id: &str) -> Option<String> {
    let text = std::fs::read_to_string(notes_path(id).ok()?).ok()?;
    let trimmed = text.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

/// The `history://` virtual path a resumed session can read or grep.
pub fn history_uri(id: &str) -> String {
    format!("history://{id}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A temporary `GREENCODE_HOME`, restored on drop.
    struct Home(std::path::PathBuf);

    impl Home {
        fn new() -> Self {
            let dir = std::env::temp_dir().join(format!("greencode-notebook-{}-{:?}", std::process::id(), std::thread::current().id()));
            std::fs::create_dir_all(&dir).unwrap();
            crate::set_test_home(Some(&dir));
            Self(dir)
        }
    }

    impl Drop for Home {
        fn drop(&mut self) {
            crate::set_test_home(None);
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn notes_and_history_accumulate_per_session() {
        let _home = Home::new();
        let id = "n1";
        let replaced = vec![
            Message::user("first request"),
            Message { role: Role::Assistant, content: vec![Block::ToolResult { tool_use_id: "1".into(), content: "x".repeat(RAW_LIMIT + 100), is_error: false }] },
        ];
        let path = record(id, "goal: ship it", &replaced).unwrap();
        record(id, "goal: still shipping", &replaced).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("goal: ship it") && text.contains("goal: still shipping"));
        assert_eq!(notes(id).as_deref(), Some(text.trim()));
        let history = std::fs::read_to_string(history_path(id).unwrap()).unwrap();
        let lines: Vec<_> = history.lines().collect();
        assert_eq!(lines.len(), 4);
        let first: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(first["role"], json!("user"));
        assert_eq!(first["text"], json!("first request"));
        assert!(lines[1].len() < RAW_LIMIT * 2, "one message is bounded");
        assert!(history_uri(id).starts_with("history://"));
    }

    #[test]
    fn notes_keep_the_newest_entries_under_the_cap() {
        let notes = std::env::temp_dir().join(format!("greencode-notes-{}.md", std::process::id()));
        let _ = std::fs::remove_file(&notes);
        append_notes(&notes, &"old ".repeat(NOTES_LIMIT)).unwrap();
        append_notes(&notes, "the newest summary").unwrap();
        let text = std::fs::read_to_string(&notes).unwrap();
        assert!(text.len() <= NOTES_LIMIT);
        assert!(text.contains("the newest summary"));
        let _ = std::fs::remove_file(&notes);
    }
}
