//! Kiro sign-in (OMP `registry/oauth/kiro.ts`): AWS IAM Identity Center / Builder ID through the
//! SSO-OIDC device grant, and Kiro's own social device flow for Google/GitHub.
//!
//! `Start` has no prompt channel, so the method comes from `KIRO_LOGIN_METHOD` (`builder` by default,
//! `idc`, `google`, or `github`); Identity Center also needs `KIRO_IDC_START_URL` and `KIRO_IDC_REGION`.
//! `ksk_` API keys are the ordinary API-key path. A profile, when the account has one, is the first
//! listed; Kiro only requires one for Identity Center accounts.

use super::{now_ms, post_json, Cred, OAuth, Start};
use crate::provider::kiro::{is_region, os, profile_region, service_headers, uuid_v4, LIST_PROFILES};
use anyhow::{anyhow, bail, Result};
use serde_json::{json, Map, Value};
use std::time::Duration;

const SCOPES: [&str; 3] = ["codewhisperer:completions", "codewhisperer:analysis", "codewhisperer:conversations"];
const BUILDER_ID_START_URL: &str = "https://view.awsapps.com/start";
const BUILDER_ID_REGION: &str = "us-east-1";
const SOCIAL_AUTH_URL: &str = "https://prod.us-east-1.auth.desktop.kiro.dev";
const SOCIAL_CLIENT_ID: &str = "Kiro-CLI";
/// The profile the CLI assigns a social account that lists none; the runtime rejects a bearer without one.
const SOCIAL_DEFAULT_PROFILE_ARN: &str = "arn:aws:codewhisperer:us-east-1:699475941385:profile/EHGA3GRVQMUK";
const DEFAULT_TOKEN_LIFETIME_MS: u64 = 3_600_000;
/// Commercial regions a profile listing is probed in when the login region does not pin it.
const BOOTSTRAP_REGIONS: [&str; 2] = ["us-east-1", "eu-central-1"];

/// The service hosts, under test the mock's own.
pub(crate) struct Ends {
    social: String,
    /// `{region}` becomes the login's OIDC region.
    oidc: String,
    /// `{region}` becomes the profile's region.
    management: String,
}

impl Default for Ends {
    fn default() -> Self {
        Self {
            social: SOCIAL_AUTH_URL.into(),
            oidc: "https://oidc.{region}.amazonaws.com".into(),
            management: "https://management.{region}.kiro.dev".into(),
        }
    }
}

/// Social auth's headers: its own two headers plus a plain JSON content type (OMP `KIRO_CLI_SOCIAL_HEADERS`).
fn social_headers() -> [(&'static str, &'static str); 4] {
    [
        ("content-type", "application/json"),
        ("user-agent", "Kiro-CLI"),
        ("accept", "*/*"),
        ("accept-encoding", "gzip"),
    ]
}

pub async fn login() -> Result<Start> {
    login_at(&Ends::default(), Duration::from_secs(1)).await
}

pub(crate) async fn login_at(ends: &Ends, unit: Duration) -> Result<Start> {
    let method = std::env::var("KIRO_LOGIN_METHOD").ok().map(|method| method.trim().to_ascii_lowercase());
    match method.as_deref() {
        Some("google") => social(ends, "Google", unit).await,
        Some("github") => social(ends, "Github", unit).await,
        Some("builder") | None => device(ends, BUILDER_ID_START_URL, BUILDER_ID_REGION, unit).await,
        Some("idc") => {
            let start_url = std::env::var("KIRO_IDC_START_URL").ok().filter(|url| url.starts_with("https://"))
                .ok_or_else(|| anyhow!("KIRO_IDC_START_URL に AWS アクセスポータルの URL（https://…）を設定してください"))?;
            let region = std::env::var("KIRO_IDC_REGION").ok().filter(|r| is_region(r))
                .ok_or_else(|| anyhow!("KIRO_IDC_REGION に Identity Center のリージョン（例: us-east-1）を設定してください"))?;
            device(ends, &start_url, &region, unit).await
        }
        Some(other) => bail!("KIRO_LOGIN_METHOD が不正です（{other}）。idc / builder / google / github のいずれかを指定してください"),
    }
}

fn host(template: &str, region: &str) -> String {
    template.replace("{region}", region)
}

/// Identity Center / Builder ID: register a public OIDC client, run the device grant, list a profile.
pub(crate) async fn device(ends: &Ends, start_url: &str, region: &str, unit: Duration) -> Result<Start> {
    let oidc = host(&ends.oidc, region);
    let oidc_headers = oidc_headers();
    let oidc_refs = refs(&oidc_headers);
    let client = post_json(
        &format!("{oidc}/client/register"),
        &json!({"clientName": "Kiro CLI", "clientType": "public", "scopes": SCOPES}),
        &oidc_refs,
    )
    .await?;
    let client_id = text(&client, "clientId")?;
    let client_secret = text(&client, "clientSecret")?;
    let auth = post_json(
        &format!("{oidc}/device_authorization"),
        &json!({"clientId": client_id, "clientSecret": client_secret, "startUrl": start_url}),
        &oidc_refs,
    )
    .await?;
    let device_code = text(&auth, "deviceCode")?;
    let user_code = text(&auth, "userCode")?;
    let url = auth["verificationUriComplete"].as_str().filter(|u| !u.is_empty())
        .ok_or_else(|| anyhow!("Kiro の認証応答に確認 URL がありません"))?
        .to_string();
    let interval = auth["interval"].as_u64().unwrap_or(1).max(1);
    let expires_in = auth["expiresIn"].as_u64().unwrap_or(600);
    let grant = json!({
        "clientId": client_id,
        "clientSecret": client_secret,
        "deviceCode": device_code,
        "grantType": "urn:ietf:params:oauth:grant-type:device_code",
    });
    let token_url = format!("{oidc}/token");
    let login_region = region.to_string();
    let client_id = client_id.to_string();
    let client_secret = client_secret.to_string();
    let builder = start_url == BUILDER_ID_START_URL;
    let ends = Ends { social: ends.social.clone(), oidc: ends.oidc.clone(), management: ends.management.clone() };
    Ok(super::device_start(
        super::Device { user_code, url, device_code: String::new(), interval, expires_in },
        move |_| async move {
            let v = poll(&token_url, &grant, &interval, expires_in, unit).await?;
            let mut cred = tokens(&v, None)?;
            cred.extra.insert("kiroClientId".into(), json!(client_id));
            cred.extra.insert("kiroClientSecret".into(), json!(client_secret));
            cred.extra.insert("kiroOidcRegion".into(), json!(login_region));
            cred.extra.insert("accountId".into(), json!(uuid_v4()));
            if builder {
                // Builder ID accounts route by region alone and carry no profile.
                cred.extra.insert("region".into(), json!(BUILDER_ID_REGION));
                return Ok(Cred::OAuth(cred));
            }
            let arn = first_profile(&ends, &cred.access, &login_region).await?;
            cred.extra.insert("kiroProfileArn".into(), json!(arn));
            cred.extra.insert("region".into(), json!(profile_region(&arn).unwrap_or(login_region)));
            Ok(Cred::OAuth(cred))
        },
    ))
}

/// Google/GitHub: the social device flow, whose poll answers HTTP 200 with the state in `status`.
pub(crate) async fn social(ends: &Ends, provider: &str, unit: Duration) -> Result<Start> {
    let headers = social_headers();
    let auth = post_json(
        &format!("{}/oauth/device/authorization", ends.social),
        &json!({"clientId": SOCIAL_CLIENT_ID, "loginProvider": provider}),
        &headers,
    )
    .await?;
    let device_code = text(&auth, "deviceCode")?;
    let user_code = text(&auth, "userCode")?;
    let url = auth["verificationUriComplete"].as_str().filter(|u| !u.is_empty())
        .ok_or_else(|| anyhow!("Kiro の認証応答に確認 URL がありません"))?
        .to_string();
    // The social service counts in milliseconds.
    let interval = auth["intervalInMilliseconds"].as_u64().map_or(5, |ms| (ms / 1000).max(1));
    let expires_in = auth["expiresInMilliseconds"].as_u64().map_or(600, |ms| ms / 1000);
    let poll_url = format!("{}/oauth/device/poll", ends.social);
    let label = if provider == "Github" { "GitHub" } else { "Google" };
    let ends = Ends { social: ends.social.clone(), oidc: ends.oidc.clone(), management: ends.management.clone() };
    Ok(super::device_start(
        super::Device { user_code, url, device_code: String::new(), interval, expires_in },
        move |_| async move {
            let body = json!({"deviceCode": device_code, "clientId": SOCIAL_CLIENT_ID});
            let mut wait = interval as f64;
            let mut waited = 0.0;
            let completed = loop {
                if waited >= expires_in as f64 {
                    bail!("ログインの有効期限が切れました。もう一度お試しください");
                }
                tokio::time::sleep(unit.mul_f64(wait)).await;
                waited += wait;
                let v = super::post_json(&poll_url, &body, &headers).await?;
                match v["status"].as_str() {
                    Some("authorization_pending") => {}
                    Some("slow_down") => wait += 5.0,
                    Some(_) if v["accessToken"].as_str().is_some_and(|t| !t.is_empty()) => break v,
                    _ => bail!("Kiro の {label} サインインが完了しませんでした"),
                }
            };
            let mut cred = tokens(&completed, None)?;
            cred.extra.insert("accountId".into(), json!(uuid_v4()));
            // The poll's own profile ARN wins; a listed one is next; otherwise the CLI's social default.
            let arn = match completed["profileArn"].as_str().filter(|arn| profile_region(arn).is_some()) {
                Some(arn) => arn.to_string(),
                None => match first_profile(&ends, &cred.access, BUILDER_ID_REGION).await {
                    Ok(arn) => arn,
                    Err(_) => SOCIAL_DEFAULT_PROFILE_ARN.into(),
                },
            };
            cred.extra.insert("region".into(), json!(profile_region(&arn).unwrap_or_else(|| BUILDER_ID_REGION.into())));
            cred.extra.insert("kiroProfileArn".into(), json!(arn));
            Ok(Cred::OAuth(cred))
        },
    ))
}

/// The first profile the bearer sees across the bootstrap regions; empty when the account has none.
async fn first_profile(ends: &Ends, token: &str, region: &str) -> Result<String> {
    let regions = if region.starts_with("us-gov-") { vec![region] } else { BOOTSTRAP_REGIONS.to_vec() };
    for region in regions {
        let headers = service_headers(LIST_PROFILES, token, 1, 1);
        let Ok(v) = service_post(&format!("{}/", host(&ends.management, region)), "{}", &headers).await else {
            continue;
        };
        let arn = v["profiles"].as_array().into_iter().flatten()
            .filter_map(|profile| profile["arn"].as_str())
            .find(|arn| profile_region(arn).is_some());
        if let Some(arn) = arn {
            return Ok(arn.to_string());
        }
    }
    bail!("Kiro のプロファイルが見つかりませんでした")
}

pub async fn refresh(cred: OAuth) -> Result<OAuth> {
    refresh_at(&Ends::default(), &cred).await
}

async fn refresh_at(ends: &Ends, cred: &OAuth) -> Result<OAuth> {
    let refresh = cred.refresh.as_deref().filter(|token| !token.is_empty())
        .ok_or_else(|| anyhow!("Kiro の更新トークンがありません。/login kiro でもう一度ログインしてください"))?;
    let extra = |key: &str| cred.extra.get(key).and_then(Value::as_str).map(str::to_string);
    let (Some(client_id), Some(client_secret), Some(region)) =
        (extra("kiroClientId"), extra("kiroClientSecret"), extra("kiroOidcRegion"))
    else {
        // No OIDC client: a social login, refreshed by the Kiro auth service (which may move its profile).
        let v = post_json(
            &format!("{}/refreshToken", ends.social),
            &json!({"refreshToken": refresh}),
            &social_headers(),
        )
        .await
        .map_err(|e| anyhow!("Kiro のサインインの有効期限が切れた可能性があります。/login kiro でもう一度ログインしてください（{e}）"))?;
        let mut next = tokens(&v, Some(cred))?;
        if let Some(moved) = v["profileArn"].as_str().filter(|arn| profile_region(arn).is_some()) {
            next.extra.insert("kiroProfileArn".into(), json!(moved));
            next.extra.insert("region".into(), json!(profile_region(moved).unwrap()));
        }
        return Ok(next);
    };
    let v = post_json(
        &format!("{}/token", host(&ends.oidc, &region)),
        &json!({
            "grantType": "refresh_token",
            "refreshToken": refresh,
            "clientId": client_id,
            "clientSecret": client_secret,
        }),
        &refs(&oidc_headers()),
    )
    .await
    .map_err(|e| anyhow!("Kiro のサインインの有効期限が切れた可能性があります。/login kiro でもう一度ログインしてください（{e}）"))?;
    tokens(&v, Some(cred))
}

/// An OIDC or social token answer as a credential; a refresh answer may omit the refresh token.
fn tokens(v: &Value, previous: Option<&OAuth>) -> Result<OAuth> {
    let access = v["accessToken"].as_str().filter(|token| !token.is_empty())
        .ok_or_else(|| anyhow!("Kiro の応答に accessToken がありません"))?;
    let refresh = v["refreshToken"].as_str().filter(|token| !token.is_empty()).map(str::to_string)
        .or_else(|| previous.and_then(|previous| previous.refresh.clone()))
        .ok_or_else(|| anyhow!("Kiro の応答に refreshToken がありません"))?;
    // The social service omits the lifetime; the CLI assumes an hour.
    let lifetime = v["expiresIn"].as_u64().filter(|seconds| *seconds > 0).map_or(DEFAULT_TOKEN_LIFETIME_MS, |s| s * 1000);
    Ok(OAuth {
        access: access.to_string(),
        refresh: Some(refresh),
        expires: Some(now_ms() + lifetime),
        extra: previous.map(|previous| previous.extra.clone()).unwrap_or_else(Map::new),
    })
}

/// The CLI's AWS SSO-OIDC client identity.
fn oidc_headers() -> Vec<(&'static str, String)> {
    let base = format!("aws-sdk-rust/1.6.0 ua/2.1 api/ssooidc/1.100.0 os/{} lang/rust/1.94.1", os());
    vec![
        ("content-type", "application/json".into()),
        ("user-agent", format!("{base} m/E,N app/AmazonQ-For-CLI")),
        ("x-amz-user-agent", format!("{base} m/E,N app/AmazonQ-For-CLI")),
        ("amz-sdk-request", "attempt=1; max=3".into()),
        ("amz-sdk-invocation-id", uuid_v4()),
        ("accept", "*/*".into()),
        ("accept-encoding", "gzip".into()),
    ]
}

fn refs<'a>(headers: &'a [(&'static str, String)]) -> Vec<(&'static str, &'a str)> {
    headers.iter().map(|(name, value)| (*name, value.as_str())).collect()
}

/// A device-code poll that retries a timed-out request on the next interval, as the CLI does.
async fn poll(url: &str, body: &Value, interval: &u64, expires_in: u64, unit: Duration) -> Result<Value> {
    let headers = oidc_headers();
    let mut wait = *interval as f64;
    let mut waited = 0.0;
    while waited < expires_in as f64 {
        tokio::time::sleep(unit.mul_f64(wait)).await;
        waited += wait;
        let v = match post_json(url, body, &refs(&headers)).await {
            Ok(v) => v,
            // Pending is an HTTP 400 with an OAuth error body; a timeout is worth another interval.
            Err(e) => match e.downcast_ref::<super::HttpError>() {
                Some(super::HttpError { body, .. }) => serde_json::from_str(body).unwrap_or_default(),
                None => continue,
            },
        };
        if v["accessToken"].as_str().is_some_and(|t| !t.is_empty()) || v["access_token"].as_str().is_some_and(|t| !t.is_empty()) {
            return Ok(v);
        }
        match v["error"].as_str() {
            Some("authorization_pending") => {}
            Some("slow_down") => wait += 5.0,
            Some("access_denied") => bail!("ログインが拒否されました"),
            Some("expired_token") => break,
            Some(e) => bail!("Kiro のデバイスサインインに失敗しました: {e}"),
            None => bail!("Kiro のデバイストークン応答が不正です: {v}"),
        }
    }
    bail!("ログインの有効期限が切れました。もう一度お試しください")
}

/// An AWS JSON 1.0 POST whose headers are exactly the CLI's (no default JSON content type).
async fn service_post(url: &str, body: &str, headers: &[(&'static str, String)]) -> Result<Value> {
    let mut request = reqwest::Client::new().post(url).timeout(Duration::from_secs(30)).body(body.to_string());
    for (name, value) in headers {
        request = request.header(*name, value);
    }
    let response = request.send().await?;
    let status = response.status();
    let text = response.text().await?;
    if !status.is_success() {
        return Err(super::HttpError { status, body: text }.into());
    }
    serde_json::from_str(&text).map_err(|_| anyhow!("Kiro の応答が JSON ではありません: {}", text.chars().take(200).collect::<String>()))
}

fn text(v: &Value, key: &str) -> Result<String> {
    v[key].as_str().filter(|s| !s.is_empty()).map(str::to_string)
        .ok_or_else(|| anyhow!("Kiro の応答に {key} がありません"))
}

#[cfg(test)]
mod tests {
    use super::super::mock::Mock;
    use super::*;

    fn ends(origin: &str) -> Ends {
        Ends {
            social: origin.to_string(),
            oidc: format!("{origin}/oidc/{{region}}"),
            management: format!("{origin}/management/{{region}}"),
        }
    }

    #[test]
    fn a_profile_arn_names_the_region_it_routes_to() {
        assert_eq!(profile_region("arn:aws:codewhisperer:eu-central-1:1:profile/ABC"), Some("eu-central-1".into()));
        assert_eq!(profile_region("arn:aws:codewhisperer:us-east-1:1:profile/ABC"), Some("us-east-1".into()));
        assert_eq!(profile_region("arn:aws-us-gov:codewhisperer:us-gov-west-1:1:profile/ABC"), Some("us-gov-west-1".into()));
        assert_eq!(profile_region("arn:aws:iam::1:user/x"), None);
        assert_eq!(profile_region("arn:aws:codewhisperer:us-east-1:1:profile/"), None);
        assert_eq!(profile_region("not an arn"), None);
        assert!(is_region("us-east-1") && is_region("us-gov-west-1") && !is_region("us-east"));
    }

    #[tokio::test]
    async fn builder_id_registers_a_client_and_polls_the_oidc_device_grant() {
        let mock = Mock::start(&[
            ("200 OK", r#"{"clientId":"cid","clientSecret":"csec"}"#),
            ("200 OK", r#"{"deviceCode":"dc","userCode":"UC-1","verificationUriComplete":"{origin}/device","interval":1,"expiresIn":60}"#),
            ("400 Bad Request", r#"{"error":"authorization_pending"}"#),
            ("200 OK", r#"{"accessToken":"at","refreshToken":"rt","expiresIn":3600}"#),
        ])
        .await;
        let start = device(&ends(&mock.origin), BUILDER_ID_START_URL, "us-east-1", Duration::from_millis(1)).await.unwrap();
        assert_eq!(start.url, format!("{}/device", mock.origin));
        assert_eq!(start.code.as_deref(), Some("UC-1"));
        let Cred::OAuth(cred) = start.done.await.unwrap() else { panic!("expected an oauth credential") };
        assert_eq!((cred.access.as_str(), cred.refresh.as_deref()), ("at", Some("rt")));
        assert_eq!(cred.extra["region"], "us-east-1");
        // Builder ID accounts have no profile to route with.
        assert!(cred.extra.get("kiroProfileArn").is_none());
        assert_eq!(cred.extra["kiroClientId"], "cid");
        assert_eq!(cred.extra["kiroClientSecret"], "csec");
        assert_eq!(cred.extra["kiroOidcRegion"], "us-east-1");
        assert!(cred.expires.unwrap() > crate::login::now_ms() + 3_000_000);

        let requests = mock.requests();
        assert_eq!(requests[0].path, "/oidc/us-east-1/client/register");
        assert_eq!(requests[0].json()["clientName"], "Kiro CLI");
        assert_eq!(requests[0].json()["clientType"], "public");
        assert_eq!(requests[0].json()["scopes"][0], "codewhisperer:completions");
        assert_eq!(requests[0].json()["scopes"].as_array().unwrap().len(), 3);
        let agent = requests[0].header("user-agent").unwrap();
        assert!(agent.contains("api/ssooidc/1.100.0") && agent.contains("app/AmazonQ-For-CLI"), "{agent}");
        assert_eq!(requests[0].header("x-amz-target"), None);
        assert_eq!(requests[1].path, "/oidc/us-east-1/device_authorization");
        assert_eq!(requests[1].json()["clientId"], "cid");
        assert_eq!(requests[1].json()["clientSecret"], "csec");
        assert_eq!(requests[1].json()["startUrl"], BUILDER_ID_START_URL);
        assert_eq!(requests[2].path, "/oidc/us-east-1/token");
        assert_eq!(requests[2].json()["grantType"], "urn:ietf:params:oauth:grant-type:device_code");
        assert_eq!(requests[2].json()["deviceCode"], "dc");
        assert_eq!(requests[3].json()["grantType"], "urn:ietf:params:oauth:grant-type:device_code");
    }

    #[tokio::test]
    async fn identity_center_picks_the_first_listed_profile_and_its_region() {
        let arn = "arn:aws:codewhisperer:eu-central-1:123:profile/XYZ";
        let mock = Mock::start(&[
            ("200 OK", r#"{"clientId":"cid","clientSecret":"csec"}"#),
            ("200 OK", r#"{"deviceCode":"dc","userCode":"UC","verificationUriComplete":"{origin}/d","interval":1,"expiresIn":60}"#),
            ("200 OK", r#"{"accessToken":"at","refreshToken":"rt","expiresIn":3600}"#),
            ("200 OK", r#"{"profiles":[{"arn":"arn:aws:codewhisperer:eu-central-1:123:profile/XYZ","profileName":"acme"}]}"#),
        ])
        .await;
        let start = device(&ends(&mock.origin), "https://acme.awsapps.com/start", "eu-central-1", Duration::from_millis(1)).await.unwrap();
        let Cred::OAuth(cred) = start.done.await.unwrap() else { panic!("expected an oauth credential") };
        assert_eq!(cred.extra["kiroProfileArn"], arn);
        assert_eq!(cred.extra["region"], "eu-central-1");
        let requests = mock.requests();
        assert_eq!(requests[1].json()["startUrl"], "https://acme.awsapps.com/start");
        // A commercial OIDC login lists profiles across the bootstrap regions, us-east-1 first.
        assert_eq!(requests[3].path, "/management/us-east-1/");
        let profile = &requests[3];
        assert_eq!(profile.header("x-amz-target"), Some(LIST_PROFILES));
        assert_eq!(profile.header("authorization"), Some("Bearer at"));
        assert_eq!(profile.header("content-type"), Some("application/x-amz-json-1.0"));
        assert!(profile.header("user-agent").is_some_and(|a| a.contains("md/appVersion-3.2.0")), "{:?}", profile.header("user-agent"));
        assert_eq!(profile.header("amz-sdk-request"), Some("attempt=1; max=1"));
        assert_eq!(profile.header("tokentype"), None);
    }

    #[tokio::test]
    async fn the_social_flow_polls_until_the_status_names_a_token() {
        let mock = Mock::start(&[
            ("200 OK", r#"{"deviceCode":"dc","userCode":"UC-2","verificationUriComplete":"{origin}/social","intervalInMilliseconds":1000,"expiresInMilliseconds":60000}"#),
            ("200 OK", r#"{"status":"authorization_pending"}"#),
            ("200 OK", r#"{"status":"success","accessToken":"sat","refreshToken":"srt","profileArn":"arn:aws:codewhisperer:us-east-1:9:profile/P"}"#),
        ])
        .await;
        let start = social(&ends(&mock.origin), "Github", Duration::from_millis(1)).await.unwrap();
        assert_eq!(start.code.as_deref(), Some("UC-2"));
        let Cred::OAuth(cred) = start.done.await.unwrap() else { panic!("expected an oauth credential") };
        assert_eq!((cred.access.as_str(), cred.refresh.as_deref()), ("sat", Some("srt")));
        assert_eq!(cred.extra["kiroProfileArn"], "arn:aws:codewhisperer:us-east-1:9:profile/P");
        assert_eq!(cred.extra["region"], "us-east-1");
        let requests = mock.requests();
        let auth = &requests[0];
        assert_eq!(auth.path, "/oauth/device/authorization");
        assert_eq!(auth.json()["clientId"], "Kiro-CLI");
        assert_eq!(auth.json()["loginProvider"], "Github");
        assert_eq!(auth.header("user-agent"), Some("Kiro-CLI"));
        assert_eq!(requests[1].path, "/oauth/device/poll");
        assert_eq!(requests[1].json()["deviceCode"], "dc");
        assert_eq!(requests[1].json()["clientId"], "Kiro-CLI");
    }

    #[tokio::test]
    async fn social_refresh_moves_the_profile_and_keeps_the_old_refresh_token_when_omitted() {
        let mock = Mock::start(&[(
            "200 OK",
            r#"{"accessToken":"a2","profileArn":"arn:aws:codewhisperer:eu-central-1:9:profile/Q"}"#,
        )])
        .await;
        let cred = OAuth {
            access: "a1".into(),
            refresh: Some("r1".into()),
            expires: None,
            extra: Map::from_iter([
                ("kiroProfileArn".to_string(), json!("arn:aws:codewhisperer:us-east-1:9:profile/P")),
                ("region".to_string(), json!("us-east-1")),
            ]),
        };
        let next = refresh_at(&ends(&mock.origin), &cred).await.unwrap();
        assert_eq!((next.access.as_str(), next.refresh.as_deref()), ("a2", Some("r1")));
        assert_eq!(next.extra["region"], "eu-central-1");
        assert_eq!(next.extra["kiroProfileArn"], "arn:aws:codewhisperer:eu-central-1:9:profile/Q");
        let request = &mock.requests()[0];
        assert_eq!(request.path, "/refreshToken");
        assert_eq!(request.json()["refreshToken"], "r1");
        assert_eq!(request.header("user-agent"), Some("Kiro-CLI"));
    }

    #[tokio::test]
    async fn an_oidc_refresh_posts_the_client_it_registered() {
        let mock = Mock::start(&[("200 OK", r#"{"accessToken":"a2","refreshToken":"r2","expiresIn":7200}"#)]).await;
        let cred = OAuth {
            access: "a1".into(),
            refresh: Some("r1".into()),
            expires: None,
            extra: Map::from_iter([
                ("kiroClientId".to_string(), json!("cid")),
                ("kiroClientSecret".to_string(), json!("csec")),
                ("kiroOidcRegion".to_string(), json!("eu-central-1")),
            ]),
        };
        let next = refresh_at(&ends(&mock.origin), &cred).await.unwrap();
        assert_eq!((next.access.as_str(), next.refresh.as_deref()), ("a2", Some("r2")));
        assert_eq!(next.extra["kiroClientId"], "cid");
        let request = &mock.requests()[0];
        assert_eq!(request.path, "/oidc/eu-central-1/token");
        assert_eq!(request.json()["grantType"], "refresh_token");
        assert_eq!(request.json()["clientId"], "cid");
        assert_eq!(request.json()["clientSecret"], "csec");
        assert!(request.header("amz-sdk-invocation-id").is_some_and(|id| id.len() == 36), "{:?}", request.header("amz-sdk-invocation-id"));
    }
}
