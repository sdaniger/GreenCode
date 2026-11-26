//! Composer completions: `/command`, `/skill NAME`, `$skill`, `@file`, `@thread`, `#PR`; `@thread:ID` and `#N` expand when sent.

use super::*;
use greencode_core::{context, skills, Block, Message};
use std::time::{Duration, Instant};

const THREAD: &str = "@thread:";
const SKILLS_TTL: Duration = Duration::from_secs(5);
/// Messages and characters per message a thread reference carries; the rest is in the transcript file.
const RECENT: usize = 4;
const CLIP: usize = 400;

/// Skills of the active project, re-read at most every few seconds because completions run on every render.
#[derive(Default)]
pub(super) struct Cache(Option<(PathBuf, Instant, Vec<String>)>);

fn text_of(m: &Message) -> String {
    m.content
        .iter()
        .filter_map(|b| match b {
            Block::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// What the agent gets for `@thread:ID`: title, id, transcript path and the last few messages.
fn thread_block(title: &str, id: &str, messages: &[Message]) -> String {
    let recent: Vec<String> = messages
        .iter()
        .filter(|m| !context::is_update(&text_of(m)))
        .map(|m| (m.role == greencode_core::Role::User, text_of(m)))
        .filter(|(_, t)| !t.trim().is_empty())
        .map(|(user, t)| {
            let t: String = t.trim().chars().take(CLIP).collect();
            format!("- {}: {t}", if user { "user" } else { "assistant" })
        })
        .collect();
    let skip = recent.len().saturating_sub(RECENT);
    format!(
        "[Referenced thread: {title} ({id})]\nFull transcript: {}\nRecent messages:\n{}",
        Session::path(id).display(),
        recent[skip..].join("\n")
    )
}

/// `text` followed by one block per distinct `@thread:ID` (via `thread`) and `#N` pull request (via `prs`) it mentions.
fn expand(
    text: &str,
    thread: impl Fn(&str) -> Option<String>,
    prs: &[(u64, String, String)],
) -> String {
    let mut blocks: Vec<String> = vec![];
    for word in text.split_whitespace() {
        let word = word.trim_end_matches([',', '.', ';', ':', ')', '、', '。']);
        let block = if let Some(id) = word.strip_prefix(THREAD) {
            thread(id)
        } else if let Some(n) = word.strip_prefix('#').and_then(|n| n.parse::<u64>().ok()) {
            prs.iter()
                .find(|p| p.0 == n)
                .map(|(n, title, head)| format!("[Referenced pull request #{n}: {title} (branch {head})]"))
        } else {
            None
        };
        if let Some(block) = block.filter(|b| !blocks.contains(b)) {
            blocks.push(block);
        }
    }
    if blocks.is_empty() {
        return text.to_string();
    }
    format!("{text}\n\n{}", blocks.join("\n\n"))
}

impl Workspace {
    fn skill_names(&mut self) -> &[String] {
        let cwd = self.thread().meta.cwd.clone();
        let stale = self.mentions.0.as_ref().is_none_or(|(c, at, _)| *c != cwd || at.elapsed() > SKILLS_TTL);
        if stale {
            let names = skills::catalog(&cwd, &self.cfg).into_iter().map(|s| s.name).collect();
            self.mentions.0 = Some((cwd, Instant::now(), names));
        }
        &self.mentions.0.as_ref().unwrap().2
    }

    /// Completions for the word being typed.
    pub(super) fn completions(&mut self, cx: &App) -> Vec<String> {
        let value = self.input.read(cx).value();
        let word = value.rsplit(char::is_whitespace).next().unwrap_or("");
        let slash_skills = self.thread().meta.backend == "native" && self.ui["slash_skills_off"] != true;
        if let Some(q) = value.strip_prefix('/').filter(|q| !q.contains(char::is_whitespace)) {
            let mut found: Vec<String> =
                self.commands.iter().filter(|c| c.starts_with(q)).take(6).map(|c| format!("/{c}")).collect();
            if slash_skills {
                let q = q.to_lowercase();
                let skills: Vec<String> = self
                    .skill_names()
                    .iter()
                    .filter(|s| s.to_lowercase().starts_with(&q))
                    .take(2)
                    .map(|s| format!("/skill {s}"))
                    .collect();
                found.truncate(6 - skills.len());
                found.extend(skills);
            }
            found
        } else if let Some(q) = value.strip_prefix("/skill ").filter(|q| !q.contains(char::is_whitespace)) {
            let q = q.to_lowercase();
            self.skill_names()
                .iter()
                .filter(|s| s.to_lowercase().contains(&q))
                .take(6)
                .map(|s| format!("/skill {s}"))
                .collect()
        } else if let Some(q) = word.strip_prefix('$') {
            let q = q.to_lowercase();
            self.skill_names().iter().filter(|s| s.to_lowercase().contains(&q)).take(6).map(|s| format!("${s}")).collect()
        } else if let Some(q) = word.strip_prefix('#') {
            let q = q.to_lowercase();
            self.prs
                .iter()
                .filter(|(n, title, _)| n.to_string().starts_with(&q) || title.to_lowercase().contains(&q))
                .take(6)
                .map(|(n, ..)| format!("#{n}"))
                .collect()
        } else if let Some(q) = word.strip_prefix('@') {
            let q = q.to_lowercase();
            let active = self.thread().meta.id.clone();
            let threads: Vec<String> = self
                .threads
                .iter()
                .filter(|t| t.meta.id != active && !t.meta.title.is_empty() && t.meta.title.to_lowercase().contains(&q))
                .take(2)
                .map(|t| format!("{THREAD}{}", t.meta.id))
                .collect();
            let mut found: Vec<String> = self
                .files()
                .iter()
                .filter(|f| f.to_lowercase().contains(&q))
                .take(6 - threads.len())
                .map(|f| format!("@{f}"))
                .collect();
            found.extend(threads);
            found
        } else {
            vec![]
        }
    }

    /// A completion as shown on its chip: thread ids and PR numbers get their titles.
    pub(super) fn completion_label(&self, s: &str) -> String {
        if let Some(id) = s.strip_prefix(THREAD) {
            if let Some(t) = self.threads.iter().find(|t| t.meta.id == id) {
                return format!("@スレッド: {}", t.meta.title.chars().take(30).collect::<String>());
            }
        } else if let Some(n) = s.strip_prefix('#').and_then(|n| n.parse::<u64>().ok()) {
            if let Some((_, title, _)) = self.prs.iter().find(|p| p.0 == n) {
                return format!("#{n} {}", title.chars().take(30).collect::<String>());
            }
        }
        s.to_string()
    }

    /// Completes `$skill`, `@thread:ID`, `#N` and `/skill NAME`. False: a command or file, which `complete` handles.
    pub(super) fn complete_mention(&mut self, s: &str, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let value = self.input.read(cx).value().to_string();
        let start = value.rmatch_indices(char::is_whitespace).next().map_or(0, |(i, space)| i + space.len());
        let new = if s.starts_with("/skill ") {
            format!("{s} ")
        } else if let Some(name) = s.strip_prefix('$') {
            let rest = value[..start].trim_end().to_string();
            self.input.update(cx, |st, cx| set_input(st, rest, window, cx));
            self.run(Act::Skill(name.to_string()), window, cx);
            return true;
        } else if s.starts_with(THREAD) || s.starts_with('#') {
            format!("{}{s} ", &value[..start])
        } else {
            return false;
        };
        self.input.update(cx, |st, cx| set_input(st, new, window, cx));
        cx.notify();
        true
    }

    /// The message with `@thread:ID` and `#N` references expanded for the agent.
    pub(super) fn expand_mentions(&self, text: String) -> String {
        if !text.contains(THREAD) && !text.contains('#') {
            return text;
        }
        let thread = |id: &str| {
            let t = self.threads.iter().find(|t| t.meta.id == id)?;
            let messages = Session::load(id).map(|(_, m)| m).unwrap_or_default();
            Some(thread_block(&t.meta.title, id, &messages))
        };
        expand(&text, thread, &self.prs)
    }
}

#[cfg(test)]
mod tests {
    // No glob import: it would bring in a `test` macro that shadows `#[test]`.
    use super::{expand, thread_block};
    use crate::workspace::tests::open;
    use crate::workspace::{set_input, Act, Meta, Thread, Workspace};
    use gpui_kit::{Context, TestAppContext, Window};
    use greencode_core::Message;
    use serde_json::json;

    #[test]
    fn references_expand_once_and_only_when_known() {
        let prs = vec![(12, "Fix it".to_string(), "fix".to_string())];
        let thread = |id: &str| (id == "abc").then(|| "[T]".to_string());
        let out = expand("see @thread:abc, @thread:abc and @thread:zzz, #12 #99", thread, &prs);
        assert_eq!(out.matches("[T]").count(), 1);
        assert!(out.contains("pull request #12: Fix it (branch fix)") && !out.contains("#99:"));
        assert_eq!(expand("plain", thread, &prs), "plain");
    }

    #[test]
    fn thread_block_keeps_the_last_messages() {
        let messages: Vec<_> = (0..6).map(|i| Message::user(format!("m{i}"))).collect();
        let block = thread_block("Title", "id1", &messages);
        assert!(block.contains("Title (id1)") && block.contains("m5") && block.contains("m2") && !block.contains("m1"));
    }

    #[gpui_kit::test]
    async fn dollar_hash_and_at_complete_and_skills_can_be_hidden(cx: &mut TestAppContext) {
        let (ws, cx) = open(cx);
        let cwd = ws.read_with(cx, |ws, _| ws.thread().meta.cwd.clone());
        let skill = greencode_core::home().join("skills/mention-test-skill");
        std::fs::create_dir_all(&skill).unwrap();
        std::fs::write(skill.join("SKILL.md"), "---\nname: mention-test-skill\n---\nDo the thing.").unwrap();
        cx.update(|window, cx| {
            ws.update(cx, |ws, cx| {
                ws.prs = vec![(123, "Add widgets".into(), "widgets".into())];
                ws.threads[0].meta.title = "other".into();
                let mut other = Thread::new(Meta::new(cwd.clone(), "m".into(), "native".into()), true, ws.cfg.mode);
                other.meta.title = "Refactor parser".into();
                ws.threads.push(other);
                let type_in = |ws: &mut Workspace, s: &str, window: &mut Window, cx: &mut Context<Workspace>| {
                    ws.input.update(cx, |st, cx| set_input(st, s, window, cx));
                    ws.completions(cx)
                };
                assert_eq!(type_in(ws, "use $mention-t", window, cx), ["$mention-test-skill"]);
                assert_eq!(type_in(ws, "fix #12", window, cx), ["#123"]);
                assert_eq!(ws.completion_label("#123"), "#123 Add widgets");
                let threads = type_in(ws, "see @refac", window, cx);
                assert_eq!(threads, [format!("@thread:{}", ws.threads[1].meta.id)]);
                assert_eq!(type_in(ws, "/skill mention", window, cx), ["/skill mention-test-skill"]);
                assert!(type_in(ws, "/mention", window, cx).contains(&"/skill mention-test-skill".to_string()));
                ws.ui["slash_skills_off"] = json!(true);
                assert!(type_in(ws, "/mention", window, cx).is_empty());
                // `$name` loads the skill into the draft in place of the typed word.
                ws.input.update(cx, |st, cx| set_input(st, "use $mention-t", window, cx));
                assert!(ws.complete_mention("$mention-test-skill", window, cx));
                let draft = ws.input.read(cx).value().to_string();
                assert!(draft.starts_with("use") && draft.contains("Do the thing.") && !draft.contains("$mention"), "{draft}");
            })
        });
        let _ = std::fs::remove_dir_all(skill);
    }

    #[gpui_kit::test]
    async fn stash_keeps_attachment_paths(cx: &mut TestAppContext) {
        let (ws, cx) = open(cx);
        cx.update(|window, cx| {
            ws.update(cx, |ws, cx| {
                let draft = "この画像 @shot.png と @notes.md を見て";
                ws.input.update(cx, |st, cx| set_input(st, draft, window, cx));
                ws.run(Act::Stash, window, cx);
                assert!(ws.input.read(cx).value().is_empty());
                // An empty composer restores the single stash, attachments included.
                ws.run(Act::Stash, window, cx);
                let restored = ws.input.read(cx).value().to_string();
                assert_eq!(restored, draft);
            })
        });
    }
}
