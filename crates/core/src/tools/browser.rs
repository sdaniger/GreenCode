//! Native `browser` tool: Chromium over CDP (launched or attached), the browser relay, CDP discovery
//! endpoints, Tern picture-in-picture blocks, the ABKit WebKit host, and cmux surfaces.
use crate::{
    config::{browser::BrowserCfg, Config},
    ToolDef,
};


use anyhow::{anyhow, bail, Context, Result};
use base64::Engine;
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    process::Stdio,
    sync::{Arc, LazyLock, Mutex as StdMutex},
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader},
    net::UnixStream,
    process::{Child, Command},
    sync::Mutex,
};
use tokio_tungstenite::{
    tungstenite::{client::IntoClientRequest, Message},
    MaybeTlsStream, WebSocketStream,
};

pub fn def() -> ToolDef {
    let (s, n) = (json!({"type": "string"}), json!({"type": "integer"}));
    ToolDef {
        name: "browser".into(),
        description: "Drive a web browser. `open` starts the tab (Chromium, or the relay/CDP endpoint, Tern, WebKit or cmux \
            when configured) and navigates to `url`; later calls reuse tab `name` (default `main`). `app` overrides the \
            backend for one call: `relay`, `tern`, `webkit`, `cmux`, `chromium`, `own`, an HTTP CDP endpoint or a browser \
            executable. `eval` runs JavaScript in the page and returns its JSON value; `screenshot` saves a PNG and \
            returns its path; `snapshot` returns the URL, title and visible text."
            .into(),
        schema: json!({"type": "object", "required": ["action"], "properties": {
            "action": {"enum": ["open", "goto", "eval", "click", "type", "press", "screenshot", "snapshot", "wait", "tabs", "close"]},
            "name": s, "url": s, "expression": s, "selector": s, "text": s, "key": s,
            "app": s, "persist": {"type": "boolean"}, "full_page": {"type": "boolean"},
            "all": {"type": "boolean"}, "timeout_ms": n,
        }}),
    }
}

// ─── Tab registry ──────────────────────────────────────────────────────────────

struct Tab {
    backend: Backend,
    url: String,
    /// The browser process is ours: only such a tab is frozen or idle-closed.
    owned: bool,
    /// `persist: true` on open opts the tab out of turn-end freezing.
    persist: bool,
    frozen: bool,
    last: Instant,
    /// The CDP target, Tern block or cmux surface this tab drives.
    target: String,
    kind: &'static str,
}

impl Tab {
    /// Unfreeze before a use, so nothing runs against a frozen page.
    async fn resume(&mut self) -> Result<()> {
        self.last = Instant::now();
        if self.frozen {
            self.backend.freeze(false).await?;
            self.frozen = false;
        }
        Ok(())
    }

    async fn close(&mut self) -> Result<()> {
        let target = self.target.clone();
        self.backend.close(&target).await
    }
}

static TABS: LazyLock<StdMutex<BTreeMap<(PathBuf, String), Arc<Mutex<Tab>>>>> =
    LazyLock::new(|| StdMutex::new(BTreeMap::new()));

fn tabs_in(cwd: &Path) -> Vec<((PathBuf, String), Arc<Mutex<Tab>>)> {
    TABS.lock()
        .unwrap()
        .iter()
        .filter(|((dir, _), _)| dir == cwd)
        .map(|(key, tab)| (key.clone(), tab.clone()))
        .collect()
}

fn register(cwd: &Path, name: &str, tab: Arc<Mutex<Tab>>) {
    TABS.lock().unwrap().insert((cwd.to_path_buf(), name.to_string()), tab);
}

fn forget(cwd: &Path, name: &str) {
    TABS.lock().unwrap().remove(&(cwd.to_path_buf(), name.to_string()));
}

/// Closes every tab of `cwd` that sat idle longer than `browser.idle_close_sec`; never touches tabs in use.
pub async fn reap_idle(cwd: &Path, cfg: &Config) {
    let secs = cfg.integration_opts.browser.idle_close_sec;
    if secs == 0 {
        return;
    }
    for (key, tab) in tabs_in(cwd) {
        let Ok(mut tab) = tab.try_lock() else { continue };
        if tab.last.elapsed() < Duration::from_secs(secs) {
            continue;
        }
        let _ = tab.close().await;
        drop(tab);
        forget(&key.0, &key.1);
    }
}

/// Freezes (and reaps) this project's tabs once a turn settles; the agent holds one for its turn.
pub struct Settle(PathBuf);

impl Settle {
    pub fn new(cwd: PathBuf) -> Self {
        Self(cwd)
    }
}

impl Drop for Settle {
    fn drop(&mut self) {
        let cwd = self.0.clone();
        tokio::spawn(async move { settle(&cwd, &super::config()).await });
    }
}

/// A turn end: our own Chromium tabs freeze so animated pages stop drawing, then idle tabs close.
async fn settle(cwd: &Path, cfg: &Config) {
    for (_, tab) in tabs_in(cwd) {
        let Ok(mut tab) = tab.try_lock() else { continue };
        if cfg.integration_opts.browser.freeze_on_turn_end && tab.kind == "chromium" && !tab.persist && !tab.frozen {
            if tab.backend.freeze(true).await.is_ok() {
                tab.frozen = true;
            }
        }
    }
    reap_idle(cwd, cfg).await;
}

// ─── Backend selection ─────────────────────────────────────────────────────────

#[derive(Clone, Debug, PartialEq)]
enum Kind {
    Chromium(Option<String>),
    Relay(String),
    Cdp(String),
    Tern((String, i64)),
    Webkit(PathBuf),
    Cmux(String),
}

/// Environment overrides, as a map so tests never touch the process environment.
type Env = BTreeMap<String, String>;

fn env_map() -> Env {
    std::env::vars().collect()
}

/// `0/false/off/no` and `1/true/on/yes`, anything else keeping the setting.
fn flag(env: &Env, name: &str, setting: bool) -> Option<bool> {
    let raw = env.get(name)?.trim().to_ascii_lowercase();
    match raw.as_str() {
        "" => Some(setting),
        "0" | "false" | "off" | "no" => Some(false),
        "1" | "true" | "on" | "yes" => Some(true),
        _ => Some(setting),
    }
}

/// Tabs go beside the terminal that owns this process: a shell started from another session's pane must
/// not take over its picture-in-picture. Unclaimed terminals (no owner var) are ours.
fn owns_terminal(env: &Env) -> bool {
    match env.get("GREENCODE_TERMINAL_OWNER_PID") {
        Some(owner) => owner.trim() == std::process::id().to_string(),
        None => true,
    }
}

fn tern_pane(env: &Env) -> Option<(String, i64)> {
    let socket = env.get("TERN_PANE_SOCKET")?.trim();
    let pane = env.get("TERN_PANE")?.trim().parse().ok()?;
    (!socket.is_empty()).then(|| (socket.to_string(), pane))
}

fn installed_webkit(cfg: &BrowserCfg, env: &Env) -> Option<PathBuf> {
    let configured = env
        .get("GREENCODE_BROWSER_WEBKIT_PATH")
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .or_else(|| cfg.webkit_path.clone())
        .filter(|s| !s.trim().is_empty());
    let location = match configured {
        Some(path) => BrowserCfg::expand(&path),
        None => crate::home().join("lib/abkit"),
    };
    // A configured location that holds no host does not fall through to another.
    webkit_host(&location)
}

/// The ABKit host executable at `location`: a file is the host itself, a directory holds it as `ABHost`.
fn webkit_host(location: &Path) -> Option<PathBuf> {
    let name = if cfg!(windows) { "ABHost.exe" } else { "ABHost" };
    let candidate = if location.is_dir() { location.join(name) } else { location.to_path_buf() };
    let ok = candidate.is_file()
        && (cfg!(windows)
            || std::os::unix::fs::PermissionsExt::mode(&candidate.metadata().ok()?.permissions()) & 0o111 != 0);
    ok.then_some(candidate)
}

/// The browser one call drives, by OMP's precedence: an explicit `app`, the relay, the configured CDP
/// endpoint, Tern (only inside a pane this process owns), cmux, then the native browser (WebKit when
/// installed, else Chromium).
fn resolve(cfg: &BrowserCfg, app: Option<&str>, env: &Env) -> Kind {
    let keywords = ["relay", "tern", "webkit", "chromium", "own", "cmux", "auto"];
    if let Some(app) = app.map(str::trim).filter(|a| !a.is_empty()) {
        if app.starts_with("http://") || app.starts_with("https://") || app.starts_with("ws://") || app.starts_with("wss://") {
            return Kind::Cdp(discovery_base(app));
        }
        if !keywords.contains(&app) {
            // Any other value is a browser executable to launch.
            return Kind::Chromium(Some(app.to_string()));
        }
    }
    let relay = flag(env, "GREENCODE_BROWSER_RELAY", cfg.relay);
    let relay_url = env
        .get("GREENCODE_BROWSER_RELAY_URL")
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .or_else(|| cfg.relay_url.clone())
        .unwrap_or_else(|| "http://127.0.0.1:9224".into());
    let relay_only = env.get("GREENCODE_BROWSER_RELAY").is_some_and(|v| v.trim().eq_ignore_ascii_case("only"));
    match app {
        Some("relay") => return Kind::Relay(relay_url),
        Some("tern") => return Kind::Tern(tern_pane(env).unwrap_or_default()),
        Some("webkit") => {
            return installed_webkit(cfg, env).map(Kind::Webkit).unwrap_or(Kind::Chromium(None));
        }
        Some("chromium") => return Kind::Chromium(None),
        Some("cmux") => return Kind::Cmux(env.get("CMUX_SOCKET_PATH").cloned().unwrap_or_default()),
        _ => {}
    }
    if app != Some("auto") && (relay_only || relay == Some(true)) {
        return Kind::Relay(relay_url);
    }
    if let Some(url) = cfg.cdp_url.as_deref().map(str::trim).filter(|u| !u.is_empty()) {
        return Kind::Cdp(discovery_base(url));
    }
    if let Some((socket, pane)) = tern_pane(env) {
        if app == Some("tern") || (owns_terminal(env) && flag(env, "GREENCODE_BROWSER_TERN", cfg.tern) != Some(false)) {
            return Kind::Tern((socket, pane));
        }
    }
    if let Some(socket) = env.get("CMUX_SOCKET_PATH").map(|s| s.trim()).filter(|s| !s.is_empty()) {
        if flag(env, "GREENCODE_BROWSER_CMUX", cfg.cmux) != Some(false) {
            return Kind::Cmux(socket.to_string());
        }
    }
    if flag(env, "GREENCODE_BROWSER_WEBKIT", cfg.webkit) != Some(false)
        && app != Some("chromium")
        && flag(env, "GREENCODE_BROWSER_HEADLESS", cfg.headless) != Some(false)
    {
        if let Some(host) = installed_webkit(cfg, env) {
            return Kind::Webkit(host);
        }
    }
    Kind::Chromium(None)
}

/// A `ws://` browser endpoint points at a browser already; an HTTP one is a discovery endpoint.
fn discovery_base(url: &str) -> String {
    url.trim_end_matches('/').to_string()
}

// ─── CDP over WebSocket ────────────────────────────────────────────────────────

type Ws = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

struct Cdp {
    ws: Ws,
    session: Option<String>,
    target: String,
    next: u64,
    /// The Chromium this tab launched, killed with the tab.
    child: Option<Child>,
    profile: Option<PathBuf>,
}

impl Cdp {
    /// Attaches to the browser named by `url` (its browser WebSocket endpoint) and opens a page target.
    async fn attach(url: &str, profile: Option<PathBuf>, child: Option<Child>) -> Result<Cdp> {
        let mut request = url.into_client_request().with_context(|| format!("invalid CDP URL {url}"))?;
        request.headers_mut().insert("Host", "localhost".parse().unwrap());
        let (ws, _) = tokio_tungstenite::connect_async(request)
            .await
            .with_context(|| format!("cannot connect to the CDP endpoint at {url}"))?;
        let mut cdp = Cdp { ws, session: None, target: String::new(), next: 1, child, profile };
        let created = cdp.call("Target.createTarget", json!({"url": "about:blank"})).await?;
        let target = created["targetId"].as_str().context("CDP createTarget returned no targetId")?.to_string();
        let attached = cdp.call("Target.attachToTarget", json!({"targetId": target, "flatten": true})).await?;
        let session = attached["sessionId"].as_str().context("CDP attachToTarget returned no sessionId")?.to_string();
        cdp.session = Some(session);
        cdp.target = target;
        Ok(cdp)
    }

    async fn call(&mut self, method: &str, params: Value) -> Result<Value> {
        let id = self.next;
        self.next += 1;
        let mut message = json!({"id": id, "method": method, "params": params});
        if let Some(session) = &self.session {
            message["sessionId"] = json!(session);
        }
        self.ws
            .send(Message::Text(message.to_string().into()))
            .await
            .with_context(|| format!("CDP {method} could not be sent"))?;
        loop {
            let frame = match self.ws.next().await {
                Some(Ok(frame)) => frame,
                Some(Err(error)) => bail!("CDP {method} failed: {error}"),
                None => bail!("the browser closed the CDP connection during {method}"),
            };
            match frame {
                Message::Text(text) => {
                    let reply: Value = serde_json::from_str(&text).with_context(|| format!("CDP {method} sent invalid JSON"))?;
                    if reply["id"].as_u64() != Some(id) {
                        continue; // An event, or the answer to an earlier call.
                    }
                    if let Some(error) = reply.get("error") {
                        bail!("CDP {method}: {}", error["message"].as_str().unwrap_or(&error.to_string()));
                    }
                    return Ok(reply["result"].clone());
                }
                Message::Ping(payload) => {
                    self.ws.send(Message::Pong(payload)).await?;
                }
                Message::Close(_) => bail!("the browser closed the CDP connection during {method}"),
                _ => {}
            }
        }
    }

    /// Evaluates `expression` in the page and returns its JSON value, reading a thrown error as one.
    async fn eval(&mut self, expression: &str) -> Result<Value> {
        let result = self
            .call(
                "Runtime.evaluate",
                json!({"expression": expression, "awaitPromise": true, "returnByValue": true}),
            )
            .await?;
        if let Some(exception) = result.get("exceptionDetails") {
            let text = exception["exception"]["description"]
                .as_str()
                .or_else(|| exception["text"].as_str())
                .unwrap_or("JavaScript exception");
            bail!("{}", text);
        }
        Ok(result.get("result").and_then(|r| r.get("value")).cloned().unwrap_or(Value::Null))
    }

    async fn navigate(&mut self, url: &str) -> Result<()> {
        let reply = self.call("Page.navigate", json!({"url": url})).await?;
        if let Some(error) = reply["errorText"].as_str() {
            bail!("cannot navigate to {url}: {error}");
        }
        // Waiting for the document beats depending on event plumbing: poll `readyState`.
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let state = self.eval("document.readyState").await.unwrap_or(Value::Null);
            if state.as_str() == Some("complete") {
                return Ok(());
            }
            if Instant::now() >= deadline {
                bail!("navigation to {url} did not finish within 30s");
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    async fn capture(&mut self, full_page: bool) -> Result<Vec<u8>> {
        let result = self
            .call("Page.captureScreenshot", json!({"format": "png", "captureBeyondViewport": full_page}))
            .await?;
        let data = result["data"].as_str().context("CDP screenshot returned no data")?;
        Ok(base64::engine::general_purpose::STANDARD.decode(data)?)
    }

    async fn close(&mut self) -> Result<()> {
        let _ = self.call("Target.closeTarget", json!({"targetId": self.target})).await;
        if let Some(child) = &mut self.child {
            let _ = child.start_kill();
        }
        if let Some(profile) = &self.profile {
            let _ = std::fs::remove_dir_all(profile);
        }
        Ok(())
    }
}

/// Where an HTTP CDP endpoint advertises its browser WebSocket; a relay answers 503 until its extension dials in.
async fn discover(base: &str) -> Result<String> {
    if base.starts_with("ws://") || base.starts_with("wss://") {
        return Ok(base.to_string());
    }
    let response = reqwest::Client::new()
        .get(format!("{base}/json/version"))
        .timeout(Duration::from_secs(5))
        .send()
        .await
        .with_context(|| {
            format!("no CDP endpoint answered at {base}; start the browser with --remote-debugging-port or fix cdp_url")
        })?;
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    if !status.is_success() {
        bail!(
            "the CDP endpoint at {base} answered {status}: {} — for a relay this means its extension is not connected",
            body.trim()
        );
    }
    let version: Value = serde_json::from_str(&body).with_context(|| format!("{base}/json/version is not JSON"))?;
    version["webSocketDebuggerUrl"]
        .as_str()
        .map(String::from)
        .with_context(|| format!("{base}/json/version lists no webSocketDebuggerUrl"))
}

/// The Chromium executables an open tries, in order.
fn chromium_candidates() -> Vec<PathBuf> {
    let mut out = vec![];
    if cfg!(target_os = "macos") {
        for app in [
            "Google Chrome.app/Contents/MacOS/Google Chrome",
            "Chromium.app/Contents/MacOS/Chromium",
            "Microsoft Edge.app/Contents/MacOS/Microsoft Edge",
            "Brave Browser.app/Contents/MacOS/Brave Browser",
        ] {
            for root in ["/Applications", "/Applications/Utilities"] {
                out.push(Path::new(root).join(app));
            }
            out.push(crate::user_home().join("Applications").join(app));
        }
    }
    for name in ["chromium", "chromium-browser", "google-chrome", "google-chrome-stable", "chrome", "msedge"] {
        if let Some(path) = crate::config::executable(name) {
            out.push(path);
        }
    }
    out.retain(|path| path.is_file());
    out
}

/// Launches Chromium with a private profile and returns its browser WebSocket endpoint.
///
/// Shortcut: the profile is one per native home, not one per session, so logins survive between
/// sessions; a leftover browser from a crashed run is killed at the next launch by its recorded pid.
async fn launch_chromium(cfg: &BrowserCfg, executable: Option<&str>) -> Result<(String, Child, PathBuf)> {
    let exe = match executable.filter(|e| !e.trim().is_empty()) {
        Some(path) => BrowserCfg::expand(path),
        None => chromium_candidates().into_iter().next().with_context(|| {
            "no Chromium-based browser found: install Chrome/Chromium/Edge/Brave, set browser.cdp_url to a running \
             browser, or name the executable with the `app` argument"
        })?,
    };
    let dir = crate::home().join("browser");
    std::fs::create_dir_all(&dir)?;
    let profile = dir.join("profile");
    reap_previous(&dir, &profile);
    let headless = flag(&env_map(), "GREENCODE_BROWSER_HEADLESS", cfg.headless) != Some(false);
    let mut command = Command::new(&exe);
    command
        .args(["--remote-debugging-port=0", "--no-first-run", "--no-default-browser-check", "--disable-features=Translate"])
        .arg(format!("--user-data-dir={}", profile.display()))
        .arg("about:blank")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    if headless {
        command.arg("--headless=new");
    }
    let child = command.spawn().with_context(|| format!("cannot start {}", exe.display()))?;
    let _ = std::fs::write(dir.join("pid"), child.id().unwrap_or(0).to_string());
    let port_file = profile.join("DevToolsActivePort");
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if let Ok(text) = std::fs::read_to_string(&port_file) {
            let mut lines = text.lines();
            if let (Some(port), Some(path)) = (lines.next(), lines.next()) {
                if let Ok(port) = port.trim().parse::<u16>() {
                    return Ok((format!("ws://127.0.0.1:{port}{}", path.trim()), child, profile));
                }
            }
        }
        if Instant::now() >= deadline {
            bail!("{} started but never opened a CDP port (looked for {})", exe.display(), port_file.display());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Kills the Chromium a crashed previous run left behind, identified by our own profile on its command line.
fn reap_previous(dir: &Path, profile: &Path) {
    let pid: u32 = std::fs::read_to_string(dir.join("pid")).ok().and_then(|t| t.trim().parse().ok()).unwrap_or(0);
    let _ = std::fs::remove_file(dir.join("pid"));
    if pid == 0 || pid == std::process::id() {
        return;
    }
    let output = std::process::Command::new("ps").args(["-o", "command=", "-p", &pid.to_string()]).output();
    let command = output.ok().map(|o| String::from_utf8_lossy(&o.stdout).to_string()).unwrap_or_default();
    if command.contains(&profile.display().to_string()) {
        let _ = std::process::Command::new("kill").args(["-9", &pid.to_string()]).status();
    }
}

// ─── Tern protocol (the daemon's socket, or the WebKit host's standard streams) ─

const MAX_FRAME: u32 = 256 << 20;

struct Tern {
    write: Box<dyn AsyncWrite + Send + Unpin>,
    read: Box<dyn AsyncRead + Send + Unpin>,
    next: u64,
    product: &'static str,
    child: Option<Child>,
}

impl Tern {
    async fn over_socket(path: &str, product: &'static str) -> Result<Tern> {
        let stream = UnixStream::connect(path)
            .await
            .with_context(|| format!("cannot connect to the {product} socket at {path}"))?;
        let (read, write) = stream.into_split();
        Tern::greet(Box::new(write), Box::new(read), product, None).await
    }

    async fn over_host(executable: &Path) -> Result<Tern> {
        let mut child = Command::new(executable)
            .current_dir(executable.parent().unwrap_or(Path::new(".")))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .with_context(|| format!("cannot start the WebKit host {}", executable.display()))?;
        let write = child.stdin.take().context("the WebKit host has no input")?;
        let read = child.stdout.take().context("the WebKit host has no output")?;
        Tern::greet(Box::new(write), Box::new(read), "WebKit", Some(child)).await
    }

    async fn greet(
        write: Box<dyn AsyncWrite + Send + Unpin>,
        read: Box<dyn AsyncRead + Send + Unpin>,
        product: &'static str,
        child: Option<Child>,
    ) -> Result<Tern> {
        let mut tern = Tern { write, read, next: 1, product, child };
        tern.write_frame(&json!({"hello": {}})).await?;
        let welcome = tokio::time::timeout(Duration::from_secs(10), tern.read_frame())
            .await
            .map_err(|_| anyhow!("the {product} browser did not answer the greeting in time"))??;
        if welcome.get("welcome").is_none() {
            bail!("the {product} browser answered the greeting without a welcome");
        }
        Ok(tern)
    }

    async fn write_frame(&mut self, message: &Value) -> Result<()> {
        let body = serde_json::to_vec(message)?;
        self.write.write_all(&(body.len() as u32).to_le_bytes()).await?;
        self.write.write_all(&body).await?;
        self.write.flush().await?;
        Ok(())
    }

    async fn read_frame(&mut self) -> Result<Value> {
        let mut header = [0u8; 4];
        self.read.read_exact(&mut header).await?;
        let length = u32::from_le_bytes(header);
        if length > MAX_FRAME {
            bail!("the {} browser sent a {length}-byte frame (limit {MAX_FRAME})", self.product);
        }
        let mut body = vec![0u8; length as usize];
        self.read.read_exact(&mut body).await?;
        serde_json::from_slice(&body).with_context(|| format!("the {} browser sent invalid JSON", self.product))
    }

    /// Sends one browser op and returns its `ok` result.
    async fn request(&mut self, op: Value) -> Result<Value> {
        let id = self.next;
        self.next += 1;
        self.write_frame(&json!({"id": id, "browser": op})).await?;
        let label = op["op"].as_str().unwrap_or("op").to_string();
        loop {
            let reply = self.read_frame().await?;
            if reply["id"].as_u64() != Some(id) {
                continue;
            }
            let answer = &reply["browser"];
            if let Some(error) = answer.get("error") {
                bail!(
                    "the {} browser {label} failed ({}): {}",
                    self.product,
                    error["kind"].as_str().unwrap_or("failed"),
                    error["message"].as_str().unwrap_or("failed")
                );
            }
            return answer.get("ok").cloned().ok_or_else(|| anyhow!("the {} browser answered {label} without ok", self.product));
        }
    }

    async fn open(&mut self, pane: i64) -> Result<i64> {
        let reply = self
            .request(json!({"op": "open", "owner": pane, "url": "about:blank", "width": 1280, "height": 800}))
            .await?;
        reply["block"].as_i64().context("the browser answered open without a block id")
    }

    async fn navigate(&mut self, block: i64, url: &str) -> Result<()> {
        self.request(json!({"op": "goto", "block": block, "url": url})).await?;
        Ok(())
    }

    async fn eval(&mut self, block: i64, expression: &str) -> Result<Value> {
        // A page-side try/catch names the thrown error instead of failing as an opaque engine error.
        let wrapped = format!(
            "(function(){{ try {{ return ({expression}); }} catch (error) {{ return {{ __error: String(error && error.stack || error) }}; }} }})()"
        );
        let reply = self.request(json!({"op": "eval", "block": block, "function": wrapped, "args": []})).await?;
        let value = reply.get("value").cloned().unwrap_or(Value::Null);
        if let Some(error) = value.get("__error").and_then(Value::as_str) {
            bail!("{error}");
        }
        Ok(value)
    }

    async fn capture(&mut self, block: i64) -> Result<Vec<u8>> {
        let reply = self.request(json!({"op": "capture", "block": block, "format": "png", "scale": 1})).await?;
        let data = reply["data"].as_str().context("the browser capture returned no image data")?;
        Ok(base64::engine::general_purpose::STANDARD.decode(data)?)
    }

    async fn close(&mut self, block: i64) -> Result<()> {
        self.request(json!({"op": "close", "block": block})).await?;
        if let Some(child) = &mut self.child {
            let _ = child.start_kill();
        }
        Ok(())
    }
}

// ─── cmux socket ───────────────────────────────────────────────────────────────

struct Cmux {
    read: BufReader<tokio::net::unix::OwnedReadHalf>,
    write: tokio::net::unix::OwnedWriteHalf,
    next: u64,
}

impl Cmux {
    async fn connect(path: &str, password: Option<&str>) -> Result<Cmux> {
        if !path.starts_with('/') {
            bail!("cmux browser needs a socket path (CMUX_SOCKET_PATH), not the relay endpoint {path}");
        }
        let stream = UnixStream::connect(path)
            .await
            .with_context(|| format!("cannot connect to the cmux socket at {path}"))?;
        let (read, write) = stream.into_split();
        let mut cmux = Cmux { read: BufReader::new(read), write, next: 1 };
        if let Some(password) = password.filter(|p| !p.is_empty()) {
            cmux.write.write_all(format!("auth {password}\n").as_bytes()).await?;
            let line = cmux.read_line().await?;
            if let Some(error) = line.strip_prefix("ERROR:") {
                bail!("cmux refused the password: {error}");
            }
        }
        Ok(cmux)
    }

    async fn read_line(&mut self) -> Result<String> {
        let mut line = String::new();
        self.read.read_line(&mut line).await?;
        Ok(line.trim_end_matches(['\r', '\n']).to_string())
    }

    async fn request(&mut self, method: &str, params: Value) -> Result<Value> {
        let id = self.next;
        self.next += 1;
        let message = json!({"id": id.to_string(), "method": method, "params": params});
        self.write.write_all(format!("{message}\n").as_bytes()).await?;
        let line = self.read_line().await?;
        if let Some(error) = line.strip_prefix("ERROR:") {
            bail!("cmux {method}: {error}");
        }
        let reply: Value = serde_json::from_str(&line).with_context(|| format!("cmux sent invalid JSON for {method}: {line}"))?;
        if reply["ok"] == Value::Bool(true) {
            return Ok(reply.get("result").cloned().unwrap_or_else(|| json!({})));
        }
        let error = &reply["error"];
        bail!(
            "cmux {method}: {}: {}",
            error["code"].as_str().unwrap_or("error"),
            error["message"].as_str().unwrap_or("failed")
        )
    }

    async fn open(&mut self, url: &str, env: &Env) -> Result<String> {
        let mut params = json!({"url": url, "focus": false});
        for name in ["CMUX_WORKSPACE_ID", "CMUX_SURFACE_ID"] {
            if let Some(value) = env.get(name).filter(|v| !v.is_empty()) {
                params[if name.ends_with("WORKSPACE_ID") { "workspace_id" } else { "surface_id" }] = json!(value);
            }
        }
        let reply = self.request("browser.open_split", params).await?;
        reply["surface_id"]
            .as_str()
            .map(String::from)
            .context("cmux browser.open_split returned no surface_id")
    }

    async fn navigate(&mut self, surface: &str, url: &str) -> Result<()> {
        self.request("browser.navigate", json!({"surface_id": surface, "url": url})).await?;
        self.request("browser.wait", json!({"surface_id": surface, "load_state": "complete", "timeout_ms": 30_000}))
            .await?;
        Ok(())
    }

    async fn eval(&mut self, surface: &str, script: &str) -> Result<Value> {
        let reply = self.request("browser.eval", json!({"surface_id": surface, "script": script})).await?;
        Ok(reply.get("value").cloned().unwrap_or(Value::Null))
    }

    async fn capture(&mut self, surface: &str) -> Result<Vec<u8>> {
        let reply = self.request("browser.screenshot", json!({"surface_id": surface})).await?;
        let data = reply["png_base64"].as_str().context("cmux browser.screenshot returned no png_base64")?;
        Ok(base64::engine::general_purpose::STANDARD.decode(data)?)
    }

    async fn close(&mut self, surface: &str) -> Result<()> {
        self.request("surface.close", json!({"surface_id": surface})).await?;
        Ok(())
    }
}

// ─── Backends ──────────────────────────────────────────────────────────────────

enum Backend {
    Cdp(Cdp),
    Tern(Tern),
    Cmux(Cmux),
}

impl Backend {
    fn is_webkit(&self) -> bool {
        matches!(self, Backend::Tern(tern) if tern.child.is_some())
    }

    async fn eval(&mut self, target: &str, expression: &str) -> Result<Value> {
        match self {
            Backend::Cdp(cdp) => cdp.eval(expression).await,
            Backend::Tern(tern) => tern.eval(target.parse()?, expression).await,
            Backend::Cmux(cmux) => cmux.eval(target, expression).await,
        }
    }

    async fn navigate(&mut self, target: &str, url: &str) -> Result<()> {
        match self {
            Backend::Cdp(cdp) => cdp.navigate(url).await,
            Backend::Tern(tern) => tern.navigate(target.parse()?, url).await,
            Backend::Cmux(cmux) => cmux.navigate(target, url).await,
        }
    }

    async fn capture(&mut self, target: &str, full_page: bool) -> Result<Vec<u8>> {
        match self {
            Backend::Cdp(cdp) => cdp.capture(full_page).await,
            Backend::Tern(tern) => tern.capture(target.parse()?).await,
            Backend::Cmux(cmux) => cmux.capture(target).await,
        }
    }

    async fn press(&mut self, target: &str, key: &str) -> Result<()> {
        match self {
            Backend::Cdp(cdp) => {
                let (name, code, text) = key_event(key)?;
                let mut down = json!({"type": "keyDown", "key": name, "windowsVirtualKeyCode": code, "nativeVirtualKeyCode": code});
                if let Some(text) = &text {
                    down["text"] = json!(text);
                }
                cdp.call("Input.dispatchKeyEvent", down).await?;
                cdp.call("Input.dispatchKeyEvent", json!({"type": "keyUp", "key": name, "windowsVirtualKeyCode": code})).await?;
                Ok(())
            }
            Backend::Cmux(cmux) => {
                cmux.request("browser.press", json!({"surface_id": target, "key": key})).await?;
                Ok(())
            }
            Backend::Tern(tern) => {
                // A Tern web view takes key events over its input op only; a DOM event reaches the focused field.
                let script = format!(
                    "(function(){{ const el = document.activeElement || document.body; el.dispatchEvent(new KeyboardEvent('keydown', {{key: {}, bubbles: true}})); el.dispatchEvent(new KeyboardEvent('keyup', {{key: {}, bubbles: true}})); return true; }})()",
                    serde_json::to_string(key)?,
                    serde_json::to_string(key)?
                );
                tern.eval(target.parse()?, &script).await?;
                Ok(())
            }
        }
    }

    /// Freezes or revives a Chromium tab; `false` for a backend that cannot freeze.
    async fn freeze(&mut self, frozen: bool) -> Result<bool> {
        let Backend::Cdp(cdp) = self else { return Ok(false) };
        cdp.call(
            "Page.setWebLifecycleState",
            json!({"state": if frozen { "frozen" } else { "active" }}),
        )
        .await?;
        Ok(true)
    }

    async fn close(&mut self, target: &str) -> Result<()> {
        match self {
            Backend::Cdp(cdp) => cdp.close().await,
            Backend::Tern(tern) => tern.close(target.parse()?).await,
            Backend::Cmux(cmux) => cmux.close(target).await,
        }
    }

    fn describe(&self) -> &'static str {
        match self {
            Backend::Cdp(cdp) if cdp.child.is_some() => "chromium",
            Backend::Cdp(_) => "cdp",
            Backend::Tern(tern) if tern.child.is_some() => "webkit",
            Backend::Tern(_) => "tern",
            Backend::Cmux(_) => "cmux",
        }
    }
}

/// The CDP key event for `key`: a named key or a single character.
fn key_event(key: &str) -> Result<(String, u32, Option<String>)> {
    let named = match key {
        "Enter" | "Return" => ("Enter", 13, Some("\r".to_string())),
        "Tab" => ("Tab", 9, Some("\t".to_string())),
        "Escape" => ("Escape", 27, None),
        "Backspace" => ("Backspace", 8, None),
        "Delete" => ("Delete", 46, None),
        "Space" | " " => (" ", 32, Some(" ".into())),
        "ArrowUp" => ("ArrowUp", 38, None),
        "ArrowDown" => ("ArrowDown", 40, None),
        "ArrowLeft" => ("ArrowLeft", 37, None),
        "ArrowRight" => ("ArrowRight", 39, None),
        "Home" => ("Home", 36, None),
        "End" => ("End", 35, None),
        "PageUp" => ("PageUp", 33, None),
        "PageDown" => ("PageDown", 34, None),
        other => {
            let mut chars = other.chars();
            match (chars.next(), chars.next()) {
                (Some(c), None) => return Ok((c.to_string(), c.to_ascii_uppercase() as u32, Some(c.to_string()))),
                _ => bail!("unknown key {other:?}: use a single character or Enter/Tab/Escape/Backspace/Delete/Space/Arrow*/Home/End/PageUp/PageDown"),
            }
        }
    };
    Ok((named.0.to_string(), named.1, named.2))
}

// ─── The tool ──────────────────────────────────────────────────────────────────

struct Opened {
    backend: Backend,
    target: String,
    kind: &'static str,
    owned: bool,
}

async fn open_backend(cfg: &Config, kind: &Kind) -> Result<Opened> {
    let env = env_map();
    match kind.clone() {
        Kind::Chromium(_) | Kind::Relay(_) | Kind::Cdp(_) => {
            let (url, child, profile, kind_name, owned) = match kind {
                Kind::Chromium(exe) => {
                    let (url, child, profile) = launch_chromium(&cfg.integration_opts.browser, exe.as_deref()).await?;
                    (url, Some(child), Some(profile), "chromium", true)
                }
                Kind::Relay(base) => (discover(base).await?, None, None, "relay", false),
                Kind::Cdp(base) => (discover(base).await?, None, None, "cdp", false),
                _ => unreachable!(),
            };
            let cdp = Cdp::attach(&url, profile, child).await?;
            Ok(Opened { target: cdp.target.clone(), backend: Backend::Cdp(cdp), kind: kind_name, owned })
        }
        Kind::Tern((socket, pane)) => {
            let mut tern = Tern::over_socket(&socket, "Tern").await?;
            let block = match tern.open(pane).await {
                Ok(block) => block,
                Err(error) => bail!(
                    "{error} — a Tern pane that cannot host browser picture-in-pictures falls back only when no explicit \
                     app is given"
                ),
            };
            Ok(Opened { target: block.to_string(), backend: Backend::Tern(tern), kind: "tern", owned: true })
        }
        Kind::Webkit(host) => {
            let mut tern = Tern::over_host(&host).await?;
            let block = tern.open(0).await?;
            Ok(Opened { target: block.to_string(), backend: Backend::Tern(tern), kind: "webkit", owned: true })
        }
        Kind::Cmux(socket) => {
            let mut cmux = Cmux::connect(&socket, env.get("CMUX_SOCKET_PASSWORD").map(String::as_str)).await?;
            let surface = cmux.open("about:blank", &env).await?;
            Ok(Opened { target: surface, backend: Backend::Cmux(cmux), kind: "cmux", owned: false })
        }
    }
}

/// The tab `name` in `cwd`, opened through `kind` when it does not exist yet.
async fn tab_for(
    cfg: &Config,
    cwd: &Path,
    name: &str,
    kind: &Kind,
    persist: bool,
) -> Result<Arc<Mutex<Tab>>> {
    let key = (cwd.to_path_buf(), name.to_string());
    if let Some(tab) = TABS.lock().unwrap().get(&key).cloned() {
        return Ok(tab);
    }
    let Opened { backend, target, kind: kind_name, owned } = open_backend(cfg, kind).await?;
    let tab = Arc::new(Mutex::new(Tab {
        backend,
        url: "about:blank".into(),
        owned,
        persist,
        frozen: false,
        last: Instant::now(),
        target,
        kind: kind_name,
    }));
    register(cwd, name, tab.clone());
    Ok(tab)
}

fn str_arg<'a>(input: &'a Value, key: &str) -> Option<&'a str> {
    input[key].as_str().filter(|s| !s.trim().is_empty())
}

fn require<'a>(input: &'a Value, key: &str) -> Result<&'a str> {
    str_arg(input, key).ok_or_else(|| anyhow!("`{key}` is required for this action"))
}

fn js_string(value: &str) -> Result<String> {
    Ok(serde_json::to_string(value)?)
}

/// Saves a PNG to `browser.screenshot_dir` (or a temp file) and returns its path.
fn save_screenshot(cfg: &Config, bytes: &[u8]) -> Result<PathBuf> {
    use sha2::{Digest, Sha256};
    let dir = match &&cfg.integration_opts.browser.screenshot_dir {
        Some(dir) => BrowserCfg::expand(dir),
        None => std::env::temp_dir(),
    };
    std::fs::create_dir_all(&dir)?;
    let hash = Sha256::digest(bytes);
    let path = dir.join(format!("browser-{}.png", hash[..8].iter().map(|b| format!("{b:02x}")).collect::<String>()));
    std::fs::write(&path, bytes)?;
    Ok(path)
}

/// Runs one browser action and returns its text plus the screenshot it saved, if any.
pub async fn run(cfg: &Config, cwd: &Path, input: &Value) -> Result<(String, Option<PathBuf>)> {
    let action = require(input, "action")?;
    let name = str_arg(input, "name").unwrap_or("main");
    let env = env_map();
    if action == "tabs" {
        let mut rows = vec![];
        for ((_, key), tab) in tabs_in(cwd) {
            let Ok(tab) = tab.try_lock() else {
                rows.push(format!("{key}: busy"));
                continue;
            };
            rows.push(format!(
                "{key}: {} ({}) {} · idle {}s{}",
                tab.url,
                tab.kind,
                if tab.frozen { "frozen" } else { "live" },
                tab.last.elapsed().as_secs(),
                if tab.persist { " · persist" } else { "" }
            ));
        }
        return Ok((
            if rows.is_empty() { "no browser tabs".to_string() } else { rows.join("\n") },
            None,
        ));
    }
    if action == "close" {
        let all = input["all"].as_bool().unwrap_or(false) || name == "*";
        let targets = if all { tabs_in(cwd) } else { tabs_in(cwd).into_iter().filter(|((_, n), _)| n == name).collect() };
        if targets.is_empty() {
            bail!("no browser tab named {name:?} in {}", cwd.display());
        }
        let mut closed = 0;
        for (key, tab) in targets {
            tab.lock().await.close().await.with_context(|| format!("closing tab {}", key.1))?;
            forget(&key.0, &key.1);
            closed += 1;
        }
        return Ok((format!("closed {closed} browser tab(s)"), None));
    }
    // An idle timer only fires while the process is inside a browser call; turn end and open both reap.
    if action == "open" {
        reap_idle(cwd, cfg).await;
    }
    let kind = resolve(&cfg.integration_opts.browser, str_arg(input, "app"), &env);
    match &kind {
        Kind::Tern((socket, _)) if socket.is_empty() => {
            bail!("app: \"tern\" requires running inside a Tern pane (TERN_PANE_SOCKET and TERN_PANE are not set)")
        }
        Kind::Cmux(socket) if socket.is_empty() => {
            bail!("app: \"cmux\" requires CMUX_SOCKET_PATH to name the cmux socket")
        }
        _ => {}
    }
    let persist = input["persist"].as_bool().unwrap_or(false);
    let tab = tab_for(cfg, cwd, name, &kind, persist).await?;
    let screenshot = match action {
        "open" | "goto" => {
            let url = require(input, "url")?;
            let mut tab = tab.lock().await;
            tab.resume().await?;
            let target = tab.target.clone();
            tab.backend.navigate(&target, url).await?;
            tab.url = url.to_string();
            (format!("{} tab {name:?} at {url}", tab.kind), None)
        }
        "eval" => {
            let expression = require(input, "expression")?;
            let mut tab = tab.lock().await;
            tab.resume().await?;
            let target = tab.target.clone();
            let value = tab.backend.eval(&target, expression).await?;
            (match value {
                Value::String(text) => text,
                Value::Null => "null".into(),
                other => serde_json::to_string_pretty(&other)?,
            }, None)
        }
        "click" => {
            let selector = require(input, "selector")?;
            let script = format!(
                "(function(){{ const el = document.querySelector({sel}); if (!el) throw new Error('no element matches ' + {sel}); el.scrollIntoView({{block: 'center'}}); el.click(); return true; }})()",
                sel = js_string(selector)?
            );
            let mut tab = tab.lock().await;
            tab.resume().await?;
            let target = tab.target.clone();
            tab.backend.eval(&target, &script).await?;
            (format!("clicked {selector}"), None)
        }
        "type" => {
            let selector = require(input, "selector")?;
            let text = require(input, "text")?;
            let script = format!(
                "(function(){{ const el = document.querySelector({sel}); if (!el) throw new Error('no element matches ' + {sel}); el.focus(); \
                 if (el.isContentEditable) el.textContent = {text}; else el.value = {text}; \
                 el.dispatchEvent(new Event('input', {{bubbles: true}})); el.dispatchEvent(new Event('change', {{bubbles: true}})); return true; }})()",
                sel = js_string(selector)?,
                text = js_string(text)?
            );
            let mut tab = tab.lock().await;
            tab.resume().await?;
            let target = tab.target.clone();
            tab.backend.eval(&target, &script).await?;
            (format!("typed {} chars into {selector}", text.chars().count()), None)
        }
        "press" => {
            let key = require(input, "key")?;
            let mut tab = tab.lock().await;
            tab.resume().await?;
            let target = tab.target.clone();
            tab.backend.press(&target, key).await?;
            (format!("pressed {key}"), None)
        }
        "screenshot" => {
            let full_page = input["full_page"].as_bool().unwrap_or(false);
            let mut tab = tab.lock().await;
            tab.resume().await?;
            let target = tab.target.clone();
            let png = tab.backend.capture(&target, full_page).await?;
            let path = save_screenshot(cfg, &png)?;
            (format!("screenshot saved to {}", path.display()), Some(path))
        }
        "snapshot" => {
            let script = "(function(){ const text = (document.body && document.body.innerText) || ''; \
                          return {url: location.href, title: document.title, text: text.slice(0, 20000)}; })()";
            let mut tab = tab.lock().await;
            tab.resume().await?;
            let target = tab.target.clone();
            let value = tab.backend.eval(&target, script).await?;
            tab.url = value["url"].as_str().unwrap_or(&tab.url).to_string();
            (
                format!(
                    "{}\n{}\n---\n{}",
                    value["url"].as_str().unwrap_or(""),
                    value["title"].as_str().unwrap_or(""),
                    value["text"].as_str().unwrap_or("")
                ),
                None,
            )
        }
        "wait" => {
            let expression = require(input, "expression")?;
            let timeout = Duration::from_millis(input["timeout_ms"].as_u64().unwrap_or(30_000).min(600_000));
            let deadline = Instant::now() + timeout;
            let mut tab = tab.lock().await;
            tab.resume().await?;
            loop {
                let target = tab.target.clone();
            let value = tab.backend.eval(&target, expression).await?;
                if !matches!(value, Value::Bool(false) | Value::Null) {
                    break (format!("ok: {value}"), None);
                }
                if Instant::now() >= deadline {
                    bail!("{expression} stayed false for {}ms", timeout.as_millis());
                }
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
        }
        other => bail!("unknown browser action {other:?}"),
    };
    Ok(screenshot)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::atomic::{AtomicBool, Ordering};

    fn env(pairs: &[(&str, &str)]) -> Env {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    fn cfg_with(url: &str) -> Config {
        let mut cfg = Config::default();
        cfg.integration_opts.browser.cdp_url = Some(url.to_string());
        cfg
    }

    /// Serves `/json/version` in plain HTTP and CDP on the same port; records every method it is sent.
    async fn mock_cdp() -> (String, Arc<StdMutex<Vec<String>>>, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let seen = Arc::new(StdMutex::new(Vec::new()));
        let log = seen.clone();
        let handle = tokio::spawn(async move {
            loop {
                let Ok((mut tcp, _)) = listener.accept().await else { return };
                let log = log.clone();
                tokio::spawn(async move {
                    let mut head = [0u8; 4];
                    // Discovery is a plain GET; a WebSocket handshake starts with the same bytes, so read more.
                    let mut buffer = vec![0u8; 4096];
                    let read = tcp.peek(&mut buffer).await.unwrap_or(0);
                    let _ = &mut head;
                    let request = String::from_utf8_lossy(&buffer[..read]).to_string();
                    if !request.starts_with("GET ") || request.contains("Upgrade: websocket") || request.contains("Upgrade: WebSocket") {
                        let mut socket = match tokio_tungstenite::accept_async(tcp).await {
                            Ok(socket) => socket,
                            Err(_) => return,
                        };
                        while let Some(Ok(frame)) = socket.next().await {
                            let Message::Text(text) = frame else { continue };
                            let call: Value = match serde_json::from_str(&text) {
                                Ok(call) => call,
                                Err(_) => continue,
                            };
                            let method = call["method"].as_str().unwrap_or("").to_string();
                            log.lock().unwrap().push(method.clone());
                            let result = match method.as_str() {
                                "Target.createTarget" => json!({"targetId": "target-1"}),
                                "Target.attachToTarget" => json!({"sessionId": "session-1"}),
                                "Page.navigate" => json!({"frameId": "frame-1"}),
                                "Page.setWebLifecycleState" | "Target.closeTarget" => json!({}),
                                "Page.captureScreenshot" => json!({"data": base64::engine::general_purpose::STANDARD.encode(png_bytes())}),
                                "Runtime.evaluate" => {
                                    let expression = call["params"]["expression"].as_str().unwrap_or("");
                                    let value = match expression {
                                        "document.readyState" => json!("complete"),
                                        "document.title" => json!("Mock"),
                                        other if other.contains("location.href") => {
                                            json!({"url": "https://example.test/", "title": "Mock", "text": "hello"})
                                        }
                                        _ => json!(42),
                                    };
                                    json!({"result": {"type": "number", "value": value}})
                                }
                                _ => json!({}),
                            };
                            let reply = json!({"id": call["id"], "result": result});
                            if socket.send(Message::Text(reply.to_string().into())).await.is_err() {
                                return;
                            }
                        }
                        return;
                    }
                    use tokio::io::AsyncWriteExt;
                    let body = format!(r#"{{"webSocketDebuggerUrl":"ws://127.0.0.1:{port}/devtools/browser/mock"}}"#);
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = tcp.write_all(response.as_bytes()).await;
                });
            }
        });
        (format!("http://127.0.0.1:{port}"), seen, handle)
    }

    fn png_bytes() -> Vec<u8> {
        // 1×1 transparent PNG.
        base64::engine::general_purpose::STANDARD
            .decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAAC0lEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==")
            .unwrap()
    }

    fn temp_cwd(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("greencode-browser-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// The whole flow against a mock browser: open, eval, snapshot, screenshot, freeze, close.
    #[tokio::test]
    async fn drives_a_mock_cdp_browser() {
        let (url, seen, server) = mock_cdp().await;
        let cwd = temp_cwd("cdp");
        let shots = cwd.join("shots");
        let mut cfg = cfg_with(&url);
        cfg.integration_opts.browser.screenshot_dir = Some(shots.to_string_lossy().into());
        let run = |action: Value| {
            let cfg = cfg.clone();
            let cwd = cwd.clone();
            async move { run(&cfg, &cwd, &action).await }
        };
        let (text, _) = run(json!({"action": "open", "name": "main", "url": "https://example.test/"})).await.unwrap();
        assert!(text.contains("cdp") && text.contains("https://example.test/"), "{text}");
        let (value, _) = run(json!({"action": "eval", "name": "main", "expression": "1 + 1"})).await.unwrap();
        assert_eq!(value, "42");
        let (snapshot, _) = run(json!({"action": "snapshot", "name": "main"})).await.unwrap();
        assert!(snapshot.contains("Mock") && snapshot.contains("hello"), "{snapshot}");
        let (text, path) = run(json!({"action": "screenshot", "name": "main"})).await.unwrap();
        let path = path.expect("screenshot path");
        assert_eq!(path.parent(), Some(shots.as_path()), "{text}");
        assert_eq!(std::fs::read(&path).unwrap(), png_bytes());
        // Tab reuse: a second open of the same name does not create another target.
        run(json!({"action": "goto", "name": "main", "url": "https://example.test/next"})).await.unwrap();
        assert_eq!(seen.lock().unwrap().iter().filter(|m| *m == "Target.createTarget").count(), 1);
        // A launched browser's tab is OMP-owned; this mock attaches over CDP, so mark it as the managed
        // Chromium tab the turn-end pass freezes (launching a real browser is out of scope here).
        for (_, tab) in tabs_in(&cwd) {
            let mut tab = tab.lock().await;
            tab.kind = "chromium";
            tab.owned = true;
        }
        // Turn end freezes the tab, and the next use revives it before touching the page.
        settle(&cwd, &cfg).await;
        assert!(seen.lock().unwrap().iter().any(|m| m == "Page.setWebLifecycleState"));
        run(json!({"action": "eval", "name": "main", "expression": "1"})).await.unwrap();
        let states = seen.lock().unwrap().iter().filter(|m| *m == "Page.setWebLifecycleState").count();
        assert_eq!(states, 2, "freeze then unfreeze");
        run(json!({"action": "close", "name": "main"})).await.unwrap();
        assert!(seen.lock().unwrap().iter().any(|m| m == "Target.closeTarget"));
        assert!(tabs_in(&cwd).is_empty());
        forget(&cwd, "main");
        server.abort();
        let _ = std::fs::remove_dir_all(&cwd);
    }

    /// `persist: true` opts a tab out of turn-end freezing, and idle-close never fires below its threshold.
    #[tokio::test]
    async fn persist_and_idle_close_light_their_switches() {
        let (url, seen, server) = mock_cdp().await;
        let cwd = temp_cwd("idle");
        let mut cfg = cfg_with(&url);
        cfg.integration_opts.browser.freeze_on_turn_end = true;
        let (_, _) = run(&cfg, &cwd, &json!({"action": "open", "name": "keep", "url": "about:blank", "persist": true}))
            .await
            .unwrap();
        settle(&cwd, &cfg).await;
        assert!(!seen.lock().unwrap().iter().any(|m| m == "Page.setWebLifecycleState"), "a persisted tab stays live");
        // A 60s threshold with a fresh tab closes nothing.
        cfg.integration_opts.browser.idle_close_sec = 60;
        cfg.integration_opts.browser.freeze_on_turn_end = false;
        reap_idle(&cwd, &cfg).await;
        assert_eq!(tabs_in(&cwd).len(), 1);
        // An elapsed timer does close it.
        cfg.integration_opts.browser.idle_close_sec = 1;
        for (_, tab) in tabs_in(&cwd) {
            let mut tab = tab.lock().await;
            tab.last = Instant::now() - Duration::from_secs(5);
        }
        reap_idle(&cwd, &cfg).await;
        assert!(tabs_in(&cwd).is_empty());
        assert!(seen.lock().unwrap().iter().any(|m| m == "Target.closeTarget"));
        server.abort();
        let _ = std::fs::remove_dir_all(&cwd);
    }

    /// Precedence: explicit app, then relay, then cdpUrl, then Tern/cmux, then the native browser.
    #[test]
    fn resolves_backends_in_omp_order() {
        let plain = BrowserCfg::default();
        let relay_env = env(&[("GREENCODE_BROWSER_RELAY", "1")]);
        // app wins over every setting
        assert_eq!(resolve(&plain, Some("chromium"), &relay_env), Kind::Chromium(None));
        assert!(matches!(resolve(&plain, Some("wx"), &Env::default()), Kind::Chromium(Some(exe)) if exe == "wx"));
        // relay beats cdp_url, and its URL is configurable
        let cdp = BrowserCfg { cdp_url: Some("http://127.0.0.1:9222/".into()), relay_url: Some("http://relay.lan:9224".into()), ..plain.clone() };
        assert_eq!(resolve(&cdp, None, &relay_env), Kind::Relay("http://relay.lan:9224".into()));
        // GREENCODE_BROWSER_RELAY=0 turns it off again, leaving the configured endpoint
        let off = env(&[("GREENCODE_BROWSER_RELAY", "0")]);
        assert_eq!(resolve(&cdp, None, &off), Kind::Cdp("http://127.0.0.1:9222".into()));
        // app: "auto" skips the relay but keeps the endpoint
        assert_eq!(resolve(&cdp, Some("auto"), &relay_env), Kind::Cdp("http://127.0.0.1:9222".into()));
        // An explicit HTTP app URL is a discovery endpoint, not a keyword
        assert_eq!(resolve(&cdp, Some("http://box:9333/"), &off), Kind::Cdp("http://box:9333".into()));
        // Tern needs a pane; another session's pane is left alone.
        let pane = env(&[("TERN_PANE_SOCKET", "/tmp/tern.sock"), ("TERN_PANE", "7")]);
        assert_eq!(resolve(&plain, None, &pane), Kind::Tern(("/tmp/tern.sock".into(), 7)));
        let foreign = env(&[
            ("TERN_PANE_SOCKET", "/tmp/tern.sock"),
            ("TERN_PANE", "7"),
            ("GREENCODE_TERMINAL_OWNER_PID", "999999"),
        ]);
        assert_eq!(resolve(&plain, None, &foreign), Kind::Chromium(None));
        // app: "tern" inside a pane whose socket is unset has no pane to open in.
        assert_eq!(resolve(&plain, Some("tern"), &Env::default()), Kind::Tern(("".into(), 0)));
        assert_eq!(resolve(&plain, None, &env(&[("CMUX_SOCKET_PATH", "/tmp/cmux.sock")])), Kind::Cmux("/tmp/cmux.sock".into()));
        // No backends configured: the native browser.
        assert_eq!(resolve(&plain, None, &Env::default()), Kind::Chromium(None));
        // app: "webkit" without an engine still launches Chromium rather than failing.
        assert_eq!(resolve(&plain, Some("webkit"), &Env::default()), Kind::Chromium(None));
    }

    /// The WebKit host is found at a configured path and only when it is executable.
    #[test]
    fn finds_the_webkit_host_by_path() {
        let dir = temp_cwd("webkit");
        let host = dir.join("ABHost");
        assert_eq!(webkit_host(&dir), None);
        std::fs::write(&host, "#!/bin/sh\n").unwrap();
        assert_eq!(webkit_host(&dir), None, "not executable yet");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&host, std::fs::Permissions::from_mode(0o755)).unwrap();
            assert_eq!(webkit_host(&dir), Some(host.clone()));
            assert_eq!(webkit_host(&host), Some(host));
        }
        // A wrong configured path does not silently fall back to the home default.
        let cfg = BrowserCfg { webkit_path: Some(dir.join("missing").to_string_lossy().into()), ..BrowserCfg::default() };
        assert_eq!(installed_webkit(&cfg, &Env::default()), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The Tern wire: 4-byte LE length then JSON, `{"hello":{}}` greeted and `browser` ops answered.
    #[tokio::test]
    async fn speaks_the_tern_wire() {
        let dir = temp_cwd("tern");
        let socket_path = dir.join("tern.sock");
        let listener = tokio::net::UnixListener::bind(&socket_path).unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut header = [0u8; 4];
            stream.read_exact(&mut header).await.unwrap();
            let mut body = vec![0u8; u32::from_le_bytes(header) as usize];
            stream.read_exact(&mut body).await.unwrap();
            assert_eq!(serde_json::from_slice::<Value>(&body).unwrap()["hello"], json!({}));
            write_frame(&mut stream, &json!({"welcome": {"ops": ["browser"]}})).await;
            loop {
                let mut header = [0u8; 4];
                if stream.read_exact(&mut header).await.is_err() {
                    return;
                }
                let mut body = vec![0u8; u32::from_le_bytes(header) as usize];
                stream.read_exact(&mut body).await.unwrap();
                let request: Value = serde_json::from_slice(&body).unwrap();
                let op = request["browser"]["op"].as_str().unwrap().to_string();
                let ok = match op.as_str() {
                    "open" => json!({"block": 3}),
                    "eval" => json!({"value": "Tern"}),
                    "capture" => json!({"data": base64::engine::general_purpose::STANDARD.encode(png_bytes())}),
                    _ => json!({}),
                };
                let reply = json!({"id": request["id"], "browser": {"ok": ok}});
                write_frame(&mut stream, &reply).await;
            }
        });
        async fn write_frame(stream: &mut tokio::net::UnixStream, value: &Value) {
            let body = serde_json::to_vec(value).unwrap();
            stream.write_all(&(body.len() as u32).to_le_bytes()).await.unwrap();
            stream.write_all(&body).await.unwrap();
        }
        let mut tern = Tern::over_socket(&socket_path.to_string_lossy(), "Tern").await.unwrap();
        let block = tern.open(1).await.unwrap();
        assert_eq!(block, 3);
        assert_eq!(tern.eval(block, "document.title").await.unwrap(), json!("Tern"));
        assert_eq!(tern.capture(block).await.unwrap(), png_bytes());
        server.abort();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The cmux line-delimited RPC: `open_split`, `navigate`, `eval`, `screenshot`, `surface.close`.
    #[tokio::test]
    async fn speaks_the_cmux_socket() {
        let dir = temp_cwd("cmux");
        let socket_path = dir.join("cmux.sock");
        let listener = tokio::net::UnixListener::bind(&socket_path).unwrap();
        let closed = Arc::new(AtomicBool::new(false));
        let flag = closed.clone();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (read, mut write) = stream.into_split();
            let mut lines = BufReader::new(read).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let request: Value = serde_json::from_str(&line).unwrap();
                let method = request["method"].as_str().unwrap().to_string();
                if method == "surface.close" {
                    flag.store(true, Ordering::SeqCst);
                }
                let result = match method.as_str() {
                    "browser.open_split" => json!({"surface_id": "surface-9"}),
                    "browser.eval" => json!({"value": "cmux"}),
                    "browser.screenshot" => json!({"png_base64": base64::engine::general_purpose::STANDARD.encode(png_bytes())}),
                    _ => json!({}),
                };
                let reply = json!({"ok": true, "result": result});
                let _ = write.write_all(format!("{reply}\n").as_bytes()).await;
            }
        });
        let mut cmux = Cmux::connect(&socket_path.to_string_lossy(), None).await.unwrap();
        let surface = cmux.open("https://example.test/", &Env::default()).await.unwrap();
        assert_eq!(surface, "surface-9");
        cmux.navigate(&surface, "https://example.test/next").await.unwrap();
        assert_eq!(cmux.eval(&surface, "document.title").await.unwrap(), json!("cmux"));
        assert_eq!(cmux.capture(&surface).await.unwrap(), png_bytes());
        cmux.close(&surface).await.unwrap();
        assert!(closed.load(Ordering::SeqCst));
        server.abort();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A relay that is up with no extension answers 503, and the error says what to do.
    #[tokio::test]
    async fn relay_without_an_extension_explains_itself() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (mut tcp, _) = listener.accept().await.unwrap();
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            // Closing with the request unread resets the connection, which the client sees as no answer.
            let _ = tcp.read(&mut [0u8; 1024]).await;
            let body = r#"{"extensionSeen":false,"uptimeMs":5}"#;
            let response = format!(
                "HTTP/1.1 503 Service Unavailable\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = tcp.write_all(response.as_bytes()).await;
        });
        let error = discover(&format!("http://127.0.0.1:{port}")).await.unwrap_err().to_string();
        assert!(error.contains("503") && error.contains("extension"), "{error}");
    }

    /// An unreachable endpoint names the setting that points at it.
    #[tokio::test]
    async fn an_unreachable_endpoint_names_cdp_url() {
        let error = discover("http://127.0.0.1:1").await.unwrap_err().to_string();
        assert!(error.contains("cdp_url"), "{error}");
    }

    /// Opening without any browser installed says what to install instead of a bare spawn error.
    #[tokio::test]
    async fn missing_chromium_names_the_options() {
        let cfg = BrowserCfg { headless: true, ..BrowserCfg::default() };
        let error = launch_chromium(&cfg, Some("/nonexistent/browser")).await.unwrap_err().to_string();
        assert!(error.contains("/nonexistent/browser"), "{error}");
    }

    /// Screenshots land in `browser.screenshot_dir` with `~` expanded, and collide by content only.
    #[test]
    fn screenshot_directory_and_hash() {
        let dir = temp_cwd("shots");
        let cfg = Config {
            integration_opts: crate::config::integrations::IntegrationOpts {
                browser: BrowserCfg { screenshot_dir: Some(dir.join("out").to_string_lossy().into()), ..BrowserCfg::default() },
                ..Default::default()
            },
            ..Config::default()
        };
        let first = save_screenshot(&cfg, b"png").unwrap();
        let again = save_screenshot(&cfg, b"png").unwrap();
        assert_eq!(first, again);
        assert_eq!(first.parent(), Some(dir.join("out").as_path()));
        assert_ne!(save_screenshot(&cfg, b"other").unwrap(), first);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
