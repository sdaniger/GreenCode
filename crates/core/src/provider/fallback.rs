//! `retry.fallbackChains`: switching to another model after a provider error, and back after the cooldown.
//!
//! Roles resolve first, then exact model selectors, then `provider/*` wildcards and finally
//! id-prefixed wildcards (`openrouter/google/*`). A wildcard entry keeps the failing model's bare
//! id and only replaces the provider (or its prefix).
use crate::{
    config::{
        provider::{FallbackOn, FallbackRevertPolicy, UsageReservePolicy},
        Config,
    },
    provider::{self, limit, LiveSteer, Provider, Reply, Request},
    usage, Event, EventTx, Message, ToolDef,
};
use anyhow::{anyhow, bail, Result};
use std::sync::Arc;

/// Cooldown used when the failing reply does not say when its limit resets.
const COOLDOWN_SECS: u64 = 300;
const USAGE_CACHE_SECS: u64 = 60;

fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// What one agent switched to, and which primaries are still cooling down.
#[derive(Default)]
pub(crate) struct State {
    active: Option<Active>,
    suppressed: std::collections::BTreeMap<String, u64>,
    /// Provider quota reports, cached so the preflight does not hit the network on every request.
    usage: Option<(u64, Vec<usage::Report>)>,
}

struct Active {
    spec: String,
}

/// Roles that may name a chain: the sub-agent definition, every `model_roles` entry pointing at the
/// active selector, and `default` for the main agent.
pub(crate) fn roles(cfg: &Config, spec: &str, agent: Option<&str>, subagent: bool) -> Vec<String> {
    let mut roles: Vec<String> = Vec::new();
    if let Some(name) = agent {
        roles.push(name.to_string());
    }
    for (name, selector) in &cfg.model_roles {
        let (bare, _) = thinking(selector);
        if bare == spec && !roles.contains(name) {
            roles.push(name.clone());
        }
    }
    if !subagent {
        roles.push("default".into());
    }
    roles
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Candidate {
    /// `provider/model` with no thinking suffix.
    pub spec: String,
    /// Explicit `:level` suffix; `None` inherits the failing turn's effort.
    pub effort: Option<String>,
}

/// Effort spellings a fallback entry may pin.
const EFFORTS: &[&str] = &["minimal", "low", "medium", "high", "xhigh", "max"];

fn thinking(entry: &str) -> (&str, Option<String>) {
    match entry.rsplit_once(':') {
        Some((spec, level)) if EFFORTS.contains(&level) => (spec, Some(level.to_string())),
        // shortcut: GreenCode has no `off` wire spelling; such an entry inherits the turn's effort instead.
        Some((spec, "off")) => (spec, None),
        _ => (entry, None),
    }
}

/// One fallback entry as a concrete selector. `provider/*` and `provider/prefix/*` keep the failing
/// model's bare id (`google/gemini-x` -> `openrouter/google/gemini-x`).
fn expand(entry: &str, failing: &str) -> Option<Candidate> {
    let (entry, effort) = thinking(entry);
    let (pname, rest) = entry.split_once('/')?;
    let Some(prefix) = rest.strip_suffix('*') else {
        return Some(Candidate { spec: entry.to_string(), effort });
    };
    let bare = failing.rsplit('/').next()?;
    Some(Candidate { spec: format!("{pname}/{prefix}{bare}"), effort: None })
}

/// The ordered candidates for one failing selector, already filtered of the failing selector itself.
pub(crate) fn chain(cfg: &Config, spec: &str, roles: &[String]) -> Vec<Candidate> {
    let Some((provider_name, rest)) = spec.split_once('/') else { return Vec::new() };
    let (failing, _) = thinking(spec);
    let chains = &cfg.provider_opts.retry.fallback_chains;
    let mut keys: Vec<(u8, &String)> = Vec::new();
    for key in chains.keys() {
        let (bare_key, _) = thinking(key);
        let rank = if roles.iter().any(|role| role == key) {
            0
        } else if !key.contains('*') && bare_key == failing {
            1
        } else if key == &format!("{provider_name}/*") {
            2
        } else if let Some(prefix) = key.strip_prefix(&format!("{provider_name}/")).and_then(|r| r.strip_suffix('*')) {
            if !rest.starts_with(prefix) {
                continue;
            }
            3
        } else {
            continue;
        };
        keys.push((rank, key));
    }
    // Longest id prefix first within the id-prefixed class; everything else is deterministic by key.
    keys.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| b.1.len().cmp(&a.1.len())).then_with(|| a.1.cmp(b.1)));
    let mut out: Vec<Candidate> = Vec::new();
    for (_, key) in keys {
        for entry in &chains[key] {
            let Some(candidate) = expand(entry, spec) else { continue };
            if candidate.spec == failing || out.iter().any(|seen| seen.spec == candidate.spec) {
                continue;
            }
            out.push(candidate);
        }
    }
    out
}

/// The trigger policy decides which failures may leave the failing model.
fn allows(on: FallbackOn, err: &anyhow::Error) -> bool {
    let limited = err.downcast_ref::<limit::LimitError>().is_some();
    match on {
        FallbackOn::Any => true,
        FallbackOn::UsageLimit => limited,
        FallbackOn::ExceptUsageLimit => !limited,
    }
}

fn reset_at(err: &anyhow::Error) -> Option<u64> {
    err.downcast_ref::<limit::LimitError>().and_then(|limit| limit.reset_at).filter(|at| *at > now())
}

/// Subscription reports name their plans, not the provider id chains are keyed by.
fn provider_of_report(name: &str) -> Option<&'static str> {
    match name {
        "Claude" => Some("anthropic"),
        "GitHub Copilot" => Some("github-copilot"),
        _ => None,
    }
}

/// Remaining percentage of the tightest known limit for `provider`, below the reserve margin.
fn reserve_pressure(reports: &[usage::Report], provider_name: &str, pct: u8) -> Option<f64> {
    reports
        .iter()
        .filter(|report| provider_of_report(&report.provider) == Some(provider_name))
        .flat_map(|report| report.limits.iter())
        .map(|limit| 100. - limit.used)
        // Like OMP, a window sitting exactly on the reserve is inside it (slack for `100 - used` rounding).
        .filter(|remaining| *remaining <= pct as f64 + 1e-9)
        .min_by(f64::total_cmp)
}

/// Reports for this config, reusing the cache while it is fresh. Only signed-in coding plans report.
async fn reports(cfg: &Config, state: &mut State, at: u64) -> Vec<usage::Report> {
    if let Some((cached_at, cached)) = &state.usage {
        if at.saturating_sub(*cached_at) < USAGE_CACHE_SECS {
            return cached.clone();
        }
    }
    let fresh: Vec<_> = usage::all(cfg).await.into_iter().filter_map(Result::ok).collect();
    state.usage = Some((at, fresh.clone()));
    fresh
}

async fn confirm(tx: &EventTx, message: &str) -> bool {
    let (reply, rx) = tokio::sync::oneshot::channel();
    let request = serde_json::json!({"method": "confirm", "title": "使用量の予備を確保", "message": message});
    if tx.send(Event::Dialog { request, reply }).is_err() {
        return false;
    }
    matches!(
        tokio::time::timeout(std::time::Duration::from_secs(120), rx).await,
        Ok(Ok(value)) if value["confirmed"] == true
    )
}

/// What a provider needs to receive for one attempt.
pub(crate) struct Template<'a> {
    pub system: &'a str,
    pub messages: &'a [Message],
    pub tools: &'a [ToolDef],
    pub max_tokens: u32,
    pub live: Option<&'a LiveSteer>,
}

pub(crate) struct Outcome {
    pub reply: Reply,
    /// The selector that answered, possibly a fallback.
    pub spec: String,
}

/// Sends `tpl` to the primary, walking the fallback chain per `retry.*` when it fails.
/// `build(tpl_spec, effort)` constructs a fallback client; the primary reuses the agent's client.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn request(
    cfg: &Config,
    primary: &Arc<dyn Provider>,
    primary_model: &str,
    spec: &str,
    roles: &[String],
    subagent: bool,
    state: &mut State,
    tpl: &Template<'_>,
    on_text: &(dyn Fn(&str) + Send + Sync),
    build: &(dyn Fn(&str, Option<&str>) -> Result<(Arc<dyn Provider>, String)> + Send + Sync),
    tx: &EventTx,
) -> Result<Outcome> {
    let at = now();
    let retry = &cfg.provider_opts.retry;
    if retry.fallback_revert_policy == FallbackRevertPolicy::CooldownExpiry
        && state.active.is_some()
        && state.suppressed.get(spec).is_none_or(|until| *until <= at)
    {
        state.active = None;
        let _ = tx.send(Event::Notify(format!("主モデル {spec} に復帰しました")));
    }
    let candidates = if retry.model_fallback || retry.usage_aware_fallback {
        chain(cfg, spec, roles)
    } else {
        Vec::new()
    };
    // Usage-aware: prefer configured fallbacks before a hard limit, per the reserve policy.
    let mut prefer_fallback = false;
    if retry.usage_aware_fallback {
        if let Some((provider_name, _)) = spec.split_once('/') {
            let reports = reports(cfg, state, at).await;
            if let Some(remaining) = reserve_pressure(&reports, provider_name, retry.usage_reserve_pct) {
                let message = format!(
                    "{provider_name} の残量が {remaining:.0}% です（予備 {}%）。フォールバックモデルに切り替えますか？",
                    retry.usage_reserve_pct
                );
                match retry.usage_reserve_policy {
                    UsageReservePolicy::Auto => prefer_fallback = true,
                    UsageReservePolicy::FailClosed => bail!("利用上限に近づいています: {message}"),
                    UsageReservePolicy::Confirm => prefer_fallback = subagent || confirm(tx, &message).await,
                }
            }
        }
    }

    enum Attempt {
        Primary,
        Other(Candidate),
    }
    let active_spec = state.active.as_ref().map(|active| active.spec.clone());
    let mut attempts: Vec<Attempt> = Vec::new();
    match &active_spec {
        Some(active) => attempts.push(Attempt::Other(Candidate { spec: active.clone(), effort: None })),
        None if prefer_fallback => {}
        None => attempts.push(Attempt::Primary),
    }
    for candidate in candidates {
        if active_spec.as_deref() == Some(candidate.spec.as_str()) {
            continue;
        }
        attempts.push(Attempt::Other(candidate));
    }
    if prefer_fallback && active_spec.is_none() {
        attempts.push(Attempt::Primary);
    }

    let inherited = provider::model_effort(cfg, spec);
    let total = attempts.len();
    let mut last: Option<anyhow::Error> = None;
    for (index, attempt) in attempts.into_iter().enumerate() {
        let usage_preferred = matches!(attempt, Attempt::Other(_));
        let (target, client, model) = match attempt {
            Attempt::Primary => (spec.to_string(), primary.clone(), primary_model.to_string()),
            Attempt::Other(candidate) => {
                let effort = candidate.effort.clone().or_else(|| inherited.clone());
                match build(&candidate.spec, effort.as_deref()) {
                    Ok((client, model)) => (candidate.spec, client, model),
                    Err(e) => {
                        last = Some(e);
                        continue;
                    }
                }
            }
        };
        let request = Request {
            model: &model,
            system: tpl.system,
            messages: tpl.messages,
            tools: tpl.tools,
            max_tokens: tpl.max_tokens,
            // Live steering is a property of the transport actually answering, not of the primary.
            live: tpl.live.filter(|_| client.live_steering(&model)),
        };
        match client.complete(request, on_text).await {
            Ok(reply) => {
                if target != spec {
                    if state.active.as_ref().is_none_or(|active| active.spec != target) {
                        let _ = tx.send(Event::Notify(format!("{spec} が失敗したため {target} に切り替えました")));
                    }
                    state.active = Some(Active { spec: target.clone() });
                }
                return Ok(Outcome { reply, spec: target });
            }
            Err(e) => {
                let walk = (retry.model_fallback && allows(retry.fallback_on, &e)) || usage_preferred;
                if index + 1 >= total || !walk {
                    return Err(e);
                }
                let until = reset_at(&e).unwrap_or_else(|| at.saturating_add(COOLDOWN_SECS));
                state.suppressed.insert(target.clone(), until);
                if state.active.as_ref().is_some_and(|active| active.spec == target) {
                    state.active = None;
                }
                let _ = tx.send(Event::Notify(format!("{target} が失敗しました: {e:#}")));
                last = Some(e);
            }
        }
    }
    Err(last.unwrap_or_else(|| anyhow!("フォールバック先のモデルがありません")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Block;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;
    use tokio::sync::mpsc;

    fn config(chains: &[(&str, &[&str])]) -> Config {
        let mut cfg = Config { model: "a/m".into(), ..Default::default() };
        cfg.provider_opts.retry.fallback_chains = chains
            .iter()
            .map(|(key, list)| (key.to_string(), list.iter().map(|s| s.to_string()).collect()))
            .collect();
        cfg
    }

    #[test]
    fn chains_resolve_roles_selectors_and_wildcards() {
        let cfg = config(&[
            ("default", &["openai/gpt-4o-mini"]),
            ("google-antigravity/*", &["google/*", "google-vertex/*"]),
            ("openrouter/google/*", &["deepseek/*"]),
            ("openai/gpt-5", &["anthropic/claude:low"]),
        ]);
        let specs = |spec: &str, roles: &[&str]| -> Vec<String> {
            chain(&cfg, spec, &roles.iter().map(|r| r.to_string()).collect::<Vec<_>>())
                .into_iter()
                .map(|c| format!("{}@{:?}", c.spec, c.effort))
                .collect()
        };
        assert_eq!(
            specs("anthropic/claude", &["default"]),
            ["openai/gpt-4o-mini@None"],
            "role keys resolve for the main agent"
        );
        assert_eq!(
            specs("google-antigravity/gemini-x", &[]),
            ["google/gemini-x@None", "google-vertex/gemini-x@None"],
            "provider wildcards keep the failing id"
        );
        assert_eq!(
            specs("openrouter/google/gemini-x", &[]),
            ["deepseek/gemini-x@None"],
            "id-prefixed keys match only that provider's ids under the prefix"
        );
        assert_eq!(
            specs("openai/gpt-5", &[]),
            ["anthropic/claude@Some(\"low\")"],
            "a selector key applies regardless of role and carries its thinking suffix"
        );
        let resolved = roles(&cfg, "anthropic/claude", Some("scout"), false);
        assert_eq!(resolved, ["scout", "default"]);
        let mut with_role = cfg.clone();
        with_role.model_roles.insert("worker".into(), "anthropic/claude".into());
        assert_eq!(roles(&with_role, "anthropic/claude", None, true), ["worker"]);
    }

    #[derive(Clone, Copy)]
    enum Step {
        Ok,
        Fail,
        Limit,
    }

    struct Mock {
        calls: Arc<AtomicUsize>,
        script: Arc<Mutex<Vec<Step>>>,
    }

    impl Mock {
        fn new(script: &[Step]) -> Arc<Self> {
            Arc::new(Self { calls: Default::default(), script: Arc::new(Mutex::new(script.to_vec())) })
        }
    }

    impl Provider for Mock {
        fn complete<'a>(
            &'a self,
            _req: Request<'a>,
            _on_text: &'a (dyn Fn(&str) + Send + Sync),
        ) -> crate::BoxFut<'a, Result<Reply>> {
            Box::pin(async move {
                self.calls.fetch_add(1, Ordering::Relaxed);
                let step = {
                    let mut script = self.script.lock().unwrap();
                    if script.len() > 1 { script.remove(0) } else { script.first().copied().unwrap_or(Step::Ok) }
                };
                match step {
                    Step::Ok => {
                        let mut reply = Reply::default();
                        reply.blocks.push(Block::Text { text: "ok".into() });
                        Ok(reply)
                    }
                    Step::Fail => Err(anyhow!("HTTP 500: boom")),
                    Step::Limit => Err(limit::parse(
                        429,
                        &reqwest::header::HeaderMap::new(),
                        r#"{"error":{"type":"rate_limit_error","message":"slow down"}}"#,
                        now(),
                    )
                    .unwrap()
                    .into()),
                }
            })
        }
    }

    struct Reply_ {
        out: Result<Outcome, anyhow::Error>,
        events: Vec<Event>,
    }

    async fn drive(
        cfg: &Config,
        primary: &Arc<Mock>,
        fallback: &Arc<Mock>,
        state: &mut State,
        fallback_spec: &str,
    ) -> Reply_ {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let client = primary.clone() as Arc<dyn Provider>;
        let fallback_client = fallback.clone() as Arc<dyn Provider>;
        let tpl = Template { system: "", messages: &[], tools: &[], max_tokens: 1, live: None };
        let build = move |spec: &str, _effort: Option<&str>| -> Result<(Arc<dyn Provider>, String)> {
            anyhow::ensure!(spec == fallback_spec, "unexpected fallback {spec}");
            Ok((fallback_client.clone(), "m".into()))
        };
        let out = request(cfg, &client, "m", "a/m", &["default".into()], false, state, &tpl, &|_| {}, &build, &tx).await;
        let mut events = Vec::new();
        while let Ok(event) = rx.try_recv() {
            events.push(event);
        }
        Reply_ { out, events }
    }

    fn notified(events: &[Event], needle: &str) -> bool {
        events.iter().any(|event| matches!(event, Event::Notify(text) if text.contains(needle)))
    }

    #[tokio::test]
    async fn switches_on_failure_then_reverts_after_the_cooldown() {
        let cfg = config(&[("default", &["b/m"])]);
        let primary = Mock::new(&[Step::Fail, Step::Ok]);
        let fallback = Mock::new(&[Step::Ok]);
        let mut state = State::default();
        let first = drive(&cfg, &primary, &fallback, &mut state, "b/m").await;
        assert_eq!(first.out.unwrap().spec, "b/m");
        assert!(notified(&first.events, "切り替え"));
        let (p, f) = (primary.calls.load(Ordering::Relaxed), fallback.calls.load(Ordering::Relaxed));
        assert_eq!((p, f), (1, 1));
        // The active fallback answers without retrying the primary.
        let second = drive(&cfg, &primary, &fallback, &mut state, "b/m").await;
        assert_eq!(second.out.unwrap().spec, "b/m");
        assert_eq!(primary.calls.load(Ordering::Relaxed), p);
        // Cooldown expiry: the primary answers again.
        state.suppressed.clear();
        let third = drive(&cfg, &primary, &fallback, &mut state, "b/m").await;
        assert_eq!(third.out.unwrap().spec, "a/m");
        assert!(notified(&third.events, "復帰"));
        assert_eq!(primary.calls.load(Ordering::Relaxed), p + 1);
        // `never` keeps an active fallback after the cooldown instead of reverting.
        let mut cfg = cfg;
        cfg.provider_opts.retry.fallback_revert_policy = FallbackRevertPolicy::Never;
        let fresh_primary = Mock::new(&[Step::Ok]);
        let mut active_state = State { active: Some(Active { spec: "b/m".into() }), ..Default::default() };
        let fourth = drive(&cfg, &fresh_primary, &fallback, &mut active_state, "b/m").await;
        assert_eq!(fourth.out.unwrap().spec, "b/m", "never keeps the active fallback");
        assert_eq!(fresh_primary.calls.load(Ordering::Relaxed), 0, "the primary is not retried");
        assert!(!notified(&fourth.events, "復帰"));
    }

    #[tokio::test]
    async fn fallback_on_gates_limit_and_other_errors() {
        let mut cfg = config(&[("default", &["b/m"])]);
        cfg.provider_opts.retry.fallback_on = FallbackOn::UsageLimit;
        let (primary, fallback) = (Mock::new(&[Step::Fail]), Mock::new(&[Step::Ok]));
        let mut state = State::default();
        assert!(drive(&cfg, &primary, &fallback, &mut state, "b/m").await.out.is_err());
        assert_eq!(fallback.calls.load(Ordering::Relaxed), 0);
        let limit = Mock::new(&[Step::Limit]);
        let switched = drive(&cfg, &limit, &fallback, &mut state, "b/m").await;
        assert_eq!(switched.out.unwrap().spec, "b/m");

        cfg.provider_opts.retry.fallback_on = FallbackOn::ExceptUsageLimit;
        let mut state = State::default();
        let (limit, fallback) = (Mock::new(&[Step::Limit, Step::Limit]), Mock::new(&[Step::Ok]));
        let kept = drive(&cfg, &limit, &fallback, &mut state, "b/m").await;
        assert!(kept.out.is_err());
        assert_eq!(fallback.calls.load(Ordering::Relaxed), 0, "a limit must not switch");
        let switched = drive(&cfg, &primary, &fallback, &mut state, "b/m").await;
        assert_eq!(switched.out.unwrap().spec, "b/m");
    }

    #[test]
    fn reserve_pressure_reads_plans_and_keeps_unknown_ones_primary() {
        let report = |provider: &str, used: f64| usage::Report {
            provider: provider.into(),
            plan: None,
            limits: vec![usage::Limit { label: "window".into(), used, resets_at: None }],
        };
        let reports = [report("Claude", 96.), report("GitHub Copilot", 50.)];
        assert_eq!(reserve_pressure(&reports, "anthropic", 10), Some(4.));
        assert_eq!(reserve_pressure(&reports, "anthropic", 5), Some(4.), "4% left is inside a 5% reserve");
        assert_eq!(reserve_pressure(&reports, "anthropic", 4), Some(4.), "a window on the reserve is inside it");
        assert_eq!(reserve_pressure(&reports, "anthropic", 3), None);
        assert_eq!(reserve_pressure(&reports, "github-copilot", 10), None);
        assert_eq!(reserve_pressure(&reports, "openai", 25), None, "unknown usage keeps the primary");
    }

    #[tokio::test]
    async fn rate_limit_retries_walk_or_fail_fast_per_setting() {
        use std::sync::atomic::AtomicUsize;
        use tokio::{
            io::{AsyncReadExt, AsyncWriteExt},
            net::TcpListener,
        };
        async fn hits(walk: bool) -> usize {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let count = Arc::new(AtomicUsize::new(0));
            let seen = count.clone();
            let server = tokio::spawn(async move {
                while let Ok((mut socket, _)) = listener.accept().await {
                    seen.fetch_add(1, Ordering::Relaxed);
                    let mut buf = [0u8; 1024];
                    let _ = socket.read(&mut buf).await;
                    let body = r#"{"error":{"type":"rate_limit_error","message":"slow down"}}"#;
                    let _ = socket
                        .write_all(
                            format!(
                                "HTTP/1.1 429 Too Many Requests\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                                body.len()
                            )
                            .as_bytes(),
                        )
                        .await;
                }
            });
            let retry = crate::config::provider::Retry {
                max_retries: 2,
                base_delay_ms: 1,
                max_delay_ms: 10,
                rate_limit_walks_chain: walk,
                ..Default::default()
            };
            let http = reqwest::Client::new();
            let url = format!("http://{addr}/x");
            match crate::provider::send(&retry, || http.get(&url)).await {
                Ok(_) => panic!("the mock answers 429"),
                Err(e) => assert!(e.downcast_ref::<limit::LimitError>().is_some(), "{e:#}"),
            }
            server.abort();
            count.load(Ordering::Relaxed)
        }
        assert_eq!(hits(true).await, 1, "a chain walk skips the waits");
        assert_eq!(hits(false).await, 3, "the default spends maxRetries first");
    }
}
