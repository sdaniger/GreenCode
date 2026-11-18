use super::*;
use std::path::{Path, PathBuf};

pub(super) struct Storage {
    pub field: Entity<InputState>,
    pub error: Option<String>,
    pub candidates: Vec<(PathBuf, worktree::Candidate)>,
    pub busy: bool,
}

type Entry = (String, PathBuf, bool, String);

fn location(value: &str) -> Result<PathBuf, &'static str> {
    if value.is_empty() {
        return Ok(greencode_core::home().join("worktrees"));
    }
    let path = PathBuf::from(value);
    if !path.is_absolute() {
        return Err("絶対パスを入力してください");
    }
    Ok(path)
}

fn scan(entries: &[(PathBuf, Entry)], bases: &[PathBuf]) -> Vec<(PathBuf, worktree::Candidate)> {
    let mut projects: Vec<PathBuf> = entries.iter().map(|(project, _)| project.clone()).collect();
    projects.sort();
    projects.dedup();
    let mut results = Vec::new();
    for project in projects {
        let threads: Vec<Entry> = entries
            .iter()
            .map(|(p, e)| {
                let mut e = e.clone();
                if *p != project {
                    e.2 = false;
                }
                e
            })
            .collect();
        results.extend(
            worktree::cleanup_candidates_in(&threads, &project, &[])
                .into_iter()
                .filter(|c| c.reason == "変更なし・保管済み")
                .map(|c| (project.clone(), c)),
        );
    }
    let threads: Vec<Entry> = entries.iter().map(|(_, e)| e.clone()).collect();
    results.extend(
        worktree::cleanup_candidates_in(&threads, Path::new(""), bases)
            .into_iter()
            .map(|c| (PathBuf::new(), c)),
    );
    results
}

impl Workspace {
    pub(super) fn worktree_base(&self) -> PathBuf {
        location(self.ui["worktree_dir"].as_str().unwrap_or(""))
            .unwrap_or_else(|_| greencode_core::home().join("worktrees"))
    }

    pub(super) fn save_worktree_dir(&mut self, cx: &mut Context<Self>) {
        let value = self.storage.field.read(cx).value().trim().to_string();
        match location(&value) {
            Ok(_) => {
                self.ui["worktree_dir"] = json!(value);
                self.storage.error = None;
                self.save_ui();
            }
            Err(e) => self.storage.error = Some(e.into()),
        }
        cx.notify();
    }

    pub(super) fn cleanup(&mut self, remove: bool, automatic: bool, cx: &mut Context<Self>) {
        if self.storage.busy {
            return;
        }
        let entries: Vec<_> = self
            .threads
            .iter()
            .filter_map(|t| {
                t.meta.repo.as_ref().map(|p| {
                    (
                        p.clone(),
                        (
                            t.meta.id.clone(),
                            t.meta.cwd.clone(),
                            t.archived && t.status == Status::Idle && t.queue.is_empty(),
                            String::new(),
                        ),
                    )
                })
            })
            .collect();
        let mut bases = vec![
            greencode_core::home().join("worktrees"),
            self.worktree_base(),
        ];
        bases.extend(
            entries
                .iter()
                .filter_map(|(_, e)| e.1.parent().map(Path::to_path_buf)),
        );
        bases.sort();
        bases.dedup();
        let approved: Vec<PathBuf> = self
            .storage
            .candidates
            .iter()
            .filter(|(_, c)| c.reason == "変更なし・保管済み")
            .map(|(_, c)| c.path.clone())
            .collect();
        self.storage.busy = true;
        let task = self.rt.spawn_blocking(move || {
            let candidates = scan(&entries, &bases);
            let mut removed = 0usize;
            let mut errors = Vec::new();
            if remove || automatic {
                for (project, c) in &candidates {
                    if c.reason != "変更なし・保管済み"
                        || (!automatic && !approved.contains(&c.path))
                    {
                        continue;
                    }
                    match worktree::remove_worktree(project, &c.path) {
                        Ok(()) => removed += 1,
                        Err(e) => errors.push(format!("{}: {e:#}", c.path.display())),
                    }
                }
            }
            (candidates, removed, errors)
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| {
                this.storage.busy = false;
                match result {
                    Ok((candidates, removed, errors)) => {
                        this.storage.candidates = if remove || automatic {
                            Vec::new()
                        } else {
                            candidates
                        };
                        if remove || automatic {
                            this.notice(format!(
                                "クリーンアップ: {removed} 件削除{}",
                                if errors.is_empty() {
                                    String::new()
                                } else {
                                    format!("\n{}", errors.join("\n"))
                                }
                            ));
                        } else {
                            this.notice(format!(
                                "クリーンアップ候補: {} 件",
                                this.storage.candidates.len()
                            ));
                        }
                    }
                    Err(e) => this.fail(format!("クリーンアップに失敗しました: {e}")),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::location;
    #[test]
    fn storage_rejects_relative_location() {
        assert!(location("relative/path").is_err());
        assert!(location("/absolute/path").is_ok());
    }
}
