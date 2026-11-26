//! Disassembler settings: `disassembler.*`, `ida.*`, `ghidra.*`, `binja.*`; flattened into `Config`.
use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::PathBuf;

#[derive(Deserialize, Serialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct DisassemblerOpts {
    pub disassembler: DisassemblerCfg,
    pub ida: IdaCfg,
    pub ghidra: GhidraCfg,
    pub binja: BinjaCfg,
}

#[derive(Deserialize, Serialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct DisassemblerCfg {
    /// `auto` picks IDA when installed, else Ghidra; `binja` is never automatic.
    pub backend: Backend,
    /// Databases open at once for one project; the least recently used unreferenced one is closed first.
    pub max_open: u32,
    /// Save and close unreferenced databases idle longer than this many seconds; 0 never closes them.
    pub idle_close_sec: u64,
}

#[derive(Deserialize, Serialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Backend {
    #[default]
    Auto,
    Ghidra,
    Ida,
    Binja,
    None,
}

impl Backend {
    /// `None` when the setting cannot name a backend; used by `config set` and the GUI's chips.
    pub fn parse(text: &str) -> Option<Backend> {
        match text.trim() {
            "auto" => Some(Backend::Auto),
            "ghidra" => Some(Backend::Ghidra),
            "ida" => Some(Backend::Ida),
            "binja" => Some(Backend::Binja),
            "none" => Some(Backend::None),
            _ => None,
        }
    }
}

impl Default for DisassemblerCfg {
    fn default() -> Self {
        Self { backend: Backend::Auto, max_open: 4, idle_close_sec: 900 }
    }
}

#[derive(Deserialize, Serialize, Clone, Debug, Default, PartialEq)]
#[serde(default)]
pub struct IdaCfg {
    /// Python that can import `ida_domain` and `idapro`; empty auto-detects.
    pub python: String,
    /// Directory containing `libidalib`, exported as `IDADIR`; empty auto-detects.
    pub install_dir: String,
}

#[derive(Deserialize, Serialize, Clone, Debug, Default, PartialEq)]
#[serde(default)]
pub struct GhidraCfg {
    /// Ghidra 12+ install directory; empty uses `$GHIDRA_INSTALL_DIR`, Homebrew, else a managed download.
    pub install_dir: String,
    /// Python that can import `pyghidra`; empty uses a managed venv built from the install's bundled wheels.
    pub python: String,
}

#[derive(Deserialize, Serialize, Clone, Debug, Default, PartialEq)]
#[serde(default)]
pub struct BinjaCfg {
    /// Directory containing the Binary Ninja core library, or the macOS `.app` bundle; empty auto-detects.
    pub install_dir: String,
    /// Python 3 that can load the API from the install's python directory; empty auto-detects.
    pub python: String,
}

impl Default for DisassemblerOpts {
    fn default() -> Self {
        Self { disassembler: DisassemblerCfg::default(), ida: IdaCfg::default(), ghidra: GhidraCfg::default(), binja: BinjaCfg::default() }
    }
}

/// OMP key -> native dotted key; `config import-omp` copies the value verbatim unless `convert` handles it.
pub(super) const OMP: &[(&str, &str)] = &[
    ("disassembler.backend", "disassembler.backend"),
    ("disassembler.maxOpen", "disassembler.max_open"),
    ("disassembler.idleCloseSec", "disassembler.idle_close_sec"),
    ("ida.python", "ida.python"),
    ("ida.installDir", "ida.install_dir"),
    ("ghidra.installDir", "ghidra.install_dir"),
    ("ghidra.python", "ghidra.python"),
    ("binja.installDir", "binja.install_dir"),
    ("binja.python", "binja.python"),
];

/// Translates an OMP value whose shape or units differ from the native field; `None` copies it verbatim.
pub(super) fn convert(from: &str, value: &Value) -> Option<Result<toml::Value>> {
    match from {
        // Accepted spellings are already native ones, but an unknown backend must fail the import.
        "disassembler.backend" => Some(match value.as_str().and_then(Backend::parse) {
            Some(_) => Ok(toml::Value::String(value.as_str().unwrap_or_default().into())),
            None => Err(anyhow::anyhow!("disassembler.backend must be auto, ghidra, ida, binja or none")),
        }),
        _ => None,
    }
}

impl DisassemblerOpts {
    /// `~` in a configured path is the user's home, as everywhere else in `config.toml`.
    pub fn path(text: &str) -> PathBuf {
        match text.trim() {
            "" => PathBuf::new(),
            t => match t.strip_prefix("~/") {
                Some(rest) => crate::user_home().join(rest),
                None => PathBuf::from(t),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_and_omp_spellings_round_trip() {
        let cfg = crate::config::Config::default();
        let d = &cfg.disasm_opts;
        assert_eq!(d.disassembler.backend, Backend::Auto);
        assert_eq!((d.disassembler.max_open, d.disassembler.idle_close_sec), (4, 900));
        let tree = serde_json::to_value(&cfg).unwrap();
        assert_eq!(tree["disassembler"]["backend"], "auto");
        assert_eq!(tree["disassembler"]["maxOpen".to_string()], Value::Null);
        for (text, want) in [
            ("auto", Backend::Auto),
            ("ghidra", Backend::Ghidra),
            ("ida", Backend::Ida),
            ("binja", Backend::Binja),
            ("none", Backend::None),
        ] {
            assert_eq!(Backend::parse(text), Some(want), "{text}");
            assert_eq!(serde_json::to_value(want).unwrap(), text);
        }
        assert_eq!(Backend::parse("IDA"), None);
        let table = crate::config::settings::omp::translate(
            "disassembler:\n  backend: binja\n  maxOpen: 8\n  idleCloseSec: 300\nida:\n  installDir: /opt/ida\n  python: /opt/ida/python3\nbinja:\n  python: /usr/bin/python3\n",
        )
        .unwrap();
        let cfg = crate::config::settings::decode(toml::Value::Table(table)).unwrap();
        let d = &cfg.disasm_opts;
        assert_eq!(d.disassembler.backend, Backend::Binja);
        assert_eq!((d.disassembler.max_open, d.disassembler.idle_close_sec), (8, 300));
        assert_eq!((d.ida.install_dir.as_str(), d.ida.python.as_str()), ("/opt/ida", "/opt/ida/python3"));
        assert_eq!(d.binja.python, "/usr/bin/python3");
        assert!(crate::config::settings::omp::translate("disassembler:\n  backend: nope\n").is_err());
    }

    #[test]
    fn tilde_paths_expand_against_the_user_home() {
        let home = crate::user_home();
        assert_eq!(DisassemblerOpts::path("~/tools/ida"), home.join("tools/ida"));
        assert_eq!(DisassemblerOpts::path("/opt/ida"), PathBuf::from("/opt/ida"));
        assert_eq!(DisassemblerOpts::path("  "), PathBuf::new());
    }
}
