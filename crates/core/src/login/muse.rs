//! Muse Code (Meta subscription): RFC 8628 device sign-in against Meta's OIDC endpoint,
//! then the subscription's Model API key (OMP `registry/oauth/muse-code.ts`,
//! rule `auth/muse-code.kdl`).

use super::{device, device_start, poll_every, post_json, Cred, Start};
use anyhow::{anyhow, bail, Context, Result};
use serde_json::json;
use std::time::Duration;

const CLIENT_ID: &str = "1031625952748946";
const DEVICE_URL: &str = "https://auth.meta.com/oidc/device/authorization/";
const TOKEN_URL: &str = "https://auth.meta.com/oidc/device/token/";
const KEY_URL: &str = "https://api.meta.ai/muse-code/key";
const API_VERSION: &str = "1.0.0";
const ACCEPT: [(&str, &str); 2] = [("Accept", "application/json"), ("x-api-version", API_VERSION)];

pub async fn login() -> Result<Start> {
    login_at(DEVICE_URL, TOKEN_URL, KEY_URL, Duration::from_secs(1)).await
}

/// A sign-in whose three endpoints are replaceable, so tests never reach the network.
async fn login_at(device_url: &str, token_url: &str, key_url: &str, unit: Duration) -> Result<Start> {
    let device = device(device_url, &[("client_id", CLIENT_ID)], &ACCEPT).await?;
    let (token_url, key_url) = (token_url.to_string(), key_url.to_string());
    Ok(device_start(device, move |device| async move {
        // Meta's device grant rejects refresh grants and omits expiry; the credential is the minted key.
        let v = poll_every(&device, &token_url, &[("client_id", CLIENT_ID)], &ACCEPT, unit).await?;
        let access = v["access_token"].as_str().filter(|t| !t.is_empty())
            .ok_or_else(|| anyhow!("Muse Code のデバイストークン応答にアクセストークンがありません"))?
            .to_string();
        Ok(Cred::Key(mint(&key_url, &access).await?))
    }))
}

/// Exchanges a Meta account token for the Model API key the subscription authorizes.
async fn mint(url: &str, access: &str) -> Result<String> {
    let authorization = format!("Bearer {access}");
    let v = post_json(url, &json!({"onboard": true}), &[("Authorization", &authorization)])
        .await
        .context("Muse Code の API キー取得に失敗しました")?;
    if v["is_subs_active"] == false {
        bail!("Muse Code のサブスクリプションが有効ではありません。https://dev.meta.ai で契約状況を確認してください")
    }
    if let Some(key) = v["api_key"].as_str().map(str::trim).filter(|k| !k.is_empty()) {
        return Ok(key.to_string());
    }
    let action = v["action_url"].as_str().or(v["require_payment_action_url"].as_str())
        .map(str::trim).filter(|u| !u.is_empty());
    bail!(match action {
        Some(url) => format!("Muse Code のサブスクリプションが必要です: {url}"),
        None => "Muse Code の API キーが応答にありません。サインインし直してください".into(),
    })
}

#[cfg(test)]
mod tests {
    use super::super::mock::Mock;
    use super::*;

    /// Drives the device flow against a mock: one device answer, one poll, one key answer.
    async fn run(mock: &Mock) -> Result<Cred> {
        let origin = &mock.origin;
        let start = login_at(&format!("{origin}/device"), &format!("{origin}/token"), &format!("{origin}/muse-code/key"), Duration::from_millis(1)).await?;
        // The page and code come from the service, not from this module.
        assert!(start.code.is_some_and(|c| !c.is_empty()), "device flow must show a code");
        start.done.await
    }

    #[tokio::test]
    async fn signs_in_with_a_device_code_and_mints_the_subscription_key() {
        let mock = Mock::start(&[
            ("200", r#"{"device_code":"dc","user_code":"AB-12","verification_uri":"{origin}/device","interval":1,"expires_in":60}"#),
            ("200", r#"{"access_token":"meta-token"}"#),
            ("200", r#"{"api_key":"muse-key","user_email":"a@b.c","user_id":"u1","is_subs_active":true}"#),
        ])
        .await;
        let cred = run(&mock).await.unwrap();
        assert!(matches!(cred, Cred::Key(ref k) if k == "muse-key"), "{cred:?}");

        let req = mock.requests();
        assert_eq!(req[0].path, "/device");
        assert_eq!(req[0].field("client_id").as_deref(), Some(CLIENT_ID));
        assert_eq!(req[0].header("x-api-version"), Some("1.0.0"));
        assert_eq!(req[1].path, "/token");
        assert_eq!(req[1].field("device_code").as_deref(), Some("dc"));
        assert_eq!(req[1].header("x-api-version"), Some("1.0.0"));
        assert_eq!(req[2].path, "/muse-code/key");
        assert_eq!(req[2].header("authorization"), Some("Bearer meta-token"));
        assert_eq!(req[2].json()["onboard"], true);
    }

    #[tokio::test]
    async fn a_subscription_that_needs_payment_reports_the_action_url() {
        let mock = Mock::start(&[
            ("200", r#"{"device_code":"dc","user_code":"U","verification_uri":"{origin}/d","interval":1,"expires_in":60}"#),
            ("200", r#"{"access_token":"t"}"#),
            ("200", r#"{"is_subs_active":true,"require_payment":true,"action_url":"https://dev.meta.ai/billing"}"#),
        ])
        .await;
        let err = run(&mock).await.unwrap_err().to_string();
        assert!(err.contains("https://dev.meta.ai/billing"), "{err}");
    }

    #[tokio::test]
    async fn an_inactive_subscription_is_named_as_such() {
        let mock = Mock::start(&[
            ("200", r#"{"device_code":"dc","user_code":"U","verification_uri":"{origin}/d","interval":1,"expires_in":60}"#),
            ("200", r#"{"access_token":"t"}"#),
            ("200", r#"{"is_subs_active":false,"api_key":"ignored"}"#),
        ])
        .await;
        let err = run(&mock).await.unwrap_err().to_string();
        assert!(err.contains("サブスクリプション"), "{err}");
    }
}
