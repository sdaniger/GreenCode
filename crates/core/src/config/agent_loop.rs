//! Settings for the agent loop: steering, interrupts, loop guards and system prompt shaping; flattened into `Config`.
use anyhow::{ensure, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Deserialize, Serialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct LoopOpts {
    /// `all` pushes every queued steer before the next request; `one-at-a-time` pushes one per request.
    pub steering_mode: SteeringMode,
    /// `immediate` drops the running tool batch when a steer arrives; `wait` delivers it after the results.
    pub interrupt_mode: InterruptMode,
    pub power: PowerCfg,
    pub loop_guard: LoopGuardCfg,
    pub tool_call_loop_guard: ToolCallLoopGuardCfg,
    pub include_model_in_prompt: bool,
    /// Model selector (case-insensitive substring) → prompt file that replaces the default instructions.
    pub system_prompt_templates: BTreeMap<String, String>,
    pub include_workspace_tree: bool,
    pub encourage_edits: bool,
    pub personality: Personality,
    pub features: FeatureCfg,
    /// Stop the model the moment it starts writing a tool result inline with in-band tool calls.
    pub abort_on_fabricated_result: bool,
    pub speculative_execution: SpeculativeCfg,
}

/// Discard-safe prefetch of validated local reads before normal tool dispatch.
#[derive(Deserialize, Serialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct SpeculativeCfg {
    pub enabled: bool,
    /// Maximum validated local reads started before dispatch.
    pub max_in_flight: usize,
}

impl Default for SpeculativeCfg {
    fn default() -> Self {
        Self { enabled: false, max_in_flight: 2 }
    }
}

impl Default for LoopOpts {
    fn default() -> Self {
        Self {
            // Native delivered every queued message at once before this setting existed.
            steering_mode: SteeringMode::All,
            // Native cannot interrupt a tool batch in flight before this setting existed.
            interrupt_mode: InterruptMode::Wait,
            power: PowerCfg::default(),
            loop_guard: LoopGuardCfg::default(),
            tool_call_loop_guard: ToolCallLoopGuardCfg::default(),
            include_model_in_prompt: true,
            system_prompt_templates: BTreeMap::new(),
            include_workspace_tree: false,
            encourage_edits: true,
            personality: Personality::default(),
            features: FeatureCfg::default(),
            abort_on_fabricated_result: true,
            speculative_execution: SpeculativeCfg::default(),
        }
    }
}

impl LoopOpts {
    pub(crate) fn validate(&self) -> Result<()> {
        ensure!(
            self.tool_call_loop_guard.threshold > 0,
            "tool_call_loop_guard.threshold must be positive"
        );
        for (selector, path) in &self.system_prompt_templates {
            ensure!(!selector.trim().is_empty(), "system_prompt_templates has a blank model selector");
            ensure!(
                path.starts_with("~/") || std::path::Path::new(path).is_absolute(),
                "system_prompt_templates.{selector} must be an absolute path or start with `~/`"
            );
        }
        Ok(())
    }
}

#[derive(Deserialize, Serialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum SteeringMode {
    #[default]
    All,
    OneAtATime,
}

#[derive(Deserialize, Serialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum InterruptMode {
    Immediate,
    #[default]
    Wait,
}

#[derive(Deserialize, Serialize, Clone, Debug, Default, PartialEq, Eq)]
#[serde(default)]
pub struct PowerCfg {
    pub sleep_prevention: SleepPrevention,
}

/// Cumulative: each level also holds the lower levels' assertions.
#[derive(Deserialize, Serialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum SleepPrevention {
    Off,
    #[default]
    Idle,
    Display,
    System,
}

#[derive(Deserialize, Serialize, Clone, Debug, PartialEq, Eq)]
#[serde(default)]
pub struct LoopGuardCfg {
    pub enabled: bool,
    pub check_assistant_content: bool,
    /// Interrupt a Gemini reasoning stream of planning headers with a reminder to call a tool.
    pub tool_call_reminder: bool,
}

impl Default for LoopGuardCfg {
    fn default() -> Self {
        Self { enabled: true, check_assistant_content: true, tool_call_reminder: true }
    }
}

#[derive(Deserialize, Serialize, Clone, Debug, PartialEq, Eq)]
#[serde(default)]
pub struct ToolCallLoopGuardCfg {
    pub enabled: bool,
    pub threshold: u32,
    pub exempt_tools: Vec<String>,
}

impl Default for ToolCallLoopGuardCfg {
    fn default() -> Self {
        Self { enabled: true, threshold: 5, exempt_tools: vec!["wait".into()] }
    }
}

#[derive(Deserialize, Serialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Personality {
    #[default]
    Default,
    Friendly,
    Pragmatic,
    None,
}

#[derive(Deserialize, Serialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum UnexpectedStopDetection {
    None,
    #[default]
    Mechanical,
    /// Native has no small classifier, so `smart` recovers like `mechanical`.
    Smart,
}

#[derive(Deserialize, Serialize, Clone, Debug, Default, PartialEq, Eq)]
#[serde(default)]
pub struct FeatureCfg {
    pub unexpected_stop_detection: UnexpectedStopDetection,
}

/// OMP key → native dotted key; `config import-omp` copies the value verbatim unless `convert` handles it.
pub(super) const OMP: &[(&str, &str)] = &[
    ("steeringMode", "steering_mode"),
    ("interruptMode", "interrupt_mode"),
    ("power.sleepPrevention", "power.sleep_prevention"),
    ("model.loopGuard.enabled", "loop_guard.enabled"),
    ("model.loopGuard.checkAssistantContent", "loop_guard.check_assistant_content"),
    ("model.loopGuard.toolCallReminder", "loop_guard.tool_call_reminder"),
    ("model.toolCallLoopGuard.enabled", "tool_call_loop_guard.enabled"),
    ("model.toolCallLoopGuard.threshold", "tool_call_loop_guard.threshold"),
    ("model.toolCallLoopGuard.exemptTools", "tool_call_loop_guard.exempt_tools"),
    ("includeModelInPrompt", "include_model_in_prompt"),
    ("systemPromptTemplates", "system_prompt_templates"),
    ("includeWorkspaceTree", "include_workspace_tree"),
    ("encourageEdits", "encourage_edits"),
    ("personality", "personality"),
    ("features.unexpectedStopDetection", "features.unexpected_stop_detection"),
    ("tools.abortOnFabricatedResult", "abort_on_fabricated_result"),
    ("tools.speculativeExecution.enabled", "speculative_execution.enabled"),
    ("tools.speculativeExecution.maxInFlight", "speculative_execution.max_in_flight"),
];

/// Translates an OMP value whose shape or units differ from the native field; `None` copies it verbatim.
pub(super) fn convert(_from: &str, _value: &Value) -> Option<Result<toml::Value>> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn imports_omp_loop_settings_verbatim() {
        let yaml = "steeringMode: one-at-a-time\n\
interruptMode: immediate\n\
power:\n  sleepPrevention: display\n\
model:\n  loopGuard:\n    enabled: false\n    checkAssistantContent: false\n  toolCallLoopGuard:\n    enabled: false\n    threshold: 3\n    exemptTools: [wait, mcp__peers_wait]\n\
includeModelInPrompt: false\n\
systemPromptTemplates:\n  \"openai/gpt-5\": /tmp/gpt.md\n\
includeWorkspaceTree: true\n\
encourageEdits: false\n\
personality: pragmatic\n\
features:\n  unexpectedStopDetection: none\n";
        let table = crate::config::settings::omp::translate(yaml).unwrap();
        let cfg = crate::config::settings::decode(toml::Value::Table(table)).unwrap();
        let opts = &cfg.loop_opts;
        assert_eq!(opts.steering_mode, SteeringMode::OneAtATime);
        assert_eq!(opts.interrupt_mode, InterruptMode::Immediate);
        assert_eq!(opts.power.sleep_prevention, SleepPrevention::Display);
        assert!(!opts.loop_guard.enabled && !opts.loop_guard.check_assistant_content);
        assert!(!opts.tool_call_loop_guard.enabled);
        assert_eq!(opts.tool_call_loop_guard.threshold, 3);
        assert_eq!(opts.tool_call_loop_guard.exempt_tools, ["wait", "mcp__peers_wait"]);
        assert!(!opts.include_model_in_prompt && opts.include_workspace_tree && !opts.encourage_edits);
        assert_eq!(opts.system_prompt_templates["openai/gpt-5"], "/tmp/gpt.md");
        assert_eq!(opts.personality, Personality::Pragmatic);
        assert_eq!(opts.features.unexpected_stop_detection, UnexpectedStopDetection::None);
        assert_eq!(LoopOpts::default().tool_call_loop_guard.threshold, 5);
        assert_eq!(LoopOpts::default().steering_mode, SteeringMode::All);
        let decode = |yaml: &str| {
            crate::config::settings::decode(toml::Value::Table(crate::config::settings::omp::translate(yaml)?))
        };
        assert!(decode("personality: chatty\n").is_err());
        assert!(decode("model:\n  toolCallLoopGuard:\n    threshold: 0\n").is_err());
        assert!(decode("systemPromptTemplates:\n  \"\": /tmp/x.md\n").is_err());
        assert!(decode("systemPromptTemplates:\n  gpt: relative.md\n").is_err());
    }
}
