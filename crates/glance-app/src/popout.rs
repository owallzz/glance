//! Fullscreen (immersive) viewing and popping a stream out into its own window.

use glance_core::PeerId;
use gpui_kit::*;

use crate::app::GlanceApp;
use crate::video::VideoView;

/// The root of a pop-out window: just the video, black around it.
pub struct PopoutView {
    pub video: Entity<VideoView>,
}

impl Render for PopoutView {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .bg(gpui_kit::black())
            .child(self.video.clone())
    }
}

impl GlanceApp {
    /// Fill the window (and go OS-fullscreen) with one stream.
    pub fn enter_fullscreen(&mut self, peer: PeerId, window: &mut Window, cx: &mut Context<Self>) {
        self.immersive = Some(peer);
        // Keyboard shortcuts (Esc) follow focus; make sure we have it.
        self.focus.focus(window, cx);
        if !window.is_fullscreen() {
            window.toggle_fullscreen();
        }
        cx.notify();
    }

    pub fn exit_fullscreen(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.immersive.take().is_some() && window.is_fullscreen() {
            window.toggle_fullscreen();
        }
        cx.notify();
    }

    /// Move a stream into its own window.
    pub fn pop_out(&mut self, peer: PeerId, cx: &mut Context<Self>) {
        let me = self.me.id;
        if peer == me || self.room.as_ref().is_none_or(|r| r.handle.is_none()) {
            return;
        }
        let title = self
            .profile_of(peer)
            .map(|p| format!("{}'s screen — glance", p.name))
            .unwrap_or_else(|| "Stream — glance".into());
        // Stop the in-window copy; the pop-out gets its own fresh stream.
        self.stop_watching(peer, cx);
        let app = cx.entity().downgrade();
        let options = WindowOptions {
            titlebar: Some(TitlebarOptions {
                title: Some(title.into()),
                ..Default::default()
            }),
            window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
                None,
                size(px(960.), px(560.)),
                cx,
            ))),
            app_id: Some("glance".into()),
            ..Default::default()
        };
        let mut video = None;
        let opened = gpui_kit::open_window(options, cx, |window, cx| {
            let v = cx.new(|_| VideoView::empty(glance_media::TargetSize::default()));
            video = Some(v.clone());
            // Closing the pop-out puts the stream back in the main window.
            let app = app.clone();
            window.on_window_should_close(cx, move |_, cx| {
                if let Some(app) = app.upgrade() {
                    app.update(cx, |app, cx| app.pop_in(peer, cx));
                }
                true
            });
            cx.new(|_| PopoutView { video: v })
        });
        let (Ok(_), Some(video)) = (opened, video) else {
            return;
        };
        if let Some(room) = self.room.as_mut() {
            room.popouts.insert(peer, video);
        }
        tracing::info!("popped out; requesting stream");
        self.request_stream(peer);
        cx.notify();
    }

    /// The pop-out closed: show the stream in the main window again.
    pub fn pop_in(&mut self, peer: PeerId, cx: &mut Context<Self>) {
        tracing::info!("pop in");
        let live = self
            .room
            .as_mut()
            .map(|r| {
                r.popouts.remove(&peer);
                r.participants
                    .iter()
                    .any(|p| p.profile.id == peer && p.stream.is_some())
            })
            .unwrap_or(false);
        if let Some(h) = self.room.as_ref().and_then(|r| r.handle.as_ref())
            && let Ok(id) = glance_net::EndpointId::from_bytes(&peer.0)
        {
            h.send(glance_net::Command::Unwatch(id));
        }
        if live {
            self.watch(peer, cx);
        }
        cx.notify();
    }

    fn request_stream(&self, peer: PeerId) {
        if let Some(h) = self.room.as_ref().and_then(|r| r.handle.as_ref())
            && let Ok(id) = glance_net::EndpointId::from_bytes(&peer.0)
        {
            h.send(glance_net::Command::Watch(id));
        }
    }
}
