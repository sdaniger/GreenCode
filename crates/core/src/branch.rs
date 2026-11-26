//! `branchSummary.enabled`: when a thread is forked, the branch being left gets a summary in
//! `handoff-<stamp>.md` beside the session, so the new thread starts from a written recap.
use anyhow::{Context, Result};
use std::path::PathBuf;

use crate::session::{Meta, Session};

/// Summarizes the transcript of `id` and saves it as `handoff-<stamp>.md` next to that session.
/// The summary is produced without a model call: the sessions directory is meant to stay readable
/// even when the provider is unreachable, so the recap is the transcript's own structure.
pub fn write(id: &str) -> Result<PathBuf> {
    let (meta, messages) = Session::load(id)?;
    let document = recap(&meta, &messages);
    let path = crate::home().join("sessions").join(format!("handoff-{}.md", stamp()));
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, document).with_context(|| path.display().to_string())?;
    Ok(path)
}

/// A stable, filesystem-safe timestamp for the summary file name.
fn stamp() -> String {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
    format!("{}", now.as_millis())
}

/// The recap itself: goal, files touched, decisions and the last state, straight from the transcript.
pub fn recap(meta: &Meta, messages: &[crate::Message]) -> String {
    let mut out = format!("# Branch summary: {}\n\nModel: {}\nWorking directory: {}\n", meta.title, meta.model, meta.cwd.display());
    if let Some(first) = messages.iter().find(|m| m.role == crate::Role::User) {
        out.push_str("\n## First request\n");
        out.push_str(&text(first));
        out.push('\n');
    }
    let mut files = Vec::new();
    let mut commands = Vec::new();
    for message in messages {
        for block in &message.content {
            if let crate::Block::ToolUse { name, input, .. } = block {
                match name.as_str() {
                    "read" | "edit" | "write" => {
                        if let Some(path) = input["path"].as_str() {
                            let path = path.to_string();
                            if !files.contains(&path) {
                                files.push(path);
                            }
                        }
                    }
                    "bash" => {
                        if let Some(command) = input["command"].as_str() {
                            if commands.len() < 20 {
                                commands.push(command.lines().next().unwrap_or("").to_string());
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
    }
    if !files.is_empty() {
        out.push_str("\n## Files touched\n");
        for file in files {
            out.push_str(&format!("- {file}\n"));
        }
    }
    if !commands.is_empty() {
        out.push_str("\n## Commands run\n");
        for command in commands {
            out.push_str(&format!("- `{command}`\n"));
        }
    }
    if let Some(last) = messages.iter().rev().find(|m| m.role == crate::Role::Assistant) {
        out.push_str("\n## Last state\n");
        out.push_str(&text(last));
        out.push('\n');
    }
    out.push_str(&format!("\nMessages carried to the fork: {}\n", messages.len()));
    out
}

fn text(message: &crate::Message) -> String {
    let text = message
        .content
        .iter()
        .filter_map(|block| match block {
            crate::Block::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    text.chars().take(2_000).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn meta() -> Meta {
        Meta::new(std::path::PathBuf::from("/work"), "anthropic/claude-sonnet-5-5".into(), "native".into())
    }

    #[test]
    fn recap_lists_the_goal_files_and_last_state() {
        let messages = vec![
            crate::Message::user("add a settings tab"),
            crate::Message {
                role: crate::Role::Assistant,
                content: vec![
                    crate::Block::ToolUse { id: "1".into(), name: "read".into(), input: json!({"path": "src/workspace.rs"}) },
                    crate::Block::ToolUse { id: "2".into(), name: "bash".into(), input: json!({"command": "cargo test -p greencode-app"}) },
                    crate::Block::Text { text: "Added the tab.".into() },
                ],
            },
        ];
        let text = recap(&meta(), &messages);
        assert!(text.contains("add a settings tab"));
        assert!(text.contains("- src/workspace.rs"));
        assert!(text.contains("cargo test -p greencode-app"));
        assert!(text.contains("Added the tab."));
        assert!(text.contains("Messages carried to the fork: 2"));
    }
}
