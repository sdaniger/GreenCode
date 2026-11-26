//! Minimal client WebSocket: connect with headers, send one JSON text frame at a time, read with a deadline.
use anyhow::{anyhow, Result};
use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use std::time::Duration;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;

pub struct Ws {
    stream: tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
}

impl Ws {
    /// `headers` carry the provider's auth; `wss://` is used for `https://` origins.
    pub async fn connect(url: &str, headers: &[(&str, String)]) -> Result<Ws> {
        let mut request = url.into_client_request()?;
        for (name, value) in headers {
            request.headers_mut().insert(
                reqwest::header::HeaderName::from_bytes(name.as_bytes())?,
                reqwest::header::HeaderValue::from_str(value)?,
            );
        }
        let (stream, _) = tokio_tungstenite::connect_async(request)
            .await
            .map_err(|e| anyhow!("WebSocket connect to {url} failed: {e}"))?;
        Ok(Ws { stream })
    }

    pub async fn send_json(&mut self, value: &Value) -> Result<()> {
        self.stream
            .send(Message::text(serde_json::to_string(value)?))
            .await
            .map_err(|e| anyhow!("WebSocket send failed: {e}"))
    }

    /// Next text frame, `None` once the peer closed; timeout when the server stays silent.
    pub async fn next_text(&mut self, timeout: Option<Duration>) -> Result<Option<String>> {
        let read = async {
            loop {
                match self.stream.next().await {
                    Some(Ok(Message::Text(text))) => return Ok(Some(text.to_string())),
                    Some(Ok(Message::Binary(bytes))) => {
                        return Ok(Some(String::from_utf8_lossy(&bytes).into_owned()))
                    }
                    Some(Ok(Message::Close(_))) | None => return Ok(None),
                    // Ping/Pong are answered by tungstenite when the stream is polled again.
                    Some(Ok(_)) => continue,
                    Some(Err(e)) => return Err(anyhow!("WebSocket read failed: {e}")),
                }
            }
        };
        match timeout {
            Some(limit) => tokio::time::timeout(limit, read)
                .await
                .map_err(|_| anyhow!("no WebSocket event within {}s", limit.as_secs()))?,
            None => read.await,
        }
    }
}

/// `https://host/path` → `wss://host/path`.
pub fn socket_url(base: &str) -> String {
    match base.strip_prefix("https://") {
        Some(rest) => format!("wss://{rest}"),
        None => match base.strip_prefix("http://") {
            Some(rest) => format!("ws://{rest}"),
            None => base.to_string(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn socket_url_upgrades_the_scheme() {
        assert_eq!(socket_url("https://api.openai.com/v1/responses"), "wss://api.openai.com/v1/responses");
        assert_eq!(socket_url("http://127.0.0.1:9/responses"), "ws://127.0.0.1:9/responses");
    }

    /// The mock server answers like the Responses API: created, a text delta, completed.
    #[tokio::test]
    async fn frames_round_trip_over_a_local_socket() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_async(tcp).await.unwrap();
            let first = socket.next().await.unwrap().unwrap();
            assert!(first.to_text().unwrap().contains("response.create"));
            for event in [
                serde_json::json!({"type": "response.created", "response": {"id": "resp_1"}}),
                serde_json::json!({"type": "response.output_text.delta", "delta": "hi"}),
                serde_json::json!({"type": "response.completed", "response": {"usage": {}}}),
            ] {
                socket.send(Message::text(event.to_string())).await.unwrap();
            }
        });
        let mut ws = Ws::connect(&format!("ws://127.0.0.1:{port}/responses"), &[("authorization", "Bearer t".into())])
            .await
            .unwrap();
        ws.send_json(&serde_json::json!({"type": "response.create"})).await.unwrap();
        let mut seen = Vec::new();
        while let Some(text) = ws.next_text(Some(Duration::from_secs(5))).await.unwrap() {
            let event: Value = serde_json::from_str(&text).unwrap();
            seen.push(event["type"].as_str().unwrap().to_string());
            if seen.len() == 3 {
                break;
            }
        }
        assert_eq!(seen, ["response.created", "response.output_text.delta", "response.completed"]);
        server.await.unwrap();
    }
}
