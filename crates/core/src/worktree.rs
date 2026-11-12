use anyhow::{bail, Result};
use std::{
    path::{Path, PathBuf},
    process::Command,
};

fn git(dir: &Path, args: &[&str]) -> Result<String> {
    let out = Command::new("git").arg("-C").arg(dir).args(args).output()?;
    if !out.status.success() {
        bail!("git {}: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Creates a worktree on a new branch so parallel threads don't touch each other's files.
pub fn create(repo: &Path, id: &str) -> Result<PathBuf> {
    let path = crate::home().join("worktrees").join(id);
    std::fs::create_dir_all(path.parent().unwrap())?;
    git(repo, &["worktree", "add", "-b", &format!("greencode/{id}"), &path.to_string_lossy()])?;
    Ok(path)
}

/// Uncommitted changes including new files, as a unified diff. Empty outside a git repo.
pub fn diff(dir: &Path) -> String {
    let mut out = git(dir, &["diff", "HEAD"]).or_else(|_| git(dir, &["diff"])).unwrap_or_default();
    for f in git(dir, &["ls-files", "--others", "--exclude-standard"]).unwrap_or_default().lines() {
        // `--no-index` exits 1 when files differ, so read stdout regardless of status.
        if let Ok(o) = Command::new("git").arg("-C").arg(dir).args(["diff", "--no-index", "--", "/dev/null", f]).output() {
            out.push_str(&String::from_utf8_lossy(&o.stdout));
        }
    }
    out
}
