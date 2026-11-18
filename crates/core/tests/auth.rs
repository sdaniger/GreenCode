use greencode_core::{auth, config::Config};

/// GREENCODE_HOME is process-wide.
static HOME: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[test]
fn auth_keys_are_stored_replaced_and_removed() {
    let _guard = HOME.lock().unwrap_or_else(|e| e.into_inner());
    let home = std::env::temp_dir().join(format!("greencode-auth-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&home);
    std::env::set_var("GREENCODE_HOME", &home);
    let mut cfg = Config::load().unwrap();
    auth::set_key(&mut cfg, "openai", " k1 ").unwrap();
    assert_eq!(auth::status("openai", &cfg.providers["openai"]), auth::Method::StoredKey);
    #[cfg(unix)]
    assert_eq!(std::os::unix::fs::PermissionsExt::mode(&std::fs::metadata(auth::path()).unwrap().permissions()) & 0o777, 0o600);
    assert!(std::fs::read_to_string(auth::path()).unwrap().contains("k1"));
    let again = Config::load().unwrap();
    assert_eq!(again.providers["openai"].api_key.as_deref(), Some("k1"));
    assert!(again.providers["openai"].stored);

    auth::set_key(&mut cfg, "openai", "k2").unwrap();
    assert_eq!(Config::load().unwrap().providers["openai"].api_key.as_deref(), Some("k2"));

    auth::logout(&mut cfg, "openai").unwrap();
    assert_eq!(Config::load().unwrap().providers["openai"].api_key, None);
    assert!(auth::logout(&mut cfg, "openai").is_err());
    assert!(auth::set_key(&mut cfg, "nope", "k").is_err());

    // A corrupt store is never overwritten.
    std::fs::write(auth::path(), "{").unwrap();
    assert!(auth::set_key(&mut cfg, "openai", "k3").is_err());
    assert_eq!(std::fs::read_to_string(auth::path()).unwrap(), "{");
    std::fs::remove_file(auth::path()).unwrap();

    // Key -> subscription switch updates the in-memory config.
    auth::set_key(&mut cfg, "anthropic", "sk-a").unwrap();
    assert_eq!(auth::status("anthropic", &cfg.providers["anthropic"]), auth::Method::StoredKey);
    #[cfg(unix)]
    {
        use greencode_core::provider::anthropic_oauth::{self, StoredTokens};
        StoredTokens { access_token: "a".into(), refresh_token: "r".into(), expires_at: u64::MAX }.save(&anthropic_oauth::path()).unwrap();
        auth::forget_key(&mut cfg, "anthropic").unwrap();
        assert_eq!(auth::status("anthropic", &cfg.providers["anthropic"]), auth::Method::Subscription);
    }
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn auth_accounts_are_saved_switched_and_removed() {
    let _guard = HOME.lock().unwrap_or_else(|e| e.into_inner());
    let home = std::env::temp_dir().join(format!("greencode-auth-accounts-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&home);
    std::env::set_var("GREENCODE_HOME", &home);
    let mut cfg = Config::load().unwrap();
    auth::set_key(&mut cfg, "openai", "k1").unwrap();
    auth::save_as(&mut cfg, "openai", "work").unwrap();
    auth::set_key(&mut cfg, "openai", "k2").unwrap();
    auth::save_as(&mut cfg, "openai", "personal").unwrap();
    assert_eq!(auth::accounts("openai").unwrap(), (Some("personal".into()), vec!["personal".to_string(), "work".to_string()]));

    auth::switch(&mut cfg, "openai", "work").unwrap();
    assert_eq!(cfg.providers["openai"].api_key.as_deref(), Some("k1"));
    assert_eq!(Config::load().unwrap().providers["openai"].api_key.as_deref(), Some("k1"));
    assert_eq!(auth::accounts("openai").unwrap(), (Some("work".into()), vec!["personal".to_string(), "work".to_string()]));
    assert!(auth::remove_account("openai", "work").is_err());
    assert!(auth::switch(&mut cfg, "openai", "nope").is_err());

    auth::switch(&mut cfg, "openai", "personal").unwrap();
    assert_eq!(cfg.providers["openai"].api_key.as_deref(), Some("k2"));
    auth::remove_account("openai", "work").unwrap();
    assert_eq!(auth::accounts("openai").unwrap().1, vec!["personal".to_string()]);

    // Subscription files: the inactive one lives under accounts/.
    #[cfg(unix)]
    {
        use greencode_core::provider::anthropic_oauth::{self, StoredTokens};
        let tokens = |a: &str| StoredTokens { access_token: a.into(), refresh_token: "r".into(), expires_at: u64::MAX };
        tokens("a1").save(&anthropic_oauth::path()).unwrap();
        auth::save_as(&mut cfg, "anthropic", "sub").unwrap();
        auth::set_key(&mut cfg, "anthropic", "sk-a").unwrap();
        auth::save_as(&mut cfg, "anthropic", "key").unwrap();
        assert!(!anthropic_oauth::path().exists());
        assert!(home.join("accounts/anthropic/sub.json").exists());
        auth::switch(&mut cfg, "anthropic", "sub").unwrap();
        assert!(anthropic_oauth::path().exists());
        assert_eq!(auth::status("anthropic", &cfg.providers["anthropic"]), auth::Method::Subscription);
        auth::switch(&mut cfg, "anthropic", "key").unwrap();
        assert_eq!(cfg.providers["anthropic"].api_key.as_deref(), Some("sk-a"));
        assert!(!anthropic_oauth::path().exists());
    }
    let _ = std::fs::remove_dir_all(&home);
}
