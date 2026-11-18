use super::{Act, Workspace};
use gpui_kit::*;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::sync::oneshot;

pub(super) struct VoiceRecording {
    pub(super) stop: oneshot::Sender<bool>,
    pub(super) id: u64,
}

static NEXT: AtomicU64 = AtomicU64::new(0);

impl Workspace {
    pub(super) fn toggle_voice(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(recording) = self.voice_recording.take() {
            let _ = recording.stop.send(true);
            cx.notify();
            return;
        }
        let provider = self.ui["voice_provider"]
            .as_str()
            .unwrap_or("off")
            .to_string();
        if !matches!(provider.as_str(), "openai" | "groq" | "local") {
            if self.settings.is_none() {
                self.run(Act::Settings, window, cx);
            }
            self.notice("設定で文字起こし先を選んでください");
            cx.notify();
            return;
        }
        if cfg!(test) {
            return;
        }
        let id = NEXT.fetch_add(1, Ordering::Relaxed);
        let thread_id = self.thread().meta.id.clone();
        let cfg = self.cfg.clone();
        let (stop, rx) = oneshot::channel();
        self.voice_recording = Some(VoiceRecording { stop, id });
        let task = self.rt.spawn(async move {
            let dir = std::env::temp_dir().join(format!("greencode-voice-{}-{id}", std::process::id()));
            let mut builder = std::fs::DirBuilder::new();
            #[cfg(unix)] {
                use std::os::unix::fs::DirBuilderExt;
                builder.mode(0o700);
            }
            builder.create(&dir).map_err(|e| e.to_string())?;
            let result: Result<Option<String>, String> = async {
                let path = dir.join("recording.m4a");
                let mut recording = greencode_core::voice::start(&path).await.map_err(|e| format!("{e:#}"))?;
                let stop = tokio::select! {
                    result = recording.wait() => return Err(result.err().map(|e| format!("{e:#}")).unwrap_or_else(|| "録音が予期せず終了しました".into())),
                    result = rx => result.unwrap_or(false),
                };
                if stop {
                    let path = recording.stop().await.map_err(|e| format!("{e:#}"))?;
                    let text = if provider == "local" {
                        greencode_core::voice::transcribe_local(&path, None).await
                    } else {
                        greencode_core::voice::transcribe(&cfg, &provider, &path, None).await
                    }.map_err(|e| format!("{e:#}"))?;
                    Ok(Some(text))
                } else { drop(recording); Ok(None) }
            }.await;
            let _ = std::fs::remove_dir_all(dir);
            result
        });
        cx.spawn_in(window, async move |this, cx| {
            let result = match task.await {
                Ok(result) => result,
                Err(e) => Err(e.to_string()),
            };
            this.update_in(cx, |this, window, cx| {
                if this.voice_recording.as_ref().is_some_and(|r| r.id == id) {
                    this.voice_recording = None;
                }
                match result {
                    Ok(Some(text)) => {
                        if let Some(ix) = this.threads.iter().position(|t| t.meta.id == thread_id) {
                            let draft = if ix == this.active {
                                this.input.read(cx).value().to_string()
                            } else {
                                this.threads[ix].draft.clone()
                            };
                            let separator = if draft.is_empty()
                                || draft.ends_with(char::is_whitespace)
                                || text.is_empty()
                            {
                                ""
                            } else {
                                " "
                            };
                            let draft = format!("{draft}{separator}{text}");
                            this.threads[ix].draft = draft.clone();
                            if ix == this.active {
                                this.input
                                    .update(cx, |s, cx| super::set_input(s, draft, window, cx));
                            }
                            if let Err(e) = this.threads[ix].save() {
                                this.fail(e);
                            }
                        }
                    }
                    Ok(None) => {}
                    Err(e) => this.fail(format!("音声入力に失敗しました: {e}")),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    pub(super) fn cancel_voice(&mut self, cx: &mut Context<Self>) {
        if let Some(recording) = self.voice_recording.take() {
            let _ = recording.stop.send(false);
        }
        cx.notify();
    }
}
