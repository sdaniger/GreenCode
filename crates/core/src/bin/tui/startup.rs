//! `startup.showSplash` and `startup.setupWizard`: splash text and once-per-setup-version onboarding.
use anyhow::Result;
use greencode_core::config::{interaction::StartupCfg, settings};
use std::path::Path;

/// Bump when the wizard gains steps: files recording an older version show it again.
pub const SETUP_VERSION: u32 = 1;

/// Version recorded by the last completed or skipped setup; 0 when the file is missing or unreadable.
pub fn setup_version(home: &Path) -> u32 {
    std::fs::read_to_string(home.join("setup-version")).ok().and_then(|text| text.trim().parse().ok()).unwrap_or(0)
}

pub fn record_setup_version(home: &Path) -> Result<()> {
    std::fs::create_dir_all(home)?;
    let path = home.join("setup-version");
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, SETUP_VERSION.to_string())?;
    std::fs::rename(tmp, path)?;
    Ok(())
}

/// `startup.setupWizard`: newly added onboarding runs once per setup version.
pub fn wizard_due(cfg: &StartupCfg, home: &Path) -> bool {
    cfg.setup_wizard && setup_version(home) < SETUP_VERSION
}

/// Dismissing or finishing the wizard counts as seen, so it does not return on the next start.
pub fn finish(home: &Path, config: &Path, model: Option<&str>) -> Result<()> {
    if let Some(model) = model {
        let text = std::fs::read_to_string(config).unwrap_or_default();
        let out = settings::set_text(&text, "model", &[model.to_owned()])?;
        if let Some(dir) = config.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(config, out)?;
    }
    record_setup_version(home)
}

/// `startup.showSplash`: a normal interactive start shows it; Quiet Startup still suppresses it.
pub fn splash_due(cfg: &StartupCfg) -> bool {
    cfg.show_splash && !cfg.quiet
}

pub fn splash_lines() -> Vec<String> {
    vec![format!("◆ GreenCode {}", env!("CARGO_PKG_VERSION")), "native terminal · /help for commands".into()]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn home(name: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!("gc-startup-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        path
    }

    #[test]
    fn wizard_shows_once_per_setup_version_and_honors_the_switch() {
        let home = home("wizard");
        let cfg = StartupCfg { setup_wizard: true, ..StartupCfg::default() };
        assert!(wizard_due(&cfg, &home));
        record_setup_version(&home).unwrap();
        assert_eq!(setup_version(&home), SETUP_VERSION);
        assert!(!wizard_due(&cfg, &home));
        assert!(!wizard_due(&StartupCfg { setup_wizard: false, ..cfg.clone() }, &home), "off never shows it");
        // A file from an older setup version runs the newly added steps again.
        std::fs::write(home.join("setup-version"), "0").unwrap();
        assert!(wizard_due(&cfg, &home));
        std::fs::write(home.join("setup-version"), "garbage").unwrap();
        assert!(wizard_due(&cfg, &home));
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn wizard_choice_writes_the_model_and_records_completion() {
        let home = home("choice");
        std::fs::create_dir_all(&home).unwrap();
        let config = home.join("config.toml");
        finish(&home, &config, Some("openai/gpt-5")).unwrap();
        assert!(std::fs::read_to_string(&config).unwrap().contains("model = \"openai/gpt-5\""));
        assert_eq!(setup_version(&home), SETUP_VERSION);
        // A second finish keeps the previous choice and stays idempotent.
        finish(&home, &config, None).unwrap();
        assert!(std::fs::read_to_string(&config).unwrap().contains("model = \"openai/gpt-5\""));
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn splash_requires_the_switch_and_a_normal_start() {
        let off = StartupCfg::default();
        let on = StartupCfg { show_splash: true, ..StartupCfg::default() };
        assert!(!splash_due(&off) && !splash_due(&StartupCfg { quiet: true, ..on.clone() }));
        assert!(splash_due(&on));
        let lines = splash_lines();
        assert!(lines[0].contains("GreenCode") && lines[0].contains(env!("CARGO_PKG_VERSION")));
    }
}
