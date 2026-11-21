//! Direct messages with contacts: one conversation per contact plus a small
//! index (last message, unread and undelivered counts) so the home screen
//! can list conversations without loading them all. Pure state; the app
//! does the file I/O ([`Conversation::load`] and the `to_json` helpers give
//! it what to write).
//!
//! There's no server, so an outgoing message waits in the conversation as
//! [`DmState::Queued`] until the contact is online, goes [`DmState::Sending`]
//! when it's handed to the network, and [`DmState::Delivered`] once the
//! contact acks it. Message ids are random and stable, so a resend after a
//! lost ack is recognised and dropped by the receiver.

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

/// Messages kept per conversation; older delivered ones are dropped.
pub const MAX_MESSAGES: usize = 2000;
/// Characters of the last message shown in the conversation list.
const PREVIEW_CHARS: usize = 80;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum DmState {
    /// Ours, waiting for the contact to come online.
    Queued,
    /// Ours, handed to the network, no ack yet.
    Sending,
    /// Ours, acked by the contact.
    Delivered,
    /// Theirs.
    Received,
}

impl DmState {
    /// Ours and not acked yet (needs a (re)send).
    pub fn is_pending(self) -> bool {
        matches!(self, DmState::Queued | DmState::Sending)
    }
}

/// A file sent or received in the conversation (the transfer itself lives
/// in the app's direct-file list, keyed by hash).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DmFile {
    pub hash: [u8; 32],
    pub name: String,
    pub size: u64,
    pub mime: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum DmBody {
    Text(String),
    File(DmFile),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DmMessage {
    pub id: u64,
    /// We wrote it.
    pub mine: bool,
    /// Unix time in milliseconds (the sender's clock).
    pub sent_at: u64,
    pub body: DmBody,
    pub state: DmState,
}

impl DmMessage {
    pub fn preview(&self) -> String {
        match &self.body {
            DmBody::Text(t) => {
                let line = t.lines().next().unwrap_or_default();
                let mut p: String = line.chars().take(PREVIEW_CHARS).collect();
                if p.len() < t.len() {
                    p.push('…');
                }
                p
            }
            DmBody::File(f) => format!("Sent a file: {}", f.name),
        }
    }
}

/// What the conversation list needs about one conversation.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DmSummary {
    pub last_at: u64,
    pub preview: String,
    /// Their messages we haven't seen.
    pub unread: u32,
    /// Ours that aren't delivered yet.
    pub pending: u32,
}

/// One conversation, saved as `dms/<peer hex>.json`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Conversation {
    pub messages: Vec<DmMessage>,
    #[serde(default)]
    pub unread: u32,
}

impl Conversation {
    /// Load a saved conversation. Messages that were in flight when the app
    /// stopped go back to queued.
    pub fn load(path: &Path) -> Self {
        let mut c: Conversation = std::fs::read_to_string(path)
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default();
        for m in &mut c.messages {
            if m.state == DmState::Sending {
                m.state = DmState::Queued;
            }
        }
        c
    }

    pub fn to_json(&self) -> Vec<u8> {
        serde_json::to_vec(self).unwrap_or_default()
    }

    fn find_mut(&mut self, id: u64) -> Option<&mut DmMessage> {
        self.messages.iter_mut().rev().find(|m| m.id == id)
    }

    pub fn contains(&self, id: u64) -> bool {
        self.messages.iter().any(|m| m.id == id)
    }

    /// Add a message we wrote (queued until it's sent).
    pub fn push_mine(&mut self, id: u64, sent_at: u64, body: DmBody, state: DmState) {
        self.messages.push(DmMessage {
            id,
            mine: true,
            sent_at,
            body,
            state,
        });
        self.trim();
    }

    /// Add a message from the contact. Returns false for a duplicate (a
    /// resend whose ack got lost). `seen`: the conversation is on screen,
    /// so it doesn't count as unread. Messages are kept in send order, so a
    /// queued message that arrives late slots in where it was written.
    pub fn receive(&mut self, id: u64, sent_at: u64, body: DmBody, seen: bool) -> bool {
        if self.contains(id) {
            return false;
        }
        let at = self
            .messages
            .iter()
            .rposition(|m| m.sent_at <= sent_at)
            .map_or(0, |i| i + 1);
        self.messages.insert(
            at,
            DmMessage {
                id,
                mine: false,
                sent_at,
                body,
                state: DmState::Received,
            },
        );
        if !seen {
            self.unread += 1;
        }
        self.trim();
        true
    }

    /// Our message went to the network. Returns false if it's unknown or
    /// already delivered.
    pub fn mark_sending(&mut self, id: u64) -> bool {
        match self.find_mut(id) {
            Some(m) if m.mine && m.state.is_pending() => {
                m.state = DmState::Sending;
                true
            }
            _ => false,
        }
    }

    /// Our message couldn't go out (they're offline): wait for them.
    pub fn mark_queued(&mut self, id: u64) -> bool {
        match self.find_mut(id) {
            Some(m) if m.mine && m.state == DmState::Sending => {
                m.state = DmState::Queued;
                true
            }
            _ => false,
        }
    }

    /// The contact acked our message.
    pub fn ack(&mut self, id: u64) -> bool {
        match self.find_mut(id) {
            Some(m) if m.mine && m.state.is_pending() => {
                m.state = DmState::Delivered;
                true
            }
            _ => false,
        }
    }

    /// Our text messages that still need sending, oldest first.
    pub fn pending(&self) -> Vec<&DmMessage> {
        self.messages
            .iter()
            .filter(|m| m.mine && m.state.is_pending() && matches!(m.body, DmBody::Text(_)))
            .collect()
    }

    pub fn mark_read(&mut self) -> bool {
        std::mem::take(&mut self.unread) > 0
    }

    pub fn summary(&self) -> DmSummary {
        let last = self.messages.last();
        DmSummary {
            last_at: last.map_or(0, |m| m.sent_at),
            preview: last.map(DmMessage::preview).unwrap_or_default(),
            unread: self.unread,
            pending: self.pending().len() as u32,
        }
    }

    /// Keep at most [`MAX_MESSAGES`], dropping the oldest messages that
    /// aren't waiting to be sent.
    fn trim(&mut self) {
        let mut excess = self.messages.len().saturating_sub(MAX_MESSAGES);
        if excess == 0 {
            return;
        }
        self.messages.retain(|m| {
            if excess > 0 && !m.state.is_pending() {
                excess -= 1;
                false
            } else {
                true
            }
        });
    }
}

/// Summaries of every conversation, saved as `dms/index.json` and keyed by
/// the contact's id in hex.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DmIndex {
    pub conversations: BTreeMap<String, DmSummary>,
}

impl DmIndex {
    pub fn load(path: &Path) -> Self {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default()
    }

    pub fn to_json(&self) -> Vec<u8> {
        serde_json::to_vec_pretty(self).unwrap_or_default()
    }

    pub fn get(&self, peer: &[u8; 32]) -> Option<&DmSummary> {
        self.conversations.get(&hex(peer))
    }

    /// Record a conversation's current state.
    pub fn update(&mut self, peer: &[u8; 32], c: &Conversation) {
        if c.messages.is_empty() {
            self.conversations.remove(&hex(peer));
        } else {
            self.conversations.insert(hex(peer), c.summary());
        }
    }

    pub fn total_unread(&self) -> u32 {
        self.conversations.values().map(|s| s.unread).sum()
    }

    /// Conversations, most recent first.
    pub fn recent(&self) -> Vec<([u8; 32], &DmSummary)> {
        let mut list: Vec<([u8; 32], &DmSummary)> = self
            .conversations
            .iter()
            .filter_map(|(k, s)| Some((unhex(k)?, s)))
            .collect();
        list.sort_by_key(|(_, s)| std::cmp::Reverse(s.last_at));
        list
    }
}

/// A contact id as lowercase hex (file names, index keys).
pub fn hex(id: &[u8; 32]) -> String {
    id.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex(s: &str) -> Option<[u8; 32]> {
    if s.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, b) in out.iter_mut().enumerate() {
        *b = u8::from_str_radix(s.get(i * 2..i * 2 + 2)?, 16).ok()?;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(t: &str) -> DmBody {
        DmBody::Text(t.into())
    }

    #[test]
    fn outgoing_moves_from_queued_to_delivered() {
        let mut c = Conversation::default();
        c.push_mine(1, 10, text("hi"), DmState::Queued);
        assert_eq!(c.pending().len(), 1);
        assert!(c.mark_sending(1));
        assert_eq!(c.messages[0].state, DmState::Sending);
        // Offline again: back to waiting.
        assert!(c.mark_queued(1));
        assert_eq!(c.messages[0].state, DmState::Queued);
        assert!(c.mark_sending(1));
        assert!(c.ack(1));
        assert_eq!(c.messages[0].state, DmState::Delivered);
        assert!(c.pending().is_empty());
        // A second ack (from a resend) changes nothing.
        assert!(!c.ack(1));
        assert!(!c.mark_sending(1));
        assert!(!c.mark_queued(1));
        assert!(!c.ack(99));
    }

    #[test]
    fn received_messages_are_deduped_and_counted_unread() {
        let mut c = Conversation::default();
        assert!(c.receive(7, 100, text("a"), false));
        assert!(!c.receive(7, 100, text("a"), false));
        assert!(c.receive(8, 200, text("b"), true));
        assert_eq!(c.messages.len(), 2);
        assert_eq!(c.unread, 1);
        assert!(c.mark_read());
        assert!(!c.mark_read());
        // Can't ack their message.
        assert!(!c.ack(7));
    }

    #[test]
    fn late_messages_slot_in_by_send_time() {
        let mut c = Conversation::default();
        c.push_mine(1, 100, text("mine"), DmState::Delivered);
        c.push_mine(2, 300, text("later"), DmState::Delivered);
        c.receive(3, 200, text("theirs"), true);
        c.receive(4, 50, text("oldest"), true);
        let ids: Vec<u64> = c.messages.iter().map(|m| m.id).collect();
        assert_eq!(ids, [4, 1, 3, 2]);
    }

    #[test]
    fn cap_keeps_undelivered_messages() {
        let mut c = Conversation::default();
        c.push_mine(0, 0, text("waiting"), DmState::Queued);
        for i in 1..=(MAX_MESSAGES as u64 + 10) {
            c.receive(i, i, text("x"), true);
        }
        assert_eq!(c.messages.len(), MAX_MESSAGES);
        assert_eq!(c.messages[0].id, 0, "the queued message survives");
        assert_eq!(c.messages[1].id, 12, "the 11 oldest delivered ones went");
        assert_eq!(c.messages.last().unwrap().id, MAX_MESSAGES as u64 + 10);
    }

    #[test]
    fn index_sorts_by_last_message_and_sums_unread() {
        let (a, b) = ([1u8; 32], [2u8; 32]);
        let mut ca = Conversation::default();
        ca.receive(1, 100, text("old"), false);
        let mut cb = Conversation::default();
        cb.push_mine(2, 500, text("a long first line\nsecond"), DmState::Queued);
        cb.receive(3, 400, text("x"), false);
        cb.receive(4, 450, text("y"), false);
        let mut idx = DmIndex::default();
        idx.update(&a, &ca);
        idx.update(&b, &cb);
        let recent = idx.recent();
        assert_eq!(recent[0].0, b);
        assert_eq!(recent[1].0, a);
        assert_eq!(idx.total_unread(), 3);
        let s = idx.get(&b).unwrap();
        assert_eq!(s.pending, 1);
        assert_eq!(s.preview, "a long first line…");
        assert_eq!(s.last_at, 500);
        idx.update(&a, &Conversation::default());
        assert!(idx.get(&a).is_none());
    }

    #[test]
    fn save_and_load_requeue_in_flight_messages() {
        let dir = std::env::temp_dir().join(format!("glance-dms-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("{}.json", hex(&[9; 32])));
        let mut c = Conversation::default();
        c.push_mine(1, 1, text("in flight"), DmState::Sending);
        c.push_mine(2, 2, text("done"), DmState::Delivered);
        c.push_mine(
            3,
            3,
            DmBody::File(DmFile {
                hash: [4; 32],
                name: "a.png".into(),
                size: 3,
                mime: "image/png".into(),
            }),
            DmState::Delivered,
        );
        c.receive(4, 4, text("hey"), false);
        std::fs::write(&path, c.to_json()).unwrap();
        let back = Conversation::load(&path);
        assert_eq!(back.messages.len(), 4);
        assert_eq!(back.messages[0].state, DmState::Queued);
        assert_eq!(back.messages[1].state, DmState::Delivered);
        assert_eq!(back.unread, 1);
        assert_eq!(back.messages[2].preview(), "Sent a file: a.png");
        assert_eq!(Conversation::load(&dir.join("missing.json")), Conversation::default());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn hex_round_trips() {
        let id = [0xab; 32];
        assert_eq!(unhex(&hex(&id)), Some(id));
        assert_eq!(unhex("zz"), None);
    }
}
