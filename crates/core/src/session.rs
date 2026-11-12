use crate::Message;
use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    io::{BufRead, BufReader, Write},
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Meta {
    pub id: String,
    pub cwd: PathBuf,
    pub title: String,
    pub model: String,
    /// `native` or the name of an ACP backend.
    pub backend: String,
    pub created: u64,
    /// Set when `cwd` is a worktree: the repository it was created from.
    #[serde(default)]
    pub repo: Option<PathBuf>,
}

impl Meta {
    pub fn new(cwd: PathBuf, model: String, backend: String) -> Meta {
        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
        // The clock alone can repeat when threads are created back to back.
        static SEQ: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Meta { id: format!("{:x}-{seq:x}", now.as_micros()), cwd, title: String::new(), model, backend, created: now.as_secs(), repo: None }
    }
}

fn dir() -> PathBuf {
    crate::home().join("sessions")
}

/// Append-only JSONL: a `{"meta":..}` line, then messages and `{"compact":..}` markers.
pub struct Session {
    path: PathBuf,
}

impl Session {
    pub fn path(id: &str) -> PathBuf {
        dir().join(format!("{id}.jsonl"))
    }

    pub fn open(id: &str) -> Option<Session> {
        let path = Self::path(id);
        path.exists().then_some(Session { path })
    }

    pub fn create(meta: &Meta) -> Result<Session> {
        std::fs::create_dir_all(dir())?;
        let s = Session { path: Self::path(&meta.id) };
        s.append(&json!({ "meta": meta }))?;
        Ok(s)
    }

    pub fn append<T: Serialize>(&self, v: &T) -> Result<()> {
        let mut f = std::fs::OpenOptions::new().create(true).append(true).open(&self.path)?;
        writeln!(f, "{}", serde_json::to_string(v)?)?;
        Ok(())
    }

    /// Messages since the last compaction marker (the marker becomes the first message).
    pub fn load(id: &str) -> Result<(Meta, Vec<Message>)> {
        let f = std::fs::File::open(Self::path(id))?;
        let mut meta = None;
        let mut messages = Vec::new();
        for line in BufReader::new(f).lines() {
            let Ok(v) = serde_json::from_str::<Value>(&line?) else { continue };
            if let Some(m) = v.get("meta") {
                meta = serde_json::from_value(m.clone()).ok();
            } else if let Some(s) = v.get("compact").and_then(Value::as_str) {
                messages = vec![Message::user(s)];
            } else if let Ok(m) = serde_json::from_value(v) {
                messages.push(m);
            }
        }
        Ok((meta.ok_or_else(|| anyhow!("session {id} has no meta line"))?, messages))
    }

    /// Newest first. Reads only the first line of each file.
    pub fn list() -> Vec<Meta> {
        let mut out: Vec<Meta> = std::fs::read_dir(dir())
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|e| {
                let mut line = String::new();
                BufReader::new(std::fs::File::open(e.path()).ok()?).read_line(&mut line).ok()?;
                serde_json::from_value(serde_json::from_str::<Value>(&line).ok()?.get("meta")?.clone()).ok()
            })
            .collect();
        out.sort_by(|a, b| b.created.cmp(&a.created).then(b.id.cmp(&a.id)));
        out
    }

    pub fn delete(id: &str) {
        let _ = std::fs::remove_file(Self::path(id));
    }
}
