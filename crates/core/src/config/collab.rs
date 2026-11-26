//! Settings for `/collab`, `/share` and live streams; flattened into `Config`.
use anyhow::{ensure, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Deserialize, Serialize, Clone, Debug, Default, PartialEq)]
#[serde(default)]
pub struct CollabOpts {
    pub collab: CollabCfg,
    pub share: ShareCfg,
    pub stream: StreamCfg,
}

#[derive(Deserialize, Serialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct CollabCfg {
    /// Relay used by `/collab`: `wss://host[:port]`, `ws://`, `https://` or `http://`.
    pub relay_url: Option<String>,
    /// Browser UI base; empty derives it from `relay_url`. An explicit `http://` URL must be loopback.
    pub web_url: Option<String>,
    /// Name shown to other participants; unset uses the OS username.
    pub display_name: Option<String>,
    /// Host every interactive session on start: off, view-only or control.
    pub auto_start: AutoStart,
}

impl Default for CollabCfg {
    fn default() -> Self {
        Self { relay_url: None, web_url: None, display_name: None, auto_start: AutoStart::Off }
    }
}

#[derive(Deserialize, Serialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum AutoStart {
    #[default]
    Off,
    View,
    Control,
}

#[derive(Deserialize, Serialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct ShareCfg {
    /// Viewer/upload base used by `/share`; links are `<base>/<id>#<key>`.
    pub server_url: Option<String>,
    pub store: ShareStore,
    /// Obfuscate secret values in the snapshot before it is encrypted and uploaded.
    pub redact_secrets: bool,
}

impl Default for ShareCfg {
    fn default() -> Self {
        Self { server_url: None, store: ShareStore::Blob, redact_secrets: true }
    }
}

#[derive(Deserialize, Serialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ShareStore {
    #[default]
    Blob,
    Gist,
}

#[derive(Deserialize, Serialize, Clone, Debug, Default, PartialEq)]
#[serde(default)]
pub struct StreamCfg {
    /// Live stream server base (`https://host[:port]`; `http://` is allowed for loopback).
    pub server_url: Option<String>,
    /// Extra regular expressions redacted from every streamed row.
    pub redact_patterns: Vec<String>,
}

impl CollabOpts {
    /// Rejects settings that would otherwise fail (or silently do nothing) at first use.
    pub fn validate(&self) -> Result<()> {
        for (name, url) in [
            ("collab.relay_url", &self.collab.relay_url),
            ("share.server_url", &self.share.server_url),
            ("stream.server_url", &self.stream.server_url),
        ] {
            if let Some(url) = url.as_deref().filter(|u| !u.trim().is_empty()) {
                crate::collab::http_base(url).map_err(|e| anyhow::anyhow!("{name}: {e:#}"))?;
            }
        }
        if let Some(url) = self.collab.web_url.as_deref().filter(|u| !u.trim().is_empty()) {
            crate::collab::web_base(self.collab.relay_url.as_deref(), Some(url))
                .map_err(|e| anyhow::anyhow!("collab.web_url: {e:#}"))?;
        }
        ensure!(
            self.collab.auto_start == AutoStart::Off || self.collab.relay_url.as_deref().is_some_and(|u| !u.trim().is_empty()),
            "collab.auto_start needs collab.relay_url"
        );
        for pattern in &self.stream.redact_patterns {
            fancy_regex::Regex::new(pattern).map_err(|e| anyhow::anyhow!("stream.redact_patterns: invalid regex `{pattern}`: {e}"))?;
        }
        Ok(())
    }
}

/// OMP key → native dotted key; values are copied verbatim.
pub(super) const OMP: &[(&str, &str)] = &[
    ("collab.relayUrl", "collab.relay_url"),
    ("collab.webUrl", "collab.web_url"),
    ("collab.displayName", "collab.display_name"),
    ("collab.autoStart", "collab.auto_start"),
    ("share.serverUrl", "share.server_url"),
    ("share.store", "share.store"),
    ("share.redactSecrets", "share.redact_secrets"),
    ("stream.serverUrl", "stream.server_url"),
    ("stream.redactPatterns", "stream.redact_patterns"),
];

pub(super) fn convert(_from: &str, _value: &Value) -> Option<Result<toml::Value>> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    #[test]
    fn defaults_and_enums_survive_json() {
        let tree = serde_json::to_value(Config::default()).unwrap();
        assert_eq!(tree["collab"]["auto_start"], "off");
        assert_eq!(tree["share"]["store"], "blob");
        assert_eq!(tree["share"]["redact_secrets"], true);
        assert_eq!(tree["stream"]["redact_patterns"], serde_json::json!([]));
        let cfg: Config = toml::from_str(
            "collab.relay_url = \"wss://relay.example.com\"\ncollab.auto_start = \"control\"\nshare.store = \"gist\"\nstream.redact_patterns = [\"sk-[a-z]+\"]\n",
        )
        .unwrap();
        assert_eq!(cfg.collab_opts.collab.auto_start, AutoStart::Control);
        assert_eq!(cfg.collab_opts.share.store, ShareStore::Gist);
        assert!(cfg.collab_opts.validate().is_ok());
        assert!(toml::from_str::<Config>("collab.auto_start = \"host\"").is_err(), "only off|view|control");
    }

    #[test]
    fn validation_rejects_what_use_would() {
        let mut opts = CollabOpts::default();
        opts.collab.auto_start = AutoStart::View;
        assert!(opts.validate().is_err(), "auto start needs a relay");
        opts.collab.relay_url = Some("relay.example.com".into());
        assert!(opts.validate().is_err(), "a relay needs an http(s) or ws(s) scheme");
        opts.collab.relay_url = Some("wss://relay.example.com".into());
        opts.collab.web_url = Some("http://relay.example.com".into());
        assert!(opts.validate().is_err(), "remote http web UI would send room ids in clear");
        opts.collab.web_url = Some("http://127.0.0.1:8080".into());
        assert!(opts.validate().is_ok());
        opts.stream.redact_patterns = vec!["(".into()];
        assert!(opts.validate().is_err());
    }

    #[test]
    fn omp_rows_import() {
        let yaml = "\
collab:
  relayUrl: wss://relay.example.com
  webUrl: https://relay.example.com/ui
  displayName: Ada
  autoStart: control
share:
  serverUrl: https://share.example.com
  store: gist
  redactSecrets: false
stream:
  serverUrl: https://stream.example.com
  redactPatterns: [\"secret-[0-9]+\"]
skills:
  registryUrl: https://skills.example.com
";
        let layer = crate::config::settings::translate(yaml).unwrap();
        let cfg = crate::config::settings::decode(toml::Value::Table(layer)).unwrap();
        assert_eq!(cfg.collab_opts.collab.relay_url.as_deref(), Some("wss://relay.example.com"));
        assert_eq!(cfg.collab_opts.collab.web_url.as_deref(), Some("https://relay.example.com/ui"));
        assert_eq!(cfg.collab_opts.collab.display_name.as_deref(), Some("Ada"));
        assert_eq!(cfg.collab_opts.collab.auto_start, AutoStart::Control);
        assert_eq!(cfg.collab_opts.share.store, ShareStore::Gist);
        assert!(!cfg.collab_opts.share.redact_secrets);
        assert_eq!(cfg.collab_opts.stream.redact_patterns, ["secret-[0-9]+"]);
        assert_eq!(cfg.integration_opts.skills.registry_url.as_deref(), Some("https://skills.example.com"));
    }
}
