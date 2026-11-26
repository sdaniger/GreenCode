//! `memory.backend = local`: session summaries kept as text files under the agent home, recalled by
//! keyword. No index, no model: the cheap backend between `off` and Mnemopi's SQLite bank.
use super::{Hit, Backend};
use crate::BoxFut;
use anyhow::Result;
use std::path::{Path, PathBuf};

/// Largest summary kept per session; older text is dropped from the front.
const MAX: usize = 40_000;

pub struct Local {
    dir: PathBuf,
}

impl Local {
    pub fn new(cwd: &Path) -> Self {
        let dir = crate::home().join("memory/local").join(crate::config::mnemopi::project_key(cwd));
        Self { dir }
    }

    fn files(&self) -> Vec<PathBuf> {
        let Ok(entries) = std::fs::read_dir(&self.dir) else { return Vec::new() };
        entries
            .filter_map(|entry| entry.ok().map(|e| e.path()))
            .filter(|path| path.extension().is_some_and(|e| e == "md"))
            .collect()
    }
}

impl Backend for Local {
    fn instructions<'a>(&'a self) -> BoxFut<'a, Result<String>> {
        // Curated `MEMORY.md` files are already injected by `context::session_context`.
        Box::pin(async { Ok(String::new()) })
    }

    fn recall<'a>(&'a self, query: &'a str, limit: usize) -> BoxFut<'a, Result<Vec<Hit>>> {
        Box::pin(async move {
            let terms: Vec<String> = query
                .split(|c: char| !c.is_alphanumeric())
                .map(str::to_lowercase)
                .filter(|t| t.chars().count() >= 4)
                .collect();
            if terms.is_empty() {
                return Ok(vec![]);
            }
            let mut scored: Vec<(usize, String)> = Vec::new();
            for path in self.files() {
                let Ok(text) = std::fs::read_to_string(&path) else { continue };
                for line in text.lines() {
                    let line = line.trim().trim_start_matches(['[', '-']).trim();
                    if line.len() < 20 {
                        continue;
                    }
                    let lower = line.to_lowercase();
                    let score = terms.iter().filter(|term| lower.contains(term.as_str())).count();
                    if score > 0 {
                        scored.push((score, line.chars().take(400).collect()));
                    }
                }
            }
            scored.sort_by(|a, b| b.0.cmp(&a.0));
            Ok(scored
                .into_iter()
                .take(limit)
                .map(|(score, text)| Hit { text, source: Some("local".into()), score: Some(score as f64) })
                .collect())
        })
    }

    fn retain<'a>(&'a self, session: &'a str, text: &'a str) -> BoxFut<'a, Result<()>> {
        Box::pin(async move {
            std::fs::create_dir_all(&self.dir)?;
            let name: String = session
                .chars()
                .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '-' })
                .collect();
            let file = self.dir.join(format!("{}.md", if name.is_empty() { "session" } else { &name }));
            let text: String = text.chars().skip(text.chars().count().saturating_sub(MAX)).collect();
            std::fs::write(file, text)?;
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn retain_then_recall_finds_a_line_by_keyword() {
        let dir = tempfile::tempdir().unwrap();
        let local = Local { dir: dir.path().to_path_buf() };
        local
            .retain("s1", "[role: user]\nThe rate limiter sits behind nginx\n[user:end]\n[role: assistant]\nConfig lives in nginx.conf\n[assistant:end]")
            .await
            .unwrap();
        let hits = local.recall("where is the rate limiter configured", 8).await.unwrap();
        assert!(hits.iter().any(|h| h.text.contains("nginx")), "{hits:?}");
        assert_eq!(hits[0].source.as_deref(), Some("local"));
        assert!(local.recall("anything", 8).await.unwrap().is_empty(), "no term of four characters");
        // Re-retaining a session overwrites its file instead of appending a duplicate.
        local.retain("s1", "[role: user]\nNow it moved to Caddy\n[user:end]").await.unwrap();
        assert!(local.recall("nginx", 8).await.unwrap().is_empty());
        assert_eq!(local.recall("caddy", 8).await.unwrap().len(), 1);
    }
}
