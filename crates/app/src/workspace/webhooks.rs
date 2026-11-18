use super::*;
use greencode_core::webhook::{self, Hook};

impl Workspace {
    fn webhook_port(&self) -> u16 {
        self.ui["webhook_port"]
            .as_u64()
            .and_then(|p| u16::try_from(p).ok())
            .filter(|p| *p != 0)
            .unwrap_or(47321)
    }

    fn webhook_sync(&mut self) {
        *self.webhooks.lock().unwrap() =
            serde_json::from_value(self.ui["webhooks"].clone()).unwrap_or_default();
    }

    pub(super) fn start_webhooks(&mut self, cx: &mut Context<Self>) {
        if self.cfg_error.is_some()
            || self
                .cfg
                .policy
                .as_ref()
                .is_some_and(|p| p.limits.webhooks == Some(false))
        {
            return;
        }
        self.webhook_sync();
        if cfg!(test) || self.webhook_server.is_some() || self.webhooks.lock().unwrap().is_empty() {
            return;
        }
        let (tx, mut rx) = mpsc::unbounded_channel::<Result<(String, String), String>>();
        let hooks = self.webhooks.clone();
        let port = self.webhook_port();
        let task = self.rt.spawn(async move {
            let sender = tx.clone();
            if let Err(e) = webhook::serve(
                port,
                move |id, prompt| {
                    let _ = sender.send(Ok((id, prompt)));
                },
                hooks,
            )
            .await
            {
                let _ = tx.send(Err(format!("Webhook サーバーを開始できません: {e:#}")));
            }
        });
        self.webhook_server = Some(task.abort_handle());
        cx.spawn(async move |this, cx| {
            while let Some(event) = rx.recv().await {
                if this
                    .update(cx, |this, cx| {
                        match event {
                            Ok((id, prompt)) => {
                                let thread = this
                                    .webhooks
                                    .lock()
                                    .unwrap()
                                    .iter()
                                    .find(|h| h.id == id)
                                    .map(|h| h.thread.clone());
                                if let Some(ix) = thread.and_then(|id| {
                                    this.threads.iter().position(|t| t.meta.id == id)
                                }) {
                                    if this.threads[ix].status == Status::Idle {
                                        this.threads[ix].queue_held = false;
                                        this.start_in(ix, prompt, cx);
                                    } else {
                                        this.threads[ix].queue.push_back(prompt);
                                        if let Err(e) = this.threads[ix].save() {
                                            this.fail(e);
                                        }
                                    }
                                }
                            }
                            Err(e) => {
                                this.webhook_server = None;
                                this.fail(e);
                            }
                        }
                        cx.notify();
                    })
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
    }

    pub(super) fn create_webhook(&mut self, prompt: &str, cx: &mut Context<Self>) {
        let hook = match Hook::new(self.thread().meta.id.clone(), prompt.to_string()) {
            Ok(h) => h,
            Err(e) => {
                self.fail(format!("Webhook を作成できません: {e:#}"));
                return;
            }
        };
        let url = format!("http://127.0.0.1:{}/hooks/{}", self.webhook_port(), hook.id);
        if !self.ui["webhooks"].is_array() {
            self.ui["webhooks"] = json!([]);
        }
        self.ui["webhooks"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::to_value(hook).unwrap());
        self.save_ui();
        self.start_webhooks(cx);
        cx.write_to_clipboard(ClipboardItem::new_string(url.clone()));
        self.notice(format!("Webhook URL: {url}"));
    }

    /// Replaces a hook's id, so the old URL stops working (T3's Rotate).
    pub(super) fn rotate_webhook(&mut self, id: &str, cx: &mut Context<Self>) {
        let Some(thread) = self.ui["webhooks"]
            .as_array()
            .and_then(|hooks| hooks.iter().find(|h| h["id"].as_str() == Some(id)))
            .and_then(|h| h["thread"].as_str().map(String::from))
        else {
            self.fail("Webhook が見つかりません");
            return;
        };
        let Some(fresh) = Hook::new(thread, String::new()).ok().map(|h| h.id) else {
            self.fail("Webhook を更新できません");
            return;
        };
        let Some(hook) = self.ui["webhooks"]
            .as_array_mut()
            .and_then(|hooks| hooks.iter_mut().find(|h| h["id"].as_str() == Some(id)))
        else {
            return;
        };
        hook["id"] = json!(fresh);
        self.save_ui();
        self.webhook_sync();
        let url = format!("http://127.0.0.1:{}/hooks/{fresh}", self.webhook_port());
        cx.write_to_clipboard(ClipboardItem::new_string(url.clone()));
        self.notice(format!(
            "Webhook URL を更新しました（古い URL は無効）: {url}"
        ));
    }

    pub(super) fn webhook_secret(&mut self, arg: &str) {
        let (id, secret) = arg.split_once(char::is_whitespace).unwrap_or(("", ""));
        let secret = secret.trim();
        if id.is_empty() || secret.is_empty() {
            self.fail("使い方: /webhook-secret ID SECRET");
            return;
        }
        let Some(hook) = self.ui["webhooks"]
            .as_array_mut()
            .and_then(|hooks| hooks.iter_mut().find(|h| h["id"].as_str() == Some(id)))
        else {
            self.fail("Webhook が見つかりません");
            return;
        };
        hook["secret"] = json!(secret);
        self.save_ui();
        self.webhook_sync();
        self.notice("Webhook のシークレットを設定しました");
    }

    pub(super) fn webhook_rows(&self) -> Vec<(String, Act)> {
        self.ui["webhooks"]
            .as_array()
            .into_iter()
            .flatten()
            .flat_map(|h| {
                let id = h["id"].as_str().unwrap_or("").to_string();
                let prompt: String = h["prompt"]
                    .as_str()
                    .unwrap_or("")
                    .chars()
                    .take(40)
                    .collect();
                [
                    (
                        format!("Webhook: {prompt}（URL をコピー）"),
                        Act::CopyWebhook(id.clone()),
                    ),
                    (format!("Webhook: {prompt}（削除）"), Act::DeleteWebhook(id)),
                ]
            })
            .collect()
    }

    pub(super) fn copy_webhook(&mut self, id: &str, cx: &mut Context<Self>) {
        let url = format!("http://127.0.0.1:{}/hooks/{id}", self.webhook_port());
        cx.write_to_clipboard(ClipboardItem::new_string(url.clone()));
        self.notice(format!("Webhook URL: {url}"));
    }

    pub(super) fn delete_webhook(&mut self, id: &str) {
        if let Some(hooks) = self.ui["webhooks"].as_array_mut() {
            hooks.retain(|h| h["id"].as_str() != Some(id));
        }
        self.save_ui();
        self.webhook_sync();
        self.notice("Webhook を削除しました");
    }
}
