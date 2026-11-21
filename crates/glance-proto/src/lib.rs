//! Wire types shared by every glance peer: room tickets, the room control
//! protocol, and length-prefixed postcard framing.
//!
//! The protocol is versioned by ALPN ([`ROOM_ALPN`]); a breaking change gets
//! a new ALPN rather than in-band negotiation.

pub mod contacts;
pub mod frame;
pub mod room;
pub mod ticket;

pub use contacts::*;
pub use frame::{FrameError, MAX_FRAME, MAX_VIDEO_FRAME, decode, encode, encode_with_limit};
pub use room::*;
pub use ticket::{RoomId, Ticket, TicketError};

pub use iroh_base::{EndpointAddr, EndpointId};

/// ALPN for the room mesh (control stream per peer).
pub const ROOM_ALPN: &[u8] = b"glance/room/1";

/// ALPN for room liveness probes ([`ProbeRequest`] / [`ProbeReply`]). A
/// separate protocol so peers that predate it simply refuse the connection,
/// which reads as "unknown".
pub const ROOM_PROBE_ALPN: &[u8] = b"glance/room-probe/1";

/// Cut `s` to at most `max` bytes on a character boundary.
pub fn truncate_utf8(s: &mut String, max: usize) {
    if s.len() > max {
        let mut end = max;
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        s.truncate(end);
    }
}

#[cfg(test)]
mod tests {
    use super::truncate_utf8;

    #[test]
    fn truncates_on_char_boundaries() {
        let mut s = "aé".repeat(3);
        truncate_utf8(&mut s, 4);
        assert_eq!(s, "aéa");
        let mut s = "short".to_string();
        truncate_utf8(&mut s, 10);
        assert_eq!(s, "short");
    }
}
