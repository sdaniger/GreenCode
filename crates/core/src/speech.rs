//! Speaking the assistant's output aloud, the `tts` speech-file tool, and `speech.enhanced` rewriting.
//! On macOS `say` is the on-device synthesizer; `.mp3`-style files go through xAI Grok Voice when configured.
use crate::config::{Config, SpeechMode};
use anyhow::{bail, Context, Result};
use regex_lite::Regex;
use serde_json::Value;
use std::{
    collections::VecDeque,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{Arc, LazyLock, Mutex, mpsc},
    time::Duration,
};

/// `tts.localVoice` / `speech.voice` spellings (American/British, female/male) and their macOS voices.
const VOICES: &[(&str, &str)] = &[
    ("harper", "Samantha"),
    ("hadley", "Ava"),
    ("delia", "Allison"),
    ("everly", "Susan"),
    ("easton", "Alex"),
    ("preston", "Tom"),
    ("miles", "Nathan"),
    ("trevor", "Fred"),
    ("maeve", "Serena"),
    ("bryony", "Kate"),
    ("edwin", "Daniel"),
    ("hugh", "Oliver"),
];

/// Base words per minute at speed 1.
const BASE_WPM: f64 = 175.0;
/// Longer unpunctuated stretches are cut at a space so speech starts early.
const MAX_CHUNK: usize = 240;
/// Minimum characters before a sentence break is spoken on its own.
const MIN_CHUNK: usize = 24;
/// Spoken text never exceeds this; the tail is dropped after a full sentence.
const LIMIT: usize = 2_000;

/// A Phonon id mapped to an installed macOS voice; an already-native name (capitalized or spaced) passes
/// through, and anything else falls back to the system default instead of failing synthesis.
pub fn say_voice(requested: &str) -> Option<String> {
    let requested = requested.trim();
    if requested.is_empty() {
        return None;
    }
    if let Some((_, name)) = VOICES.iter().find(|(id, _)| id.eq_ignore_ascii_case(requested)) {
        return Some((*name).into());
    }
    requested.chars().any(|c| c.is_uppercase() || c == ' ').then(|| requested.to_string())
}

/// Speaking rate in words per minute: `speed` clamped to 0.8–1.3, non-finite = normal.
pub fn rate(speed: f64) -> f64 {
    if speed.is_finite() { speed.clamp(0.8, 1.3) } else { 1.0 }
}

/// `say` arguments for one utterance; `None` voice leaves `say` on the system default.
pub fn say_args(voice: Option<&str>, speed: f64, text: &str) -> Vec<String> {
    let mut args = Vec::new();
    if let Some(voice) = voice {
        args.push("-v".into());
        args.push(voice.into());
    }
    args.push("-r".into());
    args.push(format!("{:.0}", BASE_WPM * rate(speed)));
    args.push("--".into());
    args.push(text.into());
    args
}

/// Whether text of this kind is spoken now: `all` includes thinking, `yield` only the turn's final message.
pub fn mode_speaks(mode: SpeechMode, thinking: bool, final_only: bool) -> bool {
    match mode {
        SpeechMode::All => true,
        SpeechMode::Assistant => !thinking,
        SpeechMode::Yield => final_only,
    }
}

fn link_re() -> &'static Regex {
    static RE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"\[([^\]]*)\]\((?:[^()\s]|\([^()\s]*\))*\)").unwrap());
    &RE
}

fn url_re() -> &'static Regex {
    static RE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"https?://([^/\s)>]+)[^\s)]*").unwrap());
    &RE
}

/// Plain prose for speech: code blocks dropped, inline code kept, links and bare URLs reduced to a name or
/// host, markdown markers removed. Long results stop after a complete sentence near [`LIMIT`].
pub fn plain(markdown: &str) -> String {
    let mut out = String::with_capacity(markdown.len().min(LIMIT));
    let mut fence: Option<String> = None;
    for line in markdown.lines() {
        match &fence {
            Some(marker) => {
                if line.trim_start().starts_with(marker.as_str()) {
                    fence = None;
                }
                continue;
            }
            None => {
                if line.trim_start().starts_with("```") {
                    fence = Some("```".to_string());
                    continue;
                }
            }
        }
        let line = line.trim().trim_start_matches(['#', '>', '-', '*', '+', ' ']);
        if line.is_empty() || line.chars().all(|c| matches!(c, '-' | '|' | ':' | ' ')) {
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }
    let mut text = link_re().replace_all(&out, "$1").into_owned();
    text = url_re().replace_all(&text, "$1").into_owned();
    let text = text.replace(['*', '`', '|'], "").replace('_', " ");
    let text = text.replace('<', "").replace('>', "");
    let mut spoken = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if spoken.chars().count() > LIMIT {
        let cut = spoken.char_indices().take(LIMIT).map(|(i, _)| i).last().unwrap_or(0);
        spoken.truncate(cut);
        if let Some((at, _)) = spoken.char_indices().rev().find(|(_, c)| matches!(c, '.' | '!' | '?' | '。' | '！' | '？')) {
            spoken.truncate(at + spoken[at..].chars().next().map_or(1, char::len_utf8));
        }
    }
    spoken.trim().to_string()
}

/// Accumulates streamed output into sentences and hands each to the speaker.
pub struct Vocalizer {
    mode: SpeechMode,
    voice: String,
    speed: f64,
    /// `speech.enhanced`: text is buffered for one rewritten utterance at turn end, never spoken mid-stream.
    enhanced: bool,
    buf: String,
}

impl Vocalizer {
    /// `None` when speech vocalization is off.
    pub fn new(cfg: &Config) -> Option<Self> {
        let speech = &cfg.services_opts.speech;
        speech.enabled.then(|| Self {
            mode: speech.mode,
            voice: speech.voice.clone(),
            speed: speech.speed,
            enhanced: speech.enhanced,
            buf: String::new(),
        })
    }

    /// Feeds one streamed delta of a message (`thinking`) or assistant text; whole sentences are spoken.
    /// `yield` mode only buffers assistant text; [`Vocalizer::finish`] speaks it at turn end.
    pub fn feed(&mut self, thinking: bool, delta: &str) {
        // Enhanced text is rewritten once at turn end, so nothing is spoken mid-stream.
        if self.enhanced {
            if !thinking {
                self.buf.push_str(delta);
            }
            return;
        }
        if !mode_speaks(self.mode, thinking, false) {
            if self.mode == SpeechMode::Yield && !thinking {
                self.buf.push_str(delta);
            }
            return;
        }
        self.buf.push_str(delta);
        for chunk in take_sentences(&mut self.buf) {
            speak_voice(&self.voice, self.speed, &chunk);
        }
    }

    /// True when the caller must rewrite the buffer and speak it once ([`enhanced`]) instead of finishing.
    pub fn enhanced(&self) -> bool {
        self.enhanced
    }

    /// The buffered text, taken out of the queue.
    pub fn take(&mut self) -> String {
        std::mem::take(&mut self.buf)
    }

    /// Voice and rate to use when the caller speaks rewritten text.
    pub fn voice(&self) -> (&str, f64) {
        (&self.voice, self.speed)
    }

    /// Speaks whatever is left, e.g. a message that did not end in punctuation.
    pub fn finish(&mut self) {
        let rest = std::mem::take(&mut self.buf);
        let rest = plain(&rest);
        if !rest.is_empty() {
            speak_voice(&self.voice, self.speed, &rest);
        }
    }

    /// Speaks one finished message now (yield mode, or `speech.enhanced` output).
    pub fn say(&self, text: &str) {
        speak_voice(&self.voice, self.speed, &plain(text));
    }

    /// Ends a turn without speaking the tail (a cancel).
    pub fn clear(&mut self) {
        self.buf.clear();
    }
}

/// Whole sentences (and newline-terminated lines) at least [`MIN_CHUNK`] long, cut at a space when too long.
fn take_sentences(buf: &mut String) -> Vec<String> {
    let mut out = Vec::new();
    loop {
        let mut cut = None;
        for (at, c) in buf.char_indices() {
            if matches!(c, '.' | '!' | '?' | '。' | '！' | '？' | '\n') {
                let end = at + c.len_utf8();
                if buf[..end].chars().count() >= MIN_CHUNK {
                    cut = Some(end);
                    break;
                }
            }
        }
        if cut.is_none() && buf.chars().count() > MAX_CHUNK {
            let at = buf.char_indices().take(MAX_CHUNK).map(|(i, _)| i).last().unwrap_or(0);
            cut = buf[..at].rfind(' ').map(|space| space + 1);
        }
        let Some(cut) = cut else { break };
        let chunk = plain(buf[..cut].trim_end_matches(|c: char| matches!(c, '.' | '!' | '?' | '。' | '！' | '？')));
        // Whitespace between sentences belongs to neither; leaving it makes an idle buffer look pending.
        *buf = buf[cut..].trim_start().to_string();
        if !chunk.is_empty() {
            out.push(chunk);
        }
    }
    out
}

/// One queued utterance: the voice and rate in effect when it was queued.
struct Job {
    voice: Option<String>,
    speed: f64,
    text: String,
}

struct Speaker {
    tx: mpsc::Sender<Job>,
    current: Arc<Mutex<Option<Child>>>,
}

static SPEAKER: LazyLock<Speaker> = LazyLock::new(|| {
    let (tx, rx) = mpsc::channel::<Job>();
    let current: Arc<Mutex<Option<Child>>> = Arc::default();
    let running = current.clone();
    std::thread::spawn(move || {
        let mut queue: VecDeque<Job> = VecDeque::new();
        loop {
            while let Ok(job) = rx.try_recv() {
                queue.push_back(job);
            }
            let Some(job) = queue.pop_front() else {
                match rx.recv() {
                    Ok(job) => queue.push_back(job),
                    Err(_) => return,
                }
                continue;
            };
            let args = say_args(job.voice.as_deref(), job.speed, &job.text);
            let Ok(child) = Command::new("say").args(&args).stdout(Stdio::null()).stderr(Stdio::null()).spawn() else {
                continue;
            };
            *running.lock().unwrap() = Some(child);
            // Poll through the shared handle so `stop` can take and kill the child while it speaks.
            loop {
                let mut slot = running.lock().unwrap();
                match slot.as_mut() {
                    Some(child) => match child.try_wait() {
                        Ok(Some(_)) | Err(_) => break,
                        Ok(None) => {}
                    },
                    None => break,
                }
                drop(slot);
                std::thread::sleep(Duration::from_millis(50));
            }
            *running.lock().unwrap() = None;
        }
    });
    Speaker { tx, current }
});

/// Speaks `text` with `voice` (a Phonon id or macOS name) and `speed` after anything already queued.
pub fn speak_voice(voice: &str, speed: f64, text: &str) {
    let text = text.trim();
    if !text.is_empty() {
        let _ = SPEAKER.tx.send(Job { voice: say_voice(voice), speed, text: text.to_string() });
    }
}

/// Speaks `text` with the system default voice at normal speed.
pub fn speak(text: &str) {
    speak_voice("", 1.0, text);
}

/// Speaks `text` and waits for it to finish (CLI `say`).
pub fn speak_blocking(voice: &str, speed: f64, text: &str) -> Result<()> {
    let voice = say_voice(voice);
    let status = Command::new("say")
        .args(say_args(voice.as_deref(), speed, text))
        .status()
        .map_err(|e| anyhow::anyhow!("speech needs macOS `say`: {e}"))?;
    anyhow::ensure!(status.success(), "say exited {status}");
    Ok(())
}

/// Stops the utterance in progress. Utterances already queued still play.
pub fn stop() {
    if let Some(mut child) = SPEAKER.current.lock().unwrap().take() {
        let _ = child.kill();
        let _ = child.wait();
    }
}

/// `speech.enhanced`: rewrites output into spoken prose with the tiny/smol role, or [`plain`] when that fails.
pub async fn enhanced(cfg: &Config, text: &str) -> String {
    let Some(spec) = ["tiny", "smol"].iter().find_map(|role| cfg.model_roles.get(*role).cloned()) else {
        return plain(text);
    };
    const SYSTEM: &str = "Rewrite this assistant output as prose to be read aloud: describe code in words, drop links, paths and markdown, keep the speaker's language and meaning, add nothing. Reply with the spoken text only.";
    let source = plain(text);
    if source.is_empty() {
        return source;
    }
    let reply = if crate::tiny::local_spec(&spec).is_some() {
        crate::tiny::generate(cfg, &spec, SYSTEM, &source, 400).await
    } else {
        crate::textgen::generate(cfg, &spec, SYSTEM, &source, 400).await
    };
    reply.map(|r| plain(&r)).unwrap_or_else(|_| source)
}

/// Extension → `say --file-format`, for the on-device backend.
fn file_format(ext: &str) -> Option<&'static str> {
    match ext.to_ascii_lowercase().as_str() {
        "wav" => Some("WAVE"),
        "aif" | "aiff" => Some("AIFF"),
        "m4a" => Some("m4af"),
        "caf" => Some("caff"),
        _ => None,
    }
}

/// The xAI Grok Voice backend, when the output format needs it and the provider is configured.
fn cloud(cfg: &Config, ext: &str) -> Option<String> {
    if file_format(ext).is_some() {
        return None;
    }
    cfg.providers.get("xai").filter(|p| p.api_key.is_some() || !p.base_url.is_empty()).map(|p| p.base_url.clone())
}

/// The `tts` ext tool: speech text into a file, on-device or through xAI. Returns a one-line report.
pub async fn tts(cfg: &Config, args: &Value, cwd: &Path) -> Result<String> {
    let text = args["text"].as_str().map(str::trim).filter(|t| !t.is_empty())
        .ok_or_else(|| anyhow::anyhow!("tts needs `text`"))?;
    let voice_id = args["voice_id"].as_str().map(str::trim).filter(|v| !v.is_empty()).unwrap_or("eve");
    let requested = match args["output_path"].as_str().map(str::trim).filter(|p| !p.is_empty()) {
        Some(path) => {
            let path = PathBuf::from(path);
            if path.is_absolute() { path } else { cwd.join(path) }
        }
        None => cwd.join(format!("speech-{}.wav", std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs()))),
    };
    if let Some(parent) = requested.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("cannot create {}", parent.display()))?;
    }
    let ext = requested.extension().and_then(|e| e.to_str()).unwrap_or("wav");
    if let Some(base) = cloud(cfg, ext) {
        let bytes = cloud_tts(cfg, &base, voice_id, args["language"].as_str(), text).await?;
        std::fs::write(&requested, &bytes)?;
        return Ok(format!("wrote {} ({} bytes, xAI Grok Voice `{voice_id}`)", requested.display(), bytes.len()));
    }
    let Some(format) = file_format(ext) else {
        bail!(
            "`{}` needs the xAI TTS backend: configure [providers.xai] api_key (or XAI_API_KEY), or ask for a .wav",
            requested.display()
        );
    };
    let local = cfg.services_opts.tts.local_voice.as_str();
    let voice = say_voice(local);
    let mut cmd = tokio::process::Command::new(say_bin());
    if let Some(voice) = &voice {
        cmd.arg("-v").arg(voice);
    }
    let out = cmd
        .arg("-r").arg(format!("{:.0}", BASE_WPM * rate(cfg.services_opts.tts.local_speed)))
        .arg("-o").arg(&requested)
        .arg("--file-format").arg(format)
        .arg("--data-format").arg("LEI16@24000")
        .arg("--").arg(text)
        .stdin(Stdio::null())
        .output()
        .await
        .map_err(|e| anyhow::anyhow!("the on-device speech backend needs macOS `say`: {e}"))?;
    if !out.status.success() {
        bail!("say failed: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    let bytes = std::fs::metadata(&requested).map(|m| m.len()).unwrap_or(0);
    anyhow::ensure!(bytes > 0, "say produced no audio at {}", requested.display());
    Ok(format!(
        "wrote {} ({bytes} bytes, on-device voice {})",
        requested.display(),
        voice.as_deref().unwrap_or(local)
    ))
}

#[cfg(test)]
thread_local! {
    /// A stand-in `say` for this test thread; mutating the shared PATH would hand it to parallel tests.
    static SAY: std::cell::RefCell<Option<std::path::PathBuf>> = const { std::cell::RefCell::new(None) };
}

fn say_bin() -> std::ffi::OsString {
    #[cfg(test)]
    if let Some(path) = SAY.with(|say| say.borrow().clone()) {
        return path.into_os_string();
    }
    "say".into()
}

/// `POST {base}/tts` on the OpenAI-compatible xAI API; returns the audio bytes.
async fn cloud_tts(cfg: &Config, base: &str, voice_id: &str, language: Option<&str>, text: &str) -> Result<Vec<u8>> {
    let provider = cfg.providers.get("xai").ok_or_else(|| anyhow::anyhow!("[providers.xai] is not configured"))?;
    let key = crate::provider::key("xai", provider)?;
    anyhow::ensure!(!key.is_empty(), "the xAI speech backend needs an API key: set providers.xai.api_key or XAI_API_KEY");
    let url = format!("{}/tts", base.trim_end_matches('/'));
    let body = serde_json::json!({"text": text, "voice_id": voice_id, "language": language.unwrap_or("auto")});
    let response = reqwest::Client::new()
        .post(&url)
        .bearer_auth(key)
        .json(&body)
        .timeout(Duration::from_secs(120))
        .send()
        .await
        .with_context(|| format!("xAI TTS request to {url} failed"))?;
    let status = response.status();
    let bytes = response.bytes().await?;
    anyhow::ensure!(status.is_success(), "xAI TTS returned {status}: {}", String::from_utf8_lossy(&bytes));
    anyhow::ensure!(!bytes.is_empty(), "xAI TTS returned no audio");
    Ok(bytes.to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ProviderCfg;

    #[test]
    fn plain_prose_selects_the_spoken_parts() {
        let text = plain(
            "# Title\n\nUse `cargo test` now.\n```\nlet x = 1;\n```\n- next step\n\nSee [the docs](https://example.com/a?b=1) and https://crates.io/crates/serde for more.\n",
        );
        assert!(text.starts_with("Title Use cargo test now."), "{text}");
        assert!(text.contains("next step"), "{text}");
        assert!(text.contains("See the docs and crates.io for more."), "{text}");
        assert!(!text.contains("let x") && !text.contains('`') && !text.contains('#') && !text.contains("https://"));
        assert!(!text.contains('|'));
    }

    #[test]
    fn plain_cuts_long_output_after_a_sentence() {
        let sentence = "This is a sentence about the change. ";
        let text = plain(&sentence.repeat(200));
        assert!(text.chars().count() <= LIMIT, "{}", text.chars().count());
        assert!(text.ends_with('.'), "{}", &text[text.len().saturating_sub(40)..]);
    }

    #[test]
    fn mode_and_final_only_decisions() {
        use SpeechMode::*;
        assert!(mode_speaks(All, true, false) && mode_speaks(All, false, false));
        assert!(!mode_speaks(Assistant, true, false) && mode_speaks(Assistant, false, false));
        assert!(!mode_speaks(Yield, false, false) && mode_speaks(Yield, true, true));
    }

    #[test]
    fn rate_clamps_to_the_documented_band() {
        assert_eq!(rate(1.0), 1.0);
        assert_eq!(rate(0.1), 0.8);
        assert_eq!(rate(9.0), 1.3);
        assert_eq!(rate(f64::NAN), 1.0);
        assert_eq!(rate(f64::INFINITY), 1.0);
        let args = say_args(Some("Samantha"), 2.0, "hello");
        assert_eq!(args, ["-v", "Samantha", "-r", "228", "--", "hello"]);
        assert_eq!(say_args(None, 0.5, "hi")[..2], ["-r", "140"]);
    }

    #[test]
    fn voices_map_and_unknown_ids_use_the_default() {
        assert_eq!(say_voice("maeve").as_deref(), Some("Serena"));
        assert_eq!(say_voice("Hugh").as_deref(), Some("Oliver"));
        assert_eq!(say_voice("Samantha").as_deref(), Some("Samantha"));
        assert_eq!(say_voice("my voice"), Some("my voice".into()));
        assert_eq!(say_voice("nonsense"), None);
        assert_eq!(say_voice("  "), None);
    }

    #[test]
    fn sentences_and_long_lines_split_for_streaming() {
        let cfg = Config {
            services_opts: crate::config::services::ServicesOpts {
                speech: crate::config::services::SpeechCfg { enabled: true, ..Default::default() },
                ..Default::default()
            },
            ..Default::default()
        };
        let mut v = Vocalizer::new(&cfg).unwrap();
        v.feed(false, "This is the first sentence of the reply. It continues here");
        assert!(v.buf.ends_with("It continues here"), "the sentence was spoken and the tail kept");
        let mut buf = String::from("Short. Then a longer sentence that is definitely spoken now. tail");
        assert_eq!(take_sentences(&mut buf).len(), 1);
        assert!(buf.ends_with("tail"));
        let mut long = "word ".repeat(60);
        let chunks = take_sentences(&mut long);
        assert_eq!(chunks.len(), 1);
        assert!(chunks[0].chars().count() <= MAX_CHUNK + 8 && long.starts_with('w'));
    }

    #[test]
    fn thinking_is_only_spoken_in_all_mode() {
        let cfg = |mode| Config {
            services_opts: crate::config::services::ServicesOpts {
                speech: crate::config::services::SpeechCfg { enabled: true, mode, ..Default::default() },
                ..Default::default()
            },
            ..Default::default()
        };
        let mut v = Vocalizer::new(&cfg(SpeechMode::Assistant)).unwrap();
        v.feed(true, "A thought long enough to look like a sentence. ");
        assert!(v.buf.is_empty(), "assistant mode ignores thinking");
        let mut v = Vocalizer::new(&cfg(SpeechMode::All)).unwrap();
        v.feed(true, "A thought long enough to look like a sentence. ");
        assert!(v.buf.is_empty(), "the sentence was spoken, not left in the buffer");
        let mut v = Vocalizer::new(&cfg(SpeechMode::Yield)).unwrap();
        v.feed(false, "Streamed text is not spoken in yield mode. ");
        // Held, not spoken: yield mode reads the whole answer once when the turn ends.
        assert!(v.buf.contains("yield mode") && !mode_speaks(SpeechMode::Yield, false, false));
        let mut off = Config::default();
        assert!(Vocalizer::new(&off).is_none());
        off.services_opts.speech.enabled = true;
        assert!(Vocalizer::new(&off).is_some());
    }

    #[test]
    fn on_device_format_choice_and_backend_pick() {
        assert_eq!(file_format("wav"), Some("WAVE"));
        assert_eq!(file_format("AIFF"), Some("AIFF"));
        assert_eq!(file_format("m4a"), Some("m4af"));
        assert_eq!(file_format("mp3"), None);
        let mut cfg = Config::default();
        assert!(cloud(&cfg, "mp3").is_none(), "an unconfigured xAI provider never wins");
        assert!(cloud(&cfg, "wav").is_none(), "wav stays on-device even when xAI exists");
        cfg.providers.insert("xai".into(), ProviderCfg { kind: "openai".into(), base_url: "https://api.x.ai/v1".into(), ..Default::default() });
        assert_eq!(cloud(&cfg, "mp3").as_deref(), Some("https://api.x.ai/v1"));
    }

    /// A local server answering `POST {base}/tts` like the OpenAI-compatible xAI API; asserts the request body.
    #[tokio::test]
    async fn mp3_goes_through_the_configured_cloud_backend() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let seen = std::sync::Arc::new(tokio::sync::Mutex::new(String::new()));
        let recorded = seen.clone();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = Vec::new();
            let mut chunk = [0u8; 4096];
            loop {
                let n = socket.read(&mut chunk).await.unwrap();
                buf.extend_from_slice(&chunk[..n]);
                let text = String::from_utf8_lossy(&buf).to_string();
                if let Some((head, body)) = text.split_once("\r\n\r\n") {
                    let len: usize = head.lines().find_map(|l| l.strip_prefix("content-length: ").or_else(|| l.strip_prefix("Content-Length: "))).and_then(|v| v.trim().parse().ok()).unwrap_or(0);
                    if body.len() >= len {
                        recorded.lock().await.push_str(&format!("{head}\n\n{body}"));
                        break;
                    }
                }
                if n == 0 { break; }
            }
            socket.write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 5\r\ncontent-type: audio/mpeg\r\n\r\nID3xm").await.unwrap();
        });
        let dir = std::env::temp_dir().join(format!("greencode-tts-mp3-ok-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut cfg = Config::default();
        cfg.providers.insert(
            "xai".into(),
            ProviderCfg { kind: "openai".into(), base_url: format!("http://127.0.0.1:{port}/v1"), api_key: Some("k".into()), ..Default::default() },
        );
        let args = serde_json::json!({"text": "hello there", "output_path": "out.mp3", "voice_id": "eve"});
        let report = tts(&cfg, &args, &dir).await.unwrap();
        assert_eq!(std::fs::read(dir.join("out.mp3")).unwrap(), b"ID3xm");
        assert!(report.contains("xAI Grok Voice `eve`"), "{report}");
        let request = seen.lock().await.clone();
        assert!(request.starts_with("POST /v1/tts "), "{request}");
        assert!(request.contains("\"voice_id\":\"eve\"") && request.contains("hello there"), "{request}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A stand-in `say` writes the audio file and records its flags.
    #[tokio::test]
    async fn on_device_tts_writes_the_file_with_the_configured_voice_and_rate() {
        let dir = std::env::temp_dir().join(format!("greencode-tts-ok-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join("say");
        // usage: say [-v VOICE] -r RATE -o OUT --file-format F --data-format D -- TEXT
        std::fs::write(&script, format!("#!/bin/sh\necho \"$@\" > \"{}/args\"\nwhile [ \"$1\" != \"-o\" ]; do shift; done\nprintf RIFF > \"$2\"\n", dir.display())).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        SAY.with(|say| *say.borrow_mut() = Some(script.clone()));
        let mut cfg = Config::default();
        cfg.services_opts.tts.local_voice = "maeve".into();
        cfg.services_opts.tts.local_speed = 3.0;
        let args = serde_json::json!({"text": "hello there", "output_path": "out.wav"});
        let report = tts(&cfg, &args, &dir).await.unwrap();
        SAY.with(|say| *say.borrow_mut() = None);
        let flags = std::fs::read_to_string(dir.join("args")).unwrap();
        assert!(flags.contains("-v Serena"), "{flags}");
        assert!(flags.contains("-r 228"), "clamped to 1.3: {flags}");
        assert!(flags.contains("--file-format WAVE"), "{flags}");
        assert!(report.contains("4 bytes") && report.contains("Serena"), "{report}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_format_without_a_backend_names_both_choices() {
        let dir = std::env::temp_dir().join(format!("greencode-tts-mp3-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let cfg = Config::default();
        let args = serde_json::json!({"text": "hi", "output_path": "out.mp3"});
        let error = tts(&cfg, &args, &dir).await.unwrap_err().to_string();
        assert!(error.contains("xAI") && error.contains(".wav"), "{error}");
        assert!(!dir.join("out.mp3").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn enhanced_falls_back_to_mechanical_cleanup_without_a_model() {
        let cfg = Config::default();
        let text = "Use `cargo test` now.";
        assert_eq!(enhanced(&cfg, text).await, "Use cargo test now.");
    }
}
