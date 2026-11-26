use super::*;

struct Connection {
    key: String,
    name: String,
    status: String,
    version: Option<String>,
    toggle: Option<(&'static str, bool)>,
}

impl Workspace {
    fn connections(&self) -> Vec<Connection> {
        let mut rows = Vec::new();
        let disabled_providers = self.disabled("disabled_providers");
        let disabled_backends = self.disabled("disabled_backends");
        for (name, p) in &self.cfg.providers {
            let method = greencode_core::auth::status(name, p);
            if !method.signed_in() && !PRESET_SHOWN.contains(&name.as_str()) {
                continue;
            }
            let account = greencode_core::auth::accounts(name)
                .ok()
                .and_then(|(active, _)| active);
            let mut status = if disabled_providers.contains(&name.to_string()) {
                "無効".to_string()
            } else {
                method_label(&method)
            };
            if let Some(account) = account.filter(|_| status != "無効") {
                status.push_str(&format!(" · {account}"));
            }
            rows.push(Connection {
                key: format!("provider:{name}"),
                name: name.clone(),
                status,
                version: None,
                toggle: Some(("disabled_providers", disabled_providers.contains(name))),
            });
        }
        for backend in &self.cfg.backends {
            let off = disabled_backends.contains(&backend.name);
            let (status, version) = if backend.name == greencode_core::antigravity::BACKEND {
                let installed = greencode_core::antigravity::is_runtime_installed(backend);
                let st = greencode_core::antigravity::status();
                let status = if off {
                    "無効".into()
                } else if !installed {
                    "未インストール".into()
                } else if st.signed_in {
                    match st.auth_type.as_str() {
                        "oauth-business" => "Google ログイン済み · 組織".into(),
                        "gemini-api-key" => "API キー設定済み".into(),
                        _ => "Google ログイン済み".into(),
                    }
                } else {
                    "未ログイン".into()
                };
                let version = greencode_core::antigravity::installed_version().or_else(|| {
                    self.agent_versions
                        .get(backend.name.as_str())
                        .and_then(Clone::clone)
                });
                (status, version)
            } else {
                let status = if off {
                    "無効".into()
                } else if backend
                    .command
                    .first()
                    .and_then(|c| greencode_core::config::executable(c))
                    .is_none()
                {
                    format!(
                        "未検出 · {} が見つかりません",
                        backend
                            .command
                            .first()
                            .map(String::as_str)
                            .unwrap_or("実行ファイル")
                    )
                } else {
                    "利用可能".into()
                };
                let version = self
                    .agent_versions
                    .get(backend.name.as_str())
                    .and_then(Clone::clone);
                (status, version)
            };
            rows.push(Connection {
                key: format!("backend:{}", backend.name),
                name: backend.name.clone(),
                status,
                version,
                toggle: Some(("disabled_backends", off)),
            });
        }
        for agent in greencode_core::agents::AGENTS {
            if self.cfg.backends.iter().any(|b| b.name == agent.name) {
                continue;
            }
            let version = self.agent_versions.get(agent.name).and_then(Clone::clone);
            let status = match self.agent_versions.get(agent.name) {
                Some(Some(v)) => format!("v{v}"),
                Some(None) => "未インストール".into(),
                None if cfg!(test) => "未インストール".into(),
                None => "確認中…".into(),
            };
            rows.push(Connection {
                key: format!("agent:{}", agent.name),
                name: agent.name.into(),
                status,
                version,
                toggle: None,
            });
        }
        rows
    }

    pub(super) fn connection_selection(&self) -> String {
        self.connection_selection_in(&self.connections())
    }

    fn connection_selection_in(&self, rows: &[Connection]) -> String {
        if let Some(key) = self.ui["connection_selected"]
            .as_str()
            .filter(|key| rows.iter().any(|r| r.key == *key))
        {
            return key.to_string();
        }
        self.cfg
            .providers
            .iter()
            .find(|(name, p)| greencode_core::auth::status(name, p).signed_in())
            .map(|(name, _)| format!("provider:{name}"))
            .or_else(|| rows.first().map(|r| r.key.clone()))
            .unwrap_or_default()
    }

    pub(super) fn connection_display_name(&self, key: &str) -> String {
        self.ui["connection_names"][key]
            .as_str()
            .map(str::to_string)
            .unwrap_or_else(|| {
                key.split_once(':')
                    .map_or(key, |(_, name)| name)
                    .to_string()
            })
    }

    pub(super) fn select_connection(&mut self, key: String, window: &mut Window, cx: &mut Context<Self>) {
        let display = self.connection_display_name(&key);
        self.ui["connection_selected"] = json!(key);
        self.save_ui();
        if let Some((field, _)) = &self.connection_name {
            field.update(cx, |s, cx| s.set_value(display, window, cx));
        }
        self.fill_connection_fields(window, cx);
        cx.notify();
    }

    pub(super) fn fill_connection_fields(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((fields, _)) = &self.connection_fields else { return };
        let key = self.connection_selection();
        let (kind, name) = key.split_once(':').unwrap_or((key.as_str(), ""));
        let over = (kind == "backend").then(|| self.cfg.backend_overrides.get(name)).flatten();
        let values = [
            over.and_then(|o| o.executable.clone()).unwrap_or_default(),
            over.map(|o| o.args.join(" ")).unwrap_or_default(),
            over.map(|o| o.env.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join(" ")).unwrap_or_default(),
            String::new(),
            String::new(),
        ];
        for (field, value) in fields.iter().zip(values) {
            field.update(cx, |s, cx| s.set_value(value, window, cx));
        }
    }

    fn connection_field_value(&self, index: usize, cx: &Context<Self>) -> Option<String> {
        Some(self.connection_fields.as_ref()?.0.get(index)?.read(cx).value().trim().to_string())
    }

    /// Enter or 保存 on one of the connections-tab inputs.
    pub(super) fn save_connection_field(&mut self, index: usize, _window: &mut Window, cx: &mut Context<Self>) {
        let Some((kind, name)) = self.connection_selection().split_once(':').map(|(k, n)| (k.to_string(), n.to_string())) else { return };
        let Some(value) = self.connection_field_value(index, cx) else { return };
        match (kind.as_str(), index) {
            ("backend", 0..=2) => {
                let field = ["executable", "args", "env"][index];
                let values: Vec<String> = if index == 0 {
                    (!value.is_empty()).then_some(value).into_iter().collect()
                } else {
                    value.split_whitespace().map(String::from).collect()
                };
                match greencode_core::config::set_backend_override(&name, field, &values) {
                    Ok(()) => {
                        self.reload_config();
                        self.notice(format!("バックエンド「{name}」の起動設定を保存しました"));
                    }
                    Err(e) => self.fail(format!("保存できません: {e:#}")),
                }
            }
            ("backend", 3) => self.change_backend_model(&name, &value, true),
            ("provider", 4) => self.change_provider_model(&name, &value, true),
            _ => {}
        }
        cx.notify();
    }

    /// − on a connections-tab model row.
    pub(super) fn remove_connection_field(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some((kind, name)) = self.connection_selection().split_once(':').map(|(k, n)| (k.to_string(), n.to_string())) else { return };
        let Some(value) = self.connection_field_value(index, cx) else { return };
        match (kind.as_str(), index) {
            ("backend", 3) => self.change_backend_model(&name, &value, false),
            ("provider", 4) => self.change_provider_model(&name, &value, false),
            _ => {}
        }
        cx.notify();
    }

    pub(super) fn reload_config(&mut self) {
        let mode = self.cfg.mode;
        match greencode_core::config::Config::load() {
            Ok(mut cfg) => {
                cfg.mode = mode;
                super::advanced::apply_ui(&mut cfg, &self.ui);
                self.cfg = Arc::new(cfg);
                self.cfg_error = None;
                self.push_config();
            }
            Err(e) => self.fail(format!("設定を読み込めません: {e:#}")),
        }
    }

    fn change_backend_model(&mut self, name: &str, model: &str, add: bool) {
        if model.is_empty() {
            self.notice("モデルIDを入力してください");
            return;
        }
        let mut models = self.cfg.backend_overrides.get(name).map(|o| o.models.clone()).unwrap_or_default();
        if add {
            if models.iter().any(|m| m == model) {
                self.notice("そのモデルは追加済みです");
                return;
            }
            models.push(model.into());
        } else if let Some(i) = models.iter().position(|m| m == model) {
            models.remove(i);
        } else {
            self.notice(format!("`{model}` は追加されていません"));
            return;
        }
        match greencode_core::config::set_backend_override(name, "models", &models) {
            Ok(()) => {
                self.reload_config();
                self.notice(if add { format!("モデルを追加しました: {model}") } else { format!("モデルを削除しました: {model}") });
            }
            Err(e) => self.fail(format!("保存できません: {e:#}")),
        }
    }

    fn change_provider_model(&mut self, name: &str, model: &str, add: bool) {
        if model.is_empty() {
            self.notice("モデルIDを入力してください");
            return;
        }
        let Some(p) = self.cfg.providers.get(name) else { return };
        let mut models = p.models.clone();
        if add {
            if models.iter().any(|m| m == model) {
                self.notice("そのモデルは追加済みです");
                return;
            }
            models.push(model.into());
        } else if let Some(i) = models.iter().position(|m| m == model) {
            models.remove(i);
        } else {
            self.notice(format!("`{model}` は追加されていません"));
            return;
        }
        let result = if models.is_empty() {
            greencode_core::config::settings::edit_global(&format!("providers.{name}.models"), None).map(|_| ())
        } else {
            greencode_core::config::set_provider_models(name, &models)
        };
        match result {
            Ok(()) => {
                Arc::make_mut(&mut self.cfg).providers.get_mut(name).unwrap().models = models;
                self.push_config();
                self.notice(if add { format!("モデルを追加しました: {model}") } else { format!("モデルを削除しました: {model}") });
            }
            Err(e) => self.fail(format!("保存できません: {e:#}")),
        }
    }

    pub(super) fn save_connection_name(&mut self, cx: &mut Context<Self>) {
        let Some((field, _)) = &self.connection_name else {
            return;
        };
        let name = field.read(cx).value().trim().to_string();
        let key = self.connection_selection();
        if key.is_empty() {
            return;
        }
        if !self.ui["connection_names"].is_object() {
            self.ui["connection_names"] = json!({});
        }
        if name.is_empty() {
            self.ui["connection_names"]
                .as_object_mut()
                .unwrap()
                .remove(&key);
        } else {
            self.ui["connection_names"][key.as_str()] = json!(name);
        }
        self.save_ui();
        cx.notify();
    }

    fn refresh_connections(&mut self, cx: &mut Context<Self>) {
        self.refresh_backend_models(true, cx);
        self.refresh_agent_versions(cx);
        self.ui["connections_checked"] = json!(now());
        self.save_ui();
        cx.notify();
    }

    pub(super) fn connections_view(&self, pal: Pal, cx: &mut Context<Self>) -> impl IntoElement {
        let rows = self.connections();
        let selected = self.connection_selection_in(&rows);
        let checked = self.ui["connections_checked"]
            .as_u64()
            .map(|t| format!("⟳ 最終確認 {}分前", now().saturating_sub(t) / 60))
            .unwrap_or_else(|| "⟳ 未確認".into());
        let header = div()
            .flex()
            .items_center()
            .justify_between()
            .pt_3()
            .pb_1()
            .child(chip("picker-add-connection", "+ 追加", pal).on_click(cx.listener(
                |this, _: &ClickEvent, window, cx| this.run(Act::AddProvider, window, cx),
            )))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(chip("connections-refresh", "再確認", pal).on_click(
                        cx.listener(|this, _: &ClickEvent, _, cx| this.refresh_connections(cx)),
                    ))
                    .child(div().text_xs().text_color(pal.muted).child(checked)),
            );
        let mut list = div()
            .id("connections-list")
            .w(px(280.))
            .h_full()
            .overflow_y_scroll()
            .flex_shrink_0()
            .flex()
            .flex_col()
            .border_r_1()
            .border_color(pal.line);
        for (i, row) in rows.iter().enumerate() {
            let key = row.key.clone();
            let mut item = div()
                .id(("connection-row", i))
                .flex()
                .items_center()
                .gap_1()
                .p_2()
                .when(row.key == selected, |d| d.bg(pal.selected))
                .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                    this.select_connection(key.clone(), window, cx)
                }))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .flex_col()
                        .child(
                            div().truncate().child(format!(
                                "{}{}",
                                self.connection_display_name(&row.key),
                                row.version
                                    .as_ref()
                                    .map(|v| format!("  v{v}"))
                                    .unwrap_or_default()
                            )),
                        )
                        .child(
                            div()
                                .text_xs()
                                .text_color(pal.muted)
                                .truncate()
                                .child(row.status.clone()),
                        ),
                );
            if let Some((kind, off)) = row.toggle {
                let name = row.name.clone();
                item = item.child(
                    chip(
                        ("connection-toggle", i),
                        if off { "無効" } else { "有効" },
                        pal,
                    )
                    .on_click(cx.listener(
                        move |this, _: &ClickEvent, window, cx| {
                            cx.stop_propagation();
                            this.run(Act::ToggleDisabled(kind, name.clone()), window, cx);
                        },
                    )),
                );
            }
            list = list.child(item);
        }
        list = list.child(chip("connection-add", "+ 接続を追加", pal).on_click(
            cx.listener(|this, _: &ClickEvent, window, cx| this.run(Act::AddProvider, window, cx)),
        ));
        let mut detail = div()
            .id("connection-detail")
            .flex_1()
            .min_w_0()
            .h_full()
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .gap_2()
            .p_3();
        if let Some(row) = rows.iter().find(|r| r.key == selected) {
            detail = detail
                .child(
                    div()
                        .font_weight(FontWeight::MEDIUM)
                        .child(self.connection_display_name(&row.key)),
                )
                .child(div().text_xs().text_color(pal.muted).child("ステータス"))
                .child(row.status.clone())
                .child(div().text_xs().text_color(pal.muted).child("表示名"));
            if let Some((field, _)) = &self.connection_name {
                detail = detail.child(
                    div()
                        .flex()
                        .items_center()
                        .gap_1()
                        .child(div().flex_1().min_w_0().child(Input::new(field)))
                        .child(
                            chip("connection-name-save", "＋", pal).on_click(cx.listener(
                                |this, _: &ClickEvent, _, cx| this.save_connection_name(cx),
                            )),
                        ),
                );
            }
            let (kind, name) = row.key.split_once(':').unwrap();
            if kind != "agent" {
                detail = detail.child(div().text_xs().text_color(pal.muted).child("セットアップ"));
                if kind == "provider" {
                    let (active, accounts) =
                        greencode_core::auth::accounts(name).unwrap_or_default();
                    let mut account_row = div()
                        .flex()
                        .items_center()
                        .justify_between()
                        .child("アカウント");
                    if accounts.len() > 1 {
                        let next = accounts
                            .iter()
                            .position(|a| Some(a.as_str()) == active.as_deref())
                            .map_or(0, |i| (i + 1) % accounts.len());
                        let act = Act::SwitchAccount(name.to_string(), accounts[next].clone());
                        account_row = account_row.child(
                            chip(
                                "connection-account",
                                format!("{} ▾", active.unwrap_or_else(|| "既定".into())),
                                pal,
                            )
                            .on_click(cx.listener(
                                move |this, _: &ClickEvent, window, cx| {
                                    this.run(act.clone(), window, cx)
                                },
                            )),
                        );
                    } else {
                        account_row = account_row.child(
                            div()
                                .text_color(pal.muted)
                                .child(active.unwrap_or_else(|| "既定".into())),
                        );
                    }
                    let signed_in =
                        greencode_core::auth::status(name, self.cfg.providers.get(name).unwrap())
                            .signed_in();
                    let keyless = matches!(
                        greencode_core::auth::status(name, self.cfg.providers.get(name).unwrap()),
                        greencode_core::auth::Method::Keyless
                    );
                    let act = if name == "anthropic" {
                        Act::AnthropicLogin
                    } else {
                        Act::LoginProvider(name.to_string())
                    };
                    let label = if signed_in {
                        "再サインイン"
                    } else {
                        "サインイン"
                    };
                    detail = detail.child(account_row).when(!keyless, |d| {
                        d.child(chip("connection-login", label, pal).on_click(cx.listener(
                            move |this, _: &ClickEvent, window, cx| {
                                this.run(act.clone(), window, cx)
                            },
                        )))
                    });
                } else if name == greencode_core::antigravity::BACKEND {
                    let st = greencode_core::antigravity::status();
                    let types: Vec<&'static str> = greencode_core::antigravity::AUTH_TYPES
                        .iter()
                        .map(|(k, _)| *k)
                        .collect();
                    let next = types
                        .iter()
                        .position(|k| *k == st.auth_type)
                        .map_or(types[0], |i| types[(i + 1) % types.len()]);
                    let auth_row = div()
                        .flex()
                        .items_center()
                        .justify_between()
                        .child("認証方式")
                        .child(
                            chip(
                                "connection-antigravity-auth",
                                format!(
                                    "{} ▾",
                                    greencode_core::antigravity::auth_type_label(&st.auth_type)
                                ),
                                pal,
                            )
                            .on_click(cx.listener(
                                move |this, _: &ClickEvent, window, cx| {
                                    this.run(Act::AntigravityAuthType(next.into()), window, cx)
                                },
                            )),
                        );
                    let login_label = if st.signed_in {
                        "再サインイン"
                    } else {
                        "Google でサインイン"
                    };
                    let install_label = if let Some(label) = self.antigravity_install {
                            label
                        } else if greencode_core::antigravity::installed_version().is_some() {
                            "ランタイムを更新"
                        } else {
                            "ランタイムをインストール"
                        };
                    let mut actions = div()
                        .flex()
                        .items_center()
                        .gap_1()
                        .child(chip("connection-login", login_label, pal).on_click(
                            cx.listener(|this, _: &ClickEvent, window, cx| {
                                this.run(Act::AntigravityLogin, window, cx)
                            }),
                        ))
                        .child(
                            chip("connection-antigravity-install", install_label, pal).on_click(
                                cx.listener(|this, _: &ClickEvent, window, cx| {
                                    this.run(Act::AntigravityInstall, window, cx)
                                }),
                            ),
                        );
                    if st.signed_in {
                        actions = actions.child(chip("connection-logout", "ログアウト", pal).on_click(
                            cx.listener(|this, _: &ClickEvent, window, cx| {
                                this.run(Act::Logout("antigravity".into()), window, cx)
                            }),
                        ));
                    }
                    detail = detail.child(auth_row).child(actions);
                } else {
                    let name = name.to_string();
                    detail = detail.child(chip("connection-login", "サインイン", pal).on_click(
                        cx.listener(move |this, _: &ClickEvent, window, cx| {
                            if name == "opencode" {
                                this.run(Act::OpenCodeLogin, window, cx);
                            } else {
                                this.notice(format!(
                                    "ターミナルで `{name}` を実行してログインしてください"
                                ));
                                cx.notify();
                            }
                        }),
                    ));
                }
                static HOSTNAME: std::sync::LazyLock<String> = std::sync::LazyLock::new(|| {
                    std::env::var("HOSTNAME")
                        .ok()
                        .filter(|s| !s.is_empty())
                        .or_else(|| {
                            std::process::Command::new("/bin/hostname")
                                .output()
                                .ok()
                                .and_then(|o| String::from_utf8(o.stdout).ok())
                                .map(|s| s.trim().to_string())
                                .filter(|s| !s.is_empty())
                        })
                        .unwrap_or_else(|| "このコンピュータ".into())
                });
                let hostname = &*HOSTNAME;
                detail = detail.child(
                    div()
                        .text_xs()
                        .text_color(pal.muted)
                        .child(format!("このマシン（{hostname}）でサインインします。")),
                );
            }
            detail = detail.child(div().text_xs().text_color(pal.muted).child("ランタイム"));
            if let Some(p) = (kind == "provider")
                .then(|| self.cfg.providers.get(name))
                .flatten()
            {
                let api = if p.api_key.is_some() {
                    "保存済み".into()
                } else if !p.api_key_env.is_empty() {
                    format!("環境変数 {}", p.api_key_env)
                } else {
                    "未設定".into()
                };
                detail = detail
                    .child(format!("ベースURL: {}", p.base_url))
                    .child(format!("APIキー: {api}"))
                    .child(format!(
                        "モデル: {}{}",
                        p.models.len(),
                        if p.models.is_empty() { String::new() } else { format!("（{}）", p.models.join(", ")) }
                    ));
                if let Some((fields, _)) = &self.connection_fields {
                    detail = detail
                        .child(div().text_xs().text_color(pal.muted).child("カスタムモデル（config.toml の providers に保存）"))
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap_1()
                                .child(div().flex_1().min_w_0().child(Input::new(&fields[4])))
                                .child(chip("connection-model-add", "＋", pal).on_click(cx.listener(
                                    |this, _: &ClickEvent, window, cx| this.save_connection_field(4, window, cx),
                                )))
                                .child(chip("connection-model-remove", "−", pal).on_click(cx.listener(
                                    |this, _: &ClickEvent, _, cx| this.remove_connection_field(4, cx),
                                ))),
                        );
                }
                if matches!(
                    greencode_core::auth::status(name, p),
                    greencode_core::auth::Method::StoredKey
                        | greencode_core::auth::Method::Subscription
                ) {
                    let act = Act::Logout(name.to_string());
                    detail = detail.child(chip("connection-logout", "ログアウト", pal).on_click(
                        cx.listener(move |this, _: &ClickEvent, window, cx| {
                            this.run(act.clone(), window, cx)
                        }),
                    ));
                }
            } else if let Some(b) = (kind == "backend")
                .then(|| self.cfg.backends.iter().find(|b| b.name == name))
                .flatten()
            {
                detail = detail
                    .child(format!(
                        "実行ファイル: {}",
                        b.command.first().map(String::as_str).unwrap_or("")
                    ))
                    .child(format!(
                        "引数: {}",
                        b.command
                            .iter()
                            .skip(1)
                            .cloned()
                            .collect::<Vec<_>>()
                            .join(" ")
                    ))
                    .child(format!(
                        "環境変数: {}",
                        b.env.keys().cloned().collect::<Vec<_>>().join(", ")
                    ));
                if let Some((fields, _)) = &self.connection_fields {
                    let field_row = |label: &'static str, i: usize, save: Option<&'static str>, remove: bool| {
                        let mut row = div()
                            .flex()
                            .items_center()
                            .gap_1()
                            .child(div().w(px(96.)).text_xs().text_color(pal.muted).child(label))
                            .child(div().flex_1().min_w_0().child(Input::new(&fields[i])));
                        if let Some(save) = save {
                            row = row.child(
                                chip(("connection-save", i), save, pal).on_click(cx.listener(
                                    move |this, _: &ClickEvent, window, cx| this.save_connection_field(i, window, cx),
                                )),
                            );
                        }
                        if remove {
                            row = row.child(
                                chip(("connection-remove", i), "−", pal).on_click(cx.listener(
                                    move |this, _: &ClickEvent, _, cx| this.remove_connection_field(i, cx),
                                )),
                            );
                        }
                        row
                    };
                    detail = detail
                        .child(div().text_xs().text_color(pal.muted).child(
                            "起動設定（config.toml の [backend_overrides] に保存。~ はホームに展開）",
                        ))
                        .child(field_row("実行ファイル", 0, Some("保存"), false))
                        .child(field_row("追加の引数", 1, Some("保存"), false))
                        .child(field_row("環境変数", 2, Some("保存"), false))
                        .child(field_row("追加モデル", 3, Some("＋"), true));
                }
            } else if kind == "agent" {
                if let Some(agent) = greencode_core::agents::AGENTS
                    .iter()
                    .find(|a| a.name == name)
                {
                    let label = if agent.install.is_empty() {
                        "ドキュメント"
                    } else if row.version.is_some() {
                        "更新"
                    } else {
                        "インストール"
                    };
                    let act = Act::InstallAgent(agent.name);
                    detail = detail.child(chip("connection-install", label, pal).on_click(
                        cx.listener(move |this, _: &ClickEvent, window, cx| {
                            this.run(act.clone(), window, cx)
                        }),
                    ));
                }
            }
        }
        // Fills the settings tab; the list and the detail scroll on their own.
        div().flex().flex_col().h_full().child(header).child(
            div()
                .flex()
                .flex_1()
                .min_h_0()
                .rounded_lg()
                .border_1()
                .border_color(pal.line)
                .child(list)
                .child(detail),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::open;
    use super::super::{Act, Workspace};
    use gpui_kit::TestAppContext;
    use std::sync::Arc;

    fn with_backend(ws: &mut Workspace) {
        Arc::make_mut(&mut ws.cfg)
            .providers
            .get_mut("openai")
            .unwrap()
            .api_key = Some("test-key".into());
        Arc::make_mut(&mut ws.cfg)
            .backends
            .push(greencode_core::config::AcpCfg {
                name: "test-backend".into(),
                command: vec!["missing-test-backend".into()],
                protocol: greencode_core::config::Protocol::Acp,
                env: Default::default(),
            });
    }

    #[gpui_kit::test]
    async fn connections_list_shows_providers_and_backends(cx: &mut TestAppContext) {
        let (ws, cx) = open(cx);
        ws.update(cx, |ws, _| with_backend(ws));
        ws.read_with(cx, |ws, _| {
            let rows = ws.connections();
            assert!(rows
                .iter()
                .any(|r| r.key == "provider:openai" && !r.status.is_empty()));
            assert!(rows
                .iter()
                .any(|r| r.key == "backend:test-backend" && !r.status.is_empty()));
        });
    }

    #[gpui_kit::test]
    async fn connection_display_name_saves(cx: &mut TestAppContext) {
        let (ws, cx) = open(cx);
        ws.update_in(cx, |ws, window, cx| {
            ws.run(Act::Settings, window, cx);
            ws.select_connection("provider:openai".into(), window, cx);
            ws.connection_name
                .as_ref()
                .unwrap()
                .0
                .update(cx, |s, cx| s.set_value("個人", window, cx));
            ws.save_connection_name(cx);
        });
        ws.read_with(cx, |ws, _| {
            assert_eq!(ws.connection_display_name("provider:openai"), "個人")
        });
    }

    #[gpui_kit::test]
    async fn connection_toggles_and_refresh_stamp(cx: &mut TestAppContext) {
        let (ws, cx) = open(cx);
        ws.update(cx, |ws, _| with_backend(ws));
        ws.update_in(cx, |ws, window, cx| {
            ws.run(
                Act::ToggleDisabled("disabled_providers", "openai".into()),
                window,
                cx,
            );
            ws.run(
                Act::ToggleDisabled("disabled_backends", "test-backend".into()),
                window,
                cx,
            );
            ws.select_connection("backend:test-backend".into(), window, cx);
            ws.refresh_connections(cx);
        });
        ws.read_with(cx, |ws, _| {
            assert!(ws
                .disabled("disabled_providers")
                .contains(&"openai".to_string()));
            assert!(ws
                .disabled("disabled_backends")
                .contains(&"test-backend".to_string()));
            assert!(ws
                .connections()
                .iter()
                .any(|r| r.key == ws.connection_selection() && r.status == "無効"));
            assert!(ws.ui["connections_checked"].as_u64().is_some());
        });
    }

    #[gpui_kit::test]
    async fn antigravity_connection_shows_and_cycles_auth_type(cx: &mut TestAppContext) {
        let (ws, cx) = open(cx);
        ws.update_in(cx, |ws, window, cx| {
            ws.run(Act::Settings, window, cx);
            assert!(ws
                .connections()
                .iter()
                .any(|r| r.key == "backend:antigravity"));
            ws.select_connection("backend:antigravity".into(), window, cx);
            ws.run(Act::AntigravityAuthType("oauth-business".into()), window, cx);
            assert_eq!(
                greencode_core::antigravity::status().auth_type,
                "oauth-business"
            );
            ws.run(
                Act::ToggleDisabled("disabled_backends", "antigravity".into()),
                window,
                cx,
            );
            assert!(ws
                .connections()
                .iter()
                .any(|r| r.key == "backend:antigravity" && r.status == "無効"));
        });
    }
}
