use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::BTreeSet,
    io::Write,
    path::{Component, Path, PathBuf},
    sync::Mutex,
};

use crate::config::{
    bash::{BashCfg, BashRule, RuleApproval},
    interaction::ApprovalMode,
    Config,
};

/// One approval question: `key` is what "always allow" would store; restricted folders and
/// explicit `prompt` policies set `remember: false` so every call asks again.
#[derive(Clone, Debug, PartialEq)]
pub struct Ask {
    pub key: String,
    pub reason: Option<String>,
    pub remember: bool,
}

/// An approval decision made without asking the user.
#[derive(Clone, Debug, PartialEq)]
pub enum Verdict {
    Allow,
    Ask(Ask),
    Deny(String),
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Default)]
#[serde(rename_all = "kebab-case")]
pub enum Mode {
    /// Ask before every write and command.
    #[default]
    Ask,
    /// File edits inside the project run without asking.
    AutoEdit,
    /// Nothing asks.
    Full,
    /// Read-only: the agent may look and plan but not change anything.
    Plan,
}

pub struct Perms {
    mode: Mutex<Mode>,
    allow: Mutex<BTreeSet<String>>,
}

fn allow_file() -> PathBuf {
    crate::home().join("allow")
}

/// Resolves `.` and `..` without touching the filesystem.
pub fn normalize(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            c => out.push(c),
        }
    }
    out
}

pub fn resolve(cwd: &Path, path: &str) -> PathBuf {
    let p = match path.strip_prefix("~/") {
        Some(rest) => crate::user_home().join(rest),
        None => cwd.join(path),
    };
    normalize(&p)
}

fn physical(path: &Path) -> PathBuf {
    let mut existing = path;
    let mut suffix = Vec::new();
    loop {
        if let Ok(mut resolved) = existing.canonicalize() {
            for name in suffix.iter().rev() { resolved.push(name); }
            return resolved;
        }
        if let (Some(parent), Some(name)) = (existing.parent(), existing.file_name()) {
            suffix.push(name);
            existing = parent;
        } else { return path.to_path_buf(); }
    }
}

impl Perms {
    pub fn new(mode: Mode) -> Self {
        let allow = std::fs::read_to_string(allow_file()).unwrap_or_default().lines().map(String::from).collect();
        Perms { mode: Mutex::new(mode), allow: Mutex::new(allow) }
    }

    pub fn mode(&self) -> Mode {
        *self.mode.lock().unwrap()
    }

    pub fn set_mode(&self, m: Mode) {
        *self.mode.lock().unwrap() = m;
    }

    /// Resolution order: bash deny rules, a user `deny`, restricted folders, bash prompt/allow
    /// rules, a tool-wide `prompt`/`allow` policy, then the approval mode. None can bypass a
    /// restricted folder.
    pub fn verdict(&self, tool: &str, input: &Value, cwd: &Path) -> Verdict {
        let cfg = crate::tools::config();
        let policy = approval_policy(&cfg, tool);
        let pattern = (tool == "bash")
            .then(|| bash_patterns(&cfg.bash_opts.bash, input["command"].as_str().unwrap_or("")))
            .flatten();
        if pattern == Some(RuleApproval::Deny) {
            return Verdict::Deny("bash.patterns denies this command".into());
        }
        if policy == Some(RuleApproval::Deny) {
            return Verdict::Deny(format!("tools.approval denies `{tool}`"));
        }
        let key = || self.ask_key(tool, input, cwd);
        if let Some(dir) = restricted_dir(&cfg.bash_opts.restricted_folders, tool, input, cwd) {
            return Verdict::Ask(Ask {
                key: format!("!restricted:{}", dir.display()),
                reason: Some(format!("Restricted folder: {}", dir.display())),
                remember: false,
            });
        }
        // `bash.patterns` is per-segment policy; `tools.approval` covers only unmatched segments.
        if pattern == Some(RuleApproval::Prompt) {
            return Verdict::Ask(Ask {
                key: key().unwrap_or_default(),
                reason: Some("bash.patterns requires approval".into()),
                remember: false,
            });
        }
        if pattern == Some(RuleApproval::Allow) {
            return Verdict::Allow;
        }
        if policy == Some(RuleApproval::Prompt) {
            return Verdict::Ask(Ask { key: key().unwrap_or_default(), reason: None, remember: false });
        }
        if policy == Some(RuleApproval::Allow) {
            return Verdict::Allow;
        }
        let approval = self.approval(&cfg);
        if approval == ApprovalMode::Yolo {
            return Verdict::Allow;
        }
        match key() {
            Some(key) => Verdict::Ask(Ask { key, reason: None, remember: approval != ApprovalMode::AlwaysAsk }),
            None => Verdict::Allow,
        }
    }

    /// Ask-key form of [`Perms::verdict`]: `None` when allowed, else the ask key (or denial reason).
    pub fn check(&self, tool: &str, input: &Value, cwd: &Path) -> Option<String> {
        match self.verdict(tool, input, cwd) {
            Verdict::Allow => None,
            Verdict::Ask(a) => Some(a.key),
            Verdict::Deny(reason) => Some(reason),
        }
    }

    /// The approval behavior in force: a thread switched to `full` or `auto-edit` wins over `tools.approvalMode`.
    fn approval(&self, cfg: &Config) -> ApprovalMode {
        match self.mode() {
            Mode::Full => ApprovalMode::Yolo,
            Mode::AutoEdit => ApprovalMode::Write,
            Mode::Ask | Mode::Plan => cfg.interaction_opts.approval_mode,
        }
    }

    /// `None`: nothing to ask for. `Some(key)`: ask; `key` is what "always allow" stores.
    fn ask_key(&self, tool: &str, input: &Value, cwd: &Path) -> Option<String> {
        let mode = self.mode();
        let approval = self.approval(&crate::tools::config());
        // OMP's "ask and remember" also asks before file reads; the other modes approve them.
        let asks_reads = approval == ApprovalMode::AlwaysAskCached;
        let path = resolve(cwd, input["path"].as_str().unwrap_or(""));
        let target = physical(&path);
        // `workspace.additionalDirectories` are workspace roots too, so reading them needs no prompt.
        let roots = crate::tools::config().context_opts.workspace.roots(cwd);
        let inside = !input["path"].as_str().is_some_and(|s| s.contains("://"))
            && (target.starts_with(physical(cwd))
                || target.starts_with(physical(&crate::home()))
                || roots.iter().any(|root| target.starts_with(physical(root))));
        let key = match tool {
            "task" | "todo" | "web_search" | "advisor" => return None,
            "memory" => format!("memory:{}", input["scope"].as_str().unwrap_or("project")),
            "web_fetch" => format!("web_fetch:{}", reqwest::Url::parse(input["url"].as_str().unwrap_or("")).ok().and_then(|u| u.host_str().map(String::from)).unwrap_or_default()),
            "read" | "grep" | "glob" => {
                if (inside && !asks_reads) || path.starts_with(crate::user_home().join(".claude")) {
                    return None;
                }
                format!("read:{}", path.display())
            }
            "write" | "edit" => {
                // `.git/config` and hooks run commands, and read-only `git status` would then run them unasked.
                // APFS is case-insensitive, and a symlink can point at `.git`: check the resolved path, any case.
                if inside && (mode == Mode::AutoEdit || approval == ApprovalMode::Write)
                    && !target.components().any(|c| c.as_os_str().eq_ignore_ascii_case(".git")) {
                    return None;
                }
                if inside { tool.to_string() } else { format!("{tool}:{}", path.display()) }
            }
            "bash" => {
                let cmd = input["command"].as_str().unwrap_or("");
                if read_only_command(cmd, cwd) {
                    return None;
                }
                // Only a plain command may be allowed by its program name; anything
                // with shell operators must match exactly.
                if cmd.contains(|c| ";&|`$><\n(".contains(c)) {
                    format!("bash={cmd}")
                } else {
                    format!("bash:{}", cmd.split_whitespace().next().unwrap_or(""))
                }
            }
            // Approved per app, so "always allow" never covers an app the user has not seen named.
            "computer" => {
                if input["action"] == "list_apps" {
                    return None;
                }
                format!("computer:{}", input["app"].as_str().unwrap_or("").to_lowercase())
            }
            "mcp" => {
                if input["action"] != "call" {
                    return None;
                }
                format!("mcp:{}/{}", input["server"].as_str().unwrap_or(""), input["tool"].as_str().unwrap_or(""))
            }
            other => other.to_string(),
        };
        if self.allow.lock().unwrap().contains(&key) {
            None
        } else {
            Some(key)
        }
    }

    pub fn always(&self, key: String) {
        if key.contains('\n') || key.starts_with('!') {
            return; // one key per line; `!` keys are per-call and never remembered
        }
        if self.allow.lock().unwrap().insert(key.clone()) {
            let _ = std::fs::create_dir_all(crate::home());
            if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(allow_file()) {
                let _ = writeln!(f, "{key}");
            }
        }
    }
}

fn approval_policy(cfg: &Config, tool: &str) -> Option<RuleApproval> {
    let value = cfg.bash_opts.approval.get(tool)?.trim().to_ascii_lowercase();
    match value.as_str() {
        "allow" => Some(RuleApproval::Allow),
        "prompt" => Some(RuleApproval::Prompt),
        "deny" => Some(RuleApproval::Deny),
        _ => None,
    }
}

/// The restricted folder covering a call, if any. A tool with a path target is matched by that
/// target (and a search rooted above a restricted folder matches too); every other tool is
/// matched by the directory it runs in, as OMP does.
fn restricted_dir(folders: &[String], tool: &str, input: &Value, cwd: &Path) -> Option<PathBuf> {
    if folders.is_empty() {
        return None;
    }
    let names_path = matches!(tool, "read" | "write" | "edit" | "grep" | "glob");
    let target = if tool == "memory" {
        if input["scope"] == "global" {
            crate::home().join("memory/MEMORY.md")
        } else {
            cwd.join(".greencode/MEMORY.md")
        }
    } else {
        match names_path.then(|| input["path"].as_str()).flatten() {
            Some(path) => resolve(cwd, path),
            None => cwd.to_path_buf(),
        }
    };
    let target = physical(&target);
    for folder in folders {
        if folder.trim().is_empty() {
            continue;
        }
        let folder = match folder.strip_prefix("~/") {
            Some(rest) => crate::user_home().join(rest),
            None => cwd.join(folder),
        };
        let dir = physical(&normalize(&folder));
        if target.starts_with(&dir) || (names_path && matches!(tool, "grep" | "glob") && dir.starts_with(&target)) {
            return Some(dir);
        }
    }
    None
}

/// Splits a command at unquoted control operators; the flag marks a stage fed by a pipe.
pub(crate) fn segments(command: &str) -> Vec<(String, bool)> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quote = None;
    let mut piped = false;
    let mut chars = command.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' if quote != Some('\'') => {
                cur.push(c);
                if let Some(n) = chars.next() {
                    cur.push(n);
                }
            }
            '\'' | '"' if quote.is_none() => {
                quote = Some(c);
                cur.push(c);
            }
            c if quote == Some(c) => {
                quote = None;
                cur.push(c);
            }
            '&' | '|' | ';' | '\n' if quote.is_none() => {
                let mut op = c.to_string();
                if let Some(&n) = chars.peek() {
                    if matches!((c, n), ('&', '&') | ('|', '|') | ('|', '&')) {
                        op.push(chars.next().unwrap());
                    }
                }
                let seg = cur.trim();
                if !seg.is_empty() {
                    out.push((seg.to_string(), piped));
                }
                cur.clear();
                piped = matches!(op.as_str(), "|" | "|&");
            }
            _ => cur.push(c),
        }
    }
    let seg = cur.trim();
    if !seg.is_empty() {
        out.push((seg.to_string(), piped));
    }
    out
}

/// Ordered rule evaluation for `bash.patterns`; the first matching rule wins per segment,
/// denies beat prompts across segments, and allow rules only approve standalone literal commands
/// (or, with `allow_compound_commands`, every segment of a flat literal `&&` chain).
fn bash_patterns(cfg: &BashCfg, command: &str) -> Option<RuleApproval> {
    let rules = &cfg.patterns;
    if rules.is_empty() || command.trim().is_empty() {
        return None;
    }
    let segs = segments(command);
    let chain = cfg.allow_compound_commands.then(|| literal_chain(command)).flatten();
    if let Some(chain) = chain {
        let mut prompt = false;
        let mut all_allow = true;
        for segment in &chain {
            match match_rule(rules, &cfg.allowed_env_vars, segment) {
                Some(RuleApproval::Deny) => return Some(RuleApproval::Deny),
                Some(RuleApproval::Prompt) => prompt = true,
                Some(RuleApproval::Allow) => {}
                None => all_allow = false,
            }
        }
        match match_rule(rules, &cfg.allowed_env_vars, command.trim()) {
            Some(RuleApproval::Deny) => return Some(RuleApproval::Deny),
            Some(RuleApproval::Prompt) => prompt = true,
            _ => {}
        }
        return if prompt {
            Some(RuleApproval::Prompt)
        } else if all_allow {
            Some(RuleApproval::Allow)
        } else {
            None
        };
    }
    if segs.len() <= 1 {
        return match_rule(rules, &cfg.allowed_env_vars, command.trim());
    }
    // A compound line that does not qualify: allow rules cannot approve it, but deny and prompt
    // rules are still checked per segment, with deny precedence.
    let mut prompt = false;
    if let Some(rule) = match_rule(rules, &cfg.allowed_env_vars, command.trim()) {
        match rule {
            RuleApproval::Deny => return Some(RuleApproval::Deny),
            RuleApproval::Prompt => prompt = true,
            RuleApproval::Allow => {}
        }
    }
    for (segment, _) in &segs {
        match match_rule(rules, &cfg.allowed_env_vars, segment) {
            Some(RuleApproval::Deny) => return Some(RuleApproval::Deny),
            Some(RuleApproval::Prompt) => prompt = true,
            _ => {}
        }
    }
    prompt.then_some(RuleApproval::Prompt)
}

fn match_rule(rules: &[BashRule], allowed_env: &[String], text: &str) -> Option<RuleApproval> {
    for rule in rules {
        if wildcard(&rule.pattern, text) {
            return Some(rule.approval);
        }
        let allowed = |name: &str| allowed_env.iter().any(|v| v == name) || rule.env.iter().any(|v| v == name);
        let stripped = match rule.approval {
            RuleApproval::Allow => strip_env(text, &allowed),
            _ => strip_env(text, &|_| true),
        };
        if stripped != text && wildcard(&rule.pattern, &stripped) {
            return Some(rule.approval);
        }
    }
    None
}

/// Drops leading `NAME=value` assignments that `allowed` accepts.
pub(crate) fn strip_env(text: &str, allowed: &impl Fn(&str) -> bool) -> String {
    let mut rest = text.trim_start();
    loop {
        let token = rest.split_whitespace().next().unwrap_or("");
        let Some((name, _)) = token.split_once('=') else { break };
        if name.is_empty() || name.starts_with(|c: char| c.is_ascii_digit())
            || !name.chars().all(|c| c == '_' || c.is_ascii_alphanumeric())
            || !allowed(name)
        {
            break;
        }
        rest = rest[token.len()..].trim_start();
    }
    rest.to_string()
}

/// A flat `&&` chain of literal commands, or `None` when the line needs a real shell parser.
fn literal_chain(command: &str) -> Option<Vec<String>> {
    if command.contains('\n') {
        return None;
    }
    let mut segs = Vec::new();
    let mut cur = String::new();
    let mut quote = None;
    let mut chars = command.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\'' | '"' if quote.is_none() => {
                quote = Some(c);
                cur.push(c);
            }
            c if quote == Some(c) => {
                quote = None;
                cur.push(c);
            }
            '&' if quote.is_none() && chars.peek() == Some(&'&') => {
                chars.next();
                segs.push(cur.trim().to_string());
                cur.clear();
            }
            c if quote.is_none()
                && matches!(c, '&' | '|' | ';' | '<' | '>' | '(' | ')' | '`' | '$' | '*' | '?' | '[' | ']' | '{' | '}' | '\\') =>
            {
                return None
            }
            _ => cur.push(c),
        }
    }
    if quote.is_some() {
        return None;
    }
    let last = cur.trim();
    if !last.is_empty() {
        segs.push(last.to_string());
    }
    if segs.len() < 2 {
        return None;
    }
    for segment in &segs {
        let first = segment.split_whitespace().next()?;
        if first.contains('=')
            || matches!(
                first,
                "cd" | "source" | "." | "eval" | "exec" | "export" | "unset" | "alias" | "set" | "shopt" | "trap" | "umask"
                    | "pushd" | "popd" | "read" | "exit" | "return"
            )
        {
            return None;
        }
    }
    Some(segs)
}

/// `*` matches any run of characters; whitespace runs are normalized before matching.
fn wildcard(pattern: &str, text: &str) -> bool {
    let normalize = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ");
    let (p, t) = (normalize(pattern), normalize(text));
    let (pb, tb) = (p.as_bytes(), t.as_bytes());
    let (mut pi, mut ti) = (0, 0);
    let (mut star, mut mark) = (None, 0);
    while ti < tb.len() {
        if pi < pb.len() && pb[pi] == tb[ti] {
            pi += 1;
            ti += 1;
        } else if pi < pb.len() && pb[pi] == b'*' {
            star = Some(pi);
            pi += 1;
            mark = ti;
        } else if let Some(s) = star {
            pi = s + 1;
            mark += 1;
            ti = mark;
        } else {
            return false;
        }
    }
    while pi < pb.len() && pb[pi] == b'*' {
        pi += 1;
    }
    pi == pb.len()
}

fn inside_project(cwd: &Path, arg: &str) -> bool {
    let target = physical(&resolve(cwd, arg));
    target.starts_with(physical(cwd)) || target.starts_with(physical(&crate::home()))
}

/// A shell command that only looks: known read-only programs, joined at most by `|`, `&&`, `||` or `;`,
/// with no redirection, substitution, background `&`, quoting or expansion, and every path argument inside
/// the project. These never ask, like the `read` tool.
pub fn read_only_command(cmd: &str, cwd: &Path) -> bool {
    // Quotes and expansions make the words the shell runs differ from the words checked here.
    if cmd.trim().is_empty() || cmd.contains(|c| "`$><()\n\\'\"{}*?[".contains(c)) {
        return false;
    }
    let unified = cmd.replace("&&", "|").replace("||", "|").replace(';', "|");
    if unified.contains('&') {
        return false;
    }
    unified.split('|').all(|seg| read_only_segment(seg, cwd))
}

fn read_only_segment(seg: &str, cwd: &Path) -> bool {
    let words: Vec<&str> = seg.split_whitespace().collect();
    let Some((&prog, args)) = words.split_first() else { return false; };
    // `long`: refused long options, also as `--opt=value` and as an unambiguous abbreviation (getopt, git).
    // `short`: refused short option letters, also grouped or with the value glued on (`-no`, `-ofile`).
    let flags_ok = |long: &[&str], short: &str| !args.iter().any(|a| {
        let name = a.split('=').next().unwrap_or(a);
        long.iter().any(|b| name == *b || (name.len() > 3 && name.starts_with("--") && b.starts_with(name)))
            || (a.starts_with('-') && !a.starts_with("--") && a[1..].contains(|c| short.contains(c)))
    });
    let paths = args.iter().filter(|a| !a.starts_with('-')).count();
    let ok = match prog {
        "ls" | "pwd" | "cat" | "head" | "tail" | "wc" | "file" | "stat" | "which" | "echo" | "grep" | "egrep" | "fgrep" | "diff" | "du" | "df"
        | "date" | "whoami" | "uname" | "cut" | "tr" | "basename" | "dirname" | "realpath" | "jq" | "true" | "nl" | "column" => true,
        // `cd` alone goes to the home directory, outside the project.
        "cd" => paths == 1,
        // A second operand is the output file.
        "uniq" => paths <= 1,
        "tree" => flags_ok(&[], "o"),
        "rg" => flags_ok(&["--pre", "--pre-glob"], ""),
        "sort" => flags_ok(&["--output", "--compress-program"], "o"),
        "find" => flags_ok(&["-exec", "-execdir", "-ok", "-okdir", "-delete", "-fprint", "-fprint0", "-fprintf", "-fls"], ""),
        "git" => match args.iter().find(|a| !a.starts_with('-')).copied() {
            Some("status" | "diff" | "log" | "show" | "rev-parse" | "ls-files" | "blame" | "describe" | "shortlog" | "grep") => flags_ok(&["--output", "--ext-diff", "--open-files-in-pager"], "O"),
            Some("branch") => args.iter().all(|a| matches!(*a, "branch" | "-a" | "-r" | "-v" | "-vv" | "--list" | "--show-current")),
            Some("remote") => args.iter().all(|a| matches!(*a, "remote" | "-v")),
            _ => false,
        },
        _ => false,
    };
    // Paths may also hide in `--opt=value`.
    ok && args.iter().map(|a| if a.starts_with('-') { a.split_once('=').map_or("", |(_, v)| v) } else { a }).all(|a| {
        !a.starts_with('~') && (!(a.starts_with('/') || a.contains("..")) || inside_project(cwd, a))
    })
}

/// One line describing a tool call for the approval prompt.
/// Tools that cannot change anything, and so stay available in plan mode.
pub fn read_only(tool: &str, input: &Value) -> bool {
    matches!(tool, "read" | "grep" | "glob" | "web_fetch" | "web_search" | "todo" | "task" | "advisor" | "think")
        || (tool == "lsp" && input["action"] != "rename")
        || (tool == "mcp" && input["action"] == "search")
}

impl Perms {
    /// Stored "always allow" keys.
    pub fn allowed(&self) -> Vec<String> {
        self.allow.lock().unwrap().iter().cloned().collect()
    }

    /// Removes a key here and in the file. Other threads loaded their own copy, so call it on each.
    pub fn revoke(&self, key: &str) -> std::io::Result<()> {
        self.allow.lock().unwrap().remove(key);
        let kept: String = std::fs::read_to_string(allow_file()).unwrap_or_default().lines().filter(|k| *k != key).map(|k| format!("{k}\n")).collect();
        std::fs::write(allow_file(), kept)
    }
}

pub fn summary(tool: &str, input: &Value) -> String {
    // Native tools and external agents (opencode `shell`, `filePath`, Claude `file_path`) name their arguments differently.
    let first = |keys: &[&str]| keys.iter().find_map(|k| input[*k].as_str().filter(|v| !v.is_empty())).map(String::from);
    let found = match tool {
        "bash" => first(&["command"]),
        "read" | "write" | "edit" => first(&["path"]),
        "ext" => {
            let inner = ["path", "command", "code", "pattern", "question", "action", "text", "scope"].iter().find_map(|k| input["args"][*k].as_str().filter(|v| !v.is_empty())).unwrap_or("");
            Some(format!("{} {inner}", input["tool"].as_str().unwrap_or("")).trim().to_string())
        }
        _ => first(&["command", "path", "filePath", "file_path", "url", "pattern", "query", "code", "program", "question"]),
    };
    found.unwrap_or_else(|| if input.is_null() || input.as_object().is_some_and(|o| o.is_empty()) { String::new() } else { input.to_string() })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn rule(pattern: &str, approval: RuleApproval) -> BashRule {
        BashRule { pattern: pattern.into(), approval, env: Vec::new() }
    }

    #[test]
    fn restricted_folders_follow_traversal_symlinks_and_cwd() {
        let root = tempfile::tempdir().unwrap();
        let secret = root.path().join("secret");
        let work = root.path().join("work");
        let sibling = root.path().join("secret2");
        for dir in [&secret, &work, &sibling] {
            std::fs::create_dir(dir).unwrap();
        }
        std::fs::write(secret.join("key.txt"), "secret").unwrap();
        let canon = secret.canonicalize().unwrap();
        let folders = ["../secret".to_string()];

        let read = json!({"path": "../secret/key.txt"});
        assert_eq!(restricted_dir(&folders, "read", &read, &work), Some(canon.clone()));
        // `/a/foo` never covers `/a/foobar`.
        let neighbour = json!({"path": "../secret2/key.txt"});
        assert_eq!(restricted_dir(&folders, "read", &neighbour, &work), None);
        // Commands are matched by the directory they run in.
        assert!(restricted_dir(&folders, "bash", &json!({}), &secret).is_some());
        assert!(restricted_dir(&folders, "bash", &json!({}), &work).is_none());
        // A search rooted above a restricted folder reads into it; one rooted in a sibling does not.
        assert!(restricted_dir(&folders, "grep", &json!({"path": ".."}), &work).is_some());
        assert!(restricted_dir(&folders, "grep", &json!({"path": "."}), &work).is_none());

        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(secret.join("key.txt"), work.join("link.txt")).unwrap();
            let link = json!({"path": "link.txt"});
            assert_eq!(restricted_dir(&folders, "read", &link, &work), Some(canon));
        }
    }

    #[test]
    fn compound_rules_allow_only_literal_and_chains() {
        let mut cfg = BashCfg {
            allow_compound_commands: true,
            patterns: vec![
                rule("cmp *", RuleApproval::Allow),
                rule("rm -f *", RuleApproval::Allow),
                rule("git push *", RuleApproval::Deny),
                rule("make *", RuleApproval::Prompt),
            ],
            ..BashCfg::default()
        };
        assert_eq!(bash_patterns(&cfg, "cmp a && rm -f a"), Some(RuleApproval::Allow));
        assert_eq!(bash_patterns(&cfg, "cmp 'a b' && rm -f 'a b'"), Some(RuleApproval::Allow));
        // Unmatched segments keep the normal policy.
        assert_eq!(bash_patterns(&cfg, "cmp a && touch b"), None);
        // Deny wins across the chain.
        assert_eq!(bash_patterns(&cfg, "git push origin && rm -f a"), Some(RuleApproval::Deny));
        assert_eq!(bash_patterns(&cfg, "make all"), Some(RuleApproval::Prompt));
        // Redirection or another operator is not a literal chain: allow cannot approve it.
        assert_eq!(bash_patterns(&cfg, "echo x > y && rm -f a"), None);
        cfg.allow_compound_commands = false;
        assert_eq!(bash_patterns(&cfg, "cmp a && rm -f a"), None);
    }

    #[test]
    fn leading_environment_assignments_need_allowed_names() {
        let cargo = BashRule {
            pattern: "cargo test *".into(),
            approval: RuleApproval::Allow,
            env: Vec::new(),
        };
        let mut cfg = BashCfg {
            patterns: vec![cargo, rule("rm *", RuleApproval::Deny)],
            allowed_env_vars: vec!["CARGO_TARGET_DIR".into()],
            ..BashCfg::default()
        };
        assert_eq!(bash_patterns(&cfg, "CARGO_TARGET_DIR=/tmp cargo test x"), Some(RuleApproval::Allow));
        assert_eq!(bash_patterns(&cfg, "LD_PRELOAD=x cargo test x"), None);
        // Deny and prompt rules see through any leading assignment.
        assert_eq!(bash_patterns(&cfg, "FOO=1 rm x"), Some(RuleApproval::Deny));
        cfg.allowed_env_vars.clear();
        assert_eq!(bash_patterns(&cfg, "CARGO_TARGET_DIR=/tmp cargo test x"), None);
    }

    #[test]
    fn segments_respect_quotes_and_pipes() {
        assert_eq!(
            segments("echo \"a;b\" | grep a && rm -f x"),
            vec![("echo \"a;b\"".into(), false), ("grep a".into(), true), ("rm -f x".into(), false)]
        );
    }
}
