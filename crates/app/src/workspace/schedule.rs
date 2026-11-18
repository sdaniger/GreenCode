//! `/schedule HH:MM|+30m|+2h プロンプト`: one-shot prompts sent to a thread at a later time while the app runs.
//! Stored in `ui["reminders"]` as `{thread, at, prompt}`. (`ui["schedules"]` is the recurring `/schedule 30m` feature.)
use super::*;

/// Local offset from UTC in seconds, read once from `/bin/date +%z` (`+0900`); 0 when unavailable.
pub(super) fn utc_offset() -> i64 {
    static OFFSET: std::sync::LazyLock<i64> = std::sync::LazyLock::new(|| {
        std::process::Command::new("/bin/date")
            .arg("+%z")
            .output()
            .ok()
            .and_then(|o| parse_offset(String::from_utf8_lossy(&o.stdout).trim()))
            .unwrap_or(0)
    });
    *OFFSET
}

fn parse_offset(z: &str) -> Option<i64> {
    let (sign, digits) = match z.split_at_checked(1)? {
        ("+", d) => (1, d),
        ("-", d) => (-1, d),
        _ => return None,
    };
    if digits.len() != 4 || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some(sign * (digits[..2].parse::<i64>().ok()? * 3600 + digits[2..].parse::<i64>().ok()? * 60))
}

/// Unix time for `30m`, `2h` (optionally `+`-prefixed: relative) or `HH:MM` (next occurrence in local time at `offset_secs`).
pub(super) fn parse_when(s: &str, now: u64, offset_secs: i64) -> Option<u64> {
    let s = s.trim();
    let rel = s.strip_prefix('+').unwrap_or(s);
    if let Some((n, unit)) = rel
        .strip_suffix('m')
        .map(|n| (n, 60))
        .or(rel.strip_suffix('h').map(|n| (n, 3600)))
    {
        if n.bytes().all(|b| b.is_ascii_digit()) && !n.is_empty() {
            return Some(
                now + n
                    .parse::<u64>()
                    .ok()
                    .filter(|n| *n > 0)?
                    .checked_mul(unit)?,
            );
        }
        return None;
    }
    let (h, m) = s.split_once(':')?;
    if h.is_empty() || m.len() != 2 || !h.bytes().chain(m.bytes()).all(|b| b.is_ascii_digit()) {
        return None;
    }
    let (h, m) = (
        h.parse::<i64>().ok().filter(|h| *h < 24)?,
        m.parse::<i64>().ok().filter(|m| *m < 60)?,
    );
    let local = now as i64 + offset_secs;
    let at = local - local.rem_euclid(86400) + h * 3600 + m * 60 - offset_secs;
    Some(if at <= now as i64 { at + 86400 } else { at } as u64)
}

fn clock(at: u64, offset_secs: i64) -> String {
    let t = (at as i64 + offset_secs).rem_euclid(86400);
    format!("{:02}:{:02}", t / 3600, t % 3600 / 60)
}

/// (month, day) of the civil date `days` after 1970-01-01.
fn month_day(days: i64) -> (u32, u32) {
    let z = days + 719468;
    let doe = z.rem_euclid(146097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    (if mp < 10 { mp + 3 } else { mp - 9 } as u32, day)
}

/// `(month, day, hour, minute)` in local time at `offset_secs`.
fn calendar(at: u64, offset_secs: i64) -> (u32, u32, u32, u32) {
    let local = at as i64 + offset_secs;
    let (m, d) = month_day(local.div_euclid(86400));
    let t = local.rem_euclid(86400);
    (m, d, (t / 3600) as u32, (t % 3600 / 60) as u32)
}

/// The CLI next to this executable, else the first `greencode-cli` on `PATH`.
fn cli_path() -> Option<std::path::PathBuf> {
    let sibling = std::env::current_exe()
        .ok()
        .and_then(|e| e.parent().map(|d| d.join("greencode-cli")))
        .filter(|p| p.is_file());
    sibling.or_else(|| {
        std::env::split_paths(&std::env::var_os("PATH")?)
            .map(|d| d.join("greencode-cli"))
            .find(|p| p.is_file())
    })
}

fn mode_arg(m: Mode) -> &'static str {
    match m {
        Mode::Ask => "ask",
        Mode::AutoEdit => "auto-edit",
        Mode::Full => "full",
        Mode::Plan => "plan",
    }
}

impl Workspace {
    /// `ui["launch_agents_dir"]` (tests) or `~/Library/LaunchAgents`.
    fn launch_agents_dir(&self) -> std::path::PathBuf {
        self.ui["launch_agents_dir"]
            .as_str()
            .map(Into::into)
            .unwrap_or_else(greencode_core::launchd::agents_dir)
    }

    /// Installs the launchd job that runs the prompt through the CLI at `at`; returns its label.
    fn install_schedule_job(
        &mut self,
        at: u64,
        prompt: &str,
        offset: i64,
    ) -> Result<String, String> {
        let cli = self.ui["cli_path"]
            .as_str()
            .map(std::path::PathBuf::from)
            .or_else(cli_path)
            .ok_or("greencode-cli が見つかりません")?;
        self.thread().save()?;
        if !Session::path(&self.thread().meta.id).exists() {
            return Err(String::new());
        }
        let (id, mode) = (self.thread().meta.id.clone(), self.thread().perms.mode());
        // Two prompts for the same thread and minute must not share a job or its `.done` marker.
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        let mut label = format!("dev.greencode.schedule.{id}-{at}-{nonce:x}");
        for n in 1.. {
            if !self
                .launch_agents_dir()
                .join(format!("{label}.plist"))
                .exists()
            {
                break;
            }
            label = format!("dev.greencode.schedule.{id}-{at}-{nonce:x}-{n}");
        }
        let dir = greencode_core::home().join("scheduled");
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        let agents = self.launch_agents_dir();
        let args: Vec<String> = [
            &*cli.to_string_lossy(),
            "--scheduled",
            &label,
            "--launch-agents-dir",
            &*agents.to_string_lossy(),
            "--resume",
            &id,
            "--mode",
            mode_arg(mode),
            "-p",
            prompt,
        ]
        .map(String::from)
        .into();
        let plist = greencode_core::launchd::plist(
            &label,
            &args,
            calendar(at, offset),
            &dir.join(format!("{label}.log")),
        );
        greencode_core::launchd::install(&self.launch_agents_dir(), &label, &plist)
            .map_err(|e| format!("{e:#}"))?;
        Ok(label)
    }

    /// Handles `/schedule` arguments that are one-shot times; false leaves them to the recurring form.
    /// `background`: also install a launchd job (`/schedule!`) so it runs while the app is closed.
    pub(super) fn schedule_once(&mut self, arg: &str, background: bool) -> bool {
        let (when, prompt) = arg.split_once(char::is_whitespace).unwrap_or((arg, ""));
        if !(when.starts_with('+') || when.contains(':')) && !background {
            return false;
        }
        let (prompt, offset) = (prompt.trim(), utc_offset());
        let Some(at) = parse_when(when, now(), offset).filter(|_| !prompt.is_empty()) else {
            self.fail("使い方: /schedule HH:MM プロンプト、/schedule +30m プロンプト、/schedule +2h プロンプト（/schedule! でアプリ終了中も実行）");
            return true;
        };
        if !self.ui["reminders"].is_array() {
            self.ui["reminders"] = json!([]);
        }
        let mut entry = json!({"thread": self.thread().meta.id, "at": at, "prompt": prompt});
        let mut note = format!(
            "{} にこのスレッドへ送信します（GreenCode の起動中のみ）",
            clock(at, offset)
        );
        if background {
            match self.install_schedule_job(at, prompt, offset) {
                Ok(label) => {
                    entry["label"] = json!(label);
                    entry["mode"] = json!(mode_arg(self.thread().perms.mode()));
                    note = format!(
                        "{} にこのスレッドへ送信します（アプリ終了中も launchd で実行）",
                        clock(at, offset)
                    );
                }
                Err(e) if e.is_empty() => {
                    self.fail("先に一度メッセージを送ってから /schedule! を使ってください");
                    return true;
                }
                Err(e) => self.fail(format!(
                    "launchd の登録に失敗しました。アプリ内の予約のみ登録します: {e}"
                )),
            }
        }
        self.ui["reminders"].as_array_mut().unwrap().push(entry);
        self.save_ui();
        self.notice(note);
        true
    }

    /// Palette rows `予約: HH:MM prompt（取り消す）`, one per stored reminder.
    pub(super) fn reminder_rows(&self) -> Vec<(String, Act)> {
        let offset = utc_offset();
        self.ui["reminders"]
            .as_array()
            .into_iter()
            .flatten()
            .enumerate()
            .map(|(i, r)| {
                let prefix: String = r["prompt"]
                    .as_str()
                    .unwrap_or("")
                    .chars()
                    .take(40)
                    .collect();
                (
                    format!(
                        "予約: {} {}（取り消す）",
                        clock(r["at"].as_u64().unwrap_or(0), offset),
                        prefix
                    ),
                    Act::Unschedule(i),
                )
            })
            .collect()
    }

    pub(super) fn unschedule(&mut self, index: usize) {
        let Some(removed) = self.ui["reminders"]
            .as_array()
            .and_then(|l| l.get(index))
            .cloned()
        else {
            return;
        };
        if let Some(label) = removed["label"].as_str() {
            if let Err(e) = greencode_core::launchd::cancel_schedule(
                &greencode_core::home().join("scheduled"),
                label,
            ) {
                self.fail(format!("予約を取り消せません: {e:#}"));
                return;
            }
            if let Err(e) = greencode_core::launchd::uninstall(&self.launch_agents_dir(), label) {
                self.fail(format!("launchd のジョブを削除できません: {e:#}"));
            }
        }
        self.ui["reminders"].as_array_mut().unwrap().remove(index);
        self.save_ui();
    }

    /// Sends every reminder due at `now`: started in its thread when idle, queued while it is running.
    pub(super) fn run_due_reminders(&mut self, now: u64, cx: &mut Context<Self>) {
        let scheduled = greencode_core::home().join("scheduled");
        let Some(list) = self.ui["reminders"].as_array_mut() else {
            return;
        };
        let before = list.len();
        list.retain(|r| {
            r["label"]
                .as_str()
                .is_none_or(|l| !scheduled.join(format!("{l}.done")).exists())
        });
        let retired = before != list.len();
        let (due, keep): (Vec<Value>, Vec<Value>) = std::mem::take(list)
            .into_iter()
            .partition(|r| !r["label"].is_string() && r["at"].as_u64().unwrap_or(0) <= now);
        *list = keep;
        let background: Vec<Value> = list
            .iter()
            .filter(|r| {
                r["label"].is_string() && r["at"].as_u64().unwrap_or(0).saturating_add(600) <= now
            })
            .cloned()
            .collect();
        if retired || !due.is_empty() {
            self.save_ui();
        }
        for r in background {
            let label = r["label"].as_str().unwrap();
            let failure = match greencode_core::launchd::schedule_failed(&scheduled, label) {
                Ok(true) => Some("実行に失敗または中断しました。操作が一部実行された可能性があるため自動再試行しません".to_string()),
                Err(e) => Some(format!("実行状態を確認できません: {e:#}")),
                Ok(false) if !scheduled.join(format!("{label}.started")).exists() => {
                    let cli = self.ui["cli_path"].as_str().map(std::path::PathBuf::from).or_else(cli_path);
                    let id = r["thread"].as_str().unwrap_or("");
                    let mode = r["mode"].as_str().map(str::to_string).or_else(|| self.threads.iter().find(|t| t.meta.id == id).map(|t| mode_arg(t.perms.mode()).to_string()));
                    match (cli, mode, r["prompt"].as_str()) {
                        (Some(cli), Some(mode), Some(prompt)) => {
                            let log = std::fs::OpenOptions::new().create(true).append(true).open(scheduled.join(format!("{label}.log")));
                            match log.and_then(|log| {
                                let err = log.try_clone()?;
                                std::process::Command::new(cli).args(["--scheduled", label, "--launch-agents-dir"]).arg(self.launch_agents_dir()).args(["--resume", id, "--mode", &mode, "-p", prompt])
                                    .stdin(std::process::Stdio::null()).stdout(log).stderr(err).spawn()
                            }) {
                                Ok(mut child) => {
                                    let dir = scheduled.clone();
                                    let label = label.to_string();
                                    std::thread::spawn(move || {
                                        if !child.wait().is_ok_and(|status| status.success()) && !dir.join(format!("{label}.started")).exists() {
                                            let _ = std::fs::write(dir.join(format!("{label}.failed")), "The scheduled CLI failed before claiming the turn.");
                                        }
                                    });
                                    None
                                }
                                Err(e) => Some(format!("予約を起動できません: {e}")),
                            }
                        }
                        _ => Some("予約の CLI、スレッドまたはプロンプトが見つかりません".to_string()),
                    }
                }
                Ok(false) => None,
            };
            if let Some(message) = failure {
                if let Some(entry) = self.ui["reminders"]
                    .as_array_mut()
                    .and_then(|list| list.iter_mut().find(|v| v["label"].as_str() == Some(label)))
                {
                    if entry["failure_notified"] != true {
                        entry["failure_notified"] = json!(true);
                        self.save_ui();
                        self.fail(format!("{label}: {message}"));
                    }
                }
            }
        }
        if due.is_empty() {
            return;
        }
        let mut sent = false;
        for r in due {
            // Unlabelled reminders are delivered in the GUI as before.
            let (Some(id), Some(prompt)) = (r["thread"].as_str(), r["prompt"].as_str()) else {
                continue;
            };
            let Some(ix) = self.threads.iter().position(|t| t.meta.id == id) else {
                continue;
            };
            if self.threads[ix].status == Status::Idle {
                self.threads[ix].queue_held = false;
                self.start_in(ix, prompt.to_string(), cx);
            } else {
                self.threads[ix].queue.push_back(prompt.to_string());
                if let Err(e) = self.threads[ix].save() {
                    self.fail(e);
                }
            }
            sent = true;
        }
        if sent {
            os_notify("予約したプロンプトを送信しました", cx);
            cx.notify();
        }
    }
}

#[cfg(test)]
mod tests {
    // Not `super::*`: that glob carries gpui_kit's `test` attribute, which shadows the builtin and recurses.
    use super::{clock, parse_offset, parse_when};

    #[test]
    fn schedule_parse_when_handles_relative_and_clock_times() {
        let now = 1_700_000_000; // 2023-11-14 22:13:20 UTC = 2023-11-15 07:13:20 at +09:00
        assert_eq!(parse_when("30m", now, 0), Some(now + 1800));
        assert_eq!(parse_when("+2h", now, 0), Some(now + 7200));
        let off = 32400;
        let local_midnight = now as i64 - (now as i64 + off).rem_euclid(86400);
        assert_eq!(
            parse_when("09:00", now, off),
            Some((local_midnight + 9 * 3600) as u64)
        );
        assert_eq!(
            parse_when("07:00", now, off),
            Some((local_midnight + 86400 + 7 * 3600) as u64)
        );
        for bad in [
            "", "0m", "m", "25:00", "09:60", "9", "ab:cd", "9:5", "-5m", "1d",
        ] {
            assert_eq!(parse_when(bad, now, off), None, "{bad}");
        }
        assert_eq!(parse_offset("+0900"), Some(32400));
        assert_eq!(parse_offset("-0330"), Some(-12600));
        assert_eq!(parse_offset("x"), None);
        assert_eq!(clock(now, off), "07:13");
    }
}
