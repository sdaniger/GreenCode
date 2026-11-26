//! `shellMinimizer`: compress the low-signal bulk of verbose `git status`, `cargo`, and `npm` output before the
//! model sees it, plus the source-outline level shared with `read` and `cat`. Only the named commands are
//! touched; anything else is returned byte-for-byte.
use crate::config::{bash::SourceOutlineLevel, files::ReadCfg, Config};
use std::{collections::HashSet, path::Path};

/// Progress lines that never tell the model anything it can act on.
const NOISE: [&str; 17] = [
    "Compiling ",
    "Checking ",
    "Downloading ",
    "Downloaded ",
    "Updating ",
    "Locking ",
    "Adding ",
    "Blocking waiting",
    "Fresh ",
    "Documenting ",
    "Running `",
    "npm WARN",
    "npm http",
    "npm timing",
    "added 0 packages",
    "removed 0 packages",
    "changed 0 packages",
];

/// Lines of `git status` untracked listings kept before the rest are counted.
const UNTRACKED_KEEP: usize = 20;

fn is_noise(line: &str) -> bool {
    NOISE.iter().any(|prefix| line.trim_start().starts_with(prefix))
}

/// The program and subcommand a shell line runs, ignoring env assignments and a leading `cd`.
fn program(command: &str) -> Option<String> {
    let mut words = command.split_whitespace().skip_while(|w| w.contains('=') || *w == "cd" || w.starts_with('-'));
    let program = words.next()?.rsplit('/').next()?.to_string();
    Some(match program.as_str() {
        "git" => format!("git {}", words.next().unwrap_or("")),
        _ => program,
    })
}

/// `read`'s summarize thresholds for the configured outline level; aggressive folds more, sooner.
pub(crate) fn outline_cfg(read: &ReadCfg, level: SourceOutlineLevel) -> ReadCfg {
    let mut read = read.clone();
    if level == SourceOutlineLevel::Aggressive {
        let s = &mut read.summarize;
        s.min_total_lines = s.min_total_lines.min(40);
        s.min_body_lines = s.min_body_lines.min(2);
        s.min_comment_lines = s.min_comment_lines.min(3);
        s.unfold_until = s.unfold_until.min(20);
        s.unfold_limit = s.unfold_limit.min(60);
    }
    read
}

/// `cat` of one source file returns its structural outline instead of the whole body, under the same level
/// `read` uses. `None` keeps the command's own output.
pub(crate) fn cat_outline(command: &str, cwd: &Path, cfg: &Config) -> Option<String> {
    let mut words = command.split_whitespace();
    if words.next()? != "cat" {
        return None;
    }
    let path = words.next()?;
    if words.next().is_some() || path.starts_with('-') {
        return None;
    }
    let path = cwd.join(path);
    let text = std::fs::read_to_string(&path).ok()?;
    let read = outline_cfg(&cfg.file_opts.read, cfg.bash_opts.shell_minimizer.source_outline_level);
    super::summary::try_summary(&text, &path.display().to_string(), &read).map(|s| s.body)
}

fn wants(command: &str) -> bool {
    matches!(
        program(command).as_deref(),
        Some("git status" | "git diff" | "git log" | "cargo" | "npm" | "yarn" | "pnpm")
    )
}

/// `output` compressed for `command`, or unchanged when the command is not one the minimizer covers or there is
/// nothing to drop. Dropped lines are reported once so the model can ask for the raw log if it needs it.
pub(crate) fn minimize(command: &str, output: &str) -> String {
    if !wants(command) {
        return output.to_string();
    }
    let mut kept: Vec<&str> = Vec::new();
    let mut seen: HashSet<&str> = HashSet::new();
    let mut dropped = 0usize;
    let mut untracked = 0usize;
    let mut in_untracked = false;
    for line in output.lines() {
        if line.trim_end() == "Untracked files:" || line.starts_with("Untracked files:") {
            in_untracked = true;
            kept.push(line);
            continue;
        }
        if in_untracked {
            if line.trim().is_empty() || !line.starts_with('\t') {
                in_untracked = false;
            } else {
                untracked += 1;
                if untracked <= UNTRACKED_KEEP {
                    kept.push(line);
                }
                continue;
            }
        }
        // `test foo ... ok` runs get counted instead of listed.
        if line.trim_start().starts_with("test ") && line.trim_end().ends_with(" ok") {
            dropped += 1;
            continue;
        }
        if is_noise(line) {
            dropped += 1;
            continue;
        }
        // Identical diagnostics repeated by several targets keep the first copy.
        if !line.trim().is_empty() && !seen.insert(line) {
            dropped += 1;
            continue;
        }
        kept.push(line);
    }
    if untracked > UNTRACKED_KEEP {
        dropped += untracked - UNTRACKED_KEEP;
    }
    if dropped == 0 {
        return output.to_string();
    }
    let body = kept.join("\n");
    format!("{body}\n[shell output minimized: {dropped} lines omitted; disable with shell_minimizer.enabled]")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cargo_progress_and_duplicate_diagnostics_collapse() {
        let output = "   Compiling serde v1.0\n   Compiling tokio v1.0\nwarning: unused variable\nwarning: unused variable\n    Finished `dev` profile\n";
        let out = minimize("cargo build", output);
        assert!(!out.contains("Compiling"), "{out}");
        assert_eq!(out.matches("warning: unused variable").count(), 1, "{out}");
        assert!(out.contains("Finished"), "{out}");
        assert!(out.contains("3 lines omitted"), "{out}");
    }

    #[test]
    fn passing_cargo_tests_become_a_count_while_failures_stay() {
        let output = "test a ... ok\ntest b ... ok\ntest c ... FAILED\n";
        let out = minimize("cargo test -p x", output);
        assert!(!out.contains("test a ... ok"), "{out}");
        assert!(out.contains("test c ... FAILED"), "{out}");
        assert!(out.contains("2 lines omitted"), "{out}");
    }

    #[test]
    fn git_untracked_listings_keep_a_window() {
        let mut output = String::from("On branch main\nUntracked files:\n");
        for i in 0..30 {
            output.push_str(&format!("\tfile{i}.rs\n"));
        }
        output.push_str("\nno changes added to commit\n");
        let out = minimize("git status", &output);
        assert!(out.contains("file19.rs"), "{out}");
        assert!(!out.contains("file20.rs"), "{out}");
        assert!(out.contains("10 lines omitted"), "{out}");
        assert!(out.contains("no changes added to commit"), "{out}");
    }

    #[test]
    fn npm_install_noise_collapses_but_errors_survive() {
        let output = "npm WARN idealTree Removing\nnpm http fetch GET 200\nnpm timing idealTree Completed\nadded 0 packages\nnpm error code ELIFECYCLE\nnpm error Failed at the build script\n";
        let out = minimize("npm install", output);
        assert!(!out.contains("npm WARN"), "{out}");
        assert!(!out.contains("added 0 packages"), "{out}");
        assert!(out.contains("npm error Failed at the build script"), "{out}");
        assert!(out.contains("4 lines omitted"), "{out}");
    }

    #[test]
    fn aggressive_outline_folds_sooner_than_the_default() {
        let read = ReadCfg::default();
        let default = outline_cfg(&read, SourceOutlineLevel::Default);
        let aggressive = outline_cfg(&read, SourceOutlineLevel::Aggressive);
        assert_eq!(default, read, "default keeps the configured thresholds");
        assert!(aggressive.summarize.min_total_lines <= 40);
        assert!(aggressive.summarize.min_body_lines <= 2);
        assert!(aggressive.summarize.min_comment_lines <= 3);
        assert!(aggressive.summarize.unfold_until <= 20);
        assert!(aggressive.summarize.unfold_limit <= 60);
    }

    #[test]
    fn other_commands_and_clean_output_are_untouched() {
        let output = "hello\nhello\n";
        assert_eq!(minimize("echo hi", output), output);
        assert_eq!(minimize("cargo build", "warning: one\n"), "warning: one\n");
    }
}
