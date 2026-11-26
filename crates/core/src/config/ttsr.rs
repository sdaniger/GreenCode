//! Time-Traveling Stream Rules settings: `ttsr.*`; flattened into `Config`.
use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Deserialize, Serialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct TtsrOpts {
    pub ttsr: TtsrCfg,
}

#[derive(Deserialize, Serialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct TtsrCfg {
    /// Match rule patterns while the reply streams; `rules_enabled` ([`crate::config::Config`]) stays the CLI-wide off switch.
    pub enabled: bool,
    /// `auto` judges only when a `judge` model role is configured; `on` falls back to this thread's model.
    pub judge: Judge,
    /// `discard` drops the partial reply a rule interrupted; `keep` commits it before the warning.
    pub context_mode: ContextMode,
    /// Which streams a match may interrupt mid-flight; everything else warns after the reply completes.
    pub interrupt_mode: InterruptMode,
    /// `once` fires each rule once per session; `after-gap` waits `repeat_gap` turns.
    pub repeat_mode: RepeatMode,
    pub repeat_gap: u32,
    /// Load the rules shipped with GreenCode; `disabled_rules` turns any of them off by name.
    pub builtin_rules: bool,
    pub disabled_rules: Vec<String>,
}

#[derive(Deserialize, Serialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Judge {
    #[default]
    Auto,
    On,
    Off,
}

#[derive(Deserialize, Serialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ContextMode {
    #[default]
    Discard,
    Keep,
}

#[derive(Deserialize, Serialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum InterruptMode {
    Never,
    ProseOnly,
    ToolOnly,
    #[default]
    Always,
}

impl InterruptMode {
    /// The spelling rules use in `interruptMode:` frontmatter.
    pub fn parse(text: &str) -> Option<InterruptMode> {
        match text.trim() {
            "never" => Some(InterruptMode::Never),
            "prose-only" => Some(InterruptMode::ProseOnly),
            "tool-only" => Some(InterruptMode::ToolOnly),
            "always" => Some(InterruptMode::Always),
            _ => None,
        }
    }
}

#[derive(Deserialize, Serialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum RepeatMode {
    #[default]
    Once,
    AfterGap,
}

impl Default for TtsrOpts {
    fn default() -> Self {
        Self { ttsr: TtsrCfg::default() }
    }
}

impl Default for TtsrCfg {
    fn default() -> Self {
        Self {
            enabled: true,
            judge: Judge::Auto,
            context_mode: ContextMode::Discard,
            interrupt_mode: InterruptMode::Always,
            repeat_mode: RepeatMode::Once,
            repeat_gap: 10,
            builtin_rules: true,
            disabled_rules: Vec::new(),
        }
    }
}

/// OMP key -> native dotted key; `config import-omp` copies the value verbatim unless `convert` handles it.
pub(super) const OMP: &[(&str, &str)] = &[
    ("ttsr.enabled", "ttsr.enabled"),
    ("ttsr.judge", "ttsr.judge"),
    ("ttsr.contextMode", "ttsr.context_mode"),
    ("ttsr.interruptMode", "ttsr.interrupt_mode"),
    ("ttsr.repeatMode", "ttsr.repeat_mode"),
    ("ttsr.repeatGap", "ttsr.repeat_gap"),
    ("ttsr.builtinRules", "ttsr.builtin_rules"),
    ("ttsr.disabledRules", "ttsr.disabled_rules"),
];

/// Translates an OMP value whose shape or units differ; `None` copies it verbatim.
pub(super) fn convert(_from: &str, _value: &Value) -> Option<Result<toml::Value>> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_omp_and_import_keeps_enum_spellings() {
        let cfg = crate::config::Config::default();
        let t = &cfg.ttsr_opts.ttsr;
        assert!(t.enabled && t.builtin_rules && t.disabled_rules.is_empty());
        assert_eq!((t.judge, t.context_mode), (Judge::Auto, ContextMode::Discard));
        assert_eq!((t.interrupt_mode, t.repeat_mode, t.repeat_gap), (InterruptMode::Always, RepeatMode::Once, 10));
        let tree = serde_json::to_value(&cfg).unwrap();
        assert_eq!(tree["ttsr"]["interrupt_mode"], "always");
        assert_eq!(tree["ttsr"]["context_mode"], "discard");
        let table = crate::config::settings::translate_omp(
            "ttsr:\n  judge: off\n  contextMode: keep\n  interruptMode: tool-only\n  repeatMode: after-gap\n  repeatGap: 15\n  builtinRules: false\n  disabledRules: [no-sycophancy]\n",
        )
        .unwrap();
        let cfg = crate::config::settings::decode(toml::Value::Table(table)).unwrap().ttsr_opts.ttsr;
        assert_eq!(cfg.judge, Judge::Off);
        assert_eq!(cfg.context_mode, ContextMode::Keep);
        assert_eq!(cfg.interrupt_mode, InterruptMode::ToolOnly);
        assert_eq!((cfg.repeat_mode, cfg.repeat_gap), (RepeatMode::AfterGap, 15));
        assert!(!cfg.builtin_rules && cfg.disabled_rules == ["no-sycophancy"]);
        assert_eq!(InterruptMode::parse("prose-only"), Some(InterruptMode::ProseOnly));
    }
}
