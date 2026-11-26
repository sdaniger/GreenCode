use greencode_core::{
    agent::Agent,
    config::Config,
    context,
    permission::{Mode, Perms},
    provider::{anthropic, openai, Provider, Reply, Request},
    session::{Meta, Session},
    tools, Backend, Block, BoxFut, Decision, Event, Message, Role,
};
use serde_json::{json, Value};
use std::{
    collections::VecDeque,
    path::PathBuf,
    sync::{Arc, Mutex, OnceLock},
};

#[test]
fn opencode_effort_config_merges_per_model() {
    let existing = json!({"theme":"dark", "provider":{"codex-web-gpt":{"models":{
        "chatgpt-web/gpt-6-sol":{"name":"Sol","options":{"reasoningEffort":"high","other":true}},
        "other":{"options":{"keep":1}}
    }}}, "agent":{"build":{"options":{"keep":2}}}})
    .to_string();
    let merged = greencode_core::opencode_effort_env(
        "codex-web-gpt/chatgpt-web/gpt-6-sol",
        Some("low"),
        Some(&existing),
    )
    .unwrap();
    let merged: Value = serde_json::from_str(&merged).unwrap();
    assert_eq!(
        merged["provider"]["codex-web-gpt"]["models"]["chatgpt-web/gpt-6-sol"]["options"],
        json!({"reasoningEffort":"low","other":true})
    );
    assert_eq!(
        merged["provider"]["codex-web-gpt"]["models"]["chatgpt-web/gpt-6-sol"]["name"],
        "Sol"
    );
    assert_eq!(
        merged["provider"]["codex-web-gpt"]["models"]["other"]["options"]["keep"],
        1
    );
    assert_eq!(merged["agent"]["build"]["options"]["keep"], 2);
    assert_eq!(merged["theme"], "dark");
    assert!(greencode_core::opencode_effort_env("provider/model", None, Some(&existing)).is_none());
    assert!(greencode_core::opencode_effort_env("default", Some("low"), None).is_none());
    assert!(
        greencode_core::opencode_effort_env("provider/model", Some("low"), Some("invalid"))
            .is_none()
    );
}

#[test]
fn opencode_asks_for_shell_and_edits_unless_configured() {
    let ask: Value =
        serde_json::from_str(&greencode_core::opencode_permission_env(None).unwrap()).unwrap();
    assert_eq!(ask["permission"], json!({"bash":"ask","edit":"ask"}));
    let kept: Value = serde_json::from_str(
        &greencode_core::opencode_permission_env(Some(
            r#"{"theme":"x","permission":{"bash":"allow"}}"#,
        ))
        .unwrap(),
    )
    .unwrap();
    assert_eq!(
        (&kept["permission"], &kept["theme"]),
        (&json!({"bash":"allow"}), &json!("x"))
    );
    assert!(greencode_core::opencode_permission_env(Some("invalid")).is_none());
}

#[test]
fn provider_effort_is_clamped_to_model_capabilities() {
    use greencode_core::config::ProviderCfg;
    use greencode_core::provider::{effort_levels, model_effort};
    let mut cfg = Config::default();
    for (name, kind, url) in [
        ("anthropic", "anthropic", "https://api.anthropic.com"),
        ("openai", "openai", "https://api.openai.com/v1"),
        ("chatgpt", "codex", "https://chatgpt.com/backend-api/codex"),
        ("ollama", "openai", "http://localhost:11434/v1"),
        ("proxy", "openai", "https://api.openai.com.example/v1"),
    ] {
        cfg.providers.insert(
            name.into(),
            ProviderCfg {
                kind: kind.into(),
                base_url: url.into(),
                ..Default::default()
            },
        );
    }
    cfg.effort = Some("max".into());
    assert_eq!(
        model_effort(&cfg, "anthropic/claude-opus-5-5").as_deref(),
        Some("max")
    );
    assert_eq!(model_effort(&cfg, "anthropic/claude-haiku-5-5"), None);
    assert_eq!(model_effort(&cfg, "chatgpt/gpt-6-test"), None);
    assert_eq!(model_effort(&cfg, "openai/gpt-6-luna"), None);
    assert_eq!(effort_levels(&cfg, "ollama/x"), Vec::<&str>::new());
    assert!(effort_levels(&cfg, "proxy/gpt-6-luna").is_empty());
    assert_eq!(
        effort_levels(&cfg, "anthropic/claude-opus-5-5"),
        ["low", "medium", "high", "xhigh", "max"]
    );
    assert_eq!(
        effort_levels(&cfg, "anthropic/claude-haiku-5-5"),
        ["low", "medium", "high"]
    );
    assert_eq!(
        effort_levels(&cfg, "openai/gpt-6-luna"),
        ["low", "medium", "high", "xhigh"]
    );
    assert_eq!(
        effort_levels(&cfg, "chatgpt/GPT-5.5"),
        ["low", "medium", "high", "xhigh"]
    );
    cfg.effort = Some("xhigh".into());
    assert_eq!(
        model_effort(&cfg, "chatgpt/gpt-6-test").as_deref(),
        Some("xhigh")
    );
    assert_eq!(model_effort(&cfg, "ollama/x"), None);
}

/// One shared GREENCODE_HOME (env is process-wide), one project dir per test.
fn dir(name: &str) -> PathBuf {
    static HOME: OnceLock<PathBuf> = OnceLock::new();
    let home = HOME.get_or_init(|| {
        let h = std::env::temp_dir().join(format!("greencode-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&h);
        std::fs::create_dir_all(h.join("extensions")).unwrap();
        // Loaded by every agent in this process; only reacts to its own markers.
        std::fs::write(
            h.join("extensions/test.lua"),
            r#"
gc.tool{ name = "shout", description = "upper-case", schema = { type = "object", properties = { s = { type = "string" } } },
  run = function(i) return string.upper(i.s) end }
gc.on("tool_call", function(c)
  if c.tool == "bash" and c.input.command:find("forbidden") then return { deny = "nope" } end
end)
gc.command("hi", function(args) return "hello " .. args end)
"#,
        )
        .unwrap();
        std::env::set_var("GREENCODE_HOME", &h);
        h
    });
    let d = home.join("projects").join(name);
    std::fs::create_dir_all(&d).unwrap();
    d
}

#[derive(Default)]
struct Mock {
    replies: Mutex<VecDeque<Reply>>,
    routes: Mutex<Vec<(String, VecDeque<Reply>)>>,
    hang: Mutex<Vec<String>>,
    gates: Mutex<Vec<(String, Arc<tokio::sync::Notify>)>>,
    /// (system + tools, messages) as sent, serialized.
    seen: Mutex<Vec<(String, String)>>,
    models: Mutex<Vec<String>>,
    steer: Mutex<Option<(usize, tokio::sync::mpsc::UnboundedSender<Value>, Value)>>,
}

impl Mock {
    fn new(replies: Vec<Vec<Block>>) -> Arc<Mock> {
        let replies = replies
            .into_iter()
            .map(|blocks| Reply {
                blocks,
                ..Default::default()
            })
            .collect();
        Arc::new(Mock {
            replies: Mutex::new(replies),
            ..Default::default()
        })
    }
    fn routed(replies: Vec<Vec<Block>>, routes: Vec<(&str, Vec<Vec<Block>>)>) -> Arc<Mock> {
        let replies = replies
            .into_iter()
            .map(|blocks| Reply {
                blocks,
                ..Default::default()
            })
            .collect();
        let routes = routes
            .into_iter()
            .map(|(marker, replies)| {
                (
                    marker.to_string(),
                    replies
                        .into_iter()
                        .map(|blocks| Reply {
                            blocks,
                            ..Default::default()
                        })
                        .collect(),
                )
            })
            .collect();
        Arc::new(Mock {
            replies: Mutex::new(replies),
            routes: Mutex::new(routes),
            ..Default::default()
        })
    }
}

impl Provider for Mock {
    fn complete<'a>(
        &'a self,
        req: Request<'a>,
        on_text: &'a (dyn Fn(&str) + Send + Sync),
    ) -> BoxFut<'a, anyhow::Result<Reply>> {
        Box::pin(async move {
            let prefix = format!(
                "{}{}",
                req.system,
                serde_json::to_string(req.tools).unwrap()
            );
            let messages = serde_json::to_string(req.messages).unwrap();
            let call = {
                let mut seen = self.seen.lock().unwrap();
                seen.push((prefix.clone(), messages.clone()));
                seen.len()
            };
            let steered = {
                let mut steer = self.steer.lock().unwrap();
                if steer.as_ref().is_some_and(|(at, _, _)| *at == call) {
                    let (_, tx, message) = steer.take().unwrap();
                    let sub = message["type"] == "sub_steer";
                    tx.send(message).unwrap();
                    sub
                } else {
                    false
                }
            };
            if steered {
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            }
            self.models.lock().unwrap().push(req.model.to_string());
            let is_sub = prefix.contains("# Role: ") || messages.contains("# Role: ");
            let hangs = is_sub
                && self
                    .hang
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|marker| messages.contains(marker));
            if hangs {
                std::future::pending::<()>().await;
            }
            let gate = self
                .gates
                .lock()
                .unwrap()
                .iter()
                .find(|(marker, _)| is_sub && messages.contains(marker))
                .map(|(_, gate)| gate.clone());
            if let Some(gate) = gate {
                gate.notified().await;
            }
            let reply = {
                let mut routes = self.routes.lock().unwrap();
                match routes
                    .iter_mut()
                    .find(|(marker, _)| is_sub && messages.contains(marker))
                {
                    Some((_, replies)) => {
                        replies.pop_front().expect("mock route ran out of replies")
                    }
                    None => self
                        .replies
                        .lock()
                        .unwrap()
                        .pop_front()
                        .expect("mock ran out of replies"),
                }
            };
            for b in &reply.blocks {
                if let Block::Text { text } = b {
                    on_text(text);
                }
            }
            Ok(reply)
        })
    }
}

fn text(s: &str) -> Block {
    Block::Text { text: s.into() }
}

#[test]
fn external_runtime_rejects_another_backend() {
    let meta = Meta::new(dir("runtime_owner"), "test".into(), "pi".into());
    assert!(Session::runtime_for_backend(&meta.id, "pi")
        .unwrap()
        .is_null());
    Session::save_runtime(
        &meta.id,
        &json!({"backend": "pi", "sessionFile": "session.jsonl"}),
    )
    .unwrap();
    assert_eq!(
        Session::runtime_for_backend(&meta.id, "pi").unwrap()["sessionFile"],
        "session.jsonl"
    );
    assert!(Session::runtime_for_backend(&meta.id, "other").is_err());
    assert_eq!(Session::runtime(&meta.id).unwrap()["backend"], "pi");
}

#[tokio::test]
async fn external_model_selection_reports_backend_start_failure() {
    let meta = Meta::new(
        dir("model_start_failure"),
        "default".into(),
        "missing-backend".into(),
    );
    let (tx, mut rx) = greencode_core::spawn(
        &tokio::runtime::Handle::current(),
        Arc::new(Config::default()),
        meta,
        Arc::new(Perms::new(Mode::default())),
    );
    tx.send(greencode_core::Command::Model("new-model".into()))
        .unwrap();
    let event = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
        .await
        .unwrap();
    assert!(matches!(event, Some(Event::Error(message)) if message.contains("unknown backend")));
}

#[test]
fn reset_hides_history_from_the_agent_but_not_from_the_view() {
    let meta = Meta::new(dir("reset"), "test".into(), "native".into());
    let session = Session::create(&meta).unwrap();
    session.append(&Message::user("before")).unwrap();
    Session::reset(&meta.id).unwrap();
    session.append(&Message::user("after")).unwrap();
    assert_eq!(Session::load(&meta.id).unwrap().1.len(), 1);
    let (_, all, resets) = Session::load_all(&meta.id).unwrap();
    assert_eq!((all.len(), resets), (2, vec![1]));
    assert_eq!(Session::handoff(&meta.id, 10_000).unwrap().0.len(), 1);
}

#[test]
fn handoff_keeps_recent_complete_text_without_tool_state() {
    let meta = Meta::new(dir("handoff"), "test".into(), "native".into());
    let session = Session::create(&meta).unwrap();
    session
        .append(&Message::user("old conversation".repeat(30)))
        .unwrap();
    session
        .append(&Message {
            role: Role::Assistant,
            content: vec![
                text("最近の回答"),
                call("id", "read", json!({"path":"secret"})),
                Block::Other(json!({"type":"thinking","thinking":"private"})),
            ],
        })
        .unwrap();
    session.append(&Message::user("next question")).unwrap();
    let (history, omitted) = Session::handoff(&meta.id, 200).unwrap();
    assert!(omitted);
    assert_eq!(history.len(), 2);
    assert_eq!(history[0].role, Role::Assistant);
    assert_eq!(history[1].role, Role::User);
    let rendered = serde_json::to_string(&history).unwrap();
    assert!(rendered.contains("最近の回答") && rendered.contains("next question"));
    assert!(!rendered.contains("secret") && !rendered.contains("private"));
    assert!(rendered.len() + history.len() <= 200);
}

#[test]
fn handoff_reports_omitted_non_text_even_with_room() {
    let meta = Meta::new(dir("handoff_image"), "test".into(), "native".into());
    let session = Session::create(&meta).unwrap();
    session
        .append(&Message {
            role: Role::User,
            content: vec![
                text("describe this"),
                Block::Other(json!({"type":"image","source":{}})),
            ],
        })
        .unwrap();
    let (history, omitted) = Session::handoff(&meta.id, 10_000).unwrap();
    assert!(omitted);
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].content.len(), 1);
}

#[test]
fn handoff_preserves_original_request_across_compaction() {
    let meta = Meta::new(dir("handoff_compact"), "test".into(), "native".into());
    let session = Session::create(&meta).unwrap();
    session.append(&Message::user("original request")).unwrap();
    session
        .append(&Message::user("older detail".repeat(30)))
        .unwrap();
    session.append(&json!({"compact": "summary"})).unwrap();
    session.append(&Message::user("latest request")).unwrap();
    let (history, omitted) = Session::handoff(&meta.id, 160).unwrap();
    let rendered = serde_json::to_string(&history).unwrap();
    assert!(omitted);
    assert!(rendered.contains("original request") && rendered.contains("latest request"));
    assert!(!rendered.contains("summary"));
    assert!(rendered.len() + history.len() <= 160);
}
fn call(id: &str, name: &str, input: Value) -> Block {
    Block::ToolUse {
        id: id.into(),
        name: name.into(),
        input,
    }
}

fn agent(name: &str, mode: Mode, mock: &Arc<Mock>) -> (Agent, Meta) {
    let meta = Meta::new(dir(name), "mock/m".into(), "native".into());
    let mut cfg = Config::default();
    cfg.learning.auto = false;
    let a = Agent::with_provider(
        Arc::new(cfg),
        &meta,
        Arc::new(Perms::new(mode)),
        mock.clone(),
        "m".into(),
    );
    (a, meta)
}

#[tokio::test]
async fn inline_edit_payloads_written_as_text_run_as_edit_calls() {
    let dir = dir("recover-inline");
    let path = dir.join("a.txt");
    std::fs::write(&path, "first\n").unwrap();
    let mock = Mock::new(vec![
        vec![text("```json\n{\"path\":\"a.txt\",\"old_string\":\"first\",\"new_string\":\"second\"}\n```")],
        vec![text("done")],
    ]);
    let meta = Meta::new(dir, "mock/m".into(), "native".into());
    let mut cfg = Config::default();
    cfg.learning.auto = false;
    let owner = meta.id.clone();
    let mut a = Agent::with_provider(Arc::new(cfg), &meta, Arc::new(Perms::new(Mode::Full)), mock.clone(), "m".into());
    let (result, events) = run(&mut a, "apply that", Decision::Allow).await;
    result.unwrap();
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "second\n");
    assert!(results(&events).iter().any(|(output, error)| !error && output.contains("replaced 1")));
}

#[tokio::test]
async fn native_steer_during_a_tool_round_reaches_the_next_request() {
    let mock = Mock::new(vec![
        vec![call("t1", "unknown_tool", json!({}))],
        vec![text("done")],
    ]);
    let (mut a, _) = agent("steer-tool-round", Mode::Full, &mock);
    *mock.steer.lock().unwrap() = Some((
        1,
        Backend::live(&a).unwrap(),
        json!({"type":"steer","message":"actually do X"}),
    ));
    run(&mut a, "start", Decision::Deny).await.0.unwrap();
    let seen = mock.seen.lock().unwrap();
    assert_eq!(seen.len(), 2);
    let messages: Vec<Message> = serde_json::from_str(&seen[1].1).unwrap();
    assert!(matches!(
        &messages[messages.len() - 2].content[0],
        Block::ToolResult { .. }
    ));
    assert_eq!(messages.last().unwrap().role, Role::User);
    assert!(
        matches!(&messages.last().unwrap().content[0], Block::Text { text } if text == "actually do X")
    );
    assert!(
        matches!(&a.messages.last().unwrap().content[0], Block::Text { text } if text == "done")
    );
}

#[tokio::test]
async fn broad_tool_outputs_share_two_kib_without_cutting_a_targeted_read() {
    let big = |c: char| json!({"command": format!("python3 -c \"print('{c}'*1800)\"")});
    let mock = Mock::new(vec![
        vec![
            call("t1", "bash", big('a')),
            call("t2", "bash", big('b')),
            call(
                "read",
                "read",
                json!({"path":"exact.txt","offset":1,"limit":1}),
            ),
        ],
        vec![text("done")],
    ]);
    let (mut a, meta) = agent("turn-budget", Mode::Full, &mock);
    let exact = "detail".repeat(400);
    std::fs::write(meta.cwd.join("exact.txt"), &exact).unwrap();
    run(&mut a, "start", Decision::Deny).await.0.unwrap();
    let seen = mock.seen.lock().unwrap();
    let messages: Vec<Message> = serde_json::from_str(&seen[1].1).unwrap();
    let results: Vec<&str> = messages
        .last()
        .unwrap()
        .content
        .iter()
        .filter_map(|b| match b {
            Block::ToolResult { content, .. } => Some(content.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(results.len(), 3);
    assert_eq!(results[2], exact);
    assert!(results[..2].iter().map(|s| s.len()).sum::<usize>() <= 2048);
    for (result, ch) in results[..2].iter().zip(["a", "b"]) {
        let path = result
            .split_once("full output: ")
            .unwrap()
            .1
            .split_once(" ...]")
            .unwrap()
            .0;
        assert_eq!(
            std::fs::read_to_string(path).unwrap(),
            format!("{}\n", ch.repeat(1800))
        );
    }
}

#[tokio::test]
async fn max_tool_calls_counts_across_model_replies() {
    let mock = Mock::new(vec![
        vec![call("a", "read", json!({"path": "missing"}))],
        vec![call("b", "read", json!({"path": "missing"}))],
        vec![text("done")],
    ]);
    let meta = Meta::new(dir("tool-limit"), "mock/m".into(), "native".into());
    let cfg = Arc::new(Config {
        max_tool_calls: 1,
        ..Config::default()
    });
    let mut a = Agent::with_provider(
        cfg,
        &meta,
        Arc::new(Perms::new(Mode::Full)),
        mock,
        "m".into(),
    );
    assert!(run(&mut a, "start", Decision::Deny).await.0.is_err());
}

#[tokio::test]
async fn native_steer_during_the_last_reply_continues_the_turn() {
    let mock = Mock::new(vec![vec![text("first")], vec![text("done")]]);
    let (mut a, _) = agent("steer-last-reply", Mode::Full, &mock);
    *mock.steer.lock().unwrap() = Some((
        1,
        Backend::live(&a).unwrap(),
        json!({"type":"steer","message":"actually do X"}),
    ));
    let (result, events) = run(&mut a, "start", Decision::Deny).await;
    result.unwrap();
    assert!(
        events
            .iter()
            .any(|e| matches!(e, Event::Text(t) if t == "\n\n")),
        "the follow-up reply must be separated from the first"
    );
    let seen = mock.seen.lock().unwrap();
    assert_eq!(seen.len(), 2);
    let messages: Vec<Message> = serde_json::from_str(&seen[1].1).unwrap();
    assert_eq!(messages.last().unwrap().role, Role::User);
    assert!(
        matches!(&messages.last().unwrap().content[0], Block::Text { text } if text == "actually do X")
    );
}

#[tokio::test]
async fn a_steer_left_by_a_cancelled_turn_stays_before_the_next_prompt() {
    let mock = Mock::new(vec![vec![text("ok")]]);
    let (mut a, _) = agent("steer-stale", Mode::Full, &mock);
    Backend::live(&a)
        .unwrap()
        .send(serde_json::json!({"type": "steer", "message": "old instruction"}))
        .unwrap();
    run(&mut a, "new prompt", Decision::Deny).await.0.unwrap();
    let seen = mock.seen.lock().unwrap();
    let messages: Vec<Message> = serde_json::from_str(&seen[0].1).unwrap();
    let texts: Vec<&str> = messages
        .iter()
        .skip(1)
        .filter_map(|m| match &m.content[0] {
            Block::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(texts, ["old instruction", "new prompt"]);
}

/// Runs a prompt, answering every permission request with `decision`.
async fn run(a: &mut Agent, prompt: &str, decision: Decision) -> (anyhow::Result<()>, Vec<Event>) {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let fut = a.prompt(prompt.into(), &tx);
    tokio::pin!(fut);
    let mut events = Vec::new();
    let result = loop {
        tokio::select! {
            r = &mut fut => break r,
            Some(ev) = rx.recv() => match ev {
                Event::Permission { reply, tool, summary } => {
                    let _ = reply.send(decision);
                    let (reply, _) = tokio::sync::oneshot::channel();
                    events.push(Event::Permission { reply, tool, summary });
                }
                ev => events.push(ev),
            }
        }
    };
    while let Ok(ev) = rx.try_recv() {
        events.push(ev);
    }
    (result, events)
}

fn results(events: &[Event]) -> Vec<(String, bool)> {
    events
        .iter()
        .filter_map(|e| {
            if let Event::ToolEnd {
                output, is_error, ..
            } = e
            {
                Some((output.clone(), *is_error))
            } else {
                None
            }
        })
        .collect()
}

#[test]
fn anthropic_stream() {
    let mut acc = anthropic::Acc::default();
    let mut text = String::new();
    for ev in [
        json!({"type":"message_start","message":{"usage":{"input_tokens":5,"cache_read_input_tokens":100,"cache_creation_input_tokens":7}}}),
        json!({"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"hm"}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"sig"}}),
        json!({"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}),
        json!({"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"Hel"}}),
        json!({"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"lo"}}),
        json!({"type":"content_block_start","index":2,"content_block":{"type":"tool_use","id":"t1","name":"read","input":{}}}),
        json!({"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"{\"path\":"}}),
        json!({"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"\"a.txt\"}"}}),
        json!({"type":"content_block_start","index":3,"content_block":{"type":"tool_use","id":"t2","name":"read","input":{}}}),
        json!({"type":"content_block_delta","index":3,"delta":{"type":"input_json_delta","partial_json":"{\"path\":\"cut"}}),
        json!({"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":9}}),
    ] {
        if let Some(t) = acc.feed(&ev).unwrap() {
            text.push_str(&t);
        }
    }
    let r = acc.finish();
    assert_eq!(text, "Hello");
    assert_eq!(r.stop, "tool_use");
    assert_eq!(
        (
            r.usage.input,
            r.usage.cache_read,
            r.usage.cache_write,
            r.usage.output
        ),
        (5, 100, 7, 9)
    );
    // Thinking is kept verbatim so it can be replayed unchanged.
    assert_eq!(
        serde_json::to_value(&r.blocks[0]).unwrap(),
        json!({"type":"thinking","thinking":"hm","signature":"sig"})
    );
    assert!(matches!(&r.blocks[1], Block::Text { text } if text == "Hello"));
    assert!(matches!(&r.blocks[2], Block::ToolUse { input, .. } if input["path"] == "a.txt"));
    assert!(
        matches!(&r.blocks[3], Block::ToolUse { input, .. } if input.get("__invalid_json").is_some())
    );
    assert!(acc_error().is_err());
}

fn acc_error() -> anyhow::Result<Option<String>> {
    anthropic::Acc::default().feed(&json!({"type":"error","error":{"type":"overloaded_error"}}))
}

#[test]
fn openai_stream_and_conversion() {
    let mut acc = openai::Acc::default();
    for ev in [
        json!({"choices":[{"delta":{"content":"Hi"}}]}),
        json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c1","function":{"name":"bash","arguments":"{\"comm"}}]}}]}),
        json!({"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"and\":\"ls\"}"}}]},"finish_reason":"tool_calls"}]}),
        json!({"choices":[],"usage":{"prompt_tokens":50,"completion_tokens":4,"prompt_tokens_details":{"cached_tokens":30}}}),
    ] {
        acc.feed(&ev).unwrap();
    }
    let r = acc.finish();
    assert_eq!(r.stop, "tool_use");
    assert_eq!(
        (r.usage.input, r.usage.cache_read, r.usage.output),
        (20, 30, 4)
    );
    assert!(
        matches!(&r.blocks[1], Block::ToolUse { id, name, input } if id == "c1" && name == "bash" && input["command"] == "ls")
    );

    let history = vec![
        Message::user("list"),
        Message {
            role: Role::Assistant,
            content: r.blocks,
        },
        Message {
            role: Role::User,
            content: vec![Block::ToolResult {
                tool_use_id: "c1".into(),
                content: "a\nb".into(),
                is_error: false,
            }],
        },
    ];
    let out = openai::convert("sys", &history);
    assert_eq!(out[0], json!({"role":"system","content":"sys"}));
    assert_eq!(out[1], json!({"role":"user","content":"list"}));
    assert_eq!(
        out[2]["tool_calls"][0]["function"]["arguments"],
        "{\"command\":\"ls\"}"
    );
    assert_eq!(
        out[3],
        json!({"role":"tool","tool_call_id":"c1","content":"a\nb"})
    );
    assert_eq!(out.len(), 4);
}

#[test]
fn openai_cache_writes_are_not_uncached_input() {
    let mut acc = openai::Acc::default();
    acc.feed(
        &json!({"choices":[],"usage":{"prompt_tokens":1000,"completion_tokens":7,
        "prompt_tokens_details":{"cached_tokens":600,"cache_write_tokens":300}}}),
    )
    .unwrap();
    let u = acc.finish().usage;
    assert_eq!(
        (u.input, u.cache_read, u.cache_write, u.context()),
        (100, 600, 300, 1000)
    );
}

/// The native prompt is sent on every request, so keep it lean.
#[test]
fn native_prompt_stays_lean() {
    let system_len = greencode_core::context::base_prompt().len();
    let tools = tools::defs(false, &[]);
    let schemas: usize = tools
        .iter()
        .map(|t| t.name.len() + t.description.len() + t.schema.to_string().len())
        .sum();
    assert!(
        system_len < 1_200,
        "system prompt grew to {system_len} bytes"
    );
    // Default tools include `browser` everywhere and `computer` on macOS (about 2.4 KB together).
    assert!(schemas < 8_000, "tool schemas grew to {schemas} bytes");
    assert!(system_len + schemas < 9_000);
}

#[tokio::test]
async fn lsp_definition_uses_the_language_server() {
    if greencode_core::config::executable("rust-analyzer").is_none() {
        return;
    }
    let root = dir("lsp-definition");
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"lsp_fixture\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    std::fs::write(
        root.join("src/lib.rs"),
        "pub fn helper() -> u8 { 1 }\npub fn caller() -> u8 { helper() }\n",
    )
    .unwrap();
    let output = tools::run(
        "lsp",
        &json!({"action":"definition","path":"src/lib.rs","line":2,"column":25}),
        &root,
    )
    .await
    .unwrap();
    assert!(output.contains("src/lib.rs:1"), "{output}");
}

#[tokio::test]
async fn lsp_resyncs_an_open_document_after_an_edit() {
    if greencode_core::config::executable("rust-analyzer").is_none() {
        return;
    }
    let root = dir("lsp-resync");
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"lsp_resync\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    std::fs::write(
        root.join("src/lib.rs"),
        "pub fn helper() -> u8 { 1 }\npub fn caller() -> u8 { helper() }\n",
    )
    .unwrap();
    // Opening the document (diagnostics) puts version 1 in the server's overlay.
    tools::run(
        "lsp",
        &json!({"action":"diagnostics","path":"src/lib.rs"}),
        &root,
    )
    .await
    .unwrap();
    // The file moves on disk: `helper` is now on line 3. A stale overlay would answer line 1.
    std::fs::write(
        root.join("src/lib.rs"),
        "// moved\n// down\npub fn helper() -> u8 { 1 }\npub fn caller() -> u8 { helper() }\n",
    )
    .unwrap();
    let output = tools::run(
        "lsp",
        &json!({"action":"definition","path":"src/lib.rs","line":4,"column":25}),
        &root,
    )
    .await
    .unwrap();
    assert!(
        output.contains("src/lib.rs:3"),
        "definition follows the edited text: {output}"
    );
}

#[tokio::test]
async fn lsp_diagnostics_report_a_real_error() {
    if greencode_core::config::executable("rust-analyzer").is_none() {
        return;
    }
    let root = dir("lsp-diagnostics");
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"lsp_diag\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    std::fs::write(
        root.join("src/lib.rs"),
        "pub fn broken() -> u8 { \"text\" }\n",
    )
    .unwrap();
    let output = tools::run(
        "lsp",
        &json!({"action":"diagnostics","path":"src/lib.rs"}),
        &root,
    )
    .await
    .unwrap();
    assert!(
        output.contains("error"),
        "a type error is reported: {output}"
    );
    // After an edit the server must speak again: the same file, fixed, is clean.
    std::fs::write(root.join("src/lib.rs"), "pub fn fixed() -> u8 { 1 }\n").unwrap();
    let output = tools::run(
        "lsp",
        &json!({"action":"diagnostics","path":"src/lib.rs"}),
        &root,
    )
    .await
    .unwrap();
    assert_eq!(output, "no diagnostics", "diagnostics follow the edit");

    // A fresh project with a clean file: the server publishes an empty list, which is a clean result.
    let clean = dir("lsp-diagnostics-clean");
    std::fs::create_dir_all(clean.join("src")).unwrap();
    std::fs::write(
        clean.join("Cargo.toml"),
        "[package]\nname = \"lsp_clean\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    std::fs::write(clean.join("src/lib.rs"), "pub fn fixed() -> u8 { 1 }\n").unwrap();
    let output = tools::run(
        "lsp",
        &json!({"action":"diagnostics","path":"src/lib.rs"}),
        &clean,
    )
    .await
    .unwrap();
    assert_eq!(output, "no diagnostics");
}

#[tokio::test]
async fn lsp_reports_a_missing_server() {
    let error = tools::run(
        "lsp",
        &json!({"action":"diagnostics","path":"example.txt"}),
        &dir("lsp-missing"),
    )
    .await
    .unwrap_err();
    assert!(error.to_string().contains("no language server"), "{error}");
}

#[test]
fn deepseek_requests_clamp_output_and_read_cache_hits() {
    let api = openai::OpenAi {
        http: Default::default(),
        base: "https://api.deepseek.com/v1".into(),
        key: String::new(),
        copilot: false,
        effort: None,
        wire: Default::default(),
    };
    let messages = vec![Message::user("hi")];
    let tools = tools::defs(false, &[]);
    let req = Request {
        model: "deepseek-chat",
        system: "stable",
        messages: &messages,
        tools: &tools,
        max_tokens: 64_000,
        live: None,
    };
    let body = api.body(&req);
    assert_eq!(
        body["max_tokens"], 8_192,
        "DeepSeek rejects larger output budgets"
    );
    assert!(body.get("max_completion_tokens").is_none());
    // DeepSeek reports prefix-cache hits separately from `prompt_tokens_details`.
    let mut acc = openai::Acc::default();
    acc.feed(&json!({"choices":[],"usage":{"prompt_tokens":100,"completion_tokens":5,"prompt_cache_hit_tokens":60}})).unwrap();
    let u = acc.finish().usage;
    assert_eq!((u.input, u.cache_read, u.output), (40, 60, 5));
}

#[test]
fn deepseek_reasoning_is_kept_as_a_thinking_block() {
    let mut acc = openai::Acc::default();
    acc.feed(&json!({"choices":[{"delta":{"reasoning_content":"think "}}]}))
        .unwrap();
    acc.feed(&json!({"choices":[{"delta":{"reasoning_content":"more"}}]}))
        .unwrap();
    acc.feed(&json!({"choices":[{"delta":{"content":"answer"},"finish_reason":"stop"}]}))
        .unwrap();
    let reply = acc.finish();
    assert!(
        matches!(&reply.blocks[0], greencode_core::Block::Other(v) if v["type"] == "thinking" && v["thinking"] == "think more")
    );
    assert!(matches!(&reply.blocks[1], greencode_core::Block::Text { text } if text == "answer"));
}

#[test]
fn openai_cache_controls_do_not_leak_to_compatible_servers() {
    let mut api = openai::OpenAi {
        http: Default::default(),
        base: "https://api.openai.com/v1".into(),
        key: String::new(),
        copilot: false,
        effort: None,
        wire: Default::default(),
    };
    let messages = vec![Message::user("hi")];
    let tools = tools::defs(false, &[]);
    for model in ["gpt-6-luna", "gpt-5.6", "gpt-5.6-2026-06-01"] {
        let req = Request {
            model,
            system: "stable",
            messages: &messages,
            tools: &tools,
            max_tokens: 123,
            live: None,
        };
        let body = api.body(&req);
        assert_eq!(body["prompt_cache_options"]["mode"], "implicit");
        assert_eq!(
            body["messages"][0]["content"][0]["prompt_cache_breakpoint"]["mode"],
            "explicit"
        );
        assert_eq!(body["max_completion_tokens"], 123);
        assert!(body.get("max_tokens").is_none());
        assert_eq!(body["messages"][1], json!({"role":"user","content":"hi"}));
    }
    let req = Request {
        model: "gpt-5",
        system: "stable",
        messages: &messages,
        tools: &tools,
        max_tokens: 123,
        live: None,
    };
    let legacy = api.body(&req);
    assert!(legacy.get("prompt_cache_options").is_none());
    assert_eq!(legacy["prompt_cache_key"], "greencode");
    assert_eq!(legacy["messages"][0]["content"], "stable");
    for base in [
        "https://example.com/v1",
        "https://api.openai.com.example.com/v1",
    ] {
        api.base = base.into();
        let proxy = api.body(&Request {
            model: "gpt-6-luna",
            ..req
        });
        assert!(proxy.get("prompt_cache_options").is_none());
        assert!(proxy.get("prompt_cache_key").is_none());
        assert_eq!(proxy["max_tokens"], 123);
        assert_eq!(proxy["messages"][0]["content"], "stable");
    }
}

#[tokio::test]
async fn file_tools() {
    let d = dir("file_tools");
    tools::run(
        "write",
        &json!({"path": "a.txt", "content": "one\ntwo\nthree\ntwo\n"}),
        &d,
    )
    .await
    .unwrap();
    assert_eq!(
        tools::run(
            "read",
            &json!({"path": "a.txt", "offset": 2, "limit": 2}),
            &d
        )
        .await
        .unwrap(),
        "two\nthree"
    );
    // Ambiguous and missing matches must not write.
    assert!(tools::run(
        "edit",
        &json!({"path": "a.txt", "old_string": "two", "new_string": "2"}),
        &d
    )
    .await
    .is_err());
    assert!(tools::run(
        "edit",
        &json!({"path": "a.txt", "old_string": "nine", "new_string": "9"}),
        &d
    )
    .await
    .is_err());
    tools::run(
        "edit",
        &json!({"path": "a.txt", "old_string": "three", "new_string": "3"}),
        &d,
    )
    .await
    .unwrap();
    tools::run(
        "edit",
        &json!({"path": "a.txt", "old_string": "two", "new_string": "2", "replace_all": true}),
        &d,
    )
    .await
    .unwrap();
    assert_eq!(
        std::fs::read_to_string(d.join("a.txt")).unwrap(),
        "one\n2\n3\n2\n"
    );
    assert!(
        tools::run("bash", &json!({"command": "echo hi; exit 3"}), &d)
            .await
            .unwrap_err()
            .to_string()
            .contains("[exit 3]")
    );
    assert!(
        tools::run("bash", &json!({"command": "sleep 5", "timeout": 1}), &d)
            .await
            .unwrap_err()
            .to_string()
            .contains("timed out")
    );
}

#[tokio::test]
async fn memory_tool() {
    let d = dir("memory_tool");
    let m = |v: Value| {
        let d = d.clone();
        async move { tools::run("memory", &v, &d).await }
    };
    m(json!({"action": "add", "scope": "project", "text": "uses pnpm"}))
        .await
        .unwrap();
    m(json!({"action": "add", "scope": "project", "text": "tests: cargo test"}))
        .await
        .unwrap();
    m(json!({"action": "replace", "scope": "project", "old": "pnpm", "text": "uses bun"}))
        .await
        .unwrap();
    m(json!({"action": "remove", "scope": "project", "old": "cargo"}))
        .await
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(d.join(".greencode/MEMORY.md")).unwrap(),
        "- uses bun\n"
    );
    assert!(context::session_context(&d, &Config::default()).contains("- uses bun"));
    let big = "x".repeat(context::MEMORY_LIMIT);
    assert!(m(json!({"action": "add", "scope": "project", "text": big}))
        .await
        .is_err());
}

#[test]
fn config_defaults_enable_learning_and_keep_tools_unrestricted() {
    let cfg = Config::default();
    assert!(cfg.learning.enabled);
    assert!(cfg.learning.auto);
    assert!(cfg.tools.is_empty());
    assert!(cfg.compaction_enabled);
    assert_eq!(cfg.max_tool_calls, 0);
}

#[test]
fn config_reads_native_omp_controls_and_roles() {
    let cfg: Config = toml::from_str(
        r#"
model = "openai/default"
model_roles = { task = "openai/task", advisor = "openai/advisor" }
max_tool_calls = 3
compaction_enabled = false
tools = ["read", "ext"]
context_files = false
[learning]
enabled = true
auto = true
model = "openai/learn"
scope = "global"
max_chars = 120
"#,
    )
    .unwrap();
    assert_eq!(cfg.role_or_default("task"), "openai/task");
    assert_eq!(cfg.role_or_default("missing"), "openai/default");
    assert_eq!(cfg.role_model("advisor", None), Some("openai/advisor"));
    assert_eq!(cfg.max_tool_calls, 3);
    assert!(!cfg.compaction_enabled && !cfg.context_files);
    assert_eq!(cfg.learning.model.as_deref(), Some("openai/learn"));
    assert_eq!(cfg.learning.scope, "global");
    assert_eq!(cfg.learning.max_chars, 120);
}

#[test]
fn truncation_keeps_both_ends() {
    dir("trunc");
    let long = format!("START{}END", "é".repeat(40_000));
    let (out, spill) = context::truncate(long.clone(), "id/1");
    assert!(out.len() < 25_000 && out.starts_with("START") && out.ends_with("END"));
    let path = spill.expect("a long result spills to a file");
    assert_eq!(std::fs::read_to_string(path).unwrap(), long);
    assert_eq!(
        context::truncate("short".into(), "x"),
        ("short".to_string(), None)
    );
}

#[test]
fn permissions() {
    let d = dir("perms");
    let ask = Perms::new(Mode::Ask);
    assert_eq!(ask.check("read", &json!({"path": "a"}), &d), None);
    assert!(ask
        .check("read", &json!({"path": "/etc/passwd"}), &d)
        .is_some());
    assert_eq!(
        ask.check("write", &json!({"path": "a"}), &d).as_deref(),
        Some("write")
    );
    assert_eq!(
        ask.check("bash", &json!({"command": "cargo test"}), &d)
            .as_deref(),
        Some("bash:cargo")
    );
    // A program-name grant must not cover a chained command.
    ask.always("bash:cargo".into());
    assert_eq!(
        ask.check("bash", &json!({"command": "cargo build"}), &d),
        None
    );
    assert!(ask
        .check("bash", &json!({"command": "cargo build; rm -rf /"}), &d)
        .is_some());
    let auto = Perms::new(Mode::AutoEdit);
    assert_eq!(auto.check("edit", &json!({"path": "src/a.rs"}), &d), None);
    assert!(auto
        .check("edit", &json!({"path": "../../../outside"}), &d)
        .is_some());
    assert!(auto
        .check("write", &json!({"path": "/tmp/x"}), &d)
        .is_some());
    assert!(auto
        .check("edit", &json!({"path": ".git/config"}), &d)
        .is_some());
    assert!(auto
        .check("edit", &json!({"path": ".GIT/config"}), &d)
        .is_some());
    // Commands that only look never ask; anything that writes, chains with `;` or leaves the project does.
    for cmd in [
        "ls",
        "ls -la",
        "git status",
        "git diff HEAD~1 | head -20",
        "cat a.txt && wc -l a.txt",
        "find . -name x.rs",
        "cd src && ls",
        "git status --short; git log --oneline -3",
        "sort -nr a",
        "uniq a",
    ] {
        assert_eq!(
            auto.check("bash", &json!({"command": cmd}), &d),
            None,
            "{cmd}"
        );
    }
    // Quoting, expansion, glued or abbreviated write options and home-relative paths all ask.
    for cmd in [
        "ls; rm x",
        "cat a > b",
        "find . -delete",
        "git push",
        "git branch -D x",
        "cat /etc/passwd",
        "ls /etc",
        "ls ../../../../../../etc",
        "echo $(rm x)",
        "sort -o a a",
        "rg --pre sh x",
        "ls && rm x",
        "python3 hello.py",
        "git grep -Oecho -e x",
        "git grep -nOecho x",
        "find . \"-delete\"",
        "find . -d''elete",
        "git diff --output=a",
        "git diff --out=a",
        "sort -oa x",
        "sort -roa x",
        "sort --out=a x",
        "sort --compress-program=sh x",
        "uniq a b",
        "tree -o a",
        "cat .{.,.}/x",
        "cat ~root/x",
        "ls ~",
        "cd",
        "cd; cat x",
        "ls .*",
        "diff --from-file=/etc/passwd a",
    ] {
        assert!(
            auto.check("bash", &json!({"command": cmd}), &d).is_some(),
            "{cmd}"
        );
    }
    assert_eq!(
        Perms::new(Mode::Full).check("bash", &json!({"command": "rm x"}), &d),
        None
    );
}

#[tokio::test]
async fn loop_runs_tools_and_keeps_prefix_stable() {
    let mock = Mock::new(vec![
        vec![
            text("writing"),
            call("t1", "write", json!({"path": "hello.txt", "content": "hi"})),
        ],
        vec![text("done")],
        vec![text("again")],
    ]);
    let (mut a, meta) = agent("loop", Mode::Full, &mock);
    let (r, events) = run(&mut a, "make hello.txt", Decision::Deny).await;
    r.unwrap();
    assert_eq!(
        std::fs::read_to_string(meta.cwd.join("hello.txt")).unwrap(),
        "hi"
    );
    assert_eq!(results(&events), vec![("wrote 2 bytes".to_string(), false)]);
    run(&mut a, "second turn", Decision::Deny).await.0.unwrap();

    // Cache safety: system+tools never change and history only grows.
    let seen = mock.seen.lock().unwrap();
    assert_eq!(seen.len(), 3);
    for w in seen.windows(2) {
        assert_eq!(w[0].0, w[1].0);
        assert!(w[1].1.starts_with(w[0].1.trim_end_matches(']')));
    }
    drop(seen);

    // The session file restores the same history.
    let (saved, messages) = Session::load(&meta.id).unwrap();
    assert_eq!(saved.title, "make hello.txt");
    assert_eq!(
        serde_json::to_string(&messages).unwrap(),
        serde_json::to_string(&a.messages).unwrap()
    );
    assert!(Session::list().iter().any(|m| m.id == meta.id));
}

#[tokio::test]
async fn denied_and_invalid_calls_become_error_results() {
    let mock = Mock::new(vec![
        vec![
            call("t1", "bash", json!({"command": "cargo build"})),
            call("t2", "read", json!({"__invalid_json": "{"})),
        ],
        vec![text("ok")],
    ]);
    let (mut a, _) = agent("denied", Mode::Ask, &mock);
    let (r, events) = run(&mut a, "go", Decision::Deny).await;
    r.unwrap();
    let res = results(&events);
    assert!(res.iter().all(|(_, err)| *err));
    assert!(
        res.iter().any(|(o, _)| o.contains("denied"))
            && res.iter().any(|(o, _)| o.contains("INVALID_JSON"))
    );
    // Both results go back in one user message.
    assert_eq!(a.messages[2].content.len(), 2);
}

#[tokio::test]
async fn interrupted_tool_calls_are_repaired() {
    let mock = Mock::new(vec![vec![text("ok")]]);
    let (mut a, _) = agent("repair", Mode::Full, &mock);
    a.messages.push(Message::user("x"));
    a.messages.push(Message {
        role: Role::Assistant,
        content: vec![call("t9", "bash", json!({"command": "sleep 100"}))],
    });
    run(&mut a, "next", Decision::Deny).await.0.unwrap();
    assert!(
        matches!(&a.messages[2].content[0], Block::ToolResult { tool_use_id, is_error: true, .. } if tool_use_id == "t9")
    );
}

#[tokio::test]
async fn lua_tool_hook_and_command() {
    let mock = Mock::new(vec![
        vec![
            call("t1", "shout", json!({"s": "abc"})),
            call("t2", "bash", json!({"command": "echo forbidden"})),
        ],
        vec![text("ok")],
    ]);
    let (mut a, _) = agent("lua", Mode::Full, &mock);
    let (r, events) = run(&mut a, "/hi there", Decision::Deny).await;
    r.unwrap();
    assert!(matches!(&a.messages[0].content[0], Block::Text { text } if text == "hello there"));
    let res = results(&events);
    assert!(res.contains(&("ABC".to_string(), false)));
    assert!(res.iter().any(|(o, err)| *err && o.contains("nope")));
    assert!(mock.seen.lock().unwrap()[0].0.contains("\"shout\""));
}

fn advertised(events: &[Event]) -> Vec<String> {
    events
        .iter()
        .rev()
        .find_map(|e| match e {
            Event::Remote { command, data } if command == "get_commands" => Some(
                data["commands"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|c| c["name"].as_str().unwrap().to_string())
                    .collect(),
            ),
            _ => None,
        })
        .unwrap()
}

async fn commands_of(a: &mut Agent) -> Vec<Event> {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    Backend::control(a, json!({"type": "get_commands"}), &tx)
        .await
        .unwrap();
    std::iter::from_fn(move || rx.try_recv().ok()).collect()
}

#[tokio::test]
async fn native_commands_are_advertised_and_untrusted_extensions_stay_unloaded() {
    let mock = Mock::new(vec![vec![text("ok")], vec![text("ok")]]);
    let (mut a, meta) = agent("lua-commands", Mode::Full, &mock);
    let marker = meta.cwd.join("loaded");
    std::fs::create_dir_all(meta.cwd.join(".greencode/commands")).unwrap();
    std::fs::create_dir_all(meta.cwd.join(".greencode/extensions")).unwrap();
    std::fs::write(
        meta.cwd.join(".greencode/commands/proj.md"),
        "do $ARGUMENTS",
    )
    .unwrap();
    std::fs::write(
        meta.cwd.join(".greencode/extensions/p.lua"),
        format!(
            "gc.exec('touch {}')\ngc.command('projext', function(a) return 'ran ' .. a end)",
            marker.display()
        ),
    )
    .unwrap();
    let names = advertised(&commands_of(&mut a).await);
    assert!(
        ["compact", "proj", "hi"]
            .iter()
            .all(|n| names.contains(&n.to_string())),
        "{names:?}"
    );
    assert!(
        !names.contains(&"projext".to_string()) && !marker.exists(),
        "advertising must not run untrusted code"
    );
    // Denying approval keeps the extension unloaded; the command is then plain prompt text.
    let (r, events) = run(&mut a, "/projext x", Decision::Deny).await;
    r.unwrap();
    assert!(events
        .iter()
        .any(|e| matches!(e, Event::Permission { tool, .. } if tool == "load project extensions")));
    assert!(!marker.exists() && !advertised(&events).contains(&"projext".to_string()));
    assert!(matches!(&a.messages[0].content[0], Block::Text { text } if text == "/projext x"));
    // Approval loads it, advertises it, and the command reaches the extension handler.
    let (r, events) = run(&mut a, "/projext y", Decision::Allow).await;
    r.unwrap();
    assert!(marker.exists() && advertised(&events).contains(&"projext".to_string()));
    assert!(a
        .messages
        .iter()
        .any(|m| matches!(&m.content[0], Block::Text { text } if text == "ran y")));
    assert!(advertised(&commands_of(&mut a).await).contains(&"projext".to_string()));
}

#[tokio::test]
async fn subagent_returns_only_its_answer() {
    let mock = Mock::new(vec![
        vec![call(
            "t1",
            "task",
            json!({"tasks":[{"prompt": "look around"}]}),
        )],
        vec![text("sub answer")],
        vec![text("final")],
    ]);
    for reply in mock.replies.lock().unwrap().iter_mut() {
        reply.usage.input = 100;
    }
    let (mut a, _) = agent("sub", Mode::Full, &mock);
    let (r, events) = run(&mut a, "delegate", Decision::Deny).await;
    r.unwrap();
    assert_eq!(results(&events), vec![("sub answer".to_string(), false)]);
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, Event::Usage(_)))
            .count(),
        2
    );
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, Event::UsageAux(_)))
            .count(),
        1
    );
    let seen = mock.seen.lock().unwrap();
    assert!(seen[1].1.contains("look around") && !seen[1].1.contains("delegate"));
    assert_eq!(*mock.models.lock().unwrap(), ["m", "m", "m"]);
}

#[tokio::test]
async fn task_with_backend_runs_as_a_child_thread_on_that_harness() {
    if std::process::Command::new("python3").arg("--version").output().is_err() {
        return;
    }
    let d = dir("task-backend");
    std::fs::write(d.join("child.py"), r#"
import json, sys
def send(m): print(json.dumps(m), flush=True)
for line in sys.stdin:
    m = json.loads(line)
    meth = m.get("method")
    if meth == "initialize": send({"jsonrpc": "2.0", "id": m["id"], "result": {"protocolVersion": 1}})
    elif meth == "session/new": send({"jsonrpc": "2.0", "id": m["id"], "result": {"sessionId": "c"}})
    elif meth == "session/prompt":
        text = m["params"]["prompt"][0]["text"]
        send({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": "c", "update": {"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "child saw " + text.rsplit("\n", 1)[-1]}}}})
        send({"jsonrpc": "2.0", "id": m["id"], "result": {"stopReason": "end_turn"}})
"#).unwrap();
    let mock = Mock::new(vec![
        vec![call("t1", "task", json!({"tasks":[{"prompt": "count files", "backend": "child", "name": "kid"}]}))],
        vec![text("final")],
    ]);
    let mut cfg = Config::default();
    cfg.learning.auto = false;
    cfg.backends.push(greencode_core::config::AcpCfg { name: "child".into(), command: vec!["python3".into(), "child.py".into()], env: Default::default(), protocol: Default::default() });
    let meta = Meta::new(d, "mock/m".into(), "native".into());
    let mut a = Agent::with_provider(Arc::new(cfg), &meta, Arc::new(Perms::new(Mode::Full)), mock.clone(), "m".into());
    let (r, events) = run(&mut a, "delegate", Decision::Deny).await;
    r.unwrap();
    assert_eq!(results(&events), vec![("child saw count files".to_string(), false)]);
    let child = events.iter().find_map(|e| match e {
        Event::Remote { command, data } if command == "subagent" => data["task"].as_str().map(str::to_string),
        _ => None,
    }).expect("a delegated row names its child thread");
    let (child_meta, _) = Session::load(&child).unwrap();
    assert_eq!((child_meta.parent, child_meta.backend.as_str()), (Some(meta.id.clone()), "child"));
    // Only the parent's two requests went to the native model.
    assert_eq!(mock.seen.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn subagent_models_can_be_selected_overridden_and_reset_without_losing_context() {
    let mock = Mock::new(vec![
        vec![call(
            "t1",
            "task",
            json!({"tasks":[{"prompt": "first task"}]}),
        )],
        vec![text("first answer")],
        vec![text("first final")],
        vec![call(
            "t2",
            "task",
            json!({"tasks":[{"prompt": "second task", "model": "mock/override"}]}),
        )],
        vec![text("second answer")],
        vec![text("second final")],
        vec![call(
            "t3",
            "task",
            json!({"tasks":[{"prompt": "third task"}]}),
        )],
        vec![text("third answer")],
        vec![text("third final")],
    ]);
    let (mut a, meta) = agent("sub-models", Mode::Full, &mock);
    a.set_subagent_model(Some("mock/default".into())).unwrap();
    run(&mut a, "one", Decision::Deny).await.0.unwrap();
    assert_eq!(
        Session::load(&meta.id).unwrap().0.subagent_model.as_deref(),
        Some("mock/default")
    );
    run(&mut a, "two", Decision::Deny).await.0.unwrap();
    let history = serde_json::to_value(&a.messages).unwrap();
    a.set_subagent_model(None).unwrap();
    assert_eq!(serde_json::to_value(&a.messages).unwrap(), history);
    assert_eq!(Session::load(&meta.id).unwrap().0.subagent_model, None);
    run(&mut a, "three", Decision::Deny).await.0.unwrap();
    assert_eq!(
        *mock.models.lock().unwrap(),
        ["m", "default", "m", "m", "override", "m", "m", "m", "m"]
    );
}

#[tokio::test]
async fn invalid_subagent_models_fail_as_tool_errors_without_running_a_subagent() {
    for (i, model) in [
        json!(null),
        json!(7),
        json!(""),
        json!("bare"),
        json!("/model"),
        json!("mock/"),
        json!("mock/with space"),
        json!("missing/model"),
    ]
    .into_iter()
    .enumerate()
    {
        let mock = Mock::new(vec![
            vec![call(
                "t1",
                "task",
                json!({"tasks":[{"prompt": "inspect", "model": model}]}),
            )],
            vec![text("handled")],
        ]);
        let (mut a, _) = agent(&format!("sub-invalid-{i}"), Mode::Full, &mock);
        let (result, events) = run(&mut a, "delegate", Decision::Deny).await;
        result.unwrap();
        assert!(results(&events)[0].1, "invalid model accepted: {model}");
        assert_eq!(*mock.models.lock().unwrap(), ["m", "m"]);
    }
}

#[test]
fn subagent_model_schema_and_legacy_settings() {
    let config: Config = toml::from_str("subagent_model = 'openai/gpt-6-luna'").unwrap();
    assert_eq!(config.subagent_model.as_deref(), Some("openai/gpt-6-luna"));
    assert!(toml::from_str::<Config>("")
        .unwrap()
        .subagent_model
        .is_none());
    let meta = Meta::new(dir("legacy-sub-model"), "mock/m".into(), "native".into());
    let mut legacy = serde_json::to_value(&meta).unwrap();
    legacy.as_object_mut().unwrap().remove("subagent_model");
    assert!(serde_json::from_value::<Meta>(legacy)
        .unwrap()
        .subagent_model
        .is_none());
    let defs = tools::defs(false, &[]);
    let task = defs.iter().find(|tool| tool.name == "task").unwrap();
    assert_eq!(
        task.schema["properties"]["tasks"]["items"]["properties"]["model"]["type"],
        "string"
    );
    assert_eq!(task.schema["required"], json!(["tasks"]));
    assert!(tools::defs(true, &[])
        .iter()
        .all(|tool| tool.name != "task"));
}

#[tokio::test]
async fn task_batch_runs_in_parallel_and_labels_results() {
    let mock = Mock::routed(
        vec![
            vec![call(
                "t",
                "task",
                json!({"context":"shared contract", "tasks":[{"name":"A","prompt":"marker-alpha"},{"name":"B","prompt":"marker-beta"}]}),
            )],
            vec![text("done")],
        ],
        vec![
            ("marker-alpha", vec![vec![text("answer alpha")]]),
            ("marker-beta", vec![vec![text("answer beta")]]),
        ],
    );
    let (mut a, _) = agent("batch-agents", Mode::Full, &mock);
    let (r, events) = run(&mut a, "delegate", Decision::Deny).await;
    r.unwrap();
    let output = &results(&events)[0].0;
    assert!(
        output.contains("## A (task)\nanswer alpha") && output.contains("## B (task)\nanswer beta")
    );
    let seen = mock.seen.lock().unwrap();
    assert_eq!(
        seen.iter()
            .filter(|(_, m)| m.contains("# Role: ") && m.contains("shared contract"))
            .count(),
        2
    );
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, Event::Usage(_)))
            .count(),
        2
    );
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, Event::UsageAux(_)))
            .count(),
        2
    );
}

#[tokio::test]
async fn sub_cancel_stops_one_sub_agent_and_keeps_the_other() {
    let mock = Mock::routed(
        vec![
            vec![call(
                "t",
                "task",
                json!({"tasks":[{"name":"a","prompt":"HANGME"},{"name":"b","prompt":"normal-marker"}]}),
            )],
            vec![text("done")],
        ],
        vec![("normal-marker", vec![vec![text("answer b")]])],
    );
    mock.hang.lock().unwrap().push("HANGME".into());
    let (mut a, _) = agent("sub-cancel-one", Mode::Full, &mock);
    let live = Backend::live(&a).unwrap();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        live.send(json!({"type":"sub_cancel","name":"a"})).unwrap();
    });
    let (result, events) = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        run(&mut a, "delegate", Decision::Deny),
    )
    .await
    .unwrap();
    result.unwrap();
    let output = &results(&events)[0].0;
    assert!(
        output.contains("a (task) failed")
            && output.contains("cancelled by the user")
            && output.contains("answer b")
    );
    for (name, state) in [("a", "cancelled"), ("b", "done")] {
        assert!(events.iter().any(|e| matches!(e, Event::Remote { command, data } if command == "subagent" && data["name"] == name && data["state"] == state)));
    }
}

#[tokio::test]
async fn subagent_limit_spans_batches_and_queued_tasks_can_be_cancelled() {
    let batch = |id, names: &[&str]| {
        call(
            id,
            "task",
            json!({"tasks": names.iter().map(|name| json!({"name":name,"prompt":name})).collect::<Vec<_>>()}),
        )
    };
    let mock = Mock::new(vec![
        vec![
            batch("a", &["A-block", "B-block"]),
            batch("b", &["C-block", "D-block", "E-block"]),
        ],
        vec![text("done")],
    ]);
    mock.hang.lock().unwrap().push("-block".into());
    let (mut a, _) = agent("global-subagent-limit", Mode::Full, &mock);
    let live = a.live().unwrap();
    let watched = mock.clone();
    let cancel = async move {
        loop {
            let n = watched
                .seen
                .lock()
                .unwrap()
                .iter()
                .filter(|(_, m)| m.contains("# Role: "))
                .count();
            if n >= 4 {
                assert_eq!(n, 4);
                break;
            }
            tokio::task::yield_now().await;
        }
        for name in ["E-block", "D-block", "C-block", "B-block", "A-block"] {
            live.send(json!({"type":"sub_cancel", "name":name}))
                .unwrap();
        }
    };
    let ((r, events), _) = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        tokio::join!(run(&mut a, "delegate", Decision::Deny), cancel)
    })
    .await
    .unwrap();
    r.unwrap();
    assert_eq!(
        mock.seen
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, m)| m.contains("# Role: "))
            .count(),
        4
    );
    assert_eq!(events.iter().filter(|e| matches!(e, Event::Remote { command, data } if command == "subagent" && data["state"] == "cancelled")).count(), 5);
}

#[tokio::test]
async fn sub_steer_reaches_the_named_sub_agent() {
    let mock = Mock::routed(
        vec![
            vec![call(
                "t",
                "task",
                json!({"tasks":[{"name":"a","prompt":"steer-marker"}]}),
            )],
            vec![text("parent done")],
        ],
        vec![(
            "steer-marker",
            vec![
                vec![call("r", "read", json!({"path":"missing.txt"}))],
                vec![text("sub done")],
            ],
        )],
    );
    let (mut a, _) = agent("sub-steer", Mode::Full, &mock);
    *mock.steer.lock().unwrap() = Some((
        2,
        Backend::live(&a).unwrap(),
        json!({"type":"sub_steer","name":"a","message":"steer instruction"}),
    ));
    let (result, events) = run(&mut a, "delegate", Decision::Deny).await;
    result.unwrap();
    let seen = mock.seen.lock().unwrap();
    let requests: Vec<_> = seen
        .iter()
        .filter(|(_, m)| m.contains("# Role: "))
        .collect();
    assert_eq!(requests.len(), 2);
    let messages: Vec<Message> = serde_json::from_str(&requests[1].1).unwrap();
    assert!(messages.iter().any(|m| m.role == Role::User
        && m.content
            .iter()
            .any(|b| matches!(b, Block::Text { text } if text == "steer instruction"))));
    assert!(events.iter().any(|e| matches!(e, Event::Remote { command, data } if command == "subagent" && data["name"] == "a" && data["state"] == "running" && data["detail"] == "read missing.txt")));
}

#[tokio::test]
async fn scout_cannot_write() {
    let mock = Mock::routed(
        vec![
            vec![call(
                "t",
                "task",
                json!({"tasks":[{"agent":"scout","prompt":"scout-marker"}]}),
            )],
            vec![text("done")],
        ],
        vec![(
            "scout-marker",
            vec![
                vec![call(
                    "w",
                    "write",
                    json!({"path":"new.txt","content":"bad"}),
                )],
                vec![text("scout answer")],
            ],
        )],
    );
    let (mut a, meta) = agent("scout-read-only", Mode::Full, &mock);
    run(&mut a, "delegate", Decision::Deny).await.0.unwrap();
    assert!(!meta.cwd.join("new.txt").exists());
    let seen = mock.seen.lock().unwrap();
    let requests: Vec<_> = seen
        .iter()
        .filter(|(_, m)| m.contains("# Role: "))
        .collect();
    assert!(
        requests[0].0.contains("\"read\"")
            && !requests[0].0.contains("\"write\"")
            && !requests[0].0.contains("\"task\"")
    );
    assert!(requests[1].1.contains("not available to this agent"));
}

#[tokio::test]
async fn task_background_results_are_delivered() {
    let mock = Mock::routed(
        vec![
            vec![call(
                "t",
                "task",
                json!({"background":true,"tasks":[{"name":"A","prompt":"background-marker"}]}),
            )],
            vec![text("waiting")],
            vec![text("reaction")],
        ],
        vec![("background-marker", vec![vec![text("background answer")]])],
    );
    let (mut a, _) = agent("background-agents", Mode::Full, &mock);
    let (r, events) = run(&mut a, "delegate", Decision::Deny).await;
    r.unwrap();
    assert!(results(&events)[0]
        .0
        .starts_with("Started in the background"));
    assert!(mock
        .seen
        .lock()
        .unwrap()
        .iter()
        .any(|(_, m)| !m.contains("# Role: ")
            && m.contains("Background task \\\"A\\\" (task) finished:")
            && m.contains("background answer")));
}

#[tokio::test]
async fn cancel_aborts_background_jobs_and_nothing_is_delivered_later() {
    let mock = Mock::new(vec![
        vec![call(
            "t1",
            "task",
            json!({"background":true,"tasks":[{"prompt":"HANGME work"}]}),
        )],
        vec![text("waiting")],
        vec![text("fresh")],
    ]);
    mock.hang.lock().unwrap().push("HANGME".into());
    let (mut a, _) = agent("cancel-background", Mode::Full, &mock);
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    assert!(tokio::time::timeout(
        std::time::Duration::from_millis(300),
        a.prompt("delegate".into(), &tx)
    )
    .await
    .is_err());
    assert!(mock
        .seen
        .lock()
        .unwrap()
        .iter()
        .any(|(_, messages)| messages.contains("# Role: ") && messages.contains("HANGME")));
    a.cancel(&tx).await.unwrap();
    let after_cancel = mock.seen.lock().unwrap().len();
    let (result, events) = run(&mut a, "new request", Decision::Deny).await;
    result.unwrap();
    assert!(events
        .iter()
        .any(|event| matches!(event, Event::Text(s) if s == "fresh")));
    assert!(mock.seen.lock().unwrap()[after_cancel..]
        .iter()
        .all(|(_, messages)| !messages.contains("Background task")));
}

#[tokio::test]
async fn sub_cancel_stops_a_hung_background_sub_agent_while_the_turn_waits() {
    let mock = Mock::new(vec![
        vec![call(
            "t1",
            "task",
            json!({"background":true,"tasks":[{"name":"A","prompt":"HANGME work"}]}),
        )],
        vec![text("waiting")],
        vec![text("all done")],
    ]);
    mock.hang.lock().unwrap().push("HANGME".into());
    let (mut a, _) = agent("cancel-background-sub", Mode::Full, &mock);
    let live = Backend::live(&a).unwrap();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        live.send(json!({"type": "sub_cancel", "name": "A"}))
            .unwrap();
    });
    let (result, events) = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        run(&mut a, "delegate", Decision::Deny),
    )
    .await
    .expect("the turn must not hang on a cancelled background sub-agent");
    result.unwrap();
    assert!(events
        .iter()
        .any(|e| matches!(e, Event::Text(s) if s == "all done")));
    let seen = mock.seen.lock().unwrap();
    assert!(seen.last().unwrap().1.contains("cancelled by the user"));
}

#[tokio::test]
async fn steering_wakes_a_turn_waiting_for_background_tasks() {
    let mock = Mock::new(vec![
        vec![call(
            "t",
            "task",
            json!({"background":true,"tasks":[{"name":"A","prompt":"HANGME"}]}),
        )],
        vec![text("waiting")],
        vec![text("steering received")],
        vec![text("done")],
    ]);
    mock.hang.lock().unwrap().push("HANGME".into());
    let (mut a, _) = agent("steer-background-wait", Mode::Full, &mock);
    let live = a.live().unwrap();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let control = async {
        while let Some(ev) = rx.recv().await {
            match ev {
                Event::Text(s) if s == "waiting" => {
                    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                    live.send(json!({"type":"steer","message":"EXACT_WAIT_STEER"}))
                        .unwrap();
                }
                Event::Text(s) if s == "steering received" => {
                    live.send(json!({"type":"sub_cancel","name":"A"})).unwrap();
                    break;
                }
                _ => {}
            }
        }
    };
    let (r, ()) = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        tokio::join!(a.prompt("delegate".into(), &tx), control)
    })
    .await
    .unwrap();
    r.unwrap();
    let seen = mock.seen.lock().unwrap();
    assert!(seen
        .iter()
        .any(|(_, m)| m.contains("EXACT_WAIT_STEER") && !m.contains("cancelled by the user")));
    assert!(seen.last().unwrap().1.contains("cancelled by the user"));
}

#[tokio::test]
async fn task_schema_returns_submitted_json() {
    let schema = json!({"type":"object","properties":{"files":{"type":"array"},"count":{"type":"integer"}},"required":["files","count"]});
    let mock = Mock::routed(
        vec![
            vec![call(
                "t",
                "task",
                json!({"tasks":[{"agent":"scout","prompt":"schema-marker","schema":schema}]}),
            )],
            vec![text("done")],
        ],
        vec![(
            "schema-marker",
            vec![vec![call(
                "s",
                "submit",
                json!({"files":["a.rs"],"count":1}),
            )]],
        )],
    );
    let (mut a, _) = agent("schema-result", Mode::Ask, &mock);
    let (result, events) = run(&mut a, "delegate", Decision::Deny).await;
    result.unwrap();
    let out = results(&events);
    assert_eq!(out.len(), 1);
    assert!(!out[0].1);
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&out[0].0).unwrap(),
        json!({"files":["a.rs"],"count":1})
    );
    let seen = mock.seen.lock().unwrap();
    let requests: Vec<_> = seen
        .iter()
        .filter(|(_, m)| m.contains("# Role: "))
        .collect();
    assert_eq!(requests.len(), 1);
    assert!(
        requests[0].0.contains("\"submit\"") && requests[0].1.contains("Finish by calling submit")
    );
}

#[tokio::test]
async fn task_schema_rejects_bad_submit_then_accepts() {
    let schema = json!({"type":"object","properties":{"files":{"type":"array"},"count":{"type":"integer"}},"required":["files","count"]});
    let mock = Mock::routed(
        vec![
            vec![call(
                "t",
                "task",
                json!({"tasks":[{"prompt":"retry-schema-marker","schema":schema}]}),
            )],
            vec![text("done")],
        ],
        vec![(
            "retry-schema-marker",
            vec![
                vec![call("s1", "submit", json!({"files":["a.rs"]}))],
                vec![call("s2", "submit", json!({"files":["a.rs"],"count":"1"}))],
                vec![call("s3", "submit", json!({"files":["a.rs"],"count":1}))],
            ],
        )],
    );
    let (mut a, _) = agent("schema-retry", Mode::Full, &mock);
    let (result, events) = run(&mut a, "delegate", Decision::Deny).await;
    result.unwrap();
    let out = results(&events);
    assert_eq!(out.len(), 1);
    assert!(!out[0].1);
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&out[0].0).unwrap(),
        json!({"files":["a.rs"],"count":1})
    );
    let seen = mock.seen.lock().unwrap();
    let requests: Vec<_> = seen
        .iter()
        .filter(|(_, m)| m.contains("# Role: "))
        .collect();
    assert_eq!(requests.len(), 3);
    let messages: Vec<Message> = serde_json::from_str(&requests[1].1).unwrap();
    assert!(messages.iter().flat_map(|m| &m.content).any(|b| matches!(b, Block::ToolResult { content, is_error: true, .. } if content.contains("missing required field `count`"))));
    let messages: Vec<Message> = serde_json::from_str(&requests[2].1).unwrap();
    assert!(messages.iter().flat_map(|m| &m.content).any(|b| matches!(b, Block::ToolResult { content, is_error: true, .. } if content.contains("field `count` must be a integer"))));
}

#[tokio::test]
async fn task_schema_without_submit_fails() {
    let mock = Mock::routed(
        vec![
            vec![call(
                "t",
                "task",
                json!({"tasks":[{"prompt":"plain-schema-marker","schema":{"type":"object"}}]}),
            )],
            vec![text("done")],
        ],
        vec![("plain-schema-marker", vec![vec![text("plain answer")]])],
    );
    let (mut a, _) = agent("schema-no-submit", Mode::Full, &mock);
    let (result, events) = run(&mut a, "delegate", Decision::Deny).await;
    result.unwrap();
    assert!(results(&events)
        .iter()
        .any(|(output, error)| *error
            && output.contains("sub-agent finished without calling submit")));
}

#[tokio::test]
async fn task_rejects_bad_input() {
    let cases = vec![
        json!({"tasks":[]}),
        json!({"tasks":[{"name":"A","prompt":"a"},{"name":"A","prompt":"b"}]}),
        json!({"tasks":[{"agent":"nonexistent-agent","prompt":"a"}]}),
        json!({"tasks":vec![json!({"prompt":"a"}); 9]}),
        json!({"tasks":[{"prompt":"a","schema":"x"}]}),
        json!({"tasks":[{"prompt":"a","schema":{"type":"array"}}]}),
    ];
    for (index, input) in cases.into_iter().enumerate() {
        let mock = Mock::new(vec![vec![call("t", "task", input)], vec![text("handled")]]);
        let (mut a, _) = agent(&format!("bad-task-{index}"), Mode::Full, &mock);
        let (r, events) = run(&mut a, "delegate", Decision::Deny).await;
        r.unwrap();
        assert!(results(&events)[0].1);
        assert_eq!(mock.seen.lock().unwrap().len(), 2);
    }
}

#[cfg(unix)]
#[tokio::test]
async fn task_isolated_uses_a_worktree() {
    let mock = Mock::routed(
        vec![
            vec![call(
                "t",
                "task",
                json!({"tasks":[{"isolated":true,"prompt":"worktree-marker"}]}),
            )],
            vec![text("done")],
        ],
        vec![(
            "worktree-marker",
            vec![
                vec![call(
                    "w",
                    "write",
                    json!({"path":"out.txt","content":"worktree"}),
                )],
                vec![text("written")],
            ],
        )],
    );
    let (mut a, meta) = agent("isolated-task", Mode::Full, &mock);
    let git = |args: &[&str]| {
        let output = std::process::Command::new("git")
            .current_dir(&meta.cwd)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    };
    git(&["init"]);
    git(&[
        "-c",
        "user.name=Test",
        "-c",
        "user.email=test@example.com",
        "commit",
        "--allow-empty",
        "-m",
        "initial",
    ]);
    let (r, events) = run(&mut a, "delegate", Decision::Deny).await;
    r.unwrap();
    let output = &results(&events)[0].0;
    assert!(!meta.cwd.join("out.txt").exists() && output.contains("out.txt"));
    let path = output
        .split("[worktree ")
        .nth(1)
        .unwrap()
        .split(", branch ")
        .next()
        .unwrap();
    assert!(std::path::Path::new(path).exists());
    git(&["worktree", "remove", "--force", path]);
}

#[cfg(unix)]
#[tokio::test]
async fn failed_or_cancelled_tasks_clean_only_pristine_worktrees() {
    let schema = json!({"type":"object"});
    let mock = Mock::routed(
        vec![
            vec![call(
                "t",
                "task",
                json!({"tasks":[
                    {"name":"clean","isolated":true,"prompt":"fail-clean","schema":schema},
                    {"name":"dirty","isolated":true,"prompt":"fail-dirty","schema":schema},
                    {"name":"cancel","isolated":true,"prompt":"cancel-clean"},
                ]}),
            )],
            vec![text("done")],
        ],
        vec![
            ("fail-clean", vec![vec![text("no submit")]]),
            (
                "fail-dirty",
                vec![
                    vec![call(
                        "w",
                        "write",
                        json!({"path":"keep.txt","content":"keep me"}),
                    )],
                    vec![text("no submit")],
                ],
            ),
        ],
    );
    mock.hang.lock().unwrap().push("cancel-clean".into());
    let (mut a, meta) = agent("cleanup-task-worktree", Mode::Full, &mock);
    let git = |args: &[&str]| {
        let out = std::process::Command::new("git")
            .current_dir(&meta.cwd)
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap()
    };
    git(&["init", "-q"]);
    git(&[
        "-c",
        "user.name=Test",
        "-c",
        "user.email=test@example.com",
        "commit",
        "--allow-empty",
        "-qm",
        "initial",
    ]);
    let live = a.live().unwrap();
    let cancel = async {
        while !mock
            .seen
            .lock()
            .unwrap()
            .iter()
            .any(|(_, m)| m.contains("cancel-clean") && m.contains("# Role: "))
        {
            tokio::task::yield_now().await;
        }
        live.send(json!({"type":"sub_cancel","name":"cancel"}))
            .unwrap();
    };
    let ((r, events), ()) = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        tokio::join!(run(&mut a, "delegate", Decision::Deny), cancel)
    })
    .await
    .unwrap();
    r.unwrap();
    let listing = git(&["worktree", "list", "--porcelain"]);
    assert_eq!(
        listing
            .lines()
            .filter(|line| line.starts_with("worktree "))
            .count(),
        2,
        "{listing}"
    );
    let tree = listing
        .lines()
        .filter_map(|line| line.strip_prefix("worktree "))
        .last()
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(PathBuf::from(tree).join("keep.txt")).unwrap(),
        "keep me"
    );
    assert_eq!(
        git(&["branch", "--list", "greencode/sub-*"])
            .lines()
            .count(),
        1
    );
    let notices: Vec<_> = events
        .iter()
        .filter_map(|e| {
            if let Event::Notify(s) = e {
                Some(s)
            } else {
                None
            }
        })
        .collect();
    let name = std::path::Path::new(tree)
        .file_name()
        .unwrap()
        .to_str()
        .unwrap();
    assert!(
        notices
            .iter()
            .any(|s| s.contains("worktree cleanup incomplete") && s.contains(name)),
        "{notices:?}"
    );
}

#[tokio::test]
async fn compaction_replaces_history() {
    let mock = Mock::new(vec![
        vec![text("first")],
        vec![text("the summary")],
        vec![text("after")],
    ]);
    let (mut a, meta) = agent("compact", Mode::Full, &mock);
    run(&mut a, "hello", Decision::Deny).await.0.unwrap();
    run(&mut a, "/compact", Decision::Deny).await.0.unwrap();
    assert_eq!(a.messages.len(), 1);
    run(&mut a, "continue", Decision::Deny).await.0.unwrap();
    let (_, loaded) = Session::load(&meta.id).unwrap();
    assert_eq!(loaded.len(), 3);
    assert!(matches!(&loaded[0].content[0], Block::Text { text } if text.contains("the summary")));
}

#[tokio::test]
async fn previewed_read_keeps_exact_history_when_compacted_and_resumed() {
    let meta = Meta::new(dir("compact-preview"), "mock/m".into(), "native".into());
    let session = Session::create(&meta).unwrap();
    let id = format!("old-read-{}", meta.id);
    let result = format!("{}INTERIOR{}", "old head\n".repeat(70), "\nold tail".repeat(70));
    let rule = "Use the exact RIVER: prefix even after resume.";
    session.append(&Message::user(rule)).unwrap();
    session.append(&Message { role: Role::Assistant, content: vec![call(&id, "read", json!({"path": "gone.txt"}))] }).unwrap();
    session.append(&Message { role: Role::User, content: vec![Block::ToolResult {
        tool_use_id: id, content: result.clone(), is_error: false,
    }] }).unwrap();
    for prompt in ["second prompt", "third prompt", "fourth prompt"] {
        session.append(&Message::user(prompt)).unwrap();
        session.append(&Message { role: Role::Assistant, content: vec![text("reply")] }).unwrap();
    }
    let raw = std::fs::read(Session::path(&meta.id)).unwrap();
    let mock = Mock::new(vec![vec![text("RIVER:answer")]]);
    let mut cfg = Config { old_read_previews: true, ..Config::default() };
    cfg.learning.auto = false;
    let cfg = Arc::new(cfg);
    let mut agent = Agent::with_provider(cfg.clone(), &meta, Arc::new(Perms::new(Mode::Full)), mock.clone(), "m".into());
    run(&mut agent, "/compact summary", Decision::Deny).await.0.unwrap();
    assert!(mock.seen.lock().unwrap().is_empty());
    assert_eq!(std::fs::read(Session::path(&meta.id)).unwrap(), raw);
    drop(agent);
    let mut resumed = Agent::with_provider(cfg, &meta, Arc::new(Perms::new(Mode::Full)), mock.clone(), "m".into());
    run(&mut resumed, "continue", Decision::Deny).await.0.unwrap();
    let seen = mock.seen.lock().unwrap();
    let messages: Vec<Message> = serde_json::from_str(&seen[0].1).unwrap();
    assert!(matches!(&messages[1].content[0], Block::Text { text } if text == rule));
    let Block::ToolResult { content, .. } = &messages[3].content[0] else { panic!("missing preview") };
    assert!(!content.contains("INTERIOR"));
    let path = content.split_once("full output: ").unwrap().1.split_once(" ...]").unwrap().0;
    assert_eq!(std::fs::read_to_string(path).unwrap(), result);
    let _ = std::fs::remove_file(path);
}

#[tokio::test]
async fn compaction_keeps_a_verbatim_tail() {
    let mock = Mock::new(vec![
        vec![text("seed")],
        vec![text("the summary")],
        vec![text("done")],
    ]);
    let meta = Meta::new(dir("compact-tail"), "mock/m".into(), "native".into());
    let mut cfg = Config::default();
    cfg.learning.auto = false;
    let mut a = Agent::with_provider(
        Arc::new(cfg),
        &meta,
        Arc::new(Perms::new(Mode::Full)),
        mock.clone(),
        "m".into(),
    );
    run(&mut a, "seed", Decision::Deny).await.0.unwrap();
    a.messages = vec![
        Message::user(format!("OLD {}", "x".repeat(35_000))),
        Message {
            role: Role::Assistant,
            content: vec![text("old answer")],
        },
        Message::user("NEWEST PROMPT"),
        Message {
            role: Role::Assistant,
            content: vec![text("recent answer")],
        },
    ];
    run(&mut a, "/compact", Decision::Deny).await.0.unwrap();
    assert_eq!(a.messages.len(), 3, "summary plus the retained tail");
    let seen = mock.seen.lock().unwrap();
    assert!(
        !seen[1].1.contains("NEWEST PROMPT"),
        "the tail is not replayed for the summary"
    );
    assert!(seen[1].1.contains("OLD "));
    drop(seen);
    run(&mut a, "after", Decision::Deny).await.0.unwrap();
    let seen = mock.seen.lock().unwrap();
    assert!(
        seen[2].1.contains("<compacted-summary>")
            && seen[2].1.contains("NEWEST PROMPT")
            && seen[2].1.contains("recent answer")
    );
    assert!(!seen[2].1.contains("old answer"));
    drop(seen);
    let (_, loaded) = Session::load(&meta.id).unwrap();
    assert_eq!(loaded.len(), 5);
    assert!(matches!(&loaded[1].content[0], Block::Text { text } if text == "NEWEST PROMPT"));
}

#[tokio::test]
async fn compaction_preserves_new_prompt_and_reports_its_usage() {
    let mock = Mock::new(vec![
        vec![text("first")],
        vec![text("summary")],
        vec![text("after")],
    ]);
    for reply in mock.replies.lock().unwrap().iter_mut() {
        reply.usage.input = 100;
    }
    let meta = Meta::new(dir("compact-new-prompt"), "mock/m".into(), "native".into());
    let mut cfg = Config {
        compact_at: 50,
        ..Config::default()
    };
    cfg.learning.auto = false;
    let mut a = Agent::with_provider(
        Arc::new(cfg),
        &meta,
        Arc::new(Perms::new(Mode::Full)),
        mock.clone(),
        "m".into(),
    );
    run(&mut a, "old task", Decision::Deny).await.0.unwrap();
    let (result, events) = run(&mut a, "NEW EXACT REQUIREMENT", Decision::Deny).await;
    result.unwrap();
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, Event::Usage(_)))
            .count(),
        1
    );
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, Event::UsageAux(_)))
            .count(),
        1
    );
    let seen = mock.seen.lock().unwrap();
    assert!(!seen[1].1.contains("NEW EXACT REQUIREMENT"));
    assert!(seen[2].1.contains("NEW EXACT REQUIREMENT"));
}

#[tokio::test]
async fn compaction_can_wait_until_the_next_user_turn() {
    let mock = Mock::new(vec![
        vec![call("check", "read", json!({"path": "input.txt"}))],
        vec![text("first answer")],
        vec![text("summary")],
        vec![text("second answer")],
    ]);
    for reply in mock.replies.lock().unwrap().iter_mut().take(2) {
        reply.usage.input = 100;
    }
    let meta = Meta::new(dir("compact-disabled-midturn"), "mock/m".into(), "native".into());
    std::fs::write(meta.cwd.join("input.txt"), "EXACT TOOL RESULT").unwrap();
    let mut cfg = Config { compact_at: 50, compaction_mid_turn: false, ..Config::default() };
    cfg.learning.auto = false;
    let mut agent = Agent::with_provider(Arc::new(cfg), &meta, Arc::new(Perms::new(Mode::Ask)), mock.clone(), "m".into());

    let (first, events) = run(&mut agent, "read that file", Decision::Deny).await;
    first.unwrap();
    assert!(!events.iter().any(|e| matches!(e, Event::UsageAux(_))));
    let (second, events) = run(&mut agent, "NEW EXACT PROMPT", Decision::Deny).await;
    second.unwrap();
    assert!(events.iter().any(|e| matches!(e, Event::UsageAux(_))));

    let seen = mock.seen.lock().unwrap();
    assert!(seen[1].1.contains("EXACT TOOL RESULT"));
    assert!(!seen[2].1.contains("NEW EXACT PROMPT"));
    assert!(seen[3].1.contains("<compacted-summary>") && seen[3].1.contains("NEW EXACT PROMPT"));
}

#[tokio::test]
async fn compaction_uses_the_active_models_fixed_threshold() {
    for (name, enabled, exact, expected) in [
        ("exact", true, 150, false),
        ("prefix", true, 0, true),
        ("disabled", false, 0, false),
    ] {
        let mock = Mock::new(vec![vec![text("first")], vec![text("second")], vec![text("third")]]);
        mock.replies.lock().unwrap()[0].usage.input = 100;
        let meta = Meta::new(dir(&format!("compact-model-{name}")), "mock/model".into(), "native".into());
        let mut cfg = Config {
            compact_at: 200,
            model_compact_at_enabled: enabled,
            model_compact_at: [("mock/*".into(), 150), ("mock/mo*".into(), 50), ("mock/model".into(), exact)].into(),
            ..Config::default()
        };
        cfg.learning.auto = false;
        let mut agent = Agent::with_provider(Arc::new(cfg), &meta, Arc::new(Perms::new(Mode::Ask)), mock.clone(), "model".into());
        run(&mut agent, "initial input", Decision::Deny).await.0.unwrap();
        let (result, events) = run(&mut agent, "next input", Decision::Deny).await;
        result.unwrap();
        assert_eq!(events.iter().any(|e| matches!(e, Event::UsageAux(_))), expected, "{name}");
        let seen = mock.seen.lock().unwrap();
        assert_eq!(seen[1].1.contains("next input"), !expected, "{name}");
        assert!(seen.last().unwrap().1.contains("next input"), "{name}");
    }
}

#[tokio::test]
async fn compaction_rebases_instructions_and_memory_even_after_resume() {
    let mock = Mock::new(vec![
        vec![text("first")],
        vec![text("second")],
        vec![text("summary")],
        vec![text("resumed")],
    ]);
    let cwd = dir("compact-workspace");
    std::fs::create_dir_all(cwd.join(".greencode")).unwrap();
    let memory = cwd.join(".greencode/MEMORY.md");
    let instructions = cwd.join("AGENTS.md");
    std::fs::write(&instructions, "STALE_INSTRUCTIONS").unwrap();
    let (mut a, meta) = agent("compact-workspace", Mode::Full, &mock);
    run(&mut a, "first", Decision::Deny).await.0.unwrap();
    std::fs::write(&memory, "STALE_MEMORY").unwrap();
    run(&mut a, "second", Decision::Deny).await.0.unwrap();
    std::fs::write(&instructions, "CURRENT_INSTRUCTIONS").unwrap();
    std::fs::write(&memory, "CURRENT_MEMORY").unwrap();
    run(&mut a, "/compact summary", Decision::Deny)
        .await
        .0
        .unwrap();
    drop(a);
    let mut a = Agent::with_provider(
        Arc::new(Config::default()),
        &meta,
        Arc::new(Perms::new(Mode::Full)),
        mock.clone(),
        "m".into(),
    );
    run(&mut a, "resume", Decision::Deny).await.0.unwrap();
    let seen = mock.seen.lock().unwrap();
    assert!(!seen[2].1.contains("STALE_"));
    assert!(!seen[3].1.contains("STALE_"));
    assert!(!seen[3].1.contains(greencode_core::context::CONTEXT_MARKER));
    assert!(seen[3].1.contains("CURRENT_INSTRUCTIONS") && seen[3].1.contains("CURRENT_MEMORY"));
}

#[tokio::test]
async fn native_resume_keeps_compaction_threshold() {
    let mock = Mock::new(vec![
        vec![text("old answer")],
        vec![text("summary")],
        vec![text("new answer")],
    ]);
    mock.replies.lock().unwrap()[0].usage = greencode_core::Usage {
        input: 10,
        cache_read: 35,
        cache_write: 3,
        output: 5,
    };
    let meta = Meta::new(dir("compact-resume"), "mock/m".into(), "native".into());
    let mut cfg = Config {
        compact_at: 50,
        ..Config::default()
    };
    cfg.learning.auto = false;
    let cfg = Arc::new(cfg);
    let perms = Arc::new(Perms::new(Mode::Full));
    let mut a = Agent::with_provider(cfg.clone(), &meta, perms.clone(), mock.clone(), "m".into());
    run(&mut a, "old task", Decision::Deny).await.0.unwrap();
    drop(a);
    let mut resumed = Agent::with_provider(cfg, &meta, perms, mock.clone(), "m".into());
    let (result, events) = run(&mut resumed, "NEW EXACT REQUIREMENT", Decision::Deny).await;
    result.unwrap();
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, Event::Usage(_)))
            .count(),
        1
    );
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, Event::UsageAux(_)))
            .count(),
        1
    );
    let seen = mock.seen.lock().unwrap();
    assert_eq!(seen.len(), 3);
    assert!(!seen[1].1.contains("NEW EXACT REQUIREMENT"));
    assert!(seen[2].1.contains("<compacted-summary>\\nsummary"));
    assert!(seen[2].1.contains("NEW EXACT REQUIREMENT"));
    assert!(!seen[2].1.contains("old answer"));
    assert_eq!(seen[0].0, seen[2].0);
}

#[tokio::test]
async fn native_compaction_reset_survives_resume() {
    let mock = Mock::new(vec![
        vec![text("first")],
        vec![text("the summary")],
        vec![text("after")],
    ]);
    for reply in mock.replies.lock().unwrap().iter_mut().take(2) {
        reply.usage.input = 100;
    }
    let meta = Meta::new(
        dir("compact-reset-resume"),
        "mock/m".into(),
        "native".into(),
    );
    let mut cfg = Config {
        compact_at: 50,
        ..Config::default()
    };
    cfg.learning.auto = false;
    let cfg = Arc::new(cfg);
    let perms = Arc::new(Perms::new(Mode::Full));
    let mut a = Agent::with_provider(cfg.clone(), &meta, perms.clone(), mock.clone(), "m".into());
    run(&mut a, "hello", Decision::Deny).await.0.unwrap();
    run(&mut a, "/compact", Decision::Deny).await.0.unwrap();
    drop(a);
    let mut resumed = Agent::with_provider(cfg, &meta, perms, mock.clone(), "m".into());
    let (result, events) = run(&mut resumed, "continue", Decision::Deny).await;
    result.unwrap();
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, Event::Usage(_)))
            .count(),
        1
    );
    assert!(!events
        .iter()
        .any(|e| matches!(e, Event::Notify(t) if t.contains("Compacting"))));
    let seen = mock.seen.lock().unwrap();
    assert_eq!(seen.len(), 3);
    assert!(seen[2].1.contains("the summary"));
    assert!(!seen[2].1.contains("hello"));
}

#[tokio::test]
async fn native_resume_reuses_the_context_prefix_and_appends_changes() {
    let mock = Mock::new(vec![
        vec![text("one")],
        vec![text("two")],
        vec![text("new thread")],
        vec![text("updated rules")],
    ]);
    let (mut a, meta) = agent("prefix-resume", Mode::Full, &mock);
    run(&mut a, "first", Decision::Deny).await.0.unwrap();
    // Older runtime snapshots have no token checkpoint.
    let mut legacy = Session::runtime(&meta.id).unwrap();
    legacy.as_object_mut().unwrap().remove("context");
    Session::save_runtime(&meta.id, &legacy).unwrap();
    std::fs::create_dir_all(meta.cwd.join(".greencode")).unwrap();
    std::fs::write(
        meta.cwd.join(".greencode/MEMORY.md"),
        "new durable fact for the next session",
    )
    .unwrap();
    let mut cfg = Config::default();
    cfg.learning.auto = false;
    let mut resumed = Agent::with_provider(
        Arc::new(cfg.clone()),
        &meta,
        Arc::new(Perms::new(Mode::Full)),
        mock.clone(),
        "m".into(),
    );
    run(&mut resumed, "second", Decision::Deny).await.0.unwrap();
    let fresh = Meta::new(meta.cwd.clone(), meta.model.clone(), meta.backend.clone());
    let mut new_thread = Agent::with_provider(
        Arc::new(cfg.clone()),
        &fresh,
        Arc::new(Perms::new(Mode::Full)),
        mock.clone(),
        "m".into(),
    );
    run(&mut new_thread, "third", Decision::Deny)
        .await
        .0
        .unwrap();
    std::fs::write(
        meta.cwd.join("AGENTS.md"),
        "new project safety instructions",
    )
    .unwrap();
    run(&mut resumed, "fourth", Decision::Deny).await.0.unwrap();
    let seen = mock.seen.lock().unwrap();
    // The system prompt and tools never change; only the context message and history do.
    assert_eq!(seen[0].0, seen[1].0);
    assert_eq!(seen[1].0, seen[2].0);
    assert!(seen[0]
        .0
        .contains("\"schema\":{\"properties\":{\"s\":{\"type\":\"string\"}},\"type\":\"object\"}"));
    let first = |i: usize| {
        serde_json::to_value(
            serde_json::from_str::<Vec<Message>>(&seen[i].1)
                .unwrap()
                .remove(0),
        )
        .unwrap()
    };
    assert_eq!(
        first(0),
        first(1),
        "a resumed session reuses its saved context message"
    );
    assert!(seen[1].1.contains("new durable fact"));
    assert!(seen[2].1.contains("new durable fact"));
    assert!(seen[3].1.contains("new project safety instructions"));
}

#[test]
fn anthropic_body_is_cacheable() {
    let a = anthropic::Anthropic {
        http: Default::default(),
        base: "https://api.anthropic.com".into(),
        key: String::new(),
        oauth: None,
        effort: None,
        wire: Default::default(),
    };
    let tools = tools::defs(false, &[]);
    let msgs = vec![Message::user("hi")];
    let body = a.body(&Request {
        model: "claude-opus-5-5",
        system: "s",
        messages: &msgs,
        tools: &tools,
        max_tokens: 10,
        live: None,
    });
    assert_eq!(body["cache_control"]["type"], "ephemeral");
    assert_eq!(body["system"][0]["cache_control"]["type"], "ephemeral");
    assert_eq!(body["fallbacks"], "default");
    assert_eq!(
        body["tools"].as_array().unwrap().len(),
        14 + cfg!(target_os = "macos") as usize
    );
    assert_eq!(
        body["messages"][0],
        json!({"role":"user","content":[{"type":"text","text":"hi"}]})
    );

    let mut large = msgs.clone();
    large.push(Message {
        role: Role::Assistant,
        content: (0..25)
            .map(|i| call(&format!("t{i}"), "read", json!({"path":"x"})))
            .collect(),
    });
    large.push(Message {
        role: Role::User,
        content: (0..25)
            .map(|i| Block::ToolResult {
                tool_use_id: format!("t{i}"),
                content: "result".into(),
                is_error: false,
            })
            .collect(),
    });
    let body = a.body(&Request {
        model: "unused",
        system: "s",
        messages: &large,
        tools: &tools,
        max_tokens: 10,
        live: None,
    });
    assert_eq!(
        body["messages"][0]["content"][0]["cache_control"]["type"],
        "ephemeral"
    );
    assert!(serde_json::to_value(&large).unwrap()[0]["content"][0]
        .get("cache_control")
        .is_none());
}

#[tokio::test]
async fn mcp_stdio_search_and_call() {
    if std::process::Command::new("python3")
        .arg("--version")
        .output()
        .is_err()
    {
        return; // the fake server below needs python3
    }
    let d = dir("mcp");
    std::fs::write(
        d.join("server.py"),
        r#"
import json, sys
for line in sys.stdin:
    m = json.loads(line)
    if "id" not in m: continue
    if m["method"] == "initialize": r = {"protocolVersion": "2025-06-18", "capabilities": {}, "serverInfo": {"name": "t", "version": "1"}}
    elif m["method"] == "tools/list": r = {"tools": [
        {"name": "add", "description": "Add two numbers", "inputSchema": {"type": "object", "properties": {"a": {"type": "number"}, "b": {"type": "number"}}}},
        {"name": "weather", "description": "Forecast for a city", "inputSchema": {"type": "object"}}]}
    else:
        a = m["params"]["arguments"]
        r = {"content": [{"type": "text", "text": str(a["a"] + a["b"])}]}
    print(json.dumps({"jsonrpc": "2.0", "id": m["id"], "result": r}), flush=True)
"#,
    )
    .unwrap();
    std::fs::write(
        d.join(".mcp.json"),
        r#"{"mcpServers": {"calc": {"command": "python3", "args": ["server.py"]}}}"#,
    )
    .unwrap();
    let mcp = tools::mcp::Mcp::load(&d);
    assert_eq!(mcp.names(), vec!["calc"]);
    // The tool list stays out of the prompt: only the server name is in the definition.
    let def = tools::defs(false, &mcp.names()).pop().unwrap();
    assert!(def.description.contains("calc") && !def.description.contains("weather"));
    let found = mcp
        .run(&json!({"action": "search", "query": "add numbers"}), &d)
        .await
        .unwrap();
    assert!(found.contains("calc/add") && found.contains("\"a\"") && !found.contains("weather"));
    let out = mcp
        .run(
            &json!({"action": "call", "server": "calc", "tool": "add", "args": {"a": 2, "b": 40}}),
            &d,
        )
        .await
        .unwrap();
    assert_eq!(out, "42");
    assert!(mcp
        .run(
            &json!({"action": "call", "server": "nope", "tool": "x"}),
            &d
        )
        .await
        .is_err());
}

#[tokio::test]
async fn acp_backend_streams_asks_and_saves() {
    if std::process::Command::new("python3")
        .arg("--version")
        .output()
        .is_err()
    {
        return; // the fake agent below needs python3
    }
    let d = dir("acp");
    std::fs::write(
        d.join("agent.py"),
        r#"
import json, sys
def send(m): print(json.dumps(m), flush=True)
def update(u): send({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": "s1", "update": u}})
for line in sys.stdin:
    m = json.loads(line)
    meth = m.get("method")
    if meth == "initialize": send({"jsonrpc": "2.0", "id": m["id"], "result": {"protocolVersion": 1}})
    elif meth == "session/new": send({"jsonrpc": "2.0", "id": m["id"], "result": {"sessionId": "s1"}})
    elif meth == "session/prompt":
        prompt_id = m["id"]
        update({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "working"}})
        update({"sessionUpdate": "tool_call", "toolCallId": "c1", "title": "Run ls", "status": "pending", "rawInput": {"cwd": "/x"}})
        for _ in range(2): update({"sessionUpdate": "tool_call_update", "toolCallId": "c1", "status": "in_progress", "title": "ls", "rawInput": {"command": "ls", "cwd": "/x"}})
        send({"jsonrpc": "2.0", "id": 99, "method": "session/request_permission", "params": {"sessionId": "s1",
              "toolCall": {"toolCallId": "c1", "title": "Run ls"},
              "options": [{"optionId": "yes", "kind": "allow_once"}, {"optionId": "no", "kind": "reject_once"}]}})
    elif m.get("id") == 99:
        picked = m["result"]["outcome"].get("optionId")
        update({"sessionUpdate": "tool_call_update", "toolCallId": "c1", "status": "completed",
                "content": [{"type": "content", "content": {"type": "text", "text": "picked " + str(picked)}}]})
        send({"jsonrpc": "2.0", "id": prompt_id, "result": {"stopReason": "end_turn"}})
"#,
    )
    .unwrap();
    let meta = Meta::new(d.clone(), String::new(), "fake".into());
    let command = vec!["python3".to_string(), "agent.py".to_string()];
    let mut acp = greencode_core::acp::Acp::start(
        &command,
        &Default::default(),
        &meta,
        Arc::new(Perms::new(Mode::Ask)),
    )
    .await
    .unwrap();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let events = {
        let fut = acp.prompt("list files".into(), &tx);
        tokio::pin!(fut);
        let mut events = Vec::new();
        loop {
            tokio::select! {
                r = &mut fut => { r.unwrap(); break }
                Some(ev) = rx.recv() => match ev {
                    Event::Permission { reply, tool, .. } => { assert_eq!(tool, "Run ls"); let _ = reply.send(Decision::Always); }
                    ev => events.push(ev),
                }
            }
        }
        while let Ok(ev) = rx.try_recv() {
            events.push(ev);
        }
        events
    };
    assert!(events
        .iter()
        .any(|e| matches!(e, Event::Text(t) if t == "working")));
    // The call opens with a stub input; the real arguments arrive once, under the same id and name.
    let starts: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            Event::ToolStart { id, name, input } => {
                Some((id.as_str(), name.as_str(), input.clone()))
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        starts,
        vec![
            ("c1", "Run ls", json!({"cwd": "/x"})),
            ("c1", "Run ls", json!({"command": "ls", "cwd": "/x"}))
        ]
    );
    // "Always" falls back to allow_once when the agent offers no allow_always.
    assert_eq!(results(&events), vec![("picked yes".to_string(), false)]);
    let (saved, messages) = Session::load(&meta.id).unwrap();
    assert_eq!((saved.title.as_str(), messages.len()), ("list files", 2));
}

#[tokio::test]
async fn acp_preset_options_precede_first_prompt() {
    if std::process::Command::new("python3")
        .arg("--version")
        .output()
        .is_err()
    {
        return;
    }
    let d = dir("acp-preset");
    std::fs::write(d.join("agent.py"), r#"
import json, sys
for line in sys.stdin:
    m = json.loads(line)
    method = m.get("method")
    if method is None: continue
    with open("requests.jsonl", "a") as f: f.write(json.dumps(m) + "\n")
    if method == "initialize": result = {"protocolVersion": 1}
    elif method == "session/new": result = {"sessionId": "s1", "configOptions": [{"id":"reasoning_effort","currentValue":"low"}]}
    elif method == "session/set_config_option": result = {"configOptions": [{"id":"reasoning_effort","currentValue":m["params"]["value"]}]}
    elif method == "session/prompt": result = {"stopReason":"end_turn"}
    else: raise RuntimeError(method)
    print(json.dumps({"jsonrpc":"2.0","id":m["id"],"result":result}), flush=True)
"#).unwrap();
    let mut cfg = Config::default();
    cfg.backends.push(greencode_core::config::AcpCfg {
        name: "fake-preset".into(),
        command: vec!["python3".into(), "agent.py".into()],
        protocol: greencode_core::config::Protocol::Acp,
        env: Default::default(),
    });
    let meta = Meta::new(d.clone(), "default".into(), "fake-preset".into());
    let (tx, mut rx) = greencode_core::spawn(
        &tokio::runtime::Handle::current(),
        Arc::new(cfg),
        meta.clone(),
        Arc::new(Perms::new(Mode::Ask)),
    );
    tx.send(greencode_core::Command::PromptWithOptions(
        " \t/compact now".into(),
        vec![json!({"type":"set_config_option","configId":"reasoning_effort","value":"xhigh"})],
    ))
    .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while let Some(event) = rx.recv().await {
            match event {
                Event::Error(e) => panic!("{e}"),
                Event::Done => break,
                _ => {}
            }
        }
    })
    .await
    .unwrap();
    let requests: Vec<serde_json::Value> = std::fs::read_to_string(d.join("requests.jsonl"))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let methods: Vec<_> = requests
        .iter()
        .filter_map(|request| request["method"].as_str())
        .collect();
    assert_eq!(
        methods,
        [
            "initialize",
            "session/new",
            "session/set_config_option",
            "session/prompt"
        ]
    );
    assert_eq!(requests[2]["params"]["configId"], "reasoning_effort");
    assert_eq!(requests[2]["params"]["value"], "xhigh");
    assert_eq!(
        requests[3]["params"]["prompt"],
        json!([{"type":"text","text":"/compact now"}])
    );
    drop(tx);
    Session::delete(&meta.id);
}

#[tokio::test]
async fn acp_commands_advertised_at_session_start_reach_the_ui() {
    if std::process::Command::new("python3")
        .arg("--version")
        .output()
        .is_err()
    {
        return;
    }
    let d = dir("acp-commands");
    std::fs::write(d.join("agent.py"), r#"
import json, sys
def send(o): print(json.dumps(o), flush=True)
for line in sys.stdin:
    m = json.loads(line)
    method = m.get("method")
    if method is None: continue
    if method == "initialize": result = {"protocolVersion": 1}
    elif method == "session/new":
        send({"jsonrpc":"2.0","id":m["id"],"result":{"sessionId":"s1"}})
        send({"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s1","update":{"sessionUpdate":"available_commands_update","availableCommands":[{"name":"compact","description":"d"}]}}})
        continue
    elif method == "session/prompt": result = {"stopReason":"end_turn"}
    else: raise RuntimeError(method)
    send({"jsonrpc":"2.0","id":m["id"],"result":result})
"#).unwrap();
    let mut cfg = Config::default();
    cfg.backends.push(greencode_core::config::AcpCfg {
        name: "fake-commands".into(),
        command: vec!["python3".into(), "agent.py".into()],
        protocol: greencode_core::config::Protocol::Acp,
        env: Default::default(),
    });
    let meta = Meta::new(d.clone(), "default".into(), "fake-commands".into());
    let (tx, mut rx) = greencode_core::spawn(
        &tokio::runtime::Handle::current(),
        Arc::new(cfg),
        meta.clone(),
        Arc::new(Perms::new(Mode::Ask)),
    );
    tx.send(greencode_core::Command::Prompt("hello".into()))
        .unwrap();
    let names = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        let mut names = None;
        while let Some(event) = rx.recv().await {
            match event {
                Event::Error(e) => panic!("{e}"),
                Event::Remote { command, data } if command == "get_commands" => {
                    names = Some(data["commands"][0]["name"].clone())
                }
                Event::Done => break,
                _ => {}
            }
        }
        names
    })
    .await
    .unwrap();
    assert_eq!(names, Some(json!("compact")));
    drop(tx);
    Session::delete(&meta.id);
}

#[tokio::test]
async fn acp_commands_sent_just_after_session_new_are_found_by_discovery() {
    if std::process::Command::new("python3")
        .arg("--version")
        .output()
        .is_err()
    {
        return;
    }
    let d = dir("acp-discovery");
    // Like opencode/codex-acp: the advertisement trails the session/new response by a moment.
    std::fs::write(d.join("agent.py"), r#"
import json, sys, time
def send(o): print(json.dumps(o), flush=True)
for line in sys.stdin:
    m = json.loads(line)
    method = m.get("method")
    if method == "initialize": send({"jsonrpc":"2.0","id":m["id"],"result":{"protocolVersion":1}})
    elif method == "session/new":
        send({"jsonrpc":"2.0","id":m["id"],"result":{"sessionId":"s1"}})
        time.sleep(0.1)
        send({"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s1","update":{"sessionUpdate":"available_commands_update","availableCommands":[{"name":"review","description":"d","input":None}]}}})
"#).unwrap();
    let mut cfg = Config::default();
    cfg.backends.push(greencode_core::config::AcpCfg {
        name: "fake-late".into(),
        command: vec!["python3".into(), "agent.py".into()],
        protocol: greencode_core::config::Protocol::Acp,
        env: Default::default(),
    });
    let meta = Meta::new(d.clone(), "default".into(), "fake-late".into());
    let (tx, mut rx) = greencode_core::spawn(
        &tokio::runtime::Handle::current(),
        Arc::new(cfg),
        meta.clone(),
        Arc::new(Perms::new(Mode::Ask)),
    );
    tx.send(greencode_core::Command::Remote(json!({"type":"get_state"})))
        .unwrap();
    let name = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while let Some(event) = rx.recv().await {
            match event {
                Event::Error(e) => panic!("{e}"),
                Event::Remote { command, data } if command == "get_commands" => {
                    return Some(data["commands"][0]["name"].clone())
                }
                Event::Done => break,
                _ => {}
            }
        }
        None
    })
    .await
    .unwrap();
    assert_eq!(name, Some(json!("review")));
    drop(tx);
    Session::delete(&meta.id);
}

#[tokio::test]
async fn acp_command_changes_after_done_reach_the_ui_while_idle() {
    if std::process::Command::new("python3")
        .arg("--version")
        .output()
        .is_err()
    {
        return;
    }
    let d = dir("acp-idle");
    std::fs::write(d.join("agent.py"), r#"
import json, sys, time
def send(o): print(json.dumps(o), flush=True)
def cmds(n): send({"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s1","update":{"sessionUpdate":"available_commands_update","availableCommands":[{"name":n,"description":"d"}]}}})
for line in sys.stdin:
    m = json.loads(line)
    method = m.get("method")
    if method == "initialize": send({"jsonrpc":"2.0","id":m["id"],"result":{"protocolVersion":1}})
    elif method == "session/new":
        send({"jsonrpc":"2.0","id":m["id"],"result":{"sessionId":"s1"}})
        cmds("early")
    elif method == "session/prompt":
        send({"jsonrpc":"2.0","id":m["id"],"result":{"stopReason":"end_turn"}})
        time.sleep(0.5)
        cmds("late")
"#).unwrap();
    let mut cfg = Config::default();
    cfg.backends.push(greencode_core::config::AcpCfg {
        name: "fake-idle".into(),
        command: vec!["python3".into(), "agent.py".into()],
        protocol: greencode_core::config::Protocol::Acp,
        env: Default::default(),
    });
    let meta = Meta::new(d.clone(), "default".into(), "fake-idle".into());
    let (tx, mut rx) = greencode_core::spawn(
        &tokio::runtime::Handle::current(),
        Arc::new(cfg),
        meta.clone(),
        Arc::new(Perms::new(Mode::Ask)),
    );
    tx.send(greencode_core::Command::Prompt("hi".into()))
        .unwrap();
    let mut done = false;
    let late = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while let Some(event) = rx.recv().await {
            match event {
                Event::Error(e) => panic!("{e}"),
                Event::Done => done = true,
                Event::Remote { command, data }
                    if command == "get_commands" && data["commands"][0]["name"] == "late" =>
                {
                    return done
                }
                _ => {}
            }
        }
        false
    })
    .await
    .unwrap();
    assert!(
        late,
        "the late advertisement must arrive after Done, with no further command sent"
    );
    drop(tx);
    Session::delete(&meta.id);
}

#[test]
fn worktree_isolates_a_thread_and_diff_shows_new_files() {
    let d = dir("wt");
    let git = |args: &[&str]| {
        let ok = std::process::Command::new("git")
            .arg("-C")
            .arg(&d)
            .args(["-c", "user.name=t", "-c", "user.email=t@t"])
            .args(args)
            .output()
            .unwrap()
            .status
            .success();
        assert!(ok, "git {args:?}");
    };
    git(&["init", "-q"]);
    std::fs::write(d.join("a.txt"), "one\n").unwrap();
    git(&["add", "."]);
    git(&["commit", "-qm", "init"]);
    let wt = greencode_core::worktree::create(&d, &d.with_extension("worktrees"), "t1").unwrap();
    assert_eq!(std::fs::read_to_string(wt.join("a.txt")).unwrap(), "one\n");
    std::fs::write(wt.join("a.txt"), "two\n").unwrap();
    std::fs::write(wt.join("new.txt"), "fresh\n").unwrap();
    let diff = greencode_core::worktree::diff(&wt);
    assert!(diff.contains("-one") && diff.contains("+two") && diff.contains("+fresh"));
    // The original checkout is untouched.
    assert_eq!(greencode_core::worktree::diff(&d), "");
}

#[test]
fn diff_shows_staged_initial_changes_and_unusual_untracked_names() {
    let d = dir("initial-diff");
    let git = |args: &[&str]| {
        assert!(std::process::Command::new("git")
            .arg("-C")
            .arg(&d)
            .args(args)
            .status()
            .unwrap()
            .success());
    };
    git(&["init", "-q"]);
    std::fs::write(d.join("staged"), "staged content\n").unwrap();
    git(&["add", "staged"]);
    std::fs::write(d.join("new\nfile"), "new content\n").unwrap();
    let diff = greencode_core::worktree::diff(&d);
    assert!(diff.contains("+staged content"), "{diff}");
    assert!(diff.contains("+new content"), "{diff}");
}

#[test]
fn diff_keeps_staged_and_unstaged_changes_that_cancel_out() {
    let d = dir("split-diff");
    let git = |args: &[&str]| {
        assert!(std::process::Command::new("git")
            .arg("-C")
            .arg(&d)
            .args(["-c", "user.name=t", "-c", "user.email=t@t"])
            .args(args)
            .status()
            .unwrap()
            .success());
    };
    git(&["init", "-q"]);
    std::fs::write(d.join("file"), "original\n").unwrap();
    git(&["add", "file"]);
    git(&["commit", "-qm", "init"]);
    std::fs::write(d.join("file"), "staged\n").unwrap();
    git(&["add", "file"]);
    std::fs::write(d.join("file"), "original\n").unwrap();
    let diff = greencode_core::worktree::diff(&d);
    assert!(
        diff.contains("Staged:") && diff.contains("+staged"),
        "{diff}"
    );
    assert!(
        diff.contains("Unstaged:") && diff.contains("-staged"),
        "{diff}"
    );
}

#[test]
fn thread_view_persists_without_putting_ui_state_in_model_history() {
    use greencode_core::session::View;
    let meta = Meta::new(dir("thread-view"), "test/model".into(), "pi".into());
    let view = View {
        meta: meta.clone(),
        draft: "unsent draft".into(),
        queue: vec!["held follow-up".into()],
        pinned: true,
        archived: true,
        mode: Mode::AutoEdit,
        snoozed: 0,
        auto_settle: Default::default(),
        linked_pr: None,
        linked_pr_manual: None,
        last_user_at: 0,
    };
    Session::save_view(&view).unwrap();
    assert!(Session::list().iter().any(|m| m.id == meta.id));
    assert!(Session::open(&meta.id).is_none());
    let s = Session::create(&meta).unwrap();
    s.append(&Message::user("sent prompt")).unwrap();
    let (_, messages) = Session::load(&meta.id).unwrap();
    assert_eq!(messages.len(), 1);
    let loaded = Session::view(&meta.id).unwrap().unwrap();
    assert_eq!(loaded.draft, "unsent draft");
    assert_eq!(loaded.queue, ["held follow-up"]);
    assert!(loaded.pinned && loaded.archived);
    assert_eq!(loaded.mode, Mode::AutoEdit);
    for id in ["../outside", "", "/absolute"] {
        assert!(Session::load(id).is_err());
        assert!(Session::runtime(id).is_err());
    }
}

#[cfg(unix)]
#[test]
fn auto_edit_does_not_follow_symlinks_outside_the_project_or_dispatch_uri_tools() {
    let project = dir("permission-symlink");
    let outside = std::env::temp_dir().join(format!(
        "greencode-permission-outside-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&outside).unwrap();
    let link = project.join("external");
    std::os::unix::fs::symlink(&outside, &link).unwrap();
    let perms = Perms::new(Mode::AutoEdit);
    assert!(perms
        .check("write", &json!({"path":"external/new-file"}), &project)
        .is_some());
    assert!(perms
        .check("write", &json!({"path":"xd://bash"}), &project)
        .is_some());
    assert!(perms
        .check("write", &json!({"path":"new-file"}), &project)
        .is_none());
}

#[test]
fn presets_and_added_providers_resolve_and_persist() {
    use greencode_core::{config::ProviderCfg, provider};
    dir("providers");
    let path = greencode_core::home().join("config.toml");
    std::fs::write(&path, "# keep me\nmodel = 'ollama/llama'").unwrap();
    let mut cfg = Config::load().unwrap();
    // Keyless presets work without any environment; nested model ids keep their slashes.
    assert_eq!(
        provider::make(&cfg, "ollama/library/llama").unwrap().1,
        "library/llama"
    );
    assert!(cfg.providers.len() >= 20);
    let custom = ProviderCfg {
        kind: "openai".into(),
        base_url: "http://localhost:9/v1".into(),
        api_key: Some("s\"k".into()),
        models: vec!["a".into(), "b".into()],
        ..Default::default()
    };
    cfg.add_provider("my-llm", custom.clone()).unwrap();
    assert!(
        cfg.add_provider("my-llm", custom.clone()).is_err(),
        "duplicate"
    );
    assert!(cfg.add_provider("bad name", custom.clone()).is_err());
    assert!(cfg
        .add_provider(
            "x",
            ProviderCfg {
                base_url: "ftp://x".into(),
                ..custom
            }
        )
        .is_err());
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(
        text.starts_with("# keep me\n") && text.matches("[providers.").count() == 1,
        "{text}"
    );
    // A built-in name with no URL keeps the preset endpoint and stores the key.
    cfg.add_provider(
        "groq",
        ProviderCfg {
            api_key: Some("k".into()),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(
        (
            cfg.providers["groq"].base_url.as_str(),
            cfg.providers["groq"].kind.as_str()
        ),
        ("https://api.groq.com/openai/v1", "openai")
    );
    cfg.add_provider(
        "openai",
        ProviderCfg {
            models: vec!["test".into()],
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(cfg.providers["openai"].api_key_env, "OPENAI_API_KEY");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
    let cfg = Config::load().unwrap();
    assert_eq!(cfg.providers["my-llm"].api_key.as_deref(), Some("s\"k"));
    assert_eq!(cfg.providers["openai"].api_key_env, "OPENAI_API_KEY");
    assert_eq!(
        cfg.model_list(),
        ["ollama/llama", "my-llm/a", "my-llm/b", "openai/test"]
    );
    assert!(provider::make(&cfg, "my-llm/a").is_ok());
    std::fs::remove_file(path).unwrap();
}

#[test]
fn claude_and_codex_sessions_import_as_text_only_native_threads() {
    use greencode_core::{import, session::Session};
    let project = dir("import");
    let cwd = project.to_str().unwrap();
    let claude = project.join("claude.jsonl");
    let lines = [
        json!({"type":"ai-title","aiTitle":"x"}),
        json!({"type":"user","cwd":cwd,"isMeta":true,"message":{"role":"user","content":"caveat"}}),
        json!({"type":"user","cwd":cwd,"message":{"role":"user","content":"fix the bug\nplease"}}),
        json!({"type":"assistant","message":{"role":"assistant","content":[{"type":"thinking","thinking":"hm"},{"type":"text","text":"Looking."},{"type":"tool_use","id":"t","name":"Bash","input":{}}]}}),
        json!({"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t","content":"out"}]}}),
        json!({"type":"assistant","isSidechain":true,"message":{"role":"assistant","content":[{"type":"text","text":"sub-agent"}]}}),
        json!({"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"Fixed."}]}}),
        json!({"type":"user","message":{"role":"user","content":[{"type":"text","text":"<system-reminder>x</system-reminder>"},{"type":"text","text":"thanks"}]}}),
    ];
    std::fs::write(&claude, lines.map(|l| l.to_string()).join("\n")).unwrap();
    let source = std::fs::read(&claude).unwrap();
    let meta = import::import(&claude, "ollama/m").unwrap();
    assert_eq!(
        (meta.title.as_str(), meta.backend.as_str(), &meta.cwd),
        ("fix the bug", "native", &project)
    );
    let (_, messages) = Session::load(&meta.id).unwrap();
    assert_eq!(
        serde_json::to_value(&messages).unwrap(),
        json!([
            {"role":"user","content":[{"type":"text","text":"fix the bug\nplease"}]},
            {"role":"assistant","content":[{"type":"text","text":"Looking."},{"type":"text","text":"Fixed."}]},
            {"role":"user","content":[{"type":"text","text":"thanks"}]},
        ])
    );
    assert_eq!(std::fs::read(&claude).unwrap(), source);

    let codex_home = project.join("codex");
    let day = codex_home.join("sessions/2026/11/12");
    std::fs::create_dir_all(&day).unwrap();
    let lines = [
        json!({"type":"session_meta","payload":{"cwd":cwd,"base_instructions":"big"}}),
        json!({"type":"response_item","payload":{"type":"message","role":"developer","content":[{"type":"input_text","text":"rules"}]}}),
        json!({"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"<environment_context>x"}]}}),
        json!({"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"old"}]}}),
        json!({"type":"compacted","payload":{"message":"summary so far"}}),
        json!({"type":"response_item","payload":{"type":"reasoning","encrypted_content":"x"}}),
        json!({"type":"response_item","payload":{"type":"custom_tool_call","name":"shell","input":"ls"}}),
        json!({"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"done"}]}}),
    ];
    std::fs::write(
        day.join("rollout-a.jsonl"),
        lines.map(|l| l.to_string()).join("\n"),
    )
    .unwrap();
    std::fs::write(
        day.join("rollout-b.jsonl"),
        json!({"type":"session_meta","payload":{"cwd":"/elsewhere"}}).to_string(),
    )
    .unwrap();
    std::env::set_var("CODEX_HOME", &codex_home);
    std::env::set_var("CLAUDE_CONFIG_DIR", project.join("no-claude"));
    let found = import::discover(&project);
    assert_eq!(
        found
            .iter()
            .map(|f| (f.source, f.title.as_str()))
            .collect::<Vec<_>>(),
        [("Codex", "old")]
    );
    let (_, messages) =
        Session::load(&import::import_found(&found[0], "ollama/m").unwrap().id).unwrap();
    assert_eq!(
        serde_json::to_value(&messages).unwrap(),
        json!([
            {"role":"user","content":[{"type":"text","text":"summary so far"}]},
            {"role":"assistant","content":[{"type":"text","text":"done"}]},
        ])
    );
    assert!(import::import(&day.join("rollout-b.jsonl"), "ollama/m").is_err());
}

#[test]
fn import_file_detects_native_jsonl_and_opencode_json() {
    use greencode_core::{import, session::Session};
    let project = dir("import-file");
    let claude = project.join("claude.jsonl");
    std::fs::write(&claude, [
        json!({"type":"user","cwd":project,"message":{"role":"user","content":"open this"}}).to_string(),
        json!({"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"done"}]}}).to_string(),
    ].join("\n")).unwrap();
    let meta = import::import_file(&claude, "ollama/m").unwrap();
    let (_, messages) = Session::load(&meta.id).unwrap();
    assert_eq!(messages.len(), 2);
    assert_eq!(meta.title, "open this");

    let export = project.join("export.json");
    std::fs::write(&export, json!({
        "info": {"location": {"directory": project}},
        "messages": [{"type":"user","text":"from export"}, {"type":"assistant","content":[{"type":"text","text":"ok"}]}]
    }).to_string()).unwrap();
    let meta = import::import_file(&export, "ollama/m").unwrap();
    assert_eq!(meta.title, "from export");
}

#[test]
fn opencode_session_export_imports_as_text_only_native_thread() {
    use greencode_core::{import, session::Session};
    let project = dir("opencode-import");
    let export = project.join("session.json");
    let lines = [
        json!({"id":"m0","type":"assistant","content":[{"type":"text","text":"orphan"}]}),
        json!({"id":"m1","type":"user","text":"start"}),
        json!({"id":"m2","type":"assistant","content":[
            {"type":"reasoning","text":"hm"},
            {"type":"tool","name":"shell","state":{"status":"completed"}},
            {"type":"text","text":"first answer"}
        ]}),
        json!({"id":"m3","type":"model-switched","model":{"id":"x"}}),
        // The newest summary replaces the history it compacted.
        json!({"id":"m4","type":"compaction","summary":"summary so far"}),
        json!({"id":"m5","type":"user","text":"next"}),
        json!({"id":"m6","type":"assistant","content":[{"type":"text","text":"second answer"}]}),
        json!({"id":"m7","type":"system","text":"today"}),
    ];
    std::fs::write(
        &export,
        json!({
            "info": {"location": {"directory": project.to_str().unwrap()}},
            "messages": lines,
        })
        .to_string(),
    )
    .unwrap();
    let source = std::fs::read(&export).unwrap();
    let meta = import::import_export(&export, "ollama/m").unwrap();
    assert_eq!(
        (meta.title.as_str(), meta.backend.as_str(), &meta.cwd),
        ("summary so far", "native", &project)
    );
    let (_, messages) = Session::load(&meta.id).unwrap();
    assert_eq!(
        serde_json::to_value(&messages).unwrap(),
        json!([
            {"role":"user","content":[{"type":"text","text":"summary so far"},{"type":"text","text":"next"}]},
            {"role":"assistant","content":[{"type":"text","text":"second answer"}]},
        ])
    );
    assert_eq!(std::fs::read(&export).unwrap(), source);
}

#[test]
fn computer_use_asks_per_app_and_screenshots_reach_openai_after_tool_results() {
    let project = dir("computer");
    let perms = Perms::new(Mode::AutoEdit);
    assert!(perms
        .check("computer", &json!({"action":"list_apps"}), &project)
        .is_none());
    assert_eq!(
        perms
            .check(
                "computer",
                &json!({"action":"click","app":"Notes","index":1}),
                &project
            )
            .as_deref(),
        Some("computer:notes")
    );
    assert!(Perms::new(Mode::Full)
        .check(
            "computer",
            &json!({"action":"click","app":"Notes"}),
            &project
        )
        .is_none());
    assert_eq!(
        tools::defs(false, &[]).iter().any(|t| t.name == "computer"),
        cfg!(target_os = "macos")
    );

    let image =
        json!({"type":"image","source":{"type":"base64","media_type":"image/jpeg","data":"QUJD"}});
    let messages: Vec<Message> = serde_json::from_value(json!([
        {"role":"user","content":[{"type":"text","text":"go"}]},
        {"role":"assistant","content":[{"type":"tool_use","id":"c1","name":"computer","input":{}}]},
        {"role":"user","content":[{"type":"tool_result","tool_use_id":"c1","content":"0 Window"}, image]},
    ])).unwrap();
    // Stored and replayed to Anthropic verbatim.
    assert_eq!(
        serde_json::to_value(&messages[2]).unwrap()["content"][1],
        image
    );
    let converted = openai::convert("sys", &messages);
    assert_eq!(converted[3]["role"], "tool");
    assert_eq!(
        converted[4],
        json!({"role":"user","content":[{"type":"image_url","image_url":{"url":"data:image/jpeg;base64,QUJD"}}]})
    );
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn computer_helper_builds_and_answers_without_touching_any_app() {
    dir("computer-helper");
    if std::process::Command::new("swiftc")
        .arg("--version")
        .output()
        .is_err()
    {
        return;
    }
    let cfg = greencode_core::config::Config::default();
    let (apps, image) = tools::computer::run(&cfg, &json!({"action":"list_apps"}))
        .await
        .unwrap();
    assert!(image.is_none(), "{apps}");
    // Errors come back as tool errors and the helper stays usable.
    let e = tools::computer::run(&cfg, &json!({})).await.unwrap_err();
    assert!(e.to_string().contains("`action` is required"), "{e}");
    assert!(tools::computer::run(&cfg, &json!({"action":"list_apps"}))
        .await
        .is_ok());
}

#[tokio::test]
async fn provider_models_are_listed_from_the_models_endpoint() {
    use greencode_core::{config::ProviderCfg, provider};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base_url = format!("http://{}/v1", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut buf = vec![0; 4096];
        let n = socket.read(&mut buf).await.unwrap();
        let body = r#"{"data":[{"id":"a"},{"id":"org/b"}]}"#;
        socket.write_all(format!("HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
        String::from_utf8_lossy(&buf[..n]).to_lowercase()
    });
    let mut cfg = Config::default();
    cfg.providers.insert(
        "local".into(),
        ProviderCfg {
            kind: "openai".into(),
            base_url,
            api_key: Some("sk".into()),
            ..Default::default()
        },
    );
    assert_eq!(
        provider::list_models(&cfg, "local").await.unwrap(),
        ["local/a", "local/org/b"]
    );
    let request = server.await.unwrap();
    assert!(
        request.starts_with("get /v1/models ") && request.contains("authorization: bearer sk"),
        "{request}"
    );
    assert!(provider::list_models(&cfg, "missing").await.is_err());
}

/// A DeepSeek-style stream: reasoning deltas, text, cache-hit usage.
#[tokio::test]
async fn deepseek_stream_keeps_reasoning_and_cache_hits() {
    use greencode_core::provider::{self, Request};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut buf = Vec::new();
        let mut chunk = [0u8; 4096];
        // Headers and body arrive separately; keep reading until the JSON body is complete.
        while !String::from_utf8_lossy(&buf).contains("max_tokens") {
            let n = socket.read(&mut chunk).await.unwrap();
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&chunk[..n]);
        }
        let events = [
            json!({"choices":[{"delta":{"reasoning_content":"plan "}}]}),
            json!({"choices":[{"delta":{"reasoning_content":"the edit"}}]}),
            json!({"choices":[{"delta":{"content":"done"},"finish_reason":"stop"}]}),
            json!({"choices":[],"usage":{"prompt_tokens":100,"completion_tokens":2,"prompt_cache_hit_tokens":70}}),
        ];
        let body: String = events
            .iter()
            .map(|e| format!("data: {e}\n\n"))
            .collect::<String>()
            + "data: [DONE]\n\n";
        socket.write_all(format!("HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
        String::from_utf8_lossy(&buf).to_lowercase()
    });
    let mut cfg = Config::default();
    cfg.providers.insert(
        "deepseek".into(),
        greencode_core::config::ProviderCfg {
            kind: "openai".into(),
            base_url: format!("{base}/v1"),
            api_key: Some("sk".into()),
            ..Default::default()
        },
    );
    let (p, model) = provider::make(&cfg, "deepseek/deepseek-reasoner").unwrap();
    let messages = vec![Message::user("hi")];
    let tools = tools::defs(false, &[]);
    let reply = p
        .complete(
            Request {
                model: &model,
                system: "sys",
                messages: &messages,
                tools: &tools,
                max_tokens: 64_000,
                live: None,
            },
            &|_| {},
        )
        .await
        .unwrap();
    assert!(
        matches!(&reply.blocks[0], greencode_core::Block::Other(v) if v["thinking"] == "plan the edit")
    );
    assert!(matches!(&reply.blocks[1], greencode_core::Block::Text { text } if text == "done"));
    assert_eq!(
        (
            reply.usage.input,
            reply.usage.cache_read,
            reply.usage.output
        ),
        (30, 70, 2)
    );
    let request = server.await.unwrap();
    assert!(
        request.starts_with("post /v1/chat/completions "),
        "{request}"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn anthropic_oauth_request_uses_subscription_identity() {
    use greencode_core::provider::anthropic_oauth::StoredTokens;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let tmp = std::env::temp_dir().join(format!("greencode-oauth-wire-{}", std::process::id()));
    std::fs::create_dir_all(&tmp).unwrap();
    let path = tmp.join("anthropic-oauth.json");
    StoredTokens {
        access_token: "oat".into(),
        refresh_token: "r".into(),
        expires_at: u64::MAX,
    }
    .save(&path)
    .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut buf = Vec::new();
        let mut chunk = vec![0; 4096];
        let (head, body) = loop {
            let n = socket.read(&mut chunk).await.unwrap();
            assert!(n > 0, "connection closed before the full request arrived");
            buf.extend_from_slice(&chunk[..n]);
            let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") else {
                continue;
            };
            let head = String::from_utf8_lossy(&buf[..end]).to_lowercase();
            let len: usize = head
                .lines()
                .find_map(|l| l.strip_prefix("content-length:"))
                .unwrap()
                .trim()
                .parse()
                .unwrap();
            if buf.len() >= end + 4 + len {
                break (
                    head,
                    serde_json::from_slice::<Value>(&buf[end + 4..end + 4 + len]).unwrap(),
                );
            }
        };
        let events = [
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"t1","name":"mcp__read","input":{}}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"path\":\"x\"}"}}"#,
            r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":1}}"#,
        ];
        let mut reply = String::from(
            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n",
        );
        for e in events {
            reply += &format!("data: {e}\n\n");
        }
        socket.write_all(reply.as_bytes()).await.unwrap();
        (head, body)
    });
    let a = anthropic::Anthropic {
        http: Default::default(),
        base: format!("http://{addr}"),
        key: "unused".into(),
        oauth: Some(path),
        effort: None,
        wire: Default::default(),
    };
    let msgs = vec![
        Message::user("hi"),
        Message {
            role: Role::Assistant,
            content: vec![call("t0", "read", json!({"path":"x"}))],
        },
        Message {
            role: Role::User,
            content: vec![Block::ToolResult {
                tool_use_id: "t0".into(),
                content: "ok".into(),
                is_error: false,
            }],
        },
    ];
    let tools = tools::defs(false, &[]);
    let reply = a
        .complete(
            Request {
                model: "claude-opus-5-5",
                system: "s",
                messages: &msgs,
                tools: &tools,
                max_tokens: 10,
                live: None,
            },
            &|_| {},
        )
        .await
        .unwrap();
    let (head, body) = server.await.unwrap();
    for want in [
        "authorization: bearer oat",
        "anthropic-beta: claude-code-20250219,oauth-2025-04-20",
        "x-app: cli",
        "user-agent: claude-cli/",
    ] {
        assert!(head.contains(want), "{want} missing in {head}");
    }
    assert!(!head.contains("x-api-key"), "{head}");
    assert_eq!(
        body["system"][0]["text"],
        "You are Claude Code, Anthropic's official CLI for Claude."
    );
    assert_eq!(body["system"][1]["cache_control"]["type"], "ephemeral");
    assert_eq!(body["tools"][0]["name"], "mcp__read");
    assert_eq!(body["messages"][1]["content"][0]["name"], "mcp__read");
    assert!(
        matches!(&reply.blocks[0], Block::ToolUse { name, .. } if name == "read"),
        "{:?}",
        reply.blocks
    );
    let _ = std::fs::remove_dir_all(&tmp);
}

#[tokio::test]
async fn search_todo_and_background_tools() {
    let d = dir("search-tools");
    std::fs::create_dir_all(d.join("src")).unwrap();
    std::fs::write(d.join("src/a.rs"), "fn alpha() {}\nfn beta() {}\n").unwrap();
    std::fs::write(d.join("notes.md"), "alpha notes\n").unwrap();
    let found = tools::run("grep", &json!({"pattern": "alp.a", "glob": "*.rs"}), &d)
        .await
        .unwrap();
    // Project-relative paths: an absolute prefix on every line costs tokens on each later request.
    assert_eq!(found.trim(), "src/a.rs:1:fn alpha() {}", "{found}");
    let sub = tools::run("grep", &json!({"pattern": "beta", "path": "src"}), &d).await.unwrap();
    assert_eq!(sub.trim(), "src/a.rs:2:fn beta() {}", "{sub}");
    assert_eq!(
        tools::run("grep", &json!({"pattern": "zzz-nothing"}), &d)
            .await
            .unwrap(),
        "no matches"
    );
    let files = tools::run("glob", &json!({"pattern": "*.rs"}), &d)
        .await
        .unwrap();
    assert_eq!(files.trim(), "src/a.rs", "{files}");
    let todo = tools::run(
        "todo",
        &json!({"todos": [{"text": "one", "done": true}, {"text": "two"}]}),
        &d,
    )
    .await
    .unwrap();
    assert_eq!(todo, "☑ one\n☐ two");

    let started = tools::run(
        "bash",
        &json!({"command": "echo from-job", "background": true}),
        &d,
    )
    .await
    .unwrap();
    let log = started
        .split("Output: ")
        .nth(1)
        .unwrap()
        .split(" (use")
        .next()
        .unwrap()
        .to_string();
    for _ in 0..50 {
        if std::fs::read_to_string(&log).is_ok_and(|s| s.contains("from-job")) {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert_eq!(std::fs::read_to_string(&log).unwrap(), "from-job\n");

    let perms = Perms::new(Mode::Ask);
    assert!(perms.check("grep", &json!({"pattern": "x"}), &d).is_none());
    assert_eq!(
        perms
            .check("web_fetch", &json!({"url": "https://example.com/a?b"}), &d)
            .as_deref(),
        Some("web_fetch:example.com")
    );
    assert!(perms
        .check("web_search", &json!({"query": "x"}), &d)
        .is_none());
}

#[test]
fn web_pages_and_search_results_become_text() {
    let page = "<html><head><style>p{}</style><script>var a = '<p>';</script></head><body><h1>Title</h1><p>One &amp; two<br>three</p></body></html>";
    assert_eq!(tools::html_text(page), "Title\nOne & two\nthree");
    let results = r#"<div><a rel="nofollow" class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2Fa%3Fx%3D1&amp;rut=abc">Example <b>Site</b></a>
        <a class="result__snippet" href="x">A <b>fine</b> snippet</a></div>"#;
    assert_eq!(
        tools::search_results(results),
        "Example Site\nhttps://example.com/a?x=1\nA fine snippet"
    );
    assert_eq!(tools::search_results("<html></html>"), "no results");
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn sandboxed_bash_cannot_write_outside_the_project() {
    use std::sync::atomic::Ordering;
    let d = dir("sandbox");
    let outside = greencode_core::user_home()
        .join(format!(".greencode-sandbox-probe-{}", std::process::id()));
    tools::SANDBOX.store(true, Ordering::Relaxed);
    let inside = tools::run(
        "bash",
        &json!({"command": "echo ok > inside.txt && cat inside.txt"}),
        &d,
    )
    .await;
    let escaped = tools::run(
        "bash",
        &json!({"command": format!("echo no > '{}'", outside.display())}),
        &d,
    )
    .await;
    tools::SANDBOX.store(false, Ordering::Relaxed);
    assert_eq!(inside.unwrap(), "ok\n");
    assert!(escaped.is_err() && !outside.exists(), "{escaped:?}");
}

#[tokio::test]
async fn plan_mode_blocks_changes_and_undo_restores_files_and_history() {
    let mock = Mock::new(vec![
        vec![
            call("p1", "write", json!({"path": "plan.txt", "content": "x"})),
            call("p2", "read", json!({"path": "keep.txt"})),
        ],
        vec![text("here is the plan")],
        vec![
            call(
                "w1",
                "write",
                json!({"path": "keep.txt", "content": "changed"}),
            ),
            call("w2", "write", json!({"path": "new.txt", "content": "new"})),
        ],
        vec![
            Block::Other(json!({"type": "thinking", "thinking": "let me think", "signature": "s"})),
            text("edited"),
        ],
    ]);
    let (mut a, meta) = agent("plan-undo", Mode::Plan, &mock);
    std::fs::write(meta.cwd.join("keep.txt"), "original").unwrap();
    let (r, events) = run(&mut a, "plan it", Decision::Allow).await;
    r.unwrap();
    let results = results(&events);
    assert!(
        results[0].1 && results[0].0.contains("plan mode"),
        "{results:?}"
    );
    assert_eq!(results[1], ("original".to_string(), false));
    assert!(!meta.cwd.join("plan.txt").exists());

    let perms = Arc::new(Perms::new(Mode::Full));
    let mut cfg = Config::default();
    cfg.learning.auto = false;
    let mut a = Agent::with_provider(Arc::new(cfg), &meta, perms, mock.clone(), "m".into());
    let (_, events) = run(&mut a, "now edit", Decision::Deny).await;
    assert!(events
        .iter()
        .any(|e| matches!(e, Event::Thinking(t) if t == "let me think")));
    assert_eq!(
        std::fs::read_to_string(meta.cwd.join("keep.txt")).unwrap(),
        "changed"
    );
    assert!(meta.cwd.join("new.txt").exists());

    // A fork keeps the whole history; undo removes the last turn and puts the files back.
    let fork = Session::fork(&meta.id).unwrap();
    assert_eq!(
        Session::load(&fork.id).unwrap().1.len(),
        Session::load(&meta.id).unwrap().1.len()
    );
    assert_eq!(fork.title, "plan it (fork)");
    let markdown = Session::markdown(&meta.id).unwrap();
    assert!(
        markdown.contains("## User\n\nnow edit")
            && markdown.contains("**write**")
            && markdown.contains("## Assistant\n\nedited"),
        "{markdown}"
    );
    assert_eq!(Session::search("NOW EDIT").len(), 2);

    assert_eq!(
        Session::undo(&meta.id).unwrap().as_deref(),
        Some("now edit")
    );
    assert_eq!(
        std::fs::read_to_string(meta.cwd.join("keep.txt")).unwrap(),
        "original"
    );
    assert!(!meta.cwd.join("new.txt").exists());
    let (_, messages) = Session::load(&meta.id).unwrap();
    assert!(
        matches!(&messages.last().unwrap().content[..], [Block::Text { text }] if text == "here is the plan")
    );
    assert_eq!(Session::undo(&meta.id).unwrap().as_deref(), Some("plan it"));
    assert_eq!(Session::undo(&meta.id).unwrap(), None);
}

#[tokio::test]
async fn images_named_in_a_prompt_are_attached_and_priced_usage_adds_up() {
    let mock = Mock::new(vec![vec![text("a red dot")]]);
    let (mut a, meta) = agent("attach", Mode::Full, &mock);
    std::fs::write(meta.cwd.join("dot.png"), [0x89, b'P', b'N', b'G']).unwrap();
    run(
        &mut a,
        "what is in @dot.png and missing.png?",
        Decision::Deny,
    )
    .await
    .0
    .unwrap();
    let sent: Value = serde_json::from_str(&mock.seen.lock().unwrap()[0].1).unwrap();
    assert_eq!(sent[1]["content"].as_array().unwrap().len(), 2);
    assert_eq!(
        sent[1]["content"][1],
        json!({"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "iVBORw=="}})
    );
    // OpenAI-compatible servers get the image as its own user message before the text.
    let converted = openai::convert("s", &a.messages);
    assert_eq!(
        converted[1]["content"][0]["image_url"]["url"],
        "data:image/png;base64,iVBORw=="
    );
    assert_eq!(
        converted[2]["content"],
        "what is in @dot.png and missing.png?"
    );

    let config: Config =
        toml::from_str("sandbox = true\n[prices]\n'p/m' = [3.0, 15.0, 0.3]").unwrap();
    let usage = greencode_core::Usage {
        input: 1_000_000,
        output: 100_000,
        cache_read: 2_000_000,
        cache_write: 1_000_000,
    };
    assert!(
        config.sandbox
            && (config.cost("p/m", &usage).unwrap() - (3.0 + 1.5 + 0.6 + 3.0)).abs() < 1e-9
    );
    assert!(config.cost("p/other", &usage).is_none());
}

fn ext(tool: &str, args: Value) -> Value {
    json!({"action": "call", "tool": tool, "args": args})
}

/// The tag `hread` reports for a file.
async fn tag_of(dir: &std::path::Path, path: &str) -> String {
    let read = tools::ext::run("hread", &json!({"path": path}), dir)
        .await
        .unwrap();
    read.split_once('#')
        .unwrap()
        .1
        .split_once(']')
        .unwrap()
        .0
        .to_string()
}

#[tokio::test]
async fn ext_help_and_unknown_tool() {
    let mock = Mock::new(vec![
        vec![
            call("a", "ext", json!({"action": "help", "tool": "hedit"})),
            call("b", "ext", json!({"action": "help", "tool": "nope"})),
        ],
        vec![text("done")],
    ]);
    let (mut a, _) = agent("ext-help", Mode::Full, &mock);
    let (r, events) = run(&mut a, "go", Decision::Deny).await;
    r.unwrap();
    let out = results(&events);
    assert!(!out[0].1 && out[0].0.contains("\"ops\""), "{out:?}");
    assert!(
        out[1].1
            && out[1]
                .0
                .contains("available: hread, hedit, ast, eval, debug, advisor"),
        "{out:?}"
    );
}

#[tokio::test]
async fn find_ranks_files_by_query_word_coverage() {
    let d = dir("ext-find");
    std::fs::write(
        d.join("retry.rs"),
        "fn x() {}\n// retry the upload\nfn backoff_upload() {}\n",
    )
    .unwrap();
    std::fs::write(
        d.join("upload.rs"),
        "// upload only\n// upload again\n// upload thrice\n",
    )
    .unwrap();
    std::fs::write(d.join("none.rs"), "nothing here\n").unwrap();
    let out = tools::ext::run(
        "find",
        &json!({"query": "Where does the upload retry?"}),
        &d,
    )
    .await
    .unwrap();
    let lines: Vec<&str> = out.lines().collect();
    assert!(
        lines[0].starts_with("retry.rs:2-3 1.00 2: // retry the upload"),
        "{out}"
    );
    assert!(lines[1].starts_with("upload.rs:1-3 0.50"), "{out}");
    assert_eq!(lines.len(), 2, "{out}");
}

#[tokio::test]
async fn rewind_drops_exploration_since_checkpoint_and_survives_reload() {
    let mock = Mock::new(vec![
        vec![call(
            "c",
            "ext",
            ext("checkpoint", json!({"goal": "find it"})),
        )],
        vec![call("r", "read", json!({"path": "big.txt"}))],
        vec![call(
            "w",
            "ext",
            ext("rewind", json!({"report": "REPORT: answer is 42"})),
        )],
        vec![text("done")],
    ]);
    let (mut a, meta) = agent("ext-rewind", Mode::Full, &mock);
    std::fs::write(meta.cwd.join("big.txt"), "EXPLORED_BYTES").unwrap();
    run(&mut a, "start", Decision::Deny).await.0.unwrap();
    let last = mock.seen.lock().unwrap()[3].1.clone();
    assert!(
        !last.contains("EXPLORED_BYTES")
            && last.contains("REPORT: answer is 42")
            && last.contains("start"),
        "{last}"
    );
    let saved = serde_json::to_string(&Session::load(&meta.id).unwrap().1).unwrap();
    assert!(
        !saved.contains("EXPLORED_BYTES")
            && saved.contains("REPORT: answer is 42")
            && saved.contains("done"),
        "{saved}"
    );
}

#[tokio::test]
async fn rewind_preserves_steering_and_background_results_once() {
    let mock = Mock::routed(
        vec![
            vec![call("c", "ext", ext("checkpoint", json!({})))],
            vec![call(
                "t",
                "task",
                json!({"background":true,"tasks":[{"name":"scan","prompt":"scan-marker"}]}),
            )],
            vec![text("waiting for scan")],
            vec![call(
                "w",
                "ext",
                ext("rewind", json!({"report":"scan report"})),
            )],
            vec![text("done")],
        ],
        vec![("scan-marker", vec![vec![text("EXACT_BACKGROUND_RESULT")]])],
    );
    let (mut a, meta) = agent("rewind-external-messages", Mode::Full, &mock);
    *mock.steer.lock().unwrap() = Some((
        2,
        a.live().unwrap(),
        json!({"type":"steer", "message":"EXACT_USER_STEER"}),
    ));
    let gate = Arc::new(tokio::sync::Notify::new());
    mock.gates
        .lock()
        .unwrap()
        .push(("scan-marker".into(), gate.clone()));
    let watched = mock.clone();
    let release = async move {
        while watched
            .seen
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, m)| !m.contains("# Role: "))
            .count()
            < 3
        {
            tokio::task::yield_now().await;
        }
        gate.notify_one();
    };
    let ((result, _), _) = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        tokio::join!(run(&mut a, "explore", Decision::Deny), release)
    })
    .await
    .unwrap();
    result.unwrap();
    let loaded = serde_json::to_string(&Session::load(&meta.id).unwrap().1).unwrap();
    assert_eq!(serde_json::to_string(&a.messages).unwrap(), loaded);
    let seen = mock.seen.lock().unwrap();
    for history in [&loaded, &seen.last().unwrap().1] {
        assert_eq!(history.matches("EXACT_USER_STEER").count(), 1);
        assert_eq!(history.matches("EXACT_BACKGROUND_RESULT").count(), 1);
        assert!(history.contains("scan report") && !history.contains("waiting for scan"));
    }
}

#[tokio::test]
async fn rewind_without_checkpoint_is_refused() {
    let mock = Mock::new(vec![
        vec![call("w", "ext", ext("rewind", json!({"report": "x"})))],
        vec![text("done")],
    ]);
    let (mut a, _) = agent("ext-rewind-none", Mode::Full, &mock);
    let (r, events) = run(&mut a, "go", Decision::Deny).await;
    r.unwrap();
    assert!(results(&events)[0].1 && results(&events)[0].0.contains("no checkpoint"));
}

#[tokio::test]
async fn new_context_replaces_history_with_handoff() {
    let mock = Mock::new(vec![
        vec![call(
            "n",
            "ext",
            ext("new_context", json!({"handoff": "HANDOFF: keep going"})),
        )],
        vec![text("done")],
    ]);
    let (mut a, meta) = agent("ext-new-context", Mode::Full, &mock);
    run(&mut a, "OLD_PROMPT", Decision::Deny).await.0.unwrap();
    let last = mock.seen.lock().unwrap()[1].1.clone();
    assert!(
        !last.contains("OLD_PROMPT") && last.contains("HANDOFF: keep going"),
        "{last}"
    );
    let saved = serde_json::to_string(&Session::load(&meta.id).unwrap().1).unwrap();
    assert!(
        !saved.contains("OLD_PROMPT") && saved.contains("HANDOFF: keep going"),
        "{saved}"
    );
}

#[tokio::test]
async fn ask_returns_the_users_answers_and_settings_can_turn_it_off() {
    let mock = Mock::new(vec![
        vec![call(
            "q",
            "ext",
            ext(
                "ask",
                json!({"questions": [{"question": "which?", "options": ["A", "B"]}]}),
            ),
        )],
        vec![text("done")],
    ]);
    let (mut a, _) = agent("ext-ask", Mode::Full, &mock);
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let fut = a.prompt("go".into(), &tx);
    tokio::pin!(fut);
    let mut output = None;
    loop {
        tokio::select! {
            r = &mut fut => { r.unwrap(); break; }
            Some(ev) = rx.recv() => match ev {
                Event::Dialog { request, reply } => {
                    assert_eq!(request["questions"][0]["options"][1]["label"], "B");
                    reply.send(json!({"answers": [{"id": "q1", "selectedOptions": ["B"]}]})).unwrap();
                }
                Event::ToolEnd { output: o, is_error, .. } => { assert!(!is_error, "{o}"); output = Some(o); }
                _ => {}
            },
        }
    }
    while let Ok(ev) = rx.try_recv() {
        if let Event::ToolEnd { output: o, .. } = ev {
            output = Some(o);
        }
    }
    assert!(output.unwrap().contains("\"selectedOptions\":[\"B\"]"));

    let mock = Mock::new(vec![
        vec![call(
            "q",
            "ext",
            ext("ask", json!({"questions": [{"question": "x"}]})),
        )],
        vec![text("done")],
    ]);
    let meta = Meta::new(dir("ext-ask-off"), "mock/m".into(), "native".into());
    let mut cfg = Config::default();
    cfg.ext_off = vec!["ask".into()];
    let mut a = Agent::with_provider(
        Arc::new(cfg),
        &meta,
        Arc::new(Perms::new(Mode::Full)),
        mock.clone(),
        "m".into(),
    );
    let (r, events) = run(&mut a, "go", Decision::Deny).await;
    r.unwrap();
    assert!(results(&events)[0].1 && results(&events)[0].0.contains("turned off"));
    let prefix = mock.seen.lock().unwrap()[0].0.clone();
    assert!(
        prefix.contains("hread (") && !prefix.contains("ask (ask the user"),
        "{prefix}"
    );
}

#[tokio::test]
async fn secret_tool_stores_the_value_outside_the_transcript() {
    let mock = Mock::new(vec![
        vec![call(
            "s",
            "ext",
            ext("secret", json!({"name": "DEPLOY_TOKEN", "reason": "deploy"})),
        )],
        vec![text("done")],
    ]);
    let (mut a, meta) = agent("ext-secret", Mode::Full, &mock);
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let fut = a.prompt("go".into(), &tx);
    tokio::pin!(fut);
    let mut output = None;
    loop {
        tokio::select! {
            r = &mut fut => { r.unwrap(); break; }
            Some(ev) = rx.recv() => match ev {
                Event::Dialog { request, reply } => {
                    assert_eq!(request["method"], "secret");
                    assert!(request["title"].as_str().unwrap().contains("DEPLOY_TOKEN"));
                    reply.send(json!({"value": "hunter2"})).unwrap();
                }
                Event::ToolEnd { output: o, is_error, .. } => { assert!(!is_error, "{o}"); output = Some(o); }
                _ => {}
            },
        }
    }
    while let Ok(ev) = rx.try_recv() {
        if let Event::ToolEnd { output: o, .. } = ev { output = Some(o); }
    }
    let output = output.unwrap();
    assert!(output.contains("$DEPLOY_TOKEN") && output.contains("secrets/DEPLOY_TOKEN"), "{output}");
    assert!(!output.contains("hunter2"), "{output}");
    assert_eq!(
        greencode_core::secrets::env(),
        vec![("DEPLOY_TOKEN".to_string(), "hunter2".to_string())]
    );
    let saved = serde_json::to_string(&Session::load(&meta.id).unwrap().1).unwrap();
    assert!(!saved.contains("hunter2"), "{saved}");
}

#[tokio::test]
async fn hashline_edit_applies_and_rejects_stale_tag() {
    let d = dir("hashline");
    let file = d.join("f.txt");
    std::fs::write(&file, "a\nb\nc\n").unwrap();
    let read = tools::ext::run("hread", &json!({"path": "f.txt"}), &d)
        .await
        .unwrap();
    let (head, body) = read.split_once('\n').unwrap();
    assert!(head.starts_with("[f.txt#") && head.ends_with(']'), "{head}");
    assert_eq!(body, "1:a\n2:b\n3:c");
    let tag = tag_of(&d, "f.txt").await;
    let edit = |ops: Value, tag: &str| json!({"path": "f.txt", "tag": tag, "ops": ops});
    let ops = json!([{"op": "replace", "start": 2, "text": "B1\nB2"}, {"op": "insert", "start": 0, "text": "top"}, {"op": "delete", "start": 3}]);
    let done = tools::ext::run("hedit", &edit(ops.clone(), &tag), &d)
        .await
        .unwrap();
    assert!(done.starts_with("edited f.txt (+3 −2) [f.txt#"), "{done}");
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "top\na\nB1\nB2\n");
    // The old tag no longer matches, and bad ranges are refused: nothing is written.
    let stale = tools::ext::run("hedit", &edit(ops, &tag), &d)
        .await
        .unwrap_err()
        .to_string();
    assert!(stale.contains("stale tag"), "{stale}");
    let tag = tag_of(&d, "f.txt").await;
    for bad in [
        json!([{"op": "replace", "start": 1, "end": 3, "text": "x"}, {"op": "delete", "start": 2}]),
        json!([{"op": "delete", "start": 9}]),
        json!([]),
    ] {
        assert!(tools::ext::run("hedit", &edit(bad, &tag), &d)
            .await
            .is_err());
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "top\na\nB1\nB2\n");
    }
}

#[tokio::test]
async fn hedit_is_judged_and_undone_like_edit() {
    for (name, mode, refusal) in [
        ("hedit-ask", Mode::Ask, "denied"),
        ("hedit-plan", Mode::Plan, "plan mode"),
        ("hedit-full", Mode::Full, ""),
    ] {
        let d = dir(name);
        std::fs::write(d.join("f.txt"), "orig\n").unwrap();
        let tag = tag_of(&d, "f.txt").await;
        let args = json!({"path": "f.txt", "tag": tag, "ops": [{"op": "replace", "start": 1, "text": "new"}]});
        let mock = Mock::new(vec![
            vec![call("e", "ext", ext("hedit", args))],
            vec![text("done")],
        ]);
        let (mut a, meta) = agent(name, mode, &mock);
        let (r, events) = run(&mut a, "edit", Decision::Deny).await;
        r.unwrap();
        let out = results(&events);
        if refusal.is_empty() {
            assert!(!out[0].1, "{out:?}");
            assert_eq!(std::fs::read_to_string(d.join("f.txt")).unwrap(), "new\n");
            Session::undo(&meta.id).unwrap();
            assert_eq!(std::fs::read_to_string(d.join("f.txt")).unwrap(), "orig\n");
        } else {
            assert!(out[0].1 && out[0].0.contains(refusal), "{name}: {out:?}");
            assert_eq!(std::fs::read_to_string(d.join("f.txt")).unwrap(), "orig\n");
        }
    }
}

#[tokio::test]
async fn learn_ext_uses_memory_permission_boundary() {
    let d = dir("learn-ask");
    let mock = Mock::new(vec![
        vec![call("l", "ext", ext("learn", json!({"text": "one fact"})))],
        vec![text("done")],
    ]);
    let meta = Meta::new(d.clone(), "mock/m".into(), "native".into());
    let mut cfg = Config::default();
    cfg.learning.auto = false;
    let mut a = Agent::with_provider(
        Arc::new(cfg),
        &meta,
        Arc::new(Perms::new(Mode::Ask)),
        mock,
        "m".into(),
    );
    let (result, events) = run(&mut a, "learn", Decision::Deny).await;
    result.unwrap();
    assert!(results(&events)[0].1);
    assert!(!d.join(".greencode/MEMORY.md").exists());
}

#[tokio::test]
async fn automatic_learning_is_enabled_by_default_without_an_extra_request() {
    let d = dir("learn-auto");
    let mock = Mock::new(vec![vec![text("done\n\nLesson: Use the project helper")]]);
    let meta = Meta::new(d.clone(), "mock/m".into(), "native".into());
    let mut cfg = Config::default();
    cfg.learning.max_chars = 8;
    let mut a = Agent::with_provider(
        Arc::new(cfg),
        &meta,
        Arc::new(Perms::new(Mode::Full)),
        mock.clone(),
        "m".into(),
    );
    let (result, events) = run(&mut a, "finish", Decision::Deny).await;
    result.unwrap();
    assert_eq!(
        std::fs::read_to_string(d.join(".greencode/MEMORY.md")).unwrap(),
        "- Use the\n"
    );
    assert_eq!(mock.seen.lock().unwrap().len(), 1);
    assert!(!events
        .iter()
        .any(|event| matches!(event, Event::UsageAux(_))));
}

#[tokio::test]
async fn eval_keeps_state_between_calls() {
    let d = dir("eval");
    let eval = |lang: &'static str, code: &'static str, reset: bool| {
        let d = d.clone();
        async move {
            tools::ext::run(
                "eval",
                &json!({"lang": lang, "code": code, "reset": reset}),
                &d,
            )
            .await
        }
    };
    if greencode_core::config::executable("python3").is_some() {
        eval("py", "x = 41", false).await.unwrap();
        assert_eq!(eval("py", "x + 1", false).await.unwrap().trim(), "42");
        assert!(eval("py", "x", true).await.unwrap().contains("NameError"));
    }
    if greencode_core::config::executable("node")
        .or_else(|| greencode_core::config::executable("bun"))
        .is_some()
    {
        eval("js", "let y = 5", false).await.unwrap();
        assert_eq!(eval("js", "y * 2", false).await.unwrap().trim(), "10");
    }
}

#[tokio::test]
async fn configured_tool_allowlist_rejects_hidden_calls() {
    let mock = Mock::new(vec![
        vec![call("w", "write", json!({"path": "x", "content": "y"}))],
        vec![text("handled")],
    ]);
    let meta = Meta::new(dir("tool-allowlist"), "mock/m".into(), "native".into());
    let mut cfg = Config {
        tools: vec!["read".into()],
        ..Config::default()
    };
    cfg.learning.auto = false;
    let mut a = Agent::with_provider(
        Arc::new(cfg),
        &meta,
        Arc::new(Perms::new(Mode::Full)),
        mock,
        "m".into(),
    );
    let (result, events) = run(&mut a, "write", Decision::Deny).await;
    result.unwrap();
    assert!(results(&events)[0].1 && results(&events)[0].0.contains("disabled in config.toml"));
}

#[tokio::test]
async fn advisor_needs_configuration_and_reports_usage() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let ask = || {
        Mock::new(vec![
            vec![call(
                "q",
                "ext",
                ext("advisor", json!({"question": "which way?"})),
            )],
            vec![text("done")],
        ])
    };
    let mock = ask();
    let (mut a, _) = agent("advisor-off", Mode::Full, &mock);
    let (r, events) = run(&mut a, "go", Decision::Deny).await;
    r.unwrap();
    assert!(results(&events)[0].1 && results(&events)[0].0.contains("advisor is not configured"));

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}/v1", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut buf = Vec::new();
        let mut chunk = [0u8; 4096];
        while !String::from_utf8_lossy(&buf).contains("max_tokens") {
            let n = socket.read(&mut chunk).await.unwrap();
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&chunk[..n]);
        }
        let events = [
            json!({"choices":[{"delta":{"content":"go left"},"finish_reason":"stop"}]}),
            json!({"choices":[],"usage":{"prompt_tokens":10,"completion_tokens":2}}),
        ];
        let body: String = events
            .iter()
            .map(|e| format!("data: {e}\n\n"))
            .collect::<String>()
            + "data: [DONE]\n\n";
        socket.write_all(format!("HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
        String::from_utf8_lossy(&buf).into_owned()
    });
    let mut cfg = Config::default();
    cfg.providers.insert(
        "adv".into(),
        greencode_core::config::ProviderCfg {
            kind: "openai".into(),
            base_url: base,
            api_key: Some("sk".into()),
            ..Default::default()
        },
    );
    cfg.advisor_model = Some("adv/m".into());
    let mock = ask();
    let meta = Meta::new(dir("advisor-on"), "mock/m".into(), "native".into());
    let mut a = Agent::with_provider(
        Arc::new(cfg),
        &meta,
        Arc::new(Perms::new(Mode::Full)),
        mock.clone(),
        "m".into(),
    );
    let (r, events) = run(&mut a, "go", Decision::Deny).await;
    r.unwrap();
    assert_eq!(results(&events)[0], ("go left".to_string(), false));
    // The agent's two requests plus the advisor's.
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, Event::Usage(_)))
            .count(),
        2
    );
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, Event::UsageAux(_)))
            .count(),
        1
    );
    let request = server.await.unwrap();
    assert!(
        request.contains("which way?") && request.contains("\"model\":\"m\""),
        "{request}"
    );
}

#[tokio::test]
async fn rules_fire_once_on_matching_output() {
    let d = dir("rules");
    std::fs::create_dir_all(d.join(".greencode/rules")).unwrap();
    std::fs::write(
        d.join(".greencode/rules/no-unwrap.md"),
        "---\ntrigger: unwrap\\(\\)\n---\nPrefer ? over unwrap().\n",
    )
    .unwrap();
    let mock = Mock::new(vec![
        vec![text("I will call unwrap() here")],
        vec![text("no unwrap() after all")],
    ]);
    let (mut a, _) = agent("rules", Mode::Full, &mock);
    let (r, events) = run(&mut a, "go", Decision::Deny).await;
    r.unwrap();
    let seen = mock.seen.lock().unwrap();
    assert_eq!(seen.len(), 2);
    assert!(
        seen[1].1.contains("Rule \\\"no-unwrap\\\" applies")
            && seen[1].1.contains("Prefer ? over unwrap()."),
        "{}",
        seen[1].1
    );
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, Event::Notify(n) if n.contains("ルール適用")))
            .count(),
        1
    );
}

#[tokio::test]
async fn grandchildren_cannot_spawn() {
    // main -> child -> grandchild run one after another, so the replies are consumed in this order.
    let mock = Mock::new(vec![
        vec![call(
            "m",
            "task",
            json!({"tasks": [{"prompt": "child work"}]}),
        )],
        vec![call(
            "c",
            "task",
            json!({"tasks": [{"prompt": "grandchild work"}]}),
        )],
        vec![call(
            "g",
            "task",
            json!({"tasks": [{"prompt": "great-grandchild work"}]}),
        )],
        vec![text("grandchild done")],
        vec![text("child done")],
        vec![text("all done")],
    ]);
    let (mut a, _) = agent("grandchildren", Mode::Full, &mock);
    let (r, _) = run(&mut a, "go", Decision::Deny).await;
    r.unwrap();
    let seen = mock.seen.lock().unwrap();
    let has_task = |i: usize| seen[i].0.contains("\"name\":\"task\"");
    assert!(
        has_task(0) && has_task(1) && !has_task(2),
        "main, child may spawn; grandchild may not"
    );
    assert!(
        seen[3].1.contains("not available to this agent"),
        "{}",
        seen[3].1
    );
}

#[tokio::test]
async fn learn_ext_writes_project_memory_without_an_extra_model_request() {
    let d = dir("learn-ext");
    let mock = Mock::new(vec![
        vec![call(
            "l",
            "ext",
            ext(
                "learn",
                json!({"scope": "project", "text": "uses cargo test"}),
            ),
        )],
        vec![text("saved")],
    ]);
    let (mut a, _) = agent("learn-ext", Mode::Full, &mock);
    let (result, events) = run(&mut a, "learn this", Decision::Deny).await;
    result.unwrap();
    assert_eq!(
        std::fs::read_to_string(d.join(".greencode/MEMORY.md")).unwrap(),
        "- uses cargo test\n"
    );
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, Event::Usage(_)))
            .count(),
        2
    );
}

#[tokio::test]
async fn dap_session_against_a_fake_adapter() {
    let Some(python) = greencode_core::config::executable("python3") else {
        return;
    };
    let d = dir("dap");
    std::fs::write(d.join("prog.py"), "x = 1\ny = 2\nprint(x + y)\n").unwrap();
    std::fs::write(d.join("adapter.py"), r#"
import json, sys
seq = 0
def read():
    n = 0
    while True:
        line = sys.stdin.buffer.readline()
        if not line: sys.exit(0)
        if line in (b"\r\n", b"\n"): break
        key, _, value = line.decode().partition(":")
        if key.lower() == "content-length": n = int(value)
    return json.loads(sys.stdin.buffer.read(n))
def send(m):
    global seq
    seq += 1
    m["seq"] = seq
    body = json.dumps(m).encode()
    sys.stdout.buffer.write(b"Content-Length: %d\r\n\r\n" % len(body) + body)
    sys.stdout.buffer.flush()
def reply(r, body=None):
    send({"type": "response", "request_seq": r["seq"], "success": True, "command": r["command"], "body": body or {}})
def event(name, body=None):
    send({"type": "event", "event": name, "body": body or {}})
while True:
    r = read()
    c = r["command"]
    if c == "launch":
        reply(r)
        event("initialized")
    elif c == "configurationDone":
        reply(r)
        event("output", {"output": "hello\n"})
        event("stopped", {"reason": "breakpoint", "threadId": 1})
    elif c == "stackTrace":
        reply(r, {"stackFrames": [{"id": 10, "name": "main", "line": 3, "source": {"path": sys.argv[1]}}]})
    elif c == "scopes":
        reply(r, {"scopes": [{"variablesReference": 5}]})
    elif c == "variables":
        reply(r, {"variables": [{"name": "x", "value": "1"}]})
    elif c == "evaluate":
        reply(r, {"result": "3"})
    elif c == "disconnect":
        reply(r)
        sys.exit(0)
    else:
        reply(r)
"#).unwrap();
    let debug = |args: Value| {
        let d = d.clone();
        async move { tools::ext::run("debug", &args, &d).await }
    };
    let adapter = json!([python, d.join("adapter.py"), d.join("prog.py")]);
    assert!(
        debug(json!({"action": "break", "path": "prog.py", "line": 3}))
            .await
            .unwrap()
            .contains("prog.py:3")
    );
    let stopped = debug(json!({"action": "launch", "program": "prog.py", "adapter": adapter}))
        .await
        .unwrap();
    assert!(
        stopped.starts_with("stopped: breakpoint at") && stopped.contains("prog.py:3"),
        "{stopped}"
    );
    assert!(debug(json!({"action": "vars"}))
        .await
        .unwrap()
        .contains("x = 1"));
    assert_eq!(
        debug(json!({"action": "eval", "expr": "x + y"}))
            .await
            .unwrap(),
        "3"
    );
    assert_eq!(debug(json!({"action": "stop"})).await.unwrap(), "stopped");
    assert!(debug(json!({"action": "stack"}))
        .await
        .unwrap_err()
        .to_string()
        .contains("no debug session"));
}

#[tokio::test]
async fn ast_reports_missing_binary_or_matches() {
    let d = dir("ast");
    std::fs::write(d.join("a.js"), "foo(1);\nbar(2);\n").unwrap();
    let found = tools::ext::run("ast", &json!({"pattern": "foo($A)", "lang": "js"}), &d).await;
    if greencode_core::config::executable("ast-grep")
        .or_else(|| greencode_core::config::executable("sg"))
        .is_none()
    {
        assert!(found
            .unwrap_err()
            .to_string()
            .contains("brew install ast-grep"));
    } else {
        let found = found.unwrap();
        assert!(
            found.contains("a.js") && found.contains("foo(1)") && !found.contains("bar"),
            "{found}"
        );
        tools::ext::run(
            "ast",
            &json!({"pattern": "foo($A)", "lang": "js", "path": "a.js", "rewrite": "baz($A)"}),
            &d,
        )
        .await
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(d.join("a.js")).unwrap(),
            "baz(1);\nbar(2);\n"
        );
    }
}

#[tokio::test]
async fn acp_sends_image_blocks_mcp_servers_usage_and_non_text_placeholders() {
    if std::process::Command::new("python3").arg("--version").output().is_err() {
        return; // the fake agent below needs python3
    }
    let d = dir("acp-image");
    std::fs::write(d.join("dot.png"), b"PNG").unwrap();
    std::fs::write(
        d.join(".mcp.json"),
        r#"{"mcpServers":{"calc":{"command":"python3","args":["calc.py"],"env":{"A":"1"}},"web":{"url":"http://x"}}}"#,
    )
    .unwrap();
    std::fs::write(
        d.join("agent.py"),
        r#"
import json, sys
def send(m): print(json.dumps(m), flush=True)
def update(u): send({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": "s1", "update": u}})
for line in sys.stdin:
    m = json.loads(line)
    meth = m.get("method")
    if meth == "initialize":
        send({"jsonrpc": "2.0", "id": m["id"], "result": {"protocolVersion": 1,
              "agentCapabilities": {"promptCapabilities": {"image": True}}}})
    elif meth == "session/new":
        open("new.json", "w").write(json.dumps(m["params"]))
        send({"jsonrpc": "2.0", "id": m["id"], "result": {"sessionId": "s1"}})
    elif meth == "session/prompt":
        open("prompt.json", "w").write(json.dumps(m["params"]["prompt"]))
        update({"sessionUpdate": "usage_update", "used": 42, "size": 200000, "cost": {"amount": 0.5, "currency": "USD"}})
        update({"sessionUpdate": "agent_message_chunk", "content": {"type": "image", "data": "aGk=", "mimeType": "image/png"}})
        update({"sessionUpdate": "agent_message_chunk", "content": {"type": "resource_link", "uri": "file:///tmp/x.txt", "name": "x"}})
        update({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "done"}})
        send({"jsonrpc": "2.0", "id": m["id"], "result": {"stopReason": "end_turn"}})
"#,
    )
    .unwrap();
    let meta = Meta::new(d.clone(), String::new(), "fake".into());
    let mut acp = greencode_core::acp::Acp::start(
        &["python3".to_string(), "agent.py".to_string()],
        &Default::default(),
        &meta,
        Arc::new(Perms::new(Mode::Ask)),
    )
    .await
    .unwrap();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let events = {
        let fut = acp.prompt("look dot.png".into(), &tx);
        tokio::pin!(fut);
        let mut events = Vec::new();
        loop {
            tokio::select! {
                r = &mut fut => { r.unwrap(); break }
                Some(ev) = rx.recv() => events.push(ev),
            }
        }
        while let Ok(ev) = rx.try_recv() { events.push(ev); }
        events
    };
    // The image path becomes a base64 block only because the agent advertised image prompts.
    let prompt: Value = serde_json::from_str(&std::fs::read_to_string(d.join("prompt.json")).unwrap()).unwrap();
    assert_eq!(
        prompt,
        json!([{"type": "text", "text": "look dot.png"}, {"type": "image", "data": "UE5H", "mimeType": "image/png"}])
    );
    // Only the stdio server is handed to the agent; the HTTP one stays behind (no mcpCapabilities.http advertised).
    let params: Value = serde_json::from_str(&std::fs::read_to_string(d.join("new.json")).unwrap()).unwrap();
    assert_eq!(
        params["mcpServers"],
        json!([{"name": "calc", "command": "python3", "args": ["calc.py"], "env": [{"name": "A", "value": "1"}]}])
    );
    // Non-text output is not dropped; usage arrives as a Remote update the app can show.
    let texts: Vec<&str> = events.iter().filter_map(|e| if let Event::Text(t) = e { Some(t.as_str()) } else { None }).collect();
    assert_eq!(texts, ["[画像]", "[リンク: x](file:///tmp/x.txt)", "done"]);
    assert!(events.iter().any(|e| matches!(
        e,
        Event::Remote { command, data }
            if command == "usage_update" && data["used"].as_u64() == Some(42) && data["size"].as_u64() == Some(200_000)
    )));
}

#[tokio::test]
async fn acp_keeps_image_paths_as_text_without_image_capability() {
    if std::process::Command::new("python3").arg("--version").output().is_err() {
        return;
    }
    let d = dir("acp-no-image");
    std::fs::write(d.join("dot.png"), b"PNG").unwrap();
    std::fs::write(
        d.join("agent.py"),
        r#"
import json, sys
def send(m): print(json.dumps(m), flush=True)
for line in sys.stdin:
    m = json.loads(line)
    meth = m.get("method")
    if meth == "initialize":
        send({"jsonrpc": "2.0", "id": m["id"], "result": {"protocolVersion": 1,
              "agentCapabilities": {"promptCapabilities": {"image": False}}}})
    elif meth == "session/new":
        open("new.json", "w").write(json.dumps(m["params"]))
        send({"jsonrpc": "2.0", "id": m["id"], "result": {"sessionId": "s1"}})
    elif meth == "session/prompt":
        open("prompt.json", "w").write(json.dumps(m["params"]["prompt"]))
        send({"jsonrpc": "2.0", "id": m["id"], "result": {"stopReason": "end_turn"}})
"#,
    )
    .unwrap();
    let meta = Meta::new(d.clone(), String::new(), "fake".into());
    let mut acp = greencode_core::acp::Acp::start(
        &["python3".to_string(), "agent.py".to_string()],
        &Default::default(),
        &meta,
        Arc::new(Perms::new(Mode::Ask)),
    )
    .await
    .unwrap();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let fut = acp.prompt("look dot.png".into(), &tx);
    tokio::pin!(fut);
    loop {
        tokio::select! {
            r = &mut fut => { r.unwrap(); break }
            Some(_) = rx.recv() => {}
        }
    }
    let prompt: Value = serde_json::from_str(&std::fs::read_to_string(d.join("prompt.json")).unwrap()).unwrap();
    assert_eq!(prompt, json!([{"type": "text", "text": "look dot.png"}]));
    let params: Value = serde_json::from_str(&std::fs::read_to_string(d.join("new.json")).unwrap()).unwrap();
    assert_eq!(params["mcpServers"], json!([]));
}

/// Streams scripted chunks with a pause between them, so a rule can match while the request is in flight.
struct Streamer {
    calls: Mutex<Vec<Vec<&'static str>>>,
    texts: Mutex<Vec<String>>,
    completed: std::sync::atomic::AtomicUsize,
}

impl Streamer {
    fn new(calls: Vec<Vec<&'static str>>) -> Arc<Streamer> {
        Arc::new(Streamer {
            calls: Mutex::new(calls),
            texts: Mutex::new(Vec::new()),
            completed: Default::default(),
        })
    }
}

impl Provider for Streamer {
    fn complete<'a>(
        &'a self,
        req: Request<'a>,
        on_text: &'a (dyn Fn(&str) + Send + Sync),
    ) -> BoxFut<'a, anyhow::Result<Reply>> {
        Box::pin(async move {
            self.texts.lock().unwrap().push(serde_json::to_string(req.messages).unwrap());
            let chunks = self.calls.lock().unwrap().remove(0);
            let mut all = String::new();
            for chunk in chunks {
                on_text(chunk);
                all.push_str(chunk);
                tokio::time::sleep(std::time::Duration::from_millis(40)).await;
            }
            self.completed.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Ok(Reply { blocks: vec![text(&all)], ..Default::default() })
        })
    }
}

fn rule_dir(name: &str, rule: &str, file: &str) -> PathBuf {
    let d = dir(name);
    std::fs::create_dir_all(d.join(".greencode/rules")).unwrap();
    std::fs::write(d.join(".greencode/rules").join(file), rule).unwrap();
    d
}

fn agent_cfg(name: &str, cfg: Config, provider: Arc<dyn Provider>) -> Agent {
    let meta = Meta::new(dir(name), "mock/m".into(), "native".into());
    Agent::with_provider(Arc::new(cfg), &meta, Arc::new(Perms::new(Mode::Full)), provider, "m".into())
}

#[tokio::test]
async fn ttsr_cancels_the_stream_and_discards_the_partial_reply() {
    rule_dir(
        "ttsr-stream",
        "---\ntrigger: unwrap\\(\\)\n---\nPrefer ? over unwrap().\n",
        "no-unwrap.md",
    );
    let streamer = Streamer::new(vec![
        vec!["I will call ", "unwrap()", " here"],
        vec!["no unwrap after all"],
    ]);
    let mut cfg = Config::default();
    cfg.learning.auto = false;
    let mut a = agent_cfg("ttsr-stream", cfg, streamer.clone());
    let (r, events) = run(&mut a, "go", Decision::Deny).await;
    r.unwrap();
    assert_eq!(
        streamer.completed.load(std::sync::atomic::Ordering::Relaxed),
        1,
        "the interrupted request must never return its reply"
    );
    let texts = streamer.texts.lock().unwrap();
    assert_eq!(texts.len(), 2);
    assert!(texts[1].contains("Rule \\\"no-unwrap\\\" applies"), "{}", texts[1]);
    assert!(texts[1].contains("Prefer ? over unwrap()."), "{}", texts[1]);
    assert_eq!(
        events.iter().filter(|e| matches!(e, Event::Notify(n) if n.contains("ルール適用"))).count(),
        1
    );
    assert!(
        a.messages.iter().all(|m| match m.content.first() {
            Some(Block::Text { text }) => !text.contains("unwrap() here"),
            _ => true,
        }),
        "`contextMode = discard` must not commit the partial reply"
    );
}

#[tokio::test]
async fn ttsr_keep_commits_the_partial_reply_before_the_warning() {
    rule_dir("ttsr-keep", "---\ntrigger: boom\n---\nbody\n", "boom.md");
    let streamer = Streamer::new(vec![vec!["half a sen", "boom"], vec!["all clear now"]]);
    let mut cfg = Config::default();
    cfg.learning.auto = false;
    cfg.ttsr_opts.ttsr.context_mode = greencode_core::config::ttsr::ContextMode::Keep;
    let mut a = agent_cfg("ttsr-keep", cfg, streamer.clone());
    let (r, _) = run(&mut a, "go", Decision::Deny).await;
    r.unwrap();
    assert!(
        a.messages.iter().any(|m| matches!(m.content.first(), Some(Block::Text { text }) if text.contains("half a sen"))),
        "`keep` must commit what streamed before the interrupt"
    );
    let texts = streamer.texts.lock().unwrap();
    assert!(texts[1].contains("Rule \\\"boom\\\" applies"), "{}", texts[1]);
}

#[tokio::test]
async fn ttsr_tool_only_blocks_the_matching_call_and_runs_the_prose() {
    rule_dir(
        "ttsr-tool",
        "---\ntrigger: reset --hard\nscope: tool:bash\ninterruptMode: tool-only\n---\nNever discard work.\n",
        "no-reset.md",
    );
    let mock = Mock::new(vec![
        vec![call("t1", "bash", json!({"command": "git reset --hard"}))],
        vec![text("done")],
    ]);
    let mut cfg = Config::default();
    cfg.learning.auto = false;
    let mut a = agent_cfg("ttsr-tool", cfg, mock);
    let (r, events) = run(&mut a, "go", Decision::Deny).await;
    r.unwrap();
    let ended = events
        .iter()
        .find_map(|e| match e {
            Event::ToolEnd { output, is_error, .. } => Some((output.clone(), *is_error)),
            _ => None,
        })
        .expect("the blocked call still answers the model");
    assert!(ended.1, "the matching tool call must not run: {ended:?}");
    assert!(ended.0.contains("Never discard work."), "{}", ended.0);
}

#[tokio::test]
async fn ttsr_disabled_leaves_streams_and_calls_alone() {
    rule_dir("ttsr-off", "---\ntrigger: unwrap\n---\nbody\n", "no-unwrap.md");
    let streamer = Streamer::new(vec![vec!["calling unwrap now"]]);
    let mut cfg = Config::default();
    cfg.learning.auto = false;
    cfg.ttsr_opts.ttsr.enabled = false;
    let mut a = agent_cfg("ttsr-off", cfg, streamer.clone());
    let (r, _) = run(&mut a, "go", Decision::Deny).await;
    r.unwrap();
    assert_eq!(streamer.completed.load(std::sync::atomic::Ordering::Relaxed), 1);
    assert!(streamer.texts.lock().unwrap().len() == 1, "no retry when rules are off");
}

#[tokio::test]
async fn antigravity_auto_restarts_the_runtime_on_the_sandbox_after_a_connection_failure() {
    if std::process::Command::new("python3").arg("--version").output().is_err() {
        return; // the fake agent below needs python3
    }
    let d = dir("antigravity-endpoint");
    std::fs::write(
        d.join("agent.py"),
        r#"
import json, os, sys
host = os.environ.get("CODE_ASSIST_ENDPOINT", "")
open("calls.log", "a").write(host + " " + os.environ.get("CODE_ASSIST_ENDPOINT_FALLBACK", "") + "\n")
if "sandbox" not in host:
    # A production endpoint that is unreachable: the runtime dies during `initialize`.
    sys.exit(1)
def send(m): print(json.dumps(m), flush=True)
for line in sys.stdin:
    m = json.loads(line)
    meth = m.get("method")
    if meth == "initialize": send({"jsonrpc": "2.0", "id": m["id"], "result": {"protocolVersion": 1}})
    elif meth == "session/new": send({"jsonrpc": "2.0", "id": m["id"], "result": {"sessionId": "s1"}})
    elif meth == "session/prompt": send({"jsonrpc": "2.0", "id": m["id"], "result": {"stopReason": "end_turn"}})
"#,
    )
    .unwrap();
    let meta = Meta::new(d.clone(), String::new(), "fake".into());
    let mut env = std::collections::BTreeMap::new();
    greencode_core::antigravity::endpoint_env(
        greencode_core::config::services_extra::AntigravityEndpoint::Auto,
        &mut env,
    );
    let mut acp = greencode_core::acp::Acp::start(
        &["python3".to_string(), "agent.py".to_string()],
        &env,
        &meta,
        Arc::new(Perms::new(Mode::Ask)),
    )
    .await
    .unwrap();
    let calls = std::fs::read_to_string(d.join("calls.log")).unwrap();
    let hosts: Vec<&str> = calls.lines().map(|line| line.split_once(' ').unwrap().0).collect();
    assert_eq!(hosts.len(), 2, "production runs first, then the sandbox: {calls}");
    assert!(hosts[0].contains("daily-cloudcode-pa.googleapis.com") && !hosts[0].contains("sandbox"), "{calls}");
    assert!(hosts[1].contains("sandbox"), "{calls}");
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let fut = acp.prompt("hello".into(), &tx);
    tokio::pin!(fut);
    loop {
        tokio::select! {
            r = &mut fut => { r.unwrap(); break }
            Some(_) = rx.recv() => {}
        }
    }
}

#[tokio::test]
async fn task_agent_idle_ttl_parks_a_named_subagent_and_revives_it_on_the_next_message() {
    let mock = Mock::routed(
        vec![
            vec![call(
                "t1",
                "task",
                json!({"tasks":[{"name":"keeper","prompt":"first-marker"}]}),
            )],
            vec![text("first done")],
            vec![call(
                "t2",
                "task",
                json!({"tasks":[{"name":"keeper","prompt":"second-marker"}]}),
            )],
            vec![text("second done")],
        ],
        vec![("marker", vec![vec![text("note-one")], vec![text("note-two")]])],
    );
    let (mut a, _) = agent("idle-ttl", Mode::Full, &mock);
    let (r, _) = run(&mut a, "delegate", Decision::Deny).await;
    r.unwrap();
    let (r, events) = run(&mut a, "delegate again", Decision::Deny).await;
    r.unwrap();
    assert!(results(&events)[0].0.contains("note-two"));
    // The second run of the same name starts from the parked transcript instead of an empty one.
    let seen = mock.seen.lock().unwrap();
    let second = seen
        .iter()
        .filter(|(_, m)| m.contains("# Role: keeper") && m.contains("second-marker"))
        .map(|(_, m)| m.clone())
        .next()
        .expect("second sub-agent request");
    assert!(second.contains("first-marker") && second.contains("note-one"), "{second}");

    // A zero TTL parks nothing on disk but keeps the transcript in memory, so the same name still continues.
    let mock = Mock::routed(
        vec![
            vec![call("t1", "task", json!({"tasks":[{"name":"keeper","prompt":"first-marker"}]}))],
            vec![text("first done")],
            vec![call("t2", "task", json!({"tasks":[{"name":"keeper","prompt":"second-marker"}]}))],
            vec![text("second done")],
        ],
        vec![("marker", vec![vec![text("note-one")], vec![text("note-two")]])],
    );
    let meta_dir = dir("idle-ttl-off");
    let meta = Meta::new(meta_dir, "mock/m".into(), "native".into());
    let mut cfg = Config::default();
    cfg.learning.auto = false;
    cfg.task.agent_idle_ttl_ms = 0;
    let owner = meta.id.clone();
    let mut a = Agent::with_provider(Arc::new(cfg), &meta, Arc::new(Perms::new(Mode::Full)), mock.clone(), "m".into());
    run(&mut a, "delegate", Decision::Deny).await.0.unwrap();
    run(&mut a, "delegate again", Decision::Deny).await.0.unwrap();
    let seen = mock.seen.lock().unwrap();
    let second = seen
        .iter()
        .find(|(_, m)| m.contains("# Role: keeper") && m.contains("second-marker"))
        .map(|(_, m)| m.clone())
        .expect("second sub-agent request");
    assert!(second.contains("first-marker") && second.contains("note-one"), "{second}");
    assert!(
        !std::fs::read_dir(greencode_core::home().join("parked").join(&owner))
            .map(|d| d.count())
            .unwrap_or(0)
            .gt(&0),
        "a zero TTL must not park anything on disk"
    );
}
