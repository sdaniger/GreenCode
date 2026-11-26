//! Cloudflare AI Gateway (`providers/cloudflare-ai-gateway.ts`): one base URL, three upstream routes.
//!
//! `anthropic/<model>` rides the Anthropic Messages passthrough (`{base}/anthropic`, `x-api-key`),
//! `openai/<model>` the OpenAI passthrough (`{base}/openai`, stripped id), and every other id the
//! Workers AI OpenAI-compatible route (`{base}/compat`, id kept whole). Those two send the gateway
//! token as `cf-aig-authorization` instead of an API key. `<account>`/`<gateway>` in the configured
//! base come from the environment when the user leaves them in place.
use super::anthropic::{self, Anthropic};
use super::openai::{self, OpenAi};
use super::{send, sse, Provider, Reply, Request};
use crate::BoxFut;
use anyhow::{anyhow, bail, Result};
use serde_json::Value;

pub struct Gateway {
    pub http: reqwest::Client,
    pub base: String,
    pub key: String,
    pub effort: Option<String>,
    pub wire: super::Wire,
}

enum Route {
    Anthropic,
    OpenAi,
    Compat,
}

impl Gateway {
    /// The configured base with the routing ids filled in, as OMP resolves them per request.
    pub fn base_url(&self) -> Result<String> {
        if !self.base.contains("<account>") && !self.base.contains("<gateway>") {
            return Ok(self.base.trim_end_matches('/').to_string());
        }
        let env = |name: &str| std::env::var(name).ok().filter(|v| !v.trim().is_empty());
        let account = env("CLOUDFLARE_ACCOUNT_ID")
            .ok_or_else(|| anyhow!("cloudflare-ai-gateway にはアカウント ID が必要です。base_url の <account> を置き換えるか、CLOUDFLARE_ACCOUNT_ID を設定してください"))?;
        let gateway = env("CLOUDFLARE_GATEWAY_ID")
            .ok_or_else(|| anyhow!("cloudflare-ai-gateway にはゲートウェイ ID が必要です。base_url の <gateway> を置き換えるか、CLOUDFLARE_GATEWAY_ID を設定してください"))?;
        Ok(self.base.replace("<account>", account.trim()).replace("<gateway>", gateway.trim()).trim_end_matches('/').to_string())
    }

    fn route(model: &str) -> (Route, &str) {
        if let Some(rest) = model.strip_prefix("anthropic/") {
            (Route::Anthropic, rest)
        } else if let Some(rest) = model.strip_prefix("openai/") {
            (Route::OpenAi, rest)
        } else {
            (Route::Compat, model)
        }
    }
}

impl Provider for Gateway {
    fn complete<'a>(&'a self, req: Request<'a>, on_text: &'a (dyn Fn(&str) + Send + Sync)) -> BoxFut<'a, Result<Reply>> {
        Box::pin(async move {
            if self.key.is_empty() {
                bail!("cloudflare-ai-gateway のトークンがありません。CLOUDFLARE_AI_GATEWAY_API_KEY を設定してください")
            }
            let base = self.base_url()?;
            let (route, model) = Self::route(req.model);
            match route {
                Route::Anthropic => {
                    // The passthrough takes the upstream id: prefix stripped, dots as dashes.
                    let model = model.replace('.', "-");
                    let client = Anthropic {
                        http: self.http.clone(),
                        base: format!("{base}/anthropic"),
                        key: self.key.clone(),
                        oauth: None,
                        effort: self.effort.clone(),
                        wire: self.wire.clone(),
                    };
                    let body = client.body(&Request { model: &model, ..req });
                    let resp = send(&self.wire.retry, || {
                        self.http.post(format!("{base}/anthropic/v1/messages"))
                            .header("anthropic-version", "2026-10-21")
                            .header("x-api-key", &self.key)
                            .json(&body)
                    })
                    .await?;
                    let mut acc = anthropic::Acc::default();
                    sse(resp, self.wire.timeouts, |data| {
                        if data == "[DONE]" {
                            return Ok(true);
                        }
                        if let Ok(ev) = serde_json::from_str::<Value>(data) {
                            if let Some(t) = acc.feed(&ev)? {
                                on_text(&t);
                            }
                        }
                        Ok(false)
                    })
                    .await?;
                    Ok(acc.finish())
                }
                Route::OpenAi | Route::Compat => {
                    let route_base = match route {
                        Route::OpenAi => format!("{base}/openai"),
                        _ => format!("{base}/compat"),
                    };
                    let client = OpenAi {
                        http: self.http.clone(),
                        base: route_base.clone(),
                        key: String::new(),
                        copilot: false,
                        effort: self.effort.clone(),
                        wire: self.wire.clone(),
                    };
                    let body = client.body(&Request { model, ..req });
                    let resp = send(&self.wire.retry, || {
                        self.http.post(format!("{route_base}/chat/completions"))
                            .header("cf-aig-authorization", format!("Bearer {}", self.key))
                            .json(&body)
                    })
                    .await?;
                    let mut acc = openai::Acc::default();
                    sse(resp, self.wire.timeouts, |data| {
                        if data == "[DONE]" {
                            return Ok(true);
                        }
                        if let Ok(ev) = serde_json::from_str::<Value>(data) {
                            if let Some(t) = acc.feed(&ev)? {
                                on_text(&t);
                            }
                        }
                        Ok(false)
                    })
                    .await?;
                    Ok(acc.finish())
                }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::localserver::{serve, Request as Http};
    use crate::Message;
    use std::sync::Mutex;

    async fn record(answer: &'static str) -> (String, tokio::task::JoinHandle<Http>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let (tx, rx) = tokio::sync::oneshot::channel();
        let tx = Mutex::new(Some(tx));
        tokio::spawn(async move {
            let _ = serve(listener, 1 << 20, move |req: Http| {
                let tx = tx.lock().unwrap().take();
                async move {
                    if let Some(tx) = tx {
                        let _ = tx.send(req);
                    }
                    crate::localserver::Response::new("200 OK", "text/event-stream", answer)
                }
            }).await;
        });
        (origin, tokio::spawn(async move { rx.await.unwrap() }))
    }

    fn gateway(base: String) -> Gateway {
        Gateway { http: reqwest::Client::new(), base, key: "cfut_x".into(), effort: None, wire: super::super::Wire::default() }
    }

    const ANTHROPIC: &str = "data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"hi\"}}\n\ndata: {\"type\":\"message_stop\"}\n";
    const CHAT: &str = "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\ndata: [DONE]\n";

    #[tokio::test]
    async fn anthropic_route_strips_the_namespace_and_sends_the_api_key() {
        let (origin, recorded) = record(ANTHROPIC).await;
        let _ = gateway(origin).complete(
            Request { model: "anthropic/claude-sonnet-4.5", system: "", messages: &[Message::user("hi")], tools: &[], max_tokens: 50, live: None },
            &|_| {},
        ).await.unwrap();
        let req = recorded.await.unwrap();
        assert_eq!(req.path, "/anthropic/v1/messages");
        assert_eq!(req.header("x-api-key"), Some("cfut_x"));
        let body: Value = serde_json::from_slice(&req.body).unwrap();
        assert_eq!(body["model"], "claude-sonnet-4-5");
    }

    #[tokio::test]
    async fn openai_route_uses_the_gateway_authorization_header() {
        let (origin, recorded) = record(CHAT).await;
        let _ = gateway(origin).complete(
            Request { model: "openai/gpt-5.1", system: "", messages: &[Message::user("hi")], tools: &[], max_tokens: 50, live: None },
            &|_| {},
        ).await.unwrap();
        let req = recorded.await.unwrap();
        assert_eq!(req.path, "/openai/chat/completions");
        assert_eq!(req.header("cf-aig-authorization"), Some("Bearer cfut_x"));
        assert!(req.header("authorization").is_none());
        let body: Value = serde_json::from_slice(&req.body).unwrap();
        assert_eq!(body["model"], "gpt-5.1");
    }

    #[tokio::test]
    async fn other_ids_ride_the_compat_route_whole() {
        let (origin, recorded) = record(CHAT).await;
        let _ = gateway(origin).complete(
            Request { model: "workers-ai/@cf/meta/llama-4", system: "", messages: &[Message::user("hi")], tools: &[], max_tokens: 50, live: None },
            &|_| {},
        ).await.unwrap();
        let req = recorded.await.unwrap();
        assert_eq!(req.path, "/compat/chat/completions");
        let body: Value = serde_json::from_slice(&req.body).unwrap();
        assert_eq!(body["model"], "workers-ai/@cf/meta/llama-4");
    }

    #[test]
    fn placeholders_are_filled_from_the_environment() {
        std::env::set_var("CLOUDFLARE_ACCOUNT_ID", "acc");
        std::env::set_var("CLOUDFLARE_GATEWAY_ID", "gw");
        let g = gateway("https://gateway.ai.cloudflare.com/v1/<account>/<gateway>/".into());
        assert_eq!(g.base_url().unwrap(), "https://gateway.ai.cloudflare.com/v1/acc/gw");
    }
}
