//! Cursor sign-in: the browser-poll flow the Cursor CLI uses (OMP
//! `registry/oauth/cursor.ts`, rule `auth/cursor.kdl`). The page shows a code only
//! in the browser; this process polls `auth/poll` with its PKCE verifier until the
//! account is approved, and renews the session with `oauth/token`.

use super::{pkce, Cred, OAuth, Start};
use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};
use std::time::Duration;

const LOGIN_URL: &str = "https://cursor.com/loginDeepControl";
const POLL_URL: &str = "https://api2.cursor.sh/auth/poll";
const REFRESH_URL: &str = "https://api2.cursor.sh/oauth/token";
/// The client the Cursor IDE renews its login session with.
const CLIENT_ID: &str = "KbZUR41cY7W6zRSdpSUJ7I7mLYBKOCmB";
const POLL_MAX_ATTEMPTS: u32 = 150;
const POLL_MAX_DELAY: f64 = 10.0;
const POLL_BACKOFF: f64 = 1.2;
/// Cursor answers 200 with an empty token when the session ended.
const LOGGED_OUT: &str = "Cursor のセッションが終了しました。/login cursor でもう一度サインインしてください";

pub async fn login() -> Result<Start> {
    login_at(LOGIN_URL, POLL_URL, Duration::from_secs(1)).await
}

async fn login_at(login_url: &str, poll_url: &str, unit: Duration) -> Result<Start> {
    let (verifier, challenge) = pkce()?;
    let uuid = uuid_v4();
    let url = url::Url::parse_with_params(
        login_url,
        &[("challenge", challenge.as_str()), ("uuid", &uuid), ("mode", "login"), ("redirectTarget", "cli")],
    )?
    .to_string();
    let poll_url = poll_url.to_string();
    Ok(Start { url, code: None, paste: None, done: Box::pin(async move { poll(&poll_url, &uuid, &verifier, unit).await }) })
}

/// Polls until the user approves: 404 means still waiting, and three consecutive
/// other failures (or 150 attempts) give up.
async fn poll(poll_url: &str, uuid: &str, verifier: &str, unit: Duration) -> Result<Cred> {
    let http = reqwest::Client::new();
    let mut delay = 1.0;
    let mut errors = 0;
    for _ in 0..POLL_MAX_ATTEMPTS {
        tokio::time::sleep(unit.mul_f64(delay)).await;
        delay = (delay * POLL_BACKOFF).min(POLL_MAX_DELAY);
        let url = url::Url::parse_with_params(poll_url, &[("uuid", uuid), ("verifier", verifier)])?;
        let answer = http.get(url.clone()).timeout(Duration::from_secs(30)).send().await;
        let resp = match answer {
            Ok(resp) => resp,
            Err(_) => {
                if errors >= 2 {
                    bail!("Cursor のサインイン確認に繰り返し失敗しました。もう一度お試しください");
                }
                errors += 1;
                continue;
            }
        };
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            errors = 0;
            continue;
        }
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            if errors >= 2 {
                bail!("Cursor のサインイン確認に失敗しました (HTTP {status})。もう一度お試しください");
            }
            errors += 1;
            continue;
        }
        let v: Value = serde_json::from_str(&body).with_context(|| format!("Cursor の応答が JSON ではありません: {body}"))?;
        let access = v["accessToken"].as_str().filter(|s| !s.is_empty())
            .ok_or_else(|| anyhow!("Cursor の応答に accessToken がありません"))?;
        let refresh = v["refreshToken"].as_str().filter(|s| !s.is_empty()).map(str::to_string);
        return Ok(Cred::OAuth(OAuth { access: access.into(), refresh, expires: Some(expiry(access)), extra: Default::default() }));
    }
    bail!("Cursor のサインインがタイムアウトしました。もう一度お試しください")
}

pub async fn refresh(cred: OAuth) -> Result<OAuth> {
    refresh_at(REFRESH_URL, cred).await
}

async fn refresh_at(url: &str, cred: OAuth) -> Result<OAuth> {
    let refresh = cred.refresh.as_deref().filter(|r| !r.is_empty()).ok_or_else(|| anyhow!(LOGGED_OUT))?;
    let body = json!({"grant_type": "refresh_token", "client_id": CLIENT_ID, "refresh_token": refresh});
    let resp = reqwest::Client::new().post(url).timeout(Duration::from_secs(30)).json(&body).send().await
        .with_context(|| format!("POST {url}"))?;
    let status = resp.status();
    let text = resp.text().await?;
    let v: Value = serde_json::from_str(&text).unwrap_or_default();
    if v["shouldLogout"].as_bool() == Some(true) {
        bail!("{LOGGED_OUT}");
    }
    if !status.is_success() {
        bail!("Cursor のトークンを更新できませんでした (HTTP {status}): {}", text.chars().take(300).collect::<String>());
    }
    let access = v["access_token"].as_str().filter(|s| !s.is_empty()).ok_or_else(|| anyhow!("{LOGGED_OUT}"))?;
    // A renewal usually answers only an access token; keep the stored refresh token then.
    let next_refresh = v["refresh_token"].as_str().filter(|s| !s.is_empty()).map_or_else(|| Some(refresh.to_string()), |r| Some(r.to_string()));
    Ok(OAuth { access: access.into(), refresh: next_refresh, expires: Some(expiry(access)), extra: cred.extra })
}

/// A JWT `exp` five minutes early, else an hour out: the margin covers clock skew and request time.
fn expiry(access: &str) -> u64 {
    super::jwt_expiry(access).map(|at| at.saturating_sub(300_000)).unwrap_or_else(|| super::now_ms() + 3_600_000)
}

fn uuid_v4() -> String {
    let mut b = [0u8; 16];
    let _ = getrandom::fill(&mut b);
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!("{}-{}-{}-{}-{}", &h[..8], &h[8..12], &h[12..16], &h[16..20], &h[20..32])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::localserver::{serve, Request, Response};
    use base64::Engine as _;
    use std::sync::{Arc, Mutex};

    fn jwt(claims: &str) -> String {
        format!("e30.{}.x", base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(claims))
    }

    /// 404 twice, then a token pair; records every requested path and query.
    async fn server(polls: usize) -> (String, Arc<Mutex<Vec<Request>>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let seen: Arc<Mutex<Vec<Request>>> = Arc::new(Mutex::new(Vec::new()));
        let log = seen.clone();
        tokio::spawn(async move {
            let _ = serve(listener, 1 << 20, move |req: Request| {
                let log = log.clone();
                async move {
                    let token = jwt(&format!(r#"{{"exp":{},"sub":"auth0|u1"}}"#, super::super::now_ms() / 1000 + 3600));
                    let refresh = req.path.ends_with("/oauth/token");
                    let pending = !refresh && log.lock().unwrap().iter().filter(|r| r.path == "/auth/poll").count() < polls;
                    log.lock().unwrap().push(req);
                    if pending {
                        Response::new("404 Not Found", "application/json", "{}")
                    } else if refresh {
                        // A renewal usually answers only an access token.
                        Response::json(json!({"access_token": token}))
                    } else {
                        Response::json(json!({"accessToken": token, "refreshToken": "r1"}))
                    }
                }
            }).await;
        });
        (origin, seen)
    }

    #[tokio::test]
    async fn poll_waits_through_404_and_stores_the_refresh_token() {
        let (origin, seen) = server(2).await;
        let start = login_at("http://login.invalid/", &format!("{origin}/auth/poll"), Duration::from_millis(1)).await.unwrap();
        assert!(start.url.starts_with("http://login.invalid/") && start.url.contains("redirectTarget=cli"));
        let cred = start.done.await.unwrap();
        let Cred::OAuth(cred) = cred else { panic!("not oauth") };
        assert_eq!(cred.refresh.as_deref(), Some("r1"));
        assert!(cred.expires.unwrap() > super::super::now_ms() + 3_000_000);
        // The profile lookup is a separate endpoint; only the polls reached `/auth/poll`.
        let seen = seen.lock().unwrap();
        let polls: Vec<_> = seen.iter().filter(|r| r.path == "/auth/poll").collect();
        assert_eq!(polls.len(), 3);
        assert!(polls[0].query("uuid").is_some() && polls[0].query("verifier").is_some());
    }

    #[tokio::test]
    async fn refresh_keeps_the_old_refresh_token_and_reports_ended_sessions() {
        let (origin, seen) = server(0).await;
        let mut cred = OAuth { access: "old".into(), refresh: Some("r0".into()), expires: None, extra: Default::default() };
        cred.extra.insert("email".into(), json!("a@b"));
        let v = refresh_at(&format!("{origin}/oauth/token"), cred).await.unwrap();
        assert_eq!(v.refresh.as_deref(), Some("r0"));
        assert_eq!(v.extra["email"], "a@b");
        let sent: Value = {
            let seen = seen.lock().unwrap();
            serde_json::from_slice(&seen.last().unwrap().body).unwrap()
        };
        assert_eq!(sent["client_id"], CLIENT_ID);
        assert_eq!(sent["grant_type"], "refresh_token");

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/oauth/token", listener.local_addr().unwrap());
        tokio::spawn(async move {
            let _ = serve(listener, 1 << 20, |_req: Request| async {
                Response::json(json!({"shouldLogout": true}))
            }).await;
        });
        let cred = OAuth { access: "old".into(), refresh: Some("r0".into()), expires: None, extra: Default::default() };
        let err = refresh_at(&url, cred).await.unwrap_err().to_string();
        assert!(err.contains("/login cursor"), "{err}");
    }
}
