//! 詳細設定: every native `config.toml` setting (`greencode-cli config list/set/reset`) in the GUI,
//! 動作設定: the native settings catalog grouped by what it controls.
use super::*;
use greencode_core::config::settings::{self, Native, UiSetting};
use std::collections::HashMap;

/// Rows shown before the filter narrows them; the full list is several hundred keys.
const SHOWN: usize = 80;

/// Area names, in `settings::UI_TABS` order.
const RUNTIME_CATEGORIES: [&str; 10] = [
    "ターミナルの表示", "モデル", "操作", "会話履歴", "記憶", "ファイル", "シェル", "ツール", "タスク", "プロバイダ",
];

/// Native keys the GUI also keeps in `ui.json` (quick toggles elsewhere on the settings screen), with that field.
/// A `config.toml` edit drops the override so the edit takes effect.
const UI_OVERRIDES: [(&str, &str); 6] = [
    ("model", "model"),
    ("effort", "effort"),
    ("learning.auto", "learning_auto"),
    ("ext_off", "ext_off"),
    ("compaction_enabled", "compaction"),
    ("compaction", "compaction"),
];

pub(super) struct Advanced {
    filter: Entity<InputState>,
    pub(super) value: Entity<InputState>,
    _subs: [Subscription; 2],
    /// Loaded when the tab opens and after each edit, not on every frame.
    pub(super) rows: Vec<settings::Row>,
    pub(super) key: Option<String>,
    /// Latest load/save result: (ok, message).
    pub(super) outcome: Option<(bool, String)>,
    catalog: Vec<UiSetting>,
    pub(super) runtime_category: usize,
    /// Catalog choices that the native key accepts, by native key; computed with `rows`.
    pub(super) choices: HashMap<&'static str, Vec<&'static str>>,
}

impl Advanced {
    pub(super) fn new(window: &mut Window, cx: &mut Context<Workspace>) -> Self {
        let filter = cx.new(|cx| InputState::new(window, cx).placeholder("設定を検索（例: bash、tui.composer、compaction）"));
        let value = cx.new(|cx| InputState::new(window, cx).placeholder("値（TOML: true / 12 / \"text\" / [\"a\", \"b\"]）"));
        let _subs = [
            cx.subscribe_in(&filter, window, |_, _, ev: &InputEvent, _, cx| {
                if matches!(ev, InputEvent::Change) {
                    cx.notify();
                }
            }),
            cx.subscribe_in(&value, window, |this, _, ev: &InputEvent, window, cx| {
                if matches!(ev, InputEvent::PressEnter { .. }) {
                    this.run(Act::SettingSave, window, cx);
                }
            }),
        ];
        Self {
            filter,
            value,
            _subs,
            rows: Vec::new(),
            key: None,
            outcome: None,
            catalog: settings::ui_settings(),
            runtime_category: 0,
            choices: HashMap::new(),
        }
    }
}

/// The text a value is edited as: strings bare (as `config set` takes them), the rest as TOML.
fn edit_text(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::String(s) => s.clone(),
        Value::Object(o) => {
            let fields: Vec<_> = o.iter().map(|(k, v)| format!("{} = {}", Value::String(k.clone()), toml_value(v))).collect();
            format!("{{ {} }}", fields.join(", "))
        }
        other => toml_value(other),
    }
}

/// JSON scalars and arrays are already TOML; nested tables need TOML inline syntax.
fn toml_value(v: &Value) -> String {
    match v {
        Value::Object(_) => edit_text(v),
        Value::Array(a) => format!("[{}]", a.iter().map(toml_value).collect::<Vec<_>>().join(", ")),
        other => other.to_string(),
    }
}

/// Applies the `ui.json` fields that override `config.toml` (see `UI_OVERRIDES`), except `mode`.
pub(super) fn apply_ui(cfg: &mut greencode_core::config::Config, ui: &Value) {
    if let Some(effort) = ui["effort"].as_str() {
        cfg.effort = Some(effort.into());
    }
    if let Some(model) = ui["model"].as_str() {
        cfg.model = model.into();
    }
    if let Some(auto) = ui["learning_auto"].as_bool() {
        cfg.learning.auto = auto;
    }
    if let Some(off) = ui["ext_off"].as_array() {
        cfg.ext_off = off.iter().filter_map(Value::as_str).map(String::from).collect();
    }
    match ui["compaction"].as_str() {
        Some("off") => cfg.compaction_enabled = false,
        Some(method @ ("summary" | "snap")) => {
            cfg.compaction_enabled = true;
            cfg.compaction = method.into();
        }
        _ => {}
    }
}

impl Workspace {
    pub(super) fn advanced_load(&mut self) {
        match settings::rows() {
            Ok(rows) => self.advanced.rows = rows,
            Err(e) => self.advanced.outcome = Some((false, format!("設定を読み込めません: {e:#}"))),
        }
        if self.advanced.choices.is_empty() {
            for s in &self.advanced.catalog {
                let Some(Native::Key(key)) = s.native else { continue };
                let valid: Vec<_> = s.values.iter().copied().filter(|v| settings::set_text("", key, &[v.to_string()]).is_ok()).collect();
                if !valid.is_empty() {
                    self.advanced.choices.insert(key, valid);
                }
            }
        }
    }

    pub(super) fn advanced_pick(&mut self, key: String, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.advanced.rows.iter().find(|r| r.key == key).map(|r| edit_text(&r.value)).unwrap_or_default();
        self.advanced.value.update(cx, |s, cx| {
            s.set_value(text, window, cx);
            s.focus(window, cx);
        });
        self.advanced.key = Some(key);
        self.advanced.outcome = None;
    }

    /// Writes (or with `reset`, removes) the picked key in the global config, then reloads it.
    pub(super) fn advanced_save(&mut self, reset: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(key) = self.advanced.key.clone() else { return };
        let value = (!reset).then(|| self.advanced.value.read(cx).value().to_string());
        if self.advanced_write(&key, value) {
            // Show the value now in effect (the default after a reset).
            let outcome = self.advanced.outcome.take();
            self.advanced_pick(key, window, cx);
            self.advanced.outcome = outcome;
        }
    }

    /// Sets `key` in the global config (`None` removes it), reloads the config and reports the outcome.
    pub(super) fn advanced_write(&mut self, key: &str, value: Option<String>) -> bool {
        let values = value.map(|v| [v]);
        let result = settings::edit_global(key, values.as_ref().map(|v| &v[..]));
        let ok = result.is_ok();
        self.advanced.outcome = Some(match result {
            Ok(_) => {
                if let Some((_, field)) = UI_OVERRIDES.iter().find(|(k, _)| *k == key) {
                    if self.ui.as_object_mut().and_then(|o| o.remove(*field)).is_some() {
                        self.save_ui();
                    }
                }
                self.reload_config();
                self.advanced_load();
                (true, if values.is_none() { format!("{key} を既定に戻しました") } else { format!("{key} を保存しました") })
            }
            Err(e) => (false, format!("{e:#}")),
        });
        ok
    }

    /// Turns a native tool on or off: `ext` tools as the ツール tab does, the rest through the `tools` allowlist.
    pub(super) fn advanced_tool(&mut self, name: &'static str, on: bool, window: &mut Window, cx: &mut Context<Self>) {
        if greencode_core::tools::ext::NAMES.contains(&name) {
            if self.cfg.ext_off.iter().any(|o| o == name) == on {
                self.run(Act::ToggleExt(name), window, cx);
            }
            return;
        }
        let tools = greencode_core::tools::toggle_tool(&self.cfg.tools, name, on);
        self.advanced_write("tools", (!tools.is_empty()).then(|| toml_value(&json!(tools))));
    }

    fn tool_is_on(&self, name: &str) -> bool {
        if greencode_core::tools::ext::NAMES.contains(&name) {
            !self.cfg.ext_off.iter().any(|o| o == name)
        } else {
            greencode_core::tools::tool_on(&self.cfg.tools, name)
        }
    }

    pub(super) fn runtime_section(&self, mut col: SettingsPage, pal: Pal, window: &mut Window, cx: &mut Context<Self>) -> SettingsPage {
        let adv = &self.advanced;
        let button = |id, label, act: Act, cx: &mut Context<Self>| {
            control_button(id, label, pal)
                .border_1()
                .border_color(pal.line)
                .bg(pal.panel)
                .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| this.run(act.clone(), window, cx)))
        };
        let category = adv.runtime_category;
        col = col.child(settings_row(
            "分類".into(),
            "ターミナルの表示、会話履歴、シェルなどの動作を設定します。",
            settings_select(
                "runtime-category",
                "分類",
                RUNTIME_CATEGORIES[category],
                RUNTIME_CATEGORIES.iter().enumerate().map(|(i, &label)| (label, vec![Act::RuntimeCategory(i)])).collect(),
                pal,
                window,
                cx,
            ),
            pal,
        ));
        col = col.children(adv.outcome.as_ref().map(|(ok, text)| {
            div().text_xs().text_color(if *ok { pal.muted } else { gpui_kit::red() }).child(text.clone())
        }));
        let tab = settings::UI_TABS[category];
        let mut group = "";
        for (i, s) in adv.catalog.iter().enumerate().filter(|(_, s)| s.tab == tab) {
            let Some(native) = s.native else { continue };
            if s.group != group {
                group = s.group;
                col = col.section(group);
            }
            let row = adv.rows.iter().find(|r| matches!(native, Native::Key(k) if r.key == k));
            let mut controls = div().flex().flex_wrap().gap_2().items_center();
            let mut editing = false;
            let toggle = |on: bool, act: Act, cx: &mut Context<Self>| {
                let click = cx.listener(move |this, _: &ClickEvent, window, cx| this.run(act.clone(), window, cx));
                switch(("runtime-switch", i), s.label, on, pal).on_change(move |_, event, window, cx| click(event, window, cx))
            };
            match native {
                Native::Tool(name) => {
                    let on = self.tool_is_on(name);
                    controls = controls.child(toggle(on, Act::RuntimeTool(name, !on), cx));
                }
                Native::Key(key) => {
                    let value = row.map_or(&Value::Null, |r| &r.value);
                    if let Value::Bool(on) = value {
                        controls = controls.child(toggle(*on, Act::SettingWrite(key.into(), Some((!on).to_string())), cx));
                    } else if let Some(choices) = adv.choices.get(key) {
                        controls = controls.child(settings_select(
                            ("runtime-select", i),
                            s.label,
                            &edit_text(value),
                            choices.iter().map(|&v| (v, vec![Act::SettingWrite(key.into(), Some(v.into()))])).collect(),
                            pal,
                            window,
                            cx,
                        ));
                    } else {
                        editing = adv.key.as_deref() == Some(key);
                        controls = controls
                            .child(div().max_w(px(180.)).truncate().text_xs().text_color(pal.muted).child(edit_text(value)))
                            .child(button(("runtime-edit", i), "編集…", Act::SettingPick(key.into()), cx));
                    }
                    if row.is_some_and(|r| r.set) && !editing {
                        controls = controls.child(button(("runtime-reset", i), "既定に戻す", Act::SettingWrite(key.into(), None), cx));
                    }
                }
            }
            col = col.child(
                div().flex().flex_col().gap_2()
                    .child(settings_row(s.label.into(), s.description, controls, pal))
                    .when(editing, |d| {
                        d.child(
                            div()
                                .flex()
                                .gap_2()
                                .items_center()
                                .child(div().flex_1().min_w_0().child(Input::new(&adv.value)))
                                .child(button(("runtime-save", i), "保存", Act::SettingSave, cx))
                                .child(button(("runtime-reset-edit", i), "既定に戻す", Act::SettingReset, cx)),
                        )
                    }),
            );
        }
        col
    }

    pub(super) fn advanced_section(&self, col: SettingsPage, pal: Pal, cx: &mut Context<Self>) -> SettingsPage {
        let small = |id: &'static str, label: &'static str, act: Act, cx: &mut Context<Self>| {
            chip(id, label, pal)
                .border_1()
                .border_color(pal.line)
                .bg(pal.panel)
                .text_color(pal.fg)
                .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| this.run(act.clone(), window, cx)))
        };
        let query = self.advanced.filter.read(cx).value().trim().to_lowercase();
        let matching: Vec<_> = self
            .advanced
            .rows
            .iter()
            .filter(|r| r.writable && (query.is_empty() || r.key.to_lowercase().contains(&query)))
            .collect();
        let mut col = col
            .section("config.toml の全設定")
            .child(Input::new(&self.advanced.filter));
        if let Some(key) = &self.advanced.key {
            col = col.child(
                div()
                    .flex()
                    .gap_2()
                    .items_center()
                    .child(div().flex_shrink_0().font_weight(FontWeight::MEDIUM).child(key.clone()))
                    .child(div().flex_1().child(Input::new(&self.advanced.value)))
                    .child(small("setting-save", "保存", Act::SettingSave, cx))
                    .child(small("setting-reset", "既定に戻す", Act::SettingReset, cx)),
            );
        }
        col = col.children(self.advanced.outcome.as_ref().map(|(ok, text)| {
            div().text_xs().text_color(if *ok { pal.muted } else { gpui_kit::red() }).child(text.clone())
        }));
        for (i, row) in matching.iter().take(SHOWN).enumerate() {
            let key = row.key.clone();
            let selected = self.advanced.key.as_deref() == Some(row.key.as_str());
            col = col.child(
                div()
                    .id(("setting-row", i))
                    .flex()
                    .gap_3()
                    .px_2()
                    .py_0p5()
                    .rounded_md()
                    .cursor_pointer()
                    .when(selected, |d| d.bg(pal.selected))
                    .hover(|s| s.bg(pal.hover))
                    .child(div().flex_shrink_0().child(row.key.clone()))
                    .child(div().flex_1().truncate().text_color(pal.muted).child(edit_text(&row.value)))
                    .child(div().flex_shrink_0().text_xs().text_color(pal.muted).child(if row.set { "変更済み" } else { "既定" }))
                    .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                        this.run(Act::SettingPick(key.clone()), window, cx)
                    })),
            );
        }
        if matching.len() > SHOWN {
            col = col.child(div().text_xs().text_color(pal.muted).child(format!("ほか {} 件。検索で絞り込んでください", matching.len() - SHOWN)));
        }
        col
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn edit_text_round_trips_through_toml() {
        let v = serde_json::json!({"a b": [1, {"x": "y"}], "on": true});
        assert_eq!(super::edit_text(&v), r#"{ "a b" = [1, { "x" = "y" }], "on" = true }"#);
        assert_eq!(super::edit_text(&serde_json::json!("plain")), "plain");
        assert_eq!(super::edit_text(&serde_json::Value::Null), "");
    }
}
