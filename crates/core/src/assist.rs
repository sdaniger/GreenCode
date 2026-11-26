//! Passive advisor reviews and prewalk model hand-off.
//!
//! The advisor is a second model (the `advisor` role) that reads a digest of the conversation and
//! returns JSON notes; notes either reach the agent mid-turn, wake it after a final answer, or stay
//! as a card for the next prompt. Prewalk starts on the strong model and switches to a cheap one at
//! the first edit/write after the plan's todo list exists.
use crate::{
    config::advisor::{Auto, ReviewMode, Severity, SyncBacklog},
    config::Config,
    provider::Provider,
    Block, Event, EventTx, Message,
};
use anyhow::Result;
use serde_json::{json, Value};
use std::{
    collections::HashSet,
    sync::Arc,
    time::Duration,
};
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};

/// One advisor note.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Note {
    pub severity: Severity,
    pub text: String,
}

const SYSTEM: &str = "You are a passive advisor to another coding agent. Review the conversation digest and reply with one JSON object: \
{\"severity\": \"nit|concern|blocker\", \"text\": \"<one actionable sentence>\"}. Use blocker only when continuing would corrupt work, \
lose data or break the build. Use nit for taste. Reply with {\"severity\": \"none\"} when there is nothing worth saying.";

const PLAN_NUDGE: &str = "<system-reminder>Before editing anything, write the plan as `todo` items with the `todo` tool, then start implementing.</system-reminder>";

/// Characters of the newest conversation a review sees; older context is summarized by the digest prefix.
const DIGEST_TAIL: usize = 24_000;

/// How long a threshold backlog wait blocks the primary agent (strict has no cap).
const BACKLOG_WAIT: Duration = Duration::from_secs(30);

/// The verdict of the final (agent-end) review.
pub enum Verdict {
    /// Wake the agent with this advisory message.
    Wake(String),
    /// Leave these as cards at the end of the transcript.
    Cards(Vec<String>),
    Quiet,
}

/// State carried across turns by one agent.
pub struct Assist {
    /// Advisor reviews in flight.
    pending: usize,
    rx: Option<UnboundedReceiver<Vec<Note>>>,
    tx: Option<UnboundedSender<Vec<Note>>>,
    /// Messages length when the newest review started, for stale-result eviction.
    evict_before: usize,
    /// Primary updates since the last review, and total updates this session.
    updates: u32,
    /// Primary turn number; immune turns are counted from the turn a note interrupted.
    turn: u32,
    /// Turns after `interrupted_on` during which only blockers interrupt.
    immune_until: u32,
    /// Targets prewalk hands off to, armed by the setting or `/prewalk`.
    prewalk: Option<String>,
    /// A provider injected by tests or a harness whose model is not in the provider registry.
    provider: Option<Arc<dyn Provider>>,
    /// A final-answer wake already happened this turn; later notes stay cards, so an agent that
    /// answers again cannot be woken in a loop.
    woke: bool,
    /// The last digest sent, so tests and debugging can inspect it.
    pub last_digest: Option<String>,
}

impl Default for Assist {
    fn default() -> Self {
        Self {
            pending: 0,
            rx: None,
            tx: None,
            evict_before: 0,
            updates: 0,
            turn: 0,
            immune_until: 0,
            prewalk: None,
            provider: None,
            woke: false,
            last_digest: None,
        }
    }
}

impl Assist {
    pub fn new() -> Self {
        Self::default()
    }

    /// Reuse this provider for reviews whose model has no entry in `cfg.providers` (tests, custom hosts).
    pub fn set_provider(&mut self, provider: Arc<dyn Provider>) {
        self.provider = Some(provider);
    }

    /// Arms (`Some`) or clears (`None`) the hand-off target.
    pub fn set_prewalk(&mut self, spec: Option<String>) {
        self.prewalk = spec;
    }
}

/// Parses the advisor's reply: a JSON object or array of notes; plain text becomes one concern.
pub fn parse_notes(raw: &str, max_non_blockers: u32) -> Vec<Note> {
    let text = raw.trim();
    let trimmed = text.trim_start_matches("```json").trim_start_matches("```").trim_end_matches("```").trim();
    let value = serde_json::from_str::<Value>(trimmed).ok();
    let mut notes = Vec::new();
    let push = |v: &Value, notes: &mut Vec<Note>| {
        if v["severity"] == "none" {
            return;
        }
        let text = v["text"].as_str().unwrap_or("").trim();
        if text.is_empty() {
            return;
        }
        notes.push(Note { severity: v["severity"].as_str().and_then(Severity::parse).unwrap_or(Severity::Concern), text: text.to_string() });
    };
    match value {
        Some(Value::Array(items)) => items.iter().for_each(|v| push(v, &mut notes)),
        Some(v @ Value::Object(_)) => push(&v, &mut notes),
        _ if !text.is_empty() && text != "null" => notes.push(Note { severity: Severity::Concern, text: text.to_string() }),
        _ => {}
    }
    let cap = max_non_blockers as usize;
    let mut kept = 0;
    notes.retain(|note| {
        if note.severity == Severity::Blocker {
            return true;
        }
        kept += 1;
        kept <= cap
    });
    notes
}

/// Advisor text as the agent sees it.
pub fn render(note: &Note, aside: bool) -> String {
    if aside {
        format!("<advisory aside=\"true\" severity=\"{}\">{}</advisory>", severity_name(note.severity), note.text)
    } else {
        format!("<advisory severity=\"{}\">{}</advisory>", severity_name(note.severity), note.text)
    }
}

fn severity_name(severity: Severity) -> &'static str {
    match severity {
        Severity::Nit => "nit",
        Severity::Concern => "concern",
        Severity::Blocker => "blocker",
    }
}

/// Whether the Nth primary update is due for a review.
pub fn due(updates: u32, interval: u32) -> bool {
    interval <= 1 || updates % interval == 0
}

/// A digest of the conversation; with `evict`, read/grep/glob results from before `evict_before`
/// become a placeholder so the review sees current output rather than every historical dump.
pub fn digest(messages: &[Message], evict_before: usize, evict: bool) -> String {
    let stale: HashSet<&str> = if evict {
        messages
            .iter()
            .take(evict_before)
            .flat_map(|m| &m.content)
            .filter_map(|b| match b {
                Block::ToolUse { id, name, .. } if matches!(name.as_str(), "read" | "grep" | "glob") => Some(id.as_str()),
                _ => None,
            })
            .collect()
    } else {
        HashSet::new()
    };
    let mut out = String::new();
    for m in messages {
        if stale.is_empty() {
            out.push_str(&crate::snap::serialize(std::slice::from_ref(m)));
        } else {
            let mut m = m.clone();
            for block in &mut m.content {
                if let Block::ToolResult { tool_use_id, content, is_error: false } = block {
                    if stale.contains(tool_use_id.as_str()) {
                        *content = "[output evicted from an older review]".into();
                    }
                }
            }
            out.push_str(&crate::snap::serialize(std::slice::from_ref(&m)));
        }
        out.push('\n');
    }
    let start = out.len().saturating_sub(DIGEST_TAIL);
    if start > 0 {
        let start = out.char_indices().map(|(i, _)| i).find(|&i| i >= start).unwrap_or(out.len());
        out.drain(..start);
    }
    out
}

/// The `provider/model` a role or selector resolves to; `@default` is the configured model.
pub fn resolve_target(cfg: &Config, spec: &str) -> Option<String> {
    let spec = spec.trim();
    if spec.is_empty() {
        return None;
    }
    if let Some(role) = spec.strip_prefix('@') {
        return match role {
            "default" => Some(cfg.model.clone()),
            // Like OMP, an unconfigured role resolves to nothing: prewalk stays unarmed and says why.
            role => cfg.model_roles.get(role).cloned(),
        };
    }
    if spec.contains('/') {
        return Some(spec.to_string());
    }
    cfg.model_roles.get(spec).cloned()
}

/// The model the advisor runs on: the `advisor` role, then `advisor_model`.
pub fn advisor_spec(cfg: &Config) -> Option<String> {
    cfg.model_roles.get("advisor").cloned().or_else(|| cfg.advisor_model.clone())
}

/// The `judge` role, when one is configured.
pub fn judge_spec(cfg: &Config) -> Option<String> {
    cfg.model_roles.get("judge").cloned()
}

/// One non-streaming completion for judgement calls; `None` when the call fails, so callers fail open.
async fn judge(cfg: &Config, system: &str, prompt: &str, max_tokens: u32) -> Option<String> {
    let spec = judge_spec(cfg)?;
    crate::textgen::generate(cfg, &spec, system, prompt, max_tokens).await.ok()
}

/// Whether the task front-loads its hard reasoning, so a cheaper model can finish it.
pub async fn plan_first(cfg: &Config, task: &str) -> Option<bool> {
    let prompt = format!(
        "Task:\n{}\n\nDoes this task front-load its hard reasoning (design, debugging, architecture) before the edits begin, \
so a strong model should plan and a cheap model can finish it? Reply with one word: yes or no.",
        task.chars().take(4000).collect::<String>()
    );
    let answer = judge(cfg, "Classify the task; reply with yes or no only.", &prompt, 4).await?;
    Some(answer.trim().to_ascii_lowercase().starts_with('y'))
}

/// The parts of a prompt that can run concurrently, as one line each.
pub async fn parallel_parts(cfg: &Config, task: &str) -> Option<Vec<String>> {
    let prompt = format!(
        "Prompt:\n{}\n\nList the parts of this prompt that could be done concurrently by separate workers, one per line, \
each under 12 words. Reply `none` when the work is sequential.",
        task.chars().take(4000).collect::<String>()
    );
    let answer = judge(cfg, "Split work into independent parts; reply with lines only.", &prompt, 200).await?;
    let parts: Vec<String> = answer
        .lines()
        .map(|l| l.trim().trim_start_matches(['-', '*', '1', '.', ' ']).trim().to_string())
        .filter(|l| !l.is_empty() && !l.eq_ignore_ascii_case("none"))
        .take(8)
        .collect();
    (parts.len() > 1).then_some(parts)
}

/// Applies a new top-level prompt: injects the prewalk plan nudge, arms auto-prewalk, and asks the
/// judge for parallel parts. Judgement failures are silent: without a judge nothing is classified.
pub async fn open(rt: &mut Assist, cfg: &Config, message: &mut Message, top: bool, tx: &EventTx) {
    rt.turn += 1;
    rt.woke = false;
    if let Some(Block::Text { text }) = message.content.first_mut() {
        let prompt = text.trim().to_string();
        let mut extra = String::new();
        if top && cfg.assist_opts.prewalk.enabled && rt.prewalk.is_none() {
            rt.prewalk = resolve_target(cfg, &cfg.assist_opts.prewalk.into);
            if rt.prewalk.is_none() {
                let _ = tx.send(Event::Notify(format!("prewalk not armed: \"{}\" names no known model", cfg.assist_opts.prewalk.into)));
            }
        }
        if top && !cfg.assist_opts.prewalk.enabled && cfg.assist_opts.prewalk.auto != Auto::Off && rt.prewalk.is_none() {
            if plan_first(cfg, &prompt).await == Some(true) {
                if cfg.assist_opts.prewalk.auto == Auto::On {
                    rt.prewalk = resolve_target(cfg, &cfg.assist_opts.prewalk.auto_target);
                    let _ = tx.send(Event::Notify(format!("auto-prewalk: will hand off to {}", rt.prewalk.as_deref().unwrap_or("?"))));
                } else {
                    extra.push_str("\n\n<system-reminder>This task front-loads its hard reasoning; consider `/prewalk` to plan here and finish on a cheaper model.</system-reminder>");
                }
            }
        }
        if rt.prewalk.is_some() {
            extra.push_str("\n\n");
            extra.push_str(PLAN_NUDGE);
        }
        if top && cfg.task.auto_parallel != Auto::Off {
            if let Some(parts) = parallel_parts(cfg, &prompt).await {
                let heads: Vec<String> = parts.iter().map(|p| format!("- {p}")).collect();
                let verb = if cfg.task.auto_parallel == Auto::On { "Spawn them" } else { "Consider spawning them" };
                extra.push_str(&format!(
                    "\n\n<system-reminder>These parts can run concurrently:\n{}\n{verb} as `task` calls in one batch.</system-reminder>",
                    heads.join("\n")
                ));
            }
        }
        if !extra.is_empty() {
            text.push_str(&extra);
        }
    }
}

/// Starts a review when this update is due (`advisor.reviewMode = turn`); returns whether one started.
pub fn review_now(rt: &mut Assist, cfg: &Arc<Config>, messages: &[Message], tx: &EventTx) -> bool {
    let advisor = &cfg.assist_opts.advisor;
    rt.updates += 1;
    if !advisor.enabled || advisor.review_mode != ReviewMode::Turn || !due(rt.updates, advisor.review_interval) {
        return false;
    }
    let Some(spec) = advisor_spec(cfg) else { return false };
    rt.evict_before = messages.len();
    let body = digest(messages, rt.evict_before, advisor.evict_stale_results);
    rt.last_digest = Some(body.clone());
    start(rt, cfg.clone(), spec, body, tx);
    true
}

fn start(rt: &mut Assist, cfg: Arc<Config>, spec: String, body: String, tx: &EventTx) {
    if rt.tx.is_none() {
        let (send, recv) = mpsc::unbounded_channel();
        rt.tx = Some(send);
        rt.rx = Some(recv);
    }
    let send = rt.tx.as_ref().expect("channel just created").clone();
    rt.pending += 1;
    let provider = rt.provider.clone();
    let max_notes = cfg.assist_opts.advisor.max_notes_per_update;
    let events = tx.clone();
    tokio::spawn(async move {
        let result = complete(&cfg, provider, &spec, SYSTEM, &body, 400).await;
        let notes = match result {
            Ok(text) => {
                let notes = parse_notes(&text, max_notes);
                if let Some(note) = notes.first() {
                    let _ = events.send(Event::Notify(format!(
                        "advisor ({}): {}",
                        severity_name(note.severity),
                        note.text
                    )));
                }
                notes
            }
            Err(e) => {
                let _ = events.send(Event::Notify(format!("advisor review failed: {e:#}")));
                Vec::new()
            }
        };
        let _ = send.send(notes);
    });
}

async fn complete(
    cfg: &Config,
    provider: Option<Arc<dyn Provider>>,
    spec: &str,
    system: &str,
    prompt: &str,
    max_tokens: u32,
) -> Result<String> {
    let (pname, model) = spec
        .split_once('/')
        .ok_or_else(|| anyhow::anyhow!("`{spec}` must be `provider/model`"))?;
    if !cfg.providers.contains_key(pname) {
        if let Some(provider) = provider {
            let messages = [Message::user(prompt)];
            let req = crate::provider::Request { model, system, messages: &messages, tools: &[], max_tokens, live: None };
            let reply = tokio::time::timeout(Duration::from_secs(45), provider.complete(req, &|_| {}))
                .await
                .map_err(|_| anyhow::anyhow!("the advisor call timed out"))??;
            let text: String = reply
                .blocks
                .iter()
                .filter_map(|b| if let Block::Text { text } = b { Some(text.as_str()) } else { None })
                .collect();
            anyhow::ensure!(!text.trim().is_empty(), "the advisor returned no text");
            return Ok(text);
        }
    }
    crate::textgen::generate(cfg, spec, system, prompt, max_tokens).await
}

/// Drains finished reviews, waiting for the backlog when configured; returns the notes.
async fn collect(rt: &mut Assist, backlog: SyncBacklog) -> Vec<Note> {
    let Some(rx) = rt.rx.as_mut() else { return Vec::new() };
    let mut notes = Vec::new();
    loop {
        while let Ok(batch) = rx.try_recv() {
            rt.pending = rt.pending.saturating_sub(1);
            notes.extend(batch);
        }
        let Some(threshold) = backlog.threshold().filter(|&t| rt.pending >= t as usize) else { break };
        let wait = async {
            loop {
                match rx.recv().await {
                    Some(batch) => {
                        rt.pending = rt.pending.saturating_sub(1);
                        notes.extend(batch);
                        if rt.pending < threshold as usize {
                            break;
                        }
                    }
                    None => break,
                }
            }
        };
        // `strict` waits for every scheduled review; a threshold caps the pause at 30 seconds.
        let waited = if backlog == SyncBacklog::Strict {
            wait.await;
            true
        } else {
            tokio::time::timeout(BACKLOG_WAIT, wait).await.is_ok()
        };
        if !waited {
            break;
        }
        if rt.pending < threshold as usize {
            break;
        }
    }
    notes
}

/// Waits out a configured backlog and returns the notes to inject before the next request.
pub async fn gate(rt: &mut Assist, cfg: &Arc<Config>, tx: &EventTx) -> Option<String> {
    let advisor = &cfg.assist_opts.advisor;
    if !advisor.enabled {
        return None;
    }
    let notes = collect(rt, advisor.sync_backlog).await;
    if notes.is_empty() {
        return None;
    }
    // Blockers lead, so a truncated prompt still shows the important one.
    let mut sorted = notes;
    sorted.sort_by(|a, b| b.severity.cmp(&a.severity));
    if sorted.iter().any(|n| n.severity == Severity::Blocker) {
        rt.immune_until = rt.turn + cfg.assist_opts.advisor.immune_turns;
    }
    let text = sorted.iter().map(|n| render(n, true)).collect::<Vec<_>>().join("\n");
    let _ = tx.send(Event::Notify(format!("advisor: {} note(s) injected", sorted.len())));
    Some(text)
}

/// Runs the final (agent-end) review and decides whether it wakes the agent.
pub async fn settle_final(rt: &mut Assist, cfg: &Arc<Config>, messages: &[Message], tx: &EventTx) -> Verdict {
    let advisor = &cfg.assist_opts.advisor;
    if !advisor.enabled {
        return Verdict::Quiet;
    }
    // Agent-end reviews every final yield; turn mode reviews the final update like any other.
    let run = !rt.woke
        && (advisor.review_mode == ReviewMode::AgentEnd || due(rt.updates + 1, advisor.review_interval));
    if run {
        if let Some(spec) = advisor_spec(cfg) {
            rt.updates += 1;
            rt.evict_before = messages.len();
            let body = digest(messages, rt.evict_before, advisor.evict_stale_results);
            rt.last_digest = Some(body.clone());
            start(rt, cfg.clone(), spec, body, tx);
        }
    }
    let notes = collect(rt, SyncBacklog::Strict).await;
    if notes.is_empty() {
        return Verdict::Quiet;
    }
    let immune = rt.turn < rt.immune_until || rt.woke;
    let (wake, cards): (Vec<Note>, Vec<Note>) = notes.into_iter().partition(|n| {
        n.severity == Severity::Blocker || (n.severity >= advisor.continue_severity && !immune)
    });
    if wake.is_empty() {
        return Verdict::Cards(cards.iter().map(|n| render(n, true)).collect());
    }
    rt.immune_until = rt.turn + advisor.immune_turns;
    rt.woke = true;
    let mut text = wake.iter().map(|n| render(n, false)).collect::<Vec<_>>().join("\n");
    if !cards.is_empty() {
        text.push('\n');
        text.push_str(&cards.iter().map(|n| render(n, true)).collect::<Vec<_>>().join("\n"));
    }
    let _ = tx.send(Event::Notify("advisor: the agent was woken to handle a note".into()));
    Verdict::Wake(text)
}

/// Arms prewalk for a sub-agent: its `prewalk` frontmatter wins, then `task.prewalk` for the generic agent.
pub fn arm_subagent(rt: &mut Assist, cfg: &Config, def: &crate::subagents::AgentDef) {
    if cfg.no_tools {
        return;
    }
    match def.prewalk.as_deref() {
        Some("off") => return,
        Some(spec) => rt.prewalk = resolve_target(cfg, spec),
        None if def.name == "task" && cfg.task.prewalk => {
            rt.prewalk = resolve_target(cfg, &cfg.assist_opts.prewalk.into);
        }
        None => {}
    }
}

/// Consumes the armed prewalk target once `wrote` after the plan's todo list exists; returns the target.
pub fn prewalk_take(rt: &mut Assist, messages: &[Message], wrote: bool) -> Option<String> {
    if !wrote {
        return None;
    }
    let planned = messages.iter().any(|m| {
        m.content.iter().any(|b| matches!(b, Block::ToolUse { name, .. } if name == "todo"))
    });
    if !planned {
        return None;
    }
    rt.prewalk.take()
}

/// `prewalk` frontmatter on a user agent: `off`, a model/role, or unset.
pub fn prewalk_frontmatter(value: &str) -> Option<String> {
    let value = value.trim().trim_matches(['\'', '"']);
    (!value.is_empty() && value != "off").then(|| value.to_string())
}

/// The note a switch leaves in the transcript, and the model picker event surfaces the new model.
pub fn switched_event(spec: &str) -> (String, Value) {
    (
        format!("prewalk: switched to {spec} at the first edit; continue the implementation on this model."),
        json!({"model": spec}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{Reply, Request};
    use crate::BoxFut;
    use std::sync::{atomic::{AtomicUsize, Ordering}, Mutex};

    struct Mock {
        reply: String,
        calls: AtomicUsize,
        systems: Mutex<Vec<String>>,
    }

    impl Provider for Mock {
        fn complete<'a>(&'a self, _: Request<'a>, _: &'a (dyn Fn(&str) + Send + Sync)) -> BoxFut<'a, Result<Reply>> {
            Box::pin(async move {
                self.calls.fetch_add(1, Ordering::SeqCst);
                Ok(Reply { blocks: vec![Block::Text { text: self.reply.clone() }], ..Default::default() })
            })
        }
    }

    fn mock(reply: &str) -> Arc<Mock> {
        Arc::new(Mock { reply: reply.into(), calls: AtomicUsize::new(0), systems: Mutex::new(Vec::new()) })
    }

    fn cfg() -> Arc<Config> {
        let mut cfg = Config::default();
        cfg.assist_opts.advisor.enabled = true;
        cfg.model_roles.insert("advisor".into(), "mock/advisor".into());
        cfg.model_roles.insert("judge".into(), "mock/judge".into());
        cfg.model_roles.insert("smol".into(), "mock/small".into());
        cfg.assist_opts.advisor.review_interval = 2;
        Arc::new(cfg)
    }

    fn user(text: &str) -> Message {
        Message::user(text)
    }

    #[test]
    fn parses_and_caps_notes() {
        let raw = r#"{"severity":"blocker","text":"wrong file"}"#;
        assert_eq!(parse_notes(raw, 1), [Note { severity: Severity::Blocker, text: "wrong file".into() }]);
        let raw = r#"[{"severity":"nit","text":"a"},{"severity":"concern","text":"b"},{"severity":"blocker","text":"c"}]"#;
        let notes = parse_notes(raw, 1);
        assert_eq!(notes.len(), 2, "{notes:?}");
        assert_eq!(notes[1].severity, Severity::Blocker);
        assert!(parse_notes(r#"{"severity":"none"}"#, 3).is_empty());
        assert_eq!(parse_notes("plain advice", 3)[0].severity, Severity::Concern);
    }

    #[test]
    fn evicts_only_old_read_output() {
        let messages = vec![
            Message { role: crate::Role::Assistant, content: vec![Block::ToolUse { id: "r1".into(), name: "read".into(), input: json!({"path": "a"}) }] },
            Message { role: crate::Role::User, content: vec![Block::ToolResult { tool_use_id: "r1".into(), content: "OLD".into(), is_error: false }] },
            Message { role: crate::Role::Assistant, content: vec![Block::ToolUse { id: "r2".into(), name: "read".into(), input: json!({"path": "b"}) }] },
            Message { role: crate::Role::User, content: vec![Block::ToolResult { tool_use_id: "r2".into(), content: "NEW".into(), is_error: false }] },
        ];
        let body = digest(&messages, 2, true);
        assert!(body.contains("evicted from an older review") && !body.contains("OLD"));
        assert!(body.contains("NEW"));
        assert!(digest(&messages, 2, false).contains("OLD"));
    }

    #[tokio::test]
    async fn reviews_on_cadence_and_wakes_after_final() {
        // `strict` makes the next gate wait for the scheduled review instead of racing it.
        let mut cfg = (*cfg()).clone();
        cfg.assist_opts.advisor.sync_backlog = crate::config::advisor::SyncBacklog::Strict;
        let cfg = Arc::new(cfg);
        let provider = mock(r#"{"severity":"concern","text":"add a test"}"#);
        let mut rt = Assist::new();
        rt.set_provider(provider.clone());
        let (tx, _rx) = mpsc::unbounded_channel();
        let messages = vec![user("do it")];
        assert!(due(2, 2) && !due(1, 2));
        assert!(!review_now(&mut rt, &cfg, &messages, &tx), "update 1 of 2 is not due");
        assert_eq!(provider.calls.load(Ordering::SeqCst), 0);
        assert!(review_now(&mut rt, &cfg, &messages, &tx));
        let note = gate(&mut rt, &cfg, &tx).await.expect("a note");
        assert!(note.contains(r#"aside="true""#) && note.contains("add a test"), "{note}");
        assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
        // As in OMP the final answer follows the cadence: update 3 is skipped, so the final is update 4.
        assert!(!review_now(&mut rt, &cfg, &messages, &tx));
        match settle_final(&mut rt, &cfg, &messages, &tx).await {
            Verdict::Wake(text) => assert!(text.contains(r#"severity="concern""#)),
            Verdict::Cards(_) | Verdict::Quiet => panic!("a concern at the final answer must wake the agent"),
        }
    }

    #[tokio::test]
    async fn immune_turns_keep_concerns_aside_but_blockers_wake() {
        let mut cfg = (*cfg()).clone();
        cfg.assist_opts.advisor.continue_severity = Severity::Concern;
        cfg.assist_opts.advisor.immune_turns = 3;
        cfg.assist_opts.advisor.review_mode = ReviewMode::AgentEnd;
        for (reply, wake) in [(r#"{"severity":"concern","text":"c"}"#, false), (r#"{"severity":"blocker","text":"b"}"#, true)] {
            let cfg = Arc::new(cfg.clone());
            let mut rt = Assist::new();
            rt.set_provider(mock(reply));
            rt.turn = 5;
            rt.immune_until = 8;
            let (tx, _rx) = mpsc::unbounded_channel();
            let verdict = settle_final(&mut rt, &cfg, &[user("x")], &tx).await;
            assert_eq!(matches!(verdict, Verdict::Wake(_)), wake, "{reply}");
        }
    }

    #[test]
    fn prewalk_waits_for_a_write_and_the_todo_list() {
        let mut cfg = Config {
            assist_opts: crate::config::advisor::AdvisorOpts {
                prewalk: crate::config::advisor::PrewalkCfg { enabled: true, into: "@smol".into(), ..Default::default() },
                ..Default::default()
            },
            ..Default::default()
        };
        assert_eq!(resolve_target(&cfg, &cfg.assist_opts.prewalk.into), None, "an unresolvable role stays unarmed");
        cfg.model_roles.insert("smol".into(), "mock/small".into());
        let mut rt = Assist::new();
        rt.prewalk = resolve_target(&cfg, &cfg.assist_opts.prewalk.into);
        assert_eq!(rt.prewalk.as_deref(), Some("mock/small"));
        let todo = Message { role: crate::Role::Assistant, content: vec![Block::ToolUse { id: "t".into(), name: "todo".into(), input: json!({"todos": []}) }] };
        let planned = std::slice::from_ref(&todo);
        assert_eq!(prewalk_take(&mut rt, &[user("x")], true), None, "no todo list yet");
        assert_eq!(prewalk_take(&mut rt, planned, false), None, "no write yet");
        assert_eq!(prewalk_take(&mut rt, planned, true).as_deref(), Some("mock/small"));
        assert!(prewalk_take(&mut rt, planned, true).is_none());
        let mut cfg2 = Config::default();
        cfg2.model_roles.insert("smol".into(), "mock/small".into());
        assert_eq!(resolve_target(&cfg2, "@smol").as_deref(), Some("mock/small"));
        assert_eq!(resolve_target(&cfg2, "@default"), Some(cfg2.model.clone()));
    }
}
