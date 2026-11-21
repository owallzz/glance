//! Recent rooms and saved room chat.
//!
//! Rooms you were in are remembered in rooms.json (link, name, who was in
//! it and where they were reachable, when you left); each room's chat is
//! saved in `chats/<room id>.json`. The home screen asks remembered members
//! whether a room is still going only while it shows the list (never in the
//! background), and Rejoin dials whoever is still there, so a room outlives
//! the person who started it.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use glance_core::rooms::{
    self, ChatLog, RecentMember, RecentRoom, RecentRooms, SavedBody, SavedMessage,
};
use glance_core::{ChatBody, ChatMessage, FileOffer, PeerId, Profile, Transfer};
use glance_net::{EndpointId, RoomStatus};
use glance_proto::{EndpointAddr, HistoryBody, HistoryItem, MemberInfo, Ticket};
use gpui_kit::assets::IconName;
use gpui_kit::component::Sizable;
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::app::{GlanceApp, Room, now_ms};
use crate::theme::Palette;
use crate::ui::{icon, icon_button, section_label};

/// While the home screen is up, check the rooms again at most this often.
const PROBE_EVERY: Duration = Duration::from_secs(30);
/// Rooms shown (and checked) on the home screen.
pub const SHOWN: usize = 6;

/// Is a remembered room still going?
#[derive(Clone, Debug, PartialEq)]
pub enum Liveness {
    Checking,
    /// Someone in it answered; these are the members.
    Alive(Vec<MemberInfo>),
    /// Everyone we reached has left it.
    Empty,
    /// Nobody answered (offline, or an older glance).
    Unknown,
}

enum WriteOp {
    Write(PathBuf, Vec<u8>),
    Remove(PathBuf),
}

/// Recent rooms and what checking them found.
#[derive(Default)]
pub struct Recent {
    pub rooms: RecentRooms,
    pub status: HashMap<String, Liveness>,
    probed_at: Option<Instant>,
    /// File writes go through one background task, in order (the latest
    /// content always lands last).
    writer: Option<flume::Sender<WriteOp>>,
}

impl Recent {
    /// Check the rooms again next time the home screen shows.
    pub fn stale(&mut self) {
        self.probed_at = None;
    }

    fn write(&self, op: WriteOp) {
        if let Some(w) = &self.writer {
            let _ = w.send(op);
        }
    }
}

/// Per-room bookkeeping for remembering it.
#[derive(Default)]
pub struct RoomMemory {
    /// Hex room id; empty for the demo room (nothing is saved).
    pub room_id: String,
    /// The link we came in with (its secret lets us rejoin).
    pub ticket_link: String,
    /// Where members were reachable.
    pub addrs: HashMap<PeerId, EndpointAddr>,
    /// How many messages at the top of the chat came from the saved log
    /// (newer history from peers goes below them, under a divider).
    pub saved_len: usize,
    pub joined_at: u64,
}

impl RoomMemory {
    pub fn new(ticket: &Ticket) -> Self {
        Self {
            room_id: room_hex(ticket),
            ticket_link: ticket.to_link(),
            addrs: HashMap::new(),
            saved_len: 0,
            joined_at: now_ms(),
        }
    }
}

pub fn room_hex(ticket: &Ticket) -> String {
    ticket.room.0.iter().map(|b| format!("{b:02x}")).collect()
}

/// Who to ask about a remembered room: members who were still there when
/// we left first.
fn candidates(room: &RecentRoom) -> Vec<EndpointAddr> {
    let mut members: Vec<&RecentMember> = room.members.iter().collect();
    members.sort_by_key(|m| !m.present);
    members
        .into_iter()
        .filter_map(|m| {
            m.addr
                .as_deref()
                .and_then(|a| serde_json::from_str::<EndpointAddr>(a).ok())
                .filter(|a| a.id.as_bytes() == &m.id)
                .or_else(|| EndpointId::from_bytes(&m.id).ok().map(EndpointAddr::new))
        })
        .collect()
}

/// `who` can serve this file now: remember that, and stop showing it as
/// unavailable.
pub fn add_provider(room: &mut Room, hash: [u8; 32], who: EndpointId) {
    let e = room.files.entry(hash).or_default();
    if !e.providers.contains(&who) {
        e.providers.push(who);
    }
    for m in room.chat.iter_mut() {
        if let ChatBody::File(f) = &mut m.body
            && f.hash == hash
            && f.transfer == Transfer::Unavailable
        {
            f.transfer = Transfer::Available;
        }
    }
}

/// Someone joined: files they offered earlier (saved chat) are available
/// again.
pub fn author_is_back(room: &mut Room, id: PeerId, endpoint: EndpointId) {
    let hashes: Vec<[u8; 32]> = room
        .chat
        .iter()
        .filter(|m| m.author == id)
        .filter_map(|m| match &m.body {
            ChatBody::File(f) if f.transfer == Transfer::Unavailable => Some(f.hash),
            _ => None,
        })
        .collect();
    for hash in hashes {
        add_provider(room, hash, endpoint);
    }
}

fn saved_from_history(item: &HistoryItem) -> SavedMessage {
    SavedMessage {
        author: *item.author.as_bytes(),
        author_name: item.author_name.clone(),
        author_color: item.author_color,
        sent_at: item.sent_at,
        body: match &item.body {
            HistoryBody::Text(t) => SavedBody::Text(t.clone()),
            HistoryBody::File(f) => SavedBody::File {
                hash: f.hash,
                name: f.name.clone(),
                size: f.size,
                mime: f.mime.clone(),
            },
        },
    }
}

/// A saved message as a chat line. Files show as `transfer` unless we sent
/// them.
fn chat_line(m: SavedMessage, me: PeerId, transfer: Transfer) -> ChatMessage {
    let author = PeerId(m.author);
    let body = match m.body {
        SavedBody::Text(t) => ChatBody::Text(t),
        SavedBody::File {
            hash,
            name,
            size,
            mime,
        } => ChatBody::File(FileOffer {
            hash,
            name,
            size,
            mime,
            transfer: if author == me {
                Transfer::Sent
            } else {
                transfer
            },
        }),
    };
    ChatMessage {
        id: 0,
        author,
        sent_at: m.sent_at,
        body,
    }
}

fn renumber(chat: &mut [ChatMessage]) {
    for (i, m) in chat.iter_mut().enumerate() {
        m.id = i as u64 + 1;
    }
}

impl GlanceApp {
    fn rooms_path(&self) -> Option<PathBuf> {
        Some(self.net.as_ref()?.data_dir.join("rooms.json"))
    }

    fn chat_file(&self, room_id: &str) -> Option<PathBuf> {
        Some(rooms::chat_path(&self.net.as_ref()?.data_dir, room_id))
    }

    /// Read rooms.json, forget old rooms, and start the file writer.
    pub fn load_recent(&mut self, cx: &mut Context<Self>) {
        let Some(path) = self.rooms_path() else {
            return;
        };
        let (tx, rx) = flume::unbounded::<WriteOp>();
        cx.background_executor()
            .spawn(async move {
                while let Ok(op) = rx.recv_async().await {
                    let result = match &op {
                        WriteOp::Write(path, bytes) => rooms::write_atomic(path, bytes),
                        WriteOp::Remove(path) => match std::fs::remove_file(path) {
                            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                            r => r,
                        },
                    };
                    if let Err(e) = result {
                        tracing::warn!("saving rooms/chat failed: {e}");
                    }
                }
            })
            .detach();
        self.recent.writer = Some(tx);
        self.recent.rooms = RecentRooms::load(&path);
        let gone = self.recent.rooms.prune(now_ms());
        if !gone.is_empty() {
            for id in gone {
                if let Some(chat) = self.chat_file(&id) {
                    self.recent.write(WriteOp::Remove(chat));
                }
            }
            self.save_recent();
        }
    }

    fn save_recent(&self) {
        let Some(path) = self.rooms_path() else {
            return;
        };
        match serde_json::to_vec_pretty(&self.recent.rooms) {
            Ok(bytes) => self.recent.write(WriteOp::Write(path, bytes)),
            Err(e) => tracing::warn!("encoding rooms.json: {e}"),
        }
    }

    /// Name of a chat author, also for people who already left.
    fn author_label(&self, id: PeerId) -> (String, u32) {
        self.profile_of(id)
            .or_else(|| self.room.as_ref()?.met.get(&id).cloned())
            .map(|p| (p.name, p.color))
            .unwrap_or_else(|| ("Someone".into(), 0x99aab5))
    }

    /// The room's chat (without system lines) in saved form.
    fn saved_chat(&self) -> Vec<SavedMessage> {
        let Some(room) = self.room.as_ref() else {
            return Vec::new();
        };
        room.chat
            .iter()
            .filter_map(|m| {
                let body = match &m.body {
                    ChatBody::Text(t) => SavedBody::Text(t.clone()),
                    ChatBody::File(f) => SavedBody::File {
                        hash: f.hash,
                        name: f.name.clone(),
                        size: f.size,
                        mime: f.mime.clone(),
                    },
                    ChatBody::System(_) => return None,
                };
                let (author_name, author_color) = self.author_label(m.author);
                Some(SavedMessage {
                    author: m.author.0,
                    author_name,
                    author_color,
                    sent_at: m.sent_at,
                    body,
                })
            })
            .collect()
    }

    /// Save the room's chat (written in the background).
    pub fn persist_chat(&self) {
        let Some(room) = self.room.as_ref() else {
            return;
        };
        if room.memory.room_id.is_empty() || room.handle.is_none() {
            return;
        }
        let Some(path) = self.chat_file(&room.memory.room_id) else {
            return;
        };
        let mut log = ChatLog {
            messages: self.saved_chat(),
        };
        if log.messages.is_empty() {
            return;
        }
        log.cap();
        match serde_json::to_vec(&log) {
            Ok(bytes) => self.recent.write(WriteOp::Write(path, bytes)),
            Err(e) => tracing::warn!("encoding chat: {e}"),
        }
    }

    /// Show the room's saved chat (from earlier visits) first.
    pub fn load_saved_chat(&mut self) {
        let me = self.me.id;
        let Some(path) = self
            .room
            .as_ref()
            .filter(|r| !r.memory.room_id.is_empty())
            .and_then(|r| self.chat_file(&r.memory.room_id))
        else {
            return;
        };
        let log = ChatLog::load(&path);
        let Some(room) = self.room.as_mut() else {
            return;
        };
        let mut lines = Vec::with_capacity(log.messages.len());
        for m in log.messages {
            let author = PeerId(m.author);
            if author != me {
                room.history_names
                    .entry(author)
                    .or_insert((m.author_name.clone(), m.author_color));
            }
            if let SavedBody::File { hash, .. } = &m.body {
                room.files.entry(*hash).or_default();
            }
            // Until someone who has a file shows up, it can't be fetched.
            lines.push(chat_line(m, me, Transfer::Unavailable));
        }
        room.memory.saved_len = lines.len();
        lines.append(&mut room.chat);
        renumber(&mut lines);
        room.chat = lines;
    }

    /// Chat and files from before we (re)joined: anything we haven't seen
    /// goes below the saved chat, under "While you were away".
    pub fn on_history(&mut self, items: Vec<HistoryItem>) {
        let me = self.me.id;
        let known = self.saved_chat();
        let Some(room) = self.room.as_mut() else {
            return;
        };
        for item in &items {
            if let HistoryBody::File(f) = &item.body {
                add_provider(room, f.hash, item.author);
            }
        }
        let fresh = rooms::new_messages(&known, items.iter().map(saved_from_history));
        let at = std::mem::take(&mut room.memory.saved_len).min(room.chat.len());
        if fresh.is_empty() {
            return;
        }
        let mut lines = Vec::with_capacity(fresh.len() + 1);
        if at > 0 {
            lines.push(ChatMessage {
                id: 0,
                author: me,
                sent_at: fresh[0].sent_at,
                body: ChatBody::System("While you were away".into()),
            });
        }
        for m in fresh {
            let author = PeerId(m.author);
            if author != me {
                room.history_names
                    .entry(author)
                    .or_insert((m.author_name.clone(), m.author_color));
            }
            lines.push(chat_line(m, me, Transfer::Available));
        }
        room.chat.splice(at..at, lines);
        renumber(&mut room.chat);
    }

    /// Remember the room we're in (on joining, when someone joins, and on
    /// leaving).
    pub fn remember_room(&mut self, left: bool) {
        let me = self.me.id;
        let Some(room) = self.room.as_ref() else {
            return;
        };
        if room.memory.room_id.is_empty() || room.handle.is_none() {
            return;
        }
        let present: HashSet<PeerId> = room
            .participants
            .iter()
            .map(|p| p.profile.id)
            .filter(|id| *id != me)
            .collect();
        let mut people: Vec<&Profile> = room
            .participants
            .iter()
            .map(|p| &p.profile)
            .filter(|p| p.id != me)
            .collect();
        let mut earlier: Vec<&Profile> = room
            .met
            .values()
            .filter(|p| p.id != me && !present.contains(&p.id))
            .collect();
        earlier.sort_by(|a, b| a.name.cmp(&b.name));
        people.extend(earlier);
        let members: Vec<RecentMember> = people
            .iter()
            .map(|p| RecentMember {
                id: p.id.0,
                name: p.name.clone(),
                color: p.color,
                addr: room
                    .memory
                    .addrs
                    .get(&p.id)
                    .and_then(|a| serde_json::to_string(a).ok()),
                present: present.contains(&p.id),
            })
            .collect();
        let previous = self.recent.rooms.get(&room.memory.room_id);
        let name = if people.is_empty() {
            previous
                .map(|r| r.name.clone())
                .unwrap_or_else(|| room.name.clone())
        } else {
            let names: Vec<String> = people.iter().map(|p| p.name.clone()).collect();
            rooms::room_name(&names)
        };
        let entry = RecentRoom {
            room_id: room.memory.room_id.clone(),
            link: room.memory.ticket_link.clone(),
            name,
            members,
            joined_at: room.memory.joined_at,
            left_at: left.then(now_ms),
        };
        self.recent.rooms.upsert(entry);
        let gone = self.recent.rooms.prune(now_ms());
        for id in gone {
            if let Some(chat) = self.chat_file(&id) {
                self.recent.write(WriteOp::Remove(chat));
            }
        }
        self.save_recent();
    }

    /// Called while the home screen shows the list: check the rooms if we
    /// haven't lately.
    pub fn ensure_recent_checked(&mut self, cx: &mut Context<Self>) {
        if self.net.is_none() || self.recent.rooms.rooms.is_empty() {
            return;
        }
        if self
            .recent
            .probed_at
            .is_some_and(|t| t.elapsed() < PROBE_EVERY)
        {
            return;
        }
        self.check_recent(cx);
    }

    /// Ask remembered members of each shown room whether it's still going.
    pub fn check_recent(&mut self, cx: &mut Context<Self>) {
        let Some(net) = self.net.clone() else {
            return;
        };
        self.recent.probed_at = Some(Instant::now());
        let current = self.room.as_ref().map(|r| r.memory.room_id.clone());
        let targets: Vec<(String, Ticket, Vec<EndpointAddr>)> = self
            .recent
            .rooms
            .rooms
            .iter()
            .take(SHOWN)
            .filter(|r| current.as_ref() != Some(&r.room_id))
            .filter_map(|r| {
                Some((
                    r.room_id.clone(),
                    Ticket::parse(&r.link).ok()?,
                    candidates(r),
                ))
            })
            .collect();
        for (id, ticket, candidates) in targets {
            self.recent
                .status
                .entry(id.clone())
                .or_insert(Liveness::Checking);
            let fut = net.probe_room(ticket, candidates);
            cx.spawn(async move |this, cx| {
                let status = fut.await;
                let _ = this.update(cx, |app, cx| {
                    let live = match status {
                        RoomStatus::Alive(members) => Liveness::Alive(members),
                        RoomStatus::Empty => Liveness::Empty,
                        RoomStatus::Unknown => Liveness::Unknown,
                    };
                    app.recent.status.insert(id, live);
                    cx.notify();
                });
            })
            .detach();
        }
    }

    /// Rejoin a remembered room through whoever is still in it.
    pub fn rejoin_recent(&mut self, room_id: &str, cx: &mut Context<Self>) {
        let Some(r) = self.recent.rooms.get(room_id).cloned() else {
            return;
        };
        if self
            .room
            .as_ref()
            .is_some_and(|room| room.memory.room_id == room_id)
        {
            self.screen = crate::app::Screen::Room;
            cx.notify();
            return;
        }
        let Ok(ticket) = Ticket::parse(&r.link) else {
            self.join_error =
                Some("That room's link is damaged; forget it and ask for a new one.".into());
            cx.notify();
            return;
        };
        let mut via: Vec<EndpointAddr> = match self.recent.status.get(room_id) {
            Some(Liveness::Alive(members)) => members.iter().map(|m| m.addr.clone()).collect(),
            _ => Vec::new(),
        };
        for addr in candidates(&r) {
            if !via.iter().any(|a| a.id == addr.id) {
                via.push(addr);
            }
        }
        self.join_with(ticket, Some(via), r.name.clone(), cx);
    }

    /// Everyone left: start the room again with the same link, so the others
    /// can rejoin it.
    pub fn restart_recent(&mut self, room_id: &str, cx: &mut Context<Self>) {
        let Some(r) = self.recent.rooms.get(room_id).cloned() else {
            return;
        };
        let (Some(net), Ok(ticket)) = (self.net.clone(), Ticket::parse(&r.link)) else {
            return;
        };
        if self.room.is_some() {
            self.leave_room(cx);
        }
        if self.busy {
            return;
        }
        self.busy = true;
        self.join_error = None;
        let state = crate::session::member_state(&glance_core::Participant {
            profile: self.me.clone(),
            muted: self.muted,
            deafened: self.deafened,
            speaking: false,
            camera: false,
            stream: None,
            connection: None,
            is_contact: false,
        });
        let fut = net.host_room(ticket, crate::net::wire_profile(&self.me), state);
        let name = r.name.clone();
        cx.spawn(async move |this, cx| {
            let result = fut.await;
            let _ = this.update(cx, |app, cx| {
                app.busy = false;
                match result {
                    Ok(handle) => app.attach_room(handle, name, cx),
                    Err(e) => app.join_error = Some(format!("Couldn't start the room: {e:#}")),
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    /// Forget a room and its saved chat.
    pub fn forget_recent(&mut self, room_id: &str, cx: &mut Context<Self>) {
        if self.recent.rooms.forget(room_id) {
            self.recent.status.remove(room_id);
            if let Some(chat) = self.chat_file(room_id) {
                self.recent.write(WriteOp::Remove(chat));
            }
            self.save_recent();
        }
        cx.notify();
    }

    /// "Recent rooms" on the home screen.
    pub fn render_recent_rooms(
        &mut self,
        p: &Palette,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if self.recent.rooms.rooms.is_empty() {
            return None;
        }
        self.ensure_recent_checked(cx);
        let current = self.room.as_ref().map(|r| r.memory.room_id.clone());
        let rows: Vec<AnyElement> = self
            .recent
            .rooms
            .rooms
            .iter()
            .take(SHOWN)
            .cloned()
            .collect::<Vec<_>>()
            .into_iter()
            .map(|r| {
                let here = current.as_ref() == Some(&r.room_id);
                self.recent_row(p, r, here, cx)
            })
            .collect();
        Some(
            div()
                .flex()
                .flex_col()
                .gap_2()
                .child(
                    div()
                        .pt_2()
                        .flex()
                        .items_center()
                        .justify_between()
                        .child(section_label(p, "RECENT ROOMS"))
                        .child(
                            icon_button(
                                "recent-refresh",
                                IconName::RefreshCw,
                                "Check again",
                                p.interactive,
                                p,
                            )
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.check_recent(cx);
                                cx.notify();
                            })),
                        ),
                )
                .children(rows)
                .into_any_element(),
        )
    }

    fn recent_row(
        &mut self,
        p: &Palette,
        r: RecentRoom,
        here: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let status = if here {
            None
        } else {
            Some(
                self.recent
                    .status
                    .get(&r.room_id)
                    .cloned()
                    .unwrap_or(Liveness::Checking),
            )
        };
        let when = r
            .left_at
            .map(|t| format!("Left {}", ago(now_ms().saturating_sub(t))))
            .unwrap_or_default();
        let (line, dim) = match &status {
            None => ("You're in it".to_string(), false),
            Some(Liveness::Checking) => ("Checking…".to_string(), false),
            Some(Liveness::Alive(members)) => {
                let names: Vec<&str> = members.iter().map(|m| m.profile.name.as_str()).collect();
                let n = members.len();
                (
                    format!(
                        "{n} {} in it: {}",
                        if n == 1 { "person" } else { "people" },
                        names.join(", ")
                    ),
                    false,
                )
            }
            Some(Liveness::Empty) => ("Empty".to_string(), true),
            Some(Liveness::Unknown) => ("Couldn't reach anyone".to_string(), true),
        };
        let id = r.room_id.clone();
        let action = {
            let id = id.clone();
            let busy = self.busy;
            let base = Button::new(SharedString::from(format!("recent-go-{}", r.room_id))).small();
            match &status {
                None => base
                    .primary()
                    .label("Open")
                    .on_click(cx.listener(move |this, _, _, cx| this.rejoin_recent(&id, cx))),
                Some(Liveness::Empty) => base
                    .label("Start again")
                    .loading(busy)
                    .on_click(cx.listener(move |this, _, _, cx| this.restart_recent(&id, cx))),
                Some(Liveness::Alive(_)) => base
                    .success()
                    .label("Rejoin")
                    .loading(busy)
                    .on_click(cx.listener(move |this, _, _, cx| this.rejoin_recent(&id, cx))),
                Some(_) => base
                    .label("Rejoin")
                    .loading(busy)
                    .on_click(cx.listener(move |this, _, _, cx| this.rejoin_recent(&id, cx))),
            }
        };
        let live_dot = matches!(status, Some(Liveness::Alive(_)) | None);
        div()
            .id(SharedString::from(format!("recent-{}", r.room_id)))
            .p_3()
            .rounded(px(8.))
            .bg(p.bg_sidebar)
            .flex()
            .flex_col()
            .gap_2()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(icon(
                        IconName::Volume2,
                        16.,
                        if live_dot { p.green } else { p.text_muted },
                    ))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(p.text_header)
                            .truncate()
                            .child(r.name.clone()),
                    )
                    .when(!here, |d| {
                        d.child(
                            icon_button(
                                SharedString::from(format!("recent-forget-{}", r.room_id)),
                                IconName::X,
                                "Forget this room",
                                p.interactive,
                                p,
                            )
                            .on_click(
                                cx.listener(move |this, _, _, cx| this.forget_recent(&id, cx)),
                            ),
                        )
                    }),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(if dim { p.text_muted } else { p.text_normal })
                                    .truncate()
                                    .child(line),
                            )
                            .when(!when.is_empty(), |d| {
                                d.child(div().text_xs().text_color(p.text_muted).child(when))
                            }),
                    )
                    .child(action),
            )
            .into_any_element()
    }
}

/// "5 minutes ago", "yesterday", "3 days ago".
fn ago(ms: u64) -> String {
    let secs = ms / 1000;
    match secs {
        0..60 => "just now".into(),
        60..3600 => format!("{} min ago", secs / 60),
        3600..86_400 => format!("{} h ago", secs / 3600),
        86_400..172_800 => "yesterday".into(),
        _ => format!("{} days ago", secs / 86_400),
    }
}

#[cfg(test)]
mod tests {
    use super::{ago, candidates};
    use glance_core::rooms::{RecentMember, RecentRoom};
    use glance_proto::EndpointAddr;

    #[test]
    fn ago_reads_naturally() {
        assert_eq!(ago(5_000), "just now");
        assert_eq!(ago(5 * 60_000), "5 min ago");
        assert_eq!(ago(3 * 3_600_000), "3 h ago");
        assert_eq!(ago(30 * 3_600_000), "yesterday");
        assert_eq!(ago(5 * 86_400_000), "5 days ago");
    }

    #[test]
    fn candidates_put_present_members_first_and_ignore_bad_addresses() {
        let key = |n: u8| iroh::SecretKey::from_bytes(&[n; 32]).public();
        let addr = EndpointAddr::new(key(1)).with_ip_addr("192.0.2.1:4433".parse().unwrap());
        let member = |n: u8, present: bool, addr: Option<String>| RecentMember {
            id: *key(n).as_bytes(),
            name: format!("m{n}"),
            color: 0,
            addr,
            present,
        };
        let room = RecentRoom {
            room_id: "aa".into(),
            link: String::new(),
            name: "Room".into(),
            members: vec![
                member(1, false, Some(serde_json::to_string(&addr).unwrap())),
                // An address that belongs to someone else is not trusted.
                member(2, true, Some(serde_json::to_string(&addr).unwrap())),
                member(3, false, Some("garbage".into())),
            ],
            joined_at: 0,
            left_at: None,
        };
        let got = candidates(&room);
        assert_eq!(got.len(), 3);
        assert_eq!(got[0], EndpointAddr::new(key(2)));
        assert_eq!(got[1], addr);
        assert_eq!(got[2], EndpointAddr::new(key(3)));
    }
}
