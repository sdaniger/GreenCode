//! Composer spelling: misspelled-word underlines, history-backed completion and confident autocorrect.
//!
//! macOS uses `NSSpellChecker` through `spelling.swift` (compiled by `swiftc` on first use, like
//! `tools/computer.swift`); elsewhere `aspell` or `hunspell` is used when installed. The backend is
//! behind [`Checker`] so the TUI behavior is tested against a fake.
use crate::config::interaction::{SpellingCfg, WordCompletion};
use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    io::{BufRead, BufReader, Write},
    ops::Range,
    process::{Child, ChildStdin, ChildStdout, Command, Stdio},
};

const SOURCE: &str = include_str!("spelling.swift");

pub trait Checker: Send {
    /// One flag per word, `true` when the word is misspelled.
    fn misspelled(&mut self, words: &[&str]) -> Result<Vec<bool>>;
    fn suggestions(&mut self, word: &str) -> Result<Vec<String>>;
    /// Word completions for an unfinished word; unsupported backends return none.
    fn complete(&mut self, _prefix: &str) -> Result<Vec<String>> {
        Ok(Vec::new())
    }
}

/// Builds and speaks the line-delimited JSON protocol of `spelling.swift`.
struct Apple {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

impl Drop for Apple {
    fn drop(&mut self) {
        let _ = self.child.kill();
    }
}

// shortcut: compiled with swiftc on first use (needs the Xcode command line tools); bundle a
// prebuilt helper for distribution, as with the computer-use and voice helpers.
fn build() -> Result<std::path::PathBuf> {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    SOURCE.hash(&mut hasher);
    let dir = crate::home().join("bin");
    let bin = dir.join(format!("spelling-{:x}", hasher.finish()));
    if !bin.exists() {
        std::fs::create_dir_all(&dir)?;
        let src = dir.join("spelling.swift");
        std::fs::write(&src, SOURCE)?;
        let tmp = bin.with_extension("tmp");
        let out = Command::new("swiftc")
            .arg("-O")
            .arg("-o")
            .arg(&tmp)
            .arg(&src)
            .output()
            .context("spell checking needs `swiftc`; install it with `xcode-select --install`")?;
        anyhow::ensure!(
            out.status.success(),
            "cannot build the spelling helper: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        std::fs::rename(tmp, &bin)?;
    }
    Ok(bin)
}

impl Apple {
    fn start() -> Result<Self> {
        let mut child = Command::new(build()?)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?;
        let stdin = child.stdin.take().unwrap();
        let stdout = BufReader::new(child.stdout.take().unwrap());
        Ok(Self { child, stdin, stdout })
    }

    fn ask(&mut self, request: &Value) -> Result<Value> {
        writeln!(self.stdin, "{request}")?;
        self.stdin.flush()?;
        let mut line = String::new();
        anyhow::ensure!(self.stdout.read_line(&mut line)? != 0, "the spelling helper stopped");
        let value: Value = serde_json::from_str(&line)?;
        if let Some(error) = value["error"].as_str() {
            anyhow::bail!("spelling helper: {error}");
        }
        Ok(value)
    }
}

impl Checker for Apple {
    fn misspelled(&mut self, words: &[&str]) -> Result<Vec<bool>> {
        let value = self.ask(&json!({"op": "check", "words": words}))?;
        Ok(value["misspelled"]
            .as_array()
            .map(|list| list.iter().map(|v| v.as_bool().unwrap_or(false)).collect())
            .unwrap_or_default())
    }

    fn suggestions(&mut self, word: &str) -> Result<Vec<String>> {
        Ok(strings(self.ask(&json!({"op": "suggest", "word": word}))?["suggestions"].clone()))
    }

    fn complete(&mut self, prefix: &str) -> Result<Vec<String>> {
        Ok(strings(self.ask(&json!({"op": "complete", "prefix": prefix}))?["completions"].clone()))
    }
}

fn strings(value: Value) -> Vec<String> {
    value.as_array().map(|list| list.iter().filter_map(|v| v.as_str().map(str::to_owned)).collect()).unwrap_or_default()
}

#[derive(Clone, Copy)]
enum External {
    Aspell,
    Hunspell,
}

impl External {
    fn find() -> Option<Self> {
        if crate::config::executable("aspell").is_some() {
            Some(Self::Aspell)
        } else if crate::config::executable("hunspell").is_some() {
            Some(Self::Hunspell)
        } else {
            None
        }
    }

    fn run(self, args: &[&str], input: &str) -> Result<String> {
        let program = match self {
            Self::Aspell => "aspell",
            Self::Hunspell => "hunspell",
        };
        let mut child = Command::new(program)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .with_context(|| format!("cannot run {program}"))?;
        child.stdin.as_mut().unwrap().write_all(input.as_bytes())?;
        drop(child.stdin.take());
        let output = child.wait_with_output()?;
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }
}

impl Checker for External {
    fn misspelled(&mut self, words: &[&str]) -> Result<Vec<bool>> {
        let input = words.join("\n") + "\n";
        let output = match self {
            Self::Aspell => self.run(&["--encoding=utf-8", "list"], &input)?,
            Self::Hunspell => self.run(&["-l"], &input)?,
        };
        let flagged: Vec<String> = output.lines().map(|line| line.trim().to_lowercase()).filter(|line| !line.is_empty()).collect();
        Ok(words.iter().map(|word| flagged.iter().any(|f| f == &word.to_lowercase())).collect())
    }

    fn suggestions(&mut self, word: &str) -> Result<Vec<String>> {
        let output = match self {
            Self::Aspell => self.run(&["--encoding=utf-8", "-a"], &format!("{word}\n"))?,
            Self::Hunspell => self.run(&["-a"], &format!("{word}\n"))?,
        };
        // `& misspelling 1 0: first, second` or `# misspelling 1 0:`
        Ok(output.lines().find_map(|line| line.strip_prefix("& ")).and_then(|rest| rest.split_once(':'))
            .map(|(_, list)| list.split(',').map(|s| s.trim().to_owned()).filter(|s| !s.is_empty()).collect())
            .unwrap_or_default())
    }
}

/// Connects the native spell checker; call from a worker because the first run compiles Swift.
pub fn connect() -> Result<Box<dyn Checker>> {
    #[cfg(target_os = "macos")]
    if let Ok(apple) = Apple::start() {
        return Ok(Box::new(apple));
    }
    if let Some(external) = External::find() {
        return Ok(Box::new(external));
    }
    anyhow::bail!("no spell checker available: macOS NSSpellChecker needs swiftc, or install aspell/hunspell")
}

/// A word candidate: only prose is checked, so code spans, paths, URLs and identifiers are skipped.
fn token_skip(token: &str) -> bool {
    if token.starts_with('`') || token.contains("://") || token.contains('@') || token.contains('\\') {
        return true;
    }
    let body = token.trim_start_matches(|c: char| !c.is_alphanumeric());
    if body.starts_with("www.") || body.contains('/') || body.contains('~') || body.contains('_') {
        return true;
    }
    if body.chars().any(|c| c.is_ascii_digit()) {
        return true;
    }
    // A dot between word characters is a file name or domain, not sentence punctuation.
    let chars: Vec<char> = body.chars().collect();
    if chars.windows(3).any(|w| w[1] == '.' && w[0].is_alphanumeric() && w[2].is_alphanumeric()) {
        return true;
    }
    let letters: Vec<char> = chars.into_iter().filter(|c| c.is_alphabetic()).collect();
    letters.windows(2).any(|w| w[0].is_lowercase() && w[1].is_uppercase())
}

fn eligible(word: &str) -> bool {
    let chars: Vec<char> = word.chars().collect();
    chars.len() >= 3
        && chars[0].is_alphabetic()
        && chars.iter().all(|c| c.is_alphabetic() || matches!(c, '\'' | '’'))
        && chars.iter().any(|c| c.is_lowercase())
}

/// Prose words with their byte ranges, excluding fenced and inline code, paths, URLs and identifiers.
pub fn prose_words(text: &str) -> Vec<(Range<usize>, String)> {
    let mut out = Vec::new();
    let mut offset = 0;
    let mut fenced = false;
    for line in text.split_inclusive('\n') {
        let body = line.strip_suffix('\n').unwrap_or(line);
        let trimmed = body.trim_start();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            fenced = !fenced;
            offset += line.len();
            continue;
        }
        if !fenced {
            let mut code = vec![false; body.len()];
            let mut inside = false;
            for (at, c) in body.char_indices() {
                if c == '`' {
                    inside = !inside;
                }
                code[at] = inside;
            }
            let mut at = 0;
            for token in body.split_whitespace() {
                let start = body[at..].find(token).map_or(at, |i| at + i);
                let end = start + token.len();
                at = end;
                if code[start..end].iter().any(|c| *c) || token_skip(token) {
                    continue;
                }
                let chars: Vec<(usize, char)> = token.char_indices().collect();
                let mut i = 0;
                while i < chars.len() {
                    if !(chars[i].1.is_alphabetic() || matches!(chars[i].1, '\'' | '’')) {
                        i += 1;
                        continue;
                    }
                    let begin = i;
                    while i < chars.len() && (chars[i].1.is_alphabetic() || matches!(chars[i].1, '\'' | '’')) {
                        i += 1;
                    }
                    let from = start + chars[begin].0;
                    let to = if i < chars.len() { start + chars[i].0 } else { end };
                    let word = text[offset + from..offset + to].trim_matches(['\'', '’']);
                    if eligible(word) {
                        let skew = (text[offset + from..offset + to].len() - text[offset + from..offset + to].trim_start_matches(['\'', '’']).len())
                            .min(word.len());
                        let begin = offset + from + skew;
                        out.push((begin..begin + word.len(), word.to_owned()));
                    }
                }
            }
        }
        offset += line.len();
    }
    out
}

pub struct Spell {
    cfg: SpellingCfg,
    checker: Option<Box<dyn Checker>>,
    /// Spellings seen in prompts and replies; they suppress underlines and feed completion.
    vocabulary: HashMap<String, u32>,
}

impl Spell {
    pub fn new(cfg: SpellingCfg) -> Self {
        Self { cfg, checker: None, vocabulary: HashMap::new() }
    }

    pub fn with_checker(cfg: SpellingCfg, checker: Box<dyn Checker>) -> Self {
        Self { cfg, checker: Some(checker), vocabulary: HashMap::new() }
    }

    pub fn use_checker(&mut self, checker: Box<dyn Checker>) {
        self.checker = Some(checker);
    }

    pub fn has_checker(&self) -> bool {
        self.checker.is_some()
    }

    pub fn enabled(&self) -> bool {
        self.cfg.typo_detection || self.cfg.autocorrect || self.cfg.autocomplete != WordCompletion::Off
    }

    /// Ranges of misspelled prose words, for underlining the composer.
    pub fn typos(&mut self, text: &str) -> Vec<Range<usize>> {
        if !self.cfg.typo_detection {
            return Vec::new();
        }
        let words: Vec<(Range<usize>, String)> = prose_words(text)
            .into_iter()
            .filter(|(_, word)| !self.vocabulary.contains_key(&word.to_lowercase()))
            .collect();
        let Some(checker) = self.checker.as_mut() else { return Vec::new() };
        let list: Vec<&str> = words.iter().map(|(_, word)| word.as_str()).collect();
        match checker.misspelled(&list) {
            Ok(flags) => words.into_iter().zip(flags).filter(|(_, bad)| *bad).map(|((range, _), _)| range).collect(),
            Err(_) => Vec::new(),
        }
    }

    /// A completion for the word the cursor is in; `None` when off, inside code, or without a candidate.
    pub fn completion(&mut self, text: &str, cursor: usize) -> Option<(Range<usize>, String)> {
        if self.cfg.autocomplete == WordCompletion::Off {
            return None;
        }
        let cursor = cursor.min(text.len());
        if text[cursor..].chars().next().is_some_and(|c| c.is_alphanumeric() || c == '_') {
            return None;
        }
        let mut start = cursor;
        for (at, c) in text[..cursor].char_indices().rev() {
            if c.is_alphabetic() || c == '\'' {
                start = at;
            } else {
                break;
            }
        }
        let prefix = &text[start..cursor];
        if prefix.chars().count() < 2 || !prefix.chars().all(char::is_alphabetic) {
            return None;
        }
        if start > 0 && text[..start].chars().next_back().is_some_and(|c| c.is_alphanumeric() || c == '_') {
            return None;
        }
        let lowered = prefix.to_lowercase();
        let mut candidates = Vec::new();
        if matches!(self.cfg.autocomplete, WordCompletion::Apple | WordCompletion::Auto) {
            if let Some(checker) = self.checker.as_mut() {
                if let Ok(list) = checker.complete(prefix) {
                    candidates.extend(list);
                }
            }
        }
        if self.cfg.autocomplete != WordCompletion::Apple {
            let mut seen: Vec<(&String, &u32)> = self.vocabulary.iter()
                .filter(|(word, _)| word.starts_with(&lowered) && word.len() > lowered.len())
                .collect();
            seen.sort_by(|a, b| b.1.cmp(a.1).then(a.0.cmp(b.0)));
            candidates.extend(seen.into_iter().map(|(word, _)| word.clone()));
        }
        let word = candidates.into_iter().find(|word| {
            let lower = word.to_lowercase();
            lower.starts_with(&lowered) && lower.len() > lowered.len() && lower.chars().all(char::is_alphabetic)
        })?;
        let word = match word.chars().next() {
            Some(first) if prefix.chars().next().is_some_and(char::is_uppercase) => {
                first.to_uppercase().collect::<String>() + &word[first.len_utf8()..]
            }
            _ => word,
        };
        Some((start..cursor, word))
    }

    /// The correction for the word that just ended at `cursor`, when a checked suggestion is confident.
    pub fn autocorrect(&mut self, text: &str, cursor: usize) -> Option<(Range<usize>, String)> {
        if !self.cfg.autocorrect {
            return None;
        }
        let cursor = cursor.min(text.len());
        let boundary = text[..cursor].chars().next_back()?;
        if !(boundary.is_whitespace() || boundary.is_ascii_punctuation()) {
            return None;
        }
        let end = cursor - boundary.len_utf8();
        let mut start = end;
        for (at, c) in text[..end].char_indices().rev() {
            if c.is_alphabetic() || c == '\'' {
                start = at;
            } else {
                break;
            }
        }
        let word = &text[start..end];
        if !eligible(word) || self.vocabulary.contains_key(&word.to_lowercase()) {
            return None;
        }
        if !prose_words(text).iter().any(|(range, _)| range == &(start..end)) {
            return None;
        }
        let checker = self.checker.as_mut()?;
        if !checker.misspelled(&[word]).ok()?.first().copied().unwrap_or(false) {
            return None;
        }
        let suggestion = checker.suggestions(word).ok()?.into_iter().find(|candidate| {
            eligible(candidate) && candidate.to_lowercase() != word.to_lowercase() && confident(word, candidate)
        })?;
        let suggestion = match word.chars().next() {
            Some(first) if first.is_uppercase() => first.to_uppercase().collect::<String>() + &suggestion[first.len_utf8()..],
            _ => suggestion,
        };
        Some((start..end, suggestion))
    }

    /// Feeds prompt and reply prose into the completion vocabulary.
    pub fn learn(&mut self, text: &str) {
        for (_, word) in prose_words(text) {
            *self.vocabulary.entry(word.to_lowercase()).or_insert(0) += 1;
        }
        // shortcut: unbounded history vocabulary; prune when memory or completion latency matters.
    }
}

/// Applies a completion to `text`, returning the new text and cursor; `None` when it no longer fits.
pub fn apply_completion(text: &str, cursor: usize, completion: &(Range<usize>, String)) -> Option<(String, usize)> {
    let (range, word) = completion;
    if range.end != cursor {
        return None;
    }
    let typed = text.get(range.clone())?;
    if !word.to_lowercase().starts_with(&typed.to_lowercase()) || word.len() <= typed.len() {
        return None;
    }
    let mut out = text.to_owned();
    out.replace_range(range.clone(), word);
    Some((out, range.start + word.len()))
}

/// Edit distance, allowing one adjacent transposition per step (Damerau-Levenshtein).
fn distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().flat_map(char::to_lowercase).collect();
    let b: Vec<char> = b.chars().flat_map(char::to_lowercase).collect();
    let mut d = vec![vec![0; b.len() + 1]; a.len() + 1];
    for (i, row) in d.iter_mut().enumerate() { row[0] = i; }
    for j in 0..=b.len() { d[0][j] = j; }
    for i in 1..=a.len() {
        for j in 1..=b.len() {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            let mut best = (d[i - 1][j] + 1).min(d[i][j - 1] + 1).min(d[i - 1][j - 1] + cost);
            if i > 1 && j > 1 && a[i - 1] == b[j - 2] && a[i - 2] == b[j - 1] {
                best = best.min(d[i - 2][j - 2] + 1);
            }
            d[i][j] = best;
        }
    }
    d[a.len()][b.len()]
}

/// Autocorrect only one or two edits away; a short word tolerates a single edit.
fn confident(word: &str, candidate: &str) -> bool {
    let limit = if word.chars().count() <= 4 { 1 } else { 2 };
    word.len().abs_diff(candidate.len()) <= limit && distance(word, candidate) <= limit
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fake {
        bad: Vec<String>,
        suggestions: Vec<String>,
        completions: Vec<String>,
    }

    impl Fake {
        fn new(bad: &[&str], suggestions: &[&str], completions: &[&str]) -> Self {
            Self {
                bad: bad.iter().map(|w| w.to_lowercase()).collect(),
                suggestions: suggestions.iter().map(|w| w.to_string()).collect(),
                completions: completions.iter().map(|w| w.to_string()).collect(),
            }
        }
    }

    impl Checker for Fake {
        fn misspelled(&mut self, words: &[&str]) -> Result<Vec<bool>> {
            Ok(words.iter().map(|w| self.bad.contains(&w.to_lowercase())).collect())
        }
        fn suggestions(&mut self, _word: &str) -> Result<Vec<String>> {
            Ok(self.suggestions.clone())
        }
        fn complete(&mut self, _prefix: &str) -> Result<Vec<String>> {
            Ok(self.completions.clone())
        }
    }

    fn cfg() -> SpellingCfg {
        SpellingCfg { typo_detection: true, autocomplete: WordCompletion::Auto, autocorrect: true }
    }

    #[test]
    fn typos_skip_code_paths_urls_and_identifiers() {
        let mut spell = Spell::with_checker(cfg(), Box::new(Fake::new(&["teh"], &[], &[])));
        let text = "teh `teh` src/teh.rs https://example.com/teh tehVar snake_teh camelTeh\n```\nteh\n```";
        assert_eq!(spell.typos(text), vec![0..3]);
        // A learned spelling is never underlined again.
        spell.learn("teh");
        assert!(spell.typos(text).is_empty());
    }

    #[test]
    fn autocomplete_uses_the_backend_and_history_and_applies_on_tab() {
        let mut spell = Spell::with_checker(cfg(), Box::new(Fake::new(&[], &[], &["recommendation"])));
        assert_eq!(spell.completion("rec", 3), Some((0..3, "recommendation".into())));
        let (range, word) = spell.completion("rec", 3).unwrap();
        assert_eq!(apply_completion("rec", 3, &(range, word)).map(|(text, at)| (text, at)), Some(("recommendation".into(), 14)));
        assert!(apply_completion("recommendation", 14, &(0..3, "recommendation".into())).is_none());
        // `ngram` ignores the backend and completes from learned prose.
        let mut history = Spell::with_checker(
            SpellingCfg { autocomplete: WordCompletion::Ngram, ..cfg() },
            Box::new(Fake::new(&[], &[], &["wrong"])),
        );
        history.learn("We should reconsider the recommendation.");
        assert_eq!(history.completion("reco", 4).map(|(_, word)| word), Some("recommendation".into()));
        // `off` never completes.
        history.cfg.autocomplete = WordCompletion::Off;
        assert!(history.completion("reco", 4).is_none());
    }

    #[test]
    fn autocorrect_only_rewrites_confident_prose_on_a_boundary() {
        let mut spell = Spell::with_checker(cfg(), Box::new(Fake::new(&["teh"], &["the"], &[])));
        assert_eq!(spell.autocorrect("teh ", 4), Some((0..3, "the".into())));
        assert_eq!(spell.autocorrect("teh", 3), None, "no boundary typed yet");
        assert_eq!(spell.autocorrect("`teh` ", 6), None, "code is not corrected");
        spell.cfg.autocorrect = false;
        assert_eq!(spell.autocorrect("teh ", 4), None);
        // A distant suggestion is not confident enough.
        let mut far = Spell::with_checker(cfg(), Box::new(Fake::new(&["teh"], &["theater"], &[])));
        assert_eq!(far.autocorrect("teh ", 4), None);
    }
}
