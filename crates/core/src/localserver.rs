//! A tiny bounded HTTP/1.1 server shared by the collab relay, `/share` and stream servers.
//! One request per connection, `Connection: close`; bodies are capped at the caller's limit.
use anyhow::{bail, Result};
use serde_json::Value;
use std::{future::Future, net::SocketAddr, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};

pub struct Request {
    pub method: String,
    pub path: String,
    pub query: Vec<(String, String)>,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Request {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.trim())
    }

    /// `Bearer` token from `Authorization`, or the `token` query parameter.
    pub fn token(&self) -> Option<&str> {
        self.header("authorization").and_then(|v| v.strip_prefix("Bearer ")).or_else(|| {
            self.query.iter().find(|(k, _)| k == "token").map(|(_, v)| v.as_str())
        })
    }

    pub fn query(&self, name: &str) -> Option<&str> {
        self.query.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
    }

    pub fn authorized(&self, token: Option<&str>) -> bool {
        match token {
            Some(token) => self.token() == Some(token),
            None => true,
        }
    }
}

pub struct Response {
    pub status: &'static str,
    pub content_type: &'static str,
    pub body: Vec<u8>,
}

impl Response {
    pub fn new(status: &'static str, content_type: &'static str, body: impl Into<Vec<u8>>) -> Self {
        Self { status, content_type, body: body.into() }
    }

    pub fn json(value: Value) -> Self {
        Self::new("200 OK", "application/json", value.to_string())
    }

    pub fn error(status: &'static str, message: &str) -> Self {
        Self::new(status, "application/json", serde_json::json!({ "error": message }).to_string())
    }

    pub fn html(body: &str) -> Self {
        Self::new("200 OK", "text/html; charset=utf-8", body.as_bytes().to_vec())
    }
}

async fn read_request(stream: &mut TcpStream, max_body: usize) -> Result<Request> {
    let mut data = Vec::new();
    let mut buf = [0; 4096];
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    let (end, len) = loop {
        let n = tokio::time::timeout_at(deadline, stream.read(&mut buf)).await??;
        if n == 0 { bail!("incomplete request"); }
        data.extend_from_slice(&buf[..n]);
        if data.len() > max_body + 65_536 { bail!("request too large"); }
        if let Some(pos) = data.windows(4).position(|w| w == b"\r\n\r\n") {
            let end = pos + 4;
            let headers = std::str::from_utf8(&data[..pos])?;
            let mut lengths = headers.lines().skip(1).filter_map(|line| line.split_once(':')).filter(|(k, _)| k.eq_ignore_ascii_case("content-length"));
            let len = lengths.next().map(|(_, v)| v.trim().parse::<usize>()).transpose()?.unwrap_or(0);
            if lengths.next().is_some() || len > max_body { bail!("invalid request length"); }
            if data.len() >= end + len { break (end, len); }
        }
    };
    let head = std::str::from_utf8(&data[..end])?;
    let mut lines = head.lines();
    let mut request = lines.next().unwrap_or_default().split_whitespace();
    let method = request.next().unwrap_or_default().to_owned();
    let target = request.next().unwrap_or("/").to_owned();
    let (path, query) = target.split_once('?').unwrap_or((&target, ""));
    let headers: Vec<(String, String)> = lines.filter_map(|line| {
        let (k, v) = line.split_once(':')?;
        Some((k.trim().to_owned(), v.trim().to_owned()))
    }).collect();
    Ok(Request {
        method,
        path: path.to_owned(),
        query: url::form_urlencoded::parse(query.as_bytes()).map(|(k, v)| (k.into_owned(), v.into_owned())).collect(),
        headers,
        body: data[end..end + len].to_vec(),
    })
}

/// Serves until the listener closes. Handlers run on their own task per connection.
pub async fn serve<F, Fut>(listener: TcpListener, max_body: usize, handler: F) -> Result<()>
where
    F: Fn(Request) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Response> + Send,
{
    let handler = std::sync::Arc::new(handler);
    loop {
        let (mut socket, _) = listener.accept().await?;
        let handler = handler.clone();
        tokio::spawn(async move {
            let result = async {
                let request = read_request(&mut socket, max_body).await?;
                let response = handler(request).await;
                let head = format!(
                    "HTTP/1.1 {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\nCache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\nReferrer-Policy: no-referrer\r\n\r\n",
                    response.status, response.content_type, response.body.len()
                );
                socket.write_all(head.as_bytes()).await?;
                socket.write_all(&response.body).await?;
                Ok::<_, anyhow::Error>(())
            };
            let _ = result.await;
        });
    }
}

/// `host:port` for a listener, with a wildcard bind reported as loopback so local links work.
pub fn address(listener: &TcpListener) -> SocketAddr {
    listener.local_addr().unwrap_or_else(|_| SocketAddr::from(([127, 0, 0, 1], 0)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn serves_one_request_and_rejects_oversized_bodies() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(serve(listener, 8, |request| async move {
            Response::json(serde_json::json!({"path": request.path, "value": request.query("v")}))
        }));
        let client = reqwest::Client::new();
        let value: Value = client.get(format!("http://{addr}/hello?v=2")).send().await.unwrap().json().await.unwrap();
        assert_eq!(value, serde_json::json!({"path": "/hello", "value": "2"}));
    }
}
