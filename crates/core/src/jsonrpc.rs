//! Newline-delimited JSON-RPC 2.0 over a child process's stdio (MCP and ACP both use it).

use anyhow::{anyhow, bail, Result};
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    path::Path,
    process::Stdio,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStdin, Command},
    sync::{mpsc, oneshot},
};

pub enum Incoming {
    Request { id: Value, method: String, params: Value },
    Notification { method: String, params: Value },
}

type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value>>>>>;

pub struct Rpc {
    stdin: tokio::sync::Mutex<ChildStdin>,
    pending: Pending,
    next: AtomicU64,
    _child: Child,
}

impl Rpc {
    pub fn spawn(
        command: &[String],
        env: &[(String, String)],
        cwd: &Path,
    ) -> Result<(Arc<Rpc>, mpsc::UnboundedReceiver<Incoming>)> {
        let (prog, args) = command.split_first().ok_or_else(|| anyhow!("empty command"))?;
        let mut child = Command::new(prog)
            .args(args)
            .envs(env.iter().cloned())
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| anyhow!("cannot start `{prog}`: {e}"))?;
        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let pending: Pending = Default::default();
        let (tx, rx) = mpsc::unbounded_channel();
        let p = pending.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let Ok(v) = serde_json::from_str::<Value>(&line) else { continue };
                if let Some(method) = v["method"].as_str() {
                    let (method, params) = (method.to_string(), v["params"].clone());
                    let _ = tx.send(match v.get("id") {
                        Some(id) if !id.is_null() => Incoming::Request { id: id.clone(), method, params },
                        _ => Incoming::Notification { method, params },
                    });
                } else if let Some(reply) = v["id"].as_u64().and_then(|id| p.lock().unwrap().remove(&id)) {
                    let _ = reply.send(match v.get("error") {
                        Some(e) if !e.is_null() => Err(anyhow!("{}", e["message"].as_str().unwrap_or(&e.to_string()))),
                        _ => Ok(v["result"].clone()),
                    });
                }
            }
            // Process exited: fail everything still waiting.
            p.lock().unwrap().clear();
        });
        Ok((Arc::new(Rpc { stdin: tokio::sync::Mutex::new(stdin), pending, next: AtomicU64::new(1), _child: child }), rx))
    }

    async fn write(&self, v: Value) -> Result<()> {
        let mut line = v.to_string();
        line.push('\n');
        let mut stdin = self.stdin.lock().await;
        stdin.write_all(line.as_bytes()).await?;
        stdin.flush().await?;
        Ok(())
    }

    pub async fn request(&self, method: &str, params: Value) -> Result<Value> {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(id, tx);
        self.write(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})).await?;
        match rx.await {
            Ok(r) => r,
            Err(_) => bail!("process exited during `{method}`"),
        }
    }

    pub async fn notify(&self, method: &str, params: Value) -> Result<()> {
        self.write(json!({"jsonrpc": "2.0", "method": method, "params": params})).await
    }

    pub async fn respond(&self, id: Value, result: Result<Value, String>) -> Result<()> {
        self.write(match result {
            Ok(r) => json!({"jsonrpc": "2.0", "id": id, "result": r}),
            Err(m) => json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32603, "message": m}}),
        })
        .await
    }
}
