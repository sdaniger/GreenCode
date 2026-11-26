//! Auto-redeem of saved Codex / Claude resets when a provider request hits a usage limit
//! (`codexResets.*`, `claudeResets.*`).
//!
//! The decision is pure (`decide`); the HTTP flows only gather credits and post the redemption.
use crate::config::{services_extra::ResetPolicy, Config};
use anyhow::{anyhow, Context, Result};
use serde_json::Value;
use std::future::Future;

/// One saved reset: `expires_at` is unix seconds.
#[derive(Clone, Debug, PartialEq)]
pub struct Credit {
    pub id: String,
    pub status: String,
    pub expires_at: Option<u64>,
    /// Windows this credit restores, as provider ids.
    pub clears: Vec<String>,
}

/// A window the provider reports, used to tell "meaningful usage" from an untouched window.
#[derive(Clone, Debug, PartialEq)]
pub struct Window {
    pub id: String,
    /// 0-1 used.
    pub used: f64,
    pub resets_at: Option<u64>,
}

/// Five minutes before expiry a credit is spent regardless of usage or reserve.
const EXPIRING_SOON: u64 = 300;
/// A window at or above this used fraction counts as meaningful usage.
const MEANINGFUL: f64 = 0.25;

#[derive(Clone, Debug, PartialEq)]
pub struct Decision {
    pub credit_id: String,
    pub reason: &'static str,
}

/// Whether to spend a credit now. `blocked_wait` is the limit reply's own wait, used when no
/// usage report names the exhausted windows.
pub fn decide(
    policy: &ResetPolicy,
    credits: &[Credit],
    windows: &[Window],
    blocked_wait: Option<u64>,
    now: u64,
) -> Option<Decision> {
    let available: Vec<&Credit> = credits
        .iter()
        .filter(|c| c.status == "available" && c.expires_at.is_none_or(|at| at > now))
        .collect();
    if available.is_empty() {
        return None;
    }
    let expiring = |c: &&Credit| c.expires_at.is_some_and(|at| at <= now + EXPIRING_SOON);
    let willing = |n: usize| (n as u64) > policy.keep_credits;
    // 1. A credit about to expire preserves nothing: spend it even below the reserve.
    if let Some(c) = available.iter().find(|c| expiring(c)) {
        return Some(Decision { credit_id: c.id.clone(), reason: "expiring" });
    }
    if !willing(available.len()) {
        return None;
    }
    // 2. A block far enough out is worth a credit; only exhausted windows count as blocking.
    let unblock = windows
        .iter()
        .filter(|w| w.used >= 0.999)
        .filter_map(|w| w.resets_at)
        .max();
    let wait = match (unblock, blocked_wait) {
        (Some(at), _) => at.saturating_sub(now),
        (None, Some(wait)) => wait,
        (None, None) => 0,
    };
    if wait >= policy.min_blocked_minutes.saturating_mul(60) {
        return Some(Decision { credit_id: available[0].id.clone(), reason: "blocked" });
    }
    // 3. Salvage: a credit close to expiry while the windows it covers have real usage.
    if policy.salvage_horizon_hours > 0 {
        let horizon = now + policy.salvage_horizon_hours * 3600;
        if let Some(c) = available.iter().find(|c| {
            c.expires_at.is_some_and(|at| at <= horizon)
                && windows.iter().any(|w| w.used >= MEANINGFUL && (c.clears.is_empty() || c.clears.contains(&w.id)))
        }) {
            return Some(Decision { credit_id: c.id.clone(), reason: "salvage" });
        }
    }
    None
}

pub fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// Codex's reset endpoints live beside the chat transport: `…/backend-api/codex` → `…/backend-api`.
pub fn codex_api_base(base: &str) -> String {
    let base = base.trim_end_matches('/');
    base.strip_suffix("/codex").unwrap_or(base).to_string()
}

/// `GET {api}/wham/rate-limit-reset-credits`.
pub async fn codex_credits(http: &reqwest::Client, base: &str, access: &str, account: &str) -> Result<Vec<Credit>> {
    let v = codex_get(http, base, access, account, "rate-limit-reset-credits").await?;
    let list = v["credits"].as_array().or_else(|| v["data"].as_array()).cloned().unwrap_or_default();
    Ok(list
        .iter()
        .filter_map(|c| {
            Some(Credit {
                id: c["id"].as_str()?.to_string(),
                status: c["status"].as_str().unwrap_or("available").to_string(),
                expires_at: c["expires_at"].as_str().and_then(crate::usage::parse_time),
                clears: Vec::new(),
            })
        })
        .collect())
}

/// `GET {api}/wham/usage` → the chat windows.
pub async fn codex_windows(http: &reqwest::Client, base: &str, access: &str, account: &str) -> Result<Vec<Window>> {
    let v = codex_get(http, base, access, account, "usage").await?;
    let mut out = Vec::new();
    let limits = &v["rate_limit"];
    for (key, id) in [("primary_window", "5h"), ("secondary_window", "weekly")] {
        let w = &limits[key];
        let Some(used) = w["used_percent"].as_f64() else { continue };
        // A window longer than a day is the weekly window whatever slot it arrived in.
        let mut id = id;
        if w["limit_window_seconds"].as_u64().is_some_and(|s| s > 86400) {
            id = "weekly";
        }
        out.push(Window {
            id: id.into(),
            used: used / 100.,
            resets_at: w["reset_at"].as_u64().or_else(|| w["resets_at"].as_u64()),
        });
    }
    Ok(out)
}

async fn codex_get(http: &reqwest::Client, base: &str, access: &str, account: &str, path: &str) -> Result<Value> {
    let url = format!("{}/wham/{path}", codex_api_base(base));
    let mut req = http.get(url).bearer_auth(access).header("chatgpt-account-id", account);
    if account.is_empty() {
        req = http.get(format!("{}/wham/{path}", codex_api_base(base))).bearer_auth(access);
    }
    Ok(req.timeout(std::time::Duration::from_secs(10)).send().await?.error_for_status()?.json().await?)
}

/// `POST {api}/wham/rate-limit-reset-credits/consume`; true when the server applied it.
pub async fn codex_consume(http: &reqwest::Client, base: &str, access: &str, account: &str, credit: &str, request: &str) -> Result<bool> {
    let url = format!("{}/wham/rate-limit-reset-credits/consume", codex_api_base(base));
    let body = serde_json::json!({"credit_id": credit, "redeem_request_id": request, "account_id": account});
    let mut req = http.post(url).bearer_auth(access).json(&body);
    if !account.is_empty() {
        req = req.header("chatgpt-account-id", account);
    }
    let v: Value = req.timeout(std::time::Duration::from_secs(15)).send().await?.error_for_status()?.json().await?;
    Ok(v["code"].as_str() == Some("reset") || v["success"].as_bool() == Some(true))
}

/// Claude's OAuth API prefix: `https://api.anthropic.com` → `…/api/oauth`.
fn claude_oauth_base(base: &str) -> String {
    let trimmed = base.trim().trim_end_matches('/');
    let origin = reqwest::Url::parse(trimmed)
        .ok()
        .filter(|u| u.scheme() == "https" || u.scheme() == "http")
        .map(|u| u.origin().ascii_serialization())
        .unwrap_or_else(|| "https://api.anthropic.com".into());
    origin
}

fn claude_headers(req: reqwest::RequestBuilder, token: &str) -> reqwest::RequestBuilder {
    req.bearer_auth(token)
        .header("anthropic-beta", crate::provider::anthropic_oauth::BETAS)
        .header("accept", "application/json, text/plain, */*")
        .header("x-app", "cli")
}

/// `GET {origin}/api/oauth/usage?cedar_ember=1&skip_spend=1` → credits and grant windows.
pub async fn claude_credits(http: &reqwest::Client, base: &str, token: &str) -> Result<(Vec<Credit>, Vec<Window>)> {
    let url = format!("{}/api/oauth/usage?cedar_ember=1&skip_spend=1", claude_oauth_base(base));
    let v: Value = claude_headers(http.get(url), token)
        .timeout(std::time::Duration::from_secs(10))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let mut credits = Vec::new();
    for g in v["cedar_ember"]["grants"].as_array().into_iter().flatten() {
        let Some(id) = g["id"].as_str() else { continue };
        if !g["id"].as_str().is_some_and(|id| id.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')) {
            continue;
        }
        let remaining = g["resets_left"].as_u64().unwrap_or(0);
        credits.push(Credit {
            id: id.to_string(),
            status: if g["paused"].as_bool() == Some(true) {
                "paused".into()
            } else if remaining == 0 {
                "redeemed".into()
            } else if g["usable_now"].as_bool() == Some(true) {
                "available".into()
            } else {
                "unavailable".into()
            },
            expires_at: g["ends_at"].as_str().and_then(crate::usage::parse_time),
            clears: g["clears"].as_array().into_iter().flatten().filter_map(Value::as_str).map(String::from).collect(),
        });
    }
    let mut windows = Vec::new();
    for (key, id) in [("five_hour", "anthropic:5h"), ("seven_day", "anthropic:7d")] {
        let Some(used) = v[key]["utilization"].as_f64() else { continue };
        windows.push(Window {
            id: id.into(),
            used: (used / 100.).clamp(0., 1.),
            resets_at: v[key]["resets_at"].as_str().and_then(crate::usage::parse_time),
        });
    }
    Ok((credits, windows))
}

/// `POST {origin}/api/organizations/{org}/reset_rate_limits` for Cedar; Juniper posts a bare `{"program": …}`.
pub async fn claude_redeem(http: &reqwest::Client, base: &str, token: &str, org: &str, credit: &str, request: &str) -> Result<bool> {
    let origin = claude_oauth_base(base);
    let url = format!("{origin}/api/organizations/{org}/reset_rate_limits");
    let body = serde_json::json!({"program": "cedar_ember", "grant_id": credit, "request_id": request});
    let v: Value = claude_headers(http.post(url), token)
        .json(&body)
        .timeout(std::time::Duration::from_secs(20))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    Ok(v["success"].as_bool().unwrap_or(true))
}

/// The signed-in Claude organization for the reset endpoint.
pub async fn claude_org(http: &reqwest::Client, token: &str) -> Result<String> {
    let url = "https://api.anthropic.com/api/oauth/profile";
    let v: Value = claude_headers(http.get(url), token)
        .timeout(std::time::Duration::from_secs(5))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let org = v["organization"]["uuid"].as_str().or_else(|| v["organization_uuid"].as_str());
    org.filter(|o| !o.is_empty()).map(str::to_string).ok_or_else(|| anyhow!("no Claude organization in the profile reply"))
}

fn request_id() -> String {
    let mut bytes = [0u8; 16];
    let _ = getrandom::fill(&mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Applies the configured policy for a limited `provider/model` turn: redeems a saved reset and
/// reports whether the caller should retry the request once.
pub async fn redeem(cfg: &Config, spec: &str, blocked_wait: Option<u64>) -> Result<Option<String>> {
    let (provider, _) = spec.split_once('/').ok_or_else(|| anyhow!("model must be `provider/model`"))?;
    let p = cfg.providers.get(provider).ok_or_else(|| anyhow!("unknown provider `{provider}`"))?;
    let http = reqwest::Client::new();
    let at = now();
    match p.kind.as_str() {
        "codex" => {
            let policy = &cfg.services_extra_opts.codex_resets;
            let access = crate::provider::codex::access_token(&crate::provider::codex::path()).await?;
            let (account, _) = crate::provider::codex::account(&access);
            let account = account.unwrap_or_default();
            let credits = codex_credits(&http, &p.base_url, &access, &account).await?;
            let windows = codex_windows(&http, &p.base_url, &access, &account).await.unwrap_or_default();
            let Some(decision) = decide(policy, &credits, &windows, blocked_wait, at) else { return Ok(None) };
            let consumed = codex_consume(&http, &p.base_url, &access, &account, &decision.credit_id, &request_id()).await?;
            Ok(consumed.then(|| format!("spent a saved Codex reset ({})", decision.reason)))
        }
        "anthropic" => {
            let Some(path) = crate::provider::oauth(provider, p) else { return Ok(None) };
            let policy = &cfg.services_extra_opts.claude_resets;
            let token = crate::provider::anthropic_oauth::access_token(&path).await?;
            let (credits, windows) = claude_credits(&http, &p.base_url, &token).await?;
            let Some(decision) = decide(policy, &credits, &windows, blocked_wait, at) else { return Ok(None) };
            let org = claude_org(&http, &token).await.context("Claude reset needs the signed-in organization")?;
            let done = claude_redeem(&http, &p.base_url, &token, &org, &decision.credit_id, &request_id()).await?;
            Ok(done.then(|| format!("spent a saved Claude reset ({})", decision.reason)))
        }
        _ => Ok(None),
    }
}

/// Retries a limited request once after a successful redemption.
pub async fn with_redeem<T, F, Fut>(cfg: &Config, spec: &str, blocked_wait: Option<u64>, attempt: F) -> Result<T>
where
    F: Fn() -> Fut,
    Fut: Future<Output = Result<T>>,
{
    match attempt().await {
        Ok(v) => Ok(v),
        Err(e) => {
            // An account-risk reply pauses the provider until the user unpauses it.
            if let Some(risk) = crate::auth::account_risk(&format!("{e:#}")) {
                let provider = spec.split_once('/').map_or(spec, |(p, _)| p);
                let _ = crate::auth::pause(provider, risk);
            }
            let Some(limit) = e.downcast_ref::<crate::provider::limit::LimitError>() else { return Err(e) };
            let wait = limit.wait.or(blocked_wait);
            if redeem(cfg, spec, wait).await?.is_none() {
                return Err(e);
            }
            attempt().await
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::services_extra::ResetPolicy;

    fn credit(id: &str, expires: Option<u64>) -> Credit {
        Credit { id: id.into(), status: "available".into(), expires_at: expires, clears: vec![] }
    }

    fn window(id: &str, used: f64, resets: Option<u64>) -> Window {
        Window { id: id.into(), used, resets_at: resets }
    }

    #[test]
    fn spends_when_the_unblock_is_far_enough_away() {
        let p = ResetPolicy { min_blocked_minutes: 60, keep_credits: 0, salvage_horizon_hours: 12 };
        let now = 1_000_000;
        let credits = [credit("c1", None)];
        assert_eq!(decide(&p, &credits, &[], None, now), None, "no reset time at all");
        let far = [window("5h", 1.0, Some(now + 2 * 3600))];
        assert_eq!(decide(&p, &credits, &far, None, now).unwrap().reason, "blocked");
        let soon = [window("5h", 1.0, Some(now + 20 * 60))];
        assert_eq!(decide(&p, &credits, &soon, None, now), None);
        assert_eq!(decide(&p, &credits, &[], Some(2 * 3600), now).unwrap().reason, "blocked");
        let lax = ResetPolicy { min_blocked_minutes: 360, ..p.clone() };
        assert_eq!(decide(&lax, &credits, &far, None, now), None, "5h-only block ignored");
    }

    #[test]
    fn reserve_and_salvage() {
        let now = 1_000_000;
        let mut p = ResetPolicy { min_blocked_minutes: 60, keep_credits: 1, salvage_horizon_hours: 12 };
        let far = [window("5h", 0.1, Some(now + 2 * 3600))];
        let credits = [credit("a", None), credit("b", None)];
        assert_eq!(decide(&p, &credits, &far, None, now), None, "reserve keeps the last credit");
        // A credit expiring within 5 minutes beats the reserve.
        let credits = [credit("a", None), credit("b", Some(now + 120))];
        assert_eq!(decide(&p, &credits, &far, None, now).unwrap().credit_id, "b");
        // Salvage needs real usage in a covered window and a credit inside the horizon.
        let quiet = [window("5h", 0.05, Some(now + 2 * 3600))];
        let credits = [credit("a", Some(now + 3600)), credit("b", None)];
        p.keep_credits = 0;
        assert_eq!(decide(&p, &credits, &quiet, None, now), None);
        let busy = [window("5h", 0.6, Some(now + 2 * 3600))];
        assert_eq!(decide(&p, &credits, &busy, None, now).unwrap().reason, "salvage");
        p.salvage_horizon_hours = 0;
        assert_eq!(decide(&p, &credits, &busy, None, now), None, "0 disables salvage");
        let expired = [credit("old", Some(now - 1))];
        assert_eq!(decide(&p, &expired, &busy, None, now), None);
        let redeemed = [Credit { id: "x".into(), status: "redeemed".into(), expires_at: None, clears: vec![] }];
        assert_eq!(decide(&p, &redeemed, &busy, None, now), None);
    }

    #[tokio::test]
    async fn codex_flow_reads_credits_and_consumes_one() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let log = seen.clone();
        tokio::spawn(async move {
            for _ in 0..3 {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut buf = vec![0u8; 4096];
                let n = socket.read(&mut buf).await.unwrap();
                let text = String::from_utf8_lossy(&buf[..n]).into_owned();
                let path = text.split_whitespace().nth(1).unwrap_or("").to_string();
                log.lock().unwrap().push(text);
                let body = match path.as_str() {
                    "/backend-api/wham/rate-limit-reset-credits" => {
                        r#"{"credits":[{"id":"c1","status":"available","expires_at":"1970-01-02T00:00:00Z"}]}"#
                    }
                    "/backend-api/wham/usage" => {
                        r#"{"rate_limit":{"primary_window":{"used_percent":100.0,"reset_at":90000}}}"#
                    }
                    _ => r#"{"code":"reset"}"#,
                };
                let reply = format!("HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}", body.len());
                socket.write_all(reply.as_bytes()).await.unwrap();
            }
        });
        let base = format!("http://{addr}/backend-api/codex");
        assert_eq!(codex_api_base(&base), format!("http://{addr}/backend-api"));
        let http = reqwest::Client::new();
        let credits = codex_credits(&http, &base, "token", "acct").await.unwrap();
        assert_eq!(credits.len(), 1);
        assert_eq!(credits[0].id, "c1");
        assert_eq!(credits[0].expires_at, Some(86400));
        let windows = codex_windows(&http, &base, "token", "acct").await.unwrap();
        assert_eq!(windows[0].used, 1.0);
        assert!(codex_consume(&http, &base, "token", "acct", "c1", "req").await.unwrap());
        let log = seen.lock().unwrap();
        assert!(log[0].starts_with("GET /backend-api/wham/rate-limit-reset-credits "), "{}", log[0]);
        assert!(log[0].to_ascii_lowercase().contains("chatgpt-account-id: acct"), "{}", log[0]);
        assert!(log[2].starts_with("POST /backend-api/wham/rate-limit-reset-credits/consume "), "{}", log[2]);
        assert!(log[2].contains("\"credit_id\":\"c1\""), "{}", log[2]);
    }

    #[tokio::test]
    async fn claude_flow_reads_grants_and_redeems() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let log = seen.clone();
        tokio::spawn(async move {
            for i in 0..2 {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut buf = vec![0u8; 4096];
                let n = socket.read(&mut buf).await.unwrap();
                log.lock().unwrap().push(String::from_utf8_lossy(&buf[..n]).into_owned());
                let body = if i == 0 {
                    r#"{"five_hour":{"utilization":80.0,"resets_at":"1970-01-02T00:00:00Z"},
                        "cedar_ember":{"eligible":true,"grants":[{"id":"cedar-1","resets_left":1,"usable_now":true,"ends_at":"1970-01-05T00:00:00Z","clears":["five_hour"]}]}}"#
                } else {
                    r#"{"success":true}"#
                };
                let reply = format!("HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}", body.len());
                socket.write_all(reply.as_bytes()).await.unwrap();
            }
        });
        let base = format!("http://{addr}");
        let http = reqwest::Client::new();
        let (credits, windows) = claude_credits(&http, &base, "tok").await.unwrap();
        assert_eq!(credits.len(), 1);
        assert_eq!(credits[0].status, "available");
        assert_eq!(credits[0].expires_at, Some(4 * 86400));
        assert_eq!(credits[0].clears, ["five_hour"]);
        assert_eq!(windows[0].used, 0.8);
        assert!(claude_redeem(&http, &base, "tok", "org-1", "cedar-1", "req").await.unwrap());
        let log = seen.lock().unwrap();
        assert!(log[0].starts_with("GET /api/oauth/usage?cedar_ember=1&skip_spend=1 "), "{}", log[0]);
        assert!(log[0].contains("authorization: Bearer tok"), "{}", log[0]);
        assert!(log[1].starts_with("POST /api/organizations/org-1/reset_rate_limits "), "{}", log[1]);
        assert!(log[1].contains("\"grant_id\":\"cedar-1\""), "{}", log[1]);
    }

    #[test]
    fn oauth_bases_are_reduced_to_an_origin() {
        assert_eq!(claude_oauth_base("https://api.anthropic.com/v1"), "https://api.anthropic.com");
        assert_eq!(claude_oauth_base(""), "https://api.anthropic.com");
    }
}
