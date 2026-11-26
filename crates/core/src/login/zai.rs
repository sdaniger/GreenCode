//! Z.ai GLM Coding Plan: ZCode's browser sign-in, then the business-API key mint
//! (OMP `registry/oauth/zai.ts`, rule `auth/zai-coding-plan.kdl`).

use super::{browser, post_json, Callback, Cred, Start};
use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};
use std::time::Duration;

const CLIENT_ID: &str = "client_P8X5CMWmlaRO9gyO-KSqtg";
const AUTHORIZE_URL: &str = "https://chat.z.ai/api/oauth/authorize";
const TOKEN_URL: &str = "https://zcode.z.ai/api/v1/oauth/token";
/// Z.ai's allowlist rejects every loopback redirect for this client; only the ZCode desktop scheme validates.
const REDIRECT_URI: &str = "zcode://zai-auth/callback";
const BIZ_BASE: &str = "https://api.z.ai";
const BUSINESS_LOGIN_URL: &str = "https://api.z.ai/api/auth/z/login";
/// Our own key name, so signing in never touches a key another client created.
const KEY_NAME: &str = "greencode";

struct Endpoints {
    client_id: String,
    authorize: String,
    token: String,
    redirect_uri: String,
    biz: String,
    business_login: String,
}

impl Endpoints {
    fn from_env() -> Self {
        let env = |name: &str, default: &str| {
            std::env::var(name).ok().filter(|v| !v.trim().is_empty()).unwrap_or_else(|| default.into())
        };
        Self {
            client_id: env("ZAI_OAUTH_CLIENT_ID", CLIENT_ID),
            authorize: env("ZAI_OAUTH_AUTHORIZE_URL", AUTHORIZE_URL),
            token: env("ZAI_OAUTH_TOKEN_URL", TOKEN_URL),
            redirect_uri: env("ZAI_OAUTH_REDIRECT_URI", REDIRECT_URI),
            biz: env("ZAI_BIZ_BASE", BIZ_BASE).trim_end_matches('/').into(),
            business_login: env("ZAI_BUSINESS_LOGIN_URL", BUSINESS_LOGIN_URL),
        }
    }
}

pub async fn login() -> Result<Start> {
    start_at(Endpoints::from_env())
}

fn start_at(ep: Endpoints) -> Result<Start> {
    let state = super::random()?;
    let query = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("client_id", &ep.client_id)
        .append_pair("response_type", "code")
        .append_pair("redirect_uri", &ep.redirect_uri)
        .append_pair("state", &state)
        .finish();
    let url = format!("{}?{query}", ep.authorize);
    Ok(browser(url, Callback::manual(&ep.redirect_uri), move |redirect, _| async move {
        let code = redirect.code()?.to_string();
        mint_key(&ep, &code, &state).await.map(Cred::Key)
    }))
}

/// Exchanges the authorization code for the OAuth token, then that token for a durable `id.secret` API key.
async fn mint_key(ep: &Endpoints, code: &str, state: &str) -> Result<String> {
    let body = json!({"provider": "zai", "code": code, "redirect_uri": ep.redirect_uri, "state": state});
    let v = post_json(&ep.token, &body, &[]).await.context("Z.AI のトークン交換に失敗しました")?;
    let data = envelope(&v, "トークン交換")?;
    let access = data["zai"]["access_token"]
        .as_str()
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .ok_or_else(|| anyhow!("Z.AI のトークン交換応答にアクセストークンがありません"))?;
    provision(ep, access).await
}

/// `business login -> default org/project -> find or create our key -> copy its secret`.
async fn provision(ep: &Endpoints, oauth: &str) -> Result<String> {
    let v = post_json(&ep.business_login, &json!({"token": oauth}), &[])
        .await
        .context("Z.AI のビジネスログインに失敗しました")?;
    let data = envelope(&v, "ビジネスログイン")?;
    let biz = data["access_token"].as_str().or(data["accessToken"].as_str())
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .ok_or_else(|| anyhow!("Z.AI のビジネスログイン応答にアクセストークンがありません"))?;
    let bearer = format!("Bearer {biz}");

    let customer = get_json(&format!("{}/api/biz/customer/getCustomerInfo", ep.biz), biz).await?;
    let customer = envelope(&customer, "アカウント情報の取得")?;
    let orgs = customer["organizations"].as_array().map(Vec::as_slice).unwrap_or(&[]);
    let org = orgs.iter().find(|o| o["isDefault"] == true).or_else(|| orgs.first())
        .ok_or_else(|| anyhow!("Z.AI のアカウントに組織がありません"))?;
    let projects = org["projects"].as_array().map(Vec::as_slice).unwrap_or(&[]);
    let project = projects.iter().find(|p| p["isDefault"] == true).or_else(|| projects.first())
        .ok_or_else(|| anyhow!("Z.AI のアカウントにプロジェクトがありません"))?;
    let organization_id = trimmed(org["organizationId"].as_str()).ok_or_else(|| anyhow!("Z.AI の組織 ID を取得できませんでした"))?;
    let project_id = trimmed(project["projectId"].as_str()).ok_or_else(|| anyhow!("Z.AI のプロジェクト ID を取得できませんでした"))?;

    let keys_url = format!("{}/api/biz/v1/organization/{organization_id}/projects/{project_id}/api_keys", ep.biz);
    let listed = get_json(&keys_url, biz).await?;
    let existing = key_list(envelope(&listed, "API キー一覧の取得")?)
        .into_iter()
        .find(|k| k["name"].as_str() == Some(KEY_NAME));
    let record = match existing {
        Some(record) => record,
        None => {
            let created = post_json(&keys_url, &json!({"name": KEY_NAME}), &[("Authorization", &bearer)])
                .await
                .context("Z.AI の API キー作成に失敗しました")?;
            envelope(&created, "API キー作成")?.clone()
        }
    };
    let api_key = trimmed(record["apiKey"].as_str()).ok_or_else(|| anyhow!("Z.AI の API キー作成応答に apiKey がありません"))?;

    // The list masks the secret; only the copy endpoint returns it in full.
    let encoded: String = url::form_urlencoded::byte_serialize(api_key.as_bytes()).collect();
    let copied = get_json(&format!("{keys_url}/copy/{encoded}"), biz).await?;
    let secret = trimmed(envelope(&copied, "API キーの取得")?["secretKey"].as_str())
        .ok_or_else(|| anyhow!("Z.AI の API キー取得応答にシークレットがありません"))?;
    Ok(format!("{api_key}.{secret}"))
}

/// Z.ai's `{ code, msg, data, success }` envelope: success is `code` 0 or 200 with `success` not false.
fn envelope<'a>(body: &'a Value, what: &str) -> Result<&'a Value> {
    let Some(obj) = body.as_object().filter(|o| o.contains_key("code") || o.contains_key("success")) else { return Ok(body) };
    let ok = obj.get("success") != Some(&Value::Bool(false))
        && match obj.get("code") {
            None | Some(Value::Null) => true,
            Some(Value::Number(n)) => matches!(n.as_i64(), Some(0 | 200)),
            Some(Value::String(s)) => s == "0" || s == "200",
            Some(_) => false,
        };
    if !ok {
        let msg = obj.get("msg").and_then(Value::as_str).unwrap_or("不明なエラー");
        bail!("Z.AI の{what}に失敗しました: {msg}")
    }
    Ok(obj.get("data").unwrap_or(body))
}

fn key_list(v: &Value) -> Vec<Value> {
    if let Some(list) = v.as_array() {
        return list.clone();
    }
    ["list", "keys", "apiKeys", "records"].iter()
        .find_map(|field| v.get(*field).and_then(Value::as_array))
        .cloned()
        .unwrap_or_default()
}

fn trimmed(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|s| !s.is_empty())
}

async fn get_json(url: &str, bearer: &str) -> Result<Value> {
    let resp = reqwest::Client::new()
        .get(url)
        .timeout(Duration::from_secs(30))
        .bearer_auth(bearer)
        .header("Accept", "application/json")
        .send()
        .await
        .with_context(|| format!("GET {url}"))?;
    let status = resp.status();
    let text = resp.text().await?;
    if !status.is_success() {
        bail!("Z.AI の API 呼び出しに失敗しました（{status}）: {}", text.chars().take(300).collect::<String>())
    }
    serde_json::from_str(&text).with_context(|| format!("Z.AI の API 応答が JSON ではありません: {}", text.chars().take(200).collect::<String>()))
}

#[cfg(test)]
mod tests {
    use super::super::mock::Mock;
    use super::*;

    fn endpoints(origin: &str) -> Endpoints {
        Endpoints {
            client_id: CLIENT_ID.into(),
            authorize: format!("{origin}/api/oauth/authorize"),
            token: format!("{origin}/api/v1/oauth/token"),
            redirect_uri: REDIRECT_URI.into(),
            biz: origin.into(),
            business_login: format!("{origin}/api/auth/z/login"),
        }
    }

    /// Runs a whole sign-in through the pasted redirect, the way a browser unable to return does.
    async fn run(ep: Endpoints) -> Result<Cred> {
        let start = start_at(ep)?;
        start.paste.unwrap().send("zcode://zai-auth/callback?code=the-code".into()).unwrap();
        start.done.await
    }

    #[tokio::test]
    async fn signs_in_and_mints_a_key_from_a_new_api_key_slot() {
        let mock = Mock::start(&[
            ("200", r#"{"code":0,"data":{"zai":{"access_token":"oauth-token","refresh_token":"r","expires_in":3600}}}"#),
            ("200", r#"{"code":0,"success":true,"data":{"access_token":"biz-token"}}"#),
            ("200", r#"{"code":0,"data":{"organizations":[{"organizationId":"org-1","isDefault":true,"projects":[{"projectId":"proj-1","isDefault":true}]}]}}"#),
            ("200", r#"{"code":0,"data":{"list":[{"id":"k0","name":"zcode"}]}}"#),
            ("200", r#"{"success":true,"data":{"id":"k1","name":"greencode","apiKey":"api-key-1"}}"#),
            ("200", r#"{"code":0,"data":{"secretKey":"secret-1"}}"#),
        ])
        .await;
        let cred = run(endpoints(&mock.origin)).await.unwrap();
        assert!(matches!(cred, Cred::Key(ref k) if k == "api-key-1.secret-1"), "{cred:?}");

        let req = mock.requests();
        assert_eq!(req.len(), 6);
        assert!(req[0].path.ends_with("/api/v1/oauth/token"));
        let body = req[0].json();
        assert_eq!(body["provider"], "zai");
        assert_eq!(body["code"], "the-code");
        assert_eq!(body["redirect_uri"], REDIRECT_URI);
        assert!(body["state"].as_str().is_some_and(|s| !s.is_empty()));
        assert_eq!(req[0].header("content-type"), Some("application/json"));

        assert_eq!(req[1].json()["token"], "oauth-token");
        for r in &req[2..] {
            assert_eq!(r.header("authorization"), Some("Bearer biz-token"), "{}", r.path);
        }
        assert!(req[2].path.ends_with("/api/biz/customer/getCustomerInfo"));
        assert!(req[3].path.ends_with("/api/biz/v1/organization/org-1/projects/proj-1/api_keys"));
        assert_eq!(req[4].json()["name"], "greencode");
        assert!(req[5].path.ends_with("/api_keys/copy/api-key-1"));
    }

    #[tokio::test]
    async fn reuses_an_existing_key_and_reports_envelope_errors() {
        let mock = Mock::start(&[
            ("200", r#"{"code":0,"data":{"zai":{"access_token":"oauth-token"}}}"#),
            ("200", r#"{"code":0,"data":{"access_token":"biz-token"}}"#),
            ("200", r#"{"code":0,"data":{"organizations":[{"organizationId":"org-1","projects":[{"projectId":"proj-1"}]}]}}"#),
            ("200", r#"{"code":0,"data":[{"apiKey":"api-key-2","name":"greencode"}]}"#),
            ("200", r#"{"code":0,"data":{"secretKey":"secret-2"}}"#),
        ])
        .await;
        let cred = run(endpoints(&mock.origin)).await.unwrap();
        assert!(matches!(cred, Cred::Key(ref k) if k == "api-key-2.secret-2"), "{cred:?}");
        assert_eq!(mock.requests().len(), 5, "an existing key must not be created again");

        let denied = Mock::start(&[("200", r#"{"code":1001,"msg":"invalid code","success":false}"#)]).await;
        let err = run(endpoints(&denied.origin)).await.unwrap_err().to_string();
        assert!(err.contains("invalid code"), "{err}");
    }

    #[test]
    fn the_authorize_url_shows_the_desktop_redirect() {
        let start = start_at(Endpoints {
            client_id: "cid".into(),
            authorize: "https://chat.z.ai/api/oauth/authorize".into(),
            token: TOKEN_URL.into(),
            redirect_uri: REDIRECT_URI.into(),
            biz: BIZ_BASE.into(),
            business_login: BUSINESS_LOGIN_URL.into(),
        })
        .unwrap();
        let url = url::Url::parse(&start.url).unwrap();
        let q: std::collections::HashMap<_, _> = url.query_pairs().into_owned().collect();
        assert_eq!(url.host_str(), Some("chat.z.ai"));
        assert_eq!(q.get("client_id").map(String::as_str), Some("cid"));
        assert_eq!(q.get("response_type").map(String::as_str), Some("code"));
        assert_eq!(q.get("redirect_uri").map(String::as_str), Some(REDIRECT_URI));
        assert!(q.contains_key("state") && !q.contains_key("scope"), "no scope, no PKCE: {q:?}");
        assert!(start.code.is_none() && start.paste.is_some());
    }
}
