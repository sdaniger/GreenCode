//! Microphone recording and opt-in transcription (OpenAI-compatible, Grok, OpenRouter, whisper.cpp).
use crate::{config::Config, provider, textgen};
use anyhow::{anyhow, bail, ensure, Context, Result};
use base64::Engine;
use regex_lite::{NoExpand, Regex};
use serde::{Deserialize, Serialize};
use std::{hash::{Hash, Hasher}, path::{Path, PathBuf}, process::Stdio, time::Duration};
use tokio::{io::{AsyncReadExt, AsyncWriteExt}, process::{Child, Command}};

const SOURCE: &str = include_str!("voice.swift");

/// Recordings stop and transcribe by themselves after this long.
pub const MAX_SECONDS: u64 = 300;
/// Hint entries sent per recording (project spellings first).
const MAX_WORDS: usize = 100;

pub struct Recording { child: Child, path: PathBuf }

/// A dictionary entry: `term` is the spelling to get right, `aliases` are misspellings the transcript is corrected from.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Word {
    pub term: String,
    #[serde(default)]
    pub aliases: Vec<String>,
}

#[derive(Clone, Debug, Default)]
pub struct Options {
    /// ISO code such as `ja`; `None` lets the model detect it.
    pub language: Option<String>,
    pub translate: bool,
    pub words: Vec<Word>,
    /// Sent as a hint only.
    pub project: Option<String>,
    pub remove_fillers: bool,
    /// Model of providers that have no default (OpenRouter).
    pub model: Option<String>,
    /// Replaces the built-in cleanup instructions when set.
    pub cleanup_instructions: Option<String>,
    /// Spoken cue that flags a self-correction, e.g. 「いや」.
    pub correction: Option<String>,
}

pub struct Device { pub id: String, pub name: String }

/// Project spellings win over personal ones that share a term; at most [`MAX_WORDS`] remain.
pub fn merge_words(project: &[Word], personal: &[Word]) -> Vec<Word> {
    let mut out: Vec<Word> = Vec::new();
    for w in project.iter().chain(personal) {
        let term = w.term.trim();
        if !term.is_empty() && !out.iter().any(|o| o.term.eq_ignore_ascii_case(term)) {
            out.push(Word { term: term.into(), aliases: w.aliases.clone() });
        }
    }
    out.truncate(MAX_WORDS);
    out
}

/// Free-text hint for the model: dictionary terms, the correction cue and the project name.
fn hint(o: &Options) -> Option<String> {
    let mut parts = Vec::new();
    if let Some(p) = o.project.as_deref().filter(|p| !p.is_empty()) { parts.push(format!("Project: {p}.")); }
    let terms: Vec<&str> = o.words.iter().map(|w| w.term.as_str())
        .chain(o.correction.as_deref().map(str::trim).filter(|c| !c.is_empty()))
        .collect();
    if !terms.is_empty() { parts.push(format!("Vocabulary: {}.", terms.join(", "))); }
    (!parts.is_empty()).then(|| parts.join(" "))
}

/// Replaces each alias (case-insensitively, on word edges for ASCII) with its dictionary term.
pub fn apply_aliases(text: &str, words: &[Word]) -> String {
    let mut out = text.to_string();
    for w in words {
        for alias in w.aliases.iter().map(|a| a.trim()).filter(|a| !a.is_empty()) {
            let edge = |c: Option<char>| if c.is_some_and(|c| c.is_ascii_alphanumeric()) { r"\b" } else { "" };
            let pattern = format!("(?i){}{}{}", edge(alias.chars().next()), regex_lite::escape(alias), edge(alias.chars().next_back()));
            if let Ok(re) = Regex::new(&pattern) { out = re.replace_all(&out, NoExpand(&w.term)).into_owned(); }
        }
    }
    out
}

/// Drops hesitation sounds. Bare Japanese あの is a real word, so it only goes with its comma.
pub fn remove_fillers(text: &str) -> String {
    // shortcut: English fillers need ASCII word edges and Japanese ones are matched anywhere; upgrade with a language-aware list if false positives appear.
    let re = Regex::new(r"(?i)\b(?:uh+m?|um+|hmm+|ehh|ehm|mmm+)\b[,.]?|(?:えーっと|えーと|えっと|えー+|あのー+|あのう|あの[、,]|そのー+|うーん|んー+)[、,]?").unwrap();
    let out = re.replace_all(text, "");
    let out = Regex::new(r"[ \t]{2,}").unwrap().replace_all(&out, " ");
    out.trim().to_string()
}

/// Alias corrections, then filler removal when asked.
fn finish(text: &str, o: &Options) -> String {
    let text = apply_aliases(text, &o.words);
    if o.remove_fillers { remove_fillers(&text) } else { text.trim().to_string() }
}

// shortcut: compiling on first use requires the Xcode command line tools; bundle a binary for distribution.
async fn helper() -> Result<PathBuf> {
    if !cfg!(target_os = "macos") { bail!("voice recording is only available on macOS"); }
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    SOURCE.hash(&mut hasher);
    let dir = crate::home().join("bin");
    let bin = dir.join(format!("voice-{:x}", hasher.finish()));
    if !bin.exists() {
        std::fs::create_dir_all(&dir)?;
        let src = dir.join("voice.swift");
        std::fs::write(&src, SOURCE)?;
        let tmp = bin.with_extension("tmp");
        let output = Command::new("swiftc").arg("-O").arg("-o").arg(&tmp).arg(&src).output().await
            .context("voice input needs `swiftc`; install it with `xcode-select --install`")?;
        if !output.status.success() { bail!("cannot build the voice helper: {}", String::from_utf8_lossy(&output.stderr)); }
        std::fs::rename(tmp, &bin)?;
    }
    Ok(bin)
}

/// Starts recording from `device` (a unique id from [`devices`]); an unplugged or unknown device falls back to the system default.
pub async fn start(out: &Path, device: Option<&str>) -> Result<Recording> {
    let mut cmd = Command::new(helper().await?);
    cmd.arg("record").arg(out);
    if let Some(device) = device { cmd.arg(device); }
    let child = cmd.stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::piped()).kill_on_drop(true).spawn()?;
    Ok(Recording { child, path: out.to_owned() })
}

/// Connected audio inputs.
pub async fn devices() -> Result<Vec<Device>> {
    let out = Command::new(helper().await?).arg("list").output().await?;
    ensure!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr).trim());
    Ok(String::from_utf8_lossy(&out.stdout).lines().filter_map(|l| l.split_once('\t')).map(|(id, name)| Device { id: id.into(), name: name.into() }).collect())
}

/// Records three seconds from `device` and returns `(rms dBFS, peak dBFS)`; nothing is transcribed or kept.
pub async fn test_microphone(device: Option<&str>) -> Result<(f32, f32)> {
    let dir = std::env::temp_dir().join(format!("greencode-mic-{}", std::process::id()));
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("test.m4a");
    let result = async {
        let recording = start(&path, device).await?;
        tokio::time::sleep(Duration::from_secs(3)).await;
        let path = recording.stop().await?;
        let out = Command::new(helper().await?).arg("level").arg(&path).output().await?;
        ensure!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr).trim());
        let text = String::from_utf8_lossy(&out.stdout);
        let mut n = text.split_whitespace().filter_map(|v| v.parse::<f32>().ok());
        Ok((n.next().ok_or_else(|| anyhow!("no level"))?, n.next().ok_or_else(|| anyhow!("no level"))?))
    }.await;
    let _ = std::fs::remove_dir_all(dir);
    result
}

impl Recording {
    pub async fn stop(mut self) -> Result<PathBuf> {
        let write = self.child.stdin.take().unwrap().write_all(b"\n").await;
        let output = self.child.wait_with_output().await?;
        if !output.status.success() { bail!("recording failed: {}", String::from_utf8_lossy(&output.stderr).trim()); }
        write?;
        Ok(self.path)
    }
    /// Reports a helper that exits before the user stops recording.
    pub async fn wait(&mut self) -> Result<()> {
        let status = self.child.wait().await?;
        let mut error = String::new();
        if let Some(stderr) = self.child.stderr.take() { stderr.take(8192).read_to_string(&mut error).await?; }
        if !status.success() { bail!("recording failed: {}", error.trim()); }
        bail!("recording stopped unexpectedly")
    }

}

/// Multipart form: the text `fields` in order, then the audio file part (xAI ignores fields after it).
fn multipart(boundary: &str, audio: &[u8], filename: &str, content_type: &str, fields: &[(&str, &str)]) -> Vec<u8> {
    let mut body = Vec::new();
    for (name, value) in fields {
        body.extend_from_slice(format!("--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n").as_bytes());
    }
    body.extend_from_slice(format!("--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"{filename}\"\r\nContent-Type: {content_type}\r\n\r\n").as_bytes());
    body.extend_from_slice(audio);
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    body
}

/// Whisper model for local transcription: `GREENCODE_WHISPER_MODEL`, else a cached whisper.cpp model.
pub fn local_model() -> Option<std::path::PathBuf> {
    if let Some(path) = std::env::var_os("GREENCODE_WHISPER_MODEL").map(std::path::PathBuf::from).filter(|p| p.is_file()) { return Some(path); }
    let dir = crate::user_home().join(".cache/whisper.cpp");
    ["ggml-large-v3-turbo-q5_0.bin", "ggml-large-v3-turbo.bin", "ggml-medium.bin", "ggml-small.bin", "ggml-base.en.bin"]
        .into_iter().map(|name| dir.join(name)).find(|p| p.is_file())
}

/// True when local transcription can run: `whisper-cli` installed and a model present.
pub fn local_available() -> bool {
    crate::config::executable("whisper-cli").is_some() && local_model().is_some()
}

/// Joins the plain lines whisper.cpp prints with `-nt -np`.
fn parse_whisper(stdout: &str) -> String {
    stdout.lines().map(str::trim).filter(|l| !l.is_empty()).collect::<Vec<_>>().join(" ")
}

/// whisper-cli arguments after the model and file: language, optional translation and the vocabulary prompt.
fn local_args(o: &Options) -> Vec<String> {
    let mut args = vec!["-nt".into(), "-np".into(), "-l".into(), o.language.clone().unwrap_or_else(|| "auto".into())];
    if o.translate { args.push("-tr".into()); }
    if let Some(hint) = hint(o) { args.extend(["--prompt".into(), hint]); }
    args
}

/// Converts the recording to 16 kHz mono WAV next to it.
async fn to_wav(file: &Path) -> Result<PathBuf> {
    let wav = file.with_extension("16k.wav");
    let converted = Command::new("/usr/bin/afconvert").args(["-f", "WAVE", "-d", "LEI16@16000", "-c", "1"]).arg(file).arg(&wav).output().await?;
    ensure!(converted.status.success(), "afconvert failed: {}", String::from_utf8_lossy(&converted.stderr).trim());
    Ok(wav)
}

/// Transcribes locally with `whisper-cli`.
pub async fn transcribe_local(file: &Path, o: &Options) -> Result<String> {
    let whisper = crate::config::executable("whisper-cli").ok_or_else(|| anyhow!("whisper-cli is not installed (brew install whisper-cpp)"))?;
    let model = local_model().ok_or_else(|| anyhow!("no whisper model found; set GREENCODE_WHISPER_MODEL or download one into ~/.cache/whisper.cpp"))?;
    let wav = to_wav(file).await?;
    let out = Command::new(whisper).arg("-m").arg(&model).arg("-f").arg(&wav).args(local_args(o)).output().await;
    let _ = std::fs::remove_file(&wav);
    let out = out?;
    ensure!(out.status.success(), "whisper-cli failed: {}", String::from_utf8_lossy(&out.stderr).trim());
    Ok(finish(&parse_whisper(&String::from_utf8_lossy(&out.stdout)), o))
}

/// Cloud transcription. `openai`/`groq` use `/audio/transcriptions` (`/audio/translations` to English),
/// `xai` (Grok Voice) uses `/stt`, `openrouter` takes base64 audio as JSON.
pub async fn transcribe(cfg: &Config, provider_name: &str, file: &Path, o: &Options) -> Result<String> {
    let p = cfg.providers.get(provider_name).ok_or_else(|| anyhow!("unknown provider `{provider_name}`"))?;
    if p.kind != "openai" { bail!("voice provider `{provider_name}` must be OpenAI-compatible"); }
    let key = provider::key(provider_name, p)?;
    let base = p.base_url.trim_end_matches('/');
    let language = o.language.as_deref();
    let hint = hint(o);
    let http = reqwest::Client::new();
    let request = match provider_name {
        "openai" | "groq" => {
            // Only whisper models translate.
            let (model, path) = match (provider_name, o.translate) {
                ("openai", false) => ("gpt-transcribe", "transcriptions"),
                ("openai", true) => ("whisper-1", "translations"),
                (_, false) => ("whisper-large-v3-turbo", "transcriptions"),
                (_, true) => ("whisper-large-v3", "translations"),
            };
            let mut fields = vec![("model", model)];
            // The translations endpoint has no language field. OpenAI's gpt-transcribe takes multiple languages.
            if let Some(language) = language.filter(|_| !o.translate) {
                fields.push((if provider_name == "openai" { "languages[]" } else { "language" }, language));
            }
            if let Some(hint) = &hint { fields.push(("prompt", hint)); }
            let (boundary, body) = form(&std::fs::read(file)?, "recording.m4a", "audio/mp4", &fields)?;
            http.post(format!("{base}/audio/{path}")).header(reqwest::header::CONTENT_TYPE, format!("multipart/form-data; boundary={boundary}")).body(body)
        }
        "xai" => {
            ensure!(!o.translate, "Grok Voice は英訳に対応していません");
            let wav = to_wav(file).await?;
            let audio = std::fs::read(&wav);
            let _ = std::fs::remove_file(&wav);
            let mut fields = vec![("model", "grok-voice-transcribe-2.0")];
            if let Some(language) = language { fields.extend([("language", language), ("format", "true")]); }
            fields.extend(o.words.iter().map(|w| ("keyterm", w.term.as_str())));
            // The correction cue is a filler-like word the model must hear.
            let keep_fillers = !o.remove_fillers || o.correction.as_deref().is_some_and(|c| !c.trim().is_empty());
            fields.push(("filler_words", if keep_fillers { "true" } else { "false" }));
            let (boundary, body) = form(&audio?, "dictation.wav", "audio/wav", &fields)?;
            http.post(format!("{base}/stt")).header(reqwest::header::CONTENT_TYPE, format!("multipart/form-data; boundary={boundary}")).body(body)
        }
        "openrouter" => {
            ensure!(!o.translate, "OpenRouter は英訳に対応していません");
            let model = o.model.as_deref().ok_or_else(|| anyhow!("OpenRouter のモデルを `/voice model ID` で指定してください"))?;
            let wav = to_wav(file).await?;
            let audio = std::fs::read(&wav);
            let _ = std::fs::remove_file(&wav);
            let mut body = serde_json::json!({"model": model, "input_audio": {"data": base64::engine::general_purpose::STANDARD.encode(audio?), "format": "wav"}, "provider": {"zdr": true}});
            if let Some(language) = language { body["language"] = language.into(); }
            http.post(format!("{base}/audio/transcriptions")).json(&body)
        }
        _ => bail!("unsupported voice provider `{provider_name}`"),
    };
    let response = request.bearer_auth(key).send().await?.error_for_status()?;
    let value: serde_json::Value = response.json().await?;
    let text = value["text"].as_str().ok_or_else(|| anyhow!("transcription response has no text"))?;
    Ok(finish(text, o))
}

fn form(audio: &[u8], filename: &str, content_type: &str, fields: &[(&str, &str)]) -> Result<(String, Vec<u8>)> {
    // A boundary absent from the recording avoids accidentally ending the file part.
    let mut boundary = format!("greencode-{:x}", std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_nanos());
    while audio.windows(boundary.len()).any(|w| w == boundary.as_bytes()) { boundary.push('x'); }
    let body = multipart(&boundary, audio, filename, content_type, fields);
    Ok((boundary, body))
}

const CLEANUP_SYSTEM: &str = "You clean up a speech-to-text transcript that will be inserted into a text box. Fix punctuation and numbers, and apply spoken self-corrections such as \"make it 42, sorry, 24\". Keep the speaker's language, wording and meaning; do not translate, answer or add anything. Reply with the cleaned transcript only.";

/// Forward context is trimmed where the transcript will land: its last and next 1,024 characters.
fn cleanup_prompt(text: &str, o: &Options, before: &str, after: &str) -> String {
    let tail = |s: &str| s.chars().rev().take(1024).collect::<Vec<_>>().into_iter().rev().collect::<String>();
    let mut prompt = String::new();
    if let Some(cue) = o.correction.as_deref().map(str::trim).filter(|c| !c.is_empty()) {
        prompt += &format!("The speaker says \"{cue}\" to flag a correction; apply it where the context shows one and drop the cue.\n");
    }
    if !o.words.is_empty() { prompt += &format!("Dictionary: {}\n", o.words.iter().map(|w| w.term.as_str()).collect::<Vec<_>>().join(", ")); }
    if !before.is_empty() { prompt += &format!("Text before the insertion point: {}\n", tail(before)); }
    if !after.is_empty() { prompt += &format!("Text after the insertion point: {}\n", after.chars().take(1024).collect::<String>()); }
    prompt += &format!("Transcript:\n{text}");
    prompt
}

/// Cleans `text` with `model` (a `provider/model` spec) through [`textgen::generate`]; no session is touched.
/// Callers keep the raw text when this fails.
pub async fn cleanup(cfg: &Config, model: &str, text: &str, o: &Options, before: &str, after: &str) -> Result<String> {
    let system = match o.cleanup_instructions.as_deref().map(str::trim).filter(|i| !i.is_empty()) {
        Some(instructions) => format!("{CLEANUP_SYSTEM}\nAdditional instructions: {instructions}"),
        None => CLEANUP_SYSTEM.to_string(),
    };
    textgen::generate(cfg, model, &system, &cleanup_prompt(text, o, before, after), 2048).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, ProviderCfg};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn word(term: &str, aliases: &[&str]) -> Word { Word { term: term.into(), aliases: aliases.iter().map(|a| a.to_string()).collect() } }

    #[test]
    fn multipart_contains_audio_and_fields_before_the_file() {
        let body = multipart("boundary", b"audio\0bytes", "recording.m4a", "audio/mp4", &[("model", "model-id"), ("language", "ja")]);
        assert!(body.windows(b"audio\0bytes".len()).any(|w| w == b"audio\0bytes"));
        let s = String::from_utf8(body).unwrap();
        for part in ["--boundary", "name=\"model\"", "model-id", "filename=\"recording.m4a\"", "name=\"language\"", "ja"] { assert!(s.contains(part)); }
        assert!(s.find("name=\"language\"").unwrap() < s.find("name=\"file\"").unwrap());
    }

    #[test]
    fn aliases_are_replaced_case_insensitively_on_word_edges() {
        let words = [word("GreenCode", &["green code", "グリーンコード"]), word("gpui", &["gee pee you eye"])];
        assert_eq!(apply_aliases("Open Green Code and グリーンコードを見て、gee pee you eye", &words), "Open GreenCode and GreenCodeを見て、gpui");
        assert_eq!(apply_aliases("evergreen codec", &[word("X", &["green code"])]), "evergreen codec");
    }

    #[test]
    fn fillers_are_removed_but_real_words_stay() {
        assert_eq!(remove_fillers("Um, I think uh the thing, hmm works"), "I think the thing, works");
        assert_eq!(remove_fillers("えーと、これをあの、直して。あの人は来た"), "これを直して。あの人は来た");
        assert_eq!(remove_fillers("umbrella summit"), "umbrella summit");
    }

    #[test]
    fn project_words_win_and_the_list_is_capped() {
        let merged = merge_words(&[word("Foo", &["fu"])], &[word("foo", &["fuu"]), word("Bar", &[])]);
        assert_eq!(merged, vec![word("Foo", &["fu"]), word("Bar", &[])]);
        let many: Vec<Word> = (0..150).map(|i| word(&format!("w{i}"), &[])).collect();
        assert_eq!(merge_words(&[], &many).len(), MAX_WORDS);
    }

    #[test]
    fn local_args_carry_language_translation_and_prompt() {
        let o = Options { language: Some("ja".into()), translate: true, words: vec![word("GreenCode", &[])], project: Some("app".into()), ..Default::default() };
        assert_eq!(local_args(&o), ["-nt", "-np", "-l", "ja", "-tr", "--prompt", "Project: app. Vocabulary: GreenCode."]);
        assert_eq!(local_args(&Options::default()), ["-nt", "-np", "-l", "auto"]);
    }

    #[test]
    fn correction_cue_joins_the_hint() {
        let o = Options { words: vec![word("GreenCode", &[])], correction: Some(" いや ".into()), ..Default::default() };
        assert_eq!(hint(&o).unwrap(), "Vocabulary: GreenCode, いや.");
        assert_eq!(hint(&Options { correction: Some(" ".into()), ..Default::default() }), None);
    }

    #[test]
    fn cleanup_prompt_carries_the_dictionary_cue_and_context_edges() {
        let o = Options { correction: Some("いや".into()), words: vec![word("GreenCode", &[])], ..Default::default() };
        let prompt = cleanup_prompt("green code を直して", &o, &"Ｚ".repeat(1100), &"Ｙ".repeat(1100));
        assert!(prompt.contains("The speaker says \"いや\" to flag a correction"));
        assert!(prompt.contains("Dictionary: GreenCode\n"));
        assert!(prompt.contains("Transcript:\ngreen code を直して"));
        assert_eq!(prompt.matches('Ｚ').count(), 1024);
        assert_eq!(prompt.matches('Ｙ').count(), 1024);
    }

    /// Serves one canned JSON reply and returns the base URL plus the request it received (headers + body).
    async fn serve(reply: &'static str) -> (String, tokio::task::JoinHandle<String>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut data = Vec::new();
            loop {
                let mut buf = [0; 4096];
                let n = stream.read(&mut buf).await.unwrap();
                assert!(n > 0);
                data.extend_from_slice(&buf[..n]);
                if let Some(end) = data.windows(4).position(|w| w == b"\r\n\r\n") {
                    let headers = String::from_utf8_lossy(&data[..end]).to_lowercase();
                    let length: usize = headers.lines().find_map(|line| line.strip_prefix("content-length: ")).unwrap().parse().unwrap();
                    if data.len() >= end + 4 + length { break; }
                }
            }
            stream.write_all(format!("HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{reply}", reply.len()).as_bytes()).await.unwrap();
            String::from_utf8_lossy(&data).into_owned()
        });
        (format!("http://{addr}"), server)
    }

    fn config(name: &str, base: String) -> Config {
        let mut cfg = Config::default();
        cfg.providers.insert(name.into(), ProviderCfg { kind: "openai".into(), base_url: base, api_key: Some("secret".into()), ..Default::default() });
        cfg
    }

    fn recording(tag: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!("greencode-voice-test-{tag}-{}", std::process::id()));
        std::fs::write(&path, b"audio").unwrap();
        path
    }

    #[tokio::test]
    async fn transcription_posts_to_provider() {
        let (base, server) = serve("{\"text\":\"こんにちは\"}").await;
        let path = recording("plain");
        assert_eq!(transcribe(&config("openai", base), "openai", &path, &Options::default()).await.unwrap(), "こんにちは");
        let request = server.await.unwrap();
        assert!(request.to_lowercase().contains("authorization: bearer secret"));
        assert!(request.starts_with("POST /audio/transcriptions "));
        assert!(!request.contains("name=\"prompt\"") && !request.contains("name=\"language\""));
        std::fs::remove_file(path).unwrap();
    }

    #[tokio::test]
    async fn transcription_sends_language_and_prompt_then_post_processes() {
        let (base, server) = serve("{\"text\":\"um, green code です\"}").await;
        let path = recording("opts");
        let o = Options { language: Some("ja".into()), words: vec![word("GreenCode", &["green code"])], project: Some("app".into()), remove_fillers: true, ..Default::default() };
        assert_eq!(transcribe(&config("groq", base), "groq", &path, &o).await.unwrap(), "GreenCode です");
        let request = server.await.unwrap();
        assert!(request.contains("name=\"language\"\r\n\r\nja"));
        assert!(request.contains("name=\"prompt\"\r\n\r\nProject: app. Vocabulary: GreenCode."));
        assert!(request.contains("whisper-large-v3-turbo"));
        std::fs::remove_file(path).unwrap();
    }

    #[tokio::test]
    async fn openai_receives_the_language_as_an_array() {
        let (base, server) = serve("{\"text\":\"hello\"}").await;
        let path = recording("lang");
        let o = Options { language: Some("ja".into()), ..Default::default() };
        assert_eq!(transcribe(&config("openai", base), "openai", &path, &o).await.unwrap(), "hello");
        let request = server.await.unwrap();
        assert!(request.contains("name=\"languages[]\"\r\n\r\nja"));
        assert!(request.contains("gpt-transcribe"));
        std::fs::remove_file(path).unwrap();
    }

    #[tokio::test]
    async fn translation_uses_the_translations_endpoint_without_language() {
        let (base, server) = serve("{\"text\":\"hello\"}").await;
        let path = recording("tr");
        let o = Options { language: Some("ja".into()), translate: true, ..Default::default() };
        assert_eq!(transcribe(&config("openai", base), "openai", &path, &o).await.unwrap(), "hello");
        let request = server.await.unwrap();
        assert!(request.starts_with("POST /audio/translations ") && request.contains("whisper-1") && !request.contains("name=\"language\""));
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn whisper_output_is_joined_into_one_line() {
        assert_eq!(parse_whisper("  hello\n\nworld \n"), "hello world");
        assert_eq!(parse_whisper(""), "");
    }

    #[test]
    fn local_model_prefers_the_environment_override() {
        let dir = std::env::temp_dir().join(format!("greencode-whisper-{}-{}", std::process::id(), std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
        std::fs::create_dir_all(&dir).unwrap();
        let model = dir.join("ggml-test.bin");
        std::fs::write(&model, b"x").unwrap();
        unsafe { std::env::set_var("GREENCODE_WHISPER_MODEL", &model) };
        assert_eq!(local_model(), Some(model.clone()));
        unsafe { std::env::remove_var("GREENCODE_WHISPER_MODEL") };
        assert_ne!(local_model(), Some(model));
        let _ = std::fs::remove_dir_all(dir);
    }
}
