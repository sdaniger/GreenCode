//! One-shot launchd user agents: scheduled work that runs while the app is closed.
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
/// Exclusive, persistent claim for a scheduled turn. A started turn is never retried automatically:
/// it may already have performed irreversible actions even if its process subsequently dies.
pub struct ScheduleClaim {
    _lock: std::fs::File,
    dir: PathBuf,
    label: String,
    finished: bool,
}

fn schedule_path(dir: &Path, label: &str, suffix: &str) -> Result<PathBuf> {
    anyhow::ensure!(!label.is_empty() && label.bytes().all(|c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'-' | b'_')), "invalid schedule label");
    Ok(dir.join(format!("{label}.{suffix}")))
}

/// `None` means another process owns, completed, cancelled, or already attempted this turn.
pub fn claim_schedule(dir: &Path, label: &str) -> Result<Option<ScheduleClaim>> {
    std::fs::create_dir_all(dir)?;
    let lock = std::fs::OpenOptions::new().write(true).create(true).truncate(false).open(schedule_path(dir, label, "lock")?)?;
    match lock.try_lock() {
        Ok(()) => {}
        Err(std::fs::TryLockError::WouldBlock) => return Ok(None),
        Err(std::fs::TryLockError::Error(e)) => return Err(e.into()),
    }
    for suffix in ["done", "cancelled", "started", "failed"] {
        if schedule_path(dir, label, suffix)?.exists() { return Ok(None); }
    }
    std::fs::write(schedule_path(dir, label, "started")?, "")?;
    Ok(Some(ScheduleClaim { _lock: lock, dir: dir.to_owned(), label: label.to_owned(), finished: false }))
}

impl ScheduleClaim {
    /// Record success before releasing the claim.
    pub fn complete(mut self) -> Result<()> {
        std::fs::write(schedule_path(&self.dir, &self.label, "done")?, "")?;
        self.finished = true;
        Ok(())
    }
}

impl Drop for ScheduleClaim {
    fn drop(&mut self) {
        if !self.finished {
            if let Ok(path) = schedule_path(&self.dir, &self.label, "failed") {
                let _ = std::fs::write(path, "Scheduled turn failed or was interrupted; it was not retried because it may have performed actions.");
            }
        }
    }
}

/// Cancel before a turn starts; a running claim cannot safely be cancelled here.
pub fn cancel_schedule(dir: &Path, label: &str) -> Result<()> {
    std::fs::create_dir_all(dir)?;
    let lock = std::fs::OpenOptions::new().write(true).create(true).truncate(false).open(schedule_path(dir, label, "lock")?)?;
    match lock.try_lock() {
        Ok(()) => {}
        Err(std::fs::TryLockError::WouldBlock) => anyhow::bail!("scheduled turn is already running; it cannot be cancelled safely"),
        Err(std::fs::TryLockError::Error(e)) => return Err(e.into()),
    }
    std::fs::write(schedule_path(dir, label, "cancelled")?, "")?;
    Ok(())
}

/// An unlocked started claim without a success marker is an interrupted or failed turn.
pub fn schedule_failed(dir: &Path, label: &str) -> Result<bool> {
    if schedule_path(dir, label, "failed")?.exists() && !schedule_path(dir, label, "started")?.exists() { return Ok(true); }
    if !schedule_path(dir, label, "started")?.exists() || schedule_path(dir, label, "done")?.exists() { return Ok(false); }
    let lock = std::fs::OpenOptions::new().write(true).open(schedule_path(dir, label, "lock")?)?;
    match lock.try_lock() {
        Ok(()) => Ok(true),
        Err(std::fs::TryLockError::WouldBlock) => Ok(false),
        Err(std::fs::TryLockError::Error(e)) => Err(e.into()),
    }
}

fn escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

/// A LaunchAgent plist that runs `program_args` once at `at` = (month, day, hour, minute), logging to `log`.
pub fn plist(label: &str, program_args: &[String], at: (u32, u32, u32, u32), log: &Path) -> String {
    let args: String = program_args.iter().map(|a| format!("\t\t<string>{}</string>\n", escape(a))).collect();
    let log = escape(&log.to_string_lossy());
    let (month, day, hour, minute) = at;
    // launchd starts jobs with a minimal environment: pass the same home and agent search path.
    let home = escape(&std::fs::canonicalize(crate::home()).unwrap_or_else(|_| crate::home()).to_string_lossy());
    let path = crate::config::agent_path().map(|p| format!("\t\t<key>PATH</key>\n\t\t<string>{}</string>\n", escape(&p.to_string_lossy()))).unwrap_or_default();
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
<plist version=\"1.0\">\n<dict>\n\
\t<key>Label</key>\n\t<string>{}</string>\n\
\t<key>ProgramArguments</key>\n\t<array>\n{args}\t</array>\n\
\t<key>StartCalendarInterval</key>\n\t<dict>\n\
\t\t<key>Month</key>\n\t\t<integer>{month}</integer>\n\
\t\t<key>Day</key>\n\t\t<integer>{day}</integer>\n\
\t\t<key>Hour</key>\n\t\t<integer>{hour}</integer>\n\
\t\t<key>Minute</key>\n\t\t<integer>{minute}</integer>\n\
\t</dict>\n\
\t<key>EnvironmentVariables</key>\n\t<dict>\n\t\t<key>GREENCODE_HOME</key>\n\t\t<string>{home}</string>\n{path}\t</dict>\n\
\t<key>RunAtLoad</key>\n\t<false/>\n\
\t<key>StandardOutPath</key>\n\t<string>{log}</string>\n\
\t<key>StandardErrorPath</key>\n\t<string>{log}</string>\n\
</dict>\n</plist>\n",
        escape(label)
    )
}

fn launchctl_enabled() -> bool {
    !cfg!(test) && std::env::var_os("GREENCODE_NO_LAUNCHCTL").is_none()
}

fn domain() -> Result<String> {
    let out = std::process::Command::new("id").arg("-u").output().context("id -u")?;
    Ok(format!("gui/{}", String::from_utf8_lossy(&out.stdout).trim()))
}

/// Writes `<dir>/<label>.plist` and loads it with `launchctl bootstrap`.
pub fn install(dir: &Path, label: &str, plist: &str) -> Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    let path = dir.join(format!("{label}.plist"));
    std::fs::write(&path, plist)?;
    if launchctl_enabled() {
        let status = std::process::Command::new("launchctl").arg("bootstrap").arg(domain()?).arg(&path).status().context("launchctl")?;
        if !status.success() {
            let _ = std::fs::remove_file(&path);
            anyhow::bail!("launchctl bootstrap failed: {status}");
        }
    }
    Ok(path)
}

/// Unloads the job (if loaded) and deletes its plist. Missing jobs are not an error.
pub fn uninstall(dir: &Path, label: &str) -> Result<()> {
    let path = dir.join(format!("{label}.plist"));
    if launchctl_enabled() {
        let _ = std::process::Command::new("launchctl").arg("bootout").arg(format!("{}/{label}", domain()?)).status();
    }
    match std::fs::remove_file(&path) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.into()),
        _ => Ok(()),
    }
}

/// `~/Library/LaunchAgents` in the real home directory.
pub fn agents_dir() -> PathBuf {
    crate::user_home().join("Library/LaunchAgents")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plist_contains_label_args_time_and_log() {
        let args = vec!["/bin/x y".to_string(), "-p".to_string(), "a<b & c".to_string()];
        let p = plist("dev.greencode.schedule.t-1", &args, (11, 14, 9, 5), Path::new("/tmp/l.log"));
        for s in ["<string>dev.greencode.schedule.t-1</string>", "<string>/bin/x y</string>", "a&lt;b &amp; c",
            "<key>Month</key>\n\t\t<integer>11</integer>", "<key>Day</key>\n\t\t<integer>14</integer>",
            "<key>Hour</key>\n\t\t<integer>9</integer>", "<key>Minute</key>\n\t\t<integer>5</integer>",
            "<key>RunAtLoad</key>\n\t<false/>", "<key>StandardOutPath</key>\n\t<string>/tmp/l.log</string>",
            "<key>StandardErrorPath</key>", "<key>EnvironmentVariables</key>", "<key>GREENCODE_HOME</key>", "<key>PATH</key>"] {
            assert!(p.contains(s), "{s}\n{p}");
        }
        assert!(p.starts_with("<?xml") && p.trim_end().ends_with("</plist>"));
    }

    #[test]
    fn install_and_uninstall_manage_the_plist_file() {
        let dir = std::env::temp_dir().join(format!("greencode-launchd-{}", std::process::id()));
        let path = install(&dir, "dev.greencode.schedule.a-1", "<plist/>").unwrap();
        assert_eq!(path, dir.join("dev.greencode.schedule.a-1.plist"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "<plist/>");
        uninstall(&dir, "dev.greencode.schedule.a-1").unwrap();
        assert!(!path.exists());
        uninstall(&dir, "dev.greencode.schedule.a-1").unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }
    #[test]
    fn scheduled_claim_survives_concurrency_and_failure() {
        use std::io::{BufRead, Write};
        if let Some(dir) = std::env::var_os("GREENCODE_CLAIM_TEST_CHILD") {
            let dir = PathBuf::from(dir);
            let _claim = claim_schedule(&dir, "job").unwrap().unwrap();
            println!("claimed");
            std::io::stdout().flush().unwrap();
            let mut line = String::new();
            std::io::stdin().lock().read_line(&mut line).unwrap();
            return; // Simulates a failed turn: the claim is dropped without completion.
        }
        let dir = std::env::temp_dir().join(format!("greencode-claim-{}-{}", std::process::id(), std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
        std::fs::create_dir(&dir).unwrap();
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "launchd::tests::scheduled_claim_survives_concurrency_and_failure", "--nocapture"])
            .env("GREENCODE_CLAIM_TEST_CHILD", &dir)
            .stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped()).spawn().unwrap();
        let mut output = std::io::BufReader::new(child.stdout.take().unwrap());
        loop {
            let mut line = String::new();
            assert_ne!(output.read_line(&mut line).unwrap(), 0, "child exited before claiming");
            if line.contains("claimed") { break; }
        }
        assert!(claim_schedule(&dir, "job").unwrap().is_none());
        // The child is held at a pipe barrier with its claim locked.
        assert!(cancel_schedule(&dir, "job").is_err());
        assert!(!dir.join("job.cancelled").exists());
        assert!(!schedule_failed(&dir, "job").unwrap());
        writeln!(child.stdin.take().unwrap(), "finish").unwrap();
        assert!(child.wait().unwrap().success());
        assert!(schedule_failed(&dir, "job").unwrap());
        assert!(dir.join("job.failed").exists());
        assert!(!dir.join("job.done").exists());
        assert!(claim_schedule(&dir, "job").unwrap().is_none());
        // The kernel can release the child's lock a moment after `wait` returns; retry briefly.
        let mut cancelled = cancel_schedule(&dir, "job");
        for _ in 0..100 {
            if cancelled.is_ok() { break; }
            std::thread::sleep(std::time::Duration::from_millis(20));
            cancelled = cancel_schedule(&dir, "job");
        }
        cancelled.unwrap();
        assert!(dir.join("job.cancelled").exists());
        claim_schedule(&dir, "success").unwrap().unwrap().complete().unwrap();
        assert!(dir.join("success.done").exists());
        assert!(claim_schedule(&dir, "success").unwrap().is_none());
        cancel_schedule(&dir, "cancelled").unwrap();
        assert!(claim_schedule(&dir, "cancelled").unwrap().is_none());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
