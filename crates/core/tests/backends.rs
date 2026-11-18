#![cfg(unix)]
use greencode_core::config::Config;
use std::os::unix::fs::PermissionsExt;

#[test]
fn installed_agents_become_acp_backends() {
    let dir = std::env::temp_dir().join(format!("greencode-backends-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let exe = dir.join("kiro-cli");
    std::fs::write(&exe, "#!/bin/sh\n").unwrap();
    std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::env::set_var("GREENCODE_HOME", dir.join("home"));
    let old = std::env::var_os("PATH").unwrap_or_default();
    let mut paths = vec![dir.clone()];
    paths.extend(std::env::split_paths(&old));
    std::env::set_var("PATH", std::env::join_paths(paths).unwrap());
    let cfg = Config::load();
    std::env::set_var("PATH", old);
    let cfg = cfg.unwrap();
    let kiro = cfg.backends.iter().find(|b| b.name == "kiro").expect("kiro backend");
    assert_eq!(kiro.command, [exe.to_string_lossy().as_ref(), "acp", "--agent-engine=v3", "--auth-method=cli"]);
    std::fs::remove_dir_all(dir).unwrap();
}
