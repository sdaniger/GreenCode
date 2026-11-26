//! AWS credentials for Bedrock: environment keys, shared `~/.aws` profiles (static keys or
//! `credential_process`), and the ambient region/profile selection the AWS SDK uses.
//!
//! Not ported from OMP: SSO, `role_arn` chaining, web identity, ECS and EC2 IMDS. Each needs a
//! network exchange or an SSO cache of its own; the error names what to configure instead.
use super::aws_sigv4::Credentials;
use anyhow::{anyhow, bail, Result};
use std::{collections::BTreeMap, path::Path, path::PathBuf, time::Duration};

/// Session-token keys in the credentials file are rotated in place by external tools, so re-read them.
const FILE_SESSION_TTL: Duration = Duration::from_secs(5 * 60);

struct Resolved {
    credentials: Credentials,
    /// `None` = valid for the process (static keys); otherwise a unix-ms deadline.
    expires: Option<u128>,
}

/// The selected shared-credentials profile; `AWS_PROFILE` wins over `default`.
pub fn profile(explicit: Option<&str>) -> String {
    let from_env = std::env::var("AWS_PROFILE").ok().filter(|p| !p.is_empty());
    explicit.map(str::to_string).or(from_env).unwrap_or_else(|| "default".into())
}

/// An explicit profile (or `AWS_PROFILE`) opts the shared config file in; otherwise only
/// `AWS_SDK_LOAD_CONFIG` does, matching the SDK.
fn load_shared_config(explicit: Option<&str>) -> bool {
    if explicit.is_some() || std::env::var("AWS_PROFILE").is_ok_and(|p| !p.is_empty()) {
        return true;
    }
    std::env::var("AWS_SDK_LOAD_CONFIG").is_ok_and(|v| v == "1" || v.eq_ignore_ascii_case("true"))
}

/// Region the environment or selected profile names, without the `us-east-1` fallback.
pub fn ambient(profile: Option<&str>) -> Option<String> {
    let from_env = std::env::var("AWS_REGION")
        .or_else(|_| std::env::var("AWS_DEFAULT_REGION"))
        .ok()
        .filter(|r| !r.is_empty());
    if from_env.is_some() {
        return from_env;
    }
    let name = self::profile(profile);
    if load_shared_config(profile) {
        return ini(&config_path()).remove(&name).and_then(|s| s.get("region").cloned());
    }
    None
}

/// Region precedence: explicit, `AWS_REGION`, `AWS_DEFAULT_REGION`, the profile's `region`, `us-east-1`.
pub fn region(explicit: Option<&str>, profile: Option<&str>) -> String {
    explicit.map(str::to_string).or_else(|| ambient(profile)).unwrap_or_else(|| "us-east-1".into())
}

fn credentials_path() -> PathBuf {
    std::env::var("AWS_SHARED_CREDENTIALS_FILE")
        .map(PathBuf::from)
        .unwrap_or_else(|_| crate::user_home().join(".aws/credentials"))
}

fn config_path() -> PathBuf {
    std::env::var("AWS_CONFIG_FILE").map(PathBuf::from).unwrap_or_else(|_| crate::user_home().join(".aws/config"))
}

/// Parses an INI file, normalizing `[profile x]` and `[sso-session x]` section names.
fn ini(path: &Path) -> BTreeMap<String, BTreeMap<String, String>> {
    let Ok(text) = std::fs::read_to_string(path) else { return BTreeMap::new() };
    let mut out: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
    let mut current: Option<String> = None;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')).map(str::trim) {
            let name = match name.strip_prefix("profile ").map(str::trim) {
                Some(name) => name.to_string(),
                None => name.strip_prefix("sso-session ").map_or_else(|| name.to_string(), |s| format!("sso-session:{}", s.trim())),
            };
            current = Some(name);
            continue;
        }
        if let (Some(section), Some((key, value))) = (&current, line.split_once('=')) {
            out.entry(section.clone()).or_default().insert(key.trim().to_string(), value.trim().to_string());
        }
    }
    out
}

/// Process-wide cache keyed by profile, so `credential_process` helpers are not spawned per request.
static CACHE: std::sync::LazyLock<std::sync::Mutex<BTreeMap<String, Resolved>>> =
    std::sync::LazyLock::new(Default::default);

/// Drops cached credentials; a rotated key gets one more attempt after a 401/403.
pub fn invalidate(profile: &str) {
    CACHE.lock().unwrap().remove(profile);
}

/// Resolves credentials for a request: environment keys first, then the selected profile.
pub async fn resolve(profile: Option<&str>) -> Result<Credentials> {
    let name = self::profile(profile);
    let now = unix_ms();
    if let Some(hit) = CACHE.lock().unwrap().get(&name) {
        if hit.expires.is_none_or(|expires| expires > now + 60_000) {
            return Ok(clone(&hit.credentials));
        }
    }
    let env = || -> Option<Credentials> {
        let key = std::env::var("AWS_ACCESS_KEY_ID").ok().filter(|k| !k.is_empty())?;
        let secret = std::env::var("AWS_SECRET_ACCESS_KEY").ok().filter(|s| !s.is_empty())?;
        Some(Credentials { access_key_id: key, secret_access_key: secret, session_token: std::env::var("AWS_SESSION_TOKEN").ok().filter(|t| !t.is_empty()) })
    };
    let resolved = match env() {
        Some(credentials) => Resolved { credentials, expires: None },
        None => from_profile(&name, profile).await?,
    };
    let credentials = clone(&resolved.credentials);
    CACHE.lock().unwrap().insert(name, resolved);
    Ok(credentials)
}

fn clone(credentials: &Credentials) -> Credentials {
    Credentials {
        access_key_id: credentials.access_key_id.clone(),
        secret_access_key: credentials.secret_access_key.clone(),
        session_token: credentials.session_token.clone(),
    }
}

/// Merged view of one profile: `~/.aws/config` first, then `~/.aws/credentials` (credentials win).
fn section(name: &str, load_config: bool) -> BTreeMap<String, String> {
    let config = if load_config { ini(&config_path()).remove(name).unwrap_or_default() } else { BTreeMap::new() };
    let mut merged = config;
    merged.extend(ini(&credentials_path()).remove(name).unwrap_or_default());
    merged
}

async fn from_profile(name: &str, explicit: Option<&str>) -> Result<Resolved> {
    let merged = section(name, load_shared_config(explicit));
    if merged.is_empty() {
        bail!(
            "AWS の認証情報が見つかりません（プロファイル `{name}`）。AWS_ACCESS_KEY_ID と AWS_SECRET_ACCESS_KEY を設定するか、`aws configure` でプロファイルを作成してください"
        );
    }
    if merged.contains_key("sso_account_id") || merged.contains_key("sso_session") {
        bail!("AWS プロファイル `{name}` は SSO です。このプロバイダーは SSO 未対応です。`aws configure export-credentials` を credential_process に設定するか、静的キーを使ってください");
    }
    if merged.contains_key("role_arn") {
        bail!("AWS プロファイル `{name}` は role_arn（ロール引き受け）です。このプロバイダーは未対応です。静的キーを使ってください");
    }
    if let (Some(key), Some(secret)) = (merged.get("aws_access_key_id"), merged.get("aws_secret_access_key")) {
        let session_token = merged.get("aws_session_token").filter(|t| !t.is_empty()).cloned();
        return Ok(Resolved {
            credentials: Credentials { access_key_id: key.clone(), secret_access_key: secret.clone(), session_token: session_token.clone() },
            expires: session_token.map(|_| unix_ms() + FILE_SESSION_TTL.as_millis()),
        });
    }
    if let Some(command) = merged.get("credential_process") {
        return credential_process(name, command).await;
    }
    bail!("AWS プロファイル `{name}` に aws_access_key_id / aws_secret_access_key がありません");
}

/// Runs the profile's `credential_process` and reads its version-1 JSON envelope.
async fn credential_process(profile: &str, command: &str) -> Result<Resolved> {
    // shortcut: the command runs through `sh -c`, so shell quoting differs slightly from the SDK's
    // argv tokenizer; upgrade when a profile needs botocore-exact splitting.
    let output = tokio::time::timeout(
        Duration::from_secs(30),
        tokio::process::Command::new("/bin/sh").arg("-c").arg(command).stdin(std::process::Stdio::null()).output(),
    )
    .await
    .map_err(|_| anyhow!("AWS credential_process（プロファイル `{profile}`）が 30 秒で応答しません"))??;
    let stdout = String::from_utf8_lossy(&output.stdout);
    if !output.status.success() {
        let tail: String = {
            let stderr = String::from_utf8_lossy(&output.stderr);
            stderr.trim().chars().rev().take(512).collect::<Vec<_>>().into_iter().rev().collect()
        };
        bail!("AWS credential_process（プロファイル `{profile}`）が失敗しました（{}）: {}", output.status.code().unwrap_or(-1), if tail.is_empty() { "(no output)" } else { &tail });
    }
    let envelope: serde_json::Value = serde_json::from_str(&stdout)
        .map_err(|e| anyhow!("AWS credential_process（プロファイル `{profile}`）が JSON を返しませんでした: {e}"))?;
    if envelope["Version"].as_u64() != Some(1) {
        bail!("AWS credential_process（プロファイル `{profile}`）の Version は 1 である必要があります");
    }
    let (Some(key), Some(secret)) = (envelope["AccessKeyId"].as_str(), envelope["SecretAccessKey"].as_str()) else {
        bail!("AWS credential_process（プロファイル `{profile}`）に AccessKeyId / SecretAccessKey がありません");
    };
    Ok(Resolved {
        credentials: Credentials {
            access_key_id: key.to_string(),
            secret_access_key: secret.to_string(),
            session_token: envelope["SessionToken"].as_str().filter(|t| !t.is_empty()).map(str::to_string),
        },
        expires: envelope["Expiration"].as_str().and_then(parse_ms),
    })
}

fn parse_ms(text: &str) -> Option<u128> {
    // `2025-01-01T00:00:00Z`; a missing or malformed value disables caching rather than failing.
    let (date, time) = text.trim_end_matches('Z').split_once('T')?;
    let mut parts = date.split('-').map(|p| p.parse::<i64>().ok());
    let (year, month, day) = (parts.next()??, parts.next()??, parts.next()??);
    let mut clock = time.split(':').map(|p| p.parse::<i64>().ok());
    let (hour, minute, second) = (clock.next()??, clock.next()??, clock.next().flatten()?);
    // Days from civil date (Howard Hinnant), enough for the second-resolution timestamps AWS emits.
    let y = year - i64::from(month <= 2);
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some((days * 86_400 + hour * 3600 + minute * 60 + second) as u128 * 1000)
}

fn unix_ms() -> u128 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ini_sections_normalize_profile_and_sso_names() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config");
        std::fs::write(&path, "# comment\n[default]\nregion = eu-west-1\n\n[profile work]\nregion=ap-northeast-1\naws_access_key_id = A\n\n[sso-session corp]\nsso_region = us-east-1\n").unwrap();
        let parsed = ini(&path);
        assert_eq!(parsed["default"]["region"], "eu-west-1");
        assert_eq!(parsed["work"]["aws_access_key_id"], "A");
        assert_eq!(parsed["work"]["region"], "ap-northeast-1");
        assert!(parsed.contains_key("sso-session:corp"), "sso-session sections get their own key");
    }

    #[test]
    fn expiration_dates_parse_to_unix_ms() {
        assert_eq!(parse_ms("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_ms("2025-07-26T14:40:00Z"), Some(1_753_540_800_000));
        assert_eq!(parse_ms("not a date"), None);
    }

    #[test]
    fn a_static_profile_resolves_and_reports_its_session_token() {
        let dir = tempfile::tempdir().unwrap();
        let credentials = dir.path().join("credentials");
        std::fs::write(&credentials, "[work]\naws_access_key_id = AKID\naws_secret_access_key = SECRET\naws_session_token = TOKEN\n").unwrap();
        let merged = {
            let config = ini(&dir.path().join("config"));
            assert!(config.is_empty(), "a missing config file reads as empty");
            ini(&credentials).remove("work").unwrap_or_default()
        };
        assert_eq!(merged["aws_access_key_id"], "AKID");
        assert_eq!(merged["aws_session_token"], "TOKEN");
    }
}
