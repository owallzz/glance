//! Rooms you were in (rooms.json in the data dir) and each room's saved chat
//! (`chats/<room id>.json`). Pure logic plus atomic load/save, like the
//! contact book.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// How many rooms to remember.
pub const MAX_RECENT_ROOMS: usize = 50;
/// Forget rooms (and their chat) you haven't been in for this long.
pub const MAX_ROOM_AGE_MS: u64 = 90 * 24 * 60 * 60 * 1000;
/// Messages kept per room.
pub const MAX_SAVED_MESSAGES: usize = 1000;
/// Members remembered per room (for probing and rejoining).
pub const MAX_MEMBERS: usize = 24;

/// Write a file atomically: a temp file next to it, then rename.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(tmp, path)
}

fn load_json<T: for<'de> Deserialize<'de> + Default>(path: &Path) -> T {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

/// Someone who was in a room with you.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecentMember {
    pub id: [u8; 32],
    pub name: String,
    pub color: u32,
    /// Where they were reachable, encoded by the app (opaque here).
    #[serde(default)]
    pub addr: Option<String>,
    /// Still in the room when you left.
    #[serde(default)]
    pub present: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecentRoom {
    /// Hex room id (also names the chat file).
    pub room_id: String,
    /// The invite link (holds the room secret).
    pub link: String,
    pub name: String,
    /// Most recently seen first.
    #[serde(default)]
    pub members: Vec<RecentMember>,
    /// Unix ms.
    pub joined_at: u64,
    /// Unix ms; `None` while you're in it (or if glance quit abruptly).
    #[serde(default)]
    pub left_at: Option<u64>,
}

impl RecentRoom {
    pub fn last_active(&self) -> u64 {
        self.left_at.unwrap_or(self.joined_at).max(self.joined_at)
    }
}

/// "Room with Ana", "Room with Ana and Ben", "Room with Ana, Ben and 2 others".
pub fn room_name(names: &[String]) -> String {
    match names {
        [] => "Your room".into(),
        [a] => format!("Room with {a}"),
        [a, b] => format!("Room with {a} and {b}"),
        [a, b, c] => format!("Room with {a}, {b} and {c}"),
        [a, b, rest @ ..] => format!("Room with {a}, {b} and {} others", rest.len()),
    }
}

/// The rooms you were in, newest first.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecentRooms {
    pub rooms: Vec<RecentRoom>,
}

impl RecentRooms {
    pub fn load(path: &Path) -> Self {
        load_json(path)
    }

    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        write_atomic(
            path,
            &serde_json::to_vec_pretty(self).map_err(std::io::Error::other)?,
        )
    }

    pub fn get(&self, room_id: &str) -> Option<&RecentRoom> {
        self.rooms.iter().find(|r| r.room_id == room_id)
    }

    /// Remember a room (moves it to the top). Members you saw before but
    /// not this time are kept after the new ones.
    pub fn upsert(&mut self, mut room: RecentRoom) {
        if let Some(ix) = self.rooms.iter().position(|r| r.room_id == room.room_id) {
            let old = self.rooms.remove(ix);
            let mut seen: HashSet<[u8; 32]> = room.members.iter().map(|m| m.id).collect();
            for mut m in old.members {
                if seen.insert(m.id) {
                    m.present = false;
                    room.members.push(m);
                }
            }
            room.joined_at = room.joined_at.max(old.joined_at);
        }
        room.members.truncate(MAX_MEMBERS);
        self.rooms.insert(0, room);
    }

    pub fn forget(&mut self, room_id: &str) -> bool {
        let before = self.rooms.len();
        self.rooms.retain(|r| r.room_id != room_id);
        self.rooms.len() != before
    }

    /// Drop rooms older than [`MAX_ROOM_AGE_MS`] and beyond
    /// [`MAX_RECENT_ROOMS`]; returns their ids (to delete their chat).
    pub fn prune(&mut self, now: u64) -> Vec<String> {
        let mut gone = Vec::new();
        let mut kept = Vec::new();
        for r in self.rooms.drain(..) {
            if kept.len() < MAX_RECENT_ROOMS
                && now.saturating_sub(r.last_active()) <= MAX_ROOM_AGE_MS
            {
                kept.push(r);
            } else {
                gone.push(r.room_id);
            }
        }
        self.rooms = kept;
        gone
    }
}

/// Where a room's chat is saved. Non-hex characters are dropped from the id
/// so it can't escape the folder.
pub fn chat_path(data_dir: &Path, room_id: &str) -> PathBuf {
    let safe: String = room_id
        .chars()
        .filter(char::is_ascii_hexdigit)
        .take(32)
        .collect();
    data_dir.join("chats").join(format!("{safe}.json"))
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SavedBody {
    Text(String),
    File {
        hash: [u8; 32],
        name: String,
        size: u64,
        mime: String,
    },
}

/// One chat message or file offer, as saved.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedMessage {
    pub author: [u8; 32],
    pub author_name: String,
    pub author_color: u32,
    /// Unix ms, as stamped by the sender (the same on every peer).
    pub sent_at: u64,
    pub body: SavedBody,
}

/// What identifies a message across peers and saves: the wire has no
/// message id in history, but (author, send time, content) is unique enough.
pub type MessageKey = ([u8; 32], u64, SavedBody);

impl SavedMessage {
    pub fn key(&self) -> MessageKey {
        (self.author, self.sent_at, self.body.clone())
    }
}

/// A room's saved chat, oldest first.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChatLog {
    pub messages: Vec<SavedMessage>,
}

impl ChatLog {
    pub fn load(path: &Path) -> Self {
        let mut log: Self = load_json(path);
        log.cap();
        log
    }

    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        write_atomic(
            path,
            &serde_json::to_vec(self).map_err(std::io::Error::other)?,
        )
    }

    /// Keep only the newest [`MAX_SAVED_MESSAGES`].
    pub fn cap(&mut self) {
        let extra = self.messages.len().saturating_sub(MAX_SAVED_MESSAGES);
        self.messages.drain(..extra);
    }
}

/// Of `incoming`, the messages not already in `known`, without duplicates,
/// in send-time order.
pub fn new_messages<'a>(
    known: impl IntoIterator<Item = &'a SavedMessage>,
    incoming: impl IntoIterator<Item = SavedMessage>,
) -> Vec<SavedMessage> {
    let mut seen: HashSet<MessageKey> = known.into_iter().map(SavedMessage::key).collect();
    let mut fresh: Vec<SavedMessage> = incoming
        .into_iter()
        .filter(|m| seen.insert(m.key()))
        .collect();
    fresh.sort_by_key(|m| m.sent_at);
    fresh
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("glance-rooms-{name}-{}", std::process::id()))
    }

    fn room(id: &str, at: u64) -> RecentRoom {
        RecentRoom {
            room_id: id.into(),
            link: format!("glance://join/{id}"),
            name: "Room".into(),
            members: Vec::new(),
            joined_at: at,
            left_at: Some(at + 10),
        }
    }

    fn member(n: u8, present: bool) -> RecentMember {
        RecentMember {
            id: [n; 32],
            name: format!("m{n}"),
            color: 0,
            addr: None,
            present,
        }
    }

    fn text(author: u8, at: u64, t: &str) -> SavedMessage {
        SavedMessage {
            author: [author; 32],
            author_name: "a".into(),
            author_color: 0,
            sent_at: at,
            body: SavedBody::Text(t.into()),
        }
    }

    #[test]
    fn rooms_save_atomically_and_load_back() {
        let dir = tmp("save");
        let path = dir.join("rooms.json");
        let mut rooms = RecentRooms::default();
        rooms.upsert(room("aa", 1));
        rooms.save(&path).unwrap();
        assert!(!dir.join("rooms.json.tmp").exists());
        assert_eq!(RecentRooms::load(&path), rooms);
        // A broken file reads as empty rather than failing.
        std::fs::write(&path, b"{nope").unwrap();
        assert_eq!(RecentRooms::load(&path), RecentRooms::default());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn upsert_moves_to_top_and_merges_members() {
        let mut rooms = RecentRooms::default();
        let mut a = room("aa", 1);
        a.members = vec![member(1, true), member(2, true)];
        rooms.upsert(a);
        rooms.upsert(room("bb", 2));
        let mut again = room("aa", 3);
        again.members = vec![member(3, true), member(1, true)];
        rooms.upsert(again);
        assert_eq!(rooms.rooms[0].room_id, "aa");
        assert_eq!(rooms.rooms.len(), 2);
        let ids: Vec<u8> = rooms.rooms[0].members.iter().map(|m| m.id[0]).collect();
        assert_eq!(ids, [3, 1, 2]);
        assert!(!rooms.rooms[0].members[2].present);
        assert!(rooms.forget("aa"));
        assert!(!rooms.forget("aa"));
        assert_eq!(rooms.rooms.len(), 1);
    }

    #[test]
    fn prune_drops_old_rooms_and_caps_the_count() {
        let now = MAX_ROOM_AGE_MS * 2;
        let mut rooms = RecentRooms::default();
        rooms.upsert(room("old", 1));
        for i in 0..MAX_RECENT_ROOMS + 3 {
            rooms.upsert(room(&format!("{i:02x}"), now - 1000 + i as u64));
        }
        let gone = rooms.prune(now);
        assert_eq!(rooms.rooms.len(), MAX_RECENT_ROOMS);
        assert_eq!(gone.len(), 4);
        assert!(gone.contains(&"old".to_string()));
        // The newest are kept.
        assert_eq!(
            rooms.rooms[0].room_id,
            format!("{:02x}", MAX_RECENT_ROOMS + 2)
        );
    }

    #[test]
    fn names_read_naturally() {
        let n = |v: &[&str]| room_name(&v.iter().map(|s| s.to_string()).collect::<Vec<_>>());
        assert_eq!(n(&[]), "Your room");
        assert_eq!(n(&["Ana"]), "Room with Ana");
        assert_eq!(n(&["Ana", "Ben"]), "Room with Ana and Ben");
        assert_eq!(n(&["Ana", "Ben", "Cy"]), "Room with Ana, Ben and Cy");
        assert_eq!(
            n(&["Ana", "Ben", "Cy", "Di"]),
            "Room with Ana, Ben and 2 others"
        );
    }

    #[test]
    fn chat_path_stays_in_the_folder() {
        let p = chat_path(Path::new("/data"), "../../etc/passwd0a1b");
        assert_eq!(p, Path::new("/data/chats/ecad0a1b.json"));
    }

    #[test]
    fn chat_log_caps_and_round_trips() {
        let dir = tmp("chat");
        let path = chat_path(&dir, "abcd");
        let mut log = ChatLog {
            messages: (0..MAX_SAVED_MESSAGES as u64 + 5)
                .map(|i| text(1, i, "x"))
                .collect(),
        };
        log.cap();
        assert_eq!(log.messages.len(), MAX_SAVED_MESSAGES);
        assert_eq!(log.messages[0].sent_at, 5);
        log.save(&path).unwrap();
        assert_eq!(ChatLog::load(&path), log);
        assert_eq!(ChatLog::load(&dir.join("missing.json")), ChatLog::default());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn merging_skips_messages_already_known() {
        let saved = vec![text(1, 10, "hi"), text(2, 20, "yo")];
        let file = SavedMessage {
            body: SavedBody::File {
                hash: [7; 32],
                name: "f".into(),
                size: 1,
                mime: "text/plain".into(),
            },
            ..text(2, 30, "")
        };
        let incoming = vec![
            text(1, 10, "hi"),
            text(3, 40, "new"),
            file.clone(),
            text(3, 40, "new"),
            // Same time and author, different text: a different message.
            text(1, 10, "hi again"),
        ];
        let fresh = new_messages(&saved, incoming);
        assert_eq!(
            fresh,
            vec![text(1, 10, "hi again"), file, text(3, 40, "new")]
        );
        assert!(new_messages(&saved, saved.clone()).is_empty());
    }
}
