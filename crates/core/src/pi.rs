//! Pi and Oh My Pi's native JSONL protocol. No second agent loop or prompt injection.

use crate::{
    config::Protocol,
    permission::{self, Perms},
    session::{Meta, Session},
    Backend, Block, BoxFut, Decision, Event, EventTx, Message, Role, Usage,
};
use anyhow::{bail, Context, Result};
use base64::{engine::general_purpose::STANDARD, Engine};
use serde_json::{json, Value};
use std::{collections::VecDeque, process::Stdio, sync::Arc};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader, Lines},
    process::{Child, ChildStdin, ChildStdout},
    sync::{mpsc, oneshot, Mutex},
};

// The hook changes execution permission only, never the model's prompt or tool schemas.
pub(crate) const APPROVAL_EXTENSION: &str = r#"export default function (pi: any) {
  pi.on("tool_call", async (event: any, ctx: any) => {
    const allowed = await ctx.ui.confirm("GreenCode tool approval", JSON.stringify({tool: event.toolName, input: event.input}));
    if (!allowed) return {block: true, reason: "The user denied this action"};
  });
}
"#;

const PI_RELOAD_EXTENSION: &str = r#"import { getAgentDir, getPackageDir } from "@earendil-works/pi-coding-agent";
export default function (pi: any) {
  pi.registerCommand("greencode-reload", {description: "Reload Pi resources", handler: async (_args: string, ctx: any) => {
    await ctx.reload();
  }});
  pi.registerCommand("greencode-changelog", {description: "Show Pi changelog", handler: async (_args: string, ctx: any) => {
    const fs = await import("node:fs/promises");
    const path = await import("node:path");
    const text = await fs.readFile(path.join(getPackageDir(), "CHANGELOG.md"), "utf8");
    const sections = text.split(/(?=^## )/m);
    ctx.ui.notify(sections.length > 1 ? sections.slice(1).reverse().join("") : text, "info");
  }});
  pi.registerCommand("greencode-hotkeys", {description: "Show Pi terminal hotkeys", handler: async (_args: string, ctx: any) => {
    const path = await import("node:path");
    const { pathToFileURL } = await import("node:url");
    // shortcut: Pi exposes no public keybinding manager; update this path if its package layout changes.
    const { KeybindingsManager, KEYBINDINGS } = await import(pathToFileURL(path.join(getPackageDir(), "dist/core/keybindings.js")).href);
    const keys = KeybindingsManager.create().getEffectiveConfig();
    const lines = Object.entries(KEYBINDINGS).map(([name, info]: [string, any]) => `${name}: ${(Array.isArray(keys[name]) ? keys[name] : [keys[name]]).filter(Boolean).join(", ") || "(unbound)"} — ${info.description || ""}`);
    ctx.ui.notify("Pi terminal keybindings (not GreenCode shortcuts):\n" + lines.join("\n") + "\nPi terminal keybindings; extension shortcuts not included.", "info");
  }});
  pi.registerCommand("greencode-trust", {description: "Manage Pi project trust", handler: async (_args: string, ctx: any) => {
    const path = await import("node:path");
    const { pathToFileURL } = await import("node:url");
    // shortcut: Pi does not export its trust store; update this path if its package layout changes.
    const { ProjectTrustStore, getProjectTrustOptions } = await import(pathToFileURL(path.join(getPackageDir(), "dist/core/trust-manager.js")).href);
    const store = new ProjectTrustStore(getAgentDir());
    const saved = store.getEntry(ctx.cwd);
    const options = getProjectTrustOptions(ctx.cwd);
    const choice = await ctx.ui.select(`Pi project trust: ${ctx.cwd}\nCurrent session: ${ctx.isProjectTrusted() ? "trusted" : "untrusted"}; saved: ${saved ? `${saved.decision ? "trusted" : "untrusted"} (${saved.path})` : "none"}`, options.map((o: any) => o.label));
    const option = options.find((o: any) => o.label === choice);
    if (!option) return;
    const changes = option.updates.map((u: any) => `${u.path}: ${u.decision === null ? "remove explicit decision" : u.decision ? "trusted" : "untrusted"}`).join("\n");
    if (!await ctx.ui.confirm("Save Pi project trust?", `${changes}\nTrust allows project-local Pi resources to run. Restart Pi for changes to take effect.`)) return;
    store.setMany(option.updates);
    ctx.ui.notify("Pi project trust saved; restart the Pi session for it to take effect.", "info");
  }});
  pi.registerCommand("greencode-export-jsonl", {description: "Export Pi session as JSONL", handler: async (args: string, ctx: any) => {
    const fs = await import("node:fs/promises");
    const path = await import("node:path");
    const os = await import("node:os");
    let dest = JSON.parse(args);
    if (dest === "~" || dest.startsWith("~/")) dest = path.join(os.homedir(), dest.slice(2));
    dest = path.resolve(ctx.cwd, dest);
    const session = ctx.sessionManager;
    const header = session.getHeader();
    if (!header) throw new Error("Session header is unavailable");
    let parentId = null;
    const entries = session.getBranch().map((entry: any) => {
      const item = {...entry, parentId};
      parentId = entry.id;
      return item;
    });
    await fs.mkdir(path.dirname(dest), {recursive: true});
    const activePath = session.getSessionFile();
    if (activePath) {
      const canonical = async (p: string) => path.join(await fs.realpath(path.dirname(p)), path.basename(p));
      if (await canonical(dest) === await canonical(path.resolve(activePath))) throw new Error("Cannot overwrite the active session");
    }
    const data = [header, ...entries].map((entry: any) => JSON.stringify(entry)).join("\n") + "\n";
    const before = await fs.lstat(dest).catch((e: any) => { if (e.code === "ENOENT") return null; throw e; });
    if (before) {
      if (!before.isFile()) throw new Error("Export destination is not a regular file");
      const current = activePath ? await fs.stat(activePath).catch((e: any) => { if (e.code === "ENOENT") return null; throw e; }) : null;
      if (current && current.dev === before.dev && current.ino === before.ino) throw new Error("Cannot overwrite the active session");
      if (!await ctx.ui.confirm("Overwrite export?", dest)) return;
    }
    const staging = await fs.mkdtemp(path.join(path.dirname(dest), ".greencode-export-"));
    try {
      const temp = path.join(staging, "session.jsonl");
      await fs.writeFile(temp, data, {mode: before ? before.mode & 0o777 : 0o600});
      if (before) {
        const after = await fs.lstat(dest);
        if (before.dev !== after.dev || before.ino !== after.ino || before.size !== after.size || before.mtimeMs !== after.mtimeMs || before.ctimeMs !== after.ctimeMs) throw new Error("Export destination changed; try again");
        await fs.rename(temp, dest);
      } else {
        await fs.link(temp, dest);
      }
    } finally {
      await fs.rm(staging, {recursive: true, force: true});
    }
    ctx.ui.notify(`Session exported to: ${dest}`, "info");
  }});
}
"#;

// Built-ins that Pi/OMP only execute in their terminal UI. GreenCode maps them to RPC commands; the
// descriptions are shown next to the commands the agent itself advertises.
const PI_LOCAL: &[(&str, &str)] = &[
    ("new", "新しいセッションを開始"),
    ("clear", "新しいセッションを開始"),
    ("compact", "コンテキストを圧縮"),
    ("model", "モデルを表示・切り替え"),
    ("thinking", "思考レベルを表示・変更"),
    ("session", "セッション統計を表示"),
    ("stats", "セッション統計を表示"),
    ("export", "セッションをHTMLまたはJSONLに出力"),
    ("name", "セッション名を表示・設定"),
    ("clone", "現在位置でセッションを複製"),
    ("copy", "最後の返答を表示"),
    ("fork", "過去のメッセージから分岐"),
    ("tree", "セッションツリーを表示"),
    ("reload", "拡張・スキル・設定を再読み込み"),
    ("changelog", "Pi の変更履歴を表示"),
    ("hotkeys", "Pi 端末のキー割り当てを表示"),
    ("trust", "Pi のプロジェクト信頼設定を変更"),
    ("resume", "セッションファイルへ切り替え"),
];
// OMP already runs its text-mode built-ins through `prompt` (see `get_available_commands`); these are the rest.
const OMP_LOCAL: &[(&str, &str)] = &[
    ("new", "新しいセッションを開始"),
    ("clear", "新しいセッションを開始"),
    ("thinking", "思考レベルを表示・変更"),
    ("copy", "最後の返答を表示"),
    ("fork", "現在位置から分岐"),
    ("branch", "過去のメッセージから分岐"),
    ("rewind", "過去のメッセージから分岐"),
    ("tree", "セッションツリーを表示"),
    ("resume", "セッションファイルへ切り替え"),
    ("login", "OAuthでログイン"),
    ("logout", "保存済みの認証情報を削除"),
    ("agents", "サブエージェント一覧"),
    ("hub", "サブエージェント一覧"),
    ("subagents", "サブエージェント一覧"),
];
// Pi/OMP would send these to the model as plain text, so refuse them instead.
const PI_UI_ONLY: &[&str] = &[
    "settings", "scoped-models", "login", "logout", "llama", "import", "share", "bug", "mcp",
    "quit", "debug",
];
const OMP_UI_ONLY: &[&str] = &[
    "settings", "setup", "providers", "plan", "plan-review", "vibe", "goal", "guided-goal", "loop", "queue",
    "collab", "join", "leave", "open", "hotkeys", "extensions", "status", "git", "local", "delete", "seance",
    "btw", "tan", "omfg", "cleanse", "debug", "exit", "restart", "skills", "live", "record", "pause", "quit", "q",
];

struct Chunks {
    id: String,
    count: u64,
    next: u64,
    size: usize,
    bytes: Vec<u8>,
}

#[derive(Default)]
struct Decoder {
    chunks: Option<Chunks>,
}

impl Decoder {
    fn decode(&mut self, value: Value) -> Result<Option<Value>> {
        if value["type"] != "rpc_chunk" {
            if self.chunks.is_some() {
                bail!("interrupted RPC chunk sequence");
            }
            return Ok(Some(value));
        }
        let id = value["chunkId"].as_str().context("missing chunkId")?;
        let index = value["index"].as_u64().context("missing chunk index")?;
        let count = value["count"]
            .as_u64()
            .filter(|n| *n > 0 && *n <= 65536)
            .context("invalid chunk count")?;
        let size = value["byteLength"]
            .as_u64()
            .filter(|n| *n <= 64 * 1024 * 1024)
            .context("RPC frame exceeds 64 MiB")? as usize;
        if self.chunks.is_none() {
            if index != 0 {
                bail!("RPC chunk sequence did not start at zero");
            }
            self.chunks = Some(Chunks {
                id: id.into(),
                count,
                next: 0,
                size,
                bytes: Vec::new(),
            });
        }
        let c = self.chunks.as_mut().unwrap();
        if c.id != id || c.next != index || c.count != count || c.size != size {
            bail!("invalid RPC chunk sequence");
        }
        let bytes = STANDARD.decode(value["data"].as_str().context("missing chunk data")?)?;
        if c.bytes.len() + bytes.len() > size {
            bail!("RPC chunk exceeds declared byteLength");
        }
        c.bytes.extend(bytes);
        c.next += 1;
        if c.next != c.count {
            return Ok(None);
        }
        let c = self.chunks.take().unwrap();
        if c.bytes.len() != size {
            bail!("RPC frame byteLength mismatch");
        }
        Ok(Some(serde_json::from_slice(&c.bytes)?))
    }
}

pub struct Pi {
    child: Child,
    stdin: ChildStdin,
    lines: Lines<BufReader<ChildStdout>>,
    stderr: Arc<Mutex<Vec<u8>>>,
    decoder: Decoder,
    protocol: Protocol,
    meta: Meta,
    perms: Arc<Perms>,
    next: u64,
    initialized: bool,
    settled: bool,
    streamed: bool,
    live_tx: mpsc::UnboundedSender<Value>,
    live_rx: mpsc::UnboundedReceiver<Value>,
    models: Vec<Value>,
    commands: Vec<String>,
    idle_event: Option<Value>,
    pending_frames: VecDeque<Value>,
    /// Declared last so it is released after the child has been signalled; no other process may write this thread's session meanwhile.
    _writer: crate::handoff::WriterLock,
}

impl Pi {
    pub async fn start(
        command: &[String],
        protocol: Protocol,
        meta: &Meta,
        perms: Arc<Perms>,
    ) -> Result<Pi> {
        let writer = crate::handoff::writer_lock(&meta.id)?;
        if crate::handoff::is_open(&meta.id) {
            bail!("a terminal handoff is open for this thread; finish or abandon it first");
        }
        let (program, args) = command.split_first().context("empty Pi command")?;
        let saved = Session::runtime_for_backend(&meta.id, &meta.backend)?;
        let dir = crate::home().join("provider-sessions").join(&meta.id);
        std::fs::create_dir_all(&dir)?;
        let extension = dir.join("greencode-permissions.ts");
        std::fs::write(&extension, APPROVAL_EXTENSION)?;
        let mut cmd = tokio::process::Command::new(program);
        let mode = if protocol == Protocol::Omp {
            "rpc-ui"
        } else {
            "rpc"
        };
        cmd.args(args)
            .args(["--mode", mode, "--session-dir"])
            .arg(&dir)
            .arg("--extension")
            .arg(extension);
        if protocol == Protocol::Pi {
            let reload = dir.join("greencode-reload.ts");
            std::fs::write(&reload, PI_RELOAD_EXTENSION)?;
            cmd.arg("--extension").arg(reload);
        }
        if let Some(source) = crate::delegate::pi_extension(meta).await {
            // Holds this thread's bearer token, so owner-only.
            let path = dir.join("greencode-delegate.ts");
            crate::remote::write_private(&path, source.as_bytes())?;
            cmd.arg("--extension").arg(path);
        }
        if let Some(file) = saved["sessionFile"].as_str() {
            cmd.arg("--session").arg(file);
        } else if !Session::can_start_external(&meta.id)? {
            bail!("this thread has no native session handle; start a new thread instead of silently losing its context");
        }
        let mut child = cmd
            .env("PATH", crate::config::agent_path()?)
            .current_dir(&meta.cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .with_context(|| {
                format!("cannot start `{program}`; install it or configure its executable path")
            })?;
        let stdin = child.stdin.take().unwrap();
        let lines = BufReader::new(child.stdout.take().unwrap()).lines();
        let stderr = Arc::new(Mutex::new(Vec::new()));
        let capture = stderr.clone();
        let mut err = child.stderr.take().unwrap();
        tokio::spawn(async move {
            let mut bytes = [0; 2048];
            while let Ok(n) = err.read(&mut bytes).await {
                if n == 0 {
                    break;
                }
                let mut out = capture.lock().await;
                out.extend_from_slice(&bytes[..n]);
                if out.len() > 8192 {
                    let end = out.len() - 8192;
                    out.drain(..end);
                }
            }
        });
        let (live_tx, live_rx) = mpsc::unbounded_channel();
        Ok(Pi {
            child,
            stdin,
            lines,
            stderr,
            decoder: Decoder::default(),
            protocol,
            meta: meta.clone(),
            perms,
            next: 0,
            initialized: false,
            settled: false,
            streamed: false,
            live_tx,
            live_rx,
            models: vec![],
            commands: vec![],
            idle_event: None,
            pending_frames: VecDeque::new(),
            _writer: writer,
        })
    }

    async fn write(&mut self, value: &Value) -> Result<()> {
        let mut bytes = serde_json::to_vec(value)?;
        bytes.push(b'\n');
        self.stdin.write_all(&bytes).await?;
        self.stdin.flush().await?;
        Ok(())
    }

    async fn read(&mut self) -> Result<Value> {
        if let Some(value) = self.pending_frames.pop_front() { return Ok(value); }
        self.read_wire().await
    }

    async fn read_wire(&mut self) -> Result<Value> {
        loop {
            let Some(line) = self.lines.next_line().await? else {
                let status = self.child.wait().await?;
                let err = self.stderr.lock().await;
                bail!(
                    "agent exited ({status}): {}",
                    String::from_utf8_lossy(&err).trim()
                );
            };
            if line.trim().is_empty() {
                continue;
            }
            let v: Value = serde_json::from_str(&line).context("agent emitted invalid JSONL")?;
            if let Some(value) = self.decoder.decode(v)? {
                return Ok(value);
            }
        }
    }

    async fn request(&mut self, mut command: Value, tx: &EventTx) -> Result<Value> {
        if !command.is_object() || command["type"].as_str().is_none() {
            bail!("RPC command needs a string `type`");
        }
        self.next += 1;
        let id = format!("gc-{}", self.next);
        command["id"] = json!(id);
        self.write(&command).await?;
        loop {
            let value = self.read().await?;
            if value["type"] == "response" && value["id"] == id {
                if value["success"] != true {
                    bail!(
                        "{}: {}",
                        command["type"],
                        value["error"].as_str().unwrap_or("unknown RPC error")
                    );
                }
                return Ok(value["data"].clone());
            }
            self.handle(value, tx).await?;
        }
    }

    async fn initialize(&mut self, tx: &EventTx) -> Result<()> {
        if self.initialized {
            return Ok(());
        }
        self.execute(json!({"type": "get_state"}), tx).await?;
        // OMP advertises v2 in its ready frame; Pi has no negotiation command.
        if self.protocol == Protocol::Omp {
            self.request(
                json!({"type": "negotiate_protocol", "protocolVersion": 2}),
                tx,
            )
            .await?;
            self.request(
                json!({"type":"set_subagent_subscription","level":"progress"}),
                tx,
            )
            .await?;
            self.request(json!({"type":"set_ask_dialog","enabled":true}), tx).await?;
        }
        self.initialized = true;
        self.execute(json!({"type": "get_available_models"}), tx)
            .await?;
        if self.meta.model != "default" {
            self.execute(self.model_command(&self.meta.model)?, tx)
                .await?;
        }
        let command = if self.protocol == Protocol::Omp {
            "get_available_commands"
        } else {
            "get_commands"
        };
        self.execute(json!({"type": command}), tx).await?;
        self.execute(json!({"type":"get_available_thinking_levels"}), tx)
            .await?;
        Ok(())
    }

    async fn execute(&mut self, command: Value, tx: &EventTx) -> Result<Value> {
        let name = command["type"]
            .as_str()
            .context("RPC command needs `type`")?
            .to_string();
        let data = self.request(command, tx).await?;
        if name == "get_available_models" {
            self.models = data["models"].as_array().cloned().unwrap_or_default();
        }
        let shown = if matches!(name.as_str(), "get_commands" | "get_available_commands") {
            self.track_commands(&data)
        } else {
            data.clone()
        };
        if name == "get_state" {
            if let Some(file) = data["sessionFile"].as_str() {
                Session::save_runtime(
                    &self.meta.id,
                    &json!({"backend": self.meta.backend, "sessionFile": file}),
                )?;
            }
        }
        let _ = tx.send(Event::Remote {
            command: name,
            data: shown,
        });
        Ok(data)
    }

    /// Remembers what the agent itself advertises (names and aliases) and returns the list the UI should
    /// show: those plus the built-ins GreenCode maps to RPC commands.
    fn track_commands(&mut self, data: &Value) -> Value {
        let flat = |c: &Value| -> Vec<String> {
            c.as_str()
                .or(c["name"].as_str())
                .into_iter()
                .chain(c["aliases"].as_array().into_iter().flatten().filter_map(|a| a.as_str()))
                .map(|n| n.trim_start_matches('/').to_string())
                .collect()
        };
        self.commands = data["commands"].as_array().into_iter().flatten().flat_map(flat).collect();
        let mut shown = data.clone();
        if !shown["commands"].is_array() {
            shown["commands"] = json!([]);
        }
        let table = if self.protocol == Protocol::Omp { OMP_LOCAL } else { PI_LOCAL };
        let list = shown["commands"].as_array_mut().unwrap();
        for command in data["commands"].as_array().into_iter().flatten() {
            for alias in command["aliases"].as_array().into_iter().flatten().filter_map(Value::as_str) {
                let alias = alias.trim_start_matches('/');
                if !alias.is_empty() && !list.iter().any(|c| c.as_str().or(c["name"].as_str()).is_some_and(|n| n.trim_start_matches('/') == alias)) {
                    list.push(json!({"name": alias, "description": command["description"], "source": command["source"]}));
                }
            }
        }
        for (name, description) in table {
            if !self.commands.iter().any(|c| c == name) {
                list.push(json!({"name": name, "description": description, "source": "greencode"}));
            }
        }
        shown
    }

    fn model_command(&self, slug: &str) -> Result<Value> {
        let pair = self.models.iter().find(|m| {
            format!(
                "{}/{}",
                m["provider"].as_str().unwrap_or(""),
                m["id"].as_str().unwrap_or("")
            ) == slug
        });
        let (provider, model) = match pair {
            Some(m) => (m["provider"].as_str().unwrap(), m["id"].as_str().unwrap()),
            None => slug
                .split_once('/')
                .filter(|(p, m)| !p.is_empty() && !m.is_empty())
                .context("model must be provider/model")?,
        };
        Ok(json!({"type":"set_model","provider":provider,"modelId":model}))
    }

    fn save_message(&mut self, value: &Value) -> Result<()> {
        if let Some(message) = native_message(value) {
            let s = Session::open(&self.meta.id).context("missing thread transcript")?;
            s.append(&message)?;
        }
        Ok(())
    }

    async fn handle(&mut self, v: Value, tx: &EventTx) -> Result<()> {
        let kind = v["type"].as_str().unwrap_or("");
        match kind {
            "message_start" => self.streamed = false,
            "message_update" => {
                let e = &v["assistantMessageEvent"];
                if e["type"] == "text_delta" {
                    self.streamed = true;
                    let _ = tx.send(Event::Text(e["delta"].as_str().unwrap_or("").into()));
                }
            }
            "message_end" => {
                let m = &v["message"];
                self.save_message(m)?;
                if m["role"] == "assistant" {
                    if !self.streamed {
                        let text = text_content(&m["content"]);
                        if !text.is_empty() {
                            let _ = tx.send(Event::Text(text));
                        }
                    }
                    let u = &m["usage"];
                    let _ = tx.send(Event::Usage(Usage {
                        input: u["input"].as_u64().unwrap_or(0),
                        output: u["output"].as_u64().unwrap_or(0),
                        cache_read: u["cacheRead"].as_u64().unwrap_or(0),
                        cache_write: u["cacheWrite"].as_u64().unwrap_or(0),
                    }));
                    if m["stopReason"] == "error" {
                        let _ = tx.send(Event::Error(
                            m["errorMessage"]
                                .as_str()
                                .unwrap_or("provider error")
                                .into(),
                        ));
                    }
                }
            }
            "tool_execution_start" => {
                let _ = tx.send(Event::ToolStart {
                    id: v["toolCallId"].as_str().unwrap_or("").into(),
                    name: v["toolName"].as_str().unwrap_or("tool").into(),
                    input: v["args"].clone(),
                });
            }
            "tool_execution_end" => {
                let _ = tx.send(Event::ToolEnd {
                    id: v["toolCallId"].as_str().unwrap_or("").into(),
                    output: text_content(&v["result"]["content"]),
                    is_error: v["isError"].as_bool().unwrap_or(false),
                });
            }
            "agent_settled" | "session_settled" => self.settled = true,
            "prompt_result" => {
                if v["status"] == "error" {
                    let _ = tx.send(Event::Error(
                        v["error"]["message"]
                            .as_str()
                            .unwrap_or("provider error")
                            .into(),
                    ));
                }
                if v["sessionSettled"] == true {
                    self.settled = true;
                }
            }
            "extension_ui_request" => self.dialog(&v, tx).await?,
            // An extension handler or command failed; the agent process and this turn are still fine.
            "extension_error" => {
                let _ = tx.send(Event::Error(format!(
                    "extension {}: {}",
                    v["extensionPath"].as_str().unwrap_or("?"),
                    v["error"].as_str().unwrap_or("failed")
                )));
            }
            "rpc_frame_error" => bail!(
                "agent {kind}: {}",
                v["error"].as_str().unwrap_or("protocol failure")
            ),
            "available_commands_update" => {
                let data = self.track_commands(&v);
                let _ = tx.send(Event::Remote {
                    command: kind.into(),
                    data,
                });
            }
            "response" if v["success"] == false => {
                let _ = tx.send(Event::Error(
                    v["error"].as_str().unwrap_or("RPC command failed").into(),
                ));
            }
            "ready"
            | "agent_start"
            | "agent_end"
            | "turn_start"
            | "turn_end"
            | "tool_execution_update" => {}
            _ => {
                let _ = tx.send(Event::Remote {
                    command: kind.into(),
                    data: v.clone(),
                });
            }
        }
        Ok(())
    }

    async fn dialog(&mut self, v: &Value, tx: &EventTx) -> Result<()> {
        let method = v["method"].as_str().unwrap_or("");
        let mut response = if method == "confirm" && v["title"] == "GreenCode tool approval" {
            let call: Value = serde_json::from_str(
                v["message"]
                    .as_str()
                    .context("missing tool approval payload")?,
            )?;
            let tool = call["tool"].as_str().context("missing approval tool")?;
            // Plan mode is read-only for every backend, whatever "always allow" says.
            let plan = self.perms.mode() == permission::Mode::Plan && !permission::read_only(tool, &call["input"]);
            let allowed = if plan {
                false
            } else {
                match self.perms.verdict(tool, &call["input"], &self.meta.cwd) {
                    permission::Verdict::Allow => true,
                    permission::Verdict::Deny(_) => false,
                    permission::Verdict::Ask(ask) => {
                        let (reply, rx) = oneshot::channel();
                        let _ = tx.send(Event::Permission {
                            tool: tool.into(),
                            summary: permission::summary(tool, &call["input"]),
                            reply,
                        });
                        match rx.await.unwrap_or(Decision::Deny) {
                            Decision::Allow => true,
                            Decision::Always if ask.remember => {
                                self.perms.always(ask.key);
                                true
                            }
                            Decision::Always => true,
                            Decision::Deny => false,
                        }
                    }
                }
            };
            json!({"confirmed": allowed})
        } else if matches!(method, "select" | "confirm" | "input" | "editor" | "ask") {
            let (reply, rx) = oneshot::channel();
            let _ = tx.send(Event::Dialog {
                request: v.clone(),
                reply,
            });
            let mut rx = rx;
            let deadline = v["timeout"].as_u64().map(|ms| tokio::time::Instant::now() + std::time::Duration::from_millis(ms));
            let answer = loop {
                tokio::select! {
                    answer = &mut rx => break answer.unwrap_or_else(|_| json!({"cancelled": true})),
                    _ = async { match deadline { Some(at) => tokio::time::sleep_until(at).await, None => std::future::pending().await } } => {
                        let _ = tx.send(Event::Remote { command: "dialog_closed".into(), data: json!({"id":v["id"]}) });
                        break json!({"cancelled": true, "timedOut": true});
                    }
                    frame = self.read_wire() => {
                        let frame = frame?;
                        if frame["type"] == "extension_ui_request" && frame["method"] == "cancel" && frame["targetId"] == v["id"] {
                            let _ = tx.send(Event::Remote { command: "dialog_closed".into(), data: json!({"id":v["id"]}) });
                            return Ok(());
                        }
                        self.pending_frames.push_back(frame);
                    }
                }
            };
            if method == "ask" && answer.get("answers").is_none() && answer["cancelled"] != true {
                json!({"cancelled": true})
            } else {
                answer
            }
        } else if method == "cancel" {
            return Ok(()); // the agent already closed that dialog; nothing to answer
        } else if method == "open_url" {
            // OAuth from `/login`; `launchUrl` is the short loopback redirect that survives line wrapping.
            let url = v["launchUrl"].as_str().or(v["url"].as_str()).unwrap_or("");
            let mut note = format!("ブラウザで開いてください: {url}");
            if let Some(hint) = v["instructions"].as_str() {
                note.push('\n');
                note.push_str(hint);
            }
            let _ = tx.send(Event::Notify(note));
            return Ok(());
        } else {
            let _ = tx.send(Event::Remote {
                command: method.into(),
                data: v.clone(),
            });
            return Ok(());
        };
        if !response.is_object() {
            response = json!({"cancelled": true});
        }
        response["type"] = json!("extension_ui_response");
        response["id"] = v["id"].clone();
        self.write(&response).await
    }

    /// An advertised extension/prompt/skill command must reach the agent, not a built-in shortcut.
    fn advertised(&self, rest: &str) -> bool {
        let name = rest.split_whitespace().next().unwrap_or("");
        self.commands.iter().any(|c| c == name)
    }

    /// Runs a built-in that Pi/OMP only implement in their terminal UI through its RPC equivalent.
    /// `Ok(false)`: not such a command, send it to the agent as a prompt.
    async fn slash(&mut self, name: &str, args: &str, tx: &EventTx) -> Result<bool> {
        let omp = self.protocol == Protocol::Omp;
        let (table, ui_only) = if omp {
            (OMP_LOCAL, OMP_UI_ONLY)
        } else {
            (PI_LOCAL, PI_UI_ONLY)
        };
        if !table.iter().any(|(n, _)| *n == name) {
            if ui_only.contains(&name) {
                bail!(
                    "/{name} only exists in {}'s terminal UI and has no RPC equivalent; run it in the {} CLI",
                    if omp { "Oh My Pi" } else { "Pi" },
                    if omp { "omp" } else { "pi" }
                );
            }
            return Ok(false);
        }
        let say = |text: String| {
            let _ = tx.send(Event::Notify(text));
        };
        match name {
            "new" | "clear" => {
                let d = self.execute(json!({"type":"new_session"}), tx).await?;
                if d["cancelled"] == true {
                    say("新しいセッションは取り消されました".into());
                }
            }
            "compact" => {
                let command = if args.is_empty() {
                    json!({"type":"compact"})
                } else {
                    json!({"type":"compact","customInstructions":args})
                };
                self.execute(command, tx).await?;
            }
            "thinking" if args.is_empty() => {
                let s = self.execute(json!({"type":"get_state"}), tx).await?;
                let l = self.execute(json!({"type":"get_available_thinking_levels"}), tx).await?;
                let levels: Vec<_> = l["levels"].as_array().into_iter().flatten().filter_map(|v| v.as_str()).collect();
                say(format!(
                    "思考レベル: {} (選択可能: {})",
                    s["thinkingLevel"].as_str().unwrap_or("-"),
                    levels.join(", ")
                ));
            }
            "thinking" => {
                self.execute(json!({"type":"set_thinking_level","level":args}), tx).await?;
            }
            "model" if args.is_empty() => {
                let s = self.execute(json!({"type":"get_state"}), tx).await?;
                let slug = |m: &Value| format!("{}/{}", m["provider"].as_str().unwrap_or(""), m["id"].as_str().unwrap_or(""));
                let all: Vec<_> = self.models.iter().take(60).map(slug).collect();
                say(format!("モデル: {}\n選択可能: {}", slug(&s["model"]), all.join(", ")));
            }
            "model" => {
                let command = self.model_command(args)?;
                self.execute(command, tx).await?;
            }
            "session" | "stats" => {
                self.execute(json!({"type":"get_session_stats"}), tx).await?;
            }
            "changelog" => {
                if !self.advertised("greencode-changelog") { bail!("Pi changelog extension is unavailable"); }
                let result = self.request(json!({"type":"prompt","message":"/greencode-changelog"}), tx).await?;
                if result["disposition"] != "handled" { bail!("Pi did not handle the changelog command"); }
            }
            "hotkeys" => {
                if !self.advertised("greencode-hotkeys") { bail!("Pi hotkeys extension is unavailable"); }
                let result = self.request(json!({"type":"prompt","message":"/greencode-hotkeys"}), tx).await?;
                if result["disposition"] != "handled" { bail!("Pi did not handle the hotkeys command"); }
            }
            "trust" => {
                if !self.advertised("greencode-trust") { bail!("Pi trust extension is unavailable"); }
                let result = self.request(json!({"type":"prompt","message":"/greencode-trust"}), tx).await?;
                if result["disposition"] != "handled" { bail!("Pi did not handle the trust command"); }
            }
            "reload" => {
                if !self.advertised("greencode-reload") {
                    bail!("Pi reload extension is unavailable");
                }
                let result = self.request(json!({"type":"prompt","message":"/greencode-reload"}), tx).await?;
                if result["disposition"] != "handled" {
                    bail!("Pi did not handle the reload command");
                }
                self.execute(json!({"type":"get_commands"}), tx).await?;
                self.execute(json!({"type":"get_available_models"}), tx).await?;
            }
            "export" => {
                let args = args.strip_prefix('"').and_then(|s| s.strip_suffix('"'))
                    .or_else(|| args.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')))
                    .unwrap_or(args);
                if !omp && args.ends_with(".jsonl") {
                    if !self.advertised("greencode-export-jsonl") {
                        bail!("Pi JSONL export extension is unavailable");
                    }
                    let result = self.request(json!({"type":"prompt","message":format!("/greencode-export-jsonl {}", serde_json::to_string(args)?)}), tx).await?;
                    if result["disposition"] != "handled" {
                        bail!("Pi did not handle the JSONL export command");
                    }
                    self.execute(json!({"type":"get_state"}), tx).await?;
                    return Ok(true);
                }
                let command = if args.is_empty() {
                    json!({"type":"export_html"})
                } else {
                    json!({"type":"export_html","outputPath":args})
                };
                self.execute(command, tx).await?;
            }
            "name" if args.is_empty() => {
                let s = self.execute(json!({"type":"get_state"}), tx).await?;
                say(format!("セッション名: {}", s["sessionName"].as_str().unwrap_or("(未設定)")));
            }
            "name" => {
                self.execute(json!({"type":"set_session_name","name":args}), tx).await?;
            }
            "clone" => {
                self.execute(json!({"type":"clone"}), tx).await?;
            }
            "copy" => {
                let d = self.execute(json!({"type":"get_last_assistant_text"}), tx).await?;
                if let Some(text) = d["text"].as_str().filter(|t| !t.is_empty()) {
                    let _ = tx.send(Event::Remote { command: "copy_text".into(), data: json!({"text": text}) });
                } else {
                    say("返答がまだありません".into());
                }
            }
            "fork" | "branch" | "rewind" => {
                if !args.is_empty() || (omp && name == "fork") {
                    let mut command = json!({"type": if name == "fork" {"fork"} else {"branch"}});
                    if !args.is_empty() {
                        command["entryId"] = json!(args);
                    }
                    let d = self.execute(command, tx).await?;
                    say(if d["cancelled"] == true {
                        "分岐は取り消されました".into()
                    } else {
                        format!("分岐しました: {}", d["text"].as_str().unwrap_or(""))
                    });
                } else {
                    let kind = if omp { "get_branch_messages" } else { "get_fork_messages" };
                    let d = self.execute(json!({"type":kind}), tx).await?;
                    let lines: Vec<_> = d["messages"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .map(|m| {
                            let id = m["entryId"].as_str().or(m["id"].as_str()).unwrap_or("?");
                            format!("{id}  {}", clip(m["text"].as_str().unwrap_or(""), 80))
                        })
                        .collect();
                    say(if lines.is_empty() {
                        "分岐できるメッセージがありません".into()
                    } else {
                        format!("/{name} <entryId> で分岐:\n{}", lines.join("\n"))
                    });
                }
            }
            "tree" => {
                let d = self.execute(json!({"type":"get_tree"}), tx).await?;
                let mut lines = vec![];
                tree_lines(&d["tree"], d["leafId"].as_str().unwrap_or(""), 0, &mut lines);
                say(if lines.is_empty() { "セッションは空です".into() } else { lines.join("\n") });
            }
            "resume" if args.is_empty() => say("使い方: /resume <セッションファイルのパス>".into()),
            "resume" => {
                let d = self.execute(json!({"type":"switch_session","sessionPath":args}), tx).await?;
                if d["cancelled"] == true {
                    say("切り替えは取り消されました".into());
                }
            }
            "login" if args.is_empty() => {
                let d = self.execute(json!({"type":"get_login_providers"}), tx).await?;
                let lines: Vec<_> = d["providers"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter(|p| p["available"] != false)
                    .map(|p| {
                        format!(
                            "{} ({}){}",
                            p["id"].as_str().unwrap_or("?"),
                            p["name"].as_str().unwrap_or(""),
                            if p["authenticated"] == true { " 認証済み" } else { "" }
                        )
                    })
                    .collect();
                say(format!("/login <provider>:\n{}", lines.join("\n")));
            }
            "login" => {
                // Long-running: the agent interleaves `open_url` and `input` requests that `request` services.
                self.execute(json!({"type":"login","providerId":args}), tx).await?;
                say(format!("{args} にログインしました"));
            }
            "logout" if args.is_empty() => say("使い方: /logout <provider> [credentialId]".into()),
            "logout" => {
                let (provider, id) = args.split_once(char::is_whitespace).unwrap_or((args, ""));
                let d = self
                    .execute(json!({"type":"get_logout_accounts","providerId":provider}), tx)
                    .await?;
                let accounts = d["accounts"].as_array().cloned().unwrap_or_default();
                let chosen = match id.trim() {
                    "" if accounts.len() == 1 => accounts[0]["credentialId"].as_i64(),
                    "" => None,
                    id => id.parse().ok(),
                };
                match chosen {
                    Some(credential) => {
                        self.execute(json!({"type":"logout","providerId":provider,"credentialId":credential}), tx).await?;
                        say(format!("{provider} の認証情報 {credential} を削除しました"));
                    }
                    None => {
                        let lines: Vec<_> = accounts
                            .iter()
                            .map(|a| {
                                format!(
                                    "{}  {} {}",
                                    a["credentialId"],
                                    a["label"].as_str().unwrap_or(""),
                                    a["detail"].as_str().unwrap_or("")
                                )
                            })
                            .collect();
                        say(if lines.is_empty() {
                            format!("{provider} に保存済みの認証情報はありません")
                        } else {
                            format!("/logout {provider} <credentialId>:\n{}", lines.join("\n"))
                        });
                    }
                }
            }
            "agents" | "hub" | "subagents" => {
                let d = self.execute(json!({"type":"get_subagents"}), tx).await?;
                let lines: Vec<_> = d["subagents"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|a| clip(&a.to_string(), 200))
                    .collect();
                say(if lines.is_empty() { "サブエージェントはありません".into() } else { lines.join("\n") });
            }
            _ => unreachable!("{name} is in the command table without a handler"),
        }
        self.execute(json!({"type":"get_state"}), tx).await?;
        Ok(true)
    }

    async fn run(&mut self, text: String, tx: &EventTx) -> Result<()> {
        self.initialize(tx).await?;
        if Session::open(&self.meta.id).is_none() {
            if self.meta.title.is_empty() {
                self.meta.title = text.chars().take(60).collect();
            }
            Session::create(&self.meta)?;
        }
        if let Some(rest) = text.strip_prefix('/').filter(|r| !self.advertised(r)) {
            let (name, args) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
            if self.slash(name, args.trim(), tx).await? {
                return Ok(());
            }
        }
        self.settled = false;
        let r = self
            .execute(json!({"type": "prompt", "message": text}), tx)
            .await?;
        if r["agentInvoked"] == false {
            self.settled = true;
        } else if r["disposition"] == "handled" {
            // An extension command can start its own turn (`pi.sendUserMessage`); then wait for it to settle.
            let state = self.execute(json!({"type": "get_state"}), tx).await?;
            self.settled = state["isStreaming"] != true;
        }
        while !self.settled {
            if let Some(frame) = self.pending_frames.pop_front() {
                self.handle(frame, tx).await?;
                continue;
            }
            tokio::select! {
                line = self.lines.next_line() => {
                    let line = line?.context("agent exited during a prompt")?;
                    if !line.trim().is_empty() {
                        let value = serde_json::from_str(&line).context("agent emitted invalid JSONL")?;
                        if let Some(v) = self.decoder.decode(value)? { self.handle(v, tx).await?; }
                    }
                }
                Some(command) = self.live_rx.recv() => {
                    if let Err(e) = self.execute(command, tx).await { let _ = tx.send(Event::Error(format!("{e:#}"))); }
                }
            }
        }
        self.execute(json!({"type": "get_state"}), tx).await?;
        Ok(())
    }
}

fn clip(s: &str, max: usize) -> String {
    s.replace('\n', " ").chars().take(max).collect()
}

/// Indented outline of the session tree, capped so the notice stays readable. The marked entry is the leaf.
fn tree_lines(nodes: &Value, leaf: &str, depth: usize, out: &mut Vec<String>) {
    for n in nodes.as_array().into_iter().flatten() {
        if out.len() >= 80 {
            return;
        }
        let e = &n["entry"];
        let id = e["id"].as_str().unwrap_or("");
        let what = e["message"]["role"].as_str().or(e["type"].as_str()).unwrap_or("?");
        out.push(format!(
            "{}{}{id} [{what}] {}",
            "  ".repeat(depth.min(20)),
            if id == leaf { "* " } else { "" },
            clip(&text_content(&e["message"]["content"]), 60)
        ));
        tree_lines(&n["children"], leaf, depth + 1, out);
    }
}


/// A Pi/OMP session message as a GreenCode transcript message; `None` for roles and empty content GreenCode does not keep.
pub(crate) fn native_message(value: &Value) -> Option<Message> {
    let role = value["role"].as_str().unwrap_or("");
    let mut content = Vec::new();
    if role == "toolResult" {
        content.push(Block::ToolResult {
            tool_use_id: value["toolCallId"].as_str().unwrap_or("").into(),
            content: text_content(&value["content"]),
            is_error: value["isError"].as_bool().unwrap_or(false),
        });
    } else {
        for c in value["content"].as_array().into_iter().flatten() {
            match c["type"].as_str() {
                Some("text") => content.push(Block::Text {
                    text: c["text"].as_str().unwrap_or("").into(),
                }),
                Some("toolCall") => content.push(Block::ToolUse {
                    id: c["id"].as_str().unwrap_or("").into(),
                    name: c["name"].as_str().unwrap_or("").into(),
                    input: c["arguments"].clone(),
                }),
                _ => {}
            }
        }
        if let Some(text) = value["content"].as_str() {
            content.push(Block::Text { text: text.into() });
        }
    }
    (!content.is_empty() && matches!(role, "user" | "assistant" | "toolResult")).then(|| Message {
        role: if role == "assistant" { Role::Assistant } else { Role::User },
        content,
    })
}

fn text_content(content: &Value) -> String {
    if let Some(s) = content.as_str() {
        return s.into();
    }
    content
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|c| c["text"].as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

impl Backend for Pi {
    fn prompt<'a>(&'a mut self, text: String, tx: &'a EventTx) -> BoxFut<'a, Result<()>> {
        Box::pin(self.run(text, tx))
    }

    fn live(&self) -> Option<mpsc::UnboundedSender<Value>> {
        Some(self.live_tx.clone())
    }

    fn control<'a>(&'a mut self, command: Value, tx: &'a EventTx) -> BoxFut<'a, Result<()>> {
        Box::pin(async move {
            self.initialize(tx).await?;
            let command = if command["type"] == "select_model" {
                self.model_command(command["model"].as_str().context("missing model")?)?
            } else {
                command
            };
            self.execute(command, tx).await?;
            self.execute(json!({"type": "get_state"}), tx).await?;
            Ok(())
        })
    }
    fn idle<'a>(&'a mut self) -> BoxFut<'a, Result<()>> {
        Box::pin(async move {
            if self.idle_event.is_none() {
                self.idle_event = Some(self.read().await?);
            }
            Ok(())
        })
    }

    fn handle_idle<'a>(&'a mut self, tx: &'a EventTx) -> BoxFut<'a, Result<()>> {
        Box::pin(async move {
            if let Some(value) = self.idle_event.take() {
                self.handle(value, tx).await?;
            }
            Ok(())
        })
    }


    fn cancel<'a>(&'a mut self, tx: &'a EventTx) -> BoxFut<'a, Result<()>> {
        Box::pin(async move {
            tokio::time::timeout(std::time::Duration::from_secs(10), async {
                self.execute(json!({"type": "abort"}), tx).await?;
                while !self.settled {
                    let v = self.read().await?;
                    self.handle(v, tx).await?;
                }
                self.execute(json!({"type": "get_state"}), tx).await?;
                Ok::<_, anyhow::Error>(())
            })
            .await
            .context("agent did not acknowledge cancellation within 10 seconds")?
        })
    }

    /// Persists the exact session handle a terminal UI must resume, then stops the agent. It refuses while the
    /// agent is working, so nothing is cut off mid-write, and acknowledges only once the process is dead.
    fn handoff<'a>(&'a mut self, tx: &'a EventTx) -> BoxFut<'a, Result<()>> {
        Box::pin(async move {
            if let Some(event) = self.idle_event.take() {
                self.handle(event, tx).await?;
            }
            let state = tokio::time::timeout(
                std::time::Duration::from_secs(10),
                self.request(json!({"type": "get_state"}), tx),
            )
            .await
            .context("agent did not report its session within 10 seconds")??;
            if state["isStreaming"] == true || state["isCompacting"] == true {
                bail!("the agent is still working; wait for it to finish or cancel it before handing off");
            }
            let file = state["sessionFile"]
                .as_str()
                .context("the agent reported no session file; its context cannot be handed off")?;
            Session::save_runtime(&self.meta.id, &json!({"backend": self.meta.backend, "sessionFile": file}))?;
            if Session::runtime_for_backend(&self.meta.id, &self.meta.backend)?["sessionFile"] != file {
                bail!("the session file `{file}` was not saved");
            }
            self.child.kill().await.context("cannot stop the agent")?;
            self._writer.unlock()?;
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn chunk_decoder_is_lossless_and_rejects_invalid_sequences() {
        let bytes = serde_json::to_vec(&json!({"text": "日本語\u{2028}test"})).unwrap();
        let mut d = Decoder::default();
        let frame = |index, data: &[u8]| json!({"type":"rpc_chunk","chunkId":"c","index":index,"count":2,"byteLength":bytes.len(),"data":STANDARD.encode(data)});
        assert!(d.decode(frame(0, &bytes[..7])).unwrap().is_none());
        assert_eq!(
            d.decode(frame(1, &bytes[7..])).unwrap().unwrap()["text"],
            "日本語\u{2028}test"
        );
        assert!(d.decode(frame(1, &bytes[7..])).is_err());
        let mut d = Decoder::default();
        d.decode(frame(0, &bytes[..7])).unwrap();
        assert!(d.decode(json!({"type":"response"})).is_err());
    }
}
