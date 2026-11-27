//! Import a connection, not a second copy of Hermes' credentials or model context.

use crate::config::{AcpCfg, Protocol};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::{io::Write, path::{Path, PathBuf}};

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct Profile {
    pub name: String,
    pub home: PathBuf,
}

fn valid_name(name: &str) -> bool {
    !name.is_empty() && name.len() <= 64 && name.bytes().enumerate().all(|(i, c)| {
        c.is_ascii_lowercase() || c.is_ascii_digit() || (i > 0 && matches!(c, b'_' | b'-'))
    })
}

fn expand(path: &Path) -> PathBuf {
    path.strip_prefix("~").map(|rest| crate::user_home().join(rest)).unwrap_or_else(|_| path.to_path_buf())
}

fn root() -> PathBuf {
    let home = std::env::var_os("HERMES_HOME").filter(|p| !p.is_empty()).map(|p| expand(Path::new(&p)))
        .unwrap_or_else(|| crate::user_home().join(".hermes"));
    if home.parent().is_some_and(|p| p.file_name().is_some_and(|n| n == "profiles")) {
        home.parent().unwrap().parent().unwrap().to_path_buf()
    } else { home }
}

pub fn is_backend(name: &str) -> bool {
    name == "hermes" || name.starts_with("hermes:")
}

fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Interactive setup command for `hermes` or an imported `hermes:<profile>` backend.
pub fn setup_command(backend: &str) -> String {
    if let Some(name) = backend.strip_prefix("hermes:") {
        if let Ok(profiles) = imported() {
            if let Some(profile) = profiles.into_iter().find(|p| p.name == name) {
                if let Ok((root, selector)) = profile.root_and_selector() {
                    if let Some(root) = root.to_str() {
                        return format!("HERMES_HOME={} hermes --profile {} acp --setup", shell_quote(root), shell_quote(selector));
                    }
                }
            }
        }
        return format!("hermes --profile {} acp --setup", shell_quote(name));
    }
    "hermes acp --setup".into()
}

impl Profile {
    fn root_and_selector(&self) -> Result<(&Path, &str)> {
        self.validate()?;
        match self.home.parent().filter(|p| p.file_name().is_some_and(|n| n == "profiles")) {
            Some(parent) => Ok((parent.parent().unwrap(), self.home.file_name().unwrap().to_str().context("non-UTF-8 Hermes profile path")?)),
            None => Ok((self.home.as_path(), "default")),
        }
    }

    fn validate(&self) -> Result<()> {
        anyhow::ensure!(valid_name(&self.name), "invalid Hermes profile name `{}`", self.name);
        anyhow::ensure!(self.home.is_absolute(), "Hermes profile home must be absolute");
        let markers = ["config.yaml", ".env", "SOUL.md", "profile.yaml", "auth.json", "state.db"];
        anyhow::ensure!(self.home.is_dir() && markers.iter().any(|p| {
            let p = self.home.join(p);
            p.is_file() || p.is_symlink()
        }), "{} is not a Hermes profile directory", self.home.display());
        if let Some(parent) = self.home.parent().filter(|p| p.file_name().is_some_and(|n| n == "profiles")) {
            let selector = self.home.file_name().and_then(|n| n.to_str()).context("invalid Hermes profile directory name")?;
            anyhow::ensure!(valid_name(selector) && selector != "default", "invalid named Hermes profile `{selector}`");
            anyhow::ensure!(!parent.join(".deleted").join(self.home.file_name().unwrap()).exists(), "Hermes profile has been deleted");
        }
        Ok(())
    }

    pub fn backend(&self) -> Result<AcpCfg> {
        let (root, selector) = self.root_and_selector()?;
        let hermes = crate::config::executable("hermes").unwrap_or_else(|| "hermes".into());
        // Explicit selector prevents Hermes' sticky active_profile from redirecting this connection.
        Ok(AcpCfg {
            name: format!("hermes:{}", self.name), protocol: Protocol::Acp,
            env: Default::default(),
            command: vec!["/usr/bin/env".into(), format!("HERMES_HOME={}", root.to_str().context("non-UTF-8 Hermes home")?),
                hermes.to_string_lossy().into_owned(), "--profile".into(), selector.into(), "acp".into()],
        })
    }
}

fn discover_at(root: &Path) -> Result<Vec<Profile>> {
    let root = std::path::absolute(root)?;
    let default = Profile { name: "default".into(), home: root.clone() };
    let mut profiles = vec![];
    if default.validate().is_ok() { profiles.push(default); }
    match std::fs::read_dir(root.join("profiles")) {
        Ok(entries) => for entry in entries {
            let entry = entry?;
            let profile = Profile { name: entry.file_name().to_string_lossy().into_owned(), home: entry.path() };
            if profile.validate().is_ok() { profiles.push(profile); }
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {},
        Err(e) => return Err(e.into()),
    }
    profiles.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(profiles)
}

pub fn discover() -> Result<Vec<Profile>> { discover_at(&root()) }

pub fn resolve(source: &str) -> Result<Profile> {
    let home = if source == "default" { root() }
        else if valid_name(source) && !Path::new(source).is_dir() { root().join("profiles").join(source) }
        else { expand(Path::new(source)) };
    let home = std::path::absolute(home)?;
    let name = if home.parent().is_some_and(|p| p.file_name().is_some_and(|n| n == "profiles")) {
        home.file_name().and_then(|n| n.to_str()).context("profile directory has no valid name")?.to_string()
    } else { "default".into() };
    let profile = Profile { name, home };
    profile.validate()?;
    Ok(profile)
}

pub fn imported() -> Result<Vec<Profile>> {
    let dir = crate::home().join("hermes-profiles");
    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
        Err(e) => return Err(e.into()),
    };
    let mut profiles = vec![];
    for entry in entries {
        let path = entry?.path();
        if path.extension().is_none_or(|e| e != "json") { continue; }
        let profile: Profile = serde_json::from_slice(&std::fs::read(&path)?).with_context(|| format!("{}", path.display()))?;
        profile.validate()?;
        anyhow::ensure!(path.file_stem().and_then(|n| n.to_str()) == Some(profile.name.as_str()), "Hermes profile registration name mismatch");
        profiles.push(profile);
    }
    profiles.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(profiles)
}

fn save(profile: &Profile, dir: &Path) -> Result<()> {
    profile.validate()?;
    std::fs::create_dir_all(dir)?;
    let path = dir.join(format!("{}.json", profile.name));
    let bytes = serde_json::to_vec_pretty(profile)?;
    let tmp = dir.join(format!("{}.tmp", crate::session::Meta::new(dir.into(), String::new(), String::new()).id));
    let mut file = std::fs::OpenOptions::new().write(true).create_new(true).open(&tmp)?;
    let result = file.write_all(&bytes).and_then(|_| file.sync_all()).and_then(|_| std::fs::hard_link(&tmp, &path));
    let _ = std::fs::remove_file(tmp);
    match result {
        Ok(()) => {},
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            let saved: Profile = serde_json::from_slice(&std::fs::read(&path)?)?;
            anyhow::ensure!(saved == *profile, "Hermes profile `{}` is already imported from {}; registration was not overwritten", profile.name, saved.home.display());
        }
        Err(e) => return Err(e.into()),
    }
    Ok(())
}

pub fn import(profile: &Profile, cfg: &mut crate::config::Config) -> Result<String> {
    let backend = profile.backend()?;
    if let Some(existing) = cfg.backends.iter().find(|b| b.name == backend.name) {
        anyhow::ensure!(existing.command == backend.command && existing.protocol == backend.protocol, "backend `{}` already exists; it was not overwritten", backend.name);
    }
    save(profile, &crate::home().join("hermes-profiles"))?;
    let name = backend.name.clone();
    if !cfg.backends.iter().any(|b| b.name == name) { cfg.backends.push(backend); }
    Ok(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discovery_and_import_preserve_sources_and_refuse_collisions() {
        let dir = std::env::temp_dir().join(format!("greencode-hermes-{}", std::process::id()));
        let root = dir.join("Hermes home with spaces");
        let coder = root.join("profiles/coder");
        for path in [&root, &coder, &root.join("profiles/deleted")] {
            std::fs::create_dir_all(path).unwrap();
            std::fs::write(path.join("config.yaml"), "model: fixture\n").unwrap();
        }
        std::fs::create_dir_all(root.join("profiles/ghost/logs")).unwrap();
        std::fs::create_dir_all(root.join("profiles/.deleted/deleted")).unwrap();
        let profiles = discover_at(&root).unwrap();
        assert_eq!(profiles.iter().map(|p| p.name.as_str()).collect::<Vec<_>>(), ["coder", "default"]);
        let profile = &profiles[0];
        let dest = dir.join("registrations");
        save(profile, &dest).unwrap();
        save(profile, &dest).unwrap();
        let bytes = std::fs::read(dest.join("coder.json")).unwrap();
        assert_eq!(serde_json::from_slice::<Profile>(&bytes).unwrap(), *profile);
        assert_eq!(std::fs::read_to_string(coder.join("config.yaml")).unwrap(), "model: fixture\n");
        assert!(!coder.join("state.db").exists());
        assert_eq!(std::fs::read_dir(&dest).unwrap().count(), 1);
        let other = Profile { name: "coder".into(), home: root.clone() };
        assert!(save(&other, &dest).is_err());
        assert_eq!(std::fs::read(dest.join("coder.json")).unwrap(), bytes);
        assert!(save(&Profile { name:"../escape".into(), home:root }, &dest).is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn backend_matching_and_setup_commands() {
        assert!(is_backend("hermes"));
        assert!(is_backend("hermes:default"));
        assert!(is_backend("hermes:coder"));
        assert!(!is_backend("hermes_other"));
        assert!(!is_backend("opencode"));
        assert_eq!(setup_command("hermes"), "hermes acp --setup");
        assert_eq!(setup_command("hermes:missing"), "hermes --profile 'missing' acp --setup");
    }
}
