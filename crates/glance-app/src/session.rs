//! Live rooms: start/join over the network and apply room events to the UI
//! state. The `--demo` room bypasses all of this.

use std::collections::HashMap;
use std::time::Instant;

use glance_core::{
    ChatBody, ChatMessage, ConnectionInfo, Participant, PeerId, Profile, Route, StreamInfo,
};
use glance_net::{Command, RoomEvent, RoomHandle};
use glance_proto::{MemberState, Ticket, WireStream};
use gpui_kit::component::input::{InputEvent, InputState};
use gpui_kit::*;

use crate::app::{GlanceApp, Room, Screen, now_ms};
use crate::net::{SavedProfile, peer_id, save_profile, wire_profile};

/// Your participant row as the state the room protocol carries.
pub fn member_state(p: &Participant) -> MemberState {
    MemberState {
        muted: p.muted,
        deafened: p.deafened,
        camera: p.camera,
        stream: p.stream.as_ref().map(|s| WireStream {
            width: s.width,
            height: s.height,
            fps: s.fps,
            has_audio: s.has_audio,
            source: s.source.clone(),
        }),
    }
}

fn apply_state(p: &mut Participant, s: MemberState) {
    p.muted = s.muted;
    p.deafened = s.deafened;
    p.camera = s.camera;
    p.stream = s.stream.map(|w| StreamInfo {
        width: w.width,
        height: w.height,
        fps: w.fps,
        has_audio: w.has_audio,
        source: w.source,
    });
}

impl GlanceApp {
    fn self_participant(&self) -> Participant {
        Participant {
            profile: self.me.clone(),
            muted: self.voice.muted,
            deafened: self.voice.deafened,
            speaking: false,
            camera: false,
            stream: None,
            connection: None,
            is_contact: false,
        }
    }

    /// Start a new room; the invite link is copied when it's ready.
    pub fn start_room(&mut self, cx: &mut Context<Self>) {
        if self.room.is_some() {
            self.screen = Screen::Room;
            cx.notify();
            return;
        }
        let Some(net) = self.net.clone() else {
            self.join_error = Some("Networking isn't available; see the log.".into());
            cx.notify();
            return;
        };
        if self.busy {
            return;
        }
        self.busy = true;
        self.join_error = None;
        let state = member_state(&self.self_participant());
        let fut = net.create_room(wire_profile(&self.me), state);
        cx.spawn(async move |this, cx| {
            let result = fut.await;
            this.update(cx, |app, cx| {
                app.busy = false;
                match result {
                    Ok(handle) => {
                        let link = handle.link();
                        tracing::info!("room link: {link}");
                        app.attach_room(handle, "Your room".into(), cx);
                        app.copy_invite_link(cx);
                        app.show_toast(
                            "Invite link copied. Send it to whoever you want in the room.",
                            cx,
                        );
                        app.ring_pending();
                    }
                    Err(e) => app.join_error = Some(format!("Couldn't start a room: {e:#}")),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    /// Join the room in the join box.
    pub fn join_from_input(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.join_input.read(cx).value().to_string();
        match Ticket::parse(&text) {
            Ok(ticket) => {
                self.join_input
                    .update(cx, |s, cx| s.set_value("", window, cx));
                self.join(ticket, cx);
            }
            Err(e) => {
                self.join_error = Some(if text.trim().is_empty() {
                    "Paste the link someone sent you first.".into()
                } else {
                    e.to_string()
                });
                cx.notify();
            }
        }
    }

    pub fn join(&mut self, ticket: Ticket, cx: &mut Context<Self>) {
        self.join_with(ticket, None, "Room".into(), cx);
    }

    /// Join a room; with `via`, dial those members first (rejoining a room
    /// whose creator may have left).
    pub fn join_with(
        &mut self,
        ticket: Ticket,
        via: Option<Vec<glance_proto::EndpointAddr>>,
        name: String,
        cx: &mut Context<Self>,
    ) {
        let Some(net) = self.net.clone() else {
            return;
        };
        if self.room.is_some() {
            self.leave_room(cx);
        }
        self.busy = true;
        self.join_error = None;
        let state = member_state(&self.self_participant());
        let fut = match via {
            Some(members) => {
                futures_join(net.join_room_via(ticket, members, wire_profile(&self.me), state))
            }
            None => futures_join(net.join_room(ticket, wire_profile(&self.me), state)),
        };
        cx.spawn(async move |this, cx| {
            let result = fut.await;
            this.update(cx, |app, cx| {
                app.busy = false;
                match result {
                    Ok(handle) => {
                        app.link_copied = false;
                        app.attach_room(handle, name, cx);
                    }
                    Err(e) => app.join_error = Some(format!("Couldn't join: {e:#}")),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    pub fn attach_room(&mut self, handle: RoomHandle, name: String, cx: &mut Context<Self>) {
        let events = handle.events.clone();
        let memory = crate::recent::RoomMemory::new(&handle.ticket);
        let task = cx.spawn(async move |this, cx| {
            while let Ok(ev) = events.recv_async().await {
                let closed = matches!(ev, RoomEvent::Closed);
                if this
                    .update(cx, |app, cx| app.on_room_event(ev, cx))
                    .is_err()
                    || closed
                {
                    break;
                }
            }
        });
        let started = Instant::now();
        self.room = Some(Room {
            name,
            link: handle.link(),
            participants: vec![self.self_participant()],
            chat: Vec::new(),
            chat_open: true,
            focused: None,
            filmstrip_hidden: false,
            watching: HashMap::new(),
            started,
            handle: Some(handle),
            _events: Some(task),
            _clock: Some(Self::call_clock(started, cx)),
            broadcast: None,
            _broadcast_task: None,
            audio: None,
            _voice_task: None,
            menu: None,
            popouts: Default::default(),
            cameras: Default::default(),
            camera_broadcast: None,
            stream_audio: None,
            live_settings: None,
            _camera_task: None,
            files: Default::default(),
            history_names: Default::default(),
            met: Default::default(),
            memory,
        });
        self.screen = Screen::Room;
        self.link_revealed = false;
        self.load_saved_chat();
        self.remember_room(false);
        self.start_voice(cx);
        self.push_presence();
        self.refresh_contacts();
    }

    fn on_room_event(&mut self, ev: RoomEvent, cx: &mut Context<Self>) {
        // Someone we're ringing joined (maybe from the link): stop ringing.
        if let RoomEvent::MemberJoined { id, .. } = &ev {
            self.stop_ringing(peer_id(*id));
        }
        let membership = matches!(
            ev,
            RoomEvent::MemberJoined { .. } | RoomEvent::MemberLeft { .. }
        );
        // Files someone can serve now (history, a sender back, a new
        // provider): small images download for their inline preview.
        let files = matches!(
            ev,
            RoomEvent::MemberJoined { .. } | RoomEvent::History(_) | RoomEvent::FileProvider { .. }
        );
        self.apply_room_event(ev, cx);
        if files {
            self.auto_preview_images(cx);
        }
        // The ringback plays only while we're alone in the room.
        if membership {
            self.sync_ringback();
        }
    }

    fn apply_room_event(&mut self, ev: RoomEvent, cx: &mut Context<Self>) {
        let joined = matches!(ev, RoomEvent::MemberJoined { .. });
        let chatty = matches!(
            ev,
            RoomEvent::Chat { .. } | RoomEvent::History(_) | RoomEvent::FileOffered { .. }
        );
        let sounds = self.settings.sounds;
        let Some(room) = self.room.as_mut() else {
            return;
        };
        let chime = |room: &Room, s: glance_media::audio::sounds::Sound| {
            if sounds && let Some(engine) = &room.audio {
                engine.play(s);
            }
        };
        let system = |room: &mut Room, author: PeerId, text: String| {
            let id = room.chat.last().map_or(1, |m| m.id + 1);
            room.chat.push(ChatMessage {
                id,
                author,
                sent_at: now_ms(),
                body: ChatBody::System(text),
            });
        };
        match ev {
            RoomEvent::MemberJoined {
                id: endpoint,
                profile,
                state,
                addr,
                existing,
            } => {
                let id = peer_id(endpoint);
                room.memory.addrs.insert(id, addr);
                // They can serve the files they offered before (saved chat).
                crate::recent::author_is_back(room, id, endpoint);
                if room.participants.iter().any(|p| p.profile.id == id) {
                    return;
                }
                let mut p = Participant {
                    profile: Profile {
                        id,
                        name: profile.name.clone(),
                        color: profile.color,
                    },
                    muted: false,
                    deafened: false,
                    speaking: false,
                    camera: false,
                    stream: None,
                    connection: None,
                    is_contact: false,
                };
                apply_state(&mut p, state);
                p.is_contact = self.book.is_mutual(&id.0);
                room.met.insert(id, p.profile.clone());
                room.participants.push(p);
                // Their saved volume/mute (the engine started without them).
                if let Some(engine) = &room.audio {
                    crate::voice::apply_peer_audio(engine, id, self.settings.peer_audio.get(id));
                }
                if room.name == "Room" && room.participants.len() == 2 {
                    room.name = format!("{}'s room", profile.name);
                }
                // Only newcomers get a "joined" line and chime, not the
                // people who were already here when we arrived.
                if !existing {
                    system(room, id, format!("{} joined", profile.name));
                    chime(room, glance_media::audio::sounds::Sound::Join);
                }
            }
            RoomEvent::MemberUpdated { id, state } => {
                let id = peer_id(id);
                if let Some(p) = room.participants.iter_mut().find(|p| p.profile.id == id) {
                    let was_live = p.stream.is_some();
                    apply_state(p, state);
                    let name = p.profile.name.clone();
                    match (was_live, p.stream.is_some()) {
                        (false, true) => system(room, id, format!("{name} started streaming")),
                        (true, false) => {
                            system(room, id, format!("{name} stopped streaming"));
                            room.watching.remove(&id);
                            if room.focused == Some(id) {
                                room.focused = None;
                            }
                        }
                        _ => {}
                    }
                }
            }
            RoomEvent::MemberProfile { id, profile } => {
                let id = peer_id(id);
                if let Some(p) = room.participants.iter_mut().find(|p| p.profile.id == id) {
                    p.profile.name = profile.name;
                    p.profile.color = profile.color;
                }
            }
            RoomEvent::MemberLeft { id } => {
                let id = peer_id(id);
                if let Some(ix) = room.participants.iter().position(|p| p.profile.id == id) {
                    if let Some(engine) = &room.audio {
                        engine.remove_peer(id.0);
                    }
                    let p = room.participants.remove(ix);
                    room.watching.remove(&id);
                    if room.focused == Some(id) {
                        room.focused = None;
                    }
                    system(room, id, format!("{} left", p.profile.name));
                    chime(room, glance_media::audio::sounds::Sound::Leave);
                }
            }
            RoomEvent::Chat {
                from,
                sent_at,
                text,
            } => {
                let id = room.chat.last().map_or(1, |m| m.id + 1);
                room.chat.push(ChatMessage {
                    id,
                    author: peer_id(from),
                    sent_at,
                    body: ChatBody::Text(text),
                });
            }
            RoomEvent::Connection {
                id,
                relayed,
                rtt_ms,
            } => {
                let id = peer_id(id);
                if let Some(p) = room.participants.iter_mut().find(|p| p.profile.id == id) {
                    p.connection = Some(ConnectionInfo {
                        route: if relayed {
                            Route::Relayed
                        } else {
                            Route::Direct
                        },
                        rtt_ms,
                    });
                }
            }
            RoomEvent::Video { from, track, feed } => {
                self.on_video_feed(peer_id(from), track, feed, cx);
            }
            RoomEvent::History(items) => self.on_history(items),
            RoomEvent::FileOffered {
                from,
                sent_at,
                file,
            } => {
                self.on_file_offered(from, sent_at, file, cx);
            }
            RoomEvent::FileProvider { from, hash } => self.on_file_provider(from, hash),
            RoomEvent::Watchers { track, count } => {
                if let Some(b) = room.control(track) {
                    b.watchers
                        .store(count, std::sync::atomic::Ordering::Relaxed);
                }
            }
            RoomEvent::Congested(track) => {
                if let Some(b) = room.control(track) {
                    b.congestion
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                }
            }
            RoomEvent::KeyframeNeeded(track) => {
                if let Some(b) = room.control(track) {
                    b.keyframe.store(true, std::sync::atomic::Ordering::Relaxed);
                }
            }
            RoomEvent::Problem(msg) => {
                let me = self.me.id;
                system(room, me, msg);
            }
            RoomEvent::Closed => {}
        }
        if joined {
            self.remember_room(false);
        }
        if chatty {
            self.persist_chat();
        }
        // Participant count changed or not: cheap to re-send.
        self.push_presence();
        self.sync_cameras(cx);
        // Testing aid: GLANCE_AUTOWATCH=1 watches every stream automatically.
        if std::env::var_os("GLANCE_AUTOWATCH").is_some() {
            let me = self.me.id;
            let to_watch: Vec<PeerId> = self
                .room
                .as_ref()
                .map(|r| {
                    r.participants
                        .iter()
                        .filter(|p| {
                            p.profile.id != me
                                && p.stream.is_some()
                                && !r.watching.contains_key(&p.profile.id)
                        })
                        .map(|p| p.profile.id)
                        .collect()
                })
                .unwrap_or_default();
            for id in to_watch {
                self.watch(id, cx);
            }
        }
        cx.notify();
    }

    /// Send your current state to the room.
    pub fn push_state(&mut self) {
        let me = self.me.id;
        if let Some(room) = &self.room
            && let Some(handle) = &room.handle
            && let Some(p) = room.participants.iter().find(|p| p.profile.id == me)
        {
            handle.send(Command::SetState(member_state(p)));
        }
    }

    pub fn on_join_input(
        &mut self,
        _: &Entity<InputState>,
        event: &InputEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let InputEvent::PressEnter { .. } = event {
            self.join_from_input(window, cx);
        }
    }

    pub fn on_name_input(
        &mut self,
        _: &Entity<InputState>,
        event: &InputEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let InputEvent::PressEnter { .. } = event {
            self.save_name(cx);
        }
    }

    /// Save the first-run name (also used by the profile settings later).
    pub fn save_name(&mut self, cx: &mut Context<Self>) {
        let name = self.name_input.read(cx).value().trim().to_string();
        if name.is_empty() {
            return;
        }
        let name: String = name.chars().take(32).collect();
        self.me.name = name.clone();
        if let Some(net) = &self.net {
            let saved = SavedProfile {
                name,
                color: self.me.color,
            };
            if let Err(e) = save_profile(&net.data_dir, &saved) {
                self.join_error = Some(format!("Couldn't save your name: {e:#}"));
            }
        }
        if let Some(room) = &self.room
            && let Some(handle) = &room.handle
        {
            handle.send(Command::SetProfile(wire_profile(&self.me)));
        }
        self.needs_profile = false;
        cx.notify();
    }
}

/// Box the two join futures into one type.
fn futures_join(
    fut: impl std::future::Future<Output = anyhow::Result<RoomHandle>> + 'static,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<RoomHandle>>>> {
    Box::pin(fut)
}
