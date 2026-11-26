//! Disassembly backends. `read` shows on-disk ELF/PE/Mach-O metadata and analyses on request;
//! `dsm` drives one Python worker per database. Backends: IDA (idalib), Ghidra (PyGhidra) and
//! Binary Ninja, selected by `disassembler.backend` (see `config::disassembler`).

use crate::config::disassembler::DisassemblerOpts;
use crate::config::Config;
use anyhow::{anyhow, bail, ensure, Context, Result};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::LazyLock;
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout};
use tokio::sync::Mutex;

pub use crate::config::disassembler::Backend;

const WORKER: &str = include_str!("disasm/worker.py");
const OPEN_TIMEOUT: Duration = Duration::from_secs(3600);
const CALL_TIMEOUT: Duration = Duration::from_secs(120);
const EXEC_TIMEOUT: Duration = Duration::from_secs(300);

/// Everything needed to start one database's worker. `script` is the bundled `worker.py`, or the
/// file in `GREENCODE_DISASM_WORKER` (tests and debugging).
#[derive(Clone, Debug)]
pub struct Invocation {
    pub backend: Backend,
    pub python: PathBuf,
    pub env: Vec<(String, String)>,
    pub script: PathBuf,
}

fn random_hex(bytes: usize) -> String {
    let mut raw = vec![0u8; bytes];
    let _ = getrandom::fill(&mut raw);
    raw.iter().map(|b| format!("{b:02x}")).collect()
}

fn bundled_script() -> Result<PathBuf> {
    let path = std::env::temp_dir().join(format!("greencode-dsm-{}-{}.py", std::process::id(), random_hex(4)));
    std::fs::write(&path, WORKER).with_context(|| path.display().to_string())?;
    Ok(path)
}

pub fn label(backend: Backend) -> &'static str {
    match backend {
        Backend::Auto => "Auto",
        Backend::Ida => "IDA",
        Backend::Ghidra => "Ghidra",
        Backend::Binja => "Binary Ninja",
        Backend::None => "None",
    }
}

/// Which backend a configured choice resolves to, given what is installed.
pub fn pick(chosen: Backend, ida: bool, ghidra: bool, binja: bool) -> Option<Backend> {
    match chosen {
        Backend::Auto => [Backend::Ida, Backend::Ghidra].into_iter().find(|&b| match b {
            Backend::Ida => ida,
            _ => ghidra,
        }),
        Backend::Ida => ida.then_some(Backend::Ida),
        Backend::Ghidra => ghidra.then_some(Backend::Ghidra),
        Backend::Binja => binja.then_some(Backend::Binja),
        Backend::None => None,
    }
}

/// The backend and interpreter to run. `Auto` prefers IDA over Ghidra and never Binary Ninja.
pub async fn runtime(cfg: &Config) -> Result<Invocation> {
    let chosen = cfg.disasm_opts.disassembler.backend;
    ensure!(chosen != Backend::None, "disassembly is disabled by `disassembler.backend = \"none\"`");
    if let Some(script) = std::env::var_os("GREENCODE_DISASM_WORKER") {
        let python = std::env::var_os("GREENCODE_DISASM_PYTHON").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("python3"));
        let backend = if chosen == Backend::Auto { Backend::Ghidra } else { chosen };
        return Ok(Invocation { backend, python, env: Vec::new(), script: PathBuf::from(script) });
    }
    let opts = &cfg.disasm_opts;
    let ida = (chosen != Backend::Binja).then(|| ida_install(opts)).flatten();
    let binja = (chosen == Backend::Binja).then(|| binja_install(opts)).flatten();
    let ghidra_possible = chosen != Backend::Ida && chosen != Backend::Binja;
    let backend = pick(chosen, ida.is_some(), ghidra_possible, binja.is_some()).ok_or_else(|| match chosen {
        Backend::Auto => anyhow!("no disassembler is available: no IDA install with idalib, and Ghidra needs `ghidra.install_dir`, $GHIDRA_INSTALL_DIR or Homebrew"),
        Backend::Ida => anyhow!("no IDA install with idalib found (`ida.install_dir`, $IDADIR, ida-config.json, standard paths)"),
        Backend::Binja => anyhow!("no Binary Ninja install with the core library found (`binja.install_dir`, $BN_INSTALL_DIR, lastrun, default locations)"),
        _ => unreachable!(),
    })?;
    let script = bundled_script()?;
    match backend {
        Backend::Ida => {
            let dir = ida.expect("picked IDA");
            let env = vec![
                ("IDADIR".into(), dir.display().to_string()),
                ("PYTHONPATH".into(), prepend_path(&dir.join("idalib").join("python"))),
            ];
            let probe = "import importlib.util as u,sys;sys.exit(0 if u.find_spec('ida_domain') and u.find_spec('idapro') else 3)";
            let python = probe_python(&opts.ida.python, &["python3", "python"], &env, probe).await.ok_or_else(|| anyhow!(
                "no Python interpreter can import ida_domain and idapro (tried: {}). Install ida-domain into the Python targeted by IDA's idapyswitch, or set `ida.python`",
                if opts.ida.python.trim().is_empty() { "python3, python".into() } else { opts.ida.python.clone() }
            ))?;
            Ok(Invocation { backend, python, env, script })
        }
        Backend::Ghidra => {
            let (dir, managed) = ghidra_install(opts).await?;
            let python = match probe_python(&opts.ghidra.python, &["python3", "python"], &[], "import pyghidra").await {
                Some(python) => python,
                None => pyghidra_venv(&dir, managed).await?.ok_or_else(|| anyhow!(
                    "ghidra.python cannot import pyghidra and no managed environment could be built from {}; the install's PyGhidra wheels are missing or no Python 3.9+ is available",
                    dir.display()
                ))?,
            };
            let mut env = vec![("GHIDRA_INSTALL_DIR".into(), dir.display().to_string())];
            if let Some(java) = java_home() {
                env.push(("JAVA_HOME".into(), java));
            } else {
                bail!("Ghidra needs a JDK 21+; set JAVA_HOME or put `java` on PATH")
            }
            Ok(Invocation { backend, python, env, script })
        }
        Backend::Binja => {
            let dir = binja.expect("picked Binary Ninja");
            let py_dir = binja_python_dir(&dir);
            let env = vec![
                ("BN_INSTALL_DIR".into(), dir.display().to_string()),
                ("PYTHONPATH".into(), prepend_path(&py_dir)),
            ];
            let probe = "import binaryninja as b; b.BinaryView.new(b'\\x00')";
            let python = probe_python(&opts.binja.python, &["python3", "python"], &env, probe).await.ok_or_else(|| anyhow!(
                "no Python interpreter can import binaryninja from {} (tried: {}). Set `binja.python` to a Python 3 interpreter supported by this release",
                py_dir.display(),
                if opts.binja.python.trim().is_empty() { "python3, python".into() } else { opts.binja.python.clone() }
            ))?;
            Ok(Invocation { backend, python, env, script })
        }
        _ => unreachable!(),
    }
}

/// A configured interpreter wins, but only if it really imports the backend; otherwise the first
/// candidate that does.
async fn probe_python(explicit: &str, candidates: &[&str], env: &[(String, String)], probe: &str) -> Option<PathBuf> {
    let explicit = explicit.trim();
    let list: Vec<&str> = if explicit.is_empty() { candidates.to_vec() } else { vec![explicit] };
    for python in list {
        let mut command = tokio::process::Command::new(python);
        command.arg("-c").arg(probe).envs(env.iter().map(|(k, v)| (k, v)));
        command.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
        if let Ok(Ok(status)) = tokio::time::timeout(Duration::from_secs(120), command.status()).await {
            if status.success() {
                return Some(PathBuf::from(python));
            }
        }
    }
    None
}

fn prepend_path(extra: &Path) -> String {
    match std::env::var("PYTHONPATH").ok().filter(|p| !p.is_empty()) {
        Some(existing) => format!("{}{}{existing}", extra.display(), if cfg!(windows) { ';' } else { ':' }),
        None => extra.display().to_string(),
    }
}

fn file_name_starts_with(prefix: &str, name: &str) -> bool {
    name.to_lowercase().starts_with(prefix)
}

/// Entries of `dir` whose name starts with `prefix`, newest version first, with `suffix` appended.
fn versioned_dirs(dir: &Path, prefix: &str, suffix: &str) -> Vec<PathBuf> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| entry.file_name().to_str().map(str::to_string))
        .filter(|name| file_name_starts_with(prefix, name))
        .collect();
    names.sort_by(|a, b| version_cmp(b, a));
    names.into_iter().map(|name| dir.join(name).join(suffix)).collect()
}

fn version_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    let parts = |s: &str| -> Vec<u64> { s.split(|c: char| !c.is_ascii_digit()).filter_map(|p| p.parse().ok()).collect() };
    parts(a).cmp(&parts(b))
}

// --- IDA --------------------------------------------------------------------

fn ida_core_library() -> &'static str {
    if cfg!(target_os = "macos") {
        "libidalib.dylib"
    } else if cfg!(windows) {
        "idalib.dll"
    } else {
        "libidalib.so"
    }
}

/// A directory containing `libidalib`: what `IDADIR` is, and where the worker's Python bindings live.
pub fn ida_install(opts: &DisassemblerOpts) -> Option<PathBuf> {
    let valid = |dir: &Path| dir.join("idalib").join(ida_core_library()).is_file();
    let configured = DisassemblerOpts::path(&opts.ida.install_dir);
    if !configured.as_os_str().is_empty() {
        return valid(&configured).then_some(configured);
    }
    let home = crate::user_home();
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(dir) = std::env::var_os("IDADIR") {
        candidates.push(PathBuf::from(dir));
    }
    candidates.extend(ida_config_installs());
    if cfg!(target_os = "macos") {
        candidates.extend(versioned_dirs(Path::new("/Applications"), "ida", "Contents/MacOS"));
        candidates.extend(versioned_dirs(&home.join("Applications"), "ida", "Contents/MacOS"));
    } else if cfg!(windows) {
        let program_files = std::env::var_os("ProgramFiles").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("C:\\Program Files"));
        candidates.extend(versioned_dirs(&program_files, "ida", ""));
    } else {
        candidates.extend(versioned_dirs(&home, "ida", ""));
        candidates.extend(versioned_dirs(Path::new("/opt"), "ida", ""));
    }
    candidates.into_iter().find(|dir| valid(dir))
}

/// `ida-config.json` under `$IDAUSR` (metadata paths, e.g. an AppImage) and `~/.idapro`.
fn ida_config_installs() -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = std::env::var_os("IDAUSR").map(|raw| std::env::split_paths(&raw).collect()).unwrap_or_default();
    dirs.push(crate::user_home().join(".idapro"));
    dirs.into_iter()
        .filter_map(|dir| std::fs::read_to_string(dir.join("ida-config.json")).ok())
        .filter_map(|text| serde_json::from_str::<Value>(&text).ok())
        .filter_map(|value| value.pointer("/Paths/ida-install-dir").and_then(Value::as_str).map(PathBuf::from))
        .collect()
}

// --- Ghidra -----------------------------------------------------------------

const GHIDRA_VERSION: &str = "12.2";
const GHIDRA_TAG: &str = "Ghidra_12.2_build";
const GHIDRA_ASSET: &str = "ghidra_12.2_PUBLIC_20261016.zip";
const GHIDRA_SHA256: &str = "2c2cd776db3c1f6a097ba418922378ac3649cadd5e844a6f5f088ae628a7be67";
const GHIDRA_DIR: &str = "ghidra_12.2_PUBLIC";

fn ghidra_version(dir: &Path) -> Option<u32> {
    let text = std::fs::read_to_string(dir.join("Ghidra").join("application.properties")).ok()?;
    text.lines().find_map(|line| {
        let rest = line.strip_prefix("application.version")?.trim_start().strip_prefix('=')?;
        rest.trim().split('.').next()?.parse().ok()
    })
}

fn ghidra_valid(dir: &Path) -> bool {
    dir.join("Ghidra").join("Features").join("PyGhidra").join("lib").join("PyGhidra.jar").is_file()
        && ghidra_version(dir).is_some_and(|major| major >= 12)
}

/// A Ghidra 12+ install with PyGhidra: config, `$GHIDRA_INSTALL_DIR`, Homebrew, else the managed
/// download under `~/.greencode/ghidra`. `true` means managed, so a PyGhidra venv may be built.
pub async fn ghidra_install(opts: &DisassemblerOpts) -> Result<(PathBuf, bool)> {
    let configured = DisassemblerOpts::path(&opts.ghidra.install_dir);
    if !configured.as_os_str().is_empty() {
        ensure!(ghidra_valid(&configured), "`ghidra.install_dir` {} is not a Ghidra 12+ install with PyGhidra", configured.display());
        return Ok((configured, false));
    }
    if let Some(dir) = std::env::var_os("GHIDRA_INSTALL_DIR").map(PathBuf::from).filter(|d| d.is_dir()) {
        if ghidra_valid(&dir) {
            return Ok((dir, false));
        }
    }
    for prefix in ["/opt/homebrew", "/usr/local", "/home/linuxbrew/.linuxbrew"] {
        let dir = Path::new(prefix).join("opt/ghidra/libexec");
        if ghidra_valid(&dir) {
            return Ok((dir, false));
        }
    }
    let managed = crate::home().join("ghidra");
    let current = managed.join(GHIDRA_DIR);
    if ghidra_valid(&current) {
        return Ok((current, true));
    }
    download_ghidra(&managed).await?;
    Ok((current, true))
}

/// Downloads and extracts the pinned release, verifying the published SHA-256.
async fn download_ghidra(managed: &Path) -> Result<()> {
    std::fs::create_dir_all(managed)?;
    let zip = managed.join(format!("{GHIDRA_ASSET}.part"));
    let url = format!("https://github.com/NationalSecurityAgency/ghidra/releases/download/{GHIDRA_TAG}/{GHIDRA_ASSET}");
    let mut response = reqwest::Client::new()
        .get(&url)
        .timeout(Duration::from_secs(30 * 60))
        .send()
        .await
        .with_context(|| format!("cannot download Ghidra from {url}"))?
        .error_for_status()
        .with_context(|| format!("cannot download Ghidra from {url}"))?;
    let mut file = std::fs::File::create(&zip)?;
    let mut hash = Sha256::new();
    while let Some(chunk) = response.chunk().await.context("the Ghidra download was interrupted")? {
        hash.update(&chunk);
        std::io::Write::write_all(&mut file, &chunk)?;
    }
    std::io::Write::flush(&mut file)?;
    drop(file);
    let digest = format!("{:x}", hash.finalize());
    if digest != GHIDRA_SHA256 {
        let _ = std::fs::remove_file(&zip);
        bail!("Ghidra download checksum mismatch (got {digest})");
    }
    let staging = managed.join(format!(".extract-{}", random_hex(4)));
    extract_zip(&zip, &staging).await.with_context(|| format!("cannot extract {}", zip.display()))?;
    let extracted = staging.join(GHIDRA_DIR);
    if !ghidra_valid(&extracted) {
        let _ = std::fs::remove_dir_all(&staging);
        bail!("the Ghidra archive has no Ghidra 12+ install with PyGhidra")
    }
    let target = managed.join(GHIDRA_DIR);
    let _ = std::fs::remove_dir_all(&target);
    std::fs::rename(&extracted, &target)?;
    let _ = std::fs::remove_dir_all(&staging);
    let _ = std::fs::remove_file(&zip);
    std::fs::write(target.join(".greencode-ready"), GHIDRA_VERSION)?;
    Ok(())
}

async fn extract_zip(zip: &Path, into: &Path) -> Result<()> {
    std::fs::create_dir_all(into)?;
    let has = |name: &str| std::process::Command::new("sh").arg("-c").arg(format!("command -v {name}")).output().is_ok_and(|o| o.status.success());
    let (program, args) = if cfg!(target_os = "macos") && has("ditto") {
        ("ditto", vec!["-x".into(), "-k".into(), zip.display().to_string(), into.display().to_string()])
    } else if has("unzip") {
        ("unzip", vec!["-q".into(), zip.display().to_string(), "-d".into(), into.display().to_string()])
    } else {
        bail!("no unzip or ditto found to extract the Ghidra archive")
    };
    let output = tokio::process::Command::new(program).args(&args).output().await?;
    ensure!(output.status.success(), "{program} failed: {}", String::from_utf8_lossy(&output.stderr).trim());
    Ok(())
}

/// Builds (once) a venv from the install's bundled PyGhidra wheels.
async fn pyghidra_venv(install: &Path, managed: bool) -> Result<Option<PathBuf>> {
    if !managed {
        return Ok(None);
    }
    let wheels = install.join("Ghidra").join("Features").join("PyGhidra").join("pypkg").join("dist");
    let version = std::fs::read_dir(&wheels).into_iter().flatten().flatten().find_map(|entry| {
        let name = entry.file_name().to_string_lossy().to_string();
        name.strip_prefix("pyghidra-")?.strip_suffix("-py3-none-any.whl").map(str::to_string)
    });
    let Some(version) = version else { return Ok(None) };
    let venv = crate::home().join("ghidra").join(format!("pyghidra-{version}"));
    let python = venv.join(if cfg!(windows) { "Scripts/python.exe" } else { "bin/python" });
    if python.is_file() {
        return Ok(Some(python));
    }
    let base = probe_python("", &["python3", "python"], &[], "import sys;sys.exit(0 if sys.version_info >= (3, 9) else 3)")
        .await
        .ok_or_else(|| anyhow!("Python 3.9+ is needed to build the PyGhidra environment"))?;
    let created = tokio::process::Command::new(&base).arg("-m").arg("venv").arg(&venv).output().await?;
    ensure!(created.status.success(), "python3 -m venv {} failed: {}", venv.display(), String::from_utf8_lossy(&created.stderr).trim());
    let installed = tokio::process::Command::new(&python)
        .args(["-m", "pip", "install", "--no-index", "--find-links"])
        .arg(&wheels)
        .arg(format!("pyghidra=={version}"))
        .output()
        .await?;
    ensure!(
        installed.status.success(),
        "installing PyGhidra {version} from {} failed: {}",
        wheels.display(),
        String::from_utf8_lossy(&installed.stderr).trim()
    );
    Ok(Some(python))
}

fn java_home() -> Option<String> {
    if let Some(java) = std::env::var("JAVA_HOME").ok().filter(|j| !j.trim().is_empty()) {
        return Some(java);
    }
    let output = std::process::Command::new("sh").arg("-c").arg("command -v java").output().ok()?;
    let java = PathBuf::from(String::from_utf8_lossy(&output.stdout).trim());
    java.parent()?.parent().map(|p| p.display().to_string())
}

// --- Binary Ninja -----------------------------------------------------------

fn binja_core_library() -> &'static str {
    if cfg!(target_os = "macos") {
        "libbinaryninjacore.dylib"
    } else if cfg!(windows) {
        "binaryninjacore.dll"
    } else {
        "libbinaryninjacore.so.1"
    }
}

/// The directory holding the core library, from config, `$BN_INSTALL_DIR`, the user folder's
/// `lastrun`, or the default install locations.
pub fn binja_install(opts: &DisassemblerOpts) -> Option<PathBuf> {
    let configured = DisassemblerOpts::path(&opts.binja.install_dir);
    if !configured.as_os_str().is_empty() {
        if binja_core(&configured).is_none() {
            eprintln!("binja.install_dir has no Binary Ninja core library; Binary Ninja features stay hidden");
        }
        return binja_core(&configured);
    }
    let home = crate::user_home();
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(dir) = std::env::var_os("BN_INSTALL_DIR") {
        candidates.push(PathBuf::from(dir));
    }
    if let Ok(last) = std::fs::read_to_string(binja_user_dir().join("lastrun")) {
        if !last.trim().is_empty() {
            candidates.push(PathBuf::from(last.trim()));
        }
    }
    if cfg!(target_os = "macos") {
        candidates.push(PathBuf::from("/Applications/Binary Ninja.app"));
        candidates.push(home.join("Applications/Binary Ninja.app"));
    } else if cfg!(windows) {
        let local = std::env::var_os("LOCALAPPDATA").map(PathBuf::from).unwrap_or_else(|| home.join("AppData/Local"));
        let program_files = std::env::var_os("ProgramFiles").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("C:\\Program Files"));
        candidates.push(program_files.join("Vector35/BinaryNinja"));
        candidates.push(local.join("Vector35/BinaryNinja"));
        candidates.push(local.join("Programs/Vector35/BinaryNinja"));
    } else {
        candidates.push(home.join("binaryninja"));
    }
    candidates.into_iter().find_map(|dir| binja_core(&dir))
}

fn binja_user_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("BN_USER_DIRECTORY") {
        return PathBuf::from(dir);
    }
    let home = crate::user_home();
    if cfg!(target_os = "macos") {
        home.join("Library/Application Support/Binary Ninja")
    } else if cfg!(windows) {
        std::env::var_os("APPDATA").map(PathBuf::from).unwrap_or_else(|| home.join("AppData/Roaming")).join("Binary Ninja")
    } else {
        home.join(".binaryninja")
    }
}

/// The directory holding the core library: the macOS `.app` bundle resolves to `Contents/MacOS`.
fn binja_core(dir: &Path) -> Option<PathBuf> {
    if dir.join(binja_core_library()).is_file() {
        return Some(dir.to_path_buf());
    }
    if cfg!(target_os = "macos") {
        let inner = dir.join("Contents/MacOS");
        if inner.join(binja_core_library()).is_file() {
            return Some(inner);
        }
    }
    None
}

fn binja_python_dir(core: &Path) -> PathBuf {
    if cfg!(target_os = "macos") {
        core.parent().map(|macos| macos.join("../Resources/python")).unwrap_or_else(|| core.join("python"))
    } else {
        core.join("python")
    }
}

// --- worker -----------------------------------------------------------------

struct Worker {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
}

impl Worker {
    /// Starts the worker and waits for its `ready` line; the analysis runs before that line.
    async fn spawn(inv: &Invocation, binary: &Path, database: &Path, arch: &str) -> Result<(Worker, Value)> {
        let name = match inv.backend {
            Backend::Ida => "ida",
            Backend::Ghidra => "ghidra",
            Backend::Binja => "binja",
            _ => bail!("not an analysis backend"),
        };
        let mut child = tokio::process::Command::new(&inv.python)
            .arg(&inv.script)
            .arg(name)
            .arg(binary)
            .arg(database)
            .arg(arch)
            .envs(inv.env.iter().map(|(k, v)| (k, v)))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .with_context(|| format!("cannot start {} ({})", inv.python.display(), label(inv.backend)))?;
        let stdin = child.stdin.take().expect("piped stdin");
        let stdout = BufReader::new(child.stdout.take().expect("piped stdout"));
        let mut worker = Worker { child, stdin, stdout, next_id: 1 };
        let mut line = String::new();
        match tokio::time::timeout(OPEN_TIMEOUT, worker.stdout.read_line(&mut line)).await {
            Ok(Ok(0)) => bail!("{} exited right after starting", label(inv.backend)),
            Ok(Ok(_)) => {}
            Ok(Err(e)) => bail!("{} worker: {e}", label(inv.backend)),
            Err(_) => bail!(
                "{} did not finish opening the database within {}s; provisioning or analysis may still be running, retry later",
                label(inv.backend),
                OPEN_TIMEOUT.as_secs()
            ),
        }
        let ready: Value = serde_json::from_str(line.trim()).with_context(|| format!("{} worker sent an unreadable handshake", label(inv.backend)))?;
        if ready["ready"] != true {
            bail!("{}", ready["error"].as_str().unwrap_or("the database could not be opened"));
        }
        Ok((worker, ready["info"].clone()))
    }

    async fn call(&mut self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id;
        self.next_id += 1;
        self.stdin.write_all(format!("{}\n", json!({"id": id, "method": method, "params": params})).as_bytes()).await?;
        self.stdin.flush().await?;
        let timeout = if method == "exec" { EXEC_TIMEOUT } else { CALL_TIMEOUT };
        let mut line = String::new();
        let read = tokio::time::timeout(timeout, self.stdout.read_line(&mut line)).await;
        let read = match read {
            Ok(read) => read,
            Err(_) => {
                // A hung worker must not stay half-usable: kill it and let the caller reopen.
                let _ = self.child.kill().await;
                bail!("the analysis worker did not answer {method} within {}s", timeout.as_secs())
            }
        };
        match read {
            Ok(0) => bail!("the analysis worker exited"),
            Ok(_) => {}
            Err(e) => bail!("the analysis worker: {e}"),
        }
        let reply: Value = serde_json::from_str(line.trim()).context("the analysis worker sent a malformed response")?;
        ensure!(reply["id"].as_u64() == Some(id), "the analysis worker answered a different request");
        if reply["ok"] == true {
            Ok(reply.get("result").cloned().unwrap_or(Value::Null))
        } else {
            bail!("{}", reply["error"].as_str().unwrap_or("the analysis worker failed"))
        }
    }

    /// Saves when asked, then stops the worker.
    async fn shutdown(mut self, save: bool) {
        let _ = self.call("close", json!({ "save": save })).await;
        let _ = tokio::time::timeout(Duration::from_secs(10), self.child.wait()).await;
        let _ = self.child.kill().await;
    }
}

// --- databases --------------------------------------------------------------

struct Db {
    id: String,
    instance: String,
    backend: Backend,
    source: PathBuf,
    database: PathBuf,
    info: Value,
    worker: Option<Worker>,
    refs: u32,
    last_used: Instant,
}

type SharedDb = std::sync::Arc<Mutex<Db>>;
static REGISTRY: LazyLock<Mutex<Vec<SharedDb>>> = LazyLock::new(|| Mutex::new(Vec::new()));

fn db_id(source: &Path) -> String {
    let canonical = source.canonicalize().unwrap_or_else(|_| source.to_path_buf());
    let digest = Sha256::digest(canonical.display().to_string().as_bytes());
    let hash: String = digest.iter().take(8).map(|b| format!("{b:02x}")).collect();
    let name: String = source
        .file_name()
        .map(|n| n.to_string_lossy().chars().filter(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_')).collect())
        .unwrap_or_else(|| "db".into());
    format!("{hash}-{name}")
}

fn handle(backend: Backend, id: &str, instance: &str) -> String {
    let name = match backend {
        Backend::Ida => "ida",
        Backend::Ghidra => "ghidra",
        Backend::Binja => "binja",
        Backend::Auto | Backend::None => "none",
    };
    format!("dsm://{name}/{id}/{instance}")
}

fn parse_handle(text: &str) -> Option<(Backend, String, String)> {
    let mut parts = text.strip_prefix("dsm://")?.split('/');
    let backend = match parts.next()? {
        "ida" => Backend::Ida,
        "ghidra" => Backend::Ghidra,
        "binja" => Backend::Binja,
        _ => return None,
    };
    let id = parts.next()?.to_string();
    let instance = parts.next()?.to_string();
    if parts.next().is_some() || id.is_empty() || instance.is_empty() {
        return None;
    }
    Some((backend, id, instance))
}

fn is_database(backend: Backend, path: &Path) -> bool {
    let ext = path.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
    match backend {
        Backend::Ida => ext == "i64" || ext == "idb",
        Backend::Binja => ext == "bndb",
        _ => false,
    }
}

fn database_owner(path: &Path) -> Option<Backend> {
    [Backend::Ida, Backend::Binja].into_iter().find(|&b| is_database(b, path))
}

/// The managed database file for a source binary.
fn store_path(backend: Backend, source: &Path) -> PathBuf {
    let name = source.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| "binary".into());
    let (dir, file) = match backend {
        Backend::Ghidra => (crate::home().join("agent/ghidra-projects"), "greencode.gpr".to_string()),
        Backend::Binja => (crate::home().join("agent/bndbs"), format!("{name}.bndb")),
        _ => (crate::home().join("agent/idbs"), format!("{name}.i64")),
    };
    dir.join(db_id(source)).join(file)
}

/// Opens (or attaches to) the database for `source`, adding one reference, and returns its handle.
/// Idle and `max_open` limits are enforced here.
pub async fn open(cfg: &Config, source: &Path, arch: &str) -> Result<String> {
    let inv = runtime(cfg).await?;
    let source = source.canonicalize().unwrap_or_else(|_| source.to_path_buf());
    if let Some(owner) = database_owner(&source) {
        ensure!(owner == inv.backend, "{} is not a {} database; switch `disassembler.backend` to its owner", source.display(), label(inv.backend));
        return open_with(cfg, inv, &source, &source, &source, arch).await;
    }
    let database = store_path(inv.backend, &source);
    std::fs::create_dir_all(database.parent().unwrap_or(Path::new(".")))?;
    let binary = stage_slice(&source, arch, database.parent().unwrap_or(Path::new("."))).await?;
    open_with(cfg, inv, &source, &binary, &database, arch).await
}

async fn open_with(cfg: &Config, inv: Invocation, source: &Path, binary: &Path, database: &Path, arch: &str) -> Result<String> {
    sweep(cfg).await;
    let id = db_id(source);
    if let Some(existing) = find_shared(&id).await {
        let mut db = existing.lock().await;
        ensure!(inv.backend == db.backend, "{} is open as a {} database", source.display(), label(db.backend));
        ensure!(db.worker.is_some(), "the {} database was closed; open it again", label(db.backend));
        db.refs += 1;
        db.last_used = Instant::now();
        return Ok(handle(db.backend, &db.id, &db.instance));
    }
    evict_one(cfg).await?;
    let (worker, info) = Worker::spawn(&inv, binary, database, arch).await?;
    let db = Db {
        id: id.clone(),
        instance: random_hex(16),
        backend: inv.backend,
        source: source.to_path_buf(),
        database: database.to_path_buf(),
        info,
        worker: Some(worker),
        refs: 1,
        last_used: Instant::now(),
    };
    let handle = handle(db.backend, &db.id, &db.instance);
    REGISTRY.lock().await.push(std::sync::Arc::new(Mutex::new(db)));
    Ok(handle)
}

/// Closes the least recently used unreferenced database while `max_open` is reached.
async fn evict_one(cfg: &Config) -> Result<()> {
    let max = cfg.disasm_opts.disassembler.max_open.max(1) as usize;
    loop {
        let entries: Vec<SharedDb> = REGISTRY.lock().await.clone();
        if entries.len() < max {
            return Ok(());
        }
        let mut victim: Option<(SharedDb, Instant)> = None;
        for db in entries {
            let Ok(guard) = db.try_lock() else { continue };
            if guard.refs == 0 && guard.worker.is_some() && victim.as_ref().is_none_or(|(_, time)| guard.last_used < *time) {
                let time = guard.last_used;
                drop(guard);
                victim = Some((db, time));
            }
        }
        let Some((db, _)) = victim else {
            bail!("{max} databases are already open and all are in use; close a dsm handle first")
        };
        let worker = {
            let mut guard = db.lock().await;
            guard.worker.take()
        };
        {
            let mut registry = REGISTRY.lock().await;
            registry.retain(|entry| !std::sync::Arc::ptr_eq(entry, &db));
        }
        if let Some(worker) = worker {
            worker.shutdown(true).await;
        }
    }
}

/// Universal Mach-O slices are staged as a thin file so every backend opens one architecture.
async fn stage_slice(source: &Path, arch: &str, dir: &Path) -> Result<PathBuf> {
    let Some(meta) = metadata(source) else { return Ok(source.to_path_buf()) };
    if meta.slices.len() <= 1 || arch.is_empty() {
        return Ok(source.to_path_buf());
    }
    let Some(slice) = meta.slices.iter().find(|s| s.arch.eq_ignore_ascii_case(arch)) else {
        bail!("no {arch} slice; available: {}", meta.slices.iter().map(|s| s.arch.clone()).collect::<Vec<_>>().join(", "))
    };
    let name = source.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| "binary".into());
    let staged = dir.join(format!("{name}.{arch}"));
    if !staged.is_file() {
        use std::io::{Read, Seek, SeekFrom};
        let mut input = std::fs::File::open(source)?;
        input.seek(SeekFrom::Start(slice.offset))?;
        let mut out = std::fs::File::create(&staged)?;
        std::io::copy(&mut input.take(slice.size), &mut out)?;
    }
    Ok(staged)
}

/// Saves and closes unreferenced databases idle longer than `idle_close_sec`.
async fn sweep(cfg: &Config) {
    let idle = cfg.disasm_opts.disassembler.idle_close_sec;
    if idle == 0 {
        return;
    }
    let entries: Vec<SharedDb> = REGISTRY.lock().await.clone();
    for db in entries {
        let due = db.try_lock().is_ok_and(|guard| guard.refs == 0 && guard.last_used.elapsed() >= Duration::from_secs(idle));
        if !due {
            continue;
        }
        let worker = {
            let mut guard = db.lock().await;
            guard.worker.take()
        };
        REGISTRY.lock().await.retain(|entry| !std::sync::Arc::ptr_eq(entry, &db));
        if let Some(worker) = worker {
            worker.shutdown(true).await;
        }
    }
}

/// A live entry by database id.
async fn find_shared(id: &str) -> Option<SharedDb> {
    let entries: Vec<SharedDb> = REGISTRY.lock().await.clone();
    for db in entries {
        if db.lock().await.id == id {
            return Some(db);
        }
    }
    None
}

/// A live entry for a handle or an id, checking that the handle's backend and incarnation match.
async fn find(cfg: &Config, target: &str) -> Result<SharedDb> {
    let (want, id, instance) = match parse_handle(target) {
        Some((backend, id, instance)) => (Some(backend), id, Some(instance)),
        None => (None, target.trim().to_string(), None),
    };
    if target.trim().starts_with("dsm://") && want.is_none() {
        bail!("invalid dsm handle; pass the string returned by dsm.open")
    }
    sweep(cfg).await;
    let Some(db) = find_shared(&id).await else {
        bail!("no live database `{target}`; open it again with dsm.open(path)")
    };
    {
        let guard = db.lock().await;
        if let Some(want) = want {
            ensure!(want == guard.backend, "that handle belongs to {} but {} is selected; switch `disassembler.backend`", label(want), label(guard.backend));
        }
        if let Some(instance) = instance {
            ensure!(
                instance == guard.instance && guard.worker.is_some(),
                "this dsm handle has expired; the original database is no longer open. Open it again"
            );
        }
        ensure!(guard.worker.is_some(), "the database was closed; open it again");
    }
    Ok(db)
}

async fn describe(db: &SharedDb, current: Option<&str>) -> Value {
    let guard = db.lock().await;
    json!({
        "id": guard.id,
        "handle": current.map(str::to_string).unwrap_or_else(|| handle(guard.backend, &guard.id, &guard.instance)),
        "backend": label(guard.backend),
        "path": guard.source.display().to_string(),
        "database": guard.database.display().to_string(),
        "references": guard.refs,
        "info": guard.info,
    })
}

/// Drops one reference. The count reaching zero keeps the database open (idle close saves it) unless
/// `close_now` asks for the worker to stop.
pub async fn release(cfg: &Config, target: &str, close_now: bool, save: bool) -> Result<()> {
    if target.trim().is_empty() {
        return Ok(());
    }
    let db = find(cfg, target).await?;
    let mut guard = db.lock().await;
    guard.refs = guard.refs.saturating_sub(1);
    guard.last_used = Instant::now();
    if guard.refs > 0 || !close_now {
        return Ok(());
    }
    let worker = guard.worker.take();
    drop(guard);
    REGISTRY.lock().await.retain(|entry| !std::sync::Arc::ptr_eq(entry, &db));
    if let Some(worker) = worker {
        worker.shutdown(save).await;
    }
    Ok(())
}

// --- analysis requests ------------------------------------------------------

pub async fn view(cfg: &Config, target: &str, kind: &str, func: Option<&str>) -> Result<String> {
    let db = find(cfg, target).await?;
    let mut guard = db.lock().await;
    let worker = guard.worker.as_mut().ok_or_else(|| anyhow!("the database is closed"))?;
    let result = worker.call("view", json!({ "kind": kind, "target": func })).await;
    guard.last_used = Instant::now();
    let value = result?;
    Ok(value.as_str().unwrap_or_default().to_string())
}

pub async fn exec(cfg: &Config, target: &str, code: &str) -> Result<Value> {
    let db = find(cfg, target).await?;
    let mut guard = db.lock().await;
    let worker = guard.worker.as_mut().ok_or_else(|| anyhow!("the database is closed"))?;
    let result = worker.call("exec", json!({ "code": code })).await;
    guard.last_used = Instant::now();
    result
}

/// `rename` and `comment` share one shape: a target plus the new text.
pub async fn mutate(cfg: &Config, target: &str, method: &str, params: Value) -> Result<Value> {
    let db = find(cfg, target).await?;
    let mut guard = db.lock().await;
    let worker = guard.worker.as_mut().ok_or_else(|| anyhow!("the database is closed"))?;
    let result = worker.call(method, params).await;
    guard.last_used = Instant::now();
    result
}

pub async fn save(cfg: &Config, target: &str) -> Result<Value> {
    let db = find(cfg, target).await?;
    let mut guard = db.lock().await;
    let worker = guard.worker.as_mut().ok_or_else(|| anyhow!("the database is closed"))?;
    let result = worker.call("save", json!({})).await;
    guard.last_used = Instant::now();
    result
}

/// Descriptors of the live databases, with their portable handles and reference counts.
pub async fn list() -> Vec<Value> {
    let entries: Vec<SharedDb> = REGISTRY.lock().await.clone();
    let mut out = Vec::new();
    for db in entries {
        let guard = db.lock().await;
        if guard.worker.is_some() {
            out.push(json!({
                "id": guard.id,
                "handle": handle(guard.backend, &guard.id, &guard.instance),
                "backend": label(guard.backend),
                "path": guard.source.display().to_string(),
                "database": guard.database.display().to_string(),
                "references": guard.refs,
                "info": guard.info,
            }));
        }
    }
    out
}

// --- on-disk executable metadata -------------------------------------------

#[derive(Clone, Debug, PartialEq)]
pub struct Slice {
    pub arch: String,
    pub offset: u64,
    pub size: u64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Meta {
    pub format: String,
    pub arch: String,
    pub bits: u32,
    /// Entry point; a PE's is a relative virtual address, as its header stores it.
    pub entry: Option<u64>,
    pub size: u64,
    pub slices: Vec<Slice>,
}

fn u16_at(bytes: &[u8], at: usize, big: bool) -> Option<u16> {
    Some(if big { u16::from_be_bytes(bytes.get(at..at + 2)?.try_into().ok()?) } else { u16::from_le_bytes(bytes.get(at..at + 2)?.try_into().ok()?) })
}

fn u32_at(bytes: &[u8], at: usize, big: bool) -> Option<u32> {
    Some(if big { u32::from_be_bytes(bytes.get(at..at + 4)?.try_into().ok()?) } else { u32::from_le_bytes(bytes.get(at..at + 4)?.try_into().ok()?) })
}

fn u64_at(bytes: &[u8], at: usize, big: bool) -> Option<u64> {
    Some(if big { u64::from_be_bytes(bytes.get(at..at + 8)?.try_into().ok()?) } else { u64::from_le_bytes(bytes.get(at..at + 8)?.try_into().ok()?) })
}

fn macho_arch(cpu: u32) -> String {
    match cpu {
        7 => "i386".into(),
        0x0100_0007 => "x86_64".into(),
        12 => "arm".into(),
        0x0100_000c => "arm64".into(),
        0x0200_000c => "arm64_32".into(),
        18 => "ppc".into(),
        0x0100_0012 => "ppc64".into(),
        other => format!("cpu{other}"),
    }
}

/// Mach-O, ELF or PE facts from the headers in `head`; `None` for anything else.
fn thin_meta(head: &[u8], size: u64) -> Option<Meta> {
    if head.starts_with(b"\x7fELF") {
        let class = *head.get(4)?;
        let big = *head.get(5)? == 2;
        let bits = if class == 2 { 64 } else { 32 };
        let machine = u16_at(head, 18, big)?;
        let arch = match machine {
            3 => "i386".to_string(),
            8 => "mips".to_string(),
            20 => "ppc".to_string(),
            21 => "ppc64".to_string(),
            40 => "arm".to_string(),
            62 => "x86_64".to_string(),
            183 => "aarch64".to_string(),
            243 => "riscv".to_string(),
            other => format!("machine{other}"),
        };
        let entry = if bits == 64 { u64_at(head, 24, big) } else { u32_at(head, 24, big).map(u64::from) };
        return Some(Meta { format: "ELF".into(), arch, bits, entry, size, slices: Vec::new() });
    }
    if head.starts_with(b"MZ") {
        let pe = u32_at(head, 0x3c, false)? as usize;
        if head.get(pe..pe + 4)? != b"PE\0\0" {
            return None;
        }
        let machine = u16_at(head, pe + 4, false)?;
        let optional = pe + 24;
        let bits = if u16_at(head, optional, false)? == 0x20b { 64 } else { 32 };
        let arch = match machine {
            0x014c => "i386".to_string(),
            0x8664 => "x86_64".to_string(),
            0xaa64 => "arm64".to_string(),
            0x01c4 => "arm".to_string(),
            other => format!("machine{other:x}"),
        };
        return Some(Meta { format: "PE".into(), arch, bits, entry: u32_at(head, optional + 16, false).map(u64::from), size, slices: Vec::new() });
    }
    let le = u32_at(head, 0, false)?;
    let be = u32_at(head, 0, true)?;
    let (bits, big) = match (le, be) {
        (0xfeed_facf, _) => (64, false),
        (0xfeed_face, _) => (32, false),
        (_, 0xfeed_facf) => (64, true),
        (_, 0xfeed_face) => (32, true),
        _ => return None,
    };
    let cpu = u32_at(head, 4, big)?;
    let ncmds = u32_at(head, 16, big)?;
    let mut entry = None;
    let mut at = if bits == 64 { 32 } else { 28 };
    for _ in 0..ncmds.min(4096) {
        let cmd = u32_at(head, at, big)?;
        let cmdsize = u32_at(head, at + 4, big)? as usize;
        if cmdsize < 8 {
            break;
        }
        if cmd == 0x8000_0028 {
            entry = u64_at(head, at + 8, big);
        }
        at += cmdsize;
    }
    Some(Meta { format: "Mach-O".into(), arch: macho_arch(cpu), bits, entry, size, slices: Vec::new() })
}

fn read_head(path: &Path, at: u64, len: usize) -> Option<Vec<u8>> {
    use std::io::{Read, Seek, SeekFrom};
    let mut file = std::fs::File::open(path).ok()?;
    if at > 0 {
        file.seek(SeekFrom::Start(at)).ok()?;
    }
    let mut head = Vec::with_capacity(len);
    file.take(len as u64).read_to_end(&mut head).ok()?;
    Some(head)
}

/// On-disk Mach-O/ELF/PE facts, without any backend. `None` for anything else.
pub fn metadata(path: &Path) -> Option<Meta> {
    let size = std::fs::metadata(path).ok()?.len();
    let head = read_head(path, 0, 4096.min(size as usize).max(64))?;
    let magic = u32_at(&head, 0, true)?;
    if magic != 0xcafe_babe && magic != 0xcafe_babf {
        return thin_meta(&head, size);
    }
    let fat64 = magic == 0xcafe_babf;
    let count = u32_at(&head, 4, true)?;
    if !(1..=64).contains(&count) {
        return None;
    }
    let stride = if fat64 { 32 } else { 20 };
    let mut slices = Vec::new();
    for index in 0..count as usize {
        let at = 8 + index * stride;
        let cpu = u32_at(&head, at, true)?;
        let (offset, len) = if fat64 {
            (u64_at(&head, at + 8, true)?, u64_at(&head, at + 16, true)?)
        } else {
            (u32_at(&head, at + 8, true)? as u64, u32_at(&head, at + 12, true)? as u64)
        };
        if offset.saturating_add(len) > size {
            return None;
        }
        slices.push(Slice { arch: macho_arch(cpu), offset, size: len });
    }
    let host = slices.iter().find(|s| s.arch == "arm64").or_else(|| slices.first())?;
    let host_head = read_head(path, host.offset, 4096)?;
    let mut meta = thin_meta(&host_head, size)?;
    meta.format = "Mach-O universal".into();
    meta.slices = slices;
    Some(meta)
}

fn human_size(bytes: u64) -> String {
    if bytes >= 1024 * 1024 {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    } else if bytes >= 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{bytes} B")
    }
}

fn metadata_text(meta: &Meta, path: &Path) -> String {
    let mut out = vec![format!("{} {} {}-bit  {}", meta.format, meta.arch, meta.bits, human_size(meta.size))];
    out.push(format!("path: {}", path.display()));
    if let Some(entry) = meta.entry {
        out.push(format!("entry: 0x{entry:x}{}", if meta.format == "PE" { " (RVA)" } else { "" }));
    }
    if meta.slices.len() > 1 {
        out.push(format!("slices: {}", meta.slices.iter().map(|s| s.arch.clone()).collect::<Vec<_>>().join(", ")));
    }
    out.push("read <path>:<view> analyses it (views: overview, imports, exports, strings, <func>, <func>:asm, xrefs:<func>)".into());
    out.join("\n")
}

/// The analysis views `read` and `dsm view` accept.
#[derive(Clone, Debug, PartialEq)]
pub enum Selector {
    Overview,
    Pseudocode(String),
    Asm(String),
    Imports,
    Exports,
    Strings,
    Xrefs(String),
}

impl Selector {
    pub fn parse(text: &str) -> Option<Selector> {
        let text = text.trim();
        if text.is_empty() {
            return None;
        }
        if let Some(rest) = text.strip_prefix("xrefs:") {
            return (!rest.is_empty()).then(|| Selector::Xrefs(rest.to_string()));
        }
        if let Some((func, view)) = text.rsplit_once(':') {
            return (view == "asm" && !func.is_empty()).then(|| Selector::Asm(func.to_string()));
        }
        Some(match text {
            "overview" => Selector::Overview,
            "imports" => Selector::Imports,
            "exports" => Selector::Exports,
            "strings" => Selector::Strings,
            other => Selector::Pseudocode(other.to_string()),
        })
    }

    pub fn kind(&self) -> &'static str {
        match self {
            Selector::Overview => "overview",
            Selector::Pseudocode(_) => "pseudocode",
            Selector::Asm(_) => "asm",
            Selector::Imports => "imports",
            Selector::Exports => "exports",
            Selector::Strings => "strings",
            Selector::Xrefs(_) => "xrefs",
        }
    }

    pub fn target(&self) -> Option<&str> {
        match self {
            Selector::Pseudocode(name) | Selector::Asm(name) | Selector::Xrefs(name) => Some(name),
            _ => None,
        }
    }

}

/// `@arm64:main` names a universal slice; the view is what follows the first colon.
fn strip_arch(text: &str) -> (String, String) {
    let Some(rest) = text.strip_prefix('@') else { return (String::new(), text.to_string()) };
    match rest.split_once(':') {
        Some((arch, view)) => (arch.to_string(), view.to_string()),
        None => (rest.to_string(), String::new()),
    }
}

/// Splits `bin:main:10-40`-style trailing parts off a path that does not exist as a whole.
fn split_selector(path: &Path) -> Option<(PathBuf, String)> {
    let text = path.to_string_lossy().to_string();
    let mut at = text.len();
    while let Some(colon) = text[..at].rfind(':') {
        if colon == 0 {
            break;
        }
        let base = PathBuf::from(&text[..colon]);
        if base.is_file() && (metadata(&base).is_some() || database_owner(&base).is_some()) {
            return Some((base, text[colon + 1..].to_string()));
        }
        at = colon;
    }
    None
}

/// A trailing `:N` or `:N-M` line range, applied to the analysis output as `read`'s offset/limit.
fn peel_range(suffix: &str) -> (String, Option<u64>, Option<u64>) {
    let Some((head, tail)) = suffix.rsplit_once(':') else { return (suffix.to_string(), None, None) };
    let number = |text: &str| text.trim().parse::<u64>().ok();
    match tail.split_once('-') {
        Some((from, to)) => match (number(from), number(to)) {
            (Some(from), Some(to)) if from >= 1 && to >= from => (head.to_string(), Some(from), Some(to - from + 1)),
            _ => (suffix.to_string(), None, None),
        },
        None => match number(tail) {
            Some(line) if line >= 1 => (head.to_string(), Some(line), Some(1)),
            _ => (suffix.to_string(), None, None),
        },
    }
}

fn window(text: &str, offset: Option<u64>, limit: Option<u64>) -> String {
    let offset = offset.unwrap_or(1).max(1) as usize - 1;
    let lines: Vec<&str> = text.lines().skip(offset).collect();
    match limit {
        Some(limit) => lines[..lines.len().min(limit as usize)].join("\n"),
        None => lines.join("\n"),
    }
}

/// The `read` path for executables and analysis databases: metadata alone without a view, analysis
/// for a view. `None` when `path` is neither.
pub async fn read(cfg: &Config, path: &Path, offset: Option<u64>, limit: Option<u64>) -> Result<Option<String>> {
    let (path, suffix) = match split_selector(path) {
        Some((base, suffix)) if !path.exists() => (base, suffix),
        _ => (path.to_path_buf(), String::new()),
    };
    let (arch, suffix) = strip_arch(&suffix);
    let (suffix, start, count) = peel_range(&suffix);
    let offset = offset.or(start);
    let limit = limit.or(count);
    if let Some(owner) = database_owner(&path) {
        let inv = runtime(cfg).await?;
        ensure!(owner == inv.backend, "{} is not a {} database; switch `disassembler.backend` to its owner", path.display(), label(inv.backend));
        let handle = open_with(cfg, inv, &path, &path, &path, &arch).await?;
        let selector = Selector::parse(&suffix).unwrap_or(Selector::Overview);
        let text = view(cfg, &handle, selector.kind(), selector.target()).await;
        release(cfg, &handle, false, false).await?;
        return Ok(Some(window(&text?, offset, limit)));
    }
    let Some(meta) = metadata(&path) else { return Ok(None) };
    if suffix.is_empty() {
        let mut text = metadata_text(&meta, &path);
        if !arch.is_empty() {
            ensure!(meta.slices.iter().any(|s| s.arch == arch), "no {arch} slice; available: {}", meta.slices.iter().map(|s| s.arch.clone()).collect::<Vec<_>>().join(", "));
            text.push_str(&format!("\nslice: {arch}"));
        }
        return Ok(Some(window(&text, offset, limit)));
    }
    let selector = Selector::parse(&suffix).ok_or_else(|| anyhow!(
        "unknown analysis view `{suffix}`; use overview, imports, exports, strings, <func>, <func>:asm or xrefs:<func>"
    ))?;
    let handle = open(cfg, &path, &arch).await?;
    let text = view(cfg, &handle, selector.kind(), selector.target()).await;
    release(cfg, &handle, false, false).await?;
    Ok(Some(window(&text?, offset, limit)))
}

// --- the `dsm` tool namespace ----------------------------------------------

/// `ext` tool `dsm`: open/list/get/view/exec/rename/comment/save/close, plus `metadata`.
pub async fn tool(cfg: &Config, args: &Value, cwd: &Path) -> Result<String> {
    let action = args["action"].as_str().ok_or_else(|| anyhow!("dsm requires `action`"))?;
    let text_arg = |key: &str| -> Result<String> {
        args[key].as_str().map(str::to_string).filter(|s| !s.is_empty()).ok_or_else(|| anyhow!("dsm {action} requires `{key}`"))
    };
    let db = || -> Result<String> {
        match (args["handle"].as_str(), args["id"].as_str()) {
            (Some(handle), _) if !handle.is_empty() => Ok(handle.to_string()),
            (_, Some(id)) if !id.is_empty() => Ok(id.to_string()),
            _ => bail!("dsm {action} requires `handle` (from dsm.open) or `id`"),
        }
    };
    Ok(match action {
        "open" => {
            let path = crate::permission::resolve(cwd, &text_arg("path")?);
            let handle = open(cfg, &path, args["arch"].as_str().unwrap_or("")).await?;
            serde_json::to_string_pretty(&describe(&find(cfg, &handle).await?, Some(&handle)).await)?
        }
        "list" => {
            let open = list().await;
            if open.is_empty() {
                "no open databases".to_string()
            } else {
                serde_json::to_string_pretty(&open)?
            }
        }
        "get" => serde_json::to_string_pretty(&describe(&find(cfg, &db()?).await?, None).await)?,
        "view" => {
            let selector = Selector::parse(args["view"].as_str().unwrap_or("overview"))
                .ok_or_else(|| anyhow!("dsm view needs one of: overview, imports, exports, strings, <func>, <func>:asm, xrefs:<func>"))?;
            view(cfg, &db()?, selector.kind(), selector.target()).await?
        }
        "exec" => serde_json::to_string_pretty(&exec(cfg, &db()?, &text_arg("code")?).await?)?,
        "rename" => serde_json::to_string_pretty(&mutate(cfg, &db()?, "rename", json!({"target": text_arg("target")?, "name": text_arg("name")?})).await?)?,
        "comment" => serde_json::to_string_pretty(&mutate(cfg, &db()?, "comment", json!({"target": text_arg("target")?, "text": text_arg("text")?})).await?)?,
        "set_type" => serde_json::to_string_pretty(&mutate(cfg, &db()?, "set_type", json!({"target": text_arg("target")?, "decl": text_arg("decl")?})).await?)?,
        "make_function" => serde_json::to_string_pretty(&mutate(cfg, &db()?, "make_function", json!({"target": text_arg("target")?})).await?)?,
        "save" => serde_json::to_string_pretty(&save(cfg, &db()?).await?)?,
        "retain" => {
            let handle = db()?;
            let db = find(cfg, &handle).await?;
            let mut guard = db.lock().await;
            guard.refs += 1;
            let result = describe_locked(&guard, &handle);
            result
        }
        "close" => {
            release(cfg, &db()?, true, args["save"].as_bool().unwrap_or(true)).await?;
            "closed".to_string()
        }
        "metadata" => {
            let path = crate::permission::resolve(cwd, &text_arg("path")?);
            match metadata(&path) {
                Some(meta) => metadata_text(&meta, &path),
                None => format!("{} is not an ELF, PE or Mach-O executable, or an analysis database", path.display()),
            }
        }
        other => bail!("unknown dsm action `{other}`; use open, list, get, view, exec, rename, comment, set_type, make_function, save, retain, close, metadata"),
    })
}

fn describe_locked(guard: &Db, handle: &str) -> String {
    serde_json::to_string_pretty(&json!({
        "id": guard.id,
        "handle": handle,
        "backend": label(guard.backend),
        "path": guard.source.display().to_string(),
        "database": guard.database.display().to_string(),
        "references": guard.refs,
        "info": guard.info,
    }))
    .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::disassembler::DisassemblerOpts;

    /// A worker that speaks the real protocol and pretends to have analysed one function.
    const FAKE: &str = r#"
import json, sys
backend, binary, database, arch = sys.argv[1], sys.argv[2], sys.argv[3], sys.argv[4]
state = {"renamed": None, "comments": [], "saved": 0, "types": [], "made": []}
sys.stdout.write(json.dumps({"ready": True, "info": {"backend": backend, "path": binary, "arch": arch, "functions": 1}}) + "\n")
sys.stdout.flush()
for line in sys.stdin:
    request = json.loads(line)
    rid, method, params = request["id"], request["method"], request.get("params") or {}
    if method == "view":
        if params["kind"] == "overview":
            result = f"{backend} {binary}\n1 function"
        elif params["kind"] == "pseudocode":
            result = f"int {state['renamed'] or params['target']}(void) {{ return 1; }}"
        elif params["kind"] == "asm":
            result = "0x1000  mov eax, 1\n0x1005  ret"
        else:
            result = f"{params['kind']}: none"
    elif method == "exec":
        scope = {"state": state, "db": params.get("code")}
        value = eval(params["code"], scope)
        result = {"output": "", "value": repr(value), "error": None}
    elif method == "rename":
        state["renamed"] = params["name"]
        result = {"target": params["target"], "name": params["name"]}
    elif method == "comment":
        state["comments"].append(params["text"])
        result = {"text": params["text"]}
    elif method == "set_type":
        state["types"].append(params["decl"])
        result = {"decl": params["decl"]}
    elif method == "make_function":
        state["made"].append(params["target"])
        result = {"function": params["target"]}
    elif method == "save":
        state["saved"] += 1
        result = {"saved": True, "count": state["saved"]}
    elif method == "close":
        result = {"saved": bool(params.get("save")), "count": state["saved"]}
        sys.stdout.write(json.dumps({"id": rid, "ok": True, "result": result}) + "\n")
        sys.stdout.flush()
        break
    else:
        sys.stdout.write(json.dumps({"id": rid, "ok": False, "error": f"unknown method {method}"}) + "\n")
        sys.stdout.flush()
        continue
    sys.stdout.write(json.dumps({"id": rid, "ok": True, "result": result}) + "\n")
    sys.stdout.flush()
"#;

    fn have_python() -> bool {
        std::process::Command::new("python3").arg("-c").arg("import sys").status().is_ok_and(|s| s.success())
    }

    fn fixture(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("greencode-dsm-test-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join("fake_worker.py");
        std::fs::write(&script, FAKE).unwrap();
        script
    }

    fn invocation(script: &Path, backend: Backend) -> Invocation {
        Invocation { backend, python: PathBuf::from("python3"), env: Vec::new(), script: script.to_path_buf() }
    }

    /// A thin 64-bit Mach-O with one LC_MAIN (entry offset 0x1000) for arm64.
    fn macho(head_at: usize, len: usize) -> Vec<u8> {
        let mut bytes = vec![0u8; len];
        bytes[head_at..head_at + 4].copy_from_slice(&0xfeed_facfu32.to_le_bytes());
        bytes[head_at + 4..head_at + 8].copy_from_slice(&0x0100_000cu32.to_le_bytes());
        bytes[head_at + 16..head_at + 20].copy_from_slice(&1u32.to_le_bytes());
        bytes[head_at + 20..head_at + 24].copy_from_slice(&24u32.to_le_bytes());
        bytes[head_at + 32..head_at + 36].copy_from_slice(&0x8000_0028u32.to_le_bytes());
        bytes[head_at + 36..head_at + 40].copy_from_slice(&24u32.to_le_bytes());
        bytes[head_at + 40..head_at + 48].copy_from_slice(&0x1000u64.to_le_bytes());
        bytes
    }

    #[test]
    fn detection_and_selector_parsing() {
        // auto prefers IDA, then Ghidra, and never Binary Ninja.
        assert_eq!(pick(Backend::Auto, true, true, true), Some(Backend::Ida));
        assert_eq!(pick(Backend::Auto, false, true, true), Some(Backend::Ghidra));
        assert_eq!(pick(Backend::Auto, false, false, true), None);
        assert_eq!(pick(Backend::Binja, true, true, false), None);
        assert_eq!(pick(Backend::Binja, true, true, true), Some(Backend::Binja));
        assert_eq!(pick(Backend::None, true, true, true), None);

        assert_eq!(Selector::parse("overview"), Some(Selector::Overview));
        assert_eq!(Selector::parse("main"), Some(Selector::Pseudocode("main".into())));
        assert_eq!(Selector::parse("main:asm"), Some(Selector::Asm("main".into())));
        assert_eq!(Selector::parse("xrefs:main"), Some(Selector::Xrefs("main".into())));
        assert_eq!(Selector::parse(""), None);
        assert_eq!(strip_arch("@arm64:main"), ("arm64".into(), "main".into()));
        assert_eq!(strip_arch("@arm64"), ("arm64".into(), String::new()));
        assert_eq!(strip_arch("main"), (String::new(), "main".into()));
        assert_eq!(peel_range("main:10-40"), ("main".into(), Some(10), Some(31)));
        assert_eq!(peel_range("main:7"), ("main".into(), Some(7), Some(1)));
        assert_eq!(peel_range("main"), ("main".into(), None, None));
        assert_eq!(window("a\nb\nc\nd", Some(2), Some(2)), "b\nc");
    }

    #[test]
    fn install_detection_reads_the_configured_directories() {
        let root = std::env::temp_dir().join(format!("greencode-dsm-installs-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("ida/idalib")).unwrap();
        std::fs::write(root.join("ida/idalib").join(ida_core_library()), b"").unwrap();
        std::fs::create_dir_all(root.join("ghidra/Ghidra/Features/PyGhidra/lib")).unwrap();
        std::fs::write(root.join("ghidra/Ghidra/application.properties"), b"application.version=12.2\n").unwrap();
        std::fs::write(root.join("ghidra/Ghidra/Features/PyGhidra/lib/PyGhidra.jar"), b"").unwrap();
        std::fs::create_dir_all(root.join("binja/python")).unwrap();
        std::fs::write(root.join("binja").join(binja_core_library()), b"").unwrap();

        let mut opts = DisassemblerOpts::default();
        opts.ida.install_dir = root.join("ida").display().to_string();
        opts.ghidra.install_dir = root.join("ghidra").display().to_string();
        opts.binja.install_dir = root.join("binja").display().to_string();
        assert_eq!(ida_install(&opts), Some(root.join("ida")));
        assert!(ghidra_valid(&root.join("ghidra")));
        assert_eq!(binja_install(&opts), Some(root.join("binja")));

        std::fs::write(root.join("ghidra/Ghidra/application.properties"), b"application.version=11.4\n").unwrap();
        assert!(!ghidra_valid(&root.join("ghidra")));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn metadata_reads_each_executable_header() {
        let dir = std::env::temp_dir().join(format!("greencode-dsm-meta-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        let mut elf = vec![0u8; 64];
        elf[0..4].copy_from_slice(b"\x7fELF");
        elf[4] = 2;
        elf[5] = 1;
        elf[18..20].copy_from_slice(&62u16.to_le_bytes());
        elf[24..32].copy_from_slice(&0x401000u64.to_le_bytes());
        let path = dir.join("a.out");
        std::fs::write(&path, &elf).unwrap();
        let meta = metadata(&path).unwrap();
        assert_eq!((meta.format.as_str(), meta.arch.as_str(), meta.bits, meta.entry), ("ELF", "x86_64", 64, Some(0x401000)));

        let path = dir.join("macho");
        std::fs::write(&path, macho(0, 256)).unwrap();
        let meta = metadata(&path).unwrap();
        assert_eq!((meta.format.as_str(), meta.arch.as_str(), meta.entry), ("Mach-O", "arm64", Some(0x1000)));

        // A universal header with the arm64 slice first and an x86_64 slice after it.
        let mut fat = vec![0u8; 4096];
        fat[0..4].copy_from_slice(&0xcafe_babeu32.to_be_bytes());
        fat[4..8].copy_from_slice(&2u32.to_be_bytes());
        // fat32 header: cputype, cpusubtype, offset, size (20 bytes each) starting at byte 8.
        fat[8..12].copy_from_slice(&0x0100_000cu32.to_be_bytes());
        fat[12..16].copy_from_slice(&0u32.to_be_bytes());
        fat[16..20].copy_from_slice(&0x100u32.to_be_bytes());
        fat[20..24].copy_from_slice(&0x200u32.to_be_bytes());
        fat[28..32].copy_from_slice(&0x0100_0007u32.to_be_bytes());
        fat[32..36].copy_from_slice(&3u32.to_be_bytes());
        fat[36..40].copy_from_slice(&0x300u32.to_be_bytes());
        fat[40..44].copy_from_slice(&0x200u32.to_be_bytes());
        fat[0x100..0x200].copy_from_slice(&macho(0, 0x100));
        fat[0x300..0x500].copy_from_slice(&macho(0, 0x200));
        let path = dir.join("universal");
        std::fs::write(&path, &fat).unwrap();
        let meta = metadata(&path).unwrap();
        assert_eq!(meta.format, "Mach-O universal");
        assert_eq!(meta.arch, "arm64");
        assert_eq!(meta.slices.iter().map(|s| (s.arch.as_str(), s.offset, s.size)).collect::<Vec<_>>(), [("arm64", 0x100, 0x200), ("x86_64", 0x300, 0x200)]);

        std::fs::write(dir.join("plain.txt"), b"hello").unwrap();
        assert_eq!(metadata(&dir.join("plain.txt")), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn worker_speaks_the_jsonl_protocol() {
        if !have_python() {
            return;
        }
        let dir = std::env::temp_dir().join(format!("greencode-dsm-worker-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("binary"), b"\x7fELF").unwrap();
        let inv = invocation(&fixture("protocol"), Backend::Ghidra);
        let (mut worker, info) = Worker::spawn(&inv, &dir.join("binary"), &dir.join("db"), "arm64").await.unwrap();
        assert_eq!(info["arch"], "arm64");
        assert_eq!(worker.call("view", json!({"kind": "asm", "target": "main"})).await.unwrap(), "0x1000  mov eax, 1\n0x1005  ret");
        let exec = worker.call("exec", json!({"code": "40 + 2"})).await.unwrap();
        assert_eq!(exec["value"], "42");
        assert_eq!(worker.call("rename", json!({"target": "0x1000", "name": "start"})).await.unwrap()["name"], "start");
        assert_eq!(
            worker.call("view", json!({"kind": "pseudocode", "target": "0x1000"})).await.unwrap(),
            "int start(void) { return 1; }"
        );
        assert!(worker.call("nope", json!({})).await.is_err());
        worker.shutdown(true).await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn registry_counts_references_and_enforces_max_open() {
        if !have_python() {
            return;
        }
        let script = fixture("registry");
        let dir = std::env::temp_dir().join(format!("greencode-dsm-registry-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut cfg = Config::default();
        cfg.disasm_opts.disassembler.backend = Backend::Ghidra;
        cfg.disasm_opts.disassembler.max_open = 1;
        cfg.disasm_opts.disassembler.idle_close_sec = 0;
        let first = dir.join("first");
        let second = dir.join("second");
        std::fs::write(&first, b"\x7fELF").unwrap();
        std::fs::write(&second, b"\x7fELF").unwrap();

        // Open through the public entry point, whose backend detection is bypassed by the env worker.
        unsafe { std::env::set_var("GREENCODE_DISASM_WORKER", &script) };
        let handle = open(&cfg, &first, "").await.unwrap();
        let again = open(&cfg, &first, "").await.unwrap();
        assert_eq!(handle, again);
        let live = find(&cfg, &handle).await.unwrap();
        assert_eq!(live.lock().await.refs, 2);

        release(&cfg, &handle, false, false).await.unwrap();
        assert_eq!(live.lock().await.refs, 1);
        // The database is still open at zero references, so reopening attaches to the same worker.
        release(&cfg, &handle, false, false).await.unwrap();
        assert_eq!(open(&cfg, &first, "").await.unwrap(), handle);
        release(&cfg, &handle, false, false).await.unwrap();

        // A second database evicts the idle one because max_open is 1.
        let other = open(&cfg, &second, "").await.unwrap();
        assert!(find(&cfg, &handle).await.is_err(), "the idle database is evicted");
        assert!(find(&cfg, &other).await.is_ok());

        // An evicted handle stays expired instead of silently reopening.
        assert!(view(&cfg, &handle, "overview", None).await.is_err());
        release(&cfg, &other, true, true).await.unwrap();
        assert!(list().await.iter().all(|db| db["path"] != second.display().to_string()));

        // Idle close: a database idle past the timeout is saved and closed.
        cfg.disasm_opts.disassembler.idle_close_sec = 900;
        let handle = open(&cfg, &first, "").await.unwrap();
        release(&cfg, &handle, false, false).await.unwrap();
        {
            let db = find(&cfg, &handle).await.unwrap();
            db.lock().await.last_used = Instant::now() - Duration::from_secs(1000);
        }
        sweep(&cfg).await;
        assert!(list().await.iter().all(|db| db["id"] != db_id(&first)));
        unsafe { std::env::remove_var("GREENCODE_DISASM_WORKER") };
        let _ = std::fs::remove_dir_all(&dir);
    }
}
