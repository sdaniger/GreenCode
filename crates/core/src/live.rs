//! Realtime voice sessions (`live.*`): speaks to an OpenAI Realtime-compatible WebSocket, either the Codex
//! backend or a local/custom endpoint, and returns the assistant transcript.
use crate::config::Config;
use crate::provider::ws::Ws;
use anyhow::{bail, Context, Result};
use base64::Engine;
use serde_json::{json, Value};
use std::path::Path;
use std::time::Duration;

/// The Codex realtime socket used when `live.endpoint` is empty.
pub const CODEX_REALTIME: &str = "wss://chatgpt.com/backend-api/codex/realtime?intent=conversation";
/// How long one session setup or `response.done` may take.
const REPLY_TIMEOUT: Duration = Duration::from_secs(120);

/// `live.endpoint` as a Realtime WebSocket URL; empty goes to the Codex backend. A plain `http(s)` base is
/// converted and gets `/realtime?intent=conversation&model=...`.
pub fn realtime_url(cfg: &Config) -> Result<String> {
    let base = cfg.services_opts.live.endpoint.trim();
    if base.is_empty() {
        return Ok(CODEX_REALTIME.into());
    }
    let ws = if let Some(rest) = base.strip_prefix("https://") {
        format!("wss://{rest}")
    } else if let Some(rest) = base.strip_prefix("http://") {
        format!("ws://{rest}")
    } else if base.starts_with("ws://") || base.starts_with("wss://") {
        base.to_string()
    } else {
        bail!("live.endpoint must be an http(s) or ws(s) URL, got `{base}`")
    };
    let ws = ws.trim_end_matches('/').to_string();
    if ws.contains("/realtime") {
        return Ok(ws);
    }
    let sep = if ws.contains('?') { '&' } else { '?' };
    Ok(format!("{ws}/realtime{sep}intent=conversation&model=gpt-realtime"))
}

/// The Bearer for the socket: `live.api_key` (literal, env var name, or `!command`) for a custom endpoint, the
/// Codex login token for the Codex backend, and nothing when neither is set.
pub async fn api_key(cfg: &Config) -> Result<String> {
    let configured = key_from(&cfg.services_opts.live.api_key)?;
    if !configured.is_empty() {
        return Ok(configured);
    }
    if cfg.services_opts.live.endpoint.trim().is_empty() {
        return crate::provider::codex::access_token(&crate::provider::codex::path()).await;
    }
    Ok(String::new())
}

fn key_from(raw: &str) -> Result<String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Ok(String::new());
    }
    if let Some(command) = raw.strip_prefix('!') {
        let out = std::process::Command::new("sh")
            .arg("-c")
            .arg(command)
            .stderr(std::process::Stdio::null())
            .output()
            .with_context(|| format!("cannot run `{command}` for live.api_key"))?;
        let key = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if !out.status.success() || key.is_empty() {
            bail!("`{command}` for live.api_key exited {} without a key", out.status);
        }
        return Ok(key);
    }
    if raw.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_') {
        if let Ok(value) = std::env::var(raw) {
            if !value.trim().is_empty() {
                return Ok(value.trim().to_string());
            }
        }
    }
    Ok(raw.to_string())
}

/// What one exchange produced.
#[derive(Debug, Default, PartialEq)]
pub struct Reply {
    pub transcript: String,
    pub audio_bytes: usize,
}

/// One exchange on a fresh socket.
pub async fn ask(cfg: &Config, text: &str, audio: Option<&mut Vec<u8>>) -> Result<Reply> {
    let mut session = LiveSession::connect(cfg).await?;
    let reply = session.ask(text, audio).await;
    session.close().await;
    reply
}

/// A connected session, kept so a CLI or TUI can reuse one socket for several turns.
pub struct LiveSession {
    ws: Ws,
    model: String,
}

impl LiveSession {
    /// Connects and sends `session.update` carrying `live.voice`.
    pub async fn connect(cfg: &Config) -> Result<Self> {
        let url = realtime_url(cfg)?;
        let key = api_key(cfg).await?;
        let headers: Vec<(&str, String)> = if key.is_empty() { Vec::new() } else { vec![("authorization", format!("Bearer {key}"))] };
        let mut ws = Ws::connect(&url, &headers).await.with_context(|| format!("cannot open the realtime voice session at {url}"))?;
        let model = url.split("model=").nth(1).and_then(|m| m.split('&').next()).unwrap_or("gpt-realtime").to_string();
        let voice = cfg.services_opts.live.voice.trim();
        let update = json!({
            "type": "session.update",
            "session": {
                "type": "realtime",
                "model": model,
                "output_modalities": ["text"],
                "audio": {"output": {"voice": if voice.is_empty() { "sol" } else { voice }}},
            }
        });
        ws.send_json(&update).await?;
        if let Some(acknowledged) = ws.next_text(Some(REPLY_TIMEOUT)).await? {
            let v: Value = serde_json::from_str(&acknowledged).unwrap_or(Value::Null);
            if v["type"] == "error" {
                bail!("the live endpoint rejected the session: {}", v["error"]["message"].as_str().unwrap_or(&acknowledged));
            }
        }
        Ok(Self { ws, model })
    }

    /// Sends one user message and collects the reply until `response.done`; `audio` receives raw PCM deltas.
    pub async fn ask(&mut self, text: &str, mut audio: Option<&mut Vec<u8>>) -> Result<Reply> {
        self.ws
            .send_json(&json!({"type": "conversation.item.create", "item": {"type": "message", "role": "user", "content": [{"type": "input_text", "text": text}]}}))
            .await?;
        self.ws.send_json(&json!({"type": "response.create", "response": {"modalities": ["text"]}})).await?;
        let mut reply = Reply::default();
        loop {
            let frame = self.ws.next_text(Some(REPLY_TIMEOUT)).await?.ok_or_else(|| anyhow::anyhow!("the live endpoint closed the session"))?;
            let v: Value = serde_json::from_str(&frame).unwrap_or(Value::Null);
            match v["type"].as_str().unwrap_or("") {
                "response.output_text.delta" | "response.output_audio_transcript.delta" => {
                    reply.transcript.push_str(v["delta"].as_str().unwrap_or(""));
                }
                "response.output_text.done" | "response.output_audio_transcript.done" if reply.transcript.is_empty() => {
                    reply.transcript = v["text"].as_str().unwrap_or("").to_string();
                }
                "response.output_audio.delta" => {
                    if let (Some(target), Some(b64)) = (audio.as_deref_mut(), v["delta"].as_str()) {
                        if let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(b64) {
                            reply.audio_bytes += bytes.len();
                            target.extend_from_slice(&bytes);
                        }
                    }
                }
                "error" => bail!("the live endpoint returned an error: {}", v["error"]["message"].as_str().unwrap_or(&frame)),
                "response.done" => {
                    if v["response"]["status"].as_str() == Some("failed") {
                        bail!("the live response failed: {}", v["response"]["status_details"]);
                    }
                    reply.transcript = reply.transcript.trim().to_string();
                    return Ok(reply);
                }
                _ => {}
            }
        }
    }

    /// Closes politely; a dead socket is already closed.
    pub async fn close(mut self) {
        let _ = self.ws.send_json(&json!({"type": "session.close"})).await;
    }

    pub fn model(&self) -> &str {
        &self.model
    }
}

/// Writes raw 24 kHz mono 16-bit PCM as a WAV file (`greencode-cli live --out`).
pub fn write_wav(path: &Path, pcm: &[u8]) -> Result<()> {
    let data_len = pcm.len() as u32;
    let mut wav = Vec::with_capacity(44 + pcm.len());
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(36 + data_len).to_le_bytes());
    wav.extend_from_slice(b"WAVEfmt ");
    wav.extend_from_slice(&16u32.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes());
    wav.extend_from_slice(&24_000u32.to_le_bytes());
    wav.extend_from_slice(&48_000u32.to_le_bytes());
    wav.extend_from_slice(&2u16.to_le_bytes());
    wav.extend_from_slice(&16u16.to_le_bytes());
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&data_len.to_le_bytes());
    wav.extend_from_slice(pcm);
    std::fs::write(path, wav).with_context(|| format!("cannot write {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::services::LiveCfg;
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::{accept_async, tungstenite::Message};

    fn cfg(endpoint: &str, key: &str) -> Config {
        let mut cfg = Config::default();
        cfg.services_opts.live = LiveCfg { endpoint: endpoint.into(), voice: "eve".into(), api_key: key.into() };
        cfg
    }

    #[test]
    fn endpoints_convert_and_keep_an_explicit_realtime_path() {
        let mut c = Config::default();
        assert_eq!(realtime_url(&c).unwrap(), CODEX_REALTIME);
        c.services_opts.live.endpoint = "http://127.0.0.1:8080/v1".into();
        assert_eq!(realtime_url(&c).unwrap(), "ws://127.0.0.1:8080/v1/realtime?intent=conversation&model=gpt-realtime");
        c.services_opts.live.endpoint = "https://x.test/v1/realtime".into();
        assert_eq!(realtime_url(&c).unwrap(), "wss://x.test/v1/realtime");
        c.services_opts.live.endpoint = "ftp://x".into();
        assert!(realtime_url(&c).is_err());
    }

    #[test]
    fn api_keys_resolve_literals_env_names_and_commands() {
        assert_eq!(key_from("").unwrap(), "");
        assert_eq!(key_from("sk-literal").unwrap(), "sk-literal");
        std::env::set_var("GREENCODE_LIVE_TEST_KEY", "sk-env");
        assert_eq!(key_from("GREENCODE_LIVE_TEST_KEY").unwrap(), "sk-env");
        assert_eq!(key_from("GREENCODE_LIVE_MISSING_VAR").unwrap(), "GREENCODE_LIVE_MISSING_VAR", "an unset name is a literal");
        assert_eq!(key_from("!printf sk-cmd").unwrap(), "sk-cmd");
        assert!(key_from("!false").is_err());
    }

    #[test]
    fn wav_layout_is_a_valid_header() {
        let path = std::env::temp_dir().join(format!("greencode-live-{}.wav", std::process::id()));
        write_wav(&path, &[0, 1, 2, 3]).unwrap();
        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(&bytes[..4], b"RIFF");
        assert_eq!(&bytes[8..12], b"WAVE");
        assert_eq!(bytes.len(), 48);
        assert_eq!(&bytes[44..], &[0, 1, 2, 3]);
        let _ = std::fs::remove_file(path);
    }

    /// A real WebSocket handshake and one exchange against a local Realtime-shaped server.
    #[tokio::test]
    async fn handshake_sends_session_update_and_reads_the_reply() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let seen = std::sync::Arc::new(tokio::sync::Mutex::new(Vec::<Value>::new()));
        let recorded = seen.clone();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = accept_async(stream).await.unwrap();
            while let Some(Ok(Message::Text(text))) = ws.next().await {
                let v: Value = serde_json::from_str(&text).unwrap();
                recorded.lock().await.push(v.clone());
                match v["type"].as_str() {
                    Some("session.update") => {
                        ws.send(Message::text(json!({"type": "session.updated"}).to_string())).await.unwrap();
                    }
                    Some("response.create") => {
                        for (kind, delta) in [
                            ("response.output_audio_transcript.delta", "Hello "),
                            ("response.output_audio_transcript.delta", "there"),
                            ("response.output_audio.delta", "AAEC"),
                            ("response.done", ""),
                        ] {
                            let frame = if kind == "response.done" {
                                json!({"type": kind, "response": {"status": "completed"}})
                            } else {
                                json!({"type": kind, "delta": delta})
                            };
                            ws.send(Message::text(frame.to_string())).await.unwrap();
                        }
                    }
                    _ => {}
                }
            }
        });
        let cfg = cfg(&format!("http://127.0.0.1:{port}/v1"), "");
        let mut session = LiveSession::connect(&cfg).await.unwrap();
        let mut audio = Vec::new();
        let reply = session.ask("say hi", Some(&mut audio)).await.unwrap();
        assert_eq!(reply.transcript, "Hello there");
        assert_eq!(reply.audio_bytes, 3);
        assert_eq!(audio, [0, 1, 2]);
        session.close().await;
        let frames = seen.lock().await;
        let update = frames.iter().find(|f| f["type"] == "session.update").unwrap();
        assert_eq!(update["session"]["audio"]["output"]["voice"], "eve");
        assert_eq!(update["session"]["output_modalities"], json!(["text"]));
        assert!(frames.iter().any(|f| f["type"] == "conversation.item.create" && f["item"]["content"][0]["text"] == "say hi"));
    }

    #[tokio::test]
    async fn a_server_error_is_reported_with_its_message() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = accept_async(stream).await.unwrap();
            while let Some(Ok(Message::Text(text))) = ws.next().await {
                let v: Value = serde_json::from_str(&text).unwrap();
                let frame = match v["type"].as_str() {
                    Some("session.update") => json!({"type": "session.updated"}),
                    Some("response.create") => json!({"type": "error", "error": {"message": "unknown model"}}),
                    _ => continue,
                };
                ws.send(Message::text(frame.to_string())).await.unwrap();
            }
        });
        let cfg = cfg(&format!("http://127.0.0.1:{port}/v1"), "");
        let error = ask(&cfg, "hi", None).await.unwrap_err().to_string();
        assert!(error.contains("unknown model"), "{error}");
    }
}
