//! `eval`: a long-lived Python or JS interpreter per project whose variables survive between calls.
//! A kernel is a small driver script speaking one JSON line each way over stdin/stdout.

use super::str_arg;
use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    process::Stdio,
    sync::{Arc, LazyLock},
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStdin, ChildStdout},
};

/// Each call has this long; a hung kernel is killed, since it cannot be interrupted in place.
const TIMEOUT: Duration = Duration::from_secs(120);

// fd 1 carries the protocol, so it is moved aside first and the kernel's own fd 1 points at /dev/null: a child process printing to it cannot corrupt a reply.
const PY_DRIVER: &str = r#"import contextlib, io, json, os, sys, traceback
proto = os.fdopen(os.dup(1), "w")
os.dup2(os.open(os.devnull, os.O_WRONLY), 1)
env = {"__name__": "__main__"}
for line in sys.stdin:
    code = json.loads(line)["code"]
    buf = io.StringIO()
    with contextlib.redirect_stdout(buf), contextlib.redirect_stderr(buf):
        try:
            try:
                compiled, single = compile(code, "<eval>", "eval"), True
            except SyntaxError:
                compiled, single = compile(code, "<eval>", "exec"), False
            value = eval(compiled, env)
            if single and value is not None:
                print(repr(value))
        except BaseException:
            traceback.print_exc()
    proto.write(json.dumps({"out": buf.getvalue()}) + "\n")
    proto.flush()
"#;

const JS_DRIVER: &str = r#"const vm = require("vm");
const util = require("util");
const readline = require("readline");
const write = process.stdout.write.bind(process.stdout);
let buf = "";
const say = (...a) => { buf += a.map((x) => (typeof x === "string" ? x : util.inspect(x))).join(" ") + "\n"; };
const ctx = vm.createContext({
  console: { log: say, info: say, error: say, warn: say, debug: say },
  require, setTimeout, clearTimeout, setInterval, clearInterval, Buffer, URL, fetch,
  process: { env: process.env, cwd: () => process.cwd() },
});
(async () => {
  for await (const line of readline.createInterface({ input: process.stdin })) {
    const { code } = JSON.parse(line);
    buf = "";
    try {
      let value = vm.runInContext(code, ctx, { filename: "eval" });
      if (value && typeof value.then === "function") value = await value;
      if (value !== undefined) say(value);
    } catch (e) {
      buf += (e && e.stack ? e.stack : String(e)) + "\n";
    }
    write(JSON.stringify({ out: buf }) + "\n");
  }
})();
"#;

struct Kernel {
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    _child: Child,
}

type Key = (PathBuf, String);
static KERNELS: LazyLock<tokio::sync::Mutex<HashMap<Key, Arc<tokio::sync::Mutex<Kernel>>>>> =
    LazyLock::new(Default::default);

fn quoted(path: &Path) -> String {
    format!("'{}'", path.to_string_lossy().replace('\'', "'\\''"))
}

fn spawn(lang: &str, cwd: &Path) -> Result<Kernel> {
    let (interpreter, driver, file) = match lang {
        "py" => (
            format!(
                "{} -u",
                quoted(
                    &crate::config::executable("python3")
                        .ok_or_else(|| anyhow!("eval: python3 not found"))?
                )
            ),
            PY_DRIVER,
            "eval-kernel.py",
        ),
        "js" => (
            quoted(
                &crate::config::executable("node")
                    .or_else(|| crate::config::executable("bun"))
                    .ok_or_else(|| anyhow!("eval: node or bun not found"))?,
            ),
            JS_DRIVER,
            "eval-kernel.js",
        ),
        other => bail!("eval: lang must be py or js, got `{other}`"),
    };
    let dir = crate::home().join("eval");
    std::fs::create_dir_all(&dir)?;
    let script = dir.join(file);
    std::fs::write(&script, driver).with_context(|| script.display().to_string())?;
    // Through `shell`, so the `sandbox` setting limits a kernel exactly as it limits `bash`.
    let mut child = super::shell(&format!("exec {interpreter} {}", quoted(&script)), cwd)?
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()?;
    let (stdin, stdout) = (
        child.stdin.take().expect("piped"),
        child.stdout.take().expect("piped"),
    );
    Ok(Kernel {
        stdin,
        stdout: BufReader::new(stdout),
        _child: child,
    })
}

async fn exchange(kernel: &mut Kernel, code: &str) -> Result<String> {
    kernel
        .stdin
        .write_all(format!("{}\n", json!({"code": code})).as_bytes())
        .await?;
    kernel.stdin.flush().await?;
    let mut line = String::new();
    if kernel.stdout.read_line(&mut line).await? == 0 {
        bail!("kernel exited");
    }
    let reply: Value = serde_json::from_str(&line)?;
    Ok(reply["out"].as_str().unwrap_or("").to_string())
}

pub async fn run(args: &Value, cwd: &Path) -> Result<String> {
    let (lang, code) = (str_arg(args, "lang")?, str_arg(args, "code")?);
    let key: Key = (
        std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf()),
        lang.to_string(),
    );
    let kernel = {
        let mut all = KERNELS.lock().await;
        if args["reset"] == true {
            all.remove(&key);
        }
        match all.get(&key) {
            Some(k) => k.clone(),
            None => {
                let k = Arc::new(tokio::sync::Mutex::new(spawn(lang, cwd)?));
                all.insert(key.clone(), k.clone());
                k
            }
        }
    };
    let result = {
        let mut guard = kernel.lock().await;
        tokio::time::timeout(TIMEOUT, exchange(&mut guard, code)).await
    };
    let forget = || async {
        let mut all = KERNELS.lock().await;
        if all.get(&key).is_some_and(|k| Arc::ptr_eq(k, &kernel)) {
            all.remove(&key);
        }
    };
    match result {
        Err(_) => {
            forget().await;
            bail!(
                "eval timed out after {}s; the kernel was restarted and its state is lost",
                TIMEOUT.as_secs()
            );
        }
        Ok(Err(_)) => {
            forget().await;
            bail!("eval kernel exited; state is lost, run again");
        }
        Ok(Ok(out)) => Ok(if out.trim().is_empty() {
            "(no output)".into()
        } else {
            out
        }),
    }
}
