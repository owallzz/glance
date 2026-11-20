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
            muted: self.muted,
            deafened: self.deafened,
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
                        cx.write_to_clipboard(ClipboardItem::new_string(link));
                        app.link_copied = true;
                        app.attach_room(handle, "Your room".into(), cx);
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
        let Some(net) = self.net.clone() else {
            return;
        };
        if self.room.is_some() {
            self.leave_room(cx);
        }
        self.busy = true;
        self.join_error = None;
        let state = member_state(&self.self_participant());
        let fut = net.join_room(ticket, wire_profile(&self.me), state);
        cx.spawn(async move |this, cx| {
            let result = fut.await;
            this.update(cx, |app, cx| {
                app.busy = false;
                match result {
                    Ok(handle) => {
                        app.link_copied = false;
                        app.attach_room(handle, "Room".into(), cx);
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

    fn attach_room(&mut self, handle: RoomHandle, name: String, cx: &mut Context<Self>) {
        let events = handle.events.clone();
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
        self.room = Some(Room {
            name,
            link: handle.link(),
            participants: vec![self.self_participant()],
            chat: Vec::new(),
            chat_open: true,
            focused: None,
            watching: HashMap::new(),
            started: Instant::now(),
            handle: Some(handle),
            _events: Some(task),
            broadcast: None,
            _broadcast_task: None,
        });
        self.screen = Screen::Room;
    }

    fn on_room_event(&mut self, ev: RoomEvent, cx: &mut Context<Self>) {
        let Some(room) = self.room.as_mut() else {
            return;
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
            RoomEvent::MemberJoined { id, profile, state } => {
                let id = peer_id(id);
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
                room.participants.push(p);
                if room.name == "Room" && room.participants.len() == 2 {
                    room.name = format!("{}'s room", profile.name);
                }
                system(room, id, format!("{} joined", profile.name));
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
                    let p = room.participants.remove(ix);
                    room.watching.remove(&id);
                    if room.focused == Some(id) {
                        room.focused = None;
                    }
                    system(room, id, format!("{} left", p.profile.name));
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
            RoomEvent::Video { from, feed } => {
                self.on_video_feed(peer_id(from), feed, cx);
            }
            RoomEvent::Watchers(n) => {
                if let Some(b) = &room.broadcast {
                    b.watchers.store(n, std::sync::atomic::Ordering::Relaxed);
                }
            }
            RoomEvent::KeyframeNeeded => {
                if let Some(b) = &room.broadcast {
                    b.keyframe.store(true, std::sync::atomic::Ordering::Relaxed);
                }
            }
            RoomEvent::Problem(msg) => {
                let me = self.me.id;
                system(room, me, msg);
            }
            RoomEvent::Closed => {}
        }
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
