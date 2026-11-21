//! The updater in the app: a timer that checks at startup and every 6 hours,
//! the "Update ready" pill in the title bar, the tray item, and the status
//! part of Settings → Updates. The work itself is in `update/`.

use std::sync::Arc;
use std::time::Duration;

use gpui_kit::assets::IconName;
use gpui_kit::component::Disableable;
use gpui_kit::component::Sizable;
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::app::GlanceApp;
use crate::theme::Palette;
use crate::ui::icon;
use crate::update::{Config, Status, apply};

/// Let startup (network, contacts) settle before the first check.
const STARTUP_DELAY: Duration = Duration::from_secs(20);
const CHECK_EVERY: Duration = Duration::from_secs(6 * 60 * 60);
/// First retry after a failed check (offline, GitHub down); doubles up to
/// `CHECK_EVERY`.
const RETRY_MIN: Duration = Duration::from_secs(15 * 60);

#[derive(Default)]
pub struct UpdateState {
    pub cfg: Option<Arc<Config>>,
    pub status: Status,
    _timer: Option<Task<()>>,
    _check: Option<Task<bool>>,
}

impl GlanceApp {
    /// Set up the updater and its timer (not in the demo, or without net).
    pub fn start_updater(&mut self, cx: &mut Context<Self>) {
        let Some(net) = &self.net else { return };
        let cfg = Arc::new(Config::from_env(&net.data_dir));
        let cleanup = cfg.clone();
        std::thread::Builder::new()
            .name("update-cleanup".into())
            .spawn(move || apply::cleanup(&cleanup.kind, &cleanup.stage_dir, &cleanup.current))
            .ok();
        self.update.status = cfg
            .unavailable
            .clone()
            .map_or(Status::Idle, Status::Unavailable);
        self.update.cfg = Some(cfg.clone());
        if cfg.unavailable.is_some() {
            return;
        }
        self.update._timer = Some(cx.spawn(async move |this, cx| {
            let (mut wait, mut retry) = (STARTUP_DELAY, RETRY_MIN);
            loop {
                cx.background_executor().timer(wait).await;
                let Ok(check) = this.update(cx, |app, cx| {
                    let ready = matches!(app.update.status, Status::Ready(_));
                    (app.settings.auto_update && !ready)
                        .then(|| app.check_for_updates(cx))
                        .flatten()
                }) else {
                    break;
                };
                let ok = match check {
                    Some(task) => task.await,
                    None => true,
                };
                wait = if ok {
                    retry = RETRY_MIN;
                    CHECK_EVERY
                } else {
                    let w = retry;
                    retry = (retry * 2).min(CHECK_EVERY);
                    w
                };
            }
        }));
    }

    /// Check (and download) now. The task resolves to false on errors and
    /// on releases still being published (the timer then retries soon).
    /// `None` when a check is already running or updates are unavailable.
    pub fn check_for_updates(&mut self, cx: &mut Context<Self>) -> Option<Task<bool>> {
        let net = self.net.clone()?;
        let cfg = self.update.cfg.clone()?;
        if cfg.unavailable.is_some() || self.update.status.busy() {
            return None;
        }
        self.update.status = Status::Checking;
        cx.notify();
        let (tx, rx) = flume::unbounded::<(bool, Status)>();
        net.spawn(async move {
            let progress = tx.clone();
            let status = crate::update::check_and_stage(&cfg, move |s| {
                let _ = progress.send((false, s));
            })
            .await
            .unwrap_or_else(|e| Status::Error(format!("{e:#}")));
            let _ = tx.send((true, status));
        });
        Some(cx.spawn(async move |this, cx| {
            while let Ok((done, status)) = rx.recv_async().await {
                let ok = !status.retry_soon();
                if this
                    .update(cx, |app, cx| app.set_update_status(status, cx))
                    .is_err()
                {
                    return false;
                }
                if done {
                    return ok;
                }
            }
            false
        }))
    }

    fn set_update_status(&mut self, status: Status, cx: &mut Context<Self>) {
        if let Status::Error(e) = &status {
            tracing::warn!("update check failed: {e}");
        }
        // Only touch the tray when its item changes, not on every download
        // percent (each update re-sends the menu over D-Bus on Linux).
        let label = status.tray_label();
        if label != self.update.status.tray_label()
            && let Some(tray) = &self._tray
        {
            tray.set_update_ready(label);
        }
        self.update.status = status;
        cx.notify();
    }

    /// Install the staged update, start the new version and quit. Never
    /// during a call. The copy (or the macOS `tar`) runs off the UI thread.
    pub fn restart_to_update(&mut self, cx: &mut Context<Self>) {
        if self.room.is_some() {
            return;
        }
        let Status::Ready(staged) = self.update.status.clone() else {
            return;
        };
        self.set_update_status(Status::Installing(staged.version.clone()), cx);
        let install = cx.background_spawn(async move {
            apply::apply(&staged).and_then(|next| match next {
                apply::Next::Launch(exe) => apply::relaunch(&exe, &[apply::AFTER_UPDATE]),
                apply::Next::InstallerRelaunches => Ok(()),
            })
        });
        cx.spawn(async move |this, cx| {
            let result = install.await;
            let _ = this.update(cx, |app, cx| match result {
                Ok(()) => cx.quit(),
                Err(e) => app.set_update_status(Status::Error(format!("{e:#}")), cx),
            });
        })
        .detach();
    }

    /// "Update ready — vX.Y.Z · Restart" in the title bar.
    pub fn render_update_pill(
        &mut self,
        p: &Palette,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if let Status::Available(v, why) = &self.update.status
            && why == crate::update::FLATPAK_HINT
        {
            // A Flatpak bundle can't update itself: point at the release.
            return Some(
                div()
                    .id("update-pill")
                    .occlude()
                    .on_mouse_down(MouseButton::Left, |_, window, _| window.prevent_default())
                    .ml_3()
                    .px_2()
                    .py(px(1.))
                    .flex()
                    .items_center()
                    .gap_1()
                    .rounded_full()
                    .bg(p.green)
                    .text_xs()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(gpui_kit::white())
                    .cursor_pointer()
                    .child(icon(IconName::Download, 12., gpui_kit::white()))
                    .child(format!("v{v} is out · Download"))
                    .tooltip(|window, cx| {
                        Tooltip::new("Opens the release page for the new .flatpak")
                            .build(window, cx)
                    })
                    .on_click(|_, _, cx| cx.open_url(crate::update::RELEASES_URL))
                    .into_any_element(),
            );
        }
        let Status::Ready(staged) = &self.update.status else {
            return None;
        };
        let in_call = self.room.is_some();
        let label = format!("Update ready · v{}", staged.version);
        // The pill sits in the title bar's drag area. `occlude` keeps the
        // Windows caption hit test from answering "drag" over the button, and
        // preventing the default stops the title bar from turning the press
        // into a window move on Linux.
        let restart = div()
            .id("update-restart")
            .occlude()
            .on_mouse_down(MouseButton::Left, |_, window, _| window.prevent_default())
            .px_2()
            .rounded_full()
            .bg(gpui_kit::white().opacity(0.2))
            .child("Restart");
        let restart = if in_call {
            restart
                .opacity(0.5)
                .tooltip(|window, cx| Tooltip::new("Finish your call first").build(window, cx))
        } else {
            restart
                .cursor_pointer()
                .hover(|s| s.bg(gpui_kit::white().opacity(0.3)))
                .on_click(cx.listener(|this, _, _, cx| this.restart_to_update(cx)))
        };
        Some(
            div()
                .id("update-pill")
                .ml_3()
                .pl_2()
                .pr(px(2.))
                .py(px(1.))
                .flex()
                .items_center()
                .gap_1()
                .rounded_full()
                .bg(p.green)
                .text_xs()
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(gpui_kit::white())
                .child(icon(IconName::Download, 12., gpui_kit::white()))
                .child(label)
                .child(restart)
                .into_any_element(),
        )
    }

    /// Settings → Updates without the toggle: status, Check now / Restart.
    pub fn render_update_status(&mut self, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let status = self.update.status.clone();
        let in_call = self.room.is_some();
        let unavailable = matches!(status, Status::Unavailable(_));
        let action = match &status {
            Status::Unavailable(_) => None,
            Status::Available(_, why) if why == crate::update::FLATPAK_HINT => Some(
                Button::new("update-download")
                    .primary()
                    .small()
                    .label("Download")
                    .on_click(|_, _, cx| cx.open_url(crate::update::RELEASES_URL))
                    .into_any_element(),
            ),
            Status::Ready(_) => Some(
                Button::new("update-restart-now")
                    .primary()
                    .small()
                    .label("Restart to update")
                    .disabled(in_call)
                    .when(in_call, |b| b.tooltip("Finish your call first"))
                    .on_click(cx.listener(|this, _, _, cx| this.restart_to_update(cx)))
                    .into_any_element(),
            ),
            s => Some(
                Button::new("update-check-now")
                    .small()
                    .label("Check now")
                    .disabled(s.busy())
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.update._check = this.check_for_updates(cx);
                    }))
                    .into_any_element(),
            ),
        };
        let color = match status {
            Status::Error(_) => p.red,
            Status::Ready(_) => p.green,
            _ => p.text_normal,
        };
        div()
            .py_4()
            .flex()
            .items_center()
            .gap_4()
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(
                        div()
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(p.text_header)
                            .child(if unavailable { "Updates" } else { "Status" }),
                    )
                    .child(div().text_sm().text_color(color).child(status.text()))
                    .when(in_call && matches!(status, Status::Ready(_)), |d| {
                        d.child(
                            div()
                                .text_xs()
                                .text_color(p.text_muted)
                                .child("Finish your call first, then restart."),
                        )
                    }),
            )
            .children(action)
            .into_any_element()
    }

    /// "0.2.0 · AppImage" for settings.
    pub fn version_label(&self) -> String {
        match &self.update.cfg {
            Some(cfg) => format!("{} · {}", cfg.current, cfg.kind.label()),
            None => env!("CARGO_PKG_VERSION").to_string(),
        }
    }

    /// Whether this install downloads updates (shows the auto toggle).
    pub fn updates_available(&self) -> bool {
        self.update
            .cfg
            .as_ref()
            .is_some_and(|c| c.unavailable.is_none())
    }
}
