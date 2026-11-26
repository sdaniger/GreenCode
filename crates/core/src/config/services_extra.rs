//! Service routing, saved-reset policies, secret hiding and OTLP export (OMP's `Services` / `Privacy`
//! rows); flattened into `Config`.
use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Deserialize, Serialize, Clone, Debug, Default, PartialEq)]
#[serde(default)]
pub struct ServicesExtraOpts {
    /// `[auth]`: providers an earlier risk pause turned off.
    pub auth: AuthCfg,
    pub antigravity_endpoint: AntigravityEndpoint,
    pub fetch_provider: FetchProvider,
    pub web_keyless: Keyless,
    pub codex_resets: ResetPolicy,
    pub claude_resets: ResetPolicy,
    pub secrets: SecretsCfg,
    pub telemetry: TelemetryCfg,
}

#[derive(Deserialize, Serialize, Clone, Debug, Default, PartialEq)]
#[serde(default)]
pub struct AuthCfg {
    /// Providers whose account-risk pause was lifted: login, token refresh and stored credentials work again.
    pub unpaused_providers: Vec<String>,
}

#[derive(Deserialize, Serialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum AntigravityEndpoint {
    /// Production first; the sandbox endpoint takes over on 5xx/429.
    #[default]
    Auto,
    Production,
    Sandbox,
}

/// Reader backend for `web_fetch`; `auto` tries the whole priority chain.
#[derive(Deserialize, Serialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum FetchProvider {
    #[default]
    Auto,
    Native,
    Trafilatura,
    Lynx,
    Parallel,
    Firecrawl,
    Jina,
}

impl FetchProvider {
    /// The backends to try, in order.
    pub fn chain(self) -> &'static [FetchProvider] {
        use FetchProvider::*;
        const AUTO: &[FetchProvider] = &[Native, Trafilatura, Lynx, Parallel, Firecrawl, Jina];
        const ONE_NATIVE: &[FetchProvider] = &[Native];
        const ONE_TRAFILATURA: &[FetchProvider] = &[Trafilatura];
        const ONE_LYNX: &[FetchProvider] = &[Lynx];
        const ONE_PARALLEL: &[FetchProvider] = &[Parallel];
        const ONE_FIRECRAWL: &[FetchProvider] = &[Firecrawl];
        const ONE_JINA: &[FetchProvider] = &[Jina];
        match self {
            Auto => AUTO,
            Native => ONE_NATIVE,
            Trafilatura => ONE_TRAFILATURA,
            Lynx => ONE_LYNX,
            Parallel => ONE_PARALLEL,
            Firecrawl => ONE_FIRECRAWL,
            Jina => ONE_JINA,
        }
    }
}

/// OMP's three-way spend gate for keyless third-party web services.
#[derive(Deserialize, Serialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Keyless {
    /// Ask before spending; there is no prompt path for web requests, so they are not made.
    Unset,
    #[default]
    Yes,
    No,
}

impl Keyless {
    /// Whether a request that needs no configured credential may run.
    pub fn allowed(self) -> bool { self == Keyless::Yes }
}

#[derive(Deserialize, Serialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct ResetPolicy {
    /// Only auto-redeem when the natural unblock is at least this many minutes away.
    pub min_blocked_minutes: u64,
    /// Never auto-spend below this many saved resets; credits about to expire are exempt.
    pub keep_credits: u64,
    /// Salvage a reset within this many hours of expiry; 0 disables early salvage.
    pub salvage_horizon_hours: u64,
}

impl Default for ResetPolicy {
    fn default() -> Self { Self { min_blocked_minutes: 60, keep_credits: 0, salvage_horizon_hours: 12 } }
}

#[derive(Deserialize, Serialize, Clone, Debug, Default, PartialEq)]
#[serde(default)]
pub struct SecretsCfg {
    /// Obfuscate configured secrets and redact credential-shaped tokens before sending to providers.
    pub enabled: bool,
}

#[derive(Deserialize, Serialize, Clone, Debug, Default, PartialEq)]
#[serde(default)]
pub struct TelemetryCfg {
    /// Export traces, logs and metrics to the OTEL_* endpoints.
    pub otlp_export_enabled: bool,
}

impl ServicesExtraOpts {
    pub fn validate(&self) -> Result<()> {
        if self.auth.unpaused_providers.iter().any(|p| p.trim().is_empty()) {
            return Err(anyhow!("auth.unpaused_providers entries must not be empty"));
        }
        Ok(())
    }
}

/// OMP key → native dotted key; `config import-omp` copies the value verbatim unless `convert` handles it.
pub(super) const OMP: &[(&str, &str)] = &[
    ("auth.unpausedProviders", "auth.unpaused_providers"),
    ("providers.antigravityEndpoint", "antigravity_endpoint"),
    ("providers.fetch", "fetch_provider"),
    ("providers.webKeyless", "web_keyless"),
    ("codexResets.minBlockedMinutes", "codex_resets.min_blocked_minutes"),
    ("codexResets.keepCredits", "codex_resets.keep_credits"),
    ("codexResets.salvageHorizonHours", "codex_resets.salvage_horizon_hours"),
    ("claudeResets.minBlockedMinutes", "claude_resets.min_blocked_minutes"),
    ("claudeResets.keepCredits", "claude_resets.keep_credits"),
    ("claudeResets.salvageHorizonHours", "claude_resets.salvage_horizon_hours"),
    ("secrets.enabled", "secrets.enabled"),
    ("telemetry.otlpExportEnabled", "telemetry.otlp_export_enabled"),
];

/// Translates an OMP value whose shape or units differ from the native field; `None` copies it verbatim.
pub(super) fn convert(from: &str, value: &Value) -> Option<Result<toml::Value>> {
    match from {
        // OMP stores a boolean; older files may hold the yes/no/unset spelling the catalog lists.
        "providers.webKeyless" => Some(match value {
            Value::Bool(true) => Ok(toml::Value::String("yes".into())),
            Value::Bool(false) => Ok(toml::Value::String("no".into())),
            Value::String(s) if matches!(s.as_str(), "unset" | "yes" | "no") => Ok(toml::Value::String(s.clone())),
            other => Err(anyhow!("{from} must be true, false, unset, yes or no, got {other}")),
        }),
        "auth.unpausedProviders" => Some(
            value
                .as_array()
                .ok_or_else(|| anyhow!("{from} must be a list of provider ids"))
                .and_then(|list| {
                    let mut out = Vec::with_capacity(list.len());
                    for item in list {
                        let name = item.as_str().map(str::trim).filter(|s| !s.is_empty())
                            .ok_or_else(|| anyhow!("{from} entries must be provider ids"))?;
                        out.push(toml::Value::String(name.into()));
                    }
                    Ok(toml::Value::Array(out))
                }),
        ),
        _ if from.ends_with("Minutes") || from.ends_with("Credits") || from.ends_with("Hours") => Some(
            value
                .as_u64()
                .map(|n| toml::Value::Integer(n as i64))
                .ok_or_else(|| anyhow!("{from} must be a non-negative whole number")),
        ),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    #[test]
    fn omp_rows_import_under_native_keys() {
        let yaml = "\
auth:
  unpausedProviders: [anthropic, chatgpt]
providers:
  antigravityEndpoint: sandbox
  fetch: lynx
  webKeyless: false
codexResets:
  minBlockedMinutes: 90
  keepCredits: 1
  salvageHorizonHours: 6
claudeResets:
  minBlockedMinutes: 120
  keepCredits: 2
  salvageHorizonHours: 0
secrets:
  enabled: true
telemetry:
  otlpExportEnabled: true
";
        let table = crate::config::settings::omp::translate(yaml).unwrap();
        let cfg = crate::config::settings::decode(toml::Value::Table(table)).unwrap();
        let o = &cfg.services_extra_opts;
        assert_eq!(o.auth.unpaused_providers, ["anthropic", "chatgpt"]);
        assert_eq!(o.antigravity_endpoint, AntigravityEndpoint::Sandbox);
        assert_eq!(o.fetch_provider, FetchProvider::Lynx);
        assert_eq!(o.web_keyless, Keyless::No);
        assert_eq!(o.codex_resets, ResetPolicy { min_blocked_minutes: 90, keep_credits: 1, salvage_horizon_hours: 6 });
        assert_eq!(o.claude_resets, ResetPolicy { min_blocked_minutes: 120, keep_credits: 2, salvage_horizon_hours: 0 });
        assert!(o.secrets.enabled && o.telemetry.otlp_export_enabled);
    }

    #[test]
    fn defaults_and_choices_are_omp_s() {
        let cfg = Config::default();
        let o = &cfg.services_extra_opts;
        assert_eq!(o.antigravity_endpoint, AntigravityEndpoint::Auto);
        assert_eq!(o.fetch_provider, FetchProvider::Auto);
        assert_eq!(o.web_keyless, Keyless::Yes);
        assert_eq!(o.codex_resets, ResetPolicy::default());
        assert_eq!(o.claude_resets, ResetPolicy::default());
        let tree = serde_json::to_value(&cfg).unwrap();
        for key in ["fetch_provider", "web_keyless", "antigravity_endpoint", "secrets", "telemetry", "codex_resets", "claude_resets", "auth"] {
            assert!(tree.get(key).is_some(), "{key}");
        }
        // The catalog spellings deserialize; anything else is rejected.
        for text in ["auto", "native", "trafilatura", "lynx", "parallel", "firecrawl", "jina"] {
            let v: FetchProvider = serde_json::from_value(Value::String(text.into())).unwrap();
            // `auto` walks every reader starting with the built-in one; a named reader is its own chain.
            let first = if v == FetchProvider::Auto { FetchProvider::Native } else { v };
            assert_eq!(v.chain().first(), Some(&first));
        }
        assert!(serde_json::from_value::<FetchProvider>(Value::String("curl".into())).is_err());
        for text in ["unset", "yes", "no"] {
            serde_json::from_value::<Keyless>(Value::String(text.into())).unwrap();
        }
        assert!(serde_json::from_value::<Keyless>(Value::String("maybe".into())).is_err());
        assert_eq!(FetchProvider::Auto.chain().len(), 6);
    }

    #[test]
    fn imports_accept_omp_spellings_but_reject_bad_units() {
        let decode = |text: &str| crate::config::settings::decode(toml::Value::Table(crate::config::settings::omp::translate(text)?));
        assert_eq!(decode("providers:\n  webKeyless: true\n").unwrap().services_extra_opts.web_keyless, Keyless::Yes);
        assert_eq!(decode("providers.webKeyless: no\n").unwrap().services_extra_opts.web_keyless, Keyless::No);
        assert!(decode("providers.webKeyless: maybe\n").is_err());
        assert!(decode("codexResets:\n  minBlockedMinutes: -1\n").is_err());
        assert!(decode("codexResets:\n  keepCredits: 1.5\n").is_err());
        assert!(decode("auth:\n  unpausedProviders: [\" \"]\n").is_err());
        assert_eq!(decode("providers:\n  fetch: jina\n").unwrap().services_extra_opts.fetch_provider, FetchProvider::Jina);
    }
}
