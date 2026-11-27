//! Sidebar actions are keyed by thread ID, not by the current selection or row position.
use super::*;
use gpui_kit::component::{menu::{PopupMenu, PopupMenuItem}, Icon};
use std::path::Path;

#[derive(Clone)]
enum Action {
    Tab,
    Window,
    Terminal,
    Rename,
    Pin,
    Unread,
    Color(Option<u32>),
    CopyId,
    Fork,
    Export,
    Project(PathBuf),
    Archive,
    Delete,
}

fn menu_item(
    label: impl Into<SharedString>,
    icon: Ic,
    action: Action,
    id: &str,
    workspace: &WeakEntity<Workspace>,
) -> PopupMenuItem {
    let id = id.to_owned();
    let workspace = workspace.clone();
    let icon = Icon::new(icon);
    PopupMenuItem::new(label).icon(icon).on_click(move |_, window, cx| {
        let _ = workspace.update(cx, |ws, cx| ws.thread_menu_action(&id, action.clone(), window, cx));
    })
}

pub(super) fn builder(
    id: String,
    cx: &Context<Workspace>,
) -> impl Fn(PopupMenu, &mut Window, &mut Context<PopupMenu>) -> PopupMenu + 'static {
    let workspace = cx.weak_entity();
    move |menu, window, cx| {
        let Some(ws) = workspace.upgrade() else { return menu };
        let ws = ws.read(cx);
        let Some(t) = ws.threads.iter().find(|t| t.meta.id == id) else { return menu };
        let item = |label, icon, action| menu_item(label, icon, action, &id, &workspace);
        let pinned = t.pinned;
        let unread = t.unread;
        let archived = t.archived;
        let has_session = Session::open(&id).is_some();
        let can_fork = t.meta.backend == "native" && has_session && t.status == Status::Idle;
        let can_delete = !ws.thread_busy(t);
        let removed = ws.pal(cx).removed;
        let color = t.sidebar_color;
        let current_project = t.sidebar_project().to_path_buf();
        let projects = ws.known_projects();
        let colors = (id.clone(), workspace.clone());
        let destinations = (id.clone(), workspace.clone());
        menu.item(item("新しいタブで開く", Ic::PanelTop, Action::Tab))
            .item(item("新しいウィンドウ", Ic::ExternalLink, Action::Window))
            .item(item("ターミナルで開く", Ic::SquareTerminal, Action::Terminal))
            .separator()
            .item(item("名前を変更…", Ic::Pencil, Action::Rename))
            .item(if pinned { item("ピン留めを解除", Ic::PinOff, Action::Pin) } else { item("ピン留め", Ic::Pin, Action::Pin) })
            .item(if unread { item("既読にする", Ic::MailOpen, Action::Unread) } else { item("未読にする", Ic::Mail, Action::Unread) })
            .submenu_with_icon(Some(Icon::new(Ic::Palette)), "外観", window, cx, move |menu, _, _| {
                [
                    ("既定", None), ("青", Some(0x4d91e0)), ("緑", Some(0x45a876)),
                    ("オレンジ", Some(0xd88339)), ("紫", Some(0xa280d8)),
                ].into_iter().fold(menu, |menu, (label, choice)| {
                    let item = menu_item(label, Ic::Circle, Action::Color(choice), &colors.0, &colors.1)
                        .checked(color == choice);
                    menu.item(if let Some(rgb_color) = choice {
                        item.icon(Icon::new(Ic::Circle).text_color(rgb(rgb_color)))
                    } else {
                        item
                    })
                })
            })
            .item(item("ID をコピー", Ic::Copy, Action::CopyId))
            .separator()
            .item(item("分岐", Ic::GitFork, Action::Fork).disabled(!can_fork))
            .item(item("エクスポート", Ic::CloudDownload, Action::Export).disabled(!has_session))
            .submenu_with_icon(Some(Icon::new(Ic::Folder)), "プロジェクトに移動", window, cx, move |menu, _, _| {
                projects.iter().fold(menu, |menu, project| {
                    menu.item(menu_item(project.display().to_string(), Ic::Folder,
                        Action::Project(project.clone()), &destinations.0, &destinations.1)
                        .checked(*project == current_project))
                })
            })
            .separator()
            .item(if archived { item("アーカイブから復元", Ic::ArchiveRestore, Action::Archive) } else { item("アーカイブ", Ic::Archive, Action::Archive) })
            .item(item("削除…", Ic::Trash, Action::Delete)
                .icon(Icon::new(Ic::Trash).text_color(removed))
                .disabled(!can_delete))
    }
}

impl Thread {
    pub(super) fn sidebar_project(&self) -> &Path {
        self.sidebar_project.as_deref().or(self.meta.repo.as_deref()).unwrap_or(&self.meta.cwd)
    }

    pub(super) fn load_items(&mut self) {
        if !self.loaded {
            if Session::open(&self.meta.id).is_some() {
                self.items = thread_view::session_items(&self.meta.id)
                    .unwrap_or_else(|e| vec![Item::Error(e.into())]);
            }
            self.loaded = true;
        }
    }
}

impl Workspace {
    pub(super) fn thread_busy(&self, t: &Thread) -> bool {
        t.status != Status::Idle
            || self.handoffs.contains(&t.meta.id)
            || greencode_core::handoff::is_open(&t.meta.id)
            || greencode_core::delegate::is_running(&t.meta.id)
    }

    fn save_thread(&mut self, ix: usize, cx: &mut Context<Self>) {
        if let Err(e) = self.threads[ix].save() {
            self.fail(e);
        }
        cx.notify();
    }

    pub(super) fn mark_thread_read(&mut self, ix: usize, cx: &mut Context<Self>) {
        if std::mem::take(&mut self.threads[ix].unread) {
            self.save_thread(ix, cx);
        }
    }

    pub(super) fn rename_thread(&mut self, ix: usize, title: &str, cx: &mut Context<Self>) {
        let title = title.trim();
        if !title.is_empty() {
            self.threads[ix].meta.title = title.chars().take(60).collect();
            self.save_thread(ix, cx);
        }
    }

    pub(super) fn toggle_thread_pin(&mut self, ix: usize) {
        let t = &mut self.threads[ix];
        self.undo.record(nav::SidebarAction::Pin { id: t.meta.id.clone(), value: t.pinned });
        t.pinned = !t.pinned;
    }

    pub(super) fn toggle_thread_archive(&mut self, ix: usize) {
        let t = &mut self.threads[ix];
        self.undo.record(nav::SidebarAction::Archive { id: t.meta.id.clone(), value: t.archived });
        t.archived = !t.archived;
    }

    pub(super) fn fork_thread(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        let t = &self.threads[ix];
        if t.meta.backend != "native" || Session::open(&t.meta.id).is_none() {
            return self.fail("複製できるのは、会話のあるネイティブスレッドだけです");
        }
        match Session::fork(&t.meta.id) {
            Ok(meta) => {
                self.threads.insert(0, Thread::new(meta, false, self.cfg.mode));
                self.active += 1;
                self.select(0, window, cx);
            }
            Err(e) => self.fail(format!("複製できません: {e:#}")),
        }
    }

    pub(super) fn export_thread(&mut self, ix: usize) {
        let id = &self.threads[ix].meta.id;
        let dir = greencode_core::home().join("exports");
        let path = dir.join(format!("{id}.md"));
        match Session::markdown(id).and_then(|md| {
            std::fs::create_dir_all(&dir)?;
            Ok(std::fs::write(&path, md)?)
        }) {
            Ok(()) => {
                self.notice(format!("エクスポートしました: {}", path.display()));
                spawn_external("/usr/bin/open", &["-R", &path.to_string_lossy()]);
            }
            Err(e) => self.fail(format!("エクスポートできません: {e:#}")),
        }
    }

    fn thread_menu_action(&mut self, id: &str, action: Action, window: &mut Window, cx: &mut Context<Self>) {
        let Some(ix) = self.threads.iter().position(|t| t.meta.id == id) else { return };
        match action {
            Action::Tab => {
                for i in [self.active, ix] {
                    if !self.thread_tabs.contains(&self.threads[i].meta.id) {
                        self.thread_tabs.push(self.threads[i].meta.id.clone());
                    }
                }
                self.select(ix, window, cx);
            }
            Action::Window => self.open_thread_window(ix, window, cx),
            Action::Terminal => {
                self.select(ix, window, cx);
                if self.active_terminal().is_none() {
                    self.new_terminal(window, cx);
                }
                if let Some(term) = self.active_terminal() {
                    term.input.update(cx, |s, cx| s.focus(window, cx));
                    self.terminal_visible = true;
                }
            }
            Action::Rename => {
                let title = self.threads[ix].meta.title.clone();
                self.text_prompt = Some(git::TextPrompt::RenameThread(id.into()));
                self.open_palette(&title, window, cx);
                if let Some((input, _)) = &self.palette {
                    input.update(cx, |s, cx| s.set_selected_range(0..title.len(), cx));
                }
            }
            Action::CopyId => cx.write_to_clipboard(ClipboardItem::new_string(id.to_string())),
            Action::Fork => self.fork_thread(ix, window, cx),
            Action::Export => self.export_thread(ix),
            Action::Delete => {
                if self.thread_busy(&self.threads[ix]) { return; }
                let message = format!("「{}」を削除しますか？", self.threads[ix].meta.title);
                let response = window.prompt(PromptLevel::Warning, &message,
                    Some("会話の履歴を削除します。作業フォルダやブランチは残ります。"),
                    &[PromptButton::cancel("キャンセル"), PromptButton::new("削除")], cx);
                let id = id.to_string();
                cx.spawn_in(window, async move |ws, cx| {
                    if response.await == Ok(1) {
                        let _ = ws.update_in(cx, |ws, window, cx| ws.run(Act::DeleteThread(id), window, cx));
                    }
                }).detach();
            }
            action => {
                match action {
                    Action::Pin => self.toggle_thread_pin(ix),
                    Action::Archive => self.toggle_thread_archive(ix),
                    Action::Unread => self.threads[ix].unread = !self.threads[ix].unread,
                    Action::Color(color) => self.threads[ix].sidebar_color = color,
                    Action::Project(project) => {
                        let t = &mut self.threads[ix];
                        let original = t.meta.repo.as_ref().unwrap_or(&t.meta.cwd);
                        t.sidebar_project = (*original != project).then_some(project);
                    }
                    _ => unreachable!(),
                }
                self.save_thread(ix, cx);
                return;
            }
        }
        cx.notify();
    }

    pub(super) fn thread_tabs(&self, pal: Pal, cx: &Context<Self>) -> Option<Stateful<Div>> {
        if self.thread_tabs.is_empty() { return None; }
        Some(div().id("thread-tabs").flex().flex_shrink_0().gap_1().px_3().py_1()
            .overflow_x_scroll().border_b_1().border_color(pal.line)
            .children(self.thread_tabs.iter().filter_map(|id| {
                let t = self.threads.iter().find(|t| &t.meta.id == id)?;
                let label = if t.meta.title.is_empty() { "新しいスレッド" } else { &t.meta.title };
                let selected = *id == self.thread().meta.id;
                let (tab_id, close_id) = (id.clone(), id.clone());
                Some(div().flex().flex_shrink_0().items_center().rounded_md().when(selected, |d| d.bg(pal.selected))
                    .child(control_button((ElementId::from("thread-tab"), SharedString::from(id.clone())), label.to_owned(), pal)
                        .max_w(px(200.)).truncate()
                        .on_click(cx.listener(move |ws, _: &ClickEvent, window, cx| ws.run(Act::Thread(tab_id.clone()), window, cx))))
                    .child(control_button((ElementId::from("close-thread-tab"), SharedString::from(id.clone())), "×", pal)
                        .accessibility_label(format!("タブを閉じる: {label}"))
                        .on_click(cx.listener(move |ws, _: &ClickEvent, window, cx| {
                            if let Some(i) = ws.thread_tabs.iter().position(|id| id == &close_id) {
                                ws.thread_tabs.remove(i);
                                if ws.thread().meta.id == close_id {
                                    if let Some(id) = ws.thread_tabs.get(i).or_else(|| ws.thread_tabs.last()) {
                                        ws.run(Act::Thread(id.clone()), window, cx);
                                    }
                                }
                                cx.notify();
                            }
                        }))))
            })))
    }

    fn open_thread_window(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        let id = self.threads[ix].meta.id.clone();
        let main = window.window_handle();
        let workspace = cx.weak_entity();
        // GPUI draws new windows synchronously; the viewer must read the workspace after this update.
        cx.defer(move |cx| {
            let Some(ws) = workspace.upgrade() else { return };
            let existing = ws.read(cx).thread_windows.get(&id).copied();
            if existing.is_some_and(|handle| {
                handle.update(cx, |_, window, _| window.activate_window()).is_ok()
            }) { return; }
            let Some(title) = ws.update(cx, |ws, _| {
                let t = ws.threads.iter_mut().find(|t| t.meta.id == id)?;
                t.load_items();
                Some(t.meta.title.clone())
            }) else { return };
            let options = WindowOptions {
                titlebar: Some(TitlebarOptions { title: Some(title.into()), ..Default::default() }),
                window_bounds: Some(WindowBounds::Windowed(Bounds::centered(None, size(px(820.), px(640.)), cx))),
                ..Default::default()
            };
            let result = gpui_kit::open_window(options, cx, |_, cx| cx.new(|cx| {
                cx.observe(&ws, |_: &mut ThreadWindow, _, cx| cx.notify()).detach();
                cx.on_release(|viewer: &mut ThreadWindow, cx| {
                    let _ = viewer.workspace.update(cx, |ws, _| {
                        ws.thread_windows.remove(&viewer.id);
                        if ws.thread().meta.id != viewer.id {
                            if let Some(t) = ws.threads.iter_mut().find(|t| t.meta.id == viewer.id && t.status == Status::Idle) {
                                t.items = Vec::new();
                                t.loaded = false;
                            }
                        }
                    });
                }).detach();
                ThreadWindow { id: id.clone(), workspace: workspace.clone(), main,
                    list: ListState::new(0, ListAlignment::Bottom, px(600.)) }
            }));
            ws.update(cx, |ws, cx| match result {
                Ok((handle, _)) => { ws.thread_windows.insert(id, handle); }
                Err(e) => { ws.fail(format!("ウィンドウを開けません: {e:#}")); cx.notify(); }
            });
        });
    }
}

struct ThreadWindow {
    id: String,
    workspace: WeakEntity<Workspace>,
    main: AnyWindowHandle,
    list: ListState,
}

impl Render for ThreadWindow {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(ws) = self.workspace.upgrade() else { return div().child("スレッドが見つかりません") };
        let ws = ws.read(cx);
        let Some(t) = ws.threads.iter().find(|t| t.meta.id == self.id) else { return div().child("スレッドが見つかりません") };
        let pal = ws.pal(cx);
        let n = t.items.len();
        let old = self.list.item_count();
        if n < old { self.list.reset(n); }
        else if n > old { self.list.splice(old..old, n - old); }
        if n != 0 { self.list.remeasure_items(n - 1..n); }
        let main = self.main;
        let workspace = self.workspace.clone();
        let id = self.id.clone();
        div().size_full().flex().flex_col().bg(pal.panel).text_color(pal.fg)
            .child(div().flex().items_center().gap_2().p_3().border_b_1().border_color(pal.line)
                .child(div().flex_1().truncate().font_weight(FontWeight::SEMIBOLD).child(t.meta.title.clone()))
                .child(div().text_sm().text_color(pal.muted).child("閲覧のみ"))
                .when(cx.windows().contains(&main), |d| d.child(control_button("continue-in-main", "メインで開く", pal)
                    .on_click(move |_, _, cx| {
                        let _ = main.update(cx, |_, window, cx| {
                            let _ = workspace.update(cx, |ws, cx| ws.run(Act::Thread(id.clone()), window, cx));
                            window.activate_window();
                        });
                    }))))
            .child(list(self.list.clone(), cx.processor(|view, ix, _, cx| view.item(ix, cx))).flex_1())
    }
}

impl ThreadWindow {
    fn item(&mut self, ix: usize, cx: &Context<Self>) -> AnyElement {
        let Some(workspace) = self.workspace.upgrade() else { return div().into_any_element() };
        let ws = workspace.read(cx);
        let item = ws.threads.iter().find(|t| t.meta.id == self.id).and_then(|t| t.items.get(ix));
        let Some(item) = item else { return div().into_any_element() };
        thread_view::render_item(ix, item, ws.pal(cx), cx.theme().mono_font_family.clone(),
            cx.listener(move |view, _: &ClickEvent, _, cx| {
                let _ = view.workspace.update(cx, |ws, cx| {
                    let Some(t) = ws.threads.iter_mut().find(|t| t.meta.id == view.id) else { return };
                    if let Some(Item::Tool { open, .. }) = t.items.get_mut(ix) {
                        *open = !*open;
                        if ws.thread().meta.id == view.id { ws.list.remeasure_items(ix..ix + 1); }
                        view.list.remeasure_items(ix..ix + 1);
                        cx.notify();
                    }
                });
            }),
            |url, _, _, cx| {
                if url::Url::parse(url).is_ok_and(|u| matches!(u.scheme(), "http" | "https" | "mailto")) {
                    cx.open_url(url);
                }
            })
    }
}

#[cfg(test)]
mod tests {
    use super::{super::tests::open, Action, Item, Meta, Mode, Session, Thread};
    use gpui_kit::{point, px, MouseButton, TestAppContext};
    use gpui_kit::test::{TestQueryExt, TestWindowExt};

    #[gpui_kit::test]
    async fn thread_menu_targets_the_clicked_thread(cx: &mut TestAppContext) {
        let (ws, cx) = open(cx);
        let (active, target, cwd, project) = ws.update(cx, |ws, _| {
            let active = ws.thread().meta.id.clone();
            let project = ws.thread().meta.cwd.clone();
            let cwd = project.join("other");
            let mut meta = Meta::new(cwd.clone(), "test/model".into(), "native".into());
            meta.title = "対象スレッド".into();
            Session::create(&meta).unwrap().append(&greencode_core::Message::user("保存済みの会話")).unwrap();
            let target = meta.id.clone();
            ws.threads.insert(0, Thread::new(meta, false, Mode::Ask));
            ws.active += 1;
            (active, target, cwd, project)
        });
        cx.simulate_input("現在の下書き");
        let row = cx.update(|window, cx| {
            window.render_frame(cx);
            let button = window.find(("thread-delete", 0usize)).bounds().center();
            point(button.x - px(48.), button.y)
        });
        cx.simulate_mouse_move(row, None, Default::default());
        cx.simulate_mouse_down(row, MouseButton::Right, Default::default());
        cx.simulate_mouse_up(row, MouseButton::Right, Default::default());
        let pin = cx.update(|window, cx| {
            window.render_frame(cx);
            for label in [
                "新しいタブで開く", "新しいウィンドウ", "ターミナルで開く", "名前を変更…",
                "未読にする", "外観", "ID をコピー", "分岐", "エクスポート",
                "プロジェクトに移動", "アーカイブ", "削除…",
            ] {
                assert!(window.find_by_label(label).visible(), "{label}");
            }
            window.find_by_label("ピン留め").bounds().center()
        });
        cx.simulate_mouse_move(pin, None, Default::default());
        cx.simulate_click(pin, Default::default());
        cx.update(|window, cx| ws.update(cx, |ws, cx| {
            for action in [Action::Unread, Action::Color(Some(0x4d91e0)), Action::Project(project.clone()), Action::Rename] {
                ws.thread_menu_action(&target, action, window, cx);
            }
        }));
        cx.simulate_input("変更後");
        cx.simulate_keystrokes("enter");
        ws.read_with(cx, |ws, cx| {
            assert_eq!(ws.thread().meta.id, active);
            assert_eq!(ws.input.read(cx).value().as_ref(), "現在の下書き");
        });
        let view = Session::view(&target).unwrap().unwrap();
        assert_eq!(view.meta.title, "変更後");
        assert_eq!(view.meta.cwd, cwd);
        assert_eq!(view.sidebar_project, Some(project));
        assert_eq!(view.sidebar_color, Some(0x4d91e0));
        assert!(view.pinned && view.unread);

        cx.update(|window, cx| ws.update(cx, |ws, cx| ws.thread_menu_action(&target, Action::Tab, window, cx)));
        ws.read_with(cx, |ws, _| {
            assert_eq!(ws.thread_tabs, [active, target.clone()]);
            assert_eq!(ws.thread().meta.id, target);
            assert!(matches!(ws.thread().items.first(), Some(Item::User(text)) if text.as_ref() == "保存済みの会話"));
        });
        assert!(!Session::view(&target).unwrap().unwrap().unread);

        cx.update(|window, cx| ws.update(cx, |ws, cx| ws.thread_menu_action(&target, Action::Window, window, cx)));
        cx.run_until_parked();
        let detached = ws.read_with(cx, |ws, _| ws.thread_windows[&target]);
        cx.update(|_, cx| {
            detached.update(cx, |_, window, cx| {
                window.render_frame(cx);
                assert!(window.find_by_label("メインで開く").visible());
                window.remove_window();
            }).unwrap();
        });
    }
}
