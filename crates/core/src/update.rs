//! Public release feeds for tagged releases and builds from main.

use anyhow::{bail, Context, Result};
use reqwest::Client;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::{fs, path::Path, process::{Command, Stdio}, time::Duration};

const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");
const DEFAULT_REPO: &str = "sdaniger/GreenCode";
const MAX_ASSET_BYTES: usize = 128 * 1024 * 1024;
const MAX_FEED_BYTES: usize = 64 * 1024;

#[derive(serde::Serialize, serde::Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Channel {
    Stable,
    Main,
}

impl Default for Channel {
    fn default() -> Self {
        if main_commit().is_some() { Self::Main } else { Self::Stable }
    }
}

impl std::str::FromStr for Channel {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self> {
        match value {
            "stable" => Ok(Self::Stable),
            "main" => Ok(Self::Main),
            _ => bail!("update channel must be stable or main"),
        }
    }
}

#[derive(Clone, Debug)]
pub struct UpdateResult {
    pub current: String,
    pub latest: String,
    pub url: String,
    pub available: bool,
    pub installed: bool,
}

#[derive(Deserialize)]
struct MainFeed {
    commit: String,
    sha256: std::collections::BTreeMap<String, String>,
}

fn valid_commit(sha: &str) -> bool {
    sha.len() == 40 && sha.bytes().all(|b| b.is_ascii_hexdigit())
}

fn main_commit() -> Option<&'static str> {
    option_env!("GREENCODE_MAIN_COMMIT").filter(|sha| valid_commit(sha))
}

pub fn repository(configured: Option<&str>, override_repo: Option<&str>) -> Result<String> {
    let env_repo = std::env::var("GREENCODE_UPDATE_REPO").ok();
    normalize_repository(override_repo.or(env_repo.as_deref()).or(configured).unwrap_or(DEFAULT_REPO))
}

fn normalize_repository(raw: &str) -> Result<String> {
    let mut value = raw.trim().trim_end_matches('/').to_string();
    if let Some(rest) = value.strip_prefix("https://github.com/") { value = rest.to_string(); }
    if let Some(rest) = value.strip_prefix("http://github.com/") { value = rest.to_string(); }
    if let Some(rest) = value.strip_prefix("github.com/") { value = rest.to_string(); }
    if let Some(rest) = value.strip_suffix(".git") { value = rest.to_string(); }
    let mut parts = value.split('/');
    let (Some(owner), Some(repo), None) = (parts.next(), parts.next(), parts.next()) else {
        bail!("GitHub repository must be OWNER/REPO")
    };
    let valid = |part: &str| !matches!(part, "" | "." | "..") && part.bytes().all(valid_repo_byte);
    if !valid(owner) || !valid(repo) {
        bail!("GitHub repository must be OWNER/REPO")
    }
    Ok(format!("{owner}/{repo}"))
}

fn valid_repo_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.')
}

pub async fn run(repo: &str, channel: Channel, executable: Option<&Path>) -> Result<UpdateResult> {
    let repo = normalize_repository(repo)?;
    let client = Client::builder()
        .user_agent(concat!("greencode/", env!("CARGO_PKG_VERSION")))
        .https_only(true)
        .timeout(Duration::from_secs(120))
        .build()?;
    update_from(&client, "https://github.com", &repo, channel, main_commit(), executable).await
}

async fn update_from(
    client: &Client,
    github: &str,
    repo: &str,
    channel: Channel,
    commit: Option<&str>,
    executable: Option<&Path>,
) -> Result<UpdateResult> {
    let name = asset_name()?;
    let releases = format!("{github}/{repo}/releases");
    let current = commit.map_or_else(|| CURRENT_VERSION.into(), |sha| format!("main@{}", &sha[..12]));
    let (latest, url, available, tag, digest) = match channel {
        Channel::Stable => {
            let response = client.get(format!("{releases}/latest")).send().await
                .and_then(reqwest::Response::error_for_status)
                .map_err(reqwest::Error::without_url)
                .context("could not find the latest stable release")?;
            let url = response.url().clone();
            let (path, tag) = url.path().rsplit_once('/').context("invalid stable release URL")?;
            anyhow::ensure!(
                url.origin() == reqwest::Url::parse(github)?.origin()
                    && path.eq_ignore_ascii_case(&format!("/{repo}/releases/tag")),
                "no stable release is published yet"
            );
            let version = tag.strip_prefix('v').filter(|version| {
                !version.is_empty() && version.bytes().all(|b| valid_repo_byte(b) || b == b'+')
            }).context("invalid stable release tag")?;
            let checksum_url = format!("{releases}/download/{tag}/SHA256SUMS");
            let bytes = download(client, &checksum_url, MAX_FEED_BYTES).await
                .context("could not read the stable release checksums")?;
            let digest = checksum_for(std::str::from_utf8(&bytes)?, &name)
                .with_context(|| format!("stable release {tag} has no checksum for {name}"))?;
            (
                version.to_owned(),
                url.to_string(),
                commit.is_some() || compare_versions(CURRENT_VERSION, version).is_lt(),
                tag.to_owned(),
                digest,
            )
        }
        Channel::Main => {
            let tag = "main-build";
            let feed_url = format!("{releases}/download/{tag}/update.json");
            let bytes = download(client, &feed_url, MAX_FEED_BYTES).await
                .context("could not read the main update feed; retry once the build is published")?;
            let feed: MainFeed = serde_json::from_slice(&bytes).context("invalid main update feed")?;
            let sha = feed.commit.as_str();
            anyhow::ensure!(valid_commit(sha), "main update feed has no valid commit SHA");
            let digest = feed.sha256.get(&name).and_then(|value| parse_digest(value))
                .with_context(|| format!("main update feed has no checksum for {name}"))?;
            (format!("main@{}", &sha[..12]), format!("{github}/{repo}/commit/{sha}"), commit != Some(sha), tag.into(), digest)
        }
    };
    let installed = if let Some(executable) = executable.filter(|_| available) {
        let archive_url = format!("{releases}/download/{tag}/{name}");
        let archive = download(client, &archive_url, MAX_ASSET_BYTES).await
            .context("could not download the update archive")?;
        let actual = format!("{:x}", Sha256::digest(&archive));
        anyhow::ensure!(actual == digest, "update checksum mismatch; retry after the release finishes publishing");
        replace_binary(executable, &archive).context("could not replace the installed GreenCode binaries")?;
        true
    } else {
        false
    };
    Ok(UpdateResult { current, latest, url, available, installed })
}

fn compare_versions(current: &str, latest: &str) -> std::cmp::Ordering {
    let parse = |value: &str| -> Option<Vec<u64>> {
        let value = value.split_once('-').map_or(value, |(v, _)| v);
        let parts: Vec<_> = value.split('.').collect();
        if parts.is_empty() || parts.iter().any(|part| part.is_empty() || !part.bytes().all(|b| b.is_ascii_digit())) { return None; }
        parts.into_iter().map(str::parse).collect::<std::result::Result<Vec<_>, _>>().ok()
    };
    match (parse(current), parse(latest)) {
        (Some(a), Some(b)) => (0..a.len().max(b.len())).find_map(|i| (a.get(i).copied().unwrap_or(0) != b.get(i).copied().unwrap_or(0)).then_some(a.get(i).copied().unwrap_or(0).cmp(&b.get(i).copied().unwrap_or(0)))).unwrap_or(std::cmp::Ordering::Equal),
        _ => current.cmp(latest),
    }
}

async fn download(client: &Client, url: &str, limit: usize) -> Result<Vec<u8>> {
    let mut response = client.get(url).send().await
        .and_then(reqwest::Response::error_for_status)
        .map_err(reqwest::Error::without_url)?;
    let len = response.content_length().unwrap_or(0);
    anyhow::ensure!(len <= limit as u64, "update asset is too large");
    let mut bytes = Vec::with_capacity(len as usize);
    while let Some(chunk) = response.chunk().await.map_err(reqwest::Error::without_url)? {
        anyhow::ensure!(chunk.len() <= limit - bytes.len(), "update asset is too large");
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

fn parse_digest(value: &str) -> Option<String> {
    let value = value.strip_prefix("sha256:").unwrap_or(value).trim();
    (value.len() == 64 && value.bytes().all(|b| b.is_ascii_hexdigit())).then(|| value.to_ascii_lowercase())
}

fn checksum_for(text: &str, name: &str) -> Option<String> {
    text.lines().find_map(|line| {
        let mut fields = line.split_whitespace();
        let digest = parse_digest(fields.next()?)?;
        let file = fields.next()?.trim_start_matches('*');
        (Path::new(file).file_name().and_then(|n| n.to_str()) == Some(name)).then_some(digest)
    })
}

fn asset_name() -> Result<String> {
    let arch = match std::env::consts::ARCH {
        "aarch64" => "arm64",
        "x86_64" => "x86_64",
        other => bail!("updates are not published for macOS {other} yet"),
    };
    match std::env::consts::OS {
        "macos" => Ok(format!("greencode-macos-{arch}.tar.gz")),
        other => bail!("updates are not published for {other} yet"),
    }
}

/// Replaces the running binary and, when installed next to it, its GUI/CLI sibling, so both stay on one build.
fn replace_binary(executable: &Path, archive: &[u8]) -> Result<()> {
    let executable = executable.canonicalize().with_context(|| format!("cannot locate {}", executable.display()))?;
    let parent = executable.parent().context("installed executable has no parent directory")?;
    let name = executable.file_name().context("installed executable has no file name")?;
    let sibling = if name == "greencode" { "greencode-cli" } else { "greencode" };
    let mut targets = vec![executable.clone()];
    if name == "greencode" || name == "greencode-cli" {
        targets.extend(Some(parent.join(sibling)).filter(|p| p.is_file()));
    }
    let staging = tempfile::tempdir_in(parent)?;
    let archive_path = staging.path().join("archive.tar.gz");
    fs::write(&archive_path, archive)?;
    // Extract every binary before replacing any, so a bad archive leaves the install untouched.
    let mut staged = Vec::new();
    for (i, target) in targets.iter().enumerate() {
        let name = target.file_name().context("installed executable has no file name")?;
        let replacement = staging.path().join(format!("binary-{i}"));
        // Extract only the selected binary to stdout; tar never creates archive paths on disk.
        let status = Command::new("tar")
            .arg("-xOzf").arg(&archive_path).arg("--").arg(name)
            .stdout(Stdio::from(fs::File::create(&replacement)?))
            .stderr(Stdio::null())
            .status()?;
        anyhow::ensure!(status.success() && replacement.metadata()?.len() != 0, "update archive has no {} binary", name.to_string_lossy());
        fs::set_permissions(&replacement, target.metadata()?.permissions())?;
        staged.push((replacement, target));
    }
    for (replacement, target) in staged {
        fs::rename(&replacement, target).with_context(|| format!("cannot replace {}", target.display()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repository_inputs_are_normalized_and_validated() {
        assert_eq!(normalize_repository("https://github.com/acme/green.git").unwrap(), "acme/green");
        assert!(normalize_repository("acme/green/other").is_err());
        assert!(normalize_repository("acme/green?x").is_err());
        assert!(normalize_repository("acme/..").is_err());
    }

    #[test]
    fn release_version_comparison_pads_components() {
        assert!(compare_versions("0.1.0", "v0.1.1").is_lt());
        assert!(!compare_versions("0.1.0", "0.1.0").is_lt());
        assert!(!compare_versions("0.1.10", "0.1.2").is_lt());
    }

    #[test]
    fn checksum_lines_match_archive_names() {
        let digest = "a".repeat(64);
        assert_eq!(checksum_for(&format!("{digest} *greencode-macos-arm64.tar.gz\n"), "greencode-macos-arm64.tar.gz"), Some(digest));
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn public_release_feeds_verify_downloads_before_replacement() {
        use std::os::unix::fs::PermissionsExt;
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        fs::create_dir(&source).unwrap();
        fs::write(source.join("greencode-cli"), "#!/bin/sh\nprintf updated").unwrap();
        fs::write(source.join("greencode"), "#!/bin/sh\nprintf gui-updated").unwrap();
        let archive = temp.path().join("update.tar.gz");
        assert!(Command::new("tar").arg("-czf").arg(&archive).arg("-C").arg(&source)
            .arg("greencode-cli").arg("greencode").status().unwrap().success());
        let archive = fs::read(archive).unwrap();
        let executable = temp.path().join("greencode-cli");
        let gui = temp.path().join("greencode");
        for (path, text) in [(&executable, "original"), (&gui, "gui-original")] {
            fs::write(path, format!("#!/bin/sh\nprintf {text}")).unwrap();
            fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
        }
        let run = |path: &Path| Command::new(path).output().unwrap().stdout;
        let output = || run(&executable);

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let sha = "0123456789abcdef0123456789abcdef01234567";
        let name = asset_name().unwrap();
        let digest = format!("{:x}", Sha256::digest(&archive));
        let stable_checksums = format!("{digest}  {name}\n");
        let feed = serde_json::to_vec(&serde_json::json!({
            "commit": sha, "sha256": { (name.as_str()): digest },
        })).unwrap();
        let archive_path = format!("/acme/green/releases/download/main-build/{name}");
        let server = tokio::spawn(async move {
            let feed_path = "/acme/green/releases/download/main-build/update.json";
            let stable_path = "/acme/green/releases/tag/v99.0.0";
            let redirect = format!("302 Found\r\nLocation: {stable_path}");
            for (path, status, body) in [
                ("/acme/green/releases/latest", redirect.as_str(), b"".as_slice()),
                (stable_path, "200 OK", b"".as_slice()),
                ("/acme/green/releases/download/v99.0.0/SHA256SUMS", "200 OK", stable_checksums.as_bytes()),
                (feed_path, "200 OK", feed.as_slice()),
                (feed_path, "200 OK", feed.as_slice()),
                (feed_path, "200 OK", b"{}"),
                (feed_path, "200 OK", feed.as_slice()),
                (archive_path.as_str(), "200 OK", b"incomplete upload"),
                (feed_path, "200 OK", feed.as_slice()),
                (archive_path.as_str(), "200 OK", archive.as_slice()),
            ] {
                let (mut conn, _) = listener.accept().await.unwrap();
                let mut reader = BufReader::new(&mut conn);
                let mut line = String::new();
                reader.read_line(&mut line).await.unwrap();
                assert_eq!(line.trim_end(), format!("GET {path} HTTP/1.1"));
                loop {
                    line.clear();
                    if reader.read_line(&mut line).await.unwrap() == 0 || line == "\r\n" { break; }
                    assert!(!line.to_ascii_lowercase().starts_with("authorization:"));
                }
                let header = format!("HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
                conn.write_all(header.as_bytes()).await.unwrap();
                conn.write_all(body).await.unwrap();
            }
        });

        let client = Client::builder().no_proxy().timeout(Duration::from_secs(5)).build().unwrap();
        let stable = update_from(&client, &base, "acme/green", Channel::Stable, None, None).await.unwrap();
        assert!(stable.available && !stable.installed);
        assert_eq!(stable.latest, "99.0.0");
        assert_eq!(stable.url, format!("{base}/acme/green/releases/tag/v99.0.0"));

        let update = |commit, executable| update_from(&client, &base, "acme/green", Channel::Main, commit, executable);
        let check = update(None, None).await.unwrap();
        assert!(check.available && !check.installed);
        assert_eq!(check.latest, "main@0123456789ab");
        assert_eq!(check.url, format!("{base}/acme/green/commit/{sha}"));

        let unchanged = update(Some(sha), Some(&executable)).await.unwrap();
        assert!(!unchanged.available && !unchanged.installed);
        let error = update(None, Some(&executable)).await.unwrap_err();
        assert!(error.to_string().contains("invalid main update feed"));
        assert_eq!(output(), b"original");
        assert_eq!(run(&gui), b"gui-original");
        let error = update(None, Some(&executable)).await.unwrap_err();
        assert!(error.to_string().contains("checksum mismatch"));
        assert_eq!(output(), b"original");
        assert_eq!(run(&gui), b"gui-original");

        assert!(update(None, Some(&executable)).await.unwrap().installed);
        assert_eq!(output(), b"updated");
        assert_eq!(run(&gui), b"gui-updated", "the GUI next to the CLI moves to the same build");
        server.await.unwrap();
    }
}
