//! `greencode-cli collab|share|stream|skill`: relay hosting, encrypted shares, live streams and the
//! skill registry. The TUI's `/collab` and `/share` use the same modules.
use anyhow::{bail, Result};
use greencode_core::{config::Config, collab, share, skillshare, skills, stream};
use std::{path::PathBuf, time::Duration};

const USAGE: &str = "greencode-cli collab relay [--host HOST] [--port PORT]\n\
greencode-cli collab list [--json]\n\
greencode-cli share [THREAD] [--store blob|gist]\n\
greencode-cli share serve [--host HOST] [--port PORT] [--dir DIR]\n\
greencode-cli share open LINK [--json]\n\
greencode-cli stream [THREAD] [--follow] [--poll SECS]\n\
greencode-cli stream serve [--host HOST] [--port PORT] [--dir DIR]\n\
greencode-cli skill search QUERY\n\
greencode-cli skill install NAME[@VERSION] [--dir DIR]\n\
greencode-cli skill publish DIR [--version V]";

fn host_port(args: &[String]) -> Result<(String, u16)> {
    let (mut host, mut port) = ("127.0.0.1".to_string(), 0u16);
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        let mut value = || it.next().cloned().ok_or_else(|| anyhow::anyhow!("{arg} needs a value"));
        match arg.as_str() {
            "--host" => host = value()?,
            "--port" => port = value()?.parse()?,
            other => bail!("unexpected `{other}`\n{USAGE}"),
        }
    }
    Ok((host, port))
}

/// `GREENCODE_HOME/skills` holds skills the native engine already reads.
fn skill_dir() -> PathBuf {
    greencode_core::home().join("skills")
}

fn thread_or_latest(arg: Option<&String>) -> Result<String> {
    match arg {
        Some(id) => Ok(id.clone()),
        None => greencode_core::session::Session::list().into_iter().next().map(|meta| meta.id)
            .ok_or_else(|| anyhow::anyhow!("no saved thread; pass a THREAD ID")),
    }
}

pub fn run(args: &[String]) -> Result<()> {
    let Some(kind) = args.first().map(String::as_str) else {
        bail!("{USAGE}");
    };
    let rest = &args[1..];
    let cfg = Config::load()?;
    let rt = tokio::runtime::Runtime::new()?;
    match kind {
        "collab" => match rest.first().map(String::as_str) {
            Some("list") => {
                let rooms = collab::registry();
                if rest.iter().any(|a| a == "--json") {
                    println!("{}", serde_json::to_string_pretty(&rooms)?);
                } else if rooms.is_empty() {
                    println!("No collab rooms hosted from this machine");
                } else {
                    for room in rooms {
                        println!("{}  {}  {}  {}", room.room, room.mode, room.name, room.link);
                    }
                }
                Ok(())
            }
            Some("relay") => {
                let (host, port) = host_port(&rest[1..])?;
                rt.block_on(async move {
                    let listener = tokio::net::TcpListener::bind((host.as_str(), port)).await?;
                    let addr = listener.local_addr()?;
                    println!("Collab relay on http://{addr}");
                    println!("Set collab.relayUrl = \"wss://{addr}\" (the relay speaks HTTP; ws URLs map to it)");
                    collab::serve(listener).await
                })
            }
            _ => bail!("usage:\n{USAGE}"),
        },
        "share" => match rest.first().map(String::as_str) {
            Some("serve") => {
                let mut dir = share::store_dir();
                let mut args = Vec::new();
                let mut it = rest[1..].iter();
                while let Some(arg) = it.next() {
                    match arg.as_str() {
                        "--dir" => dir = it.next().map(PathBuf::from).ok_or_else(|| anyhow::anyhow!("--dir needs a value"))?,
                        _ => args.push(arg.clone()),
                    }
                }
                let (host, port) = host_port(&args)?;
                rt.block_on(share::serve_cli(&host, port, dir))
            }
            Some("open") => {
                let link = rest.get(1).ok_or_else(|| anyhow::anyhow!("usage: greencode-cli share open LINK"))?;
                let value = rt.block_on(share::open(link))?;
                let messages = value["messages"].as_array().map_or(0, Vec::len);
                let title = value["meta"]["title"].as_str().unwrap_or_default();
                if rest.iter().any(|a| a == "--json") {
                    println!("{}", serde_json::to_string_pretty(&value)?);
                } else {
                    println!("{title} · {messages} messages");
                    for message in value["messages"].as_array().into_iter().flatten() {
                        let role = message["role"].as_str().unwrap_or_default();
                        for block in message["content"].as_array().into_iter().flatten() {
                            let (kind, text) = if block["type"] == "text" { ("", block["text"].as_str().unwrap_or_default().to_owned()) }
                                else if block["type"] == "thinking" { ("thinking: ", block["thinking"].as_str().unwrap_or_default().to_owned()) }
                                else if block["type"] == "tool_use" { ("tool: ", format!("{} {}", block["name"].as_str().unwrap_or_default(), block["input"])) }
                                else if block["type"] == "tool_result" { ("result: ", block["content"].to_string()) }
                                else { continue };
                            println!("[{role}] {kind}{text}");
                        }
                    }
                }
                Ok(())
            }
            _ => {
                let mut thread = None;
                let mut cfg = cfg;
                let mut it = rest.iter();
                while let Some(arg) = it.next() {
                    match arg.as_str() {
                        "--store" => {
                            let value = it.next().ok_or_else(|| anyhow::anyhow!("--store needs blob or gist"))?;
                            cfg.collab_opts.share.store = serde_json::from_value(serde_json::Value::String(value.clone()))?;
                        }
                        "--json" => {}
                        other if other.starts_with('-') => bail!("unexpected `{other}`\n{USAGE}"),
                        other => thread = Some(other.to_owned()),
                    }
                }
                let thread = thread_or_latest(thread.as_ref())?;
                let link = rt.block_on(share::publish(&cfg, &thread))?;
                println!("{link}");
                Ok(())
            }
        },
        "stream" => match rest.first().map(String::as_str) {
            Some("serve") => {
                let mut dir = stream::store_dir();
                let mut args = Vec::new();
                let mut it = rest[1..].iter();
                while let Some(arg) = it.next() {
                    match arg.as_str() {
                        "--dir" => dir = it.next().map(PathBuf::from).ok_or_else(|| anyhow::anyhow!("--dir needs a value"))?,
                        _ => args.push(arg.clone()),
                    }
                }
                let (host, port) = host_port(&args)?;
                rt.block_on(stream::serve_cli(&host, port, dir))
            }
            _ => {
                let mut follow = false;
                let mut poll = Duration::from_secs(2);
                let mut thread = None;
                let mut it = rest.iter();
                while let Some(arg) = it.next() {
                    match arg.as_str() {
                        "--follow" => follow = true,
                        "--poll" => poll = Duration::from_secs_f64(it.next().ok_or_else(|| anyhow::anyhow!("--poll needs seconds"))?.parse()?),
                        other if other.starts_with('-') => bail!("unexpected `{other}`\n{USAGE}"),
                        other => thread = Some(other.to_owned()),
                    }
                }
                let thread = thread_or_latest(thread.as_ref())?;
                rt.block_on(stream::run(&cfg, &thread, follow, poll))
            }
        },
        "skill" => match rest.first().map(String::as_str) {
            Some("search") => {
                let query = rest.get(1).map(String::as_str).unwrap_or_default();
                let found = rt.block_on(skillshare::search(&cfg, query))?;
                if found.is_empty() {
                    println!("No skills match `{query}`");
                }
                for skill in found {
                    println!("{} {}  {}", skill.name, skill.version, skill.description);
                }
                Ok(())
            }
            Some("install") => {
                let spec = rest.get(1).ok_or_else(|| anyhow::anyhow!("usage: greencode-cli skill install NAME[@VERSION]"))?;
                let mut dir = skill_dir();
                let mut it = rest[2..].iter();
                while let Some(arg) = it.next() {
                    if arg == "--dir" {
                        dir = it.next().map(PathBuf::from).ok_or_else(|| anyhow::anyhow!("--dir needs a value"))?;
                    } else {
                        bail!("unexpected `{arg}`\n{USAGE}");
                    }
                }
                std::fs::create_dir_all(&dir)?;
                let version = rt.block_on(skillshare::install(&cfg, spec, &dir))?;
                println!("Installed {} {version} into {}", spec, dir.display());
                Ok(())
            }
            Some("publish") => {
                let dir = rest.get(1).map(PathBuf::from).ok_or_else(|| anyhow::anyhow!("usage: greencode-cli skill publish DIR"))?;
                let mut version = None;
                let mut it = rest[2..].iter();
                while let Some(arg) = it.next() {
                    match arg.as_str() {
                        "--version" => version = Some(it.next().ok_or_else(|| anyhow::anyhow!("--version needs a value"))?.as_str()),
                        other => bail!("unexpected `{other}`\n{USAGE}"),
                    }
                }
                if cfg.integration_opts.skills.registry_url.is_none() {
                    bail!("set skills.registry.url first (greencode-cli config set skills.registry_url https://…)");
                }
                println!("Published {}", rt.block_on(skillshare::publish(&cfg, &dir, version))?);
                Ok(())
            }
            _ => {
                // `greencode-cli skill` alone lists installed skills, like the TUI picker.
                let list = skills::list(&std::env::current_dir()?, &cfg);
                for skill in list {
                    println!("{}  {}", skill.name, skill.description);
                }
                Ok(())
            }
        },
        other => bail!("unknown command `{other}`\n{USAGE}"),
    }
}
