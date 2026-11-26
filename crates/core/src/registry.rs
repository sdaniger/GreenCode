//! ACP registry discovery and installation.
use std::{collections::BTreeMap, path::{Component, Path}};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256, Sha512};
use crate::config::{AcpCfg, Protocol};

const URL: &str = "https://cdn.agentclientprotocol.com/registry/v1/latest/registry.json";
pub(crate) const GEMINI_CLI_ID: &str = "gemini";

#[derive(Clone, Debug, Deserialize)]
pub struct Entry {
    pub id: String,
    pub name: String,
    pub version: String,
    #[serde(default)]
    pub description: String,
    #[serde(skip)]
    pub kind: String,
    pub distribution: Distribution,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct Distribution {
    pub npx: Option<Package>,
    pub uvx: Option<Package>,
    pub binary: Option<BTreeMap<String, Binary>>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct Package {
    pub package: String,
    #[serde(default)] pub args: Vec<String>,
    #[serde(default)] pub env: BTreeMap<String, String>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct Binary {
    pub archive: String,
    pub cmd: String,
    #[serde(default)] pub args: Vec<String>,
    #[serde(default)] pub env: BTreeMap<String, String>,
    pub sha256: Option<String>,
    /// Google's agy release manifest publishes SHA-512 instead.
    #[serde(default)] pub sha512: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Installed {
    pub id: String,
    pub name: String,
    pub version: String,
    pub command: Vec<String>,
    #[serde(default)] pub env: BTreeMap<String, String>,
}

impl Installed {
    pub fn backend(&self) -> AcpCfg {
        AcpCfg { name: self.id.clone(), command: self.command.clone(), env: self.env.clone(), protocol: Protocol::Acp }
    }
}

pub fn parse(json: &str) -> Result<Vec<Entry>> {
    #[derive(Deserialize)] struct Registry { agents: Vec<Entry> }
    let mut entries = serde_json::from_str::<Registry>(json)?.agents;
    entries.retain(|entry| entry.id != GEMINI_CLI_ID);
    for entry in &mut entries {
        entry.kind = if entry.distribution.npx.is_some() { "npx" } else if entry.distribution.uvx.is_some() { "uvx" } else if entry.distribution.binary.is_some() { "binary" } else { "unknown" }.into();
    }
    Ok(entries)
}

pub async fn fetch() -> Result<String> {
    let path = crate::home().join("acp-registry.json");
    let result = async {
        let text = reqwest::Client::builder().timeout(std::time::Duration::from_secs(15)).build()?.get(URL).send().await?.error_for_status()?.text().await?;
        parse(&text)?;
        std::fs::create_dir_all(crate::home())?;
        std::fs::write(&path, &text)?;
        Ok::<_, anyhow::Error>(text)
    }.await;
    result.or_else(|e| std::fs::read_to_string(path).context(e))
}

pub fn platform() -> &'static str {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => "darwin-aarch64", ("macos", "x86_64") => "darwin-x86_64",
        ("linux", "aarch64") => "linux-aarch64", ("linux", "x86_64") => "linux-x86_64",
        ("windows", "x86_64") => "windows-x86_64", ("windows", "aarch64") => "windows-aarch64", _ => "unsupported",
    }
}

fn records() -> Result<Vec<Installed>> {
    match std::fs::read(crate::home().join("registry-agents.json")) {
        Ok(bytes) => Ok(serde_json::from_slice(&bytes)?),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(vec![]),
        Err(e) => Err(e.into()),
    }
}
pub fn installed() -> Vec<Installed> { records().unwrap_or_default() }

/// Installed entries the registry lists at another version; only those are update candidates.
/// Antigravity is skipped: GreenCode installs it from Google's agy release manifest, which runs ahead of the registry.
pub fn outdated(installed: &[Installed], entries: &[Entry]) -> Vec<Entry> {
    entries.iter().filter(|entry| entry.id != crate::antigravity::REGISTRY_ID && installed.iter().any(|i| i.id == entry.id && i.version != entry.version)).cloned().collect()
}

/// `marketplace.autoUpdate`: what startup does with the update candidates.
#[derive(Debug)]
pub struct UpdatePlan {
    /// Off: no check at all.
    pub check: bool,
    pub install: bool,
    pub entries: Vec<Entry>,
}

pub fn update_plan(mode: crate::config::interaction::MarketplaceUpdate, stale: Vec<Entry>) -> UpdatePlan {
    use crate::config::interaction::MarketplaceUpdate::*;
    match mode {
        Off => UpdatePlan { check: false, install: false, entries: Vec::new() },
        Notify => UpdatePlan { check: true, install: false, entries: stale },
        Auto => UpdatePlan { check: true, install: true, entries: stale },
    }
}
fn save(records: &[Installed]) -> Result<()> {
    std::fs::create_dir_all(crate::home())?;
    let path = crate::home().join("registry-agents.json");
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(records)?)?;
    std::fs::rename(tmp, path)?;
    Ok(())
}
fn lock_records() -> Result<std::fs::File> {
    std::fs::create_dir_all(crate::home())?;
    let lock = std::fs::OpenOptions::new().write(true).create(true).truncate(false).open(crate::home().join("registry-agents.lock"))?;
    lock.lock()?;
    Ok(lock)
}
fn component(s: &str) -> Result<()> {
    anyhow::ensure!(!s.is_empty() && s != "." && s != ".." && s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.')), "invalid registry path component");
    Ok(())
}
async fn check_archive(archive: &Path, zip: bool) -> Result<()> {
    let (program, names, details): (&str, &[&str], &[&str]) = if zip {
        ("/usr/bin/unzip", &["-Z", "-1"], &["-Z", "-l"])
    } else {
        ("tar", &["tf"], &["tvf"])
    };
    let list = tokio::process::Command::new(program).args(names).arg(archive).output().await?;
    anyhow::ensure!(list.status.success(), "cannot list archive");
    let names = std::str::from_utf8(&list.stdout)?.lines().collect::<Vec<_>>();
    for &name in &names {
        let path = Path::new(name);
        anyhow::ensure!(!name.contains('\\') && !path.is_absolute() && path.components().all(|c| matches!(c, Component::Normal(_) | Component::CurDir)), "unsafe archive path: {name}");
    }
    let list = tokio::process::Command::new(program).args(details).arg(archive).output().await?;
    anyhow::ensure!(list.status.success(), "cannot inspect archive");
    let details = std::str::from_utf8(&list.stdout)?.lines().collect::<Vec<_>>();
    let entries = if zip {
        anyhow::ensure!(details.len() >= 3 && details[0].starts_with("Archive:") && details[1].starts_with("Zip file size:"), "invalid zip listing");
        &details[2..details.len() - 1]
    } else { &details[..] };
    anyhow::ensure!(entries.len() == names.len(), "invalid archive listing");
    archive_sizes(entries, zip)?;
    Ok(())
}

/// Rejects links, special files and archives that expand past the cap; returns the declared total.
fn archive_sizes(entries: &[&str], zip: bool) -> Result<u64> {
    // The download limit bounds compressed bytes only; a small archive can still expand enormously.
    const MAX_UNCOMPRESSED: u64 = 2 * 1024 * 1024 * 1024;
    let mut total: u64 = 0;
    for line in entries {
        // `unzip -Z -l` right-aligns the length, so zip rows start with spaces; tar rows start with the mode.
        let line = line.trim_start();
        anyhow::ensure!(line.starts_with('-') || line.starts_with('d') || (zip && line.starts_with('?')), "archive links or special files are not supported");
        if line.starts_with('d') || (zip && line.starts_with('?') && line.trim_end().ends_with('/')) { continue; }
        let size = if zip { line.split_whitespace().nth(3) } else { line.split_whitespace().nth(4) }.and_then(|s| s.parse::<u64>().ok());
        total = total.checked_add(size.context("archive entry size is unknown")?).context("archive size overflow")?;
        anyhow::ensure!(total <= MAX_UNCOMPRESSED, "archive expands to more than 2 GiB");
    }
    Ok(total)
}

fn finalize_extracted(root: &Path) -> Result<()> {
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir)? {
            let entry = entry?;
            let path = entry.path();
            let meta = std::fs::symlink_metadata(&path)?;
            let ft = meta.file_type();
            anyhow::ensure!(ft.is_file() || ft.is_dir(), "archive links or special files are not supported");
            if ft.is_dir() {
                stack.push(path);
            } else {
                #[cfg(unix)]
                if entry.file_name().to_str().is_some_and(|n| n.starts_with("agy_acp_server") || n.starts_with("localharness")) {
                    use std::os::unix::fs::PermissionsExt;
                    let mut perms = meta.permissions();
                    perms.set_mode(perms.mode() | 0o111);
                    std::fs::set_permissions(&path, perms)?;
                }
            }
        }
    }
    Ok(())
}

struct Staging(std::path::PathBuf);
impl Drop for Staging {
    fn drop(&mut self) { let _ = std::fs::remove_dir_all(&self.0); }
}


pub async fn install(entry: &Entry) -> Result<AcpCfg> {
    anyhow::ensure!(entry.id != GEMINI_CLI_ID, "Gemini CLI is no longer supported; use Google Antigravity");
    component(&entry.id)?;
    component(&entry.version)?;
    // Read records before changing any installed files, including on a corrupt record file.
    records()?;
    let (mut command, env, staged) = if let Some(p) = &entry.distribution.npx {
        (vec!["npx".into(), "-y".into(), p.package.clone()].into_iter().chain(p.args.clone()).collect::<Vec<String>>(), p.env.clone(), None)
    } else if let Some(p) = &entry.distribution.uvx {
        (vec!["uvx".into(), p.package.clone()].into_iter().chain(p.args.clone()).collect(), p.env.clone(), None)
    } else {
        let b = entry.distribution.binary.as_ref().and_then(|b| b.get(platform())).context("agent has no binary for this platform")?;
        let relative = Path::new(&b.cmd);
        anyhow::ensure!(!b.cmd.is_empty() && !b.cmd.contains('\\') && !relative.is_absolute() && relative.components().all(|c| matches!(c, Component::Normal(_) | Component::CurDir)), "invalid binary command path");
        let mut response = reqwest::get(&b.archive).await?.error_for_status()?;
        const MAX_ARCHIVE_SIZE: usize = 512 * 1024 * 1024;
        anyhow::ensure!(response.content_length().is_none_or(|n| n <= MAX_ARCHIVE_SIZE as u64), "archive download too large");
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await? {
            anyhow::ensure!(chunk.len() <= MAX_ARCHIVE_SIZE - bytes.len(), "archive download too large");
            bytes.extend_from_slice(&chunk);
        }
        if let Some(expected) = &b.sha256 { anyhow::ensure!(format!("{:x}", Sha256::digest(&bytes)).eq_ignore_ascii_case(expected), "archive sha256 mismatch"); }
        if let Some(expected) = &b.sha512 { anyhow::ensure!(format!("{:x}", Sha512::digest(&bytes)).eq_ignore_ascii_case(expected), "archive sha512 mismatch"); }
        let agents = crate::home().join("agents");
        std::fs::create_dir_all(&agents)?;
        anyhow::ensure!(!std::fs::symlink_metadata(&agents)?.file_type().is_symlink(), "agents directory is a symlink");
        let agents = std::fs::canonicalize(agents)?;
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let stage = Staging(agents.join(format!(".stage-{}-{}", std::process::id(), NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed))));
        std::fs::create_dir(&stage.0)?;
        let contents = stage.0.join("contents");
        std::fs::create_dir(&contents)?;
        let url = b.archive.split('?').next().unwrap_or(&b.archive);
        if url.ends_with(".exe") {
            // A bare executable (agy's Windows releases): the download is the command itself.
            std::fs::write(contents.join(relative), &bytes)?;
        } else {
            let archive = stage.0.join(".download");
            std::fs::write(&archive, &bytes)?;
            let zip = url.ends_with(".zip");
            let (program, args): (&str, &[&str]) = if zip { ("/usr/bin/unzip", &["-q", "-o"]) } else if url.ends_with(".tar.gz") || url.ends_with(".tgz") { ("tar", &["xzf"]) } else if url.ends_with(".tar.bz2") { ("tar", &["xjf"]) } else { bail!("unsupported archive format") };
            check_archive(&archive, zip).await?;
            let status = tokio::process::Command::new(program).args(args).arg(&archive).current_dir(&contents).status().await?;
            std::fs::remove_file(&archive)?;
            anyhow::ensure!(status.success(), "archive extraction failed: {status}");
        }
        finalize_extracted(&contents)?;
        let cmd = std::fs::canonicalize(contents.join(relative)).context("archive command missing")?;
        anyhow::ensure!(cmd.starts_with(&contents) && cmd.is_file(), "archive command is not a file inside the install directory");
        let suffix = cmd.strip_prefix(&contents)?.to_path_buf();
        let dir = agents.join(&entry.id).join(&entry.version);
        #[cfg(unix)] {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&cmd)?.permissions();
            perms.set_mode(perms.mode() | 0o111);
            std::fs::set_permissions(&cmd, perms)?;
        }
        (std::iter::once(dir.join(suffix).to_string_lossy().into_owned()).chain(b.args.clone()).collect(), b.env.clone(), Some((stage, dir)))
    };
    let _guard = lock_records()?;
    let mut all = records()?;
    if let Some((stage, dir)) = staged {
        let parent = dir.parent().context("invalid agent directory")?;
        std::fs::create_dir_all(parent)?;
        anyhow::ensure!(!std::fs::symlink_metadata(parent)?.file_type().is_symlink(), "agent directory is a symlink");
        if dir.exists() { std::fs::remove_dir_all(&dir)?; }
        std::fs::rename(stage.0.join("contents"), &dir)?;
        command[0] = std::fs::canonicalize(&command[0])?.to_string_lossy().into_owned();
    }
    let item = Installed { id: entry.id.clone(), name: entry.name.clone(), version: entry.version.clone(), command, env };
    all.retain(|i| i.id != item.id);
    all.push(item.clone());
    save(&all)?;
    Ok(item.backend())
}

pub fn uninstall(id: &str) -> Result<()> {
    component(id)?;
    let _guard = lock_records()?;
    let mut all = records()?;
    anyhow::ensure!(all.iter().any(|i| i.id == id), "agent is not installed: {id}");
    let agents = crate::home().join("agents");
    if let Ok(metadata) = std::fs::symlink_metadata(&agents) {
        anyhow::ensure!(!metadata.file_type().is_symlink(), "agents directory is a symlink");
    }
    let dir = agents.join(id);
    match std::fs::remove_dir_all(dir) { Ok(()) => {}, Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}, Err(e) => return Err(e.into()) }
    all.retain(|i| i.id != id);
    save(&all)
}

#[cfg(test)]
mod tests {
    use super::archive_sizes;

    #[test]
    fn archive_sizes_reject_links_and_bombs() {
        let tar = ["-rw-r--r--  0 501  20  1024 Nov 12 21:00 a", "drwxr-xr-x  0 501  20  0 Nov 12 21:00 dir/"];
        assert_eq!(archive_sizes(&tar, false).unwrap(), 1024);
        assert!(archive_sizes(&["lrwxrwxrwx  0 501  20  0 Nov 12 21:00 link"], false).is_err());
        let bomb = ["-rw-r--r--  0 501  20  1073741824 Nov 12 21:00 a", "-rw-r--r--  0 501  20  1073741824 Nov 12 21:00 b", "-rw-r--r--  0 501  20  1 Nov 12 21:00 c"];
        assert!(archive_sizes(&bomb, false).is_err());
        assert_eq!(archive_sizes(&["-rw-r--r--  3.0 unx  2048 tx  2048 stor 26-Nov-13 04:21 a.txt"], true).unwrap(), 2048);
        assert_eq!(archive_sizes(&["?rw-------  2.0 unx  1024 b-  1024 stor 26-Nov-18 14:57 agy_acp_server.par", "?rwxr-xr-x  2.0 unx  0 b-  0 stor 26-Nov-18 14:57 sub/"], true).unwrap(), 1024);
        assert!(archive_sizes(&["lrwxrwxrwx  2.0 unx     6 b-     6 stor 80-Jan-01 00:00 link"], true).is_err());
        assert!(archive_sizes(&["-rw-r--r--  3.0 unx  huge tx  1 stor 26-Nov-13 04:21 a.txt"], true).is_err());
    }

    #[test]
    fn outdated_lists_only_installed_entries_at_another_version() {
        let entries = super::parse(r#"{"agents":[
            {"id":"a","name":"A","version":"2","distribution":{"npx":{"package":"a"}}},
            {"id":"b","name":"B","version":"1","distribution":{"npx":{"package":"b"}}},
            {"id":"c","name":"C","version":"1","distribution":{"npx":{"package":"c"}}}]}"#).unwrap();
        let installed = ["a", "b", "z"].into_iter().zip(["1", "1", "1"]).map(|(id, version)| super::Installed {
            id: id.into(), name: id.into(), version: version.into(), command: vec![], env: Default::default(),
        }).collect::<Vec<_>>();
        let stale: Vec<_> = super::outdated(&installed, &entries).into_iter().map(|e| e.id).collect();
        assert_eq!(stale, ["a"]);
    }

    #[test]
    fn marketplace_modes_check_report_or_install() {
        use crate::config::interaction::MarketplaceUpdate;
        let entries = super::parse(r#"{"agents":[{"id":"a","name":"A","version":"2","distribution":{"npx":{"package":"a"}}}]}"#).unwrap();
        let off = super::update_plan(MarketplaceUpdate::Off, entries.clone());
        assert!(!off.check && !off.install && off.entries.is_empty());
        let notify = super::update_plan(MarketplaceUpdate::Notify, entries.clone());
        assert!(notify.check && !notify.install && notify.entries.len() == 1);
        let auto = super::update_plan(MarketplaceUpdate::Auto, entries);
        assert!(auto.check && auto.install && auto.entries.len() == 1);
    }
}
