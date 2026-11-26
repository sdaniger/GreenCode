//! Custom UI themes: palette JSON files in `~/.greencode/themes`, VS Code import, and `greencode-cli theme`.

use anyhow::{bail, Context, Result};
use serde_json::{json, Map, Value};
use std::{collections::BTreeMap, fs, path::{Path, PathBuf}};

/// The palette fields a theme sets, in editor order. A missing key falls back to the built-in light/dark palette.
pub const KEYS: [&str; 13] = [
    "sidebar", "panel", "card", "fg", "muted", "line", "hover", "selected", "accent", "accent_fg", "added", "removed", "warn",
];
/// Names that would clash with built-in themes and the `/theme` command.
const RESERVED: [&str; 8] = ["system", "light", "dark", "emerald", "emerald-light", "paper", "sand", "graphite"];
pub const MAX_BACKGROUND: u64 = 16 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq)]
pub struct Theme {
    pub name: String,
    pub dark: bool,
    /// Key (from `KEYS`) to `#RRGGBB` or `#RRGGBBAA`.
    pub colors: BTreeMap<String, String>,
}

type Rgba = [u8; 4];

fn parse_hex(s: &str) -> Option<Rgba> {
    let h = s.trim().trim_start_matches('#');
    if !h.is_ascii() || !matches!(h.len(), 3 | 4 | 6 | 8) {
        return None;
    }
    let digit = |i: usize, n: usize| u8::from_str_radix(&h[i..i + n], 16).ok();
    let short = h.len() <= 4;
    let n = if short { 1 } else { 2 };
    let ch = |i: usize| digit(i * n, n).map(|v| if short { v * 17 } else { v });
    Some([ch(0)?, ch(1)?, ch(2)?, if h.len() % 4 == 0 { ch(3)? } else { 255 }])
}

fn fmt_hex([r, g, b, a]: Rgba) -> String {
    if a == 255 { format!("#{r:02X}{g:02X}{b:02X}") } else { format!("#{r:02X}{g:02X}{b:02X}{a:02X}") }
}

/// `#RGB`, `#RGBA`, `#RRGGBB` or `#RRGGBBAA` as upper-case `#RRGGBB[AA]`.
pub fn normalize_hex(s: &str) -> Option<String> {
    parse_hex(s).map(fmt_hex)
}

/// The bytes of any accepted hex form, for callers that paint with their own color type.
pub fn rgba(s: &str) -> Option<[u8; 4]> {
    parse_hex(s)
}

fn luminance(c: Rgba) -> f32 {
    let lin = |v: u8| {
        let v = v as f32 / 255.;
        if v <= 0.03928 { v / 12.92 } else { ((v + 0.055) / 1.055).powf(2.4) }
    };
    0.2126 * lin(c[0]) + 0.7152 * lin(c[1]) + 0.0722 * lin(c[2])
}

fn with_alpha(c: &str, a: u8) -> String {
    let mut c = parse_hex(c).unwrap_or([0, 0, 0, 255]);
    c[3] = a;
    fmt_hex(c)
}

/// `a` blended towards `b` by `t`, opaque.
fn mix(a: &str, b: &str, t: f32) -> String {
    let (a, b) = (parse_hex(a).unwrap_or([0, 0, 0, 255]), parse_hex(b).unwrap_or([255, 255, 255, 255]));
    let m = |i: usize| (a[i] as f32 + (b[i] as f32 - a[i] as f32) * t).round() as u8;
    fmt_hex([m(0), m(1), m(2), 255])
}

impl Theme {
    /// GreenCode's own format: `{"name", "appearance": "light"|"dark", <KEYS>: "#hex"}`.
    pub fn from_json(v: &Value) -> Result<Theme> {
        let mut colors = BTreeMap::new();
        for k in KEYS {
            if let Some(s) = v.get(k).and_then(Value::as_str) {
                colors.insert(k.to_string(), normalize_hex(s).with_context(|| format!("invalid color for {k}: {s}"))?);
            }
        }
        let dark = match v.get("appearance").and_then(Value::as_str) {
            Some("dark") => true,
            Some("light") => false,
            Some(other) => bail!("appearance must be light or dark, not {other}"),
            None => colors.get("panel").and_then(|c| parse_hex(c)).is_none_or(|c| luminance(c) < 0.18),
        };
        Ok(Theme { name: v.get("name").and_then(Value::as_str).unwrap_or_default().to_string(), dark, colors })
    }

    pub fn to_json(&self) -> Value {
        let mut m = Map::new();
        m.insert("name".into(), json!(self.name));
        m.insert("appearance".into(), json!(if self.dark { "dark" } else { "light" }));
        for k in KEYS {
            if let Some(c) = self.colors.get(k) {
                m.insert(k.into(), json!(c));
            }
        }
        Value::Object(m)
    }

    /// Maps a VS Code color theme's `colors` onto the palette; colors the theme lacks are derived from its foreground.
    pub fn from_vscode(v: &Value) -> Result<Theme> {
        let get = |ks: &[&str]| ks.iter().find_map(|k| v["colors"][*k].as_str().and_then(normalize_hex));
        let panel = get(&["editor.background"]).context("the theme has no editor.background color")?;
        let panel_rgba = parse_hex(&panel).unwrap_or([0, 0, 0, 255]);
        let dark = match v["type"].as_str() {
            Some("light" | "hc-light") => false,
            Some("dark" | "hc" | "hc-black") => true,
            _ => luminance(panel_rgba) < 0.18,
        };
        let fg = get(&["editor.foreground", "foreground"]).unwrap_or_else(|| if dark { "#E6E6E6" } else { "#1F1F1F" }.into());
        let d = |dark_c: &str, light_c: &str| if dark { dark_c } else { light_c }.to_string();
        let accent = get(&["button.background", "focusBorder", "textLink.foreground", "progressBar.background"])
            .unwrap_or_else(|| d("#4C8DFF", "#1D68D8"));
        let accent_fg = get(&["button.foreground"]).unwrap_or_else(|| {
            let lum = luminance(parse_hex(&accent).unwrap_or([0, 0, 0, 255]));
            if lum > 0.4 { "#101010" } else { "#FFFFFF" }.into()
        });
        let mut colors = BTreeMap::new();
        let mut put = |k: &str, c: String| {
            colors.insert(k.to_string(), c);
        };
        put("sidebar", get(&["sideBar.background", "activityBar.background"]).unwrap_or_else(|| panel.clone()));
        put("card", get(&["editorWidget.background", "input.background", "sideBarSectionHeader.background"]).unwrap_or_else(|| mix(&panel, &fg, 0.06)));
        put("muted", get(&["descriptionForeground", "editorLineNumber.foreground"]).unwrap_or_else(|| mix(&panel, &fg, 0.62)));
        put("line", get(&["panel.border", "sideBar.border", "editorGroup.border", "contrastBorder"]).unwrap_or_else(|| with_alpha(&fg, 0x1F)));
        put("hover", get(&["list.hoverBackground", "toolbar.hoverBackground"]).unwrap_or_else(|| with_alpha(&fg, 0x0F)));
        put("selected", get(&["list.activeSelectionBackground", "list.inactiveSelectionBackground", "editor.selectionBackground"]).unwrap_or_else(|| with_alpha(&fg, 0x1A)));
        put("added", get(&["gitDecoration.addedResourceForeground", "editorGutter.addedBackground", "terminal.ansiGreen"]).unwrap_or_else(|| d("#68B384", "#2E6F52")));
        put("removed", get(&["gitDecoration.deletedResourceForeground", "editorGutter.deletedBackground", "terminal.ansiRed", "errorForeground"]).unwrap_or_else(|| d("#DE6F68", "#B53832")));
        put("warn", get(&["editorWarning.foreground", "gitDecoration.modifiedResourceForeground", "terminal.ansiYellow"]).unwrap_or_else(|| d("#DF9F45", "#AD6C16")));
        put("panel", panel);
        put("fg", fg);
        put("accent", accent);
        put("accent_fg", accent_fg);
        Ok(Theme { name: v["name"].as_str().unwrap_or_default().to_string(), dark, colors })
    }

    /// Reads either format; VS Code themes are recognised by their `colors` object and may contain comments.
    pub fn parse(text: &str) -> Result<Theme> {
        let v: Value = serde_json::from_str(&strip_jsonc(text)).context("not valid JSON")?;
        if v["colors"].is_object() { Self::from_vscode(&v) } else { Self::from_json(&v) }
    }
}

/// Removes `//` and `/* */` comments and trailing commas, which VS Code theme files allow.
pub fn strip_jsonc(s: &str) -> String {
    let c: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    let (mut i, mut in_str) = (0, false);
    while i < c.len() {
        match c[i] {
            '"' => {
                in_str = !in_str;
                out.push('"');
            }
            '\\' if in_str => {
                out.push('\\');
                i += 1;
                if let Some(&n) = c.get(i) {
                    out.push(n);
                }
            }
            '/' if !in_str && c.get(i + 1) == Some(&'/') => {
                while i < c.len() && c[i] != '\n' {
                    i += 1;
                }
                continue;
            }
            '/' if !in_str && c.get(i + 1) == Some(&'*') => {
                i += 2;
                while i + 1 < c.len() && !(c[i] == '*' && c[i + 1] == '/') {
                    i += 1;
                }
                i += 2;
                continue;
            }
            ch => out.push(ch),
        }
        i += 1;
    }
    // Second pass, once comments between a comma and its bracket are gone.
    let c: Vec<char> = out.chars().collect();
    let (mut out, mut in_str, mut esc) = (String::with_capacity(c.len()), false, false);
    for (i, &ch) in c.iter().enumerate() {
        let trailing = ch == ',' && !in_str && c[i + 1..].iter().find(|x| !x.is_whitespace()).is_some_and(|x| matches!(x, '}' | ']'));
        if !trailing {
            out.push(ch);
        }
        in_str = if esc { in_str } else { in_str ^ (ch == '"') };
        esc = in_str && ch == '\\' && !esc;
    }
    out
}

pub fn dir() -> PathBuf {
    crate::home().join("themes")
}

/// The environment default written by `greencode-cli theme set`: one line naming a theme in `dir()`.
pub fn default_path() -> PathBuf {
    crate::home().join("theme-default")
}

/// File-name stem for a theme name; keeps letters, digits, `-` and `_`.
pub fn slug(name: &str) -> Result<String> {
    let s: String = name
        .trim()
        .chars()
        .map(|c| if c.is_alphanumeric() || c == '-' || c == '_' { c.to_ascii_lowercase() } else { '-' })
        .collect();
    let s = s.trim_matches('-').to_string();
    if s.is_empty() || RESERVED.contains(&s.as_str()) {
        bail!("`{name}` cannot be used as a theme name");
    }
    Ok(s)
}

fn write_atomic(path: &Path, text: &str) -> Result<()> {
    fs::create_dir_all(path.parent().context("no parent directory")?)?;
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, text)?;
    fs::rename(&tmp, path)?;
    Ok(())
}

/// Saves `theme` as `themes/<slug>.json` and returns the slug.
pub fn save(theme: &Theme) -> Result<String> {
    let slug = slug(&theme.name)?;
    write_atomic(&dir().join(format!("{slug}.json")), &serde_json::to_string_pretty(&theme.to_json())?)?;
    Ok(slug)
}

/// Every valid theme in `dir()` as `(slug, theme)`, sorted by slug. Invalid files are skipped.
pub fn load_all() -> Vec<(String, Theme)> {
    let mut v: Vec<(String, Theme)> = fs::read_dir(dir())
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let p = e.path();
            let stem = p.file_stem()?.to_str()?;
            if p.extension()? != "json" {
                return None;
            }
            // The file name is the id; a human-readable one (`Night Owl.json`) gets a slug id.
            let id = slug(stem).ok()?;
            let mut t = Theme::parse(&fs::read_to_string(&p).ok()?).ok()?;
            if t.name.is_empty() {
                t.name = stem.to_string();
            }
            Some((id, t))
        })
        .collect();
    v.sort_by(|a, b| a.0.cmp(&b.0));
    v
}

pub fn default_name() -> Option<String> {
    fs::read_to_string(default_path()).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

/// A background image must be a JPEG, PNG or WebP of at most 16 MB.
pub fn check_background(path: &Path) -> Result<()> {
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or_default().to_ascii_lowercase();
    if !matches!(ext.as_str(), "jpg" | "jpeg" | "png" | "webp") {
        bail!("background image must be JPEG, PNG or WebP");
    }
    let len = fs::metadata(path).with_context(|| format!("cannot read {}", path.display()))?.len();
    if len > MAX_BACKGROUND {
        bail!("background image is {} MB; the limit is 16 MB", len / (1024 * 1024));
    }
    Ok(())
}

/// `greencode-cli theme set FILE|NAME | clear | show`.
pub fn cli(args: &[String]) -> Result<()> {
    const USAGE: &str = "greencode-cli theme set FILE|NAME | theme clear | theme show";
    match args.iter().map(String::as_str).collect::<Vec<_>>().as_slice() {
        ["set", target] => {
            let path = Path::new(target);
            let (mut theme, source) = if path.is_file() {
                (Theme::parse(&fs::read_to_string(path)?).with_context(|| format!("cannot read theme {target}"))?, Some(path))
            } else {
                let name = slug(target)?;
                let found = load_all().into_iter().find(|(s, _)| *s == name).with_context(|| format!("no theme file or installed theme named {target}"))?;
                (found.1, None)
            };
            if theme.name.is_empty() {
                theme.name = source.and_then(|p| p.file_stem()).and_then(|s| s.to_str()).unwrap_or(target).to_string();
            }
            let slug = if source.is_some() { save(&theme)? } else { slug(&theme.name).or_else(|_| slug(target))? };
            // Rewritten even when unchanged, so running it again re-applies the theme.
            write_atomic(&default_path(), &format!("{slug}\n"))?;
            println!("Default theme: {slug} ({})", if theme.dark { "dark" } else { "light" });
        }
        ["clear"] => {
            match fs::remove_file(default_path()) {
                Ok(()) => println!("Default theme cleared."),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => println!("No default theme was set."),
                Err(e) => return Err(e.into()),
            }
        }
        ["show"] => {
            println!("Default: {}", default_name().unwrap_or_else(|| "(none)".into()));
            for (slug, t) in load_all() {
                println!("  {slug}\t{}\t{}", t.name, if t.dark { "dark" } else { "light" });
            }
        }
        _ => bail!("usage: {USAGE}"),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vscode_import_maps_colors_and_tolerates_comments() {
        let text = r##"{
            // a comment, with "quotes" and a url http://x
            "name": "Night Owl", "type": "dark",
            "colors": {
                "editor.background": "#011627", "editor.foreground": "#d6deeb",
                "sideBar.background": "#010e1a", "button.background": "#7e57c2cc",
                "button.foreground": "#fff", "list.hoverBackground": "#011627",
                "gitDecoration.addedResourceForeground": "#a2bffc",
                "terminal.ansiRed": "#ef5350", /* block */
            },
            "tokenColors": [],
        }"##;
        let t = Theme::parse(text).unwrap();
        assert_eq!((t.name.as_str(), t.dark), ("Night Owl", true));
        let c = |k: &str| t.colors[k].as_str();
        assert_eq!((c("panel"), c("fg"), c("sidebar")), ("#011627", "#D6DEEB", "#010E1A"));
        assert_eq!((c("accent"), c("accent_fg"), c("hover")), ("#7E57C2CC", "#FFFFFF", "#011627"));
        assert_eq!((c("added"), c("removed")), ("#A2BFFC", "#EF5350"));
        // Missing entries are derived, so every palette field is present.
        assert!(KEYS.iter().all(|k| t.colors.contains_key(*k)));
        assert_eq!(c("line"), "#D6DEEB1F");
    }

    #[test]
    fn vscode_without_editor_background_is_rejected_and_light_type_is_honoured() {
        assert!(Theme::parse(r##"{"colors": {"foreground": "#fff"}}"##).is_err());
        let t = Theme::parse(r##"{"colors": {"editor.background": "#ffffff"}}"##).unwrap();
        assert!(!t.dark);
    }

    #[test]
    fn json_round_trips_and_rejects_bad_colors() {
        let t = Theme::parse(r##"{"colors": {"editor.background": "#fdf6e3", "editor.foreground": "#657b83"}, "type": "light"}"##).unwrap();
        let again = Theme::from_json(&t.to_json()).unwrap();
        assert_eq!(t, again);
        assert!(Theme::from_json(&json!({"appearance": "dark", "accent": "blue"})).is_err());
        assert!(Theme::from_json(&json!({"appearance": "sepia"})).is_err());
    }

    #[test]
    fn names_reject_builtins_and_hex_forms_normalize() {
        assert_eq!(slug(" Night Owl! ").unwrap(), "night-owl");
        assert!(slug("Paper").is_err() && slug("???").is_err());
        assert_eq!(normalize_hex("#abc").as_deref(), Some("#AABBCC"));
        assert_eq!(normalize_hex("1122").as_deref(), Some("#11112222"));
        assert_eq!(normalize_hex("#12345"), None);
    }
}
