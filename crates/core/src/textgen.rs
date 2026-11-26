//! One-shot text generation (thread titles, commit messages, PR text, branch names) with a configured model.
//! Reuses the normal provider request path; nothing is streamed to the UI and nothing is kept in a session.

use crate::{config::Config, provider::{self, Request}, Block, Message};
use anyhow::{anyhow, Result};
use std::time::Duration;

const TIMEOUT: Duration = Duration::from_secs(45);
/// Diffs sent to the model are cut here (characters), enough for a message without a huge request.
const DIFF_LIMIT: usize = 20_000;

/// The model's reply to `prompt` under `system`, trimmed. Errors on timeout or an empty reply.
pub async fn generate(cfg: &Config, model: &str, system: &str, prompt: &str, max_tokens: u32) -> Result<String> {
    if crate::tiny::local_spec(model).is_some() {
        return crate::tiny::generate(cfg, model, system, prompt, max_tokens).await;
    }
    let (provider, model) = provider::make(cfg, model)?;
    let messages = [Message::user(prompt)];
    let req = Request { model: &model, system, messages: &messages, tools: &[], max_tokens, live: None };
    let reply = tokio::time::timeout(TIMEOUT, provider.complete(req, &|_| {})).await.map_err(|_| anyhow!("テキスト生成がタイムアウトしました"))??;
    let text: String = reply.blocks.iter().filter_map(|b| if let Block::Text { text } = b { Some(text.as_str()) } else { None }).collect();
    let text = text.trim();
    anyhow::ensure!(!text.is_empty(), "テキスト生成の応答が空でした");
    Ok(text.to_string())
}

fn clip(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

/// A short thread title from the first exchange. `instructions` are the user's extra wishes.
pub async fn title(cfg: &Config, model: &str, instructions: &str, user: &str, assistant: &str) -> Result<String> {
    let system = format!("Write a short title (at most 40 characters, no quotes, no trailing punctuation) for this coding conversation, in the user's language. Reply with the title only.{}", extra(instructions));
    let prompt = format!("User:\n{}\n\nAssistant:\n{}", clip(user, 2000), clip(assistant, 2000));
    Ok(clean_title(&generate(cfg, model, &system, &prompt, 60).await?))
}

fn extra(instructions: &str) -> String {
    match instructions.trim() {
        "" => String::new(),
        i => format!("\nAdditional instructions: {i}"),
    }
}

/// A brief recap of where the session stands, shown after the terminal sits idle.
pub async fn recap(cfg: &Config, model: &str, user: &str, assistant: &str) -> Result<String> {
    let system = "Recap the conversation for the operator in at most three short bullets: what was asked, what changed, what is still open. Write in the user's language. Reply with the recap only.";
    let prompt = format!("User:\n{}\n\nAssistant:\n{}", clip(user, 2000), clip(assistant, 3000));
    let text = generate(cfg, model, system, &prompt, 300).await?;
    let recap = text.lines().map(str::trim).filter(|l| !l.is_empty()).collect::<Vec<_>>().join("\n");
    anyhow::ensure!(!recap.is_empty(), "recap was empty");
    Ok(recap)
}

/// First non-empty line without surrounding quotes or `Title:` prefix, at most 60 characters.
fn clean_title(raw: &str) -> String {
    let line = raw.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("");
    let line = line.strip_prefix("Title:").or_else(|| line.strip_prefix("タイトル:")).unwrap_or(line).trim();
    clip(line.trim_matches(|c| matches!(c, '"' | '\'' | '`' | '「' | '」' | '*' | '#' | ' ')), 60)
}

/// Up to `count` short guesses for the operator's next message, most likely first.
pub async fn next_prompt(cfg: &Config, model: &str, user: &str, assistant: &str, count: usize) -> Result<Vec<String>> {
    let count = count.clamp(1, 4);
    let system = format!("Predict the operator's next short reply to this coding assistant. Reply with at most {count} lines, most likely first, without numbering or quotes.");
    let prompt = format!("User:\n{}\n\nAssistant:\n{}", clip(user, 1500), clip(assistant, 1500));
    let text = generate(cfg, model, &system, &prompt, 120).await?;
    let predictions = prediction_lines(&text, count);
    anyhow::ensure!(!predictions.is_empty(), "予測の応答が空でした");
    Ok(predictions)
}

fn prediction_lines(raw: &str, count: usize) -> Vec<String> {
    raw.lines()
        .map(|line| line.trim().trim_start_matches(|c: char| c.is_ascii_digit() || matches!(c, '.' | ')' | '-')).trim().trim_matches(['"', '\'']).to_owned())
        .filter(|line| !line.is_empty())
        .take(count)
        .collect()
}

/// A commit message (subject, optionally a body) for a staged or working-tree diff.
pub async fn commit_message(cfg: &Config, model: &str, diff: &str) -> Result<String> {
    let system = "Write a git commit message for this diff: an imperative subject line of at most 72 characters, optionally a blank line and a short body. Reply with the message only, no code fences.";
    let text = generate(cfg, model, system, &clip(diff, DIFF_LIMIT), 400).await?;
    Ok(strip_fence(&text))
}

/// A pull request (title, body) for the commits and diff of a branch.
pub async fn pr_text(cfg: &Config, model: &str, log: &str, diff: &str) -> Result<(String, String)> {
    let system = "Write a GitHub pull request for these changes. First line: the title (at most 72 characters). Then a blank line, then a Markdown description (what and why, short bullet points). Reply with that only, no code fences.";
    let prompt = format!("Commits:\n{}\n\nDiff:\n{}", clip(log, 4000), clip(diff, DIFF_LIMIT));
    let text = strip_fence(&generate(cfg, model, system, &prompt, 800).await?);
    let (title, body) = text.split_once('\n').unwrap_or((&text, ""));
    Ok((clean_title(title), body.trim().to_string()))
}

/// A branch name under `prefix` (already normalized: a trailing `/` or empty) for a task described by `prompt`.
pub async fn branch_name(cfg: &Config, model: &str, prefix: &str, prompt: &str) -> Result<String> {
    let system = "Suggest a git branch name for this task: 2 to 5 lowercase English words joined by hyphens. Reply with the name only.";
    let text = generate(cfg, model, system, &clip(prompt, 2000), 30).await?;
    let slug = slug(&text);
    anyhow::ensure!(!slug.is_empty(), "ブランチ名を生成できませんでした");
    Ok(format!("{prefix}{slug}"))
}

/// Lowercase ASCII words joined by `-`, at most 40 characters; a leading `prefix/` is dropped.
pub fn slug(raw: &str) -> String {
    let line = raw.lines().next().unwrap_or("").trim();
    let line = line.rsplit('/').next().unwrap_or(line);
    let mut out = String::new();
    for c in line.chars() {
        match c.to_ascii_lowercase() {
            c if c.is_ascii_alphanumeric() => out.push(c),
            _ if !out.is_empty() && !out.ends_with('-') => out.push('-'),
            _ => {}
        }
    }
    clip(out.trim_end_matches('-'), 40).trim_end_matches('-').to_string()
}

fn strip_fence(s: &str) -> String {
    let s = s.trim();
    match s.strip_prefix("```") {
        Some(rest) => {
            let rest = rest.split_once('\n').map_or("", |(_, r)| r);
            rest.trim_end().strip_suffix("```").unwrap_or(rest).trim().to_string()
        }
        None => s.to_string(),
    }
}

/// The model that names a session: `first-message` names nothing, `fork` uses the active model, `tiny` the
/// `smol` role when configured.
pub fn title_model(cfg: &Config, active: &str, generator: crate::config::TitleGenerator) -> Option<String> {
    use crate::config::TitleGenerator;
    match generator {
        TitleGenerator::FirstMessage => None,
        TitleGenerator::Fork => Some(active.to_string()),
        TitleGenerator::Tiny => Some(cfg.model_roles.get("smol").cloned().unwrap_or_else(|| active.to_string())),
    }
}

/// Whether a generated title should be regenerated after `prompts` operator prompts.
pub fn refresh_title(prompts: u32, every: u32, generated: bool) -> bool {
    generated && every > 0 && prompts > 0 && prompts % every == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ProviderCfg;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[test]
    fn cleaning_and_slugs() {
        assert_eq!(clean_title("\n「ログイン修正」\nextra"), "ログイン修正");
        assert_eq!(clean_title("Title: \"Fix login\""), "Fix login");
        assert_eq!(slug("greencode/Fix the Login_bug!\nmore"), "fix-the-login-bug");
        assert_eq!(slug("work/Fix login"), "fix-login");
        assert_eq!(slug("日本語"), "");
        assert_eq!(strip_fence("```\nfeat: x\n\nbody\n```"), "feat: x\n\nbody");
    }

    #[test]
    fn title_generator_picks_models_and_refresh_interval() {
        use crate::config::TitleGenerator;
        let mut cfg = Config::default();
        cfg.model_roles.insert("smol".into(), "openai/tiny".into());
        assert_eq!(title_model(&cfg, "anthropic/x", TitleGenerator::FirstMessage), None);
        assert_eq!(title_model(&cfg, "anthropic/x", TitleGenerator::Fork).as_deref(), Some("anthropic/x"));
        assert_eq!(title_model(&cfg, "anthropic/x", TitleGenerator::Tiny).as_deref(), Some("openai/tiny"));
        cfg.model_roles.clear();
        assert_eq!(title_model(&cfg, "anthropic/x", TitleGenerator::Tiny).as_deref(), Some("anthropic/x"));
        assert!(refresh_title(4, 4, true));
        assert!(!refresh_title(4, 4, false));
        assert!(!refresh_title(3, 4, true));
        assert!(!refresh_title(4, 0, true));
    }

    #[test]
    fn prediction_lines_trim_numbering_quotes_and_count() {
        assert_eq!(prediction_lines("1. \"run the tests\"\n2) fix it\n\n3. ship", 2), ["run the tests", "fix it"]);
        assert_eq!(prediction_lines("- yes\n- no", 4), ["yes", "no"]);
        assert!(prediction_lines("  \n", 2).is_empty());
    }

    /// Serves one OpenAI-style streamed reply and returns the request body it received.
    async fn serve(reply: &'static str) -> (String, tokio::task::JoinHandle<String>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut data = Vec::new();
            let body = loop {
                let mut buf = [0; 4096];
                let n = stream.read(&mut buf).await.unwrap();
                assert!(n > 0);
                data.extend_from_slice(&buf[..n]);
                if let Some(end) = data.windows(4).position(|w| w == b"\r\n\r\n") {
                    let headers = String::from_utf8_lossy(&data[..end]).to_lowercase();
                    let length: usize = headers.lines().find_map(|l| l.strip_prefix("content-length: ")).unwrap().parse().unwrap();
                    if data.len() >= end + 4 + length {
                        break String::from_utf8_lossy(&data[end + 4..]).into_owned();
                    }
                }
            };
            let sse = format!("data: {}\n\ndata: [DONE]\n\n", serde_json::json!({"choices": [{"delta": {"content": reply}, "finish_reason": "stop"}]}));
            stream.write_all(format!("HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{sse}", sse.len()).as_bytes()).await.unwrap();
            body
        });
        (base, server)
    }

    #[tokio::test]
    async fn generates_title_and_pr_text_from_a_local_server() {
        let mut cfg = Config::default();
        let (base, server) = serve("\"Fix login\"").await;
        cfg.providers.insert("mock".into(), ProviderCfg { kind: "openai".into(), base_url: base, ..Default::default() });
        let t = title(&cfg, "mock/m", "日本語で", "fix the login", "done").await.unwrap();
        assert_eq!(t, "Fix login");
        let body = server.await.unwrap();
        assert!(body.contains("fix the login") && body.contains("日本語で"));

        let (base, server) = serve("Add login\n\n- fixes it").await;
        cfg.providers.get_mut("mock").unwrap().base_url = base;
        let (t, b) = pr_text(&cfg, "mock/m", "abc fix", "diff").await.unwrap();
        server.await.unwrap();
        assert_eq!((t.as_str(), b.as_str()), ("Add login", "- fixes it"));

        let (base, server) = serve("\"Fix the Login_bug!\"").await;
        cfg.providers.get_mut("mock").unwrap().base_url = base;
        assert_eq!(
            branch_name(&cfg, "mock/m", "greencode/", "fix the login").await.unwrap(),
            "greencode/fix-the-login-bug"
        );
        server.await.unwrap();
    }
}
