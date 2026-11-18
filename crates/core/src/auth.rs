//! Credentials GreenCode manages: API keys in auth.json and the Claude subscription file.

use crate::{config::{Config, ProviderCfg}, provider::anthropic_oauth};
use anyhow::{bail, Result};
use serde_json::{json, Map, Value};
use std::{collections::BTreeMap, path::PathBuf};

#[derive(Debug, Clone, PartialEq)]
pub enum Method {
    Subscription,
    StoredKey,
    ConfigKey,
    Env(String),
    Keyless,
    None(String),
}

impl Method {
    pub fn signed_in(&self) -> bool { !matches!(self, Method::None(_)) }

    pub fn label(&self) -> String {
        match self {
            Method::Subscription => "subscription".into(),
            Method::StoredKey => "API key (auth.json)".into(),
            Method::ConfigKey => "API key (config.toml)".into(),
            Method::Env(v) => v.clone(),
            Method::Keyless => "no key needed".into(),
            Method::None(v) if v.is_empty() => "not signed in".into(),
            Method::None(v) => format!("not signed in (set {v} or run auth login)"),
        }
    }
}

pub fn path() -> PathBuf { crate::home().join("auth.json") }

fn read() -> Result<Map<String, Value>> {
    let path = path();
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Map::new()),
        Err(_) => bail!("{} is not valid JSON; fix or delete it", path.display()),
    };
    match serde_json::from_str(&text) {
        Ok(map) => Ok(map),
        Err(_) => bail!("{} is not valid JSON; fix or delete it", path.display()),
    }
}

fn write(map: &Map<String, Value>) -> Result<()> {
    use std::io::Write;
    std::fs::create_dir_all(crate::home())?;
    let path = path();
    let tmp = path.with_file_name("auth.json.tmp");
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut opts, 0o600);
    let mut file = opts.open(&tmp)?;
    file.write_all(serde_json::to_string_pretty(map)?.as_bytes())?;
    file.sync_all()?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

/// Stored secrets by provider: (secret, is_oauth).
pub(crate) fn stored() -> BTreeMap<String, (String, bool)> {
    read().unwrap_or_default().into_iter().filter_map(|(name, v)| match v["type"].as_str()? {
        "api" => Some((name, (v["key"].as_str()?.to_string(), false))),
        "oauth" => Some((name, (v["access"].as_str()?.to_string(), true))),
        _ => None,
    }).collect()
}

pub fn stored_keys() -> BTreeMap<String, String> {
    stored().into_iter().map(|(name, (secret, _))| (name, secret)).collect()
}

pub fn status(name: &str, p: &ProviderCfg) -> Method {
    if p.kind == "codex" {
        return if crate::provider::codex::path().exists() { Method::Subscription } else { Method::None(String::new()) };
    }
    if p.api_key.is_some() {
        if p.stored && p.oauth { Method::Subscription } else if p.stored { Method::StoredKey } else { Method::ConfigKey }
    } else if crate::provider::oauth(name, p).is_some() {
        Method::Subscription
    } else if p.api_key_env.is_empty() {
        Method::Keyless
    } else if std::env::var_os(&p.api_key_env).is_some() {
        Method::Env(p.api_key_env.clone())
    } else {
        Method::None(p.api_key_env.clone())
    }
}

fn remove_subscription() -> Result<bool> {
    match std::fs::remove_file(anthropic_oauth::path()) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e.into()),
    }
}

pub fn set_key(cfg: &mut Config, name: &str, key: &str) -> Result<()> {
    store(cfg, name, key, false)
}

/// Stores an OAuth access token (used as the bearer credential) for `name`.
pub fn set_token(cfg: &mut Config, name: &str, token: &str) -> Result<()> {
    store(cfg, name, token, true)
}

fn store(cfg: &mut Config, name: &str, secret: &str, oauth: bool) -> Result<()> {
    let Some(p) = cfg.providers.get_mut(name) else { bail!("unknown provider {name}") };
    let secret = secret.trim();
    if secret.is_empty() { bail!(if oauth { "token is empty" } else { "API key is empty" }) }
    if p.api_key.is_some() && !p.stored { bail!("{name} has api_key in config.toml; edit that file instead") }
    let mut map = read()?;
    let mut entry = if oauth { json!({"type": "oauth", "access": secret}) } else { json!({"type": "api", "key": secret}) };
    // The new credential replaces the active one but keeps its account label and the saved accounts.
    if let Some(old) = map.get(name).and_then(Value::as_object) {
        for k in ["account", "accounts"] {
            if let Some(v) = old.get(k) { entry[k] = v.clone(); }
        }
    }
    map.insert(name.into(), entry);
    write(&map)?;
    p.api_key = Some(secret.into());
    p.stored = true;
    p.oauth = oauth;
    if name == "anthropic" { remove_subscription()?; }
    Ok(())
}

/// Removes the stored credential (keeping the account label and saved accounts unless `keep_label` is false
/// and nothing else is saved); returns whether a credential existed.
fn drop_entry(name: &str, keep_label: bool) -> Result<bool> {
    let mut map = read()?;
    let Some(entry) = map.get_mut(name).and_then(Value::as_object_mut) else { return Ok(false) };
    let had = entry.contains_key("type");
    for k in ["type", "key", "access"] { entry.remove(k); }
    if !keep_label { entry.remove("account"); }
    if entry.get("accounts").and_then(Value::as_object).is_some_and(Map::is_empty) { entry.remove("accounts"); }
    if entry.is_empty() { map.remove(name); }
    write(&map)?;
    Ok(had)
}

pub fn forget_key(cfg: &mut Config, name: &str) -> Result<()> {
    drop_entry(name, true)?;
    if let Some(p) = cfg.providers.get_mut(name).filter(|p| p.stored) {
        p.api_key = None;
        p.stored = false;
        p.oauth = false;
    }
    Ok(())
}

pub fn logout(cfg: &mut Config, name: &str) -> Result<()> {
    if crate::antigravity::is_alias(name) {
        if !crate::antigravity::logout()? {
            bail!("{name} is not signed in through GreenCode");
        }
        return Ok(());
    }
    let Some(p) = cfg.providers.get(name) else { bail!("unknown provider {name}") };
    if p.api_key.is_some() && !p.stored { bail!("{name}: api_key is set in config.toml; remove it there") }
    let p_is_codex = p.kind == "codex";
    let mut removed = drop_entry(name, false)?;
    if p.stored { removed = true; }
    forget_key(cfg, name)?;
    if name == "anthropic" { removed |= remove_subscription()?; }
    if p_is_codex {
        removed |= match std::fs::remove_file(crate::provider::codex::path()) {
            Ok(()) => true,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
            Err(e) => return Err(e.into()),
        };
    }
    if !removed { bail!("{name} is not signed in through GreenCode") }
    Ok(())
}

/// Signs in with a Claude subscription and switches `cfg` over to it immediately.
pub async fn claude_login(cfg: &mut Config, login: anthropic_oauth::Login, code: &str) -> Result<()> {
    if cfg.providers.get("anthropic").is_some_and(|p| p.api_key.is_some() && !p.stored) {
        bail!("anthropic has api_key in config.toml; remove it there to use a subscription");
    }
    login.exchange_and_save(code, &anthropic_oauth::path()).await?;
    forget_key(cfg, "anthropic")
}

// ---- Multiple accounts per provider ----
//
// The active credential stays where it always was (the top-level auth.json entry, or the subscription
// file) and the entry's `"account"` names it. Other accounts are `"accounts": {name: entry}` in the same
// entry, or `home/accounts/<provider>/<name>.json` for subscription files.

const DEFAULT_ACCOUNT: &str = "default";

/// The active subscription file of providers that sign in with one.
fn subscription_path(provider: &str) -> Option<PathBuf> {
    match provider {
        "anthropic" => Some(anthropic_oauth::path()),
        "chatgpt" => Some(crate::provider::codex::path()),
        _ => None,
    }
}

fn account_file(provider: &str, account: &str) -> PathBuf {
    crate::home().join("accounts").join(provider).join(format!("{account}.json"))
}

fn check_account_name(account: &str) -> Result<()> {
    if account.is_empty() || !account.chars().all(|c| c.is_alphanumeric() || c == '-' || c == '_') {
        bail!("account name must be letters, digits, '-' or '_'");
    }
    Ok(())
}

fn active_label(map: &Map<String, Value>, provider: &str) -> String {
    map.get(provider).and_then(|e| e["account"].as_str()).unwrap_or(DEFAULT_ACCOUNT).to_string()
}

fn signed_in(map: &Map<String, Value>, provider: &str) -> bool {
    map.get(provider).is_some_and(|e| e["type"].is_string()) || subscription_path(provider).is_some_and(|p| p.exists())
}

fn entry_mut<'a>(map: &'a mut Map<String, Value>, provider: &str) -> Result<&'a mut Map<String, Value>> {
    match map.entry(provider).or_insert_with(|| json!({})).as_object_mut() {
        Some(e) => Ok(e),
        None => bail!("{} entry for {provider} is not an object; fix or delete it", path().display()),
    }
}

fn copy_private(src: &std::path::Path, dst: &std::path::Path) -> Result<()> {
    use std::io::Write;
    std::fs::create_dir_all(dst.parent().expect("account path has a parent"))?;
    let tmp = dst.with_extension("json.tmp");
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut opts, 0o600);
    let mut file = opts.open(&tmp)?;
    file.write_all(&std::fs::read(src)?)?;
    file.sync_all()?;
    std::fs::rename(&tmp, dst)?;
    Ok(())
}

fn remove_if_exists(path: &std::path::Path) -> Result<()> {
    match std::fs::remove_file(path) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.into()),
        _ => Ok(()),
    }
}

/// Copies the active credential under `label`; false when nothing is signed in.
fn snapshot(map: &mut Map<String, Value>, provider: &str, label: &str) -> Result<bool> {
    if let Some(entry) = map.get_mut(provider).and_then(Value::as_object_mut).filter(|e| e.contains_key("type")) {
        let mut copy = entry.clone();
        copy.remove("account");
        copy.remove("accounts");
        let accounts = entry.entry("accounts").or_insert_with(|| json!({}));
        let Some(accounts) = accounts.as_object_mut() else { bail!("accounts of {provider} is not an object") };
        accounts.insert(label.into(), Value::Object(copy));
        return Ok(true);
    }
    match subscription_path(provider).filter(|p| p.exists()) {
        Some(src) => { copy_private(&src, &account_file(provider, label))?; Ok(true) }
        None => Ok(false),
    }
}

fn names(map: &Map<String, Value>, provider: &str) -> Vec<String> {
    let mut all: Vec<String> = map.get(provider).and_then(|e| e["accounts"].as_object()).map(|a| a.keys().cloned().collect()).unwrap_or_default();
    if let Ok(dir) = std::fs::read_dir(crate::home().join("accounts").join(provider)) {
        all.extend(dir.flatten().filter_map(|f| f.file_name().to_str()?.strip_suffix(".json").map(String::from)));
    }
    if signed_in(map, provider) { all.push(active_label(map, provider)); }
    all.sort();
    all.dedup();
    all
}

/// Active account name (if signed in) and every account name, sorted.
pub fn accounts(provider: &str) -> Result<(Option<String>, Vec<String>)> {
    let map = read()?;
    Ok((signed_in(&map, provider).then(|| active_label(&map, provider)), names(&map, provider)))
}

fn managed<'a>(cfg: &'a Config, provider: &str) -> Result<&'a ProviderCfg> {
    let Some(p) = cfg.providers.get(provider) else { bail!("unknown provider {provider}") };
    if p.api_key.is_some() && !p.stored { bail!("{provider}: api_key is set in config.toml; remove it there") }
    Ok(p)
}

/// Mirrors the stored credential of `provider` into the in-memory config.
fn sync_config(cfg: &mut Config, provider: &str) {
    let stored = stored();
    if let Some(p) = cfg.providers.get_mut(provider) {
        match stored.get(provider) {
            Some((secret, oauth)) => { p.api_key = Some(secret.clone()); p.stored = true; p.oauth = *oauth; }
            None => { p.api_key = None; p.stored = false; p.oauth = false; }
        }
    }
}

/// Saves the active credential of `provider` under `account` and makes that its name.
pub fn save_as(cfg: &mut Config, provider: &str, account: &str) -> Result<()> {
    managed(cfg, provider)?;
    check_account_name(account)?;
    let mut map = read()?;
    if !snapshot(&mut map, provider, account)? { bail!("{provider} is not signed in through GreenCode") }
    entry_mut(&mut map, provider)?.insert("account".into(), json!(account));
    write(&map)
}

/// Makes `account` the active credential of `provider`; the previous one is saved under its name first.
pub fn switch(cfg: &mut Config, provider: &str, account: &str) -> Result<()> {
    managed(cfg, provider)?;
    let mut map = read()?;
    if !names(&map, provider).iter().any(|n| n == account) { bail!("{provider} has no account {account}") }
    let label = active_label(&map, provider);
    if signed_in(&map, provider) && label == account { return Ok(()) }
    snapshot(&mut map, provider, &label)?;
    let entry = entry_mut(&mut map, provider)?;
    let saved = entry.get_mut("accounts").and_then(Value::as_object_mut).and_then(|a| a.remove(account));
    let mut new = Map::new();
    if let Some(Value::Object(credential)) = saved {
        new.extend(credential);
        if let Some(active) = subscription_path(provider) { remove_if_exists(&active)?; }
    } else {
        let active = subscription_path(provider).filter(|_| account_file(provider, account).exists());
        let Some(active) = active else { bail!("{provider} has no account {account}") };
        copy_private(&account_file(provider, account), &active)?;
    }
    new.insert("account".into(), json!(account));
    if let Some(rest) = entry.remove("accounts").filter(|a| a.as_object().is_some_and(|a| !a.is_empty())) { new.insert("accounts".into(), rest); }
    map.insert(provider.into(), Value::Object(new));
    write(&map)?;
    sync_config(cfg, provider);
    Ok(())
}

/// Deletes a saved account; the active one cannot be removed.
pub fn remove_account(provider: &str, account: &str) -> Result<()> {
    let mut map = read()?;
    if !names(&map, provider).iter().any(|n| n == account) { bail!("{provider} has no account {account}") }
    if signed_in(&map, provider) && active_label(&map, provider) == account { bail!("{account} is the active account of {provider}; switch first") }
    if let Some(a) = map.get_mut(provider).and_then(|e| e["accounts"].as_object_mut()) { a.remove(account); }
    remove_if_exists(&account_file(provider, account))?;
    let empty = map.get(provider).and_then(Value::as_object).is_some_and(|e| e.get("accounts").and_then(Value::as_object).is_none_or(Map::is_empty) && !e.contains_key("type") && !e.contains_key("account"));
    if empty { map.remove(provider); }
    write(&map)
}
