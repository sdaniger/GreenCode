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
pub fn create(repo: &Path, base: &Path, id: &str) -> Result<PathBuf> {
    let path = base.join(id);
    std::fs::create_dir_all(path.parent().unwrap())?;
    git(repo, &["worktree", "add", "-b", &format!("greencode/{id}"), &path.to_string_lossy()])?;
    Ok(path)
}

/// Remove only a pristine task checkout that still points to its original commit and branch.
pub(crate) fn remove_pristine_task(repo: &Path, path: &Path, branch: &str, initial: &str) -> Result<()> {
    let reference = format!("refs/heads/{branch}");
    anyhow::ensure!(git(path, &["symbolic-ref", "HEAD"])?.trim() == reference
        && git(path, &["rev-parse", "HEAD"])?.trim() == initial,
        "HEAD changed");
    anyhow::ensure!(git(path, &["status", "--porcelain", "--untracked-files=all", "--ignored", "--ignore-submodules=none"])?.is_empty(),
        "worktree contains changes or local files");
    for r in ["HEAD", reference.as_str()] {
        anyhow::ensure!(git(path, &["reflog", "show", "--format=%H", r])?.lines().all(|sha| sha == initial),
            "reflog contains other commits");
    }
    remove_worktree(repo, path)?;
    // Compare-and-delete: never remove a branch that moved during cleanup.
    git(repo, &["update-ref", "--no-deref", "-d", &reference, initial])?;
    Ok(())
}

pub(crate) fn head(path: &Path) -> Result<String> {
    Ok(git(path, &["rev-parse", "HEAD"])?.trim().to_string())
}

/// Short status of a worktree, capped to avoid flooding task output.
pub fn summary(dir: &Path) -> String {
    let Ok(status) = git(dir, &["status", "--short"]) else { return String::new() };
    let lines: Vec<_> = status.lines().collect();
    let mut result = lines.iter().take(40).copied().collect::<Vec<_>>().join("\n");
    if lines.len() > 40 { result.push_str(&format!("\n… {} more", lines.len() - 40)); }
    result
}

#[derive(Clone, Debug)]
pub struct Candidate {
    pub path: PathBuf,
    pub reason: &'static str,
    pub size_bytes: u64,
}

fn size(path: &Path) -> u64 {
    let Ok(meta) = std::fs::symlink_metadata(path) else { return 0 };
    if !meta.is_dir() { return meta.len(); }
    std::fs::read_dir(path).into_iter().flatten().flatten().map(|entry| size(&entry.path())).sum()
}

/// Lists safe archived worktrees and unowned folders, which are never removable.
pub fn cleanup_candidates(threads: &[(String, PathBuf, bool, String)], project: &Path) -> Vec<Candidate> {
    let bases: Vec<_> = threads.iter().filter_map(|(_, path, _, _)| path.parent().map(Path::to_path_buf)).collect();
    cleanup_candidates_in(threads, project, &bases)
}

/// Includes explicit storage roots even when they contain no known worktrees.
pub fn cleanup_candidates_in(threads: &[(String, PathBuf, bool, String)], project: &Path, bases: &[PathBuf]) -> Vec<Candidate> {
    let head = git(project, &["rev-parse", "HEAD"]).ok();
    let mut candidates = Vec::new();
    for (_, path, archived, _) in threads {
        if !archived || threads.iter().any(|(_, other, archived, _)| other == path && !archived) || candidates.iter().any(|c: &Candidate| c.path == *path) { continue; }
        let Some(head) = head.as_deref() else { continue };
        if git(path, &["status", "--porcelain"]).is_ok_and(|s| s.is_empty())
            && git(path, &["rev-list", "--count", "HEAD", &format!("^{}", head.trim())]).is_ok_and(|s| s.trim() == "0") {
            candidates.push(Candidate { path: path.clone(), reason: "変更なし・保管済み", size_bytes: size(path) });
        }
    }
    for base in bases {
        for entry in std::fs::read_dir(base).into_iter().flatten().flatten() {
            let path = entry.path();
            if entry.file_type().is_ok_and(|t| t.is_dir()) && !threads.iter().any(|(_, used, _, _)| *used == path)
                && !candidates.iter().any(|c| c.path == path) {
                candidates.push(Candidate { size_bytes: size(&path), path, reason: "未使用フォルダ（削除しません）" });
            }
        }
    }
    candidates
}

pub fn remove_worktree(project: &Path, path: &Path) -> Result<()> {
    git(project, &["worktree", "remove", "--", &path.to_string_lossy()])?;
    Ok(())
}

/// Uncommitted changes including new files, as a unified diff. Empty outside a git repo.
pub fn diff(dir: &Path) -> String {
    let mut out = String::new();
    for (label, args) in [("Staged", &["diff", "--cached"][..]), ("Unstaged", &["diff"][..])] {
        let changes = git(dir, args).unwrap_or_default();
        if !changes.is_empty() { out.push_str(&format!("{label}:\n{changes}")); }
    }
    for f in git(dir, &["ls-files", "--others", "--exclude-standard", "-z"]).unwrap_or_default().split('\0').filter(|f| !f.is_empty()) {
        // `--no-index` exits 1 when files differ, so read stdout regardless of status.
        if let Ok(o) = Command::new("git").arg("-C").arg(dir).args(["diff", "--no-index", "--", "/dev/null", f]).output() {
            out.push_str(&String::from_utf8_lossy(&o.stdout));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worktree_cleanup_preserves_changes_and_orphans() {
        let root = std::env::temp_dir().join(format!("greencode-cleanup-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let project = root.join("repo");
        let base = root.join("trees");
        std::fs::create_dir_all(&project).unwrap();
        let run = |dir: &Path, args: &[&str]| {
            let out = Command::new("git").arg("-C").arg(dir).args(["-c", "user.name=test", "-c", "user.email=test@example.com"]).args(args).output().unwrap();
            assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        };
        run(&project, &["init", "-q"]);
        std::fs::write(project.join("tracked"), "one").unwrap();
        run(&project, &["add", "."]);
        run(&project, &["commit", "-qm", "initial"]);
        let clean = create(&project, &base, "clean").unwrap();
        let dirty = create(&project, &base, "dirty").unwrap();
        let ahead = create(&project, &base, "ahead").unwrap();
        std::fs::write(dirty.join("new"), "change").unwrap();
        std::fs::write(ahead.join("tracked"), "two").unwrap();
        run(&ahead, &["add", "."]);
        run(&ahead, &["commit", "-qm", "extra"]);
        let orphan = base.join("orphan");
        std::fs::create_dir_all(&orphan).unwrap();
        let threads: Vec<_> = [clean.clone(), dirty.clone(), ahead.clone()].into_iter().enumerate().map(|(i, p)| (i.to_string(), p, true, String::new())).collect();
        let found = cleanup_candidates(&threads, &project);
        assert!(found.iter().any(|c| c.path == clean && c.reason == "変更なし・保管済み"));
        assert!(!found.iter().any(|c| c.path == dirty || c.path == ahead));
        assert!(found.iter().any(|c| c.path == orphan && c.reason == "未使用フォルダ（削除しません）"));
        remove_worktree(&project, &clean).unwrap();
        assert!(!clean.exists() && orphan.exists());
        std::fs::remove_dir_all(root).unwrap();
    }
}
