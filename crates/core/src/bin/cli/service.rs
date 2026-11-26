//! `greencode-cli service …`: install, inspect, restart and remove the background server.
use anyhow::{bail, Result};

const USAGE: &str = "greencode-cli service install [--lan] | status | restart | uninstall";

fn usage(message: &str) -> ! {
    eprintln!("{message}\n{USAGE}");
    std::process::exit(2)
}

pub(super) fn run(args: &[String]) -> Result<()> {
    let Some(command) = args.first().map(String::as_str) else { usage("service needs a command") };
    let mut lan = false;
    for arg in &args[1..] {
        match arg.as_str() {
            "--lan" => lan = true,
            "--help" | "-h" => { println!("{USAGE}"); return Ok(()); }
            other => usage(&format!("unknown service option {other}")),
        }
    }
    if lan && command != "install" {
        bail!("--lan only applies to `service install`");
    }
    match command {
        "install" => println!("{}", greencode_core::service::install(lan)?),
        "status" => print!("{}", greencode_core::service::status()?),
        "restart" => println!("{}", greencode_core::service::restart()?),
        "uninstall" => println!("{}", greencode_core::service::uninstall()?),
        other => usage(&format!("unknown service command {other}")),
    }
    Ok(())
}
