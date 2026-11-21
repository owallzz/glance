//! Direct messages with contacts: the conversation view on the home screen,
//! the outbox (queued until the contact is online, resent until acked),
//! unread counts and notifications. Conversations are saved per contact in
//! `dms/` in the data dir and loaded the first time they're needed; a
//! writer thread does the disk work.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use chrono::{DateTime, Local, TimeZone};
use glance_core::dms::{Conversation, DmBody, DmFile, DmIndex, DmState, hex};
use glance_core::{ContactState, PeerId, Presence, Profile, format};
use glance_net::contacts::{ContactCommand, DmFailure};
use glance_proto::{MAX_DM_LEN, WireDm, WireFile};
use gpui_kit::assets::IconName;
use gpui_kit::component::input::{InputEvent, Textarea, TextareaState};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::app::{GlanceApp, Screen, now_ms};
use crate::theme::Palette;
use crate::ui::{Avatar, icon, icon_button, presence_color};

/// Messages within this long of the previous one by the same author share
/// its header.
const GROUP_GAP_MS: u64 = 7 * 60 * 1000;
/// Messages rendered at first; "Show older" adds this many more.
const PAGE: usize = 150;

/// Direct-message state that lives in the app.
#[derive(Default)]
pub struct Dms {
    pub index: DmIndex,
    /// Conversations loaded so far.
    convs: HashMap<PeerId, Conversation>,
    dir: Option<PathBuf>,
    writer: Option<flume::Sender<Save>>,
    /// Conversations changed since the last [`Dms::flush`].
    dirty: HashSet<PeerId>,
    /// Contacts whose glance predates direct messages (until they reconnect).
    pub unsupported: HashSet<PeerId>,
    /// How many messages of the open conversation are shown.
    pub shown: usize,
    pub scroll: ScrollHandle,
}

impl Dms {
    pub fn open(data_dir: &std::path::Path) -> Self {
        let dir = data_dir.join("dms");
        let (tx, rx) = flume::unbounded::<Save>();
        let spawned = std::thread::Builder::new()
            .name("glance-dms".into())
            .spawn(move || writer(rx))
            .is_ok();
        let index = match DmIndex::load(&dir.join("index.json")) {
            Ok(index) => index,
            Err(e) => {
                tracing::warn!("{e}; rebuilding the message index");
                DmIndex::rebuild(&dir)
            }
        };
        let dms = Self {
            index,
            dir: Some(dir),
            writer: spawned.then_some(tx),
            shown: PAGE,
            ..Default::default()
        };
        if dms.dir.as_ref().is_some_and(|d| d.exists()) {
            // Write the (possibly rebuilt) index back.
            dms.save_index();
        }
        dms
    }

    fn path(&self, peer: PeerId) -> Option<PathBuf> {
        Some(self.dir.as_ref()?.join(format!("{}.json", hex(&peer.0))))
    }

    /// The conversation with `peer`, loading it on first use.
    pub fn conv(&mut self, peer: PeerId) -> &mut Conversation {
        let path = self.path(peer);
        self.convs.entry(peer).or_insert_with(|| {
            let Some(path) = path else {
                return Conversation::default();
            };
            Conversation::load(&path).unwrap_or_else(|e| {
                tracing::warn!("{e}; starting the conversation afresh");
                Conversation::default()
            })
        })
    }

    pub fn loaded(&self, peer: PeerId) -> Option<&Conversation> {
        self.convs.get(&peer)
    }

    /// Note that a conversation changed: its summary updates now, the
    /// files are written by the next [`Dms::flush`].
    pub fn save(&mut self, peer: PeerId) {
        let Some(c) = self.convs.get(&peer) else {
            return;
        };
        self.index.update(&peer.0, c);
        self.dirty.insert(peer);
    }

    /// Hand the changed conversations and the index to the writer thread,
    /// which serializes and writes them. Call once per batch of changes.
    pub fn flush(&mut self) {
        if self.dirty.is_empty() {
            return;
        }
        for peer in std::mem::take(&mut self.dirty) {
            let (Some(tx), Some(path), Some(c)) =
                (&self.writer, self.path(peer), self.convs.get(&peer))
            else {
                continue;
            };
            let _ = tx.send(Save::Conversation(path, c.clone()));
        }
        self.save_index();
    }

    fn save_index(&self) {
        if let (Some(tx), Some(dir)) = (&self.writer, &self.dir) {
            let _ = tx.send(Save::Index(dir.join("index.json"), self.index.clone()));
        }
    }

    pub fn unread(&self, peer: PeerId) -> u32 {
        self.index.get(&peer.0).map_or(0, |s| s.unread)
    }
}

/// A snapshot for the writer thread to serialize and write.
enum Save {
    Conversation(PathBuf, Conversation),
    Index(PathBuf, DmIndex),
}

impl Save {
    fn path(&self) -> &std::path::Path {
        match self {
            Save::Conversation(p, _) | Save::Index(p, _) => p,
        }
    }

    fn bytes(&self) -> Vec<u8> {
        match self {
            Save::Conversation(_, c) => c.to_json(),
            Save::Index(_, i) => i.to_json(),
        }
    }
}

/// Write files atomically, coalescing writes queued for the same file.
fn writer(rx: flume::Receiver<Save>) {
    while let Ok(first) = rx.recv() {
        let mut batch: Vec<Save> = vec![first];
        for save in rx.try_iter() {
            match batch.iter_mut().find(|s| s.path() == save.path()) {
                Some(slot) => *slot = save,
                None => batch.push(save),
            }
        }
        for save in batch {
            if let Err(e) = write_atomic(save.path(), &save.bytes()) {
                tracing::warn!("saving {} failed: {e}", save.path().display());
            }
        }
    }
}

/// Write to a temp file, flush it to disk, then rename it over `path`, so a
/// crash leaves either the old file or the new one.
fn write_atomic(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write as _;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("json.tmp");
    let mut f = std::fs::File::create(&tmp)?;
    f.write_all(bytes)?;
    f.sync_all()?;
    drop(f);
    std::fs::rename(&tmp, path)?;
    // Make the rename itself durable (not possible on Windows).
    #[cfg(unix)]
    if let Some(dir) = path.parent() {
        std::fs::File::open(dir)?.sync_all()?;
    }
    Ok(())
}

/// "Today at 14:05", "Yesterday at 09:12", "2026-03-04 18:30".
pub fn message_time<Tz: TimeZone>(at: DateTime<Tz>, now: DateTime<Tz>) -> String
where
    Tz::Offset: std::fmt::Display,
{
    let days = now
        .date_naive()
        .signed_duration_since(at.date_naive())
        .num_days();
    match days {
        0 => format!("Today at {}", at.format("%H:%M")),
        1 => format!("Yesterday at {}", at.format("%H:%M")),
        _ => at.format("%Y-%m-%d %H:%M").to_string(),
    }
}

fn local_time(ms: u64) -> String {
    match Local.timestamp_millis_opt(ms as i64).single() {
        Some(at) => message_time(at, Local::now()),
        None => String::new(),
    }
}

/// An id for a received file's timeline entry (stable, so a resent file
/// doesn't show twice).
fn file_entry_id(hash: &[u8; 32]) -> u64 {
    u64::from_le_bytes(hash[..8].try_into().expect("8 bytes"))
}

impl GlanceApp {
    pub fn new_dm_input(window: &mut Window, cx: &mut Context<Self>) -> Entity<TextareaState> {
        let input = cx.new(|cx| {
            TextareaState::new(window, cx)
                .placeholder("Message")
                .submit_on_enter(true)
                .auto_grow(1, 8)
        });
        cx.subscribe_in(
            &input,
            window,
            |this: &mut Self, input, ev: &InputEvent, window, cx| {
                if let InputEvent::PressEnter {
                    shift: false,
                    secondary: false,
                } = ev
                {
                    let text = input.read(cx).value().trim().to_string();
                    if text.is_empty() {
                        return;
                    }
                    if let Some(peer) = this.dm_open {
                        this.send_dm(peer, text);
                    }
                    input.update(cx, |s, cx| s.set_value("", window, cx));
                    cx.notify();
                }
            },
        )
        .detach();
        input
    }

    /// Keep track of whether the window is focused (for unread counts and
    /// notifications).
    pub fn watch_window_activation(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.window_active = window.is_window_active();
        cx.observe_window_activation(window, |this, window, cx| {
            this.window_active = window.is_window_active();
            if this.window_active {
                this.mark_open_dm_read();
                cx.notify();
            }
        })
        .detach();
    }

    fn contacts_send(&self, cmd: ContactCommand) {
        if let Some(net) = &self.net {
            net.contacts().send(cmd);
        }
    }

    /// The DM with `peer` is on screen and the window is focused.
    fn dm_visible(&self, peer: PeerId) -> bool {
        self.window_active && self.screen == Screen::Home && self.dm_open == Some(peer)
    }

    /// Open the conversation with a contact.
    pub fn open_dm(&mut self, peer: PeerId, window: &mut Window, cx: &mut Context<Self>) {
        self.screen = Screen::Home;
        if self.dm_open != Some(peer) {
            self.dm_open = Some(peer);
            self.dms.shown = PAGE;
            let name = self
                .profile_of(peer)
                .map(|p| p.name)
                .unwrap_or_else(|| "contact".into());
            self.dm_input.update(cx, |s, cx| {
                s.set_value("", window, cx);
                s.set_placeholder(format!("Message @{name}"), window, cx);
            });
        }
        self.dms.conv(peer);
        self.mark_open_dm_read();
        self.dms.scroll.scroll_to_bottom();
        self.dm_input.update(cx, |s, cx| s.focus(window, cx));
        cx.notify();
    }

    pub fn close_dm(&mut self, cx: &mut Context<Self>) {
        self.dm_open = None;
        cx.notify();
    }

    fn mark_open_dm_read(&mut self) {
        let Some(peer) = self.dm_open.filter(|_| self.screen == Screen::Home) else {
            return;
        };
        if self.dms.unread(peer) > 0 || self.dms.loaded(peer).is_some_and(|c| c.unread > 0) {
            self.dms.conv(peer).mark_read();
            self.dms.save(peer);
            self.dms.flush();
        }
    }

    /// Unread messages from mutual contacts (the rail badge). Conversations
    /// with people who are no longer contacts are hidden, so they don't
    /// count.
    pub fn dm_total_unread(&self) -> u32 {
        self.dm_list().iter().map(|(_, _, unread)| unread).sum()
    }

    /// Write a message to a contact: it goes out now if they're online,
    /// otherwise it waits for them.
    pub fn send_dm(&mut self, peer: PeerId, mut text: String) {
        glance_proto::truncate_utf8(&mut text, MAX_DM_LEN);
        let id: u64 = rand::random();
        let sent_at = now_ms();
        let online = self.live.online.contains(&peer) && !self.dms.unsupported.contains(&peer);
        let state = if online {
            DmState::Sending
        } else {
            DmState::Queued
        };
        self.dms
            .conv(peer)
            .push_mine(id, sent_at, DmBody::Text(text.clone()), state);
        self.dms.save(peer);
        self.dms.flush();
        self.dms.scroll.scroll_to_bottom();
        if online && let Ok(to) = glance_net::EndpointId::from_bytes(&peer.0) {
            self.contacts_send(ContactCommand::SendDm {
                to,
                dm: WireDm { id, sent_at, text },
            });
        }
    }

    /// A contact came online: send what's waiting for them.
    pub fn flush_dms(&mut self, peer: PeerId) {
        let waiting = self.dms.index.get(&peer.0).is_some_and(|s| s.pending > 0)
            || self
                .dms
                .loaded(peer)
                .is_some_and(|c| !c.pending().is_empty());
        if !waiting {
            return;
        }
        let Ok(to) = glance_net::EndpointId::from_bytes(&peer.0) else {
            return;
        };
        let conv = self.dms.conv(peer);
        let out: Vec<WireDm> = conv
            .pending()
            .into_iter()
            .filter_map(|m| match &m.body {
                DmBody::Text(t) => Some(WireDm {
                    id: m.id,
                    sent_at: m.sent_at,
                    text: t.clone(),
                }),
                DmBody::File(_) => None,
            })
            .collect();
        for dm in &out {
            conv.mark_sending(dm.id);
        }
        self.dms.save(peer);
        self.dms.flush();
        for dm in out {
            self.contacts_send(ContactCommand::SendDm { to, dm });
        }
    }

    pub fn on_dm_received(&mut self, from: PeerId, dm: WireDm) {
        let seen = self.dm_visible(from);
        let text = dm.text.clone();
        if !self
            .dms
            .conv(from)
            .receive(dm.id, dm.sent_at, DmBody::Text(dm.text), seen)
        {
            return;
        }
        self.dms.save(from);
        if seen {
            self.dms.scroll.scroll_to_bottom();
        } else if self.presence != Presence::DoNotDisturb {
            let name = self
                .profile_of(from)
                .map(|p| p.name)
                .unwrap_or_else(|| "A contact".into());
            let preview: String = text.chars().take(120).collect();
            crate::contacts::notify(&name, &preview);
        }
    }

    pub fn on_dm_acked(&mut self, from: PeerId, id: u64) {
        if self.dms.conv(from).ack(id) {
            self.dms.save(from);
        }
    }

    pub fn on_dm_failed(&mut self, to: PeerId, id: u64, reason: DmFailure) {
        if reason == DmFailure::Unsupported {
            self.dms.unsupported.insert(to);
        }
        if self.dms.conv(to).mark_queued(id) {
            self.dms.save(to);
        }
    }

    /// A direct file went to, or came from, a contact: show it in the
    /// conversation too.
    pub fn dm_file(&mut self, peer: PeerId, file: &WireFile, mine: bool) {
        let entry = DmFile {
            hash: file.hash,
            name: file.name.clone(),
            size: file.size,
            mime: file.mime.clone(),
        };
        let now = now_ms();
        if mine {
            self.dms.conv(peer).push_mine(
                rand::random(),
                now,
                DmBody::File(entry),
                DmState::Delivered,
            );
        } else {
            let seen = self.dm_visible(peer);
            let id = file_entry_id(&file.hash);
            if !self
                .dms
                .conv(peer)
                .receive(id, now, DmBody::File(entry), seen)
            {
                return;
            }
        }
        self.dms.save(peer);
        self.dms.flush();
        self.dms.scroll.scroll_to_bottom();
    }

    // ---- rendering ----

    /// Conversations for the home sidebar, most recent first (contacts only).
    pub fn dm_list(&self) -> Vec<(Profile, String, u32)> {
        self.dms
            .index
            .recent()
            .into_iter()
            .filter_map(|(id, s)| {
                let c = self
                    .contacts
                    .iter()
                    .find(|c| c.profile.id.0 == id && c.state == ContactState::Mutual)?;
                let mut profile = c.profile.clone();
                profile.name = c.display_name().to_string();
                Some((profile, s.preview.clone(), s.unread))
            })
            .collect()
    }

    pub fn render_dm(&mut self, p: &Palette, peer: PeerId, cx: &mut Context<Self>) -> AnyElement {
        let contact = self.contacts.iter().find(|c| c.profile.id == peer).cloned();
        let Some(contact) = contact else {
            self.dm_open = None;
            return div().into_any_element();
        };
        let name = contact.display_name().to_string();
        let online = contact.presence.is_online();
        let can_ring = contact.presence.can_ring();
        let mutual = contact.state == ContactState::Mutual;
        let status = match contact.in_call {
            Some(n) => format!("In a call ({n})"),
            None => contact.presence.label().to_string(),
        };
        let unsupported = self.dms.unsupported.contains(&peer);
        // On screen in the focused window, however we got back here (e.g.
        // leaving a call): nothing in it is unread.
        if self.window_active {
            self.mark_open_dm_read();
        }

        let shown_max = self.dms.shown;
        let conv = self.dms.conv(peer);
        let total = conv.messages.len();
        let hidden = total - shown_max.min(total);
        // Only the visible tail is copied out for rendering.
        let visible = conv.messages[hidden..].to_vec();
        // Group consecutive messages by author when close in time.
        let mut groups: Vec<Vec<glance_core::dms::DmMessage>> = Vec::new();
        for m in visible {
            match groups.last_mut() {
                Some(g)
                    if g[0].mine == m.mine
                        && m.sent_at.saturating_sub(g.last().map_or(0, |l| l.sent_at))
                            < GROUP_GAP_MS =>
                {
                    g.push(m)
                }
                _ => groups.push(vec![m]),
            }
        }
        let me = self.me.clone();
        let mut them = contact.profile.clone();
        them.name = name.clone();
        // Selection order across the whole list.
        let mut order = 0;
        let rendered: Vec<AnyElement> = groups
            .into_iter()
            .map(|g| {
                let who = if g[0].mine { &me } else { &them };
                self.dm_group(p, peer, who, &name, g, &mut order, cx)
            })
            .collect();

        let header = div()
            .h(px(48.))
            .px_4()
            .flex()
            .items_center()
            .gap_3()
            .border_b_1()
            .border_color(p.bg_rail)
            .child(
                Avatar::new(&them, 24.).status(presence_color(p, contact.presence), p.bg_content),
            )
            .child(
                div()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(p.text_header)
                    .child(name.clone()),
            )
            .child(div().text_sm().text_color(p.text_muted).child(status))
            .child(div().flex_1())
            .when(can_ring && mutual, |d| {
                d.child(
                    icon_button("dm-call", IconName::Phone, "Start a call", p.interactive, p)
                        .on_click(
                            cx.listener(move |this, _, _, cx| this.call_contacts(vec![peer], cx)),
                        ),
                )
            })
            .when(online && mutual, |d| {
                d.child(
                    icon_button(
                        "dm-file",
                        IconName::Paperclip,
                        "Send file",
                        p.interactive,
                        p,
                    )
                    .on_click(
                        cx.listener(move |this, _, _, cx| this.send_file_to_contact(peer, cx)),
                    ),
                )
            })
            .child(
                icon_button("dm-close", IconName::X, "Close", p.interactive, p)
                    .on_click(cx.listener(|this, _, _, cx| this.close_dm(cx))),
            );

        let intro = div()
            .px_4()
            .pt_8()
            .pb_4()
            .flex()
            .flex_col()
            .gap_2()
            .child(Avatar::new(&them, 72.))
            .child(
                div()
                    .text_2xl()
                    .font_weight(FontWeight::BOLD)
                    .text_color(p.text_header)
                    .child(name.clone()),
            )
            .child(div().text_sm().text_color(p.text_muted).child(format!(
                "This is the beginning of your direct messages with {name}. Messages go straight \
                 to them while you're both online and are kept on this computer only."
            )));

        let list = div()
            .id("dm-scroll")
            .track_scroll(&self.dms.scroll)
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .pb_4()
            .flex()
            .flex_col()
            .gap_4()
            .when(hidden == 0, |d| d.child(intro))
            .when(hidden > 0, |d| {
                let hover = p.bg_hover;
                d.child(
                    div().pt_2().flex().justify_center().child(
                        div()
                            .id("dm-older")
                            .px_3()
                            .py_1()
                            .rounded(px(4.))
                            .cursor_pointer()
                            .text_sm()
                            .text_color(p.text_muted)
                            .hover(move |s| s.bg(hover))
                            .child(format!("Show older messages ({hidden})"))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.dms.shown += PAGE;
                                cx.notify();
                            })),
                    ),
                )
            })
            .children(rendered);

        let note = if !mutual {
            Some("You can only message contacts.".to_string())
        } else if unsupported {
            Some(format!(
                "{name}'s glance is too old for messages. They'll get yours once they update."
            ))
        } else if !online {
            Some(format!(
                "{name} is offline. Messages are sent when you're both online."
            ))
        } else {
            None
        };

        let composer = div()
            .px_4()
            .pb_4()
            .flex()
            .flex_col()
            .gap_1()
            .children(note.map(|n| div().text_xs().text_color(p.text_muted).child(n)))
            .child(
                div()
                    .rounded(px(8.))
                    .bg(p.bg_hover)
                    .flex()
                    .items_start()
                    .gap_2()
                    .px_2()
                    .py(px(6.))
                    .when(online && mutual, |d| {
                        d.child(
                            icon_button(
                                "dm-attach",
                                IconName::CirclePlus,
                                "Send a file",
                                p.interactive,
                                p,
                            )
                            .on_click(cx.listener(
                                move |this, _, _, cx| this.send_file_to_contact(peer, cx),
                            )),
                        )
                    })
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .py(px(6.))
                            .child(Textarea::new(&self.dm_input)),
                    ),
            );

        div()
            .flex_1()
            .min_w_0()
            .h_full()
            .flex()
            .flex_col()
            .bg(p.bg_content)
            .child(header)
            .child(list)
            .when(mutual, |d| d.child(composer))
            .into_any_element()
    }

    #[allow(clippy::too_many_arguments)]
    fn dm_group(
        &mut self,
        p: &Palette,
        peer: PeerId,
        who: &Profile,
        their_name: &str,
        msgs: Vec<glance_core::dms::DmMessage>,
        order: &mut u64,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let time = local_time(msgs[0].sent_at);
        let mut rows = Vec::with_capacity(msgs.len());
        for m in msgs {
            let pending = m.state.is_pending();
            let state_note = match m.state {
                DmState::Queued => Some(format!("Waiting for {their_name}")),
                DmState::Sending => Some("Sending…".to_string()),
                _ => None,
            };
            let body = match m.body {
                DmBody::Text(t) => {
                    let msg = self.md_cache.get(&t);
                    let view = crate::markdown_view::MessageView {
                        scope: crate::markdown_view::ChatScope::Dm(peer),
                        msg_id: m.id,
                        order: &mut *order,
                        revealed: &self.revealed_spoilers,
                    };
                    div()
                        .text_color(p.text_normal)
                        .when(pending, |d| d.opacity(0.6))
                        .child(crate::markdown_view::render_message(&msg, view, cx))
                        .into_any_element()
                }
                DmBody::File(f) => self.dm_file_card(p, m.mine, &f, cx),
            };
            rows.push(
                div()
                    .flex()
                    .flex_col()
                    .child(body)
                    .children(state_note.map(|n| {
                        div()
                            .flex()
                            .items_center()
                            .gap_1()
                            .text_xs()
                            .text_color(p.text_muted)
                            .child(icon(IconName::Clock, 12., p.text_muted))
                            .child(n)
                    }))
                    .into_any_element(),
            );
        }
        div()
            .px_4()
            .flex()
            .gap_3()
            .child(Avatar::new(who, 40.))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(
                                div()
                                    .font_weight(FontWeight::MEDIUM)
                                    .text_color(crate::ui::color(who.color))
                                    .child(who.name.clone()),
                            )
                            .child(div().text_xs().text_color(p.text_muted).child(time)),
                    )
                    .children(rows),
            )
            .into_any_element()
    }

    /// A file in the conversation: the live card from the direct-file list
    /// while it's there, otherwise just its name and size.
    fn dm_file_card(
        &mut self,
        p: &Palette,
        mine: bool,
        f: &DmFile,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        if let Some(i) = self
            .direct_files
            .iter()
            .position(|d| d.offer.hash == f.hash && d.incoming != mine)
        {
            return div()
                .max_w(px(360.))
                .child(self.direct_file_card(p, i, cx))
                .into_any_element();
        }
        div()
            .max_w(px(360.))
            .p_3()
            .rounded(px(8.))
            .bg(p.bg_sidebar)
            .flex()
            .items_center()
            .gap_3()
            .child(icon(IconName::FileText, 24., p.text_muted))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .text_color(p.text_normal)
                            .truncate()
                            .child(f.name.clone()),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(p.text_muted)
                            .child(format::human_bytes(f.size)),
                    ),
            )
            .into_any_element()
    }
}

/// Red unread count pill.
pub fn unread_badge(p: &Palette, n: u32) -> Div {
    div()
        .min_w(px(16.))
        .h(px(16.))
        .px(px(4.))
        .rounded_full()
        .bg(p.red)
        .flex()
        .items_center()
        .justify_center()
        .text_size(px(11.))
        .font_weight(FontWeight::BOLD)
        .text_color(gpui_kit::white())
        .child(if n > 99 {
            "99+".to_string()
        } else {
            n.to_string()
        })
}

#[cfg(test)]
mod tests {
    use super::{Dms, message_time};
    use chrono::{TimeZone, Utc};
    use glance_core::PeerId;
    use glance_core::dms::{DmBody, DmState, hex};
    use std::time::{Duration, Instant};

    /// Wait for the writer thread to have written `path` with `pred` true.
    fn wait_for(path: &std::path::Path, pred: impl Fn(&str) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if std::fs::read_to_string(path).is_ok_and(|t| pred(&t)) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "{} never written",
                path.display()
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn outbox_survives_a_restart_and_flushes_once() {
        let data = std::env::temp_dir().join(format!("glance-app-dms-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&data);
        let peer = PeerId([7; 32]);
        let conv_path = data.join("dms").join(format!("{}.json", hex(&peer.0)));
        let index_path = data.join("dms").join("index.json");

        let mut dms = Dms::open(&data);
        let c = dms.conv(peer);
        c.push_mine(1, 10, DmBody::Text("in flight".into()), DmState::Sending);
        c.push_mine(2, 20, DmBody::Text("waiting".into()), DmState::Queued);
        dms.save(peer);
        // Saving only marks the conversation; the summary is current at once.
        assert_eq!(dms.index.get(&peer.0).unwrap().pending, 2);
        assert!(!conv_path.exists());
        dms.conv(peer)
            .receive(3, 30, DmBody::Text("hi".into()), false);
        dms.save(peer);
        dms.flush();
        wait_for(&conv_path, |t| t.contains("\"hi\""));
        wait_for(&index_path, |t| t.contains("\"unread\": 1"));
        drop(dms);

        // After a restart, what was in flight waits to be resent with the
        // rest of the outbox.
        let mut dms = Dms::open(&data);
        let s = dms.index.get(&peer.0).unwrap();
        assert_eq!((s.pending, s.unread), (2, 1));
        let pending: Vec<_> = dms
            .conv(peer)
            .pending()
            .iter()
            .map(|m| (m.id, m.state))
            .collect();
        assert_eq!(pending, vec![(1, DmState::Queued), (2, DmState::Queued)]);
        // Delivered once acked.
        assert!(dms.conv(peer).ack(1));
        assert!(dms.conv(peer).ack(2));
        dms.save(peer);
        dms.flush();
        wait_for(&index_path, |t| t.contains("\"pending\": 0"));
        let _ = std::fs::remove_dir_all(&data);
    }

    #[test]
    fn message_times_read_naturally() {
        let now = Utc.with_ymd_and_hms(2026, 3, 5, 12, 0, 0).unwrap();
        let t = |d, h, m| Utc.with_ymd_and_hms(2026, 3, d, h, m, 0).unwrap();
        assert_eq!(message_time(t(5, 9, 7), now), "Today at 09:07");
        assert_eq!(message_time(t(4, 23, 59), now), "Yesterday at 23:59");
        assert_eq!(message_time(t(1, 18, 30), now), "2026-03-01 18:30");
    }
}
