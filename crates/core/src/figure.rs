//! Inline figures: `renderSvg` and `renderEmbeds` let the transcript show SVG fences and
//! `<img>`/`<iframe>` sources inline. Local paths and `http(s)` URLs are fetched, SVG is rasterized,
//! and the TUI draws the result through the terminal's graphics protocol.

/// A figure the reply asked to show inline.
#[derive(Clone, Debug, PartialEq)]
pub struct Figure {
    pub src: String,
}

/// Every figure in `text`, in order: HTML `<img src>`, `<iframe src>`, Markdown `![alt](src)`, and
/// ```svg fences (kept as SVG source with no `src`).
pub fn figures(text: &str, svg: bool, embeds: bool) -> Vec<Figure> {
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(at) = rest.find('<') {
        rest = &rest[at..];
        let Some(end) = rest.find('>') else { break };
        let tag = &rest[1..end];
        let is_img = tag.starts_with("img") && embeds;
        let is_frame = tag.starts_with("iframe") && embeds;
        if is_img || is_frame {
            if let Some(src) = attribute(tag, "src") {
                out.push(Figure { src });
            }
        }
        rest = &rest[end + 1..];
    }
    if embeds {
        let mut rest = text;
        while let Some(at) = rest.find("![") {
            let tail = &rest[at + 2..];
            if let Some(close) = tail.find(']') {
                let after = &tail[close + 1..];
                if let Some(inner) = after.strip_prefix('(') {
                    if let Some(end) = inner.find(')') {
                        out.push(Figure { src: inner[..end].trim().to_string() });
                    }
                }
            }
            rest = tail;
        }
    }
    if svg {
        let mut rest = text;
        while let Some(at) = rest.find("```svg") {
            let tail = &rest[at + 6..];
            let tail = tail.strip_prefix('\n').unwrap_or(tail);
            let Some(end) = tail.find("```") else { break };
            out.push(Figure { src: tail[..end].trim().to_string() });
            rest = &tail[end + 3..];
        }
    }
    out.into_iter().filter(|figure| !figure.src.is_empty()).collect()
}

fn attribute(tag: &str, name: &str) -> Option<String> {
    let needle = format!("{name}=");
    let at = tag.find(&needle)? + needle.len();
    let rest = tag[at..].trim_start();
    let rest = rest.strip_prefix(['"', '\'']).unwrap_or(rest);
    let end = rest.find(['"', '\'', ' ', '>']).unwrap_or(rest.len());
    Some(rest[..end].to_string())
}

/// Whether a source is SVG rather than a bitmap.
pub fn is_svg(src: &str, bytes: &[u8]) -> bool {
    src.to_ascii_lowercase().ends_with(".svg") || bytes.starts_with(b"<svg") || String::from_utf8_lossy(&bytes[..bytes.len().min(256)]).contains("<svg")
}

/// Rasterize SVG source to PNG bytes at `width` pixels.
pub fn svg_png(source: &str, width: u32) -> Option<Vec<u8>> {
    let tree = resvg::usvg::Tree::from_str(source, &resvg::usvg::Options::default()).ok()?;
    let size = tree.size();
    let scale = width as f32 / size.width().max(1.0);
    let (w, h) = ((size.width() * scale).ceil() as u32, (size.height() * scale).ceil() as u32);
    let mut pixmap = resvg::tiny_skia::Pixmap::new(w.max(1), h.max(1))?;
    resvg::render(&tree, resvg::tiny_skia::Transform::from_scale(scale, scale), &mut pixmap.as_mut());
    pixmap.encode_png().ok()
}

/// Whether a remote figure URL names a private, loopback or link-local host.
pub fn blocked_url(src: &str) -> bool {
    use url::Host;
    let Ok(url) = url::Url::parse(src) else { return true };
    match url.host() {
        Some(Host::Ipv4(ip)) => ip.is_private() || ip.is_loopback() || ip.is_link_local() || ip.is_unspecified() || ip.is_broadcast(),
        Some(Host::Ipv6(ip)) => {
            ip.is_loopback() || ip.is_unspecified() || (ip.segments()[0] & 0xfe00) == 0xfc00 || ip.is_unicast_link_local()
        }
        Some(Host::Domain(name)) => {
            let name = name.trim_end_matches('.').to_ascii_lowercase();
            name == "localhost" || name.ends_with(".localhost") || name.ends_with(".local") || name.ends_with(".internal")
        }
        None => true,
    }
}

/// A figure's media type and bytes: a local path, `file://`, or an `http(s)` URL.
pub async fn load(src: &str, cwd: &std::path::Path) -> anyhow::Result<(String, Vec<u8>)> {
    if src.starts_with("http://") || src.starts_with("https://") {
        anyhow::ensure!(!blocked_url(src), "refusing to fetch a private or loopback URL: {src}");
        let response = reqwest::Client::new().get(src).timeout(std::time::Duration::from_secs(15)).send().await?;
        anyhow::ensure!(response.status().is_success(), "fetch {src} failed: {}", response.status());
        let media = response.headers().get(reqwest::header::CONTENT_TYPE).and_then(|v| v.to_str().ok())
            .map_or_else(|| media_for(src), |m| m.split(';').next().unwrap_or(m).to_string());
        let bytes = response.bytes().await?;
        anyhow::ensure!(bytes.len() <= 8 * 1024 * 1024, "{src} is larger than 8 MiB");
        return Ok((media, bytes.to_vec()));
    }
    let path = src.strip_prefix("file://").unwrap_or(src);
    let path = if path.starts_with("~/") { crate::user_home().join(&path[2..]) } else { cwd.join(path) };
    let bytes = std::fs::read(&path)?;
    anyhow::ensure!(bytes.len() <= 8 * 1024 * 1024, "{} is larger than 8 MiB", path.display());
    Ok((media_for(src), bytes))
}

fn media_for(src: &str) -> String {
    let lower = src.to_ascii_lowercase();
    let ext = lower.rsplit('.').next().unwrap_or_default();
    match ext {
        "svg" => "image/svg+xml",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "html" | "htm" => "text/html",
        _ => "image/png",
    }.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scans_html_markdown_and_svg_fences_without_duplicates() {
        let text = "Look:\n<img src=\"out/chart.png\" width=2>\n![diagram](docs/a.svg)\n```svg\n<svg xmlns=\"http://www.w3.org/2000/svg\"><rect width=\"10\" height=\"10\"/></svg>\n```\n";
        let all = figures(text, true, true);
        assert_eq!(all.len(), 3, "{all:?}");
        assert_eq!(all[0].src, "out/chart.png");
        assert_eq!(all[1].src, "docs/a.svg");
        assert!(all[2].src.starts_with("<svg"));
        assert_eq!(figures(text, false, false), Vec::new());
        assert_eq!(figures("<iframe src='https://x.test/a'></iframe>", true, true)[0].src, "https://x.test/a");
    }

    #[test]
    fn rasterizes_svg_to_png() {
        let svg = "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"40\" height=\"20\"><rect width=\"40\" height=\"20\" fill=\"#0af\"/></svg>";
        let png = svg_png(svg, 40).unwrap();
        assert!(png.starts_with(b"\x89PNG"));
        assert!(is_svg("a.svg", b""));
        assert!(is_svg("a.bin", b"<svg xmlns=\"http://www.w3.org/2000/svg\"/>"));
        assert!(!is_svg("a.png", b"\x89PNG"));
    }

    #[test]
    fn blocks_private_and_loopback_figure_urls() {
        for url in ["http://127.0.0.1/a.png", "http://localhost/x", "http://[::1]/x", "http://10.0.0.2/a", "http://169.254.1.1/a", "http://printer.local/a"] {
            assert!(blocked_url(url), "{url}");
        }
        for url in ["https://example.com/a.png", "https://cdn.example.org/x.svg"] {
            assert!(!blocked_url(url), "{url}");
        }
    }
}
