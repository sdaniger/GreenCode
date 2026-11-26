//! SnapShot (macOS): a global shortcut captures the frontmost window and attaches it to the draft.
//! `snapshot.swift` owns the hotkey and the capture; this module starts it, asks for permissions and attaches
//! what it leaves in `~/.greencode/captures`. A capture is pending while its `.json` marker exists, so one
//! taken while the app was closing is attached on the next launch.
use super::*;
use std::path::Path;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;

const MAX_BYTES: u64 = 10 * 1024 * 1024;
/// Helper names, labels. Shift + Shift is T3's default; it needs Input Monitoring, the chords do not.
const SHORTCUTS: [(&str, &str); 3] = [
    ("shift", "Shift + Shift"),
    ("ctrl-opt-cmd-s", "⌃⌥⌘S"),
    ("ctrl-opt-cmd-g", "⌃⌥⌘G"),
];

fn captures() -> PathBuf {
    greencode_core::home().join("captures")
}

fn shortcut_index(ui: &Value) -> usize {
    SHORTCUTS
        .iter()
        .position(|(k, _)| ui["snapshot_shortcut"] == *k)
        .unwrap_or(0)
}

pub(super) fn shortcut_label(ui: &Value) -> &'static str {
    SHORTCUTS[shortcut_index(ui)].1
}

/// Permissions the current settings rely on: (helper name, settings pane, label).
fn needed(ui: &Value) -> Vec<(&'static str, &'static str, &'static str)> {
    let mut v = vec![("screen", "Privacy_ScreenCapture", "画面収録")];
    if ui["snapshot_text"] == true {
        v.push(("accessibility", "Privacy_Accessibility", "アクセシビリティ"));
    }
    if SHORTCUTS[shortcut_index(ui)].0 == "shift" {
        v.push(("input", "Privacy_ListenEvent", "入力監視"));
    }
    v
}

struct Pending {
    png: PathBuf,
    marker: PathBuf,
    app: String,
    title: String,
    text: String,
}

/// Message text that tells the agent what the screenshot shows.
fn describe(p: &Pending) -> String {
    let mut s = format!("スナップショット: {}", p.app);
    if !p.title.is_empty() {
        s.push_str(&format!(" — {}", p.title));
    }
    if !p.text.trim().is_empty() {
        // A fence in the window text must not end ours.
        s.push_str(&format!(
            "\nウィンドウのテキスト:\n```\n{}\n```",
            p.text.trim().replace("```", "'''")
        ));
    }
    s
}

/// Completed captures, oldest first. Oversized or unreadable ones are deleted; the first list is what to attach,
/// the second the notices for what was discarded.
fn pending(dir: &Path) -> (Vec<Pending>, Vec<String>) {
    let mut markers: Vec<PathBuf> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "json"))
        .collect();
    markers.sort();
    let (mut ready, mut discarded) = (vec![], vec![]);
    for marker in markers {
        let png = marker.with_extension("png");
        let meta: Value = std::fs::read(&marker)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        let field = |k: &str| meta[k].as_str().unwrap_or_default().to_string();
        match std::fs::metadata(&png) {
            Ok(m) if m.len() > MAX_BYTES => {
                let _ = std::fs::remove_file(&png);
                let _ = std::fs::remove_file(&marker);
                discarded.push(format!(
                    "{} のスナップショットは 10 MiB を超えているため破棄しました",
                    field("app")
                ));
            }
            Ok(_) => ready.push(Pending {
                png,
                marker,
                app: field("app"),
                title: field("title"),
                text: field("text"),
            }),
            Err(_) => {
                let _ = std::fs::remove_file(&marker);
            }
        }
    }
    (ready, discarded)
}

impl Workspace {
    /// Launch hook: attaches captures left from the last run and starts the shortcut when it is on.
    pub(super) fn snapshot_start(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if cfg!(test) {
            return;
        }
        self.attach_pending(&captures(), window, cx);
        self.snapshot_sync(window, cx);
    }

    /// Makes the helper process match the settings: stopped when off, restarted otherwise.
    fn snapshot_sync(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let restart = self.snapshot.take().is_some_and(|h| {
            h.abort(); // drops the helper, which unregisters the shortcut
            true
        });
        if cfg!(test) || !cfg!(target_os = "macos") || self.ui["snapshot"] != true {
            return;
        }
        let args = [
            "run".to_string(),
            SHORTCUTS[shortcut_index(&self.ui)].0.into(),
            if self.ui["snapshot_text"] == true { "1" } else { "0" }.into(),
            captures().display().to_string(),
        ];
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        self.snapshot = Some(
            self.rt
                .spawn(async move {
                    // The old helper needs a moment to die and free the hotkey before the new one registers it.
                    if restart {
                        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                    }
                    let result: Result<(), String> = async {
                        let bin = greencode_core::tools::computer::snapshot_helper()
                            .await
                            .map_err(|e| format!("{e:#}"))?;
                        let mut child = Command::new(bin)
                            .args(args)
                            .stdin(std::process::Stdio::piped())
                            .stdout(std::process::Stdio::piped())
                            .stderr(std::process::Stdio::null())
                            .kill_on_drop(true)
                            .spawn()
                            .map_err(|e| e.to_string())?;
                        // The helper exits when this pipe closes, even if the app is killed.
                        let _stdin = child.stdin.take();
                        let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
                        while let Some(line) = lines.next_line().await.map_err(|e| e.to_string())? {
                            let _ = tx.send(line);
                        }
                        Err("撮影ヘルパーが終了しました".into())
                    }
                    .await;
                    if let Err(e) = result {
                        // "error" is one failed press; "fatal" means no helper is running any more.
                        let _ = tx.send(format!("fatal {e}"));
                    }
                })
                .abort_handle(),
        );
        cx.spawn_in(window, async move |this, cx| {
            while let Some(line) = rx.recv().await {
                let alive = this
                    .update_in(cx, |this, window, cx| {
                        if line == "capture" {
                            this.attach_pending(&captures(), window, cx);
                            cx.activate(true);
                            window.activate_window();
                        } else if let Some(e) = line.strip_prefix("fatal ") {
                            if let Some(h) = this.snapshot.take() {
                                h.abort();
                            }
                            this.ui["snapshot"] = json!(false);
                            this.save_ui();
                            this.fail(format!("スナップショットを開始できません: {e}"));
                        } else {
                            this.fail(format!(
                                "スナップショット: {}",
                                line.strip_prefix("error ").unwrap_or(&line)
                            ));
                        }
                        cx.notify();
                    })
                    .is_ok();
                if !alive {
                    break;
                }
            }
        })
        .detach();
    }

    /// Attaches every pending capture to the current draft: the image, then its description.
    fn attach_pending(&mut self, dir: &Path, window: &mut Window, cx: &mut Context<Self>) {
        let (ready, discarded) = pending(dir);
        for notice in discarded {
            self.notice(notice);
        }
        for p in ready {
            self.attach_paths(vec![p.png.clone()], window, cx);
            let value = format!("{}\n{}\n", self.input.read(cx).value(), describe(&p));
            self.input
                .update(cx, |s, cx| set_input(s, value.clone(), window, cx));
            let thread = &mut self.threads[self.active];
            thread.draft = value;
            match thread.save() {
                // The draft holds the capture now; the marker was all that kept it pending.
                Ok(()) => {
                    let _ = std::fs::remove_file(&p.marker);
                }
                Err(e) => self.fail(e),
            }
        }
        cx.notify();
    }

    pub(super) fn snapshot_toggle(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.ui["snapshot"] == true {
            self.ui["snapshot"] = json!(false);
            self.save_ui();
            self.snapshot_sync(window, cx);
            self.notice("スナップショットをオフにしました（ショートカットを解除）");
            cx.notify();
        } else {
            self.snapshot_enable(window, cx);
        }
    }

    /// Setting changed (text, shortcut) while on: re-check permissions, then restart.
    pub(super) fn snapshot_changed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.ui["snapshot"] == true {
            self.snapshot_enable(window, cx);
        }
    }

    /// Asks macOS for the permissions the settings need; on success turns capture on, otherwise leaves it off
    /// and opens the matching System Settings pane.
    fn snapshot_enable(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !cfg!(target_os = "macos") {
            self.notice("スナップショットは macOS のみ対応しています");
            cx.notify();
            return;
        }
        if cfg!(test) {
            return;
        }
        let need = needed(&self.ui);
        let names: Vec<&str> = need.iter().map(|n| n.0).collect();
        let task = self.rt.spawn({
            let mut args = vec!["request".to_string()];
            args.extend(names.iter().map(|n| n.to_string()));
            async move {
                let bin = greencode_core::tools::computer::snapshot_helper()
                    .await
                    .map_err(|e| format!("{e:#}"))?;
                let out = Command::new(bin)
                    .args(args)
                    .output()
                    .await
                    .map_err(|e| e.to_string())?;
                serde_json::from_slice::<Vec<String>>(&out.stdout).map_err(|e| e.to_string())
            }
        });
        cx.spawn_in(window, async move |this, cx| {
            let missing = task.await.map_err(|e| e.to_string()).and_then(|r| r);
            this.update_in(cx, |this, window, cx| {
                match missing {
                    Err(e) => {
                        this.ui["snapshot"] = json!(false);
                        this.fail(format!("スナップショットを有効にできません: {e}"));
                    }
                    Ok(missing) if missing.is_empty() => {
                        this.ui["snapshot"] = json!(true);
                        this.notice(format!(
                            "スナップショットをオンにしました。他のアプリで {} を押すとウィンドウを撮影して添付します",
                            shortcut_label(&this.ui)
                        ));
                    }
                    Ok(missing) => {
                        this.ui["snapshot"] = json!(false);
                        let labels: Vec<_> = need
                            .iter()
                            .filter(|n| missing.iter().any(|m| m == n.0))
                            .map(|n| {
                                spawn_external(
                                    "/usr/bin/open",
                                    &[&format!("x-apple.systempreferences:com.apple.preference.security?{}", n.1)],
                                );
                                n.2
                            })
                            .collect();
                        this.notice(format!(
                            "スナップショットには「{}」の許可が必要です。システム設定で GreenCode を許可してから、もう一度オンにしてください（反映にアプリの再起動が必要な場合があります）",
                            labels.join("」「")
                        ));
                    }
                }
                this.save_ui();
                this.snapshot_sync(window, cx);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    pub(super) fn snapshot_cycle_shortcut(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let next = SHORTCUTS[(shortcut_index(&self.ui) + 1) % SHORTCUTS.len()].0;
        self.ui["snapshot_shortcut"] = json!(next);
        self.save_ui();
        self.snapshot_changed(window, cx);
        cx.notify();
    }

    pub(super) fn snapshot_toggle_text(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.ui["snapshot_text"] = json!(self.ui["snapshot_text"] != true);
        self.save_ui();
        self.snapshot_changed(window, cx);
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::{describe, needed, pending, Pending, MAX_BYTES};
    use crate::workspace::tests::open;
    use gpui_kit::{TestAppContext, VisualTestContext};
    use serde_json::{json, Value};
    use std::path::{Path, PathBuf};

    fn capture(dir: &Path, name: &str, png_len: usize, meta: Value) {
        std::fs::write(dir.join(format!("{name}.png")), vec![0u8; png_len]).unwrap();
        std::fs::write(dir.join(format!("{name}.json")), meta.to_string()).unwrap();
    }

    fn temp(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("greencode-snap-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn describes_app_title_and_text() {
        let p = |title: &str, text: &str| Pending {
            png: "a.png".into(),
            marker: "a.json".into(),
            app: "Safari".into(),
            title: title.into(),
            text: text.into(),
        };
        assert_eq!(describe(&p("", "")), "スナップショット: Safari");
        assert_eq!(describe(&p("Docs", "  ")), "スナップショット: Safari — Docs");
        assert_eq!(
            describe(&p("Docs", "a\n```x```")),
            "スナップショット: Safari — Docs\nウィンドウのテキスト:\n```\na\n'''x'''\n```"
        );
    }

    #[test]
    fn permissions_follow_the_settings() {
        let names = |ui: &Value| needed(ui).iter().map(|n| n.0).collect::<Vec<_>>();
        assert_eq!(names(&json!({})), ["screen", "input"]);
        assert_eq!(
            names(&json!({"snapshot_shortcut": "ctrl-opt-cmd-s", "snapshot_text": true})),
            ["screen", "accessibility"]
        );
    }

    #[test]
    fn pending_keeps_complete_captures_and_discards_oversized() {
        let dir = temp("pending");
        capture(&dir, "snap-1", 4, json!({"app": "A", "title": "t", "text": "x"}));
        capture(&dir, "snap-2", MAX_BYTES as usize + 1, json!({"app": "Big"}));
        // No marker yet: still being written, left alone.
        std::fs::write(dir.join("snap-3.png.tmp"), "x").unwrap();
        std::fs::write(dir.join("snap-4.png"), "x").unwrap();
        // Marker whose image is gone.
        std::fs::write(dir.join("snap-5.json"), "{}").unwrap();
        let (ready, discarded) = pending(&dir);
        assert_eq!(ready.len(), 1);
        assert_eq!((ready[0].app.as_str(), ready[0].text.as_str()), ("A", "x"));
        assert_eq!(discarded.len(), 1);
        assert!(discarded[0].contains("Big"), "{discarded:?}");
        assert!(!dir.join("snap-2.png").exists() && !dir.join("snap-5.json").exists());
        assert!(dir.join("snap-3.png.tmp").exists() && dir.join("snap-4.png").exists());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui_kit::test]
    async fn pending_capture_is_attached_to_the_draft_once(cx: &mut TestAppContext) {
        let (ws, cx) = open(cx);
        let dir = temp("attach");
        capture(&dir, "snap-1", 4, json!({"app": "Notes", "title": "Todo", "text": ""}));
        let attach = |cx: &mut VisualTestContext| {
            cx.update(|window, cx| ws.update(cx, |ws, cx| ws.attach_pending(&dir, window, cx)))
        };
        attach(cx);
        let value = ws.read_with(cx, |ws, cx| ws.input.read(cx).value().to_string());
        assert!(value.contains("snap-1.png"), "{value}");
        assert!(value.contains("スナップショット: Notes — Todo"), "{value}");
        assert!(!dir.join("snap-1.json").exists());
        attach(cx);
        let again = ws.read_with(cx, |ws, cx| ws.input.read(cx).value().to_string());
        assert_eq!(again, value);
        let _ = std::fs::remove_dir_all(dir);
    }
}
