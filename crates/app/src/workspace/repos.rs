//! Projects with several repositories: the repository list, extra folders, the diff picker,
//! per-repository details actions and worktree layout.

use super::*;
use greencode_core::repos;
// `gpui_kit::*` (via super) also exports a `Path` element type; these signatures mean the filesystem one.
use std::path::Path;

#[derive(Default)]
pub(super) struct RepoState {
    /// Repositories of the active thread with branch and dirty count; refreshed with the git state.
    list: Vec<repos::RepoInfo>,
    /// Repositories of the project folder, for the palette's worktree selection.
    project_list: Vec<PathBuf>,
    /// `.code-workspace` folders outside the project that are not added yet.
    suggestions: Vec<PathBuf>,
    /// The diff panel's repository; `None` groups all of them.
    pub(super) selected: Option<PathBuf>,
    generation: u64,
}

/// What a background diff needs, captured on the UI thread.
pub(super) struct DiffScope {
    root: PathBuf,
    extra: Vec<PathBuf>,
    linked: bool,
    selected: Option<PathBuf>,
}

impl DiffScope {
    pub(super) fn diff(&self) -> String {
        let found = repos::discover(&self.root, &self.extra, self.linked);
        if let Some(r) = &self.selected {
            return repos::diff(&self.root, std::slice::from_ref(r));
        }
        if found.is_empty() || (found.len() == 1 && found[0] == self.root) {
            return greencode_core::worktree::diff(&self.root);
        }
        repos::diff(&self.root, &found)
    }
}

impl Workspace {
    fn extra_repos(&self, project: &Path) -> Vec<PathBuf> {
        self.ui["project_repos"][project.to_string_lossy().as_ref()]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|v| v.as_str().map(PathBuf::from))
            .collect()
    }

    fn set_ui_list(&mut self, group: &str, project: &Path, list: Vec<PathBuf>) {
        if !self.ui[group].is_object() {
            self.ui[group] = json!({});
        }
        self.ui[group][project.to_string_lossy().as_ref()] = json!(list);
        self.save_ui();
    }

    /// Repositories the active thread works in, read from disk.
    pub(super) fn thread_repos(&self) -> Vec<PathBuf> {
        self.repos_of(self.thread())
    }

    pub(super) fn repos_of(&self, t: &Thread) -> Vec<PathBuf> {
        let extra = if t.meta.repo.is_some() { vec![] } else { self.extra_repos(&t.project()) };
        repos::discover(&t.meta.cwd, &extra, t.meta.repo.is_some())
    }

    pub(super) fn diff_scope(&self) -> DiffScope {
        let t = self.thread();
        DiffScope {
            root: t.meta.cwd.clone(),
            extra: if t.meta.repo.is_some() { vec![] } else { self.extra_repos(&t.project()) },
            linked: t.meta.repo.is_some(),
            selected: self.repos.selected.clone(),
        }
    }

    pub(super) fn refresh_repos(&mut self, cx: &mut Context<Self>) {
        let scope = self.diff_scope();
        // Suggestions come from the project folder itself, which a worktree thread does not copy.
        let (project, added) = (self.thread().project(), self.extra_repos(&self.thread().project()));
        self.repos.generation = self.repos.generation.wrapping_add(1);
        let generation = self.repos.generation;
        cx.spawn(async move |this, cx| {
            let (list, suggestions, project_list) = cx
                .background_executor()
                .spawn(async move {
                    let found = repos::discover(&scope.root, &scope.extra, scope.linked);
                    let list = found.iter().map(|r| repos::info(r)).collect::<Vec<_>>();
                    (list, repos::workspace_suggestions(&project, &added), repos::discover(&project, &added, false))
                })
                .await;
            this.update(cx, |this, cx| {
                if this.repos.generation != generation {
                    return;
                }
                if this.repos.selected.as_ref().is_some_and(|s| !list.iter().any(|i| i.path == *s)) {
                    this.repos.selected = None;
                }
                (this.repos.list, this.repos.suggestions, this.repos.project_list) = (list, suggestions, project_list);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Worktree for a new thread: one plain worktree, or a mirrored folder with one per repository.
    pub(super) fn make_worktree(&mut self, project: &Path, id: &str) -> Result<PathBuf, String> {
        let base = self.worktree_base();
        let all = repos::discover(project, &self.extra_repos(project), false);
        if !repos::is_multi(project, &all) {
            return greencode_core::worktree::create_with(project, &base, id, &self.worktree_options())
                .map_err(|e| format!("worktree を作成できません: {e:#}"));
        }
        let chosen = self.ui["worktree_repos"][project.to_string_lossy().as_ref()]
            .as_array()
            .map(|a| a.iter().filter_map(|v| v.as_str().map(PathBuf::from)).collect::<Vec<_>>());
        let selected = repos::default_selection(&all, chosen.as_deref());
        if selected.is_empty() {
            return Err("worktree の対象リポジトリがありません。パレットの「Worktree 対象」で選んでください".into());
        }
        let (path, skipped) = repos::create_worktrees(project, &all, &selected, &base, id, &self.worktree_options())
            .map_err(|e| format!("worktree を作成できません: {e}"))?;
        if !skipped.is_empty() {
            let names = skipped.iter().map(|p| repos::label(project, p)).collect::<Vec<_>>().join(", ");
            self.notice(format!("コミットがないため worktree を作らなかったリポジトリ: {names}"));
        }
        Ok(path)
    }

    pub(super) fn repo_acts(&self) -> Vec<(String, Act)> {
        let project = self.thread().project();
        let mut v = vec![("リポジトリを追加…".to_string(), Act::AddRepo)];
        for p in self.extra_repos(&project) {
            v.push((format!("リポジトリを外す: {}", p.display()), Act::RemoveRepo(p)));
        }
        for p in &self.repos.suggestions {
            v.push((format!("リポジトリを追加: {}（.code-workspace）", p.display()), Act::AddRepoPath(p.clone())));
        }
        // Cached by the git refresh: `acts()` runs on each palette keystroke, so no disk scan here.
        let all = &self.repos.project_list;
        if repos::is_multi(&project, all) {
            let chosen = self.worktree_selection(&project, all);
            for r in repos::default_selection(all, Some(all)) {
                let mark = if chosen.contains(&r) { "✓" } else { "✗" };
                v.push((format!("Worktree 対象 {mark} {}", repos::label(&project, &r)), Act::ToggleWorktreeRepo(r)));
            }
        }
        v
    }

    fn worktree_selection(&self, project: &Path, all: &[PathBuf]) -> Vec<PathBuf> {
        let chosen = self.ui["worktree_repos"][project.to_string_lossy().as_ref()]
            .as_array()
            .map(|a| a.iter().filter_map(|v| v.as_str().map(PathBuf::from)).collect::<Vec<_>>());
        repos::default_selection(all, chosen.as_deref())
    }

    pub(super) fn repo_act(&mut self, act: Act, window: &mut Window, cx: &mut Context<Self>) {
        let project = self.thread().project();
        match act {
            Act::AddRepo => {
                let rx = cx.prompt_for_paths(PathPromptOptions {
                    files: false,
                    directories: true,
                    multiple: false,
                    prompt: Some("リポジトリを追加".into()),
                });
                cx.spawn_in(window, async move |this, cx| {
                    if let Ok(Ok(Some(paths))) = rx.await {
                        if let Some(p) = paths.into_iter().next() {
                            this.update_in(cx, |this, window, cx| this.repo_act(Act::AddRepoPath(p), window, cx)).ok();
                        }
                    }
                })
                .detach();
                return;
            }
            Act::AddRepoPath(p) => {
                let mut list = self.extra_repos(&project);
                if !p.join(".git").exists() {
                    return self.fail(format!("Git リポジトリではありません: {}", p.display()));
                }
                if list.len() >= repos::MAX_REPOS {
                    return self.fail(format!("追加できるのは {} 件までです", repos::MAX_REPOS));
                }
                if !list.contains(&p) {
                    list.push(p);
                }
                self.set_ui_list("project_repos", &project, list);
            }
            Act::RemoveRepo(p) => {
                let list = self.extra_repos(&project).into_iter().filter(|r| *r != p).collect();
                self.set_ui_list("project_repos", &project, list);
            }
            Act::ToggleWorktreeRepo(r) => {
                let mut chosen = self.worktree_selection(&project, &self.repos.project_list);
                match chosen.iter().position(|c| *c == r) {
                    Some(i) => drop(chosen.remove(i)),
                    None => chosen.push(r),
                }
                self.set_ui_list("worktree_repos", &project, chosen);
            }
            Act::DiffRepo(r) => self.repos.selected = r,
            Act::RepoCommit(p) => {
                let text = format!("{COMMIT_PROMPT} Only the repository at `{}` (use git -C).", p.display());
                if self.thread().status == Status::Idle {
                    self.threads[self.active].queue_held = false;
                    self.start_prompt(text, cx);
                } else {
                    self.threads[self.active].queue.push_back(text);
                }
                return;
            }
            Act::RepoPush(p) => {
                let cmd = format!("git -C {} push -u origin HEAD", shell_quote(&p.to_string_lossy()));
                return self.run_shell(cmd, false, cx);
            }
            _ => return,
        }
        self.refresh_repos(cx);
        self.refresh_diff(cx);
        cx.notify();
    }

    /// Chips above the diff: all repositories, or one.
    pub(super) fn repo_picker(&self, pal: Pal, cx: &mut Context<Self>) -> Option<Div> {
        if self.repos.list.len() < 2 {
            return None;
        }
        let root = self.thread().meta.cwd.clone();
        let chip_of = |id: usize, label: String, target: Option<PathBuf>, on: bool, cx: &mut Context<Self>| {
            chip(("repo-pick", id), label, pal).when(on, |c| c.bg(pal.selected).text_color(pal.fg)).on_click(
                cx.listener(move |this, _: &ClickEvent, window, cx| this.run(Act::DiffRepo(target.clone()), window, cx)),
            )
        };
        let mut row = div().flex().flex_wrap().gap_1().px_2().py_1().border_b_1().border_color(pal.line);
        row = row.child(chip_of(0, "すべて".into(), None, self.repos.selected.is_none(), cx));
        for (i, r) in self.repos.list.iter().enumerate() {
            let on = self.repos.selected.as_ref() == Some(&r.path);
            row = row.child(chip_of(i + 1, repos::label(&root, &r.path), Some(r.path.clone()), on, cx));
        }
        Some(row)
    }

    /// One row per repository in the project details card: branch, uncommitted count, commit and push.
    pub(super) fn repo_rows(&self, mut card: Div, pal: Pal, cx: &mut Context<Self>) -> Div {
        if self.repos.list.len() < 2 && self.extra_repos(&self.thread().project()).is_empty() {
            return card;
        }
        let root = self.thread().meta.cwd.clone();
        for (i, r) in self.repos.list.iter().enumerate() {
            let (c, p) = (r.path.clone(), r.path.clone());
            card = card.child(
                div()
                    .px_2()
                    .py_1()
                    .child(SharedString::from(format!(
                        "{}  ⎇ {} · 未コミット {}",
                        repos::label(&root, &r.path),
                        if r.branch.is_empty() { "(detached)" } else { r.branch.as_str() },
                        r.dirty
                    )))
                    .child(
                        div()
                            .flex()
                            .gap_1()
                            .child(chip(("repo-commit", i), "コミット", pal).on_click(cx.listener(
                                move |this, _: &ClickEvent, window, cx| this.run(Act::RepoCommit(c.clone()), window, cx),
                            )))
                            .child(chip(("repo-push", i), "プッシュ", pal).on_click(cx.listener(
                                move |this, _: &ClickEvent, window, cx| this.run(Act::RepoPush(p.clone()), window, cx),
                            ))),
                    ),
            );
        }
        card
    }
}
