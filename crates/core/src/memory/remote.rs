//! Hindsight remote memory: bank naming, retain, recall and mental models over the HTTP API.
//! Endpoints and request shapes follow the Hindsight server: `PUT /v1/default/banks/{bank}`,
//! `POST .../memories`, `POST .../memories/recall`, `GET`/`POST .../mental-models`.
use crate::config::{
    memory::{HindsightCfg, RetainMode, Scoping},
    Config,
};
use crate::memory::{Backend, Hit};
use crate::BoxFut;
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::Duration;

/// The shared bank name when `hindsight.bank_id` is unset; per-project scoping derives from it.
const DEFAULT_BANK: &str = "greencode";
/// Sent above every `<memories>` block so conflicting memories lean recent.
pub const PREAMBLE: &str = "Relevant memories from past conversations (prioritize recent when conflicting). Only use memories that are directly useful to continue this conversation; ignore the rest:";
const PROJECT_TAG: &str = "project:";
/// Recall types the server returns; `world` facts and `experience` episodes.
const RECALL_TYPES: [&str; 2] = ["world", "experience"];
const PAGE: usize = 200;
/// Recall budget the server spends on the answer.
const RECALL_MAX_TOKENS: u32 = 1024;
/// Total render budget for the `<mental_models>` block, in characters.
const MENTAL_MODELS_CHARS: usize = 15_000;
const TRUNCATED: &str = "\n…[truncated at render budget]";

/// Built-in mental-model seeds; created once per bank when absent, never modified afterwards.
struct Seed {
    id: &'static str,
    name: &'static str,
    query: &'static str,
    /// Tags the model with the project so it only refreshes from this project's retains.
    project_tagged: bool,
    max_tokens: u32,
    /// Needs a project, so skipped under global scoping.
    project_only: bool,
}

const SEEDS: [Seed; 3] = [
    Seed {
        id: "user-preferences",
        name: "User Preferences",
        query: "What does the user prefer in coding style, tooling, communication, and review? Capture only durable preferences expressed across sessions, not one-off requests.",
        project_tagged: false,
        max_tokens: 600,
        project_only: false,
    },
    Seed {
        id: "project-conventions",
        name: "Project Conventions",
        query: "What are this project's conventions for code style, build, testing, release, and pull-request review? Only include conventions that are explicit in the project (settings, scripts, contributor docs, repeatedly enforced in review).",
        project_tagged: true,
        max_tokens: 800,
        project_only: true,
    },
    Seed {
        id: "project-decisions",
        name: "Project Decisions",
        query: "What durable architectural or product decisions have been made for this project, and what rationale or trade-offs were recorded? Include only decisions that are stable across sessions; exclude transient plans, unresolved ideas, and active task state.",
        project_tagged: true,
        max_tokens: 800,
        project_only: true,
    },
];

/// The project name used in bank ids and `project:` tags: the repository root's directory name,
/// lowercased, falling back to the working directory's.
pub fn project_slug(cwd: &Path) -> String {
    let dir = std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
    let root = dir.ancestors().find(|dir| dir.join(".git").exists()).unwrap_or(&dir);
    let name = root.file_name().and_then(|name| name.to_str()).unwrap_or("").to_lowercase();
    if name.is_empty() { "unknown".into() } else { name }
}

/// The bank id and the retain/recall tags a scoping choice implies.
pub fn target(cfg: &HindsightCfg, cwd: &Path) -> (String, Vec<String>) {
    let slug = project_slug(cwd);
    let base = cfg
        .bank_id
        .as_deref()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .unwrap_or(DEFAULT_BANK)
        .to_string();
    match cfg.scoping {
        Scoping::Global => (base, Vec::new()),
        Scoping::PerProject => (format!("{base}-{slug}"), Vec::new()),
        Scoping::PerProjectTagged => (base, vec![format!("{PROJECT_TAG}{slug}")]),
    }
}

pub struct Hindsight {
    http: reqwest::Client,
    base: String,
    token: Option<String>,
    bank: String,
    tags: Vec<String>,
    cfg: HindsightCfg,
    ensured: AtomicBool,
    seeded: AtomicBool,
    last_retained: AtomicU32,
}

impl Hindsight {
    /// `None` when `memory.backend = "hindsight"` but no server URL is configured (the backend stays inert).
    pub fn new(cfg: &Config, cwd: &Path) -> Option<Hindsight> {
        let hindsight = cfg.memory_opts.hindsight.clone();
        let base = hindsight.api_url.trim().trim_end_matches('/').to_string();
        if base.is_empty() {
            return None;
        }
        let (bank, tags) = target(&hindsight, cwd);
        Some(Hindsight {
            http: reqwest::Client::new(),
            base,
            token: hindsight.api_token.clone().map(|t| t.trim().to_string()).filter(|t| !t.is_empty()),
            bank,
            tags,
            cfg: hindsight,
            ensured: AtomicBool::new(false),
            seeded: AtomicBool::new(false),
            last_retained: AtomicU32::new(0),
        })
    }

    /// Whether a retain may drop the project tag: `hindsight.global_scope` on a shared bank.
    pub fn global_retain_available(&self) -> bool {
        self.cfg.global_scope && self.cfg.scoping != Scoping::PerProject
    }

    /// Stores one explicit note (the `memory` tool's `scope: global`), without the project tag when global.
    /// Errors when global scope is requested but unavailable, like OMP's `assertGlobalRetainAvailable`.
    pub async fn retain_note(&self, text: &str, global: bool) -> Result<()> {
        let tags = if global {
            if !self.global_retain_available() {
                bail!("Hindsight global scope requires hindsight.global_scope and global or per-project-tagged scoping.");
            }
            Vec::new()
        } else {
            self.tags.clone()
        };
        self.ensure_bank().await;
        let body = json!({ "items": [{ "content": text, "context": "greencode", "tags": tags }], "async": true });
        self.call(reqwest::Method::POST, &format!("/v1/default/banks/{}/memories", self.bank), Some(body))
            .await
            .map(|_| ())
    }

    /// PUTs the bank; the server accepts it repeatedly, so one process only sends it once.
    async fn ensure_bank(&self) {
        if self.ensured.load(Ordering::Relaxed) {
            return;
        }
        let body = json!({ "name": self.bank });
        match self
            .call(reqwest::Method::PUT, &format!("/v1/default/banks/{}", self.bank), Some(body))
            .await
        {
            Ok(_) => {
                self.ensured.store(true, Ordering::Relaxed);
            }
            // Keep the failure visible but do not block the turn: the next call reports it again.
            Err(e) => eprintln!("Hindsight: cannot create bank {}: {e:#}", self.bank),
        }
    }

    async fn call(&self, method: reqwest::Method, path: &str, body: Option<Value>) -> Result<Value> {
        let url = format!("{}{path}", self.base);
        let mut request = self.http.request(method, &url).timeout(Duration::from_secs(30));
        if let Some(token) = &self.token {
            request = request.bearer_auth(token);
        }
        if let Some(body) = body {
            request = request.json(&body);
        }
        let reply = request.send().await.with_context(|| format!("Hindsight request to {url} failed"))?;
        let status = reply.status();
        let text = reply.text().await.unwrap_or_default();
        let value: Value = if text.trim().is_empty() {
            Value::Object(Default::default())
        } else {
            serde_json::from_str(&text).unwrap_or(Value::String(text.clone()))
        };
        if !status.is_success() {
            let detail = value
                .get("detail")
                .or_else(|| value.get("message"))
                .map(|v| v.as_str().map(str::to_string).unwrap_or_else(|| v.to_string()))
                .unwrap_or_else(|| text.clone());
            bail!("Hindsight {path} failed ({}): {}", status.as_u16(), detail.trim());
        }
        Ok(value)
    }

    /// Existing mental models, paging until a short page arrives.
    async fn list_mental_models(&self, detail: &str) -> Result<Vec<Value>> {
        let mut items = Vec::new();
        loop {
            let body = self
                .call(
                    reqwest::Method::GET,
                    &format!(
                        "/v1/default/banks/{}/mental-models?detail={detail}&limit={PAGE}&offset={}",
                        self.bank,
                        items.len()
                    ),
                    None,
                )
                .await?;
            let page = body["items"].as_array().cloned().unwrap_or_default();
            let total = body["total"].as_u64().map(|total| total as usize);
            let done = page.len() < PAGE || total.is_some_and(|total| items.len() + page.len() >= total);
            items.extend(page);
            if done {
                return Ok(items);
            }
        }
    }

    /// Creates the built-in mental models that are absent from the bank; existing ones are never touched.
    async fn seed_mental_models(&self) {
        if self.seeded.swap(true, Ordering::Relaxed) {
            return;
        }
        let existing = match self.list_mental_models("metadata").await {
            Ok(items) => items,
            Err(e) => {
                eprintln!("Hindsight: cannot list mental models for seeding: {e:#}");
                return;
            }
        };
        let slug = self.tags.first().map(|tag| tag.trim_start_matches(PROJECT_TAG)).unwrap_or("");
        for seed in SEEDS {
            if seed.project_only && self.cfg.scoping == Scoping::Global {
                continue;
            }
            let tagged = seed.project_tagged && self.cfg.scoping == Scoping::PerProjectTagged && !slug.is_empty();
            let id = if tagged { format!("{}-{slug}", seed.id) } else { seed.id.to_string() };
            let tags = if tagged { self.tags.clone() } else { Vec::new() };
            let present = existing.iter().any(|model| model["id"].as_str() == Some(id.as_str()));
            if present {
                continue;
            }
            let body = json!({
                "id": id,
                "name": seed.name,
                "source_query": seed.query,
                "tags": if tags.is_empty() { Value::Null } else { json!(tags) },
                "max_tokens": seed.max_tokens,
                "trigger": { "mode": "delta", "refresh_after_consolidation": true },
            });
            if let Err(e) = self
                .call(reqwest::Method::POST, &format!("/v1/default/banks/{}/mental-models", self.bank), Some(body))
                .await
            {
                eprintln!("Hindsight: cannot seed mental model {id}: {e:#}");
            }
        }
    }

    /// The `<mental_models>` block from the bank's curated summaries, within the render budget.
    async fn mental_model_block(&self) -> Result<String> {
        let models: Vec<Value> = self
            .list_mental_models("content")
            .await?
            .into_iter()
            .filter(|model| {
                let content = model["content"].as_str().unwrap_or("").trim();
                if content.is_empty() {
                    return false;
                }
                // A tagged model only applies when its tags intersect this project's tags.
                let tags = model["tags"].as_array().filter(|tags| !tags.is_empty());
                match tags {
                    Some(tags) if !self.tags.is_empty() => tags
                        .iter()
                        .filter_map(Value::as_str)
                        .any(|tag| self.tags.iter().any(|wanted| wanted == tag)),
                    _ => true,
                }
            })
            .collect();
        if models.is_empty() {
            return Ok(String::new());
        }
        Ok(render_models(&models, MENTAL_MODELS_CHARS))
    }
}

/// Renders `<mental_models>`, giving each model an equal share of `budget` and truncating long ones.
fn render_models(models: &[Value], budget: usize) -> String {
    if models.is_empty() || budget < 64 {
        return String::new();
    }
    let share = (budget - 64) / models.len();
    let mut truncated = false;
    let mut out = String::from("<mental_models>");
    for model in models {
        let name = model["name"].as_str().unwrap_or("");
        let content = model["content"].as_str().unwrap_or("").trim();
        let room = share.saturating_sub(name.len() + 4).max(8);
        let take: String = content.chars().take(room).collect();
        truncated |= take.chars().count() < content.chars().count();
        out.push_str(&format!("\n\n# {name}\n{take}"));
    }
    if truncated {
        out.push_str(TRUNCATED);
    }
    out.push_str("\n</mental_models>");
    out
}

impl Backend for Hindsight {
    fn instructions<'a>(&'a self) -> BoxFut<'a, Result<String>> {
        Box::pin(async move {
            if !self.cfg.mental_models_enabled {
                return Ok(String::new());
            }
            self.ensure_bank().await;
            if self.cfg.mental_model_auto_seed {
                self.seed_mental_models().await;
            }
            self.mental_model_block().await
        })
    }

    fn recall<'a>(&'a self, query: &'a str, limit: usize) -> BoxFut<'a, Result<Vec<Hit>>> {
        Box::pin(async move {
            if !self.cfg.auto_recall {
                return Ok(Vec::new());
            }
            let mut body = json!({
                "query": query,
                "types": RECALL_TYPES,
                "max_tokens": RECALL_MAX_TOKENS,
                "budget": "mid",
            });
            if !self.tags.is_empty() {
                body["tags"] = json!(self.tags);
                body["tags_match"] = json!("any");
            }
            let reply = self
                .call(reqwest::Method::POST, &format!("/v1/default/banks/{}/memories/recall", self.bank), Some(body))
                .await?;
            Ok(reply["results"]
                .as_array()
                .map(|results| {
                    results
                        .iter()
                        .filter_map(|result| {
                            let text = result["text"].as_str()?.trim();
                            (!text.is_empty()).then(|| Hit {
                                text: text.to_string(),
                                source: result["type"].as_str().map(str::to_string),
                                score: result["score"].as_f64(),
                            })
                        })
                        .take(limit)
                        .collect()
                })
                .unwrap_or_default())
        })
    }

    fn retain<'a>(&'a self, session: &'a str, text: &'a str) -> BoxFut<'a, Result<()>> {
        Box::pin(async move {
            self.ensure_bank().await;
            let (document, content) = match self.cfg.retain_mode {
                RetainMode::FullSession => (session.to_string(), text.to_string()),
                RetainMode::LastTurn => {
                    let chunk = tail_turns(text, self.cfg.retain_every_n_turns);
                    let stamp = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_millis())
                        .unwrap_or_default();
                    (format!("{session}-{stamp}"), chunk)
                }
            };
            let body = json!({
                "items": [{
                    "content": content,
                    "context": "greencode",
                    "document_id": document,
                    "metadata": { "session_id": session },
                    "tags": self.tags,
                }],
                "async": true,
            });
            self.call(reqwest::Method::POST, &format!("/v1/default/banks/{}/memories", self.bank), Some(body))
                .await?;
            Ok(())
        })
    }

    fn retain_due(&self, turns: u32) -> bool {
        let every = self.cfg.retain_every_n_turns.max(1);
        if !self.cfg.auto_retain || turns.saturating_sub(self.last_retained.load(Ordering::Relaxed)) < every {
            return false;
        }
        self.last_retained.store(turns, Ordering::Relaxed);
        true
    }
}

/// Keeps the transcript's last `turns` user turns (the chunk a `last-turn` retain sends).
fn tail_turns(transcript: &str, turns: u32) -> String {
    const MARK: &str = "\n[role: user]\n";
    let blocks: Vec<&str> = transcript.split(MARK).collect();
    let keep = turns.max(1) as usize;
    if blocks.len() <= keep {
        return transcript.to_string();
    }
    format!("[role: user]\n{}", blocks[blocks.len() - keep..].join(MARK))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::memory::MemoryBackend;
    use crate::memory::{recall_block, transcript, Backend, RECALL_LIMIT};
    use crate::{Block, Message, Role};
    use serde_json::Value;
    use std::sync::{Arc, Mutex};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    struct Request {
        method: String,
        path: String,
        body: Value,
        authorization: Option<String>,
    }

    /// One connection per reply, taken in order; every received request is recorded.
    async fn serve(replies: Vec<(u16, String)>) -> (String, Arc<Mutex<Vec<Request>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let seen = Arc::new(Mutex::new(Vec::new()));
        let recorded = seen.clone();
        tokio::spawn(async move {
            for (status, body) in replies {
                let Ok((mut stream, _)) = listener.accept().await else { return };
                let mut bytes = Vec::new();
                let mut chunk = [0u8; 4096];
                loop {
                    let Ok(n) = stream.read(&mut chunk).await else { return };
                    bytes.extend_from_slice(&chunk[..n]);
                    let text = String::from_utf8_lossy(&bytes).to_string();
                    if let Some(head) = text.find("\r\n\r\n") {
                        let length = text[..head]
                            .lines()
                            .find_map(|line| {
                                let lower = line.to_ascii_lowercase();
                                lower.strip_prefix("content-length: ").and_then(|v| v.trim().parse::<usize>().ok())
                            })
                            .unwrap_or(0);
                        if bytes.len() >= head + 4 + length {
                            let request_line = text.lines().next().unwrap_or("").to_string();
                            let mut parts = request_line.split(' ');
                            let method = parts.next().unwrap_or("").to_string();
                            let path = parts.next().unwrap_or("").to_string();
                            let authorization = text[..head].lines().find_map(|line| {
                                let (name, value) = line.split_once(':')?;
                                name.eq_ignore_ascii_case("authorization").then(|| value.trim().to_string())
                            });
                            let body = serde_json::from_slice(&bytes[head + 4..]).unwrap_or(Value::Null);
                            recorded.lock().unwrap().push(Request { method, path, body, authorization });
                            break;
                        }
                    }
                }
                let _ = stream
                    .write_all(
                        format!(
                            "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                            body.len()
                        )
                        .as_bytes(),
                    )
                    .await;
            }
        });
        (url, seen)
    }

    fn cfg(api: &str) -> Config {
        let mut cfg = Config::default();
        cfg.memory_opts.memory.backend = MemoryBackend::Hindsight;
        cfg.memory_opts.hindsight.api_url = api.into();
        cfg.memory_opts.hindsight.api_token = Some("secret".into());
        cfg.memory_opts.hindsight.retain_every_n_turns = 2;
        cfg
    }

    fn hindsight(cfg: &Config) -> Hindsight {
        Hindsight::new(cfg, Path::new(".")).unwrap()
    }

    fn assistant(text: &str) -> Message {
        Message { role: Role::Assistant, content: vec![Block::Text { text: text.into() }] }
    }

    #[test]
    fn bank_follows_scoping_and_override() {
        let dir = std::env::temp_dir().join(format!("hindsight-bank-{}", std::process::id()));
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        let slug = project_slug(&dir);
        let mut h = HindsightCfg::default();
        assert_eq!(h.scoping, Scoping::PerProjectTagged);
        h.scoping = Scoping::Global;
        assert_eq!(target(&h, &dir), ("greencode".to_string(), Vec::new()));
        h.scoping = Scoping::PerProject;
        assert_eq!(target(&h, &dir), (format!("greencode-{slug}"), Vec::new()));
        h.scoping = Scoping::PerProjectTagged;
        assert_eq!(target(&h, &dir), ("greencode".to_string(), vec![format!("project:{slug}")]));
        h.bank_id = Some("team".into());
        assert_eq!(target(&h, &dir).0, "team");
        std::fs::remove_dir_all(dir).ok();
    }

    #[tokio::test]
    async fn retain_sends_document_and_turns_gate_it() {
        let (url, seen) = serve(vec![(200, "{}".into()), (200, "{}".into())]).await;
        let mut cfg = cfg(&url);
        cfg.memory_opts.hindsight.scoping = Scoping::PerProject;
        let h = hindsight(&cfg);
        assert!(!h.retain_due(1));
        assert!(h.retain_due(2));
        assert!(!h.retain_due(3));
        let text = "[role: user]\nhello\n[user:end]\n[role: assistant]\nhi\n[assistant:end]";
        h.retain("session-1", text).await.unwrap();
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 2);
        assert_eq!(seen[0].method, "PUT");
        assert_eq!(
            seen[0].path,
            format!("/v1/default/banks/greencode-{}", project_slug(Path::new(".")))
        );
        assert_eq!(seen[1].method, "POST");
        assert!(seen[1].path.ends_with("/memories"));
        let item = &seen[1].body["items"][0];
        assert_eq!(item["document_id"], "session-1");
        assert_eq!(item["content"], text);
        assert_eq!(seen[1].authorization.as_deref(), Some("Bearer secret"));
    }

    #[tokio::test]
    async fn recall_posts_the_query_and_maps_results() {
        let reply = json!({ "results": [
            { "text": "use pnpm", "type": "world", "score": 0.3 },
            { "text": "", "type": "world" },
        ]});
        let (url, seen) = serve(vec![(200, reply.to_string())]).await;
        let mut cfg = cfg(&url);
        cfg.memory_opts.hindsight.scoping = Scoping::PerProjectTagged;
        let h = hindsight(&cfg);
        let hits = h.recall("where is the lockfile?", RECALL_LIMIT).await.unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].text, "use pnpm");
        assert_eq!(hits[0].source.as_deref(), Some("world"));
        let seen = seen.lock().unwrap();
        assert_eq!(seen[0].body["query"], "where is the lockfile?");
        assert_eq!(seen[0].body["types"], json!(["world", "experience"]));
        assert_eq!(seen[0].body["tags_match"], "any");
        assert_eq!(
            seen[0].body["tags"],
            json!([format!("project:{}", project_slug(Path::new(".")))])
        );
    }

    #[tokio::test]
    async fn auto_recall_off_skips_the_request() {
        let (url, seen) = serve(vec![]).await;
        let mut cfg = cfg(&url);
        cfg.memory_opts.hindsight.auto_recall = false;
        let h = hindsight(&cfg);
        assert!(h.recall("q", RECALL_LIMIT).await.unwrap().is_empty());
        assert!(seen.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn mental_models_are_seeded_then_loaded_within_budget() {
        let metadata = json!({ "items": [
            { "id": "project-conventions", "name": "Project Conventions" },
            { "id": "project-decisions", "name": "Project Decisions" },
        ], "total": 2 });
        let content = json!({ "items": [
            { "id": "project-conventions", "name": "Project Conventions", "content": "Use tabs." },
            { "id": "user-preferences", "name": "User Preferences", "content": "Be terse." },
        ], "total": 2 });
        let (url, seen) = serve(vec![
            (200, "{}".into()),
            (200, metadata.to_string()),
            (200, "{}".into()),
            (200, content.to_string()),
        ])
        .await;
        let mut cfg = cfg(&url);
        cfg.memory_opts.hindsight.scoping = Scoping::Global;
        cfg.memory_opts.hindsight.mental_models_enabled = true;
        let h = hindsight(&cfg);
        let block = h.instructions().await.unwrap();
        assert!(block.starts_with("<mental_models>") && block.ends_with("</mental_models>"));
        assert!(block.contains("# Project Conventions\nUse tabs."));
        assert!(block.contains("# User Preferences\nBe terse."));
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 4);
        assert_eq!(seen[1].method, "GET");
        assert!(seen[1].path.contains("detail=metadata"));
        assert_eq!(seen[2].method, "POST");
        assert_eq!(seen[2].body["id"], "user-preferences");
        assert!(seen[2].body["tags"].is_null());
        assert!(seen[3].path.contains("detail=content"));
    }

    #[tokio::test]
    async fn mental_models_off_makes_no_request() {
        let (url, seen) = serve(vec![]).await;
        let mut cfg = cfg(&url);
        cfg.memory_opts.hindsight.mental_models_enabled = false;
        let h = hindsight(&cfg);
        assert!(h.instructions().await.unwrap().is_empty());
        assert!(seen.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn global_retain_requires_the_setting_and_a_shared_bank() {
        let (url, seen) = serve(vec![(200, "{}".into()), (200, "{}".into())]).await;
        let mut cfg = cfg(&url);
        let h = hindsight(&cfg);
        let error = h.retain_note("ship on fridays", true).await.unwrap_err().to_string();
        assert!(error.contains("global scope"), "{error}");
        assert!(seen.lock().unwrap().is_empty());
        cfg.memory_opts.hindsight.global_scope = true;
        cfg.memory_opts.hindsight.scoping = Scoping::PerProjectTagged;
        let h = hindsight(&cfg);
        assert!(h.global_retain_available());
        h.retain_note("ship on fridays", true).await.unwrap();
        let seen = seen.lock().unwrap();
        assert_eq!(seen[1].body["items"][0]["tags"], json!([]));
        assert_eq!(seen[1].body["items"][0]["content"], "ship on fridays");
    }

    #[tokio::test]
    async fn last_turn_retain_sends_only_the_newest_turn() {
        let (url, seen) = serve(vec![(200, "{}".into()), (200, "{}".into())]).await;
        let mut cfg = cfg(&url);
        cfg.memory_opts.hindsight.retain_mode = RetainMode::LastTurn;
        cfg.memory_opts.hindsight.retain_every_n_turns = 1;
        let h = hindsight(&cfg);
        let text = "[role: user]\nfirst\n[user:end]\n\n[role: assistant]\nok\n[assistant:end]\n\n[role: user]\nsecond\n[user:end]";
        h.retain("session-2", text).await.unwrap();
        let seen = seen.lock().unwrap();
        let item = &seen[1].body["items"][0];
        assert_eq!(item["content"], "[role: user]\nsecond\n[user:end]");
        assert!(
            item["document_id"].as_str().unwrap().starts_with("session-2-"),
            "{}",
            item["document_id"]
        );
    }

    #[test]
    fn render_budget_truncates_models() {
        let long = "x".repeat(5000);
        let models = vec![json!({ "name": "A", "content": long }), json!({ "name": "B", "content": "short" })];
        let block = render_models(&models, 400);
        assert!(block.starts_with("<mental_models>") && block.ends_with("</mental_models>"));
        assert!(block.contains(TRUNCATED));
        assert!(block.contains("# B\nshort"));
        assert!(block.len() < 500);
        assert!(render_models(&[], 100).is_empty());
    }

    #[test]
    fn transcript_skips_injected_blocks_and_empty_messages() {
        let messages = vec![
            Message::user("<memories>\n- old\n\nPREAMBLE\n</memories>\n\nfix the build"),
            assistant("done"),
            Message::user("   "),
        ];
        let text = transcript(&messages).unwrap();
        assert!(!text.contains("<memories>"));
        assert!(text.contains("fix the build"));
        assert!(text.contains("[role: assistant]"));
        let block = recall_block(&[crate::memory::Hit { text: "use pnpm".into(), source: None, score: None }]);
        assert!(block.contains("- use pnpm"));
    }
}
