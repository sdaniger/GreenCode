//! Todo list behavior beyond the tool itself: mirroring to an extension and the widget's auto-clear rule.
//! The list itself lives in the transcript; nothing here stores it.

use crate::config::Config;
use anyhow::{bail, Result};
use serde_json::{json, Value};
use std::time::{Duration, Instant};

/// `todo.clear_delay` as a duration; `None` keeps completed items for the session (`-1`).
pub fn clear_after(delay: i64) -> Option<Duration> {
    (delay >= 0).then(|| Duration::from_secs(delay as u64))
}

/// Items the widget still shows at `now`: every open item, plus completed ones inside the clear delay.
/// `stamp` is when the item was first seen completed; an item without one was completed just now.
pub fn visible<'a>(
    items: &'a [(String, bool, Option<Instant>)],
    clear_after: Option<Duration>,
    now: Instant,
) -> Vec<&'a str> {
    let keep = |item: &(String, bool, Option<Instant>)| match (item.1, clear_after) {
        (false, _) => true,
        (true, None) => true,
        (true, Some(delay)) => item.2.map_or(!delay.is_zero(), |at| now.saturating_duration_since(at) < delay),
    };
    items.iter().filter(|item| keep(item)).map(|item| item.0.as_str()).collect()
}

/// The extension `todo.backend` names, when set.
pub fn backend(cfg: &Config) -> Option<&str> {
    cfg.bash_opts
        .todo
        .backend
        .as_deref()
        .map(str::trim)
        .filter(|name| !name.is_empty())
}

/// Sends the list to the extension's `gc.on("todo", ...)` handler after a `todo` call.
/// A name whose extension file or handler is missing is an error the model should see once.
pub async fn mirror(lua: &crate::lua::Lua, cfg: &Config, cwd: &std::path::Path, input: &Value) -> Result<()> {
    let Some(name) = backend(cfg) else { return Ok(()) };
    let file = [crate::home().join("extensions"), cwd.join(".greencode/extensions")]
        .into_iter()
        .map(|dir| dir.join(format!("{name}.lua")))
        .find(|path| path.is_file());
    if file.is_none() {
        bail!("todo.backend is `{name}` but no extension named `{name}.lua` is installed (checked ~/.greencode/extensions and .greencode/extensions)");
    }
    if !lua.has_hook("todo") {
        bail!("todo.backend is `{name}` but it registers no `gc.on(\"todo\", ...)` handler");
    }
    lua.hook("todo", json!({"todos": input["todos"].clone(), "backend": name}))
        .await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(text: &str, done: bool, age: Option<u64>) -> (String, bool, Option<Instant>) {
        let now = Instant::now();
        (text.into(), done, age.map(|secs| now - Duration::from_secs(secs)))
    }

    #[test]
    fn completed_items_disappear_after_the_clear_delay() {
        let items = vec![item("open", false, None), item("done now", true, None), item("done 10s ago", true, Some(10))];
        let now = Instant::now();
        assert_eq!(visible(&items, Some(Duration::from_secs(5)), now), ["open", "done now"]);
        assert_eq!(visible(&items, Some(Duration::from_secs(30)), now), ["open", "done now", "done 10s ago"]);
    }

    #[test]
    fn zero_hides_completed_at_once_and_minus_one_keeps_them() {
        let items = vec![item("open", false, None), item("done", true, None)];
        assert_eq!(visible(&items, clear_after(0), Instant::now()), ["open"]);
        // -1 keeps completed items for the session.
        assert_eq!(visible(&items, clear_after(-1), Instant::now()), ["open", "done"]);
        assert_eq!(clear_after(-1), None);
        assert_eq!(clear_after(60), Some(Duration::from_secs(60)));
    }

    #[test]
    fn backend_only_reads_a_configured_name() {
        let mut cfg = Config::default();
        assert_eq!(backend(&cfg), None);
        cfg.bash_opts.todo.backend = Some("  ".into());
        assert_eq!(backend(&cfg), None);
        cfg.bash_opts.todo.backend = Some("tracker".into());
        assert_eq!(backend(&cfg), Some("tracker"));
    }
}
