use super::{def as tool_def, str_arg, OUTPUT_CHARS};
use crate::{permission::resolve, ToolDef};
use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};
use std::{
    collections::HashMap,
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
    process::{Child, ChildStdin, Command},
    sync::{oneshot, Notify},
};

pub fn def() -> ToolDef {
    tool_def("lsp", "Language server query. Path selects a file; line and column are 1-based. Symbols uses query.", json!({"action":{"enum":["diagnostics","definition","references","hover","symbols"]},"path":{"type":"string"},"line":{"type":"integer"},"column":{"type":"integer"},"query":{"type":"string"}}), &["action"])
}

fn server_for(path: &Path) -> Option<(&'static str, Vec<&'static str>)> {
    let (name, args) = match path.extension()?.to_str()? {
        "rs" => ("rust-analyzer", vec![]),
        "go" => ("gopls", vec!["serve"]),
        "ts" | "tsx" | "js" | "jsx" => ("typescript-language-server", vec!["--stdio"]),
        "py" => ("pyright-langserver", vec!["--stdio"]),
        _ => return None,
    };
    crate::config::executable(name).map(|_| (name, args))
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
    stdin: tokio::sync::Mutex<ChildStdin>,
    pending: Mutex<Pending>,
    diagnostics: Mutex<HashMap<String, Value>>,
    changed: Notify,
    documents: tokio::sync::Mutex<HashMap<String, (String, u64)>>,
    next: AtomicU64,
    exited: AtomicBool,
    /// Set when the server reports the end of its work-done progress (project loaded).
    ready: AtomicBool,
    _child: Child,
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
        let result = tokio::time::timeout(Duration::from_secs(20), async {
            self.send(json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}))
                .await?;
            rx.await.map_err(|_| anyhow!("language server exited"))?
        })
        .await;
        self.pending.lock().unwrap().remove(&id);
        match result {
            Err(_) => bail!("language server request {method} timed out after 20 seconds"),
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
    let server = Arc::new(Server {
        stdin: tokio::sync::Mutex::new(child.stdin.take().unwrap()),
        pending: Mutex::new(HashMap::new()),
        diagnostics: Mutex::new(HashMap::new()),
        changed: Notify::new(),
        documents: tokio::sync::Mutex::new(HashMap::new()),
        next: AtomicU64::new(1),
        exited: AtomicBool::new(false),
        ready: AtomicBool::new(false),
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
            if path.is_file()
                && matches!(
                    path.extension().and_then(|e| e.to_str()),
                    Some("rs" | "go" | "ts" | "tsx" | "js" | "jsx" | "py")
                )
            {
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

pub async fn run(cwd: &Path, input: &Value) -> Result<String> {
    let action = str_arg(input, "action")?;
    if ![
        "diagnostics",
        "definition",
        "references",
        "hover",
        "symbols",
    ]
    .contains(&action)
    {
        bail!("unknown lsp action `{action}`");
    }
    if action == "symbols" {
        str_arg(input, "query")?;
    }
    let pos = if ["definition", "references", "hover"].contains(&action) {
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
    let mut all = SERVERS.lock().await;
    let server = {
        let path = &selection;
        let ext = path.extension().and_then(|s| s.to_str()).unwrap_or("");
        let (name, args) = server_for(path).ok_or_else(|| {
            let name = match ext {
                "rs" => "rust-analyzer",
                "go" => "gopls",
                "ts" | "tsx" | "js" | "jsx" => "typescript-language-server",
                "py" => "pyright-langserver",
                _ => "a language server",
            };
            anyhow!("no language server for {ext}; install {name}")
        })?;
        let key = (root.clone(), name.to_string());
        if all
            .get(&key)
            .is_some_and(|s| s.exited.load(Ordering::Acquire))
        {
            all.remove(&key);
        }
        if let Some(s) = all.get(&key) {
            s.clone()
        } else {
            let s = start(&root, name, &args).await?;
            all.insert(key, s.clone());
            s
        }
    };
    drop(all);
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
            let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
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
                if (error.is_none() && !empty(&value))
                    || server.exited.load(Ordering::Acquire)
                    || waited >= Duration::from_secs(15)
                    || (error.is_none() && waited >= Duration::from_secs(5))
                {
                    break (value, error);
                }
                tokio::time::sleep(Duration::from_millis(500)).await;
            };
            if empty(&result) {
                if let Some(e) = error {
                    return Err(e);
                }
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

#[cfg(test)]
mod tests {
    use super::*;
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
}
