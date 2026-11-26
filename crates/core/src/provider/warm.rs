//! Prompt-cache warming: after a request, refresh its cache entry shortly before the entry expires,
//! and decide when a long tool call should be bridged by compaction instead.
use super::{Provider, Request};
use crate::config::provider::{CacheRetention, WarmingStrategy};
use crate::config::Config;
use crate::{Message, ToolDef};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// The request worth refreshing, captured after the agent's own successful request.
#[derive(Clone, Debug)]
pub struct Snapshot {
    pub model: String,
    pub system: String,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolDef>,
    pub max_tokens: u32,
    /// Prompt tokens of the response that wrote this entry, when the provider reported them.
    pub prompt_tokens: u64,
    /// Cost of one warm request in USD, when the model has a `prices` entry.
    pub price: Option<f64>,
}

/// Resolved warming policy for one provider/model/tier.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Plan {
    pub enabled: bool,
    /// `cache_control.ttl` of the tier that is being refreshed (Anthropic's 5m/1h windows).
    pub ttl: Duration,
    /// `false` for sub-agents, which only warm while their own tool call runs.
    pub idle: bool,
    /// Anthropic first-party accepts `max_tokens: 0`; everyone else needs one output token.
    pub free_prewarm: bool,
    pub strategy: WarmingStrategy,
    /// Prompt-token threshold from which compaction may bridge a long tool call.
    pub compact_at: u64,
}

impl Plan {
    /// Warming is off unless the provider is one whose cache entries expire on a known, short clock.
    pub fn resolve(cfg: &Config, provider: &str, model: &str, oauth: bool) -> Plan {
        let opts = &cfg.provider_opts;
        let anthropic = provider == "anthropic" || model.to_ascii_lowercase().contains("claude");
        let retention = opts.cache_retention;
        let ttl = match retention {
            // Long-lived entries do not need a refresh; only short ones are kept alive.
            CacheRetention::Long => Duration::from_secs(3600),
            _ if oauth => Duration::from_secs(3600),
            _ => Duration::from_secs(300),
        };
        let compact_at = match cfg.compaction_opts.maintenance.context_window {
            Some(window) => (window as f64 * opts.cache_warming_compact_percent as f64 / 100.).max(0.) as u64,
            None => u64::MAX,
        }
        .min(opts.cache_warming_compact_tokens);
        Plan {
            enabled: opts.cache_warming.during_run() && anthropic && retention != CacheRetention::None,
            ttl,
            idle: opts.cache_warming.when_idle(),
            free_prewarm: anthropic && provider == "anthropic",
            strategy: opts.cache_warming_strategy,
            compact_at,
        }
    }

    /// The moment to wake up: a little before expiry, so the refresh lands inside the window.
    pub fn refresh_at(&self) -> Duration {
        self.ttl.mul_f64(0.8)
    }

    /// Longest a request can stay silent before the refreshed entry is surely gone.
    pub fn idle_deadline(&self) -> Duration {
        // "idle also refreshes until 30 min after the last real request, then stops."
        Duration::from_secs(1800)
    }
}

/// Shared switches the agent flips; the warming task only reads them.
#[derive(Clone, Debug, Default)]
pub struct Gates {
    /// The agent is inside a turn (a tool call may be running).
    pub active: Arc<AtomicBool>,
    /// Set when the agent should compact at its next loop iteration.
    pub bridge: Arc<AtomicBool>,
    /// Bumped by every real request, so a warmer for a stale snapshot stops.
    pub generation: Arc<AtomicU64>,
}

/// Refreshes one snapshot every `refresh_at` until the cache would have expired anyway.
pub fn spawn(
    provider: Arc<dyn Provider>,
    snap: Snapshot,
    plan: Plan,
    gates: Gates,
    generation: u64,
) -> Option<tokio::task::JoinHandle<()>> {
    if !plan.enabled {
        return None;
    }
    Some(tokio::spawn(async move {
        let started = Instant::now();
        let mut spent = 0f64;
        loop {
            tokio::time::sleep(plan.refresh_at()).await;
            if gates.generation.load(Ordering::Relaxed) != generation {
                return;
            }
            let active = gates.active.load(Ordering::Relaxed);
            if !plan.idle && !active {
                return;
            }
            // Idle warming stops 30 minutes after the request it refreshes.
            if !active && started.elapsed() > plan.idle_deadline() {
                return;
            }
            // A prompt at the threshold may be compacted instead of refreshed; the agent runs it.
            let bridgeable = plan.strategy.may_compact() && snap.prompt_tokens >= plan.compact_at;
            if bridgeable && plan.strategy == WarmingStrategy::Compact {
                gates.bridge.store(true, Ordering::Relaxed);
                return;
            }
            // A refresh that costs more than the miss it avoids is not worth sending. `price` is one
            // prompt read; a miss costs a cache write plus that read, about twice as much.
            if let Some(price) = snap.price.filter(|_| !plan.free_prewarm) {
                if spent + price > 2. * price - 0.05 {
                    if bridgeable && plan.strategy == WarmingStrategy::Auto {
                        gates.bridge.store(true, Ordering::Relaxed);
                    }
                    return;
                }
            }
            let request = Request {
                model: &snap.model,
                system: &snap.system,
                messages: &snap.messages,
                tools: &snap.tools,
                max_tokens: if plan.free_prewarm { 0 } else { 1 },
                live: None,
            };
            if provider.complete(request, &|_| {}).await.is_err() {
                return;
            }
            spent += snap.price.unwrap_or(0.);
        }
    }))
}

/// Prompt tokens a compaction would buy back: everything above the configured threshold.
pub fn bridge_gain(prompt_tokens: u64, plan: Plan) -> u64 {
    prompt_tokens.saturating_sub(plan.compact_at)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::provider::{CacheWarming, ProviderOpts};

    fn cfg(warming: CacheWarming, strategy: WarmingStrategy) -> Config {
        let mut cfg = Config::default();
        cfg.provider_opts = ProviderOpts {
            cache_warming: warming,
            cache_warming_strategy: strategy,
            cache_warming_compact_percent: 50,
            cache_warming_compact_tokens: 150_000,
            ..Default::default()
        };
        cfg.compaction_opts.maintenance.context_window = Some(200_000);
        cfg
    }

    #[test]
    fn warming_is_scoped_to_anthropic_with_a_short_cache_tier() {
        assert!(!Plan::resolve(&cfg(CacheWarming::Idle, WarmingStrategy::Auto), "openai", "gpt-6-sol", false).enabled);
        let api = Plan::resolve(&cfg(CacheWarming::Idle, WarmingStrategy::Auto), "anthropic", "claude-opus-5", false);
        assert!(api.enabled);
        assert_eq!(api.ttl, Duration::from_secs(300));
        assert!(api.free_prewarm);
        // A subscription uses the one-hour tier; long is already long-lived either way.
        assert_eq!(Plan::resolve(&cfg(CacheWarming::Idle, WarmingStrategy::Auto), "anthropic", "claude-opus-5", true).ttl, Duration::from_secs(3600));
        // Off disables entirely, and `none` retention has no entry to keep alive.
        assert!(!Plan::resolve(&cfg(CacheWarming::Off, WarmingStrategy::Auto), "anthropic", "claude-opus-5", false).enabled);
        let mut none = cfg(CacheWarming::Idle, WarmingStrategy::Auto);
        none.provider_opts.cache_retention = CacheRetention::None;
        assert!(!Plan::resolve(&none, "anthropic", "claude-opus-5", false).enabled);
    }

    #[test]
    fn compact_threshold_is_the_lower_of_window_share_and_tokens() {
        let plan = Plan::resolve(&cfg(CacheWarming::Idle, WarmingStrategy::Auto), "anthropic", "claude-opus-5", false);
        assert_eq!(plan.compact_at, 100_000, "50% of 200k is lower than 150k");
        assert_eq!(bridge_gain(120_000, plan), 20_000);
        let mut small = cfg(CacheWarming::Idle, WarmingStrategy::Auto);
        small.provider_opts.cache_warming_compact_percent = 80;
        assert_eq!(Plan::resolve(&small, "anthropic", "claude-opus-5", false).compact_at, 150_000);
        // No window registry entry: the token limit alone applies.
        let mut no_window = cfg(CacheWarming::Idle, WarmingStrategy::Auto);
        no_window.compaction_opts.maintenance.context_window = None;
        assert_eq!(Plan::resolve(&no_window, "anthropic", "claude-opus-5", false).compact_at, 150_000);
    }

    #[test]
    fn refresh_lands_before_expiry() {
        let plan = Plan::resolve(&cfg(CacheWarming::Idle, WarmingStrategy::Auto), "anthropic", "claude-opus-5", false);
        assert_eq!(plan.refresh_at(), Duration::from_secs(240));
        assert_eq!(plan.idle_deadline(), Duration::from_secs(1800));
        assert!(WarmingStrategy::Keepalive.may_compact() == false);
        assert!(WarmingStrategy::Auto.may_compact());
    }
}
