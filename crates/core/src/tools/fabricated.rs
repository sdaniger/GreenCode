//! Fabricated tool results: some models write the tool result inline while still emitting in-band tool calls.
//! With `abort_on_fabricated_result` the stream is stopped the moment the marker appears; without it the
//! fabricated continuation is cut from the text block and the real tool results follow.

use serde_json::Value;

/// Markers a model writes when it starts inventing a tool result: ReAct's `Observation:`, XML-style
/// `<tool_result>`, or a raw wire object. Deliberately narrow: ordinary prose may say "Result:".
const MARKERS: &[&str] = &[
    "\nObservation:",
    "\nObservation ",
    "<tool_result>",
    "\nToolResult {",
    "\n\"type\": \"tool_result\"",
    "\n{\"type\":\"tool_result\"",
];

/// Byte offset where a fabricated tool result starts in `text`.
pub fn marker_at(text: &str) -> Option<usize> {
    MARKERS
        .iter()
        .filter_map(|marker| text.find(marker))
        .min()
}

/// Removes a fabricated continuation and anything after it; `true` when something was cut.
pub fn strip(text: &mut String) -> bool {
    match marker_at(text) {
        Some(at) => {
            text.truncate(at);
            text.truncate(text.trim_end().len());
            true
        }
        None => false,
    }
}

/// Applies `strip` to every text block of a reply. Returns whether anything was removed.
/// The tool calls stay: their real results still run, so the fabricated continuation is simply ignored.
pub fn strip_blocks(blocks: &mut Vec<crate::Block>) -> bool {
    let mut cut = false;
    for block in blocks.iter_mut() {
        if let crate::Block::Text { text } = block {
            cut |= strip(text);
        }
    }
    cut
}

/// The model-visible notice after a fabricated result was removed; JSON-ish shapes name the tool.
pub fn notice(cut_at: &str) -> String {
    format!(
        "[The model wrote a tool result itself (starting at `{cut_at}`); that continuation was discarded. Emit tool calls and wait for their real results.]"
    )
}

/// Keeps the provider payload free of the extra `input_type`-style keys a fabricated object may carry.
pub fn is_fabricated_object(value: &Value) -> bool {
    value["type"] == "tool_result" || value["type"] == "tool_use_result"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detection_finds_react_and_wire_markers_only() {
        // The marker includes its leading newline, which avoids matching mid-sentence prose.
        assert_eq!(marker_at("Let me look.\nObservation: it works"), Some(12));
        assert_eq!(marker_at("text <tool_result>{\"ok\":true}</tool_result>"), Some(5));
        assert_eq!(marker_at("The Result: 42, as expected."), None);
        assert_eq!(marker_at("plain answer with no fabrication"), None);
    }

    #[test]
    fn strip_cuts_the_continuation_and_trailing_space() {
        let mut text = "I'll check the file.\nObservation: the file contains 42".into();
        assert!(strip(&mut text));
        assert_eq!(text, "I'll check the file.");
        let mut clean = "just an answer".to_string();
        assert!(!strip(&mut clean));
        assert_eq!(clean, "just an answer");
    }

    #[test]
    fn strip_blocks_keeps_real_tool_calls() {
        let mut blocks = vec![
            crate::Block::Text { text: "Looking.\nObservation: fake".into() },
            crate::Block::ToolUse { id: "t1".into(), name: "read".into(), input: serde_json::json!({"path": "a"}) },
        ];
        assert!(strip_blocks(&mut blocks));
        assert!(matches!(&blocks[0], crate::Block::Text { text } if text == "Looking."));
        assert_eq!(blocks.len(), 2);
        assert!(is_fabricated_object(&serde_json::json!({"type": "tool_result"})));
        assert!(!is_fabricated_object(&serde_json::json!({"type": "text"})));
    }
}
