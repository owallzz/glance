//! glance: peer-to-peer screen sharing rooms.

mod app;
mod bot;
mod camera;
mod contacts;
mod files;
mod home;
mod instance;
mod net;
mod overlays;
mod popout;
mod room;
mod scheme;
mod session;
mod spike;
mod streaming;
mod theme;
mod tray;
mod ui;
mod video;
mod voice;

use gpui_kit::component::theme::ThemeMode;
use gpui_kit::*;

actions!(glance, [Quit]);

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn".into()),
        )
        .init();
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("--export-icons") {
        let dir = args
            .get(1)
            .map_or_else(|| "icons".into(), std::path::PathBuf::from);
        if let Err(e) = tray::export_icons(&dir) {
            eprintln!("glance: exporting icons failed: {e}");
            std::process::exit(1);
        }
        return;
    }
    if args.first().map(String::as_str) == Some("bot") {
        let Some(link) = args.get(1) else {
            eprintln!(
                "usage: glance bot <link> [name] [--stream WxH@FPS] [--tone] [--file PATH] [--ring-back] [--camera]"
            );
            std::process::exit(2);
        };
        let name = args
            .get(2)
            .filter(|a| !a.starts_with("--"))
            .map(String::as_str)
            .unwrap_or("Bot");
        // --stream [WxH@FPS], default 1280x720@30
        let stream = args.iter().position(|a| a == "--stream").map(|i| {
            args.get(i + 1)
                .and_then(|spec| {
                    let (size, fps) = spec.split_once('@')?;
                    let (w, h) = size.split_once('x')?;
                    Some((w.parse().ok()?, h.parse().ok()?, fps.parse().ok()?))
                })
                .unwrap_or((1280, 720, 30))
        });
        let tone = args.iter().any(|a| a == "--tone");
        let file = args
            .iter()
            .position(|a| a == "--file")
            .and_then(|i| args.get(i + 1))
            .map(std::path::PathBuf::from);
        let ring_back = args.iter().any(|a| a == "--ring-back");
        let camera = args.iter().any(|a| a == "--camera");
        if let Err(e) = bot::run(link, name, stream, tone, file, ring_back, camera) {
            eprintln!("glance bot: {e:#}");
            std::process::exit(1);
        }
        return;
    }

    // A second launch (e.g. a clicked glance:// link) hands off to the
    // running app instead of starting another copy of the same identity.
    let normal_launch = !args.iter().any(|a| a == "--spike" || a == "--demo");
    let (instance_tx, instance_rx) = flume::unbounded::<instance::Request>();
    if normal_launch {
        let dir = net::data_dir();
        if instance::hand_off(&dir, &instance::Request::from_args(&args)) {
            return;
        }
        if let Err(e) = instance::listen(&dir, instance_tx.clone()) {
            tracing::warn!("single-instance listener failed: {e}");
        }
        scheme::register();
    }

    let app = gpui_kit::application().with_assets(gpui_kit::assets::AllAssets);
    // macOS delivers clicked links as open-URL events, not arguments.
    let url_tx = instance_tx.clone();
    app.on_open_urls(move |urls| {
        for url in urls {
            if url.starts_with("glance://") {
                let _ = url_tx.send(instance::Request::Join(url));
            }
        }
    });
    app.run(move |cx: &mut App| {
        gpui_kit::init(cx);
        theme::init(ThemeMode::Dark, cx);

        cx.on_action(|_: &Quit, cx| cx.quit());

        cx.bind_keys([KeyBinding::new(app::SHORTCUTS[3], Quit, None)]);
        app::bind_keys(cx);

        let bounds = Bounds::centered(None, size(px(1200.), px(760.)), cx);
        // We draw our own Discord-style title bar (drag area, minimize,
        // maximize, close). Wayland compositors like GNOME don't provide
        // one, and on Windows/macOS it replaces the system bar.
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            window_min_size: Some(size(px(720.), px(480.))),
            window_decorations: Some(WindowDecorations::Client),
            // The OS still shows this in the taskbar and Alt+Tab.
            titlebar: Some(TitlebarOptions {
                title: Some("glance".into()),
                ..gpui_kit::component::TitleBar::title_bar_options()
            }),
            app_id: Some("glance".into()),
            ..gpui_kit::component::TitleBar::window_options()
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
            let demo = args.iter().any(|a| a == "--demo");
            let net = if demo {
                None
            } else {
                match net::Net::start(net::data_dir()) {
                    Ok(net) => Some(std::rc::Rc::new(net)),
                    Err(e) => {
                        eprintln!("glance: networking failed to start: {e:#}");
                        None
                    }
                }
            };
            let join = args
                .iter()
                .position(|a| a == "join")
                .and_then(|i| args.get(i + 1))
                .and_then(|link| glance_proto::Ticket::parse(link).ok())
                .or_else(|| {
                    args.iter().find_map(|a| {
                        glance_proto::Ticket::parse(a)
                            .ok()
                            .filter(|_| a.starts_with("glance://"))
                    })
                });
            let hidden = args.iter().any(|a| a == "--hidden");
            gpui_kit::open_window(options, cx, |window, cx| {
                // Started at login: stay out of the way (tray only).
                if hidden {
                    window.minimize_window();
                }
                cx.new(|cx| {
                    let mut app = app::GlanceApp::new(demo, net, window, cx);
                    app.listen_for_handoffs(instance_rx.clone(), window, cx);
                    if let Some(ticket) = join
                        && !app.needs_profile
                    {
                        app.join(ticket, cx);
                    }
                    app
                })
            })
            .expect("failed to open window");
        }
        cx.activate(true);
    });
}
