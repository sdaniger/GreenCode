//! `eval`: a long-lived Python or JS interpreter per project whose variables survive between calls.
//! A kernel is a small driver script speaking one JSON line each way over stdin/stdout.
//!
//! Cells run in a kernel thread (Python) or promise chain (JS) while the driver keeps reading, so a cell can
//! call back into the host for `agent()`/`workpool()`, and a subagent can call a tool the cell defined.
use super::str_arg;
use crate::{
    config::{integrations::PythonCfg, Config},
    permission::Mode,
    session::Meta,
    ToolDef,
};
use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};
use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    process::Stdio,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, LazyLock, Mutex,
    },
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStdin, ChildStdout},
    sync::{oneshot, Semaphore},
};

/// Each call has this long; a hung kernel is killed, since it cannot be interrupted in place.
const TIMEOUT: Duration = Duration::from_secs(120);

// fd 1 carries the protocol, so it is moved aside first and the kernel's own fd 1 points at /dev/null: a child
// process printing to it cannot corrupt a reply.
const PY_DRIVER: &str = r#"import contextlib, io, json, os, queue, sys, threading, traceback
proto = os.fdopen(os.dup(1), "w")
os.dup2(os.open(os.devnull, os.O_WRONLY), 1)
env = {"__name__": "__main__"}
tools = {}
waiters = {}
seq = [0]
lock = threading.Lock()
def send(obj):
    with lock:
        proto.write(json.dumps(obj) + "\n")
        proto.flush()
def host(fn, args):
    seq[0] += 1
    ident = seq[0]
    event, box = threading.Event(), {}
    waiters[ident] = (event, box)
    send({"host": {"id": ident, "fn": fn, "args": args}})
    event.wait()
    if "error" in box:
        raise RuntimeError(box["error"])
    return box.get("value")
def agent(prompt, agent=None, model=None):
    return host("agent", {"prompt": prompt, "agent": agent, "model": model})
def workpool(items, prompt="{}", agent=None, model=None):
    return host("workpool", {"items": list(items), "prompt": prompt, "agent": agent, "model": model})
def tool(fn=None, schema=None, description=None):
    def register(f):
        tools[getattr(f, "__name__", "tool")] = f
        if description:
            f.__doc__ = description
        f.__schema__ = schema or {"type": "object"}
        return f
    return register if fn is None else register(fn)
def listing():
    return [{"name": name, "description": (getattr(f, "__doc__", None) or "").strip()[:200],
             "schema": getattr(f, "__schema__", {"type": "object"})} for name, f in tools.items()]
def gather(run, ident):
    buf = io.StringIO()
    with contextlib.redirect_stdout(buf), contextlib.redirect_stderr(buf):
        try:
            run()
        except BaseException:
            traceback.print_exc()
    send({"id": ident, "out": buf.getvalue(), "tools": listing()})
def run_cell(code, ident):
    def run():
        try:
            compiled, single = compile(code, "<eval>", "eval"), True
        except SyntaxError:
            compiled, single = compile(code, "<eval>", "exec"), False
        value = eval(compiled, env)
        if single and value is not None:
            print(repr(value))
    gather(run, ident)
def run_tool(call, ident):
    def run():
        args = call.get("args") or {}
        fn = tools[call["name"]]
        value = fn(**args) if isinstance(args, dict) else fn(*args)
        print(value if isinstance(value, str) else repr(value))
    gather(run, ident)
# Cells evaluate against `env`, so the driver's helpers must be in it (JS puts them on `ctx`).
for _name in ("tool", "host", "agent", "workpool"):
    env[_name] = globals()[_name]
jobs = queue.Queue()
def worker():
    while True:
        cell = jobs.get()
        if cell is None:
            return
        run_cell(cell[0], cell[1])
threading.Thread(target=worker, daemon=True).start()
for line in sys.stdin:
    msg = json.loads(line)
    if "code" in msg:
        jobs.put((msg["code"], msg["id"]))
    elif "call" in msg:
        threading.Thread(target=run_tool, args=(msg["call"], msg["id"]), daemon=True).start()
    elif "host_reply" in msg:
        reply = msg["host_reply"]
        waiting = waiters.pop(reply["id"], None)
        if waiting:
            waiting[1].update(reply)
            waiting[0].set()
"#;

const JS_DRIVER: &str = r#"const vm = require("vm");
const util = require("util");
const readline = require("readline");
const write = process.stdout.write.bind(process.stdout);
const tools = new Map();
const waiters = new Map();
let seq = 0;
let buf = "";
const send = (obj) => write(JSON.stringify(obj) + "\n");
const host = (fn, args) => new Promise((resolve, reject) => {
  const id = ++seq;
  waiters.set(id, { resolve, reject });
  send({ host: { id, fn, args } });
});
const say = (...a) => { buf += a.map((x) => (typeof x === "string" ? x : util.inspect(x))).join(" ") + "\n"; };
const listing = () => [...tools.entries()].map(([name, t]) => ({ name, description: (t.description || "").slice(0, 200), schema: t.schema }));
const ctx = vm.createContext({
  console: { log: say, info: say, error: say, warn: say, debug: say },
  require, setTimeout, clearTimeout, setInterval, clearInterval, Buffer, URL, Promise,
  process: { env: process.env, cwd: () => process.cwd() },
  tool: (fn, opts = {}) => {
    tools.set(opts.name || fn.name, { fn, description: opts.description || fn.description || "", schema: opts.schema || { type: "object" } });
    return fn;
  },
  agent: (prompt, opts = {}) => host("agent", { prompt, agent: opts.agent, model: opts.model }),
  workpool: (items, opts = {}) => host("workpool", { items, prompt: opts.prompt || "{}", agent: opts.agent, model: opts.model }),
});
if (typeof fetch === "function") ctx.fetch = fetch;
async function runCell(code, id) {
  buf = "";
  try {
    let value;
    try {
      value = vm.runInContext(code, ctx, { filename: "eval" });
    } catch (e) {
      // A cell using top-level `await` runs as an async wrapper; bindings made there do not persist.
      if (e instanceof SyntaxError && /(^|[^.\w])await\b/.test(code)) value = vm.runInContext("(async () => {\n" + code + "\n})()", ctx, { filename: "eval" });
      else throw e;
    }
    if (value && typeof value.then === "function") value = await value;
    if (value !== undefined) say(value);
  } catch (e) {
    buf += (e && e.stack ? e.stack : String(e)) + "\n";
  }
  send({ id, out: buf, tools: listing() });
}
async function callTool(call, id) {
  buf = "";
  try {
    const entry = tools.get(call.name);
    if (!entry) throw new Error("unknown eval tool " + call.name);
    const args = call.args || {};
    let value = entry.fn(...(Array.isArray(args) ? args : Object.values(args)));
    if (value && typeof value.then === "function") value = await value;
    if (value !== undefined) say(value);
  } catch (e) {
    buf += (e && e.stack ? e.stack : String(e)) + "\n";
  }
  send({ id, out: buf });
}
(async () => {
  let queue = Promise.resolve();
  const enqueue = (fn) => { queue = queue.then(fn).catch(() => {}); };
  for await (const line of readline.createInterface({ input: process.stdin })) {
    const msg = JSON.parse(line);
    if (msg.code !== undefined) enqueue(() => runCell(msg.code, msg.id));
    else if (msg.call) enqueue(() => callTool(msg.call, msg.id));
    else if (msg.host_reply) {
      const waiting = waiters.get(msg.host_reply.id);
      if (waiting) {
        waiters.delete(msg.host_reply.id);
        if (msg.host_reply.error) waiting.reject(new Error(msg.host_reply.error));
        else waiting.resolve(msg.host_reply.value);
      }
    }
  }
})();
"#;

struct Kernel {
    stdin: Arc<tokio::sync::Mutex<ChildStdin>>,
    pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Value>>>>,
    seq: Arc<AtomicU64>,
    key: Key,
    _child: Child,
}

type Key = (PathBuf, String);
static KERNELS: LazyLock<tokio::sync::Mutex<HashMap<Key, Arc<Kernel>>>> = LazyLock::new(Default::default);

/// Tools the session's cells registered; each advertises to the model with the kernel that owns it.
struct Registered {
    key: Key,
    description: String,
    schema: Value,
}
static TOOLS: LazyLock<Mutex<HashMap<String, Registered>>> = LazyLock::new(Default::default);

fn quoted(path: &Path) -> String {
    format!("'{}'", path.to_string_lossy().replace('\'', "'\\''"))
}

/// An explicit `python.interpreter` replaces PATH discovery; `~` and relative paths resolve like the shell.
fn python_executable(python: &PythonCfg, cwd: &Path) -> Result<PathBuf> {
    let entry = python.interpreter.trim();
    if entry.is_empty() {
        return crate::config::executable("python3").ok_or_else(|| anyhow!("eval: python3 not found"));
    }
    Ok(match entry.strip_prefix("~/") {
        Some(rest) => crate::user_home().join(rest),
        None if entry == "~" => crate::user_home(),
        None if Path::new(entry).is_absolute() => PathBuf::from(entry),
        None => cwd.join(entry),
    })
}

/// `~/.greencode/eval/env`: the managed JavaScript package environment. With `eval.auto_provision` it is created
/// on first use, with a manifest that puts its `node_modules` on the kernel's require path.
fn js_env(dir: PathBuf, provision: bool) -> Result<PathBuf> {
    if provision {
        std::fs::create_dir_all(dir.join("node_modules")).with_context(|| dir.display().to_string())?;
        let manifest = dir.join("package.json");
        if !manifest.is_file() {
            std::fs::write(
                &manifest,
                "{\n  \"name\": \"greencode-eval\",\n  \"private\": true,\n  \"type\": \"commonjs\"\n}\n",
            )?;
        }
    }
    Ok(dir)
}

fn spawn(lang: &str, cwd: &Path, cfg: &Config) -> Result<Kernel> {
    let python = &cfg.integration_opts.python;
    let (interpreter, driver, file): (String, String, &str) = match lang {
        "py" => (
            format!("{} -u", quoted(&python_executable(python, cwd)?)),
            PY_DRIVER.replace("jobs = queue.Queue()", &format!("{}\njobs = queue.Queue()", py_namespaces(&namespace_servers(cfg, cwd)))),
            "eval-kernel.py",
        ),
        "js" => {
            let env = js_env(crate::home().join("eval/env"), cfg.integration_opts.eval.auto_provision)?;
            anyhow::ensure!(
                env.join("package.json").is_file(),
                "eval: the managed JS environment is missing; enable eval.auto_provision to create {}",
                env.display()
            );
            (
                quoted(
                    &crate::config::executable("node")
                        .or_else(|| crate::config::executable("bun"))
                        .ok_or_else(|| anyhow!("eval: node or bun not found"))?,
                ),
                format!("{JS_DRIVER}\n{}", js_namespaces(&namespace_servers(cfg, cwd))),
                "eval-kernel.js",
            )
        }
        other => bail!("eval: lang must be py or js, got `{other}`"),
    };
    let dir = crate::home().join("eval");
    std::fs::create_dir_all(&dir)?;
    let script = dir.join(file);
    std::fs::write(&script, &driver).with_context(|| script.display().to_string())?;
    // A JS kernel resolves `require("foo")` from the managed environment first.
    let node_path = (lang == "js").then(|| crate::home().join("eval/env/node_modules"));
    // Through `shell`, so the `sandbox` setting limits a kernel exactly as it limits `bash`.
    let mut command = super::shell(&format!("exec {interpreter} {}", quoted(&script)), cwd)?;
    if let Some(node_path) = node_path {
        let existing = std::env::var("NODE_PATH").unwrap_or_default();
        let joined = if existing.is_empty() {
            node_path.display().to_string()
        } else {
            format!("{}:{existing}", node_path.display())
        };
        command.env("NODE_PATH", joined);
    }
    let mut child = command
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
    let key: Key = (
        std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf()),
        lang.to_string(),
    );
    let kernel = Kernel {
        stdin: Arc::new(tokio::sync::Mutex::new(stdin)),
        pending: Arc::new(Mutex::new(HashMap::new())),
        seq: Arc::new(AtomicU64::new(1)),
        key,
        _child: child,
    };
    read_replies(&kernel, stdout, cfg.clone(), cwd.to_path_buf());
    Ok(kernel)
}

/// Routes each kernel line: a reply to a waiting call, or a host request (`agent`, `workpool`).
fn read_replies(kernel: &Kernel, stdout: ChildStdout, cfg: Config, cwd: PathBuf) {
    let (pending, stdin, key) = (kernel.pending.clone(), kernel.stdin.clone(), kernel.key.clone());
    tokio::spawn(async move {
        let mut lines = BufReader::new(stdout).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let Ok(reply) = serde_json::from_str::<Value>(&line) else { continue };
            // A cell's reply carries the full tool listing, which replaces what this kernel had before.
            if let Some(list) = reply["tools"].as_array() {
                let names: HashSet<&str> = list.iter().filter_map(|tool| tool["name"].as_str()).collect();
                let mut known = TOOLS.lock().unwrap();
                known.retain(|name, tool| tool.key != key || names.contains(name.as_str()));
                for tool in list {
                    let Some(name) = tool["name"].as_str() else { continue };
                    known.insert(
                        name.to_string(),
                        Registered {
                            key: key.clone(),
                            description: tool["description"].as_str().unwrap_or("").to_string(),
                            schema: tool["schema"].clone(),
                        },
                    );
                }
            }
            if let Some(request) = reply.get("host") {
                let (stdin, cfg, cwd, request) = (stdin.clone(), cfg.clone(), cwd.clone(), request.clone());
                tokio::spawn(async move {
                    let id = request["id"].as_u64().unwrap_or_default();
                    let payload = match host_request(&request, &cfg, &cwd).await {
                        Ok(value) => json!({"host_reply": {"id": id, "value": value}}),
                        Err(error) => json!({"host_reply": {"id": id, "error": format!("{error:#}")}}),
                    };
                    let mut stdin = stdin.lock().await;
                    let _ = stdin.write_all(format!("{payload}\n").as_bytes()).await;
                    let _ = stdin.flush().await;
                });
                continue;
            }
            let id = reply["id"].as_u64().unwrap_or_default();
            if let Some(waiting) = pending.lock().unwrap().remove(&id) {
                let _ = waiting.send(reply);
            }
        }
    });
}

/// Registers a request and writes it; the reply goes to the returned receiver by id.
async fn begin(kernel: &Kernel, payload: Value) -> Result<(oneshot::Receiver<Value>)> {
    let id = kernel.seq.fetch_add(1, Ordering::SeqCst);
    let (tx, rx) = oneshot::channel();
    kernel.pending.lock().unwrap().insert(id, tx);
    let mut payload = payload;
    payload["id"] = json!(id);
    let mut stdin = kernel.stdin.lock().await;
    stdin.write_all(format!("{payload}\n").as_bytes()).await?;
    stdin.flush().await?;
    Ok(rx)
}

/// How a workpool runs its items: fresh agents trickle in one at a time under the slot limit, a reused pool takes
/// a full batch and waits for it before the next.
fn plan(n: usize, slots: usize, fresh: bool) -> Vec<Vec<usize>> {
    let slots = slots.max(1);
    if fresh {
        (0..n).map(|index| vec![index]).collect()
    } else {
        (0..n).collect::<Vec<_>>().chunks(slots).map(<[usize]>::to_vec).collect()
    }
}

/// `agent()`/`workpool()` from a cell: real subagents, with the calling agent's mode and UI.
async fn host_request(request: &Value, cfg: &Config, cwd: &Path) -> Result<Value> {
    // `mcp.evalNamespaces`: `server.tool(**args)` from a cell runs that MCP tool.
    if request["fn"] == "mcp" {
        let server = request["args"]["server"].as_str().unwrap_or("");
        let mcp = super::mcp::shared(cwd, cfg.mcp_project_config, &cfg.integration_opts.mcp);
        let text = mcp
            .run(
                &json!({"action": "call", "server": server, "tool": request["args"]["tool"], "args": request["args"]["args"]}),
                cwd,
            )
            .await?;
        return Ok(json!({ "value": text }));
    }
    let (parent, mode, events) = match super::agent_context() {
        Some((meta, mode, events)) => (meta, mode, Some(events)),
        None => (
            Meta::new(cwd.to_path_buf(), cfg.model.clone(), "native".into()),
            Mode::Ask,
            None,
        ),
    };
    match request["fn"].as_str() {
        Some("agent") => {
            let prompt = request["args"]["prompt"].as_str().unwrap_or("").trim().to_string();
            anyhow::ensure!(!prompt.is_empty(), "eval: agent(prompt) needs a prompt");
            let req = crate::delegate::Request {
                backend: "native".into(),
                model: request["args"]["model"].as_str().filter(|m| !m.is_empty()).map(String::from),
                task: prompt,
                name: request["args"]["agent"].as_str().filter(|a| !a.is_empty()).map(String::from),
                isolated: false,
                mode: None,
                top_level: true,
            };
            let id = crate::delegate::start(&parent, Arc::new(cfg.clone()), mode, req, false, events)?;
            let text = match crate::delegate::wait(&id, None).await {
                Some(crate::delegate::Outcome::Completed(text)) => text.trim().to_string(),
                Some(crate::delegate::Outcome::Failed(error)) => bail!("subagent failed: {error}"),
                Some(crate::delegate::Outcome::Cancelled) => bail!("subagent cancelled"),
                None => bail!("subagent did not finish"),
            };
            Ok(json!({ "value": text }))
        }
        Some("workpool") => {
            let items = request["args"]["items"].as_array().cloned().unwrap_or_default();
            let template = request["args"]["prompt"].as_str().unwrap_or("{}").to_string();
            let agent = request["args"]["agent"].as_str().filter(|a| !a.is_empty()).map(String::from);
            let model = request["args"]["model"].as_str().filter(|a| !a.is_empty()).map(String::from);
            let results = workpool(&items, &template, agent, model, &parent, cfg, mode, events).await;
            Ok(json!({ "value": results }))
        }
        other => bail!("eval: unknown host function `{}`", other.unwrap_or("")),
    }
}

#[allow(clippy::too_many_arguments)]
async fn workpool(
    items: &[Value],
    template: &str,
    agent: Option<String>,
    model: Option<String>,
    parent: &Meta,
    cfg: &Config,
    mode: Mode,
    events: Option<crate::EventTx>,
) -> Vec<Value> {
    let slots = cfg.task.slots().max(1);
    let fresh = cfg.integration_opts.eval.workpool.fresh_agents;
    let semaphore: Option<Arc<Semaphore>> = fresh.then(|| Arc::new(Semaphore::new(slots)));
    let mut out = Vec::with_capacity(items.len());
    let mut handles = Vec::with_capacity(items.len());
    for batch in plan(items.len(), slots, fresh) {
        for index in batch {
            // A fresh item waits here for a free slot; its task holds the permit until the subagent ends.
            let permit = match &semaphore {
                Some(semaphore) => Some(semaphore.clone().acquire_owned().await),
                None => None,
            };
            let prompt = template.replace("{}", &items[index].as_str().map(str::to_string).unwrap_or_else(|| items[index].to_string()));
            let req = crate::delegate::Request {
                backend: "native".into(),
                model: model.clone(),
                task: prompt,
                name: agent.clone(),
                isolated: false,
                mode: None,
                top_level: true,
            };
            let (parent, cfg) = (parent.clone(), Arc::new(cfg.clone()));
            let events = events.clone();
            handles.push(tokio::spawn(async move {
                let _permit = permit;
                let id = crate::delegate::start(&parent, cfg, mode, req, false, events).map_err(|e| format!("{e:#}"))?;
                match crate::delegate::wait(&id, None).await {
                    Some(crate::delegate::Outcome::Completed(text)) => Ok(text.trim().to_string()),
                    Some(crate::delegate::Outcome::Failed(error)) => Err(error),
                    Some(crate::delegate::Outcome::Cancelled) => Err("cancelled".to_string()),
                    None => Err("did not finish".to_string()),
                }
            }));
        }
        // A reused pool waits for the whole batch before starting the next one.
        if !fresh {
            out.extend(collect(std::mem::take(&mut handles)).await);
        }
    }
    out.extend(collect(handles).await);
    out
}

async fn collect(handles: Vec<tokio::task::JoinHandle<Result<String, String>>>) -> Vec<Value> {
    let mut out = Vec::with_capacity(handles.len());
    for task in handles {
        out.push(json!(match task.await {
            Ok(Ok(text)) => text,
            Ok(Err(error)) => format!("error: {error}"),
            Err(error) => format!("error: workpool item failed: {error}"),
        }));
    }
    out
}

fn output(out: String) -> String {
    if out.trim().is_empty() {
        "(no output)".into()
    } else {
        out
    }
}

/// Runs one cell: `id` is dispatched, then the reply is awaited within `TIMEOUT`, moving to a background job when
/// `eval.auto_background` is on and the threshold passes first.
async fn run_cell(kernel: &Kernel, code: &str, cfg: &Config) -> Result<String> {
    let receiver = begin(kernel, json!({"code": code})).await?;
    let auto = cfg.integration_opts.eval.auto_background;
    if auto.enabled && super::running_jobs() < super::job_capacity(cfg) {
        let mut done = tokio::spawn(async move {
            receiver.await.ok().map(|reply| reply["out"].as_str().unwrap_or("").to_string()).unwrap_or_default()
        });
        match tokio::time::timeout(Duration::from_millis(auto.threshold_ms), &mut done).await {
            Ok(joined) => return Ok(output(joined.unwrap_or_default())),
            Err(_) => {
                super::reserve_job(cfg)?;
                let log = super::log_path();
                let written = log.clone();
                tokio::spawn(async move {
                    let text = done.await.unwrap_or_default();
                    let _ = std::fs::write(&written, &text);
                    super::job_finished(super::JobResult { command: "eval".into(), log: written.clone(), status: Some(0) });
                });
                return Ok(format!(
                    "still running after {} ms; moved to the background. Output goes to {} (use `read`).",
                    auto.threshold_ms,
                    log.display()
                ));
            }
        }
    }
    match tokio::time::timeout(TIMEOUT, receiver).await {
        Err(_) => bail!("eval timed out after {}s", TIMEOUT.as_secs()),
        Ok(Err(_)) => bail!("eval kernel exited; run again"),
        Ok(Ok(reply)) => Ok(output(reply["out"].as_str().unwrap_or("").to_string())),
    }
}

/// The session's kernel for `lang`, spawned on first use.
async fn session_kernel(lang: &str, cwd: &Path, cfg: &Config) -> Result<Arc<Kernel>> {
    let key: Key = (
        std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf()),
        lang.to_string(),
    );
    let mut all = KERNELS.lock().await;
    if let Some(kernel) = all.get(&key) {
        return Ok(kernel.clone());
    }
    let kernel = Arc::new(spawn(lang, cwd, cfg)?);
    all.insert(key, kernel.clone());
    Ok(kernel)
}

/// `foo` defined by a cell, as the model sees it.
pub fn tool_defs() -> Vec<ToolDef> {
    TOOLS
        .lock()
        .unwrap()
        .iter()
        .map(|(name, tool)| ToolDef {
            name: name.clone(),
            description: format!("{} (defined by an eval cell; call with its schema)", tool.description),
            schema: tool.schema.clone(),
        })
        .collect()
}

pub fn is_tool(name: &str) -> bool {
    TOOLS.lock().unwrap().contains_key(name)
}

/// Calls an eval-defined tool; the kernel keeps running whatever cell defined it.
pub async fn call_tool(name: &str, args: &Value, cfg: &Config) -> Result<String> {
    anyhow::ensure!(cfg.integration_opts.eval.tools.enabled, "eval: eval-defined tools are off (eval.tools.enabled)");
    let entry = TOOLS.lock().unwrap().get(name).map(|tool| tool.key.clone());
    let Some(key) = entry else { bail!("eval: no eval tool named `{name}`; define it in a py or js cell first") };
    let kernel = KERNELS.lock().await.get(&key).cloned();
    let Some(kernel) = kernel else { bail!("eval: the kernel that defined `{name}` is gone; run the cell again") };
    let receiver = begin(&kernel, json!({"call": {"name": name, "args": args}})).await?;
    match tokio::time::timeout(TIMEOUT, receiver).await {
        Err(_) => bail!("eval tool `{name}` timed out after {}s", TIMEOUT.as_secs()),
        Ok(Err(_)) => bail!("eval kernel exited"),
        Ok(Ok(reply)) => Ok(output(reply["out"].as_str().unwrap_or("").to_string())),
    }
}

/// `mcp.evalNamespaces`: `server.tool(**args)` in Python. The class is installed in the driver's
/// globals and pushed into each cell's `env`, which is what `run_cell` evaluates against.
fn py_namespaces(servers: &[String]) -> String {
    if servers.is_empty() {
        return String::new();
    }
    let mut out = String::from("def __mcp(server, tool, args):\n    return host('mcp', {'server': server, 'tool': tool, 'args': args})\n");
    for (i, server) in servers.iter().enumerate() {
        out.push_str(&format!(
            "class __Mcp{i}:\n    def __getattr__(self, tool):\n        return lambda **a: __mcp({server:?}, tool, a)\nNS_{i} = __Mcp{i}()\nenv[{var:?}] = NS_{i}\n",
            var = server.replace(['-', '.'], "_"),
        ));
    }
    out
}

/// `server.tool(args)` in JS: a Proxy makes every tool name resolve without connecting first.
fn js_namespaces(servers: &[String]) -> String {
    let mut out = String::new();
    for server in servers {
        out.push_str(&format!(
            "ctx.{var} = new Proxy({{}}, {{ get: (_, tool) => (args = {{}}) => host(\"mcp\", {{ server: {server:?}, tool, args }}) }});\n",
            var = server.replace(['-', '.'], "_"),
        ));
    }
    out
}

/// The servers whose namespaces exist; connecting is lazy, so names come from the config file.
fn namespace_servers(cfg: &Config, cwd: &Path) -> Vec<String> {
    if !cfg.integration_opts.mcp.eval_namespaces {
        return Vec::new();
    }
    super::mcp::shared(cwd, cfg.mcp_project_config, &cfg.integration_opts.mcp)
        .names()
        .into_iter()
        .filter(|server| crate::discovery::transform(server, cfg.integration_opts.mcp.builtin_transforms).is_none())
        .collect()
}

pub async fn run(args: &Value, cwd: &Path) -> Result<String> {
    run_with(&Config::default(), args, cwd).await
}

pub async fn run_with(cfg: &Config, args: &Value, cwd: &Path) -> Result<String> {
    let (lang, code) = (str_arg(args, "lang")?, str_arg(args, "code")?);
    let opts = &cfg.integration_opts;
    if lang == "py" && !opts.eval.py {
        bail!("eval: Python cells are disabled by eval.py = false");
    }
    if lang == "js" && !opts.eval.js {
        bail!("eval: JavaScript cells are disabled by eval.js = false");
    }
    if opts.python.kernel_mode == "per-call" {
        let kernel = spawn(lang, cwd, cfg)?;
        let receiver = begin(&kernel, json!({"code": code})).await?;
        return match tokio::time::timeout(TIMEOUT, receiver).await {
            Err(_) => bail!("eval timed out after {}s", TIMEOUT.as_secs()),
            Ok(Err(_)) => bail!("eval kernel exited; run again"),
            Ok(Ok(reply)) => Ok(output(reply["out"].as_str().unwrap_or("").to_string())),
        };
    }
    if args["reset"] == true {
        // Dropping the last reference kills the kernel; the next call starts a fresh one.
        let key: Key = (
            std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf()),
            lang.to_string(),
        );
        KERNELS.lock().await.remove(&key);
    }
    let kernel = session_kernel(lang, cwd, cfg).await?;
    run_cell(&kernel, code, cfg).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn python() -> bool {
        crate::config::executable("python3").is_some()
    }

    #[tokio::test]
    async fn disabled_languages_are_refused_before_spawning() {
        let cwd = std::env::temp_dir();
        let mut cfg = Config::default();
        cfg.integration_opts.eval.py = false;
        let error = run_with(&cfg, &json!({"lang": "py", "code": "1"}), &cwd)
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("eval.py"), "{error}");
        cfg.integration_opts.eval.py = true;
        cfg.integration_opts.eval.js = false;
        let error = run_with(&cfg, &json!({"lang": "js", "code": "1"}), &cwd)
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("eval.js"), "{error}");
    }

    #[test]
    fn explicit_interpreter_skips_path_lookup() {
        let cwd = Path::new("/tmp");
        let python = PythonCfg { kernel_mode: "session".into(), interpreter: "~/venv/bin/python".into() };
        assert_eq!(python_executable(&python, cwd).unwrap(), crate::user_home().join("venv/bin/python"));
        let python = PythonCfg { interpreter: "venv/python".into(), ..python };
        assert_eq!(python_executable(&python, cwd).unwrap(), cwd.join("venv/python"));
    }

    #[test]
    fn auto_provisioning_creates_the_managed_js_environment_only_when_asked() {
        let home = tempfile::tempdir().unwrap();
        let dir = home.path().join("eval/env");
        assert!(js_env(dir.clone(), false).unwrap().join("package.json").exists() == false);
        assert!(!dir.join("package.json").exists());
        js_env(dir.clone(), true).unwrap();
        assert!(dir.join("package.json").is_file());
        assert!(dir.join("node_modules").is_dir());
        let manifest = std::fs::read_to_string(dir.join("package.json")).unwrap();
        assert!(manifest.contains("greencode-eval"), "{manifest}");
    }

    #[test]
    fn workpool_plan_trickles_fresh_agents_and_batches_reused_ones() {
        assert_eq!(plan(5, 2, false), vec![vec![0, 1], vec![2, 3], vec![4]]);
        assert_eq!(plan(3, 2, true), vec![vec![0], vec![1], vec![2]]);
        assert_eq!(plan(0, 4, false), Vec::<Vec<usize>>::new());
    }

    #[tokio::test]
    async fn cells_define_tools_that_the_host_can_call() {
        if !python() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = Config::default();
        cfg.integration_opts.python.kernel_mode = "session".into();
        cfg.integration_opts.eval.tools.enabled = true;
        run_with(
            &cfg,
            &json!({"lang": "py", "code": "@tool\ndef add(a, b):\n    \"\"\"Add two numbers.\"\"\"\n    return a + b\n"}),
            dir.path(),
        )
        .await
        .unwrap();
        assert!(is_tool("add"), "the cell's tool was not registered");
        let out = call_tool("add", &json!({"a": 2, "b": 3}), &cfg).await.unwrap();
        assert!(out.contains('5'), "{out}");
        TOOLS.lock().unwrap().remove("add");
    }

    #[tokio::test]
    async fn eval_tools_are_off_when_configured_off() {
        let mut cfg = Config::default();
        cfg.integration_opts.eval.tools.enabled = false;
        let error = call_tool("add", &json!({}), &cfg).await.unwrap_err().to_string();
        assert!(error.contains("eval.tools.enabled"), "{error}");
    }

    #[tokio::test]
    async fn a_long_cell_moves_to_a_background_job() {
        if !python() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = Config::default();
        cfg.integration_opts.python.kernel_mode = "session".into();
        cfg.integration_opts.eval.auto_background.enabled = true;
        cfg.integration_opts.eval.auto_background.threshold_ms = 10;
        let out = run_with(&cfg, &json!({"lang": "py", "code": "import time; time.sleep(0.5); print('settled')"}), dir.path())
            .await
            .unwrap();
        assert!(out.contains("moved to the background"), "{out}");
        let job = super::super::next_job_result().await.expect("a background eval job");
        assert!(std::fs::read_to_string(&job.log).unwrap().contains("settled"));
    }
}
