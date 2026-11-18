//! Whole-worktree checkpoints for rewinding changes the tool-level restore cannot see (`bash`).

use anyhow::{bail, Result};
use std::path::Path;
use std::process::Command;

fn git(dir: &Path, args: &[&str]) -> Result<String> {
    let out = Command::new("git").arg("-C").arg(dir).args(args).output()?;
    if !out.status.success() {
        bail!("git {}: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// A commit holding the tracked worktree state, without touching the worktree or index: a stash
/// commit when there are changes, else `HEAD`. `None` outside a repository or before the first commit.
pub fn create(dir: &Path) -> Option<String> {
    let stash = git(dir, &["stash", "create"]).ok()?;
    let stash = stash.trim();
    if !stash.is_empty() { return Some(stash.to_string()); }
    git(dir, &["rev-parse", "HEAD"]).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

/// Restores tracked files from `sha` and reports how many paths differed first.
/// Untracked files are never removed, so this cannot delete work the checkpoint did not contain.
pub fn restore(dir: &Path, sha: &str) -> Result<usize> {
    let changed = git(dir, &["diff", "--name-only", sha])?;
    let count = changed.lines().filter(|l| !l.is_empty()).count();
    if count > 0 {
        git(dir, &["checkout", sha, "--", "."])?;
    }
    Ok(count)
}

/// Unified diff between `sha` and the current worktree, for the per-turn view.
pub fn diff(dir: &Path, sha: &str) -> Result<String> {
    git(dir, &["diff", sha])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("greencode-checkpoint-{}-{}", std::process::id(), std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
        std::fs::create_dir_all(&dir).unwrap();
        for args in [&["init", "-q"][..], &["config", "user.email", "a@b"][..], &["config", "user.name", "a"][..]] {
            git(&dir, args).unwrap();
        }
        std::fs::write(dir.join("a.txt"), "one\n").unwrap();
        git(&dir, &["add", "."]).unwrap();
        git(&dir, &["commit", "-qm", "init"]).unwrap();
        dir
    }

    #[test]
    fn checkpoints_restore_tracked_files_and_keep_untracked_ones() {
        let dir = repo();
        std::fs::write(dir.join("a.txt"), "one\nbash edit\n").unwrap();
        let sha = create(&dir).expect("checkpoint");
        // A later bash change is undone by restoring the checkpoint.
        std::fs::write(dir.join("a.txt"), "one\nlater\n").unwrap();
        std::fs::write(dir.join("new.txt"), "untracked\n").unwrap();
        assert!(diff(&dir, &sha).unwrap().contains("later"));
        assert_eq!(restore(&dir, &sha).unwrap(), 1);
        assert_eq!(std::fs::read_to_string(dir.join("a.txt")).unwrap(), "one\nbash edit\n");
        assert!(dir.join("new.txt").exists(), "untracked files are left alone");
        assert!(create(&dir).is_some());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
