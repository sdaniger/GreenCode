//! Source-control helpers: diff sources, default-branch sync, clone / new project / publish, PR merge text.
use crate::worktree::git;
use anyhow::{bail, ensure, Result};
use std::{
    path::{Path, PathBuf},
    process::Command,
};

/// What the diff panel shows.
#[derive(Clone, Debug, Default, PartialEq)]
pub enum DiffSource {
    /// Staged, unstaged and new files.
    #[default]
    Uncommitted,
    /// Everything since the branch left the default branch.
    Base,
    /// One commit (first-parent view of a merge).
    Commit(String),
}

pub fn diff_for(dir: &Path, source: &DiffSource) -> String {
    let result = match source {
        DiffSource::Uncommitted => return crate::worktree::diff(dir),
        DiffSource::Base => diff_vs_base(dir),
        DiffSource::Commit(sha) => commit_diff(dir, sha),
    };
    result.unwrap_or_else(|e| format!("{e:#}"))
}

/// The default branch: `origin/HEAD` if set, else a local `main` or `master`.
pub fn default_branch(dir: &Path) -> Option<String> {
    if let Ok(s) = git(dir, &["symbolic-ref", "--short", "refs/remotes/origin/HEAD"]) {
        if let Some(b) = s.trim().strip_prefix("origin/") {
            return Some(b.to_string());
        }
    }
    ["main", "master"]
        .into_iter()
        .find(|b| git(dir, &["rev-parse", "--verify", "-q", &format!("refs/heads/{b}")]).is_ok())
        .map(String::from)
}

/// Changes since the merge-base with the default branch, including uncommitted edits to tracked files.
pub fn diff_vs_base(dir: &Path) -> Result<String> {
    let Some(base) = default_branch(dir) else { bail!("ベースブランチが見つかりません") };
    let target = if git(dir, &["rev-parse", "--verify", "-q", &format!("refs/heads/{base}")]).is_ok() {
        base.clone()
    } else {
        format!("origin/{base}")
    };
    let merge_base = git(dir, &["merge-base", "HEAD", &target])?;
    let diff = git(dir, &["diff", merge_base.trim()])?;
    Ok(if diff.is_empty() { format!("{base} との差分はありません") } else { diff })
}

/// The newest `limit` commits on the first-parent line as `(short sha, subject)`.
pub fn recent_commits(dir: &Path, limit: usize) -> Vec<(String, String)> {
    git(dir, &["log", "--first-parent", &format!("-{limit}"), "--format=%h%x09%s"])
        .unwrap_or_default()
        .lines()
        .filter_map(|l| l.split_once('\t'))
        .map(|(sha, subject)| (sha.to_string(), subject.to_string()))
        .collect()
}

pub fn commit_diff(dir: &Path, sha: &str) -> Result<String> {
    ensure!(!sha.starts_with('-'), "不正なコミット");
    git(dir, &["show", "--first-parent", "--format=%h  %s%n%an · %ad%n", sha])
}

#[derive(Debug, PartialEq)]
pub enum Sync {
    UpToDate,
    Updated(String),
    Skipped(&'static str),
}

/// Fetches `origin` and fast-forwards the default branch. A branch checked out with local changes,
/// or one that has diverged from origin, is left alone.
pub fn sync_default_branch(repo: &Path) -> Result<Sync> {
    let Some(branch) = default_branch(repo) else { return Ok(Sync::Skipped("既定ブランチが見つかりません")) };
    if git(repo, &["remote", "get-url", "origin"]).is_err() {
        return Ok(Sync::Skipped("origin がありません"));
    }
    let out = Command::new("git")
        .arg("-C").arg(repo)
        .args(["fetch", "--quiet", "origin", &branch])
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()?;
    ensure!(out.status.success(), "git fetch: {}", String::from_utf8_lossy(&out.stderr).trim());
    let (local_ref, remote_ref) = (format!("refs/heads/{branch}"), format!("refs/remotes/origin/{branch}"));
    let (Ok(local), Ok(remote)) = (git(repo, &["rev-parse", &local_ref]), git(repo, &["rev-parse", &remote_ref])) else {
        return Ok(Sync::Skipped("ブランチが見つかりません"));
    };
    let (local, remote) = (local.trim(), remote.trim());
    if local == remote {
        return Ok(Sync::UpToDate);
    }
    if git(repo, &["merge-base", "--is-ancestor", local, remote]).is_err() {
        return Ok(Sync::Skipped("ローカルが origin と分岐しています"));
    }
    match checked_out_at(repo, &local_ref) {
        Some(path) => {
            if !git(&path, &["status", "--porcelain", "--untracked-files=no"])?.is_empty() {
                return Ok(Sync::Skipped("チェックアウト中で未コミットの変更があります"));
            }
            git(&path, &["merge", "--ff-only", "--quiet", remote])?;
        }
        // Compare-and-swap: nothing moves the branch if it changed meanwhile.
        None => {
            git(repo, &["update-ref", &local_ref, remote, local])?;
        }
    }
    Ok(Sync::Updated(remote.chars().take(8).collect()))
}

/// The worktree that has `reference` checked out, if any.
fn checked_out_at(repo: &Path, reference: &str) -> Option<PathBuf> {
    let list = git(repo, &["worktree", "list", "--porcelain"]).ok()?;
    let mut path = None;
    for line in list.lines() {
        if let Some(p) = line.strip_prefix("worktree ") {
            path = Some(PathBuf::from(p));
        } else if line.strip_prefix("branch ") == Some(reference) {
            return path;
        }
    }
    None
}

/// Whether `text` credits a coding agent rather than a person.
fn agent_name(text: &str) -> bool {
    let t = text.to_lowercase();
    ["claude", "anthropic", "codex", "openai", "copilot", "cursor", "gemini", "aider", "devin", "greencode", "opencode", "chatgpt"]
        .iter()
        .any(|a| t.contains(a))
}

/// Removes agent co-author and "Generated with/by" lines from a commit message; human co-authors stay.
pub fn strip_agent_credits(message: &str) -> String {
    let kept: Vec<&str> = message
        .lines()
        .filter(|l| {
            let line = l.trim().trim_start_matches(|c: char| !c.is_alphanumeric()).to_lowercase();
            let credit = (line.starts_with("co-authored-by:") || line.starts_with("generated with") || line.starts_with("generated by"))
                && agent_name(&line);
            !credit
        })
        .collect();
    kept.join("\n").trim_end().to_string()
}

/// GitHub's default squash message for a PR: the lone commit's body, else one bullet per commit.
pub fn squash_body(commits: &[(String, String)]) -> String {
    match commits {
        [(_, body)] => body.trim().to_string(),
        _ => commits
            .iter()
            .map(|(headline, body)| format!("* {headline}\n\n{}", body.trim()).trim_end().to_string())
            .collect::<Vec<_>>()
            .join("\n\n"),
    }
}

fn gh_available() -> bool {
    Command::new("gh").arg("--version").output().is_ok_and(|o| o.status.success())
}

fn gh(dir: &Path, args: &[&str]) -> Result<String> {
    let out = Command::new("gh").current_dir(dir).args(args).env("GIT_TERMINAL_PROMPT", "0").output()?;
    if !out.status.success() {
        bail!("gh {}: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Squash-merges a PR with `gh`, or turns on auto-merge. With `strip`, agent credits are removed from the message.
pub fn merge_pr(dir: &Path, number: u64, auto: bool, strip: bool) -> Result<String> {
    let n = number.to_string();
    let mut args = vec!["pr", "merge", n.as_str(), "--squash"];
    if auto {
        args.push("--auto");
    }
    let (subject, body);
    if strip {
        let info: serde_json::Value = serde_json::from_str(&gh(dir, &["pr", "view", &n, "--json", "title,commits"])?)?;
        let commits: Vec<(String, String)> = info["commits"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|c| (c["messageHeadline"].as_str().unwrap_or("").to_string(), c["messageBody"].as_str().unwrap_or("").to_string()))
            .collect();
        subject = format!("{} (#{n})", info["title"].as_str().unwrap_or(""));
        body = strip_agent_credits(&squash_body(&commits));
        args.extend(["--subject", &subject, "--body", &body]);
    }
    gh(dir, &args)
}

/// Where a repository comes from and what it is called on disk.
#[derive(Debug, PartialEq)]
pub struct CloneSpec {
    pub url: String,
    pub https: String,
    pub host: String,
    pub owner: String,
    pub name: String,
}

/// Accepts `owner/name`, an HTTPS or SSH URL (also a branch or file page of a host), or a local path.
pub fn parse_repo(input: &str) -> Result<CloneSpec> {
    let s = input.trim().trim_end_matches('/');
    ensure!(!s.is_empty() && !s.contains(char::is_whitespace) && !s.starts_with('-'), "リポジトリを入力してください（owner/name または URL）");
    let (host, path) = if let Some(rest) = s.strip_prefix("https://").or_else(|| s.strip_prefix("http://")) {
        let (host, path) = rest.split_once('/').unwrap_or((rest, ""));
        // A page inside the repository (tree/blob/…) names the repository by its first two segments.
        let segments: Vec<&str> = path.split('/').collect();
        let cut = segments.iter().position(|p| matches!(*p, "tree" | "blob" | "-" | "src" | "commit" | "pull")).unwrap_or(segments.len());
        (host.to_string(), segments[..cut].join("/"))
    } else if let Some(rest) = s.strip_prefix("ssh://") {
        let (host, path) = rest.split_once('/').unwrap_or((rest, ""));
        (host.rsplit('@').next().unwrap_or(host).to_string(), path.to_string())
    } else if let Some((user_host, path)) = s.split_once(':').filter(|(h, _)| h.contains('@')) {
        (user_host.rsplit('@').next().unwrap_or(user_host).to_string(), path.to_string())
    } else if s.matches('/').count() == 1 && !s.starts_with(['/', '.', '~']) {
        ("github.com".to_string(), s.to_string())
    } else if Path::new(s).exists() {
        let name = Path::new(s).file_stem().and_then(|n| n.to_str()).unwrap_or("repo");
        return Ok(CloneSpec { url: s.into(), https: s.into(), host: String::new(), owner: String::new(), name: name.into() });
    } else {
        bail!("リポジトリを解釈できません: {s}");
    };
    let path = path.trim_end_matches(".git").trim_matches('/');
    let (owner, name) = path.rsplit_once('/').unwrap_or(("", path));
    ensure!(!name.is_empty(), "リポジトリ名がありません: {s}");
    let https = format!("https://{host}/{path}.git");
    let url = if s.starts_with("ssh://") || (!s.starts_with("http") && s.contains('@')) { s.to_string() } else { https.clone() };
    Ok(CloneSpec { url, https, host, owner: owner.into(), name: name.into() })
}

/// Where `spec` is cloned under `root`: `<name>`, or `<host>/<owner>/<name>` like ghq.
pub fn clone_dest(root: &Path, spec: &CloneSpec, host_folders: bool) -> PathBuf {
    if host_folders && !spec.host.is_empty() { root.join(&spec.host).join(&spec.owner).join(&spec.name) } else { root.join(&spec.name) }
}

/// Clones with `gh` for GitHub shorthand when it is installed, else `git`. A refused SSH key retries over HTTPS.
pub fn clone_repo(spec: &CloneSpec, dest: &Path, input: &str) -> Result<()> {
    ensure!(!dest.exists() || std::fs::read_dir(dest)?.next().is_none(), "{} は既に存在します", dest.display());
    std::fs::create_dir_all(dest.parent().unwrap_or(Path::new(".")))?;
    let run = |program: &str, args: &[&str]| -> Result<()> {
        let out = Command::new(program).args(args).env("GIT_TERMINAL_PROMPT", "0").output()?;
        ensure!(out.status.success(), "{program} clone: {}", String::from_utf8_lossy(&out.stderr).trim());
        Ok(())
    };
    let dest_s = dest.to_string_lossy();
    if spec.host == "github.com" && input.trim().matches('/').count() == 1 && !input.contains(':') && gh_available() {
        return run("gh", &["repo", "clone", &format!("{}/{}", spec.owner, spec.name), &dest_s]);
    }
    run("git", &["clone", "--", &spec.url, &dest_s]).or_else(|e| {
        if spec.url == spec.https {
            return Err(e);
        }
        let _ = std::fs::remove_dir_all(dest);
        run("git", &["clone", "--", &spec.https, &dest_s])
    })
}

/// A folder name for a project: `Pinball Stats` → `pinball-stats`.
pub fn project_slug(name: &str) -> String {
    let mut slug = String::new();
    for c in name.trim().chars() {
        if c.is_alphanumeric() {
            slug.extend(c.to_lowercase());
        } else if !slug.ends_with('-') {
            slug.push('-');
        }
    }
    slug.trim_matches('-').to_string()
}

/// Makes `root/<slug>` a git repository with a README. The first commit needs a git identity; without one
/// the project is left uncommitted. Returns the folder and whether the commit was made.
pub fn create_project(root: &Path, name: &str) -> Result<(PathBuf, bool)> {
    let slug = project_slug(name);
    ensure!(!slug.is_empty(), "プロジェクト名を入力してください");
    let dir = root.join(&slug);
    ensure!(!dir.exists(), "{} は既に存在します", dir.display());
    std::fs::create_dir_all(&dir)?;
    git(&dir, &["init", "-q"])?;
    std::fs::write(dir.join("README.md"), format!("# {}\n", name.trim()))?;
    let identity = ["user.name", "user.email"].iter().all(|k| git(&dir, &["config", k]).is_ok_and(|v| !v.trim().is_empty()));
    if identity {
        git(&dir, &["add", "README.md"])?;
        git(&dir, &["commit", "-qm", "Initial commit"])?;
    }
    Ok((dir, identity))
}

/// Creates a private GitHub repository from the local one and pushes it. Needs a commit and no `origin`.
pub fn publish(dir: &Path) -> Result<String> {
    ensure!(git(dir, &["rev-parse", "--verify", "-q", "HEAD"]).is_ok(), "先に最初のコミットを作成してください");
    ensure!(git(dir, &["remote", "get-url", "origin"]).is_err(), "origin が既にあります");
    ensure!(gh_available(), "gh がインストールされていません");
    let name = dir.file_name().and_then(|n| n.to_str()).unwrap_or("repo");
    gh(dir, &["repo", "create", name, "--private", "--source", ".", "--push"])
}

/// A branch-name prefix from a user setting: invalid characters become `-`, a trailing `/` is added, empty stays empty.
pub fn branch_prefix(raw: &str) -> String {
    let mut p: String = raw
        .trim()
        .chars()
        .map(|c| if c.is_whitespace() || c.is_control() || "~^:?*[\\".contains(c) { '-' } else { c })
        .collect();
    while p.contains("..") || p.contains("@{") {
        p = p.replace("..", "-").replace("@{", "-");
    }
    let p = p.trim_start_matches('/');
    if p.is_empty() || p.ends_with('/') { p.to_string() } else { format!("{p}/") }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(dir: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .arg("-C").arg(dir)
            .args(["-c", "user.name=test", "-c", "user.email=test@example.com", "-c", "protocol.file.allow=always"])
            .args(args)
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("greencode-git-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn commit(dir: &Path, file: &str, text: &str) {
        std::fs::write(dir.join(file), text).unwrap();
        run(dir, &["add", "."]);
        run(dir, &["commit", "-qm", text]);
    }

    /// An `origin` and a clone of it on `main`, plus a second clone that pushes new work.
    fn remotes(name: &str) -> (PathBuf, PathBuf, PathBuf) {
        let root = temp(name);
        let (origin, a, b) = (root.join("origin.git"), root.join("a"), root.join("b"));
        run(&root, &["init", "-q", "--bare", "-b", "main", origin.to_str().unwrap()]);
        run(&root, &["clone", "-q", origin.to_str().unwrap(), a.to_str().unwrap()]);
        run(&a, &["symbolic-ref", "HEAD", "refs/heads/main"]);
        commit(&a, "f", "one");
        run(&a, &["push", "-q", "-u", "origin", "main"]);
        run(&root, &["clone", "-q", origin.to_str().unwrap(), b.to_str().unwrap()]);
        (root, a, b)
    }

    #[test]
    fn base_diff_and_commit_list() {
        let (root, a, _) = remotes("base");
        run(&a, &["checkout", "-q", "-b", "feature"]);
        commit(&a, "g", "two");
        std::fs::write(a.join("f"), "edited").unwrap();
        let diff = diff_for(&a, &DiffSource::Base);
        assert!(diff.contains("+two") || diff.contains("g"), "{diff}");
        assert!(diff.contains("+edited"), "{diff}");
        let commits = recent_commits(&a, 100);
        assert_eq!(commits.len(), 2);
        assert_eq!(commits[0].1, "two");
        let shown = commit_diff(&a, &commits[0].0).unwrap();
        assert!(shown.contains("+two") && !shown.contains("+edited"));
        assert_eq!(diff_for(&root, &DiffSource::Base), "ベースブランチが見つかりません");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn default_branch_sync_only_moves_clean_checkouts() {
        let (root, a, b) = remotes("sync");
        assert_eq!(sync_default_branch(&a).unwrap(), Sync::UpToDate);
        commit(&b, "f", "two");
        run(&b, &["push", "-q", "origin", "main"]);
        // Checked out with local changes: untouched.
        std::fs::write(a.join("f"), "dirty").unwrap();
        assert!(matches!(sync_default_branch(&a).unwrap(), Sync::Skipped(_)));
        assert_eq!(std::fs::read_to_string(a.join("f")).unwrap(), "dirty");
        // Clean: fast-forwarded in place.
        run(&a, &["checkout", "--", "f"]);
        assert!(matches!(sync_default_branch(&a).unwrap(), Sync::Updated(_)));
        assert_eq!(std::fs::read_to_string(a.join("f")).unwrap(), "two");
        // Not checked out: the branch ref moves without touching the working tree.
        commit(&b, "f", "three");
        run(&b, &["push", "-q", "origin", "main"]);
        run(&a, &["checkout", "-q", "-b", "other"]);
        assert!(matches!(sync_default_branch(&a).unwrap(), Sync::Updated(_)));
        assert_eq!(run(&a, &["show", "main:f"]), "three");
        // Diverged: left alone.
        run(&a, &["checkout", "-q", "main"]);
        commit(&a, "local", "mine");
        commit(&b, "f", "four");
        run(&b, &["push", "-q", "origin", "main"]);
        assert!(matches!(sync_default_branch(&a).unwrap(), Sync::Skipped(_)));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn credit_stripping_keeps_humans() {
        let message = "Fix parser\n\nReal explanation.\n\n🤖 Generated with [Claude Code](https://claude.com/claude-code)\n\nCo-authored-by: Claude <noreply@anthropic.com>\nCo-Authored-By: Alice <alice@example.com>\nCo-authored-by: Codex <codex@openai.com>\n";
        assert_eq!(strip_agent_credits(message), "Fix parser\n\nReal explanation.\n\n\nCo-Authored-By: Alice <alice@example.com>");
        let commits = vec![("a".to_string(), "x\nCo-authored-by: Claude".into()), ("b".to_string(), String::new())];
        assert_eq!(strip_agent_credits(&squash_body(&commits)), "* a\n\nx\n\n* b");
    }

    #[test]
    fn parses_repositories() {
        let github = |url: &str| parse_repo(url).unwrap();
        for input in ["owner/name", "https://github.com/owner/name", "https://github.com/owner/name.git", "https://github.com/owner/name/tree/main/src", "git@github.com:owner/name.git"] {
            let spec = github(input);
            assert_eq!((spec.host.as_str(), spec.owner.as_str(), spec.name.as_str()), ("github.com", "owner", "name"), "{input}");
        }
        assert_eq!(github("owner/name").https, "https://github.com/owner/name.git");
        assert!(parse_repo("not a repo").is_err());
        assert_eq!(clone_dest(Path::new("/r"), &github("owner/name"), true), Path::new("/r/github.com/owner/name"));
        assert_eq!(clone_dest(Path::new("/r"), &github("owner/name"), false), Path::new("/r/name"));
        assert_eq!(project_slug("Pinball Stats!"), "pinball-stats");
        assert_eq!(branch_prefix("feat bar"), "feat-bar/");
        assert_eq!(branch_prefix(""), "");
    }

    #[test]
    fn clone_local_and_publish_guard() {
        let (root, a, _) = remotes("clone");
        let spec = parse_repo(root.join("origin.git").to_str().unwrap()).unwrap();
        let dest = clone_dest(&root.join("projects"), &spec, false);
        clone_repo(&spec, &dest, "").unwrap();
        assert!(dest.join("f").exists());
        assert!(clone_repo(&spec, &dest, "").is_err());
        let fresh = root.join("fresh");
        std::fs::create_dir_all(&fresh).unwrap();
        run(&fresh, &["init", "-q"]);
        assert!(publish(&fresh).unwrap_err().to_string().contains("コミット"));
        assert!(publish(&a).unwrap_err().to_string().contains("origin"));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn new_project_has_readme_and_commit() {
        let root = temp("new");
        let (dir, committed) = create_project(&root, "Pinball Stats").unwrap();
        assert_eq!(dir, root.join("pinball-stats"));
        assert!(dir.join("README.md").exists());
        // The first commit depends on the machine's git identity.
        if committed {
            assert_eq!(run(&dir, &["rev-list", "--count", "HEAD"]).trim(), "1");
        }
        assert!(create_project(&root, "Pinball Stats").is_err());
        std::fs::remove_dir_all(root).unwrap();
    }
}
