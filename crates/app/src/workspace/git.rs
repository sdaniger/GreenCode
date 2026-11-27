//! Source-control actions: clone / new project / publish, diff sources, default-branch sync,
//! worktree options and project pin/archive.

use super::*;
use greencode_core::git::{self, DiffSource};
use std::{
    path::Path,
    time::{Duration, Instant},
};

/// A value the palette is currently asking for in its search box.
#[derive(Clone, Copy, PartialEq)]
pub(super) enum TextPrompt {
    Clone,
    OpenProject,
    NewProject,
    WorktreePrefix,
    TitleInstructions,
    VoiceCleanupModel,
    VoiceCorrection,
    VoiceInstructions,
}

impl TextPrompt {
    pub(super) fn hint(self) -> &'static str {
        match self {
            Self::Clone => "クローンするリポジトリ（owner/name または URL）を入力して ⏎",
            Self::OpenProject => "開くフォルダのパスを入力して ⏎",
            Self::NewProject => "プロジェクト名を入力して ⏎",
            Self::WorktreePrefix => "ワークツリーのブランチ接頭辞。空欄にすると付けません",
            Self::TitleInstructions => "タイトル生成の追加指示。空欄で解除します",
            Self::VoiceCleanupModel => "整形に使うモデル ID。空欄でスレッドのモデルを使います",
            Self::VoiceCorrection => "訂正の合図にする語句。空欄で解除します",
            Self::VoiceInstructions => "整形の追加指示。空欄で解除します",
        }
    }

    pub(super) fn action(self) -> &'static str {
        match self {
            Self::Clone => "クローン",
            Self::OpenProject => "開く",
            Self::NewProject => "作成",
            _ => "保存",
        }
    }

    /// The `ui.json` key a settings prompt edits.
    fn key(self) -> &'static str {
        match self {
            Self::Clone | Self::NewProject => "",
            Self::WorktreePrefix => "worktree_prefix",
            Self::TitleInstructions => "title_instructions",
            Self::VoiceCleanupModel => "voice_cleanup_model",
            Self::VoiceCorrection => "voice_correction",
            Self::VoiceInstructions => "voice_cleanup_instructions",
            Self::OpenProject => "",
        }
    }
}

/// What a base/commit diff needs; captured on the UI thread, run in the background.
pub(super) struct SourceScope {
    root: PathBuf,
    dirs: Vec<PathBuf>,
    pub(super) source: DiffSource,
}

impl SourceScope {
    pub(super) fn diff(&self) -> String {
        if self.dirs.is_empty() {
            return "Git リポジトリが見つかりません".into();
        }
        if self.dirs.len() == 1 {
            return git::diff_for(&self.dirs[0], &self.source);
        }
        let mut out = String::new();
        for dir in &self.dirs {
            let diff = git::diff_for(dir, &self.source);
            if diff.is_empty() {
                continue;
            }
            let name = greencode_core::repos::label(&self.root, dir);
            out.push_str(&format!("## {name}\n"));
            out.push_str(&greencode_core::repos::prefix_diff(&diff, &name));
        }
        out
    }
}

fn expand_home(raw: &str) -> PathBuf {
    match raw.trim() {
        "~" => greencode_core::user_home(),
        path if path.starts_with("~/") => greencode_core::user_home().join(&path[2..]),
        path => PathBuf::from(path),
    }
}

fn submodule_label(value: &str) -> &'static str {
    match value {
        "top" => "トップレベルのみ",
        "skip" => "スキップ",
        _ => "再帰的",
    }
}

impl Workspace {
    // ---- clone / new project / publish ----

    pub(super) fn clone_root(&self) -> PathBuf {
        self.ui["clone_root"]
            .as_str()
            .filter(|s| !s.trim().is_empty())
            .map(expand_home)
            .unwrap_or_else(|| greencode_core::home().join("projects"))
    }

    pub(super) fn start_clone(&mut self, input: String, cx: &mut Context<Self>) {
        let spec = match git::parse_repo(&input) {
            Ok(spec) => spec,
            Err(e) => return self.fail(format!("{e:#}")),
        };
        let dest = git::clone_dest(&self.clone_root(), &spec, self.ui["clone_host_folders"] == true);
        if dest.exists() {
            return self.fail(format!("{} は既に存在します", dest.display()));
        }
        self.notice(format!("クローンしています: {} → {}", spec.url, dest.display()));
        let (target, shown) = (dest.clone(), dest.clone());
        let background = spec;
        let original = input;
        cx.spawn(async move |this, cx| {
            let started = std::time::Instant::now();
            let result = cx
                .background_executor()
                .spawn(async move { git::clone_repo(&background, &target, &original) })
                .await;
            this.update_in(cx, |this, window, cx| {
                match result {
                    Ok(()) => {
                        this.new_thread(shown.clone(), false, window, cx);
                        this.notice(format!(
                            "クローンしました（{:.0}秒）: {}",
                            started.elapsed().as_secs_f64(),
                            shown.display()
                        ));
                    }
                    Err(e) => this.fail(format!("クローンできません: {e:#}")),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    pub(super) fn start_new_project(&mut self, name: String, _window: &mut Window, cx: &mut Context<Self>) {
        let root = self.clone_root();
        let name = name.trim().to_string();
        if name.is_empty() {
            return self.fail("プロジェクト名を入力してください");
        }
        self.notice(format!("プロジェクトを作成しています: {name}"));
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { git::create_project(&root, &name) })
                .await;
            this.update_in(cx, |this, window, cx| {
                match result {
                    Ok((dir, committed)) => {
                        let path = dir.display().to_string();
                        this.new_thread(dir, false, window, cx);
                        if !committed {
                            this.notice("git の user.name / user.email が未設定のため、最初のコミットは作成していません");
                        }
                        this.notice(format!("プロジェクトを作成しました: {path}"));
                    }
                    Err(e) => this.fail(format!("プロジェクトを作成できません: {e:#}")),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    pub(super) fn publish_project(&mut self, cx: &mut Context<Self>) {
        let dir = self.thread().project();
        let guard = if !dir.join(".git").exists() {
            Err("Git リポジトリではありません".to_string())
        } else if std::process::Command::new("git")
            .arg("-C")
            .arg(&dir)
            .args(["rev-parse", "--verify", "-q", "HEAD"])
            .output()
            .is_ok_and(|o| o.status.success())
        {
            Ok(())
        } else {
            Err("先に最初のコミットを作成してください".to_string())
        };
        if let Err(e) = guard {
            return self.fail(e);
        }
        self.notice("GitHub に非公開リポジトリとして公開しています…");
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { git::publish(&dir) })
                .await;
            this.update(cx, |this, cx| {
                match result {
                    Ok(out) => {
                        this.notice(format!("GitHub に公開しました: {}", out.trim()))
                    }
                    Err(e) => this.fail(format!("公開できません: {e:#}")),
                }
                this.refresh_git(cx);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    // ---- diff sources ----

    pub(super) fn diff_source_scope(&self) -> SourceScope {
        let root = self.thread().meta.cwd.clone();
        let dirs: Vec<PathBuf> = match &self.repos.selected {
            Some(repo) => vec![repo.clone()],
            // A worktree or project checkout is itself a repository; use it before discovering nested ones.
            None if root.join(".git").exists() => vec![root.clone()],
            None => self.thread_repos(),
        };
        SourceScope {
            root,
            dirs: dirs.into_iter().filter(|d| d.join(".git").exists()).collect(),
            source: self.diff_source.clone(),
        }
    }

    /// Loads the newest commits of the diff's repository into the palette.
    pub(super) fn pick_commit(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        let Some(dir) = self.diff_source_scope().dirs.into_iter().next() else {
            return self.fail("Git リポジトリが見つかりません");
        };
        cx.spawn(async move |this, cx| {
            let list = cx
                .background_executor()
                .spawn(async move { git::recent_commits(&dir, 100) })
                .await;
            this.update_in(cx, |this, window, cx| {
                if list.is_empty() {
                    this.fail("コミットがありません");
                    return;
                }
                this.diff_commits = list;
                this.picking_commit = true;
                this.open_palette("コミット: ", window, cx);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    pub(super) fn choose_commit(&mut self, sha: String, cx: &mut Context<Self>) {
        self.diff_source = DiffSource::Commit(sha);
        self.picking_commit = false;
        self.show_diff = true;
        self.files_panel = None;
        self.pr_panel = None;
        self.refresh_diff(cx);
    }

    /// The source picker above the diff list.
    pub(super) fn diff_source_row(&self, pal: Pal, cx: &mut Context<Self>) -> Div {
        let current = &self.diff_source;
        let pick = |id: &'static str, label: String, source: DiffSource, on: bool, cx: &mut Context<Self>| {
            chip(id, label, pal)
                .when(on, |c| c.bg(pal.selected).text_color(pal.fg))
                .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                    this.run(Act::DiffSource(source.clone()), window, cx)
                }))
        };
        let commit_label = match current {
            DiffSource::Commit(sha) => format!("コミット {sha}"),
            _ => "コミット…".into(),
        };
        div()
            .flex()
            .flex_wrap()
            .gap_1()
            .px_2()
            .py_1()
            .border_b_1()
            .border_color(pal.line)
            .child(pick("diff-src-uncommitted", "未コミット".into(), DiffSource::Uncommitted, matches!(current, DiffSource::Uncommitted), cx))
            .child(pick("diff-src-base", "ベースブランチとの差分".into(), DiffSource::Base, matches!(current, DiffSource::Base), cx))
            .child(
                chip("diff-src-commit", commit_label, pal)
                    .when(matches!(current, DiffSource::Commit(_)), |c| c.bg(pal.selected).text_color(pal.fg))
                    .on_click(cx.listener(|this, _: &ClickEvent, window, cx| this.run(Act::PickCommit, window, cx))),
            )
    }

    // ---- default branch ----

    /// `force` skips the hourly gate; both are silent unless the branch actually moved.
    pub(super) fn sync_default_branch(&mut self, force: bool, cx: &mut Context<Self>) {
        if !force && self.ui["sync_default_branch"] != true {
            return;
        }
        let project = self.thread().project();
        let dir = if project.join(".git").exists() { project } else { self.thread().meta.cwd.clone() };
        if !dir.join(".git").exists() {
            if force {
                self.fail("Git リポジトリが見つかりません");
            }
            return;
        }
        let now = Instant::now();
        if !force && self.synced.get(&dir).is_some_and(|at| now.duration_since(*at) < Duration::from_secs(3600)) {
            return;
        }
        self.synced.insert(dir.clone(), now);
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { greencode_core::git::sync_default_branch(&dir) })
                .await;
            this.update(cx, |this, cx| {
                match result {
                    Ok(greencode_core::git::Sync::Updated(sha)) => {
                        this.notice(format!("既定ブランチを最新にしました: {sha}"));
                        this.refresh_git(cx);
                        cx.notify();
                    }
                    Ok(greencode_core::git::Sync::UpToDate) if force => {
                        this.notice("既定ブランチは最新です")
                    }
                    Ok(greencode_core::git::Sync::Skipped(why)) if force => {
                        this.notice(format!("同期しませんでした: {why}"))
                    }
                    Ok(_) => {}
                    Err(e) if force => this.fail(format!("同期できません: {e:#}")),
                    Err(_) => {}
                }
            })
            .ok();
        })
        .detach();
    }

    // ---- worktrees ----

    pub(super) fn worktree_options(&self) -> worktree::Options {
        let prefix = git::branch_prefix(self.ui["worktree_prefix"].as_str().unwrap_or("greencode/"));
        let submodules = match self.ui["worktree_submodules"].as_str() {
            Some("top") => worktree::Submodules::TopLevel,
            Some("skip") => worktree::Submodules::Skip,
            _ => worktree::Submodules::Recursive,
        };
        worktree::Options {
            prefix,
            submodules,
            clone: self.cfg.worktree.clone,
            backend: self.cfg.isolation_opts.isolation.backend,
        }
    }

    // ---- project pin / archive ----

    fn project_list(&self, key: &str) -> Vec<PathBuf> {
        self.ui[key]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|v| v.as_str().map(PathBuf::from))
            .collect()
    }

    pub(super) fn pinned_projects(&self) -> Vec<PathBuf> {
        self.project_list("pinned_projects")
    }

    pub(super) fn archived_projects(&self) -> Vec<PathBuf> {
        self.project_list("archived_projects")
    }

    pub(super) fn project_pinned(&self, project: &Path) -> bool {
        self.pinned_projects().iter().any(|p| p == project)
    }

    pub(super) fn project_archived(&self, project: &Path) -> bool {
        self.archived_projects().iter().any(|p| p == project)
    }

    /// Toggles `key` ("pinned_projects" or "archived_projects") for `project`.
    pub(super) fn toggle_project(&mut self, key: &str, project: &Path) {
        let mut list = self.project_list(key);
        match list.iter().position(|p| p == project) {
            Some(i) => drop(list.remove(i)),
            None => list.push(project.to_path_buf()),
        }
        self.ui[key] = json!(list
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect::<Vec<_>>());
        self.save_ui();
        self.list.reset(self.thread().items.len());
    }

    /// Every known project: pinned order first, then by first thread appearance.
    pub(super) fn known_projects(&self) -> Vec<PathBuf> {
        let mut projects = self.pinned_projects();
        for t in &self.threads {
            let project = t.project();
            if !projects.contains(&project) {
                projects.push(project);
            }
        }
        for project in self.archived_projects() {
            if !projects.contains(&project) {
                projects.push(project);
            }
        }
        projects
    }

    pub(super) fn project_acts(&self) -> Vec<(String, Act)> {
        let mut rows = Vec::new();
        for project in self.known_projects() {
            let name = project_name(&project);
            rows.push((
                if self.project_pinned(&project) {
                    format!("プロジェクトのピン留めを解除: {name}")
                } else {
                    format!("プロジェクトをピン留め: {name}")
                },
                Act::PinProject(project.clone()),
            ));
            rows.push((
                if self.project_archived(&project) {
                    format!("プロジェクトを復元: {name}")
                } else {
                    format!("プロジェクトをアーカイブ: {name}")
                },
                Act::ArchiveProject(project),
            ));
        }
        rows
    }

    // ---- dispatch ----

    pub(super) fn submit_prompt(
        &mut self,
        prompt: TextPrompt,
        text: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match prompt {
            TextPrompt::Clone => self.start_clone(text, cx),
            TextPrompt::OpenProject => {
                if text.is_empty() {
                    return;
                }
                let path = Path::new(&text);
                let path = if let Ok(rest) = path.strip_prefix("~") {
                    greencode_core::user_home().join(rest)
                } else {
                    self.thread().project().join(path)
                };
                match path.canonicalize() {
                    Ok(path) if path.is_dir() => self.new_thread(path, false, window, cx),
                    Ok(_) => self.fail(format!("フォルダではありません: {}", path.display())),
                    Err(e) => self.fail(format!("フォルダを開けません: {}: {e}", path.display())),
                }
            }
            TextPrompt::NewProject => self.start_new_project(text, window, cx),
            TextPrompt::WorktreePrefix => {
                let prefix = git::branch_prefix(&text);
                self.ui["worktree_prefix"] = json!(prefix);
                self.save_ui();
                self.notice(format!(
                    "ワークツリーのブランチ接頭辞: {}",
                    if prefix.is_empty() { "なし" } else { &prefix }
                ));
            }
            TextPrompt::TitleInstructions => self.set_title_instructions(text.trim()),
            TextPrompt::VoiceCleanupModel if text.trim().is_empty() => {
                self.ui["voice_cleanup_model"] = Value::Null;
                self.save_ui();
                self.notice("文字起こしの整形にスレッドのモデルを使います");
            }
            TextPrompt::VoiceCleanupModel => self.voice_command(&format!("cleanup-model {text}"), cx),
            TextPrompt::VoiceCorrection => self.voice_command(&format!("correction {text}"), cx),
            TextPrompt::VoiceInstructions => self.voice_command(&format!("instructions {text}"), cx),
        }
    }

    pub(super) fn git_act(&mut self, act: Act, window: &mut Window, cx: &mut Context<Self>) {
        match act {
            Act::CloneProject => {
                self.text_prompt = Some(TextPrompt::Clone);
                self.open_palette("", window, cx);
            }
            Act::NewProject => {
                self.text_prompt = Some(TextPrompt::NewProject);
                self.open_palette("", window, cx);
            }
            Act::PublishProject => self.publish_project(cx),
            Act::SyncDefaultBranch => self.sync_default_branch(true, cx),
            Act::DiffSource(source) => {
                self.diff_source = source;
                self.show_diff = true;
                self.files_panel = None;
                self.pr_panel = None;
                self.refresh_diff(cx);
            }
            Act::DiffCommit(sha) => self.choose_commit(sha, cx),
            Act::PickCommit => self.pick_commit(window, cx),
            Act::PinProject(project) => self.toggle_project("pinned_projects", &project),
            Act::ArchiveProject(project) => self.toggle_project("archived_projects", &project),
            Act::SetCloneRoot => {
                let rx = cx.prompt_for_paths(PathPromptOptions {
                    files: false,
                    directories: true,
                    multiple: false,
                    prompt: Some("クローン先フォルダ".into()),
                });
                cx.spawn_in(window, async move |this, cx| {
                    if let Ok(Ok(Some(paths))) = rx.await {
                        if let Some(path) = paths.into_iter().next() {
                            this.update(cx, |this, cx| {
                                this.ui["clone_root"] = json!(path.to_string_lossy());
                                this.save_ui();
                                this.notice(format!("クローン先: {}", path.display()));
                                cx.notify();
                            })
                            .ok();
                        }
                    }
                })
                .detach();
            }
            Act::EditText(prompt) => {
                self.text_prompt = Some(prompt);
                let default = if prompt == TextPrompt::WorktreePrefix { "greencode/" } else { "" };
                let current = self.ui[prompt.key()].as_str().unwrap_or(default).to_string();
                self.open_palette(&current, window, cx);
            }
            Act::CycleWorktreeSubmodules => {
                let next = match self.ui["worktree_submodules"].as_str() {
                    Some("recursive") => "top",
                    Some("top") => "skip",
                    Some("skip") => "recursive",
                    _ => "top",
                };
                self.ui["worktree_submodules"] = json!(next);
                self.save_ui();
                self.notice(format!("新しいワークツリーのサブモジュール: {}", submodule_label(next)));
            }
            _ => {}
        }
    }
}
