//! Pure pieces of the display settings: the working-label shimmer, OSC 8 hyperlinks, the bash
//! visualization box, collapsed advisor notes, pinned sub-agent rows, and the reset fireworks.

use greencode_core::config::{Hyperlinks, Motion, PinnedAgents, Shimmer};
use std::collections::BTreeMap;

/// The char range of `text` to highlight this frame; `None` freezes the label.
pub fn shimmer_at(text: &str, tick: usize, mode: Shimmer, motion: Motion) -> Option<(usize, usize)> {
    if mode == Shimmer::Disabled || motion == Motion::None {
        return None;
    }
    let chars: Vec<char> = text.chars().collect();
    if chars.is_empty() {
        return None;
    }
    let length = match mode {
        Shimmer::Classic => 1,
        Shimmer::Kitt => 3,
        Shimmer::Disabled => return None,
    }
    .min(chars.len());
    // Kitt sweeps once per second at 10 fps; classic alternates the bright head every other frame.
    let step = if mode == Shimmer::Kitt { 10 } else { 2 };
    let start = (tick / step) % chars.len();
    Some((start, (start + length).min(chars.len())))
}

/// Wrap bare URLs in OSC 8 links so terminals open them on click; `auto` skips when the terminal
/// reports a non-supporting program.
pub fn linkify(text: &str, mode: Hyperlinks) -> String {
    if mode == Hyperlinks::Off {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find("https://").or_else(|| rest.find("http://")) {
        out.push_str(&rest[..at]);
        let tail = &rest[at..];
        let end = tail.find(|c: char| c.is_whitespace() || matches!(c, ')' | ']' | '>' | '"')).unwrap_or(tail.len());
        // Sentence punctuation right after a URL ends the sentence, not the link.
        let end = tail[..end].trim_end_matches(['.', ',', ';', ':', '!', '?']).len();
        let (url, after) = tail.split_at(end);
        out.push_str(&format!("\x1b]8;;{url}\x1b\\{url}\x1b]8;;\x1b\\"));
        rest = after;
    }
    out.push_str(rest);
    out
}

/// Whether the terminal claims OSC 8 support (`auto` mode's gate).
pub fn terminal_links() -> bool {
    match std::env::var("TERM_PROGRAM").as_deref() {
        Ok("Apple_Terminal") => false,
        _ => true,
    }
}

/// A boxed bash call: the command in the top border, the first eight output lines inside.
pub fn shell_box(command: &str, output: &str, width: usize, ascii: bool) -> Vec<String> {
    let (tl, tr, bl, br, h) = if ascii { ('+', '+', '+', '+', '-') } else { ('╭', '╮', '╰', '╯', '─') };
    let inner = width.max(8) - 2;
    let command: String = command.lines().next().unwrap_or("").chars().take(inner.saturating_sub(4)).collect();
    let head = format!(" $ {command} ");
    let mut lines = vec![format!("{tl}{head}{}{tr}", h.to_string().repeat(inner.saturating_sub(head.chars().count())))];
    let body: Vec<&str> = output.lines().collect();
    let limit = body.len().min(8);
    for line in body.iter().take(limit) {
        let text: String = line.chars().take(inner - 2).collect();
        lines.push(format!("│ {text:<w$} │", w = inner - 2));
    }
    if body.len() > limit {
        lines.push(format!("│ {:<w$} │", format!("… {} more line(s)", body.len() - limit), w = inner - 2));
    }
    lines.push(format!("{bl}{}{br}", h.to_string().repeat(inner)));
    lines
}

/// A collapsed advisor card keeps its header plus `lines` notes; 0 keeps the full preview.
pub fn advisor_preview(text: &str, lines: u8) -> String {
    if lines == 0 {
        return text.to_string();
    }
    let limit = usize::from(lines);
    let all: Vec<&str> = text.lines().collect();
    if all.len() <= limit {
        return text.to_string();
    }
    let mut out = all[..limit].join("\n");
    let more = all.len() - limit;
    out.push_str(&format!("\n… {more} more note {}", if more == 1 { "line" } else { "lines" }));
    out
}

/// Pinned agent rows above the composer: name, state and — with the preview on — its last call.
pub fn pinned_rows(
    agents: &BTreeMap<String, (String, String, String)>,
    mode: PinnedAgents,
    preview: bool,
    badge: bool,
    width: usize,
) -> Vec<String> {
    let limit = match mode {
        PinnedAgents::Off => return Vec::new(),
        PinnedAgents::Collapsed => 3,
        PinnedAgents::Full => usize::MAX,
    };
    let live: Vec<(&String, &(String, String, String))> = agents.iter().filter(|(_, (state, _, _))| state == "running").collect();
    let shown = live.len().min(limit);
    let model = |spec: &String| (badge && !spec.is_empty()).then(|| format!(" [{spec}]")).unwrap_or_default();
    let mut rows: Vec<String> = live.iter().take(shown).map(|(name, (state, detail, spec))| {
        let mut row = format!("▸ {name} · {state}");
        if preview && !detail.is_empty() {
            row.push_str(&format!(" · {detail}"));
        }
        row.push_str(&model(spec));
        row.chars().take(width).collect()
    }).collect();
    if live.len() > shown {
        rows.push(format!("… {} more agent(s)", live.len() - shown));
    }
    // Finished agents stay visible until the next turn so a jump target does not vanish mid-read.
    if rows.is_empty() {
        if let Some((name, (state, detail, spec))) = agents.iter().rev().find(|(_, (state, _, _))| state == "done" || state == "failed") {
            let mut row = format!("▸ {name} · {state}");
            if preview && !detail.is_empty() {
                row.push_str(&format!(" · {detail}"));
            }
            row.push_str(&model(spec));
            rows.push(row.chars().take(width).collect());
        }
    }
    rows
}

/// Row `row` (0-based inside the pinned block) resolves to that agent's name; `None` past the rows.
pub fn pinned_hit(agents: &BTreeMap<String, (String, String, String)>, mode: PinnedAgents, row: usize) -> Option<String> {
    let limit = match mode {
        PinnedAgents::Off => return None,
        PinnedAgents::Collapsed => 3,
        PinnedAgents::Full => usize::MAX,
    };
    let live: Vec<&String> = agents.iter().filter(|(_, (state, _, _))| state == "running").map(|(name, _)| name).collect();
    let shown = live.len().min(limit);
    if row < shown {
        return live.get(row).map(|name| (*name).clone());
    }
    if live.len() > shown {
        return None;
    }
    agents.iter().rev().find(|(_, (state, _, _))| state == "done" || state == "failed").map(|(name, _)| name.clone())
}

/// One line of scaled text for `heading`: Kitty's OSC 66 at `scale`, plain text otherwise.
pub fn sized_heading(text: &str, scale: u8, kitty: bool) -> String {
    if !kitty || scale < 2 {
        return text.to_string();
    }
    let safe = text.replace('\x1b', "").replace('\x07', "");
    format!("\x1b]66;s={scale};{safe}\x1b\\")
}

/// Fireworks sparks over the top third: deterministic per tick so redraws match.
pub fn fireworks(tick: usize, width: u16, height: u16) -> Vec<(u16, u16, char)> {
    let height = (height / 3).max(3);
    let mut sparks = Vec::new();
    for burst in 0..3u16 {
        let origin_x = (u32::from(width) * (u32::from(burst) + 1) / 4) as u16;
        let origin_y = height / 2;
        for arm in 0..8u16 {
            let radius = ((tick as u16 / 2) + arm * 2 + burst * 3) % height.max(1);
            let angle = f32::from(arm) * std::f32::consts::FRAC_PI_4 + (tick % 4) as f32 * 0.08;
            let x = i32::from(origin_x) + (f32::from(radius) * angle.cos() * 2.0) as i32;
            let y = i32::from(origin_y) + (f32::from(radius) * angle.sin()) as i32;
            if x >= 0 && y >= 0 && x < i32::from(width) && y < i32::from(height) {
                sparks.push((x as u16, y as u16, ['*', '✶', '✦', '·'][(tick + usize::from(arm)) % 4]));
            }
        }
    }
    sparks
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shimmer_sweeps_and_freezes() {
        let text = "Working";
        let first = shimmer_at(text, 0, Shimmer::Kitt, Motion::Full).unwrap();
        let later = shimmer_at(text, 20, Shimmer::Kitt, Motion::Full).unwrap();
        assert_ne!(first, later);
        assert!(first.1 <= text.chars().count());
        assert_eq!(shimmer_at(text, 0, Shimmer::Disabled, Motion::Full), None);
        assert_eq!(shimmer_at(text, 0, Shimmer::Classic, Motion::None), None);
    }

    #[test]
    fn hyperlinks_wrap_urls_and_stop_at_punctuation() {
        let out = linkify("see https://example.com/a and http://b.test/c.", Hyperlinks::Auto);
        assert!(out.contains("\x1b]8;;https://example.com/a\x1b\\"), "{out:?}");
        assert!(out.contains("\x1b]8;;http://b.test/c\x1b\\http://b.test/c\x1b]8;;"), "{out:?}");
        assert_eq!(linkify("https://x", Hyperlinks::Off), "https://x");
    }

    #[test]
    fn shell_box_and_advisor_preview_cut_long_text() {
        let boxed = shell_box("cargo test --lib", "a\nb\nc\nd\ne\nf\ng\nh\ni", 40, true);
        assert!(boxed[0].contains("$ cargo test --lib"), "{boxed:?}");
        assert!(boxed.iter().any(|line| line.contains("1 more line")), "{boxed:?}");
        assert!(boxed.len() < 12);
        let notes = advisor_preview("one\ntwo\nthree\nfour", 2);
        assert!(notes.starts_with("one\ntwo"), "{notes}");
        assert!(notes.contains("2 more note line"), "{notes}");
        assert_eq!(advisor_preview("one\ntwo", 0), "one\ntwo");
    }

    #[test]
    fn sized_heading_emits_kitty_osc66_only_when_enabled() {
        let scaled = sized_heading("# Title", 2, true);
        assert!(scaled.starts_with("\x1b]66;s=2;"), "{scaled:?}");
        assert!(scaled.ends_with("\x1b\\"), "{scaled:?}");
        assert!(scaled.contains("# Title"));
        assert_eq!(sized_heading("# Title", 2, false), "# Title");
        assert_eq!(sized_heading("# Title", 1, true), "# Title");
    }

    #[test]
    fn pinned_hit_maps_rows_to_agent_names() {
        let mut agents = BTreeMap::new();
        for name in ["a", "b", "c", "d"] {
            agents.insert(name.to_string(), ("running".to_string(), String::new(), String::new()));
        }
        assert_eq!(pinned_hit(&agents, PinnedAgents::Off, 0), None);
        assert_eq!(pinned_hit(&agents, PinnedAgents::Collapsed, 0).as_deref(), Some("a"));
        assert_eq!(pinned_hit(&agents, PinnedAgents::Collapsed, 2).as_deref(), Some("c"));
        // Row 3 is the "1 more agent(s)" summary row, not an agent.
        assert_eq!(pinned_hit(&agents, PinnedAgents::Collapsed, 3), None);
        assert_eq!(pinned_hit(&agents, PinnedAgents::Full, 3).as_deref(), Some("d"));
    }

    #[test]
    fn pinned_rows_respect_mode_and_preview_and_fireworks_stay_inside() {
        let mut agents = BTreeMap::new();
        for name in ["a", "b", "c", "d"] {
            agents.insert(name.to_string(), ("running".to_string(), "grep".to_string(), String::new()));
        }
        assert_eq!(pinned_rows(&agents, PinnedAgents::Off, true, true, 40).len(), 0);
        let collapsed = pinned_rows(&agents, PinnedAgents::Collapsed, true, true, 40);
        assert_eq!(collapsed.len(), 4, "{collapsed:?}");
        assert!(collapsed[0].contains("grep"));
        assert_eq!(pinned_rows(&agents, PinnedAgents::Full, false, true, 40).len(), 4);
        assert!(!pinned_rows(&agents, PinnedAgents::Full, false, true, 40)[0].contains("grep"));

        // `task.showResolvedModelBadge`: the resolved model rides on the subagent event.
        agents.get_mut("a").unwrap().2 = "openai/gpt-6".into();
        let badge = pinned_rows(&agents, PinnedAgents::Collapsed, false, true, 60);
        assert!(badge.iter().any(|row| row.contains("openai/gpt-6")), "{badge:?}");
        let plain = pinned_rows(&agents, PinnedAgents::Collapsed, false, false, 60);
        assert!(!plain.iter().any(|row| row.contains("openai/gpt-6")), "{plain:?}");
        let sparks = fireworks(7, 40, 12);
        assert!(!sparks.is_empty());
        assert!(sparks.iter().all(|&(x, y, _)| x < 40 && y < 12));
    }
}
