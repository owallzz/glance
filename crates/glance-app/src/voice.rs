//! Voice chat in a live room: the audio engine, mute/deafen, speaking
//! rings, per-user volume and device/processing settings.

use std::time::Duration;

use glance_core::PeerId;
use glance_media::audio::{AudioEngine, EncodedAudio, EngineConfig, InputKind, OutputKind};
use glance_net::{Command, EndpointId};
use glance_proto::AudioDatagram;
use gpui_kit::*;

use crate::app::{GlanceApp, now_ms};

impl GlanceApp {
    /// Start voice for the current room (called when a live room opens or
    /// audio settings change). Failures leave the room usable without voice.
    pub fn start_voice(&mut self, cx: &mut Context<Self>) {
        let config = EngineConfig {
            input: InputKind::Device(self.settings.input_device.clone()),
            output: OutputKind::Device(self.settings.output_device.clone()),
            echo_cancel: self.settings.echo_cancel,
            noise_suppression: self.settings.noise_suppression,
            auto_gain: self.settings.auto_gain,
        };
        let ringing = !self.live.ringing_out.is_empty();
        let Some(room) = self.room.as_mut() else {
            return;
        };
        let Some(handle) = room.handle.as_ref() else {
            return;
        };
        // Stop the old engine first so the devices are free.
        room.audio = None;
        let commander = handle.commander();
        let outgoing = move |pkt: EncodedAudio| {
            let wire = AudioDatagram {
                seq: pkt.seq,
                level: pkt.level,
                data: pkt.data,
                stream: false,
            };
            if let Ok(frame) = glance_proto::encode(&wire) {
                let _ = commander.send(Command::Audio(bytes::Bytes::from(frame[4..].to_vec())));
            }
        };
        let engine = match AudioEngine::start(config, outgoing) {
            Ok(e) => e,
            Err(e) => {
                self.system_message(format!("Voice is unavailable: {e:#}"));
                return;
            }
        };
        engine.set_muted(self.muted);
        engine.set_deafened(self.deafened);
        engine.set_input_volume(self.settings.input_volume);
        engine.set_output_volume(self.settings.output_volume);
        // Network -> engine.
        let (tx, rx) = flume::bounded::<(EndpointId, AudioDatagram)>(512);
        let into_engine = engine.incoming();
        std::thread::Builder::new()
            .name("voice-in".into())
            .spawn(move || {
                while let Ok((from, d)) = rx.recv() {
                    let pkt = EncodedAudio {
                        seq: d.seq,
                        level: d.level,
                        data: d.data,
                    };
                    let key = if d.stream {
                        glance_media::stream_audio::stream_key(*from.as_bytes())
                    } else {
                        *from.as_bytes()
                    };
                    if into_engine.send((key, pkt)).is_err() {
                        break;
                    }
                }
            })
            .ok();
        handle.send(Command::SetAudioSink(Some(tx)));
        for p in &room.participants {
            if let Some(v) = self.volumes.get(&p.profile.id) {
                engine.set_peer_volume(p.profile.id.0, *v);
            }
            if let Some(v) = self.stream_volumes.get(&p.profile.id) {
                engine.set_peer_volume(glance_media::stream_audio::stream_key(p.profile.id.0), *v);
            }
        }
        engine.set_ringback(ringing);
        let problems = engine.problems.clone();
        room.audio = Some(engine);

        // Poll speaking state (and device problems) ~10x a second; only
        // re-render when something changed.
        let task = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(100))
                    .await;
                if let Ok(problem) = problems.try_recv() {
                    let _ = this.update(cx, |app, cx| {
                        app.system_message(problem);
                        cx.notify();
                    });
                }
                let alive = this
                    .update(cx, |app, cx| {
                        if app.update_speaking() {
                            cx.notify();
                        }
                        app.room.as_ref().is_some_and(|r| r.audio.is_some())
                    })
                    .unwrap_or(false);
                if !alive {
                    break;
                }
            }
        });
        if let Some(room) = self.room.as_mut() {
            room._voice_task = Some(task);
        }
    }

    /// Refresh `speaking` flags from the engine. Returns true if any changed.
    fn update_speaking(&mut self) -> bool {
        let me = self.me.id;
        let Some(room) = self.room.as_mut() else {
            return false;
        };
        let Some(engine) = room.audio.as_ref() else {
            return false;
        };
        let speaking: Vec<[u8; 32]> = engine.speaking_peers();
        let me_speaking = engine.me_speaking();
        let mut changed = false;
        for p in room.participants.iter_mut() {
            let now = if p.profile.id == me {
                me_speaking
            } else {
                !p.muted && speaking.contains(&p.profile.id.0)
            };
            if p.speaking != now {
                p.speaking = now;
                changed = true;
            }
        }
        changed
    }

    /// Push mute/deafen to the engine (called from set_muted/set_deafened).
    pub fn sync_voice(&mut self) {
        if let Some(engine) = self.room.as_ref().and_then(|r| r.audio.as_ref()) {
            engine.set_muted(self.muted);
            engine.set_deafened(self.deafened);
        }
    }

    /// A mic (`input`) or speaker volume slider moved: apply it live, save
    /// when let go.
    pub fn on_volume_slider(
        &mut self,
        input: bool,
        ev: &gpui_kit::component::slider::SliderEvent,
        cx: &mut Context<Self>,
    ) {
        use gpui_kit::component::slider::SliderEvent;
        let (SliderEvent::Change(v) | SliderEvent::Release(v)) = ev;
        let volume = (v.start() / 100.0).clamp(0.0, 2.0);
        if input {
            self.settings.input_volume = volume;
        } else {
            self.settings.output_volume = volume;
        }
        if let Some(engine) = self.room.as_ref().and_then(|r| r.audio.as_ref()) {
            engine.set_input_volume(self.settings.input_volume);
            engine.set_output_volume(self.settings.output_volume);
        }
        if matches!(ev, SliderEvent::Release(_)) {
            self.save_settings();
        }
        cx.notify();
    }

    /// Per-user volume, 0–200%.
    pub fn set_user_volume(&mut self, peer: PeerId, volume: f32) {
        let v = volume.clamp(0.0, 2.0);
        self.volumes.insert(peer, v);
        if let Some(engine) = self.room.as_ref().and_then(|r| r.audio.as_ref()) {
            engine.set_peer_volume(peer.0, v);
        }
    }

    pub fn user_volume(&self, peer: PeerId) -> f32 {
        self.volumes.get(&peer).copied().unwrap_or(1.0)
    }

    /// Volume of the audio shared with someone's screen, 0–200%.
    pub fn set_stream_volume(&mut self, peer: PeerId, volume: f32) {
        let v = volume.clamp(0.0, 2.0);
        self.stream_volumes.insert(peer, v);
        if let Some(engine) = self.room.as_ref().and_then(|r| r.audio.as_ref()) {
            engine.set_peer_volume(glance_media::stream_audio::stream_key(peer.0), v);
        }
    }

    pub fn stream_volume(&self, peer: PeerId) -> f32 {
        self.stream_volumes.get(&peer).copied().unwrap_or(1.0)
    }

    pub fn apply_audio_processing(&mut self) {
        if let Some(engine) = self.room.as_ref().and_then(|r| r.audio.as_ref()) {
            engine.set_processing(
                self.settings.echo_cancel,
                self.settings.noise_suppression,
                self.settings.auto_gain,
            );
        }
    }

    /// Append a system line to the room chat.
    pub fn system_message(&mut self, text: impl Into<String>) {
        let me = self.me.id;
        if let Some(room) = self.room.as_mut() {
            let id = room.chat.last().map_or(1, |m| m.id + 1);
            room.chat.push(glance_core::ChatMessage {
                id,
                author: me,
                sent_at: now_ms(),
                body: glance_core::ChatBody::System(text.into()),
            });
        }
    }
}
