use super::{Act, Context, Window, Workspace};
use greencode_core::registry::{self, Entry};
use std::sync::Arc;

impl Workspace {
    fn registry_rows(entries: Vec<Entry>) -> Vec<(String, Act)> {
        let installed = registry::installed();
        entries
            .into_iter()
            .map(|entry| {
                let is_installed = installed.iter().any(|i| i.id == entry.id);
                let label = format!(
                    "{}  {}  {}{}",
                    entry.name,
                    entry.version,
                    entry.description,
                    if is_installed {
                        "  インストール済み · 削除"
                    } else {
                        ""
                    }
                );
                let action = if is_installed {
                    Act::RegistryRemove(entry.id.clone())
                } else {
                    Act::RegistryInstall(Box::new(entry))
                };
                (label, action)
            })
            .collect()
    }

    pub(super) fn open_registry(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.palette.is_none() {
            self.toggle_palette(window, cx);
        }
        self.registry_entries = Some(vec![]);
        self.notice("ACP レジストリを読み込み中…");
        let task = self
            .rt
            .spawn(async { registry::parse(&registry::fetch().await?) });
        cx.spawn_in(window, async move |this, cx| {
            let result = match task.await {
                Ok(r) => r.map_err(|e| format!("{e:#}")),
                Err(e) => Err(e.to_string()),
            };
            this.update_in(cx, |this, _, cx| {
                if this.registry_entries.is_none() {
                    return;
                }
                match result {
                    Ok(entries) => this.registry_entries = Some(Self::registry_rows(entries)),
                    Err(e) => {
                        this.registry_entries = None;
                        this.fail(e);
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    pub(super) fn install_registry(&mut self, entry: Entry, cx: &mut Context<Self>) {
        let name = entry.name.clone();
        self.notice("インストール中…");
        let task = self
            .rt
            .spawn(async move { registry::install(&entry).await });
        cx.spawn(async move |this, cx| {
            let result = match task.await {
                Ok(r) => r.map_err(|e| format!("{e:#}")),
                Err(e) => Err(e.to_string()),
            };
            this.update(cx, |this, cx| {
                match result {
                    Ok(_) => {
                        this.reload_registry_backends();
                        this.notice(format!("{name} を追加しました。新しいスレッドで選べます。"));
                    }
                    Err(e) => this.fail(e),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    pub(super) fn remove_registry(&mut self, id: String, cx: &mut Context<Self>) {
        match registry::uninstall(&id) {
            Ok(()) => {
                self.reload_registry_backends();
                self.notice(format!("{id} を削除しました。"));
            }
            Err(e) => self.fail(e),
        }
        cx.notify();
    }

    pub(super) fn reload_registry_backends(&mut self) {
        match greencode_core::config::Config::load() {
            Ok(cfg) => {
                Arc::make_mut(&mut self.cfg).backends = cfg.backends;
                self.push_config();
            }
            Err(e) => self.fail(format!("バックエンドの再読み込みに失敗しました: {e:#}")),
        }
    }
}
