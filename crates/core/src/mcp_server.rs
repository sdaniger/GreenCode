//! MCP access to local GreenCode threads over stdio or the remote HTTP server.
use std::{collections::{BTreeSet, HashMap}, sync::{Arc, LazyLock, Mutex}, time::Duration};
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use tokio::{io::{AsyncBufReadExt, AsyncWriteExt, BufReader}, sync::{mpsc, watch}};
use crate::{config::Config, permission::Perms, session::{Meta, Session}, Block, Command, Decision, Event, Role};

pub(crate) fn tools() -> Value {
    let string = json!({"type":"string"});
    let entries = [
        ("list_projects", "List distinct project directories.", json!({}), vec![]),
        ("list_threads", "List threads, optionally filtered by project directory.", json!({"project":string}), vec![]),
        ("read_thread", "Read a thread's text-only transcript.", json!({"id":string,"max_messages":{"type":"integer","minimum":0}}), vec!["id"]),
        ("start_thread", "Start a thread and wait for its response (or return its id at once with wait=false, then use wait_thread). Approval requests are denied unless the configured mode allows the action.", json!({"cwd":string,"prompt":string,"backend":string,"model":string,"wait":{"type":"boolean"}}), vec!["cwd","prompt"]),
        ("send_message", "Send a message and wait for its response (or return at once with wait=false, then use wait_thread). Approval requests are denied unless the thread mode allows the action.", json!({"id":string,"text":string,"wait":{"type":"boolean"}}), vec!["id","text"]),
        ("search_threads", "Find threads whose title, project directory or transcript contains the query (case-insensitive), newest first.", json!({"query":string,"project":string,"limit":{"type":"integer","minimum":1,"maximum":100}}), vec!["query"]),
        ("wait_thread", "Wait for a turn started through this server to finish and return its response. Returns running=true if timeout_secs (default 600) passes first; a thread with no running turn returns its last reply.", json!({"id":string,"timeout_secs":{"type":"integer","minimum":0}}), vec!["id"]),
        ("stop_thread", "Cancel the running turn of a thread that was started through this server.", json!({"id":string}), vec!["id"]),
        ("list_models", "List configured native models and cached external backend models. Supply project to include its OpenCode models.", json!({"project":string}), vec![]),
    ];
    Value::Array(entries.into_iter().map(|(name, description, properties, required)| json!({"name":name,"description":description,"inputSchema":{"type":"object","properties":properties,"required":required}})).collect())
}

fn required<'a>(args: &'a Value, key: &str) -> Result<&'a str> {
    args.get(key).and_then(Value::as_str).filter(|s| !s.trim().is_empty()).with_context(|| format!("missing or empty {key}"))
}

/// A turn started by this process, so later calls can wait for it or stop it.
struct Running { tx: mpsc::UnboundedSender<Command>, done: watch::Receiver<Option<Result<Value, String>>> }
static RUNNING: LazyLock<Mutex<HashMap<String, Arc<Running>>>> = LazyLock::new(Default::default);

impl Running {
    /// `None` when `limit` passes before the turn finishes.
    async fn wait(&self, limit: Option<Duration>) -> Option<Result<Value>> {
        let mut done = self.done.clone();
        let finished = done.wait_for(Option::is_some);
        let result = match limit { Some(limit) => tokio::time::timeout(limit, finished).await.ok()?, None => finished.await };
        let value = result.ok()?.clone()?;
        Some(value.map_err(anyhow::Error::msg))
    }
}

fn last_reply(id: &str) -> Option<String> {
    let (_, messages) = Session::load(id).ok()?;
    let last = messages.iter().rev().find(|m| m.role == Role::Assistant)?;
    Some(last.content.iter().filter_map(|b| if let Block::Text { text } = b { Some(text.as_str()) } else { None }).collect::<Vec<_>>().join("\n"))
}

/// Starts a turn without waiting for it. A turn that outlives 10 minutes is cancelled.
fn launch(cfg: Config, meta: Meta, text: String) -> Result<Arc<Running>> {
    if !meta.cwd.is_dir() { bail!("cwd must be a directory"); }
    if let Some(policy) = &cfg.policy {
        anyhow::ensure!(policy.backend_allowed(&meta.backend), "backend blocked by policy");
    }
    let mode = Session::view(&meta.id)?.map_or(cfg.mode, |v| v.mode);
    if let Some(policy) = &cfg.policy { policy.check_mode(mode)?; }
    let id = meta.id.clone();
    let mut running = RUNNING.lock().unwrap();
    anyhow::ensure!(!running.contains_key(&id), "thread already has a running turn");
    let (tx, mut rx) = crate::spawn(&tokio::runtime::Handle::current(), Arc::new(cfg), meta, Arc::new(Perms::new(mode)));
    tx.send(Command::Prompt(text))?;
    let (done_tx, done) = watch::channel(None);
    let turn = Arc::new(Running { tx: tx.clone(), done });
    running.insert(id.clone(), turn.clone());
    tokio::spawn(async move {
        let result = tokio::time::timeout(Duration::from_secs(600), async {
            let mut text = String::new();
            let mut error = None;
            while let Some(event) = rx.recv().await {
                match event {
                    Event::Text(s) => text.push_str(&s),
                    Event::Permission { reply, .. } => { let _ = reply.send(Decision::Deny); }
                    Event::Dialog { reply, .. } => { let _ = reply.send(json!({"cancelled":true})); }
                    Event::Error(e) => error = Some(e),
                    Event::Done { .. } => {
                        if let Some(e) = error { bail!(e); }
                        if let Some(final_text) = last_reply(&id).filter(|t| !t.is_empty()) { text = final_text; }
                        return Ok(json!({"id":id,"text":text}));
                    }
                    _ => {}
                }
            }
            bail!("thread ended before completion")
        }).await.unwrap_or_else(|_| { let _ = tx.send(Command::Cancel); Err(anyhow::anyhow!("turn timed out after 10 minutes")) });
        RUNNING.lock().unwrap().remove(&id);
        done_tx.send_replace(Some(result.map_err(|e| format!("{e:#}"))));
    });
    Ok(turn)
}

/// Runs one turn to completion; approval requests are denied.
pub async fn turn(cfg: Config, meta: Meta, text: String) -> Result<Value> {
    launch(cfg, meta, text)?.wait(None).await.context("thread ended before completion")?
}

async fn turn_or_detach(cfg: Config, meta: Meta, text: String, args: &Value) -> Result<Value> {
    if args.get("wait").and_then(Value::as_bool).unwrap_or(true) { return turn(cfg, meta, text).await; }
    let id = meta.id.clone();
    launch(cfg, meta, text)?;
    Ok(json!({"id":id,"running":true}))
}

pub(crate) async fn call(name: &str, args: &Value, cfg: Config) -> Result<Value> {
    match name {
        "list_projects" => Ok(json!(Session::list().into_iter().map(|m| m.cwd).collect::<BTreeSet<_>>())),
        "list_threads" => {
            let project = args.get("project").and_then(Value::as_str);
            Ok(json!(Session::list().into_iter().filter(|m| project.is_none_or(|p| m.cwd == std::path::Path::new(p))).collect::<Vec<_>>()))
        }
        "read_thread" => {
            let (meta, messages) = Session::load(required(args, "id")?)?;
            let max = args.get("max_messages").and_then(Value::as_u64).unwrap_or(u64::MAX).min(usize::MAX as u64) as usize;
            let transcript: Vec<_> = messages.iter().skip(messages.len().saturating_sub(max)).map(|m| json!({"role":m.role,"text":m.content.iter().filter_map(|b| if let Block::Text { text } = b { Some(text.as_str()) } else { None }).collect::<Vec<_>>().join("\n")})).collect();
            Ok(json!({"meta":meta,"messages":transcript}))
        }
        "start_thread" => {
            let cwd = required(args, "cwd")?.into();
            let prompt = required(args, "prompt")?.to_string();
            let backend = args.get("backend").and_then(Value::as_str).unwrap_or("native").to_string();
            let model = args.get("model").and_then(Value::as_str).map(str::to_string).unwrap_or_else(|| if backend == "native" { cfg.model.clone() } else { "default".into() });
            let mut meta = Meta::new(cwd, model, backend);
            meta.subagent_model = cfg.subagent_model.clone();
            turn_or_detach(cfg, meta, prompt, args).await
        }
        "send_message" => {
            let (meta, _) = Session::load(required(args, "id")?)?;
            turn_or_detach(cfg, meta, required(args, "text")?.to_string(), args).await
        }
        "search_threads" => {
            let query = required(args, "query")?.to_lowercase();
            let project = args.get("project").and_then(Value::as_str);
            let limit = args.get("limit").and_then(Value::as_u64).unwrap_or(20).clamp(1, 100) as usize;
            let in_transcript: BTreeSet<_> = Session::search(&query).into_iter().collect();
            let found = Session::list().into_iter()
                .filter(|m| project.is_none_or(|p| m.cwd == std::path::Path::new(p)))
                .filter(|m| m.title.to_lowercase().contains(&query) || m.cwd.to_string_lossy().to_lowercase().contains(&query) || in_transcript.contains(&m.id))
                .take(limit).collect::<Vec<_>>();
            Ok(json!(found))
        }
        "wait_thread" => {
            let id = required(args, "id")?;
            let running = RUNNING.lock().unwrap().get(id).cloned();
            let Some(running) = running else {
                Session::load(id)?;
                return Ok(json!({"id":id,"running":false,"text":last_reply(id)}));
            };
            let limit = Duration::from_secs(args.get("timeout_secs").and_then(Value::as_u64).unwrap_or(600).min(3600));
            match running.wait(Some(limit)).await {
                Some(result) => { let mut value = result?; value["running"] = json!(false); Ok(value) }
                None => Ok(json!({"id":id,"running":true})),
            }
        }
        "stop_thread" => {
            let id = required(args, "id")?;
            let running = RUNNING.lock().unwrap().get(id).cloned();
            if let Some(running) = &running { let _ = running.tx.send(Command::Cancel); }
            Ok(json!({"id":id,"stopped":running.is_some()}))
        }
        "list_models" => {
            let mut native: BTreeSet<String> = cfg.models.iter().cloned().collect();
            native.insert(cfg.model.clone());
            for (name, p) in &cfg.providers { for model in &p.models { native.insert(format!("{name}/{model}")); } }
            let backends = if args.get("project").is_some() {
                crate::cached_project_backend_models(std::path::Path::new(required(args, "project")?))
            } else {
                let mut models = crate::cached_backend_models();
                models.remove("opencode");
                models
            };
            Ok(json!({"native":native,"backends":backends}))
        }
        _ => bail!("unknown tool: {name}"),
    }
}

/// Handles a single JSON-RPC request. Notifications do not produce a response.
pub async fn handle(req: Value) -> Option<Value> {
    handle_with_config(req, Config::load()).await
}

pub(crate) async fn handle_with_config(req: Value, cfg: Result<Config>) -> Option<Value> {
    let id = req.get("id").cloned();
    if req.get("jsonrpc") != Some(&json!("2.0")) || !req.get("method").is_some_and(Value::is_string) {
        return Some(json!({"jsonrpc":"2.0","id":id,"error":{"code":-32600,"message":"Invalid Request"}}));
    }
    let id = id?;
    let method = req["method"].as_str().unwrap_or_default();
    let result = match method {
        "initialize" => json!({"protocolVersion":"2025-06-18","capabilities":{"tools":{}},"serverInfo":{"name":"GreenCode","version":env!("CARGO_PKG_VERSION")}}),
        "ping" => json!({}),
        "tools/list" => json!({"tools":tools()}),
        "tools/call" => {
            let name = req["params"]["name"].as_str().unwrap_or("");
            let args = &req["params"]["arguments"];
            let outcome = match cfg { Ok(cfg) => call(name, args, cfg).await, Err(e) => Err(e) };
            match outcome {
                Ok(value) => json!({"content":[{"type":"text","text":value.to_string()}]}),
                Err(e) => json!({"content":[{"type":"text","text":format!("{e:#}")}],"isError":true}),
            }
        }
        _ => return Some(json!({"jsonrpc":"2.0","id":id,"error":{"code":-32601,"message":"Method not found"}})),
    };
    Some(json!({"jsonrpc":"2.0","id":id,"result":result}))
}

/// Run newline-delimited JSON-RPC over standard input and output. Requests run concurrently, so
/// `stop_thread` and `wait_thread` can arrive while another call is waiting.
pub async fn stdio() -> Result<()> {
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    let out = Arc::new(tokio::sync::Mutex::new(tokio::io::stdout()));
    let mut tasks = tokio::task::JoinSet::new();
    while let Some(line) = lines.next_line().await? {
        let out = out.clone();
        tasks.spawn(async move {
            let response = match serde_json::from_str(&line) {
                Ok(req) => handle(req).await,
                Err(_) => Some(json!({"jsonrpc":"2.0","id":null,"error":{"code":-32700,"message":"Parse error"}})),
            };
            if let Some(response) = response {
                let mut out = out.lock().await;
                let _ = out.write_all(format!("{response}\n").as_bytes()).await;
                let _ = out.flush().await;
            }
        });
    }
    while tasks.join_next().await.is_some() {}
    Ok(())
}
