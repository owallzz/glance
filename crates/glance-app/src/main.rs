//! glance: peer-to-peer screen sharing rooms.

mod home;
mod spike;
mod theme;
mod video;

use gpui_kit::component::theme::ThemeMode;
use gpui_kit::*;

actions!(glance, [Quit]);

fn main() {
    gpui_kit::application()
        .with_assets(gpui_kit::assets::AllAssets)
        .run(|cx: &mut App| {
            gpui_kit::init(cx);
            theme::init(ThemeMode::Dark, cx);

            cx.on_action(|_: &Quit, cx| cx.quit());
            cx.bind_keys([KeyBinding::new("ctrl-q", Quit, None)]);

            let bounds = Bounds::centered(None, size(px(1200.), px(760.)), cx);
            let options = WindowOptions {
                titlebar: Some(TitlebarOptions {
                    title: Some("glance".into()),
                    ..Default::default()
                }),
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                window_min_size: Some(size(px(720.), px(480.))),
                app_id: Some("glance".into()),
                ..Default::default()
            };
            let args: Vec<String> = std::env::args().skip(1).collect();
            if args.first().map(String::as_str) == Some("--spike") {
                let num = |i: usize, d: u32| args.get(i).and_then(|v| v.parse().ok()).unwrap_or(d);
                let (n, w, h, fps) = (num(1, 1), num(2, 1920), num(3, 1080), num(4, 60));
                gpui_kit::open_window(options, cx, |_, cx| {
                    cx.new(|cx| spike::Spike::new(n as usize, w, h, fps, cx))
                })
                .expect("failed to open window");
            } else {
                gpui_kit::open_window(options, cx, |_, cx| cx.new(|_| home::Home))
                    .expect("failed to open window");
            }
            cx.activate(true);
        });
}
