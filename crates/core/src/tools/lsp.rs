use super::{def as tool_def, str_arg, OUTPUT_CHARS};
use crate::{
    config::{integrations::{LspCfg, LspServerCfg}, Config},
    permission::resolve,
    ToolDef,
};
use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    path::{Path, PathBuf},
    process::Stdio,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, LazyLock, Mutex,
    },
    time::Duration,
};
use tokio::{
    io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader},
    process::{Child, Command},
    sync::{oneshot, Notify},
};

pub fn def() -> ToolDef {
    tool_def("lsp", "Language server query. Path selects a file; line and column are 1-based. Symbols uses query; rename uses query as the new name.", json!({"action":{"enum":["diagnostics","definition","references","hover","symbols","rename"]},"path":{"type":"string"},"line":{"type":"integer"},"column":{"type":"integer"},"query":{"type":"string"}}), &["action"])
}

/// `lsp.requestTimeout` in seconds; servers are process-wide, so the timeout is too.
static REQUEST_TIMEOUT: AtomicU64 = AtomicU64::new(20);

/// `lsp.shared`: reach servers through the per-project broker instead of spawning a private child.
static SHARED: AtomicBool = AtomicBool::new(false);

/// `lsp.servers`: user-configured servers, consulted before the built-in table.
static CUSTOM: LazyLock<Mutex<BTreeMap<String, LspServerCfg>>> = LazyLock::new(Default::default);

/// Applies `[lsp]` before a session uses the servers; later sessions win.
pub fn configure(cfg: &Config) {
    REQUEST_TIMEOUT.store(cfg.integration_opts.lsp.request_timeout.clamp(1, 600), Ordering::Relaxed);
    SHARED.store(cfg.integration_opts.lsp.shared, Ordering::Relaxed);
    *CUSTOM.lock().unwrap() = cfg.integration_opts.lsp.servers.clone();
}

fn request_timeout() -> Duration {
    Duration::from_secs(REQUEST_TIMEOUT.load(Ordering::Relaxed))
}

const BUILTIN: &[(&[&str], &str, &[&str])] = &[
    (&["rs"], "rust-analyzer", &[]),
    (&["go"], "gopls", &["serve"]),
    (&["ts", "tsx", "js", "jsx"], "typescript-language-server", &["--stdio"]),
    (&["py"], "pyright-langserver", &["--stdio"]),
];

/// The server (executable name, args) for a file extension, installed or not: `lsp.servers` first, then built-ins.
fn server_spec(ext: &str) -> Option<(String, Vec<String>)> {
    let custom = CUSTOM.lock().unwrap();
    if let Some((name, s)) = custom.iter().find(|(_, s)| s.extensions.iter().any(|e| e.trim_start_matches('.') == ext)) {
        return Some((name.clone(), s.args.clone()));
    }
    BUILTIN
        .iter()
        .find(|(exts, ..)| exts.contains(&ext))
        .map(|(_, name, args)| (name.to_string(), args.iter().map(|a| a.to_string()).collect()))
}

fn server_for(path: &Path) -> Option<(String, Vec<String>)> {
    let (name, args) = server_spec(path.extension()?.to_str()?)?;
    crate::config::executable(&name).map(|_| (name, args))
}

pub(super) async fn write_frame(w: &mut (impl AsyncWrite + Unpin), value: &Value) -> Result<()> {
    let body = serde_json::to_vec(value)?;
    w.write_all(format!("Content-Length: {}\r\n\r\n", body.len()).as_bytes())
        .await?;
    w.write_all(&body).await?;
    w.flush().await?;
    Ok(())
}

pub(super) async fn read_frame(r: &mut (impl AsyncBufRead + Unpin)) -> Result<Value> {
    let mut length = None;
    loop {
        let mut line = String::new();
        if r.read_line(&mut line).await? == 0 {
            bail!("server exited");
        }
        if line == "\r\n" || line == "\n" {
            break;
        }
        if let Some((key, value)) = line.split_once(':') {
            if key.eq_ignore_ascii_case("content-length") {
                length = Some(value.trim().parse::<usize>()?);
            }
        }
    }
    let mut body = vec![0; length.ok_or_else(|| anyhow!("missing Content-Length"))?];
    r.read_exact(&mut body).await?;
    Ok(serde_json::from_slice(&body)?)
}

type Pending = HashMap<u64, oneshot::Sender<Result<Value>>>;
struct Server {
    stdin: tokio::sync::Mutex<Box<dyn AsyncWrite + Unpin + Send>>,
    pending: Mutex<Pending>,
    diagnostics: Mutex<HashMap<String, Value>>,
    changed: Notify,
    documents: tokio::sync::Mutex<HashMap<String, (String, u64)>>,
    next: AtomicU64,
    exited: AtomicBool,
    /// Set when the server reports the end of its work-done progress (project loaded).
    ready: AtomicBool,
    /// The private child; `None` when the broker owns it.
    _child: Option<Child>,
}

impl Server {
    async fn send(&self, value: Value) -> Result<()> {
        write_frame(&mut *self.stdin.lock().await, &value)
            .await
            .context("writing to language server")
    }
    async fn request(&self, method: &str, params: Value) -> Result<Value> {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        {
            let mut pending = self.pending.lock().unwrap();
            if self.exited.load(Ordering::Acquire) {
                bail!("language server exited");
            }
            pending.insert(id, tx);
        }
        let result = tokio::time::timeout(request_timeout(), async {
            self.send(json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}))
                .await?;
            rx.await.map_err(|_| anyhow!("language server exited"))?
        })
        .await;
        self.pending.lock().unwrap().remove(&id);
        match result {
            Err(_) => bail!(
                "language server request {method} timed out after {} seconds",
                request_timeout().as_secs()
            ),
            Ok(result) => result,
        }
    }
    async fn notify(&self, method: &str, params: Value) -> Result<()> {
        self.send(json!({"jsonrpc":"2.0","method":method,"params":params}))
            .await
    }
    async fn open(&self, path: &Path, uri: &str) -> Result<()> {
        let text = std::fs::read_to_string(path).with_context(|| path.display().to_string())?;
        let mut docs = self.documents.lock().await;
        match docs.get(uri) {
            None => {
                let language = match path.extension().and_then(|s| s.to_str()).unwrap_or("") {
                    "rs" => "rust",
                    "go" => "go",
                    "ts" => "typescript",
                    "tsx" => "typescriptreact",
                    "js" => "javascript",
                    "jsx" => "javascriptreact",
                    "py" => "python",
                    s => s,
                };
                self.notify("textDocument/didOpen", json!({"textDocument":{"uri":uri,"languageId":language,"version":1,"text":text}})).await?;
                docs.insert(uri.into(), (text, 1));
            }
            Some((old, version)) if old != &text => {
                let version = version + 1;
                self.diagnostics.lock().unwrap().remove(uri);
                self.notify("textDocument/didChange", json!({"textDocument":{"uri":uri,"version":version},"contentChanges":[{"text":text}]})).await?;
                // Servers commonly recompute diagnostics on save, not on every change.
                self.notify("textDocument/didSave", json!({"textDocument":{"uri":uri}}))
                    .await?;
                docs.insert(uri.into(), (text, version));
            }
            _ => {}
        }
        Ok(())
    }
}

type Servers = HashMap<(PathBuf, String), Arc<Server>>;
static SERVERS: LazyLock<tokio::sync::Mutex<Servers>> =
    LazyLock::new(|| tokio::sync::Mutex::new(HashMap::new()));

async fn start(root: &Path, name: &str, args: &[&str]) -> Result<Arc<Server>> {
    let exe = crate::config::executable(name)
        .ok_or_else(|| anyhow!("language server {name} not installed"))?;
    if SHARED.load(Ordering::Relaxed) {
        match shared_server(root, &exe, args).await {
            Ok(server) => return Ok(server),
            // The broker is optional: a project without one still gets a private server.
            Err(_) => {}
        }
    }
    let mut child = Command::new(exe)
        .args(args)
        .current_dir(root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("starting language server {name}"))?;
    let stdout = child.stdout.take().unwrap();
    let stdin = child.stdin.take().unwrap();
    assemble(root, Box::new(stdin), stdout, Some(child), false).await
}

/// The broker socket for `root`: one per project, under the GreenCode home.
pub fn broker_socket(root: &Path) -> PathBuf {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    root.hash(&mut hasher);
    crate::home().join("lsp").join(format!("{:x}.sock", hasher.finish()))
}

/// `lsp.shared`: connect to the per-project broker, starting it on demand, and run one server through it.
/// The broker returns raw framed LSP, so the rest of this module cannot tell the difference.
async fn shared_server(root: &Path, exe: &Path, args: &[&str]) -> Result<Arc<Server>> {
    shared_at(&broker_socket(root), root, exe, args).await
}

/// The broker speaks over a Unix socket; elsewhere `lsp.shared` reports that and the caller runs its own server.
#[cfg(not(unix))]
async fn shared_at(_socket: &Path, _root: &Path, _exe: &Path, _args: &[&str]) -> Result<Arc<Server>> {
    bail!("lsp.shared needs Unix sockets")
}

/// `shared_server` at an explicit socket, so tests can run a broker in-process.
#[cfg(unix)]
async fn shared_at(socket: &Path, root: &Path, exe: &Path, args: &[&str]) -> Result<Arc<Server>> {
    use tokio::net::UnixStream;
    let socket = socket.to_path_buf();
    let stream = match UnixStream::connect(&socket).await {
        Ok(stream) => stream,
        Err(_) => {
            if let Some(dir) = socket.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            let _ = Command::new(std::env::current_exe()?)
                .arg("lsp-broker")
                .arg(&socket)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()?;
            let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
            loop {
                match UnixStream::connect(&socket).await {
                    Ok(stream) => break stream,
                    Err(e) if tokio::time::Instant::now() >= deadline => return Err(e.into()),
                    Err(_) => tokio::time::sleep(Duration::from_millis(50)).await,
                }
            }
        }
    };
    let (mut read, mut write) = stream.into_split();
    let hello = json!({"root": root, "command": exe, "args": args});
    let body = serde_json::to_vec(&hello)?;
    write
        .write_all(format!("Content-Length: {}\r\n\r\n", body.len()).as_bytes())
        .await?;
    write.write_all(&body).await?;
    write.flush().await?;
    let mut reader = BufReader::new(read);
    let reply = read_frame(&mut reader).await?;
    if let Some(error) = reply["error"].as_str() {
        bail!("lsp broker: {error}");
    }
    // A warm server will not send its load progress again, so do not wait for it.
    let warm = reply["reused"] == json!(true);
    assemble(root, Box::new(write), reader, None, warm).await
}

/// Wires a server's streams: the reader loop, `initialize`, and the wait for the project to load.
async fn assemble(
    root: &Path,
    stdin: Box<dyn AsyncWrite + Unpin + Send>,
    stdout: impl tokio::io::AsyncRead + Unpin + Send + 'static,
    child: Option<Child>,
    warm: bool,
) -> Result<Arc<Server>> {
    let server = Arc::new(Server {
        stdin: tokio::sync::Mutex::new(stdin),
        pending: Mutex::new(HashMap::new()),
        diagnostics: Mutex::new(HashMap::new()),
        changed: Notify::new(),
        documents: tokio::sync::Mutex::new(HashMap::new()),
        next: AtomicU64::new(1),
        exited: AtomicBool::new(false),
        ready: AtomicBool::new(warm),
        _child: child,
    });
    let weak = Arc::downgrade(&server);
    tokio::spawn(async move {
        let mut reader = BufReader::new(stdout);
        while let Ok(message) = read_frame(&mut reader).await {
            let Some(server) = weak.upgrade() else { return };
            if let Some(method) = message["method"].as_str() {
                if method == "textDocument/publishDiagnostics" {
                    if let Some(uri) = message["params"]["uri"].as_str() {
                        server
                            .diagnostics
                            .lock()
                            .unwrap()
                            .insert(uri.to_string(), message["params"]["diagnostics"].clone());
                        server.changed.notify_waiters();
                    }
                } else if method == "$/progress" && message["params"]["value"]["kind"] == "end" {
                    server.ready.store(true, Ordering::Release);
                    server.changed.notify_waiters();
                } else if !message["id"].is_null() {
                    let result = if method == "workspace/configuration" {
                        json!(message["params"]["items"]
                            .as_array()
                            .map(|a| vec![Value::Null; a.len()])
                            .unwrap_or_default())
                    } else {
                        Value::Null
                    };
                    let _ = server
                        .send(json!({"jsonrpc":"2.0","id":message["id"],"result":result}))
                        .await;
                }
            } else if let Some(id) = message["id"].as_u64() {
                if let Some(tx) = server.pending.lock().unwrap().remove(&id) {
                    let result = if message.get("error").is_some() {
                        Err(anyhow!("language server error: {}", message["error"]))
                    } else {
                        Ok(message["result"].clone())
                    };
                    let _ = tx.send(result);
                }
            }
        }
        if let Some(server) = weak.upgrade() {
            let mut pending = server.pending.lock().unwrap();
            server.exited.store(true, Ordering::Release);
            pending.clear();
            server.changed.notify_waiters();
        }
    });
    let root_uri = url::Url::from_directory_path(root)
        .map_err(|_| anyhow!("invalid project root {}", root.display()))?;
    server.request("initialize", json!({"processId":std::process::id(),"rootUri":root_uri.as_str(),"capabilities":{"window":{"workDoneProgress":true}}})).await?;
    server.notify("initialized", json!({})).await?;
    // File queries are cancelled (`content modified`) until the server finishes loading the project; it
    // announces that with the end of its work-done progress. Servers that send no progress just start.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    while !server.ready.load(Ordering::Acquire) {
        let notified = server.changed.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        if server.exited.load(Ordering::Acquire)
            || tokio::time::timeout_at(deadline, notified).await.is_err()
        {
            break;
        }
    }
    Ok(server)
}

fn position(input: &Value) -> Result<Value> {
    let coord = |key: &str| {
        input[key]
            .as_u64()
            .filter(|n| *n > 0)
            .map(|n| n - 1)
            .ok_or_else(|| anyhow!("`{key}` must be a positive integer"))
    };
    Ok(json!({"line":coord("line")?,"character":coord("column")?}))
}

fn location(value: &Value, cwd: &Path) -> Option<String> {
    let uri = value["uri"]
        .as_str()
        .or_else(|| value["targetUri"].as_str())?;
    let path = url::Url::parse(uri).ok()?.to_file_path().ok()?;
    let path = path.strip_prefix(cwd).unwrap_or(&path);
    let range = value
        .get("range")
        .or_else(|| value.get("targetSelectionRange"))?;
    Some(format!(
        "{}:{}:{}",
        path.display(),
        range["start"]["line"].as_u64()? + 1,
        range["start"]["character"].as_u64()? + 1
    ))
}

fn hover(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Array(a) => a
            .iter()
            .map(hover)
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join("\n"),
        Value::Object(_) => value["value"].as_str().unwrap_or("").to_string(),
        _ => String::new(),
    }
}

fn cap(s: String) -> String {
    s.chars().take(OUTPUT_CHARS).collect()
}

/// Diagnostics for one file. `None` means the server never published them (it exited or stayed silent), which
/// is not the same as a clean file and must not be reported as one.
fn diagnostics_text(published: Option<&Value>) -> Result<String> {
    let Some(published) = published else {
        bail!("the language server published no diagnostics in time (it may still be indexing or have exited); the file's state is unknown")
    };
    let lines = published
        .as_array()
        .into_iter()
        .flatten()
        .map(|d| {
            format!(
                "{} {}:{} {}",
                match d["severity"].as_u64() {
                    Some(1) => "error",
                    Some(2) => "warning",
                    Some(3) => "info",
                    Some(4) => "hint",
                    _ => "note",
                },
                d["range"]["start"]["line"].as_u64().unwrap_or(0) + 1,
                d["range"]["start"]["character"].as_u64().unwrap_or(0) + 1,
                d["message"].as_str().unwrap_or("").trim()
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    Ok(if lines.is_empty() {
        "no diagnostics".into()
    } else {
        cap(lines)
    })
}

/// A language server answers null (or an empty list) while it is still indexing.
fn empty(value: &Value) -> bool {
    value.is_null() || value.as_array().is_some_and(Vec::is_empty) || value["contents"].is_null()
}

fn workspace_file(root: &Path) -> Option<PathBuf> {
    for (marker, sample) in [
        ("src/main.rs", "src/main.rs"),
        ("src/lib.rs", "src/lib.rs"),
        ("main.go", "main.go"),
        ("package.json", "index.ts"),
        ("pyproject.toml", "main.py"),
        ("setup.py", "main.py"),
    ] {
        if root.join(marker).is_file() {
            return Some(root.join(sample));
        }
    }
    fn walk(dir: &Path, depth: usize) -> Option<PathBuf> {
        let mut entries = std::fs::read_dir(dir)
            .ok()?
            .filter_map(Result::ok)
            .collect::<Vec<_>>();
        entries.sort_by_key(|e| e.file_name());
        for entry in entries {
            let path = entry.path();
            if path.is_file() && path.extension().and_then(|e| e.to_str()).is_some_and(|e| server_spec(e).is_some()) {
                return Some(path);
            }
            if depth > 0
                && entry.file_type().ok()?.is_dir()
                && !matches!(
                    entry.file_name().to_str(),
                    Some("target" | "node_modules" | ".git")
                )
            {
                if let Some(path) = walk(&path, depth - 1) {
                    return Some(path);
                }
            }
        }
        None
    }
    walk(root, 2)
}

/// A running server for `selection`'s language, started on first use.
async fn acquire(root: &Path, selection: &Path) -> Result<Arc<Server>> {
    let ext = selection.extension().and_then(|s| s.to_str()).unwrap_or("");
    let (name, args) = server_for(selection).ok_or_else(|| {
        let name = server_spec(ext).map_or("a language server".into(), |(name, _)| name);
        anyhow!("no language server for {ext}; install {name}")
    })?;
    let key = (root.to_path_buf(), name.clone());
    let mut all = SERVERS.lock().await;
    if all
        .get(&key)
        .is_some_and(|s| s.exited.load(Ordering::Acquire))
    {
        all.remove(&key);
    }
    if let Some(s) = all.get(&key) {
        return Ok(s.clone());
    }
    let s = start(root, &name, &args.iter().map(String::as_str).collect::<Vec<_>>()).await?;
    all.insert(key, s.clone());
    Ok(s)
}

pub async fn run(cwd: &Path, input: &Value) -> Result<String> {
    let action = str_arg(input, "action")?;
    if ![
        "diagnostics",
        "definition",
        "references",
        "hover",
        "symbols",
        "rename",
    ]
    .contains(&action)
    {
        bail!("unknown lsp action `{action}`");
    }
    if action == "symbols" || action == "rename" {
        str_arg(input, "query")?;
    }
    let pos = if ["definition", "references", "hover", "rename"].contains(&action) {
        Some(position(input)?)
    } else {
        None
    };
    let path = if action == "symbols" {
        input["path"].as_str()
    } else {
        Some(str_arg(input, "path")?)
    }
    .map(|p| resolve(cwd, p));
    let root = std::fs::canonicalize(cwd).with_context(|| cwd.display().to_string())?;
    let selection = path
        .clone()
        .or_else(|| workspace_file(&root))
        .ok_or_else(|| anyhow!("no language server: no supported file found in workspace"))?;
    let server = acquire(&root, &selection).await?;
    let uri = if let Some(path) = path.filter(|_| action != "symbols") {
        let path = std::fs::canonicalize(&path).with_context(|| path.display().to_string())?;
        let uri = url::Url::from_file_path(&path)
            .map_err(|_| anyhow!("invalid file path {}", path.display()))?
            .to_string();
        // Every action syncs the document: after an edit, a server that still holds an open overlay answers
        // semantic queries from the stale text. `open` sends didChange only when the text differs.
        server.open(&path, &uri).await?;
        Some(uri)
    } else {
        None
    };
    match action {
        "diagnostics" => {
            let uri = uri.unwrap();
            let deadline = tokio::time::Instant::now() + request_timeout();
            // Servers publish an empty list when a file opens and the real diagnostics later; keep the last
            // publication and stop early only on a non-empty one.
            let mut last = None;
            loop {
                let notified = server.changed.notified();
                tokio::pin!(notified);
                notified.as_mut().enable();
                if let Some(d) = server.diagnostics.lock().unwrap().get(&uri).cloned() {
                    let empty = d.as_array().is_some_and(Vec::is_empty);
                    last = Some(d);
                    if !empty {
                        break;
                    }
                }
                if server.exited.load(Ordering::Acquire)
                    || tokio::time::timeout_at(deadline, notified).await.is_err()
                {
                    break;
                }
            }
            let published = last;
            diagnostics_text(published.as_ref())
        }
        "symbols" => {
            let result = server
                .request(
                    "workspace/symbol",
                    json!({"query":str_arg(input, "query")?}),
                )
                .await?;
            Ok(cap(result
                .as_array()
                .into_iter()
                .flatten()
                .take(50)
                .filter_map(|s| {
                    let loc = location(&s["location"], &root)?;
                    let (path, _) = loc.rsplit_once(':')?;
                    Some(format!(
                        "{} {} {}",
                        s["name"].as_str().unwrap_or(""),
                        s["kind"],
                        path
                    ))
                })
                .collect::<Vec<_>>()
                .join("\n")))
        }
        _ => {
            let mut params = json!({"textDocument":{"uri":uri.unwrap()},"position":pos.unwrap()});
            if action == "references" {
                params["context"] = json!({"includeDeclaration":true});
            }
            if action == "rename" {
                params["newName"] = json!(str_arg(input, "query")?);
            }
            // A rename answers a WorkspaceEdit, which has no `contents`; only null means "not ready".
            let blank = |v: &Value| if action == "rename" { v.is_null() } else { empty(v) };
            // A cold server answers null (or `content modified`) until it has loaded the crate; retry briefly.
            // A cold server answers `content modified` or `file not found` until `cargo metadata` and indexing
            // finish, and null while it is still resolving: retry errors for longer than empty answers.
            let started = tokio::time::Instant::now();
            let (result, error) = loop {
                let (value, error) = match server
                    .request(&format!("textDocument/{action}"), params.clone())
                    .await
                {
                    Ok(value) => (value, None),
                    Err(e) => (Value::Null, Some(e)),
                };
                let waited = started.elapsed();
                if (error.is_none() && !blank(&value))
                    || server.exited.load(Ordering::Acquire)
                    || waited >= Duration::from_secs(15)
                    || (error.is_none() && waited >= Duration::from_secs(5))
                {
                    break (value, error);
                }
                tokio::time::sleep(Duration::from_millis(500)).await;
            };
            if blank(&result) {
                if let Some(e) = error {
                    return Err(e);
                }
            }
            if action == "rename" {
                return rename(&server, &root, &result).await;
            }
            if action == "hover" {
                return Ok(cap(hover(&result["contents"])));
            }
            let values = match &result {
                Value::Array(a) => a.clone(),
                Value::Null => vec![],
                _ => vec![result.clone()],
            };
            Ok(cap(values
                .iter()
                .filter_map(|v| location(v, &root))
                .collect::<Vec<_>>()
                .join("\n")))
        }
    }
}

/// Starts the workspace's server ahead of the first query (`lsp.lazy = false`).
pub async fn warm(cwd: &Path) {
    let Ok(root) = std::fs::canonicalize(cwd) else { return };
    let Some(file) = workspace_file(&root) else { return };
    let _ = acquire(&root, &file).await;
}

/// Which tool changed the file; `edit` does not format by default.
#[derive(Clone, Copy, PartialEq)]
pub(crate) enum After {
    Write,
    Edit,
}

/// Diagnostics already shown per file, for `lsp.diagnosticsDeduplicate`.
static SHOWN: LazyLock<Mutex<HashMap<PathBuf, BTreeSet<String>>>> = LazyLock::new(Default::default);

/// Diagnostics worth appending: errors only (warnings would ride along on nearly every edit), and (when
/// deduplicating) only the ones this file has not shown. The `lsp diagnostics` action still lists everything.
fn diagnostics_note(path: &Path, published: &Value, deduplicate: bool) -> Option<String> {
    let errors: Vec<Value> = published.as_array()?.iter().filter(|d| d["severity"].as_u64() == Some(1)).cloned().collect();
    if errors.is_empty() {
        // A file without errors forgets its entry, so the same errors after a fix are reported again.
        SHOWN.lock().unwrap().remove(path);
        return None;
    }
    let text = diagnostics_text(Some(&Value::Array(errors))).ok()?;
    if !deduplicate {
        return Some(format!("LSP diagnostics for {}:\n{text}", path.display()));
    }
    let mut shown = SHOWN.lock().unwrap();
    let seen = shown.entry(path.to_path_buf()).or_default();
    let new: Vec<&str> = text.lines().filter(|line| !seen.contains(*line)).collect();
    if new.is_empty() {
        return None;
    }
    seen.extend(new.iter().map(|line| line.to_string()));
    Some(format!("LSP diagnostics for {}:\n{}", path.display(), new.join("\n")))
}

/// Byte offset of an LSP (line, UTF-16 character) position; positions past the line end clamp to it.
fn position_byte(text: &str, line: u64, character: u64) -> Option<usize> {
    let (mut l, mut utf16) = (0u64, 0u64);
    for (i, ch) in text.char_indices() {
        if ch == '\n' {
            if l == line {
                return Some(i);
            }
            l += 1;
            utf16 = 0;
        } else if l == line {
            if utf16 >= character {
                return Some(i);
            }
            utf16 += ch.len_utf16() as u64;
        }
    }
    (l == line).then_some(text.len())
}

/// LSP `TextEdit`s applied to `text`; `None` for malformed or overlapping edits.
fn apply_edits(text: &str, edits: &Value) -> Option<String> {
    let mut edits: Vec<(usize, usize, &str)> = edits
        .as_array()?
        .iter()
        .map(|e| {
            Some((
                position_byte(text, e["range"]["start"]["line"].as_u64()?, e["range"]["start"]["character"].as_u64()?)?,
                position_byte(text, e["range"]["end"]["line"].as_u64()?, e["range"]["end"]["character"].as_u64()?)?,
                e["newText"].as_str()?,
            ))
        })
        .collect::<Option<_>>()?;
    if edits.is_empty() {
        return None;
    }
    // Apply back to front so earlier offsets stay valid; inserts at one position keep their listed order.
    let mut order: Vec<usize> = (0..edits.len()).collect();
    order.sort_by_key(|&i| std::cmp::Reverse((edits[i].0, i)));
    let mut out = text.to_string();
    let mut limit = text.len();
    for i in order {
        let (start, end, new) = std::mem::take(&mut edits[i]);
        if start > end || end > limit || !out.is_char_boundary(start) || !out.is_char_boundary(end) {
            return None;
        }
        out.replace_range(start..end, new);
        limit = start;
    }
    Some(out)
}

/// Applies a rename's `WorkspaceEdit`. Every new text is computed before anything is written, so a malformed
/// edit, a file outside the project, or a file create/rename/delete leaves every file untouched.
async fn rename(server: &Server, root: &Path, edit: &Value) -> Result<String> {
    // Servers send `documentChanges` or `changes`; with both, `documentChanges` is authoritative.
    let mut per_uri: BTreeMap<String, Vec<Value>> = BTreeMap::new();
    if let Some(changes) = edit["documentChanges"].as_array() {
        for change in changes {
            if change.get("kind").is_some() {
                bail!("the rename would create, rename or delete files; refusing");
            }
            let uri = change["textDocument"]["uri"].as_str().context("rename edit without a document uri")?;
            per_uri.entry(uri.into()).or_default().extend(change["edits"].as_array().into_iter().flatten().cloned());
        }
    } else if let Some(changes) = edit["changes"].as_object() {
        for (uri, edits) in changes {
            per_uri.entry(uri.clone()).or_default().extend(edits.as_array().into_iter().flatten().cloned());
        }
    }
    per_uri.retain(|_, edits| !edits.is_empty());
    if per_uri.is_empty() {
        bail!("the language server found nothing to rename at that position");
    }
    let mut planned = Vec::new();
    for (uri, edits) in per_uri {
        let path = url::Url::parse(&uri)
            .ok()
            .and_then(|u| u.to_file_path().ok())
            .with_context(|| format!("rename edit for a non-file uri {uri}"))?;
        let path = std::fs::canonicalize(&path).with_context(|| path.display().to_string())?;
        if !path.starts_with(root) {
            bail!("the rename would edit {} outside the project; refusing", path.display());
        }
        let text = std::fs::read_to_string(&path).with_context(|| path.display().to_string())?;
        let new = apply_edits(&text, &Value::Array(edits.clone()))
            .with_context(|| format!("the language server sent malformed or overlapping edits for {}", path.display()))?;
        planned.push((path, uri, new, edits.len()));
    }
    let mut out = format!("renamed in {} files", planned.len());
    for (path, uri, new, count) in &planned {
        std::fs::write(path, new).with_context(|| path.display().to_string())?;
        server.open(path, uri).await?;
        out.push_str(&format!("\n{}: {count}", path.strip_prefix(root).unwrap_or(path).display()));
    }
    Ok(out)
}

/// Formats and/or collects diagnostics after `write`/`edit`; `None` leaves the tool output unchanged.
pub(crate) async fn after_change(cfg: &Config, cwd: &Path, path: &Path, kind: After) -> Option<String> {
    let opts: &LspCfg = &cfg.integration_opts.lsp;
    let want_format = kind == After::Write && opts.format_on_write;
    let want_diagnostics = match kind {
        After::Write => opts.diagnostics_on_write,
        After::Edit => opts.diagnostics_on_edit,
    };
    if !want_format && !want_diagnostics {
        return None;
    }
    let root = std::fs::canonicalize(cwd).ok()?;
    let path = std::fs::canonicalize(path).ok()?;
    let (name, _) = server_for(&path)?;
    let server = {
        let all = SERVERS.lock().await;
        all.get(&(root.clone(), name.to_string()))
            .filter(|s| !s.exited.load(Ordering::Acquire))
            .cloned()
    };
    let server = match server {
        Some(server) => server,
        // Formatting was asked for, so pay the one-time warmup.
        None if want_format => acquire(&root, &path).await.ok()?,
        None => {
            // shortcut: diagnostics-only starts the server in the background and the next write sees them;
            // blocking the first write of every file for a warmup costs more than one write without diagnostics.
            tokio::spawn(async move { let _ = acquire(&root, &path).await; });
            return None;
        }
    };
    let uri = url::Url::from_file_path(&path).ok()?.to_string();
    server.open(&path, &uri).await.ok()?;
    if want_format {
        if let Ok(edits) = server
            .request(
                "textDocument/formatting",
                json!({"textDocument": {"uri": uri}, "options": {"tabSize": 4, "insertSpaces": true}}),
            )
            .await
        {
            let formatted = std::fs::read_to_string(&path).ok().and_then(|old| apply_edits(&old, &edits));
            if let Some(text) = formatted {
                if std::fs::write(&path, text).is_ok() {
                    server.open(&path, &uri).await.ok()?;
                }
            }
        }
    }
    if !want_diagnostics {
        return None;
    }
    let deadline = tokio::time::Instant::now() + request_timeout();
    let mut last = None;
    loop {
        let notified = server.changed.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        if let Some(d) = server.diagnostics.lock().unwrap().get(&uri).cloned() {
            let empty = d.as_array().is_some_and(Vec::is_empty);
            last = Some(d);
            if !empty {
                break;
            }
        }
        if server.exited.load(Ordering::Acquire) {
            break;
        }
        // Servers publish an empty list on change and the real diagnostics later: wait longer for the first
        // publication, then a short grace after an empty one.
        let wait = if last.is_some() {
            Duration::from_millis(500)
        } else {
            request_timeout()
        };
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        if tokio::time::timeout(wait.min(left), notified).await.is_err() {
            break;
        }
    }
    diagnostics_note(&path, last.as_ref()?, opts.diagnostics_deduplicate)
}


/// A warm language server the broker keeps between connections.
#[cfg(unix)]
struct Pooled {
    stdin: Option<tokio::process::ChildStdin>,
    /// Where the server's stdout goes; `None` while no client is attached (bytes are dropped).
    sink: Arc<Mutex<Option<tokio::sync::mpsc::UnboundedSender<Vec<u8>>>>>,
    exited: Arc<AtomicBool>,
    command: String,
    _child: Child,
}

/// `lsp.shared`: the per-project broker. Clients send one JSON hello `{root, command, args}` and get one
/// `{reused}` reply, then raw framed LSP both ways. A warm server for the same command is reused; when a
/// client disconnects its server returns to the pool, and the broker exits after `idle_ms` with nothing
/// attached, killing the pool. `GREENCODE_LSP_BROKER_IDLE_MS` shortens the idle for tests.
#[cfg(unix)]
pub async fn serve_broker(socket: &Path) -> Result<()> {
    let idle = std::env::var("GREENCODE_LSP_BROKER_IDLE_MS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .map_or(Duration::from_secs(120), Duration::from_millis);
    serve_broker_with(socket, idle).await
}

#[cfg(not(unix))]
pub async fn serve_broker(_socket: &Path) -> Result<()> {
    bail!("lsp.shared needs Unix sockets")
}

/// `serve_broker` with an explicit idle timeout, for tests.
#[cfg(unix)]
pub async fn serve_broker_with(socket: &Path, idle: Duration) -> Result<()> {
    use tokio::net::UnixListener;
    let _ = std::fs::remove_file(socket);
    if let Some(dir) = socket.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let listener = UnixListener::bind(socket)?;
    let pool: Arc<tokio::sync::Mutex<Vec<Pooled>>> = Arc::default();
    let live = Arc::new(AtomicU64::new(0));
    loop {
        let accepted = tokio::time::timeout(idle, listener.accept()).await;
        match accepted {
            Ok(Ok((stream, _))) => {
                live.fetch_add(1, Ordering::AcqRel);
                let pool = pool.clone();
                let live = live.clone();
                tokio::spawn(async move {
                    let _ = brokered(stream, pool).await;
                    live.fetch_sub(1, Ordering::AcqRel);
                });
            }
            Ok(Err(e)) => return Err(e.into()),
            Err(_) if live.load(Ordering::Acquire) == 0 => {
                pool.lock().await.clear();
                let _ = std::fs::remove_file(socket);
                return Ok(());
            }
            Err(_) => {}
        }
    }
}

/// Serves one client: attach a warm server or spawn one, then copy bytes both ways.
#[cfg(unix)]
async fn brokered(stream: tokio::net::UnixStream, pool: Arc<tokio::sync::Mutex<Vec<Pooled>>>) -> Result<()> {
    use tokio::{io::AsyncReadExt, sync::mpsc};
    let (read, mut write) = stream.into_split();
    let mut reader = BufReader::new(read);
    let hello = read_frame(&mut reader).await?;
    let root = hello["root"].as_str().map(PathBuf::from).unwrap_or_default();
    let command = hello["command"].as_str().unwrap_or("").to_string();
    let args: Vec<String> = hello["args"]
        .as_array()
        .map(|a| a.iter().filter_map(Value::as_str).map(String::from).collect())
        .unwrap_or_default();
    let key = format!("{command} {}", args.join(" "));
    let (mut server, reused) = {
        let mut idle = pool.lock().await;
        match idle.iter().position(|p| p.command == key && !p.exited.load(Ordering::Acquire)) {
            Some(i) => (idle.remove(i), true),
            None => {
                let mut child = Command::new(&command)
                    .args(&args)
                    .current_dir(&root)
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::null())
                    .kill_on_drop(true)
                    .spawn()
                    .with_context(|| format!("starting language server {command}"))?;
                let stdin = child.stdin.take().unwrap();
                let stdout = child.stdout.take().unwrap();
                let sink: Arc<Mutex<Option<mpsc::UnboundedSender<Vec<u8>>>>> = Arc::default();
                let exited = Arc::new(AtomicBool::new(false));
                let (sink_task, exited_task) = (sink.clone(), exited.clone());
                tokio::spawn(async move {
                    let mut stdout = stdout;
                    let mut buf = [0u8; 8192];
                    while let Ok(n) = stdout.read(&mut buf).await {
                        if n == 0 {
                            break;
                        }
                        let target = sink_task.lock().unwrap().clone();
                        if let Some(tx) = target {
                            let _ = tx.send(buf[..n].to_vec());
                        }
                    }
                    exited_task.store(true, Ordering::Release);
                });
                (Pooled { stdin: Some(stdin), sink, exited, command: key.clone(), _child: child }, false)
            }
        }
    };
    write_frame(&mut write, &json!({"reused": reused})).await?;
    let (tx, mut rx) = mpsc::unbounded_channel::<Vec<u8>>();
    *server.sink.lock().unwrap() = Some(tx);
    let mut writer = write;
    let outbound = tokio::spawn(async move {
        while let Some(bytes) = rx.recv().await {
            if writer.write_all(&bytes).await.is_err() {
                break;
            }
        }
    });
    let mut stdin = server.stdin.take().ok_or_else(|| anyhow!("server already attached"))?;
    let mut buf = [0u8; 8192];
    let result = loop {
        match reader.read(&mut buf).await {
            Ok(0) | Err(_) => break Ok(()),
            Ok(n) => {
                if let Err(e) = stdin.write_all(&buf[..n]).await {
                    break Err(e.into());
                }
            }
        }
    };
    outbound.abort();
    *server.sink.lock().unwrap() = None;
    server.stdin = Some(stdin);
    if !server.exited.load(Ordering::Acquire) {
        pool.lock().await.push(server);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    /// A fake language server: answers `initialize` and echoes `ping` as `pong`, and appends its pid to
    /// `$FAKE_LSP_PIDS` on start so the broker's reuse (one process) is visible.
    fn fake_server(dir: &Path) -> PathBuf {
        let script = dir.join("fake_lsp.py");
        std::fs::write(
            &script,
            r#"import json, os, sys
log = os.environ.get("FAKE_LSP_PIDS")
if log:
    open(log, "a").write(str(os.getpid()) + "\n")
def read():
    n = 0
    while True:
        line = sys.stdin.buffer.readline()
        if not line:
            sys.exit(0)
        if line in (b"\r\n", b"\n"):
            break
        k, _, v = line.decode().partition(":")
        if k.lower() == "content-length":
            n = int(v.strip())
    return json.loads(sys.stdin.buffer.read(n))
def write(obj):
    body = json.dumps(obj).encode()
    sys.stdout.buffer.write(b"Content-Length: " + str(len(body)).encode() + b"\r\n\r\n" + body)
    sys.stdout.buffer.flush()
while True:
    msg = read()
    if msg.get("method") == "initialize":
        write({"jsonrpc": "2.0", "id": msg["id"], "result": {"capabilities": {}}})
        write({"jsonrpc": "2.0", "method": "$/progress", "params": {"token": "t", "value": {"kind": "end"}}})
    elif msg.get("method") == "ping":
        write({"jsonrpc": "2.0", "id": msg["id"], "result": {"pong": True}})
    elif "id" in msg:
        write({"jsonrpc": "2.0", "id": msg["id"], "result": None})
"#,
        )
        .unwrap();
        script
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn shared_broker_forwards_frames_and_reuses_the_warm_server() {
        let dir = tempfile::tempdir().unwrap();
        let script = fake_server(dir.path());
        let pids = dir.path().join("pids");
        std::env::set_var("FAKE_LSP_PIDS", &pids);
        let socket = dir.path().join("broker.sock");
        let serve = tokio::spawn({
            let socket = socket.clone();
            async move { serve_broker_with(&socket, Duration::from_millis(300)).await }
        });
        let python = crate::config::executable("python3").unwrap();
        // Two connections: each initializes, pings, and answers from the same warm process.
        for round in 0..2 {
            let server = shared_at(&socket, dir.path(), &python, &[script.to_str().unwrap().into()])
                .await
                .unwrap();
            let pong = server
                .request("ping", json!({}))
                .await
                .expect("forwarded request");
            assert_eq!(pong["pong"], json!(true), "round {round}");
        }
        // The server process outlived the first connection: exactly one pid was ever started.
        tokio::time::sleep(Duration::from_millis(200)).await;
        let started = std::fs::read_to_string(&pids).unwrap();
        assert_eq!(started.lines().count(), 1, "{started}");
        // Idle with no clients: the broker exits and takes its server with it.
        tokio::time::timeout(Duration::from_secs(3), serve)
            .await
            .expect("broker exits when idle")
            .unwrap()
            .unwrap();
        assert!(!socket.exists());
    }

    #[tokio::test]
    async fn framing_round_trip() {
        let (mut writer, reader) = tokio::io::duplex(1024);
        let value = json!({"message":"hello 世界"});
        write_frame(&mut writer, &value).await.unwrap();
        assert_eq!(
            read_frame(&mut BufReader::new(reader)).await.unwrap(),
            value
        );
    }
    #[test]
    fn diagnostics_distinguish_clean_from_unknown() {
        assert_eq!(
            diagnostics_text(Some(&json!([]))).unwrap(),
            "no diagnostics"
        );
        let text = diagnostics_text(Some(&json!([{"severity":1,"range":{"start":{"line":2,"character":4}},"message":"mismatched types"}]))).unwrap();
        assert_eq!(text, "error 3:5 mismatched types");
        // A silent or exited server is not a clean file.
        let error = diagnostics_text(None).unwrap_err().to_string();
        assert!(error.contains("state is unknown"), "{error}");
    }

    #[test]
    fn positions_are_one_based() {
        assert_eq!(
            position(&json!({"line":2,"column":3})).unwrap(),
            json!({"line":1,"character":2})
        );
        assert!(position(&json!({"line":0,"column":1})).is_err());
    }

    #[test]
    fn formatting_edits_apply_back_to_front() {
        let text = "abc\ndef\n";
        let edits = json!([
            {"range":{"start":{"line":0,"character":1},"end":{"line":0,"character":1}},"newText":"X"},
            {"range":{"start":{"line":1,"character":1},"end":{"line":1,"character":1}},"newText":"Y"},
        ]);
        assert_eq!(apply_edits(text, &edits).unwrap(), "aXbc\ndYef\n");
        assert!(apply_edits(text, &json!([])).is_none());
        assert!(apply_edits("abc\n", &json!([{"range":{"start":{"line":9,"character":0},"end":{"line":9,"character":0}},"newText":"x"}])).is_none());
    }

    #[test]
    fn overlapping_edits_are_refused_and_same_spot_inserts_keep_order() {
        let edit = |s: u64, e: u64, t: &str| json!({"range":{"start":{"line":0,"character":s},"end":{"line":0,"character":e}},"newText":t});
        assert!(apply_edits("abcdef", &json!([edit(0, 3, "X"), edit(2, 4, "Y")])).is_none());
        assert_eq!(apply_edits("abcdef", &json!([edit(1, 1, "A"), edit(1, 1, "B")])).unwrap(), "aABbcdef");
        assert_eq!(apply_edits("abcdef", &json!([edit(0, 2, "X"), edit(2, 4, "Y")])).unwrap(), "XYef");
    }

    #[test]
    fn rename_is_judged_as_an_edit_and_servers_come_from_config() {
        let input = json!({"action":"rename","path":"src/lib.rs","query":"x"});
        assert_eq!(crate::tools::ext::effective("lsp", &input), ("edit".to_string(), json!({"path":"src/lib.rs"})));
        assert!(!crate::permission::read_only("lsp", &input));
        assert!(crate::permission::read_only("lsp", &json!({"action":"hover"})));
        let mut cfg = Config::default();
        cfg.integration_opts.lsp.servers.insert("zls".into(), LspServerCfg { args: vec!["--stdio".into()], extensions: vec![".zigfake".into()] });
        configure(&cfg);
        assert_eq!(server_spec("zigfake"), Some(("zls".into(), vec!["--stdio".into()])));
        assert_eq!(server_spec("go").unwrap().0, "gopls");
        configure(&Config::default());
    }

    #[test]
    fn repeated_diagnostics_can_be_deduplicated() {
        let path = Path::new("/tmp/greencode-dedup-lsp.rs");
        let errors = json!([{"severity":1,"range":{"start":{"line":0,"character":0}},"message":"boom"}]);
        assert!(diagnostics_note(path, &errors, true).is_some());
        assert!(diagnostics_note(path, &errors, true).is_none());
        // A changed file shows only the diagnostics it has not shown yet.
        let mut both = errors.as_array().unwrap().clone();
        both.push(json!({"severity":1,"range":{"start":{"line":1,"character":0}},"message":"new one"}));
        both.push(json!({"severity":2,"range":{"start":{"line":2,"character":0}},"message":"just a warning"}));
        let note = diagnostics_note(path, &json!(both), true).unwrap();
        assert!(note.contains("new one") && !note.contains("boom") && !note.contains("warning"), "{note}");
        assert!(diagnostics_note(path, &errors, false).is_some());
        // A publication without errors forgets the file, so the same errors after a fix are reported again.
        assert!(diagnostics_note(path, &json!([{"severity":2,"range":{"start":{"line":0,"character":0}},"message":"w"}]), true).is_none());
        assert!(diagnostics_note(path, &errors, true).is_some());
    }
}
