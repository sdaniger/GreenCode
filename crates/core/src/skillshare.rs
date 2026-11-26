//! `skills.registryUrl`: install, search and publish skills through a Skillshare-compatible registry.
//!
//! Wire protocol (GreenCode's own; a Skillshare deployment speaks the same shape):
//! - `GET /v1/skills?q=QUERY` → `{"skills":[{"name","version","description"}]}`
//! - `GET /v1/skills/<name>[?version=V]` → `{"name","version","description","files":{path:content}}`
//! - `POST /v1/skills` (bearer token `$GREENCODE_REGISTRY_TOKEN`) same shape → `201`
use crate::config::Config;
use anyhow::{bail, ensure, Context, Result};
use serde_json::{json, Value};
use std::{collections::BTreeMap, path::{Component, Path, PathBuf}, time::Duration};

pub struct Found {
    pub name: String,
    pub version: String,
    pub description: String,
}

fn base(cfg: &Config) -> Result<String> {
    let url = cfg.integration_opts.skills.registry_url.as_deref().context("skills.registry_url is not set")?;
    let base = crate::collab::http_base(url)?;
    if base.starts_with("http://") && !is_loopback(&base) {
        bail!("skills.registry_url must be https:// (http:// is only for localhost)");
    }
    Ok(base)
}

fn is_loopback(base: &str) -> bool {
    let host = base.split_once("://").map(|(_, r)| r).unwrap_or(base).split(['/', ':']).next().unwrap_or_default();
    matches!(host, "localhost" | "127.0.0.1" | "[::1]")
}

fn client() -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder().timeout(Duration::from_secs(60)).build()?)
}

fn token() -> Result<String> {
    std::env::var("GREENCODE_REGISTRY_TOKEN")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .or_else(|| crate::secrets::env().into_iter().find(|(name, _)| name == "REGISTRY_TOKEN").map(|(_, v)| v))
        .context("set GREENCODE_REGISTRY_TOKEN (or save REGISTRY_TOKEN with the secrets tool) to publish")
}

pub async fn search(cfg: &Config, query: &str) -> Result<Vec<Found>> {
    let base = base(cfg)?;
    let mut url = url::Url::parse(&format!("{base}/v1/skills"))?;
    url.query_pairs_mut().append_pair("q", query);
    let response = client()?.get(url).send().await.with_context(|| format!("skill registry {base} is unreachable"))?;
    ensure!(response.status().is_success(), "skill registry answered {}", response.status());
    let value: Value = response.json().await?;
    Ok(value["skills"].as_array().into_iter().flatten().filter_map(|s| Some(Found {
        name: s["name"].as_str()?.to_owned(),
        version: s["version"].as_str().unwrap_or_default().to_owned(),
        description: s["description"].as_str().unwrap_or_default().to_owned(),
    })).collect())
}

/// `name` or `name@version`; writes `<dest>/<name>/...` and returns the installed version.
pub async fn install(cfg: &Config, spec: &str, dest: &Path) -> Result<String> {
    let (name, version) = spec.split_once('@').unwrap_or((spec, ""));
    valid_name(name)?;
    let mut url = url::Url::parse(&format!("{}/v1/skills/{name}", base(cfg)?))?;
    if !version.is_empty() {
        url.query_pairs_mut().append_pair("version", version);
    }
    let request = client()?.get(url);
    let response = request.send().await.context("skill registry is unreachable")?;
    ensure!(response.status().is_success(), "skill registry answered {} for {name}", response.status());
    let value: Value = response.json().await?;
    let version = value["version"].as_str().unwrap_or_default().to_owned();
    let files: BTreeMap<String, String> = serde_json::from_value(value["files"].clone()).context("registry returned no files")?;
    ensure!(files.contains_key("SKILL.md"), "skill {name} has no SKILL.md");
    let target = dest.join(name);
    if target.exists() {
        bail!("skill `{name}` is already installed at {}", target.display());
    }
    let tmp = dest.join(format!(".{name}.tmp"));
    let _ = std::fs::remove_dir_all(&tmp);
    let result = (|| -> Result<()> {
        for (path, content) in &files {
            let relative = safe_relative(path)?;
            let file = tmp.join(relative);
            if let Some(parent) = file.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&file, content)?;
        }
        std::fs::rename(&tmp, &target)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_dir_all(&tmp);
    }
    result.with_context(|| format!("cannot install skill {name}"))?;
    Ok(version)
}

fn valid_name(name: &str) -> Result<()> {
    let bytes = name.as_bytes();
    ensure!(!name.is_empty() && name.len() <= 64, "invalid skill name `{name}`");
    ensure!(bytes[0].is_ascii_lowercase() || bytes[0].is_ascii_digit(), "skill names start with a lowercase letter or digit: `{name}`");
    ensure!(name.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_'), "invalid skill name `{name}`");
    Ok(())
}

/// Registry paths are relative, contain no `..`, and cannot escape the install directory.
fn safe_relative(path: &str) -> Result<PathBuf> {
    let parsed = Path::new(path);
    ensure!(!path.is_empty() && !parsed.is_absolute(), "invalid skill path `{path}`");
    for component in parsed.components() {
        ensure!(matches!(component, Component::Normal(_)), "invalid skill path `{path}`");
    }
    Ok(parsed.to_path_buf())
}

/// Publishes a skill directory: `SKILL.md` plus every file below it, with its frontmatter description.
pub async fn publish(cfg: &Config, dir: &Path, version: Option<&str>) -> Result<String> {
    let text = std::fs::read_to_string(dir.join("SKILL.md")).with_context(|| format!("{} has no SKILL.md", dir.display()))?;
    let (front, _) = frontmatter(&text);
    let name = field(front, "name").map(str::to_owned).context("SKILL.md frontmatter needs a name")?;
    valid_name(&name)?;
    let version = version.map(str::to_owned).or_else(|| field(front, "version").map(str::to_owned)).unwrap_or_else(|| "0.1.0".into());
    let description = field(front, "description").unwrap_or_default().to_owned();
    let mut files = BTreeMap::new();
    collect(dir, dir, &mut files)?;
    let response = client()?.post(format!("{}/v1/skills", base(cfg)?))
        .bearer_auth(token()?)
        .json(&json!({"name": name, "version": version, "description": description, "files": files}))
        .send().await.context("skill registry is unreachable")?;
    ensure!(response.status().is_success(), "skill registry answered {} for {name}", response.status());
    Ok(format!("{name}@{version}"))
}

fn collect(root: &Path, dir: &Path, out: &mut BTreeMap<String, String>) -> Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        // Never publish VCS data, secrets or prior installs.
        let name = entry.file_name().to_string_lossy().into_owned();
        if matches!(name.as_str(), ".git" | ".greencode" | "target" | "node_modules") {
            continue;
        }
        if path.is_dir() {
            collect(root, &path, out)?;
        } else {
            let relative = path.strip_prefix(root)?.to_string_lossy().replace('\\', "/");
            out.insert(relative, std::fs::read_to_string(&path).with_context(|| format!("{} is not UTF-8", path.display()))?);
        }
    }
    Ok(())
}

fn frontmatter(s: &str) -> (&str, &str) {
    if let Some(rest) = s.strip_prefix("---") {
        if let Some(end) = rest.find("\n---") {
            let body = &rest[end + 4..];
            return (&rest[..end], body.strip_prefix('\n').unwrap_or(body));
        }
    }
    ("", s)
}

fn field<'a>(front: &'a str, key: &str) -> Option<&'a str> {
    front.lines().find_map(|l| Some(l.strip_prefix(key)?.strip_prefix(':')?.trim().trim_matches(['"', '\''])))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::localserver::{self, Response};
    use tokio::net::TcpListener;

    fn cfg_for(url: &str) -> Config {
        let mut cfg = Config::default();
        cfg.integration_opts.skills.registry_url = Some(url.into());
        cfg
    }

    async fn registry() -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(localserver::serve(listener, 1024 * 1024, move |request| async move {
            match (request.method.as_str(), request.path.as_str()) {
                ("GET", "/v1/skills") => Response::json(json!({"skills": [{"name": "rust-review", "version": "1.2.0", "description": "Review Rust"}]})),
                ("GET", "/v1/skills/rust-review") => Response::json(json!({
                    "name": "rust-review", "version": "1.2.0",
                    "files": {"SKILL.md": "---\nname: rust-review\ndescription: Review Rust\n---\nBody\n", "notes/extra.md": "extra"}
                })),
                ("GET", "/v1/skills/bad") => Response::json(json!({"name": "bad", "version": "1", "files": {"../escape.md": "x"}})),
                ("POST", "/v1/skills") => {
                    if request.token() != Some("registry-token") { return Response::error("401 Unauthorized", "token"); }
                    let body: Value = serde_json::from_slice(&request.body).unwrap();
                    assert_eq!(body["name"], "demo");
                    assert!(body["files"]["SKILL.md"].as_str().unwrap().contains("demo body"));
                    Response::new("201 Created", "application/json", json!({"ok": true}).to_string())
                }
                _ => Response::error("404 Not Found", "not found"),
            }
        }));
        base
    }

    #[tokio::test]
    async fn search_install_and_publish_against_a_local_registry() {
        let base = registry().await;
        let cfg = cfg_for(&base);
        let found = search(&cfg, "rust").await.unwrap();
        assert_eq!(found[0].name, "rust-review");
        assert_eq!(found[0].version, "1.2.0");

        let dest = std::env::temp_dir().join(format!("greencode-skillshare-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dest);
        std::fs::create_dir_all(&dest).unwrap();
        let version = install(&cfg, "rust-review", &dest).await.unwrap();
        assert_eq!(version, "1.2.0");
        assert!(dest.join("rust-review/SKILL.md").exists());
        assert_eq!(std::fs::read_to_string(dest.join("rust-review/notes/extra.md")).unwrap(), "extra");
        assert!(install(&cfg, "rust-review", &dest).await.is_err(), "already installed");
        assert!(install(&cfg, "bad", &dest).await.is_err(), "paths may not escape");
        assert!(!dest.join("escape.md").exists() && !dest.join("bad").exists());

        let skill = dest.join("demo");
        std::fs::create_dir_all(skill.join("ref")).unwrap();
        std::fs::write(skill.join("SKILL.md"), "---\nname: demo\ndescription: Demo\n---\ndemo body\n").unwrap();
        std::fs::write(skill.join("ref/a.md"), "a").unwrap();
        unsafe { std::env::set_var("GREENCODE_REGISTRY_TOKEN", "registry-token") };
        assert_eq!(publish(&cfg, &skill, Some("0.2.0")).await.unwrap(), "demo@0.2.0");
        std::fs::remove_dir_all(&dest).ok();
    }

    #[test]
    fn external_registries_must_be_https_and_paths_stay_relative() {
        assert!(base(&cfg_for("http://skills.example.com")).is_err());
        assert_eq!(base(&cfg_for("https://skills.example.com")).unwrap(), "https://skills.example.com");
        assert!(base(&cfg_for("http://127.0.0.1:8080")).is_ok());
        assert!(base(&Config::default()).is_err());
        assert!(valid_name("rust-review").is_ok());
        assert!(valid_name("Rust").is_err());
        assert!(safe_relative("/etc/passwd").is_err());
        assert!(safe_relative("a/../b").is_err());
        assert!(safe_relative("a/b.md").is_ok());
    }
}
