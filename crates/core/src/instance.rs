//! `task.crossInstance`: GreenCode instances sharing a folder list themselves in
//! `.greencode/instances.json`, append messages to `.greencode/agents.jsonl`, and every instance
//! picks up what is addressed to it.
use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct Instance {
    pub pid: u32,
    pub session: String,
    /// Running subagent names in that instance, kept fresh by its own registration.
    #[serde(default)]
    pub agents: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct Message {
    pub from: String,
    pub to: String,
    pub text: String,
    #[serde(default)]
    pub ts: u64,
}

fn registry(dir: &Path) -> PathBuf { dir.join(".greencode/instances.json") }
fn mailbox(dir: &Path) -> PathBuf { dir.join(".greencode/agents.jsonl") }

fn pid_alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        std::process::Command::new("kill").args(["-0", &pid.to_string()]).stderr(std::process::Stdio::null()).status().is_ok_and(|s| s.success())
    }
    #[cfg(not(unix))]
    { pid != 0 }
}

/// Registers this instance (replacing its own earlier entry) and drops entries whose process is gone.
pub fn register(dir: &Path, session: &str, agents: &[String]) -> Result<()> {
    let mut list = list(dir);
    list.retain(|i| i.session != session && pid_alive(i.pid));
    list.push(Instance { pid: std::process::id(), session: session.to_string(), agents: agents.to_vec() });
    let path = registry(dir);
    if let Some(parent) = path.parent() { std::fs::create_dir_all(parent)?; }
    std::fs::write(path, serde_json::to_vec_pretty(&list)?)?;
    Ok(())
}

pub fn list(dir: &Path) -> Vec<Instance> {
    std::fs::read(registry(dir)).ok()
        .and_then(|bytes| serde_json::from_slice::<Vec<Instance>>(&bytes).ok())
        .unwrap_or_default()
        .into_iter()
        .filter(|i| pid_alive(i.pid))
        .collect()
}

/// Removes this instance's entry when its session ends.
pub fn unregister(dir: &Path, session: &str) {
    let mut list = list(dir);
    list.retain(|i| i.session != session);
    if let Ok(bytes) = serde_json::to_vec_pretty(&list) { let _ = std::fs::write(registry(dir), bytes); }
}

/// Appends one message; `to` is a session id, or `all` for every instance in the folder.
pub fn send(dir: &Path, from: &str, to: &str, text: &str) -> Result<()> {
    let message = Message {
        from: from.into(),
        to: to.into(),
        text: text.into(),
        ts: std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0),
    };
    let path = mailbox(dir);
    if let Some(parent) = path.parent() { std::fs::create_dir_all(parent)?; }
    let mut line = serde_json::to_string(&message)?;
    line.push('\n');
    use std::io::Write;
    std::fs::OpenOptions::new().create(true).append(true).open(path)?.write_all(line.as_bytes())?;
    Ok(())
}

/// Messages addressed to `session` (or `all`) that this instance has not seen yet; the mailbox keeps
/// the last delivered offset in `.greencode/agents.offset`.
pub fn take_inbox(dir: &Path, session: &str) -> Vec<Message> {
    let Ok(text) = std::fs::read_to_string(mailbox(dir)) else { return Vec::new() };
    let offset_path = dir.join(".greencode/agents.offset");
    let key = format!("{session}:");
    let seen: usize = std::fs::read_to_string(&offset_path).ok()
        .and_then(|all| all.lines().find_map(|line| line.strip_prefix(&key).and_then(|n| n.parse().ok())))
        .unwrap_or(0);
    let mut taken = Vec::new();
    let mut end = 0;
    for (index, line) in text.lines().enumerate() {
        end = index + 1;
        if index < seen { continue; }
        let Ok(message) = serde_json::from_str::<Message>(line) else { continue };
        if message.from == session { continue; }
        if message.to == session || message.to == "all" { taken.push(message); }
    }
    if end > seen {
        let all = std::fs::read_to_string(&offset_path).unwrap_or_default();
        let mut kept: Vec<String> = all.lines().filter(|l| !l.starts_with(&key)).map(str::to_string).collect();
        kept.push(format!("{key}{end}"));
        let _ = std::fs::write(offset_path, kept.join("\n") + "\n");
    }
    taken
}

/// The `instances` result the TUI and RPC report: other instances and their agents.
pub fn report(dir: &Path, session: &str) -> Value {
    let others: Vec<Value> = list(dir).into_iter().filter(|i| i.session != session)
        .map(|i| json!({"pid": i.pid, "session": i.session, "agents": i.agents})).collect();
    json!({"instances": others})
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("greencode-inst-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn instances_register_and_message_each_other_once() {
        let dir = temp("msg");
        register(&dir, "s1", &["task1".into()]).unwrap();
        register(&dir, "s2", &[]).unwrap();
        let live = list(&dir);
        assert!(live.iter().any(|i| i.session == "s1" && i.agents == ["task1"]));
        assert_eq!(report(&dir, "s1")["instances"].as_array().unwrap().len(), 1);
        send(&dir, "s2", "s1", "please rebase").unwrap();
        send(&dir, "s2", "all", "heads up").unwrap();
        let got = take_inbox(&dir, "s1");
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].text, "please rebase");
        assert!(take_inbox(&dir, "s1").is_empty(), "a delivered message is not delivered twice");
        assert!(take_inbox(&dir, "s2").is_empty(), "the sender does not read its own message back");
        unregister(&dir, "s1");
        assert!(!list(&dir).iter().any(|i| i.session == "s1"));
        std::fs::remove_dir_all(dir).unwrap();
    }
}
