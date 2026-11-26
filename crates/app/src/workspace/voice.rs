use super::*;
use greencode_core::voice::{self as core_voice, Options, Word};
use std::sync::atomic::{AtomicU64, Ordering};

pub(super) struct VoiceRecording {
    pub(super) stop: oneshot::Sender<bool>,
    pub(super) id: u64,
}

static NEXT: AtomicU64 = AtomicU64::new(0);

/// Dictation languages the settings row cycles through; `/voice language CODE` accepts any ISO code.
const LANGUAGES: [&str; 8] = ["auto", "ja", "en", "zh", "ko", "es", "fr", "de"];

pub(super) fn next_provider(current: Option<&str>, local: bool) -> &'static str {
    match current {
        Some("openai") => "groq",
        Some("groq") => "xai",
        Some("xai") => "openrouter",
        Some("openrouter") if local => "local",
        Some("openrouter") | Some("local") => "off",
        _ => "openai",
    }
}

pub(super) fn provider_label(provider: Option<&str>) -> &'static str {
    match provider {
        Some("openai") => "OpenAI",
        Some("groq") => "Groq",
        Some("xai") => "Grok Voice（xAI）",
        Some("openrouter") => "OpenRouter",
        Some("local") => "ローカル（whisper.cpp）",
        _ => "オフ",
    }
}

pub(super) fn language_label(ui: &Value) -> String {
    match ui["voice_language"].as_str() {
        None | Some("auto") => "自動検出".into(),
        Some(code) => code.into(),
    }
}

pub(super) fn mic_label(ui: &Value) -> String {
    ui["voice_mic_name"].as_str().filter(|_| ui["voice_mic"].is_string()).unwrap_or("システム既定").into()
}

pub(super) fn dict_label(ui: &Value) -> String {
    let personal = ui["voice_dict"].as_array().map_or(0, Vec::len);
    let project: usize = ui["voice_project_dict"].as_object().map(|m| m.values().map(|v| v.as_array().map_or(0, Vec::len)).sum()).unwrap_or(0);
    format!("個人 {personal} 語 / プロジェクト {project} 語")
}

/// `TERM = alias, alias` → a dictionary entry; `None` for an empty term.
fn parse_entry(text: &str) -> Option<Word> {
    let (term, aliases) = text.split_once('=').unwrap_or((text, ""));
    let term = term.trim();
    let aliases = aliases.split([',', '、']).map(str::trim).filter(|a| !a.is_empty()).map(String::from).collect();
    (!term.is_empty()).then(|| Word { term: term.into(), aliases })
}

/// Where the transcript goes: replaces the selection made when recording started, or lands after it when the
/// draft has changed since; a draft that no longer contains that text gets it appended.
/// Returns the new draft and the cursor offset after the inserted text.
fn insert(now: &str, snapshot: &str, range: std::ops::Range<usize>, text: &str) -> (String, usize) {
    let range = if now == snapshot && now.get(range.clone()).is_some() {
        range
    } else if snapshot.get(..range.end).is_some_and(|head| now.starts_with(head)) {
        range.end..range.end
    } else {
        now.len()..now.len()
    };
    let (before, after) = (&now[..range.start], &now[range.end..]);
    let lead = if before.is_empty() || before.ends_with(char::is_whitespace) || text.is_empty() { "" } else { " " };
    let trail = if after.is_empty() || after.starts_with(char::is_whitespace) || text.is_empty() { "" } else { " " };
    let cursor = before.len() + lead.len() + text.len();
    (format!("{before}{lead}{text}{trail}{after}"), cursor)
}

impl Workspace {
    /// Dictation settings of the active thread's project: personal and project dictionaries, language and post-processing.
    fn voice_options(&self) -> Options {
        let project = self.thread().project();
        let personal: Vec<Word> = serde_json::from_value(self.ui["voice_dict"].clone()).unwrap_or_default();
        let local: Vec<Word> = serde_json::from_value(self.ui["voice_project_dict"][project.to_string_lossy().as_ref()].clone()).unwrap_or_default();
        Options {
            language: self.ui["voice_language"].as_str().filter(|l| *l != "auto").map(String::from),
            translate: self.ui["voice_translate"] == true,
            words: core_voice::merge_words(&local, &personal),
            project: project.file_name().map(|n| n.to_string_lossy().into_owned()),
            remove_fillers: self.ui["voice_fillers"] == true,
            model: self.ui["voice_model"].as_str().map(String::from),
            cleanup_instructions: self.ui["voice_cleanup_instructions"].as_str().map(String::from),
            correction: self.ui["voice_correction"].as_str().map(String::from),
        }
    }

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
        if !matches!(provider.as_str(), "openai" | "groq" | "xai" | "openrouter" | "local") {
            self.run(Act::SettingsTab(VOICE_SETTINGS), window, cx);
            self.notice("音声入力を使うには、文字起こし先を選んでください");
            cx.notify();
            return;
        }
        if cfg!(test) {
            return;
        }
        let id = NEXT.fetch_add(1, Ordering::Relaxed);
        let thread_id = self.thread().meta.id.clone();
        let cfg = self.cfg.clone();
        let options = self.voice_options();
        let mic = self.ui["voice_mic"].as_str().map(String::from);
        let snapshot = self.input.read(cx).value().to_string();
        let range = self.input.read(cx).selected_range();
        let cleanup = (self.ui["voice_cleanup"] == true).then(|| {
            self.ui["voice_cleanup_model"].as_str().map(String::from).unwrap_or_else(|| self.thread().meta.model.clone())
        });
        let (before, after) = (
            snapshot.get(..range.start).unwrap_or_default().to_string(),
            snapshot.get(range.end..).unwrap_or_default().to_string(),
        );
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
                let mut recording = core_voice::start(&path, mic.as_deref()).await.map_err(|e| format!("{e:#}"))?;
                let stop = tokio::select! {
                    result = recording.wait() => return Err(result.err().map(|e| format!("{e:#}")).unwrap_or_else(|| "録音が予期せず終了しました".into())),
                    result = rx => result.unwrap_or(false),
                    // Five minutes at most: stop and transcribe what was said.
                    _ = tokio::time::sleep(std::time::Duration::from_secs(core_voice::MAX_SECONDS)) => true,
                };
                if stop {
                    let path = recording.stop().await.map_err(|e| format!("{e:#}"))?;
                    let text = if provider == "local" {
                        core_voice::transcribe_local(&path, &options).await
                    } else {
                        core_voice::transcribe(&cfg, &provider, &path, &options).await
                    }.map_err(|e| format!("{e:#}"))?;
                    // A failed cleanup keeps the raw transcript.
                    let text = match cleanup {
                        Some(model) if !text.is_empty() => core_voice::cleanup(&cfg, &model, &text, &options, &before, &after).await.unwrap_or(text),
                        _ => text,
                    };
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
                            let (draft, cursor) = insert(&draft, &snapshot, range, &text);
                            this.threads[ix].draft = draft.clone();
                            if ix == this.active {
                                this.input.update(cx, |s, cx| {
                                    s.set_value(draft, window, cx);
                                    s.set_selected_range(cursor..cursor, cx);
                                });
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

    /// Settings buttons: `language` cycles the dictation language, `mic` the preferred microphone, `test` measures it.
    pub(super) fn voice_action(&mut self, what: &'static str, cx: &mut Context<Self>) {
        match what {
            "language" => {
                let current = self.ui["voice_language"].as_str().unwrap_or("auto");
                let next = LANGUAGES.iter().position(|l| *l == current).map_or(LANGUAGES[0], |i| LANGUAGES[(i + 1) % LANGUAGES.len()]);
                self.ui["voice_language"] = json!(next);
                self.save_ui();
            }
            "mic" | "test" if cfg!(test) => {}
            "mic" => {
                let current = self.ui["voice_mic"].as_str().map(String::from);
                let task = self.rt.spawn(core_voice::devices());
                cx.spawn(async move |this, cx| {
                    let devices = task.await.map_err(|e| e.to_string()).and_then(|r| r.map_err(|e| format!("{e:#}")));
                    let _ = this.update(cx, |this, cx| {
                        match devices {
                            // System default first, then each connected input; an unplugged choice starts over.
                            Ok(devices) => {
                                let next = match current.and_then(|id| devices.iter().position(|d| d.id == id)) {
                                    None => devices.first(),
                                    Some(i) => devices.get(i + 1),
                                };
                                match next {
                                    Some(d) => {
                                        this.ui["voice_mic"] = json!(d.id);
                                        this.ui["voice_mic_name"] = json!(d.name);
                                    }
                                    None => {
                                        this.ui["voice_mic"] = Value::Null;
                                        this.ui["voice_mic_name"] = Value::Null;
                                    }
                                }
                                this.save_ui();
                            }
                            Err(e) => this.fail(format!("マイク一覧を取得できませんでした: {e}")),
                        }
                        cx.notify();
                    });
                })
                .detach();
            }
            "test" => {
                let mic = self.ui["voice_mic"].as_str().map(String::from);
                self.notice("マイクをテストしています（3 秒間録音。音声は保存・送信されません）");
                let task = self.rt.spawn(async move { core_voice::test_microphone(mic.as_deref()).await });
                cx.spawn(async move |this, cx| {
                    let result = task.await.map_err(|e| e.to_string()).and_then(|r| r.map_err(|e| format!("{e:#}")));
                    let _ = this.update(cx, |this, cx| {
                        match result {
                            Ok((_, peak)) if peak < -60.0 => this.notice(format!("マイクの音を検出できませんでした（ピーク {peak:.0} dB）。マイクの選択と入力音量を確認してください")),
                            Ok((rms, peak)) => this.notice(format!("マイクは動作しています（平均 {rms:.0} dB / ピーク {peak:.0} dB）")),
                            Err(e) => this.fail(format!("マイクのテストに失敗しました: {e}")),
                        }
                        cx.notify();
                    });
                })
                .detach();
            }
            _ => {}
        }
    }

    /// `/voice [language CODE | model ID | cleanup-model ID | dict [project] [add 用語 = 別名, 別名 | rm 用語] | mic | test]`.
    pub(super) fn voice_command(&mut self, arg: &str, cx: &mut Context<Self>) {
        let (sub, rest) = arg.split_once(char::is_whitespace).map_or((arg, ""), |(s, r)| (s, r.trim()));
        match sub {
            "language" if !rest.is_empty() => {
                self.ui["voice_language"] = json!(rest);
                self.save_ui();
                self.notice(format!("音声入力の言語: {}", language_label(&self.ui)));
            }
            key @ ("model" | "cleanup-model") if !rest.is_empty() => {
                self.ui[if key == "model" { "voice_model" } else { "voice_cleanup_model" }] = json!(rest);
                self.save_ui();
                self.notice(if key == "model" { format!("OpenRouter の文字起こしモデル: {rest}") } else { format!("文字起こしの整形に使うモデル: {rest}") });
            }
            key @ ("correction" | "instructions") => {
                let field = if key == "correction" { "voice_correction" } else { "voice_cleanup_instructions" };
                self.ui[field] = if rest.is_empty() { Value::Null } else { json!(rest) };
                self.save_ui();
                self.notice(match (key, rest.is_empty()) {
                    ("correction", true) => "訂正の合図を解除しました".into(),
                    ("correction", false) => format!("訂正の合図: {rest}"),
                    (_, true) => "整形の追加指示を解除しました".into(),
                    (_, false) => format!("文字起こし整形の追加指示: {rest}"),
                });
            }
            "mic" => self.voice_action("mic", cx),
            "test" => self.voice_action("test", cx),
            "dict" => {
                let (project, rest) = match rest.strip_prefix("project") {
                    Some(r) if r.is_empty() || r.starts_with(char::is_whitespace) => (true, r.trim()),
                    _ => (false, rest),
                };
                let key = self.thread().project().to_string_lossy().into_owned();
                let mut words: Vec<Word> = serde_json::from_value(if project { self.ui["voice_project_dict"][&key].clone() } else { self.ui["voice_dict"].clone() }).unwrap_or_default();
                let (op, text) = rest.split_once(char::is_whitespace).map_or((rest, ""), |(o, t)| (o, t.trim()));
                match (op, parse_entry(text)) {
                    ("add", Some(word)) => {
                        words.retain(|w| !w.term.eq_ignore_ascii_case(&word.term));
                        words.push(word);
                    }
                    ("rm", Some(word)) => words.retain(|w| !w.term.eq_ignore_ascii_case(&word.term)),
                    ("", _) => {
                        let list = words.iter().map(|w| if w.aliases.is_empty() { w.term.clone() } else { format!("{} = {}", w.term, w.aliases.join(", ")) }).collect::<Vec<_>>().join("\n");
                        self.notice(format!("{}辞書（最大 {} 語まで送信）:\n{}", if project { "プロジェクト" } else { "個人" }, 100, if list.is_empty() { "（空）".into() } else { list }));
                        return;
                    }
                    _ => return self.notice("使い方: /voice dict [project] add 用語 = 別名, 別名 ｜ rm 用語"),
                }
                self.ui[if project { "voice_project_dict" } else { "voice_dict" }] = if project {
                    let mut all = self.ui["voice_project_dict"].as_object().cloned().unwrap_or_default();
                    all.insert(key, json!(words));
                    Value::Object(all)
                } else {
                    json!(words)
                };
                self.save_ui();
                self.notice(format!("{}辞書を更新しました（{} 語）", if project { "プロジェクト" } else { "個人" }, words.len()));
            }
            _ => self.notice(format!(
                "音声入力: {} / 言語 {} / マイク {}\n/voice language CODE（auto で自動検出） · /voice dict [project] add 用語 = 別名, 別名 · /voice dict [project] rm 用語 · /voice model ID（OpenRouter） · /voice cleanup-model ID · /voice correction 語句（訂正の合図） · /voice instructions 指示（整形） · /voice mic · /voice test",
                provider_label(self.ui["voice_provider"].as_str()), language_label(&self.ui), mic_label(&self.ui)
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    // No glob import: it would bring in gpui-kit's `test` macro, which shadows `#[test]`.
    use super::{insert, next_provider, parse_entry, Word};

    #[test]
    fn providers_cycle_through_cloud_local_and_off() {
        assert_eq!(next_provider(None, false), "openai");
        assert_eq!(next_provider(Some("openai"), false), "groq");
        assert_eq!(next_provider(Some("groq"), false), "xai");
        assert_eq!(next_provider(Some("xai"), false), "openrouter");
        assert_eq!(next_provider(Some("openrouter"), false), "off");
        assert_eq!(next_provider(Some("openrouter"), true), "local");
        assert_eq!(next_provider(Some("local"), true), "off");
        assert_eq!(next_provider(Some("off"), true), "openai");
    }

    #[test]
    fn entries_split_term_and_aliases() {
        assert_eq!(parse_entry(" GreenCode = green code、グリーンコード, "), Some(Word { term: "GreenCode".into(), aliases: vec!["green code".into(), "グリーンコード".into()] }));
        assert_eq!(parse_entry("gpui"), Some(Word { term: "gpui".into(), aliases: vec![] }));
        assert_eq!(parse_entry(" = x"), None);
    }

    #[test]
    fn transcript_replaces_the_selection_or_follows_it_when_the_draft_changed() {
        // "fix the bug": "the" (4..7) is selected.
        assert_eq!(insert("fix the bug", "fix the bug", 4..7, "that"), ("fix that bug".into(), 8));
        // A cited message was appended meanwhile: the text goes after the selection, which stays.
        assert_eq!(insert("fix the bug\n> quote", "fix the bug", 4..7, "now"), ("fix the now bug\n> quote".into(), 11));
        // The draft was rewritten: append.
        assert_eq!(insert("other", "fix the bug", 4..7, "x"), ("other x".into(), 7));
        assert_eq!(insert("", "", 0..0, "hello"), ("hello".into(), 5));
    }
}
