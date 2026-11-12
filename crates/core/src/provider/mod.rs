pub mod anthropic;
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

/// Resolves `provider/model` to a client and the bare model name.
pub fn make(cfg: &Config, spec: &str) -> Result<(Arc<dyn Provider>, String)> {
    let (pname, model) = spec.split_once('/').ok_or_else(|| anyhow!("model must be `provider/model`, got `{spec}`"))?;
    let p = cfg.providers.get(pname).ok_or_else(|| anyhow!("unknown provider `{pname}`"))?;
    let key = std::env::var(&p.api_key_env).map_err(|_| anyhow!("environment variable {} is not set", p.api_key_env))?;
    let http = reqwest::Client::new();
    let base = p.base_url.trim_end_matches('/').to_string();
    let provider: Arc<dyn Provider> = match p.kind.as_str() {
        "anthropic" => Arc::new(anthropic::Anthropic { http, base, key, effort: cfg.effort.clone() }),
        "openai" => Arc::new(openai::OpenAi { http, base, key }),
        k => bail!("provider `{pname}`: unknown kind `{k}`"),
    };
    Ok((provider, model.to_string()))
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
