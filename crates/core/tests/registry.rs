use greencode_core::{config::Config, registry};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[test]
fn registry_process_install() {
    let Ok(json) = std::env::var("GREENCODE_REGISTRY_TEST_ENTRY") else { return };
    let entry = registry::parse(&json).unwrap().remove(0);
    tokio::runtime::Runtime::new().unwrap().block_on(registry::install(&entry)).unwrap();
}

#[tokio::test]
async fn registry_install_and_load() {
    let home = std::env::temp_dir().join(format!("greencode-registry-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(&home).unwrap();
    unsafe { std::env::set_var("GREENCODE_HOME", &home); }
    let entries = registry::parse(include_str!("fixtures/acp-registry.json")).unwrap();
    assert_eq!(entries.len(), 3);
    for kind in ["npx", "uvx"] {
        let entry = entries.iter().find(|e| e.kind == kind).unwrap();
        let cfg = registry::install(entry).await.unwrap();
        let package = if kind == "npx" { entry.distribution.npx.as_ref().unwrap() } else { entry.distribution.uvx.as_ref().unwrap() };
        let mut expected = vec![kind.to_string()];
        if kind == "npx" { expected.push("-y".into()); }
        expected.push(package.package.clone());
        expected.extend(package.args.clone());
        assert_eq!(cfg.command, expected);
        assert_eq!(cfg.env, package.env);
        assert!(Config::load().unwrap().backends.iter().any(|b| b.name == entry.id && b.command == expected));
    }
    let source = home.join("source");
    std::fs::create_dir_all(&source).unwrap();
    std::fs::write(source.join("agent"), "#!/bin/sh\necho ok\n").unwrap();
    std::fs::write(source.join(".download"), "archive member").unwrap();
    let archive = home.join("agent.tar.gz");
    assert!(std::process::Command::new("tar").args(["czf"]).arg(&archive).arg("-C").arg(&source).args(["agent", ".download"]).status().unwrap().success());
    let bytes = std::fs::read(&archive).unwrap();
    let hash = format!("{:x}", Sha256::digest(&bytes));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        for _ in 0..2 {
            let (mut conn, _) = listener.accept().await.unwrap();
            let mut buf = [0; 4096];
            let _ = conn.read(&mut buf).await.unwrap();
            conn.write_all(format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", bytes.len()).as_bytes()).await.unwrap();
            conn.write_all(&bytes).await.unwrap();
        }
    });
    let json = serde_json::json!({"agents":[{"id":"test-binary","name":"Test","version":"1","description":"test", "distribution":{"binary":{registry::platform():{"archive":format!("http://{addr}/agent.tar.gz"),"cmd":"./agent","args":["--acp"],"env":{"EXAMPLE":"yes"},"sha256":"bad"}}}}]});
    let mut entry = registry::parse(&json.to_string()).unwrap().remove(0);
    assert!(registry::install(&entry).await.unwrap_err().to_string().contains("sha256"));
    assert!(!registry::installed().iter().any(|i| i.id == entry.id));
    entry.distribution.binary.as_mut().unwrap().get_mut(registry::platform()).unwrap().sha256 = Some(hash);
    let cfg = registry::install(&entry).await.unwrap();
    assert!(std::path::Path::new(&cfg.command[0]).is_file());
    assert_eq!(cfg.command[1], "--acp");
    assert_eq!(cfg.env["EXAMPLE"], "yes");
    assert!(registry::installed().iter().any(|i| i.id == entry.id));
    assert!(Config::load().unwrap().backends.iter().any(|b| b.name == entry.id && b.env == cfg.env));
    assert_eq!(std::fs::read(home.join("agents/test-binary/1/.download")).unwrap(), b"archive member");
    let record_path = home.join("registry-agents.json");
    let saved_records = std::fs::read(&record_path).unwrap();
    std::fs::write(&record_path, b"invalid json").unwrap();
    assert!(registry::install(&entry).await.is_err());
    assert!(std::path::Path::new(&cfg.command[0]).is_file());
    std::fs::write(&record_path, saved_records).unwrap();
    registry::uninstall(&entry.id).unwrap();
    assert!(!home.join("agents").join(&entry.id).exists());
    assert!(!registry::installed().iter().any(|i| i.id == entry.id));
    server.await.unwrap();
    // Both processes must reach the download before either can commit.
    std::fs::write(source.join("agent2"), "second agent").unwrap();
    let second_archive = home.join("second.tar.gz");
    assert!(std::process::Command::new("tar").args(["czf"]).arg(&second_archive).arg("-C").arg(&source).arg("agent2").status().unwrap().success());
    let archives = [std::fs::read(&archive).unwrap(), std::fs::read(&second_archive).unwrap()];
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let mut requests = Vec::new();
        for _ in 0..2 {
            let (mut conn, _) = listener.accept().await.unwrap();
            let mut buf = [0; 4096];
            let n = conn.read(&mut buf).await.unwrap();
            let index = if buf[..n].starts_with(b"GET /0.tar.gz ") { 0 } else { 1 };
            requests.push((conn, index));
        }
        for (mut conn, index) in requests {
            let bytes = &archives[index];
            conn.write_all(format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", bytes.len()).as_bytes()).await.unwrap();
            conn.write_all(bytes).await.unwrap();
        }
    });
    let mut children = Vec::new();
    for (index, command) in ["agent", "agent2"].into_iter().enumerate() {
        let json = serde_json::json!({"agents":[{"id":"test-binary","name":"Test","version":"1","distribution":{"binary":{registry::platform():{"archive":format!("http://{addr}/{index}.tar.gz"),"cmd":command}}}}]});
        children.push(tokio::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "registry_process_install"])
            .env("GREENCODE_HOME", &home).env("GREENCODE_REGISTRY_TEST_ENTRY", json.to_string()).spawn().unwrap());
    }
    for mut child in children { assert!(child.wait().await.unwrap().success()); }
    server.await.unwrap();
    let records = registry::installed();
    let records: Vec<_> = records.iter().filter(|i| i.id == entry.id).collect();
    assert_eq!(records.len(), 1);
    assert!(std::path::Path::new(&records[0].command[0]).is_file());
    registry::uninstall(&entry.id).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut conn, _) = listener.accept().await.unwrap();
        let mut buf = [0; 4096];
        let _ = conn.read(&mut buf).await.unwrap();
        conn.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 536870913\r\nConnection: close\r\n\r\n").await.unwrap();
    });
    let binary = entry.distribution.binary.as_mut().unwrap().get_mut(registry::platform()).unwrap();
    binary.archive = format!("http://{addr}/agent.tar.gz");
    assert!(registry::install(&entry).await.unwrap_err().to_string().contains("too large"));
    server.await.unwrap();
    #[cfg(unix)] {
        use std::os::unix::fs::symlink;
        let outside = home.join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("sentinel"), "safe").unwrap();
        symlink(&outside, source.join("link")).unwrap();
        std::fs::hard_link(source.join("agent"), source.join("hardlink")).unwrap();
        let mut archives = Vec::new();
        for members in [&["link"][..], &["agent", "hardlink"][..]] {
            let path = home.join(format!("bad-{}.tar.gz", archives.len()));
            assert!(std::process::Command::new("tar").arg("czf").arg(&path).arg("-C").arg(&source).args(members).status().unwrap().success());
            archives.push(std::fs::read(path).unwrap());
        }
        archives.push(std::fs::read(&archive).unwrap());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            for bytes in archives {
                let (mut conn, _) = listener.accept().await.unwrap();
                let mut buf = [0; 4096];
                let _ = conn.read(&mut buf).await.unwrap();
                conn.write_all(format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", bytes.len()).as_bytes()).await.unwrap();
                conn.write_all(&bytes).await.unwrap();
            }
        });
        let binary = entry.distribution.binary.as_mut().unwrap().get_mut(registry::platform()).unwrap();
        binary.archive = format!("http://{addr}/agent.tar.gz");
        binary.sha256 = None;
        for _ in 0..2 {
            assert!(registry::install(&entry).await.is_err());
            assert!(!registry::installed().iter().any(|i| i.id == entry.id));
        }
        symlink(&outside, home.join("agents").join(&entry.id)).unwrap();
        assert!(registry::install(&entry).await.is_err());
        assert_eq!(std::fs::read(outside.join("sentinel")).unwrap(), b"safe");
        assert!(!outside.join("1").exists());
        server.await.unwrap();
    }
    std::fs::remove_dir_all(home).unwrap();
}
