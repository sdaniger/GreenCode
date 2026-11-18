#[cfg(unix)]
#[test]
fn cli_imports_persistently_and_resumes_without_copying_context() {
    use std::{fs, os::unix::fs::PermissionsExt, process::Command};
    let dir = std::env::temp_dir().join(format!("greencode-hermes-cli-{}", std::process::id()));
    let home = dir.join("greencode");
    let root = dir.join("Hermes home with spaces");
    let coder = root.join("profiles/coder");
    let bin = dir.join("bin");
    for path in [&home, &coder, &bin] { fs::create_dir_all(path).unwrap(); }
    fs::write(root.join("config.yaml"), "model: root-model\n").unwrap();
    fs::write(root.join("active_profile"), "other\n").unwrap();
    fs::write(coder.join("config.yaml"), "model: profile-model\n").unwrap();
    fs::write(home.join("config.toml"), "model = 'openai/leave-unchanged'\n").unwrap();
    fs::copy(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/hermes_acp.py"), bin.join("hermes")).unwrap();
    fs::set_permissions(bin.join("hermes"), fs::Permissions::from_mode(0o755)).unwrap();
    let mut paths = vec![bin];
    paths.extend(std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()));
    let path = std::env::join_paths(paths).unwrap();
    let run = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_greencode-cli")).args(args)
            .env("GREENCODE_HOME", &home).env("HERMES_HOME", &coder).env("PATH", &path)
            .current_dir(&dir).output().unwrap()
    };
    let discovered = run(&["--hermes-profiles"]);
    assert!(discovered.status.success(), "{:?}", discovered);
    assert!(String::from_utf8_lossy(&discovered.stdout).contains("coder\t"));
    for _ in 0..2 {
        let imported = run(&["--import-hermes", "coder"]);
        assert!(imported.status.success(), "{:?}", imported);
    }
    let listed = run(&["--backends"]);
    assert!(listed.status.success(), "{:?}", listed);
    assert_eq!(String::from_utf8_lossy(&listed.stdout).matches("hermes:coder\t").count(), 1);
    let first = run(&["--backend", "hermes:coder", "--json", "-p", "first"]);
    assert!(first.status.success(), "{:?}", first);
    assert!(String::from_utf8_lossy(&first.stdout).contains("coder:first"));
    let session_path = fs::read_dir(home.join("sessions")).unwrap().next().unwrap().unwrap().path();
    let id = session_path.file_stem().unwrap().to_str().unwrap();
    let resumed = run(&["--resume", id, "--json", "-p", "second"]);
    assert!(resumed.status.success(), "{:?}", resumed);
    assert!(String::from_utf8_lossy(&resumed.stdout).contains("coder:second"));
    assert_eq!(fs::read_to_string(coder.join("config.yaml")).unwrap(), "model: profile-model\n");
    assert_eq!(fs::read_to_string(home.join("config.toml")).unwrap(), "model = 'openai/leave-unchanged'\n");
    assert!(!home.join("skills").exists() && !home.join("auth.json").exists());
    let missing = run(&["--import-hermes", "missing"]);
    assert!(!missing.status.success());
    assert!(!home.join("hermes-profiles/missing.json").exists());
    fs::remove_dir_all(dir).unwrap();
}
