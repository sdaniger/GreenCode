//! Appearance mode, per-appearance themes, custom themes (`~/.greencode/themes`), and the background image.

use super::*;
use greencode_core::theme::{self, Theme as CustomTheme};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::SystemTime;

/// System follows the OS appearance; Light/Dark force the matching theme slot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ThemeMode {
    System,
    Light,
    Dark,
}

impl ThemeMode {
    pub(super) const ALL: [ThemeMode; 3] = [Self::System, Self::Light, Self::Dark];

    pub(super) fn id(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::Light => "light",
            Self::Dark => "dark",
        }
    }

    pub(super) fn label(self) -> &'static str {
        match self {
            Self::System => "システム",
            Self::Light => "ライト",
            Self::Dark => "ダーク",
        }
    }

    pub(super) fn hint(self) -> &'static str {
        match self {
            Self::System => "システムに合わせる",
            Self::Light => "ライト",
            Self::Dark => "ダーク",
        }
    }

    pub(super) fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "system" | "auto" | "システム" => Some(Self::System),
            "light" | "ライト" => Some(Self::Light),
            "dark" | "ダーク" => Some(Self::Dark),
            _ => None,
        }
    }

    pub(super) fn cycle(self) -> Self {
        match self {
            Self::System => Self::Light,
            Self::Light => Self::Dark,
            Self::Dark => Self::System,
        }
    }
}

/// A built-in palette or a theme file in `~/.greencode/themes` (by slug).
#[derive(Clone, Debug, PartialEq)]
pub(super) enum Choice {
    Builtin(ThemeId),
    Custom(String),
}

impl Choice {
    pub(super) fn parse(s: &str) -> Option<Self> {
        if let Some(t) = ThemeId::parse(s).filter(|t| *t != ThemeId::System) {
            return Some(Self::Builtin(t));
        }
        theme::slug(s).ok().map(Self::Custom)
    }

    pub(super) fn id(&self) -> String {
        match self {
            Self::Builtin(t) => t.id().to_string(),
            Self::Custom(slug) => slug.clone(),
        }
    }

    pub(super) fn label(&self, custom: &[(String, CustomTheme)]) -> String {
        match self {
            Self::Builtin(t) => t.label().to_string(),
            Self::Custom(slug) => custom
                .iter()
                .find(|(s, _)| s == slug)
                .map(|(_, t)| t.name.clone())
                .filter(|name| !name.is_empty())
                .unwrap_or_else(|| slug.clone()),
        }
    }

    pub(super) fn dark(&self, custom: &[(String, CustomTheme)], system_dark: bool) -> bool {
        match self {
            Self::Builtin(t) => t.is_dark(system_dark),
            Self::Custom(slug) => custom
                .iter()
                .find(|(s, _)| s == slug)
                .is_some_and(|(_, t)| t.dark),
        }
    }
}

/// Every selectable theme: custom files first, then the built-in palettes. `System` is a mode, not a theme.
pub(super) fn choices(custom: &[(String, CustomTheme)]) -> Vec<Choice> {
    custom
        .iter()
        .map(|(slug, _)| Choice::Custom(slug.clone()))
        .chain(
            ThemeId::ALL
                .into_iter()
                .filter(|t| *t != ThemeId::System)
                .map(Choice::Builtin),
        )
        .collect()
}

pub(super) fn resolve(mode: ThemeMode, system_dark: bool, light: Choice, dark: Choice) -> Choice {
    match mode {
        ThemeMode::System => {
            if system_dark {
                dark
            } else {
                light
            }
        }
        ThemeMode::Light => light,
        ThemeMode::Dark => dark,
    }
}

pub(super) fn mode_from(ui: &Value) -> ThemeMode {
    if let Some(mode) = ui["theme_mode"].as_str().and_then(ThemeMode::parse) {
        return mode;
    }
    // Legacy `theme` held both the mode and the palette. An explicit palette kept its appearance.
    match &ui["theme"] {
        Value::Bool(true) => ThemeMode::Dark,
        Value::Bool(false) => ThemeMode::Light,
        Value::String(s) => ThemeId::parse(s)
            .map(|t| match t {
                ThemeId::System => ThemeMode::System,
                ThemeId::Light => ThemeMode::Light,
                ThemeId::Dark => ThemeMode::Dark,
                t if t.is_dark(false) => ThemeMode::Dark,
                _ => ThemeMode::Light,
            })
            .unwrap_or(ThemeMode::System),
        _ => ThemeMode::System,
    }
}

pub(super) fn slot_from(ui: &Value, dark: bool) -> Choice {
    let key = if dark { "theme_dark" } else { "theme_light" };
    if let Some(choice) = ui[key].as_str().and_then(Choice::parse) {
        return choice;
    }
    match &ui["theme"] {
        Value::String(s) => match ThemeId::parse(s) {
            Some(ThemeId::System) | None => {}
            Some(t) if t.is_dark(false) == dark => return Choice::Builtin(t),
            Some(_) => {}
        },
        _ => {}
    }
    Choice::Builtin(if dark {
        ThemeId::Dark
    } else {
        ThemeId::Light
    })
}

pub(super) fn set_mode(ui: &mut Value, mode: ThemeMode) {
    if !ui.is_object() {
        *ui = json!({});
    }
    ui["theme_mode"] = json!(mode.id());
}

pub(super) fn set_slot(ui: &mut Value, dark: bool, choice: &Choice) {
    if !ui.is_object() {
        *ui = json!({});
    }
    ui[if dark { "theme_dark" } else { "theme_light" }] = json!(choice.id());
}

fn hex_hsla(s: &str) -> Option<Hsla> {
    let [r, g, b, a] = greencode_core::theme::rgba(s)?;
    Some(gpui_kit::rgba(u32::from_be_bytes([r, g, b, a])).into())
}

fn hex_of(c: Hsla) -> String {
    let c: gpui_kit::Rgba = c.into();
    let b = |v: f32| (v.clamp(0., 1.) * 255.).round() as u8;
    let (r, g, b, a) = (b(c.r), b(c.g), b(c.b), b(c.a));
    if a == 255 {
        format!("#{r:02X}{g:02X}{b:02X}")
    } else {
        format!("#{r:02X}{g:02X}{b:02X}{a:02X}")
    }
}

/// `Pal` in `greencode_core::theme::KEYS` order, for saving a palette as a theme file.
fn pal_colors(pal: Pal) -> [Hsla; 13] {
    [
        pal.sidebar, pal.panel, pal.card, pal.fg, pal.muted, pal.line, pal.hover, pal.selected,
        pal.accent, pal.accent_fg, pal.added, pal.removed, pal.warn,
    ]
}

pub(super) fn pal_theme(name: &str, dark: bool, pal: Pal) -> CustomTheme {
    let colors = theme::KEYS
        .iter()
        .copied()
        .zip(pal_colors(pal))
        .map(|(key, c)| (key.to_string(), hex_of(c)))
        .collect();
    CustomTheme {
        name: name.to_string(),
        dark,
        colors,
    }
}

/// A custom theme onto the built-in base of its appearance; keys it omits keep the built-in color.
pub(super) fn custom_pal(t: &CustomTheme) -> Pal {
    let mut pal = Pal::for_theme(
        if t.dark { ThemeId::Dark } else { ThemeId::Light },
        t.dark,
    );
    let set = |field: &mut Hsla, key: &str| {
        if let Some(c) = t.colors.get(key).and_then(|s| hex_hsla(s)) {
            *field = c;
        }
    };
    set(&mut pal.sidebar, "sidebar");
    set(&mut pal.panel, "panel");
    set(&mut pal.card, "card");
    set(&mut pal.fg, "fg");
    set(&mut pal.muted, "muted");
    set(&mut pal.line, "line");
    set(&mut pal.hover, "hover");
    set(&mut pal.selected, "selected");
    set(&mut pal.accent, "accent");
    set(&mut pal.accent_fg, "accent_fg");
    set(&mut pal.added, "added");
    set(&mut pal.removed, "removed");
    set(&mut pal.warn, "warn");
    pal
}

/// Makes the window surfaces translucent so the background image shows through. Text stays opaque.
fn veiled(mut pal: Pal, opacity: f32) -> Pal {
    if opacity <= 0. {
        return pal;
    }
    let fade = |c: Hsla, keep: f32| Hsla {
        a: c.a * (1. - (1. - keep) * opacity),
        ..c
    };
    pal.sidebar = fade(pal.sidebar, 0.30);
    pal.panel = fade(pal.panel, 0.32);
    pal.card = fade(pal.card, 0.55);
    pal
}

fn mtime(path: &std::path::Path) -> Option<SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

/// One hex input per palette color, plus the theme name and appearance.
pub(super) struct ThemeEdit {
    pub(super) name: Entity<InputState>,
    pub(super) fields: Vec<(&'static str, Entity<InputState>)>,
    pub(super) dark: bool,
    pub(super) error: Option<SharedString>,
}

impl Workspace {
    pub(super) fn theme_mode(&self) -> ThemeMode {
        mode_from(&self.ui)
    }

    pub(super) fn theme_slot(&self, dark: bool) -> Choice {
        slot_from(&self.ui, dark)
    }

    pub(super) fn theme_choice(&self, system_dark: bool) -> Choice {
        resolve(
            self.theme_mode(),
            system_dark,
            self.theme_slot(false),
            self.theme_slot(true),
        )
    }

    pub(super) fn custom_theme(&self, choice: &Choice) -> Option<&CustomTheme> {
        let Choice::Custom(slug) = choice else {
            return None;
        };
        self.custom_themes
            .iter()
            .find(|(s, _)| s == slug)
            .map(|(_, t)| t)
    }

    pub(super) fn theme_pal(&self, choice: &Choice, system_dark: bool) -> Pal {
        match choice {
            Choice::Builtin(t) => Pal::for_theme(*t, system_dark),
            Choice::Custom(_) => self.custom_theme(choice).map(custom_pal).unwrap_or_else(|| {
                Pal::for_theme(
                    if system_dark {
                        ThemeId::Dark
                    } else {
                        ThemeId::Light
                    },
                    system_dark,
                )
            }),
        }
    }

    /// True when the window is in dark system appearance.
    pub(super) fn window_is_dark(window: &Window) -> bool {
        matches!(
            window.appearance(),
            gpui_kit::WindowAppearance::Dark | gpui_kit::WindowAppearance::VibrantDark
        )
    }

    pub(super) fn app_is_dark(cx: &App) -> bool {
        matches!(
            cx.window_appearance(),
            gpui_kit::WindowAppearance::Dark | gpui_kit::WindowAppearance::VibrantDark
        )
    }

    pub(super) fn background_image(&self) -> Option<PathBuf> {
        let path = PathBuf::from(self.ui["background_image"].as_str()?);
        path.is_file().then_some(path)
    }

    pub(super) fn background_opacity(&self) -> f32 {
        self.ui["background_opacity"].as_u64().unwrap_or(35).min(100) as f32 / 100.
    }

    pub(super) fn pal(&self, cx: &App) -> Pal {
        let system_dark = Self::app_is_dark(cx);
        let choice = self.theme_choice(system_dark);
        let pal = self.theme_pal(&choice, system_dark);
        if self.background_image().is_some() {
            veiled(pal, self.background_opacity())
        } else {
            pal
        }
    }

    pub(super) fn apply_theme(&self, window: &mut Window, cx: &mut App) {
        let system_dark = Self::window_is_dark(window);
        let choice = self.theme_choice(system_dark);
        let dark = choice.dark(&self.custom_themes, system_dark);
        Theme::change(
            if dark {
                gpui_kit::component::ThemeMode::Dark
            } else {
                gpui_kit::component::ThemeMode::Light
            },
            Some(window),
            cx,
        );
        let pal = self.theme_pal(&choice, system_dark);
        Theme::update(cx, |t| pal.sync_kit_theme(t, dark));
    }

    pub(super) fn refresh_custom_themes(&mut self) {
        self.custom_themes = theme::load_all();
    }

    /// Sets the theme slot the user is looking at: the mode's slot, or the current system appearance's slot.
    pub(super) fn set_theme(&mut self, choice: Choice, window: &mut Window, cx: &mut Context<Self>) {
        let dark = match self.theme_mode() {
            ThemeMode::Light => false,
            ThemeMode::Dark => true,
            ThemeMode::System => Self::window_is_dark(window),
        };
        set_slot(&mut self.ui, dark, &choice);
        self.save_ui();
        self.apply_theme(window, cx);
        cx.notify();
    }

    pub(super) fn set_theme_mode(
        &mut self,
        mode: ThemeMode,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        set_mode(&mut self.ui, mode);
        self.save_ui();
        self.apply_theme(window, cx);
        self.notice(format!("外観: {}", mode.label()));
        cx.notify();
    }

    pub(super) fn background_layer(&self) -> Option<AnyElement> {
        let path = self.background_image()?;
        let opacity = self.background_opacity();
        if opacity <= 0. {
            return None;
        }
        Some(
            div()
                .absolute()
                .inset_0()
                .child(
                    img(path)
                        .size_full()
                        .object_fit(ObjectFit::Cover)
                        .opacity(opacity),
                )
                .into_any_element(),
        )
    }

    pub(super) fn import_theme(&mut self, path: &std::path::Path, window: &mut Window, cx: &mut Context<Self>) {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) => return self.fail(format!("テーマを読み込めません: {e}")),
        };
        let mut theme = match CustomTheme::parse(&text) {
            Ok(theme) => theme,
            Err(e) => return self.fail(format!("テーマを読み込めません: {e:#}")),
        };
        if theme.name.is_empty() {
            theme.name = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("theme")
                .to_string();
        }
        match theme::save(&theme) {
            Ok(slug) => {
                self.refresh_custom_themes();
                set_slot(&mut self.ui, theme.dark, &Choice::Custom(slug));
                set_mode(
                    &mut self.ui,
                    if theme.dark {
                        ThemeMode::Dark
                    } else {
                        ThemeMode::Light
                    },
                );
                self.save_ui();
                self.apply_theme(window, cx);
                self.notice(format!("テーマ「{}」を読み込みました", theme.name));
            }
            Err(e) => self.fail(format!("テーマを保存できません: {e:#}")),
        }
        cx.notify();
    }

    pub(super) fn export_theme(&mut self, window: &mut Window) {
        let system_dark = Self::window_is_dark(window);
        let choice = self.theme_choice(system_dark);
        let theme = match &choice {
            Choice::Custom(_) => self.custom_theme(&choice).cloned(),
            Choice::Builtin(t) => Some(pal_theme(
                &format!("{} コピー", t.label()),
                t.is_dark(system_dark),
                Pal::for_theme(*t, system_dark),
            )),
        };
        let Some(mut theme) = theme else {
            return self.fail("書き出すテーマがありません");
        };
        if theme.name.is_empty() {
            theme.name = choice.label(&self.custom_themes);
        }
        match theme::save(&theme) {
            Ok(slug) => {
                self.refresh_custom_themes();
                self.notice(format!(
                    "テーマを書き出しました: {}",
                    theme::dir().join(format!("{slug}.json")).display()
                ));
            }
            Err(e) => self.fail(format!("テーマを書き出せません: {e:#}")),
        }
    }

    pub(super) fn open_theme_editor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let system_dark = Self::window_is_dark(window);
        let choice = self.theme_choice(system_dark);
        let pal = self.theme_pal(&choice, system_dark);
        let mut theme = match &choice {
            Choice::Custom(_) => self.custom_theme(&choice).cloned(),
            Choice::Builtin(t) => Some(pal_theme(
                &format!("{} カスタム", t.label()),
                t.is_dark(system_dark),
                pal,
            )),
        }
        .unwrap_or_else(|| pal_theme(&choice.id(), system_dark, pal));
        if theme.name.is_empty() {
            theme.name = choice.label(&self.custom_themes);
        }
        // Every key gets a field, including ones the file omitted.
        let base = pal_theme(&theme.name, theme.dark, custom_pal(&theme));
        let name = cx.new(|cx| InputState::new(window, cx).placeholder("テーマ名"));
        name.update(cx, |s, cx| s.set_value(theme.name.clone(), window, cx));
        let fields = theme::KEYS
            .iter()
            .map(|key| {
                let value = theme
                    .colors
                    .get(*key)
                    .or_else(|| base.colors.get(*key))
                    .cloned()
                    .unwrap_or_default();
                let input = cx.new(|cx| InputState::new(window, cx).placeholder("#RRGGBB"));
                input.update(cx, |s, cx| s.set_value(value, window, cx));
                (*key, input)
            })
            .collect();
        name.update(cx, |s, cx| s.focus(window, cx));
        self.theme_edit = Some(ThemeEdit {
            name,
            fields,
            dark: theme.dark,
            error: None,
        });
        cx.notify();
    }

    pub(super) fn close_theme_editor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.theme_edit = None;
        self.input.update(cx, |s, cx| s.focus(window, cx));
        cx.notify();
    }

    pub(super) fn save_theme_editor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (name, dark, values) = {
            let Some(form) = &self.theme_edit else {
                return;
            };
            (
                form.name.read(cx).value().trim().to_string(),
                form.dark,
                form.fields
                    .iter()
                    .map(|(key, input)| (*key, input.read(cx).value().trim().to_string()))
                    .collect::<Vec<_>>(),
            )
        };
        if name.is_empty() {
            self.theme_edit.as_mut().unwrap().error = Some("テーマ名を入力してください".into());
            return cx.notify();
        }
        let mut colors = BTreeMap::new();
        for (key, value) in values {
            match theme::normalize_hex(&value) {
                Some(color) => {
                    colors.insert(key.to_string(), color);
                }
                None => {
                    self.theme_edit.as_mut().unwrap().error =
                        Some(format!("{key}: #RGB / #RRGGBB / #RRGGBBAA で入力してください").into());
                    return cx.notify();
                }
            }
        }
        let theme = CustomTheme { name, dark, colors };
        match theme::save(&theme) {
            Ok(slug) => {
                self.theme_edit = None;
                self.refresh_custom_themes();
                set_slot(&mut self.ui, dark, &Choice::Custom(slug));
                set_mode(
                    &mut self.ui,
                    if dark {
                        ThemeMode::Dark
                    } else {
                        ThemeMode::Light
                    },
                );
                self.save_ui();
                self.apply_theme(window, cx);
                self.notice(format!("テーマ「{}」を保存しました", theme.name));
                self.input.update(cx, |s, cx| s.focus(window, cx));
            }
            Err(e) => {
                self.theme_edit.as_mut().unwrap().error =
                    Some(format!("テーマを保存できません: {e:#}").into());
            }
        }
        cx.notify();
    }

    pub(super) fn theme_editor_view(&self, pal: Pal, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let form = self.theme_edit.as_ref()?;
        let mut grid = div().grid().grid_cols(2).gap_2();
        for (key, input) in &form.fields {
            grid = grid.child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .min_w_0()
                    .child(div().text_xs().text_color(pal.muted).child(*key))
                    .child(Input::new(input)),
            );
        }
        let mut col = div()
            .w(px(760.))
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
            .child("テーマを編集")
            .child(div().text_xs().text_color(pal.muted).child("#RGB / #RRGGBB / #RRGGBBAA"))
            .child(Input::new(&form.name))
            .child(grid)
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_1()
                    .child(
                        chip(
                            "theme-dark",
                            if form.dark {
                                "外観: ダーク"
                            } else {
                                "外観: ライト"
                            },
                            pal,
                        )
                        .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                            if let Some(form) = &mut this.theme_edit {
                                form.dark = !form.dark;
                            }
                            cx.notify();
                        })),
                    )
                    .child(div().flex_1())
                    .child(chip("theme-cancel", "キャンセル  esc", pal).on_click(
                        cx.listener(|this, _: &ClickEvent, window, cx| {
                            this.close_theme_editor(window, cx)
                        }),
                    ))
                    .child(chip("theme-save", "保存", pal).on_click(
                        cx.listener(|this, _: &ClickEvent, window, cx| {
                            this.save_theme_editor(window, cx)
                        }),
                    )),
            );
        if let Some(e) = &form.error {
            col = col.child(div().text_xs().text_color(gpui_kit::red()).child(e.clone()));
        }
        Some(
            div()
                .absolute()
                .inset_0()
                .flex()
                .justify_center()
                .pt(px(80.))
                .child(
                    div()
                        .id("theme-editor-scroll")
                        .max_h(relative(0.9))
                        .overflow_y_scroll()
                        .child(col),
                ),
        )
    }

    /// Applies the theme written by `greencode-cli theme set`, unless the UI saved a choice after it.
    pub(super) fn apply_environment_theme(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(name) = theme::default_name() else {
            return false;
        };
        self.refresh_custom_themes();
        let Ok(slug) = theme::slug(&name) else {
            return false;
        };
        if !self.custom_themes.iter().any(|(s, _)| *s == slug) {
            return false;
        }
        let choice = Choice::Custom(slug);
        let label = self
            .custom_theme(&choice)
            .map_or_else(|| choice.id(), |t| t.name.clone());
        set_slot(&mut self.ui, false, &choice);
        set_slot(&mut self.ui, true, &choice);
        self.save_ui();
        self.apply_theme(window, cx);
        self.notice(format!("既定のテーマ「{label}」を適用しました"));
        cx.notify();
        true
    }

    /// Polls the environment default: one `theme set` applies once, a later in-app choice sticks.
    pub(super) fn watch_environment_theme(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if cfg!(test) {
            return;
        }
        let path = theme::default_path();
        let ui = greencode_core::home().join("ui.json");
        let mut last = mtime(&path);
        // Offline when the CLI ran: apply a default newer than the last UI save now.
        if last.is_some_and(|t| mtime(&ui).is_none_or(|saved| t > saved)) {
            self.apply_environment_theme(window, cx);
            last = mtime(&path);
        }
        cx.spawn_in(window, async move |this, cx| loop {
            cx.background_executor()
                .timer(std::time::Duration::from_secs(2))
                .await;
            let now = mtime(&path);
            if now == last {
                continue;
            }
            last = now;
            if now.is_some()
                && this
                    .update_in(cx, |this, window, cx| {
                        this.apply_environment_theme(window, cx);
                    })
                    .is_err()
            {
                break;
            }
        })
        .detach();
    }
}

#[cfg(test)]
mod tests {
    // No glob import: with test-support, `gpui_kit::*` brings a `test` macro that shadows `#[test]`.
    use super::{
        custom_pal, mode_from, pal_theme, resolve, set_mode, set_slot, slot_from, Choice, ThemeMode,
    };
    use crate::thread_view::{Pal, ThemeId};
    use greencode_core::theme::{self, Theme};
    use serde_json::json;

    #[test]
    fn mode_and_slots_resolve_light_and_dark() {
        let e = Choice::Builtin(ThemeId::Emerald);
        let p = Choice::Builtin(ThemeId::Paper);
        assert_eq!(resolve(ThemeMode::System, false, p.clone(), e.clone()), p);
        assert_eq!(resolve(ThemeMode::System, true, p.clone(), e.clone()), e);
        assert_eq!(resolve(ThemeMode::Light, true, p.clone(), e.clone()), p);
        assert_eq!(resolve(ThemeMode::Dark, false, p, e.clone()), e);
        assert_eq!(ThemeMode::System.cycle(), ThemeMode::Light);
        assert_eq!(ThemeMode::Dark.cycle(), ThemeMode::System);
    }

    #[test]
    fn legacy_theme_json_maps_to_mode_and_slots() {
        let dark = json!({"theme": true});
        assert_eq!(mode_from(&dark), ThemeMode::Dark);
        assert_eq!(slot_from(&dark, true), Choice::Builtin(ThemeId::Dark));

        let light = json!({"theme": "light"});
        assert_eq!(mode_from(&light), ThemeMode::Light);
        assert_eq!(slot_from(&light, false), Choice::Builtin(ThemeId::Light));

        let emerald = json!({"theme": "emerald"});
        assert_eq!(mode_from(&emerald), ThemeMode::Dark);
        assert_eq!(slot_from(&emerald, true), Choice::Builtin(ThemeId::Emerald));
        // The unused slot keeps the neutral default.
        assert_eq!(slot_from(&emerald, false), Choice::Builtin(ThemeId::Light));

        let system = json!({});
        assert_eq!(mode_from(&system), ThemeMode::System);
        assert_eq!(slot_from(&system, false), Choice::Builtin(ThemeId::Light));
        assert_eq!(slot_from(&system, true), Choice::Builtin(ThemeId::Dark));

        let mut split = json!({"theme": "emerald"});
        set_mode(&mut split, ThemeMode::System);
        set_slot(&mut split, false, &Choice::Builtin(ThemeId::Paper));
        set_slot(&mut split, true, &Choice::Custom("night".into()));
        assert_eq!(mode_from(&split), ThemeMode::System);
        assert_eq!(slot_from(&split, false), Choice::Builtin(ThemeId::Paper));
        assert_eq!(slot_from(&split, true), Choice::Custom("night".into()));
    }

    #[test]
    fn palette_saves_and_reloads_as_a_theme() {
        let pal = Pal::for_theme(ThemeId::Emerald, true);
        let t = pal_theme("チェック", true, pal);
        let again = Theme::from_json(&t.to_json()).unwrap();
        assert_eq!(t, again);
        assert_eq!(t.colors.len(), theme::KEYS.len());
        let back = custom_pal(&again);
        let (a, b): (gpui_kit::Rgba, gpui_kit::Rgba) = (back.accent.into(), pal.accent.into());
        assert!((a.r - b.r).abs() < 0.01 && (a.g - b.g).abs() < 0.01 && (a.b - b.b).abs() < 0.01);
    }
}
