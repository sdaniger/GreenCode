//! Terminal and headless entry point for the shared GreenCode agent engine.

use greencode_core::{config::Config, permission::{Mode, Perms}, session::{Meta, Session}, Command, Decision, Event};
use serde_json::{json, Value};
use std::{io::{BufRead, IsTerminal, Write}, path::PathBuf, sync::Arc};

#[path = "cli/dialog.rs"]
mod dialog;
#[path = "cli/html.rs"]
mod html;
mod tui;

fn read_line(prompt: &str) -> anyhow::Result<String> {
    eprint!("{prompt}");
    let mut line = String::new();
    anyhow::ensure!(std::io::stdin().read_line(&mut line)? != 0, "no input");
    Ok(line.trim().to_string())
}

fn auth(cfg: &mut Config, args: &[String]) -> anyhow::Result<()> {
    use greencode_core::{auth, config::PRESETS};
    let label = |cfg: &Config, name: &str| cfg.providers.get(name).map(|p| auth::status(name, p).label()).unwrap_or_default();
    let sub = args.first().map(String::as_str).unwrap_or("list");
    match sub {
        "list" | "ls" => {
            for (name, p) in &cfg.providers {
                if auth::status(name, p).signed_in() || ["anthropic", "openai", "openrouter", "google", "github-copilot", "chatgpt"].contains(&name.as_str()) {
                    println!("{name:<14}{}", auth::status(name, p).label());
                }
            }
            let ag = greencode_core::antigravity::status();
            let ag_label = if ag.signed_in {
                format!("Google OAuth ({})", ag.auth_type)
            } else {
                "not signed in (run auth login antigravity)".into()
            };
            println!("{:<14}{ag_label}", "antigravity");
            println!("\nExternal agents use their own login: opencode auth login, pi, omp, codex, claude.");
        }
        // Borrows an existing Claude Code / Codex CLI sign-in without copying its rotating refresh
        // token, so those CLIs stay signed in; the borrowed token is used until it expires.
        "import" => {
            let claude = greencode_core::provider::anthropic_oauth::import_into(&greencode_core::provider::anthropic_oauth::path())?;
            match claude {
                Some(_) => println!("Imported the Claude Code login; the anthropic provider uses it until the token expires."),
                None => println!("No Claude Code login imported (missing ~/.claude/.credentials.json, or a browser login is already saved)."),
            }
            let codex = greencode_core::provider::codex::import_into(&greencode_core::provider::codex::path())?;
            match codex {
                Some(_) => println!("Imported the Codex CLI login; the chatgpt provider uses it until the token expires."),
                None => println!("No Codex CLI login imported (missing ~/.codex/auth.json, or a browser login is already saved)."),
            }
            println!("Refreshing a borrowed token is never attempted: re-run `greencode-cli auth import` after the source CLI renews it (a borrowed token is replaced; a browser login never is).");
        }
        "login" => {
            let mut method_flag: Option<String> = None;
            let mut target: Option<String> = None;
            let mut it = args.iter().skip(1);
            while let Some(arg) = it.next() {
                if arg == "--method" {
                    method_flag = Some(it.next().ok_or_else(|| anyhow::anyhow!("--method needs a value"))?.clone());
                } else if target.is_none() {
                    target = Some(arg.clone());
                } else {
                    anyhow::bail!("unexpected argument `{arg}`");
                }
            }
            let mut claude = false;
            let mut name = match target {
                Some(n) => n,
                None => {
                    let mut names: Vec<String> = PRESETS.iter().map(|p| p.0.to_string()).collect();
                    names.extend(cfg.providers.keys().filter(|k| !PRESETS.iter().any(|p| p.0 == k.as_str())).cloned());
                    names.push("antigravity".into());
                    for (i, n) in names.iter().enumerate() {
                        let st = if n == "antigravity" {
                            let ag = greencode_core::antigravity::status();
                            if ag.signed_in { format!("Google OAuth ({})", ag.auth_type) } else { "not signed in".into() }
                        } else {
                            label(cfg, n)
                        };
                        eprintln!("{:>2}) {n:<14}{st}", i + 1);
                    }
                    let answer = read_line("Provider number or name: ")?;
                    match answer.parse::<usize>() {
                        Ok(i) => names.get(i.wrapping_sub(1)).cloned().ok_or_else(|| anyhow::anyhow!("no provider number {i}"))?,
                        Err(_) => answer,
                    }
                }
            };
            if greencode_core::antigravity::is_alias(&name) {
                let method = match method_flag {
                    Some(m) => m,
                    None if std::io::stdin().is_terminal() => {
                        let choice = read_line("1) Google account (oauth-personal)  2) Gemini Enterprise (oauth-business)  3) GEMINI_API_KEY  [1]: ")?;
                        match choice.as_str() {
                            "" | "1" | "oauth-personal" => "oauth-personal".into(),
                            "2" | "oauth-business" => "oauth-business".into(),
                            "3" | "gemini-api-key" => "gemini-api-key".into(),
                            other => anyhow::bail!("unknown Antigravity auth method `{other}`"),
                        }
                    }
                    None => "oauth-personal".into(),
                };
                let rt = tokio::runtime::Runtime::new()?;
                let cwd = std::env::current_dir()?;
                match rt.block_on(greencode_core::antigravity::start_login(cfg, Some(&method), cwd.clone()))? {
                    greencode_core::antigravity::LoginStart::Ready(done) => {
                        if done.models.is_empty() {
                            println!("Signed in to antigravity ({}).", done.auth_type);
                        } else {
                            println!("Signed in to antigravity ({}; {} models).", done.auth_type, done.models.len());
                        }
                    }
                    greencode_core::antigravity::LoginStart::Browser(pending) => {
                        let auth_url = pending.url.clone();
                        eprintln!("Open {auth_url} and sign in with Google.");
                        eprintln!("Waiting for approval (if 127.0.0.1 cannot load in a remote browser, paste the redirect URL here)…");
                        #[cfg(target_os = "macos")]
                        let _ = std::process::Command::new("/usr/bin/open").arg(&auth_url).status();
                        let done = rt.block_on(async {
                            let mut finish = tokio::spawn(greencode_core::antigravity::finish_login(pending, cwd));
                            if std::io::stdin().is_terminal() {
                                let (line_tx, mut line_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
                                std::thread::spawn(move || {
                                    let stdin = std::io::stdin();
                                    for line in stdin.lock().lines().map_while(Result::ok) {
                                        if line_tx.send(line).is_err() {
                                            break;
                                        }
                                    }
                                });
                                let mut stdin_open = true;
                                loop {
                                    tokio::select! {
                                        res = &mut finish => break res.map_err(|e| anyhow::anyhow!("{e}"))?,
                                        maybe = line_rx.recv(), if stdin_open => match maybe {
                                            Some(line) => {
                                                let trimmed = line.trim();
                                                if !trimmed.is_empty() {
                                                    if let Err(e) = greencode_core::antigravity::submit_callback_url(trimmed, Some(&auth_url)).await {
                                                        eprintln!("{e}");
                                                    }
                                                }
                                            }
                                            None => stdin_open = false,
                                        }
                                    }
                                }
                            } else {
                                finish.await.map_err(|e| anyhow::anyhow!("{e}"))?
                            }
                        })?;
                        if done.models.is_empty() {
                            println!("Signed in to antigravity ({}).", done.auth_type);
                        } else {
                            println!("Signed in to antigravity ({}; {} models).", done.auth_type, done.models.len());
                        }
                    }
                }
                return Ok(());
            }
            if name == "claude" { name = "anthropic".into(); claude = true; }
            if name == "copilot" { name = "github-copilot".into(); }
            if name == "codex" { name = "chatgpt".into(); }
            let mut vibe = matches!(name.as_str(), "vibe" | "mistral-vibe");
            if vibe { name = "mistral".into(); }
            if name == "mistral" && !vibe {
                let choice = read_line("1) Mistral Vibe subscription (browser)  2) API key  [1]: ")?;
                vibe = choice.is_empty() || choice == "1";
            }
            if vibe {
                let rt = tokio::runtime::Runtime::new()?;
                let sign_in = rt.block_on(greencode_core::provider::mistral::start())?;
                eprintln!("Open {} and approve Mistral Vibe access.", sign_in.url);
                eprintln!("Waiting for approval…");
                #[cfg(target_os = "macos")]
                let _ = std::process::Command::new("/usr/bin/open").arg(&sign_in.url).status();
                let key = rt.block_on(greencode_core::provider::mistral::finish(sign_in))?;
                auth::set_key(cfg, "mistral", &key)?;
                println!("Signed in to mistral with Mistral Vibe.");
                return Ok(());
            }
            let p = cfg.providers.get(&name).ok_or_else(|| anyhow::anyhow!("unknown provider {name}"))?;
            if p.api_key_env.is_empty() && name != "anthropic" && name != "chatgpt" {
                println!("{name} needs no key.");
                return Ok(());
            }
            if name == "chatgpt" {
                let rt = tokio::runtime::Runtime::new()?;
                let device = rt.block_on(greencode_core::provider::codex::start())?;
                eprintln!("Open {} and enter the code: {}", device.verification_uri, device.user_code);
                eprintln!("Waiting for approval…");
                #[cfg(target_os = "macos")]
                let _ = std::process::Command::new("/usr/bin/open").arg(&device.verification_uri).status();
                let tokens = rt.block_on(greencode_core::provider::codex::finish(device))?;
                greencode_core::provider::codex::save(&greencode_core::provider::codex::path(), &tokens)?;
                println!("Signed in to chatgpt.");
                return Ok(());
            }
            if name == "github-copilot" {
                let rt = tokio::runtime::Runtime::new()?;
                let device = rt.block_on(greencode_core::provider::copilot::start())?;
                eprintln!("Open {} and enter the code: {}", device.verification_uri, device.user_code);
                eprintln!("Waiting for approval…");
                #[cfg(target_os = "macos")]
                let _ = std::process::Command::new("/usr/bin/open").arg(&device.verification_uri).status();
                let token = rt.block_on(greencode_core::provider::copilot::finish(device))?;
                auth::set_token(cfg, "github-copilot", &token)?;
                println!("Signed in to github-copilot.");
                return Ok(());
            }
            if name == "anthropic" && !claude {
                let choice = read_line("1) Claude Pro/Max subscription (browser)  2) API key  [1]: ")?;
                claude = choice.is_empty() || choice == "1";
            }
            if claude {
                #[cfg(unix)]
                {
                    let login = greencode_core::provider::anthropic_oauth::Login::new()?;
                    let url = login.url();
                    let _ = std::process::Command::new("/usr/bin/open").arg(&url).status();
                    eprintln!("Open this URL in a browser:\n{url}");
                    let code = read_line("Paste the authorization code: ")?;
                    anyhow::ensure!(!code.is_empty(), "no authorization code provided");
                    tokio::runtime::Runtime::new()?.block_on(auth::claude_login(cfg, login, code.as_str()))?;
                    println!("Signed in to anthropic with a Claude subscription.");
                }
                #[cfg(not(unix))]
                anyhow::bail!("Anthropic OAuth credential storage requires Unix");
            } else {
                use std::io::IsTerminal;
                let tty = std::io::stdin().is_terminal();
                let stty = |arg: &str| { let _ = std::process::Command::new("stty").arg(arg).status(); };
                if tty { stty("-echo"); }
                let key = read_line(&format!("API key for {name}: "));
                if tty { stty("echo"); eprintln!(); }
                auth::set_key(cfg, &name, &key?)?;
                println!("Saved the {name} key to {}.", auth::path().display());
            }
        }
        "logout" => {
            // Without a provider, offer the signed-in ones like `auth login` does.
            let name = match args.get(1) {
                Some(name) => name.clone(),
                None => {
                    // Only credentials GreenCode stores can be removed; env vars, config.toml keys and keyless servers cannot.
                    let removable = |m: &auth::Method| matches!(m, auth::Method::StoredKey | auth::Method::Subscription);
                    let mut signed: Vec<_> = cfg.providers.iter().filter(|(name, p)| removable(&auth::status(name, p))).map(|(n, _)| n.clone()).collect();
                    if greencode_core::antigravity::status().signed_in {
                        signed.push("antigravity".into());
                    }
                    anyhow::ensure!(!signed.is_empty(), "no provider is signed in through GreenCode");
                    for (i, name) in signed.iter().enumerate() { eprintln!("{}) {name}", i + 1); }
                    eprint!("Provider number or name to sign out: ");
                    let mut line = String::new();
                    anyhow::ensure!(std::io::stdin().read_line(&mut line)? != 0, "no provider chosen");
                    let line = line.trim();
                    if let Ok(n) = line.parse::<usize>() { signed.get(n.saturating_sub(1)).cloned().ok_or_else(|| anyhow::anyhow!("no provider {n}"))? } else { line.to_string() }
                }
            };
            auth::logout(cfg, &name)?;
            println!("Signed out of {name}.");
        }
        "accounts" | "save" | "switch" | "remove" => {
            let usage = || anyhow::anyhow!("usage: greencode-cli auth accounts PROVIDER | save PROVIDER NAME | switch PROVIDER NAME | remove PROVIDER NAME");
            let provider = args.get(1).ok_or_else(usage)?;
            if sub == "accounts" {
                let (active, all) = auth::accounts(provider)?;
                for a in &all { println!("{} {a}", if active.as_deref() == Some(a) { "*" } else { " " }); }
                if all.is_empty() { println!("no accounts for {provider}"); }
                return Ok(());
            }
            let account = args.get(2).ok_or_else(usage)?;
            match sub {
                "save" => { auth::save_as(cfg, provider, account)?; println!("Saved the current {provider} credential as {account}."); }
                "switch" => { auth::switch(cfg, provider, account)?; println!("{provider} now uses the {account} account."); }
                _ => { auth::remove_account(provider, account)?; println!("Removed the {account} account of {provider}."); }
            }
        }
        other => anyhow::bail!("unknown auth command {other}; use login, list, logout, accounts, save, switch or remove"),
    }
    Ok(())
}

fn main() -> anyhow::Result<()> {
    let policy_args: Vec<String> = std::env::args().skip(1).collect();
    if policy_args.first().is_some_and(|s| s == "config") {
        return greencode_core::config::settings::cli(&policy_args[1..]);
    }
    if policy_args.first().is_some_and(|s| s == "mcp") {
        anyhow::ensure!(policy_args.len() == 1, "usage: greencode-cli mcp");
        let cfg = Config::load()?;
        greencode_core::tools::SANDBOX.store(cfg.sandbox, std::sync::atomic::Ordering::Relaxed);
        return tokio::runtime::Runtime::new()?.block_on(greencode_core::mcp_server::stdio());
    }
    if policy_args.first().is_some_and(|s| s == "serve") {
        let mut host = "127.0.0.1".to_string();
        let mut port = 47322u16;
        let mut rotate = false;
        let mut args = policy_args[1..].iter();
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--host" => host = args.next().ok_or_else(|| anyhow::anyhow!("--host needs a value"))?.clone(),
                "--port" => port = args.next().ok_or_else(|| anyhow::anyhow!("--port needs a value"))?.parse()?,
                "--rotate" => rotate = true,
                _ => anyhow::bail!("usage: greencode-cli serve [--port PORT] [--host HOST] [--rotate]"),
            }
        }
        let ip: std::net::IpAddr = host.parse()?;
        anyhow::ensure!(ip.is_loopback() || ip == std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED), "host must be a loopback address or 0.0.0.0");
        if ip.is_unspecified() { eprintln!("Warning: anyone on the network with this URL can control agents."); }
        let cfg = Config::load()?;
        greencode_core::tools::SANDBOX.store(cfg.sandbox, std::sync::atomic::Ordering::Relaxed);
        let rt = tokio::runtime::Runtime::new()?;
        return rt.block_on(async {
            let listener = tokio::net::TcpListener::bind((ip, port)).await?;
            let token = greencode_core::remote::token(rotate)?;
            let addr = listener.local_addr()?;
            println!("http://{addr}/?token={token}");
            greencode_core::remote::serve(listener, token, cfg).await
        });
    }
    if policy_args.first().is_some_and(|s| s == "policy") {
        match policy_args.get(1).map(String::as_str) {
            Some("check") if policy_args.len() == 3 => println!("{}", greencode_core::policy::check(&std::fs::read_to_string(&policy_args[2])?)?),
            Some("show") if policy_args.len() == 2 => println!("{}", greencode_core::policy::load()?.map_or_else(|| "No policy".into(), |p| p.summary())),
            _ => anyhow::bail!("usage: greencode-cli policy check FILE | policy show"),
        }
        return Ok(());
    }
    if policy_args.first().is_some_and(|s| s == "update") {
        let mut check = false;
        let mut repo = None;
        let mut args = policy_args[1..].iter();
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--check" => check = true,
                "--repo" => repo = Some(args.next().ok_or_else(|| anyhow::anyhow!("--repo needs OWNER/REPO"))?.clone()),
                _ => anyhow::bail!("usage: greencode-cli update [--check] [--repo OWNER/REPO]"),
            }
        }
        let cfg = Config::load()?;
        let repo = greencode_core::update::repository(cfg.update_repo.as_deref(), repo.as_deref())?;
        let rt = tokio::runtime::Runtime::new()?;
        if check {
            let release = rt.block_on(greencode_core::update::check(&repo))?;
            println!("current {} · latest {} · {}", greencode_core::update::CURRENT_VERSION, release.version, release.url);
        } else {
            let executable = std::env::current_exe()?;
            let result = rt.block_on(greencode_core::update::install(&repo, &executable))?;
            if result.installed { println!("Updated GreenCode {} → {}; restart GreenCode to use it.", result.current, result.latest); }
            else { println!("GreenCode {} is already current (latest {}).", result.current, result.latest); }
        }
        return Ok(());
    }
    if policy_args.first().is_some_and(|s| s == "agents") {
        use greencode_core::registry;
        match (policy_args.get(1).map(String::as_str), policy_args.get(2), policy_args.len()) {
            (Some("list"), None, 2) => {
                let rt = tokio::runtime::Runtime::new()?;
                let entries = registry::parse(&rt.block_on(registry::fetch())?)?;
                let installed = registry::installed();
                for e in entries {
                    println!("{} {}\t{}\t{}\t{}", if installed.iter().any(|i| i.id == e.id) { "*" } else { " " }, e.id, e.name, e.version, e.description);
                }
            }
            (Some("install"), Some(id), 3) => {
                let lookup = if id == "antigravity" { greencode_core::antigravity::REGISTRY_ID } else { id.as_str() };
                let rt = tokio::runtime::Runtime::new()?;
                let entries = registry::parse(&rt.block_on(registry::fetch())?)?;
                let entry = entries.iter().find(|e| e.id == lookup).ok_or_else(|| anyhow::anyhow!("unknown registry agent: {id}"))?;
                rt.block_on(registry::install(entry))?;
                println!("Installed {} ({lookup})", entry.name);
            }
            (Some("remove"), Some(id), 3) => {
                let lookup = if id == "antigravity" { greencode_core::antigravity::REGISTRY_ID } else { id.as_str() };
                registry::uninstall(lookup)?;
                println!("Removed {lookup}");
            }
            _ => anyhow::bail!("usage: greencode-cli agents list | agents install ID | agents remove ID"),
        }
        return Ok(());
    }
    // launchd puts --scheduled first, so even configuration and session-load errors are recorded.
    let mut claim = if policy_args.first().is_some_and(|a| a == "--scheduled") {
        let label = policy_args.get(1).ok_or_else(|| anyhow::anyhow!("--scheduled needs a value"))?;
        match greencode_core::launchd::claim_schedule(&greencode_core::home().join("scheduled"), label)? {
            Some(claim) => Some(claim),
            None => return Ok(()),
        }
    } else { None };
    let (run_args, overlays) = config_args(policy_args)?;
    let mut cfg = Config::load_with_overlays(&overlays)?;
    greencode_core::tools::SANDBOX.store(cfg.sandbox, std::sync::atomic::Ordering::Relaxed);
    let (mut prompt, mut backend, mut resume) = (None, "native".to_string(), None);
    let (mut rpc, mut json_output, mut model_override, mut backend_override) = (None, false, false, false);
    let (mut subagent_override, mut scheduled, mut mode_override) = (false, None::<String>, false);
    let (mut native_controls, mut thinking_override) = (false, false);
    let (mut tui, mut text, mut use_latest) = (false, false, false);
    let mut no_resume = false;
    let mut launch_agents_dir = None::<std::path::PathBuf>;
    let mut args = run_args.into_iter();
    while let Some(a) = args.next() {
        let mut val = || args.next().ok_or_else(|| anyhow::anyhow!("{a} needs a value"));
        match a.as_str() {
            "-p" | "--print" => prompt = Some(val()?),
            "--tui" => tui = true,
            "--text" => text = true,
            "-c" | "--continue" => use_latest = true,
            "--no-resume" => no_resume = true,
            "--model" => { cfg.model = val()?; model_override = true; }
            "--thinking" => {
                let level = val()?;
                cfg.effort = (level != "default").then_some(level);
                thinking_override = true;
                native_controls = true;
            }
            "--tools" => {
                let names = val()?;
                cfg.tools = names.split(',').map(str::trim).map(str::to_owned).collect();
                anyhow::ensure!(cfg.tools.iter().all(|n| !n.is_empty()), "--tools needs comma-separated names; use --no-tools to disable all");
                native_controls = true;
            }
            "--no-tools" => { cfg.no_tools = true; native_controls = true; }
            "--no-skills" => { cfg.skills_enabled = false; native_controls = true; }
            "--no-rules" => { cfg.rules_enabled = false; native_controls = true; }
            "--no-extensions" => { cfg.extensions_enabled = false; native_controls = true; }
            "--no-context-files" => { cfg.context_files = false; native_controls = true; }
            "--hide-thinking" => { cfg.tui.hide_thinking = true; native_controls = true; }
            "--system-prompt" => { cfg.system_prompt = Some(val()?); native_controls = true; }
            "--append-system-prompt" => { cfg.append_system_prompt = Some(val()?); native_controls = true; }
            "--subagent-model" => {
                let model = val()?;
                cfg.subagent_model = if model == "parent" { None } else { Some(model) };
                subagent_override = true;
            }
            "--backend" => {
                backend = greencode_core::antigravity::canonical_backend(&val()?).to_string();
                backend_override = true;
            }
            "--scheduled" => scheduled = Some(val()?),
            "--launch-agents-dir" => launch_agents_dir = Some(val()?.into()),
            "--resume" => resume = Some(val()?),
            "--rpc" => rpc = Some(serde_json::from_str::<Value>(&val()?)?),
            "--json" => json_output = true,
            "--opencode-login" => {
                let executable = cfg.backends.iter().find(|b| b.name == "opencode")
                    .and_then(|b| b.command.first()).filter(|cmd| std::path::Path::new(cmd).file_name().is_some_and(|n| n == "opencode"))
                    .map(String::as_str).unwrap_or("opencode");
                let status = std::process::Command::new(executable).args(["auth", "login"]).args(args).status()?;
                anyhow::ensure!(status.success(), "OpenCode login failed: {status}");
                return Ok(());
            }
            "auth" => {
                let rest: Vec<String> = args.collect();
                auth(&mut cfg, &rest)?;
                return Ok(());
            }
            "--hermes-profiles" => {
                for profile in greencode_core::hermes::discover()? {
                    println!("{}\t{}", profile.name, profile.home.display());
                }
                return Ok(());
            }
            "--import-hermes" => {
                let profile = greencode_core::hermes::resolve(&val()?)?;
                let name = greencode_core::hermes::import(&profile, &mut cfg)?;
                println!("Imported {name} from {}\nUse --backend {name} for a new thread. Hermes retains its own configuration, credentials, skills and memory.", profile.home.display());
                return Ok(());
            }
            "--models" => {
                let name = greencode_core::antigravity::canonical_backend(&val()?).to_string();
                let rt = tokio::runtime::Runtime::new()?;
                let models = rt.block_on(greencode_core::backend_models(Arc::new(cfg), &name, std::env::current_dir()?));
                if models.is_empty() { anyhow::bail!("{name} reported no models; check that it is installed and signed in"); }
                println!("{}", models.join("\n"));
                return Ok(());
            }
            "--import-session" => {
                let path = std::path::PathBuf::from(val()?);
                let meta = greencode_core::import::import_file(&path, &cfg.model)?;
                println!("Imported as {}\nContinue with --resume {}", meta.id, meta.id);
                return Ok(());
            }
            "--export-session" => {
                let id = val()?;
                let path = args.next().map(std::path::PathBuf::from).unwrap_or_else(|| greencode_core::home().join("exports").join(format!("{id}.jsonl")));
                greencode_core::session::Session::export(&id, &path)?;
                println!("Exported {id} to {}\nImport it on another machine with --import-session {}", path.display(), path.display());
                return Ok(());
            }
            "--export-html" => {
                let id = val()?;
                let path = args.next().map(PathBuf::from);
                anyhow::ensure!(args.next().is_none(), "usage: --export-html ID [FILE]");
                println!("Exported {id} to {}", html::export(&id, path)?.display());
                return Ok(());
            }
            "--subagents" => {
                for agent in cfg.task.visible(&greencode_core::subagents::discover(&std::env::current_dir()?)) {
                    println!("{}  {}  {}  {}", agent.name, if agent.bundled { "bundled" } else { "custom" }, agent.tools.as_ref().map(|t| t.join(",")).unwrap_or_else(|| "all".into()), agent.description);
                }
                return Ok(());
            }
            "--backends" => {
                println!("native\tAPI\tconfigured in config.toml");
                for b in &cfg.backends {
                    println!("{}\t{:?}\t{}", b.name, b.protocol, b.command.join(" "));
                }
                return Ok(());
            }
            "--mode" => {
                mode_override = true;
                use tui::parse_mode;
                cfg.mode = parse_mode(&val()?)?;
            }
            "--plan" => { cfg.mode = Mode::Plan; mode_override = true; }
            "--help" | "-h" => {
                println!("greencode-cli [--tui | --text | -p PROMPT | --rpc JSON] [--json] [--config FILE] [--model provider/model] [--mode ask|auto-edit|full|plan] [--backend NAME] [--resume ID | --continue]");
                println!("Interactive native sessions open in the TUI by default; --text uses the line-oriented runner");
                println!("--no-resume bypasses [tui] auto_resume for this launch");
                println!("greencode-cli config path | list | get KEY | set KEY VALUE | reset KEY");
                println!("greencode-cli config import-omp FILE [FILE...] | --profile NAME   print supported settings as a TOML overlay");
                println!("Native controls: --plan --thinking LEVEL --tools NAME,... --no-tools --no-skills --no-rules --no-extensions --no-context-files --hide-thinking --system-prompt TEXT --append-system-prompt TEXT");
                println!("greencode-cli --scheduled LABEL --resume ID -p PROMPT   run a launchd-scheduled turn (approvals are denied), then remove its job");
                println!("greencode-cli --hermes-profiles | --import-hermes NAME_OR_DIRECTORY");
                println!("greencode-cli agents list | agents install ID | agents remove ID   manage ACP registry agents");
                println!("greencode-cli --import-session SESSION.jsonl   import a Claude/Codex transcript or a GreenCode export");
                println!("greencode-cli --import-session EXPORT.json   import `opencode session export` output");
                println!("greencode-cli --export-session ID [FILE]   write a portable copy of a thread");
                println!("greencode-cli --export-html ID [FILE]   write a local HTML transcript without overwriting an existing file");
                println!("greencode-cli --models BACKEND   list the models an external backend offers");
                println!("greencode-cli auth login [PROVIDER] [--method METHOD] | auth list | auth logout PROVIDER   manage provider sign-in (API keys, Claude, Copilot, ChatGPT, Antigravity)");
                println!("greencode-cli auth import   borrow the existing Claude Code / Codex CLI sign-in without copying its refresh token");
                println!("greencode-cli --opencode-login [OPENCODE_ARGS...]    sign in using OpenCode's own login flow");
                println!("Native sub-agents: --subagent-model provider/model (or parent to follow the parent model)");
                println!("greencode-cli update [--check] [--repo OWNER/REPO]   check or install the latest GitHub Release asset");
                return Ok(());
            }
            _ => anyhow::bail!("unknown option {a}; use --help"),
        }
    }
    if prompt.is_some() && rpc.is_some() { anyhow::bail!("use either -p or --rpc"); }
    anyhow::ensure!(!(tui && (text || prompt.is_some() || rpc.is_some() || json_output)), "--tui is incompatible with --text, -p, --rpc or --json");
    anyhow::ensure!(!use_latest || resume.is_none(), "use either --resume or --continue");
    anyhow::ensure!(!no_resume || (!use_latest && resume.is_none()), "--no-resume conflicts with --resume or --continue");
    if scheduled.is_some() && claim.is_none() {
        claim = match greencode_core::launchd::claim_schedule(&greencode_core::home().join("scheduled"), scheduled.as_deref().unwrap())? {
            Some(claim) => Some(claim),
            None => return Ok(()),
        };
    }
    if scheduled.is_some() { anyhow::ensure!(prompt.is_some() && rpc.is_none(), "--scheduled requires -p PROMPT"); }
    let model = if backend == "native" || model_override { cfg.model.clone() } else { "default".into() };
    let mut meta = Meta::new(std::env::current_dir()?, model, backend.clone());
    let terminal = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
    let can_tui = terminal && !text && prompt.is_none() && rpc.is_none() && !json_output;
    if use_latest || (resume.is_none() && !no_resume && can_tui && backend == "native" && cfg.tui.auto_resume) {
        resume = Session::list().into_iter()
            .filter(|m| m.cwd == meta.cwd && m.backend == backend && Session::open(&m.id).is_some())
            .filter_map(|m| Some((std::fs::metadata(Session::path(&m.id)).ok()?.modified().ok()?, m.id)))
            .max()
            .map(|(_, id)| id);
        anyhow::ensure!(!use_latest || resume.is_some(), "no saved {backend} session in {}", meta.cwd.display());
    }
    meta.subagent_model = cfg.subagent_model.clone();
    if let Some(id) = resume {
        meta = Session::load(&id)?.0;
        if backend_override && meta.backend != backend { anyhow::bail!("cannot change a resumed thread's backend"); }
        if model_override { meta.model = cfg.model.clone(); }
        if subagent_override { meta.subagent_model = cfg.subagent_model.clone(); }
    }
    anyhow::ensure!(!native_controls || meta.backend == "native", "native controls do not change external agents; use that backend's configuration");
    if let Some(level) = cfg.effort.as_deref().filter(|_| thinking_override) {
        anyhow::ensure!(greencode_core::provider::effort_levels(&cfg, &meta.model).contains(&level), "model {} does not support thinking level {level}", meta.model);
    }
    if subagent_override && meta.backend != "native" { anyhow::bail!("--subagent-model is for native threads; configure sub-agents in the external backend"); }
    if tui { anyhow::ensure!(terminal && meta.backend == "native", "--tui requires a terminal and a native thread"); }
    let interactive = can_tui && meta.backend == "native";
    let rt = tokio::runtime::Runtime::new()?;
    if scheduled.is_some() && !mode_override { cfg.mode = Mode::AutoEdit; }
    if let Some(policy) = &cfg.policy {
        if mode_override {
            anyhow::ensure!(!policy.settings.default_mode.as_ref().is_some_and(|s| s.locked && s.value != cfg.mode), "Permission mode is locked by policy ({})", policy.organization);
        }
        policy.check_mode(cfg.mode)?;
        anyhow::ensure!(policy.backend_allowed(&meta.backend), "管理ポリシーで制限されています（{}）", policy.organization);
    }
    if subagent_override {
        if let Some(session) = Session::open(&meta.id) { session.append(&json!({"meta": meta}))?; }
        if let Some(mut view) = Session::view(&meta.id)? {
            view.meta.subagent_model = meta.subagent_model.clone();
            Session::save_view(&view)?;
        }
    }
    if interactive { return tui::run(rt.handle(), cfg, meta, overlays.is_empty()); }
    eprintln!("session {}", meta.id);
    let perms = Arc::new(Perms::new(cfg.mode));
    let (tx, mut rx) = greencode_core::spawn(rt.handle(), Arc::new(cfg), meta, perms);
    let stdin = std::io::stdin();
    let mut turn = |command: Command| -> bool {
        if tx.send(command).is_err() { return false; }
        let mut ok = true;
        let mut done = false;
        while let Some(ev) = rx.blocking_recv() {
            if json_output { println!("{}", greencode_core::event_json(&ev)); }
            match ev {
                Event::Text(t) => {
                    if !json_output { print!("{t}"); }
                    let _ = std::io::stdout().flush();
                }
                Event::Thinking(_) | Event::Image(_) => {}
                Event::ToolStart { name, input, .. } => eprintln!("\n> {name} {input}"),
                Event::ToolEnd { output, is_error, .. } => {
                    let first = output.lines().next().unwrap_or("");
                    eprintln!("{} {first}", if is_error { "✗" } else { "✓" });
                }
                Event::Permission { tool, summary, reply } if scheduled.is_some() => {
                    eprintln!("\n[scheduled run: denied {tool}: {summary} (needs approval)]");
                    let _ = reply.send(Decision::Deny);
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
                Event::UsageAux(u) => eprintln!("\n[aux in {} · cached {} · out {}]", u.input, u.cache_read, u.output),
                Event::Remote { command, data } => {
                    if !json_output && rpc.as_ref().is_some_and(|r| r["type"] == command) {
                        println!("{}", serde_json::to_string_pretty(&data).unwrap_or_default());
                    }
                    if !json_output && matches!(command.as_str(), "notify" | "command_output") {
                        if let Some(text) = data["message"].as_str().or_else(|| data["text"].as_str()).filter(|s| !s.is_empty()) {
                            eprintln!("\n[{text}]");
                        }
                    }
                }
                Event::Dialog { request, reply } => {
                    let value = dialog(&request, &mut stdin.lock());
                    let _ = reply.send(value);
                }
                Event::Notify(n) => eprintln!("\n[{n}]"),
                Event::Error(e) => {
                    eprintln!("\nerror: {e}");
                    ok = false;
                }
                Event::Done => { done = true; break; }
            }
        }
        if !json_output { println!(); }
        ok && done
    };
    if let Some(command) = rpc.clone().map(Command::Remote).or_else(|| prompt.map(Command::Prompt)) {
        let ok = turn(command);
        if !ok { anyhow::bail!("turn failed or ended without completion"); }
        if let Some(claim) = claim.take() {
            claim.complete()?;
            let _ = std::io::stdout().flush();
            if let Some(label) = &scheduled {
                if let Err(e) = greencode_core::launchd::uninstall(&launch_agents_dir.unwrap_or_else(greencode_core::launchd::agents_dir), label) {
                    eprintln!("scheduled turn completed, but its launchd job could not be removed: {e:#}");
                }
            }
        }
    } else {
        use std::io::IsTerminal;
        loop {
            if stdin.is_terminal() { eprint!("> "); }
            let mut line = String::new();
            if stdin.lock().read_line(&mut line)? == 0 {
                break;
            }
            if !line.trim().is_empty() {
                turn(Command::Prompt(line.trim().to_string()));
            }
        }
    }
    Ok(())
}

/// Remove runner overlays without interpreting prompt strings or a subcommand's own arguments.
fn config_args(args: Vec<String>) -> anyhow::Result<(Vec<String>, Vec<PathBuf>)> {
    let mut argv = args.into_iter();
    let (mut rest, mut overlays) = (Vec::new(), Vec::new());
    while let Some(arg) = argv.next() {
        match arg.as_str() {
            "--config" => overlays.push(argv.next().ok_or_else(|| anyhow::anyhow!("--config needs a file"))?.into()),
            s if s.starts_with("--config=") => overlays.push(s["--config=".len()..].into()),
            "auth" | "--opencode-login" => { rest.push(arg); rest.extend(argv); break; }
            "-p" | "--print" | "--model" | "--subagent-model" | "--backend" | "--scheduled" |
            "--launch-agents-dir" | "--resume" | "--rpc" | "--import-hermes" | "--models" |
            "--import-session" | "--export-session" | "--export-html" | "--mode" | "--thinking" | "--tools" |
            "--system-prompt" | "--append-system-prompt" => {
                rest.push(arg);
                rest.extend(argv.next());
            }
            _ => rest.push(arg),
        }
    }
    Ok((rest, overlays))
}

fn dialog(request: &Value, stdin: &mut impl BufRead) -> Value {
    let cancelled = || json!({"cancelled": true});
    let Some(mut dialog) = dialog::Dialog::new(request.clone()) else { return cancelled(); };
    if dialog.secret() {
        eprintln!("Secret input requires the terminal UI.");
        return cancelled();
    }
    for text in &dialog.prompts {
        eprintln!("{text}");
        let mut line = String::new();
        if stdin.read_line(&mut line).unwrap_or(0) == 0 { return cancelled(); }
        dialog.answers.push(line);
    }
    dialog.finish()
}

#[cfg(test)]
mod dialog_tests {
    use super::*;

    #[test]
    fn ask_accepts_multiple_choices_and_free_text() {
        let request = json!({"method":"ask","questions":[
            {"id":"a","question":"Choose","multiSelect":true,"options":[{"label":"one"},{"label":"two"}]},
            {"id":"b","question":"Explain","options":[]}
        ]});
        let result = dialog(&request, &mut std::io::Cursor::new("1,2\ndetails\n"));
        assert_eq!(result["answers"], json!([
            {"id":"a","selectedOptions":["one","two"]},
            {"id":"b","selectedOptions":[],"customInput":"details"}
        ]));
        assert_eq!(dialog(&request, &mut std::io::Cursor::new("1\n")), json!({"cancelled":true}));
    }
}

