//! `/login` and `/logout`: sign-in status, the API-key form, and the Claude subscription sign-in.

use super::*;
use greencode_core::auth::{self, Method};

pub(super) fn method_label(m: &Method) -> String {
    match m {
        Method::Subscription => "ブラウザでログイン済み".to_string(),
        Method::StoredKey => "キー保存済み".to_string(),
        Method::ConfigKey => "config.toml のキー".to_string(),
        Method::Env(v) => format!("{v} 設定済み"),
        Method::Keyless => "キー不要".to_string(),
        Method::None(_) => "未ログイン".to_string(),
    }
}

/// One masked field that stores (or replaces) a provider's API key.
pub(super) struct KeyForm {
    pub(super) provider: String,
    pub(super) field: Entity<InputState>,
    pub(super) error: Option<SharedString>,
    _sub: Subscription,
}

/// One field naming the account that the provider's current credential is saved as.
pub(super) struct AccountForm {
    pub(super) provider: String,
    pub(super) field: Entity<InputState>,
    pub(super) error: Option<SharedString>,
    _sub: Subscription,
}

#[derive(Clone, Copy, PartialEq)]
pub(super) enum DeviceKind {
    Copilot,
    ChatGpt,
    Mistral,
    Antigravity,
    /// A provider in `greencode_core::login::PROVIDERS`.
    Login(&'static str),
}

impl DeviceKind {
    fn title(self) -> String {
        match self {
            DeviceKind::ChatGpt => "ChatGPT (Codex) でログイン".into(),
            DeviceKind::Mistral => "Mistral Vibe でログイン".into(),
            _ => format!("{} でログイン", self.name()),
        }
    }

    fn provider(self) -> &'static str {
        match self {
            DeviceKind::Copilot => "github-copilot",
            DeviceKind::ChatGpt => "chatgpt",
            DeviceKind::Mistral => "mistral",
            DeviceKind::Antigravity => "antigravity",
            DeviceKind::Login(name) => name,
        }
    }

    fn name(self) -> &'static str {
        match self {
            DeviceKind::Copilot => "GitHub Copilot",
            DeviceKind::ChatGpt => "ChatGPT",
            DeviceKind::Mistral => "Mistral",
            DeviceKind::Antigravity => "Google Antigravity",
            DeviceKind::Login(name) => greencode_core::config::preset(name).map_or(name, |p| p.label),
        }
    }

    /// The sign-in kind of `name` when it has a browser or device login of its own.
    pub(super) fn login(name: &str) -> Option<DeviceKind> {
        greencode_core::login::PROVIDERS.iter().find(|p| **p == name).map(|p| DeviceKind::Login(p))
    }
}

/// A freshly issued device code, whichever service issued it.
enum Issued {
    Copilot(greencode_core::provider::copilot::Device),
    ChatGpt(greencode_core::provider::codex::Device),
    Mistral(greencode_core::provider::mistral::SignIn),
    Antigravity(greencode_core::antigravity::LoginStart),
    Login(greencode_core::login::Start),
}

/// What a finished device flow produced.
enum Granted {
    Copilot(String),
    ChatGpt(greencode_core::provider::codex::Tokens),
    /// A regular Mistral API key billed against the Vibe plan.
    Mistral(String),
    Antigravity(greencode_core::antigravity::LoginDone),
    Login(greencode_core::login::Cred),
}

/// Device or browser sign-in: the code or URL is shown while the browser approves it.
pub(super) struct DeviceForm {
    pub(super) kind: DeviceKind,
    /// `None` until the service has issued the code.
    pub(super) code: Option<SharedString>,
    pub(super) url: Option<SharedString>,
    pub(super) pending: Option<tokio::task::AbortHandle>,
    /// Tells a late result from an older form apart from the current one.
    pub(super) id: u64,
    pub(super) callback: Option<(Entity<InputState>, Subscription)>,
    /// Where a pasted redirect goes for `DeviceKind::Login` sign-ins.
    pub(super) paste: Option<tokio::sync::mpsc::UnboundedSender<String>>,
    pub(super) error: Option<SharedString>,
}

impl Workspace {
    pub(super) fn start_device_login(
        &mut self,
        kind: DeviceKind,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        static NEXT_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        if let Some(h) = self.device_form.take().and_then(|f| f.pending) {
            h.abort();
        }
        let id = NEXT_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.device_form = Some(DeviceForm {
            kind,
            code: None,
            url: None,
            pending: None,
            id,
            callback: None,
            paste: None,
            error: None,
        });
        cx.notify();
        // No network from UI tests.
        if cfg!(test) {
            return;
        }
        let cfg = (*self.cfg).clone();
        let cwd = self.thread().project();
        let task = self.rt.spawn(async move {
            match kind {
                DeviceKind::Copilot => greencode_core::provider::copilot::start()
                    .await
                    .map(Issued::Copilot),
                DeviceKind::ChatGpt => greencode_core::provider::codex::start()
                    .await
                    .map(Issued::ChatGpt),
                DeviceKind::Mistral => greencode_core::provider::mistral::start()
                    .await
                    .map(Issued::Mistral),
                DeviceKind::Antigravity => {
                    greencode_core::antigravity::start_login(&cfg, None, cwd)
                        .await
                        .map(Issued::Antigravity)
                }
                DeviceKind::Login(name) => greencode_core::login::start(name).await.map(Issued::Login),
            }
        });
        self.device_form.as_mut().unwrap().pending = Some(task.abort_handle());
        cx.spawn_in(window, async move |this, cx| {
            let result: Result<_, String> = match task.await {
                Ok(r) => r.map_err(|e| format!("{e:#}")),
                Err(e) if e.is_cancelled() => return,
                Err(e) => Err(e.to_string()),
            };
            this.update_in(cx, |this, window, cx| {
                if !this.device_form.as_ref().is_some_and(|f| f.id == id) {
                    return;
                }
                match result {
                    Ok(device) => this.await_device_approval(kind, device, id, window, cx),
                    Err(e) => {
                        this.close_device_form(window, cx);
                        this.fail(format!("{} ログインに失敗しました: {e}", kind.name()));
                    }
                }
            })
            .ok();
        })
        .detach();
    }

    fn await_device_approval(
        &mut self,
        kind: DeviceKind,
        mut device: Issued,
        id: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Issued::Antigravity(greencode_core::antigravity::LoginStart::Ready(done)) = device {
            self.close_device_form(window, cx);
            self.reload_registry_backends();
            if !done.models.is_empty() {
                self.backend_models.insert("antigravity".into(), done.models);
            }
            if !done.options.is_empty() {
                self.backend_options.insert("antigravity".into(), done.options);
            }
            self.notice("Google Antigravity にログインしました。");
            self.open_model_picker_for(Some("antigravity".into()), window, cx);
            cx.notify();
            return;
        }
        let cwd = self.thread().project();
        let paste = match &mut device {
            Issued::Login(start) => start.paste.take(),
            _ => None,
        };
        let cb = if paste.is_some()
            || matches!(
                &device,
                Issued::Antigravity(greencode_core::antigravity::LoginStart::Browser(_))
            ) {
            let field = cx.new(|cx| {
                InputState::new(window, cx)
                    .placeholder("リモート等で 127.0.0.1 に戻れない場合のみリダイレクト先 URL を貼り付けて ⏎")
            });
            let sub = cx.subscribe_in(&field, window, |this, _, ev: &InputEvent, window, cx| {
                if let InputEvent::PressEnter { .. } = ev {
                    this.submit_antigravity_callback(window, cx);
                }
            });
            Some((field, sub))
        } else {
            None
        };
        let Some(form) = &mut self.device_form else {
            return;
        };
        let (code, url) = match &device {
            Issued::Copilot(d) => (Some(d.user_code.clone()), d.verification_uri.clone()),
            Issued::ChatGpt(d) => (Some(d.user_code.clone()), d.verification_uri.clone()),
            Issued::Mistral(s) => (None, s.url.clone()),
            Issued::Antigravity(greencode_core::antigravity::LoginStart::Browser(p)) => {
                (None, p.url.clone())
            }
            Issued::Antigravity(greencode_core::antigravity::LoginStart::Ready(_)) => unreachable!(),
            Issued::Login(s) => (s.code.clone(), s.url.clone()),
        };
        if let Some(code) = &code {
            cx.write_to_clipboard(ClipboardItem::new_string(code.clone()));
        }
        spawn_external("/usr/bin/open", &[&url]);
        form.code = code.map(Into::into);
        form.url = Some(url.into());
        form.callback = cb;
        form.paste = paste;
        form.error = None;
        let task = self.rt.spawn(async move {
            match device {
                Issued::Copilot(d) => greencode_core::provider::copilot::finish(d)
                    .await
                    .map(Granted::Copilot),
                Issued::ChatGpt(d) => greencode_core::provider::codex::finish(d)
                    .await
                    .map(Granted::ChatGpt),
                Issued::Mistral(s) => greencode_core::provider::mistral::finish(s)
                    .await
                    .map(Granted::Mistral),
                Issued::Antigravity(greencode_core::antigravity::LoginStart::Browser(pending)) => {
                    greencode_core::antigravity::finish_login(pending, cwd)
                        .await
                        .map(Granted::Antigravity)
                }
                Issued::Antigravity(greencode_core::antigravity::LoginStart::Ready(_)) => {
                    unreachable!()
                }
                Issued::Login(start) => start.done.await.map(Granted::Login),
            }
        });
        form.pending = Some(task.abort_handle());
        cx.notify();
        cx.spawn_in(window, async move |this, cx| {
            let result: Result<Granted, String> = match task.await {
                Ok(r) => r.map_err(|e| format!("{e:#}")),
                Err(e) if e.is_cancelled() => return,
                Err(e) => Err(e.to_string()),
            };
            this.update_in(cx, |this, window, cx| {
                if !this.device_form.as_ref().is_some_and(|f| f.id == id) {
                    return;
                }
                this.close_device_form(window, cx);
                match result {
                    Ok(Granted::Antigravity(done)) => {
                        this.reload_registry_backends();
                        if !done.models.is_empty() {
                            this.backend_models.insert("antigravity".into(), done.models);
                        }
                        if !done.options.is_empty() {
                            this.backend_options.insert("antigravity".into(), done.options);
                        }
                        this.notice("Google Antigravity にログインしました。");
                        this.open_model_picker_for(Some("antigravity".into()), window, cx);
                    }
                    Ok(granted) => {
                        let saved = match granted {
                            Granted::Copilot(token) => {
                                auth::set_token(Arc::make_mut(&mut this.cfg), "github-copilot", &token)
                            }
                            Granted::ChatGpt(tokens) => greencode_core::provider::codex::save(
                                &greencode_core::provider::codex::path(),
                                &tokens,
                            ),
                            Granted::Mistral(key) => {
                                auth::set_key(Arc::make_mut(&mut this.cfg), "mistral", &key)
                            }
                            Granted::Login(cred) => auth::save(Arc::make_mut(&mut this.cfg), kind.provider(), cred),
                            Granted::Antigravity(_) => unreachable!(),
                        }
                        .map_err(|e| format!("{e:#}"));
                        match saved {
                            Ok(()) => {
                                this.push_config();
                                this.notice(format!("{} にログインしました。", kind.name()));
                                this.refresh_models(cx);
                                this.open_palette(&format!("モデル: {}/", kind.provider()), window, cx);
                            }
                            Err(e) => this.fail(format!("{} ログインに失敗しました: {e}", kind.name())),
                        }
                    }
                    Err(e) => this.fail(format!("{} ログインに失敗しました: {e}", kind.name())),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Hands a pasted redirect URL to the sign-in in progress.
    pub(super) fn submit_antigravity_callback(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(form) = &mut self.device_form else {
            return;
        };
        let Some((field, _)) = &form.callback else {
            return;
        };
        let value = field.read(cx).value().trim().to_string();
        if value.is_empty() {
            return;
        }
        if let Some(paste) = &form.paste {
            // The sign-in task reports a bad paste as its result; a closed channel means it already ended.
            let _ = paste.send(value);
            return;
        }
        let auth_url = form.url.as_ref().map(|u| u.to_string());
        let id = form.id;
        form.error = None;
        cx.notify();
        let task = self.rt.spawn(async move {
            greencode_core::antigravity::submit_callback_url(&value, auth_url.as_deref()).await
        });
        cx.spawn_in(window, async move |this, cx| {
            let result: Result<(), String> = match task.await {
                Ok(r) => r.map_err(|e| format!("{e:#}")),
                Err(e) if e.is_cancelled() => return,
                Err(e) => Err(e.to_string()),
            };
            this.update_in(cx, |this, _, cx| {
                let Some(form) = &mut this.device_form else {
                    return;
                };
                if form.id != id {
                    return;
                }
                match result {
                    Ok(()) => form.error = None,
                    Err(e) => form.error = Some(e.into()),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Busy label of the Antigravity install button.
    pub(super) const INSTALLING: &'static str = "インストール中…";

    pub(super) fn install_antigravity(&mut self, cx: &mut Context<Self>) {
        // Progress lives on the settings button, so repeated clicks cannot fill the chat.
        if self.antigravity_install == Some(Self::INSTALLING) {
            return;
        }
        self.antigravity_install = Some(Self::INSTALLING);
        cx.notify();
        let task = self.rt.spawn(async { greencode_core::antigravity::install_or_update().await });
        cx.spawn(async move |this, cx| {
            let result: Result<_, String> = match task.await {
                Ok(r) => r.map_err(|e| format!("{e:#}")),
                Err(e) => Err(e.to_string()),
            };
            this.update(cx, |this, cx| {
                match result {
                    Ok(_) => {
                        this.antigravity_install = Some("インストール済み ✓");
                        this.reload_registry_backends();
                        this.refresh_backend_models(true, cx);
                    }
                    Err(e) => {
                        this.antigravity_install = None;
                        this.fail(format!("Google Antigravity ランタイムをインストールできません: {e}"));
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    pub(super) fn close_device_form(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(handle) = self.device_form.take().and_then(|f| f.pending) {
            handle.abort();
        }
        self.input.update(cx, |s, cx| s.focus(window, cx));
        cx.notify();
    }

    pub(super) fn device_form_view(
        &self,
        pal: Pal,
        cx: &mut Context<Self>,
    ) -> Option<impl IntoElement> {
        let form = self.device_form.as_ref()?;
        let code = match &form.code {
            Some(c) => div()
                .text_size(px(28.))
                .font_family(cx.theme().mono_font_family.clone())
                .child(c.clone()),
            None if form.url.is_some() => div(),
            None if form.kind == DeviceKind::Antigravity => div()
                .text_sm()
                .text_color(pal.muted)
                .child("ランタイムと認証 URL を準備中…"),
            None => div()
                .text_sm()
                .text_color(pal.muted)
                .child("コードを取得中…"),
        };
        let mut actions = div().flex().items_center().gap_1();
        if let Some(url) = form.url.clone() {
            actions = actions
                .child(chip("device-open", "ブラウザを開く", pal).on_click(
                    move |_: &ClickEvent, _, _| spawn_external("/usr/bin/open", &[&url]),
                ));
        }
        if let Some(code) = form.code.clone() {
            actions = actions.child(chip("device-copy", "コードをコピー", pal).on_click(
                move |_: &ClickEvent, _, cx| {
                    cx.write_to_clipboard(ClipboardItem::new_string(code.to_string()))
                },
            ));
        } else if matches!(form.kind, DeviceKind::Antigravity | DeviceKind::Login(_)) {
            if let Some(url) = form.url.clone() {
                actions = actions.child(chip("device-copy-url", "URLをコピー", pal).on_click(
                    move |_: &ClickEvent, _, cx| {
                        cx.write_to_clipboard(ClipboardItem::new_string(url.to_string()))
                    },
                ));
            }
        }
        let hint = match form.kind {
            DeviceKind::Mistral => "ブラウザで Mistral アカウントにサインインし、Mistral Vibe へのアクセスを承認してください",
            DeviceKind::Antigravity => "ブラウザで Google アカウントにサインインして承認してください",
            DeviceKind::Login(_) if form.code.is_none() => "ブラウザでサインインして承認してください",
            _ => "ブラウザで次のコードを入力してください（クリップボードにコピー済み）",
        };
        actions = actions.child(div().flex_1()).child(
            chip("device-cancel", "キャンセル  esc", pal).on_click(
                cx.listener(|this, _: &ClickEvent, window, cx| this.close_device_form(window, cx)),
            ),
        );
        let mut col = div()
            .w(px(540.))
            .flex()
            .flex_col()
            .gap_2()
            .p_3()
            .self_start()
            .rounded_xl()
            .border_1()
            .border_color(pal.line)
            .bg(pal.card)
            .shadow_lg()
            .child(form.kind.title())
            .child(div().text_xs().text_color(pal.muted).child(hint))
            .child(code);
        if let Some(url) = &form.url {
            col = col
                .child(div().text_xs().text_color(pal.muted).child(url.clone()))
                .child(
                    div()
                        .text_xs()
                        .text_color(pal.muted)
                        .child("承認を待っています…"),
                );
        }
        if let Some((field, _)) = &form.callback {
            col = col.child(Input::new(field));
        }
        if let Some(e) = &form.error {
            col = col.child(div().text_xs().text_color(gpui_kit::red()).child(e.clone()));
        }
        col = col.child(actions);
        Some(
            div()
                .absolute()
                .inset_0()
                .flex()
                .justify_center()
                .pt(px(110.))
                .child(col),
        )
    }
}

impl Workspace {
    /// Started threads hold their own config copy.
    pub(super) fn push_config(&self) {
        for t in &self.threads {
            if let Some(cmd) = &t.cmd {
                let _ = cmd.send(Command::Config(self.cfg.clone()));
            }
        }
    }

    pub(super) fn open_key_form(
        &mut self,
        name: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let field = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("APIキーを貼り付けて ⏎")
                .masked(true)
        });
        let sub = cx.subscribe_in(&field, window, |this, _, ev: &InputEvent, window, cx| {
            if let InputEvent::PressEnter { .. } = ev {
                this.submit_key(window, cx)
            }
        });
        field.update(cx, |s, cx| s.focus(window, cx));
        self.key_form = Some(KeyForm {
            provider: name,
            field,
            error: None,
            _sub: sub,
        });
    }

    pub(super) fn close_key_form(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.key_form = None;
        self.input.update(cx, |s, cx| s.focus(window, cx));
        cx.notify();
    }

    pub(super) fn submit_key(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(form) = &self.key_form else { return };
        let key = form.field.read(cx).value().trim().to_string();
        if key.is_empty() {
            return;
        }
        let provider = form.provider.clone();
        match auth::set_key(Arc::make_mut(&mut self.cfg), &provider, &key) {
            Ok(()) => {
                self.push_config();
                self.close_key_form(window, cx);
                self.notice(format!("{provider} にログインしました。"));
                self.refresh_models(cx);
                self.open_palette(&format!("モデル: {provider}/"), window, cx);
            }
            Err(e) => {
                self.key_form.as_mut().unwrap().error = Some(format!("{e:#}").into());
                cx.notify();
            }
        }
    }

    pub(super) fn close_oauth_form(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(handle) = self.oauth_form.take().and_then(|f| f.pending) {
            handle.abort();
        }
        self.input.update(cx, |s, cx| s.focus(window, cx));
        cx.notify();
    }

    pub(super) fn submit_oauth(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(form) = &mut self.oauth_form else {
            return;
        };
        let code = form.field.read(cx).value().trim().to_string();
        if code.is_empty() {
            return;
        }
        let Some(login) = form.login.take() else {
            return;
        };
        let url = form.url.clone();
        let task = self.rt.spawn(async move {
            login
                .exchange_and_save(&code, &greencode_core::provider::anthropic_oauth::path())
                .await
        });
        form.pending = Some(task.abort_handle());
        cx.notify();
        cx.spawn_in(window, async move |this, cx| {
            // `crates/app` has no `anyhow` dependency: flatten join and exchange errors to text.
            let result: Result<(), String> = match task.await {
                Ok(r) => r.map_err(|e| format!("{e:#}")),
                Err(e) if e.is_cancelled() => return,
                Err(e) => Err(e.to_string()),
            };
            this.update_in(cx, |this, window, cx| {
                // A newer login form must not be closed by an older result.
                if this.oauth_form.as_ref().is_some_and(|f| f.url == url) {
                    this.oauth_form = None;
                    this.input.update(cx, |s, cx| s.focus(window, cx));
                }
                match result {
                    Ok(()) => {
                        if let Err(e) = auth::forget_key(Arc::make_mut(&mut this.cfg), "anthropic")
                        {
                            this.fail(format!("{e:#}"));
                        }
                        this.push_config();
                        this.notice("Claude サブスクリプションでログインしました。");
                        this.refresh_models(cx);
                        this.open_palette("モデル: anthropic/", window, cx);
                    }
                    Err(e) => this.fail(format!(
                        "Claude ログインに失敗しました（/login からやり直してください）: {e}"
                    )),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    pub(super) fn switch_account(&mut self, provider: &str, account: &str, cx: &mut Context<Self>) {
        match auth::switch(Arc::make_mut(&mut self.cfg), provider, account) {
            Ok(()) => {
                self.push_config();
                let prefix = format!("{provider}/");
                self.fetched_models.retain(|m| !m.starts_with(&prefix));
                self.notice(format!(
                    "{provider} のアカウントを {account} に切り替えました"
                ));
                self.refresh_models(cx);
            }
            Err(e) => self.fail(format!("{e:#}")),
        }
        cx.notify();
    }

    pub(super) fn open_account_form(
        &mut self,
        provider: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let field = cx.new(|cx| {
            InputState::new(window, cx).placeholder("アカウント名（例: work）を入力して ⏎")
        });
        let sub = cx.subscribe_in(&field, window, |this, _, ev: &InputEvent, window, cx| {
            if let InputEvent::PressEnter { .. } = ev {
                this.submit_account(window, cx)
            }
        });
        field.update(cx, |s, cx| s.focus(window, cx));
        self.account_form = Some(AccountForm {
            provider,
            field,
            error: None,
            _sub: sub,
        });
        cx.notify();
    }

    pub(super) fn close_account_form(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.account_form = None;
        self.input.update(cx, |s, cx| s.focus(window, cx));
        cx.notify();
    }

    pub(super) fn submit_account(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(form) = &self.account_form else {
            return;
        };
        let account = form.field.read(cx).value().trim().to_string();
        if account.is_empty() {
            return;
        }
        let provider = form.provider.clone();
        match auth::save_as(Arc::make_mut(&mut self.cfg), &provider, &account) {
            Ok(()) => {
                self.close_account_form(window, cx);
                self.notice(format!(
                    "{provider} の現在のログインをアカウント {account} として保存しました"
                ));
            }
            Err(e) => {
                self.account_form.as_mut().unwrap().error = Some(format!("{e:#}").into());
                cx.notify();
            }
        }
    }

    pub(super) fn account_form_view(
        &self,
        pal: Pal,
        cx: &mut Context<Self>,
    ) -> Option<impl IntoElement> {
        let form = self.account_form.as_ref()?;
        let actions = div()
            .flex()
            .items_center()
            .gap_1()
            .child(div().flex_1())
            .child(chip("account-cancel", "キャンセル  esc", pal).on_click(
                cx.listener(|this, _: &ClickEvent, window, cx| this.close_account_form(window, cx)),
            ))
            .child(chip("account-save", "保存  ⏎", pal).on_click(
                cx.listener(|this, _: &ClickEvent, window, cx| this.submit_account(window, cx)),
            ));
        let mut col = div()
            .w(px(540.))
            .flex()
            .flex_col()
            .gap_2()
            .p_3()
            .self_start()
            .rounded_xl()
            .border_1()
            .border_color(pal.line)
            .bg(pal.card)
            .shadow_lg()
            .child(format!("{} のアカウント名", form.provider))
            .child(
                div()
                    .text_xs()
                    .text_color(pal.muted)
                    .child("現在のログインをこの名前で保存し、後からパレットで切り替えられます。"),
            )
            .child(Input::new(&form.field))
            .child(actions);
        if let Some(e) = &form.error {
            col = col.child(div().text_xs().text_color(gpui_kit::red()).child(e.clone()));
        }
        Some(
            div()
                .absolute()
                .inset_0()
                .flex()
                .justify_center()
                .pt(px(110.))
                .child(col),
        )
    }

    pub(super) fn logout(&mut self, name: &str) {
        let canonical = greencode_core::antigravity::canonical_backend(name);
        match auth::logout(Arc::make_mut(&mut self.cfg), canonical) {
            Ok(()) => {
                self.push_config();
                if canonical == greencode_core::antigravity::BACKEND {
                    self.backend_models.remove(canonical);
                    self.backend_options.remove(canonical);
                    for t in &mut self.threads {
                        if greencode_core::antigravity::is_alias(&t.meta.backend)
                            && t.status == Status::Idle
                        {
                            t.cmd = None;
                        }
                    }
                    self.notice("Google Antigravity からログアウトしました。");
                } else {
                    let prefix = format!("{canonical}/");
                    self.fetched_models.retain(|m| !m.starts_with(&prefix));
                    self.notice(format!("{canonical} からログアウトしました。"));
                }
            }
            Err(e) => self.fail(format!("{e:#}")),
        }
    }

    /// Rows of the `/login` picker, before the query filter.
    pub(super) fn login_rows(&self) -> Vec<(String, Act)> {
        let status = |name: &str, p: &ProviderCfg| auth::status(name, p);
        let suffix = |name: &str| {
            auth::accounts(name)
                .ok()
                .and_then(|(active, _)| active)
                .filter(|a| a != "default")
                .map(|a| format!(" · {a}"))
                .unwrap_or_default()
        };
        let claude = match self
            .cfg
            .providers
            .get("anthropic")
            .map(|p| status("anthropic", p))
        {
            Some(Method::Subscription) => format!(
                "Claude Pro/Max でログイン（ブラウザ）   ✓ ログイン中{}",
                suffix("anthropic")
            ),
            _ => "Claude Pro/Max でログイン（ブラウザ）".to_string(),
        };
        let copilot = match self
            .cfg
            .providers
            .get("github-copilot")
            .map(|p| status("github-copilot", p))
        {
            Some(Method::Subscription) => format!(
                "GitHub Copilot でログイン（ブラウザ）   ✓ ログイン中{}",
                suffix("github-copilot")
            ),
            _ => "GitHub Copilot でログイン（ブラウザ）".to_string(),
        };
        let chatgpt = match self
            .cfg
            .providers
            .get("chatgpt")
            .map(|p| status("chatgpt", p))
        {
            Some(Method::Subscription) => format!(
                "ChatGPT Plus/Pro でログイン（ブラウザ）   ✓ ログイン中{}",
                suffix("chatgpt")
            ),
            _ => "ChatGPT Plus/Pro でログイン（ブラウザ）".to_string(),
        };
        let antigravity = if greencode_core::antigravity::status().signed_in {
            "Google Antigravity でログイン（ブラウザ）   ✓ ログイン中".to_string()
        } else {
            "Google Antigravity でログイン（ブラウザ）".to_string()
        };
        let mut rows = vec![
            (claude, Act::AnthropicLogin),
            (copilot, Act::CopilotLogin),
            (chatgpt, Act::ChatGptLogin),
            (
                "Mistral Vibe でログイン（ブラウザ）".into(),
                Act::MistralLogin,
            ),
            (antigravity, Act::AntigravityLogin),
        ];
        // Mistral keeps its generic API-key row: the Vibe sign-in only mints a regular key.
        let dedicated = |name: &str| matches!(name, "github-copilot" | "chatgpt");
        for (name, p) in &self.cfg.providers {
            let m = status(name, p);
            if dedicated(name) || (name == "anthropic" && m == Method::Subscription) {
                continue;
            }
            if m.signed_in() && m != Method::Keyless {
                rows.push((
                    format!("{name}   ✓ {}{}", method_label(&m), suffix(name)),
                    Act::LoginProvider(name.clone()),
                ));
            }
        }
        for p in greencode_core::config::PRESETS {
            let name = p.name;
            if dedicated(name) {
                continue;
            }
            let listed = self.cfg.providers.get(name).is_some_and(|p| {
                let m = status(name, p);
                m.signed_in() && m != Method::Keyless
            });
            if !listed {
                // The id stays in the row so typing it in the palette still finds it.
                let row = if p.label.eq_ignore_ascii_case(name) { p.label.to_string() } else { format!("{}  ({name})", p.label) };
                rows.push((row, Act::LoginProvider(name.to_string())));
            }
        }
        rows.push((
            "OpenCode でログイン（ターミナル）".into(),
            Act::OpenCodeLogin,
        ));
        rows.push((
            "Hermes をセットアップ（ターミナル）".into(),
            Act::HermesSetup("hermes".into()),
        ));
        #[cfg(unix)]
        rows.push((
            "既存の Claude Code / ChatGPT ログインを取り込む".into(),
            Act::ImportLogins,
        ));
        rows.push(("＋ カスタムプロバイダを追加…".into(), Act::AddProvider));
        rows
    }

    pub(super) fn key_form_view(
        &self,
        pal: Pal,
        cx: &mut Context<Self>,
    ) -> Option<impl IntoElement> {
        let form = self.key_form.as_ref()?;
        let mut actions = div().flex().items_center().gap_1();
        let browser = match form.provider.as_str() {
            "anthropic" => Some(("Claude Pro/Max でログイン（ブラウザ）", Act::AnthropicLogin)),
            "mistral" => Some(("Mistral Vibe でログイン（ブラウザ）", Act::MistralLogin)),
            "google" => Some(("Google Antigravity でログイン（ブラウザ）", Act::AntigravityLogin)),
            name if DeviceKind::login(name).is_some() => Some(("ブラウザでログイン", Act::SignIn(name.to_string()))),
            _ => None,
        };
        if let Some((label, act)) = browser {
            actions = actions.child(chip("key-browser", label, pal).on_click(cx.listener(
                move |this, _: &ClickEvent, window, cx| {
                    this.close_key_form(window, cx);
                    this.run(act.clone(), window, cx);
                },
            )));
        }
        if let Some(url) = greencode_core::config::preset(&form.provider).map(|p| p.key_url).filter(|u| !u.is_empty()) {
            actions = actions.child(chip("key-get", "キーを取得", pal).on_click(move |_, _, _| {
                let _ = open::that_detached(url);
            }));
        }
        actions = actions
            .child(div().flex_1())
            .child(chip("key-cancel", "キャンセル  esc", pal).on_click(
                cx.listener(|this, _: &ClickEvent, window, cx| this.close_key_form(window, cx)),
            ))
            .child(chip("key-save", "保存  ⏎", pal).on_click(
                cx.listener(|this, _: &ClickEvent, window, cx| this.submit_key(window, cx)),
            ));
        let mut col = div()
            .w(px(540.))
            .flex()
            .flex_col()
            .gap_2()
            .p_3()
            .self_start()
            .rounded_xl()
            .border_1()
            .border_color(pal.line)
            .bg(pal.card)
            .shadow_lg()
            .child(format!(
                "{} の API キー",
                greencode_core::config::preset(&form.provider).map_or(form.provider.as_str(), |p| p.label)
            ))
            .child(div().text_xs().text_color(pal.muted).child("キーは ~/.greencode/auth.json（600）に保存されます。既存のキーは置き換えられます。"))
            .child(Input::new(&form.field).mask_toggle())
            .child(actions);
        if let Some(e) = &form.error {
            col = col.child(div().text_xs().text_color(gpui_kit::red()).child(e.clone()));
        }
        Some(
            div()
                .absolute()
                .inset_0()
                .flex()
                .justify_center()
                .pt(px(110.))
                .child(col),
        )
    }
}
