//! GitHub Copilot: device-flow sign-in and the request identity its API expects.

use crate::{Block, Message};
use anyhow::{bail, Result};
use serde_json::Value;
use std::time::Duration;

pub const BASE: &str = "https://api.githubcopilot.com";
const ORIGIN: &str = "https://github.com";
const CLIENT_ID: &str = "Ov23li8tweQw6odWQebz";
const VERSION: &str = "copilot/1.0.82";

#[derive(Debug)]
pub struct Device {
    pub user_code: String,
    pub verification_uri: String,
    device_code: String,
    interval: u64,
    expires_in: u64,
}

pub async fn start() -> Result<Device> { start_at(ORIGIN).await }

pub async fn start_at(origin: &str) -> Result<Device> {
    let v = post(origin, "/login/device/code", &[("client_id", CLIENT_ID), ("scope", "read:user")]).await?;
    let text = |k: &str| v[k].as_str().map(str::to_string);
    let (Some(device_code), Some(user_code), Some(verification_uri), Some(interval), Some(expires_in)) =
        (text("device_code"), text("user_code"), text("verification_uri"), v["interval"].as_u64(), v["expires_in"].as_u64())
    else { bail!("invalid device code response") };
    Ok(Device { user_code, verification_uri, device_code, interval, expires_in })
}

pub async fn finish(device: Device) -> Result<String> { finish_at(ORIGIN, device, Duration::from_secs(1)).await }

/// `unit` is the length of one "second" of waiting, so tests can run fast.
pub async fn finish_at(origin: &str, device: Device, unit: Duration) -> Result<String> {
    let mut interval = device.interval.max(1) as f64;
    let mut waited = 0.0;
    while waited < device.expires_in as f64 {
        let wait = interval * 1.2;
        tokio::time::sleep(unit.mul_f64(wait)).await;
        waited += wait;
        let v = post(origin, "/login/oauth/access_token", &[
            ("client_id", CLIENT_ID),
            ("device_code", &device.device_code),
            ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
        ]).await?;
        if let Some(token) = v["access_token"].as_str() { return Ok(token.to_string()); }
        match v["error"].as_str() {
            Some("authorization_pending") => {}
            Some("slow_down") => interval = v["interval"].as_u64().map_or(interval + 5.0, |i| i as f64),
            Some(e) => bail!("Device flow failed: {e}: {}", v["error_description"].as_str().unwrap_or("")),
            None => bail!("invalid device flow response"),
        }
    }
    bail!("Device flow timed out")
}

async fn post(origin: &str, path: &str, form: &[(&str, &str)]) -> Result<Value> {
    let body = url::form_urlencoded::Serializer::new(String::new()).extend_pairs(form).finish();
    let resp = reqwest::Client::new().post(format!("{origin}{path}"))
        .header("Accept", "application/json")
        .header("User-Agent", "copilot-developer-action/0.0.1")
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(body)
        .send().await?;
    let status = resp.status();
    let text = resp.text().await?;
    if !status.is_success() { bail!("HTTP {status}: {text}") }
    Ok(serde_json::from_str(&text)?)
}

fn fixed(initiator: &str) -> Vec<(&'static str, String)> {
    let kind = if initiator == "agent" { "conversation-agent" } else { "conversation-user" };
    vec![
        ("User-Agent", VERSION.into()),
        ("Editor-Version", VERSION.into()),
        ("Copilot-Integration-Id", "copilot-chat".into()),
        ("Copilot-Harness-Id", "copilot-sdk".into()),
        ("Openai-Intent", "conversation-agent".into()),
        ("X-GitHub-Api-Version", "2026-08-01".into()),
        ("X-Initiator", initiator.into()),
        ("X-Interaction-Type", kind.into()),
    ]
}

/// Identity headers for one request.
pub fn headers(messages: &[Message]) -> Vec<(&'static str, String)> {
    let agent = messages.last().is_some_and(|m| m.content.iter().any(|b| matches!(b, Block::ToolResult { .. })));
    let mut h = fixed(if agent { "agent" } else { "user" });
    if messages.iter().flat_map(|m| &m.content).any(|b| matches!(b, Block::Other(v) if v["type"] == "image")) {
        h.push(("Copilot-Vision-Request", "true".into()));
    }
    h
}

/// Identity headers for `/models`.
pub fn discovery_headers() -> Vec<(&'static str, String)> { fixed("user") }
