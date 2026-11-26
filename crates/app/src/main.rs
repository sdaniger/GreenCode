mod thread_view;
mod workspace;

use gpui_kit::*;
use greencode_core::config::Config;

actions!(greencode, [Quit]);

/// gpui-kit's icons plus the harness logos shown in the model picker.
struct Assets;

impl AssetSource for Assets {
    fn load(&self, path: &str) -> Result<Option<std::borrow::Cow<'static, [u8]>>> {
        let bytes: &'static [u8] = match path {
            "harness/native.svg" => include_bytes!("../../../packaging/app-icon.svg"),
            "harness/antigravity.png" => include_bytes!("../assets/harness/antigravity.png"),
            "harness/claude.svg" => include_bytes!("../assets/harness/claude.svg"),
            "harness/codex.svg" => include_bytes!("../assets/harness/codex.svg"),
            "harness/opencode.svg" => include_bytes!("../assets/harness/opencode.svg"),
            "harness/pi.svg" => include_bytes!("../assets/harness/pi.svg"),
            "harness/kiro.svg" => include_bytes!("../assets/harness/kiro.svg"),
            "harness/grok.svg" => include_bytes!("../assets/harness/grok.svg"),
            "harness/cursor.svg" => include_bytes!("../assets/harness/cursor.svg"),
            "harness/devin.svg" => include_bytes!("../assets/harness/devin.svg"),
            _ => return gpui_kit::assets::Assets.load(path),
        };
        Ok(Some(std::borrow::Cow::Borrowed(bytes)))
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        gpui_kit::assets::Assets.list(path)
    }
}

fn main() {
    let (cfg, cfg_error) = match Config::load() {
        Ok(c) => (c, None),
        // A broken config.toml must not start in YOLO mode or drop the managed policy.
        Err(e) => {
            let mut c = Config { mode: greencode_core::permission::Mode::Ask, policy: greencode_core::policy::load().ok().flatten(), ..Config::default() };
            if let Some(s) = c.policy.as_ref().and_then(|p| p.settings.sandbox.as_ref()) { c.sandbox = s.value; }
            (c, Some(format!("{e:#}")))
        }
    };
    greencode_core::tools::SANDBOX.store(cfg.sandbox, std::sync::atomic::Ordering::Relaxed);
    // Links arrive from the OS (URL scheme) or as the first argument; the window opens them as drafts.
    let (links, mut inbox) = tokio::sync::mpsc::unbounded_channel::<String>();
    for link in std::env::args().skip(1).filter(|a| a.starts_with("greencode://")) {
        let _ = links.send(link);
    }
    let app = gpui_kit::application();
    app.on_open_urls(move |urls| urls.into_iter().for_each(|u| drop(links.send(u))));
    app.with_assets(Assets).run(move |cx| {
        gpui_kit::init(cx);
        // ⌘Q goes through Workspace::on_key so `quit_mode` can require a hold or two presses.
        // The menu's Quit action, if a menu is installed, still quits directly.
        cx.on_action(|_: &Quit, cx| cx.quit());
        let options = |cx: &mut App| WindowOptions {
            titlebar: Some(TitlebarOptions {
                title: Some("GreenCode".into()),
                appears_transparent: true,
                traffic_light_position: Some(point(px(14.), px(14.))),
            }),
            window_background: WindowBackgroundAppearance::Opaque,
            window_bounds: Some(WindowBounds::Windowed(Bounds::centered(None, size(px(1180.), px(760.)), cx))),
            ..Default::default()
        };
        let (links_cfg, links_error) = (cfg.clone(), cfg_error.clone());
        let (mut window, mut workspace) = gpui_kit::open_window(options(cx), cx, |window, cx| cx.new(|cx| workspace::Workspace::new(cfg, cfg_error, window, cx)))
            .expect("failed to open window");
        cx.spawn(async move |cx| {
            while let Some(link) = inbox.recv().await {
                // A link can arrive after the window was closed; reopen it rather than dropping the link.
                if !open_link(&link, window, &workspace, cx) {
                    let (w, ws) = cx.update(|cx| {
                        gpui_kit::open_window(options(cx), cx, |window, cx| {
                            cx.new(|cx| workspace::Workspace::new(links_cfg.clone(), links_error.clone(), window, cx))
                        })
                        .expect("failed to open window")
                    });
                    (window, workspace) = (w, ws);
                    open_link(&link, window, &workspace, cx);
                }
                let _ = cx.update(|cx| cx.activate(true));
            }
        })
        .detach();
        cx.activate(true);
    });
}

/// Opens a link in an open window; false when that window has closed.
fn open_link(link: &str, window: AnyWindowHandle, workspace: &Entity<workspace::Workspace>, cx: &mut AsyncApp) -> bool {
    window.update(cx, |_, window, cx| workspace.update(cx, |ws, cx| ws.open_link(link, window, cx))).is_ok()
}

// The test macro expands to `gpui::` paths.
#[cfg(test)]
extern crate gpui_kit as gpui;
