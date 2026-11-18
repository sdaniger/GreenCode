//! Rules injected only when the model's own output matches a pattern ("time-travelling" rules): they cost
//! no prompt bytes until they apply. A rule is a markdown file with `trigger:` (a regex) in its frontmatter.

use regex_lite::Regex;
use std::path::Path;

pub struct Rule {
    pub name: String,
    pub trigger: Regex,
    pub body: String,
}

fn parse(text: &str, stem: &str) -> Option<Rule> {
    let rest = text.strip_prefix("---")?.trim_start_matches(['\r', '\n']);
    let (front, body) = rest.split_once("\n---")?;
    let field = |key: &str| front.lines().find_map(|l| l.strip_prefix(key)?.trim().strip_prefix(':').map(|v| v.trim().trim_matches(['"', '\'']).to_string()));
    let trigger = Regex::new(&field("trigger")?).ok()?;
    let body = body.trim_start_matches(|c| c != '\n').trim().to_string();
    Some(Rule { name: field("name").filter(|n| !n.is_empty()).unwrap_or_else(|| stem.to_string()), trigger, body })
}

/// `~/.greencode/rules/*.md`, then `<project>/.greencode/rules/*.md`; a project rule replaces a global one of the same name.
pub fn load(cwd: &Path) -> Vec<Rule> {
    let mut rules: Vec<Rule> = vec![];
    for dir in [crate::home().join("rules"), cwd.join(".greencode/rules")] {
        let mut files: Vec<_> = std::fs::read_dir(dir).into_iter().flatten().flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|e| e == "md")).collect();
        files.sort();
        for path in files {
            let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("rule");
            if let Some(rule) = std::fs::read_to_string(&path).ok().and_then(|t| parse(&t, stem)) {
                rules.retain(|r| r.name != rule.name);
                rules.push(rule);
            }
        }
    }
    rules
}
