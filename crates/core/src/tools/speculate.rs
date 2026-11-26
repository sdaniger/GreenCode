//! Speculative execution: before a tool batch dispatches, validated local `read` calls start at once and
//! their results replace re-reading the same file in the batch. Only reads are prefetched: they have no side
//! effects, so a discarded result costs one file read and nothing else.

use crate::config::{agent_loop::SpeculativeCfg, Config};
use serde_json::Value;
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};

/// A call worth prefetching: a `read` whose path is an existing local file inside `cwd` and is not an image.
/// Only these can be discarded without a trace, and only these decide nothing else (no permission prompt,
/// no nested tool, no network).
pub fn validated_read(cwd: &Path, name: &str, input: &Value) -> Option<PathBuf> {
    if name != "read" {
        return None;
    }
    let raw = input["path"].as_str()?;
    if raw.starts_with("out://") || raw.starts_with("disasm:") {
        return None;
    }
    let path = crate::permission::resolve(cwd, raw);
    let real = path.canonicalize().ok()?;
    let root = cwd.canonicalize().ok()?;
    if !real.starts_with(&root) || !real.is_file() {
        return None;
    }
    let image = matches!(
        real.extension().and_then(|e| e.to_str()).map(str::to_ascii_lowercase).as_deref(),
        Some("png" | "jpg" | "jpeg" | "gif" | "webp")
    );
    (!image).then_some(real)
}

/// Cache key: the resolved path plus the exact bytes selector the call asked for.
pub fn key(cwd: &Path, input: &Value) -> Option<String> {
    let path = validated_read(cwd, "read", input)?;
    Some(format!("{}:{}:{}", path.display(), input["offset"].as_u64().unwrap_or(u64::MAX), input["limit"].as_u64().unwrap_or(u64::MAX)))
}

/// A prefetched read: the cache key it was fetched under, and its text.
#[derive(Clone, Debug)]
pub struct Warm {
    pub key: String,
    pub text: String,
}

/// Runs the validated reads, at most `max_in_flight` at a time, and returns their results keyed by call id.
/// A failed read is not cached: the batch runs it again and reports its real error.
pub async fn prefetch(calls: &[(String, Value)], cwd: &Path, cfg: &Config) -> HashMap<String, Warm> {
    let spec: &SpeculativeCfg = &cfg.loop_opts.speculative_execution;
    if !spec.enabled || spec.max_in_flight == 0 {
        return HashMap::new();
    }
    let mut seen = std::collections::HashSet::new();
    let jobs: Vec<(String, Value, String)> = calls
        .iter()
        .filter_map(|(id, input)| Some((id.clone(), input.clone(), key(cwd, input)?)))
        .filter(|(_, _, k)| seen.insert(k.clone()))
        .take(spec.max_in_flight)
        .collect();
    let sem = std::sync::Arc::new(tokio::sync::Semaphore::new(spec.max_in_flight));
    let runs = jobs.into_iter().map(|(id, input, k)| {
        let sem = sem.clone();
        let cfg = cfg.clone();
        let cwd = cwd.to_path_buf();
        async move {
            let _permit = sem.acquire().await.ok()?;
            let text = crate::tools::run_with("read", &input, &cwd, &cfg).await.ok()?;
            Some((id, Warm { key: k, text }))
        }
    });
    futures_util::future::join_all(runs)
        .await
        .into_iter()
        .flatten()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tempdir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("greencode-speculate-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn only_local_non_image_reads_are_validated() {
        let dir = tempdir("valid");
        let file = dir.join("a.txt");
        std::fs::write(&file, "hello").unwrap();
        std::fs::write(dir.join("b.png"), [0u8; 4]).unwrap();
        assert!(validated_read(&dir, "read", &json!({"path": "a.txt"})).is_some());
        assert_eq!(validated_read(&dir, "read", &json!({"path": "b.png"})), None);
        assert_eq!(validated_read(&dir, "read", &json!({"path": "../outside.txt"})), None);
        assert_eq!(validated_read(&dir, "write", &json!({"path": "a.txt"})), None);
        assert_eq!(validated_read(&dir, "read", &json!({"path": "missing.txt"})), None);
        assert_eq!(validated_read(&dir, "read", &json!({"path": "out://x"})), None);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn prefetch_is_bounded_and_skips_duplicates() {
        let dir = tempdir("bounded");
        std::fs::write(dir.join("a.txt"), "alpha").unwrap();
        std::fs::write(dir.join("b.txt"), "beta").unwrap();
        let mut cfg = Config::default();
        cfg.loop_opts.speculative_execution = SpeculativeCfg { enabled: true, max_in_flight: 1 };
        let calls = vec![
            ("c1".to_string(), json!({"path": "a.txt"})),
            ("c2".to_string(), json!({"path": "a.txt"})),
            ("c3".to_string(), json!({"path": "b.txt"})),
        ];
        let got = prefetch(&calls, &dir, &cfg).await;
        assert_eq!(got.len(), 1, "one slot in flight and the duplicate path collapses");
        assert!(got["c1"].text.contains("alpha"));
        cfg.loop_opts.speculative_execution.max_in_flight = 2;
        let got = prefetch(&calls, &dir, &cfg).await;
        assert_eq!(got.len(), 2);
        assert!(got["c3"].text.contains("beta"));
        cfg.loop_opts.speculative_execution.enabled = false;
        assert!(prefetch(&calls, &dir, &cfg).await.is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }
}
