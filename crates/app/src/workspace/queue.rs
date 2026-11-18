use super::*;

pub(super) struct QueueEdit {
    pub index: usize,
    pub draft: String,
    pub original: String,
    pub removed: bool,
}

impl Workspace {
    pub(super) fn edit_queue(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        if self.queue_edit.is_some() {
            self.finish_queue_edit(false, window, cx);
        }
        let Some(text) = self.thread().queue.get(index).cloned() else {
            return;
        };
        let draft = self.input.read(cx).value().to_string();
        self.queue_edit = Some(QueueEdit {
            index,
            draft,
            original: text.clone(),
            removed: false,
        });
        self.input.update(cx, |s, cx| {
            super::set_input(s, text, window, cx);
            s.focus(window, cx);
        });
        cx.notify();
    }

    pub(super) fn finish_queue_edit(
        &mut self,
        save: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(edit) = self.queue_edit.take() else {
            return;
        };
        let text = self.input.read(cx).value().to_string();
        if save && !edit.removed {
            if let Some(queued) = self.threads[self.active].queue.get_mut(edit.index) {
                *queued = text.clone();
            }
        }
        let draft = if edit.removed && edit.draft.is_empty() && text != edit.original {
            text
        } else {
            edit.draft
        };
        self.threads[self.active].draft = draft.clone();
        self.input
            .update(cx, |s, cx| super::set_input(s, draft, window, cx));
        if let Err(e) = self.thread().save() {
            self.fail(e);
        }
        cx.notify();
    }

    pub(super) fn queue_removed(&mut self, index: usize) {
        if let Some(edit) = &mut self.queue_edit {
            if edit.index == index {
                edit.removed = true;
            } else if edit.index > index {
                edit.index -= 1;
            }
        }
    }

    pub(super) fn remove_queue(
        &mut self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.threads[self.active].queue.remove(index).is_none() {
            return;
        }
        self.queue_removed(index);
        if self.queue_edit.as_ref().is_some_and(|e| e.removed) {
            self.finish_queue_edit(false, window, cx);
        }
        if let Err(e) = self.thread().save() {
            self.fail(e);
        }
        cx.notify();
    }

    /// Moves a queued message into an empty composer for editing, as T3 does.
    pub(super) fn move_queue_to_composer(
        &mut self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.input.read(cx).value().trim().is_empty() {
            self.notice("入力欄が空のときだけ下書きへ移せます");
            return;
        }
        let Some(text) = self.threads[self.active].queue.remove(index) else {
            return;
        };
        self.queue_removed(index);
        if self.queue_edit.as_ref().is_some_and(|e| e.removed) {
            self.finish_queue_edit(false, window, cx);
        }
        self.threads[self.active].draft = text.clone();
        self.input.update(cx, |s, cx| {
            super::set_input(s, text, window, cx);
            s.focus(window, cx);
        });
        if let Err(e) = self.thread().save() {
            self.fail(e);
        }
        cx.notify();
    }

    pub(super) fn send_queue_now(
        &mut self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let steer = self.thread().status == Status::Running
            && can_steer(&self.cfg, &self.thread().meta.backend)
            && self.thread().cmd.is_some();
        let Some(text) = self.threads[self.active].queue.remove(index) else {
            return;
        };
        self.queue_removed(index);
        if self.queue_edit.as_ref().is_some_and(|e| e.removed) {
            self.finish_queue_edit(false, window, cx);
        }
        if steer {
            if let Some(cmd) = &self.thread().cmd {
                let _ = cmd.send(Command::Steer(text.clone()));
            }
            self.threads[self.active]
                .items
                .push(Item::User(text.into()));
            self.list.reset(self.thread().items.len());
            self.list.scroll_to_end();
        } else if self.thread().status == Status::Idle {
            self.threads[self.active].queue_held = false;
            self.start_prompt(text, cx);
        } else {
            self.threads[self.active].queue.push_front(text);
            if let Some(edit) = &mut self.queue_edit {
                edit.index += 1;
            }
        }
        if let Err(e) = self.thread().save() {
            self.fail(e);
        }
        cx.notify();
    }
}
