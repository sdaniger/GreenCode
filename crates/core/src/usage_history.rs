//! Token and cost history per day/model/provider, from GreenCode's own usage log plus Claude Code,
//! Codex and Pi session files. Scanned files are cached by mtime in `<home>/usage-history.json`.

use crate::Usage;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::BTreeMap,
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
    time::UNIX_EPOCH,
};

/// Days of history kept in the cache and shown at most.
pub const RETENTION_DAYS: u32 = 92;
const CACHE_VERSION: u32 = 1;

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Row {
    /// Days since 1970-01-01 (UTC).
    pub day: u32,
    pub provider: String,
    pub model: String,
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    /// Cost the source itself reported (Pi); used only when no price is known.
    #[serde(default)]
    pub reported: f64,
}

/// USD per million tokens. Missing cache rates fall back to the input rate.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Price {
    pub input: f64,
    pub output: f64,
    pub cache_read: Option<f64>,
    pub cache_write: Option<f64>,
}

// shortcut: approximate public list prices; first match wins, so specific names come first. Custom prices override.
const PRICES: &[(&str, [f64; 4])] = &[
    ("opus", [5., 25., 0.5, 6.25]),
    ("sonnet", [3., 15., 0.3, 3.75]),
    ("haiku", [1., 5., 0.1, 1.25]),
    ("gpt-5-mini", [0.25, 2., 0.025, 0.25]),
    ("gpt-5-nano", [0.05, 0.4, 0.005, 0.05]),
    ("gpt-5", [1.25, 10., 0.125, 1.25]),
    ("gpt-4.1-mini", [0.4, 1.6, 0.1, 0.4]),
    ("gpt-4.1", [2., 8., 0.5, 2.]),
    ("gpt-4o-mini", [0.15, 0.6, 0.075, 0.15]),
    ("gpt-4o", [2.5, 10., 1.25, 2.5]),
    ("o4-mini", [1.1, 4.4, 0.275, 1.1]),
    ("o3", [2., 8., 0.5, 2.]),
    ("gemini-2.5-pro", [1.25, 10., 0.31, 1.25]),
    ("gemini-2.5-flash", [0.3, 2.5, 0.075, 0.3]),
];

pub fn builtin_price(model: &str) -> Option<Price> {
    let m = model.to_ascii_lowercase();
    PRICES.iter().find(|(k, _)| m.contains(k)).map(|(_, p)| Price {
        input: p[0],
        output: p[1],
        cache_read: Some(p[2]),
        cache_write: Some(p[3]),
    })
}

#[derive(Clone, Debug, Default)]
pub struct Settings {
    prices: BTreeMap<String, Price>,
    map_to: BTreeMap<String, String>,
    include: Vec<(Option<String>, String)>,
    exclude: Vec<(Option<String>, String)>,
}

fn matchers(v: &Value) -> Vec<(Option<String>, String)> {
    v.as_array()
        .into_iter()
        .flatten()
        .filter_map(|e| Some((e["provider"].as_str().map(str::to_owned), e["model"].as_str()?.to_owned())))
        .collect()
}

impl Settings {
    /// Reads `model_prices` (`{"model": {"input":3,"output":15,"cache_read":0.3,"cache_write":3.75,"map_to":"other"}}`)
    /// and `usage_model_filter` (`{"include":[{"provider":"claude","model":"x"}],"exclude":[..]}`) from ui.json.
    pub fn from_ui(ui: &Value) -> Settings {
        let mut s = Settings::default();
        for (model, e) in ui["model_prices"].as_object().into_iter().flatten() {
            if let Some(to) = e["map_to"].as_str().filter(|t| !t.is_empty()) {
                s.map_to.insert(model.clone(), to.to_owned());
            }
            if let (Some(input), Some(output)) = (e["input"].as_f64(), e["output"].as_f64()) {
                s.prices.insert(model.clone(), Price { input, output, cache_read: e["cache_read"].as_f64(), cache_write: e["cache_write"].as_f64() });
            }
        }
        let f = &ui["usage_model_filter"];
        s.include = matchers(&f["include"]);
        s.exclude = matchers(&f["exclude"]);
        s
    }

    fn keeps(&self, r: &Row) -> bool {
        let hit = |l: &[(Option<String>, String)]| l.iter().any(|(p, m)| *m == r.model && p.as_ref().is_none_or(|p| *p == r.provider));
        (self.include.is_empty() || hit(&self.include)) && !hit(&self.exclude)
    }

    pub fn price(&self, model: &str) -> Option<Price> {
        self.prices.get(model).copied().or_else(|| builtin_price(model))
    }

    /// The model a row is shown under: the filter applies first, then `map_to`.
    fn display<'a>(&'a self, model: &'a str) -> &'a str {
        self.map_to.get(model).map_or(model, String::as_str)
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Totals {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub cost: f64,
    /// What cache reads saved against the input rate.
    pub savings: f64,
    /// Some tokens have neither a price nor a reported cost.
    pub unpriced: bool,
}

impl Totals {
    pub fn tokens(&self) -> u64 {
        self.input + self.output + self.cache_read + self.cache_write
    }

    fn add(&mut self, r: &Row, price: Option<Price>) {
        self.input += r.input;
        self.output += r.output;
        self.cache_read += r.cache_read;
        self.cache_write += r.cache_write;
        match price {
            Some(p) => {
                let cr = p.cache_read.unwrap_or(p.input);
                self.cost += (r.input as f64 * p.input
                    + r.output as f64 * p.output
                    + r.cache_read as f64 * cr
                    + r.cache_write as f64 * p.cache_write.unwrap_or(p.input))
                    / 1e6;
                self.savings += r.cache_read as f64 * (p.input - cr) / 1e6;
            }
            None if r.reported > 0. => self.cost += r.reported,
            None => self.unpriced |= r.input + r.output + r.cache_read + r.cache_write > 0,
        }
    }

    fn merge(&mut self, o: &Totals) {
        self.input += o.input;
        self.output += o.output;
        self.cache_read += o.cache_read;
        self.cache_write += o.cache_write;
        self.cost += o.cost;
        self.savings += o.savings;
        self.unpriced |= o.unpriced;
    }
}

#[derive(Clone, Debug, Default)]
pub struct Summary {
    pub total: Totals,
    /// One entry per day, oldest first, `days` long; empty days are zero.
    pub days: Vec<(u32, Totals)>,
    /// By cost then tokens, largest first.
    pub models: Vec<(String, Totals)>,
}

pub fn summarize(rows: &[Row], cfg: &Settings, days: u32, today: u32) -> Summary {
    let first = (today + 1).saturating_sub(days);
    let mut per_day = vec![Totals::default(); days as usize];
    let mut models: BTreeMap<String, Totals> = BTreeMap::new();
    for r in rows.iter().filter(|r| (first..=today).contains(&r.day) && cfg.keeps(r)) {
        let model = cfg.display(&r.model);
        let price = cfg.price(model);
        per_day[(r.day - first) as usize].add(r, price);
        models.entry(model.to_owned()).or_default().add(r, price);
    }
    let mut total = Totals::default();
    per_day.iter().for_each(|t| total.merge(t));
    let mut models: Vec<_> = models.into_iter().collect();
    models.sort_by(|a, b| b.1.cost.total_cmp(&a.1.cost).then(b.1.tokens().cmp(&a.1.tokens())));
    Summary { total, days: per_day.into_iter().enumerate().map(|(i, t)| (first + i as u32, t)).collect(), models }
}

/// `(year, month, day)` of a day number.
pub fn civil(day: u32) -> (u32, u32, u32) {
    let z = day as i64 + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    ((y + (m <= 2) as i64) as u32, m as u32, d as u32)
}

/// `(month, day)` of a day number.
pub fn month_day(day: u32) -> (u32, u32) {
    let (_, m, d) = civil(day);
    (m, d)
}

fn day_of(secs: u64) -> u32 {
    (secs / 86400) as u32
}

fn day_of_iso(ts: &Value) -> Option<u32> {
    crate::usage::parse_time(ts.as_str()?).map(day_of)
}

#[derive(Default)]
struct Rows(BTreeMap<(u32, String, String), Row>);

impl Rows {
    fn add(&mut self, day: u32, provider: &str, model: &str, u: Usage, reported: f64) {
        if u.input + u.output + u.cache_read + u.cache_write == 0 && reported == 0. {
            return;
        }
        let r = self.0.entry((day, provider.to_owned(), model.to_owned())).or_insert_with(|| Row { day, provider: provider.into(), model: model.into(), ..Default::default() });
        r.input += u.input;
        r.output += u.output;
        r.cache_read += u.cache_read;
        r.cache_write += u.cache_write;
        r.reported += reported;
    }

    fn done(self) -> Vec<Row> {
        self.0.into_values().collect()
    }
}

fn parsed<'a, S: AsRef<str> + 'a>(lines: impl Iterator<Item = S> + 'a, needle: &'a str) -> impl Iterator<Item = Value> + 'a {
    lines.filter_map(move |l| {
        let l = l.as_ref();
        l.contains(needle).then(|| serde_json::from_str::<Value>(l).ok()).flatten()
    })
}

/// Claude Code transcripts repeat a message's usage on every content block; the last line per message id wins.
pub fn parse_claude<S: AsRef<str>>(lines: impl Iterator<Item = S>) -> Vec<Row> {
    let mut last: BTreeMap<String, (u32, String, Usage)> = BTreeMap::new();
    for (n, v) in parsed(lines, "\"usage\"").enumerate() {
        let m = &v["message"];
        let (Some(model), Some(day)) = (m["model"].as_str().filter(|m| !m.starts_with('<')), day_of_iso(&v["timestamp"])) else { continue };
        let u = &m["usage"];
        let get = |k: &str| u[k].as_u64().unwrap_or(0);
        let usage = Usage { input: get("input_tokens"), output: get("output_tokens"), cache_read: get("cache_read_input_tokens"), cache_write: get("cache_creation_input_tokens") };
        let id = m["id"].as_str().map_or_else(|| format!("line{n}"), str::to_owned);
        last.insert(id, (day, model.to_owned(), usage));
    }
    let mut rows = Rows::default();
    for (day, model, u) in last.into_values() {
        rows.add(day, "claude", &model, u, 0.);
    }
    rows.done()
}

/// Codex reports `last_token_usage` per request in `token_count` events (repeated verbatim on retries);
/// its input count includes cached tokens, and the model comes from the preceding `turn_context`.
pub fn parse_codex<S: AsRef<str>>(lines: impl Iterator<Item = S>) -> Vec<Row> {
    let mut rows = Rows::default();
    let (mut model, mut prev_total) = (String::from("unknown"), None);
    for v in parsed(lines, "\"payload\"") {
        let p = &v["payload"];
        match v["type"].as_str() {
            Some("turn_context") => {
                if let Some(m) = p["model"].as_str() {
                    model = m.to_owned();
                }
            }
            Some("event_msg") if p["type"] == "token_count" => {
                let info = &p["info"];
                let total = info["total_token_usage"]["total_tokens"].as_u64();
                if total.is_none() || total == prev_total {
                    continue;
                }
                prev_total = total;
                let (Some(day), u) = (day_of_iso(&v["timestamp"]), &info["last_token_usage"]) else { continue };
                let get = |k: &str| u[k].as_u64().unwrap_or(0);
                let cached = get("cached_input_tokens");
                rows.add(day, "codex", &model, Usage { input: get("input_tokens").saturating_sub(cached), output: get("output_tokens"), cache_read: cached, cache_write: get("cache_write_input_tokens") }, 0.);
            }
            _ => {}
        }
    }
    rows.done()
}

pub fn parse_pi<S: AsRef<str>>(lines: impl Iterator<Item = S>) -> Vec<Row> {
    let mut rows = Rows::default();
    for v in parsed(lines, "\"usage\"") {
        let m = &v["message"];
        let (true, Some(model), Some(day)) = (v["type"] == "message" && m["role"] == "assistant", m["model"].as_str(), day_of_iso(&v["timestamp"])) else { continue };
        let u = &m["usage"];
        let get = |k: &str| u[k].as_u64().unwrap_or(0);
        let usage = Usage { input: get("input"), output: get("output"), cache_read: get("cacheRead"), cache_write: get("cacheWrite") };
        rows.add(day, m["provider"].as_str().unwrap_or("pi"), model, usage, u["cost"]["total"].as_f64().unwrap_or(0.));
    }
    rows.done()
}

/// GreenCode's own requests, logged by [`record`]: `{"at":secs,"provider":..,"model":..,"input":..}`.
pub fn parse_native<S: AsRef<str>>(lines: impl Iterator<Item = S>) -> Vec<Row> {
    let mut rows = Rows::default();
    for v in parsed(lines, "\"at\"") {
        let (Some(at), Some(model)) = (v["at"].as_u64(), v["model"].as_str()) else { continue };
        let get = |k: &str| v[k].as_u64().unwrap_or(0);
        rows.add(day_of(at), v["provider"].as_str().unwrap_or("greencode"), model, Usage { input: get("input"), output: get("output"), cache_read: get("cache_read"), cache_write: get("cache_write") }, 0.);
    }
    rows.done()
}

fn now() -> u64 {
    std::time::SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs()
}

fn log_dir() -> PathBuf {
    crate::home().join("usage-log")
}

/// Appends one model call to the monthly log; best effort, usage must never fail a request.
pub fn record(provider: &str, model: &str, u: &Usage) {
    if u.input + u.output + u.cache_read + u.cache_write == 0 {
        return;
    }
    let at = now();
    let (y, m, _) = civil(day_of(at));
    // Month files keep the incremental rescan small: only the current one changes.
    let path = log_dir().join(format!("{y}-{m:02}.jsonl"));
    let line = serde_json::json!({"at": at, "provider": provider, "model": model, "input": u.input, "output": u.output, "cache_read": u.cache_read, "cache_write": u.cache_write});
    let _ = std::fs::create_dir_all(log_dir()).and_then(|_| std::fs::OpenOptions::new().create(true).append(true).open(path)).and_then(|mut f| writeln!(f, "{line}"));
}

#[derive(Serialize, Deserialize)]
struct FileEntry {
    mtime: u64,
    len: u64,
    rows: Vec<Row>,
}

#[derive(Default, Serialize, Deserialize)]
struct Cache {
    version: u32,
    files: BTreeMap<String, FileEntry>,
}

#[derive(Clone, Copy)]
enum Kind {
    Native,
    Claude,
    Codex,
    Pi,
}

fn env_dir(var: &str) -> Option<PathBuf> {
    std::env::var_os(var).filter(|v| !v.is_empty()).map(PathBuf::from)
}

fn sources() -> Vec<(PathBuf, Kind)> {
    let home = crate::user_home();
    let pi = env_dir("PI_CODING_AGENT_SESSION_DIR")
        .or_else(|| env_dir("PI_CODING_AGENT_DIR").map(|d| d.join("sessions")))
        .unwrap_or_else(|| home.join(".pi/agent/sessions"));
    vec![
        (log_dir(), Kind::Native),
        (env_dir("CLAUDE_CONFIG_DIR").unwrap_or_else(|| home.join(".claude")).join("projects"), Kind::Claude),
        (env_dir("CODEX_HOME").unwrap_or_else(|| home.join(".codex")).join("sessions"), Kind::Codex),
        (pi, Kind::Pi),
    ]
}

fn walk(dir: &Path, f: &mut dyn FnMut(&Path)) {
    for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let p = e.path();
        match e.file_type() {
            Ok(t) if t.is_dir() => walk(&p, f),
            Ok(_) if p.extension().is_some_and(|x| x == "jsonl") => f(&p),
            _ => {}
        }
    }
}

/// Loads every source, re-reading only files whose mtime or size changed, and returns the last 92 days.
/// Cached rows of files that were since deleted (Claude prunes its transcripts) are kept until they age out.
pub fn scan() -> Vec<Row> {
    let cutoff = day_of(now()).saturating_sub(RETENTION_DAYS - 1);
    let cache_path = crate::home().join("usage-history.json");
    let mut cache = std::fs::read(&cache_path)
        .ok()
        .and_then(|b| serde_json::from_slice::<Cache>(&b).ok())
        .filter(|c| c.version == CACHE_VERSION)
        .unwrap_or(Cache { version: CACHE_VERSION, files: BTreeMap::new() });
    let mut dirty = false;
    for (root, kind) in sources() {
        walk(&root, &mut |p| {
            let Ok(md) = p.metadata() else { return };
            let mtime = md.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map_or(0, |d| d.as_secs());
            let key = p.to_string_lossy().into_owned();
            if day_of(mtime) < cutoff || cache.files.get(&key).is_some_and(|e| e.mtime == mtime && e.len == md.len()) {
                return;
            }
            let Ok(f) = std::fs::File::open(p) else { return };
            let lines = BufReader::new(f).lines().map_while(Result::ok);
            let rows = match kind {
                Kind::Native => parse_native(lines),
                Kind::Claude => parse_claude(lines),
                Kind::Codex => parse_codex(lines),
                Kind::Pi => parse_pi(lines),
            };
            cache.files.insert(key, FileEntry { mtime, len: md.len(), rows });
            dirty = true;
        });
    }
    for e in cache.files.values_mut() {
        let n = e.rows.len();
        e.rows.retain(|r| r.day >= cutoff);
        dirty |= e.rows.len() != n;
    }
    let n = cache.files.len();
    cache.files.retain(|k, e| !e.rows.is_empty() || Path::new(k).exists());
    if dirty || cache.files.len() != n {
        let tmp = cache_path.with_extension("json.tmp");
        let _ = serde_json::to_vec(&cache).map_err(std::io::Error::other).and_then(|b| std::fs::write(&tmp, b)).and_then(|_| std::fs::rename(&tmp, &cache_path));
    }
    cache.files.into_values().flat_map(|e| e.rows).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_dedupes_message_ids_and_skips_synthetic() {
        let lines = [
            r#"{"timestamp":"2026-11-13T11:41:00.711Z","message":{"id":"m1","model":"claude-sonnet-4-5","usage":{"input_tokens":2,"cache_creation_input_tokens":100,"cache_read_input_tokens":1000,"output_tokens":4}}}"#,
            r#"{"timestamp":"2026-11-13T11:41:01.000Z","message":{"id":"m1","model":"claude-sonnet-4-5","usage":{"input_tokens":2,"cache_creation_input_tokens":100,"cache_read_input_tokens":1000,"output_tokens":40}}}"#,
            r#"{"timestamp":"2026-11-13T11:42:00Z","message":{"id":"m2","model":"<synthetic>","usage":{"input_tokens":0,"output_tokens":0}}}"#,
            "not json \"usage\"",
        ];
        let rows = parse_claude(lines.iter());
        assert_eq!(rows.len(), 1);
        let r = &rows[0];
        assert_eq!((r.input, r.output, r.cache_read, r.cache_write), (2, 40, 1000, 100));
        assert_eq!(month_day(r.day), (11, 13));
    }

    #[test]
    fn codex_uses_turn_model_and_ignores_repeated_totals() {
        let tc = |total: u64, input: u64, cached: u64| format!(r#"{{"timestamp":"2026-11-16T02:01:48.021Z","type":"event_msg","payload":{{"type":"token_count","info":{{"total_token_usage":{{"total_tokens":{total}}},"last_token_usage":{{"input_tokens":{input},"cached_input_tokens":{cached},"output_tokens":10}}}}}}}}"#);
        let lines = vec![
            r#"{"timestamp":"2026-11-16T02:01:40.151Z","type":"turn_context","payload":{"model":"gpt-5"}}"#.to_string(),
            r#"{"timestamp":"2026-11-16T02:01:41Z","type":"event_msg","payload":{"type":"token_count","info":null}}"#.to_string(),
            tc(110, 100, 60),
            tc(110, 100, 60),
            tc(250, 120, 0),
        ];
        let rows = parse_codex(lines.iter());
        assert_eq!(rows.len(), 1);
        let r = &rows[0];
        assert_eq!((r.model.as_str(), r.input, r.cache_read, r.output), ("gpt-5", 40 + 120, 60, 20));
    }

    #[test]
    fn pi_and_native_lines() {
        let pi = r#"{"type":"message","timestamp":"2026-10-13T07:54:48.603Z","message":{"role":"assistant","provider":"openai-codex","model":"gpt-6.1-sol","usage":{"input":9837,"output":22,"cacheRead":5,"cacheWrite":0,"cost":{"total":0.0199}}}}"#;
        let rows = parse_pi([pi].iter());
        assert_eq!((rows[0].provider.as_str(), rows[0].input, rows[0].cache_read, rows[0].reported), ("openai-codex", 9837, 5, 0.0199));
        let native = r#"{"at":86400,"provider":"anthropic","model":"claude-opus-4-5","input":1,"output":2,"cache_read":3,"cache_write":4}"#;
        let rows = parse_native([native].iter());
        assert_eq!((rows[0].day, rows[0].output, rows[0].cache_write), (1, 2, 4));
    }

    #[test]
    fn cost_prices_aliases_and_filters() {
        let row = |model: &str, day: u32| Row { day, provider: "claude".into(), model: model.into(), input: 1_000_000, output: 1_000_000, cache_read: 1_000_000, cache_write: 0, reported: 0. };
        let rows = [row("claude-sonnet-4-5", 100), row("preview-x", 99), row("claude-haiku-5-5", 100), row("old", 10), row("mystery", 100)];
        let ui = serde_json::json!({
            "model_prices": {"preview-x": {"map_to": "claude-sonnet-4-5"}, "mystery": {"input": 2.0, "output": 4.0}},
            "usage_model_filter": {"exclude": [{"model": "claude-haiku-5-5"}]},
        });
        let s = summarize(&rows, &Settings::from_ui(&ui), 7, 100);
        // sonnet 3 + 15 + 0.3 per row, twice (preview-x is mapped onto it), mystery 2 + 4 + 2 (cache falls back to input)
        let sonnet = &s.models.iter().find(|m| m.0 == "claude-sonnet-4-5").unwrap().1;
        assert!((sonnet.cost - 2. * 18.3).abs() < 1e-9);
        assert!((sonnet.savings - 2. * 2.7).abs() < 1e-9);
        assert!((s.total.cost - (36.6 + 8.)).abs() < 1e-9);
        assert_eq!(s.models.len(), 2, "excluded and out-of-range models vanish");
        assert_eq!((s.days.len(), s.days[6].0), (7, 100));
        assert!(!s.total.unpriced);
        let unpriced = summarize(&[row("gpt-6.1-sol", 100)], &Settings::default(), 7, 100);
        assert!(unpriced.total.unpriced && unpriced.total.cost == 0.);
    }
}
