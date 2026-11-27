//! Records the commit a source build came from so the updater can place it on `main`.

use std::{env, fs, path::Path, process::Command};

fn main() {
    println!("cargo:rerun-if-env-changed=GREENCODE_MAIN_COMMIT");
    println!("cargo:rerun-if-env-changed=GREENCODE_RELEASE_COMMIT");
    watch_head();

    // The release workflow records the channel commit explicitly; the checkout is authoritative then.
    if env::var_os("GREENCODE_MAIN_COMMIT").is_some()
        || env::var_os("GREENCODE_RELEASE_COMMIT").is_some()
    {
        return;
    }
    let Some(commit) = git(&["rev-parse", "HEAD"]) else {
        return;
    };
    if !valid_commit(&commit) {
        return;
    }
    // A main checkout is a commit build. Anything else (a tag, a feature branch, a worktree)
    // stays on the stable channel but can still prove its place in main's history.
    if git(&["rev-parse", "--abbrev-ref", "HEAD"]).as_deref() == Some("main") {
        println!("cargo:rustc-env=GREENCODE_MAIN_COMMIT={commit}");
    } else {
        println!("cargo:rustc-env=GREENCODE_BUILD_COMMIT={commit}");
    }
}

/// Rebuild when HEAD moves, including through a checked-out branch.
fn watch_head() {
    let Some(dir) = git(&["rev-parse", "--absolute-git-dir"]) else {
        return;
    };
    println!("cargo:rerun-if-changed={dir}/HEAD");
    if let Ok(head) = fs::read_to_string(Path::new(&dir).join("HEAD")) {
        if let Some(reference) = head.strip_prefix("ref: ") {
            println!("cargo:rerun-if-changed={dir}/{}", reference.trim());
        }
    }
}

fn git(args: &[&str]) -> Option<String> {
    let output = Command::new("git").args(args).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let value = String::from_utf8(output.stdout).ok()?.trim().to_string();
    (!value.is_empty()).then_some(value)
}

fn valid_commit(value: &str) -> bool {
    value.len() == 40 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}
