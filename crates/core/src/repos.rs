//! Projects that cover several Git repositories: discovery, per-repo status, grouped diffs,
//! mirrored worktrees and checkpoints. Everything is read from disk on each call.

use anyhow::{bail, Result};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    process::Command,
};

/// Most folders a project may cover, including added ones.
pub const MAX_REPOS: usize = 128;
/// Worktrees are created for every repository up to this many; beyond it the user must choose.
pub const AUTO_SELECT_MAX: usize = 8;

#[derive(Clone)]
pub struct RepoInfo {
    pub path: PathBuf,
    pub branch: String,
    pub dirty: usize,
}

fn git(dir: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git").arg("-C").arg(dir).args(args).stderr(std::process::Stdio::null()).output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

fn is_repo(dir: &Path) -> bool {
    dir.join(".git").exists()
}

/// A `.git` file pointing into `worktrees/` is a linked worktree of another repository; `modules/` is a submodule.
fn linked_worktree(dir: &Path) -> bool {
    std::fs::read_to_string(dir.join(".git"))
        .is_ok_and(|s| s.starts_with("gitdir:") && s.contains("/worktrees/") && !s.contains("/modules/"))
}

fn children(dir: &Path) -> Vec<PathBuf> {
    let mut v: Vec<_> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()) && !e.file_name().to_string_lossy().starts_with('.'))
        .map(|e| e.path())
        .collect();
    v.sort();
    v
}

/// Relative paths named by `ignoreRepositories` in the project's `greencode.json` or `t3.json`.
pub fn ignored(root: &Path) -> Vec<PathBuf> {
    ["greencode.json", "t3.json"]
        .iter()
        .filter_map(|f| serde_json::from_str::<serde_json::Value>(&std::fs::read_to_string(root.join(f)).ok()?).ok())
        .flat_map(|v| v["ignoreRepositories"].as_array().cloned().unwrap_or_default())
        .filter_map(|p| p.as_str().map(|p| root.join(p.trim_end_matches('/'))))
        .collect()
}

fn submodules(repo: &Path) -> Vec<PathBuf> {
    std::fs::read_to_string(repo.join(".gitmodules"))
        .unwrap_or_default()
        .lines()
        .filter_map(|l| l.trim().strip_prefix("path"))
        .filter_map(|l| l.trim_start().strip_prefix('='))
        .map(|p| repo.join(p.trim()))
        .filter(|p| is_repo(p))
        .collect()
}

/// Repositories of a project: the folder itself, children one level down, one further level inside
/// folders that are not repositories, submodules, and the `extra` folders added by hand.
/// `include_linked` keeps worktrees of other repositories, for a thread's own worktree folder.
pub fn discover(root: &Path, extra: &[PathBuf], include_linked: bool) -> Vec<PathBuf> {
    let skip = ignored(root);
    let ok = |p: &Path| is_repo(p) && !skip.iter().any(|s| s == p) && (include_linked || !linked_worktree(p));
    let mut found = Vec::new();
    if ok(root) {
        found.push(root.to_path_buf());
        found.extend(submodules(root));
    }
    for child in children(root) {
        if ok(&child) {
            found.push(child.clone());
            found.extend(submodules(&child));
        } else if !is_repo(&child) && !skip.contains(&child) {
            for grand in children(&child) {
                if ok(&grand) {
                    found.push(grand.clone());
                    found.extend(submodules(&grand));
                }
            }
        }
    }
    found.extend(extra.iter().filter(|p| is_repo(p)).cloned());
    let mut seen = std::collections::HashSet::new();
    found.retain(|p| seen.insert(p.clone()));
    found.truncate(MAX_REPOS);
    found
}

/// Folders listed by a `.code-workspace` file in `root` that lie outside it and are not added yet.
/// shortcut: plain JSON only (no comments), so a commented file yields nothing.
pub fn workspace_suggestions(root: &Path, added: &[PathBuf]) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let base = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    for file in std::fs::read_dir(root).into_iter().flatten().flatten().map(|e| e.path()) {
        if file.extension().is_none_or(|e| e != "code-workspace") { continue; }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&std::fs::read_to_string(&file).unwrap_or_default()) else { continue };
        for f in v["folders"].as_array().into_iter().flatten().filter_map(|f| f["path"].as_str()) {
            let p = root.join(f);
            let p = p.canonicalize().unwrap_or(p);
            if !p.starts_with(&base) && is_repo(&p) && !added.iter().any(|a| a.canonicalize().unwrap_or_else(|_| a.clone()) == p) && !out.contains(&p) { out.push(p); }
        }
    }
    out
}

pub fn info(repo: &Path) -> RepoInfo {
    RepoInfo {
        path: repo.to_path_buf(),
        branch: git(repo, &["branch", "--show-current"]).unwrap_or_default().trim().to_string(),
        dirty: git(repo, &["status", "--porcelain"]).unwrap_or_default().lines().count(),
    }
}

/// Name shown for `repo`: its path relative to `root`, the folder name for the root itself or an outside folder.
pub fn label(root: &Path, repo: &Path) -> String {
    match repo.strip_prefix(root) {
        Ok(r) if r.as_os_str().is_empty() => root.file_name().map_or(".".into(), |n| n.to_string_lossy().into()),
        Ok(r) => r.to_string_lossy().into(),
        Err(_) => repo.to_string_lossy().into(),
    }
}

/// Rewrites the file headers of a unified diff so paths carry the `prefix/` of their repository.
/// shortcut: a hunk line that itself starts with `--- a/` is rewritten too; rare, cosmetic.
pub fn prefix_diff(diff: &str, prefix: &str) -> String {
    let mut out = String::new();
    for l in diff.lines() {
        let line = if let Some(rest) = l.strip_prefix("diff --git a/") {
            match rest.split_once(" b/") {
                Some((a, b)) => format!("diff --git a/{prefix}/{a} b/{prefix}/{b}"),
                None => l.to_string(),
            }
        } else if let Some(r) = l.strip_prefix("--- a/") {
            format!("--- a/{prefix}/{r}")
        } else if let Some(r) = l.strip_prefix("+++ b/") {
            format!("+++ b/{prefix}/{r}")
        } else {
            l.to_string()
        };
        out.push_str(&line);
        out.push('\n');
    }
    out
}

/// Path prefix of `repo`'s files in a grouped diff: its place under `root`, or its folder name when
/// it was added from outside the project.
fn diff_prefix(root: &Path, repo: &Path) -> String {
    match repo.strip_prefix(root) {
        Ok(r) if !r.as_os_str().is_empty() => r.to_string_lossy().into_owned(),
        Ok(_) => String::new(),
        Err(_) => repo.file_name().map_or("external".into(), |n| n.to_string_lossy().into_owned()),
    }
}

/// Uncommitted changes of every repository, one `## name` section each, with prefixed paths.
pub fn diff(root: &Path, repos: &[PathBuf]) -> String {
    let mut out = String::new();
    for r in repos {
        let d = crate::worktree::diff(r);
        if d.is_empty() { continue; }
        let name = label(root, r);
        out.push_str(&format!("## {name}\n"));
        let prefix = diff_prefix(root, r);
        out.push_str(&if prefix.is_empty() { d } else { prefix_diff(&d, &prefix) });
    }
    out
}

/// Repositories that get a worktree of their own: all but submodules, which their parent decides where
/// to check out. A separate repository nested inside another still gets one.
fn top_level(repos: &[PathBuf]) -> Vec<&PathBuf> {
    let nested: Vec<PathBuf> = repos.iter().flat_map(|r| submodules(r)).collect();
    repos.iter().filter(|r| !nested.contains(r)).collect()
}

/// Whether a thread over `repos` needs the mirrored layout rather than one plain worktree of `project`.
pub fn is_multi(project: &Path, repos: &[PathBuf]) -> bool {
    let top = top_level(repos);
    !(top.len() == 1 && top[0] == project) && !top.is_empty()
}

/// Repositories to give a worktree: `chosen` when set, else all up to `AUTO_SELECT_MAX`, else none.
pub fn default_selection(repos: &[PathBuf], chosen: Option<&[PathBuf]>) -> Vec<PathBuf> {
    let top: Vec<_> = top_level(repos).into_iter().cloned().collect();
    match chosen {
        Some(c) => top.into_iter().filter(|r| c.contains(r)).collect(),
        None if top.len() <= AUTO_SELECT_MAX => top,
        None => vec![],
    }
}

/// Creates a thread folder `base/id/<project name>` mirroring `project`, with one worktree per
/// selected repository on `options.prefix + id`. Non-repository files at the top level are symlinked.
/// Repositories without a commit are returned as skipped. Failures roll back what was made.
pub fn create_worktrees(project: &Path, all: &[PathBuf], selected: &[PathBuf], base: &Path, id: &str, options: &crate::worktree::Options) -> Result<(PathBuf, Vec<PathBuf>)> {
    let thread = base.join(id);
    let root = thread.join(project.file_name().unwrap_or_default());
    std::fs::create_dir_all(&root)?;
    let mut made: Vec<(PathBuf, PathBuf)> = Vec::new();
    let mut skipped = Vec::new();
    let result = (|| -> Result<()> {
        for repo in top_level(selected) {
            if git(repo, &["rev-parse", "--verify", "-q", "HEAD"]).is_none() { skipped.push(repo.clone()); continue; }
            let dest = match repo.strip_prefix(project) {
                Ok(rel) => root.join(rel),
                // Kept inside the mirrored root so the thread finds it by discovery like the rest.
                Err(_) => root.join("_external").join(repo.file_name().unwrap_or_default()),
            };
            if let Some(p) = dest.parent() { std::fs::create_dir_all(p)?; }
            crate::worktree::create_at_with(repo, &dest, &format!("{}{id}", options.prefix))?;
            made.push((repo.clone(), dest.clone()));
            // Best effort: a failed submodule checkout leaves it uninitialized.
            let submodules: &[&str] = match options.submodules {
                crate::worktree::Submodules::Skip => &[],
                crate::worktree::Submodules::TopLevel => &["submodule", "update", "--init"],
                crate::worktree::Submodules::Recursive => &["submodule", "update", "--init", "--recursive"],
            };
            if !submodules.is_empty() {
                let _ = git(&dest, submodules);
            }
        }
        #[cfg(unix)]
        for entry in std::fs::read_dir(project)?.flatten() {
            let path = entry.path();
            let name = entry.file_name();
            if name == ".git" || root.join(&name).exists() || all.iter().any(|r| r.starts_with(&path)) { continue; }
            std::os::unix::fs::symlink(&path, root.join(&name))?;
        }
        Ok(())
    })();
    if let Err(e) = result {
        for (repo, dest) in made.iter().rev() {
            let _ = crate::worktree::remove_worktree(repo, dest);
            let _ = crate::worktree::git(repo, &["branch", "-D", &format!("{}{id}", options.prefix)]);
        }
        let _ = std::fs::remove_dir_all(&thread);
        bail!("{e:#}");
    }
    Ok((root, skipped))
}

/// Checkpoint of every repository, keyed by path.
pub fn checkpoint(repos: &[PathBuf]) -> BTreeMap<String, String> {
    repos.iter().filter_map(|r| Some((r.to_string_lossy().into_owned(), crate::checkpoint::create(r)?))).collect()
}

/// Restores each repository that has a snapshot. Returns the restored file count and the repositories
/// (or the plain project folder) that could not be restored.
pub fn restore(root: &Path, repos: &[PathBuf], snaps: &BTreeMap<String, String>) -> (usize, Vec<PathBuf>) {
    let (mut count, mut skipped) = (0, Vec::new());
    if !is_repo(root) { skipped.push(root.to_path_buf()); }
    for r in repos {
        match snaps.get(r.to_string_lossy().as_ref()).map(|s| crate::checkpoint::restore(r, s)) {
            Some(Ok(n)) => count += n,
            _ => skipped.push(r.clone()),
        }
    }
    (count, skipped)
}

/// Tracked diff of every repository against its snapshot, with prefixed paths.
pub fn checkpoint_diff(root: &Path, repos: &[PathBuf], snaps: &BTreeMap<String, String>) -> String {
    let mut out = String::new();
    for r in repos {
        let Some(Ok(d)) = snaps.get(r.to_string_lossy().as_ref()).map(|s| crate::checkpoint::diff(r, s)) else { continue };
        if d.is_empty() { continue; }
        let name = label(root, r);
        out.push_str(&format!("## {name}\n"));
        let prefix = diff_prefix(root, r);
        out.push_str(&if prefix.is_empty() { d } else { prefix_diff(&d, &prefix) });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(dir: &Path, args: &[&str]) {
        let out = Command::new("git").arg("-C").arg(dir)
            .args(["-c", "user.name=t", "-c", "user.email=t@e", "-c", "protocol.file.allow=always"]).args(args).output().unwrap();
        assert!(out.status.success(), "{args:?}: {}", String::from_utf8_lossy(&out.stderr));
    }

    fn repo(dir: &Path) {
        std::fs::create_dir_all(dir).unwrap();
        run(dir, &["init", "-q"]);
        std::fs::write(dir.join("f"), "one\n").unwrap();
        run(dir, &["add", "."]);
        run(dir, &["commit", "-qm", "init"]);
    }

    #[test]
    fn discovers_nested_repos_and_skips_worktrees_and_ignored() {
        let root = std::env::temp_dir().join(format!("greencode-repos-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        // The submodule source is a local path, which git refuses by default.
        std::env::set_var("GIT_CONFIG_COUNT", "1");
        std::env::set_var("GIT_CONFIG_KEY_0", "protocol.file.allow");
        std::env::set_var("GIT_CONFIG_VALUE_0", "always");
        let shop = root.join("shop");
        let (api, web, big, sub_src) = (shop.join("api"), shop.join("packages/web"), shop.join("vendor/big"), root.join("sub-src"));
        for r in [&api, &web, &big, &sub_src] { repo(r); }
        run(&api, &["submodule", "add", "-q", &sub_src.to_string_lossy(), "libs"]);
        run(&api, &["commit", "-qm", "sub"]);
        run(&api, &["worktree", "add", "-q", &shop.join("api-wt").to_string_lossy()]);
        std::fs::write(shop.join("t3.json"), r#"{"ignoreRepositories":["vendor/big"]}"#).unwrap();
        std::fs::write(shop.join("README"), "x").unwrap();

        let found = discover(&shop, &[], false);
        assert_eq!(found, vec![api.clone(), api.join("libs"), web.clone()]);
        assert!(discover(&shop, &[], true).contains(&shop.join("api-wt")));
        let outside = root.join("outside");
        repo(&outside);
        assert!(discover(&shop, &[outside.clone()], false).contains(&outside));
        std::fs::write(shop.join("shop.code-workspace"), r#"{"folders":[{"path":"."},{"path":"../outside"}]}"#).unwrap();
        assert_eq!(workspace_suggestions(&shop, &[]), vec![outside.canonicalize().unwrap()]);
        assert!(workspace_suggestions(&shop, &[outside.clone()]).is_empty(), "already added folders are not offered");

        std::fs::write(web.join("f"), "two\n").unwrap();
        assert_eq!(info(&web).dirty, 1);
        let d = diff(&shop, &found);
        assert!(d.contains("## packages/web") && d.contains("+++ b/packages/web/f"));

        let selected = default_selection(&found, None);
        assert_eq!(selected, vec![api.clone(), web.clone()]);
        assert!(is_multi(&shop, &found));
        let all: Vec<_> = found.iter().chain([&outside]).cloned().collect();
        let picked: Vec<_> = selected.iter().chain([&outside]).cloned().collect();
        let (thread, skipped) = create_worktrees(&shop, &all, &picked, &root.join("trees"), "t1", &crate::worktree::Options::default()).unwrap();
        assert!(skipped.is_empty() && thread.join("api/f").exists() && thread.join("packages/web/f").exists());
        assert!(thread.join("README").exists() && thread.join("vendor").is_symlink());
        assert!(thread.join("_external/outside/f").exists(), "outside folder mirrored inside the thread");
        assert!(thread.join("api/libs/f").exists(), "submodule checked out");
        let inside = discover(&thread, &[], true);
        assert!(inside.contains(&thread.join("api")) && !discover(&thread, &[], false).contains(&thread.join("api")));
        assert!(inside.contains(&thread.join("_external/outside")), "thread reaches the added folder");
        assert!(inside.iter().all(|p| !p.ends_with("big")), "an ignored repository gets no worktree");

        let snaps = checkpoint(&found);
        std::fs::write(web.join("f"), "three\n").unwrap();
        let d = checkpoint_diff(&shop, &found, &snaps);
        assert!(d.contains("## packages/web") && d.contains("b/packages/web/f"));
        let (n, skipped) = restore(&shop, &found, &snaps);
        assert_eq!(n, 1);
        assert!(skipped.contains(&shop), "plain folder reported");
        let _ = std::fs::remove_dir_all(&root);
    }
}
