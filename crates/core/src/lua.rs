//! Lua extensions. One VM on its own thread (the VM is not `Send`), started only
//! when an extension file exists and rebuilt whenever one changes.

use crate::ToolDef;
use mlua::LuaSerdeExt;
use serde_json::{json, Value};
use std::{
    path::{Path, PathBuf},
    sync::{mpsc, Arc, Mutex},
    time::SystemTime,
};
use tokio::sync::oneshot;

const PRELUDE: &str = r#"
gc = { _tools = {}, _commands = {}, _hooks = {} }
function gc.tool(t) gc._tools[t.name] = t end
function gc.command(name, fn) gc._commands[name] = fn end
function gc.on(ev, fn) local h = gc._hooks[ev] or {}; gc._hooks[ev] = h; h[#h + 1] = fn end
function gc._info()
  local tools, commands, hooks = {}, {}, {}
  for name, t in pairs(gc._tools) do
    tools[#tools + 1] = { name = name, description = t.description or "", schema = t.schema or { type = "object" } }
  end
  table.sort(tools, function(a, b) return a.name < b.name end)
  for name in pairs(gc._commands) do commands[#commands + 1] = name end
  table.sort(commands)
  for ev in pairs(gc._hooks) do hooks[#hooks + 1] = ev end
  return { tools = tools, commands = commands, hooks = hooks }
end
function gc._tool(name, input)
  local r = gc._tools[name].run(input)
  if r == nil then return "" end
  return tostring(r)
end
function gc._command(name, args) return gc._commands[name](args) end
-- Handlers may return a table: `deny` stops the action, other keys overwrite the payload.
function gc._hook(ev, p)
  local out = nil
  for _, fn in ipairs(gc._hooks[ev] or {}) do
    local r = fn(p)
    if type(r) == "table" then
      if r.deny then return { deny = tostring(r.deny) } end
      out = out or {}
      for k, v in pairs(r) do out[k] = v; p[k] = v end
    end
  end
  return out
end
"#;

#[derive(Default, Clone)]
struct Info {
    tools: Vec<ToolDef>,
    commands: Vec<String>,
    hooks: Vec<String>,
}

type Reply = oneshot::Sender<Result<Value, String>>;

enum Req {
    Load(Vec<PathBuf>, Reply),
    /// Calls `gc.<fn>(a, b)`.
    Call(&'static str, Value, Value, Reply),
}

#[derive(Default)]
struct Inner {
    tx: Option<mpsc::Sender<Req>>,
    stamp: Vec<(PathBuf, Option<SystemTime>)>,
    info: Info,
}

pub struct Lua {
    cwd: PathBuf,
    inner: Mutex<Inner>,
    notes: Arc<Mutex<Vec<String>>>,
}

fn trusted_file() -> PathBuf {
    crate::home().join("trusted")
}

fn lua_files(dir: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "lua"))
        .collect();
    v.sort();
    v
}

impl Lua {
    pub fn new(cwd: &Path) -> Lua {
        Lua { cwd: cwd.to_path_buf(), inner: Default::default(), notes: Default::default() }
    }

    fn project_dir(&self) -> PathBuf {
        self.cwd.join(".greencode/extensions")
    }

    fn is_trusted(&self) -> bool {
        let dir = self.project_dir();
        std::fs::read_to_string(trusted_file()).unwrap_or_default().lines().any(|l| Path::new(l) == dir)
    }

    /// The project's extension directory, if it has code the user has not approved yet.
    /// Project extensions run with the user's privileges, so a cloned repo must not load them silently.
    pub fn untrusted(&self) -> Option<PathBuf> {
        let dir = self.project_dir();
        (!lua_files(&dir).is_empty() && !self.is_trusted()).then_some(dir)
    }

    pub fn trust(&self) {
        use std::io::Write;
        let _ = std::fs::create_dir_all(crate::home());
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(trusted_file()) {
            let _ = writeln!(f, "{}", self.project_dir().display());
        }
    }

    /// (Re)loads extensions if any file changed since the last call.
    pub async fn refresh(&self) -> Result<(), String> {
        let mut files = lua_files(&crate::home().join("extensions"));
        if self.is_trusted() {
            files.extend(lua_files(&self.project_dir()));
        }
        let stamp: Vec<_> = files.iter().map(|p| (p.clone(), p.metadata().and_then(|m| m.modified()).ok())).collect();
        let rx = {
            let mut inner = self.inner.lock().unwrap();
            if inner.stamp == stamp {
                return Ok(());
            }
            inner.stamp = stamp;
            if files.is_empty() {
                *inner = Inner::default(); // dropping the sender ends the thread
                return Ok(());
            }
            let tx = inner.tx.get_or_insert_with(|| {
                let (tx, rx) = mpsc::channel();
                let notes = self.notes.clone();
                std::thread::Builder::new().name("lua".into()).spawn(move || worker(rx, notes)).expect("spawn lua thread");
                tx
            });
            let (reply, rx) = oneshot::channel();
            let _ = tx.send(Req::Load(files, reply));
            rx
        };
        let v = rx.await.map_err(|_| "lua thread died".to_string())?;
        let mut inner = self.inner.lock().unwrap();
        inner.info = Info::default();
        let v = v?;
        let strings = |k: &str| -> Vec<String> {
            v[k].as_array().into_iter().flatten().filter_map(|s| s.as_str().map(String::from)).collect()
        };
        inner.info = Info {
            tools: v["tools"].as_array().into_iter().flatten().filter_map(|t| serde_json::from_value(t.clone()).ok()).collect(),
            commands: strings("commands"),
            hooks: strings("hooks"),
        };
        Ok(())
    }

    pub fn tools(&self) -> Vec<ToolDef> {
        self.inner.lock().unwrap().info.tools.clone()
    }

    pub fn commands(&self) -> Vec<String> {
        self.inner.lock().unwrap().info.commands.clone()
    }

    pub fn has_hook(&self, ev: &str) -> bool {
        self.inner.lock().unwrap().info.hooks.iter().any(|h| h == ev)
    }

    /// Messages from `gc.notify` since the last call.
    pub fn take_notes(&self) -> Vec<String> {
        std::mem::take(&mut *self.notes.lock().unwrap())
    }

    async fn call(&self, f: &'static str, a: Value, b: Value) -> Result<Value, String> {
        let rx = {
            let inner = self.inner.lock().unwrap();
            let tx = inner.tx.as_ref().ok_or("no extensions loaded")?;
            let (reply, rx) = oneshot::channel();
            let _ = tx.send(Req::Call(f, a, b, reply));
            rx
        };
        rx.await.map_err(|_| "lua thread died".to_string())?
    }

    pub async fn tool(&self, name: &str, input: Value) -> Result<String, String> {
        Ok(self.call("_tool", json!(name), input).await?.as_str().unwrap_or("").to_string())
    }

    /// `Ok(Some(text))` is sent to the model as the prompt.
    pub async fn command(&self, name: &str, args: &str) -> Result<Option<String>, String> {
        Ok(self.call("_command", json!(name), json!(args)).await?.as_str().map(String::from))
    }

    /// Runs the handlers for `ev`. `Null` when nothing was registered or returned.
    pub async fn hook(&self, ev: &str, payload: Value) -> Value {
        if !self.has_hook(ev) {
            return Value::Null;
        }
        match self.call("_hook", json!(ev), payload).await {
            Ok(v) => v,
            Err(e) => {
                self.notes.lock().unwrap().push(format!("{ev} hook failed: {e}"));
                Value::Null
            }
        }
    }
}

fn worker(rx: mpsc::Receiver<Req>, notes: Arc<Mutex<Vec<String>>>) {
    let mut lua: Option<mlua::Lua> = None;
    for req in rx {
        match req {
            Req::Load(files, reply) => {
                let r = load(&files, &notes).and_then(|l| {
                    let info = call(&l, "_info", Value::Null, Value::Null)?;
                    lua = Some(l);
                    Ok(info)
                });
                if r.is_err() {
                    lua = None;
                }
                let _ = reply.send(r.map_err(|e| e.to_string()));
            }
            Req::Call(f, a, b, reply) => {
                let r = match &lua {
                    Some(l) => call(l, f, a, b).map_err(|e| e.to_string()),
                    None => Err("extensions failed to load".into()),
                };
                let _ = reply.send(r);
            }
        }
    }
}

fn load(files: &[PathBuf], notes: &Arc<Mutex<Vec<String>>>) -> mlua::Result<mlua::Lua> {
    let lua = mlua::Lua::new();
    lua.load(PRELUDE).set_name("prelude").exec()?;
    let gc: mlua::Table = lua.globals().get("gc")?;
    let notes = notes.clone();
    gc.set("notify", lua.create_function(move |_, text: String| {
        notes.lock().unwrap().push(text);
        Ok(())
    })?)?;
    gc.set("exec", lua.create_function(|_, cmd: String| {
        let out = std::process::Command::new("sh").arg("-c").arg(&cmd).output().map_err(mlua::Error::external)?;
        let mut s = String::from_utf8_lossy(&out.stdout).into_owned();
        s.push_str(&String::from_utf8_lossy(&out.stderr));
        Ok((s, out.status.code().unwrap_or(-1)))
    })?)?;
    gc.set("read", lua.create_function(|_, path: String| Ok(std::fs::read_to_string(path).ok()))?)?;
    drop(gc);
    for f in files {
        let src = std::fs::read_to_string(f).map_err(mlua::Error::external)?;
        lua.load(src).set_name(f.to_string_lossy()).exec()?;
    }
    Ok(lua)
}

fn call(lua: &mlua::Lua, f: &str, a: Value, b: Value) -> mlua::Result<Value> {
    let gc: mlua::Table = lua.globals().get("gc")?;
    let func: mlua::Function = gc.get(f)?;
    let r: mlua::Value = func.call((lua.to_value(&a)?, lua.to_value(&b)?))?;
    lua.from_value(r)
}
