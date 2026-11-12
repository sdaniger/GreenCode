use serde::Deserialize;
use serde_json::Value;
use std::{
    collections::BTreeSet,
    io::Write,
    path::{Component, Path, PathBuf},
    sync::Mutex,
};

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Default)]
#[serde(rename_all = "kebab-case")]
pub enum Mode {
    /// Ask before every write and command.
    #[default]
    Ask,
    /// File edits inside the project run without asking.
    AutoEdit,
    /// Nothing asks.
    Full,
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
        // shortcut: lexical check only, a symlink inside the project can point outside; resolve with canonicalize if that matters.
        let path = resolve(cwd, input["path"].as_str().unwrap_or(""));
        let inside = path.starts_with(cwd) || path.starts_with(crate::home());
        let key = match tool {
            "memory" | "task" => return None,
            "read" => {
                if inside || path.starts_with(crate::user_home().join(".claude")) {
                    return None;
                }
                format!("read:{}", path.display())
            }
            "write" | "edit" => {
                if inside && mode == Mode::AutoEdit {
                    return None;
                }
                if inside { tool.to_string() } else { format!("{tool}:{}", path.display()) }
            }
            "bash" => {
                let cmd = input["command"].as_str().unwrap_or("");
                // Only a plain command may be allowed by its program name; anything
                // with shell operators must match exactly.
                if cmd.contains(|c| ";&|`$><\n(".contains(c)) {
                    format!("bash={cmd}")
                } else {
                    format!("bash:{}", cmd.split_whitespace().next().unwrap_or(""))
                }
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

/// One line describing a tool call for the approval prompt.
pub fn summary(tool: &str, input: &Value) -> String {
    match tool {
        "bash" => input["command"].as_str().unwrap_or("").to_string(),
        "read" | "write" | "edit" => input["path"].as_str().unwrap_or("").to_string(),
        _ => input.to_string(),
    }
}
