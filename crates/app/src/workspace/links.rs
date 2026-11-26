//! `greencode://` links: copy a prompt or thread link, and open one as a draft or thread.
//! Opening never sends anything and never changes mode or branch.

use super::*;
use greencode_core::links::{self, Link};

/// `claude`/`codex` are what T3 Code calls the providers GreenCode names `anthropic`/`openai`.
fn alias(provider: &str) -> &str {
    match provider {
        "claude" => "anthropic",
        "codex" => "openai",
        other => other,
    }
}

/// The `(backend, model)` a link asks for: a native `provider/model` or a model of an external backend.
fn resolve_model(
    native: &[String],
    backends: &BTreeMap<String, Vec<String>>,
    provider: Option<&str>,
    model: &str,
) -> Option<(String, String)> {
    let provider = provider.map(str::to_lowercase);
    let wanted = |name: &str| {
        provider
            .as_deref()
            .is_none_or(|p| name.eq_ignore_ascii_case(p) || name.eq_ignore_ascii_case(alias(p)))
    };
    let same = |id: &str| {
        id.eq_ignore_ascii_case(model)
            || id.rsplit('/').next().is_some_and(|tail| tail.eq_ignore_ascii_case(model))
    };
    let native = native
        .iter()
        .filter(|m| wanted(m.split_once('/').map_or("", |(p, _)| p)) && same(m))
        .map(|m| ("native".to_string(), m.clone()));
    let external = backends
        .iter()
        .filter(|(name, _)| wanted(name))
        .flat_map(|(name, ids)| ids.iter().filter(|id| same(id)).map(|id| (name.clone(), id.clone())));
    native.chain(external).next()
}

impl Workspace {
    fn copied(&mut self, label: &str, text: String, cx: &mut Context<Self>) {
        cx.write_to_clipboard(gpui_kit::ClipboardItem::new_string(text));
        self.notice(format!("{label}をコピーしました"));
        cx.notify();
    }

    pub(super) fn copy_prompt_link(&mut self, with_repo: bool, cx: &mut Context<Self>) {
        let draft = self.input.read(cx).value().to_string();
        let t = self.thread();
        let (project, backend, id) = (t.project(), t.meta.backend.clone(), t.meta.model.clone());
        let (mut repo, mut provider, mut model) = (None, None, None);
        if with_repo {
            let Some(slug) = links::origin_slug(&project) else {
                return self.fail("GitHub の origin がないため、リポジトリ付きリンクは作れません");
            };
            repo = Some(slug);
            let (p, m) = match (backend.as_str(), id.split_once('/')) {
                ("native", Some((p, m))) => (p.to_string(), m.to_string()),
                _ => (backend.clone(), id.clone()),
            };
            (provider, model) = (Some(p), Some(m));
        }
        match links::prompt_link(&draft, repo.as_deref(), provider.as_deref(), model.as_deref()) {
            Ok(link) => self.copied("プロンプトリンク", link, cx),
            Err(e) => self.fail(format!("{e:#}")),
        }
    }

    pub(super) fn copy_thread_link(&mut self, cx: &mut Context<Self>) {
        let link = links::thread_link(&self.thread().meta.id);
        self.copied("スレッドリンク", link, cx);
    }

    pub(super) fn copy_thread_id(&mut self, cx: &mut Context<Self>) {
        let id = self.thread().meta.id.clone();
        self.copied("スレッドID", id, cx);
    }

    pub(super) fn open_link_from_clipboard(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match cx.read_from_clipboard().and_then(|c| c.text()) {
            Some(text) => self.open_link(&text, window, cx),
            None => self.fail("クリップボードにテキストがありません"),
        }
    }

    /// The most recent open project whose GitHub origin is `repo`.
    fn project_for(&self, repo: &str) -> Option<PathBuf> {
        let mut seen = HashSet::new();
        self.threads
            .iter()
            .map(Thread::project)
            .filter(|p| seen.insert(p.clone()))
            .find(|p| links::origin_slug(p).is_some_and(|slug| slug.eq_ignore_ascii_case(repo)))
    }

    /// Puts `prompt` in the composer, after anything already typed so no draft is lost.
    fn put_draft(&mut self, prompt: &str, window: &mut Window, cx: &mut Context<Self>) {
        let typed = self.input.read(cx).value().to_string();
        let text = if typed.trim().is_empty() { prompt.to_string() } else { format!("{typed}\n\n{prompt}") };
        self.threads[self.active].draft = text.clone();
        self.input.update(cx, |s, cx| {
            set_input(s, text, window, cx);
            s.focus(window, cx);
        });
    }

    pub fn open_link(&mut self, link: &str, window: &mut Window, cx: &mut Context<Self>) {
        match links::parse(link) {
            Err(e) => self.fail(format!("{e:#}")),
            Ok(Link::Open(path)) if path.is_dir() => self.new_thread(path, false, window, cx),
            Ok(Link::Open(path)) => self.fail(format!("フォルダがありません: {}", path.display())),
            Ok(Link::Thread(id, draft)) => {
                let Some(ix) = self.threads.iter().position(|t| t.meta.id == id) else {
                    return self.fail(format!("スレッドが見つかりません: {id}"));
                };
                self.select(ix, window, cx);
                self.apply_draft(&draft.prompt, draft.stripped, window, cx);
            }
            Ok(Link::New(draft)) => {
                let found = draft.repo.as_deref().and_then(|r| self.project_for(r));
                let project = found.clone().unwrap_or_else(|| self.thread().project());
                let linked = draft.pr.as_ref().and_then(|(_, n)| {
                    found.as_ref()?;
                    self.threads.iter().position(|t| {
                        t.project() == project
                            && t.linked_pr_manual.as_ref().or(t.linked_pr.as_ref()).is_some_and(|p| p.number == *n)
                    })
                });
                match linked {
                    Some(ix) => self.select(ix, window, cx),
                    None => self.new_thread(project.clone(), false, window, cx),
                }
                let mut notes = Vec::new();
                if let (Some(repo), None) = (&draft.repo, &found) {
                    notes.push(format!("{repo} のプロジェクトが開かれていないため、現在のプロジェクトに下書きを置きました"));
                }
                if let Some((_, n)) = draft.pr.as_ref().filter(|_| linked.is_none()) {
                    if found.is_some() {
                        self.run(Act::LinkPr(Some(*n)), window, cx);
                    }
                    notes.push(format!("PR #{n} のブランチは取得していません。必要なら先にチェックアウトしてください"));
                }
                if let Some(model) = &draft.model {
                    let choice = resolve_model(
                        &self.models(),
                        &self.models_for_project(&project),
                        draft.provider.as_deref(),
                        model,
                    );
                    match choice {
                        Some((backend, model)) if linked.is_none() => self.run(Act::BackendModel(backend, model), window, cx),
                        Some(_) => {}
                        None => notes.push("指定のモデルを使えないため、既定のモデルを使います".into()),
                    }
                }
                for note in notes {
                    self.notice(note);
                }
                self.apply_draft(&draft.prompt, draft.stripped, window, cx);
            }
        }
        cx.notify();
    }

    fn apply_draft(&mut self, prompt: &str, stripped: bool, window: &mut Window, cx: &mut Context<Self>) {
        if stripped {
            self.notice("⚠ プロンプトに見えない文字が含まれていたため削除しました。悪意のある指示の可能性があります。内容をよく確認してください");
        }
        if !prompt.is_empty() {
            self.put_draft(prompt, window, cx);
            self.notice("リンクから下書きを開きました。内容を確認してから送信してください");
        }
    }
}

#[cfg(test)]
mod tests {
    // No glob import: it would bring in a `test` macro that shadows `#[test]`.
    use super::resolve_model;
    use std::collections::BTreeMap;

    #[test]
    fn picks_the_model_a_link_names() {
        let native = vec!["anthropic/claude-opus".to_string(), "openai/gpt-5".to_string()];
        let backends = BTreeMap::from([("pi".to_string(), vec!["x/gpt-5".to_string()])]);
        let pick = |p, m| resolve_model(&native, &backends, p, m);
        assert_eq!(pick(Some("claude"), "claude-opus"), Some(("native".into(), "anthropic/claude-opus".into())));
        assert_eq!(pick(Some("codex"), "GPT-5"), Some(("native".into(), "openai/gpt-5".into())));
        assert_eq!(pick(Some("pi"), "gpt-5"), Some(("pi".into(), "x/gpt-5".into())));
        assert_eq!(pick(Some("openai"), "claude-opus"), None);
        assert_eq!(pick(None, "gpt-5"), Some(("native".into(), "openai/gpt-5".into())));
    }
}
