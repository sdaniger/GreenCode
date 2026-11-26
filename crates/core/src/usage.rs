//! Subscription limits for signed-in providers (Claude Pro/Max, GitHub Copilot).

use crate::{auth::{self, Method}, config::Config, provider};
use anyhow::{Context, Result};
use serde_json::Value;

#[derive(Debug, Clone, PartialEq)]
pub struct Limit {
    pub label: String,
    /// 0–100 used.
    pub used: f64,
    /// Unix seconds; None when unknown.
    pub resets_at: Option<u64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Report {
    pub provider: String,
    pub plan: Option<String>,
    pub limits: Vec<Limit>,
}

/// Days since 1970-01-01 of a proleptic Gregorian date.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

/// `YYYY-MM-DD` or `YYYY-MM-DDTHH:MM:SS[.fff](Z|±HH:MM)` as unix seconds; fractions are ignored.
pub(crate) fn parse_time(s: &str) -> Option<u64> {
    let s = s.trim();
    let num = |r: std::ops::Range<usize>| s.get(r)?.parse::<i64>().ok();
    let days = days_from_civil(num(0..4)?, num(5..7)?, num(8..10)?);
    let mut secs = days * 86400;
    if s.len() > 10 {
        if !matches!(s.as_bytes()[10], b'T' | b' ') { return None }
        secs += num(11..13)? * 3600 + num(14..16)? * 60 + num(17..19).unwrap_or(0);
        let rest = s.get(19..)?;
        let rest = match rest.strip_prefix('.') { Some(r) => r.trim_start_matches(|c: char| c.is_ascii_digit()), None => rest };
        if let Some(sign) = rest.chars().next().filter(|c| matches!(c, '+' | '-')) {
            let off = &rest[1..];
            let (h, m) = (off.get(0..2)?.parse::<i64>().ok()?, off.get(3..5).or(off.get(2..4)).and_then(|m| m.parse::<i64>().ok()).unwrap_or(0));
            let off = h * 3600 + m * 60;
            secs -= if sign == '+' { off } else { -off };
        }
    }
    u64::try_from(secs).ok()
}

pub fn parse_claude(v: &Value) -> Vec<Limit> {
    let mut out = vec![];
    for (key, label) in [("five_hour", "5時間"), ("seven_day", "7日間"), ("seven_day_opus", "7日間 (Opus)"), ("seven_day_sonnet", "7日間 (Sonnet)")] {
        if let Some(used) = v[key]["utilization"].as_f64() {
            out.push(Limit { label: label.into(), used, resets_at: v[key]["resets_at"].as_str().and_then(parse_time) });
        }
    }
    for e in v["limits"].as_array().into_iter().flatten() {
        let (Some(label), Some(used)) = (e["label"].as_str().or(e["name"].as_str()), e["percent"].as_f64()) else { continue };
        out.push(Limit { label: label.into(), used, resets_at: e["resets_at"].as_str().and_then(parse_time) });
    }
    out
}

pub fn parse_copilot(v: &Value) -> Report {
    let resets_at = v["quota_reset_date"].as_str().and_then(parse_time);
    let limits = [("premium_interactions", "プレミアムリクエスト"), ("chat", "チャット"), ("completions", "補完")].into_iter().filter_map(|(key, label)| {
        let q = &v["quota_snapshots"][key];
        if q["unlimited"] == true { return None }
        Some(Limit { label: label.into(), used: 100. - q["percent_remaining"].as_f64()?, resets_at })
    }).collect();
    Report { provider: "GitHub Copilot".into(), plan: v["copilot_plan"].as_str().map(String::from), limits }
}

async fn get_json(req: reqwest::RequestBuilder) -> Result<Value> {
    let resp = req.send().await?;
    let status = resp.status();
    if !status.is_success() { anyhow::bail!("HTTP {status}") }
    resp.json().await.context("invalid response")
}

/// `GET {base}/api/oauth/usage`.
pub async fn claude(base: &str, token: &str) -> Result<Report> {
    let req = reqwest::Client::new().get(format!("{}/api/oauth/usage", base.trim_end_matches('/')))
        .header("authorization", format!("Bearer {token}"))
        .header("anthropic-beta", "oauth-2025-04-20")
        .header("user-agent", provider::anthropic_oauth::user_agent().await)
        .header("accept", "application/json");
    let v = get_json(req).await?;
    Ok(Report { provider: "Claude".into(), plan: None, limits: parse_claude(&v) })
}

/// `GET {base}/copilot_internal/user`.
pub async fn copilot(base: &str, token: &str) -> Result<Report> {
    let req = reqwest::Client::new().get(format!("{}/copilot_internal/user", base.trim_end_matches('/')))
        .header("Authorization", format!("token {token}"))
        .header("Accept", "application/json")
        .header("User-Agent", "copilot/1.0.82");
    Ok(parse_copilot(&get_json(req).await?))
}

/// Reports for every signed-in subscription in `cfg`; providers that fail are returned as errors in the Vec.
pub async fn all(cfg: &Config) -> Vec<Result<Report, String>> {
    let mut out = vec![];
    if let Some(p) = cfg.providers.get("anthropic") {
        if let Some(path) = provider::oauth("anthropic", p) {
            out.push(async { claude("https://api.anthropic.com", &provider::anthropic_oauth::access_token(&path).await?).await }.await
                .map_err(|e| format!("Claude: {e:#}")));
        }
    }
    if let Some(p) = cfg.providers.get("github-copilot") {
        if let (Method::Subscription, Some(token)) = (auth::status("github-copilot", p), &p.api_key) {
            out.push(match crate::secrets::resolve(token, "github-copilot") {
                Ok(token) => copilot("https://api.github.com", &token).await,
                Err(e) => Err(e),
            }.map_err(|e| format!("GitHub Copilot: {e:#}")));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn iso_times_parse_to_unix_seconds() {
        assert_eq!(parse_time("1970-01-02T00:00:00Z"), Some(86400));
        assert_eq!(parse_time("2026-11-13T05:00:00.5+09:00"), parse_time("2026-11-12T20:00:00Z"));
        assert_eq!(parse_time("1970-01-02"), Some(86400));
    }

    #[test]
    fn claude_buckets_and_limits_parse() {
        let v = json!({
            "five_hour": {"utilization": 42.5, "resets_at": "1970-01-02T00:00:00Z"},
            "seven_day": null,
            "limits": [{"name": "Extra", "percent": 7.0, "resets_at": null}],
        });
        assert_eq!(parse_claude(&v), vec![
            Limit { label: "5時間".into(), used: 42.5, resets_at: Some(86400) },
            Limit { label: "Extra".into(), used: 7., resets_at: None },
        ]);
    }

    #[test]
    fn copilot_skips_unlimited_snapshots() {
        let v = json!({
            "copilot_plan": "individual",
            "quota_reset_date": "1970-01-02",
            "quota_snapshots": {
                "premium_interactions": {"percent_remaining": 25.0, "unlimited": false},
                "chat": {"unlimited": true, "percent_remaining": 100.0},
            },
        });
        let r = parse_copilot(&v);
        assert_eq!(r.plan.as_deref(), Some("individual"));
        assert_eq!(r.limits, vec![Limit { label: "プレミアムリクエスト".into(), used: 75., resets_at: Some(86400) }]);
    }
}
