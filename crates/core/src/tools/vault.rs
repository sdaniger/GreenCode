//! `vault://`: read and edit an Obsidian vault. `vault://` lists vaults, `vault://<name|path>/<file>` reads or
//! writes one note. Paths are confined to the vault root. `write <path>?open=1` opens it in Obsidian through its
//! CLI when one is installed.
use crate::config::Config;
use anyhow::{bail, Context, Result};
use serde_json::Value;
use std::path::{Path, PathBuf};

fn vault_root(cfg: &Config, name: Option<&str>) -> Result<PathBuf> {
    let v = &cfg.extra_tools_opts.vault;
    if !v.enabled {
        bail!("vault:// is disabled by vault.enabled");
    }
    if let Some(name) = name.filter(|n| !n.trim().is_empty() && *n != "default") {
        if v.path.trim().is_empty() {
            bail!("vault://{name} names a vault, but only vault.path is configured; set vault.path or use vault://default/<note>");
        }
        if !Path::new(&v.path).file_name().is_some_and(|folder| folder == name) {
            bail!("vault://{name} is not the configured vault ({}); use vault://default/<note>", v.path);
        }
    }
    v.root()
}

/// `vault://` (empty) → the vault list; `vault://<name>/<note>` → the root and the note path.
pub fn route(cfg: &Config, rest: &str) -> Result<(PathBuf, String)> {
    let rest = rest.trim_start_matches('/');
    if rest.is_empty() {
        return Ok((vault_root(cfg, None)?, String::new()));
    }
    // A first segment matching the configured vault folder (or `default`) names the vault; anything else is
    // the note path, so `vault://notes/a.md` and `vault://Vault/notes/a.md` both work.
    if let Some((head, tail)) = rest.split_once('/') {
        if let Ok(root) = vault_root(cfg, Some(head)) {
            return Ok((root, tail.to_string()));
        }
    }
    Ok((vault_root(cfg, None)?, rest.to_string()))
}

fn contained(root: &Path, rel: &str) -> Result<PathBuf> {
    let rel = rel.trim_start_matches('/');
    if rel.split('/').any(|part| part == "..") {
        bail!("vault paths cannot contain `..`");
    }
    let root = root.canonicalize().with_context(|| format!("vault {} is missing", root.display()))?;
    let path = crate::permission::normalize(&root.join(rel));
    let exists = path.exists().then(|| path.canonicalize().ok()).flatten();
    let candidate = exists.unwrap_or(path);
    if !candidate.starts_with(&root) {
        bail!("{} is outside the vault", candidate.display());
    }
    Ok(candidate)
}

/// The bytes behind a `vault://` read: the note, or a note listing for an empty path.
pub fn read(cfg: &Config, rest: &str) -> Result<String> {
    let (root, rel) = route(cfg, rest)?;
    if rel.is_empty() {
        let mut names: Vec<String> = std::fs::read_dir(&root)
            .with_context(|| format!("cannot list vault {}", root.display()))?
            .flatten()
            .filter_map(|e| e.file_name().into_string().ok())
            .filter(|n| !n.starts_with('.'))
            .collect();
        names.sort();
        return Ok(format!("vault {}:\n{}", root.display(), names.join("\n")));
    }
    let path = contained(&root, &rel)?;
    let bytes = std::fs::read(&path).with_context(|| path.display().to_string())?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

pub fn write(cfg: &Config, rest: &str, content: &str) -> Result<String> {
    let (root, rel) = route(cfg, rest)?;
    if rel.is_empty() {
        bail!("write needs a note path: vault://<note>");
    }
    if rel.ends_with('/') {
        bail!("`{rel}` is a folder");
    }
    let path = contained(&root, &rel)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, content)?;
    Ok(format!("wrote {} bytes to {}", content.len(), path.display()))
}

pub fn edit(cfg: &Config, rest: &str, old: &str, new: &str, all: bool) -> Result<String> {
    let (root, rel) = route(cfg, rest)?;
    let path = contained(&root, &rel)?;
    let text = std::fs::read_to_string(&path).with_context(|| path.display().to_string())?;
    let count = text.matches(old).count();
    if count == 0 {
        bail!("old_string not found in {}", path.display());
    }
    if count > 1 && !all {
        bail!("old_string matches {count} times; add context or set replace_all");
    }
    let out = if all { text.replace(old, new) } else { text.replacen(old, new, 1) };
    std::fs::write(&path, out)?;
    Ok(format!("replaced {} occurrence(s) in {}", if all { count } else { 1 }, path.display()))
}

/// Opens a note in Obsidian through whichever CLI is installed; a missing CLI is a clear error.
pub fn open(cfg: &Config, rest: &str) -> Result<String> {
    let (root, rel) = route(cfg, rest)?;
    let path = contained(&root, &rel)?;
    let cli = cfg
        .extra_tools_opts
        .vault
        .cli
        .trim()
        .to_string();
    let (program, args): (String, Vec<String>) = if !cli.is_empty() {
        (cli, vec!["open".into(), path.display().to_string()])
    } else if crate::config::executable("obsidian-cli").is_some() {
        ("obsidian-cli".into(), vec!["open".into(), path.display().to_string()])
    } else if let Some(obsidian) = crate::config::executable("obsidian") {
        (obsidian.display().to_string(), vec![format!("obsidian://open?path={}", path.display())])
    } else {
        bail!("no Obsidian CLI found (set vault.cli, or install `obsidian-cli`)")
    };
    let status = std::process::Command::new(&program)
        .args(&args)
        .stdin(std::process::Stdio::null())
        .status()
        .with_context(|| format!("cannot run {program}"))?;
    if !status.success() {
        bail!("{program} exited with {status}");
    }
    Ok(format!("opened {} in Obsidian", path.display()))
}

/// The `vault` ext tool.
pub fn command(cfg: &Config, args: &Value) -> Result<String> {
    let path = args["path"].as_str().unwrap_or("");
    match args["action"].as_str().context("vault needs an `action`")? {
        "list" => read(cfg, path),
        "read" => read(cfg, path),
        "write" => write(cfg, path, args["content"].as_str().unwrap_or_default()),
        "edit" => edit(
            cfg,
            path,
            args["old"].as_str().context("edit needs `old`")?,
            args["new"].as_str().context("edit needs `new`")?,
            args["replace_all"].as_bool().unwrap_or(false),
        ),
        "open" => open(cfg, path),
        other => bail!("unknown vault action `{other}`; use list, read, write, edit or open"),
    }
}

/// The one-line system-prompt entry, present only while vaults are enabled.
pub fn prompt_line(cfg: &Config) -> Option<&'static str> {
    cfg.extra_tools_opts.vault.enabled.then_some(
        "Obsidian vault: `vault://` lists notes and `vault://<note>` reads one with the `read` tool; \
         `write`/`edit` change them (confined to the vault).",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg_for(root: &Path) -> Config {
        let mut cfg = Config::default();
        cfg.extra_tools_opts.vault.path = root.display().to_string();
        cfg
    }

    #[test]
    fn lists_reads_writes_edits_and_confines_paths() {
        let dir = tempfile::tempdir().unwrap();
        // The vault root is canonicalized on access, so the fixture root must be too (`/var` → `/private/var`).
        let root = dir.path().join("Vault");
        std::fs::create_dir_all(root.join("notes")).unwrap();
        let root = std::fs::canonicalize(&root).unwrap();
        std::fs::write(root.join("notes/a.md"), "alpha beta beta").unwrap();
        let cfg = cfg_for(&root);
        let listing = read(&cfg, "").unwrap();
        assert!(listing.contains("notes") && listing.contains("Vault"), "{listing}");
        assert_eq!(read(&cfg, "notes/a.md").unwrap(), "alpha beta beta");
        write(&cfg, "notes/b.md", "new note").unwrap();
        assert_eq!(std::fs::read_to_string(root.join("notes/b.md")).unwrap(), "new note");
        edit(&cfg, "notes/a.md", "beta", "gamma", false).unwrap_err();
        assert_eq!(edit(&cfg, "notes/a.md", "beta", "gamma", true).unwrap(), "replaced 2 occurrence(s) in ".to_string() + &root.join("notes/a.md").display().to_string());
        assert_eq!(std::fs::read_to_string(root.join("notes/a.md")).unwrap(), "alpha gamma gamma");
        assert!(read(&cfg, "../escape.md").is_err());
        assert!(write(&cfg, "notes/", "x").is_err());
        let mut off = cfg.clone();
        off.extra_tools_opts.vault.enabled = false;
        assert!(read(&off, "").unwrap_err().to_string().contains("vault.enabled"));
        assert!(prompt_line(&cfg).is_some() && prompt_line(&off).is_none());
        let mut cli = cfg.clone();
        cli.extra_tools_opts.vault.cli = "/nonexistent/obsidian".into();
        assert!(open(&cli, "notes/a.md").is_err());
        assert!(read(&cfg, "default/notes/a.md").unwrap().contains("gamma"));
        assert!(read(&cfg, "other/notes/a.md").is_err());
    }
}
