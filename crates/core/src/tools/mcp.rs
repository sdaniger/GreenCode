//! MCP client behind a single `mcp` tool: server tool schemas never enter the
//! prompt; the model searches for them when it needs one.

use crate::{
    jsonrpc::Rpc,
    provider::{send, sse},
};
use anyhow::{anyhow, bail, Result};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, HashMap},
    path::Path,
    sync::{Arc, Mutex},
};

enum Transport {
    Stdio(Arc<Rpc>),
    Http { http: reqwest::Client, url: String, headers: Vec<(String, String)>, session: Mutex<Option<String>> },
}

struct Conn {
    transport: Transport,
    tools: Vec<Value>,
}

pub struct Mcp {
    /// Claude Code's `.mcp.json` format: name -> {command, args, env} | {url, headers}.
    servers: BTreeMap<String, Value>,
    conns: tokio::sync::Mutex<HashMap<String, Arc<Conn>>>,
}

fn pairs(v: &Value) -> Vec<(String, String)> {
    v.as_object().into_iter().flatten().filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_string()))).collect()
}

impl Transport {
    async fn request(&self, method: &str, params: Value) -> Result<Value> {
        match self {
            Transport::Stdio(rpc) => rpc.request(method, params).await,
            Transport::Http { .. } => {
                let v = self.post(json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params})).await?;
                let v = v.ok_or_else(|| anyhow!("no response to `{method}`"))?;
                match v.get("error") {
                    Some(e) if !e.is_null() => bail!("{}", e["message"].as_str().unwrap_or(&e.to_string())),
                    _ => Ok(v["result"].clone()),
                }
            }
        }
    }

    async fn notify(&self, method: &str) -> Result<()> {
        match self {
            Transport::Stdio(rpc) => rpc.notify(method, json!({})).await,
            Transport::Http { .. } => self.post(json!({"jsonrpc": "2.0", "method": method})).await.map(|_| ()),
        }
    }

    /// Streamable HTTP: the reply is plain JSON or an SSE stream containing it.
    async fn post(&self, body: Value) -> Result<Option<Value>> {
        let Transport::Http { http, url, headers, session } = self else { unreachable!() };
        let sid = session.lock().unwrap().clone();
        let resp = send(|| {
            let mut r = http.post(url).header("Accept", "application/json, text/event-stream").json(&body);
            for (k, v) in headers {
                r = r.header(k, v);
            }
            if let Some(s) = &sid {
                r = r.header("Mcp-Session-Id", s);
            }
            r
        })
        .await?;
        if let Some(s) = resp.headers().get("mcp-session-id").and_then(|v| v.to_str().ok()) {
            *session.lock().unwrap() = Some(s.to_string());
        }
        let is_sse = resp.headers().get("content-type").and_then(|v| v.to_str().ok()).is_some_and(|c| c.contains("event-stream"));
        if !is_sse {
            let text = resp.text().await?;
            return Ok(serde_json::from_str(&text).ok());
        }
        let mut found = None;
        sse(resp, |data| {
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

impl Mcp {
    pub fn load(cwd: &Path) -> Mcp {
        let mut servers = BTreeMap::new();
        for path in [crate::home().join("mcp.json"), cwd.join(".mcp.json")] {
            let Ok(s) = std::fs::read_to_string(path) else { continue };
            let Ok(v) = serde_json::from_str::<Value>(&s) else { continue };
            for (k, v) in v["mcpServers"].as_object().into_iter().flatten() {
                servers.insert(k.clone(), v.clone());
            }
        }
        Mcp { servers, conns: Default::default() }
    }

    pub fn names(&self) -> Vec<String> {
        self.servers.keys().cloned().collect()
    }

    /// Servers start on first use.
    async fn conn(&self, name: &str, cwd: &Path) -> Result<Arc<Conn>> {
        let mut conns = self.conns.lock().await;
        if let Some(c) = conns.get(name) {
            return Ok(c.clone());
        }
        let cfg = self.servers.get(name).ok_or_else(|| anyhow!("unknown MCP server `{name}`"))?;
        let transport = if let Some(cmd) = cfg["command"].as_str() {
            let mut command = vec![cmd.to_string()];
            command.extend(cfg["args"].as_array().into_iter().flatten().filter_map(|a| a.as_str().map(String::from)));
            // shortcut: server-to-client requests (sampling, roots) are dropped; add a handler if a server needs them.
            Transport::Stdio(Rpc::spawn(&command, &pairs(&cfg["env"]), cwd)?.0)
        } else if let Some(url) = cfg["url"].as_str() {
            // shortcut: no OAuth; servers needing it must be given a token via `headers`.
            Transport::Http { http: reqwest::Client::new(), url: url.into(), headers: pairs(&cfg["headers"]), session: Mutex::new(None) }
        } else {
            bail!("MCP server `{name}` needs `command` or `url`");
        };
        transport
            .request("initialize", json!({
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": {"name": "greencode", "version": env!("CARGO_PKG_VERSION")},
            }))
            .await?;
        transport.notify("notifications/initialized").await?;
        // shortcut: first page only; follow `nextCursor` if a server has more tools than one page.
        let tools = transport.request("tools/list", json!({})).await?["tools"].as_array().cloned().unwrap_or_default();
        let c = Arc::new(Conn { transport, tools });
        conns.insert(name.to_string(), c.clone());
        Ok(c)
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
                hits.sort_by(|a, b| b.0.cmp(&a.0));
                // Without a query, list names only; schemas are the expensive part.
                for (_, server, t) in hits.iter().take(if words.is_empty() { 100 } else { 5 }) {
                    let desc = t["description"].as_str().unwrap_or("").lines().next().unwrap_or("");
                    out.push_str(&format!("{server}/{} - {desc}\n", t["name"].as_str().unwrap_or("")));
                    if !words.is_empty() {
                        out.push_str(&format!("  args: {}\n", t["inputSchema"]));
                    }
                }
                Ok(if out.is_empty() { "no matching tools".into() } else { out })
            }
            Some("call") => {
                let server = input["server"].as_str().ok_or_else(|| anyhow!("missing `server`"))?;
                let tool = input["tool"].as_str().ok_or_else(|| anyhow!("missing `tool`"))?;
                let args = if input["args"].is_object() { input["args"].clone() } else { json!({}) };
                let conn = self.conn(server, cwd).await?;
                let r = conn.transport.request("tools/call", json!({"name": tool, "arguments": args})).await?;
                let text: Vec<&str> = r["content"].as_array().into_iter().flatten().filter_map(|c| c["text"].as_str()).collect();
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
