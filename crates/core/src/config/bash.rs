//! Settings for bash, the shell, tool output spill, todos and async jobs; flattened into `Config`.
use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Deserialize, Serialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct BashOpts {
    pub artifact: ArtifactCfg,
    pub todo: TodoCfg,
    /// `[async]`; the field keeps the keyword raw because TOML has no reserved words.
    pub r#async: AsyncCfg,
    pub bash: BashCfg,
    pub shell_minimizer: ShellMinimizerCfg,
    pub bash_interceptor: InterceptorCfg,
    /// Shell binary for the `bash` tool; unset uses `sh`.
    pub shell_path: Option<String>,
    /// Per-line byte cap for `bash` and `read` output; 0 disables.
    pub output_max_columns: usize,
    /// Ask the model to describe each tool call's intent.
    pub intent_tracing: bool,
    /// Maximum `bash` timeout in seconds; 0 removes the native 600 s cap.
    pub max_timeout: u64,
    /// Per-tool `allow` | `prompt` | `deny`; invalid values are ignored.
    pub approval: BTreeMap<String, String>,
    /// Folders whose contents always need explicit approval.
    pub restricted_folders: Vec<String>,
    /// Mid-task replies written to `out://` by the `write` tool.
    pub out: OutCfg,
}

#[derive(Deserialize, Serialize, Clone, Copy, Debug, PartialEq)]
#[serde(default)]
pub struct OutCfg {
    pub enabled: bool,
}

impl Default for OutCfg {
    fn default() -> Self {
        Self { enabled: true }
    }
}

impl Default for BashOpts {
    fn default() -> Self {
        Self {
            artifact: ArtifactCfg::default(),
            todo: TodoCfg::default(),
            r#async: AsyncCfg::default(),
            bash: BashCfg::default(),
            shell_minimizer: ShellMinimizerCfg::default(),
            bash_interceptor: InterceptorCfg::default(),
            shell_path: None,
            // Native output never cut long lines, and its schemas stay lean without `intent`;
            // OMP's 768 / true import when set.
            output_max_columns: 0,
            intent_tracing: false,
            // Native `bash` capped timeouts at 600 s before this setting existed.
            max_timeout: 600,
            approval: BTreeMap::new(),
            restricted_folders: Vec::new(),
            out: OutCfg::default(),
        }
    }
}

/// OMP's `tools.artifact*` budgets, in its published units (KiB and MiB).
#[derive(Deserialize, Serialize, Clone, Copy, Debug, PartialEq)]
#[serde(default)]
pub struct ArtifactCfg {
    pub spill_threshold_kb: u64,
    pub head_kb: u64,
    pub tail_kb: u64,
    pub tail_lines: usize,
    pub max_bytes_mb: u64,
}

impl Default for ArtifactCfg {
    // Native's 8 KB preview (4 KB head + 4 KB tail) keeps prompts lean; OMP's 50/20/20 imports when set.
    fn default() -> Self {
        Self { spill_threshold_kb: 8, head_kb: 4, tail_kb: 4, tail_lines: 500, max_bytes_mb: 16 }
    }
}

#[derive(Deserialize, Serialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct TodoCfg {
    pub reminders: bool,
    pub reminders_max: u64,
    pub mid_run_nudges: MidRunNudges,
    pub eager: Eager,
    /// Extension that receives the native todo list after each `todo` call (`gc.on("todo", ...)`); unset keeps it session-only.
    pub backend: Option<String>,
    /// Seconds a completed or abandoned item stays in the todo widget; -1 keeps it for the session.
    pub clear_delay: i64,
}

impl Default for TodoCfg {
    fn default() -> Self {
        Self {
            reminders: true,
            reminders_max: 3,
            mid_run_nudges: MidRunNudges::Capped,
            eager: Eager::Default,
            backend: None,
            // Native never drew completed items; OMP's 300 imports when set.
            clear_delay: 0,
        }
    }
}

#[derive(Deserialize, Serialize, Clone, Copy, Debug, Default, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum MidRunNudges {
    Off,
    #[default]
    Capped,
    Backoff,
}

#[derive(Deserialize, Serialize, Clone, Copy, Debug, Default, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum Eager {
    #[default]
    Default,
    Preferred,
    Always,
}

#[derive(Deserialize, Serialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct AsyncCfg {
    pub max_jobs: usize,
    pub print_wake: bool,
    /// Draw a live widget, and a one-line model caption, for each running background job.
    pub widgets: bool,
}

impl Default for AsyncCfg {
    fn default() -> Self {
        Self { max_jobs: 100, print_wake: false, widgets: false }
    }
}

#[derive(Deserialize, Serialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct BashCfg {
    pub allow_compound_commands: bool,
    pub git_guard: bool,
    pub allowed_env_vars: Vec<String>,
    pub patterns: Vec<BashRule>,
    pub auto_background: AutoBackground,
    pub direnv: Direnv,
    pub direnv_load_timeout_ms: u64,
}

impl Default for BashCfg {
    fn default() -> Self {
        Self {
            allow_compound_commands: false,
            git_guard: false,
            allowed_env_vars: Vec::new(),
            patterns: Vec::new(),
            auto_background: AutoBackground::default(),
            direnv: Direnv::default(),
            direnv_load_timeout_ms: 30_000,
        }
    }
}

/// Compress verbose `git`, `npm` and `cargo` output before the model sees it, and set how much a
/// source-file outline folds away.
#[derive(Deserialize, Serialize, Clone, Copy, Debug, PartialEq)]
#[serde(default)]
pub struct ShellMinimizerCfg {
    pub enabled: bool,
    pub source_outline_level: SourceOutlineLevel,
}

impl Default for ShellMinimizerCfg {
    fn default() -> Self {
        Self { enabled: true, source_outline_level: SourceOutlineLevel::default() }
    }
}

#[derive(Deserialize, Serialize, Clone, Copy, Debug, Default, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum SourceOutlineLevel {
    #[default]
    Default,
    Aggressive,
}

#[derive(Deserialize, Serialize, Clone, Copy, Debug, PartialEq)]
#[serde(default)]
pub struct AutoBackground {
    pub enabled: bool,
    pub threshold_ms: u64,
}

impl Default for AutoBackground {
    // Native bash has always waited for the command; OMP's auto-detach imports when set.
    fn default() -> Self {
        Self { enabled: false, threshold_ms: 60_000 }
    }
}

#[derive(Deserialize, Serialize, Clone, Copy, Debug, Default, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum Direnv {
    #[default]
    Auto,
    Off,
}

#[derive(Deserialize, Serialize, Clone, Debug, PartialEq)]
pub struct BashRule {
    #[serde(rename = "match")]
    pub pattern: String,
    pub approval: RuleApproval,
    /// Extra environment names this rule may strip before an `allow` match.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub env: Vec<String>,
}

#[derive(Deserialize, Serialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum RuleApproval {
    Allow,
    Prompt,
    Deny,
}

#[derive(Deserialize, Serialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct InterceptorCfg {
    pub enabled: bool,
    pub patterns: Vec<InterceptorRule>,
}

impl Default for InterceptorCfg {
    fn default() -> Self {
        Self { enabled: false, patterns: default_patterns() }
    }
}

#[derive(Deserialize, Serialize, Clone, Debug, PartialEq)]
pub struct InterceptorRule {
    pub pattern: String,
    pub tool: String,
    pub message: String,
}

/// OMP's shipped interceptor rules. Patterns are JavaScript-flavoured regexes, so matching uses
/// a lookaround-capable engine.
fn default_patterns() -> Vec<InterceptorRule> {
    let rule = |pattern: &str, tool: &str, message: &str| InterceptorRule {
        pattern: pattern.into(),
        tool: tool.into(),
        message: message.into(),
    };
    vec![
        rule(
            r#"^\s*(?:(?:head|tail|less|more)\s+|cat\s+(?!\s|(?:[02-9]>{1,2}\|?\s*[^\s>|]\S*\s+)*(?:<<|1?>(?!&)|&>)))"#,
            "read",
            "Use the `read` tool instead of cat/head/tail. It provides better context and handles binary files.",
        ),
        rule(r#"^\s*(grep|rg|ripgrep|ag|ack)\s+"#, "grep",
            "Use the `grep` tool instead of grep/rg. It respects .gitignore and provides structured output."),
        rule(r#"^\s*(find|fd|locate)\s+.*(-name|-iname|-type|--type|-glob)"#, "glob",
            "Use the `glob` tool instead of find/fd. It respects .gitignore and is faster for glob patterns."),
        rule(r#"^\s*sed\s+(-i|--in-place)"#, "edit",
            "Use the `edit` tool instead of sed -i. It provides diff preview and fuzzy matching."),
        rule(r#"^\s*perl\s+.*-[pn]?i"#, "edit",
            "Use the `edit` tool instead of perl -i. It provides diff preview and fuzzy matching."),
        rule(r#"^\s*awk\s+.*-i\s+inplace"#, "edit",
            "Use the `edit` tool instead of awk -i inplace. It provides diff preview and fuzzy matching."),
        rule(
            r#"^\s*(?:(?:echo|printf)\s+|cat\s*<<|cat(?=\s+(?:\d+|&)?>)\s+)(?:(?:\\[\s\S]|\$\((?:[^()"'`]|"[^"]*"|'[^']*'|`[^`]*`|\([^()]*\))*\)|\$\{[^{}]*\}|`[^`]*`|\$(?!\((?:[^()"'`]|"[^"]*"|'[^']*'|`[^`]*`|\([^()]*\))*\)|\{[^{}]*\})|`(?![^`]*`)|[^"'>;&|\n\\$`]|(?<=[<>])&|(?<![<>])&(?=>)|(?<=(?:^|\s)[02-9])>{1,2}\|?|"[^"]*"|'[^']*')|(?<!\||>|(?:^|\s)[02-9])>{1,2}\|?\s*(?:"/dev/(?:null|tty|stdout|stderr)"|'/dev/(?:null|tty|stdout|stderr)'|/dev/(?:null|tty|stdout|stderr))(?=[\s;&|]|$))*(?<!\||>|(?:^|\s)[02-9])>{1,2}\|?\s*(?!(?:"/dev/(?:null|tty|stdout|stderr)"|'/dev/(?:null|tty|stdout|stderr)'|/dev/(?:null|tty|stdout|stderr))(?:[\s;&|]|$))[$\w./~"'-]"#,
            "write",
            "Use the `write` tool instead of echo/cat redirection. It handles encoding and provides confirmation. `write` replaces the whole file; to append to an existing file, use `edit` to insert after its last line.",
        ),
        rule(r#"^\s*nohup\s+|(?<!&)&\s*$"#, "bash",
            "Use `bash` with `name` instead of nohup or background shell syntax so the service stays observable and managed."),
        rule(
            r#"^\s*(?:(?:bun|npm|pnpm|yarn)\s+(?:run\s+)?(?:dev|start)(?:\s|$)|(?:vite|next\s+dev|nuxt\s+dev|nodemon|lldb|gdb|tail\s+-f)(?:\s|$)|docker\s+compose\s+up(?!.*(?:\s-d(?:\s|$)|--detach))(?:\s|$))"#,
            "bash",
            "Use `bash` with `name` for services, watchers, and debuggers; inspect with `read proc://<name>`.",
        ),
        rule(
            r#"^\s*(?:(?:bun|npm|pnpm|yarn)\s+(?:run\s+)?\S+|cargo\s+watch|watchexec|pytest|vitest|jest|tsc)(?:.|\n)*(?:--watch|-w)(?:\s|$)"#,
            "bash",
            "Use `bash` with `name` for watch mode so its output, input, and lifecycle stay managed.",
        ),
    ]
}

/// OMP key → native dotted key; `config import-omp` copies the value verbatim unless `convert` handles it.
pub(super) const OMP: &[(&str, &str)] = &[
    ("tools.artifactSpillThreshold", "artifact.spill_threshold_kb"),
    ("tools.artifactHeadBytes", "artifact.head_kb"),
    ("tools.artifactTailBytes", "artifact.tail_kb"),
    ("tools.artifactTailLines", "artifact.tail_lines"),
    ("tools.artifactMaxBytes", "artifact.max_bytes_mb"),
    ("tools.outputMaxColumns", "output_max_columns"),
    ("tools.approval", "approval"),
    ("tools.restrictedFolders", "restricted_folders"),
    ("tools.intentTracing", "intent_tracing"),
    ("tools.maxTimeout", "max_timeout"),
    ("out.enabled", "out.enabled"),
    ("todo.reminders", "todo.reminders"),
    ("todo.remindersMax", "todo.reminders_max"),
    ("todo.midRunNudges", "todo.mid_run_nudges"),
    ("todo.eager", "todo.eager"),
    ("todo.backend", "todo.backend"),
    ("tasks.todoClearDelay", "todo.clear_delay"),
    ("async.maxJobs", "async.max_jobs"),
    ("async.printWake", "async.print_wake"),
    ("async.widgets", "async.widgets"),
    ("shellPath", "shell_path"),
    ("bash.allowCompoundCommands", "bash.allow_compound_commands"),
    ("shellMinimizer.enabled", "shell_minimizer.enabled"),
    ("shellMinimizer.sourceOutlineLevel", "shell_minimizer.source_outline_level"),
    ("bash.autoBackground.enabled", "bash.auto_background.enabled"),
    ("bash.autoBackground.thresholdMs", "bash.auto_background.threshold_ms"),
    ("bash.gitGuard", "bash.git_guard"),
    ("bash.patterns", "bash.patterns"),
    ("bash.allowedEnvVars", "bash.allowed_env_vars"),
    ("bashInterceptor.enabled", "bash_interceptor.enabled"),
    ("bashInterceptor.patterns", "bash_interceptor.patterns"),
    ("bash.direnv", "bash.direnv"),
    ("bash.direnvLoadTimeoutMs", "bash.direnv_load_timeout_ms"),
];

/// Translates an OMP value whose shape or units differ from the native field; `None` copies it verbatim.
pub(super) fn convert(_from: &str, _value: &Value) -> Option<Result<toml::Value>> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn omp_rows_import_with_native_units_and_enums() {
        let yaml = "tools:\n  artifactSpillThreshold: 64\n  artifactHeadBytes: 0\n  outputMaxColumns: 0\n  approval:\n    bash: prompt\n  restrictedFolders: [\"~/secrets\", /srv/pay]\n  intentTracing: false\n  maxTimeout: 90\n  artifactMaxBytes: 0\nout:\n  enabled: false\ntodo:\n  reminders: false\n  remindersMax: 1\n  midRunNudges: backoff\n  eager: always\nasync:\n  maxJobs: 4\n  printWake: true\nshellPath: /bin/zsh\nbash:\n  allowCompoundCommands: true\n  gitGuard: true\n  allowedEnvVars: [CARGO_TARGET_DIR]\n  autoBackground:\n    enabled: false\n    thresholdMs: 5000\n  direnv: off\n  direnvLoadTimeoutMs: 100\n  patterns:\n    - match: \"cargo test *\"\n      approval: allow\n      env: [RUST_LOG]\nbashInterceptor:\n  enabled: true\n  patterns:\n    - pattern: \"^\\\\s*cat \"\n      tool: read\n      message: use read\n";
        let table = crate::config::settings::translate_omp(yaml).unwrap();
        let cfg = crate::config::settings::decode(toml::Value::Table(table)).unwrap();
        let b = &cfg.bash_opts;
        assert_eq!(b.artifact.spill_threshold_kb, 64);
        assert_eq!((b.artifact.head_kb, b.artifact.max_bytes_mb), (0, 0));
        assert_eq!(b.output_max_columns, 0);
        assert_eq!(b.approval["bash"], "prompt");
        assert_eq!(b.restricted_folders, ["~/secrets", "/srv/pay"]);
        assert!(!b.intent_tracing, "intentTracing");
        assert_eq!(b.max_timeout, 90);
        assert!(!b.todo.reminders, "todo.reminders");
        assert!(!b.out.enabled, "out.enabled");
        assert_eq!((b.todo.reminders_max, b.todo.mid_run_nudges, b.todo.eager), (1, MidRunNudges::Backoff, Eager::Always));
        assert_eq!((b.r#async.max_jobs, b.r#async.print_wake), (4, true));
        assert_eq!(b.shell_path.as_deref(), Some("/bin/zsh"));
        assert!(b.bash.allow_compound_commands && b.bash.git_guard);
        assert_eq!(b.bash.allowed_env_vars, ["CARGO_TARGET_DIR"]);
        assert!(!b.bash.auto_background.enabled && b.bash.auto_background.threshold_ms == 5000);
        assert_eq!((b.bash.direnv, b.bash.direnv_load_timeout_ms), (Direnv::Off, 100));
        assert_eq!(b.bash.patterns[0].approval, RuleApproval::Allow);
        assert_eq!(b.bash.patterns[0].env, ["RUST_LOG"]);
        assert!(b.bash_interceptor.enabled);
        assert_eq!(b.bash_interceptor.patterns[0].tool, "read");
    }

    #[test]
    fn defaults_keep_native_budgets_and_otherwise_match_omp() {
        let b = BashOpts::default();
        assert_eq!((b.artifact.spill_threshold_kb, b.artifact.head_kb, b.artifact.tail_kb), (8, 4, 4));
        assert_eq!((b.artifact.tail_lines, b.artifact.max_bytes_mb), (500, 16));
        assert!(b.todo.reminders && b.todo.reminders_max == 3);
        assert!(!b.bash.auto_background.enabled && b.bash.auto_background.threshold_ms == 60_000);
        assert_eq!(b.bash.direnv_load_timeout_ms, 30_000);
        assert_eq!(b.max_timeout, 600); // OMP defaults to 0; the native cap stays unless configured away.
        assert_eq!(b.bash_interceptor.patterns.len(), 10);
    }
}
