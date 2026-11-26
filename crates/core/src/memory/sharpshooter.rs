//! Sharpshooter: project decision memory. Each stop extracts durable decisions from the last user
//! turn with `sharpshooter.model` (empty = the `smol` role), keeps them per project under
//! `~/.greencode/sharpshooter/<project>/decisions.jsonl`, and injects them as instructions.
use crate::config::Config;
use crate::memory::{Backend, Hit};
use crate::BoxFut;
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// Requests shorter than this are greetings or one-word answers; nothing durable to extract.
const MIN_PROMPT: usize = 16;
/// Decisions injected at boot are cut here (characters), newest kept.
const INJECT_LIMIT: usize = 15_000;
const SYSTEM: &str = "You extract durable project decisions from a coding conversation. \
Reply with JSON only: {\"deltas\":[{\"statement\":\"...\",\"kind\":\"architecture_decision\",\"rationale\":\"...\"}]}. \
Include only decisions that are stable across future sessions (architecture, dependencies, conventions, tooling, product choices). \
Exclude transient plans, open questions, and task state; reply with {\"deltas\":[]} when nothing qualifies.";

pub struct Sharpshooter {
    cfg: Config,
    dir: PathBuf,
    /// Resolved model spec; `None` when neither `sharpshooter.model` nor the `smol` role is set.
    model: Option<String>,
    /// The last extracted prompt, so the same text never buys a second request.
    last: Mutex<Option<String>>,
}

impl Sharpshooter {
    pub fn new(cfg: &Config, cwd: &Path) -> Option<Sharpshooter> {
        Some(Sharpshooter::with_root(cfg, cwd, crate::home().join("sharpshooter")))
    }

    fn with_root(cfg: &Config, cwd: &Path, root: PathBuf) -> Sharpshooter {
        let configured = cfg
            .memory_opts
            .sharpshooter
            .model
            .as_deref()
            .map(str::trim)
            .filter(|model| !model.is_empty())
            .map(str::to_string);
        let model = configured.or_else(|| {
            cfg.model_roles
                .get("smol")
                .map(|model| model.trim().to_string())
                .filter(|model| !model.is_empty())
        });
        Sharpshooter {
            cfg: cfg.clone(),
            dir: root.join(crate::memory::remote::project_slug(cwd)),
            model,
            last: Mutex::new(None),
        }
    }

    fn decisions_file(&self) -> PathBuf {
        self.dir.join("decisions.jsonl")
    }

    /// Stored decisions, oldest first; unreadable lines are skipped.
    fn decisions(&self) -> Vec<Value> {
        std::fs::read_to_string(self.decisions_file())
            .unwrap_or_default()
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .filter(|value| value["statement"].as_str().is_some_and(|text| !text.trim().is_empty()))
            .collect()
    }

    fn append(&self, deltas: &[Value], prompt: &str) -> Result<usize> {
        let mut known: Vec<String> = self
            .decisions()
            .iter()
            .filter_map(|value| value["statement"].as_str().map(normalize))
            .collect();
        let mut lines = String::new();
        let mut added = 0;
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or_default();
        for delta in deltas {
            let statement = delta["statement"].as_str().unwrap_or("").trim();
            if statement.is_empty() || known.contains(&normalize(statement)) {
                continue;
            }
            let record = json!({
                "ts": stamp,
                "kind": delta["kind"].as_str().unwrap_or("decision"),
                "statement": statement,
                "rationale": delta["rationale"].as_str().unwrap_or("").trim(),
                "source": prompt.trim(),
            });
            lines.push_str(&serde_json::to_string(&record)?);
            lines.push('\n');
            known.push(normalize(statement));
            added += 1;
        }
        if added == 0 {
            return Ok(0);
        }
        std::fs::create_dir_all(&self.dir)
            .with_context(|| format!("cannot create {}", self.dir.display()))?;
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.decisions_file())
            .with_context(|| format!("cannot write {}", self.decisions_file().display()))?;
        file.write_all(lines.as_bytes())?;
        Ok(added)
    }

    /// The newest decisions that fit the injection budget.
    fn render(&self) -> String {
        let decisions = self.decisions();
        if decisions.is_empty() {
            return String::new();
        }
        let mut sections: Vec<String> = Vec::new();
        let mut used = 0;
        let mut truncated = false;
        for decision in decisions.iter().rev() {
            let statement = decision["statement"].as_str().unwrap_or("").trim();
            let kind = decision["kind"].as_str().unwrap_or("decision");
            let section = format!("- {statement} ({kind})");
            if used + section.len() + 1 > INJECT_LIMIT {
                truncated = true;
                break;
            }
            used += section.len() + 1;
            sections.push(section);
        }
        sections.reverse();
        let mut out = String::from("<project_decisions>\n");
        if truncated {
            out.push_str("… older decisions omitted at the injection limit\n");
        }
        out.push_str(&sections.join("\n"));
        out.push_str("\n</project_decisions>");
        out
    }
}

fn normalize(statement: &str) -> String {
    statement
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// The last user turn's text, which is what a retain extracts from.
fn last_user_turn(transcript: &str) -> String {
    const OPEN: &str = "[role: user]\n";
    const CLOSE: &str = "\n[user:end]";
    let at = match transcript.rfind(OPEN) {
        Some(at) => at + OPEN.len(),
        None => return transcript.trim().to_string(),
    };
    let rest = &transcript[at..];
    match rest.rfind(CLOSE) {
        Some(end) => rest[..end].trim().to_string(),
        None => rest.trim().to_string(),
    }
}

/// Parses the model reply, tolerating code fences and surrounding prose.
fn deltas(reply: &str) -> Result<Vec<Value>> {
    let start = reply.find('{').context("the model reply has no JSON object")?;
    let end = reply.rfind('}').context("the model reply has an unterminated JSON object")?;
    let value: Value = serde_json::from_str(&reply[start..=end]).context("the model reply is not valid JSON")?;
    match value["deltas"].as_array() {
        Some(deltas) => Ok(deltas.clone()),
        None => bail!("the model reply has no `deltas` array"),
    }
}

impl Backend for Sharpshooter {
    fn instructions<'a>(&'a self) -> BoxFut<'a, Result<String>> {
        Box::pin(async move { Ok(self.render()) })
    }

    /// Recall is not part of this backend: decisions arrive through `instructions`.
    fn recall<'a>(&'a self, _query: &'a str, _limit: usize) -> BoxFut<'a, Result<Vec<Hit>>> {
        Box::pin(async move { Ok(Vec::new()) })
    }

    fn retain<'a>(&'a self, _session: &'a str, text: &'a str) -> BoxFut<'a, Result<()>> {
        Box::pin(async move {
            let Some(model) = self.model.as_deref() else { return Ok(()) };
            let prompt = last_user_turn(text);
            if prompt.trim().chars().count() < MIN_PROMPT {
                return Ok(());
            }
            if self.last.lock().unwrap().as_deref() == Some(prompt.as_str()) {
                return Ok(());
            }
            let reply = crate::textgen::generate(&self.cfg, model, SYSTEM, &prompt, 700).await?;
            self.append(&deltas(&reply)?, &prompt)?;
            *self.last.lock().unwrap() = Some(prompt);
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ProviderCfg;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    /// One OpenAI-compatible SSE reply; returns the server so a test can read the request body.
    async fn serve(reply: &str) -> (String, tokio::task::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let reply = reply.to_string();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut buf = vec![0; 8192];
            let mut data = Vec::new();
            let n = stream.read(&mut buf).await.unwrap();
            data.extend_from_slice(&buf[..n]);
            let body = String::from_utf8_lossy(&data).to_string();
            let sse = format!(
                "data: {}\n\ndata: [DONE]\n\n",
                json!({"choices": [{"delta": {"content": reply}, "finish_reason": "stop"}]})
            );
            stream
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{sse}",
                        sse.len()
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
            body
        });
        (base, server)
    }

    fn transcript(user: &str) -> String {
        format!("[role: user]\n{user}\n[user:end]\n[role: assistant]\nok\n[assistant:end]")
    }

    #[tokio::test]
    async fn retain_extracts_decisions_and_instructions_inject_them() {
        let dir = std::env::temp_dir().join(format!("sharpshooter-test-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        let reply = r#"```json
{"deltas":[
 {"statement":"Use pnpm for installs.","kind":"architecture_decision","rationale":"lockfile"},
 {"statement":"Keep providers behind the trait.","kind":"convention"}
]}
```"#;
        let (base, server) = serve(reply).await;
        let mut cfg = Config::default();
        cfg.providers.insert("mock".into(), ProviderCfg { kind: "openai".into(), base_url: base, ..Default::default() });
        cfg.memory_opts.sharpshooter.model = Some("mock/m".into());
        let sharp = Sharpshooter::with_root(&cfg, Path::new("."), dir.clone());
        sharp
            .retain("s1", &transcript("How should installs and providers work in this repo?"))
            .await
            .unwrap();
        let body = server.await.unwrap();
        assert!(body.contains("How should installs and providers work in this repo?"));
        let block = sharp.instructions().await.unwrap();
        assert!(block.starts_with("<project_decisions>\n"), "{block}");
        assert!(block.contains("- Use pnpm for installs. (architecture_decision)"));
        assert!(block.contains("- Keep providers behind the trait. (convention)"));
        // The same transcript must not spend a second request: the mock server accepts only one.
        sharp.retain("s1", &transcript("How should installs and providers work in this repo?")).await.unwrap();
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn empty_model_falls_back_to_the_smol_role() {
        let mut cfg = Config::default();
        cfg.model_roles.insert("smol".into(), "mock/small".into());
        let sharp = Sharpshooter::with_root(&cfg, Path::new("."), std::env::temp_dir());
        assert_eq!(sharp.model.as_deref(), Some("mock/small"));
        cfg.model_roles.clear();
        let sharp = Sharpshooter::with_root(&cfg, Path::new("."), std::env::temp_dir());
        assert_eq!(sharp.model, None);
    }

    #[test]
    fn render_keeps_newest_decisions_within_the_limit() {
        let dir = std::env::temp_dir().join(format!("sharpshooter-render-{}", std::process::id()));
        let project = dir.join("p");
        std::fs::create_dir_all(&project).unwrap();
        let mut file = String::new();
        for i in 0..400 {
            file.push_str(&json!({"ts": i, "kind": "decision", "statement": format!("decision-{i} {}", "x".repeat(50))}).to_string());
            file.push('\n');
        }
        std::fs::write(project.join("decisions.jsonl"), file).unwrap();
        let cfg = Config::default();
        let mut sharp = Sharpshooter::with_root(&cfg, Path::new("."), dir.clone());
        sharp.dir = project;
        let block = sharp.render();
        assert!(block.len() <= INJECT_LIMIT + 200, "{}", block.len());
        assert!(block.contains("older decisions omitted"));
        assert!(block.contains("decision-399"), "newest kept");
        assert!(!block.contains("decision-0 "), "oldest dropped");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn last_user_turn_and_json_parsing_are_lenient() {
        assert_eq!(last_user_turn(&transcript("fix it")), "fix it");
        let two = format!("{}{}", transcript("first"), transcript("second"));
        assert_eq!(last_user_turn(&two), "second");
        let parsed = deltas("noise {\"deltas\":[{\"statement\":\"a\"}]} tail").unwrap();
        assert_eq!(parsed[0]["statement"], "a");
        assert!(deltas("not json").is_err());
    }
}
