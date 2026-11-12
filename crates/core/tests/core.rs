use greencode_core::{
    agent::Agent,
    config::Config,
    context,
    permission::{Mode, Perms},
    provider::{anthropic, openai, Provider, Reply, Request},
    session::{Meta, Session},
    tools, Backend, Block, BoxFut, Decision, Event, Message, Role,
};
use serde_json::{json, Value};
use std::{
    collections::VecDeque,
    path::PathBuf,
    sync::{Arc, Mutex, OnceLock},
};

/// One shared GREENCODE_HOME (env is process-wide), one project dir per test.
fn dir(name: &str) -> PathBuf {
    static HOME: OnceLock<PathBuf> = OnceLock::new();
    let home = HOME.get_or_init(|| {
        let h = std::env::temp_dir().join(format!("greencode-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&h);
        std::fs::create_dir_all(h.join("extensions")).unwrap();
        // Loaded by every agent in this process; only reacts to its own markers.
        std::fs::write(
            h.join("extensions/test.lua"),
            r#"
gc.tool{ name = "shout", description = "upper-case", schema = { type = "object", properties = { s = { type = "string" } } },
  run = function(i) return string.upper(i.s) end }
gc.on("tool_call", function(c)
  if c.tool == "bash" and c.input.command:find("forbidden") then return { deny = "nope" } end
end)
gc.command("hi", function(args) return "hello " .. args end)
"#,
        )
        .unwrap();
        std::env::set_var("GREENCODE_HOME", &h);
        h
    });
    let d = home.join("projects").join(name);
    std::fs::create_dir_all(&d).unwrap();
    d
}

#[derive(Default)]
struct Mock {
    replies: Mutex<VecDeque<Reply>>,
    /// (system + tools, messages) as sent, serialized.
    seen: Mutex<Vec<(String, String)>>,
}

impl Mock {
    fn new(replies: Vec<Vec<Block>>) -> Arc<Mock> {
        let replies = replies.into_iter().map(|blocks| Reply { blocks, ..Default::default() }).collect();
        Arc::new(Mock { replies: Mutex::new(replies), ..Default::default() })
    }
}

impl Provider for Mock {
    fn complete<'a>(&'a self, req: Request<'a>, on_text: &'a (dyn Fn(&str) + Send + Sync)) -> BoxFut<'a, anyhow::Result<Reply>> {
        Box::pin(async move {
            let prefix = format!("{}{}", req.system, serde_json::to_string(req.tools).unwrap());
            self.seen.lock().unwrap().push((prefix, serde_json::to_string(req.messages).unwrap()));
            let reply = self.replies.lock().unwrap().pop_front().expect("mock ran out of replies");
            for b in &reply.blocks {
                if let Block::Text { text } = b {
                    on_text(text);
                }
            }
            Ok(reply)
        })
    }
}

fn text(s: &str) -> Block {
    Block::Text { text: s.into() }
}
fn call(id: &str, name: &str, input: Value) -> Block {
    Block::ToolUse { id: id.into(), name: name.into(), input }
}

fn agent(name: &str, mode: Mode, mock: &Arc<Mock>) -> (Agent, Meta) {
    let meta = Meta::new(dir(name), "mock/m".into(), "native".into());
    let a = Agent::with_provider(Arc::new(Config::default()), &meta, Arc::new(Perms::new(mode)), mock.clone(), "m".into());
    (a, meta)
}

/// Runs a prompt, answering every permission request with `decision`.
async fn run(a: &mut Agent, prompt: &str, decision: Decision) -> (anyhow::Result<()>, Vec<Event>) {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let fut = a.prompt(prompt.into(), &tx);
    tokio::pin!(fut);
    let mut events = Vec::new();
    let result = loop {
        tokio::select! {
            r = &mut fut => break r,
            Some(ev) = rx.recv() => match ev {
                Event::Permission { reply, tool, summary } => {
                    let _ = reply.send(decision);
                    let (reply, _) = tokio::sync::oneshot::channel();
                    events.push(Event::Permission { reply, tool, summary });
                }
                ev => events.push(ev),
            }
        }
    };
    while let Ok(ev) = rx.try_recv() {
        events.push(ev);
    }
    (result, events)
}

fn results(events: &[Event]) -> Vec<(String, bool)> {
    events.iter().filter_map(|e| if let Event::ToolEnd { output, is_error, .. } = e { Some((output.clone(), *is_error)) } else { None }).collect()
}

#[test]
fn anthropic_stream() {
    let mut acc = anthropic::Acc::default();
    let mut text = String::new();
    for ev in [
        json!({"type":"message_start","message":{"usage":{"input_tokens":5,"cache_read_input_tokens":100,"cache_creation_input_tokens":7}}}),
        json!({"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"hm"}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"sig"}}),
        json!({"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}),
        json!({"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"Hel"}}),
        json!({"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"lo"}}),
        json!({"type":"content_block_start","index":2,"content_block":{"type":"tool_use","id":"t1","name":"read","input":{}}}),
        json!({"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"{\"path\":"}}),
        json!({"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"\"a.txt\"}"}}),
        json!({"type":"content_block_start","index":3,"content_block":{"type":"tool_use","id":"t2","name":"read","input":{}}}),
        json!({"type":"content_block_delta","index":3,"delta":{"type":"input_json_delta","partial_json":"{\"path\":\"cut"}}),
        json!({"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":9}}),
    ] {
        if let Some(t) = acc.feed(&ev).unwrap() {
            text.push_str(&t);
        }
    }
    let r = acc.finish();
    assert_eq!(text, "Hello");
    assert_eq!(r.stop, "tool_use");
    assert_eq!((r.usage.input, r.usage.cache_read, r.usage.cache_write, r.usage.output), (5, 100, 7, 9));
    // Thinking is kept verbatim so it can be replayed unchanged.
    assert_eq!(serde_json::to_value(&r.blocks[0]).unwrap(), json!({"type":"thinking","thinking":"hm","signature":"sig"}));
    assert!(matches!(&r.blocks[1], Block::Text { text } if text == "Hello"));
    assert!(matches!(&r.blocks[2], Block::ToolUse { input, .. } if input["path"] == "a.txt"));
    assert!(matches!(&r.blocks[3], Block::ToolUse { input, .. } if input.get("__invalid_json").is_some()));
    assert!(acc_error().is_err());
}

fn acc_error() -> anyhow::Result<Option<String>> {
    anthropic::Acc::default().feed(&json!({"type":"error","error":{"type":"overloaded_error"}}))
}

#[test]
fn openai_stream_and_conversion() {
    let mut acc = openai::Acc::default();
    for ev in [
        json!({"choices":[{"delta":{"content":"Hi"}}]}),
        json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c1","function":{"name":"bash","arguments":"{\"comm"}}]}}]}),
        json!({"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"and\":\"ls\"}"}}]},"finish_reason":"tool_calls"}]}),
        json!({"choices":[],"usage":{"prompt_tokens":50,"completion_tokens":4,"prompt_tokens_details":{"cached_tokens":30}}}),
    ] {
        acc.feed(&ev).unwrap();
    }
    let r = acc.finish();
    assert_eq!(r.stop, "tool_use");
    assert_eq!((r.usage.input, r.usage.cache_read, r.usage.output), (20, 30, 4));
    assert!(matches!(&r.blocks[1], Block::ToolUse { id, name, input } if id == "c1" && name == "bash" && input["command"] == "ls"));

    let history = vec![
        Message::user("list"),
        Message { role: Role::Assistant, content: r.blocks },
        Message { role: Role::User, content: vec![Block::ToolResult { tool_use_id: "c1".into(), content: "a\nb".into(), is_error: false }] },
    ];
    let out = openai::convert("sys", &history);
    assert_eq!(out[0], json!({"role":"system","content":"sys"}));
    assert_eq!(out[1], json!({"role":"user","content":"list"}));
    assert_eq!(out[2]["tool_calls"][0]["function"]["arguments"], "{\"command\":\"ls\"}");
    assert_eq!(out[3], json!({"role":"tool","tool_call_id":"c1","content":"a\nb"}));
    assert_eq!(out.len(), 4);
}

#[tokio::test]
async fn file_tools() {
    let d = dir("file_tools");
    tools::run("write", &json!({"path": "a.txt", "content": "one\ntwo\nthree\ntwo\n"}), &d).await.unwrap();
    assert_eq!(tools::run("read", &json!({"path": "a.txt", "offset": 2, "limit": 2}), &d).await.unwrap(), "two\nthree");
    // Ambiguous and missing matches must not write.
    assert!(tools::run("edit", &json!({"path": "a.txt", "old_string": "two", "new_string": "2"}), &d).await.is_err());
    assert!(tools::run("edit", &json!({"path": "a.txt", "old_string": "nine", "new_string": "9"}), &d).await.is_err());
    tools::run("edit", &json!({"path": "a.txt", "old_string": "three", "new_string": "3"}), &d).await.unwrap();
    tools::run("edit", &json!({"path": "a.txt", "old_string": "two", "new_string": "2", "replace_all": true}), &d).await.unwrap();
    assert_eq!(std::fs::read_to_string(d.join("a.txt")).unwrap(), "one\n2\n3\n2\n");
    assert!(tools::run("bash", &json!({"command": "echo hi; exit 3"}), &d).await.unwrap_err().to_string().contains("[exit 3]"));
    assert!(tools::run("bash", &json!({"command": "sleep 5", "timeout": 1}), &d).await.unwrap_err().to_string().contains("timed out"));
}

#[tokio::test]
async fn memory_tool() {
    let d = dir("memory_tool");
    let m = |v: Value| { let d = d.clone(); async move { tools::run("memory", &v, &d).await } };
    m(json!({"action": "add", "scope": "project", "text": "uses pnpm"})).await.unwrap();
    m(json!({"action": "add", "scope": "project", "text": "tests: cargo test"})).await.unwrap();
    m(json!({"action": "replace", "scope": "project", "old": "pnpm", "text": "uses bun"})).await.unwrap();
    m(json!({"action": "remove", "scope": "project", "old": "cargo"})).await.unwrap();
    assert_eq!(std::fs::read_to_string(d.join(".greencode/MEMORY.md")).unwrap(), "- uses bun\n");
    assert!(context::system_prompt(&d).contains("- uses bun"));
    let big = "x".repeat(context::MEMORY_LIMIT);
    assert!(m(json!({"action": "add", "scope": "project", "text": big})).await.is_err());
}

#[test]
fn truncation_keeps_both_ends() {
    dir("trunc");
    let long = format!("START{}END", "é".repeat(40_000));
    let out = context::truncate(long.clone(), "id/1");
    assert!(out.len() < 25_000 && out.starts_with("START") && out.ends_with("END"));
    let path = out.split("full output: ").nth(1).unwrap().split(" ...]").next().unwrap();
    assert_eq!(std::fs::read_to_string(path).unwrap(), long);
    assert_eq!(context::truncate("short".into(), "x"), "short");
}

#[test]
fn permissions() {
    let d = dir("perms");
    let ask = Perms::new(Mode::Ask);
    assert_eq!(ask.check("read", &json!({"path": "a"}), &d), None);
    assert!(ask.check("read", &json!({"path": "/etc/passwd"}), &d).is_some());
    assert_eq!(ask.check("write", &json!({"path": "a"}), &d).as_deref(), Some("write"));
    assert_eq!(ask.check("bash", &json!({"command": "cargo test"}), &d).as_deref(), Some("bash:cargo"));
    // A program-name grant must not cover a chained command.
    ask.always("bash:cargo".into());
    assert_eq!(ask.check("bash", &json!({"command": "cargo build"}), &d), None);
    assert!(ask.check("bash", &json!({"command": "cargo build; rm -rf /"}), &d).is_some());
    let auto = Perms::new(Mode::AutoEdit);
    assert_eq!(auto.check("edit", &json!({"path": "src/a.rs"}), &d), None);
    assert!(auto.check("edit", &json!({"path": "../../../outside"}), &d).is_some());
    assert!(auto.check("write", &json!({"path": "/tmp/x"}), &d).is_some());
    assert!(auto.check("bash", &json!({"command": "ls"}), &d).is_some());
    assert_eq!(Perms::new(Mode::Full).check("bash", &json!({"command": "rm x"}), &d), None);
}

#[tokio::test]
async fn loop_runs_tools_and_keeps_prefix_stable() {
    let mock = Mock::new(vec![
        vec![text("writing"), call("t1", "write", json!({"path": "hello.txt", "content": "hi"}))],
        vec![text("done")],
        vec![text("again")],
    ]);
    let (mut a, meta) = agent("loop", Mode::Full, &mock);
    let (r, events) = run(&mut a, "make hello.txt", Decision::Deny).await;
    r.unwrap();
    assert_eq!(std::fs::read_to_string(meta.cwd.join("hello.txt")).unwrap(), "hi");
    assert_eq!(results(&events), vec![("wrote 2 bytes".to_string(), false)]);
    run(&mut a, "second turn", Decision::Deny).await.0.unwrap();

    // Cache safety: system+tools never change and history only grows.
    let seen = mock.seen.lock().unwrap();
    assert_eq!(seen.len(), 3);
    for w in seen.windows(2) {
        assert_eq!(w[0].0, w[1].0);
        assert!(w[1].1.starts_with(w[0].1.trim_end_matches(']')));
    }
    drop(seen);

    // The session file restores the same history.
    let (saved, messages) = Session::load(&meta.id).unwrap();
    assert_eq!(saved.title, "make hello.txt");
    assert_eq!(serde_json::to_string(&messages).unwrap(), serde_json::to_string(&a.messages).unwrap());
    assert!(Session::list().iter().any(|m| m.id == meta.id));
}

#[tokio::test]
async fn denied_and_invalid_calls_become_error_results() {
    let mock = Mock::new(vec![
        vec![call("t1", "bash", json!({"command": "echo hi"})), call("t2", "read", json!({"__invalid_json": "{"}))],
        vec![text("ok")],
    ]);
    let (mut a, _) = agent("denied", Mode::Ask, &mock);
    let (r, events) = run(&mut a, "go", Decision::Deny).await;
    r.unwrap();
    let res = results(&events);
    assert!(res.iter().all(|(_, err)| *err));
    assert!(res.iter().any(|(o, _)| o.contains("denied")) && res.iter().any(|(o, _)| o.contains("INVALID_JSON")));
    // Both results go back in one user message.
    assert_eq!(a.messages[2].content.len(), 2);
}

#[tokio::test]
async fn interrupted_tool_calls_are_repaired() {
    let mock = Mock::new(vec![vec![text("ok")]]);
    let (mut a, _) = agent("repair", Mode::Full, &mock);
    a.messages.push(Message::user("x"));
    a.messages.push(Message { role: Role::Assistant, content: vec![call("t9", "bash", json!({"command": "sleep 100"}))] });
    run(&mut a, "next", Decision::Deny).await.0.unwrap();
    assert!(matches!(&a.messages[2].content[0], Block::ToolResult { tool_use_id, is_error: true, .. } if tool_use_id == "t9"));
}

#[tokio::test]
async fn lua_tool_hook_and_command() {
    let mock = Mock::new(vec![
        vec![call("t1", "shout", json!({"s": "abc"})), call("t2", "bash", json!({"command": "echo forbidden"}))],
        vec![text("ok")],
    ]);
    let (mut a, _) = agent("lua", Mode::Full, &mock);
    let (r, events) = run(&mut a, "/hi there", Decision::Deny).await;
    r.unwrap();
    assert!(matches!(&a.messages[0].content[0], Block::Text { text } if text == "hello there"));
    let res = results(&events);
    assert!(res.contains(&("ABC".to_string(), false)));
    assert!(res.iter().any(|(o, err)| *err && o.contains("nope")));
    assert!(mock.seen.lock().unwrap()[0].0.contains("\"shout\""));
}

#[tokio::test]
async fn subagent_returns_only_its_answer() {
    let mock = Mock::new(vec![
        vec![call("t1", "task", json!({"prompt": "look around"}))],
        vec![text("sub answer")],
        vec![text("final")],
    ]);
    let (mut a, _) = agent("sub", Mode::Full, &mock);
    let (r, events) = run(&mut a, "delegate", Decision::Deny).await;
    r.unwrap();
    assert_eq!(results(&events), vec![("sub answer".to_string(), false)]);
    let seen = mock.seen.lock().unwrap();
    assert!(seen[1].1.contains("look around") && !seen[1].1.contains("delegate"));
    assert!(!seen[1].0.contains("\"task\""));
}

#[tokio::test]
async fn compaction_replaces_history() {
    let mock = Mock::new(vec![vec![text("first")], vec![text("the summary")], vec![text("after")]]);
    let (mut a, meta) = agent("compact", Mode::Full, &mock);
    run(&mut a, "hello", Decision::Deny).await.0.unwrap();
    run(&mut a, "/compact", Decision::Deny).await.0.unwrap();
    assert_eq!(a.messages.len(), 1);
    run(&mut a, "continue", Decision::Deny).await.0.unwrap();
    let (_, loaded) = Session::load(&meta.id).unwrap();
    assert_eq!(loaded.len(), 3);
    assert!(matches!(&loaded[0].content[0], Block::Text { text } if text.contains("the summary")));
}

#[test]
fn anthropic_body_is_cacheable() {
    let a = anthropic::Anthropic { http: Default::default(), base: "https://api.anthropic.com".into(), key: String::new(), effort: None };
    let tools = tools::defs(false, &[]);
    let msgs = vec![Message::user("hi")];
    let body = a.body(&Request { model: "claude-opus-5-5", system: "s", messages: &msgs, tools: &tools, max_tokens: 10 });
    assert_eq!(body["cache_control"]["type"], "ephemeral");
    assert_eq!(body["fallbacks"], "default");
    assert_eq!(body["tools"].as_array().unwrap().len(), 6);
    assert_eq!(body["messages"][0], json!({"role":"user","content":[{"type":"text","text":"hi"}]}));
}

#[tokio::test]
async fn mcp_stdio_search_and_call() {
    if std::process::Command::new("python3").arg("--version").output().is_err() {
        return; // the fake server below needs python3
    }
    let d = dir("mcp");
    std::fs::write(
        d.join("server.py"),
        r#"
import json, sys
for line in sys.stdin:
    m = json.loads(line)
    if "id" not in m: continue
    if m["method"] == "initialize": r = {"protocolVersion": "2025-06-18", "capabilities": {}, "serverInfo": {"name": "t", "version": "1"}}
    elif m["method"] == "tools/list": r = {"tools": [
        {"name": "add", "description": "Add two numbers", "inputSchema": {"type": "object", "properties": {"a": {"type": "number"}, "b": {"type": "number"}}}},
        {"name": "weather", "description": "Forecast for a city", "inputSchema": {"type": "object"}}]}
    else:
        a = m["params"]["arguments"]
        r = {"content": [{"type": "text", "text": str(a["a"] + a["b"])}]}
    print(json.dumps({"jsonrpc": "2.0", "id": m["id"], "result": r}), flush=True)
"#,
    )
    .unwrap();
    std::fs::write(d.join(".mcp.json"), r#"{"mcpServers": {"calc": {"command": "python3", "args": ["server.py"]}}}"#).unwrap();
    let mcp = tools::mcp::Mcp::load(&d);
    assert_eq!(mcp.names(), vec!["calc"]);
    // The tool list stays out of the prompt: only the server name is in the definition.
    let def = tools::defs(false, &mcp.names()).pop().unwrap();
    assert!(def.description.contains("calc") && !def.description.contains("weather"));
    let found = mcp.run(&json!({"action": "search", "query": "add numbers"}), &d).await.unwrap();
    assert!(found.contains("calc/add") && found.contains("\"a\"") && !found.contains("weather"));
    let out = mcp.run(&json!({"action": "call", "server": "calc", "tool": "add", "args": {"a": 2, "b": 40}}), &d).await.unwrap();
    assert_eq!(out, "42");
    assert!(mcp.run(&json!({"action": "call", "server": "nope", "tool": "x"}), &d).await.is_err());
}

#[tokio::test]
async fn acp_backend_streams_asks_and_saves() {
    if std::process::Command::new("python3").arg("--version").output().is_err() {
        return; // the fake agent below needs python3
    }
    let d = dir("acp");
    std::fs::write(
        d.join("agent.py"),
        r#"
import json, sys
def send(m): print(json.dumps(m), flush=True)
def update(u): send({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": "s1", "update": u}})
for line in sys.stdin:
    m = json.loads(line)
    meth = m.get("method")
    if meth == "initialize": send({"jsonrpc": "2.0", "id": m["id"], "result": {"protocolVersion": 1}})
    elif meth == "session/new": send({"jsonrpc": "2.0", "id": m["id"], "result": {"sessionId": "s1"}})
    elif meth == "session/prompt":
        prompt_id = m["id"]
        update({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "working"}})
        update({"sessionUpdate": "tool_call", "toolCallId": "c1", "title": "Run ls", "status": "pending", "rawInput": {"command": "ls"}})
        send({"jsonrpc": "2.0", "id": 99, "method": "session/request_permission", "params": {"sessionId": "s1",
              "toolCall": {"toolCallId": "c1", "title": "Run ls"},
              "options": [{"optionId": "yes", "kind": "allow_once"}, {"optionId": "no", "kind": "reject_once"}]}})
    elif m.get("id") == 99:
        picked = m["result"]["outcome"].get("optionId")
        update({"sessionUpdate": "tool_call_update", "toolCallId": "c1", "status": "completed",
                "content": [{"type": "content", "content": {"type": "text", "text": "picked " + str(picked)}}]})
        send({"jsonrpc": "2.0", "id": prompt_id, "result": {"stopReason": "end_turn"}})
"#,
    )
    .unwrap();
    let meta = Meta::new(d.clone(), String::new(), "fake".into());
    let command = vec!["python3".to_string(), "agent.py".to_string()];
    let mut acp = greencode_core::acp::Acp::start(&command, &meta, Arc::new(Perms::new(Mode::Ask))).await.unwrap();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let events = {
        let fut = acp.prompt("list files".into(), &tx);
        tokio::pin!(fut);
        let mut events = Vec::new();
        loop {
            tokio::select! {
                r = &mut fut => { r.unwrap(); break }
                Some(ev) = rx.recv() => match ev {
                    Event::Permission { reply, tool, .. } => { assert_eq!(tool, "Run ls"); let _ = reply.send(Decision::Always); }
                    ev => events.push(ev),
                }
            }
        }
        while let Ok(ev) = rx.try_recv() {
            events.push(ev);
        }
        events
    };
    assert!(events.iter().any(|e| matches!(e, Event::Text(t) if t == "working")));
    // "Always" falls back to allow_once when the agent offers no allow_always.
    assert_eq!(results(&events), vec![("picked yes".to_string(), false)]);
    let (saved, messages) = Session::load(&meta.id).unwrap();
    assert_eq!((saved.title.as_str(), messages.len()), ("list files", 2));
}

#[test]
fn worktree_isolates_a_thread_and_diff_shows_new_files() {
    let d = dir("wt");
    let git = |args: &[&str]| {
        let ok = std::process::Command::new("git").arg("-C").arg(&d).args(["-c", "user.name=t", "-c", "user.email=t@t"]).args(args).output().unwrap().status.success();
        assert!(ok, "git {args:?}");
    };
    git(&["init", "-q"]);
    std::fs::write(d.join("a.txt"), "one\n").unwrap();
    git(&["add", "."]);
    git(&["commit", "-qm", "init"]);
    let wt = greencode_core::worktree::create(&d, "t1").unwrap();
    assert_eq!(std::fs::read_to_string(wt.join("a.txt")).unwrap(), "one\n");
    std::fs::write(wt.join("a.txt"), "two\n").unwrap();
    std::fs::write(wt.join("new.txt"), "fresh\n").unwrap();
    let diff = greencode_core::worktree::diff(&wt);
    assert!(diff.contains("-one") && diff.contains("+two") && diff.contains("+fresh"));
    // The original checkout is untouched.
    assert_eq!(greencode_core::worktree::diff(&d), "");
}
