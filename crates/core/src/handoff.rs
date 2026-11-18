//! Hands one Pi/OMP thread to the terminal UI and back, with exactly one writer at a time.
//!
//! `prepare` writes two TUI extensions (GreenCode's approval hook and a tracker) and records the open handoff on
//! disk. The tracker appends one fsynced JSON line per `session_start`/`switch`/`branch`/`tree`/`shutdown` with
//! the exact session file, id, leaf and branch ids Pi/OMP itself reports. `finish` trusts nothing else: no
//! newest-file or mtime guessing. A missing, truncated or unclean log, a file the log does not vouch for, or a
//! branch it cannot verify is an error that changes nothing; a transcript that does not extend the displayed one
//! is `Diverged` and changes nothing. Only then does it append the missing messages and repoint
//! `runtime/<id>.json`, in that order, so a repeated `finish` after a crash is idempotent.
//!
//! The caller must have stopped the RPC child, and waited for it to exit, before `prepare` (Pi has no lease to
//! check; OMP's lease is probed). `prepare` blocks briefly for OMP (one `omp session lease` call).

use crate::{
    config::{Config, Protocol},
    pi::{native_message, APPROVAL_EXTENSION},
    session::{validate_id, Meta, Session},
};
use anyhow::{bail, ensure, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::{HashMap, HashSet},
    io::{ErrorKind, Write},
    path::{Path, PathBuf},
};

// Records ids only (no message content). Every handler appends one whole line and fsyncs it; a handler that
// fails records an `error` line, and a write that fails leaves no line, so `finish` fails closed either way.
const TRACKER: &str = r#"import { closeSync, fsyncSync, openSync, writeSync } from "node:fs";
const LOG = __LOG__;
export default function (pi: any) {
  const note = (event: string, reason: unknown, ctx: any) => {
    let record: any;
    try {
      const m = ctx.sessionManager;
      record = { event, reason: reason ?? null, pid: process.pid, id: m.getSessionId(), file: m.getSessionFile() ?? null, leaf: m.getLeafId() ?? null, branch: m.getBranch().map((e: any) => e.id), messages: m.getEntries().filter((e: any) => e.type === "message").length };
    } catch (e) {
      record = { event: "error", at: event, pid: process.pid, message: String(e) };
    }
    const fd = openSync(LOG, "a");
    try { writeSync(fd, JSON.stringify(record) + "\n"); fsyncSync(fd); } finally { closeSync(fd); }
  };
  pi.on("session_start", (e: any, ctx: any) => note("start", e?.reason, ctx));
  pi.on("session_switch", (e: any, ctx: any) => note("switch", e?.reason, ctx));
  pi.on("session_branch", (e: any, ctx: any) => note("branch", e?.reason, ctx));
  pi.on("session_tree", (_e: any, ctx: any) => note("tree", null, ctx));
  pi.on("session_shutdown", (e: any, ctx: any) => note("shutdown", e?.reason, ctx));
}
"#;

/// What the terminal pane runs. Spawn `argv` directly (no shell) in `cwd` with `PATH` = `path_env`.
#[derive(Clone, Debug)]
pub struct Launch {
    pub argv: Vec<String>,
    pub cwd: PathBuf,
    pub path_env: String,
    pub env: std::collections::BTreeMap<String, String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// The displayed transcript already matched the terminal session.
    Unchanged,
    /// This many messages from the terminal were appended to the displayed transcript.
    Appended(usize),
    /// The terminal session does not extend what GreenCode shows. Nothing was written and the handoff stays open:
    /// `adopt` it (backs up the old transcript) or `abandon` it.
    Diverged(String),
}

#[derive(Serialize, Deserialize)]
struct State {
    backend: String,
    /// The session file the terminal was told to open; `None` for a thread with no native session yet.
    session_file: Option<PathBuf>,
}

#[derive(Deserialize)]
struct Note {
    event: String,
    #[serde(default)]
    pid: u64,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    file: Option<PathBuf>,
    #[serde(default)]
    leaf: Option<String>,
    #[serde(default)]
    branch: Vec<String>,
    #[serde(default)]
    messages: Option<usize>,
    #[serde(default)]
    message: Option<String>,
}

/// A verified terminal session.
struct Checked {
    /// The tracked session file, when the terminal persisted one.
    file: Option<PathBuf>,
    /// Transcript messages of the branch Pi/OMP will resume on.
    native: Vec<Value>,
    /// Messages GreenCode shows now (`None`: no transcript yet).
    shown: Option<Vec<Value>>,
    /// Set when the terminal ended on a different branch than the one a restart resumes.
    moved: Option<String>,
}

fn runtime_file(id: &str, suffix: &str) -> PathBuf {
    crate::home().join("runtime").join(format!("{id}.{suffix}"))
}

fn remove(path: &Path) -> Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
}

fn text(path: &Path) -> Result<&str> {
    path.to_str().with_context(|| format!("path is not valid UTF-8: {}", path.display()))
}

fn backend<'a>(cfg: &'a Config, meta: &Meta) -> Result<(&'a [String], Protocol)> {
    let b = cfg.backends.iter().find(|b| b.name == meta.backend).with_context(|| format!("unknown backend `{}`", meta.backend))?;
    ensure!(b.protocol != Protocol::Acp, "backend `{}` has no terminal UI handoff (only Pi and Oh My Pi)", meta.backend);
    ensure!(!b.command.is_empty(), "backend `{}` has no command", meta.backend);
    Ok((&b.command, b.protocol))
}

/// True while a handoff is open for the thread: no RPC child may write its session then.
pub fn is_open(id: &str) -> bool {
    validate_id(id).is_ok() && runtime_file(id, "handoff.json").exists()
}

/// Advisory exclusive lock (flock) on a thread's native-session writer. `Pi::start` takes it before its
/// `is_open` check and holds it for the life of its RPC child; `prepare` takes it while it checks and creates
/// the open marker. So two GreenCode processes sharing one GREENCODE_HOME cannot run an RPC child and open a
/// terminal handoff for the same thread at once. It does not stop a `pi`/`omp` the user starts by hand.
pub struct WriterLock(#[allow(dead_code)] std::fs::File);

impl WriterLock {
    pub(crate) fn unlock(&self) -> Result<()> {
        self.0.unlock()?;
        Ok(())
    }
}

pub fn writer_lock(id: &str) -> Result<WriterLock> {
    validate_id(id)?;
    std::fs::create_dir_all(crate::home().join("runtime"))?;
    let file = std::fs::OpenOptions::new().create(true).write(true).truncate(false).open(runtime_file(id, "writer.lock"))?;
    match file.try_lock() {
        Ok(()) => Ok(WriterLock(file)),
        Err(std::fs::TryLockError::WouldBlock) => bail!("another GreenCode process is running this thread's agent; close it first"),
        Err(std::fs::TryLockError::Error(e)) => Err(e.into()),
    }
}

/// Writes the TUI extensions, records the open handoff and returns the terminal command.
pub fn prepare(cfg: &Config, meta: &Meta) -> Result<Launch> {
    validate_id(&meta.id)?;
    let (command, protocol) = backend(cfg, meta)?;
    let _writer = writer_lock(&meta.id)?;
    ensure!(!is_open(&meta.id), "a terminal handoff is already open for this thread; finish or abandon it first");
    let path_env = crate::config::agent_path()?.into_string().map_err(|_| anyhow::anyhow!("PATH is not valid UTF-8"))?;
    let saved = Session::runtime_for_backend(&meta.id, &meta.backend)?;
    let history = shown(&meta.id)?.is_some_and(|m| !m.is_empty());
    let file = match saved["sessionFile"].as_str().map(PathBuf::from) {
        Some(file) if file.is_file() => {
            let native = read_native(&file)?;
            if protocol == Protocol::Omp {
                ensure!(!lease_held(&command[0], &native.id, &path_env)?, "the Oh My Pi session is still open for writing in another process; stop it and retry");
            }
            Some(file)
        }
        // Pi/OMP report a session path before they write anything. That is a fresh thread only if no history
        // exists; the terminal then starts its own session, which `finish` tracks.
        pointer => {
            ensure!(!history, "this thread has history but no readable native session ({}); start a new thread instead of silently losing its context", pointer.map_or("no handle".into(), |p| p.display().to_string()));
            None
        }
    };

    let dir = crate::home().join("provider-sessions").join(&meta.id);
    std::fs::create_dir_all(&dir)?;
    let permissions = dir.join("greencode-permissions.ts");
    std::fs::write(&permissions, APPROVAL_EXTENSION)?;
    let log = runtime_file(&meta.id, "handoff.log");
    remove(&log)?; // no handoff is open, so any log is a leftover
    let tracker = dir.join("greencode-handoff-tracker.ts");
    std::fs::write(&tracker, TRACKER.replace("__LOG__", &serde_json::to_string(text(&log)?)?))?;

    let mut argv = command.to_vec();
    argv.extend(["--session-dir".to_string(), text(&dir)?.to_string()]);
    if let Some(file) = &file {
        argv.extend(["--session".to_string(), text(file)?.to_string()]);
    }
    argv.extend(["--extension".to_string(), text(&permissions)?.to_string(), "--extension".to_string(), text(&tracker)?.to_string()]);

    // Durable and atomic: the marker appears complete (hard link of a synced file) or not at all, and the
    // link fails if another prepare created it first.
    let state_path = runtime_file(&meta.id, "handoff.json");
    let tmp = runtime_file(&meta.id, &format!("handoff.{}.tmp", std::process::id()));
    let mut out = std::fs::File::create(&tmp)?;
    out.write_all(&serde_json::to_vec(&State { backend: meta.backend.clone(), session_file: file })?)?;
    out.sync_all()?;
    let linked = std::fs::hard_link(&tmp, &state_path);
    remove(&tmp)?;
    match linked {
        Ok(()) => {}
        Err(e) if e.kind() == ErrorKind::AlreadyExists => bail!("a terminal handoff is already open for this thread"),
        Err(e) => return Err(e.into()),
    }
    std::fs::File::open(state_path.parent().unwrap())?.sync_all()?;
    Ok(Launch { argv, cwd: meta.cwd.clone(), path_env, env: cfg.backends.iter().find(|b| b.name == meta.backend).unwrap().env.clone() })
}

/// Call after the terminal exits (any exit status). See the module docs for what is verified.
pub fn finish(cfg: &Config, meta: &Meta) -> Result<Outcome> {
    let checked = check(cfg, meta)?;
    if let Some(reason) = checked.moved {
        return Ok(Outcome::Diverged(reason));
    }
    let shown = checked.shown.as_deref().unwrap_or(&[]);
    let skip = match extends(shown, &checked.native) {
        Ok(skip) => skip,
        Err(reason) => return Ok(Outcome::Diverged(reason)),
    };
    let new = &checked.native[skip..];
    if !new.is_empty() {
        let session = match Session::open(&meta.id) {
            Some(session) => session,
            None => Session::create(meta)?,
        };
        for message in new {
            session.append(message)?;
        }
    }
    commit(meta, checked.file.as_deref())?;
    Ok(if new.is_empty() { Outcome::Unchanged } else { Outcome::Appended(new.len()) })
}

/// Explicit user decision after `Diverged`: the terminal session becomes the displayed transcript. The old
/// transcript is first copied to `sessions/<id>.jsonl.pre-handoff-<n>` (never overwritten); its checkpoints (undo
/// points) are not carried over. Returns the number of messages now shown.
pub fn adopt(cfg: &Config, meta: &Meta) -> Result<usize> {
    let checked = check(cfg, meta)?;
    let path = Session::path(&meta.id);
    let mut lines = Vec::new();
    if path.exists() {
        backup(&path)?;
        for line in std::fs::read_to_string(&path)?.lines() {
            if let Ok(v) = serde_json::from_str::<Value>(line) {
                if v.get("meta").is_some() {
                    lines.push(v);
                }
            }
        }
    }
    if lines.is_empty() {
        lines.push(json!({ "meta": meta }));
    }
    lines.extend(checked.native.iter().cloned());
    let mut body = Vec::new();
    for line in &lines {
        writeln!(body, "{}", serde_json::to_string(line)?)?;
    }
    std::fs::create_dir_all(path.parent().unwrap())?;
    let tmp = path.with_extension("jsonl.handoff-tmp");
    std::fs::write(&tmp, body)?;
    std::fs::rename(&tmp, &path)?;
    commit(meta, checked.file.as_deref())?;
    Ok(checked.native.len())
}

/// Drops the open handoff only; the transcript and `runtime/<id>.json` stay as they were.
pub fn abandon(meta: &Meta) -> Result<()> {
    validate_id(&meta.id)?;
    remove(&runtime_file(&meta.id, "handoff.json"))?;
    remove(&runtime_file(&meta.id, "handoff.log"))
}

fn commit(meta: &Meta, file: Option<&Path>) -> Result<()> {
    if let Some(file) = file {
        let mut runtime = Session::runtime_for_backend(&meta.id, &meta.backend)?;
        if !runtime.is_object() {
            runtime = json!({});
        }
        runtime["backend"] = json!(meta.backend);
        runtime["sessionFile"] = json!(text(file)?);
        Session::save_runtime(&meta.id, &runtime)?;
    }
    remove(&runtime_file(&meta.id, "handoff.json"))?;
    remove(&runtime_file(&meta.id, "handoff.log"))
}

fn backup(path: &Path) -> Result<PathBuf> {
    let mut n = 1;
    loop {
        let to = PathBuf::from(format!("{}.pre-handoff-{n}", path.display()));
        match std::fs::OpenOptions::new().write(true).create_new(true).open(&to) {
            Ok(mut out) => {
                std::io::copy(&mut std::fs::File::open(path)?, &mut out)?;
                out.sync_all()?;
                return Ok(to);
            }
            Err(e) if e.kind() == ErrorKind::AlreadyExists => n += 1,
            Err(e) => return Err(e.into()),
        }
    }
}

fn lease_held(program: &str, session_id: &str, path_env: &str) -> Result<bool> {
    let out = std::process::Command::new(program)
        .args(["session", "lease", "--json", "--", session_id])
        .env("PATH", path_env)
        .output()
        .context("cannot run `omp session lease` to check the session owner")?;
    ensure!(out.status.success(), "`omp session lease` failed; cannot prove no other process writes this session");
    let report: Value = serde_json::from_slice(&out.stdout).context("`omp session lease` printed no JSON")?;
    let listed = |key: &str| report[key].as_array().is_some_and(|ids| ids.iter().any(|v| v == session_id));
    Ok(listed("held") || listed("unknown"))
}

/// Verifies the tracked terminal session against the file and the displayed transcript. Writes nothing.
fn check(cfg: &Config, meta: &Meta) -> Result<Checked> {
    validate_id(&meta.id)?;
    backend(cfg, meta)?;
    let bytes = match std::fs::read(runtime_file(&meta.id, "handoff.json")) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == ErrorKind::NotFound => bail!("no terminal handoff is open for this thread"),
        Err(e) => return Err(e.into()),
    };
    let state: State = serde_json::from_slice(&bytes).context("the handoff state is damaged; abandon the handoff")?;
    ensure!(state.backend == meta.backend, "the open handoff belongs to backend `{}`", state.backend);

    let last = tracked(&meta.id, state.session_file.as_deref())?;
    let session_id = last.id.as_deref().context("the terminal reported no session id")?;
    let shown = shown(&meta.id)?;
    let file = match last.file.as_deref() {
        Some(file) => {
            ensure!(file.is_absolute(), "the terminal reported a relative session path");
            if file.is_file() {
                Some(file)
            } else {
                // A new session is only written on its first message; anything else missing is an error.
                ensure!(state.session_file.is_none() && (last.branch.is_empty() || last.messages == Some(0)) && shown.as_ref().is_none_or(|m| m.is_empty()), "the terminal's session file is missing: {}", file.display());
                None
            }
        }
        None => {
            ensure!(state.session_file.is_none() && (last.branch.is_empty() || last.messages == Some(0)) && shown.as_ref().is_none_or(|m| m.is_empty()), "the terminal reported no session file");
            None
        }
    };
    let Some(file) = file else {
        return Ok(Checked { file: None, native: vec![], shown, moved: None });
    };

    let native = read_native(file)?;
    ensure!(native.id == session_id, "the session file's id differs from the one the terminal reported");
    let by_id: HashMap<&str, &Value> = native.entries.iter().filter_map(|e| Some((e["id"].as_str()?, e))).collect();
    ensure!(by_id.len() == native.entries.len(), "the session file contains duplicate entry ids");
    let ids = |path: &[&Value]| path.iter().map(|e| e["id"].as_str().unwrap_or("").to_string()).collect::<Vec<_>>();

    ensure!(last.leaf.as_deref().is_none_or(|l| by_id.contains_key(l)), "the terminal's last entry is not in the session file (not flushed?)");
    ensure!(ids(&walk(&by_id, last.leaf.as_deref())?) == last.branch, "cannot verify the terminal's branch against the session file");
    let resumed = walk(&by_id, resume_leaf(&native.entries).as_deref())?;
    let resumed_ids = ids(&resumed);
    // Entries Pi/OMP append after the shutdown event (OMP's `session_exit`) are fine; messages are not.
    let moved = (!(resumed_ids.starts_with(&last.branch) && resumed[last.branch.len()..].iter().all(|e| e["type"] != "message")))
        .then(|| "the terminal ended on a different branch than the session file resumes on".to_string());
    let native = resumed
        .iter()
        .filter(|e| e["type"] == "message")
        .filter_map(|e| native_message(&e["message"]))
        .map(serde_json::to_value)
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Checked { file: Some(file.to_path_buf()), native, shown, moved })
}

/// Entries from the root to `leaf`; missing parents are rejected rather than silently truncating history.
fn walk<'a>(by_id: &HashMap<&str, &'a Value>, leaf: Option<&str>) -> Result<Vec<&'a Value>> {
    let (mut path, mut seen) = (vec![], HashSet::new());
    let mut cur = leaf.map(|l| by_id.get(l).copied().with_context(|| format!("missing session entry {l}"))).transpose()?;
    while let Some(entry) = cur {
        ensure!(seen.insert(entry["id"].as_str().unwrap_or("")), "the session file's entry tree has a cycle");
        path.push(entry);
        cur = entry["parentId"].as_str().map(|p| by_id.get(p).copied().with_context(|| format!("missing session parent {p}"))).transpose()?;
    }
    path.reverse();
    Ok(path)
}

/// The final `shutdown` note, after checking the log is complete, from one process, and vouches for its session.
fn tracked(id: &str, launched: Option<&Path>) -> Result<Note> {
    let raw = match std::fs::read_to_string(runtime_file(id, "handoff.log")) {
        Ok(raw) => raw,
        Err(e) if e.kind() == ErrorKind::NotFound => bail!("the terminal never reported its session (it did not start, or the tracking extension did not load); nothing was changed"),
        Err(e) => return Err(e.into()),
    };
    ensure!(raw.is_empty() || raw.ends_with('\n'), "the terminal's tracking log is truncated (it was probably killed); nothing was changed");
    let notes = raw.lines().map(|l| serde_json::from_str::<Note>(l).context("the terminal's tracking log is damaged")).collect::<Result<Vec<_>>>()?;
    if let Some(error) = notes.iter().find(|n| n.event == "error") {
        bail!("the tracking extension failed inside the terminal: {}", error.message.as_deref().unwrap_or("unknown error"));
    }
    let (first, last) = (notes.first().context("the terminal never reported its session; nothing was changed")?, notes.last().unwrap());
    ensure!(first.event == "start", "the tracking log does not begin with a session start");
    ensure!(first.pid != 0, "the tracking log has no process id");
    ensure!(notes.iter().all(|n| n.pid == first.pid), "the tracking log mixes several processes");
    if let Some(launched) = launched {
        let same = |a: &Path, b: &Path| a == b || matches!((a.canonicalize(), b.canonicalize()), (Ok(a), Ok(b)) if a == b);
        ensure!(first.file.as_deref().is_some_and(|f| same(f, launched)), "the terminal did not open the session it was given");
    }
    ensure!(last.event == "shutdown", "the terminal did not shut down cleanly (crashed or killed); nothing was changed");
    let active = notes.iter().rev().find(|n| n.event != "shutdown").unwrap_or(first);
    ensure!(active.id == last.id && active.file == last.file, "the tracking log disagrees about which session was active at shutdown");
    Ok(Note { event: last.event.clone(), pid: last.pid, id: last.id.clone(), file: last.file.clone(), leaf: last.leaf.clone(), branch: last.branch.clone(), messages: last.messages, message: None })
}

struct Native {
    id: String,
    entries: Vec<Value>,
}

/// A Pi/OMP session file: header (after OMP's optional leading title slot) then id-bearing entries.
fn read_native(path: &Path) -> Result<Native> {
    let raw = std::fs::read_to_string(path).with_context(|| format!("cannot read the session file {}", path.display()))?;
    ensure!(raw.ends_with('\n'), "the session file {} does not end with a complete line", path.display());
    let (mut id, mut entries) = (None, vec![]);
    for (n, line) in raw.lines().enumerate() {
        let v: Value = serde_json::from_str(line).with_context(|| format!("line {} of {} is not JSON", n + 1, path.display()))?;
        match v["type"].as_str() {
            Some("title") if n == 0 => {}
            Some("session") => {
                ensure!(id.is_none(), "{} has two session headers", path.display());
                id = Some(v["id"].as_str().with_context(|| format!("the session header of {} has no id", path.display()))?.to_string());
            }
            _ => {
                ensure!(id.is_some() && v["id"].is_string(), "line {} of {} is not a session entry", n + 1, path.display());
                entries.push(v);
            }
        }
    }
    Ok(Native { id: id.with_context(|| format!("{} has no session header", path.display()))?, entries })
}

/// The leaf Pi/OMP selects when it opens the file (OMP's `resolveSessionLeafId`; Pi has no `activeLeafId`, so
/// it is the last entry).
fn resume_leaf(entries: &[Value]) -> Option<String> {
    let selects = |e: &Value| match e.get("activeLeafId") {
        None => e["id"].as_str().map(str::to_string),
        Some(v) => v.as_str().map(str::to_string),
    };
    let last = entries.last()?;
    let leaf = selects(last)?;
    if last["id"] == leaf.as_str() || entries.iter().any(|e| e["id"] == leaf.as_str()) {
        return Some(leaf);
    }
    entries.iter().rev().find(|e| selects(e).is_some_and(|l| e["id"] == l.as_str())).and_then(|e| e["id"].as_str().map(str::to_string))
}

/// The displayed transcript as normalized message values. Anything but `meta` and messages is refused.
fn shown(id: &str) -> Result<Option<Vec<Value>>> {
    let raw = match std::fs::read_to_string(Session::path(id)) {
        Ok(raw) => raw,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let mut messages = vec![];
    for (n, line) in raw.lines().enumerate().filter(|(_, l)| !l.trim().is_empty()) {
        let v: Value = serde_json::from_str(line).with_context(|| format!("line {} of the GreenCode transcript is not JSON", n + 1))?;
        if v.get("meta").is_some() {
            continue;
        }
        let message: crate::Message = serde_json::from_value(v).with_context(|| format!("line {} of the GreenCode transcript is not a message", n + 1))?;
        messages.push(serde_json::to_value(message)?);
    }
    Ok(Some(messages))
}

/// How many leading messages the terminal session shares with what is shown, when it only adds to it.
fn extends(shown: &[Value], native: &[Value]) -> Result<usize, String> {
    if native.len() < shown.len() {
        return Err(format!("GreenCode shows {} messages but the terminal session ends with {}", shown.len(), native.len()));
    }
    match shown.iter().zip(native).position(|(a, b)| a != b) {
        Some(i) => Err(format!("message {} differs between GreenCode and the terminal session", i + 1)),
        None => Ok(shown.len()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tracking_and_untrusted_sessions() {
        const CHILD: &str = "GREENCODE_HANDOFF_TEST_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let home = std::env::temp_dir().join(format!("greencode-handoff-test-{}", std::process::id()));
            std::fs::create_dir(&home).unwrap();
            let status = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "handoff::tests::tracking_and_untrusted_sessions"])
                .env(CHILD, "1")
                .env("GREENCODE_HOME", &home)
                .status().unwrap();
            std::fs::remove_dir_all(&home).unwrap();
            assert!(status.success());
            return;
        }

        let home = crate::home();
        let runtime = home.join("runtime");
        std::fs::create_dir(&runtime).unwrap();
        let file = home.join("native.jsonl");
        let id = "thread";
        let log = runtime_file(id, "handoff.log");
        let state = runtime_file(id, "handoff.json");
        let pointer = runtime_file(id, "json");
        let mut cfg = Config::default();
        cfg.backends.push(crate::config::AcpCfg {
            name: "pi".into(), command: vec!["pi".into()], env: Default::default(), protocol: Protocol::Pi,
        });
        let meta = Meta {
            id: id.into(), cwd: home.clone(), title: String::new(), model: String::new(),
            subagent_model: None, backend: "pi".into(), created: 0, repo: None,
        };
        let state_bytes = serde_json::to_vec(&State { backend: "pi".into(), session_file: Some(file.clone()) }).unwrap();
        std::fs::write(&state, &state_bytes).unwrap();
        std::fs::write(&pointer, b"original pointer").unwrap();
        let native = concat!(
            "{\"type\":\"session\",\"id\":\"session\"}\n",
            "{\"type\":\"custom\",\"id\":\"root\",\"parentId\":null}\n",
            "{\"type\":\"message\",\"id\":\"leaf\",\"parentId\":\"root\",\"message\":{\"role\":\"user\",\"content\":[{\"type\":\"text\",\"text\":\"hello\"}]}}\n",
        );
        std::fs::write(&file, native).unwrap();
        let note = |event: &str, pid: u64, path: &Path, leaf: &str, branch: &[&str]| {
            json!({"event": event, "pid": pid, "id": "session", "file": path, "leaf": leaf, "branch": branch}).to_string()
        };
        let start = note("start", 7, &file, "root", &["root"]);
        let end = note("shutdown", 7, &file, "leaf", &["root", "leaf"]);
        let valid = format!("{start}\n{end}\n");
        std::fs::write(&log, &valid).unwrap();
        let checked = check(&cfg, &meta).unwrap();
        assert_eq!(checked.file.as_deref(), Some(file.as_path()));
        assert!(checked.moved.is_none());
        assert_eq!(tracked(id, Some(&file)).unwrap().branch, ["root", "leaf"]);

        let bad_logs = [
            (String::new(), "never reported"),
            (valid.trim_end().to_string(), "truncated"),
            ("not json\n".to_string(), "damaged"),
            (format!("{end}\n"), "does not begin"),
            (format!("{start}\n{}\n", note("shutdown", 8, &file, "leaf", &["root", "leaf"])), "mixes several processes"),
            (format!("{}\n{}\n", note("start", 0, &file, "root", &["root"]), note("shutdown", 0, &file, "leaf", &["root", "leaf"])), "no process id"),
            (format!("{start}\n"), "did not shut down cleanly"),
            (format!("{start}\n{}\n", note("shutdown", 7, &home.join("other"), "leaf", &["root", "leaf"])), "disagrees"),
            (format!("{start}\n{}\n", json!({"event":"error","pid":7,"message":"failure"})), "tracking extension failed"),
            (format!("{start}\n{}\n", note("shutdown", 7, &file, "leaf", &["leaf"])), "cannot verify"),
            (format!("{start}\n{}\n", note("shutdown", 7, &file, "missing", &["missing"])), "last entry is not"),
        ];
        for (contents, expected) in bad_logs {
            std::fs::write(&log, contents).unwrap();
            let error = finish(&cfg, &meta).unwrap_err().to_string();
            assert!(error.contains(expected), "{error}");
            assert_eq!(std::fs::read(&state).unwrap(), state_bytes);
            assert_eq!(std::fs::read(&pointer).unwrap(), b"original pointer");
            assert_eq!(std::fs::read_to_string(&file).unwrap(), native);
            assert!(!Session::path(id).exists());
        }

        std::fs::write(&log, format!("{start}\n{}\n", note("shutdown", 7, &file, "root", &["root"]))).unwrap();
        assert!(matches!(finish(&cfg, &meta).unwrap(), Outcome::Diverged(_)));
        assert_eq!(std::fs::read(&state).unwrap(), state_bytes);
        assert_eq!(std::fs::read(&pointer).unwrap(), b"original pointer");

        std::fs::write(&log, &valid).unwrap();
        assert!(tracked(id, Some(&home.join("other"))).err().unwrap().to_string().contains("did not open"));
        std::fs::remove_file(&file).unwrap();
        assert!(finish(&cfg, &meta).unwrap_err().to_string().contains("session file is missing"));
        std::fs::remove_file(&log).unwrap();
        assert!(finish(&cfg, &meta).unwrap_err().to_string().contains("never reported"));
        assert_eq!(std::fs::read(&state).unwrap(), state_bytes);
        assert_eq!(std::fs::read(&pointer).unwrap(), b"original pointer");

        std::fs::write(&state, serde_json::to_vec(&State { backend: "pi".into(), session_file: None }).unwrap()).unwrap();
        let terminal_note = |messages| json!({"event":"shutdown","pid":7,"id":"session","file":file,"leaf":"root","branch":["root"],"messages":messages});
        std::fs::write(&log, format!("{start}\n{}\n", terminal_note(1))).unwrap();
        assert!(finish(&cfg, &meta).unwrap_err().to_string().contains("session file is missing"));
        std::fs::write(&log, format!("{start}\n{}\n", terminal_note(0))).unwrap();
        assert_eq!(finish(&cfg, &meta).unwrap(), Outcome::Unchanged);
        std::fs::write(&state, &state_bytes).unwrap();

        std::fs::write(&file, native).unwrap();
        std::fs::write(&log, &valid).unwrap();
        std::fs::write(&pointer, serde_json::to_vec(&json!({"backend":"pi","sessionFile":file})).unwrap()).unwrap();
        assert_eq!(finish(&cfg, &meta).unwrap(), Outcome::Appended(1));
        assert!(!is_open(id));
        let (_, messages) = Session::load(id).unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(serde_json::to_value(&messages[0]).unwrap(), serde_json::to_value(native_message(&json!({"role":"user","content":[{"type":"text","text":"hello"}]})).unwrap()).unwrap());

        std::fs::write(&state, &state_bytes).unwrap();
        std::fs::write(&log, &valid).unwrap();
        std::fs::write(&file, native.replace("hello", "different")).unwrap();
        assert!(matches!(finish(&cfg, &meta).unwrap(), Outcome::Diverged(_)));
        assert_eq!(adopt(&cfg, &meta).unwrap(), 1);
        assert!(!is_open(id));
        assert!(PathBuf::from(format!("{}.pre-handoff-1", Session::path(id).display())).is_file());
        let (_, messages) = Session::load(id).unwrap();
        assert_eq!(serde_json::to_value(&messages[0]).unwrap(), serde_json::to_value(native_message(&json!({"role":"user","content":[{"type":"text","text":"different"}]})).unwrap()).unwrap());
    }

    #[test]
    fn branch_walk_and_resume_are_verified() {
        let entries = vec![
            json!({"id":"a","parentId":null}),
            json!({"id":"b","parentId":"a"}),
            json!({"id":"c","parentId":"a","activeLeafId":"b"}),
        ];
        let by_id: HashMap<_, _> = entries.iter().map(|e| (e["id"].as_str().unwrap(), e)).collect();
        let branch = walk(&by_id, Some("b")).unwrap();
        assert_eq!(branch.iter().map(|e| e["id"].as_str().unwrap()).collect::<Vec<_>>(), ["a", "b"]);
        assert_eq!(resume_leaf(&entries).as_deref(), Some("b"));
        let cycle = [json!({"id":"x","parentId":"y"}), json!({"id":"y","parentId":"x"})];
        let by_id: HashMap<_, _> = cycle.iter().map(|e| (e["id"].as_str().unwrap(), e)).collect();
        assert!(walk(&by_id, Some("x")).unwrap_err().to_string().contains("cycle"));
        let missing = json!({"id":"z","parentId":"absent"});
        let by_id = HashMap::from([("z", &missing)]);
        assert!(walk(&by_id, Some("z")).unwrap_err().to_string().contains("missing session parent"));
    }
}
