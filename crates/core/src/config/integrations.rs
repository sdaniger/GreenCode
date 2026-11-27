//! Settings for LSP, MCP, eval, skills, commands, extensions, autolearn, commit and computer; flattened into `Config`.
use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Deserialize, Serialize, Clone, Debug, Default, PartialEq)]
#[serde(default)]
pub struct IntegrationOpts {
    pub autolearn: AutoLearnCfg,
    pub commit: CommitCfg,
    pub computer: ComputerCfg,
    pub github: GithubCfg,
    pub lsp: LspCfg,
    pub eval: EvalCfg,
    pub python: PythonCfg,
    pub skills: SkillsCfg,
    pub commands: CommandsCfg,
    pub mcp: McpCfg,
    /// The native `browser` tool's backends.
    pub browser: super::browser::BrowserCfg,
    /// Extra Lua extension files or directories next to the discovered ones.
    pub extensions: Vec<String>,
    /// `extension-module:<name>` and `skill:<pattern>` entries that stay unloaded.
    pub disabled_extensions: Vec<String>,
}

#[derive(Deserialize, Serialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct AutoLearnCfg {
    /// Run one private capture request at stop when no learning model is configured.
    pub auto_continue: bool,
    /// Completed tool calls a turn needs before automatic capture runs; 0 captures every turn.
    pub min_tool_calls: u32,
}

impl Default for AutoLearnCfg {
    fn default() -> Self {
        Self {
            auto_continue: false,
            // Native auto-learn has always considered every turn; OMP's 5 imports when set.
            min_tool_calls: 0,
        }
    }
}

#[derive(Deserialize, Serialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct CommitCfg {
    /// Enables the commit message action and `/commit`.
    pub enabled: bool,
}

impl Default for CommitCfg {
    fn default() -> Self {
        Self { enabled: true }
    }
}

#[derive(Deserialize, Serialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct ComputerCfg {
    /// Maximum screenshot width in pixels; larger captures are scaled down.
    pub max_width: u32,
    /// Maximum screenshot height in pixels.
    pub max_height: u32,
    /// `active` captures the active window's display, `all` every display, else a native display id.
    pub display: String,
    /// When window input takes over the real pointer and keyboard instead of running in the background.
    pub takeover: Takeover,
}

/// How window input may take over the real pointer and keyboard; `fallback` only when background input is unavailable.
#[derive(Deserialize, Serialize, Clone, Copy, Debug, Default, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum Takeover {
    #[default]
    Fallback,
    Foreground,
    Always,
    Refuse,
}

impl Default for ComputerCfg {
    fn default() -> Self {
        Self {
            max_width: 3840,
            max_height: 2400,
            display: "active".into(),
            takeover: Takeover::Fallback,
        }
    }
}

#[derive(Deserialize, Serialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct GithubCfg {
    pub cache: GithubCacheCfg,
}

impl Default for GithubCfg {
    fn default() -> Self {
        Self { cache: GithubCacheCfg::default() }
    }
}

#[derive(Deserialize, Serialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct GithubCacheCfg {
    /// Cache rendered issue/PR views in the native home's `cache/github-cache.json`.
    pub enabled: bool,
    /// Seconds a cached view is returned without refreshing.
    pub soft_ttl_sec: u64,
    /// Seconds a cached view may be returned stale while it refreshes; past it the row is dropped.
    pub hard_ttl_sec: u64,
}

impl Default for GithubCacheCfg {
    fn default() -> Self {
        Self { enabled: true, soft_ttl_sec: 300, hard_ttl_sec: 7 * 24 * 3600 }
    }
}

#[derive(Deserialize, Serialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct LspCfg {
    /// Start language servers on first use instead of at session startup.
    pub lazy: bool,
    /// Format code files through the language server after `write`.
    pub format_on_write: bool,
    /// Append LSP diagnostics after `write`.
    pub diagnostics_on_write: bool,
    /// Append LSP diagnostics after `edit`.
    pub diagnostics_on_edit: bool,
    /// Only show diagnostics that differ from the last ones shown for that file.
    pub diagnostics_deduplicate: bool,
    /// Seconds a language server request may take; also the wait for post-write diagnostics.
    pub request_timeout: u64,
    /// Reach language servers through the per-project daemon broker so a released server stays warm for the
    /// next connection; falls back to private servers when the broker is unavailable.
    pub shared: bool,
    /// Extra or replacement language servers, keyed by executable name; an extension listed here wins over
    /// the built-in server for it.
    pub servers: std::collections::BTreeMap<String, LspServerCfg>,
}

#[derive(Deserialize, Serialize, Clone, Debug, Default, PartialEq)]
#[serde(default)]
pub struct LspServerCfg {
    pub args: Vec<String>,
    /// File extensions without the dot, e.g. `["zig", "zon"]`.
    pub extensions: Vec<String>,
}

impl Default for LspCfg {
    fn default() -> Self {
        Self {
            lazy: true,
            format_on_write: false,
            diagnostics_on_write: true,
            diagnostics_on_edit: true,
            diagnostics_deduplicate: true,
            // Native language servers may need this long on a cold crate; OMP ships 10.
            request_timeout: 20,
            shared: true,
            servers: Default::default(),
        }
    }
}

#[derive(Deserialize, Serialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct EvalCfg {
    /// Allow Python cells.
    pub py: bool,
    /// Allow JavaScript cells.
    pub js: bool,
    /// Create the managed JavaScript eval package environment on first use.
    pub auto_provision: bool,
    pub tools: EvalToolsCfg,
    pub workpool: WorkpoolCfg,
    pub auto_background: EvalBackgroundCfg,
}

#[derive(Deserialize, Serialize, Clone, Copy, Debug, PartialEq)]
#[serde(default)]
pub struct EvalToolsCfg {
    /// Let cells define tools that the agent and its subagents can call.
    pub enabled: bool,
}

impl Default for EvalToolsCfg {
    fn default() -> Self {
        Self { enabled: true }
    }
}

#[derive(Deserialize, Serialize, Clone, Copy, Debug, PartialEq)]
#[serde(default)]
pub struct WorkpoolCfg {
    /// One subagent per workpool item instead of reusing workers over batches.
    pub fresh_agents: bool,
}

impl Default for WorkpoolCfg {
    fn default() -> Self {
        Self { fresh_agents: true }
    }
}

#[derive(Deserialize, Serialize, Clone, Copy, Debug, PartialEq)]
#[serde(default)]
pub struct EvalBackgroundCfg {
    pub enabled: bool,
    /// A cell still running after this long moves to a background job.
    pub threshold_ms: u64,
}

impl Default for EvalBackgroundCfg {
    fn default() -> Self {
        Self { enabled: false, threshold_ms: 60_000 }
    }
}

impl Default for EvalCfg {
    fn default() -> Self {
        Self {
            py: true,
            js: true,
            auto_provision: true,
            tools: EvalToolsCfg::default(),
            workpool: WorkpoolCfg::default(),
            auto_background: EvalBackgroundCfg::default(),
        }
    }
}

#[derive(Deserialize, Serialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct PythonCfg {
    /// `session` keeps the interpreter alive between calls; `per-call` starts a fresh one each time.
    pub kernel_mode: String,
    /// Exact Python executable; empty uses automatic discovery.
    pub interpreter: String,
}

impl Default for PythonCfg {
    fn default() -> Self {
        Self {
            kernel_mode: "session".into(),
            interpreter: String::new(),
        }
    }
}

#[derive(Deserialize, Serialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct SkillsCfg {
    /// Load `~/.codex/skills` and `.codex/skills`.
    pub enable_codex_user: bool,
    /// Load `~/.claude/skills`; on by default, unlike OMP, so existing skills keep working.
    pub enable_claude_user: bool,
    /// Load `.claude/skills`.
    pub enable_claude_project: bool,
    /// Load the native `~/.greencode/skills`; OMP calls this source `pi`.
    pub enable_pi_user: bool,
    /// Load the native `.greencode/skills`.
    pub enable_pi_project: bool,
    /// Load `~/.agents/skills`.
    pub enable_agents_user: bool,
    /// Load `.agents/skills`.
    pub enable_agents_project: bool,
    /// Extra directories holding `<name>/SKILL.md`, scanned non-recursively; they win over the defaults.
    pub custom_directories: Vec<String>,
    /// Glob patterns removed entirely.
    pub ignored_skills: Vec<String>,
    /// Glob patterns kept out of the model's catalog but still reachable by name and `/skill`.
    pub manual_skills: Vec<String>,
    /// Glob allowlist for the model's catalog; empty lists everything.
    pub include_skills: Vec<String>,
    /// Skillshare registry used by `greencode-cli skill` to install, search and publish skills.
    pub registry_url: Option<String>,
}

impl Default for SkillsCfg {
    fn default() -> Self {
        Self {
            enable_codex_user: false,
            // Native always read these roots; OMP ships false for both.
            enable_claude_user: true,
            enable_claude_project: true,
            enable_pi_user: true,
            enable_pi_project: true,
            enable_agents_user: true,
            enable_agents_project: true,
            custom_directories: Vec::new(),
            ignored_skills: Vec::new(),
            manual_skills: Vec::new(),
            include_skills: Vec::new(),
            registry_url: None,
        }
    }
}

#[derive(Deserialize, Serialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct CommandsCfg {
    /// Load `~/.claude/commands`; on by default, unlike OMP, because native always read it.
    pub enable_claude_user: bool,
    /// Load `.claude/commands`.
    pub enable_claude_project: bool,
    /// Load `~/.config/opencode/commands`; on by default, unlike OMP, because native always read it.
    pub enable_opencode_user: bool,
    /// Load `.opencode/commands`.
    pub enable_opencode_project: bool,
}

impl Default for CommandsCfg {
    fn default() -> Self {
        Self {
            enable_claude_user: true,
            enable_claude_project: true,
            enable_opencode_user: true,
            enable_opencode_project: true,
        }
    }
}

#[derive(Deserialize, Serialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct McpCfg {
    /// Milliseconds the first turn waits for MCP tool discovery; 0 waits until every connection settles.
    pub startup_timeout_ms: u64,
    /// Stop a server idle this many minutes; 0 keeps it for the session.
    pub idle_stop_minutes: u64,
    /// Run servers through the shared daemon so every instance uses one process per server.
    pub shared: bool,
    /// Expose each server as an eval global instead of a tool.
    pub eval_namespaces: bool,
    /// Skip servers whose tools a native tool replaces (`node_repl` when eval runs JavaScript).
    pub builtin_transforms: bool,
    /// Inject MCP resource updates into the conversation.
    pub notifications: bool,
    /// Debounce window in milliseconds for those updates.
    pub notification_debounce_ms: u64,
}

impl Default for McpCfg {
    fn default() -> Self {
        Self {
            startup_timeout_ms: 250,
            // Native kept MCP servers (and their state) for the session; OMP's 5 imports when set.
            idle_stop_minutes: 0,
            shared: true,
            eval_namespaces: false,
            builtin_transforms: true,
            notifications: true,
            notification_debounce_ms: 250,
        }
    }
}

/// OMP key → native dotted key; `config import-omp` copies the value verbatim unless `convert` handles it.
pub(super) const OMP: &[(&str, &str)] = &[
    // `learning.enabled` also keeps the `learn` tool; native default stays true, OMP ships false.
    ("autolearn.enabled", "learning.enabled"),
    ("autolearn.autoContinue", "autolearn.auto_continue"),
    ("autolearn.minToolCalls", "autolearn.min_tool_calls"),
    ("commit.enabled", "commit.enabled"),
    ("computer.maxWidth", "computer.max_width"),
    ("computer.maxHeight", "computer.max_height"),
    ("computer.display", "computer.display"),
    ("computer.takeover", "computer.takeover"),
    ("github.cache.enabled", "github.cache.enabled"),
    ("github.cache.softTtlSec", "github.cache.soft_ttl_sec"),
    ("github.cache.hardTtlSec", "github.cache.hard_ttl_sec"),
    ("lsp.lazy", "lsp.lazy"),
    ("lsp.formatOnWrite", "lsp.format_on_write"),
    ("lsp.diagnosticsOnWrite", "lsp.diagnostics_on_write"),
    ("lsp.diagnosticsOnEdit", "lsp.diagnostics_on_edit"),
    ("lsp.diagnosticsDeduplicate", "lsp.diagnostics_deduplicate"),
    ("lsp.requestTimeout", "lsp.request_timeout"),
    ("lsp.shared", "lsp.shared"),
    ("eval.py", "eval.py"),
    ("eval.js", "eval.js"),
    ("eval.autoProvision", "eval.auto_provision"),
    ("eval.tools.enabled", "eval.tools.enabled"),
    ("eval.workpool.freshAgents", "eval.workpool.fresh_agents"),
    ("eval.autoBackground.enabled", "eval.auto_background.enabled"),
    ("python.kernelMode", "python.kernel_mode"),
    ("python.interpreter", "python.interpreter"),
    ("skills.enableCodexUser", "skills.enable_codex_user"),
    ("skills.enableClaudeUser", "skills.enable_claude_user"),
    ("skills.enableClaudeProject", "skills.enable_claude_project"),
    ("skills.enablePiUser", "skills.enable_pi_user"),
    ("skills.enablePiProject", "skills.enable_pi_project"),
    ("skills.enableAgentsUser", "skills.enable_agents_user"),
    ("skills.enableAgentsProject", "skills.enable_agents_project"),
    ("skills.customDirectories", "skills.custom_directories"),
    ("skills.ignoredSkills", "skills.ignored_skills"),
    ("skills.manualSkills", "skills.manual_skills"),
    ("skills.includeSkills", "skills.include_skills"),
    ("skills.registryUrl", "skills.registry_url"),
    ("commands.enableClaudeUser", "commands.enable_claude_user"),
    ("commands.enableClaudeProject", "commands.enable_claude_project"),
    ("commands.enableOpencodeUser", "commands.enable_opencode_user"),
    ("commands.enableOpencodeProject", "commands.enable_opencode_project"),
    ("mcp.startupTimeoutMs", "mcp.startup_timeout_ms"),
    ("mcp.idleStopMinutes", "mcp.idle_stop_minutes"),
    ("mcp.shared", "mcp.shared"),
    ("mcp.evalNamespaces", "mcp.eval_namespaces"),
    ("mcp.builtinTransforms", "mcp.builtin_transforms"),
    ("mcp.notifications", "mcp.notifications"),
    ("mcp.notificationDebounceMs", "mcp.notification_debounce_ms"),
    ("extensions", "extensions"),
    ("disabledExtensions", "disabled_extensions"),
];

/// Translates an OMP value whose shape or units differ from the native field; `None` copies it verbatim.
pub(super) fn convert(_from: &str, _value: &Value) -> Option<Result<toml::Value>> {
    None
}

#[cfg(test)]
mod tests {
    use crate::config::Config;

    /// Every row above survives YAML → TOML → `Config` under its native key.
    #[test]
    fn omp_rows_import() {
        let yaml = "\
autolearn:
  autoContinue: true
  minToolCalls: 2
commit:
  enabled: false
computer:
  maxWidth: 1920
  maxHeight: 1080
lsp:
  lazy: false
  formatOnWrite: true
  diagnosticsOnWrite: false
  diagnosticsOnEdit: true
  diagnosticsDeduplicate: false
  requestTimeout: 30
eval:
  py: false
  js: false
python:
  kernelMode: per-call
  interpreter: /opt/python3
skills:
  enableCodexUser: true
  enableClaudeUser: false
  enableClaudeProject: false
  enablePiUser: false
  enablePiProject: false
  enableAgentsUser: false
  enableAgentsProject: false
  customDirectories: [/skills]
  ignoredSkills: [old-*]
  manualSkills: [pdf]
  includeSkills: [team/**]
commands:
  enableClaudeUser: false
  enableClaudeProject: false
  enableOpencodeUser: false
  enableOpencodeProject: false
mcp:
  startupTimeoutMs: 10
  idleStopMinutes: 2
extensions: [/ext/a.lua]
disabledExtensions: ['extension-module:old', 'skill:pdf']
";
        let table = crate::config::settings::omp::translate(yaml).unwrap();
        let cfg = crate::config::settings::decode(toml::Value::Table(table)).unwrap();
        let o = &cfg.integration_opts;
        assert!(o.autolearn.auto_continue);
        assert_eq!(o.autolearn.min_tool_calls, 2);
        assert!(!o.commit.enabled);
        assert_eq!((o.computer.max_width, o.computer.max_height), (1920, 1080));
        assert_eq!(
            (o.lsp.lazy, o.lsp.format_on_write, o.lsp.diagnostics_on_write, o.lsp.diagnostics_on_edit, o.lsp.diagnostics_deduplicate, o.lsp.request_timeout),
            (false, true, false, true, false, 30)
        );
        assert_eq!((o.eval.py, o.eval.js), (false, false));
        assert_eq!((o.python.kernel_mode.as_str(), o.python.interpreter.as_str()), ("per-call", "/opt/python3"));
        assert!(o.skills.enable_codex_user && !o.skills.enable_claude_user && !o.skills.enable_pi_user);
        assert_eq!(o.skills.custom_directories, ["/skills"]);
        assert_eq!(o.skills.ignored_skills, ["old-*"]);
        assert_eq!(o.skills.manual_skills, ["pdf"]);
        assert_eq!(o.skills.include_skills, ["team/**"]);
        assert!(!o.commands.enable_claude_user && !o.commands.enable_opencode_project);
        assert_eq!((o.mcp.startup_timeout_ms, o.mcp.idle_stop_minutes), (10, 2));
        assert_eq!(o.extensions, ["/ext/a.lua"]);
        assert_eq!(o.disabled_extensions, ["extension-module:old", "skill:pdf"]);
        // Untouched keys keep their defaults.
        let default = Config::default().integration_opts;
        assert_eq!(default.lsp.request_timeout, 20);
        assert_eq!((default.autolearn.min_tool_calls, default.mcp.idle_stop_minutes), (0, 0));
        assert!(default.commit.enabled);
    }
}
