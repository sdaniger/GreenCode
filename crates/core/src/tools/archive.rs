//! `archive`: read-only history over GreenCode's own sessions — prompt history, recent projects, past sessions
//! and recaps. Never writes, never resumes.
use crate::{Block, Message, Role};
use anyhow::{bail, Context, Result};
use serde_json::Value;
use std::path::{Path, PathBuf};

fn sessions_dir(home: &Path) -> PathBuf {
    home.join("sessions")
}

/// Every stored thread, newest first, read straight from the first line of each session file.
fn metas(home: &Path) -> Vec<Value> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(sessions_dir(home)).into_iter().flatten().flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|e| e != "jsonl") {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else { continue };
        let Some(first) = text.lines().next() else { continue };
        let Ok(v) = serde_json::from_str::<Value>(first) else { continue };
        let Some(meta) = v.get("meta") else { continue };
        out.push(serde_json::json!({
            "id": path.file_stem().unwrap_or_default().to_string_lossy(),
            "title": meta["title"].as_str().unwrap_or(""),
            "cwd": meta["cwd"].as_str().unwrap_or(""),
            "model": meta["model"].as_str().unwrap_or(""),
            "created": meta["created"].as_u64().unwrap_or(0),
        }));
    }
    out.sort_by_key(|m| std::cmp::Reverse(m["created"].as_u64().unwrap_or(0)));
    out
}

fn messages(home: &Path, id: &str) -> Result<Vec<Message>> {
    if id.is_empty() || !id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
        bail!("bad session id `{id}`");
    }
    let path = sessions_dir(home).join(format!("{id}.jsonl"));
    let text = std::fs::read_to_string(&path).with_context(|| format!("no session `{id}`"))?;
    Ok(text.lines().filter_map(|line| serde_json::from_str::<Message>(line).ok()).collect())
}

fn role(message: &Message) -> &'static str {
    match message.role {
        Role::User => "user",
        Role::Assistant => "assistant",
    }
}

fn text_of(message: &Message) -> String {
    message
        .content
        .iter()
        .filter_map(|block| match block {
            Block::Text { text } => Some(text.clone()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn snippet(text: &str, query: &str) -> Option<String> {
    let at = text.to_lowercase().find(&query.to_lowercase())?;
    let start = text[..at].char_indices().rev().nth(60).map_or(0, |(i, _)| i);
    let end = text[at..].char_indices().nth(160).map_or(text.len(), |(i, _)| at + i);
    Some(format!("…{}…", text[start..end].replace('\n', " ")))
}

/// The operations behind the `archive` ext tool. `limit` defaults to 20 and is capped at 200.
pub fn run(home: &Path, args: &Value) -> Result<String> {
    let limit = args["limit"].as_u64().unwrap_or(20).clamp(1, 200) as usize;
    let query = args["query"].as_str().map(str::trim).filter(|q| !q.is_empty());
    match args["action"].as_str().context("archive needs an `action`")? {
        "prompts" => {
            let mut out = Vec::new();
            for meta in metas(home).iter().take(200) {
                let id = meta["id"].as_str().unwrap_or_default();
                let Ok(list) = messages(home, id) else { continue };
                for message in list.iter().filter(|m| m.role == Role::User) {
                    let text = text_of(message);
                    if text.starts_with("<") || text.trim().is_empty() {
                        continue;
                    }
                    if let Some(query) = query {
                        if !text.to_lowercase().contains(&query.to_lowercase()) {
                            continue;
                        }
                    }
                    out.push(format!("{}  {}\n  {}", meta["created"], id, text.replace('\n', " ").chars().take(300).collect::<String>()));
                    if out.len() >= limit {
                        break;
                    }
                }
                if out.len() >= limit {
                    break;
                }
            }
            Ok(if out.is_empty() { "no prompts found".into() } else { out.join("\n") })
        }
        "projects" => {
            let mut counts: std::collections::BTreeMap<String, (usize, u64)> = Default::default();
            for meta in metas(home) {
                let entry = counts.entry(meta["cwd"].as_str().unwrap_or("").to_string()).or_default();
                entry.0 += 1;
                entry.1 = entry.1.max(meta["created"].as_u64().unwrap_or(0));
            }
            let mut rows: Vec<_> = counts.into_iter().collect();
            rows.sort_by_key(|(_, (_, created))| std::cmp::Reverse(*created));
            Ok(rows
                .into_iter()
                .take(limit)
                .map(|(cwd, (count, created))| format!("{cwd}\t{count} session(s)\tlast {created}"))
                .collect::<Vec<_>>()
                .join("\n"))
        }
        "sessions" => {
            let rows: Vec<String> = metas(home)
                .iter()
                .filter(|m| {
                    query.is_none_or(|q| {
                        let hay = format!("{} {} {}", m["title"].as_str().unwrap_or(""), m["cwd"].as_str().unwrap_or(""), m["id"].as_str().unwrap_or("")).to_lowercase();
                        hay.contains(&q.to_lowercase())
                    })
                })
                .take(limit)
                .map(|m| format!("{}\t{}\t{}\t{}", m["id"].as_str().unwrap_or(""), m["created"], m["cwd"].as_str().unwrap_or(""), m["title"].as_str().unwrap_or("(untitled)")))
                .collect();
            Ok(if rows.is_empty() { "no sessions found".into() } else { rows.join("\n") })
        }
        "search" => {
            let query = query.context("search needs a `query`")?;
            let mut out = Vec::new();
            for meta in metas(home).iter().take(200) {
                let id = meta["id"].as_str().unwrap_or_default();
                let Ok(list) = messages(home, id) else { continue };
                for message in &list {
                    let text = text_of(message);
                    if let Some(line) = snippet(&text, query) {
                        out.push(format!("{id}  {}  {}\n  {line}", meta["created"], role(message)));
                        break;
                    }
                }
                if out.len() >= limit {
                    break;
                }
            }
            Ok(if out.is_empty() { format!("no session matches `{query}`") } else { out.join("\n") })
        }
        "recap" => {
            let id = args["session"].as_str().context("recap needs a `session` id")?;
            let turns = args["turns"].as_u64().unwrap_or(6).clamp(1, 50) as usize;
            let list = messages(home, id)?;
            let mut picked: Vec<&Message> = list.iter().filter(|m| !text_of(m).trim().is_empty()).collect();
            picked = picked.split_off(picked.len().saturating_sub(turns * 2));
            Ok(format!(
                "session {id}: {} message(s)\n{}",
                list.len(),
                picked
                    .iter()
                    .map(|m| format!("{}: {}", role(m), text_of(m).chars().take(400).collect::<String>().replace('\n', " ")))
                    .collect::<Vec<_>>()
                    .join("\n")
            ))
        }
        other => bail!("unknown archive action `{other}`; use prompts, projects, sessions, search or recap"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn seed(home: &Path) {
        let dir = sessions_dir(home);
        std::fs::create_dir_all(&dir).unwrap();
        let meta = json!({"meta": {"id": "aaa", "cwd": "/tmp/proj", "title": "Fix parser", "model": "openai/gpt-5", "backend": "native", "created": 100}});
        std::fs::write(
            dir.join("aaa.jsonl"),
            format!(
                "{}\n{}\n{}\n",
                meta,
                json!({"role": "user", "content": [{"type": "text", "text": "fix the parser bug"}]}),
                json!({"role": "assistant", "content": [{"type": "text", "text": "I will look at lexer.rs"}]})
            ),
        )
        .unwrap();
        std::fs::write(
            dir.join("bbb.jsonl"),
            format!(
                "{}\n{}\n",
                json!({"meta": {"id": "bbb", "cwd": "/tmp/proj2", "title": "", "model": "x", "backend": "native", "created": 200}}),
                json!({"role": "user", "content": [{"type": "text", "text": "write docs"}]})
            ),
        )
        .unwrap();
    }

    #[test]
    fn reads_prompts_projects_sessions_search_and_recap() {
        let home = tempfile::tempdir().unwrap();
        seed(home.path());
        let prompts = run(home.path(), &json!({"action": "prompts"})).unwrap();
        assert!(prompts.contains("write docs") && prompts.contains("fix the parser bug"), "{prompts}");
        assert!(prompts.find("write docs").unwrap() < prompts.find("fix the parser bug").unwrap());
        assert!(run(home.path(), &json!({"action": "prompts", "query": "docs"})).unwrap().contains("write docs"));
        let projects = run(home.path(), &json!({"action": "projects"})).unwrap();
        assert!(projects.contains("/tmp/proj") && projects.contains("/tmp/proj2"), "{projects}");
        let sessions = run(home.path(), &json!({"action": "sessions", "query": "parser"})).unwrap();
        assert!(sessions.starts_with("aaa") && sessions.contains("Fix parser"), "{sessions}");
        let search = run(home.path(), &json!({"action": "search", "query": "lexer"})).unwrap();
        assert!(search.contains("aaa") && search.contains("lexer.rs"), "{search}");
        let recap = run(home.path(), &json!({"action": "recap", "session": "aaa"})).unwrap();
        assert!(recap.contains("fix the parser bug") && recap.contains("lexer.rs"), "{recap}");
        assert!(run(home.path(), &json!({"action": "recap", "session": "../x"})).is_err());
        assert!(run(home.path(), &json!({"action": "nope"})).is_err());
    }
}
