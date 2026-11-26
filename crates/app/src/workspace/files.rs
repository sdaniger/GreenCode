//! Right panel: project details, a file tree, and a read-only file viewer.

use super::*;
use gpui_kit::component::text::TextView;
use std::collections::BTreeSet;

const MAX_LINES: usize = 5_000;
const MAX_BYTES: u64 = 2 * 1024 * 1024;

/// What the viewer shows for the open file.
#[derive(Clone)]
pub(super) enum Content {
    Text(Vec<SharedString>),
    Image(PathBuf),
    /// A thumbnail is being rendered in the background.
    Pending,
    /// First page of a PDF; `None` when rendering failed.
    Pdf(Option<PathBuf>),
    Markdown {
        source: SharedString,
        lines: Vec<SharedString>,
    },
    /// HTML/SVG thumbnail; `None` when rendering failed.
    Rendered {
        png: Option<PathBuf>,
        lines: Vec<SharedString>,
    },
}

pub(super) struct FilesPanel {
    query: Entity<InputState>,
    pub(super) expanded: BTreeSet<String>,
    /// Relative path and its content.
    pub(super) open: Option<(String, Content)>,
    /// Shows the line-numbered source of a previewable file.
    show_source: bool,
    poll: Option<Task<()>>,
    render: Option<Task<()>>,
    _sub: Subscription,
}

impl FilesPanel {
    pub(super) fn new(window: &mut Window, cx: &mut Context<Workspace>) -> Self {
        let query = cx.new(|cx| InputState::new(window, cx).placeholder("ファイルを検索"));
        let _sub = cx.subscribe_in(&query, window, |_, _, ev: &InputEvent, _, cx| {
            if let InputEvent::Change = ev {
                cx.notify()
            }
        });
        Self {
            query,
            expanded: BTreeSet::new(),
            open: None,
            show_source: false,
            poll: None,
            render: None,
            _sub,
        }
    }
}

#[derive(Default)]
struct Node {
    dirs: std::collections::BTreeMap<String, Node>,
    files: Vec<String>,
}

pub(super) enum Row {
    Dir {
        path: String,
        name: String,
        depth: usize,
        open: bool,
    },
    File {
        path: String,
        name: String,
        depth: usize,
    },
}

fn walk(node: &Node, prefix: &str, depth: usize, expanded: &BTreeSet<String>, out: &mut Vec<Row>) {
    for (name, child) in &node.dirs {
        let path = format!("{prefix}{name}");
        let open = expanded.contains(&path);
        out.push(Row::Dir {
            path: path.clone(),
            name: name.clone(),
            depth,
            open,
        });
        if open {
            walk(child, &format!("{path}/"), depth + 1, expanded, out)
        }
    }
    for path in &node.files {
        let name = path.rsplit('/').next().unwrap_or(path).to_string();
        out.push(Row::File {
            path: path.clone(),
            name,
            depth,
        });
    }
}

/// Directories first, then files, both alphabetical; only expanded directories are descended.
pub(super) fn tree_rows(paths: &[String], expanded: &BTreeSet<String>) -> Vec<Row> {
    let mut root = Node::default();
    for path in paths {
        let mut node = &mut root;
        let mut parts = path.split('/').peekable();
        while let Some(part) = parts.next() {
            if parts.peek().is_some() {
                node = node.dirs.entry(part.to_string()).or_default();
            }
        }
        node.files.push(path.clone());
    }
    let mut out = vec![];
    walk(&root, "", 0, expanded, &mut out);
    out
}

fn load_lines(path: &std::path::Path) -> Vec<SharedString> {
    let unreadable = || vec![SharedString::from("表示できないファイルです")];
    match std::fs::metadata(path) {
        Ok(m) if m.len() <= MAX_BYTES => {}
        _ => return unreadable(),
    }
    match std::fs::read_to_string(path) {
        Ok(text) => text
            .lines()
            .take(MAX_LINES)
            .map(|l| SharedString::from(l.to_string()))
            .collect(),
        Err(_) => unreadable(),
    }
}

fn changed(prev: Option<std::time::SystemTime>, now: Option<std::time::SystemTime>) -> bool {
    prev != now
}

fn snapshot(png: &std::path::Path, dir: &std::path::Path) -> std::io::Result<PathBuf> {
    use std::io::ErrorKind;
    std::fs::create_dir_all(dir)?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    for n in 0u64.. {
        let path = dir.join(format!("{now:x}-{n:x}.png"));
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(mut dest) => {
                let result =
                    std::fs::File::open(png).and_then(|mut src| std::io::copy(&mut src, &mut dest));
                if let Err(e) = result {
                    let _ = std::fs::remove_file(&path);
                    return Err(e);
                }
                return Ok(path);
            }
            Err(e) if e.kind() == ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    unreachable!()
}

/// Renders page 1 / a preview of `abs` to a PNG with Quick Look.
async fn thumbnail(abs: PathBuf) -> Option<PathBuf> {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    abs.hash(&mut h);
    std::fs::metadata(&abs).ok()?.modified().ok().hash(&mut h);
    let dir = std::env::temp_dir().join(format!(
        "greencode-preview-{}-{:x}",
        std::process::id(),
        h.finish()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).ok()?;
    tokio::process::Command::new("/usr/bin/qlmanage")
        .args(["-t", "-s", "1600", "-o"])
        .arg(&dir)
        .arg(&abs)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .await
        .ok()?;
    let png = dir.join(format!("{}.png", abs.file_name()?.to_string_lossy()));
    png.exists().then_some(png)
}

/// Counts added and removed lines of a unified diff.
fn diff_stats(diff: &[SharedString]) -> (usize, usize) {
    let added = diff
        .iter()
        .filter(|l| l.starts_with('+') && !l.starts_with("+++"))
        .count();
    let removed = diff
        .iter()
        .filter(|l| l.starts_with('-') && !l.starts_with("---"))
        .count();
    (added, removed)
}

impl Workspace {
    pub(super) fn toggle_files(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.files_panel.take().is_none() {
            self.files = None; // the agent may have created files since the list was cached
            self.files_panel = Some(FilesPanel::new(window, cx));
            self.show_diff = false;
            self.pr_panel = None;
            self.refresh_diff(cx);
        }
        cx.notify();
    }

    pub(super) fn open_file(&mut self, path: &str, cx: &mut Context<Self>) {
        let Some(panel) = &mut self.files_panel else {
            return;
        };
        panel.poll = None;
        panel.show_source = false;
        self.reload_file(path, cx);
        if cfg!(test) {
            return;
        }
        let path = path.to_string();
        let abs = self.thread().meta.cwd.join(&path);
        let mut mtime = std::fs::metadata(&abs).and_then(|m| m.modified()).ok();
        let poll = cx.spawn(async move |this, cx| loop {
            cx.background_executor()
                .timer(std::time::Duration::from_secs(2))
                .await;
            let now = std::fs::metadata(&abs).and_then(|m| m.modified()).ok();
            let modified = changed(mtime, now);
            mtime = now;
            if !this
                .update(cx, |this, cx| {
                    if !this
                        .files_panel
                        .as_ref()
                        .and_then(|p| p.open.as_ref())
                        .is_some_and(|(open, _)| *open == path)
                        || this.thread().meta.cwd.join(&path) != abs
                    {
                        return false;
                    }
                    if modified {
                        gpui::ImageSource::from(abs.clone()).remove_asset(cx);
                        this.reload_file(&path, cx);
                    }
                    true
                })
                .unwrap_or(false)
            {
                break;
            }
        });
        if let Some(panel) = &mut self.files_panel {
            panel.poll = Some(poll);
        }
    }

    fn reload_file(&mut self, path: &str, cx: &mut Context<Self>) {
        let abs = self.thread().meta.cwd.join(path);
        let ext = abs
            .extension()
            .map(|e| e.to_string_lossy().to_lowercase())
            .unwrap_or_default();
        let (content, render) = match ext.as_str() {
            "png" | "jpg" | "jpeg" | "gif" | "webp" => (Content::Image(abs.clone()), false),
            "pdf" | "html" | "htm" | "svg" => (Content::Pending, true),
            "md" => {
                let lines = load_lines(&abs);
                let source = SharedString::from(
                    lines
                        .iter()
                        .map(|l| l.as_ref())
                        .collect::<Vec<_>>()
                        .join("\n"),
                );
                (Content::Markdown { source, lines }, false)
            }
            _ => (Content::Text(load_lines(&abs)), false),
        };
        let Some(panel) = &mut self.files_panel else {
            return;
        };
        panel.render = None;
        panel.open = Some((path.to_string(), content));
        cx.notify();
        if !render || cfg!(test) {
            return;
        }
        let task = self.rt.spawn(thumbnail(abs.clone()));
        let path = path.to_string();
        let render = cx.spawn(async move |this, cx| {
            let png = task.await.ok().flatten();
            let _ = this.update(cx, |this, cx| {
                let Some((open, content)) = this.files_panel.as_mut().and_then(|p| p.open.as_mut())
                else {
                    return;
                };
                if *open != path {
                    return;
                }
                *content = if abs
                    .extension()
                    .is_some_and(|e| e.eq_ignore_ascii_case("pdf"))
                {
                    Content::Pdf(png)
                } else {
                    Content::Rendered {
                        png,
                        lines: load_lines(&abs),
                    }
                };
                cx.notify();
            });
        });
        if let Some(panel) = &mut self.files_panel {
            panel.render = Some(render);
        }
    }

    fn details_row(id: &'static str, label: impl Into<SharedString>, pal: Pal) -> Stateful<Div> {
        div()
            .id(id)
            .px_2()
            .py_1()
            .rounded_md()
            .cursor_pointer()
            .hover(|s| s.bg(pal.hover))
            .child(label.into())
    }

    pub(super) fn files_panel_view(
        &mut self,
        pal: Pal,
        cx: &mut Context<Self>,
    ) -> Option<impl IntoElement> {
        let panel = self.files_panel.as_ref()?;
        let query_state = panel.query.clone();
        let query = panel.query.read(cx).value().trim().to_lowercase();
        let expanded = panel.expanded.clone();
        let open = panel.open.clone();
        let show_source = panel.show_source;
        let project = self.thread().meta.cwd.clone();
        let mono = cx.theme().mono_font_family.clone();

        let frame = div()
            .w(px(380.))
            .h_full()
            .flex()
            .flex_col()
            .flex_shrink_0()
            .overflow_hidden()
            .bg(pal.panel)
            .border_l_1()
            .border_color(pal.line);

        if let Some((path, content)) = open {
            let abs = project.join(&path);
            let name = path.rsplit('/').next().unwrap_or(&path).to_string();
            let lines_view = |lines: Vec<SharedString>| {
                let count = lines.len();
                let width = count.max(1).to_string().len();
                uniform_list("file-lines", count, move |range: Range<usize>, _, _| {
                    range
                        .map(|i| {
                            div()
                                .flex()
                                .gap_3()
                                .px_3()
                                .whitespace_nowrap()
                                .child(
                                    div()
                                        .flex_shrink_0()
                                        .w(px(8. * width as f32 + 4.))
                                        .text_right()
                                        .text_color(pal.muted)
                                        .child(SharedString::from((i + 1).to_string())),
                                )
                                .child(lines[i].clone())
                        })
                        .collect::<Vec<_>>()
                })
                .flex_1()
                .font_family(mono.clone())
                .text_xs()
                .into_any_element()
            };
            let note = |text: &'static str| {
                div()
                    .p_3()
                    .text_xs()
                    .text_color(pal.muted)
                    .child(text)
                    .into_any_element()
            };
            let picture = |p: &PathBuf| {
                div()
                    .p_2()
                    .child(
                        img(p.clone())
                            .w_full()
                            .object_fit(ObjectFit::Contain)
                            .rounded_md(),
                    )
                    .into_any_element()
            };
            let scroll = |child: AnyElement| {
                div()
                    .id("file-preview")
                    .flex_1()
                    .overflow_y_scroll()
                    .child(child)
                    .into_any_element()
            };
            let body = match &content {
                Content::Text(lines) => lines_view(lines.clone()),
                Content::Markdown { lines, .. } | Content::Rendered { lines, .. }
                    if show_source =>
                {
                    lines_view(lines.clone())
                }
                Content::Image(p) => scroll(picture(p)),
                Content::Pending => note("プレビューを作成中…"),
                Content::Pdf(Some(p)) => scroll(
                    div()
                        .child(note("1 ページ目のプレビュー"))
                        .child(picture(p))
                        .into_any_element(),
                ),
                Content::Pdf(None) | Content::Rendered { png: None, .. } => {
                    note("プレビューを作成できませんでした")
                }
                Content::Rendered { png: Some(p), .. } => scroll(picture(p)),
                Content::Markdown { source, .. } => scroll(
                    div()
                        .p_3()
                        .child(
                            TextView::markdown(("file-md", 0usize), source.clone())
                                .selectable(true),
                        )
                        .into_any_element(),
                ),
            };
            let toggle = matches!(content, Content::Markdown { .. } | Content::Rendered { .. })
                .then(|| {
                    chip(
                        "file-toggle",
                        if show_source {
                            "プレビュー"
                        } else {
                            "ソース"
                        },
                        pal,
                    )
                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                        if let Some(p) = &mut this.files_panel {
                            p.show_source = !p.show_source
                        }
                        cx.notify();
                    }))
                });
            let browser = {
                let abs = abs.clone();
                chip("file-browser", "ブラウザで開く", pal).on_click(cx.listener(
                    move |_, _: &ClickEvent, _, _| {
                        spawn_external("/usr/bin/open", &[&abs.to_string_lossy()]);
                    },
                ))
            };
            let snapshot = match &content {
                Content::Rendered { png: Some(p), .. } | Content::Pdf(Some(p)) => Some(p.clone()),
                _ => None,
            }
            .map(|png| {
                chip("file-snapshot", "スナップショットを添付", pal).on_click(cx.listener(
                    move |this, _: &ClickEvent, window, cx| match snapshot(
                        &png,
                        &greencode_core::home().join("attachments"),
                    ) {
                        Ok(path) => {
                            this.attach_paths(vec![path], window, cx);
                            this.notice("スナップショットを添付しました");
                        }
                        Err(e) => this.fail(format!("スナップショットを添付できません: {e}")),
                    },
                ))
            });
            return Some(
                frame
                    .child(
                        div()
                            .flex_shrink_0()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .px_3()
                            .py_2()
                            .border_b_1()
                            .border_color(pal.line)
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap_1()
                                    .child(
                                        div()
                                            .flex_1()
                                            .overflow_hidden()
                                            .whitespace_nowrap()
                                            .font_weight(FontWeight::MEDIUM)
                                            .child(SharedString::from(name)),
                                    )
                                    .child(chip("file-close", "閉じる", pal).on_click(
                                        cx.listener(|this, _: &ClickEvent, _, cx| {
                                            if let Some(p) = &mut this.files_panel {
                                                p.open = None;
                                                p.poll = None;
                                                p.render = None;
                                            }
                                            cx.notify();
                                        }),
                                    )),
                            )
                            .child(
                                div()
                                    .flex()
                                    .flex_wrap()
                                    .gap_1()
                                    .children(toggle)
                                    .child(browser)
                                    .children(snapshot),
                            ),
                    )
                    .child(body),
            );
        }

        // Details card.
        let (added, removed) = diff_stats(&self.diff);
        let project_str = project.to_string_lossy().into_owned();
        let mut card = div()
            .flex()
            .flex_col()
            .m_2()
            .p_1()
            .rounded_lg()
            .bg(pal.card)
            .text_xs()
            .child(
                div()
                    .px_2()
                    .py_1()
                    .font_weight(FontWeight::MEDIUM)
                    .child(SharedString::from(project_name(&project))),
            );
        if !self.branch.is_empty() {
            card = card.child(
                Self::details_row("details-branch", format!("⎇ {}", self.branch), pal).on_click(
                    cx.listener(|this, _: &ClickEvent, window, cx| {
                        this.open_palette("ブランチ", window, cx)
                    }),
                ),
            );
        }
        card = self.repo_rows(card, pal, cx);
        let p1 = project_str.clone();
        let p2 = project_str;
        card = card
            .child(
                Self::details_row("details-vscode", "VS Code で開く", pal).on_click(cx.listener(
                    move |_, _: &ClickEvent, _, _| {
                        spawn_external("/usr/bin/open", &["-a", "Visual Studio Code", &p1]);
                    },
                )),
            )
            .child(
                Self::details_row("details-finder", "Finder で表示", pal).on_click(cx.listener(
                    move |_, _: &ClickEvent, _, _| {
                        spawn_external("/usr/bin/open", &[&p2]);
                    },
                )),
            )
            .child(
                Self::details_row("details-changes", format!("変更 +{added} −{removed}"), pal)
                    .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                        this.run(Act::ToggleDiff, window, cx)
                    })),
            );

        // Tree, or a flat match list while searching.
        let paths = self.files().to_vec();
        let rows: Vec<Row> = if query.is_empty() {
            tree_rows(&paths, &expanded)
        } else {
            paths
                .iter()
                .filter(|p| p.to_lowercase().contains(&query))
                .take(200)
                .map(|p| Row::File {
                    path: p.clone(),
                    name: p.clone(),
                    depth: 0,
                })
                .collect()
        };
        let rows = Arc::new(rows);
        let count = rows.len();
        let tree = uniform_list(
            "file-tree",
            count,
            cx.processor(move |_: &mut Self, range: Range<usize>, _, cx| {
                range
                    .map(|i| match &rows[i] {
                        Row::Dir {
                            path,
                            name,
                            depth,
                            open,
                        } => {
                            let path = path.clone();
                            div()
                                .id(("dir", i))
                                .flex()
                                .items_center()
                                .h(px(24.))
                                .pr_2()
                                .pl(px(8. + 12. * *depth as f32))
                                .cursor_pointer()
                                .hover(|s| s.bg(pal.hover))
                                .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                                    if let Some(p) = &mut this.files_panel {
                                        if !p.expanded.remove(&path) {
                                            p.expanded.insert(path.clone());
                                        }
                                    }
                                    cx.notify();
                                }))
                                .child(SharedString::from(format!(
                                    "{} {name}",
                                    if *open { "▾" } else { "▸" }
                                )))
                        }
                        Row::File { path, name, depth } => {
                            let (open_path, at_path) = (path.clone(), path.clone());
                            div()
                                .id(("file", i))
                                .flex()
                                .items_center()
                                .h(px(24.))
                                .pr_2()
                                .pl(px(8. + 12. * *depth as f32))
                                .cursor_pointer()
                                .hover(|s| s.bg(pal.hover))
                                .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                                    this.open_file(&open_path, cx)
                                }))
                                .child(
                                    div()
                                        .flex_1()
                                        .overflow_hidden()
                                        .whitespace_nowrap()
                                        .child(SharedString::from(name.clone())),
                                )
                                .child(chip(("at", i), "@", pal).on_click(cx.listener(
                                    move |this, _: &ClickEvent, window, cx| {
                                        cx.stop_propagation();
                                        this.insert_path(
                                            std::path::Path::new(&format!("@{at_path}")),
                                            window,
                                            cx,
                                        );
                                    },
                                )))
                        }
                    })
                    .collect::<Vec<_>>()
            }),
        )
        .flex_1()
        .text_xs();

        Some(
            frame
                .child(
                    div()
                        .h(px(44.))
                        .flex_shrink_0()
                        .flex()
                        .items_center()
                        .px_3()
                        .border_b_1()
                        .border_color(pal.line)
                        .font_weight(FontWeight::MEDIUM)
                        .child("ファイル"),
                )
                .child(card)
                .child(div().px_2().pb_1().child(Input::new(&query_state)))
                .child(tree),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::{changed, snapshot};
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    #[test]
    fn detects_mtime_changes() {
        let t = UNIX_EPOCH + Duration::from_secs(10);
        assert!(!changed(Some(t), Some(t)));
        assert!(changed(Some(t), Some(t + Duration::from_secs(1))));
        assert!(changed(Some(t), None));
        assert!(changed(None, Some(t)));
        assert!(!changed(None, None));
    }

    #[test]
    fn snapshot_copies_to_unique_attachment_paths() {
        let dir = std::env::temp_dir().join(format!(
            "greencode-snapshot-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let png = dir.join("source.png");
        std::fs::write(&png, b"png bytes").unwrap();
        let attachments = dir.join("attachments");
        let a = snapshot(&png, &attachments).unwrap();
        let b = snapshot(&png, &attachments).unwrap();
        assert_ne!(a, b);
        assert_eq!(a.parent(), Some(attachments.as_path()));
        assert_eq!(b.parent(), Some(attachments.as_path()));
        assert_eq!(std::fs::read(a).unwrap(), b"png bytes");
        assert_eq!(std::fs::read(b).unwrap(), b"png bytes");
        std::fs::remove_dir_all(dir).unwrap();
    }
}
