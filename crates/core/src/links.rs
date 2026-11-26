//! `greencode://` links: prompt drafts, thread links and "open this folder" requests.
//! A link only ever fills a draft or switches threads; it never sends, nor changes mode or branch.

use anyhow::{bail, ensure, Context, Result};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    process::Command,
    sync::Mutex,
    time::{Duration, Instant},
};
use url::Url;

pub const SCHEME: &str = "greencode";
pub const MAX_PROMPT: usize = 8000;

#[derive(Debug, Default, PartialEq)]
pub struct Draft {
    pub prompt: String,
    /// `owner/name`.
    pub repo: Option<String>,
    pub provider: Option<String>,
    pub model: Option<String>,
    /// `(owner/name, number)`.
    pub pr: Option<(String, u64)>,
    /// Invisible characters were removed from the prompt.
    pub stripped: bool,
}

#[derive(Debug, PartialEq)]
pub enum Link {
    New(Draft),
    /// The prompt may be empty: the link only switches to the thread.
    Thread(String, Draft),
    Open(PathBuf),
}

/// Control, zero-width, bidi-control and tag characters hide instructions from the reader but not from the model.
/// U+200D is handled separately because emoji sequences use it visibly.
fn invisible(c: char) -> bool {
    matches!(c,
        '\u{0}'..='\u{8}' | '\u{B}' | '\u{C}' | '\u{E}'..='\u{1F}' | '\u{7F}'..='\u{9F}'
        | '\u{AD}' | '\u{34F}' | '\u{61C}' | '\u{115F}' | '\u{1160}' | '\u{17B4}' | '\u{17B5}'
        | '\u{180B}'..='\u{180F}' | '\u{200B}' | '\u{200C}' | '\u{200E}' | '\u{200F}'
        | '\u{202A}'..='\u{202E}' | '\u{2060}'..='\u{206F}' | '\u{3164}' | '\u{FE00}'..='\u{FE0D}'
        | '\u{FEFF}' | '\u{FFA0}' | '\u{FFF0}'..='\u{FFFB}' | '\u{1BCA0}'..='\u{1BCA3}'
        | '\u{1D173}'..='\u{1D17A}' | '\u{E0000}'..='\u{E0FFF}')
}

/// The characters an emoji sequence may join with U+200D without hiding anything from the reader.
fn emoji_part(c: Option<char>) -> bool {
    c.is_some_and(|c| matches!(c, '\u{1F000}'..='\u{1FAFF}' | '\u{2190}'..='\u{2BFF}' | '\u{FE0F}'))
}

fn sanitize(s: &str) -> (String, bool) {
    // Windows and Unicode line separators become plain newlines so the draft keeps its shape.
    let normalized = s.replace("\r\n", "\n").replace(['\r', '\u{2028}', '\u{2029}'], "\n");
    let mut stripped = normalized.chars().any(invisible);
    let chars: Vec<char> = normalized.chars().filter(|c| !invisible(*c)).collect();
    let mut clean = String::with_capacity(normalized.len());
    for (i, c) in chars.iter().enumerate() {
        // A joiner is visible inside an emoji sequence; anywhere else it hides text.
        if *c == '\u{200D}'
            && !(emoji_part(chars.get(i.wrapping_sub(1)).copied()) && emoji_part(chars.get(i + 1).copied()))
        {
            stripped = true;
            continue;
        }
        clean.push(*c);
    }
    (clean.trim().to_string(), stripped)
}

/// GitHub's rules: an owner starts with a letter or digit, a name is not all dots.
fn repo_part(owner: &str, name: &str) -> bool {
    let mut chars = owner.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphanumeric())
        && owner.len() <= 39
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '-')
        && name.len() <= 100
        && name.chars().any(|c| c != '.')
        && name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

/// `owner/name` from `owner/name`, a GitHub URL or an scp-style remote.
fn slug(s: &str) -> Option<String> {
    let s = s.trim();
    let rest = s.find("github.com").map_or(s, |i| s[i + 10..].trim_start_matches([':', '/']));
    let mut it = rest.split('/');
    let (owner, name) = (it.next()?, it.next()?);
    let name = if name.len() > 4 && name.to_ascii_lowercase().ends_with(".git") { &name[..name.len() - 4] } else { name };
    repo_part(owner, name).then(|| format!("{owner}/{name}"))
}

/// `owner/name/123` or `https://github.com/owner/name/pull/123`.
fn pr(s: &str) -> Option<(String, u64)> {
    let tail = s.trim().trim_end_matches('/').rsplit('/').next()?;
    let number = tail.parse().ok().filter(|n| *n > 0)?;
    let rest = s.trim().trim_end_matches('/');
    let rest = rest.strip_suffix(tail)?.trim_end_matches('/');
    let rest = rest.strip_suffix("/pull").unwrap_or(rest);
    Some((slug(rest)?, number))
}

fn draft(url: &Url) -> Result<Draft> {
    let mut d = Draft::default();
    let mut prompt = String::new();
    for (key, value) in url.query_pairs() {
        match &*key {
            "prompt" => prompt = value.into_owned(),
            "repo" => d.repo = Some(slug(&value).with_context(|| format!("repo が不正です: {value}"))?),
            "pr" => d.pr = Some(pr(&value).with_context(|| format!("pr が不正です: {value}"))?),
            "provider" => d.provider = Some(value.trim()).filter(|v| !v.is_empty()).map(str::to_owned),
            "model" => d.model = Some(value.trim()).filter(|v| !v.is_empty()).map(str::to_owned),
            _ => {}
        }
    }
    let typed = !prompt.trim().is_empty();
    (d.prompt, d.stripped) = sanitize(&prompt);
    ensure!(
        !(typed && d.prompt.is_empty()),
        "プロンプトがすべて見えない文字でした。悪意のある指示の可能性があるため開きません"
    );
    ensure!(d.prompt.chars().count() <= MAX_PROMPT, "prompt が長すぎます（最大 {MAX_PROMPT} 文字）");
    if let Some((repo, _)) = &d.pr {
        d.repo.get_or_insert_with(|| repo.clone());
    }
    Ok(d)
}

pub fn parse(link: &str) -> Result<Link> {
    let url = Url::parse(link.trim()).context("リンクを解釈できません")?;
    ensure!(url.scheme() == SCHEME, "{SCHEME}:// のリンクではありません");
    match url.host_str() {
        Some("new") => {
            let d = draft(&url)?;
            ensure!(!d.prompt.trim().is_empty() || d.pr.is_some(), "prompt がありません");
            Ok(Link::New(d))
        }
        Some("thread") => {
            let id = url.path().trim_matches('/');
            ensure!(!id.is_empty() && !id.contains('/'), "スレッドIDがありません");
            Ok(Link::Thread(id.to_string(), draft(&url)?))
        }
        Some("open") => {
            let path = url.query_pairs().find(|(k, _)| k == "path").map(|(_, v)| PathBuf::from(&*v));
            Ok(Link::Open(path.filter(|p| p.is_absolute()).context("path がありません")?))
        }
        _ => bail!("未対応のリンクです: {link}"),
    }
}

/// `greencode://new?...`; fails when the prompt would be refused by [`parse`].
pub fn prompt_link(prompt: &str, repo: Option<&str>, provider: Option<&str>, model: Option<&str>) -> Result<String> {
    let prompt = prompt.trim();
    ensure!(!prompt.is_empty(), "下書きが空です");
    ensure!(prompt.chars().count() <= MAX_PROMPT, "下書きが長すぎます（最大 {MAX_PROMPT} 文字）");
    let mut url = Url::parse("greencode://new")?;
    let mut q = url.query_pairs_mut();
    q.append_pair("prompt", prompt);
    for (k, v) in [("repo", repo), ("provider", provider), ("model", model)] {
        if let Some(v) = v {
            q.append_pair(k, v);
        }
    }
    drop(q);
    Ok(url.into())
}

pub fn thread_link(id: &str) -> String {
    format!("{SCHEME}://thread/{id}")
}

pub fn open_link(path: &Path) -> String {
    let mut url = Url::parse("greencode://open").expect("static url");
    url.query_pairs_mut().append_pair("path", &path.to_string_lossy());
    url.into()
}

/// `owner/name` of a GitHub `origin` remote; cached briefly because the palette asks on every keystroke.
pub fn origin_slug(dir: &Path) -> Option<String> {
    static CACHE: Mutex<Option<HashMap<PathBuf, (Instant, Option<String>)>>> = Mutex::new(None);
    let mut cache = CACHE.lock().ok()?;
    let cache = cache.get_or_insert_with(HashMap::new);
    if let Some((_, found)) = cache.get(dir).filter(|(at, _)| at.elapsed() < Duration::from_secs(10)) {
        return found.clone();
    }
    let found = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["remote", "get-url", "origin"])
        .stderr(std::process::Stdio::null())
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .filter(|url| url.contains("github.com"))
        .and_then(|url| slug(&url));
    cache.insert(dir.to_path_buf(), (Instant::now(), found.clone()));
    found
}

/// Hands a link to the running GUI through the OS (macOS routes it by the bundle's URL scheme);
/// otherwise starts the GUI binary next to this one with the link as its argument.
// shortcut: without a macOS app bundle or on Linux/Windows a new GUI process starts instead of reusing a running one.
pub fn deliver(link: &str) -> Result<()> {
    if cfg!(target_os = "macos") && Command::new("open").arg(link).status().is_ok_and(|s| s.success()) {
        return Ok(());
    }
    let gui = std::env::current_exe()?.with_file_name(if cfg!(windows) { "greencode.exe" } else { "greencode" });
    ensure!(gui.is_file(), "GreenCode アプリが見つかりません（{}）", gui.display());
    Command::new(gui).arg(link).spawn().context("GreenCode アプリを起動できません")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn new(link: &str) -> Draft {
        match parse(link).unwrap() {
            Link::New(d) => d,
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn parses_every_parameter() {
        let d = new("greencode://new?prompt=Fix%20the%20build&repo=https://github.com/o/n.git&provider=claude&model=opus");
        assert_eq!(d.prompt, "Fix the build");
        assert_eq!((d.repo.as_deref(), d.provider.as_deref(), d.model.as_deref()), (Some("o/n"), Some("claude"), Some("opus")));
        let d = new("greencode://new?pr=o/n/12&prompt=x");
        assert_eq!((d.pr, d.repo), (Some(("o/n".into(), 12)), Some("o/n".into())));
        assert_eq!(new("greencode://new?pr=https://github.com/o/n/pull/7").pr, Some(("o/n".into(), 7)));
    }

    #[test]
    fn strips_invisible_characters_and_reports_it() {
        let d = new("greencode://new?prompt=a%E2%80%8Bb%E2%80%AEc%F3%A0%80%81%01");
        assert_eq!((d.prompt.as_str(), d.stripped), ("abc", true));
        assert!(!new("greencode://new?prompt=plain").stripped);
        // U+200D is visible between emoji parts but hides text anywhere else.
        let family = "%F0%9F%91%A8%E2%80%8D%F0%9F%91%A9";
        let d = new(&format!("greencode://new?prompt={family}"));
        assert_eq!((d.prompt.as_str(), d.stripped), ("👨\u{200D}👩", false));
        assert_eq!(new("greencode://new?prompt=a%E2%80%8Db"), Draft { prompt: "ab".into(), stripped: true, ..Draft::default() });
        // Line endings become newlines and surrounding whitespace is dropped, without a warning.
        assert_eq!(new("greencode://new?prompt=%0D%0Ahi%20"), Draft { prompt: "hi".into(), ..Draft::default() });
    }

    #[test]
    fn caps_prompt_and_requires_one() {
        let ok = "a".repeat(MAX_PROMPT);
        assert!(parse(&format!("greencode://new?prompt={ok}")).is_ok());
        assert!(parse(&format!("greencode://new?prompt={ok}a")).is_err());
        assert!(parse("greencode://new").is_err());
        assert!(parse("greencode://new?prompt=%20").is_err());
        assert!(parse("greencode://new?prompt=%E2%80%8B").is_err());
        assert!(parse("greencode://new?prompt=x&repo=not-a-repo").is_err());
        assert!(parse("greencode://new?prompt=x&repo=-o/n").is_err());
        assert!(parse("https://github.com/o/n").is_err());
    }

    #[test]
    fn thread_open_and_round_trip() {
        assert_eq!(parse("greencode://thread/1a-2?prompt=hi").unwrap(), Link::Thread("1a-2".into(), Draft { prompt: "hi".into(), ..Draft::default() }));
        assert!(matches!(parse(&thread_link("abc")).unwrap(), Link::Thread(id, d) if id == "abc" && d.prompt.is_empty()));
        assert_eq!(parse(&open_link(Path::new("/a b/c"))).unwrap(), Link::Open("/a b/c".into()));
        assert!(parse("greencode://open?path=rel").is_err());
        let link = prompt_link("日本語 & more=1", Some("o/n"), Some("openai"), Some("gpt")).unwrap();
        assert_eq!(new(&link), Draft { prompt: "日本語 & more=1".into(), repo: Some("o/n".into()), provider: Some("openai".into()), model: Some("gpt".into()), ..Draft::default() });
        assert!(prompt_link(&"a".repeat(MAX_PROMPT + 1), None, None, None).is_err());
    }

    #[test]
    fn github_remotes() {
        for url in ["git@github.com:o/n.git", "https://github.com/o/n", "ssh://git@github.com/o/n.git"] {
            assert_eq!(slug(url).as_deref(), Some("o/n"), "{url}");
        }
        assert_eq!(slug("https://gitlab.com/o/n"), None);
    }
}
