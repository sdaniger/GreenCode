#[cfg(unix)]
#[test]
fn login_delegates_to_opencode_and_propagates_failure() {
    use std::{os::unix::fs::PermissionsExt, process::Command};
    let dir = std::env::temp_dir().join(format!("greencode-opencode-login-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let executable = dir.join("opencode");
    std::fs::write(&executable, "#!/bin/sh\n[ \"$1\" = auth ] && [ \"$2\" = login ] && [ \"$#\" = 2 ] || exit 9\nexit 0\n").unwrap();
    std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
    let run = || Command::new(env!("CARGO_BIN_EXE_greencode-cli"))
        .arg("--opencode-login").env("PATH", &dir).env("GREENCODE_HOME", dir.join("config")).output().unwrap();
    assert!(run().status.success());
    std::fs::write(&executable, "#!/bin/sh\n[ \"$1\" = auth ] && [ \"$2\" = login ] && [ \"$3\" = openai ] && [ \"$4\" = --method ] && [ \"$5\" = oauth ] && [ \"$#\" = 5 ] || exit 9\n").unwrap();
    assert!(Command::new(env!("CARGO_BIN_EXE_greencode-cli"))
        .args(["--opencode-login", "openai", "--method", "oauth"])
        .env("PATH", &dir).env("GREENCODE_HOME", dir.join("config")).status().unwrap().success());
    std::fs::write(&executable, "#!/bin/sh\nexit 7\n").unwrap();
    assert!(!run().status.success());
    std::fs::remove_dir_all(dir).unwrap();
}
