//! Image handling: describe-for-text-models, resizing, URL publishing and terminal inline rendering.
use crate::config::{Config, files::ImageUrlsCfg};
use anyhow::{anyhow, bail, Context, Result};
use base64::Engine;
use serde_json::json;
use std::{
    collections::HashMap,
    sync::{Arc, LazyLock, Mutex},
    time::{Duration, Instant},
};

/// Largest edge native keeps when `images.autoResize` is on; OMP's default.
const MAX_EDGE: u32 = 2000;

fn ext_of(media: &str) -> &'static str {
    match media {
        "image/png" => "png",
        "image/gif" => "gif",
        "image/webp" => "webp",
        _ => "jpg",
    }
}

/// Shrinks an image that exceeds `MAX_EDGE` on either side, keeping its format.
/// `None` when it is already small enough or cannot be decoded (the caller keeps the original).
pub fn resize(media: &str, bytes: &[u8]) -> Option<(String, Vec<u8>)> {
    use image::{ExtendedColorType, ImageEncoder, ImageReader};
    // Animated GIFs cannot survive a re-encode, and every other format round-trips as PNG or JPEG.
    if media == "image/gif" {
        return None;
    }
    let reader = ImageReader::new(std::io::Cursor::new(bytes)).with_guessed_format().ok()?;
    let (w, h) = reader.into_dimensions().ok()?;
    if w <= MAX_EDGE && h <= MAX_EDGE {
        return None;
    }
    let image = ImageReader::new(std::io::Cursor::new(bytes)).with_guessed_format().ok()?.decode().ok()?;
    let image = image.resize(MAX_EDGE, MAX_EDGE, image::imageops::FilterType::Lanczos3);
    let mut out = Vec::new();
    let media = if media == "image/png" {
        image::codecs::png::PngEncoder::new(&mut out).write_image(image.as_bytes(), image.width(), image.height(), ExtendedColorType::from(image.color())).ok()?;
        "image/png"
    } else {
        let rgb = image.to_rgb8();
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 92)
            .write_image(rgb.as_raw(), rgb.width(), rgb.height(), ExtendedColorType::Rgb8)
            .ok()?;
        "image/jpeg"
    };
    Some((media.to_string(), out))
}

/// Rewrites every image block in `messages` in place for the model that will read them.
///
/// A model without vision gets a saved `local://` copy plus a vision model's description; a vision
/// model gets a resized image, published as a URL when `images.urls.enabled` and a backend works.
/// Returned notices are shown to the user (a failed publish falls back to inline).
pub async fn prepare(cfg: &Config, model: &str, messages: &mut [crate::Message]) -> Vec<String> {
    let files = &cfg.file_opts.images;
    if files.block_images {
        return Vec::new();
    }
    let vision = reads_images(cfg, model);
    let describe = files.describe_for_text_models && !vision;
    if !describe && !files.auto_resize && !files.urls.enabled {
        return Vec::new();
    }
    let mut notices = Vec::new();
    for message in messages {
        for block in &mut message.content {
            let crate::Block::Other(v) = block else { continue };
            // Screenshot blocks carry a path instead of bytes and are materialized by `image_bytes`.
            if v["type"] != "image" && v["type"] != "greencode_screenshot" {
                continue;
            }
            if v["source"]["type"] == "url" {
                // A resumed conversation re-arms the same link instead of publishing a second copy.
                if files.urls.enabled {
                    if let Some(url) = v["source"]["url"].as_str() {
                        rearm(url, files.urls.ttl_hours);
                    }
                }
                continue;
            }
            let Some((media, bytes)) = crate::context::image_bytes(v) else { continue };
            let media = media.to_string();
            if describe {
                match describe_cached(cfg, model, &media, &bytes).await {
                    Ok(text) => *block = crate::Block::Text { text },
                    // Dropping the image silently would lose what the user sent; keep the inline copy.
                    Err(e) => notices.push(format!("Image kept inline (no description): {e:#}")),
                }
                continue;
            }
            let (media, bytes) = match files.auto_resize.then(|| resize(&media, &bytes)).flatten() {
                Some((media, bytes)) => (media, bytes),
                None => (media.to_string(), bytes),
            };
            if !files.urls.enabled {
                *v = image_block(&media, &bytes);
                continue;
            }
            match publish_config(&files.urls, &media, &bytes).await {
                Ok(url) => *v = json!({"type": "image", "source": {"type": "url", "url": url}}),
                Err(e) => {
                    notices.push(format!("Image kept inline: {e:#}"));
                    *v = image_block(&media, &bytes);
                }
            }
        }
    }
    notices
}

pub fn image_block(media: &str, bytes: &[u8]) -> serde_json::Value {
    json!({"type": "image", "source": {"type": "base64", "media_type": media,
        "data": base64::engine::general_purpose::STANDARD.encode(bytes)}})
}

/// Saved path for an image the current model cannot see, exposed to the model as `local://<path>`.
pub fn save_image(bytes: &[u8], ext: &str) -> Result<std::path::PathBuf> {
    use sha2::{Digest, Sha256};
    let dir = crate::home().join("media");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{:x}.{ext}", Sha256::digest(bytes)));
    if !path.exists() {
        std::fs::write(&path, bytes)?;
    }
    Ok(path)
}

/// Descriptions already produced in this process, keyed by image hash: a resumed or repeated image
/// costs one vision call, not one per request.
static DESCRIBED: LazyLock<Mutex<HashMap<[u8; 32], String>>> = LazyLock::new(Default::default);

async fn describe_cached(cfg: &Config, model: &str, media: &str, bytes: &[u8]) -> Result<String> {
    use sha2::{Digest, Sha256};
    let hash: [u8; 32] = Sha256::digest(bytes).into();
    let cached = DESCRIBED.lock().unwrap().get(&hash).cloned();
    if let Some(text) = cached {
        return Ok(text);
    }
    let path = save_image(bytes, ext_of(media))?;
    let answer = ask(
        cfg,
        model,
        media,
        bytes,
        "Describe this image for a text-only assistant: what it is, any text in it, and any error or detail that matters. Under 200 words.",
    )
    .await?;
    let text = format!("[Image saved at local://{}; the model that received this message cannot see images, so a vision model described it]\n{answer}", path.display());
    DESCRIBED.lock().unwrap().insert(hash, text.clone());
    Ok(text)
}

/// Asks a vision-capable model about an image (`read`'s `?q=`), failing fast on a stalled provider.
pub async fn ask(cfg: &Config, model: &str, media: &str, bytes: &[u8], question: &str) -> Result<String> {
    let spec = vision_model(cfg, model)?;
    let timeout = cfg.file_opts.images.question_timeout_ms;
    let (provider, name) = crate::provider::make(cfg, &spec)?;
    let message = crate::Message {
        role: crate::Role::User,
        content: vec![
            crate::Block::Text { text: question.to_string() },
            crate::Block::Other(image_block(media, bytes)),
        ],
    };
    let tools = Vec::new();
    let request = crate::provider::Request {
        model: &name,
        system: "You answer a question about one attached image. Be concise and factual; quote what the image shows.",
        messages: &[message],
        tools: &tools,
        max_tokens: cfg.max_tokens.min(2048),
        live: None,
    };
    let sink = |_: &str| {};
    let call = provider.complete(request, &sink);
    let reply = if timeout == 0 {
        call.await?
    } else {
        tokio::time::timeout(Duration::from_millis(timeout), call)
            .await
            .map_err(|_| anyhow!("image question timed out after {timeout}ms; raise images.question_timeout_ms (0 disables it) or check {spec}"))??
    };
    let text: String = reply
        .blocks
        .iter()
        .filter_map(|block| match block {
            crate::Block::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    anyhow::ensure!(!text.trim().is_empty(), "{spec} returned no answer about the image");
    Ok(text)
}

/// `model_roles.vision`, then `model_roles.default`, then the current model when it reads images.
pub fn vision_model(cfg: &Config, current: &str) -> Result<String> {
    for role in ["vision", "default"] {
        if let Some(model) = cfg.model_roles.get(role) {
            return Ok(model.clone());
        }
    }
    if reads_images(cfg, current) {
        return Ok(current.to_string());
    }
    bail!("no vision model: set `model_roles.vision` (config set model_roles.vision provider/model) or select a model that reads images")
}

/// Whether this configured provider/model accepts image input.
pub fn reads_images(cfg: &Config, spec: &str) -> bool {
    let Some((pname, model)) = spec.split_once('/') else { return false };
    let Some(provider) = cfg.providers.get(pname) else { return false };
    if provider.kind == "codex" || crate::snap::frame_edge(provider, model).is_some() {
        return true;
    }
    // shortcut: id heuristic for third-party endpoints; a false positive fails at the provider.
    let id = model.rsplit('/').next().unwrap_or(model).to_lowercase();
    ["gpt-4", "gpt-5", "gpt-6", "o3", "o4", "claude", "gemini", "llava", "qwen-vl", "qwen2-vl", "qwen3-vl",
        "pixtral", "internvl", "glm-4v", "llama-4"]
        .iter()
        .any(|needle| id.contains(needle))
}

// ---------------------------------------------------------------------------
// URL publishing: a token-addressed in-process blob server plus command and ssh backends.
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct Entry {
    media: String,
    bytes: Vec<u8>,
    /// `None` when `ttl_hours` is 0: alive as long as this process is.
    expires: Option<Instant>,
}

#[derive(Clone, Default)]
struct Broker {
    entries: Arc<Mutex<HashMap<String, Entry>>>,
    port: u16,
    ttl_hours: u64,
}

static BROKER: LazyLock<Mutex<Option<Broker>>> = LazyLock::new(|| Mutex::new(None));
/// Reverse forwards live for the process, so their ssh children are parked here.
static SSH: LazyLock<tokio::sync::Mutex<Vec<tokio::process::Child>>> = LazyLock::new(Default::default);

/// Publishes through the configured backend chain; the first URL wins.
async fn publish_config(urls: &ImageUrlsCfg, media: &str, bytes: &[u8]) -> Result<String> {
    anyhow::ensure!(!urls.backends.is_empty(), "images.urls.backends is empty");
    let mut errors = Vec::new();
    for name in &urls.backends {
        let attempt = match name.as_str() {
            "command" => command_backend(urls, media, bytes).await,
            "ssh" => ssh_backend(urls, media, bytes).await,
            "direct" => direct_backend(urls, media, bytes).await,
            other => Err(anyhow!("unknown backend `{other}`; native supports command, ssh, direct")),
        };
        match attempt {
            Ok(url) => return Ok(url),
            Err(e) => errors.push(format!("{name}: {e:#}")),
        }
    }
    bail!("no image URL backend published the image ({})", errors.join("; "))
}

async fn command_backend(urls: &ImageUrlsCfg, media: &str, bytes: &[u8]) -> Result<String> {
    let template = urls.command.as_ref().filter(|template| !template.trim().is_empty())
        .ok_or_else(|| anyhow!("images.urls.command is empty"))?;
    anyhow::ensure!(template.contains("{file}"), "images.urls.command must reference {{file}}");
    let path = save_image(bytes, ext_of(media))?.display().to_string();
    let argv: Vec<String> = template
        .split_whitespace()
        .map(|token| token.replace("{file}", &path).replace("{mime}", media).replace("{ext}", ext_of(media)))
        .collect();
    let output = tokio::process::Command::new(&argv[0])
        .args(&argv[1..])
        .output()
        .await
        .with_context(|| format!("cannot run `{}`", argv[0]))?;
    anyhow::ensure!(output.status.success(), "`{}` failed: {}", argv[0], String::from_utf8_lossy(&output.stderr).trim());
    // The last URL printed wins, as in OMP; other words on the line (the temp path) are ignored.
    String::from_utf8_lossy(&output.stdout)
        .split_whitespace()
        .filter(|word| word.starts_with("http://") || word.starts_with("https://"))
        .last()
        .map(str::to_string)
        .ok_or_else(|| anyhow!("`{}` printed no URL", argv[0]))
}

async fn direct_backend(urls: &ImageUrlsCfg, media: &str, bytes: &[u8]) -> Result<String> {
    // An explicit port in the base is the port to serve on; a proxy-only base must name the forwarded port.
    let base = urls.public_base_url.clone();
    let port = base.as_deref().and_then(|base| url::Url::parse(base).ok()).and_then(|url| url.port()).unwrap_or(0);
    let broker = broker(&urls.bind_host, port, urls.ttl_hours).await?;
    let token = publish(&broker, media, bytes);
    let base = base.unwrap_or_else(|| format!("http://{}:{}", urls.bind_host, broker.port));
    Ok(format!("{}/{token}.{}", base.trim_end_matches('/'), ext_of(media)))
}

async fn ssh_backend(urls: &ImageUrlsCfg, media: &str, bytes: &[u8]) -> Result<String> {
    let target = urls.ssh_target.as_deref().ok_or_else(|| anyhow!("images.urls.sshTarget is required for the ssh backend"))?;
    let base = urls.public_base_url.as_deref().ok_or_else(|| anyhow!("images.urls.publicBaseUrl is required for the ssh backend"))?;
    let broker = broker(&urls.bind_host, 0, urls.ttl_hours).await?;
    let mut children = SSH.lock().await;
    children.retain_mut(|child| child.try_wait().is_ok_and(|status| status.is_none()));
    if children.is_empty() {
        let mut child = tokio::process::Command::new("ssh")
            .args(["-N", "-o", "ExitOnForwardFailure=yes", "-R",
                &format!("{}:127.0.0.1:{}", urls.ssh_remote_port, broker.port), target])
            .kill_on_drop(true)
            .spawn()
            .with_context(|| format!("cannot run ssh to {target}"))?;
        tokio::time::sleep(Duration::from_millis(600)).await;
        if let Some(status) = child.try_wait()? {
            bail!("ssh to {target} exited ({status}); check the host key and images.urls.sshRemotePort");
        }
        children.push(child);
    }
    let token = publish(&broker, media, bytes);
    Ok(format!("{}/{token}.{}", base.trim_end_matches('/'), ext_of(media)))
}

async fn broker(bind_host: &str, port: u16, ttl_hours: u64) -> Result<Broker> {
    let existing = BROKER.lock().unwrap().clone();
    if let Some(broker) = existing.filter(|broker| port == 0 || broker.port == port) {
        return Ok(broker);
    }
    let listener = tokio::net::TcpListener::bind((bind_host, port))
        .await
        .with_context(|| format!("cannot bind the image blob server to {bind_host}:{port}"))?;
    let broker = Broker { entries: Default::default(), port: listener.local_addr()?.port(), ttl_hours };
    let serving = broker.clone();
    tokio::spawn(async move {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        loop {
            let Ok((mut stream, _)) = listener.accept().await else { continue };
            let broker = serving.clone();
            tokio::spawn(async move {
                let mut buf = vec![0u8; 4096];
                let Ok(n) = stream.read(&mut buf).await else { return };
                let request = String::from_utf8_lossy(&buf[..n]);
                let path = request.split_whitespace().nth(1).unwrap_or("/");
                let token = path.trim_start_matches('/').split('.').next().unwrap_or_default();
                let entry = broker.entries.lock().unwrap().get(token).filter(|entry| !expired(entry)).cloned();
                let Some(entry) = entry else {
                    let _ = stream.write_all(b"HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\nconnection: close\r\n\r\n").await;
                    return;
                };
                // Kept after serving: a provider retry or a second request must still fetch it.
                let headers = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: {}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                    entry.media, entry.bytes.len()
                );
                let _ = stream.write_all(headers.as_bytes()).await;
                let _ = stream.write_all(&entry.bytes).await;
            });
        }
    });
    *BROKER.lock().unwrap() = Some(broker.clone());
    Ok(broker)
}

fn expired(entry: &Entry) -> bool {
    entry.expires.is_some_and(|at| at <= Instant::now())
}

fn publish(broker: &Broker, media: &str, bytes: &[u8]) -> String {
    let mut raw = [0u8; 16];
    let _ = getrandom::fill(&mut raw);
    let token: String = raw.iter().map(|byte| format!("{byte:02x}")).collect();
    let expires = (broker.ttl_hours > 0).then(|| Instant::now() + Duration::from_secs(broker.ttl_hours * 3600));
    broker
        .entries
        .lock()
        .unwrap()
        .insert(token.clone(), Entry { media: media.to_string(), bytes: bytes.to_vec(), expires });
    token
}

fn rearm(url: &str, ttl_hours: u64) {
    if ttl_hours == 0 {
        return;
    }
    let token = url.rsplit('/').next().and_then(|last| last.split('.').next()).unwrap_or_default();
    let Some(broker) = BROKER.lock().unwrap().clone() else { return };
    let mut entries = broker.entries.lock().unwrap();
    if let Some(entry) = entries.get_mut(token) {
        entry.expires = Some(Instant::now() + Duration::from_secs(ttl_hours * 3600));
    }
}

// ---------------------------------------------------------------------------
// Terminal inline images: kitty and iTerm2 escapes.
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Protocol {
    /// Kitty graphics protocol: `kitty`, `ghostty` and `xterm-kitty`.
    Kitty,
    /// iTerm2's OSC 1337 `File=inline=1`, also spoken by WezTerm and mintty.
    Iterm2,
}

/// The terminal's inline-image protocol; `GREENCODE_IMAGE_PROTOCOL` overrides detection.
pub fn protocol() -> Option<Protocol> {
    let override_value = std::env::var("GREENCODE_IMAGE_PROTOCOL").ok();
    protocol_from(
        std::env::var("KITTY_WINDOW_ID").is_ok(),
        std::env::var("TERM").ok().as_deref().unwrap_or_default(),
        std::env::var("TERM_PROGRAM").ok().as_deref().unwrap_or_default(),
        override_value.as_deref(),
    )
}

fn protocol_from(kitty_env: bool, term: &str, term_program: &str, override_value: Option<&str>) -> Option<Protocol> {
    match override_value {
        Some("kitty") => return Some(Protocol::Kitty),
        Some("iterm2") => return Some(Protocol::Iterm2),
        Some("none" | "") => return None,
        Some(_) => return None,
        None => {}
    }
    if kitty_env || term.contains("kitty") || term.contains("ghostty") {
        return Some(Protocol::Kitty);
    }
    matches!(term_program, "iTerm.app" | "WezTerm" | "mintty" | "vscode").then_some(Protocol::Iterm2)
}

/// Escape bytes that draw this image over `cols` terminal cells, and the rows it occupies.
/// PNG is passed through; every other format is re-encoded so kitty's `f=100` accepts it.
pub fn inline_escape(protocol: Protocol, media: &str, bytes: &[u8], cols: u16) -> Option<(Vec<u8>, u16)> {
    let png = to_png(media, bytes)?;
    let (width, height) = image::ImageReader::new(std::io::Cursor::new(&png)).with_guessed_format().ok()?.into_dimensions().ok()?;
    if width == 0 || height == 0 {
        return None;
    }
    // Terminal cells are about twice as tall as wide; half blocks keep the aspect ratio.
    let rows = (u64::from(height) * u64::from(cols) / (u64::from(width) * 2)).max(1) as u16;
    let data = base64::engine::general_purpose::STANDARD.encode(&png);
    let mut escape = Vec::new();
    match protocol {
        Protocol::Kitty => {
            for (index, chunk) in data.as_bytes().chunks(4096).enumerate() {
                let more = u8::from((index + 1) * 4096 < data.len());
                if index == 0 {
                    escape.extend_from_slice(format!("\x1b_Ga=T,f=100,c={cols},r={rows},m={more};").as_bytes());
                } else {
                    escape.extend_from_slice(format!("\x1b_Gm={more};").as_bytes());
                }
                escape.extend_from_slice(chunk);
                escape.extend_from_slice(b"\x1b\\");
            }
        }
        Protocol::Iterm2 => {
            escape.extend_from_slice(format!("\x1b]1337;File=inline=1;width={cols};height={rows};preserveAspectRatio=1:").as_bytes());
            escape.extend_from_slice(data.as_bytes());
            escape.push(0x07);
        }
    }
    Some((escape, rows))
}

/// Re-encodes any decodable image (TIFF and BMP included) as PNG.
pub fn to_png(media: &str, bytes: &[u8]) -> Option<Vec<u8>> {
    if media == "image/png" {
        return Some(bytes.to_vec());
    }
    use image::{ExtendedColorType, ImageEncoder, ImageReader};
    let image = ImageReader::new(std::io::Cursor::new(bytes)).with_guessed_format().ok()?.decode().ok()?;
    let mut out = Vec::new();
    image::codecs::png::PngEncoder::new(&mut out)
        .write_image(image.as_bytes(), image.width(), image.height(), ExtendedColorType::from(image.color()))
        .ok()?;
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png(width: u32, height: u32) -> Vec<u8> {
        let mut bytes = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut bytes, width, height);
            encoder.set_color(png::ColorType::Grayscale);
            encoder.set_depth(png::BitDepth::Eight);
            encoder.write_header().unwrap().write_image_data(&vec![7u8; (width * height) as usize]).unwrap();
        }
        bytes
    }

    #[test]
    fn resize_shrinks_only_oversized_images() {
        assert!(resize("image/png", &png(4, 4)).is_none());
        let (media, bytes) = resize("image/png", &png(2400, 1200)).expect("resized");
        assert_eq!(media, "image/png");
        let (width, height) = image::ImageReader::new(std::io::Cursor::new(&bytes))
            .with_guessed_format().unwrap().into_dimensions().unwrap();
        assert!(width <= MAX_EDGE && height <= MAX_EDGE && width == MAX_EDGE, "{width}x{height}");
    }

    #[test]
    fn inline_escapes_carry_the_image_and_its_row_count() {
        let image = png(200, 100);
        let (kitty, rows) = inline_escape(Protocol::Kitty, "image/png", &image, 40).unwrap();
        assert_eq!(rows, 10);
        let kitty = String::from_utf8(kitty).unwrap();
        assert!(kitty.starts_with("\x1b_Ga=T,f=100,c=40,r=10,m=0;") && kitty.ends_with("\x1b\\"), "{kitty:?}");
        let (iterm, _) = inline_escape(Protocol::Iterm2, "image/png", &image, 40).unwrap();
        assert!(String::from_utf8(iterm).unwrap().starts_with("\x1b]1337;File=inline=1;width=40;height=10;"));
    }

    #[test]
    fn protocol_detection_prefers_the_override_and_known_terminals() {
        assert_eq!(protocol_from(true, "xterm-kitty", "", None), Some(Protocol::Kitty));
        assert_eq!(protocol_from(false, "xterm-256color", "iTerm.app", None), Some(Protocol::Iterm2));
        assert_eq!(protocol_from(false, "dumb", "", None), None);
        assert_eq!(protocol_from(true, "xterm-kitty", "", Some("none")), None);
        assert_eq!(protocol_from(false, "xterm", "", Some("iterm2")), Some(Protocol::Iterm2));
    }

    #[tokio::test]
    async fn direct_backend_serves_a_published_image() {
        let urls = ImageUrlsCfg {
            backends: vec!["direct".into()],
            bind_host: "127.0.0.1".into(),
            ttl_hours: 1,
            ..Default::default()
        };
        let url = direct_backend(&urls, "image/png", b"png-bytes").await.unwrap();
        assert!(url.starts_with("http://127.0.0.1:") && url.ends_with(".png"), "{url}");
        let bytes = reqwest::get(&url).await.unwrap().bytes().await.unwrap();
        assert_eq!(bytes.as_ref(), b"png-bytes");
        // The entry survives a second fetch (a retried request must still resolve).
        assert_eq!(reqwest::get(&url).await.unwrap().bytes().await.unwrap().as_ref(), b"png-bytes");
    }

    #[tokio::test]
    async fn unknown_backends_are_skipped_and_reported() {
        let urls = ImageUrlsCfg {
            backends: vec!["tailscale".into(), "command".into()],
            // `{file}` is required by the template; the last URL on stdout wins, so the path may follow it.
            command: Some("echo https://example.test/{ext} {file}".into()),
            ..Default::default()
        };
        assert_eq!(publish_config(&urls, "image/png", b"x").await.unwrap(), "https://example.test/png");
        let urls = ImageUrlsCfg { backends: vec!["cloudflared".into()], ..Default::default() };
        let error = publish_config(&urls, "image/png", b"x").await.unwrap_err().to_string();
        assert!(error.contains("unknown backend `cloudflared`"), "{error}");
    }

    /// A one-shot OpenAI-compatible SSE server on loopback: the `describe`/`?q=` path reads it.
    async fn mock_vision(answer: &'static str) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            let Ok((mut stream, _)) = listener.accept().await else { return };
            let mut buf = vec![0u8; 65536];
            let _ = stream.read(&mut buf).await;
            let body = format!(
                "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
                json!({"choices": [{"delta": {"content": answer}}]}),
                json!({"choices": [{"delta": {}, "finish_reason": "stop"}], "usage": {"prompt_tokens": 1, "completion_tokens": 1}})
            );
            let _ = stream
                .write_all(format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                ).as_bytes())
                .await;
            let _ = stream.shutdown().await;
        });
        base
    }

    async fn vision_cfg(base: String) -> Config {
        let mut cfg = Config::default();
        cfg.providers.insert("vision".into(), crate::config::ProviderCfg {
            kind: "openai".into(), base_url: base, ..Default::default()
        });
        cfg.model_roles.insert("vision".into(), "vision/qwen2-vl-7b".into());
        cfg
    }

    #[tokio::test]
    async fn text_models_get_a_saved_path_and_the_vision_description() {
        let cfg = vision_cfg(mock_vision("A red square with the text OK.").await).await;
        let mut messages = vec![crate::Message {
            role: crate::Role::User,
            content: vec![crate::Block::Other(crate::image::image_block("image/png", &png(8, 8)))],
        }];
        let notices = prepare(&cfg, "anthropic/claude-opus-5-text-only", &mut messages).await;
        assert!(notices.is_empty(), "{notices:?}");
        let crate::Block::Text { text } = &messages[0].content[0] else { panic!("image not replaced: {:?}", messages[0].content) };
        assert!(text.contains("A red square with the text OK."), "{text}");
        assert!(text.contains("local://") && text.contains(".png"), "{text}");
        let media = crate::home().join("media");
        assert!(std::fs::read_dir(media).unwrap().next().is_some(), "the image was not saved under media/");
    }

    #[tokio::test]
    async fn a_stalled_vision_call_fails_fast_with_the_timeout() {
        use tokio::io::AsyncReadExt;
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        // Accepts and then says nothing, so only the timeout can end the call.
        tokio::spawn(async move {
            let Ok((mut stream, _)) = listener.accept().await else { return };
            let mut buf = vec![0u8; 65536];
            let _ = stream.read(&mut buf).await;
            std::future::pending::<()>().await;
        });
        let mut cfg = vision_cfg(base).await;
        cfg.file_opts.images.question_timeout_ms = 50;
        let error = ask(&cfg, "anthropic/claude-opus-5", "image/png", &png(4, 4), "what is this?").await.unwrap_err();
        assert!(error.to_string().contains("timed out after 50ms"), "{error}");
    }

    #[test]
    fn reads_images_recognises_first_party_and_common_ids() {
        let mut cfg = Config::default();
        cfg.providers.insert("anthropic".into(), crate::config::ProviderCfg { kind: "anthropic".into(), ..Default::default() });
        cfg.providers.insert("local".into(), crate::config::ProviderCfg { kind: "openai".into(), base_url: "http://localhost:1".into(), ..Default::default() });
        assert!(reads_images(&cfg, "anthropic/claude-opus-5"));
        assert!(!reads_images(&cfg, "local/deepseek-chat"));
        assert!(reads_images(&cfg, "local/qwen2-vl-7b"));
        assert!(vision_model(&cfg, "local/deepseek-chat").is_err());
        cfg.model_roles.insert("vision".into(), "local/qwen2-vl-7b".into());
        assert_eq!(vision_model(&cfg, "local/deepseek-chat").unwrap(), "local/qwen2-vl-7b");
    }
}
