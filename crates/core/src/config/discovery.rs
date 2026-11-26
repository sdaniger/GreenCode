//! Discovery surface: bash-mounted tool commands, extension handler/renderer limits, Auto QA and
//! the Spotlight experiment. Flattened into `Config`.
use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Which mounted commands get their `--help` text inlined in the system prompt.
#[derive(Deserialize, Serialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum CommandDocs {
    /// Every mounted command, schema included.
    Inline,
    /// Core tools stay inline; dynamic commands stay on demand unless `command_inline` matches.
    #[default]
    Builtins,
    /// Names only, no help text.
    Catalog,
}

impl CommandDocs {
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "inline" => Some(Self::Inline),
            "builtins" => Some(Self::Builtins),
            "catalog" => Some(Self::Catalog),
            _ => None,
        }
    }
}

#[derive(Deserialize, Serialize, Clone, Debug, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct ToolCommandsCfg {
    /// Mount discoverable tools as bash commands with `--help` instead of top-level schemas.
    pub enabled: bool,
    pub docs: CommandDocs,
    /// Glob patterns for dynamic commands to inline when `docs` is `builtins`.
    pub inline: Vec<String>,
    /// Show a notice when commands are mounted or unmounted.
    pub notices: bool,
}

impl Default for ToolCommandsCfg {
    fn default() -> Self {
        Self { enabled: true, docs: CommandDocs::Builtins, inline: Vec::new(), notices: true }
    }
}

#[derive(Deserialize, Serialize, Clone, Debug, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct ExtensionHandlersCfg {
    /// Active-work timeout for Lua `tool_call` hooks; 0 or non-finite values fall back to 30000.
    pub tool_call_timeout_ms: u64,
    /// Let a `tool_call` hook switch plan/vibe/full mode through `mode` in its reply.
    pub mode_control: bool,
}

impl Default for ExtensionHandlersCfg {
    fn default() -> Self {
        Self { tool_call_timeout_ms: 30_000, mode_control: true }
    }
}

impl ExtensionHandlersCfg {
    pub fn timeout(&self) -> std::time::Duration {
        std::time::Duration::from_millis(self.tool_call_timeout_ms.max(1).min(3_600_000))
    }
}

#[derive(Deserialize, Serialize, Clone, Debug, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct ExtensionRenderersCfg {
    /// Let a `render_assistant` hook redraw assistant replies.
    pub assistant_text: bool,
    /// Let a `present_transcript` hook draw tool blocks.
    pub transcript_presenters: bool,
}

impl Default for ExtensionRenderersCfg {
    fn default() -> Self {
        Self { assistant_text: true, transcript_presenters: true }
    }
}

#[derive(Deserialize, Serialize, Clone, Debug, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct AutoqaPushCfg {
    /// Full URL receiving Auto QA JSON reports; empty keeps reports local.
    pub endpoint: String,
}

impl Default for AutoqaPushCfg {
    fn default() -> Self {
        Self { endpoint: String::new() }
    }
}

#[derive(Deserialize, Serialize, Clone, Debug, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct DevCfg {
    /// `report_issue` reporting; the first report asks for consent.
    pub autoqa: bool,
    pub autoqa_push: AutoqaPushCfg,
}

impl Default for DevCfg {
    fn default() -> Self {
        Self { autoqa: true, autoqa_push: AutoqaPushCfg::default() }
    }
}

#[derive(Deserialize, Serialize, Clone, Debug, Default, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct SpotlightCfg {
    pub experiment: SpotlightExperiment,
}

#[derive(Deserialize, Serialize, Clone, Debug, Default, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct SpotlightExperiment {
    pub enabled: bool,
}

#[derive(Deserialize, Serialize, Clone, Debug, Default, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct DiscoveryOpts {
    pub tool_commands: ToolCommandsCfg,
    pub extension_handlers: ExtensionHandlersCfg,
    pub extension_renderers: ExtensionRenderersCfg,
    pub dev: DevCfg,
    pub spotlight: SpotlightCfg,
}

/// OMP key → native dotted key.
pub(super) const OMP: &[(&str, &str)] = &[
    ("tools.commands", "tool_commands.enabled"),
    ("tools.commandDocs", "tool_commands.docs"),
    ("tools.commandInline", "tool_commands.inline"),
    ("tools.commandNotices", "tool_commands.notices"),
    ("extensionHandlers.toolCallTimeoutMs", "extension_handlers.tool_call_timeout_ms"),
    ("extensionHandlers.modeControl", "extension_handlers.mode_control"),
    ("extensionRenderers.assistantText", "extension_renderers.assistant_text"),
    ("extensionRenderers.transcriptPresenters", "extension_renderers.transcript_presenters"),
    ("dev.autoqa", "dev.autoqa"),
    ("dev.autoqaPush.endpoint", "dev.autoqa_push.endpoint"),
    ("spotlight.experiment.enabled", "spotlight.experiment.enabled"),
];

pub(super) fn convert(from: &str, value: &Value) -> Option<Result<toml::Value>> {
    match from {
        // `CommandDocs` already only accepts these spellings; the import keeps the same contract.
        "tools.commandDocs" => Some(match value.as_str().and_then(CommandDocs::parse) {
            Some(docs) => Ok(toml::Value::String(docs.as_str().into())),
            None => Err(anyhow!("tools.commandDocs must be inline, builtins or catalog")),
        }),
        _ => None,
    }
}

impl CommandDocs {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Inline => "inline",
            Self::Builtins => "builtins",
            Self::Catalog => "catalog",
        }
    }
}

/// Validates an imported enum spelling; used by `settings::decode` through serde already, so this
/// only exists for `--docs`-style command-line input.
pub fn docs_from_cli(text: &str) -> Result<CommandDocs> {
    CommandDocs::parse(text).ok_or_else(|| anyhow!("docs must be inline, builtins or catalog"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_and_omp_rows_import() {
        let cfg = crate::config::Config::default();
        let d = &cfg.discovery;
        assert!(d.tool_commands.enabled);
        assert_eq!(d.tool_commands.docs, CommandDocs::Builtins);
        assert_eq!(d.extension_handlers.tool_call_timeout_ms, 30_000);
        assert!(d.extension_handlers.mode_control && d.dev.autoqa);
        assert!(!d.spotlight.experiment.enabled);
        let tree = serde_json::to_value(&cfg).unwrap();
        assert_eq!(tree["tool_commands"]["docs"], "builtins");
        assert_eq!(tree["dev"]["autoqa_push"]["endpoint"], "");
        let table = crate::config::settings::translate(
            "tools:\n  commands: false\n  commandDocs: catalog\n  commandInline: ['mcp__context_mode_*']\n  commandNotices: false\nextensionHandlers:\n  toolCallTimeoutMs: 5000\n  modeControl: false\nextensionRenderers:\n  assistantText: false\n  transcriptPresenters: false\ndev:\n  autoqa: false\n  autoqaPush:\n    endpoint: http://127.0.0.1:9/v1\nspotlight:\n  experiment:\n    enabled: true\n",
        )
        .unwrap();
        let cfg = crate::config::settings::decode(toml::Value::Table(table)).unwrap();
        let d = &cfg.discovery;
        assert!(!d.tool_commands.enabled && !d.tool_commands.notices);
        assert_eq!(d.tool_commands.docs, CommandDocs::Catalog);
        assert_eq!(d.tool_commands.inline, ["mcp__context_mode_*"]);
        assert_eq!(d.extension_handlers.timeout().as_millis(), 5000);
        assert!(!d.extension_handlers.mode_control && !d.extension_renderers.assistant_text);
        assert!(!d.dev.autoqa && d.dev.autoqa_push.endpoint == "http://127.0.0.1:9/v1");
        assert!(d.spotlight.experiment.enabled);
        assert!(crate::config::settings::translate("tools:\n  commandDocs: sometimes\n").is_err());
    }
}
