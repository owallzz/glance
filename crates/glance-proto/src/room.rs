//! The room control protocol.
//!
//! Every pair of members has one QUIC connection on [`crate::ROOM_ALPN`]
//! with one bidirectional control stream. The dialer sends [`Hello`] first;
//! the acceptor checks the proof and answers with [`Control::Welcome`]. After
//! that both sides send [`Control`] frames.

use iroh_base::{EndpointAddr, EndpointId};
use serde::{Deserialize, Serialize};

use crate::ticket::RoomId;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WireProfile {
    pub name: String,
    pub color: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WireStream {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub has_audio: bool,
    pub source: String,
}

/// What a member is doing; sent whole whenever any of it changes.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemberState {
    pub muted: bool,
    pub deafened: bool,
    pub camera: bool,
    pub stream: Option<WireStream>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemberInfo {
    pub addr: EndpointAddr,
    pub profile: WireProfile,
}

/// First frame from the dialing side.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hello {
    pub room: RoomId,
    /// [`join_proof`] for (dialer -> acceptor).
    pub proof: [u8; 32],
    pub profile: WireProfile,
    pub state: MemberState,
    /// The dialer's reachable address, so others it doesn't know yet can be told.
    pub addr: EndpointAddr,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Control {
    /// Acceptor's reply to a valid [`Hello`]: its own profile/state and the
    /// other members it knows, so the newcomer can dial them.
    Welcome {
        profile: WireProfile,
        state: MemberState,
        members: Vec<MemberInfo>,
    },
    /// The proof was wrong or the room is unknown; the connection closes.
    Rejected,
    State(MemberState),
    Profile(WireProfile),
    Chat {
        id: u64,
        sent_at: u64,
        text: String,
    },
    /// A member the sender just met, for peers that may not know them yet.
    Introduce(MemberInfo),
    Ping(u64),
    Pong(u64),
    Leave,
    /// Viewer -> sharer: start sending me your screen.
    Watch,
    /// Viewer -> sharer: stop sending it.
    Unwatch,
    /// Viewer -> sharer: I lost packets; send a keyframe.
    KeyframeRequest,
}

/// First byte of a unidirectional stream: what it carries.
pub const UNI_VIDEO: u8 = 1;

/// One encoded picture on a video stream (sharer -> viewer).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VideoPacket {
    pub ts_us: u64,
    pub keyframe: bool,
    pub width: u32,
    pub height: u32,
    /// Annex-B H.264 access unit.
    pub data: Vec<u8>,
}

/// Proof that `from` knows the room secret, bound to the pair of endpoints so
/// it can't be replayed to someone else.
pub fn join_proof(secret: &[u8; 16], room: RoomId, from: EndpointId, to: EndpointId) -> [u8; 32] {
    let key = blake3::derive_key("glance room secret v1", secret);
    let mut h = blake3::Hasher::new_keyed(&key);
    h.update(b"glance-join-v1");
    h.update(&room.0);
    h.update(from.as_bytes());
    h.update(to.as_bytes());
    *h.finalize().as_bytes()
}

/// Constant-time check of a received proof.
pub fn verify_proof(
    secret: &[u8; 16],
    room: RoomId,
    from: EndpointId,
    to: EndpointId,
    proof: &[u8; 32],
) -> bool {
    blake3::Hash::from(join_proof(secret, room, from, to)) == blake3::Hash::from(*proof)
}

#[cfg(test)]
mod tests {
    use super::*;
    use iroh_base::SecretKey;

    #[test]
    fn proof_is_bound_to_direction_and_secret() {
        let a = SecretKey::from_bytes(&[1; 32]).public();
        let b = SecretKey::from_bytes(&[2; 32]).public();
        let room = RoomId([3; 8]);
        let secret = [4; 16];
        let p = join_proof(&secret, room, a, b);
        assert!(verify_proof(&secret, room, a, b, &p));
        assert!(!verify_proof(&secret, room, b, a, &p));
        assert!(!verify_proof(&[5; 16], room, a, b, &p));
        assert!(!verify_proof(&secret, RoomId([0; 8]), a, b, &p));
    }

    #[test]
    fn control_frames_round_trip() {
        let msg = Control::Chat {
            id: 3,
            sent_at: 99,
            text: "hi".into(),
        };
        let f = crate::encode(&msg).unwrap();
        assert_eq!(crate::decode::<Control>(&f[4..]).unwrap(), msg);
    }
}
