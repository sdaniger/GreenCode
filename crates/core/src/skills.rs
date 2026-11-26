use crate::config::{
    integrations::{CommandsCfg, SkillsCfg},
    Config,
};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

pub struct Skill {
    pub name: String,
    pub description: String,
    pub path: PathBuf,
    /// Loaded but left out of the model's catalog (`manualSkills` or frontmatter `hide: true`).
    pub hidden: bool,
}

/// Claude and native command roots; Claude is read too so existing commands just work.
fn roots(cwd: &Path, opts: &CommandsCfg) -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if opts.enable_claude_user {
        roots.push(crate::user_home().join(".claude"));
    }
    roots.push(crate::home());
    if opts.enable_claude_project {
        roots.push(cwd.join(".claude"));
    }
    roots.push(cwd.join(".greencode"));
    roots
}

/// Expands `~` and resolves a relative entry against `cwd`.
fn dir(entry: &str, cwd: &Path) -> PathBuf {
    let path = match entry.strip_prefix("~/") {
        Some(rest) => crate::user_home().join(rest),
        None if entry == "~" => crate::user_home(),
        None => cwd.join(entry),
    };
    path
}

/// Directories holding `<name>/SKILL.md`, later wins. `.gemini` has no toggle; `customDirectories` come last
/// and are scanned directly (they are the parent of the skill directories, not a `skills` root).
fn skill_dirs(cwd: &Path, opts: &SkillsCfg) -> Vec<PathBuf> {
    let user = crate::user_home();
    let mut roots = Vec::new();
    let mut push = |root: PathBuf| roots.push(root.join("skills"));
    if opts.enable_agents_user {
        push(user.join(".agents"));
    }
    if opts.enable_claude_user {
        push(user.join(".claude"));
    }
    if opts.enable_pi_user {
        push(crate::home());
    }
    if opts.enable_codex_user {
        push(user.join(".codex"));
    }
    if opts.enable_agents_project {
        push(cwd.join(".agents"));
    }
    push(cwd.join(".gemini"));
    if opts.enable_codex_user {
        push(cwd.join(".codex"));
    }
    if opts.enable_pi_project {
        push(cwd.join(".greencode"));
    }
    if opts.enable_claude_project {
        push(cwd.join(".claude"));
    }
    roots.extend(opts.custom_directories.iter().map(|entry| dir(entry, cwd)));
    roots
}

/// OpenCode custom commands. Project files override global files, matching OpenCode's loading order.
pub fn opencode_commands(cwd: &Path, cfg: &Config) -> Vec<String> {
    let opts = &cfg.integration_opts.commands;
    let mut names = BTreeMap::new();
    let mut roots = Vec::new();
    if opts.enable_opencode_project {
        let mut current = cwd.to_path_buf();
        loop {
            roots.push(current.join(".opencode"));
            if !current.pop() {
                break;
            }
        }
    }
    if opts.enable_opencode_user {
        roots.push(crate::user_home().join(".config/opencode"));
    }
    for root in roots.into_iter().rev() {
        for dir in [root.join("commands"), root.join("command")] {
            collect_opencode_commands(&dir, "", &mut names);
        }
    }
    names.into_keys().collect()
}

fn collect_opencode_commands(dir: &Path, prefix: &str, out: &mut BTreeMap<String, ()>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        if path.is_dir() {
            let next = if prefix.is_empty() {
                name
            } else {
                format!("{prefix}/{name}")
            };
            collect_opencode_commands(&path, &next, out);
        } else if path.extension().is_some_and(|ext| ext == "md") {
            let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
            if !stem.is_empty() {
                let command = if prefix.is_empty() {
                    stem.to_string()
                } else {
                    format!("{prefix}/{stem}")
                };
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
    front.lines().find_map(|l| {
        Some(
            l.strip_prefix(key)?
                .strip_prefix(':')?
                .trim()
                .trim_matches(['"', '\'']),
        )
    })
}

/// `*` and `?` stay inside one `/` segment; `**` crosses them.
fn glob(pattern: &str, name: &str) -> bool {
    let (p, s): (Vec<char>, Vec<char>) = (pattern.chars().collect(), name.chars().collect());
    fn go(p: &[char], s: &[char]) -> bool {
        let Some(&first) = p.first() else {
            return s.is_empty();
        };
        match first {
            '*' if p.get(1) == Some(&'*') => {
                let rest = &p[2..];
                let skip_slash = rest.first() == Some(&'/');
                (0..=s.len()).any(|i| go(if skip_slash { &rest[1..] } else { rest }, &s[i..]))
                    || (0..=s.len()).any(|i| go(rest, &s[i..]))
            }
            '*' => {
                let rest = &p[1..];
                let mut i = 0;
                loop {
                    if go(rest, &s[i..]) {
                        return true;
                    }
                    if i == s.len() || s[i] == '/' {
                        return false;
                    }
                    i += 1;
                }
            }
            '?' => !s.is_empty() && s[0] != '/' && go(&p[1..], &s[1..]),
            c => s.first() == Some(&c) && go(&p[1..], &s[1..]),
        }
    }
    go(&p, &s)
}

fn any<'a>(patterns: impl Iterator<Item = &'a str>, name: &str) -> bool {
    patterns.into_iter().any(|pattern| glob(pattern, name))
}

/// `disabledExtensions` ids that remove skills: `skill:<pattern>`.
fn disabled_skills(cfg: &Config) -> impl Iterator<Item = &str> {
    cfg.integration_opts
        .disabled_extensions
        .iter()
        .filter_map(|entry| entry.strip_prefix("skill:"))
}

pub fn list(cwd: &Path, cfg: &Config) -> Vec<Skill> {
    let opts = &cfg.integration_opts.skills;
    let manual = opts.manual_skills.iter().map(String::as_str);
    let mut found = BTreeMap::new();
    for root in skill_dirs(cwd, opts) {
        for e in std::fs::read_dir(root).into_iter().flatten().flatten() {
            let path = e.path().join("SKILL.md");
            let Ok(s) = std::fs::read_to_string(&path) else {
                continue;
            };
            let (front, _) = frontmatter(&s);
            let name = field(front, "name")
                .map(String::from)
                .unwrap_or_else(|| e.file_name().to_string_lossy().into());
            if any(opts.ignored_skills.iter().map(String::as_str), &name)
                || any(disabled_skills(cfg), &name)
            {
                continue;
            }
            let description = field(front, "description").unwrap_or("").to_string();
            let hidden = field(front, "hide") == Some("true") || any(manual.clone(), &name);
            found.insert(
                name.clone(),
                Skill {
                    name,
                    description,
                    path,
                    hidden,
                },
            );
        }
    }
    found.into_values().collect()
}

/// The model's skill catalog: `includeSkills` allowlist (when set) without manually hidden skills.
pub fn catalog(cwd: &Path, cfg: &Config) -> Vec<Skill> {
    let include = &cfg.integration_opts.skills.include_skills;
    list(cwd, cfg)
        .into_iter()
        .filter(|s| !s.hidden)
        .filter(|s| include.is_empty() || any(include.iter().map(String::as_str), &s.name))
        .collect()
}

/// Body of `skills/<name>/SKILL.md`, without its frontmatter.
pub fn body(cwd: &Path, cfg: &Config, name: &str) -> Option<String> {
    if name.is_empty() || name.contains(['/', '\\', '.']) {
        return None;
    }
    let skill = list(cwd, cfg).into_iter().find(|s| s.name == name)?;
    Some(
        frontmatter(&std::fs::read_to_string(skill.path).ok()?)
            .1
            .trim()
            .to_string(),
    )
}

/// Body of `commands/<name>.md`.
pub fn command(cwd: &Path, cfg: &Config, name: &str) -> Option<String> {
    if name.is_empty() || name.contains(['/', '\\', '.']) {
        return None;
    }
    roots(cwd, &cfg.integration_opts.commands).iter().rev().find_map(|r| {
        let s = std::fs::read_to_string(r.join("commands").join(format!("{name}.md"))).ok()?;
        Some(frontmatter(&s).1.to_string())
    })
}

pub fn commands(cwd: &Path, cfg: &Config) -> Vec<String> {
    let mut names: Vec<String> = roots(cwd, &cfg.integration_opts.commands)
        .iter()
        .flat_map(|r| {
            std::fs::read_dir(r.join("commands"))
                .into_iter()
                .flatten()
                .flatten()
        })
        .filter_map(|e| Some(e.file_name().to_str()?.strip_suffix(".md")?.to_string()))
        .collect();
    names.sort();
    names.dedup();
    names
}

#[cfg(test)]
mod tests {
    use super::{catalog, commands, glob, list, opencode_commands};
    use crate::config::Config;

    fn temp(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("greencode-skills-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Hermetic options: only the temp project's roots, never the test machine's home.
    fn local() -> Config {
        let mut cfg = Config::default();
        cfg.integration_opts.skills.enable_codex_user = false;
        cfg.integration_opts.skills.enable_claude_user = false;
        cfg.integration_opts.skills.enable_pi_user = false;
        cfg.integration_opts.skills.enable_agents_user = false;
        cfg
    }

    fn skill(dir: &std::path::Path, root: &str, name: &str, front: &str) {
        let path = dir.join(root).join("skills").join(name);
        std::fs::create_dir_all(&path).unwrap();
        std::fs::write(path.join("SKILL.md"), format!("---\n{front}\n---\nbody")).unwrap();
    }

    /// A `customDirectories` entry is itself the parent of the skill directories.
    fn custom(dir: &std::path::Path, root: &str, name: &str, front: &str) {
        let path = dir.join(root).join(name);
        std::fs::create_dir_all(&path).unwrap();
        std::fs::write(path.join("SKILL.md"), format!("---\n{front}\n---\nbody")).unwrap();
    }

    #[test]
    fn lists_skills_from_agents_and_gemini_dirs_with_claude_winning() {
        let dir = temp("roots");
        for (root, name) in [(".agents", "shared"), (".gemini", "gem"), (".agents", "dup"), (".claude", "dup")] {
            skill(&dir, root, name, &format!("description: from {root}"));
        }
        let mut cfg = local();
        cfg.integration_opts.skills.enable_pi_project = false;
        let found: Vec<_> = list(&dir, &cfg)
            .into_iter()
            .filter(|s| ["shared", "gem", "dup"].contains(&s.name.as_str()))
            .map(|s| (s.name, s.description))
            .collect();
        assert_eq!(found, [("dup".into(), "from .claude".into()), ("gem".into(), "from .gemini".into()), ("shared".into(), "from .agents".into())]);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn source_flags_filters_and_manual_skills() {
        let dir = temp("filters");
        skill(&dir, ".claude", "claude-only", "description: c");
        skill(&dir, ".codex", "codex-only", "description: x");
        skill(&dir, ".greencode", "native", "description: n");
        custom(&dir, "custom", "custom-one", "description: custom");
        let mut cfg = local();
        cfg.integration_opts.skills.enable_claude_project = false;
        cfg.integration_opts.skills.enable_codex_user = false;
        cfg.integration_opts.skills.custom_directories = vec!["custom".into()];
        cfg.integration_opts.skills.manual_skills = vec!["custom-*".into()];
        cfg.integration_opts.skills.ignored_skills = vec!["nativ*".into()];
        let names = |cfg: &Config| list(&dir, cfg).into_iter().map(|s| s.name).collect::<Vec<_>>();
        assert_eq!(names(&cfg), ["custom-one"]);
        // The custom directory overrides a same-named provider skill.
        skill(&dir, ".greencode", "custom-one", "description: shadowed");
        assert_eq!(list(&dir, &cfg).iter().find(|s| s.name == "custom-one").unwrap().description, "custom");
        // Manual skills stay loaded but leave the catalog.
        assert!(catalog(&dir, &cfg).is_empty());
        assert!(list(&dir, &cfg).iter().all(|s| s.hidden));
        // Source flags are independent.
        cfg.integration_opts.skills.enable_claude_project = true;
        cfg.integration_opts.skills.enable_codex_user = true;
        assert_eq!(names(&cfg), ["claude-only", "codex-only", "custom-one"]);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn include_allowlist_hides_from_the_catalog_only() {
        let dir = temp("include");
        custom(&dir, "custom", "alpha", "description: a");
        custom(&dir, "custom", "beta", "description: b");
        let mut cfg = local();
        cfg.integration_opts.skills.custom_directories = vec!["custom".into()];
        cfg.integration_opts.skills.include_skills = vec!["al*".into()];
        assert_eq!(catalog(&dir, &cfg).into_iter().map(|s| s.name).collect::<Vec<_>>(), ["alpha"]);
        assert_eq!(list(&dir, &cfg).len(), 2);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn claude_command_roots_follow_command_flags() {
        let dir = temp("command-flags");
        std::fs::create_dir_all(dir.join(".claude/commands")).unwrap();
        std::fs::write(dir.join(".claude/commands/deploy.md"), "Deploy").unwrap();
        let mut cfg = local();
        assert!(commands(&dir, &cfg).iter().any(|name| name == "deploy"));
        cfg.integration_opts.commands.enable_claude_project = false;
        assert!(!commands(&dir, &cfg).iter().any(|name| name == "deploy"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn discovers_nested_project_commands() {
        let dir = temp("commands");
        std::fs::create_dir_all(dir.join(".opencode/commands/team")).unwrap();
        std::fs::write(dir.join(".opencode/commands/review.md"), "Review").unwrap();
        std::fs::write(dir.join(".opencode/commands/team/test.md"), "Test").unwrap();
        let cfg = local();
        assert_eq!(opencode_commands(&dir, &cfg), ["review", "team/test"]);
        let mut off = local();
        off.integration_opts.commands.enable_opencode_project = false;
        assert!(!opencode_commands(&dir, &off).iter().any(|c| c == "review"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn globs_stay_in_segments_unless_doubled() {
        assert!(glob("old-*", "old-skill"));
        assert!(!glob("old-*", "old/other"));
        assert!(glob("team/**", "team/a/b"));
        assert!(glob("**/x", "a/x"));
        assert!(glob("**/x", "x"));
        assert!(!glob("*", "a/b"));
        assert!(glob("a?c", "abc"));
    }
}
