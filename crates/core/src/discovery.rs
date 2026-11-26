//! Discovery surface: bash-mounted tool commands, MCP sharing/notifications/transforms, extension
//! handler and renderer policy, Auto QA and Spotlight. `config::discovery` holds the settings.
use crate::{config::Config, permission::Mode, ToolDef};
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    path::Path,
    sync::Mutex,
    time::{Duration, Instant},
};

/// A tool exposed as a bash command instead of a top-level schema.
pub struct Mounted {
    pub name: String,
    pub help: String,
}

/// Mounted commands are only useful when the bash tool exists and `tool_commands.enabled` is on.
pub fn mounts(cfg: &Config) -> bool {
    cfg.discovery.tool_commands.enabled && !cfg.no_tools && (cfg.tools.is_empty() || cfg.tools.iter().any(|t| t == "bash"))
}

/// Renders a tool's schema the way `--help` shows it.
pub fn help_text(tool: &ToolDef) -> String {
    let props = tool.schema["properties"].as_object().cloned().unwrap_or_default();
    let required: Vec<&str> = tool.schema["required"].as_array().map(|a| a.iter().filter_map(Value::as_str).collect()).unwrap_or_default();
    let mut out = format!("{}\n\n{}", tool.name, tool.description);
    for (name, schema) in props {
        let mut tags: Vec<&str> = schema["type"].as_str().into_iter().collect();
        if required.contains(&name.as_str()) {
            tags.push("required");
        }
        out.push_str(&format!("\n  {name}"));
        if !tags.is_empty() {
            out.push_str(&format!(" ({})", tags.join(", ")));
        }
        if let Some(items) = schema["enum"].as_array() {
            let values: Vec<&str> = items.iter().filter_map(Value::as_str).collect();
            out.push_str(&format!(": {}", values.join("|")));
        } else if let Some(description) = schema["description"].as_str() {
            out.push_str(&format!(": {description}"));
        }
    }
    out
}

/// The commands mounted for these tools; `hidden` keeps a tool top-level.
pub fn catalog(defs: &[ToolDef], hidden: &[&str], dynamic: &[(&str, String)]) -> Vec<Mounted> {
    let mut out: Vec<Mounted> = defs
        .iter()
        .filter(|tool| !hidden.contains(&tool.name.as_str()))
        .map(|tool| Mounted { name: tool.name.clone(), help: help_text(tool) })
        .collect();
    out.extend(dynamic.iter().map(|(name, help)| Mounted { name: (*name).to_string(), help: help.clone() }));
    out
}

/// `find_issue({...})` / `find_issue --name x`: the command word plus a JSON or `key=value` argument object.
pub fn parse<'a>(command: &'a str, names: &[String]) -> Option<(&'a str, Value)> {
    let line = command.trim();
    // Only a bare command line is dispatched; pipes, redirects and sequencing keep normal shell semantics.
    if line.is_empty() || line.contains(['|', '>', '<', '&', ';', '\n']) {
        return None;
    }
    let mut words = line.split_whitespace();
    let head = words.next()?;
    if !names.iter().any(|name| name == head) {
        return None;
    }
    let rest: Vec<&str> = words.collect();
    if rest.is_empty() {
        return Some((head, json!({})));
    }
    if rest[0] == "--help" {
        return Some((head, json!({"__help": true})));
    }
    if rest[0].starts_with('{') {
        return serde_json::from_str(&rest.join(" ")).ok().map(|args| (head, args));
    }
    let mut args = serde_json::Map::new();
    for word in rest {
        let (key, value) = word.split_once('=')?;
        args.insert(key.trim_start_matches('-').into(), Value::String(value.into()));
    }
    Some((head, Value::Object(args)))
}

/// `--help` text for one mounted command, or a listing of every command.
pub fn help_for(catalog: &[Mounted], name: &str) -> String {
    match catalog.iter().find(|m| m.name == name) {
        Some(m) => m.help.clone(),
        None => {
            let mut out = String::from("Mounted tool commands. Pass --help to one for its arguments:\n");
            for m in catalog {
                out.push_str(&format!("  {}\n", m.name));
            }
            out
        }
    }
}

/// The description line of a help text: the first line after the `name` and blank line.
fn one_line(help: &str) -> &str {
    help.split_once("\n\n").and_then(|(_, rest)| rest.lines().next()).unwrap_or_default()
}

fn glob(pattern: &str, name: &str) -> bool {
    match pattern.split_once('*') {
        None => pattern == name,
        Some((head, tail)) => {
            name.len() >= head.len() + tail.len()
                && name.starts_with(head)
                && (tail.is_empty() || name.ends_with(tail))
                && (pattern.matches('*').count() > 1 || !tail.contains('*'))
        }
    }
}

/// The "mounted commands" section of the system prompt for the configured docs mode.
pub fn docs_section(cfg: &Config, catalog: &[Mounted]) -> String {
    use crate::config::discovery::CommandDocs;
    if catalog.is_empty() {
        return String::new();
    }
    let docs = cfg.discovery.tool_commands.docs;
    let mut out = String::from("\n\n# Tool commands\nRarely used tools are mounted as bash commands; a JSON object argument calls one, `--help` prints its arguments.\n");
    match docs {
        CommandDocs::Inline => {
            for m in catalog {
                out.push_str(&format!("\n{}\n", m.help));
            }
        }
        CommandDocs::Builtins => {
            for m in catalog {
                let dynamic = cfg.discovery.tool_commands.inline.iter().any(|p| glob(p, &m.name));
                out.push_str(&format!("\n{}: {}", m.name, if dynamic { one_line(&m.help) } else { "on demand (`--help`)" }));
            }
        }
        CommandDocs::Catalog => {
            let names: Vec<&str> = catalog.iter().map(|m| m.name.as_str()).collect();
            out.push_str(&format!("Commands: {}", names.join(", ")));
        }
    }
    out
}

/// Notice text for a mount-set change; `None` when notices are off or nothing changed.
pub fn notice(cfg: &Config, before: &[String], after: &[String]) -> Option<String> {
    if !cfg.discovery.tool_commands.notices {
        return None;
    }
    let added: Vec<&str> = after.iter().filter(|n| !before.contains(n)).map(String::as_str).collect();
    let removed: Vec<&str> = before.iter().filter(|n| !after.contains(n)).map(String::as_str).collect();
    if added.is_empty() && removed.is_empty() {
        return None;
    }
    let mut parts = Vec::new();
    if !added.is_empty() {
        parts.push(format!("mounted: {}", added.join(", ")));
    }
    if !removed.is_empty() {
        parts.push(format!("unmounted: {}", removed.join(", ")));
    }
    Some(format!("Tool commands {}", parts.join("; ")))
}

/// The first word of `command` as a mounted command name, for a dispatch through the owning tool.
pub fn dispatch<'a>(cfg: &Config, catalog: &[Mounted], command: &'a str) -> Option<(&'a str, Value)> {
    if !mounts(cfg) {
        return None;
    }
    let names: Vec<String> = catalog.iter().map(|m| m.name.clone()).collect();
    parse(command, &names)
}

/// Injectable update queue for `mcp.notifications` + `mcp.notification_debounce_ms`.
#[derive(Default)]
pub struct Notifier {
    pending: Mutex<BTreeMap<String, Value>>,
    since: Mutex<Option<Instant>>,
}

impl Notifier {
    pub fn record(&self, uri: &str, update: Value) {
        self.pending.lock().unwrap().insert(uri.into(), update);
        self.since.lock().unwrap().get_or_insert_with(Instant::now);
    }

    /// The debounced update lines, once the window elapsed; `None` keeps them queued.
    pub fn drain(&self, debounce: Duration) -> Option<Vec<String>> {
        if self.pending.lock().unwrap().is_empty() {
            return None;
        }
        let since = (*self.since.lock().unwrap())?;
        if since.elapsed() < debounce {
            return None;
        }
        *self.since.lock().unwrap() = None;
        let updates = std::mem::take(&mut *self.pending.lock().unwrap());
        Some(updates.into_iter().map(|(uri, v)| format!("MCP resource updated: {uri} {v}")).collect())
    }
}

/// `mcp.builtin_transforms`: tools a native tool replaces, per server.
pub fn transform(server: &str, enabled: bool) -> Option<&'static str> {
    if !enabled {
        return None;
    }
    match server.to_ascii_lowercase().as_str() {
        s if s.contains("codex") => Some("eval runs JavaScript natively"),
        s if s.contains("playwright") || s.contains("browser") => Some("the browser tool replaces it"),
        _ => None,
    }
}

/// Filters a server's tools through the transforms; each skipped tool names its replacement.
pub fn apply_transforms<'a>(cfg: &Config, server: &str, tools: &'a [Value]) -> (Vec<&'a Value>, Vec<String>) {
    match transform(server, cfg.integration_opts.mcp.builtin_transforms) {
        None => (tools.iter().collect(), Vec::new()),
        Some(why) => (
            Vec::new(),
            tools.iter().map(|t| format!("{}: skipped ({why})", t["name"].as_str().unwrap_or("?"))).collect(),
        ),
    }
}

/// `mcp.eval_namespaces`: a prelude that turns each `server.tool(args)` into an `mcp` call.
pub fn eval_prelude(servers: &[String]) -> String {
    let mut out = String::from("# MCP namespaces: `server.tool(**args)` runs the MCP tool through greencode-cli.\n");
    for server in servers {
        out.push_str(&format!(
            "class __Mcp{i}:\n    def __getattr__(self, tool):\n        return lambda **a: __mcp({server:?}, tool, a)\n{server_var} = __Mcp{i}()\n",
            i = 0,
            server_var = server.replace(['-', '.'], "_"),
        ));
    }
    out
}

/// `extension_handlers.tool_call_timeout_ms`, with OMP's 30000 ms fallback for invalid values.
pub fn hook_timeout(cfg: &Config) -> Duration {
    cfg.discovery.extension_handlers.timeout()
}

/// `extension_handlers.mode_control`: the mode a `tool_call` hook asks for, when allowed.
pub fn mode_request(cfg: &Config, reply: &Value) -> Option<Mode> {
    if !cfg.discovery.extension_handlers.mode_control {
        return None;
    }
    match reply["mode"].as_str()?.to_ascii_lowercase().as_str() {
        "plan" => Some(Mode::Plan),
        "ask" => Some(Mode::Ask),
        "auto-edit" | "autoedit" | "vibe" => Some(Mode::AutoEdit),
        "full" => Some(Mode::Full),
        _ => None,
    }
}

/// `extension_renderers.assistant_text`: may a `render_assistant` hook redraw this reply?
pub fn renders_assistant(cfg: &Config, hook: bool) -> bool {
    hook && cfg.discovery.extension_renderers.assistant_text
}

/// `extension_renderers.transcript_presenters`: may a `present_transcript` hook draw this block?
pub fn presents_transcript(cfg: &Config, hook: bool) -> bool {
    hook && cfg.discovery.extension_renderers.transcript_presenters
}

/// Spotlight: output above this many lines is shaped.
const SPOTLIGHT_LINES: usize = 200;

/// `spotlight.experiment.enabled`: keep head and tail of broad output, and log the reduction.
pub fn shape(cfg: &Config, tool: &str, output: &str) -> (String, Option<Value>) {
    if !cfg.discovery.spotlight.experiment.enabled {
        return (output.to_string(), None);
    }
    let lines: Vec<&str> = output.lines().collect();
    if lines.len() <= SPOTLIGHT_LINES + 40 {
        return (output.to_string(), None);
    }
    let head = SPOTLIGHT_LINES;
    let tail = 40;
    let marker = format!("\n... {} lines elided by spotlight ...\n", lines.len() - head - tail);
    let shaped = format!("{}{}{}", lines[..head].join("\n"), marker, lines[lines.len() - tail..].join("\n"));
    let log = json!({"spotlight": {"tool": tool, "before": lines.len(), "after": head + tail}});
    (shaped, Some(log))
}

/// Spotlight ranking: rarer query terms weigh more, so a file holding the distinctive term wins.
pub fn rank(pattern: &str, mut hits: Vec<(String, String)>) -> Vec<(String, String)> {
    let terms: Vec<String> = pattern
        .split(|c: char| !c.is_alphanumeric() && c != '_')
        .filter(|t| !t.is_empty() && !["and", "or", "not", "the"].contains(t))
        .map(str::to_string)
        .collect();
    if terms.is_empty() {
        return hits;
    }
    let doc_freq: BTreeMap<&str, usize> = terms
        .iter()
        .map(|t| (t.as_str(), hits.iter().filter(|(_, text)| text.contains(t.as_str())).count()))
        .collect();
    hits.sort_by(|a, b| {
        let score = |text: &str| terms.iter().filter(|t| text.contains(t.as_str())).map(|t| 1. / doc_freq[t.as_str()].max(1) as f64).sum::<f64>();
        score(&b.1).partial_cmp(&score(&a.1)).unwrap_or(std::cmp::Ordering::Equal)
    });
    hits
}

/// Auto QA report body; `report_issue` sends exactly this.
pub fn report_payload(tool: &str, input: &Value, output: &str, error: bool, model: &str) -> Value {
    json!({
        "kind": "autoqa",
        "version": env!("CARGO_PKG_VERSION"),
        "tool": tool,
        "is_error": error,
        "model": model,
        "input": input,
        "output": output.chars().take(4_000).collect::<String>(),
        "at": crate::telemetry::millis(),
    })
}

/// `dev.autoqa`: is reporting enabled at all?
pub fn autoqa_enabled(cfg: &Config) -> bool {
    cfg.discovery.dev.autoqa
}

/// Stores a report under `~/.greencode/autoqa/` and returns its path.
pub fn store(dir: &Path, payload: &Value) -> std::io::Result<std::path::PathBuf> {
    std::fs::create_dir_all(dir)?;
    let path = dir.join(format!("grievance-{}.json", payload["at"].as_u64().unwrap_or_default()));
    std::fs::write(&path, serde_json::to_vec_pretty(payload).unwrap_or_default())?;
    Ok(path)
}

/// `dev.autoqaPush.endpoint`: POSTs the report; an empty endpoint keeps it local.
pub async fn push(endpoint: &str, payload: &Value) -> anyhow::Result<()> {
    if endpoint.trim().is_empty() {
        return Ok(());
    }
    reqwest::Client::new()
        .post(endpoint)
        .json(payload)
        .send()
        .await?
        .error_for_status()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{discovery::CommandDocs, Config};

    fn tool(name: &str) -> ToolDef {
        ToolDef {
            name: name.into(),
            description: "does a thing".into(),
            schema: json!({"type": "object", "properties": {"path": {"type": "string"}, "mode": {"enum": ["a", "b"]}}, "required": ["path"]}),
        }
    }

    #[test]
    fn commands_parse_and_print_help() {
        let cat = catalog(&[tool("find_issue"), tool("read")], &["read"], &[]);
        assert_eq!(cat.len(), 1);
        assert!(cat[0].help.contains("path (string, required)") && cat[0].help.contains("mode: a|b"));
        let names: Vec<String> = cat.iter().map(|m| m.name.clone()).collect();
        assert_eq!(parse("find_issue {\"path\": \"x\"}", &names).unwrap().1["path"], "x");
        assert_eq!(parse("find_issue --path=x", &names).unwrap().1["path"], "x");
        assert_eq!(parse("find_issue --help", &names).unwrap().1["__help"], true);
        assert!(parse("find_issue | head", &names).is_none());
        assert!(parse("echo find_issue", &names).is_none());
        assert!(help_for(&cat, "find_issue").contains("does a thing"));
    }

    #[test]
    fn docs_modes_and_notices() {
        let mut cfg = Config::default();
        let cat = catalog(&[tool("find_issue"), tool("mcp__ctx__search")], &[], &[]);
        cfg.discovery.tool_commands.docs = CommandDocs::Inline;
        assert!(docs_section(&cfg, &cat).contains("does a thing"));
        cfg.discovery.tool_commands.docs = CommandDocs::Catalog;
        let catalog_only = docs_section(&cfg, &cat);
        assert!(catalog_only.contains("Commands: find_issue, mcp__ctx__search") && !catalog_only.contains("does a thing"));
        cfg.discovery.tool_commands.inline = vec!["mcp__ctx_*".into()];
        cfg.discovery.tool_commands.docs = CommandDocs::Builtins;
        let builtins = docs_section(&cfg, &cat);
        assert!(builtins.contains("find_issue: on demand"));
        assert!(builtins.contains("mcp__ctx__search: does a thing"), "{builtins}");
        assert_eq!(notice(&cfg, &["a".into()], &["b".into()]).unwrap(), "Tool commands mounted: b; unmounted: a");
        cfg.discovery.tool_commands.notices = false;
        assert!(notice(&cfg, &["a".into()], &["b".into()]).is_none());
        assert!(dispatch(&cfg, &cat, "find_issue {}").is_some());
        cfg.tools = vec!["read".into()];
        assert!(dispatch(&cfg, &cat, "find_issue {}").is_none(), "no bash tool, no mounts");
    }

    #[test]
    fn notifications_debounce_and_transforms() {
        let n = Notifier::default();
        n.record("a://1", json!({}));
        assert!(n.drain(Duration::from_secs(30)).is_none());
        std::thread::sleep(Duration::from_millis(5));
        n.record("a://2", json!({}));
        let lines = n.drain(Duration::ZERO).unwrap();
        assert_eq!(lines.len(), 2, "both updates share one injection");
        assert!(n.drain(Duration::ZERO).is_none());
        let cfg = Config::default();
        let tools = vec![json!({"name": "node_repl"}), json!({"name": "shell"})];
        let (kept, skipped) = apply_transforms(&cfg, "codex", &tools);
        assert!(kept.is_empty() && skipped[0].contains("eval runs JavaScript"));
        let (kept, _) = apply_transforms(&cfg, "github", &tools);
        assert_eq!(kept.len(), 2);
        assert!(eval_prelude(&["github".into()]).contains("github = __Mcp0()"));
    }

    #[test]
    fn handler_limits_modes_and_renderers() {
        let mut cfg = Config::default();
        assert_eq!(hook_timeout(&cfg).as_millis(), 30_000);
        cfg.discovery.extension_handlers.tool_call_timeout_ms = 5_000;
        assert_eq!(hook_timeout(&cfg).as_millis(), 5_000);
        assert_eq!(mode_request(&cfg, &json!({"mode": "plan"})), Some(Mode::Plan));
        assert_eq!(mode_request(&cfg, &json!({"mode": "vibe"})), Some(Mode::AutoEdit));
        assert_eq!(mode_request(&cfg, &json!({"mode": "goal"})), None);
        assert!(renders_assistant(&cfg, true) && !renders_assistant(&cfg, false));
        cfg.discovery.extension_renderers.transcript_presenters = false;
        assert!(!presents_transcript(&cfg, true));
    }

    #[test]
    fn spotlight_shapes_and_ranks() {
        let mut cfg = Config::default();
        let long: String = (0..400).map(|i| format!("line {i}\n")).collect();
        assert_eq!(shape(&cfg, "grep", &long).0, long);
        cfg.discovery.spotlight.experiment.enabled = true;
        let (shaped, log) = shape(&cfg, "grep", &long);
        assert!(shaped.contains("160 lines elided by spotlight") && shaped.ends_with("line 399"));
        assert_eq!(log.unwrap()["spotlight"]["after"], 240);
        let hits = vec![("common.rs".into(), "foo bar".into()), ("rare.rs".into(), "foo zzz".into())];
        assert_eq!(rank("foo zzz", hits)[0].0, "rare.rs");
    }

    #[test]
    fn autoqa_payloads_store_and_push() {
        let cfg = Config::default();
        assert!(autoqa_enabled(&cfg));
        let payload = report_payload("bash", &json!({"command": "ls"}), "boom", true, "m");
        assert_eq!(payload["tool"], "bash");
        let dir = std::env::temp_dir().join(format!("greencode-autoqa-{}", std::process::id()));
        let path = store(&dir, &payload).unwrap();
        assert!(std::fs::read_to_string(&path).unwrap().contains("boom"));
        let _ = std::fs::remove_dir_all(dir);
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!("http://{}/v1/grievances", listener.local_addr().unwrap());
        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = [0u8; 4096];
            let read = std::io::Read::read(&mut stream, &mut buf).unwrap();
            let body = String::from_utf8_lossy(&buf[..read]).to_string();
            std::io::Write::write_all(&mut stream, b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\n\r\n").unwrap();
            body
        });
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(push(&endpoint, &json!({"tool": "bash"})))
            .unwrap();
        let request = handle.join().unwrap();
        assert!(request.contains("POST /v1/grievances"));
    }
}
