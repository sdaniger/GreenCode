//! Interaction settings: follow-up draining, approvals, spelling, dictation, idle recap,
//! magic keywords and startup; flattened into `Config`.
use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Deserialize, Serialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct InteractionOpts {
    /// How queued follow-ups drain once a turn completes.
    pub follow_up_mode: FollowUpDrain,
    /// Default tool approval behavior; a thread switched to `full` still bypasses it.
    pub approval_mode: ApprovalMode,
    pub btw: BtwCfg,
    pub spelling: SpellingCfg,
    pub stt: SttCfg,
    pub recap: RecapCfg,
    pub magic_keywords: MagicKeywordsCfg,
    pub startup: StartupCfg,
}

impl Default for InteractionOpts {
    fn default() -> Self {
        Self {
            follow_up_mode: FollowUpDrain::OneAtATime,
            approval_mode: ApprovalMode::AlwaysAsk,
            btw: BtwCfg::default(),
            spelling: SpellingCfg::default(),
            stt: SttCfg::default(),
            recap: RecapCfg::default(),
            magic_keywords: MagicKeywordsCfg::default(),
            startup: StartupCfg::default(),
        }
    }
}

/// `all` drains the whole queue into one turn; `one-at-a-time` starts one turn per message.
#[derive(Deserialize, Serialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum FollowUpDrain {
    All,
    #[default]
    OneAtATime,
}

#[derive(Deserialize, Serialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum ApprovalMode {
    /// Read-only tools run; every write and exec asks.
    /// Default here (OMP's is `write`): GreenCode threads start in Ask mode, which asks before writes.
    #[default]
    AlwaysAsk,
    /// Like `always-ask`, but a remembered file or folder stops the asking.
    AlwaysAskCached,
    /// Read and workspace-write tools run; exec asks.
    Write,
    /// Every tier runs unless a rule, folder or policy says otherwise.
    Yolo,
}

#[derive(Deserialize, Serialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct BtwCfg {
    /// Highlight a `/btw` answer's `<inject>` block and let Alt+I steer the main session with it.
    pub inject: bool,
}

impl Default for BtwCfg {
    fn default() -> Self {
        Self { inject: false }
    }
}

#[derive(Deserialize, Serialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct SpellingCfg {
    /// Mark misspelled prompt words with the active macOS dictionaries.
    pub typo_detection: bool,
    pub autocomplete: WordCompletion,
    /// Apply confident macOS corrections once a word is completed.
    pub autocorrect: bool,
}

impl Default for SpellingCfg {
    fn default() -> Self {
        Self { typo_detection: true, autocomplete: WordCompletion::Auto, autocorrect: false }
    }
}

/// Ghost-text completion engine; `off` disables it, `auto` picks what is installed.
#[derive(Deserialize, Serialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum WordCompletion {
    Off,
    #[default]
    Auto,
    Ngram,
    Smollm,
    Apple,
}

#[derive(Deserialize, Serialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct SttCfg {
    /// Microphone dictation in the composer.
    pub enabled: bool,
    /// When a finished dictation submits instead of staying in the editor.
    pub submit_trigger: SubmitTrigger,
}

impl Default for SttCfg {
    fn default() -> Self {
        Self { enabled: false, submit_trigger: SubmitTrigger::Never }
    }
}

#[derive(Deserialize, Serialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum SubmitTrigger {
    #[default]
    Never,
    /// Release after two or more words.
    Release,
    /// Release once the utterance ends in sentence punctuation.
    ReleaseComplete,
    /// The word "submit" ends the utterance; it and anything spoken after it are stripped.
    SaySubmit,
}

impl SubmitTrigger {
    /// Whether a transcribed utterance submits, and how many trailing bytes to strip first.
    pub fn evaluate(self, utterance: &str) -> (bool, usize) {
        let trimmed = utterance.trim();
        if trimmed.is_empty() || self == SubmitTrigger::Never {
            return (false, 0);
        }
        match self {
            SubmitTrigger::Never => (false, 0),
            SubmitTrigger::Release => (trimmed.split_whitespace().count() >= 2, 0),
            SubmitTrigger::ReleaseComplete => {
                (trimmed.ends_with(['.', '?', '!', '…', '。', '？', '！']), 0)
            }
            SubmitTrigger::SaySubmit => {
                // Byte offset of the last standalone "submit" (trailing punctuation allowed) in `utterance`.
                let hit = utterance
                    .split_whitespace()
                    .filter(|word| {
                        word.trim_end_matches(['.', '?', '!', '…', '。', '？', '！', ',']).eq_ignore_ascii_case("submit")
                    })
                    .last()
                    .map(|word| word.as_ptr() as usize - utterance.as_ptr() as usize);
                hit.map_or((false, 0), |start| (true, utterance.len() - start))
            }
        }
    }
}

#[derive(Deserialize, Serialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct RecapCfg {
    /// One short recap of where things stand once the session sits idle.
    pub enabled: bool,
    /// Idle seconds before the recap is shown.
    pub idle_seconds: u64,
}

impl Default for RecapCfg {
    fn default() -> Self {
        Self { enabled: true, idle_seconds: 240 }
    }
}

impl RecapCfg {
    /// A recap is due once the session has been idle for `idle_seconds`, with nothing running
    /// and nothing held, and only once per idle period.
    pub fn due(&self, idle: std::time::Duration, busy: bool, held: bool, already_shown: bool) -> bool {
        self.enabled && !busy && !held && !already_shown && idle.as_secs() >= self.idle_seconds
    }
}

#[derive(Deserialize, Serialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct MagicKeywordsCfg {
    /// Append the hidden notice for a standalone magic keyword in a prompt.
    pub enabled: bool,
}

impl Default for MagicKeywordsCfg {
    fn default() -> Self {
        Self { enabled: true }
    }
}

#[derive(Deserialize, Serialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct StartupCfg {
    /// Skip the welcome screen and startup status messages.
    pub quiet: bool,
    /// Show the full animated setup splash on a normal interactive start.
    pub show_splash: bool,
    /// Show newly added onboarding steps once per setup version.
    pub setup_wizard: bool,
    pub marketplace_auto_update: MarketplaceUpdate,
    pub changelog_mode: ChangelogMode,
}

impl Default for StartupCfg {
    fn default() -> Self {
        Self {
            quiet: false,
            show_splash: false,
            setup_wizard: true,
            marketplace_auto_update: MarketplaceUpdate::Notify,
            changelog_mode: ChangelogMode::Summary,
        }
    }
}

/// `off` skips plugin update checks; `notify` reports them; `auto` installs them.
#[derive(Deserialize, Serialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum MarketplaceUpdate {
    Off,
    #[default]
    Notify,
    Auto,
}

/// How update notes appear on startup.
#[derive(Deserialize, Serialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ChangelogMode {
    #[default]
    Summary,
    Expanded,
    Hidden,
}

/// OMP key -> native dotted key; `config import-omp` copies the value verbatim unless `convert` handles it.
pub(super) const OMP: &[(&str, &str)] = &[
    ("followUpMode", "follow_up_mode"),
    ("tools.approvalMode", "approval_mode"),
    ("btw.inject", "btw.inject"),
    ("spelling.typoDetection", "spelling.typo_detection"),
    ("spelling.autocomplete", "spelling.autocomplete"),
    ("spelling.autocorrect", "spelling.autocorrect"),
    ("stt.enabled", "stt.enabled"),
    ("stt.submitTrigger", "stt.submit_trigger"),
    ("recap.enabled", "recap.enabled"),
    ("recap.idleSeconds", "recap.idle_seconds"),
    ("magicKeywords.enabled", "magic_keywords.enabled"),
    ("startup.quiet", "startup.quiet"),
    ("startup.showSplash", "startup.show_splash"),
    ("startup.setupWizard", "startup.setup_wizard"),
    ("marketplace.autoUpdate", "startup.marketplace_auto_update"),
    ("startup.changelogMode", "startup.changelog_mode"),
];

/// Translates an OMP value whose shape or units differ from the native field; `None` copies it verbatim.
pub(super) fn convert(_from: &str, _value: &Value) -> Option<Result<toml::Value>> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    #[test]
    fn defaults_match_omp_and_import_keeps_enum_spellings() {
        let cfg = Config::default();
        assert_eq!(cfg.interaction_opts.follow_up_mode, FollowUpDrain::OneAtATime);
        assert_eq!(cfg.interaction_opts.approval_mode, ApprovalMode::AlwaysAsk, "Ask mode keeps asking before writes");
        assert!(!cfg.interaction_opts.btw.inject);
        assert_eq!(cfg.interaction_opts.spelling.autocomplete, WordCompletion::Auto);
        assert!(cfg.interaction_opts.spelling.typo_detection && !cfg.interaction_opts.spelling.autocorrect);
        assert_eq!((cfg.interaction_opts.stt.enabled, cfg.interaction_opts.stt.submit_trigger), (false, SubmitTrigger::Never));
        assert_eq!((cfg.interaction_opts.recap.enabled, cfg.interaction_opts.recap.idle_seconds), (true, 240));
        assert!(cfg.interaction_opts.magic_keywords.enabled);
        assert_eq!(
            (cfg.interaction_opts.startup.quiet, cfg.interaction_opts.startup.show_splash, cfg.interaction_opts.startup.setup_wizard),
            (false, false, true)
        );
        assert_eq!(cfg.interaction_opts.startup.marketplace_auto_update, MarketplaceUpdate::Notify);
        assert_eq!(cfg.interaction_opts.startup.changelog_mode, ChangelogMode::Summary);
        for (omp, spellings) in [
            ("followUpMode", "all\n---\none-at-a-time"),
            ("tools.approvalMode", "always-ask\n---\nalways-ask-cached\n---\nwrite\n---\nyolo"),
            ("spelling.autocomplete", "off\n---\nauto\n---\nngram\n---\nsmollm\n---\napple"),
            ("stt.submitTrigger", "never\n---\nrelease\n---\nrelease-complete\n---\nsay-submit"),
            ("marketplace.autoUpdate", "off\n---\nnotify\n---\nauto"),
            ("startup.changelogMode", "summary\n---\nexpanded\n---\nhidden"),
        ] {
            for spelling in spellings.split("---") {
                let yaml = format!("{omp}: {}\n", spelling.trim());
                let value = super::super::settings::translate(&yaml).unwrap_or_else(|e| panic!("{yaml}: {e:#}"));
                crate::config::settings::decode(toml::Value::Table(value))
                    .unwrap_or_else(|e| panic!("{yaml}: {e:#}"));
            }
        }
    }

    #[test]
    fn submit_trigger_matches_omp_release_rules() {
        assert_eq!(SubmitTrigger::Never.evaluate("hello there."), (false, 0));
        assert_eq!(SubmitTrigger::Release.evaluate(" hello   there "), (true, 0));
        assert_eq!(SubmitTrigger::Release.evaluate("hello"), (false, 0));
        assert_eq!(SubmitTrigger::ReleaseComplete.evaluate("done."), (true, 0));
        assert_eq!(SubmitTrigger::ReleaseComplete.evaluate("done"), (false, 0));
        assert_eq!(SubmitTrigger::ReleaseComplete.evaluate("完了。"), (true, 0));
        let (submit, trim) = SubmitTrigger::SaySubmit.evaluate("ship it submit please");
        assert!(submit);
        assert_eq!("ship it submit please"[.."ship it submit please".len() - trim].trim_end(), "ship it");
        assert_eq!(SubmitTrigger::SaySubmit.evaluate("submitted already"), (false, 0));
    }

    #[test]
    fn recap_fires_once_after_the_configured_idle_time() {
        let cfg = RecapCfg::default();
        assert!(!cfg.due(std::time::Duration::from_secs(239), false, false, false));
        assert!(cfg.due(std::time::Duration::from_secs(240), false, false, false));
        assert!(!cfg.due(std::time::Duration::from_secs(600), true, false, false));
        assert!(!cfg.due(std::time::Duration::from_secs(600), false, true, false));
        assert!(!cfg.due(std::time::Duration::from_secs(600), false, false, true));
        assert!(!RecapCfg { enabled: false, ..cfg }.due(std::time::Duration::from_secs(600), false, false, false));
    }
}
