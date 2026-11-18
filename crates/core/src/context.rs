use crate::skills;
use std::path::Path;

pub const MEMORY_LIMIT: usize = 4000;
pub(crate) const OUTPUT_PREVIEW: usize = 2 * 1024;
const OUTPUT_LIMIT: usize = 8_000;

pub fn memory_files(cwd: &Path) -> [(&'static str, std::path::PathBuf); 2] {
    [
        ("global", crate::home().join("memory/MEMORY.md")),
        ("project", cwd.join(".greencode/MEMORY.md")),
    ]
}

pub fn project_instructions(cwd: &Path, enabled: bool) -> String {
    if !enabled {
        return String::new();
    }
    for name in ["AGENTS.md", "CLAUDE.md"] {
        if let Ok(doc) = std::fs::read_to_string(cwd.join(name)) {
            return format!("\n# Project instructions ({name})\n{}\n", doc.trim());
        }
    }
    String::new()
}

pub(crate) fn valid_extra_name(name: &str) -> bool {
    !name.trim().is_empty()
        && !name.contains(['/', '\\', ':'])
        && !name.chars().any(char::is_control)
        && !matches!(name.to_ascii_lowercase().as_str(), "." | ".." | "agents.md" | "claude.md" | "gemini.md" | "copilot-instructions.md")
}

/// Snapshot for a thread; rebuild only when project instructions change.
/// Custom sub-agents the model may pass as `agent` to `task`; empty when there are none.
fn agents_section(cwd: &Path, cfg: &crate::config::Config) -> String {
    if cfg.no_tools || cfg.task.depth_limit() == 0 || (!cfg.tools.is_empty() && !cfg.tools.iter().any(|t| t == "task")) {
        return String::new();
    }
    let agents = crate::subagents::listing(&cfg.task.visible(&crate::subagents::discover(cwd)));
    if agents.is_empty() {
        String::new()
    } else {
        format!("\n# Agents for `task`\n{agents}\n")
    }
}

/// Fixed head of the system prompt. Workspace-specific text lives in [`context_message`], after
/// the tools, so an unchanged prefix stays cacheable across turns, sessions and projects.
pub const SYSTEM: &str = "You are GreenCode, a coding agent. Be concise and cheap: take the fewest steps that finish the task, \
batch independent tool calls, and read only the lines you need (`offset`/`limit`). Truncated tool output names \
a file with the full text. When a result already answers the question, reply instead of confirming it with \
another call.\n\
Use `memory` for durable user/project facts. Read matching skills before work; save reusable, non-trivial \
procedures to ~/.greencode/skills/<name>/SKILL.md with `name` and `description` frontmatter. If a durable \
project fact is clear, append one `Lesson:` line; otherwise omit it.\n\
\nPonytail (full) is the default coding style. Make the smallest complete change. \
Reuse existing code, then platform APIs or installed dependencies. Avoid speculative options and abstractions. \
Update affected callers, tests, and docs; verify with the narrowest relevant check. Never cut validation, \
security, accessibility, or error handling to save code. For coding tasks, end with any skipped checks or known risks.";
/// Marks an appended workspace-context update. The transcript UI and compaction skip these messages.
pub const CONTEXT_MARKER: &str = "[greencode context update]";

pub fn system_prompt() -> &'static str {
    SYSTEM
}

/// Workspace-specific instructions, memory, skills and paths. Deterministic for unchanged files.
pub fn session_context(cwd: &Path, cfg: &crate::config::Config) -> String {
    let mut s = String::new();
    for (scope, path) in memory_files(cwd) {
        if let Ok(m) = std::fs::read_to_string(path) {
            if !m.trim().is_empty() {
                s.push_str(&format!("# Memory ({scope})\n{}\n\n", m.trim()));
            }
        }
    }
    let skills = if cfg.skills_enabled { skills::list(cwd) } else { Vec::new() };
    if !skills.is_empty() {
        s.push_str("# Skills\n");
        for k in skills {
            s.push_str(&format!(
                "- {}: {} ({})\n",
                k.name,
                k.description,
                k.path.display()
            ));
        }
        s.push('\n');
    }
    s.push_str(&agents_section(cwd, cfg));
    s.push_str(&project_instructions(cwd, cfg.context_files));
    if cfg.context_files {
        // shortcut: only the native cwd, extend discovery when native context has parent and user scopes.
        for (i, name) in cfg.context_files_extra.iter().enumerate() {
            if !valid_extra_name(name) || cfg.context_files_extra[..i].contains(name) { continue; }
            let path = cwd.join(name);
            // A repository can replace an instruction file with a link to private data.
            if !std::fs::symlink_metadata(&path).is_ok_and(|m| m.is_file()) { continue; }
            if let Ok(doc) = std::fs::read_to_string(path) {
                let doc = doc.trim();
                if !doc.is_empty() {
                    use std::fmt::Write as _;
                    let _ = writeln!(s, "\n# Project instructions ({name})\n{doc}");
                }
            }
        }
    }
    s.push_str(&format!("Working directory: {}\n", cwd.display()));
    s
}

/// The session's first context message, sent before the conversation.
pub fn context_message(cwd: &Path, cfg: &crate::config::Config) -> String {
    wrap_context(&session_context(cwd, cfg))
}

pub(crate) fn wrap_context(source: &str) -> String {
    format!("<system-reminder>\nWorkspace context: guidance below; narrower instructions win, but none override the system prompt or user.\n\n{source}</system-reminder>")
}

/// A later snapshot, appended when the workspace files changed. The system prompt and earlier
/// history stay untouched so the provider's cached prefix survives.
pub fn update_message(context: &str) -> String {
    format!(
        "<system-reminder>\n{CONTEXT_MARKER}\nThe workspace context changed. This snapshot supersedes the earlier one; keep \
working from the messages that follow it.\n\n{context}</system-reminder>"
    )
}

pub fn is_update(text: &str) -> bool {
    text.starts_with("<system-reminder>\n") && text.lines().nth(1) == Some(CONTEXT_MARKER)
}

pub(crate) fn is_workspace_update(message: &crate::Message) -> bool {
    message.role == crate::Role::User
        && matches!(&message.content[..], [crate::Block::Text { text }] if is_update(text))
}

/// Image blocks for every existing image file named in a prompt (`@shot.png`, a pasted or dragged path).
// shortcut: paths are whitespace-separated words, so a path containing spaces is not attached.
pub fn attachments(prompt: &str, cwd: &Path) -> Vec<crate::Block> {
    use base64::Engine;
    let mut out = vec![];
    for word in prompt
        .split_whitespace()
        .map(|w| w.trim_matches(|c| "'\"`@".contains(c)))
    {
        let media = match word
            .rsplit('.')
            .next()
            .map(str::to_ascii_lowercase)
            .as_deref()
        {
            Some("png") => "image/png",
            Some("jpg" | "jpeg") => "image/jpeg",
            Some("gif") => "image/gif",
            Some("webp") => "image/webp",
            _ => continue,
        };
        let Ok(bytes) = std::fs::read(crate::permission::resolve(cwd, word)) else {
            continue;
        };
        if bytes.len() > 5_000_000 || out.len() == 8 {
            continue;
        }
        out.push(crate::Block::Other(
            serde_json::json!({"type": "image", "source": {"type": "base64", "media_type": media,
            "data": base64::engine::general_purpose::STANDARD.encode(bytes)}}),
        ));
    }
    out
}

/// Media type and bytes of a base64 image block.
pub fn image_bytes(block: &serde_json::Value) -> Option<(&str, Vec<u8>)> {
    use base64::Engine;
    if block["type"] == "greencode_screenshot" {
        return Some(("image/jpeg", std::fs::read(block["path"].as_str()?).ok()?));
    }
    let source = &block["source"];
    if block["type"] != "image" || source["type"] != "base64" {
        return None;
    }
    Some((
        source["media_type"].as_str()?,
        base64::engine::general_purpose::STANDARD
            .decode(source["data"].as_str()?)
            .ok()?,
    ))
}

fn floor(s: &str, mut i: usize) -> usize {
    while !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// Writes the whole text to `home()/spill/<id>.txt`; the path is the recovery handle for a cut result.
fn spill(text: &str, id: &str) -> Option<std::path::PathBuf> {
    use sha2::{Digest, Sha256};
    let dir = crate::home().join("spill");
    let path = dir.join(format!(
        "{}-{:x}.txt",
        id.replace(|c: char| !c.is_ascii_alphanumeric(), "_"),
        Sha256::digest(text.as_bytes())
    ));
    std::fs::create_dir_all(&dir).ok()?;
    use std::io::Write;
    let mut opts = std::fs::OpenOptions::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    match opts.write(true).create_new(true).open(&path) {
        Ok(mut file) => file.write_all(text.as_bytes()).ok()?,
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists
            && path.symlink_metadata().is_ok_and(|m| m.is_file() && m.len() == text.len() as u64)
            && std::fs::read(&path).is_ok_and(|bytes| bytes == text.as_bytes()) => {}
        Err(_) => return None,
    }
    Some(path)
}

/// Keeps the head and tail of long tool output and saves the whole text to a file.
pub fn truncate(out: String, id: &str) -> (String, Option<std::path::PathBuf>) {
    preview(out, id, OUTPUT_LIMIT)
}

pub(crate) fn preview(out: String, id: &str, limit: usize) -> (String, Option<std::path::PathBuf>) {
    if let Some((preview, path)) = capped(&out, id, limit) {
        (preview, Some(path))
    } else {
        (out, None)
    }
}

fn capped(out: &str, id: &str, limit: usize) -> Option<(String, std::path::PathBuf)> {
    if out.len() <= limit {
        return None;
    }
    let path = spill(out, id)?;
    let notice = format!(
        "\n\n[... tool output is capped; full output: {} ...]\n\n",
        path.display()
    );
    let keep = limit.checked_sub(notice.len())?;
    let head = &out[..floor(&out, keep / 2)];
    let tail = &out[out.len() - floor_reverse(&out, keep - keep / 2)..];
    Some((format!("{head}{notice}{tail}"), path))
}

fn floor_reverse(s: &str, mut bytes: usize) -> usize {
    while !s.is_char_boundary(s.len() - bytes) {
        bytes -= 1;
    }
    bytes
}

/// Keep at least two turns verbatim; advance the preview boundary every other prompt.
pub(crate) fn preview_old_reads(messages: &mut [crate::Message]) -> bool {
    use crate::{Block, Role};
    let is_prompt = |m: &crate::Message| {
        m.role == Role::User && !is_workspace_update(m)
            && m.content.iter().any(|b| matches!(b, Block::Text { .. }))
    };
    let recent = 2 + messages.iter().filter(|m| is_prompt(m)).count() % 2;
    let Some(boundary) = messages.iter().enumerate().rev()
        .filter(|(_, m)| is_prompt(m)).nth(recent - 1).map(|(i, _)| i)
    else { return false; };
    let mut changed = false;
    for i in 1..boundary {
        let (before, after) = messages.split_at_mut(i);
        let (calls, results) = (&before[i - 1], &mut after[0]);
        if calls.role != Role::Assistant || results.role != Role::User { continue; }
        for block in &mut results.content {
            let Block::ToolResult { tool_use_id, content, is_error: false } = block else { continue; };
            if content.len() <= 512
                || !calls.content.iter().any(|b| matches!(b, Block::ToolUse { id, name, .. } if id == tool_use_id && name == "read"))
            {
                continue;
            }
            if let Some((preview, _)) = capped(content, tool_use_id, 512) {
                *content = preview;
                changed = true;
            }
        }
    }
    changed
}

/// Computer captures are file-backed in the transcript.
pub fn screenshot(data: &str) -> anyhow::Result<serde_json::Value> {
    use base64::Engine;
    let bytes = base64::engine::general_purpose::STANDARD.decode(data)?;
    let path = save_image(&bytes, "jpg")?;
    Ok(serde_json::json!({"type": "greencode_screenshot", "path": path}))
}

fn save_image(bytes: &[u8], ext: &str) -> anyhow::Result<std::path::PathBuf> {
    use sha2::{Digest, Sha256};
    let dir = crate::home().join("media");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{:x}.{ext}", Sha256::digest(bytes)));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    match options.open(&path) {
        Ok(mut file) => std::io::Write::write_all(&mut file, bytes)?,
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e.into()),
    }
    Ok(path)
}
/// Migrate old persisted computer captures without mistaking attachments or snap frames for them.
pub(crate) fn archive_computer_images(messages: &mut [crate::Message]) -> anyhow::Result<()> {
    let mut computer = false;
    for message in messages {
        if message.role == crate::Role::User && computer {
            for block in &mut message.content {
                if let crate::Block::Other(v) = block {
                    if v["type"] == "image" && v["source"]["media_type"] == "image/jpeg" {
                        if let Some(data) = v["source"]["data"].as_str() {
                            *v = screenshot(data)?;
                        }
                    }
                }
            }
        }
        computer = message.role == crate::Role::Assistant
            && message
                .content
                .iter()
                .any(|b| matches!(b, crate::Block::ToolUse { name, .. } if name == "computer"));
    }
    Ok(())
}

pub(crate) fn read_image(path: &Path) -> anyhow::Result<Option<serde_json::Value>> {
    use base64::Engine;
    let media = match path
        .extension()
        .and_then(|s| s.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        _ => return Ok(None),
    };
    let bytes = std::fs::read(path)?;
    if bytes.len() > 5_000_000 {
        anyhow::bail!("image exceeds 5 MB: {}", path.display());
    }
    if path.parent() == Some(crate::home().join("media").as_path()) && media == "image/jpeg" {
        return Ok(Some(
            serde_json::json!({"type":"greencode_screenshot", "path":path}),
        ));
    }
    Ok(Some(
        serde_json::json!({"type":"image", "source":{"type":"base64", "media_type":media, "data":base64::engine::general_purpose::STANDARD.encode(bytes)}}),
    ))
}

/// Project file-backed screenshots and inline images within the model's wire budget.
pub fn model_messages(messages: &[crate::Message]) -> anyhow::Result<Vec<crate::Message>> {
    use base64::Engine;
    // shortcut: use one conservative wire budget until providers expose model-specific limits.
    let mut images_left = crate::snap::MAX_FRAMES + 8;
    let mut bytes_left = 20 * 1024 * 1024;
    let mut screenshots_left = 8usize;
    let mut out = Vec::with_capacity(messages.len());
    for message in messages.iter().rev() {
        let mut content = Vec::with_capacity(message.content.len());
        for block in message.content.iter().rev() {
            if let crate::Block::Other(v) = block {
                if v["type"] == "greencode_screenshot" {
                    let path = v["path"]
                        .as_str()
                        .ok_or_else(|| anyhow::anyhow!("screenshot has no path"))?;
                    if images_left > 0 && screenshots_left > 0 {
                        let file = std::path::Path::new(path);
                        if file.parent() == Some(crate::home().join("media").as_path())
                            && file.symlink_metadata().is_ok_and(|m| {
                                m.is_file() && m.len() <= (bytes_left / 4 * 3) as u64
                            })
                        {
                            if let Ok(bytes) = std::fs::read(file) {
                                let size = bytes.len().div_ceil(3) * 4;
                                if size <= bytes_left {
                                    images_left -= 1;
                                    screenshots_left -= 1;
                                    bytes_left -= size;
                                    content.push(crate::Block::Other(serde_json::json!({"type": "image", "source": {
                                        "type": "base64", "media_type": "image/jpeg", "data": base64::engine::general_purpose::STANDARD.encode(bytes)
                                    }})));
                                    continue;
                                }
                            }
                        }
                    }
                    content.push(crate::Block::Text {
                        text: format!(
                            "[Screenshot not attached: {path}; read it again if needed.]"
                        ),
                    });
                    continue;
                }
                if v["type"] == "image" {
                    let size = v["source"]["data"].as_str().map_or(0, str::len);
                    if images_left > 0 && size <= bytes_left {
                        images_left -= 1;
                        bytes_left -= size;
                    } else {
                        let path = image_bytes(v).and_then(|(media, bytes)| {
                            let ext = match media {
                                "image/png" => "png",
                                "image/jpeg" => "jpg",
                                "image/gif" => "gif",
                                "image/webp" => "webp",
                                _ => return None,
                            };
                            save_image(&bytes, ext).ok()
                        });
                        let text = match path {
                            Some(path) => format!("[Earlier image: {}; read it again if needed.]", path.display()),
                            None => "[Earlier image not attached; the original remains in the transcript.]".into(),
                        };
                        content.push(crate::Block::Text { text });
                        continue;
                    }
                }
            }
            content.push(block.clone());
        }
        content.reverse();
        out.push(crate::Message {
            role: message.role,
            content,
        });
    }
    out.reverse();
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extra_instructions_are_opt_in_and_local() {
        let root = tempfile::tempdir().unwrap();
        let cwd = root.path().join("repo");
        std::fs::create_dir(&cwd).unwrap();
        std::fs::write(cwd.join("AGENTS.md"), "BASE INSTRUCTIONS").unwrap();
        std::fs::write(cwd.join("TEAM.md"), "EXTRA INSTRUCTIONS").unwrap();
        std::fs::write(root.path().join("PRIVATE.md"), "NEVER IN PROMPT").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(root.path().join("PRIVATE.md"), cwd.join("LINK.md")).unwrap();
        let mut cfg = crate::config::Config::default();
        assert!(!session_context(&cwd, &cfg).contains("EXTRA INSTRUCTIONS"));
        cfg.context_files_extra = ["TEAM.md", "TEAM.md", "LINK.md", "../PRIVATE.md"].map(str::to_owned).into();
        let context = session_context(&cwd, &cfg);
        assert!(context.find("BASE INSTRUCTIONS").unwrap() < context.find("EXTRA INSTRUCTIONS").unwrap());
        assert_eq!(context.matches("EXTRA INSTRUCTIONS").count(), 1);
        assert!(!context.contains("NEVER IN PROMPT"));
        cfg.context_files = false;
        let context = session_context(&cwd, &cfg);
        assert!(!context.contains("BASE INSTRUCTIONS") && !context.contains("EXTRA INSTRUCTIONS"));
    }

    #[test]
    fn truncation_preserves_original_utf8_tail_and_distinct_outputs() {
        let id = format!("truncate-{}", std::process::id());
        let body = format!("{}tail", "日本語".repeat(2_000));
        let (cut, path) = truncate(body.clone(), &id);
        let path = path.unwrap();
        assert!(cut.ends_with("tail"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), body);
        let (_, other) = truncate("b".repeat(OUTPUT_LIMIT + 1), &id);
        let other = other.unwrap();
        assert_ne!(path, other);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), body);
        assert_eq!(truncate("short".into(), &id), ("short".into(), None));
        let corrupted = "x".repeat(body.len());
        std::fs::write(&path, &corrupted).unwrap();
        assert_eq!(truncate(body.clone(), &id), (body, None));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), corrupted);
        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_file(other);
    }

    #[test]
    fn screenshots_leave_room_for_attachments_and_archive_frames() {
        let marker = screenshot("aW1hZ2U=").unwrap();
        let image = crate::Block::Other(
            serde_json::json!({"type":"image", "source":{"type":"base64","media_type":"image/png","data":"cG5n"}}),
        );
        let mut messages = vec![crate::Message {
            role: crate::Role::User,
            content: vec![image.clone(), image.clone()],
        }];
        messages.extend((0..12).map(|_| crate::Message {
            role: crate::Role::User,
            content: vec![crate::Block::Other(marker.clone())],
        }));
        let projected = model_messages(&messages).unwrap();
        assert_eq!(
            serde_json::to_value(&projected[0]).unwrap(),
            serde_json::to_value(&messages[0]).unwrap()
        );
        assert!(
            matches!(&projected[1].content[0], crate::Block::Text { text } if text.contains(marker["path"].as_str().unwrap()))
        );
        assert_eq!(
            projected
                .iter()
                .flat_map(|m| &m.content)
                .filter(|b| matches!(b, crate::Block::Other(v) if v["type"] == "image"))
                .count(),
            10
        );
        assert!(projected
            .last()
            .unwrap()
            .content
            .iter()
            .any(|b| matches!(b, crate::Block::Other(v) if image_bytes(v).unwrap().1 == b"image")));
        assert!(matches!(&messages[1].content[0], crate::Block::Other(v) if v == &marker));
    }

    #[test]
    fn inline_images_share_the_count_and_wire_budgets() {
        let image = |data: String| {
            crate::Block::Other(
                serde_json::json!({"type":"image", "source":{"type":"base64","media_type":"image/png","data":data}}),
            )
        };
        let count = crate::snap::MAX_FRAMES + 8;
        let mut content = vec![image("cG5n".into()); count + 1];
        let project = |content| {
            model_messages(&[crate::Message {
                role: crate::Role::User,
                content,
            }])
            .unwrap()
            .remove(0)
            .content
        };
        let projected = project(content.clone());
        assert!(matches!(&projected[0], crate::Block::Text { text } if text.contains(".png")));
        assert_eq!(
            projected
                .iter()
                .filter(|b| matches!(b, crate::Block::Other(_)))
                .count(),
            count
        );
        content[1] = image("AAAA".repeat(6 * 1024 * 1024));
        let projected = project(content);
        assert!(matches!(&projected[1], crate::Block::Text { .. }));
        assert!(matches!(projected.last(), Some(crate::Block::Other(_))));
    }
}
