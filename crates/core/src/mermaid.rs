//! Render the common Mermaid diagram kinds as ASCII art inside the terminal transcript.
//!
//! Supported: `graph`/`flowchart` (TD/TB/LR/RL/BT), `sequenceDiagram`, `pie`, `stateDiagram`,
//! `classDiagram` and `mindmap` degrade to an indented outline; anything else returns `None` so the
//! caller keeps the fenced source.

/// Render every ```mermaid fence in `text`; unanswered fences keep their source.
pub fn render_fences(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find("```mermaid") {
        out.push_str(&rest[..at]);
        let after = &rest[at + "```mermaid".len()..];
        let skip = after.len() - after.strip_prefix('\n').unwrap_or(after).len();
        let after = after.strip_prefix('\n').unwrap_or(after);
        let Some(end) = after.find("```") else {
            out.push_str(&rest[at..]);
            return out;
        };
        let source = &after[..end];
        match render(source) {
            Some(art) => {
                out.push_str(&art);
                if !art.ends_with('\n') {
                    out.push('\n');
                }
            }
            None => out.push_str(&rest[at..at + "```mermaid".len() + skip + end + 3]),
        }
        rest = &after[end + 3..];
    }
    out.push_str(rest);
    out
}

/// One diagram, or `None` when the kind is unsupported or the source is not a diagram.
pub fn render(source: &str) -> Option<String> {
    let mut lines = source.lines().map(str::trim).filter(|line| !line.is_empty() && !line.starts_with("%%"));
    let header = lines.next()?;
    let body: Vec<&str> = lines.collect();
    if let Some(direction) = header.strip_prefix("graph").or_else(|| header.strip_prefix("flowchart")) {
        let direction = direction.trim();
        return Some(flowchart(&body, direction));
    }
    if header.starts_with("sequenceDiagram") {
        return Some(sequence(&body));
    }
    if header.starts_with("pie") {
        return Some(pie(header, &body));
    }
    if header.starts_with("stateDiagram") || header.starts_with("classDiagram") || header.starts_with("mindmap") {
        return Some(outline(&body));
    }
    None
}

fn label(raw: &str) -> String {
    let raw = raw.trim();
    let raw = raw.strip_prefix('"').and_then(|s| s.strip_suffix('"')).unwrap_or(raw);
    format!("[{}]", raw.trim_matches(|c| matches!(c, '[' | ']' | '(' | ')' | '{' | '}')))
}

/// The node id before any shape bracket.
fn node_id(text: &str) -> &str {
    let text = text.split_once('|').map_or(text, |(node, _)| node);
    let end = text.find(['[', '(', '{', ' ']).unwrap_or(text.len());
    &text[..end]
}

/// The bracketed labels in one side of an edge.
fn node_labels(text: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut rest = text.split_once('|').map_or(text, |(node, _)| node).trim();
    while let Some(at) = rest.find(['[', '(', '{']) {
        let open = rest.as_bytes()[at];
        let close = match open { b'[' => ']', b'(' => ')', _ => '}' };
        let Some(len) = rest[at..].find(close) else { break };
        out.push(&rest[at..at + len + 1]);
        rest = &rest[at + len + 1..];
    }
    out
}

/// `A[Start] -->|yes| B{Check}` → edges with their link labels; a node's first label wins.
fn flowchart(body: &[&str], direction: &str) -> String {
    use std::collections::BTreeMap;
    let mut nodes: BTreeMap<String, String> = BTreeMap::new();
    let mut edges: Vec<(String, String, String)> = Vec::new();
    let mut declare = |text: &str, nodes: &mut BTreeMap<String, String>| -> String {
        let id = node_id(text).trim().to_string();
        let labels = node_labels(text);
        if !id.is_empty() {
            match labels.first() {
                Some(l) => { nodes.entry(id.clone()).or_insert_with(|| label(l)); }
                None => { nodes.entry(id.clone()).or_insert_with(|| format!("({id})")); }
            }
        }
        id
    };
    for &line in body {
        if line.starts_with("subgraph") || line == "end" {
            continue;
        }
        // Node texts and link labels; `-->|yes|` puts the label between pipes after the arrow.
        let mut rest = line;
        let mut previous: Option<String> = None;
        let mut link = String::new();
        while !rest.trim().is_empty() {
            let arrow = ["-->", "---", "->", "--"]
                .iter()
                .filter_map(|a| rest.find(a).map(|at| (at, a.len())))
                .min_by_key(|&(at, len)| (at, std::cmp::Reverse(len)));
            let (left, next) = match arrow {
                Some((at, len)) => (&rest[..at], Some(&rest[at + len..])),
                None => (rest, None),
            };
            let id = declare(left, &mut nodes);
            if let Some(prev) = previous.replace(id.clone()) {
                edges.push((prev, id, std::mem::take(&mut link)));
            }
            match next {
                Some(next) => {
                    let next = next.trim_start();
                    if let Some((label, after)) = next.strip_prefix('|').and_then(|n| n.split_once('|')) {
                        link = label.trim().to_string();
                        rest = after.trim_start();
                    } else {
                        rest = next;
                    }
                }
                None => break,
            }
        }
    }
    if edges.is_empty() {
        return nodes.values().map(|v| format!("  {v}")).collect::<Vec<_>>().join("\n");
    }
    let mut out = String::new();
    let text = |id: &str, nodes: &BTreeMap<String, String>| nodes.get(id).cloned().unwrap_or_else(|| format!("({id})"));
    let downstream = matches!(direction.to_ascii_uppercase().as_str(), "" | "TD" | "TB" | "BT");
    if downstream {
        let mut depth: BTreeMap<&str, usize> = BTreeMap::new();
        depth.insert(edges[0].0.as_str(), 0);
        for _ in 0..=edges.len() {
            for (from, to, _) in &edges {
                let d = depth.get(from.as_str()).copied().unwrap_or(0) + 1;
                let slot = depth.entry(to.as_str()).or_insert(d);
                *slot = (*slot).min(d);
            }
        }
        let mut levels: BTreeMap<usize, Vec<String>> = BTreeMap::new();
        for (id, d) in &depth {
            levels.entry(*d).or_default().push(text(id, &nodes));
        }
        for (i, (_, mut items)) in levels.into_iter().enumerate() {
            items.sort();
            if i > 0 {
                out.push_str("  │\n  ▼\n");
            }
            out.push_str(&format!("  {}\n", items.join("   ")));
        }
    } else {
        out.push_str(&format!("  {}\n", text(&edges[0].0, &nodes)));
        for (_, to, link) in &edges {
            let arrow = if link.is_empty() { "───▶".to_string() } else { format!("── {link} ─▶") };
            out.push_str(&format!("  {arrow} {}\n", text(to, &nodes)));
        }
    }
    out.trim_end().to_string()
}

fn sequence(body: &[&str]) -> String {
    let mut actors: Vec<String> = Vec::new();
    let mut events: Vec<(String, String, String)> = Vec::new();
    for &line in body {
        let mut words = line.splitn(2, ':');
        let head = words.next().unwrap_or_default().trim();
        let message = words.next().unwrap_or_default().trim();
        if let Some(rest) = head.strip_prefix("participant ").or_else(|| head.strip_prefix("actor ")) {
            let name = rest.split_once(" as ").map_or(rest, |(_, alias)| alias).trim().to_string();
            if !actors.contains(&name) {
                actors.push(name);
            }
            continue;
        }
        if head.starts_with("Note") {
            events.push((String::new(), String::new(), format!("[ {message} ]")));
            continue;
        }
        let Some((from, to)) = head.split_once("->>").or_else(|| head.split_once("-->>")).or_else(|| head.split_once("->")) else { continue };
        let (from, to) = (from.trim().to_string(), to.trim().to_string());
        for name in [&from, &to] {
            if !actors.contains(name) {
                actors.push(name.clone());
            }
        }
        events.push((from, to, message.to_string()));
    }
    if actors.is_empty() {
        return String::new();
    }
    let width = actors.iter().map(|a| a.chars().count()).max().unwrap_or(0) + 2;
    let mut header = String::new();
    for actor in &actors {
        header.push_str(&format!("{actor:<width$}"));
    }
    let mut out = format!("  {header}\n  {}\n", "─".repeat(header.chars().count()));
    for (from, to, message) in events {
        if from.is_empty() {
            out.push_str(&format!("  {message}\n"));
            continue;
        }
        let from_at = actors.iter().position(|a| *a == from).unwrap_or(0);
        let to_at = actors.iter().position(|a| *a == to).unwrap_or(0);
        let column = |i: usize| i * width + 1;
        let (a, b) = (column(from_at), column(to_at));
        let mut line: Vec<char> = " ".repeat(header.chars().count()).chars().collect();
        let (lo, hi) = (a.min(b), a.max(b));
        for cell in line.iter_mut().take(hi).skip(lo) {
            *cell = '─';
        }
        line[a] = if a < b { '╰' } else { '╯' };
        line[b] = if a < b { '╮' } else { '╭' };
        out.push_str(&format!("  {}\n  {message}\n", line.into_iter().collect::<String>().trim_end()));
    }
    out.trim_end().to_string()
}

fn pie(header: &str, body: &[&str]) -> String {
    let title = header.split_once("title").map_or("", |(_, title)| title.trim());
    let mut entries: Vec<(String, f64)> = Vec::new();
    for &line in body {
        let Some((name, value)) = line.split_once(':') else { continue };
        let value = value.trim().trim_end_matches('%').trim();
        if let Ok(value) = value.parse::<f64>() {
            entries.push((name.trim().trim_matches('"').to_string(), value));
        }
    }
    let total: f64 = entries.iter().map(|(_, v)| v).sum();
    if total <= 0.0 {
        return String::new();
    }
    let mut out = String::new();
    if !title.is_empty() {
        out.push_str(&format!("  {title}\n"));
    }
    for (name, value) in &entries {
        let share = value / total;
        let cells = (share * 24.0).round() as usize;
        out.push_str(&format!("  {name:<14} {} {:>5.1}%\n", "█".repeat(cells.max(1)), share * 100.0));
    }
    out.trim_end().to_string()
}

fn outline(body: &[&str]) -> String {
    let mut out = String::new();
    let mut depth = 0usize;
    for &line in body {
        if line == "}" || line == "end" || line.starts_with("end ") {
            depth = depth.saturating_sub(1);
            continue;
        }
        let text = line.trim_end_matches(['{', ':']).trim();
        if text.is_empty() {
            continue;
        }
        out.push_str(&format!("  {}{}\n", "  ".repeat(depth), text));
        if line.ends_with('{') || line.ends_with(':') || line.starts_with("state ") && !line.contains(':') {
            depth += 1;
        }
    }
    out.trim_end().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flowcharts_draw_downstream_and_sideways() {
        let down = render("graph TD\n  A[Read] -->|ok| B{Parse}\n  B --> C[Write]\n").unwrap();
        assert!(down.contains("[Read]") && down.contains("[Parse]") && down.contains("[Write]"), "{down}");
        assert!(down.contains('▼'), "{down}");
        let side = render("flowchart LR\n  A[Read] --> B[Write]\n").unwrap();
        assert!(side.contains("───▶ [Write]"), "{side}");
    }

    #[test]
    fn sequence_and_pie_render_labels() {
        let seq = render("sequenceDiagram\n  participant C as Client\n  C->>S: GET /x\n  S-->>C: 200\n").unwrap();
        assert!(seq.contains("Client"), "{seq}");
        assert!(seq.contains("GET /x") && seq.contains("200"), "{seq}");
        let pie = render("pie title Split\n  \"a\": 75\n  \"b\": 25\n").unwrap();
        assert!(pie.contains("Split"), "{pie}");
        assert!(pie.contains("75.0%") && pie.contains("25.0%"), "{pie}");
    }

    #[test]
    fn fences_are_replaced_in_place_and_unknown_kinds_keep_their_source() {
        let text = "before\n```mermaid\ngraph TD\n  A[One] --> B[Two]\n```\nafter";
        let out = render_fences(text);
        assert!(out.starts_with("before\n"), "{out}");
        assert!(out.contains("[One]") && out.ends_with("after"), "{out}");
        let unknown = "```mermaid\nkanban\n  a\n```";
        assert_eq!(render_fences(unknown), unknown);
        let unclosed = "```mermaid\ngraph TD\n A";
        assert_eq!(render_fences(unclosed), unclosed);
    }
}
