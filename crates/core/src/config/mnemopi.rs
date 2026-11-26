//! Mnemopi settings: the local SQLite memory bank; flattened into `Config` by `MemoryOpts`.
use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::PathBuf;

#[derive(Deserialize, Serialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct MnemopiCfg {
    /// SQLite file; a leading `~` is the home directory. Defaults to the agent memories directory.
    pub db_path: Option<String>,
    /// Shared bank base name; per-project modes derive project-local banks from it.
    pub bank: Option<String>,
    pub scoping: Scoping,
    pub embedding_variant: EmbeddingVariant,
    /// Recall local memories into the first turn of each session.
    pub auto_recall: bool,
    pub recall_relevance: Relevance,
    pub auto_retain: bool,
    pub polyphonic_recall: bool,
    pub enhanced_recall: bool,
    pub proactive_linking: bool,
    /// Force deterministic FTS-only recall instead of vector embeddings.
    pub no_embeddings: bool,
    /// Explicit embedding model id; empty uses `embedding_variant`.
    pub embedding_model: Option<String>,
    pub embedding_api_url: Option<String>,
    pub embedding_api_key: Option<String>,
    /// Prepended to recall queries before embedding; stored memories are embedded unchanged.
    pub embedding_query_prefix: Option<String>,
    /// Send `input_type` (`query`/`passage`) with embedding API requests.
    pub embedding_input_type: bool,
    pub llm_mode: LlmMode,
    pub llm_base_url: Option<String>,
    pub llm_api_key: Option<String>,
    pub llm_model: Option<String>,
}

impl Default for MnemopiCfg {
    fn default() -> Self {
        Self {
            db_path: None,
            bank: None,
            scoping: Scoping::PerProject,
            embedding_variant: EmbeddingVariant::En,
            auto_recall: true,
            recall_relevance: Relevance::Off,
            auto_retain: true,
            polyphonic_recall: false,
            enhanced_recall: false,
            proactive_linking: false,
            no_embeddings: false,
            embedding_model: None,
            embedding_api_url: None,
            embedding_api_key: None,
            embedding_query_prefix: None,
            embedding_input_type: false,
            llm_mode: LlmMode::Smol,
            llm_base_url: None,
            llm_api_key: None,
            llm_model: None,
        }
    }
}

impl MnemopiCfg {
    /// The SQLite file: `~` expands to the home directory; a relative path lives under the agent home.
    pub fn db_file(&self) -> PathBuf {
        let raw = self.db_path.as_deref().map(str::trim).filter(|p| !p.is_empty());
        match raw {
            Some("~") => crate::user_home().join(".greencode/memory/mnemopi.db"),
            Some(path) => match path.strip_prefix("~/") {
                Some(rest) => crate::user_home().join(rest),
                None => {
                    let path = PathBuf::from(path);
                    if path.is_absolute() { path } else { crate::home().join(path) }
                }
            },
            None => crate::home().join("memory/mnemopi.db"),
        }
    }

    /// The bank base name, sanitized to `[a-z0-9-]`; empty means `greencode`.
    pub fn bank_base(&self) -> String {
        let raw = self.bank.as_deref().map(str::trim).unwrap_or("");
        let sanitized = sanitize(if raw.is_empty() { "greencode" } else { raw });
        if sanitized.is_empty() { "greencode".into() } else { sanitized }
    }

    /// `(bank, project)` for a working directory under the configured scoping.
    pub fn store_for(&self, cwd: &std::path::Path) -> (String, String) {
        let base = self.bank_base();
        let key = project_key(cwd);
        match self.scoping {
            Scoping::Global => (base, String::new()),
            Scoping::PerProject => (format!("{base}-{key}"), key),
            Scoping::PerProjectTagged => (base, key),
        }
    }
}

/// Stable per-directory key: sanitized basename plus eight hex characters of the full path.
pub fn project_key(cwd: &std::path::Path) -> String {
    use sha2::{Digest, Sha256};
    let full = std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
    let name = full.file_name().and_then(|n| n.to_str()).unwrap_or("");
    let name = sanitize(name);
    let digest = format!("{:x}", Sha256::digest(full.to_string_lossy().as_bytes()));
    let name = if name.is_empty() { "project".to_string() } else { name };
    format!("{name}-{}", &digest[..8])
}

fn sanitize(raw: &str) -> String {
    raw.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { '-' })
        .collect::<String>()
        .trim_matches('-')
        .to_string()
}

#[derive(Deserialize, Serialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum Scoping {
    Global,
    #[default]
    PerProject,
    PerProjectTagged,
}

#[derive(Deserialize, Serialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum EmbeddingVariant {
    #[default]
    En,
    Multilingual,
}

impl EmbeddingVariant {
    /// Dimension of the built-in hashed embedding for this family.
    pub fn dim(self) -> usize {
        match self {
            EmbeddingVariant::En => 768,
            EmbeddingVariant::Multilingual => 1024,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            EmbeddingVariant::En => "en",
            EmbeddingVariant::Multilingual => "multilingual",
        }
    }
}

#[derive(Deserialize, Serialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Relevance {
    #[default]
    Auto,
    On,
    Off,
}

#[derive(Deserialize, Serialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum LlmMode {
    #[default]
    None,
    Smol,
    Remote,
}

/// OMP key -> native dotted key.
pub(super) const OMP: &[(&str, &str)] = &[
    ("mnemopi.dbPath", "mnemopi.db_path"),
    ("mnemopi.bank", "mnemopi.bank"),
    ("mnemopi.scoping", "mnemopi.scoping"),
    ("mnemopi.embeddingVariant", "mnemopi.embedding_variant"),
    ("mnemopi.autoRecall", "mnemopi.auto_recall"),
    ("mnemopi.recallRelevance", "mnemopi.recall_relevance"),
    ("mnemopi.autoRetain", "mnemopi.auto_retain"),
    ("mnemopi.polyphonicRecall", "mnemopi.polyphonic_recall"),
    ("mnemopi.enhancedRecall", "mnemopi.enhanced_recall"),
    ("mnemopi.proactiveLinking", "mnemopi.proactive_linking"),
    ("mnemopi.noEmbeddings", "mnemopi.no_embeddings"),
    ("mnemopi.embeddingModel", "mnemopi.embedding_model"),
    ("mnemopi.embeddingApiUrl", "mnemopi.embedding_api_url"),
    ("mnemopi.embeddingApiKey", "mnemopi.embedding_api_key"),
    ("mnemopi.embeddingQueryPrefix", "mnemopi.embedding_query_prefix"),
    ("mnemopi.embeddingInputType", "mnemopi.embedding_input_type"),
    ("mnemopi.llmMode", "mnemopi.llm_mode"),
    ("mnemopi.llmBaseUrl", "mnemopi.llm_base_url"),
    ("mnemopi.llmApiKey", "mnemopi.llm_api_key"),
    ("mnemopi.llmModel", "mnemopi.llm_model"),
];

/// Blanks clear optional fields: OMP's empty string means "unset", not `Some("")`.
pub(super) fn convert(from: &str, value: &Value) -> Option<Result<toml::Value>> {
    let optional = matches!(
        from,
        "mnemopi.embeddingModel"
            | "mnemopi.dbPath"
            | "mnemopi.bank"
            | "mnemopi.embeddingApiUrl"
            | "mnemopi.embeddingApiKey"
            | "mnemopi.embeddingQueryPrefix"
            | "mnemopi.llmBaseUrl"
            | "mnemopi.llmApiKey"
            | "mnemopi.llmModel"
    );
    optional.then(|| {
        let text = value.as_str().unwrap_or("").trim();
        Ok(toml::Value::String(text.into()))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scoping_derives_a_stable_bank_and_project() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = MnemopiCfg { bank: Some("Team Bank".into()), ..Default::default() };
        let (bank, project) = cfg.store_for(dir.path());
        assert!(bank.starts_with("team-bank-"), "{bank}");
        assert_eq!(bank, format!("team-bank-{project}"));
        assert_eq!(cfg.store_for(dir.path()), (bank, project.clone()));
        let tagged = MnemopiCfg { scoping: Scoping::PerProjectTagged, ..cfg.clone() };
        assert_eq!(tagged.store_for(dir.path()), ("team-bank".into(), project));
        let global = MnemopiCfg { scoping: Scoping::Global, ..cfg };
        assert_eq!(global.store_for(dir.path()), ("team-bank".into(), String::new()));
    }

    #[test]
    fn db_path_expands_home_and_defaults_beside_the_agent_home() {
        let cfg = MnemopiCfg::default();
        assert_eq!(cfg.db_file(), crate::home().join("memory/mnemopi.db"));
        let custom = MnemopiCfg { db_path: Some("~/mem/m.db".into()), ..Default::default() };
        assert_eq!(custom.db_file(), crate::user_home().join("mem/m.db"));
        let absolute = tempfile::tempdir().unwrap().path().join("x.db");
        let custom = MnemopiCfg { db_path: Some(absolute.to_string_lossy().into()), ..Default::default() };
        assert_eq!(custom.db_file(), absolute);
    }

    #[test]
    fn blank_optional_fields_import_as_empty_strings() {
        let converted = convert("mnemopi.bank", &Value::String("  ".into())).unwrap().unwrap();
        assert_eq!(converted.as_str(), Some(""));
        assert!(convert("mnemopi.scoping", &Value::String("global".into())).is_none());
    }

    #[test]
    fn defaults_match_omp_off_and_smol() {
        let cfg = MnemopiCfg::default();
        assert_eq!(cfg.llm_mode, LlmMode::Smol);
        assert_eq!(cfg.recall_relevance, Relevance::Off);
        assert_eq!(cfg.scoping, Scoping::PerProject);
        // Every `mnemopi.*` value OMP lists parses to a native variant.
        for value in ["none", "smol", "remote"] {
            assert!(serde_json::from_value::<LlmMode>(Value::String(value.into())).is_ok(), "{value}");
        }
        for value in ["global", "per-project", "per-project-tagged"] {
            assert!(serde_json::from_value::<Scoping>(Value::String(value.into())).is_ok(), "{value}");
        }
    }
}
