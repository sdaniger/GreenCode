//! `launch`: named long-running services and their `proc://` supervision. A service is a shell command whose
//! pid, log and stdin stay addressable by name for the life of this CLI process.
use crate::config::Config;
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    io::Write,
    path::{Path, PathBuf},
    sync::{LazyLock, Mutex},
};

struct Service {
    pid: u32,
    command: String,
    cwd: PathBuf,
    log: PathBuf,
    started: u64,
    stdin: Option<std::process::ChildStdin>,
}

static SERVICES: LazyLock<Mutex<HashMap<String, Service>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

fn services() -> &'static Mutex<HashMap<String, Service>> {
    &SERVICES
}

fn dir(home: &Path) -> PathBuf {
    home.join("services")
}

fn check_name(name: &str) -> Result<()> {
    if name.is_empty() || name.len() > 64 || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.') {
        bail!("service name must be 1-64 chars of letters, digits, `-`, `_` or `.`");
    }
    Ok(())
}

fn pid_alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        std::process::Command::new("kill")
            .args(["-0", &pid.to_string()])
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    }
    #[cfg(windows)]
    {
        let out = std::process::Command::new("tasklist").args(["/FI", &format!("PID eq {pid}"), "/NH"]).output();
        out.is_ok_and(|o| String::from_utf8_lossy(&o.stdout).contains(&pid.to_string()))
    }
}

/// Starts `name` and returns the pid. A live service of the same name is refused, so a retry cannot double-start.
pub fn start_in(home: &Path, cwd: &Path, name: &str, command: &str, cfg: &Config) -> Result<String> {
    check_name(name)?;
    let mut map = services().lock().unwrap();
    if let Some(s) = map.get_mut(name) {
        if pid_alive(s.pid) {
            bail!("service `{name}` is already running (pid {})", s.pid);
        }
        map.remove(name);
    }
    let dir = dir(home);
    std::fs::create_dir_all(&dir)?;
    let log = dir.join(format!("{name}.log"));
    let file = std::fs::File::create(&log)?;
    // No async runtime is needed to own a long-lived service, so this uses a plain `sh` like `bash` does.
    let mut cmd = std::process::Command::new(cfg.bash_opts.shell_path.as_deref().filter(|p| !p.trim().is_empty()).unwrap_or("sh"));
    cmd.arg("-c")
        .arg(command)
        .envs(crate::secrets::env())
        .current_dir(cwd)
        .stdin(std::process::Stdio::piped())
        .stdout(file.try_clone()?)
        .stderr(file);
    let mut child = cmd.spawn().with_context(|| format!("cannot start service `{name}`"))?;
    let pid = child.id();
    let stdin = child.stdin.take();
    // Reap in the background; the pid and log stay usable after exit.
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    let started = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    std::fs::write(
        dir.join(format!("{name}.json")),
        serde_json::to_vec(&json!({"name": name, "pid": pid, "command": command, "cwd": cwd, "log": log, "started": started}))?,
    )?;
    map.insert(
        name.into(),
        Service { pid, command: command.into(), cwd: cwd.into(), log: log.clone(), started, stdin },
    );
    Ok(format!(
        "service `{name}` started, pid {pid}. Log: {} (read with `read proc://{name}`); send stdin with `write proc://{name}`; stop it with `write proc://{name}/kill`.",
        log.display()
    ))
}

/// One service's record plus the end of its log, or the list when `path` is empty.
pub fn read(cfg: &Config, path: &str) -> Result<String> {
    if !cfg.extra_tools_opts.launch.enabled {
        bail!("proc:// is disabled by launch.enabled");
    }
    let name = path.trim_end_matches('/');
    if name.is_empty() {
        let map = services().lock().unwrap();
        let mut rows: Vec<String> = map
            .iter()
            .map(|(name, s)| {
                format!("{name}\tpid {} {}\t{}", s.pid, if pid_alive(s.pid) { "running" } else { "exited" }, s.command)
            })
            .collect();
        if rows.is_empty() {
            for entry in std::fs::read_dir(crate::home().join("services")).into_iter().flatten().flatten() {
                let path = entry.path();
                if path.extension().is_some_and(|e| e == "json") {
                    if let Ok(text) = std::fs::read_to_string(&path) {
                        if let Ok(v) = serde_json::from_str::<Value>(&text) {
                            let pid = v["pid"].as_u64().unwrap_or(0) as u32;
                            rows.push(format!(
                                "{}\tpid {pid} {}\t{}",
                                v["name"].as_str().unwrap_or("?"),
                                if pid_alive(pid) { "running" } else { "exited" },
                                v["command"].as_str().unwrap_or("")
                            ));
                        }
                    }
                }
            }
        }
        rows.sort();
        return Ok(if rows.is_empty() { "no named services".into() } else { rows.join("\n") });
    }
    if let Some(rest) = name.strip_suffix("/kill") {
        return kill(rest);
    }
    let map = services().lock().unwrap();
    let (record, log) = match map.get(name) {
        Some(s) => (
            json!({"name": name, "pid": s.pid, "command": s.command, "cwd": s.cwd, "started": s.started}),
            s.log.clone(),
        ),
        None => {
            let path = crate::home().join("services").join(format!("{name}.json"));
            let record: Value = serde_json::from_slice(&std::fs::read(&path).with_context(|| format!("no service `{name}`"))?)?;
            let log = PathBuf::from(record["log"].as_str().unwrap_or_default());
            (record, log)
        }
    };
    let pid = record["pid"].as_u64().unwrap_or(0) as u32;
    let tail = std::fs::read_to_string(&log).unwrap_or_default();
    let tail: Vec<&str> = tail.lines().collect();
    let tail = tail[tail.len().saturating_sub(40)..].join("\n");
    Ok(format!(
        "{} (pid {pid}, {})\n{tail}",
        record["command"].as_str().unwrap_or(""),
        if pid_alive(pid) { "running" } else { "exited" }
    ))
}

fn kill(name: &str) -> Result<String> {
    check_name(name)?;
    let mut map = services().lock().unwrap();
    let Some(s) = map.get(name) else { return Ok(format!("no running service `{name}`")) };
    if !pid_alive(s.pid) {
        map.remove(name);
        return Ok(format!("service `{name}` had already exited"));
    }
    #[cfg(unix)]
    let status = std::process::Command::new("kill").args(["-TERM", &s.pid.to_string()]).status();
    #[cfg(windows)]
    let status = std::process::Command::new("taskkill").args(["/PID", &s.pid.to_string(), "/T", "/F"]).status();
    status?;
    let pid = s.pid;
    map.remove(name);
    Ok(format!("stopped service `{name}` (pid {pid})"))
}

/// `write proc://<name>` sends the content to the service's stdin (only while this process owns it).
pub fn send_stdin(name: Option<&str>, content: &str) -> Result<String> {
    let mut map = services().lock().unwrap();
    let name = name.unwrap_or_default().trim_end_matches('/');
    if name.is_empty() {
        bail!("write needs a service name: `proc://<name>`");
    }
    let s = map.get_mut(name).with_context(|| format!("no running service `{name}` in this process"))?;
    if !pid_alive(s.pid) {
        bail!("service `{name}` has exited");
    }
    let stdin = s.stdin.as_mut().context("no stdin pipe for this service")?;
    let mut payload = content.to_string();
    if !payload.ends_with('\n') {
        payload.push('\n');
    }
    stdin.write_all(payload.as_bytes())?;
    stdin.flush()?;
    Ok(format!("sent {} bytes to service `{name}`", content.len()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn starts_reads_stdin_and_kills() {
        let home = tempfile::tempdir().unwrap();
        let cfg = Config::default();
        let cwd = home.path();
        let started = start_in(home.path(), cwd, "echoer", "cat", &cfg).unwrap();
        assert!(started.contains("pid"));
        assert!(start_in(home.path(), cwd, "echoer", "cat", &cfg).unwrap_err().to_string().contains("already running"));
        send_stdin(Some("echoer"), "hello").unwrap();
        let mut seen = String::new();
        for _ in 0..50 {
            let text = read(&cfg, "echoer").unwrap();
            if text.contains("hello") {
                seen = text;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(40));
        }
        assert!(seen.contains("hello"), "{seen}");
        assert!(read(&cfg, "echoer/kill").unwrap().contains("stopped"));
        assert!(read(&cfg, "").unwrap().contains("no named services") || read(&cfg, "").unwrap().contains("exited"));
        let mut off = Config::default();
        off.extra_tools_opts.launch.enabled = false;
        assert!(read(&off, "").is_err());
        assert!(start_in(home.path(), cwd, "bad name!", "cat", &cfg).is_err());
    }
}
