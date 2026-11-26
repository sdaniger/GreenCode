//! `[memory]`: which memory backend runs, plus Hindsight (remote) and Sharpshooter (project decisions) settings.
//! Flattened into `Config`.
use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Deserialize, Serialize, Clone, Debug, Default, PartialEq)]
#[serde(default)]
pub struct MemoryOpts {
    pub memory: MemoryCfg,
    pub hindsight: HindsightCfg,
    pub mnemopi: crate::config::mnemopi::MnemopiCfg,
    pub sharpshooter: SharpshooterCfg,
}

#[derive(Deserialize, Serialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum MemoryBackend {
    #[default]
    Off,
    Local,
    Hindsight,
    Mnemopi,
    Sharpshooter,
}

#[derive(Deserialize, Serialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct MemoryCfg {
    pub backend: MemoryBackend,
}

impl Default for MemoryCfg {
    fn default() -> Self {
        // OMP ships `off`, but natively `off` would also stop injecting the long-standing `MEMORY.md`
        // files, so the default is the local summary backend: same recall, plus session summaries.
        Self { backend: MemoryBackend::Local }
    }
}

/// Which bank memories live in and what retains are tagged with.
#[derive(Deserialize, Serialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum Scoping {
    /// One shared bank.
    Global,
    /// An isolated bank per project directory.
    PerProject,
    /// A shared bank whose retains carry `project:<name>` tags.
    #[default]
    PerProjectTagged,
}

#[derive(Deserialize, Serialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum RetainMode {
    /// Upsert one document per session.
    #[default]
    FullSession,
    /// One chunked document per retain, sliced by turn boundaries.
    LastTurn,
}

#[derive(Deserialize, Serialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct HindsightCfg {
    /// Server URL, Cloud or self-hosted; empty makes the backend inert.
    pub api_url: String,
    /// Bearer token for authenticated servers.
    pub api_token: Option<String>,
    /// Memory bank identifier; unset uses the project name.
    pub bank_id: Option<String>,
    pub scoping: Scoping,
    /// Lets retain and learn store a memory without the project tag, so every project recalls it.
    pub global_scope: bool,
    /// Recall memories on the first turn of each session.
    pub auto_recall: bool,
    /// Retain the transcript every N turns and when the agent stops.
    pub auto_retain: bool,
    pub retain_mode: RetainMode,
    /// User turns between automatic retains.
    pub retain_every_n_turns: u32,
    /// Read curated reflect summaries into the instructions at session start.
    pub mental_models_enabled: bool,
    /// Create the built-in mental models on the bank when they are missing.
    pub mental_model_auto_seed: bool,
}

impl Default for HindsightCfg {
    fn default() -> Self {
        Self {
            api_url: "http://localhost:8888".into(),
            api_token: None,
            bank_id: None,
            scoping: Scoping::PerProjectTagged,
            global_scope: false,
            auto_recall: true,
            auto_retain: true,
            retain_mode: RetainMode::FullSession,
            retain_every_n_turns: 3,
            mental_models_enabled: true,
            mental_model_auto_seed: true,
        }
    }
}

#[derive(Deserialize, Serialize, Clone, Debug, Default, PartialEq)]
#[serde(default)]
pub struct SharpshooterCfg {
    /// Model for extraction/consolidation; empty uses the `smol` role.
    pub model: Option<String>,
}

/// OMP key → native dotted key; `config import-omp` copies the value verbatim unless `convert` handles it.
pub(super) const OMP: &[(&str, &str)] = &[
    ("memory.backend", "memory.backend"),
    ("hindsight.apiUrl", "hindsight.api_url"),
    ("hindsight.apiToken", "hindsight.api_token"),
    ("hindsight.bankId", "hindsight.bank_id"),
    ("hindsight.scoping", "hindsight.scoping"),
    ("hindsight.globalScope", "hindsight.global_scope"),
    ("hindsight.autoRecall", "hindsight.auto_recall"),
    ("hindsight.autoRetain", "hindsight.auto_retain"),
    ("hindsight.retainMode", "hindsight.retain_mode"),
    ("hindsight.retainEveryNTurns", "hindsight.retain_every_n_turns"),
    ("hindsight.mentalModelsEnabled", "hindsight.mental_models_enabled"),
    ("hindsight.mentalModelAutoSeed", "hindsight.mental_model_auto_seed"),
    ("sharpshooter.model", "sharpshooter.model"),
];

/// Hindsight values are shaped like the native fields; nothing to translate.
pub(super) fn convert(_from: &str, _value: &Value) -> Option<Result<toml::Value>> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    #[test]
    fn defaults_and_omp_spellings_round_trip() {
        let cfg = Config::default();
        let h = &cfg.memory_opts.hindsight;
        assert_eq!(h.api_url, "http://localhost:8888");
        assert_eq!(h.scoping, Scoping::PerProjectTagged);
        assert_eq!(h.retain_mode, RetainMode::FullSession);
        assert!(h.auto_recall && h.auto_retain && h.mental_models_enabled && h.mental_model_auto_seed);
        assert!(!h.global_scope);
        assert_eq!(cfg.memory_opts.memory.backend, MemoryBackend::Local);
        assert_eq!(cfg.memory_opts.sharpshooter.model, None);

        let yaml = "memory: {backend: hindsight}\n\
hindsight: {apiUrl: http://127.0.0.1:9, apiToken: t0ken, bankId: team, scoping: per-project, globalScope: true,\n\
  autoRecall: false, autoRetain: false, retainMode: last-turn, retainEveryNTurns: 5, mentalModelsEnabled: false, mentalModelAutoSeed: false}\n\
sharpshooter: {model: openai/smol}\n";
        let table = crate::config::settings::translate_omp(yaml).unwrap();
        let cfg = crate::config::settings::decode(toml::Value::Table(table)).unwrap();
        let h = &cfg.memory_opts.hindsight;
        assert_eq!(cfg.memory_opts.memory.backend, MemoryBackend::Hindsight);
        assert_eq!((h.api_url.as_str(), h.api_token.as_deref(), h.bank_id.as_deref()), ("http://127.0.0.1:9", Some("t0ken"), Some("team")));
        assert_eq!(h.scoping, Scoping::PerProject);
        assert_eq!(h.retain_mode, RetainMode::LastTurn);
        assert_eq!(h.retain_every_n_turns, 5);
        assert!(h.global_scope && !h.auto_recall && !h.auto_retain && !h.mental_models_enabled && !h.mental_model_auto_seed);
        assert_eq!(cfg.memory_opts.sharpshooter.model.as_deref(), Some("openai/smol"));

        let cfg = crate::config::settings::decode(toml::Value::Table(
            crate::config::settings::translate_omp("memory: {backend: bogus}\n").unwrap(),
        ));
        assert!(cfg.is_err());
    }

    #[test]
    fn every_hindsight_and_sharpshooter_key_maps_to_a_default_value() {
        let tree = serde_json::to_value(Config::default()).unwrap();
        for (from, to) in OMP {
            assert!(
                to.split('.').try_fold(&tree, |node, seg| node.get(seg)).is_some(),
                "{from} -> {to}"
            );
        }
    }
}
