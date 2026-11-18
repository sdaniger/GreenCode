//! "Suggest next prompt": one cheap extra request that predicts what the user will type next.

use crate::{config::Config, provider::{self, Request}, Block, Message, Role};
use anyhow::Result;

const SYSTEM: &str = "Predict the user's next message to this coding agent. Reply with that message only, one line, in the user's language, at most 120 characters. Reply with nothing if the task looks finished.";

/// The last `limit` messages reduced to their text, starting with a user message.
/// Tool blocks are dropped to keep the request cheap; messages left empty vanish and neighbours of one role merge.
fn recent_text(messages: &[Message], limit: usize) -> Vec<Message> {
    let tail = &messages[messages.len().saturating_sub(limit)..];
    let mut out: Vec<Message> = vec![];
    for m in tail {
        let text: Vec<&str> = m.content.iter().filter_map(|b| if let Block::Text { text } = b { Some(text.as_str()) } else { None }).filter(|t| !t.trim().is_empty()).collect();
        if text.is_empty() || (out.is_empty() && m.role != Role::User) {
            continue;
        }
        let text = text.join("\n");
        match out.last_mut() {
            Some(last) if last.role == m.role => last.content.push(Block::Text { text }),
            _ => out.push(Message { role: m.role, content: vec![Block::Text { text }] }),
        }
    }
    out
}

/// A guess at the user's next message: the first line of the reply, trimmed. Empty when the task looks finished.
pub async fn suggest_next(cfg: &Config, model: &str, messages: &[Message]) -> Result<String> {
    let messages = recent_text(messages, 12);
    if messages.is_empty() {
        return Ok(String::new());
    }
    let (provider, model) = provider::make(cfg, model)?;
    let req = Request { model: &model, system: SYSTEM, messages: &messages, tools: &[], max_tokens: 200 };
    let reply = provider.complete(req, &|_| {}).await?;
    let text: String = reply.blocks.iter().filter_map(|b| if let Block::Text { text } = b { Some(text.as_str()) } else { None }).collect();
    Ok(text.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("").to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn text(m: &Message) -> String {
        m.content.iter().map(|b| if let Block::Text { text } = b { text.clone() } else { String::new() }).collect::<Vec<_>>().join("|")
    }

    #[test]
    fn recent_text_drops_tools_and_starts_with_user() {
        let call = Block::ToolUse { id: "1".into(), name: "read".into(), input: json!({}) };
        let result = Block::ToolResult { tool_use_id: "1".into(), content: "x".into(), is_error: false };
        let messages = vec![
            Message { role: Role::Assistant, content: vec![Block::Text { text: "orphan".into() }] },
            Message::user("fix it"),
            Message { role: Role::Assistant, content: vec![call] },
            Message { role: Role::User, content: vec![result] },
            Message { role: Role::Assistant, content: vec![Block::Text { text: "done".into() }] },
        ];
        let kept = recent_text(&messages, 12);
        assert_eq!(kept.iter().map(|m| (m.role, text(m))).collect::<Vec<_>>(), [(Role::User, "fix it".into()), (Role::Assistant, "done".into())]);
        // Only the last messages count; a window that begins with an assistant message skips it.
        let kept = recent_text(&messages, 1);
        assert!(kept.is_empty());
        let many: Vec<Message> = (0..30).map(|i| if i % 2 == 0 { Message::user(format!("u{i}")) } else { Message { role: Role::Assistant, content: vec![Block::Text { text: format!("a{i}") }] } }).collect();
        let kept = recent_text(&many, 12);
        assert_eq!((kept.len(), text(&kept[0])), (12, "u18".into()));
    }
}
