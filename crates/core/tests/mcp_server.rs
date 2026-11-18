use greencode_core::{config::Config, mcp_server, remote, session::{Meta, Session}};
use serde_json::{json, Value};
use tokio::{io::{AsyncReadExt, AsyncWriteExt}, net::TcpListener};

#[tokio::test]
async fn mcp_requests_and_http_auth() {
    let home = std::env::temp_dir().join(format!("greencode-mcp-test-{}", std::process::id()));
    std::fs::create_dir_all(&home).unwrap();
    // This test binary has a single test and a private home.
    unsafe { std::env::set_var("GREENCODE_HOME", &home); }
    let fake = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = fake.local_addr().unwrap();
    std::fs::write(home.join("config.toml"), format!("model = \"local/test\"\n[providers.local]\nkind = \"anthropic\"\nbase_url = \"http://{addr}\"\napi_key = \"test\"\n")).unwrap();
    let upstream = tokio::spawn(async move {
        let (mut socket, _) = fake.accept().await.unwrap();
        let mut buf = Vec::new();
        let mut chunk = [0; 4096];
        loop {
            let n = socket.read(&mut chunk).await.unwrap();
            assert!(n > 0);
            buf.extend_from_slice(&chunk[..n]);
            if let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                let headers = String::from_utf8_lossy(&buf[..end]).to_lowercase();
                let len: usize = headers.lines().find_map(|l| l.strip_prefix("content-length:")).unwrap().trim().parse().unwrap();
                if buf.len() >= end + 4 + len { break; }
            }
        }
        let body = concat!("data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n", "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"hello mcp\"}}\n\n", "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":1}}\n\n");
        socket.write_all(format!("HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n{body}").as_bytes()).await.unwrap();
    });
    let meta = Meta::new(home.clone(), "local/test".into(), "native".into());
    Session::create(&meta).unwrap();
    let request = |id, method, params| json!({"jsonrpc":"2.0","id":id,"method":method,"params":params});
    let init = mcp_server::handle(request(1, "initialize", json!({}))).await.unwrap();
    assert_eq!(init["result"]["protocolVersion"], "2025-06-18");
    let tools = mcp_server::handle(request(2, "tools/list", json!({}))).await.unwrap();
    assert_eq!(tools["result"]["tools"].as_array().unwrap().len(), 6);
    let list = mcp_server::handle(request(3, "tools/call", json!({"name":"list_threads","arguments":{}}))).await.unwrap();
    let threads: Value = serde_json::from_str(list["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(threads[0]["id"], meta.id);
    let reply = mcp_server::handle(request(4, "tools/call", json!({"name":"send_message","arguments":{"id":meta.id,"text":"hello"}}))).await.unwrap();
    let result: Value = serde_json::from_str(reply["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(result["text"], "hello mcp");
    upstream.await.unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/mcp", listener.local_addr().unwrap());
    let server = tokio::spawn(remote::serve(listener, "secret".into(), Config::default()));
    let client = reqwest::Client::new();
    assert_eq!(client.post(&url).json(&request(1, "initialize", json!({}))).send().await.unwrap().status(), 401);
    let response: Value = client.post(&url).bearer_auth("secret").json(&request(1, "ping", json!({}))).send().await.unwrap().json().await.unwrap();
    assert_eq!(response["result"], json!({}));
    server.abort();
    std::fs::remove_dir_all(home).unwrap();
}
