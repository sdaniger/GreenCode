//! `compaction.remoteStreamingV2Enabled`: server-side compaction over the Responses endpoint.
//! OMP sends the transcript plus a `compaction_trigger` item and keeps the returned opaque
//! `compaction` item as the retained context; native keeps that item as a `Block::Other`, which the
//! Responses transport replays verbatim and the other wires ignore.
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};

use crate::config::{Config, ProviderCfg};
use crate::{Block, Message, Role};

/// Characters of a readable summary kept beside the opaque item, so the thread stays understandable.
const SUMMARY_HINT: &str = "The provider compacted this conversation server-side; the retained state follows in the next request.";

/// Whether this provider/model implements Responses streaming compaction.
pub fn supported(cfg: &Config, spec: &str) -> bool {
    let Some((pname, _)) = spec.split_once('/') else { return false };
    let Some(provider) = cfg.providers.get(pname) else { return false };
    provider.kind == "openai" && provider.base_url.contains("api.openai.com")
}

/// Runs the compaction call and returns the checkpoint message that replaces the history.
pub async fn compact(cfg: &Config, spec: &str, system: &str, messages: &[Message]) -> Result<Message> {
    let (pname, model) = spec.split_once('/').context("model must be `provider/model`")?;
    let provider = cfg.providers.get(pname).context(format!("unknown provider `{pname}`"))?;
    if provider.kind != "openai" {
        bail!("remote compaction v2 needs an OpenAI Responses provider; `{pname}` is `{}`", provider.kind);
    }
    let input = input_items(system, messages);
    let body = json!({
        "model": model,
        "input": input,
        "stream": true,
        "store": false,
        "instructions": system,
    });
    let mut body = body;
    // The trigger is the last input item: the server compacts everything before it.
    body["input"].as_array_mut().expect("input is an array").push(json!({"type": "compaction_trigger"}));
    let key = api_key(pname, provider)?;
    let url = format!("{}/responses", provider.base_url.trim_end_matches('/'));
    let response = reqwest::Client::new()
        .post(&url)
        .header("authorization", format!("Bearer {key}"))
        .json(&body)
        .send()
        .await
        .with_context(|| format!("POST {url}"))?;
    let status = response.status();
    if !status.is_success() {
        let text = response.text().await.unwrap_or_default();
        bail!("remote compaction failed ({status}): {text}");
    }
    // SSE: the retained item arrives as `response.output_item.done` with `item.type == "compaction"`.
    let stream = response.text().await?;
    let mut kept: Option<Value> = None;
    for line in stream.lines() {
        let Some(data) = line.strip_prefix("data:") else { continue };
        let data = data.trim();
        if data.is_empty() || data == "[DONE]" {
            continue;
        }
        let Ok(event) = serde_json::from_str::<Value>(data) else { continue };
        let item = match event["type"].as_str() {
            Some("response.output_item.done") => event["item"].clone(),
            // Some deployments stream the compacted context as the completed response's output.
            Some("response.completed") => event["response"]["output"]
                .as_array()
                .and_then(|items| items.iter().find(|i| i["type"] == "compaction").cloned())
                .unwrap_or(Value::Null),
            _ => continue,
        };
        if item["type"] == "compaction" {
            kept = Some(item);
        }
    }
    let item = kept.context("remote compaction returned no `compaction` item")?;
    let mut content = vec![Block::Text { text: format!("{SUMMARY_HINT}\n\n<compacted-summary>\n{SUMMARY_HINT}\n</compacted-summary>") }];
    content.push(Block::Other(item));
    Ok(Message { role: Role::User, content })
}

fn api_key(pname: &str, provider: &ProviderCfg) -> Result<String> {
    if let Some(key) = provider.api_key.as_deref().filter(|k| !k.is_empty()) {
        return Ok(key.to_string());
    }
    let env = provider.api_key_env.as_str();
    if !env.is_empty() {
        if let Ok(key) = std::env::var(env) {
            if !key.is_empty() {
                return Ok(key);
            }
        }
    }
    bail!("{pname} has no API key for remote compaction; set `api_key` or ${env}")
}

/// Responses input items for a transcript: messages stay messages, calls their function items.
fn input_items(system: &str, messages: &[Message]) -> Vec<Value> {
    let mut input = vec![json!({"type": "message", "role": "system", "content": system})];
    for message in messages {
        for block in &message.content {
            match block {
                Block::Text { text } => input.push(json!({
                    "type": "message",
                    "role": match message.role { Role::User => "user", Role::Assistant => "assistant" },
                    "content": text,
                })),
                Block::ToolUse { id, name, input: args } => {
                    input.push(json!({"type": "function_call", "call_id": id, "name": name, "arguments": args.to_string()}));
                }
                Block::ToolResult { tool_use_id, content, .. } => {
                    input.push(json!({"type": "function_call_output", "call_id": tool_use_id, "output": content}));
                }
                // A previous compaction item must be replayed unchanged.
                Block::Other(v) if v["type"] == "compaction" => input.push(v.clone()),
                Block::Other(_) => {}
            }
        }
    }
    input
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_first_party_openai_offers_streaming_compaction() {
        let mut cfg = Config::default();
        let mut openai = ProviderCfg { kind: "openai".into(), base_url: "https://api.openai.com/v1".into(), ..Default::default() };
        openai.api_key = Some("k".into());
        cfg.providers.insert("openai".into(), openai);
        cfg.providers.insert("proxy".into(), ProviderCfg { kind: "openai".into(), base_url: "http://127.0.0.1:9".into(), ..Default::default() });
        assert!(supported(&cfg, "openai/gpt-6-sol"));
        assert!(!supported(&cfg, "proxy/gpt-6-sol"));
        assert!(!supported(&cfg, "missing/gpt-6-sol"));
    }

    #[test]
    fn input_items_replay_tools_and_prior_compaction() {
        let messages = vec![
            crate::Message::user("hello"),
            Message { role: Role::Assistant, content: vec![Block::ToolUse { id: "1".into(), name: "read".into(), input: json!({"path": "a"}) }] },
            Message { role: Role::User, content: vec![Block::ToolResult { tool_use_id: "1".into(), content: "ok".into(), is_error: false }] },
            Message { role: Role::User, content: vec![Block::Other(json!({"type": "compaction", "encrypted_content": "opaque"}))] },
        ];
        let items = input_items("be brief", &messages);
        assert_eq!(items[0]["role"], json!("system"));
        assert_eq!(items[1]["content"], json!("hello"));
        assert_eq!(items[2]["type"], json!("function_call"));
        assert_eq!(items[3]["output"], json!("ok"));
        assert_eq!(items[4]["encrypted_content"], json!("opaque"));
    }

    #[tokio::test]
    async fn reads_the_compaction_item_out_of_the_stream() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 8192];
            let _ = socket.read(&mut buf).await;
            let body = "data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"message\"}}\n\ndata: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"compaction\",\"encrypted_content\":\"opaque\"}}\n\ndata: [DONE]\n\n";
            let response = format!("HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\n\r\n{body}", body.len());
            let _ = socket.write_all(response.as_bytes()).await;
            let _ = socket.shutdown().await;
        });
        let mut cfg = Config::default();
        let mut provider = ProviderCfg { kind: "openai".into(), base_url: format!("http://{addr}/v1"), ..Default::default() };
        provider.api_key = Some("k".into());
        cfg.providers.insert("openai".into(), provider);
        let messages = vec![crate::Message::user("compact me")];
        let checkpoint = compact(&cfg, "openai/gpt-6-sol", "system", &messages).await.unwrap();
        assert_eq!(checkpoint.role, Role::User);
        let item = checkpoint.content.iter().find_map(|b| match b {
            Block::Other(v) => Some(v.clone()),
            _ => None,
        }).expect("compaction item is kept");
        assert_eq!(item["encrypted_content"], json!("opaque"));
        server.await.unwrap();
    }
}
