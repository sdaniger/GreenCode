use greencode_core::{agent::Agent, config::{Config, ProviderCfg}, permission::{Mode, Perms}, provider::{Provider, Reply, Request}, session::{Meta, Session}, snap, Backend, Block, BoxFut, Message, Role};
use std::sync::{Arc, LazyLock, Mutex};

#[derive(Default)]
struct Mock { seen: Mutex<Vec<String>> }
impl Provider for Mock {
    fn complete<'a>(&'a self, req: Request<'a>, _: &'a (dyn Fn(&str) + Send + Sync)) -> BoxFut<'a, anyhow::Result<Reply>> {
        Box::pin(async move {
            self.seen.lock().unwrap().push(serde_json::to_string(req.messages).unwrap());
            Ok(Reply { blocks: vec![Block::Text { text: "summary".into() }], ..Default::default() })
        })
    }
}
fn agent(name: &str, kind: &str, base: &str) -> (Agent, Meta, Arc<Mock>) {
    static HOME: LazyLock<std::path::PathBuf> = LazyLock::new(|| {
        let path = std::env::temp_dir().join(format!("greencode-snap-test-{}", std::process::id()));
        std::fs::create_dir_all(&path).unwrap();
        std::env::set_var("GREENCODE_HOME", &path);
        path
    });
    let cwd = HOME.join(name);
    std::fs::create_dir_all(&cwd).unwrap();
    let meta = Meta::new(cwd, "anthropic/claude-x".into(), "native".into());
    let mut cfg = Config::default();
    cfg.providers.insert("anthropic".into(), ProviderCfg { kind: kind.into(), base_url: base.into(), ..Default::default() });
    let mock = Arc::new(Mock::default());
    let a = Agent::with_provider(Arc::new(cfg), &meta, Arc::new(Perms::new(Mode::Full)), mock.clone(), "claude-x".into());
    (a, meta, mock)
}
async fn run(a: &mut Agent, command: &str) -> anyhow::Result<()> {
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    a.prompt(command.into(), &tx).await
}
fn append_history(a: &mut Agent, session: &Session, count: usize) {
    for i in 0..count {
        let prompt = Message::user(format!("prompt {i} {}", "x".repeat(5000)));
        let result = Message { role: Role::User, content: vec![Block::ToolResult { tool_use_id: "t".into(), content: "output".repeat(500), is_error: false }] };
        session.append(&prompt).unwrap(); session.append(&result).unwrap();
        a.messages.push(prompt); a.messages.push(result);
    }
}
#[tokio::test]
async fn snap_persists_and_rearchives_without_model() {
    let (mut a, meta, mock) = agent("snap", "anthropic", "");
    let session = Session::create(&meta).unwrap();
    append_history(&mut a, &session, 10);
    let last = Message::user("last exact prompt");
    session.append(&last).unwrap(); a.messages.push(last);
    // Reopen so the agent owns the session created above.
    drop(a);
    let mut cfg = Config::default();
    cfg.providers.insert("anthropic".into(), ProviderCfg { kind: "anthropic".into(), ..Default::default() });
    let mut a = Agent::with_provider(Arc::new(cfg), &meta, Arc::new(Perms::new(Mode::Full)), mock.clone(), "claude-x".into());
    run(&mut a, "/compact snap").await.unwrap();
    assert!(mock.seen.lock().unwrap().is_empty());
    assert!(matches!(&a.messages[0].content[0], Block::Text { text } if text.starts_with("Resume prior conversation")));
    assert!(a.messages[0].content.iter().any(|b| matches!(b, Block::Other(v) if v["source"]["media_type"] == "image/png")));
    assert!(a.messages.iter().any(|m| matches!(m.content.first(), Some(Block::Text { text }) if text == "last exact prompt")));
    let (_, loaded) = Session::load(&meta.id).unwrap();
    assert_eq!(serde_json::to_value(&a.messages).unwrap(), serde_json::to_value(loaded).unwrap());
    let source = Session::snap_source(&meta.id).unwrap();
    append_history(&mut a, &session, 10);
    run(&mut a, "/compact snap").await.unwrap();
    let source2 = Session::snap_source(&meta.id).unwrap();
    assert!(source2.starts_with(&source));
    assert!(!source2.contains("Resume prior conversation"));
    assert!(a.messages[0].content.len() - 1 <= snap::MAX_FRAMES);
    assert!(mock.seen.lock().unwrap().is_empty());
}
#[tokio::test]
async fn nonvision_explicit_snap_errors() {
    let (mut a, _, mock) = agent("nonvision", "openai", "http://localhost:1/v1");
    a.messages.push(Message::user("hello"));
    assert!(run(&mut a, "/compact snap").await.unwrap_err().to_string().contains("/compact summary"));
    assert!(mock.seen.lock().unwrap().is_empty());
}
#[tokio::test]
async fn default_compaction_uses_summary() {
    let (mut a, _, mock) = agent("summary", "anthropic", "");
    a.messages.push(Message::user("hello"));
    run(&mut a, "/compact").await.unwrap();
    assert_eq!(mock.seen.lock().unwrap().len(), 1);
    assert!(matches!(&a.messages[0].content[0], Block::Text { text } if text.contains("summary")));
}

#[tokio::test]
async fn snap_keeps_a_tail_in_one_long_turn() {
    let (mut a, meta, mock) = agent("long-turn", "anthropic", "");
    let session = Session::create(&meta).unwrap();
    let prompt = Message::user("please refactor the parser");
    session.append(&prompt).unwrap(); a.messages.push(prompt);
    for i in 0..30 {
        let id = format!("t{i}");
        let call = Message { role: Role::Assistant, content: vec![Block::ToolUse { id: id.clone(), name: "read".into(), input: serde_json::json!({}) }] };
        let result = Message { role: Role::User, content: vec![Block::ToolResult { tool_use_id: id, content: "x".repeat(3000), is_error: false }] };
        session.append(&call).unwrap(); session.append(&result).unwrap();
        a.messages.push(call); a.messages.push(result);
    }
    drop(a);
    let mut cfg = Config::default();
    cfg.providers.insert("anthropic".into(), ProviderCfg { kind: "anthropic".into(), ..Default::default() });
    let mut a = Agent::with_provider(Arc::new(cfg), &meta, Arc::new(Perms::new(Mode::Full)), mock.clone(), "claude-x".into());
    run(&mut a, "/compact snap").await.unwrap();
    assert!(a.messages.len() > 1);
    assert!(matches!(&a.messages[0].content[0], Block::Text { text } if text.contains("Latest user request (verbatim, still in force)") && text.contains("please refactor the parser")));
    assert_eq!(a.messages[1].role, Role::Assistant);
    let mut calls = std::collections::HashSet::new();
    for message in &a.messages[1..] {
        for block in &message.content {
            match block {
                Block::ToolUse { id, .. } => { calls.insert(id); }
                Block::ToolResult { tool_use_id, .. } => assert!(calls.contains(tool_use_id)),
                _ => (),
            }
        }
    }
    let (_, loaded) = Session::load(&meta.id).unwrap();
    assert_eq!(serde_json::to_value(&a.messages).unwrap(), serde_json::to_value(loaded).unwrap());
    assert!(mock.seen.lock().unwrap().is_empty());
}

#[tokio::test]
async fn snap_falls_back_to_summary_for_japanese_history() {
    let (mut a, meta, mock) = agent("japanese", "anthropic", "");
    let session = Session::create(&meta).unwrap();
    for _ in 0..10 {
        for message in [Message::user("日本語".repeat(1200)), Message { role: Role::Assistant, content: vec![Block::Text { text: "履歴".repeat(1800) }] }] {
            session.append(&message).unwrap(); a.messages.push(message);
        }
    }
    drop(a);
    let mut cfg = Config::default();
    cfg.providers.insert("anthropic".into(), ProviderCfg { kind: "anthropic".into(), ..Default::default() });
    let mut a = Agent::with_provider(Arc::new(cfg), &meta, Arc::new(Perms::new(Mode::Full)), mock.clone(), "claude-x".into());
    run(&mut a, "/compact snap").await.unwrap();
    assert_eq!(mock.seen.lock().unwrap().len(), 1);
    assert!(a.messages.len() > 1);
    assert!(matches!(&a.messages[0].content[0], Block::Text { text } if text.contains("<compacted-summary>\nsummary")));
    let lines = std::fs::read_to_string(Session::path(&meta.id)).unwrap();
    let marker = lines.lines().filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok()).rfind(|v| v.get("compact").is_some()).unwrap();
    assert!(marker.get("frames").is_none());
}
