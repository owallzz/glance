//! The contacts protocol (ALPN [`CONTACTS_ALPN`]): contact requests,
//! presence, ringing, and asking to join a contact's call.
//!
//! One connection per pair of apps, one bidirectional stream, frames are
//! [`ContactMsg`]. The dialer sends [`ContactMsg::Hello`] first.

use serde::{Deserialize, Serialize};

use crate::room::{WireFile, WireProfile};

pub const CONTACTS_ALPN: &[u8] = b"glance/contacts/1";

/// Status shown to contacts.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum WirePresence {
    Online,
    Idle,
    DoNotDisturb,
}

/// What a contact shares about themselves while connected.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PresenceInfo {
    pub presence: WirePresence,
    /// People in the call they're in, if any.
    pub in_call: Option<u8>,
    /// They let contacts join their call.
    pub joinable: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ContactMsg {
    /// First frame from the dialer: who I am and my status (status is only
    /// meaningful between mutual contacts).
    Hello {
        profile: WireProfile,
        presence: Option<PresenceInfo>,
    },
    /// "Add me as a contact."
    Request {
        profile: WireProfile,
    },
    /// Accept a request; we're now mutual contacts.
    Accept {
        profile: WireProfile,
    },
    /// Cancel a request, decline one, or remove an existing contact.
    Remove,
    Presence(PresenceInfo),
    /// Ring: join me in this room.
    Ring {
        call_id: u64,
        /// The room link (`glance://join/...`).
        link: String,
        /// Names of people already in the call.
        members: Vec<String>,
    },
    /// The caller gave up (or the call ended) before an answer.
    CancelRing {
        call_id: u64,
    },
    /// The callee's answer.
    RingAnswer {
        call_id: u64,
        accepted: bool,
    },
    /// "Can I join your call?" (only when you're joinable).
    JoinRequest,
    /// Reply to [`ContactMsg::JoinRequest`].
    JoinTicket {
        link: Option<String>,
    },
    /// A file for you (fetch it from the sender's blob store by hash).
    File(WireFile),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_round_trip() {
        for m in [
            ContactMsg::Request {
                profile: WireProfile {
                    name: "Sam".into(),
                    color: 1,
                },
            },
            ContactMsg::Ring {
                call_id: 9,
                link: "glance://join/x".into(),
                members: vec!["Ana".into()],
            },
            ContactMsg::File(WireFile {
                hash: [7; 32],
                name: "notes.txt".into(),
                size: 12,
                mime: "text/plain".into(),
            }),
            ContactMsg::Presence(PresenceInfo {
                presence: WirePresence::DoNotDisturb,
                in_call: Some(3),
                joinable: true,
            }),
        ] {
            let f = crate::encode(&m).unwrap();
            assert_eq!(crate::decode::<ContactMsg>(&f[4..]).unwrap(), m);
        }
    }
}
