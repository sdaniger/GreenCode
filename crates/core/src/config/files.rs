//! Settings for read, edit, grep and image handling; flattened into `Config`.
use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Deserialize, Serialize, Clone, Debug, Default, PartialEq)]
#[serde(default)]
pub struct FileOpts {
    pub images: ImagesCfg,
    pub edit: EditCfg,
    pub read: ReadCfg,
    pub grep: GrepCfg,
    pub find: FindCfg,
}

#[derive(Deserialize, Serialize, Clone, Debug, Default, PartialEq)]
#[serde(default)]
pub struct FindCfg {
    /// Model that verifies `find` passages before the judge role; unset keeps verification on the judge role.
    pub verify_model: Option<String>,
}

#[derive(Deserialize, Serialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct ImagesCfg {
    /// Replace every image block with a text note before it reaches a provider.
    pub block_images: bool,
    /// Describe images with a vision model for models that cannot read them, instead of dropping them.
    pub describe_for_text_models: bool,
    /// Shrink images above 2000x2000 so models accept them.
    pub auto_resize: bool,
    /// Timeout for the vision call behind `read`'s `?q=`; 0 disables it.
    pub question_timeout_ms: u64,
    pub urls: ImageUrlsCfg,
}

#[derive(Deserialize, Serialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct ImageUrlsCfg {
    /// Publish outgoing images and send providers that fetch URLs a link instead of inline base64.
    pub enabled: bool,
    /// Backend names tried in order: `command`, `ssh`, `direct`.
    pub backends: Vec<String>,
    /// Argv template for `command`; `{file}`, `{mime}` and `{ext}` are substituted.
    pub command: Option<String>,
    /// Externally reachable base URL fronting the blob server.
    pub public_base_url: Option<String>,
    /// Serving window measured from the last publish; 0 keeps links alive while the broker runs.
    pub ttl_hours: u64,
    /// Host the blob server binds to.
    pub bind_host: String,
    /// `user@host` for the ssh reverse forward.
    pub ssh_target: Option<String>,
    /// Remote listen port of the ssh reverse forward.
    pub ssh_remote_port: u16,
}

impl Default for ImagesCfg {
    fn default() -> Self {
        Self {
            block_images: false,
            describe_for_text_models: true,
            auto_resize: true,
            question_timeout_ms: 300_000,
            urls: ImageUrlsCfg::default(),
        }
    }
}

impl Default for ImageUrlsCfg {
    fn default() -> Self {
        Self {
            enabled: false,
            // OMP's default chain needs external services; native's own broker always works locally.
            backends: vec!["direct".into()],
            command: None,
            public_base_url: None,
            ttl_hours: 72,
            bind_host: "127.0.0.1".into(),
            ssh_target: None,
            ssh_remote_port: 8787,
        }
    }
}

/// Which edit tool the model gets; `magic` takes the change in words and a fast model writes it, `none`
/// removes the tool (bash stays available).
#[derive(Deserialize, Serialize, Clone, Copy, Debug, Default, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum EditMode {
    #[default]
    Replace,
    Magic,
    None,
}

#[derive(Deserialize, Serialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct EditCfg {
    /// Accept a near-identical match when the exact text is absent; whitespace-only differences always score 1.
    pub fuzzy_match: bool,
    /// Minimum character similarity (0-1) for a fuzzy match.
    pub fuzzy_threshold: f64,
    /// Refuse edits to a file whose banner says a generator wrote it.
    pub block_auto_generated: bool,
    /// Refuse edits to lines that no read or grep displayed in this process. Off by default: native edits are
    /// exact-text, and a session that never read the file is common (scripts, resumed threads).
    pub enforce_seen_lines: bool,
    pub mode: EditMode,
    pub magic: MagicEditCfg,
    /// Stop the turn when a streamed edit call's preview fails instead of running it.
    pub streaming_abort: bool,
    /// Turn edit payloads the model wrote as plain-text fenced blocks into real edit tool calls.
    pub recover_inline_edits: bool,
    pub blackbox: BlackboxCfg,
    pub auto_repair: AutoRepairCfg,
}

#[derive(Deserialize, Serialize, Clone, Debug, Default, PartialEq)]
#[serde(default)]
pub struct MagicEditCfg {
    /// Model selector or `@role` alias for `mode = magic`; empty uses the `smol` role.
    pub model: Option<String>,
}

#[derive(Deserialize, Serialize, Clone, Debug, Default, PartialEq)]
#[serde(default)]
pub struct BlackboxCfg {
    /// Append the full before/after source when an edit introduces a parse failure.
    pub enabled: bool,
}

#[derive(Deserialize, Serialize, Clone, Debug, Default, PartialEq)]
#[serde(default)]
pub struct AutoRepairCfg {
    /// Ask the model to fix a region that an edit left unparseable; accepted only when it re-parses.
    pub enabled: bool,
}

impl Default for EditCfg {
    fn default() -> Self {
        Self {
            fuzzy_match: true,
            fuzzy_threshold: 0.95,
            block_auto_generated: true,
            enforce_seen_lines: false,
            mode: EditMode::Replace,
            magic: MagicEditCfg::default(),
            streaming_abort: false,
            recover_inline_edits: true,
            blackbox: BlackboxCfg::default(),
            auto_repair: AutoRepairCfg::default(),
        }
    }
}

#[derive(Deserialize, Serialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct ReadCfg {
    /// Prefix `LINE:` to every line the read tool returns.
    pub line_numbers: bool,
    /// Lines returned when the model omits `limit`.
    pub default_limit: usize,
    pub summarize: SummarizeCfg,
}

impl Default for ReadCfg {
    fn default() -> Self {
        Self { line_numbers: false, default_limit: crate::tools::READ_LINES, summarize: SummarizeCfg::default() }
    }
}

#[derive(Deserialize, Serialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct SummarizeCfg {
    /// Answer `read` without a selector with a structural summary instead of the first `default_limit` lines.
    pub enabled: bool,
    /// Summarize Markdown and plain text paragraphs too; off by default.
    pub prose: bool,
    /// Minimum multiline body or literal length before read summaries collapse it.
    pub min_body_lines: usize,
    /// Minimum multiline block comment length before read summaries collapse it.
    pub min_comment_lines: usize,
    /// Files with fewer total lines are read verbatim instead of structurally summarized.
    pub min_total_lines: usize,
    /// BFS-unfold elidable spans until the summary is at least this many visible lines; 0 keeps only the
    /// outermost elisions.
    pub unfold_until: usize,
    /// Ceiling on visible summary lines while unfolding: an unfold that would exceed it is skipped and the
    /// span stays folded.
    pub unfold_limit: usize,
}

impl Default for SummarizeCfg {
    fn default() -> Self {
        Self {
            enabled: true,
            prose: false,
            min_body_lines: 4,
            min_comment_lines: 6,
            min_total_lines: 100,
            unfold_until: 50,
            unfold_limit: 100,
        }
    }
}

#[derive(Deserialize, Serialize, Clone, Debug, Default, PartialEq)]
#[serde(default)]
pub struct GrepCfg {
    pub context_before: usize,
    pub context_after: usize,
}

/// OMP key → native dotted key; `config import-omp` copies the value verbatim unless `convert` handles it.
pub(super) const OMP: &[(&str, &str)] = &[
    ("images.blockImages", "images.block_images"),
    ("images.describeForTextModels", "images.describe_for_text_models"),
    ("images.autoResize", "images.auto_resize"),
    ("images.questionTimeoutMs", "images.question_timeout_ms"),
    ("images.urls.enabled", "images.urls.enabled"),
    ("images.urls.backends", "images.urls.backends"),
    ("images.urls.command", "images.urls.command"),
    ("images.urls.publicBaseUrl", "images.urls.public_base_url"),
    ("images.urls.ttlHours", "images.urls.ttl_hours"),
    ("images.urls.bindHost", "images.urls.bind_host"),
    ("images.urls.sshTarget", "images.urls.ssh_target"),
    ("images.urls.sshRemotePort", "images.urls.ssh_remote_port"),
    ("edit.fuzzyMatch", "edit.fuzzy_match"),
    ("edit.fuzzyThreshold", "edit.fuzzy_threshold"),
    ("edit.blockAutoGenerated", "edit.block_auto_generated"),
    ("edit.enforceSeenLines", "edit.enforce_seen_lines"),
    ("edit.mode", "edit.mode"),
    ("edit.magic.model", "edit.magic.model"),
    ("edit.streamingAbort", "edit.streaming_abort"),
    ("edit.recoverInlineEdits", "edit.recover_inline_edits"),
    ("edit.blackbox.enabled", "edit.blackbox.enabled"),
    ("edit.autoRepair.enabled", "edit.auto_repair.enabled"),
    ("readLineNumbers", "read.line_numbers"),
    ("read.defaultLimit", "read.default_limit"),
    ("read.summarize.enabled", "read.summarize.enabled"),
    ("read.summarize.prose", "read.summarize.prose"),
    ("read.summarize.minBodyLines", "read.summarize.min_body_lines"),
    ("read.summarize.minCommentLines", "read.summarize.min_comment_lines"),
    ("read.summarize.minTotalLines", "read.summarize.min_total_lines"),
    ("read.summarize.unfoldUntil", "read.summarize.unfold_until"),
    ("read.summarize.unfoldLimit", "read.summarize.unfold_limit"),
    ("grep.contextBefore", "grep.context_before"),
    ("grep.contextAfter", "grep.context_after"),
    ("find.verifyModel", "find.verify_model"),
];

/// Translates an OMP value whose shape or units differ from the native field; `None` copies it verbatim.
pub(super) fn convert(from: &str, value: &Value) -> Option<Result<toml::Value>> {
    match from {
        // OMP's default chain names external services native does not speak; keep the names it does.
        "images.urls.backends" => Some(native_backends(value).map(|names| toml::Value::Array(names.into_iter().map(toml::Value::String).collect()))),
        // OMP ships provider-files/tailscale/cloudflared/litterbox; a value it never sends is null.
        "images.urls.command" | "images.urls.publicBaseUrl" | "images.urls.sshTarget" => {
            Some(Ok(value.as_str().map_or(toml::Value::String(String::new()), |s| toml::Value::String(s.into()))))
        }
        // Native has one exact-text edit tool; OMP's structured variants import as it.
        "edit.mode" => Some(match value.as_str() {
            Some(mode @ ("magic" | "none")) => Ok(toml::Value::String(mode.into())),
            Some("replace" | "patch" | "hashline" | "apply_patch" | "sloppy") => Ok(toml::Value::String("replace".into())),
            _ => Err(anyhow::anyhow!("edit.mode must be magic, none or an OMP edit variant")),
        }),
        // An empty magic model means the `smol` role, which the native empty value also selects.
        "edit.magic.model" => Some(Ok(toml::Value::String(value.as_str().unwrap_or_default().into()))),
        _ => None,
    }
}

fn native_backends(value: &Value) -> Result<Vec<String>> {
    let list = value.as_array().ok_or_else(|| anyhow::anyhow!("images.urls.backends must be a list"))?;
    // OMP's stock chain names services native has no client for; dropping them keeps import working,
    // and an empty result falls back to inline images.
    Ok(list.iter().filter_map(Value::as_str).filter(|name| matches!(*name, "command" | "ssh" | "direct")).map(str::to_string).collect())
}

#[cfg(test)]
mod tests {
    #[test]
    fn omp_rows_import_to_native_keys() {
        let cases = [
            ("images:\n  blockImages: true\n", "images.block_images", toml::Value::Boolean(true)),
            ("edit:\n  fuzzyMatch: false\n", "edit.fuzzy_match", toml::Value::Boolean(false)),
            ("edit.fuzzyThreshold: 0.9", "edit.fuzzy_threshold", toml::Value::Float(0.9)),
            ("edit.blockAutoGenerated: false\n", "edit.block_auto_generated", toml::Value::Boolean(false)),
            ("edit.enforceSeenLines: true\n", "edit.enforce_seen_lines", toml::Value::Boolean(true)),
            ("readLineNumbers: true\n", "read.line_numbers", toml::Value::Boolean(true)),
            ("read.defaultLimit: 120\n", "read.default_limit", toml::Value::Integer(120)),
            ("read:\n  summarize:\n    enabled: false\n", "read.summarize.enabled", toml::Value::Boolean(false)),
            ("read:\n  summarize:\n    prose: true\n", "read.summarize.prose", toml::Value::Boolean(true)),
            ("read:\n  summarize:\n    minBodyLines: 8\n", "read.summarize.min_body_lines", toml::Value::Integer(8)),
            ("read:\n  summarize:\n    minCommentLines: 10\n", "read.summarize.min_comment_lines", toml::Value::Integer(10)),
            ("read:\n  summarize:\n    minTotalLines: 200\n", "read.summarize.min_total_lines", toml::Value::Integer(200)),
            ("read:\n  summarize:\n    unfoldUntil: 30\n", "read.summarize.unfold_until", toml::Value::Integer(30)),
            ("read:\n  summarize:\n    unfoldLimit: 60\n", "read.summarize.unfold_limit", toml::Value::Integer(60)),
            ("grep.contextBefore: 2\n", "grep.context_before", toml::Value::Integer(2)),
            ("grep.contextAfter: 3\n", "grep.context_after", toml::Value::Integer(3)),
        ];
        for (yaml, key, want) in cases {
            let mut value = toml::Value::Table(crate::config::settings::omp::translate(yaml).unwrap());
            for segment in key.split('.') {
                value = value.get(segment).cloned().unwrap_or_else(|| panic!("{key} missing for {yaml:?}"));
            }
            assert_eq!(value, want, "{yaml:?}");
        }
    }

    #[test]
    fn imported_rows_decode_into_file_opts() {
        let yaml = "images:\n  blockImages: true\nedit:\n  fuzzyMatch: false\n  fuzzyThreshold: 0.8\n  blockAutoGenerated: false\n  enforceSeenLines: true\nreadLineNumbers: true\nread:\n  defaultLimit: 50\n  summarize:\n    enabled: false\n    prose: true\n    minBodyLines: 6\n    minCommentLines: 8\n    minTotalLines: 150\n    unfoldUntil: 20\n    unfoldLimit: 40\ngrep:\n  contextBefore: 1\n  contextAfter: 4\n";
        let cfg: crate::config::Config = crate::config::settings::decode(toml::Value::Table(
            crate::config::settings::omp::translate(yaml).unwrap(),
        ))
        .unwrap();
        assert!(cfg.file_opts.images.block_images);
        assert!(!cfg.file_opts.edit.fuzzy_match);
        assert_eq!(cfg.file_opts.edit.fuzzy_threshold, 0.8);
        assert!(!cfg.file_opts.edit.block_auto_generated && cfg.file_opts.edit.enforce_seen_lines);
        assert!(cfg.file_opts.read.line_numbers);
        assert_eq!(cfg.file_opts.read.default_limit, 50);
        let summarize = &cfg.file_opts.read.summarize;
        assert!(!summarize.enabled && summarize.prose);
        assert_eq!((summarize.min_body_lines, summarize.min_comment_lines, summarize.min_total_lines), (6, 8, 150));
        assert_eq!((summarize.unfold_until, summarize.unfold_limit), (20, 40));
        assert_eq!((cfg.file_opts.grep.context_before, cfg.file_opts.grep.context_after), (1, 4));
    }
}
