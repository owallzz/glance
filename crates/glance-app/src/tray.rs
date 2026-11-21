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
    /// "Restart to update" (only listed while an update is staged).
    RestartToUpdate,
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

/// `glance --export-icons <dir>`: icon_<size>.png for packaging.
pub fn export_icons(dir: &std::path::Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    for size in [16, 32, 64, 128, 256, 512, 1024] {
        std::fs::write(dir.join(format!("icon_{size}.png")), icon_png(size))?;
    }
    Ok(())
}

/// A running tray icon; dropping it removes the icon.
pub struct Tray {
    #[cfg(target_os = "linux")]
    _handle: ksni::Handle<linux::GlanceTray>,
    #[cfg(target_os = "linux")]
    rt: tokio::runtime::Handle,
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    _icon: tray_icon::TrayIcon,
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    menu: tray_icon::menu::Menu,
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    update_item: tray_icon::menu::MenuItem,
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    update_shown: std::cell::Cell<bool>,
}

impl Tray {
    /// Show "Restart to update to vX" in the menu (`None` hides it).
    #[cfg(target_os = "linux")]
    pub fn set_update_ready(&self, label: Option<String>) {
        let handle = self._handle.clone();
        self.rt.spawn(async move {
            handle.update(move |t| t.update = label).await;
        });
    }

    #[cfg(any(target_os = "macos", target_os = "windows"))]
    pub fn set_update_ready(&self, label: Option<String>) {
        match label {
            Some(label) => {
                self.update_item.set_text(label);
                if !self.update_shown.get() && self.menu.insert(&self.update_item, 1).is_ok() {
                    self.update_shown.set(true);
                }
            }
            None => {
                if self.update_shown.get() && self.menu.remove(&self.update_item).is_ok() {
                    self.update_shown.set(false);
                }
            }
        }
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    pub fn set_update_ready(&self, _label: Option<String>) {}
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
        /// "Restart to update" label while an update is staged.
        pub update: Option<String>,
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
            let mut items = vec![item("Open glance", TrayEvent::Open)];
            if let Some(label) = &self.update {
                items.push(item(label, TrayEvent::RestartToUpdate));
            }
            items.extend([
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
            ]);
            items
        }
    }
}

/// Show the tray icon. On Linux this needs a StatusNotifierItem host
/// (KDE, most GNOME setups with the AppIndicator extension); returns `None`
/// when there's no tray to show in.
#[cfg(target_os = "linux")]
pub fn start(net: &crate::net::Net, tx: flume::Sender<TrayEvent>) -> Option<Tray> {
    use ksni::TrayMethods;
    let (handle, rt) = net
        .block_on(async move {
            let tray = linux::GlanceTray { tx, update: None }.spawn().await;
            tray.map(|t| (t, tokio::runtime::Handle::current()))
        })
        .ok()?;
    TRAY_ACTIVE.store(true, Ordering::Relaxed);
    Some(Tray {
        _handle: handle,
        rt,
    })
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
    // Inserted below "Open glance" once an update is staged.
    let update_item = MenuItem::new("Restart to update", true, None);
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
        (update_item.id().clone(), TrayEvent::RestartToUpdate),
        (quit.id().clone(), TrayEvent::Quit),
    ];
    let icon = tray_icon::Icon::from_rgba(icon_rgba(32), 32, 32).ok()?;
    let tray = tray_icon::TrayIconBuilder::new()
        .with_icon(icon)
        .with_tooltip("glance")
        .with_menu(Box::new(menu.clone()))
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
    Some(Tray {
        _icon: tray,
        menu,
        update_item,
        update_shown: std::cell::Cell::new(false),
    })
}

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
pub fn start(_net: &crate::net::Net, _tx: flume::Sender<TrayEvent>) -> Option<Tray> {
    None
}

/// Register or unregister "start on login". Only release builds touch the
/// system, and never for test identities (`GLANCE_DATA_DIR`).
pub fn sync_autostart(enabled: bool) {
    if cfg!(debug_assertions) || std::env::var_os("GLANCE_DATA_DIR").is_some() {
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

#[cfg(test)]
mod tests {
    use super::icon_rgba;

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
