//! Plan and goal modes; flattened into `Config` so `/settings` can reach `plan.*` and `goal.*`.
use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Deserialize, Serialize, Clone, Debug, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct PlanModeCfg {
    /// Offer plan mode and the `/plan` command.
    pub enabled: bool,
    /// Enter plan mode at the start of every new interactive session.
    pub default_on_startup: bool,
}

impl Default for PlanModeCfg {
    fn default() -> Self { Self { enabled: true, default_on_startup: false } }
}

/// Run modes where an active goal may auto-continue between turns.
#[derive(Deserialize, Serialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum RunMode { #[default] Interactive, Rpc }

#[derive(Deserialize, Serialize, Clone, Debug, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct GoalCfg {
    /// Enable per-session goal mode and the hidden `goal` tool.
    pub enabled: bool,
    /// Show the objective and token budget in the status line.
    pub status_in_footer: bool,
    pub continuation_modes: Vec<RunMode>,
    /// Automatic continuations before the goal pauses itself; 0 means no limit.
    pub max_turns: u32,
}

impl Default for GoalCfg {
    fn default() -> Self { Self { enabled: true, status_in_footer: true, continuation_modes: vec![RunMode::Interactive], max_turns: 20 } }
}

#[derive(Deserialize, Serialize, Clone, Debug, Default, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct ModesOpts {
    pub plan: PlanModeCfg,
    pub goal: GoalCfg,
}

impl ModesOpts {
    /// The mode a new interactive session starts in; startup plan mode needs plan mode itself.
    pub fn startup_plan(&self) -> bool { self.plan.enabled && self.plan.default_on_startup }
    pub fn continues(&self, run: RunMode) -> bool { self.goal.enabled && self.goal.continuation_modes.contains(&run) }
}

/// OMP key → native dotted key.
pub(crate) const OMP: &[(&str, &str)] = &[
    ("plan.enabled", "plan.enabled"),
    ("plan.defaultOnStartup", "plan.default_on_startup"),
    ("goal.enabled", "goal.enabled"),
    ("goal.statusInFooter", "goal.status_in_footer"),
    ("goal.continuationModes", "goal.continuation_modes"),
];

pub(crate) fn convert(from: &str, value: &Value) -> Option<Result<toml::Value>> {
    if from != "goal.continuationModes" { return None; }
    Some((|| {
        let modes = value.as_array().ok_or_else(|| anyhow!("goal.continuationModes must be a list"))?;
        modes.iter().map(|m| match m.as_str() {
            Some(mode @ ("interactive" | "rpc")) => Ok(toml::Value::String(mode.into())),
            _ => Err(anyhow!("goal.continuationModes holds only interactive or rpc")),
        }).collect::<Result<Vec<_>>>().map(toml::Value::Array)
    })())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn omp_plan_and_goal_rows_import() {
        let cfg = crate::config::settings::decode(toml::Value::Table(crate::config::settings::translate(
            "plan:\n  enabled: false\n  defaultOnStartup: true\ngoal:\n  enabled: true\n  statusInFooter: false\n  continuationModes: [rpc, interactive]\n",
        ).unwrap())).unwrap();
        assert!(!cfg.modes.plan.enabled && cfg.modes.plan.default_on_startup);
        assert!(!cfg.modes.startup_plan(), "a disabled plan mode never starts on");
        assert!(!cfg.modes.goal.status_in_footer);
        assert!(cfg.modes.continues(RunMode::Rpc) && cfg.modes.continues(RunMode::Interactive));
        assert!(crate::config::settings::translate("goal:\n  continuationModes: [print]\n").is_err());
        assert!(GoalCfg::default().continuation_modes == [RunMode::Interactive]);
    }
}
