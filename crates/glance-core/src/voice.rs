//! Your own mute/deafen state and the per-person audio choices you make for
//! others (volume, local mute). Pure logic; the app saves [`PeerAudio`] in
//! settings.json.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::model::PeerId;

/// Your mic and speakers, with Discord's rules: deafening also mutes you,
/// undeafening puts your mic back the way it was, and unmuting while
/// deafened undeafens too.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SelfVoice {
    pub muted: bool,
    pub deafened: bool,
    /// Whether you were muted before deafening (restored on undeafen).
    muted_before_deafen: bool,
}

impl SelfVoice {
    pub fn set_muted(&mut self, muted: bool) {
        self.muted = muted;
        if !muted && self.deafened {
            self.deafened = false;
        }
    }

    pub fn set_deafened(&mut self, deafened: bool) {
        if deafened == self.deafened {
            return;
        }
        if deafened {
            self.muted_before_deafen = self.muted;
            self.muted = true;
        } else {
            self.muted = self.muted_before_deafen;
        }
        self.deafened = deafened;
    }
}

/// What you chose for one person. Defaults (100%, not muted) aren't stored.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PeerAudioPrefs {
    /// Their voice, 0.0–2.0.
    pub volume: f32,
    /// Silenced for you only (their volume is kept for when you unmute).
    pub muted: bool,
    /// The audio shared with their screen, 0.0–2.0.
    pub stream_volume: f32,
    pub stream_muted: bool,
}

impl Default for PeerAudioPrefs {
    fn default() -> Self {
        Self {
            volume: 1.0,
            muted: false,
            stream_volume: 1.0,
            stream_muted: false,
        }
    }
}

/// Per-person audio choices, keyed by peer id (hex, for JSON).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PeerAudio(BTreeMap<String, PeerAudioPrefs>);

fn hex(id: PeerId) -> String {
    id.0.iter().map(|b| format!("{b:02x}")).collect()
}

impl PeerAudio {
    pub fn get(&self, id: PeerId) -> PeerAudioPrefs {
        self.0.get(&hex(id)).copied().unwrap_or_default()
    }

    /// Change one person's choices; drops them again once back to defaults.
    pub fn update(&mut self, id: PeerId, f: impl FnOnce(&mut PeerAudioPrefs)) -> PeerAudioPrefs {
        let mut prefs = self.get(id);
        f(&mut prefs);
        prefs.volume = prefs.volume.clamp(0.0, 2.0);
        prefs.stream_volume = prefs.stream_volume.clamp(0.0, 2.0);
        if prefs == PeerAudioPrefs::default() {
            self.0.remove(&hex(id));
        } else {
            self.0.insert(hex(id), prefs);
        }
        prefs
    }

    /// What their voice is mixed at: 0 while muted, else their volume.
    pub fn voice_gain(&self, id: PeerId) -> f32 {
        let p = self.get(id);
        if p.muted { 0.0 } else { p.volume }
    }

    pub fn stream_gain(&self, id: PeerId) -> f32 {
        let p = self.get(id);
        if p.stream_muted { 0.0 } else { p.stream_volume }
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Everyone with saved choices (entries with a malformed id are skipped).
    pub fn iter(&self) -> impl Iterator<Item = (PeerId, PeerAudioPrefs)> + '_ {
        self.0
            .iter()
            .filter_map(|(k, prefs)| Some((parse_hex(k)?, *prefs)))
    }
}

fn parse_hex(s: &str) -> Option<PeerId> {
    if s.len() != 64 || !s.is_ascii() {
        return None;
    }
    let mut id = [0u8; 32];
    for (i, b) in id.iter_mut().enumerate() {
        *b = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(PeerId(id))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iter_lists_saved_people_and_skips_bad_ids() {
        let mut a = PeerAudio::default();
        a.update(PeerId([7; 32]), |p| p.muted = true);
        a.update(PeerId([9; 32]), |p| p.stream_volume = 0.5);
        a.0.insert("not-hex".into(), PeerAudioPrefs::default());
        let got: Vec<_> = a.iter().collect();
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].0, PeerId([7; 32]));
        assert!(got[0].1.muted);
        assert_eq!(got[1].0, PeerId([9; 32]));
        assert_eq!(got[1].1.stream_volume, 0.5);
    }

    #[test]
    fn deafen_mutes_and_undeafen_restores_previous_mute() {
        let mut v = SelfVoice::default();
        v.set_deafened(true);
        assert!(v.muted && v.deafened);
        v.set_deafened(false);
        assert!(!v.muted && !v.deafened, "wasn't muted before deafening");

        v.set_muted(true);
        v.set_deafened(true);
        v.set_deafened(false);
        assert!(v.muted && !v.deafened, "was muted before deafening");
    }

    #[test]
    fn unmuting_while_deafened_undeafens() {
        let mut v = SelfVoice::default();
        v.set_deafened(true);
        v.set_muted(false);
        assert!(!v.muted && !v.deafened);
        // Muting while deafened keeps you deafened.
        v.set_deafened(true);
        v.set_muted(true);
        assert!(v.muted && v.deafened);
    }

    #[test]
    fn deafening_twice_keeps_the_remembered_mute() {
        let mut v = SelfVoice::default();
        v.set_deafened(true);
        v.set_deafened(true);
        v.set_deafened(false);
        assert!(!v.muted);
    }

    #[test]
    fn mute_keeps_volume_and_defaults_are_not_stored() {
        let a = PeerId([1; 32]);
        let mut book = PeerAudio::default();
        book.update(a, |p| p.volume = 0.5);
        book.update(a, |p| p.muted = true);
        assert_eq!(book.voice_gain(a), 0.0);
        assert_eq!(book.get(a).volume, 0.5);
        book.update(a, |p| p.muted = false);
        assert_eq!(book.voice_gain(a), 0.5);
        book.update(a, |p| p.volume = 1.0);
        assert!(book.is_empty());
        book.update(a, |p| p.volume = 9.0);
        assert_eq!(book.get(a).volume, 2.0);
    }

    #[test]
    fn peer_audio_survives_a_json_round_trip() {
        let (a, b) = (PeerId([1; 32]), PeerId([0xab; 32]));
        let mut book = PeerAudio::default();
        book.update(a, |p| p.muted = true);
        book.update(b, |p| {
            p.volume = 1.5;
            p.stream_muted = true;
        });
        let json = serde_json::to_string(&book).unwrap();
        assert!(json.contains(&"ab".repeat(32)));
        let back: PeerAudio = serde_json::from_str(&json).unwrap();
        assert_eq!(back, book);
        assert!(back.get(a).muted);
        assert_eq!(back.stream_gain(b), 0.0);
        assert_eq!(back.get(PeerId([2; 32])), PeerAudioPrefs::default());
        // Old or partial entries fill in defaults.
        let partial: PeerAudio =
            serde_json::from_str(&format!(r#"{{"{}":{{"muted":true}}}}"#, "01".repeat(32)))
                .unwrap();
        assert_eq!(partial.get(a).volume, 1.0);
        assert!(partial.get(a).muted);
    }
}
