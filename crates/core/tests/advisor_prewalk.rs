//! `[advisor]` and `[prewalk]`: reviews run on cadence with notes injected into the transcript, and
//! prewalk switches models at the first edit after the plan's todo list exists.
use greencode_core::{
    agent::Agent,
    config::Config,
    permission::{Mode, Perms},
    provider::{Provider, Reply, Request},
    session::Meta,
    Backend, Block, BoxFut, Decision, Event,
};
use serde_json::json;
use std::{
    path::PathBuf,
    sync::{Arc, LazyLock, Mutex},
    time::Duration,
};

fn dir(name: &str) -> PathBuf {
    static HOME: LazyLock<PathBuf> = LazyLock::new(|| {
        let h = std::env::temp_dir().join(format!("greencode-advisor-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&h);
        std::fs::create_dir_all(&h).unwrap();
        std::env::set_var("GREENCODE_HOME", &h);
        std::env::set_var("HOME", &h);
        h
    });
    let d = HOME.join("projects").join(name);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// Sees every request; the primary plans (todo), then writes, then answers; the advisor/judge by marker.
struct Mock {
    seen: Mutex<Vec<String>>,
}

impl Provider for Mock {
    fn complete<'a>(&'a self, req: Request<'a>, _: &'a (dyn Fn(&str) + Send + Sync)) -> BoxFut<'a, anyhow::Result<Reply>> {
        Box::pin(async move {
            let messages = serde_json::to_string(req.messages).unwrap();
            self.seen.lock().unwrap().push(format!("{}||{}||{messages}", req.model, req.system));
            let system = req.system.to_string();
            let blocks = if system.contains("passive advisor") {
                vec![Block::Text { text: r#"{"severity":"blocker","text":"the file is wrong"}"#.into() }]
            } else if system.contains("Classify the task") {
                vec![Block::Text { text: "yes".into() }]
            } else {
                match messages.matches("\"tool_result\"").count() {
                    0 => vec![Block::ToolUse { id: "t1".into(), name: "todo".into(), input: json!({"todos": [{"text":"a","done":false}]}) }],
                    1 => vec![Block::ToolUse { id: "t2".into(), name: "write".into(), input: json!({"path": "a.txt", "content": "x"}) }],
                    _ => vec![Block::Text { text: "done".into() }],
                }
            };
            Ok(Reply { blocks, ..Default::default() })
        })
    }
}

fn agent(name: &str, cfg: Config, mock: &Arc<Mock>, model: &str) -> Agent {
    let meta = Meta::new(dir(name), format!("mock/{model}"), "native".into());
    Agent::with_provider(Arc::new(cfg), &meta, Arc::new(Perms::new(Mode::Full)), mock.clone(), model.into())
}

async fn run(a: &mut Agent) -> Vec<Event> {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let fut = a.prompt("go".into(), &tx);
    tokio::pin!(fut);
    let mut events = Vec::new();
    let result = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            tokio::select! {
                r = &mut fut => break r,
                Some(ev) = rx.recv() => match ev {
                    Event::Permission { reply, .. } => { let _ = reply.send(Decision::Allow); }
                    other => events.push(other),
                },
            }
        }
    })
    .await
    .expect("the turn hung");
    result.unwrap();
    while let Ok(ev) = rx.try_recv() {
        events.push(ev);
    }
    events
}

fn base() -> Config {
    let mut cfg = Config::default();
    cfg.learning.auto = false;
    cfg.model_roles.insert("smol".into(), "mock/small".into());
    cfg.model_roles.insert("judge".into(), "mock/judge".into());
    cfg.model_roles.insert("advisor".into(), "mock/advisor".into());
    cfg
}

/// A blocker at the final answer wakes the agent; the note lands in the transcript.
#[tokio::test]
async fn advisor_notes_inject_after_the_final_answer() {
    let mock = Arc::new(Mock { seen: Default::default() });
    let mut cfg = base();
    cfg.assist_opts.advisor.enabled = true;
    // `agent-end` keeps the cadence deterministic: one review at the final yield, no mid-turn noise.
    cfg.assist_opts.advisor.review_mode = greencode_core::config::advisor::ReviewMode::AgentEnd;
    let mut a = agent("notes", cfg, &mock, "m");
    let events = run(&mut a).await;
    let wake = events.iter().any(|e| matches!(e, Event::Notify(t) if t.contains("woken")));
    assert!(wake, "a blocker must wake the finished agent");
    // The review sees the transcript and the advisory lands in it as a message.
    assert!(mock.seen.lock().unwrap().iter().any(|s| s.contains("passive advisor") && s.contains("go")));
    assert!(a.messages.iter().any(|m| m.content.iter().any(|b| matches!(b, Block::Text { text } if text.contains("the file is wrong")))));
}

/// The first successful edit switches the model to `smol`.
#[tokio::test]
async fn prewalk_switches_model_at_the_first_edit() {
    let mock = Arc::new(Mock { seen: Default::default() });
    let mut cfg = base();
    cfg.assist_opts.prewalk.enabled = true;
    cfg.assist_opts.advisor.enabled = false;
    let mut a = agent("prewalk", cfg, &mock, "big");
    let events = run(&mut a).await;
    let switched = events.iter().any(|e| matches!(e, Event::Remote { command, data } if command == "model" && data["model"] == "mock/small"));
    assert!(switched, "prewalk must announce the hand-off: {events:?}");
    assert!(a.messages.iter().any(|m| m.content.iter().any(|b| matches!(b, Block::Text { text } if text.contains("prewalk: switched")))));
    let seen = mock.seen.lock().unwrap();
    assert!(seen.iter().any(|s| s.starts_with("big||") && s.contains("go")));
    assert!(seen.iter().any(|s| s.starts_with("small||") && s.contains("prewalk: switched")));
}
