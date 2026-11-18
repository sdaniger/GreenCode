//! Per-turn worktree checkpoints, so undo also reverts changes made through `bash`.
use super::*;

impl Workspace {
    /// Records the tracked worktree state before a native turn; keeps the last 50 per thread.
    pub(super) fn record_checkpoint(&mut self, ix: usize) {
        let t = &self.threads[ix];
        let Some(sha) = greencode_core::checkpoint::create(&t.meta.cwd) else {
            return;
        };
        let id = t.meta.id.clone();
        if !self.ui["checkpoints"].is_object() {
            self.ui["checkpoints"] = json!({});
        }
        let list = self.ui["checkpoints"][id.as_str()]
            .as_array()
            .cloned()
            .unwrap_or_default();
        let mut list = list;
        list.push(json!(sha));
        let len = list.len();
        if len > 50 {
            list.drain(..len - 50);
        }
        self.ui["checkpoints"][id.as_str()] = json!(list);
        self.save_ui();
    }

    /// Restores the checkpoint of the turn `back` before the last one; returns the restored file count.
    pub(super) fn restore_checkpoint(&mut self, back: usize) -> Option<usize> {
        let t = self.thread();
        let list = self.ui["checkpoints"][t.meta.id.as_str()].as_array()?;
        let sha = list
            .get(list.len().checked_sub(back + 1)?)?
            .as_str()?
            .to_string();
        let cwd = t.meta.cwd.clone();
        match greencode_core::checkpoint::restore(&cwd, &sha) {
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
        let Some(sha) = self.ui["checkpoints"][t.meta.id.as_str()]
            .as_array()
            .and_then(|l| l.last())
            .and_then(Value::as_str)
            .map(String::from)
        else {
            self.notice("このスレッドにはまだチェックポイントがありません");
            return;
        };
        let cwd = t.meta.cwd.clone();
        match greencode_core::checkpoint::diff(&cwd, &sha) {
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
