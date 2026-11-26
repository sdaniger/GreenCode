//! Secret references (`op://vault/item/field`) read through the 1Password CLI when a credential is used, and
//! values an agent asked the user for (`~/.greencode/secrets/<NAME>`, exported to `bash`).
//! `OP_SERVICE_ACCOUNT_TOKEN` and `OP_CONNECT_*` reach `op` through the inherited environment.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    io::Read,
    path::PathBuf,
    process::{Command, Stdio},
    sync::{LazyLock, Mutex},
    time::{Duration, Instant},
};

/// A configured secret provider (`[[secret_providers]]`); 1Password is the only kind.
#[derive(Deserialize, Serialize, Clone, Debug, Default, PartialEq)]
pub struct SecretProvider {
    pub name: String,
    /// `op account list` shorthand, sign-in address or ID. Ignored by `op` under a service account or Connect.
    pub account: String,
}

const TIMEOUT: Duration = Duration::from_secs(45);
static PROVIDERS: Mutex<Vec<SecretProvider>> = Mutex::new(Vec::new());
static CACHE: LazyLock<Mutex<HashMap<(String, String), String>>> = LazyLock::new(Default::default);

/// Remembers the configured providers; the first one's account is used for every read.
pub fn set_providers(list: &[SecretProvider]) {
    *PROVIDERS.lock().unwrap() = list.to_vec();
}

pub fn clear_cache() {
    CACHE.lock().unwrap().clear();
}

/// `op://vault/item/field` or `op://vault/item/section/field`.
pub fn is_ref(v: &str) -> bool {
    v.strip_prefix("op://").is_some_and(|rest| {
        let parts: Vec<&str> = rest.split('/').collect();
        (3..=4).contains(&parts.len()) && parts.iter().all(|p| !p.trim().is_empty())
    })
}

/// A plain value unchanged; a reference read from 1Password. `who` (provider or variable) is named in errors.
pub fn resolve(value: &str, who: &str) -> Result<String> {
    if !is_ref(value) {
        return Ok(value.to_string());
    }
    let account = PROVIDERS.lock().unwrap().first().map(|p| p.account.clone()).unwrap_or_default();
    let key = (account.clone(), value.to_string());
    if let Some(hit) = CACHE.lock().unwrap().get(&key) {
        return Ok(hit.clone());
    }
    let secret = read_op(value, &account, TIMEOUT).with_context(|| format!("1Password から読めません ({who})"))?;
    CACHE.lock().unwrap().insert(key, secret.clone());
    Ok(secret)
}

/// Reads `reference` now, ignoring and refreshing the cache (the settings test button).
pub fn test(reference: &str, who: &str) -> Result<()> {
    clear_cache();
    ensure_ref(reference)?;
    resolve(reference, who).map(drop)
}

fn ensure_ref(v: &str) -> Result<()> {
    if !is_ref(v) {
        bail!("op://vault/item/field の形式で入力してください")
    }
    Ok(())
}

fn read_op(reference: &str, account: &str, timeout: Duration) -> Result<String> {
    let mut cmd = Command::new("op");
    cmd.args(["read", "--no-newline"]);
    if !account.is_empty() {
        cmd.args(["--account", account]);
    }
    let mut child = cmd
        .arg(reference)
        .env("PATH", crate::config::agent_path()?)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("op コマンドを起動できません。1Password CLI をインストールしてください")?;
    let drain = |mut pipe: Box<dyn Read + Send>| {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = pipe.read_to_end(&mut buf);
            String::from_utf8_lossy(&buf).into_owned()
        })
    };
    let (out, err) = (drain(Box::new(child.stdout.take().unwrap())), drain(Box::new(child.stderr.take().unwrap())));
    let deadline = Instant::now() + timeout;
    let status = loop {
        if let Some(s) = child.try_wait()? {
            break s;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            bail!("{}秒以内に応答がありませんでした", timeout.as_secs())
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let (out, err) = (out.join().unwrap_or_default(), err.join().unwrap_or_default());
    if !status.success() {
        bail!("{}", err.trim().lines().last().unwrap_or("op read が失敗しました"))
    }
    Ok(out)
}

/// Writes `secret_providers` to the global config.toml, keeping its comments.
pub fn save_providers(list: &[SecretProvider]) -> Result<()> {
    let value = toml::Value::try_from(list)?.to_string();
    crate::config::settings::edit_global("secret_providers", Some(&[value]))?;
    Ok(())
}

/// Fields that hold a secret reference, for the removal warning.
pub fn dependents(cfg: &crate::config::Config) -> Vec<String> {
    let mut out = Vec::new();
    for (name, p) in &cfg.providers {
        if p.api_key.as_deref().is_some_and(is_ref) {
            out.push(format!("プロバイダー {name} の api_key"));
        }
    }
    for b in &cfg.backends {
        for (k, v) in &b.env {
            if is_ref(v) {
                out.push(format!("バックエンド {} の env {k}", b.name));
            }
        }
    }
    out
}

// ---- values the agent asked the user for ----

fn dir() -> PathBuf {
    crate::home().join("secrets")
}

pub fn valid_name(name: &str) -> bool {
    let mut c = name.chars();
    c.next().is_some_and(|f| f.is_ascii_alphabetic() || f == '_')
        && name.len() <= 64
        && c.all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
}

/// Saves `value` as `~/.greencode/secrets/<name>` (mode 600) and returns the path.
pub fn store(name: &str, value: &str) -> Result<PathBuf> {
    if !valid_name(name) {
        bail!("secret の name は英数字と _ のみ（先頭は英字か _）にしてください")
    }
    let dir = dir();
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(name);
    #[cfg(unix)]
    {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
        let _ = std::fs::remove_file(&path);
        let mut f = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&path)?;
        std::io::Write::write_all(&mut f, value.as_bytes())?;
    }
    #[cfg(not(unix))]
    std::fs::write(&path, value)?;
    Ok(path)
}

/// A masked dialog for one value; `None` when the user declined or no UI can ask.
pub async fn ask_value(tx: &crate::EventTx, title: &str, message: &str) -> Result<Option<String>> {
    let (reply, rx) = tokio::sync::oneshot::channel();
    let request = serde_json::json!({"method": "secret", "secret": true, "title": title, "message": message});
    tx.send(crate::Event::Dialog { request, reply }).map_err(|_| anyhow::anyhow!("no user can answer here"))?;
    let answer = rx.await.map_err(|_| anyhow::anyhow!("no user can answer here"))?;
    Ok(answer["value"].as_str().filter(|v| !v.is_empty() && answer["cancelled"] != true).map(str::to_string))
}

/// Asks for secret `name` and saves it; the stored file, or `None` when the user declined.
pub async fn ask(tx: &crate::EventTx, name: &str, reason: &str) -> Result<Option<PathBuf>> {
    if !valid_name(name) {
        bail!("secret name must be letters, digits and _ (not starting with a digit)")
    }
    match ask_value(tx, &format!("秘密情報を保存: {name}"), reason).await? {
        Some(value) => store(name, &value).map(Some),
        None => Ok(None),
    }
}

/// Every saved secret as `(NAME, value)`, exported to `bash` commands.
pub fn env() -> Vec<(String, String)> {
    let Ok(rd) = std::fs::read_dir(dir()) else { return Vec::new() };
    rd.flatten()
        .filter_map(|e| {
            let name = e.file_name().into_string().ok().filter(|n| valid_name(n))?;
            Some((name, std::fs::read_to_string(e.path()).ok()?))
        })
        .collect()
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    static LOCK: Mutex<()> = Mutex::new(());

    fn fake_op(script: &str) -> (tempfile::TempDir, std::ffi::OsString) {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("op");
        std::fs::write(&p, format!("#!/bin/sh\n{script}\n")).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        let old = std::env::var_os("PATH").unwrap_or_default();
        let mut paths = vec![d.path().to_path_buf()];
        paths.extend(std::env::split_paths(&old));
        std::env::set_var("PATH", std::env::join_paths(paths).unwrap());
        (d, old)
    }

    #[test]
    fn parses_references() {
        assert!(is_ref("op://Automation/provider-token/credential"));
        assert!(is_ref("op://v/i/section/f"));
        assert!(!is_ref("op://v/i"));
        assert!(!is_ref("sk-abc"));
        assert!(!is_ref("op://v//f"));
    }

    #[test]
    fn reads_caches_and_reports_errors() {
        let _g = LOCK.lock().unwrap();
        let counter = tempfile::tempdir().unwrap();
        let log = counter.path().join("calls");
        let (_d, old) = fake_op(&format!(
            "echo \"$@\" >> {}\ncase \"$*\" in *bad*) echo '[ERROR] item not found' >&2; exit 1;; esac\nprintf s3cret",
            log.display()
        ));
        set_providers(&[SecretProvider { name: "op".into(), account: "my.1password.com".into() }]);
        clear_cache();
        assert_eq!(resolve("op://v/i/f", "anthropic").unwrap(), "s3cret");
        assert_eq!(resolve("op://v/i/f", "anthropic").unwrap(), "s3cret");
        let calls = std::fs::read_to_string(&log).unwrap();
        assert_eq!(calls.lines().count(), 1, "second read is cached");
        assert_eq!(calls.trim(), "read --no-newline --account my.1password.com op://v/i/f");
        assert_eq!(resolve("plain", "x").unwrap(), "plain");
        let e = format!("{:#}", resolve("op://v/bad/f", "openai").unwrap_err());
        assert!(e.contains("1Password から読めません (openai)") && e.contains("item not found"), "{e}");
        // A provider's configured api_key resolves through the same path, so errors name it.
        let cfg = crate::config::ProviderCfg { api_key: Some("op://v/bad/f".into()), ..Default::default() };
        let e = format!("{:#}", crate::provider::key("openai", &cfg).unwrap_err());
        assert!(e.contains("1Password から読めません (openai)"), "{e}");
        set_providers(&[]);
        std::env::set_var("PATH", old);
    }

    #[test]
    fn times_out() {
        let _g = LOCK.lock().unwrap();
        let (_d, old) = fake_op("exec sleep 5");
        let t = Instant::now();
        let e = read_op("op://v/i/f", "", Duration::from_millis(300)).unwrap_err();
        assert!(t.elapsed() < Duration::from_secs(3) && e.to_string().contains("応答がありません"), "{e}");
        std::env::set_var("PATH", old);
    }

    #[test]
    fn stores_private_file() {
        let _g = LOCK.lock().unwrap();
        let home = tempfile::tempdir().unwrap();
        crate::set_test_home(Some(home.path()));
        let path = store("API_TOKEN", "hunter2").unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(env(), vec![("API_TOKEN".to_string(), "hunter2".to_string())]);
        assert!(store("bad name", "x").is_err());
        let list = vec![SecretProvider { name: "op".into(), account: "me.1password.com".into() }];
        save_providers(&list).unwrap();
        let mut cfg = crate::config::Config::load().unwrap();
        assert_eq!(cfg.secret_providers, list);
        cfg.providers.insert("p".into(), crate::config::ProviderCfg { api_key: Some("op://v/i/f".into()), ..Default::default() });
        assert_eq!(dependents(&cfg), vec!["プロバイダー p の api_key".to_string()]);
        crate::set_test_home(None);
    }
}

/// Obfuscation for requests leaving for a model provider (`secrets.enabled`): configured secrets
/// become stable `$$SECRET_<n>$$` placeholders and credential-shaped tokens are redacted, then
/// restored in the streamed reply and tool inputs so the model's own echo keeps working.
pub struct Obfuscator {
    pairs: Vec<(String, String)>,
    restore: HashMap<String, String>,
}

impl Obfuscator {
    /// Every configured credential: provider keys, auth.json entries and saved secret files.
    pub fn from_config(cfg: &crate::config::Config) -> Obfuscator {
        let mut values: Vec<String> = Vec::new();
        for (name, p) in &cfg.providers {
            if let Some(key) = &p.api_key {
                if crate::secrets::is_ref(key) {
                    if let Ok(v) = crate::secrets::resolve(key, name) {
                        values.push(v);
                    }
                } else {
                    values.push(key.clone());
                }
            }
            if !p.api_key_env.is_empty() {
                if let Ok(v) = std::env::var(&p.api_key_env) {
                    values.push(v);
                }
            }
        }
        values.extend(crate::auth::stored_keys().into_values());
        values.extend(crate::secrets::env().into_iter().map(|(_, v)| v));
        values.retain(|v| v.trim().len() >= 8);
        values.sort_by_key(|v| std::cmp::Reverse(v.len()));
        values.dedup();
        let mut restore = HashMap::new();
        let pairs = values
            .into_iter()
            .enumerate()
            .map(|(i, value)| {
                let placeholder = format!("$$SECRET_{i}$$");
                restore.insert(placeholder.clone(), value.clone());
                (value, placeholder)
            })
            .collect();
        Obfuscator { pairs, restore }
    }

    /// Replaces configured secrets, then credential-shaped tokens, in `text`.
    pub fn obfuscate(&self, text: &str) -> String {
        let mut out = text.to_string();
        for (value, placeholder) in &self.pairs {
            if out.contains(value.as_str()) {
                out = out.replace(value.as_str(), placeholder);
            }
        }
        redact_shapes(&out)
    }

    /// Restores anything `obfuscate` changed; unknown text is unchanged.
    pub fn deobfuscate(&self, text: &str) -> String {
        let mut out = text.to_string();
        for (placeholder, value) in &self.restore {
            if out.contains(placeholder.as_str()) {
                out = out.replace(placeholder.as_str(), value);
            }
        }
        out
    }

    /// Rewrites every text, tool result and tool input in `messages` in place.
    pub fn obfuscate_messages(&self, messages: &mut [crate::Message]) {
        for message in messages {
            for block in &mut message.content {
                match block {
                    crate::Block::Text { text } => *text = self.obfuscate(text),
                    crate::Block::ToolResult { content, .. } => *content = self.obfuscate(content),
                    crate::Block::ToolUse { input, .. } => *input = self.obfuscate_value(input),
                    crate::Block::Other(v) => *v = self.obfuscate_value(v),
                }
            }
        }
    }

    fn obfuscate_value(&self, value: &serde_json::Value) -> serde_json::Value {
        match value {
            serde_json::Value::String(s) => serde_json::Value::String(self.obfuscate(s)),
            serde_json::Value::Array(list) => list.iter().map(|v| self.obfuscate_value(v)).collect(),
            serde_json::Value::Object(map) => map.iter().map(|(k, v)| (k.clone(), self.obfuscate_value(v))).collect(),
            other => other.clone(),
        }
    }

    /// Restores a reply's text, tool arguments and thinking blocks.
    pub fn deobfuscate_reply(&self, reply: &mut crate::provider::Reply) {
        for block in &mut reply.blocks {
            match block {
                crate::Block::Text { text } => *text = self.deobfuscate(text),
                crate::Block::ToolUse { input, .. } => *input = self.deobfuscate_value(input),
                crate::Block::Other(v) => *v = self.deobfuscate_value(v),
                crate::Block::ToolResult { content, .. } => *content = self.deobfuscate(content),
            }
        }
    }

    fn deobfuscate_value(&self, value: &serde_json::Value) -> serde_json::Value {
        match value {
            serde_json::Value::String(s) => serde_json::Value::String(self.deobfuscate(s)),
            serde_json::Value::Array(list) => list.iter().map(|v| self.deobfuscate_value(v)).collect(),
            serde_json::Value::Object(map) => map.iter().map(|(k, v)| (k.clone(), self.deobfuscate_value(v))).collect(),
            other => other.clone(),
        }
    }
}

/// Credential shapes that are not configured secrets: provider keys, bearer tokens, JWTs and
/// `KEY=value` assignments. Each hit becomes a stable `$$REDACTED_<hash>$$` placeholder.
fn redact_shapes(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for token in text.split_inclusive(char::is_whitespace) {
        let (body, tail) = match token.trim_end().len() {
            n if n < token.len() => token.split_at(n),
            _ => (token, ""),
        };
        let trimmed = body.trim_start_matches(['"', '\'', '(', '`', '=']);
        let prefix = &body[..body.len() - trimmed.len()];
        let value = trimmed.trim_end_matches([',', ';', ')', '"', '\'', '`']);
        if credential_shaped(value) {
            out.push_str(prefix);
            out.push_str(&format!("$$REDACTED_{:016x}$$", fnv(value.as_bytes())));
            out.push_str(&body[prefix.len() + value.len()..]);
        } else {
            out.push_str(body);
        }
        out.push_str(tail);
    }
    out
}

fn credential_shaped(value: &str) -> bool {
    let v = value.trim();
    let body = v.split_once('=').map_or(v, |(_, rest)| rest);
    let body = body.trim_matches(['"', '\'']);
    if body.starts_with("sk-") || body.starts_with("ghp_") || body.starts_with("github_pat_") || body.starts_with("xoxb-") {
        return body.len() >= 20;
    }
    if body.starts_with("AKIA") && body.len() == 20 {
        return true;
    }
    let jwt_like = body.split('.').count() == 3 && body.len() >= 60 && body.starts_with("eyJ");
    jwt_like || (v.starts_with("Bearer ") && body.len() >= 20)
}

fn fnv(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf29ce484222325u64, |hash, b| (hash ^ *b as u64).wrapping_mul(0x100000001b3))
}

/// A streamed reply may split a placeholder across chunks; this holds back a partial tail until
/// the placeholder closes (or the turn ends).
#[derive(Default)]
pub struct StreamDeobfuscator {
    tail: String,
}

impl StreamDeobfuscator {
    /// Text safe to show now, with placeholders restored.
    pub fn push(&mut self, chunk: &str, obf: &Obfuscator) -> String {
        self.tail.push_str(chunk);
        // Hold from the first unclosed `$$`, or a trailing `$` that may start one.
        let mut scan = 0;
        let mut hold = None;
        while let Some(at) = self.tail[scan..].find("$$") {
            let open = scan + at;
            if let Some(close) = self.tail[open + 2..].find("$$") {
                scan = open + 2 + close + 2;
            } else {
                hold = Some(open);
                break;
            }
        }
        let trailing_dollar = self.tail.ends_with('$') && !self.tail.ends_with("$$");
        let cut = hold.unwrap_or(self.tail.len() - usize::from(trailing_dollar));
        let head = self.tail[..cut].to_string();
        self.tail = self.tail[cut..].to_string();
        obf.deobfuscate(&head)
    }

    /// Flushes the held tail at the end of the stream.
    pub fn finish(&mut self, obf: &Obfuscator) -> String {
        obf.deobfuscate(&std::mem::take(&mut self.tail))
    }
}

#[cfg(test)]
mod obfuscation_tests {
    use super::*;

    fn obf() -> Obfuscator {
        Obfuscator { pairs: vec![("sk-config-secret-123456".into(), "$$SECRET_0$$".into())], restore: HashMap::from([("$$SECRET_0$$".into(), "sk-config-secret-123456".into())]) }
    }

    #[test]
    fn configured_secrets_become_placeholders_and_come_back() {
        let o = obf();
        let text = o.obfuscate("key=sk-config-secret-123456 and done");
        assert_eq!(text, "key=$$SECRET_0$$ and done");
        assert_eq!(o.deobfuscate(&text), "key=sk-config-secret-123456 and done");
        assert_eq!(o.deobfuscate("nothing"), "nothing");
    }

    #[test]
    fn credential_shapes_are_redacted_without_a_config() {
        let o = Obfuscator { pairs: vec![], restore: HashMap::new() };
        let out = o.obfuscate("use sk-abcdefghijklmnopqrstuvwx now; Authorization: Bearer abcdefghijklmnopqrstuvwxyz");
        assert!(!out.contains("sk-abcdefghijklmnopqrstuvwx"), "{out}");
        assert!(out.contains("$$REDACTED_"), "{out}");
        assert!(o.obfuscate("normal words stay").contains("normal words stay"));
    }

    #[test]
    fn messages_and_replies_round_trip() {
        let o = obf();
        let mut messages = vec![crate::Message::user("token sk-config-secret-123456")];
        o.obfuscate_messages(&mut messages);
        assert!(!serde_json::to_string(&messages).unwrap().contains("sk-config-secret-123456"));
        let mut reply = crate::provider::Reply {
            blocks: vec![crate::Block::Text { text: "echo $$SECRET_0$$".into() }],
            ..Default::default()
        };
        o.deobfuscate_reply(&mut reply);
        assert!(matches!(&reply.blocks[0], crate::Block::Text { text } if text == "echo sk-config-secret-123456"));
    }

    #[test]
    fn streaming_restores_a_placeholder_split_across_chunks() {
        let o = obf();
        let mut d = StreamDeobfuscator::default();
        let mut out = String::new();
        for chunk in ["the key is $$SEC", "RET_", "0$$ ok"] {
            out.push_str(&d.push(chunk, &o));
        }
        out.push_str(&d.finish(&o));
        assert_eq!(out, "the key is sk-config-secret-123456 ok");
        let mut plain = StreamDeobfuscator::default();
        assert_eq!(plain.push("hello", &o) + &plain.finish(&o), "hello");
    }

    #[test]
    fn from_config_skips_short_values_and_keeps_longest_first() {
        let mut cfg = crate::config::Config::default();
        cfg.providers.insert("p".into(), crate::config::ProviderCfg { api_key: Some("short".into()), ..Default::default() });
        let o = Obfuscator::from_config(&cfg);
        assert!(o.pairs.is_empty());
    }
}
