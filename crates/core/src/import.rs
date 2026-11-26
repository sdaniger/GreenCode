//! Import Claude Code, Codex and OpenCode transcripts as native threads. The source is only read.

use crate::{session::{Meta, Session}, Block, Message, Role};
use anyhow::{Context, Result};
use serde_json::Value;
use std::{io::{BufRead, BufReader}, path::{Path, PathBuf}};

#[derive(Clone)]
pub struct Found {
    /// `Claude Code`, `Codex` or `OpenCode`.
    pub source: &'static str,
    pub title: String,
    pub origin: Origin,
}

#[derive(Clone)]
pub enum Origin {
    File(PathBuf),
    /// Exported with `opencode session export` when imported; OpenCode owns its storage format.
    OpenCode(String),
}

/// Harness-injected user text, not something the user typed.
const INJECTED: [&str; 5] = ["<environment_context>", "<user_instructions>", "<system-reminder>", "<local-command", "<command-"];

// shortcut: text only; tool calls, tool results, images and thinking are dropped because each
// source tool's tool names and reasoning signatures do not replay on another provider. Upgrade if
// imported threads need the file-level detail.
fn read(path: &Path, first_only: bool) -> Result<(Option<PathBuf>, Vec<Message>)> {
    let (mut cwd, mut messages) = (None, Vec::<Message>::new());
    for line in BufReader::new(std::fs::File::open(path)?).lines() {
        let Ok(v) = serde_json::from_str::<Value>(&line?) else { continue };
        let codex = &v["payload"];
        if let Some(dir) = v["cwd"].as_str().or(codex["cwd"].as_str()).filter(|_| cwd.is_none()) { cwd = Some(PathBuf::from(dir)); }
        // Both tools replace the earlier history with a summary when they compact.
        if v["type"] == "compacted" {
            if let Some(summary) = codex["message"].as_str() { messages = vec![Message::user(summary)]; }
            continue;
        }
        let message = match v["type"].as_str() {
            Some("user" | "assistant") if v["isSidechain"] != true && v["isMeta"] != true => &v["message"],
            Some("response_item") if codex["type"] == "message" => codex,
            // Pi session lines: `{"type":"message","message":{"role":..,"content":[..]}}`.
            Some("message") => &v["message"],
            _ => continue,
        };
        let role = match message["role"].as_str() {
            Some("user") => Role::User,
            Some("assistant") => Role::Assistant,
            _ => continue,
        };
        let parts: Vec<&str> = match &message["content"] {
            Value::String(s) => vec![s.as_str()],
            Value::Array(blocks) => blocks.iter().filter(|b| matches!(b["type"].as_str(), Some("text" | "input_text" | "output_text"))).filter_map(|b| b["text"].as_str()).collect(),
            _ => continue,
        };
        let text = parts.into_iter().map(str::trim).filter(|t| !t.is_empty() && !(role == Role::User && INJECTED.iter().any(|p| t.starts_with(p)))).collect::<Vec<_>>().join("\n");
        if text.is_empty() { continue; }
        if v["isCompactSummary"] == true { messages.clear(); }
        push(&mut messages, role, &text);
        if first_only { break; }
    }
    Ok((cwd, messages))
}

fn push(messages: &mut Vec<Message>, role: Role, text: &str) {
    if text.is_empty() { return; }
    // Providers require the history to open with a user turn.
    if messages.is_empty() && role == Role::Assistant { return; }
    match messages.last_mut().filter(|m| m.role == role) {
        Some(last) => last.content.push(Block::Text { text: text.into() }),
        None => messages.push(Message { role, content: vec![Block::Text { text: text.into() }] }),
    }
}

/// The text of an `opencode session export` object. Tool calls, tool output and reasoning are dropped.
fn opencode(v: &Value) -> Option<(Option<PathBuf>, Vec<Message>)> {
    let list = v["messages"].as_array()?;
    let cwd = v["info"]["location"]["directory"].as_str().map(PathBuf::from);
    let mut messages = Vec::new();
    for m in list {
        match m["type"].as_str() {
            Some("user") => push(&mut messages, Role::User, m["text"].as_str().unwrap_or("").trim()),
            Some("assistant") => {
                let text = m["content"].as_array().into_iter().flatten()
                    .filter(|p| p["type"] == "text")
                    .filter_map(|p| p["text"].as_str())
                    .map(str::trim).filter(|t| !t.is_empty()).collect::<Vec<_>>().join("\n");
                push(&mut messages, Role::Assistant, &text);
            }
            // The newest completed summary replaces the history it compacted.
            Some("compaction") => {
                if let Some(summary) = m["summary"].as_str().filter(|s| !s.trim().is_empty()) {
                    messages = vec![Message::user(summary.trim())];
                }
            }
            _ => {}
        }
    }
    Some((cwd, messages))
}

fn title(messages: &[Message]) -> String {
    match messages.first().and_then(|m| m.content.first()) {
        Some(Block::Text { text }) => text.lines().next().unwrap_or("").chars().take(60).collect(),
        _ => String::new(),
    }
}

fn env_dir(var: &str, default: &str) -> PathBuf {
    std::env::var_os(var).filter(|p| !p.is_empty()).map(PathBuf::from).unwrap_or_else(|| crate::user_home().join(default))
}

fn jsonl_under(dir: &Path, depth: u8, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let path = entry.path();
        if path.extension().is_some_and(|e| e == "jsonl") { out.push(path); }
        else if depth > 0 && path.is_dir() { jsonl_under(&path, depth - 1, out); }
    }
}

/// The newest Claude Code, Codex and OpenCode sessions started in `project`, newest first per source.
pub fn discover(project: &Path) -> Vec<Found> {
    let mut found = vec![];
    // Claude Code keeps one directory per project, named after the path.
    let encoded: String = project.to_string_lossy().chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect();
    let mut claude = vec![];
    jsonl_under(&env_dir("CLAUDE_CONFIG_DIR", ".claude").join("projects").join(encoded), 0, &mut claude);
    claude.sort_by_key(|p| std::cmp::Reverse(p.metadata().and_then(|m| m.modified()).ok()));
    // Codex files are `sessions/YYYY/MM/DD/rollout-<time>-<id>.jsonl`, so the path orders them by time.
    let mut codex = vec![];
    jsonl_under(&env_dir("CODEX_HOME", ".codex").join("sessions"), 3, &mut codex);
    codex.sort_by(|a, b| b.cmp(a));
    // shortcut: only the 200 newest Codex sessions are checked for this project; older ones need `--import-session`.
    codex.truncate(200);
    for (source, paths, check_cwd) in [("Claude Code", claude, false), ("Codex", codex, true)] {
        let sessions = paths.into_iter().filter_map(|path| {
            let (cwd, messages) = read(&path, true).ok()?;
            if messages.is_empty() || (check_cwd && cwd.as_deref() != Some(project)) { return None; }
            Some(Found { source, title: title(&messages), origin: Origin::File(path) })
        });
        found.extend(sessions.take(20));
    }
    found.extend(opencode_sessions(project));
    found
}

/// A directory with recent conversations from Claude Code, Codex or Pi.
pub struct Project {
    pub dir: PathBuf,
    /// Newest first, at most three.
    pub sessions: Vec<Found>,
    pub newest: std::time::SystemTime,
}

/// Directories used by Claude Code, Codex and Pi in the last 30 days, newest first.
pub fn scan_projects() -> Vec<Project> {
    let pi = std::env::var_os("PI_CODING_AGENT_SESSION_DIR")
        .filter(|p| !p.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| env_dir("PI_CODING_AGENT_DIR", ".pi/agent").join("sessions"));
    scan_in(
        &env_dir("CLAUDE_CONFIG_DIR", ".claude").join("projects"),
        &env_dir("CODEX_HOME", ".codex").join("sessions"),
        &pi,
    )
}

// shortcut: at most 300 newest files per source are read and non-git folders are not separated; upgrade if histories exceed that.
fn scan_in(claude: &Path, codex: &Path, pi: &Path) -> Vec<Project> {
    let cutoff = std::time::SystemTime::now() - std::time::Duration::from_secs(30 * 86400);
    let mut by_dir = std::collections::BTreeMap::<PathBuf, Project>::new();
    for (source, root, depth) in [("Claude Code", claude, 1), ("Codex", codex, 3), ("Pi", pi, 1)] {
        let mut files = vec![];
        jsonl_under(root, depth, &mut files);
        let mut files: Vec<_> = files.into_iter().filter_map(|p| Some((p.metadata().ok()?.modified().ok().filter(|t| *t >= cutoff)?, p))).collect();
        files.sort_by(|a, b| b.0.cmp(&a.0));
        files.truncate(300);
        for (time, path) in files {
            let Ok((Some(dir), messages)) = read(&path, true) else { continue };
            let skipped = dir.components().any(|c| c.as_os_str() == "Downloads") || dir.ends_with("Documents/Codex") || dir.to_string_lossy().contains("Documents/Codex/");
            if messages.is_empty() || skipped || !dir.is_dir() { continue; }
            let project = by_dir.entry(dir.clone()).or_insert(Project { dir, sessions: vec![], newest: time });
            project.newest = project.newest.max(time);
            project.sessions.push(Found { source, title: title(&messages), origin: Origin::File(path) });
        }
    }
    let mut projects: Vec<_> = by_dir.into_values().collect();
    for p in &mut projects {
        let mtime = |f: &Found| match &f.origin { Origin::File(path) => path.metadata().and_then(|m| m.modified()).ok(), Origin::OpenCode(_) => None };
        p.sessions.sort_by_key(|f| std::cmp::Reverse(mtime(f)));
        p.sessions.truncate(3);
    }
    projects.sort_by(|a, b| b.dir.join(".git").exists().cmp(&a.dir.join(".git").exists()).then(b.newest.cmp(&a.newest)));
    projects
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scans_recent_projects_from_all_three_tools() {
        let home = tempfile::tempdir().unwrap();
        let (a, b, c, old) = (home.path().join("a"), home.path().join("b"), home.path().join("Downloads/c"), home.path().join("old"));
        for d in [&a, &b, &c, &old] { std::fs::create_dir_all(d).unwrap(); }
        std::fs::create_dir(b.join(".git")).unwrap();
        let write = |path: PathBuf, line: String| { std::fs::create_dir_all(path.parent().unwrap()).unwrap(); std::fs::write(path, line + "\n").unwrap(); };
        let claude = |d: &Path| format!(r#"{{"type":"user","cwd":"{}","message":{{"role":"user","content":"hi"}}}}"#, d.display());
        let codex = |d: &Path| format!(r#"{{"type":"response_item","payload":{{"type":"message","role":"user","cwd":"{}","content":[{{"type":"input_text","text":"hi"}}]}}}}"#, d.display());
        let pi = |d: &Path| format!(r#"{{"type":"message","cwd":"{}","message":{{"role":"user","content":[{{"type":"text","text":"hi"}}]}}}}"#, d.display());
        for i in 0..4 { write(home.path().join(format!("claude/p/{i}.jsonl")), claude(&a)); }
        write(home.path().join("codex/2026/01/01/r.jsonl"), codex(&b));
        write(home.path().join("pi/p/s.jsonl"), pi(&a));
        write(home.path().join("claude/q/x.jsonl"), claude(&c));
        write(home.path().join("claude/o/x.jsonl"), claude(&old));
        let f = std::fs::File::options().write(true).open(home.path().join("claude/o/x.jsonl")).unwrap();
        f.set_modified(std::time::SystemTime::now() - std::time::Duration::from_secs(40 * 86400)).unwrap();
        let projects = scan_in(&home.path().join("claude"), &home.path().join("codex"), &home.path().join("pi"));
        let dirs: Vec<_> = projects.iter().map(|p| p.dir.clone()).collect();
        assert_eq!(dirs.len(), 2, "{dirs:?}");
        assert_eq!(projects[0].dir, b, "git repositories sort first");
        let pa = projects.iter().find(|p| p.dir == a).unwrap();
        assert_eq!(pa.sessions.len(), 3);
        assert!(projects.iter().any(|p| p.dir == b && p.sessions[0].source == "Codex"));
    }
}

/// `opencode session list` is already scoped to its working directory. OpenCode may not be installed.
fn opencode_sessions(project: &Path) -> Vec<Found> {
    let Some(exe) = crate::config::executable("opencode") else { return vec![] };
    let Ok(out) = std::process::Command::new(exe).args(["session", "list", "--format", "json"]).current_dir(project).output() else { return vec![] };
    let Ok(Value::Array(list)) = serde_json::from_slice(&out.stdout) else { return vec![] };
    list.into_iter().filter_map(|s| {
        let id = s["id"].as_str()?;
        let title = s["title"].as_str().filter(|t| !t.trim().is_empty()).unwrap_or("無題").to_string();
        Some(Found { source: "OpenCode", title, origin: Origin::OpenCode(id.into()) })
    }).take(20).collect()
}

/// Copies a GreenCode session (JSONL with a `meta` line) into a new thread, keeping tool blocks.
/// Used for handoffs between machines; the source file is only read.
pub fn import_native(path: &Path) -> Result<Option<Meta>> {
    let text = std::fs::read_to_string(path)?;
    let mut meta: Option<Meta> = None;
    let mut messages = Vec::new();
    for line in text.lines() {
        let Ok(v) = serde_json::from_str::<Value>(line) else { continue };
        if let Some(m) = v.get("meta") { meta = serde_json::from_value(m.clone()).ok(); }
        else if let Ok(message) = serde_json::from_value::<Message>(v) { messages.push(message); }
    }
    let Some(mut meta) = meta else { return Ok(None) };
    anyhow::ensure!(!messages.is_empty(), "{} has no conversation", path.display());
    if !meta.cwd.is_dir() { meta.cwd = std::env::current_dir()?; }
    let mut fresh = Meta::new(meta.cwd.clone(), meta.model.clone(), meta.backend.clone());
    fresh.title = meta.title.clone();
    fresh.subagent_model = meta.subagent_model.clone();
    let session = Session::create(&fresh)?;
    for message in &messages { session.append(message)?; }
    Ok(Some(fresh))
}

fn into_thread(cwd: Option<PathBuf>, messages: Vec<Message>, model: &str) -> Result<Meta> {
    let cwd = cwd.filter(|d| d.is_dir()).map_or_else(std::env::current_dir, Ok)?;
    let mut meta = Meta::new(cwd, model.into(), "native".into());
    meta.title = title(&messages);
    let session = Session::create(&meta)?;
    for message in &messages { session.append(message)?; }
    Ok(meta)
}

pub fn import(path: &Path, model: &str) -> Result<Meta> {
    let (cwd, messages) = read(path, false).with_context(|| format!("{}", path.display()))?;
    anyhow::ensure!(!messages.is_empty(), "{} has no Claude Code or Codex conversation", path.display());
    into_thread(cwd, messages, model)
}

/// Imports a GreenCode, Claude Code, Codex, or OpenCode export by its file extension and content.
pub fn import_file(path: &Path, model: &str) -> Result<Meta> {
    match import_native(path)? {
        Some(meta) => Ok(meta),
        None if path.extension().is_some_and(|e| e == "json") => import_export(path, model),
        None => import(path, model),
    }
}

/// A file written by `opencode session export ID > FILE`.
pub fn import_export(path: &Path, model: &str) -> Result<Meta> {
    let v: Value = serde_json::from_str(&std::fs::read_to_string(path)?).with_context(|| format!("{} is not JSON", path.display()))?;
    let (cwd, messages) = opencode(&v).with_context(|| format!("{} is not an OpenCode session export", path.display()))?;
    anyhow::ensure!(!messages.is_empty(), "{} has no OpenCode conversation", path.display());
    into_thread(cwd, messages, model)
}

pub fn import_found(found: &Found, model: &str) -> Result<Meta> {
    match &found.origin {
        Origin::File(path) => import(path, model),
        Origin::OpenCode(id) => import_opencode(id, model),
    }
}

fn import_opencode(id: &str, model: &str) -> Result<Meta> {
    let exe = crate::config::executable("opencode").context("OpenCode is not installed")?;
    let out = std::process::Command::new(&exe).args(["session", "export", id]).output().with_context(|| format!("cannot run {}", exe.display()))?;
    anyhow::ensure!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr).trim());
    let v: Value = serde_json::from_slice(&out.stdout)?;
    let (cwd, messages) = opencode(&v).context("opencode returned no session")?;
    anyhow::ensure!(!messages.is_empty(), "session {id} has no OpenCode conversation");
    into_thread(cwd, messages, model)
}
