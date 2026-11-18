pub mod computer;
mod dap;
mod eval;
pub mod ext;
mod lsp;
pub mod mcp;

use crate::{
    context::{memory_files, MEMORY_LIMIT},
    permission::resolve,
    ToolDef,
};
use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};
use std::{
    path::Path,
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};

/// When set, `bash` may only write inside the project, temp directories and the GreenCode home (macOS).
pub static SANDBOX: AtomicBool = AtomicBool::new(false);

fn def(name: &str, description: &str, props: Value, required: &[&str]) -> ToolDef {
    ToolDef {
        name: name.into(),
        description: description.into(),
        schema: json!({"type": "object", "properties": props, "required": required}),
    }
}

/// The built-in tools. Order and wording are fixed: they sit at the front of the cached prefix.
pub fn defs(leaf: bool, mcp_servers: &[String]) -> Vec<ToolDef> {
    let s = json!({"type": "string"});
    let mut v = vec![
        def("read", "Read a text file. `offset` is the first line (1-based), `limit` the number of lines.",
            json!({"path": s, "offset": {"type": "integer"}, "limit": {"type": "integer"}}), &["path"]),
        def("write", "Create or overwrite a file.", json!({"path": s, "content": s}), &["path", "content"]),
        def("edit", "Replace `old_string` with `new_string`. It must match exactly once unless `replace_all` is true.",
            json!({"path": s, "old_string": s, "new_string": s, "replace_all": {"type": "boolean"}}), &["path", "old_string", "new_string"]),
        def("bash", "Run a shell command in the project. `timeout` seconds (default 120, max 600). `background: true` starts a long-running command and returns its pid and log at once.",
            json!({"command": s, "timeout": {"type": "integer"}, "background": {"type": "boolean"}}), &["command"]),
        def("memory", "Edit persistent memory. `add` appends `text`; `replace` swaps the entry containing `old`; `remove` deletes it.",
            json!({"action": {"enum": ["add", "replace", "remove"]}, "scope": {"enum": ["global", "project"]}, "text": s, "old": s}), &["action", "scope"]),
        def("grep", "Search file contents by regex; returns `file:line:text`. `path` defaults to the project, `glob` filters files (e.g. `*.rs`).",
            json!({"pattern": s, "path": s, "glob": s}), &["pattern"]),
        def("glob", "List files matching a glob such as `src/**/*.rs`.", json!({"pattern": s, "path": s}), &["pattern"]),
        def("web_fetch", "Fetch a URL and return its text (HTML is reduced to readable text).", json!({"url": s}), &["url"]),
        def("web_search", "Search the web; returns titles, URLs and snippets.", json!({"query": s}), &["query"]),
        def("todo", "Show your task list for multi-step work. Send the whole list each time; mark finished items `done`.",
            json!({"todos": {"type": "array", "items": {"type": "object", "properties": {"text": s, "done": {"type": "boolean"}}, "required": ["text"]}}}), &["todos"]),
        lsp::def(),
        ext::def(&[]).expect("ext has tools"),
    ];
    if !leaf {
        v.push(def("task", "Run sub-agents in parallel; each returns its final answer. Every task needs a self-contained `prompt`. `agent`: `task` (all tools), `scout`/`reviewer`/`security` (read-only), or a custom one. `context` is shared; `model` overrides the default; `isolated` uses a worktree; `background` returns at once.",
            json!({"context": s, "tasks": {"type": "array", "items": {"type": "object", "properties": {"prompt": s, "agent": s, "name": s, "model": s, "isolated": {"type": "boolean"}, "schema": {"type": "object", "description": "JSON Schema (type object) for the result; the agent must end by calling submit with it"}}, "required": ["prompt"]}}, "background": {"type": "boolean"}}), &["tasks"]));
    }
    if cfg!(target_os = "macos") {
        v.push(computer::def());
    }
    if !mcp_servers.is_empty() {
        v.push(def("mcp", &format!("Use MCP servers ({}). `search` finds tools by keyword and returns their schemas; `call` runs one.", mcp_servers.join(", ")),
            json!({"action": {"enum": ["search", "call"]}, "query": s, "server": s, "tool": s, "args": {"type": "object"}}), &["action"]));
    }
    v
}

/// Cap for search and listing output; a token budget, not a line count.
const OUTPUT_CHARS: usize = 3_000;
/// Lines `read` returns when no `limit` is given, and the bytes it may return in one call.
const READ_LINES: usize = 400;
const READ_BYTES: usize = 2_500;

pub(crate) fn str_arg<'a>(input: &'a Value, key: &str) -> Result<&'a str> {
    input[key]
        .as_str()
        .ok_or_else(|| anyhow!("missing string argument `{key}`"))
}

/// Tools `run` handles.
pub const BUILTIN: [&str; 11] = [
    "read",
    "write",
    "edit",
    "bash",
    "memory",
    "grep",
    "glob",
    "web_fetch",
    "web_search",
    "todo",
    "lsp",
];

/// The window `read` and `hread` return. Small files come back whole; big ones stop at READ_BYTES and say where to resume.
pub(super) fn view(text: &str, offset: Option<u64>, limit: Option<u64>, numbered: bool) -> String {
    let offset = offset.unwrap_or(1).max(1) as usize - 1;
    let all: Vec<&str> = text.lines().collect();
    let want = limit.map_or(READ_LINES, |n| n as usize);
    let mut out = String::new();
    let mut taken = 0;
    for (i, line) in all.iter().enumerate().skip(offset).take(want) {
        if !out.is_empty() && out.len() + line.len() + 1 > READ_BYTES {
            break;
        }
        if numbered {
            out.push_str(&format!("{}:", i + 1));
        }
        out.push_str(line);
        out.push('\n');
        taken += 1;
    }
    out.pop();
    let left = all.len().saturating_sub(offset + taken);
    // An explicit `limit` that was fully served needs no hint; a byte cut always does.
    let cut = taken < want.min(all.len().saturating_sub(offset));
    if left > 0 && (cut || limit.is_none()) {
        out.push_str(&format!(
            "\n[{left} more lines; read with offset {}]",
            offset + taken + 1
        ));
    }
    out
}

/// Search root relative to `cwd` so every result line is not prefixed with the absolute project path;
/// `None` searches `cwd` itself, which makes ripgrep print bare relative paths.
fn search_path(cwd: &Path, path: Option<&str>) -> Option<std::path::PathBuf> {
    let full = resolve(cwd, path.unwrap_or("."));
    match full.strip_prefix(cwd) {
        Ok(rel) if rel.as_os_str().is_empty() => None,
        Ok(rel) => Some(rel.to_path_buf()),
        Err(_) => Some(full),
    }
}

/// Output of a search command, capped so one broad pattern cannot flood the context.
pub(super) async fn lines(mut cmd: tokio::process::Command, cwd: &Path) -> Result<String> {
    let out = tokio::time::timeout(
        Duration::from_secs(30),
        cmd.current_dir(cwd)
            .stdin(std::process::Stdio::null())
            .kill_on_drop(true)
            .output(),
    )
    .await
    .map_err(|_| anyhow!("timed out after 30s"))??;
    let text = String::from_utf8_lossy(&out.stdout);
    if text.trim().is_empty() {
        // rg and grep exit 1 for "nothing found".
        if out.status.success() || out.status.code() == Some(1) {
            return Ok("no matches".into());
        }
        bail!("{}", String::from_utf8_lossy(&out.stderr).trim());
    }
    // Paths and match lines are long: cap the bytes as well as the line count.
    let all: Vec<&str> = text.lines().collect();
    let mut s = String::new();
    let mut taken = 0;
    for line in &all {
        if taken == 200 || s.len() + line.len() + 1 > OUTPUT_CHARS {
            break;
        }
        s.push_str(line);
        s.push('\n');
        taken += 1;
    }
    if taken < all.len() {
        s.push_str(&format!(
            "[{} more lines; narrow the pattern or path]",
            all.len() - taken
        ));
    }
    Ok(s)
}

/// Readable text of an HTML page: scripts, styles and tags removed.
pub fn html_text(html: &str) -> String {
    let (mut out, mut rest) = (String::new(), html);
    while let Some(i) = rest.find('<') {
        out.push_str(&rest[..i]);
        rest = &rest[i..];
        let name: String = rest[1..]
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '/')
            .collect::<String>()
            .to_ascii_lowercase();
        let end = match name.as_str() {
            "script" | "style" => rest
                .to_ascii_lowercase()
                .find(&format!("</{name}"))
                .and_then(|j| rest[j..].find('>').map(|k| j + k + 1)),
            _ => rest.find('>').map(|j| j + 1),
        };
        if matches!(
            name.trim_start_matches('/'),
            "p" | "div"
                | "br"
                | "li"
                | "tr"
                | "h1"
                | "h2"
                | "h3"
                | "h4"
                | "pre"
                | "section"
                | "article"
        ) {
            out.push('\n');
        }
        match end {
            Some(end) => rest = &rest[end..],
            None => {
                rest = "";
                break;
            }
        }
    }
    out.push_str(rest);
    let out = out
        .replace("&nbsp;", " ")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&#x27;", "'")
        .replace("&amp;", "&");
    out.lines()
        .map(|l| l.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

fn http() -> Result<reqwest::Client> {
    // Some sites refuse clients without a browser-like agent string.
    Ok(reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .user_agent("Mozilla/5.0 (Macintosh) GreenCode")
        .build()?)
}

async fn web_fetch(url: &str) -> Result<String> {
    if !url.starts_with("http://") && !url.starts_with("https://") {
        bail!("url must start with http:// or https://");
    }
    let resp = http()?.get(url).send().await?.error_for_status()?;
    let html = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|t| t.contains("html"));
    let body = resp.text().await?;
    Ok(if html { html_text(&body) } else { body })
}

/// Results from DuckDuckGo's HTML page. shortcut: scraped markup, no API key; switch to a search API if the page layout changes.
pub fn search_results(html: &str) -> String {
    let mut out = vec![];
    for chunk in html.split("class=\"result__a\"").skip(1).take(8) {
        let Some(href) = chunk
            .split("href=\"")
            .nth(1)
            .and_then(|s| s.split('"').next())
        else {
            continue;
        };
        let href = href.replace("&amp;", "&");
        // Links are redirects carrying the real address in `uddg`.
        let url = reqwest::Url::parse(&format!("https:{href}"))
            .ok()
            .filter(|_| href.starts_with("//"))
            .and_then(|u| {
                u.query_pairs()
                    .find(|(k, _)| k == "uddg")
                    .map(|(_, v)| v.into_owned())
            })
            .unwrap_or(href);
        let title = chunk
            .split_once('>')
            .map(|(_, s)| s.split("</a>").next().unwrap_or(""))
            .unwrap_or("");
        let snippet = chunk
            .split("class=\"result__snippet\"")
            .nth(1)
            .and_then(|s| s.split_once('>'))
            .map(|(_, s)| s.split("</a>").next().unwrap_or(""))
            .unwrap_or("");
        out.push(format!(
            "{}\n{url}\n{}",
            html_text(title),
            html_text(snippet)
        ));
    }
    if out.is_empty() {
        "no results".into()
    } else {
        out.join("\n\n")
    }
}

/// Runs read / write / edit / bash / memory / grep / glob / web_fetch / web_search / todo.
pub async fn run(name: &str, input: &Value, cwd: &Path) -> Result<String> {
    match name {
        "lsp" => lsp::run(cwd, input).await,
        "read" => {
            let path = resolve(cwd, str_arg(input, "path")?);
            let bytes = std::fs::read(&path).with_context(|| path.display().to_string())?;
            let text = String::from_utf8_lossy(&bytes);
            Ok(view(
                &text,
                input["offset"].as_u64(),
                input["limit"].as_u64(),
                false,
            ))
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
            let text =
                std::fs::read_to_string(&path).with_context(|| path.display().to_string())?;
            let (old, new) = (str_arg(input, "old_string")?, str_arg(input, "new_string")?);
            let all = input["replace_all"].as_bool().unwrap_or(false);
            let n = if old.is_empty() {
                0
            } else {
                text.matches(old).count()
            };
            match n {
                0 => bail!("old_string not found in {}", path.display()),
                1 => {}
                n if !all => bail!("old_string matches {n} times; add context to make it unique or set replace_all"),
                _ => {}
            }
            std::fs::write(&path, text.replace(old, new))?;
            Ok(format!("replaced {n} occurrence(s)"))
        }
        "bash" => {
            bash(
                str_arg(input, "command")?,
                input["timeout"].as_u64().unwrap_or(120).min(600),
                input["background"] == true,
                cwd,
            )
            .await
        }
        "grep" => {
            let (pattern, path, glob) = (
                str_arg(input, "pattern")?,
                search_path(cwd, input["path"].as_str()),
                input["glob"].as_str(),
            );
            let cmd = match crate::config::executable("rg") {
                Some(rg) => {
                    let mut c = tokio::process::Command::new(rg);
                    c.args(["-n", "--no-heading", "--color", "never"]);
                    if let Some(g) = glob {
                        c.args(["-g", g]);
                    }
                    c.args(["-e", pattern, "--"]).args(&path);
                    c
                }
                None => {
                    let mut c = tokio::process::Command::new("grep");
                    c.args([
                        "-rnIE",
                        "--exclude-dir=.git",
                        "--exclude-dir=target",
                        "--exclude-dir=node_modules",
                    ]);
                    if let Some(g) = glob {
                        c.arg(format!("--include={g}"));
                    }
                    c.args(["-e", pattern, "--"]).arg(path.as_deref().unwrap_or(Path::new(".")));
                    c
                }
            };
            lines(cmd, cwd).await
        }
        "glob" => {
            let (pattern, path) = (
                str_arg(input, "pattern")?,
                search_path(cwd, input["path"].as_str()),
            );
            let mut c = match crate::config::executable("rg") {
                Some(rg) => {
                    let mut c = tokio::process::Command::new(rg);
                    c.args(["--files", "-g", pattern]).args(&path);
                    c
                }
                // shortcut: without ripgrep only the file-name part of the pattern is matched.
                None => {
                    let mut c = tokio::process::Command::new("find");
                    c.arg(path.as_deref().unwrap_or(Path::new("."))).args([
                        "-type",
                        "f",
                        "-name",
                        pattern.rsplit('/').next().unwrap_or(pattern),
                        "-not",
                        "-path",
                        "*/.git/*",
                    ]);
                    c
                }
            };
            c.stderr(std::process::Stdio::piped());
            lines(c, cwd).await
        }
        "web_fetch" => web_fetch(str_arg(input, "url")?).await,
        "web_search" => {
            let url = reqwest::Url::parse_with_params(
                "https://html.duckduckgo.com/html/",
                [("q", str_arg(input, "query")?)],
            )?;
            Ok(search_results(
                &http()?
                    .get(url)
                    .send()
                    .await?
                    .error_for_status()?
                    .text()
                    .await?,
            ))
        }
        "todo" => {
            let todos = input["todos"]
                .as_array()
                .ok_or_else(|| anyhow!("`todos` must be an array"))?;
            Ok(todos
                .iter()
                .map(|t| {
                    format!(
                        "{} {}",
                        if t["done"] == true { "☑" } else { "☐" },
                        t["text"].as_str().unwrap_or("")
                    )
                })
                .collect::<Vec<_>>()
                .join("\n"))
        }
        "memory" => memory(input, cwd),
        other => bail!("unknown tool `{other}`"),
    }
}

/// `sh -c`, wrapped in a write-restricting sandbox when `SANDBOX` is on.
pub(super) fn shell(command: &str, cwd: &Path) -> Result<tokio::process::Command> {
    if !SANDBOX.load(Ordering::Relaxed) {
        let mut c = tokio::process::Command::new("sh");
        c.arg("-c").arg(command);
        return Ok(c);
    }
    if !cfg!(target_os = "macos") {
        bail!("the bash sandbox is only available on macOS; turn `sandbox` off");
    }
    let allowed = [
        cwd.canonicalize()?,
        crate::home(),
        "/private/tmp".into(),
        "/private/var/folders".into(),
        "/dev".into(),
    ];
    let paths = allowed
        .iter()
        .map(|p| {
            let p = p.to_string_lossy();
            if p.contains('"') {
                bail!("sandbox: unsupported path {p}");
            }
            Ok(format!("(subpath \"{p}\")"))
        })
        .collect::<Result<Vec<_>>>()?
        .join(" ");
    // shortcut: writes only; reads and network stay open. A worktree's parent `.git` is outside the project, so commits there need the sandbox off.
    let mut c = tokio::process::Command::new("/usr/bin/sandbox-exec");
    c.arg("-p")
        .arg(format!(
            "(version 1)(allow default)(deny file-write*)(allow file-write* {paths})"
        ))
        .args(["sh", "-c", command]);
    Ok(c)
}

async fn bash(command: &str, timeout: u64, background: bool, cwd: &Path) -> Result<String> {
    if background {
        let dir = crate::home().join("jobs");
        std::fs::create_dir_all(&dir)?;
        let log = dir.join(format!(
            "{}.log",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_micros()
        ));
        let file = std::fs::File::create(&log)?;
        let mut child = shell(command, cwd)?
            .current_dir(cwd)
            .stdin(std::process::Stdio::null())
            .stdout(file.try_clone()?)
            .stderr(file)
            .spawn()?;
        let pid = child.id().unwrap_or(0);
        tokio::spawn(async move {
            let _ = child.wait().await;
        }); // reap it when it ends
        return Ok(format!("started in the background, pid {pid}. Output: {} (use `read`). Stop it with `kill {pid}`.", log.display()));
    }
    // shortcut: kill_on_drop stops the shell, not grandchildren it backgrounded; use a process group if that leaks.
    let child = shell(command, cwd)?
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
        s.push_str(&format!(
            "\n[exit {}]",
            out.status.code().map_or("signal".into(), |c| c.to_string())
        ));
        // A missing interpreter is common (`python`, `pytest`); say so once instead of letting the model retry blind.
        if out.status.code() == Some(127) {
            s.push_str("\n[command not found: check the name, or try python3, node, cargo]");
        }
        bail!(s);
    }
    Ok(s)
}

/// One entry per line. The size cap forces the model to consolidate instead of growing the prompt.
fn memory(input: &Value, cwd: &Path) -> Result<String> {
    let scope = str_arg(input, "scope")?;
    let path = memory_files(cwd)
        .into_iter()
        .find(|(s, _)| *s == scope)
        .ok_or_else(|| anyhow!("scope must be global or project"))?
        .1;
    let cur = std::fs::read_to_string(&path).unwrap_or_default();
    let mut lines: Vec<String> = cur.lines().map(String::from).collect();
    let entry = || -> Result<String> {
        Ok(format!(
            "- {}",
            str_arg(input, "text")?
                .replace('\n', " ")
                .trim()
                .trim_start_matches("- ")
        ))
    };
    let find = |lines: &[String]| -> Result<usize> {
        let old = str_arg(input, "old")?;
        let hits: Vec<usize> = lines
            .iter()
            .enumerate()
            .filter(|(_, l)| l.contains(old))
            .map(|(i, _)| i)
            .collect();
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
        bail!(
            "memory is full ({} of {MEMORY_LIMIT} bytes); remove or merge entries first",
            text.len()
        );
    }
    if let Some(p) = path.parent() {
        std::fs::create_dir_all(p)?;
    }
    std::fs::write(&path, text)?;
    Ok("saved; it will be in context from the next session".into())
}
