//! `edit.magic`, `edit.blackbox`, `edit.auto_repair` and the inline-payload recovery used by the agent.
use crate::{config::Config, Block};
use anyhow::Result;
use serde_json::Value;
use std::path::Path;

/// The model that writes magic edits: `edit.magic.model` (selector or `@role`), else the `smol` role.
pub fn magic_model(cfg: &Config) -> String {
    cfg.file_opts
        .edit
        .magic
        .model
        .as_deref()
        .map(str::trim)
        .filter(|m| !m.is_empty())
        .map(|m| crate::assist::resolve_target(cfg, m).unwrap_or_else(|| m.to_string()))
        .unwrap_or_else(|| cfg.role_or_default("smol").to_string())
}

fn strip_fence(text: &str) -> &str {
    let text = text.trim();
    let Some(rest) = text.strip_prefix("```") else { return text };
    let rest = rest.split_once('\n').map_or("", |(_, body)| body);
    rest.strip_suffix("```").unwrap_or(rest).trim_end_matches('\n')
}

/// `edit.mode = magic`: the fast model writes the whole new file for `change`.
pub async fn magic(cfg: &Config, path: &Path, before: &str, change: &str) -> Result<String> {
    let model = magic_model(cfg);
    let prompt = format!(
        "File: {}\n\nChange to make:\n{change}\n\nCurrent content:\n{before}",
        path.display()
    );
    let text = crate::textgen::generate(
        cfg,
        &model,
        "Apply the requested change to the file. Reply with the complete new file content only, no prose and no fences.",
        &prompt,
        8000,
    )
    .await?;
    Ok(strip_fence(&text).to_string())
}

/// The parse oracle: JSON through `serde_json`; Rust through `rustfmt --check` when installed; other code
/// extensions through bracket balance that skips strings and comments. Unknown files are not checked.
/// Returns a short reason when the text does not parse.
pub fn parse_error(path: &Path, text: &str) -> Option<String> {
    match path.extension().and_then(|e| e.to_str()).unwrap_or("") {
        "json" => serde_json::from_str::<Value>(text).err().map(|e| format!("JSON: {e}")),
        "rs" => {
            let rustfmt = crate::config::executable("rustfmt")?;
            // A file that was already unparsable is not this edit's regression.
            let mut child = std::process::Command::new(rustfmt)
                .args(["--check", "--edition", "2021"])
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .ok()?;
            use std::io::Write as _;
            child.stdin.take()?.write_all(text.as_bytes()).ok()?;
            let out = child.wait_with_output().ok()?;
            let err = String::from_utf8_lossy(&out.stderr);
            // rustfmt exits non-zero for formatting differences too; only syntax errors print `error:`.
            (out.status.code() != Some(0) && err.contains("error"))
                .then(|| err.lines().find(|l| l.contains("error")).unwrap_or("parse error").trim().to_string())
        }
        "go" | "ts" | "tsx" | "js" | "jsx" | "py" | "c" | "h" | "cc" | "cpp" | "hpp" | "java"
        | "swift" | "kt" | "rb" | "sh" | "bash" => balance_error(text),
        _ => None,
    }
}

/// Unbalanced `{}`, `[]` or `()`, ignoring strings, chars, and line/block comments.
fn balance_error(text: &str) -> Option<String> {
    let mut stack: Vec<char> = Vec::new();
    let mut chars = text.chars().peekable();
    let mut quote: Option<char> = None;
    while let Some(c) = chars.next() {
        match quote {
            Some(q) => {
                if c == '\\' {
                    chars.next();
                } else if c == q {
                    quote = None;
                }
            }
            None => match c {
                '"' | '\'' | '`' => quote = Some(c),
                '/' if chars.peek() == Some(&'/') => {
                    for c in chars.by_ref() {
                        if c == '\n' {
                            break;
                        }
                    }
                }
                '/' if chars.peek() == Some(&'*') => {
                    chars.next();
                    while let Some(c) = chars.next() {
                        if c == '*' && chars.peek() == Some(&'/') {
                            chars.next();
                            break;
                        }
                    }
                }
                '{' | '[' | '(' => stack.push(c),
                '}' | ']' | ')' => {
                    let open = match c {
                        '}' => '{',
                        ']' => '[',
                        _ => '(',
                    };
                    if stack.pop() != Some(open) {
                        return Some(format!("unbalanced `{c}`"));
                    }
                }
                _ => {}
            },
        }
    }
    if let Some(open) = stack.last() {
        return Some(format!("unclosed `{open}`"));
    }
    if quote.is_some() {
        return Some("unterminated string".into());
    }
    None
}

/// Appends one JSON line per regression under `home/blackbox/edits.jsonl`.
fn blackbox_record(home: &Path, path: &Path, error: &str, before: &str, after: &str, input: &str) -> Result<()> {
    let dir = home.join("blackbox");
    std::fs::create_dir_all(&dir)?;
    let line = serde_json::json!({
        "at": std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis()),
        "path": path,
        "error": error,
        "input": input,
        "before": before,
        "after": after,
    });
    use std::io::Write as _;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("edits.jsonl"))?;
    writeln!(file, "{line}")?;
    Ok(())
}

/// One smol-model repair of the broken text; `None` when the model is unavailable.
async fn repair(cfg: &Config, path: &Path, broken: &str, error: &str) -> Option<String> {
    let model = cfg.role_or_default("smol").to_string();
    let text = crate::textgen::generate(
        cfg,
        &model,
        "Repair the syntax error in this file. Reply with the complete corrected file content only, no prose and no fences.",
        &format!("File: {}\nParse error: {error}\n\n{broken}", path.display()),
        8000,
    )
    .await
    .ok()?;
    Some(strip_fence(&text).to_string())
}

/// `edit.blackbox.enabled` / `edit.auto_repair.enabled`: called after `edit`/`write` changed `path`.
/// `None` when the new text parses; otherwise the warning appended to the tool result.
pub async fn after_change(
    cfg: &Config,
    home: &Path,
    path: &Path,
    before: &str,
    input: &Value,
) -> Option<String> {
    let edit = &cfg.file_opts.edit;
    if !edit.blackbox.enabled && !edit.auto_repair.enabled {
        return None;
    }
    let after = std::fs::read_to_string(path).ok()?;
    let error = parse_error(path, &after)?;
    // Only a regression counts: a file that did not parse before the edit is not blamed on it.
    if parse_error(path, before).is_some() {
        return None;
    }
    let mut warning = format!("warning: {} no longer parses ({error})", path.display());
    if edit.blackbox.enabled {
        match blackbox_record(home, path, &error, before, &after, &input.to_string()) {
            Ok(()) => warning.push_str(&format!("; recorded in {}", home.join("blackbox/edits.jsonl").display())),
            Err(e) => warning.push_str(&format!("; could not record it: {e:#}")),
        }
    }
    if edit.auto_repair.enabled {
        match repair(cfg, path, &after, &error).await {
            Some(fixed) if parse_error(path, &fixed).is_none() => {
                match std::fs::write(path, &fixed) {
                    Ok(()) => warning.push_str("; a smol-model repair was applied after it re-parsed"),
                    Err(e) => warning.push_str(&format!("; a repair parsed but could not be written: {e:#}")),
                }
            }
            Some(_) => warning.push_str("; the repair still did not parse, so the edit stands"),
            None => warning.push_str("; no repair model answered, so the edit stands"),
        }
    }
    Some(warning)
}

/// `edit.recoverInlineEdits`: an edit payload the model wrote as a fenced block becomes a real `edit` call.
/// Finds fenced ```json/```edit blocks holding `{"path","old_string","new_string"}`, appends the calls to
/// `blocks`, and leaves the text as the model wrote it.
pub fn recover_inline(cfg: &Config, blocks: &mut Vec<Block>) {
    if !cfg.file_opts.edit.recover_inline_edits || cfg.no_tools {
        return;
    }
    let mut found = Vec::new();
    for block in blocks.iter() {
        let Block::Text { text } = block else { continue };
        let mut rest = text.as_str();
        while let Some(start) = rest.find("```") {
            let body = &rest[start + 3..];
            let Some((tag, body)) = body.split_once('\n') else { break };
            let Some(end) = body.find("```") else { break };
            rest = &body[end + 3..];
            if !matches!(tag.trim(), "json" | "edit" | "jsonc") {
                continue;
            }
            let Ok(Value::Object(map)) = serde_json::from_str::<Value>(body[..end].trim()) else { continue };
            let has = |k: &str| map.get(k).and_then(Value::as_str).is_some();
            if has("path") && has("old_string") && has("new_string") {
                found.push(Value::Object(map));
            }
        }
    }
    for (i, input) in found.into_iter().enumerate() {
        blocks.push(Block::ToolUse {
            id: format!("recovered-inline-{i}"),
            name: "edit".into(),
            input,
        });
    }
}

/// `edit.streamingAbort`: while a streamed `edit` call's arguments are incomplete JSON, say whether they are
/// already known to fail (the file is missing, the anchor absent, or the anchor ambiguous). Providers call this
/// on every argument delta and stop reading the stream on `Err`.
pub fn stream_edit_error(cwd: &Path, json: &str) -> Option<String> {
    // An incomplete string is expected mid-stream; a complete object is not.
    let Ok(Value::Object(map)) = serde_json::from_str::<Value>(json) else { return None };
    let (Some(path), Some(old)) = (
        map.get("path").and_then(Value::as_str),
        map.get("old_string").and_then(Value::as_str),
    ) else {
        return None;
    };
    let path = crate::permission::resolve(cwd, path);
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Some(format!("old preview: {} cannot be read", path.display()));
    };
    let hits = text.match_indices(old).count();
    match (hits, map.get("replace_all") == Some(&Value::Bool(true))) {
        (0, _) => Some(format!("old preview: old_string is not in {}", path.display())),
        (n, false) if n > 1 => Some(format!("old preview: old_string matches {n} times")),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::files::EditCfg;

    fn cfg() -> Config {
        let mut cfg = Config::default();
        cfg.learning.auto = false;
        cfg.file_opts.edit = EditCfg { blackbox: crate::config::files::BlackboxCfg { enabled: true }, ..Default::default() };
        cfg
    }

    #[test]
    fn magic_model_resolves_the_configured_selector_and_role() {
        let mut cfg = Config::default();
        cfg.model = "main/model".into();
        assert_eq!(magic_model(&cfg), "main/model");
        cfg.model_roles.insert("smol".into(), "fast/model".into());
        assert_eq!(magic_model(&cfg), "fast/model");
        cfg.file_opts.edit.magic.model = Some("@smol".into());
        assert_eq!(magic_model(&cfg), "fast/model");
        cfg.file_opts.edit.magic.model = Some("picked/model".into());
        assert_eq!(magic_model(&cfg), "picked/model");
    }

    #[test]
    fn balance_oracle_ignores_strings_and_comments() {
        assert!(balance_error("fn a() { let s = \"}\"; } // (\n").is_none());
        assert!(balance_error("# a\n```\nx\n```\n").is_none());
        assert!(balance_error("fn a() {").unwrap().contains("unclosed"));
        assert!(balance_error("fn a() }").unwrap().contains("unbalanced"));
        assert!(balance_error("\"abc").unwrap().contains("unterminated"));
        assert!(parse_error(Path::new("a.txt"), "{{{").is_none());
        assert!(parse_error(Path::new("a.json"), "{").is_some());
        assert!(parse_error(Path::new("a.json"), "{}").is_none());
    }

    #[test]
    fn inline_payloads_become_edit_calls_once() {
        let mut blocks = vec![Block::Text {
            text: "I will edit.\n```json\n{\"path\":\"a.rs\",\"old_string\":\"1\",\"new_string\":\"2\"}\n```\n".into(),
        }];
        recover_inline(&cfg(), &mut blocks);
        assert!(matches!(&blocks[1], Block::ToolUse { name, input, .. } if name == "edit" && input["new_string"] == "2"));
        // A payload without all three keys is ignored, and the flag turns recovery off.
        let mut none = vec![Block::Text { text: "```json\n{\"path\":\"a.rs\"}\n```".into() }];
        recover_inline(&cfg(), &mut none);
        assert_eq!(none.len(), 1);
        let mut off = cfg();
        off.file_opts.edit.recover_inline_edits = false;
        let mut blocks = vec![Block::Text {
            text: "```json\n{\"path\":\"a.rs\",\"old_string\":\"1\",\"new_string\":\"2\"}\n```".into(),
        }];
        recover_inline(&off, &mut blocks);
        assert_eq!(blocks.len(), 1);
    }

    #[test]
    fn stream_preview_reports_failures_before_the_full_reply() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "one\ntwo\n").unwrap();
        assert!(stream_edit_error(dir.path(), "").is_none());
        assert!(stream_edit_error(dir.path(), "{\"path\":\"a.txt\"").is_none());
        assert!(stream_edit_error(dir.path(), "{\"path\":\"a.txt\",\"old_string\":\"three\"}").unwrap().contains("not in"));
        assert!(stream_edit_error(dir.path(), "{\"path\":\"a.txt\",\"old_string\":\"one\"}").is_none());
        assert!(stream_edit_error(dir.path(), "{\"path\":\"missing.txt\",\"old_string\":\"x\"}").unwrap().contains("cannot be read"));
    }

    #[tokio::test]
    async fn blackbox_records_only_regressions_and_repair_never_accepts_broken_text() {
        let home = tempfile::tempdir().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.go");
        std::fs::write(&path, "package a\n\nfunc f() {\n}\n").unwrap();
        // A clean edit records nothing.
        assert!(after_change(&cfg(), home.path(), &path, "package a\n", &serde_json::json!({})).await.is_none());
        // A regression appends before/after to the capture file.
        std::fs::write(&path, "package a\n\nfunc f() {\n").unwrap();
        let warning = after_change(&cfg(), home.path(), &path, "package a\n", &serde_json::json!({"path":"a.go"})).await.unwrap();
        assert!(warning.contains("no longer parses"), "{warning}");
        let captured = std::fs::read_to_string(home.path().join("blackbox/edits.jsonl")).unwrap();
        let line: Value = serde_json::from_str(captured.lines().next().unwrap()).unwrap();
        assert_eq!(line["after"], "package a\n\nfunc f() {\n");
        assert_eq!(line["before"], "package a\n");
        // A file that was already broken is not recorded as a regression.
        std::fs::write(&path, "package a\n").unwrap();
        let before = "package a\n\nfunc f() {\n";
        std::fs::write(&path, before).unwrap();
        assert!(after_change(&cfg(), home.path(), &path, before, &serde_json::json!({})).await.is_none());
    }
}
