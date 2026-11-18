//! Machine-managed settings and capability limits.
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use crate::permission::Mode;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Setting<T> { pub value: T, pub locked: bool }

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
pub struct Settings {
    pub default_mode: Option<Setting<Mode>>,
    pub sandbox: Option<Setting<bool>>,
    pub suggest_next: Option<Setting<bool>>,
    pub voice_provider: Option<Setting<VoiceProvider>>,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum VoiceProvider { Off, Openai, Groq }

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
pub struct Limits {
    pub max_mode: Option<Mode>,
    pub providers: Option<Vec<String>>,
    pub backends: Option<Vec<String>>,
    pub webhooks: Option<bool>,
    pub telemetry: Option<bool>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Policy {
    pub schema_version: u32,
    pub organization: String,
    pub support_url: Option<String>,
    #[serde(default)] pub settings: Settings,
    #[serde(default)] pub limits: Limits,
}

impl Policy {
    pub fn parse(text: &str) -> Result<Self> {
        let value: serde_json::Value = serde_json::from_str(text).context("invalid policy JSON")?;
        let mut unknown = Vec::new();
        for (path, allowed) in [
            ("", &["schemaVersion", "organization", "supportUrl", "settings", "limits"][..]),
            ("settings", &["defaultMode", "sandbox", "suggestNext", "voiceProvider"][..]),
            ("limits", &["maxMode", "providers", "backends", "webhooks", "telemetry"][..]),
            ("settings.defaultMode", &["value", "locked"][..]),
            ("settings.sandbox", &["value", "locked"][..]),
            ("settings.suggestNext", &["value", "locked"][..]),
            ("settings.voiceProvider", &["value", "locked"][..]),
        ] {
            let mut node = &value;
            for part in path.split('.').filter(|part| !part.is_empty()) { node = &node[part]; }
            if let Some(object) = node.as_object() {
                for key in object.keys().filter(|key| !allowed.contains(&key.as_str())) {
                    unknown.push(if path.is_empty() { key.clone() } else { format!("{path}.{key}") });
                }
            }
        }
        ensure!(unknown.is_empty(), "unknown policy keys: {}", unknown.join(", "));
        let p: Self = serde_json::from_str(text).context("invalid policy")?;
        ensure!(p.schema_version == 1, "unsupported policy schemaVersion {}", p.schema_version);
        ensure!(!p.organization.trim().is_empty(), "policy organization must not be empty");
        ensure!(p.limits.max_mode != Some(Mode::Plan), "maxMode must be ask, auto-edit or full");
        if let Some(url) = &p.support_url { ensure!(url.starts_with("https://") || url.starts_with("http://"), "supportUrl must be an HTTP(S) URL"); }
        if let Some(s) = &p.settings.default_mode { p.check_mode(s.value)?; }
        Ok(p)
    }
    pub fn check_mode(&self, mode: Mode) -> Result<()> {
        fn rank(m: Mode) -> u8 { match m { Mode::Plan => 0, Mode::Ask => 1, Mode::AutoEdit => 2, Mode::Full => 3 } }
        if self.limits.max_mode.is_some_and(|max| rank(mode) > rank(max)) { bail!("管理ポリシーで制限されています（{}）", self.organization); }
        Ok(())
    }
    pub fn provider_allowed(&self, name: &str) -> bool { self.limits.providers.as_ref().is_none_or(|v| v.iter().any(|s| s == name)) }
    pub fn backend_allowed(&self, name: &str) -> bool { name == "native" || self.limits.backends.as_ref().is_none_or(|v| v.iter().any(|s| s == name)) }
    pub fn summary(&self) -> String { format!("Managed policy: {}\n{}", self.organization, serde_json::to_string_pretty(self).expect("policy serialization")) }
}

pub fn load() -> Result<Option<Policy>> {
    let path = std::env::var_os("GREENCODE_POLICY").map(std::path::PathBuf::from).unwrap_or_else(|| "/Library/Application Support/GreenCode/policy.json".into());
    match std::fs::read_to_string(&path) {
        Ok(text) => Policy::parse(&text).with_context(|| format!("{}", path.display())).map(Some),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("{}", path.display())),
    }
}

pub fn check(text: &str) -> Result<String> { Ok(Policy::parse(text)?.summary()) }

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn policy_parse_and_modes() {
        let p = Policy::parse(r#"{"schemaVersion":1,"organization":"Example","limits":{"maxMode":"auto-edit"}}"#).unwrap();
        assert!(p.check_mode(Mode::Plan).is_ok());
        assert!(p.check_mode(Mode::AutoEdit).is_ok());
        assert!(p.check_mode(Mode::Full).is_err());
        assert!(Policy::parse(r#"{"schemaVersion":2,"organization":"X"}"#).is_err());
        assert!(Policy::parse(r#"{"schemaVersion":1,"organization":"X","limits":{"unknown":true}}"#).unwrap_err().to_string().contains("limits.unknown"));
        assert!(Policy::parse(r#"{"schemaVersion":1}"#).is_err());
    }
}
