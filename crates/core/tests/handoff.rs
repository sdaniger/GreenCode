//! Portable handoff: a GreenCode transcript exported on one machine imports as a new thread.
use greencode_core::{import, session::{Meta, Session}, Block, Message, Role};

#[test]
fn exported_thread_imports_with_its_tool_blocks() {
    let home = std::env::temp_dir().join(format!("greencode-handoff-{}-{}", std::process::id(), std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
    std::fs::create_dir_all(&home).unwrap();
    unsafe { std::env::set_var("GREENCODE_HOME", &home) };

    let mut meta = Meta::new(std::env::temp_dir(), "anthropic/claude-opus-5-5".into(), "native".into());
    meta.title = "handoff".into();
    let session = Session::create(&meta).unwrap();
    session.append(&Message::user("read a file")).unwrap();
    session.append(&Message { role: Role::Assistant, content: vec![Block::ToolUse { id: "t1".into(), name: "read".into(), input: serde_json::json!({"path":"a"}) }] }).unwrap();
    session.append(&Message { role: Role::User, content: vec![Block::ToolResult { tool_use_id: "t1".into(), content: "ok".into(), is_error: false }] }).unwrap();

    let file = home.join("handoff.jsonl");
    Session::export(&meta.id, &file).unwrap();
    let imported = import::import_native(&file).unwrap().expect("native transcript");
    assert_ne!(imported.id, meta.id, "a new thread id");
    assert_eq!(imported.title, "handoff");
    let (_, messages) = Session::load(&imported.id).unwrap();
    assert_eq!(messages.len(), 3);
    assert!(matches!(&messages[1].content[0], Block::ToolUse { name, .. } if name == "read"), "tool blocks survive");
    assert!(import::import_native(&home.join("missing.jsonl")).is_err());
    let _ = std::fs::remove_dir_all(home);
}
