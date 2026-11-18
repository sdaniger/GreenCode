use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, path::{Path, PathBuf}, time::Duration};

#[derive(Clone, Debug)]
pub struct AgentDef {
    pub name: String,
    pub description: String,
    pub tools: Option<Vec<String>>,
    pub model: Option<String>,
    pub prompt: String,
    pub bundled: bool,
}

pub fn bundled() -> Vec<AgentDef> {
    [
        ("task", "General-purpose worker with every tool; the default", None, "Finish only the assigned work. Edit and run what the task needs. Return a concise result; the user cannot see you."),
        ("scout", "Read-only codebase research; returns findings with path:line anchors", Some(vec!["read", "grep", "glob", "web_fetch", "web_search"]), "Research only; do not modify files. Return findings with path:line anchors."),
        ("reviewer", "Read-only code review; may run read-only git commands", Some(vec!["read", "grep", "glob", "bash"]), "Never modify files. Use bash only for read-only commands such as git diff, git log, and git show. Report concrete findings with file:line and severity."),
        ("security", "Read-only security audit; reports exploitable findings with evidence", Some(vec!["read", "grep", "glob", "bash"]), "Never modify files. Use bash only for read-only commands. Look for injection, path traversal, secrets in code, unsafe deserialization, missing authorization and unvalidated input at trust boundaries. Report only findings with a concrete path:line, an exploit sketch and severity; say so when nothing is found."),
    ].into_iter().map(|(name, description, tools, prompt)| AgentDef { name: name.into(), description: description.into(), tools: tools.map(|v| v.into_iter().map(String::from).collect()), model: None, prompt: prompt.into(), bundled: true }).collect()
}

pub fn tool_name(raw: &str) -> Option<&'static str> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "read" => Some("read"), "write" => Some("write"), "edit" | "multiedit" => Some("edit"),
        "bash" => Some("bash"), "grep" => Some("grep"), "glob" | "find" => Some("glob"),
        "webfetch" | "web_fetch" => Some("web_fetch"), "websearch" | "web_search" => Some("web_search"),
        "todo" | "todowrite" => Some("todo"), "memory" => Some("memory"), "ext" => Some("ext"), _ => None,
    }
}

pub fn parse(text: &str, fallback_name: &str) -> Option<AgentDef> {
    let mut name = fallback_name.to_string();
    let mut description = None;
    let mut tools = None;
    let mut model = None;
    let mut body = text;
    if text.lines().next().is_some_and(|line| line.trim() == "---") {
        let start = text.find('\n')? + 1;
        let mut end = None;
        let mut offset = start;
        for line in text[start..].split_inclusive('\n') {
            if line.trim() == "---" { end = Some((offset, offset + line.len())); break; }
            offset += line.len();
        }
        let (end, after) = end?;
        body = &text[after..];
        for line in text[start..end].lines() {
            let Some((key, value)) = line.split_once(':') else { continue };
            let value = value.trim().trim_matches(|c| c == '\'' || c == '"').trim();
            match key.trim() {
                "name" if !value.is_empty() => name = value.into(),
                "description" => description = Some(value.to_string()),
                "model" if value.contains('/') => model = Some(value.to_string()),
                "tools" => {
                    let value = value.trim_matches(|c| c == '[' || c == ']').trim();
                    let items: Vec<_> = value.split(',').map(|s| s.trim().trim_matches(|c| c == '\'' || c == '"')).filter(|s| !s.is_empty()).collect();
                    if !items.is_empty() && !items.contains(&"*") {
                        let list: Vec<String> = items.into_iter().filter_map(tool_name).map(String::from).collect();
                        if list.is_empty() { return None; }
                        tools = Some(list);
                    }
                }
                _ => {}
            }
        }
    }
    Some(AgentDef { description: description.unwrap_or_else(|| name.clone()), name, tools, model, prompt: body.trim().into(), bundled: false })
}

fn discover_in(cwd: &Path, user: &Path, home: &Path) -> Vec<AgentDef> {
    let mut defs = bundled();
    let dirs: [PathBuf; 4] = [user.join(".claude/agents"), home.join("agents"), cwd.join(".claude/agents"), cwd.join(".greencode/agents")];
    for dir in dirs {
        let Ok(entries) = std::fs::read_dir(dir) else { continue };
        let mut paths: Vec<_> = entries.flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|e| e == "md")).collect();
        paths.sort();
        for path in paths {
            let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else { continue };
            let Ok(text) = std::fs::read_to_string(&path) else { continue };
            let Some(def) = parse(&text, stem) else { continue };
            if let Some(existing) = defs.iter_mut().find(|d| d.name == def.name) { *existing = def; } else { defs.push(def); }
        }
    }
    defs
}

pub fn discover(cwd: &Path) -> Vec<AgentDef> {
    let user = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
    discover_in(cwd, &user, &crate::home())
}

pub fn find(cwd: &Path, name: &str) -> Option<AgentDef> { discover(cwd).into_iter().find(|d| d.name == name) }

pub fn listing(defs: &[AgentDef]) -> String {
    const MAX_AGENTS: usize = 10;
    const MAX_DESCRIPTION: usize = 60;
    let custom: Vec<&AgentDef> = defs.iter().filter(|d| !d.bundled).collect();
    let mut lines: Vec<String> = custom.iter().take(MAX_AGENTS).map(|d| {
        // The system prompt is sent on every request: first line only, bounded.
        let first = d.description.lines().next().unwrap_or("").trim();
        let short: String = first.chars().take(MAX_DESCRIPTION).collect();
        format!("- {}: {}{}", d.name, short, if short.len() < first.len() { "…" } else { "" })
    }).collect();
    if custom.len() > MAX_AGENTS { lines.push(format!("- … {} more (greencode-cli --subagents)", custom.len() - MAX_AGENTS)); }
    lines.join("\n")
}

/// Whether a `task` call may choose its own model: `eval` only concerns eval-script spawns, which the native harness has not got, so it behaves like `off` here.
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum PerCallModel { Off, Eval, #[default] On }

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct TaskIsolation {
    /// Allow `isolated: true` (a git worktree per task).
    pub enabled: bool,
}
impl Default for TaskIsolation { fn default() -> Self { Self { enabled: true } } }

/// `[task]` in config.toml. Defaults keep the behaviour before these settings existed.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct TaskConfig {
    /// Sub-agents running at once; one waiting only for its own children does not count.
    pub max_concurrency: usize,
    /// Nesting levels: the main agent is 0, so 2 lets its tasks spawn tasks; 0 removes `task`.
    pub max_recursion_depth: u8,
    /// Wall-clock limit per sub-agent once it runs; 0 = none.
    pub max_runtime_ms: u64,
    pub disabled_agents: Vec<String>,
    /// Agent name -> `provider/model`; beats the agent definition's own model.
    pub agent_model_overrides: BTreeMap<String, String>,
    pub per_call_model: PerCallModel,
    /// Exact `provider/model` selectors a per-call model must be listed in; empty = any.
    pub per_call_model_allow: Vec<String>,
    /// Allow a per-task result `schema`.
    pub structured: bool,
    pub isolation: TaskIsolation,
    /// Allow `background: true` on `task` (does not gate bash).
    pub background: bool,
}

impl Default for TaskConfig {
    fn default() -> Self {
        Self { max_concurrency: 4, max_recursion_depth: 2, max_runtime_ms: 0, disabled_agents: vec![], agent_model_overrides: BTreeMap::new(), per_call_model: PerCallModel::On, per_call_model_allow: vec![], structured: true, isolation: TaskIsolation::default(), background: true }
    }
}

pub const MAX_CONCURRENCY: usize = 64;
pub const MAX_RECURSION_DEPTH: u8 = 8;
pub const MAX_RUNTIME_MS: u64 = 86_400_000;

/// `provider/model` without whitespace.
pub fn model_spec(spec: &str) -> Result<(&str, &str), String> {
    spec.split_once('/')
        .filter(|(p, m)| !p.is_empty() && !m.is_empty() && !spec.chars().any(char::is_whitespace))
        .ok_or_else(|| format!("task model must be `provider/model`, got `{spec}`"))
}

impl TaskConfig {
    pub fn validate(&self) -> Result<(), String> {
        if !(1..=MAX_CONCURRENCY).contains(&self.max_concurrency) { return Err(format!("task.max_concurrency must be 1..={MAX_CONCURRENCY}")); }
        if self.max_recursion_depth > MAX_RECURSION_DEPTH { return Err(format!("task.max_recursion_depth must be 0..={MAX_RECURSION_DEPTH}")); }
        if self.max_runtime_ms > MAX_RUNTIME_MS { return Err(format!("task.max_runtime_ms must be 0..={MAX_RUNTIME_MS}")); }
        if self.disabled_agents.iter().any(|a| a.trim().is_empty()) { return Err("task.disabled_agents must not hold empty names".into()); }
        for (agent, model) in &self.agent_model_overrides {
            if agent.trim().is_empty() { return Err("task.agent_model_overrides must not hold empty agent names".into()); }
            model_spec(model).map_err(|e| format!("task.agent_model_overrides.{agent}: {e}"))?;
        }
        for model in &self.per_call_model_allow { model_spec(model).map_err(|e| format!("task.per_call_model_allow: {e}"))?; }
        Ok(())
    }

    /// Clamped copies of the numeric limits, so an unvalidated config cannot hang or overflow the runtime.
    pub fn slots(&self) -> usize { self.max_concurrency.clamp(1, MAX_CONCURRENCY) }
    pub fn depth_limit(&self) -> u8 { self.max_recursion_depth.min(MAX_RECURSION_DEPTH) }
    pub fn runtime(&self) -> Option<Duration> { (self.max_runtime_ms > 0).then(|| Duration::from_millis(self.max_runtime_ms.min(MAX_RUNTIME_MS))) }
    pub fn agent_enabled(&self, name: &str) -> bool { !self.disabled_agents.iter().any(|a| a == name) }

    /// Whether an agent at `depth` may spawn: `depth` below the limit and some agent enabled.
    pub fn can_spawn(&self, depth: u8, defs: &[AgentDef]) -> bool { depth < self.depth_limit() && defs.iter().any(|d| self.agent_enabled(&d.name)) }

    /// Agents a `task` call may name; empty when nothing can spawn from the main agent.
    pub fn visible(&self, defs: &[AgentDef]) -> Vec<AgentDef> {
        if !self.can_spawn(0, defs) { return vec![]; }
        defs.iter().filter(|d| self.agent_enabled(&d.name)).cloned().collect()
    }

    /// A per-call `model` the caller asked for.
    pub fn check_call_model(&self, spec: &str) -> Result<(), String> {
        if self.per_call_model != PerCallModel::On { return Err("a per-task `model` is disabled by task.per_call_model".into()); }
        model_spec(spec)?;
        if !self.per_call_model_allow.is_empty() && !self.per_call_model_allow.iter().any(|m| m == spec) {
            return Err(format!("model `{spec}` is not in task.per_call_model_allow: {}", self.per_call_model_allow.join(", ")));
        }
        Ok(())
    }

    /// The model an agent runs on when the call names none: settings override, then its definition.
    pub fn agent_model(&self, def: &AgentDef) -> Option<String> { self.agent_model_overrides.get(&def.name).cloned().or_else(|| def.model.clone()) }

    /// Hides what the settings forbid from the `task` tool ad. Default settings leave it byte-identical (cached prefix).
    pub fn adjust_ad(&self, tool: &mut crate::ToolDef) {
        const AGENTS: &str = "`agent`: `task` (all tools), `scout`/`reviewer`/`security` (read-only), or a custom one.";
        const OPTIONS: &str = "`context` is shared; `model` overrides the default; `isolated` uses a worktree; `background` returns at once.";
        let item = &mut tool.schema["properties"]["tasks"]["items"]["properties"];
        let mut options = vec!["`context` is shared"];
        if self.per_call_model == PerCallModel::On { options.push("`model` overrides the default") } else { item.as_object_mut().map(|o| o.remove("model")); }
        if self.isolation.enabled { options.push("`isolated` uses a worktree") } else { item.as_object_mut().map(|o| o.remove("isolated")); }
        if !self.structured { item.as_object_mut().map(|o| o.remove("schema")); }
        if self.background { options.push("`background` returns at once") } else { tool.schema["properties"].as_object_mut().map(|o| o.remove("background")); }
        let mut agents = Vec::new();
        if self.agent_enabled("task") { agents.push("`task` (all tools)".to_string()); }
        let read_only: Vec<String> = ["scout", "reviewer", "security"].into_iter().filter(|n| self.agent_enabled(n)).map(|n| format!("`{n}`")).collect();
        if !read_only.is_empty() { agents.push(format!("{} (read-only)", read_only.join("/"))); }
        agents.push("or a custom one".into());
        tool.description = tool.description.replace(AGENTS, &format!("`agent`: {}.", agents.join(", "))).replace(OPTIONS, &format!("{}.", options.join("; ")));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parses_definitions() {
        let d = parse("---\nname: 'example'\ndescription: \"A worker\"\ntools: [Read, Grep, Glob, Bash, Task, Unknown]\nmodel: anthropic/claude-x\n---\nHello", "fallback").unwrap();
        assert_eq!(d.name, "example"); assert_eq!(d.description, "A worker");
        assert_eq!(d.tools.unwrap(), ["read", "grep", "glob", "bash"]); assert_eq!(d.model.as_deref(), Some("anthropic/claude-x")); assert_eq!(d.prompt, "Hello");
        assert!(parse("---\ntools: Task, unknown\n---\n", "x").is_none());
        let d = parse("---\ntools: Read, Write\nmodel: sonnet\n---\n", "x").unwrap();
        assert_eq!(d.tools.unwrap(), ["read", "write"]); assert!(d.model.is_none());
    }
    #[test]
    fn project_overrides_user_and_bundled() {
        let root = std::env::temp_dir().join(format!("greencode-agents-test-{}", std::process::id()));
        let user = root.join("user"); let home = root.join("home"); let project = root.join("project");
        std::fs::create_dir_all(home.join("agents")).unwrap(); std::fs::create_dir_all(project.join(".greencode/agents")).unwrap();
        std::fs::write(home.join("agents/x.md"), "---\nname: scout\ndescription: user\n---\n").unwrap();
        std::fs::write(project.join(".greencode/agents/x.md"), "---\nname: scout\ndescription: project\n---\n").unwrap();
        let defs = discover_in(&project, &user, &home); let d = defs.iter().find(|d| d.name == "scout").unwrap();
        assert_eq!(d.description, "project"); assert!(!d.bundled); let _ = std::fs::remove_dir_all(root);
    }
    #[test]
    fn task_settings_parse_validate_and_shape_the_ad() {
        let ad = || crate::tools::defs(false, &[]).into_iter().find(|t| t.name == "task").unwrap();
        let base = ad();
        let mut same = ad(); TaskConfig::default().adjust_ad(&mut same);
        assert_eq!(same.description, base.description); assert_eq!(same.schema, base.schema);
        let c: TaskConfig = toml::from_str("max_concurrency = 1\nper_call_model = 'eval'\ndisabled_agents = ['scout']\nstructured = false\nbackground = false\n[isolation]\nenabled = false\n[agent_model_overrides]\nreviewer = 'a/b'").unwrap();
        c.validate().unwrap();
        let mut t = ad(); c.adjust_ad(&mut t);
        let props = &t.schema["properties"]["tasks"]["items"]["properties"];
        assert!(["model", "isolated", "schema"].iter().all(|k| props.get(k).is_none()) && t.schema["properties"].get("background").is_none());
        assert!(t.description.contains("`reviewer`/`security` (read-only)") && !t.description.contains("scout") && !t.description.contains("worktree") && !t.description.contains("`model`") && !t.description.contains("returns at once"), "{}", t.description);
        assert!(toml::from_str::<TaskConfig>("max_concurency = 1").is_err());
        for bad in ["max_concurrency = 0", "max_concurrency = 65", "max_recursion_depth = 9", "max_runtime_ms = 86400001", "disabled_agents = ['']", "per_call_model_allow = ['bare']", "[agent_model_overrides]\nx = 'bare'"] {
            assert!(toml::from_str::<TaskConfig>(bad).unwrap().validate().is_err(), "{bad}");
        }
    }
    #[test]
    fn per_call_models_follow_mode_and_allow_list_and_depth_follows_agents() {
        let mut c = TaskConfig::default();
        assert!(c.check_call_model("a/b").is_ok() && c.check_call_model("bare").is_err());
        c.per_call_model_allow = vec!["a/b".into()];
        assert!(c.check_call_model("a/b").is_ok() && c.check_call_model("a/c").is_err());
        for mode in [PerCallModel::Off, PerCallModel::Eval] { c.per_call_model = mode; assert!(c.check_call_model("a/b").is_err()); }
        let defs = bundled(); let mut c = TaskConfig::default();
        assert!(c.can_spawn(1, &defs) && !c.can_spawn(2, &defs) && c.visible(&defs).len() == 4);
        c.disabled_agents = defs.iter().map(|d| d.name.clone()).collect();
        assert!(!c.can_spawn(0, &defs) && c.visible(&defs).is_empty());
        c.disabled_agents.clear(); c.max_recursion_depth = 0;
        assert!(!c.can_spawn(0, &defs) && c.visible(&defs).is_empty());
        let mut d = defs[1].clone(); d.model = Some("x/y".into());
        let mut c = TaskConfig::default(); assert_eq!(c.agent_model(&d).as_deref(), Some("x/y"));
        c.agent_model_overrides.insert("scout".into(), "p/q".into()); assert_eq!(c.agent_model(&d).as_deref(), Some("p/q"));
    }
}
