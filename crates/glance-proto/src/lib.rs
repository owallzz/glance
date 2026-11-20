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
