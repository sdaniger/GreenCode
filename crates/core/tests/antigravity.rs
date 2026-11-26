#![cfg(unix)]

use greencode_core::{
    antigravity::{self, LoginStart},
    config::{AcpCfg, Config, Protocol},
    permission::{Mode, Perms},
    registry,
    session::{Meta, Session},
    Command, Event,
};
use sha2::{Digest, Sha512};
use std::{
    os::unix::fs::PermissionsExt,
    path::PathBuf,
    sync::Arc,
    time::Duration,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

static ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn build_zip(dir: &std::path::Path, entries: &[(&str, &[u8])]) -> Vec<u8> {
    let zip_path = dir.join("bundle.zip");
    let script = r#"
import json, sys, zipfile
out, entries = sys.argv[1], json.loads(sys.argv[2])
with zipfile.ZipFile(out, "w") as zf:
    zf.writestr("sub/", b"")
    for name, content in entries:
        zf.writestr(name, content.encode("utf-8"))
"#;
    let payload = serde_json::to_string(
        &entries
            .iter()
            .map(|(n, b)| (*n, std::str::from_utf8(b).unwrap()))
            .collect::<Vec<_>>(),
    )
    .unwrap();
    let status = std::process::Command::new("python3")
        .args(["-c", script, zip_path.to_str().unwrap(), &payload])
        .status()
        .unwrap();
    assert!(status.success());
    std::fs::read(&zip_path).unwrap()
}

#[tokio::test]
async fn managed_zip_install_sets_permissions_and_exposes_single_backend() {
    let _guard = ENV_LOCK.lock().await;
    if std::process::Command::new("python3").arg("--version").output().is_err() {
        return;
    }
    let root = std::env::temp_dir().join(format!("greencode-ag-install-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let home = root.join("home");
    let gemini = root.join("gemini");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&gemini).unwrap();
    unsafe {
        std::env::set_var("GREENCODE_HOME", &home);
        std::env::set_var("GEMINI_HOME", &gemini);
    }

    let zip_bytes = build_zip(&root, &[("antigravity", b"#!/bin/sh\necho ok\n")]);
    let sha512 = format!("{:x}", Sha512::digest(&zip_bytes));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let served = zip_bytes.clone();
    let server = tokio::spawn(async move {
        for _ in 0..2 {
            let (mut conn, _) = listener.accept().await.unwrap();
            let mut buf = [0; 4096];
            let _ = conn.read(&mut buf).await.unwrap();
            conn.write_all(
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    served.len()
                )
                .as_bytes(),
            )
            .await
            .unwrap();
            conn.write_all(&served).await.unwrap();
        }
    });

    // The shape `antigravity::manifest_entry` builds from Google's release manifest, served locally.
    let entry = |sha512: &str| {
        let json = serde_json::json!({"agents": [{
            "id": antigravity::REGISTRY_ID,
            "name": "Google Antigravity",
            "version": "1.5.17",
            "distribution": {"binary": {registry::platform(): {
                "archive": format!("http://{addr}/cli.zip"),
                "cmd": "antigravity",
                "args": ["acp"],
                "sha512": sha512,
            }}},
        }]});
        registry::parse(&json.to_string()).unwrap().pop().unwrap()
    };
    let err = registry::install(&entry(&"0".repeat(128))).await.unwrap_err();
    assert!(err.to_string().contains("sha512"), "{err:#}");
    registry::install(&entry(&sha512)).await.unwrap();
    let agy = home.join("agents/antigravity-acp/1.5.17/antigravity");
    assert_ne!(std::fs::metadata(&agy).unwrap().permissions().mode() & 0o111, 0);

    let loaded = Config::load().unwrap();
    let ag_backends: Vec<_> = loaded
        .backends
        .iter()
        .filter(|b| b.name == "antigravity" || b.name == "antigravity-acp")
        .collect();
    assert_eq!(ag_backends.len(), 1);
    assert_eq!(ag_backends[0].name, "antigravity");
    assert!(ag_backends[0].command[0].ends_with("/antigravity"));
    assert_eq!(ag_backends[0].command[1], "acp");
    assert!(antigravity::is_runtime_installed(ag_backends[0]));
    assert!(!antigravity::is_legacy_runtime(ag_backends[0]));
    assert_eq!(antigravity::installed_version().as_deref(), Some("1.5.17"));

    server.await.unwrap();
    std::fs::remove_dir_all(&root).unwrap();
}

#[tokio::test]
async fn oauth_login_auth_negotiation_thinking_and_logout() {
    let _guard = ENV_LOCK.lock().await;
    if std::process::Command::new("python3").arg("--version").output().is_err() {
        return;
    }
    let root = std::env::temp_dir().join(format!("greencode-ag-auth-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let home = root.join("home");
    let gemini = root.join("gemini");
    let workdir = root.join("proj");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&gemini).unwrap();
    std::fs::create_dir_all(&workdir).unwrap();

    let mock_py = root.join("mock_acp.py");
    std::fs::write(
        &mock_py,
        r#"
import http.server
import json
import os
import pathlib
import sys
import urllib.parse

gemini_home = pathlib.Path(os.environ.get("GEMINI_HOME", str(pathlib.Path.home() / ".gemini")))
token_file = gemini_home / "antigravity-acp" / "acp_token.json"

def send(obj):
    print(json.dumps(obj), flush=True)

for line in sys.stdin:
    msg = json.loads(line)
    method = msg.get("method")
    if method is None:
        continue
    mid = msg["id"]
    if method == "initialize":
        send({
            "jsonrpc": "2.0",
            "id": mid,
            "result": {
                "protocolVersion": 1,
                "agentCapabilities": {"loadSession": True},
                "authMethods": [
                    {"id": "gemini-api-key", "name": "API Key"},
                    {"id": "oauth-personal", "name": "Sign in with Google"}
                ]
            }
        })
    elif method == "authenticate":
        assert os.environ.get("NO_BROWSER") == "true", os.environ.get("NO_BROWSER")
        assert "ELECTRON_RUN_AS_NODE" not in os.environ
        assert "CHROME_CRASHPAD_PIPE_NAME" not in os.environ
        auth_method = msg["params"]["methodId"]
        if token_file.exists() and token_file.stat().st_size > 2:
            send({"jsonrpc": "2.0", "id": mid, "result": {"authenticated": True}})
            continue

        done = {"ok": False}
        class Handler(http.server.BaseHTTPRequestHandler):
            def do_GET(self):
                parsed = urllib.parse.urlparse(self.path)
                qs = urllib.parse.parse_qs(parsed.query)
                if parsed.path == "/cb" and qs.get("state") == ["s123"] and qs.get("code") == ["ok"]:
                    done["ok"] = True
                    self.send_response(200)
                    self.end_headers()
                    self.wfile.write(b"ok")
                else:
                    self.send_response(400)
                    self.end_headers()
                    self.wfile.write(b"bad")
            def log_message(self, *args):
                pass

        srv = http.server.HTTPServer(("127.0.0.1", 0), Handler)
        port = srv.server_address[1]
        print(
            f"Open https://accounts.google.com/o/oauth2/auth?client_id=test&redirect_uri=http://127.0.0.1:{port}/cb&state=s123",
            file=sys.stderr,
            flush=True,
        )
        while not done["ok"]:
            srv.handle_request()
        srv.server_close()
        token_file.parent.mkdir(parents=True, exist_ok=True)
        token_file.write_text(json.dumps({"access_token": "tok", "method": auth_method}))
        send({"jsonrpc": "2.0", "id": mid, "result": {"authenticated": True}})
    elif method == "session/new":
        if not token_file.exists() or token_file.stat().st_size <= 2:
            send({
                "jsonrpc": "2.0",
                "id": mid,
                "error": {"code": -32000, "message": "Authentication required: Call authenticate before starting a session"}
            })
        else:
            send({
                "jsonrpc": "2.0",
                "id": mid,
                "result": {
                    "sessionId": "ag-sess-1",
                    "models": {
                        "currentModelId": "gemini-2.5-pro",
                        "availableModels": [
                            {"modelId": "gemini-2.5-pro"},
                            {"modelId": "claude-sonnet-4-5"}
                        ]
                    },
                    "configOptions": [
                        {
                            "id": "thinking_level",
                            "category": "thought_level",
                            "name": "Thinking",
                            "currentValue": "medium",
                            "options": [
                                {"value": "low"},
                                {"value": "medium"},
                                {"value": "high"}
                            ]
                        }
                    ]
                }
            })
    elif method == "session/prompt":
        sid = msg["params"]["sessionId"]
        send({
            "jsonrpc": "2.0",
            "method": "session/update",
            "params": {
                "sessionId": sid,
                "update": {
                    "sessionUpdate": "agent_thought_chunk",
                    "content": {"type": "text", "text": "thinking step"}
                }
            }
        })
        send({
            "jsonrpc": "2.0",
            "method": "session/update",
            "params": {
                "sessionId": sid,
                "update": {
                    "sessionUpdate": "agent_message_chunk",
                    "content": {"type": "text", "text": "final answer"}
                }
            }
        })
        send({"jsonrpc": "2.0", "id": mid, "result": {"stopReason": "end_turn"}})
    else:
        raise RuntimeError(f"unexpected method: {method}")
"#,
    )
    .unwrap();

    unsafe {
        std::env::set_var("GREENCODE_HOME", &home);
        std::env::set_var("GEMINI_HOME", &gemini);
        std::env::set_var("ELECTRON_RUN_AS_NODE", "1");
        std::env::set_var("CHROME_CRASHPAD_PIPE_NAME", "\\\\.\\pipe\\test");
    }

    let mut cfg = Config::default();
    cfg.backends.push(AcpCfg {
        name: "antigravity".into(),
        command: vec!["python3".into(), mock_py.to_string_lossy().into_owned()],
        env: Default::default(),
        protocol: Protocol::Acp,
    });

    let pending = match antigravity::start_login(&cfg, Some("oauth-personal"), workdir.clone())
        .await
        .unwrap()
    {
        LoginStart::Browser(p) => p,
        LoginStart::Ready(_) => panic!("expected interactive OAuth URL"),
    };
    assert!(pending.url.starts_with("https://accounts.google.com/o/oauth2/auth?"));
    assert!(
        antigravity::submit_callback_url(
            "https://evil.example/cb?code=ok&state=s123",
            Some(&pending.url),
        )
        .await
        .is_err()
    );

    let parsed = url::Url::parse(&pending.url).unwrap();
    let redirect = parsed
        .query_pairs()
        .find(|(k, _)| k == "redirect_uri")
        .unwrap()
        .1;
    let redir_url = url::Url::parse(&redirect).unwrap();
    let port = redir_url.port().unwrap();

    assert!(
        antigravity::submit_callback_url(
            &format!("http://127.0.0.1:{port}/cb?code=ok&state=wrong"),
            Some(&pending.url),
        )
        .await
        .is_err()
    );
    antigravity::submit_callback_url(
        &format!("http://127.0.0.1:{port}/cb?code=ok&state=s123"),
        Some(&pending.url),
    )
    .await
    .unwrap();

    let done = antigravity::finish_login(pending, workdir.clone()).await.unwrap();
    assert_eq!(done.auth_type, "oauth-personal");
    assert_eq!(done.models, ["gemini-2.5-pro", "claude-sonnet-4-5"]);
    assert!(!done.options.is_empty());

    let settings: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(gemini.join("antigravity-acp/settings.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(settings["auth"]["type"], "oauth-personal");
    assert!(antigravity::status().signed_in);
    assert_eq!(
        greencode_core::cached_backend_models()["antigravity"],
        ["gemini-2.5-pro", "claude-sonnet-4-5"]
    );
    assert!(!greencode_core::cached_backend_options()["antigravity"].is_empty());

    let cfg = Arc::new(cfg);
    let meta = Meta::new(workdir.clone(), "default".into(), "antigravity".into());
    let (tx, mut rx) = greencode_core::spawn(
        &tokio::runtime::Handle::current(),
        cfg.clone(),
        meta.clone(),
        Arc::new(Perms::new(Mode::Ask)),
    );
    tx.send(Command::Prompt("hello".into())).unwrap();
    let mut thinking = String::new();
    let mut answer = String::new();
    tokio::time::timeout(Duration::from_secs(10), async {
        while let Some(ev) = rx.recv().await {
            match ev {
                Event::Thinking(t) => thinking.push_str(&t),
                Event::Text(t) => answer.push_str(&t),
                Event::Error(e) => panic!("unexpected error: {e}"),
                Event::Done => break,
                _ => {}
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(thinking, "thinking step");
    assert_eq!(answer, "final answer");
    drop(tx);
    Session::delete(&meta.id);

    assert!(antigravity::logout_in(&home, &gemini, false).unwrap());
    assert!(!antigravity::status().signed_in);
    assert!(!gemini.join("antigravity-acp/acp_token.json").exists());
    assert!(!greencode_core::cached_backend_models().contains_key("antigravity"));

    let meta2 = Meta::new(workdir.clone(), "default".into(), "antigravity".into());
    let (tx2, mut rx2) = greencode_core::spawn(
        &tokio::runtime::Handle::current(),
        cfg,
        meta2.clone(),
        Arc::new(Perms::new(Mode::Ask)),
    );
    tx2.send(Command::Prompt("hello".into())).unwrap();
    let err = tokio::time::timeout(Duration::from_secs(5), async {
        let mut found = None;
        while let Some(ev) = rx2.recv().await {
            match ev {
                Event::Error(e) => found = Some(e),
                Event::Done => break,
                _ => {}
            }
        }
        found
    })
    .await
    .unwrap();
    let err = err.expect("unauthenticated session should fail fast");
    assert!(err.contains("/login antigravity"), "{err}");
    drop(tx2);
    Session::delete(&meta2.id);

    unsafe {
        std::env::remove_var("ELECTRON_RUN_AS_NODE");
        std::env::remove_var("CHROME_CRASHPAD_PIPE_NAME");
    }
    let _ = PathBuf::from(&root);
    std::fs::remove_dir_all(&root).unwrap();
}
