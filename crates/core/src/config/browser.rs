//! Settings for the native `browser` tool: Chromium over CDP, the Tern/WebKit host and cmux surfaces; flattened into `Config`.
use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Deserialize, Serialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct BrowserCfg {
    /// HTTP CDP discovery endpoint (`http://127.0.0.1:9222`) to attach to instead of launching a browser.
    pub cdp_url: Option<String>,
    /// Drive the user's own Chrome through the browser relay; takes precedence over `cdp_url`.
    pub relay: bool,
    /// Relay endpoint; unset means `http://127.0.0.1:9224`.
    pub relay_url: Option<String>,
    /// Launch without a window.
    pub headless: bool,
    /// Use cmux WKWebView surfaces when `CMUX_SOCKET_PATH` is set.
    pub cmux: bool,
    /// Open picture-in-picture tabs in a Tern pane when `TERN_PANE_SOCKET` and `TERN_PANE` are set.
    pub tern: bool,
    /// Prefer the ABKit WebKit host over managed Chromium when it is installed.
    pub webkit: bool,
    /// The WebKit host executable, or an ABKit engine directory holding it as `ABHost`.
    pub webkit_path: Option<String>,
    /// Freeze native headless Chromium tabs when a turn settles.
    pub freeze_on_turn_end: bool,
    /// Close native headless tabs idle this many seconds; 0 never closes them.
    pub idle_close_sec: u64,
    /// Directory screenshots are saved to; unset means a temp file.
    pub screenshot_dir: Option<String>,
}

impl Default for BrowserCfg {
    fn default() -> Self {
        Self {
            cdp_url: None,
            relay: false,
            relay_url: None,
            headless: true,
            cmux: true,
            tern: true,
            webkit: true,
            webkit_path: None,
            freeze_on_turn_end: true,
            idle_close_sec: 1800,
            screenshot_dir: None,
        }
    }
}

impl BrowserCfg {
    /// `~` and `~/...` expanded against the user's home; every other path as written.
    pub fn expand(path: &str) -> std::path::PathBuf {
        match path.strip_prefix("~/") {
            Some(rest) => crate::user_home().join(rest),
            None if path == "~" => crate::user_home(),
            None => std::path::PathBuf::from(path),
        }
    }
}

/// OMP key → native dotted key; `config import-omp` copies the value verbatim unless `convert` handles it.
pub(super) const OMP: &[(&str, &str)] = &[
    ("browser.cdpUrl", "browser.cdp_url"),
    ("browser.relay", "browser.relay"),
    ("browser.relayUrl", "browser.relay_url"),
    ("browser.headless", "browser.headless"),
    ("browser.cmux", "browser.cmux"),
    ("browser.tern", "browser.tern"),
    ("browser.webkit", "browser.webkit"),
    ("browser.webkitPath", "browser.webkit_path"),
    ("browser.freezeOnTurnEnd", "browser.freeze_on_turn_end"),
    ("browser.idleCloseSec", "browser.idle_close_sec"),
    ("browser.screenshotDir", "browser.screenshot_dir"),
];

/// Translates an OMP value whose shape or units differ from the native field; `None` copies it verbatim.
pub(super) fn convert(_from: &str, _value: &Value) -> Option<Result<toml::Value>> {
    None
}

#[cfg(test)]
mod tests {
    use crate::config::Config;

    /// Every row above survives YAML → TOML → `Config` under its native key.
    #[test]
    fn omp_rows_import() {
        let yaml = "\
browser:
  cdpUrl: http://127.0.0.1:9222
  relay: true
  relayUrl: http://relay.lan:9224
  headless: false
  cmux: false
  tern: false
  webkit: false
  webkitPath: ~/.greencode/lib/abkit
  freezeOnTurnEnd: false
  idleCloseSec: 900
  screenshotDir: ~/Downloads
";
        let table = crate::config::settings::omp::translate(yaml).unwrap();
        let cfg = crate::config::settings::decode(toml::Value::Table(table)).unwrap();
        let b = &cfg.integration_opts.browser;
        assert_eq!(b.cdp_url.as_deref(), Some("http://127.0.0.1:9222"));
        assert!(b.relay);
        assert_eq!(b.relay_url.as_deref(), Some("http://relay.lan:9224"));
        assert!(!b.headless && !b.cmux && !b.tern && !b.webkit);
        assert_eq!(b.webkit_path.as_deref(), Some("~/.greencode/lib/abkit"));
        assert!(!b.freeze_on_turn_end);
        assert_eq!(b.idle_close_sec, 900);
        assert_eq!(b.screenshot_dir.as_deref(), Some("~/Downloads"));
        let default = Config::default().integration_opts.browser;
        assert_eq!((default.headless, default.relay, default.idle_close_sec), (true, false, 1800));
        assert!(default.cmux && default.tern && default.webkit && default.freeze_on_turn_end);
        assert_eq!((default.cdp_url, default.screenshot_dir), (None, None));
        assert_eq!(super::BrowserCfg::expand("~/Downloads"), crate::user_home().join("Downloads"));
    }
}
