use greencode_core::{
    config::{AcpCfg, Config, Protocol},
    permission::{Mode, Perms},
    pi::Pi,
    session::{Meta, Session},
    Backend, Command, Event,
};
use serde_json::json;
use std::{
    path::PathBuf,
    sync::{Arc, OnceLock},
    time::Duration,
};
use tokio::sync::mpsc;

fn meta(name: &str) -> Meta {
    static HOME: OnceLock<PathBuf> = OnceLock::new();
    let home = HOME.get_or_init(|| {
        let path = std::env::temp_dir().join(format!("greencode-pi-test-{}", std::process::id()));
        std::fs::create_dir_all(&path).unwrap();
        std::env::set_var("GREENCODE_HOME", &path);
        path
    });
    let cwd = home.join(name);
    std::fs::create_dir_all(&cwd).unwrap();
    Meta::new(cwd, "default".into(), "fixture".into())
}

fn command() -> Vec<String> {
    vec![
        "python3".into(),
        format!("{}/tests/fixtures/pi_rpc.py", env!("CARGO_MANIFEST_DIR")),
    ]
}

async fn try_run(pi: &mut Pi, text: &str) -> anyhow::Result<Vec<Event>> {
    let (tx, mut rx) = mpsc::unbounded_channel();
    tokio::time::timeout(Duration::from_secs(5), async {
        let fut = pi.prompt(text.into(), &tx);
        tokio::pin!(fut);
        let mut events = Vec::new();
        let mut unanswered = Vec::new();
        let result;
        loop {
            tokio::select! {
                r = &mut fut => { result = r; break; }
                Some(e) = rx.recv() => match e {
                    Event::Dialog { request, reply } if request["id"] == "slow" => unanswered.push(reply),
                    Event::Dialog { request, reply } => {
                        assert_eq!(request["id"], "question");
                        let answer = if request["method"] == "ask" {
                            json!({"answers":[{"id":"a","selectedOptions":["one","two"]},{"id":"b","selectedOptions":[],"customInput":"details"}]})
                        } else { json!({"value":"operator answer"}) };
                        reply.send(answer).unwrap();
                    }
                    e => events.push(e),
                }
            }
        }
        while let Ok(e) = rx.try_recv() {
            events.push(e);
        }
        result.map(|_| events)
    })
    .await
    .unwrap()
}

async fn run(pi: &mut Pi, text: &str) -> Vec<Event> {
    try_run(pi, text).await.unwrap()
}

fn wire(meta: &Meta) -> Vec<serde_json::Value> {
    std::fs::read_to_string(greencode_core::home().join("provider-sessions").join(&meta.id).join("wire.jsonl"))
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

fn notices(events: &[Event]) -> Vec<&str> {
    events.iter().filter_map(|e| if let Event::Notify(t) = e { Some(t.as_str()) } else { None }).collect()
}

fn advertised(events: &[Event], command: &str) -> Vec<String> {
    events
        .iter()
        .rev()
        .find_map(|e| match e {
            Event::Remote { command: c, data } if c == command => Some(
                data["commands"].as_array().unwrap().iter().map(|c| c["name"].as_str().unwrap().trim_start_matches('/').to_string()).collect(),
            ),
            _ => None,
        })
        .unwrap()
}

#[tokio::test]
async fn rpc_resumes_native_history_and_roundtrips_dialogs_without_replay() {
    let meta = meta("resume");
    Session::create(&meta).unwrap();
    let perms = Arc::new(Perms::new(Mode::Ask));
    let mut pi = Pi::start(&command(), Protocol::Pi, &meta, perms.clone())
        .await
        .unwrap();
    let first = run(&mut pi, "first").await;
    assert!(first
        .iter()
        .any(|e| matches!(e, Event::Text(t) if t == "1:first\u{2028}日本語")));
    let (tx, _) = mpsc::unbounded_channel();
    pi.handoff(&tx).await.unwrap();
    drop(pi);
    let mut pi = Pi::start(&command(), Protocol::Pi, &meta, perms.clone())
        .await
        .unwrap();
    let next = run(&mut pi, "second").await;
    assert!(next
        .iter()
        .any(|e| matches!(e, Event::Text(t) if t.starts_with("2:second"))));
    let dialog = run(&mut pi, "dialog").await;
    assert!(dialog
        .iter()
        .any(|e| matches!(e, Event::Text(t) if t == "operator answer")));
    let (_, messages) = Session::load(&meta.id).unwrap();
    assert_eq!(messages.len(), 6);
    let wire = std::fs::read_to_string(
        greencode_core::home()
            .join("provider-sessions")
            .join(&meta.id)
            .join("wire.jsonl"),
    )
    .unwrap();
    let prompts: Vec<_> = wire
        .lines()
        .map(|l| serde_json::from_str::<serde_json::Value>(l).unwrap())
        .filter(|v| v["type"] == "prompt")
        .collect();
    assert_eq!(prompts.len(), 3);
    assert_eq!(prompts[1]["message"], "second");
    assert!(prompts.iter().all(|p| p.get("messages").is_none()
        && p.get("system").is_none()
        && p.get("tools").is_none()));

    drop(pi);
    Session::clear_runtime(&meta.id).unwrap();
    let error = Pi::start(&command(), Protocol::Pi, &meta, perms)
        .await.err().expect("a thread with history cannot start fresh");
    assert!(error.to_string().contains("no native session handle"));
}

#[tokio::test]
async fn slash_command_is_forwarded_and_settles_on_handled_response() {
    let meta = meta("extension-command");
    let mut pi = Pi::start(&command(), Protocol::Pi, &meta, Arc::new(Perms::new(Mode::Ask)))
        .await
        .unwrap();
    let events = run(&mut pi, "/hello world").await;
    assert!(!events.iter().any(|e| matches!(e, Event::Text(_))));
    let wire = std::fs::read_to_string(
        greencode_core::home()
            .join("provider-sessions")
            .join(&meta.id)
            .join("wire.jsonl"),
    )
    .unwrap();
    assert!(wire.lines().any(|l| l.contains(r#""message": "/hello world""#)));
}

#[tokio::test]
async fn advertised_commands_beat_builtin_shortcuts() {
    let meta = meta("collision");
    let mut pi = Pi::start(&command(), Protocol::Pi, &meta, Arc::new(Perms::new(Mode::Ask)))
        .await
        .unwrap();
    run(&mut pi, "/stats").await; // advertised: forwarded as a prompt
    run(&mut pi, "/compact").await; // unadvertised: native shortcut
    let wire = std::fs::read_to_string(
        greencode_core::home()
            .join("provider-sessions")
            .join(&meta.id)
            .join("wire.jsonl"),
    )
    .unwrap();
    let sent: Vec<serde_json::Value> = wire.lines().map(|l| serde_json::from_str(l).unwrap()).collect();
    assert!(sent.iter().any(|v| v["type"] == "prompt" && v["message"] == "/stats"));
    assert!(!sent.iter().any(|v| v["type"] == "get_session_stats"));
    assert!(sent.iter().any(|v| v["type"] == "compact"));
    assert!(!sent.iter().any(|v| v["type"] == "prompt" && v["message"] == "/compact"));
}

#[tokio::test]
async fn rpc_steers_waits_past_agent_end_and_cancel_does_not_poison_next_turn() {
    let meta = meta("live");
    let mut pi = Pi::start(
        &command(),
        Protocol::Pi,
        &meta,
        Arc::new(Perms::new(Mode::Ask)),
    )
    .await
    .unwrap();
    let live = pi.live().unwrap();
    let (tx, mut rx) = mpsc::unbounded_channel();
    let result = tokio::time::timeout(Duration::from_secs(5), async {
        let fut = pi.prompt("steer".into(), &tx);
        tokio::pin!(fut);
        let mut text = String::new();
        loop {
            tokio::select! {
                r = &mut fut => { r.unwrap(); break; }
                Some(e) = rx.recv() => {
                    if let Event::Text(t) = e {
                        if t == "working" { live.send(json!({"type":"steer","message":"new direction"})).unwrap(); }
                        text.push_str(&t);
                    }
                }
            }
        }
        while let Ok(e) = rx.try_recv() { if let Event::Text(t) = e { text.push_str(&t); } }
        text
    }).await.unwrap();
    assert!(result.contains("new direction"));
    {
        let cancel = pi.prompt("cancel".into(), &tx);
        tokio::pin!(cancel);
        loop {
            tokio::select! {
                r = &mut cancel => panic!("turn ended before cancel: {r:?}"),
                Some(e) = rx.recv() => if matches!(e, Event::Text(t) if t == "working") { break; },
            }
        }
    }
    pi.cancel(&tx).await.unwrap();
    let next = run(&mut pi, "after cancel").await;
    assert!(next
        .iter()
        .any(|e| matches!(e, Event::Text(t) if t.contains("after cancel"))));
}

#[tokio::test]
async fn runner_does_not_discard_prompts_received_during_a_turn() {
    let meta = meta("queue");
    let mut cfg = Config::default();
    cfg.backends.push(AcpCfg {
        name: "fixture".into(),
        command: command(),
        env: Default::default(),
        protocol: Protocol::Pi,
    });
    let (tx, mut rx) = greencode_core::spawn(
        &tokio::runtime::Handle::current(),
        Arc::new(cfg),
        meta,
        Arc::new(Perms::new(Mode::Ask)),
    );
    tx.send(Command::Prompt("one".into())).unwrap();
    tx.send(Command::Prompt("two".into())).unwrap();
    let text = tokio::time::timeout(Duration::from_secs(5), async {
        let (mut done, mut text) = (0, String::new());
        while done < 2 {
            match rx.recv().await.unwrap() {
                Event::Text(t) => text.push_str(&t),
                Event::Done { .. } => done += 1,
                Event::Error(e) => panic!("{e}"),
                _ => {}
            }
        }
        text
    })
    .await
    .unwrap();
    assert!(text.contains("1:one") && text.contains("2:two"));
}

#[tokio::test]
async fn idle_command_updates_are_published_before_the_next_prompt() {
    let meta = meta("idle-command-update");
    let mut cfg = Config::default();
    cfg.backends.push(AcpCfg {
        name: "fixture".into(), command: command(), env: Default::default(), protocol: Protocol::Pi,
    });
    let (tx, mut rx) = greencode_core::spawn(
        &tokio::runtime::Handle::current(), Arc::new(cfg), meta.clone(), Arc::new(Perms::new(Mode::Ask)),
    );
    tx.send(Command::Remote(json!({"type":"schedule_update"}))).unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        let mut done = false;
        loop {
            match rx.recv().await.unwrap() {
                Event::Done { .. } => done = true,
                Event::Remote { command, data } if command == "available_commands_update" => {
                    assert!(done);
                    assert!(data["commands"].as_array().unwrap().iter().any(|v| v["name"] == "settings"));
                    break;
                }
                Event::Error(e) => panic!("{e}"),
                _ => {}
            }
        }
        tx.send(Command::Prompt("/settings".into())).unwrap();
        loop {
            match rx.recv().await.unwrap() {
                Event::Done { .. } => break,
                Event::Error(e) => panic!("{e}"),
                _ => {}
            }
        }
    }).await.unwrap();
    assert!(wire(&meta).iter().any(|v| v["type"] == "prompt" && v["message"] == "/settings"));
}

#[tokio::test]
async fn cached_thinking_and_preset_precede_first_prompt() {
    let mut meta = meta("thinking-preset");
    meta.backend = "thinking-fixture".into();
    let mut cfg = Config::default();
    cfg.backends.push(AcpCfg {
        name: meta.backend.clone(), command: command(), env: Default::default(), protocol: Protocol::Pi,
    });
    let cfg = Arc::new(cfg);
    let models = greencode_core::backend_models(cfg.clone(), &meta.backend, meta.cwd.clone()).await;
    assert_eq!(models, ["test/model"]);
    let cached = greencode_core::cached_backend_options();
    let option = &cached[&meta.backend][0];
    assert_eq!(option["options"], json!([{"value":"off"},{"value":"low"},{"value":"high"}]));
    assert!(option.get("currentValue").is_none());

    let (tx, mut rx) = greencode_core::spawn(
        &tokio::runtime::Handle::current(), cfg, meta.clone(), Arc::new(Perms::new(Mode::Ask)),
    );
    tx.send(Command::PromptWithOptions("hello".into(), vec![json!({"type":"set_thinking_level","level":"high"})])).unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match rx.recv().await.unwrap() {
                Event::Error(e) => panic!("{e}"),
                Event::Done { .. } => break,
                _ => {}
            }
        }
    }).await.unwrap();
    let wire = std::fs::read_to_string(greencode_core::home().join("provider-sessions").join(&meta.id).join("wire.jsonl")).unwrap();
    let requests: Vec<serde_json::Value> = wire.lines().map(|line| serde_json::from_str(line).unwrap()).collect();
    let set = requests.iter().position(|v| v["type"] == "set_thinking_level").unwrap();
    let prompt = requests.iter().position(|v| v["type"] == "prompt").unwrap();
    assert!(set < prompt);
    assert_eq!(requests[set]["level"], "high");
}

#[tokio::test]
async fn tui_builtins_are_advertised_and_map_to_rpc_commands() {
    let meta = meta("pi-builtins");
    let mut pi = Pi::start(&command(), Protocol::Pi, &meta, Arc::new(Perms::new(Mode::Ask))).await.unwrap();
    let first = run(&mut pi, "/copy").await;
    let names = advertised(&first, "get_commands");
    // Agent-advertised commands stay, built-ins Pi only has in its TUI are added once.
    for name in ["hello", "stats", "new", "tree", "fork", "name", "clone", "resume", "session"] {
        assert_eq!(names.iter().filter(|n| *n == name).count(), 1, "{name} in {names:?}");
    }
    assert!(first.iter().any(|event| matches!(event, Event::Remote { command, data } if command == "copy_text" && data["text"] == "last answer")));

    assert!(notices(&run(&mut pi, "/tree").await).iter().any(|n| n.contains("* e2 [assistant] yo") && n.contains("  ") && n.contains("e1 [user] hi")));
    assert!(notices(&run(&mut pi, "/fork").await).iter().any(|n| n.contains("e1  hi there")));
    assert!(notices(&run(&mut pi, "/thinking").await).iter().any(|n| n.contains("low") && n.contains("high")));
    assert!(notices(&run(&mut pi, "/name").await).iter().any(|n| n.contains("named")));
    assert!(notices(&run(&mut pi, "/model").await).iter().any(|n| n.contains("test/model")));
    run(&mut pi, "/new").await;
    run(&mut pi, "/fork e1").await;
    run(&mut pi, "/name renamed").await;
    run(&mut pi, "/resume /tmp/other.jsonl").await;
    let sent = wire(&meta);
    let has = |f: &dyn Fn(&serde_json::Value) -> bool| sent.iter().any(|v| f(v));
    assert!(has(&|v| v["type"] == "new_session"));
    assert!(has(&|v| v["type"] == "fork" && v["entryId"] == "e1"));
    assert!(has(&|v| v["type"] == "set_session_name" && v["name"] == "renamed"));
    assert!(has(&|v| v["type"] == "switch_session" && v["sessionPath"] == "/tmp/other.jsonl"));
    assert!(!has(&|v| v["type"] == "prompt"), "no built-in may reach the model");
}

#[tokio::test]
async fn interactive_only_commands_fail_instead_of_reaching_the_model() {
    let meta = meta("pi-ui-only");
    let mut pi = Pi::start(&command(), Protocol::Pi, &meta, Arc::new(Perms::new(Mode::Ask))).await.unwrap();
    for name in ["/settings", "/login"] {
        let err = try_run(&mut pi, name).await.unwrap_err().to_string();
        assert!(err.contains("terminal UI"), "{name}: {err}");
    }
    assert!(!wire(&meta).iter().any(|v| v["type"] == "prompt" || v["type"] == "export_html"));
}

#[tokio::test]
async fn pi_reload_uses_extension_and_refreshes_commands() {
    let meta = meta("pi-reload");
    let mut pi = Pi::start(&command(), Protocol::Pi, &meta, Arc::new(Perms::new(Mode::Ask))).await.unwrap();
    run(&mut pi, "/reload").await;
    let sent = wire(&meta);
    let reload = sent.iter().position(|v| v["type"] == "prompt" && v["message"] == "/greencode-reload").unwrap();
    assert!(sent[reload + 1..].iter().any(|v| v["type"] == "get_commands"));
    assert!(sent[reload + 1..].iter().any(|v| v["type"] == "get_available_models"));
}
#[tokio::test]
async fn pi_changelog_shows_agent_output_without_a_model_turn() {
    let meta = meta("pi-changelog");
    let mut pi = Pi::start(&command(), Protocol::Pi, &meta, Arc::new(Perms::new(Mode::Ask))).await.unwrap();
    let events = run(&mut pi, "/changelog").await;
    assert!(events.iter().any(|e| matches!(e, Event::Remote { command, data } if command == "notify" && data["message"] == "Pi changelog fixture")));
    assert!(wire(&meta).iter().any(|v| v["type"] == "prompt" && v["message"] == "/greencode-changelog"));
}

#[tokio::test]
async fn pi_hotkeys_shows_agent_output_without_a_model_turn() {
    let meta = meta("pi-hotkeys");
    let mut pi = Pi::start(&command(), Protocol::Pi, &meta, Arc::new(Perms::new(Mode::Ask))).await.unwrap();
    let events = run(&mut pi, "/hotkeys").await;
    assert!(events.iter().any(|e| matches!(e, Event::Remote { command, data } if command == "notify" && data["message"].as_str().unwrap_or("").contains("app.interrupt: escape"))));
    assert!(wire(&meta).iter().any(|v| v["type"] == "prompt" && v["message"] == "/greencode-hotkeys"));
}

#[tokio::test]
async fn pi_trust_routes_the_selection_dialog_without_a_model_turn() {
    let meta = meta("pi-trust");
    let mut pi = Pi::start(&command(), Protocol::Pi, &meta, Arc::new(Perms::new(Mode::Ask))).await.unwrap();
    run(&mut pi, "/trust").await;
    let sent = wire(&meta);
    assert!(sent.iter().any(|v| v["type"] == "prompt" && v["message"] == "/greencode-trust"));
    assert!(sent.iter().any(|v| v["type"] == "extension_ui_response" && v["id"] == "question" && v["value"] == "operator answer"));
}


#[tokio::test]
async fn pi_jsonl_export_is_routed_to_session_extension() {
    let meta = meta("pi-export-jsonl");
    let mut pi = Pi::start(&command(), Protocol::Pi, &meta, Arc::new(Perms::new(Mode::Ask))).await.unwrap();
    run(&mut pi, "/export file with spaces.jsonl").await;
    assert!(wire(&meta).iter().any(|v| v["type"] == "prompt" && v["message"] == r#"/greencode-export-jsonl "file with spaces.jsonl""#));
}

#[tokio::test]
async fn omp_commands_use_aliases_live_updates_and_rpc_login() {
    let meta = meta("omp-commands");
    Session::create(&meta).unwrap();
    let mut pi = Pi::start(&command(), Protocol::Omp, &meta, Arc::new(Perms::new(Mode::Ask))).await.unwrap();
    // `info` is an alias of the advertised `session`: it must reach OMP, not a GreenCode shortcut.
    let first = run(&mut pi, "/info").await;
    let names = advertised(&first, "get_available_commands");
    assert!(names.contains(&"session".to_string()) && names.contains(&"info".to_string()) && names.contains(&"new".to_string()) && names.contains(&"login".to_string()));
    assert_eq!(names.iter().filter(|n| *n == "export").count(), 1);
    // `available_commands_update` frames refresh both the UI list and what counts as advertised.
    assert!(advertised(&first, "available_commands_update").contains(&"late".to_string()));
    run(&mut pi, "/late now").await;

    let login = run(&mut pi, "/login acme").await;
    assert!(notices(&login).iter().any(|n| n.contains("about:blank") && n.contains("paste the code")));
    assert!(notices(&login).iter().any(|n| n.contains("acme")));
    run(&mut pi, "/new").await;
    let err = try_run(&mut pi, "/plan").await.unwrap_err().to_string();
    assert!(err.contains("Oh My Pi"));
    let sent = wire(&meta);
    let prompts: Vec<_> = sent.iter().filter(|v| v["type"] == "prompt").map(|v| v["message"].as_str().unwrap()).collect();
    assert_eq!(prompts, ["/info", "/late now"]);
    assert!(sent.iter().any(|v| v["type"] == "login" && v["providerId"] == "acme"));
    assert!(sent.iter().any(|v| v["type"] == "extension_ui_response" && v["value"] == "operator answer"));
    assert!(sent.iter().any(|v| v["type"] == "new_session"));
}

#[tokio::test]
async fn omp_enables_and_answers_ask_dialogs() {
    let meta = meta("omp-ask");
    let mut pi = Pi::start(&command(), Protocol::Omp, &meta, Arc::new(Perms::new(Mode::Ask))).await.unwrap();
    let events = run(&mut pi, "ask").await;
    assert!(events.iter().any(|e| matches!(e, Event::Text(t) if t == "answered")));
    let sent = wire(&meta);
    assert!(sent.iter().any(|v| v["type"] == "set_ask_dialog" && v["enabled"] == true));
    assert!(sent.iter().any(|v| v["type"] == "extension_ui_response" && v["answers"][1]["customInput"] == "details"));
}

#[tokio::test]
async fn extension_failures_and_self_started_turns_do_not_break_commands() {
    let meta = meta("pi-extension-turn");
    let mut pi = Pi::start(&command(), Protocol::Pi, &meta, Arc::new(Perms::new(Mode::Ask))).await.unwrap();
    let events = run(&mut pi, "/boom").await;
    assert!(events.iter().any(|e| matches!(e, Event::Error(t) if t.contains("/ext/boom.ts") && t.contains("exploded"))));
    // The command answered `handled` but started a turn: its output must belong to this prompt.
    let events = run(&mut pi, "/turn").await;
    assert!(events.iter().any(|e| matches!(e, Event::Text(t) if t == "turn done")));
}

#[tokio::test]
async fn dialog_timeouts_and_open_url_requests_are_answered_or_surfaced() {
    let meta = meta("pi-dialog-timeout");
    let mut pi = Pi::start(&command(), Protocol::Pi, &meta, Arc::new(Perms::new(Mode::Ask))).await.unwrap();
    // The operator never answers; the agent's 50 ms timeout must unblock the turn with `timedOut`.
    let events = run(&mut pi, "timeout").await;
    assert!(events.iter().any(|e| matches!(e, Event::Text(t) if t == "timedOut")));
    assert!(events.iter().any(|e| matches!(e, Event::Remote { command, data } if command == "dialog_closed" && data["id"] == "slow")));
    let events = run(&mut pi, "ask-timeout").await;
    assert!(events.iter().any(|e| matches!(e, Event::Text(t) if t == "timedOut")));
    let events = run(&mut pi, "open").await;
    assert!(notices(&events).iter().any(|n| n.contains("about:blank")));
    assert!(events.iter().any(|e| matches!(e, Event::Text(t) if t == "opened")));
}

#[tokio::test]
async fn agent_cancel_closes_dialog_and_preserves_interleaved_events() {
    let meta = meta("omp-cancel-dialog");
    let mut pi = Pi::start(&command(), Protocol::Omp, &meta, Arc::new(Perms::new(Mode::Ask))).await.unwrap();
    let events = run(&mut pi, "agent-cancels-dialog").await;
    assert!(events.iter().any(|e| matches!(e, Event::Remote { command, data } if command == "dialog_closed" && data["id"] == "slow")));
    assert!(events.iter().any(|e| matches!(e, Event::Remote { command, data } if command == "notify" && data["message"] == "before cancellation")));
    assert!(events.iter().any(|e| matches!(e, Event::Text(t) if t == "continued")));
    let notice = events.iter().position(|e| matches!(e, Event::Remote { command, .. } if command == "notify")).unwrap();
    let continued = events.iter().position(|e| matches!(e, Event::Text(t) if t == "continued")).unwrap();
    assert!(notice < continued);
    assert!(!wire(&meta).iter().any(|v| v["type"] == "extension_ui_response" && v["id"] == "slow"));
}

fn alive(meta: &Meta) -> bool {
    let pid = std::fs::read_to_string(greencode_core::home().join("provider-sessions").join(&meta.id).join("pid")).unwrap();
    std::process::Command::new("kill").args(["-0", pid.trim()]).stderr(std::process::Stdio::null()).status().unwrap().success()
}

fn fixture_thread(name: &str) -> (Meta, mpsc::UnboundedSender<Command>, mpsc::UnboundedReceiver<Event>) {
    let meta = meta(name);
    let mut cfg = Config::default();
    cfg.backends.push(AcpCfg { name: "fixture".into(), command: command(), env: Default::default(), protocol: Protocol::Pi });
    let (tx, rx) = greencode_core::spawn(&tokio::runtime::Handle::current(), Arc::new(cfg), meta.clone(), Arc::new(Perms::new(Mode::Ask)));
    (meta, tx, rx)
}

async fn hand_off(tx: &mpsc::UnboundedSender<Command>) -> Result<(), String> {
    let (reply, ack) = tokio::sync::oneshot::channel();
    tx.send(Command::Handoff(reply)).unwrap();
    tokio::time::timeout(Duration::from_secs(5), ack).await.unwrap().unwrap()
}

fn saved_runtime(meta: &Meta) -> serde_json::Value {
    Session::runtime_for_backend(&meta.id, &meta.backend).unwrap()
}

fn native_file(meta: &Meta) -> PathBuf {
    greencode_core::home().join("provider-sessions").join(&meta.id).join("native.json")
}

#[tokio::test]
async fn handoff_saves_the_exact_session_file_and_waits_for_the_agent_to_die() {
    let meta = meta("handoff");
    let mut pi = Pi::start(&command(), Protocol::Pi, &meta, Arc::new(Perms::new(Mode::Ask))).await.unwrap();
    run(&mut pi, "first").await;
    assert!(alive(&meta));
    Session::clear_runtime(&meta.id).unwrap(); // only the handoff itself may write it back
    let (tx, _rx) = mpsc::unbounded_channel();
    tokio::time::timeout(Duration::from_secs(5), pi.handoff(&tx)).await.unwrap().unwrap();
    assert!(!alive(&meta));
    assert_eq!(saved_runtime(&meta), json!({"backend": "fixture", "sessionFile": native_file(&meta)}));
    assert!(pi.prompt("late".into(), &tx).await.is_err());
}

#[tokio::test]
async fn handoff_refuses_a_working_or_sessionless_agent_and_leaves_it_running() {
    let meta = meta("handoff-refused");
    let mut pi = Pi::start(&command(), Protocol::Pi, &meta, Arc::new(Perms::new(Mode::Ask))).await.unwrap();
    let (tx, _rx) = mpsc::unbounded_channel();
    pi.control(json!({"type":"set_busy"}), &tx).await.unwrap();
    let err = pi.handoff(&tx).await.unwrap_err();
    assert!(format!("{err:#}").contains("still working"), "{err:#}");
    assert!(alive(&meta));
    pi.control(json!({"type":"abort"}), &tx).await.unwrap();
    pi.control(json!({"type":"drop_session"}), &tx).await.unwrap();
    let err = pi.handoff(&tx).await.unwrap_err();
    assert!(format!("{err:#}").contains("no session file"), "{err:#}");
    assert!(alive(&meta));
}

#[tokio::test]
async fn runner_handoff_starts_an_idle_thread_stops_it_and_then_refuses_work() {
    let (meta, tx, mut rx) = fixture_thread("runner-handoff");
    hand_off(&tx).await.unwrap();
    assert!(!alive(&meta));
    greencode_core::handoff::writer_lock(&meta.id).unwrap(); // the Pi that held the lock is gone before the acknowledgement
    assert_eq!(saved_runtime(&meta), json!({"backend": "fixture", "sessionFile": native_file(&meta)}));
    hand_off(&tx).await.unwrap(); // already handed off: still dead, still acknowledged
    tx.send(Command::Prompt("late".into())).unwrap();
    tx.send(Command::Model("test/model".into())).unwrap();
    let refused = tokio::time::timeout(Duration::from_secs(5), async {
        let mut errors = vec![];
        while errors.len() < 2 {
            if let Event::Error(e) = rx.recv().await.unwrap() {
                errors.push(e);
            }
        }
        errors
    })
    .await
    .unwrap();
    assert!(refused.iter().all(|e| e.contains("handed off")), "{refused:?}");
    assert!(!alive(&meta));
    assert!(!wire(&meta).iter().any(|v| v["type"] == "prompt"));
}

#[tokio::test]
async fn runner_handoff_is_refused_mid_turn_and_succeeds_after_cancel() {
    let (meta, tx, mut rx) = fixture_thread("runner-handoff-busy");
    tx.send(Command::Prompt("cancel".into())).unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match rx.recv().await.unwrap() {
                Event::Text(t) if t == "working" => break,
                Event::Error(e) => panic!("{e}"),
                _ => {}
            }
        }
    })
    .await
    .unwrap();
    let err = hand_off(&tx).await.unwrap_err();
    assert!(err.contains("still running"), "{err}");
    assert!(alive(&meta));
    tx.send(Command::Cancel).unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while !matches!(rx.recv().await.unwrap(), Event::Done { .. }) {}
    })
    .await
    .unwrap();
    hand_off(&tx).await.unwrap();
    assert!(!alive(&meta));
}

#[tokio::test]
async fn handoff_is_refused_for_backends_that_cannot_stop_cleanly() {
    let mut meta = meta("native-handoff");
    meta.backend = "native".into();
    let (tx, _rx) = greencode_core::spawn(&tokio::runtime::Handle::current(), Arc::new(Config::default()), meta, Arc::new(Perms::new(Mode::Ask)));
    let err = hand_off(&tx).await.unwrap_err();
    assert!(err.contains("does not support session handoff"), "{err}");
}
