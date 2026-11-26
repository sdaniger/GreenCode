//! `github` view cache: rendered issue/PR view output kept in the native home so a repeated read is free.
//! Rows are JSON (`cache/github-cache.json` across processes): an entry soft-fresh returns directly, one past
//! the soft TTL returns stale and refreshes in the background, one past the hard TTL is dropped.

use crate::config::{integrations::GithubCacheCfg, Config};
use anyhow::Result;
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    future::Future,
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};

type Rows = BTreeMap<String, (u64, String)>;

fn path() -> PathBuf {
    crate::home().join("cache").join("github-cache.json")
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs()
}

fn load() -> Rows {
    // A corrupt cache costs one `gh` call; never let it fail the tool.
    std::fs::read_to_string(path())
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

fn save(rows: &Rows) {
    let path = path();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(text) = serde_json::to_vec(rows) {
        let _ = std::fs::write(path, text);
    }
}

/// The cache key for one `github` read: the gh arguments plus the repo directory the command ran in.
pub fn key(args: &[String], cwd: &std::path::Path) -> String {
    format!("{}::{}", args.join(" "), cwd.display())
}

/// `Some(text)` when the row may answer this call; `Some` stale rows also schedule a refresh.
/// `None` when caching is off, the row is missing, or it is past the hard TTL.
pub fn lookup(cfg: &Config, key: &str) -> Option<String> {
    let c: &GithubCacheCfg = &cfg.integration_opts.github.cache;
    if !c.enabled {
        return None;
    }
    let rows = load();
    let (stamp, text) = rows.get(key)?;
    let age = now().saturating_sub(*stamp);
    (age < c.hard_ttl_sec.max(1)).then(|| text.clone())
}

/// Whether a hit older than the soft TTL should be refreshed in the background.
pub fn stale(cfg: &Config, key: &str) -> bool {
    let c = &cfg.integration_opts.github.cache;
    if !c.enabled {
        return false;
    }
    load()
        .get(key)
        .is_some_and(|(stamp, _)| now().saturating_sub(*stamp) >= c.soft_ttl_sec)
}

pub fn store(cfg: &Config, key: &str, text: &str) {
    if !cfg.integration_opts.github.cache.enabled {
        return;
    }
    let mut rows = load();
    // Drop everything past the hard TTL so the file cannot grow without bound.
    let hard = cfg.integration_opts.github.cache.hard_ttl_sec.max(1);
    rows.retain(|_, (stamp, _)| now().saturating_sub(*stamp) < hard);
    rows.insert(key.to_string(), (now(), text.to_string()));
    save(&rows);
}

/// Serves one `github` read through the cache: fresh rows answer at once, stale rows answer and refresh,
/// misses run `fetch` and store its result.
pub async fn serve<F, Fut>(cfg: &Config, key: &str, fetch: F) -> Result<String>
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: Future<Output = Result<String>> + Send + 'static,
{
    if let Some(text) = lookup(cfg, key) {
        if stale(cfg, key) {
            let cfg = cfg.clone();
            let key = key.to_string();
            tokio::spawn(async move {
                if let Ok(text) = fetch().await {
                    store(&cfg, &key, &text);
                }
            });
        }
        return Ok(text);
    }
    let text = fetch().await?;
    store(cfg, key, &text);
    Ok(text)
}

/// `GREENCODE_HOME` isolation makes this testable: the row lives under that home, not the real one.
#[cfg(test)]
mod tests {
    use super::*;

    struct Home(PathBuf);
    impl Drop for Home {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn home(name: &str) -> Home {
        let dir = std::env::temp_dir().join(format!("greencode-ghcache-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        crate::set_test_home(Some(&dir));
        Home(dir)
    }

    #[test]
    fn soft_and_hard_ttl_decide_hits_and_drops() {
        let _home = home("ttl");
        let mut cfg = Config::default();
        cfg.integration_opts.github.cache = GithubCacheCfg { enabled: true, soft_ttl_sec: 300, hard_ttl_sec: 7 * 24 * 3600 };
        let key = key(&["pr".into(), "view".into(), "1".into()], std::path::Path::new("/repo"));
        assert!(lookup(&cfg, &key).is_none(), "no row yet");
        store(&cfg, &key, "PR #1");
        assert_eq!(lookup(&cfg, &key).as_deref(), Some("PR #1"));
        assert!(!stale(&cfg, &key), "fresh row needs no refresh");
        // Backdate the row past the soft TTL: it is still served, and marked for refresh.
        let mut rows = load();
        rows.insert(key.clone(), (now() - 301, "PR #1".into()));
        save(&rows);
        assert_eq!(lookup(&cfg, &key).as_deref(), Some("PR #1"));
        assert!(stale(&cfg, &key));
        // Past the hard TTL the row is gone.
        let mut rows = load();
        rows.insert(key.clone(), (now() - 8 * 24 * 3600, "PR #1".into()));
        save(&rows);
        assert!(lookup(&cfg, &key).is_none());
        // Off means no reads and no writes: a fresh key is never stored.
        cfg.integration_opts.github.cache.enabled = false;
        let other = super::key(&["issue".into(), "view".into(), "2".into()], std::path::Path::new("/repo"));
        store(&cfg, &other, "issue #2");
        assert!(!load().contains_key(&other));
    }

    #[tokio::test]
    async fn serve_fetches_once_then_answers_from_cache() {
        let _home = home("serve");
        let mut cfg = Config::default();
        cfg.integration_opts.github.cache.soft_ttl_sec = 300;
        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let first = {
            let calls = calls.clone();
            serve(&cfg, "k", move || async move {
                calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok("rendered".to_string())
            })
            .await
            .unwrap()
        };
        assert_eq!(first, "rendered");
        let second = {
            let calls = calls.clone();
            serve(&cfg, "k", move || async move {
                calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok("rendered".to_string())
            })
            .await
            .unwrap()
        };
        assert_eq!(second, "rendered");
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[test]
    fn rows_are_json_under_the_home_cache_dir() {
        let _home = home("json");
        let cfg = Config::default();
        store(&cfg, "a b", "x");
        let text = std::fs::read_to_string(path()).unwrap();
        let rows: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(rows["a b"][1], json!("x"));
        assert!(path().starts_with(crate::home()));
    }
}
