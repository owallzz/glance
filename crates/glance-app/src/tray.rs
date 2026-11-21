//! System tray icon (StatusNotifierItem via ksni on Linux, tray-icon on
//! macOS/Windows): Open, status, Quit. While the tray is up, closing the
//! window minimizes it instead, so calls continue and contacts can ring you.

use std::sync::atomic::{AtomicBool, Ordering};

use glance_core::Presence;

/// True while a tray icon is showing (the title bar's close button checks it).
pub static TRAY_ACTIVE: AtomicBool = AtomicBool::new(false);

pub fn active() -> bool {
    TRAY_ACTIVE.load(Ordering::Relaxed)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrayEvent {
    Open,
    SetPresence(Presence),
    Quit,
}

/// The app icon, drawn in code: a blurple rounded square with a white
/// monitor. Returns RGBA, `size`×`size`.
pub fn icon_rgba(size: u32) -> Vec<u8> {
    let s = size as f32;
    let mut out = vec![0u8; (size * size * 4) as usize];
    let radius = s * 0.28;
    for y in 0..size {
        for x in 0..size {
            let (fx, fy) = (x as f32 + 0.5, y as f32 + 0.5);
            // Rounded-square coverage.
            let dx = (fx - s / 2.0).abs() - (s / 2.0 - radius);
            let dy = (fy - s / 2.0).abs() - (s / 2.0 - radius);
            let outside = (dx.max(0.0).powi(2) + dy.max(0.0).powi(2)).sqrt() - radius;
            if outside > 0.5 {
                continue;
            }
            let alpha = (0.5 - outside).clamp(0.0, 1.0);
            // Monitor: outlined screen plus a stand.
            let (u, v) = (fx / s, fy / s);
            let screen_outer = (0.22..=0.78).contains(&u) && (0.26..=0.64).contains(&v);
            let screen_inner = (0.29..=0.71).contains(&u) && (0.33..=0.57).contains(&v);
            let stand = ((0.46..=0.54).contains(&u) && (0.64..=0.72).contains(&v))
                || ((0.36..=0.64).contains(&u) && (0.72..=0.77).contains(&v));
            let white = (screen_outer && !screen_inner) || stand;
            let (r, g, b) = if white {
                (255, 255, 255)
            } else {
                (0x58, 0x65, 0xf2)
            };
            let i = ((y * size + x) * 4) as usize;
            out[i..i + 4].copy_from_slice(&[r, g, b, (alpha * 255.0) as u8]);
        }
    }
    out
}

/// The app icon as a PNG file.
pub fn icon_png(size: u32) -> Vec<u8> {
    let mut out = Vec::new();
    let mut enc = png::Encoder::new(&mut out, size, size);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    let written = enc
        .write_header()
        .and_then(|mut w| w.write_image_data(&icon_rgba(size)));
    if written.is_err() {
        out.clear();
    }
    out
}

/// The app icon as a Windows .ico (PNG-compressed entries, 16 to 256 px).
pub fn icon_ico() -> Vec<u8> {
    const SIZES: [u32; 7] = [16, 24, 32, 48, 64, 128, 256];
    let images: Vec<Vec<u8>> = SIZES.iter().map(|&s| icon_png(s)).collect();
    let mut out = vec![0, 0, 1, 0];
    out.extend_from_slice(&(SIZES.len() as u16).to_le_bytes());
    let mut offset = 6 + 16 * SIZES.len();
    for (&size, png) in SIZES.iter().zip(&images) {
        // 0 means 256 in the directory entry.
        let dim = if size >= 256 { 0 } else { size as u8 };
        out.extend_from_slice(&[dim, dim, 0, 0]);
        out.extend_from_slice(&1u16.to_le_bytes()); // colour planes
        out.extend_from_slice(&32u16.to_le_bytes()); // bits per pixel
        out.extend_from_slice(&(png.len() as u32).to_le_bytes());
        out.extend_from_slice(&(offset as u32).to_le_bytes());
        offset += png.len();
    }
    for png in images {
        out.extend(png);
    }
    out
}

/// `glance --export-icons <dir>`: icon_<size>.png and glance.ico for
/// packaging.
pub fn export_icons(dir: &std::path::Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    for size in [16, 32, 48, 64, 128, 256, 512, 1024] {
        std::fs::write(dir.join(format!("icon_{size}.png")), icon_png(size))?;
    }
    std::fs::write(dir.join("glance.ico"), icon_ico())?;
    Ok(())
}

/// A running tray icon; dropping it removes the icon.
pub struct Tray {
    #[cfg(target_os = "linux")]
    _handle: ksni::Handle<linux::GlanceTray>,
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    _icon: tray_icon::TrayIcon,
}

impl Drop for Tray {
    fn drop(&mut self) {
        TRAY_ACTIVE.store(false, Ordering::Relaxed);
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use super::TrayEvent;
    use glance_core::Presence;

    #[derive(Debug)]
    pub struct GlanceTray {
        pub tx: flume::Sender<TrayEvent>,
    }

    impl ksni::Tray for GlanceTray {
        fn id(&self) -> String {
            "glance".into()
        }
        fn title(&self) -> String {
            "glance".into()
        }
        fn icon_pixmap(&self) -> Vec<ksni::Icon> {
            [22u32, 32, 48]
                .into_iter()
                .map(|size| {
                    let mut data = super::icon_rgba(size);
                    // RGBA -> ARGB (network byte order).
                    for px in data.as_chunks_mut::<4>().0 {
                        px.rotate_right(1);
                    }
                    ksni::Icon {
                        width: size as i32,
                        height: size as i32,
                        data,
                    }
                })
                .collect()
        }
        // If the tray host goes away, closing the window should quit again.
        fn watcher_online(&self) {
            super::TRAY_ACTIVE.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        fn watcher_offline(&self, _reason: ksni::OfflineReason) -> bool {
            super::TRAY_ACTIVE.store(false, std::sync::atomic::Ordering::Relaxed);
            true
        }
        fn activate(&mut self, _x: i32, _y: i32) {
            let _ = self.tx.send(TrayEvent::Open);
        }
        fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
            use ksni::menu::*;
            let item = |label: &str, ev: TrayEvent| -> ksni::MenuItem<Self> {
                StandardItem {
                    label: label.into(),
                    activate: Box::new(move |this: &mut Self| {
                        let _ = this.tx.send(ev);
                    }),
                    ..Default::default()
                }
                .into()
            };
            vec![
                item("Open glance", TrayEvent::Open),
                MenuItem::Separator,
                item("Online", TrayEvent::SetPresence(Presence::Online)),
                item("Idle", TrayEvent::SetPresence(Presence::Idle)),
                item(
                    "Do Not Disturb",
                    TrayEvent::SetPresence(Presence::DoNotDisturb),
                ),
                item("Invisible", TrayEvent::SetPresence(Presence::Invisible)),
                MenuItem::Separator,
                item("Quit glance", TrayEvent::Quit),
            ]
        }
    }
}

/// Show the tray icon. On Linux this needs a StatusNotifierItem host
/// (KDE, most GNOME setups with the AppIndicator extension); returns `None`
/// when there's no tray to show in.
#[cfg(target_os = "linux")]
pub fn start(net: &crate::net::Net, tx: flume::Sender<TrayEvent>) -> Option<Tray> {
    use ksni::TrayMethods;
    // Flatpak doesn't let apps own the spec's StatusNotifierItem-PID-N
    // name; hosts accept the unique bus name instead.
    let sandboxed = crate::net::in_flatpak();
    let handle = net
        .block_on(async move {
            linux::GlanceTray { tx }
                .disable_dbus_name(sandboxed)
                .spawn()
                .await
        })
        .ok()?;
    TRAY_ACTIVE.store(true, Ordering::Relaxed);
    Some(Tray { _handle: handle })
}

#[cfg(any(target_os = "macos", target_os = "windows"))]
pub fn start(_net: &crate::net::Net, tx: flume::Sender<TrayEvent>) -> Option<Tray> {
    use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
    let menu = Menu::new();
    let open = MenuItem::new("Open glance", true, None);
    let online = MenuItem::new("Online", true, None);
    let idle = MenuItem::new("Idle", true, None);
    let dnd = MenuItem::new("Do Not Disturb", true, None);
    let invisible = MenuItem::new("Invisible", true, None);
    let quit = MenuItem::new("Quit glance", true, None);
    menu.append_items(&[
        &open,
        &PredefinedMenuItem::separator(),
        &online,
        &idle,
        &dnd,
        &invisible,
        &PredefinedMenuItem::separator(),
        &quit,
    ])
    .ok()?;
    let ids = [
        (open.id().clone(), TrayEvent::Open),
        (
            online.id().clone(),
            TrayEvent::SetPresence(Presence::Online),
        ),
        (idle.id().clone(), TrayEvent::SetPresence(Presence::Idle)),
        (
            dnd.id().clone(),
            TrayEvent::SetPresence(Presence::DoNotDisturb),
        ),
        (
            invisible.id().clone(),
            TrayEvent::SetPresence(Presence::Invisible),
        ),
        (quit.id().clone(), TrayEvent::Quit),
    ];
    let icon = tray_icon::Icon::from_rgba(icon_rgba(32), 32, 32).ok()?;
    let tray = tray_icon::TrayIconBuilder::new()
        .with_icon(icon)
        .with_tooltip("glance")
        .with_menu(Box::new(menu))
        .build()
        .ok()?;
    // Menu clicks arrive on a global channel; forward them.
    std::thread::spawn(move || {
        while let Ok(ev) = MenuEvent::receiver().recv() {
            if let Some((_, e)) = ids.iter().find(|(id, _)| *id == ev.id)
                && tx.send(*e).is_err()
            {
                break;
            }
        }
    });
    TRAY_ACTIVE.store(true, Ordering::Relaxed);
    Some(Tray { _icon: tray })
}

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
pub fn start(_net: &crate::net::Net, _tx: flume::Sender<TrayEvent>) -> Option<Tray> {
    None
}

/// Whether "start on login" can work for this copy: not for portable
/// copies, dev builds or test identities.
pub fn autostart_available() -> bool {
    crate::net::may_integrate()
}

/// Register or unregister "start on login". Only release builds touch the
/// system, and never for test identities (`GLANCE_DATA_DIR`) or portable
/// copies. Inside Flatpak it asks the Background portal instead.
pub fn sync_autostart(enabled: bool) {
    if !autostart_available() {
        return;
    }
    #[cfg(target_os = "linux")]
    if crate::net::in_flatpak() {
        portal::request_background(enabled);
        return;
    }
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let Some(path) = exe.to_str() else { return };
    let Ok(launcher) = auto_launch::AutoLaunchBuilder::new()
        .set_app_name("glance")
        .set_app_path(path)
        .set_args(&["--hidden"])
        .build()
    else {
        return;
    };
    let result = match (enabled, launcher.is_enabled().unwrap_or(false)) {
        (true, false) => launcher.enable(),
        (false, true) => launcher.disable(),
        _ => Ok(()),
    };
    if let Err(e) = result {
        tracing::warn!("couldn't update start-on-login: {e}");
    }
}

/// The XDG Background portal: a sandboxed app can't write its own
/// autostart entry, so it asks the desktop to (the user may be prompted).
#[cfg(target_os = "linux")]
mod portal {
    use std::collections::HashMap;
    use std::time::Duration;

    use zbus::zvariant::Value;

    pub fn request_background(autostart: bool) {
        let spawned = std::thread::Builder::new()
            .name("background-portal".into())
            .spawn(move || {
                let Ok(rt) = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                else {
                    return;
                };
                if let Err(e) = rt.block_on(request(autostart)) {
                    tracing::warn!("couldn't update start-on-login through the portal: {e}");
                }
            });
        if let Err(e) = spawned {
            tracing::warn!("couldn't start the portal thread: {e}");
        }
    }

    async fn request(autostart: bool) -> zbus::Result<()> {
        use futures_util::StreamExt;
        let conn = zbus::Connection::session().await?;
        // Listen for the answer before asking, on the path the portal will
        // use for this token.
        let token = format!("glance_autostart_{}", std::process::id());
        let sender = conn
            .unique_name()
            .map(|n| n.as_str().trim_start_matches(':').replace('.', "_"))
            .unwrap_or_default();
        let path = format!("/org/freedesktop/portal/desktop/request/{sender}/{token}");
        let rule = zbus::MatchRule::builder()
            .msg_type(zbus::message::Type::Signal)
            .interface("org.freedesktop.portal.Request")?
            .member("Response")?
            .path(path.as_str())?
            .build();
        let mut answers = zbus::MessageStream::for_match_rule(rule, &conn, Some(1)).await?;
        let mut options: HashMap<&str, Value> = HashMap::new();
        options.insert("handle_token", Value::from(token.as_str()));
        options.insert(
            "reason",
            Value::from("Start glance when you sign in, so contacts can ring you."),
        );
        options.insert("autostart", Value::from(autostart));
        options.insert("commandline", Value::from(vec!["glance", "--hidden"]));
        conn.call_method(
            Some("org.freedesktop.portal.Desktop"),
            "/org/freedesktop/portal/desktop",
            Some("org.freedesktop.portal.Background"),
            "RequestBackground",
            &("", options),
        )
        .await?;
        // The portal drops requests whose caller has gone, so stay connected
        // until the user answers (or gives up).
        let _ = tokio::time::timeout(Duration::from_secs(300), answers.next()).await;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::icon_rgba;

    #[test]
    fn ico_matches_the_packaged_one() {
        let ico = super::icon_ico();
        assert_eq!(&ico[..4], &[0, 0, 1, 0]);
        assert_eq!(u16::from_le_bytes([ico[4], ico[5]]), 7);
        // The Windows build embeds packaging/windows/glance.ico; regenerate it
        // with `glance --export-icons <dir>` when the icon changes.
        let packaged = include_bytes!("../../../packaging/windows/glance.ico");
        assert!(
            ico == packaged,
            "packaging/windows/glance.ico is stale: run `glance --export-icons` and copy glance.ico"
        );
    }

    #[test]
    fn icon_png_is_a_png() {
        let png = super::icon_png(64);
        assert_eq!(&png[1..4], b"PNG");
        let dir = std::env::temp_dir().join(format!("glance-icons-{}", std::process::id()));
        super::export_icons(&dir).unwrap();
        assert!(dir.join("icon_1024.png").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn icon_has_transparent_corners_and_white_glyph() {
        let px = |d: &[u8], x: u32, y: u32| {
            let i = ((y * 32 + x) * 4) as usize;
            [d[i], d[i + 1], d[i + 2], d[i + 3]]
        };
        let d = icon_rgba(32);
        assert_eq!(px(&d, 0, 0)[3], 0, "corner should be transparent");
        assert_eq!(
            px(&d, 16, 4),
            [0x58, 0x65, 0xf2, 255],
            "background is blurple"
        );
        assert_eq!(
            px(&d, 8, 9)[..3],
            [255, 255, 255],
            "screen outline is white"
        );
    }
}
