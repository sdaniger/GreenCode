pub mod fallback;
pub mod limit;
pub mod anthropic;
pub mod anthropic_oauth;
pub mod codex;
pub mod compact;
pub mod copilot;
pub mod mistral;
pub mod openai;
pub mod responses;
pub mod thinking;
pub mod warm;
pub mod ws;

use crate::{config::Config, Block, BoxFut, Message, ToolDef, Usage};
use anyhow::{anyhow, bail, Result};
use std::{sync::Arc, time::Duration};

pub struct Request<'a> {
    pub model: &'a str,
    pub system: &'a str,
    pub messages: &'a [Message],
    pub tools: &'a [ToolDef],
    pub max_tokens: u32,
    /// Messages typed while this response streams (Codex WebSocket live steering).
    pub live: Option<&'a LiveSteer>,
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

    /// True when `complete` already consumed a full transcript and can continue indefinitely
    /// (`provider.appendOnlyContext`); the agent then only appends and never rewrites history.
    fn append_only(&self) -> bool {
        false
    }

    /// True when this provider can carry a message typed mid-response over its live connection.
    fn live_steering(&self, _model: &str) -> bool {
        false
    }
}

/// Messages typed while a response streams, handed to the transport that can carry them mid-flight.
#[derive(Clone, Debug, Default)]
pub struct LiveSteer {
    queue: Arc<std::sync::Mutex<Vec<String>>>,
    /// What a transport actually sent, so the agent can record the same instructions in the transcript.
    sent: Arc<std::sync::Mutex<Vec<String>>>,
}

impl LiveSteer {
    pub fn push(&self, text: String) {
        self.queue.lock().unwrap().push(text);
    }

    /// Takes every queued message, oldest first, remembering it was handed to the transport.
    pub fn take(&self) -> Vec<String> {
        let taken = std::mem::take(&mut *self.queue.lock().unwrap());
        self.sent.lock().unwrap().extend(taken.iter().cloned());
        taken
    }

    pub fn is_empty(&self) -> bool {
        self.queue.lock().unwrap().is_empty()
    }

    /// Instructions the transport carried into the response, cleared so they are recorded once.
    pub fn take_sent(&self) -> Vec<String> {
        std::mem::take(&mut *self.sent.lock().unwrap())
    }
}

/// Anthropic's `cache_control` value for the configured retention: TTL on long, `null` when caching is off.
pub fn anthropic_breakpoint(wire: &Wire) -> serde_json::Value {
    if !wire.cache_retention.caching() {
        return serde_json::Value::Null;
    }
    match wire.cache_retention.anthropic_ttl() {
        Some(ttl) => serde_json::json!({"type": "ephemeral", "ttl": ttl}),
        None => serde_json::json!({"type": "ephemeral"}),
    }
}

/// The parts of `[provider_opts]` one client needs, resolved once when the client is built.
#[derive(Clone, Debug)]
pub struct Wire {
    pub retry: crate::config::provider::Retry,
    pub timeouts: StreamTimeouts,
    pub sampling: crate::config::provider::Sampling,
    pub verbosity: crate::config::provider::TextVerbosity,
    pub thinking: crate::config::provider::ThinkingBudgets,
    pub fireworks_tier: crate::config::provider::FireworksTier,
    pub server_side_fallback: bool,
    /// Prompt-cache retention the provider should use for this model.
    pub cache_retention: crate::config::provider::CacheRetention,
    /// Keep a byte-stable prompt prefix (system + tools + append-only log).
    pub append_only: bool,
    /// WebSocket policy for the Responses transports.
    pub websockets: crate::config::provider::Websockets,
    /// Omit thinking summaries upstream (`omitThinking`).
    pub omit_thinking: bool,
    /// Provider reasoning off; a `think` tool carries the scratchpad instead.
    pub external_thinking: bool,
    /// Processing tier resolved for this model's family, already endpoint-checked.
    pub service_tier: Option<crate::config::provider::ServiceTier>,
    /// `extendedContext`: ask for the provider's larger context window where it has one.
    pub extended_context: bool,
}

impl Default for Wire {
    fn default() -> Self {
        let opts = crate::config::provider::ProviderOpts::default();
        Self::from(&opts, false)
    }
}

impl Wire {
    /// `fireworks` is true when this client talks to the Fireworks provider.
    pub fn from(opts: &crate::config::provider::ProviderOpts, fireworks: bool) -> Self {
        let fireworks_tier = if fireworks { opts.fireworks_tier } else { Default::default() };
        Self {
            retry: opts.retry.clone(),
            timeouts: StreamTimeouts::new(opts.stream_first_event_timeout_seconds, opts.stream_idle_timeout_seconds),
            sampling: opts.sampling.clone(),
            verbosity: opts.text_verbosity,
            thinking: opts.thinking,
            fireworks_tier,
            server_side_fallback: opts.anthropic.server_side_fallback,
            extended_context: false,
            cache_retention: opts.cache_retention,
            append_only: false,
            websockets: opts.openai_websockets,
            omit_thinking: opts.omit_thinking,
            external_thinking: opts.external_thinking,
            service_tier: None,
        }
    }
}

/// Stream watchdogs: the wait for the first chunk, then the longest allowed silence between chunks.
#[derive(Clone, Copy, Debug)]
pub struct StreamTimeouts {
    pub first: Option<Duration>,
    pub idle: Option<Duration>,
}

impl Default for StreamTimeouts {
    fn default() -> Self {
        Self::new(-1, -1)
    }
}

impl StreamTimeouts {
    /// -1 keeps OMP's 900s default, 0 disables that watchdog.
    pub fn new(first_seconds: i64, idle_seconds: i64) -> Self {
        let pick = |seconds: i64| match seconds {
            0 => None,
            n if n < 0 => Some(Duration::from_secs(900)),
            n => Some(Duration::from_secs(n as u64)),
        };
        Self { first: pick(first_seconds), idle: pick(idle_seconds) }
    }
}

/// `tier.*` resolved for this model, filtered to what its endpoint actually accepts.
pub fn tier_for(
    tier: &crate::config::provider::TierCfg,
    provider: &str,
    kind: &str,
    base_url: &str,
    model: &str,
) -> Option<crate::config::provider::ServiceTier> {
    use crate::config::provider::{tier_family, ServiceTier, TierFamily};
    let family = tier_family(provider, kind, base_url, model)?;
    let host = reqwest::Url::parse(base_url).ok().and_then(|u| u.host_str().map(str::to_ascii_lowercase));
    let first_party = match family {
        TierFamily::OpenAi => host.as_deref() == Some("api.openai.com") || kind == "codex",
        TierFamily::Anthropic => host.as_deref() == Some("api.anthropic.com"),
        TierFamily::Google => host.as_deref().is_some_and(|h| h.ends_with("googleapis.com")),
    };
    let openrouter = host.as_deref().is_some_and(|h| h.ends_with("openrouter.ai"));
    let chosen = tier.family(family);
    chosen.wire()?;
    match family {
        // First-party and OpenRouter take the field; other hosts pick their own path.
        TierFamily::OpenAi if first_party || openrouter => {
            (first_party || chosen != ServiceTier::Ultrafast).then_some(chosen)
        }
        TierFamily::OpenAi => None,
        // `priority` is fast mode, first-party only; Bedrock/Vertex/OpenRouter ignore it.
        TierFamily::Anthropic => (first_party && chosen == ServiceTier::Priority).then_some(chosen),
        TierFamily::Google => matches!(chosen, ServiceTier::Flex | ServiceTier::Priority).then_some(chosen),
    }
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

/// `providers.openaiWebsockets` is the master switch for either socket lane; Codex always has a
/// socket, while a standard Responses model needs `providers.openaiResponsesWebsockets` too.
fn socket_policy(lane: openai::Lane, opts: &crate::config::provider::ProviderOpts) -> crate::config::provider::Websockets {
    let lane = match lane {
        openai::Lane::Codex => true,
        openai::Lane::Responses => opts.openai_responses_websockets,
        openai::Lane::Chat => false,
    };
    if lane && opts.openai_websockets.enabled(true) {
        crate::config::provider::Websockets::On
    } else {
        crate::config::provider::Websockets::Off
    }
}

/// Resolves `provider/model` to a client and the bare model name.
pub fn make(cfg: &Config, spec: &str) -> Result<(Arc<dyn Provider>, String)> {
    make_with_effort(cfg, spec, None)
}

/// `make`, with `effort` replacing the configured level when this model supports it (task per-spawn effort).
pub fn make_with_effort(cfg: &Config, spec: &str, effort: Option<&str>) -> Result<(Arc<dyn Provider>, String)> {
    let (pname, model) = spec.split_once('/').ok_or_else(|| anyhow!("model must be `provider/model`, got `{spec}`"))?;
    if let Some(policy) = &cfg.policy {
        anyhow::ensure!(policy.provider_allowed(pname), "管理ポリシーで制限されています（{}）", policy.organization);
    }
    let p = cfg.providers.get(pname).ok_or_else(|| anyhow!("unknown provider `{pname}`"))?;
    crate::auth::ensure_not_paused(cfg, pname)?;
    let opts = &cfg.provider_opts;
    let effort = effort.map(str::to_string).filter(|level| effort_levels(cfg, spec).contains(&level.as_str())).or_else(|| model_effort(cfg, spec));
    let openrouter = p.kind == "openai"
        && (pname == "openrouter"
            || reqwest::Url::parse(&p.base_url).is_ok_and(|url| url.host_str().is_some_and(|host| host.ends_with("openrouter.ai"))));
    let model = match opts.openrouter_variant.suffix() {
        Some(suffix) if openrouter && !model.contains(':') => format!("{model}{suffix}"),
        _ => model.to_string(),
    };
    let fireworks = pname == "fireworks"
        || reqwest::Url::parse(&p.base_url).is_ok_and(|url| url.host_str().is_some_and(|host| host.ends_with("fireworks.ai")));
    let oauth = oauth(pname, p);
    let key = if oauth.is_some() { String::new() } else { key(pname, p)? };
    let http = reqwest::Client::new();
    let base = p.base_url.trim_end_matches('/').to_string();
    let lane = openai::lane(&base, &model, &p.kind);
    let mut wire = Wire::from(opts, fireworks);
    wire.append_only = opts.append_only_context.enabled(pname, &model);
    wire.service_tier = tier_for(&opts.tier, pname, &p.kind, &p.base_url, &model);
    wire.websockets = socket_policy(lane, opts);
    wire.extended_context = cfg.context_opts.extended_context;
    if p.kind == "codex" {
        let path = codex::path();
        if !path.exists() { bail!("{pname} にサインインしていません。/login {pname} を実行してください") }
        let codex = codex::Codex::new(base, path, effort).with_wire(wire);
        return Ok((limited(pname, opts, Arc::new(codex)), model));
    }
    // Kimi's Anthropic-compatible endpoint: `providers.kimiApiFormat` can override the kind when a
    // Kimi provider is configured with the OpenAI wire (its `base_url` then points at Moonshot).
    let kind = if opts.kimi_api_format.anthropic(&p.kind) && family(pname, &base) == Family::Kimi {
        "anthropic"
    } else {
        p.kind.as_str()
    };
    let provider: Arc<dyn Provider> = match kind {
        "anthropic" => Arc::new(anthropic::Anthropic { http, base, key, oauth, effort, wire }),
        "openai" => Arc::new(openai::OpenAi { http, copilot: base == copilot::BASE, base, key, effort, wire }),
        k => bail!("provider `{pname}`: unknown kind `{k}`"),
    };
    Ok((limited(pname, opts, provider), model))
}

/// Wire families the format switches apply to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Family {
    Kimi,
    Other,
}

/// Recognizes Kimi/Moonshot by provider name or endpoint, the same way OpenRouter and Fireworks are.
pub fn family(pname: &str, base: &str) -> Family {
    let named = |text: &str| {
        let text = text.to_ascii_lowercase();
        text.contains("kimi") || text.contains("moonshot")
    };
    match named(pname) || reqwest::Url::parse(base).is_ok_and(|url| url.host_str().is_some_and(named)) {
        true => Family::Kimi,
        false => Family::Other,
    }
}

/// Applies `max_in_flight` for this provider; the semaphore is shared by every client of the process.
fn limited(pname: &str, opts: &crate::config::provider::ProviderOpts, inner: Arc<dyn Provider>) -> Arc<dyn Provider> {
    let Some(&max) = opts.max_in_flight.get(pname).filter(|max| **max > 0) else { return inner };
    static LIMITS: std::sync::LazyLock<std::sync::Mutex<std::collections::BTreeMap<(String, usize), Arc<tokio::sync::Semaphore>>>> =
        std::sync::LazyLock::new(Default::default);
    let semaphore = LIMITS.lock().unwrap().entry((pname.to_string(), max)).or_insert_with(|| Arc::new(tokio::sync::Semaphore::new(max))).clone();
    Arc::new(Limited { inner, semaphore })
}

/// Holds one provider request slot while the request runs.
struct Limited {
    inner: Arc<dyn Provider>,
    semaphore: Arc<tokio::sync::Semaphore>,
}

impl Provider for Limited {
    fn append_only(&self) -> bool {
        self.inner.append_only()
    }

    fn live_steering(&self, model: &str) -> bool {
        self.inner.live_steering(model)
    }

    fn complete<'a>(&'a self, req: Request<'a>, on_text: &'a (dyn Fn(&str) + Send + Sync)) -> BoxFut<'a, Result<Reply>> {
        Box::pin(async move {
            let _slot = self.semaphore.clone().acquire_owned().await.map_err(|_| anyhow!("provider concurrency limiter closed"))?;
            self.inner.complete(req, on_text).await
        })
    }
}


pub(crate) fn key(pname: &str, p: &crate::config::ProviderCfg) -> Result<String> {
    match &p.api_key {
        Some(key) => crate::secrets::resolve(key, pname),
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
    let allowed = |id: String| cfg.provider_opts.model_allowed(&id).then_some(id);
    if cfg.provider_opts.models_offline {
        return Ok(p.models.iter().map(|model| format!("{pname}/{model}")).filter_map(allowed).collect());
    }
    if p.kind == "codex" {
        if !codex::path().exists() { return Ok(vec![]); }
        let cached = codex::models();
        let models = if cached.is_empty() {
            ["gpt-5.5", "gpt-5.5-codex", "gpt-6-luna", "gpt-6-sol"].iter().map(|id| format!("{pname}/{id}")).collect::<Vec<_>>()
        } else {
            cached.into_iter().map(|model| format!("{pname}/{}", model.id)).collect::<Vec<_>>()
        };
        return Ok(models.into_iter().filter_map(allowed).collect());
    }
    let (base, http) = (p.base_url.trim_end_matches('/'), reqwest::Client::new());
    let req = match (p.kind.as_str(), oauth(pname, p)) {
        ("anthropic", Some(path)) => http.get(format!("{base}/v1/models?limit=1000"))
            .bearer_auth(anthropic_oauth::access_token(&path).await?)
            .header("anthropic-beta", anthropic_oauth::BETAS)
            .header(reqwest::header::USER_AGENT, anthropic_oauth::user_agent().await)
            .header("x-app", "cli")
            .header("anthropic-version", "2026-10-21"),
        ("anthropic", None) => http.get(format!("{base}/v1/models?limit=1000")).header("x-api-key", key(pname, p)?).header("anthropic-version", "2026-10-21"),
        _ => {
            let req = http.get(format!("{base}/models")).bearer_auth(key(pname, p)?);
            if base == copilot::BASE { copilot::discovery_headers().into_iter().fold(req, |r, (k, v)| r.header(k, v)) } else { req }
        }
    };
    let v: serde_json::Value = req.timeout(Duration::from_secs(10)).send().await?.error_for_status()?.json().await?;
    // `{"data": [...]}` is the standard shape; a few servers return the array itself.
    let list = v["data"].as_array().or(v.as_array()).ok_or_else(|| anyhow!("{pname}: unexpected /models reply"))?;
    Ok(list.iter().filter_map(|m| m["id"].as_str()).map(|id| format!("{pname}/{id}")).filter_map(allowed).collect())
}

/// Sends with the configured retry policy on 429/5xx/connection errors.
pub(crate) async fn send(policy: &crate::config::provider::Retry, build: impl Fn() -> reqwest::RequestBuilder) -> Result<reqwest::Response> {
    let mut attempt = 0;
    loop {
        let err = match build().send().await {
            Ok(r) if r.status().is_success() => return Ok(r),
            Ok(r) => {
                let status = r.status();
                let retry = status.as_u16() == 429 || status.is_server_error();
                let headers = r.headers().clone();
                let body = r.text().await.unwrap_or_default();
                let e = limit::http_error(status, &headers, &body);
                if !retry {
                    return Err(e);
                }
                e
            }
            Err(e) => e.into(),
        };
        let limit_error = err.downcast_ref::<limit::LimitError>();
        let usage_reset = limit_error.and_then(|limit| limit.wait);
        // `retry.rateLimitWalksChain`: a rate limit goes straight to the fallback chain instead of spending retries.
        if limit_error.is_some() && policy.rate_limit_walks_chain {
            return Err(err);
        }
        match policy.wait(attempt, usage_reset) {
            Some(wait) => {
                tokio::time::sleep(wait).await;
                attempt += 1;
            }
            // Out of retries, or the limit reset is farther out than `max_delay_ms`; waiting would only delay the notice.
            None => return Err(err),
        }
    }
}

/// Feeds each SSE `data:` payload to `on`; `on` returns true to stop early.
pub(crate) async fn sse(mut resp: reqwest::Response, timeouts: StreamTimeouts, mut on: impl FnMut(&str) -> Result<bool>) -> Result<()> {
    let mut buf: Vec<u8> = Vec::new();
    let mut first = timeouts.first;
    loop {
        // The first-event budget applies to the first chunk only; later gaps use the idle budget.
        let budget = first.take().or(timeouts.idle);
        let chunk = match budget {
            Some(budget) => tokio::time::timeout(budget, resp.chunk())
                .await
                .map_err(|_| anyhow!("model stream stayed silent for {}s", budget.as_secs()))??,
            None => resp.chunk().await?,
        };
        let Some(chunk) = chunk else { break };
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stream_timeouts_map_omps_sentinels() {
        let default = StreamTimeouts::new(-1, -1);
        assert_eq!((default.first, default.idle), (Some(Duration::from_secs(900)), Some(Duration::from_secs(900))));
        let off = StreamTimeouts::new(0, 0);
        assert_eq!((off.first, off.idle), (None, None));
        let explicit = StreamTimeouts::new(5, 90);
        assert_eq!((explicit.first, explicit.idle), (Some(Duration::from_secs(5)), Some(Duration::from_secs(90))));
    }

    #[tokio::test]
    async fn offline_discovery_returns_only_configured_and_enabled_models() {
        let mut cfg = Config { model: "anthropic/current".into(), ..Default::default() };
        cfg.providers.insert("anthropic".into(), crate::config::ProviderCfg {
            kind: "anthropic".into(),
            base_url: "https://api.anthropic.com".into(),
            models: vec!["keep".into(), "hidden".into()],
            ..Default::default()
        });
        cfg.provider_opts.models_offline = true;
        // No network: offline answers from `providers.*.models`, after the enabledModels filter.
        assert_eq!(list_models(&cfg, "anthropic").await.unwrap(), ["anthropic/keep", "anthropic/hidden"]);
        cfg.provider_opts.enabled_models = vec!["anthropic/keep".into()];
        assert_eq!(list_models(&cfg, "anthropic").await.unwrap(), ["anthropic/keep"]);
        cfg.provider_opts.disabled_providers = vec!["anthropic".into()];
        assert!(list_models(&cfg, "anthropic").await.unwrap().is_empty());
    }

    #[test]
    fn kimi_format_switches_the_wire_for_kimi_providers_only() {
        use crate::config::provider::KimiApiFormat;
        // Auto follows the configured kind; an explicit format overrides it.
        assert!(KimiApiFormat::Auto.anthropic("anthropic"));
        assert!(!KimiApiFormat::Auto.anthropic("openai"));
        assert!(KimiApiFormat::Anthropic.anthropic("openai"));
        assert!(!KimiApiFormat::Openai.anthropic("anthropic"));
        // Only Kimi/Moonshot providers are affected, by name or endpoint.
        assert_eq!(family("kimi", "https://api.moonshot.cn/v1"), Family::Kimi);
        assert_eq!(family("custom", "https://api.moonshot.ai/anthropic"), Family::Kimi);
        assert_eq!(family("custom", "https://api.kimi.com/v1"), Family::Kimi);
        assert_eq!(family("openai", "https://api.openai.com/v1"), Family::Other);
        assert_eq!(family("custom", "not a url"), Family::Other);
    }

    #[test]
    fn build_uses_the_anthropic_wire_for_a_kimi_provider_asked_for_it() {
        use crate::config::provider::KimiApiFormat;
        let mut cfg = Config::default();
        cfg.providers.insert("kimi".into(), crate::config::ProviderCfg {
            kind: "openai".into(),
            base_url: "https://api.moonshot.cn/v1".into(),
            api_key: Some("test".into()),
            ..Default::default()
        });
        cfg.provider_opts.kimi_api_format = KimiApiFormat::Anthropic;
        let (provider, model) = make(&cfg, "kimi/kimi-k2").unwrap();
        assert_eq!(model, "kimi-k2");
        assert!(provider.append_only(), "Anthropic caching is append-only by default too");
        // The same provider under the OpenAI format stays on Chat Completions.
        cfg.provider_opts.kimi_api_format = KimiApiFormat::Openai;
        assert!(make(&cfg, "kimi/kimi-k2").is_ok());
        // A non-Kimi provider ignores the setting entirely (unknown base, same wire either way).
        cfg.providers.insert("proxy".into(), crate::config::ProviderCfg {
            kind: "openai".into(),
            base_url: "https://api.example.com/v1".into(),
            api_key: Some("test".into()),
            ..Default::default()
        });
        cfg.provider_opts.kimi_api_format = KimiApiFormat::Anthropic;
        assert!(make(&cfg, "proxy/some-model").is_ok());
    }

    #[test]
    fn websocket_policy_reaches_only_the_lanes_that_carry_a_socket() {
        use crate::config::provider::Websockets;
        assert!(Websockets::On.enabled(false), "On attempts the socket even when the catalog is unsure");
        assert!(!Websockets::Off.enabled(true), "Off always wins");
        assert!(Websockets::Auto.enabled(true));
        assert!(!Websockets::Auto.enabled(false));
        // Codex always has a socket; standard Responses models need the separate opt-in.
        let mut opts = crate::config::provider::ProviderOpts::default();
        use openai::Lane;
        assert_eq!(socket_policy(Lane::Codex, &opts), Websockets::On);
        assert_eq!(socket_policy(Lane::Responses, &opts), Websockets::Off, "the Responses socket is opt-in");
        assert_eq!(socket_policy(Lane::Chat, &opts), Websockets::Off, "Chat has no socket");
        opts.openai_responses_websockets = true;
        assert_eq!(socket_policy(Lane::Responses, &opts), Websockets::On);
        opts.openai_websockets = Websockets::Off;
        assert_eq!(socket_policy(Lane::Codex, &opts), Websockets::Off, "the master switch still wins");
        assert_eq!(socket_policy(Lane::Responses, &opts), Websockets::Off, "the master switch still wins");
    }

    #[test]
    fn live_steering_needs_an_on_socket_and_a_gpt6_responses_model() {
        use crate::config::provider::Websockets;
        let codex = |on: bool, model: &str| {
            let mut cfg = Config::default();
            cfg.providers.insert("codex".into(), crate::config::ProviderCfg {
                kind: "codex".into(),
                base_url: codex::BASE.into(),
                ..Default::default()
            });
            let mut wire = Wire::default();
            wire.websockets = if on { Websockets::On } else { Websockets::Off };
            let client = codex::Codex::new(codex::BASE.into(), codex::path(), None).with_wire(wire);
            client.live_steering(model)
        };
        assert!(codex(true, "gpt-6-sol"));
        assert!(!codex(false, "gpt-6-sol"));
        assert!(!codex(true, "gpt-5.5-codex"));
        let mut openai = openai::OpenAi {
            http: reqwest::Client::new(),
            base: "https://api.openai.com/v1".into(),
            key: "test".into(),
            copilot: false,
            effort: None,
            wire: Wire::default(),
        };
        openai.wire.websockets = Websockets::On;
        assert!(openai.live_steering("gpt-6-sol"));
        assert!(!openai.live_steering("gpt-5.6-sol"), "only GPT-6 streams accept a steer");
        openai.base = "https://api.example.com/v1".into();
        assert!(!openai.live_steering("gpt-6-sol"), "a compatible proxy keeps the Chat wire");
    }

    #[test]
    fn live_steer_remembers_what_the_transport_consumed() {
        let live = LiveSteer::default();
        assert!(live.is_empty());
        live.push("first".into());
        live.push("second".into());
        assert!(!live.is_empty());
        // A transport that took them carried both into the response.
        assert_eq!(live.take(), ["first", "second"]);
        assert!(live.is_empty());
        assert_eq!(live.take_sent(), ["first", "second"], "recorded once");
        assert!(live.take_sent().is_empty(), "cleared after recording");
        // A message the transport never consumed is not recorded as answered.
        live.push("third".into());
        assert_eq!(live.take_sent(), Vec::<String>::new());
        assert_eq!(live.take(), ["third"]);
    }

    #[test]
    fn append_only_follows_its_setting_and_known_prefix_caches() {
        use crate::config::provider::AppendOnly;
        assert!(AppendOnly::Auto.enabled("deepseek", "deepseek-chat"));
        assert!(AppendOnly::Auto.enabled("custom", "claude-sonnet-5"));
        assert!(AppendOnly::Auto.enabled("xiaomi", "mimo-7b"));
        assert!(!AppendOnly::Auto.enabled("openai", "gpt-4o-mini"));
        assert!(AppendOnly::On.enabled("openai", "gpt-4o-mini"), "On is unconditional");
        assert!(!AppendOnly::Off.enabled("deepseek", "deepseek-chat"), "Off is unconditional");
    }
}
