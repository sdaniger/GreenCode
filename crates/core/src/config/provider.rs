//! Settings for provider requests, retry, sampling, model lists and web search; flattened into `Config`.
use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Which models the pickers offer and in what provider order.
#[derive(Deserialize, Serialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct ProviderOpts {
    /// Glob selectors for `provider/model` or a bare model id; empty offers every model.
    pub enabled_models: Vec<String>,
    /// Provider ids (globs allowed) the pickers offer; empty offers every provider.
    pub enabled_providers: Vec<String>,
    /// Provider ids (globs allowed) the pickers never offer.
    pub disabled_providers: Vec<String>,
    /// Provider ids in picker order; unlisted providers follow alphabetically.
    pub model_provider_order: Vec<String>,
    /// Per-request sampling overrides; a negative value keeps the provider default.
    pub sampling: Sampling,
    /// Response verbosity for the Codex Responses API.
    pub text_verbosity: TextVerbosity,
    /// Retry policy for provider requests.
    pub retry: Retry,
    /// Web search backends.
    pub web: Web,
    /// Maximum concurrent requests per provider id; omitted providers are unlimited.
    pub max_in_flight: std::collections::BTreeMap<String, usize>,
    /// Seconds to wait for the first stream event; -1 uses the 300s default, 0 disables.
    pub stream_first_event_timeout_seconds: i64,
    /// Seconds a stream may stay silent between chunks; -1 uses the 300s default, 0 disables.
    pub stream_idle_timeout_seconds: i64,
    /// Routing-variant suffix appended to OpenRouter model ids.
    pub openrouter_variant: OpenRouterVariant,
    /// Serving path for Fireworks requests.
    pub fireworks_tier: FireworksTier,
    /// Discover models from the network; off keeps the configured lists only.
    pub models_offline: bool,
    /// Session cost warning and cap, in USD.
    pub cost: CostLimits,
    /// Thinking token budgets by effort level.
    pub thinking: ThinkingBudgets,
    /// Anthropic-specific request behavior.
    pub anthropic: Anthropic,
    /// Wire format for Kimi Code providers: auto follows the configured model kind.
    pub kimi_api_format: KimiApiFormat,
    /// WebSocket policy for the Codex (ChatGPT subscription) transport.
    pub openai_websockets: Websockets,
    /// Use the WebSocket transport for standard Responses models on the official OpenAI endpoint.
    pub openai_responses_websockets: bool,
    /// Deliver messages typed while a GPT-6 response streams into that response (Codex WebSocket).
    pub openai_live_steering: bool,
    /// Prompt-cache retention forwarded to providers that support it.
    pub cache_retention: CacheRetention,
    /// Refresh the last request's prompt-cache entry before it expires.
    pub cache_warming: CacheWarming,
    /// How a long tool call is bridged once refreshing stops paying.
    pub cache_warming_strategy: WarmingStrategy,
    /// Context-window share a prompt must reach before a long tool call may be bridged by compaction.
    pub cache_warming_compact_percent: u32,
    /// Prompt-token count from which a long tool call may be bridged by compaction.
    pub cache_warming_compact_tokens: u64,
    /// Keep a byte-stable prompt prefix so provider prefix caches hit.
    pub append_only_context: AppendOnly,
    /// Omit thinking summaries upstream where the provider supports it.
    pub omit_thinking: bool,
    /// Private scratchpad: provider reasoning off, a `think` tool instead.
    pub external_thinking: bool,
    /// Highest effort the `auto` classifier may resolve.
    pub auto_thinking_max_effort: AutoThinkingMaxEffort,
    /// Per-family processing tiers (`tier.*`) and the subagent/advisor overrides.
    pub tier: TierCfg,
    /// Where model role assignments are saved.
    pub model_role_storage: ModelRoleStorage,
    /// Render tool descriptors in the system prompt and strip them from provider schemas.
    pub inline_tool_descriptors: InlineToolDescriptors,
    /// List available skills in the system prompt.
    pub skillful: bool,
}

impl Default for ProviderOpts {
    fn default() -> Self {
        Self {
            enabled_models: Vec::new(),
            enabled_providers: Vec::new(),
            disabled_providers: Vec::new(),
            model_provider_order: Vec::new(),
            sampling: Sampling::default(),
            text_verbosity: TextVerbosity::default(),
            retry: Retry::default(),
            web: Web::default(),
            max_in_flight: Default::default(),
            stream_first_event_timeout_seconds: -1,
            stream_idle_timeout_seconds: -1,
            openrouter_variant: OpenRouterVariant::default(),
            fireworks_tier: FireworksTier::default(),
            models_offline: false,
            cost: CostLimits::default(),
            thinking: ThinkingBudgets::default(),
            anthropic: Anthropic::default(),
            kimi_api_format: KimiApiFormat::default(),
            openai_websockets: Websockets::default(),
            openai_responses_websockets: false,
            openai_live_steering: false,
            cache_retention: CacheRetention::default(),
            cache_warming: CacheWarming::default(),
            cache_warming_strategy: WarmingStrategy::default(),
            cache_warming_compact_percent: 60,
            cache_warming_compact_tokens: 150_000,
            append_only_context: AppendOnly::default(),
            omit_thinking: false,
            external_thinking: false,
            auto_thinking_max_effort: AutoThinkingMaxEffort::default(),
            tier: TierCfg::default(),
            model_role_storage: ModelRoleStorage::default(),
            inline_tool_descriptors: InlineToolDescriptors::default(),
            skillful: true,
        }
    }
}

impl ProviderOpts {
    /// Whether the pickers may offer this provider.
    pub fn provider_allowed(&self, provider: &str) -> bool {
        (self.enabled_providers.is_empty() || self.enabled_providers.iter().any(|pattern| glob(pattern, provider)))
            && !self.disabled_providers.iter().any(|pattern| glob(pattern, provider))
    }

    /// Whether the pickers may offer this `provider/model` selector.
    pub fn model_allowed(&self, spec: &str) -> bool {
        if let Some((provider, _)) = spec.split_once('/') {
            if !self.provider_allowed(provider) {
                return false;
            }
        }
        if self.enabled_models.is_empty() {
            return true;
        }
        let bare = spec.split_once('/').map_or(spec, |(_, model)| model);
        self.enabled_models.iter().any(|raw| {
            // OMP patterns may pin a thinking level (`anthropic/*:high`); the level is not part of the selector.
            let pattern = raw
                .split_once(':')
                .filter(|(_, level)| matches!(*level, "minimal" | "low" | "medium" | "high" | "xhigh" | "max" | "auto"))
                .map_or(raw.as_str(), |(pattern, _)| pattern);
            glob(pattern, spec) || glob(pattern, bare)
        })
    }

    /// Where this provider sorts in the picker; unlisted providers come last.
    pub fn provider_rank(&self, provider: &str) -> usize {
        self.model_provider_order.iter().position(|pattern| glob(pattern, provider)).unwrap_or(usize::MAX)
    }
}

/// `*` and `?` globs, enough for OMP's model selectors.
fn glob(pattern: &str, text: &str) -> bool {
    let (pattern, text): (Vec<char>, Vec<char>) = (pattern.chars().collect(), text.chars().collect());
    let (mut p, mut t, mut star, mut mark) = (0, 0, None, 0);
    while t < text.len() {
        if p < pattern.len() && (pattern[p] == '?' || pattern[p] == text[t]) {
            (p, t) = (p + 1, t + 1);
        } else if p < pattern.len() && pattern[p] == '*' {
            (star, p, mark) = (Some(p), p + 1, t);
        } else if let Some(at) = star {
            (p, t, mark) = (at + 1, mark + 1, mark + 1);
        } else {
            return false;
        }
    }
    pattern[p..].iter().all(|&c| c == '*')
}

/// Sampling overrides; `-1` leaves the provider's own default.
#[derive(Deserialize, Serialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct Sampling {
    pub temperature: f64,
    pub top_p: f64,
    pub top_k: f64,
    pub min_p: f64,
    pub presence_penalty: f64,
    pub repetition_penalty: f64,
}

impl Default for Sampling {
    fn default() -> Self {
        Self { temperature: -1., top_p: -1., top_k: -1., min_p: -1., presence_penalty: -1., repetition_penalty: -1. }
    }
}

impl Sampling {
    /// The value when it is an override rather than "provider default".
    pub fn value(setting: f64) -> Option<f64> {
        (setting >= 0.).then_some(setting)
    }
}

#[derive(Deserialize, Serialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum TextVerbosity {
    #[default]
    Low,
    Medium,
    High,
}

impl TextVerbosity {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
        }
    }
}

#[derive(Deserialize, Serialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum OpenRouterVariant {
    #[default]
    Default,
    Nitro,
    Floor,
    Online,
    Exacto,
}

impl OpenRouterVariant {
    /// OpenRouter's `:variant` suffix; `None` for the plain model id.
    pub fn suffix(self) -> Option<&'static str> {
        match self {
            Self::Default => None,
            Self::Nitro => Some(":nitro"),
            Self::Floor => Some(":floor"),
            Self::Online => Some(":online"),
            Self::Exacto => Some(":exacto"),
        }
    }
}

#[derive(Deserialize, Serialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum FireworksTier {
    #[default]
    Standard,
    Priority,
}

/// Kimi Code speaks either wire format; `auto` follows the provider kind (`anthropic` kind = Messages).
#[derive(Deserialize, Serialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum KimiApiFormat {
    #[default]
    Auto,
    Openai,
    Anthropic,
}

impl KimiApiFormat {
    /// The Messages wire when this format asks for Anthropic, or `auto` on an Anthropic-kind provider.
    pub fn anthropic(self, kind: &str) -> bool {
        match self {
            Self::Auto => kind == "anthropic",
            Self::Openai => false,
            Self::Anthropic => true,
        }
    }
}

/// WebSocket policy for a transport that also works over HTTP/SSE.
#[derive(Deserialize, Serialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Websockets {
    /// Use the socket when the provider and model support it.
    #[default]
    Auto,
    Off,
    On,
}

impl Websockets {
    /// Whether to attempt the socket; `Off` always wins, `On` attempts whenever the model allows it.
    pub fn enabled(self, supported: bool) -> bool {
        match self {
            Self::Off => false,
            Self::On => true,
            Self::Auto => supported,
        }
    }
}

/// Prompt-cache retention forwarded to providers that expose it.
#[derive(Deserialize, Serialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum CacheRetention {
    #[default]
    Auto,
    /// Five-minute entries (Anthropic's default TTL).
    Short,
    /// One-hour entries where the provider supports them.
    Long,
    /// Turn prompt caching off and send no cache markers.
    None,
}

impl CacheRetention {
    /// Anthropic's `cache_control.ttl`; `None` for the default five-minute window.
    pub fn anthropic_ttl(self) -> Option<&'static str> {
        (self == Self::Long).then_some("1h")
    }

    /// Whether cache markers belong in the request at all.
    pub fn caching(self) -> bool {
        self != Self::None
    }

    /// OpenAI's legacy `prompt_cache_retention`: `in_memory` or `24h`.
    pub fn prompt_cache_retention(self) -> Option<&'static str> {
        match self {
            Self::Short => Some("in_memory"),
            Self::Long => Some("24h"),
            Self::Auto | Self::None => None,
        }
    }
}

/// Prompt-cache warming: refresh the last request's cache entry before it expires.
#[derive(Deserialize, Serialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum CacheWarming {
    Off,
    /// Only while a tool call runs, so its prefix survives the gap.
    Streaming,
    /// Also between runs, for cache lifetimes shorter than the idle window.
    #[default]
    Idle,
}

impl CacheWarming {
    /// Whether warming runs during a long tool call.
    pub fn during_run(self) -> bool {
        self != Self::Off
    }

    /// Whether warming also runs while the agent idles between prompts.
    pub fn when_idle(self) -> bool {
        self == Self::Idle
    }
}

/// How a long tool call is bridged once refreshing stops paying for itself.
#[derive(Deserialize, Serialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum WarmingStrategy {
    /// Compact when that costs less than letting the entry expire.
    #[default]
    Auto,
    /// Never compact; keep refreshing or let it expire.
    Keepalive,
    /// Compact at the first decision.
    Compact,
}

impl WarmingStrategy {
    /// Whether a long tool call may be bridged by compacting the history.
    pub fn may_compact(self) -> bool {
        self != Self::Keepalive
    }
}

/// Byte-stable prompt prefix for providers that cache by prefix.
#[derive(Deserialize, Serialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum AppendOnly {
    /// On for providers known to cache prefixes.
    #[default]
    Auto,
    On,
    Off,
}

impl AppendOnly {
    /// Whether this provider/model keeps an append-only request prefix.
    pub fn enabled(self, provider: &str, model: &str) -> bool {
        match self {
            Self::On => true,
            Self::Off => false,
            Self::Auto => prefix_cache_provider(provider, model),
        }
    }
}

/// Hosts and model families that cache the request prefix server-side.
pub fn prefix_cache_provider(provider: &str, model: &str) -> bool {
    let host = provider.to_ascii_lowercase();
    let model = model.to_ascii_lowercase();
    ["deepseek", "moonshot", "kimi", "anthropic"].iter().any(|name| host.contains(name))
        || ["claude",
            "deepseek",
            "kimi",
            "moonshot",
            "glm",
            "qwen",
            "minimax",
            "xiaomi",
            "mimo",
            "sglang",
            "gemini",
            "gpt-4.1",
            "gpt-5",
            "gpt-6",
            "o1",
            "o3",
            "o4"]
            .iter()
            .any(|prefix| model.contains(prefix))
}

/// Highest effort the `auto` thinking classifier may resolve.
#[derive(Deserialize, Serialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum AutoThinkingMaxEffort {
    /// Keep the classifier one tier below the top; only an explicit `max` reaches it.
    #[default]
    Xhigh,
    Max,
}

/// Where model selector role assignments are saved.
#[derive(Deserialize, Serialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ModelRoleStorage {
    #[default]
    Global,
    Project,
}

/// Whether tool descriptors go into the system prompt instead of the tool schemas.
#[derive(Deserialize, Serialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum InlineToolDescriptors {
    /// On for Gemini models, off elsewhere.
    #[default]
    Auto,
    On,
    Off,
}

impl InlineToolDescriptors {
    /// Whether this provider/model pair sends descriptors in the prompt.
    pub fn enabled(self, provider: &str, model: &str) -> bool {
        match self {
            Self::On => true,
            Self::Off => false,
            Self::Auto => {
                let id = format!("{provider}/{model}").to_ascii_lowercase();
                id.contains("gemini")
            }
        }
    }
}

/// A provider processing tier; `none` omits the field entirely.
#[derive(Deserialize, Serialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ServiceTier {
    #[default]
    None,
    Auto,
    Default,
    Flex,
    Scale,
    Priority,
    Ultrafast,
    /// Follow the main agent's per-family tiers (subagent/advisor only).
    Inherit,
}

impl ServiceTier {
    /// The provider wire value, when this tier is sent at all.
    pub fn wire(self) -> Option<&'static str> {
        match self {
            Self::None | Self::Inherit => None,
            Self::Auto => Some("auto"),
            Self::Default => Some("default"),
            Self::Flex => Some("flex"),
            Self::Scale => Some("scale"),
            Self::Priority => Some("priority"),
            Self::Ultrafast => Some("ultrafast"),
        }
    }

    /// The tier a subagent/advisor request applies, given its family.
    pub fn inherit(self, main: ServiceTier) -> ServiceTier {
        if self == Self::Inherit { main } else { self }
    }
}

/// `tier.*`: processing tiers per model family, plus the subagent and advisor overrides.
#[derive(Deserialize, Serialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(default)]
pub struct TierCfg {
    pub openai: ServiceTier,
    pub anthropic: ServiceTier,
    pub google: ServiceTier,
    pub subagent: ServiceTier,
    pub advisor: ServiceTier,
}

/// Which provider family a model belongs to, for `tier.*`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TierFamily {
    OpenAi,
    Anthropic,
    Google,
}

impl TierCfg {
    /// The configured tier for a family, before family-specific validity and endpoint rules.
    pub fn family(self, family: TierFamily) -> ServiceTier {
        match family {
            TierFamily::OpenAi => self.openai,
            TierFamily::Anthropic => self.anthropic,
            TierFamily::Google => self.google,
        }
    }

    /// Applies a subagent/advisor choice over the main agent's per-family tiers.
    pub fn override_family(&mut self, family: TierFamily, choice: ServiceTier) {
        let tier = choice.inherit(self.family(family));
        match family {
            TierFamily::OpenAi => self.openai = tier,
            TierFamily::Anthropic => self.anthropic = tier,
            TierFamily::Google => self.google = tier,
        }
    }
}

/// Resolves the family of a configured model by provider kind, endpoint and model id.
pub fn tier_family(provider: &str, kind: &str, base_url: &str, model: &str) -> Option<TierFamily> {
    let id = model.to_ascii_lowercase();
    let host = reqwest::Url::parse(base_url).ok().and_then(|u| u.host_str().map(str::to_ascii_lowercase));
    if id.contains("gemini") || host.as_deref().is_some_and(|h| h.contains("googleapis.com")) {
        return Some(TierFamily::Google);
    }
    if id.contains("claude") || kind == "anthropic" || host.as_deref().is_some_and(|h| h.contains("anthropic.com")) {
        return Some(TierFamily::Anthropic);
    }
    if kind == "codex" || id.starts_with("gpt-") || id.starts_with("o1") || id.starts_with("o3") || id.starts_with("o4")
        || host.as_deref() == Some("api.openai.com")
        || host.as_deref().is_some_and(|h| h.ends_with("openrouter.ai")) && provider == "openrouter"
    {
        return Some(TierFamily::OpenAi);
    }
    None
}

/// Which failed requests may switch to a `fallback_chains` model.
#[derive(Deserialize, Serialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum FallbackOn {
    #[default]
    Any,
    UsageLimit,
    ExceptUsageLimit,
}

/// When a fallback hands the conversation back to the primary model.
#[derive(Deserialize, Serialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum FallbackRevertPolicy {
    /// Once the failing primary's cooldown (its usage reset, or 5 minutes) has passed.
    #[default]
    CooldownExpiry,
    Never,
}

/// What to do when every signed-in account of the active coding plan is inside the reserve margin.
#[derive(Deserialize, Serialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum UsageReservePolicy {
    /// Ask the user, then switch (or keep the primary when declined).
    #[default]
    Confirm,
    /// Switch without asking.
    Auto,
    /// Refuse the request rather than spend into the reserve.
    FailClosed,
}

#[derive(Deserialize, Serialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct Retry {
    pub enabled: bool,
    pub max_retries: u32,
    pub base_delay_ms: u64,
    pub max_delay_ms: u64,
    /// Also wait past `max_delay_ms` when the provider reports a usage-limit reset time.
    pub wait_for_usage_reset: bool,
    /// `retry.modelFallback`: a failed request may continue on a `fallback_chains` model.
    pub model_fallback: bool,
    /// Which failures may switch to a fallback.
    pub fallback_on: FallbackOn,
    /// On a rate limit, fail fast so the chain is reached instead of spending `max_retries` on the failing model.
    pub rate_limit_walks_chain: bool,
    /// Prefer a fallback before a signed-in coding plan hits its hard limit.
    pub usage_aware_fallback: bool,
    /// Remaining percentage below which a coding-plan model counts as near its limit.
    pub usage_reserve_pct: u8,
    pub usage_reserve_policy: UsageReservePolicy,
    /// Role / model selector / provider wildcard -> ordered fallback selectors.
    pub fallback_chains: std::collections::BTreeMap<String, Vec<String>>,
    pub fallback_revert_policy: FallbackRevertPolicy,
}

impl Default for Retry {
    fn default() -> Self {
        Self {
            enabled: true,
            max_retries: 10,
            base_delay_ms: 500,
            max_delay_ms: 300_000,
            wait_for_usage_reset: false,
            model_fallback: true,
            fallback_on: FallbackOn::Any,
            rate_limit_walks_chain: false,
            usage_aware_fallback: false,
            usage_reserve_pct: 10,
            usage_reserve_policy: UsageReservePolicy::Confirm,
            fallback_chains: Default::default(),
            fallback_revert_policy: FallbackRevertPolicy::CooldownExpiry,
        }
    }
}

impl Retry {
    /// How long to wait after `attempt` failed requests; `None` means give up now.
    /// A usage-limit reset is waited out only when it fits `max_delay_ms`, unless `wait_for_usage_reset`.
    pub fn wait(&self, attempt: u32, usage_reset_secs: Option<u64>) -> Option<std::time::Duration> {
        if !self.enabled || attempt >= self.max_retries {
            return None;
        }
        if let Some(secs) = usage_reset_secs {
            let millis = secs.saturating_mul(1000);
            if !self.wait_for_usage_reset && self.max_delay_ms > 0 && millis > self.max_delay_ms {
                return None;
            }
            return Some(std::time::Duration::from_secs(secs.max(1)));
        }
        let millis = self.base_delay_ms.saturating_mul(1u64 << attempt.min(31));
        let millis = if self.max_delay_ms > 0 { millis.min(self.max_delay_ms) } else { millis };
        Some(std::time::Duration::from_millis(millis))
    }
}

#[derive(Deserialize, Serialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct CostLimits {
    /// Warn once when session spend first reaches this many USD.
    pub warn: Option<f64>,
    /// Refuse the next model request once session spend reaches this many USD.
    pub max: Option<f64>,
}

impl Default for CostLimits {
    fn default() -> Self {
        Self { warn: None, max: None }
    }
}

fn usd(amount: f64) -> String {
    format!("${amount:.prec$}", prec = if amount < 0.01 { 4 } else { 2 })
}

impl CostLimits {
    /// Why the next request must not be sent, when the cap is reached.
    pub fn refusal(&self, spent: f64) -> Option<String> {
        let cap = self.max?;
        (spent >= cap).then(|| format!(
            "Cost cap reached: this session spent {} of its {} cap (cost.max), so the next model request was not sent.",
            usd(spent), usd(cap)
        ))
    }

    /// The one-time warning when spend first reaches the threshold.
    pub fn warning(&self, spent: f64, already_warned: bool) -> Option<String> {
        let warn = self.warn?;
        (!already_warned && spent >= warn).then(|| format!(
            "Cost warning: this session has spent {}, past its {} warning threshold (cost.warn).",
            usd(spent), usd(warn)
        ))
    }
}

/// OMP's per-effort thinking token budgets, in tokens.
#[derive(Deserialize, Serialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(default)]
pub struct ThinkingBudgets {
    pub low: u32,
    pub medium: u32,
    pub high: u32,
    pub xhigh: u32,
    pub max: u32,
}

impl Default for ThinkingBudgets {
    fn default() -> Self {
        Self { low: 2048, medium: 8192, high: 16384, xhigh: 32768, max: 32768 }
    }
}

impl ThinkingBudgets {
    /// The budget for a configured effort level; native has no `minimal` level.
    pub fn budget(&self, level: &str) -> Option<u32> {
        match level {
            "low" => Some(self.low),
            "medium" => Some(self.medium),
            "high" => Some(self.high),
            "xhigh" => Some(self.xhigh),
            "max" => Some(self.max),
            _ => None,
        }
    }
}

/// OMP puts this under `providers`; native keeps provider request behavior in its own table
/// because `providers` is the provider map.
#[derive(Deserialize, Serialize, Clone, Debug, PartialEq, Eq)]
#[serde(default)]
pub struct Anthropic {
    /// Send Anthropic's server-side refusal fallback for the models that accept it.
    pub server_side_fallback: bool,
}

impl Default for Anthropic {
    fn default() -> Self {
        // Native always sent the fallback for five models; keeping that avoids dropping it for existing users.
        Self { server_side_fallback: true }
    }
}

#[derive(Deserialize, Serialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct Web {
    /// Hard timeout for one search request, in seconds (maximum 300).
    pub search_timeout_seconds: u64,
    pub exa: Exa,
    pub searxng: Searxng,
}

impl Default for Web {
    fn default() -> Self {
        Self { search_timeout_seconds: 60, exa: Exa::default(), searxng: Searxng::default() }
    }
}

impl Web {
    /// The search transport timeout, clamped to the documented maximum.
    pub fn search_timeout(&self) -> std::time::Duration {
        std::time::Duration::from_secs(self.search_timeout_seconds.clamp(1, 300))
    }
}

#[derive(Deserialize, Serialize, Clone, Debug, PartialEq, Eq)]
#[serde(default)]
pub struct Exa {
    pub enabled: bool,
    /// Minimum spacing between Exa requests; 0 sends without pacing.
    pub search_delay_ms: u64,
}

impl Default for Exa {
    fn default() -> Self {
        Self { enabled: true, search_delay_ms: 1000 }
    }
}

#[derive(Deserialize, Serialize, Clone, Debug, Default, PartialEq, Eq)]
#[serde(default)]
pub struct Searxng {
    pub endpoint: Option<String>,
    pub token: Option<String>,
    pub basic_username: Option<String>,
    pub basic_password: Option<String>,
    pub categories: Option<String>,
    pub engines: Option<String>,
    pub language: Option<String>,
    pub safesearch: Option<u8>,
}

/// OMP key → native dotted key; `config import-omp` copies the value verbatim unless `convert` handles it.
pub(super) const OMP: &[(&str, &str)] = &[
    ("enabledModels", "enabled_models"),
    ("enabledProviders", "enabled_providers"),
    ("disabledProviders", "disabled_providers"),
    ("modelProviderOrder", "model_provider_order"),
    ("temperature", "sampling.temperature"),
    ("topP", "sampling.top_p"),
    ("topK", "sampling.top_k"),
    ("minP", "sampling.min_p"),
    ("presencePenalty", "sampling.presence_penalty"),
    ("repetitionPenalty", "sampling.repetition_penalty"),
    ("textVerbosity", "text_verbosity"),
    ("retry.enabled", "retry.enabled"),
    ("retry.maxRetries", "retry.max_retries"),
    ("retry.baseDelayMs", "retry.base_delay_ms"),
    ("retry.maxDelayMs", "retry.max_delay_ms"),
    ("retry.waitForUsageReset", "retry.wait_for_usage_reset"),
    ("retry.modelFallback", "retry.model_fallback"),
    ("retry.fallbackOn", "retry.fallback_on"),
    ("retry.rateLimitWalksChain", "retry.rate_limit_walks_chain"),
    ("retry.usageAwareFallback", "retry.usage_aware_fallback"),
    ("retry.usageReservePct", "retry.usage_reserve_pct"),
    ("retry.usageReservePolicy", "retry.usage_reserve_policy"),
    ("retry.fallbackChains", "retry.fallback_chains"),
    ("retry.fallbackRevertPolicy", "retry.fallback_revert_policy"),
    ("providers.maxInFlightRequests", "max_in_flight"),
    ("providers.webSearchTimeoutSeconds", "web.search_timeout_seconds"),
    ("providers.anthropic.serverSideFallback", "anthropic.server_side_fallback"),
    ("providers.fireworksTier", "fireworks_tier"),
    ("providers.openrouterVariant", "openrouter_variant"),
    ("providers.kimiApiFormat", "kimi_api_format"),
    ("providers.openaiWebsockets", "openai_websockets"),
    ("providers.openaiResponsesWebsockets", "openai_responses_websockets"),
    ("providers.openaiLiveSteering", "openai_live_steering"),
    ("providers.cacheRetention", "cache_retention"),
    ("providers.cacheWarming", "cache_warming"),
    ("providers.cacheWarmingStrategy", "cache_warming_strategy"),
    ("providers.cacheWarmingCompactPercent", "cache_warming_compact_percent"),
    ("providers.cacheWarmingCompactTokens", "cache_warming_compact_tokens"),
    ("provider.appendOnlyContext", "append_only_context"),
    ("providers.streamFirstEventTimeoutSeconds", "stream_first_event_timeout_seconds"),
    ("providers.streamIdleTimeoutSeconds", "stream_idle_timeout_seconds"),
    ("defaultThinkingLevel", "effort"),
    ("omitThinking", "omit_thinking"),
    ("externalThinking", "external_thinking"),
    ("providers.autoThinkingMaxEffort", "auto_thinking_max_effort"),
    ("tier.openai", "tier.openai"),
    ("tier.anthropic", "tier.anthropic"),
    ("tier.google", "tier.google"),
    ("tier.subagent", "tier.subagent"),
    ("tier.advisor", "tier.advisor"),
    ("modelRoleStorage", "model_role_storage"),
    ("inlineToolDescriptors", "inline_tool_descriptors"),
    ("skillful", "skillful"),
    ("thinkingBudgets.low", "thinking.low"),
    ("thinkingBudgets.medium", "thinking.medium"),
    ("thinkingBudgets.high", "thinking.high"),
    ("thinkingBudgets.xhigh", "thinking.xhigh"),
    ("thinkingBudgets.max", "thinking.max"),
    ("models.offline", "models_offline"),
    ("session.warnCost", "cost.warn"),
    ("session.maxCost", "cost.max"),
    ("exa.enabled", "web.exa.enabled"),
    ("exa.searchDelayMs", "web.exa.search_delay_ms"),
    ("searxng.endpoint", "web.searxng.endpoint"),
    ("searxng.token", "web.searxng.token"),
    ("searxng.basicUsername", "web.searxng.basic_username"),
    ("searxng.basicPassword", "web.searxng.basic_password"),
    ("searxng.categories", "web.searxng.categories"),
    ("searxng.engines", "web.searxng.engines"),
    ("searxng.language", "web.searxng.language"),
    ("searxng.safesearch", "web.searxng.safesearch"),
];

/// Translates an OMP value whose shape or units differ from the native field; `None` copies it verbatim.
pub(super) fn convert(from: &str, value: &Value) -> Option<Result<toml::Value>> {
    if from == "providers.maxInFlightRequests" {
        return Some((|| {
            let providers = value.as_object().ok_or_else(|| anyhow!("{from} must be a mapping of provider ids"))?;
            let mut limits = toml::Table::new();
            for (provider, limit) in providers {
                let limit = limit.as_u64().ok_or_else(|| anyhow!("{from}.{provider} must be a non-negative integer"))?;
                // OMP treats 0 as unlimited, the native default.
                if limit > 0 {
                    limits.insert(provider.clone(), toml::Value::Integer(limit as i64));
                }
            }
            Ok(toml::Value::Table(limits))
        })());
    }
    let number = matches!(
        from,
        "temperature" | "topP" | "topK" | "minP" | "presencePenalty" | "repetitionPenalty" | "session.warnCost" | "session.maxCost"
    );
    number.then(|| {
        value
            .as_f64()
            .map(toml::Value::Float)
            .ok_or_else(|| anyhow!("{from} must be a number, got {value}"))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn imported_rows_become_native_keys() {
        let yaml = r#"
enabledModels: ["anthropic/*", "openai/gpt-5"]
enabledProviders: [anthropic]
disabledProviders: [groq]
modelProviderOrder: [openai, anthropic]
temperature: 0
topP: 0.9
topK: 40
minP: 0.05
presencePenalty: 1
repetitionPenalty: 1.1
textVerbosity: high
retry:
  enabled: false
  maxRetries: 4
  baseDelayMs: 250
  maxDelayMs: 60000
  waitForUsageReset: true
providers:
  maxInFlightRequests:
    openai: 2
  webSearchTimeoutSeconds: 45
  anthropic:
    serverSideFallback: false
  fireworksTier: priority
  openrouterVariant: nitro
  streamFirstEventTimeoutSeconds: 120
  streamIdleTimeoutSeconds: 90
thinkingBudgets:
  high: 20000
models:
  offline: true
session:
  warnCost: 5
  maxCost: 20
exa:
  enabled: false
  searchDelayMs: 0
searxng:
  endpoint: https://search.example
  token: t0k
  categories: general
  safesearch: 1
"#;
        let cases = [
            ("enabled_models", toml::Value::Array(vec!["anthropic/*".into(), "openai/gpt-5".into()])),
            ("sampling.temperature", toml::Value::Float(0.)),
            ("sampling.repetition_penalty", toml::Value::Float(1.1)),
            ("text_verbosity", toml::Value::String("high".into())),
            ("retry.max_retries", toml::Value::Integer(4)),
            ("retry.base_delay_ms", toml::Value::Integer(250)),
            ("retry.wait_for_usage_reset", toml::Value::Boolean(true)),
            ("max_in_flight.openai", toml::Value::Integer(2)),
            ("web.search_timeout_seconds", toml::Value::Integer(45)),
            ("anthropic.server_side_fallback", toml::Value::Boolean(false)),
            ("fireworks_tier", toml::Value::String("priority".into())),
            ("openrouter_variant", toml::Value::String("nitro".into())),
            ("stream_first_event_timeout_seconds", toml::Value::Integer(120)),
            ("stream_idle_timeout_seconds", toml::Value::Integer(90)),
            ("thinking.high", toml::Value::Integer(20000)),
            ("models_offline", toml::Value::Boolean(true)),
            ("cost.warn", toml::Value::Float(5.)),
            ("cost.max", toml::Value::Float(20.)),
            ("web.exa.enabled", toml::Value::Boolean(false)),
            ("web.exa.search_delay_ms", toml::Value::Integer(0)),
            ("web.searxng.endpoint", toml::Value::String("https://search.example".into())),
            ("web.searxng.safesearch", toml::Value::Integer(1)),
        ];
        let value = toml::Value::Table(crate::config::settings::omp::translate(yaml).unwrap());
        for (key, want) in cases {
            let mut here = value.clone();
            for segment in key.split('.') {
                here = here.get(segment).cloned().unwrap_or_else(|| panic!("{key} missing"));
            }
            assert_eq!(here, want, "{key}");
        }
        let cfg: crate::config::Config = crate::config::settings::decode(value.clone()).unwrap();
        assert_eq!(cfg.provider_opts.retry.max_retries, 4);
        assert_eq!(cfg.provider_opts.web.searxng.endpoint.as_deref(), Some("https://search.example"));
        assert!(!cfg.provider_opts.anthropic.server_side_fallback);
        assert_eq!(cfg.provider_opts.cost.max, Some(20.));
        assert_eq!(cfg.provider_opts.sampling.top_k, 40.);
        assert_eq!(cfg.provider_opts.thinking.high, 20_000);
        assert_eq!(cfg.provider_opts.max_in_flight.get("openai"), Some(&2));
        // Integer-valued YAML numbers still decode into the float settings.
        let cfg: crate::config::Config =
            crate::config::settings::decode(toml::Value::Table(crate::config::settings::omp::translate("temperature: 1\n").unwrap())).unwrap();
        assert_eq!(cfg.provider_opts.sampling.temperature, 1.);
    }

    #[test]
    fn thinking_and_tier_rows_import_and_decode() {
        for (omp, want) in [
            ("defaultThinkingLevel: high", "effort"),
            ("omitThinking: true", "omit_thinking"),
            ("externalThinking: true", "external_thinking"),
            ("providers:\n  autoThinkingMaxEffort: max", "auto_thinking_max_effort"),
            ("tier:\n  openai: priority", "tier.openai"),
            ("tier:\n  anthropic: priority", "tier.anthropic"),
            ("tier:\n  google: flex", "tier.google"),
            ("tier:\n  subagent: inherit", "tier.subagent"),
            ("tier:\n  advisor: none", "tier.advisor"),
            ("modelRoleStorage: project", "model_role_storage"),
            ("inlineToolDescriptors: on", "inline_tool_descriptors"),
            ("skillful: false", "skillful"),
            ("model:\n  loopGuard:\n    toolCallReminder: false", "loop_guard.tool_call_reminder"),
        ] {
            let table = toml::Value::Table(crate::config::settings::omp::translate(omp).unwrap());
            let mut here = table.clone();
            for segment in want.split('.') {
                here = here.get(segment).cloned().unwrap_or_else(|| panic!("{omp}: {want} missing in {table}"));
            }
        }
        let cfg: crate::config::Config = crate::config::settings::decode(toml::Value::Table(
            crate::config::settings::omp::translate(
                "defaultThinkingLevel: high\nomitThinking: true\nexternalThinking: true\nproviders:\n  autoThinkingMaxEffort: max\ntier:\n  openai: priority\n  subagent: inherit\nmodelRoleStorage: project\ninlineToolDescriptors: on\nskillful: false\nmodel:\n  loopGuard:\n    toolCallReminder: false\n",
            )
            .unwrap(),
        ))
        .unwrap();
        let opts = &cfg.provider_opts;
        assert_eq!(cfg.effort.as_deref(), Some("high"));
        assert!(opts.omit_thinking && opts.external_thinking);
        assert_eq!(opts.auto_thinking_max_effort, AutoThinkingMaxEffort::Max);
        assert_eq!(opts.tier.openai, ServiceTier::Priority);
        assert_eq!(opts.tier.subagent, ServiceTier::Inherit);
        assert_eq!(opts.model_role_storage, ModelRoleStorage::Project);
        assert_eq!(opts.inline_tool_descriptors, InlineToolDescriptors::On);
        assert!(!opts.skillful);
        assert!(!cfg.loop_opts.loop_guard.tool_call_reminder);
        // Defaults are OMP's: no tier, global role storage, auto descriptors, skills listed.
        let d = ProviderOpts::default();
        assert_eq!((d.tier, d.model_role_storage), (TierCfg::default(), ModelRoleStorage::Global));
        assert_eq!(d.inline_tool_descriptors, InlineToolDescriptors::Auto);
        assert!(d.skillful && !d.omit_thinking && !d.external_thinking);
        assert_eq!(d.auto_thinking_max_effort, AutoThinkingMaxEffort::Xhigh);
    }

    #[test]
    fn tier_resolution_respects_family_and_endpoint() {
        use crate::config::provider::{tier_family, TierFamily};
        assert_eq!(tier_family("openai", "openai", "https://api.openai.com/v1", "gpt-6"), Some(TierFamily::OpenAi));
        assert_eq!(tier_family("openrouter", "openai", "https://openrouter.ai/api/v1", "google/gemini-3-pro"), Some(TierFamily::Google));
        assert_eq!(tier_family("x", "anthropic", "https://api.anthropic.com", "claude-opus-5-5"), Some(TierFamily::Anthropic));
        assert_eq!(tier_family("x", "openai", "https://api.fireworks.ai/v1", "accounts/fireworks/models/llama"), None);
        let tier = TierCfg { openai: ServiceTier::Priority, anthropic: ServiceTier::Priority, google: ServiceTier::Flex, ..TierCfg::default() };
        let tiers = |p: &str, kind: &str, base: &str, model: &str| crate::provider::tier_for(&tier, p, kind, base, model);
        assert_eq!(tiers("openai", "openai", "https://api.openai.com/v1", "gpt-6"), Some(ServiceTier::Priority));
        assert_eq!(tiers("openai", "openai", "https://proxy.test/v1", "gpt-6"), None);
        // Priority on Anthropic is fast mode, first party only.
        assert_eq!(tiers("anthropic", "anthropic", "https://api.anthropic.com", "claude-opus-5-5"), Some(ServiceTier::Priority));
        assert_eq!(tiers("openrouterm", "anthropic", "https://openrouter.ai/api/v1", "anthropic/claude-opus-5-5"), None);
        assert_eq!(tiers("openrouter", "openai", "https://openrouter.ai/api/v1", "google/gemini-3-pro"), Some(ServiceTier::Flex));
        // A family with no configured tier sends nothing.
        let none = TierCfg::default();
        assert_eq!(crate::provider::tier_for(&none, "openai", "openai", "https://api.openai.com/v1", "gpt-6"), None);
    }

    #[test]
    fn model_and_provider_filters_use_globs_and_keep_current_model_first() {
        let mut cfg = crate::config::Config { model: "anthropic/keep".into(), ..Default::default() };
        for (name, models) in [("zzz", vec!["one"]), ("aaa", vec!["two"]), ("groq", vec!["three"])] {
            cfg.providers.insert(name.into(), crate::config::ProviderCfg {
                kind: "openai".into(),
                base_url: "http://x".into(),
                models: models.into_iter().map(String::from).collect(),
                ..Default::default()
            });
        }
        cfg.models = vec!["extra/bare".into()];
        let list = cfg.model_list();
        assert_eq!(list, ["anthropic/keep", "extra/bare", "aaa/two", "groq/three", "zzz/one"]);
        cfg.provider_opts.enabled_models = vec!["aaa/*".into(), "extra/bare".into()];
        assert_eq!(cfg.model_list(), ["anthropic/keep", "extra/bare", "aaa/two"]);
        cfg.provider_opts.enabled_models.clear();
        cfg.provider_opts.enabled_providers = vec!["anthropic".into(), "zzz".into()];
        assert_eq!(cfg.model_list(), ["anthropic/keep", "zzz/one"]);
        cfg.provider_opts.enabled_providers.clear();
        cfg.provider_opts.disabled_providers = vec!["g*".into()];
        assert_eq!(cfg.model_list(), ["anthropic/keep", "extra/bare", "aaa/two", "zzz/one"]);
        cfg.provider_opts.model_provider_order = vec!["zzz".into(), "aaa".into()];
        assert_eq!(cfg.model_list(), ["anthropic/keep", "extra/bare", "zzz/one", "aaa/two"]);
    }

    #[test]
    fn retry_backoff_is_exponential_and_bounded() {
        let retry = Retry::default();
        assert_eq!(retry.wait(0, None), Some(std::time::Duration::from_millis(500)));
        assert_eq!(retry.wait(1, None), Some(std::time::Duration::from_millis(1000)));
        assert_eq!(retry.wait(3, None), Some(std::time::Duration::from_millis(4000)));
        assert_eq!(Retry { max_retries: 20, ..retry.clone() }.wait(19, None), Some(std::time::Duration::from_millis(300_000)));
        assert_eq!(retry.wait(10, None), None, "max_retries bounds the attempts");
        let capped = Retry { max_retries: 20, max_delay_ms: 1000, ..retry.clone() };
        assert_eq!(capped.wait(10, None), Some(std::time::Duration::from_millis(1000)));
        assert_eq!(capped.wait(0, Some(3600)), None, "a reset past the ceiling fails fast");
        assert_eq!(retry.wait(0, Some(2)), Some(std::time::Duration::from_secs(2)));
        let patient = Retry { wait_for_usage_reset: true, ..retry.clone() };
        assert_eq!(patient.wait(0, Some(3600)), Some(std::time::Duration::from_secs(3600)));
        assert_eq!(Retry { enabled: false, ..retry }.wait(0, None), None);
    }

    #[test]
    fn cost_limits_warn_once_and_refuse_at_the_cap() {
        let limits = CostLimits { warn: Some(5.), max: Some(9.) };
        assert_eq!(limits.warning(4.99, false), None);
        assert!(limits.warning(5., false).unwrap().contains("$5.00"));
        assert_eq!(limits.warning(6., true), None, "warned only once");
        assert!(limits.refusal(8.99).is_none());
        assert!(limits.refusal(9.).unwrap().contains("$9.00"));
        assert!(CostLimits::default().refusal(1e9).is_none());
    }
}
