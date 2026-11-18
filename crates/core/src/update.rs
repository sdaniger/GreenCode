//! GitHub Releases updater for the native binaries.

use anyhow::{bail, Context, Result};
use reqwest::Client;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::{fs, path::{Component, Path, PathBuf}, process::Command};

pub const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");
const MAX_ASSET_BYTES: u64 = 128 * 1024 * 1024;

#[derive(Clone, Debug)]
pub struct Release {
    pub tag: String,
    pub version: String,
    pub url: String,
    asset: Asset,
    checksum: Option<Asset>,
}

#[derive(Clone, Debug)]
pub struct UpdateResult {
    pub current: String,
    pub latest: String,
    pub installed: bool,
}

#[derive(Clone, Debug, Deserialize)]
struct ApiRelease {
    tag_name: String,
    html_url: String,
    assets: Vec<ApiAsset>,
}

#[derive(Clone, Debug, Deserialize)]
struct ApiAsset {
    name: String,
    browser_download_url: String,
    #[serde(default)]
    digest: Option<String>,
}

#[derive(Clone, Debug)]
struct Asset {
    name: String,
    url: String,
    digest: Option<String>,
}

pub fn repository(configured: Option<&str>, override_repo: Option<&str>) -> Result<String> {
    let raw = override_repo
        .map(str::to_owned)
        .or_else(|| std::env::var("GREENCODE_UPDATE_REPO").ok())
        .or_else(|| configured.map(str::to_owned))
        .ok_or_else(|| anyhow::anyhow!("update repository is not configured; set update_repo = \"OWNER/REPO\", GREENCODE_UPDATE_REPO, or pass --repo OWNER/REPO"))?;
    normalize_repository(&raw)
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
    if owner.is_empty() || repo.is_empty() || !owner.bytes().all(valid_repo_byte) || !repo.bytes().all(valid_repo_byte) {
        bail!("GitHub repository must be OWNER/REPO")
    }
    Ok(format!("{owner}/{repo}"))
}

fn valid_repo_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.')
}

pub async fn check(repo: &str) -> Result<Release> {
    let repo = normalize_repository(repo)?;
    let client = Client::builder().user_agent(format!("greencode/{CURRENT_VERSION}")).build()?;
    let api: ApiRelease = client
        .get(format!("https://api.github.com/repos/{repo}/releases/latest"))
        .header("accept", "application/vnd.github+json")
        .send()
        .await?
        .error_for_status()
        .with_context(|| format!("could not read the latest GitHub release for {repo}"))?
        .json()
        .await?;
    let asset_name = asset_name()?;
    let asset = api.assets.iter().find(|a| a.name == asset_name).ok_or_else(|| anyhow::anyhow!("latest release {} has no {asset_name} asset", api.tag_name))?;
    let checksum = api.assets.iter().find(|a| a.name == "SHA256SUMS").map(to_asset);
    Ok(Release {
        tag: api.tag_name.clone(),
        version: api.tag_name.trim_start_matches('v').to_string(),
        url: api.html_url,
        asset: to_asset(asset),
        checksum,
    })
}

fn to_asset(asset: &ApiAsset) -> Asset {
    Asset { name: asset.name.clone(), url: asset.browser_download_url.clone(), digest: asset.digest.clone() }
}

pub fn is_newer(release: &Release) -> bool {
    compare_versions(CURRENT_VERSION, &release.version).is_lt()
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

pub async fn install(repo: &str, executable: &Path) -> Result<UpdateResult> {
    let release = check(repo).await?;
    if !is_newer(&release) {
        return Ok(UpdateResult { current: CURRENT_VERSION.into(), latest: release.version, installed: false });
    }
    let client = Client::builder().user_agent(format!("greencode/{CURRENT_VERSION}")).build()?;
    let response = client.get(&release.asset.url).header("accept", "application/octet-stream").send().await?.error_for_status()?;
    if response.content_length().is_some_and(|n| n > MAX_ASSET_BYTES) { bail!("update asset is too large") }
    let archive = response.bytes().await?;
    if archive.len() as u64 > MAX_ASSET_BYTES { bail!("update asset is too large") }
    verify_digest(&client, &release, &archive).await?;
    replace_binary(executable, &archive, &release.asset.name)?;
    Ok(UpdateResult { current: CURRENT_VERSION.into(), latest: release.version, installed: true })
}

async fn verify_digest(client: &Client, release: &Release, archive: &[u8]) -> Result<()> {
    let expected = if let Some(digest) = release.asset.digest.as_deref().and_then(parse_digest) {
        Some(digest)
    } else if let Some(checksum) = &release.checksum {
        let text = client.get(&checksum.url).header("accept", "text/plain").send().await?.error_for_status()?.text().await?;
        checksum_for(&text, &release.asset.name)
    } else {
        None
    };
    let Some(expected) = expected else { bail!("latest release has no checksum for {}", release.asset.name) };
    let actual = Sha256::digest(archive).iter().map(|b| format!("{b:02x}")).collect::<String>();
    anyhow::ensure!(actual == expected, "update checksum mismatch for {}", release.asset.name);
    Ok(())
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

fn replace_binary(executable: &Path, archive: &[u8], asset_name: &str) -> Result<()> {
    let executable = executable.canonicalize().with_context(|| format!("cannot locate {}", executable.display()))?;
    let parent = executable.parent().ok_or_else(|| anyhow::anyhow!("installed executable has no parent directory"))?;
    let staging = temporary_directory(parent)?;
    let result = (|| {
        let archive_path = staging.join(asset_name);
        fs::write(&archive_path, archive)?;
        let listing = Command::new("tar").args(["-tzf", archive_path.to_str().unwrap_or_default()]).output()?;
        anyhow::ensure!(listing.status.success(), "cannot inspect update archive");
        for entry in String::from_utf8_lossy(&listing.stdout).lines() {
            let path = Path::new(entry);
            anyhow::ensure!(!path.is_absolute() && !path.components().any(|c| matches!(c, Component::ParentDir)), "update archive contains an unsafe path");
        }
        let unpacked = staging.join("unpacked");
        fs::create_dir(&unpacked)?;
        let status = Command::new("tar").args(["-xzf", archive_path.to_str().unwrap_or_default(), "-C", unpacked.to_str().unwrap_or_default()]).status()?;
        anyhow::ensure!(status.success(), "cannot unpack update archive");
        let name = executable.file_name().and_then(|n| n.to_str()).ok_or_else(|| anyhow::anyhow!("installed executable has no file name"))?;
        let candidate = unpacked.join(name);
        anyhow::ensure!(candidate.is_file() && !candidate.symlink_metadata()?.file_type().is_symlink(), "update archive has no {name} binary");
        let replacement = parent.join(format!(".{}.update-{}", name, std::process::id()));
        fs::copy(&candidate, &replacement)?;
        #[cfg(unix)] {
            use std::os::unix::fs::PermissionsExt;
            let mode = executable.metadata()?.permissions().mode();
            fs::set_permissions(&replacement, fs::Permissions::from_mode(mode))?;
        }
        fs::rename(&replacement, &executable).with_context(|| format!("cannot replace {}", executable.display()))?;
        Ok::<(), anyhow::Error>(())
    })();
    let _ = fs::remove_dir_all(&staging);
    result
}

fn temporary_directory(parent: &Path) -> Result<PathBuf> {
    for attempt in 0..100u32 {
        let path = parent.join(format!(".greencode-update-{}-{attempt}", std::process::id()));
        match fs::create_dir(&path) {
            Ok(()) => return Ok(path),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e).with_context(|| format!("cannot create update staging directory in {}", parent.display())),
        }
    }
    bail!("cannot create a unique update staging directory")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repository_inputs_are_normalized_and_validated() {
        assert_eq!(normalize_repository("https://github.com/acme/green.git").unwrap(), "acme/green");
        assert!(normalize_repository("acme/green/other").is_err());
        assert!(normalize_repository("acme/green?x").is_err());
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
}
