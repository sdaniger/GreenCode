//! Snowflake: local-application OAuth PKCE on the account's own host
//! (OMP `registry/oauth/snowflake.ts`, rule `auth/snowflake.kdl`).
//!
//! OMP prompts for the account before the browser opens; GreenCode's sign-in has no prompt step,
//! so the account comes from `SNOWFLAKE_ACCOUNT`, else the configured `providers.snowflake.base_url`.

use super::{browser, pkce, random, Callback, Cred, OAuth, Start};
use anyhow::{bail, Context, Result};
use serde_json::{Map, Value};

const CLIENT_ID: &str = "LOCAL_APPLICATION";
/// The local-application integration registers this loopback port.
const CALLBACK_PORT: u16 = 54551;
const INVALID: &str = "Snowflake のアカウント識別子（orgname-accountname）かアカウント URL を指定してください";

/// An account identifier or account/Snowsight URL as `https://<host>.snowflakecomputing.com`.
pub fn account_url(input: &str) -> Result<String> {
    let value = input.trim();
    if value.is_empty() { bail!("Snowflake のアカウントが必要です。SNOWFLAKE_ACCOUNT を設定してください") }
    if value.contains('\\') || value.contains(['/', '?', '#']) && !value.contains("://") { bail!(INVALID) }
    let lower = value.to_ascii_lowercase();
    if lower.starts_with("http://") { bail!("Snowflake のアカウント URL は https で指定してください") }
    let host = match value.split_once("://") {
        Some((scheme, rest)) => {
            if !scheme.eq_ignore_ascii_case("https") { bail!("Snowflake のアカウント URL は https で指定してください") }
            let host = rest.split(['/', '?', '#']).next().unwrap_or("").to_ascii_lowercase().replace('_', "-");
            if host == "app.snowflake.com" {
                // Snowsight links are /<org>/<account>; legacy /<region>/<locator> links name no host.
                let mut segments = rest.split(['?', '#']).next().unwrap_or("").split('/').skip(1);
                let (org, account) = (segments.next().unwrap_or(""), segments.next().unwrap_or(""));
                let label = |s: &str| !s.is_empty() && s.len() <= 63 && s.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
                // OMP's LEGACY_SNOWSIGHT_REGION `^[a-z]{2}(-gov)?-[a-z]+-\d+$`, e.g. us-east-1 or us-gov-west-1.
                let letters = |s: &str| !s.is_empty() && s.chars().all(|c| c.is_ascii_lowercase());
                let region = match org.split('-').collect::<Vec<_>>().as_slice() {
                    [geo, area, n] | [geo, "gov", area, n] => {
                        geo.len() == 2 && letters(geo) && letters(area) && !n.is_empty() && n.chars().all(|c| c.is_ascii_digit())
                    }
                    _ => false,
                };
                if !label(org) || !label(account) || region {
                    bail!("旧形式の Snowsight リンクはアカウントのホストを特定できません。アカウント識別子（orgname-accountname）を指定してください")
                }
                format!("{org}-{account}.snowflakecomputing.com")
            } else {
                host
            }
        }
        None => value.to_ascii_lowercase().replace('_', "-"),
    };
    let host = host.strip_suffix(":443").unwrap_or(&host);
    if host.ends_with(".snowflakecomputing.cn") {
        bail!("中国リージョンの Snowflake アカウントでは Cortex REST API を利用できません")
    }
    let host = if host.ends_with(".snowflakecomputing.com") { host.to_string() } else { format!("{host}.snowflakecomputing.com") };
    let valid = host.len() > ".snowflakecomputing.com".len()
        && host.split('.').all(|label| {
            !label.is_empty() && label.len() <= 63
                && label.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
                && !label.starts_with('-') && !label.ends_with('-')
        });
    if !valid { bail!(INVALID) }
    Ok(format!("https://{host}"))
}

/// The account to sign in to: the env, else the configured base URL when it names one.
fn configured_account() -> Result<String> {
    if let Some(account) = std::env::var("SNOWFLAKE_ACCOUNT").ok().filter(|a| !a.trim().is_empty()) {
        return account_url(&account);
    }
    let base = crate::config::Config::load().ok()
        .and_then(|cfg| cfg.providers.get("snowflake").map(|p| p.base_url.clone()))
        .unwrap_or_default();
    if base.contains("<account>") || base.trim().is_empty() {
        bail!("Snowflake のアカウントが必要です。SNOWFLAKE_ACCOUNT を設定してから /login snowflake を実行してください")
    }
    account_url(base.split("/api/").next().unwrap_or(&base))
}

pub async fn login() -> Result<Start> {
    let account = configured_account()?;
    let (verifier, challenge) = pkce()?;
    let state = random()?;
    let callback = Callback::bind(CALLBACK_PORT, "127.0.0.1", "/").await?;
    let query = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("client_id", CLIENT_ID)
        .append_pair("response_type", "code")
        .append_pair("redirect_uri", &callback.redirect_uri)
        .append_pair("scope", "refresh_token")
        .append_pair("state", &state)
        .append_pair("code_challenge", &challenge)
        .append_pair("code_challenge_method", "S256")
        .finish();
    let url = format!("{account}/oauth/authorize?{query}");
    Ok(browser(url, callback, move |redirect, redirect_uri| async move {
        if let Some(got) = redirect.get("state").filter(|got| *got != state) {
            bail!("Snowflake のログイン状態が一致しません（{got}）。もう一度お試しください");
        }
        let code = redirect.code()?.to_string();
        let token = token_request(&account, &[
            ("grant_type", "authorization_code"),
            ("code", &code),
            ("redirect_uri", &redirect_uri),
            ("code_verifier", &verifier),
            ("client_id", CLIENT_ID),
        ]).await.context("Snowflake のトークン交換に失敗しました")?;
        Ok(Cred::OAuth(credential(token, &account)))
    }))
}

pub async fn refresh(cred: OAuth) -> Result<OAuth> {
    let account = cred.extra.get("account").and_then(Value::as_str).map(account_url).transpose()?;
    let Some(refresh) = cred.refresh.clone().filter(|r| !r.trim().is_empty()) else {
        // No refresh token issued: keep a still-valid access token, else the user must sign in again.
        if cred.expires.is_some_and(|at| at > super::now_ms()) { return Ok(cred) }
        bail!("Snowflake のトークンが切れました。/login snowflake でもう一度ログインしてください")
    };
    let account = account.ok_or_else(|| anyhow::anyhow!("Snowflake のアカウント情報がありません。/login snowflake でもう一度ログインしてください"))?;
    let token = token_request(&account, &[
        ("grant_type", "refresh_token"),
        ("refresh_token", &refresh),
        ("client_id", CLIENT_ID),
    ]).await.context("Snowflake のトークン更新に失敗しました")?;
    let mut next = credential(token, &account);
    if next.refresh.is_none() { next.refresh = Some(refresh) }
    Ok(next)
}

async fn token_request(account: &str, form: &[(&str, &str)]) -> Result<Value> {
    let url = std::env::var("SNOWFLAKE_OAUTH_TOKEN_URL").ok().filter(|u| !u.trim().is_empty())
        .unwrap_or_else(|| format!("{account}/oauth/token-request"));
    super::post_form(&url, form, &[]).await
}

fn credential(v: Value, account: &str) -> OAuth {
    let mut extra = Map::new();
    extra.insert("account".into(), Value::String(account.into()));
    OAuth {
        access: v["access_token"].as_str().unwrap_or_default().trim().to_string(),
        refresh: v["refresh_token"].as_str().map(str::trim).filter(|r| !r.is_empty()).map(str::to_string),
        expires: super::expires_in(v["expires_in"].as_u64()),
        extra,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identifiers_and_snowsight_links_normalize_to_the_account_host() {
        assert_eq!(account_url("myorg-myaccount").unwrap(), "https://myorg-myaccount.snowflakecomputing.com");
        assert_eq!(account_url(" https://acme.snowflakecomputing.com/ ").unwrap(), "https://acme.snowflakecomputing.com");
        assert_eq!(account_url("acme:443").unwrap(), "https://acme.snowflakecomputing.com");
        assert_eq!(account_url("https://app.snowflake.com/acme/ab12345/").unwrap(), "https://acme-ab12345.snowflakecomputing.com");
        assert!(account_url("").is_err());
        assert!(account_url("http://acme").is_err());
        assert!(account_url("acme.snowflakecomputing.cn").is_err());
        assert!(account_url("https://app.snowflake.com/us-east-1/xy12345").is_err());
        assert!(account_url("bad host").is_err());
    }

    #[tokio::test]
    async fn exchange_and_refresh_use_the_account_token_endpoint() {
        let mock = crate::login::mock::Mock::start(&[
            ("200", r#"{"access_token":"at1","refresh_token":"rt1","expires_in":600}"#),
            ("200", r#"{"access_token":"at2","expires_in":600}"#),
        ]).await;
        // Account hosts must be https *.snowflakecomputing.com; the override sends the token calls to the mock.
        let account = account_url("acme").unwrap();
        unsafe { std::env::set_var("SNOWFLAKE_OAUTH_TOKEN_URL", format!("{}/oauth/token-request", mock.origin)) };
        let cred = {
            let v = token_request(&account, &[
                ("grant_type", "authorization_code"),
                ("code", "c1"),
                ("redirect_uri", "http://127.0.0.1:54551/"),
                ("code_verifier", "v1"),
                ("client_id", CLIENT_ID),
            ]).await.unwrap();
            credential(v, &account)
        };
        assert_eq!((cred.access.as_str(), cred.refresh.as_deref()), ("at1", Some("rt1")));
        assert_eq!(cred.extra["account"], account);
        let refreshed = refresh(cred).await.unwrap();
        assert_eq!(refreshed.access, "at2");
        // The response carried no new refresh token, so the old one stays.
        assert_eq!(refreshed.refresh.as_deref(), Some("rt1"));
        let reqs = mock.requests();
        assert_eq!(reqs[0].path, "/oauth/token-request");
        assert_eq!(reqs[0].field("code_verifier").as_deref(), Some("v1"));
        assert_eq!(reqs[0].field("client_id").as_deref(), Some(CLIENT_ID));
        assert_eq!(reqs[1].field("grant_type").as_deref(), Some("refresh_token"));
        assert_eq!(reqs[1].field("refresh_token").as_deref(), Some("rt1"));
    }
}
