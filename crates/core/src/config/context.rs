//! Multi-root workspaces, context promotion, extended context, branch summaries and the tool-call
//! dialect switch; flattened into `Config`.
use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::PathBuf;

#[derive(Deserialize, Serialize, Clone, Debug, Default, PartialEq)]
#[serde(default)]
pub struct ContextOpts {
    /// OMP `workspace.*`.
    pub workspace: WorkspaceCfg,
    /// OMP `contextPromotion.*`.
    pub context_promotion: PromotionCfg,
    /// OMP `extendedContext`.
    pub extended_context: bool,
    /// OMP `branchSummary.*`.
    pub branch_summary: BranchSummaryCfg,
    /// OMP `tools.format`. Native spelling avoids the `tools` allowlist key.
    pub tools_format: ToolFormat,
}

#[derive(Deserialize, Serialize, Clone, Debug, Default, PartialEq)]
#[serde(default)]
pub struct WorkspaceCfg {
    /// Extra roots every session may read, grep and glob. Relative paths resolve from the launch directory.
    pub additional_directories: Vec<PathBuf>,
}

impl WorkspaceCfg {
    /// Absolute, de-duplicated roots; `~` expands and relative paths resolve from `cwd`.
    pub fn roots(&self, cwd: &std::path::Path) -> Vec<PathBuf> {
        let mut out: Vec<PathBuf> = Vec::new();
        for dir in &self.additional_directories {
            let path = match dir.strip_prefix("~/") {
                Ok(rest) => crate::user_home().join(rest),
                Err(_) if dir.is_absolute() => dir.clone(),
                Err(_) => cwd.join(dir),
            };
            let path = crate::permission::normalize(&path);
            if !out.contains(&path) {
                out.push(path);
            }
        }
        out
    }

    /// The block appended to the session context so the agent knows these roots exist.
    pub fn block(&self, cwd: &std::path::Path) -> String {
        let roots = self.roots(cwd);
        if roots.is_empty() {
            return String::new();
        }
        let mut out = String::from("# Additional workspace roots\nThese directories are additional roots of this workspace; read, grep and glob them by absolute path.\n");
        for root in roots {
            out.push_str(&format!("- {}\n", root.display()));
        }
        out
    }
}

#[derive(Deserialize, Serialize, Clone, Debug, Default, PartialEq)]
#[serde(default)]
pub struct PromotionCfg {
    /// On overflow, switch to `target` instead of compacting.
    pub enabled: bool,
    /// `provider/model` promoted to; OMP reads it from the model's `contextPromotionTarget`.
    pub target: Option<String>,
}

impl PromotionCfg {
    /// The model to switch to, when promotion applies to the overflow from `current`.
    pub fn target_for<'a>(&'a self, current: &str) -> Option<&'a str> {
        let target = self.target.as_deref()?;
        (self.enabled && target != current).then_some(target)
    }
}

#[derive(Deserialize, Serialize, Clone, Debug, Default, PartialEq)]
#[serde(default)]
pub struct BranchSummaryCfg {
    /// Prompt to summarize the branch being left before the forked thread continues.
    pub enabled: bool,
}

/// What the model is asked to describe its tool calls with; `auto`/`native` keep provider-native calls.
#[derive(Deserialize, Serialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ToolFormat {
    #[default]
    Auto,
    Native,
    Glm,
    Hermes,
    Kimi,
    Xml,
    Anthropic,
    Deepseek,
    Harmony,
    Qwen3,
    Gemini,
    Gemma,
    Minimax,
}

impl ToolFormat {
    /// The owned dialect name, or `None` for provider-native tool calls. `auto` is native: like OMP it
    /// would fall back to GLM only for a model marked as lacking tool support, and no model is.
    pub fn dialect(self) -> Option<&'static str> {
        match self {
            ToolFormat::Auto | ToolFormat::Native => None,
            ToolFormat::Glm => Some("glm"),
            ToolFormat::Hermes => Some("hermes"),
            ToolFormat::Kimi => Some("kimi"),
            ToolFormat::Xml => Some("xml"),
            ToolFormat::Anthropic => Some("anthropic"),
            ToolFormat::Deepseek => Some("deepseek"),
            ToolFormat::Harmony => Some("harmony"),
            ToolFormat::Qwen3 => Some("qwen3"),
            ToolFormat::Gemini => Some("gemini"),
            ToolFormat::Gemma => Some("gemma"),
            ToolFormat::Minimax => Some("minimax"),
        }
    }
}

/// OMP key → native dotted key.
pub(super) const OMP: &[(&str, &str)] = &[
    ("workspace.additionalDirectories", "workspace.additional_directories"),
    ("contextPromotion.enabled", "context_promotion.enabled"),
    ("extendedContext", "extended_context"),
    ("branchSummary.enabled", "branch_summary.enabled"),
    ("tools.format", "tools_format"),
];

/// No OMP value has a different shape here: paths, switches and the dialect name copy verbatim.
pub(super) fn convert(_from: &str, _value: &Value) -> Option<Result<toml::Value>> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn additional_directories_resolve_and_describe_themselves() {
        let cfg = WorkspaceCfg {
            additional_directories: vec![PathBuf::from("extra"), PathBuf::from("extra"), PathBuf::from("~/lib")],
        };
        let cwd = std::path::Path::new("/work/project");
        let roots = cfg.roots(cwd);
        assert_eq!(roots.len(), 2);
        assert_eq!(roots[0], PathBuf::from("/work/project/extra"));
        assert!(roots[1].ends_with("lib") && roots[1].is_absolute());
        let block = cfg.block(cwd);
        assert!(block.contains("/work/project/extra"));
        assert!(WorkspaceCfg::default().block(cwd).is_empty());
    }

    #[test]
    fn promotion_targets_a_different_model_only_when_enabled() {
        let mut p = PromotionCfg { enabled: false, target: Some("anthropic/claude-opus-5-5".into()) };
        assert_eq!(p.target_for("anthropic/claude-sonnet-5-5"), None);
        p.enabled = true;
        assert_eq!(p.target_for("anthropic/claude-sonnet-5-5"), Some("anthropic/claude-opus-5-5"));
        assert_eq!(p.target_for("anthropic/claude-opus-5-5"), None);
        assert_eq!(PromotionCfg { enabled: true, target: None }.target_for("x/y"), None);
    }

    #[test]
    fn tool_format_resolves_native_and_dialects() {
        assert_eq!(ToolFormat::Auto.dialect(), None);
        assert_eq!(ToolFormat::Native.dialect(), None);
        assert_eq!(ToolFormat::Minimax.dialect(), Some("minimax"));
        for (name, expected) in [
            ("auto", ToolFormat::Auto),
            ("native", ToolFormat::Native),
            ("glm", ToolFormat::Glm),
            ("hermes", ToolFormat::Hermes),
            ("kimi", ToolFormat::Kimi),
            ("xml", ToolFormat::Xml),
            ("anthropic", ToolFormat::Anthropic),
            ("deepseek", ToolFormat::Deepseek),
            ("harmony", ToolFormat::Harmony),
            ("qwen3", ToolFormat::Qwen3),
            ("gemini", ToolFormat::Gemini),
            ("gemma", ToolFormat::Gemma),
            ("minimax", ToolFormat::Minimax),
        ] {
            let parsed: ToolFormat = serde_json::from_value(Value::String(name.into())).unwrap();
            assert_eq!(parsed, expected, "{name}");
        }
        assert!(serde_json::from_value::<ToolFormat>(Value::String("bogus".into())).is_err());
    }

    #[test]
    fn imports_workspace_promotion_extended_context_and_dialect() {
        let yaml = "workspace:\n  additionalDirectories: [/srv/data, ../shared]\ncontextPromotion:\n  enabled: true\nextendedContext: true\nbranchSummary:\n  enabled: true\ntools:\n  format: hermes\n";
        let table = crate::config::settings::translate_omp(yaml).unwrap();
        let cfg = crate::config::settings::decode(toml::Value::Table(table)).unwrap();
        assert_eq!(cfg.context_opts.workspace.additional_directories.len(), 2);
        assert!(cfg.context_opts.context_promotion.enabled);
        assert!(cfg.context_opts.extended_context);
        assert!(cfg.context_opts.branch_summary.enabled);
        assert_eq!(cfg.context_opts.tools_format, ToolFormat::Hermes);
        let edited = crate::config::settings::set_text("", "tools_format", &["gemma".into()]).unwrap();
        let edited = crate::config::settings::decode(toml::from_str(&edited).unwrap()).unwrap();
        assert_eq!(edited.context_opts.tools_format, ToolFormat::Gemma);
        assert!(crate::config::settings::set_text("", "tools_format", &["bogus".into()]).is_err());
    }
}
