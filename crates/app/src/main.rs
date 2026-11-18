mod thread_view;
mod workspace;

use gpui_kit::*;
use greencode_core::config::Config;

actions!(greencode, [Quit]);

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
    gpui_kit::application().with_assets(gpui_kit::assets::Assets).run(move |cx| {
        gpui_kit::init(cx);
        cx.bind_keys([KeyBinding::new("cmd-q", Quit, None)]);
        cx.on_action(|_: &Quit, cx| cx.quit());
        let options = WindowOptions {
            titlebar: Some(TitlebarOptions {
                title: Some("GreenCode".into()),
                appears_transparent: true,
                traffic_light_position: Some(point(px(14.), px(14.))),
            }),
            window_background: WindowBackgroundAppearance::Opaque,
            window_bounds: Some(WindowBounds::Windowed(Bounds::centered(None, size(px(1180.), px(760.)), cx))),
            ..Default::default()
        };
        gpui_kit::open_window(options, cx, |window, cx| cx.new(|cx| workspace::Workspace::new(cfg, cfg_error, window, cx)))
            .expect("failed to open window");
        cx.activate(true);
    });
}

// The test macro expands to `gpui::` paths.
#[cfg(test)]
extern crate gpui_kit as gpui;
