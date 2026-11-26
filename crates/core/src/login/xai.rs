//! xAI Grok OAuth (SuperGrok / X Premium+): RFC 8628 device sign-in against auth.x.ai
//! (OMP `registry/oauth/xai-oauth.ts`, rule `auth/xai-oauth.kdl`).

use super::{device, device_start, jwt_claims, poll_every, post_form, Cred, OAuth, Start};
use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};
use std::time::Duration;

const ISSUER: &str = "https://auth.x.ai";
const CLIENT_ID: &str = "b1a00492-073a-47ea-816f-4c329264a828";
const SCOPES: &str = "openid profile email offline_access grok-cli:access api:access";
const ACCEPT: [(&str, &str); 1] = [("Accept", "application/json")];

pub async fn login() -> Result<Start> {
    let token = token_endpoint(ISSUER).await?;
    login_with(ISSUER, &token, Duration::from_secs(1)).await
}

async fn login_with(issuer: &str, token_url: &str, unit: Duration) -> Result<Start> {
    let device = device(
        &format!("{issuer}/oauth2/device/code"),
        &[("client_id", CLIENT_ID), ("scope", SCOPES)],
        &ACCEPT,
    )
    .await?;
    let token_url = token_url.to_string();
    let userinfo = format!("{issuer}/oauth2/userinfo");
    Ok(device_start(device, move |device| async move {
        let v = poll_every(&device, &token_url, &[("client_id", CLIENT_ID)], &ACCEPT, unit).await?;
        let mut cred = super::standard(&v)?;
        identity(&mut cred, &userinfo).await;
        Ok(Cred::OAuth(cred))
    }))
}

pub async fn refresh(cred: OAuth) -> Result<OAuth> {
    let token = token_endpoint(ISSUER).await?;
    refresh_with(ISSUER, &token, cred).await
}

async fn refresh_with(issuer: &str, token_url: &str, cred: OAuth) -> Result<OAuth> {
    let refresh = cred.refresh.as_deref().filter(|r| !r.is_empty())
        .ok_or_else(|| anyhow!("xAI の更新トークンがありません。/login xai-oauth でもう一度ログインしてください"))?;
    let v = post_form(
        token_url,
        &[("grant_type", "refresh_token"), ("client_id", CLIENT_ID), ("refresh_token", refresh)],
        &[],
    )
    .await?;
    let mut next = super::standard(&v)?;
    // The service does not always rotate the refresh token; keep the old one then.
    if next.refresh.is_none() { next.refresh = Some(refresh.to_string()); }
    for key in ["account_id", "email"] {
        if let Some(value) = cred.extra.get(key) { next.extra.insert(key.into(), value.clone()); }
    }
    identity(&mut next, &format!("{issuer}/oauth2/userinfo")).await;
    Ok(next)
}

/// Fills `account_id` (`sub`, token claim first) and `email` from the userinfo endpoint; failures leave them unset.
async fn identity(cred: &mut OAuth, userinfo: &str) {
    let claim = jwt_claims(&cred.access).and_then(|c| c.get("sub").and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_string));
    let (mut id, mut email) = (claim, None);
    if let Some(v) = userinfo_at(userinfo, &cred.access).await {
        let text = |k: &str| v[k].as_str().filter(|s| !s.is_empty()).map(str::to_string);
        id = text("sub").or(id);
        email = text("email");
    }
    if let Some(id) = id { cred.extra.insert("account_id".into(), json!(id)); }
    if let Some(email) = email { cred.extra.insert("email".into(), json!(email)); }
}

async fn userinfo_at(url: &str, token: &str) -> Option<Value> {
    let resp = reqwest::Client::new().get(url).bearer_auth(token).header("Accept", "application/json")
        .timeout(Duration::from_secs(15)).send().await.ok()?;
    if !resp.status().is_success() { return None }
    let v: Value = resp.json().await.ok()?;
    let text = |k: &str| v[k].as_str().is_some_and(|s| !s.is_empty());
    (text("sub") || text("email")).then_some(v)
}

/// The discovered token endpoint receives every future refresh token, so pin it to HTTPS on `x.ai`.
fn validate_endpoint(url: &str) -> Result<String> {
    let host = reqwest::Url::parse(url).ok()
        .filter(|u| u.scheme() == "https")
        .and_then(|u| u.host_str().map(|h| h.to_ascii_lowercase()));
    let pinned = host.is_some_and(|h| h == "x.ai" || h.ends_with(".x.ai"));
    if !pinned { bail!("xAI のトークンエンドポイントが不正です: {url}") }
    Ok(url.to_string())
}

async fn token_endpoint(issuer: &str) -> Result<String> {
    let url = format!("{issuer}/.well-known/openid-configuration");
    let resp = reqwest::Client::new().get(&url).header("Accept", "application/json")
        .timeout(Duration::from_secs(15)).send().await.with_context(|| format!("GET {url}"))?;
    let status = resp.status();
    let body = resp.text().await?;
    if !status.is_success() {
        bail!("xAI の OIDC 設定を取得できませんでした (HTTP {status}): {}", body.chars().take(300).collect::<String>())
    }
    let v: Value = serde_json::from_str(&body).with_context(|| "xAI の OIDC 設定が JSON ではありません")?;
    validate_endpoint(v["token_endpoint"].as_str().map(str::trim).unwrap_or(""))
}

#[cfg(test)]
mod tests {
    use super::super::mock::Mock;
    use super::*;
    use base64::Engine as _;
    use serde_json::Map;

    fn map(pairs: &[(&str, Value)]) -> Map<String, Value> {
        pairs.iter().map(|(k, v)| ((*k).to_string(), v.clone())).collect()
    }

    fn jwt(claims: &str) -> String {
        format!("e30.{}.x", base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(claims))
    }

    #[tokio::test]
    async fn device_flow_posts_the_scopes_then_reads_userinfo() {
        let mock = Mock::start(&[
            ("200 OK", r#"{"user_code":"AB-12","device_code":"dc","verification_uri":"{origin}/device","verification_uri_complete":"{origin}/device?user_code=AB-12","interval":1,"expires_in":60}"#),
            ("200 OK", &format!(r#"{{"access_token":"{}","refresh_token":"r1","expires_in":3600}}"#, jwt(r#"{"sub":"jwt-sub"}"#))),
            ("200 OK", r#"{"sub":"u-9","email":"User@Example.COM"}"#),
        ]).await;
        let token = format!("{}/oauth2/token", mock.origin);
        let start = login_with(&mock.origin, &token, Duration::from_millis(1)).await.unwrap();
        assert_eq!(start.url, format!("{}/device?user_code=AB-12", mock.origin));
        assert_eq!(start.code.as_deref(), Some("AB-12"));
        let Cred::OAuth(cred) = start.done.await.unwrap() else { panic!("expected an oauth credential") };
        assert_eq!((cred.access.as_str(), cred.refresh.as_deref()), (jwt(r#"{"sub":"jwt-sub"}"#).as_str(), Some("r1")));
        assert!(cred.expires.unwrap() > super::super::now_ms() + 3_000_000);
        // userinfo overrides the token claim.
        assert_eq!(cred.extra["account_id"], "u-9");
        assert_eq!(cred.extra["email"], "User@Example.COM");

        let requests = mock.requests();
        assert_eq!(requests[0].path, "/oauth2/device/code");
        assert_eq!(requests[0].field("client_id").as_deref(), Some(CLIENT_ID));
        assert_eq!(requests[0].field("scope").as_deref(), Some(SCOPES));
        assert_eq!(requests[1].path, "/oauth2/token");
        assert_eq!(requests[1].field("grant_type").as_deref(), Some("urn:ietf:params:oauth:grant-type:device_code"));
        assert_eq!(requests[1].field("device_code").as_deref(), Some("dc"));
        assert_eq!(requests[1].field("client_id").as_deref(), Some(CLIENT_ID));
        assert_eq!(requests[2].path, "/oauth2/userinfo");
        assert_eq!(requests[2].header("authorization"), Some(format!("Bearer {}", jwt(r#"{"sub":"jwt-sub"}"#)).as_str()));
    }

    #[tokio::test]
    async fn refresh_rotates_the_tokens_and_keeps_a_missing_refresh_and_the_identity() {
        let mock = Mock::start(&[
            ("200 OK", r#"{"access_token":"a2","expires_in":3600}"#),
            ("401 Unauthorized", r#"{"error":"invalid_token"}"#),
        ]).await;
        let cred = OAuth {
            access: "a1".into(),
            refresh: Some("r1".into()),
            expires: None,
            extra: map(&[("account_id", json!("stored")), ("email", json!("old@example.com"))]),
        };
        let token = format!("{}/oauth2/token", mock.origin);
        let next = refresh_with(&mock.origin, &token, cred).await.unwrap();
        assert_eq!((next.access.as_str(), next.refresh.as_deref()), ("a2", Some("r1")));
        assert!(next.expires.unwrap() > super::super::now_ms() + 3_000_000);
        // A failed userinfo leaves the stored identity alone.
        assert_eq!(next.extra["account_id"], "stored");
        assert_eq!(next.extra["email"], "old@example.com");

        let requests = mock.requests();
        assert_eq!(requests[0].path, "/oauth2/token");
        assert_eq!(requests[0].field("grant_type").as_deref(), Some("refresh_token"));
        assert_eq!(requests[0].field("client_id").as_deref(), Some(CLIENT_ID));
        assert_eq!(requests[0].field("refresh_token").as_deref(), Some("r1"));
        assert_eq!(requests[1].path, "/oauth2/userinfo");
    }

    #[tokio::test]
    async fn discovery_is_pinned_to_x_ai_over_https() {
        let ok = Mock::start(&[("200 OK", r#"{"token_endpoint":"https://auth.x.ai/oauth2/token"}"#)]).await;
        assert_eq!(token_endpoint(&ok.origin).await.unwrap(), "https://auth.x.ai/oauth2/token");
        for bad in ["http://auth.x.ai/oauth2/token", "https://evil.example/oauth2/token", "https://x.ai.evil/oauth2/token"] {
            assert!(validate_endpoint(bad).is_err(), "{bad} accepted");
        }
        let evil = Mock::start(&[("200 OK", r#"{"token_endpoint":"https://evil.example/oauth2/token"}"#)]).await;
        let error = token_endpoint(&evil.origin).await.unwrap_err().to_string();
        assert!(error.contains("トークンエンドポイント"), "{error}");
    }
}
