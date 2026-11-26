//! `[advisor]` (passive second-model review) and `[prewalk]` (plan on the strong model, hand off before editing); flattened into `Config`.
use anyhow::Result;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;

/// How badly the advisor wants the primary agent to change course; ordered.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Nit,
    #[default]
    Concern,
    Blocker,
}

impl Severity {
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "nit" => Some(Self::Nit),
            "concern" => Some(Self::Concern),
            "blocker" => Some(Self::Blocker),
            _ => None,
        }
    }
}

/// When the advisor reviews: every primary turn, or only when the agent finishes with a final answer.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ReviewMode {
    #[default]
    Turn,
    AgentEnd,
}

/// How often the primary agent waits for a review backlog.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SyncBacklog {
    /// Never wait for scheduled reviews.
    #[default]
    Off,
    /// Pause until fewer than this many reviews are in flight; capped at 30 seconds per wait.
    Threshold(u32),
    /// Pause until every scheduled review has landed, without a wall-clock cap.
    Strict,
}

impl SyncBacklog {
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "off" => Some(Self::Off),
            "strict" => Some(Self::Strict),
            n => n.parse().ok().filter(|&n: &u32| n > 0).map(Self::Threshold),
        }
    }

    /// The number of in-flight reviews at which the primary agent waits; `None` never waits.
    pub fn threshold(self) -> Option<u32> {
        match self {
            Self::Off => None,
            Self::Threshold(n) => Some(n.max(1)),
            Self::Strict => Some(1),
        }
    }
}

impl Serialize for SyncBacklog {
    fn serialize<S: Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        match self {
            Self::Off => s.serialize_str("off"),
            Self::Strict => s.serialize_str("strict"),
            Self::Threshold(n) => s.serialize_u32(*n),
        }
    }
}

impl<'de> Deserialize<'de> for SyncBacklog {
    fn deserialize<D: Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        let value = Value::deserialize(d)?;
        let parsed = match &value {
            Value::Bool(false) => Some(Self::Off),
            Value::Bool(true) => Some(Self::Strict),
            Value::Number(n) => n.as_u64().map(|n| Self::Threshold(n.min(u32::MAX as u64) as u32)),
            Value::String(s) => Self::parse(s),
            _ => None,
        };
        parsed.ok_or_else(|| serde::de::Error::custom("sync_backlog must be off, strict or a count"))
    }
}

/// `off|suggest|on`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Auto {
    #[default]
    Off,
    Suggest,
    On,
}

#[derive(Deserialize, Serialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct AdvisorCfg {
    /// Pair a second model (the `advisor` role) that passively reviews each turn and injects notes.
    pub enabled: bool,
    pub sync_backlog: SyncBacklog,
    /// Turns after an interrupting note during which concerns stay non-interrupting; blockers are exempt.
    pub immune_turns: u32,
    /// Lowest severity that wakes the primary agent after it finished with a final answer.
    pub continue_severity: Severity,
    pub review_mode: ReviewMode,
    /// Review every Nth eligible primary update; 1 = every update.
    pub review_interval: u32,
    /// Non-blocker notes accepted per review; blockers are exempt.
    pub max_notes_per_update: u32,
    /// Replace old read/grep/glob output in the review digest with a placeholder, keeping the latest review.
    pub evict_stale_results: bool,
}

impl Default for AdvisorCfg {
    fn default() -> Self {
        Self {
            enabled: false,
            sync_backlog: SyncBacklog::Off,
            immune_turns: 0,
            continue_severity: Severity::Concern,
            review_mode: ReviewMode::Turn,
            review_interval: 1,
            max_notes_per_update: 3,
            evict_stale_results: true,
        }
    }
}

impl AdvisorCfg {
    pub fn validate(&self) -> Result<(), String> {
        if self.review_interval == 0 {
            return Err("advisor.review_interval must be at least 1".into());
        }
        if !(1..=32).contains(&self.max_notes_per_update) {
            return Err("advisor.max_notes_per_update must be 1..=32".into());
        }
        if self.immune_turns > 1000 {
            return Err("advisor.immune_turns must be 0..=1000".into());
        }
        Ok(())
    }
}

#[derive(Deserialize, Serialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct PrewalkCfg {
    /// Start on the active model, then switch to `into` at the first edit/write after the plan's todos.
    pub enabled: bool,
    /// Model or role planning starts on; `/prewalk restart` returns here.
    pub from: String,
    /// Model or role prewalk hands off to.
    pub into: String,
    /// Ask the `judge` role whether a new task front-loads its hard reasoning.
    pub auto: Auto,
    /// Model or role auto-prewalk hands off to.
    pub auto_target: String,
}

impl Default for PrewalkCfg {
    fn default() -> Self {
        Self { enabled: false, from: "@default".into(), into: "@smol".into(), auto: Auto::Off, auto_target: "@smol".into() }
    }
}

impl PrewalkCfg {
    pub fn validate(&self) -> Result<(), String> {
        for (name, spec) in [("prewalk.from", &self.from), ("prewalk.into", &self.into), ("prewalk.autoTarget", &self.auto_target)] {
            if spec.trim().is_empty() {
                return Err(format!("{name} must name a model or role"));
            }
        }
        Ok(())
    }
}

#[derive(Deserialize, Serialize, Clone, Debug, Default, PartialEq)]
#[serde(default)]
pub struct AdvisorOpts {
    pub advisor: AdvisorCfg,
    pub prewalk: PrewalkCfg,
}

/// OMP key → native dotted key; `config import-omp` copies the value verbatim unless `convert` handles it.
pub(super) const OMP: &[(&str, &str)] = &[
    ("advisor.enabled", "advisor.enabled"),
    ("advisor.syncBacklog", "advisor.sync_backlog"),
    ("advisor.immuneTurns", "advisor.immune_turns"),
    ("advisor.continueSeverity", "advisor.continue_severity"),
    ("advisor.reviewMode", "advisor.review_mode"),
    ("advisor.reviewInterval", "advisor.review_interval"),
    ("advisor.maxNotesPerUpdate", "advisor.max_notes_per_update"),
    ("advisor.evictStaleResults", "advisor.evict_stale_results"),
    ("prewalk.enabled", "prewalk.enabled"),
    ("prewalk.from", "prewalk.from"),
    ("prewalk.into", "prewalk.into"),
    ("prewalk.auto", "prewalk.auto"),
    ("prewalk.autoTarget", "prewalk.auto_target"),
];

/// OMP writes a numeric backlog as a string (`"2"`); the native key takes the integer.
pub(super) fn convert(from: &str, value: &Value) -> Option<Result<toml::Value>> {
    if from != "advisor.syncBacklog" {
        return None;
    }
    Some(match value {
        Value::Bool(false) => Ok(toml::Value::String("off".into())),
        Value::Bool(true) => Ok(toml::Value::String("strict".into())),
        Value::String(s) => match s.trim() {
            "off" | "strict" => Ok(toml::Value::String(s.trim().into())),
            n => n
                .parse::<i64>()
                .ok()
                .filter(|&n| n > 0)
                .map(|n| toml::Value::Integer(n))
                .ok_or_else(|| anyhow::anyhow!("{from} must be off, strict or a positive count")),
        },
        Value::Number(n) => n
            .as_u64()
            .map(|n| toml::Value::Integer(n as i64))
            .ok_or_else(|| anyhow::anyhow!("{from} must be off, strict or a positive count")),
        _ => Err(anyhow::anyhow!("{from} must be off, strict or a positive count")),
    })
}

#[cfg(test)]
mod tests {
    use crate::config::Config;

    #[test]
    fn omp_rows_import_and_defaults_hold() {
        let yaml = "\
advisor:
  enabled: true
  syncBacklog: '2'
  immuneTurns: 3
  continueSeverity: blocker
  reviewMode: agent-end
  reviewInterval: 5
  maxNotesPerUpdate: 4
  evictStaleResults: false
prewalk:
  enabled: true
  from: '@default'
  into: '@slow'
  auto: suggest
  autoTarget: mock/small
";
        let table = crate::config::settings::omp::translate(yaml).unwrap();
        let cfg = crate::config::settings::decode(toml::Value::Table(table)).unwrap();
        let a = &cfg.assist_opts.advisor;
        assert!(a.enabled);
        assert_eq!(a.sync_backlog, super::SyncBacklog::Threshold(2));
        assert_eq!(a.immune_turns, 3);
        assert_eq!(a.continue_severity, super::Severity::Blocker);
        assert_eq!(a.review_mode, super::ReviewMode::AgentEnd);
        assert_eq!(a.review_interval, 5);
        assert_eq!(a.max_notes_per_update, 4);
        assert!(!a.evict_stale_results);
        let p = &cfg.assist_opts.prewalk;
        assert!(p.enabled && p.auto == super::Auto::Suggest);
        assert_eq!((p.from.as_str(), p.into.as_str(), p.auto_target.as_str()), ("@default", "@slow", "mock/small"));
        let d = Config::default();
        assert_eq!(d.assist_opts.advisor.review_interval, 1);
        assert_eq!(d.assist_opts.advisor.sync_backlog, super::SyncBacklog::Off);
        assert_eq!(d.assist_opts.prewalk.into, "@smol");
        assert_eq!(super::SyncBacklog::parse("strict"), Some(super::SyncBacklog::Strict));
        assert_eq!(super::SyncBacklog::parse("7"), Some(super::SyncBacklog::Threshold(7)));
        assert_eq!(super::SyncBacklog::parse("0"), None);
        assert!(crate::config::settings::decode(toml::Value::Table(
            crate::config::settings::omp::translate("advisor:\n  reviewInterval: 0\n").unwrap()
        )).is_err());
    }
}
