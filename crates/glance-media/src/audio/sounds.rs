//! Short UI sounds, synthesized (no audio files): someone joined or left,
//! mute/unmute, deafen/undeafen. Our own, soft, sine-based chimes.

use super::SAMPLE_RATE;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sound {
    Join,
    Leave,
    Mute,
    Unmute,
    Deafen,
    Undeafen,
}

/// The sound's samples (48 kHz mono, peak about 0.25).
pub fn samples(sound: Sound) -> Vec<f32> {
    // (frequency Hz, start ms, length ms) notes.
    let notes: &[(f32, u32, u32)] = match sound {
        Sound::Join => &[(659.3, 0, 120), (880.0, 90, 180)],
        Sound::Leave => &[(880.0, 0, 120), (587.3, 90, 200)],
        Sound::Mute => &[(392.0, 0, 70)],
        Sound::Unmute => &[(523.3, 0, 70)],
        Sound::Deafen => &[(440.0, 0, 70), (329.6, 60, 90)],
        Sound::Undeafen => &[(329.6, 0, 70), (440.0, 60, 90)],
    };
    let ms = |m: u32| (m * SAMPLE_RATE / 1000) as usize;
    let len = notes.iter().map(|&(_, s, l)| ms(s + l)).max().unwrap_or(0);
    let mut out = vec![0.0f32; len];
    for &(freq, start, length) in notes {
        let (start, length) = (ms(start), ms(length));
        for i in 0..length {
            let t = i as f32 / SAMPLE_RATE as f32;
            // Quick attack, exponential decay: a soft bell.
            let attack = (i as f32 / ms(5) as f32).min(1.0);
            let decay = (-(i as f32) / (length as f32 * 0.35)).exp();
            let s = (std::f32::consts::TAU * freq * t).sin()
                + 0.25 * (std::f32::consts::TAU * freq * 2.0 * t).sin();
            out[start + i] += 0.2 * attack * decay * s;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sounds_are_short_and_quiet() {
        for s in [
            Sound::Join,
            Sound::Leave,
            Sound::Mute,
            Sound::Unmute,
            Sound::Deafen,
            Sound::Undeafen,
        ] {
            let v = samples(s);
            assert!(!v.is_empty() && v.len() < SAMPLE_RATE as usize / 2, "{s:?}");
            let peak = v.iter().fold(0.0f32, |m, x| m.max(x.abs()));
            assert!((0.05..0.4).contains(&peak), "{s:?} peak {peak}");
        }
        assert_ne!(samples(Sound::Join), samples(Sound::Leave));
    }
}
