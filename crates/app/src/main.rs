mod thread_view;
mod workspace;

use gpui_kit::*;
use greencode_core::config::Config;

actions!(greencode, [Quit]);

fn main() {
    let (cfg, cfg_error) = match Config::load() {
        Ok(c) => (c, None),
        Err(e) => (Config::default(), Some(format!("{e:#}"))),
    };
    gpui_kit::application().run(move |cx| {
        gpui_kit::init(cx);
        cx.bind_keys([KeyBinding::new("cmd-q", Quit, None)]);
        cx.on_action(|_: &Quit, cx| cx.quit());
        let options = WindowOptions {
            titlebar: Some(TitlebarOptions {
                title: Some("GreenCode".into()),
                appears_transparent: true,
                traffic_light_position: Some(point(px(14.), px(14.))),
            }),
            // The desktop shows through, blurred; panels tint it.
            window_background: WindowBackgroundAppearance::Blurred,
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
