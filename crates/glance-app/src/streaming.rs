//! Screen sharing in a live room: Go Live (capture -> encode -> send),
//! changing a live stream's quality, sound or source without stopping it,
//! a preview of your own stream, and watching others (receive -> decode).

use std::sync::Arc;

use glance_core::{PeerId, StreamInfo};
use glance_media::TargetSize;
use glance_media::capture::{self, Selection, SourceKind};
use glance_media::pipeline::{
    BroadcastConfig, BroadcastNotice, output_size, spawn_decoder, start_broadcast,
};
use glance_media::stream_audio::{AudioSource, StreamAudio};
use glance_net::{Command, EndpointId, RoomHandle, VideoFeed};
use glance_proto::VideoPacket;
use gpui_kit::*;

use crate::app::{GlanceApp, GoLive, LiveSettings, now_ms};
use crate::video::VideoView;

impl GlanceApp {
    /// Open the Go Live picker with this machine's real capture sources.
    pub fn open_go_live(&mut self, cx: &mut Context<Self>) {
        let (resolution, fps) = (self.settings.stream_resolution, self.settings.stream_fps);
        self.open_picker(resolution, fps, None, None, cx);
    }

    /// While live: the same picker as "Stream settings", showing what the
    /// stream uses now, with "Apply" instead of "Go Live".
    pub fn open_stream_settings(&mut self, cx: &mut Context<Self>) {
        let Some(live) = self.room.as_ref().and_then(|r| r.live_settings.clone()) else {
            return;
        };
        let current = live.pending_selection.unwrap_or(live.selection);
        self.open_picker(live.resolution, live.fps, live.audio, Some(current), cx);
    }

    /// `editing`: the running stream's source (the picker opens on it).
    fn open_picker(
        &mut self,
        resolution: glance_core::Resolution,
        fps: u32,
        audio: Option<AudioSource>,
        editing: Option<Selection>,
        cx: &mut Context<Self>,
    ) {
        let system_picker = capture::uses_system_picker();
        let mut sources = if system_picker {
            Vec::new()
        } else {
            capture::list_sources()
        };
        let (windows_tab, source) = preselect(&mut sources, editing.as_ref());
        self.go_live = Some(GoLive {
            windows_tab,
            source,
            sources,
            system_picker,
            resolution,
            fps,
            error: None,
            // The current choice stays listed until the real list arrives.
            audio_sources: audio.iter().cloned().collect(),
            audio,
            editing: editing.is_some(),
        });
        // Listing audio apps talks to PipeWire; keep it off the UI thread.
        if glance_media::stream_audio::supported() {
            let list = cx.background_spawn(async { glance_media::stream_audio::list_sources() });
            cx.spawn(async move |this, cx| {
                let mut sources = list.await;
                let _ = this.update(cx, |app, cx| {
                    if let Some(gl) = app.go_live.as_mut() {
                        // An app being shared now stays pickable even if
                        // PipeWire lists it differently by now.
                        if let Some(cur) = &gl.audio
                            && !sources.contains(cur)
                        {
                            sources.push(cur.clone());
                        }
                        gl.audio_sources = sources;
                        cx.notify();
                    }
                });
            })
            .detach();
        }
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
        let (resolution, fps, audio_source) = match &self.go_live {
            Some(gl) => (gl.resolution, gl.fps, gl.audio.clone()),
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
        let source_name = source_label(&selection);
        let initial_size = match &selection {
            Selection::Source(s) => s.size.map(|sz| output_size(sz, resolution.max_height())),
            Selection::SystemPicker => None,
        };
        let capture = match capture::start(selection.clone(), fps) {
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
                encoder: if self.settings.hardware_encoding {
                    glance_media::codec::EncoderChoice::Auto
                } else {
                    glance_media::codec::EncoderChoice::Software
                },
            },
            Some((preview_tx, preview_target.clone())),
            sink,
        );

        let me = self.me.id;
        let preview = cx.new(|cx| VideoView::new(preview_rx, preview_target, cx));
        let (w, h) = initial_size.unwrap_or((0, 0));
        let control_for_size = control.clone();
        // When the broadcast ends (stopped, or the user ended it from the OS),
        // clear our live state. Also keep the advertised size up to date (it
        // changes with the quality and the source) and report source switches.
        let task = cx.spawn(async move |this, cx| {
            let mut size = (w, h);
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
                while let Some(notice) = control_for_size.try_notice() {
                    let _ = this.update(cx, |app, cx| app.on_broadcast_notice(notice, cx));
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
                    let hw = control_for_size
                        .hardware
                        .load(std::sync::atomic::Ordering::Relaxed);
                    format!(
                        "Live to {watchers} · {:.1} Mbps{}",
                        kbps as f64 / 1000.0,
                        if hw { " · GPU" } else { "" }
                    )
                    .into()
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
                let out = control_for_size.output_size();
                if out.0 > 0 && out != size {
                    size = out;
                    let _ = this.update(cx, |app, cx| app.set_stream_size(out, cx));
                }
            }
        });

        // Audio goes only to people watching; a failure keeps the video.
        let mut audio_error = None;
        let stream_audio =
            audio_source
                .clone()
                .and_then(|src| match start_stream_audio(handle, src) {
                    Ok(s) => Some(s),
                    Err(e) => {
                        audio_error = Some(format!("Sharing audio didn't work: {e:#}"));
                        None
                    }
                });
        let has_audio = stream_audio.is_some();
        if let Some(e) = audio_error {
            self.system_message(e);
        }

        let room = self.room.as_mut().expect("room");
        room.broadcast = Some(control);
        room.live_settings = Some(LiveSettings {
            selection,
            pending_selection: None,
            resolution,
            fps,
            audio: audio_source.filter(|_| has_audio),
        });
        room.stream_audio = stream_audio;
        room._broadcast_task = Some(task);
        room.watching.insert(me, preview);
        if let Some(x) = room.participants.iter_mut().find(|x| x.profile.id == me) {
            x.stream = Some(StreamInfo {
                width: w,
                height: h,
                fps,
                has_audio,
                source: source_name,
            });
        }
        self.go_live = None;
        self.remember_quality(resolution, fps);
        self.push_state();
        cx.notify();
    }

    fn remember_quality(&mut self, resolution: glance_core::Resolution, fps: u32) {
        if (self.settings.stream_resolution, self.settings.stream_fps) != (resolution, fps) {
            self.settings.stream_resolution = resolution;
            self.settings.stream_fps = fps;
            self.save_settings();
        }
    }

    /// "Apply" in Stream settings: change what differs (quality, source,
    /// sound) on the running stream. Viewers keep watching; a new size or
    /// frame rate costs them one keyframe.
    pub fn apply_stream_settings(&mut self, cx: &mut Context<Self>) {
        let Some(gl) = self.go_live.as_ref() else {
            return;
        };
        let (resolution, fps, audio) = (gl.resolution, gl.fps, gl.audio.clone());
        let picked = if gl.system_picker {
            None // the portal is reopened with "Change source" instead
        } else {
            self.go_live_selection()
        };
        let Some(room) = self.room.as_mut() else {
            return;
        };
        let me = self.me.id;
        if room.handle.is_none() {
            // Demo room: only the fake stream's numbers change.
            if let Some(live) = room.live_settings.as_mut() {
                (live.resolution, live.fps, live.audio) = (resolution, fps, audio);
            }
            if let Some(s) = room
                .participants
                .iter_mut()
                .find(|x| x.profile.id == me)
                .and_then(|x| x.stream.as_mut())
            {
                s.fps = fps;
            }
            self.go_live = None;
            cx.notify();
            return;
        }
        let (Some(control), Some(live), Some(handle)) = (
            room.broadcast.clone(),
            room.live_settings.as_mut(),
            room.handle.as_ref(),
        ) else {
            self.go_live = None;
            cx.notify();
            return;
        };
        let mut problems = Vec::new();

        // The source first: a capture that can't start leaves the rest as is.
        let current = live.pending_selection.as_ref().unwrap_or(&live.selection);
        if let Some(sel) = picked.filter(|s| !same_source(s, current)) {
            match capture::start(sel.clone(), fps) {
                Ok(c) => {
                    control.replace_capture(c, source_label(&sel));
                    live.pending_selection = Some(sel);
                }
                Err(e) => {
                    if let Some(gl) = self.go_live.as_mut() {
                        gl.error = Some(format!("Couldn't capture that: {e:#}"));
                    }
                    cx.notify();
                    return;
                }
            }
        }

        if (live.resolution, live.fps) != (resolution, fps) {
            control.reconfigure(resolution.max_height(), fps);
            live.resolution = resolution;
            live.fps = fps;
        }

        let mut has_audio = room.stream_audio.is_some();
        if live.audio != audio {
            // Stop the old capture before starting the new one, so "Entire
            // system" never picks up our own previous share.
            room.stream_audio = None;
            live.audio = None;
            has_audio = false;
            if let Some(src) = audio {
                match start_stream_audio(handle, src.clone()) {
                    Ok(s) => {
                        room.stream_audio = Some(s);
                        live.audio = Some(src);
                        has_audio = true;
                    }
                    Err(e) => problems.push(format!("Sharing audio didn't work: {e:#}")),
                }
            }
        }

        if let Some(s) = room
            .participants
            .iter_mut()
            .find(|x| x.profile.id == me)
            .and_then(|x| x.stream.as_mut())
        {
            s.fps = fps;
            s.has_audio = has_audio;
        }
        self.go_live = None;
        self.remember_quality(resolution, fps);
        for p in problems {
            self.system_message(p);
        }
        self.push_state();
        cx.notify();
    }

    /// "Change source" where the system picker chooses (Wayland portal):
    /// open it again; the stream switches over once something is picked
    /// and keeps going unchanged if the dialog is cancelled.
    pub fn change_stream_source(&mut self, cx: &mut Context<Self>) {
        let Some(room) = self.room.as_mut() else {
            return;
        };
        let (Some(control), Some(live)) = (room.broadcast.clone(), room.live_settings.as_mut())
        else {
            return;
        };
        match capture::start(Selection::SystemPicker, live.fps) {
            Ok(c) => {
                control.replace_capture(c, source_label(&Selection::SystemPicker));
                live.pending_selection = Some(Selection::SystemPicker);
                // Quality and sound picked in the same window apply too
                // (this also closes it).
                self.apply_stream_settings(cx);
            }
            Err(e) => {
                if let Some(gl) = self.go_live.as_mut() {
                    gl.error = Some(format!("Couldn't start screen capture: {e:#}"));
                }
                cx.notify();
            }
        }
    }

    fn on_broadcast_notice(&mut self, notice: BroadcastNotice, cx: &mut Context<Self>) {
        let me = self.me.id;
        let Some(room) = self.room.as_mut() else {
            return;
        };
        match notice {
            BroadcastNotice::SourceChanged(label) => {
                if let Some(live) = room.live_settings.as_mut()
                    && let Some(sel) = live.pending_selection.take()
                {
                    live.selection = sel;
                }
                if let Some(s) = room
                    .participants
                    .iter_mut()
                    .find(|x| x.profile.id == me)
                    .and_then(|x| x.stream.as_mut())
                {
                    s.source = label;
                }
                self.push_state();
            }
            BroadcastNotice::SourceFailed { label, reason } => {
                if let Some(live) = room.live_settings.as_mut() {
                    live.pending_selection = None;
                }
                self.system_message(format!(
                    "Still sharing the same thing: switching to {label} didn't work ({reason})."
                ));
            }
        }
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
            room.stream_audio = None;
            room.live_settings = None;
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
                .map(|p| p.video.clone())
                .or_else(|| room.watching.get(&peer).cloned()),
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

/// Which tab and card the picker opens on. While editing, that is the
/// running stream's source; if the fresh list misses it (a minimized or
/// untitled window, another Space), it is added so it stays selected.
/// Otherwise "Apply" would quietly switch the stream to the first screen.
fn preselect(sources: &mut Vec<capture::Source>, editing: Option<&Selection>) -> (bool, usize) {
    if let Some(Selection::Source(cur)) = editing {
        let of_kind = || sources.iter().filter(|s| s.kind == cur.kind);
        let index = match of_kind().position(|s| s.id == cur.id) {
            Some(i) => i,
            None => {
                let at = sources
                    .iter()
                    .position(|s| s.kind == cur.kind)
                    .unwrap_or(sources.len());
                sources.insert(at, cur.clone());
                0
            }
        };
        return (cur.kind == SourceKind::Window, index);
    }
    let has_screens = sources.iter().any(|s| s.kind == SourceKind::Screen);
    (!has_screens && !sources.is_empty(), 0)
}

/// Whether two selections capture the same thing (a window's title or a
/// screen's size may have changed since).
fn same_source(a: &Selection, b: &Selection) -> bool {
    match (a, b) {
        (Selection::Source(a), Selection::Source(b)) => (a.kind, a.id) == (b.kind, b.id),
        (a, b) => a == b,
    }
}

/// What a selection is called in the stream's badge.
fn source_label(selection: &Selection) -> String {
    match selection {
        Selection::SystemPicker => "Screen".to_string(),
        Selection::Source(s) => s.name.clone(),
    }
}

/// Capture `src` and send it, Opus-encoded, to the people watching us.
fn start_stream_audio(handle: &RoomHandle, src: AudioSource) -> anyhow::Result<StreamAudio> {
    let commander = handle.commander();
    let sink = move |pkt: glance_media::audio::EncodedAudio| {
        let wire = glance_proto::AudioDatagram {
            seq: pkt.seq,
            level: pkt.level,
            data: pkt.data,
            stream: true,
        };
        if let Ok(frame) = glance_proto::encode(&wire) {
            let _ = commander.send(Command::StreamAudio(bytes::Bytes::from(
                frame[4..].to_vec(),
            )));
        }
    };
    glance_media::stream_audio::start(src, sink)
}

#[cfg(test)]
mod tests {
    use super::{output_size, preselect, same_source};
    use glance_media::capture::{Selection, Source, SourceKind};

    fn source(kind: SourceKind, id: u32) -> Source {
        Source {
            id,
            name: format!("{kind:?} {id}"),
            kind,
            size: None,
        }
    }

    #[test]
    fn editing_opens_on_the_running_source() {
        let list = vec![
            source(SourceKind::Screen, 1),
            source(SourceKind::Window, 7),
            source(SourceKind::Window, 9),
        ];
        let mut sources = list.clone();
        let cur = Selection::Source(source(SourceKind::Window, 9));
        assert_eq!(preselect(&mut sources, Some(&cur)), (true, 1));
        assert_eq!(sources, list);
        // Nothing being edited: screens first.
        assert_eq!(preselect(&mut sources, None), (false, 0));
    }

    #[test]
    fn an_unlisted_running_source_stays_selected() {
        // A minimized window can be missing from a fresh list; "Apply" must
        // not fall back to the first screen.
        let mut sources = vec![source(SourceKind::Screen, 1), source(SourceKind::Window, 7)];
        let cur = source(SourceKind::Window, 9);
        let (windows_tab, i) = preselect(&mut sources, Some(&Selection::Source(cur.clone())));
        assert!(windows_tab);
        let picked = sources
            .iter()
            .filter(|s| s.kind == SourceKind::Window)
            .nth(i)
            .unwrap();
        assert_eq!(picked, &cur);
        assert!(same_source(
            &Selection::Source(picked.clone()),
            &Selection::Source(cur)
        ));

        let mut empty = Vec::new();
        let screen = source(SourceKind::Screen, 2);
        assert_eq!(
            preselect(&mut empty, Some(&Selection::Source(screen.clone()))),
            (false, 0)
        );
        assert_eq!(empty, vec![screen]);
    }

    #[test]
    fn a_renamed_window_is_the_same_source() {
        let src = |id, name: &str| {
            Selection::Source(Source {
                id,
                name: name.into(),
                kind: SourceKind::Window,
                size: None,
            })
        };
        assert!(same_source(&src(3, "Editor"), &src(3, "Editor — file.rs")));
        assert!(!same_source(&src(3, "Editor"), &src(4, "Editor")));
        assert!(!same_source(&src(3, "Editor"), &Selection::SystemPicker));
        assert!(same_source(
            &Selection::SystemPicker,
            &Selection::SystemPicker
        ));
    }

    #[test]
    fn preset_sizes_match_core_rules() {
        // glance-core's Resolution::output_size and the encoder's output_size
        // must agree, or the picker shows the wrong number.
        for (src, r) in [
            ((3840, 2160), glance_core::Resolution::P1080),
            ((5120, 1440), glance_core::Resolution::P1440),
            ((1367, 769), glance_core::Resolution::Source),
            ((5120, 1440), glance_core::Resolution::Source),
            ((5120, 1440), glance_core::Resolution::P2160),
            ((2160, 3840), glance_core::Resolution::Source),
            ((3440, 1440), glance_core::Resolution::P1080),
            ((7680, 4320), glance_core::Resolution::Source),
            ((4096, 2160), glance_core::Resolution::Source),
            ((5120, 2160), glance_core::Resolution::P2160),
        ] {
            assert_eq!(output_size(src, r.max_height()), r.output_size(src));
        }
    }
}
