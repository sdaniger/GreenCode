//! Installation information for supported external agents.

pub struct AgentInfo {
    pub name: &'static str,
    pub binary: &'static str,
    pub install: &'static str,
    pub docs: &'static str,
}

pub const AGENTS: &[AgentInfo] = &[
    AgentInfo { name: "opencode", binary: "opencode", install: "curl -fsSL https://opencode.ai/install | bash", docs: "https://opencode.ai/docs/" },
    AgentInfo { name: "pi", binary: "pi", install: "npm install -g @mariozechner/pi-coding-agent", docs: "https://github.com/badlogic/pi-mono/tree/main/packages/coding-agent" },
    AgentInfo { name: "omp", binary: "omp", install: "bun install -g @oh-my-pi/pi-coding-agent", docs: "https://github.com/can1357/oh-my-pi" },
    AgentInfo { name: "codex", binary: "codex", install: "npm install -g @openai/codex", docs: "https://developers.openai.com/codex/" },
    AgentInfo { name: "claude", binary: "claude", install: "curl -fsSL https://claude.ai/install.sh | bash", docs: "https://code.claude.com/docs/" },
    AgentInfo { name: "kiro", binary: "kiro-cli", install: "curl -fsSL https://cli.kiro.dev/install | bash", docs: "https://kiro.dev/docs/cli/" },
    AgentInfo { name: "grok", binary: "grok", install: "", docs: "https://github.com/superagent-ai/grok-cli" },
    AgentInfo { name: "hermes", binary: "hermes", install: "curl -fsSL https://hermes-agent.nousresearch.com/install.sh | bash", docs: "https://hermes-agent.nousresearch.com/docs/" },
];

/// Find the first version-shaped sequence in a command's output.
pub fn parse_version(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    for start in 0..bytes.len() {
        if !bytes[start].is_ascii_digit() { continue; }
        let mut end = start;
        while end < bytes.len() && bytes[end].is_ascii_digit() { end += 1; }
        if bytes.get(end) != Some(&b'.') || !bytes.get(end + 1).is_some_and(u8::is_ascii_digit) { continue; }
        end += 2;
        while end < bytes.len() && (bytes[end].is_ascii_alphanumeric() || matches!(bytes[end], b'_' | b'.' | b'-')) { end += 1; }
        return Some(text[start..end].to_string());
    }
    None
}

pub async fn version(binary: &str) -> Option<String> {
    let exe = crate::config::executable(binary)?;
    let mut command = tokio::process::Command::new(exe);
    command.arg("--version").stdin(std::process::Stdio::null()).kill_on_drop(true);
    let output = tokio::time::timeout(std::time::Duration::from_secs(5), command.output()).await.ok()?.ok()?;
    parse_version(&format!("{}\n{}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr)))
}

#[cfg(test)]
mod tests {
    use super::{parse_version, AGENTS};

    #[test]
    fn parses_versions_and_registry_names_are_unique() {
        for (text, expected) in [("opencode 2.2.6", "2.2.6"), ("1.5.2", "1.5.2"), ("codex-cli 0.167.2", "0.167.2"), ("claude 2.1.322 (Claude Code)", "2.1.322"), ("Hermes Agent v0.23.1+4166.g72c8803 (2026.9.24)", "0.23.1")] {
            assert_eq!(parse_version(text).as_deref(), Some(expected));
        }
        assert_eq!(parse_version("garbage"), None);
        let names: std::collections::HashSet<_> = AGENTS.iter().map(|a| a.name).collect();
        assert_eq!(names.len(), AGENTS.len());
    }
}
