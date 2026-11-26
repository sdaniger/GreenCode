use anyhow::{anyhow, Context, Result};
use ratatui::style::Color;
use serde::{Deserialize, Deserializer};
use std::{collections::BTreeMap, path::Path};

#[derive(Deserialize)]
#[serde(untagged)]
enum Token {
    Index(u8),
    Text(String),
}

fn color(token: &Token, vars: &BTreeMap<String, Token>) -> Result<Color> {
    let mut token = token;
    for _ in 0..=vars.len() {
        match token {
            Token::Index(n) => return Ok(Color::Indexed(*n)),
            Token::Text(s) if s.is_empty() => return Ok(Color::Reset),
            Token::Text(s) if s.starts_with('#') => return Ok(s.parse()?),
            Token::Text(s) => token = vars.get(s).with_context(|| format!("Unknown theme variable {s:?}"))?,
        }
    }
    Err(anyhow!("Cyclic theme variable reference"))
}

macro_rules! palette {
    ($($field:ident => $token:literal: $default:path),+ $(,)?) => {
        pub(super) struct Theme { $(pub $field: Color,)+ }

        impl Default for Theme {
            fn default() -> Self { Self { $($field: $default,)+ } }
        }

        impl<'de> Deserialize<'de> for Theme {
            fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
                #[derive(Deserialize)]
                struct Document {
                    #[serde(default)] vars: BTreeMap<String, Token>,
                    colors: BTreeMap<String, serde_json::Value>,
                }
                let doc = Document::deserialize(de)?;
                let get = |key: &str, default| {
                    let Some(value) = doc.colors.get(key) else { return Ok(default) };
                    Token::deserialize(value).map_err(anyhow::Error::from)
                        .and_then(|token| color(&token, &doc.vars))
                        .map_err(|err| serde::de::Error::custom(format!("colors.{key}: {err}")))
                };
                Ok(Self { $($field: get($token, $default)?,)+ })
            }
        }
    };
}

palette! {
    accent => "accent": Color::Cyan,
    border => "borderAccent": Color::Cyan,
    warning => "warning": Color::Yellow,
    muted => "muted": Color::Gray,
    dim => "dim": Color::DarkGray,
    text => "text": Color::Reset,
    user => "userMessageText": Color::Cyan,
    assistant => "assistantMessageText": Color::Reset,
    thinking => "thinkingText": Color::DarkGray,
    tool => "toolTitle": Color::Blue,
    success => "success": Color::Green,
    error => "error": Color::LightRed,
    heading => "mdHeading": Color::LightCyan,
    code => "mdCodeBlockBorder": Color::LightCyan,
    selection_bg => "selectedBg": Color::Reset,
    user_bg => "userMessageBg": Color::Reset,
    tool_bg => "toolPendingBg": Color::Reset,
    tool_ok_bg => "toolSuccessBg": Color::Reset,
    tool_error_bg => "toolErrorBg": Color::Reset,
    status_bg => "statusLineBg": Color::Reset,
}

impl Theme {
    pub fn load(path: Option<&Path>) -> Result<Self> {
        let Some(path) = path else { return Ok(Self::default()) };
        let bytes = std::fs::read(path).with_context(|| format!("Cannot read theme {}", path.display()))?;
        serde_json::from_slice(&bytes).with_context(|| format!("Invalid theme {}", path.display()))
    }

    /// `theme_light`/`theme_dark` for the terminal's reported background, then `theme`.
    pub fn load_preferred(tui: &greencode_core::config::TuiCfg) -> Result<Self> {
        let path = if light_background() { tui.theme_light.as_deref() } else { tui.theme_dark.as_deref() }
            .or(tui.theme.as_deref());
        Self::load(path.and_then(resolve_theme).as_deref())
    }
}

/// `COLORFGBG` is `fg;bg`; 7 and 9–15 are light backgrounds.
fn light_background() -> bool {
    std::env::var("COLORFGBG").ok()
        .and_then(|v| v.rsplit(';').next()?.trim().parse::<u8>().ok())
        .is_some_and(|bg| bg == 7 || (9..=15).contains(&bg))
}

/// A bare name resolves to `~/.greencode/tui-themes/NAME.json` so OMP theme names import unchanged;
/// an unavailable bare name keeps the built-in palette instead of breaking startup.
fn resolve_theme(path: &Path) -> Option<std::path::PathBuf> {
    let bare = !path.is_absolute() && path.extension().is_none()
        && !path.to_string_lossy().contains(['/', '\\']);
    if !bare { return Some(path.to_path_buf()); }
    if path.is_file() { return Some(path.to_path_buf()); }
    let named = greencode_core::home().join("tui-themes").join(format!("{}.json", path.display()));
    named.is_file().then_some(named)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_bare_theme_names_keep_the_builtin_palette() {
        assert!(resolve_theme(Path::new("greencode-missing-theme")).is_none());
        assert_eq!(resolve_theme(Path::new("/no/such/theme.json")), Some(Path::new("/no/such/theme.json").into()));
    }

    #[test]
    fn imports_omp_colors_without_interpreting_unrendered_tokens() {
        let theme: Theme = serde_json::from_str(r##"{"vars":{"blue":"#102030","alias":"blue"},"colors":{"accent":"alias","success":12,"assistantMessageText":"","unused":"unknown"}}"##).unwrap();
        assert_eq!(theme.accent, Color::Rgb(16, 32, 48));
        assert_eq!(theme.success, Color::Indexed(12));
        assert_eq!(theme.assistant, Color::Reset);
        let err = serde_json::from_str::<Theme>(r#"{"vars":{"x":"y","y":"x"},"colors":{"accent":"x"}}"#).err().unwrap();
        assert!(err.to_string().contains("colors.accent: Cyclic"));
    }
}
