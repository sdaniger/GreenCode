//! Live job widgets: one model-written spec and caption per background job, rendered from its log on every
//! draw (updates cost no model calls). Shown by `wait` (given the job's log) and `/jobs`.

use crate::config::Config;
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{LazyLock, Mutex},
};

/// What the widget tracks; the model picks it once, the values come from the log.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub enum Kind {
    /// A percentage or `x/y` figure in the output.
    #[default]
    Progress,
    /// Pass/fail counts.
    Counts,
    /// The last non-empty output line as a status.
    Tail,
}

#[derive(Clone, Debug, Default)]
pub struct Widget {
    pub caption: Option<String>,
    pub kind: Kind,
}

/// One widget per log path; started when the job starts, kept until the process ends.
static WIDGETS: LazyLock<Mutex<HashMap<PathBuf, Widget>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

fn role(cfg: &Config) -> String {
    cfg.model_roles
        .get("widget")
        .or_else(|| cfg.model_roles.get("smol"))
        .cloned()
        .unwrap_or_else(|| cfg.model.clone())
}

/// The widest bar the widget draws; labels stay readable in the pill.
const BAR: usize = 20;

/// Derives a widget from the log's tail, for when no model answered or the model named no usable signal.
pub fn derive(output: &str, widget: Option<&Widget>) -> Vec<String> {
    let kind = widget.map(|w| w.kind).unwrap_or_else(|| infer(output));
    let last = output.lines().rev().find(|l| !l.trim().is_empty()).map(str::trim).unwrap_or("");
    let label = widget.and_then(|w| w.caption.as_deref()).unwrap_or("");
    let line = match kind {
        Kind::Progress => match figure(output) {
            Some((done, total)) => {
                let filled = (done * BAR).checked_div(total).unwrap_or(0).min(BAR);
                format!("[{}{}] {done}/{total}", "█".repeat(filled), "░".repeat(BAR - filled))
            }
            None => format!("… {last}"),
        },
        Kind::Counts => {
            let (pass, fail) = counts(output);
            format!("✓ {pass}  ✗ {fail}")
        }
        Kind::Tail => last.chars().take(80).collect(),
    };
    let mut lines = Vec::with_capacity(2);
    if !label.is_empty() {
        lines.push(label.to_string());
    }
    lines.push(line);
    lines
}

/// Percent or `x/y` in the output, last occurrence first: `42%` becomes `(42, 100)`.
fn figure(output: &str) -> Option<(usize, usize)> {
    for line in output.lines().rev() {
        if let Some((done, total)) = line.split_once('/').and_then(|(a, b)| {
            let n = |s: &str| s.split_whitespace().last().and_then(|w| w.trim_matches(|c: char| !c.is_ascii_digit()).parse::<usize>().ok());
            Some((n(a)?, n(b)?))
        }) {
            if total > 0 && done <= total.saturating_mul(4) {
                return Some((done, total));
            }
        }
        if let Some(at) = line.rfind('%') {
            let digits: String = line[..at].chars().rev().take_while(char::is_ascii_digit).collect();
            let done: String = digits.chars().rev().collect();
            if let Ok(done) = done.parse::<usize>() {
                if done <= 100 {
                    return Some((done, 100));
                }
            }
        }
    }
    None
}

fn counts(output: &str) -> (usize, usize) {
    // Cargo, pytest and jest all print a summary line; count what they say, once.
    let mut pass = 0;
    let mut fail = 0;
    for line in output.lines() {
        let words: Vec<&str> = line.split_whitespace().collect();
        for pair in words.windows(2) {
            let n = pair[0].parse::<usize>().ok();
            match pair[1].trim_end_matches(|c: char| !c.is_ascii_alphabetic()) {
                "passed" | "passing" | "ok" => pass = n.unwrap_or(pass),
                "failed" | "failing" | "errors" => fail = n.unwrap_or(fail),
                _ => {}
            }
        }
    }
    (pass, fail)
}

fn infer(output: &str) -> Kind {
    if figure(output).is_some() {
        Kind::Progress
    } else if counts(output) != (0, 0) {
        Kind::Counts
    } else {
        Kind::Tail
    }
}

/// Reads the log's tail; a running job's log grows, so only the end is read.
pub fn log_tail(path: &Path) -> String {
    const TAIL: usize = 16 * 1024;
    let Ok(bytes) = std::fs::read(path) else { return String::new() };
    let start = bytes.len().saturating_sub(TAIL);
    String::from_utf8_lossy(&bytes[start..]).into_owned()
}

/// The widget lines for a job, derived from its current output.
pub fn render(log: &Path) -> Vec<String> {
    if !crate::tools::config().bash_opts.r#async.widgets {
        return Vec::new();
    }
    let widget = WIDGETS.lock().unwrap().get(log).cloned();
    derive(&log_tail(log), widget.as_ref())
}

/// The caption for the jobs pill: one line on what the job does.
pub fn caption(log: &Path) -> Option<String> {
    WIDGETS.lock().unwrap().get(log).and_then(|w| w.caption.clone())
}

/// Renders a widget line appended to `wait`'s output.
pub fn wait_card(log: &Path) -> String {
    render(log).join("\n")
}

/// The jobs pill's text: `2 jobs · sync · tests`.
pub fn pill() -> Option<String> {
    let jobs = crate::tools::running_job_list();
    if jobs.is_empty() || !crate::tools::config().bash_opts.r#async.widgets {
        return None;
    }
    let mut captions: Vec<String> = jobs
        .iter()
        .filter_map(|(.., log)| caption(log))
        .take(3)
        .collect();
    if captions.is_empty() {
        captions = jobs.iter().take(3).map(|(_, command, _)| short(command)).collect();
    }
    Some(format!("{} job{}  ·  {}", jobs.len(), if jobs.len() == 1 { "" } else { "s" }, captions.join("  ·  ")))
}

/// The jobs sheet: one block per running job with its widget and log path.
pub fn sheet() -> String {
    let jobs = crate::tools::running_job_list();
    let mut out = Vec::new();
    for (pid, command, log) in jobs {
        let mut block = format!("pid {pid}  {}\n{}", short(&command), log.display());
        let widget = render(&log);
        if !widget.is_empty() {
            block.push('\n');
            block.push_str(&widget.join("\n"));
        }
        out.push(block);
    }
    out.join("\n\n")
}

/// One line on what a command does: its model caption, else the first line, capped.
fn short(command: &str) -> String {
    command.lines().next().unwrap_or_default().chars().take(72).collect()
}

/// The model's answer: a caption and a tracked signal; both are optional.
pub fn parse(text: &str) -> Widget {
    let json = text.trim().trim_start_matches("```json").trim_start_matches("```").trim_end_matches("```").trim();
    let value: Option<Value> = serde_json::from_str(json).ok();
    let value = value.unwrap_or_else(|| json!({ "caption": text.lines().next().unwrap_or("") }));
    let kind = match value["spec"].as_str().or_else(|| value["kind"].as_str()).map(str::to_ascii_lowercase).as_deref() {
        Some("progress" | "percent" | "bar") => Kind::Progress,
        Some("counts" | "tests" | "pass_fail") => Kind::Counts,
        Some("tail" | "status" | "lines") => Kind::Tail,
        _ => Kind::default(),
    };
    let caption = value["caption"]
        .as_str()
        .map(|c| c.lines().next().unwrap_or("").trim().chars().take(72).collect::<String>())
        .filter(|c| !c.is_empty());
    Widget { caption, kind }
}

/// Starts the one model call a job gets; nothing is awaited by the job itself.
pub fn start(cfg: &Config, command: &str, log: &Path) {
    if !cfg.bash_opts.r#async.widgets {
        return;
    }
    let cfg = cfg.clone();
    let model = role(&cfg);
    let log = log.to_path_buf();
    let command = command.to_string();
    WIDGETS.lock().unwrap().insert(log.clone(), Widget::default());
    tokio::spawn(async move {
        let system = "You label a background shell command for a status widget. Reply with JSON only: \
            {\"caption\": \"one line, at most 60 characters, on what the command does\", \
             \"spec\": one of \"progress\", \"counts\", \"tail\"}. \
             progress tracks a percentage or x/y figure, counts tracks pass/fail test counts, tail shows the last output line.";
        let prompt = format!("Command: {command}");
        if let Ok(text) = crate::textgen::generate(&cfg, &model, system, &prompt, 120).await {
            let widget = parse(&text);
            if let Some(slot) = WIDGETS.lock().unwrap().get_mut(&log) {
                *slot = widget;
            }
        }
    });
}

/// Drops a finished job's widget, so the map does not grow with the session.
pub fn finish(log: &Path) {
    WIDGETS.lock().unwrap().remove(log);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derives_progress_bar_from_percent_and_fractions() {
        let lines = derive("downloading\n  57%\n", None);
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("57/100"), "{}", lines[0]);
        let lines = derive("make: 3/10 targets\n", None);
        assert!(lines[0].contains("3/10"));
    }

    #[test]
    fn derives_test_counts_and_tail() {
        assert_eq!(derive("test result: ok. 12 passed; 2 failed;\n", None), ["✓ 12  ✗ 2"]);
        assert_eq!(derive("Compiling core\nwarning: unused import\n", None), ["warning: unused import"]);
    }

    #[test]
    fn model_spec_chooses_the_tracked_signal_and_caption() {
        let widget = parse("{\"caption\": \"Runs the parser tests\", \"spec\": \"counts\"}");
        assert_eq!(widget.kind, Kind::Counts);
        assert_eq!(widget.caption.as_deref(), Some("Runs the parser tests"));
        let lines = derive("42%\n7 passed; 0 failed\n", Some(&widget));
        assert_eq!(lines, ["Runs the parser tests", "✓ 7  ✗ 0"]);
        // An unparsable answer keeps the first line as its caption and the derived signal.
        let widget = parse("Runs a long build");
        assert_eq!(widget.caption.as_deref(), Some("Runs a long build"));
        assert_eq!(widget.kind, Kind::Progress);
    }

    #[test]
    fn render_reads_the_log_tail() {
        let dir = std::env::temp_dir().join(format!("greencode-jobwidget-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let log = dir.join("job.log");
        std::fs::write(&log, "step 1/4\n").unwrap();
        WIDGETS.lock().unwrap().insert(log.clone(), Widget { caption: Some("Sync".into()), kind: Kind::Progress });
        let mut cfg = Config::default();
        cfg.bash_opts.r#async.widgets = true;
        crate::tools::configure(&cfg);
        assert_eq!(render(&log), ["Sync", "[█████░░░░░░░░░░░░░░░] 1/4"]);
        assert_eq!(caption(&log).as_deref(), Some("Sync"));
        let _ = std::fs::remove_dir_all(dir);
    }
}
