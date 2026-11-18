//! MCP access to local GreenCode threads over stdio or the remote HTTP server.
use std::{collections::BTreeSet, sync::Arc, time::Duration};
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use crate::{config::Config, permission::Perms, session::{Meta, Session}, Block, Command, Decision, Event, Role};

fn tools() -> Value {
    let string = json!({"type":"string"});
    let entries = [
        ("list_projects", "List distinct project directories.", json!({}), vec![]),
        ("list_threads", "List threads, optionally filtered by project directory.", json!({"project":string}), vec![]),
        ("read_thread", "Read a thread's text-only transcript.", json!({"id":string,"max_messages":{"type":"integer","minimum":0}}), vec!["id"]),
        ("start_thread", "Start a thread and wait for its response. Approval requests are denied unless the configured mode allows the action.", json!({"cwd":string,"prompt":string,"backend":string,"model":string}), vec!["cwd","prompt"]),
        ("send_message", "Send a message and wait for its response. Approval requests are denied unless the thread mode allows the action.", json!({"id":string,"text":string}), vec!["id","text"]),
        ("list_models", "List configured native models and cached external backend models. Supply project to include its OpenCode models.", json!({"project":string}), vec![]),
    ];
    Value::Array(entries.into_iter().map(|(name, description, properties, required)| json!({"name":name,"description":description,"inputSchema":{"type":"object","properties":properties,"required":required}})).collect())
}

fn required<'a>(args: &'a Value, key: &str) -> Result<&'a str> {
    args.get(key).and_then(Value::as_str).filter(|s| !s.trim().is_empty()).with_context(|| format!("missing or empty {key}"))
}

async fn turn(cfg: Config, meta: Meta, text: String) -> Result<Value> {
    if !meta.cwd.is_dir() { bail!("cwd must be a directory"); }
    if let Some(policy) = &cfg.policy {
        anyhow::ensure!(policy.backend_allowed(&meta.backend), "backend blocked by policy");
    }
    let mode = Session::view(&meta.id)?.map_or(cfg.mode, |v| v.mode);
    if let Some(policy) = &cfg.policy { policy.check_mode(mode)?; }
    let id = meta.id.clone();
    let (tx, mut rx) = crate::spawn(&tokio::runtime::Handle::current(), Arc::new(cfg), meta, Arc::new(Perms::new(mode)));
    tx.send(Command::Prompt(text))?;
    let result = tokio::time::timeout(Duration::from_secs(600), async {
        let mut text = String::new();
        let mut error = None;
        while let Some(event) = rx.recv().await {
            match event {
                Event::Text(s) => text.push_str(&s),
                Event::Permission { reply, .. } => { let _ = reply.send(Decision::Deny); }
                Event::Dialog { reply, .. } => { let _ = reply.send(json!({"cancelled":true})); }
                Event::Error(e) => error = Some(e),
                Event::Done => {
                    if let Some(e) = error { bail!(e); }
                    if let Ok((_, messages)) = Session::load(&id) {
                        if let Some(last) = messages.iter().rev().find(|m| m.role == Role::Assistant) {
                            let final_text = last.content.iter().filter_map(|b| if let Block::Text { text } = b { Some(text.as_str()) } else { None }).collect::<Vec<_>>().join("\n");
                            if !final_text.is_empty() { text = final_text; }
                        }
                    }
                    return Ok(json!({"id":id,"text":text}));
                }
                _ => {}
            }
        }
        bail!("thread ended before completion")
    }).await;
    match result { Ok(r) => r, Err(_) => { let _ = tx.send(Command::Cancel); bail!("turn timed out after 10 minutes") } }
}

async fn call(name: &str, args: &Value, cfg: Config) -> Result<Value> {
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
            turn(cfg, meta, prompt).await
        }
        "send_message" => {
            let (meta, _) = Session::load(required(args, "id")?)?;
            turn(cfg, meta, required(args, "text")?.to_string()).await
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

/// Run newline-delimited JSON-RPC over standard input and output.
pub async fn stdio() -> Result<()> {
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    let mut out = tokio::io::stdout();
    while let Some(line) = lines.next_line().await? {
        let response = match serde_json::from_str(&line) {
            Ok(req) => handle(req).await,
            Err(_) => Some(json!({"jsonrpc":"2.0","id":null,"error":{"code":-32700,"message":"Parse error"}})),
        };
        if let Some(response) = response { out.write_all(format!("{response}\n").as_bytes()).await?; out.flush().await?; }
    }
    Ok(())
}
