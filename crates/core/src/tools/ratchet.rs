//! `ratchet`: an eval flow with a baseline. A flow is cases (command + expected substring) stored in
//! `.greencode/ratchet/<flow>.json`; every `run` appends a score and says whether it beat the baseline.
//! `/ratchet` in the CLI turns the prelude on for the session and starts the hill climb.
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

fn dir(cwd: &Path) -> PathBuf {
    cwd.join(".greencode/ratchet")
}

fn flow_path(cwd: &Path, flow: &str) -> PathBuf {
    dir(cwd).join(format!("{}.json", sanitize(flow)))
}

fn sanitize(flow: &str) -> String {
    let name: String = flow
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' { c } else { '-' })
        .collect();
    let name = name.trim_matches('.').to_string();
    if name.is_empty() { "default".into() } else { name }
}

fn read_flow(cwd: &Path, flow: &str) -> Result<Value> {
    let path = flow_path(cwd, flow);
    Ok(serde_json::from_slice(&std::fs::read(&path).with_context(|| format!("no ratchet flow `{flow}`; run `init` first"))?)?)
}

fn append(cwd: &Path, flow: &str, record: &Value) -> Result<()> {
    use std::io::Write;
    let dir = dir(cwd);
    std::fs::create_dir_all(&dir)?;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join(format!("{}.runs.jsonl", sanitize(flow))))?;
    writeln!(file, "{}", serde_json::to_string(record)?)?;
    Ok(())
}

fn runs(cwd: &Path, flow: &str) -> Vec<Value> {
    std::fs::read_to_string(dir(cwd).join(format!("{}.runs.jsonl", sanitize(flow))))
        .map(|text| text.lines().filter_map(|l| serde_json::from_str(l).ok()).collect())
        .unwrap_or_default()
}

/// `init`: store the cases and a zero baseline.
fn init(cwd: &Path, args: &Value) -> Result<String> {
    let flow = args["flow"].as_str().unwrap_or("default");
    let cases: Vec<Value> = args["cases"]
        .as_array()
        .context("init needs a `cases` array")?
        .iter()
        .map(|case| {
            let command = case["command"].as_str().context("every case needs a `command`")?;
            Ok(json!({
                "name": case["name"].as_str().unwrap_or(command),
                "command": command,
                "expect": case["expect"].as_str(),
                "timeout": case["timeout"].as_u64().unwrap_or(120),
            }))
        })
        .collect::<Result<_>>()?;
    if cases.is_empty() {
        bail!("init needs at least one case");
    }
    let path = flow_path(cwd, flow);
    std::fs::create_dir_all(path.parent().unwrap())?;
    std::fs::write(&path, serde_json::to_vec_pretty(&json!({"flow": flow, "cases": cases, "baseline": 0.0}))?)?;
    Ok(format!("flow `{flow}`: {} case(s) in {}; run it with `run`.", cases.len(), path.display()))
}

/// Runs every case; a case passes when its `expect` substring is in the output, or when it exits 0 with no `expect`.
fn run_flow(cwd: &Path, args: &Value) -> Result<String> {
    let flow = args["flow"].as_str().unwrap_or("default");
    let stored = read_flow(cwd, flow)?;
    let cases = stored["cases"].as_array().cloned().unwrap_or_default();
    let baseline = stored["baseline"].as_f64().unwrap_or(0.0);
    let mut rows = Vec::new();
    let mut passed = 0usize;
    for case in &cases {
        let command = case["command"].as_str().unwrap_or_default();
        let timeout = case["timeout"].as_u64().unwrap_or(120).clamp(1, 600);
        let expect = case["expect"].as_str();
        let output = std::process::Command::new("sh")
            .arg("-c")
            .arg(command)
            .current_dir(cwd)
            .stdin(std::process::Stdio::null())
            .output()
            .with_context(|| format!("cannot run `{command}`"))?;
        let text = format!("{}{}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
        let ok = match expect {
            Some(expect) => text.contains(expect),
            None => output.status.success(),
        };
        let _ = timeout;
        if ok { passed += 1; }
        let mut line = format!("{} {}\t{}", if ok { "PASS" } else { "FAIL" }, case["name"].as_str().unwrap_or(command), command);
        if !ok {
            let tail: String = text.chars().rev().take(300).collect::<String>().chars().rev().collect();
            line.push_str(&format!("\n  {}", tail.trim().replace('\n', "\n  ")));
        }
        rows.push(line);
    }
    let score = passed as f64 / cases.len().max(1) as f64;
    let record = json!({"score": score, "passed": passed, "total": cases.len(), "at": now()});
    append(cwd, flow, &record)?;
    let mut doc = stored.clone();
    doc["baseline"] = json!(baseline.max(score));
    std::fs::write(flow_path(cwd, flow), serde_json::to_vec_pretty(&doc)?)?;
    let verdict = if score > baseline + f64::EPSILON {
        format!("new best {:.0}% (baseline was {:.0}%)", score * 100.0, baseline * 100.0)
    } else if (score - baseline).abs() < f64::EPSILON {
        format!("matches the baseline {:.0}%", baseline * 100.0)
    } else {
        format!("regressed: {:.0}% vs baseline {:.0}%", score * 100.0, baseline * 100.0)
    };
    rows.push(format!("\npassed {passed}/{} — {verdict}", cases.len()));
    Ok(rows.join("\n"))
}

fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs()
}

fn status(cwd: &Path, args: &Value) -> Result<String> {
    let flow = args["flow"].as_str().unwrap_or("default");
    let stored = read_flow(cwd, flow)?;
    let runs = runs(cwd, flow);
    let best = runs.iter().filter_map(|r| r["score"].as_f64()).fold(stored["baseline"].as_f64().unwrap_or(0.0), f64::max);
    let mut rows = vec![format!(
        "flow `{flow}`: {} case(s), baseline {:.0}%, best {:.0}%, {} run(s)",
        stored["cases"].as_array().map_or(0, Vec::len),
        stored["baseline"].as_f64().unwrap_or(0.0) * 100.0,
        best * 100.0,
        runs.len()
    )];
    for case in stored["cases"].as_array().into_iter().flatten() {
        rows.push(format!("case {}", case["name"].as_str().unwrap_or_default()));
    }
    for run in runs.iter().rev().take(5) {
        rows.push(format!(
            "{}  {}/{}  {:.0}%",
            run["at"].as_u64().unwrap_or(0),
            run["passed"].as_u64().unwrap_or(0),
            run["total"].as_u64().unwrap_or(0),
            run["score"].as_f64().unwrap_or(0.0) * 100.0
        ));
    }
    Ok(rows.join("\n"))
}

/// `accept`: the last run becomes the baseline (for example after an intentional output change).
fn accept(cwd: &Path, args: &Value) -> Result<String> {
    let flow = args["flow"].as_str().unwrap_or("default");
    let mut stored = read_flow(cwd, flow)?;
    let last = runs(cwd, flow).pop().context("no run recorded yet")?;
    stored["baseline"] = json!(last["score"].as_f64().unwrap_or(0.0));
    std::fs::write(flow_path(cwd, flow), serde_json::to_vec_pretty(&stored)?)?;
    Ok(format!("baseline for `{flow}` is now {:.0}%", last["score"].as_f64().unwrap_or(0.0) * 100.0))
}

pub fn run(cwd: &Path, args: &Value) -> Result<String> {
    match args["action"].as_str().context("ratchet needs an `action`")? {
        "init" => init(cwd, args),
        "run" => run_flow(cwd, args),
        "status" => status(cwd, args),
        "accept" => accept(cwd, args),
        other => bail!("unknown ratchet action `{other}`; use init, run, status or accept"),
    }
}

/// The prelude `/ratchet` prepends to the user's goal for the session.
pub fn prelude(goal: &str) -> String {
    format!(
        "RATCHET MODE. Goal: {goal}\n\
         Hill-climb with the `ratchet` ext tool: `init` the flow from the goal's cases (each a command and the \
         substring its correct output contains), then loop `run` → change the code → `run`. Keep a change only when \
         the score rises above the baseline; revert it otherwise. `status` shows the history, and `accept` sets a \
         deliberate new baseline. Report the score after each run."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn init_run_status_accept() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path();
        let init = run(cwd, &json!({"action": "init", "flow": "demo", "cases": [
            {"name": "ok", "command": "printf good", "expect": "good"},
            {"name": "bad", "command": "printf bad", "expect": "good"},
        ]})).unwrap();
        assert!(init.contains("2 case(s)"), "{init}");
        let run1 = run(cwd, &json!({"action": "run", "flow": "demo"})).unwrap();
        assert!(run1.contains("PASS ok") && run1.contains("FAIL bad"), "{run1}");
        assert!(run1.contains("new best 50%"), "{run1}");
        let again = run(cwd, &json!({"action": "run", "flow": "demo"})).unwrap();
        assert!(again.contains("matches the baseline 50%"), "{again}");
        let status = run(cwd, &json!({"action": "status", "flow": "demo"})).unwrap();
        assert!(status.contains("2 run(s)") && status.contains("baseline 50%"), "{status}");
        std::fs::write(cwd.join("fixed"), "good").unwrap();
        std::fs::write(
            cwd.join(".greencode/ratchet/demo.json"),
            serde_json::to_vec(&json!({"flow": "demo", "baseline": 0.0, "cases": [
                {"name": "ok", "command": "cat fixed", "expect": "good"}
            ]})).unwrap(),
        )
        .unwrap();
        let better = run(cwd, &json!({"action": "run", "flow": "demo"})).unwrap();
        assert!(better.contains("new best 100%"), "{better}");
        assert!(run(cwd, &json!({"action": "accept", "flow": "demo"})).unwrap().contains("100%"));
        assert!(run(cwd, &json!({"action": "bogus"})).is_err());
        assert!(run(cwd, &json!({"action": "run", "flow": "missing"})).is_err());
        assert!(prelude("raise coverage").starts_with("RATCHET MODE."));
    }
}
