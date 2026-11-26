use greencode_core::{config::{Config, ProviderCfg}, remote, session::{Meta, Session}};
use serde_json::json;
use tokio::{io::{AsyncReadExt, AsyncWriteExt}, net::TcpListener};

#[tokio::test]
async fn remote_auth_threads_and_stream() {
    let home = std::env::temp_dir().join(format!("greencode-remote-test-{}", std::process::id()));
    std::fs::create_dir_all(&home).unwrap();
    // This integration test binary has one test and a private home.
    unsafe { std::env::set_var("GREENCODE_HOME", &home); }
    let fake = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let fake_addr = fake.local_addr().unwrap();
    let upstream = tokio::spawn(async move {
        let (mut socket, _) = fake.accept().await.unwrap();
        let mut buf = Vec::new();
        let mut chunk = [0; 4096];
        loop {
            let n = socket.read(&mut chunk).await.unwrap();
            assert!(n > 0);
            buf.extend_from_slice(&chunk[..n]);
            if let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                let head = String::from_utf8_lossy(&buf[..end]).to_lowercase();
                let len: usize = head.lines().find_map(|l| l.strip_prefix("content-length:")).unwrap().trim().parse().unwrap();
                if buf.len() >= end + 4 + len { break; }
            }
        }
        let body = concat!(
            "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"hello remote\"}}\n\n",
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":1}}\n\n"
        );
        socket.write_all(format!("HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n{body}").as_bytes()).await.unwrap();
    });
    let mut cfg = Config::default();
    cfg.mode = greencode_core::permission::Mode::Full;
    cfg.policy = Some(greencode_core::policy::Policy::parse(
        r#"{"schemaVersion":1,"organization":"test","limits":{"maxMode":"ask"}}"#,
    ).unwrap());
    cfg.providers.insert("local".into(), ProviderCfg { kind: "anthropic".into(), base_url: format!("http://{fake_addr}"), api_key: Some("test".into()), ..Default::default() });
    let mut meta = Meta::new(home.clone(), "local/test".into(), "native".into());
    meta.title = "test thread".into();
    Session::create(&meta).unwrap();
    let view = serde_json::from_value(serde_json::json!({"meta":meta,"mode":"plan"})).unwrap();
    Session::save_view(&view).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let token = remote::token(true).unwrap();
    assert_eq!(remote::token(false).unwrap(), token);
    let server = tokio::spawn(remote::serve(listener, token.clone(), cfg));
    let client = reqwest::Client::new();
    assert_eq!(client.get(format!("{base}/api/threads")).send().await.unwrap().status(), 401);
    let html = client.get(format!("{base}/?token={token}")).send().await.unwrap().text().await.unwrap();
    assert!(html.contains("<title>GreenCode"));
    assert!(html.contains("tool_end") && html.contains("const tools={}"), "the page renders tool rows");
    let list: serde_json::Value = client.get(format!("{base}/api/threads")).bearer_auth(&token).send().await.unwrap().json().await.unwrap();
    assert!(list.as_array().unwrap().iter().any(|v| v["id"] == meta.id));
    let url = format!("{base}/api/threads/{}", meta.id);
    let response = client.post(format!("{url}/prompt")).bearer_auth(&token).json(&json!({"text":"hi"})).send().await.unwrap();
    assert_eq!(response.status(), 202);
    let events = tokio::time::timeout(std::time::Duration::from_secs(10), client.get(format!("{url}/events")).bearer_auth(&token).send()).await.unwrap().unwrap().text().await.unwrap();
    assert!(events.contains("\"type\":\"text\"") && events.contains("hello remote"), "{events}");
    assert!(events.contains("\"type\":\"done\""), "{events}");
    upstream.await.unwrap();
    server.abort();
    std::fs::remove_dir_all(home).unwrap();
}

#[tokio::test]
async fn remote_rejects_cross_origin_requests_and_hides_referrer() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(remote::serve(listener, "secret".into(), Config::default()));
    let client = reqwest::Client::new();
    let url = format!("{base}/?token=secret");
    let response = client.get(&url).header("Origin", &base).send().await.unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()["referrer-policy"], "no-referrer");
    for origin in ["http://attacker.example", "null"] {
        assert_eq!(client.get(&url).header("Origin", origin).send().await.unwrap().status(), 403);
        assert_eq!(client.post(format!("{base}/mcp?token=secret")).header("Origin", origin).body("{}").send().await.unwrap().status(), 403);
    }
    assert_eq!(client.get(&url).header("Origin", &base).header("Origin", &base).send().await.unwrap().status(), 403);
    server.abort();
}
