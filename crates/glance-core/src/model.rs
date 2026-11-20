//! Types the UI renders. Networking (M3+) produces these; the demo module
//! fakes them for UI work.

use std::fmt;

/// A peer's stable identity: its iroh public key (32 bytes).
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PeerId(pub [u8; 32]);

impl PeerId {
    /// Short, human-checkable form (first 4 bytes as hex).
    pub fn short(&self) -> String {
        self.0[..4].iter().map(|b| format!("{b:02x}")).collect()
    }
}

impl fmt::Debug for PeerId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "PeerId({})", self.short())
    }
}

/// Name and look of a person, as they chose them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Profile {
    pub id: PeerId,
    pub name: String,
    /// Avatar background, 0xRRGGBB.
    pub color: u32,
}

impl Profile {
    /// Up to two initials for the avatar ("Sam Lee" -> "SL", "ana" -> "A").
    pub fn initials(&self) -> String {
        let mut out: String = self
            .name
            .split_whitespace()
            .filter_map(|w| w.chars().next())
            .take(2)
            .flat_map(char::to_uppercase)
            .collect();
        if out.is_empty() {
            out.push('?');
        }
        out
    }
}

/// Status a person shows their contacts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Presence {
    Online,
    Idle,
    DoNotDisturb,
    /// Shown to others as Offline.
    Invisible,
    Offline,
}

impl Presence {
    pub fn label(self) -> &'static str {
        match self {
            Presence::Online => "Online",
            Presence::Idle => "Idle",
            Presence::DoNotDisturb => "Do Not Disturb",
            Presence::Invisible => "Invisible",
            Presence::Offline => "Offline",
        }
    }

    /// Whether contacts can ring this person right now.
    pub fn can_ring(self) -> bool {
        matches!(self, Presence::Online | Presence::Idle)
    }

    pub fn is_online(self) -> bool {
        !matches!(self, Presence::Offline | Presence::Invisible)
    }
}

/// How we reach a peer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Route {
    Direct,
    Relayed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ConnectionInfo {
    pub route: Route,
    pub rtt_ms: u32,
}

/// A live screen (or window) share.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StreamInfo {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub has_audio: bool,
    /// What is being shared, e.g. "Screen 1" or a window title.
    pub source: String,
}

/// Someone in the current room (including you).
#[derive(Clone, Debug, PartialEq)]
pub struct Participant {
    pub profile: Profile,
    pub muted: bool,
    pub deafened: bool,
    pub speaking: bool,
    pub camera: bool,
    pub stream: Option<StreamInfo>,
    /// `None` for yourself.
    pub connection: Option<ConnectionInfo>,
    pub is_contact: bool,
}

/// Where a contact relationship stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ContactState {
    Mutual,
    /// They asked to add you.
    Incoming,
    /// You asked to add them.
    Outgoing,
    Blocked,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Contact {
    pub profile: Profile,
    pub state: ContactState,
    pub nickname: Option<String>,
    pub favorite: bool,
    pub presence: Presence,
    /// Number of people in the call they're in, if they share that.
    pub in_call: Option<u8>,
    /// They allow contacts to join their call.
    pub joinable: bool,
}

impl Contact {
    pub fn display_name(&self) -> &str {
        self.nickname.as_deref().unwrap_or(&self.profile.name)
    }
}

/// A file someone offered in chat.
#[derive(Clone, Debug, PartialEq)]
pub struct FileOffer {
    /// BLAKE3 hash of the content (iroh-blobs).
    pub hash: [u8; 32],
    pub name: String,
    pub size: u64,
    pub mime: String,
    pub transfer: Transfer,
}

impl FileOffer {
    pub fn is_image(&self) -> bool {
        self.mime.starts_with("image/")
    }
}

/// Download state of a file offer on this machine.
#[derive(Clone, Debug, PartialEq)]
pub enum Transfer {
    /// Offered, not downloaded.
    Available,
    Downloading {
        done: u64,
        bytes_per_sec: u64,
    },
    Done,
    Failed(String),
    /// You sent it.
    Sent,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ChatBody {
    Text(String),
    File(FileOffer),
    /// "Sam joined", "Ana started streaming", ...
    System(String),
}

#[derive(Clone, Debug, PartialEq)]
pub struct ChatMessage {
    pub id: u64,
    pub author: PeerId,
    /// Unix time, milliseconds.
    pub sent_at: u64,
    pub body: ChatBody,
}

/// Stream resolution choices in the Go Live picker.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Resolution {
    P720,
    P1080,
    P1440,
    P2160,
    /// The source's native size (ultrawide etc.).
    Source,
}

impl Resolution {
    pub const ALL: [Resolution; 5] = [
        Resolution::P720,
        Resolution::P1080,
        Resolution::P1440,
        Resolution::P2160,
        Resolution::Source,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Resolution::P720 => "720p",
            Resolution::P1080 => "1080p",
            Resolution::P1440 => "1440p",
            Resolution::P2160 => "4K",
            Resolution::Source => "Source",
        }
    }

    /// Height cap, or `None` for source.
    pub fn max_height(self) -> Option<u32> {
        match self {
            Resolution::P720 => Some(720),
            Resolution::P1080 => Some(1080),
            Resolution::P1440 => Some(1440),
            Resolution::P2160 => Some(2160),
            Resolution::Source => None,
        }
    }

    /// Output size for a source of `src` pixels: scaled down to the cap,
    /// keeping aspect ratio, never upscaled, dimensions even (for H.264).
    pub fn output_size(self, src: (u32, u32)) -> (u32, u32) {
        let (w, h) = src;
        let (w, h) = match self.max_height() {
            Some(cap) if h > cap => {
                let scaled_w = (w as u64 * cap as u64 / h as u64) as u32;
                (scaled_w, cap)
            }
            _ => (w, h),
        };
        (w & !1, h & !1)
    }

    /// Needs a hardware encoder to hold 60 fps.
    pub fn needs_hardware(self, fps: u32) -> bool {
        match self {
            Resolution::P720 => false,
            Resolution::P1080 => fps > 60,
            _ => true,
        }
    }
}

pub const FRAME_RATES: [u32; 4] = [15, 30, 60, 144];

/// A past call in the contacts home.
#[derive(Clone, Debug, PartialEq)]
pub struct CallRecord {
    pub with: Vec<PeerId>,
    pub started_at: u64,
    pub duration_secs: u64,
    pub missed: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile(name: &str) -> Profile {
        Profile {
            id: PeerId([1; 32]),
            name: name.into(),
            color: 0,
        }
    }

    #[test]
    fn initials() {
        assert_eq!(profile("Sam Lee").initials(), "SL");
        assert_eq!(profile("ana").initials(), "A");
        assert_eq!(profile("  ").initials(), "?");
        assert_eq!(profile("a b c").initials(), "AB");
    }

    #[test]
    fn presence_rules() {
        assert!(Presence::Online.can_ring());
        assert!(!Presence::DoNotDisturb.can_ring());
        assert!(!Presence::Invisible.is_online());
    }

    #[test]
    fn resolution_output_sizes() {
        assert_eq!(Resolution::P1080.output_size((3840, 2160)), (1920, 1080));
        assert_eq!(Resolution::P2160.output_size((1920, 1080)), (1920, 1080));
        assert_eq!(Resolution::P1440.output_size((5120, 1440)), (5120, 1440));
        assert_eq!(Resolution::P1080.output_size((5120, 1440)), (3840, 1080));
        assert_eq!(Resolution::Source.output_size((1367, 769)), (1366, 768));
        assert!(Resolution::P2160.needs_hardware(30));
        assert!(!Resolution::P720.needs_hardware(60));
    }

    #[test]
    fn peer_id_short_is_hex_prefix() {
        let mut b = [0u8; 32];
        b[..4].copy_from_slice(&[0xde, 0xad, 0xbe, 0xef]);
        assert_eq!(PeerId(b).short(), "deadbeef");
    }
}
