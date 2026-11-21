//! The contacts protocol (ALPN [`CONTACTS_ALPN`]): contact requests,
//! presence, ringing, and asking to join a contact's call.
//!
//! One connection per pair of apps, one bidirectional stream, frames are
//! [`ContactMsg`]. The dialer sends [`ContactMsg::Hello`] first.
//!
//! Versions: `glance/contacts/2` added direct messages ([`ContactMsg::Dm`]
//! and [`ContactMsg::DmAck`]). Peers accept both ALPNs and dial offering
//! both (2 preferred), so 0.1.0 apps still get presence, rings and files;
//! DM frames only go over a version 2 connection (an older peer would fail
//! to decode them). New variants are only ever appended, so version 1
//! messages encode the same on both.

use serde::{Deserialize, Serialize};

use crate::room::{WireFile, WireProfile};

/// The current contacts protocol (with direct messages).
pub const CONTACTS_ALPN: &[u8] = b"glance/contacts/2";
/// The 0.1.0 contacts protocol (no direct messages), still accepted.
pub const CONTACTS_ALPN_V1: &[u8] = b"glance/contacts/1";

/// Longest direct message, in bytes (longer ones are cut by the sender and
/// dropped by the receiver).
pub const MAX_DM_LEN: usize = 4000;

/// A direct message between mutual contacts.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WireDm {
    /// Random, chosen by the sender; the receiver dedupes resends by it.
    pub id: u64,
    /// Unix time in milliseconds, by the sender's clock.
    pub sent_at: u64,
    pub text: String,
}

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
    // ---- glance/contacts/2 ----
    /// A direct message (version 2 connections only).
    Dm(WireDm),
    /// "Got your message `id`" (version 2 connections only).
    DmAck {
        id: u64,
    },
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
            ContactMsg::Dm(WireDm {
                id: u64::MAX,
                sent_at: 1_700_000_000_000,
                text: "hi there\nsecond line".into(),
            }),
            ContactMsg::DmAck { id: 42 },
        ] {
            let f = crate::encode(&m).unwrap();
            assert_eq!(crate::decode::<ContactMsg>(&f[4..]).unwrap(), m);
        }
    }

    /// Version 1 messages keep their encoding (variants are only appended),
    /// so a 0.1.0 peer still decodes everything we send it.
    #[test]
    fn version_1_encoding_is_unchanged() {
        assert_eq!(crate::encode(&ContactMsg::Remove).unwrap()[4..], [3]);
        assert_eq!(crate::encode(&ContactMsg::JoinRequest).unwrap()[4..], [8]);
        // The version 2 messages come after `File` (10).
        assert_eq!(
            crate::encode(&ContactMsg::DmAck { id: 1 }).unwrap()[4..],
            [12, 1]
        );
    }
}
