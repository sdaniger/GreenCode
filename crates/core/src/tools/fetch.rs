//! `web_fetch` reader backends (`providers.fetch`) and the keyless-service gate (`providers.webKeyless`).
use crate::config::{Config, services_extra::FetchProvider};
use anyhow::{bail, Context, Result};
use std::path::PathBuf;

/// Human-readable name for errors and notices.
pub fn label(p: FetchProvider) -> &'static str {
    match p {
        FetchProvider::Auto => "auto",
        FetchProvider::Native => "native",
        FetchProvider::Trafilatura => "trafilatura",
        FetchProvider::Lynx => "lynx",
        FetchProvider::Parallel => "parallel",
        FetchProvider::Firecrawl => "firecrawl",
        FetchProvider::Jina => "jina",
    }
}

/// Environment variable a keyed backend reads; empty for backends that need none.
pub fn key_env(p: FetchProvider) -> &'static str {
    match p {
        FetchProvider::Parallel => "PARALLEL_API_KEY",
        FetchProvider::Firecrawl => "FIRECRAWL_API_KEY",
        FetchProvider::Jina => "JINA_API_KEY",
        _ => "",
    }
}

/// Whether this backend can run without a configured credential.
pub fn keyless(p: FetchProvider) -> bool {
    matches!(p, FetchProvider::Native | FetchProvider::Trafilatura | FetchProvider::Lynx | FetchProvider::Jina)
}

/// Whether `p` may run: it needs no key, has one, or keyless services are explicitly allowed.
pub fn available(p: FetchProvider, keyless_ok: bool) -> bool {
    key_env(p).is_empty() || keyless_ok || std::env::var_os(key_env(p)).is_some_and(|v| !v.is_empty())
}

fn key(p: FetchProvider) -> Option<String> {
    std::env::var(key_env(p)).ok().filter(|v| !v.trim().is_empty())
}

fn client() -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .user_agent("Mozilla/5.0 (Macintosh) GreenCode")
        .timeout(std::time::Duration::from_secs(60))
        .build()?)
}

/// A backend executable: on PATH, or the named `fallback` when `p` is a managed runner.
fn program(name: &str) -> Option<PathBuf> {
    crate::config::executable(name)
}

fn check_url(url: &str) -> Result<()> {
    if !url.starts_with("http://") && !url.starts_with("https://") {
        bail!("url must start with http:// or https://");
    }
    Ok(())
}

async fn native(url: &str) -> Result<String> {
    let resp = client()?.get(url).send().await?.error_for_status()?;
    let html = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|t| t.contains("html"));
    let body = resp.text().await?;
    Ok(if html { super::html_text(&body) } else { body })
}

/// Trafilatura through `uvx` (or a `pip install trafilatura` console script).
async fn trafilatura(url: &str) -> Result<String> {
    let out = if let Some(exe) = program("trafilatura") {
        tokio::process::Command::new(exe).args(["-u", url]).output().await?
    } else if let Some(uvx) = program("uvx") {
        tokio::process::Command::new(uvx)
            .args(["--quiet", "--from", "trafilatura", "trafilatura", "-u", url])
            .output()
            .await?
    } else if let Some(python) = program("python3") {
        tokio::process::Command::new(python).args(["-m", "trafilatura", "-u", url]).output().await?
    } else {
        bail!("trafilatura is not installed; install it with `uv tool install trafilatura` or `pip install trafilatura`")
    };
    if !out.status.success() {
        bail!("trafilatura failed: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

async fn lynx(url: &str) -> Result<String> {
    let Some(exe) = program("lynx") else {
        bail!("lynx is not installed; install the lynx system package to use providers.fetch = lynx")
    };
    let out = tokio::process::Command::new(exe).args(["-dump", "-nolist", url]).output().await?;
    if !out.status.success() {
        bail!("lynx failed: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn missing(p: FetchProvider) -> anyhow::Error {
    anyhow::anyhow!("{} is unset; set it or turn providers.webKeyless on", key_env(p))
}

async fn parallel(url: &str) -> Result<String> {
    let key = key(FetchProvider::Parallel).ok_or_else(|| missing(FetchProvider::Parallel))?;
    let v: serde_json::Value = client()?
        .post("https://api.parallel.ai/v1beta/extract")
        .header("x-api-key", key)
        .json(&serde_json::json!({"urls": [url], "objective": "Return the readable text of the page"}))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let text = v["results"][0]["content"].as_str().or(v["results"][0]["excerpts"][0].as_str());
    text.map(str::to_string).with_context(|| format!("parallel returned no text: {v}"))
}

async fn firecrawl(url: &str) -> Result<String> {
    let key = key(FetchProvider::Firecrawl).ok_or_else(|| missing(FetchProvider::Firecrawl))?;
    let v: serde_json::Value = client()?
        .post("https://api.firecrawl.dev/v2/scrape")
        .bearer_auth(key)
        .json(&serde_json::json!({"url": url, "formats": ["markdown"]}))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    v["data"]["markdown"].as_str().map(str::to_string).with_context(|| format!("firecrawl returned no markdown: {v}"))
}

async fn jina(url: &str) -> Result<String> {
    let mut req = client()?
        .get(format!("https://r.jina.ai/{url}"))
        .header("Accept", "text/markdown");
    if let Some(key) = key(FetchProvider::Jina) {
        req = req.bearer_auth(key);
    }
    Ok(req.send().await?.error_for_status()?.text().await?)
}

/// Reads `url` with the configured backend priority; the first backend that succeeds wins.
pub async fn read(cfg: &Config, url: &str) -> Result<String> {
    check_url(url)?;
    let opts = &cfg.services_extra_opts;
    let keyless_ok = opts.web_keyless.allowed();
    let mut failures = vec![];
    for &backend in opts.fetch_provider.chain() {
        if !available(backend, keyless_ok) {
            failures.push(format!("{}: skipped without a credential", label(backend)));
            continue;
        }
        let result = match backend {
            FetchProvider::Native => native(url).await,
            FetchProvider::Trafilatura => trafilatura(url).await,
            FetchProvider::Lynx => lynx(url).await,
            FetchProvider::Parallel => parallel(url).await,
            FetchProvider::Firecrawl => firecrawl(url).await,
            FetchProvider::Jina => jina(url).await,
            FetchProvider::Auto => unreachable!("auto expands to the chain"),
        };
        match result {
            Ok(text) => return Ok(text),
            Err(e) => failures.push(format!("{}: {e:#}", label(backend))),
        }
    }
    bail!("no reader backend could fetch {url} ({})", failures.join("; "))
}

/// Whether a keyless scraped search engine or reader may be used for this config.
pub fn keyless_allowed(cfg: &Config) -> bool {
    cfg.services_extra_opts.web_keyless.allowed()
}

/// What the user sees when keyless use is switched off and nothing else is configured.
pub const KEYLESS_OFF: &str =
    "No configured web service is available, and providers.webKeyless is off. Configure a search credential or turn providers.webKeyless on.";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::services_extra::{FetchProvider::*, Keyless};

    #[test]
    fn auto_chain_is_the_documented_priority() {
        assert_eq!(FetchProvider::Auto.chain(), &[Native, Trafilatura, Lynx, Parallel, Firecrawl, Jina]);
        for one in [Native, Trafilatura, Lynx, Parallel, Firecrawl, Jina] {
            assert_eq!(one.chain(), &[one]);
        }
    }

    #[test]
    fn keyless_backends_need_the_switch_and_keyed_ones_their_key() {
        assert!(keyless(Native) && keyless(Jina) && !keyless(Parallel) && !keyless(Firecrawl));
        assert!(available(Native, false));
        assert!(!available(Parallel, false));
        assert!(!available(Firecrawl, false));
        let mut cfg = Config::default();
        cfg.services_extra_opts.web_keyless = Keyless::No;
        assert!(!keyless_allowed(&cfg));
        cfg.services_extra_opts.web_keyless = Keyless::Unset;
        assert!(!keyless_allowed(&cfg));
        cfg.services_extra_opts.web_keyless = Keyless::Yes;
        assert!(keyless_allowed(&cfg));
    }

    #[tokio::test]
    async fn native_reader_strips_html_and_rejects_bad_schemes() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let mut buf = [0u8; 1024];
            let _ = socket.read(&mut buf).await;
            socket
                .write_all(b"HTTP/1.1 200 OK\r\ncontent-type: text/html\r\ncontent-length: 30\r\n\r\n<p>Hello <b>there</b></p>plain")
                .await
                .unwrap();
        });
        let mut cfg = Config::default();
        cfg.services_extra_opts.fetch_provider = Native;
        let text = read(&cfg, &format!("http://{addr}/")).await.unwrap();
        assert!(text.contains("Hello there"), "{text}");
        assert!(read(&cfg, "file:///etc/passwd").await.is_err());
    }

    #[tokio::test]
    async fn missing_keyed_backend_names_its_variable() {
        let err = parallel("https://example.test").await.unwrap_err().to_string();
        assert!(err.contains("PARALLEL_API_KEY"), "{err}");
        let err = firecrawl("https://example.test").await.unwrap_err().to_string();
        assert!(err.contains("FIRECRAWL_API_KEY"), "{err}");
    }
}
