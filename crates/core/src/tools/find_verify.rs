//! `find`'s passage verification: after the cheap word-coverage ranking, the top passages are checked by a
//! model so the returned ranges really are where the behavior lives. `find.verify_model` selects that model
//! first; unset (or when it fails) keeps the judge role, and a failed verification leaves the ranking alone.

use crate::config::Config;

/// The model that verifies passages: `find.verify_model`, else the judge role, else the active model.
pub fn model(cfg: &Config) -> String {
    cfg.file_opts
        .find
        .verify_model
        .as_deref()
        .map(str::trim)
        .filter(|spec| !spec.is_empty())
        .map(String::from)
        .unwrap_or_else(|| cfg.role_or_default("judge").to_string())
}

/// The candidate ranges, best first, as `path:first-last score line`.
pub struct Candidate {
    pub path: String,
    pub first: u64,
    pub last: u64,
    pub score: f64,
    pub line: String,
}

/// Parses `find`'s own rendered rows back into candidates (kept so the verification round-trips through the
/// same text the model would see).
pub fn parse_rows(text: &str) -> Vec<Candidate> {
    text.lines()
        .filter_map(|row| {
            // `path:a-b score line: text`: the line part keeps its own `N:` prefix so a row round-trips.
            let mut parts = row.splitn(3, ' ');
            let range = parts.next()?;
            let score = parts.next()?.parse().ok()?;
            let line = parts.next().unwrap_or("").to_string();
            let (path, lines) = range.rsplit_once(':')?;
            let (first, last) = lines.split_once('-')?;
            Some(Candidate { path: path.into(), first: first.parse().ok()?, last: last.parse().ok()?, score, line })
        })
        .collect()
}

/// The verification request: show the passages and ask which really implement the behavior.
pub fn prompt(query: &str, candidates: &[Candidate]) -> String {
    let mut out = format!("The user is looking for: {query}\n\nCandidate passages:\n");
    for (i, c) in candidates.iter().enumerate() {
        out.push_str(&format!("{}. {}:{}: {}\n", i + 1, c.path, c.first, c.line));
    }
    out.push_str("\nReply with the numbers of the passages that plausibly implement this behavior, most relevant first, comma-separated. Reply `none` if none do.");
    out
}

pub const SYSTEM: &str =
    "You verify search results. Judge only whether each passage's shown line could plausibly be where the described behavior lives. Reply with numbers only.";

/// `None` when the model named no usable passage (`none`, empty, or only out-of-range numbers).
pub fn verified(text: &str, len: usize) -> Option<Vec<usize>> {
    let mut picked: Vec<usize> = text
        .split(|c: char| !c.is_ascii_digit())
        .filter_map(|n| n.parse::<usize>().ok())
        .filter(|n| (1..=len).contains(n))
        .map(|n| n - 1)
        .collect();
    picked.dedup();
    (!picked.is_empty()).then_some(picked)
}

/// Reorders `rows` by the verification; unverified rows follow in their original order so a wrong verdict
/// loses nothing.
pub fn reorder(rows: &str, verdict: Option<&[usize]>) -> String {
    let candidates = parse_rows(rows);
    if candidates.is_empty() {
        return rows.to_string();
    }
    // A verified index appears once; every other row follows in its original order.
    let mut order: Vec<usize> = Vec::new();
    for &i in verdict.unwrap_or_default() {
        if i < candidates.len() && !order.contains(&i) {
            order.push(i);
        }
    }
    let rest: Vec<usize> = (0..candidates.len()).filter(|i| !order.contains(i)).collect();
    order.extend(rest);
    order
        .into_iter()
        .map(|i| {
            let c = &candidates[i];
            format!("{}:{}-{} {:.2} {}", c.path, c.first, c.last, c.score, c.line)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Verifies `rows` with the configured model and returns them reordered. A single candidate needs no model;
/// a missing provider or a failed call leaves the ranking as it is, so `find` never fails over verification.
pub async fn verify(cfg: &crate::config::Config, query: &str, rows: &str) -> String {
    let candidates = parse_rows(rows);
    if candidates.len() < 2 || rows.is_empty() {
        return rows.to_string();
    }
    let model = model(cfg);
    let prompt = prompt(query, &candidates);
    match crate::textgen::generate(cfg, &model, SYSTEM, &prompt, 120).await {
        Ok(text) => match verified(&text, candidates.len()) {
            Some(order) => reorder(rows, Some(&order)),
            None => rows.to_string(),
        },
        Err(_) => rows.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ROWS: &str = "src/a.rs:10-30 0.75 12: fn handle() {\nsrc/b.rs:4-8 0.50 5: let x = 1;";

    #[test]
    fn verify_model_falls_back_to_the_judge_role() {
        let mut cfg = Config::default();
        cfg.model = "provider/main".into();
        assert_eq!(model(&cfg), "provider/main");
        cfg.model_roles.insert("judge".into(), "provider/judge".into());
        assert_eq!(model(&cfg), "provider/judge");
        cfg.file_opts.find.verify_model = Some(" openrouter/liquid ".into());
        assert_eq!(model(&cfg), "openrouter/liquid");
        cfg.file_opts.find.verify_model = Some(" ".into());
        assert_eq!(model(&cfg), "provider/judge");
    }

    #[test]
    fn rows_round_trip_and_reorder_by_the_verdict() {
        let rows = parse_rows(ROWS);
        assert_eq!(rows.len(), 2);
        assert_eq!((rows[0].path.as_str(), rows[0].first, rows[0].last), ("src/a.rs", 10, 30));
        assert_eq!(reorder(ROWS, Some(&[1, 0])), "src/b.rs:4-8 0.50 5: let x = 1;\nsrc/a.rs:10-30 0.75 12: fn handle() {");
        // Unverified rows keep their order after the verified ones.
        assert_eq!(reorder(ROWS, Some(&[1])), "src/b.rs:4-8 0.50 5: let x = 1;\nsrc/a.rs:10-30 0.75 12: fn handle() {");
        assert_eq!(reorder(ROWS, None), ROWS);
    }

    #[test]
    fn verdict_parsing_ignores_junk_and_none() {
        assert_eq!(verified("2, 1", 2), Some(vec![1, 0]));
        assert_eq!(verified("none", 2), None);
        assert_eq!(verified("9, 0", 2), None);
        assert_eq!(verified("", 2), None);
        assert!(prompt("where is auth", &parse_rows(ROWS)).contains("1. src/a.rs:10"));
    }
}
