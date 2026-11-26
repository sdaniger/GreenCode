//! GitLab Duo sign-in: GitLab's OAuth authorization-code grant with PKCE.
//! `gitlab-duo` returns to a loopback listener; `gitlab-duo-agent` uses GitLab's official
//! VS Code OAuth application whose redirect (`vscode://`) must be pasted back
//! (OMP `auth/gitlab-duo*.kdl`, engine `registry/engine/oauth-code.ts`).

use super::{browser, pkce, random, Callback, Cred, OAuth, Start};
use anyhow::{anyhow, Result};
use serde_json::{Map, Value};

/// GitLab's bundled Duo OAuth application (`GITLAB_CLIENT_ID` overrides it).
const CLIENT_ID: &str = "da4edff2e6ebd2bc3208611e2768bc1c1dd7be791dc5ff26ca34ca9ee44f7d4b";
/// GitLab's official VS Code extension application.
const AGENT_CLIENT_ID: &str = "36f2a70cddeb5a0889d4fd8295c241b7e9848e89cf9e599d0eed2d8e5350fbf5";
const AUTHORIZE: &str = "https://gitlab.com/oauth/authorize";
const TOKEN: &str = "https://gitlab.com/oauth/token";
const AGENT_REDIRECT: &str = "vscode://gitlab.gitlab-workflow/authentication";

struct Endpoints {
    client_id: String,
    authorize: String,
    token: String,
    /// `GITLAB_REDIRECT_URI`: advertised verbatim, with the random-port fallback off.
    redirect_uri: Option<String>,
}

impl Endpoints {
    fn from_env(agent: bool) -> Self {
        let env = |name: &str| std::env::var(name).ok().filter(|v| !v.trim().is_empty());
        Self {
            client_id: match agent {
                true => AGENT_CLIENT_ID.into(),
                false => env("GITLAB_CLIENT_ID").unwrap_or_else(|| CLIENT_ID.into()),
            },
            authorize: AUTHORIZE.into(),
            token: TOKEN.into(),
            redirect_uri: match agent {
                true => None,
                false => env("GITLAB_REDIRECT_URI"),
            },
        }
    }
}

pub async fn login() -> Result<Start> {
    let ep = Endpoints::from_env(false);
    let callback = match ep.redirect_uri.clone() {
        Some(uri) => override_callback(&uri).await?,
        None => match Callback::bind(8080, "localhost", "/callback").await {
            // GitLab rejects an unregistered port; 8080 is the registered one.
            Ok(callback) => callback,
            Err(_) => Callback::bind(0, "localhost", "/callback").await?,
        },
    };
    let (verifier, challenge) = pkce()?;
    let state = random()?;
    let url = authorize_url(&ep.authorize, &ep.client_id, &callback.redirect_uri, &challenge, &state);
    Ok(browser(url, callback, move |redirect, redirect_uri| async move {
        let code = redirect.code()?.to_string();
        Ok(Cred::OAuth(exchange_at(&ep.token, &ep.client_id, &code, &redirect_uri, &verifier).await?))
    }))
}

pub async fn login_agent() -> Result<Start> {
    let ep = Endpoints::from_env(true);
    let callback = Callback::manual(AGENT_REDIRECT);
    let (verifier, challenge) = pkce()?;
    let state = random()?;
    let url = authorize_url(&ep.authorize, &ep.client_id, &callback.redirect_uri, &challenge, &state);
    Ok(browser(url, callback, move |redirect, redirect_uri| async move {
        let code = redirect.code()?.to_string();
        Ok(Cred::OAuth(exchange_at(&ep.token, &ep.client_id, &code, &redirect_uri, &verifier).await?))
    }))
}

/// An `http://` override names the listener it must advertise; any other scheme is paste-only.
async fn override_callback(uri: &str) -> Result<Callback> {
    let parsed = url::Url::parse(uri).map_err(|_| anyhow!("GITLAB_REDIRECT_URI が正しい URL ではありません: {uri}"))?;
    let Some(host) = parsed.host_str().filter(|_| parsed.scheme() == "http") else {
        return Ok(Callback::manual(uri));
    };
    let port = parsed.port_or_known_default().unwrap_or(80);
    Callback::bind(port, host, parsed.path()).await
}

fn authorize_url(authorize: &str, client_id: &str, redirect_uri: &str, challenge: &str, state: &str) -> String {
    let query = url::form_urlencoded::Serializer::new(String::new())
        .extend_pairs([
            ("client_id", client_id),
            ("response_type", "code"),
            ("redirect_uri", redirect_uri),
            ("scope", "api"),
            ("code_challenge", challenge),
            ("code_challenge_method", "S256"),
            ("state", state),
        ])
        .finish();
    format!("{authorize}?{query}")
}

pub async fn refresh(cred: OAuth) -> Result<OAuth> {
    let ep = Endpoints::from_env(false);
    let renewed = refresh_at(&ep.token, &ep.client_id, &cred, None).await?;
    crate::provider::gitlab_duo::clear_cache();
    Ok(renewed)
}

pub async fn refresh_agent(cred: OAuth) -> Result<OAuth> {
    let ep = Endpoints::from_env(true);
    let renewed = refresh_at(&ep.token, &ep.client_id, &cred, Some(AGENT_REDIRECT)).await?;
    crate::provider::gitlab_duo::clear_cache();
    Ok(renewed)
}

/// The authorization code is exchanged form-encoded, as GitLab requires.
async fn exchange_at(token: &str, client_id: &str, code: &str, redirect_uri: &str, verifier: &str) -> Result<OAuth> {
    let form = [
        ("grant_type", "authorization_code"),
        ("client_id", client_id),
        ("code", code),
        ("redirect_uri", redirect_uri),
        ("code_verifier", verifier),
    ];
    let v = super::post_form(token, &form, &[]).await?;
    credential(&v, None)
}

async fn refresh_at(token: &str, client_id: &str, cred: &OAuth, redirect_uri: Option<&str>) -> Result<OAuth> {
    let refresh = cred.refresh.as_deref().filter(|t| !t.is_empty())
        .ok_or_else(|| anyhow!("GitLab の更新トークンがありません。/login gitlab-duo でもう一度ログインしてください"))?;
    let mut form = vec![
        ("grant_type", "refresh_token"),
        ("client_id", client_id),
        ("refresh_token", refresh),
    ];
    // The agent application's refresh grants repeat the redirect URI (OMP's rule).
    if let Some(uri) = redirect_uri {
        form.push(("redirect_uri", uri));
    }
    let v = super::post_form(token, &form, &[]).await?;
    credential(&v, Some(cred))
}

/// `expires = created_at + expires_in - 5 min`; GitLab's `created_at` is seconds.
fn credential(v: &Value, previous: Option<&OAuth>) -> Result<OAuth> {
    let access = v["access_token"].as_str().filter(|t| !t.is_empty())
        .ok_or_else(|| anyhow!("GitLab のトークン応答に access_token がありません"))?;
    let created_ms = v["created_at"].as_u64().map_or_else(super::now_ms, |s| s.saturating_mul(1000));
    let expires = v["expires_in"].as_u64()
        .map(|seconds| (created_ms + seconds.saturating_mul(1000)).saturating_sub(300_000));
    Ok(OAuth {
        access: access.into(),
        // A refresh answer may omit the token; the old one stays valid then (RFC 6749 §6).
        refresh: v["refresh_token"].as_str().filter(|t| !t.is_empty()).map(str::to_string)
            .or_else(|| previous.and_then(|p| p.refresh.clone())),
        expires,
        extra: Map::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::super::mock::Mock;
    use super::*;

    #[test]
    fn the_authorize_url_carries_pkce_and_the_redirect() {
        let url = authorize_url(AUTHORIZE, "client", "http://localhost:8080/callback", "chal", "st");
        let parsed = url::Url::parse(&url).unwrap();
        let q: std::collections::BTreeMap<_, _> = parsed.query_pairs().into_owned().collect();
        assert_eq!(q["client_id"], "client");
        assert_eq!(q["redirect_uri"], "http://localhost:8080/callback");
        assert_eq!(q["code_challenge_method"], "S256");
        assert_eq!(q["scope"], "api");
        assert_eq!((q["code_challenge"].as_str(), q["state"].as_str()), ("chal", "st"));
    }

    #[tokio::test]
    async fn the_code_exchange_posts_the_verifier_and_subtracts_the_skew() {
        let mock = Mock::start(&[("200", r#"{"access_token":"at","refresh_token":"rt","expires_in":7200,"created_at":1700000000}"#)]).await;
        let cred = exchange_at(&format!("{}/oauth/token", mock.origin), "client", "the-code", "http://localhost:8080/callback", "verifier").await.unwrap();
        let request = &mock.requests()[0];
        assert_eq!(request.path, "/oauth/token");
        assert_eq!(request.field("grant_type").as_deref(), Some("authorization_code"));
        assert_eq!(request.field("code").as_deref(), Some("the-code"));
        assert_eq!(request.field("code_verifier").as_deref(), Some("verifier"));
        assert_eq!(request.field("redirect_uri").as_deref(), Some("http://localhost:8080/callback"));
        assert_eq!(cred.access, "at");
        assert_eq!(cred.refresh.as_deref(), Some("rt"));
        assert_eq!(cred.expires, Some(1_700_000_000 * 1000 + 7_200_000 - 300_000));
    }

    #[tokio::test]
    async fn a_missing_created_at_still_expires_in_the_future() {
        let mock = Mock::start(&[("200", r#"{"access_token":"at","expires_in":3600}"#)]).await;
        let before = super::super::now_ms();
        let cred = exchange_at(&format!("{}/oauth/token", mock.origin), "client", "c", "r", "v").await.unwrap();
        assert!(cred.expires.unwrap() >= before + 3_300_000, "{:?}", cred.expires);
    }

    #[tokio::test]
    async fn the_agent_refresh_repeats_the_vscode_redirect_uri() {
        let mock = Mock::start(&[("200", r#"{"access_token":"at2","refresh_token":"rt2","expires_in":7200}"#)]).await;
        let cred = OAuth { access: "at".into(), refresh: Some("rt".into()), expires: None, extra: Map::new() };
        let renewed = refresh_at(&format!("{}/oauth/token", mock.origin), AGENT_CLIENT_ID, &cred, Some(AGENT_REDIRECT)).await.unwrap();
        let request = &mock.requests()[0];
        assert_eq!(request.field("grant_type").as_deref(), Some("refresh_token"));
        assert_eq!(request.field("refresh_token").as_deref(), Some("rt"));
        assert_eq!(request.field("redirect_uri").as_deref(), Some(AGENT_REDIRECT));
        assert_eq!(renewed.access, "at2");
    }

    #[tokio::test]
    async fn a_refresh_without_a_refresh_token_asks_for_a_new_login() {
        let cred = OAuth { access: "at".into(), refresh: None, expires: None, extra: Map::new() };
        let err = refresh_at("http://127.0.0.1:9/oauth/token", "client", &cred, None).await.unwrap_err();
        assert!(err.to_string().contains("もう一度ログイン"), "{err}");
    }

    #[tokio::test]
    async fn a_refresh_without_a_new_token_keeps_the_old_one() {
        let mock = Mock::start(&[("200", r#"{"access_token":"at2","expires_in":7200}"#)]).await;
        let cred = OAuth { access: "at".into(), refresh: Some("rt".into()), expires: None, extra: Map::new() };
        let renewed = refresh_at(&format!("{}/oauth/token", mock.origin), "client", &cred, None).await.unwrap();
        assert_eq!((renewed.access.as_str(), renewed.refresh.as_deref()), ("at2", Some("rt")));
    }
}
