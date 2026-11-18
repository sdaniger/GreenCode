use greencode_core::provider::mistral;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Serves one JSON reply per connection, in order (`reply(origin)` may point URLs back at the server);
/// returns each request's first line and body.
async fn serve(reply: impl FnOnce(&str) -> Vec<String> + Send + 'static) -> (String, tokio::task::JoinHandle<Vec<(String, String)>>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let replies = reply(&origin);
    let task = tokio::spawn(async move {
        let mut seen = Vec::new();
        for reply in replies {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = Vec::new();
            let (head, body) = loop {
                let mut chunk = [0; 4096];
                let n = socket.read(&mut chunk).await.unwrap();
                assert!(n > 0, "connection closed early");
                buf.extend_from_slice(&chunk[..n]);
                let text = String::from_utf8_lossy(&buf).to_string();
                if let Some((head, body)) = text.split_once("\r\n\r\n") {
                    let len = head.to_lowercase().lines().find_map(|l| l.strip_prefix("content-length:")?.trim().parse::<usize>().ok()).unwrap_or(0);
                    if body.len() >= len { break (head.lines().next().unwrap().to_string(), body.to_string()); }
                }
            };
            seen.push((head, body));
            socket.write_all(format!("HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{reply}", reply.len()).as_bytes()).await.unwrap();
        }
        seen
    });
    (origin, task)
}

#[tokio::test]
async fn sign_in_polls_then_exchanges_for_an_api_key() {
    let (origin, server) = serve(|o| vec![
        format!(r#"{{"process_id":"p1","sign_in_url":"{o}/vibe/approve","poll_url":"{o}/api/vibe/sign-in/p1","expires_at":"2099-01-01T00:00:00Z"}}"#),
        r#"{"status":"pending"}"#.into(),
        r#"{"status":"completed","exchange_token":"x1"}"#.into(),
        r#"{"api_key":"mk-123"}"#.into(),
    ]).await;
    let sign_in = mistral::start_at(&origin).await.unwrap();
    assert_eq!(sign_in.url, format!("{origin}/vibe/approve"));
    assert_eq!(mistral::finish_at(&origin, sign_in, Duration::from_millis(1)).await.unwrap(), "mk-123");
    let seen = server.await.unwrap();
    assert!(seen[0].0.starts_with("POST /api/vibe/sign-in ") && seen[0].1.contains(r#""code_challenge_method":"S256""#), "{seen:?}");
    assert!(seen[1].0.starts_with("GET /api/vibe/sign-in/p1 "), "{seen:?}");
    assert!(seen[3].0.starts_with("POST /api/vibe/sign-in/p1/exchange ") && seen[3].1.contains(r#""exchange_token":"x1""#) && seen[3].1.contains("code_verifier"), "{seen:?}");
}

#[tokio::test]
async fn sign_in_refuses_a_poll_url_off_the_console_api() {
    let (origin, _server) = serve(|o| vec![
        format!(r#"{{"process_id":"p1","sign_in_url":"{o}/x","poll_url":"https://evil.example/api/p1","expires_at":"2099-01-01T00:00:00Z"}}"#),
    ]).await;
    let e = mistral::start_at(&origin).await.unwrap_err();
    assert!(e.to_string().contains("unexpected URL"), "{e}");
}

#[tokio::test]
async fn sign_in_reports_denial() {
    let (origin, _server) = serve(|o| vec![
        format!(r#"{{"process_id":"p1","sign_in_url":"{o}/x","poll_url":"{o}/api/p1","expires_at":"2099-01-01T00:00:00Z"}}"#),
        r#"{"status":"denied"}"#.into(),
    ]).await;
    let sign_in = mistral::start_at(&origin).await.unwrap();
    let e = mistral::finish_at(&origin, sign_in, Duration::from_millis(1)).await.unwrap_err();
    assert!(e.to_string().contains("拒否"), "{e}");
}
