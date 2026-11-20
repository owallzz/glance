//! Screen sharing in a live room: Go Live (capture -> encode -> send),
//! a preview of your own stream, and watching others (receive -> decode).

use std::sync::Arc;

use glance_core::{PeerId, Resolution, StreamInfo};
use glance_media::TargetSize;
use glance_media::capture::{self, Selection, SourceKind};
use glance_media::pipeline::{BroadcastConfig, output_size, spawn_decoder, start_broadcast};
use glance_net::{Command, EndpointId, VideoFeed};
use glance_proto::VideoPacket;
use gpui_kit::*;

use crate::app::{GlanceApp, GoLive, now_ms};
use crate::video::VideoView;

impl GlanceApp {
    /// Open the Go Live picker with this machine's real capture sources.
    pub fn open_go_live(&mut self, cx: &mut Context<Self>) {
        let system_picker = capture::uses_system_picker();
        let sources = if system_picker {
            Vec::new()
        } else {
            capture::list_sources()
        };
        let has_screens = sources.iter().any(|s| s.kind == SourceKind::Screen);
        self.go_live = Some(GoLive {
            windows_tab: !has_screens && !sources.is_empty(),
            source: 0,
            sources,
            system_picker,
            resolution: Resolution::P1080,
            fps: 60,
            error: None,
        });
        cx.notify();
    }

    /// The source currently selected in the picker, if any.
    pub fn go_live_selection(&self) -> Option<Selection> {
        let gl = self.go_live.as_ref()?;
        if gl.system_picker {
            return Some(Selection::SystemPicker);
        }
        let kind = if gl.windows_tab {
            SourceKind::Window
        } else {
            SourceKind::Screen
        };
        gl.sources
            .iter()
            .filter(|s| s.kind == kind)
            .nth(gl.source)
            .cloned()
            .map(Selection::Source)
    }

    /// "Go Live": start capturing and broadcasting.
    pub fn start_streaming(&mut self, cx: &mut Context<Self>) {
        let Some(selection) = self.go_live_selection() else {
            if let Some(gl) = self.go_live.as_mut() {
                gl.error = Some("Pick a screen or window first.".into());
            }
            cx.notify();
            return;
        };
        let (resolution, fps) = match &self.go_live {
            Some(gl) => (gl.resolution, gl.fps),
            None => return,
        };
        if !capture::has_permission() && !capture::request_permission() {
            if let Some(gl) = self.go_live.as_mut() {
                gl.error = Some(
                    "glance needs permission to record your screen. Allow it in System Settings → Privacy & Security → Screen Recording, then try again."
                        .into(),
                );
            }
            cx.notify();
            return;
        }
        let Some(room) = self.room.as_ref() else {
            return;
        };
        let Some(handle) = room.handle.as_ref() else {
            // Demo room: keep the fake stream behaviour.
            self.start_demo_stream(cx);
            return;
        };
        let source_name = match &selection {
            Selection::SystemPicker => "Screen".to_string(),
            Selection::Source(s) => s.name.clone(),
        };
        let initial_size = match &selection {
            Selection::Source(s) => s.size.map(|sz| output_size(sz, resolution.max_height())),
            Selection::SystemPicker => None,
        };
        let capture = match capture::start(selection, fps) {
            Ok(c) => c,
            Err(e) => {
                if let Some(gl) = self.go_live.as_mut() {
                    gl.error = Some(format!("Couldn't start screen capture: {e:#}"));
                }
                cx.notify();
                return;
            }
        };

        // Encoded packets go straight from the broadcast thread to the room.
        let commander = handle.commander();
        let sink = move |pkt: glance_media::codec::EncodedPacket| {
            let _ = commander.send(Command::Video(
                glance_proto::Track::Screen,
                Arc::new(VideoPacket {
                    ts_us: pkt.ts_us,
                    keyframe: pkt.keyframe,
                    width: pkt.width,
                    height: pkt.height,
                    data: pkt.data,
                }),
            ));
        };
        let preview_target = TargetSize::default();
        let (preview_tx, preview_rx) = flume::bounded(2);
        let (control, ended) = start_broadcast(
            capture,
            BroadcastConfig {
                max_height: resolution.max_height(),
                fps,
            },
            Some((preview_tx, preview_target.clone())),
            sink,
        );

        let me = self.me.id;
        let preview = cx.new(|cx| VideoView::new(preview_rx, preview_target, cx));
        let (w, h) = initial_size.unwrap_or((0, 0));
        let control_for_size = control.clone();
        // When the broadcast ends (stopped, or the user ended it from the OS),
        // clear our live state. Also fill in the real size once known.
        let task = cx.spawn(async move |this, cx| {
            let mut size_known = w > 0;
            loop {
                cx.background_executor()
                    .timer(std::time::Duration::from_millis(400))
                    .await;
                if let Ok(reason) = ended.try_recv() {
                    let _ = this.update(cx, |app, cx| app.on_broadcast_ended(reason, cx));
                    break;
                }
                let gone = this
                    .update(cx, |app, _| {
                        app.room.as_ref().is_none_or(|r| r.broadcast.is_none())
                    })
                    .unwrap_or(true);
                if gone {
                    break;
                }
                // Preview label: who's watching and at what bitrate.
                let watchers = control_for_size
                    .watchers
                    .load(std::sync::atomic::Ordering::Relaxed);
                let kbps = control_for_size
                    .bitrate
                    .load(std::sync::atomic::Ordering::Relaxed)
                    / 1000;
                let label: SharedString = if watchers == 0 {
                    "Nobody watching yet".into()
                } else {
                    format!("Live to {watchers} · {:.1} Mbps", kbps as f64 / 1000.0).into()
                };
                let _ = this.update(cx, |app, cx| {
                    let me = app.me.id;
                    if let Some(v) = app.room.as_ref().and_then(|r| r.watching.get(&me)).cloned() {
                        v.update(cx, |v, cx| {
                            if v.label.as_ref() != Some(&label) {
                                v.label = Some(label.clone());
                                cx.notify();
                            }
                        });
                    }
                });
                if !size_known {
                    let src = control_for_size.source_size();
                    if src.0 > 0 {
                        size_known = true;
                        let out = output_size(src, resolution.max_height());
                        let _ = this.update(cx, |app, cx| app.set_stream_size(out, cx));
                    }
                }
            }
        });

        let room = self.room.as_mut().expect("room");
        room.broadcast = Some(control);
        room._broadcast_task = Some(task);
        room.watching.insert(me, preview);
        if let Some(x) = room.participants.iter_mut().find(|x| x.profile.id == me) {
            x.stream = Some(StreamInfo {
                width: w,
                height: h,
                fps,
                has_audio: false,
                source: source_name,
            });
        }
        self.go_live = None;
        self.push_state();
        cx.notify();
    }

    fn set_stream_size(&mut self, size: (u32, u32), cx: &mut Context<Self>) {
        let me = self.me.id;
        if let Some(room) = self.room.as_mut()
            && let Some(s) = room
                .participants
                .iter_mut()
                .find(|x| x.profile.id == me)
                .and_then(|x| x.stream.as_mut())
        {
            s.width = size.0;
            s.height = size.1;
        }
        self.push_state();
        cx.notify();
    }

    /// Stop sharing (button, or the broadcast ended on its own).
    pub fn stop_streaming(&mut self, cx: &mut Context<Self>) {
        let me = self.me.id;
        if let Some(room) = self.room.as_mut() {
            if let Some(b) = room.broadcast.take() {
                b.stop();
            }
            room._broadcast_task = None;
            room.watching.remove(&me);
            if room.focused == Some(me) {
                room.focused = None;
            }
            if let Some(x) = room.participants.iter_mut().find(|x| x.profile.id == me) {
                x.stream = None;
            }
        }
        self.push_state();
        cx.notify();
    }

    fn on_broadcast_ended(&mut self, reason: String, cx: &mut Context<Self>) {
        let live = self.room.as_ref().is_some_and(|r| r.broadcast.is_some());
        if !live {
            return;
        }
        self.stop_streaming(cx);
        if reason != "stopped" {
            let me = self.me.id;
            if let Some(room) = self.room.as_mut() {
                let id = room.chat.last().map_or(1, |m| m.id + 1);
                room.chat.push(glance_core::ChatMessage {
                    id,
                    author: me,
                    sent_at: now_ms(),
                    body: glance_core::ChatBody::System(format!("Your stream ended: {reason}")),
                });
            }
        }
    }

    /// Ask a sharer for their stream; frames show up when it arrives.
    pub fn watch_live(&mut self, peer: PeerId, cx: &mut Context<Self>) {
        let Some(room) = self.room.as_mut() else {
            return;
        };
        let Some(handle) = &room.handle else { return };
        let Ok(id) = EndpointId::from_bytes(&peer.0) else {
            return;
        };
        let view = cx.new(|_| VideoView::empty(TargetSize::default()));
        room.watching.insert(peer, view);
        handle.send(Command::Watch(id, glance_proto::Track::Screen));
        cx.notify();
    }

    /// A sharer's video stream arrived: decode it into their tile.
    pub fn on_video_feed(
        &mut self,
        peer: PeerId,
        track: glance_proto::Track,
        feed: VideoFeed,
        cx: &mut Context<Self>,
    ) {
        let Some(room) = self.room.as_mut() else {
            return;
        };
        // Cameras go to camera tiles; a popped-out screen to its own window.
        let view = match track {
            glance_proto::Track::Camera => room.cameras.get(&peer).cloned(),
            glance_proto::Track::Screen => room
                .popouts
                .get(&peer)
                .or_else(|| room.watching.get(&peer))
                .cloned(),
        };
        let Some(view) = view else {
            return; // stopped watching meanwhile; dropping the feed ends it
        };
        let Some(handle) = &room.handle else { return };
        let Ok(id) = EndpointId::from_bytes(&peer.0) else {
            return;
        };
        let commander = handle.commander();
        let target = view.read(cx).target();
        let packets = feed.0;
        let bytes = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let counter = bytes.clone();
        let frames = spawn_decoder(
            move || {
                packets.recv().ok().map(|p| {
                    counter.fetch_add(p.data.len() as u64, std::sync::atomic::Ordering::Relaxed);
                    p.data
                })
            },
            target,
            {
                // At most one keyframe request a second.
                let mut last: Option<std::time::Instant> = None;
                move || {
                    let now = std::time::Instant::now();
                    if last.is_none_or(|t| now - t >= std::time::Duration::from_secs(1)) {
                        last = Some(now);
                        let _ = commander.send(Command::RequestKeyframe(id, track));
                    }
                }
            },
        );
        view.update(cx, |v, cx| {
            v.set_bytes_counter(bytes);
            v.set_source(frames, cx);
        });
    }
}

#[cfg(test)]
mod tests {
    use super::output_size;

    #[test]
    fn preset_sizes_match_core_rules() {
        // glance-core's Resolution::output_size and the encoder's output_size
        // must agree, or the picker shows the wrong number.
        for (src, r) in [
            ((3840, 2160), glance_core::Resolution::P1080),
            ((5120, 1440), glance_core::Resolution::P1440),
            ((1367, 769), glance_core::Resolution::Source),
        ] {
            assert_eq!(output_size(src, r.max_height()), r.output_size(src));
        }
    }
}
