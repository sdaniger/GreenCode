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

/// OpenCode custom commands. Project files override global files, matching OpenCode's loading order.
pub fn opencode_commands(cwd: &Path) -> Vec<String> {
    let mut names = BTreeMap::new();
    let mut roots = Vec::new();
    let mut current = cwd.to_path_buf();
    loop {
        roots.push(current.join(".opencode"));
        if !current.pop() { break; }
    }
    roots.push(crate::user_home().join(".config/opencode"));
    for root in roots.into_iter().rev() {
        for dir in [root.join("commands"), root.join("command")] {
            collect_opencode_commands(&dir, "", &mut names);
        }
    }
    names.into_keys().collect()
}

fn collect_opencode_commands(dir: &Path, prefix: &str, out: &mut BTreeMap<String, ()>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        if path.is_dir() {
            let next = if prefix.is_empty() { name } else { format!("{prefix}/{name}") };
            collect_opencode_commands(&path, &next, out);
        } else if path.extension().is_some_and(|ext| ext == "md") {
            let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
            if !stem.is_empty() {
                let command = if prefix.is_empty() { stem.to_string() } else { format!("{prefix}/{stem}") };
                out.insert(command, ());
            }
        }
    }
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

/// Body of `skills/<name>/SKILL.md`, without its frontmatter.
pub fn body(cwd: &Path, name: &str) -> Option<String> {
    if name.is_empty() || name.contains(['/', '\\', '.']) { return None; }
    let skill = list(cwd).into_iter().find(|s| s.name == name)?;
    Some(frontmatter(&std::fs::read_to_string(skill.path).ok()?).1.trim().to_string())
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

#[cfg(test)]
mod tests {
    use super::opencode_commands;

    #[test]
    fn discovers_nested_project_commands() {
        let dir = std::env::temp_dir().join(format!("greencode-opencode-skills-{}", std::process::id()));
        std::fs::create_dir_all(dir.join(".opencode/commands/team")).unwrap();
        std::fs::write(dir.join(".opencode/commands/review.md"), "Review").unwrap();
        std::fs::write(dir.join(".opencode/commands/team/test.md"), "Test").unwrap();
        assert_eq!(opencode_commands(&dir), ["review", "team/test"]);
        let _ = std::fs::remove_dir_all(dir);
    }
}
