//! The TUI status line: which segments each preset shows, how separators look, and how the
//! context-reactive gap between the left and right segments renders.
//!
//! Pure text composition; colors and placement stay in the terminal UI.

use crate::config::{ContextLine, StatusLineCfg, StatusPreset, StatusSeparator};

pub struct Info<'a> {
    pub model: &'a str,
    pub mode: &'a str,
    pub effort: &'a str,
    pub context: u64,
    /// The model's context window when known; the gauge falls back to the compaction threshold.
    pub context_window: Option<u64>,
    pub cost: Option<f64>,
    pub git: Option<(&'a str, bool)>,
    pub queue: usize,
    pub agents: usize,
    pub session: &'a str,
    pub turn_tokens: Option<(u64, u64)>,
    pub scroll: bool,
}

/// Left text, right text, and the gap in between as (glyph, filled) cells.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Row {
    pub left: String,
    pub right: String,
    pub gauge: Vec<(char, bool)>,
}

impl Row {
    /// The row as one string, for tests and non-styled output.
    pub fn text(&self) -> String {
        let gap: String = self.gauge.iter().map(|&(c, _)| c).collect();
        format!("{}{gap}{}", self.left, self.right)
    }
}

/// Nerd-font preset icons heading every segment.
fn preset_icons(preset: StatusPreset) -> bool {
    preset == StatusPreset::Nerd
}

fn sep_glyph(separator: StatusSeparator, preset: StatusPreset, transparent: bool) -> &'static str {
    if preset == StatusPreset::Ascii {
        return "|";
    }
    match separator {
        // The powerline cap needs a segment fill to read as a triangle; with the terminal
        // background showing through it becomes a stray glyph.
        StatusSeparator::Powerline if transparent => " ",
        StatusSeparator::Powerline => "\u{e0b0}",
        StatusSeparator::PowerlineThin => "\u{e0b1}",
        StatusSeparator::Slash => "/",
        StatusSeparator::Pipe => "|",
        StatusSeparator::Block => "█",
        StatusSeparator::None => " ",
        StatusSeparator::Ascii => "|",
    }
}

/// One glyph per reasoning level; used on the model name when `compact_thinking_level` is on.
pub fn thinking_icon(effort: &str, ascii: bool) -> &'static str {
    let levels = if ascii {
        ["-", "1", "2", "3", "4", "5"]
    } else {
        ["○", "◔", "◑", "◕", "●", "◉"]
    };
    let index = match effort.to_ascii_lowercase().as_str() {
        "minimal" => 0,
        "low" => 1,
        "medium" => 2,
        "high" => 3,
        "xhigh" => 4,
        "max" => 5,
        _ => 0,
    };
    levels[index]
}

fn money(cost: f64) -> String {
    if cost >= 100.0 {
        format!("${cost:.0}")
    } else if cost >= 1.0 {
        format!("${cost:.2}")
    } else {
        format!("${cost:.3}")
    }
}

fn tokens(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1e6)
    } else if n >= 1_000 {
        format!("{:.0}k", n as f64 / 1e3)
    } else {
        n.to_string()
    }
}

/// Which segments a preset shows as (left, right) labels; `None` entries are omitted.
fn layout(preset: StatusPreset) -> (Vec<&'static str>, Vec<&'static str>) {
    match preset {
        StatusPreset::Minimal => (vec!["model"], vec!["context"]),
        StatusPreset::Compact => (vec!["model", "thinking"], vec!["git", "context"]),
        StatusPreset::Default => (vec!["model", "thinking"], vec!["git", "context"]),
        StatusPreset::Full | StatusPreset::Nerd | StatusPreset::Ascii => (
            vec!["model", "mode", "thinking", "queue"],
            vec!["agents", "tokens", "cost", "context", "session"],
        ),
        StatusPreset::Custom => (
            vec!["model", "mode", "thinking", "queue"],
            vec!["agents", "tokens", "cost", "context", "session", "scroll"],
        ),
    }
}

fn segments(cfg: &StatusLineCfg, info: &Info<'_>) -> (Vec<String>, Vec<String>) {
    let icons = preset_icons(cfg.preset);
    let ascii = cfg.preset == StatusPreset::Ascii;
    let window = info.context_window.unwrap_or(0);
    let percent = if window == 0 {
        String::new()
    } else {
        format!("{}%", (info.context as f64 / window as f64 * 100.0).round().min(999.0) as u64)
    };
    let (mut left, mut right) = (Vec::new(), Vec::new());
    let (left_keys, right_keys) = layout(cfg.preset);
    for key in left_keys {
        let text = match key {
            "model" => {
                let compact = cfg.compact_thinking_level && cfg.preset != StatusPreset::Minimal;
                if compact {
                    format!("{}{}", info.model, thinking_icon(info.effort, ascii))
                } else if icons {
                    format!("\u{f2db} {}", info.model)
                } else {
                    info.model.to_string()
                }
            }
            "mode" => format!("{}{}", if icons { "\u{f023} " } else { "" }, info.mode),
            "thinking" => {
                if cfg.compact_thinking_level && cfg.preset != StatusPreset::Minimal {
                    continue;
                }
                format!("thinking {}", info.effort)
            }
            "queue" => {
                if info.queue == 0 {
                    continue;
                }
                format!("{} queued", info.queue)
            }
            _ => continue,
        };
        left.push(text);
    }
    for key in right_keys {
        let text = match key {
            "git" => match info.git {
                Some((branch, dirty)) => format!("{}{branch}{}", if icons { "\u{e725} " } else { "" }, if dirty { "*" } else { "" }),
                None => continue,
            },
            "agents" => {
                if info.agents == 0 {
                    continue;
                }
                format!("{} sub-agent(s)", info.agents)
            }
            "context" => {
                if percent.is_empty() {
                    continue;
                }
                format!("{}{percent} ctx", if icons { "\u{f1c0} " } else { "" })
            }
            "tokens" => match info.turn_tokens {
                Some((input, output)) => format!("{}{}↑ {}↓", if icons { "\u{f1e0} " } else { "" }, tokens(input), tokens(output)),
                None => continue,
            },
            "cost" => match info.cost {
                Some(cost) => format!("{}{}", if icons { "\u{f155} " } else { "" }, money(cost)),
                None => continue,
            },
            "session" => {
                if info.session.trim().is_empty() {
                    continue;
                }
                format!("{}{}", if icons { "\u{f135} " } else { "" }, info.session)
            }
            "scroll" => {
                if !info.scroll {
                    continue;
                }
                "PgDn latest".to_string()
            }
            _ => continue,
        };
        right.push(text);
    }
    (left, right)
}

/// `width` cells of gauge; the label is embedded for `embedded`, shown alone otherwise.
pub fn gauge(cfg: &StatusLineCfg, info: &Info<'_>, width: usize) -> Vec<(char, bool)> {
    if width == 0 || cfg.context_line == ContextLine::Off {
        return Vec::new();
    }
    let window = info.context_window.unwrap_or(0).max(1);
    let fraction = (info.context as f64 / window as f64).clamp(0.0, 1.0);
    let label = match cfg.context_line {
        ContextLine::Off => String::new(),
        ContextLine::Percentage => format!("{:>3}%", (fraction * 100.0).round() as u64),
        ContextLine::Annotated => format!("{:>3}% {} / {}", (fraction * 100.0).round() as u64, tokens(info.context), tokens(window)),
        ContextLine::Embedded => format!("{}%", (fraction * 100.0).round() as u64),
    };
    let chars: Vec<char> = label.chars().collect();
    if chars.len() >= width {
        return chars.into_iter().take(width).map(|c| (c, false)).collect();
    }
    match cfg.context_line {
        ContextLine::Percentage | ContextLine::Annotated => {
            let lead = (width - chars.len()) / 2;
            let mut cells = vec![(' ', false); width];
            for (i, c) in chars.into_iter().enumerate() {
                cells[lead + i] = (c, false);
            }
            cells
        }
        ContextLine::Embedded => {
            let bar = width - chars.len();
            let filled = (bar as f64 * fraction).round() as usize;
            let lead = (bar + 1) / 2;
            (0..width)
                .map(|i| {
                    if i >= lead && i < lead + chars.len() {
                        (chars[i - lead], false)
                    } else {
                        let slot = if i < lead { i } else { i - chars.len() };
                        if slot < filled { ('█', true) } else { ('▁', false) }
                    }
                })
                .collect()
        }
        ContextLine::Off => Vec::new(),
    }
}

/// The whole status row fitted to `width` columns: right segments are dropped before the left
/// is cut, and the gauge takes whatever gap remains.
pub fn compose(cfg: &StatusLineCfg, info: &Info<'_>, width: usize) -> Row {
    let (mut left, mut right) = segments(cfg, info);
    let sep = sep_glyph(cfg.separator, cfg.preset, cfg.transparent);
    let join = |parts: &[String]| parts.join(&format!(" {sep} "));
    let mut left_text = join(&left);
    let mut right_text = join(&right);
    // Drop right segments, then left ones, until the row fits.
    while left_text.chars().count() + right_text.chars().count() > width && !right.is_empty() {
        right.pop();
        right_text = join(&right);
    }
    while left_text.chars().count() > width && left.len() > 1 {
        left.pop();
        left_text = join(&left);
    }
    let gauge_width = width.saturating_sub(left_text.chars().count() + right_text.chars().count());
    let gauge = if cfg.context_line == ContextLine::Off { Vec::new() } else { gauge(cfg, info, gauge_width) };
    let actual_gauge = gauge.len();
    let total = left_text.chars().count() + right_text.chars().count() + actual_gauge;
    if total > width {
        left_text = left_text.chars().take(width.saturating_sub(right_text.chars().count() + actual_gauge)).collect();
    }
    let used = left_text.chars().count() + right_text.chars().count() + actual_gauge;
    left_text.push_str(&" ".repeat(width.saturating_sub(used)));
    Row { left: left_text, right: right_text, gauge }
}

/// A stable palette index (0..5) from the session name, for the session accent color.
pub fn session_color(name: &str) -> u8 {
    let hash = name.bytes().fold(2166136261u32, |acc, b| (acc ^ u32::from(b)).wrapping_mul(16777619));
    (hash % 6) as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info() -> Info<'static> {
        Info {
            model: "anthropic/claude", mode: "ask", effort: "high", context: 50_000,
            context_window: Some(200_000), cost: Some(0.25), git: Some(("main", true)),
            queue: 0, agents: 1, session: "parser fix", turn_tokens: Some((52_000, 900)), scroll: false,
        }
    }

    #[test]
    fn presets_show_different_segments_and_compact_thinking_joins_the_model() {
        let cfg = StatusLineCfg::default();
        let row = compose(&cfg, &info(), 80);
        assert!(row.left.contains("anthropic/claude"), "{}", row.left);
        assert!(!row.left.contains("thinking"), "compact thinking joins the model: {}", row.left);
        assert!(row.left.contains('◕'), "{}", row.left);
        assert!(row.right.contains("main*"), "{}", row.right);
        assert!(row.right.contains("25%"), "{}", row.right);

        let minimal = StatusLineCfg { preset: StatusPreset::Minimal, ..StatusLineCfg::default() };
        let row = compose(&minimal, &info(), 80);
        assert_eq!(row.left, "anthropic/claude");
        assert!(!row.right.contains("main"));

        let full = StatusLineCfg { preset: StatusPreset::Full, ..StatusLineCfg::default() };
        let row = compose(&full, &info(), 120);
        assert!(row.left.contains("ask") && row.left.contains('◕'));
        assert!(row.right.contains("1 sub-agent"), "{}", row.right);
        assert!(row.right.contains("$0.25") && row.right.contains("parser fix"));
    }

    #[test]
    fn context_line_embeds_the_percentage_in_the_bar() {
        let cfg = StatusLineCfg::default();
        let cells = gauge(&cfg, &info(), 21);
        assert_eq!(cells.len(), 21);
        let text: String = cells.iter().map(|&(c, _)| c).collect();
        assert!(text.contains("25%"), "{text}");
        let filled = cells.iter().filter(|&&(_, f)| f).count();
        assert!((4..=5).contains(&filled), "a quarter of 18 bar cells is filled: {filled}");
        let off = StatusLineCfg { context_line: ContextLine::Off, ..StatusLineCfg::default() };
        assert!(gauge(&off, &info(), 21).is_empty());
        let percentage = StatusLineCfg { context_line: ContextLine::Percentage, ..StatusLineCfg::default() };
        let text: String = gauge(&percentage, &info(), 10).into_iter().map(|(c, _)| c).collect();
        assert!(text.trim() == "25%", "{text:?}");
    }

    #[test]
    fn transparent_status_drops_powerline_glyphs_and_session_colors_are_stable() {
        let cfg = StatusLineCfg { preset: StatusPreset::Full, separator: StatusSeparator::Powerline, transparent: true, ..StatusLineCfg::default() };
        let row = compose(&cfg, &info(), 120);
        assert!(!row.left.contains('\u{e0b0}'), "{}", row.left);
        assert_eq!(session_color("abc"), session_color("abc"));
        assert!(session_color("abc") < 6);
    }

    #[test]
    fn a_narrow_row_drops_right_segments_before_cutting_the_model() {
        let cfg = StatusLineCfg { preset: StatusPreset::Full, ..StatusLineCfg::default() };
        let row = compose(&cfg, &info(), 24);
        assert!(row.left.starts_with("anthropic/claude"));
        assert!(row.left.chars().count() + row.right.chars().count() + row.gauge.len() <= 24);
    }
}
