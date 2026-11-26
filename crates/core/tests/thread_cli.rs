use serde_json::Value;
use std::{io::Write, process::{Command, Stdio}};
use tokio::{io::{AsyncReadExt, AsyncWriteExt}, net::TcpListener};

fn sse(text: &str) -> String {
    format!(
        "data: {{\"type\":\"content_block_start\",\"index\":0,\"content_block\":{{\"type\":\"text\",\"text\":\"\"}}}}\n\n\
         data: {{\"type\":\"content_block_delta\",\"index\":0,\"delta\":{{\"type\":\"text_delta\",\"text\":\"{text}\"}}}}\n\n\
         data: {{\"type\":\"message_delta\",\"delta\":{{\"stop_reason\":\"end_turn\"}},\"usage\":{{\"output_tokens\":1}}}}\n\n"
    )
}

/// Accepts one fake Anthropic request per queued reply.
fn fake_provider(listener: TcpListener, replies: Vec<&'static str>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        for reply in replies {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = Vec::new();
            let mut chunk = [0; 4096];
            loop {
                let n = socket.read(&mut chunk).await.unwrap();
                assert!(n > 0);
                buf.extend_from_slice(&chunk[..n]);
                if let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                    let head = String::from_utf8_lossy(&buf[..end]).to_lowercase();
                    let len: usize = head.lines().find_map(|line| line.strip_prefix("content-length:")).unwrap().trim().parse().unwrap();
                    if buf.len() >= end + 4 + len {
                        break;
                    }
                }
            }
            socket.write_all(format!("HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n{}", sse(reply)).as_bytes()).await.unwrap();
        }
    })
}

#[tokio::test(flavor = "multi_thread")]
async fn thread_cli_starts_lists_shows_and_sends_with_json() {
    let root = std::env::temp_dir().join(format!("greencode-thread-cli-{}", std::process::id()));
    let home = root.join("home");
    let project = root.join("project");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&project).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let provider = fake_provider(listener, vec!["first reply", "second reply"]);
    std::fs::write(
        home.join("config.toml"),
        format!("model = \"local/test\"\n[providers.local]\nkind = \"anthropic\"\nbase_url = \"http://{addr}\"\napi_key = \"test\"\n"),
    ).unwrap();
    let binary = env!("CARGO_BIN_EXE_greencode-cli");
    let run = |args: &[&str]| {
        Command::new(binary).args(args)
            .env("GREENCODE_HOME", &home)
            .env("GREENCODE_NO_LAUNCHCTL", "1")
            .current_dir(&project)
            .output()
            .unwrap()
    };

    let started = run(&["thread", "start", "--project", project.to_str().unwrap(), "--backend", "native", "--model", "local/test", "--wait", "--json", "hello thread"]);
    assert!(started.status.success(), "{}", String::from_utf8_lossy(&started.stderr));
    let start: Value = serde_json::from_slice(&started.stdout).unwrap_or_else(|e| panic!("{e}: {}", String::from_utf8_lossy(&started.stdout)));
    let id = start["id"].as_str().unwrap().to_string();
    assert_eq!(start["running"], false);
    assert_eq!(start["text"], "first reply");

    let listed = run(&["thread", "list", "--json"]);
    assert!(listed.status.success(), "{}", String::from_utf8_lossy(&listed.stderr));
    let list: Value = serde_json::from_slice(&listed.stdout).unwrap();
    let thread = list.as_array().unwrap().iter().find(|m| m["id"] == id.as_str()).expect("started thread is listed");
    assert_eq!(thread["title"], "hello thread");

    let shown = run(&["thread", "show", &id, "--json"]);
    assert!(shown.status.success(), "{}", String::from_utf8_lossy(&shown.stderr));
    let shown: Value = serde_json::from_slice(&shown.stdout).unwrap();
    assert_eq!(shown["meta"]["id"], id.as_str());
    let messages = shown["messages"].as_array().unwrap();
    assert!(messages.iter().any(|m| m["role"] == "user" && m["content"][0]["text"] == "hello thread"), "{shown}");
    assert!(messages.iter().any(|m| m["role"] == "assistant" && m["content"][0]["text"] == "first reply"), "{shown}");

    let mut child = Command::new(binary)
        .args(["thread", "send", &id, "--wait", "--json", "-"])
        .env("GREENCODE_HOME", &home)
        .current_dir(&project)
        .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped())
        .spawn().unwrap();
    child.stdin.take().unwrap().write_all(b"second question\n").unwrap();
    let sent = child.wait_with_output().unwrap();
    assert!(sent.status.success(), "{}", String::from_utf8_lossy(&sent.stderr));
    let sent: Value = serde_json::from_slice(&sent.stdout).unwrap();
    assert_eq!(sent["id"], id.as_str());
    assert_eq!(sent["text"], "second reply");

    let usage = run(&["thread", "show", "--json"]);
    assert_eq!(usage.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&usage.stderr).contains("thread show needs exactly one thread ID"));

    provider.abort();
    std::fs::remove_dir_all(root).unwrap();
}
