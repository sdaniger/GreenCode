//! `ext`: one tool that reaches the less common tools. Its description is a one-line catalog; a tool's arguments
//! are returned by `help` only when the model asks, so the cached prompt prefix stays small (like `mcp` `search`).

use super::{def as tool_def, str_arg};
use crate::{permission::resolve, ToolDef};
use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::path::Path;

/// Name and catalog blurb of every ext tool, in catalog order.
pub const CATALOG: [(&str, &str); 24] = [
    ("hread", "line-anchored read"),
    ("hedit", "line-anchored edit"),
    ("ast", "structural search/replace"),
    ("eval", "persistent Python/JS"),
    ("debug", "debugger"),
    ("advisor", "second opinion"),
    ("learn", "durable project lessons"),
    ("ask", "ask the user"),
    ("secret", "ask the user privately for a credential"),
    ("checkpoint", "mark context"),
    ("rewind", "drop exploration since checkpoint, keep a report"),
    ("new_context", "restart from a handoff"),
    ("recall", "search past sessions"),
    ("wait", "wait for a pid"),
    ("manage_skill", "save skills"),
    ("find", "files for a described behavior"),
    ("github", "read issues/PRs/CI via gh"),
    ("dsm", "disassemble binaries"),
    ("tts", "speech text to an audio file"),
    ("ratchet", "eval flow against a baseline"),
    ("archive", "prompt/session history"),
    ("vault", "Obsidian vault notes"),
    ("security", "scan planning and runs"),
    ("report_issue", "report a wrong or broken tool result"),
];

pub const NAMES: [&str; 24] = {
    let mut names = [""; 24];
    let mut i = 0;
    while i < CATALOG.len() {
        names[i] = CATALOG[i].0;
        i += 1;
    }
    names
};

/// `None` when every ext tool is turned off in settings.
pub fn def(cfg: &crate::config::Config, off: &[String]) -> Option<ToolDef> {
    let s = json!({"type": "string"});
    let autoqa = cfg.discovery.dev.autoqa;
    let on: Vec<String> = CATALOG
        .iter()
        .filter(|(name, _)| !off.iter().any(|o| o == name) && (*name != "report_issue" || autoqa))
        .map(|(name, about)| format!("{name} ({about})"))
        .collect();
    if on.is_empty() {
        return None;
    }
    Some(tool_def(
        "ext",
        &format!(
            "Extra tools. `help` returns one tool's arguments, `call` runs it: {}.",
            on.join(", ")
        ),
        json!({"action": {"enum": ["help", "call"]}, "tool": s, "args": {"type": "object"}}),
        &["action", "tool"],
    ))
}

/// One line about a tool and the JSON Schema of its arguments.
pub fn help(tool: &str) -> Result<String> {
    let s = json!({"type": "string"});
    let n = json!({"type": "integer"});
    let (about, props, required): (&str, Value, &[&str]) = match tool {
        "hread" => (
            "Read a file with line numbers and a tag. hedit needs the tag to prove the file did not change.",
            json!({"path": s, "offset": n, "limit": n}), &["path"],
        ),
        "hedit" => (
            "Edit by line number against the numbering `hread` showed. replace: lines start..=end become `text`; delete: remove start..=end; insert: `text` goes after line `start` (0 = top). `end` defaults to `start`. A stale tag is refused and nothing is written.",
            json!({"path": s, "tag": s, "ops": {"type": "array", "items": {"type": "object", "properties": {"op": {"enum": ["replace", "insert", "delete"]}, "start": n, "end": n, "text": s}, "required": ["op", "start"]}}}),
            &["path", "tag", "ops"],
        ),
        "ast" => (
            "Structural search with an ast-grep pattern (`foo($A)`, `$$$` for many nodes). With `rewrite` (needs one file in `path`) it rewrites every match.",
            json!({"pattern": s, "lang": s, "path": s, "rewrite": s}), &["pattern"],
        ),
        "eval" => (
            "Run code in a kernel that keeps its variables between calls. The last expression is printed. A cell may define tools (`@tool` in Python, `tool(fn)` in JS) that you and your subagents then call directly, and may run subagents with `agent(prompt)` or `workpool(items)`. A cell still running after eval.auto_background.thresholdMs moves to a background job.",
            json!({"lang": {"enum": ["py", "js"]}, "code": s, "reset": {"type": "boolean"}}), &["lang", "code"],
        ),
        "debug" => (
            "Debug a program through a DAP adapter. launch (program, args) runs to the first breakpoint; break (path, line) toggles one; continue, step (kind: over|in|out), stack, vars (frame), eval (expr), stop. `.py` programs use debugpy, others lldb-dap; `adapter` overrides with an argv.",
            json!({"action": {"enum": ["launch", "break", "continue", "step", "stack", "vars", "eval", "stop"]}, "program": s, "args": {"type": "array", "items": s},
                   "adapter": {"type": "array", "items": s}, "path": s, "line": n, "kind": {"enum": ["over", "in", "out"]}, "frame": n, "expr": s}),
            &["action"],
        ),
        "advisor" => (
            "Ask a second model that reads this conversation. Needs `advisor_model` in config.toml.",
            json!({"question": s}), &["question"],
        ),
        "learn" => (
            "Save one concise reusable lesson after the user approves the tool call. Scope defaults to project; use global only for facts valid across projects.",
            json!({"scope": {"enum": ["project", "global"]}, "text": s}), &["text"],
        ),
        "ask" => (
            "Ask the user one or more questions and wait for the answers. Each answer lists the chosen option labels and any free text.",
            json!({"questions": {"type": "array", "items": {"type": "object", "properties": {"question": s, "options": {"type": "array", "items": s}, "multi": {"type": "boolean"}}, "required": ["question"]}}}),
            &["questions"],
        ),
        "secret" => (
            "Ask the user for a credential in a private dialog; the value never enters the conversation. It is saved as env var NAME (available in later `bash` calls) and in ~/.greencode/secrets/NAME. Never print or echo it.",
            json!({"name": s, "reason": s}), &["name", "reason"],
        ),
        "checkpoint" => (
            "Mark the current point before a long read-only exploration. `rewind` replaces the exploration with a report and keeps user instructions and background results.",
            json!({"goal": s}), &[],
        ),
        "rewind" => (
            "Return to the last checkpoint: the exploration since it is removed from the context and replaced by `report`. Call it alone, not beside other tools.",
            json!({"report": s}), &["report"],
        ),
        "new_context" => (
            "Replace the whole context with `handoff`: goals, decisions, files touched, state and next steps. Use when the history is mostly stale. Call it alone.",
            json!({"handoff": s}), &["handoff"],
        ),
        "recall" => (
            "Search past GreenCode sessions for text; returns session ids, titles, projects and a matching line.",
            json!({"query": s}), &["query"],
        ),
        "wait" => (
            "Wait until a background process (the pid `bash` returned) exits, up to `seconds` (default 60, max 600).",
            json!({"pid": n, "seconds": n}), &["pid"],
        ),
        "manage_skill" => (
            "Create, update or delete a reusable skill in ~/.greencode/skills/<name>/SKILL.md (kebab-case name). It is loaded in future threads.",
            json!({"action": {"enum": ["create", "update", "delete"]}, "name": s, "description": s, "body": s}), &["action", "name"],
        ),
        "find" => (
            "Find the files that implement a behavior: plain-language `query`, ranked by how many of its words a file contains. Returns `path:first-last score line`; read those ranges. Known strings: use grep.",
            json!({"query": s, "path": s}), &["query"],
        ),
        "github" => (
            "Read GitHub through `gh` (read-only). repo_view; issue_view/pr_view/pr_diff/pr_checks need `number`; search_issues/search_prs need `query`; run_list; run_view needs `run` (failed job logs). `repo` (owner/repo) defaults to the current checkout.",
            json!({"op": {"enum": ["repo_view", "issue_view", "pr_view", "pr_diff", "pr_checks", "search_issues", "search_prs", "run_list", "run_view"]}, "number": n, "query": s, "run": n, "repo": s}),
            &["op"],
        ),
        "dsm" => (
            "Binary analysis. `open` (path) analyses an executable or database and returns `handle`; `list`, `get`, `view` (overview, imports, exports, strings, <func>, <func>:asm, xrefs:<func>), `exec` (Python in the worker), `rename`, `comment`, `set_type`, `make_function`, `save`, `retain`, `close`, and `metadata` (header facts only). Backend from `disassembler.backend`. Pass `handle` to every action except open/list/metadata.",
            json!({"action": {"enum": ["open", "list", "get", "view", "exec", "rename", "comment", "set_type", "make_function", "save", "retain", "close", "metadata"]},
                   "path": s, "handle": s, "id": s, "arch": s, "view": s, "code": s, "target": s,
                   "name": s, "text": s, "decl": s, "save": {"type": "boolean"}}),
            &["action"],
        ),
        "tts" => (
            "Synthesize speech into an audio file. `text` is spoken; `output_path` (relative to the working directory) defaults to a timestamped .wav; .wav/.aiff/.m4a/.caf go through the on-device voice (`tts.local_voice`/`tts.local_speed`), other formats through xAI Grok Voice with `voice_id` (default eve) and `language` (default auto).",
            json!({"text": s, "output_path": s, "voice_id": s, "language": s}), &["text"],
        ),
        "ratchet" => (
            "Eval flow with a baseline for a hill climb. `init` needs `cases` (name, command, expect substring); `run` executes them and reports the score against the baseline; `status` shows history; `accept` makes the last run the baseline. Runs kebab-case `flow` (default `default`) under .greencode/ratchet.",
            json!({"action": {"enum": ["init", "run", "status", "accept"]}, "flow": s,
                   "cases": {"type": "array", "items": {"type": "object", "properties": {"name": s, "command": s, "expect": s, "timeout": n}, "required": ["command"]}}}),
            &["action"],
        ),
        "archive" => (
            "Read-only history of GreenCode sessions. `prompts` lists user prompts (newest first, optional `query`); `projects` groups sessions by folder; `sessions` lists threads; `search` finds a matching line; `recap` needs `session` and returns its last `turns` messages.",
            json!({"action": {"enum": ["prompts", "projects", "sessions", "search", "recap"]}, "query": s, "session": s, "turns": n, "limit": n}), &["action"],
        ),
        "vault" => (
            "Obsidian vault notes. `list` lists the vault; `read`/`write` take `path` (if omitted, `vault://`); `edit` swaps `old` for `new`; `open` launches the note in Obsidian through its CLI (needs one installed).",
            json!({"action": {"enum": ["list", "read", "write", "edit", "open"]}, "path": s, "content": s, "old": s, "new": s, "replace_all": {"type": "boolean"}}), &["action"],
        ),
        "report_issue" => (
            "Report a wrong, broken or misleading tool result to GreenCode's maintainers. `tool` is the tool that misbehaved, `summary` what went wrong; `details` adds the exact result.",
            json!({"tool": s, "summary": s, "details": s}), &["tool", "summary"],
        ),
        "security" => (
            "Security scans. `plan` writes the checks for this project; `run` executes them and stores the report; `list` shows past scans; `show` needs `id` and returns its findings. Reports are readable as security://<id>.",
            json!({"action": {"enum": ["plan", "run", "list", "show"]}, "id": s, "limit": n}), &["action"],
        ),
        other => bail!("unknown ext tool `{other}`; available: {}", NAMES.join(", ")),
    };
    Ok(format!(
        "{about}\nargs: {}",
        json!({"type": "object", "properties": props, "required": required})
    ))
}

/// The tool and input that permission, plan-mode and undo checks should judge: `ext` itself says nothing about
/// what a call does, so each inner tool maps onto the built-in it behaves like.
pub fn effective(name: &str, input: &Value) -> (String, Value) {
    // `lsp rename` rewrites files, so it asks and stops in plan mode like `edit`.
    if name == "lsp" && input["action"] == "rename" {
        return ("edit".into(), json!({"path": input["path"]}));
    }
    if name != "ext" {
        return (name.to_string(), input.clone());
    }
    let args = &input["args"];
    let path = args["path"].clone();
    match (input["action"].as_str(), input["tool"].as_str()) {
        (Some("call"), Some("hread")) => ("read".into(), json!({"path": path})),
        (Some("call"), Some("hedit")) => ("edit".into(), json!({"path": path})),
        (Some("call"), Some("ast")) if args["rewrite"].is_string() => {
            ("edit".into(), json!({"path": path}))
        }
        (Some("call"), Some("ast")) => (
            "grep".into(),
            json!({"pattern": args["pattern"], "path": if path.is_string() { path } else { json!(".") }}),
        ),
        (Some("call"), Some(tool @ ("eval" | "debug" | "advisor"))) => (
            tool.into(),
            if args.is_object() {
                args.clone()
            } else {
                json!({})
            },
        ),
        (Some("call"), Some("learn")) => (
            "memory".into(),
            json!({"scope": args["scope"].as_str().unwrap_or("project"), "action": "add", "text": args["text"]}),
        ),
        (Some("call"), Some("manage_skill")) => (
            "write".into(),
            json!({"path": skill_file(args["name"].as_str().unwrap_or("")).unwrap_or_else(|_| crate::home().join("skills"))}),
        ),
        (Some("call"), Some("find")) => (
            "grep".into(),
            json!({"pattern": args["query"], "path": if path.is_string() { path } else { json!(".") }}),
        ),
        // Every github op only reads, like fetching a page.
        (Some("call"), Some("github")) => {
            ("web_fetch".into(), json!({"url": "https://github.com"}))
        }
        // Opening and viewing analyse a file; the editing actions write to its database.
        (Some("call"), Some("dsm")) if matches!(args["action"].as_str(), Some("rename" | "comment" | "set_type" | "make_function" | "save" | "close")) => (
            "edit".into(),
            json!({"path": if path.is_string() { path } else { json!(".") }}),
        ),
        (Some("call"), Some("vault")) if matches!(args["action"].as_str(), Some("write" | "edit")) => (
            "edit".into(),
            json!({"path": if path.is_string() { path } else { json!(".") }}),
        ),
        (Some("call"), Some("ratchet")) => (
            "bash".into(),
            json!({"command": args["cases"].as_array().and_then(|c| c.first()).and_then(|c| c["command"].as_str()).unwrap_or("true")}),
        ),
        // Synthesizing writes the audio file.
        (Some("call"), Some("tts")) => (
            "write".into(),
            json!({"path": if path.is_string() { path } else { json!(".") }}),
        ),
        (Some("call"), Some("dsm")) => (
            "read".into(),
            json!({"path": if path.is_string() { path } else { json!(".") }}),
        ),
        _ => ("read".into(), json!({"path": "."})),
    }
}

/// Runs every ext tool except `advisor`, which needs the agent's provider.
pub async fn run(tool: &str, args: &Value, cwd: &Path) -> Result<String> {
    run_with(None, tool, args, cwd).await
}

pub async fn run_with(cfg: Option<&crate::config::Config>, tool: &str, args: &Value, cwd: &Path) -> Result<String> {
    match tool {
        "hread" => hread(args, cwd),
        "hedit" => hedit(args, cwd),
        "ast" => ast(args, cwd).await,
        "eval" => match cfg {
            Some(cfg) => super::eval::run_with(cfg, args, cwd).await,
            None => super::eval::run(args, cwd).await,
        },
        "debug" => super::dap::run(args, cwd).await,
        "recall" => recall(str_arg(args, "query")?),
        "wait" => wait(args).await,
        "manage_skill" => manage_skill(args),
        "find" => find(cfg, args, cwd).await,
        "github" => github(cfg, args, cwd).await,
        "dsm" => match cfg {
            Some(cfg) => crate::disasm::tool(cfg, args, cwd).await,
            None => bail!("dsm needs the loaded config; call it from the agent"),
        },
        "tts" => match cfg {
            Some(cfg) => crate::speech::tts(cfg, args, cwd).await,
            None => bail!("tts needs the loaded config; call it from the agent"),
        },
        "ratchet" => {
            let cfg = cfg.context("ratchet needs the loaded config; call it from the agent")?;
            anyhow::ensure!(cfg.extra_tools_opts.ratchet.enabled, "ratchet is disabled by ratchet.enabled");
            super::ratchet::run(cwd, args)
        }
        "archive" => {
            let cfg = cfg.context("archive needs the loaded config; call it from the agent")?;
            anyhow::ensure!(cfg.extra_tools_opts.archive.enabled, "archive is disabled by archive.enabled");
            super::archive::run(&crate::home(), args)
        }
        "vault" => {
            let cfg = cfg.context("vault needs the loaded config; call it from the agent")?;
            super::vault::command(cfg, args)
        }
        "report_issue" => {
            let cfg = cfg.context("report_issue needs the loaded config; call it from the agent")?;
            let payload = crate::discovery::report_payload(
                str_arg(args, "tool")?,
                &args["details"].clone(),
                str_arg(args, "summary")?,
                false,
                &cfg.model,
            );
            let path = crate::discovery::store(&crate::home().join("autoqa"), &payload)
                .map_err(|e| anyhow!("cannot store the report: {e}"))?;
            match crate::discovery::push(&cfg.discovery.dev.autoqa_push.endpoint, &payload).await {
                Ok(()) if !cfg.discovery.dev.autoqa_push.endpoint.trim().is_empty() => Ok(format!("reported to {} ({})", cfg.discovery.dev.autoqa_push.endpoint, path.display())),
                Ok(()) => Ok(format!("report saved to {}; set dev.autoqa_push.endpoint to send it", path.display())),
                Err(e) => Ok(format!("report saved to {}; push failed: {e:#}", path.display())),
            }
        }
        "security" => {
            let cfg = cfg.context("security needs the loaded config; call it from the agent")?;
            anyhow::ensure!(cfg.extra_tools_opts.security.enabled, "security is disabled by security.enabled");
            super::security::tool(&crate::home(), cwd, args)
        }
        other => bail!(
            "unknown ext tool `{other}`; available: {}",
            NAMES.join(", ")
        ),
    }
}

/// Words of a `find` query worth matching: lowercase, 3+ characters, common filler dropped.
fn find_terms(query: &str) -> Vec<String> {
    const STOP: [&str; 14] = [
        "the", "and", "for", "that", "this", "with", "from", "where", "what", "how", "does",
        "which", "when", "into",
    ];
    let mut terms: Vec<String> = query
        .split(|c: char| !c.is_alphanumeric() && c != '_')
        .map(str::to_lowercase)
        .filter(|w| w.chars().count() >= 3 && !STOP.contains(&w.as_str()))
        .collect();
    terms.sort();
    terms.dedup();
    terms
}

async fn find(cfg: Option<&crate::config::Config>, args: &Value, cwd: &Path) -> Result<String> {
    let query = str_arg(args, "query")?;
    let terms = find_terms(query);
    if terms.is_empty() {
        bail!("find needs a `query` with words of 3+ characters")
    }
    let path = resolve(cwd, args["path"].as_str().unwrap_or("."));
    let mut c = match crate::config::executable("rg") {
        Some(rg) => {
            let mut c = tokio::process::Command::new(rg);
            c.args([
                "-i",
                "-n",
                "-H",
                "--no-heading",
                "--color",
                "never",
                "-m",
                "200",
            ]);
            c
        }
        None => {
            let mut c = tokio::process::Command::new("grep");
            c.args([
                "-rniIEH",
                "--exclude-dir=.git",
                "--exclude-dir=target",
                "--exclude-dir=node_modules",
            ]);
            c
        }
    };
    for t in &terms {
        c.args(["-e", &regex_lite::escape(t)]);
    }
    c.arg("--").arg(&path);
    let out = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        c.current_dir(cwd)
            .stdin(std::process::Stdio::null())
            .kill_on_drop(true)
            .output(),
    )
    .await
    .map_err(|_| anyhow!("timed out after 30s"))??;
    let root = format!("{}/", cwd.display());
    // file -> (terms seen, hits, first line, last line, best line)
    let mut files: std::collections::HashMap<
        String,
        (
            std::collections::HashSet<usize>,
            usize,
            u64,
            u64,
            (usize, String),
        ),
    > = Default::default();
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let mut parts = line.splitn(3, ':');
        let (Some(file), Some(Ok(n)), Some(text)) = (
            parts.next(),
            parts.next().map(str::parse::<u64>),
            parts.next(),
        ) else {
            continue;
        };
        let lower = text.to_lowercase();
        let seen: Vec<usize> = terms
            .iter()
            .enumerate()
            .filter(|(_, t)| lower.contains(t.as_str()))
            .map(|(i, _)| i)
            .collect();
        let file = file
            .strip_prefix(root.as_str())
            .or_else(|| file.strip_prefix("./"))
            .unwrap_or(file);
        let e = files
            .entry(file.to_string())
            .or_insert_with(|| (Default::default(), 0, n, n, (0, String::new())));
        e.1 += 1;
        e.2 = e.2.min(n);
        e.3 = e.3.max(n);
        if seen.len() > e.4 .0 {
            e.4 = (
                seen.len(),
                format!("{n}: {}", text.trim().chars().take(120).collect::<String>()),
            );
        }
        e.0.extend(seen);
    }
    if files.is_empty() {
        return Ok("no matches; try other words or grep".into());
    }
    // Coverage of the query's words decides; the hit count only breaks ties.
    let mut ranked: Vec<_> = files
        .into_iter()
        .map(|(f, (seen, hits, a, b, best))| {
            (
                seen.len() as f64 / terms.len() as f64,
                hits,
                f,
                a,
                b,
                best.1,
            )
        })
        .collect();
    ranked.sort_by(|x, y| y.0.total_cmp(&x.0).then(y.1.cmp(&x.1)).then(x.2.cmp(&y.2)));
    let rows = ranked
        .iter()
        .take(10)
        .map(|(score, _, f, a, b, best)| format!("{f}:{a}-{b} {score:.2} {best}"))
        .collect::<Vec<_>>()
        .join("\n");
    // Verification only runs with a loaded config; without one the judged ranking stands.
    let Some(cfg) = cfg else { return Ok(rows) };
    // `spotlight.experiment.enabled`: rare query words outweigh common ones when ranking files.
    let rows = if cfg.discovery.spotlight.experiment.enabled {
        let hits: Vec<(String, String)> = ranked.iter().map(|(_, _, f, .., best)| (f.clone(), best.clone())).collect();
        crate::discovery::rank(query, hits).into_iter().map(|(f, best)| format!("{f} 1.00 {best}")).collect::<Vec<_>>().join("\n")
    } else {
        rows
    };
    Ok(crate::tools::find_verify::verify(cfg, query, &rows).await)
}

/// The `gh` arguments for one read-only github op.
fn github_args(args: &Value) -> Result<Vec<String>> {
    let num = |key: &str| {
        args[key]
            .as_u64()
            .map(|n| n.to_string())
            .ok_or_else(|| anyhow!("github needs a number `{key}`"))
    };
    let mut a: Vec<String> = match str_arg(args, "op")? {
        "repo_view" => vec!["repo".into(), "view".into()],
        // Without a tty `--comments` prints only the comments, so views show the body and metadata.
        "issue_view" => vec!["issue".into(), "view".into(), num("number")?],
        "pr_view" => vec!["pr".into(), "view".into(), num("number")?],
        "pr_diff" => vec!["pr".into(), "diff".into(), num("number")?],
        "pr_checks" => vec!["pr".into(), "checks".into(), num("number")?],
        op @ ("search_issues" | "search_prs") => vec![
            "search".into(),
            if op == "search_prs" { "prs" } else { "issues" }.into(),
            str_arg(args, "query")?.into(),
            "--limit".into(),
            "20".into(),
        ],
        "run_list" => vec!["run".into(), "list".into(), "--limit".into(), "10".into()],
        "run_view" => vec![
            "run".into(),
            "view".into(),
            num("run")?,
            "--log-failed".into(),
        ],
        other => bail!("unknown github op `{other}`"),
    };
    if let Some(repo) = args["repo"].as_str().filter(|r| !r.is_empty()) {
        // `gh repo view` takes the repo as an argument; search scopes it with --repo like the other commands.
        if a[0] == "repo" {
            a.push(repo.into())
        } else {
            a.extend(["--repo".into(), repo.into()])
        }
    }
    Ok(a)
}

async fn github(cfg: Option<&crate::config::Config>, args: &Value, cwd: &Path) -> Result<String> {
    let argv = github_args(args)?;
    // Only rendered views are cached; a diff or log is large and changes with every push.
    let view = matches!(args["op"].as_str(), Some("issue_view" | "pr_view" | "repo_view"));
    let key = super::github_cache::key(&argv, cwd);
    let fetch = {
        let argv = argv.clone();
        let cwd = cwd.to_path_buf();
        move || async move { gh(&argv, &cwd).await }
    };
    match cfg.filter(|_| view) {
        Some(cfg) => super::github_cache::serve(cfg, &key, fetch).await,
        None => fetch().await,
    }
}

async fn gh(argv: &[String], cwd: &Path) -> Result<String> {
    let gh = crate::config::executable("gh")
        .ok_or_else(|| anyhow!("gh is not installed (brew install gh, then gh auth login)"))?;
    let out = tokio::time::timeout(
        std::time::Duration::from_secs(60),
        tokio::process::Command::new(gh)
            .args(argv)
            .current_dir(cwd)
            .stdin(std::process::Stdio::null())
            .kill_on_drop(true)
            .output(),
    )
    .await
    .map_err(|_| anyhow!("gh timed out after 60s"))??;
    let text = String::from_utf8_lossy(&out.stdout);
    if !out.status.success() {
        bail!("gh failed: {}", String::from_utf8_lossy(&out.stderr).trim())
    }
    // Diffs and logs can be huge: keep the head, say how much was cut.
    const LIMIT: usize = 20_000;
    Ok(match text.char_indices().nth(LIMIT) {
        Some((at, _)) => format!(
            "{}\n[cut: {} of {} bytes shown]",
            &text[..at],
            at,
            text.len()
        ),
        None if text.trim().is_empty() => "(no output)".into(),
        None => text.into_owned(),
    })
}

/// Lists matching sessions, newest first, with the first matching line of each.
fn recall(query: &str) -> Result<String> {
    if query.trim().is_empty() {
        bail!("recall requires a non-empty `query`")
    }
    let hits = crate::session::Session::search(query);
    let q = query.to_lowercase();
    let found: Vec<String> = crate::session::Session::list()
        .into_iter()
        .filter(|m| hits.contains(&m.id))
        .take(8)
        .map(|m| {
            let text =
                std::fs::read_to_string(crate::session::Session::path(&m.id)).unwrap_or_default();
            let line = text
                .lines()
                .find(|l| l.to_lowercase().contains(&q))
                .unwrap_or("");
            let at = line
                .to_lowercase()
                .find(&q)
                .map_or(0, |i| line.to_lowercase()[..i].chars().count());
            let snippet: String = line
                .chars()
                .skip(at.saturating_sub(100))
                .take(300)
                .collect();
            format!("{} · {} · {}\n  {snippet}", m.id, m.title, m.cwd.display())
        })
        .collect();
    Ok(if found.is_empty() {
        format!("no session mentions `{query}`")
    } else {
        found.join("\n")
    })
}

async fn wait(args: &Value) -> Result<String> {
    let pid = args["pid"]
        .as_u64()
        .ok_or_else(|| anyhow!("wait requires an integer `pid`"))?
        .to_string();
    let seconds = args["seconds"].as_u64().unwrap_or(60).min(600);
    let start = std::time::Instant::now();
    loop {
        let alive = std::process::Command::new("kill")
            .args(["-0", &pid])
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success());
        if !alive {
            return Ok(format!("process {pid} has exited"));
        }
        if start.elapsed().as_secs() >= seconds {
            // `async.widgets`: the wait card carries the job's live widget.
            let card = crate::tools::running_job_list()
                .into_iter()
                .find(|(job, ..)| job.to_string() == pid)
                .map(|(.., log)| crate::tools::jobwidget::wait_card(&log))
                .filter(|card| !card.is_empty());
            let mut text = format!("process {pid} is still running after {seconds}s");
            if let Some(card) = card {
                text.push('\n');
                text.push_str(&card);
            }
            return Ok(text);
        }
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }
}

fn skill_file(name: &str) -> Result<std::path::PathBuf> {
    let valid = !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
    if !valid {
        bail!("skill `name` must be kebab-case (a-z, 0-9, -)")
    }
    Ok(crate::home().join("skills").join(name).join("SKILL.md"))
}

fn manage_skill(args: &Value) -> Result<String> {
    let name = str_arg(args, "name")?;
    let file = skill_file(name)?;
    match args["action"].as_str() {
        Some("delete") => {
            if !file.exists() {
                bail!("skill `{name}` does not exist")
            }
            std::fs::remove_dir_all(file.parent().unwrap())?;
            Ok(format!("deleted skill `{name}`"))
        }
        Some(action @ ("create" | "update")) => {
            if action == "create" && file.exists() {
                bail!("skill `{name}` already exists; use update")
            }
            if action == "update" && !file.exists() {
                bail!("skill `{name}` does not exist; use create")
            }
            let description = str_arg(args, "description")?.trim();
            let body = str_arg(args, "body")?.trim();
            if description.is_empty() || body.is_empty() || description.contains('\n') {
                bail!("skill needs a one-line `description` and a non-empty `body`")
            }
            std::fs::create_dir_all(file.parent().unwrap())?;
            std::fs::write(
                &file,
                format!("---\nname: {name}\ndescription: {description}\n---\n\n{body}\n"),
            )?;
            Ok(format!("saved {}", file.display()))
        }
        _ => bail!("manage_skill: action must be create, update or delete"),
    }
}

/// The first four hex digits of the file's SHA-256: enough to notice that it changed since `hread`.
fn tag(bytes: &[u8]) -> String {
    let hash = Sha256::digest(bytes);
    format!("{:02X}{:02X}", hash[0], hash[1])
}

fn hread(args: &Value, cwd: &Path) -> Result<String> {
    let given = str_arg(args, "path")?;
    let path = resolve(cwd, given);
    let bytes = std::fs::read(&path).with_context(|| path.display().to_string())?;
    let text = String::from_utf8_lossy(&bytes);
    Ok(format!(
        "[{given}#{}]\n{}",
        tag(&bytes),
        super::view(&text, args["offset"].as_u64(), args["limit"].as_u64(), true)
    ))
}

#[derive(PartialEq)]
enum Kind {
    Replace,
    Insert,
    Delete,
}

struct Op {
    kind: Kind,
    start: usize,
    end: usize,
    text: Vec<String>,
}

impl Op {
    /// Index in the line vector where the splice begins.
    fn at(&self) -> usize {
        if self.kind == Kind::Insert {
            self.start
        } else {
            self.start - 1
        }
    }
}

fn parse_ops(args: &Value, len: usize) -> Result<Vec<Op>> {
    let list = args["ops"]
        .as_array()
        .filter(|a| !a.is_empty())
        .ok_or_else(|| anyhow!("`ops` must be a non-empty array"))?;
    let mut ops = Vec::new();
    for (i, item) in list.iter().enumerate() {
        let n = i + 1;
        let kind = match item["op"].as_str() {
            Some("replace") => Kind::Replace,
            Some("insert") => Kind::Insert,
            Some("delete") => Kind::Delete,
            _ => bail!("op {n}: `op` must be replace, insert or delete"),
        };
        let start = item["start"]
            .as_u64()
            .ok_or_else(|| anyhow!("op {n}: `start` must be a line number"))?
            as usize;
        let end = if kind == Kind::Insert {
            start
        } else {
            item["end"].as_u64().map_or(start, |e| e as usize)
        };
        if kind == Kind::Insert {
            if start > len {
                bail!("op {n}: insert after line {start}, but the file has {len} lines");
            }
        } else {
            if start > end {
                bail!("op {n}: start {start} is after end {end}");
            }
            if start < 1 || end > len {
                bail!("op {n}: lines {start}..{end} are outside 1..{len}");
            }
        }
        let text = if kind == Kind::Delete {
            vec![]
        } else {
            let text = item["text"].as_str().ok_or_else(|| {
                anyhow!(
                    "op {n}: {} needs `text`",
                    if kind == Kind::Insert {
                        "insert"
                    } else {
                        "replace"
                    }
                )
            })?;
            text.strip_suffix('\n')
                .unwrap_or(text)
                .split('\n')
                .map(String::from)
                .collect()
        };
        ops.push(Op {
            kind,
            start,
            end,
            text,
        });
    }
    for (i, a) in ops.iter().enumerate() {
        for b in &ops[i + 1..] {
            let clash = match (a.kind == Kind::Insert, b.kind == Kind::Insert) {
                (true, true) => a.start == b.start,
                (true, false) => b.start <= a.start && a.start < b.end,
                (false, true) => a.start <= b.start && b.start < a.end,
                (false, false) => a.start <= b.end && b.start <= a.end,
            };
            if clash {
                bail!(
                    "ops overlap at lines {}..{} and {}..{}",
                    a.start,
                    a.end,
                    b.start,
                    b.end
                );
            }
        }
    }
    Ok(ops)
}

fn hedit(args: &Value, cwd: &Path) -> Result<String> {
    let given = str_arg(args, "path")?;
    let path = resolve(cwd, given);
    let bytes = std::fs::read(&path).with_context(|| path.display().to_string())?;
    let current = tag(&bytes);
    let claimed = str_arg(args, "tag")?
        .trim()
        .trim_start_matches('#')
        .to_uppercase();
    if claimed != current {
        bail!("stale tag: the file changed; hread it again (now #{current})");
    }
    let text =
        String::from_utf8(bytes).with_context(|| format!("{} is not UTF-8", path.display()))?;
    let eol = if text.contains("\r\n") { "\r\n" } else { "\n" };
    let trailing = text.is_empty() || text.ends_with('\n');
    let mut lines: Vec<String> = text.lines().map(String::from).collect();
    let mut ops = parse_ops(args, lines.len())?;
    let added: usize = ops.iter().map(|o| o.text.len()).sum();
    let removed: usize = ops
        .iter()
        .filter(|o| o.kind != Kind::Insert)
        .map(|o| o.end - o.start + 1)
        .sum();
    // Back to front keeps earlier line numbers valid. At one index the replaced lines go first, so an insert lands before them.
    ops.sort_by_key(|o| std::cmp::Reverse((o.at(), o.kind != Kind::Insert)));
    for op in ops {
        let upto = if op.kind == Kind::Insert {
            op.start
        } else {
            op.end
        };
        lines.splice(op.at()..upto, op.text);
    }
    let mut out = lines.join(eol);
    if trailing && !lines.is_empty() {
        out.push_str(eol);
    }
    std::fs::write(&path, &out).with_context(|| path.display().to_string())?;
    Ok(format!(
        "edited {given} (+{added} −{removed}) [{given}#{}]",
        tag(out.as_bytes())
    ))
}

async fn ast(args: &Value, cwd: &Path) -> Result<String> {
    let pattern = str_arg(args, "pattern")?;
    let bin = crate::config::executable("ast-grep")
        .or_else(|| crate::config::executable("sg"))
        .ok_or_else(|| anyhow!("ast-grep is not installed (brew install ast-grep)"))?;
    let rewrite = args["rewrite"].as_str();
    let path = resolve(cwd, args["path"].as_str().unwrap_or("."));
    if rewrite.is_some() && !(args["path"].is_string() && path.is_file()) {
        bail!("ast rewrite needs one file in `path`; search first, then rewrite each file");
    }
    let mut c = tokio::process::Command::new(bin);
    c.args(["run", "--pattern", pattern]);
    if let Some(lang) = args["lang"].as_str() {
        c.args(["--lang", lang]);
    }
    c.args(["--color", "never"]);
    match rewrite {
        Some(rewrite) => {
            c.args(["--rewrite", rewrite, "--update-all"]);
        }
        None => {
            c.args(["--heading", "never"]);
        }
    }
    c.arg("--").arg(&path);
    c.stderr(std::process::Stdio::piped());
    let out = super::lines(c, cwd).await?;
    Ok(match rewrite {
        Some(_) if out == "no matches" => out,
        Some(_) => format!("rewrote {}\n{out}", path.display()),
        None => out,
    })
}
