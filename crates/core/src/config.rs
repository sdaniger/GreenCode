use crate::permission::Mode;

pub mod settings;
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, path::PathBuf};

const DEFAULT_COMPACT_AT: u64 = 250_000;

#[derive(Deserialize, Serialize, Clone, Debug)]
#[serde(default)]
pub struct Config {
    /// `provider/model`.
    pub model: String,
    /// Default sub-agent model for new native threads; unset follows the parent.
    pub subagent_model: Option<String>,
    /// Named model roles, for example `task`, `advisor`, `plan` and `slow`.
    pub model_roles: BTreeMap<String, String>,
    /// Extra entries for the model picker.
    pub models: Vec<String>,
    pub mode: Mode,
    /// Anthropic `output_config.effort`; unset = API default.
    pub effort: Option<String>,
    /// Per-thread OpenCode override; not loaded from config.toml.
    #[serde(skip)]
    pub opencode_effort: Option<String>,
    pub max_tokens: u32,
    /// Maximum tool calls in one turn; zero means unlimited.
    pub max_tool_calls: u32,
    /// Summarize history once the context reaches this many tokens.
    pub compact_at: u64,
    /// Fixed token limits by exact `provider/model` or trailing-`*` prefix. Zero clears an inherited selector.
    pub model_compact_at: BTreeMap<String, u64>,
    pub model_compact_at_enabled: bool,
    /// Whether automatic compaction is enabled.
    pub compaction_enabled: bool,
    /// Auto-compact between model calls in a main-agent turn; subagents still compact mid-turn.
    pub compaction_mid_turn: bool,
    /// Compaction method: `summary` or `snap`.
    pub compaction: String,
    /// Replay older successful reads as previews with a path to the exact result.
    pub old_read_previews: bool,
    /// Empty means every built-in tool. Otherwise only these names are exposed and accepted.
    pub tools: Vec<String>,
    /// Disables provider tools entirely, even if an allowlist is configured.
    pub no_tools: bool,
    /// `ext` tools turned off in settings; they leave the catalog and are refused.
    pub ext_off: Vec<String>,
    /// Include AGENTS.md and CLAUDE.md in native context.
    pub context_files: bool,
    /// Extra instruction basenames in the native thread's working directory.
    pub context_files_extra: Vec<String>,
    pub skills_enabled: bool,
    pub rules_enabled: bool,
    pub extensions_enabled: bool,
    pub mcp_project_config: bool,
    /// Optional replacement and additional text for the default system prompt.
    pub system_prompt: Option<String>,
    pub append_system_prompt: Option<String>,
    pub providers: BTreeMap<String, ProviderCfg>,
    /// Restrict `bash` writes to the project, temp directories and the GreenCode home (macOS).
    pub sandbox: bool,
    /// `provider/model` consulted by the `advisor` ext tool; unset disables it.
    pub advisor_model: Option<String>,
    /// Hermes-like durable learning through the lazy `ext` tool and optional auto-distillation.
    pub learning: LearningCfg,
    /// GitHub `OWNER/REPO` used by `greencode-cli update`; unset disables update checks.
    pub update_repo: Option<String>,
    pub prices: BTreeMap<String, Vec<f64>>,
    /// External agents spoken to over ACP or native Pi/OMP RPC.
    pub backends: Vec<AcpCfg>,
    /// Terminal UI display and input preferences.
    pub tui: TuiCfg,
    /// Sub-agent task limits; defined in `subagents`.
    pub task: crate::subagents::TaskConfig,
    #[serde(skip)]
    pub policy: Option<crate::policy::Policy>,
}

#[derive(Deserialize, Serialize, Clone, Debug)]
#[serde(default)]
pub struct TuiCfg {
    pub hide_thinking: bool,
    pub hide_tools: bool,
    pub expand_tools: bool,
    pub tight: bool,
    pub show_usage: bool,
    pub show_turn_usage: bool,
    pub show_turn_time: bool,
    pub auto_resume: bool,
    pub autocomplete_max_visible: usize,
    pub show_terminal_progress: bool,
    pub recall_cleared_drafts: bool,
    /// Optional OMP-style JSON color palette. Relative paths are resolved from the launch directory.
    pub theme: Option<std::path::PathBuf>,
    /// OMP keymaps loaded in order; the last file wins per action.
    #[serde(deserialize_with = "keybinding_paths")]
    pub keybindings: Vec<PathBuf>,
    /// What Enter does while a turn runs: queue the message or steer the running turn.
    pub follow_up: FollowUp,
}

impl Default for TuiCfg {
    fn default() -> Self {
        Self {
            hide_thinking: false, hide_tools: false, show_usage: true, follow_up: FollowUp::Queue,
            expand_tools: false, tight: false,
            show_turn_usage: false, show_turn_time: false,
            recall_cleared_drafts: true,
            auto_resume: false, autocomplete_max_visible: 10, show_terminal_progress: false, theme: None,
            keybindings: Vec::new(),
        }
    }
}

fn keybinding_paths<'de, D: serde::Deserializer<'de>>(de: D) -> Result<Vec<PathBuf>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Input { Single(PathBuf), Layers(Vec<PathBuf>) }
    Ok(match Input::deserialize(de)? {
        Input::Single(path) => vec![path],
        Input::Layers(paths) => paths,
    })
}

#[derive(Deserialize, Serialize, Clone, Copy, Debug, Default, PartialEq)]
#[serde(rename_all = "kebab-case")]
pub enum FollowUp {
    #[default]
    Queue,
    Steer,
}

#[derive(Deserialize, Serialize, Clone, Debug)]
#[serde(default)]
pub struct LearningCfg {
    /// Enables the `ext` learning tool.
    pub enabled: bool,
    /// Automatically saves an explicit `Lesson:`/`Durable lesson:` line from the final reply without another model request.
    pub auto: bool,
    /// Optional model role/spec used for automatic distillation; setting it opts into one extra request per completed turn.
    pub model: Option<String>,
    /// Memory destination for auto-learned facts: `project` or `global`.
    pub scope: String,
    /// Maximum characters of one durable fact.
    pub max_chars: usize,
}

impl Default for LearningCfg {
    fn default() -> Self {
        Self { enabled: true, auto: true, model: None, scope: "project".into(), max_chars: 600 }
    }
}

#[derive(Deserialize, Serialize, Clone, Debug, Default)]
pub struct ProviderCfg {
    /// `anthropic` or `openai` (any OpenAI-compatible server).
    pub kind: String,
    pub base_url: String,
    /// Environment variable holding the key; empty = none.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub api_key_env: String,
    /// Key stored in config.toml; wins over `api_key_env`. Neither set = no auth (local servers).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    /// Bare model names offered in the picker as `provider/model`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub models: Vec<String>,
    /// The key came from auth.json, not config.toml.
    #[serde(skip)]
    pub stored: bool,
    /// The stored credential is an OAuth token.
    #[serde(skip)]
    pub oauth: bool,
}

/// Built-in providers: (name, kind, base URL, key variable). config.toml entries of the same name win.
pub const PRESETS: &[(&str, &str, &str, &str)] = &[
    ("anthropic", "anthropic", "https://api.anthropic.com", "ANTHROPIC_API_KEY"),
    ("openai", "openai", "https://api.openai.com/v1", "OPENAI_API_KEY"),
    ("chatgpt", "codex", "https://chatgpt.com/backend-api/codex", ""),
    ("openrouter", "openai", "https://openrouter.ai/api/v1", "OPENROUTER_API_KEY"),
    ("google", "openai", "https://generativelanguage.googleapis.com/v1beta/openai", "GEMINI_API_KEY"),
    ("xai", "openai", "https://api.x.ai/v1", "XAI_API_KEY"),
    ("groq", "openai", "https://api.groq.com/openai/v1", "GROQ_API_KEY"),
    ("deepseek", "openai", "https://api.deepseek.com/v1", "DEEPSEEK_API_KEY"),
    ("mistral", "openai", "https://api.mistral.ai/v1", "MISTRAL_API_KEY"),
    ("together", "openai", "https://api.together.xyz/v1", "TOGETHER_API_KEY"),
    ("fireworks", "openai", "https://api.fireworks.ai/inference/v1", "FIREWORKS_API_KEY"),
    ("cerebras", "openai", "https://api.cerebras.ai/v1", "CEREBRAS_API_KEY"),
    ("deepinfra", "openai", "https://api.deepinfra.com/v1/openai", "DEEPINFRA_API_KEY"),
    ("perplexity", "openai", "https://api.perplexity.ai", "PERPLEXITY_API_KEY"),
    ("nvidia", "openai", "https://integrate.api.nvidia.com/v1", "NVIDIA_API_KEY"),
    ("huggingface", "openai", "https://router.huggingface.co/v1", "HF_TOKEN"),
    ("github", "openai", "https://models.github.ai/inference", "GITHUB_TOKEN"),
    ("github-copilot", "openai", "https://api.githubcopilot.com", "COPILOT_GITHUB_TOKEN"),
    ("moonshot", "openai", "https://api.moonshot.ai/v1", "MOONSHOT_API_KEY"),
    ("zai", "openai", "https://api.z.ai/api/paas/v4", "ZAI_API_KEY"),
    ("ollama", "openai", "http://localhost:11434/v1", ""),
    ("lmstudio", "openai", "http://localhost:1234/v1", ""),
];

#[derive(Deserialize, Serialize, Clone, Debug)]
pub struct AcpCfg {
    pub name: String,
    pub command: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub protocol: Protocol,
}

#[derive(Deserialize, Serialize, Clone, Copy, Debug, Default, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum Protocol {
    #[default]
    Acp,
    Pi,
    Omp,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            model: "anthropic/claude-opus-5-5".into(),
            subagent_model: None,
            model_roles: BTreeMap::new(),
            models: vec![],
            mode: Mode::Full,
            effort: None,
            opencode_effort: None,
            max_tokens: 64000,
            max_tool_calls: 0,
            compact_at: DEFAULT_COMPACT_AT,
            model_compact_at: BTreeMap::new(),
            model_compact_at_enabled: true,
            compaction_enabled: true,
            compaction_mid_turn: true,
            compaction: "summary".into(),
            old_read_previews: false,
            tools: vec![],
            no_tools: false,
            ext_off: vec![],
            context_files: true,
            context_files_extra: Vec::new(),
            skills_enabled: true,
            rules_enabled: true,
            extensions_enabled: true,
            mcp_project_config: true,
            system_prompt: None,
            append_system_prompt: None,
            providers: BTreeMap::new(),
            sandbox: false,
            advisor_model: None,
            learning: LearningCfg::default(),
            update_repo: None,
            prices: BTreeMap::new(),
            policy: None,
            backends: vec![],
            tui: TuiCfg::default(),
            task: Default::default(),
        }
    }
}

impl Config {
    pub fn load() -> anyhow::Result<Config> { Self::load_with_overlays(&[]) }

    /// Loads the global config.toml, then layers each explicitly named overlay file over it (last wins).
    /// Overlays are never discovered automatically, so an untrusted worktree cannot change settings.
    pub fn load_with_overlays(overlays: &[std::path::PathBuf]) -> anyhow::Result<Config> {
        let path = crate::home().join("config.toml");
        let explicit = settings::merged(&path, overlays)?;
        let mut cfg = settings::decode(explicit.clone()).map_err(|e| anyhow::anyhow!("merged configuration ({}): {e}", settings::describe(&path, overlays)))?;
        cfg.policy = crate::policy::load()?;
        if let Some(policy) = &cfg.policy {
            if let Some(s) = &policy.settings.default_mode {
                if s.locked || explicit.get("mode").is_none() { cfg.mode = s.value; }
            }
            if let Some(s) = &policy.settings.sandbox {
                if s.locked || explicit.get("sandbox").is_none() { cfg.sandbox = s.value; }
            }
            if let Some(max) = policy.limits.max_mode {
                if policy.check_mode(cfg.mode).is_err() { cfg.mode = max; }
            }
        }
        for &(name, kind, url, env) in PRESETS {
            cfg.providers.entry(name.into()).or_insert(ProviderCfg {
                kind: kind.into(),
                base_url: url.into(),
                api_key_env: env.into(),
                ..Default::default()
            });
        }
        for (name, (key, oauth)) in crate::auth::stored() {
            if let Some(p) = cfg.providers.get_mut(&name).filter(|p| p.api_key.is_none()) {
                p.api_key = Some(key);
                p.stored = true;
                p.oauth = oauth;
            }
        }
        for agent in crate::registry::installed() {
            let mut backend = agent.backend();
            if backend.name == crate::antigravity::REGISTRY_ID {
                backend.name = crate::antigravity::BACKEND.into();
            }
            if !cfg.backends.iter().any(|b| b.name == backend.name) { cfg.backends.push(backend); }
        }
        for (name, protocol, args) in [
            ("opencode", Protocol::Acp, vec!["opencode", "acp"]),
            ("pi", Protocol::Pi, vec!["pi"]),
            ("omp", Protocol::Omp, vec!["omp"]),
            ("codex", Protocol::Acp, vec!["npx", "-y", "@agentclientprotocol/codex-acp@2.6.0"]),
            ("claude-code", Protocol::Acp, vec!["npx", "-y", "@agentclientprotocol/claude-agent-acp@0.106.0"]),
        ] {
            if !cfg.backends.iter().any(|b| b.name == name) {
                let mut command: Vec<String> = args.into_iter().map(String::from).collect();
                if let Some(path) = executable(&command[0]) { command[0] = path.to_string_lossy().into_owned(); }
                cfg.backends.push(AcpCfg { name: name.into(), command, protocol, env: Default::default() });
            }
        }
        if !cfg.backends.iter().any(|b| b.name == crate::antigravity::BACKEND) {
            let (command, env, _) = crate::antigravity::resolve_command();
            cfg.backends.push(AcpCfg {
                name: crate::antigravity::BACKEND.into(),
                command,
                protocol: Protocol::Acp,
                env,
            });
        }
        // Agents T3 Code supports; listed only when installed.
        for (name, exe, args) in [
            ("kiro", "kiro-cli", &["acp", "--agent-engine=v3", "--auth-method=cli"][..]),
            ("grok", "grok", &["agent", "stdio"]),
            ("fx", "fx", &["acp"]),
            ("gemini", "gemini", &["--experimental-acp"]),
        ] {
            if cfg.backends.iter().any(|b| b.name == name) { continue }
            if let Some(path) = executable(exe) {
                let command = std::iter::once(path.to_string_lossy().into_owned()).chain(args.iter().map(|a| a.to_string())).collect();
                cfg.backends.push(AcpCfg { name: name.into(), command, protocol: Protocol::Acp, env: Default::default() });
            }
        }
        // A broken or removed Hermes profile must not make the whole config unusable.
        for profile in crate::hermes::imported().unwrap_or_else(|e| { eprintln!("Hermes profiles skipped: {e:#}"); vec![] }) {
            let backend = match profile.backend() { Ok(b) => b, Err(e) => { eprintln!("Hermes profile skipped: {e:#}"); continue } };
            match cfg.backends.iter().find(|b| b.name == backend.name) {
                Some(existing) if existing.command != backend.command || existing.protocol != backend.protocol => eprintln!("Hermes profile skipped: backend `{}` already configured", backend.name),
                Some(_) => {}
                None => cfg.backends.push(backend),
            }
        }
        Ok(cfg)
    }

    /// Returns a configured role model, falling back to the supplied provider/model.
    pub fn role_model<'a>(&'a self, role: &str, fallback: Option<&'a str>) -> Option<&'a str> {
        self.model_roles.get(role).map(String::as_str).or(fallback)
    }

    /// The configured model for a role, or the native default when no override exists.
    pub fn role_or_default<'a>(&'a self, role: &str) -> &'a str {
        self.model_roles.get(role).map(String::as_str).unwrap_or(&self.model)
    }

    /// Models for the picker: the default first, then `models`, then each provider's `models`.
    pub fn model_list(&self) -> Vec<String> {
        let mut v = vec![self.model.clone()];
        let provided = self.providers.iter().flat_map(|(name, p)| p.models.iter().map(move |m| format!("{name}/{m}")));
        for m in self.models.iter().cloned().chain(provided) {
            if !v.contains(&m) { v.push(m); }
        }
        v
    }

    /// Dollar cost of one request, when the model has a `prices` entry.
    pub fn cost(&self, model: &str, u: &crate::Usage) -> Option<f64> {
        let p = self.prices.get(model)?;
        let price = |i: usize| p.get(i).or(p.first()).copied().unwrap_or(0.);
        Some((u.input as f64 * price(0) + u.output as f64 * price(1) + u.cache_read as f64 * price(2) + u.cache_write as f64 * price(3)) / 1e6)
    }

    /// Adds a provider and appends it to config.toml, leaving the rest of the file untouched.
    pub fn add_provider(&mut self, name: &str, p: ProviderCfg) -> anyhow::Result<()> {
        anyhow::ensure!(!name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'), "provider name must be letters, digits, `-` or `_`");
        let mut p = p;
        // A built-in name with no URL only adds a key or models to that provider.
        if let Some(&(_, kind, url, env)) = PRESETS.iter().find(|preset| preset.0 == name).filter(|_| p.base_url.is_empty()) {
            (p.kind, p.base_url) = (kind.into(), url.into());
            if p.api_key_env.is_empty() { p.api_key_env = env.into(); }
        }
        anyhow::ensure!(matches!(p.kind.as_str(), "openai" | "anthropic"), "kind must be `openai` or `anthropic`");
        anyhow::ensure!(p.base_url.starts_with("http://") || p.base_url.starts_with("https://"), "base URL must start with http:// or https://");
        let path = crate::home().join("config.toml");
        let mut text = std::fs::read_to_string(&path).unwrap_or_default();
        if !text.is_empty() && !text.ends_with('\n') { text.push('\n'); }
        text.push_str(&format!("\n[providers.{name}]\n{}", toml::to_string(&p)?));
        // A provider already in the file makes a duplicate table, which fails here before anything is written.
        toml::from_str::<Config>(&text).map_err(|e| anyhow::anyhow!("cannot add provider `{name}`: {}", e.message()))?;
        std::fs::create_dir_all(crate::home())?;
        std::fs::write(&path, text)?;
        #[cfg(unix)]
        if p.api_key.is_some() {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
        }
        // Adding models to a preset must not drop the auth.json key or OAuth token loaded for it.
        let mut p = p;
        if let Some(old) = self.providers.get(name).filter(|old| p.api_key.is_none() && old.stored) {
            (p.api_key, p.stored, p.oauth) = (old.api_key.clone(), old.stored, old.oauth);
        }
        self.providers.insert(name.into(), p);
        Ok(())
    }
}

/// GUI launches do not necessarily inherit the user's login-shell PATH.
fn search_dirs() -> Vec<std::path::PathBuf> {
    let mut dirs: Vec<_> = std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()).collect();
    dirs.extend([".opencode/bin", ".local/bin", ".bun/bin"].map(|p| crate::user_home().join(p)));
    dirs.extend(["/opt/homebrew/bin", "/usr/local/bin", "/usr/bin"].map(std::path::PathBuf::from));
    #[cfg(target_os = "macos")]
    dirs.push(std::path::PathBuf::from("/Applications/Antigravity.app/Contents/Resources/app/bin"));
    #[cfg(windows)]
    if let Some(program_files) = std::env::var_os("ProgramFiles") {
        let git = std::path::PathBuf::from(program_files).join("Git");
        dirs.extend([git.join("bin"), git.join("usr").join("bin")]);
    }
    dirs
}

pub(crate) fn agent_path() -> anyhow::Result<std::ffi::OsString> { Ok(std::env::join_paths(search_dirs())?) }

fn is_executable(path: &std::path::Path) -> bool {
    #[cfg(unix)] {
        use std::os::unix::fs::PermissionsExt;
        path.metadata().is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    }
    #[cfg(not(unix))] { path.is_file() }
}

pub fn executable(name: &str) -> Option<std::path::PathBuf> {
    let direct = std::path::Path::new(name);
    if direct.is_absolute() {
        return find_executable(direct.to_path_buf());
    }
    search_dirs().into_iter().find_map(|dir| find_executable(dir.join(name)))
}

fn find_executable(path: std::path::PathBuf) -> Option<std::path::PathBuf> {
    if is_executable(&path) {
        return Some(path);
    }
    #[cfg(windows)]
    if path.extension().is_none() {
        return ["exe", "cmd", "bat"].into_iter().map(|ext| path.with_extension(ext)).find(|p| is_executable(p));
    }
    None
}
