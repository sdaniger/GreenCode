//! Workflow engine acceptance: durable steps, answers, resume, retries, idempotency and agent steps.
use greencode_core::{
    agent::Agent,
    config::Config,
    permission::{Mode, Perms},
    provider::{Provider, Reply, Request},
    session::Meta,
    workflow::{self, AgentRunner, Paths, RunStatus, Runtime},
    Backend, Block, BoxFut,
};
use serde_json::{json, Value};
use std::{
    path::PathBuf,
    sync::{Arc, Mutex, OnceLock},
};

fn test_home() -> PathBuf {
    static HOME: OnceLock<PathBuf> = OnceLock::new();
    HOME.get_or_init(|| {
        let home = std::env::temp_dir().join(format!("greencode-workflow-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(home.join("extensions")).unwrap();
        std::env::set_var("GREENCODE_HOME", &home);
        home
    })
    .clone()
}

fn temp_paths(name: &str) -> Paths {
    let root = test_home()
        .join("cases")
        .join(format!("{name}-{}", workflow::now()))
        .join(std::process::id().to_string());
    std::fs::create_dir_all(root.join("workflows")).unwrap();
    Paths {
        workflows: root.join("workflows"),
        runs: root.join("runs"),
        cache: root.join("cache.json"),
    }
}

fn write_workflow(paths: &Paths, file: &str, text: &str) {
    std::fs::write(paths.workflows.join(file), text).unwrap();
}

fn runtime(paths: &Paths, agent: AgentRunner) -> Runtime {
    let mut cfg = Config::default();
    cfg.learning.auto = false;
    Runtime::with_paths(
        Arc::new(cfg),
        test_home(),
        paths.clone(),
        agent,
    )
}

fn unused_agent() -> AgentRunner {
    Arc::new(|_| Box::pin(async { anyhow::bail!("agent step must not run") }))
}

fn read_json(paths: &Paths, id: &str) -> Value {
    serde_json::from_slice(&std::fs::read(paths.runs.join(format!("{id}.json"))).unwrap()).unwrap()
}

fn write_json(paths: &Paths, id: &str, value: &Value) {
    std::fs::write(
        paths.runs.join(format!("{id}.json")),
        serde_json::to_vec_pretty(value).unwrap(),
    )
    .unwrap();
}

#[tokio::test]
async fn shell_and_ask_run_to_completion() {
    let paths = temp_paths("shell-ask");
    write_workflow(
        &paths,
        "deliver.toml",
        r#"
name = "deliver"
version = 1

[[steps]]
id = "note"
type = "shell"
command = "printf hello"

[[steps]]
id = "ship"
type = "ask"
question = "Ship {{steps.note.output}}?"
options = ["Ship", "Hold"]

[[steps]]
id = "confirm"
type = "shell"
command = "printf '{{steps.ship.output}}'"
"#,
    );
    let rt = runtime(&paths, unused_agent());
    let run = rt.start("deliver", json!({}), Some("k1".into()), "cli").unwrap();
    let run = rt.advance_run(&run.id).await.unwrap();
    assert_eq!(run.status, RunStatus::Waiting);
    assert_eq!(run.question.as_ref().unwrap().title, "Ship hello?");
    let run = rt.answer(&run.id, "ship", "Ship").unwrap();
    let run = rt.advance_run(&run.id).await.unwrap();
    assert_eq!(run.status, RunStatus::Done);
    assert_eq!(run.step("note").unwrap().output, "hello");
    assert_eq!(run.step("ship").unwrap().output, "Ship");
    assert_eq!(run.step("confirm").unwrap().output, "Ship");
}

#[tokio::test]
async fn run_resumes_from_the_saved_file_after_a_restart() {
    let paths = temp_paths("resume");
    write_workflow(
        &paths,
        "resume.toml",
        r#"
name = "resume"
version = 2

[[steps]]
id = "first"
type = "shell"
command = "printf one"

[[steps]]
id = "gate"
type = "ask"
question = "Continue?"
options = ["Yes"]

[[steps]]
id = "last"
type = "shell"
command = "printf '{{steps.first.output}}-{{steps.gate.output}}'"
"#,
    );
    let first = runtime(&paths, unused_agent());
    let run = first.start("resume", json!({}), Some("key".into()), "cli").unwrap();
    assert_eq!(first.advance_run(&run.id).await.unwrap().status, RunStatus::Waiting);
    drop(first);

    // A fresh process reads the same run file and only runs the remaining step.
    let second = runtime(&paths, unused_agent());
    let run = workflow::load_run(&paths, &run.id).unwrap();
    assert_eq!(run.step("first").unwrap().output, "one");
    let run = second.answer(&run.id, "gate", "Yes").unwrap();
    let run = second.advance_run(&run.id).await.unwrap();
    assert_eq!(run.status, RunStatus::Done);
    assert_eq!(run.step("last").unwrap().output, "one-Yes");
}

#[tokio::test]
async fn duplicate_start_key_returns_the_same_run_even_after_a_version_bump() {
    let paths = temp_paths("idempotent");
    write_workflow(
        &paths,
        "one.toml",
        r#"
name = "job"
version = 1

[[steps]]
id = "step"
type = "shell"
command = "printf v1"
"#,
    );
    let rt = runtime(&paths, unused_agent());
    let first = rt.start("job", json!({}), Some("same".into()), "cli").unwrap();
    write_workflow(
        &paths,
        "two.toml",
        r#"
name = "job"
version = 2

[[steps]]
id = "step"
type = "shell"
command = "printf v2"
"#,
    );
    let second = rt.start("job", json!({}), Some("same".into()), "cli").unwrap();
    assert_eq!(first.id, second.id);
    assert_eq!(second.version, 1);
}

#[tokio::test]
async fn shell_failures_retry_after_five_twenty_and_eighty_minutes() {
    let paths = temp_paths("retry");
    write_workflow(
        &paths,
        "retry.toml",
        r#"
name = "retry"
version = 1

[[steps]]
id = "fail"
type = "shell"
command = "echo nope >&2; exit 3"
"#,
    );
    let rt = runtime(&paths, unused_agent());
    let run = rt.start("retry", json!({}), None, "cli").unwrap();
    for (attempt, delay) in [(1, 300), (2, 1200), (3, 4800)] {
        let before = workflow::now();
        let run = rt.advance_run(&run.id).await.unwrap();
        assert_eq!(run.status, RunStatus::Paused);
        let retry_at = run.retry_at.expect("automatic retry");
        assert!(retry_at >= before + delay && retry_at <= before + delay + 5);
        assert_eq!(run.step("fail").unwrap().attempts, attempt);
        assert!(run.step("fail").unwrap().error.as_deref().unwrap_or("").contains("終了コード 3"));
        // Make the schedule due without sleeping out the real backoff.
        let mut saved = read_json(&paths, &run.id);
        saved["retry_at"] = json!(0);
        saved["status"] = json!("paused");
        write_json(&paths, &run.id, &saved);
    }
    let run = rt.advance_run(&run.id).await.unwrap();
    assert_eq!(run.status, RunStatus::Paused);
    assert!(run.retry_at.is_none(), "the fourth failure waits for the user");
    assert_eq!(run.step("fail").unwrap().attempts, 4);
    assert_eq!(rt.due_ids(), Vec::<String>::new());
}

#[tokio::test]
async fn agent_step_uses_the_mock_provider_and_interpolates_the_prompt() {
    let paths = temp_paths("agent");
    write_workflow(
        &paths,
        "agent.toml",
        r#"
name = "agent-flow"
version = 1

[payload.topic]
type = "string"

[[steps]]
id = "first"
type = "shell"
command = "printf context"

[[steps]]
id = "think"
type = "agent"
prompt = "Summarize {{payload.topic}} after {{steps.first.output}}"
model = "mock/m"
"#,
    );
    let mock = Arc::new(MockProvider::default());
    let mock_for_runner = mock.clone();
    let agent: AgentRunner = Arc::new(move |req| {
        let mock = mock_for_runner.clone();
        Box::pin(async move {
            let mut cfg = Config::default();
            cfg.learning.auto = false;
            cfg.extensions_enabled = false;
            let meta = Meta::new(req.cwd.clone(), "mock/m".into(), "native".into());
            let mut agent = Agent::with_provider(
                Arc::new(cfg),
                &meta,
                Arc::new(Perms::new(Mode::Full)),
                mock,
                "m".into(),
            );
            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
            let fut = agent.prompt(req.prompt.clone(), &tx);
            tokio::pin!(fut);
            let mut text = String::new();
            loop {
                tokio::select! {
                    result = &mut fut => { result?; break }
                    Some(event) = rx.recv() => match event {
                        greencode_core::Event::Text(t) => text.push_str(&t),
                        greencode_core::Event::Done { .. } => break,
                        _ => {}
                    }
                }
            }
            while let Ok(event) = rx.try_recv() {
                if let greencode_core::Event::Text(t) = event {
                    text.push_str(&t);
                }
            }
            Ok(text.trim().to_string())
        })
    });
    let rt = runtime(&paths, agent);
    let run = rt
        .start("agent-flow", json!({"topic": "the release"}), None, "cli")
        .unwrap();
    let run = rt.advance_run(&run.id).await.unwrap();
    assert_eq!(run.status, RunStatus::Done, "{:?}", run.error);
    assert_eq!(run.step("think").unwrap().output, "mock answer");
    let prompts = mock.prompts.lock().unwrap();
    assert_eq!(prompts.len(), 1);
    assert!(prompts[0].contains("Summarize the release after context"));
}

#[test]
fn definition_errors_are_per_file_and_keep_the_last_good_version() {
    let paths = temp_paths("definitions");
    write_workflow(
        &paths,
        "job.toml",
        r#"
name = "job"
version = 1

[[steps]]
id = "run"
type = "shell"
command = "true"
"#,
    );
    write_workflow(&paths, "broken.toml", "name = ");
    write_workflow(
        &paths,
        "duplicate.toml",
        r#"
name = "job"
version = 1

[[steps]]
id = "other"
type = "shell"
command = "true"
"#,
    );
    let defs = workflow::load_definitions(&paths);
    assert_eq!(defs.workflows.len(), 1);
    assert_eq!(defs.errors.len(), 2);
    assert!(defs.errors.iter().any(|(p, e)| p.ends_with("broken.toml") && e.contains("TOML")));
    assert!(defs.errors.iter().any(|(_, e)| e.contains("重複")));

    // A broken edit keeps the version that last loaded.
    std::fs::write(paths.workflows.join("job.toml"), "name = ").unwrap();
    let defs = workflow::load_definitions(&paths);
    assert!(defs.find("job").is_some());
    assert!(defs.errors.iter().any(|(p, _)| p.ends_with("job.toml")));
}

#[tokio::test]
async fn unfinished_runs_keep_their_definition_version() {
    let paths = temp_paths("versions");
    write_workflow(
        &paths,
        "job.toml",
        r#"
name = "job"
version = 1

[[steps]]
id = "gate"
type = "ask"
question = "Continue?"
options = ["Yes"]
"#,
    );
    let rt = runtime(&paths, unused_agent());
    let run = rt.start("job", json!({}), None, "cli").unwrap();
    let run = rt.advance_run(&run.id).await.unwrap();
    assert_eq!(run.status, RunStatus::Waiting);
    // Bump the file to v2, dropping v1 while its run is unfinished.
    write_workflow(
        &paths,
        "job.toml",
        r#"
name = "job"
version = 2

[[steps]]
id = "gate"
type = "ask"
question = "Continue?"
options = ["Yes"]
"#,
    );
    let defs = rt.definitions();
    assert!(defs.find_exact("job", 1).is_some(), "v1 is retained for the unfinished run");
    assert!(defs.find_exact("job", 2).is_some());
    let run = rt.answer(&run.id, "gate", "Yes").unwrap();
    let run = rt.advance_run(&run.id).await.unwrap();
    assert_eq!(run.status, RunStatus::Done);
}

#[derive(Default)]
struct MockProvider {
    prompts: Mutex<Vec<String>>,
}

impl Provider for MockProvider {
    fn complete<'a>(
        &'a self,
        req: Request<'a>,
        on_text: &'a (dyn Fn(&str) + Send + Sync),
    ) -> BoxFut<'a, anyhow::Result<Reply>> {
        Box::pin(async move {
            let messages = serde_json::to_string(req.messages).unwrap_or_default();
            self.prompts.lock().unwrap().push(messages);
            on_text("mock answer");
            Ok(Reply {
                blocks: vec![Block::Text {
                    text: "mock answer".into(),
                }],
                ..Default::default()
            })
        })
    }
}
