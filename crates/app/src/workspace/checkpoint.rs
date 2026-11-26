//! Per-turn worktree checkpoints, so undo also reverts changes made through `bash`.
//! A project with several repositories stores one snapshot per repository (an object keyed by path).
use super::*;
use greencode_core::repos;
use std::collections::BTreeMap;

fn snapshots(v: &Value) -> Option<BTreeMap<String, String>> {
    Some(v.as_object()?.iter().filter_map(|(k, s)| Some((k.clone(), s.as_str()?.to_string()))).collect())
}

impl Workspace {
    /// Repositories to snapshot separately, or `None` for a plain single-repository folder.
    fn multi_repos(&self, ix: usize) -> Option<Vec<PathBuf>> {
        let found = self.repos_of(&self.threads[ix]);
        let cwd = &self.threads[ix].meta.cwd;
        (found.len() > 1 || found.first().is_some_and(|r| r != cwd)).then_some(found)
    }

    /// Records the tracked worktree state before a native turn; keeps the last 50 per thread.
    pub(super) fn record_checkpoint(&mut self, ix: usize) {
        let t = &self.threads[ix];
        let entry = match self.multi_repos(ix) {
            Some(found) => json!(repos::checkpoint(&found)),
            None => match greencode_core::checkpoint::create(&t.meta.cwd) {
                Some(sha) => json!(sha),
                None => return,
            },
        };
        let id = t.meta.id.clone();
        if !self.ui["checkpoints"].is_object() {
            self.ui["checkpoints"] = json!({});
        }
        let mut list = self.ui["checkpoints"][id.as_str()]
            .as_array()
            .cloned()
            .unwrap_or_default();
        list.push(entry);
        let len = list.len();
        if len > 50 {
            list.drain(..len - 50);
        }
        self.ui["checkpoints"][id.as_str()] = json!(list);
        self.save_ui();
    }

    /// Restores the checkpoint of the turn `back` before the last one; returns the restored file count.
    /// With several repositories, names the ones that had no snapshot or are plain folders.
    pub(super) fn restore_checkpoint(&mut self, back: usize) -> Option<usize> {
        let t = self.thread();
        let list = self.ui["checkpoints"][t.meta.id.as_str()].as_array()?;
        let entry = list.get(list.len().checked_sub(back + 1)?)?.clone();
        let cwd = t.meta.cwd.clone();
        if let Some(snaps) = snapshots(&entry) {
            let (n, mut skipped) = repos::restore(&cwd, &self.thread_repos(), &snaps);
            // A worktree thread's root is a layout folder of symlinks and repositories, not a plain project copy.
            if t.meta.repo.is_some() {
                skipped.retain(|p| *p != cwd);
            }
            if !skipped.is_empty() {
                let names = skipped.iter().map(|p| repos::label(&cwd, p)).collect::<Vec<_>>().join(", ");
                self.notice(format!("戻せなかった（スナップショットなし・プレーンフォルダ）: {names}"));
            }
            return Some(n);
        }
        match greencode_core::checkpoint::restore(&cwd, entry.as_str()?) {
            Ok(n) => Some(n),
            Err(e) => {
                self.fail(format!("bash の変更を戻せません: {e:#}"));
                None
            }
        }
    }

    /// Shows the tracked diff of the last turn's checkpoint in the diff panel.
    pub(super) fn bash_diff(&mut self, cx: &mut Context<Self>) {
        let t = self.thread();
        let Some(entry) = self.ui["checkpoints"][t.meta.id.as_str()]
            .as_array()
            .and_then(|l| l.last())
            .cloned()
        else {
            self.notice("このスレッドにはまだチェックポイントがありません");
            return;
        };
        let cwd = t.meta.cwd.clone();
        let diff = match snapshots(&entry) {
            Some(snaps) => Ok(repos::checkpoint_diff(&cwd, &self.thread_repos(), &snaps)),
            None => greencode_core::checkpoint::diff(&cwd, entry.as_str().unwrap_or_default()),
        };
        match diff {
            Ok(diff) if diff.trim().is_empty() => {
                self.notice("チェックポイント以降、追跡ファイルの変更はありません")
            }
            Ok(diff) => {
                self.diff = diff
                    .lines()
                    .map(|l| SharedString::from(l.to_string()))
                    .collect();
                self.show_diff = true;
                self.files_panel = None;
                cx.notify();
            }
            Err(e) => self.fail(format!("差分を取得できません: {e:#}")),
        }
    }
}
