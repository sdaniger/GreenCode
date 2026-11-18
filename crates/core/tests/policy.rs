use greencode_core::{config::Config, permission::Mode, provider};

#[test]
fn policy_locked_default_and_provider_limit() {
    let dir = std::env::temp_dir().join(format!("greencode-policy-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let policy = dir.join("policy.json");
    std::fs::write(dir.join("config.toml"), "mode = 'full'\n").unwrap();
    std::fs::write(&policy, r#"{"schemaVersion":1,"organization":"Test","settings":{"defaultMode":{"value":"ask","locked":true}},"limits":{"providers":["openai"]}}"#).unwrap();
    // This integration test is a separate process and has no concurrent tests.
    unsafe { std::env::set_var("GREENCODE_HOME", &dir); std::env::set_var("GREENCODE_POLICY", &policy); }
    let cfg = Config::load().unwrap();
    assert_eq!(cfg.mode, Mode::Ask);
    assert!(provider::make(&cfg, "anthropic/test").err().unwrap().to_string().contains("Test"));
    std::fs::remove_dir_all(dir).unwrap();
}
