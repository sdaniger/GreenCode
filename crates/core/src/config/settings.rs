//! Layered config files and the `config path|list|get|set|reset` commands.
use super::Config;
use anyhow::{Context, Result, anyhow, bail, ensure};
use serde_json::Value as Json;
use std::path::{Path, PathBuf};

pub(crate) mod omp;
pub use omp::{ui_settings, Native, UiSetting, UI_TABS};
#[cfg(test)]
pub(crate) use omp::translate;
#[cfg(test)]
pub(crate) use omp::translate as translate_omp;

const REDACTED: &str = "<redacted>";
const PROVIDER_FIELDS: &[&str] = &["kind", "base_url", "api_key_env", "api_key", "models"];
/// Maps whose values are scalars or arrays, so everything after the map name is one key (`prices.openai/gpt-4.1`).
const FLAT_MAPS: &[&str] = &["model_roles", "model_compact_at", "prices", "task.agent_model_overrides", "system_prompt_templates", "max_in_flight", "retry.fallback_chains"];

pub fn describe(global: &Path, overlays: &[PathBuf]) -> String {
    std::iter::once(global).chain(overlays.iter().map(PathBuf::as_path)).map(|p| p.display().to_string()).collect::<Vec<_>>().join(" + ")
}

fn read_table(path: &Path, required: bool) -> Result<toml::Value> {
    let text = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) if !required && e.kind() == std::io::ErrorKind::NotFound => return Ok(toml::Value::Table(Default::default())),
        Err(e) => return Err(anyhow!("{}: {e}", path.display())),
    };
    toml::from_str(&text).map_err(|e: toml::de::Error| anyhow!("{}: {}", path.display(), e.message()))
}

pub(crate) fn decode(value: toml::Value) -> Result<Config> {
    let cfg: Config = value.try_into().map_err(|e: toml::de::Error| anyhow!("{}", e.message()))?;
    cfg.task.validate().map_err(|e| anyhow!("{e}"))?;
    cfg.assist_opts.advisor.validate().map_err(|e| anyhow!("{e}"))?;
    cfg.assist_opts.prewalk.validate().map_err(|e| anyhow!("{e}"))?;
    cfg.loop_opts.validate()?;
    for selector in cfg.model_compact_at.keys() {
        ensure!(selector.split_once('/').is_some_and(|(provider, model)| !provider.is_empty() && !model.is_empty())
            && !selector.strip_suffix('*').unwrap_or(selector).contains('*')
            && !selector.chars().any(char::is_whitespace), "invalid model_compact_at selector `{selector}`");
    }
    ensure!(cfg.context_files_extra.iter().all(|name| crate::context::valid_extra_name(name)),
        "context_files_extra must contain only non-built-in filenames, not paths");
    ensure!((3..=20).contains(&cfg.tui.autocomplete_max_visible), "tui.autocomplete_max_visible must be 3..=20");
    cfg.services_opts.validate()?;
    cfg.services_extra_opts.validate()?;
    Ok(cfg)
}

fn merge(base: &mut toml::Value, over: toml::Value) {
    match (base, over) {
        (toml::Value::Table(b), toml::Value::Table(o)) => {
            for (k, v) in o {
                match b.get_mut(&k) {
                    Some(slot) => merge(slot, v),
                    None => { b.insert(k, v); }
                }
            }
        }
        (slot, v) => *slot = v,
    }
}

/// The global file with each overlay merged over it; tables merge recursively, everything else (arrays too) is replaced.
pub fn merged(global: &Path, overlays: &[PathBuf]) -> Result<toml::Value> {
    let mut value = read_table(global, false)?;
    for o in overlays { merge(&mut value, read_table(o, true)?); }
    Ok(value)
}

fn is_secret_key(k: &str) -> bool {
    matches!(k, "api_key" | "password" | "token" | "secret")
        || ["_api_key", "_password", "_token", "_secret"].iter().any(|suffix| k.ends_with(suffix))
}

/// Masks credentials, provider URLs (they can embed them) and backend env values anywhere in the tree.
fn redact(v: &mut Json, path: &[String]) {
    match v {
        Json::Object(o) => {
            for (k, child) in o.iter_mut() {
                let mut p = path.to_vec();
                p.push(k.clone());
                let in_env = path.len() >= 2 && path[0] == "backends" && path[path.len() - 1] == "env";
                let provider_url = path.len() == 2 && path[0] == "providers" && k == "base_url";
                if (in_env || provider_url || is_secret_key(k)) && !child.is_null() && child.as_str() != Some("") {
                    *child = Json::String(REDACTED.into());
                } else {
                    redact(child, &p);
                }
            }
        }
        Json::Array(a) => {
            for (i, child) in a.iter_mut().enumerate() {
                let mut p = path.to_vec();
                p.push(i.to_string());
                redact(child, &p);
            }
        }
        _ => {}
    }
}

fn normalize(key: &str) -> Vec<String> {
    for map in FLAT_MAPS {
        if let Some(tail) = key.strip_prefix(map).and_then(|s| s.strip_prefix('.')) {
            return map.split('.').chain(std::iter::once(tail)).map(String::from).collect();
        }
    }
    key.split('.').map(String::from).collect()
}

/// Rejects keys Config does not know; dynamic maps (empty by default) accept any entry name.
fn check_key(segs: &[String]) -> Result<()> {
    ensure!(segs.iter().all(|s| !s.is_empty()), "empty key segment");
    let defaults = serde_json::to_value(Config::default())?;
    let mut node = &defaults;
    for (i, seg) in segs.iter().enumerate() {
        let here = segs[..=i].join(".");
        match node {
            Json::Object(o) if o.contains_key(seg) => node = &o[seg],
            Json::Object(o) if o.is_empty() => {
                if segs[0] == "providers" {
                    ensure!(matches!(segs.len(), 2 | 3) && segs.get(2).is_none_or(|field| PROVIDER_FIELDS.contains(&field.as_str())), "unknown setting `{}`", segs.join("."));
                } else {
                    ensure!(FLAT_MAPS.contains(&segs[..i].join(".").as_str()) && i + 1 == segs.len(), "unknown setting `{here}`");
                }
                return Ok(());
            }
            Json::Object(_) => bail!("unknown setting `{here}`"),
            Json::Null => bail!("`{}` is not a table; it has no `{seg}`", segs[..i].join(".")),
            _ => bail!("`{}` is not a table; it has no `{seg}`", segs[..i].join(".")),
        }
    }
    Ok(())
}

fn lookup<'a>(v: &'a Json, segs: &[String]) -> Option<&'a Json> {
    let Some((first, rest)) = segs.split_first() else { return Some(v) };
    match v {
        Json::Object(o) => o.get(first).and_then(|c| lookup(c, rest)),
        Json::Array(a) => first.parse::<usize>().ok().and_then(|i| a.get(i)).and_then(|c| lookup(c, rest)),
        _ => None,
    }
}

/// One leaf of the effective config, as `config list` shows it.
pub struct Row {
    pub key: String,
    pub value: Json,
    /// Set in a config file rather than a built-in default.
    pub set: bool,
    /// `config set` accepts it (credentials and backend environments are edited elsewhere).
    pub writable: bool,
}

/// The global config's leaves with credentials redacted, for settings UIs.
pub fn rows() -> Result<Vec<Row>> {
    let global = crate::home().join("config.toml");
    let merged = merged(&global, &[])?;
    let raw = serde_json::to_value(&merged)?;
    let mut tree = serde_json::to_value(decode(merged).with_context(|| global.display().to_string())?)?;
    redact(&mut tree, &[]);
    let mut out = vec![];
    flatten("", &tree, &raw, &mut out);
    Ok(out)
}

fn flatten(prefix: &str, v: &Json, set: &Json, out: &mut Vec<Row>) {
    match v {
        Json::Object(o) if !o.is_empty() => {
            for (k, c) in o { flatten(&if prefix.is_empty() { k.clone() } else { format!("{prefix}.{k}") }, c, set.get(k).unwrap_or(&Json::Null), out); }
        }
        _ => out.push(Row {
            writable: check_writable(&normalize(prefix)).is_ok(),
            key: prefix.to_string(),
            value: v.clone(),
            set: !set.is_null(),
        }),
    }
}

fn show(v: &Json) -> String { v.as_str().map(String::from).unwrap_or_else(|| v.to_string()) }

fn policy_lock(key: &str) -> Result<()> {
    if let Some(p) = crate::policy::load()?.as_ref().map(|p| &p.settings) {
        let locked = match key { "mode" => p.default_mode.as_ref().is_some_and(|s| s.locked), "sandbox" => p.sandbox.as_ref().is_some_and(|s| s.locked), _ => false };
        ensure!(!locked, "`{key}` is locked by organization policy");
    }
    Ok(())
}

fn check_writable(segs: &[String]) -> Result<()> {
    if segs[0] == "backends" || (segs[0] == "providers" && segs.len() != 3) || segs.iter().any(|s| is_secret_key(s)) {
        bail!("credentials and backend environments are not set here; use `greencode-cli auth login <provider>` or the provider settings");
    }
    Ok(())
}

fn to_edit(v: &toml::Value) -> Result<toml_edit::Item> { Ok(toml_edit::Item::Value(v.to_string().parse::<toml_edit::Value>()?)) }

/// Sets `key` in config text, keeping comments and layout. The first value reading that makes a valid Config wins.
pub fn set_text(text: &str, key: &str, values: &[String]) -> Result<String> {
    let segs = normalize(key);
    check_key(&segs)?;
    check_writable(&segs)?;
    ensure!(!values.is_empty(), "missing value for `{key}`");
    let parse = |value: &str| {
        let mut t = toml::from_str::<toml::Table>(&format!("v = {value}")).ok()?;
        if t.len() == 1 { t.remove("v") } else { None }
    };
    let mut candidates = Vec::with_capacity(3);
    if values.len() == 1 {
        candidates.extend(parse(&values[0]));
        candidates.push(toml::Value::String(values[0].clone()));
    } else {
        candidates.extend(parse(&format!("[{}]", values.join(", "))));
    }
    candidates.push(toml::Value::Array(values.iter().cloned().map(toml::Value::String).collect()));
    let mut first_err = None;
    for c in candidates {
        let mut doc: toml_edit::DocumentMut = text.parse().context("config.toml is not valid TOML")?;
        let mut table = doc.as_table_mut() as &mut dyn toml_edit::TableLike;
        for seg in &segs[..segs.len() - 1] {
            if !table.contains_key(seg) {
                let mut t = toml_edit::Table::new();
                t.set_implicit(true);
                table.insert(seg, toml_edit::Item::Table(t));
            }
            table = table.get_mut(seg).and_then(|i| i.as_table_like_mut()).ok_or_else(|| anyhow!("`{seg}` is not a table in config.toml"))?;
        }
        let last = &segs[segs.len() - 1];
        let mut item = to_edit(&c)?;
        if let Some(old) = table.get_mut(last) {
            if let (Some(value), Some(new)) = (old.as_value(), item.as_value_mut()) { *new.decor_mut() = value.decor().clone(); }
            *old = item;
        } else {
            table.insert(last, item);
        }
        let out = doc.to_string();
        match decode(toml::from_str::<toml::Value>(&out).map_err(|e| anyhow!("{}", e.message()))?) {
            Ok(_) => return Ok(out),
            Err(e) => { first_err.get_or_insert(e); }
        }
    }
    Err(anyhow!("invalid value for `{key}`: {}", first_err.map(|e| e.to_string()).unwrap_or_default()))
}

/// Removes `key` (and tables it leaves empty); `None` when it was not set.
pub fn reset_text(text: &str, key: &str) -> Result<Option<String>> {
    let segs = normalize(key);
    check_key(&segs)?;
    check_writable(&segs)?;
    let mut doc: toml_edit::DocumentMut = text.parse().context("config.toml is not valid TOML")?;
    fn go(t: &mut dyn toml_edit::TableLike, segs: &[String]) -> bool {
        if segs.len() == 1 { return t.remove(&segs[0]).is_some() }
        let Some(child) = t.get_mut(&segs[0]).and_then(|i| i.as_table_like_mut()) else { return false };
        let removed = go(child, &segs[1..]);
        if removed && child.is_empty() { t.remove(&segs[0]); }
        removed
    }
    Ok(go(doc.as_table_mut(), &segs).then(|| doc.to_string()))
}

/// Replaces the file atomically, keeping its permissions and following a symlink to the real file.
pub(crate) fn write_atomic(path: &Path, text: &str) -> Result<()> {
    let path = if path.is_symlink() { std::fs::canonicalize(path)? } else { path.to_owned() };
    if let Some(dir) = path.parent() { std::fs::create_dir_all(dir)?; }
    let tmp = path.with_extension(format!("toml.tmp{}", std::process::id()));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let mut file = options.open(&tmp).with_context(|| tmp.display().to_string())?;
    let result = (|| -> Result<()> {
        use std::io::Write;
        file.write_all(text.as_bytes())?;
        if let Ok(meta) = std::fs::metadata(&path) { file.set_permissions(meta.permissions())?; }
        file.sync_all()?;
        std::fs::rename(&tmp, &path)?;
        Ok(())
    })();
    if result.is_err() { let _ = std::fs::remove_file(&tmp); }
    result.with_context(|| path.display().to_string())
}

/// Update the user's config without dropping comments or exposing credentials.
/// With no values, reset the key instead. Returns whether a file was changed.
pub fn edit_global(key: &str, values: Option<&[String]>) -> Result<bool> {
    policy_lock(&normalize(key)[0])?;
    let path = crate::home().join("config.toml");
    let text = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => bail!("{}: {e}", path.display()),
    };
    let out = match values {
        Some(values) => Some(set_text(&text, key, values)?),
        None => reset_text(&text, key)?,
    };
    if let Some(out) = out {
        write_atomic(&path, &out)?;
        return Ok(true);
    }
    Ok(false)
}

pub fn cli(args: &[String]) -> Result<()> {
    let (mut pos, mut overlays, mut json) = (vec![], vec![], false);
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--json" => json = true,
            "--config" => overlays.push(PathBuf::from(it.next().context("--config needs a file")?)),
            s if s.starts_with("--config=") => overlays.push(PathBuf::from(&s["--config=".len()..])),
            _ => pos.push(a.clone()),
        }
    }
    let global = crate::home().join("config.toml");
    let action = pos.first().map(String::as_str).unwrap_or("list");
    let read_only = matches!(action, "list" | "get" | "path");
    ensure!(read_only || overlays.is_empty(), "--config applies to list and get");
    match action {
        "path" => { ensure!(pos.len() == 1 && overlays.is_empty(), "usage: config path"); println!("{}", global.display()); }
        "import-omp" => {
            let usage = "usage: config import-omp FILE [FILE...] | --profile NAME";
            ensure!(pos.len() >= 2 && !json, "{usage}");
            let out = if pos[1] == "--profile" {
                ensure!(pos.len() == 3, "{usage}");
                toml::Value::Table(omp::profile(&pos[2])?)
            } else {
                let mut out = toml::Value::Table(toml::Table::new());
                for path in &pos[1..] {
                    let text = std::fs::read_to_string(path).with_context(|| format!("Cannot read {path}"))?;
                    let layer = omp::translate(&text).with_context(|| format!("Cannot import {path}"))?;
                    merge(&mut out, toml::Value::Table(layer));
                }
                out
            };
            ensure!(out.as_table().is_some_and(|t| !t.is_empty()), "No supported OMP settings or keymaps found");
            let text = toml::to_string_pretty(&out)?;
            decode(out).context("Invalid merged OMP settings")?;
            print!("{text}");
        }
        "list" | "get" => {
            let merged = merged(&global, &overlays)?;
            let raw = serde_json::to_value(&merged)?;
            let cfg = decode(merged).with_context(|| format!("merged configuration ({})", describe(&global, &overlays)))?;
            let mut tree = serde_json::to_value(&cfg)?;
            redact(&mut tree, &[]);
            if action == "list" {
                ensure!(pos.len() <= 1, "list takes no key");
                if json { println!("{}", serde_json::to_string_pretty(&tree)?) } else {
                    let mut rows = vec![];
                    flatten("", &tree, &raw, &mut rows);
                    let lines: Vec<_> = rows.iter().map(|r| format!("{} = {}{}", r.key, r.value, if r.set { "" } else { "  (default)" })).collect();
                    println!("{}", lines.join("\n"));
                }
            } else {
                ensure!(pos.len() == 2, "usage: config get KEY");
                let segs = normalize(&pos[1]);
                check_key(&segs)?;
                let v = lookup(&tree, &segs).cloned().unwrap_or(Json::Null);
                println!("{}", if json { serde_json::to_string_pretty(&v)? } else { show(&v) });
            }
        }
        "set" | "reset" => {
            ensure!(pos.len() >= 2, "usage: config {action} KEY{}", if action == "set" { " VALUE..." } else { "" });
            ensure!(action == "set" || pos.len() == 2, "usage: config reset KEY");
            if action == "set" {
                edit_global(&pos[1], Some(&pos[2..]))?;
                let mut tree = serde_json::to_value(decode(merged(&global, &[])?)?)?;
                redact(&mut tree, &[]);
                println!("{} = {}", pos[1], lookup(&tree, &normalize(&pos[1])).map(show).unwrap_or_default());
            } else if edit_global(&pos[1], None)? {
                println!("{} reset to default", pos[1]);
            } else {
                println!("{} is already the default", pos[1]);
            }
        }
        other => bail!("unknown config action `{other}` (path|list|get|set|reset|import-omp)"),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str, text: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("gc-settings-{}-{name}", std::process::id()));
        std::fs::write(&p, text).unwrap();
        p
    }

    #[test]
    fn overlays_merge_last_wins_and_name_bad_files() {
        let g = tmp("g.toml", "max_tokens = 1\n[tui]\nhide_tools = true\n[providers.local]\nkind = \"openai\"\nbase_url = \"http://localhost:8080\"\n");
        let a = tmp("a.toml", "max_tokens = 2\n[tui]\nshow_usage = false\n[providers.local]\nbase_url = \"http://localhost:8081\"\n");
        let b = tmp("b.toml", "max_tokens = 3\n");
        let cfg: Config = merged(&g, &[a.clone(), b]).unwrap().try_into().unwrap();
        assert_eq!(cfg.max_tokens, 3);
        assert!(cfg.tui.hide_tools && !cfg.tui.show_usage);
        assert_eq!(cfg.providers["local"].kind, "openai");
        assert_eq!(cfg.providers["local"].base_url, "http://localhost:8081");
        let bad = tmp("bad.toml", "max_tokens = [\n");
        assert!(merged(&g, &[a, bad.clone()]).unwrap_err().to_string().contains(&bad.display().to_string()));
        assert!(merged(&g, &[PathBuf::from("/nonexistent/x.toml")]).is_err());
    }

    #[test]
    fn set_reset_keep_comments_and_reject_unknown_or_credentials() {
        let text = "# mine\nmax_tokens = 5 # keep\n";
        let out = set_text(text, "max_tokens", &["9".into()]).unwrap();
        assert_eq!(out, "# mine\nmax_tokens = 9 # keep\n");
        assert!(set_text(&out, "tui.follow_up", &["steer".into()]).unwrap().contains("follow_up = \"steer\""));
        assert!(set_text(&out, "tui.follow_up", &["nope".into()]).is_err());
        assert!(set_text(&out, "nonsense", &["1".into()]).is_err());
        assert!(set_text(&out, "providers.x.api_key", &["k".into()]).is_err());
        assert!(set_text(&out, "providers.x.bogus", &["k".into()]).is_err());
        assert!(set_text(&out, "prices.openai/gpt-4.1", &["1".into(), "2".into()]).unwrap().contains("[prices]"));
        let two = set_text(&out, "tui.hide_tools", &["true".into()]).unwrap();
        assert_eq!(reset_text(&two, "tui.hide_tools").unwrap().unwrap(), out);
        assert_eq!(reset_text(&out, "tui.hide_tools").unwrap(), None);
    }

    #[test]
    fn secrets_are_redacted_from_every_ancestor() {
        let g = tmp("s.toml", "[providers.x]\nkind=\"openai\"\nbase_url=\"https://u:p@h/v1\"\napi_key=\"sk-secret\"\n[[backends]]\nname=\"b\"\ncommand=[\"c\"]\n[backends.env]\nTOKEN=\"t0k\"\n");
        let cfg: Config = merged(&g, &[]).unwrap().try_into().unwrap();
        let mut tree = serde_json::to_value(&cfg).unwrap();
        redact(&mut tree, &[]);
        let s = tree.to_string();
        assert!(!s.contains("sk-secret") && !s.contains("u:p@") && !s.contains("t0k"), "{s}");
    }
}
