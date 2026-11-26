//! Mnemopi: the local SQLite memory bank. One row per retained fact; FTS5 gives keyword recall and a
//! deterministic hashed embedding (or a configured OpenAI-compatible embedder) gives semantic recall.
//! Entity links, a recall cache, a judge filter and LLM extraction are the `mnemopi.*` settings.
use super::{Hit, Backend};
use crate::config::{
    mnemopi::{LlmMode, MnemopiCfg, Relevance, Scoping},
    Config,
};
use crate::BoxFut;
use anyhow::{anyhow, bail, Context as _, Result};
use rusqlite::{params, Connection, OptionalExtension};
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};

/// Largest single memory entry; longer turns are split on blank lines.
const CHUNK: usize = 1_200;
/// Rows re-embedded per open when the embedding model changed.
const REEMBED_PER_OPEN: usize = 16;
/// Reciprocal-rank-fusion constant.
const RRF: f64 = 60.0;

pub struct Mnemopi {
    cfg: Config,
    db: PathBuf,
    bank: String,
    project: String,
}

/// Recall results per database, then per `(bank, project, query, limit)`; a write clears its database's entries.
static CACHE: LazyLock<Mutex<std::collections::HashMap<PathBuf, std::collections::HashMap<String, Vec<Hit>>>>> =
    LazyLock::new(Default::default);

impl Mnemopi {
    pub fn new(cfg: &Config, cwd: &Path) -> Self {
        let m = &cfg.memory_opts.mnemopi;
        let (bank, project) = m.store_for(cwd);
        Self { cfg: cfg.clone(), db: m.db_file(), bank, project }
    }

    fn m(&self) -> &MnemopiCfg {
        &self.cfg.memory_opts.mnemopi
    }

    fn open(&self) -> Result<Connection> {
        if let Some(dir) = self.db.parent() {
            std::fs::create_dir_all(dir).with_context(|| dir.display().to_string())?;
        }
        let db = Connection::open(&self.db).with_context(|| format!("mnemopi DB {}", self.db.display()))?;
        db.busy_timeout(std::time::Duration::from_secs(5))?;
        db.execute_batch(
            "PRAGMA journal_mode = WAL;
             CREATE TABLE IF NOT EXISTS memories (
                 id INTEGER PRIMARY KEY, bank TEXT NOT NULL, scope TEXT NOT NULL, project TEXT NOT NULL,
                 session TEXT NOT NULL DEFAULT '', hash TEXT NOT NULL, text TEXT NOT NULL,
                 created_at INTEGER NOT NULL, embedding BLOB, embed_sig TEXT
             );
             CREATE UNIQUE INDEX IF NOT EXISTS memories_unique ON memories(bank, project, hash);
             CREATE VIRTUAL TABLE IF NOT EXISTS memories_fts USING fts5(text, tokenize = 'unicode61');
             CREATE TABLE IF NOT EXISTS links (src INTEGER NOT NULL, dst INTEGER NOT NULL, kind TEXT NOT NULL,
                 PRIMARY KEY (src, dst, kind));",
        )?;
        Ok(db)
    }

    /// Rows kept when the embedding model changed; a row missing its vector still recalls by keyword.
    async fn reembed(&self) -> Result<()> {
        let db = self.open()?;
        let prefix = self.sig_prefix();
        let rows: Vec<(i64, String)> = {
            let mut stmt = db.prepare(
                "SELECT id, text FROM memories WHERE bank = ?1 AND (embed_sig IS NULL OR embed_sig NOT LIKE ?2 || '%')
                 LIMIT ?3",
            )?;
            let rows = stmt.query_map(params![self.bank, prefix, REEMBED_PER_OPEN as i64], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
            rows
        };
        for (id, text) in rows {
            if let Ok((vec, sig)) = self.embed(&text, false).await {
                db.execute(
                    "UPDATE memories SET embedding = ?1, embed_sig = ?2 WHERE id = ?3",
                    params![to_blob(&vec), sig, id],
                )?;
            }
        }
        Ok(())
    }

    /// Prefix every current row's `embed_sig` must carry; rows from another model recall by keyword only.
    fn sig_prefix(&self) -> String {
        match self.m().embedding_api_url.as_deref().map(str::trim).filter(|u| !u.is_empty()) {
            Some(url) => format!("{url}|{}|", self.model_id()),
            None => format!("local|{}|", self.m().embedding_variant.as_str()),
        }
    }

    fn model_id(&self) -> String {
        self.m()
            .embedding_model
            .as_deref()
            .map(str::trim)
            .filter(|m| !m.is_empty())
            .unwrap_or(self.m().embedding_variant.as_str())
            .to_string()
    }

    /// `(bank, project)` filter: a tagged bank keeps every project's rows; global rows (`project = ''`)
    /// are visible everywhere.
    fn visible(&self) -> (String, String) {
        (self.bank.clone(), self.project.clone())
    }

    /// Stores one fact; a duplicate in the same bank and project is ignored.
    async fn add(&self, text: &str, scope: &str, session: &str) -> Result<bool> {
        let text = text.trim();
        if text.is_empty() {
            return Ok(false);
        }
        let db = self.open()?;
        let project = if scope == "global" { "" } else { self.project.as_str() };
        let inserted = db.execute(
            "INSERT OR IGNORE INTO memories (bank, scope, project, session, hash, text, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![self.bank, scope, project, session, hash(text), text, now()],
        )?;
        if inserted == 0 {
            return Ok(false);
        }
        let id = db.last_insert_rowid();
        db.execute("INSERT INTO memories_fts (rowid, text) VALUES (?1, ?2)", params![id, text])?;
        // The connection must not be held across the embedding request: these futures are `Send`.
        drop(db);
        let vector = self.embed(text, false).await.ok();
        let db = self.open()?;
        if let Some((vec, sig)) = vector {
            db.execute(
                "UPDATE memories SET embedding = ?1, embed_sig = ?2 WHERE id = ?3",
                params![to_blob(&vec), sig, id],
            )?;
        }
        CACHE.lock().unwrap().remove(&self.db);
        if self.m().proactive_linking {
            let _ = self.link(&db, id, text);
        }
        Ok(true)
    }

    /// Links a new memory to existing ones in the same bank sharing an entity.
    fn link(&self, db: &Connection, id: i64, text: &str) -> Result<()> {
        for entity in entities(text).into_iter().take(8) {
            let mut stmt = db.prepare(
                "SELECT id FROM memories WHERE bank = ?1 AND project = ?2 AND id != ?3 AND text LIKE ?4
                 ORDER BY created_at DESC LIMIT 4",
            )?;
            let ids: Vec<i64> = stmt
                .query_map(params![self.bank, self.project, id, format!("%{entity}%")], |r| r.get(0))?
                .collect::<rusqlite::Result<_>>()?;
            for other in ids {
                db.execute(
                    "INSERT OR IGNORE INTO links (src, dst, kind) VALUES (?1, ?2, 'entity'), (?2, ?1, 'entity')",
                    params![id, other],
                )?;
            }
        }
        Ok(())
    }

    async fn forget_matching(&self, old: &str, replace: Option<&str>) -> Result<String> {
        let db = self.open()?;
        let rows: Vec<(i64, String)> = {
            let mut stmt = db.prepare(
                "SELECT id, text FROM memories WHERE bank = ?1 AND (project = ?2 OR project = '') AND text LIKE ?3",
            )?;
            let rows = stmt.query_map(params![self.bank, self.project, format!("%{old}%")], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
            rows
        };
        let id = match rows.as_slice() {
            [(id, _)] => *id,
            [] => bail!("no memory contains `{old}`"),
            rows => bail!("`{old}` matches {} memories; be more specific", rows.len()),
        };
        match replace {
            Some(new) => {
                db.execute("UPDATE memories SET text = ?1, hash = ?2, embedding = NULL, embed_sig = NULL WHERE id = ?3",
                    params![new.trim(), hash(new.trim()), id])?;
                db.execute("UPDATE memories_fts SET text = ?1 WHERE rowid = ?2", params![new.trim(), id])?;
            }
            None => {
                db.execute("DELETE FROM memories WHERE id = ?1", params![id])?;
                db.execute("DELETE FROM memories_fts WHERE rowid = ?1", params![id])?;
                db.execute("DELETE FROM links WHERE src = ?1 OR dst = ?1", params![id])?;
            }
        }
        CACHE.lock().unwrap().remove(&self.db);
        Ok(if replace.is_some() { "replaced".into() } else { "removed".into() })
    }

    /// Keyword and vector voices, plus graph/fact/temporal when polyphonic, fused by RRF.
    async fn search(&self, query: &str, limit: usize) -> Result<Vec<Hit>> {
        let key = cache_key(&self.bank, &self.project, query, limit);
        if self.m().enhanced_recall {
            if let Some(hits) = CACHE.lock().unwrap().get(&self.db).and_then(|m| m.get(&key)).cloned() {
                return Ok(hits);
            }
        }
        // A connection borrow must not cross an await: the futures here are `Send`.
        let _ = self.reembed().await;
        let db = self.open()?;
        let keyword = self.ids(&db, &fts_ids(&db, query, 24)?)?;
        let mut voices: Vec<(&'static str, Vec<i64>)> = vec![("keyword", keyword.clone())];
        if !self.m().no_embeddings {
            if let Ok((query_vec, _)) = self.embed(query, true).await {
                voices.push(("vector", self.vector_ids(&db, &query_vec, 24)?));
            }
        }
        if self.m().polyphonic_recall {
            voices.push(("linked", self.linked_ids(&db, &keyword)?));
            voices.push(("fact", self.ids(&db, &facts_ids(&db, query, 12)?)?));
            voices.push(("recent", self.ids(&db, &recent_ids(&db, 6)?)?));
        }
        let mut fused: std::collections::BTreeMap<i64, (f64, &'static str)> = Default::default();
        for (name, ids) in &voices {
            for (rank, id) in ids.iter().enumerate() {
                let entry = fused.entry(*id).or_insert((0.0, name));
                entry.0 += 1.0 / (RRF + rank as f64);
            }
        }
        let mut ranked: Vec<(i64, f64, &'static str)> =
            fused.into_iter().map(|(id, (score, name))| (id, score, name)).collect();
        ranked.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
        ranked.truncate(limit);
        let mut hits = Vec::new();
        for (id, score, source) in ranked {
            if let Some(text) = db
                .query_row("SELECT text FROM memories WHERE id = ?1", params![id], |r| r.get::<_, String>(0))
                .optional()?
            {
                hits.push(Hit { text, source: Some(source.into()), score: Some(score) });
            }
        }
        if self.m().enhanced_recall {
            CACHE.lock().unwrap().entry(self.db.clone()).or_default().insert(key, hits.clone());
        }
        Ok(hits)
    }

    /// Drops ids whose row belongs to another project of a shared bank.
    fn ids(&self, db: &Connection, ids: &[i64]) -> Result<Vec<i64>> {
        let (bank, project) = self.visible();
        let mut out = Vec::new();
        for id in ids {
            let visible: Option<String> = db
                .query_row(
                    "SELECT project FROM memories WHERE id = ?1 AND bank = ?2",
                    params![id, bank],
                    |r| r.get(0),
                )
                .optional()?;
            if visible.is_some_and(|row| row.is_empty() || row == project) {
                out.push(*id);
            }
        }
        Ok(out)
    }

    fn vector_ids(&self, db: &Connection, query: &[f32], limit: usize) -> Result<Vec<i64>> {
        let prefix = self.sig_prefix();
        let mut stmt = db.prepare(
            "SELECT id, embedding FROM memories WHERE bank = ?1 AND embedding IS NOT NULL AND embed_sig LIKE ?2 || '%'",
        )?;
        let mut scored: Vec<(f64, i64)> = stmt
            .query_map(params![self.bank, prefix], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, Vec<u8>>(1)?)))?
            .collect::<rusqlite::Result<Vec<(i64, Vec<u8>)>>>()?
            .into_iter()
            .filter_map(|(id, blob)| {
                let vec = from_blob(&blob);
                (vec.len() == query.len()).then(|| (cosine(query, &vec), id))
            })
            .collect();
        scored.sort_by(|a, b| b.0.total_cmp(&a.0).then(a.1.cmp(&b.1)));
        Ok(scored.into_iter().filter(|(s, _)| *s > 0.05).take(limit).map(|(_, id)| id).collect())
    }

    fn linked_ids(&self, db: &Connection, seeds: &[i64]) -> Result<Vec<i64>> {
        let mut out = Vec::new();
        for seed in seeds.iter().take(5) {
            let mut stmt = db.prepare("SELECT dst FROM links WHERE src = ?1 LIMIT 8")?;
            out.extend(stmt.query_map(params![seed], |r| r.get::<_, i64>(0))?.collect::<rusqlite::Result<Vec<_>>>()?);
        }
        out.sort_unstable();
        out.dedup();
        Ok(out)
    }

    /// The local hashed embedding, or the configured OpenAI-compatible endpoint when `embedding_api_url` is set.
    async fn embed(&self, text: &str, query: bool) -> Result<(Vec<f32>, String)> {
        if self.m().no_embeddings {
            bail!("embeddings are off");
        }
        let Some(api) = self.m().embedding_api_url.as_deref().map(str::trim).filter(|u| !u.is_empty()) else {
            let variant = self.m().embedding_variant;
            return Ok((hashed_embedding(text, variant.dim()), format!("local|{}|{}|hashed", variant.as_str(), variant.dim())));
        };
        let sent = match (query, self.m().embedding_query_prefix.as_deref().filter(|p| !p.is_empty())) {
            (true, Some(prefix)) => format!("{prefix}{text}"),
            _ => text.to_string(),
        };
        let model = self.model_id();
        let mut body = serde_json::json!({"model": model, "input": sent});
        if self.m().embedding_input_type {
            body["input_type"] = serde_json::Value::String(if query { "query" } else { "passage" }.into());
        }
        let mut req = reqwest::Client::new()
            .post(api)
            .timeout(std::time::Duration::from_secs(30))
            .json(&body);
        if let Some(key) = self.m().embedding_api_key.as_deref().filter(|k| !k.is_empty()) {
            req = req.bearer_auth(key);
        }
        let reply = req.send().await.with_context(|| format!("embedding request to {api}"))?;
        let status = reply.status();
        let text = reply.text().await?;
        anyhow::ensure!(
            status.is_success(),
            "embedding endpoint returned {status}: {}",
            text.chars().take(200).collect::<String>()
        );
        let vec = parse_embedding(&text)?;
        let sig = format!("{api}|{model}|{}", vec.len());
        Ok((vec, sig))
    }

    async fn recall_with_judge(&self, query: &str, limit: usize) -> Result<Vec<Hit>> {
        if !self.m().auto_recall {
            return Ok(vec![]);
        }
        let hits = self.search(query, limit).await?;
        if hits.is_empty() || self.m().recall_relevance == Relevance::Off {
            return Ok(hits);
        }
        match self.judge_spec() {
            // `auto` keeps recall when no judge model is configured; `on` would otherwise drop every hit.
            None => Ok(hits),
            Some(spec) => self.judge(query, hits, &spec).await,
        }
    }

    /// The judge role, else the learning/smol roles native users already configure.
    fn judge_spec(&self) -> Option<String> {
        ["judge", "learning", "smol", "tiny"].iter().find_map(|role| self.cfg.model_roles.get(*role).cloned())
    }

    async fn judge(&self, query: &str, hits: Vec<Hit>, spec: &str) -> Result<Vec<Hit>> {
        let list: Vec<String> = hits.iter().enumerate().map(|(i, h)| format!("{}. {}", i + 1, h.text.replace('\n', " "))).collect();
        let prompt = format!(
            "Task:\n{}\n\nCandidate memories:\n{}\n\nReply with the numbers of the memories that help this task, comma-separated, or `none`.",
            clip(query, 1_500),
            clip(&list.join("\n"), 4_000)
        );
        let reply = crate::textgen::generate(
            &self.cfg,
            spec,
            "You judge whether a remembered fact helps a coding task. Reply with numbers only.",
            &prompt,
            40,
        )
        .await?;
        let keep = parse_indices(&reply, hits.len());
        Ok(hits.into_iter().enumerate().filter(|(i, _)| keep.contains(i)).map(|(_, h)| h).collect())
    }

    /// Durable facts from a turn; `None` = no extraction model, so the caller stores chunks unchanged.
    async fn extract(&self, text: &str) -> Result<Option<Vec<String>>> {
        // The remote mode registers its endpoint as a `mnemopi` provider on a private copy of the config.
        let mut cfg = std::borrow::Cow::Borrowed(&self.cfg);
        let spec = match self.m().llm_mode {
            LlmMode::None => return Ok(None),
            LlmMode::Smol => match ["tiny", "smol"].iter().find_map(|role| self.cfg.model_roles.get(*role).cloned()) {
                Some(spec) => spec,
                None => return Ok(None),
            },
            LlmMode::Remote => {
                let base = self.m().llm_base_url.clone().unwrap_or_default();
                let model = self.m().llm_model.clone().unwrap_or_default();
                if base.trim().is_empty() || model.trim().is_empty() {
                    return Ok(None);
                }
                cfg.to_mut().providers.insert(
                    "mnemopi".into(),
                    crate::config::ProviderCfg {
                        kind: "openai".into(),
                        base_url: base,
                        api_key: self.m().llm_api_key.clone(),
                        ..Default::default()
                    },
                );
                format!("mnemopi/{model}")
            }
        };
        let prompt = format!(
            "Extract durable facts a developer would want remembered from this conversation. Reply with at most five one-line facts, no bullets or numbering, or NONE.\n\n{}",
            clip(text, 8_000)
        );
        let reply = crate::textgen::generate(
            &cfg,
            &spec,
            "You store only durable project facts. Never include secrets, credentials or personal data.",
            &prompt,
            300,
        )
        .await?;
        let facts: Vec<String> = reply
            .lines()
            .map(|line| line.trim().trim_start_matches(|c: char| c == '-' || c == '*' || c == '.' || c.is_ascii_digit()).trim())
            .filter(|line| line.len() >= 8 && !line.eq_ignore_ascii_case("none"))
            .map(|line| clip(line, CHUNK))
            .collect();
        Ok(Some(facts))
    }

    /// `memory` tool operations against this bank.
    pub async fn apply(&self, input: &serde_json::Value) -> Result<String> {
        let field = |name: &str| -> Result<String> {
            input[name]
                .as_str()
                .map(str::trim)
                .filter(|t| !t.is_empty())
                .map(str::to_string)
                .ok_or_else(|| anyhow!("`{name}` is required"))
        };
        let scope = match input["scope"].as_str().unwrap_or("project") {
            "global" => "global",
            "project" => "project",
            other => bail!("scope must be global or project, got `{other}`"),
        };
        match input["action"].as_str().unwrap_or_default() {
            "add" => Ok(if self.add(&field("text")?, scope, "").await? {
                "saved to Mnemopi; later sessions recall it".into()
            } else {
                "already in Mnemopi".into()
            }),
            "replace" => self.forget_matching(&field("old")?, Some(&field("text")?)).await,
            "remove" => self.forget_matching(&field("old")?, None).await,
            action => bail!("unknown action `{action}`"),
        }
    }

    /// Stores the turn as extracted facts when an LLM is configured, else as chunks of the transcript.
    async fn retain_turn(&self, session: &str, text: &str) -> Result<()> {
        if !self.m().auto_retain || text.trim().len() < 10 {
            return Ok(());
        }
        let pieces = match self.extract(text).await {
            Ok(Some(facts)) => facts,
            Ok(None) => chunks(text),
            // An extraction model that fails must not lose the turn.
            Err(_) => chunks(text),
        };
        let scope = if self.m().scoping == Scoping::Global { "global" } else { "project" };
        for piece in pieces {
            self.add(&piece, scope, session).await?;
        }
        Ok(())
    }
}

impl Backend for Mnemopi {
    fn instructions<'a>(&'a self) -> BoxFut<'a, Result<String>> {
        // Recalled memories go into the first user turn, not the system prompt.
        Box::pin(async { Ok(String::new()) })
    }

    fn recall<'a>(&'a self, query: &'a str, limit: usize) -> BoxFut<'a, Result<Vec<Hit>>> {
        Box::pin(async move { self.recall_with_judge(query, limit).await })
    }

    fn retain<'a>(&'a self, session: &'a str, text: &'a str) -> BoxFut<'a, Result<()>> {
        Box::pin(async move { self.retain_turn(session, text).await })
    }

    fn retain_due(&self, _turns: u32) -> bool {
        self.m().auto_retain
    }

    fn tool<'a>(&'a self, input: &'a serde_json::Value) -> Option<BoxFut<'a, Result<String>>> {
        Some(Box::pin(self.apply(input)))
    }
}

/// The `memories` rows for a keyword query, best first.
fn fts_ids(db: &Connection, query: &str, limit: usize) -> Result<Vec<i64>> {
    let Some(expr) = fts_query(query) else { return Ok(vec![]) };
    let mut stmt = db.prepare("SELECT rowid FROM memories_fts WHERE memories_fts MATCH ?1 ORDER BY bm25(memories_fts) LIMIT ?2")?;
    let ids = stmt.query_map(params![expr, limit as i64], |r| r.get(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(ids)
}

/// Rows holding a distinctive query token: the factual voice polyphonic recall adds.
fn facts_ids(db: &Connection, query: &str, limit: usize) -> Result<Vec<i64>> {
    let mut out = Vec::new();
    for token in tokens(query).into_iter().filter(|t| t.chars().count() >= 4).take(3) {
        let mut stmt = db.prepare("SELECT id FROM memories WHERE text LIKE ?1 ORDER BY created_at DESC LIMIT ?2")?;
        out.extend(
            stmt.query_map(params![format!("%{token}%"), limit as i64], |r| r.get::<_, i64>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?,
        );
    }
    out.sort_unstable();
    out.dedup();
    Ok(out)
}

fn recent_ids(db: &Connection, limit: usize) -> Result<Vec<i64>> {
    let mut stmt = db.prepare("SELECT id FROM memories ORDER BY created_at DESC, id DESC LIMIT ?1")?;
    let ids = stmt.query_map(params![limit as i64], |r| r.get(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(ids)
}

/// Mnemopi's recall stop words (`beam/recall.ts`): a shared "the" must not count as a keyword hit.
const RECALL_STOP_WORDS: &[&str] = &[
    "a", "an", "and", "are", "as", "at", "be", "by", "for", "from", "how", "i", "in", "is", "it", "of", "on", "or",
    "that", "the", "this", "to", "was", "what", "when", "where", "who", "with",
];

fn fts_query(query: &str) -> Option<String> {
    let terms: Vec<String> = tokens(query)
        .into_iter()
        .filter(|t| !RECALL_STOP_WORDS.contains(&t.as_str()))
        .take(12)
        .map(|t| format!("\"{}\"", t.replace('"', "")))
        .collect();
    (!terms.is_empty()).then(|| terms.join(" OR "))
}

fn tokens(text: &str) -> Vec<String> {
    let mut out: Vec<String> = text
        .split(|c: char| !c.is_alphanumeric() && c != '_' && c != '.' && c != '/')
        .filter(|t| !t.is_empty())
        .map(str::to_lowercase)
        .collect();
    out.dedup();
    out
}

/// Identifiers, dotted paths and capitalized words: the entities proactive linking joins on.
fn entities(text: &str) -> Vec<String> {
    let stop = ["the", "this", "that", "with", "from", "when", "then", "have", "will", "should"];
    let mut out: Vec<String> = text
        .split(|c: char| c.is_whitespace() || matches!(c, ',' | ';' | ':' | '(' | ')' | '[' | ']' | '"' | '\''))
        .map(|word| word.trim_matches(|c: char| !c.is_alphanumeric() && c != '_' && c != '.' && c != '/'))
        .filter(|word| word.chars().count() >= 4)
        .filter(|word| {
            let shaped = word.contains(['_', '.', '/'])
                || word.chars().any(char::is_uppercase)
                || word.chars().any(|c| c.is_ascii_digit());
            shaped && !stop.contains(&word.to_lowercase().as_str())
        })
        .map(str::to_string)
        .collect();
    out.sort();
    out.dedup();
    out
}

/// Splits a transcript into entries on blank lines, keeping each under [`CHUNK`] characters.
fn chunks(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    for line in text.lines() {
        if line.trim().is_empty() {
            if !current.trim().is_empty() {
                out.push(std::mem::take(&mut current));
            }
            continue;
        }
        if current.chars().count() + line.chars().count() > CHUNK {
            out.push(std::mem::take(&mut current));
        }
        if !current.is_empty() {
            current.push('\n');
        }
        current.push_str(line);
    }
    if !current.trim().is_empty() {
        out.push(current);
    }
    out.into_iter().map(|chunk| chunk.trim().to_string()).filter(|chunk| chunk.len() >= 10).collect()
}

/// Parses `1, 3` / `all` / `none` into zero-based indices.
fn parse_indices(reply: &str, len: usize) -> std::collections::BTreeSet<usize> {
    let lower = reply.to_lowercase();
    if lower.contains("none") {
        return Default::default();
    }
    let mut keep = std::collections::BTreeSet::new();
    let mut digits = String::new();
    for c in reply.chars().chain(std::iter::once(' ')) {
        if c.is_ascii_digit() {
            digits.push(c);
        } else if !digits.is_empty() {
            if let Ok(n) = digits.parse::<usize>() {
                if n >= 1 && n <= len {
                    keep.insert(n - 1);
                }
            }
            digits.clear();
        }
    }
    if keep.is_empty() && lower.contains("all") {
        return (0..len).collect();
    }
    keep
}

fn now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64)
}

fn hash(text: &str) -> String {
    use sha2::{Digest, Sha256};
    format!("{:x}", Sha256::digest(text.as_bytes()))
}

fn cache_key(bank: &str, project: &str, query: &str, limit: usize) -> String {
    hash(&format!("{bank}\u{1f}{project}\u{1f}{limit}\u{1f}{query}"))
}

fn clip(text: &str, max: usize) -> String {
    text.chars().take(max).collect()
}

/// Stable 64-bit FNV-1a: the embedding must hash identically across runs and versions.
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h = 0xcbf2_9ce4_8422_2325u64;
    for b in bytes {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100_0000_01b3);
    }
    h
}

fn hashed_embedding(text: &str, dim: usize) -> Vec<f32> {
    let words = tokens(text);
    let mut vec = vec![0f32; dim];
    for (i, word) in words.iter().enumerate() {
        vec[(fnv1a(word.as_bytes()) % dim as u64) as usize] += 1.0;
        if let Some(next) = words.get(i + 1) {
            vec[(fnv1a(format!("{word} {next}").as_bytes()) % dim as u64) as usize] += 0.5;
        }
    }
    let norm = vec.iter().map(|v| v * v).sum::<f32>().sqrt();
    if norm > 0.0 {
        for v in &mut vec {
            *v /= norm;
        }
    }
    vec
}

fn parse_embedding(text: &str) -> Result<Vec<f32>> {
    let value: serde_json::Value = serde_json::from_str(text).context("embedding response is not JSON")?;
    let array = value["data"][0]["embedding"]
        .as_array()
        .or_else(|| value["embedding"].as_array())
        .context("embedding response has no vector")?;
    let vec: Vec<f32> = array.iter().filter_map(|v| v.as_f64().map(|f| f as f32)).collect();
    anyhow::ensure!(!vec.is_empty(), "embedding response has an empty vector");
    Ok(vec)
}

fn to_blob(vec: &[f32]) -> Vec<u8> {
    vec.iter().flat_map(|f| f.to_le_bytes()).collect()
}

fn from_blob(bytes: &[u8]) -> Vec<f32> {
    bytes.chunks_exact(4).map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect()
}

fn cosine(a: &[f32], b: &[f32]) -> f64 {
    let (mut dot, mut na, mut nb) = (0f32, 0f32, 0f32);
    for (x, y) in a.iter().zip(b) {
        dot += x * y;
        na += x * x;
        nb += y * y;
    }
    if na == 0.0 || nb == 0.0 {
        return 0.0;
    }
    (dot / (na.sqrt() * nb.sqrt())) as f64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::mnemopi::EmbeddingVariant;

    /// The built-in embedder: hashed word unigrams and bigrams, L2-normalized so cosine is a dot product.
    fn local_embedding(text: &str, variant: EmbeddingVariant) -> Vec<f32> {
        hashed_embedding(text, variant.dim())
    }
    use crate::config::memory::{MemoryBackend, MemoryCfg};
    use serde_json::json;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn cfg() -> (tempfile::TempDir, Config) {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = Config::default();
        cfg.memory_opts.memory = MemoryCfg { backend: MemoryBackend::Mnemopi };
        cfg.memory_opts.mnemopi.db_path = Some(dir.path().join("m.db").to_string_lossy().into());
        cfg.memory_opts.mnemopi.bank = Some("test".into());
        cfg.memory_opts.mnemopi.no_embeddings = true;
        (dir, cfg)
    }

    #[tokio::test]
    async fn retains_then_recalls_by_keyword() {
        let (_dir, cfg) = cfg();
        let m = Mnemopi::new(&cfg, Path::new("/tmp/project"));
        assert!(m.add("The web build uses pnpm, never npm.", "project", "s1").await.unwrap());
        assert!(!m.add("The web build uses pnpm, never npm.", "project", "s1").await.unwrap());
        assert!(m.add("Deploys go through the ops gateway.", "project", "s1").await.unwrap());
        let hits = m.search("which package manager does the web build use", 8).await.unwrap();
        assert_eq!(hits.len(), 1, "{hits:?}");
        assert!(hits[0].text.contains("pnpm"));
        assert_eq!(hits[0].source.as_deref(), Some("keyword"));
    }

    #[tokio::test]
    async fn fts_only_mode_never_writes_an_embedding() {
        let (_dir, cfg) = cfg();
        let m = Mnemopi::new(&cfg, Path::new("/tmp/project"));
        m.add("The token refresh lives in auth.rs", "project", "").await.unwrap();
        let db = m.open().unwrap();
        let embeddings: i64 =
            db.query_row("SELECT COUNT(*) FROM memories WHERE embedding IS NOT NULL", [], |r| r.get(0)).unwrap();
        assert_eq!(embeddings, 0);
        let hits = m.search("token refresh", 8).await.unwrap();
        assert!(hits.iter().any(|h| h.text.contains("auth.rs")));
    }

    #[tokio::test]
    async fn vector_voice_finds_a_memory_without_a_shared_token() {
        let (_dir, mut cfg) = cfg();
        cfg.memory_opts.mnemopi.no_embeddings = false;
        // The local hashed embedding is token-based, so only a model embedding links disjoint words:
        // this stand-in answers every text with the same vector.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        cfg.memory_opts.mnemopi.embedding_api_url = Some(format!("http://{}", listener.local_addr().unwrap()));
        let server = tokio::spawn(async move {
            loop {
                let (mut stream, _) = listener.accept().await.unwrap();
                read_body(&mut stream).await;
                let reply = json!({"data": [{"embedding": [0.6, 0.8, 0.0]}]}).to_string();
                stream
                    .write_all(format!("HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{reply}", reply.len()).as_bytes())
                    .await
                    .unwrap();
            }
        });
        let m = Mnemopi::new(&cfg, Path::new("/tmp/project"));
        m.add("Release builds are signed with the nightly certificate.", "project", "").await.unwrap();
        let hits = m.search("credential rotation schedule", 8).await.unwrap();
        server.abort();
        assert_eq!(hits.len(), 1, "{hits:?}");
        assert_eq!(hits[0].source.as_deref(), Some("vector"), "{hits:?}");
    }

    #[tokio::test]
    async fn scoping_isolates_projects_and_tagged_mode_shares_global_rows() {
        let (_dir, mut cfg) = cfg();
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        let ma = Mnemopi::new(&cfg, a.path());
        let mb = Mnemopi::new(&cfg, b.path());
        assert_ne!(ma.bank, mb.bank);
        ma.add("Project A keeps migrations in sql/", "project", "").await.unwrap();
        assert!(mb.search("migrations", 8).await.unwrap().is_empty());

        cfg.memory_opts.mnemopi.scoping = Scoping::PerProjectTagged;
        let ma = Mnemopi::new(&cfg, a.path());
        let mb = Mnemopi::new(&cfg, b.path());
        assert_eq!(ma.bank, mb.bank);
        ma.add("Project A keeps migrations in sql/", "project", "").await.unwrap();
        ma.add("Every project runs rustfmt", "global", "").await.unwrap();
        assert!(mb.search("migrations", 8).await.unwrap().is_empty(), "another project's tag stays hidden");
        assert_eq!(mb.search("rustfmt", 8).await.unwrap().len(), 1, "a global row is visible everywhere");
    }

    #[tokio::test]
    async fn proactive_linking_surfaces_a_memory_without_a_keyword_match() {
        let (_dir, mut cfg) = cfg();
        cfg.memory_opts.mnemopi.polyphonic_recall = true;
        cfg.memory_opts.mnemopi.proactive_linking = true;
        let m = Mnemopi::new(&cfg, Path::new("/tmp/project"));
        m.add("GatewayService routes traffic to the axum server.", "project", "").await.unwrap();
        m.add("GatewayService uses the token bucket limiter.", "project", "").await.unwrap();
        m.add("Unrelated note about formatting.", "project", "").await.unwrap();
        let hits = m.search("token bucket limiter", 3).await.unwrap();
        assert!(hits.iter().any(|h| h.text.contains("axum")), "{hits:?}");
    }

    #[tokio::test]
    async fn enhanced_recall_caches_a_query_until_a_write_clears_it() {
        let (_dir, mut cfg) = cfg();
        cfg.memory_opts.mnemopi.enhanced_recall = true;
        let m = Mnemopi::new(&cfg, Path::new("/tmp/project"));
        m.add("The CI cache key is lockfile-v2", "project", "").await.unwrap();
        assert_eq!(m.search("lockfile cache key", 8).await.unwrap().len(), 1);
        m.open().unwrap().execute("DELETE FROM memories_fts", []).unwrap();
        assert_eq!(m.search("lockfile cache key", 8).await.unwrap().len(), 1, "second call came from the cache");
        m.add("A brand new fact", "project", "").await.unwrap();
        assert!(m.search("lockfile cache key", 8).await.unwrap().is_empty(), "the write cleared the cache");
    }

    #[tokio::test]
    async fn tool_ops_add_replace_and_remove_one_entry() {
        let (_dir, cfg) = cfg();
        let m = Mnemopi::new(&cfg, Path::new("/tmp/project"));
        let out = m.apply(&json!({"action": "add", "scope": "global", "text": "Use pnpm"})).await.unwrap();
        assert!(out.contains("saved"), "{out}");
        assert!(m.apply(&json!({"action": "replace", "scope": "global", "old": "pnpm", "text": "Use bun"})).await.unwrap().contains("replaced"));
        assert!(m.search("bun", 8).await.unwrap().iter().any(|h| h.text == "Use bun"));
        m.apply(&json!({"action": "remove", "scope": "global", "old": "bun"})).await.unwrap();
        assert!(m.search("bun", 8).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn retain_turn_stores_chunks_and_recall_finds_them() {
        let (_dir, cfg) = cfg();
        let m = Mnemopi::new(&cfg, Path::new("/tmp/project"));
        m.retain_turn("s1", "[role: user]\nThe API rate limiter sits behind nginx\n[user:end]\n\n[role: assistant]\nConfig lives in nginx.conf\n[assistant:end]").await.unwrap();
        let hits = m.recall("where is the rate limiter configured", 8).await.unwrap();
        assert!(hits.iter().any(|h| h.text.contains("nginx")), "{hits:?}");
        assert!(m.recall("anything", 8).await.is_ok());
    }

    #[tokio::test]
    async fn extraction_model_stores_facts_not_the_raw_transcript() {
        let (_dir, mut cfg) = cfg();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut data = Vec::new();
            loop {
                let mut buf = [0; 4096];
                let n = stream.read(&mut buf).await.unwrap();
                assert!(n > 0);
                data.extend_from_slice(&buf[..n]);
                if let Some(end) = data.windows(4).position(|w| w == b"\r\n\r\n") {
                    let headers = String::from_utf8_lossy(&data[..end]).to_lowercase();
                    let length: usize = headers.lines().find_map(|l| l.strip_prefix("content-length: ")).unwrap().parse().unwrap();
                    if data.len() >= end + 4 + length {
                        break;
                    }
                }
            }
            let sse = format!(
                "data: {}\n\ndata: [DONE]\n\n",
                json!({"choices": [{"delta": {"content": "The billing service owns invoice numbers."}, "finish_reason": "stop"}]})
            );
            stream
                .write_all(format!("HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{sse}", sse.len()).as_bytes())
                .await
                .unwrap();
        });
        cfg.memory_opts.mnemopi.llm_mode = LlmMode::Remote;
        cfg.memory_opts.mnemopi.llm_base_url = Some(base);
        cfg.memory_opts.mnemopi.llm_model = Some("tiny".into());
        let m = Mnemopi::new(&cfg, Path::new("/tmp/project"));
        m.retain_turn("s1", "[role: user]\nwhat owns invoice numbers\n[user:end]").await.unwrap();
        server.await.unwrap();
        let hits = m.search("invoice numbers", 8).await.unwrap();
        assert!(hits.iter().any(|h| h.text == "The billing service owns invoice numbers."), "{hits:?}");
        assert!(!hits.iter().any(|h| h.text.contains("[role: user]")));
    }

    #[tokio::test]
    async fn embedding_api_gets_the_query_prefix_and_input_type() {
        let (_dir, mut cfg) = cfg();
        cfg.memory_opts.mnemopi.no_embeddings = false;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let body = read_body(&mut stream).await;
            let sse = json!({"data": [{"embedding": [0.1, 0.2, 0.3]}]}).to_string();
            stream
                .write_all(format!("HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{sse}", sse.len()).as_bytes())
                .await
                .unwrap();
            body
        });
        cfg.memory_opts.mnemopi.embedding_api_url = Some(base);
        cfg.memory_opts.mnemopi.embedding_query_prefix = Some("query: ".into());
        cfg.memory_opts.mnemopi.embedding_input_type = true;
        let m = Mnemopi::new(&cfg, Path::new("/tmp/project"));
        let (vec, sig) = m.embed("login fails", true).await.unwrap();
        assert_eq!(vec, [0.1, 0.2, 0.3]);
        assert!(sig.contains("remote") || sig.ends_with("|3"), "{sig}");
        let body = server.await.unwrap();
        assert!(body.contains("\"input\":\"query: login fails\""), "{body}");
        assert!(body.contains("\"input_type\":\"query\""), "{body}");
    }

    #[tokio::test]
    async fn chunks_split_on_blank_lines_and_skip_tiny_pieces() {
        let text = format!("{}\n\nshort\n\n{}", "a".repeat(CHUNK), "b".repeat(40));
        let pieces = chunks(&text);
        assert_eq!(pieces.len(), 2);
        assert_eq!(pieces[0].len(), CHUNK);
    }

    #[test]
    fn judge_reply_parsing() {
        assert_eq!(parse_indices("2, 3", 4).into_iter().collect::<Vec<_>>(), [1, 2]);
        assert!(parse_indices("none", 4).is_empty());
        assert_eq!(parse_indices("all", 3).len(), 3);
        assert!(parse_indices("9", 3).is_empty());
    }

    #[test]
    fn embeddings_are_deterministic_per_variant() {
        let en = local_embedding("deploy with pnpm", EmbeddingVariant::En);
        assert_eq!(en, local_embedding("deploy with pnpm", EmbeddingVariant::En));
        assert_eq!(en.len(), 768);
        assert_eq!(local_embedding("deploy with pnpm", EmbeddingVariant::Multilingual).len(), 1024);
        assert!((cosine(&en, &en) - 1.0).abs() < 1e-6);
    }

    async fn read_body(stream: &mut tokio::net::TcpStream) -> String {
        let mut data = Vec::new();
        loop {
            let mut buf = [0; 4096];
            let n = stream.read(&mut buf).await.unwrap();
            assert!(n > 0);
            data.extend_from_slice(&buf[..n]);
            if let Some(end) = data.windows(4).position(|w| w == b"\r\n\r\n") {
                let headers = String::from_utf8_lossy(&data[..end]).to_lowercase();
                let length: usize = headers.lines().find_map(|l| l.strip_prefix("content-length: ")).unwrap().parse().unwrap();
                if data.len() >= end + 4 + length {
                    return String::from_utf8_lossy(&data[end + 4..]).into_owned();
                }
            }
        }
    }
}
