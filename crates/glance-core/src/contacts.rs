//! The contact book: who your contacts are, pending requests, blocks, and
//! recent calls. Saved as JSON in the data dir. Pure logic, no I/O except
//! [`ContactBook::load`]/[`ContactBook::save`].

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::model::{CallRecord, Contact, ContactState, PeerId, Presence, Profile};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum StoredState {
    Mutual,
    Incoming,
    Outgoing,
    Blocked,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredContact {
    pub id: [u8; 32],
    pub name: String,
    pub color: u32,
    pub state: StoredState,
    #[serde(default)]
    pub nickname: Option<String>,
    #[serde(default)]
    pub favorite: bool,
    pub added_at: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredCall {
    pub with: Vec<[u8; 32]>,
    pub started_at: u64,
    pub duration_secs: u64,
    pub missed: bool,
}

/// How many calls to remember.
pub const CALL_HISTORY: usize = 50;

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContactBook {
    pub contacts: Vec<StoredContact>,
    #[serde(default)]
    pub calls: Vec<StoredCall>,
}

/// What the network layer should do after a book change.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    Nothing,
    /// Show "X wants to add you".
    ShowRequest,
    /// We're now mutual contacts.
    BecameMutual,
}

impl ContactBook {
    pub fn load(path: &Path) -> Self {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default()
    }

    /// Atomic save (write a temp file, then rename).
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("json.tmp");
        std::fs::write(
            &tmp,
            serde_json::to_vec_pretty(self).map_err(std::io::Error::other)?,
        )?;
        std::fs::rename(tmp, path)
    }

    pub fn get(&self, id: &[u8; 32]) -> Option<&StoredContact> {
        self.contacts.iter().find(|c| &c.id == id)
    }

    fn get_mut(&mut self, id: &[u8; 32]) -> Option<&mut StoredContact> {
        self.contacts.iter_mut().find(|c| &c.id == id)
    }

    pub fn state(&self, id: &[u8; 32]) -> Option<StoredState> {
        self.get(id).map(|c| c.state)
    }

    pub fn is_mutual(&self, id: &[u8; 32]) -> bool {
        self.state(id) == Some(StoredState::Mutual)
    }

    fn upsert(&mut self, id: [u8; 32], name: &str, color: u32, state: StoredState, now: u64) {
        match self.get_mut(&id) {
            Some(c) => {
                c.state = state;
                c.name = name.to_string();
                c.color = color;
            }
            None => self.contacts.push(StoredContact {
                id,
                name: name.to_string(),
                color,
                state,
                nickname: None,
                favorite: false,
                added_at: now,
            }),
        }
    }

    /// We sent a request.
    pub fn request_sent(&mut self, id: [u8; 32], name: &str, color: u32, now: u64) -> Outcome {
        match self.state(&id) {
            Some(StoredState::Mutual | StoredState::Blocked) => Outcome::Nothing,
            // They'd already asked us: sending one back accepts.
            Some(StoredState::Incoming) => {
                self.upsert(id, name, color, StoredState::Mutual, now);
                Outcome::BecameMutual
            }
            _ => {
                self.upsert(id, name, color, StoredState::Outgoing, now);
                Outcome::Nothing
            }
        }
    }

    /// Someone asked to add us.
    pub fn request_received(&mut self, id: [u8; 32], name: &str, color: u32, now: u64) -> Outcome {
        match self.state(&id) {
            Some(StoredState::Blocked) => Outcome::Nothing,
            Some(StoredState::Mutual) => Outcome::Nothing,
            Some(StoredState::Outgoing) => {
                self.upsert(id, name, color, StoredState::Mutual, now);
                Outcome::BecameMutual
            }
            _ => {
                self.upsert(id, name, color, StoredState::Incoming, now);
                Outcome::ShowRequest
            }
        }
    }

    /// We accepted their request, or they accepted ours.
    pub fn accept(&mut self, id: [u8; 32], name: &str, color: u32, now: u64) -> Outcome {
        match self.state(&id) {
            Some(StoredState::Blocked) | None if name.is_empty() => Outcome::Nothing,
            Some(StoredState::Blocked) => Outcome::Nothing,
            _ => {
                self.upsert(id, name, color, StoredState::Mutual, now);
                Outcome::BecameMutual
            }
        }
    }

    /// Remove a contact or request (theirs or ours). Blocks stay.
    pub fn remove(&mut self, id: &[u8; 32]) {
        self.contacts
            .retain(|c| &c.id != id || c.state == StoredState::Blocked);
    }

    pub fn block(&mut self, id: [u8; 32], name: &str, color: u32, now: u64) {
        self.upsert(id, name, color, StoredState::Blocked, now);
    }

    pub fn unblock(&mut self, id: &[u8; 32]) {
        self.contacts
            .retain(|c| !(&c.id == id && c.state == StoredState::Blocked));
    }

    pub fn set_favorite(&mut self, id: &[u8; 32], favorite: bool) {
        if let Some(c) = self.get_mut(id) {
            c.favorite = favorite;
        }
    }

    pub fn set_nickname(&mut self, id: &[u8; 32], nickname: Option<String>) {
        if let Some(c) = self.get_mut(id) {
            c.nickname = nickname.filter(|n| !n.trim().is_empty());
        }
    }

    /// A contact changed their name or colour.
    pub fn update_profile(&mut self, id: &[u8; 32], name: &str, color: u32) {
        if let Some(c) = self.get_mut(id)
            && !name.is_empty()
        {
            c.name = name.to_string();
            c.color = color;
        }
    }

    pub fn record_call(&mut self, call: StoredCall) {
        self.calls.insert(0, call);
        self.calls.truncate(CALL_HISTORY);
    }

    /// Contacts as the UI shows them, with live presence filled in by `presence`.
    pub fn view(
        &self,
        presence: impl Fn(&[u8; 32]) -> (Presence, Option<u8>, bool),
    ) -> Vec<Contact> {
        let mut out: Vec<Contact> = self
            .contacts
            .iter()
            .map(|c| {
                let (p, in_call, joinable) = if c.state == StoredState::Mutual {
                    presence(&c.id)
                } else {
                    (Presence::Offline, None, false)
                };
                Contact {
                    profile: Profile {
                        id: PeerId(c.id),
                        name: c.name.clone(),
                        color: c.color,
                    },
                    state: match c.state {
                        StoredState::Mutual => ContactState::Mutual,
                        StoredState::Incoming => ContactState::Incoming,
                        StoredState::Outgoing => ContactState::Outgoing,
                        StoredState::Blocked => ContactState::Blocked,
                    },
                    nickname: c.nickname.clone(),
                    favorite: c.favorite,
                    presence: p,
                    in_call,
                    joinable,
                }
            })
            .collect();
        out.sort_by(|a, b| {
            (
                !a.favorite,
                !a.presence.is_online(),
                a.display_name().to_lowercase(),
            )
                .cmp(&(
                    !b.favorite,
                    !b.presence.is_online(),
                    b.display_name().to_lowercase(),
                ))
        });
        out
    }

    pub fn call_view(&self) -> Vec<CallRecord> {
        self.calls
            .iter()
            .map(|c| CallRecord {
                with: c.with.iter().map(|id| PeerId(*id)).collect(),
                started_at: c.started_at,
                duration_secs: c.duration_secs,
                missed: c.missed,
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: [u8; 32] = [1; 32];
    const B: [u8; 32] = [2; 32];

    #[test]
    fn request_then_accept_becomes_mutual() {
        let mut book = ContactBook::default();
        assert_eq!(book.request_received(A, "Ana", 1, 10), Outcome::ShowRequest);
        assert_eq!(book.state(&A), Some(StoredState::Incoming));
        assert_eq!(book.accept(A, "Ana", 1, 11), Outcome::BecameMutual);
        assert!(book.is_mutual(&A));
    }

    #[test]
    fn crossing_requests_become_mutual() {
        let mut book = ContactBook::default();
        assert_eq!(book.request_sent(A, "Ana", 1, 10), Outcome::Nothing);
        assert_eq!(book.state(&A), Some(StoredState::Outgoing));
        assert_eq!(
            book.request_received(A, "Ana", 1, 11),
            Outcome::BecameMutual
        );
        let mut book2 = ContactBook::default();
        book2.request_received(B, "Ben", 2, 10);
        assert_eq!(book2.request_sent(B, "Ben", 2, 11), Outcome::BecameMutual);
    }

    #[test]
    fn blocks_win_and_survive_remove() {
        let mut book = ContactBook::default();
        book.block(A, "Ana", 1, 10);
        assert_eq!(book.request_received(A, "Ana", 1, 11), Outcome::Nothing);
        assert_eq!(book.accept(A, "Ana", 1, 12), Outcome::Nothing);
        book.remove(&A);
        assert_eq!(book.state(&A), Some(StoredState::Blocked));
        book.unblock(&A);
        assert_eq!(book.state(&A), None);
    }

    #[test]
    fn view_sorts_favorites_then_online_then_name() {
        let mut book = ContactBook::default();
        for (id, name) in [([1; 32], "zed"), ([2; 32], "amy"), ([3; 32], "bob")] {
            book.request_received(id, name, 0, 1);
            book.accept(id, name, 0, 2);
        }
        book.set_favorite(&[1; 32], true);
        let online = |id: &[u8; 32]| {
            if id == &[3; 32] {
                (Presence::Online, None, false)
            } else {
                (Presence::Offline, None, false)
            }
        };
        let names: Vec<_> = book
            .view(online)
            .iter()
            .map(|c| c.profile.name.clone())
            .collect();
        assert_eq!(names, vec!["zed", "bob", "amy"]);
    }

    #[test]
    fn persists_and_caps_call_history() {
        let mut book = ContactBook::default();
        book.request_received(A, "Ana", 1, 1);
        book.set_nickname(&A, Some("Annie".into()));
        for i in 0..60 {
            book.record_call(StoredCall {
                with: vec![A],
                started_at: i,
                duration_secs: 5,
                missed: false,
            });
        }
        assert_eq!(book.calls.len(), CALL_HISTORY);
        assert_eq!(book.calls[0].started_at, 59);
        let path = std::env::temp_dir().join(format!("glance-book-{}.json", std::process::id()));
        book.save(&path).unwrap();
        assert_eq!(ContactBook::load(&path), book);
        let _ = std::fs::remove_file(&path);
        assert_eq!(
            ContactBook::load(Path::new("/nonexistent/x.json")),
            ContactBook::default()
        );
    }
}
