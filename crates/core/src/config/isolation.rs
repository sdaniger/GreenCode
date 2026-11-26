//! Where separate copies of a checkout come from: the isolation backend, ovnfs checkouts and
//! `worktree.cleanSource`; flattened into `Config` so `/settings` and the worktree code share it.
use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::{Path, PathBuf};

/// Copy mechanism for isolated subagents and linked worktrees.
#[derive(Deserialize, Serialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum Backend {
    /// ovnfs when found, else the best copy-on-write filesystem copy available here.
    #[default]
    Auto,
    /// macOS `clonefile`.
    Apfs,
    /// btrfs subvolume snapshot (Linux).
    Btrfs,
    /// ZFS snapshot + clone.
    Zfs,
    /// Linux `FICLONE` per file (`cp --reflink=always`).
    Reflink,
    /// Linux kernel overlay, or fuse-overlayfs.
    Overlayfs,
    /// Windows Projected File System.
    Projfs,
    /// Windows `FSCTL_DUPLICATE_EXTENTS_TO_FILE`.
    BlockClone,
    /// git worktree when possible, else a plain recursive copy.
    Rcopy,
}

#[derive(Deserialize, Serialize, Clone, Debug, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct IsolationCfg {
    pub backend: Backend,
}

impl Default for IsolationCfg {
    fn default() -> Self { Self { backend: Backend::default() } }
}

#[derive(Deserialize, Serialize, Clone, Debug, Default, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct OvnfsCfg {
    /// ovnfs executable; unset looks for `ovnfs` on PATH.
    pub path: Option<String>,
}

#[derive(Deserialize, Serialize, Clone, Debug, Default, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct IsolationOpts {
    pub isolation: IsolationCfg,
    pub ovnfs: OvnfsCfg,
}

impl IsolationOpts {
    /// `ovnfs.path` when it names an executable, else `ovnfs` found on PATH.
    pub fn ovnfs(&self) -> Option<PathBuf> {
        if let Some(path) = self.ovnfs.path.as_deref().map(str::trim).filter(|p| !p.is_empty()) {
            let path = PathBuf::from(shellexpand(path));
            return executable(&path).then_some(path);
        }
        let dirs = std::env::var_os("PATH")?;
        std::env::split_paths(&dirs)
            .flat_map(|dir| ["ovnfs", "ovnfs.exe"].map(|name| dir.join(name)))
            .find(|candidate| executable(candidate))
    }
}

fn executable(path: &Path) -> bool {
    let Some(meta) = std::fs::metadata(path).ok().filter(|m| m.is_file()) else { return false };
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    { let _ = meta; true }
}

/// A leading `~` means the user's home.
pub fn shellexpand(value: &str) -> String {
    match value.strip_prefix('~') {
        Some(rest) => crate::user_home().join(rest.trim_start_matches('/')).to_string_lossy().into_owned(),
        None => value.into(),
    }
}

/// One ovnfs checkout `source` into `dest`, whose parent must exist; returns `dest`.
/// ovnfs's daemon does the copy-on-write work, so this is ready in about a second at any size.
pub fn ovnfs_checkout(binary: &Path, source: &Path, dest: &Path) -> Result<PathBuf> {
    let name = dest.file_name().and_then(|n| n.to_str()).unwrap_or("checkout");
    let out = std::process::Command::new(binary)
        .args(["--color", "never", "--json", "new", "--dir"])
        .arg(source)
        .args(["--name", name])
        .output();
    let out = out.map_err(|e| anyhow::anyhow!("cannot run {}: {e}; set ovnfs.path to the ovnfs binary", binary.display()))?;
    if !out.status.success() {
        bail!("ovnfs new failed: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    // The daemon answers with the checkout's own path; fall back to `dest` when it prints none.
    let printed = serde_json::from_slice::<Value>(&out.stdout).ok()
        .and_then(|v| v["checkout"]["path"].as_str().map(PathBuf::from));
    if let Some(path) = printed {
        if path != dest { return Ok(path); }
    }
    if dest.exists() { Ok(dest.to_path_buf()) } else { bail!("ovnfs did not create {}", dest.display()) }
}

/// Guidance for the system prompt: with ovnfs present, agents make checkouts instead of copies.
pub const OVNFS_GUIDANCE: &str = "Need a second copy of a folder (a risky experiment, another branch, tests or review beside the user's work)? MUST use an ovnfs checkout, not `git worktree add`, `git clone` or `cp -r`, unless the user asks for those.";

pub fn prompt_note(ovnfs: Option<&Path>) -> Option<String> {
    ovnfs.map(|_| format!("{OVNFS_GUIDANCE}\nRun `ovnfs new --dir <folder> --name <name>` for it."))
}

/// OMP key → native dotted key.
pub(crate) const OMP: &[(&str, &str)] = &[
    ("isolation.backend", "isolation.backend"),
    ("ovnfs.path", "ovnfs.path"),
];

pub(crate) fn convert(_from: &str, _value: &Value) -> Option<Result<toml::Value>> { None }

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backend_values_and_ovnfs_lookup() {
        let cfg = crate::config::settings::decode(toml::Value::Table(crate::config::settings::translate(
            "isolation:\n  backend: block-clone\novnfs:\n  path: /nope/ovnfs\n",
        ).unwrap())).unwrap();
        assert_eq!(cfg.isolation_opts.isolation.backend, Backend::BlockClone);
        assert!(cfg.isolation_opts.ovnfs().is_none(), "a configured path must be executable");
        let bad = crate::config::settings::translate("isolation:\n  backend: magic\n").unwrap();
        assert!(crate::config::settings::decode(toml::Value::Table(bad)).is_err(), "an unknown backend is rejected when decoded");
        assert_eq!(Backend::default(), Backend::Auto);
        let dir = std::env::temp_dir().join(format!("greencode-ovnfs-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let bin = dir.join("ovnfs");
        std::fs::write(&bin, "#!/bin/sh\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let found = IsolationOpts { ovnfs: OvnfsCfg { path: Some(bin.to_string_lossy().into_owned()) }, ..Default::default() };
        assert_eq!(found.ovnfs().as_deref(), Some(bin.as_path()));
        assert!(prompt_note(found.ovnfs().as_deref()).unwrap().contains("ovnfs checkout"));
        assert!(prompt_note(None).is_none());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
