//! `greencode-cli thread …`: host scripts that list, read, start and continue threads.
use anyhow::{bail, Context, Result};
use greencode_core::{config::Config, permission::Perms, session::{Meta, Session}, Command, Decision, Event};
use serde_json::json;
use std::{io::{IsTerminal, Read, Write}, path::PathBuf, sync::Arc};

const USAGE: &str = "greencode-cli thread list [--json]\n\
greencode-cli thread show ID [--json]\n\
greencode-cli thread start [--project DIR] [--backend NAME] [--model MODEL] [--wait] [--json] PROMPT\n\
greencode-cli thread send ID [--wait] [--json] PROMPT\n\
Use `-` as PROMPT to read it from stdin.";

fn usage() -> ! {
    eprintln!("{USAGE}");
    std::process::exit(2)
}

fn fail_usage(message: &str) -> ! {
    eprintln!("{message}\n{USAGE}");
    std::process::exit(2)
}

/// A short title from the first line, as the app names a thread from its first message.
fn title(prompt: &str) -> String {
    prompt.lines().next().unwrap_or_default().chars().take(60).collect()
}

fn read_prompt(value: &str) -> Result<String> {
    let text = if value == "-" {
        let mut text = String::new();
        std::io::stdin().read_to_string(&mut text)?;
        text.trim_end().to_owned()
    } else {
        value.to_owned()
    };
    if text.trim().is_empty() {
        bail!("the prompt is empty");
    }
    Ok(text)
}

pub(super) fn run(args: &[String]) -> Result<()> {
    let Some(command) = args.first().map(String::as_str) else { usage() };
    let (mut project, mut backend, mut model) = (None::<String>, None::<String>, None::<String>);
    let (mut wait, mut json) = (false, false);
    let mut positional = Vec::new();
    let mut index = 1;
    while index < args.len() {
        let arg = &args[index];
        match arg.as_str() {
            "--json" => json = true,
            "--wait" => wait = true,
            "--project" | "--backend" | "--model" => {
                index += 1;
                let value = args.get(index).unwrap_or_else(|| fail_usage(&format!("{arg} needs a value"))).clone();
                match arg.as_str() {
                    "--project" => project = Some(value),
                    "--backend" => backend = Some(value),
                    _ => model = Some(value),
                }
            }
            "--help" | "-h" => { println!("{USAGE}"); return Ok(()); }
            "--" => {
                positional.extend(args[index + 1..].iter().cloned());
                break;
            }
            s if s.starts_with("--") => fail_usage(&format!("unknown option {s}")),
            _ => positional.push(arg.clone()),
        }
        index += 1;
    }
    match command {
        "list" => {
            if !positional.is_empty() || project.is_some() || backend.is_some() || model.is_some() || wait {
                fail_usage("thread list takes no arguments besides --json");
            }
            let threads = Session::list();
            if json {
                println!("{}", serde_json::to_string_pretty(&threads)?);
            } else {
                for meta in threads {
                    println!("{}\t{}\t{}\t{}", meta.id, meta.backend, meta.model, if meta.title.is_empty() { "(untitled)" } else { &meta.title });
                }
            }
            Ok(())
        }
        "show" => {
            if positional.len() != 1 || project.is_some() || backend.is_some() || model.is_some() || wait {
                fail_usage("thread show needs exactly one thread ID");
            }
            let id = &positional[0];
            if json {
                let (meta, messages) = Session::load(id).with_context(|| format!("thread {id} not found"))?;
                println!("{}", serde_json::to_string_pretty(&json!({"meta": meta, "messages": messages}))?);
            } else {
                println!("{}", Session::markdown(id).with_context(|| format!("thread {id} not found"))?);
            }
            Ok(())
        }
        "start" => {
            if positional.len() != 1 {
                fail_usage("thread start needs exactly one PROMPT");
            }
            let prompt = read_prompt(&positional[0])?;
            start(project.or_else(|| std::env::current_dir().ok().map(|p| p.to_string_lossy().into_owned())), backend, model, prompt, wait, json)
        }
        "send" => {
            if positional.len() != 2 || project.is_some() || backend.is_some() || model.is_some() {
                fail_usage("thread send needs a thread ID and PROMPT");
            }
            let prompt = read_prompt(&positional[1])?;
            send(&positional[0], prompt, wait, json)
        }
        other => fail_usage(&format!("unknown thread command {other}")),
    }
}

/// Starts a new thread. Without `--wait` a detached copy of this CLI runs the turn, because a
/// host script must be able to exit while the agent keeps working.
pub(super) fn start(project: Option<String>, backend: Option<String>, model: Option<String>, prompt: String, wait: bool, json: bool) -> Result<()> {
    let cfg = Config::load()?;
    let cwd = PathBuf::from(project.unwrap_or_else(|| ".".into())).canonicalize().context("project directory not found")?;
    if !cwd.is_dir() {
        bail!("{} is not a directory", cwd.display());
    }
    let backend = backend.unwrap_or_else(|| "native".into());
    let model = model.unwrap_or_else(|| if backend == "native" { cfg.model.clone() } else { "default".into() });
    let mut meta = Meta::new(cwd, model, backend);
    meta.subagent_model = cfg.subagent_model.clone();
    meta.title = title(&prompt);
    if let Some(policy) = &cfg.policy {
        policy.check_mode(cfg.mode)?;
        anyhow::ensure!(policy.backend_allowed(&meta.backend), "backend blocked by policy");
    }
    Session::create(&meta)?;
    if !wait {
        detach(&meta.id, &prompt)?;
        return print_result(json, json!({"id":meta.id, "running":true}));
    }
    run_turn(cfg, meta, prompt, json)
}

fn send(id: &str, prompt: String, wait: bool, json: bool) -> Result<()> {
    let cfg = Config::load()?;
    let (meta, _) = Session::load(id).with_context(|| format!("thread {id} not found"))?;
    if let Some(policy) = &cfg.policy {
        anyhow::ensure!(policy.backend_allowed(&meta.backend), "backend blocked by policy");
    }
    if !wait {
        detach(id, &prompt)?;
        return print_result(json, json!({"id":id, "running":true}));
    }
    run_turn(cfg, meta, prompt, json)
}

fn print_result(json: bool, value: serde_json::Value) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(&value)?);
    } else {
        println!("{}", value["id"].as_str().unwrap_or_default());
    }
    Ok(())
}

fn detach(id: &str, prompt: &str) -> Result<()> {
    let executable = std::env::current_exe().context("Cannot find greencode-cli")?;
    std::process::Command::new(executable)
        .args(["--resume", id, "-p", prompt])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .context("Cannot start the background turn")?;
    Ok(())
}

/// One process at a time runs a thread started from the CLI. (The app keeps its turns in memory,
/// so it does not take this lock; do not send to a thread that is open in the app.)
fn lock_thread(id: &str) -> Result<std::fs::File> {
    let dir = greencode_core::home().join("threads");
    std::fs::create_dir_all(&dir)?;
    let file = std::fs::OpenOptions::new().read(true).write(true).create(true).open(dir.join(format!("{id}.lock")))?;
    match file.try_lock() {
        Ok(()) => Ok(file),
        Err(std::fs::TryLockError::WouldBlock) => bail!("thread {id} already has a running turn"),
        Err(std::fs::TryLockError::Error(e)) => Err(e.into()),
    }
}

fn final_text(id: &str, streamed: &str) -> String {
    let last = Session::load(id).ok().and_then(|(_, messages)| {
        messages.into_iter().rev().find(|m| m.role == greencode_core::Role::Assistant).map(|m| {
            m.content.into_iter().filter_map(|b| if let greencode_core::Block::Text { text } = b { Some(text) } else { None }).collect::<Vec<_>>().join("\n")
        })
    });
    last.filter(|text| !text.is_empty()).unwrap_or_else(|| streamed.to_owned())
}

fn run_turn(cfg: Config, meta: Meta, prompt: String, json: bool) -> Result<()> {
    let _lock = lock_thread(&meta.id)?;
    let mode = Session::view(&meta.id)?.map_or(cfg.mode, |view| view.mode);
    if let Some(policy) = &cfg.policy {
        policy.check_mode(mode)?;
    }
    let rt = tokio::runtime::Runtime::new()?;
    let print_wake = cfg.bash_opts.r#async.print_wake;
    let (tx, mut rx) = greencode_core::spawn(rt.handle(), Arc::new(cfg), meta.clone(), Arc::new(Perms::new(mode)));
    tx.send(Command::Prompt(prompt))?;
    let interactive = std::io::stdin().is_terminal() && !json;
    let (mut streamed, mut error, mut waiting, mut done) = (String::new(), None::<String>, false, false);
    while let Some(event) = rx.blocking_recv() {
        match event {
            Event::Text(text) => {
                streamed.push_str(&text);
                if !json {
                    print!("{text}");
                    let _ = std::io::stdout().flush();
                }
            }
            Event::ToolStart { name, input, .. } => eprintln!("\n> {name} {input}"),
            Event::ToolEnd { output, is_error, .. } => eprintln!("{} {}", if is_error { "✗" } else { "✓" }, output.lines().next().unwrap_or("")),
            Event::Error(message) => error = Some(message),
            Event::Permission { tool, summary, reply } => {
                let decision = if interactive {
                    eprint!("\nAllow {tool}: {summary}? [y]es / [a]lways / [n]o: ");
                    let mut line = String::new();
                    let _ = std::io::stdin().read_line(&mut line);
                    match line.trim() { "y" => Decision::Allow, "a" => Decision::Always, _ => Decision::Deny }
                } else {
                    Decision::Deny
                };
                let _ = reply.send(decision);
            }
            Event::Dialog { request, reply } => {
                let answer = if interactive {
                    super::dialog(&request, &mut std::io::stdin().lock())
                } else {
                    json!({"cancelled": true})
                };
                if answer["cancelled"] == true {
                    waiting = true;
                }
                let _ = reply.send(answer);
            }
            // Delegated tasks wake this thread for another turn; exiting now would kill them.
            Event::Done { waiting_for_delegates: true } => {}
            Event::Done { waiting_for_delegates: false } => { done = true; break; }
            _ => {}
        }
    }
    if !done && error.is_none() {
        error = Some("agent exited before finishing the turn".into());
    }
    // `async.printWake`: print mode waits for the run's background jobs and reports each result.
    if print_wake {
        while let Some(job) = rt.block_on(greencode_core::tools::next_job_result()) {
            let output = std::fs::read_to_string(&job.log).unwrap_or_default();
            if json {
                println!("{}", serde_json::to_string(&json!({"background": job.command, "status": job.status, "output": output}))?);
            } else {
                println!("--- background: {} (exit {:?}) ---", job.command, job.status);
                println!("{output}");
            }
        }
    }
    if !json {
        println!();
    }
    let text = if error.is_some() { streamed.clone() } else { final_text(&meta.id, &streamed) };
    if waiting {
        // The agent asked a person something and this run could not answer it.
        if json {
            println!("{}", serde_json::to_string_pretty(&json!({"id":meta.id, "running":false, "text":text, "waiting":true}))?);
        }
        std::process::exit(2);
    }
    if let Some(error) = &error {
        if json {
            println!("{}", serde_json::to_string_pretty(&json!({"id":meta.id, "text":text, "error":error}))?);
        } else {
            eprintln!("error: {error}");
        }
        std::process::exit(1);
    }
    if json {
        println!("{}", serde_json::to_string_pretty(&json!({"id":meta.id, "running":false, "text":text}))?);
    }
    Ok(())
}
