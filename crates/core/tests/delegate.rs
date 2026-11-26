use greencode_core::{config::{AcpCfg, Config}, permission::{Mode, Perms}, session::{Meta, Session}, Command, Event};
use std::{sync::Arc, time::Duration};

// An ACP parent that delegates over the MCP server GreenCode attached, then reports what woke it.
const PARENT: &str = r#"
import json, sys, urllib.request
def send(m): print(json.dumps(m), flush=True)
def say(t): send({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": "p", "update": {"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": t}}}})
server = None
for line in sys.stdin:
    m = json.loads(line)
    meth = m.get("method")
    if meth == "initialize": send({"jsonrpc": "2.0", "id": m["id"], "result": {"protocolVersion": 1, "agentCapabilities": {"mcpCapabilities": {"http": True}}}})
    elif meth == "session/new":
        server = next(s for s in m["params"]["mcpServers"] if s.get("name") == "greencode")
        send({"jsonrpc": "2.0", "id": m["id"], "result": {"sessionId": "p"}})
    elif meth == "session/prompt":
        text = "\n".join(b.get("text", "") for b in m["params"]["prompt"])
        if "delegate it" in text:
            body = json.dumps({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": "delegate_task",
                "arguments": {"task": "say hi", "title": "kid", "target": {"backend": "child"}}}}).encode()
            req = urllib.request.Request(server["url"], body, {"content-type": "application/json", "authorization": server["headers"][0]["value"]})
            res = json.loads(urllib.request.urlopen(req).read())
            say("started " + json.loads(res["result"]["content"][0]["text"])["status"])
        else:
            say("woke with: " + text)
        send({"jsonrpc": "2.0", "id": m["id"], "result": {"stopReason": "end_turn"}})
"#;

const CHILD: &str = r#"
import json, sys
def send(m): print(json.dumps(m), flush=True)
for line in sys.stdin:
    m = json.loads(line)
    meth = m.get("method")
    if meth == "initialize": send({"jsonrpc": "2.0", "id": m["id"], "result": {"protocolVersion": 1}})
    elif meth == "session/new":
        assert not any(s.get("name") == "greencode" for s in m["params"]["mcpServers"]), "children cannot delegate"
        send({"jsonrpc": "2.0", "id": m["id"], "result": {"sessionId": "c"}})
    elif meth == "session/prompt":
        send({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": "c", "update": {"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "child result"}}}})
        send({"jsonrpc": "2.0", "id": m["id"], "result": {"stopReason": "end_turn"}})
"#;

#[tokio::test(flavor = "multi_thread")]
async fn acp_parent_delegates_to_another_backend_and_is_woken_with_the_result() {
    if std::process::Command::new("python3").arg("--version").output().is_err() {
        return;
    }
    let home = std::env::temp_dir().join(format!("greencode-delegate-{}", std::process::id()));
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
    let parent = meta.id.clone();
    let (cmd, mut rx) = greencode_core::spawn(&tokio::runtime::Handle::current(), Arc::new(cfg), meta, Arc::new(Perms::new(Mode::Full)));
    cmd.send(Command::Prompt("delegate it".into())).unwrap();
    let (mut text, mut rows, mut wake, mut dones) = (String::new(), vec![], None, 0);
    while dones < 2 {
        match tokio::time::timeout(Duration::from_secs(20), rx.recv()).await.expect("event").expect("open") {
            Event::Text(t) => text.push_str(&t),
            Event::Error(e) => panic!("{e}"),
            Event::Remote { command, data } if command == "subagent" => rows.push(data),
            Event::Remote { command, data } if command == "delegate_wake" => wake = data["text"].as_str().map(str::to_string),
            Event::Done => dones += 1,
            _ => {}
        }
    }
    assert!(text.starts_with("started running"), "{text}");
    let wake = wake.expect("the finished child woke the idle parent");
    assert!(wake.contains("### kid (child · default") && wake.contains("completed\nchild result"), "{wake}");
    assert!(text.contains("woke with: [GreenCode] Delegated task results"), "{text}");
    let child = rows[0]["task"].as_str().unwrap();
    assert!(rows.iter().any(|r| r["state"] == "done" && r["detail"] == "child result"), "{rows:?}");
    let (child_meta, messages) = Session::load(child).unwrap();
    assert_eq!((child_meta.parent.as_deref(), child_meta.backend.as_str(), messages.len()), (Some(parent.as_str()), "child", 2));
}
