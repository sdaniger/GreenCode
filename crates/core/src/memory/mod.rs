//! Memory backends: one trait the agent loop talks to, and the dispatch that picks the backend from
//! `memory.backend`. Hindsight (remote HTTP) and Sharpshooter (project decisions) live here as modules;
//! the local summary pipeline and Mnemopi are separate backends wired into `backend()`.
use crate::config::{memory::MemoryBackend, Config};
use crate::{BoxFut, Message, Role};
use anyhow::Result;
use std::{path::Path, sync::Arc};

pub mod local;
pub mod mnemopi;
pub mod remote;
pub mod sharpshooter;

/// One recalled memory, as a backend found it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Hit {
    pub text: String,
    /// Backend-specific kind (for example Hindsight's `world`), rendered in brackets.
    pub source: Option<String>,
    /// Relevance the backend reported, if any.
    pub score: Option<f64>,
}

/// Automatic recall asks for at most this many memories.
pub const RECALL_LIMIT: usize = 8;

/// A memory backend for one session. Recall and retain are asynchronous; retain is called off the turn.
pub trait Backend: Send + Sync {
    /// Developer instructions for this session (mental models, project decisions, `MEMORY.md`); empty when none.
    fn instructions<'a>(&'a self) -> BoxFut<'a, Result<String>>;

    /// Automatic recall for the first turn of a session, newest/most relevant first. Empty when the
    /// backend's auto-recall setting is off. `query` is the first prompt plus a little prior context.
    fn recall<'a>(&'a self, query: &'a str, limit: usize) -> BoxFut<'a, Result<Vec<Hit>>>;

    /// The text `retain` sends for this conversation; `None` when nothing is worth keeping.
    fn transcript(&self, messages: &[Message]) -> Option<String> {
        transcript(messages)
    }

    /// Persist `text` for `session`, so a backend may upsert one document per session.
    fn retain<'a>(&'a self, session: &'a str, text: &'a str) -> BoxFut<'a, Result<()>>;

    /// Whether the turn just completed should be retained (`turns` counts completed user turns).
    fn retain_due(&self, _turns: u32) -> bool {
        true
    }

    /// Runs one `memory` tool operation against this backend's bank; `None` = the `MEMORY.md` files.
    fn tool<'a>(&'a self, _input: &'a serde_json::Value) -> Option<BoxFut<'a, Result<String>>> {
        None
    }
}

/// The session's backend from `memory.backend`; `None` when memory is off.
pub fn backend(cfg: &Config, cwd: &Path) -> Option<Arc<dyn Backend>> {
    match cfg.memory_opts.memory.backend {
        MemoryBackend::Off => None,
        MemoryBackend::Local => Some(Arc::new(local::Local::new(cwd))),
        MemoryBackend::Mnemopi => Some(Arc::new(mnemopi::Mnemopi::new(cfg, cwd))),
        MemoryBackend::Hindsight => remote::Hindsight::new(cfg, cwd).map(|b| Arc::new(b) as Arc<dyn Backend>),
        MemoryBackend::Sharpshooter => sharpshooter::Sharpshooter::new(cfg, cwd).map(|b| Arc::new(b) as Arc<dyn Backend>),
    }
}

/// Text blocks of a message, one per line.
fn text_of(message: &Message) -> String {
    message
        .content
        .iter()
        .filter_map(|block| match block {
            crate::Block::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn has_word(text: &str) -> bool {
    text.chars().any(char::is_alphanumeric)
}

/// Removes a block previously injected into the prompt, so recall never feeds itself back.
fn strip_block(text: &str, open: &str, close: &str) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some(start) = rest.find(open) {
        out.push_str(&rest[..start]);
        match rest[start..].find(close) {
            Some(end) => rest = &rest[start + end + close.len()..],
            None => return out,
        }
    }
    out.push_str(rest);
    out
}

fn clean(text: &str) -> String {
    let text = strip_block(text, "<memories>", "</memories>");
    strip_block(&text, "<mental_models>", "</mental_models>").trim().to_string()
}

/// The conversation as role-delimited blocks, skip-able when it has no words; `None` under 10 characters.
pub fn transcript(messages: &[Message]) -> Option<String> {
    let blocks: Vec<String> = messages
        .iter()
        .filter_map(|message| {
            let text = clean(&text_of(message));
            if !has_word(&text) {
                return None;
            }
            let role = if message.role == Role::User { "user" } else { "assistant" };
            Some(format!("[role: {role}]\n{text}\n[{role}:end]"))
        })
        .collect();
    let text = blocks.join("\n");
    (text.trim().len() >= 10).then_some(text)
}

/// The first prompt plus the last exchange as context, capped near 800 characters like OMP's default.
pub fn recall_query(prompt: &str, messages: &[Message]) -> String {
    const MAX: usize = 800;
    let prompt = clean(prompt);
    if prompt.chars().count() >= MAX {
        return prompt.chars().take(MAX).collect();
    }
    let mut prior = Vec::new();
    for message in messages.iter().rev().take(4).rev() {
        let text = clean(&text_of(message));
        if !has_word(&text) {
            continue;
        }
        let role = if message.role == Role::User { "user" } else { "assistant" };
        prior.push(format!("{role}: {}", text.replace('\n', "\n  ")));
    }
    if prior.is_empty() {
        return prompt;
    }
    let query = format!("Prior context:\n{}\n{prompt}", prior.join("\n"));
    if query.chars().count() > MAX {
        return prompt;
    }
    query
}

/// The `<memories>` block injected before the first prompt.
pub fn recall_block(hits: &[Hit]) -> String {
    let lines: Vec<String> = hits
        .iter()
        .filter(|hit| has_word(&hit.text))
        .map(|hit| match &hit.source {
            Some(source) if !source.is_empty() => format!("- {} [{source}]", hit.text.trim()),
            _ => format!("- {}", hit.text.trim()),
        })
        .collect();
    if lines.is_empty() {
        return String::new();
    }
    format!(
        "<memories>\n{}\n\n{}\n</memories>",
        lines.join("\n"),
        remote::PREAMBLE
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Block;

    fn assistant(text: &str) -> Message {
        Message { role: Role::Assistant, content: vec![Block::Text { text: text.into() }] }
    }

    #[test]
    fn transcript_skips_tool_traffic_and_needs_words() {
        let messages = vec![
            Message::user("<memories>\n- old\n</memories>\n\nfix the login bug"),
            Message { role: Role::User, content: vec![Block::ToolResult { tool_use_id: "1".into(), content: "ok".into(), is_error: false }] },
            assistant("Done: the session token is refreshed now."),
        ];
        let text = transcript(&messages).unwrap();
        assert!(text.starts_with("[role: user]\nfix the login bug\n[user:end]"), "{text}");
        assert!(text.contains("[assistant:end]") && text.contains("session token"));
        assert!(!text.contains("<memories>") && !text.contains("old"));
        assert_eq!(transcript(&[Message::user("  ")]) , None);
        // Like OMP, a short framed message clears the 10-character gate; only wordless text is dropped.
        assert_eq!(transcript(&[Message::user("... ?!")]), None);
        assert!(transcript(&[Message::user("hi")]).is_some());
    }

    #[test]
    fn recall_query_adds_prior_context_but_prefers_a_long_prompt() {
        let messages = vec![Message::user("the build fails on CI"), assistant("try pnpm install")];
        let query = recall_query("and now the tests time out", &messages);
        assert_eq!(query, "Prior context:\nuser: the build fails on CI\nassistant: try pnpm install\nand now the tests time out");
        let long = "x".repeat(1200);
        assert_eq!(recall_query(&long, &messages).chars().count(), 800);
        assert_eq!(recall_query("hello", &[]), "hello");
    }

    #[test]
    fn recall_block_renders_sources_and_skips_blank_hits() {
        let hits = vec![
            Hit { text: "use pnpm".into(), source: Some("world".into()), score: Some(0.9) },
            Hit { text: " ".into(), source: None, score: None },
        ];
        let block = recall_block(&hits);
        assert!(block.starts_with("<memories>\n- use pnpm [world]\n"));
        assert!(block.ends_with("</memories>") && block.contains("Relevant memories"));
        assert!(recall_block(&[]).is_empty());
    }

    #[test]
    fn dispatch_picks_the_backend_and_its_memory_tool() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = Config::default();
        assert_eq!(cfg.memory_opts.memory.backend, MemoryBackend::Local);
        let local = backend(&cfg, dir.path()).unwrap();
        assert!(local.tool(&serde_json::json!({"action": "add"})).is_none(), "Local writes MEMORY.md");

        cfg.memory_opts.memory.backend = MemoryBackend::Mnemopi;
        cfg.memory_opts.mnemopi.db_path = Some(dir.path().join("m.db").to_string_lossy().into());
        let mnemopi = backend(&cfg, dir.path()).unwrap();
        assert!(mnemopi.tool(&serde_json::json!({"action": "add"})).is_some(), "Mnemopi owns the tool");

        cfg.memory_opts.memory.backend = MemoryBackend::Off;
        assert!(backend(&cfg, dir.path()).is_none());
    }
}
