//! Background computer use on macOS through a small Swift helper (`computer.swift`).

use crate::ToolDef;
use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};
use std::{
    hash::{Hash, Hasher},
    process::Stdio,
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStdin, ChildStdout, Command},
    sync::Mutex,
};

const SOURCE: &str = include_str!("computer.swift");

pub fn def() -> ToolDef {
    let (s, n) = (json!({"type": "string"}), json!({"type": "number"}));
    ToolDef {
        name: "computer".into(),
        description: "Operate macOS apps in the background; the user's cursor and focus stay put. \
`get_app_state` returns numbered elements and a screenshot; actions return the new state. \
Prefer `index`; `x`/`y` are screenshot pixels."
            .into(),
        schema: json!({"type": "object", "required": ["action"], "properties": {
            "action": {"enum": ["list_apps", "get_app_state", "click", "set_value", "type_text", "press_key", "scroll", "drag", "perform_action"]},
            "app": s, "index": {"type": "integer"}, "x": n, "y": n, "to_x": n, "to_y": n, "dx": n, "dy": n,
            "button": {"enum": ["left", "right"]}, "count": {"type": "integer"}, "text": s, "key": s, "ax_action": s,
        }}),
    }
}

struct Helper {
    _child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

/// One helper per process: it holds the element indexes of the last state.
static HELPER: Mutex<Option<Helper>> = Mutex::const_new(None);

// shortcut: the helper is compiled with swiftc on first use (needs the Xcode command line tools);
// ship a prebuilt binary in the app bundle when distributing to machines without them.
async fn start() -> Result<Helper> {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    SOURCE.hash(&mut hasher);
    let dir = crate::home().join("bin");
    let bin = dir.join(format!("computer-use-{:x}", hasher.finish()));
    if !bin.exists() {
        std::fs::create_dir_all(&dir)?;
        let src = dir.join("computer-use.swift");
        std::fs::write(&src, SOURCE)?;
        let tmp = bin.with_extension("tmp");
        let out = Command::new("swiftc")
            .arg("-O")
            .arg("-o")
            .arg(&tmp)
            .arg(&src)
            .output()
            .await
            .context("computer use needs `swiftc`; install it with `xcode-select --install`")?;
        if !out.status.success() {
            bail!(
                "cannot build the computer-use helper: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
        std::fs::rename(tmp, &bin)?;
    }
    let mut child = Command::new(&bin)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()?;
    let (stdin, stdout) = (
        child.stdin.take().unwrap(),
        BufReader::new(child.stdout.take().unwrap()),
    );
    Ok(Helper {
        _child: child,
        stdin,
        stdout,
    })
}

/// Returns the text result and, when there is one, a base64 JPEG screenshot.
pub async fn run(input: &Value) -> Result<(String, Option<String>)> {
    if !cfg!(target_os = "macos") {
        bail!("computer use is only available on macOS");
    }
    let mut guard = HELPER.lock().await;
    if guard.is_none() {
        *guard = Some(start().await?);
    }
    let helper = guard.as_mut().unwrap();
    let exchange = async {
        helper
            .stdin
            .write_all(format!("{input}\n").as_bytes())
            .await?;
        let mut line = String::new();
        helper.stdout.read_line(&mut line).await?;
        anyhow::Ok(line)
    };
    let line = match tokio::time::timeout(Duration::from_secs(60), exchange).await {
        Ok(Ok(line)) if !line.is_empty() => line,
        // A dead or stuck helper is dropped (and killed) so the next call starts a fresh one.
        other => {
            *guard = None;
            bail!(
                "the computer-use helper stopped responding{}",
                other
                    .ok()
                    .and_then(Result::err)
                    .map(|e| format!(": {e}"))
                    .unwrap_or_default()
            );
        }
    };
    let mut reply: Value = serde_json::from_str(&line)?;
    if let Some(e) = reply["error"].as_str() {
        bail!("{e}");
    }
    let image = reply["image"].take().as_str().map(String::from);
    Ok((
        reply["text"]
            .as_str()
            .ok_or_else(|| anyhow!("malformed helper reply"))?
            .to_string(),
        image,
    ))
}
