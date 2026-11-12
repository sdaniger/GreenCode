pub mod mcp;

use crate::{
    context::{memory_files, MEMORY_LIMIT},
    permission::resolve,
    ToolDef,
};
use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};
use std::{path::Path, time::Duration};

fn def(name: &str, description: &str, props: Value, required: &[&str]) -> ToolDef {
    ToolDef {
        name: name.into(),
        description: description.into(),
        schema: json!({"type": "object", "properties": props, "required": required}),
    }
}

/// The built-in tools. Order and wording are fixed: they sit at the front of the cached prefix.
pub fn defs(subagent: bool, mcp_servers: &[String]) -> Vec<ToolDef> {
    let s = json!({"type": "string"});
    let mut v = vec![
        def("read", "Read a text file. `offset` is the first line (1-based), `limit` the number of lines.",
            json!({"path": s, "offset": {"type": "integer"}, "limit": {"type": "integer"}}), &["path"]),
        def("write", "Create or overwrite a file.", json!({"path": s, "content": s}), &["path", "content"]),
        def("edit", "Replace `old_string` with `new_string` in a file. `old_string` must match exactly once unless `replace_all` is true.",
            json!({"path": s, "old_string": s, "new_string": s, "replace_all": {"type": "boolean"}}), &["path", "old_string", "new_string"]),
        def("bash", "Run a shell command in the project directory. `timeout` in seconds (default 120, max 600).",
            json!({"command": s, "timeout": {"type": "integer"}}), &["command"]),
        def("memory", "Edit persistent memory. `add` appends `text`; `replace` swaps the entry containing `old` for `text`; `remove` deletes the entry containing `old`.",
            json!({"action": {"enum": ["add", "replace", "remove"]}, "scope": {"enum": ["global", "project"]}, "text": s, "old": s}), &["action", "scope"]),
    ];
    if !subagent {
        v.push(def("task", "Run a sub-agent with a fresh context on a self-contained task and get back its final answer. Use it for broad searches or independent work.",
            json!({"prompt": s}), &["prompt"]));
    }
    if !mcp_servers.is_empty() {
        v.push(def("mcp", &format!("Use MCP servers ({}). `search` finds tools by keyword and returns their schemas; `call` runs one.", mcp_servers.join(", ")),
            json!({"action": {"enum": ["search", "call"]}, "query": s, "server": s, "tool": s, "args": {"type": "object"}}), &["action"]));
    }
    v
}

fn str_arg<'a>(input: &'a Value, key: &str) -> Result<&'a str> {
    input[key].as_str().ok_or_else(|| anyhow!("missing string argument `{key}`"))
}

/// Runs read / write / edit / bash / memory.
pub async fn run(name: &str, input: &Value, cwd: &Path) -> Result<String> {
    match name {
        "read" => {
            let path = resolve(cwd, str_arg(input, "path")?);
            let bytes = std::fs::read(&path).with_context(|| path.display().to_string())?;
            let text = String::from_utf8_lossy(&bytes);
            let offset = input["offset"].as_u64().unwrap_or(1).max(1) as usize - 1;
            match input["limit"].as_u64() {
                None if offset == 0 => Ok(text.into_owned()),
                limit => Ok(text.lines().skip(offset).take(limit.unwrap_or(u64::MAX) as usize).collect::<Vec<_>>().join("\n")),
            }
        }
        "write" => {
            let path = resolve(cwd, str_arg(input, "path")?);
            if let Some(p) = path.parent() {
                std::fs::create_dir_all(p)?;
            }
            let content = str_arg(input, "content")?;
            std::fs::write(&path, content).with_context(|| path.display().to_string())?;
            Ok(format!("wrote {} bytes", content.len()))
        }
        "edit" => {
            let path = resolve(cwd, str_arg(input, "path")?);
            let text = std::fs::read_to_string(&path).with_context(|| path.display().to_string())?;
            let (old, new) = (str_arg(input, "old_string")?, str_arg(input, "new_string")?);
            let all = input["replace_all"].as_bool().unwrap_or(false);
            let n = if old.is_empty() { 0 } else { text.matches(old).count() };
            match n {
                0 => bail!("old_string not found in {}", path.display()),
                1 => {}
                n if !all => bail!("old_string matches {n} times; add context to make it unique or set replace_all"),
                _ => {}
            }
            std::fs::write(&path, text.replace(old, new))?;
            Ok(format!("replaced {n} occurrence(s)"))
        }
        "bash" => bash(str_arg(input, "command")?, input["timeout"].as_u64().unwrap_or(120).min(600), cwd).await,
        "memory" => memory(input, cwd),
        other => bail!("unknown tool `{other}`"),
    }
}

async fn bash(command: &str, timeout: u64, cwd: &Path) -> Result<String> {
    // shortcut: kill_on_drop stops the shell, not grandchildren it backgrounded; use a process group if that leaks.
    let child = tokio::process::Command::new("sh")
        .arg("-c")
        .arg(command)
        .current_dir(cwd)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    let out = tokio::time::timeout(Duration::from_secs(timeout), child.wait_with_output())
        .await
        .map_err(|_| anyhow!("timed out after {timeout}s"))??;
    let mut s = String::from_utf8_lossy(&out.stdout).into_owned();
    s.push_str(&String::from_utf8_lossy(&out.stderr));
    if !out.status.success() {
        s.push_str(&format!("\n[exit {}]", out.status.code().map_or("signal".into(), |c| c.to_string())));
        bail!(s);
    }
    Ok(s)
}

/// One entry per line. The size cap forces the model to consolidate instead of growing the prompt.
fn memory(input: &Value, cwd: &Path) -> Result<String> {
    let scope = str_arg(input, "scope")?;
    let path = memory_files(cwd).into_iter().find(|(s, _)| *s == scope).ok_or_else(|| anyhow!("scope must be global or project"))?.1;
    let cur = std::fs::read_to_string(&path).unwrap_or_default();
    let mut lines: Vec<String> = cur.lines().map(String::from).collect();
    let entry = || -> Result<String> { Ok(format!("- {}", str_arg(input, "text")?.replace('\n', " ").trim().trim_start_matches("- "))) };
    let find = |lines: &[String]| -> Result<usize> {
        let old = str_arg(input, "old")?;
        let hits: Vec<usize> = lines.iter().enumerate().filter(|(_, l)| l.contains(old)).map(|(i, _)| i).collect();
        match hits[..] {
            [i] => Ok(i),
            [] => bail!("no entry contains `{old}`"),
            _ => bail!("`{old}` matches {} entries; be more specific", hits.len()),
        }
    };
    match str_arg(input, "action")? {
        "add" => lines.push(entry()?),
        "replace" => {
            let i = find(&lines)?;
            lines[i] = entry()?;
        }
        "remove" => {
            let i = find(&lines)?;
            lines.remove(i);
        }
        a => bail!("unknown action `{a}`"),
    }
    let text = lines.join("\n") + "\n";
    if text.len() > MEMORY_LIMIT {
        bail!("memory is full ({} of {MEMORY_LIMIT} bytes); remove or merge entries first", text.len());
    }
    if let Some(p) = path.parent() {
        std::fs::create_dir_all(p)?;
    }
    std::fs::write(&path, text)?;
    Ok("saved; it will be in context from the next session".into())
}
