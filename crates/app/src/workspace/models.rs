use super::*;

pub(super) struct ModelPicker {
    field: Entity<InputState>,
    _sub: Subscription,
    scroll: ScrollHandle,
    selected: usize,
    /// Provider rail selection (`None` = every provider).
    provider: Option<String>,
    /// Show every row instead of the first four per section.
    show_all: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ModelRow {
    backend: String,
    id: String,
    name: String,
}

pub(super) fn is_reasoning_option(option: &Value) -> bool {
    matches!(
        option["category"].as_str(),
        Some(
            "reasoning"
                | "effort"
                | "thought"
                | "thinking"
                | "thought_level"
                | "thinking_level"
                | "reasoning_effort"
        )
    ) || matches!(
        option["id"].as_str(),
        Some("reasoning_effort" | "effort" | "thinking" | "thinking_level" | "reasoning")
    )
}

/// The harness's logo as T3 Code shows it: brand colors where the mark has one, else `color`.
pub(super) fn harness_icon(backend: &str, size: f32, color: Hsla) -> Option<AnyElement> {
    let (path, color) = match backend {
        "native" => return Some(img("harness/native.svg").size(px(size)).flex_shrink_0().into_any_element()),
        "antigravity" => return Some(img("harness/antigravity.png").size(px(size)).flex_shrink_0().into_any_element()),
        "claude-code" | "claude" => ("harness/claude.svg", rgb(0xd97757).into()),
        "codex" => ("harness/codex.svg", color),
        "opencode" => ("harness/opencode.svg", color),
        "pi" => ("harness/pi.svg", color),
        "omp" => ("harness/omp.svg", color),
        "kiro" => ("harness/kiro.svg", color),
        "grok" => ("harness/grok.svg", color),
        "cursor" => ("harness/cursor.svg", color),
        "devin" => ("harness/devin.svg", color),
        b if greencode_core::hermes::is_backend(b) => ("harness/hermes.svg", color),
        _ => return None,
    };
    Some(svg().path(path).size(px(size)).flex_shrink_0().text_color(color).into_any_element())
}

fn effort_label(value: &str) -> String {
    match value {
        "low" => "Low".into(),
        "medium" => "Medium".into(),
        "high" => "High".into(),
        "xhigh" => "Extra High".into(),
        "max" => "Max".into(),
        "ultra" => "Ultra".into(),
        "minimal" => "Minimal".into(),
        "off" => "Off".into(),
        "default" => "既定".into(),
        _ => {
            let mut chars = value.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().chain(chars).collect(),
                None => String::new(),
            }
        }
    }
}

fn options(option: &Value) -> Vec<&Value> {
    fn collect<'a>(items: &'a Value, out: &mut Vec<&'a Value>) {
        for item in items.as_array().into_iter().flatten() {
            if item["value"].as_str().is_some() {
                out.push(item);
            }
            collect(&item["options"], out);
        }
    }
    let mut out = Vec::new();
    collect(&option["options"], &mut out);
    out
}

impl Workspace {
    fn offered_models(&self) -> Vec<ModelRow> {
        let t = self.thread();
        let cached = self.models_for_project(&t.project());
        let mut result = Vec::new();
        let mut add = |backend: &str, ids: Vec<String>, option: Option<&Value>| {
            if backend != "native"
                && self
                    .cfg
                    .policy
                    .as_ref()
                    .is_some_and(|p| !p.backend_allowed(backend))
            {
                return;
            }
            let opts = option.map(options).unwrap_or_default();
            for id in ids {
                if result
                    .iter()
                    .any(|r: &ModelRow| r.backend == backend && r.id == id)
                {
                    continue;
                }
                let name = opts
                    .iter()
                    .find(|o| o["value"].as_str() == Some(&id))
                    .and_then(|o| o["name"].as_str())
                    .unwrap_or_else(|| id.split_once('/').map_or(id.as_str(), |(_, model)| model))
                    .to_string();
                result.push(ModelRow {
                    backend: backend.into(),
                    id,
                    name,
                });
            }
        };
        let model_option = t.config_options.iter().find(|o| o["category"] == "model");
        if t.meta.backend == "native" {
            add("native", self.models(), None);
        } else {
            let mut ids = t.models.clone();
            if let Some(option) = model_option {
                for id in options(option).iter().filter_map(|o| o["value"].as_str()) {
                    if !ids.iter().any(|v| v == id) {
                        ids.push(id.into());
                    }
                }
            }
            if ids.is_empty() {
                ids = cached.get(&t.meta.backend).cloned().unwrap_or_default();
            }
            if let Some(over) = self.cfg.backend_overrides.get(&t.meta.backend) {
                for model in &over.models {
                    if !ids.contains(model) {
                        ids.push(model.clone());
                    }
                }
            }
            add(&t.meta.backend, ids, model_option.or_else(|| self.cached_model_option(&t.meta.backend)));
        }
        if t.meta.backend != "native" {
            add("native", self.models(), None);
        }
        for (backend, ids) in &cached {
            if backend != &t.meta.backend && backend != "native" {
                add(backend, ids.clone(), self.cached_model_option(backend));
            }
        }
        result
    }

    /// The model option an ACP backend reported last, for its model names.
    pub(super) fn cached_model_option(&self, backend: &str) -> Option<&Value> {
        self.backend_options.get(backend)?.iter().find(|o| o["category"] == "model")
    }

    /// Rail entries: one per backend, in list order (T3's provider rail).
    fn model_providers(&self, cx: &App) -> Vec<(String, String)> {
        let mut out: Vec<(String, String)> = Vec::new();
        for row in self.offered_models() {
            if !out.iter().any(|(k, _)| *k == row.backend) {
                out.push((row.backend.clone(), row.backend.clone()));
            }
        }
        let _ = cx;
        out
    }

    fn model_sections(&self, cx: &App) -> Vec<(String, Vec<ModelRow>)> {
        self.model_sections_in(cx, false)
    }

    /// True when "Show more" would reveal rows (sections are cut to four until then).
    fn models_hidden(&self, cx: &App) -> bool {
        let count = |sections: Vec<(String, Vec<ModelRow>)>| {
            sections.iter().map(|(_, rows)| rows.len()).sum::<usize>()
        };
        count(self.model_sections_in(cx, true)) > count(self.model_sections_in(cx, false))
    }

    fn model_sections_in(&self, cx: &App, expand: bool) -> Vec<(String, Vec<ModelRow>)> {
        let all = self.offered_models();
        let picker = self.model_picker.as_ref();
        let query = picker
            .map(|p| p.field.read(cx).value().to_lowercase())
            .unwrap_or_default();
        let terms: Vec<_> = query.split_whitespace().collect();
        let rail = picker.and_then(|p| p.provider.clone());
        // A chosen backend or a search shows every match; only the overview is cut to four rows per section.
        let show_all =
            expand || picker.is_some_and(|p| p.show_all) || rail.is_some() || !terms.is_empty();
        let mut sections = Vec::new();
        if terms.is_empty() && rail.is_none() {
            let mut recent = Vec::new();
            for item in self.ui["recent_models"].as_array().into_iter().flatten() {
                if let Some(row) = all.iter().find(|r| {
                    item["backend"].as_str() == Some(&r.backend)
                        && item["model"].as_str() == Some(&r.id)
                }) {
                    if !recent.contains(row) {
                        recent.push(row.clone());
                    }
                    if recent.len() == 5 {
                        break;
                    }
                }
            }
            if !recent.is_empty() {
                sections.push(("最近使用".into(), recent));
            }
        }
        for row in all {
            let text = format!("{} {} {}", row.backend, row.id, row.name).to_lowercase();
            if !terms.iter().all(|term| text.contains(term)) {
                continue;
            }
            let header = match row.id.split_once('/') {
                Some((provider, _)) => format!("{} · {provider}", row.backend),
                None => row.backend.clone(),
            };
            if rail.as_ref().is_some_and(|r| *r != row.backend) {
                continue;
            }
            if let Some((_, rows)) = sections.iter_mut().find(|(h, _)| *h == header) {
                rows.push(row);
            } else {
                sections.push((header, vec![row]));
            }
        }
        if !show_all {
            // Keep the thread's own model visible even when its section is long.
            let (backend, model) = (
                self.thread().meta.backend.clone(),
                self.thread().meta.model.clone(),
            );
            for (_, rows) in &mut sections {
                if let Some(pos) = rows
                    .iter()
                    .position(|r| r.backend == backend && r.id == model)
                    .filter(|p| *p >= 4)
                {
                    let row = rows.remove(pos);
                    rows.insert(3, row);
                }
                rows.truncate(4);
            }
        }
        sections
    }

    pub(super) fn open_model_picker(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.open_model_picker_for(None, window, cx);
    }

    pub(super) fn open_model_picker_for(
        &mut self,
        provider: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.model_picker.is_some() {
            return;
        }
        self.refresh_backend_models(false, cx);
        let field = cx.new(|cx| InputState::new(window, cx).placeholder("モデルを検索"));
        let sub = cx.subscribe_in(
            &field,
            window,
            |this, _, ev: &InputEvent, window, cx| match ev {
                InputEvent::PressEnter { .. } => this.select_model_picker(window, cx),
                InputEvent::Change => {
                    if let Some(p) = &mut this.model_picker {
                        p.selected = 0;
                        p.scroll.set_offset(point(px(0.), px(0.)));
                    }
                    cx.notify();
                }
                _ => {}
            },
        );
        field.update(cx, |s, cx| s.focus(window, cx));
        self.model_picker = Some(ModelPicker {
            field,
            _sub: sub,
            scroll: ScrollHandle::default(),
            selected: 0,
            provider,
            show_all: false,
        });
        let sections = self.model_sections(cx);
        let t = self.thread();
        let selected = sections
            .iter()
            .flat_map(|(_, rows)| rows)
            .position(|r| r.backend == t.meta.backend && r.id == t.meta.model)
            .unwrap_or(0);
        if let Some(p) = &mut self.model_picker {
            p.selected = selected;
        }
        self.scroll_model_picker(cx);
        cx.notify();
    }

    pub(super) fn close_model_picker(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.model_picker = None;
        self.input.update(cx, |s, cx| s.focus(window, cx));
        cx.notify();
    }

    fn scroll_model_picker(&self, cx: &App) {
        let Some(p) = &self.model_picker else { return };
        let mut index = 0;
        let mut count = 0;
        for (_, rows) in self.model_sections(cx) {
            index += 1;
            if p.selected < count + rows.len() {
                p.scroll.scroll_to_item(index + p.selected - count);
                return;
            }
            count += rows.len();
            index += rows.len() + 1;
        }
    }

    pub(super) fn move_model_picker(&mut self, down: bool, cx: &mut Context<Self>) {
        let n: usize = self
            .model_sections(cx)
            .iter()
            .map(|(_, rows)| rows.len())
            .sum();
        if n > 0 {
            if let Some(p) = &mut self.model_picker {
                p.selected = (p.selected.min(n - 1) + if down { 1 } else { n - 1 }) % n;
            }
            self.scroll_model_picker(cx);
        }
        cx.notify();
    }

    fn select_model_picker(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(p) = &self.model_picker else { return };
        if let Some(row) = self
            .model_sections(cx)
            .into_iter()
            .flat_map(|(_, rows)| rows)
            .nth(p.selected)
        {
            self.choose_model(row, window, cx);
        }
    }

    /// ⌘1–⌘9 choose the row with that badge while the picker is open.
    pub(super) fn pick_model_shortcut(
        &mut self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(row) = self
            .model_sections(cx)
            .into_iter()
            .flat_map(|(_, rows)| rows)
            .nth(index)
        {
            self.choose_model(row, window, cx);
        }
    }

    fn choose_model(&mut self, row: ModelRow, window: &mut Window, cx: &mut Context<Self>) {
        let mut recent = self.ui["recent_models"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        recent.insert(0, json!({"backend": row.backend, "model": row.id}));
        let mut seen = std::collections::HashSet::new();
        recent.retain(|v| match (v["backend"].as_str(), v["model"].as_str()) {
            (Some(backend), Some(model)) => seen.insert((backend.to_string(), model.to_string())),
            _ => false,
        });
        recent.truncate(10);
        self.ui["recent_models"] = json!(recent);
        self.save_ui();
        // Keep the picker open so another model or effort can be chosen immediately.
        let act = if row.backend == self.thread().meta.backend {
            Act::Model(row.id)
        } else {
            Act::BackendModel(row.backend, row.id)
        };
        self.run(act, window, cx);
        if let Some(picker) = &self.model_picker {
            picker.field.update(cx, |field, cx| field.focus(window, cx));
        }
    }

    pub(super) fn model_label(&self) -> String {
        let t = self.thread();
        if t.meta.backend == "native" {
            let model = t.meta.model.rsplit('/').next().unwrap_or(&t.meta.model);
            return greencode_core::provider::model_effort(&self.cfg, &t.meta.model)
                .map(|effort| format!("{model} · {}", effort_label(&effort)))
                .unwrap_or_else(|| model.to_string());
        }
        t.config_options
            .iter()
            .find(|o| o["category"] == "model")
            .and_then(|o| {
                options(o)
                    .into_iter()
                    .find(|v| v["value"].as_str() == Some(&t.meta.model))
            })
            .and_then(|v| v["name"].as_str())
            .unwrap_or(if t.meta.model.is_empty() {
                "default"
            } else {
                &t.meta.model
            })
            .to_string()
    }

    pub(super) fn mode_chips(&self, pal: Pal, cx: &mut Context<Self>) -> Vec<AnyElement> {
        self.thread().config_options.iter().filter(|o| o["category"] == "mode").enumerate().filter_map(|(i, o)| {
            let items = options(o);
            let current = items.iter().position(|v| v["value"] == o["currentValue"])?;
            let label = items[current]["name"].as_str().or_else(|| items[current]["value"].as_str())?.to_string();
            let value = items[(current + 1) % items.len()]["value"].clone();
            let id = o["id"].clone();
            Some(chip(("acp-mode", i), label, pal).on_click(cx.listener(move |this, _: &ClickEvent, window, cx| this.run(Act::Rpc(json!({"type":"set_config_option", "configId":id, "value":value})), window, cx))).into_any_element())
        }).collect()
    }

    /// True while an external backend's model list has not arrived yet.
    fn models_pending(&self) -> bool {
        let t = self.thread();
        t.meta.backend != "native"
            && t.models.is_empty()
            && t.config_options.iter().all(|o| o["category"] != "model")
    }

    fn picker_option(&self, predicate: fn(&Value) -> bool) -> Option<&Value> {
        let t = self.thread();
        t.config_options.iter().find(|o| predicate(o)).or_else(|| {
            self.backend_options
                .get(&t.meta.backend)
                .and_then(|options| options.iter().find(|o| predicate(o)))
        })
    }

    fn option_current<'a>(&'a self, option: &'a Value) -> &'a Value {
        let id = option["id"].as_str();
        self.thread()
            .pending_options
            .iter()
            .find(|(key, _)| Some(key.as_str()) == id)
            .map(|(_, value)| value)
            .unwrap_or(&option["currentValue"])
    }

    /// Reasoning levels of the current thread: native effort, or the backend's own option.
    fn reasoning_rows(&self) -> (String, Vec<(String, String, bool)>) {
        let t = self.thread();
        if rpc_backend(&self.cfg, &t.meta.backend) {
            let current = t.pending_thinking.as_deref().unwrap_or(&t.thinking);
            let rows = if t.thinking_levels.is_empty() {
                self.picker_option(|o| o["id"] == "thinking_level")
                    .map(|option| {
                        options(option)
                            .into_iter()
                            .filter_map(|o| {
                                let level = o["value"].as_str()?;
                                Some((level.to_string(), effort_label(level), level == current))
                            })
                            .collect()
                    })
                    .unwrap_or_default()
            } else {
                t.thinking_levels
                    .iter()
                    .map(|level| (level.clone(), effort_label(level), level == current))
                    .collect()
            };
            return ("思考の強さ".to_string(), rows);
        }
        if let Some(option) = self.picker_option(is_reasoning_option) {
            let current = self.option_current(option).as_str().unwrap_or("");
            let rows = options(option)
                .into_iter()
                .filter_map(|o| {
                    let value = o["value"].as_str()?.to_string();
                    let name = o["name"]
                        .as_str()
                        .filter(|name| !name.eq_ignore_ascii_case(&value))
                        .map(str::to_string)
                        .unwrap_or_else(|| effort_label(&value));
                    Some((value.clone(), name, value == current))
                })
                .collect();
            return (
                option["name"].as_str().unwrap_or("思考の強さ").to_string(),
                rows,
            );
        }
        if t.meta.backend == "opencode" {
            let mut rows = vec![(
                "default".to_string(),
                "Default".to_string(),
                t.opencode_effort.is_none(),
            )];
            rows.extend(
                ["low", "medium", "high", "xhigh", "max"]
                    .into_iter()
                    .map(|level| {
                        (
                            level.to_string(),
                            effort_label(level),
                            t.opencode_effort.as_deref() == Some(level),
                        )
                    }),
            );
            return ("思考の強さ".to_string(), rows);
        }
        // Effort only reaches native providers; an external agent owns its own reasoning settings.
        if t.meta.backend != "native" {
            return ("思考の強さ".to_string(), Vec::new());
        }
        let levels = greencode_core::provider::effort_levels(&self.cfg, &t.meta.model);
        if levels.is_empty() {
            return ("思考の強さ".to_string(), Vec::new());
        }
        let current = greencode_core::provider::model_effort(&self.cfg, &t.meta.model);
        let mut rows = vec![("default".to_string(), "既定".to_string(), current.is_none())];
        rows.extend(levels.into_iter().map(|level| {
            (
                level.to_string(),
                effort_label(level),
                current.as_deref() == Some(level),
            )
        }));
        ("思考の強さ".to_string(), rows)
    }

    fn fast_option(&self) -> Option<(Value, Option<bool>)> {
        self.picker_option(|o| {
            o["category"] == "model_config"
                && matches!(o["id"].as_str(), Some("fast" | "fast-mode"))
        })
        .map(|o| {
            (
                o["id"].clone(),
                self.option_current(o).as_str().and_then(|v| match v {
                    "on" => Some(true),
                    "off" => Some(false),
                    _ => None,
                }),
            )
        })
    }

    pub(super) fn model_picker_view(
        &self,
        pal: Pal,
        cx: &mut Context<Self>,
    ) -> Option<impl IntoElement> {
        let picker = self.model_picker.as_ref()?;
        // Left rail: 最近使用 (star) then one entry per backend/provider.
        let mut rail = div()
            .id("model-rail")
            .w(px(64.))
            .flex()
            .flex_col()
            .items_center()
            .gap_1()
            .py_3()
            .border_r_1()
            .border_color(pal.line);
        let star_selected = picker.provider.is_none();
        rail = rail.child(
            div()
                .id("model-rail-star")
                .size(px(36.))
                .flex()
                .items_center()
                .justify_center()
                .rounded_md()
                .cursor_pointer()
                .when(star_selected, |d| d.bg(pal.selected))
                .hover(|d| d.bg(pal.hover))
                .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                    if let Some(p) = &mut this.model_picker {
                        p.provider = None;
                        p.selected = 0;
                    }
                    cx.notify();
                }))
                .child(
                    div()
                        .text_color(if star_selected { pal.fg } else { pal.muted })
                        .child("★"),
                ),
        );
        for (key, label) in self.model_providers(cx) {
            let selected = picker.provider.as_deref() == Some(key.as_str());
            let initials: String = label.chars().take(2).collect::<String>().to_uppercase();
            let color = if selected { pal.fg } else { pal.muted };
            let icon = harness_icon(&key, 20., color).unwrap_or_else(|| {
                div().text_size(px(11.)).text_color(color).child(SharedString::from(initials)).into_any_element()
            });
            let pick = key.clone();
            rail = rail.child(
                div()
                    .id(SharedString::from(format!("model-rail-{key}")))
                    .size(px(36.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded_md()
                    .cursor_pointer()
                    .when(selected, |d| d.bg(pal.selected))
                    .hover(|d| d.bg(pal.hover))
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                        if let Some(p) = &mut this.model_picker {
                            p.provider = Some(pick.clone());
                            p.selected = 0;
                        }
                        cx.notify();
                    }))
                    .child(icon),
            );
        }

        // Middle: search field with an underline, then the rows.
        let mut list = div()
            .id("model-picker-rows")
            .flex()
            .flex_col()
            .flex_1()
            .overflow_y_scroll()
            .track_scroll(&picker.scroll);
        let mut ix = 0;
        let sections = self.model_sections(cx);
        let total: usize = sections.iter().map(|(_, rows)| rows.len()).sum();
        for (header, rows) in sections {
            list = list.child(
                div()
                    .px_4()
                    .pt_3()
                    .pb_1()
                    .text_xs()
                    .text_color(pal.muted)
                    .child(SharedString::from(header)),
            );
            for row in rows {
                let current =
                    row.backend == self.thread().meta.backend && row.id == self.thread().meta.model;
                let selected = ix == picker.selected;
                let display = row.name.clone();
                let id = row.id.clone();
                let shortcut = (ix < 9).then(|| format!("⌘{}", ix + 1));
                let icon = harness_icon(&row.backend, 16., pal.fg);
                list = list.child(
                    div()
                        .id(("model-row", ix))
                        .mx_2()
                        .px_3()
                        .py_2()
                        .rounded_md()
                        .flex()
                        .items_center()
                        .gap_3()
                        .cursor_pointer()
                        .when(selected, |d| d.bg(pal.selected))
                        .hover(|d| d.bg(pal.selected))
                        .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                            this.choose_model(row.clone(), window, cx)
                        }))
                        .children(icon)
                        .child(
                            div()
                                .flex_1()
                                .truncate()
                                .child(SharedString::from(display.clone())),
                        )
                        .when(id != display, |d| {
                            d.child(
                                div()
                                    .text_xs()
                                    .text_color(pal.muted)
                                    .child(SharedString::from(id)),
                            )
                        })
                        .when(current, |d| {
                            d.child(div().text_color(pal.accent).child("✓"))
                        })
                        .when_some(shortcut, |d, key| {
                            d.child(
                                div()
                                    .px_1p5()
                                    .rounded_md()
                                    .bg(pal.hover)
                                    .text_xs()
                                    .text_color(pal.muted)
                                    .child(SharedString::from(key)),
                            )
                        }),
                );
                ix += 1;
            }
        }
        if total == 0 && self.models_pending() {
            list = list.child(
                div()
                    .px_4()
                    .py_2()
                    .text_xs()
                    .text_color(pal.muted)
                    .child("モデル一覧を取得中…"),
            );
        }
        if !picker.show_all && self.models_hidden(cx) {
            list = list.child(
                div()
                    .id("model-show-more")
                    .mx_2()
                    .px_3()
                    .py_2()
                    .rounded_md()
                    .cursor_pointer()
                    .text_color(pal.muted)
                    .hover(|d| d.bg(pal.hover))
                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                        if let Some(p) = &mut this.model_picker {
                            p.show_all = true;
                        }
                        cx.notify();
                    }))
                    .child("Show more"),
            );
        }
        let middle = div()
            .flex_1()
            .min_w_0()
            .flex()
            .flex_col()
            .child(
                div()
                    .px_4()
                    .pt_3()
                    .pb_2()
                    .border_b_1()
                    .border_color(pal.accent)
                    .child(Input::new(&picker.field).appearance(false)),
            )
            .child(list);

        // Right: reasoning levels for the current thread.
        let (_, levels) = self.reasoning_rows();
        let model = self.model_label();
        let short_model = model.rsplit('/').next().unwrap_or(&model).to_string();
        let mut right = div()
            .w(px(220.))
            .flex_shrink_0()
            .flex()
            .flex_col()
            .gap_1()
            .p_4()
            .border_l_1()
            .border_color(pal.line)
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap_2()
                    .pb_2()
                    .border_b_1()
                    .border_color(pal.line)
                    .text_sm()
                    .child("Reasoning")
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .text_color(pal.muted)
                            .child(SharedString::from(short_model)),
                    ),
            );
        if levels.is_empty() {
            let message = if self.thread().meta.backend == "native" {
                "思考の強さはこのモデルでは送信されません".to_string()
            } else {
                format!(
                    "このバックエンドは思考の強さを公開していません（{}）",
                    self.thread().meta.backend
                )
            };
            right = right.child(
                div()
                    .text_xs()
                    .text_color(pal.muted)
                    .child(SharedString::from(message)),
            );
        }
        let rpc = rpc_backend(&self.cfg, &self.thread().meta.backend);
        let acp_option = if rpc {
            None
        } else {
            self.picker_option(is_reasoning_option)
                .map(|o| o["id"].clone())
        };
        let opencode = self.thread().meta.backend == "opencode";
        let note = opencode && !levels.is_empty();
        for (i, (value, name, current)) in levels.into_iter().enumerate() {
            let id = acp_option.clone();
            let v = value.clone();
            right = right.child(
                div()
                    .id(("reasoning", i))
                    .px_2()
                    .py_2()
                    .rounded_md()
                    .flex()
                    .items_center()
                    .gap_2()
                    .cursor_pointer()
                    .hover(|d| d.bg(pal.hover))
                    .when(current, |d| d.bg(pal.selected))
                    .on_click(
                        cx.listener(move |this, _: &ClickEvent, window, cx| match &id {
                            Some(config) => this.run(
                                Act::Rpc(
                                    json!({"type":"set_config_option","configId":config,"value":v}),
                                ),
                                window,
                                cx,
                            ),
                            None if rpc => this.run(
                                Act::Rpc(json!({"type":"set_thinking_level","level":v})),
                                window,
                                cx,
                            ),
                            None if opencode => this.run(
                                Act::OpenCodeEffort((v != "default").then(|| v.clone())),
                                window,
                                cx,
                            ),
                            None => this.run(
                                Act::Effort((v != "default").then(|| v.clone())),
                                window,
                                cx,
                            ),
                        }),
                    )
                    .child(div().flex_1().child(SharedString::from(name)))
                    .when(current, |d| {
                        d.child(div().text_color(pal.accent).child("✓"))
                    }),
            );
        }
        if note {
            right = right.child(div().pt_2().text_xs().text_color(pal.muted).child("opencode はモデル設定 reasoningEffort として送ります。xhigh / max を含め、モデルが対応しない値はプロバイダー側で無視またはエラーになります。"));
        }

        // Bottom bar: optional speed control and permission modes.
        let mode = self.thread().perms.mode();
        let mut bar = div()
            .flex()
            .items_center()
            .gap_2()
            .px_4()
            .py_2()
            .border_t_1()
            .border_color(pal.line);
        if let Some((id, on)) = self.fast_option() {
            let mut speed = div().flex().items_center().rounded_md().bg(pal.hover);
            for (label, value, selected) in [
                ("Standard", "off", on == Some(false)),
                ("Fast", "on", on == Some(true)),
            ] {
                let id = id.clone();
                speed = speed.child(
                    chip(
                        SharedString::from(format!("picker-speed-{value}")),
                        label,
                        pal,
                    )
                    .when(selected, |c| c.bg(pal.selected).text_color(pal.fg))
                    .on_click(cx.listener(
                        move |this, _: &ClickEvent, window, cx| {
                            this.run(
                                Act::Rpc(
                                    json!({"type":"set_config_option","configId":id,"value":value}),
                                ),
                                window,
                                cx,
                            )
                        },
                    )),
                );
            }
            bar = bar.child(speed);
        }
        bar = bar.child(div().flex_1());
        for &m in greencode_core::config::backend_modes(&self.thread().meta.backend) {
            bar = bar.child(
                chip(
                    SharedString::from(format!("picker-mode-{}", mode_label(m))),
                    mode_label(m),
                    pal,
                )
                .when(m == mode, |c| c.bg(pal.selected).text_color(pal.fg))
                .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                    this.run(Act::Mode(m), window, cx)
                })),
            );
        }

        Some(
            div()
                .id("model-picker-backdrop")
                .absolute()
                .inset_0()
                .flex()
                .justify_center()
                .pt(px(80.))
                // A click outside the panel dismisses it, as in T3.
                .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                    this.close_model_picker(window, cx)
                }))
                .child(
                    div()
                        .occlude()
                        .w(px(860.))
                        .h(px(520.))
                        .flex()
                        .flex_col()
                        .rounded_xl()
                        .border_1()
                        .border_color(pal.line)
                        .bg(pal.card)
                        .shadow_lg()
                        .overflow_hidden()
                        .child(
                            div()
                                .flex_1()
                                .min_h_0()
                                .flex()
                                .child(rail)
                                .child(middle)
                                .child(right),
                        )
                        .child(bar),
                ),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::open;
    use gpui_kit::TestAppContext;
    use serde_json::json;

    #[test]
    fn effort_labels_are_human_readable() {
        use super::effort_label;
        for (value, label) in [
            ("low", "Low"),
            ("medium", "Medium"),
            ("high", "High"),
            ("xhigh", "Extra High"),
            ("max", "Max"),
            ("ultra", "Ultra"),
            ("minimal", "Minimal"),
            ("off", "Off"),
            ("default", "既定"),
            ("custom-level", "Custom-level"),
            ("", ""),
        ] {
            assert_eq!(effort_label(value), label);
        }
    }

    #[gpui_kit::test]
    async fn fast_option_follows_acp_config(cx: &mut TestAppContext) {
        let (ws, cx) = open(cx);
        ws.update(cx, |ws, _| {
            ws.threads[0].config_options = vec![json!({"id":"fast-mode","category":"model_config","currentValue":"off","options":[{"value":"off"},{"value":"on"}]})];
            assert_eq!(ws.fast_option(), Some((json!("fast-mode"), Some(false))));
            ws.threads[0].config_options = vec![json!({"id":"fast","category":"model_config","currentValue":"on","options":[{"value":"on"},{"value":"off"}]})];
            assert_eq!(ws.fast_option(), Some((json!("fast"), Some(true))));
            ws.threads[0].config_options = vec![json!({"id":"fast","category":"other","currentValue":"on"})];
            assert_eq!(ws.fast_option(), None);
        });
    }

    #[gpui_kit::test]
    async fn cached_rpc_thinking_is_pending_before_first_prompt(cx: &mut TestAppContext) {
        let (ws, cx) = open(cx);
        cx.update(|window, cx| ws.update(cx, |ws, cx| {
            std::sync::Arc::make_mut(&mut ws.cfg).backends.push(greencode_core::config::AcpCfg {
                name: "pi".into(), command: vec!["pi".into()], protocol: greencode_core::config::Protocol::Pi, env: Default::default()
            });
            ws.threads[0].meta.backend = "pi".into();
            ws.backend_options.insert("pi".into(), vec![json!({"id":"thinking_level","category":"thought_level","options":[{"value":"off"},{"value":"high"}]})]);
            assert_eq!(ws.reasoning_rows().1, vec![("off".into(), "Off".into(), false), ("high".into(), "High".into(), false)]);
            ws.run(super::super::Act::Rpc(json!({"type":"set_thinking_level","level":"high"})), window, cx);
            assert!(ws.reasoning_rows().1[1].2);
            match ws.threads[0].prompt_command("hello".into()) {
                greencode_core::Command::PromptWithOptions(text, controls) => {
                    assert_eq!(text, "hello");
                    assert_eq!(controls, vec![json!({"type":"set_thinking_level","level":"high"})]);
                }
                _ => panic!("expected pending thinking command"),
            }
        }));
    }

    #[gpui_kit::test]
    async fn opencode_effort_reconnects_without_losing_session(cx: &mut TestAppContext) {
        let (ws, cx) = open(cx);
        cx.update(|window, cx| {
            ws.update(cx, |ws, cx| {
                ws.threads[0].meta.backend = "opencode".into();
                ws.threads[0].meta.model = "provider/model".into();
                let id = ws.thread().meta.id.clone();
                greencode_core::session::Session::save_runtime(
                    &id,
                    &json!({"backend":"opencode","sessionId":"saved"}),
                )
                .unwrap();
                assert_eq!(
                    ws.reasoning_rows().1[0],
                    ("default".into(), "Default".into(), true)
                );
                let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
                ws.threads[0].cmd = Some(tx);
                ws.run(
                    super::super::Act::OpenCodeEffort(Some("low".into())),
                    window,
                    cx,
                );
                assert_eq!(ws.thread().opencode_effort.as_deref(), Some("low"));
                assert!(ws.reasoning_rows().1[1].2);
                assert_eq!(
                    ws.reasoning_rows()
                        .1
                        .iter()
                        .map(|r| r.0.as_str())
                        .collect::<Vec<_>>(),
                    ["default", "low", "medium", "high", "xhigh", "max"]
                );
                assert!(ws.thread().cmd.is_none());
                assert!(matches!(
                    rx.try_recv(),
                    Err(tokio::sync::mpsc::error::TryRecvError::Disconnected)
                ));
                assert_eq!(
                    greencode_core::session::Session::runtime(&id).unwrap()["sessionId"],
                    "saved"
                );
                let (tx, _) = tokio::sync::mpsc::unbounded_channel();
                ws.threads[0].cmd = Some(tx);
                ws.run(
                    super::super::Act::Model("provider/other".into()),
                    window,
                    cx,
                );
                assert!(ws.thread().cmd.is_none());
                assert_eq!(ws.thread().meta.model, "provider/other");
            })
        });
    }

    #[gpui_kit::test]
    async fn cached_options_are_selected_and_replayed(cx: &mut TestAppContext) {
        let (ws, cx) = open(cx);
        cx.update(|window, cx| ws.update(cx, |ws, cx| {
            ws.threads[0].meta.backend = "codex".into();
            let options = vec![
                json!({"id":"reasoning_effort","category":"thought_level","currentValue":"low","options":[{"value":"low"},{"value":"xhigh"}]}),
                json!({"id":"fast-mode","category":"model_config","currentValue":"off","options":[{"value":"off"},{"value":"on"}]})
            ];
            ws.backend_options.insert("codex".into(), greencode_core::cacheable_picker_options(&options));
            assert!(!ws.reasoning_rows().1.iter().any(|row| row.2));
            ws.run(super::super::Act::Rpc(json!({"type":"set_config_option","configId":"reasoning_effort","value":"xhigh"})), window, cx);
            ws.run(super::super::Act::Rpc(json!({"type":"set_config_option","configId":"fast-mode","value":"on"})), window, cx);
            assert!(ws.reasoning_rows().1[1].2);
            assert_eq!(ws.fast_option(), Some((json!("fast-mode"), Some(true))));
            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
            ws.threads[0].cmd = Some(tx);
            let id = ws.thread().meta.id.clone();
            ws.on_event(&id, greencode_core::Event::Remote { command: "acp_state".into(), data: json!({"configOptions":options}) }, cx);
            for (id, value) in [("reasoning_effort", "xhigh"), ("fast-mode", "on")] {
                match rx.try_recv().unwrap() {
                    greencode_core::Command::Remote(command) => assert_eq!(command, json!({"type":"set_config_option","configId":id,"value":value})),
                    _ => panic!("expected config option command"),
                }
            }
            assert!(ws.thread().pending_options.is_empty());
            assert!(ws.reasoning_rows().1[0].2);
            let meta = greencode_core::session::Meta::new(ws.thread().project(), "default".into(), "codex".into());
            ws.threads.push(super::super::Thread::new(meta, true, greencode_core::permission::Mode::Ask));
            ws.active = ws.threads.len() - 1;
            assert!(!ws.reasoning_rows().1.iter().any(|row| row.2));
            assert_eq!(ws.fast_option(), Some((json!("fast-mode"), None)));
        }));
    }

    #[gpui_kit::test]
    async fn cached_acp_models_show_their_names(cx: &mut TestAppContext) {
        let (ws, cx) = open(cx);
        cx.update(|_, cx| ws.update(cx, |ws, _| {
            ws.backend_models.insert("claude-code".into(), vec!["default".into(), "opus".into()]);
            let option = json!({"id":"model","category":"model","currentValue":"opus","options":[{"value":"default","name":"Default (recommended)"},{"value":"opus","name":"Opus 5.5"}]});
            ws.backend_options.insert("claude-code".into(), greencode_core::cacheable_picker_options(&[option]));
            let names: Vec<_> = ws.offered_models().into_iter().filter(|r| r.backend == "claude-code").map(|r| r.name).collect();
            assert_eq!(names, ["Default (recommended)", "Opus 5.5"]);
        }));
    }

    #[gpui_kit::test]
    async fn reasoning_levels_follow_backend(cx: &mut TestAppContext) {
        let (ws, cx) = open(cx);
        ws.update(cx, |ws, cx| {
            ws.threads[0].meta.backend = "codex".into();
            let id = ws.thread().meta.id.clone();
            ws.on_event(&id, greencode_core::Event::Remote { command: "acp_state".into(), data: json!({
                "configOptions":[{"id":"reasoning_effort","category":"thought_level","currentValue":"xhigh","options":[
                    {"name":"group","options":[{"value":"low"},{"value":"xhigh"}]}
                ]}]
            }) }, cx);
            assert_eq!(ws.reasoning_rows().1, vec![("low".into(), "Low".into(), false), ("xhigh".into(), "Extra High".into(), true)]);
            ws.threads[0].config_options = vec![json!({"id":"effort","currentValue":"high","options":[{"value":"high","name":"Custom label"}]})];
            assert_eq!(ws.reasoning_rows().1, vec![("high".into(), "Custom label".into(), true)]);
            std::sync::Arc::make_mut(&mut ws.cfg).backends.push(greencode_core::config::AcpCfg {
                name: "pi".into(), command: vec!["pi".into()], protocol: greencode_core::config::Protocol::Pi, env: Default::default()
            });
            let t = &mut ws.threads[0];
            t.meta.backend = "pi".into();
            t.config_options.clear();
            t.thinking_levels = vec!["off".into(), "high".into()];
            t.thinking = "high".into();
            assert_eq!(ws.reasoning_rows(), ("思考の強さ".into(), vec![("off".into(), "Off".into(), false), ("high".into(), "High".into(), true)]));
            ws.threads[0].meta.backend = "unsupported-agent".into();
            assert!(ws.reasoning_rows().1.is_empty());
            ws.threads[0].meta.backend = "native".into();
            ws.threads[0].meta.model = "openai/gpt-6-luna".into();
            std::sync::Arc::make_mut(&mut ws.cfg).providers.insert("openai".into(), greencode_core::config::ProviderCfg {
                kind: "openai".into(), base_url: "https://api.openai.com/v1".into(), ..Default::default()
            });
            std::sync::Arc::make_mut(&mut ws.cfg).effort = None;
            let rows = ws.reasoning_rows().1;
            assert_eq!(rows[0], ("default".into(), "既定".into(), true));
            assert_eq!(rows.last().unwrap(), &("xhigh".into(), "Extra High".into(), false));
        });
    }

    #[gpui_kit::test]
    async fn native_model_label_shows_short_name_and_effort(cx: &mut TestAppContext) {
        let (ws, cx) = open(cx);
        ws.update(cx, |ws, _| {
            ws.threads[0].meta.model = "openai/gpt-6-luna".into();
            std::sync::Arc::make_mut(&mut ws.cfg).providers.insert(
                "openai".into(),
                greencode_core::config::ProviderCfg {
                    kind: "codex".into(),
                    base_url: "https://chatgpt.com/backend-api/codex".into(),
                    ..Default::default()
                },
            );
            std::sync::Arc::make_mut(&mut ws.cfg).effort = Some("low".into());
            assert_eq!(ws.model_label(), "gpt-6-luna · Low");
        });
    }

    #[gpui_kit::test]
    async fn picker_groups_searches_and_records_models(cx: &mut TestAppContext) {
        let (ws, cx) = open(cx);
        ws.update(cx, |ws, _| {
            ws.ui["recent_models"] = json!([]);
            let t = &mut ws.threads[0];
            t.meta.backend = "opencode".into();
            t.meta.model = "b/x".into();
            let mut items: Vec<_> = (0..29)
                .map(|i| {
                    let id = format!("{}/model-{i}", if i < 15 { "a" } else { "b" });
                    json!({"value":id, "name":format!("Display {i}")})
                })
                .collect();
            items.push(json!({"value":"b/x", "name":"X"}));
            t.config_options =
                vec![json!({"category":"model", "currentValue":"b/x", "options":items})];
        });
        cx.simulate_keystrokes("cmd-shift-m");
        ws.read_with(cx, |ws, cx| {
            assert!(ws.palette.is_none());
            let sections = ws.model_sections(cx);
            // Four rows per section until "Show more", as in T3.
            assert_eq!(
                sections
                    .iter()
                    .filter(|(h, _)| h.starts_with("opencode · "))
                    .map(|(_, r)| r.len())
                    .sum::<usize>(),
                8
            );
            let capped = sections
                .iter()
                .filter(|(h, _)| h.starts_with("opencode · "))
                .map(|(_, r)| r.len())
                .sum::<usize>();
            assert_eq!(capped, 8);
            let sections = ws.model_sections(cx);
            assert!(sections.iter().any(|(h, _)| h == "opencode · a"));
            assert!(sections.iter().any(|(h, _)| h == "opencode · b"));
            let selected = ws.model_picker.as_ref().unwrap().selected;
            assert_eq!(
                sections
                    .iter()
                    .flat_map(|(_, r)| r)
                    .nth(selected)
                    .unwrap()
                    .id,
                "b/x"
            );
            assert_eq!(ws.model_label(), "X");
        });
        ws.update(cx, |ws, _| {
            ws.model_picker.as_mut().unwrap().show_all = true
        });
        ws.read_with(cx, |ws, cx| {
            assert_eq!(
                ws.model_sections(cx)
                    .iter()
                    .filter(|(h, _)| h.starts_with("opencode · "))
                    .map(|(_, r)| r.len())
                    .sum::<usize>(),
                30
            )
        });
        ws.update(cx, |ws, _| {
            ws.model_picker.as_mut().unwrap().show_all = false
        });
        cx.simulate_input("b x");
        ws.read_with(cx, |ws, cx| {
            let rows: Vec<_> = ws
                .model_sections(cx)
                .into_iter()
                .flat_map(|(_, rows)| rows)
                .collect();
            assert!(rows.iter().any(|r| r.id == "b/x"));
            assert!(rows.iter().all(|r| {
                let s = format!("{} {} {}", r.backend, r.id, r.name).to_lowercase();
                s.contains('b') && s.contains('x')
            }));
        });
        cx.simulate_keystrokes("enter");
        ws.read_with(cx, |ws, _| {
            assert!(ws.model_picker.is_some(), "selection keeps the picker open");
            assert_eq!(
                ws.ui["recent_models"][0],
                json!({"backend":"opencode", "model":"b/x"})
            );
        });
    }

    #[gpui_kit::test]
    async fn one_backend_shows_every_model_behind_its_rail_entry(cx: &mut TestAppContext) {
        let (ws, cx) = open(cx);
        ws.update(cx, |ws, _| {
            ws.backend_models.insert(
                "codex".into(),
                [
                    "gpt-6.1-sol",
                    "gpt-6-astra",
                    "gpt-6.1-terra",
                    "gpt-6-sol",
                    "gpt-6-luna",
                    "gpt-5.6-sol",
                ]
                .map(String::from)
                .to_vec(),
            );
        });
        cx.simulate_keystrokes("cmd-shift-m");
        let count = |ws: &super::super::Workspace, cx: &gpui::App, id: &str| {
            ws.model_sections(cx)
                .iter()
                .flat_map(|(_, r)| r)
                .filter(|r| r.id == id)
                .count()
        };
        ws.read_with(cx, |ws, cx| {
            // Overview: cut to four rows per section, and "Show more" must say so.
            assert_eq!(count(ws, cx, "gpt-6-luna"), 0);
            assert!(ws.models_hidden(cx));
        });
        ws.update(cx, |ws, _| {
            ws.model_picker.as_mut().unwrap().provider = Some("codex".into())
        });
        ws.read_with(cx, |ws, cx| assert_eq!(count(ws, cx, "gpt-6-luna"), 1));
        ws.update(cx, |ws, _| {
            ws.model_picker.as_mut().unwrap().provider = None
        });
        cx.simulate_input("luna");
        ws.read_with(cx, |ws, cx| assert_eq!(count(ws, cx, "gpt-6-luna"), 1));
    }

    #[gpui_kit::test]
    async fn external_model_label_and_grouped_names(cx: &mut TestAppContext) {
        let (ws, cx) = open(cx);
        cx.update(|window, cx| {
            ws.update(cx, |ws, cx| {
                ws.run(
                    super::super::Act::BackendModel(
                        "opencode".into(),
                        "opencode/iceman-free".into(),
                    ),
                    window,
                    cx,
                );
                assert_eq!(ws.model_label(), "opencode/iceman-free");
                ws.backend_models.insert(
                    "claude-code".into(),
                    vec!["default".into(), "opus".into(), "sonnet".into()],
                );
                ws.project_models.insert(
                    ws.thread().project(),
                    vec!["ollama/paritok-4b-v1:latest".into()],
                );
                ws.open_model_picker(window, cx);
                let sections = ws.model_sections(cx);
                let claude = sections.iter().find(|(h, _)| h == "claude-code").unwrap();
                assert_eq!(
                    claude.1.iter().map(|r| r.name.as_str()).collect::<Vec<_>>(),
                    ["default", "opus", "sonnet"]
                );
                let ollama = sections
                    .iter()
                    .find(|(h, _)| h == "opencode · ollama")
                    .unwrap();
                assert_eq!(ollama.1[0].name, "paritok-4b-v1:latest");
            })
        });
        cx.simulate_input("ollama paritok");
        ws.read_with(cx, |ws, cx| {
            let sections = ws.model_sections(cx);
            assert!(sections
                .iter()
                .any(|(h, rows)| h == "opencode · ollama"
                    && rows[0].id == "ollama/paritok-4b-v1:latest"));
        });
    }

    #[gpui_kit::test]
    async fn picker_shortcut_selects_the_badged_row(cx: &mut TestAppContext) {
        let (ws, cx) = open(cx);
        ws.update(cx, |ws, _| {
            let t = &mut ws.threads[0];
            t.meta.backend = "opencode".into();
            t.meta.model = "a/one".into();
            t.config_options = vec![
                json!({"category":"model", "currentValue":"a/one", "options":[
                json!({"value":"a/one","name":"One"}), json!({"value":"a/two","name":"Two"})]}),
            ];
        });
        cx.simulate_keystrokes("cmd-shift-m");
        cx.simulate_keystrokes("cmd-2");
        ws.read_with(cx, |ws, _| {
            assert!(ws.model_picker.is_some(), "choosing keeps the picker open");
            assert_eq!(ws.thread().meta.model, "a/two");
        });
    }

    #[gpui_kit::test]
    async fn opencode_models_follow_project_and_nested_names(cx: &mut TestAppContext) {
        let (ws, cx) = open(cx);
        ws.update(cx, |ws, cx| {
            let a = std::path::PathBuf::from("/model-project-a");
            let b = std::path::PathBuf::from("/model-project-b");
            ws.threads[0].meta.cwd = a.clone();
            ws.threads[0].meta.repo = None;
            ws.threads[0].meta.backend = "opencode".into();
            let id = ws.thread().meta.id.clone();
            ws.on_event(&id, greencode_core::Event::Remote { command: "acp_state".into(), data: json!({
                "configOptions":[{"category":"model","options":[{"name":"group","options":[
                    {"options":[{"value":"a/model","name":"Project A"}]}
                ]}]}]
            }) }, cx);
            assert!(ws.offered_models().iter().any(|r| r.id == "a/model" && r.name == "Project A"));
            ws.threads[0].meta.cwd = b;
            ws.threads[0].models.clear();
            ws.threads[0].config_options.clear();
            assert!(!ws.offered_models().iter().any(|r| r.id == "a/model"));
            ws.on_event(&id, greencode_core::Event::Remote { command: "acp_state".into(), data: json!({
                "configOptions":[{"category":"model","options":[{"value":"b/model","name":"Project B"}]}]
            }) }, cx);
            assert!(ws.offered_models().iter().any(|r| r.id == "b/model"));
            ws.threads[0].meta.cwd = a;
            ws.threads[0].models.clear();
            ws.threads[0].config_options.clear();
            assert!(ws.offered_models().iter().any(|r| r.id == "a/model"));
            assert!(!ws.offered_models().iter().any(|r| r.id == "b/model"));
        });
    }

    #[gpui_kit::test]
    async fn hermes_harness_icon_matches_default_and_profile_backends(_cx: &mut TestAppContext) {
        assert!(super::harness_icon("hermes", 16., gpui_kit::rgb(0xffffff).into()).is_some());
        assert!(super::harness_icon("hermes:default", 16., gpui_kit::rgb(0xffffff).into()).is_some());
        assert!(super::harness_icon("hermes:omh-tui", 16., gpui_kit::rgb(0xffffff).into()).is_some());
    }
}
