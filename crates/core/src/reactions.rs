//! Emoji reactions on the user's message: the model may lead its reply with `[react: 🎉]`
//! (or an HTML comment) and the transcript shows that badge on the user bubble.

/// The first reaction marker in a reply, and the reply with the marker line removed.
pub fn extract(reply: &str) -> (Option<String>, String) {
    let mut found = None;
    let mut kept = String::with_capacity(reply.len());
    for line in reply.split_inclusive('\n') {
        match marker(line).filter(|(_, _, badge)| badge.chars().count() <= 8) {
            Some((start, end, badge)) if found.is_none() => {
                found = Some(badge.to_string());
                // A marker alone on its line disappears; inline markers keep the rest of the line.
                let (before, after) = (&line[..start], &line[end..]);
                if before.trim().is_empty() && after.trim().is_empty() {
                    continue;
                }
                kept.push_str(before);
                kept.push_str(after);
            }
            _ => kept.push_str(line),
        }
    }
    (found, kept.trim_start_matches('\n').to_string())
}

/// The first `react:` marker span in `line` and its badge text.
fn marker(line: &str) -> Option<(usize, usize, &str)> {
    let bracket = line.find("[react:").and_then(|start| {
        let end = start + line[start..].find(']')? + 1;
        Some((start, end))
    });
    let comment = line.find("<!--").and_then(|start| {
        let end = start + 4 + line[start + 4..].find("-->")? + 3;
        let badge = line[start + 4..end - 3].trim().strip_prefix("react:");
        badge.map(|_| (start, end))
    });
    let (start, end) = [bracket, comment].into_iter().flatten().min_by_key(|&(start, _)| start)?;
    let badge = line[start..end].trim_matches(|c| matches!(c, '[' | ']'));
    let badge = badge.strip_prefix("<!--").unwrap_or(badge);
    let badge = badge.strip_suffix("-->").unwrap_or(badge);
    let badge = badge.trim().strip_prefix("react:")?.trim();
    (!badge.is_empty()).then_some((start, end, badge))
}

/// One line in the system prompt while reactions are enabled.
pub const PROMPT: &str = "When a single emoji captures your reaction to the user's message, start your reply with `[react: <emoji>]`; the terminal shows it as a badge on their message and removes the line.";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn marker_line_is_removed_and_inline_comment_keeps_the_text() {
        let (badge, text) = extract("[react: 🎉]\nDone.\n");
        assert_eq!(badge.as_deref(), Some("🎉"));
        assert_eq!(text, "Done.\n");
        let (badge, text) = extract("<!-- react: 🐛 -->Fix landed\nnext");
        assert_eq!(badge.as_deref(), Some("🐛"));
        assert_eq!(text, "Fix landed\nnext");
        assert_eq!(extract("plain reply").0, None);
        assert_eq!(extract("[react: x]\nfirst\n[react: y]\n").0.as_deref(), Some("x"));
    }
}
