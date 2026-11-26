use anyhow::{anyhow, ensure, Context, Result};
use serde_json::Value;
use std::path::PathBuf;

const SETTINGS: &[(&str, &str)] = &[
    ("autoResume", "tui.auto_resume"),
    ("autocompleteMaxVisible", "tui.autocomplete_max_visible"),
    ("hideThinkingBlock", "tui.hide_thinking"),
    ("display.hideToolActivity", "tui.hide_tools"),
    ("display.showTokenUsage", "tui.show_turn_usage"),
    ("display.showTurnTime", "tui.show_turn_time"),
    ("composer.recallClearedDrafts", "tui.recall_cleared_drafts"),
    ("tui.tight", "tui.tight"),
    ("terminal.showProgress", "tui.show_terminal_progress"),
    ("contextFiles.extra", "context_files_extra"),
    ("compaction.enabled", "compaction_enabled"),
    ("compaction.midTurnEnabled", "compaction_mid_turn"),
    ("compaction.thresholdTokens", "compact_at"),
    ("compaction.modelThresholds", "model_compact_at"),
    ("compaction.modelThresholdsEnabled", "model_compact_at_enabled"),
    ("skills.enabled", "skills_enabled"),
    ("mcp.enableProjectConfig", "mcp_project_config"),
    ("async.enabled", "task.background"),
    ("task.maxConcurrency", "task.max_concurrency"),
    ("task.maxRecursionDepth", "task.max_recursion_depth"),
    ("task.maxRuntimeMs", "task.max_runtime_ms"),
    ("task.disabledAgents", "task.disabled_agents"),
    ("task.structured", "task.structured"),
    ("task.isolation.enabled", "task.isolation.enabled"),
    ("task.perCallModel", "task.per_call_model"),
    ("task.perCallModelAllow", "task.per_call_model_allow"),
    ("task.agentModelOverrides", "task.agent_model_overrides"),
    ("title.refreshOnReplan", "tui.title.refresh_on_replan"),
    ("task.steps", "task.steps"),
    ("task.speculativeLaunch", "task.speculative_launch"),
    ("task.workerThreads", "task.worker_threads"),
    ("task.crossInstance", "task.cross_instance"),
    ("task.completionProbe", "task.completion_probe"),
    ("task.agentIdleTtlMs", "task.agent_idle_ttl_ms"),
];

/// OMP TUI preferences. Everything lands under `[tui]` so `/settings` can edit it.
const TUI_SETTINGS: &[(&str, &str)] = &[
    ("cycleOrder", "tui.cycle_order"),
    ("git.enabled", "tui.git.enabled"),
    ("theme.dark", "tui.theme_dark"),
    ("theme.light", "tui.theme_light"),
    ("symbolPreset", "tui.symbol_preset"),
    ("colorBlindMode", "tui.color_blind_mode"),
    ("composer.shape", "tui.composer.shape"),
    ("composer.tokenRate", "tui.composer.token_rate"),
    ("composer.predictions", "tui.composer.predictions"),
    ("composer.predictionAlternatives", "tui.composer.prediction_alternatives"),
    ("terminal.programStatus", "tui.terminal.program_status"),
    ("terminal.showImages", "tui.terminal.show_images"),
    ("tui.colorfulMarkdown", "tui.colorful_markdown"),
    ("tui.vimMode", "tui.vim_mode"),
    ("tui.vimModeDisplay", "tui.vim_mode_display"),
    ("treeFilterMode", "tui.tree_filter_mode"),
    ("pty.notify", "tui.notify.console"),
    ("tui.titleState", "tui.title_state"),
    ("tui.titleSpinner", "tui.title_spinner"),
    ("tui.spinnerInterval", "tui.spinner_interval"),
    ("tui.maxFps", "tui.max_fps"),
    ("tui.motion", "tui.motion"),
    ("tui.imeSafeCursor", "tui.ime_safe_cursor"),
    ("display.smoothStreaming", "tui.display.smooth_streaming"),
    ("display.cacheMissMarker", "tui.display.cache_miss_marker"),
    ("tui.resizeScrollback", "tui.display.resize_scrollback"),
    ("tui.textSizing", "tui.display.text_sizing"),
    ("tui.renderMermaid", "tui.display.render_mermaid"),
    ("tui.renderSvg", "tui.display.render_svg"),
    ("tui.renderEmbeds", "tui.display.render_embeds"),
    ("tui.autoGraph", "tui.display.auto_graph"),
    ("tui.reactions", "tui.display.reactions"),
    ("tui.codexResetFireworks", "tui.display.codex_reset_fireworks"),
    ("tui.hyperlinks", "tui.display.hyperlinks"),
    ("tui.mouse", "tui.display.mouse"),
    ("display.shimmer", "tui.display.shimmer"),
    ("display.pinnedAgents", "tui.display.pinned_agents"),
    ("display.subagentLivePreview", "tui.display.subagent_live_preview"),
    ("display.shellVisualization", "tui.display.shell_visualization"),
    ("display.advisorCollapsedLines", "tui.display.advisor_collapsed_lines"),
    ("display.collapseCompacted", "tui.display.collapse_compacted"),
    ("title.icons", "tui.title.icons"),
    ("statusLine.preset", "tui.status_line.preset"),
    ("statusLine.separator", "tui.status_line.separator"),
    ("statusLine.contextLine", "tui.status_line.context_line"),
    ("statusLine.sessionAccent", "tui.status_line.session_accent"),
    ("statusLine.transparent", "tui.status_line.transparent"),
    ("statusLine.compactThinkingLevel", "tui.status_line.compact_thinking_level"),
    ("statusLine.showHookStatus", "tui.status_line.show_hook_status"),
    ("showHardwareCursor", "tui.show_hardware_cursor"),
    ("loop.mode", "tui.loop.mode"),
    ("loop.conditionTimeoutMs", "tui.loop.condition_timeout_ms"),
    ("doubleEscapeAction", "tui.double_escape_action"),
    ("input.bareExitOnEmptySession", "tui.input.bare_exit_on_empty_session"),
    ("input.bareSlashCommands", "tui.input.bare_slash_commands"),
    ("emojiAutocomplete", "tui.emoji_autocomplete"),
    ("paste.largeMenuThreshold", "tui.paste.large_menu_threshold"),
    ("startup.checkUpdate", "tui.update.check"),
    ("update.channel", "tui.update.channel"),
    ("update.auto", "tui.update.auto"),
    ("completion.notify", "tui.notify.completion"),
    ("error.notify", "tui.notify.error"),
    ("ask.timeout", "tui.ask.timeout"),
    ("ask.notify", "tui.ask.notify"),
    ("session.worktreeScope", "tui.session.worktree_scope"),
    ("expandThinkingBlocks", "tui.expand_thinking_blocks"),
    ("proseOnlyThinking", "tui.prose_only_thinking"),
    ("read.renderMarkdown", "tui.read.render_markdown"),
    ("read.toolResultPreview", "tui.read.tool_result_preview"),
    ("plan.autosave", "tui.plan.autosave"),
    ("plan.autosaveDir", "tui.plan.autosave_dir"),
    ("title.generator", "tui.title.generator"),
    ("title.refreshAfterPrompts", "tui.title.refresh_after_prompts"),
    ("skills.enableSkillCommands", "tui.skills.enable_skill_commands"),
    ("mcp.renderMarkdownResults", "tui.mcp.render_markdown_results"),
];

/// OMP values whose native spelling differs: `canary` is the native `main` channel, and extension
/// composer shapes fall back to the built-in band.
fn convert_tui(from: &str, value: &Value) -> Option<Result<toml::Value>> {
    match from {
        "update.channel" => Some(match value.as_str() {
            Some("stable") => Ok(toml::Value::String("stable".into())),
            Some("main" | "canary") => Ok(toml::Value::String("main".into())),
            _ => Err(anyhow!("update.channel must be stable or canary")),
        }),
        "composer.shape" => Some(Ok(toml::Value::String(match value.as_str() {
            Some(shape @ ("box" | "plain")) => shape.into(),
            Some(_) => "band".into(),
            None => return Some(Err(anyhow!("composer.shape must be a string"))),
        }))),
        _ => None,
    }
}

pub(super) fn profile(name: &str) -> Result<toml::Table> {
    let stem = name.split('.').next().unwrap_or_default();
    let reserved = matches!(stem.as_bytes(), b"con" | b"prn" | b"aux" | b"nul" | [b'c', b'o', b'm', b'0'..=b'9'] | [b'l', b'p', b't', b'0'..=b'9']);
    let alphanumeric = |c: u8| c.is_ascii_lowercase() || c.is_ascii_digit();
    ensure!(name.len() <= 64 && name.bytes().next().is_some_and(alphanumeric)
        && name.bytes().all(|c| alphanumeric(c) || matches!(c, b'.' | b'-' | b'_'))
        && !name.ends_with('.') && !reserved, "Invalid OMP profile name `{name}`");
    let omp_home = crate::user_home().join(".omp");
    let base = std::env::var_os("PI_CODING_AGENT_DIR").filter(|p| !p.is_empty())
        .map(PathBuf::from).unwrap_or_else(|| omp_home.join("agent"));
    let named = (name != "default").then(|| omp_home.join("profiles").join(name).join("agent"));
    let active = named.as_deref().unwrap_or(&base);
    ensure!(active.is_dir(), "OMP profile directory not found: {}", active.display());
    let mut out = toml::Table::new();
    for name in ["config.yml", "config.yaml"] {
        let path = active.join(name);
        match std::fs::read_to_string(&path) {
            Ok(text) => {
                out = translate(&text).with_context(|| format!("Cannot import {}", path.display()))?;
                break;
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e).with_context(|| format!("Cannot read {}", path.display())),
        }
    }
    let mut keymaps = Vec::new();
    for dir in [Some(base.as_path()), named.as_deref()].into_iter().flatten() {
        let path = dir.join("keybindings.yml");
        if path.try_exists()? { keymaps.push(std::path::absolute(path)?); }
    }
    if !keymaps.is_empty() {
        out.entry("tui").or_insert_with(|| toml::Value::Table(toml::Table::new()))
            .as_table_mut().unwrap().insert("keybindings".into(), toml::Value::try_from(keymaps)?);
    }
    Ok(out)
}

type Converter = fn(&str, &Value) -> Option<Result<toml::Value>>;

/// OMP tool switches with a native tool: `BUILTIN`-side names go through the `tools` allowlist,
/// `ext` ones through `ext_off`. AST search and edit are one native `ast` tool.
const TOOLS: &[(&str, &str)] = &[
    ("bash.enabled", "bash"),
    ("todo.enabled", "todo"),
    ("glob.enabled", "glob"),
    ("grep.enabled", "grep"),
    ("fetch.enabled", "web_fetch"),
    ("web_search.enabled", "web_search"),
    ("lsp.enabled", "lsp"),
    ("computer.enabled", "computer"),
    ("astGrep.enabled", "ast"),
    ("astEdit.enabled", "ast"),
    ("find.enabled", "find"),
    ("debug.enabled", "debug"),
    ("checkpoint.enabled", "checkpoint"),
    ("github.enabled", "github"),
    ("ask.enabled", "ask"),
    ("browser.enabled", "browser"),
    ("speechgen.enabled", "tts"),
];

/// Every OMP key with a native TOML key, and how its value converts.
fn pairs() -> impl Iterator<Item = ((&'static str, &'static str), Converter)> {
    let none: Converter = |_, _| None;
    let areas: Vec<(&'static [(&'static str, &'static str)], Converter)> = vec![
        (crate::config::provider::OMP, crate::config::provider::convert),
        (crate::config::advisor::OMP, crate::config::advisor::convert),
        (crate::config::agent_loop::OMP, crate::config::agent_loop::convert),
        (crate::config::bash::OMP, crate::config::bash::convert),
        (crate::config::files::OMP, crate::config::files::convert),
        (crate::config::integrations::OMP, crate::config::integrations::convert),
        (crate::config::interaction::OMP, crate::config::interaction::convert),
        (crate::config::browser::OMP, crate::config::browser::convert),
        (crate::config::compaction::OMP, crate::config::compaction::convert),
        (crate::config::context::OMP, crate::config::context::convert),
        (crate::config::memory::OMP, crate::config::memory::convert),
        (crate::config::mnemopi::OMP, crate::config::mnemopi::convert),
        (crate::config::disassembler::OMP, crate::config::disassembler::convert),
        (crate::config::extra_tools::OMP, none),
        (crate::config::services_extra::OMP, crate::config::services_extra::convert),
        (crate::config::ttsr::OMP, crate::config::ttsr::convert),
        (crate::config::collab::OMP, crate::config::collab::convert),
        (crate::config::services::OMP, crate::config::services::convert),
        (crate::subagents::OMP, crate::subagents::convert),
        (crate::worktree::OMP, none),
        (crate::config::modes::OMP, crate::config::modes::convert),
        (crate::config::isolation::OMP, crate::config::isolation::convert),
        (crate::config::discovery::OMP, crate::config::discovery::convert),
    ];
    SETTINGS.iter().map(move |&p| (p, none))
        .chain(TUI_SETTINGS.iter().map(|&p| (p, convert_tui as Converter)))
        .chain(areas.into_iter().flat_map(|(map, f)| map.iter().map(move |&p| (p, f))))
}

/// The value at an OMP key, written either dotted or nested.
fn lookup<'a>(source: &'a Value, from: &str) -> Result<Option<&'a Value>> {
    let nested = from.split('.').try_fold(source, |node, segment| node.get(segment));
    let flat = from.contains('.').then(|| source.get(from)).flatten();
    ensure!(flat.is_none() || nested.is_none(), "OMP setting {from} appears as both a dotted and nested key");
    Ok(flat.or(nested).filter(|v| !v.is_null()))
}

/// Where an OMP `/settings` entry lands natively.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Native {
    /// A `config.toml` key (as `config set` takes it).
    Key(&'static str),
    /// A native tool switched on or off.
    Tool(&'static str),
}

/// One entry of the settings catalog shown in the GUI; `group`, `label` and `description` are Japanese.
pub struct UiSetting {
    pub key: &'static str,
    pub tab: &'static str,
    pub group: &'static str,
    pub label: &'static str,
    /// The listed choices for enums and preset numbers; empty for free values.
    pub values: Vec<&'static str>,
    pub description: &'static str,
    pub native: Option<Native>,
}

/// OMP's `/settings` tabs in screen order.
pub const UI_TABS: [&str; 10] = ["appearance", "model", "interaction", "context", "memory", "files", "shell", "tools", "tasks", "providers"];

/// Every OMP 19.1.1 `/settings` entry in screen order (tab, then group), with its native target.
/// `omp_settings.tsv` is extracted from OMP's settings schema: key, tab, group, label, `|`-joined values, description.
/// `settings_ja.tsv` holds the Japanese group, label and description for the same rows in the same order.
pub fn ui_settings() -> Vec<UiSetting> {
    include_str!("omp_settings.tsv").lines().zip(include_str!("settings_ja.tsv").lines()).filter_map(|(line, ja)| {
        let [key, tab, _, _, values, _] = line.splitn(6, '\t').collect::<Vec<_>>().try_into().ok()?;
        let [ja_key, group, label, description] = ja.splitn(4, '\t').collect::<Vec<_>>().try_into().ok()?;
        debug_assert_eq!(key, ja_key);
        let native = TOOLS.iter().find(|(from, _)| *from == key).map(|&(_, tool)| Native::Tool(tool))
            .or_else(|| pairs().find(|((from, _), _)| *from == key).map(|((_, to), _)| Native::Key(to)));
        let values = values.split('|').filter(|v| !v.is_empty()).collect();
        Some(UiSetting { key, tab, group, label, values, description, native })
    }).collect()
}

pub(crate) fn translate(text: &str) -> Result<toml::Table> {
    let options = serde_saphyr::options! { strict_booleans: true };
    let source: Value = serde_saphyr::from_str_with_options(text, options).map_err(|_| anyhow!("Invalid OMP YAML"))?;
    if source.is_null() { return Ok(toml::Table::new()); }
    ensure!(source.is_object(), "OMP settings must be a YAML mapping");
    let mut out = toml::Table::new();
    for ((from, to), convert) in pairs() {
        let Some(value) = lookup(&source, from)? else { continue };
        let value = if let Some(converted) = convert(from, value) {
            converted.with_context(|| format!("Invalid OMP value for {from}"))?
        } else if from == "compaction.thresholdTokens" {
            let tokens = match value.as_i64() {
                Some(-1) => crate::config::DEFAULT_COMPACT_AT as i64,
                Some(n) if n > 0 => n,
                _ => anyhow::bail!("{from} must be a positive token count or -1 for the default"),
            };
            toml::Value::Integer(tokens)
        } else if from == "compaction.modelThresholds" {
            // shortcut: only fixed triggers are portable until the native provider registry tracks context windows.
            let mut thresholds = toml::Table::new();
            if !value.is_null() {
                for (selector, value) in value.as_object().context("compaction.modelThresholds must be a mapping")? {
                    let tokens = if value.is_null() { 0 } else {
                        value.as_str().map(str::trim).and_then(|s| s.strip_prefix(['f', 'F']))
                            .and_then(|s| s.parse::<i64>().ok()).filter(|&n| n > 0)
                            .with_context(|| format!("{from}.{selector}: only fixed triggers like f90000 can be imported"))?
                    };
                    thresholds.insert(selector.clone(), toml::Value::Integer(tokens));
                }
            }
            toml::Value::Table(thresholds)
        } else {
            toml::Value::try_from(value).map_err(|_| anyhow!("Invalid OMP value for {from}"))?
        };
        let mut table = &mut out;
        let mut segments = to.split('.').peekable();
        while let Some(key) = segments.next() {
            if segments.peek().is_none() {
                table.insert(key.into(), value);
                break;
            }
            table = table.entry(key).or_insert_with(|| toml::Value::Table(toml::Table::new())).as_table_mut().unwrap();
        }
    }
    let (mut tools, mut ext_off) = (Vec::new(), Vec::new());
    for &(from, tool) in TOOLS {
        match lookup(&source, from)? {
            None | Some(Value::Bool(true)) => {}
            Some(Value::Bool(false)) if crate::tools::ext::NAMES.contains(&tool) => {
                if !ext_off.contains(&tool) { ext_off.push(tool); }
            }
            Some(Value::Bool(false)) => tools = crate::tools::toggle_tool(&tools, tool, false),
            Some(_) => anyhow::bail!("{from} must be true or false"),
        }
    }
    if !tools.is_empty() { out.insert("tools".into(), toml::Value::try_from(tools)?); }
    if !ext_off.is_empty() { out.insert("ext_off".into(), toml::Value::try_from(ext_off)?); }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_only_compatible_explicit_settings() {
        let table = translate("autoResume: true\nhideThinkingBlock: true\nterminal.showProgress: true\ndisplay:\n  showTokenUsage: true\n  showTurnTime: true\ncontextFiles:\n  extra: [TEAM.md]\ntask:\n  maxConcurrency: 3\n  isolation:\n    enabled: false\n  perCallModel: off\n  perCallModelAllow: [openai/test]\n  agentModelOverrides:\n    scout: openai/test\ncompaction:\n  enabled: false\nauth:\n  token: private\n").unwrap();
        assert!(!table.contains_key("auth"));
        let cfg = super::super::decode(toml::Value::Table(table)).unwrap();
        assert!(cfg.tui.auto_resume && cfg.tui.show_terminal_progress && cfg.tui.hide_thinking);
        assert!(cfg.tui.show_turn_usage && cfg.tui.show_turn_time);
        assert_eq!(cfg.context_files_extra, ["TEAM.md"]);
        assert_eq!(cfg.task.max_concurrency, 3);
        assert!(!cfg.compaction_enabled && !cfg.task.isolation.enabled);
        assert_eq!(cfg.task.per_call_model, crate::subagents::PerCallModel::Off);
        assert_eq!(cfg.task.per_call_model_allow, ["openai/test"]);
        assert_eq!(cfg.task.agent_model_overrides["scout"], "openai/test");
        let decode = |text| super::super::decode(toml::Value::Table(translate(text)?));
        assert!(decode("task:\n  maxConcurrency: 0\n").is_err());
        assert!(decode("autoResume: maybe\n").is_err());
        assert!(!decode("composer:\n  recallClearedDrafts: false\n").unwrap().tui.recall_cleared_drafts);
        assert!(decode("contextFiles:\n  extra: [../TEAM.md]\n").is_err());
        let fixed = decode("compaction:\n  midTurnEnabled: false\n  thresholdTokens: 75000\n").unwrap();
        assert!(!fixed.compaction_mid_turn);
        assert_eq!(fixed.compact_at, 75_000);
        assert_eq!(decode("compaction.thresholdTokens: -1").unwrap().compact_at, crate::config::DEFAULT_COMPACT_AT);
        assert!(decode("compaction.thresholdTokens: -2").is_err());
        assert!(translate("terminal.showProgress: true\nterminal:\n  showProgress: false\n").is_err());
        assert!(translate("theme: dark\n").unwrap().is_empty());
        assert!(translate("null\n").unwrap().is_empty());
    }

    #[test]
    fn imports_every_tui_preference_and_maps_runtime_composer_shapes() {
        let yaml = "cycleOrder: [slow, default]\n\
git: {enabled: false}\n\
theme: {dark: titanium, light: light}\n\
symbolPreset: ascii\n\
colorBlindMode: true\n\
composer: {shape: bogus, tokenRate: true, predictions: true, predictionAlternatives: true}\n\
terminal: {programStatus: false}\n\
tui: {colorfulMarkdown: false, titleState: false, titleSpinner: line, spinnerInterval: 250, maxFps: 0, motion: reduced, imeSafeCursor: true}\n\
display: {smoothStreaming: false, cacheMissMarker: true}\n\
showHardwareCursor: false\n\
loop: {mode: compact, conditionTimeoutMs: 0}\n\
doubleEscapeAction: tree\n\
input: {bareExitOnEmptySession: false, bareSlashCommands: true}\n\
emojiAutocomplete: false\n\
paste: {largeMenuThreshold: 0}\n\
startup: {checkUpdate: false}\n\
update: {channel: canary, auto: true}\n\
completion: {notify: off}\n\
error: {notify: on}\n\
ask: {timeout: 30, notify: off}\n\
session: {worktreeScope: directory}\n\
expandThinkingBlocks: true\n\
proseOnlyThinking: false\n\
read: {renderMarkdown: true, toolResultPreview: true}\n\
plan: {autosave: true, autosaveDir: plans}\n\
title: {generator: tiny, refreshAfterPrompts: 5}\n\
skills: {enableSkillCommands: false}\n\
mcp: {renderMarkdownResults: false}\n";
        let translated = translate(yaml).unwrap();
        let cfg = super::super::decode(toml::Value::Table(translated.clone())).unwrap();
        let expected: &[(&str, toml::Value)] = &[
            ("cycle_order", toml::Value::Array(vec!["slow".into(), "default".into()])),
            ("git.enabled", false.into()),
            ("theme_dark", "titanium".into()),
            ("theme_light", "light".into()),
            ("symbol_preset", "ascii".into()),
            ("color_blind_mode", true.into()),
            ("composer.shape", "band".into()),
            ("composer.token_rate", true.into()),
            ("composer.predictions", true.into()),
            ("composer.prediction_alternatives", true.into()),
            ("terminal.program_status", false.into()),
            ("colorful_markdown", false.into()),
            ("title_state", false.into()),
            ("title_spinner", "line".into()),
            ("spinner_interval", 250.into()),
            ("max_fps", 0.into()),
            ("motion", "reduced".into()),
            ("ime_safe_cursor", true.into()),
            ("display.smooth_streaming", false.into()),
            ("display.cache_miss_marker", true.into()),
            ("show_hardware_cursor", false.into()),
            ("loop.mode", "compact".into()),
            ("loop.condition_timeout_ms", 0.into()),
            ("double_escape_action", "tree".into()),
            ("input.bare_exit_on_empty_session", false.into()),
            ("input.bare_slash_commands", true.into()),
            ("emoji_autocomplete", false.into()),
            ("paste.large_menu_threshold", 0.into()),
            ("update.check", false.into()),
            ("update.channel", "main".into()),
            ("update.auto", true.into()),
            ("notify.completion", "off".into()),
            ("notify.error", "on".into()),
            ("ask.timeout", 30.into()),
            ("ask.notify", "off".into()),
            ("session.worktree_scope", "directory".into()),
            ("expand_thinking_blocks", true.into()),
            ("prose_only_thinking", false.into()),
            ("read.render_markdown", true.into()),
            ("read.tool_result_preview", true.into()),
            ("plan.autosave", true.into()),
            ("plan.autosave_dir", "plans".into()),
            ("title.generator", "tiny".into()),
            ("title.refresh_after_prompts", 5.into()),
            ("skills.enable_skill_commands", false.into()),
            ("mcp.render_markdown_results", false.into()),
        ];
        let tui = translated.get("tui").and_then(toml::Value::as_table).unwrap();
        for (path, value) in expected {
            let mut node = tui;
            let mut segments = path.split('.').peekable();
            while let Some(key) = segments.next() {
                if segments.peek().is_none() {
                    assert_eq!(node.get(key), Some(value), "{path}");
                } else {
                    node = node.get(key).and_then(toml::Value::as_table).unwrap_or_else(|| panic!("missing table {key} for {path}"));
                }
            }
        }
        assert!(!cfg.tui.git.enabled && cfg.tui.color_blind_mode && !cfg.tui.emoji_autocomplete);
        assert_eq!(cfg.tui.symbol_preset, crate::config::SymbolPreset::Ascii);
        assert_eq!(cfg.tui.composer.shape, crate::config::ComposerShape::Band);
        assert_eq!(cfg.tui.update.channel, crate::update::Channel::Main);
        assert!(translate("composer: {shape: 3}\n").is_err());
    }

    #[test]
    fn tool_switches_import_into_the_allowlist_and_ext_off() {
        let cfg = super::super::decode(toml::Value::Table(
            translate("grep: {enabled: false}\nastEdit.enabled: false\nastGrep.enabled: false\ngithub: {enabled: true}\n").unwrap(),
        )).unwrap();
        assert!(!crate::tools::tool_on(&cfg.tools, "grep") && crate::tools::tool_on(&cfg.tools, "ext"));
        assert!(crate::tools::tool_on(&cfg.tools, "task") && crate::tools::tool_on(&cfg.tools, "mcp"));
        assert_eq!(cfg.ext_off, ["ast"]);
        assert!(translate("glob.enabled: true\n").unwrap().is_empty());
        assert!(translate("glob.enabled: maybe\n").is_err());
    }

    #[test]
    fn ui_catalog_targets_exist_natively() {
        let all = ui_settings();
        assert_eq!(all.len(), 470);
        assert!(all.iter().all(|s| UI_TABS.contains(&s.tab) && !s.label.is_empty()));
        let tree = serde_json::to_value(crate::config::Config::default()).unwrap();
        for s in &all {
            match s.native {
                // A map key (`model_compact_at`) exists as a table even when empty.
                Some(Native::Key(key)) => assert!(
                    key.split('.').try_fold(&tree, |node, seg| node.get(seg)).is_some(),
                    "{} -> {key}", s.key
                ),
                Some(Native::Tool(tool)) => assert!(
                    crate::tools::ext::NAMES.contains(&tool) || !crate::tools::toggle_tool(&[], tool, false).is_empty(),
                    "{} -> {tool}", s.key
                ),
                None => panic!("{} has no native setting", s.key),
            }
        }
        assert_eq!(all.iter().find(|s| s.key == "symbolPreset").unwrap().values, ["unicode", "nerd", "ascii"]);
    }

    #[test]
    fn imports_fixed_model_thresholds_and_entry_resets_without_reinterpreting_base_windows() {
        let yaml = "compaction:\n  modelThresholdsEnabled: false\n  modelThresholds:\n    openai/gpt-5*: f90000\n    openai/gpt-5.4: null\n";
        let cfg = super::super::decode(toml::Value::Table(translate(yaml).unwrap())).unwrap();
        assert!(!cfg.model_compact_at_enabled);
        assert_eq!(cfg.model_compact_at["openai/gpt-5*"], 90_000);
        assert_eq!(cfg.model_compact_at["openai/gpt-5.4"], 0);
        for unsupported in ["90000", "'80%'"] {
            assert!(translate(&format!("compaction.modelThresholds: {{ openai/gpt-5: {unsupported} }}")).is_err());
        }
        let edited = super::super::set_text("", "model_compact_at.openai/gpt-5.4", &["80000".into()]).unwrap();
        let cfg = super::super::decode(toml::from_str(&edited).unwrap()).unwrap();
        assert_eq!(cfg.model_compact_at["openai/gpt-5.4"], 80_000);
    }
}
