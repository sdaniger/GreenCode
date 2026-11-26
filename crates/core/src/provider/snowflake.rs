//! Snowflake Cortex REST (`registry/snowflake.ts`): Claude on the Anthropic Messages wire, the
//! `openai-*` ids on the OpenAI Chat wire one path deeper (`{account}/api/v2/cortex/v1`).
//!
//! The account comes from the stored sign-in credential, else `SNOWFLAKE_ACCOUNT`; a plain
//! `SNOWFLAKE_PAT` with no account needs the preset template's `<account>` filled from the env.
use super::anthropic::Anthropic;
use super::openai::OpenAi;
use super::{Provider, Reply, Request};
use crate::BoxFut;
use anyhow::{bail, Result};

pub struct Snowflake {
    pub http: reqwest::Client,
    /// The preset origin, `https://snowflake-account.invalid/api/v2/cortex` or a configured one.
    pub base: String,
    /// The stored OAuth credential's account URL, when the user signed in.
    pub account: Option<String>,
    pub key: String,
    pub effort: Option<String>,
    pub wire: super::Wire,
}

/// The preset origin (`snowflake-account.invalid`, RFC 6761) replaced by the account URL.
const PLACEHOLDER: &str = "https://snowflake-account.invalid";

impl Snowflake {
    /// `(cortex base, token)`: the OAuth account, else the preset template with the account filled.
    fn resolve(&self) -> Result<(String, String)> {
        let token = match self.key.trim() {
            "" => bail!("snowflake のトークンがありません。SNOWFLAKE_PAT を設定するか、/login snowflake を実行してください"),
            key => key,
        };
        if !self.base.starts_with(PLACEHOLDER) && !self.base.contains("<account>") {
            return Ok((self.base.trim_end_matches('/').to_string(), token.into()));
        }
        let account = match &self.account {
            Some(url) => url.trim_end_matches('/').to_string(),
            None => crate::login::snowflake::account_url(&std::env::var("SNOWFLAKE_ACCOUNT").unwrap_or_default())?,
        };
        let rest = self.base.strip_prefix(PLACEHOLDER).unwrap_or("/api/v2/cortex");
        let rest = rest.replace("<account>", "");
        Ok((format!("{account}{rest}"), token.into()))
    }
}

impl Provider for Snowflake {
    fn complete<'a>(&'a self, req: Request<'a>, on_text: &'a (dyn Fn(&str) + Send + Sync)) -> BoxFut<'a, Result<Reply>> {
        Box::pin(async move {
            let (base, key) = self.resolve()?;
            // Cortex documents a 16,384-token output ceiling.
            let req = Request { max_tokens: req.max_tokens.min(16_384), ..req };
            if req.model.starts_with("openai-") {
                let openai = OpenAi {
                    http: self.http.clone(),
                    base: format!("{base}/v1"),
                    key,
                    copilot: false,
                    effort: self.effort.clone(),
                    wire: self.wire.clone(),
                };
                return openai.complete(req, on_text).await;
            }
            let anthropic = Anthropic {
                http: self.http.clone(),
                base,
                key,
                oauth: None,
                effort: self.effort.clone(),
                wire: self.wire.clone(),
            };
            anthropic.complete(req, on_text).await
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::localserver::{serve, Request as Http};
    use crate::{Block, Message};
    use serde_json::Value;
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

    fn snowflake(base: String) -> Snowflake {
        Snowflake { http: reqwest::Client::new(), base, account: Some("https://acme.snowflakecomputing.com".into()), key: "pat".into(), effort: None, wire: super::super::Wire::default() }
    }

    #[tokio::test]
    async fn claude_requests_hit_the_cortex_messages_path_with_the_bearer() {
        let (origin, recorded) = record("data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"hi\"}}\n\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\ndata: {\"type\":\"message_stop\"}\n").await;
        let reply = snowflake(format!("{origin}/api/v2/cortex")).complete(
            Request { model: "claude-sonnet-4-5", system: "", messages: &[Message::user("hi")], tools: &[], max_tokens: 99_999, live: None },
            &|_| {},
        ).await.unwrap();
        assert!(matches!(&reply.blocks[..], [Block::Text { text }] if text == "hi"));
        let req = recorded.await.unwrap();
        assert_eq!(req.path, "/api/v2/cortex/v1/messages");
        assert_eq!(req.header("authorization"), Some("Bearer pat"));
        let body: Value = serde_json::from_slice(&req.body).unwrap();
        assert_eq!(body["max_tokens"], 16_384);
    }

    #[tokio::test]
    async fn openai_ids_use_the_v1_chat_path() {
        let (origin, recorded) = record("data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\ndata: [DONE]\n").await;
        let _ = snowflake(format!("{origin}/api/v2/cortex")).complete(
            Request { model: "openai-gpt-5.1", system: "", messages: &[Message::user("hi")], tools: &[], max_tokens: 10, live: None },
            &|_| {},
        ).await.unwrap();
        let req = recorded.await.unwrap();
        assert_eq!(req.path, "/api/v2/cortex/v1/chat/completions");
        assert_eq!(req.header("authorization"), Some("Bearer pat"));
    }

    #[tokio::test]
    async fn the_preset_placeholder_becomes_the_signed_in_account() {
        let (origin, recorded) = record("data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\ndata: [DONE]\n").await;
        // The recorded account URL is what a sign-in stores; the placeholder only supplies the path.
        let s = Snowflake { base: "https://snowflake-account.invalid/api/v2/cortex".into(), account: Some(origin.clone()), key: "pat".into(), http: reqwest::Client::new(), effort: None, wire: super::super::Wire::default() };
        let _ = s.complete(Request { model: "openai-gpt-5", system: "", messages: &[Message::user("hi")], tools: &[], max_tokens: 10, live: None }, &|_| {}).await.unwrap();
        let req = recorded.await.unwrap();
        assert!(req.path.starts_with("/api/v2/cortex/v1/chat/completions"));
    }

    #[test]
    fn a_config_template_fills_the_account_from_the_env() {
        std::env::set_var("SNOWFLAKE_ACCOUNT", "acme");
        let s = Snowflake { base: "https://<account>/api/v2/cortex".into(), account: None, key: "pat".into(), http: reqwest::Client::new(), effort: None, wire: super::super::Wire::default() };
        assert_eq!(s.resolve().unwrap().0, "https://acme.snowflakecomputing.com/api/v2/cortex");
    }
}
