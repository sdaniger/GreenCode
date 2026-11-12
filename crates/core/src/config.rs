use crate::permission::Mode;
use serde::Deserialize;
use std::collections::BTreeMap;

#[derive(Deserialize, Clone, Debug)]
#[serde(default)]
pub struct Config {
    /// `provider/model`.
    pub model: String,
    /// Extra entries for the model picker.
    pub models: Vec<String>,
    pub mode: Mode,
    /// Anthropic `output_config.effort`; unset = API default.
    pub effort: Option<String>,
    pub max_tokens: u32,
    /// Summarize history once the context reaches this many tokens.
    pub compact_at: u64,
    pub providers: BTreeMap<String, ProviderCfg>,
    /// External agents spoken to over ACP.
    pub backends: Vec<AcpCfg>,
}

#[derive(Deserialize, Clone, Debug)]
pub struct ProviderCfg {
    /// `anthropic` or `openai`.
    pub kind: String,
    pub base_url: String,
    pub api_key_env: String,
}

#[derive(Deserialize, Clone, Debug)]
pub struct AcpCfg {
    pub name: String,
    pub command: Vec<String>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            model: "anthropic/claude-opus-5-5".into(),
            models: vec![],
            mode: Mode::Ask,
            effort: None,
            max_tokens: 64000,
            compact_at: 150_000,
            providers: BTreeMap::new(),
            backends: vec![],
        }
    }
}

impl Config {
    pub fn load() -> anyhow::Result<Config> {
        let path = crate::home().join("config.toml");
        let mut cfg: Config = match std::fs::read_to_string(&path) {
            Ok(s) => toml::from_str(&s).map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))?,
            Err(_) => Config::default(),
        };
        let mut default = |name: &str, kind: &str, url: &str, env: &str| {
            cfg.providers.entry(name.into()).or_insert(ProviderCfg {
                kind: kind.into(),
                base_url: url.into(),
                api_key_env: env.into(),
            });
        };
        default("anthropic", "anthropic", "https://api.anthropic.com", "ANTHROPIC_API_KEY");
        default("openai", "openai", "https://api.openai.com/v1", "OPENAI_API_KEY");
        Ok(cfg)
    }

    /// Models for the picker: the default first, then `models`.
    pub fn model_list(&self) -> Vec<String> {
        let mut v = vec![self.model.clone()];
        v.extend(self.models.iter().filter(|m| **m != self.model).cloned());
        v
    }
}
