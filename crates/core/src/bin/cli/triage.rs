//! `greencode-cli triage`: start the app's triage playbook when the app cannot.
use anyhow::{Context, Result};
use std::io::{IsTerminal, Read};

const USAGE: &str = "greencode-cli triage [--backend NAME] [--model MODEL] [--json] [SYMPTOM]";

fn usage(message: &str) -> ! {
    eprintln!("{message}\n{USAGE}");
    std::process::exit(2)
}

pub(super) fn run(args: &[String]) -> Result<()> {
    let (mut backend, mut model, mut json) = (None, None, false);
    let mut symptom = Vec::new();
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--backend" | "--model" => {
                index += 1;
                let value = args.get(index).unwrap_or_else(|| usage("option needs a value")).clone();
                if args[index - 1] == "--backend" { backend = Some(value); } else { model = Some(value); }
            }
            "--json" => json = true,
            "--help" | "-h" => { println!("{USAGE}"); return Ok(()); }
            "-" => {
                let mut text = String::new();
                std::io::stdin().read_to_string(&mut text)?;
                symptom.push(text.trim_end().to_owned());
            }
            other if other.starts_with("--") => usage(&format!("unknown option {other}")),
            other => symptom.push(other.to_owned()),
        }
        index += 1;
    }
    // Piped input is the symptom; a terminal user can let the agent ask instead.
    let symptom = if symptom.is_empty() && !std::io::stdin().is_terminal() {
        let mut text = String::new();
        std::io::stdin().read_to_string(&mut text)?;
        text.trim_end().to_owned()
    } else {
        symptom.join(" ")
    };
    let dir = greencode_core::home().join("scratch").join(format!(
        "triage-{:x}",
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos())
    ));
    std::fs::create_dir_all(&dir).with_context(|| format!("Cannot create {}", dir.display()))?;
    if !json {
        eprintln!("トリアージを開始します（作業フォルダ: {}）", dir.display());
    }
    super::thread::start(
        Some(dir.to_string_lossy().into_owned()),
        backend,
        model,
        greencode_core::triage::prompt(&symptom),
        true,
        json,
    )
}
