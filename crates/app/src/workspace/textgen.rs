//! Automatic thread names, commit messages, PR descriptions and worktree branch names.
//! All of it is off until a text generation model is chosen; failures fall back to the old behavior.

use super::*;
use std::path::Path;

fn git(cwd: &Path, args: &[&str], stdin: Option<&str>) -> Result<String, String> {
    use std::io::Write;
    let mut child = std::process::Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .stdin(if stdin.is_some() { std::process::Stdio::piped() } else { std::process::Stdio::null() })
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| e.to_string())?;
    if let (Some(text), Some(mut pipe)) = (stdin, child.stdin.take()) {
        pipe.write_all(text.as_bytes()).map_err(|e| e.to_string())?;
    }
    let out = child.wait_with_output().map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err(format!("git {}: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim()));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Commits the staged changes (everything, when nothing is staged) with a generated message; returns `git commit`'s output.
async fn commit(cfg: &Config, model: &str, cwd: &Path) -> Result<String, String> {
    if git(cwd, &["diff", "--cached", "--quiet"], None).is_ok() {
        git(cwd, &["add", "-A"], None)?;
    }
    let diff = git(cwd, &["diff", "--cached"], None)?;
    if diff.trim().is_empty() {
        return Err("コミットする変更がありません".into());
    }
    let message = greencode_core::textgen::commit_message(cfg, model, &diff).await.map_err(|e| format!("{e:#}"))?;
    git(cwd, &["commit", "-F", "-"], Some(&message))
}

/// Opens a pull request for the current branch with a generated title and body; returns `gh`'s output (the URL).
async fn pull_request(cfg: &Config, model: &str, cwd: &Path) -> Result<String, String> {
    let base = git(cwd, &["symbolic-ref", "--short", "refs/remotes/origin/HEAD"], None)
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| "origin/main".into());
    let log = git(cwd, &["log", "--oneline", &format!("{base}..HEAD")], None)?;
    let diff = git(cwd, &["diff", &format!("{base}...HEAD")], None)?;
    if log.trim().is_empty() {
        return Err("ベースブランチとの差分がありません".into());
    }
    let (title, body) = greencode_core::textgen::pr_text(cfg, model, &log, &diff).await.map_err(|e| format!("{e:#}"))?;
    let out = tokio::process::Command::new("gh")
        .args(["pr", "create", "--title", &title, "--body", &body])
        .current_dir(cwd)
        .stdin(std::process::Stdio::null())
        .output()
        .await
        .map_err(|e| e.to_string())?;
    let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    if out.status.success() { Ok(text) } else { Err(text) }
}

/// Renames a worktree thread's `<prefix><id>` branch after its task.
async fn rename_branch(cfg: &Config, model: &str, prefix: &str, id: &str, cwd: &Path, prompt: &str) -> Option<String> {
    let current = git(cwd, &["symbolic-ref", "--short", "HEAD"], None).ok()?;
    if current.trim() != format!("{prefix}{id}") {
        return None;
    }
    let name = greencode_core::textgen::branch_name(cfg, model, prefix, prompt).await.ok()?;
    git(cwd, &["branch", "-m", &name], None).ok()?;
    Some(name)
}

impl Workspace {
    /// The chosen text generation model, when the feature is on.
    pub(super) fn text_model(&self) -> Option<String> {
        self.ui["text_model"].as_str().filter(|m| !m.is_empty()).map(String::from)
    }

    pub(super) fn set_text_model(&mut self, model: Option<String>) {
        self.ui["text_model"] = json!(model.clone().unwrap_or_default());
        self.save_ui();
        self.notice(match model {
            Some(m) => format!("自動作成に使うモデル: {m}"),
            None => "名前と文章の自動作成をオフにしました".to_string(),
        });
    }

    /// Off → each model of the picker in turn → off.
    pub(super) fn cycle_text_model(&mut self) {
        let models = self.models();
        let next = match self.text_model() {
            None => models.first().cloned(),
            Some(m) => models.iter().position(|x| *x == m).and_then(|i| models.get(i + 1).cloned()),
        };
        self.set_text_model(next);
    }

    pub(super) fn set_title_instructions(&mut self, text: &str) {
        self.ui["title_instructions"] = json!(text);
        self.save_ui();
        self.notice(if text.is_empty() { "タイトル生成の追加指示を消しました".to_string() } else { format!("タイトル生成の追加指示: {text}") });
    }

    /// After a thread's turn: writes its title (and names a worktree branch) with the text model.
    /// `force` regenerates from the first exchange even when the title was set before.
    pub(super) fn request_title(&mut self, ix: usize, force: bool, cx: &mut Context<Self>) {
        // No network from UI tests.
        if cfg!(test) {
            return;
        }
        let Some(model) = self.text_model() else {
            if force {
                self.notice("設定 → 入力と会話で、自動作成に使うモデルを選んでください");
            }
            return;
        };
        let t = &self.threads[ix];
        let users = t.items.iter().filter(|i| matches!(i, Item::User(_))).count();
        let first_user = t.items.iter().find_map(|i| if let Item::User(u) = i { Some(u.to_string()) } else { None });
        let answer = t.items.iter().rev().find_map(|i| if let Item::Assistant(a) = i { Some(a.clone()) } else { None });
        let (Some(user), Some(answer)) = (first_user, answer) else {
            return;
        };
        // Automatic: only the first turn, and only over the placeholder title (a `/rename` stays).
        if !force && (users != 1 || t.meta.title != user.chars().take(60).collect::<String>()) {
            return;
        }
        let (id, cwd, worktree) = (t.meta.id.clone(), t.meta.cwd.clone(), t.meta.repo.is_some());
        let tid = id.clone();
        let prefix = greencode_core::git::branch_prefix(self.ui["worktree_prefix"].as_str().unwrap_or("greencode/"));
        let (cfg, instructions) = (self.cfg.clone(), self.ui["title_instructions"].as_str().unwrap_or("").to_string());
        let task = self.rt.spawn(async move {
            let title = greencode_core::textgen::title(&cfg, &model, &instructions, &user, &answer).await;
            let branch = if worktree && !force { rename_branch(&cfg, &model, &prefix, &tid, &cwd, &user).await } else { None };
            (title.map_err(|e| format!("{e:#}")), branch)
        });
        cx.spawn(async move |this, cx| {
            let Ok((title, branch)) = task.await else {
                return;
            };
            let _ = this.update(cx, |this, cx| {
                let Some(t) = this.threads.iter_mut().find(|t| t.meta.id == id) else {
                    return;
                };
                match title {
                    Ok(title) if !title.is_empty() => {
                        t.meta.title = title;
                        let _ = t.save();
                    }
                    Err(e) if force => t.items.push(Item::Error(format!("タイトルを生成できません: {e}").into())),
                    _ => {}
                }
                if let Some(branch) = branch {
                    t.items.push(Item::Notice(format!("ブランチ名: {branch}").into()));
                }
                this.list.reset(this.threads.iter().find(|t| t.meta.id == id).map_or(0, |t| t.items.len()));
                cx.notify();
            });
        })
        .detach();
    }

    /// `commit`: generate a message and commit; otherwise open the pull request with generated text.
    pub(super) fn generate_git(&mut self, pr: bool, cx: &mut Context<Self>) {
        if !pr && !self.cfg.integration_opts.commit.enabled {
            self.notice("commit.enabled = false のためコミットしません".to_string());
            return;
        }
        let Some(model) = self.text_model() else {
            self.notice("設定 → 入力と会話で、自動作成に使うモデルを選んでください");
            return;
        };
        let (cfg, cwd, id) = (self.cfg.clone(), self.thread().meta.cwd.clone(), self.thread().meta.id.clone());
        self.notice(if pr { "PR のタイトルと本文を生成しています…" } else { "コミットメッセージを生成しています…" });
        let task = self.rt.spawn(async move {
            if pr { pull_request(&cfg, &model, &cwd).await } else { commit(&cfg, &model, &cwd).await }
        });
        cx.spawn(async move |this, cx| {
            let Ok(result) = task.await else {
                return;
            };
            let _ = this.update(cx, |this, cx| {
                if let Some(t) = this.threads.iter_mut().find(|t| t.meta.id == id) {
                    t.items.push(match result {
                        Ok(out) => Item::Notice(out.chars().take(6000).collect::<String>().into()),
                        Err(e) => Item::Error(e.chars().take(6000).collect::<String>().into()),
                    });
                    this.list.reset(this.threads.iter().find(|t| t.meta.id == id).map_or(0, |t| t.items.len()));
                    cx.notify();
                }
            });
        })
        .detach();
    }
}
