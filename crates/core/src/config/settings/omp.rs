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
];

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

pub(super) fn translate(text: &str) -> Result<toml::Table> {
    let options = serde_saphyr::options! { strict_booleans: true };
    let source: Value = serde_saphyr::from_str_with_options(text, options).map_err(|_| anyhow!("Invalid OMP YAML"))?;
    if source.is_null() { return Ok(toml::Table::new()); }
    ensure!(source.is_object(), "OMP settings must be a YAML mapping");
    let mut out = toml::Table::new();
    for &(from, to) in SETTINGS {
        let nested = from.split('.').try_fold(&source, |node, segment| node.get(segment));
        let flat = from.contains('.').then(|| source.get(from)).flatten();
        ensure!(flat.is_none() || nested.is_none(), "OMP setting {from} appears as both a dotted and nested key");
        let Some(value) = flat.or(nested) else { continue };
        let value = if from == "compaction.thresholdTokens" {
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
