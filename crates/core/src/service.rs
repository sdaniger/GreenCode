//! User-level background service that keeps `greencode-cli serve` running.
//!
//! macOS uses a LaunchAgent, Linux a systemd user unit. `service.env` is read by `serve` itself,
//! so restarting the service applies changes without reinstalling it.
use anyhow::{bail, Context, Result};
use std::{collections::BTreeMap, path::{Path, PathBuf}, process::Command};

pub const LABEL: &str = "dev.greencode.service";
pub const UNIT: &str = "greencode.service";
const PROTECTED: [&str; 2] = ["PATH", "GREENCODE_HOME"];

pub fn env_path() -> PathBuf {
    crate::home().join("service.env")
}

pub fn log_path() -> PathBuf {
    crate::home().join("service.log")
}

pub fn plist_path() -> PathBuf {
    crate::launchd::agents_dir().join(format!("{LABEL}.plist"))
}

pub fn unit_path() -> PathBuf {
    crate::user_home().join(".config/systemd/user").join(UNIT)
}

/// Parses `service.env`: `KEY=VALUE`, literal values, no shell expansion. `#` starts a comment.
/// Entries that would move the installation or change how the service finds it are refused.
pub fn parse_env(text: &str) -> Result<BTreeMap<String, String>> {
    let mut env = BTreeMap::new();
    for (index, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (key, value) = line.split_once('=').with_context(|| format!("service.env line {}: expected KEY=VALUE", index + 1))?;
        let key = key.trim();
        if key.is_empty() || !key.bytes().enumerate().all(|(i, b)| b == b'_' || b.is_ascii_alphabetic() || (i > 0 && b.is_ascii_digit())) {
            bail!("service.env line {}: invalid variable name `{key}`", index + 1);
        }
        if let Some(protected) = PROTECTED.iter().find(|p| p.eq_ignore_ascii_case(key)) {
            bail!("service.env line {}: {protected} cannot be set here", index + 1);
        }
        let value = value.trim();
        let value = match value.as_bytes().first() {
            Some(&quote @ (b'"' | b'\'')) => {
                let end = value[1..].find(quote as char).with_context(|| format!("service.env line {}: unterminated quote", index + 1))?;
                let rest = value[end + 2..].trim();
                if !rest.is_empty() && !rest.starts_with('#') {
                    bail!("service.env line {}: text after the closing quote", index + 1);
                }
                value[1..end + 1].to_owned()
            }
            _ => match value.find(" #") {
                Some(at) => value[..at].trim_end().to_owned(),
                None => value.to_owned(),
            },
        };
        env.insert(key.to_owned(), value);
    }
    Ok(env)
}

pub fn read_env() -> Result<BTreeMap<String, String>> {
    match std::fs::read_to_string(env_path()) {
        Ok(text) => parse_env(&text),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(BTreeMap::new()),
        Err(e) => Err(e).with_context(|| format!("Cannot read {}", env_path().display())),
    }
}

/// Applies `service.env` to this process. `serve` calls this so both launchd and systemd
/// get the same variables without embedding values in the service file.
pub fn load_env() -> Result<()> {
    for (key, value) in read_env()? {
        std::env::set_var(key, value);
    }
    Ok(())
}

/// True when the service should listen on the local network.
pub fn lan() -> bool {
    fn truthy(value: &str) -> bool {
        matches!(value.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on")
    }
    std::env::var("GREENCODE_LAN").ok().is_some_and(|v| truthy(&v))
        || read_env().ok().and_then(|env| env.get("GREENCODE_LAN").map(|v| truthy(v))).unwrap_or(false)
}

fn lan_text(text: &str, enabled: bool) -> String {
    let mut out = String::new();
    for line in text.lines() {
        let key = line.split_once('=').map(|(k, _)| k.trim());
        if key.is_some_and(|k| k.eq_ignore_ascii_case("GREENCODE_LAN")) {
            if enabled {
                out.push_str("GREENCODE_LAN=true\n");
            }
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }
    if enabled && !out.lines().any(|l| l.split_once('=').map(|(k, _)| k.trim().eq_ignore_ascii_case("GREENCODE_LAN")).unwrap_or(false)) {
        out.push_str("GREENCODE_LAN=true\n");
    }
    out
}

/// Writes `GREENCODE_LAN` in `service.env`, preserving the rest of the file.
pub fn set_lan(enabled: bool) -> Result<()> {
    let path = env_path();
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e).with_context(|| format!("Cannot read {}", path.display())),
    };
    crate::remote::write_private(&path, lan_text(&text, enabled).as_bytes())
}

fn unit_quote(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

/// A keep-alive LaunchAgent that runs `program serve`.
pub fn launchd_plist(program: &Path, log: &Path) -> String {
    use crate::launchd::escape;
    let args: String = [program.to_string_lossy().into_owned(), "serve".into()]
        .iter()
        .map(|arg| format!("\t\t<string>{}</string>\n", escape(arg)))
        .collect();
    let home = crate::home();
    let path = crate::config::agent_path().map(|p| format!("\t\t<key>PATH</key>\n\t\t<string>{}</string>\n", escape(&p.to_string_lossy()))).unwrap_or_default();
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
<plist version=\"1.0\">\n<dict>\n\
\t<key>Label</key>\n\t<string>{LABEL}</string>\n\
\t<key>ProgramArguments</key>\n\t<array>\n{args}\t</array>\n\
\t<key>EnvironmentVariables</key>\n\t<dict>\n\
\t\t<key>GREENCODE_HOME</key>\n\t\t<string>{}</string>\n{path}\t</dict>\n\
\t<key>WorkingDirectory</key>\n\t<string>{}</string>\n\
\t<key>RunAtLoad</key>\n\t<true/>\n\
\t<key>KeepAlive</key>\n\t<true/>\n\
\t<key>ThrottleInterval</key>\n\t<integer>10</integer>\n\
\t<key>StandardOutPath</key>\n\t<string>{}</string>\n\
\t<key>StandardErrorPath</key>\n\t<string>{}</string>\n\
</dict>\n</plist>\n",
        escape(&home.to_string_lossy()),
        escape(&home.to_string_lossy()),
        escape(&log.to_string_lossy()),
        escape(&log.to_string_lossy()),
    )
}

/// A systemd user unit that runs `program serve` and reads `service.env` if it exists.
pub fn systemd_unit(program: &Path, log: &Path, env_file: &Path) -> String {
    let home = crate::home();
    format!(
        "[Unit]\nDescription=GreenCode background server\nAfter=network-online.target\n\n\
[Service]\nType=simple\nEnvironment={}\nEnvironmentFile=-{}\nExecStart={} serve\n\
Restart=on-failure\nRestartSec=2\nStandardOutput=append:{}\nStandardError=append:{}\n\n\
[Install]\nWantedBy=default.target\n",
        unit_quote(&format!("GREENCODE_HOME={}", home.display())),
        unit_quote(&env_file.to_string_lossy()),
        unit_quote(&program.to_string_lossy()),
        unit_quote(&log.to_string_lossy()),
        unit_quote(&log.to_string_lossy()),
    )
}

fn command_output(program: &str, args: &[&str]) -> Result<std::process::Output> {
    Command::new(program).args(args).output().with_context(|| format!("Cannot run {program}"))
}

fn macos_domain() -> Result<String> {
    crate::launchd::domain()
}

fn service_file() -> PathBuf {
    if cfg!(target_os = "macos") { plist_path() } else { unit_path() }
}

/// Installs and starts the user service. Existing data and `service.env` are kept.
pub fn install(lan: bool) -> Result<String> {
    if cfg!(windows) {
        bail!("The background service is not supported on Windows yet; run `greencode-cli serve` in a terminal.");
    }
    let program = std::env::current_exe().context("Cannot find the greencode-cli executable")?.canonicalize()?;
    std::fs::create_dir_all(crate::home())?;
    if lan {
        set_lan(true)?;
    }
    let log = log_path();
    if cfg!(target_os = "macos") {
        let plist = launchd_plist(&program, &log);
        std::fs::create_dir_all(crate::launchd::agents_dir())?;
        let path = plist_path();
        crate::remote::write_private(&path, plist.as_bytes())?;
        let domain = macos_domain()?;
        let target = format!("{domain}/{LABEL}");
        let _ = Command::new("launchctl").args(["bootout", &target]).output();
        let loaded = command_output("launchctl", &["bootstrap", &domain, &path.to_string_lossy()])?;
        if !loaded.status.success() {
            bail!("launchctl bootstrap failed: {}", String::from_utf8_lossy(&loaded.stderr).trim());
        }
        let _ = Command::new("launchctl").args(["kickstart", "-k", &target]).output();
        Ok(format!("GreenCode service installed and started ({}).\nLog: {}\nStatus: greencode-cli service status", path.display(), log.display()))
    } else {
        let path = unit_path();
        std::fs::create_dir_all(path.parent().expect("unit path has a parent"))?;
        crate::remote::write_private(&path, systemd_unit(&program, &log, &env_path()).as_bytes())?;
        let reload = command_output("systemctl", &["--user", "daemon-reload"])?;
        if !reload.status.success() {
            bail!("systemctl --user is unavailable; a systemd user session is required ({})", String::from_utf8_lossy(&reload.stderr).trim());
        }
        let started = command_output("systemctl", &["--user", "enable", "--now", UNIT])?;
        if !started.status.success() {
            bail!("systemctl --user enable --now {UNIT} failed: {}", String::from_utf8_lossy(&started.stderr).trim());
        }
        let mut warning = String::new();
        if let Ok(output) = command_output("loginctl", &["enable-linger", &current_user()]) {
            if !output.status.success() {
                warning = format!("\nRun `sudo loginctl enable-linger {}` so the service survives logout.", current_user());
            }
        }
        Ok(format!("GreenCode service installed and started ({}).\nLog: {}\nStatus: greencode-cli service status{warning}", path.display(), log.display()))
    }
}

fn current_user() -> String {
    std::env::var("USER").or_else(|_| std::env::var("LOGNAME")).unwrap_or_else(|_| "user".into())
}

/// Prints where the service is and whether it is running. Problems are reported as
/// `linger-disabled`, `service-stopped` or `service-disabled` so scripts can grep them.
pub fn status() -> Result<String> {
    if cfg!(windows) {
        bail!("The background service is not supported on Windows yet; run `greencode-cli serve` in a terminal.");
    }
    let env = read_env();
    let port = env.as_ref().ok().and_then(|e| e.get("GREENCODE_PORT").and_then(|v| v.parse::<u16>().ok())).unwrap_or(47322);
    let lan = lan();
    let host = if lan { "0.0.0.0" } else { "127.0.0.1" };
    let mut out = String::new();
    let installed = service_file().exists();
    out.push_str(&format!("installed: {}\n", if installed { "yes" } else { "no" }));
    if !installed {
        out.push_str("running: no\nservice-not-installed\n");
        return Ok(out);
    }
    if cfg!(target_os = "macos") {
        let domain = macos_domain()?;
        let target = format!("{domain}/{LABEL}");
        let state = command_output("launchctl", &["print", &target])?;
        out.push_str(&format!("running: {}\n", if state.status.success() { "yes" } else { "no" }));
        if !state.status.success() {
            out.push_str("service-stopped\n");
        }
    } else {
        let active = command_output("systemctl", &["--user", "is-active", UNIT])?;
        let enabled = command_output("systemctl", &["--user", "is-enabled", UNIT])?;
        let running = active.status.success();
        out.push_str(&format!("running: {}\n", if running { "yes" } else { "no" }));
        out.push_str(&format!("enabled: {}\n", if enabled.status.success() { "yes" } else { "no" }));
        if !running {
            out.push_str("service-stopped\n");
        }
        if !enabled.status.success() {
            out.push_str("service-disabled\n");
        }
        if let Ok(linger) = command_output("loginctl", &["show-user", &current_user(), "--property=Linger"]) {
            let yes = String::from_utf8_lossy(&linger.stdout).trim() == "Linger=yes";
            out.push_str(&format!("linger: {}\n", if yes { "yes" } else { "no" }));
            if !yes {
                out.push_str("linger-disabled\n");
            }
        }
    }
    out.push_str(&format!("log: {}\n", log_path().display()));
    let token = crate::remote::token(false).ok();
    match token {
        Some(token) => out.push_str(&format!("url: http://{host}:{port}/?token={token}\n")),
        None => out.push_str(&format!("url: http://{host}:{port}/\n")),
    }
    if let Err(e) = env {
        out.push_str(&format!("service.env: {e:#}\n"));
    }
    Ok(out)
}

/// Restarts the installed service, interrupting running turns and remote clients.
pub fn restart() -> Result<String> {
    if cfg!(windows) {
        bail!("The background service is not supported on Windows yet; run `greencode-cli serve` in a terminal.");
    }
    if !service_file().exists() {
        bail!("The service is not installed; run `greencode-cli service install` first.");
    }
    if cfg!(target_os = "macos") {
        let domain = macos_domain()?;
        let target = format!("{domain}/{LABEL}");
        let output = command_output("launchctl", &["kickstart", "-k", &target])?;
        if !output.status.success() {
            bail!("launchctl kickstart failed: {}", String::from_utf8_lossy(&output.stderr).trim());
        }
    } else {
        let output = command_output("systemctl", &["--user", "restart", UNIT])?;
        if !output.status.success() {
            bail!("systemctl --user restart failed: {}", String::from_utf8_lossy(&output.stderr).trim());
        }
    }
    Ok("GreenCode service restarted.".into())
}

/// Stops the service and removes it from startup. Projects, threads, settings and
/// `service.env` are kept.
pub fn uninstall() -> Result<String> {
    if cfg!(windows) {
        bail!("The background service is not supported on Windows yet; run `greencode-cli serve` in a terminal.");
    }
    if cfg!(target_os = "macos") {
        if let Ok(domain) = macos_domain() {
            let target = format!("{domain}/{LABEL}");
            let _ = Command::new("launchctl").args(["bootout", &target]).output();
        }
        match std::fs::remove_file(plist_path()) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
    } else {
        let _ = Command::new("systemctl").args(["--user", "disable", "--now", UNIT]).output();
        match std::fs::remove_file(unit_path()) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        let _ = Command::new("systemctl").args(["--user", "daemon-reload"]).output();
    }
    Ok("GreenCode service removed from startup. Projects, threads and settings were kept.".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_file_is_literal_and_guards_installation_paths() {
        let env = parse_env("# comment\nexport = no\nA=one\nB = two words\nC=\"three # four\"\nD=value # comment\nE=abc#def\n").unwrap();
        assert_eq!(env["A"], "one");
        assert_eq!(env["B"], "two words");
        assert_eq!(env["C"], "three # four");
        assert_eq!(env["D"], "value");
        assert_eq!(env["E"], "abc#def");
        assert_eq!(env["export"], "no", "keywords are ordinary keys; shell syntax is not evaluated");
        assert!(parse_env("PATH=/tmp\n").unwrap_err().to_string().contains("PATH"));
        assert!(parse_env("greencode_home=/tmp\n").unwrap_err().to_string().contains("GREENCODE_HOME"));
        assert!(parse_env("NO_EQUALS\n").is_err());
        assert!(parse_env("A=\"open\n").is_err());
    }

    #[test]
    fn generation_keeps_the_service_alive_and_uses_the_env_file() {
        let program = Path::new("/Applications/GreenCode dir/greencode-cli");
        let log = Path::new("/tmp/greencode service.log");
        let plist = launchd_plist(program, log);
        for expected in [
            "<string>dev.greencode.service</string>",
            "<string>/Applications/GreenCode dir/greencode-cli</string>",
            "<string>serve</string>",
            "<key>KeepAlive</key>\n\t<true/>",
            "<key>RunAtLoad</key>\n\t<true/>",
            "<string>/tmp/greencode service.log</string>",
            "<key>GREENCODE_HOME</key>",
        ] {
            assert!(plist.contains(expected), "{expected}\n{plist}");
        }
        assert!(plist.starts_with("<?xml") && plist.trim_end().ends_with("</plist>"));
        let unit = systemd_unit(program, log, Path::new("/home/a b/.greencode/service.env"));
        for expected in [
            "ExecStart=\"/Applications/GreenCode dir/greencode-cli\" serve",
            "EnvironmentFile=-\"/home/a b/.greencode/service.env\"",
            "Restart=on-failure",
            "WantedBy=default.target",
        ] {
            assert!(unit.contains(expected), "{expected}\n{unit}");
        }
    }

    #[test]
    fn lan_switch_preserves_comments_and_other_values() {
        let off = lan_text("# keep\nPORT=1\nGREENCODE_LAN=true\n", false);
        assert_eq!(off, "# keep\nPORT=1\n");
        let on = lan_text(&off, true);
        assert_eq!(on, "# keep\nPORT=1\nGREENCODE_LAN=true\n");
        assert_eq!(lan_text(&on, true), on, "enabling twice does not duplicate the key");
    }
}
