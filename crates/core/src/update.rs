//! Public release feeds for tagged releases and builds from main.

use anyhow::{bail, Context, Result};
use reqwest::Client;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::{fs, path::Path, process::{Command, Stdio}, time::Duration};

const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");
const DEFAULT_REPO: &str = "sdaniger/GreenCode";
const MAX_ASSET_BYTES: usize = 128 * 1024 * 1024;
// shortcut: the manifest lists every ancestor; revisit encoding before the repo reaches 24,000 commits.
const MAX_FEED_BYTES: usize = 1024 * 1024;

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
    /// Why a published build was not offered, when the running build cannot be placed on its history.
    pub note: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Standing {
    Current,
    Newer,
    Unrelated,
    Unknown,
}

fn standing(commit: Option<&str>, latest: &str, ancestors: &[String]) -> Standing {
    match commit {
        Some(current) if latest.eq_ignore_ascii_case(current) => Standing::Current,
        Some(current) if ancestors.iter().any(|ancestor| ancestor.eq_ignore_ascii_case(current)) => Standing::Newer,
        Some(_) => Standing::Unrelated,
        None => Standing::Unknown,
    }
}

#[derive(Deserialize)]
struct ReleaseFeed {
    commit: String,
    // Older manifests have no ancestry proof and cannot authorize an update.
    #[serde(default)]
    ancestors: Vec<String>,
    sha256: std::collections::BTreeMap<String, String>,
}

fn valid_commit(sha: &str) -> bool {
    sha.len() == 40 && sha.bytes().all(|b| b.is_ascii_hexdigit())
}

fn main_commit() -> Option<&'static str> {
    option_env!("GREENCODE_MAIN_COMMIT").filter(|sha| valid_commit(sha))
}

/// The commit this binary was built from. CI records the channel commit explicitly;
/// a source checkout records its HEAD through `build.rs`.
fn build_commit() -> Option<&'static str> {
    main_commit()
        .or_else(|| option_env!("GREENCODE_RELEASE_COMMIT").filter(|sha| valid_commit(sha)))
        .or_else(|| option_env!("GREENCODE_BUILD_COMMIT").filter(|sha| valid_commit(sha)))
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
    let main = main_commit();
    let commit = build_commit();
    let current = main.map_or_else(|| CURRENT_VERSION.into(), |sha| format!("main@{}", &sha[..12]));
    let mut attempt = 0;
    loop {
        attempt += 1;
        match update_from(&client, "https://github.com", &repo, channel, current.clone(), commit, executable).await {
            Ok(result) => return Ok(result),
            // The rolling main release replaces its archives one at a time, so a reader can
            // see a manifest and an archive from different runs. Retry that publish race.
            Err(error) if executable.is_some() && attempt < 3 && transient(&error) => {
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
            Err(error) => return Err(error),
        }
    }
}

fn transient(error: &anyhow::Error) -> bool {
    let text = format!("{error:#}");
    ["could not read the update manifest", "could not download the update archive", "checksum mismatch"]
        .iter()
        .any(|message| text.contains(message))
}

async fn update_from(
    client: &Client,
    github: &str,
    repo: &str,
    channel: Channel,
    current: String,
    commit: Option<&str>,
    executable: Option<&Path>,
) -> Result<UpdateResult> {
    let name = asset_name()?;
    let releases = format!("{github}/{repo}/releases");
    let stable = match channel {
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
            let version = tag.strip_prefix('v').context("invalid stable release tag")?;
            let newer = semver::Version::parse(version).context("invalid stable release version")?
                .cmp_precedence(&semver::Version::parse(CURRENT_VERSION)?).is_gt();
            if !newer {
                let latest = version.to_owned();
                return Ok(UpdateResult { current, latest, url: url.into(), available: false, installed: false, note: None });
            }
            Some((tag.to_owned(), version.to_owned(), url.to_string()))
        }
        Channel::Main => None,
    };
    let stable_build = stable.is_some();
    let tag = stable.as_ref().map_or("main-build", |(tag, _, _)| tag.as_str());
    let archive_url = format!("{releases}/download/{tag}");
    let bytes = download(client, &format!("{archive_url}/update.json"), MAX_FEED_BYTES).await
        .context("could not read the update manifest; retry once the build is published")?;
    let feed: ReleaseFeed = serde_json::from_slice(&bytes).context("invalid update manifest")?;
    let sha = feed.commit.as_str();
    anyhow::ensure!(valid_commit(sha), "update manifest has no valid commit SHA");
    let (latest, url) = match stable {
        Some((_, version, url)) => (version, url),
        None => (format!("main@{}", &sha[..12]), format!("{github}/{repo}/commit/{sha}")),
    };
    let standing = standing(commit, sha, &feed.ancestors);
    let available = match channel {
        // The version check above is the safety gate for tagged releases. A build from
        // main is not an ancestor of an older tag, so ancestry cannot apply here.
        Channel::Stable => stable_build,
        // A build without a recorded commit (an old archive or a source build) cannot
        // prove ancestry. The user picked this channel explicitly and the archive is
        // checksum-verified, so install the published build instead of dead-ending.
        Channel::Main => matches!(standing, Standing::Newer | Standing::Unknown),
    };
    let note = (channel == Channel::Main && standing == Standing::Unrelated).then(|| {
        "公開ビルドは現在のビルドの子孫ではないため、更新を保留しました。".to_string()
    });
    let digest = available.then(|| {
        feed.sha256.get(&name).and_then(|value| parse_digest(value))
            .with_context(|| format!("update manifest has no checksum for {name}"))
    }).transpose()?;
    let installed = if let (Some(executable), Some(digest)) = (executable, digest) {
        let archive = download(client, &format!("{archive_url}/{name}"), MAX_ASSET_BYTES).await
            .context("could not download the update archive")?;
        let actual = format!("{:x}", Sha256::digest(&archive));
        anyhow::ensure!(actual == digest, "update checksum mismatch; retry after the release finishes publishing");
        replace_binary(executable, &archive).context("could not replace the installed GreenCode binaries")?;
        true
    } else {
        false
    };
    Ok(UpdateResult { current, latest, url, available, installed, note })
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
        let older = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let unrelated = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
        let name = asset_name().unwrap();
        let digest = format!("{:x}", Sha256::digest(&archive));
        let legacy_feed = serde_json::to_vec(&serde_json::json!({
            "commit": sha, "sha256": { (name.as_str()): digest },
        })).unwrap();
        let stale_feed = serde_json::to_vec(&serde_json::json!({
            "commit": older, "ancestors": [], "sha256": { (name.as_str()): digest },
        })).unwrap();
        let feed = serde_json::to_vec(&serde_json::json!({
            "commit": sha, "ancestors": [older], "sha256": { (name.as_str()): digest },
        })).unwrap();
        let archive_path = format!("/acme/green/releases/download/main-build/{name}");
        let server = tokio::spawn(async move {
            let feed_path = "/acme/green/releases/download/main-build/update.json";
            let stable_path = "/acme/green/releases/tag/v99.0.0";
            let stable_feed_path = "/acme/green/releases/download/v99.0.0/update.json";
            let redirect = format!("302 Found\r\nLocation: {stable_path}");
            let same_version_path = format!("/acme/green/releases/tag/v{CURRENT_VERSION}");
            let same_version = format!("302 Found\r\nLocation: {same_version_path}");
            for (path, status, body) in [
                // Stable with the running version: no feed is read.
                ("/acme/green/releases/latest", same_version.as_str(), b"".as_slice()),
                (same_version_path.as_str(), "200 OK", b"".as_slice()),
                // Stable newer than the running version.
                ("/acme/green/releases/latest", redirect.as_str(), b"".as_slice()),
                (stable_path, "200 OK", b"".as_slice()),
                (stable_feed_path, "200 OK", feed.as_slice()),
                // Main: a manifest without ancestry cannot prove an upgrade.
                (feed_path, "200 OK", legacy_feed.as_slice()),
                // Main: the published build is older than the running build.
                (feed_path, "200 OK", stale_feed.as_slice()),
                // Main: the running commit is unrelated.
                (feed_path, "200 OK", feed.as_slice()),
                // Main: no recorded commit installs the published build (check only).
                (feed_path, "200 OK", feed.as_slice()),
                // Main: already on the published commit.
                (feed_path, "200 OK", feed.as_slice()),
                // Main: the running commit is an ancestor.
                (feed_path, "200 OK", feed.as_slice()),
                // Stable from a main commit: the newer version is offered even though
                // that main commit is not an ancestor of the tag.
                ("/acme/green/releases/latest", redirect.as_str(), b"".as_slice()),
                (stable_path, "200 OK", b"".as_slice()),
                (stable_feed_path, "200 OK", feed.as_slice()),
                // Invalid manifest.
                (feed_path, "200 OK", b"{}"),
                // A half-published archive fails the checksum.
                (feed_path, "200 OK", feed.as_slice()),
                (archive_path.as_str(), "200 OK", b"incomplete upload"),
                // A complete archive installs both binaries.
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
        let same_version = update_from(&client, &base, "acme/green", Channel::Stable, "main@aaaa".into(), Some(older), Some(&executable)).await.unwrap();
        assert!(!same_version.available && !same_version.installed);
        let stable = update_from(&client, &base, "acme/green", Channel::Stable, CURRENT_VERSION.into(), Some(older), None).await.unwrap();
        assert!(stable.available && !stable.installed);
        assert_eq!(stable.latest, "99.0.0");
        assert_eq!(stable.url, format!("{base}/acme/green/releases/tag/v99.0.0"));

        let check = |commit| update_from(&client, &base, "acme/green", Channel::Main, "main@aaaa".into(), commit, None);
        let legacy = check(Some(older)).await.unwrap();
        assert!(!legacy.available && legacy.note.is_some());
        let stale = check(Some(sha)).await.unwrap();
        assert!(!stale.available && stale.note.is_some());
        let unrelated = check(Some(unrelated)).await.unwrap();
        assert!(!unrelated.available && unrelated.note.is_some());
        let unknown = check(None).await.unwrap();
        assert!(unknown.available && !unknown.installed);
        let unchanged = check(Some(sha)).await.unwrap();
        assert!(!unchanged.available && unchanged.note.is_none());
        let newer = check(Some(older)).await.unwrap();
        assert!(newer.available && !newer.installed);
        assert_eq!(newer.latest, "main@0123456789ab");
        assert_eq!(newer.url, format!("{base}/acme/green/commit/{sha}"));

        // Stable from a main commit: the version gate applies, not main's ancestry.
        let switch = update_from(&client, &base, "acme/green", Channel::Stable, "main@aaaa".into(), Some(sha), None).await.unwrap();
        assert!(switch.available && !switch.installed);
        assert_eq!(switch.latest, "99.0.0");

        let error = update_from(&client, &base, "acme/green", Channel::Main, "main@aaaa".into(), Some(older), Some(&executable)).await.unwrap_err();
        assert!(error.to_string().contains("invalid update manifest"));
        assert_eq!(output(), b"original");
        assert_eq!(run(&gui), b"gui-original");
        let error = update_from(&client, &base, "acme/green", Channel::Main, "main@aaaa".into(), Some(older), Some(&executable)).await.unwrap_err();
        assert!(error.to_string().contains("checksum mismatch"));
        assert_eq!(output(), b"original");
        assert_eq!(run(&gui), b"gui-original");

        assert!(update_from(&client, &base, "acme/green", Channel::Main, "main@aaaa".into(), Some(older), Some(&executable)).await.unwrap().installed);
        assert_eq!(output(), b"updated");
        assert_eq!(run(&gui), b"gui-updated", "the GUI next to the CLI moves to the same build");
        server.await.unwrap();
    }

    #[test]
    fn commit_standing_distinguishes_current_newer_and_unrelated() {
        let older = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string();
        let sha = "0123456789abcdef0123456789abcdef01234567";
        let unrelated = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
        let ancestors = vec![older];
        assert_eq!(standing(Some(sha), sha, &ancestors), Standing::Current);
        assert_eq!(standing(Some(&ancestors[0]), sha, &ancestors), Standing::Newer);
        assert_eq!(standing(Some(unrelated), sha, &ancestors), Standing::Unrelated);
        assert_eq!(standing(None, sha, &ancestors), Standing::Unknown);
    }

    #[test]
    fn publish_races_are_retried() {
        assert!(transient(&anyhow::anyhow!("could not read the update manifest; retry once the build is published")));
        assert!(transient(&anyhow::anyhow!("could not download the update archive")));
        assert!(transient(&anyhow::anyhow!("update checksum mismatch; retry after the release finishes publishing")));
        assert!(!transient(&anyhow::anyhow!("invalid update manifest")));
    }
}
