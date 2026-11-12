//! Headless runner: `greencode-cli -p "prompt"`, or prompts from stdin, one per line.

use greencode_core::{config::Config, permission::{Mode, Perms}, session::Meta, Command, Decision, Event};
use std::{io::{BufRead, Write}, sync::Arc};

fn main() -> anyhow::Result<()> {
    let mut cfg = Config::load()?;
    let (mut prompt, mut backend, mut resume) = (None, "native".to_string(), None);
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        let mut val = || args.next().ok_or_else(|| anyhow::anyhow!("{a} needs a value"));
        match a.as_str() {
            "-p" => prompt = Some(val()?),
            "--model" => cfg.model = val()?,
            "--backend" => backend = val()?,
            "--resume" => resume = Some(val()?),
            "--mode" => {
                cfg.mode = match val()?.as_str() {
                    "ask" => Mode::Ask,
                    "auto-edit" => Mode::AutoEdit,
                    "full" => Mode::Full,
                    m => anyhow::bail!("unknown mode {m}"),
                }
            }
            _ => anyhow::bail!("usage: greencode-cli [-p PROMPT] [--model provider/model] [--mode ask|auto-edit|full] [--backend NAME] [--resume ID]"),
        }
    }
    let mut meta = Meta::new(std::env::current_dir()?, cfg.model.clone(), backend);
    if let Some(id) = resume {
        meta.id = id;
    }
    eprintln!("session {}", meta.id);
    let rt = tokio::runtime::Runtime::new()?;
    let perms = Arc::new(Perms::new(cfg.mode));
    let (tx, mut rx) = greencode_core::spawn(rt.handle(), Arc::new(cfg), meta, perms);
    let stdin = std::io::stdin();
    let mut turn = |text: String| -> bool {
        let _ = tx.send(Command::Prompt(text));
        let mut ok = true;
        while let Some(ev) = rx.blocking_recv() {
            match ev {
                Event::Text(t) => {
                    print!("{t}");
                    let _ = std::io::stdout().flush();
                }
                Event::ToolStart { name, input, .. } => eprintln!("\n> {name} {input}"),
                Event::ToolEnd { output, is_error, .. } => {
                    let first = output.lines().next().unwrap_or("");
                    eprintln!("{} {first}", if is_error { "✗" } else { "✓" });
                }
                Event::Permission { tool, summary, reply } => {
                    eprint!("\nAllow {tool}: {summary}? [y]es / [a]lways / [n]o: ");
                    let mut line = String::new();
                    let _ = stdin.lock().read_line(&mut line);
                    let _ = reply.send(match line.trim() {
                        "y" => Decision::Allow,
                        "a" => Decision::Always,
                        _ => Decision::Deny,
                    });
                }
                Event::Usage(u) => eprintln!("\n[in {} · cached {} · cache-write {} · out {}]", u.input, u.cache_read, u.cache_write, u.output),
                Event::Notify(n) => eprintln!("\n[{n}]"),
                Event::Error(e) => {
                    eprintln!("\nerror: {e}");
                    ok = false;
                }
                Event::Done => break,
            }
        }
        println!();
        ok
    };
    if let Some(p) = prompt {
        if !turn(p) {
            std::process::exit(1);
        }
    } else {
        loop {
            let mut line = String::new();
            if stdin.lock().read_line(&mut line)? == 0 {
                break;
            }
            if !line.trim().is_empty() {
                turn(line.trim().to_string());
            }
        }
    }
    Ok(())
}
