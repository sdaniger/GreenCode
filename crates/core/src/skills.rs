use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

pub struct Skill {
    pub name: String,
    pub description: String,
    pub path: PathBuf,
}

/// Later roots win. `.claude` is read too so existing skills and commands just work.
fn roots(cwd: &Path) -> [PathBuf; 4] {
    [crate::user_home().join(".claude"), crate::home(), cwd.join(".claude"), cwd.join(".greencode")]
}

/// Splits `---` frontmatter from the body.
fn frontmatter(s: &str) -> (&str, &str) {
    if let Some(rest) = s.strip_prefix("---") {
        if let Some(end) = rest.find("\n---") {
            let body = &rest[end + 4..];
            return (&rest[..end], body.strip_prefix('\n').unwrap_or(body));
        }
    }
    ("", s)
}

fn field<'a>(front: &'a str, key: &str) -> Option<&'a str> {
    front.lines().find_map(|l| Some(l.strip_prefix(key)?.strip_prefix(':')?.trim().trim_matches(['"', '\''])))
}

pub fn list(cwd: &Path) -> Vec<Skill> {
    let mut found = BTreeMap::new();
    for root in roots(cwd) {
        for e in std::fs::read_dir(root.join("skills")).into_iter().flatten().flatten() {
            let path = e.path().join("SKILL.md");
            let Ok(s) = std::fs::read_to_string(&path) else { continue };
            let (front, _) = frontmatter(&s);
            let name = field(front, "name").map(String::from).unwrap_or_else(|| e.file_name().to_string_lossy().into());
            let description = field(front, "description").unwrap_or("").to_string();
            found.insert(name.clone(), Skill { name, description, path });
        }
    }
    found.into_values().collect()
}

/// Body of `commands/<name>.md`.
pub fn command(cwd: &Path, name: &str) -> Option<String> {
    if name.is_empty() || name.contains(['/', '\\', '.']) {
        return None;
    }
    roots(cwd).iter().rev().find_map(|r| {
        let s = std::fs::read_to_string(r.join("commands").join(format!("{name}.md"))).ok()?;
        Some(frontmatter(&s).1.to_string())
    })
}

pub fn commands(cwd: &Path) -> Vec<String> {
    let mut names: Vec<String> = roots(cwd)
        .iter()
        .flat_map(|r| std::fs::read_dir(r.join("commands")).into_iter().flatten().flatten())
        .filter_map(|e| Some(e.file_name().to_str()?.strip_suffix(".md")?.to_string()))
        .collect();
    names.sort();
    names.dedup();
    names
}
