//! `title.icons`: the icon and short project code that head a generated session title.

use crate::config::TitleIcons;
use std::path::Path;

/// Prefix a generated title with its icon and short code; user-set titles pass through untouched.
pub fn head(policy: TitleIcons, cwd: &Path, title: &str) -> String {
    let title = title.trim();
    if title.is_empty() || policy == TitleIcons::Boring {
        return title.to_string();
    }
    let Some((icon, code)) = icon(policy, cwd, title) else { return title.to_string() };
    if title.starts_with(&format!("{icon} {code} ")) || title.starts_with(&format!("{icon} ")) {
        return title.to_string();
    }
    format!("{icon} {code} {title}")
}

/// The icon and up-to-three-letter project code, or `None` for an unchanged title.
fn icon(policy: TitleIcons, cwd: &Path, title: &str) -> Option<(String, String)> {
    let code = code(cwd, title);
    let hash = title.bytes().fold(0u32, |acc, b| acc.wrapping_mul(31).wrapping_add(u32::from(b)));
    let emoji = ["🛠", "🧭", "🧩", "🌱", "⚡", "🔭"][(hash as usize) % 6];
    let nerd = ["\u{f121}", "\u{f0eb}", "\u{f085}", "\u{f1b2}", "\u{f0e7}", "\u{f06e}"][(hash as usize) % 6];
    Some(match policy {
        TitleIcons::NfEmoji => (format!("{nerd} {emoji}"), code),
        TitleIcons::Emoji => (emoji.to_string(), code),
        TitleIcons::Boring => return None,
    })
}

/// The repository or directory name, shortened to its initials (at most three capitals).
fn code(cwd: &Path, title: &str) -> String {
    let name = cwd.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let name = if name == title { String::new() } else { name };
    let initials: String = name.split(['-', '_', '.', ' ']).filter(|part| !part.is_empty()).filter_map(|part| part.chars().next()).collect();
    if initials.chars().count() >= 2 {
        return initials.chars().take(3).collect::<String>().to_uppercase();
    }
    name.chars().filter(|c| c.is_alphanumeric()).take(3).collect::<String>().to_uppercase()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn policies_add_icons_and_codes_without_doubling() {
        let cwd = PathBuf::from("/tmp/green-code");
        let emoji = head(TitleIcons::Emoji, &cwd, "Fix parser");
        assert!(emoji.ends_with(" GC Fix parser"), "{emoji}");
        assert_eq!(head(TitleIcons::Emoji, &cwd, &emoji), emoji);
        let nerd = head(TitleIcons::NfEmoji, &cwd, "Fix parser");
        assert!(nerd.contains("GC Fix parser"), "{nerd}");
        assert_ne!(emoji, nerd);
        assert_eq!(head(TitleIcons::Boring, &cwd, "Fix parser"), "Fix parser");
    }
}
