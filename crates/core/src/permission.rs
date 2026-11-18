use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::BTreeSet,
    io::Write,
    path::{Component, Path, PathBuf},
    sync::Mutex,
};

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

    /// `None`: run it. `Some(key)`: ask the user; `key` is what "always allow" stores.
    pub fn check(&self, tool: &str, input: &Value, cwd: &Path) -> Option<String> {
        let mode = self.mode();
        if mode == Mode::Full {
            return None;
        }
        let path = resolve(cwd, input["path"].as_str().unwrap_or(""));
        let target = physical(&path);
        let inside = !input["path"].as_str().is_some_and(|s| s.contains("://")) && (target.starts_with(physical(cwd)) || target.starts_with(physical(&crate::home())));
        let key = match tool {
            "task" | "todo" | "web_search" | "advisor" => return None,
            "memory" => format!("memory:{}", input["scope"].as_str().unwrap_or("project")),
            "web_fetch" => format!("web_fetch:{}", reqwest::Url::parse(input["url"].as_str().unwrap_or("")).ok().and_then(|u| u.host_str().map(String::from)).unwrap_or_default()),
            "read" | "grep" | "glob" => {
                if inside || path.starts_with(crate::user_home().join(".claude")) {
                    return None;
                }
                format!("read:{}", path.display())
            }
            "write" | "edit" => {
                // `.git/config` and hooks run commands, and read-only `git status` would then run them unasked.
                // APFS is case-insensitive, and a symlink can point at `.git`: check the resolved path, any case.
                if inside && mode == Mode::AutoEdit && !target.components().any(|c| c.as_os_str().eq_ignore_ascii_case(".git")) {
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
        if key.contains('\n') {
            return; // one key per line
        }
        if self.allow.lock().unwrap().insert(key.clone()) {
            let _ = std::fs::create_dir_all(crate::home());
            if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(allow_file()) {
                let _ = writeln!(f, "{key}");
            }
        }
    }
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
    matches!(tool, "read" | "grep" | "glob" | "web_fetch" | "web_search" | "todo" | "task" | "lsp" | "advisor") || (tool == "mcp" && input["action"] == "search")
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
