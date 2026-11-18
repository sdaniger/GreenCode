//! Right panel: open pull requests of the project (via `gh`), with a detail view.

use super::*;
use gpui_kit::component::text::TextView;
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) enum Review {
    None,
    Approved,
    ChangesRequested,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) enum Checks {
    None,
    Passing,
    Failing,
    Pending,
}

#[derive(Debug, Clone, PartialEq)]
pub(super) struct PrRow {
    pub number: u64,
    pub title: String,
    pub head: String,
    pub author: String,
    pub draft: bool,
    pub review: Review,
    pub checks: Checks,
}

#[derive(Debug, Clone, PartialEq)]
pub(super) struct PrDetail {
    pub number: u64,
    pub title: String,
    pub body: String,
    pub author: String,
    pub head: String,
    pub base: String,
    pub url: String,
    /// Comments, then reviews, each as (author, body).
    pub discussion: Vec<(String, String)>,
    pub files: Vec<String>,
    /// (name, conclusion); the conclusion is `PENDING` while a check has not finished.
    pub checks: Vec<(String, String)>,
}

fn text(v: &Value, key: &str) -> String {
    v[key].as_str().unwrap_or_default().to_string()
}

fn login(v: &Value) -> String {
    text(&v["author"], "login")
}

fn review(v: &Value) -> Review {
    match v["reviewDecision"].as_str() {
        Some("APPROVED") => Review::Approved,
        Some("CHANGES_REQUESTED") => Review::ChangesRequested,
        _ => Review::None,
    }
}

/// `(name, conclusion)` of each entry of `statusCheckRollup` (check runs and commit statuses).
fn checks(v: &Value) -> Vec<(String, String)> {
    v["statusCheckRollup"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|c| {
            let name = c["name"]
                .as_str()
                .or(c["context"].as_str())
                .unwrap_or_default()
                .to_string();
            let done = c["conclusion"]
                .as_str()
                .filter(|s| !s.is_empty())
                .or(c["state"].as_str())
                .filter(|s| !s.is_empty());
            (name, done.unwrap_or("PENDING").to_string())
        })
        .collect()
}

fn summary(checks: &[(String, String)]) -> Checks {
    let is = |c: &str, set: &[&str]| set.contains(&c);
    if checks.is_empty() {
        Checks::None
    } else if checks
        .iter()
        .any(|(_, c)| is(c, &["FAILURE", "ERROR", "TIMED_OUT", "STARTUP_FAILURE"]))
    {
        Checks::Failing
    } else if checks.iter().all(|(_, c)| is(c, &["SUCCESS"])) {
        Checks::Passing
    } else {
        Checks::Pending
    }
}

pub(super) fn parse_list(json: &[u8]) -> Vec<PrRow> {
    let Ok(Value::Array(rows)) = serde_json::from_slice::<Value>(json) else {
        return vec![];
    };
    rows.iter()
        .filter_map(|v| {
            Some(PrRow {
                number: v["number"].as_u64()?,
                title: text(v, "title"),
                head: text(v, "headRefName"),
                author: login(v),
                draft: v["isDraft"].as_bool().unwrap_or(false),
                review: review(v),
                checks: summary(&checks(v)),
            })
        })
        .collect()
}

pub(super) fn parse_view(json: &[u8]) -> Option<PrDetail> {
    let v: Value = serde_json::from_slice(json).ok()?;
    let mut discussion = vec![];
    for key in ["comments", "reviews"] {
        for c in v[key].as_array().into_iter().flatten() {
            let body = text(c, "body");
            if !body.trim().is_empty() {
                discussion.push((login(c), body));
            }
        }
    }
    Some(PrDetail {
        number: v["number"].as_u64()?,
        title: text(&v, "title"),
        body: text(&v, "body"),
        author: login(&v),
        head: text(&v, "headRefName"),
        base: text(&v, "baseRefName"),
        url: text(&v, "url"),
        discussion,
        files: v["files"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|f| f["path"].as_str().map(str::to_string))
            .collect(),
        checks: checks(&v),
    })
}

pub(super) fn review_prompt(n: u64) -> String {
    format!("Review pull request #{n} of this repository: read `gh pr view {n} --comments` and `gh pr diff {n}`. Report bugs, regressions, security problems and missing tests, most serious first, each with file and line. Do not modify files, and do not post anything to the pull request.")
}

#[derive(Clone, Debug, PartialEq)]
struct DiffLine {
    file: String,
    new_line: Option<u32>,
    old_line: Option<u32>,
    text: String,
    kind: char,
}

fn diff_lines(diff: &str) -> Vec<DiffLine> {
    let (mut file, mut old, mut new, mut in_hunk) = (String::new(), 0u32, 0u32, false);
    let mut result = Vec::new();
    for line in diff.lines() {
        if let Some(path) = line
            .strip_prefix("diff --git ")
            .and_then(|s| s.rsplit_once(" b/").map(|(_, p)| p))
        {
            file = path.to_string();
            in_hunk = false;
        } else if let Some(path) = line.strip_prefix("+++ b/") {
            file = path.to_string();
        }
        if let Some(hunk) = line.strip_prefix("@@ ") {
            let mut parts = hunk.split_whitespace();
            let parse = |s: &str| {
                s.get(1..)
                    .and_then(|s| s.split(',').next())
                    .and_then(|n| n.parse::<u32>().ok())
            };
            if let (Some(a), Some(b)) = (parts.next().and_then(parse), parts.next().and_then(parse))
            {
                old = a;
                new = b;
                in_hunk = true;
            }
        }
        let kind = line.chars().next().unwrap_or(' ');
        let (o, n) = if in_hunk && !line.starts_with("@@") {
            match kind {
                '+' => {
                    let n = new;
                    new += 1;
                    (None, Some(n))
                }
                '-' => {
                    let o = old;
                    old += 1;
                    (Some(o), None)
                }
                ' ' => {
                    let (o, n) = (old, new);
                    old += 1;
                    new += 1;
                    (Some(o), Some(n))
                }
                _ => (None, None),
            }
        } else {
            (None, None)
        };
        result.push(DiffLine {
            file: file.clone(),
            new_line: n,
            old_line: o,
            text: line.to_string(),
            kind,
        });
    }
    result
}

#[derive(Clone, Debug)]
struct Comment {
    path: String,
    line: u32,
    side: String,
    body: String,
}
fn review_json(event: &str, body: &str, comments: &[Comment]) -> Value {
    json!({"event": event, "body": body, "comments": comments.iter().map(|c| json!({"path": c.path, "line": c.line, "side": c.side, "body": c.body})).collect::<Vec<_>>()})
}
fn viewed(ui: &Value, key: &str, path: &str) -> bool {
    ui["pr_viewed"][key]
        .as_array()
        .is_some_and(|v| v.iter().any(|v| v.as_str() == Some(path)))
}
fn toggle_viewed(ui: &mut Value, key: &str, path: &str) {
    if !ui["pr_viewed"].is_object() {
        ui["pr_viewed"] = json!({});
    }
    if !ui["pr_viewed"][key].is_array() {
        ui["pr_viewed"][key] = json!([]);
    }
    let paths = ui["pr_viewed"][key].as_array_mut().unwrap();
    if let Some(i) = paths.iter().position(|v| v.as_str() == Some(path)) {
        paths.remove(i);
    } else {
        paths.push(json!(path));
    }
}
async fn review_comments(
    cwd: std::path::PathBuf,
    number: u64,
) -> Result<(String, Vec<Comment>), String> {
    let repo = gh(
        cwd.clone(),
        ["repo", "view", "--json", "nameWithOwner"]
            .map(String::from)
            .to_vec(),
    )
    .await?;
    let repo: Value = serde_json::from_slice(&repo).map_err(|e| e.to_string())?;
    let repo = repo["nameWithOwner"]
        .as_str()
        .ok_or("リポジトリ名を取得できません")?
        .to_string();
    let data = gh(
        cwd,
        vec![
            "api".into(),
            "--paginate".into(),
            "--slurp".into(),
            format!("repos/{repo}/pulls/{number}/comments"),
        ],
    )
    .await?;
    let values: Vec<Value> = serde_json::from_slice(&data).map_err(|e| e.to_string())?;
    let comments = values
        .iter()
        .flat_map(|page| page.as_array().into_iter().flatten())
        .filter_map(|v| {
            Some(Comment {
                path: v["path"].as_str()?.into(),
                line: u32::try_from(v["line"].as_u64()?).ok()?,
                side: v["side"].as_str().unwrap_or("RIGHT").into(),
                body: v["body"].as_str()?.into(),
            })
        })
        .collect();
    Ok((repo, comments))
}
async fn post_review(
    cwd: std::path::PathBuf,
    repo: String,
    number: u64,
    payload: Value,
) -> Result<(), String> {
    use tokio::io::AsyncWriteExt;
    let mut child = tokio::process::Command::new("gh")
        .args([
            "api",
            "-X",
            "POST",
            &format!("repos/{repo}/pulls/{number}/reviews"),
            "--input",
            "-",
        ])
        .current_dir(cwd)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| e.to_string())?;
    child
        .stdin
        .take()
        .ok_or("stdin を開けません")?
        .write_all(payload.to_string().as_bytes())
        .await
        .map_err(|e| e.to_string())?;
    let out = child.wait_with_output().await.map_err(|e| e.to_string())?;
    if out.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}

async fn gh(cwd: std::path::PathBuf, args: Vec<String>) -> Result<Vec<u8>, String> {
    let out = tokio::process::Command::new("gh")
        .args(&args)
        .current_dir(cwd)
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true)
        .output()
        .await
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                "gh コマンドが見つかりません".to_string()
            } else {
                e.to_string()
            }
        })?;
    if out.status.success() {
        return Ok(out.stdout);
    }
    let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
    Err(if err.is_empty() {
        format!("gh が失敗しました（{}）", out.status)
    } else {
        err
    })
}

pub(super) enum Load<T> {
    Loading,
    Ready(T),
    Failed(String),
}

#[derive(Clone, Copy, PartialEq)]
pub(super) enum Tab {
    Overview,
    Files,
    Checks,
}

pub(super) struct Open {
    pub number: u64,
    pub tab: Tab,
    pub view: Load<PrDetail>,
    diff: Load<Vec<DiffLine>>,
    repo: Option<String>,
    comments: Load<Vec<Comment>>,
    drafts: Vec<Comment>,
    editing: Option<(String, u32, String, Entity<InputState>, Subscription)>,
    summary: Entity<InputState>,
    merge_confirm: bool,
    submitting: bool,
    error: Option<String>,
}

pub(super) struct PrPanel {
    pub list: Load<Vec<PrRow>>,
    pub open: Option<Open>,
}

fn error_box(e: &str, pal: Pal) -> Div {
    div()
        .p_3()
        .flex()
        .flex_col()
        .gap_1()
        .child(
            div()
                .text_xs()
                .text_color(pal.removed)
                .child(SharedString::from(e.to_string())),
        )
        .child(
            div()
                .text_xs()
                .text_color(pal.muted)
                .child("gh auth login を実行してください"),
        )
}

fn note(s: &str, pal: Pal) -> Div {
    div()
        .p_3()
        .text_xs()
        .text_color(pal.muted)
        .child(SharedString::from(s.to_string()))
}

fn conclusion_color(c: &str, pal: Pal) -> Hsla {
    match c {
        "SUCCESS" => pal.added,
        "FAILURE" | "ERROR" | "TIMED_OUT" | "STARTUP_FAILURE" => pal.removed,
        _ => pal.warn,
    }
}

impl Workspace {
    pub(super) fn toggle_prs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.pr_panel.take().is_some() {
            self.input.update(cx, |s, cx| s.focus(window, cx));
        } else {
            self.files_panel = None;
            self.show_diff = false;
            self.pr_panel = Some(PrPanel {
                list: Load::Loading,
                open: None,
            });
            self.load_prs(cx);
        }
        cx.notify();
    }

    fn load_prs(&mut self, cx: &mut Context<Self>) {
        let Some(p) = &mut self.pr_panel else { return };
        p.list = Load::Loading;
        if cfg!(test) {
            return;
        }
        let args = "pr list --json number,title,headRefName,author,isDraft,reviewDecision,statusCheckRollup,updatedAt --limit 50".split(' ').map(String::from).collect();
        let task = self.rt.spawn(gh(self.thread().meta.cwd.clone(), args));
        cx.spawn(async move |this, cx| {
            let r = task
                .await
                .unwrap_or_else(|e| Err(e.to_string()))
                .map(|b| parse_list(&b));
            this.update(cx, |this, cx| {
                if let Some(p) = &mut this.pr_panel {
                    p.list = match r {
                        Ok(v) => Load::Ready(v),
                        Err(e) => Load::Failed(e),
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn open_pr(&mut self, number: u64, window: &mut Window, cx: &mut Context<Self>) {
        let Some(p) = &mut self.pr_panel else { return };
        let summary = cx.new(|cx| InputState::new(window, cx).placeholder("レビューの概要"));
        p.open = Some(Open {
            number,
            tab: Tab::Overview,
            view: Load::Loading,
            diff: Load::Loading,
            repo: None,
            comments: Load::Loading,
            drafts: vec![],
            editing: None,
            summary,
            merge_confirm: false,
            submitting: false,
            error: None,
        });
        cx.notify();
        if cfg!(test) {
            return;
        }
        let cwd = self.thread().meta.cwd.clone();
        let n = number.to_string();
        let view_args = ["pr", "view", &n, "--json", "number,title,body,author,headRefName,baseRefName,state,reviewDecision,comments,reviews,files,statusCheckRollup,url"].map(String::from).to_vec();
        let diff_args = ["pr", "diff", &n].map(String::from).to_vec();
        let view = self.rt.spawn(gh(cwd.clone(), view_args));
        let diff = self.rt.spawn(gh(cwd.clone(), diff_args));
        let comments = self.rt.spawn(review_comments(cwd, number));
        cx.spawn(async move |this, cx| {
            let view = view
                .await
                .unwrap_or_else(|e| Err(e.to_string()))
                .and_then(|b| {
                    parse_view(&b).ok_or_else(|| "gh の出力を解釈できません".to_string())
                });
            let diff = diff
                .await
                .unwrap_or_else(|e| Err(e.to_string()))
                .map(|b| diff_lines(&String::from_utf8_lossy(&b)));
            let comments = comments.await.unwrap_or_else(|e| Err(e.to_string()));
            this.update(cx, |this, cx| {
                if let Some(o) = this
                    .pr_panel
                    .as_mut()
                    .and_then(|p| p.open.as_mut())
                    .filter(|o| o.number == number)
                {
                    o.view = match view {
                        Ok(v) => Load::Ready(v),
                        Err(e) => Load::Failed(e),
                    };
                    o.diff = match diff {
                        Ok(v) => Load::Ready(v),
                        Err(e) => Load::Failed(e),
                    };
                    match comments {
                        Ok((repo, comments)) => {
                            o.repo = Some(repo);
                            o.comments = Load::Ready(comments);
                        }
                        Err(e) => o.comments = Load::Failed(e),
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn request_pr_merge(&mut self, cx: &mut Context<Self>) {
        if let Some(o) = self.pr_panel.as_mut().and_then(|p| p.open.as_mut()) {
            o.merge_confirm = true;
        }
        cx.notify();
    }

    fn edit_pr_comment(
        &mut self,
        path: String,
        line: u32,
        side: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self
            .pr_panel
            .as_ref()
            .and_then(|p| p.open.as_ref())
            .is_none_or(|o| o.submitting)
        {
            return;
        }
        let field = cx.new(|cx| InputState::new(window, cx).placeholder("コメントを書く… ⏎"));
        let sub = cx.subscribe_in(&field, window, |this, field, ev: &InputEvent, _, cx| {
            if let InputEvent::PressEnter { .. } = ev {
                let body = field.read(cx).value().trim().to_string();
                if body.is_empty() {
                    return;
                }
                if let Some(o) = this.pr_panel.as_mut().and_then(|p| p.open.as_mut()) {
                    if let Some((path, line, side, _, _)) = o.editing.take() {
                        o.drafts.push(Comment {
                            path,
                            line,
                            side,
                            body,
                        });
                    }
                }
                cx.notify();
            }
        });
        field.update(cx, |s, cx| s.focus(window, cx));
        if let Some(o) = self.pr_panel.as_mut().and_then(|p| p.open.as_mut()) {
            o.editing = Some((path, line, side.into(), field, sub));
        }
        cx.notify();
    }

    fn submit_pr_review(&mut self, event: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(o) = self.pr_panel.as_mut().and_then(|p| p.open.as_mut()) else {
            return;
        };
        if o.submitting {
            return;
        }
        let Some(repo) = o.repo.clone() else {
            o.error = Some("リポジトリを取得できません".into());
            cx.notify();
            return;
        };
        let number = o.number;
        let payload = review_json(event, o.summary.read(cx).value().trim(), &o.drafts);
        o.submitting = true;
        o.error = None;
        let task = self.rt.spawn(post_review(
            self.thread().meta.cwd.clone(),
            repo,
            number,
            payload,
        ));
        cx.spawn_in(window, async move |this, cx| {
            let result = task.await.unwrap_or_else(|e| Err(e.to_string()));
            this.update_in(cx, |this, window, cx| {
                if this
                    .pr_panel
                    .as_ref()
                    .and_then(|p| p.open.as_ref())
                    .is_none_or(|o| o.number != number)
                {
                    return;
                }
                match result {
                    Ok(()) => {
                        this.notice("レビューを送信しました");
                        this.open_pr(number, window, cx);
                    }
                    Err(e) => {
                        if let Some(o) = this.pr_panel.as_mut().and_then(|p| p.open.as_mut()) {
                            o.submitting = false;
                            o.error = Some(e);
                        }
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn tab_chip(
        &self,
        id: &'static str,
        label: String,
        tab: Tab,
        current: Tab,
        pal: Pal,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        chip(id, label, pal)
            .when(tab == current, |c| c.bg(pal.selected).text_color(pal.fg))
            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                if let Some(o) = this.pr_panel.as_mut().and_then(|p| p.open.as_mut()) {
                    o.tab = tab
                }
                cx.notify();
            }))
    }

    pub(super) fn pr_panel_view(
        &self,
        pal: Pal,
        cx: &mut Context<Self>,
    ) -> Option<impl IntoElement> {
        let panel = self.pr_panel.as_ref()?;
        let frame = div()
            .w(px(520.))
            .h_full()
            .flex()
            .flex_col()
            .flex_shrink_0()
            .overflow_hidden()
            .bg(pal.panel)
            .border_l_1()
            .border_color(pal.line);
        let header = |title: SharedString| {
            div()
                .h(px(44.))
                .flex_shrink_0()
                .flex()
                .items_center()
                .gap_1()
                .px_3()
                .border_b_1()
                .border_color(pal.line)
                .font_weight(FontWeight::MEDIUM)
                .child(title)
        };
        let close = chip("pr-close", "閉じる  esc", pal)
            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| this.toggle_prs(window, cx)));

        let Some(open) = &panel.open else {
            let body = match &panel.list {
                Load::Loading => note("取得中…", pal).into_any_element(),
                Load::Failed(e) => error_box(e, pal).into_any_element(),
                Load::Ready(rows) if rows.is_empty() => {
                    note("オープンなプルリクエストはありません", pal).into_any_element()
                }
                Load::Ready(rows) => {
                    let mut col = div()
                        .id("pr-list")
                        .flex_1()
                        .overflow_y_scroll()
                        .flex()
                        .flex_col()
                        .p_1();
                    for (i, r) in rows.iter().enumerate() {
                        let n = r.number;
                        let glyph = if r.draft {
                            Some(("◌", pal.muted))
                        } else {
                            match r.review {
                                Review::Approved => Some(("✓", pal.added)),
                                Review::ChangesRequested => Some(("✕", pal.removed)),
                                Review::None => None,
                            }
                        };
                        let dot = match r.checks {
                            Checks::None => None,
                            Checks::Passing => Some(pal.added),
                            Checks::Failing => Some(pal.removed),
                            Checks::Pending => Some(pal.warn),
                        };
                        col = col.child(
                            div()
                                .id(("pr", i))
                                .flex()
                                .items_center()
                                .gap_2()
                                .px_2()
                                .py_1p5()
                                .rounded_md()
                                .cursor_pointer()
                                .hover(|s| s.bg(pal.hover))
                                .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                                    this.open_pr(n, window, cx)
                                }))
                                .child(div().text_color(pal.muted).child(format!("#{n}")))
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w_0()
                                        .flex()
                                        .flex_col()
                                        .child(
                                            div()
                                                .overflow_hidden()
                                                .whitespace_nowrap()
                                                .child(SharedString::from(r.title.clone())),
                                        )
                                        .child(
                                            div()
                                                .text_xs()
                                                .text_color(pal.muted)
                                                .overflow_hidden()
                                                .whitespace_nowrap()
                                                .child(SharedString::from(r.head.clone())),
                                        ),
                                )
                                .children(glyph.map(|(g, c)| div().text_color(c).child(g)))
                                .children(dot.map(|c| div().text_color(c).child("●"))),
                        );
                    }
                    col.into_any_element()
                }
            };
            return Some(
                frame
                    .child(
                        header("プルリクエスト".into())
                            .child(div().flex_1())
                            .child(chip("pr-refresh", "更新", pal).on_click(cx.listener(
                                |this, _: &ClickEvent, _, cx| {
                                    this.load_prs(cx);
                                    cx.notify();
                                },
                            )))
                            .child(close),
                    )
                    .child(body),
            );
        };

        let n = open.number;
        let title = match &open.view {
            Load::Ready(v) => format!("#{n} {}", v.title),
            _ => format!("#{n}"),
        };
        let url = match &open.view {
            Load::Ready(v) => v.url.clone(),
            _ => String::new(),
        };
        let counts = match &open.view {
            Load::Ready(v) => (v.files.len(), v.checks.len()),
            _ => (0, 0),
        };
        let body = match &open.view {
            Load::Loading => note("取得中…", pal).into_any_element(),
            Load::Failed(e) => error_box(e, pal).into_any_element(),
            Load::Ready(v) => match open.tab {
                Tab::Overview => {
                    let mut col = div()
                        .id("pr-overview")
                        .flex_1()
                        .overflow_y_scroll()
                        .p_3()
                        .flex()
                        .flex_col()
                        .gap_2()
                        .child(
                            div()
                                .font_weight(FontWeight::MEDIUM)
                                .child(SharedString::from(v.title.clone())),
                        )
                        .child(
                            div()
                                .text_xs()
                                .text_color(pal.muted)
                                .child(SharedString::from(format!(
                                    "{} · {} → {}",
                                    v.author, v.head, v.base
                                ))),
                        )
                        .child(
                            TextView::markdown(
                                ("pr-body", 0usize),
                                SharedString::from(v.body.clone()),
                            )
                            .selectable(true),
                        );
                    for (i, (author, body)) in v.discussion.iter().enumerate() {
                        col = col.child(
                            div()
                                .flex()
                                .flex_col()
                                .gap_1()
                                .pt_2()
                                .border_t_1()
                                .border_color(pal.line)
                                .child(
                                    div()
                                        .font_weight(FontWeight::BOLD)
                                        .child(SharedString::from(author.clone())),
                                )
                                .child(
                                    TextView::markdown(
                                        ("pr-comment", i),
                                        SharedString::from(body.clone()),
                                    )
                                    .selectable(true),
                                ),
                        );
                    }
                    col.into_any_element()
                }
                Tab::Files => match &open.diff {
                    Load::Loading => note("取得中…", pal).into_any_element(),
                    Load::Failed(e) => error_box(e, pal).into_any_element(),
                    Load::Ready(lines) if lines.is_empty() => {
                        note("差分はありません", pal).into_any_element()
                    }
                    Load::Ready(lines) => {
                        let key = open.repo.as_ref().map(|r| format!("{r}#{n}"));
                        let total = v.files.len();
                        let count = key.as_ref().map_or(0, |k| {
                            v.files.iter().filter(|f| viewed(&self.ui, k, f)).count()
                        });
                        let mut col = div()
                            .id("pr-diff")
                            .flex_1()
                            .overflow_y_scroll()
                            .flex()
                            .flex_col()
                            .text_xs();
                        col = col.child(div().p_2().child(format!("{count}/{total} 閲覧済み")));
                        if let Load::Failed(e) = &open.comments {
                            col = col.child(error_box(e, pal));
                        }
                        for (i, line) in lines.iter().enumerate() {
                            if line.text.starts_with("diff --git ") {
                                let path = line.file.clone();
                                let k = key.clone();
                                let checked =
                                    k.as_ref().is_some_and(|k| viewed(&self.ui, k, &path));
                                col = col.child(
                                    div()
                                        .flex()
                                        .items_center()
                                        .gap_1()
                                        .p_2()
                                        .child(SharedString::from(path.clone()))
                                        .child(
                                            chip(
                                                ("pr-viewed", i),
                                                if checked {
                                                    "☑ 閲覧済み"
                                                } else {
                                                    "☐ 閲覧済み"
                                                },
                                                pal,
                                            )
                                            .on_click(
                                                cx.listener(move |this, _: &ClickEvent, _, cx| {
                                                    if let Some(k) = &k {
                                                        toggle_viewed(&mut this.ui, k, &path);
                                                        this.save_ui();
                                                        cx.notify();
                                                    }
                                                }),
                                            ),
                                        ),
                                );
                            }
                            if key
                                .as_ref()
                                .is_some_and(|k| viewed(&self.ui, k, &line.file))
                            {
                                continue;
                            }
                            let color = match line.kind {
                                '+' if line.new_line.is_some() => pal.added,
                                '-' if line.old_line.is_some() => pal.removed,
                                '@' => pal.accent,
                                _ => pal.muted,
                            };
                            let target = line
                                .new_line
                                .map(|n| (n, "RIGHT"))
                                .or_else(|| line.old_line.map(|n| (n, "LEFT")));
                            let mut row = div()
                                .id(("pr-line", i))
                                .px_3()
                                .whitespace_nowrap()
                                .text_color(color)
                                .child(SharedString::from(line.text.clone()));
                            if let Some((number, side)) = target {
                                let path = line.file.clone();
                                row = row.cursor_pointer().on_click(cx.listener(
                                    move |this, _: &ClickEvent, window, cx| {
                                        this.edit_pr_comment(path.clone(), number, side, window, cx)
                                    },
                                ));
                            }
                            col = col.child(row);
                            if let Some((number, side)) = target {
                                for c in match &open.comments {
                                    Load::Ready(c) => c.as_slice(),
                                    _ => &[],
                                }
                                .iter()
                                .filter(|c| {
                                    c.path == line.file
                                        && ((c.side == "LEFT" && line.old_line == Some(c.line))
                                            || (c.side == "RIGHT" && line.new_line == Some(c.line)))
                                }) {
                                    col = col.child(
                                        div()
                                            .px_3()
                                            .text_color(pal.fg)
                                            .child(SharedString::from(c.body.clone())),
                                    );
                                }
                                for (j, c) in open.drafts.iter().enumerate().filter(|(_, c)| {
                                    c.path == line.file && c.line == number && c.side == side
                                }) {
                                    col = col.child(
                                        div()
                                            .px_3()
                                            .flex()
                                            .gap_1()
                                            .child("下書き")
                                            .child(SharedString::from(c.body.clone()))
                                            .child(
                                                chip(("pr-remove-draft", j), "削除", pal).on_click(
                                                    cx.listener(
                                                        move |this, _: &ClickEvent, _, cx| {
                                                            if let Some(o) = this
                                                                .pr_panel
                                                                .as_mut()
                                                                .and_then(|p| p.open.as_mut())
                                                            {
                                                                if !o.submitting
                                                                    && j < o.drafts.len()
                                                                {
                                                                    o.drafts.remove(j);
                                                                }
                                                            }
                                                            cx.notify();
                                                        },
                                                    ),
                                                ),
                                            ),
                                    );
                                }
                                if let Some((path, l, s, field, _)) = &open.editing {
                                    if path == &line.file && *l == number && s == side {
                                        col = col.child(div().px_3().child(Input::new(field)));
                                    }
                                }
                            }
                        }
                        col.into_any_element()
                    }
                },
                Tab::Checks if v.checks.is_empty() => {
                    note("チェックはありません", pal).into_any_element()
                }
                Tab::Checks => {
                    let mut col = div()
                        .id("pr-checks")
                        .flex_1()
                        .overflow_y_scroll()
                        .p_3()
                        .flex()
                        .flex_col()
                        .gap_1();
                    for (name, c) in &v.checks {
                        col = col.child(
                            div()
                                .flex()
                                .items_center()
                                .gap_2()
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w_0()
                                        .overflow_hidden()
                                        .whitespace_nowrap()
                                        .child(SharedString::from(name.clone())),
                                )
                                .child(
                                    div()
                                        .text_xs()
                                        .text_color(conclusion_color(c, pal))
                                        .child(SharedString::from(c.clone())),
                                ),
                        );
                    }
                    col.into_any_element()
                }
            },
        };
        let tabs = div()
            .flex_shrink_0()
            .flex()
            .items_center()
            .gap_1()
            .px_3()
            .py_1()
            .border_b_1()
            .border_color(pal.line)
            .child(self.tab_chip(
                "pr-tab-overview",
                "概要".into(),
                Tab::Overview,
                open.tab,
                pal,
                cx,
            ))
            .child(self.tab_chip(
                "pr-tab-files",
                format!("ファイル ({})", counts.0),
                Tab::Files,
                open.tab,
                pal,
                cx,
            ))
            .child(self.tab_chip(
                "pr-tab-checks",
                format!("チェック ({})", counts.1),
                Tab::Checks,
                open.tab,
                pal,
                cx,
            ));
        let review_bar = (open.tab == Tab::Files).then(|| {
            let mut bar = div()
                .flex()
                .flex_col()
                .gap_1()
                .p_2()
                .border_t_1()
                .border_color(pal.line)
                .child("レビューを送信")
                .child(Input::new(&open.summary));
            if let Some(e) = &open.error {
                bar = bar.child(
                    div()
                        .text_color(pal.removed)
                        .child(SharedString::from(e.clone())),
                );
            }
            for (id, label, event) in [
                ("pr-comment", "コメント", "COMMENT"),
                ("pr-approve", "承認", "APPROVE"),
                ("pr-request", "変更を要求", "REQUEST_CHANGES"),
            ] {
                bar = bar.child(chip(id, label, pal).on_click(cx.listener(
                    move |this, _: &ClickEvent, window, cx| {
                        this.submit_pr_review(event, window, cx)
                    },
                )));
            }
            bar
        });
        let actions = div().flex_shrink_0().flex().flex_wrap().items_center().gap_1().px_3().py_2().border_t_1().border_color(pal.line)
            .child(chip("pr-checkout", "チェックアウト", pal).on_click(cx.listener(move |this, _: &ClickEvent, window, cx| { this.run(Act::Shell(format!("gh pr checkout {n}")), window, cx); })))
            .child(chip("pr-review", "エージェントにレビュー依頼", pal).on_click(cx.listener(move |this, _: &ClickEvent, window, cx| { this.run(Act::Ask(review_prompt(n)), window, cx); })))
            .child(chip("pr-browser", "ブラウザで開く", pal).on_click(cx.listener(move |_, _: &ClickEvent, _, _| if !url.is_empty() { spawn_external("/usr/bin/open", &[&url]) })))
             .child(chip("pr-guide", "ガイドを作成", pal).on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                let project = this.thread().project().to_path_buf();
                this.new_thread(project, false, window, cx);
                this.threads[this.active].meta.backend = "native".into();
                this.thread().perms.set_mode(Mode::Plan);
                this.run(Act::Ask(format!("PR #{n} のレビューガイドを書いてください。gh pr diff {n} と gh pr view {n} を読み、読む順に立ち寄り先（ファイル:行と理由）を並べ、注意して見るべき箇所に「要確認」を付け、機械的な変更は最後にまとめてください。ファイルは変更しないでください。")), window, cx);
            })))
            .child(chip("pr-merge", "マージ（squash）", pal).on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.request_pr_merge(cx))))
            .when(open.merge_confirm, |d| d.child("マージしますか？")
                .child(chip("pr-merge-yes", "はい", pal).on_click(cx.listener(move |this, _: &ClickEvent, window, cx| { if let Some(o) = this.pr_panel.as_mut().and_then(|p| p.open.as_mut()) { o.merge_confirm = false; } this.run(Act::Shell(format!("gh pr merge {n} --squash")), window, cx); })))
                .child(chip("pr-merge-no", "いいえ", pal).on_click(cx.listener(|this, _: &ClickEvent, _, cx| { if let Some(o) = this.pr_panel.as_mut().and_then(|p| p.open.as_mut()) { o.merge_confirm = false; } cx.notify(); }))));
        Some(
            frame
                .child(
                    header(SharedString::from(title))
                        .child(div().flex_1())
                        .child(chip("pr-back", "‹ 一覧", pal).on_click(cx.listener(
                            |this, _: &ClickEvent, _, cx| {
                                if let Some(p) = &mut this.pr_panel {
                                    p.open = None
                                }
                                cx.notify();
                            },
                        )))
                        .child(close),
                )
                .child(tabs)
                .child(body)
                .children(review_bar)
                .child(actions),
        )
    }
}

#[cfg(test)]
mod tests {
    // No glob import: it would bring in a `test` macro that shadows `#[test]`.
    use super::{parse_list, parse_view, Checks, Review};

    #[gpui_kit::test]
    async fn pr_merge_first_click_only_requests_confirmation(cx: &mut gpui::TestAppContext) {
        let (ws, cx) = super::super::tests::open(cx);
        ws.update_in(cx, |ws, window, cx| {
            ws.toggle_prs(window, cx);
            ws.open_pr(7, window, cx);
            let queued = ws.thread().queue.len();
            ws.request_pr_merge(cx);
            assert!(
                ws.pr_panel
                    .as_ref()
                    .unwrap()
                    .open
                    .as_ref()
                    .unwrap()
                    .merge_confirm
            );
            assert_eq!(ws.thread().queue.len(), queued);
        });
    }

    #[test]
    fn pr_diff_tracks_both_sides() {
        let lines = super::diff_lines("diff --git a/a b/a\n--- a/a\n+++ b/a\n@@ -3,2 +3,2 @@\n same\n-old\n+new\ndiff --git a/b b/b\n--- a/b\n+++ b/b\n@@ -7 +8 @@\n-next\n+after\n");
        let rows: Vec<_> = lines
            .iter()
            .filter(|l| l.new_line.is_some() || l.old_line.is_some())
            .map(|l| (l.file.as_str(), l.old_line, l.new_line))
            .collect();
        assert_eq!(
            rows,
            [
                ("a", Some(3), Some(3)),
                ("a", Some(4), None),
                ("a", None, Some(4)),
                ("b", Some(7), None),
                ("b", None, Some(8))
            ]
        );
    }

    #[test]
    fn pr_review_payload_and_viewed_persistence() {
        let c = super::Comment {
            path: "a".into(),
            line: 3,
            side: "RIGHT".into(),
            body: "hello".into(),
        };
        assert_eq!(
            super::review_json("APPROVE", "ok", &[c]),
            serde_json::json!({"event":"APPROVE","body":"ok","comments":[{"path":"a","line":3,"side":"RIGHT","body":"hello"}]})
        );
        let mut ui = serde_json::json!({});
        super::toggle_viewed(&mut ui, "o/r#1", "a");
        let mut ui: serde_json::Value = serde_json::from_str(&ui.to_string()).unwrap();
        assert!(super::viewed(&ui, "o/r#1", "a"));
        assert!(!super::viewed(&ui, "o/r#2", "a"));
        super::toggle_viewed(&mut ui, "o/r#1", "a");
        assert!(!super::viewed(&ui, "o/r#1", "a"));
    }

    #[test]
    fn list_rows_carry_review_and_check_state() {
        let rows = parse_list(br#"[
          {"number":7,"title":"Fix","headRefName":"fix","author":{"login":"a"},"isDraft":false,"reviewDecision":"APPROVED",
           "statusCheckRollup":[{"name":"build","status":"COMPLETED","conclusion":"SUCCESS"},{"name":"test","status":"COMPLETED","conclusion":"FAILURE"}]},
          {"number":9,"title":"WIP","headRefName":"wip","author":{"login":"b"},"isDraft":true,"reviewDecision":"","statusCheckRollup":[]},
          {"title":"no number"}]"#);
        assert_eq!(rows.len(), 2);
        assert_eq!(
            (rows[0].review, rows[0].checks, rows[0].draft),
            (Review::Approved, Checks::Failing, false)
        );
        assert_eq!(
            (rows[1].review, rows[1].checks, rows[1].draft),
            (Review::None, Checks::None, true)
        );
        assert_eq!(
            (rows[0].head.as_str(), rows[0].author.as_str()),
            ("fix", "a")
        );
    }

    #[test]
    fn view_extracts_body_discussion_and_files() {
        let v = parse_view(br###"{"number":7,"title":"Fix","body":"## Why","author":{"login":"a"},"headRefName":"fix","baseRefName":"main","url":"https://x/7",
          "comments":[{"author":{"login":"c1"},"body":"one"},{"author":{"login":"c2"},"body":"two"}],
          "reviews":[{"author":{"login":"r"},"body":"","state":"APPROVED"}],
          "files":[{"path":"a.rs"},{"path":"b/c.rs"}],
          "statusCheckRollup":[{"name":"build","status":"IN_PROGRESS","conclusion":""}]}"###).unwrap();
        assert_eq!(v.body, "## Why");
        assert_eq!(
            v.discussion,
            [
                ("c1".to_string(), "one".to_string()),
                ("c2".to_string(), "two".to_string())
            ]
        );
        assert_eq!(v.files, ["a.rs", "b/c.rs"]);
        assert_eq!(v.checks, [("build".to_string(), "PENDING".to_string())]);
        assert_eq!((v.head.as_str(), v.base.as_str()), ("fix", "main"));
    }
}
