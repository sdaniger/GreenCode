use greencode_core::{auth, config::Config, provider::copilot, Block, Message, Role};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Serves one JSON reply per connection, in order; returns the request bodies.
async fn serve(replies: Vec<&'static str>) -> (String, tokio::task::JoinHandle<Vec<String>>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        let mut bodies = Vec::new();
        for reply in replies {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = Vec::new();
            let body = loop {
                let mut chunk = [0; 4096];
                let n = socket.read(&mut chunk).await.unwrap();
                assert!(n > 0, "connection closed early");
                buf.extend_from_slice(&chunk[..n]);
                let text = String::from_utf8_lossy(&buf).to_string();
                if let Some((head, body)) = text.split_once("\r\n\r\n") {
                    let len = head.to_lowercase().lines().find_map(|l| l.strip_prefix("content-length:")?.trim().parse::<usize>().ok()).unwrap_or(0);
                    if body.len() >= len { break body.to_string(); }
                }
            };
            bodies.push(body);
            socket.write_all(format!("HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{reply}", reply.len()).as_bytes()).await.unwrap();
        }
        bodies
    });
    (origin, task)
}

const DEVICE: &str = r#"{"device_code":"d","user_code":"ABCD-1234","verification_uri":"https://github.com/login/device","interval":1,"expires_in":60}"#;

#[tokio::test]
async fn device_flow_polls_until_approved() {
    let (origin, server) = serve(vec![DEVICE, r#"{"error":"authorization_pending"}"#, r#"{"access_token":"gho_x"}"#]).await;
    let device = copilot::start_at(&origin).await.unwrap();
    assert_eq!(device.user_code, "ABCD-1234");
    assert_eq!(copilot::finish_at(&origin, device, Duration::from_millis(1)).await.unwrap(), "gho_x");
    let bodies = server.await.unwrap();
    assert!(bodies[0].contains("client_id=Ov23li8tweQw6odWQebz"), "{}", bodies[0]);
    assert!(bodies[1].contains("device_code=d") && bodies[2].contains("device_code=d"), "{bodies:?}");
}

#[tokio::test]
async fn device_flow_reports_denial() {
    let (origin, _server) = serve(vec![DEVICE, r#"{"error":"access_denied","error_description":"no"}"#]).await;
    let device = copilot::start_at(&origin).await.unwrap();
    let e = copilot::finish_at(&origin, device, Duration::from_millis(1)).await.unwrap_err();
    assert!(e.to_string().contains("access_denied"), "{e}");
}

#[test]
fn headers_mark_agent_turns() {
    let has = |h: &[(&str, String)], k: &str, v: &str| h.iter().any(|(n, x)| *n == k && x == v);
    let user = copilot::headers(&[Message::user("hi")]);
    assert!(has(&user, "X-Initiator", "user") && has(&user, "Copilot-Integration-Id", "copilot-chat"));
    let result = Message { role: Role::User, content: vec![Block::ToolResult { tool_use_id: "t".into(), content: "ok".into(), is_error: false }] };
    assert!(has(&copilot::headers(&[Message::user("hi"), result]), "X-Initiator", "agent"));
}

#[test]
fn token_is_stored_as_subscription() {
    let home = std::env::temp_dir().join(format!("greencode-copilot-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&home);
    std::env::set_var("GREENCODE_HOME", &home);
    let mut cfg = Config::load().unwrap();
    auth::set_token(&mut cfg, "github-copilot", "gho_x").unwrap();
    assert_eq!(auth::status("github-copilot", &cfg.providers["github-copilot"]), auth::Method::Subscription);
    let again = Config::load().unwrap();
    assert_eq!(again.providers["github-copilot"].api_key.as_deref(), Some("gho_x"));
    assert_eq!(auth::status("github-copilot", &again.providers["github-copilot"]), auth::Method::Subscription);
    auth::logout(&mut cfg, "github-copilot").unwrap();
    assert!(matches!(auth::status("github-copilot", &cfg.providers["github-copilot"]), auth::Method::None(_)));
    let _ = std::fs::remove_dir_all(&home);
}
