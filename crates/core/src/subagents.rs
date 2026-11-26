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
    /// `prewalk:` frontmatter: `off`, or a model/role to hand off to at the first edit/write.
    pub prewalk: Option<String>,
}

pub fn bundled() -> Vec<AgentDef> {
    [
        ("task", "General-purpose worker with every tool; the default", None, "Finish only the assigned work. Edit and run what the task needs. Return a concise result; the user cannot see you."),
        ("scout", "Read-only codebase research; returns findings with path:line anchors", Some(vec!["read", "grep", "glob", "web_fetch", "web_search"]), "Research only; do not modify files. Return findings with path:line anchors."),
        ("reviewer", "Read-only code review; may run read-only git commands", Some(vec!["read", "grep", "glob", "bash"]), "Never modify files. Use bash only for read-only commands such as git diff, git log, and git show. Report concrete findings with file:line and severity."),
        ("security", "Read-only security audit; reports exploitable findings with evidence", Some(vec!["read", "grep", "glob", "bash"]), "Never modify files. Use bash only for read-only commands. Look for injection, path traversal, secrets in code, unsafe deserialization, missing authorization and unvalidated input at trust boundaries. Report only findings with a concrete path:line, an exploit sketch and severity; say so when nothing is found."),
    ].into_iter().map(|(name, description, tools, prompt)| AgentDef { name: name.into(), description: description.into(), tools: tools.map(|v| v.into_iter().map(String::from).collect()), model: None, prompt: prompt.into(), bundled: true, prewalk: None }).collect()
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
    let mut prewalk = None;
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
                "prewalk" => prewalk = crate::assist::prewalk_frontmatter(value),
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
    Some(AgentDef { description: description.unwrap_or_else(|| name.clone()), name, tools, model, prompt: body.trim().into(), bundled: false, prewalk })
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

/// How an isolated task's changes reach the parent checkout.
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum IsolationMerge { /// One combined diff applied with `git apply --3way`.
    #[default]
    Patch,
    /// A commit per task, merged with `--no-ff`.
    Branch,
}

/// Commit message style for isolated task branches.
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum IsolationCommits { #[default] Generic, Ai }

/// How hard the system prompt pushes the main agent to delegate.
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Eager { #[default] Default, Preferred, Always }

/// Delegation guidance appended to the main agent's system prompt.
pub fn eager_guidance(eager: Eager) -> Option<&'static str> {
    Some(match eager {
        Eager::Default => return None,
        Eager::Preferred => "Delegation preferred: once the design settles, hand substantial multi-file work, refactors, features, tests and investigations to `task` sub-agents; judge small single-file or interactive work yourself.",
        Eager::Always => "Delegation default: once the design settles, you MUST fan work to `task` sub-agents, except for a single-file edit under about 30 lines, a direct answer without code changes, or a command the user explicitly asked you to run.",
    })
}

/// Appended to the first user message when `task.eager = "always"`.
pub const EAGER_REMINDER: &str = "[delegation] Substantial multi-file work belongs in `task` sub-agents; do the small single-file parts yourself.";

/// User-facing thinking levels, ordered least to most intensive.
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "lowercase")]
pub enum Effort { Minimal, Low, Medium, High, XHigh, #[default] Max }

impl Effort {
    /// `None` for a level this build does not know.
    pub fn parse(raw: &str) -> Option<Effort> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "minimal" => Some(Effort::Minimal), "low" => Some(Effort::Low), "medium" => Some(Effort::Medium),
            "high" => Some(Effort::High), "xhigh" => Some(Effort::XHigh), "max" => Some(Effort::Max), _ => None,
        }
    }
}

/// The highest supported level at or below `hint` (and below `ceiling`); with no hint, none.
/// An explicit hint unsupported below the ceiling falls to the lowest level the model has.
pub fn clamp_effort(levels: &[&str], ceiling: Effort, hint: Option<&str>) -> Option<String> {
    let hint = hint?;
    let target = Effort::parse(hint)?.min(ceiling);
    let mut best = levels.iter().copied().filter(|l| Effort::parse(l).is_some_and(|e| e <= target)).max_by_key(|l| Effort::parse(l).unwrap());
    if best.is_none() { best = levels.iter().copied().find(|l| Effort::parse(l).is_some_and(|e| e <= ceiling)); }
    best.map(String::from)
}

/// Bundled agents whose budget is capped regardless of a higher configured one.
const BUNDLED_SOFT_BUDGET: &[(&str, u64)] = &[("scout", 100)];

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct TaskIsolation {
    /// Allow `isolated: true` (a git worktree per task).
    pub enabled: bool,
    /// Apply a successful isolated task's changes to the parent checkout. Off by default: native
    /// isolation has always kept changes in the worktree, so turning this on is an explicit choice.
    pub apply: bool,
    pub merge: IsolationMerge,
    pub commits: IsolationCommits,
}
impl Default for TaskIsolation { fn default() -> Self { Self { enabled: true, apply: false, merge: IsolationMerge::default(), commits: IsolationCommits::default() } } }

/// What a subagent's next request should do about its soft request budget.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BudgetStep { Run, Notice, Force, Exceeded }

/// Per-run soft request guard: one wrap-up notice, one forced final request, then stop.
pub struct SoftBudget { budget: u64, notice: bool, noticed: bool, forced: bool }

impl SoftBudget {
    pub fn new(budget: u64, notice: bool) -> Self { SoftBudget { budget, notice, noticed: false, forced: false } }

    /// Classifies the next request given how many assistant requests this run already made.
    pub fn step(&mut self, requests: u64) -> BudgetStep {
        if self.budget == 0 { return BudgetStep::Run; }
        let stop = (self.budget as f64 * 1.5).ceil() as u64;
        if requests >= stop { return if self.forced { BudgetStep::Exceeded } else { self.forced = true; BudgetStep::Force }; }
        if self.notice && !self.noticed && requests >= self.budget { self.noticed = true; return BudgetStep::Notice; }
        BudgetStep::Run
    }
}

/// The steering notice injected when a subagent crosses its budget.
pub fn budget_notice(requests: u64, budget: u64) -> String {
    format!("[budget notice] You have used {requests} requests in this run (soft budget: {budget}). Wrap up now: finish the current step and deliver your final report. At {} requests the run is force-stopped and you will be asked to report whatever you have.", (budget as f64 * 1.5).ceil() as u64)
}

/// The instruction sent when the budget is exhausted; the next reply must be the report.
pub fn budget_force(requests: u64, budget: u64) -> String {
    format!("[budget notice] The soft request budget is exhausted ({requests} requests; budget {budget}). Stop calling tools and report your partial findings, and what is left, in your final message.")
}

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
    /// Delegation pressure in the system prompt (and a first-turn reminder for `always`).
    pub eager: Eager,
    /// Batch `{ context, tasks[] }` calls; off accepts one flat `prompt` per call.
    pub batch: bool,
    /// Expose and accept a per-spawn `effort` hint.
    pub enable_effort: bool,
    /// Run `lsp.*` formatting/diagnostics after subagent edits; native default stays on so existing agents keep them.
    pub enable_lsp: bool,
    /// Assistant requests per subagent run before wrap-up steering; 0 disables the guard.
    pub soft_request_budget: u64,
    /// Agent name -> request budget; replaces `soft_request_budget` for that agent.
    pub agent_soft_request_budget: BTreeMap<String, u64>,
    pub soft_request_budget_notice: bool,
    /// Ceiling for a per-spawn effort hint or an inherited thinking level.
    pub max_effort: Effort,
    /// Agent name -> compaction token threshold; non-zero wins over the model/global threshold.
    pub agent_compaction_threshold_overrides: BTreeMap<String, u64>,
    /// Agent name -> `on`, `off` or an advisor `provider/model` for that agent.
    pub agent_advisor: BTreeMap<String, String>,
    /// Show each sub-agent's resolved model id in the task widget status line.
    pub show_resolved_model_badge: bool,
    /// Ask the `judge` role which parts of a new top-level prompt can run concurrently.
    pub auto_parallel: crate::config::advisor::Auto,
    /// Arm prewalk for the bundled generic `task` subagent.
    pub prewalk: bool,
    /// `steps` arrays on spawns instead of one `prompt`.
    pub steps: bool,
    /// Start each batch item as soon as its object finishes streaming.
    pub speculative_launch: bool,
    /// Maximum worker threads subagents run on; 0 keeps them all on the caller's thread.
    pub worker_threads: u64,
    /// Let other GreenCode instances in this folder read agent outputs and message agents.
    pub cross_instance: bool,
    /// Ask a working subagent for a completion estimate at 2, 5, 10 and 30 minutes, then hourly.
    pub completion_probe: bool,
    /// Park an idle subagent to disk after this long; 0 keeps it live until exit.
    pub agent_idle_ttl_ms: u64,
}

impl Default for TaskConfig {
    fn default() -> Self {
        Self { max_concurrency: 4, max_recursion_depth: 2, max_runtime_ms: 0, disabled_agents: vec![], agent_model_overrides: BTreeMap::new(), per_call_model: PerCallModel::On, per_call_model_allow: vec![], structured: true, isolation: TaskIsolation::default(), background: true, eager: Eager::default(), batch: true, enable_effort: false, enable_lsp: true, soft_request_budget: 200, agent_soft_request_budget: BTreeMap::new(), soft_request_budget_notice: true, max_effort: Effort::Max, agent_compaction_threshold_overrides: BTreeMap::new(), agent_advisor: BTreeMap::new(), show_resolved_model_badge: false, auto_parallel: crate::config::advisor::Auto::Off, prewalk: false,
            steps: false, speculative_launch: true, worker_threads: 4, cross_instance: false, completion_probe: true, agent_idle_ttl_ms: 420_000 }
    }
}

pub const MAX_CONCURRENCY: usize = 64;
pub const MAX_RECURSION_DEPTH: u8 = 8;
pub const MAX_RUNTIME_MS: u64 = 86_400_000;

/// How `task.steps` items are grouped into messages: the first step alone, then the middle steps two at a
/// time, and the last step alone (its result is the task's result).
pub fn step_batches(steps: &[String]) -> Vec<String> {
    match steps.len() {
        0 | 1 => return steps.to_vec(),
        2..=5 => return steps.iter().map(|s| s.trim().to_string()).collect(),
        _ => {}
    }
    let mut out = vec![steps[0].trim().to_string()];
    let middle = &steps[1..steps.len() - 1];
    for pair in middle.chunks(2) {
        out.push(pair.iter().map(|s| s.trim()).collect::<Vec<_>>().join("\n\n"));
    }
    out.push(steps[steps.len() - 1].trim().to_string());
    out
}

/// `task.completionProbe`: when a subagent should be asked for a completion estimate, measured from
/// its start: 2, 5, 10 and 30 minutes, then hourly.
pub fn probe_due(elapsed: std::time::Duration, asked: usize) -> bool {
    const FIRST: [u64; 4] = [120, 300, 600, 1800];
    let secs = elapsed.as_secs();
    match FIRST.get(asked) {
        Some(&at) => secs >= at,
        None => secs >= 1800 + 3600 * (asked - FIRST.len() + 1) as u64,
    }
}

/// The probe message a subagent receives; it answers with an estimate, not a result.
pub const PROBE: &str = "Progress check from the parent: estimate how much of this task is left (a rough percentage or a few words), without stopping your work.";

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
        for agent in self.agent_soft_request_budget.keys() {
            if agent.trim().is_empty() { return Err("task.agent_soft_request_budget must not hold empty agent names".into()); }
        }
        for agent in self.agent_compaction_threshold_overrides.keys() {
            if agent.trim().is_empty() { return Err("task.agent_compaction_threshold_overrides must not hold empty agent names".into()); }
        }
        for (agent, choice) in &self.agent_advisor {
            if agent.trim().is_empty() { return Err("task.agent_advisor must not hold empty agent names".into()); }
            if !matches!(choice.as_str(), "on" | "off") { model_spec(choice).map_err(|e| format!("task.agent_advisor.{agent}: {e}"))?; }
        }
        Ok(())
    }

    /// Requests allowed for `agent`: its entry, else the global budget capped by a bundled agent's.
    pub fn soft_budget(&self, agent: &str) -> u64 {
        if let Some(&budget) = self.agent_soft_request_budget.get(agent) { return budget; }
        let bundled = BUNDLED_SOFT_BUDGET.iter().find(|(name, _)| *name == agent).map(|(_, n)| *n);
        bundled.map_or(self.soft_request_budget, |cap| if self.soft_request_budget == 0 { 0 } else { self.soft_request_budget.min(cap) })
    }

    /// `agent`'s compaction trigger, when an override is configured.
    pub fn compaction_threshold(&self, agent: &str) -> Option<u64> {
        self.agent_compaction_threshold_overrides.get(agent).copied().filter(|&n| n > 0)
    }

    /// The effort a spawn runs at: an explicit hint, or the inherited level when a ceiling below `max` applies.
    /// `levels` are the target model's supported levels.
    pub fn effective_effort(&self, levels: &[&str], hint: Option<String>, inherited: Option<&str>) -> Option<String> {
        let requested = hint.or_else(|| if self.max_effort == Effort::Max { None } else { inherited.map(String::from) });
        clamp_effort(levels, self.max_effort, requested.as_deref())
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
        const OPTIONS: &str = "`context` is shared; `model` overrides the default; `backend` runs it on another harness (`codex`, `claude`, `pi`, ...) with that harness's tools; `isolated` uses a worktree; `background` returns at once.";
        const BATCH: &str = "Run sub-agents in parallel; each returns its final answer.";
        // With batching off the call carries one spawn's fields at the top level.
        if !self.batch {
            let flat = tool.schema["properties"]["tasks"]["items"]["properties"].as_object().cloned().unwrap_or_default();
            let props = tool.schema["properties"].as_object_mut().expect("task schema properties");
            props.remove("tasks");
            props.extend(flat);
            tool.schema["required"] = serde_json::json!(["prompt"]);
            tool.description = tool.description.replace(BATCH, "Run one sub-agent; it returns its final answer.");
        }
        let item = if self.batch {
            &mut tool.schema["properties"]["tasks"]["items"]["properties"]
        } else {
            &mut tool.schema["properties"]
        };
        let mut options = vec!["`context` is shared"];
        if self.per_call_model == PerCallModel::On {
            options.extend(["`model` overrides the default", "`backend` runs it on another harness (`codex`, `claude`, `pi`, ...) with that harness's tools"]);
        } else {
            item.as_object_mut().map(|o| { o.remove("model"); o.remove("backend") });
        }
        if self.isolation.enabled { options.push("`isolated` uses a worktree") } else { item.as_object_mut().map(|o| o.remove("isolated")); }
        if self.enable_effort {
            item.as_object_mut().map(|o| { o.insert("effort".into(), serde_json::json!({"type": "string", "enum": ["minimal", "low", "medium", "high", "xhigh", "max"], "description": "Reasoning effort for this sub-agent, capped by task.max_effort"})); });
            options.push("`effort` overrides its reasoning effort");
        }
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

/// OMP key → native dotted key; `config import-omp` copies the value verbatim unless `convert` handles it.
pub(crate) const OMP: &[(&str, &str)] = &[
    ("task.isolation.apply", "task.isolation.apply"),
    ("task.isolation.merge", "task.isolation.merge"),
    ("task.isolation.commits", "task.isolation.commits"),
    ("task.eager", "task.eager"),
    ("task.showResolvedModelBadge", "task.show_resolved_model_badge"),
    ("task.batch", "task.batch"),
    ("task.enableEffort", "task.enable_effort"),
    ("task.enableLsp", "task.enable_lsp"),
    ("task.softRequestBudget", "task.soft_request_budget"),
    ("task.agentSoftRequestBudget", "task.agent_soft_request_budget"),
    ("task.softRequestBudgetNotice", "task.soft_request_budget_notice"),
    ("task.maxEffort", "task.max_effort"),
    ("task.agentCompactionThresholdOverrides", "task.agent_compaction_threshold_overrides"),
    ("task.agentAdvisor", "task.agent_advisor"),
    ("task.autoParallel", "task.auto_parallel"),
    ("task.prewalk", "task.prewalk"),
];

/// `null` entries clear an inherited value and are dropped; every remaining entry must be a non-negative integer.
pub(crate) fn convert(from: &str, value: &serde_json::Value) -> Option<anyhow::Result<toml::Value>> {
    let record = matches!(from, "task.agentSoftRequestBudget" | "task.agentCompactionThresholdOverrides" | "task.agentAdvisor");
    if !record { return None; }
    Some((|| {
        let entries = value.as_object().ok_or_else(|| anyhow::anyhow!("{from} must be a mapping of agent name"))?;
        let mut table = toml::Table::new();
        for (agent, entry) in entries {
            if entry.is_null() { continue; }
            let converted = if from == "task.agentAdvisor" {
                let choice = entry.as_str().ok_or_else(|| anyhow::anyhow!("{from}.{agent} must be `on`, `off` or `provider/model`"))?;
                if !matches!(choice, "on" | "off") { model_spec(choice).map_err(|e| anyhow::anyhow!("{from}.{agent}: {e}"))?; }
                toml::Value::String(choice.into())
            } else {
                let n = entry.as_i64().filter(|&n| n >= 0).ok_or_else(|| anyhow::anyhow!("{from}.{agent} must be a non-negative integer"))?;
                // Compaction 0 means "clear the entry" in OMP.
                if n == 0 && from == "task.agentCompactionThresholdOverrides" { continue; }
                toml::Value::Integer(n)
            };
            table.insert(agent.clone(), converted);
        }
        Ok(toml::Value::Table(table))
    })())
}

#[cfg(test)]
mod tests {
    #[test]
    fn steps_batch_later_messages_and_keep_the_last_alone() {
        let step = |n: usize| format!("step {n}");
        assert_eq!(super::step_batches(&[]), Vec::<String>::new());
        assert_eq!(super::step_batches(&[step(1)]), [step(1)]);
        // Up to four steps stay one message each.
        let four: Vec<_> = (1..=4).map(step).collect();
        assert_eq!(super::step_batches(&four), four);
        // Past four, middle messages carry two or more and the last step is its own message.
        let ten: Vec<_> = (1..=10).map(step).collect();
        let batches = super::step_batches(&ten);
        assert_eq!(batches[0], step(1));
        assert_eq!(batches.last().unwrap(), &step(10));
        // Earlier messages carry two or more: 1 | 2 3 | 4 5 | 6 7 | 8 9 | 10.
        assert_eq!(batches.len(), 6);
        assert!(batches[1].contains(&step(2)) && batches[1].contains(&step(3)));
    }

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

    #[test]
    fn soft_budget_notifies_once_then_forces_one_final_request() {
        let mut b = SoftBudget::new(10, true);
        for n in 0..10 {
            assert_eq!(b.step(n), BudgetStep::Run);
        }
        assert_eq!(b.step(10), BudgetStep::Notice);
        assert_eq!(b.step(11), BudgetStep::Run);
        assert_eq!(b.step(14), BudgetStep::Run);
        assert_eq!(b.step(15), BudgetStep::Force);
        assert_eq!(b.step(16), BudgetStep::Exceeded);
        let mut quiet = SoftBudget::new(2, false);
        assert_eq!(quiet.step(2), BudgetStep::Run);
        assert_eq!(quiet.step(3), BudgetStep::Force);
        assert_eq!(SoftBudget::new(0, true).step(100), BudgetStep::Run);
        let mut c = TaskConfig::default();
        assert_eq!(c.soft_budget("scout"), 100, "bundled scout caps the global budget");
        c.soft_request_budget = 50;
        assert_eq!(c.soft_budget("scout"), 50);
        c.agent_soft_request_budget.insert("scout".into(), 7);
        assert_eq!(c.soft_budget("scout"), 7);
        c.agent_soft_request_budget.insert("task".into(), 0);
        assert_eq!(c.soft_budget("task"), 0, "a per-agent 0 disables the guard");
    }

    #[test]
    fn effort_hints_clamp_to_the_ceiling_and_inherit_only_under_one() {
        let c = TaskConfig::default();
        assert_eq!(c.effective_effort(&["low", "high"], Some("xhigh".into()), Some("high")).as_deref(), Some("high"));
        assert_eq!(c.effective_effort(&["low", "high"], Some("minimal".into()), None).as_deref(), Some("low"));
        let mut c = TaskConfig::default();
        c.max_effort = Effort::Medium;
        assert_eq!(c.effective_effort(&["low", "medium", "high"], None, Some("high")).as_deref(), Some("medium"));
        assert_eq!(c.effective_effort(&["low", "medium", "high"], None, None), None);
    }

    #[test]
    fn omp_task_rows_import() {
        let table = crate::config::settings::translate(
            "task:\n  isolation:\n    apply: false\n    merge: branch\n    commits: ai\n  eager: always\n  batch: false\n  enableEffort: true\n  enableLsp: false\n  softRequestBudget: 30\n  softRequestBudgetNotice: false\n  maxEffort: medium\n  agentSoftRequestBudget:\n    scout: 9\n  agentCompactionThresholdOverrides:\n    reviewer: 64000\n  agentAdvisor:\n    scout: off\n    reviewer: anthropic/claude-x\n",
        )
        .unwrap();
        let cfg = crate::config::settings::decode(toml::Value::Table(table)).unwrap();
        assert!(!cfg.task.isolation.apply);
        assert_eq!(cfg.task.isolation.merge, IsolationMerge::Branch);
        assert_eq!(cfg.task.isolation.commits, IsolationCommits::Ai);
        assert_eq!(cfg.task.eager, Eager::Always);
        assert!(!cfg.task.batch);
        assert!(cfg.task.enable_effort);
        assert!(!cfg.task.enable_lsp);
        assert_eq!(cfg.task.soft_request_budget, 30);
        assert!(!cfg.task.soft_request_budget_notice);
        assert_eq!(cfg.task.max_effort, Effort::Medium);
        assert_eq!(cfg.task.agent_soft_request_budget["scout"], 9);
        assert_eq!(cfg.task.agent_compaction_threshold_overrides["reviewer"], 64_000);
        assert_eq!(cfg.task.agent_advisor["scout"], "off");
        assert_eq!(cfg.task.agent_advisor["reviewer"], "anthropic/claude-x");
        // A compaction override of 0 clears the entry instead of disabling compaction.
        let table = crate::config::settings::translate("task:\n  agentCompactionThresholdOverrides:\n    scout: 0\n").unwrap();
        assert_eq!(crate::config::settings::decode(toml::Value::Table(table)).unwrap().task.agent_compaction_threshold_overrides.get("scout"), None);
    }
}
