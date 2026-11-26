//! Settings for the extra tools (OMP's Services, Generate Image, Ratchet, Archive, Obsidian Vault and
//! Security rows); flattened into `Config`.
use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Deserialize, Serialize, Clone, Debug, Default, PartialEq)]
#[serde(default)]
pub struct ExtraToolsOpts {
    pub launch: LaunchCfg,
    pub generate_image: GenerateImageCfg,
    pub ratchet: OnOff,
    pub archive: OnOff,
    pub vault: VaultCfg,
    pub security: OnOff,
}

/// A tool switch with no other settings.
#[derive(Deserialize, Serialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(default)]
pub struct OnOff {
    pub enabled: bool,
}

impl Default for OnOff {
    fn default() -> Self {
        Self { enabled: true }
    }
}

/// Named `bash` services and `proc://` supervision.
#[derive(Deserialize, Serialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(default)]
pub struct LaunchCfg {
    pub enabled: bool,
}

impl Default for LaunchCfg {
    fn default() -> Self {
        Self { enabled: true }
    }
}

/// Text-to-image generation and editing through an OpenAI-compatible images API.
#[derive(Deserialize, Serialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct GenerateImageCfg {
    pub enabled: bool,
    /// Configured provider whose `base_url` serves `/images/generations` and `/images/edits`.
    pub provider: String,
    pub model: String,
    /// `WxH` sent when the call names no size.
    pub size: String,
}

impl Default for GenerateImageCfg {
    fn default() -> Self {
        // Off like OMP: the tool needs an images provider, and every advertised tool costs prompt bytes.
        Self { enabled: false, provider: "openai".into(), model: "gpt-image-1".into(), size: "1024x1024".into() }
    }
}

/// `vault://` over an Obsidian vault.
#[derive(Deserialize, Serialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct VaultCfg {
    pub enabled: bool,
    /// Vault root; empty discovers the open vault from Obsidian's own `obsidian.json`.
    pub path: String,
    /// Obsidian CLI for `open`; empty tries `obsidian`, then `obsidian-cli`.
    pub cli: String,
}

impl Default for VaultCfg {
    fn default() -> Self {
        Self { enabled: true, path: String::new(), cli: String::new() }
    }
}

impl ExtraToolsOpts {
    /// Ext tools the settings turn off; they leave the catalog and are refused at call time.
    pub fn off(&self) -> Vec<String> {
        [
            ("ratchet", self.ratchet.enabled),
            ("archive", self.archive.enabled),
            ("vault", self.vault.enabled),
            ("security", self.security.enabled),
        ]
        .iter()
        .filter(|(_, on)| !on)
        .map(|(name, _)| name.to_string())
        .collect()
    }
}

impl VaultCfg {
    /// The configured vault root, or the one Obsidian has open.
    pub fn root(&self) -> Result<std::path::PathBuf> {
        use anyhow::{bail, Context};
        if !self.path.trim().is_empty() {
            let path = match self.path.strip_prefix("~/") {
                Some(rest) => crate::user_home().join(rest),
                None => std::path::PathBuf::from(self.path.trim()),
            };
            anyhow::ensure!(path.is_dir(), "vault.path is not a directory: {}", path.display());
            return Ok(path);
        }
        let dir = if cfg!(target_os = "macos") {
            crate::user_home().join("Library/Application Support/obsidian")
        } else if cfg!(windows) {
            std::env::var_os("APPDATA").map(std::path::PathBuf::from).unwrap_or_default().join("obsidian")
        } else {
            std::env::var_os("XDG_CONFIG_HOME")
                .map(std::path::PathBuf::from)
                .unwrap_or_else(|| crate::user_home().join(".config"))
                .join("obsidian")
        };
        let text = std::fs::read_to_string(dir.join("obsidian.json"))
            .with_context(|| format!("no vault.path and no Obsidian install at {}", dir.display()))?;
        let json: Value = serde_json::from_str(&text).context("cannot parse obsidian.json")?;
        let mut vaults: Vec<&Value> = json["vaults"].as_object().map(|v| v.values().collect()).unwrap_or_default();
        vaults.sort_by_key(|v| std::cmp::Reverse(v["ts"].as_u64().unwrap_or(0)));
        let path = vaults
            .iter()
            .find_map(|v| v["path"].as_str())
            .ok_or_else(|| anyhow::anyhow!("obsidian.json lists no vault; set vault.path"))?;
        if !std::path::Path::new(path).is_dir() {
            bail!("Obsidian vault {path} is missing; set vault.path");
        }
        Ok(std::path::PathBuf::from(path))
    }
}

/// OMP key → native dotted key; `config import-omp` copies the value verbatim.
pub(super) const OMP: &[(&str, &str)] = &[
    ("launch.enabled", "launch.enabled"),
    ("generate_image.enabled", "generate_image.enabled"),
    ("ratchet.enabled", "ratchet.enabled"),
    ("archive.enabled", "archive.enabled"),
    ("vault.enabled", "vault.enabled"),
    ("security.enabled", "security.enabled"),
];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    #[test]
    fn omp_rows_import_under_native_keys() {
        let yaml = "launch: {enabled: false}\ngenerate_image: {enabled: true}\nratchet: {enabled: false}\narchive: {enabled: false}\nvault: {enabled: false}\nsecurity: {enabled: false}\n";
        let table = crate::config::settings::omp::translate(yaml).unwrap();
        let cfg = crate::config::settings::decode(toml::Value::Table(table)).unwrap();
        let o = &cfg.extra_tools_opts;
        assert!(!o.launch.enabled && o.generate_image.enabled && !o.ratchet.enabled);
        assert!(!o.archive.enabled && !o.vault.enabled && !o.security.enabled);
    }

    #[test]
    fn defaults_match_omp() {
        let tree = serde_json::to_value(Config::default()).unwrap();
        for key in ["launch.enabled", "ratchet.enabled", "archive.enabled", "vault.enabled", "security.enabled"] {
            let mut node = &tree;
            for seg in key.split('.') {
                node = node.get(seg).unwrap_or_else(|| panic!("missing {key}"));
            }
            assert_eq!(node, &Value::Bool(true), "{key}");
        }
        let d = Config::default().extra_tools_opts.generate_image;
        assert!(!d.enabled, "generate_image is opt-in");
        assert_eq!((d.provider.as_str(), d.model.as_str(), d.size.as_str()), ("openai", "gpt-image-1", "1024x1024"));
    }

    #[test]
    fn explicit_vault_path_must_exist() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = VaultCfg { path: dir.path().to_string_lossy().into_owned(), ..VaultCfg::default() };
        assert_eq!(cfg.root().unwrap(), dir.path());
        cfg.path = dir.path().join("gone").to_string_lossy().into_owned();
        assert!(cfg.root().is_err());
    }
}
