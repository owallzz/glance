//! Contacts in the app: the contact book, presence, ringing contacts into
//! calls, incoming calls, missed calls, and the "add contact" prompts.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use glance_core::contacts::{ContactBook, Outcome, StoredCall, StoredState};
use glance_core::{PeerId, Presence, Profile};
use glance_net::EndpointId;
use glance_net::contacts::{ContactCommand, ContactEvent, Relation};
use glance_proto::{PresenceInfo, Ticket, WirePresence, WireProfile};
use gpui_kit::*;

use crate::app::{GlanceApp, Screen, now_ms};
use crate::net::peer_id;

/// How long an unanswered call rings.
pub const RING_TIMEOUT: Duration = Duration::from_secs(30);

/// A call ringing on this machine.
pub struct IncomingCall {
    pub from: PeerId,
    pub call_id: u64,
    pub link: String,
    pub members: Vec<String>,
    pub since: Instant,
    pub _ringtone: Option<glance_media::audio::ringtone::Ringtone>,
}

/// Contact state that isn't persisted: who's online and their status.
#[derive(Default)]
pub struct Live {
    pub presence: HashMap<PeerId, PresenceInfo>,
    pub online: HashSet<PeerId>,
    /// Calls we're ringing out: call id -> who.
    pub ringing_out: HashMap<u64, Vec<PeerId>>,
    /// Ring these contacts once the room we're creating is ready.
    pub ring_when_ready: Vec<PeerId>,
    /// What we last told contacts (to skip duplicate updates).
    pub last_presence: Option<Option<PresenceInfo>>,
}

fn eid(p: PeerId) -> Option<EndpointId> {
    EndpointId::from_bytes(&p.0).ok()
}

impl GlanceApp {
    fn book_path(&self) -> Option<std::path::PathBuf> {
        self.net.as_ref().map(|n| n.data_dir.join("contacts.json"))
    }

    /// Load the contact book and start listening for contact events.
    pub fn start_contacts(&mut self, cx: &mut Context<Self>) {
        let Some(net) = self.net.clone() else { return };
        if let Some(path) = self.book_path() {
            self.book = ContactBook::load(&path);
        }
        let handle = net.contacts();
        handle.send(ContactCommand::SetProfile(WireProfile {
            name: self.me.name.clone(),
            color: self.me.color,
        }));
        self.push_relations();
        self.push_presence();
        handle.send(ContactCommand::ConnectAll);
        self.refresh_contacts();
        let events = handle.events.clone();
        self._contacts_task = Some(cx.spawn(async move |this, cx| {
            while let Ok(ev) = events.recv_async().await {
                if this
                    .update(cx, |app, cx| app.on_contact_event(ev, cx))
                    .is_err()
                {
                    break;
                }
            }
        }));
        // Time out unanswered incoming calls.
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_secs(1)).await;
                let ok = this.update(cx, |app, cx| app.expire_ring(cx)).is_ok();
                if !ok {
                    break;
                }
            }
        })
        .detach();
    }

    fn contacts_cmd(&self, cmd: ContactCommand) {
        if let Some(net) = &self.net {
            net.contacts().send(cmd);
        }
    }

    fn save_book(&mut self) {
        if let Some(path) = self.book_path()
            && let Err(e) = self.book.save(&path)
        {
            tracing::warn!("saving contacts failed: {e}");
        }
        self.refresh_contacts();
        self.push_relations();
    }

    /// Rebuild the contact list the UI renders.
    pub fn refresh_contacts(&mut self) {
        let live = &self.live;
        self.contacts = self.book.view(|id| {
            let pid = PeerId(*id);
            match live.presence.get(&pid) {
                Some(info) if live.online.contains(&pid) => (
                    match info.presence {
                        WirePresence::Online => Presence::Online,
                        WirePresence::Idle => Presence::Idle,
                        WirePresence::DoNotDisturb => Presence::DoNotDisturb,
                    },
                    info.in_call,
                    info.joinable,
                ),
                _ if live.online.contains(&pid) => (Presence::Online, None, false),
                _ => (Presence::Offline, None, false),
            }
        });
        self.calls = self.book.call_view();
        // Keep the room's "is contact" flags current.
        let book = &self.book;
        if let Some(room) = self.room.as_mut() {
            for p in room.participants.iter_mut() {
                p.is_contact = book.is_mutual(&p.profile.id.0);
            }
        }
    }

    fn push_relations(&self) {
        let map: HashMap<EndpointId, Relation> = self
            .book
            .contacts
            .iter()
            .filter_map(|c| {
                let id = EndpointId::from_bytes(&c.id).ok()?;
                Some((
                    id,
                    match c.state {
                        StoredState::Mutual => Relation::Mutual,
                        StoredState::Incoming => Relation::Incoming,
                        StoredState::Outgoing => Relation::Outgoing,
                        StoredState::Blocked => Relation::Blocked,
                    },
                ))
            })
            .collect();
        self.contacts_cmd(ContactCommand::SetRelations(map));
    }

    /// Tell contacts our status (or nothing when invisible).
    pub fn push_presence(&mut self) {
        let info = match self.presence {
            Presence::Invisible | Presence::Offline => None,
            p => Some(PresenceInfo {
                presence: match p {
                    Presence::Idle => WirePresence::Idle,
                    Presence::DoNotDisturb => WirePresence::DoNotDisturb,
                    _ => WirePresence::Online,
                },
                in_call: self
                    .room
                    .as_ref()
                    .filter(|r| r.handle.is_some())
                    .map(|r| r.participants.len().min(255) as u8),
                joinable: self.settings.let_contacts_join
                    && self.room.as_ref().is_some_and(|r| r.handle.is_some()),
            }),
        };
        // Only send when it changed (this is called on every room event).
        if self.live.last_presence.as_ref() != Some(&info) {
            self.live.last_presence = Some(info.clone());
            self.contacts_cmd(ContactCommand::SetPresence(info));
        }
    }

    pub fn set_presence(&mut self, presence: Presence, cx: &mut Context<Self>) {
        self.presence = presence;
        self.push_presence();
        cx.notify();
    }

    // ---- actions from the UI ----

    /// "Add contact" on someone from a room or the call-ended list.
    pub fn add_contact(&mut self, profile: Profile, cx: &mut Context<Self>) {
        let Some(id) = eid(profile.id) else { return };
        let outcome = self
            .book
            .request_sent(profile.id.0, &profile.name, profile.color, now_ms());
        self.save_book();
        if outcome == Outcome::BecameMutual {
            self.contacts_cmd(ContactCommand::Accept(id));
        } else {
            self.contacts_cmd(ContactCommand::SendRequest(id));
        }
        cx.notify();
    }

    pub fn accept_contact(&mut self, peer: PeerId, cx: &mut Context<Self>) {
        let Some(id) = eid(peer) else { return };
        let (name, color) = self
            .book
            .get(&peer.0)
            .map(|c| (c.name.clone(), c.color))
            .unwrap_or_default();
        self.book.accept(peer.0, &name, color, now_ms());
        self.save_book();
        self.contacts_cmd(ContactCommand::Accept(id));
        cx.notify();
    }

    /// Ignore a request, cancel ours, or remove a contact.
    pub fn remove_contact(&mut self, peer: PeerId, cx: &mut Context<Self>) {
        self.book.remove(&peer.0);
        self.save_book();
        if let Some(id) = eid(peer) {
            self.contacts_cmd(ContactCommand::Remove(id));
        }
        cx.notify();
    }

    pub fn block_contact(&mut self, peer: PeerId, cx: &mut Context<Self>) {
        let (name, color) = self
            .profile_of(peer)
            .map(|p| (p.name, p.color))
            .unwrap_or_else(|| (peer.short(), 0x80848e));
        self.book.block(peer.0, &name, color, now_ms());
        self.save_book();
        cx.notify();
    }

    pub fn unblock_contact(&mut self, peer: PeerId, cx: &mut Context<Self>) {
        self.book.unblock(&peer.0);
        self.save_book();
        cx.notify();
    }

    pub fn toggle_favorite(&mut self, peer: PeerId, cx: &mut Context<Self>) {
        let fav = self.book.get(&peer.0).is_some_and(|c| c.favorite);
        self.book.set_favorite(&peer.0, !fav);
        self.save_book();
        cx.notify();
    }

    /// Call contacts: ring them into the current room, or start one.
    pub fn call_contacts(&mut self, peers: Vec<PeerId>, cx: &mut Context<Self>) {
        if self.room.as_ref().is_some_and(|r| r.handle.is_some()) {
            for p in peers {
                self.ring(p);
            }
            self.screen = Screen::Room;
        } else {
            self.live.ring_when_ready = peers;
            self.start_room(cx);
        }
        cx.notify();
    }

    /// Called when a room we created is ready.
    pub fn ring_pending(&mut self) {
        for p in std::mem::take(&mut self.live.ring_when_ready) {
            self.ring(p);
        }
    }

    fn ring(&mut self, peer: PeerId) {
        let Some(id) = eid(peer) else { return };
        let Some(room) = self.room.as_ref() else {
            return;
        };
        let call_id: u64 = rand::random();
        let members = room
            .participants
            .iter()
            .map(|p| p.profile.name.clone())
            .collect();
        self.contacts_cmd(ContactCommand::Ring {
            to: id,
            call_id,
            link: room.link.clone(),
            members,
        });
        self.live.ringing_out.insert(call_id, vec![peer]);
        let name = self
            .profile_of(peer)
            .map(|p| p.name)
            .unwrap_or_else(|| "them".into());
        self.system_message(format!("Calling {name}…"));
    }

    /// Join button on a contact who lets contacts join their call.
    pub fn request_join(&mut self, peer: PeerId) {
        if let Some(id) = eid(peer) {
            self.contacts_cmd(ContactCommand::JoinRequest(id));
        }
    }

    pub fn answer_call(&mut self, accept: bool, cx: &mut Context<Self>) {
        let Some(call) = self.incoming_call.take() else {
            return;
        };
        if let Some(id) = eid(call.from) {
            self.contacts_cmd(ContactCommand::Answer {
                to: id,
                call_id: call.call_id,
                accepted: accept,
            });
        }
        if accept {
            match Ticket::parse(&call.link) {
                Ok(t) => self.join(t, cx),
                Err(e) => self.join_error = Some(format!("Couldn't join the call: {e}")),
            }
        }
        cx.notify();
    }

    fn expire_ring(&mut self, cx: &mut Context<Self>) {
        if self
            .incoming_call
            .as_ref()
            .is_some_and(|c| c.since.elapsed() > RING_TIMEOUT)
        {
            let call = self.incoming_call.take().expect("checked");
            self.record_missed(call.from);
            cx.notify();
        }
    }

    fn record_missed(&mut self, from: PeerId) {
        self.book.record_call(StoredCall {
            with: vec![from.0],
            started_at: now_ms(),
            duration_secs: 0,
            missed: true,
        });
        self.save_book();
    }

    /// Remember a finished call and offer to add new people as contacts.
    pub fn record_call_end(&mut self) {
        let me = self.me.id;
        let Some(room) = self.room.as_ref() else {
            return;
        };
        if room.handle.is_none() {
            return;
        }
        let others: Vec<Profile> = room
            .participants
            .iter()
            .filter(|p| p.profile.id != me)
            .map(|p| p.profile.clone())
            .collect();
        let met: Vec<Profile> = room.met.values().filter(|p| p.id != me).cloned().collect();
        let started = room.started;
        if met.is_empty() && others.is_empty() {
            return;
        }
        let with: Vec<[u8; 32]> = met.iter().map(|p| p.id.0).collect();
        self.book.record_call(StoredCall {
            with,
            started_at: now_ms().saturating_sub(started.elapsed().as_millis() as u64),
            duration_secs: started.elapsed().as_secs(),
            missed: false,
        });
        self.last_call = met
            .into_iter()
            .filter(|p| self.book.state(&p.id.0).is_none())
            .collect();
        self.save_book();
    }

    fn on_contact_event(&mut self, ev: ContactEvent, cx: &mut Context<Self>) {
        match ev {
            ContactEvent::Online {
                id,
                profile,
                presence,
            } => {
                let pid = peer_id(id);
                self.live.online.insert(pid);
                if let Some(p) = presence {
                    self.live.presence.insert(pid, p);
                }
                self.book
                    .update_profile(&pid.0, &profile.name, profile.color);
            }
            ContactEvent::Offline(id) => {
                let pid = peer_id(id);
                self.live.online.remove(&pid);
                self.live.presence.remove(&pid);
            }
            ContactEvent::Presence { id, info } => {
                self.live.presence.insert(peer_id(id), info);
                self.live.online.insert(peer_id(id));
            }
            ContactEvent::RequestReceived { id, profile } => {
                let pid = peer_id(id);
                if self
                    .book
                    .request_received(pid.0, &profile.name, profile.color, now_ms())
                    == Outcome::BecameMutual
                {
                    self.contacts_cmd(ContactCommand::Accept(id));
                } else {
                    notify(
                        &format!("{} wants to add you as a contact", profile.name),
                        "Open glance to accept.",
                    );
                }
                self.save_book();
            }
            ContactEvent::Accepted { id, profile } => {
                self.book
                    .accept(peer_id(id).0, &profile.name, profile.color, now_ms());
                self.save_book();
                self.push_presence();
                self.contacts_cmd(ContactCommand::ConnectAll);
            }
            ContactEvent::Removed(id) => {
                let pid = peer_id(id);
                if self.book.state(&pid.0) != Some(StoredState::Blocked) {
                    self.book.remove(&pid.0);
                    self.save_book();
                }
            }
            ContactEvent::Ring {
                from,
                call_id,
                link,
                members,
            } => self.on_ring(peer_id(from), call_id, link, members),
            ContactEvent::RingCancelled { from, call_id } => {
                if self
                    .incoming_call
                    .as_ref()
                    .is_some_and(|c| c.call_id == call_id && c.from == peer_id(from))
                {
                    self.incoming_call = None;
                    self.record_missed(peer_id(from));
                }
            }
            ContactEvent::RingAnswered {
                from,
                call_id,
                accepted,
            } => {
                self.live.ringing_out.remove(&call_id);
                if !accepted {
                    let name = self
                        .profile_of(peer_id(from))
                        .map(|p| p.name)
                        .unwrap_or_default();
                    self.system_message(format!("{name} can't join right now."));
                }
            }
            ContactEvent::JoinRequested(id) => {
                let link = self
                    .room
                    .as_ref()
                    .filter(|r| r.handle.is_some() && self.settings.let_contacts_join)
                    .map(|r| r.link.clone());
                self.contacts_cmd(ContactCommand::JoinReply { to: id, link });
            }
            ContactEvent::JoinTicket { from, link } => match link.as_deref().map(Ticket::parse) {
                Some(Ok(t)) => self.join(t, cx),
                _ => {
                    let name = self
                        .profile_of(peer_id(from))
                        .map(|p| p.name)
                        .unwrap_or_default();
                    self.join_error = Some(format!("{name}'s call isn't open to join right now."));
                }
            },
            ContactEvent::Undeliverable { to, what } => {
                let name = self
                    .profile_of(peer_id(to))
                    .map(|p| p.name)
                    .unwrap_or_else(|| "them".into());
                let msg = format!("Couldn't reach {name} for the {what}. They may be offline.");
                if self.room.is_some() {
                    self.system_message(msg);
                } else {
                    self.join_error = Some(msg);
                }
            }
        }
        self.refresh_contacts();
        cx.notify();
    }

    fn on_ring(&mut self, from: PeerId, call_id: u64, link: String, members: Vec<String>) {
        // Already in that room, or busy with another ring.
        let same_room = self.room.as_ref().is_some_and(|r| r.link == link);
        if same_room || self.incoming_call.is_some() {
            return;
        }
        if self.presence == Presence::DoNotDisturb {
            self.record_missed(from);
            return;
        }
        let name = self
            .profile_of(from)
            .map(|p| p.name)
            .unwrap_or_else(|| "A contact".into());
        notify(
            &format!("{name} is calling"),
            "Open glance to join the call.",
        );
        let ringtone =
            glance_media::audio::ringtone::Ringtone::start(self.settings.output_device.clone())
                .ok();
        self.incoming_call = Some(IncomingCall {
            from,
            call_id,
            link,
            members,
            since: Instant::now(),
            _ringtone: ringtone,
        });
    }
}

/// Show an OS notification (best effort).
pub fn notify(summary: &str, body: &str) {
    let summary = summary.to_string();
    let body = body.to_string();
    std::thread::spawn(move || {
        let _ = notify_rust::Notification::new()
            .appname("glance")
            .summary(&summary)
            .body(&body)
            .show();
    });
}
