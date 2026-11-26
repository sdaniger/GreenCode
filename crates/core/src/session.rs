use crate::{permission::Mode, Message};
use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    io::{BufRead, BufReader, Write},
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};

/// Drop model exploration, not standalone user messages (including steering and job outcomes).
pub(crate) fn rewind_exploration(messages: &mut Vec<Message>, at: usize) {
    let mut i = 0;
    messages.retain(|m| {
        let before = i < at;
        i += 1;
        before
            || (m.role == crate::Role::User
                && !m
                    .content
                    .iter()
                    .any(|b| matches!(b, crate::Block::ToolResult { .. })))
    });
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Meta {
    pub id: String,
    pub cwd: PathBuf,
    pub title: String,
    pub model: String,
    /// Native sub-agent default; `None` follows the parent model.
    #[serde(default)]
    pub subagent_model: Option<String>,
    /// `native` or the name of an ACP backend.
    pub backend: String,
    pub created: u64,
    /// Set when `cwd` is a worktree: the repository it was created from.
    #[serde(default)]
    pub repo: Option<PathBuf>,
    /// Set on a child thread started by `delegate_task` (or a native `task` item with `backend`): its parent thread.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
}

impl Meta {
    pub fn new(cwd: PathBuf, model: String, backend: String) -> Meta {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        // The clock alone can repeat when threads are created back to back.
        static SEQ: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Meta {
            id: format!("{:x}-{seq:x}", now.as_micros()),
            cwd,
            title: String::new(),
            model,
            subagent_model: None,
            backend,
            created: now.as_secs(),
            repo: None,
            parent: None,
        }
    }
}

/// UI-only state. None of these fields is sent as model context.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct View {
    pub meta: Meta,
    #[serde(default)]
    pub draft: String,
    #[serde(default)]
    pub queue: Vec<String>,
    #[serde(default)]
    pub pinned: bool,
    #[serde(default)]
    pub archived: bool,
    #[serde(default)]
    pub mode: Mode,
    /// Hidden from the thread list until this Unix time; 0 = not snoozed.
    #[serde(default)]
    pub snoozed: u64,
    #[serde(default)]
    pub auto_settle: AutoSettle,
    #[serde(default)]
    pub linked_pr: Option<LinkedPr>,
    #[serde(default)]
    pub linked_pr_manual: Option<LinkedPr>,
    #[serde(default)]
    pub last_user_at: u64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum AutoSettle {
    #[default]
    Follow,
    Never,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LinkedPr {
    pub number: u64,
    pub state: String,
    #[serde(default)]
    pub changed_at: u64,
}

fn dir() -> PathBuf {
    crate::home().join("sessions")
}

/// Same rule as `validate_id`, for other modules that build per-session paths.
pub(crate) fn check_id(id: &str) -> Result<()> {
    validate_id(id)
}

pub(crate) fn validate_id(id: &str) -> Result<()> {
    if id.is_empty()
        || !id
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
    {
        anyhow::bail!("invalid session id `{id}`");
    }
    Ok(())
}

/// Append-only JSONL: a `{"meta":..}` line, then messages and `{"compact":..}` markers.
pub struct Session {
    path: PathBuf,
}

impl Session {
    pub fn view(id: &str) -> Result<Option<View>> {
        validate_id(id)?;
        let path = crate::home().join("threads").join(format!("{id}.json"));
        match std::fs::read(path) {
            Ok(bytes) => Ok(Some(serde_json::from_slice(&bytes)?)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    pub fn save_view(view: &View) -> Result<()> {
        validate_id(&view.meta.id)?;
        // Preserve a damaged state file for recovery instead of overwriting it on the next keystroke.
        Self::view(&view.meta.id)?;
        let dir = crate::home().join("threads");
        std::fs::create_dir_all(&dir)?;
        let path = dir.join(format!("{}.json", view.meta.id));
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec(view)?)?;
        std::fs::rename(tmp, path)?;
        Ok(())
    }

    /// Runtime handles are separate from the transcript: never replay UI logs to a provider.
    pub fn runtime(id: &str) -> Result<Value> {
        validate_id(id)?;
        let path = crate::home().join("runtime").join(format!("{id}.json"));
        match std::fs::read(path) {
            Ok(bytes) => Ok(serde_json::from_slice(&bytes)?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Value::Null),
            Err(e) => Err(e.into()),
        }
    }

    pub fn runtime_for_backend(id: &str, backend: &str) -> Result<Value> {
        let value = Self::runtime(id)?;
        if !value.is_null() && value["backend"].as_str() != Some(backend) {
            anyhow::bail!("saved runtime does not belong to backend `{backend}`");
        }
        Ok(value)
    }

    /// Copies the transcript to `path` for a handoff to another machine.
    pub fn export(id: &str, path: &std::path::Path) -> Result<()> {
        validate_id(id)?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::copy(Self::path(id), path)?;
        Ok(())
    }

    /// Forgets the provider-side session so the next prompt starts a fresh one (T3's "Restart agent session").
    pub fn clear_runtime(id: &str) -> Result<()> {
        validate_id(id)?;
        let path = crate::home().join("runtime").join(format!("{id}.json"));
        match std::fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    pub fn save_runtime(id: &str, value: &Value) -> Result<()> {
        validate_id(id)?;
        let dir = crate::home().join("runtime");
        std::fs::create_dir_all(&dir)?;
        let path = dir.join(format!("{id}.json"));
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec(value)?)?;
        std::fs::rename(tmp, path)?;
        Ok(())
    }

    pub fn path(id: &str) -> PathBuf {
        dir().join(format!("{id}.jsonl"))
    }

    pub fn open(id: &str) -> Option<Session> {
        validate_id(id).ok()?;
        let path = Self::path(id);
        path.exists().then_some(Session { path })
    }

    /// A CLI can reserve a thread ID before starting its external backend.
    /// Without a backend handle, only that metadata-only record can start fresh.
    pub(crate) fn can_start_external(id: &str) -> Result<bool> {
        validate_id(id)?;
        let file = match std::fs::File::open(Self::path(id)) {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(true),
            Err(e) => return Err(e.into()),
        };
        let mut file = BufReader::new(file);
        let mut header = String::new();
        file.read_line(&mut header)?;
        if !file.fill_buf()?.is_empty() {
            return Ok(false);
        }
        let mut row: Value = serde_json::from_str(&header)?;
        let meta: Meta = serde_json::from_value(row.get_mut("meta").map(Value::take).unwrap_or_default())?;
        Ok(meta.id == id)
    }

    pub fn create(meta: &Meta) -> Result<Session> {
        validate_id(&meta.id)?;
        std::fs::create_dir_all(dir())?;
        let s = Session {
            path: Self::path(&meta.id),
        };
        s.append(&json!({ "meta": meta }))?;
        Ok(s)
    }

    pub fn append<T: Serialize>(&self, v: &T) -> Result<()> {
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        writeln!(f, "{}", serde_json::to_string(v)?)?;
        Ok(())
    }

    /// Source of the last compact marker, if it is a snap archive.
    pub fn snap_source(id: &str) -> Option<String> {
        validate_id(id).ok()?;
        let f = std::fs::File::open(Self::path(id)).ok()?;
        let mut source = None;
        for line in BufReader::new(f).lines() {
            let Ok(line) = line else { continue };
            let Ok(v) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            if v.get("compact").and_then(Value::as_str).is_some() {
                source = v.get("source").and_then(Value::as_str).map(str::to_owned);
            }
        }
        source
    }

    /// Messages since the last compaction marker (the marker becomes the first message) and the last `reset`.
    pub fn load(id: &str) -> Result<(Meta, Vec<Message>)> {
        let (meta, mut messages, resets) = Self::load_all(id)?;
        messages.drain(..resets.last().copied().unwrap_or(0));
        Ok((meta, messages))
    }

    /// Appends the marker `/clear` leaves: the next agent starts with no history; the view keeps what came before.
    pub fn reset(id: &str) -> Result<()> {
        match Self::open(id) {
            Some(s) => s.append(&json!({ "reset": true })),
            None => Ok(()),
        }
    }

    /// Like `load`, but keeps the messages from before each `reset`; the third value is the message index each one starts at.
    pub fn load_all(id: &str) -> Result<(Meta, Vec<Message>, Vec<usize>)> {
        validate_id(id)?;
        let f = std::fs::File::open(Self::path(id))?;
        let mut meta = None;
        let mut messages = Vec::new();
        let mut resets = Vec::new();
        for line in BufReader::new(f).lines() {
            let Ok(v) = serde_json::from_str::<Value>(&line?) else {
                continue;
            };
            let base = resets.last().copied().unwrap_or(0);
            if let Some(m) = v.get("meta") {
                meta = serde_json::from_value(m.clone()).ok();
            } else if v.get("reset").is_some() {
                resets.push(messages.len());
            } else if let Some(s) = v.get("compact").and_then(Value::as_str) {
                let frames: Vec<String> = v
                    .get("frames")
                    .and_then(|f| serde_json::from_value(f.clone()).ok())
                    .unwrap_or_default();
                messages.truncate(base);
                messages.push(crate::snap::message(s, &frames));
            } else if let Some(at) = v.get("truncate").and_then(Value::as_u64) {
                let mut tail = messages.split_off(base);
                if v["keep_user_messages"] == true {
                    rewind_exploration(&mut tail, at as usize);
                } else {
                    tail.truncate(at as usize);
                }
                messages.append(&mut tail);
            } else if let Ok(m) = serde_json::from_value(v) {
                messages.push(m);
            }
        }
        let meta = Self::view(id)?
            .map(|v| v.meta)
            .or(meta)
            .ok_or_else(|| anyhow!("session {id} has no meta line"))?;
        Ok((meta, messages, resets))
    }

    /// Text-only history for a future cross-backend handoff. Tool state and reasoning are not portable.
    pub fn handoff(id: &str, budget: usize) -> Result<(Vec<Message>, bool)> {
        validate_id(id)?;
        let f = std::fs::File::open(Self::path(id))?;
        let mut messages = Vec::new();
        for line in BufReader::new(f).lines() {
            let line = line?;
            if line.starts_with("{\"reset\"") {
                messages.clear();
                continue;
            }
            if let Ok(message) = serde_json::from_str::<Message>(&line) {
                let update = message.content.iter().any(
                    |b| matches!(b, crate::Block::Text { text } if crate::context::is_update(text)),
                );
                if !update {
                    messages.push(message);
                }
            }
        }
        let mut omitted = false;
        let messages: Vec<Message> = messages
            .into_iter()
            .filter_map(|m| {
                omitted |= m
                    .content
                    .iter()
                    .any(|b| !matches!(b, crate::Block::Text { .. }));
                let content: Vec<_> = m
                    .content
                    .into_iter()
                    .filter(|b| matches!(b, crate::Block::Text { text } if !text.is_empty()))
                    .collect();
                (!content.is_empty()).then_some(Message {
                    role: m.role,
                    content,
                })
            })
            .collect();
        let mut selected = vec![false; messages.len()];
        let mut remaining = budget;
        // Account for serialized bytes, including role and escaping, rather than estimating tokens.
        let first_user = messages.iter().position(|m| m.role == crate::Role::User);
        for i in first_user
            .into_iter()
            .chain((0..messages.len()).rev().filter(|i| Some(*i) != first_user))
        {
            let size = serde_json::to_vec(&messages[i])?.len() + 1;
            if size <= remaining {
                selected[i] = true;
                remaining -= size;
            }
        }
        omitted |= selected.iter().any(|s| !s);
        Ok((
            messages
                .into_iter()
                .zip(selected)
                .filter_map(|(m, keep)| keep.then_some(m))
                .collect(),
            omitted,
        ))
    }

    /// Drops the last user turn and everything after it, and restores the files `write`/`edit` changed since.
    /// Returns the removed prompt. Changes made through `bash` are not tracked.
    pub fn undo(id: &str) -> Result<Option<String>> {
        Self::rewind(id, 0)
    }

    /// `undo` back to the prompt `back` turns before the last one, so that prompt can be edited and sent again.
    pub fn rewind(id: &str, back: usize) -> Result<Option<String>> {
        validate_id(id)?;
        let path = Self::path(id);
        let text = std::fs::read_to_string(&path)?;
        let (mut offset, mut turns, mut restores) = (0, vec![], vec![]);
        for line in text.split_inclusive('\n') {
            let v: Value = serde_json::from_str(line).unwrap_or_default();
            let blocks = v["content"].as_array().map(Vec::as_slice).unwrap_or(&[]);
            // A turn starts at a user message that is a prompt rather than tool results or a context update.
            if v["role"] == "user"
                && !blocks.iter().any(|b| b["type"] == "tool_result")
                && !blocks
                    .iter()
                    .any(|b| b["text"].as_str().is_some_and(crate::context::is_update))
            {
                turns.push((
                    offset,
                    blocks
                        .iter()
                        .find_map(|b| b["text"].as_str())
                        .unwrap_or("")
                        .to_string(),
                ));
            } else if v["checkpoint"].is_object() {
                restores.push((turns.len(), v["checkpoint"].clone()));
            }
            offset += line.len();
        }
        let Some(keep) = turns.len().checked_sub(back + 1) else {
            return Ok(None);
        };
        let (at, prompt) = turns.swap_remove(keep);
        // Newest first, so a file changed in several turns ends at its oldest saved state.
        for (_, c) in restores.iter().rev().filter(|(turn, _)| *turn > keep) {
            let Some(file) = c["path"].as_str() else {
                continue;
            };
            match c["before"].as_str() {
                Some(before) => std::fs::write(file, before)?,
                None => {
                    let _ = std::fs::remove_file(file);
                }
            }
        }
        std::fs::OpenOptions::new()
            .write(true)
            .open(&path)?
            .set_len(at as u64)?;
        Ok(Some(prompt))
    }

    /// What `write`/`edit` changed during one turn (`back` turns before the last), as a unified diff.
    /// A file's state after the turn is what the next later checkpoint saved, or the file on disk.
    pub fn turn_diff(id: &str, back: usize) -> Result<String> {
        validate_id(id)?;
        let (mut turn, mut points) = (0usize, vec![]);
        for line in std::fs::read_to_string(Self::path(id))?.lines() {
            let v: Value = serde_json::from_str(line).unwrap_or_default();
            if v["role"] == "user"
                && !v["content"]
                    .as_array()
                    .is_some_and(|b| b.iter().any(|b| b["type"] == "tool_result"))
                && !v["content"].as_array().is_some_and(|b| {
                    b.iter()
                        .any(|b| b["text"].as_str().is_some_and(crate::context::is_update))
                })
            {
                turn += 1;
            } else if let Some(path) = v["checkpoint"]["path"].as_str() {
                points.push((
                    turn,
                    path.to_string(),
                    v["checkpoint"]["before"].as_str().map(String::from),
                ));
            }
        }
        let Some(want) = turn.checked_sub(back).filter(|t| *t > 0) else {
            return Ok(String::new());
        };
        let dir = crate::home().join("tmp").join(format!("diff-{id}"));
        std::fs::create_dir_all(&dir)?;
        let (mut out, mut seen) = (String::new(), std::collections::BTreeSet::new());
        for (i, (_, path, before)) in points.iter().enumerate().filter(|(_, p)| p.0 == want) {
            if !seen.insert(path) {
                continue;
            }
            let after = match points[i + 1..].iter().find(|p| p.0 > want && p.1 == *path) {
                Some((_, _, after)) => after.clone(),
                None => std::fs::read_to_string(path).ok(),
            };
            if *before == after {
                continue;
            }
            let side = |name: &str, text: &Option<String>| -> Result<std::path::PathBuf> {
                let Some(text) = text else {
                    return Ok("/dev/null".into());
                };
                let file = dir.join(name);
                std::fs::write(&file, text)?;
                Ok(file)
            };
            // `--no-index` exits 1 when the sides differ, so the status is not checked.
            let o = std::process::Command::new("git")
                .args(["diff", "--no-index", "--"])
                .arg(side("before", before)?)
                .arg(side("after", &after)?)
                .output()?;
            let text = String::from_utf8_lossy(&o.stdout);
            let body: Vec<&str> = text.lines().skip_while(|l| !l.starts_with("@@")).collect();
            out.push_str(&format!("--- {path}\n+++ {path}\n{}\n", body.join("\n")));
        }
        let _ = std::fs::remove_dir_all(&dir);
        Ok(out)
    }

    /// Copies a native thread's history into a new thread.
    pub fn fork(id: &str) -> Result<Meta> {
        let (old, _) = Self::load(id)?;
        let mut meta = Meta::new(old.cwd, old.model, old.backend);
        (meta.title, meta.subagent_model, meta.repo) = (
            format!("{} (fork)", old.title),
            old.subagent_model,
            old.repo,
        );
        let session = Self::create(&meta)?;
        for line in std::fs::read_to_string(Self::path(id))?.lines() {
            let Ok(v) = serde_json::from_str::<Value>(line) else {
                continue;
            };
            if v.get("meta").is_none() {
                session.append(&v)?;
            }
        }
        Ok(meta)
    }

    /// The whole transcript as Markdown.
    pub fn markdown(id: &str) -> Result<String> {
        let (meta, _) = Self::load(id)?;
        let mut out = format!(
            "# {}\n\n`{}` · {}\n",
            meta.title,
            meta.model,
            meta.cwd.display()
        );
        for line in std::fs::read_to_string(Self::path(id))?.lines() {
            let Ok(v) = serde_json::from_str::<Value>(line) else {
                continue;
            };
            if let Some(summary) = v["compact"].as_str() {
                out.push_str(&format!("\n---\n*Compacted:* {summary}\n"));
            }
            if v["content"].as_array().is_some_and(|b| {
                b.iter()
                    .any(|b| b["text"].as_str().is_some_and(crate::context::is_update))
            }) {
                continue;
            }
            for b in v["content"].as_array().into_iter().flatten() {
                match b["type"].as_str() {
                    Some("text") => out.push_str(&format!(
                        "\n## {}\n\n{}\n",
                        if v["role"] == "user" {
                            "User"
                        } else {
                            "Assistant"
                        },
                        b["text"].as_str().unwrap_or("")
                    )),
                    Some("tool_use") => out.push_str(&format!(
                        "\n**{}** `{}`\n",
                        b["name"].as_str().unwrap_or(""),
                        b["input"]
                    )),
                    Some("tool_result") => out.push_str(&format!(
                        "\n```\n{}\n```\n",
                        b["content"]
                            .as_str()
                            .unwrap_or("")
                            .chars()
                            .take(2000)
                            .collect::<String>()
                    )),
                    Some("image") => out.push_str("\n*(image)*\n"),
                    _ => {}
                }
            }
        }
        Ok(out)
    }

    /// Ids of the threads whose transcript contains `query` (case-insensitive).
    pub fn search(query: &str) -> Vec<String> {
        let q = query.to_lowercase();
        std::fs::read_dir(dir())
            .into_iter()
            .flatten()
            .flatten()
            .filter(|e| {
                std::fs::read_to_string(e.path()).is_ok_and(|t| t.to_lowercase().contains(&q))
            })
            .filter_map(|e| {
                e.path()
                    .file_stem()
                    .map(|s| s.to_string_lossy().into_owned())
            })
            .collect()
    }

    /// Newest first. Reads only the first line of each file.
    pub fn list() -> Vec<Meta> {
        let mut out: Vec<Meta> = std::fs::read_dir(dir())
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|e| {
                let mut line = String::new();
                BufReader::new(std::fs::File::open(e.path()).ok()?)
                    .read_line(&mut line)
                    .ok()?;
                serde_json::from_value(
                    serde_json::from_str::<Value>(&line)
                        .ok()?
                        .get("meta")?
                        .clone(),
                )
                .ok()
            })
            .collect();
        for e in std::fs::read_dir(crate::home().join("threads"))
            .into_iter()
            .flatten()
            .flatten()
        {
            if e.path().extension().is_none_or(|ext| ext != "json") {
                continue;
            }
            if let Ok(bytes) = std::fs::read(e.path()) {
                if let Ok(view) = serde_json::from_slice::<View>(&bytes) {
                    if let Some(meta) = out.iter_mut().find(|m| m.id == view.meta.id) {
                        *meta = view.meta;
                    } else {
                        out.push(view.meta);
                    }
                }
            }
        }
        out.sort_by(|a, b| b.created.cmp(&a.created).then(b.id.cmp(&a.id)));
        out
    }

    pub fn delete(id: &str) {
        if validate_id(id).is_err() {
            return;
        }
        let _ = std::fs::remove_file(Self::path(id));
        let _ = std::fs::remove_file(crate::home().join("threads").join(format!("{id}.json")));
        let _ = std::fs::remove_file(crate::home().join("runtime").join(format!("{id}.json")));
    }
}
