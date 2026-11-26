//! Structural summaries for `read` without a selector: elide brace-delimited bodies, long block comments and
//! prose paragraphs, then BFS-unfold spans until the summary is readable.
//!
//! shortcut: spans come from a character-level scan, not a real parse — a multiline string literal is never
//! elided and a file with unbalanced braces elides less, never more; upgrade if OMP's tree-sitter summarizer
//! is ported.
use crate::config::files::ReadCfg;
use std::path::Path;

/// Mirrors OMP's cap: bigger files fall back to a plain window.
const MAX_BYTES: usize = 2 * 1024 * 1024;
const MAX_LINES: usize = 20_000;

pub(crate) struct Summary {
    pub body: String,
    /// 1-based lines shown verbatim; `edit.enforce_seen_lines` records these, not the hidden ones.
    pub kept: Vec<usize>,
}

/// Elidable inner lines, 1-based inclusive.
type Span = (usize, usize);

struct Node {
    start: usize,
    end: usize,
    children: Vec<usize>,
}

/// A structural summary of `text`, or `None` when the file is too small/large, has nothing to elide, or is
/// prose while `read.summarize.prose` is off.
pub(crate) fn try_summary(text: &str, path: &str, read: &ReadCfg) -> Option<Summary> {
    let cfg = &read.summarize;
    if !cfg.enabled || text.len() > MAX_BYTES {
        return None;
    }
    let lines: Vec<&str> = text.lines().collect();
    if lines.len() > MAX_LINES || lines.len() < cfg.min_total_lines.max(1) {
        return None;
    }
    let prose = is_prose(path);
    if prose && !cfg.prose {
        return None;
    }
    let (min_body, min_comment) = (cfg.min_body_lines.max(1), cfg.min_comment_lines.max(1));
    let mut spans = if prose {
        prose_spans(&lines, min_body)
    } else if indented(path) {
        indent_spans(&lines, min_body)
    } else if coded(path) {
        code_spans(&lines, min_body, min_comment)
    } else {
        return None;
    };
    if spans.is_empty() {
        return None;
    }
    spans.sort_unstable_by(|a, b| a.0.cmp(&b.0).then(b.1.cmp(&a.1)));
    let (nodes, roots) = tree(&mut spans);
    let mut open = vec![false; nodes.len()];
    let mut queue: std::collections::VecDeque<usize> = roots.iter().copied().collect();
    let mut visible = summary_lines(&lines, &folded_mask(lines.len(), &nodes, &open));
    while cfg.unfold_until > 0 && visible < cfg.unfold_until as usize {
        let Some(id) = queue.pop_front() else { break };
        open[id] = true;
        let mask = folded_mask(lines.len(), &nodes, &open);
        let shown = summary_lines(&lines, &mask);
        if shown > cfg.unfold_limit as usize {
            // This span reveals too much: keep it folded and continue with the remaining spans.
            open[id] = false;
        } else {
            visible = shown;
            queue.extend(nodes[id].children.iter().copied());
        }
    }
    let (body, kept, ranges, elided) = render(&lines, &folded_mask(lines.len(), &nodes, &open), read.line_numbers);
    let first = ranges.first()?;
    let mut footer = format!("\n\n[…{elided} lines elided; re-read with offset {}, limit {}", first.0, first.1 - first.0 + 1);
    if ranges.len() > 1 {
        footer.push_str(&format!(" (or any of {} ranges)", ranges.len()));
    }
    footer.push(']');
    Some(Summary { body: format!("{body}{footer}"), kept })
}

/// Markdown flavors and plain text only make sense as prose.
fn is_prose(path: &str) -> bool {
    matches!(ext(path).as_str(), "md" | "markdown" | "mdx" | "txt")
}

/// Languages whose bodies are indentation, not delimiters.
fn indented(path: &str) -> bool {
    matches!(ext(path).as_str(), "py" | "pyi")
}

/// Extensions OMP's tree-sitter summarizer covers. Anything else (JSON, logs, CSV) is read verbatim, so a
/// data file never loses lines to a structural summary.
fn coded(path: &str) -> bool {
    matches!(
        ext(path).as_str(),
        "c" | "h"
            | "cc"
            | "cpp"
            | "cxx"
            | "hpp"
            | "hh"
            | "cs"
            | "java"
            | "kt"
            | "kts"
            | "swift"
            | "m"
            | "mm"
            | "go"
            | "rs"
            | "ts"
            | "tsx"
            | "js"
            | "jsx"
            | "mjs"
            | "cjs"
            | "php"
            | "rb"
            | "scala"
            | "scss"
            | "css"
            | "lua"
            | "sh"
            | "bash"
            | "zsh"
            | "sql"
            | "proto"
            | "dart"
    )
}

fn ext(path: &str) -> String {
    Path::new(path).extension().and_then(|e| e.to_str()).unwrap_or_default().to_ascii_lowercase()
}

/// Brace/paren/bracket bodies and multiline block comments, keyed by their inner line range.
fn code_spans(lines: &[&str], min_body: usize, min_comment: usize) -> Vec<Span> {
    let mut spans = Vec::new();
    // (opener, line it opened on)
    let mut stack: Vec<(char, usize)> = Vec::new();
    let mut comment: Option<usize> = None;
    for (i, line) in lines.iter().enumerate() {
        let ln = i + 1;
        let chars: Vec<char> = line.chars().collect();
        let mut k = 0;
        while k < chars.len() {
            let c = chars[k];
            if let Some(start) = comment {
                if c == '*' && chars.get(k + 1) == Some(&'/') {
                    // Total comment lines, opener and closer included.
                    if ln > start + 1 && ln - start + 1 >= min_comment {
                        spans.push((start + 1, ln - 1));
                    }
                    comment = None;
                    k += 2;
                } else {
                    k += 1;
                }
                continue;
            }
            match c {
                '/' if chars.get(k + 1) == Some(&'/') => break,
                '/' if chars.get(k + 1) == Some(&'*') => {
                    comment = Some(ln);
                    k += 2;
                }
                '"' | '`' => {
                    k += 1;
                    while k < chars.len() && chars[k] != c {
                        if chars[k] == '\\' {
                            k += 1;
                        }
                        k += 1;
                    }
                    k += 1;
                }
                // A char literal, not a lifetime: `'x'` or `'\n'`.
                '\'' => {
                    if chars.get(k + 2) == Some(&'\'') {
                        k += 3;
                    } else if chars.get(k + 1) == Some(&'\\') && chars.get(k + 3) == Some(&'\'') {
                        k += 4;
                    } else {
                        k += 1;
                    }
                }
                '{' | '(' | '[' => {
                    stack.push((c, ln));
                    k += 1;
                }
                '}' | ')' | ']' => {
                    let opener = match c {
                        '}' => '{',
                        ')' => '(',
                        _ => '[',
                    };
                    if let Some(pos) = stack.iter().rposition(|&(o, _)| o == opener) {
                        let (_, open_line) = stack.remove(pos);
                        // The node spans opener through closer; OMP compares that total to minBodyLines.
                        if ln > open_line + 1 && ln - open_line + 1 >= min_body {
                            spans.push((open_line + 1, ln - 1));
                        }
                    }
                    k += 1;
                }
                _ => k += 1,
            }
        }
        // shortcut: a string left open at the end of a line counts as closed, so an unterminated literal
        // cannot hide the rest of the file; upgrade if a real lexer lands.
    }
    spans
}

/// Python/YAML bodies: every line that opens a deeper-indented run, nested. Linear in file size.
fn indent_spans(lines: &[&str], min_body: usize) -> Vec<Span> {
    let mut spans = Vec::new();
    // (indent, head line) for the blocks still open at the current line.
    let mut stack: Vec<(usize, usize)> = Vec::new();
    let mut prev = 0;
    for (i, raw) in lines.iter().enumerate() {
        let trimmed = raw.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let ln = i + 1;
        let ind = indent(raw);
        while let Some(&(head_indent, head)) = stack.last() {
            if head_indent >= ind {
                stack.pop();
                if prev > head && prev - head + 1 >= min_body {
                    spans.push((head + 1, prev));
                }
            } else {
                break;
            }
        }
        stack.push((ind, ln));
        prev = ln;
    }
    while let Some((_, head)) = stack.pop() {
        if prev > head && prev - head + 1 >= min_body {
            spans.push((head + 1, prev));
        }
    }
    spans
}

fn indent(line: &str) -> usize {
    line.len() - line.trim_start().len()
}

/// Markdown headings and prose paragraphs keep their first line; fenced code keeps its fence lines.
fn prose_spans(lines: &[&str], min_body: usize) -> Vec<Span> {
    let mut spans = Vec::new();
    let mut fence: Option<usize> = None;
    let mut paragraph: Vec<usize> = Vec::new();
    fn flush(paragraph: &mut Vec<usize>, spans: &mut Vec<Span>, min_body: usize) {
        if paragraph.len() >= min_body && paragraph.len() > 1 {
            spans.push((paragraph[1], *paragraph.last().unwrap()));
        }
        paragraph.clear();
    }
    for (i, line) in lines.iter().enumerate() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            match fence.take() {
                None => {
                    flush(&mut paragraph, &mut spans, min_body);
                    fence = Some(i + 1);
                }
                Some(open) => {
                    if i + 1 > open + 1 && (i + 1) - open + 1 >= min_body {
                        spans.push((open + 1, i));
                    }
                }
            }
            continue;
        }
        if fence.is_some() {
            continue;
        }
        if line.trim().is_empty() || trimmed.starts_with('#') {
            flush(&mut paragraph, &mut spans, min_body);
            continue;
        }
        paragraph.push(i + 1);
    }
    flush(&mut paragraph, &mut spans, min_body);
    spans
}

/// Nests equal-or-contained spans; returns nodes plus the outermost ids.
fn tree(spans: &mut Vec<Span>) -> (Vec<Node>, Vec<usize>) {
    let mut nodes: Vec<Node> = Vec::new();
    let mut roots = Vec::new();
    let mut stack: Vec<usize> = Vec::new();
    for (start, end) in spans.drain(..) {
        while let Some(&top) = stack.last() {
            if nodes[top].end < end {
                stack.pop();
            } else {
                break;
            }
        }
        if let Some(&top) = stack.last() {
            if nodes[top].start == start && nodes[top].end == end {
                continue;
            }
        }
        nodes.push(Node { start, end, children: Vec::new() });
        let id = nodes.len() - 1;
        match stack.last() {
            Some(&parent) => nodes[parent].children.push(id),
            None => roots.push(id),
        }
        stack.push(id);
    }
    (nodes, roots)
}

/// Which lines are hidden; a folded node hides its whole inner range, children included.
fn folded_mask(total: usize, nodes: &[Node], open: &[bool]) -> Vec<bool> {
    let mut elided = vec![false; total + 1];
    for (id, node) in nodes.iter().enumerate() {
        if !open[id] {
            elided[node.start..=node.end].fill(true);
        }
    }
    elided
}

fn summary_lines(lines: &[&str], elided: &[bool]) -> usize {
    units(lines, elided).len()
}

#[derive(Clone, Copy)]
enum Unit {
    Line(usize),
    Elided(usize, usize),
    Merged(usize, usize),
}

/// One unit per output line: kept lines, elided runs, and brace pairs merged into `head … tail`.
fn units(lines: &[&str], elided: &[bool]) -> Vec<Unit> {
    let total = lines.len();
    let mut units = Vec::new();
    let mut i = 1;
    while i <= total {
        if elided[i] {
            let start = i;
            while i <= total && elided[i] {
                i += 1;
            }
            units.push(Unit::Elided(start, i - 1));
        } else {
            units.push(Unit::Line(i));
            i += 1;
        }
    }
    let mut merged: Vec<Unit> = Vec::with_capacity(units.len());
    let mut k = 0;
    while k < units.len() {
        if let Unit::Elided(start, end) = units[k] {
            if k > 0 && k + 1 < units.len() {
                if let (Some(Unit::Line(head)), Some(Unit::Line(tail))) = (merged.last(), units.get(k + 1)) {
                    let (head, tail) = (*head, *tail);
                    if brace_pair(lines[head - 1], lines[tail - 1]) {
                        merged.pop();
                        merged.push(Unit::Merged(head, tail));
                        k += 2;
                        continue;
                    }
                }
            }
            merged.push(Unit::Elided(start, end));
        } else {
            merged.push(units[k]);
        }
        k += 1;
    }
    merged
}

/// Renders the visible lines, reporting kept line numbers, elided ranges and how many lines were hidden.
fn render(lines: &[&str], elided: &[bool], numbered: bool) -> (String, Vec<usize>, Vec<Span>, usize) {
    let mut out = String::new();
    let mut kept = Vec::new();
    let mut ranges = Vec::new();
    let mut hidden = 0;
    for unit in units(lines, elided) {
        match unit {
            Unit::Line(n) => {
                if numbered {
                    out.push_str(&format!("{n}:"));
                }
                out.push_str(lines[n - 1].trim_end());
                out.push('\n');
                kept.push(n);
            }
            Unit::Elided(start, end) => {
                out.push_str("…\n");
                ranges.push((start, end));
                hidden += end - start + 1;
            }
            Unit::Merged(start, end) => {
                if numbered {
                    out.push_str(&format!("{start}-{end}:"));
                }
                out.push_str(lines[start - 1].trim_end());
                out.push_str(" … ");
                out.push_str(lines[end - 1].trim());
                out.push('\n');
                kept.extend([start, end]);
                // Offer the hidden interior; a single hidden line is its own range.
                ranges.push(if end >= start + 2 { (start + 1, end - 1) } else { (start, end) });
                hidden += end - start - 1;
            }
        }
    }
    out.pop();
    (out, kept, ranges, hidden)
}

/// Whether an elided span can collapse to `head … tail`: a brace/bracket opener and its matching closer.
fn brace_pair(head: &str, tail: &str) -> bool {
    let head = head.trim_end();
    let closer = match head.chars().last() {
        Some('{') => '}',
        Some('(') => ')',
        Some('[') => ']',
        _ => return false,
    };
    tail.trim()
        .strip_prefix(closer)
        .is_some_and(|rest| rest.chars().all(|c| matches!(c, ';' | ',' | ')' | ']' | '}')))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::files::{ReadCfg, SummarizeCfg};

    fn read(summarize: SummarizeCfg) -> ReadCfg {
        ReadCfg { summarize, ..Default::default() }
    }

    /// A ten-statement body, a two-line body and a tail call.
    fn fixture() -> String {
        let body = (1..=10).map(|i| format!("    let v{i} = {i};")).collect::<Vec<_>>().join("\n");
        format!("fn big() {{\n{body}\n}}\n\nfn small() {{\n    one();\n}}\n\nfn main() {{\n    big();\n}}\n")
    }

    #[test]
    fn long_bodies_collapse_to_one_brace_line() {
        let text = fixture();
        let summary =
            try_summary(&text, "a.rs", &read(SummarizeCfg { min_total_lines: 1, unfold_until: 0, ..Default::default() })).unwrap();
        assert!(summary.body.starts_with("fn big() { … }"), "{}", summary.body);
        // Two-line bodies are below minBodyLines and stay verbatim.
        assert!(summary.body.contains("fn small() {\n    one();\n}"), "{}", summary.body);
        assert!(summary.body.contains("lines elided; re-read with offset 2, limit 10"), "{}", summary.body);
        assert_eq!(summary.kept, vec![1, 12, 13, 14, 15, 16, 17, 18, 19, 20]);
    }

    #[test]
    fn short_files_and_nothing_to_elide_are_read_verbatim() {
        let text = fixture();
        assert!(try_summary(&text, "a.rs", &read(SummarizeCfg { min_total_lines: 100, ..Default::default() })).is_none());
        let tiny = "fn f() {\n    one();\n}\n";
        assert!(try_summary(tiny, "a.rs", &read(SummarizeCfg { min_total_lines: 1, ..Default::default() })).is_none());
        // Braces inside strings, char literals and lifetimes do not open a span.
        let literal = "fn f<'a>(s: &'a str) -> String {\n    let q = \"}\";\n    let c = '}';\n    format!(\"{s}{q}{c}\")\n}\n";
        assert!(try_summary(literal, "a.rs", &read(SummarizeCfg { min_total_lines: 1, min_body_lines: 6, ..Default::default() })).is_none());
    }

    #[test]
    fn min_body_lines_controls_which_spans_collapse() {
        let text = fixture();
        let high = read(SummarizeCfg { min_total_lines: 1, min_body_lines: 20, ..Default::default() });
        assert!(try_summary(&text, "a.rs", &high).is_none());
    }

    #[test]
    fn block_comments_collapse_at_min_comment_lines() {
        let body = (1..=6).map(|i| format!(" * line {i}")).collect::<Vec<_>>().join("\n");
        let text = format!("/*\n{body}\n */\nfn f() {{\n    one();\n}}\n");
        let summary = try_summary(
            &text,
            "a.c",
            &read(SummarizeCfg { min_total_lines: 1, min_body_lines: 20, unfold_until: 0, ..Default::default() }),
        )
        .unwrap();
        assert!(summary.body.starts_with("/*\n…\n */"), "{}", summary.body);
        let short = "/*\n * one\n */\nfn f() {\n    one();\n}\n";
        assert!(try_summary(short, "a.c", &read(SummarizeCfg { min_total_lines: 1, min_body_lines: 20, ..Default::default() })).is_none());
    }

    #[test]
    fn unfold_until_zero_keeps_only_outer_elisions() {
        let inner = (1..=6).map(|i| format!("        step{i}();")).collect::<Vec<_>>().join("\n");
        let text = format!("fn outer() {{\n    setup();\n    if ready {{\n{inner}\n    }}\n    finish();\n}}\n");
        let folded = try_summary(&text, "a.rs", &read(SummarizeCfg { min_total_lines: 1, unfold_until: 0, ..Default::default() }))
            .unwrap();
        assert!(folded.body.starts_with("fn outer() { … }"), "{}", folded.body);
        // Target 5 lines unfolds the outer body, whose 10-line reveal blows the 6-line ceiling, so the inner
        // `if` stays folded and unfolding stops.
        let unfolded = try_summary(
            &text,
            "a.rs",
            &read(SummarizeCfg { min_total_lines: 1, unfold_until: 5, unfold_limit: 6, ..Default::default() }),
        )
        .unwrap();
        assert!(unfolded.body.contains("    setup();"), "{}", unfolded.body);
        assert!(unfolded.body.contains("if ready { … }"), "{}", unfolded.body);
        assert!(unfolded.body.contains("    finish();"), "{}", unfolded.body);
    }

    #[test]
    fn unfold_limit_skips_the_too_large_span_and_continues() {
        let wide = (1..=30).map(|i| format!("    let w{i} = {i};")).collect::<Vec<_>>().join("\n");
        let narrow = (1..=4).map(|i| format!("    let n{i} = {i};")).collect::<Vec<_>>().join("\n");
        let text = format!("fn wide() {{\n{wide}\n}}\n\nfn narrow() {{\n{narrow}\n}}\n\nfn tail() {{\n    end();\n}}\n");
        let summary = try_summary(
            &text,
            "a.rs",
            &read(SummarizeCfg { min_total_lines: 1, unfold_until: 12, unfold_limit: 20, ..Default::default() }),
        )
        .unwrap();
        // 8 visible lines by default: wide (33 lines) blows the 20-line ceiling, narrow (8) fits.
        assert!(summary.body.starts_with("fn wide() { … }"), "{}", summary.body);
        assert!(summary.body.contains("fn narrow() {\n"), "{}", summary.body);
        assert!(summary.body.contains("    let n1 = 1;"), "{}", summary.body);
        // A zero ceiling unfolds nothing.
        let none = try_summary(&text, "a.rs", &read(SummarizeCfg { min_total_lines: 1, unfold_until: 99, unfold_limit: 0, ..Default::default() }))
            .unwrap();
        assert_eq!(visible_lines_of(&none.body), 8, "{}", none.body);
    }

    fn visible_lines_of(body: &str) -> usize {
        body.lines().take_while(|l| !l.starts_with("[…")).filter(|l| l.trim() != "…").count()
    }

    #[test]
    fn unfolding_every_span_yields_the_whole_file() {
        let text = fixture();
        let summary = try_summary(&text, "a.rs", &read(SummarizeCfg { min_total_lines: 1, unfold_until: 100, ..Default::default() }));
        assert!(summary.is_none());
    }

    const RUST_FIXTURE: &str = include_str!("../../tests/fixtures/read_summary/sample.rs");
    const PY_FIXTURE: &str = include_str!("../../tests/fixtures/read_summary/sample.py");
    const MD_FIXTURE: &str = include_str!("../../tests/fixtures/read_summary/sample.md");

    fn fixture_cfg(summarize: SummarizeCfg) -> ReadCfg {
        ReadCfg { summarize: SummarizeCfg { min_total_lines: 1, ..summarize }, ..Default::default() }
    }

    #[test]
    fn fixture_rust_folds_long_and_nested_bodies_but_not_two_liners() {
        let summary = try_summary(RUST_FIXTURE, "sample.rs", &fixture_cfg(SummarizeCfg { unfold_until: 0, ..Default::default() }))
            .unwrap();
        assert!(summary.body.contains("pub fn pipeline() { … }"), "{}", summary.body);
        assert!(summary.body.contains("fn nested(ready: bool) { … }"), "{}", summary.body);
        assert!(summary.body.contains("fn small() {\n    one();\n}"), "{}", summary.body);
        assert!(summary.body.contains("fn trivial_5() {\n    done(5);\n}"), "{}", summary.body);
        assert!(!summary.body.contains("let stage_1"), "{}", summary.body);
        assert!(!summary.body.contains("audit(1)"), "{}", summary.body);
        // The footer points at the hidden interior, and a matched read of it needs no guessing.
        assert!(summary.body.contains("offset 6, limit 14"), "{}", summary.body);
        assert!(summary.body.ends_with(']'), "{}", summary.body);
    }

    #[test]
    fn fixture_rust_unfold_target_and_ceiling_bound_the_summary() {
        // 36 visible lines with only outermost elisions. A 45-line ceiling skips the 14-line pipeline body
        // but still admits the 7-line nested body, which the BFS reaches next.
        let summary = try_summary(
            RUST_FIXTURE,
            "sample.rs",
            &fixture_cfg(SummarizeCfg { unfold_until: 50, unfold_limit: 45, ..Default::default() }),
        )
        .unwrap();
        assert!(summary.body.contains("        audit(1);"), "{}", summary.body);
        assert!(summary.body.contains("    finish();"), "{}", summary.body);
        assert!(summary.body.contains("pub fn pipeline() { … }"), "{}", summary.body);
        assert!(!summary.body.contains("let stage_1"), "{}", summary.body);
        // The shipped defaults unfold the pipeline body first and stop at 51 lines, under the 100 ceiling.
        let defaults = try_summary(RUST_FIXTURE, "sample.rs", &fixture_cfg(SummarizeCfg::default())).unwrap();
        assert!(defaults.body.contains("    let stage_1 = keep(1);"), "{}", defaults.body);
        assert!(!defaults.body.contains("audit(1)"), "{}", defaults.body);
        assert_eq!(visible_lines_of(&defaults.body), 51, "{}", defaults.body);
    }

    #[test]
    fn fixture_python_nests_blocks_by_indentation() {
        let summary = try_summary(PY_FIXTURE, "sample.py", &fixture_cfg(SummarizeCfg { unfold_until: 0, ..Default::default() }))
            .unwrap();
        assert!(summary.body.starts_with("def pipeline():\n…"), "{}", summary.body);
        assert!(summary.body.contains("def small():\n    one()"), "{}", summary.body);
        assert!(summary.body.contains("class Worker:\n…"), "{}", summary.body);
        assert!(!summary.body.contains("stage_1"), "{}", summary.body);
        assert!(!summary.body.contains("slot_1"), "{}", summary.body);
    }

    #[test]
    fn fixture_markdown_keeps_headings_and_fences() {
        let summary = try_summary(
            MD_FIXTURE,
            "sample.md",
            &fixture_cfg(SummarizeCfg { prose: true, unfold_until: 0, ..Default::default() }),
        )
        .unwrap();
        assert!(summary.body.starts_with("# Read Summary Fixture\n\nShort intro kept verbatim."), "{}", summary.body);
        assert!(summary.body.contains("\nSentence 1 explains"), "{}", summary.body);
        assert!(!summary.body.contains("Sentence 3 explains"), "{}", summary.body);
        assert!(summary.body.contains("## Details"), "{}", summary.body);
        assert!(summary.body.contains("```rust\n…\n```"), "{}", summary.body);
        assert!(!summary.body.contains("let c = 3;"), "{}", summary.body);
        assert!(summary.body.contains("Trailing line."), "{}", summary.body);
        // The default prose setting leaves Markdown alone.
        assert!(try_summary(MD_FIXTURE, "sample.md", &fixture_cfg(SummarizeCfg::default())).is_none());
    }

    #[test]
    fn prose_summaries_need_the_prose_setting() {
        let paragraph = (1..=6).map(|i| format!("Sentence number {i} about the topic.")).collect::<Vec<_>>().join("\n");
        let text = format!("# Title\n\nIntro line.\n\n{paragraph}\n\nClosing.\n");
        assert!(try_summary(&text, "notes.md", &read(SummarizeCfg { min_total_lines: 1, ..Default::default() })).is_none());
        let summary = try_summary(
            &text,
            "notes.md",
            &read(SummarizeCfg { min_total_lines: 1, prose: true, unfold_until: 0, ..Default::default() }),
        )
        .unwrap();
        assert!(summary.body.starts_with("# Title\n\nIntro line.\n\nSentence number 1"), "{}", summary.body);
        assert!(summary.body.contains("…"), "{}", summary.body);
        assert!(summary.body.contains("Closing."), "{}", summary.body);
    }

    #[test]
    fn markdown_fences_keep_their_fence_lines() {
        let code = (1..=8).map(|i| format!("    step{i}()")).collect::<Vec<_>>().join("\n");
        let text = format!("Title\n\n```rust\n{code}\n```\n\nAfter.\n");
        let summary = try_summary(
            &text,
            "notes.md",
            &read(SummarizeCfg { min_total_lines: 1, prose: true, unfold_until: 0, ..Default::default() }),
        )
        .unwrap();
        assert!(summary.body.contains("```rust\n…\n```"), "{}", summary.body);
    }

    #[test]
    fn python_blocks_fold_by_indentation() {
        let body = (1..=6).map(|i| format!("    step_{i}()")).collect::<Vec<_>>().join("\n");
        let text = format!("def f():\n{body}\n\nx = 1\n\ndef g():\n    one()\n");
        let summary = try_summary(&text, "a.py", &read(SummarizeCfg { min_total_lines: 1, unfold_until: 0, ..Default::default() })).unwrap();
        assert!(summary.body.starts_with("def f():\n…"), "{}", summary.body);
        assert!(summary.body.contains("x = 1"), "{}", summary.body);
        assert!(summary.body.contains("def g():\n    one()"), "{}", summary.body);
    }

    #[test]
    fn numbered_summaries_prefix_ranges_and_lines() {
        let text = fixture();
        let read = ReadCfg {
            line_numbers: true,
            summarize: SummarizeCfg { min_total_lines: 1, unfold_until: 0, ..Default::default() },
            ..Default::default()
        };
        let summary = try_summary(&text, "a.rs", &read).unwrap();
        assert!(summary.body.starts_with("1-12:fn big() { … }"), "{}", summary.body);
        assert!(summary.body.contains("\n14:fn small() {"), "{}", summary.body);
    }
}
