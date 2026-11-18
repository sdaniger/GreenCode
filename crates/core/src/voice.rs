//! Microphone recording and opt-in OpenAI-compatible transcription.
use crate::{config::Config, provider};
use anyhow::{anyhow, bail, Context, Result};
use std::{hash::{Hash, Hasher}, path::{Path, PathBuf}, process::Stdio};
use tokio::{io::{AsyncReadExt, AsyncWriteExt}, process::{Child, Command}};

const SOURCE: &str = include_str!("voice.swift");

pub struct Recording { child: Child, path: PathBuf }

// shortcut: compiling on first use requires the Xcode command line tools; bundle a binary for distribution.
pub async fn start(out: &Path) -> Result<Recording> {
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
    let child = Command::new(bin).arg("record").arg(out).stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::piped()).kill_on_drop(true).spawn()?;
    Ok(Recording { child, path: out.to_owned() })
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

fn multipart(boundary: &str, audio: &[u8], model: &str, language: Option<&str>) -> Vec<u8> {
    let mut body = format!("--{boundary}\r\nContent-Disposition: form-data; name=\"model\"\r\n\r\n{model}\r\n").into_bytes();
    if let Some(language) = language {
        body.extend_from_slice(format!("--{boundary}\r\nContent-Disposition: form-data; name=\"language\"\r\n\r\n{language}\r\n").as_bytes());
    }
    body.extend_from_slice(format!("--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"recording.m4a\"\r\nContent-Type: audio/mp4\r\n\r\n").as_bytes());
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

/// Transcribes locally with `whisper-cli`; the recording is converted to 16 kHz mono WAV first.
pub async fn transcribe_local(file: &Path, language: Option<&str>) -> Result<String> {
    let whisper = crate::config::executable("whisper-cli").ok_or_else(|| anyhow!("whisper-cli is not installed (brew install whisper-cpp)"))?;
    let model = local_model().ok_or_else(|| anyhow!("no whisper model found; set GREENCODE_WHISPER_MODEL or download one into ~/.cache/whisper.cpp"))?;
    let wav = file.with_extension("16k.wav");
    let converted = tokio::process::Command::new("/usr/bin/afconvert")
        .args(["-f", "WAVE", "-d", "LEI16@16000", "-c", "1"]).arg(file).arg(&wav).output().await?;
    anyhow::ensure!(converted.status.success(), "afconvert failed: {}", String::from_utf8_lossy(&converted.stderr).trim());
    let out = tokio::process::Command::new(whisper).arg("-m").arg(&model).arg("-f").arg(&wav).args(["-nt", "-np", "-l", language.unwrap_or("auto")]).output().await;
    let _ = std::fs::remove_file(&wav);
    let out = out?;
    anyhow::ensure!(out.status.success(), "whisper-cli failed: {}", String::from_utf8_lossy(&out.stderr).trim());
    Ok(parse_whisper(&String::from_utf8_lossy(&out.stdout)))
}

pub async fn transcribe(cfg: &Config, provider_name: &str, file: &Path, language: Option<&str>) -> Result<String> {
    let model = match provider_name { "openai" => "gpt-4o-mini-transcribe", "groq" => "whisper-large-v3-turbo", _ => bail!("unsupported voice provider `{provider_name}`") };
    let p = cfg.providers.get(provider_name).ok_or_else(|| anyhow!("unknown provider `{provider_name}`"))?;
    if p.kind != "openai" { bail!("voice provider `{provider_name}` must be OpenAI-compatible"); }
    let key = provider::key(p)?;
    let audio = std::fs::read(file)?;
    // A boundary absent from the recording avoids accidentally ending the file part.
    let mut boundary = format!("greencode-{:x}", std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_nanos());
    while audio.windows(boundary.len()).any(|w| w == boundary.as_bytes()) { boundary.push('x'); }
    let body = multipart(&boundary, &audio, model, language);
    let response = reqwest::Client::new().post(format!("{}/audio/transcriptions", p.base_url.trim_end_matches('/')))
        .bearer_auth(key).header(reqwest::header::CONTENT_TYPE, format!("multipart/form-data; boundary={boundary}"))
        .body(body).send().await?.error_for_status()?;
    let value: serde_json::Value = response.json().await?;
    value["text"].as_str().map(String::from).ok_or_else(|| anyhow!("transcription response has no text"))
}

#[cfg(test)]
mod tests {
    use super::{multipart, transcribe};
    use crate::config::{Config, ProviderCfg};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[test]
    fn multipart_contains_audio_and_fields() {
        let body = multipart("boundary", b"audio\0bytes", "model-id", Some("ja"));
        assert!(body.windows(b"audio\0bytes".len()).any(|w| w == b"audio\0bytes"));
        let s = String::from_utf8(body).unwrap();
        for part in ["--boundary", "name=\"model\"", "model-id", "filename=\"recording.m4a\"", "name=\"language\"", "ja"] { assert!(s.contains(part)); }
    }

    #[tokio::test]
    async fn transcription_posts_to_provider() {
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
                    if data.len() >= end + 4 + length {
                        assert!(headers.contains("authorization: bearer secret"));
                        assert!(headers.starts_with("post /audio/transcriptions "));
                        break;
                    }
                }
            }
            let body = "{\"text\":\"こんにちは\"}";
            stream.write_all(format!("HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
        });
        let path = std::env::temp_dir().join(format!("greencode-voice-test-{}", std::process::id()));
        std::fs::write(&path, b"audio").unwrap();
        let mut cfg = Config::default();
        cfg.providers.insert("openai".into(), ProviderCfg { kind: "openai".into(), base_url: format!("http://{addr}"), api_key: Some("secret".into()), ..Default::default() });
        assert_eq!(transcribe(&cfg, "openai", &path, None).await.unwrap(), "こんにちは");
        server.await.unwrap();
        std::fs::remove_file(path).unwrap();
    }
    use super::*;

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
