use crate::skills;
use std::path::Path;

pub const MEMORY_LIMIT: usize = 4000;
pub(crate) const OUTPUT_PREVIEW: usize = 2 * 1024;

/// Inline output budgets from `[artifact]`, `output_max_columns`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Limits {
    spill: usize,
    head: usize,
    tail: usize,
    tail_lines: usize,
    /// `None` = unlimited artifact file.
    max_artifact: Option<usize>,
    pub max_columns: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self::new(&crate::config::Config::default())
    }
}

impl Limits {
    pub fn new(cfg: &crate::config::Config) -> Self {
        let a = cfg.bash_opts.artifact;
        Self::from_kb(a.spill_threshold_kb, a.head_kb, a.tail_kb, a.tail_lines, a.max_bytes_mb, cfg.bash_opts.output_max_columns)
    }

    fn from_kb(spill: u64, head: u64, tail: u64, tail_lines: usize, max_mb: u64, max_columns: usize) -> Self {
        let kb = |n: u64| (n as usize).saturating_mul(1024);
        Self {
            spill: kb(spill).max(1),
            head: kb(head),
            tail: kb(tail),
            tail_lines,
            max_artifact: (max_mb > 0).then(|| kb(max_mb).saturating_mul(1024)),
            max_columns,
        }
    }
}

/// Drops everything past `max` bytes on each line and marks the cut with an ellipsis; 0 disables.
pub fn cap_columns(text: &str, max: usize) -> String {
    if max == 0 {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    for line in text.split_inclusive('\n') {
        let (body, newline) = line.strip_suffix('\n').map_or((line, ""), |b| (b, "\n"));
        if body.len() > max {
            let keep = max.saturating_sub(3);
            out.push_str(&body[..floor(body, keep)]);
            if max >= 3 {
                out.push('…');
            }
        } else {
            out.push_str(body);
        }
        out.push_str(newline);
    }
    out
}

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
another call. Finish the whole request before ending the turn: never stop to announce a next step or ask \
leave for requested work; a failed check is not a blocker.\n\
Use `memory` for durable user/project facts. Read matching skills before work; save reusable, non-trivial \
procedures to ~/.greencode/skills/<name>/SKILL.md with `name` and `description` frontmatter. If a durable \
project fact is clear, append one `Lesson:` line; otherwise omit it.\n\
\nPonytail (full) is the default coding style. Make the smallest complete change. \
Reuse existing code, then platform APIs or installed dependencies. Avoid speculative options and abstractions. \
Update affected callers, tests, and docs; verify with the narrowest relevant check. Never cut validation, \
security, accessibility, or error handling to save code. For coding tasks, end with any skipped checks or known risks.";
/// Marks an appended workspace-context update. The transcript UI and compaction skip these messages.
pub const CONTEXT_MARKER: &str = "[greencode context update]";

/// The system prompt before config shaping; kept public so callers can name the built-in text.
pub fn base_prompt() -> &'static str {
    SYSTEM
}

/// The system prompt for one active model: a custom or per-model template replaces the built-in
/// instructions, then the configured personality, edit rule, model id, tree and append text apply.
pub fn system_prompt(cfg: &crate::config::Config, model: &str, cwd: &Path) -> String {
    system_prompt_at(cfg, model, cwd, &crate::home())
}

fn system_prompt_at(cfg: &crate::config::Config, model: &str, cwd: &Path, home: &Path) -> String {
    let opts = &cfg.loop_opts;
    let template = opts.system_prompt_templates.iter()
        .filter(|(selector, _)| !selector.trim().is_empty() && model.to_lowercase().contains(&selector.to_lowercase()))
        .max_by_key(|(selector, _)| selector.chars().count())
        .and_then(|(_, path)| std::fs::read_to_string(template_path(path)?).ok().filter(|text| !text.trim().is_empty()));
    let custom = cfg.system_prompt.is_some() || template.is_some();
    let mut s = match (&cfg.system_prompt, template) {
        (Some(text), _) => text.clone(),
        (None, Some(template)) => template,
        (None, None) => {
            let mut s = SYSTEM.to_owned();
            if let Some(block) = personality_block(opts.personality, home) {
                s.push_str("\n\n");
                s.push_str(&block);
            }
            if opts.encourage_edits {
                s.push_str("\n\nFor file edits use the `edit` tool and `write` for a new file; never rewrite a file with sed, perl, or python.");
            }
            s
        }
    };
    if opts.include_model_in_prompt && !custom {
        s.push_str(&format!("\n\nActive model: {model}."));
    }
    if opts.include_workspace_tree {
        let tree = workspace_tree(cwd);
        if !tree.is_empty() {
            s.push_str("\n\n# Workspace tree\n");
            s.push_str(&tree);
        }
    }
    if let Some(text) = &cfg.append_system_prompt {
        if !s.is_empty() { s.push_str("\n\n"); }
        s.push_str(text);
    }
    // Sub-agents do not use this constructor; they clone the main system prompt and strip this line.
    if let Some(line) = crate::tools::vault::prompt_line(cfg) {
        s.push_str("\n\n");
        s.push_str(line);
    }
    if let Some(guidance) = crate::subagents::eager_guidance(cfg.task.eager) {
        s.push_str("\n\n");
        s.push_str(guidance);
    }
    // ovnfs found (or configured) means checkouts replace git worktrees and copies.
    if let Some(note) = crate::config::isolation::prompt_note(cfg.isolation_opts.ovnfs().as_deref()) {
        s.push_str("\n\n");
        s.push_str(&note);
    }
    s
}

fn template_path(path: &str) -> Option<std::path::PathBuf> {
    match path.strip_prefix("~/") {
        Some(rest) => Some(crate::user_home().join(rest)),
        None => Path::new(path).is_absolute().then(|| std::path::PathBuf::from(path)),
    }
}

/// `~/.greencode/PERSONALITY.md` replaces the preset; `none` always omits the block.
fn personality_block(personality: crate::config::agent_loop::Personality, home: &Path) -> Option<String> {
    use crate::config::agent_loop::Personality;
    if personality == Personality::None {
        return None;
    }
    if let Some(text) = std::fs::read_to_string(home.join("PERSONALITY.md")).ok().filter(|text| !text.trim().is_empty()) {
        return Some(text.trim().to_owned());
    }
    match personality {
        Personality::Friendly => Some("Be warm and encouraging: explain briefly why a change helps, and acknowledge progress without flattery.".into()),
        Personality::Pragmatic => Some("Be direct and terse: skip pleasantries, state the recommendation, then the tradeoff.".into()),
        Personality::Default | Personality::None => None,
    }
}

const TREE_SKIP: &[&str] = &[".git", ".greencode", "node_modules", "target", "dist", "build", ".venv", "vendor", "__pycache__"];

fn workspace_tree(cwd: &Path) -> String {
    const MAX_LINES: usize = 200;
    fn walk(dir: &Path, depth: usize, prefix: &str, out: &mut String, lines: &mut usize) {
        if depth >= 3 {
            return;
        }
        let Ok(entries) = std::fs::read_dir(dir) else { return };
        let mut children: Vec<(String, bool)> = entries.flatten().filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') || TREE_SKIP.contains(&name.as_str()) { return None; }
            Some((name, entry.file_type().ok()?.is_dir()))
        }).collect();
        children.sort_by(|a, b| a.0.cmp(&b.0));
        for (name, is_dir) in children {
            if *lines >= MAX_LINES { return; }
            out.push_str(prefix);
            out.push_str(&name);
            if is_dir { out.push('/'); }
            out.push('\n');
            *lines += 1;
            if is_dir { walk(&dir.join(&name), depth + 1, &format!("{prefix}  "), out, lines); }
        }
    }
    let mut out = String::new();
    walk(cwd, 0, "", &mut out, &mut 0);
    out
}

/// Workspace-specific instructions, memory, skills and paths. Deterministic for unchanged files.
pub fn session_context(cwd: &Path, cfg: &crate::config::Config) -> String {
    let mut s = String::new();
    // `memory.backend = off` means no memory at all, including the curated `MEMORY.md` files.
    if cfg.memory_opts.memory.backend != crate::config::memory::MemoryBackend::Off {
        for (scope, path) in memory_files(cwd) {
            if let Ok(m) = std::fs::read_to_string(path) {
                if !m.trim().is_empty() {
                    s.push_str(&format!("# Memory ({scope})\n{}\n\n", m.trim()));
                }
            }
        }
    }
    let skills = if cfg.skills_enabled && cfg.provider_opts.skillful {
        skills::catalog(cwd, cfg)
    } else {
        Vec::new()
    };
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
    s.push_str(&cfg.context_opts.workspace.block(cwd));
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
/// A `max` cap keeps the beginning (up to 3 MB) and the most recent remainder around a notice.
fn spill(text: &str, id: &str, max: Option<usize>) -> Option<std::path::PathBuf> {
    use sha2::{Digest, Sha256};
    let body = match max.filter(|&m| m > 0 && text.len() > m) {
        Some(max) => {
            let head = (3 * 1024 * 1024).min(max / 2);
            let notice = format!("\n\n[ARTIFACT TRUNCATED: {} bytes]\n\n", text.len());
            let tail = max.saturating_sub(head + notice.len()).min(text.len() - head);
            if tail == 0 {
                text[..floor(text, head)].to_string()
            } else {
                format!("{}{notice}{}", &text[..floor(text, head)], &text[text.len() - floor_reverse(text, tail)..])
            }
        }
        None => text.to_string(),
    };
    let dir = crate::home().join("spill");
    let path = dir.join(format!(
        "{}-{:x}.txt",
        id.replace(|c: char| !c.is_ascii_alphanumeric(), "_"),
        Sha256::digest(body.as_bytes())
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
        Ok(mut file) => file.write_all(body.as_bytes()).ok()?,
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists
            && path.symlink_metadata().is_ok_and(|m| m.is_file() && m.len() == body.len() as u64)
            && std::fs::read(&path).is_ok_and(|bytes| bytes == body.as_bytes()) => {}
        Err(_) => return None,
    }
    Some(path)
}

/// Keeps the head and tail of long tool output and saves the whole text to a file.
pub fn truncate(out: String, id: &str) -> (String, Option<std::path::PathBuf>) {
    truncate_with(out, id, &Limits::default())
}

pub fn truncate_with(out: String, id: &str, limits: &Limits) -> (String, Option<std::path::PathBuf>) {
    preview_with(out, id, limits.spill, limits)
}

pub(crate) fn preview_with(out: String, id: &str, limit: usize, limits: &Limits) -> (String, Option<std::path::PathBuf>) {
    if let Some((preview, path)) = capped(&out, id, limit, limits) {
        (preview, Some(path))
    } else {
        (out, None)
    }
}

fn capped(out: &str, id: &str, limit: usize, limits: &Limits) -> Option<(String, std::path::PathBuf)> {
    if out.len() <= limit {
        return None;
    }
    let path = spill(out, id, limits.max_artifact)?;
    let notice = format!(
        "\n\n[... tool output is capped; full output: {} ...]\n\n",
        path.display()
    );
    let keep = limit.checked_sub(notice.len())?;
    // The head never takes more than half the inline budget, as in OMP's output sink.
    let head = limits.head.min(keep / 2);
    let tail = limits.tail.min(keep - head);
    let head = &out[..floor(out, head)];
    let tail = last(out, tail, limits.tail_lines);
    Some((format!("{head}{notice}{tail}"), path))
}

/// The last `bytes` of `out`, then cut forward to at most `lines` lines.
fn last(out: &str, bytes: usize, lines: usize) -> &str {
    let mut tail = &out[out.len() - floor_reverse(out, bytes)..];
    if lines > 0 {
        let count = tail.lines().count();
        if count > lines {
            let mut skip = count - lines;
            for (i, c) in tail.char_indices() {
                if c == '\n' {
                    skip -= 1;
                    if skip == 0 {
                        tail = &tail[i + 1..];
                        break;
                    }
                }
            }
        }
    }
    tail
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
            if let Some((preview, _)) = capped(content, tool_use_id, 512, &Limits::default()) {
                *content = preview;
                changed = true;
            }
        }
    }
    changed
}

/// Blanks tool results a later action superseded: reads of a file read again, and searches that found
/// nothing. Runs on stored history, so the rewrite happens once and the prompt prefix stays stable.
pub(crate) fn prune_results(
    messages: &mut [crate::Message],
    supersede_reads: bool,
    drop_useless: bool,
) -> bool {
    use crate::{Block, Role};
    if !supersede_reads && !drop_useless {
        return false;
    }
    let mut calls: std::collections::HashMap<String, (String, String)> = Default::default();
    for m in messages.iter().filter(|m| m.role == Role::Assistant) {
        for block in &m.content {
            if let Block::ToolUse { id, name, input } = block {
                let path = input["path"].as_str().unwrap_or_default().to_string();
                calls.insert(id.clone(), (name.clone(), path));
            }
        }
    }
    let mut seen = std::collections::HashSet::new();
    let mut changed = false;
    for m in messages.iter_mut().rev().filter(|m| m.role == Role::User) {
        for block in &mut m.content {
            let Block::ToolResult { tool_use_id, content, is_error: false } = block else {
                continue;
            };
            let Some((name, path)) = calls.get(tool_use_id) else { continue };
            let replacement = if drop_useless
                && matches!(name.as_str(), "grep" | "glob")
                && content == "no matches"
            {
                Some("[Uneventful result elided]".to_string())
            } else if supersede_reads && name == "read" && !seen.insert(path.clone()) {
                Some(format!("[Superseded by a later read of {path}]"))
            } else {
                None
            };
            if let Some(text) = replacement.filter(|text| *text != *content) {
                *content = text;
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

/// Media type and bytes of an image file, or `None` when the path is not a supported image.
pub(crate) fn read_image_bytes(path: &Path) -> anyhow::Result<Option<(&'static str, Vec<u8>)>> {
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
    Ok(Some((media, bytes)))
}

pub(crate) fn read_image(path: &Path) -> anyhow::Result<Option<serde_json::Value>> {
    use base64::Engine;
    let Some((media, bytes)) = read_image_bytes(path)? else { return Ok(None) };
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
pub fn model_messages(
    messages: &[crate::Message],
    files: &crate::config::files::FileOpts,
) -> anyhow::Result<Vec<crate::Message>> {
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
                    if files.images.block_images {
                        content.push(crate::Block::Text {
                            text: format!("[Screenshot not attached: {path}; images.block_images is on.]"),
                        });
                        continue;
                    }
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
                    if files.images.block_images {
                        content.push(crate::Block::Text {
                            text: "[Image not attached: images.block_images is on.]".into(),
                        });
                        continue;
                    }
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
    fn pruning_supersedes_reads_and_drops_empty_searches() {
        use crate::{Block, Message, Role};
        let call = |id: &str, name: &str, path: &str| Message {
            role: Role::Assistant,
            content: vec![Block::ToolUse { id: id.into(), name: name.into(), input: serde_json::json!({"path": path}) }],
        };
        let result = |id: &str, content: &str| Message {
            role: Role::User,
            content: vec![Block::ToolResult { tool_use_id: id.into(), content: content.into(), is_error: false }],
        };
        let source = || vec![
            call("1", "read", "src/a.rs"),
            result("1", "OLD"),
            call("2", "read", "src/a.rs"),
            result("2", "NEW"),
            call("3", "grep", "src"),
            result("3", "no matches"),
        ];
        let text = |m: &Message| match &m.content[0] {
            Block::ToolResult { content, .. } => content.clone(),
            other => panic!("{other:?}"),
        };
        let mut messages = source();
        assert!(!prune_results(&mut messages, false, false));
        assert_eq!((text(&messages[1]), text(&messages[5])), ("OLD".into(), "no matches".into()));
        assert!(prune_results(&mut messages, true, true));
        assert_eq!(text(&messages[1]), "[Superseded by a later read of src/a.rs]");
        assert_eq!(text(&messages[3]), "NEW");
        assert_eq!(text(&messages[5]), "[Uneventful result elided]");
        assert!(!prune_results(&mut messages, true, true));
    }

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
    fn skillful_gates_the_prompt_skill_list() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join(".greencode/skills/demo");
        std::fs::create_dir_all(&path).unwrap();
        std::fs::write(path.join("SKILL.md"), "---\nname: demo\ndescription: Demo skill\n---\nbody").unwrap();
        let mut cfg = crate::config::Config::default();
        assert!(session_context(root.path(), &cfg).contains("demo: Demo skill"));
        // `/skillful` is a session toggle over this one listing, not over skill discovery.
        cfg.provider_opts.skillful = false;
        assert!(!session_context(root.path(), &cfg).contains("demo: Demo skill"));
        assert!(cfg.skills_enabled);
    }

    #[test]
    fn truncation_preserves_original_utf8_tail_and_distinct_outputs() {
        let id = format!("truncate-{}", std::process::id());
        let body = format!("{}tail", "日本語".repeat(8_000));
        let (cut, path) = truncate(body.clone(), &id);
        let path = path.unwrap();
        assert!(cut.ends_with("tail"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), body);
        let (_, other) = truncate("b".repeat(50 * 1024 + 1), &id);
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
    fn artifact_budgets_shape_the_inline_window_and_file() {
        let id = format!("artifact-{}", std::process::id());
        let limits = Limits::from_kb(2, 1, 1, 3, 1, 10);
        let body = (0..400).map(|i| format!("line {i:03}\n")).collect::<String>();
        let (cut, path) = truncate_with(body.clone(), &id, &limits);
        let path = path.unwrap();
        assert!(cut.starts_with("line 000\n"));
        assert!(cut.contains("[... tool output is capped"));
        let tail = cut.rsplit("...]\n\n").next().unwrap();
        assert_eq!(tail.lines().count(), 3, "{cut}");
        assert!(tail.ends_with("line 399\n"));
        // The artifact file keeps every byte while it fits the 1 MiB allowance.
        assert!(body.len() <= 1024 * 1024);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), body);
        let (capped, path) = truncate_with("x".repeat(2 * 1024 * 1024), &format!("{id}-cap"), &limits);
        let path = path.unwrap();
        let artifact = std::fs::read_to_string(&path).unwrap();
        assert!(artifact.len() <= 1024 * 1024 + 128, "artifact is capped around 1 MiB");
        assert!(artifact.contains("[ARTIFACT TRUNCATED: 2097152 bytes]"));
        assert!(capped.contains("capped"));
        let _ = std::fs::remove_file(path);

        let head_only = Limits::from_kb(1, 0, 1, 0, 16, 0);
        let (cut, _) = truncate_with("a".repeat(4096), &format!("{id}-head"), &head_only);
        assert!(cut.starts_with("\n\n[... tool output is capped"), "{cut:?}");
        assert!(cut.ends_with('a'));

        assert_eq!(cap_columns("short\nabcdefghij\n", 8), "short\nabcde…\n");
        assert_eq!(cap_columns("abcdefghij", 0), "abcdefghij");
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
        let projected = model_messages(&messages, &Default::default()).unwrap();
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
    fn block_images_leaves_a_note_instead_of_an_image() {
        let image = crate::Block::Other(
            serde_json::json!({"type":"image", "source":{"type":"base64","media_type":"image/png","data":"cG5n"}}),
        );
        let files = crate::config::files::FileOpts {
            images: crate::config::files::ImagesCfg { block_images: true, ..Default::default() },
            ..Default::default()
        };
        let projected = model_messages(
            &[crate::Message { role: crate::Role::User, content: vec![image] }],
            &files,
        )
        .unwrap();
        assert!(matches!(&projected[0].content[..], [crate::Block::Text { text }] if text.contains("block_images")));
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
            }], &Default::default())
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

    #[test]
    fn assembles_personality_edit_rule_model_tree_and_template() {
        use crate::config::{agent_loop::Personality, Config};
        let root = tempfile::tempdir().unwrap();
        let cwd = root.path().join("repo");
        std::fs::create_dir_all(cwd.join("src")).unwrap();
        std::fs::create_dir_all(cwd.join("target")).unwrap();
        std::fs::write(cwd.join("src/main.rs"), "fn main() {}").unwrap();
        std::fs::write(cwd.join(".env"), "SECRET=1").unwrap();
        let home = root.path().join("home");
        std::fs::create_dir(&home).unwrap();

        let mut cfg = Config::default();
        cfg.loop_opts.personality = Personality::Friendly;
        cfg.loop_opts.include_workspace_tree = true;
        let prompt = system_prompt_at(&cfg, "openai/gpt-5", &cwd, &home);
        assert!(prompt.contains("Active model: openai/gpt-5."));
        assert!(prompt.contains("warm and encouraging"));
        assert!(prompt.contains("never rewrite a file with sed"));
        assert!(prompt.contains("src/\n  main.rs"));
        assert!(!prompt.contains("target/") && !prompt.contains(".env"));

        std::fs::write(home.join("PERSONALITY.md"), "Answer in Simplified Technical English.").unwrap();
        let prompt = system_prompt_at(&cfg, "openai/gpt-5", &cwd, &home);
        assert!(prompt.contains("Simplified Technical English") && !prompt.contains("warm and encouraging"));

        cfg.loop_opts.personality = Personality::None;
        cfg.loop_opts.encourage_edits = false;
        cfg.loop_opts.include_model_in_prompt = false;
        let prompt = system_prompt_at(&cfg, "openai/gpt-5", &cwd, &home);
        assert!(!prompt.contains("Simplified Technical English"));
        assert!(!prompt.contains("never rewrite a file with sed"));
        assert!(!prompt.contains("Active model:"));

        let short = root.path().join("short.md");
        let long = root.path().join("long.md");
        std::fs::write(&short, "SHORT TEMPLATE").unwrap();
        std::fs::write(&long, "LONG TEMPLATE").unwrap();
        cfg.loop_opts.include_workspace_tree = false;
        cfg.loop_opts.system_prompt_templates.insert("gpt".into(), short.display().to_string());
        cfg.loop_opts.system_prompt_templates.insert("openai/gpt-5".into(), long.display().to_string());
        let prompt = system_prompt_at(&cfg, "openai/gpt-5", &cwd, &home);
        assert!(prompt.contains("LONG TEMPLATE") && !prompt.contains("SHORT TEMPLATE"));
        assert!(!prompt.contains("You are GreenCode"));
        assert!(!prompt.contains("src/main.rs"));
        cfg.system_prompt = Some("EXPLICIT".into());
        assert!(system_prompt_at(&cfg, "openai/gpt-5", &cwd, &home).starts_with("EXPLICIT"));
    }
}
