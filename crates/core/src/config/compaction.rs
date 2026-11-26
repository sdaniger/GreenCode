//! Settings for automatic context maintenance and snap compaction; flattened into `Config`.
use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::time::Duration;

/// Characters per token when a token budget becomes a character budget; native has no tokenizer.
pub const CHARS_PER_TOKEN: usize = 4;
/// OMP enforces the same floor: a shorter delay would compact on any pause.
pub const MIN_IDLE_SECONDS: u64 = 60;

#[derive(Deserialize, Serialize, Clone, Debug, Default, PartialEq)]
#[serde(default)]
pub struct CompactionOpts {
    /// OMP `compaction.*` keys newer than the flat fields on `Config`.
    pub maintenance: MaintenanceCfg,
    /// OMP `snapcompact.*`.
    pub snapcompact: SnapCompactCfg,
}

#[derive(Deserialize, Serialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct MaintenanceCfg {
    /// Preferred method order; empty keeps the legacy `compaction` setting (`snap` or `summary`).
    pub method_order: Vec<String>,
    /// Percent of the context window that triggers maintenance; -1 keeps the absolute `compact_at`.
    pub threshold_percent: f64,
    /// Tokens left unused below the window; unset keeps the absolute `compact_at`.
    pub reserve_tokens: Option<u64>,
    /// Recent-history budget kept verbatim after a compaction; unset keeps the legacy character budget.
    pub keep_recent_tokens: Option<u64>,
    /// Save an automatically generated compaction summary as `handoff-<timestamp>.md`.
    pub handoff_save_to_disk: bool,
    /// Compact while the thread is idle.
    pub idle_enabled: bool,
    /// Token count above which idle compaction runs.
    pub idle_threshold_tokens: u64,
    /// Seconds to wait while idle before compacting.
    pub idle_timeout_seconds: u64,
    /// Blank older read results when the same file is read again.
    pub supersede_reads: bool,
    /// Blank search results that found nothing.
    pub drop_useless: bool,
    /// Context window of the active model; native has no model-window registry, so percent and
    /// reserve thresholds need it explicitly.
    pub context_window: Option<u64>,
    /// Notes-backed context windows: keep `notes.md` and a searchable raw history across compactions.
    pub experimental_context_management: bool,
    /// Server-side streaming compaction over the Responses endpoint.
    pub remote_streaming_v2: bool,
    /// Summarize speculatively in the background as the context nears the threshold.
    pub async_enabled: bool,
}

impl Default for MaintenanceCfg {
    fn default() -> Self {
        Self {
            method_order: Vec::new(),
            threshold_percent: -1.0,
            reserve_tokens: None,
            keep_recent_tokens: None,
            handoff_save_to_disk: false,
            // Native already blanked or shortened these only at compaction, so both opt in.
            idle_enabled: false,
            idle_threshold_tokens: 200_000,
            idle_timeout_seconds: 300,
            supersede_reads: false,
            drop_useless: false,
            context_window: None,
            experimental_context_management: false,
            remote_streaming_v2: false,
            async_enabled: false,
        }
    }
}

impl MaintenanceCfg {
    /// Absolute token trigger. An explicit per-model limit wins, then percent, then the reserve, then
    /// the legacy `compact_at`.
    pub fn threshold(&self, model_limit: Option<u64>, compact_at: u64) -> Result<u64> {
        if let Some(tokens) = model_limit {
            return Ok(tokens);
        }
        if self.threshold_percent > 0.0 {
            let window = self.window()?;
            return Ok(((window as f64) * self.threshold_percent / 100.0).round() as u64);
        }
        if let Some(reserve) = self.reserve_tokens {
            let window = self.window()?;
            let reserve = ((window as f64 * 0.15) as u64).max(reserve);
            return Ok(window.saturating_sub(reserve));
        }
        Ok(compact_at)
    }

    fn window(&self) -> Result<u64> {
        self.context_window.ok_or_else(|| {
            anyhow::anyhow!(
                "maintenance.threshold_percent/reserve_tokens need the model context window, which native has no registry for; set maintenance.context_window (tokens) or use compact_at"
            )
        })
    }

    pub fn keep_chars(&self) -> usize {
        self.keep_recent_tokens.map_or(crate::snap::KEEP_CHARS, |tokens| {
            (tokens as usize).saturating_mul(CHARS_PER_TOKEN)
        })
    }

    pub fn idle_due(&self, context: u64) -> bool {
        self.idle_enabled && context >= self.idle_threshold_tokens
    }

    pub fn idle_delay(&self) -> Duration {
        Duration::from_secs(self.idle_timeout_seconds.max(MIN_IDLE_SECONDS))
    }
}

#[derive(Deserialize, Serialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct SnapCompactCfg {
    /// Frame geometry: `auto` (native 8x10), `11on16-bw`, `8on22-bw`, `8on16-bw` or `8x13-bw`.
    pub shape: String,
    /// Which system-prompt text ships as images instead of text.
    pub system_prompt: SnapSystemPrompt,
    /// Render large tool results as images instead of text.
    pub tool_results: bool,
}

impl Default for SnapCompactCfg {
    fn default() -> Self {
        Self {
            shape: "auto".into(),
            system_prompt: SnapSystemPrompt::None,
            tool_results: false,
        }
    }
}

/// Which part of the system prompt `snapcompact.system_prompt` ships as images.
#[derive(Deserialize, Serialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum SnapSystemPrompt {
    #[default]
    None,
    /// Only the AGENTS.md/CLAUDE.md project-instruction section.
    AgentsMd,
    /// The whole system prompt.
    All,
}

impl SnapSystemPrompt {
    /// Whether this section's text is replaced by frames; `heading` starts the section.
    pub fn covers(self, heading: Option<&str>) -> bool {
        match self {
            SnapSystemPrompt::None => false,
            SnapSystemPrompt::All => true,
            SnapSystemPrompt::AgentsMd => heading.is_some_and(|h| h.starts_with("# Project instructions")),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Method {
    Snap,
    Summary,
}

/// First usable maintenance method. Availability is native: `snapcompact` needs an image-reading
/// model and `soft` is summarization; OMP-only methods (`remote`, `handoff`, `shake`) advance.
pub fn method(order: &[String], legacy: &str, has_snap: bool) -> Result<Method> {
    if order.is_empty() {
        return Ok(if legacy == "snap" && has_snap { Method::Snap } else { Method::Summary });
    }
    for name in order {
        match name.as_str() {
            "snapcompact" if has_snap => return Ok(Method::Snap),
            "soft" | "summary" => return Ok(Method::Summary),
            _ => {}
        }
    }
    bail!(
        "no usable compaction method in maintenance.method_order {order:?}: native supports `snapcompact` (image-reading models) and `soft`"
    )
}

/// OMP key → native dotted key; `config import-omp` copies the value verbatim unless `convert` handles it.
pub(super) const OMP: &[(&str, &str)] = &[
    ("compaction.methodOrder", "maintenance.method_order"),
    ("compaction.thresholdPercent", "maintenance.threshold_percent"),
    ("compaction.reserveTokens", "maintenance.reserve_tokens"),
    ("compaction.keepRecentTokens", "maintenance.keep_recent_tokens"),
    ("compaction.handoffSaveToDisk", "maintenance.handoff_save_to_disk"),
    ("compaction.idleEnabled", "maintenance.idle_enabled"),
    ("compaction.idleThresholdTokens", "maintenance.idle_threshold_tokens"),
    ("compaction.idleTimeoutSeconds", "maintenance.idle_timeout_seconds"),
    ("compaction.supersedeReads", "maintenance.supersede_reads"),
    ("compaction.dropUseless", "maintenance.drop_useless"),
    ("snapcompact.shape", "snapcompact.shape"),
    ("snapcompact.systemPrompt", "snapcompact.system_prompt"),
    ("snapcompact.toolResults", "snapcompact.tool_results"),
    ("compaction.experimentalContextManagement", "maintenance.experimental_context_management"),
    ("compaction.remoteStreamingV2Enabled", "maintenance.remote_streaming_v2"),
    ("compaction.asyncEnabled", "maintenance.async_enabled"),
];

/// Translates an OMP value whose shape or units differ from the native field; `None` copies it verbatim.
pub(super) fn convert(from: &str, value: &Value) -> Option<Result<toml::Value>> {
    match from {
        // OMP prints the unset percentage as `Default`; native keeps -1 for the same meaning.
        "compaction.thresholdPercent" => {
            let percent = match value {
                Value::Null => -1.0,
                Value::String(s) if s.eq_ignore_ascii_case("default") => -1.0,
                other => match other.as_f64() {
                    Some(n) => n,
                    None => return Some(Err(anyhow::anyhow!("{from} must be a number or Default"))),
                },
            };
            Some(Ok(toml::Value::Float(percent)))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn method_order_falls_back_to_the_next_usable_method() {
        let order = |names: &[&str]| names.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let omp = order(&["remote", "snapcompact", "handoff", "shake", "soft"]);
        assert_eq!(method(&omp, "summary", true).unwrap(), Method::Snap);
        assert_eq!(method(&omp, "summary", false).unwrap(), Method::Summary);
        assert_eq!(method(&order(&["remote", "soft"]), "snap", true).unwrap(), Method::Summary);
        assert!(method(&order(&["remote", "handoff", "shake"]), "summary", true).is_err());
        assert_eq!(method(&[], "snap", true).unwrap(), Method::Snap);
        assert_eq!(method(&[], "snap", false).unwrap(), Method::Summary);
        assert_eq!(method(&[], "summary", true).unwrap(), Method::Summary);
    }

    #[test]
    fn percent_and_reserve_thresholds_need_the_context_window() {
        let mut m = MaintenanceCfg { threshold_percent: 80.0, context_window: Some(200_000), ..Default::default() };
        assert_eq!(m.threshold(None, 250_000).unwrap(), 160_000);
        m.threshold_percent = -1.0;
        m.reserve_tokens = Some(16_384);
        assert_eq!(m.threshold(None, 250_000).unwrap(), 170_000);
        m.reserve_tokens = Some(50_000);
        assert_eq!(m.threshold(None, 250_000).unwrap(), 150_000);
        assert_eq!(m.threshold(Some(90_000), 250_000).unwrap(), 90_000);
        m.context_window = None;
        assert!(m.threshold(None, 250_000).unwrap_err().to_string().contains("context_window"));
        m.threshold_percent = 50.0;
        m.reserve_tokens = None;
        assert!(m.threshold(None, 250_000).is_err());
        assert_eq!(MaintenanceCfg::default().threshold(None, 250_000).unwrap(), 250_000);
    }

    #[test]
    fn idle_compaction_waits_for_the_configured_delay_and_threshold() {
        let mut m = MaintenanceCfg::default();
        assert!(!m.idle_due(1_000_000));
        m.idle_enabled = true;
        assert!(!m.idle_due(199_999));
        assert!(m.idle_due(200_000));
        assert_eq!(m.idle_delay(), Duration::from_secs(300));
        m.idle_timeout_seconds = 5;
        assert_eq!(m.idle_delay(), Duration::from_secs(MIN_IDLE_SECONDS));
        assert_eq!(m.keep_chars(), crate::snap::KEEP_CHARS);
        m.keep_recent_tokens = Some(20_000);
        assert_eq!(m.keep_chars(), 80_000);
    }

    #[test]
    fn imports_omp_maintenance_and_snapcompact_settings() {
        let yaml = "compaction:\n  methodOrder: [snapcompact, soft]\n  thresholdPercent: 80\n  reserveTokens: 50000\n  keepRecentTokens: 12000\n  handoffSaveToDisk: true\n  idleEnabled: true\n  idleThresholdTokens: 150000\n  idleTimeoutSeconds: 120\n  supersedeReads: true\n  dropUseless: true\nsnapcompact:\n  shape: 8on22-bw\n";
        let table = crate::config::settings::translate_omp(yaml).unwrap();
        let cfg = crate::config::settings::decode(toml::Value::Table(table)).unwrap();
        let m = &cfg.compaction_opts.maintenance;
        assert_eq!(m.method_order, ["snapcompact", "soft"]);
        assert_eq!(m.threshold_percent, 80.0);
        assert_eq!(m.reserve_tokens, Some(50_000));
        assert_eq!(m.keep_recent_tokens, Some(12_000));
        assert!(m.handoff_save_to_disk && m.idle_enabled && m.supersede_reads && m.drop_useless);
        assert_eq!(m.idle_threshold_tokens, 150_000);
        assert_eq!(m.idle_timeout_seconds, 120);
        assert_eq!(cfg.compaction_opts.snapcompact.shape, "8on22-bw");
        let percent = |yaml: &str| {
            let table = crate::config::settings::translate_omp(yaml).unwrap();
            crate::config::settings::decode(toml::Value::Table(table)).unwrap().compaction_opts.maintenance.threshold_percent
        };
        assert_eq!(percent("compaction.thresholdPercent: 70\n"), 70.0);
        assert_eq!(percent("compaction.thresholdPercent: Default\n"), -1.0);
        assert!(crate::config::settings::translate_omp("compaction.thresholdPercent: half\n").is_err());
        let edited = crate::config::settings::set_text("", "maintenance.threshold_percent", &["70".into()]).unwrap();
        let edited = crate::config::settings::decode(toml::from_str(&edited).unwrap()).unwrap();
        assert_eq!(edited.compaction_opts.maintenance.threshold_percent, 70.0);
        let edited = crate::config::settings::set_text("", "snapcompact.shape", &["8on16-bw".into()]).unwrap();
        let edited = crate::config::settings::decode(toml::from_str(&edited).unwrap()).unwrap();
        assert_eq!(edited.compaction_opts.snapcompact.shape, "8on16-bw");
    }
}
