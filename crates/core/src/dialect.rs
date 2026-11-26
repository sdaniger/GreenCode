//! Owned tool-calling dialects (`tools_format`): how tools are described to a model that has no
//! provider-native function calling, and how its textual calls become native tool calls.
use crate::ToolDef;
use serde_json::{json, Value};

/// One parsed call: the tool name and its arguments.
pub type Call = (String, Value);

/// The prompt section that tells the model which dialect to write calls in.
pub fn instructions(format: &str, tools: &[ToolDef]) -> String {
    let catalog = serde_json::to_string(&tools.iter().map(|t| json!({"name": t.name, "description": t.description, "parameters": t.schema})).collect::<Vec<_>>())
        .unwrap_or_else(|_| "[]".into());
    let syntax = match format {
        "glm" => "Write each call as <tool_call>NAME<arg_key>KEY</arg_key><arg_value>VALUE</arg_value>…</tool_call>, one <arg_key>/<arg_value> pair per argument.",
        "hermes" => "Write each call as <tool_call>{\"name\": \"NAME\", \"arguments\": {…}}</tool_call>.",
        "kimi" => "Write calls as <|tool_calls_section_begin|><|tool_call_begin|>functions.NAME:0<|tool_call_argument_begin|>{…}<|tool_call_end|><|tool_calls_section_end|>.",
        "xml" => "Write each call as <tool_call><invoke name=\"NAME\"><parameter name=\"KEY\">VALUE</parameter></invoke></tool_call>.",
        "anthropic" => "Write calls as <function_calls><invoke name=\"NAME\"><parameter name=\"KEY\">VALUE</parameter></invoke></function_calls>.",
        "deepseek" => "Write calls as <｜tool▁call▁begin｜>function<｜tool▁sep｜>NAME\n```json\n{…}\n```<｜tool▁call▁end｜>.",
        "harmony" => "Write each call as <|start|>assistant<|channel|>commentary to=NAME<|message|>{…}<|call|>.",
        "qwen3" => "Write each call as <tool_call>{\"name\": \"NAME\", \"arguments\": {…}}</tool_call>.",
        "gemini" => "Write each call as ```tool_code\nNAME{…}\n```.",
        "gemma" => "Write each call as <|tool_call>call:NAME{KEY:<|\"|>VALUE<|\"|>}<tool_call|>.",
        "minimax" => "Write each call as <minimax:tool_call><invoke name=\"NAME\"><parameter name=\"KEY\">VALUE</parameter></invoke></minimax:tool_call>.",
        _ => return String::new(),
    };
    format!(
        "# Tool calling\n{syntax}\nOnly write a call when you need a tool; the harness runs it and returns the result.\n\n# Tools\n{catalog}"
    )
}

/// What marks the arguments of one tool call: the text between the wrapper and its body.
struct Shape {
    open: &'static [&'static str],
    close: &'static [&'static str],
}

fn shape(format: &str) -> Option<Shape> {
    Some(match format {
        "glm" => Shape { open: &["<tool_call>"], close: &["</tool_call>"] },
        "hermes" | "qwen3" => Shape { open: &["<tool_call>"], close: &["</tool_call>"] },
        "kimi" => Shape { open: &["<|tool_call_begin|>"], close: &["<|tool_call_end|>"] },
        "xml" => Shape { open: &["<tool_call>", "<invoke"], close: &["</tool_call>", "</invoke>"] },
        "anthropic" | "minimax" => Shape {
            open: &["<invoke", "<minimax:tool_call>"],
            close: &["</invoke>", "</minimax:tool_call>"],
        },
        "deepseek" => Shape { open: &["function<｜tool▁sep｜>"], close: &["<｜tool▁call▁end｜>"] },
        "harmony" => Shape { open: &["<|message|>"], close: &["<|call|>", "<|end|>"] },
        "gemini" => Shape { open: &["```tool_code"], close: &["```"] },
        "gemma" => Shape { open: &["<|tool_call>"], close: &["<tool_call|>"] },
        _ => return None,
    })
}

/// Arguments written as `name<arg_key>k</arg_key><arg_value>v</arg_value>…`.
fn glm_args(body: &str) -> Call {
    let body = body.trim();
    let (name, rest) = body.find('<').map_or((body, ""), |i| (&body[..i], &body[i..]));
    let pick = |tag: &str| {
        let open = format!("<{tag}>");
        let close = format!("</{tag}>");
        rest.match_indices(&open)
            .filter_map(|(i, _)| {
                let start = i + open.len();
                let end = rest[start..].find(&close)? + start;
                Some(rest[start..end].to_string())
            })
            .collect::<Vec<_>>()
    };
    let keys = pick("arg_key");
    let values = pick("arg_value");
    let mut args = serde_json::Map::new();
    for (key, value) in keys.into_iter().zip(values) {
        args.insert(key.trim().to_string(), json!(value));
    }
    (name.trim().trim_start_matches("functions.").to_string(), Value::Object(args))
}

/// `{"name": …, "arguments"|"parameters"|"input": …}`.
fn json_call(body: &str) -> Option<Call> {
    let start = body.find('{')?;
    let end = body.rfind('}')?;
    let value: Value = serde_json::from_str(&body[start..=end]).ok()?;
    let name = value["name"].as_str().or_else(|| value["tool"].as_str())?.to_string();
    let args = value
        .get("arguments")
        .or_else(|| value.get("parameters"))
        .or_else(|| value.get("input"))
        .cloned()
        .unwrap_or_else(|| json!({}));
    Some((name.trim_start_matches("functions.").to_string(), args))
}

/// `<invoke name="n"><parameter name="k">v</parameter>…</invoke>`.
fn invoke_call(body: &str) -> Option<Call> {
    let name_at = body.find("name=\"")? + "name=\"".len();
    let name_end = name_at + body[name_at..].find('"')?;
    let name = &body[name_at..name_end];
    let mut args = serde_json::Map::new();
    let mut rest = &body[name_end..];
    while let Some(at) = rest.find("<parameter") {
        rest = &rest[at..];
        let Some(tag_end) = rest.find('>') else { break };
        let tag = &rest[..tag_end];
        let value_start = tag_end + 1;
        let Some(value_end) = rest[value_start..].find("</parameter>") else { break };
        let key = tag.find("name=\"").and_then(|i| {
            let s = &tag[i + 6..];
            s.find('"').map(|e| s[..e].to_string())
        })?;
        args.insert(key, json!(rest[value_start..value_start + value_end]));
        rest = &rest[value_start + value_end + "</parameter>".len()..];
    }
    Some((name.trim_start_matches("functions.").to_string(), Value::Object(args)))
}

/// ```tool_code fenced call: `NAME{…}` or a JSON object.
fn code_call(body: &str) -> Option<Call> {
    let body = body.trim();
    if body.starts_with('{') {
        return json_call(body);
    }
    let (name, rest) = body.split_once(|c: char| c == '{' || c == '(')?;
    Some((name.trim().to_string(), loose_args(rest)))
}

/// `{key: value, …}` with optional `<|"|>` quoting (gemma) or plain JSON.
fn loose_args(text: &str) -> Value {
    let body = text.trim().trim_start_matches(['{', '(']).trim_end_matches(['}', ')']).replace("<|\"|>", "\"");
    if let Ok(value) = serde_json::from_str::<Value>(&format!("{{{body}}}")) {
        return value;
    }
    let mut args = serde_json::Map::new();
    for pair in body.split(',') {
        let Some((key, value)) = pair.split_once(':') else { continue };
        args.insert(key.trim().trim_matches('"').to_string(), json!(value.trim().trim_matches('"')));
    }
    Value::Object(args)
}

/// Parses every tool call the model wrote for `format`.
pub fn parse(format: &str, text: &str) -> Vec<Call> {
    let Some(shape) = shape(format) else { return Vec::new() };
    let mut out = Vec::new();
    let mut rest = text;
    while let Some((at, marker)) = shape.open.iter().filter_map(|m| rest.find(m).map(|i| (i, *m))).min_by_key(|(i, _)| *i) {
        let before = &rest[..at];
        rest = &rest[at + marker.len()..];
        let close = shape.close.iter().filter_map(|m| rest.find(m).map(|i| (i, *m))).min_by_key(|(i, _)| *i);
        let Some((end, close_marker)) = close else { break };
        let body = &rest[..end];
        rest = &rest[end + close_marker.len()..];
        let call = match format {
            "glm" => Some(glm_args(body)),
            "hermes" | "qwen3" => json_call(body),
            // `NAME\n```json\n{…}\n````: the name precedes the JSON, which holds only the arguments.
            "deepseek" => (|| {
                let body = body.trim();
                let (name, _) = body.split_once(|c: char| c == '{')?;
                let name = name.lines().next()?.trim();
                let start = body.find('{')?;
                let args = loose_args(&body[start..=body.rfind('}')?]);
                Some((name.to_string(), args))
            })(),
            "kimi" => (|| {
                // `functions.NAME:0<|tool_call_argument_begin|>{…}`
                let body = body.trim_start_matches("<|tool_calls_section_begin|>");
                let (head, args) = body.split_once("<|tool_call_argument_begin|>")?;
                let name = head.trim().trim_start_matches("functions.").split(':').next()?.to_string();
                Some((name, serde_json::from_str(args.trim()).unwrap_or_else(|_| json!({}))))
            })(),
            "xml" | "anthropic" | "minimax" => invoke_call(body),
            "harmony" => (|| {
                // `assistant<|channel|>commentary to=NAME` precedes `<|message|>`; the body is the JSON args.
                let name = before.rsplit("to=").next()?.split_whitespace().next()?.to_string();
                Some((name, loose_args(body)))
            })(),
            "gemini" => code_call(body),
            "gemma" => (|| {
                let body = body.trim().strip_prefix("call:").unwrap_or(body);
                let (name, rest) = body.split_once('{')?;
                Some((name.trim().to_string(), loose_args(&format!("{{{rest}"))))
            })(),
            _ => None,
        };
        if let Some((name, args)) = call.filter(|(name, _)| !name.is_empty()) {
            out.push((name, args));
        }
    }
    out
}

/// Renders one call in `format`, so a transcript round-trips through `parse`.
pub fn render(format: &str, name: &str, args: &Value) -> String {
    let pairs: Vec<(String, &Value)> = args.as_object().map(|o| o.iter().map(|(k, v)| (k.clone(), v)).collect()).unwrap_or_default();
    let text = |v: &Value| v.as_str().map(str::to_string).unwrap_or_else(|| v.to_string());
    match format {
        "glm" => format!(
            "<tool_call>{name}{}</tool_call>",
            pairs.iter().map(|(k, v)| format!("<arg_key>{k}</arg_key><arg_value>{}</arg_value>", text(v))).collect::<String>()
        ),
        "hermes" | "qwen3" => format!("<tool_call>{}</tool_call>", json!({"name": name, "arguments": args})),
        "kimi" => format!("<|tool_calls_section_begin|><|tool_call_begin|>functions.{name}:0<|tool_call_argument_begin|>{args}<|tool_call_end|><|tool_calls_section_end|>"),
        "xml" => format!("<tool_call>{}</tool_call>", invoke(name, &pairs, &text)),
        "anthropic" => format!("<function_calls>{}</function_calls>", invoke(name, &pairs, &text)),
        "minimax" => format!("<minimax:tool_call>{}</minimax:tool_call>", invoke(name, &pairs, &text)),
        "deepseek" => format!("<｜end_of_query｜>function<｜tool▁sep｜>{name}\n```json\n{args}\n```<｜tool▁call▁end｜>"),
        "harmony" => format!("<|start|>assistant<|channel|>commentary to={name}<|message|>{args}<|call|>"),
        "gemini" => format!("```tool_code\n{name}{args}\n```"),
        "gemma" => format!(
            "<|tool_call>call:{name}{{{}}}<tool_call|>",
            pairs.iter().map(|(k, v)| match v {
                Value::String(s) => format!("{k}:<|\"|>{s}<|\"|>"),
                other => format!("{k}:{other}"),
            }).collect::<Vec<_>>().join(",")
        ),
        _ => String::new(),
    }
}

fn invoke(name: &str, pairs: &[(String, &Value)], text: &impl Fn(&Value) -> String) -> String {
    format!(
        "<invoke name=\"{name}\">{}</invoke>",
        pairs.iter().map(|(k, v)| format!("<parameter name=\"{k}\">{}</parameter>", text(v))).collect::<String>()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tools() -> Vec<ToolDef> {
        vec![ToolDef { name: "read".into(), description: "Read a file".into(), schema: json!({"type":"object","properties":{"path":{"type":"string"}}}) }]
    }

    #[test]
    fn every_dialect_round_trips_its_calls() {
        let args = json!({"path": "src/main.rs", "limit": 20});
        for format in ["glm", "hermes", "kimi", "xml", "anthropic", "deepseek", "harmony", "qwen3", "gemini", "gemma", "minimax"] {
            let text = render(format, "read", &args);
            let calls = parse(format, &format!("some prose\n{text}\nmore prose"));
            assert_eq!(calls.len(), 1, "{format}: {text}");
            let (name, parsed) = &calls[0];
            assert_eq!(name, "read", "{format}");
            assert_eq!(parsed["path"], "src/main.rs", "{format}: {parsed}");
        }
        assert!(parse("glm", "no calls here").is_empty());
        assert!(parse("native", "<tool_call>x</tool_call>").is_empty());
    }

    #[test]
    fn instructions_carry_the_catalog_and_the_syntax() {
        let text = instructions("hermes", &tools());
        assert!(text.contains("\"read\"") && text.contains("<tool_call>"));
        assert!(instructions("native", &tools()).is_empty());
    }
}

