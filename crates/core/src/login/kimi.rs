//! Kimi Code: device sign-in plus the client fingerprint headers its requests carry
//! (OMP `registry/oauth/kimi.ts`, rule `auth/kimi-code.kdl`).

use super::{device, device_start, jwt_claims, poll_every, post_form, Cred, OAuth, Start};
use anyhow::{anyhow, Result};
use serde_json::{json, Value};
use std::{
    sync::OnceLock,
    time::Duration,
};

const CLIENT_ID: &str = "17e5f671-d194-4dfb-9706-5516cb48c098";
const DEFAULT_BASE: &str = "https://auth.kimi.com";

pub async fn login() -> Result<Start> {
    login_at(&base(), Duration::from_secs(1)).await
}

async fn login_at(base: &str, unit: Duration) -> Result<Start> {
    let headers = refs(headers());
    let form = [("client_id", CLIENT_ID)];
    let device = device(&format!("{base}/api/oauth/device_authorization"), &form, &headers).await?;
    let token_url = format!("{base}/api/oauth/token");
    Ok(device_start(device, move |device| async move {
        let v = poll_every(&device, &token_url, &form, &headers, unit).await?;
        Ok(Cred::OAuth(credential(&v, None)?))
    }))
}

pub async fn refresh(cred: OAuth) -> Result<OAuth> {
    refresh_at(&base(), &cred).await
}

async fn refresh_at(base: &str, cred: &OAuth) -> Result<OAuth> {
    let refresh = cred.refresh.as_deref().filter(|r| !r.is_empty())
        .ok_or_else(|| anyhow!("Kimi Code の更新トークンがありません。/login kimi-code でもう一度ログインしてください"))?;
    let v = post_form(
        &format!("{base}/api/oauth/token"),
        &[("grant_type", "refresh_token"), ("client_id", CLIENT_ID), ("refresh_token", refresh)],
        &refs(headers()),
    )
    .await?;
    credential(&v, Some(cred))
}

fn credential(v: &Value, previous: Option<&OAuth>) -> Result<OAuth> {
    let mut cred = super::standard(v)?;
    // A refresh answer without a new refresh token keeps the old one (the service only rotates it sometimes).
    if cred.refresh.is_none() {
        cred.refresh = previous.and_then(|p| p.refresh.clone());
    }
    let kept = previous.and_then(|p| p.extra.get("account_id")).and_then(Value::as_str).map(str::to_string);
    if let Some(id) = account_id(&cred.access).or(kept) {
        cred.extra.insert("account_id".into(), json!(id));
    }
    Ok(cred)
}

/// The stable Kimi account id: `user_id` from the access token, else `sub`.
fn account_id(access: &str) -> Option<String> {
    let claims = jwt_claims(access)?;
    ["user_id", "sub"]
        .iter()
        .find_map(|key| claims.get(*key).and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_string))
}

/// Whether `base` is a Kimi endpoint, whose requests carry the same fingerprint headers as sign-in
/// (OMP's `providers/kimi.ts` adds them to every Kimi request).
pub fn endpoint(base: &str) -> bool {
    reqwest::Url::parse(base).ok().and_then(|u| u.host_str().map(str::to_ascii_lowercase))
        .is_some_and(|host| host == "kimi.com" || host.ends_with(".kimi.com"))
}

/// `KIMI_CODE_OAUTH_HOST` / `KIMI_OAUTH_HOST` move both endpoints, as OMP's `base-url` env does.
fn base() -> String {
    ["KIMI_CODE_OAUTH_HOST", "KIMI_OAUTH_HOST"]
        .iter()
        .find_map(|name| std::env::var(name).ok().filter(|v| !v.trim().is_empty()))
        .unwrap_or_else(|| DEFAULT_BASE.into())
        .trim_end_matches('/')
        .to_string()
}

/// The fingerprint headers Kimi expects on sign-in, refresh and every request.
pub fn headers() -> &'static [(&'static str, String)] {
    static HEADERS: OnceLock<Vec<(&'static str, String)>> = OnceLock::new();
    HEADERS.get_or_init(|| {
        let version = env!("CARGO_PKG_VERSION").to_string();
        vec![
            ("User-Agent", format!("KimiCLI/{version}")),
            ("X-Msh-Platform", "kimi_cli".into()),
            ("X-Msh-Version", version),
            ("X-Msh-Device-Name", clean(&uname("-n"), "unknown")),
            ("X-Msh-Device-Model", clean(&device_model(), "unknown")),
            ("X-Msh-Os-Version", clean(&uname("-v"), "unknown")),
            ("X-Msh-Device-Id", clean(&device_id(), "unknown")),
        ]
    })
}

fn refs(h: &'static [(&'static str, String)]) -> Vec<(&'static str, &'static str)> {
    h.iter().map(|(k, v)| (*k, v.as_str())).collect()
}

fn uname(flag: &str) -> String {
    std::process::Command::new("uname")
        .arg(flag)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default()
}

/// `macOS 25.0.0 arm64`-style device model, like OMP's `formatDeviceModel`.
fn device_model() -> String {
    let platform = match std::env::consts::OS {
        "macos" => "macOS",
        "windows" => "Windows",
        "linux" => "Linux",
        other => other,
    };
    [platform, &uname("-r"), std::env::consts::ARCH].iter().filter(|s| !s.is_empty()).copied().collect::<Vec<_>>().join(" ")
}

/// Identifies this install; best effort like OMP, so an unwritable home never breaks a request.
fn device_id() -> String {
    let path = crate::home().join("kimi-device-id");
    if let Ok(text) = std::fs::read_to_string(&path) {
        let id = text.trim().to_string();
        if !id.is_empty() {
            return id;
        }
    }
    let id = super::random().unwrap_or_default();
    let _ = std::fs::create_dir_all(crate::home()).and_then(|_| std::fs::write(&path, &id));
    id
}

/// Header values stay printable ASCII.
fn clean(value: &str, fallback: &str) -> String {
    let value: String = value.chars().filter(|c| (' '..='~').contains(c)).collect();
    let value = value.trim();
    if value.is_empty() { fallback.into() } else { value.into() }
}

#[cfg(test)]
mod tests {
    use super::super::mock::Mock;
    use super::*;
    use base64::Engine as _;
    use serde_json::Map;

    fn access(claims: &str) -> String {
        format!("e30.{}.x", base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(claims))
    }

    #[tokio::test]
    async fn device_flow_sends_fingerprint_headers_and_keeps_the_account_id() {
        let access = access(r#"{"user_id":"u123"}"#);
        let token = format!(r#"{{"access_token":"{access}","refresh_token":"r1","expires_in":3600}}"#);
        let mock = Mock::start(&[
            ("200 OK", r#"{"user_code":"U-1","device_code":"dc","verification_uri":"{origin}/device","interval":1,"expires_in":60}"#),
            ("200 OK", &token),
        ]).await;
        // `login_at` appends the fixed paths to the base.
        let start = login_at(&mock.origin, Duration::from_millis(1)).await.unwrap();
        assert_eq!(start.url, format!("{}/device", mock.origin));
        assert_eq!(start.code.as_deref(), Some("U-1"));
        let Cred::OAuth(cred) = start.done.await.unwrap() else { panic!("expected an oauth credential") };
        assert_eq!((cred.access.as_str(), cred.refresh.as_deref()), (access.as_str(), Some("r1")));
        assert_eq!(cred.extra["account_id"], "u123");

        let requests = mock.requests();
        assert_eq!(requests[0].path, "/api/oauth/device_authorization");
        assert_eq!(requests[0].field("client_id").as_deref(), Some(CLIENT_ID));
        assert_eq!(requests[0].header("x-msh-platform"), Some("kimi_cli"));
        assert!(requests[0].header("user-agent").is_some_and(|v| v.starts_with("KimiCLI/")), "{:?}", requests[0].header("user-agent"));
        assert!(requests[0].header("x-msh-device-id").is_some_and(|v| !v.is_empty()));
        assert_eq!(requests[1].path, "/api/oauth/token");
        assert_eq!(requests[1].field("device_code").as_deref(), Some("dc"));
        assert_eq!(requests[1].header("x-msh-platform"), Some("kimi_cli"));
    }

    #[tokio::test]
    async fn refresh_keeps_the_fingerprint_headers_and_the_account_id() {
        let mock = Mock::start(&[("200 OK", r#"{"access_token":"a2","refresh_token":"r2","expires_in":3600}"#)]).await;
        let cred = OAuth { access: "a1".into(), refresh: Some("r1".into()), expires: None,
            extra: Map::from_iter([("account_id".to_string(), json!("u123"))]) };
        let next = refresh_at(&mock.origin, &cred).await.unwrap();
        assert_eq!((next.access.as_str(), next.refresh.as_deref()), ("a2", Some("r2")));
        assert!(next.expires.unwrap() > super::super::now_ms() + 3_000_000);
        assert_eq!(next.extra["account_id"], "u123");
        let requests = mock.requests();
        assert_eq!(requests[0].field("grant_type").as_deref(), Some("refresh_token"));
        assert_eq!(requests[0].field("refresh_token").as_deref(), Some("r1"));
        assert_eq!(requests[0].field("client_id").as_deref(), Some(CLIENT_ID));
        assert_eq!(requests[0].header("x-msh-platform"), Some("kimi_cli"));
    }

    #[test]
    fn headers_are_printable_ascii_and_name_kimi_cli() {
        let h = headers();
        assert!(h.iter().any(|(k, v)| *k == "X-Msh-Platform" && v == "kimi_cli"));
        assert!(h.iter().any(|(k, v)| *k == "User-Agent" && v.starts_with("KimiCLI/")));
        for (_, v) in h {
            assert!(v.chars().all(|c| (' '..='~').contains(&c)), "header value not printable: {v:?}");
            assert!(!v.is_empty());
        }
        assert!(h.iter().any(|(k, _)| *k == "X-Msh-Device-Id"));
        assert_eq!(clean("a\u{fffd}b", "x"), "ab");
        assert_eq!(clean("  ", "x"), "x");
    }

    #[test]
    fn endpoint_matches_kimi_hosts_only() {
        for kimi in ["https://api.kimi.com/coding/v1", "https://auth.kimi.com", "https://kimi.com"] {
            assert!(endpoint(kimi), "{kimi}");
        }
        for other in ["https://api.moonshot.ai/v1", "https://api.kimi.com.evil/v1", "https://example.com", "not a url"] {
            assert!(!endpoint(other), "{other}");
        }
    }

    #[test]
    fn credentials_take_the_account_id_from_the_token_claims() {
        let access = access(r#"{"sub":"s9"}"#);
        let v: Value = serde_json::from_str(&format!(r#"{{"access_token":"{access}","refresh_token":"r","expires_in":60}}"#)).unwrap();
        let cred = credential(&v, None).unwrap();
        assert_eq!(cred.extra["account_id"], "s9");
        assert_eq!(cred.refresh.as_deref(), Some("r"));
        // Refresh answers that omit the token keep the old one.
        let plain: Value = serde_json::from_str(r#"{"access_token":"t","expires_in":60}"#).unwrap();
        let kept = credential(&plain, Some(&cred)).unwrap();
        assert_eq!(kept.refresh, cred.refresh);
        assert_eq!(kept.extra["account_id"], "s9");
    }
}
