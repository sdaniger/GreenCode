//! `defaultThinkingLevel = auto`: a keyword classifier picks a reasoning effort per turn, clamped by
//! `providers.autoThinkingMaxEffort`.

use crate::config::provider::AutoThinkingMaxEffort;
use crate::config::Config;

/// The classifier's ladder, weakest first.
const LEVELS: [&str; 5] = ["low", "medium", "high", "xhigh", "max"];

/// An explicit `ultrathink` asks for the top tier; the ceiling only limits the classifier.
pub fn explicit(prompt: &str) -> Option<&'static str> {
    let text = prompt.to_ascii_lowercase();
    ["ultrathink", "ultra think", "think harder", "think hard", "think more"]
        .iter()
        .position(|needle| text.contains(needle))
        .map(|i| if i == 0 { "max" } else { "xhigh" })
}

/// Heuristic depth for one prompt: hard debug/migration work bills more, trivial asks less.
pub fn classify(prompt: &str, ceiling: AutoThinkingMaxEffort) -> &'static str {
    if let Some(level) = explicit(prompt) {
        return level;
    }
    let text = prompt.to_ascii_lowercase();
    let any = |words: &[&str]| words.iter().any(|w| text.contains(w));
    let hard = any(&[
        "race", "deadlock", "segfault", "unsafe", "concurrency", "migration", "architecture", "refactor",
        "prove", "optimi", "performance", "security", "vulnerab", "root cause",
    ]);
    let trivial = text.len() < 120
        && any(&["rename", "typo", "format", "what is", "explain", "show me", "list ", "print "])
        && !hard;
    let level = if hard {
        "xhigh"
    } else if trivial {
        "low"
    } else {
        "medium"
    };
    clamp(level, ceiling)
}

/// The ceiling keeps the classifier one tier below the top unless it allows `max`.
fn clamp(level: &'static str, ceiling: AutoThinkingMaxEffort) -> &'static str {
    let top = match ceiling {
        AutoThinkingMaxEffort::Xhigh => "xhigh",
        AutoThinkingMaxEffort::Max => "max",
    };
    let rank = |l: &str| LEVELS.iter().position(|x| *x == l).unwrap_or(0);
    if rank(level) > rank(top) { top } else { level }
}

/// The effort to build this turn's provider with; `None` when `effort` is not `auto`.
pub fn resolve(cfg: &Config, prompt: &str, spec: &str) -> Option<String> {
    if cfg.effort.as_deref() != Some("auto") {
        return None;
    }
    let level = classify(prompt, cfg.provider_opts.auto_thinking_max_effort);
    super::effort_levels(cfg, spec)
        .contains(&level)
        .then(|| level.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifier_ceiling_and_explicit_ultrathink() {
        assert_eq!(classify("rename this function", AutoThinkingMaxEffort::Xhigh), "low");
        assert_eq!(classify("find the deadlock root cause in the scheduler", AutoThinkingMaxEffort::Xhigh), "xhigh");
        assert_eq!(classify("ultrathink about the design", AutoThinkingMaxEffort::Xhigh), "max");
        // The ceiling only limits what the classifier itself may pick.
        assert_eq!(clamp("max", AutoThinkingMaxEffort::Xhigh), "xhigh");
        assert_eq!(clamp("max", AutoThinkingMaxEffort::Max), "max");
        assert_eq!(clamp("low", AutoThinkingMaxEffort::Xhigh), "low");
    }
}
