//! Short UI sounds, synthesized (no audio files): someone joined or left,
//! mute/unmute, deafen/undeafen. Same kalimba and room as the call tones
//! (`ringtone.rs`), in the same key, so all of glance's sounds are one
//! family: rising intervals for "on/in", falling for "off/out".

use std::sync::OnceLock;

use super::SAMPLE_RATE;
use super::synth::{self, Note, PLUCK, SMALL_ROOM, SOFT, pitch};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sound {
    Join,
    Leave,
    Mute,
    Unmute,
    Deafen,
    Undeafen,
}

pub const ALL: [Sound; 6] = [
    Sound::Join,
    Sound::Leave,
    Sound::Mute,
    Sound::Unmute,
    Sound::Deafen,
    Sound::Undeafen,
];

/// Peak level of the chimes (about -10 dBFS).
pub const PEAK: f32 = 0.32;
/// Chime length, seconds (the room's tail is faded out by then).
const LENGTH_SECS: f32 = 0.6;

fn render(sound: Sound) -> Vec<f32> {
    use pitch::*;
    let n = |at, freq, vel| Note {
        at,
        freq,
        vel,
        timbre: PLUCK,
    };
    let soft = |at, freq, vel| Note {
        at,
        freq,
        vel,
        timbre: SOFT,
    };
    let notes = match sound {
        // Join: up a fifth (the ringtone's opening interval).
        Sound::Join => vec![n(0.0, D5, 0.75), n(0.09, A5, 0.8)],
        // Leave: the same fifth, coming down.
        Sound::Leave => vec![n(0.0, A5, 0.75), n(0.09, D5, 0.8)],
        // Mute/unmute: one short tap, low for off, high for on.
        Sound::Mute => vec![soft(0.0, A4, 0.85)],
        Sound::Unmute => vec![soft(0.0, D5, 0.85)],
        // Deafen/undeafen: a falling/rising fourth, rounder and lower.
        Sound::Deafen => vec![soft(0.0, B4, 0.8), soft(0.07, FS4, 0.85)],
        Sound::Undeafen => vec![soft(0.0, FS4, 0.8), soft(0.07, B4, 0.85)],
    };
    let len = (LENGTH_SECS * SAMPLE_RATE as f32) as usize;
    let mut out = synth::render(&notes, SMALL_ROOM, len);
    // Fade the last bit of ring and room out smoothly.
    let tail = SAMPLE_RATE as usize / 6;
    for (i, s) in out[len - tail..].iter_mut().enumerate() {
        let g = 0.5 + 0.5 * (std::f32::consts::PI * i as f32 / (tail - 1) as f32).cos();
        *s *= g;
    }
    synth::normalize(&mut out, PEAK);
    out
}

static RENDERED: OnceLock<Vec<Vec<f32>>> = OnceLock::new();

fn index(sound: Sound) -> usize {
    ALL.iter().position(|&s| s == sound).expect("listed")
}

/// The sound's samples (48 kHz mono, peak about 0.32). Renders every
/// chime on first use, so callers on the UI or audio thread use
/// [`ready`] instead (after [`prewarm`]).
pub fn samples(sound: Sound) -> Vec<f32> {
    RENDERED.get_or_init(|| ALL.iter().map(|&s| render(s)).collect())[index(sound)].clone()
}

/// The sound's samples if the chimes are already rendered; never blocks.
pub fn ready(sound: Sound) -> Option<Vec<f32>> {
    RENDERED.get().map(|all| all[index(sound)].clone())
}

/// Render the chimes and both call tones on a background thread, so the
/// UI and audio threads never render them inline. Cheap to call again.
pub fn prewarm() {
    if RENDERED.get().is_some() && super::ringtone::ready(super::ringtone::Tone::Outgoing).is_some()
    {
        return;
    }
    let _ = std::thread::Builder::new()
        .name("sound-prewarm".into())
        .spawn(|| {
            samples(Sound::Join);
            super::ringtone::rendered(super::ringtone::Tone::Outgoing);
            super::ringtone::rendered(super::ringtone::Tone::Incoming);
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sounds_are_short_and_quiet() {
        for s in ALL {
            let v = samples(s);
            assert!(
                !v.is_empty() && v.len() <= SAMPLE_RATE as usize * 7 / 10,
                "{s:?}"
            );
            let peak = synth::peak(&v);
            assert!((0.05..0.4).contains(&peak), "{s:?} peak {peak}");
            // Starts from silence and fades out completely: no clicks.
            assert!(
                v[0].abs() < 1e-4 && v[v.len() - 1].abs() < 1e-4,
                "{s:?} edges"
            );
        }
        assert_ne!(samples(Sound::Join), samples(Sound::Leave));
        assert_ne!(samples(Sound::Mute), samples(Sound::Unmute));
    }
}
