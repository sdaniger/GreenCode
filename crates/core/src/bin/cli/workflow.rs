//! `greencode-cli workflow …`: definitions, runs, answers and resumes.
use greencode_core::{
    config::Config,
    workflow::{self, Runtime, RunStatus},
};
use serde_json::Value;
use std::sync::Arc;

const USAGE: &str = "greencode-cli workflow list [--status running|waiting|paused|done|failed|cancelled]
       start NAME [--payload JSON] [--key K]
       show RUN
       resume RUN
       cancel RUN [--reason WHY]
       answer RUN STEP VALUE

ワークフローはラボ機能です。設定 → ラボ → ワークフローをオンにするか、
ui.json に \"labs_workflows\": true を設定してください。";

fn title(run: &workflow::Run) -> String {
    format!(
        "{}  {} v{}  {}",
        run.id,
        run.workflow,
        run.version,
        run.status.label()
    )
}

fn print_run(run: &workflow::Run) {
    println!("{}", title(run));
    if let Some(question) = &run.question {
        println!("  質問: {}", question.title);
        println!("  選択肢: {}", question.options.join(" / "));
    }
    if let Some(error) = &run.error {
        println!("  {}", error.lines().next().unwrap_or(""));
    }
    for step in &run.steps {
        let state = match step.status {
            workflow::StepState::Pending => "待機",
            workflow::StepState::Running => "実行中",
            workflow::StepState::Waiting => "回答待ち",
            workflow::StepState::Done => "完了",
            workflow::StepState::Failed => "失敗",
        };
        let detail = step
            .error
            .as_deref()
            .or((!step.output.is_empty()).then_some(step.output.as_str()))
            .unwrap_or("");
        let first = detail.lines().next().unwrap_or("");
        println!("  {state:6} {}  {first}", step.id);
    }
    if let Some(at) = run.retry_at.filter(|_| run.status == RunStatus::Paused) {
        let minutes = at.saturating_sub(workflow::now()).div_ceil(60);
        println!("  {minutes} 分後に自動再試行（resume で今すぐ）");
    }
}

fn advance(rt: &tokio::runtime::Runtime, wf: &Runtime, id: &str) -> anyhow::Result<workflow::Run> {
    Ok(rt.block_on(wf.advance_run(id))?)
}

pub fn cli(args: &[String]) -> anyhow::Result<()> {
    if !workflow::enabled() {
        anyhow::bail!("{USAGE}");
    }
    let wf = Runtime::new(
        Arc::new(Config::load()?),
        std::env::current_dir()?,
    );
    let rt = tokio::runtime::Runtime::new()?;
    match args.first().map(String::as_str) {
        Some("list") => {
            let status = args
                .iter()
                .position(|a| a == "--status")
                .and_then(|i| args.get(i + 1))
                .map(String::as_str);
            for run in wf.runs() {
                let matches = status.is_none_or(|s| {
                    serde_json::to_value(run.status)
                        .ok()
                        .and_then(|v| v.as_str().map(str::to_string))
                        .is_some_and(|v| v == s)
                });
                if matches {
                    println!("{}  {}", run.id, run.summary());
                }
            }
        }
        Some("start") => {
            let name = args
                .get(1)
                .filter(|a| !a.starts_with("--"))
                .ok_or_else(|| anyhow::anyhow!("使い方: {USAGE}"))?;
            let mut payload = Value::Object(Default::default());
            let mut key = None;
            let mut i = 2;
            while i < args.len() {
                match args[i].as_str() {
                    "--payload" => {
                        payload = serde_json::from_str(
                            args.get(i + 1)
                                .ok_or_else(|| anyhow::anyhow!("--payload に JSON が必要です"))?,
                        )?;
                        i += 2;
                    }
                    "--key" => {
                        key = Some(
                            args.get(i + 1)
                                .ok_or_else(|| anyhow::anyhow!("--key に値が必要です"))?
                                .clone(),
                        );
                        i += 2;
                    }
                    other => anyhow::bail!("不明なオプション {other}\n{USAGE}"),
                }
            }
            let run = wf.start(name, payload, key, "cli")?;
            println!("実行 {}", run.id);
            let run = advance(&rt, &wf, &run.id)?;
            print_run(&run);
            if run.status == RunStatus::Waiting {
                if let Some(q) = &run.question {
                    println!(
                        "\n回答: greencode-cli workflow answer {} {} {:?}",
                        run.id,
                        q.step,
                        q.options.first().cloned().unwrap_or_default()
                    );
                }
            }
        }
        Some("show") => {
            let run = workflow::load_run(&wf.paths, args.get(1).map(String::as_str).unwrap_or(""))?;
            print_run(&run);
        }
        Some("resume") => {
            let run = wf.resume(args.get(1).map(String::as_str).unwrap_or(""))?;
            let run = advance(&rt, &wf, &run.id)?;
            print_run(&run);
        }
        Some("cancel") => {
            let reason = args
                .iter()
                .position(|a| a == "--reason")
                .and_then(|i| args.get(i + 1))
                .map(String::as_str);
            let run = wf.cancel(args.get(1).map(String::as_str).unwrap_or(""), reason)?;
            print_run(&run);
        }
        Some("answer") => {
            let id = args
                .get(1)
                .ok_or_else(|| anyhow::anyhow!("使い方: {USAGE}"))?;
            let step = args
                .get(2)
                .ok_or_else(|| anyhow::anyhow!("使い方: {USAGE}"))?;
            let value = args
                .get(3)
                .ok_or_else(|| anyhow::anyhow!("使い方: {USAGE}"))?;
            let run = wf.answer(id, step, value)?;
            let run = advance(&rt, &wf, &run.id)?;
            print_run(&run);
        }
        _ => anyhow::bail!("{USAGE}"),
    }
    Ok(())
}
