use greencode_core::{config::{Config, ProviderCfg}, remote, session::{Meta, Session}, Message};
use serde_json::{json, Value};
use tokio::{net::TcpListener, time::Duration};

#[tokio::test]
async fn remote_cancel_and_guest_links_are_scoped_and_expire() {
    let home = std::env::temp_dir().join(format!("greencode-remote-guest-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(&home).unwrap();
    // This integration test binary has one test and a private home.
    unsafe { std::env::set_var("GREENCODE_HOME", &home); }

    let hanging = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_addr = hanging.local_addr().unwrap();
    let upstream = tokio::spawn(async move {
        let (socket, _) = hanging.accept().await.unwrap();
        tokio::time::sleep(Duration::from_secs(30)).await;
        drop(socket);
    });
    let mut cfg = Config::default();
    cfg.model = "slow/test".into();
    cfg.providers.insert("slow".into(), ProviderCfg { kind: "anthropic".into(), base_url: format!("http://{upstream_addr}"), api_key: Some("test".into()), ..Default::default() });

    let first = Meta::new(home.clone(), "slow/test".into(), "native".into());
    let mut first = first;
    first.title = "First thread".into();
    Session::create(&first).unwrap();
    Session::open(&first.id).unwrap().append(&Message::user("alpha secret")).unwrap();
    let second = Meta::new(home.clone(), "slow/test".into(), "native".into());
    let mut second = second;
    second.title = "Second thread".into();
    Session::create(&second).unwrap();
    Session::open(&second.id).unwrap().append(&Message::user("beta secret")).unwrap();

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let token = remote::token(true).unwrap();
    let server = tokio::spawn(remote::serve(listener, token.clone(), cfg));
    let client = reqwest::Client::new();
    let owner = |method: reqwest::Method, path: String| client.request(method, format!("{base}{path}")).bearer_auth(&token);

    let shared: Value = owner(reqwest::Method::POST, format!("/api/threads/{}/share", first.id)).json(&json!({"ttl_secs": 3600})).send().await.unwrap().json().await.unwrap();
    let guest_token = shared["token"].as_str().unwrap().to_string();
    let guest_base = format!("{base}/guest/{guest_token}");
    assert!(shared["url"].as_str().unwrap().ends_with(&format!("/guest/{guest_token}")));
    assert!(shared["expires"].as_u64().unwrap() > 0);

    let page = client.get(&guest_base).send().await.unwrap();
    assert_eq!(page.status(), 200);
    assert!(page.text().await.unwrap().contains("read-only guest view"));
    let view: Value = client.get(format!("{guest_base}/thread")).send().await.unwrap().json().await.unwrap();
    assert_eq!(view["title"], "First thread");
    assert_eq!(view["model"], "slow/test");
    let transcript = view["messages"].to_string();
    assert!(transcript.contains("alpha secret"), "{transcript}");
    assert!(!transcript.contains("beta secret"), "guest token must not expose another thread");

    // A guest token is not an owner token and cannot open another thread's turn.
    assert_eq!(client.get(format!("{base}/api/threads")).send().await.unwrap().status(), 401);
    assert_eq!(
        owner(reqwest::Method::POST, format!("/api/threads/{}/prompt", second.id)).json(&json!({"text":"work"})).send().await.unwrap().status(),
        202
    );
    assert_eq!(client.get(format!("{guest_base}/events")).send().await.unwrap().status(), 404, "no running turn for the shared thread");
    assert_eq!(owner(reqwest::Method::POST, format!("/api/threads/{}/cancel", second.id)).send().await.unwrap().status(), 200);
    let events = owner(reqwest::Method::GET, format!("/api/threads/{}/events", second.id)).send().await.unwrap();
    let events = tokio::time::timeout(Duration::from_secs(10), events.text()).await.unwrap().unwrap();
    assert!(events.contains("\"type\":\"done\""), "{events}");
    assert_eq!(owner(reqwest::Method::POST, format!("/api/threads/{}/cancel", second.id)).send().await.unwrap().status(), 409, "cancel needs a running turn");

    let request = client.post(format!("{guest_base}/request")).json(&json!({"text":"please look"})).send().await.unwrap();
    assert_eq!(request.status(), 202);
    let pending: Value = owner(reqwest::Method::GET, format!("/api/threads/{}/requests", first.id)).send().await.unwrap().json().await.unwrap();
    assert_eq!(pending[0]["text"], "please look");
    let id = pending[0]["id"].as_str().unwrap().to_string();
    assert_eq!(owner(reqwest::Method::POST, format!("/api/threads/{}/requests/{id}/dismiss", first.id)).send().await.unwrap().status(), 200);
    let pending: Value = owner(reqwest::Method::GET, format!("/api/threads/{}/requests", first.id)).send().await.unwrap().json().await.unwrap();
    assert_eq!(pending.as_array().unwrap().len(), 0);

    assert_eq!(owner(reqwest::Method::POST, format!("/api/threads/{}/unshare", first.id)).send().await.unwrap().status(), 200);
    assert_eq!(client.get(format!("{guest_base}/thread")).send().await.unwrap().status(), 404, "revoked links stop working");

    let expired: Value = owner(reqwest::Method::POST, format!("/api/threads/{}/share", first.id)).json(&json!({"ttl_secs": 0})).send().await.unwrap().json().await.unwrap();
    let expired = format!("{base}/guest/{}", expired["token"].as_str().unwrap());
    assert_eq!(client.get(&expired).send().await.unwrap().status(), 404, "expired links stop working");

    server.abort();
    upstream.abort();
    std::fs::remove_dir_all(home).unwrap();
}
