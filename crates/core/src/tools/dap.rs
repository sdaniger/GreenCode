//! `debug`: a Debug Adapter Protocol client. One session per project, kept between calls.
//! Breakpoints set before `launch` are remembered and sent during it.

use super::{
    lsp::{read_frame, write_frame},
    str_arg,
};
use crate::permission::resolve;
use anyhow::{anyhow, bail, Result};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap, VecDeque},
    path::{Path, PathBuf},
    process::Stdio,
    sync::{LazyLock, Mutex},
    time::Duration,
};
use tokio::{
    io::BufReader,
    process::{Child, ChildStdin},
    sync::mpsc,
};

const REQUEST: Duration = Duration::from_secs(20);
const STOP: Duration = Duration::from_secs(30);

struct Session {
    stdin: ChildStdin,
    rx: mpsc::UnboundedReceiver<Value>,
    seq: u64,
    responses: HashMap<u64, Value>,
    events: VecDeque<Value>,
    thread: Option<i64>,
    frames: Vec<i64>,
    output: String,
    _child: Child,
}

enum Stop {
    Stopped(String),
    Ended(String),
    Running,
}

static SESSIONS: LazyLock<tokio::sync::Mutex<HashMap<PathBuf, Session>>> =
    LazyLock::new(Default::default);
static BREAKS: LazyLock<Mutex<HashMap<PathBuf, BTreeMap<PathBuf, BTreeSet<u32>>>>> =
    LazyLock::new(Default::default);

fn quoted(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

async fn adapter_argv(program: &Path, given: &Value) -> Result<Vec<String>> {
    if let Some(argv) = given.as_array().filter(|a| !a.is_empty()) {
        return Ok(argv
            .iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect());
    }
    if program.extension().is_some_and(|e| e == "py") {
        let python =
            crate::config::executable("python3").ok_or_else(|| anyhow!("python3 not found"))?;
        let ok = tokio::process::Command::new(&python)
            .args(["-c", "import debugpy"])
            .stderr(Stdio::null())
            .status()
            .await
            .is_ok_and(|s| s.success());
        if !ok {
            bail!("debugpy is not installed (python3 -m pip install debugpy)");
        }
        return Ok(vec![
            python.to_string_lossy().into(),
            "-m".into(),
            "debugpy.adapter".into(),
        ]);
    }
    let found = match crate::config::executable("lldb-dap") {
        Some(p) => Some(p),
        None => tokio::process::Command::new("xcrun")
            .args(["-f", "lldb-dap"])
            .output()
            .await
            .ok()
            .filter(|o| o.status.success())
            .map(|o| PathBuf::from(String::from_utf8_lossy(&o.stdout).trim())),
    };
    Ok(vec![found
        .ok_or_else(|| anyhow!("lldb-dap not found"))?
        .to_string_lossy()
        .into()])
}

impl Session {
    fn start(argv: &[String], cwd: &Path) -> Result<Session> {
        // Through `shell`, so the `sandbox` setting covers the debuggee like it covers `bash`.
        let line = format!(
            "exec {}",
            argv.iter().map(|a| quoted(a)).collect::<Vec<_>>().join(" ")
        );
        let mut child = super::shell(&line, cwd)?
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()?;
        let (stdin, stdout) = (
            child.stdin.take().expect("piped"),
            child.stdout.take().expect("piped"),
        );
        let (tx, rx) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            let mut out = BufReader::new(stdout);
            while let Ok(message) = read_frame(&mut out).await {
                if tx.send(message).is_err() {
                    break;
                }
            }
        });
        Ok(Session {
            stdin,
            rx,
            seq: 0,
            responses: HashMap::new(),
            events: VecDeque::new(),
            thread: None,
            frames: vec![],
            output: String::new(),
            _child: child,
        })
    }

    /// Takes one message from the adapter and files it. `false`: nothing arrived in time.
    async fn pump(&mut self, wait: Duration) -> Result<bool> {
        let message = match tokio::time::timeout(wait, self.rx.recv()).await {
            Err(_) => return Ok(false),
            Ok(None) => bail!("the debug adapter exited"),
            Ok(Some(m)) => m,
        };
        match message["type"].as_str() {
            Some("response") => {
                if let Some(n) = message["request_seq"].as_u64() {
                    self.responses.insert(n, message);
                }
            }
            Some("event") => self.events.push_back(message),
            _ => {}
        }
        Ok(true)
    }

    async fn send(&mut self, command: &str, arguments: Value) -> Result<u64> {
        self.seq += 1;
        write_frame(&mut self.stdin, &json!({"seq": self.seq, "type": "request", "command": command, "arguments": arguments})).await?;
        Ok(self.seq)
    }

    async fn response(&mut self, seq: u64) -> Result<Value> {
        let deadline = tokio::time::Instant::now() + REQUEST;
        loop {
            if let Some(r) = self.responses.remove(&seq) {
                if r["success"] == false {
                    bail!(
                        "{}",
                        r["message"]
                            .as_str()
                            .unwrap_or("the adapter refused the request")
                    );
                }
                return Ok(r["body"].clone());
            }
            if !self
                .pump(deadline.saturating_duration_since(tokio::time::Instant::now()))
                .await?
            {
                bail!("the debug adapter did not answer in {}s", REQUEST.as_secs());
            }
        }
    }

    async fn request(&mut self, command: &str, arguments: Value) -> Result<Value> {
        let seq = self.send(command, arguments).await?;
        self.response(seq).await
    }

    /// lldb matches the path recorded in the debug info (`/tmp/x`, not the `/private/tmp/x` that canonical paths give), so a breakpoint
    /// it could not place is retried by bare file name, and the full path is restored if that does not place it either.
    async fn set_breakpoints(&mut self, file: &Path, lines: &BTreeSet<u32>) -> Result<()> {
        let breakpoints: Vec<Value> = lines.iter().map(|l| json!({"line": l})).collect();
        let name = Path::new(file.file_name().unwrap_or_default());
        for path in [file, name] {
            let body = self
                .request(
                    "setBreakpoints",
                    json!({"source": {"path": path}, "breakpoints": breakpoints}),
                )
                .await?;
            if body["breakpoints"]
                .as_array()
                .is_none_or(|all| all.iter().all(|b| b["verified"] != false))
            {
                return Ok(());
            }
        }
        self.request(
            "setBreakpoints",
            json!({"source": {"path": file}, "breakpoints": breakpoints}),
        )
        .await?;
        Ok(())
    }

    async fn stack(&mut self, cwd: &Path, levels: u32) -> Result<Vec<String>> {
        let thread = self
            .thread
            .ok_or_else(|| anyhow!("the program is not stopped"))?;
        let body = self
            .request("stackTrace", json!({"threadId": thread, "levels": levels}))
            .await?;
        let frames = body["stackFrames"].as_array().cloned().unwrap_or_default();
        self.frames = frames.iter().filter_map(|f| f["id"].as_i64()).collect();
        Ok(frames
            .iter()
            .enumerate()
            .map(|(i, f)| {
                let file = f["source"]["path"]
                    .as_str()
                    .map(|p| {
                        p.strip_prefix(&format!("{}/", cwd.display()))
                            .unwrap_or(p)
                            .to_string()
                    })
                    .unwrap_or_else(|| f["source"]["name"].as_str().unwrap_or("?").into());
                format!(
                    "#{i} {} {file}:{}",
                    f["name"].as_str().unwrap_or("?"),
                    f["line"]
                )
            })
            .collect())
    }

    async fn wait_stop(&mut self, cwd: &Path) -> Result<Stop> {
        let deadline = tokio::time::Instant::now() + STOP;
        loop {
            while let Some(event) = self.events.pop_front() {
                let body = &event["body"];
                match event["event"].as_str() {
                    Some("output") => {
                        self.output.push_str(body["output"].as_str().unwrap_or(""));
                        let cut = self.output.len().saturating_sub(2000);
                        let cut = (cut..=self.output.len())
                            .find(|i| self.output.is_char_boundary(*i))
                            .unwrap_or(0);
                        self.output.drain(..cut);
                    }
                    Some("stopped") => {
                        self.thread = body["threadId"].as_i64().or(self.thread);
                        let reason = body["reason"].as_str().unwrap_or("stopped").to_string();
                        let frames = self.stack(cwd, 5).await?;
                        return Ok(Stop::Stopped(format!(
                            "stopped: {reason} at {}\n{}",
                            frames
                                .first()
                                .and_then(|f| f.split_once(' '))
                                .and_then(|(_, r)| r.split_once(' '))
                                .map_or("?", |(_, loc)| loc),
                            frames.join("\n")
                        )));
                    }
                    Some("exited") | Some("terminated") => {
                        let code = body["exitCode"]
                            .as_i64()
                            .map_or(String::new(), |c| format!(" (code {c})"));
                        return Ok(Stop::Ended(format!(
                            "exited{code}\n{}",
                            self.output.trim_end()
                        )));
                    }
                    _ => {}
                }
            }
            if !self
                .pump(deadline.saturating_duration_since(tokio::time::Instant::now()))
                .await?
            {
                return Ok(Stop::Running);
            }
        }
    }
}

fn listing(cwd: &Path) -> String {
    let all = BREAKS.lock().unwrap();
    let lines: Vec<String> = all
        .get(cwd)
        .into_iter()
        .flatten()
        .flat_map(|(f, ls)| ls.iter().map(move |l| format!("{}:{l}", f.display())))
        .collect();
    if lines.is_empty() {
        "no breakpoints".into()
    } else {
        format!("breakpoints:\n{}", lines.join("\n"))
    }
}

/// The text to report, and whether the session stays alive.
fn settle(stop: Stop) -> (String, bool) {
    match stop {
        Stop::Stopped(text) => (text, true),
        Stop::Ended(text) => (text, false),
        Stop::Running => (
            format!(
                "still running after {}s (use continue to wait again, or stop)",
                STOP.as_secs()
            ),
            true,
        ),
    }
}

async fn launch(args: &Value, cwd: &Path, key: &Path) -> Result<(String, Option<Session>)> {
    let program = resolve(cwd, str_arg(args, "program")?);
    if !program.is_file() {
        bail!("{} is not a file", program.display());
    }
    let mut s = Session::start(&adapter_argv(&program, &args["adapter"]).await?, cwd)?;
    s.request("initialize", json!({"adapterID": "greencode", "linesStartAt1": true, "columnsStartAt1": true, "pathFormat": "path"})).await?;
    // Some adapters (debugpy) answer `launch` only after `configurationDone`, so it is sent first and answered last.
    let program_args: Vec<&str> = args["args"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    let launch = s.send("launch", json!({"program": program, "args": program_args, "cwd": cwd, "stopOnEntry": false, "console": "internalConsole"})).await?;
    let deadline = tokio::time::Instant::now() + REQUEST;
    while !s.events.iter().any(|e| e["event"] == "initialized") {
        if let Some(r) = s.responses.get(&launch).filter(|r| r["success"] == false) {
            bail!("{}", r["message"].as_str().unwrap_or("launch failed"));
        }
        if !s
            .pump(deadline.saturating_duration_since(tokio::time::Instant::now()))
            .await?
        {
            bail!("the debug adapter never sent `initialized`");
        }
    }
    s.events.retain(|e| e["event"] != "initialized");
    let breaks = BREAKS.lock().unwrap().get(key).cloned().unwrap_or_default();
    for (file, lines) in &breaks {
        s.set_breakpoints(file, lines).await?;
    }
    s.request("configurationDone", json!({})).await?;
    s.response(launch).await?;
    let (text, alive) = settle(s.wait_stop(cwd).await?);
    Ok((text, alive.then_some(s)))
}

/// Runs one action on a live session; `false` means the session is over.
async fn act(s: &mut Session, action: &str, args: &Value, cwd: &Path) -> Result<(String, bool)> {
    let text = match action {
        "continue" | "step" => {
            let thread = s
                .thread
                .ok_or_else(|| anyhow!("the program is not stopped"))?;
            let command = match (action, args["kind"].as_str().unwrap_or("over")) {
                ("continue", _) => "continue",
                (_, "in") => "stepIn",
                (_, "out") => "stepOut",
                _ => "next",
            };
            s.request(command, json!({"threadId": thread})).await?;
            return Ok(settle(s.wait_stop(cwd).await?));
        }
        "stack" => s.stack(cwd, 20).await?.join("\n"),
        "vars" | "eval" => {
            let frame = args["frame"].as_u64().unwrap_or(0) as usize;
            if s.frames.is_empty() {
                s.stack(cwd, 20).await?;
            }
            let id = *s
                .frames
                .get(frame)
                .ok_or_else(|| anyhow!("no frame {frame}"))?;
            if action == "eval" {
                let body = s.request("evaluate", json!({"expression": str_arg(args, "expr")?, "frameId": id, "context": "repl"})).await?;
                body["result"].as_str().unwrap_or("").to_string()
            } else {
                let scopes = s.request("scopes", json!({"frameId": id})).await?;
                let Some(reference) = scopes["scopes"][0]["variablesReference"].as_i64() else {
                    return Ok(("no variables".into(), true));
                };
                let body = s
                    .request("variables", json!({"variablesReference": reference}))
                    .await?;
                body["variables"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .take(50)
                    .map(|v| {
                        format!(
                            "{} = {}",
                            v["name"].as_str().unwrap_or("?"),
                            v["value"].as_str().unwrap_or("")
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            }
        }
        "stop" => {
            let _ = tokio::time::timeout(
                Duration::from_secs(5),
                s.request("disconnect", json!({"terminateDebuggee": true})),
            )
            .await;
            return Ok(("stopped".into(), false));
        }
        other => bail!("unknown debug action `{other}`"),
    };
    Ok((text, true))
}

pub async fn run(args: &Value, cwd: &Path) -> Result<String> {
    let action = str_arg(args, "action")?;
    let key = std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
    let mut sessions = SESSIONS.lock().await;
    if action == "launch" {
        sessions.remove(&key); // the old adapter and program die with it
        let (text, session) = launch(args, cwd, &key).await?;
        if let Some(s) = session {
            sessions.insert(key, s);
        }
        return Ok(text);
    }
    if action == "break" {
        let file = resolve(cwd, str_arg(args, "path")?);
        let line = args["line"]
            .as_u64()
            .ok_or_else(|| anyhow!("missing integer argument `line`"))? as u32;
        let lines = {
            let mut all = BREAKS.lock().unwrap();
            let set = all
                .entry(key.clone())
                .or_default()
                .entry(file.clone())
                .or_default();
            if !set.remove(&line) {
                set.insert(line);
            }
            set.clone()
        };
        if let Some(s) = sessions.get_mut(&key) {
            s.set_breakpoints(&file, &lines).await?;
        }
        return Ok(listing(&key));
    }
    let s = sessions
        .get_mut(&key)
        .ok_or_else(|| anyhow!("no debug session; launch a program first"))?;
    let result = act(s, action, args, cwd).await;
    // An error leaves the session for `stop` to clean up; only a finished program or `stop` removes it.
    if matches!(result, Ok((_, false))) {
        sessions.remove(&key);
    }
    result.map(|(text, _)| text)
}
