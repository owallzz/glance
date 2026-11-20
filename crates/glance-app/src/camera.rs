//! Webcams in a live room: turn yours on/off, and show everyone's camera
//! automatically (like Discord's camera tiles).

use std::sync::Arc;

use glance_core::PeerId;
use glance_media::TargetSize;
use glance_media::pipeline::{BroadcastConfig, start_broadcast};
use glance_net::{Command, EndpointId};
use glance_proto::{Track, VideoPacket};
use gpui_kit::*;

use crate::app::GlanceApp;
use crate::video::VideoView;

/// Camera video size and rate: enough for a face, cheap for everyone.
pub const CAMERA_SIZE: (u32, u32) = (640, 360);
pub const CAMERA_FPS: u32 = 24;

impl GlanceApp {
    pub fn camera_on(&self) -> bool {
        self.room
            .as_ref()
            .is_some_and(|r| r.camera_broadcast.is_some())
    }

    pub fn toggle_camera(&mut self, cx: &mut Context<Self>) {
        if self.camera_on() {
            self.stop_camera(cx);
        } else {
            self.start_camera(cx);
        }
    }

    fn start_camera(&mut self, cx: &mut Context<Self>) {
        let Some(handle) = self.room.as_ref().and_then(|r| r.handle.as_ref()) else {
            // Demo room: just flip the flag.
            let me = self.me.id;
            if let Some(room) = self.room.as_mut()
                && let Some(p) = room.participants.iter_mut().find(|p| p.profile.id == me)
            {
                p.camera = !p.camera;
            }
            cx.notify();
            return;
        };
        let (w, h) = CAMERA_SIZE;
        let capture =
            match glance_media::camera::start(self.settings.camera_device, w, h, CAMERA_FPS) {
                Ok(c) => c,
                Err(e) => {
                    self.system_message(format!("{e:#}"));
                    cx.notify();
                    return;
                }
            };
        let commander = handle.commander();
        let preview_target = TargetSize::default();
        let (preview_tx, preview_rx) = flume::bounded(2);
        let (control, ended) = start_broadcast(
            capture,
            BroadcastConfig {
                max_height: Some(h),
                fps: CAMERA_FPS,
                encoder: glance_media::codec::EncoderChoice::Auto,
            },
            Some((preview_tx, preview_target.clone())),
            move |pkt| {
                let _ = commander.send(Command::Video(
                    Track::Camera,
                    Arc::new(VideoPacket {
                        ts_us: pkt.ts_us,
                        keyframe: pkt.keyframe,
                        width: pkt.width,
                        height: pkt.height,
                        data: pkt.data,
                    }),
                ));
            },
        );
        let me = self.me.id;
        let preview = cx.new(|cx| {
            let mut v = VideoView::new(preview_rx, preview_target, cx);
            v.cover = true;
            v.show_stats = false;
            v
        });
        let task = cx.spawn(async move |this, cx| {
            if let Ok(reason) = ended.recv_async().await {
                let _ = this.update(cx, |app, cx| {
                    if app.camera_on() {
                        app.stop_camera(cx);
                        if reason != "stopped" {
                            app.system_message(format!("Your camera turned off: {reason}"));
                        }
                    }
                });
            }
        });
        if let Some(room) = self.room.as_mut() {
            room.camera_broadcast = Some(control);
            room._camera_task = Some(task);
            room.cameras.insert(me, preview);
            if let Some(p) = room.participants.iter_mut().find(|p| p.profile.id == me) {
                p.camera = true;
            }
        }
        self.push_state();
        cx.notify();
    }

    pub fn stop_camera(&mut self, cx: &mut Context<Self>) {
        let me = self.me.id;
        if let Some(room) = self.room.as_mut() {
            if let Some(b) = room.camera_broadcast.take() {
                b.stop();
            }
            room._camera_task = None;
            room.cameras.remove(&me);
            if let Some(p) = room.participants.iter_mut().find(|p| p.profile.id == me) {
                p.camera = false;
            }
        }
        self.push_state();
        cx.notify();
    }

    /// Watch every camera that's on; drop tiles for cameras that went off.
    pub fn sync_cameras(&mut self, cx: &mut Context<Self>) {
        let me = self.me.id;
        let Some(room) = self.room.as_mut() else {
            return;
        };
        let Some(handle) = room.handle.as_ref() else {
            return;
        };
        let on: Vec<PeerId> = room
            .participants
            .iter()
            .filter(|p| p.camera && p.profile.id != me)
            .map(|p| p.profile.id)
            .collect();
        room.cameras.retain(|id, _| *id == me || on.contains(id));
        for peer in on {
            if room.cameras.contains_key(&peer) {
                continue;
            }
            let Ok(id) = EndpointId::from_bytes(&peer.0) else {
                continue;
            };
            let view = cx.new(|_| {
                let mut v = VideoView::empty(TargetSize::default());
                v.cover = true;
                v.show_stats = false;
                v
            });
            room.cameras.insert(peer, view);
            handle.send(Command::Watch(id, Track::Camera));
        }
    }
}
