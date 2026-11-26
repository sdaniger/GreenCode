//! Magic keywords: standalone lowercase prose words that append a hidden, user-attributed
//! notice for that turn (`magicKeywords.enabled`).

/// One keyword: trigger word and the notice it appends.
struct Keyword {
    word: &'static str,
    notice: &'static str,
}

const KEYWORDS: &[Keyword] = &[
    Keyword {
        word: "ultrathink",
        notice: "<system-reminder>The user asked for maximum thinking with `ultrathink`. Reason carefully and step by step before you act on this turn.</system-reminder>",
    },
    Keyword {
        word: "chillax",
        notice: "<system-reminder>The user asked to keep it light with `chillax`. Prefer the brief, direct path over exhaustive exploration.</system-reminder>",
    },
    Keyword {
        word: "orchestrate",
        notice: "<system-reminder>The user asked to `orchestrate`. Break the work into independent slices and delegate them with the `task` tool where that is faster than doing them inline.</system-reminder>",
    },
    Keyword {
        word: "jevify",
        notice: "<system-reminder>The user asked for `jevify`. Judge or classify many items at once with the `eval` tool's `judge()` helper rather than one model turn per item.</system-reminder>",
    },
    Keyword {
        word: "workflowz",
        notice: "<system-reminder>The user asked for `workflowz`. Shape the work as an explicit multi-step workflow, using the `eval` workpool or `task` batching for the steps.</system-reminder>",
    },
];

fn word_char(c: char) -> bool {
    c.is_alphanumeric() || matches!(c, '_' | '/' | '\\' | '-')
}

/// OMP's boundaries: nothing binding the keyword into an identifier, path, `a::b` or `.ext` before it,
/// and no word character, `.ext` or call parentheses after it. A sentence-ending period is fine.
fn standalone_at(before: &str, after: &str) -> bool {
    let left = !before.chars().next_back().is_some_and(|c| word_char(c) || c == '.') && !before.ends_with("::");
    let mut next = after.chars();
    let right = match next.next() {
        Some('(') => false,
        Some('.') => !next.next().is_some_and(|c| c.is_alphanumeric() || matches!(c, '_' | '-')),
        Some(c) => !word_char(c),
        None => true,
    };
    left && right
}

/// Whether `text` holds `word` as standalone lowercase prose, outside code spans and fences.
fn standalone(text: &str, word: &str) -> bool {
    let mut rest = text;
    let mut fenced = false;
    for (i, line) in text.lines().enumerate() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            fenced = !fenced;
            if i == 0 { rest = &rest[line.len().min(rest.len())..]; }
            continue;
        }
        if fenced { continue; }
        let mut from = 0;
        while let Some(at) = line[from..].find(word) {
            let start = from + at;
            let end = start + word.len();
            if standalone_at(&line[..start], &line[end..]) && !in_code_span(&line[..start]) {
                return true;
            }
            from = end;
        }
    }
    let _ = rest;
    false
}

/// Whether an odd number of backticks closed before `prefix` (i.e. the position is inline code).
fn in_code_span(prefix: &str) -> bool {
    prefix.matches('`').count() % 2 == 1
}

/// The notices to append for a prompt, in keyword order; empty when disabled or nothing matches.
pub fn notices(prompt: &str, enabled: bool) -> Vec<&'static str> {
    if !enabled {
        return Vec::new();
    }
    KEYWORDS.iter().filter(|k| standalone(prompt, k.word)).map(|k| k.notice).collect()
}

/// Appends the notices for `prompt` to a user message body.
pub fn apply(text: &mut String, prompt: &str, enabled: bool) {
    for notice in notices(prompt, enabled) {
        text.push_str("\n\n");
        text.push_str(notice);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn standalone_prose_keywords_append_notices() {
        assert_eq!(notices("please ultrathink about this", true).len(), 1);
        assert_eq!(notices("ultrathink", true).len(), 1);
        assert!(notices("ULTRATHINK this", true).is_empty());
        // Embedded in code, a path, a symbol or a longer word: not a keyword.
        assert!(notices("`ultrathink`", true).is_empty());
        assert!(notices("run ultrathink()", true).is_empty());
        assert!(notices("src/ultrathink.rs", true).is_empty());
        assert!(notices("ultrathinking", true).is_empty());
        assert!(notices("a.ultrathink", true).is_empty());
        assert!(notices("```\nultrathink\n```", true).is_empty());
        assert!(notices("ultrathink", false).is_empty());
        assert_eq!(notices("ultrathink and chillax", true).len(), 2);
    }

    #[test]
    fn apply_appends_hidden_notices_after_the_prompt() {
        let mut text = "hi chillax".to_owned();
        apply(&mut text, "hi chillax", true);
        assert!(text.starts_with("hi chillax"));
        assert!(text.contains("<system-reminder>"));
        let mut off = "hi chillax".to_owned();
        apply(&mut off, "hi chillax", false);
        assert_eq!(off, "hi chillax");
    }
}
