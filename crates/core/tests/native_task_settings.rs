//! `[task]` settings: dispatch limits, rejected options, ad shaping and deadlock freedom.
use greencode_core::{
    agent::Agent,
    config::Config,
    permission::{Mode, Perms},
    provider::{Provider, Reply, Request},
    session::Meta,
    subagents::{PerCallModel, TaskConfig},
    Backend, Block, BoxFut, Decision, Event,
};
use serde_json::{json, Value};
use std::{
    path::PathBuf,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, LazyLock, Mutex,
    },
    time::Duration,
};

fn dir(name: &str) -> PathBuf {
    static HOME: LazyLock<PathBuf> = LazyLock::new(|| {
        let h = std::env::temp_dir().join(format!("greencode-task-settings-{}", std::process::id()));
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
struct Seen {
    model: String,
    tools: Vec<String>,
    sub: bool,
}

/// `script` sees the serialized messages and returns the reply, or `None` to hang.
struct Mock {
    script: Box<dyn Fn(&str) -> Option<Vec<Block>> + Send + Sync>,
    seen: Mutex<Vec<Seen>>,
    live: AtomicUsize,
    peak: AtomicUsize,
}

impl Provider for Mock {
    fn complete<'a>(
        &'a self,
        req: Request<'a>,
        _: &'a (dyn Fn(&str) + Send + Sync),
    ) -> BoxFut<'a, anyhow::Result<Reply>> {
        Box::pin(async move {
            let messages = serde_json::to_string(req.messages).unwrap();
            let sub = messages.contains("# Role: ");
            self.seen.lock().unwrap().push(Seen {
                model: req.model.into(),
                tools: req.tools.iter().map(|t| t.name.clone()).collect(),
                sub,
            });
            if sub {
                let now = self.live.fetch_add(1, Ordering::SeqCst) + 1;
                self.peak.fetch_max(now, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(20)).await;
                self.live.fetch_sub(1, Ordering::SeqCst);
            }
            match (self.script)(&messages) {
                Some(blocks) => Ok(Reply { blocks, ..Default::default() }),
                None => std::future::pending().await,
            }
        })
    }
}

fn mock(script: impl Fn(&str) -> Option<Vec<Block>> + Send + Sync + 'static) -> Arc<Mock> {
    Arc::new(Mock { script: Box::new(script), seen: Default::default(), live: Default::default(), peak: Default::default() })
}

fn text(s: &str) -> Option<Vec<Block>> {
    Some(vec![Block::Text { text: s.into() }])
}

fn task_call(input: Value) -> Option<Vec<Block>> {
    Some(vec![Block::ToolUse { id: "t".into(), name: "task".into(), input }])
}

fn role(messages: &str) -> Option<&str> {
    let rest = &messages[messages.find("# Role: ")? + 8..];
    Some(&rest[..rest.find('\\').unwrap_or(rest.len())])
}

fn agent(name: &str, task: TaskConfig, mock: &Arc<Mock>) -> Agent {
    let meta = Meta::new(dir(name), "mock/m".into(), "native".into());
    let mut cfg = Config::default();
    cfg.learning.auto = false;
    cfg.task = task;
    Agent::with_provider(Arc::new(cfg), &meta, Arc::new(Perms::new(Mode::Full)), mock.clone(), "m".into())
}

/// Runs a prompt and returns the `(output, is_error)` of every tool call.
async fn run(a: &mut Agent) -> Vec<(String, bool)> {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let mut results = Vec::new();
    let fut = a.prompt("go".into(), &tx);
    tokio::pin!(fut);
    let r = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            tokio::select! {
                r = &mut fut => break r,
                Some(ev) = rx.recv() => match ev {
                    Event::Permission { reply, .. } => { let _ = reply.send(Decision::Deny); }
                    Event::ToolEnd { output, is_error, .. } => results.push((output, is_error)),
                    _ => {}
                },
            }
        }
    })
    .await
    .expect("the turn hung");
    r.unwrap();
    while let Ok(ev) = rx.try_recv() {
        if let Event::ToolEnd { output, is_error, .. } = ev {
            results.push((output, is_error));
        }
    }
    results
}

fn first_task_then_done(input: Value) -> impl Fn(&str) -> Option<Vec<Block>> + Send + Sync {
    move |m| if m.contains("tool_result") { text("done") } else { task_call(input.clone()) }
}

#[tokio::test]
async fn one_slot_runs_nested_and_parallel_parents_without_deadlock() {
    let m = mock(|messages| match role(messages) {
        None if messages.contains("tool_result") => text("done"),
        None => task_call(json!({"tasks":[{"name":"P1","prompt":"p"},{"name":"P2","prompt":"p"}]})),
        Some(r) if r.starts_with('P') && !messages.contains("leaf done") => {
            task_call(json!({"tasks":[{"name":format!("L-{r}"),"prompt":"l"}]}))
        }
        Some(r) if r.starts_with('P') => text("P done"),
        Some(_) => text("leaf done"),
    });
    let mut a = agent("one-slot", TaskConfig { max_concurrency: 1, ..Default::default() }, &m);
    let results = run(&mut a).await;
    assert!(!results[0].1 && results[0].0.matches("P done").count() == 2, "{results:?}");
    assert_eq!(m.peak.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn disabled_everything_or_zero_depth_hides_and_blocks_task() {
    let all = greencode_core::subagents::bundled().into_iter().map(|def| def.name).collect();
    for (i, cfg) in [
        TaskConfig { disabled_agents: all, ..Default::default() },
        TaskConfig { max_recursion_depth: 0, ..Default::default() },
    ]
    .into_iter()
    .enumerate()
    {
        let m = mock(first_task_then_done(json!({"tasks":[{"prompt":"x"}]})));
        let mut a = agent(&format!("no-task-{i}"), cfg, &m);
        let results = run(&mut a).await;
        assert!(results[0].1 && results[0].0.contains("not available"), "{results:?}");
        let seen = m.seen.lock().unwrap();
        assert!(seen.iter().all(|s| !s.sub && !s.tools.iter().any(|t| t == "task")));
    }
}

#[tokio::test]
async fn forbidden_options_fail_before_any_subagent_runs() {
    let cases = [
        (TaskConfig { per_call_model: PerCallModel::Off, ..Default::default() }, json!({"tasks":[{"prompt":"x","model":"mock/x"}]}), "per_call_model"),
        (TaskConfig { per_call_model: PerCallModel::Eval, ..Default::default() }, json!({"tasks":[{"prompt":"x","model":"mock/x"}]}), "per_call_model"),
        (TaskConfig { per_call_model_allow: vec!["mock/ok".into()], ..Default::default() }, json!({"tasks":[{"prompt":"x","model":"mock/other"}]}), "per_call_model_allow"),
        (TaskConfig { isolation: greencode_core::subagents::TaskIsolation { enabled: false, ..Default::default() }, ..Default::default() }, json!({"tasks":[{"prompt":"x","isolated":true}]}), "task.isolation.enabled"),
        (TaskConfig { structured: false, ..Default::default() }, json!({"tasks":[{"prompt":"x","schema":{"type":"object"}}]}), "task.structured"),
        (TaskConfig { background: false, ..Default::default() }, json!({"background":true,"tasks":[{"prompt":"x"}]}), "task.background"),
        (TaskConfig { disabled_agents: vec!["scout".into()], ..Default::default() }, json!({"tasks":[{"prompt":"x","agent":"scout"}]}), "task.disabled_agents"),
    ];
    for (i, (cfg, input, expect)) in cases.into_iter().enumerate() {
        let m = mock(first_task_then_done(input));
        let mut a = agent(&format!("forbidden-{i}"), cfg, &m);
        let results = run(&mut a).await;
        assert!(results[0].1 && results[0].0.contains(expect), "{i}: {results:?}");
        assert!(m.seen.lock().unwrap().iter().all(|s| !s.sub), "{i}: a sub-agent ran");
    }
}

#[tokio::test]
async fn max_runtime_stops_a_hung_subagent() {
    let m = mock(|messages| match role(messages) {
        Some(_) => None,
        None if messages.contains("tool_result") => text("done"),
        None => task_call(json!({"tasks":[{"prompt":"x"}]})),
    });
    let mut a = agent("runtime", TaskConfig { max_runtime_ms: 50, ..Default::default() }, &m);
    let results = run(&mut a).await;
    assert!(results[0].1 && results[0].0.contains("timed out after 50 ms"), "{results:?}");
}

#[tokio::test]
async fn model_precedence_is_per_call_then_override_then_definition() {
    let mut cfg = TaskConfig::default();
    cfg.agent_model_overrides.insert("scout".into(), "mock/special".into());
    for (item, expect) in [
        (json!({"prompt":"x","agent":"scout"}), "special"),
        (json!({"prompt":"x","agent":"scout","model":"mock/call"}), "call"),
        (json!({"prompt":"x","agent":"reviewer"}), "m"),
    ] {
        let m = mock(move |messages| match role(messages) {
            Some(_) => text("sub"),
            None if messages.contains("tool_result") => text("done"),
            None => task_call(json!({"tasks":[item.clone()]})),
        });
        let mut a = agent(&format!("model-{expect}"), cfg.clone(), &m);
        run(&mut a).await;
        let seen = m.seen.lock().unwrap();
        assert_eq!(seen.iter().find(|s| s.sub).unwrap().model, expect);
    }
}

#[tokio::test]
async fn recursion_depth_one_gives_subagents_no_task_tool() {
    let m = mock(|messages| match role(messages) {
        Some(_) => text("sub"),
        None if messages.contains("tool_result") => text("done"),
        None => task_call(json!({"tasks":[{"prompt":"x"}]})),
    });
    let mut a = agent("depth-one", TaskConfig { max_recursion_depth: 1, ..Default::default() }, &m);
    run(&mut a).await;
    let seen = m.seen.lock().unwrap();
    assert!(seen.iter().filter(|s| !s.sub).all(|s| s.tools.iter().any(|t| t == "task")));
    assert!(seen.iter().filter(|s| s.sub).all(|s| !s.tools.iter().any(|t| t == "task")));
}
