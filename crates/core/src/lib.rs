//! GreenCode core: agent loop, providers, tools, sessions. No GUI dependencies.

pub mod acp;
pub mod agent;
pub mod config;
pub mod context;
pub mod jsonrpc;
pub mod lua;
pub mod permission;
pub mod provider;
pub mod session;
pub mod skills;
pub mod tools;
pub mod worktree;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{future::Future, path::PathBuf, pin::Pin, sync::Arc};
use tokio::sync::{mpsc, oneshot};

pub type BoxFut<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;
pub type EventTx = mpsc::UnboundedSender<Event>;

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    User,
    Assistant,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: Vec<Block>,
}

impl Message {
    pub fn user(text: impl Into<String>) -> Self {
        Message { role: Role::User, content: vec![Block::Text { text: text.into() }] }
    }
}

fn is_false(b: &bool) -> bool {
    !*b
}

/// Anthropic wire format. Blocks we don't interpret (thinking, fallback, ...) are
/// kept verbatim in `Other` because the API requires them replayed unchanged.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Block {
    Text {
        text: String,
    },
    ToolUse {
        id: String,
        name: String,
        input: Value,
    },
    ToolResult {
        tool_use_id: String,
        content: String,
        #[serde(default, skip_serializing_if = "is_false")]
        is_error: bool,
    },
    #[serde(untagged)]
    Other(Value),
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ToolDef {
    pub name: String,
    pub description: String,
    pub schema: Value,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Usage {
    /// Uncached input tokens.
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
}

impl Usage {
    pub fn context(&self) -> u64 {
        self.input + self.cache_read + self.cache_write
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Decision {
    Allow,
    Always,
    Deny,
}

#[derive(Debug)]
pub enum Event {
    Text(String),
    ToolStart { id: String, name: String, input: Value },
    ToolEnd { id: String, output: String, is_error: bool },
    Permission { tool: String, summary: String, reply: oneshot::Sender<Decision> },
    Usage(Usage),
    Notify(String),
    Error(String),
    /// The turn is over (success, error or cancel).
    Done,
}

pub enum Command {
    Prompt(String),
    Cancel,
    Model(String),
}

pub trait Backend: Send {
    fn prompt<'a>(&'a mut self, text: String, tx: &'a EventTx) -> BoxFut<'a, anyhow::Result<()>>;
    fn cancel<'a>(&'a mut self) -> BoxFut<'a, ()> {
        Box::pin(async {})
    }
}

/// `~/.greencode`, or `$GREENCODE_HOME`.
pub fn home() -> PathBuf {
    std::env::var_os("GREENCODE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| user_home().join(".greencode"))
}

pub fn user_home() -> PathBuf {
    std::env::home_dir().unwrap_or_else(|| PathBuf::from("."))
}

async fn make_backend(
    cfg: &Arc<config::Config>,
    meta: &session::Meta,
    perms: &Arc<permission::Perms>,
) -> anyhow::Result<Box<dyn Backend>> {
    if meta.backend == "native" {
        return Ok(Box::new(agent::Agent::new(cfg.clone(), meta, perms.clone())?));
    }
    let b = cfg
        .backends
        .iter()
        .find(|b| b.name == meta.backend)
        .ok_or_else(|| anyhow::anyhow!("unknown backend `{}`", meta.backend))?;
    Ok(Box::new(acp::Acp::start(&b.command, meta, perms.clone()).await?))
}

/// Runs one thread. The backend is created on the first prompt, so idle threads cost nothing.
pub fn spawn(
    rt: &tokio::runtime::Handle,
    cfg: Arc<config::Config>,
    mut meta: session::Meta,
    perms: Arc<permission::Perms>,
) -> (mpsc::UnboundedSender<Command>, mpsc::UnboundedReceiver<Event>) {
    let (ctx, mut crx) = mpsc::unbounded_channel();
    let (tx, erx) = mpsc::unbounded_channel();
    rt.spawn(async move {
        let mut backend: Option<Box<dyn Backend>> = None;
        while let Some(cmd) = crx.recv().await {
            match cmd {
                Command::Cancel => {}
                // Native history lives in the session file, so the backend is simply rebuilt.
                Command::Model(m) => {
                    meta.model = m;
                    backend = None;
                }
                Command::Prompt(text) => {
                    if backend.is_none() {
                        match make_backend(&cfg, &meta, &perms).await {
                            Ok(b) => backend = Some(b),
                            Err(e) => {
                                let _ = tx.send(Event::Error(format!("{e:#}")));
                                let _ = tx.send(Event::Done);
                                continue;
                            }
                        }
                    }
                    let b = backend.as_mut().unwrap();
                    let cancelled = {
                        let fut = b.prompt(text, &tx);
                        tokio::pin!(fut);
                        loop {
                            tokio::select! {
                                r = &mut fut => {
                                    if let Err(e) = r {
                                        let _ = tx.send(Event::Error(format!("{e:#}")));
                                    }
                                    break false;
                                }
                                c = crx.recv() => if matches!(c, Some(Command::Cancel) | None) {
                                    break true;
                                }
                            }
                        }
                    };
                    if cancelled {
                        b.cancel().await;
                    }
                    let _ = tx.send(Event::Done);
                }
            }
        }
    });
    (ctx, erx)
}
