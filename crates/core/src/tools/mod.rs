pub mod archive;
pub mod browser;
pub mod computer;
mod dap;
pub mod edit;
mod eval;
pub mod ext;
pub mod fabricated;
pub mod fetch;
pub mod find_verify;
pub mod github_cache;
pub mod jobwidget;
pub mod lsp;
pub mod mcp;
pub mod media;
pub mod procs;
pub mod ratchet;
pub mod security;
pub mod speculate;
mod minimizer;
mod summary;
pub mod vault;

use crate::{
    config::Config,
    context::{memory_files, MEMORY_LIMIT},
    permission::resolve,
    ToolDef,
};
use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};
use std::{
    collections::{BTreeSet, HashMap},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        LazyLock, Mutex, RwLock,
    },
    time::Duration,
};

/// When set, `bash` may only write inside the project, temp directories and the GreenCode home (macOS).
pub static SANDBOX: AtomicBool = AtomicBool::new(false);

/// The turn currently running a tool: what an `eval` cell's `agent()`/`workpool()` inherit.
// shortcut: one slot for the whole process, so concurrent agents overwrite it; subagent turns share the same UI
// channel and never a broader mode, which is all a pool needs.
static AGENT_CONTEXT: LazyLock<std::sync::RwLock<Option<(crate::session::Meta, crate::permission::Mode, crate::EventTx)>>> =
    LazyLock::new(Default::default);

/// Records the running turn so eval host functions can spawn subagents with its mode and UI.
pub fn set_agent_context(meta: &crate::session::Meta, mode: crate::permission::Mode, tx: &crate::EventTx) {
    *AGENT_CONTEXT.write().unwrap() = Some((meta.clone(), mode, tx.clone()));
}

pub(crate) fn agent_context() -> Option<(crate::session::Meta, crate::permission::Mode, crate::EventTx)> {
    AGENT_CONTEXT.read().unwrap().clone()
}

/// Whether `name` is a tool a cell defined (`eval.tools.enabled`).
pub fn eval_is_tool(name: &str) -> bool {
    eval::is_tool(name)
}

/// Runs an eval-defined tool; the kernel keeps whatever state its cell left.
pub async fn eval_call_tool(name: &str, args: &Value, cfg: &Config) -> Result<String> {
    eval::call_tool(name, args, cfg).await
}

/// The active configuration for tool execution; set when an agent starts.
static CONFIG: RwLock<Option<std::sync::Arc<Config>>> = RwLock::new(None);
static DEFAULT_CONFIG: LazyLock<std::sync::Arc<Config>> = LazyLock::new(|| std::sync::Arc::new(Config::default()));

pub fn configure(cfg: &Config) {
    *CONFIG.write().unwrap() = Some(std::sync::Arc::new(cfg.clone()));
    lsp::configure(cfg);
}

/// `edit.streamingAbort`: the project root and flag a provider needs while it accumulates an edit call.
static STREAM_GUARD: RwLock<Option<(PathBuf, bool)>> = RwLock::new(None);

/// Records the session a provider's streamed edit preview resolves against; later sessions win.
pub fn set_stream_guard(cwd: &Path, cfg: &Config) {
    *STREAM_GUARD.write().unwrap() = Some((cwd.to_path_buf(), cfg.file_opts.edit.streaming_abort));
}

/// Called by providers on each streamed tool-call argument delta: `Some(reason)` when the partial `edit`
/// call already fails its preview, so the reply can be abandoned early.
pub fn stream_edit_abort(name: &str, partial: &str) -> Option<String> {
    if name != "edit" {
        return None;
    }
    let guard = STREAM_GUARD.read().unwrap().clone();
    let (cwd, enabled) = guard?;
    enabled.then(|| edit::stream_edit_error(&cwd, partial)).flatten()
}

pub(crate) fn config() -> std::sync::Arc<Config> {
    CONFIG.read().unwrap().as_ref().cloned().unwrap_or_else(|| DEFAULT_CONFIG.clone())
}

/// Completed background jobs whose result print mode has not delivered yet.
#[derive(Clone, Debug)]
pub struct JobResult {
    pub command: String,
    pub log: PathBuf,
    pub status: Option<i32>,
}

static JOBS_RUNNING: AtomicUsize = AtomicUsize::new(0);
static JOBS_DONE: Mutex<Vec<JobResult>> = Mutex::new(Vec::new());
static JOB_NOTIFY: LazyLock<tokio::sync::Notify> = LazyLock::new(tokio::sync::Notify::new);

/// Running background jobs, including auto-backgrounded commands.
pub fn running_jobs() -> usize {
    JOBS_RUNNING.load(Ordering::SeqCst)
}

/// Running jobs as `(pid, command, log)`, for the wait card, the jobs pill and the jobs sheet.
static JOBS_LIVE: Mutex<Vec<(u32, String, PathBuf)>> = Mutex::new(Vec::new());

pub fn running_job_list() -> Vec<(u32, String, PathBuf)> {
    JOBS_LIVE.lock().unwrap().clone()
}

/// The next finished job, waiting while any job still runs; `None` when none is left.
pub async fn next_job_result() -> Option<JobResult> {
    loop {
        if let Some(job) = JOBS_DONE.lock().unwrap().pop() {
            return Some(job);
        }
        if JOBS_RUNNING.load(Ordering::SeqCst) == 0 {
            return None;
        }
        JOB_NOTIFY.notified().await;
    }
}

fn job_finished(result: JobResult) {
    jobwidget::finish(&result.log);
    JOBS_DONE.lock().unwrap().push(result);
    JOBS_RUNNING.fetch_sub(1, Ordering::SeqCst);
    JOB_NOTIFY.notify_one();
}

/// `externalThinking`: provider reasoning is off, so a private scratchpad tool carries thinking.
pub fn think_def() -> ToolDef {
    def(
        "think",
        "Record a private thought you will act on next. Never shown to the user; returns nothing.",
        json!({"thought": {"type": "string"}}),
        &["thought"],
    )
}

/// `inlineToolDescriptors`: every descriptor as prompt text, rendered once.
pub fn descriptors(tools: &[ToolDef]) -> String {
    use std::fmt::Write as _;
    let mut s = String::new();
    for t in tools {
        let _ = writeln!(s, "\n## {}\n{}\nSchema: {}", t.name, t.description, t.schema);
    }
    s
}

/// `inlineToolDescriptors`: the descriptions moved into the prompt, so the schemas drop theirs.
pub fn strip_descriptions(tools: &mut [ToolDef]) {
    fn scrub(v: &mut serde_json::Value) {
        match v {
            serde_json::Value::Object(map) => {
                map.remove("description");
                for value in map.values_mut() {
                    scrub(value);
                }
            }
            serde_json::Value::Array(items) => items.iter_mut().for_each(scrub),
            _ => {}
        }
    }
    for tool in tools {
        tool.description.clear();
        scrub(&mut tool.schema);
    }
}

fn def(name: &str, description: &str, props: Value, required: &[&str]) -> ToolDef {
    ToolDef {
        name: name.into(),
        description: description.into(),
        schema: json!({"type": "object", "properties": props, "required": required}),
    }
}

/// `bash`'s description: a `name` runs a named service whose `proc://` log and stdin stay addressable.
fn bash_description(cfg: &Config) -> String {
    let mut text = String::from("Run a shell command in the project. `timeout` seconds (default 120, max 600). `background: true` starts a long-running command and returns its pid and log at once.");
    if cfg.extra_tools_opts.launch.enabled {
        text.push_str(" `name` starts a named service instead: `read proc://<name>` shows its log and status, `write proc://<name>` sends stdin, `write proc://<name>/kill` stops it.");
    }
    text
}

/// `edit.mode`: the edit tool for this config, or none when the mode removes it.
fn edit_def(cfg: &Config) -> Option<ToolDef> {
    let s = json!({"type": "string"});
    match cfg.file_opts.edit.mode {
        crate::config::files::EditMode::Replace => Some(def("edit", "Replace `old_string` with `new_string`. It must match exactly once unless `replace_all` is true.",
            json!({"path": s, "old_string": s, "new_string": s, "replace_all": {"type": "boolean"}}), &["path", "old_string", "new_string"])),
        crate::config::files::EditMode::Magic => Some(def("edit", "Change a file by describing the change in words; a fast model writes the new content and applies it. `change` is that description.",
            json!({"path": s, "change": s}), &["path", "change"])),
        crate::config::files::EditMode::None => None,
    }
}

/// The built-in tools. Order and wording are fixed: they sit at the front of the cached prefix.
pub fn defs(leaf: bool, mcp_servers: &[String]) -> Vec<ToolDef> {
    defs_for(&config(), leaf, mcp_servers)
}

/// Builds the catalog from the requesting session's config, not the last session to start.
pub fn defs_for(cfg: &Config, leaf: bool, mcp_servers: &[String]) -> Vec<ToolDef> {
    let s = json!({"type": "string"});
    let mut v = vec![
        def("read", "Read a text file. `offset` is the first line (1-based), `limit` the number of lines. Without either, long source files may come back as a structural summary whose `…` spans hold hidden lines.",
            json!({"path": s, "offset": {"type": "integer"}, "limit": {"type": "integer"}}), &["path"]),
        def("write", "Create or overwrite a file.", json!({"path": s, "content": s}), &["path", "content"]),
        def("bash", &bash_description(cfg), json!({"command": s, "name": s, "timeout": {"type": "integer"}, "background": {"type": "boolean"}}), &["command"]),
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
        ext::def(cfg, &[]).expect("ext has tools"),
    ];
    if let Some(edit) = edit_def(cfg) {
        v.insert(2, edit);
    }
    if cfg.extra_tools_opts.generate_image.enabled {
        v.push(def("generate_image", "Generate or edit an image with the configured images provider. `prompt` describes it; `image` is a path to edit; `size` defaults to generate_image.size. Returns the saved image path, which joins the conversation as an image.",
            json!({"prompt": s, "image": s, "size": s, "model": s, "format": s, "n": {"type": "integer"}}), &["prompt"]));
    }

    if !leaf {
        v.push(def("task", "Run sub-agents in parallel; each returns its final answer. Every task needs a self-contained `prompt`. `agent`: `task` (all tools), `scout`/`reviewer`/`security` (read-only), or a custom one. `context` is shared; `model` overrides the default; `backend` runs it on another harness (`codex`, `claude`, `pi`, ...) with that harness's tools; `isolated` uses a worktree; `background` returns at once.",
            json!({"context": s, "tasks": {"type": "array", "items": {"type": "object", "properties": {"prompt": s, "steps": {"type": "array", "items": s, "description": "Ordered steps instead of one prompt (`task.steps`); each next step starts after the previous finishes, and the last step's result is the task's result"}, "agent": s, "name": s, "model": s, "backend": {"type": "string", "description": "A configured backend name; `model` is then that backend's model id"}, "isolated": {"type": "boolean"}, "schema": {"type": "object", "description": "JSON Schema (type object) for the result; the agent must end by calling submit with it"}}}}, "background": {"type": "boolean"}}), &["tasks"]));
    }
    if cfg!(target_os = "macos") {
        v.push(computer::def());
    }
    v.push(browser::def());
    if !mcp_servers.is_empty() {
        v.push(def("mcp", &format!("Use MCP servers ({}). `search` finds tools by keyword and returns their schemas; `call` runs one.", mcp_servers.join(", ")),
            json!({"action": {"enum": ["search", "call"]}, "query": s, "server": s, "tool": s, "args": {"type": "object"}}), &["action"]));
    }
    if cfg.bash_opts.out.enabled {
        if let Some(write) = v.iter_mut().find(|tool| tool.name == "write") {
            write.description.push_str(" `path` may be `out://` to reply to the user mid-task (64 KiB cap); it does not end the turn.");
        }
    }
    if cfg.bash_opts.intent_tracing {
        for tool in &mut v {
            if let Some(properties) = tool.schema["properties"].as_object_mut() {
                properties.insert(
                    "intent".into(),
                    json!({"type": "string", "description": "One short sentence: why you are calling this tool now."}),
                );
            }
        }
    }
    // `eval.tools.enabled`: tools a cell defined are advertised like built-ins, so subagents see them too.
    if cfg.integration_opts.eval.tools.enabled {
        v.extend(eval::tool_defs());
    }
    v
}

/// Cap for search and listing output; a token budget, not a line count.
const OUTPUT_CHARS: usize = 3_000;
/// Default window for `hread`; `read` takes the same value from `read.default_limit`.
pub(crate) const READ_LINES: usize = 400;
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

/// Every name the `tools` allowlist can hold: `BUILTIN` plus the tools `defs` adds around them.
const ALL: [&str; 17] = [
    "read", "write", "edit", "bash", "memory", "grep", "glob", "web_fetch", "web_search", "todo", "lsp",
    "ext", "task", "computer", "mcp", "browser", "generate_image",
];

/// Whether the `tools` allowlist exposes `name` (empty means every tool).
pub fn tool_on(tools: &[String], name: &str) -> bool {
    tools.is_empty() || tools.iter().any(|t| t == name)
}

/// The `tools` allowlist with `name` turned on or off; empty again once every tool is on.
pub fn toggle_tool(tools: &[String], name: &str, on: bool) -> Vec<String> {
    let mut out: Vec<String> = if tools.is_empty() { ALL.iter().map(|t| t.to_string()).collect() } else { tools.to_vec() };
    out.retain(|t| t != name);
    if on {
        out.push(name.into());
    }
    if ALL.iter().all(|a| out.iter().any(|t| t == a)) { Vec::new() } else { out }
}

/// The window `read` and `hread` return. Small files come back whole; big ones stop at READ_BYTES and say where to resume.
pub(super) fn view(text: &str, offset: Option<u64>, limit: Option<u64>, numbered: bool) -> String {
    view_window(text, offset, limit, numbered, READ_LINES).0
}

/// `view` plus the 1-based first line and the count actually included, which is what a read saw.
fn view_window(
    text: &str,
    offset: Option<u64>,
    limit: Option<u64>,
    numbered: bool,
    default_limit: usize,
) -> (String, usize, usize) {
    let offset = offset.unwrap_or(1).max(1) as usize - 1;
    let all: Vec<&str> = text.lines().collect();
    let want = limit.map_or(default_limit.max(1), |n| n as usize);
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
    (out, offset + 1, taken)
}

/// Lines this process displayed per file; `edit.enforce_seen_lines` refuses anchors outside it.
static SEEN_LINES: LazyLock<Mutex<HashMap<PathBuf, BTreeSet<usize>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn record_seen(path: &Path, lines: impl IntoIterator<Item = usize>) {
    let Ok(path) = std::fs::canonicalize(path) else { return };
    SEEN_LINES
        .lock()
        .unwrap()
        .entry(path)
        .or_default()
        .extend(lines);
}

fn shown_lines(path: &Path) -> BTreeSet<usize> {
    let Ok(path) = std::fs::canonicalize(path) else { return BTreeSet::new() };
    SEEN_LINES.lock().unwrap().get(&path).cloned().unwrap_or_default()
}

/// Match rows are `path:line:text`; context rows (`path-line-text`) are left unrecorded, so an edit there is
/// refused until the file is read rather than admitted on a guess.
fn record_grep_seen(out: &str, cwd: &Path) {
    let mut by_file: HashMap<PathBuf, BTreeSet<usize>> = HashMap::new();
    for row in out.lines() {
        let Some((file, rest)) = row.split_once(':') else { continue };
        let Some((line, _)) = rest.split_once(':') else { continue };
        let Ok(line) = line.parse::<usize>() else { continue };
        by_file.entry(resolve(cwd, file)).or_default().insert(line);
    }
    for (path, lines) in by_file {
        record_seen(&path, lines);
    }
}

/// 1-based inclusive lines covered by a byte span; a span ending in a newline ends on its last content line.
fn touched_lines(text: &str, (start, end): (usize, usize)) -> std::ops::RangeInclusive<usize> {
    let line = |byte: usize| text.as_bytes()[..byte].iter().filter(|&&b| b == b'\n').count() + 1;
    line(start)..=line(end.saturating_sub(1).max(start))
}

/// Generator banners from protoc, sqlc, swagger and friends; editing those is usually wasted work.
fn looks_generated(text: &str) -> bool {
    let limit = text.len().min(4096);
    let mut end = limit;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    let head = text[..end].to_ascii_lowercase();
    head.contains("code generated by")
        || head.contains("automatically generated")
        || head.contains("@generated")
        || head.contains("this file is generated")
        || (head.contains("auto-generated")
            && (head.contains("do not edit") || head.contains("do not modify")))
}

/// One space between tokens, one newline between lines: indentation and layout stop mattering.
fn normalize(text: &str) -> String {
    text.lines()
        .map(|line| line.split_whitespace().collect::<Vec<_>>().join(" "))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Dice coefficient over character bigrams: near-copies score close to 1, unrelated text close to 0.
fn similarity(a: &str, b: &str) -> f64 {
    if a == b {
        return 1.0;
    }
    let (a, b): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
    if a.len() < 2 || b.len() < 2 {
        return 0.0;
    }
    let mut counts: HashMap<(char, char), usize> = HashMap::new();
    for pair in a.windows(2) {
        *counts.entry((pair[0], pair[1])).or_default() += 1;
    }
    let mut common = 0;
    for pair in b.windows(2) {
        if let Some(left) = counts.get_mut(&(pair[0], pair[1])) {
            if *left > 0 {
                *left -= 1;
                common += 1;
            }
        }
    }
    2.0 * common as f64 / (a.len() + b.len() - 2) as f64
}

/// The unique whole-line window that is close enough to `old`; several candidates are an error, never a guess.
fn fuzzy_span(text: &str, old: &str, threshold: f64) -> Result<Option<(usize, usize)>> {
    let threshold = if threshold.is_finite() { threshold.clamp(0.0, 1.0) } else { 0.95 };
    let old = normalize(old);
    if old.is_empty() {
        return Ok(None);
    }
    let lines = old.lines().count();
    let mut found = vec![];
    let (mut start, mut spans) = (0, vec![]);
    for (i, byte) in text.bytes().enumerate() {
        if byte == b'\n' {
            spans.push((start, i));
            start = i + 1;
        }
    }
    spans.push((start, text.len()));
    if spans.len() < lines {
        return Ok(None);
    }
    for i in 0..=spans.len() - lines {
        let (start, end) = (spans[i].0, spans[i + lines - 1].1);
        if similarity(&normalize(&text[start..end]), &old) >= threshold {
            found.push((start, end));
        }
    }
    match found.as_slice() {
        [] => Ok(None),
        [span] => Ok(Some(*span)),
        many => bail!(
            "old_string is absent but {} fuzzy matches reach {threshold}; add more context to make it unique",
            many.len()
        ),
    }
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

/// Latest instant Exa's documented 10 requests/second budget allows a new request.
static EXA_NEXT: LazyLock<Mutex<std::time::Instant>> =
    LazyLock::new(|| Mutex::new(std::time::Instant::now()));

async fn exa_pace(delay_ms: u64) {
    if delay_ms == 0 {
        return;
    }
    let wait = {
        let mut next = EXA_NEXT.lock().unwrap();
        let now = std::time::Instant::now();
        let wait = next.saturating_duration_since(now);
        *next = now.max(*next) + Duration::from_millis(delay_ms);
        wait
    };
    if !wait.is_zero() {
        tokio::time::sleep(wait).await;
    }
}

fn exa_key() -> Option<String> {
    std::env::var("EXA_API_KEY").ok().map(|key| key.trim().to_string()).filter(|key| !key.is_empty())
}

fn searxng_results(v: &Value) -> String {
    let results: Vec<String> = v["results"]
        .as_array()
        .into_iter()
        .flatten()
        .take(8)
        .filter_map(|r| {
            let url = r["url"].as_str()?;
            let title = html_text(r["title"].as_str().unwrap_or(""));
            let snippet = html_text(r["content"].as_str().or_else(|| r["snippet"].as_str()).unwrap_or(""));
            Some(format!("{title}\n{url}\n{snippet}"))
        })
        .collect();
    if results.is_empty() {
        "no results".into()
    } else {
        results.join("\n\n")
    }
}

async fn searxng_search(cfg: &crate::config::provider::Searxng, query: &str, http: &reqwest::Client) -> Result<String> {
    let endpoint = cfg.endpoint.as_deref().map(str::trim).filter(|e| !e.is_empty());
    let Some(endpoint) = endpoint else { bail!("searxng.endpoint is unset") };
    let mut url = reqwest::Url::parse(&format!("{}/search", endpoint.trim_end_matches('/')))?;
    {
        let mut pairs = url.query_pairs_mut();
        pairs.append_pair("q", query).append_pair("format", "json");
        for (key, value) in [
            ("categories", &cfg.categories),
            ("engines", &cfg.engines),
            ("language", &cfg.language),
        ] {
            if let Some(value) = value.as_deref().filter(|v| !v.trim().is_empty()) {
                pairs.append_pair(key, value);
            }
        }
        if let Some(safesearch) = cfg.safesearch {
            pairs.append_pair("safesearch", &safesearch.to_string());
        }
    }
    let mut req = http.get(url).header("Accept", "application/json");
    let user = cfg.basic_username.as_deref().filter(|s| !s.is_empty());
    let pass = cfg.basic_password.as_deref().filter(|s| !s.is_empty());
    req = match (user, pass) {
        (Some(user), Some(pass)) => {
            anyhow::ensure!(!user.contains(':'), "searxng.basic_username can contain only the user part; put the password in basic_password");
            req.basic_auth(user, Some(pass))
        }
        (None, None) => match cfg.token.as_deref().filter(|s| !s.is_empty()) {
            Some(token) => req.bearer_auth(token),
            None => req,
        },
        _ => bail!("searxng basic auth needs both basic_username and basic_password"),
    };
    let v: Value = req.send().await?.error_for_status()?.json().await?;
    Ok(searxng_results(&v))
}

fn exa_results(v: &Value) -> String {
    let results: Vec<String> = v["results"]
        .as_array()
        .into_iter()
        .flatten()
        .take(8)
        .filter_map(|r| {
            let url = r["url"].as_str()?;
            let title = r["title"].as_str().unwrap_or("");
            let snippet = r["summary"].as_str().or_else(|| r["text"].as_str()).unwrap_or("");
            let snippet = if snippet.is_empty() {
                r["highlights"].as_array().and_then(|h| h.first()).and_then(Value::as_str).unwrap_or("")
            } else {
                snippet
            };
            Some(format!("{title}\n{url}\n{}", snippet.chars().take(500).collect::<String>()))
        })
        .collect();
    if results.is_empty() {
        "no results".into()
    } else {
        results.join("\n\n")
    }
}

async fn exa_search(cfg: &crate::config::provider::Exa, key: &str, query: &str, http: &reqwest::Client) -> Result<String> {
    exa_pace(cfg.search_delay_ms).await;
    let body = json!({
        "query": query,
        "numResults": 8,
        "type": "auto",
        "contents": {"summary": {"query": query}},
    });
    let v: Value = http
        .post("https://api.exa.ai/search")
        .header("x-api-key", key)
        .json(&body)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    Ok(exa_results(&v))
}

/// SearXNG and Exa when configured, otherwise DuckDuckGo's HTML page.
async fn web_search(query: &str, all: &crate::config::Config) -> Result<String> {
    let cfg = &all.provider_opts.web;
    let http = reqwest::Client::builder()
        .timeout(cfg.search_timeout())
        .user_agent("Mozilla/5.0 (Macintosh) GreenCode")
        .build()?;
    let mut failures = vec![];
    if cfg.searxng.endpoint.as_deref().is_some_and(|e| !e.trim().is_empty()) {
        match searxng_search(&cfg.searxng, query, &http).await {
            Ok(results) => return Ok(results),
            Err(e) => failures.push(format!("SearXNG: {e:#}")),
        }
    }
    if let (true, Some(key)) = (cfg.exa.enabled, exa_key()) {
        match exa_search(&cfg.exa, &key, query, &http).await {
            Ok(results) => return Ok(results),
            Err(e) => failures.push(format!("Exa: {e:#}")),
        }
    }
    if !fetch::keyless_allowed(all) {
        return if failures.is_empty() { bail!(fetch::KEYLESS_OFF) } else { bail!("{} ({})", fetch::KEYLESS_OFF, failures.join("; ")) };
    }
    let url = reqwest::Url::parse_with_params("https://html.duckduckgo.com/html/", [("q", query)])?;
    let html = http
        .get(url)
        .send()
        .await
        .and_then(reqwest::Response::error_for_status);
    match html {
        Ok(resp) => Ok(search_results(&resp.text().await?)),
        Err(e) if failures.is_empty() => Err(e.into()),
        Err(e) => bail!("{e} ({})", failures.join("; ")),
    }
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
    // No session config means no language server: keep plain writes free of background servers.
    let mut cfg = crate::config::Config::default();
    cfg.integration_opts.lsp.format_on_write = false;
    cfg.integration_opts.lsp.diagnostics_on_write = false;
    cfg.integration_opts.lsp.diagnostics_on_edit = false;
    run_with(name, input, cwd, &cfg).await
}

/// `run` with the session's settings. The agent calls this; plain `run` is for callers with no config.
pub async fn run_with(
    name: &str,
    input: &Value,
    cwd: &Path,
    cfg: &crate::config::Config,
) -> Result<String> {
    match name {
        "lsp" => lsp::run(cwd, input).await,
        "browser" => {
            let (text, screenshot) = browser::run(cfg, cwd, input).await?;
            Ok(match screenshot {
                Some(path) => format!("{text}\n[image: {}]", path.display()),
                None => text,
            })
        }
        "read" => {
            let raw = str_arg(input, "path")?;
            if let Some(rest) = raw.strip_prefix("proc://") {
                return procs::read(cfg, rest);
            }
            if let Some(rest) = raw.strip_prefix("vault://") {
                return vault::read(cfg, rest);
            }
            if let Some(rest) = raw.strip_prefix("security://") {
                return security::read(&crate::home(), cwd, rest);
            }
            // `compaction.experimentalContextManagement`: the raw history of a session stays searchable.
            if let Some(id) = raw.strip_prefix("history://") {
                let path = crate::notebook::history_path(id.trim())?;
                let text = std::fs::read_to_string(&path).with_context(|| path.display().to_string())?;
                return Ok(crate::context::cap_columns(&text, cfg.bash_opts.output_max_columns));
            }
            let path = resolve(cwd, raw);
            if let Some(text) = crate::disasm::read(cfg, &path, input["offset"].as_u64(), input["limit"].as_u64()).await? {
                return Ok(crate::context::cap_columns(&text, cfg.bash_opts.output_max_columns));
            }
            let bytes = std::fs::read(&path).with_context(|| path.display().to_string())?;
            let text = String::from_utf8_lossy(&bytes);
            let numbered = cfg.file_opts.read.line_numbers;
            let selector = input["offset"].as_u64().is_some() || input["limit"].as_u64().is_some();
            if !selector {
                let read = minimizer::outline_cfg(&cfg.file_opts.read, cfg.bash_opts.shell_minimizer.source_outline_level);
                if let Some(summary) = summary::try_summary(&text, &path.display().to_string(), &read) {
                    if cfg.file_opts.edit.enforce_seen_lines {
                        record_seen(&path, summary.kept.iter().copied());
                    }
                    return Ok(crate::context::cap_columns(&summary.body, cfg.bash_opts.output_max_columns));
                }
            }
            let (body, first, shown) = view_window(
                &text,
                input["offset"].as_u64(),
                input["limit"].as_u64(),
                numbered,
                cfg.file_opts.read.default_limit,
            );
            if cfg.file_opts.edit.enforce_seen_lines && shown > 0 {
                // `cap_columns` cuts long rows; a cut row was not read in full.
                let max = cfg.bash_opts.output_max_columns;
                let numbered = cfg.file_opts.read.line_numbers;
                let prefix = |line: usize| if numbered { line.to_string().len() + 1 } else { 0 };
                let full = text
                    .lines()
                    .skip(first - 1)
                    .take(shown)
                    .enumerate()
                    .filter(|(i, row)| max == 0 || row.len() + prefix(first + *i) <= max)
                    .map(|(i, _)| first + i)
                    .collect::<Vec<_>>();
                record_seen(&path, full);
            }
            Ok(crate::context::cap_columns(&body, cfg.bash_opts.output_max_columns))
        }
        "write" => {
            let raw = str_arg(input, "path")?;
            if let Some(rest) = raw.strip_prefix("proc://") {
                anyhow::ensure!(cfg.extra_tools_opts.launch.enabled, "proc:// is disabled by launch.enabled");
                let (name, kill) = match rest.trim_end_matches('/').strip_suffix("/kill") {
                    Some(name) => (name, true),
                    None => (rest.trim_end_matches('/'), false),
                };
                if kill {
                    return procs::read(cfg, &format!("{name}/kill"));
                }
                return procs::send_stdin(Some(name), str_arg(input, "content")?);
            }
            if let Some(rest) = raw.strip_prefix("vault://") {
                return vault::write(cfg, rest, str_arg(input, "content")?);
            }
            anyhow::ensure!(
                !raw.starts_with("out://"),
                "out:// replies are made by the agent's write call, not the write tool"
            );
            let path = resolve(cwd, raw);
            if let Some(p) = path.parent() {
                std::fs::create_dir_all(p)?;
            }
            let content = str_arg(input, "content")?;
            let before = std::fs::read_to_string(&path).unwrap_or_default();
            std::fs::write(&path, content).with_context(|| path.display().to_string())?;
            // The model authored the whole file, so every line counts as seen for `edit.enforce_seen_lines`.
            if cfg.file_opts.edit.enforce_seen_lines {
                record_seen(&path, 1..=content.lines().count().max(1));
            }
            let mut reply = format!("wrote {} bytes", content.len());
            if let Some(note) = lsp::after_change(cfg, cwd, &path, lsp::After::Write).await {
                reply.push_str("\n\n");
                reply.push_str(&note);
            }
            if let Some(note) = edit::after_change(cfg, &crate::home(), &path, &before, input).await {
                reply.push_str("\n\n");
                reply.push_str(&note);
            }
            Ok(reply)
        }
        "edit" => {
            anyhow::ensure!(
                cfg.file_opts.edit.mode != crate::config::files::EditMode::None,
                "the edit tool is disabled by edit.mode = none"
            );
            let raw_edit = str_arg(input, "path")?;
            let magic = cfg.file_opts.edit.mode == crate::config::files::EditMode::Magic;
            let magic_change = if magic {
                Some(str_arg(input, "change")?)
            } else {
                None
            };
            if let Some(rest) = raw_edit.strip_prefix("vault://") {
                anyhow::ensure!(!magic, "magic edits apply to project files, not vault://");
                return vault::edit(
                    cfg,
                    rest,
                    str_arg(input, "old_string")?,
                    str_arg(input, "new_string")?,
                    input["replace_all"].as_bool().unwrap_or(false),
                );
            }
            let path = resolve(cwd, raw_edit);
            let text =
                std::fs::read_to_string(&path).with_context(|| path.display().to_string())?;
            let opts = &cfg.file_opts.edit;
            if opts.block_auto_generated && looks_generated(&text) {
                bail!(
                    "{} looks generated; edit.block_auto_generated refuses it (change the generator, or set the setting false)",
                    path.display()
                );
            }
            if let Some(change) = magic_change {
                let out = edit::magic(cfg, &path, &text, change).await?;
                std::fs::write(&path, &out)?;
                let mut reply = format!("rewrote {} for the described change", path.display());
                if let Some(note) = lsp::after_change(cfg, cwd, &path, lsp::After::Edit).await {
                    reply.push_str("\n\n");
                    reply.push_str(&note);
                }
                if let Some(note) = edit::after_change(cfg, &crate::home(), &path, &text, input).await {
                    reply.push_str("\n\n");
                    reply.push_str(&note);
                }
                return Ok(reply);
            }
            let (old, new) = (str_arg(input, "old_string")?, str_arg(input, "new_string")?);
            let all = input["replace_all"].as_bool().unwrap_or(false);
            let hits: Vec<(usize, usize)> = if old.is_empty() {
                vec![]
            } else {
                text.match_indices(old).map(|(i, m)| (i, i + m.len())).collect()
            };
            let (spans, fuzzy) = if hits.is_empty() {
                let found = if opts.fuzzy_match {
                    fuzzy_span(&text, old, opts.fuzzy_threshold)?
                } else {
                    None
                };
                match found {
                    Some(span) => (vec![span], true),
                    None => bail!("old_string not found in {}", path.display()),
                }
            } else {
                if hits.len() > 1 && !all {
                    bail!("old_string matches {} times; add context to make it unique or set replace_all", hits.len());
                }
                (hits, false)
            };
            if opts.enforce_seen_lines {
                let shown = shown_lines(&path);
                let missing: Vec<usize> = spans
                    .iter()
                    .flat_map(|&span| touched_lines(&text, span))
                    .filter(|line| !shown.contains(line))
                    .collect();
                if !missing.is_empty() {
                    let list = missing.iter().map(usize::to_string).collect::<Vec<_>>().join(", ");
                    bail!(
                        "line(s) {list} of {} were never shown by read or grep in this session; read them before editing",
                        path.display()
                    );
                }
            }
            let out = if fuzzy {
                let (start, end) = spans[0];
                format!("{}{new}{}", &text[..start], &text[end..])
            } else {
                text.replace(old, new)
            };
            std::fs::write(&path, out)?;
            let mut reply = if fuzzy {
                "replaced 1 occurrence(s) (fuzzy match)".to_string()
            } else {
                format!("replaced {} occurrence(s)", spans.len())
            };
            if let Some(note) = lsp::after_change(cfg, cwd, &path, lsp::After::Edit).await {
                reply.push_str("\n\n");
                reply.push_str(&note);
            }
            if let Some(note) = edit::after_change(cfg, &crate::home(), &path, &text, input).await {
                reply.push_str("\n\n");
                reply.push_str(&note);
            }
            Ok(reply)
        }
        "bash" if input["name"].as_str().is_some_and(|n| !n.trim().is_empty()) => {
            anyhow::ensure!(cfg.extra_tools_opts.launch.enabled, "named services are disabled by launch.enabled");
            anyhow::ensure!(input["background"] != true, "a named service is supervised; drop `background`");
            procs::start_in(&crate::home(), cwd, input["name"].as_str().unwrap(), str_arg(input, "command")?, cfg)
        }
        "bash" => {
            bash(
                str_arg(input, "command")?,
                input["timeout"].as_u64().unwrap_or(120),
                input["background"] == true,
                cwd,
                cfg,
            )
            .await
        }
        "grep" => {
            let (pattern, path, glob) = (
                str_arg(input, "pattern")?,
                search_path(cwd, input["path"].as_str()),
                input["glob"].as_str(),
            );
            let (before, after) = (cfg.file_opts.grep.context_before, cfg.file_opts.grep.context_after);
            let cmd = match crate::config::executable("rg") {
                Some(rg) => {
                    let mut c = tokio::process::Command::new(rg);
                    c.args(["-n", "--no-heading", "--color", "never"]);
                    if before > 0 {
                        c.arg("-B").arg(before.to_string());
                    }
                    if after > 0 {
                        c.arg("-A").arg(after.to_string());
                    }
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
                    if before > 0 {
                        c.arg("-B").arg(before.to_string());
                    }
                    if after > 0 {
                        c.arg("-A").arg(after.to_string());
                    }
                    if let Some(g) = glob {
                        c.arg(format!("--include={g}"));
                    }
                    c.args(["-e", pattern, "--"]).arg(path.as_deref().unwrap_or(Path::new(".")));
                    c
                }
            };
            let out = lines(cmd, cwd).await?;
            if cfg.file_opts.edit.enforce_seen_lines {
                record_grep_seen(&out, cwd);
            }
            Ok(out)
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
        "web_fetch" => fetch::read(cfg, str_arg(input, "url")?).await,
        "web_search" => web_search(str_arg(input, "query")?, &cfg).await,
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
        "think" => Ok("------".into()),
        "memory" => memory(input, cwd),
        other => bail!("unknown tool `{other}`"),
    }
}

/// `sh -c`, wrapped in a write-restricting sandbox when `SANDBOX` is on.
pub(super) fn shell(command: &str, cwd: &Path) -> Result<tokio::process::Command> {
    shell_with(command, cwd, &config())
}

fn shell_program(cfg: &Config) -> Result<std::ffi::OsString> {
    if let Some(path) = cfg.bash_opts.shell_path.as_deref().filter(|p| !p.trim().is_empty()) {
        let path = match path.strip_prefix("~/") {
            Some(rest) => crate::user_home().join(rest),
            None => PathBuf::from(path),
        };
        return Ok(path.into_os_string());
    }
    #[cfg(windows)]
    {
        return Ok(crate::config::executable("sh.exe")
            .context("sh.exe not found; install Git for Windows or add a POSIX shell to PATH")?
            .into_os_string());
    }
    #[cfg(not(windows))]
    Ok("sh".into())
}

fn shell_with(command: &str, cwd: &Path, cfg: &Config) -> Result<tokio::process::Command> {
    let program = shell_program(cfg)?;
    if !SANDBOX.load(Ordering::Relaxed) {
        let mut c = tokio::process::Command::new(program);
        c.arg("-c").arg(command);
        #[cfg(windows)]
        c.env("PATH", crate::config::agent_path()?);
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
        .arg(program)
        .arg("-c")
        .arg(command);
    Ok(c)
}

static JOB_SEQ: AtomicUsize = AtomicUsize::new(0);

fn log_path() -> PathBuf {
    let dir = crate::home().join("jobs");
    let _ = std::fs::create_dir_all(&dir);
    dir.join(format!(
        "{}-{}.log",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_micros(),
        JOB_SEQ.fetch_add(1, Ordering::SeqCst)
    ))
}

fn job_capacity(cfg: &Config) -> usize {
    cfg.bash_opts.r#async.max_jobs.max(1)
}

/// Reserves one running-job slot; `Err` when the cap is already reached.
fn reserve_job(cfg: &Config) -> Result<()> {
    if JOBS_RUNNING.fetch_add(1, Ordering::SeqCst) >= job_capacity(cfg) {
        JOBS_RUNNING.fetch_sub(1, Ordering::SeqCst);
        bail!("background job limit reached ({}); raise async.max_jobs or stop a running job", job_capacity(cfg));
    }
    Ok(())
}

/// Loads an allowed direnv/devenv environment for `cwd`; an error or timeout just means no env.
async fn direnv_env(cwd: &Path, cfg: &Config) -> Vec<(String, Option<String>)> {
    let opts = &cfg.bash_opts.bash;
    if opts.direnv == crate::config::bash::Direnv::Off
        || !cwd.ancestors().take(64).any(|dir| dir.join(".envrc").is_file())
    {
        return Vec::new();
    }
    let Some(direnv) = crate::config::executable("direnv") else { return Vec::new() };
    let mut cmd = tokio::process::Command::new(direnv);
    cmd.args(["export", "json"])
        .current_dir(cwd)
        .stdin(std::process::Stdio::null())
        .envs(crate::secrets::env());
    // shortcut: no export cache; a slow devenv shell pay its first-load cost on every command.
    let output = match tokio::time::timeout(Duration::from_millis(opts.direnv_load_timeout_ms.max(1)), cmd.output()).await {
        Ok(Ok(out)) if out.status.success() => out,
        _ => return Vec::new(),
    };
    let Ok(Value::Object(env)) = serde_json::from_slice::<Value>(&output.stdout) else { return Vec::new() };
    env.into_iter().map(|(k, v)| (k, v.as_str().map(str::to_string))).collect()
}

fn spawn_logged(
    command: &str,
    cwd: &Path,
    cfg: &Config,
    env: &[(String, Option<String>)],
) -> Result<(tokio::process::Child, PathBuf)> {
    let log = log_path();
    let file = std::fs::File::create(&log)?;
    let mut cmd = shell_with(command, cwd, cfg)?;
    cmd.envs(crate::secrets::env());
    for (name, value) in env {
        match value {
            Some(value) => cmd.env(name, value),
            None => cmd.env_remove(name),
        };
    }
    let child = cmd
        .current_dir(cwd)
        .stdin(std::process::Stdio::null())
        .stdout(file.try_clone()?)
        .stderr(file)
        .spawn()?;
    Ok((child, log))
}

/// Blocks a command whose job belongs to a dedicated tool. Invalid patterns are skipped.
fn intercept<'a>(command: &str, cfg: &'a crate::config::bash::InterceptorCfg, tools: &Config) -> Option<&'a crate::config::bash::InterceptorRule> {
    let available = |name: &str| !tools.no_tools && (tools.tools.is_empty() || tools.tools.iter().any(|t| t == name));
    let mut candidates = vec![command.to_string()];
    for (segment, piped) in crate::permission::segments(command) {
        let stripped = crate::permission::strip_env(&segment, &|_| true);
        if !piped {
            candidates.push(segment);
        }
        candidates.push(stripped);
    }
    for rule in &cfg.patterns {
        if !available(&rule.tool) {
            continue;
        }
        // OMP's service rules tell the model to use `name`; native bash has no such parameter.
        if rule.tool == "bash" && rule.message.contains("`name`") {
            continue;
        }
        let Ok(pattern) = fancy_regex::Regex::new(&rule.pattern) else { continue };
        if candidates.iter().any(|candidate| pattern.is_match(candidate).unwrap_or(false)) {
            return Some(rule);
        }
    }
    None
}

/// Refuses git commands that discard or move shared work in a repository's main checkout.
fn git_guard(command: &str, cwd: &Path, cfg: &crate::config::bash::BashCfg) -> Result<()> {
    if !cfg.git_guard {
        return Ok(());
    }
    let Some(repo) = find_git(cwd) else { return Ok(()) };
    for (segment, _) in crate::permission::segments(command) {
        let words: Vec<&str> = segment.split_whitespace().collect();
        if words.first() != Some(&"git") {
            continue;
        }
        let mut i = 1;
        while i < words.len() && words[i].starts_with('-') {
            i += if matches!(words[i], "-C" | "-c" | "--git-dir" | "--work-tree" | "--namespace") { 2 } else { 1 };
        }
        let Some(sub) = words.get(i).copied() else { continue };
        let args = &words[i + 1..];
        let in_conflict = ["MERGE_HEAD", "CHERRY_PICK_HEAD", "REVERT_HEAD", "rebase-merge", "rebase-apply"]
            .iter()
            .any(|name| repo.dir.join(name).exists());
        let refused = match sub {
            "stash" => !matches!(args.first().copied(), Some("list" | "show" | "create")),
            "reset" => {
                let hard = args.iter().any(|a| *a == "--hard");
                let rev = args.iter().take_while(|a| **a != "--").any(|a| !a.starts_with('-'));
                hard || rev
            }
            "switch" => !in_conflict,
            "checkout" => !in_conflict,
            "restore" => !(args.contains(&"--staged") && !args.contains(&"--worktree")),
            _ => false,
        };
        if !refused {
            continue;
        }
        if repo.worktree {
            // The linked worktree is this agent's own checkout; keep what the command discards recoverable.
            save_discarded(cwd);
            return Ok(());
        }
        bail!(
            "bash.git_guard refuses `git {sub}` in the shared main checkout: it discards or moves work another checkout may be using. \
             Run it from a linked worktree, resolve a merge/rebase conflict first, or turn bash.git_guard off."
        );
    }
    Ok(())
}

struct GitRepo {
    dir: PathBuf,
    worktree: bool,
}

fn find_git(cwd: &Path) -> Option<GitRepo> {
    for dir in cwd.ancestors() {
        let dot = dir.join(".git");
        if dot.is_dir() {
            return Some(GitRepo { dir: dot, worktree: false });
        }
        if let Ok(text) = std::fs::read_to_string(&dot) {
            let path = text.trim().strip_prefix("gitdir:")?.trim();
            return Some(GitRepo { dir: dir.join(path), worktree: true });
        }
    }
    None
}

/// `git stash create` records a commit without touching the tree; the ref keeps it recoverable.
fn save_discarded(cwd: &Path) {
    let Ok(out) = std::process::Command::new("git").arg("-C").arg(cwd).args(["stash", "create"]).output() else { return };
    let id = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if id.is_empty() {
        return;
    }
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let _ = std::process::Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(["update-ref", &format!("refs/greencode/discarded/{stamp}"), &id])
        .status();
}

async fn bash(command: &str, timeout: u64, background: bool, cwd: &Path, cfg: &Config) -> Result<String> {
    // `out://` is a tool path, not a shell target; a redirect would silently create a directory named `out:`.
    if command.contains("out://")
        && fancy_regex::Regex::new(r"(?:^|[^<])>{1,2}\s*out://").is_ok_and(|pattern| pattern.is_match(command).unwrap_or(false))
    {
        bail!("out:// is written with the `write` tool (path `out://`), not shell redirection");
    }
    if cfg.bash_opts.bash_interceptor.enabled {
        if let Some(rule) = intercept(command, &cfg.bash_opts.bash_interceptor, cfg) {
            bail!("Blocked: {}\nCommand: {command}", rule.message);
        }
    }
    git_guard(command, cwd, &cfg.bash_opts.bash)?;
    let timeout = match cfg.bash_opts.max_timeout {
        0 => timeout,
        max => timeout.min(max),
    }
    .max(1);
    let env = direnv_env(cwd, cfg).await;
    let background_job = |command: String, mut child: tokio::process::Child, log: PathBuf| {
        let pid = child.id().unwrap_or(0);
        // `async.widgets`: the widget and its caption are written once, while the job runs.
        jobwidget::start(cfg, &command, &log);
        JOBS_LIVE.lock().unwrap().push((pid, command.clone(), log.clone()));
        tokio::spawn(async move {
            let status = child.wait().await.ok().and_then(|status| status.code());
            JOBS_LIVE.lock().unwrap().retain(|(p, ..)| *p != pid);
            job_finished(JobResult { command, log, status });
        });
        pid
    };
    if background {
        reserve_job(cfg)?;
        let (child, log) = match spawn_logged(command, cwd, cfg, &env) {
            Ok(spawned) => spawned,
            Err(error) => {
                JOBS_RUNNING.fetch_sub(1, Ordering::SeqCst);
                return Err(error);
            }
        };
        let pid = background_job(command.to_string(), child, log.clone());
        return Ok(format!("started in the background, pid {pid}. Output: {} (use `read`). Stop it with `kill {pid}`.", log.display()));
    }
    let auto = cfg.bash_opts.bash.auto_background;
    let threshold = (auto.enabled && running_jobs() < job_capacity(cfg))
        .then(|| Duration::from_millis(auto.threshold_ms));
    let (mut child, log) = spawn_logged(command, cwd, cfg, &env)?;
    let deadline = Duration::from_secs(timeout);
    let wait_for = threshold.map_or(deadline, |threshold| threshold.min(deadline));
    match tokio::time::timeout(wait_for, child.wait()).await {
        Ok(status) => {
            let status = status?;
            let bytes = std::fs::read(&log)?;
            let _ = std::fs::remove_file(&log);
            let text = String::from_utf8_lossy(&bytes).into_owned();
            // The minimizer owns both levers: `cat` sourcing an outline first, then the verbose-command compression.
            let text = if cfg.bash_opts.shell_minimizer.enabled {
                minimizer::cat_outline(command, cwd, cfg).unwrap_or_else(|| minimizer::minimize(command, &text))
            } else {
                text
            };
            let mut text = crate::context::cap_columns(&text, cfg.bash_opts.output_max_columns);
            if !status.success() {
                text.push_str(&format!(
                    "\n[exit {}]",
                    status.code().map_or("signal".into(), |c| c.to_string())
                ));
                // A missing interpreter is common (`python`, `pytest`); say so once instead of letting the model retry blind.
                if status.code() == Some(127) {
                    text.push_str("\n[command not found: check the name, or try python3, node, cargo]");
                }
                bail!(text);
            }
            Ok(text)
        }
        Err(_) if threshold.is_some_and(|threshold| threshold < deadline) => {
            if let Err(error) = reserve_job(cfg) {
                let _ = child.start_kill();
                let _ = child.wait().await;
                let _ = std::fs::remove_file(&log);
                return Err(error);
            }
            let pid = background_job(command.to_string(), child, log.clone());
            Ok(format!(
                "still running after {} ms; moved to the background, pid {pid}. Output: {} (use `read`). Stop it with `kill {pid}`.",
                auto.threshold_ms,
                log.display()
            ))
        }
        Err(_) => {
            let _ = child.start_kill();
            let _ = child.wait().await;
            let _ = std::fs::remove_file(&log);
            bail!("timed out after {timeout}s");
        }
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{
        files::{EditCfg, FileOpts, GrepCfg, ReadCfg},
        Config,
    };
    use serde_json::json;

    fn files(edit: EditCfg) -> Config {
        Config { file_opts: FileOpts { edit, ..Default::default() }, ..Config::default() }
    }

    #[test]
    fn edit_mode_selects_the_offered_tool() {
        use crate::config::files::EditMode;
        let names = |mode| {
            let mut cfg = Config::default();
            cfg.file_opts.edit.mode = mode;
            defs_for(&cfg, true, &[]).into_iter().map(|t| t.name).collect::<Vec<_>>()
        };
        let replace = names(EditMode::Replace);
        assert!(replace.contains(&"edit".to_string()));
        // `none` removes the edit tool and keeps bash.
        let none = names(EditMode::None);
        assert!(!none.contains(&"edit".to_string()) && none.contains(&"bash".to_string()));
        // `magic` keeps the name but takes a described change instead of an anchor pair.
        let magic = defs_for(
            &Config { file_opts: FileOpts { edit: EditCfg { mode: EditMode::Magic, ..Default::default() }, ..Default::default() }, ..Config::default() },
            true,
            &[],
        );
        let edit = magic.iter().find(|t| t.name == "edit").unwrap();
        assert!(edit.schema["properties"].get("change").is_some());
        assert!(edit.schema["properties"].get("old_string").is_none());
        assert_eq!(edit.schema["required"], json!(["path", "change"]));
    }

    #[test]
    fn cache_catalog_uses_each_sessions_settings() {
        let catalog = |on| {
            let mut cfg = Config::default();
            cfg.extra_tools_opts.generate_image.enabled = on;
            cfg.extra_tools_opts.launch.enabled = on;
            cfg.bash_opts.out.enabled = on;
            cfg.bash_opts.intent_tracing = on;
            cfg.discovery.dev.autoqa = on;
            defs_for(&cfg, true, &[])
        };
        for on in [true, false] {
            let tools = catalog(on);
            let tool = |name| tools.iter().find(|t| t.name == name).unwrap();
            assert_eq!(tools.iter().any(|t| t.name == "generate_image"), on);
            assert_eq!(tool("bash").description.contains("proc://"), on);
            assert_eq!(tool("bash").schema["properties"].get("intent").is_some(), on);
            assert_eq!(tool("write").description.contains("out://"), on);
            assert_eq!(tool("ext").description.contains("report_issue"), on);
        }
    }

    #[tokio::test]
    async fn magic_mode_rewrites_the_file_the_model_returns() {
        use crate::config::{files::EditMode, ProviderCfg};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}/v1", listener.local_addr().unwrap());
        let seen = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
        let recorded = seen.clone();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = Vec::new();
            let mut chunk = [0u8; 4096];
            while !String::from_utf8_lossy(&buf).contains("max_tokens") {
                let n = socket.read(&mut chunk).await.unwrap();
                if n == 0 { break; }
                buf.extend_from_slice(&chunk[..n]);
            }
            *recorded.lock().unwrap() = String::from_utf8_lossy(&buf).to_string();
            let body = "data: {\"choices\":[{\"delta\":{\"content\":\"```\\nfn main() { println!(\\\"new\\\"); }\\n```\"}}]}\n\ndata: [DONE]\n\n";
            socket.write_all(format!("HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
        });
        let mut cfg = Config::default();
        cfg.learning.auto = false;
        cfg.file_opts.edit.mode = EditMode::Magic;
        // Not `local/`: that prefix is the tiny local runtime, not an OpenAI-compatible provider.
        cfg.providers.insert("mock".into(), ProviderCfg { kind: "openai".into(), base_url: base, api_key: Some("k".into()), ..Default::default() });
        cfg.model = "mock/writer".into();
        cfg.model_roles.insert("smol".into(), "mock/writer".into());
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.rs");
        std::fs::write(&path, "fn main() {}\n").unwrap();
        let out = run_with("edit", &json!({"path":"a.rs","change":"print new"}), dir.path(), &cfg).await.unwrap();
        assert!(out.contains("described change"), "{out}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "fn main() { println!(\"new\"); }");
        let request = seen.lock().unwrap().clone();
        assert!(request.contains("print new") || request.contains("print%20new"), "{request}");
        // With `none` the edit tool refuses even when called directly.
        cfg.file_opts.edit.mode = EditMode::None;
        let err = run_with("edit", &json!({"path":"a.rs","change":"x"}), dir.path(), &cfg).await.unwrap_err();
        assert!(err.to_string().contains("edit.mode = none"), "{err}");
    }

    #[tokio::test]
    async fn fuzzy_edit_matches_a_whitespace_difference() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.txt");
        std::fs::write(&path, "fn main() {\n    let value = 1;\n}\n").unwrap();
        let out = run_with(
            "edit",
            &json!({"path": "a.txt", "old_string": "let   value  = 1;", "new_string": "    let value = 2;"}),
            dir.path(),
            &Config::default(),
        )
        .await
        .unwrap();
        assert!(out.contains("fuzzy"), "{out}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "fn main() {\n    let value = 2;\n}\n");
    }

    #[tokio::test]
    async fn fuzzy_edit_refuses_two_candidates() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "let  value = 1;\nlet   value = 1;\n").unwrap();
        let err = run_with(
            "edit",
            &json!({"path": "a.txt", "old_string": "let value = 1;", "new_string": "let value = 9;"}),
            dir.path(),
            &Config::default(),
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("fuzzy matches"), "{err}");
    }

    #[tokio::test]
    async fn fuzzy_threshold_and_switch_decide_matches() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.txt");
        std::fs::write(&path, "let  value = 1;\nlet value = 2;\n").unwrap();
        // Default 0.95 accepts only the whitespace-equal first line.
        let out = run_with(
            "edit",
            &json!({"path": "a.txt", "old_string": "let value = 1;", "new_string": "let value = 9;"}),
            dir.path(),
            &Config::default(),
        )
        .await
        .unwrap();
        assert!(out.contains("fuzzy"), "{out}");
        // At 0.8 both lines qualify, so the edit must be refused as ambiguous.
        std::fs::write(&path, "let  value = 1;\nlet value = 2;\n").unwrap();
        let err = run_with(
            "edit",
            &json!({"path": "a.txt", "old_string": "let value = 1;", "new_string": "let value = 9;"}),
            dir.path(),
            &files(EditCfg { fuzzy_threshold: 0.8, ..Default::default() }),
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("fuzzy matches"), "{err}");
        // With fuzzy matching off the exact-text rule stands.
        let err = run_with(
            "edit",
            &json!({"path": "a.txt", "old_string": "let value = 1;", "new_string": "let value = 9;"}),
            dir.path(),
            &files(EditCfg { fuzzy_match: false, ..Default::default() }),
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("not found"), "{err}");
    }

    #[tokio::test]
    async fn seen_lines_gate_edits_until_read() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.txt");
        std::fs::write(&path, "one\ntwo\nthree\n").unwrap();
        let cfg = files(EditCfg { enforce_seen_lines: true, ..Default::default() });
        let edit = |old: &str| {
            let old = old.to_string();
            let dir = dir.path().to_path_buf();
            let cfg = cfg.clone();
            async move {
                run_with(
                    "edit",
                    &json!({"path": "a.txt", "old_string": old, "new_string": "changed"}),
                    &dir,
                    &cfg,
                )
                .await
            }
        };
        let err = edit("three").await.unwrap_err();
        assert!(err.to_string().contains("never shown"), "{err}");
        // Reading another range does not unlock line 3.
        run_with("read", &json!({"path": "a.txt", "offset": 1, "limit": 1}), dir.path(), &cfg)
            .await
            .unwrap();
        assert!(edit("three").await.is_err());
        run_with("read", &json!({"path": "a.txt", "offset": 3, "limit": 1}), dir.path(), &cfg)
            .await
            .unwrap();
        assert!(edit("three").await.is_ok());
    }

    #[tokio::test]
    async fn generated_files_are_refused_unless_allowed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("api.go");
        std::fs::write(&path, "// Code generated by sqlc. DO NOT EDIT.\npackage api\n").unwrap();
        let input = json!({"path": "api.go", "old_string": "package api", "new_string": "package main"});
        let err = run_with("edit", &input, dir.path(), &Config::default()).await.unwrap_err();
        assert!(err.to_string().contains("generated"), "{err}");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "// Code generated by sqlc. DO NOT EDIT.\npackage api\n"
        );
        let cfg = files(EditCfg { block_auto_generated: false, ..Default::default() });
        assert!(run_with("edit", &input, dir.path(), &cfg).await.is_ok());
    }

    #[tokio::test]
    async fn read_without_selector_summarizes_and_records_only_kept_lines() {
        let dir = tempfile::tempdir().unwrap();
        // A body long enough to elide above the fixture's lowered threshold.
        let body = (1..=12).map(|i| format!("    let v{i} = {i};")).collect::<Vec<_>>().join("\n");
        std::fs::write(dir.path().join("a.rs"), format!("fn big() {{\n{body}\n}}\n\nfn small() {{\n    one();\n}}\n")).unwrap();
        let summarize = crate::config::files::SummarizeCfg {
            min_total_lines: 1,
            unfold_until: 0,
            ..Default::default()
        };
        let cfg = Config {
            file_opts: FileOpts {
                read: ReadCfg { summarize: summarize.clone(), ..ReadCfg::default() },
                ..Default::default()
            },
            ..Config::default()
        };
        let out = run_with("read", &json!({"path": "a.rs"}), dir.path(), &cfg).await.unwrap();
        assert!(out.starts_with("fn big() { … }"), "{out}");
        assert!(out.contains("fn small() {"), "{out}");
        assert!(!out.contains("let v1 = 1;"), "{out}");
        // An explicit selector is a plain window even when a summary would be possible.
        let window = run_with("read", &json!({"path": "a.rs", "offset": 2, "limit": 1}), dir.path(), &cfg).await.unwrap();
        assert_eq!(window, "    let v1 = 1;");
        // Turning summaries off restores the window.
        let off = Config {
            file_opts: FileOpts {
                read: ReadCfg { summarize: crate::config::files::SummarizeCfg { enabled: false, ..summarize }, ..ReadCfg::default() },
                ..Default::default()
            },
            ..Config::default()
        };
        let plain = run_with("read", &json!({"path": "a.rs"}), dir.path(), &off).await.unwrap();
        assert!(plain.starts_with("fn big() {\n    let v1 = 1;"), "{plain}");
        // With `edit.enforce_seen_lines`, the summary unlocks the lines it shows and nothing it hid.
        let gated = Config {
            file_opts: FileOpts {
                read: ReadCfg { summarize: summarize.clone(), ..ReadCfg::default() },
                edit: EditCfg { enforce_seen_lines: true, ..Default::default() },
                ..Default::default()
            },
            ..Config::default()
        };
        run_with("read", &json!({"path": "a.rs"}), dir.path(), &gated).await.unwrap();
        let anchor = |old: &str| json!({"path": "a.rs", "old_string": old, "new_string": "changed"});
        assert!(run_with("edit", &anchor("one();"), dir.path(), &gated).await.is_ok());
        let err = run_with("edit", &anchor("let v1 = 1;"), dir.path(), &gated).await.unwrap_err();
        assert!(err.to_string().contains("never shown"), "{err}");
    }

    #[tokio::test]
    async fn read_settings_control_numbers_and_default_limit() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "a\nb\nc\n").unwrap();
        let cfg = Config {
            file_opts: FileOpts {
                read: ReadCfg { line_numbers: true, default_limit: 2, ..ReadCfg::default() },
                ..Default::default()
            },
            ..Config::default()
        };
        let out = run_with("read", &json!({"path": "a.txt"}), dir.path(), &cfg).await.unwrap();
        assert_eq!(out, "1:a\n2:b\n[1 more lines; read with offset 3]");
    }

    #[tokio::test]
    async fn grep_context_includes_neighbouring_lines() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "alpha\nneedle\ngamma\n").unwrap();
        let cfg = Config {
            file_opts: FileOpts {
                grep: GrepCfg { context_before: 1, context_after: 1 },
                ..Default::default()
            },
            ..Config::default()
        };
        let out = run_with("grep", &json!({"pattern": "needle", "path": "."}), dir.path(), &cfg)
            .await
            .unwrap();
        assert!(out.contains("alpha") && out.contains("needle") && out.contains("gamma"), "{out}");
    }

    #[test]
    fn searxng_and_exa_render_title_url_and_snippet() {
        let searxng = json!({"results": [
            {"title": "Rust", "url": "https://rust-lang.org", "content": "A language."},
            {"title": "Docs", "url": "https://docs.rs", "snippet": "Crate docs."}
        ]});
        let out = searxng_results(&searxng);
        assert!(out.starts_with("Rust\nhttps://rust-lang.org\nA language."), "{out}");
        assert!(out.contains("Docs\nhttps://docs.rs\nCrate docs."), "{out}");
        assert_eq!(searxng_results(&json!({"results": []})), "no results");

        let exa = json!({"results": [
            {"title": "Async", "url": "https://example.test/a", "summary": "Sum"},
            {"title": "Fallback", "url": "https://example.test/b", "highlights": ["Highlight"]}
        ]});
        let out = exa_results(&exa);
        assert!(out.contains("Async\nhttps://example.test/a\nSum"), "{out}");
        assert!(out.contains("Fallback\nhttps://example.test/b\nHighlight"), "{out}");
    }

    #[test]
    fn interceptor_matches_commands_with_a_better_tool() {
        let cfg = Config::default();
        let rule = intercept("cat Cargo.toml", &cfg.bash_opts.bash_interceptor, &cfg).unwrap();
        assert_eq!(rule.tool, "read");
        assert!(rule.message.contains("read"));
        assert!(intercept("ls -la", &cfg.bash_opts.bash_interceptor, &cfg).is_none());
        // A tool turned off cannot be the reason to block; the command runs.
        let mut off = Config::default();
        off.tools = vec!["bash".into()];
        assert!(intercept("cat Cargo.toml", &off.bash_opts.bash_interceptor, &off).is_none());
    }

    #[tokio::test]
    async fn slow_commands_move_to_the_background_after_the_threshold() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = Config::default();
        cfg.bash_opts.bash.auto_background = crate::config::bash::AutoBackground { enabled: true, threshold_ms: 50 };
        let out = run_with("bash", &json!({"command": "sleep 5", "timeout": 30}), dir.path(), &cfg)
            .await
            .unwrap();
        assert!(out.contains("moved to the background"), "{out}");
        let pid = out.split("pid ").nth(1).and_then(|rest| rest.split('.').next()).unwrap().trim();
        let _ = std::process::Command::new("kill").arg(pid).status();
    }

    #[tokio::test]
    async fn out_redirect_is_refused_in_favour_of_the_write_tool() {
        let dir = tempfile::tempdir().unwrap();
        let err = run_with("bash", &json!({"command": "echo hi > out://note"}), dir.path(), &Config::default())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("write"), "{err}");
    }

    #[tokio::test]
    async fn output_columns_cap_each_line() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = Config::default();
        cfg.bash_opts.output_max_columns = 5;
        let out = run_with("bash", &json!({"command": "printf 'abcdefgh\\n'"}), dir.path(), &cfg)
            .await
            .unwrap();
        assert_eq!(out.lines().next(), Some("ab…"), "{out:?}");
    }

    #[test]
    fn git_guard_refuses_the_shared_checkout_but_not_a_conflict_or_worktree() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join(".git")).unwrap();
        let mut cfg = crate::config::bash::BashCfg::default();
        cfg.git_guard = true;
        assert!(git_guard("git stash", dir.path(), &cfg).is_err());
        assert!(git_guard("git checkout -- .", dir.path(), &cfg).is_err());
        assert!(git_guard("git status", dir.path(), &cfg).is_ok());
        // Resolving a merge conflict may check files out in the main checkout.
        std::fs::write(dir.path().join(".git/MERGE_HEAD"), "deadbeef\n").unwrap();
        assert!(git_guard("git checkout -- .", dir.path(), &cfg).is_ok());
        // A linked worktree is the agent's own, so destructive commands are allowed there.
        let worktree = tempfile::tempdir().unwrap();
        std::fs::write(worktree.path().join(".git"), format!("gitdir: {}", dir.path().join(".git/worktrees/w").display())).unwrap();
        std::fs::create_dir_all(dir.path().join(".git/worktrees/w")).unwrap();
        assert!(git_guard("git stash", worktree.path(), &cfg).is_ok());
    }
}
