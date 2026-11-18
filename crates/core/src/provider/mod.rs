pub mod anthropic;
pub mod anthropic_oauth;
pub mod codex;
pub mod copilot;
pub mod mistral;
pub mod openai;

use crate::{config::Config, Block, BoxFut, Message, ToolDef, Usage};
use anyhow::{anyhow, bail, Result};
use std::{sync::Arc, time::Duration};

pub struct Request<'a> {
    pub model: &'a str,
    pub system: &'a str,
    pub messages: &'a [Message],
    pub tools: &'a [ToolDef],
    pub max_tokens: u32,
}

#[derive(Debug, Default)]
pub struct Reply {
    pub blocks: Vec<Block>,
    pub usage: Usage,
    /// Anthropic vocabulary: `end_turn`, `tool_use`, `max_tokens`, `refusal`.
    pub stop: String,
}

pub trait Provider: Send + Sync {
    fn complete<'a>(
        &'a self,
        req: Request<'a>,
        on_text: &'a (dyn Fn(&str) + Send + Sync),
    ) -> BoxFut<'a, Result<Reply>>;
}

/// Effort values actually sent for this configured provider and model.
pub fn effort_levels(cfg: &Config, spec: &str) -> Vec<&'static str> {
    let Some((name, model)) = spec.split_once('/') else { return Vec::new() };
    let Some(provider) = cfg.providers.get(name) else { return Vec::new() };
    let id = model.rsplit('/').next().unwrap_or(model).to_lowercase();
    if provider.kind == "codex" {
        if let Some(cached) = codex::efforts(&id).filter(|levels| !levels.is_empty()) {
            let known = ["low", "medium", "high", "xhigh", "max", "ultra"];
            return known.into_iter().filter(|level| cached.iter().any(|value| value.eq_ignore_ascii_case(level))).collect();
        }
        let mut levels = vec!["low", "medium", "high"];
        if id.starts_with("gpt-6") || id.starts_with("gpt-5.5") || id.starts_with("gpt-5.6") { levels.push("xhigh"); }
        return levels;
    }
    let anthropic = provider.kind == "anthropic";
    let openai = provider.kind == "openai"
        && reqwest::Url::parse(&provider.base_url).is_ok_and(|url| url.host_str() == Some("api.openai.com"));
    if !anthropic && !openai { return Vec::new(); }
    let max = anthropic && (id.contains("opus-5") || id.contains("fable"));
    let mut levels = vec!["low", "medium", "high"];
    if max || (anthropic && id.contains("sonnet-5"))
        || (openai && (id.starts_with("gpt-6") || id.starts_with("gpt-5.5") || id.starts_with("gpt-5.6"))) {
        levels.push("xhigh");
    }
    if max { levels.push("max"); }
    levels
}

/// Configured effort, only when supported by this provider and model.
pub fn model_effort(cfg: &Config, spec: &str) -> Option<String> {
    cfg.effort.as_ref().filter(|effort| effort_levels(cfg, spec).contains(&effort.as_str())).cloned()
}

/// Resolves `provider/model` to a client and the bare model name.
pub fn make(cfg: &Config, spec: &str) -> Result<(Arc<dyn Provider>, String)> {
    let (pname, model) = spec.split_once('/').ok_or_else(|| anyhow!("model must be `provider/model`, got `{spec}`"))?;
    if let Some(policy) = &cfg.policy {
        anyhow::ensure!(policy.provider_allowed(pname), "管理ポリシーで制限されています（{}）", policy.organization);
    }
    let p = cfg.providers.get(pname).ok_or_else(|| anyhow!("unknown provider `{pname}`"))?;
    let effort = model_effort(cfg, spec);
    if p.kind == "codex" {
        let path = codex::path();
        if !path.exists() { bail!("{pname} にサインインしていません。/login {pname} を実行してください") }
        return Ok((Arc::new(codex::Codex::new(p.base_url.trim_end_matches('/').to_string(), path, effort)), model.to_string()));
    }
    let oauth = oauth(pname, p);
    let key = if oauth.is_some() { String::new() } else { key(p)? };
    let http = reqwest::Client::new();
    let base = p.base_url.trim_end_matches('/').to_string();
    let provider: Arc<dyn Provider> = match p.kind.as_str() {
        "anthropic" => Arc::new(anthropic::Anthropic { http, base, key, oauth, effort }),
        "openai" => Arc::new(openai::OpenAi { http, copilot: base == copilot::BASE, base, key, effort }),
        k => bail!("provider `{pname}`: unknown kind `{k}`"),
    };
    Ok((provider, model.to_string()))
}

pub(crate) fn key(p: &crate::config::ProviderCfg) -> Result<String> {
    match &p.api_key {
        Some(key) => Ok(key.clone()),
        None if p.api_key_env.is_empty() => Ok(String::new()),
        None => std::env::var(&p.api_key_env).map_err(|_| anyhow!("{} が未設定です。/login でサインインするか、APIキーを環境変数に設定してください", p.api_key_env)),
    }
}

/// Credentials file when this provider signs in with a Claude subscription instead of an API key.
pub fn oauth(pname: &str, p: &crate::config::ProviderCfg) -> Option<std::path::PathBuf> {
    let path = anthropic_oauth::path();
    (pname == "anthropic" && p.kind == "anthropic" && p.api_key.is_none() && path.exists()).then_some(path)
}

/// Model ids from the provider's own `/models` endpoint, as `provider/model`.
pub async fn list_models(cfg: &Config, pname: &str) -> Result<Vec<String>> {
    if let Some(policy) = &cfg.policy {
        anyhow::ensure!(policy.provider_allowed(pname), "管理ポリシーで制限されています（{}）", policy.organization);
    }
    let p = cfg.providers.get(pname).ok_or_else(|| anyhow!("unknown provider `{pname}`"))?;
    if p.kind == "codex" {
        if !codex::path().exists() { return Ok(vec![]); }
        let cached = codex::models();
        if !cached.is_empty() { return Ok(cached.into_iter().map(|model| format!("{pname}/{}", model.id)).collect()); }
        return Ok(["gpt-5.5", "gpt-5.5-codex", "gpt-6-luna", "gpt-6-sol"].iter().map(|id| format!("{pname}/{id}")).collect());
    }
    let (base, http) = (p.base_url.trim_end_matches('/'), reqwest::Client::new());
    let req = match (p.kind.as_str(), oauth(pname, p)) {
        ("anthropic", Some(path)) => http.get(format!("{base}/v1/models?limit=1000"))
            .bearer_auth(anthropic_oauth::access_token(&path).await?)
            .header("anthropic-beta", anthropic_oauth::BETAS)
            .header(reqwest::header::USER_AGENT, anthropic_oauth::user_agent().await)
            .header("x-app", "cli")
            .header("anthropic-version", "2026-10-21"),
        ("anthropic", None) => http.get(format!("{base}/v1/models?limit=1000")).header("x-api-key", key(p)?).header("anthropic-version", "2026-10-21"),
        _ => {
            let req = http.get(format!("{base}/models")).bearer_auth(key(p)?);
            if base == copilot::BASE { copilot::discovery_headers().into_iter().fold(req, |r, (k, v)| r.header(k, v)) } else { req }
        }
    };
    let v: serde_json::Value = req.timeout(Duration::from_secs(10)).send().await?.error_for_status()?.json().await?;
    // `{"data": [...]}` is the standard shape; a few servers return the array itself.
    let list = v["data"].as_array().or(v.as_array()).ok_or_else(|| anyhow!("{pname}: unexpected /models reply"))?;
    Ok(list.iter().filter_map(|m| m["id"].as_str()).map(|id| format!("{pname}/{id}")).collect())
}

/// Sends with up to two retries on 429/5xx/connection errors.
pub(crate) async fn send(build: impl Fn() -> reqwest::RequestBuilder) -> Result<reqwest::Response> {
    let mut attempt = 0;
    loop {
        attempt += 1;
        let err = match build().send().await {
            Ok(r) if r.status().is_success() => return Ok(r),
            Ok(r) => {
                let status = r.status();
                let retry = status.as_u16() == 429 || status.is_server_error();
                let body = r.text().await.unwrap_or_default();
                let e = anyhow!("HTTP {status}: {body}");
                if !retry {
                    return Err(e);
                }
                e
            }
            Err(e) => e.into(),
        };
        if attempt == 3 {
            return Err(err);
        }
        tokio::time::sleep(Duration::from_secs(1 << attempt)).await;
    }
}

/// Feeds each SSE `data:` payload to `on`; `on` returns true to stop early.
pub(crate) async fn sse(mut resp: reqwest::Response, mut on: impl FnMut(&str) -> Result<bool>) -> Result<()> {
    let mut buf: Vec<u8> = Vec::new();
    while let Some(chunk) = resp.chunk().await? {
        buf.extend_from_slice(&chunk);
        while let Some(i) = buf.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = buf.drain(..=i).collect();
            let line = String::from_utf8_lossy(&line);
            if let Some(d) = line.trim_end().strip_prefix("data:") {
                if on(d.trim_start())? {
                    return Ok(());
                }
            }
        }
    }
    Ok(())
}
