//! MCP client behind a single `mcp` tool: server tool schemas never enter the
//! prompt; the model searches for them when it needs one.

use crate::{
    config::integrations::McpCfg,
    jsonrpc::Rpc,
    provider::{send, sse},
};
use anyhow::{anyhow, bail, Result};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, HashMap},
    path::Path,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

enum Transport {
    Stdio(Arc<Rpc>),
    Http {
        http: reqwest::Client,
        url: String,
        headers: Vec<(String, String)>,
        session: Mutex<Option<String>>,
    },
}

struct Conn {
    transport: Transport,
    tools: Vec<Value>,
    /// Last use, for `mcp.idleStopMinutes`.
    last: Mutex<Instant>,
}

impl Conn {
    fn touch(&self) {
        *self.last.lock().unwrap() = Instant::now();
    }
}

pub struct Mcp {
    /// Claude Code's `.mcp.json` format: name -> {command, args, env} | {url, headers}.
    servers: BTreeMap<String, Value>,
    conns: tokio::sync::Mutex<HashMap<String, Arc<Conn>>>,
    /// 0 keeps connections for the session.
    idle_stop: Duration,
    /// `mcp.notifications`: debounced `notifications/resources/updated` lines.
    notices: std::sync::Arc<crate::discovery::Notifier>,
    debounce: Duration,
    /// `mcp.builtinTransforms`: drop tools a native tool replaces.
    transforms: bool,
}

fn pairs(v: &Value) -> Vec<(String, String)> {
    v.as_object()
        .into_iter()
        .flatten()
        .filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_string())))
        .collect()
}

impl Transport {
    /// Ends a stdio server; HTTP sessions have nothing to stop.
    fn shutdown(&self) {
        if let Transport::Stdio(rpc) = self {
            rpc.shutdown();
        }
    }

    async fn request(&self, method: &str, params: Value) -> Result<Value> {
        match self {
            Transport::Stdio(rpc) => rpc.request(method, params).await,
            Transport::Http { .. } => {
                let v = self
                    .post(json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}))
                    .await?;
                let v = v.ok_or_else(|| anyhow!("no response to `{method}`"))?;
                match v.get("error") {
                    Some(e) if !e.is_null() => {
                        bail!("{}", e["message"].as_str().unwrap_or(&e.to_string()))
                    }
                    _ => Ok(v["result"].clone()),
                }
            }
        }
    }

    async fn notify(&self, method: &str) -> Result<()> {
        match self {
            Transport::Stdio(rpc) => rpc.notify(method, json!({})).await,
            Transport::Http { .. } => self
                .post(json!({"jsonrpc": "2.0", "method": method}))
                .await
                .map(|_| ()),
        }
    }

    /// Streamable HTTP: the reply is plain JSON or an SSE stream containing it.
    async fn post(&self, body: Value) -> Result<Option<Value>> {
        let Transport::Http {
            http,
            url,
            headers,
            session,
        } = self
        else {
            unreachable!()
        };
        let sid = session.lock().unwrap().clone();
        // MCP transport keeps its own short policy; `retry.*` configures model requests, not JSON-RPC.
        let retry = crate::config::provider::Retry {
            max_retries: 2,
            base_delay_ms: 2000,
            max_delay_ms: 30_000,
            ..Default::default()
        };
        let resp = send(&retry, || {
            let mut r = http
                .post(url)
                .header("Accept", "application/json, text/event-stream")
                .json(&body);
            for (k, v) in headers {
                r = r.header(k, v);
            }
            if let Some(s) = &sid {
                r = r.header("Mcp-Session-Id", s);
            }
            r
        })
        .await?;
        if let Some(s) = resp
            .headers()
            .get("mcp-session-id")
            .and_then(|v| v.to_str().ok())
        {
            *session.lock().unwrap() = Some(s.to_string());
        }
        let is_sse = resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|c| c.contains("event-stream"));
        if !is_sse {
            let text = resp.text().await?;
            return Ok(serde_json::from_str(&text).ok());
        }
        let mut found = None;
        sse(resp, Default::default(), |data| {
            if let Ok(v) = serde_json::from_str::<Value>(data) {
                if v["id"] == body["id"] && v.get("method").is_none() {
                    found = Some(v);
                    return Ok(true);
                }
            }
            Ok(false)
        })
        .await?;
        Ok(found)
    }
}

/// `mcp.shared`: one `Mcp` per working directory in this process, so the agent and eval share a
/// server process instead of each spawning its own.
pub fn shared(cwd: &Path, project: bool, opts: &McpCfg) -> std::sync::Arc<Mcp> {
    use std::sync::{Arc, Mutex, OnceLock};
    static KNOWN: OnceLock<Mutex<BTreeMap<std::path::PathBuf, Arc<Mcp>>>> = OnceLock::new();
    let key = std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
    KNOWN
        .get_or_init(Default::default)
        .lock()
        .unwrap()
        .entry(key)
        .or_insert_with(|| Arc::new(Mcp::load_with_project(cwd, project, opts)))
        .clone()
}

impl Mcp {
    pub fn load(cwd: &Path) -> Mcp {
        Self::load_with_project(cwd, true, &McpCfg::default())
    }

    pub(crate) fn load_with_project(cwd: &Path, project: bool, opts: &McpCfg) -> Mcp {
        let mut servers = BTreeMap::new();
        for path in std::iter::once(crate::home().join("mcp.json")).chain(project.then(|| cwd.join(".mcp.json"))) {
            let Ok(s) = std::fs::read_to_string(path) else {
                continue;
            };
            let Ok(v) = serde_json::from_str::<Value>(&s) else {
                continue;
            };
            for (k, v) in v["mcpServers"].as_object().into_iter().flatten() {
                servers.insert(k.clone(), v.clone());
            }
        }
        Mcp {
            servers,
            conns: Default::default(),
            idle_stop: Duration::from_secs(opts.idle_stop_minutes.saturating_mul(60)),
            notices: Default::default(),
            debounce: Duration::from_millis(opts.notification_debounce_ms),
            transforms: opts.builtin_transforms,
        }
    }

    /// Lines to inject for `mcp.notifications`, once the debounce window elapsed.
    pub fn take_updates(&self, notifications: bool) -> Vec<String> {
        if !notifications {
            return Vec::new();
        }
        self.notices.drain(self.debounce).unwrap_or_default()
    }

    /// `mcp.evalNamespaces`: `server.tool(**args)` bindings for the eval prelude.
    pub fn eval_prelude(&self) -> String {
        if self.servers.is_empty() {
            return String::new();
        }
        crate::discovery::eval_prelude(&self.names())
    }

    pub fn names(&self) -> Vec<String> {
        self.servers.keys().cloned().collect()
    }

    /// Servers start on first use. An idle connection is dropped on the next MCP use.
    async fn conn(&self, name: &str, cwd: &Path) -> Result<Arc<Conn>> {
        let mut conns = self.conns.lock().await;
        // shortcut: idle servers are pruned when an MCP call arrives, not by a timer.
        if !self.idle_stop.is_zero() {
            conns.retain(|_, c| {
                if c.last.lock().unwrap().elapsed() < self.idle_stop {
                    return true;
                }
                c.transport.shutdown();
                false
            });
        }
        if let Some(c) = conns.get(name) {
            c.touch();
            return Ok(c.clone());
        }
        let cfg = self
            .servers
            .get(name)
            .ok_or_else(|| anyhow!("unknown MCP server `{name}`"))?;
        let transport = if let Some(cmd) = cfg["command"].as_str() {
            let mut command = vec![cmd.to_string()];
            command.extend(
                cfg["args"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|a| a.as_str().map(String::from)),
            );
            // shortcut: server-to-client requests (sampling, roots) are dropped; add a handler if a server needs them.
            let (rpc, mut incoming) = Rpc::spawn(&command, &pairs(&cfg["env"]), cwd)?;
            // `mcp.notifications`: resource changes queue for the next prompt, debounced.
            crate::jsonrpc::drain_updates(incoming, self.notices.clone());
            Transport::Stdio(rpc)
        } else if let Some(url) = cfg["url"].as_str() {
            // shortcut: no OAuth; servers needing it must be given a token via `headers`.
            Transport::Http {
                http: reqwest::Client::new(),
                url: url.into(),
                headers: pairs(&cfg["headers"]),
                session: Mutex::new(None),
            }
        } else {
            bail!("MCP server `{name}` needs `command` or `url`");
        };
        transport
            .request(
                "initialize",
                json!({
                    "protocolVersion": "2025-06-18",
                    "capabilities": {},
                    "clientInfo": {"name": "greencode", "version": env!("CARGO_PKG_VERSION")},
                }),
            )
            .await?;
        transport.notify("notifications/initialized").await?;
        // shortcut: first page only; follow `nextCursor` if a server has more tools than one page.
        let tools = transport.request("tools/list", json!({})).await?["tools"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        // `mcp.builtinTransforms`: a server a native tool replaces contributes no tools.
        let tools = match crate::discovery::transform(name, self.transforms) {
            Some(_) => Vec::new(),
            None => tools,
        };
        let c = Arc::new(Conn { transport, tools, last: Mutex::new(Instant::now()) });
        conns.insert(name.to_string(), c.clone());
        Ok(c)
    }

    /// Connects every server for `mcp.startupTimeoutMs`; 0 waits until they settle. Late servers start on first use.
    pub async fn warm(self: &Arc<Self>, cwd: &Path, timeout_ms: u64) {
        if self.servers.is_empty() {
            return;
        }
        let mut jobs = tokio::task::JoinSet::new();
        for name in self.servers.keys() {
            let (this, cwd, name) = (self.clone(), cwd.to_path_buf(), name.clone());
            jobs.spawn(async move { let _ = this.conn(&name, &cwd).await; });
        }
        let all = async { while jobs.join_next().await.is_some() {} };
        if timeout_ms == 0 {
            all.await;
        } else {
            let _ = tokio::time::timeout(Duration::from_millis(timeout_ms), all).await;
        }
    }

    pub async fn run(&self, input: &Value, cwd: &Path) -> Result<String> {
        match input["action"].as_str() {
            Some("search") => {
                let query = input["query"].as_str().unwrap_or("").to_lowercase();
                let words: Vec<&str> = query.split_whitespace().collect();
                let mut hits = Vec::new();
                let mut out = String::new();
                for name in self.servers.keys() {
                    let conn = match self.conn(name, cwd).await {
                        Ok(c) => c,
                        Err(e) => {
                            out.push_str(&format!("[{name}: {e}]\n"));
                            continue;
                        }
                    };
                    for t in &conn.tools {
                        let hay = format!("{} {}", t["name"], t["description"]).to_lowercase();
                        let score = words.iter().filter(|w| hay.contains(**w)).count();
                        if words.is_empty() || score > 0 {
                            hits.push((score, name.clone(), t.clone()));
                        }
                    }
                }
                hits.sort_by_key(|a| std::cmp::Reverse(a.0));
                // Without a query, list names only; schemas are the expensive part.
                for (_, server, t) in hits.iter().take(if words.is_empty() { 100 } else { 5 }) {
                    let desc = t["description"]
                        .as_str()
                        .unwrap_or("")
                        .lines()
                        .next()
                        .unwrap_or("");
                    out.push_str(&format!(
                        "{server}/{} - {desc}\n",
                        t["name"].as_str().unwrap_or("")
                    ));
                    if !words.is_empty() {
                        out.push_str(&format!("  args: {}\n", t["inputSchema"]));
                    }
                }
                Ok(if out.is_empty() {
                    "no matching tools".into()
                } else {
                    out
                })
            }
            Some("call") => {
                let server = input["server"]
                    .as_str()
                    .ok_or_else(|| anyhow!("missing `server`"))?;
                let tool = input["tool"]
                    .as_str()
                    .ok_or_else(|| anyhow!("missing `tool`"))?;
                let args = if input["args"].is_object() {
                    input["args"].clone()
                } else {
                    json!({})
                };
                let conn = self.conn(server, cwd).await?;
                let r = conn
                    .transport
                    .request("tools/call", json!({"name": tool, "arguments": args}))
                    .await?;
                let text: Vec<&str> = r["content"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|c| c["text"].as_str())
                    .collect();
                let text = text.join("\n");
                if r["isError"].as_bool().unwrap_or(false) {
                    bail!(text);
                }
                Ok(text)
            }
            _ => bail!("action must be `search` or `call`"),
        }
    }
}
