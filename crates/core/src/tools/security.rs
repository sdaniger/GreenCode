//! `security`: scan planning, execution and the read-only `security://` namespace. A plan is a list of checks;
//! a run stores findings under `~/.greencode/security/<project>/` so later turns read them without re-scanning.
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

fn project_key(cwd: &Path) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(cwd.to_string_lossy().as_bytes());
    format!("{:x}", hasher.finalize())[..16].to_string()
}

fn project_dir(home: &Path, cwd: &Path) -> PathBuf {
    home.join("security").join(project_key(cwd))
}

/// Default checks: secret files that must not be tracked, private keys, obvious key material, and whether
/// dependency manifests exist (their audit is the package manager's job, which needs network).
fn plan_checks() -> Vec<(&'static str, &'static str)> {
    vec![
        ("secrets", "Private key material and bare secrets in the working tree"),
        ("sensitive-files", "Tracked files that usually hold credentials (.env, id_rsa, *.pem, *.key)"),
        ("manifests", "Dependency manifests present, so an audit can be planned"),
        ("permissions", "World-writable files and scripts"),
    ]
}

fn plan(home: &Path, cwd: &Path) -> Result<String> {
    let dir = project_dir(home, cwd);
    std::fs::create_dir_all(&dir)?;
    let checks: Vec<Value> = plan_checks().iter().map(|(id, about)| json!({"id": id, "about": about, "status": "pending"})).collect();
    std::fs::write(dir.join("plan.json"), serde_json::to_vec_pretty(&json!({"cwd": cwd, "checks": checks, "at": now()}))?)?;
    Ok(format!(
        "security plan for {} ({} checks) saved to {}\n{}",
        cwd.display(),
        checks.len(),
        dir.join("plan.json").display(),
        checks.iter().map(|c| format!("- {}: {}", c["id"].as_str().unwrap_or(""), c["about"].as_str().unwrap_or(""))).collect::<Vec<_>>().join("\n")
    ))
}

fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs()
}

fn walk(cwd: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![cwd.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
            let path = entry.path();
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name == ".git" || name == "target" || name == "node_modules" || name == ".greencode" {
                continue;
            }
            let Ok(kind) = entry.file_type() else { continue };
            if kind.is_dir() {
                stack.push(path);
            } else if kind.is_file() {
                out.push(path);
                if out.len() >= 4000 {
                    return out;
                }
            }
        }
    }
    out
}

fn scan(cwd: &Path) -> Vec<Value> {
    let mut findings = Vec::new();
    for path in walk(cwd) {
        let rel = path.strip_prefix(cwd).unwrap_or(&path).display().to_string();
        let name = path.file_name().unwrap_or_default().to_string_lossy().to_lowercase();
        let sensitive = [".env", ".pem", ".key", ".p12", ".pfx"].iter().any(|ext| name.ends_with(ext))
            || name == "id_rsa"
            || name == "id_ed25519";
        if sensitive {
            findings.push(json!({"check": "sensitive-files", "severity": "medium", "path": rel, "detail": "credential-bearing file present in the working tree; keep it out of version control"}));
        }
        let Ok(bytes) = std::fs::read(&path) else { continue };
        if bytes.len() > 512 * 1024 || bytes.contains(&0) {
            continue;
        }
        let text = String::from_utf8_lossy(&bytes);
        if text.contains("-----BEGIN") && text.contains("PRIVATE KEY-----") {
            findings.push(json!({"check": "secrets", "severity": "high", "path": rel, "detail": "embedded private key"}));
        }
        for (label, pattern) in [
            ("aws access key", "AKIA"),
            ("github token", "ghp_"),
            ("openai key", "sk-"),
            ("slack token", "xoxb-"),
        ] {
            if let Some(at) = text.find(pattern) {
                let len: usize = text[at..].chars().take_while(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-').count();
                if len >= match label { "openai key" => 20, "aws access key" => 20, _ => 16 } {
                    findings.push(json!({"check": "secrets", "severity": "high", "path": rel, "detail": format!("{label}-like string")}));
                    break;
                }
            }
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if let Ok(meta) = std::fs::metadata(&path) {
                if meta.permissions().mode() & 0o002 != 0 {
                    findings.push(json!({"check": "permissions", "severity": "low", "path": rel, "detail": "world-writable"}));
                }
            }
        }
    }
    for (manifest, ecosystem) in [("Cargo.toml", "cargo audit"), ("package.json", "npm audit"), ("requirements.txt", "pip-audit"), ("go.mod", "govulncheck")] {
        if cwd.join(manifest).is_file() {
            findings.push(json!({"check": "manifests", "severity": "info", "path": manifest, "detail": format!("plan `{ecosystem}` for dependency vulnerabilities")}));
        }
    }
    findings
}

fn run_scan(home: &Path, cwd: &Path) -> Result<String> {
    let dir = project_dir(home, cwd);
    std::fs::create_dir_all(&dir)?;
    let id = format!("{}-{}", &project_key(cwd)[..6], now());
    let findings = scan(cwd);
    let report = json!({"id": id, "cwd": cwd, "at": now(), "findings": findings, "counts": {
        "high": findings.iter().filter(|f| f["severity"] == "high").count(),
        "medium": findings.iter().filter(|f| f["severity"] == "medium").count(),
        "low": findings.iter().filter(|f| f["severity"] == "low").count(),
        "info": findings.iter().filter(|f| f["severity"] == "info").count(),
    }});
    std::fs::write(dir.join(format!("{id}.json")), serde_json::to_vec_pretty(&report)?)?;
    Ok(format!(
        "scan {id}: {} finding(s) — {} high, {} medium, {} low, {} info\n{}",
        findings.len(),
        report["counts"]["high"],
        report["counts"]["medium"],
        report["counts"]["low"],
        report["counts"]["info"],
        findings.iter().map(summary_line).collect::<Vec<_>>().join("\n")
    ))
}

fn summary_line(f: &Value) -> String {
    format!("[{}] {} {}: {}", f["severity"].as_str().unwrap_or(""), f["check"].as_str().unwrap_or(""), f["path"].as_str().unwrap_or(""), f["detail"].as_str().unwrap_or(""))
}

fn reports(home: &Path, cwd: &Path) -> Vec<(String, Value)> {
    let dir = project_dir(home, cwd);
    let mut out: Vec<(String, Value)> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().into_string().ok()?;
            let id = name.strip_suffix(".json")?.to_string();
            if id == "plan" {
                return None;
            }
            serde_json::from_slice(&std::fs::read(e.path()).ok()?).ok().map(|v| (id, v))
        })
        .collect();
    out.sort_by_key(|(_, v)| std::cmp::Reverse(v["at"].as_u64().unwrap_or(0)));
    out
}

/// The `security` ext tool.
pub fn tool(home: &Path, cwd: &Path, args: &Value) -> Result<String> {
    match args["action"].as_str().context("security needs an `action`")? {
        "plan" => plan(home, cwd),
        "run" => run_scan(home, cwd),
        "list" => {
            let rows: Vec<String> = reports(home, cwd)
                .into_iter()
                .take(args["limit"].as_u64().unwrap_or(10).clamp(1, 100) as usize)
                .map(|(id, v)| format!("{id}\t{}\t{} finding(s)", v["at"], v["findings"].as_array().map_or(0, Vec::len)))
                .collect();
            Ok(if rows.is_empty() { "no scans yet; run `plan` then `run`".into() } else { rows.join("\n") })
        }
        "show" => {
            let id = args["id"].as_str().context("show needs a scan `id`")?;
            show(home, cwd, id)
        }
        other => bail!("unknown security action `{other}`; use plan, run, list or show"),
    }
}

fn show(home: &Path, cwd: &Path, id: &str) -> Result<String> {
    if id.is_empty() || !id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
        bail!("bad scan id `{id}`");
    }
    let path = project_dir(home, cwd).join(format!("{id}.json"));
    let report: Value = serde_json::from_slice(&std::fs::read(&path).with_context(|| format!("no scan `{id}`"))?)?;
    let lines: Vec<String> = report["findings"].as_array().into_iter().flatten().map(summary_line).collect();
    Ok(format!("scan {id} ({}):\n{}", report["cwd"].as_str().unwrap_or(""), lines.join("\n")))
}

/// `security://` → the scan list, `security://<id>` → that report.
pub fn read(home: &Path, cwd: &Path, rest: &str) -> Result<String> {
    let id = rest.trim_matches('/');
    if id.is_empty() {
        let rows: Vec<String> = reports(home, cwd).into_iter().map(|(id, v)| format!("security://{id}\t{} finding(s)", v["findings"].as_array().map_or(0, Vec::len))).collect();
        return Ok(if rows.is_empty() { "no scans yet; call the `security` ext tool with action `run`".into() } else { rows.join("\n") });
    }
    if id == "plan" {
        let path = project_dir(home, cwd).join("plan.json");
        return Ok(std::fs::read_to_string(&path).with_context(|| format!("no plan at {}", path.display()))?);
    }
    show(home, cwd, id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plans_scans_lists_shows_and_reads_namespace() {
        let home = tempfile::tempdir().unwrap();
        let proj = tempfile::tempdir().unwrap();
        let cwd = proj.path();
        std::fs::write(cwd.join(".env"), "TOKEN=abc").unwrap();
        std::fs::write(cwd.join("key.pem"), "-----BEGIN PRIVATE KEY-----\nabc").unwrap();
        std::fs::write(cwd.join("Cargo.toml"), "[package]\nname=\"x\"").unwrap();
        std::fs::write(cwd.join("src.rs"), "let k = \"ghp_abcdefghijklmnop\";").unwrap();
        assert!(tool(home.path(), cwd, &json!({"action": "plan"})).unwrap().contains("4 checks"));
        let run = tool(home.path(), cwd, &json!({"action": "run"})).unwrap();
        assert!(run.contains("scan ") && run.contains("high") && run.contains("key.pem"), "{run}");
        assert!(!run.contains("ghp_abcdefghijklmnop") && run.contains("github token"), "{run}");
        let list = tool(home.path(), cwd, &json!({"action": "list"})).unwrap();
        let id = list.split('\t').next().unwrap().to_string();
        assert!(tool(home.path(), cwd, &json!({"action": "show", "id": id})).unwrap().contains("key.pem"));
        let namespace = read(home.path(), cwd, "").unwrap();
        assert!(namespace.contains(&id), "{namespace}");
        assert!(read(home.path(), cwd, "missing").is_err());
        assert!(read(home.path(), cwd, "../x").is_err());
        assert!(tool(home.path(), cwd, &json!({"action": "bogus"})).is_err());
        assert!(std::fs::metadata(project_dir(home.path(), cwd).join(format!("{id}.json"))).is_ok());
        let empty = tempfile::tempdir().unwrap();
        assert!(tool(home.path(), empty.path(), &json!({"action": "list"})).unwrap().contains("no scans yet"));
    }
}
