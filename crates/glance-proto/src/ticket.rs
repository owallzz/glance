//! Room tickets: everything needed to join a room, as a shareable link.
//!
//! `glance://join/<base32>` where the payload is postcard
//! `(version, room id, room secret, host address)`. Holding a ticket is the
//! permission to join: peers prove knowledge of the secret on connect.

use std::fmt;
use std::str::FromStr;

use data_encoding::BASE32_NOPAD;
use iroh_base::EndpointAddr;
use serde::{Deserialize, Serialize};

const PREFIX: &str = "glance://join/";
const VERSION: u8 = 1;

/// Random room identifier.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct RoomId(pub [u8; 8]);

impl fmt::Debug for RoomId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "RoomId({})",
            data_encoding::HEXLOWER.encode(&self.0[..4])
        )
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ticket {
    pub room: RoomId,
    /// 128-bit room secret; only holders of the link know it.
    pub secret: [u8; 16],
    /// Any current member to dial first (usually whoever made the link).
    pub host: EndpointAddr,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum TicketError {
    #[error("not a glance link")]
    NotBase32,
    #[error("this link is damaged or incomplete")]
    Malformed,
    #[error("this link is from a newer version of glance")]
    UnsupportedVersion,
}

#[derive(Serialize, Deserialize)]
struct Wire {
    version: u8,
    room: RoomId,
    secret: [u8; 16],
    host: EndpointAddr,
}

impl Ticket {
    /// The shareable link.
    pub fn to_link(&self) -> String {
        let wire = Wire {
            version: VERSION,
            room: self.room,
            secret: self.secret,
            host: self.host.clone(),
        };
        let bytes = postcard::to_stdvec(&wire).expect("ticket serializes");
        format!(
            "{PREFIX}{}",
            BASE32_NOPAD.encode(&bytes).to_ascii_lowercase()
        )
    }

    /// Parse a link or a bare code, tolerating whitespace and case.
    pub fn parse(input: &str) -> Result<Self, TicketError> {
        let s = input.trim();
        let code = s.strip_prefix(PREFIX).unwrap_or(s);
        let code: String = code
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect::<String>()
            .to_ascii_uppercase();
        let bytes = BASE32_NOPAD
            .decode(code.as_bytes())
            .map_err(|_| TicketError::NotBase32)?;
        let version = *bytes.first().ok_or(TicketError::Malformed)?;
        if version > VERSION {
            return Err(TicketError::UnsupportedVersion);
        }
        if version != VERSION {
            return Err(TicketError::Malformed);
        }
        let wire: Wire = postcard::from_bytes(&bytes).map_err(|_| TicketError::Malformed)?;
        Ok(Ticket {
            room: wire.room,
            secret: wire.secret,
            host: wire.host,
        })
    }
}

impl fmt::Display for Ticket {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_link())
    }
}

impl FromStr for Ticket {
    type Err = TicketError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ticket::parse(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use iroh_base::SecretKey;

    fn ticket() -> Ticket {
        let key = SecretKey::from_bytes(&[7; 32]);
        Ticket {
            room: RoomId([1; 8]),
            secret: [2; 16],
            host: EndpointAddr::new(key.public()).with_ip_addr("192.0.2.1:4433".parse().unwrap()),
        }
    }

    #[test]
    fn link_round_trips() {
        let t = ticket();
        let link = t.to_link();
        assert!(link.starts_with("glance://join/"));
        assert_eq!(Ticket::parse(&link).unwrap(), t);
    }

    #[test]
    fn bare_code_with_whitespace_and_case_parses() {
        let t = ticket();
        let code = t.to_link().trim_start_matches(PREFIX).to_ascii_uppercase();
        let messy = format!("  {}\n{}  ", &code[..10], &code[10..]);
        assert_eq!(Ticket::parse(&messy).unwrap(), t);
    }

    #[test]
    fn relay_only_link_is_short() {
        let key = SecretKey::from_bytes(&[7; 32]);
        let relay: iroh_base::RelayUrl = "https://euc1-1.relay.n0.iroh-canary.iroh.link./"
            .parse()
            .unwrap();
        let t = Ticket {
            room: RoomId([1; 8]),
            secret: [2; 16],
            host: EndpointAddr::from_parts(key.public(), [iroh_base::TransportAddr::Relay(relay)]),
        };
        assert!(t.to_link().len() < 200, "{}", t.to_link().len());
    }

    #[test]
    fn errors_are_friendly() {
        assert_eq!(Ticket::parse("hello!"), Err(TicketError::NotBase32));
        assert_eq!(
            Ticket::parse("glance://join/ai"),
            Err(TicketError::UnsupportedVersion)
        );
        assert_eq!(Ticket::parse(""), Err(TicketError::Malformed));
    }
}
