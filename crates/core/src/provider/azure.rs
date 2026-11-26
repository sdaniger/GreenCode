//! Azure OpenAI's Responses endpoint (`providers/azure-openai-responses.ts`).
//!
//! Same request body as the OpenAI Responses API; only transport differs: a single `api-key`
//! header (never a bearer), `api-version` as a query parameter, and the base URL from the
//! `AZURE_OPENAI_RESOURCE_NAME` template `https://<resource>.openai.azure.com/openai/v1`.
//! `/responses` is not deployment-scoped, so the model name goes in the body unchanged.
use super::{responses, sse, Provider, Reply, Request};
use crate::BoxFut;
use anyhow::{bail, Result};
use serde_json::Value;

pub struct Azure {
    pub http: reqwest::Client,
    pub base: String,
    pub key: String,
    pub effort: Option<String>,
    pub wire: super::Wire,
}

impl Azure {
    /// The configured base with `<resource>` filled from the environment.
    fn base_url(&self) -> Result<String> {
        if !self.base.contains("<resource>") {
            return Ok(self.base.trim_end_matches('/').to_string());
        }
        let resource = std::env::var("AZURE_OPENAI_RESOURCE_NAME").ok().filter(|r| !r.trim().is_empty())
            .ok_or_else(|| anyhow::anyhow!("azure にはリソース名が必要です。base_url の <resource> を置き換えるか、AZURE_OPENAI_RESOURCE_NAME を設定してください"))?;
        Ok(self.base.replace("<resource>", resource.trim()).trim_end_matches('/').to_string())
    }

    fn api_version() -> String {
        std::env::var("AZURE_OPENAI_API_VERSION").ok().filter(|v| !v.trim().is_empty()).unwrap_or_else(|| "v1".into())
    }
}

impl Provider for Azure {
    fn complete<'a>(&'a self, req: Request<'a>, on_text: &'a (dyn Fn(&str) + Send + Sync)) -> BoxFut<'a, Result<Reply>> {
        Box::pin(async move {
            let body = responses::body(
                &responses::Shape {
                    model: req.model,
                    system: req.system,
                    messages: req.messages,
                    tools: req.tools,
                    effort: self.effort.as_deref(),
                    codex: false,
                },
                &self.wire,
            );
            let base = self.base_url()?;
            if self.key.is_empty() {
                bail!("azure の API キーがありません。AZURE_OPENAI_API_KEY を設定してください")
            }
            let api_version = url::form_urlencoded::byte_serialize(Self::api_version().as_bytes()).collect::<String>();
            let url = format!("{base}/responses?api-version={api_version}");
            let resp = super::send(&self.wire.retry, || self.http.post(&url).header("api-key", &self.key).json(&body)).await?;
            let mut acc = responses::Acc::default();
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
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::localserver::{serve, Request as Http};
    use crate::Message;
    use std::sync::Mutex;

    #[tokio::test]
    async fn request_uses_api_key_header_and_api_version_query() {
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
                    let body = "data: {\"type\":\"response.output_text.delta\",\"delta\":\"hi\"}\n\ndata: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":3,\"output_tokens\":1}}}\n\n";
                    crate::localserver::Response::new("200 OK", "text/event-stream", body)
                }
            }).await;
        });
        let azure = Azure {
            http: reqwest::Client::new(),
            base: origin,
            key: "azk".into(),
            effort: None,
            wire: super::super::Wire::default(),
        };
        let reply = azure
            .complete(Request { model: "gpt-5.5", system: "", messages: &[Message::user("hi")], tools: &[], max_tokens: 100, live: None }, &|_| {})
            .await
            .unwrap();
        assert!(matches!(&reply.blocks[..], [crate::Block::Text { text }] if text == "hi"));
        let req = rx.await.unwrap();
        assert_eq!(req.path, "/responses");
        assert_eq!(req.query("api-version"), Some("v1"));
        assert_eq!(req.header("api-key"), Some("azk"));
        assert!(req.header("authorization").is_none());
        let body: Value = serde_json::from_slice(&req.body).unwrap();
        assert_eq!(body["model"], "gpt-5.5");
        assert_eq!(body["stream"], true);
    }

    #[test]
    fn resource_template_comes_from_the_environment() {
        let mut azure = Azure { http: reqwest::Client::new(), base: "https://<resource>.openai.azure.com/openai/v1/".into(), key: "k".into(), effort: None, wire: super::super::Wire::default() };
        std::env::set_var("AZURE_OPENAI_RESOURCE_NAME", "myres");
        assert_eq!(azure.base_url().unwrap(), "https://myres.openai.azure.com/openai/v1");
        azure.base = "https://x.openai.azure.com/openai/v1".into();
        assert_eq!(azure.base_url().unwrap(), "https://x.openai.azure.com/openai/v1");
    }
}
