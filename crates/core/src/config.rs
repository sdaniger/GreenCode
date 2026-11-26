use crate::permission::Mode;

pub mod advisor;
pub mod agent_loop;
pub mod bash;
pub mod browser;
pub mod collab;
pub mod compaction;
pub mod context;
pub mod disassembler;
pub mod discovery;
pub mod extra_tools;
pub mod files;
pub mod integrations;
pub mod interaction;
pub mod isolation;
pub mod memory;
pub mod mnemopi;
pub mod modes;
pub mod provider;
pub mod services;
pub mod services_extra;
pub mod roles;
pub mod settings;
pub mod ttsr;
pub use services::SpeechMode;
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
    /// Secret providers (1Password) that `op://` references are read through.
    pub secret_providers: Vec<crate::secrets::SecretProvider>,
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
    /// `[backend_overrides.NAME]`: launch changes (binary, arguments, environment, extra models) that also apply to auto-detected and installed backends.
    pub backend_overrides: BTreeMap<String, BackendOverride>,
    /// Terminal UI display and input preferences.
    pub tui: TuiCfg,
    /// Sub-agent task limits; defined in `subagents`.
    pub task: crate::subagents::TaskConfig,
    /// Agent-managed worktrees: base directory, cloning and session lifecycle.
    pub worktree: crate::worktree::WorktreeCfg,
    #[serde(flatten)]
    pub provider_opts: provider::ProviderOpts,
    #[serde(flatten)]
    pub loop_opts: agent_loop::LoopOpts,
    #[serde(flatten)]
    pub bash_opts: bash::BashOpts,
    #[serde(flatten)]
    pub file_opts: files::FileOpts,
    #[serde(flatten)]
    pub integration_opts: integrations::IntegrationOpts,
    #[serde(flatten)]
    pub interaction_opts: interaction::InteractionOpts,
    #[serde(flatten)]
    pub collab_opts: collab::CollabOpts,
    #[serde(flatten)]
    pub compaction_opts: compaction::CompactionOpts,
    #[serde(flatten)]
    pub context_opts: context::ContextOpts,
    #[serde(flatten)]
    pub assist_opts: advisor::AdvisorOpts,
    #[serde(flatten)]
    pub disasm_opts: disassembler::DisassemblerOpts,
    #[serde(flatten)]
    pub extra_tools_opts: extra_tools::ExtraToolsOpts,
    #[serde(flatten)]
    pub memory_opts: memory::MemoryOpts,
    #[serde(flatten)]
    pub services_opts: services::ServicesOpts,
    #[serde(flatten)]
    pub services_extra_opts: services_extra::ServicesExtraOpts,
    #[serde(flatten)]
    pub ttsr_opts: ttsr::TtsrOpts,
    #[serde(flatten)]
    pub modes: modes::ModesOpts,
    #[serde(flatten)]
    pub isolation_opts: isolation::IsolationOpts,
    #[serde(flatten)]
    pub discovery: discovery::DiscoveryOpts,
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
    /// Roles (or `default`) in Ctrl+P/Ctrl+Shift+P cycling order; unlisted models follow in config order.
    pub cycle_order: Vec<String>,
    pub git: GitTui,
    /// Palette used when the terminal background is dark; falls back to `theme`.
    pub theme_dark: Option<PathBuf>,
    /// Palette used when the terminal background is light; falls back to `theme`.
    pub theme_light: Option<PathBuf>,
    pub symbol_preset: SymbolPreset,
    /// Blue instead of green for successful tool output and diff additions.
    pub color_blind_mode: bool,
    pub status_line: StatusLineCfg,
    pub composer: ComposerTui,
    pub terminal: TerminalTui,
    /// Color headings and code in Markdown; off keeps them plain bold.
    pub colorful_markdown: bool,
    /// Report the run state in the terminal title (`>` your turn, `!` waiting, spinner while working).
    pub title_state: bool,
    pub title_spinner: TitleSpinner,
    /// Milliseconds between animation frames; unset follows Motion.
    pub spinner_interval: Option<u64>,
    /// Upper bound on repaints per second; 0 disables the cap.
    pub max_fps: u32,
    pub motion: Motion,
    /// Leave a blank row between the input and the composer's bottom border for macOS IME preedit.
    pub ime_safe_cursor: bool,
    pub display: DisplayTui,
    pub show_hardware_cursor: bool,
    #[serde(rename = "loop")]
    pub loop_ui: LoopTui,
    pub double_escape_action: DoubleEscape,
    pub input: InputTui,
    /// Expand `:name:` shortcodes and emoticons like `:D` when a prompt is submitted.
    pub emoji_autocomplete: bool,
    pub paste: PasteTui,
    pub update: UpdateTui,
    pub notify: NotifyTui,
    pub ask: AskTui,
    pub session: SessionTui,
    /// Keep finished thinking blocks expanded instead of collapsing them when the turn ends.
    pub expand_thinking_blocks: bool,
    /// Replace fenced code blocks in thinking summaries with an ellipsis.
    pub prose_only_thinking: bool,
    pub read: ReadTui,
    pub plan: PlanTui,
    pub title: TitleTui,
    pub skills: SkillsTui,
    pub mcp: McpTui,
    /// Modal prompt editing (OMP `tui.vimMode`).
    pub vim_mode: bool,
    pub vim_mode_display: VimModeDisplay,
    /// Default filter mode when opening the session tree.
    pub tree_filter_mode: TreeFilterMode,
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
            cycle_order: ["smol", "default", "slow"].map(String::from).to_vec(),
            git: GitTui::default(), theme_dark: None, theme_light: None,
            symbol_preset: SymbolPreset::default(), color_blind_mode: false, status_line: StatusLineCfg::default(),
            composer: ComposerTui::default(), terminal: TerminalTui::default(), colorful_markdown: true,
            title_state: true, title_spinner: TitleSpinner::default(), spinner_interval: None,
            max_fps: 30, motion: Motion::default(), ime_safe_cursor: false,
            display: DisplayTui::default(), show_hardware_cursor: true,
            loop_ui: LoopTui::default(), double_escape_action: DoubleEscape::default(),
            input: InputTui::default(), emoji_autocomplete: true, paste: PasteTui::default(),
            update: UpdateTui::default(), notify: NotifyTui::default(), ask: AskTui::default(),
            session: SessionTui::default(), expand_thinking_blocks: false, prose_only_thinking: true,
            read: ReadTui::default(), plan: PlanTui::default(), title: TitleTui::default(),
            skills: SkillsTui::default(), mcp: McpTui::default(),
            vim_mode: false, vim_mode_display: VimModeDisplay::Text,
            tree_filter_mode: TreeFilterMode::Default,
        }
    }
}

#[derive(Deserialize, Serialize, Clone, Debug)]
#[serde(default)]
pub struct GitTui {
    /// Show branch and a dirty marker in the status row.
    pub enabled: bool,
}

impl Default for GitTui {
    fn default() -> Self { Self { enabled: true } }
}

#[derive(Deserialize, Serialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum SymbolPreset {
    #[default]
    Unicode,
    Nerd,
    Ascii,
}

/// Working-state spinner in the terminal title.
#[derive(Deserialize, Serialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum TitleSpinner {
    #[default]
    Braille,
    Pulse,
    Dots,
    Line,
}

/// One switch over the TUI's spinners; `reduced` halves their speed, `none` freezes them.
#[derive(Deserialize, Serialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Motion {
    #[default]
    Full,
    Reduced,
    None,
}

#[derive(Deserialize, Serialize, Clone, Debug)]
#[serde(default)]
pub struct ComposerTui {
    pub shape: ComposerShape,
    /// Live tokens-per-second readout in the status row and composer bar.
    pub token_rate: bool,
    /// Ask the active model for next-prompt ghost text after a turn.
    pub predictions: bool,
    /// Request up to four predictions instead of one.
    pub prediction_alternatives: bool,
}

impl Default for ComposerTui {
    fn default() -> Self {
        Self { shape: ComposerShape::Band, token_rate: false, predictions: false, prediction_alternatives: false }
    }
}

/// `band` keeps the single rounded band, `box` draws a double frame, `plain` shows no frame.
#[derive(Deserialize, Serialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ComposerShape {
    #[default]
    Band,
    Box,
    Plain,
}

#[derive(Deserialize, Serialize, Clone, Debug)]
#[serde(default)]
pub struct TerminalTui {
    /// Report working/waiting/done/failed over OSC 7501 for terminal tab indicators.
    pub program_status: bool,
    /// Render images inline in the terminal (kitty graphics or iTerm2 OSC 1337).
    pub show_images: bool,
}

impl Default for TerminalTui {
    fn default() -> Self { Self { program_status: true, show_images: true } }
}

#[derive(Deserialize, Serialize, Clone, Debug)]
#[serde(default)]
pub struct DisplayTui {
    /// Reveal streamed assistant text a few characters per frame.
    pub smooth_streaming: bool,
    /// Divider after a turn whose request read no cached prompt tokens.
    pub cache_miss_marker: bool,
    /// How a settled resize refreshes transcript rows kept at an earlier width.
    pub resize_scrollback: ResizeScrollback,
    /// Render Markdown H1 headings at 2x through Kitty's OSC 66 (Kitty only).
    pub text_sizing: bool,
    /// Render Mermaid fenced blocks as ASCII diagrams.
    pub render_mermaid: bool,
    /// Invite SVG figures and draw them inline on graphics terminals.
    pub render_svg: bool,
    /// Invite `<img>`/`<iframe>` embeds and draw them inline on graphics terminals.
    pub render_embeds: bool,
    /// Chart the agent's numeric Markdown tables on graphics terminals.
    pub auto_graph: AutoGraph,
    /// Invite a leading emoji reaction badge on the user bubble.
    pub reactions: bool,
    /// Celebrate unscheduled Codex usage resets with a fireworks overlay.
    pub codex_reset_fireworks: bool,
    /// Wrap paths and URLs in OSC 8 links (text output).
    pub hyperlinks: Hyperlinks,
    /// Capture mouse clicks for the pinned sub-agent list; wheel scrolls the transcript.
    pub mouse: bool,
    /// Animation for the working/loading label; frozen under reduced or no motion.
    pub shimmer: Shimmer,
    /// Pinned live-agent list above the composer.
    pub pinned_agents: PinnedAgents,
    /// Show each pinned sub-agent's latest tool call beneath its row.
    pub subagent_live_preview: bool,
    /// Draw bash calls as a boxed command/output visualization.
    pub shell_visualization: bool,
    /// Note lines a collapsed advisor card shows; 0 keeps the whole preview.
    pub advisor_collapsed_lines: u8,
    /// Collapse pre-compaction history behind the summary divider.
    pub collapse_compacted: bool,
}

impl Default for DisplayTui {
    fn default() -> Self {
        Self {
            smooth_streaming: true, cache_miss_marker: false, resize_scrollback: ResizeScrollback::default(),
            text_sizing: false, render_mermaid: true, render_svg: true, render_embeds: true,
            auto_graph: AutoGraph::default(), reactions: true, codex_reset_fireworks: false,
            hyperlinks: Hyperlinks::default(), mouse: false, shimmer: Shimmer::default(),
            pinned_agents: PinnedAgents::default(), subagent_live_preview: false,
            shell_visualization: true, advisor_collapsed_lines: 0, collapse_compacted: true,
        }
    }
}

/// `rebuild` rewraps everything at the new width; `append` keeps settled rows and lets the
/// streaming row adapt; `preserve` keeps every existing break.
#[derive(Deserialize, Serialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ResizeScrollback {
    #[default]
    Append,
    Rebuild,
    Preserve,
}

/// `auto`/`smart` chart numeric tables in final replies; `always` also charts streamed tables; `off` never.
#[derive(Deserialize, Serialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum AutoGraph {
    #[default]
    Auto,
    Smart,
    Always,
    Off,
}

#[derive(Deserialize, Serialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Hyperlinks {
    Off,
    #[default]
    Auto,
    Always,
}

#[derive(Deserialize, Serialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Shimmer {
    #[default]
    Classic,
    Kitt,
    Disabled,
}

#[derive(Deserialize, Serialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum PinnedAgents {
    Off,
    #[default]
    Collapsed,
    Full,
}

#[derive(Deserialize, Serialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum TitleIcons {
    #[default]
    #[serde(rename = "nf+emoji")]
    NfEmoji,
    Emoji,
    Boring,
}

/// Status line segments, separators, and behavior.
#[derive(Deserialize, Serialize, Clone, Debug)]
#[serde(default)]
pub struct StatusLineCfg {
    pub preset: StatusPreset,
    pub separator: StatusSeparator,
    /// How the composer's inner line reflects context use (`box` composer only).
    pub context_line: ContextLine,
    /// Paint the editor border and status gap with the session name color.
    pub session_accent: bool,
    /// Use the terminal background instead of the theme's status line background.
    pub transparent: bool,
    /// Show the thinking level as one icon on the model name.
    pub compact_thinking_level: bool,
    /// Show hook status messages below the status line.
    pub show_hook_status: bool,
}

impl Default for StatusLineCfg {
    fn default() -> Self {
        Self {
            preset: StatusPreset::default(), separator: StatusSeparator::default(),
            context_line: ContextLine::default(), session_accent: true, transparent: false,
            compact_thinking_level: true, show_hook_status: true,
        }
    }
}

#[derive(Deserialize, Serialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum StatusPreset {
    #[default]
    Default,
    Minimal,
    Compact,
    Full,
    Nerd,
    Ascii,
    Custom,
}

#[derive(Deserialize, Serialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum StatusSeparator {
    Powerline,
    #[default]
    PowerlineThin,
    Slash,
    Pipe,
    Block,
    None,
    Ascii,
}

#[derive(Deserialize, Serialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ContextLine {
    Off,
    Percentage,
    Annotated,
    #[default]
    Embedded,
}

#[derive(Deserialize, Serialize, Clone, Debug)]
#[serde(default)]
pub struct LoopTui {
    /// `prompt` re-submits, `compact` compacts, `reset` starts a new thread between iterations.
    pub mode: LoopMode,
    /// Maximum wait for `--while`/`--until`; 0 waits forever.
    pub condition_timeout_ms: u64,
}

impl Default for LoopTui {
    fn default() -> Self { Self { mode: LoopMode::Prompt, condition_timeout_ms: 30_000 } }
}

#[derive(Deserialize, Serialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum LoopMode {
    #[default]
    Prompt,
    Compact,
    Reset,
}

#[derive(Deserialize, Serialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum DoubleEscape {
    /// Open prompt history (the closest native equivalent of a rewind selector).
    #[default]
    Rewind,
    /// Open the session picker.
    Tree,
    None,
}

#[derive(Deserialize, Serialize, Clone, Debug)]
#[serde(default)]
pub struct InputTui {
    /// `exit`, `quit` or `q` before the first prompt quits instead of prompting the model.
    pub bare_exit_on_empty_session: bool,
    /// A prompt that is exactly a command name runs it (Enter twice once the session has messages).
    pub bare_slash_commands: bool,
}

impl Default for InputTui {
    fn default() -> Self { Self { bare_exit_on_empty_session: true, bare_slash_commands: false } }
}

#[derive(Deserialize, Serialize, Clone, Debug)]
#[serde(default)]
pub struct PasteTui {
    /// Offer the wrap/save menu from this many pasted lines; 0 disables it.
    pub large_menu_threshold: u64,
}

impl Default for PasteTui {
    fn default() -> Self { Self { large_menu_threshold: 100 } }
}

#[derive(Deserialize, Serialize, Clone, Debug)]
#[serde(default)]
pub struct UpdateTui {
    /// `stable` or `main` (OMP's `canary` imports as `main`).
    pub channel: crate::update::Channel,
    /// Check for a newer build on TUI startup.
    pub check: bool,
    /// Install a newer build in the background on startup.
    pub auto: bool,
}

impl Default for UpdateTui {
    fn default() -> Self { Self { channel: crate::update::Channel::default(), check: true, auto: false } }
}

#[derive(Deserialize, Serialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum OnOff {
    #[default]
    On,
    Off,
}

impl OnOff {
    pub fn on(self) -> bool { self == Self::On }
}

#[derive(Deserialize, Serialize, Clone, Debug)]
#[serde(default)]
pub struct NotifyTui {
    pub completion: OnOff,
    pub error: OnOff,
    /// Notify when an interactive console (bash `pty`) is waiting on you.
    pub console: OnOff,
}

impl Default for NotifyTui {
    fn default() -> Self { Self { completion: OnOff::On, error: OnOff::Off, console: OnOff::On } }
}

#[derive(Deserialize, Serialize, Clone, Debug)]
#[serde(default)]
pub struct AskTui {
    /// Auto-submit the ask form's current values after this many seconds; 0 disables.
    pub timeout: u64,
    pub notify: OnOff,
}

impl Default for AskTui {
    fn default() -> Self { Self { timeout: 0, notify: OnOff::On } }
}

#[derive(Deserialize, Serialize, Clone, Debug)]
#[serde(default)]
pub struct SessionTui {
    /// `repo` includes sessions from other worktrees of the same repository.
    pub worktree_scope: WorktreeScope,
}

impl Default for SessionTui {
    fn default() -> Self { Self { worktree_scope: WorktreeScope::Repo } }
}

/// How the current Vim mode appears in the status line.
#[derive(Deserialize, Serialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum VimModeDisplay {
    #[default]
    Text,
    Icon,
    None,
}

/// Which session tree entries show when the picker opens.
#[derive(Deserialize, Serialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum TreeFilterMode {
    #[default]
    Default,
    NoTools,
    UserOnly,
    LabeledOnly,
    All,
}

#[derive(Deserialize, Serialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum WorktreeScope {
    Directory,
    #[default]
    Repo,
}

#[derive(Deserialize, Serialize, Clone, Debug, Default)]
#[serde(default)]
pub struct ReadTui {
    /// Render read results as Markdown rather than raw source.
    pub render_markdown: bool,
    /// Show read results in full instead of a 240-grapheme summary row.
    pub tool_result_preview: bool,
}

#[derive(Deserialize, Serialize, Clone, Debug)]
#[serde(default)]
pub struct PlanTui {
    /// Write the last plan to disk when plan mode is left.
    pub autosave: bool,
    /// `~`, absolute or cwd-relative; empty uses `.greencode/plans`.
    pub autosave_dir: String,
}

impl Default for PlanTui {
    fn default() -> Self { Self { autosave: false, autosave_dir: String::new() } }
}

#[derive(Deserialize, Serialize, Clone, Debug)]
#[serde(default)]
pub struct TitleTui {
    pub generator: TitleGenerator,
    /// Regenerate a generated title every this many prompts; 0 disables.
    pub refresh_after_prompts: u32,
    /// Icon and short code heading a generated or renamed session title.
    pub icons: TitleIcons,
    /// Regenerate a generated title after a todo-init replan, unless the user named the session.
    pub refresh_on_replan: bool,
}

impl Default for TitleTui {
    fn default() -> Self { Self { generator: TitleGenerator::FirstMessage, refresh_after_prompts: 0, icons: TitleIcons::default(), refresh_on_replan: true } }
}

/// `first-message` keeps the native first-line title; `fork` and `tiny` ask a model.
#[derive(Deserialize, Serialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum TitleGenerator {
    #[default]
    FirstMessage,
    Fork,
    Tiny,
}

#[derive(Deserialize, Serialize, Clone, Debug)]
#[serde(default)]
pub struct SkillsTui {
    /// Register every skill as a `/skill:name` command.
    pub enable_skill_commands: bool,
}

impl Default for SkillsTui {
    fn default() -> Self { Self { enable_skill_commands: true } }
}

#[derive(Deserialize, Serialize, Clone, Debug)]
#[serde(default)]
pub struct McpTui {
    /// Render non-JSON MCP text results as Markdown.
    pub render_markdown_results: bool,
}

impl Default for McpTui {
    fn default() -> Self { Self { render_markdown_results: true } }
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

/// A built-in provider. config.toml entries of the same name win.
pub struct Preset {
    pub name: &'static str,
    /// `anthropic` or `openai`; `codex` is the ChatGPT subscription.
    pub kind: &'static str,
    pub url: &'static str,
    /// Environment variable holding the key; empty = none needed (local servers).
    pub env: &'static str,
    /// Display name.
    pub label: &'static str,
    /// Where the user creates a key; empty when there is none.
    pub key_url: &'static str,
}

const fn p(name: &'static str, kind: &'static str, url: &'static str, env: &'static str, label: &'static str, key_url: &'static str) -> Preset {
    Preset { name, kind, url, env, label, key_url }
}

/// Built-in providers, OMP's API-key catalog included (antigravity has its own backend).
pub const PRESETS: &[Preset] = &[
    p("anthropic", "anthropic", "https://api.anthropic.com", "ANTHROPIC_API_KEY", "Anthropic", "https://console.anthropic.com/settings/keys"),
    p("openai", "openai", "https://api.openai.com/v1", "OPENAI_API_KEY", "OpenAI", "https://platform.openai.com/api-keys"),
    p("chatgpt", "codex", "https://chatgpt.com/backend-api/codex", "", "ChatGPT Plus/Pro", ""),
    p("openrouter", "openai", "https://openrouter.ai/api/v1", "OPENROUTER_API_KEY", "OpenRouter", "https://openrouter.ai/settings/keys"),
    p("google", "openai", "https://generativelanguage.googleapis.com/v1beta/openai", "GEMINI_API_KEY", "Google Gemini", "https://aistudio.google.com/apikey"),
    p("xai", "openai", "https://api.x.ai/v1", "XAI_API_KEY", "xAI", "https://console.x.ai/team/default/api-keys"),
    p("groq", "openai", "https://api.groq.com/openai/v1", "GROQ_API_KEY", "Groq", "https://console.groq.com/keys"),
    p("deepseek", "openai", "https://api.deepseek.com/v1", "DEEPSEEK_API_KEY", "DeepSeek", "https://platform.deepseek.com/api_keys"),
    p("mistral", "openai", "https://api.mistral.ai/v1", "MISTRAL_API_KEY", "Mistral", "https://console.mistral.ai/api-keys"),
    p("together", "openai", "https://api.together.xyz/v1", "TOGETHER_API_KEY", "Together", "https://api.together.xyz/settings/api-keys"),
    p("fireworks", "openai", "https://api.fireworks.ai/inference/v1", "FIREWORKS_API_KEY", "Fireworks", "https://app.fireworks.ai/settings/users/api-keys"),
    p("cerebras", "openai", "https://api.cerebras.ai/v1", "CEREBRAS_API_KEY", "Cerebras", "https://cloud.cerebras.ai/platform/"),
    p("deepinfra", "openai", "https://api.deepinfra.com/v1/openai", "DEEPINFRA_API_KEY", "DeepInfra", "https://deepinfra.com/dash/api_keys"),
    p("perplexity", "openai", "https://api.perplexity.ai", "PERPLEXITY_API_KEY", "Perplexity", "https://www.perplexity.ai/settings/api"),
    p("nvidia", "openai", "https://integrate.api.nvidia.com/v1", "NVIDIA_API_KEY", "NVIDIA", "https://org.ngc.nvidia.com/setup/personal-keys"),
    p("huggingface", "openai", "https://router.huggingface.co/v1", "HF_TOKEN", "Hugging Face", "https://huggingface.co/settings/tokens/new?ownUserPermissions=inference.serverless.write&tokenType=fineGrained"),
    p("github", "openai", "https://models.github.ai/inference", "GITHUB_TOKEN", "GitHub Models", "https://github.com/settings/personal-access-tokens"),
    p("github-copilot", "openai", "https://api.githubcopilot.com", "COPILOT_GITHUB_TOKEN", "GitHub Copilot", ""),
    p("moonshot", "openai", "https://api.moonshot.ai/v1", "MOONSHOT_API_KEY", "Moonshot (Kimi API)", "https://platform.moonshot.ai/console/api-keys"),
    p("zai", "openai", "https://api.z.ai/api/paas/v4", "ZAI_API_KEY", "Z.AI", "https://z.ai/manage-apikey/apikey-list"),
    p("ollama", "openai", "http://localhost:11434/v1", "", "Ollama (ローカル)", ""),
    p("lmstudio", "openai", "http://localhost:1234/v1", "", "LM Studio (ローカル)", ""),
    p("abliteration", "openai", "https://api.abliteration.ai/v1", "ABLITERATION_API_KEY", "Abliteration", "https://abliteration.ai/console"),
    p("aiand", "openai", "https://api.aiand.com/v1", "AIAND_API_KEY", "ai&", "https://console.aiand.com/api-keys"),
    p("aimlapi", "openai", "https://api.aimlapi.com/v1", "AIMLAPI_API_KEY", "AIML API", "https://aimlapi.com/app/keys"),
    p("alibaba-coding-plan", "openai", "https://coding-intl.dashscope.aliyuncs.com/v1", "ALIBABA_CODING_PLAN_API_KEY", "Alibaba Coding Plan", "https://modelstudio.console.alibabacloud.com/"),
    p("alibaba-token-plan", "openai", "https://token-plan.ap-southeast-1.maas.aliyuncs.com/compatible-mode/v1", "ALIBABA_TOKEN_PLAN_API_KEY", "QwenCloud Token Plan", "https://modelstudio.console.alibabacloud.com/"),
    p("baseten", "openai", "https://inference.baseten.co/v1", "BASETEN_API_KEY", "Baseten", "https://app.baseten.co/settings/api_keys"),
    p("bifrost", "openai", "http://localhost:8080/v1", "BIFROST_API_KEY", "Bifrost", "https://docs.getbifrost.ai/features/governance/virtual-keys"),
    p("charm-hyper", "openai", "https://hyper.charm.land/v1", "CHARM_HYPER_API_KEY", "Charm Hyper", "https://hyper.charm.land/"),
    p("chutes", "openai", "https://llm.chutes.ai/v1", "CHUTES_API_KEY", "Chutes", "https://chutes.ai/app/settings/api-keys"),
    p("cline-pass", "openai", "https://api.cline.bot/api/v1", "CLINE_API_KEY", "ClinePass", "https://app.cline.bot/dashboard/account"),
    p("coralbricks", "openai", "https://inference.coralbricks.ai/v1", "CORAL_API_KEY", "CoralBricks", "https://www.coralbricks.ai/api-keys"),
    p("coreweave", "openai", "https://api.inference.wandb.ai/v1", "WANDB_API_KEY", "CoreWeave", "https://wandb.ai/settings"),
    p("firepass", "openai", "https://api.fireworks.ai/inference/v1", "FIREPASS_API_KEY", "Fire Pass", "https://app.fireworks.ai/settings/users/api-keys"),
    p("gmi-cloud", "openai", "https://api.gmi-serving.com/v1", "GMI_API_KEY", "GMI Cloud", "https://console.gmicloud.ai"),
    p("helmcode", "openai", "https://api.helmcode.com/v1", "HELMCODE_API_KEY", "Helmcode", "https://cloud.helmcode.com"),
    p("inception", "openai", "https://api.inceptionlabs.ai/v1", "INCEPTION_API_KEY", "Inception", "https://platform.inceptionlabs.ai/dashboard/api-keys"),
    p("kilo", "openai", "https://api.kilo.ai/api/gateway", "KILO_API_KEY", "Kilo Gateway", "https://app.kilo.ai/profile"),
    p("kimi-code", "openai", "https://api.kimi.com/coding/v1", "KIMI_CODE_API_KEY", "Kimi Code", "https://www.kimi.com/code"),
    p("kiro", "kiro", "https://runtime.us-east-1.kiro.dev", "KIRO_API_KEY", "Kiro (Builder ID / Google / GitHub)", "https://kiro.dev"),
    p("xai-oauth", "openai", "https://api.x.ai/v1", "XAI_OAUTH_TOKEN", "xAI Grok OAuth (SuperGrok / X Premium+)", ""),
    p("litellm", "openai", "http://localhost:4000/v1", "LITELLM_API_KEY", "LiteLLM", "https://docs.litellm.ai/docs/proxy/deploy"),
    p("llamacpp", "openai", "http://127.0.0.1:8080/v1", "", "llama.cpp (ローカル)", ""),
    p("meta", "openai", "https://api.meta.ai/v1", "META_API_KEY", "Meta Model API", "https://developer.meta.com/ai/"),
    p("minimax", "anthropic", "https://api.minimax.io/anthropic", "MINIMAX_API_KEY", "MiniMax", "https://platform.minimax.io/user-center/basic-information/interface-key"),
    p("minimax-code", "anthropic", "https://api.minimax.io/anthropic", "MINIMAX_CODE_API_KEY", "MiniMax Token Plan", "https://platform.minimax.io/subscribe/token-plan"),
    p("minimax-code-cn", "anthropic", "https://api.minimaxi.com/anthropic", "MINIMAX_CODE_CN_API_KEY", "MiniMax Token Plan (中国)", "https://platform.minimaxi.com/subscribe/token-plan"),
    p("modal", "openai", "https://inference.us-west.modal.direct/v1", "MODAL_PROXY_TOKEN", "Modal", "https://modal.com/settings/proxy-auth-tokens"),
    p("modular", "openai", "https://api.modular.com/v1", "MODULAR_API_KEY", "Modular", "https://console.modular.com/api_keys"),
    p("nanogpt", "openai", "https://nano-gpt.com/api/v1", "NANO_GPT_API_KEY", "NanoGPT", "https://nano-gpt.com/api"),
    p("novita", "openai", "https://api.novita.ai/openai/v1", "NOVITA_API_KEY", "Novita", "https://novita.ai/settings/key-management"),
    p("ollama-cloud", "openai", "https://ollama.com/v1", "OLLAMA_API_KEY", "Ollama Cloud", "https://ollama.com/settings/keys"),
    p("opencode-go", "openai", "https://opencode.ai/zen/go/v1", "OPENCODE_API_KEY", "OpenCode Go", "https://opencode.ai/auth"),
    p("opencode-zen", "openai", "https://opencode.ai/zen/v1", "OPENCODE_API_KEY", "OpenCode Zen", "https://opencode.ai/auth"),
    p("otari", "openai", "https://api.otari.ai/api/v1", "OTARI_API_KEY", "Otari", "https://app.otari.ai/settings/api-keys"),
    p("qianfan", "openai", "https://qianfan.baidubce.com/v2", "QIANFAN_API_KEY", "Qianfan", "https://console.bce.baidu.com/qianfan/ais/console/apiKey"),
    p("qwen-portal", "openai", "https://portal.qwen.ai/v1", "QWEN_PORTAL_API_KEY", "Qwen Portal", "https://chat.qwen.ai"),
    p("sakana", "openai", "https://api.sakana.ai/v1", "SAKANA_API_KEY", "Sakana AI", "https://console.sakana.ai/api-keys"),
    p("sambanova", "openai", "https://api.sambanova.ai/v1", "SAMBANOVA_API_KEY", "SambaNova", "https://cloud.sambanova.ai/apis"),
    p("scaleway", "openai", "https://api.scaleway.ai/v1", "SCW_SECRET_KEY", "Scaleway", "https://console.scaleway.com/iam/api-keys"),
    p("siliconflow", "openai", "https://api.siliconflow.com/v1", "SILICONFLOW_API_KEY", "SiliconFlow", "https://cloud.siliconflow.com/account/ak"),
    p("siliconflow-cn", "openai", "https://api.siliconflow.cn/v1", "SILICONFLOW_CN_API_KEY", "SiliconFlow (中国)", "https://cloud.siliconflow.cn/account/ak"),
    p("singularityapi-dev", "openai", "https://api.singularityapi.dev/v1", "SINGULARITYAPI_DEV_API_KEY", "SingularityAPI", "https://app.singularityapi.dev"),
    p("singularityapi-tech", "openai", "https://api.singularityapi.tech/v1", "SINGULARITYAPI_TECH_API_KEY", "SingularityAPI Reserved Lanes", "https://app.singularityapi.tech/compute/billing"),
    p("stepfun", "openai", "https://api.stepfun.ai/v1", "STEPFUN_API_KEY", "StepFun", "https://platform.stepfun.ai/interface-key"),
    p("synthetic", "openai", "https://api.synthetic.new/openai/v1", "SYNTHETIC_API_KEY", "Synthetic", "https://synthetic.new/user-settings/api"),
    p("umans", "anthropic", "https://api.code.umans.ai", "UMANS_AI_CODING_PLAN_API_KEY", "Umans AI Coding Plan", "https://app.umans.ai/billing"),
    p("venice", "openai", "https://api.venice.ai/api/v1", "VENICE_API_KEY", "Venice", "https://venice.ai/settings/api"),
    p("vercel-ai-gateway", "openai", "https://ai-gateway.vercel.sh/v1", "AI_GATEWAY_API_KEY", "Vercel AI Gateway", "https://vercel.com/d?to=%2F%5Bteam%5D%2F%7E%2Fai-gateway%2Fapi-keys&title=AI+Gateway+API+Keys"),
    p("vllm", "openai", "http://127.0.0.1:8000/v1", "", "vLLM (ローカル)", ""),
    p("wafer-serverless", "openai", "https://pass.wafer.ai/v1", "WAFER_SERVERLESS_API_KEY", "Wafer Serverless", "https://app.wafer.ai/usage"),
    p("xiaomi", "openai", "https://api.xiaomimimo.com/v1", "XIAOMI_API_KEY", "Xiaomi MiMo", "https://platform.xiaomimimo.com/console/api-keys"),
    p("xiaomi-token-plan-ams", "openai", "https://token-plan-ams.xiaomimimo.com/v1", "XIAOMI_TOKEN_PLAN_AMS_API_KEY", "Xiaomi Token Plan (欧州)", "https://platform.xiaomimimo.com/console/plan-manage"),
    p("xiaomi-token-plan-cn", "openai", "https://token-plan-cn.xiaomimimo.com/v1", "XIAOMI_TOKEN_PLAN_CN_API_KEY", "Xiaomi Token Plan (中国)", "https://platform.xiaomimimo.com/console/plan-manage"),
    p("xiaomi-token-plan-sgp", "openai", "https://token-plan-sgp.xiaomimimo.com/v1", "XIAOMI_TOKEN_PLAN_SGP_API_KEY", "Xiaomi Token Plan (シンガポール)", "https://platform.xiaomimimo.com/console/plan-manage"),
    p("yolo-auto", "openai", "https://yolo-auto.com/v1", "YOLO_AUTO_API_KEY", "Yolo-Auto", "https://yolo-auto.com/app"),
    p("zai-coding-plan", "anthropic", "https://api.z.ai/api/anthropic", "ZAI_API_KEY", "Z.AI GLM Coding Plan", "https://z.ai/manage-apikey/apikey-list"),
    p("factory-droid", "factory", "https://api.factory.ai", "", "Factory Droid", ""),
    p("muse-code", "muse", "https://api.meta.ai/v1", "", "Muse Code (Meta サブスクリプション)", ""),
    // Keys come from AWS (env, ~/.aws profile) or an `api_key` Bedrock API key; no env var is required.
    p("amazon-bedrock", "bedrock", "https://bedrock-runtime.us-east-1.amazonaws.com", "", "Amazon Bedrock", "https://console.aws.amazon.com/bedrock/"),
    p("bedrock-mantle", "bedrock-mantle", "https://bedrock-mantle.{region}.api.aws/openai/v1", "", "Amazon Bedrock Mantle", "https://console.aws.amazon.com/bedrock/"),
    p("zenmux", "openai", "https://zenmux.ai/api/v1", "ZENMUX_API_KEY", "ZenMux", "https://zenmux.ai/settings/keys"),
    p("devin", "devin", "https://server.codeium.com", "DEVIN_API_KEY", "Devin", "https://app.devin.ai/"),
    p("devin-cli", "devin", "https://server.codeium.com", "", "Devin (CLI の認証情報を取り込む)", ""),
    p("devin-windsurf", "devin", "https://server.codeium.com", "", "Devin (Windsurf Enterprise)", ""),
    // <resource> is the Azure OpenAI resource name (AZURE_OPENAI_RESOURCE_NAME); api-version from AZURE_OPENAI_API_VERSION.
    p("azure", "azure", "https://<resource>.openai.azure.com/openai/v1", "AZURE_OPENAI_API_KEY", "Azure OpenAI (base_url の <resource> を置換)", "https://portal.azure.com/"),
    // <account>/<gateway> come from CLOUDFLARE_ACCOUNT_ID / CLOUDFLARE_GATEWAY_ID when left in place.
    p("cloudflare-ai-gateway", "cloudflare-ai-gateway", "https://gateway.ai.cloudflare.com/v1/<account>/<gateway>", "CLOUDFLARE_AI_GATEWAY_API_KEY", "Cloudflare AI Gateway", "https://developers.cloudflare.com/ai-gateway/configuration/authentication/"),
    // ADC from `gcloud auth application-default login`, GOOGLE_CLOUD_ACCESS_TOKEN, or GOOGLE_CLOUD_API_KEY (express mode).
    p("google-vertex", "vertex", "https://aiplatform.googleapis.com", "GOOGLE_CLOUD_API_KEY", "Google Vertex AI", "https://console.cloud.google.com/apis/credentials"),
    // SNOWFLAKE_PAT, or /login snowflake; the account comes from the sign-in or SNOWFLAKE_ACCOUNT.
    p("snowflake", "snowflake", "https://snowflake-account.invalid/api/v2/cortex", "SNOWFLAKE_PAT", "Snowflake Cortex", "https://docs.snowflake.com/en/user-guide/snowflake-cortex/cortex-rest-api"),
    // GITLAB_TOKEN (PAT), or /login gitlab-duo; GitLab mints the AI-gateway token per request.
    p("gitlab-duo", "gitlab-duo", "https://gitlab.com", "GITLAB_TOKEN", "GitLab Duo", "https://gitlab.com/-/user_settings/personal_access_tokens"),
    // The agent flows ride GitLab's Duo Workflow service, which needs its own OAuth app (VS Code extension).
    p("gitlab-duo-agent", "gitlab-duo-agent", "https://gitlab.com", "GITLAB_TOKEN", "GitLab Duo Agent", "https://gitlab.com/-/user_settings/personal_access_tokens"),
    // CURSOR_ACCESS_TOKEN, or /login cursor (the Cursor CLI's browser-poll sign-in).
    p("cursor", "cursor", "https://api2.cursor.sh", "CURSOR_ACCESS_TOKEN", "Cursor", ""),
    p("zhipu-coding-plan", "openai", "https://open.bigmodel.cn/api/coding/paas/v4", "ZHIPU_API_KEY", "Zhipu Coding Plan (智谱)", "https://bigmodel.cn/coding-plan/personal/overview"),
];

/// The built-in provider called `name`.
pub fn preset(name: &str) -> Option<&'static Preset> {
    PRESETS.iter().find(|p| p.name == name)
}

#[derive(Deserialize, Serialize, Clone, Debug)]
pub struct AcpCfg {
    pub name: String,
    pub command: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub protocol: Protocol,
}

/// Launch changes for one backend, from `[backend_overrides.NAME]`.
#[derive(Deserialize, Serialize, Clone, Debug, Default, PartialEq)]
#[serde(default)]
pub struct BackendOverride {
    /// Replaces the program (`command[0]`); `~` is expanded. The backend's own arguments and environment stay.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub executable: Option<String>,
    /// Appended after the backend's own arguments.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    /// Extra model ids offered for this backend.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub models: Vec<String>,
}

fn expand_home(path: &str) -> String {
    match path.strip_prefix('~') {
        Some("") => crate::user_home().to_string_lossy().into_owned(),
        Some(rest) if rest.starts_with(['/', '\\']) => crate::user_home().join(&rest[1..]).to_string_lossy().into_owned(),
        _ => path.to_string(),
    }
}

impl BackendOverride {
    fn apply(&self, b: &mut AcpCfg) {
        if let Some(exe) = self.executable.as_deref().map(str::trim).filter(|e| !e.is_empty()) {
            let exe = expand_home(exe);
            // A program that cannot be found stays as typed so the connection shows it as missing.
            let exe = executable(&exe).map(|p| p.to_string_lossy().into_owned()).unwrap_or(exe);
            match b.command.first_mut() {
                Some(first) => *first = exe,
                None => b.command.push(exe),
            }
        }
        b.command.extend(self.args.iter().cloned());
        b.env.extend(self.env.iter().map(|(k, v)| (k.clone(), v.clone())));
    }
}

/// Permission modes a backend can honor. T3 Code offers fx, Kiro and Muse only Supervised and Full access
/// (no auto-accept edits); plan is enforced by GreenCode itself.
pub fn backend_modes(backend: &str) -> &'static [Mode] {
    match backend {
        "fx" | "kiro" | "muse" => &[Mode::Ask, Mode::Full, Mode::Plan],
        _ => &[Mode::Ask, Mode::AutoEdit, Mode::Full, Mode::Plan],
    }
}

/// Sets one `[backend_overrides.NAME]` field in config.toml text, keeping comments and layout. `field` is `executable`,
/// `args`, `models` or `env` (values are `KEY=VALUE`); no values removes the field.
pub fn backend_override_text(text: &str, name: &str, field: &str, values: &[String]) -> anyhow::Result<String> {
    use anyhow::{anyhow, Context};
    use toml_edit::{value, Array, DocumentMut, Item, Table};
    anyhow::ensure!(!name.is_empty() && !name.contains(['"', '\n']), "invalid backend name");
    let mut doc: DocumentMut = text.parse().context("config.toml is not valid TOML")?;
    let root = doc.as_table_mut();
    if !root.contains_key("backend_overrides") {
        let mut t = Table::new();
        t.set_implicit(true);
        root.insert("backend_overrides", Item::Table(t));
    }
    let overrides = root.get_mut("backend_overrides").and_then(Item::as_table_mut).ok_or_else(|| anyhow!("`backend_overrides` is not a table in config.toml"))?;
    if !overrides.contains_key(name) { overrides.insert(name, Item::Table(Table::new())); }
    let entry = overrides.get_mut(name).and_then(Item::as_table_mut).ok_or_else(|| anyhow!("`backend_overrides.{name}` is not a table in config.toml"))?;
    if values.is_empty() {
        entry.remove(field);
    } else {
        match field {
            "executable" => { entry.insert(field, value(values[0].trim())); }
            "args" | "models" => { entry.insert(field, value(values.iter().cloned().collect::<Array>())); }
            "env" => {
                let mut env = Table::new();
                for line in values {
                    let (k, v) = line.split_once('=').ok_or_else(|| anyhow!("environment variables are written KEY=VALUE"))?;
                    anyhow::ensure!(!k.trim().is_empty() && !k.contains(char::is_whitespace), "invalid environment variable name `{}`", k.trim());
                    env.insert(k.trim(), value(v));
                }
                entry.insert(field, Item::Table(env));
            }
            _ => anyhow::bail!("unknown backend setting `{field}`"),
        }
    }
    let empty = entry.is_empty();
    if empty { overrides.remove(name); }
    if overrides.is_empty() { root.remove("backend_overrides"); }
    let out = doc.to_string();
    toml::from_str::<Config>(&out).map_err(|e| anyhow!("invalid backend setting: {}", e.message()))?;
    Ok(out)
}

fn read_config_text() -> anyhow::Result<String> {
    let path = crate::home().join("config.toml");
    match std::fs::read_to_string(&path) {
        Ok(s) => Ok(s),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(e) => anyhow::bail!("{}: {e}", path.display()),
    }
}

/// Writes one `[backend_overrides.NAME]` field to the user's config.toml.
pub fn set_backend_override(name: &str, field: &str, values: &[String]) -> anyhow::Result<()> {
    let out = backend_override_text(&read_config_text()?, name, field, values)?;
    settings::write_atomic(&crate::home().join("config.toml"), &out)
}

/// Sets `[providers.NAME] models` in config.toml text. A preset that is not written yet gets its kind and base URL,
/// because a table with only `models` would otherwise load as a provider with no API.
pub fn provider_models_text(text: &str, name: &str, models: &[String]) -> anyhow::Result<String> {
    use anyhow::{anyhow, Context};
    use toml_edit::{value, Array, DocumentMut, Item, Table};
    anyhow::ensure!(!name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'), "invalid provider name");
    let mut doc: DocumentMut = text.parse().context("config.toml is not valid TOML")?;
    let root = doc.as_table_mut();
    if !root.contains_key("providers") {
        let mut t = Table::new();
        t.set_implicit(true);
        root.insert("providers", Item::Table(t));
    }
    let providers = root.get_mut("providers").and_then(Item::as_table_mut).ok_or_else(|| anyhow!("`providers` is not a table in config.toml"))?;
    if !providers.contains_key(name) {
        let Some(&Preset { kind, url, env, .. }) = preset(name) else {
            anyhow::bail!("プロバイダ `{name}` は config.toml にないため、モデルだけを追加できません");
        };
        let mut t = Table::new();
        t.insert("kind", value(kind));
        t.insert("base_url", value(url));
        if !env.is_empty() { t.insert("api_key_env", value(env)); }
        providers.insert(name, Item::Table(t));
    }
    let entry = providers.get_mut(name).and_then(Item::as_table_mut).ok_or_else(|| anyhow!("`providers.{name}` is not a table in config.toml"))?;
    if models.is_empty() {
        entry.remove("models");
    } else {
        entry.insert("models", value(models.iter().cloned().collect::<Array>()));
    }
    let out = doc.to_string();
    let cfg: Config = toml::from_str(&out).map_err(|e| anyhow!("invalid provider models: {}", e.message()))?;
    anyhow::ensure!(cfg.providers.get(name).is_some_and(|p| !p.kind.is_empty() && !p.base_url.is_empty()), "プロバイダ `{name}` の設定が不完全です");
    Ok(out)
}

/// Writes the custom model list of one provider to the user's config.toml.
pub fn set_provider_models(name: &str, models: &[String]) -> anyhow::Result<()> {
    let out = provider_models_text(&read_config_text()?, name, models)?;
    settings::write_atomic(&crate::home().join("config.toml"), &out)
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
            secret_providers: vec![],
            backend_overrides: BTreeMap::new(),
            sandbox: false,
            advisor_model: None,
            learning: LearningCfg::default(),
            update_repo: None,
            prices: BTreeMap::new(),
            policy: None,
            backends: vec![],
            tui: TuiCfg::default(),
            task: Default::default(),
            worktree: Default::default(),
            provider_opts: Default::default(),
            loop_opts: Default::default(),
            bash_opts: Default::default(),
            file_opts: Default::default(),
            integration_opts: Default::default(),
            interaction_opts: Default::default(),
            collab_opts: Default::default(),
            compaction_opts: Default::default(),
            context_opts: Default::default(),
            assist_opts: Default::default(),
            disasm_opts: Default::default(),
            extra_tools_opts: Default::default(),
            memory_opts: Default::default(),
            services_opts: Default::default(),
            services_extra_opts: Default::default(),
            ttsr_opts: Default::default(),
            modes: Default::default(),
            isolation_opts: Default::default(),
            discovery: Default::default(),
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
        crate::secrets::set_providers(&cfg.secret_providers);
        // `modelRoleStorage = project`: a project config's roles win over the global ones.
        for (role, spec) in roles::project_roles(&std::env::current_dir().unwrap_or_default()) {
            cfg.model_roles.insert(role, spec);
        }
        cfg.policy = crate::policy::load()?;
        cfg.collab_opts.validate()?;
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
        for &Preset { name, kind, url, env, .. } in PRESETS {
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
        // Launch changes and extra models for a backend, whether configured, auto-detected or installed.
        cfg.backend_overrides.remove(crate::registry::GEMINI_CLI_ID);
        for (name, over) in &cfg.backend_overrides {
            match cfg.backends.iter_mut().find(|b| &b.name == name) {
                Some(b) => over.apply(b),
                None => {
                    let mut b = AcpCfg { name: name.clone(), command: vec![], env: Default::default(), protocol: Protocol::Acp };
                    over.apply(&mut b);
                    if !b.command.is_empty() { cfg.backends.push(b); }
                }
            }
        }
        // Old registry installs and configuration must not bring back the retired backend.
        cfg.backends.retain(|b| b.name != crate::registry::GEMINI_CLI_ID);
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

    /// Models for the picker: the current model, then `models`, then each provider's `models`, filtered and
    /// ordered by `provider_opts`. The current model stays first even when a filter hides it.
    pub fn model_list(&self) -> Vec<String> {
        let opts = &self.provider_opts;
        let mut v = vec![self.model.clone()];
        let mut providers: Vec<(&String, &ProviderCfg)> = self.providers.iter().filter(|(name, _)| opts.provider_allowed(name)).collect();
        // A stable sort keeps the map's alphabetical order for providers the order does not name.
        providers.sort_by_key(|(name, _)| opts.provider_rank(name));
        let provided = providers.into_iter().flat_map(|(name, p)| p.models.iter().map(move |m| format!("{name}/{m}")));
        for m in self.models.iter().cloned().chain(provided) {
            if !v.contains(&m) && opts.model_allowed(&m) {
                v.push(m);
            }
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
        if let Some(&Preset { kind, url, env, .. }) = preset(name).filter(|_| p.base_url.is_empty()) {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_models_create_preset_table_and_keep_other_fields() {
        let out = provider_models_text("", "openai", &["gpt-x".into()]).unwrap();
        let cfg: Config = toml::from_str(&out).unwrap();
        assert_eq!(cfg.providers["openai"].kind, "openai");
        assert_eq!(cfg.providers["openai"].models, ["gpt-x"]);
        let cleared = provider_models_text(&out, "openai", &[]).unwrap();
        assert!(!cleared.contains("models ="));
        assert!(provider_models_text(&cleared, "nope", &["m".into()]).is_err());
        // Comments and the provider's other settings survive an edit.
        let text = "[providers.mine]\nkind = \"openai\"\nbase_url = \"http://x\"\nmodels = [\"a\"]\n# keep\n";
        let out = provider_models_text(text, "mine", &["b".into()]).unwrap();
        let cfg: Config = toml::from_str(&out).unwrap();
        assert_eq!((cfg.providers["mine"].base_url.as_str(), cfg.providers["mine"].models.as_slice()), ("http://x", &["b".to_string()][..]));
        assert!(out.contains("# keep"));
    }

    #[test]
    fn backend_modes_follow_what_each_agent_supports() {
        assert!(!backend_modes("fx").contains(&Mode::AutoEdit));
        assert!(!backend_modes("kiro").contains(&Mode::AutoEdit));
        assert_eq!(backend_modes("kiro"), [Mode::Ask, Mode::Full, Mode::Plan]);
        // Pi's Auto-accept edits map to AutoEdit, so every GreenCode mode is offered.
        assert_eq!(backend_modes("pi"), [Mode::Ask, Mode::AutoEdit, Mode::Full, Mode::Plan]);
    }

    #[test]
    fn backend_override_expands_home_and_keeps_backend_args() {
        let mut b = AcpCfg { name: "kiro".into(), command: vec!["kiro-cli".into(), "acp".into()], env: Default::default(), protocol: Protocol::Acp };
        BackendOverride {
            executable: Some("~/no-such-green-code-bin".into()),
            args: vec!["--x".into()],
            env: [("K".to_string(), "v".to_string())].into(),
            models: vec!["m".into()],
        }
        .apply(&mut b);
        assert!(b.command[0].starts_with(&*crate::user_home().to_string_lossy()));
        assert_eq!(b.command[1..], ["acp", "--x"]);
        assert_eq!(b.env["K"], "v");
        assert_eq!(b.command.len(), 3);
    }

    #[test]
    fn backend_override_text_round_trips_and_clears() {
        let text = backend_override_text("", "kiro", "executable", &["~/bin/kiro".into()]).unwrap();
        let cfg: Config = toml::from_str(&text).unwrap();
        assert_eq!(cfg.backend_overrides["kiro"].executable.as_deref(), Some("~/bin/kiro"));
        let text = backend_override_text(&text, "kiro", "env", &["A=1".into(), "B=two=2".into()]).unwrap();
        let cfg: Config = toml::from_str(&text).unwrap();
        assert_eq!(cfg.backend_overrides["kiro"].env["A"], "1");
        assert_eq!(cfg.backend_overrides["kiro"].env["B"], "two=2");
        let text = backend_override_text(&text, "kiro", "args", &["--x".into(), "--y".into()]).unwrap();
        let cfg: Config = toml::from_str(&text).unwrap();
        assert_eq!(cfg.backend_overrides["kiro"].args, ["--x", "--y"]);
        let mut text = backend_override_text(&text, "kiro", "args", &[]).unwrap();
        text = backend_override_text(&text, "kiro", "env", &[]).unwrap();
        text = backend_override_text(&text, "kiro", "executable", &[]).unwrap();
        assert!(!text.contains("backend_overrides"));
    }
}
