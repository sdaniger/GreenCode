use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use std::{
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

/// What a fresh interactive session does about worktrees.
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum OnStart { #[default] Off, Ask, Create }

/// What happens to worktrees this launch created when it exits.
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum OnExit { #[default] Keep, Ask, Remove }

/// `[worktree]` in config.toml. Unset fields keep the previous behaviour.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct WorktreeCfg {
    /// Base for agent-managed worktrees; relative values are ignored.
    pub base: Option<String>,
    /// Copy git-ignored entries into new worktrees (APFS clonefile; plain checkout elsewhere).
    pub clone: bool,
    pub on_start: OnStart,
    pub on_exit: OnExit,
    /// `/wt` resets the source checkout and removes its untracked files after carrying them over.
    pub clean_source: bool,
}

impl Default for WorktreeCfg {
    fn default() -> Self { Self { base: None, clone: true, on_start: OnStart::Off, on_exit: OnExit::Keep, clean_source: false } }
}

impl WorktreeCfg {
    /// Where agent-managed worktrees live: `GREENCODE_WORKTREE_DIR`, then `worktree.base`, then the native default.
    /// Relative and empty values are ignored, and a leading `~` is the user's home.
    pub fn dir(&self) -> PathBuf {
        let expand = |value: &str| {
            let path = PathBuf::from(value);
            let path = path.strip_prefix("~").map(|rest| crate::user_home().join(rest)).unwrap_or(path);
            path.is_absolute().then_some(path)
        };
        std::env::var_os("GREENCODE_WORKTREE_DIR").filter(|v| !v.is_empty()).and_then(|v| expand(&v.to_string_lossy()))
            .or_else(|| self.base.as_deref().map(str::trim).filter(|s| !s.is_empty()).and_then(expand))
            .unwrap_or_else(|| crate::home().join("subagent-worktrees"))
    }
}

pub(crate) fn git(dir: &Path, args: &[&str]) -> Result<String> {
    let out = Command::new("git").arg("-C").arg(dir).args(args).output()?;
    if !out.status.success() {
        bail!("git {}: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Creates a worktree on a new branch so parallel threads don't touch each other's files.
pub fn create(repo: &Path, base: &Path, id: &str) -> Result<PathBuf> {
    create_with(repo, base, id, &Options::default())
}

/// How submodules are initialized in a new worktree.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub enum Submodules {
    #[default]
    Recursive,
    TopLevel,
    Skip,
}

#[derive(Clone, Debug)]
pub struct Options {
    /// Already normalized by `git::branch_prefix`.
    pub prefix: String,
    pub submodules: Submodules,
    /// Copy git-ignored build artifacts in as copy-on-write clones (`worktree.clone`).
    pub clone: bool,
    /// Copy mechanism for those clones (`isolation.backend`); `Auto` probes the filesystem.
    pub backend: crate::config::isolation::Backend,
}

impl Default for Options {
    fn default() -> Self {
        Options { prefix: "greencode/".into(), submodules: Submodules::default(), clone: true, backend: crate::config::isolation::Backend::default() }
    }
}

/// `create` with a branch prefix, submodule policy and clone choice; a failed submodule checkout removes the worktree again.
pub fn create_with(repo: &Path, base: &Path, id: &str, options: &Options) -> Result<PathBuf> {
    let path = base.join(id);
    std::fs::create_dir_all(path.parent().unwrap())?;
    let branch = format!("{}{id}", options.prefix);
    git(repo, &["worktree", "add", "-b", &branch, &path.to_string_lossy()])?;
    if options.clone { clone_ignored(repo, &path, options.backend); }
    let args: &[&str] = match options.submodules {
        Submodules::Skip => return Ok(path),
        Submodules::TopLevel => &["submodule", "update", "--init"],
        Submodules::Recursive => &["submodule", "update", "--init", "--recursive"],
    };
    if path.join(".gitmodules").exists() {
        if let Err(e) = git(&path, args) {
            let _ = git(repo, &["worktree", "remove", "--force", &path.to_string_lossy()]);
            let _ = git(repo, &["branch", "-D", &branch]);
            return Err(e);
        }
    }
    Ok(path)
}

/// Copies each top-level git-ignored entry with the configured backend so build artifacts carry over cheaply.
/// Best effort: a filesystem that cannot clone keeps the plain checkout, like OMP.
fn clone_ignored(repo: &Path, dest: &Path, backend: crate::config::isolation::Backend) {
    let Ok(entries) = git(repo, &["ls-files", "--others", "--ignored", "--exclude-standard", "--directory", "-z"]) else { return };
    for entry in entries.split('\0').filter(|e| !e.is_empty()) {
        let to = dest.join(entry);
        if let Some(parent) = to.parent() { let _ = std::fs::create_dir_all(parent); }
        if copy_entry(backend, &repo.join(entry), &to).is_err() { return; }
    }
}

/// The `cp` flags one backend uses here; `Err` names why this filesystem or platform cannot do it.
fn copy_flags(backend: crate::config::isolation::Backend) -> Result<&'static [&'static str]> {
    use crate::config::isolation::Backend;
    match backend {
        // macOS clonefile is APFS's block clone; Linux reflinks are FICLONE.
        Backend::Auto if cfg!(target_os = "macos") => Ok(&["-cR"]),
        Backend::Auto if cfg!(target_os = "linux") => Ok(&["--reflink=auto", "-R"]),
        Backend::Auto => Ok(&["-R"]),
        Backend::Apfs | Backend::BlockClone if cfg!(target_os = "macos") => Ok(&["-cR"]),
        Backend::BlockClone if cfg!(windows) => bail!("block-clone copies need Windows FSCTL_DUPLICATE_EXTENTS_TO_FILE; use auto or rcopy here"),
        // A btrfs or ZFS clone within one dataset is a reflink; a snapshot needs both paths in one subvolume.
        Backend::Reflink | Backend::Btrfs | Backend::Zfs if cfg!(target_os = "linux") => Ok(&["--reflink=always", "-R"]),
        Backend::Overlayfs if cfg!(target_os = "linux") => Ok(&["--reflink=auto", "-R"]),
        Backend::Projfs if cfg!(windows) => Ok(&["-R"]),
        Backend::Rcopy => Ok(&["-R"]),
        other => bail!("isolation.backend {other:?} cannot copy on this platform; use auto, apfs or rcopy"),
    }
}

fn copy_entry(backend: crate::config::isolation::Backend, from: &Path, to: &Path) -> Result<()> {
    let out = Command::new("cp").args(copy_flags(backend)?).arg(from).arg(to).output()?;
    if !out.status.success() { bail!("cp: {}", String::from_utf8_lossy(&out.stderr).trim()); }
    Ok(())
}

/// Creates a worktree of `repo` at exactly `path` on `branch`.
pub fn create_at_with(repo: &Path, path: &Path, branch: &str) -> Result<()> {
    git(repo, &["worktree", "add", "-b", branch, &path.to_string_lossy()])?;
    Ok(())
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

/// Whether `dir` is inside a git work tree.
pub fn is_repo(dir: &Path) -> bool {
    git(dir, &["rev-parse", "--is-inside-work-tree"]).is_ok_and(|out| out.trim() == "true")
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

/// Everything in `dir` against its HEAD as one applicable patch: untracked and binary files included.
pub fn capture_patch(dir: &Path) -> Result<String> {
    git(dir, &["add", "-A"])?;
    git(dir, &["diff", "--cached", "--binary", "HEAD"])
}

/// Applies `patch` to `repo`'s working tree; `--3way` also handles a parent that moved a little.
pub fn apply_patch(repo: &Path, patch: &str) -> Result<()> {
    if patch.trim().is_empty() { return Ok(()); }
    let mut child = Command::new("git")
        .arg("-C").arg(repo).args(["apply", "--3way", "--whitespace=nowarn", "-"])
        .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped())
        .spawn()?;
    child.stdin.take().expect("piped stdin").write_all(patch.as_bytes())?;
    let out = child.wait_with_output()?;
    if !out.status.success() { bail!("git apply --3way: {}", String::from_utf8_lossy(&out.stderr).trim()); }
    Ok(())
}

/// Commits everything in a task worktree; false when there was nothing to commit.
pub fn commit_all(dir: &Path, message: &str) -> Result<bool> {
    git(dir, &["add", "-A"])?;
    if git(dir, &["status", "--porcelain"])?.trim().is_empty() { return Ok(false); }
    git(dir, &["-c", "user.name=GreenCode", "-c", "user.email=greencode@localhost", "commit", "-q", "-m", message])?;
    Ok(true)
}

/// Merges a task branch into `repo`'s checkout; a conflict aborts the merge so the checkout stays usable.
pub fn merge_branch(repo: &Path, branch: &str, message: &str) -> Result<()> {
    if let Err(e) = git(repo, &["merge", "--no-ff", "-m", message, branch]) {
        let _ = git(repo, &["merge", "--abort"]);
        return Err(e);
    }
    Ok(())
}

/// Removes a worktree whose changes were integrated and deletes its branch.
pub fn remove_integrated(repo: &Path, path: &Path, branch: &str) -> Result<()> {
    git(repo, &["worktree", "remove", "--force", &path.to_string_lossy()])?;
    let _ = git(repo, &["branch", "-D", branch]);
    Ok(())
}

/// Where an unapplied isolated patch is written.
pub fn patch_file(base: &Path, id: &str) -> PathBuf { base.join(format!("{id}.patch")) }

/// A worktree this launch created for an interactive session (`worktree.onStart`).
pub struct SessionWorktree {
    pub repo: PathBuf,
    pub path: PathBuf,
    pub branch: String,
    pub initial: String,
}

impl SessionWorktree {
    /// Removes the worktree and its branch only when nothing would be lost.
    pub fn remove_if_pristine(&self) -> Result<()> {
        remove_pristine_task(&self.repo, &self.path, &self.branch, &self.initial)
    }

    /// `worktree.onExit` against the worktree's current state; `Prompt` means there is something to keep.
    pub fn exit_action(&self, policy: OnExit) -> ExitAction {
        match policy {
            OnExit::Keep => return ExitAction::Keep,
            OnExit::Ask => return ExitAction::Prompt,
            OnExit::Remove => {}
        }
        let clean = git(&self.path, &["status", "--porcelain"]).is_ok_and(|s| s.trim().is_empty());
        let on_branch = git(&self.path, &["symbolic-ref", "-q", "HEAD"]).is_ok_and(|s| s.trim() == format!("refs/heads/{}", self.branch));
        let moved = git(&self.path, &["rev-parse", "HEAD"]).is_ok_and(|s| s.trim() != self.initial);
        if clean && on_branch && !moved { ExitAction::Remove } else { ExitAction::Prompt }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExitAction { Keep, Remove, Prompt }

/// Forks `repo` into a linked worktree on a `wt/<id>` branch under `worktree.base`.
/// `worktree.cleanSource` carries tracked changes into the new worktree, then resets them and removes
/// untracked files from `repo`; with it off the source checkout is left exactly as it was.
pub fn create_session(repo: &Path, cfg: &WorktreeCfg, id: &str, backend: crate::config::isolation::Backend) -> Result<SessionWorktree> {
    let initial = head(repo)?;
    let carried = cfg.clean_source.then(|| capture_patch(repo)).transpose()?;
    let options = Options { prefix: "wt/".into(), clone: cfg.clone, backend, ..Options::default() };
    let path = create_with(repo, &cfg.dir(), id, &options)?;
    if let Some(patch) = carried {
        apply_patch(&path, &patch)?;
        git(repo, &["reset", "--hard"])?;
        git(repo, &["clean", "-fd"])?;
    }
    Ok(SessionWorktree { repo: repo.to_path_buf(), path, branch: format!("wt/{id}"), initial })
}

/// OMP key → native dotted key; `config import-omp` copies the value verbatim.
pub(crate) const OMP: &[(&str, &str)] = &[
    ("worktree.base", "worktree.base"),
    ("worktree.clone", "worktree.clone"),
    ("worktree.onStart", "worktree.on_start"),
    ("worktree.onExit", "worktree.on_exit"),
    ("worktree.cleanSource", "worktree.clean_source"),
];

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

#[cfg(test)]
mod submodule_tests {
    use super::*;

    #[test]
    fn new_worktrees_initialize_submodules_by_policy() {
        // Git refuses local-path submodules by default; this only widens that for the test's processes.
        std::env::set_var("GIT_CONFIG_COUNT", "1");
        std::env::set_var("GIT_CONFIG_KEY_0", "protocol.file.allow");
        std::env::set_var("GIT_CONFIG_VALUE_0", "always");
        let root = std::env::temp_dir().join(format!("greencode-submodule-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let (sub, repo) = (root.join("sub"), root.join("repo"));
        let run = |dir: &Path, args: &[&str]| {
            let out = Command::new("git").arg("-C").arg(dir).args(["-c", "user.name=t", "-c", "user.email=t@example.com"]).args(args).output().unwrap();
            assert!(out.status.success(), "{args:?}: {}", String::from_utf8_lossy(&out.stderr));
        };
        for dir in [&sub, &repo] {
            std::fs::create_dir_all(dir).unwrap();
            run(dir, &["init", "-q"]);
            std::fs::write(dir.join("file"), "x").unwrap();
            run(dir, &["add", "."]);
            run(dir, &["commit", "-qm", "init"]);
        }
        run(&repo, &["submodule", "add", "-q", sub.to_str().unwrap(), "vendor"]);
        run(&repo, &["commit", "-qm", "add submodule"]);
        let options = |submodules| Options { prefix: "work/".into(), submodules, clone: false, ..Default::default() };
        let full = create_with(&repo, &root.join("trees"), "a", &options(Submodules::Recursive)).unwrap();
        assert!(full.join("vendor/file").exists());
        assert_eq!(git(&full, &["branch", "--show-current"]).unwrap().trim(), "work/a");
        let top = create_with(&repo, &root.join("trees"), "b", &options(Submodules::TopLevel)).unwrap();
        assert!(top.join("vendor/file").exists());
        let skipped = create_with(&repo, &root.join("trees"), "c", &options(Submodules::Skip)).unwrap();
        assert!(!skipped.join("vendor/file").exists());
        std::fs::remove_dir_all(root).unwrap();
    }
}

#[cfg(test)]
mod config_tests {
    use super::*;

    fn repo(root: &Path, name: &str) -> PathBuf {
        let project = root.join(name);
        std::fs::create_dir_all(&project).unwrap();
        let run = |dir: &Path, args: &[&str]| {
            let out = Command::new("git").arg("-C").arg(dir).args(["-c", "user.name=t", "-c", "user.email=t@example.com"]).args(args).output().unwrap();
            assert!(out.status.success(), "{args:?}: {}", String::from_utf8_lossy(&out.stderr));
        };
        run(&project, &["init", "-q"]);
        std::fs::write(project.join("tracked"), "one").unwrap();
        run(&project, &["add", "."]);
        run(&project, &["commit", "-qm", "initial"]);
        project
    }

    #[test]
    fn base_ignores_relative_paths_and_expands_home() {
        let cfg = |base: Option<&str>| WorktreeCfg { base: base.map(String::from), ..WorktreeCfg::default() };
        let fallback = crate::home().join("subagent-worktrees");
        assert_eq!(cfg(None).dir(), fallback);
        assert_eq!(cfg(Some("  ")).dir(), fallback);
        assert_eq!(cfg(Some("relative/trees")).dir(), fallback);
        assert_eq!(cfg(Some("/tmp/wt-test")).dir(), PathBuf::from("/tmp/wt-test"));
        assert_eq!(cfg(Some("~/trees")).dir(), crate::user_home().join("trees"));
    }

    #[test]
    fn isolated_changes_apply_as_patch_or_merge_as_branch() {
        let root = std::env::temp_dir().join(format!("greencode-isolation-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let patch_repo = repo(&root, "patch");
        let tree = create(&patch_repo, &root.join("worktrees"), "p").unwrap();
        std::fs::write(tree.join("tracked"), "two").unwrap();
        std::fs::write(tree.join("added"), "new").unwrap();
        let patch = capture_patch(&tree).unwrap();
        assert!(patch.contains("added") && patch.contains("tracked"));
        apply_patch(&patch_repo, &patch).unwrap();
        assert_eq!(std::fs::read_to_string(patch_repo.join("tracked")).unwrap(), "two");
        assert_eq!(std::fs::read_to_string(patch_repo.join("added")).unwrap(), "new");

        let branch_repo = repo(&root, "branch");
        let tree = create(&branch_repo, &root.join("worktrees"), "b").unwrap();
        std::fs::write(tree.join("tracked"), "three").unwrap();
        assert!(commit_all(&tree, "task b: isolated changes").unwrap());
        assert!(!commit_all(&tree, "again").unwrap());
        merge_branch(&branch_repo, "greencode/b", "task b: isolated changes").unwrap();
        assert_eq!(std::fs::read_to_string(branch_repo.join("tracked")).unwrap(), "three");
        remove_integrated(&branch_repo, &tree, "greencode/b").unwrap();
        assert!(!tree.exists());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn clean_source_carries_changes_then_resets_the_source() {
        let root = std::env::temp_dir().join(format!("greencode-clean-source-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let project = repo(&root, "repo");
        let base = Some(root.join("wt").to_string_lossy().into_owned());
        std::fs::write(project.join("tracked"), "dirty").unwrap();
        std::fs::write(project.join("scratch"), "new file").unwrap();
        let clean = WorktreeCfg { base: base.clone(), clean_source: true, ..WorktreeCfg::default() };
        let tree = create_session(&project, &clean, "dirty", crate::config::isolation::Backend::Auto).unwrap();
        assert_eq!(std::fs::read_to_string(tree.path.join("tracked")).unwrap(), "dirty", "tracked changes move into the new worktree");
        assert_eq!(std::fs::read_to_string(tree.path.join("scratch")).unwrap(), "new file", "untracked files move too");
        assert_eq!(std::fs::read_to_string(project.join("tracked")).unwrap(), "one", "the source is reset to HEAD");
        assert!(!project.join("scratch").exists(), "untracked files leave the source");
        assert!(git(&project, &["status", "--porcelain"]).unwrap().trim().is_empty());

        std::fs::write(project.join("tracked"), "kept").unwrap();
        std::fs::write(project.join("scratch2"), "kept").unwrap();
        let keep = WorktreeCfg { base, clean_source: false, ..WorktreeCfg::default() };
        let other = create_session(&project, &keep, "keep", crate::config::isolation::Backend::Auto).unwrap();
        assert_eq!(std::fs::read_to_string(other.path.join("tracked")).unwrap(), "one", "off leaves the source's changes where they were");
        assert_eq!(std::fs::read_to_string(project.join("tracked")).unwrap(), "kept");
        assert!(project.join("scratch2").exists());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn session_exit_removes_only_pristine_worktrees() {
        let root = std::env::temp_dir().join(format!("greencode-session-wt-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let project = repo(&root, "repo");
        let cfg = WorktreeCfg { base: Some(root.join("wt").to_string_lossy().into_owned()), ..WorktreeCfg::default() };
        let tree = create_session(&project, &cfg, "s1", crate::config::isolation::Backend::Auto).unwrap();
        assert_eq!(tree.exit_action(OnExit::Keep), ExitAction::Keep);
        assert_eq!(tree.exit_action(OnExit::Ask), ExitAction::Prompt);
        assert_eq!(tree.exit_action(OnExit::Remove), ExitAction::Remove);
        std::fs::write(tree.path.join("tracked"), "dirty").unwrap();
        assert_eq!(tree.exit_action(OnExit::Remove), ExitAction::Prompt);
        std::fs::write(tree.path.join("tracked"), "one").unwrap();
        tree.remove_if_pristine().unwrap();
        assert!(!tree.path.exists());
        assert!(!cfg.dir().join("s1").exists());

        let tree = create_session(&project, &cfg, "s2", crate::config::isolation::Backend::Auto).unwrap();
        std::fs::write(tree.path.join("tracked"), "committed").unwrap();
        assert!(commit_all(&tree.path, "work").unwrap());
        assert_eq!(tree.exit_action(OnExit::Remove), ExitAction::Prompt, "a moved branch is kept");
        assert!(tree.remove_if_pristine().is_err());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn cloning_carries_ignored_artifacts_into_new_worktrees() {
        let root = std::env::temp_dir().join(format!("greencode-clone-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let project = repo(&root, "repo");
        std::fs::create_dir_all(project.join("cache")).unwrap();
        std::fs::write(project.join("cache/build"), "artifact").unwrap();
        std::fs::write(project.join(".gitignore"), "cache/\n").unwrap();
        let with = create_with(&project, &root.join("trees"), "cloned", &Options { prefix: "greencode/".into(), clone: true, ..Options::default() }).unwrap();
        let without = create_with(&project, &root.join("trees"), "plain", &Options { prefix: "greencode/".into(), clone: false, ..Options::default() }).unwrap();
        assert_eq!(std::fs::read_to_string(with.join("cache/build")).unwrap(), "artifact");
        assert!(!without.join("cache").exists());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn omp_worktree_rows_import() {
        let table = crate::config::settings::translate("worktree:\n  base: /tmp/wt\n  clone: false\n  onStart: create\n  onExit: remove\n").unwrap();
        let cfg = crate::config::settings::decode(toml::Value::Table(table)).unwrap();
        assert_eq!(cfg.worktree.base.as_deref(), Some("/tmp/wt"));
        assert!(!cfg.worktree.clone);
        assert_eq!(cfg.worktree.on_start, OnStart::Create);
        assert_eq!(cfg.worktree.on_exit, OnExit::Remove);
        // A null OMP value means "unset" and is not imported.
        assert!(crate::config::settings::translate("worktree:\n  base: null\n").unwrap().get("worktree").and_then(|t| t.get("base")).is_none());
        assert!(crate::config::settings::decode(toml::Value::Table(crate::config::settings::translate("worktree:\n  onExit: later\n").unwrap())).is_err());
    }
}
