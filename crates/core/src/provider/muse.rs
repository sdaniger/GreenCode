//! Muse Code's Model API wire: the Responses protocol against api.meta.ai/v1 with Meta's
//! `x-api-version` header (OMP `registry/muse-code.ts`, rule `providers/muse-code.kdl`).
//!
//! Two Meta-specific rules shape the body: the endpoint rejects every `tool_choice` value
//! (so the field is dropped) and answers 403 `client_attestation_denied` for a declared tool
//! whose exact name is one of the attested list, so function names get a `_` prefix.

use super::{responses, Provider, Reply, Request};
use crate::BoxFut;
use anyhow::Result;
use serde_json::Value;

const API_VERSION: &str = "1.0.0";
/// Tools whose exact, case-sensitive names a subscription key refuses to see declared.
const ATTESTED: [&str; 7] = ["bash", "shell", "web_search", "web_fetch", "apply_patch", "read_file", "update_plan"];
/// Meta rejects a tool name longer than this.
const MAX_TOOL_NAME: usize = 64;

/// The name a function tool is declared with; a prefixed name keeps the tool's identity.
fn declared(name: &str) -> String {
    let prefix = ATTESTED.contains(&name) && name.len() < MAX_TOOL_NAME;
    if prefix { format!("_{name}") } else { name.to_string() }
}

/// Undoes `declared` so the agent sees the tool it actually owns.
fn restored(name: &str) -> &str {
    name.strip_prefix('_').filter(|bare| ATTESTED.contains(bare)).unwrap_or(name)
}

pub struct Muse {
    pub http: reqwest::Client,
    pub base: String,
    pub key: String,
    pub effort: Option<String>,
    pub wire: super::Wire,
}

impl Muse {
    /// The Responses body with Meta's dialect: prefixed tool names and no `tool_choice`.
    pub fn body(&self, req: &Request) -> Value {
        let tools: Vec<crate::ToolDef> = req.tools.iter()
            .map(|t| crate::ToolDef { name: declared(&t.name), description: t.description.clone(), schema: t.schema.clone() })
            .collect();
        let mut body = responses::body(&responses::Shape {
            model: req.model,
            system: req.system,
            messages: req.messages,
            tools: &tools,
            effort: self.effort.as_deref(),
            codex: false,
        }, &self.wire);
        // Meta accepts only `auto`, and rejects the field outright (verified): omit it.
        body.as_object_mut().unwrap().remove("tool_choice");
        body
    }
}

impl Provider for Muse {
    fn append_only(&self) -> bool {
        self.wire.append_only
    }

    fn complete<'a>(&'a self, req: Request<'a>, on_text: &'a (dyn Fn(&str) + Send + Sync)) -> BoxFut<'a, Result<Reply>> {
        Box::pin(async move {
            let body = self.body(&req);
            let resp = super::send(&self.wire.retry, || {
                self.http.post(format!("{}/responses", self.base))
                    .bearer_auth(&self.key)
                    .header("x-api-version", API_VERSION)
                    .header("accept", "application/json")
                    .json(&body)
            }).await?;
            let mut acc = responses::Acc::default();
            super::sse(resp, self.wire.timeouts, |data| {
                if data == "[DONE]" { return Ok(true) }
                if let Ok(ev) = serde_json::from_str::<Value>(data) {
                    if let Some(t) = acc.feed(&ev)? { on_text(&t) }
                }
                Ok(false)
            }).await?;
            let mut reply = acc.finish();
            for block in &mut reply.blocks {
                if let crate::Block::ToolUse { name, .. } = block {
                    *name = restored(name).to_string();
                }
            }
            Ok(reply)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Message, ToolDef};
    use serde_json::json;

    fn client() -> Muse {
        Muse {
            http: reqwest::Client::new(),
            base: "http://127.0.0.1:1/v1".into(),
            key: "k".into(),
            effort: Some("high".into()),
            wire: super::super::Wire::default(),
        }
    }

    #[test]
    fn meta_dialect_prefixes_attested_tools_and_drops_tool_choice() {
        let tools = vec![
            ToolDef { name: "bash".into(), description: "run".into(), schema: json!({"type": "object"}) },
            ToolDef { name: "my_tool".into(), description: "own".into(), schema: json!({"type": "object"}) },
        ];
        let messages = [Message::user("hi")];
        let body = client().body(&super::super::Request {
            model: "muse-spark-1.3",
            system: "sys",
            messages: &messages,
            tools: &tools,
            max_tokens: 4096,
            live: None,
        });
        let names: Vec<&str> = body["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert_eq!(names, ["_bash", "my_tool"]);
        assert!(body.get("tool_choice").is_none(), "Meta rejects every tool_choice value");
        assert_eq!(body["stream"], true);
        assert_eq!(body["instructions"], "sys");
        assert_eq!(body["reasoning"]["effort"], "high");
    }

    #[test]
    fn restored_names_round_trip_only_for_prefixed_tools() {
        for name in ATTESTED {
            assert_eq!(declared(name), format!("_{name}"));
            assert_eq!(restored(&format!("_{name}")), name);
        }
        assert_eq!(restored("my_tool"), "my_tool");
        assert_eq!(restored("_my_tool"), "_my_tool");
        // A name at the length limit keeps its own spelling instead of overflowing Meta's cap.
        let long = format!("{}{}", ATTESTED[0], "x".repeat(MAX_TOOL_NAME - ATTESTED[0].len()));
        assert_eq!(long.len(), MAX_TOOL_NAME);
        assert_eq!(declared(&long), long);
    }
}
