//! Secret obfuscation for `/share` snapshots and streamed rows.
//!
//! Values from the environment and `~/.greencode/secrets` are replaced literally first (longest first so no
//! value is partially masked), then the built-in credential shapes and the user's extra patterns.
use anyhow::Result;
use fancy_regex::Regex;
use serde_json::Value;
use std::collections::BTreeMap;

const BUILTIN: &[&str] = &[
    r"sk-ant-[A-Za-z0-9_-]{20,}",
    r"sk-[A-Za-z0-9_-]{20,}",
    r"gh[pousr]_[A-Za-z0-9]{20,}",
    r"github_pat_[A-Za-z0-9_]{20,}",
    r"xox[baprs]-[A-Za-z0-9-]{10,}",
    r"AKIA[0-9A-Z]{16}",
    r"AIza[0-9A-Za-z_-]{35}",
    r"eyJ[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}",
    r"-----BEGIN [A-Z ]*PRIVATE KEY-----[\s\S]*?-----END [A-Z ]*PRIVATE KEY-----",
    r"(?i)bearer\s+[A-Za-z0-9._~+/=-]{16,}",
    r#"(?i)(api[_-]?key|access[_-]?token|client[_-]?secret|password|passwd)\s*[:=]\s*["']?[A-Za-z0-9._~+/=-]{8,}"#,
];

/// True for environment names whose value is a credential (`OPENAI_API_KEY`, `GITHUB_TOKEN`, ...).
fn credential_env(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    ["KEY", "TOKEN", "SECRET", "PASSWORD", "CREDENTIAL"].iter().any(|part| upper.ends_with(part) || upper.contains(&format!("_{part}_")))
}

pub struct Redactor {
    builtin: Vec<Regex>,
    extra: Vec<Regex>,
    /// Literal secret value → the label substituted for it.
    values: Vec<(String, String)>,
}

impl Redactor {
    /// Built-in shapes, `secrets.yml`-equivalent stored secrets, credential-like environment values and
    /// `extra` regular expressions.
    pub fn new(extra: &[String]) -> Result<Self> {
        let mut values: BTreeMap<String, String> = BTreeMap::new();
        for (name, value) in crate::secrets::env() {
            let value = value.trim();
            if value.len() >= 8 {
                values.entry(value.to_string()).or_insert(name);
            }
        }
        for (name, value) in std::env::vars() {
            let value = value.trim();
            if credential_env(&name) && value.len() >= 8 {
                values.entry(value.to_string()).or_insert(name);
            }
        }
        let mut values: Vec<(String, String)> = values.into_iter().collect();
        // Longest first: a secret that contains another one must be replaced whole.
        values.sort_by(|a, b| b.0.len().cmp(&a.0.len()));
        Ok(Self {
            builtin: BUILTIN.iter().map(|p| Regex::new(p)).collect::<std::result::Result<_, _>>()?,
            extra: extra.iter().map(|p| Regex::new(p)).collect::<std::result::Result<_, _>>()?,
            values,
        })
    }

    /// The configured extra patterns plus the stored and environment credentials.
    pub fn from_config(cfg: &crate::config::Config) -> Result<Self> {
        Self::new(&cfg.collab_opts.stream.redact_patterns)
    }

    pub fn text(&self, input: &str) -> String {
        let mut out = input.to_string();
        for (value, label) in &self.values {
            if out.contains(value.as_str()) {
                out = out.replace(value.as_str(), &format!("[redacted:{label}]"));
            }
        }
        for pattern in self.builtin.iter().chain(&self.extra) {
            out = pattern.replace_all(&out, "[redacted]").into_owned();
        }
        out
    }

    /// Strings inside a JSON value; object keys are kept so the shape stays readable.
    pub fn json(&self, value: &mut Value) {
        match value {
            Value::String(s) => *s = self.text(s),
            Value::Array(items) => items.iter_mut().for_each(|item| self.json(item)),
            Value::Object(map) => map.values_mut().for_each(|item| self.json(item)),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_shapes_and_extra_patterns_are_redacted() {
        let redactor = Redactor::new(&[r"internal-[0-9]{4}".into()]).unwrap();
        let text = redactor.text("key sk-abcdefghijklmnopqrstuvwxyz and internal-1234; code internal-99 stays");
        assert_eq!(text, "key [redacted] and [redacted]; code internal-99 stays");
        let json = redactor.text("Authorization: Bearer abcdefghijklmnopqrstuvwxyz");
        assert!(!json.contains("abcdefghijklmnop"), "{json}");
    }

    #[test]
    fn stored_secret_values_win_over_shapes_in_json_values_only() {
        let redactor = Redactor { builtin: vec![], extra: vec![], values: vec![("hunter2-secret".into(), "DEMO_TOKEN".into())] };
        let mut value = serde_json::json!({"hunter2-secret": "say hunter2-secret twice hunter2-secret"});
        redactor.json(&mut value);
        assert_eq!(value["hunter2-secret"], "say [redacted:DEMO_TOKEN] twice [redacted:DEMO_TOKEN]");
    }

    #[test]
    fn environment_credentials_are_collected() {
        // The test binary's own name would be arbitrary; use a variable this process definitely owns.
        unsafe { std::env::set_var("GREENCODE_REDACT_TEST_KEY", "value-that-must-vanish") };
        let redactor = Redactor::new(&[]).unwrap();
        assert_eq!(redactor.text("x value-that-must-vanish y"), "x [redacted:GREENCODE_REDACT_TEST_KEY] y");
        unsafe { std::env::remove_var("GREENCODE_REDACT_TEST_KEY") };
    }
}
