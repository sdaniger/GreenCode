use crate::skills;
use std::path::Path;

pub const MEMORY_LIMIT: usize = 4000;
const OUTPUT_LIMIT: usize = 30_000;
const KEEP: usize = 10_000;

pub fn memory_files(cwd: &Path) -> [(&'static str, std::path::PathBuf); 2] {
    [("global", crate::home().join("memory/MEMORY.md")), ("project", cwd.join(".greencode/MEMORY.md"))]
}

/// Built once per session and never changed afterwards: any byte that differs
/// between requests would invalidate the provider's prompt cache.
pub fn system_prompt(cwd: &Path) -> String {
    let mut s = format!(
        "You are GreenCode, a coding agent working in {}.\n\
Be concise. Use the tools to inspect and change the project; search with `bash` (rg, ls, git). \
Read a file before editing it. Call independent tools in parallel.\n\
Long tool output is cut in the middle; the full text is saved at the path shown.\n\
Use the `memory` tool to record durable facts about the user or project that will matter in later sessions.\n\
When a task matches a skill listed below, `read` its file first. After working out a reusable, non-trivial procedure, \
save it as a skill: write ~/.greencode/skills/<name>/SKILL.md with `name` and `description` frontmatter.\n",
        cwd.display()
    );
    for (scope, path) in memory_files(cwd) {
        if let Ok(m) = std::fs::read_to_string(path) {
            if !m.trim().is_empty() {
                s.push_str(&format!("\n# Memory ({scope})\n{}\n", m.trim()));
            }
        }
    }
    let skills = skills::list(cwd);
    if !skills.is_empty() {
        s.push_str("\n# Skills\n");
        for k in skills {
            s.push_str(&format!("- {}: {} ({})\n", k.name, k.description, k.path.display()));
        }
    }
    for name in ["AGENTS.md", "CLAUDE.md"] {
        if let Ok(doc) = std::fs::read_to_string(cwd.join(name)) {
            s.push_str(&format!("\n# Project instructions ({name})\n{}\n", doc.trim()));
            break;
        }
    }
    s
}

fn floor(s: &str, mut i: usize) -> usize {
    while !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// Keeps the head and tail of long tool output and saves the whole text to a file.
pub fn truncate(out: String, id: &str) -> String {
    if out.len() <= OUTPUT_LIMIT {
        return out;
    }
    let dir = crate::home().join("spill");
    let path = dir.join(format!("{}.txt", id.replace(|c: char| !c.is_ascii_alphanumeric(), "_")));
    let saved = std::fs::create_dir_all(&dir).and_then(|_| std::fs::write(&path, &out)).is_ok();
    let head = &out[..floor(&out, KEEP)];
    let tail = &out[floor(&out, out.len() - KEEP)..];
    let note = if saved { format!("full output: {}", path.display()) } else { "full output could not be saved".into() };
    format!("{head}\n\n[... {} bytes omitted; {note} ...]\n\n{tail}", out.len() - 2 * KEEP)
}
