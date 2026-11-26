//! Factory Droid: WorkOS device sign-in, then the account's Factory organization and region
//! (OMP `registry/oauth/factory-droid.ts`, rule `auth/factory-droid.kdl`).

use super::{device, device_start, jwt_claims, jwt_expiry, poll_every, post_form, Cred, OAuth, Start};
use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Map, Value};
use std::time::Duration;

const CLIENT_ID: &str = "client_01HNM792M5G5G1A2THWPXKFMXB";
const WORKOS: &str = "https://api.workos.com";
const API_GLOBAL: &str = "https://api.factory.ai";
const API_EU: &str = "https://api.eu.factory.ai";
const ACCEPT: [(&str, &str); 1] = [("Accept", "application/json")];
/// Fallback when the access token carries no `exp` claim.
const FALLBACK_MS: u64 = 86_400_000;

struct Endpoints {
    workos: String,
    global: String,
    eu: String,
}

impl Endpoints {
    fn production() -> Self {
        Self { workos: WORKOS.into(), global: API_GLOBAL.into(), eu: API_EU.into() }
    }

    /// Factory's API host follows the account's residency.
    fn api(&self, region: Option<&str>) -> &str {
        if region == Some("eu") { &self.eu } else { &self.global }
    }

    fn authenticate(&self) -> String {
        format!("{}/user_management/authenticate", self.workos)
    }
}

pub async fn login() -> Result<Start> {
    login_at(Endpoints::production(), Duration::from_secs(1)).await
}

async fn login_at(ep: Endpoints, unit: Duration) -> Result<Start> {
    let device = device(
        &format!("{}/user_management/authorize/device", ep.workos),
        &[("client_id", CLIENT_ID)],
        &ACCEPT,
    )
    .await?;
    Ok(device_start(device, move |device| async move {
        let v = poll_every(&device, &ep.authenticate(), &[("client_id", CLIENT_ID)], &ACCEPT, unit).await?;
        let mut cred = credential(&v)?;
        resolve(&ep, &mut cred, true).await?;
        Ok(Cred::OAuth(cred))
    }))
}

pub async fn refresh(cred: OAuth) -> Result<OAuth> {
    refresh_at(Endpoints::production(), cred).await
}

async fn refresh_at(ep: Endpoints, cred: OAuth) -> Result<OAuth> {
    let refresh = cred.refresh.as_deref().filter(|r| !r.is_empty())
        .ok_or_else(|| anyhow!("Factory の更新トークンがありません。/login factory-droid でもう一度ログインしてください"))?;
    let active = cred.extra.get("active_organization_id").and_then(Value::as_str);
    let v = authenticate(&ep, &[("grant_type", "refresh_token"), ("refresh_token", refresh)], active).await?;
    let mut next = credential(&v)?;
    for key in ["email", "account_id"] {
        if let Some(value) = cred.extra.get(key) { next.extra.insert(key.into(), value.clone()); }
    }
    // A token without an org claim keeps the stored scope; a different claim starts a new one.
    if !next.extra.contains_key("org_id") {
        for key in ["org_id", "active_organization_id", "region", "inference_region"] {
            if let Some(value) = cred.extra.get(key) { next.extra.insert(key.into(), value.clone()); }
        }
    }
    // A rejected whoami during a refresh keeps the stored scope instead of failing the request.
    resolve(&ep, &mut next, false).await?;
    Ok(next)
}

/// The WorkOS token exchange (device grant and organization-scoped refresh alike).
async fn authenticate(ep: &Endpoints, form: &[(&str, &str)], organization: Option<&str>) -> Result<Value> {
    let mut fields = vec![("client_id", CLIENT_ID)];
    fields.extend_from_slice(form);
    if let Some(org) = organization { fields.push(("organization_id", org)); }
    post_form(&ep.authenticate(), &fields, &ACCEPT).await.context("Factory のトークン交換に失敗しました")
}

/// WorkOS's token answer as a credential; identity and Factory scope follow from the JWT and `/api/cli/whoami`.
fn credential(v: &Value) -> Result<OAuth> {
    let access = v["access_token"].as_str().filter(|t| !t.is_empty())
        .ok_or_else(|| anyhow!("Factory のトークン応答にアクセストークンがありません"))?
        .to_string();
    let refresh = v["refresh_token"].as_str().filter(|t| !t.is_empty()).map(str::to_string);
    let claims = jwt_claims(&access).unwrap_or_default();
    let text = |key: &str| claims.get(key).and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_string);
    let user = v.get("user").cloned().unwrap_or(Value::Null);
    let mut extra = Map::new();
    let id = user["id"].as_str().map(str::to_string).or_else(|| text("sub"));
    if let Some(id) = id {
        extra.insert("account_id".into(), json!(id));
    }
    let email = user["email"].as_str().map(str::to_string).or_else(|| text("email"));
    if let Some(email) = email {
        extra.insert("email".into(), json!(email));
    }
    // Only the WorkOS `external_org_id` claim is a Factory org id; the WorkOS organization is internal.
    if let Some(org) = text("external_org_id") { extra.insert("org_id".into(), json!(org)); }
    if let Some(org) = v["organization_id"].as_str().filter(|s| !s.is_empty()) {
        extra.insert("active_organization_id".into(), json!(org));
    }
    Ok(OAuth { access, refresh, expires: jwt_expiry(v["access_token"].as_str().unwrap_or("")).or(Some(super::now_ms() + FALLBACK_MS)), extra })
}

/// Fills the Factory org and residency from `/api/cli/whoami`, scoping the grant to the account's
/// WorkOS organization first when the token carries no Factory org (`phase == login` fails hard).
async fn resolve(ep: &Endpoints, cred: &mut OAuth, login_phase: bool) -> Result<()> {
    let mut who = whoami(ep, cred, login_phase).await?;
    let accepted = |v: &Option<Value>| v.as_ref().and_then(|v| v["orgId"].as_str()).is_some_and(|s| !s.is_empty());
    if !cred.extra.contains_key("org_id") && !accepted(&who) && !cred.extra.contains_key("active_organization_id") {
        if let Some(workos) = lookup_org(ep, cred).await {
            cred.extra.insert("active_organization_id".into(), json!(workos));
            if let Ok(scoped) = scoped_refresh(ep, cred).await {
                // The scoped grant rotated the refresh token: keep it whether or not whoami is happy.
                *cred = scoped;
                who = whoami(ep, cred, login_phase).await?;
            }
        }
    }
    if let Some(v) = &who {
        apply_scope(cred, v);
    }
    if login_phase && !cred.extra.contains_key("org_id") {
        bail!("Factory のログインで組織を特定できませんでした。/login factory-droid でもう一度お試しください")
    }
    Ok(())
}

/// `GET /api/cli/whoami`; `None` on any failure outside a login, where the status is an error.
async fn whoami(ep: &Endpoints, cred: &OAuth, login_phase: bool) -> Result<Option<Value>> {
    let region = cred.extra.get("region").and_then(Value::as_str);
    let url = format!("{}/api/cli/whoami", ep.api(region));
    let mut r = reqwest::Client::new().get(&url)
        .timeout(Duration::from_secs(15))
        .bearer_auth(&cred.access)
        .header("Accept", "application/json")
        .header("X-Factory-Whoami-Extended", "true");
    if let Some(org) = cred.extra.get("org_id").and_then(Value::as_str) {
        r = r.header("X-Factory-Org-Id", org);
    }
    let resp = match r.send().await {
        Ok(resp) => resp,
        Err(e) if login_phase => return Err(e).with_context(|| format!("GET {url}")),
        Err(_) => return Ok(None),
    };
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        if login_phase {
            bail!("Factory のアカウント確認に失敗しました（{status}）: {}", body.chars().take(300).collect::<String>())
        }
        return Ok(None);
    }
    Ok(serde_json::from_str(&body).ok())
}

/// The WorkOS organization a bearer without an org claim belongs to, from `/api/cli/org`.
async fn lookup_org(ep: &Endpoints, cred: &OAuth) -> Option<String> {
    let region = cred.extra.get("region").and_then(Value::as_str);
    let url = format!("{}/api/cli/org", ep.api(region));
    let resp = reqwest::Client::new().get(&url)
        .timeout(Duration::from_secs(15))
        .bearer_auth(&cred.access)
        .header("Accept", "application/json")
        .send().await.ok()?;
    if !resp.status().is_success() { return None }
    let v: Value = resp.json().await.ok()?;
    v["workosOrgIds"].as_array()?.first()?.as_str().filter(|s| !s.is_empty()).map(str::to_string)
}

/// Re-runs the refresh grant scoped to `active_organization_id` so the new bearer carries the org claim.
async fn scoped_refresh(ep: &Endpoints, cred: &OAuth) -> Result<OAuth> {
    let refresh = cred.refresh.as_deref().ok_or_else(|| anyhow!("Factory の更新トークンがありません"))?;
    let org = cred.extra.get("active_organization_id").and_then(Value::as_str);
    let v = authenticate(ep, &[("grant_type", "refresh_token"), ("refresh_token", refresh)], org).await?;
    let mut next = credential(&v)?;
    for key in ["email", "account_id"] {
        if let Some(value) = cred.extra.get(key) { next.extra.insert(key.into(), value.clone()); }
    }
    Ok(next)
}

/// `whoami`'s organization and residency; a different org must not inherit the stored scope.
fn apply_scope(cred: &mut OAuth, v: &Value) {
    let stored_org = cred.extra.get("org_id").and_then(Value::as_str).map(str::to_string);
    let kept = match v["orgId"].as_str().filter(|s| !s.is_empty()) {
        Some(org) => stored_org.as_deref() == Some(org),
        None => true,
    };
    let org = v["orgId"].as_str().filter(|s| !s.is_empty()).map(str::to_string).or(stored_org);
    if let Some(org) = org { cred.extra.insert("org_id".into(), json!(org)); }
    let stored_region = kept.then(|| cred.extra.get("region").and_then(Value::as_str).map(str::to_string)).flatten();
    let region = match v["region"].as_str() {
        Some("eu") => Some("eu".to_string()),
        Some("global") => Some("global".to_string()),
        _ => stored_region,
    };
    let inference = match v["inferenceRegion"].as_str() {
        Some(r @ ("global" | "eu" | "us")) => Some(r.to_string()),
        _ if kept => cred.extra.get("inference_region").and_then(Value::as_str).map(str::to_string),
        _ => None,
    }
    .or_else(|| region.as_deref().map(|r| if r == "eu" { "eu" } else { "global" }).map(str::to_string));
    if let Some(region) = region { cred.extra.insert("region".into(), json!(region)); }
    if let Some(inference) = inference { cred.extra.insert("inference_region".into(), json!(inference)); }
}

#[cfg(test)]
mod tests {
    use super::super::mock::Mock;
    use super::*;

    fn endpoints(origin: &str) -> Endpoints {
        Endpoints { workos: origin.into(), global: origin.into(), eu: origin.into() }
    }

    /// Answers the device grant, whoami, org lookup, scoped refresh and the confirming whoami.
    async fn login_with(mock: &Mock) -> Result<Cred> {
        let ep = endpoints(&mock.origin);
        let start = login_at(ep, Duration::from_millis(1)).await?;
        start.done.await
    }

    #[tokio::test]
    async fn signs_in_and_scopes_to_the_accounts_workos_org() {
        let mock = Mock::start(&[
            ("200", r#"{"device_code":"dc","user_code":"AB-12","verification_uri":"{origin}/device","interval":1,"expires_in":60}"#),
            ("200", r#"{"access_token":"eyJhbGciOiJub25lIn0.eyJzdWIiOiJ1c2VyLTEiLCJlbWFpbCI6ImFAYi5jIn0.","refresh_token":"r1","user":{"id":"user-1","email":"A@B.c"}}"#),
            ("200", r#"{"orgId":null}"#),
            ("200", r#"{"workosOrgIds":["org_w1"]}"#),
            ("200", r#"{"access_token":"eyJhbGciOiJub25lIn0.eyJzdWIiOiJ1c2VyLTEiLCJleHRlcm5hbF9vcmdfaWQiOiJmYWMtMSJ9.","refresh_token":"r2","organization_id":"org_w1"}"#),
            ("200", r#"{"orgId":"fac-1","region":"eu","inferenceRegion":"us"}"#),
        ])
        .await;
        let cred = login_with(&mock).await.unwrap();
        let Cred::OAuth(cred) = cred else { panic!("expected an OAuth credential") };
        assert_eq!(cred.access, "eyJhbGciOiJub25lIn0.eyJzdWIiOiJ1c2VyLTEiLCJleHRlcm5hbF9vcmdfaWQiOiJmYWMtMSJ9.");
        assert_eq!(cred.refresh.as_deref(), Some("r2"));
        assert_eq!(cred.extra["org_id"], "fac-1");
        assert_eq!(cred.extra["active_organization_id"], "org_w1");
        assert_eq!(cred.extra["region"], "eu");
        assert_eq!(cred.extra["inference_region"], "us");
        assert_eq!(cred.extra["account_id"], "user-1");
        assert_eq!(cred.extra["email"], "A@B.c");
        // A JWT without `exp` falls back to a day of validity.
        assert!(cred.expires.unwrap() > super::super::now_ms() + 3_000_000);

        let req = mock.requests();
        assert_eq!(req[0].path, "/user_management/authorize/device");
        assert_eq!(req[0].field("client_id").as_deref(), Some(CLIENT_ID));
        assert_eq!(req[1].path, "/user_management/authenticate");
        assert_eq!(req[1].field("grant_type").as_deref(), Some("urn:ietf:params:oauth:grant-type:device_code"));
        assert_eq!(req[1].field("device_code").as_deref(), Some("dc"));
        assert_eq!(req[2].header("authorization"), Some("Bearer eyJhbGciOiJub25lIn0.eyJzdWIiOiJ1c2VyLTEiLCJlbWFpbCI6ImFAYi5jIn0."));
        assert_eq!(req[2].header("x-factory-whoami-extended"), Some("true"));
        assert_eq!(req[2].path, "/api/cli/whoami");
        assert_eq!(req[3].path, "/api/cli/org");
        assert_eq!(req[4].field("grant_type").as_deref(), Some("refresh_token"));
        assert_eq!(req[4].field("organization_id").as_deref(), Some("org_w1"));
        assert_eq!(req[5].header("x-factory-org-id"), Some("fac-1"));
    }

    #[tokio::test]
    async fn an_org_claim_skips_the_scoped_refresh() {
        let mock = Mock::start(&[
            ("200", r#"{"device_code":"dc","user_code":"U","verification_uri":"{origin}/d","interval":1,"expires_in":60}"#),
            ("200", r#"{"access_token":"eyJhbGciOiJub25lIn0.eyJleHRlcm5hbF9vcmdfaWQiOiJmYWMtOSJ9.","refresh_token":"r1"}"#),
            ("200", r#"{"orgId":"fac-9","region":"global"}"#),
        ])
        .await;
        let Cred::OAuth(cred) = login_with(&mock).await.unwrap() else { panic!("expected OAuth") };
        assert_eq!(cred.extra["org_id"], "fac-9");
        assert_eq!(cred.extra["inference_region"], "global");
        assert_eq!(mock.requests().len(), 3, "whoami only: no lookup, no second grant");
    }

    #[tokio::test]
    async fn a_whoami_without_an_org_fails_the_login() {
        let mock = Mock::start(&[
            ("200", r#"{"device_code":"dc","user_code":"U","verification_uri":"{origin}/d","interval":1,"expires_in":60}"#),
            ("200", r#"{"access_token":"eyJhbGciOiJub25lIn0.eyJzdWIiOiJ1In0.","refresh_token":"r1"}"#),
            ("401", r#"{"message":"unauthorized"}"#),
        ])
        .await;
        let err = login_with(&mock).await.unwrap_err().to_string();
        assert!(err.contains("アカウント確認"), "{err}");
    }

    #[tokio::test]
    async fn refresh_keeps_the_stored_identity_and_scope() {
        let mock = Mock::start(&[
            ("200", r#"{"access_token":"fresh","refresh_token":"r2"}"#),
            ("500", r#"{}"#),
        ])
        .await;
        let stored = OAuth {
            access: "old".into(),
            refresh: Some("r1".into()),
            expires: None,
            extra: Map::from_iter([
                ("org_id".into(), json!("fac-1")),
                ("active_organization_id".into(), json!("org_w1")),
                ("region".into(), json!("eu")),
                ("inference_region".into(), json!("eu")),
                ("account_id".into(), json!("u1")),
                ("email".into(), json!("a@b.c")),
            ]),
        };
        let next = refresh_at(endpoints(&mock.origin), stored).await.unwrap();
        assert_eq!((next.access.as_str(), next.refresh.as_deref()), ("fresh", Some("r2")));
        assert_eq!(next.extra["org_id"], "fac-1");
        assert_eq!(next.extra["region"], "eu");
        assert_eq!(next.extra["email"], "a@b.c");
        let req = mock.requests();
        assert_eq!(req[0].path, "/user_management/authenticate");
        assert_eq!(req[0].field("grant_type").as_deref(), Some("refresh_token"));
        assert_eq!(req[0].field("refresh_token").as_deref(), Some("r1"));
        assert_eq!(req[0].field("organization_id").as_deref(), Some("org_w1"));
    }
}
