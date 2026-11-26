//! `generate_image`: text-to-image and edits through an OpenAI-compatible images API
//! (`POST {base_url}/images/generations`, `POST {base_url}/images/edits`).
use crate::config::{Config, ProviderCfg};
use anyhow::{bail, Context, Result};
use base64::Engine;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

fn provider<'a>(cfg: &'a Config) -> Result<(&'a str, &'a ProviderCfg)> {
    let name = cfg.extra_tools_opts.generate_image.provider.trim();
    let name = if name.is_empty() { "openai" } else { name };
    let p = cfg
        .providers
        .get(name)
        .with_context(|| format!("generate_image.provider `{name}` is not a configured provider"))?;
    Ok((name, p))
}

/// Generates (or, with `image`, edits) one image and returns the saved PNG path.
pub async fn run(cfg: &Config, args: &Value, cwd: &Path) -> Result<PathBuf> {
    let opts = &cfg.extra_tools_opts.generate_image;
    if !opts.enabled {
        bail!("generate_image is disabled by generate_image.enabled");
    }
    let prompt = args["prompt"]
        .as_str()
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .context("generate_image needs a `prompt`")?;
    let (pname, p) = provider(cfg)?;
    let key = crate::provider::key(pname, p)?;
    let model = args["model"].as_str().map(str::trim).filter(|m| !m.is_empty()).unwrap_or(&opts.model);
    let size = args["size"].as_str().map(str::trim).filter(|s| !s.is_empty()).unwrap_or(&opts.size);
    let base = p.base_url.trim_end_matches('/');
    let http = reqwest::Client::builder()
        .timeout(cfg.provider_opts.web.search_timeout())
        .build()?;
    let mut req = if let Some(input) = args["image"].as_str().filter(|s| !s.trim().is_empty()) {
        let path = crate::permission::resolve(cwd, input);
        let bytes = std::fs::read(&path).with_context(|| path.display().to_string())?;
        let boundary = format!("greencode{:x}", std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_nanos());
        let mut body = Vec::new();
        let mut field = |name: &str, value: &str| {
            body.extend_from_slice(format!("--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n").as_bytes());
        };
        field("model", model);
        field("prompt", prompt);
        field("size", size);
        body.extend_from_slice(
            format!(
                "--{boundary}\r\nContent-Disposition: form-data; name=\"image\"; filename=\"{}\"\r\nContent-Type: application/octet-stream\r\n\r\n",
                path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "image.png".into())
            )
            .as_bytes(),
        );
        body.extend_from_slice(&bytes);
        body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
        http.post(format!("{base}/images/edits"))
            .header(reqwest::header::CONTENT_TYPE, format!("multipart/form-data; boundary={boundary}"))
            .body(body)
    } else {
        let mut payload = json!({"model": model, "prompt": prompt, "n": args["n"].as_u64().unwrap_or(1).clamp(1, 4), "size": size});
        if let Some(format) = args["format"].as_str().filter(|f| !f.trim().is_empty()) {
            payload["output_format"] = json!(format);
        }
        http.post(format!("{base}/images/generations")).json(&payload)
    };
    if !key.is_empty() {
        req = req.bearer_auth(&key);
    }
    let resp = req.send().await.context("images request failed")?;
    let code = resp.status().as_u16();
    let text = resp.text().await?;
    if !(200..300).contains(&code) {
        bail!("images API returned HTTP {code}: {}", text.chars().take(600).collect::<String>());
    }
    let v: Value = serde_json::from_str(&text).context("images API did not return JSON")?;
    let first = v["data"].as_array().and_then(|d| d.first()).context("images API returned no `data`")?;
    let bytes = match (first["b64_json"].as_str(), first["url"].as_str()) {
        (Some(b64), _) => base64::engine::general_purpose::STANDARD.decode(b64).context("bad b64_json")?,
        (None, Some(url)) => http.get(url).send().await?.error_for_status()?.bytes().await?.to_vec(),
        _ => bail!("images API returned neither b64_json nor url"),
    };
    let dir = crate::home().join("images");
    std::fs::create_dir_all(&dir)?;
    let mut hasher = <sha2::Sha256 as sha2::Digest>::new();
    sha2::Digest::update(&mut hasher, &bytes);
    let path = dir.join(format!("{:x}.png", sha2::Digest::finalize(hasher)));
    std::fs::write(&path, &bytes)?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Write};

    fn mock(status: &str, body: String) -> (String, std::sync::mpsc::Receiver<String>) {
        let status = status.to_string();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut request = String::new();
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                if line == "\r\n" || line.is_empty() { break; }
                request.push_str(&line);
            }
            let len: usize = request
                .lines()
                .find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length: ").map(|v| v.trim().parse().unwrap_or(0)))
                .unwrap_or(0);
            let mut body_bytes = vec![0; len];
            std::io::Read::read_exact(&mut reader, &mut body_bytes).unwrap();
            request.push_str(&String::from_utf8_lossy(&body_bytes));
            tx.send(request).unwrap();
            let mut stream = stream;
            write!(stream, "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}", body.len()).unwrap();
        });
        (format!("http://{addr}"), rx)
    }

    fn cfg_for(url: &str) -> Config {
        let mut cfg = Config::default();
        let mut p = cfg.providers.get("openai").cloned().unwrap_or(ProviderCfg {
            kind: "openai".into(),
            base_url: String::new(),
            api_key_env: String::new(),
            api_key: None,
            models: vec![],
            stored: false,
            oauth: false,
        });
        p.base_url = url.into();
        p.api_key = None;
        p.api_key_env = String::new();
        cfg.providers.insert("openai".into(), p);
        cfg.extra_tools_opts.generate_image.enabled = true;
        cfg
    }

    #[tokio::test]
    async fn posts_prompt_and_saves_png() {
        let png = base64::engine::general_purpose::STANDARD.encode(b"\x89PNG\r\n\x1a\nfake");
        let (url, rx) = mock("200 OK", json!({"data": [{"b64_json": png}]}).to_string());
        let cfg = cfg_for(&url);
        let dir = tempfile::tempdir().unwrap();
        let path = run(&cfg, &json!({"prompt": "a red cube", "size": "256x256"}), dir.path()).await.unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"\x89PNG\r\n\x1a\nfake");
        let req = rx.recv().unwrap();
        assert!(req.starts_with("POST /images/generations"), "{req}");
        assert!(req.contains("\"prompt\":\"a red cube\"") && req.contains("256x256"));
    }

    #[tokio::test]
    async fn edits_with_multipart_and_reports_api_errors() {
        let png = base64::engine::general_purpose::STANDARD.encode(b"edited");
        let (url, rx) = mock("200 OK", json!({"data": [{"b64_json": png}]}).to_string());
        let cfg = cfg_for(&url);
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("in.png"), b"orig").unwrap();
        run(&cfg, &json!({"prompt": "make it blue", "image": "in.png"}), dir.path()).await.unwrap();
        let req = rx.recv().unwrap();
        assert!(req.starts_with("POST /images/edits") && req.contains("multipart/form-data"));
        assert!(req.contains("name=\"prompt\"") && req.contains("make it blue"));
        let (url, _rx) = mock("400 Bad Request", json!({"error": {"message": "nope"}}).to_string());
        let err = run(&cfg_for(&url), &json!({"prompt": "x"}), dir.path()).await.unwrap_err();
        assert!(err.to_string().contains("400"), "{err}");
        let mut off = cfg_for("http://127.0.0.1:9");
        off.extra_tools_opts.generate_image.enabled = false;
        assert!(run(&off, &json!({"prompt": "x"}), dir.path()).await.is_err());
        let mut missing = Config::default();
        missing.extra_tools_opts.generate_image.provider = "nope".into();
        assert!(run(&missing, &json!({"prompt": "x"}), dir.path()).await.is_err());
    }
}
