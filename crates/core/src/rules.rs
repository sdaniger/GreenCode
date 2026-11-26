//! Rules injected only when the model's own output matches a pattern ("time-travelling" rules): they cost
//! no prompt bytes until they apply. A rule is a markdown file with `trigger:` in its frontmatter.
//!
//! Frontmatter (`---` block): `trigger:` or `condition:` (a regex or a list), `question:` (judged rules),
//! `scope:` (`text`, `thinking`, `tool`, `tool:NAME`; default `text, tool`), `interruptMode:`.
use crate::config::ttsr::{InterruptMode, Judge, RepeatMode, TtsrCfg};
use crate::Block;
use regex_lite::Regex;
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Mutex;

#[derive(Clone)]
pub struct Rule {
    pub name: String,
    pub triggers: Vec<Regex>,
    /// A judged rule has no trigger: the judge model decides whether the rule applies.
    pub question: Option<String>,
    pub body: String,
    /// Frontmatter override for `ttsr.interrupt_mode`.
    pub interrupt: Option<InterruptMode>,
    pub scope: Scope,
}

#[derive(Default, Clone, Debug, PartialEq)]
pub struct Scope {
    pub text: bool,
    pub thinking: bool,
    /// `Some` when tools are in scope; an empty list means every tool.
    pub tools: Option<Vec<String>>,
}

impl Scope {
    fn allows_tool(&self, tool: &str) -> bool {
        match &self.tools {
            None => false,
            Some(names) => names.is_empty() || names.iter().any(|n| n == tool),
        }
    }
}

fn parse_scope(text: &str) -> Scope {
    let mut scope = Scope::default();
    for token in text.split(',').map(str::trim).filter(|t| !t.is_empty()) {
        let lower = token.to_ascii_lowercase();
        match lower.as_str() {
            "text" | "reply" => scope.text = true,
            "thinking" | "reasoning" => scope.thinking = true,
            "tool" | "toolcall" | "tool-call" => {
                scope.tools.get_or_insert_with(Vec::new);
            }
            _ => match lower.strip_prefix("tool:") {
                Some(name) if !name.is_empty() => scope.tools.get_or_insert_with(Vec::new).push(name.to_string()),
                _ => {}
            },
        }
    }
    scope
}

/// Reads `trigger:`/`condition:` as one pattern, a `[a, b]` list, or indented `- pattern` lines.
fn parse_triggers(front: &str) -> Vec<Regex> {
    let mut patterns: Vec<String> = Vec::new();
    let lines: Vec<&str> = front.lines().collect();
    for (i, line) in lines.iter().enumerate() {
        let trimmed = line.trim_start();
        let Some((key, value)) = trimmed.split_once(':') else { continue };
        if !matches!(key.trim(), "trigger" | "condition") {
            continue;
        }
        let value = value.trim();
        if let Some(list) = value.strip_prefix('[') {
            patterns.extend(list.trim_end_matches(']').split(',').map(|p| p.trim().trim_matches(['"', '\'']).to_string()));
        } else if !value.is_empty() {
            patterns.push(value.trim_matches(['"', '\'']).to_string());
        } else {
            for item in lines[i + 1..].iter() {
                let Some(rest) = item.trim_start().strip_prefix('-') else { break };
                patterns.push(rest.trim().trim_matches(['"', '\'']).to_string());
            }
        }
    }
    patterns.iter().filter(|p| !p.trim().is_empty()).filter_map(|p| Regex::new(p).ok()).collect()
}

fn parse(text: &str, stem: &str) -> Option<Rule> {
    let rest = text.strip_prefix("---")?.trim_start_matches(['\r', '\n']);
    let (front, body) = rest.split_once("\n---")?;
    let field = |key: &str| {
        front.lines().find_map(|l| l.strip_prefix(key)?.trim().strip_prefix(':').map(|v| v.trim().trim_matches(['"', '\'']).to_string()))
    };
    let triggers = parse_triggers(front);
    let question = field("question").filter(|q| !q.is_empty());
    if triggers.is_empty() && question.is_none() {
        return None;
    }
    let scope = match field("scope") {
        Some(scope) => parse_scope(&scope),
        None => Scope { text: true, thinking: false, tools: Some(Vec::new()) },
    };
    let interrupt = field("interruptMode").or_else(|| field("interrupt")).and_then(|m| InterruptMode::parse(&m));
    let body = body.trim_start_matches(|c| c != '\n').trim().to_string();
    Some(Rule {
        name: field("name").filter(|n| !n.is_empty()).unwrap_or_else(|| stem.to_string()),
        triggers,
        question,
        body,
        interrupt,
        scope,
    })
}

/// Rules shipped with GreenCode; they warn after the reply instead of cancelling a stream.
const BUILTIN: &[(&str, &str, &str)] = &[
    (
        "no-sycophancy",
        r"(?i)\b(great question|excellent question|you'?re absolutely right|what a (?:great|good) (?:question|idea))\b",
        "Skip praise and agreement: answer the question or continue the work. Verify a claim before calling it right.",
    ),
    (
        "no-ai-disclaimers",
        r"(?i)\b(as an AI|I'?m (?:just )?a language model|my (?:knowledge )?(?:cutoff|training data))\b",
        "Do not explain yourself as a model. Read the repository, check the environment, and act on what you find.",
    ),
    (
        "no-placeholder-code",
        r"\b(TODO|FIXME|unimplemented!\(\)|todo!\(\))\b",
        "Implement the behavior now instead of leaving a placeholder. If it truly cannot be done, say what blocks it.",
    ),
    (
        "no-unverified-claims",
        r"(?i)\b(all tests pass|everything works|tested and works)\b",
        "Only state results you actually observed in this session: name the command and its outcome, or say it is untested.",
    ),
];

impl Rule {
    fn hits(&self, text: &str) -> bool {
        self.triggers.iter().any(|t| t.is_match(text))
    }
}

/// The default rules, parsed from the same format as user rules.
pub fn builtins() -> Vec<Rule> {
    BUILTIN
        .iter()
        .filter_map(|(name, trigger, body)| parse(&format!("---\ntrigger: {trigger}\ninterruptMode: never\n---\n{body}\n"), name))
        .collect()
}

/// `~/.greencode/rules/*.md`, then `<project>/.greencode/rules/*.md`; a project rule replaces a global one,
/// and `builtin_rules` adds the shipped ones (a user rule of the same name wins). `disabled_rules` drops names.
pub fn load(cwd: &Path, cfg: &TtsrCfg) -> Vec<Rule> {
    let mut rules: Vec<Rule> = if cfg.builtin_rules { builtins() } else { Vec::new() };
    for dir in [crate::home().join("rules"), cwd.join(".greencode/rules")] {
        let mut files: Vec<_> = std::fs::read_dir(dir)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|e| e == "md"))
            .collect();
        files.sort();
        for path in files {
            let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("rule");
            if let Some(rule) = std::fs::read_to_string(&path).ok().and_then(|t| parse(&t, stem)) {
                rules.retain(|r| r.name != rule.name);
                rules.push(rule);
            }
        }
    }
    let disabled: HashSet<&str> = cfg.disabled_rules.iter().map(|n| n.trim()).filter(|n| !n.is_empty()).collect();
    rules.retain(|r| !disabled.contains(r.name.as_str()));
    rules
}

/// A rule match the agent loop must act on.
pub struct Matches {
    /// Rules that fired, in match order.
    pub names: Vec<String>,
    /// Warning text to queue as the next user message.
    pub warnings: Vec<String>,
    /// Tool use id -> warning, for calls whose arguments matched and that must not run.
    pub blocked: HashMap<String, String>,
}

/// The judge's prompt: one line per rule, judged only from the reply shown.
pub const JUDGE_SYSTEM: &str = "You judge whether a coding-agent rule applies to another model's output. \
Judge only what the output itself says, not what the task asked for. \
Answer with one line per rule, exactly `NAME: yes` or `NAME: no`.";

/// Replies are cut here (characters) before the judge sees them, so a long turn stays one cheap request.
const JUDGE_LIMIT: usize = 32_000;

pub fn judge_prompt(content: &str, candidates: &[(String, String)]) -> String {
    let content: String = content.chars().take(JUDGE_LIMIT).collect();
    let rules: Vec<String> = candidates.iter().map(|(name, question)| format!("{name}: {question}")).collect();
    format!("Output:\n{content}\n\nRules:\n{}", rules.join("\n"))
}

/// Names answered `yes`, matched case-insensitively; unknown names and `no` are ignored.
pub fn parse_verdicts(text: &str, candidates: &[(String, String)]) -> Vec<String> {
    let mut yes = Vec::new();
    for line in text.lines() {
        let Some((name, verdict)) = line.split_once(':') else { continue };
        let name = name.trim().trim_matches(['-', '*', '`', ' ']);
        if verdict.trim().trim_matches(['*', '`', '.']).eq_ignore_ascii_case("yes")
            && !yes.iter().any(|n: &String| n.as_str() == name)
            && candidates.iter().any(|(n, _)| n == name)
        {
            yes.push(name.to_string());
        }
    }
    yes
}

/// Asks `model` which `question` rules apply to a completed reply; a failure is the caller's to skip.
pub async fn judge(model: &str, cfg: &crate::config::Config, content: &str, candidates: &[(String, String)]) -> anyhow::Result<Vec<String>> {
    let reply = crate::textgen::generate(cfg, model, JUDGE_SYSTEM, &judge_prompt(content, candidates), 200).await?;
    Ok(parse_verdicts(&reply, candidates))
}

/// Stream buffer for the reply currently being generated.
#[derive(Default)]
struct Stream {
    text: String,
    scanned: usize,
}

/// Time-Traveling Stream Rules runtime, shared by the request callback and the agent loop.
pub struct Ttsr {
    cfg: TtsrCfg,
    rules: Vec<Rule>,
    /// `provider/model` of the judge, when `ttsr.judge` makes one available.
    judge: Option<String>,
    turn: Mutex<u64>,
    /// Rule name -> turn it was injected on.
    injected: Mutex<HashMap<String, u64>>,
    stream: Mutex<Stream>,
    /// Rule names matched by the stream that have not been delivered yet.
    hits: Mutex<Vec<String>>,
    /// A stream match cancelled the in-flight request; `request` waits on `notify` beside the provider.
    hit: std::sync::atomic::AtomicBool,
    notify: tokio::sync::Notify,
    /// Mid-stream interrupts this turn; a looping model must not resend forever.
    interrupts: Mutex<u32>,
}

impl Ttsr {
    pub fn new(cfg: TtsrCfg, rules: Vec<Rule>, judge: Option<String>) -> Ttsr {
        Ttsr {
            cfg,
            rules,
            judge,
            turn: Mutex::new(0),
            injected: Mutex::new(HashMap::new()),
            stream: Mutex::new(Stream::default()),
            hits: Mutex::new(Vec::new()),
            hit: Default::default(),
            notify: Default::default(),
            interrupts: Mutex::new(0),
        }
    }

    /// A fresh runtime with the same rules and settings but its own turn and injection state.
    pub fn fork(&self) -> Ttsr {
        Ttsr::new(self.cfg.clone(), self.rules.clone(), self.judge.clone())
    }

    pub fn has_rules(&self) -> bool {
        !self.rules.is_empty()
    }

    pub fn judge(&self) -> Option<&str> {
        self.judge.as_deref()
    }

    /// Whether a streamed reply can be cancelled: `ttsr.enabled` and at least one rule that may interrupt prose.
    pub fn interrupting(&self) -> bool {
        self.cfg.enabled && self.rules.iter().any(|rule| self.interrupts_prose(rule))
    }

    /// Counts one user prompt, re-arms the per-turn interrupt budget and drops a stale wake-up.
    pub fn begin_prompt(&self) {
        *self.turn.lock().unwrap() += 1;
        *self.interrupts.lock().unwrap() = 0;
        self.hits.lock().unwrap().clear();
        self.try_clear_hit();
    }

    /// Waits for the current stream to be cancelled by a match (polled beside the provider request).
    pub async fn wait_hit(&self) {
        loop {
            // Register before reading the flag, so a notification landing in between is not missed.
            let notified = self.notify.notified();
            if self.hit.swap(false, std::sync::atomic::Ordering::AcqRel) {
                return;
            }
            notified.await;
        }
    }

    /// Drops a wake-up left over when a match landed in the same poll that completed the reply.
    pub fn try_clear_hit(&self) {
        self.hit.store(false, std::sync::atomic::Ordering::Release);
    }

    fn mode(&self, rule: &Rule) -> InterruptMode {
        rule.interrupt.unwrap_or(self.cfg.interrupt_mode)
    }

    /// Whether a prose match may cancel the reply mid-stream.
    fn interrupts_prose(&self, rule: &Rule) -> bool {
        matches!(self.mode(rule), InterruptMode::Always | InterruptMode::ProseOnly)
    }

    fn may_fire(&self, name: &str) -> bool {
        let turn = *self.turn.lock().unwrap();
        match self.injected.lock().unwrap().get(name) {
            None => true,
            Some(&at) => self.cfg.repeat_mode == RepeatMode::AfterGap && turn >= at + u64::from(self.cfg.repeat_gap.max(1)),
        }
    }

    /// One streamed text delta: match rules against the new text and cancel the stream when a rule allows it.
    /// Rules that only warn after completion are left to `match_reply`.
    pub fn push_delta(&self, delta: &str) {
        let mut stream = self.stream.lock().unwrap();
        stream.text.push_str(delta);
        let from = floor_boundary(&stream.text, stream.scanned.saturating_sub(512));
        stream.scanned = stream.text.len();
        let mut fired: Option<&Rule> = None;
        for rule in &self.rules {
            if rule.question.is_none()
                && rule.scope.text
                && self.interrupts_prose(rule)
                && self.may_fire(&rule.name)
                && rule.hits(&stream.text[from..])
            {
                fired = Some(rule);
                break;
            }
        }
        drop(stream);
        let Some(rule) = fired else { return };
        let mut budget = self.interrupts.lock().unwrap();
        if *budget >= 3 {
            return;
        }
        *budget += 1;
        drop(budget);
        self.hits.lock().unwrap().push(rule.name.clone());
        self.hit.store(true, std::sync::atomic::Ordering::Release);
        self.notify.notify_one();
    }

    /// Rule names whose stream match interrupted the request, once.
    pub fn take_hit_names(&self) -> Vec<String> {
        std::mem::take(&mut *self.hits.lock().unwrap())
    }

    /// Text streamed before an interrupt; `keep` commits it as the assistant message.
    pub fn partial(&self) -> String {
        std::mem::take(&mut self.stream.lock().unwrap().text)
    }

    /// Clears the stream buffer at a reply boundary.
    pub fn reset_stream(&self) {
        *self.stream.lock().unwrap() = Stream::default();
    }

    /// Warning text for rule names, marking them injected; names past the repeat gate are skipped.
    pub fn warnings(&self, names: &[String]) -> Vec<String> {
        let turn = *self.turn.lock().unwrap();
        let gap = u64::from(self.cfg.repeat_gap.max(1));
        let mut injected = self.injected.lock().unwrap();
        let mut out = Vec::new();
        for name in names {
            let allowed = match injected.get(name) {
                None => true,
                Some(&at) => self.cfg.repeat_mode == RepeatMode::AfterGap && turn >= at + gap,
            };
            if !allowed {
                continue;
            }
            let Some(rule) = self.rules.iter().find(|r| &r.name == name) else { continue };
            injected.insert(name.clone(), turn);
            out.push(format!("Rule \"{}\" applies to what you just wrote:\n{}", rule.name, rule.body));
        }
        out
    }

    /// Matches a completed reply: text, reasoning and tool arguments, honouring scope, repeat gates and
    /// `ttsr.interrupt_mode`. `block_tools` lets a matching tool call be stopped instead of run.
    pub fn match_reply(&self, blocks: &[Block], block_tools: bool) -> Matches {
        let mut names = Vec::new();
        let mut warnings = Vec::new();
        let mut blocked = HashMap::new();
        for rule in &self.rules {
            if rule.question.is_some() || !self.may_fire(&rule.name) {
                continue;
            }
            let matched = blocks.iter().any(|b| match b {
                Block::Text { text } => rule.scope.text && rule.hits(text),
                Block::Other(v) if v["type"] == "thinking" => {
                    rule.scope.thinking && rule.hits(v["thinking"].as_str().unwrap_or(""))
                }
                _ => false,
            });
            let tools: Vec<&String> = blocks
                .iter()
                .filter_map(|b| match b {
                    Block::ToolUse { id, name, input } if rule.scope.allows_tool(name) && rule.hits(&input.to_string()) => Some(id),
                    _ => None,
                })
                .collect();
            if !matched && tools.is_empty() {
                continue;
            }
            let Some(warning) = self.warnings(std::slice::from_ref(&rule.name)).into_iter().next() else {
                continue;
            };
            if block_tools && matches!(self.mode(rule), InterruptMode::Always | InterruptMode::ToolOnly) {
                for id in tools {
                    blocked.insert(id.clone(), warning.clone());
                }
            }
            names.push(rule.name.clone());
            warnings.push(warning);
        }
        Matches { names, warnings, blocked }
    }

    /// Judged (`question`) rules to ask about this reply: in scope, past the repeat gate.
    pub fn judged_candidates(&self, blocks: &[Block]) -> Vec<(String, String)> {
        if self.judge.is_none() || self.cfg.judge == Judge::Off {
            return Vec::new();
        }
        self.rules
            .iter()
            .filter(|r| r.question.is_some() && self.may_fire(&r.name))
            .filter(|r| {
                blocks.iter().any(|b| match b {
                    Block::Text { .. } => r.scope.text,
                    Block::ToolUse { name, .. } => r.scope.allows_tool(name),
                    Block::Other(v) => v["type"] == "thinking" && r.scope.thinking,
                    _ => false,
                })
            })
            .map(|r| (r.name.clone(), r.question.clone().unwrap_or_default()))
            .collect()
    }

    /// Everything one completed reply exposes to a judge: prose, reasoning and tool arguments.
    pub fn judged_content(blocks: &[Block]) -> String {
        let mut out = String::new();
        for block in blocks {
            match block {
                Block::Text { text } => out.push_str(text),
                Block::Other(v) if v["type"] == "thinking" => out.push_str(v["thinking"].as_str().unwrap_or("")),
                Block::ToolUse { name, input, .. } => out.push_str(&format!("`{name}` {input}")),
                _ => continue,
            }
            out.push('\n');
        }
        out
    }
}

fn floor_boundary(text: &str, mut at: usize) -> usize {
    while at > 0 && !text.is_char_boundary(at) {
        at -= 1;
    }
    at
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ttsr::{ContextMode, InterruptMode, RepeatMode};
    use serde_json::json;

    fn rule(name: &str, trigger: &str) -> Rule {
        parse(&format!("---\ntrigger: {trigger}\n---\nbody of {name}\n"), name).unwrap()
    }

    fn ttsr(cfg: TtsrCfg, rules: Vec<Rule>) -> Ttsr {
        Ttsr::new(cfg, rules, None)
    }

    fn text(s: &str) -> Block {
        Block::Text { text: s.into() }
    }

    #[test]
    fn frontmatter_parses_triggers_questions_scopes_and_list_shapes() {
        let r = parse("---\ncondition: [\"foo+$\", 'bar+$']\nscope: text, tool:bash\ninterruptMode: never\n---\nbody\n", "stem").unwrap();
        assert_eq!(r.triggers.len(), 2);
        assert!(r.hits("foo"));
        assert!(!r.hits("FOO"));
        assert!(r.scope.text && r.scope.allows_tool("bash") && !r.scope.allows_tool("read"));
        assert_eq!(r.interrupt, Some(InterruptMode::Never));
        assert_eq!(r.name, "stem");

        let listed = parse("---\ntrigger:\n  - a+\n  - b+\n---\nx\n", "n").unwrap();
        assert!(listed.hits("aa") && listed.hits("bb"));

        let judged = parse("---\nquestion: Does it claim a test ran?\n---\nb\n", "judged").unwrap();
        assert!(judged.triggers.is_empty() && judged.question.is_some());
        assert!(parse("---\nnothing: here\n---\nx\n", "x").is_none());

        // Default scope: prose and every tool, no reasoning.
        let plain = rule("r", "x");
        assert!(plain.scope.text && plain.scope.allows_tool("bash") && !plain.scope.thinking);
        assert!(parse_scope("thinking").thinking && !parse_scope("thinking").text);
    }

    #[test]
    fn stream_interrupts_prose_only_rules_and_respects_budget_and_mode() {
        let mut cfg = TtsrCfg::default();
        let t = ttsr(cfg.clone(), vec![rule("never-unwrap", r"unwrap\(\)")]);
        t.begin_prompt();
        t.reset_stream();
        t.push_delta("I will call unwrap");
        assert!(t.take_hit_names().is_empty(), "no match yet");
        t.push_delta("() now");
        assert_eq!(t.take_hit_names(), ["never-unwrap"]);
        assert_eq!(t.partial(), "I will call unwrap() now");

        // `tool-only` never cancels prose; `never` neither.
        for mode in [InterruptMode::ToolOnly, InterruptMode::Never] {
            cfg.interrupt_mode = mode;
            let t = ttsr(cfg.clone(), vec![rule("r", "boom")]);
            assert!(!t.interrupting());
            t.begin_prompt();
            t.push_delta("boom");
            assert!(t.take_hit_names().is_empty());
        }

        // A looping model cannot cancel more than three times in one turn.
        let mut cfg = TtsrCfg::default();
        cfg.repeat_mode = RepeatMode::AfterGap;
        cfg.repeat_gap = 1;
        let t = ttsr(cfg, vec![rule("r", "boom")]);
        t.begin_prompt();
        for _ in 0..5 {
            t.push_delta("boom");
            let _ = t.take_hit_names();
        }
        assert_eq!(*t.interrupts.lock().unwrap(), 3);
    }

    #[test]
    fn repeat_gate_fires_once_or_after_the_gap() {
        let cfg = TtsrCfg::default();
        let t = ttsr(cfg, vec![rule("r", "bad")]);
        t.begin_prompt();
        assert_eq!(t.match_reply(&[text("bad")], true).names, ["r"]);
        t.begin_prompt();
        assert!(t.match_reply(&[text("bad")], true).names.is_empty(), "once per session");

        let mut cfg = TtsrCfg::default();
        cfg.repeat_mode = RepeatMode::AfterGap;
        cfg.repeat_gap = 2;
        let t = ttsr(cfg, vec![rule("r", "bad")]);
        t.begin_prompt();
        assert_eq!(t.match_reply(&[text("bad")], true).names.len(), 1);
        t.begin_prompt();
        assert!(t.match_reply(&[text("bad")], true).names.is_empty(), "gap not reached");
        t.begin_prompt();
        assert_eq!(t.match_reply(&[text("bad")], true).names.len(), 1);
    }

    #[test]
    fn completion_matches_tool_arguments_and_mode_decides_blocking() {
        let tool = || Block::ToolUse {
            id: "t1".into(),
            name: "bash".into(),
            input: json!({"command": "git reset --hard"}),
        };
        let rules = || {
            parse("---\ntrigger: reset --hard\nscope: tool:bash\n---\nNever discard work.\n", "no-reset").unwrap()
        };
        let m = tool();

        for (mode, blocked) in [
            (InterruptMode::Always, true),
            (InterruptMode::ToolOnly, true),
            (InterruptMode::ProseOnly, false),
            (InterruptMode::Never, false),
        ] {
            let mut cfg = TtsrCfg::default();
            cfg.interrupt_mode = mode;
            let t = ttsr(cfg, vec![rules()]);
            t.begin_prompt();
            let out = t.match_reply(&[m.clone()], true);
            assert_eq!(out.blocked.contains_key("t1"), blocked, "{mode:?}");
            assert_eq!(out.names, ["no-reset"]);
            assert!(out.warnings[0].contains("Never discard work."));
        }

        // A rule out of tool scope ignores the same arguments.
        let mut cfg = TtsrCfg::default();
        cfg.interrupt_mode = InterruptMode::Always;
        let t = ttsr(cfg, vec![parse("---\ntrigger: reset --hard\nscope: text\n---\nbody\n", "prose").unwrap()]);
        t.begin_prompt();
        assert!(t.match_reply(&[m], true).names.is_empty());
    }

    #[test]
    fn builtins_are_shipped_disabled_by_list_and_overridable() {
        let all = builtins();
        assert_eq!(all.len(), BUILTIN.len());
        assert!(all.iter().all(|r| r.interrupt == Some(InterruptMode::Never)));
        let sycophancy = all.iter().find(|r| r.name == "no-sycophancy").unwrap();
        assert!(sycophancy.hits("Great question!"));

        let dir = std::env::temp_dir().join(format!("greencode-ttsr-{}", std::process::id()));
        let rules_dir = dir.join(".greencode/rules");
        std::fs::create_dir_all(&rules_dir).unwrap();
        std::fs::write(rules_dir.join("no-sycophancy.md"), "---\ntrigger: custom\n---\nmy own body\n").unwrap();
        std::fs::write(rules_dir.join("extra.md"), "---\ntrigger: extra\n---\nx\n").unwrap();

        let mut cfg = TtsrCfg::default();
        let loaded = load(&dir, &cfg);
        assert_eq!(loaded.iter().filter(|r| r.name == "no-sycophancy").count(), 1, "a user rule replaces the builtin");
        assert!(loaded.iter().any(|r| r.name == "extra"));

        cfg.disabled_rules = vec!["extra".into()];
        let loaded = load(&dir, &cfg);
        assert!(!loaded.iter().any(|r| r.name == "extra"));
        assert!(loaded.iter().any(|r| r.name == "no-sycophancy"));

        cfg.builtin_rules = false;
        cfg.disabled_rules.clear();
        let loaded = load(&dir, &cfg);
        assert!(loaded.iter().all(|r| r.name == "no-sycophancy" || r.name == "extra"));
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn judged_rules_need_a_judge_and_a_scope() {
        let rule = parse("---\nquestion: Does it claim a test passed?\n---\nbody\n", "claims").unwrap();
        let mut cfg = TtsrCfg::default();
        cfg.judge = Judge::Off;
        assert!(ttsr(cfg.clone(), vec![rule.clone()]).judged_candidates(&[text("hi")]).is_empty(), "judge off");

        // A judged rule runs only with the judge on (the default) and a judge model resolved.
        let mut t = ttsr(TtsrCfg { judge: Judge::Auto, ..cfg }, vec![rule]);
        t.judge = Some("mock/j".into());
        t.begin_prompt();
        let candidates = t.judged_candidates(&[text("hi")]);
        assert_eq!(candidates[0].0, "claims");
        assert_eq!(t.judged_candidates(&[Block::ToolResult { tool_use_id: "t".into(), content: "x".into(), is_error: false }]).len(), 0);

        let prompt = judge_prompt("the reply", &candidates);
        assert!(prompt.contains("claims: Does it claim a test passed?"), "{prompt}");
        assert_eq!(parse_verdicts("- claims: YES\nunknown: yes\nclaims: yes\n", &candidates), ["claims"]);
        assert_eq!(parse_verdicts("claims: yes\n", &candidates), ["claims"]);
        assert_eq!(parse_verdicts("```\nclaims: yes.\n```\n", &candidates), ["claims"]);
    }

    #[test]
    fn keep_and_discard_decide_the_partial_reply() {
        let cfg = TtsrCfg::default();
        assert_eq!(cfg.context_mode, ContextMode::Discard);
        let t = ttsr(cfg, vec![rule("r", "stop")]);
        t.begin_prompt();
        t.push_delta("half a sen");
        assert_eq!(t.partial(), "half a sen");
        assert_eq!(t.partial(), "", "taken once");
    }

    #[test]
    fn judged_content_shows_prose_reasoning_and_tool_arguments() {
        let blocks = [
            text("hello"),
            Block::Other(json!({"type": "thinking", "thinking": "hmm"})),
            Block::ToolUse { id: "t".into(), name: "bash".into(), input: json!({"command": "ls"}) },
        ];
        let content = Ttsr::judged_content(&blocks);
        assert!(content.contains("hello") && content.contains("hmm") && content.contains("`bash`"));
    }
}
