//! `/btw` steering injects (`btw.inject`): a side answer may draft a prompt in `<inject>` tags
//! that the operator sends to the main session.

/// Instruction appended to a `/btw` question so the answer drafts a steering prompt.
pub const DRAFT_INSTRUCTION: &str = "Also draft, in one short paragraph, the exact prompt the main session should receive, wrapped in <inject> and </inject> tags.";

/// The text of the first `<inject>` block in `answer`, trimmed; `None` when the setting is off or
/// the answer has no block.
pub fn inject_block(answer: &str, enabled: bool) -> Option<String> {
    if !enabled {
        return None;
    }
    let start = answer.find("<inject>")? + "<inject>".len();
    let end = start + answer[start..].find("</inject>")?;
    let text = answer[start..end].trim();
    (!text.is_empty()).then(|| text.to_owned())
}

/// `answer` with the `<inject>` blocks removed, for the transcript view.
pub fn without_inject(answer: &str, enabled: bool) -> String {
    if !enabled {
        return answer.to_owned();
    }
    let mut out = String::with_capacity(answer.len());
    let mut rest = answer;
    while let Some(at) = rest.find("<inject>") {
        let Some(end) = rest[at..].find("</inject>").map(|i| at + i + "</inject>".len()) else { break };
        out.push_str(rest[..at].trim_end());
        rest = rest[end..].trim_start();
        if !out.is_empty() && !rest.is_empty() {
            out.push('\n');
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inject_blocks_are_extracted_only_when_enabled() {
        let answer = "Here is a steer.\n<inject>Fix the failing test first.</inject>\n";
        assert_eq!(inject_block(answer, true).as_deref(), Some("Fix the failing test first."));
        assert_eq!(inject_block(answer, false), None);
        assert_eq!(inject_block("no block here", true), None);
        assert_eq!(inject_block("<inject>  </inject>", true), None);
        assert_eq!(without_inject(answer, true), "Here is a steer.");
        assert_eq!(without_inject(answer, false), answer);
    }

    #[test]
    fn only_the_first_inject_block_is_sent() {
        let answer = "<inject>one</inject> middle <inject>two</inject>";
        assert_eq!(inject_block(answer, true).as_deref(), Some("one"));
        assert_eq!(without_inject(answer, true), "middle");
    }
}
