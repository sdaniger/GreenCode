//! Durable workflows: TOML files chaining agents, shell commands, questions and waits.
//! Runs are JSON files, saved after every step, so a restart resumes where the run stopped.
use crate::{
    permission::{Mode, Perms},
    session::Meta,
    BoxFut, Command, Decision, Event,
};
use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, HashSet},
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

pub const WEBHOOK_PORT: u16 = 47323;
pub const RETRY_MINUTES: [u64; 3] = [5, 20, 80];

pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// `~/.greencode/workflow-runs`, `workflow-definitions-cache.json` beside it.
#[derive(Clone, Debug)]
pub struct Paths {
    pub workflows: PathBuf,
    pub runs: PathBuf,
    pub cache: PathBuf,
}

pub fn paths() -> Paths {
    let workflows = std::env::var_os("GREENCODE_WORKFLOWS")
        .map(PathBuf::from)
        .unwrap_or_else(|| crate::home().join("workflows"));
    let runs = crate::home().join("workflow-runs");
    let cache = crate::home().join("workflow-definitions-cache.json");
    Paths { workflows, runs, cache }
}

/// Whether the Labs toggle is on (`ui.json`'s `labs_workflows`).
pub fn enabled() -> bool {
    std::fs::read(crate::home().join("ui.json"))
        .ok()
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        .is_some_and(|v| v["labs_workflows"] == true)
}

// ---- definitions ----

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Definition {
    pub name: String,
    #[serde(default = "one")]
    pub version: u32,
    #[serde(default)]
    pub payload: BTreeMap<String, PayloadField>,
    pub steps: Vec<Step>,
    #[serde(default)]
    pub trigger: Trigger,
    #[serde(default)]
    pub source: PathBuf,
}

fn one() -> u32 {
    1
}

fn default_true() -> bool {
    true
}

fn default_mode() -> String {
    "full".into()
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PayloadField {
    #[serde(rename = "type", default)]
    pub kind: Option<String>,
    #[serde(default = "default_true")]
    pub required: bool,
    #[serde(rename = "default", default)]
    pub default_value: Option<Value>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Trigger {
    #[serde(default)]
    pub every: Option<String>,
    #[serde(default)]
    pub daily: Option<String>,
    #[serde(default)]
    pub webhook: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Step {
    Agent {
        id: String,
        prompt: String,
        #[serde(default)]
        backend: Option<String>,
        #[serde(default)]
        model: Option<String>,
        #[serde(default)]
        project: Option<String>,
        #[serde(default = "default_mode")]
        mode: String,
        #[serde(default)]
        timeout: Option<String>,
        #[serde(default)]
        title: Option<String>,
    },
    Shell {
        id: String,
        command: String,
        #[serde(default)]
        timeout: Option<String>,
    },
    Ask {
        id: String,
        question: String,
        #[serde(default)]
        title: Option<String>,
        #[serde(default)]
        options: Vec<String>,
    },
    Wait {
        id: String,
        duration: String,
    },
}

impl Step {
    pub fn id(&self) -> &str {
        match self {
            Step::Agent { id, .. }
            | Step::Shell { id, .. }
            | Step::Ask { id, .. }
            | Step::Wait { id, .. } => id,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct Definitions {
    /// Sorted by name, highest version last.
    pub workflows: Vec<Definition>,
    pub errors: Vec<(PathBuf, String)>,
}

impl Definitions {
    pub fn find(&self, name: &str) -> Option<&Definition> {
        self.workflows
            .iter()
            .filter(|d| d.name == name)
            .max_by_key(|d| d.version)
    }

    pub fn find_exact(&self, name: &str, version: u32) -> Option<&Definition> {
        self.workflows
            .iter()
            .find(|d| d.name == name && d.version == version)
    }

    pub fn error_text(&self) -> String {
        self.errors
            .iter()
            .map(|(path, e)| format!("{}: {e}", path.display()))
            .collect::<Vec<_>>()
            .join("\n")
    }
}

pub fn load_definitions(paths: &Paths) -> Definitions {
    let cached = std::fs::read(&paths.cache)
        .ok()
        .and_then(|b| serde_json::from_slice::<BTreeMap<String, Definition>>(&b).ok())
        .unwrap_or_default();
    let mut out = Definitions::default();
    let mut next_cache = BTreeMap::new();
    let mut seen_names = HashSet::new();
    let mut seen_hooks = HashSet::new();
    for file in definition_files(&paths.workflows) {
        let key = file.to_string_lossy().into_owned();
        let file_key = format!("file:{key}");
        let parsed = std::fs::read_to_string(&file)
            .map_err(|e| format!("読み込めません: {e}"))
            .and_then(|text| {
                toml::from_str::<Definition>(&text).map_err(|e| format!("TOML: {e}"))
            })
            .and_then(|mut def| {
                def.source = file.clone();
                validate(&def)?;
                Ok(def)
            });
        match parsed {
            Ok(def) => {
                if !seen_names.insert((def.name.clone(), def.version)) {
                    let e = format!("ワークフロー {} v{} が重複しています", def.name, def.version);
                    out.errors.push((file.clone(), e));
                    if let Some(old) = cached.get(&file_key) {
                        if seen_names.insert((old.name.clone(), old.version)) {
                            cache_definition(&mut next_cache, old);
                            out.workflows.push(old.clone());
                        }
                    }
                    continue;
                }
                if let Some(secret) = &def.trigger.webhook {
                    if !seen_hooks.insert(secret.clone()) {
                        out.errors.push((
                            file.clone(),
                            format!("Webhook パス `{secret}` が重複しています"),
                        ));
                        continue;
                    }
                }
                cache_definition(&mut next_cache, &def);
                out.workflows.push(def);
            }
            Err(e) => {
                out.errors.push((file.clone(), e));
                // A broken edit keeps the last version that loaded.
                if let Some(old) = cached.get(&file_key) {
                    if seen_names.insert((old.name.clone(), old.version)) {
                        cache_definition(&mut next_cache, old);
                        out.workflows.push(old.clone());
                    }
                }
            }
        }
    }
    // Keep a version that unfinished runs still need even when its file was removed or renamed.
    for (name, version) in unfinished_versions(&paths.runs) {
        if out.find_exact(&name, version).is_some() {
            continue;
        }
        if let Some(old) = cached.get(&format!("v:{name}@{version}")) {
            out.errors.push((
                old.source.clone(),
                format!("未完了の実行があるため {name} v{version} の定義を保持しています"),
            ));
            out.workflows.push(old.clone());
            cache_definition(&mut next_cache, old);
        }
    }
    out.workflows
        .sort_by(|a, b| a.name.cmp(&b.name).then(a.version.cmp(&b.version)));
    if cached != next_cache {
        if let Some(parent) = paths.cache.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(bytes) = serde_json::to_vec(&next_cache) {
            let tmp = paths.cache.with_extension("json.tmp");
            if std::fs::write(&tmp, bytes).is_ok() {
                let _ = std::fs::rename(&tmp, &paths.cache);
            }
        }
    }
    out
}

fn cache_definition(cache: &mut BTreeMap<String, Definition>, def: &Definition) {
    cache.insert(
        format!("file:{}", def.source.to_string_lossy()),
        def.clone(),
    );
    cache.insert(format!("v:{}@{}", def.name, def.version), def.clone());
}

/// `.toml` files in a folder; dot-files and subfolders are ignored. `GREENCODE_WORKFLOWS` may name one file.
fn definition_files(path: &Path) -> Vec<PathBuf> {
    if path.is_file() {
        return vec![path.to_path_buf()];
    }
    let Ok(entries) = std::fs::read_dir(path) else {
        return vec![];
    };
    let mut files: Vec<PathBuf> = entries
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
        .map(|e| e.path())
        .filter(|p| {
            p.extension().is_some_and(|e| e == "toml")
                && !p
                    .file_name()
                    .is_some_and(|n| n.to_string_lossy().starts_with('.'))
        })
        .collect();
    files.sort();
    files
}

pub fn validate(def: &Definition) -> std::result::Result<(), String> {
    if def.name.is_empty()
        || !def
            .name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    {
        return Err("name は英数字と - _ . だけにしてください".into());
    }
    if def.version == 0 {
        return Err("version は 1 以上にしてください".into());
    }
    if def.steps.is_empty() {
        return Err("steps がありません".into());
    }
    let mut ids = HashSet::new();
    for (i, step) in def.steps.iter().enumerate() {
        let id = step.id();
        if id.is_empty()
            || !id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
        {
            return Err(format!("step id「{id}」は英数字と - _ だけにしてください"));
        }
        if !ids.insert(id) {
            return Err(format!("step id「{id}」が重複しています"));
        }
        let mut templates: Vec<&str> = vec![];
        match step {
            Step::Agent {
                prompt,
                mode,
                timeout,
                backend,
                ..
            } => {
                if prompt.trim().is_empty() {
                    return Err(format!("step「{id}」の prompt が空です"));
                }
                if !matches!(mode.as_str(), "ask" | "auto-edit" | "full" | "plan") {
                    return Err(format!(
                        "step「{id}」の mode は ask / auto-edit / full / plan のどれかです"
                    ));
                }
                if backend.as_deref().is_some_and(|b| b.trim().is_empty()) {
                    return Err(format!("step「{id}」の backend が空です"));
                }
                if let Some(t) = timeout {
                    parse_duration(t).map_err(|e| format!("step「{id}」の timeout: {e}"))?;
                }
                templates.push(prompt);
            }
            Step::Shell {
                command,
                timeout,
                ..
            } => {
                if command.trim().is_empty() {
                    return Err(format!("step「{id}」の command が空です"));
                }
                if let Some(t) = timeout {
                    parse_duration(t).map_err(|e| format!("step「{id}」の timeout: {e}"))?;
                }
                templates.push(command);
            }
            Step::Ask {
                question,
                options,
                title,
                ..
            } => {
                if question.trim().is_empty() {
                    return Err(format!("step「{id}」の question が空です"));
                }
                if options.is_empty() {
                    return Err(format!("step「{id}」の options がありません"));
                }
                templates.push(question);
                if let Some(t) = title {
                    templates.push(t);
                }
                templates.extend(options.iter().map(String::as_str));
            }
            Step::Wait { duration, .. } => {
                parse_duration(duration).map_err(|e| format!("step「{id}」の duration: {e}"))?;
            }
        }
        for text in templates {
            check_refs(text, def, &def.steps[..i])
                .map_err(|e| format!("step「{id}」: {e}"))?;
        }
    }
    if let Some(field) = def.payload.values().find(|f| {
        f.kind
            .as_deref()
            .is_some_and(|k| !matches!(k, "string" | "number" | "boolean" | "json"))
    }) {
        let _ = field;
        return Err("payload の type は string / number / boolean / json です".into());
    }
    let triggers = [
        def.trigger.every.is_some(),
        def.trigger.daily.is_some(),
        def.trigger.webhook.is_some(),
    ]
    .into_iter()
    .filter(|b| *b)
    .count();
    if triggers > 1 {
        return Err("trigger は every / daily / webhook のどれか 1 つです".into());
    }
    if let Some(every) = &def.trigger.every {
        parse_duration(every).map_err(|e| format!("trigger.every: {e}"))?;
    }
    if let Some(daily) = &def.trigger.daily {
        parse_daily(daily).map_err(|e| format!("trigger.daily: {e}"))?;
    }
    if let Some(secret) = &def.trigger.webhook {
        if secret.is_empty()
            || !secret
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
        {
            return Err("trigger.webhook は英数字と - _ だけにしてください".into());
        }
    }
    Ok(())
}

fn check_refs(text: &str, def: &Definition, earlier: &[Step]) -> std::result::Result<(), String> {
    for key in refs(text) {
        if let Some(path) = key.strip_prefix("payload.") {
            let first = path.split('.').next().unwrap_or("");
            if !def.payload.contains_key(first) {
                return Err(format!("{{{{{key}}}}} の payload フィールドがありません"));
            }
        } else if let Some(rest) = key.strip_prefix("steps.") {
            let (id, field) = rest
                .split_once('.')
                .ok_or_else(|| format!("{{{{{key}}}}} は steps.ID.output の形にしてください"))?;
            if field != "output" {
                return Err(format!("{{{{{key}}}}} のフィールドは output だけです"));
            }
            if !earlier.iter().any(|s| s.id() == id) {
                return Err(format!("{{{{{key}}}}} の step「{id}」はまだ実行されていません"));
            }
        } else {
            return Err(format!(
                "{{{{{key}}}}} は payload.NAME か steps.ID.output にしてください"
            ));
        }
    }
    Ok(())
}

/// Keys inside `{{…}}`, skipping an unterminated opening.
fn refs(text: &str) -> Vec<String> {
    let mut out = vec![];
    let mut rest = text;
    while let Some(start) = rest.find("{{") {
        let after = &rest[start + 2..];
        let Some(end) = after.find("}}") else {
            break;
        };
        out.push(after[..end].trim().to_string());
        rest = &after[end + 2..];
    }
    out
}

fn get_path<'a>(value: &'a Value, path: &str) -> Option<&'a Value> {
    let mut current = value;
    for part in path.split('.') {
        current = match current {
            Value::Object(map) => map.get(part)?,
            Value::Array(items) => items.get(part.parse::<usize>().ok()?)?,
            _ => return None,
        };
    }
    Some(current)
}

pub fn render(template: &str, payload: &Value, steps: &[StepRun]) -> Result<String> {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(start) = rest.find("{{") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let end = after
            .find("}}")
            .ok_or_else(|| anyhow!("{{{{ が閉じていません"))?;
        let key = after[..end].trim();
        if let Some(path) = key.strip_prefix("payload.") {
            match get_path(payload, path).ok_or_else(|| anyhow!("payload.{path} がありません"))? {
                Value::String(s) => out.push_str(s),
                Value::Null => {}
                other => out.push_str(&other.to_string()),
            }
        } else if let Some(rest) = key.strip_prefix("steps.") {
            let (id, field) = rest
                .split_once('.')
                .ok_or_else(|| anyhow!("{key} は steps.ID.output の形にしてください"))?;
            if field != "output" {
                bail!("{key} のフィールドは output だけです");
            }
            let step = steps
                .iter()
                .find(|s| s.id == id)
                .ok_or_else(|| anyhow!("step「{id}」はまだ実行されていません"))?;
            out.push_str(&step.output);
        } else {
            bail!("{{{{{key}}}}} は payload.NAME か steps.ID.output にしてください");
        }
        rest = &after[end + 2..];
    }
    out.push_str(rest);
    Ok(out)
}

// ---- runs ----

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RunStatus {
    Running,
    Waiting,
    Paused,
    Done,
    Failed,
    Cancelled,
}

impl RunStatus {
    pub fn label(self) -> &'static str {
        match self {
            Self::Running => "実行中",
            Self::Waiting => "回答待ち",
            Self::Paused => "一時停止",
            Self::Done => "完了",
            Self::Failed => "失敗",
            Self::Cancelled => "キャンセル",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StepState {
    Pending,
    Running,
    Waiting,
    Done,
    Failed,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Question {
    pub step: String,
    pub title: String,
    pub options: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StepRun {
    pub id: String,
    pub status: StepState,
    #[serde(default)]
    pub output: String,
    #[serde(default)]
    pub attempts: u32,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub at: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Run {
    pub id: String,
    pub workflow: String,
    pub version: u32,
    #[serde(default)]
    pub key: Option<String>,
    pub payload: Value,
    pub status: RunStatus,
    #[serde(default)]
    pub current: usize,
    #[serde(default)]
    pub steps: Vec<StepRun>,
    #[serde(default)]
    pub question: Option<Question>,
    #[serde(default)]
    pub retry_at: Option<u64>,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub trigger: String,
    pub created: u64,
    pub updated: u64,
}

impl Run {
    pub fn step(&self, id: &str) -> Option<&StepRun> {
        self.steps.iter().find(|s| s.id == id)
    }

    /// Waiting for an answer or an explicit resume.
    pub fn needs_you(&self) -> bool {
        self.status == RunStatus::Waiting
            || (self.status == RunStatus::Paused && self.retry_at.is_none())
    }

    /// Work continues by itself: running, retrying or waiting out a `wait` step.
    pub fn auto_pending(&self) -> bool {
        self.status == RunStatus::Running
            || (self.status == RunStatus::Paused && self.retry_at.is_some())
    }

    pub fn finished(&self) -> bool {
        matches!(
            self.status,
            RunStatus::Done | RunStatus::Failed | RunStatus::Cancelled
        )
    }

    /// One-line description for CLI and the app list.
    pub fn summary(&self) -> String {
        let mut text = format!("{} v{} · {}", self.workflow, self.version, self.status.label());
        if let Some(q) = &self.question {
            text.push_str(&format!(" · {}", q.title));
        } else if let Some(e) = &self.error {
            text.push_str(&format!(" · {}", e.lines().next().unwrap_or("")));
        }
        text
    }
}

fn validate_run_id(id: &str) -> Result<()> {
    if id.is_empty()
        || !id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        bail!("invalid run id `{id}`");
    }
    Ok(())
}

fn run_path(paths: &Paths, id: &str) -> Result<PathBuf> {
    validate_run_id(id)?;
    Ok(paths.runs.join(format!("{id}.json")))
}

pub fn load_run(paths: &Paths, id: &str) -> Result<Run> {
    let path = run_path(paths, id)?;
    let bytes = std::fs::read(&path).with_context(|| format!("実行 {id} が見つかりません"))?;
    serde_json::from_slice(&bytes).with_context(|| format!("実行 {id} を読み込めません"))
}

pub fn all_runs(paths: &Paths) -> Vec<Run> {
    let Ok(entries) = std::fs::read_dir(&paths.runs) else {
        return vec![];
    };
    let mut runs: Vec<Run> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "json"))
        .filter_map(|p| std::fs::read(p).ok())
        .filter_map(|b| serde_json::from_slice::<Run>(&b).ok())
        .collect();
    runs.sort_by(|a, b| b.updated.cmp(&a.updated));
    runs
}

fn save_run(paths: &Paths, run: &mut Run) -> Result<()> {
    run.updated = now();
    std::fs::create_dir_all(&paths.runs)?;
    let path = run_path(paths, &run.id)?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(run)?)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

fn find_key(paths: &Paths, key: &str) -> Option<Run> {
    all_runs(paths)
        .into_iter()
        .find(|r| r.key.as_deref() == Some(key))
}

fn unfinished_versions(runs_dir: &Path) -> HashSet<(String, u32)> {
    let Ok(entries) = std::fs::read_dir(runs_dir) else {
        return HashSet::new();
    };
    entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "json"))
        .filter_map(|p| std::fs::read(p).ok())
        .filter_map(|b| serde_json::from_slice::<Run>(&b).ok())
        .filter(|r| !r.finished())
        .map(|r| (r.workflow, r.version))
        .collect()
}

/// Seconds to wait before the next automatic shell retry; `None` after the last one.
pub fn retry_delay(attempts: u32) -> Option<u64> {
    RETRY_MINUTES
        .get(attempts.checked_sub(1)? as usize)
        .map(|m| m * 60)
}

/// `30s`, `5m`, `2h`, `1d`; at least one second.
pub fn parse_duration(text: &str) -> std::result::Result<u64, String> {
    let text = text.trim();
    let (digits, unit) = text.split_at(text.len().saturating_sub(1));
    let n: u64 = digits
        .trim()
        .parse()
        .map_err(|_| format!("「{text}」は 30s / 5m / 2h / 1d の形にしてください"))?;
    let secs = match unit {
        "s" => n,
        "m" => n * 60,
        "h" => n * 3600,
        "d" => n * 86400,
        _ => return Err(format!("「{text}」の単位は s / m / h / d です")),
    };
    if secs == 0 {
        return Err("時間は 1 秒以上にしてください".into());
    }
    Ok(secs)
}

fn parse_daily(text: &str) -> std::result::Result<u32, String> {
    let (h, m) = text
        .trim()
        .split_once(':')
        .ok_or_else(|| "HH:MM の形にしてください".to_string())?;
    let h: u32 = h.parse().map_err(|_| "時が数値ではありません".to_string())?;
    let m: u32 = m.parse().map_err(|_| "分が数値ではありません".to_string())?;
    if h > 23 || m > 59 || text.trim().len() != 5 {
        return Err("時刻は 00:00〜23:59 です".into());
    }
    Ok(h * 3600 + m * 60)
}

/// Local UTC offset in seconds; cached once (`date +%z`, 0 when unavailable).
fn local_offset() -> i64 {
    static OFFSET: std::sync::LazyLock<i64> = std::sync::LazyLock::new(|| {
        std::process::Command::new("/bin/date")
            .arg("+%z")
            .output()
            .ok()
            .and_then(|o| parse_offset(String::from_utf8_lossy(&o.stdout).trim()))
            .unwrap_or(0)
    });
    *OFFSET
}

fn parse_offset(z: &str) -> Option<i64> {
    let (sign, digits) = match z.split_at_checked(1)? {
        ("+", d) => (1, d),
        ("-", d) => (-1, d),
        _ => return None,
    };
    if digits.len() != 4 || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some(sign * (digits[..2].parse::<i64>().ok()? * 3600 + digits[2..].parse::<i64>().ok()? * 60))
}

/// The `HH:MM` instant after `last`, in local time.
fn next_daily(last: u64, hhmm: u32) -> u64 {
    let offset = local_offset();
    let local = last as i64 + offset;
    let day = local - local.rem_euclid(86400);
    let mut target = day + hhmm as i64;
    if target <= last as i64 {
        target += 86400;
    }
    (target - offset) as u64
}

fn validate_payload(def: &Definition, payload: Value) -> Result<Value> {
    let Value::Object(mut input) = payload else {
        bail!("payload は JSON オブジェクトにしてください");
    };
    for (name, field) in &def.payload {
        let value = input.get(name).cloned().or_else(|| field.default_value.clone());
        let Some(value) = value else {
            if field.required {
                bail!("payload.{name} がありません");
            }
            continue;
        };
        let ok = match field.kind.as_deref() {
            None | Some("json") => true,
            Some("string") => value.is_string(),
            Some("number") => value.is_number(),
            Some("boolean") => value.is_boolean(),
            Some(kind) => bail!("payload.{name} の不明な type `{kind}`"),
        };
        if !ok {
            bail!(
                "payload.{name} は {} である必要があります",
                field.kind.as_deref().unwrap_or("json")
            );
        }
        input.insert(name.clone(), value);
    }
    Ok(Value::Object(input))
}

// ---- agent runner ----

#[derive(Clone, Debug)]
pub struct AgentRequest {
    pub workflow: String,
    pub step: String,
    pub prompt: String,
    pub backend: Option<String>,
    pub model: Option<String>,
    pub title: Option<String>,
    pub cwd: PathBuf,
    pub mode: Mode,
    pub timeout: Duration,
}

pub type AgentRunner = Arc<dyn Fn(AgentRequest) -> BoxFut<'static, Result<String>> + Send + Sync>;

/// The real agent: a hidden thread with the workflow's prompt, returning its text reply.
pub fn default_agent(cfg: Arc<crate::config::Config>) -> AgentRunner {
    Arc::new(move |req: AgentRequest| {
        let cfg = cfg.clone();
        Box::pin(async move { run_agent(cfg, req).await })
    })
}

async fn run_agent(cfg: Arc<crate::config::Config>, req: AgentRequest) -> Result<String> {
    let mut cfg = (*cfg).clone();
    if let Some(model) = &req.model {
        cfg.model = model.clone();
    }
    let backend = req.backend.clone().unwrap_or_else(|| "native".into());
    let model = if backend == "native" {
        cfg.model.clone()
    } else {
        req.model.clone().unwrap_or_else(|| "default".into())
    };
    let mut meta = Meta::new(req.cwd.clone(), model, backend);
    meta.title = req
        .title
        .clone()
        .unwrap_or_else(|| format!("ワークフロー {} / {}", req.workflow, req.step));
    let perms = Arc::new(Perms::new(req.mode));
    let (tx, mut rx) = crate::spawn(
        &tokio::runtime::Handle::current(),
        Arc::new(cfg),
        meta,
        perms,
    );
    tx.send(Command::Prompt(req.prompt.clone()))?;
    let mut text = String::new();
    let mut error = None;
    let deadline = tokio::time::sleep(req.timeout);
    tokio::pin!(deadline);
    loop {
        tokio::select! {
            _ = &mut deadline => bail!("エージェントがタイムアウトしました（{} 秒）", req.timeout.as_secs()),
            event = rx.recv() => match event {
                None => break,
                Some(Event::Text(t)) => text.push_str(&t),
                Some(Event::Error(e)) => error = Some(e),
                Some(Event::Permission { tool, summary, reply }) => {
                    let _ = reply.send(Decision::Deny);
                    error.get_or_insert_with(|| format!("承認が必要です（{tool}: {summary}）"));
                }
                Some(Event::Dialog { reply, .. }) => { let _ = reply.send(json!({"cancelled": true})); }
                Some(Event::Done { .. }) => break,
                _ => {}
            }
        }
    }
    drop(tx);
    if let Some(e) = error {
        bail!(e);
    }
    let text = text.trim().to_string();
    if text.is_empty() {
        bail!("エージェントが空の応答を返しました");
    }
    Ok(text)
}

// ---- runtime ----

#[derive(Clone)]
pub struct Runtime {
    pub cfg: Arc<crate::config::Config>,
    pub workdir: PathBuf,
    pub paths: Paths,
    pub agent: AgentRunner,
}

impl Runtime {
    pub fn new(cfg: Arc<crate::config::Config>, workdir: PathBuf) -> Self {
        let agent = default_agent(cfg.clone());
        Self {
            cfg,
            workdir,
            paths: paths(),
            agent,
        }
    }

    pub fn with_paths(
        cfg: Arc<crate::config::Config>,
        workdir: PathBuf,
        paths: Paths,
        agent: AgentRunner,
    ) -> Self {
        Self {
            cfg,
            workdir,
            paths,
            agent,
        }
    }

    pub fn definitions(&self) -> Definitions {
        load_definitions(&self.paths)
    }

    pub fn runs(&self) -> Vec<Run> {
        all_runs(&self.paths)
    }

    pub fn start(&self, name: &str, payload: Value, key: Option<String>, trigger: &str) -> Result<Run> {
        if let Some(key) = key.as_deref() {
            if let Some(run) = find_key(&self.paths, key) {
                return Ok(run);
            }
        }
        let defs = self.definitions();
        let def = defs.find(name).ok_or_else(|| {
            if defs.errors.is_empty() {
                anyhow!("ワークフロー `{name}` がありません（{}）", self.paths.workflows.display())
            } else {
                anyhow!("ワークフロー `{name}` がありません:\n{}", defs.error_text())
            }
        })?;
        let payload = validate_payload(def, payload)?;
        let mut bytes = [0u8; 8];
        getrandom::fill(&mut bytes).map_err(|e| anyhow!("実行 ID を作れません: {e}"))?;
        let at = now();
        let id = format!(
            "{at:x}-{}",
            bytes.iter().map(|b| format!("{b:02x}")).collect::<String>()
        );
        let mut run = Run {
            id,
            workflow: def.name.clone(),
            version: def.version,
            key,
            payload,
            status: RunStatus::Running,
            current: 0,
            steps: vec![],
            question: None,
            retry_at: None,
            error: None,
            trigger: trigger.into(),
            created: at,
            updated: at,
        };
        save_run(&self.paths, &mut run)?;
        Ok(run)
    }

    /// Applies an answer to the open question; the caller advances the run next.
    pub fn answer(&self, id: &str, step: &str, choice: &str) -> Result<Run> {
        let mut run = load_run(&self.paths, id)?;
        if run.status != RunStatus::Waiting {
            bail!("実行 {id} は回答待ちではありません（{}）", run.status.label());
        }
        let question = run
            .question
            .clone()
            .ok_or_else(|| anyhow!("実行 {id} に質問がありません"))?;
        if question.step != step {
            bail!("回答待ちの step は {} です", question.step);
        }
        if !question.options.iter().any(|o| o == choice) {
            bail!(
                "選択肢は {} のどれかです",
                question.options.join(" / ")
            );
        }
        let entry = upsert_step(&mut run, step);
        entry.status = StepState::Done;
        entry.output = choice.to_string();
        entry.error = None;
        entry.at = now();
        run.question = None;
        run.current += 1;
        run.status = RunStatus::Running;
        run.error = None;
        save_run(&self.paths, &mut run)?;
        Ok(run)
    }

    pub fn resume(&self, id: &str) -> Result<Run> {
        let mut run = load_run(&self.paths, id)?;
        if run.status == RunStatus::Waiting {
            bail!("実行 {id} は回答待ちです。先に answer してください");
        }
        if run.status != RunStatus::Paused {
            bail!("実行 {id} は一時停止していません（{}）", run.status.label());
        }
        run.status = RunStatus::Running;
        run.retry_at = None;
        run.error = None;
        let index = run.current.min(run.steps.len().saturating_sub(1));
        if let Some(step) = run.steps.get_mut(index) {
            step.attempts = 0;
        }
        save_run(&self.paths, &mut run)?;
        Ok(run)
    }

    pub fn cancel(&self, id: &str, reason: Option<&str>) -> Result<Run> {
        let mut run = load_run(&self.paths, id)?;
        if matches!(run.status, RunStatus::Done | RunStatus::Cancelled) {
            bail!("実行 {id} はすでに{}です", run.status.label());
        }
        run.status = RunStatus::Cancelled;
        run.question = None;
        run.retry_at = None;
        if let Some(reason) = reason.filter(|r| !r.is_empty()) {
            run.error = Some(reason.into());
        }
        save_run(&self.paths, &mut run)?;
        Ok(run)
    }

    /// Run ids whose next step is due. A paused run waiting for the user is not due.
    pub fn due_ids(&self) -> Vec<String> {
        let at = now();
        self.runs()
            .into_iter()
            .filter(|r| {
                r.status == RunStatus::Running
                    || (r.status == RunStatus::Paused
                        && r.question.is_none()
                        && r.retry_at.is_some_and(|t| t <= at))
            })
            .map(|r| r.id)
            .collect()
    }

    pub async fn advance_run(&self, id: &str) -> Result<Run> {
        let lock_path = self.paths.runs.join(format!("{id}.lock"));
        std::fs::create_dir_all(&self.paths.runs)?;
        let lock = std::fs::File::create(lock_path)?;
        if lock.try_lock().is_err() {
            bail!("実行 {id} は別のプロセスが進めています");
        }
        let mut run = load_run(&self.paths, id)?;
        if run.finished() || run.status == RunStatus::Waiting {
            return Ok(run);
        }
        if run.retry_at.is_some_and(|t| t > now()) {
            return Ok(run);
        }
        let defs = self.definitions();
        let Some(def) = defs.find_exact(&run.workflow, run.version) else {
            run.status = RunStatus::Paused;
            run.retry_at = None;
            run.error = Some(format!(
                "{} v{} の定義が見つかりません。定義を戻すか実行をキャンセルしてください",
                run.workflow, run.version
            ));
            save_run(&self.paths, &mut run)?;
            return Ok(run);
        };
        let def = def.clone();
        self.advance(&mut run, &def).await?;
        Ok(run)
    }

    async fn advance(&self, run: &mut Run, def: &Definition) -> Result<()> {
        if run.finished() || run.status == RunStatus::Waiting {
            return Ok(());
        }
        run.status = RunStatus::Running;
        run.retry_at = None;
        run.error = None;
        save_run(&self.paths, run)?;
        while run.current < def.steps.len() {
            // Another process (app page or CLI) may have cancelled the run mid-step.
            if let Ok(disk) = load_run(&self.paths, &run.id) {
                if disk.status == RunStatus::Cancelled {
                    *run = disk;
                    return Ok(());
                }
            }
            let step = def.steps[run.current].clone();
            match &step {
                Step::Wait { id, duration } => {
                    let waiting = run
                        .steps
                        .iter()
                        .any(|s| s.id == *id && s.status == StepState::Waiting);
                    if waiting {
                        let entry = upsert_step(run, id);
                        entry.status = StepState::Done;
                        entry.output = "待機完了".into();
                        entry.at = now();
                        run.current += 1;
                        save_run(&self.paths, run)?;
                        continue;
                    }
                    let secs = parse_duration(duration).map_err(anyhow::Error::msg)?;
                    let entry = upsert_step(run, id);
                    entry.status = StepState::Waiting;
                    entry.at = now();
                    entry.output = String::new();
                    run.status = RunStatus::Paused;
                    run.retry_at = Some(now() + secs);
                    save_run(&self.paths, run)?;
                    return Ok(());
                }
                Step::Ask { id, question, title, options } => {
                    let payload = &run.payload;
                    let steps = &run.steps;
                    let question = match render(question, payload, steps) {
                        Ok(q) => q,
                        Err(e) => {
                            return self.stop_failed(run, id, format!("質問を組み立てられません: {e:#}"));
                        }
                    };
                    let title = match title.as_deref().map(|t| render(t, payload, steps)).transpose() {
                        Ok(t) => t.unwrap_or_else(|| question.clone()),
                        Err(e) => {
                            return self.stop_failed(run, id, format!("質問を組み立てられません: {e:#}"));
                        }
                    };
                    let entry = upsert_step(run, id);
                    entry.status = StepState::Waiting;
                    entry.at = now();
                    run.question = Some(Question {
                        step: id.clone(),
                        title,
                        options: options.clone(),
                    });
                    run.status = RunStatus::Waiting;
                    run.retry_at = None;
                    save_run(&self.paths, run)?;
                    return Ok(());
                }
                Step::Shell { id, command, timeout } => {
                    let command = match render(command, &run.payload, &run.steps) {
                        Ok(c) => c,
                        Err(e) => return self.stop_failed(run, id, format!("コマンドを組み立てられません: {e:#}")),
                    };
                    {
                        let entry = upsert_step(run, id);
                        entry.status = StepState::Running;
                        entry.at = now();
                        entry.error = None;
                        save_run(&self.paths, run)?;
                    }
                    let timeout = timeout
                        .as_deref()
                        .map(parse_duration)
                        .transpose()
                        .map_err(anyhow::Error::msg)?;
                    match shell(&command, &self.workdir, timeout).await {
                        Ok(output) => {
                            let entry = upsert_step(run, id);
                            entry.status = StepState::Done;
                            entry.output = output;
                            entry.error = None;
                            entry.at = now();
                            run.current += 1;
                            save_run(&self.paths, run)?;
                        }
                        Err(e) => {
                            let delay = {
                                let entry = upsert_step(run, id);
                                entry.attempts += 1;
                                entry.status = StepState::Failed;
                                entry.error = Some(format!("{e:#}"));
                                entry.at = now();
                                retry_delay(entry.attempts)
                            };
                            run.status = RunStatus::Paused;
                            run.retry_at = delay.map(|d| now() + d);
                            run.error = Some(format!("step `{id}` が失敗しました: {e:#}"));
                            save_run(&self.paths, run)?;
                            return Ok(());
                        }
                    }
                }
                Step::Agent {
                    id,
                    prompt,
                    backend,
                    model,
                    project,
                    mode,
                    timeout,
                    title,
                } => {
                    let prompt = match render(prompt, &run.payload, &run.steps) {
                        Ok(p) => p,
                        Err(e) => return self.stop_failed(run, id, format!("プロンプトを組み立てられません: {e:#}")),
                    };
                    let title = match title.as_deref().map(|t| render(t, &run.payload, &run.steps)).transpose() {
                        Ok(t) => t,
                        Err(e) => return self.stop_failed(run, id, format!("タイトルを組み立てられません: {e:#}")),
                    };
                    let timeout = match timeout.as_deref().map(parse_duration).transpose() {
                        Ok(t) => Duration::from_secs(t.unwrap_or(1800)),
                        Err(e) => return self.stop_failed(run, id, format!("timeout: {e}")),
                    };
                    let cwd = self.project_dir(project.as_deref());
                    let mode = parse_mode(mode);
                    {
                        let entry = upsert_step(run, id);
                        entry.status = StepState::Running;
                        entry.at = now();
                        entry.error = None;
                        save_run(&self.paths, run)?;
                    }
                    let request = AgentRequest {
                        workflow: def.name.clone(),
                        step: id.clone(),
                        prompt,
                        backend: backend.clone(),
                        model: model.clone(),
                        title,
                        cwd,
                        mode,
                        timeout,
                    };
                    let fut = (self.agent)(request);
                    match tokio::time::timeout(timeout, fut).await {
                        Ok(Ok(output)) => {
                            let entry = upsert_step(run, id);
                            entry.status = StepState::Done;
                            entry.output = output;
                            entry.error = None;
                            entry.at = now();
                            run.current += 1;
                            save_run(&self.paths, run)?;
                        }
                        Ok(Err(e)) => {
                            // Agent failures pause until the user resumes; they are not retried automatically.
                            let entry = upsert_step(run, id);
                            entry.status = StepState::Failed;
                            entry.error = Some(format!("{e:#}"));
                            entry.at = now();
                            run.status = RunStatus::Paused;
                            run.retry_at = None;
                            run.error = Some(format!("step `{id}` のエージェントが失敗しました: {e:#}"));
                            save_run(&self.paths, run)?;
                            return Ok(());
                        }
                        Err(_) => {
                            return self.stop_failed(run, id, format!("エージェントがタイムアウトしました（{} 秒）", timeout.as_secs()));
                        }
                    }
                }
            }
        }
        if let Ok(disk) = load_run(&self.paths, &run.id) {
            if disk.status == RunStatus::Cancelled {
                *run = disk;
                return Ok(());
            }
        }
        run.status = RunStatus::Done;
        run.question = None;
        run.retry_at = None;
        run.error = None;
        save_run(&self.paths, run)?;
        Ok(())
    }

    fn stop_failed(&self, run: &mut Run, step: &str, message: String) -> Result<()> {
        let entry = upsert_step(run, step);
        entry.status = StepState::Failed;
        entry.error = Some(message.clone());
        entry.at = now();
        run.status = RunStatus::Paused;
        run.retry_at = None;
        run.error = Some(message);
        save_run(&self.paths, run)?;
        Ok(())
    }

    fn project_dir(&self, project: Option<&str>) -> PathBuf {
        match project {
            Some(p) if p == "~" => crate::user_home(),
            Some(p) if p.starts_with("~/") => crate::user_home().join(&p[2..]),
            Some(p) if Path::new(p).is_absolute() => PathBuf::from(p),
            Some(p) => self.workdir.join(p),
            None => self.workdir.clone(),
        }
    }
}

fn upsert_step<'a>(run: &'a mut Run, id: &str) -> &'a mut StepRun {
    if let Some(ix) = run.steps.iter().position(|s| s.id == id) {
        return &mut run.steps[ix];
    }
    run.steps.push(StepRun {
        id: id.into(),
        status: StepState::Pending,
        output: String::new(),
        attempts: 0,
        error: None,
        at: now(),
    });
    run.steps.last_mut().expect("just pushed")
}

fn parse_mode(mode: &str) -> Mode {
    match mode {
        "ask" => Mode::Ask,
        "auto-edit" => Mode::AutoEdit,
        "plan" => Mode::Plan,
        _ => Mode::Full,
    }
}

async fn shell(command: &str, cwd: &Path, timeout: Option<u64>) -> Result<String> {
    let child = tokio::process::Command::new("sh")
        .arg("-c")
        .arg(command)
        .current_dir(cwd)
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true)
        .output();
    let output = match timeout {
        Some(secs) => tokio::time::timeout(Duration::from_secs(secs), child)
            .await
            .map_err(|_| anyhow!("コマンドがタイムアウトしました（{secs} 秒）"))??,
        None => child.await?,
    };
    let stdout = String::from_utf8_lossy(&output.stdout).trim_end().to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).trim_end().to_string();
    if !output.status.success() {
        let detail = if stderr.is_empty() { stdout } else { stderr };
        bail!(
            "終了コード {} {}",
            output.status.code().unwrap_or(-1),
            detail.lines().rev().take(10).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>().join("\n")
        );
    }
    Ok(if stdout.is_empty() { stderr } else { stdout })
}

// ---- triggers ----

#[derive(Default, Serialize, Deserialize)]
struct TriggerState {
    #[serde(default)]
    seen: BTreeMap<String, u64>,
}

impl TriggerState {
    fn path(paths: &Paths) -> PathBuf {
        paths.runs.with_file_name("workflow-triggers.json")
    }

    fn load(paths: &Paths) -> Result<Self> {
        match std::fs::read(Self::path(paths)) {
            Ok(data) => Ok(serde_json::from_slice(&data)?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e.into()),
        }
    }

    fn save(&self, paths: &Paths) -> Result<()> {
        let path = Self::path(paths);
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec(self)?)?;
        std::fs::rename(tmp, path)?;
        Ok(())
    }

    /// Load after locking so the desktop and `serve` cannot both fire the same schedule.
    fn tick(rt: &Runtime, workflows: &[Definition]) -> Result<()> {
        if !workflows.iter().any(|d| d.trigger.every.is_some() || d.trigger.daily.is_some()) {
            return Ok(());
        }
        let lock_path = Self::path(&rt.paths).with_extension("lock");
        std::fs::create_dir_all(lock_path.parent().context("schedule lock has no parent")?)?;
        let lock = std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(false).open(lock_path)?;
        match lock.try_lock() {
            Ok(()) => Self::load(&rt.paths)?.advance(rt, workflows),
            Err(std::fs::TryLockError::WouldBlock) => Ok(()),
            Err(std::fs::TryLockError::Error(e)) => Err(e.into()),
        }
    }

    /// New definitions are armed (not fired) on first sight.
    fn advance(&mut self, rt: &Runtime, workflows: &[Definition]) -> Result<()> {
        let at = now();
        let mut changed = false;
        let mut due = Vec::new();
        for versions in workflows.chunk_by(|a, b| a.name == b.name) {
            let def = &versions[versions.len() - 1];
            let key = format!("{}@{}", def.name, def.version);
            let last = self.seen.get(&key).copied();
            let run = if let Some(every) = &def.trigger.every {
                let Ok(secs) = parse_duration(every) else { continue };
                if last.is_some_and(|last| at < last.saturating_add(secs)) { continue; }
                last.is_some()
            } else if let Some(daily) = &def.trigger.daily {
                let Ok(hhmm) = parse_daily(daily) else { continue };
                if let Some(last) = last {
                    let next = next_daily(last, hhmm);
                    if at < next { continue; }
                    at - next <= 600
                } else {
                    false
                }
            } else {
                continue;
            };
            self.seen.insert(key, at);
            changed = true;
            if run { due.push(def); }
        }
        if changed {
            // shortcut: a crash after saving can miss one run; use a transactional run store if catch-up is needed.
            self.save(&rt.paths)?;
        }
        for def in due {
            if let Err(e) = rt.start(&def.name, json!({}), None, "schedule") {
                eprintln!("ワークフロー {} の定期実行を開始できません: {e:#}", def.name);
            }
        }
        Ok(())
    }
}

/// The long-running server loop: reload definitions, fire schedules, advance due runs, serve workflow
/// webhooks when a port is given. Never returns.
pub async fn orchestrate(cfg: Arc<crate::config::Config>, workdir: PathBuf, webhook_port: Option<u16>) -> ! {
    let rt = Runtime::new(cfg, workdir);
    // Dropping the engine (for example, when Labs is switched off) stops accepting new webhooks.
    let mut webhooks = tokio::task::JoinSet::new();
    if let Some(port) = webhook_port {
        let hooks = rt.clone();
        webhooks.spawn(async move {
            loop {
                if let Err(e) = serve_webhooks(hooks.clone(), port).await {
                    eprintln!("ワークフロー Webhook サーバーを開始できません（{port}）: {e:#}");
                    tokio::time::sleep(Duration::from_secs(10)).await;
                }
            }
        });
    }
    let mut announced = HashSet::new();
    loop {
        let defs = rt.definitions();
        for (path, error) in defs.errors {
            if announced.insert(format!("{}:{error}", path.display())) {
                eprintln!("ワークフロー定義 {}: {error}", path.display());
            }
        }
        if let Err(e) = TriggerState::tick(&rt, &defs.workflows) {
            eprintln!("ワークフローの定期実行を確認できません: {e:#}");
        }
        for id in rt.due_ids() {
            let rt = rt.clone();
            tokio::spawn(async move {
                if let Err(e) = rt.advance_run(&id).await {
                    eprintln!("ワークフロー {id}: {e:#}");
                }
            });
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

/// Webhook path is `/workflows/<secret>`; the secret is the path itself, as in T3.
async fn serve_webhooks(rt: Runtime, port: u16) -> Result<()> {
    crate::webhook::serve_raw(port, move |req| {
        if req.method != "POST" {
            return 404;
        }
        let Some(secret) = req.path.strip_prefix("/workflows/") else {
            return 404;
        };
        let defs = rt.definitions();
        let Some(def) = defs
            .workflows
            .iter()
            .find(|d| d.trigger.webhook.as_deref() == Some(secret))
        else {
            return 404;
        };
        let payload = serde_json::from_slice::<Value>(&req.body)
            .ok()
            .filter(Value::is_object)
            .unwrap_or_else(|| json!({ "body": String::from_utf8_lossy(&req.body) }));
        let delivery = req
            .headers
            .get("x-github-delivery")
            .or_else(|| req.headers.get("x-greencode-delivery"))
            .cloned();
        let key = delivery.map(|d| format!("{}:{d}", def.name));
        match rt.start(&def.name, payload, key, "webhook") {
            Ok(run) => {
                eprintln!("ワークフロー {} を Webhook から開始: {}", def.name, run.id);
                202
            }
            Err(e) => {
                eprintln!("ワークフロー {} を Webhook から開始できません: {e:#}", def.name);
                400
            }
        }
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn def(steps: Vec<Step>) -> Definition {
        Definition {
            name: "test".into(),
            version: 1,
            payload: BTreeMap::new(),
            steps,
            trigger: Trigger::default(),
            source: PathBuf::new(),
        }
    }

    #[test]
    fn retry_delays_follow_five_twenty_eighty_minutes() {
        assert_eq!(retry_delay(1), Some(300));
        assert_eq!(retry_delay(2), Some(1200));
        assert_eq!(retry_delay(3), Some(4800));
        assert_eq!(retry_delay(4), None);
        assert_eq!(retry_delay(0), None);
    }

    #[test]
    fn durations_and_daily_times_parse() {
        assert_eq!(parse_duration("30s"), Ok(30));
        assert_eq!(parse_duration("5m"), Ok(300));
        assert_eq!(parse_duration("2h"), Ok(7200));
        assert_eq!(parse_duration("1d"), Ok(86400));
        assert!(parse_duration("5").is_err());
        assert!(parse_duration("0m").is_err());
        assert_eq!(parse_daily("09:30"), Ok(9 * 3600 + 30 * 60));
        assert!(parse_daily("24:00").is_err());
        assert!(parse_daily("9:30").is_err());
    }

    #[test]
    fn schedule_tick_observes_the_other_engines_lock_and_saved_state() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths {
            workflows: dir.path().join("definitions"),
            runs: dir.path().join("runs"),
            cache: dir.path().join("cache.json"),
        };
        std::fs::create_dir(&paths.workflows).unwrap();
        for version in [1, 2] {
            std::fs::write(paths.workflows.join(format!("test-{version}.toml")), format!(concat!(
                "name = \"test\"\nversion = {}\n[trigger]\nevery = \"1d\"\n",
                "[[steps]]\ntype = \"ask\"\nid = \"review\"\nquestion = \"Ready?\"\noptions = [\"yes\"]\n",
            ), version)).unwrap();
        }
        let rt = Runtime::with_paths(Arc::new(crate::config::Config::default()), dir.path().into(),
            paths.clone(), Arc::new(|_| Box::pin(async { unreachable!() })));
        let defs = rt.definitions();
        assert!(defs.errors.is_empty(), "{:?}", defs.errors);
        let state = TriggerState { seen: BTreeMap::from([("test@1".into(), 0), ("test@2".into(), 0)]) };
        state.save(&paths).unwrap();
        let locked = std::fs::File::create(TriggerState::path(&paths).with_extension("lock")).unwrap();
        locked.lock().unwrap();
        TriggerState::tick(&rt, &defs.workflows).unwrap();
        assert!(rt.runs().is_empty());
        drop(locked);
        TriggerState::tick(&rt, &defs.workflows).unwrap();
        TriggerState::tick(&rt, &defs.workflows).unwrap();
        let runs = rt.runs();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].version, 2);
    }

    #[test]
    fn templates_use_payload_and_earlier_outputs() {
        let steps = vec![StepRun {
            id: "notes".into(),
            status: StepState::Done,
            output: "3 fixes".into(),
            attempts: 1,
            error: None,
            at: 0,
        }];
        let rendered = render(
            "Release {{payload.name}}: {{steps.notes.output}}",
            &json!({"name": "v1.4"}),
            &steps,
        )
        .unwrap();
        assert_eq!(rendered, "Release v1.4: 3 fixes");
        assert!(render("{{steps.missing.output}}", &json!({}), &steps).is_err());
        assert!(render("{{payload.name}}", &json!({}), &steps).is_err());
    }

    #[test]
    fn validation_names_the_problem() {
        let mut bad = def(vec![Step::Shell {
            id: "build".into(),
            command: "echo {{steps.build.output}}".into(),
            timeout: None,
        }]);
        assert!(validate(&bad).unwrap_err().contains("まだ実行されていません"));
        bad.steps = vec![Step::Ask {
            id: "ship".into(),
            question: "Ship?".into(),
            title: None,
            options: vec![],
        }];
        assert!(validate(&bad).unwrap_err().contains("options"));
    }
}
