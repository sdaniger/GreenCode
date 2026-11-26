//! OpenRouter: authorization-code PKCE sign-in that mints a durable API key, or a pasted `sk-or-` key
//! (OMP rule `auth/openrouter.kdl`; OMP has no `registry/oauth/openrouter.ts` because it is pure KDL).

use super::{browser, pkce, Callback, Cred, Redirect, Start};
use anyhow::{anyhow, bail, Context, Result};
use serde_json::json;
use std::time::Duration;

const AUTHORIZE: &str = "https://openrouter.ai/auth";
const TOKEN: &str = "https://openrouter.ai/api/v1/auth/keys";
const VALIDATE: &str = "https://openrouter.ai/api/v1/auth/key";
const CALLBACK_PORT: u16 = 54549;
const KEY_PREFIX: &str = "sk-or-";

pub async fn login() -> Result<Start> {
    login_at(AUTHORIZE, TOKEN, VALIDATE, CALLBACK_PORT).await
}

async fn login_at(authorize: &str, token_url: &str, validate: &str, port: u16) -> Result<Start> {
    let (verifier, challenge) = pkce()?;
    // OMP falls back to a random port when its registered one is taken; the service accepts any callback.
    let callback = match Callback::bind(port, "localhost", "/callback").await {
        Ok(cb) => cb,
        Err(_) => Callback::bind(0, "localhost", "/callback").await?,
    };
    let query = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("callback_url", &callback.redirect_uri)
        .append_pair("code_challenge", &challenge)
        .append_pair("code_challenge_method", "S256")
        .finish();
    let url = format!("{authorize}?{query}");
    let (token_url, validate) = (token_url.to_string(), validate.to_string());
    Ok(browser(url, callback, move |redirect, _redirect_uri| async move {
        let code = redirect.code()?.to_string();
        // A pasted API key arrives through the same field: validate it and keep it as the durable key.
        if code.starts_with(KEY_PREFIX) {
            check_key(&validate, &code).await?;
            return Ok(Cred::Key(code));
        }
        let v = super::post_json(&token_url, &json!({"code": code, "code_verifier": verifier, "code_challenge_method": "S256"}), &[]).await?;
        let key = v["key"].as_str().filter(|k| !k.is_empty()).map(str::to_string)
            .ok_or_else(|| anyhow!("OpenRouter のトークン応答に key がありません"))?;
        Ok(Cred::Key(key))
    }))
}

/// `GET /auth/key` authenticates the key; any failure means it was refused.
async fn check_key(url: &str, key: &str) -> Result<()> {
    let resp = reqwest::Client::new().get(url).bearer_auth(key).timeout(Duration::from_secs(15))
        .send().await.with_context(|| format!("GET {url}"))?;
    let status = resp.status();
    if status.is_success() { return Ok(()) }
    let body = resp.text().await.unwrap_or_default();
    bail!("OpenRouter の API キーを確認できませんでした(HTTP {status}): {}", body.chars().take(300).collect::<String>())
}

#[cfg(test)]
mod tests {
    use super::super::mock::Mock;
    use super::*;

    /// Runs a login against mocks, optionally pasting a redirect/API key instead of visiting the callback.
    async fn run(authorize: &str, token: &str, validate: &str, port: u16, paste: Option<&str>) -> Result<Cred> {
        let start = login_at(authorize, token, validate, port).await?;
        let paste_tx = start.paste.clone().expect("browser sign-ins accept a paste");
        let done = tokio::spawn(start.done);
        if let Some(text) = paste {
            paste_tx.send(text.into()).unwrap();
        } else {
            let callback = url::Url::parse(&start.url).unwrap()
                .query_pairs().find(|(k, _)| k == "callback_url").expect("callback_url").1.into_owned();
            reqwest::get(format!("{callback}?code=auth-code-1")).await.unwrap();
        }
        done.await.unwrap()
    }

    #[tokio::test]
    async fn authorize_url_carries_only_the_callback_and_pkce() {
        let mock = Mock::start(&[]).await;
        let start = login_at(&format!("{}/auth", mock.origin), "http://127.0.0.1:1/keys", "http://127.0.0.1:1/key", 54549).await.unwrap();
        assert!(start.url.starts_with(&format!("{}/auth?", mock.origin)));
        assert!(start.url.contains("callback_url=http%3A%2F%2Flocalhost%3A"), "{}", start.url);
        assert!(start.url.contains("code_challenge_method=S256"));
        assert!(start.paste.is_some());
        assert!(start.code.is_none());
        // `state`, `client_id`, `response_type` and a plain redirect_uri are not part of OpenRouter's params.
        for absent in ["state=", "client_id=", "response_type=", "redirect_uri="] {
            assert!(!start.url.contains(absent), "{} sent {absent}", start.url);
        }
        drop(start);
    }

    #[tokio::test]
    async fn exchange_posts_the_code_and_verifier_as_json_and_yields_the_key() {
        let mock = Mock::start(&[("200 OK", r#"{"key":"sk-or-v1-minted"}"#)]).await;
        let cred = run(&format!("{}/auth", mock.origin), &format!("{}/api/v1/auth/keys", mock.origin),
            &format!("{}/api/v1/auth/key", mock.origin), 0, None).await.unwrap();
        assert!(matches!(&cred, Cred::Key(key) if key == "sk-or-v1-minted"), "{cred:?}");
        let requests = mock.requests();
        assert_eq!(requests[0].path, "/api/v1/auth/keys");
        let body = requests[0].json();
        assert_eq!(body["code"], "auth-code-1");
        assert_eq!(body["code_challenge_method"], "S256");
        // PKCE: the verifier hashes to the challenge in the authorize URL.
        assert!(body["code_verifier"].as_str().is_some_and(|v| v.len() >= 43), "{body}");
        assert_eq!(requests[0].header("content-type"), Some("application/json"));
    }

    #[tokio::test]
    async fn a_pasted_key_is_validated_and_kept_without_an_exchange() {
        let mock = Mock::start(&[("200 OK", r#"{"data":{"label":"sk-or"}}"#)]).await;
        let cred = run(&format!("{}/auth", mock.origin), &format!("{}/api/v1/auth/keys", mock.origin),
            &format!("{}/api/v1/auth/key", mock.origin), 0, Some("sk-or-v1-pasted")).await.unwrap();
        assert!(matches!(&cred, Cred::Key(key) if key == "sk-or-v1-pasted"), "{cred:?}");
        let requests = mock.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].path, "/api/v1/auth/key");
        assert_eq!(requests[0].header("authorization"), Some("Bearer sk-or-v1-pasted"));

        let refused = Mock::start(&[("401 Unauthorized", r#"{"error":"invalid key"}"#)]).await;
        let error = run(&format!("{}/auth", refused.origin), &format!("{}/api/v1/auth/keys", refused.origin),
            &format!("{}/api/v1/auth/key", refused.origin), 0, Some("sk-or-v1-bad")).await.unwrap_err().to_string();
        assert!(error.contains("API キーを確認できませんでした"), "{error}");
    }

    #[tokio::test]
    async fn a_pasted_redirect_url_is_accepted() {
        let mock = Mock::start(&[("200 OK", r#"{"key":"sk-or-v1-fromurl"}"#)]).await;
        let cred = run(&format!("{}/auth", mock.origin), &format!("{}/api/v1/auth/keys", mock.origin),
            &format!("{}/api/v1/auth/key", mock.origin), 0, Some("http://localhost:54549/callback?code=url-code")).await.unwrap();
        assert!(matches!(&cred, Cred::Key(key) if key == "sk-or-v1-fromurl"), "{cred:?}");
        assert_eq!(mock.requests()[0].json()["code"], "url-code");
    }

    #[test]
    fn parse_of_a_redirect_matches_the_callback_shape() {
        assert_eq!(Redirect::parse("http://localhost:54549/callback?code=abc").code().unwrap(), "abc");
        assert_eq!(Redirect::parse("sk-or-v1-key").code().unwrap(), "sk-or-v1-key");
    }
}
