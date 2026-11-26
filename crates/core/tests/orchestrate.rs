use greencode_core::{config::{AcpCfg, Config}, permission::{Mode, Perms}, session::Meta, Command, Event};
use serde_json::{json, Value};
use std::{sync::Arc, time::Duration};

// An ACP parent that drives the orchestration tools over the MCP server GreenCode attached and reports each result.
const PARENT: &str = r#"
import json, sys, urllib.request
def send(m): print(json.dumps(m), flush=True)
server = None
def call(name, args):
    body = json.dumps({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": name, "arguments": args}}).encode()
    req = urllib.request.Request(server["url"], body, {"content-type": "application/json", "authorization": server["headers"][0]["value"]})
    res = json.loads(urllib.request.urlopen(req).read())["result"]
    text = res["content"][0]["text"]
    return {"error": text} if res.get("isError") else json.loads(text)
def listed():
    body = json.dumps({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}).encode()
    req = urllib.request.Request(server["url"], body, {"content-type": "application/json", "authorization": server["headers"][0]["value"]})
    return [t["name"] for t in json.loads(urllib.request.urlopen(req).read())["result"]["tools"]]
for line in sys.stdin:
    m = json.loads(line)
    meth = m.get("method")
    if meth == "initialize": send({"jsonrpc": "2.0", "id": m["id"], "result": {"protocolVersion": 1, "agentCapabilities": {"mcpCapabilities": {"http": True}}}})
    elif meth == "session/new":
        server = next(s for s in m["params"]["mcpServers"] if s.get("name") == "greencode")
        send({"jsonrpc": "2.0", "id": m["id"], "result": {"sessionId": "p"}})
    elif meth == "session/prompt":
        out = {"tools": listed()}
        created = call("create_threads", {"threads": [{"prompt": "hello", "title": "side", "target": {"backend": "child"}}]})
        tid = created["threads"][0]["threadId"]
        out["wait1"] = call("thread_wait", {"threadId": tid})
        out["rename"] = call("thread_update", {"threadId": tid, "title": "renamed"})
        out["list"] = call("thread_list", {})
        out["send"] = call("thread_send", {"threadId": tid, "message": "again"})
        out["wait2"] = call("thread_wait", {"threadId": tid})
        out["broader"] = call("delegate_task", {"task": "x", "target": {"backend": "child", "mode": "full"}})
        out["schedule"] = call("schedule_task", {"prompt": "nightly", "schedule": {"type": "interval", "everyMs": 3600000}})
        out["secret"] = call("request_secret", {"name": "API_TOKEN", "reason": "deploy"})
        send({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": "p", "update": {"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": json.dumps(out)}}}})
        send({"jsonrpc": "2.0", "id": m["id"], "result": {"stopReason": "end_turn"}})
"#;

// Answers each prompt with what it was asked; resumable, so a later `thread_send` reopens the session.
const CHILD: &str = r#"
import json, sys
def send(m): print(json.dumps(m), flush=True)
for line in sys.stdin:
    m = json.loads(line)
    meth = m.get("method")
    if meth == "initialize": send({"jsonrpc": "2.0", "id": m["id"], "result": {"protocolVersion": 1, "agentCapabilities": {"loadSession": True}}})
    elif meth in ("session/new", "session/load"): send({"jsonrpc": "2.0", "id": m["id"], "result": {"sessionId": "c"}})
    elif meth == "session/prompt":
        text = m["params"]["prompt"][-1]["text"].splitlines()[-1]
        send({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": "c", "update": {"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "child saw " + text}}}})
        send({"jsonrpc": "2.0", "id": m["id"], "result": {"stopReason": "end_turn"}})
"#;

#[tokio::test(flavor = "multi_thread")]
async fn acp_parent_creates_messages_and_manages_threads_through_greencode() {
    if std::process::Command::new("python3").arg("--version").output().is_err() {
        return;
    }
    let home = std::env::temp_dir().join(format!("greencode-orchestrate-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&home);
    let cwd = home.join("cwd");
    std::fs::create_dir_all(&cwd).unwrap();
    std::env::set_var("GREENCODE_HOME", &home);
    std::fs::write(cwd.join("parent.py"), PARENT).unwrap();
    std::fs::write(cwd.join("child.py"), CHILD).unwrap();
    let mut cfg = Config::default();
    for name in ["parent", "child"] {
        cfg.backends.push(AcpCfg { name: name.into(), command: vec!["python3".into(), format!("{name}.py")], env: Default::default(), protocol: Default::default() });
    }
    let meta = Meta::new(cwd, "default".into(), "parent".into());
    let (cmd, mut rx) = greencode_core::spawn(&tokio::runtime::Handle::current(), Arc::new(cfg), meta, Arc::new(Perms::new(Mode::AutoEdit)));
    cmd.send(Command::Prompt("go".into())).unwrap();
    let (mut text, mut opened, mut schedule) = (String::new(), vec![], None);
    loop {
        match tokio::time::timeout(Duration::from_secs(30), rx.recv()).await.expect("event").expect("open") {
            Event::Text(t) => text.push_str(&t),
            Event::Error(e) => panic!("{e}"),
            Event::Remote { command, data } if command == "thread_opened" => opened.push(data["meta"]["id"].clone()),
            Event::Dialog { request, reply } if request["method"] == "schedule" => {
                schedule = Some(request);
                let _ = reply.send(json!({"scheduledTaskId": "s1", "enabled": true}));
            }
            Event::Dialog { request, reply } if request["method"] == "secret" => {
                assert!(request["title"].as_str().unwrap().contains("API_TOKEN"), "{request}");
                let _ = reply.send(json!({"value": "s3cret"}));
            }
            Event::Done { .. } => break,
            _ => {}
        }
    }
    let out: Value = serde_json::from_str(&text).unwrap_or_else(|_| panic!("{text}"));
    for tool in ["create_threads", "thread_send", "thread_wait", "thread_list", "schedule_task", "request_secret", "environment_link"] {
        assert!(out["tools"].as_array().unwrap().iter().any(|t| t == tool), "{tool} missing: {}", out["tools"]);
    }
    let tid = out["wait1"]["threadId"].clone();
    // Announced on creation and again when `thread_send` runs it, so the UI follows both turns.
    assert_eq!(opened, vec![tid.clone(), tid.clone()]);
    assert_eq!((out["wait1"]["status"].as_str(), out["wait1"]["lastReply"].as_str()), (Some("idle"), Some("child saw hello")), "{out}");
    assert_eq!(out["rename"]["title"], "renamed");
    let row = out["list"]["threads"].as_array().unwrap().iter().find(|t| t["threadId"] == tid).unwrap_or_else(|| panic!("{}", out["list"]));
    assert_eq!(row["title"], "renamed");
    assert_eq!(out["send"]["delivered"], "started", "{out}");
    let reply = out["wait2"]["lastReply"].as_str().unwrap();
    assert!(reply == "child saw again", "the message reached the thread as a turn of its own: {reply}");
    assert!(out["broader"]["error"].as_str().unwrap().contains("broader"), "{}", out["broader"]);
    let schedule = schedule.expect("schedule request reached the UI");
    assert_eq!((schedule["action"].as_str(), schedule["prompt"].as_str(), schedule["schedule"]["everyMs"].as_u64()), (Some("save"), Some("nightly"), Some(3_600_000)));
    assert_eq!(out["schedule"]["scheduledTaskId"], "s1");
    assert_eq!(out["secret"]["status"], "provided");
    assert_eq!(std::fs::read_to_string(out["secret"]["file"].as_str().unwrap()).unwrap(), "s3cret");
}
