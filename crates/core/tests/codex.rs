use base64::Engine;
use greencode_core::{provider::{codex, Provider, Request}, Block, Message, Role};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Serves one `(status line, content type, body)` reply per connection, in order; returns (headers, body) of each request.
async fn serve(replies: Vec<(&'static str, &'static str, String)>) -> (String, tokio::task::JoinHandle<Vec<(String, String)>>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        let mut seen = Vec::new();
        for (status, ctype, reply) in replies {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = Vec::new();
            let request = loop {
                let mut chunk = [0; 4096];
                let n = socket.read(&mut chunk).await.unwrap();
                assert!(n > 0, "connection closed early");
                buf.extend_from_slice(&chunk[..n]);
                let text = String::from_utf8_lossy(&buf).to_string();
                if let Some((head, body)) = text.split_once("\r\n\r\n") {
                    let len = head.to_lowercase().lines().find_map(|l| l.strip_prefix("content-length:")?.trim().parse::<usize>().ok()).unwrap_or(0);
                    if body.len() >= len { break (head.to_lowercase(), body.to_string()); }
                }
            };
            seen.push(request);
            socket.write_all(format!("HTTP/1.1 {status}\r\ncontent-type: {ctype}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{reply}", reply.len()).as_bytes()).await.unwrap();
        }
        seen
    });
    (origin, task)
}

fn jwt(account: &str) -> String {
    let payload = format!(r#"{{"https://api.openai.com/auth":{{"chatgpt_account_id":"{account}"}}}}"#);
    format!("x.{}.y", base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(payload))
}

fn json(s: String) -> (&'static str, &'static str, String) { ("200 OK", "application/json", s) }

fn temp(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("greencode-codex-{name}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir.join("chatgpt-oauth.json")
}

#[tokio::test]
async fn device_flow_exchanges_code() {
    let access = jwt("acc");
    let (origin, server) = serve(vec![
        json(r#"{"device_auth_id":"d","user_code":"AB-12","interval":"1"}"#.into()),
        ("403 Forbidden", "application/json", "{}".into()),
        json(r#"{"authorization_code":"c","code_verifier":"v"}"#.into()),
        json(format!(r#"{{"access_token":"{access}","refresh_token":"r","expires_in":3600}}"#)),
    ]).await;
    let device = codex::start_at(&origin).await.unwrap();
    assert_eq!(device.user_code, "AB-12");
    assert!(device.verification_uri.ends_with("/codex/device"));
    let tokens = codex::finish_at(&origin, device, Duration::from_millis(1)).await.unwrap();
    assert_eq!((tokens.access.as_str(), tokens.refresh.as_str()), (access.as_str(), "r"));
    assert!(tokens.expires_at > 0);
    let seen = server.await.unwrap();
    assert!(seen[0].1.contains("app_EMoamEEZ73f0CkXaXp7hrann"), "{seen:?}");
    assert!(seen[3].1.contains("code_verifier=v") && seen[3].1.contains("grant_type=authorization_code"), "{seen:?}");
    assert_eq!(codex::account(&access), (Some("acc".into()), None));
}

#[tokio::test]
async fn request_uses_responses_wire() {
    let path = temp("wire");
    let access = jwt("acc");
    codex::save(&path, &codex::Tokens { access, refresh: "r".into(), expires_at: u64::MAX }).unwrap();
    let sse = [
        r#"{"type":"response.output_text.delta","delta":"hi"}"#,
        r#"{"type":"response.output_item.done","item":{"type":"function_call","call_id":"c1","name":"read","arguments":"{\"path\":\"x\"}"}}"#,
        r#"{"type":"response.completed","response":{"usage":{"input_tokens":10,"output_tokens":2,"input_tokens_details":{"cached_tokens":4}}}}"#,
    ].iter().map(|d| format!("data: {d}\n\n")).collect::<String>();
    let (origin, server) = serve(vec![("200 OK", "text/event-stream", sse)]).await;
    let client = codex::Codex::new(origin, path.clone(), None);
    let messages = vec![
        Message::user("hi"),
        Message { role: Role::Assistant, content: vec![Block::ToolUse { id: "t0".into(), name: "read".into(), input: serde_json::json!({"path": "x"}) }] },
        Message { role: Role::User, content: vec![Block::ToolResult { tool_use_id: "t0".into(), content: "ok".into(), is_error: false }] },
    ];
    let tools = greencode_core::tools::defs(false, &[]);
    let streamed = std::sync::Mutex::new(String::new());
    let on_text = |t: &str| streamed.lock().unwrap().push_str(t);
    let reply = client.complete(Request { model: "gpt-5.5", system: "sys", messages: &messages, tools: &tools, max_tokens: 1000, live: None }, &on_text).await.unwrap();
    let (head, body) = server.await.unwrap().remove(0);
    assert!(head.contains("authorization: bearer "), "{head}");
    assert!(head.contains("chatgpt-account-id: acc") && head.contains("originator: greencode"), "{head}");
    let body: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(body["store"], false);
    assert_eq!(body["instructions"], "sys");
    assert_eq!(body["input"][1]["type"], "function_call");
    assert_eq!(body["input"][2]["type"], "function_call_output");
    assert_eq!(body["tools"][0]["type"], "function");
    assert_eq!(*streamed.lock().unwrap(), "hi");
    assert!(matches!(&reply.blocks[0], Block::Text { text } if text == "hi"));
    assert!(matches!(&reply.blocks[1], Block::ToolUse { id, input, .. } if id == "c1" && input["path"] == "x"));
    assert_eq!((reply.usage.input, reply.usage.cache_read, reply.usage.output), (6, 4, 2));
    assert_eq!(reply.stop, "tool_use");
    std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
}

#[tokio::test]
async fn expired_token_is_refreshed() {
    let path = temp("refresh");
    codex::save(&path, &codex::Tokens { access: "old".into(), refresh: "r0".into(), expires_at: 0 }).unwrap();
    let (origin, server) = serve(vec![json(r#"{"access_token":"new","refresh_token":"r1","expires_in":3600}"#.into())]).await;
    assert_eq!(codex::access_token_at(&origin, &path).await.unwrap(), "new");
    let (_, body) = server.await.unwrap().remove(0);
    assert!(body.contains("grant_type=refresh_token") && body.contains("refresh_token=r0"), "{body}");
    let saved: codex::Tokens = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!((saved.access.as_str(), saved.refresh.as_str()), ("new", "r1"));
    std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn concurrent_refresh_reuses_rotated_token() {
    use std::{future::Future, task::Poll};
    let path = temp("concurrent-refresh");
    codex::save(&path, &codex::Tokens { access: "old".into(), refresh: "r0".into(), expires_at: 0 }).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let (received_tx, received_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut request = [0; 4096];
        assert!(socket.read(&mut request).await.unwrap() > 0);
        received_tx.send(()).unwrap();
        release_rx.await.unwrap();
        let body = r#"{"access_token":"new","refresh_token":"r1","expires_in":3600}"#;
        socket.write_all(format!("HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
    });
    let first_origin = origin.clone();
    let first_path = path.clone();
    let first = tokio::spawn(async move { codex::access_token_at(&first_origin, &first_path).await });
    received_rx.await.unwrap();
    let mut second = Box::pin(codex::access_token_at(&origin, &path));
    std::future::poll_fn(|cx| {
        assert!(matches!(second.as_mut().poll(cx), Poll::Pending));
        Poll::Ready(())
    }).await;
    release_tx.send(()).unwrap();
    let (a, b) = tokio::time::timeout(Duration::from_secs(5), async { tokio::join!(first, second) }).await.unwrap();
    assert_eq!(a.unwrap().unwrap(), "new");
    assert_eq!(b.unwrap(), "new");
    server.await.unwrap();
    let saved: codex::Tokens = serde_json::from_reader(std::fs::File::open(&path).unwrap()).unwrap();
    assert_eq!((saved.access.as_str(), saved.refresh.as_str()), ("new", "r1"));
    std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
}
