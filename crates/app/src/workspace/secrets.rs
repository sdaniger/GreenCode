use super::*;
use greencode_core::secrets;

pub(super) struct Secrets {
    name: Entity<InputState>,
    account: Entity<InputState>,
    reference: Entity<InputState>,
    /// The private field of an agent's `secret` request; masked and never mirrored into the draft.
    pub(super) entry: Entity<InputState>,
    _sub: Subscription,
    /// Latest add/remove/test result, shown on the settings page: (ok, message).
    outcome: Option<(bool, String)>,
}

impl Secrets {
    pub(super) fn new(window: &mut Window, cx: &mut Context<Workspace>) -> Self {
        let mut field = |hint: &'static str, cx: &mut Context<Workspace>| cx.new(|cx| InputState::new(window, cx).placeholder(hint));
        let name = field("名前（例: 1password）", cx);
        let account = field("アカウント（例: my.1password.com）", cx);
        let reference = field("op://vault/item/field", cx);
        let entry = cx.new(|cx| {
            InputState::new(window, cx)
                .masked(true)
                .placeholder("値を入力（会話には残りません）")
        });
        let _sub = cx.subscribe_in(&entry, window, |this, _, ev: &InputEvent, window, cx| {
            if matches!(ev, InputEvent::PressEnter { shift: false, .. }) {
                this.secret_save(window, cx);
            }
        });
        Self { name, account, reference, entry, _sub, outcome: None }
    }
}

impl Workspace {
    pub(super) fn secrets_section(&self, col: SettingsPage, pal: Pal, cx: &mut Context<Self>) -> SettingsPage {
        let small = |id: &'static str, label: String, act: Act, cx: &mut Context<Self>| {
            chip(id, label, pal)
                .border_1()
                .border_color(pal.line)
                .bg(pal.panel)
                .text_color(pal.fg)
                .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| this.run(act.clone(), window, cx)))
        };
        let mut col = col.section("シークレット（1Password）");
        for (i, p) in self.cfg.secret_providers.iter().enumerate() {
            col = col.child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(div().flex_1().truncate().child(format!("{}  ·  {}", p.name, p.account)))
                    .child(small("secret-remove", "削除".into(), Act::SecretRemove(i), cx).id(("secret-remove", i))),
            );
        }
        col.child(
            div()
                .flex()
                .gap_2()
                .items_center()
                .child(div().flex_1().child(Input::new(&self.secrets.name)))
                .child(div().flex_1().child(Input::new(&self.secrets.account)))
                .child(small("secret-add", "追加".into(), Act::SecretAdd, cx)),
        )
        .child(
            div()
                .flex()
                .gap_2()
                .items_center()
                .child(div().flex_1().child(Input::new(&self.secrets.reference)))
                .child(small("secret-test", "読み取りテスト".into(), Act::SecretTest, cx)),
        )
        .children(self.secrets.outcome.as_ref().map(|(ok, text)| {
            div()
                .text_xs()
                .text_color(if *ok { pal.muted } else { gpui_kit::red() })
                .child(text.clone())
        }))
        .child(div().text_xs().text_color(pal.muted).child("api_key や env に op://vault/item/field と書けます"))
    }

    fn say_secret(&mut self, ok: bool, text: impl Into<String>) {
        self.secrets.outcome = Some((ok, text.into()));
    }

    fn save_secret_providers(&mut self, list: Vec<secrets::SecretProvider>) -> bool {
        match secrets::save_providers(&list) {
            Ok(()) => {
                secrets::set_providers(&list);
                secrets::clear_cache();
                Arc::make_mut(&mut self.cfg).secret_providers = list;
                true
            }
            Err(e) => {
                self.say_secret(false, format!("シークレットプロバイダーを保存できません: {e:#}"));
                false
            }
        }
    }

    pub(super) fn secret_add(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let [name, account] = [&self.secrets.name, &self.secrets.account].map(|f| f.read(cx).value().trim().to_string());
        if name.is_empty() || account.is_empty() {
            return self.say_secret(false, "名前とアカウントを入力してください");
        }
        let mut list = self.cfg.secret_providers.clone();
        list.retain(|p| p.name != name);
        list.push(secrets::SecretProvider { name: name.clone(), account });
        if self.save_secret_providers(list) {
            self.say_secret(true, format!("{name} を追加しました"));
        }
        for f in [&self.secrets.name, &self.secrets.account] {
            f.update(cx, |s, cx| s.set_value("", window, cx));
        }
    }

    pub(super) fn secret_remove(&mut self, index: usize) {
        let mut list = self.cfg.secret_providers.clone();
        if index >= list.len() {
            return;
        }
        let gone = list.remove(index);
        // Only the first provider's account is used for reads, so removing any other changes nothing.
        let users = if index == 0 { secrets::dependents(&self.cfg) } else { Vec::new() };
        if !self.save_secret_providers(list) {
            return;
        }
        if users.is_empty() {
            self.say_secret(true, format!("{} を削除しました", gone.name));
        } else {
            self.say_secret(
                false,
                format!("{} を削除しました。次の項目の op:// 参照は読めなくなります: {}", gone.name, users.join("、")),
            );
        }
    }

    pub(super) fn secret_test(&mut self, cx: &mut Context<Self>) {
        let reference = self.secrets.reference.read(cx).value().trim().to_string();
        if cfg!(test) {
            return;
        }
        self.say_secret(true, "読み取り中…");
        let task = self.rt.spawn_blocking(move || secrets::test(&reference, "読み取りテスト").map_err(|e| format!("{e:#}")));
        cx.spawn(async move |this, cx| {
            let result = task.await.unwrap_or_else(|e| Err(e.to_string()));
            this.update(cx, |this, cx| {
                match result {
                    Ok(()) => this.say_secret(true, "1Password から読み取れました"),
                    Err(e) => this.say_secret(false, e),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Saves the value typed into the dialog's private field. The field is cleared and the composer
    /// regains focus, so the value never reaches the transcript or the persisted draft.
    pub(super) fn secret_save(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let value = self.secrets.entry.read(cx).value().trim_end().to_string();
        if value.is_empty() {
            return self.notice("値を入力してから保存してください");
        }
        self.secrets.entry.update(cx, |s, cx| {
            s.set_value("", window, cx);
            s.set_selected_range(0..0, cx);
        });
        self.input.update(cx, |s, cx| s.focus(window, cx));
        self.answer_dialog(json!({"value": value}), cx);
    }

    pub(super) fn secret_refuse(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.secrets.entry.update(cx, |s, cx| {
            s.set_value("", window, cx);
            s.set_selected_range(0..0, cx);
        });
        self.input.update(cx, |s, cx| s.focus(window, cx));
        self.answer_dialog(json!({"cancelled": true}), cx);
    }
}
