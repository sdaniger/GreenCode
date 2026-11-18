use anyhow::{Context, Result};
use greencode_core::session::Session;
use std::{fs::OpenOptions, io::{self, Write}, path::PathBuf};

pub(super) fn export(id: &str, path: Option<PathBuf>) -> Result<PathBuf> {
    let transcript = Session::markdown(id)?;
    let path = match path {
        Some(path) => path,
        None => {
            let dir = greencode_core::home().join("exports");
            std::fs::create_dir_all(&dir)?;
            dir.join(format!("{id}.html"))
        }
    };
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let mut file = io::BufWriter::new(options.open(&path).with_context(|| format!("Cannot create {}", path.display()))?);
    if let Err(err) = render(&mut file, &transcript).and_then(|()| file.flush()) {
        drop(file);
        let _ = std::fs::remove_file(&path);
        return Err(err).with_context(|| format!("Cannot write {}", path.display()));
    }
    Ok(path)
}

fn render(out: &mut impl Write, transcript: &str) -> io::Result<()> {
    out.write_all(b"<!doctype html><meta charset=utf-8>\n<meta http-equiv=Content-Security-Policy content=\"default-src 'none'; style-src 'unsafe-inline'\">\n<title>GreenCode transcript</title>\n<style>:root{color-scheme:light dark}main{max-width:80ch;margin:2em auto;padding:0 1em}pre{white-space:pre-wrap;overflow-wrap:anywhere}</style>\n<main><h1>GreenCode transcript</h1><pre>\n")?;
    let mut start = 0;
    for (at, byte) in transcript.bytes().enumerate() {
        let escaped: &[u8] = match byte {
            b'&' => b"&amp;",
            b'<' => b"&lt;",
            b'>' => b"&gt;",
            _ => continue,
        };
        out.write_all(&transcript.as_bytes()[start..at])?;
        out.write_all(escaped)?;
        start = at + 1;
    }
    out.write_all(&transcript.as_bytes()[start..])?;
    out.write_all(b"</pre></main>\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transcript_cannot_add_active_html() {
        let mut bytes = Vec::new();
        render(&mut bytes, "日本語 & </pre><img src=https://example.org>").unwrap();
        let html = String::from_utf8(bytes).unwrap();
        assert!(html.contains("日本語 &amp; &lt;/pre&gt;&lt;img src=https://example.org&gt;"));
        assert!(!html.contains("<img"));
    }
}
