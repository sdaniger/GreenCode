//! Managed Google Antigravity ACP runtime, OAuth negotiation, and state.

use crate::{
    config::{AcpCfg, Config, Protocol},
    jsonrpc::{Incoming, Rpc, RpcError},
};
use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
    time::Duration,
};
use tokio::sync::mpsc;

pub const BACKEND: &str = "antigravity";
pub const REGISTRY_ID: &str = "antigravity-acp";

pub const AUTH_TYPES: &[(&str, &str)] = &[
    ("oauth-personal", "Google アカウント（個人）"),
    ("oauth-business", "Gemini Enterprise（組織）"),
    ("gemini-api-key", "GEMINI_API_KEY"),
];

pub fn is_alias(name: &str) -> bool {
    matches!(name, "antigravity" | "antigravity-acp" | "agy")
}

pub fn canonical_backend(name: &str) -> &str {
    if is_alias(name) {
        BACKEND
    } else {
        name
    }
}

pub fn auth_type_label(auth_type: &str) -> &'static str {
    match auth_type {
        "oauth-personal" => "Google アカウント（個人）",
        "oauth-business" => "Gemini Enterprise（組織）",
        "gemini-api-key" => "GEMINI_API_KEY",
        "agent-platform" => "Google Cloud (ADC)",
        _ => "Google アカウント（個人）",
    }
}

pub fn gemini_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("GEMINI_HOME").filter(|s| !s.is_empty()) {
        return PathBuf::from(dir);
    }
    if let Some(dir) = std::env::var_os("GEMINI_CLI_HOME").filter(|s| !s.is_empty()) {
        return PathBuf::from(dir).join(".gemini");
    }
    crate::user_home().join(".gemini")
}

pub fn state_path() -> PathBuf {
    crate::home().join("antigravity.json")
}

fn settings_path(gemini_home: &Path) -> PathBuf {
    gemini_home.join("antigravity-acp").join("settings.json")
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Status {
    pub signed_in: bool,
    pub auth_type: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct StoredState {
    #[serde(default)]
    signed_in: bool,
    #[serde(default)]
    auth_type: String,
}

fn read_stored_state(path: &Path) -> Option<StoredState> {
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str::<StoredState>(&text).ok()
}

fn read_settings_auth_type(gemini_home: &Path) -> Option<String> {
    let text = std::fs::read_to_string(settings_path(gemini_home)).ok()?;
    let v: Value = serde_json::from_str(&text).ok()?;
    v["auth"]["type"]
        .as_str()
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn non_empty_json_file(path: &Path) -> bool {
    std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.len() > 2)
}

fn has_gemini_api_key() -> bool {
    std::env::var_os("GEMINI_API_KEY").is_some_and(|v| !v.is_empty())
        || crate::auth::stored_keys().contains_key("google")
}

pub fn status() -> Status {
    let gdir = gemini_dir();
    let stored = read_stored_state(&state_path());
    let auth_type = stored
        .as_ref()
        .map(|s| s.auth_type.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .or_else(|| read_settings_auth_type(&gdir))
        .unwrap_or_else(|| "oauth-personal".into());
    let signed_in = if auth_type == "gemini-api-key" {
        has_gemini_api_key()
    } else {
        stored.as_ref().is_some_and(|s| s.signed_in)
            || non_empty_json_file(&gdir.join("antigravity-acp").join("acp_token.json"))
            || non_empty_json_file(&gdir.join("antigravity-cli").join("oauth_creds.json"))
            || non_empty_json_file(&gdir.join("oauth_creds.json"))
    };
    Status {
        signed_in,
        auth_type,
    }
}

fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    let parent = path.parent().context("missing parent directory")?;
    std::fs::create_dir_all(parent)?;
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("file");
    let tmp = path.with_file_name(format!(".{name}.{}.tmp", std::process::id()));
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut opts, 0o600);
    let mut file = opts.open(&tmp)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

fn valid_auth_type(auth_type: &str) -> bool {
    matches!(
        auth_type,
        "oauth-personal" | "oauth-business" | "gemini-api-key" | "agent-platform"
    )
}

pub fn set_auth_type(auth_type: &str) -> Result<()> {
    anyhow::ensure!(
        valid_auth_type(auth_type),
        "unknown Antigravity auth type `{auth_type}`"
    );
    let prev = read_stored_state(&state_path());
    let signed_in = if auth_type == "gemini-api-key" {
        has_gemini_api_key()
    } else if prev.as_ref().is_some_and(|s| s.auth_type == auth_type) {
        prev.unwrap().signed_in
    } else {
        false
    };
    let state = json!({"signed_in": signed_in, "auth_type": auth_type});
    write_private(&state_path(), serde_json::to_string_pretty(&state)?.as_bytes())?;

    let settings_file = settings_path(&gemini_dir());
    let mut doc = match std::fs::read_to_string(&settings_file) {
        Ok(text) if text.trim().is_empty() => Map::new(),
        Ok(text) => match serde_json::from_str::<Value>(&text) {
            Ok(Value::Object(map)) => map,
            _ => bail!("{} is not a JSON object; fix or delete it", settings_file.display()),
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Map::new(),
        Err(e) => return Err(e.into()),
    };
    let mut auth_obj = doc
        .get("auth")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    auth_obj.insert("type".into(), json!(auth_type));
    doc.insert("auth".into(), Value::Object(auth_obj));
    write_private(
        &settings_file,
        serde_json::to_string_pretty(&Value::Object(doc))?.as_bytes(),
    )?;
    Ok(())
}

pub fn mark_signed_in(signed_in: bool, auth_type: Option<&str>) -> Result<()> {
    let gdir = gemini_dir();
    let current = read_stored_state(&state_path());
    let resolved = auth_type
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .or_else(|| {
            current
                .as_ref()
                .map(|s| s.auth_type.clone())
                .filter(|s| !s.is_empty())
        })
        .or_else(|| read_settings_auth_type(&gdir))
        .unwrap_or_else(|| "oauth-personal".into());
    let state = json!({"signed_in": signed_in, "auth_type": resolved});
    write_private(&state_path(), serde_json::to_string_pretty(&state)?.as_bytes())?;
    Ok(())
}

pub fn resolve_command() -> (Vec<String>, BTreeMap<String, String>, Option<String>) {
    if let Some(installed) = crate::registry::installed()
        .into_iter()
        .find(|i| i.id == REGISTRY_ID || i.id == BACKEND)
    {
        if installed
            .command
            .first()
            .is_some_and(|c| Path::new(c).is_file())
        {
            return (installed.command, installed.env, Some(installed.version));
        }
    }
    if let Some(path) = crate::config::executable("agy") {
        return (
            vec![path.to_string_lossy().into_owned(), "acp".into()],
            BTreeMap::new(),
            None,
        );
    }
    if let Some(path) = crate::config::executable("agy_acp_server") {
        return (
            vec![path.to_string_lossy().into_owned()],
            BTreeMap::new(),
            None,
        );
    }
    (vec!["agy".into(), "acp".into()], BTreeMap::new(), None)
}

pub fn is_runtime_installed(b: &AcpCfg) -> bool {
    b.command
        .first()
        .and_then(|c| crate::config::executable(c))
        .is_some()
}

pub fn installed_version() -> Option<String> {
    crate::registry::installed()
        .into_iter()
        .find(|i| i.id == REGISTRY_ID || i.id == BACKEND)
        .map(|i| i.version)
}

pub async fn install_or_update() -> Result<AcpCfg> {
    let json = crate::registry::fetch().await?;
    install_from_registry_json(&json).await
}

pub async fn install_from_registry_json(json: &str) -> Result<AcpCfg> {
    let entries = crate::registry::parse(json)?;
    let entry = entries
        .into_iter()
        .find(|e| e.id == REGISTRY_ID || e.id == BACKEND)
        .context("antigravity-acp was not found in the ACP registry")?;
    let mut cfg = crate::registry::install(&entry).await?;
    cfg.name = BACKEND.into();
    cfg.protocol = Protocol::Acp;
    Ok(cfg)
}

pub async fn ensure_installed(cfg: &Config) -> Result<(Vec<String>, BTreeMap<String, String>)> {
    if let Some(b) = cfg
        .backends
        .iter()
        .find(|b| b.name == BACKEND && is_runtime_installed(b))
    {
        return Ok((b.command.clone(), b.env.clone()));
    }
    let installed = install_or_update().await?;
    Ok((installed.command, installed.env))
}

/// Endpoint the Antigravity runtime talks to, from `providers.antigravityEndpoint`.
/// `auto` starts on production; the caller may retry against `SAND_BOX`.
pub fn endpoint_hosts(mode: crate::config::services_extra::AntigravityEndpoint) -> [&'static str; 2] {
    use crate::config::services_extra::AntigravityEndpoint::*;
    const PROD: &str = "https://daily-cloudcode-pa.googleapis.com";
    const SAND: &str = "https://daily-cloudcode-pa.sandbox.googleapis.com";
    match mode {
        Production => [PROD, PROD],
        Sandbox => [SAND, SAND],
        Auto => [PROD, SAND],
    }
}

/// Environment that routes the runtime's chat, search, image and discovery calls.
pub fn endpoint_env(mode: crate::config::services_extra::AntigravityEndpoint, base: &mut BTreeMap<String, String>) {
    let [first, second] = endpoint_hosts(mode);
    base.entry("CODE_ASSIST_ENDPOINT".into()).or_insert_with(|| first.into());
    if first != second {
        base.entry("CODE_ASSIST_ENDPOINT_FALLBACK".into()).or_insert_with(|| second.into());
    }
}

pub fn runtime_env(base: &BTreeMap<String, String>, no_browser: bool) -> BTreeMap<String, String> {
    let mut env = base.clone();
    if no_browser {
        env.entry("NO_BROWSER".into()).or_insert_with(|| "true".into());
    }
    if status().auth_type == "gemini-api-key"
        && !env.contains_key("GEMINI_API_KEY")
        && std::env::var_os("GEMINI_API_KEY").is_none_or(|v| v.is_empty())
    {
        if let Some(key) = crate::auth::stored_keys().get("google") {
            env.insert("GEMINI_API_KEY".into(), key.clone());
        }
    }
    env
}

pub fn is_auth_required(err: &anyhow::Error) -> bool {
    if err
        .downcast_ref::<RpcError>()
        .is_some_and(|e| e.code == -32000)
    {
        return true;
    }
    let msg = err.to_string();
    msg.contains("Authentication required") || msg.contains("Call authenticate")
}

pub fn select_method_id(auth_methods: &Value, preferred: &str) -> Option<String> {
    let list = auth_methods.as_array()?;
    let ids: Vec<&str> = list.iter().filter_map(|m| m["id"].as_str()).collect();
    if ids.is_empty() {
        return None;
    }
    ids.iter()
        .find(|&&id| id == preferred)
        .or_else(|| ids.iter().find(|&&id| id == "oauth-personal"))
        .or_else(|| ids.iter().find(|&&id| id.starts_with("oauth")))
        .or_else(|| ids.first())
        .map(|s| (*s).to_string())
}

pub fn extract_auth_url(line: &str) -> Option<String> {
    for raw in line.split(|c: char| c.is_whitespace() || matches!(c, '"' | '\'' | '<' | '>' | '`')) {
        let token = raw
            .trim_start_matches(['(', '['])
            .trim_end_matches(['.', ',', ';', ')', ']', '}']);
        if !(token.starts_with("https://") || token.starts_with("http://")) {
            continue;
        }
        let Ok(url) = url::Url::parse(token) else {
            continue;
        };
        if url.scheme() == "https" && url.host_str() == Some("accounts.google.com") {
            return Some(url.into());
        }
        if matches!(url.scheme(), "http" | "https")
            && url
                .query_pairs()
                .any(|(k, v)| k == "client_id" || (k == "response_type" && v == "code"))
        {
            return Some(url.into());
        }
    }
    None
}

pub async fn submit_callback_url(callback_url: &str, auth_url: Option<&str>) -> Result<()> {
    let url = url::Url::parse(callback_url.trim()).context("有効な URL を入力してください")?;
    anyhow::ensure!(
        url.scheme() == "http"
            && matches!(
                url.host_str(),
                Some("127.0.0.1" | "localhost" | "[::1]")
            ),
        "コールバック URL は http://127.0.0.1 または http://localhost のみ送信できます"
    );
    if let Some(auth) = auth_url.and_then(|s| url::Url::parse(s).ok()) {
        if let Some((_, redirect)) = auth.query_pairs().find(|(k, _)| k == "redirect_uri") {
            if let Ok(redir_url) = url::Url::parse(&redirect) {
                anyhow::ensure!(
                    redir_url.port_or_known_default() == url.port_or_known_default(),
                    "このコールバック URL は現在のサインインのものではありません"
                );
            }
        }
        let expected_state = auth
            .query_pairs()
            .find(|(k, _)| k == "state")
            .map(|(_, v)| v.into_owned());
        let actual_state = url
            .query_pairs()
            .find(|(k, _)| k == "state")
            .map(|(_, v)| v.into_owned());
        if let (Some(exp), Some(act)) = (expected_state, actual_state) {
            anyhow::ensure!(
                exp == act,
                "このコールバック URL は現在のサインインのものではありません"
            );
        }
    }
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(10))
        .build()?;
    let resp = client.get(url).send().await?;
    anyhow::ensure!(
        resp.status().is_success() || resp.status().is_redirection(),
        "コールバックの送信に失敗しました: HTTP {}",
        resp.status()
    );
    Ok(())
}

#[derive(Debug, Clone)]
pub struct LoginDone {
    pub auth_type: String,
    pub models: Vec<String>,
    pub options: Vec<Value>,
}

pub struct PendingLogin {
    pub url: String,
    pub auth_type: String,
    rpc: Arc<Rpc>,
    incoming: mpsc::UnboundedReceiver<Incoming>,
    _out_rx: mpsc::UnboundedReceiver<String>,
    auth_task: Option<tokio::task::JoinHandle<Result<Value>>>,
}

impl Drop for PendingLogin {
    fn drop(&mut self) {
        if let Some(task) = self.auth_task.take() {
            task.abort();
        }
    }
}

pub enum LoginStart {
    Ready(LoginDone),
    Browser(PendingLogin),
}

async fn complete_login(
    rpc: &Arc<Rpc>,
    incoming: &mut mpsc::UnboundedReceiver<Incoming>,
    auth_type: &str,
    cwd: &Path,
) -> Result<LoginDone> {
    let mut state = rpc
        .request("session/new", json!({"cwd": cwd, "mcpServers": []}))
        .await?;
    let quiet = Duration::from_millis(250);
    let _ = tokio::time::timeout(Duration::from_secs(2), async {
        while let Ok(Some(inc)) = tokio::time::timeout(quiet, incoming.recv()).await {
            if let Incoming::Notification { method, params } = inc {
                if method == "session/update"
                    && params["update"]["sessionUpdate"] == "config_option_update"
                {
                    state["configOptions"] = params["update"]["configOptions"].clone();
                }
            }
        }
    })
    .await;
    let models = crate::remote_models("acp_state", &state).unwrap_or_default();
    let options = state["configOptions"]
        .as_array()
        .map(|opts| crate::cacheable_picker_options(opts))
        .unwrap_or_default();
    if !models.is_empty() {
        crate::cache_models_for_backend(BACKEND, cwd, &models);
    }
    if !options.is_empty() {
        let _ = crate::cache_backend_options(BACKEND, &options);
    }
    mark_signed_in(true, Some(auth_type))?;
    Ok(LoginDone {
        auth_type: auth_type.to_string(),
        models,
        options,
    })
}

pub async fn start_login(
    cfg: &Config,
    auth_type: Option<&str>,
    cwd: PathBuf,
) -> Result<LoginStart> {
    let method = auth_type
        .map(str::to_string)
        .unwrap_or_else(|| status().auth_type);
    set_auth_type(&method)?;
    if method == "gemini-api-key" && !has_gemini_api_key() {
        bail!("GEMINI_API_KEY が未設定です。/login google で API キーを保存するか環境変数を設定してください");
    }
    let (command, env) = ensure_installed(cfg).await?;
    let mut env = runtime_env(&env, true);
    endpoint_env(cfg.services_extra_opts.antigravity_endpoint, &mut env);
    let env_vec: Vec<(String, String)> = env.into_iter().collect();
    let (rpc, mut incoming, mut out_rx) = Rpc::spawn_with_output(&command, &env_vec, &cwd)?;
    let init = rpc
        .request(
            "initialize",
            json!({
                "protocolVersion": 1,
                "clientCapabilities": {"fs": {"readTextFile": true, "writeTextFile": true}},
            }),
        )
        .await?;
    let method_id = select_method_id(&init["authMethods"], &method).unwrap_or(method);
    let rpc_auth = rpc.clone();
    let auth_param = method_id.clone();
    let mut auth_task = tokio::spawn(async move {
        rpc_auth
            .request("authenticate", json!({"methodId": auth_param}))
            .await
    });
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    let mut out_open = true;
    let mut inc_open = true;
    loop {
        tokio::select! {
            res = &mut auth_task => {
                res.map_err(|e| anyhow!("認証タスクに失敗しました: {e}"))??;
                let done = complete_login(&rpc, &mut incoming, &method_id, &cwd).await?;
                return Ok(LoginStart::Ready(done));
            }
            maybe_line = out_rx.recv(), if out_open => {
                match maybe_line {
                    Some(line) => {
                        if let Some(url) = extract_auth_url(&line) {
                            return Ok(LoginStart::Browser(PendingLogin {
                                url,
                                auth_type: method_id,
                                rpc,
                                incoming,
                                _out_rx: out_rx,
                                auth_task: Some(auth_task),
                            }));
                        }
                    }
                    None => out_open = false,
                }
            }
            maybe_inc = incoming.recv(), if inc_open => {
                match maybe_inc {
                    Some(Incoming::Notification { params, .. }) => {
                        if let Some(url) = extract_auth_url(&params.to_string()) {
                            return Ok(LoginStart::Browser(PendingLogin {
                                url,
                                auth_type: method_id,
                                rpc,
                                incoming,
                                _out_rx: out_rx,
                                auth_task: Some(auth_task),
                            }));
                        }
                    }
                    Some(Incoming::Request { .. }) => {}
                    None => inc_open = false,
                }
            }
            _ = tokio::time::sleep_until(deadline) => {
                auth_task.abort();
                bail!("Google Antigravity の認証開始がタイムアウトしました");
            }
        }
    }
}

pub async fn finish_login(mut pending: PendingLogin, cwd: PathBuf) -> Result<LoginDone> {
    let mut task = pending
        .auth_task
        .take()
        .context("認証タスクが見つかりません")?;
    let res = tokio::select! {
        res = &mut task => res,
        _ = tokio::time::sleep(Duration::from_secs(600)) => {
            task.abort();
            bail!("Google Antigravity のサインインがタイムアウトしました");
        }
    };
    res.map_err(|e| anyhow!("認証タスクに失敗しました: {e}"))??;
    complete_login(&pending.rpc, &mut pending.incoming, &pending.auth_type, &cwd).await
}

pub fn logout() -> Result<bool> {
    logout_in(&crate::home(), &gemini_dir(), true)
}

pub fn logout_in(greencode_home: &Path, gemini_home: &Path, clear_keychain: bool) -> Result<bool> {
    let mut removed = false;
    let state_file = greencode_home.join("antigravity.json");
    if let Some(st) = read_stored_state(&state_file) {
        if st.signed_in {
            removed = true;
        }
    }
    match std::fs::remove_file(&state_file) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    crate::remove_backend_cache_entry(greencode_home, BACKEND);
    let token_file = gemini_home.join("antigravity-acp").join("acp_token.json");
    match std::fs::remove_file(&token_file) {
        Ok(()) => removed = true,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    if clear_keychain {
        #[cfg(target_os = "macos")]
        {
            if let Ok(status) = std::process::Command::new("/usr/bin/security")
                .args(["delete-generic-password", "-s", "gemini", "-a", "antigravity-acp"])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
            {
                if status.success() {
                    removed = true;
                }
            }
        }
    }
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_google_and_loopback_oauth_urls() {
        assert_eq!(
            extract_auth_url("Please visit: \"https://accounts.google.com/o/oauth2/v2/auth?client_id=abc&state=1\"."),
            Some("https://accounts.google.com/o/oauth2/v2/auth?client_id=abc&state=1".into())
        );
        assert_eq!(
            extract_auth_url("Open https://accounts.google.com/o/oauth2/auth?response_type=code&redirect_uri=http://127.0.0.1:9000/cb"),
            Some("https://accounts.google.com/o/oauth2/auth?response_type=code&redirect_uri=http://127.0.0.1:9000/cb".into())
        );
        assert_eq!(
            extract_auth_url("login at http://127.0.0.1:8080/auth?client_id=test&state=xyz"),
            Some("http://127.0.0.1:8080/auth?client_id=test&state=xyz".into())
        );
        assert_eq!(extract_auth_url("just some stderr log without url"), None);
    }

    #[test]
    fn selects_preferred_auth_method() {
        let methods = json!([
            {"id": "gemini-api-key"},
            {"id": "oauth-business"},
            {"id": "oauth-personal"}
        ]);
        assert_eq!(select_method_id(&methods, "oauth-business").as_deref(), Some("oauth-business"));
        assert_eq!(select_method_id(&methods, "missing").as_deref(), Some("oauth-personal"));
        assert_eq!(select_method_id(&json!([]), "oauth-personal"), None);
    }
}
