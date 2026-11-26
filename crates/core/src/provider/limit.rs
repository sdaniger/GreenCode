//! Rate-limit and usage-limit replies: when the limit resets, and a notice that says so.
use reqwest::header::HeaderMap;
use serde_json::Value;

/// A provider refused the request because a limit was reached. `reset_at` is unix seconds, when the reply says.
#[derive(Debug)]
pub struct LimitError {
    pub reset_at: Option<u64>,
    detail: String,
    /// Seconds until `reset_at`, measured when the reply arrived.
    pub wait: Option<u64>,
}

impl std::fmt::Display for LimitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "利用上限に達しました")?;
        if let Some(wait) = self.wait { write!(f, "（あと{}でリセット）", human(wait))?; }
        if !self.detail.is_empty() { write!(f, ": {}", self.detail)?; }
        Ok(())
    }
}

impl std::error::Error for LimitError {}

fn human(secs: u64) -> String {
    match secs {
        0..=59 => format!("{}秒", secs.max(1)),
        60..=3599 => format!("{}分", secs.div_ceil(60)),
        _ => match secs % 3600 / 60 { 0 => format!("{}時間", secs / 3600), m => format!("{}時間{m}分", secs / 3600) },
    }
}

/// `1h2m3.5s`, `6m0s`, `20ms` as seconds.
fn duration(s: &str) -> Option<f64> {
    let (mut total, mut num, mut seen) = (0., String::new(), false);
    let mut chars = s.trim().chars().peekable();
    while let Some(c) = chars.next() {
        if c.is_ascii_digit() || c == '.' { num.push(c); continue; }
        let unit = match (c, chars.peek()) {
            ('m', Some('s')) => { chars.next(); 0.001 }
            ('h', _) => 3600.,
            ('m', _) => 60.,
            ('s', _) => 1.,
            _ => return None,
        };
        total += num.parse::<f64>().ok()? * unit;
        (num, seen) = (String::new(), true);
    }
    (seen && num.is_empty()).then_some(total)
}

/// Unix time for a reset value: epoch seconds or milliseconds, seconds from now, an RFC 3339 time or a duration.
fn when(s: &str, now: u64) -> Option<u64> {
    let s = s.trim();
    if let Ok(n) = s.parse::<u64>() {
        return Some(match n { 0..=999_999_999 => now + n, 1_000_000_000..=99_999_999_999 => n, _ => n / 1000 });
    }
    crate::usage::parse_time(s).or_else(|| duration(s).map(|d| now + d.ceil() as u64))
}

fn body_reset(v: &Value, err: &Value, msg: &str, now: u64) -> Option<u64> {
    for node in [err, v] {
        for key in ["resets_in_seconds", "retry_after", "retry_after_seconds", "resets_at"] {
            let field = &node[key];
            let parsed = match (field.as_u64(), field.as_str()) {
                (Some(n), _) => when(&n.to_string(), now),
                (_, Some(s)) => when(s, now),
                _ => None,
            };
            if parsed.is_some() { return parsed; }
        }
    }
    // Claude subscription limits: "Claude AI usage limit reached|1763900000".
    if let Some(at) = msg.split_once("limit reached|").and_then(|(_, t)| t.split(|c: char| !c.is_ascii_digit()).next()) {
        if let Some(at) = when(at, now) { return Some(at); }
    }
    // OpenAI: "Please try again in 6m0s."
    let tail = msg.split_once("try again in ")?.1;
    let word = tail.split_whitespace().next()?.trim_end_matches(['.', ',']);
    when(word, now).filter(|_| word.chars().any(|c| c.is_ascii_alphabetic()))
}

/// The limit a failed reply reports, if it reports one.
pub fn parse(status: u16, headers: &HeaderMap, body: &str, now: u64) -> Option<LimitError> {
    let v: Value = serde_json::from_str(body).unwrap_or(Value::Null);
    let err = if v["error"].is_object() { &v["error"] } else { &v };
    let kind = err["type"].as_str().or(err["code"].as_str()).unwrap_or("");
    let msg = err["message"].as_str().or(v["detail"].as_str()).unwrap_or(body.trim());
    let text = format!("{kind} {msg}").to_lowercase();
    let limited = status == 429 || ["rate_limit", "usage_limit", "insufficient_quota", "limit reached"].iter().any(|k| text.contains(k));
    if !limited { return None; }
    let reset_at = headers.get("retry-after").and_then(|h| h.to_str().ok()?.trim().parse::<u64>().ok()).map(|s| now + s)
        .or_else(|| body_reset(&v, err, msg, now))
        // The latest `*-reset` header: a request or token window that is not exhausted resets sooner and does not matter.
        .or_else(|| headers.iter().filter(|(k, _)| k.as_str().contains("reset")).filter_map(|(_, v)| when(v.to_str().ok()?, now)).filter(|at| *at > now).max());
    let detail: String = msg.chars().take(300).collect();
    Some(LimitError { reset_at, detail, wait: reset_at.map(|at| at.saturating_sub(now)) })
}

/// The failure of a non-success reply: a `LimitError` for limits, otherwise `HTTP <status>: <body>`.
pub fn http_error(status: reqwest::StatusCode, headers: &HeaderMap, body: &str) -> anyhow::Error {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
    match parse(status.as_u16(), headers, body, now) {
        Some(limit) => limit.into(),
        None => anyhow::anyhow!("HTTP {status}: {body}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        pairs.iter().map(|(k, v)| (k.parse().unwrap(), v.parse().unwrap())).collect()
    }

    #[test]
    fn retry_after_wins() {
        let h = headers(&[("retry-after", "90"), ("anthropic-ratelimit-tokens-reset", "2026-01-01T00:00:00Z")]);
        let e = parse(429, &h, r#"{"error":{"type":"rate_limit_error","message":"slow down"}}"#, 1000).unwrap();
        assert_eq!(e.reset_at, Some(1090));
        assert_eq!(e.to_string(), "利用上限に達しました（あと2分でリセット）: slow down");
    }

    #[test]
    fn reset_headers_take_the_latest_future_time() {
        let h = headers(&[("anthropic-ratelimit-requests-reset", "1970-01-01T00:00:10Z"), ("anthropic-ratelimit-tokens-reset", "1970-01-01T00:10:00Z"), ("x-ratelimit-reset-requests", "5s")]);
        assert_eq!(parse(429, &h, "", 100).unwrap().reset_at, Some(600));
    }

    #[test]
    fn body_fields_and_messages() {
        let codex = r#"{"error":{"type":"usage_limit_reached","message":"The usage limit has been reached","resets_in_seconds":7200}}"#;
        assert_eq!(parse(429, &HeaderMap::new(), codex, 0).unwrap().reset_at, Some(7200));
        let epoch = r#"{"error":{"message":"Claude AI usage limit reached|1763900000"}}"#;
        assert_eq!(parse(429, &HeaderMap::new(), epoch, 5).unwrap().reset_at, Some(1_763_900_000));
        let openai = r#"{"error":{"code":"rate_limit_exceeded","message":"Rate limit reached. Please try again in 1m30s."}}"#;
        assert_eq!(parse(429, &HeaderMap::new(), openai, 100).unwrap().reset_at, Some(190));
        let unknown = parse(429, &HeaderMap::new(), "busy", 0).unwrap();
        assert_eq!((unknown.reset_at, unknown.to_string().as_str()), (None, "利用上限に達しました: busy"));
    }

    #[test]
    fn other_failures_are_not_limits() {
        assert!(parse(500, &HeaderMap::new(), r#"{"error":{"message":"boom"}}"#, 0).is_none());
        assert_eq!(human(3 * 3600 + 5 * 60), "3時間5分");
    }
}
