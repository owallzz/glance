//! A tiny synthesizer for glance's own sounds (no audio files to ship).
//!
//! Every sound is a few plucked notes: each note is a handful of decaying
//! sine partials (modal synthesis, like a kalimba tine or a soft mallet on
//! a wooden bar), mixed with a small mono Freeverb-style room, then a gentle
//! low-pass (nothing harsh above ~8 kHz) and a DC blocker. Rendering is
//! deterministic and takes a few milliseconds, so callers render once and
//! cache the result.

use super::SAMPLE_RATE;

const SR: f32 = SAMPLE_RATE as f32;

/// One mode of a struck/plucked object.
#[derive(Clone, Copy, Debug)]
pub struct Partial {
    /// Frequency as a multiple of the note's pitch.
    pub ratio: f32,
    /// Relative amplitude.
    pub amp: f32,
    /// Decay time constant at 440 Hz, seconds (higher notes decay faster).
    pub decay: f32,
}

/// A note's sound: its partials and attack time.
#[derive(Clone, Copy, Debug)]
pub struct Timbre {
    pub partials: &'static [Partial],
    /// Raised-cosine attack, seconds (a few ms: soft, but still a pluck).
    pub attack: f32,
}

/// Warm kalimba/bell pluck: a strong fundamental with a slightly detuned
/// twin (slow shimmer), a soft octave, and short-lived wooden overtones
/// that give the attack its "tock".
pub const PLUCK: Timbre = Timbre {
    partials: &[
        Partial {
            ratio: 1.0,
            amp: 1.0,
            decay: 0.62,
        },
        Partial {
            ratio: 1.0017,
            amp: 0.32,
            decay: 0.5,
        },
        Partial {
            ratio: 2.0,
            amp: 0.17,
            decay: 0.32,
        },
        Partial {
            ratio: 3.98,
            amp: 0.085,
            decay: 0.11,
        },
        Partial {
            ratio: 6.27,
            amp: 0.028,
            decay: 0.045,
        },
    ],
    attack: 0.0025,
};

/// Rounder, softer pluck for the outgoing ringback and the low "bed" notes:
/// almost a pure tone with a breath of octave.
pub const SOFT: Timbre = Timbre {
    partials: &[
        Partial {
            ratio: 1.0,
            amp: 1.0,
            decay: 0.55,
        },
        Partial {
            ratio: 1.0021,
            amp: 0.25,
            decay: 0.45,
        },
        Partial {
            ratio: 2.0,
            amp: 0.07,
            decay: 0.25,
        },
        Partial {
            ratio: 3.0,
            amp: 0.025,
            decay: 0.12,
        },
    ],
    attack: 0.006,
};

/// A note in a phrase.
#[derive(Clone, Copy, Debug)]
pub struct Note {
    /// Start, seconds.
    pub at: f32,
    /// Pitch, Hz.
    pub freq: f32,
    /// Loudness 0–1 (also brightens the overtones a little).
    pub vel: f32,
    pub timbre: Timbre,
}

/// Room settings.
#[derive(Clone, Copy, Debug)]
pub struct Room {
    /// 0–1, longer tails when larger.
    pub size: f32,
    /// 0–1, darker tails when larger.
    pub damp: f32,
    /// Wet level relative to the dry signal.
    pub wet: f32,
}

/// Small, warm room for the ringtone and ringback (RT60 about 1 s).
pub const ROOM: Room = Room {
    size: 0.5,
    damp: 0.45,
    wet: 0.32,
};

/// Smaller room for the short UI chimes.
pub const SMALL_ROOM: Room = Room {
    size: 0.28,
    damp: 0.5,
    wet: 0.22,
};

/// Render `notes` into `len` samples: dry plucks, room, tone shaping.
pub fn render(notes: &[Note], room: Room, len: usize) -> Vec<f32> {
    let mut dry = vec![0.0f32; len];
    for n in notes {
        pluck(&mut dry, n);
    }
    let mut out = reverb(&dry, room);
    lowpass(&mut out, 7_000.0);
    dc_block(&mut out);
    out
}

/// Add one note into `out`.
fn pluck(out: &mut [f32], n: &Note) {
    let start = (n.at * SR).round() as usize;
    if start >= out.len() {
        return;
    }
    let attack = (n.timbre.attack * SR).max(1.0);
    // Higher notes ring shorter, as real bars and tines do.
    let pitch_scale = (440.0 / n.freq).sqrt();
    for p in n.timbre.partials {
        let freq = n.freq * p.ratio;
        if freq > 8_000.0 {
            continue;
        }
        // Soft hits are darker: overtones scale with velocity.
        let amp = p.amp * n.vel * if p.ratio > 1.5 { n.vel } else { 1.0 };
        let tau = p.decay * pitch_scale * SR;
        let w = std::f64::consts::TAU * freq as f64 / SR as f64;
        // Until the partial is ~-80 dB down.
        let ring = ((tau * 9.2) as usize).min(out.len() - start);
        // Recurrences instead of exp()/sin() per sample (f64: no drift over
        // a few hundred thousand steps).
        let decay = (-1.0 / tau as f64).exp();
        let mut env = 1.0f64;
        let (sw, cw) = w.sin_cos();
        let (mut s, mut c) = (0.0f64, 1.0f64);
        for (i, o) in out[start..start + ring].iter_mut().enumerate() {
            let a = if (i as f32) < attack {
                0.5 - 0.5 * (std::f32::consts::PI * i as f32 / attack).cos()
            } else {
                1.0
            };
            *o += amp * a * (env * s) as f32;
            env *= decay;
            let ns = s * cw + c * sw;
            c = c * cw - s * sw;
            s = ns;
        }
    }
}

struct Comb {
    buf: Vec<f32>,
    idx: usize,
    store: f32,
}

struct Allpass {
    buf: Vec<f32>,
    idx: usize,
}

/// Mono Freeverb: eight damped combs in parallel, four allpasses in series,
/// after a short pre-delay. Returns dry + wet.
fn reverb(dry: &[f32], room: Room) -> Vec<f32> {
    // Freeverb's tunings are for 44.1 kHz.
    let scale = SR / 44_100.0;
    let at = |n: usize| ((n as f32 * scale) as usize).max(1);
    let mut combs: Vec<Comb> = [1116, 1188, 1277, 1356, 1422, 1491, 1557, 1617]
        .iter()
        .map(|&n| Comb {
            buf: vec![0.0; at(n)],
            idx: 0,
            store: 0.0,
        })
        .collect();
    let mut allpasses: Vec<Allpass> = [556, 441, 341, 225]
        .iter()
        .map(|&n| Allpass {
            buf: vec![0.0; at(n)],
            idx: 0,
        })
        .collect();
    let feedback = room.size * 0.28 + 0.7;
    let damp = room.damp * 0.4;
    let predelay = (0.012 * SR) as usize;
    // Freeverb's input gain, so `wet` is roughly relative to the dry level.
    let gain = 0.015 * 3.0;
    let mut out = Vec::with_capacity(dry.len());
    for i in 0..dry.len() {
        let x = if i >= predelay {
            dry[i - predelay]
        } else {
            0.0
        } * gain;
        let mut acc = 0.0;
        for c in combs.iter_mut() {
            let y = c.buf[c.idx];
            c.store = y * (1.0 - damp) + c.store * damp;
            c.buf[c.idx] = x + c.store * feedback;
            c.idx = (c.idx + 1) % c.buf.len();
            acc += y;
        }
        for a in allpasses.iter_mut() {
            let b = a.buf[a.idx];
            a.buf[a.idx] = acc + b * 0.5;
            a.idx = (a.idx + 1) % a.buf.len();
            acc = b - acc;
        }
        out.push(dry[i] + acc * room.wet);
    }
    out
}

/// One-pole low-pass at `cutoff` Hz, run twice (12 dB/octave).
fn lowpass(x: &mut [f32], cutoff: f32) {
    let a = 1.0 - (-std::f32::consts::TAU * cutoff / SR).exp();
    for _ in 0..2 {
        let mut y = 0.0f32;
        for s in x.iter_mut() {
            y += a * (*s - y);
            *s = y;
        }
    }
}

/// High-pass at ~25 Hz to remove any DC.
fn dc_block(x: &mut [f32]) {
    let r = 1.0 - std::f32::consts::TAU * 25.0 / SR;
    let (mut x1, mut y1) = (0.0f32, 0.0f32);
    for s in x.iter_mut() {
        let y = *s - x1 + r * y1;
        x1 = *s;
        y1 = y;
        *s = y;
    }
}

/// Peak absolute sample.
pub fn peak(x: &[f32]) -> f32 {
    x.iter().fold(0.0f32, |m, s| m.max(s.abs()))
}

/// Scale `x` so its peak is `target`.
pub fn normalize(x: &mut [f32], target: f32) {
    let p = peak(x);
    if p > 0.0 {
        let g = target / p;
        x.iter_mut().for_each(|s| *s *= g);
    }
}

/// Fold a one-shot render (`resp`, longer than `period`) into a seamless
/// loop of `period` samples: every sample is what you'd hear in steady
/// state, with the tails of earlier repeats ringing on under it, so the
/// end runs straight into the start without a click.
pub fn fold_loop(resp: &[f32], period: usize) -> Vec<f32> {
    let mut out = vec![0.0f32; period];
    for (i, s) in resp.iter().enumerate() {
        out[i % period] += s;
    }
    out
}

/// Note pitches (equal temperament, A4 = 440 Hz).
pub mod pitch {
    pub const D4: f32 = 293.66;
    pub const FS4: f32 = 369.99;
    pub const A4: f32 = 440.0;
    pub const B4: f32 = 493.88;
    pub const CS5: f32 = 554.37;
    pub const D5: f32 = 587.33;
    pub const E5: f32 = 659.26;
    pub const FS5: f32 = 739.99;
    pub const A5: f32 = 880.0;
    pub const B5: f32 = 987.77;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pluck_is_smooth_and_decays() {
        let note = Note {
            at: 0.0,
            freq: pitch::A5,
            vel: 1.0,
            timbre: PLUCK,
        };
        let x = render(&[note], ROOM, SAMPLE_RATE as usize * 3);
        assert!(x[0].abs() < 1e-3, "starts from silence");
        let early = peak(&x[..4800]);
        let late = peak(&x[x.len() - 4800..]);
        assert!(early > 0.3, "{early}");
        assert!(late < early * 0.01, "decays: {late} vs {early}");
        let dc = x.iter().sum::<f32>() / x.len() as f32;
        assert!(dc.abs() < 1e-3, "dc {dc}");
    }

    #[test]
    fn folded_loop_is_periodic() {
        let resp: Vec<f32> = (0..30).map(|i| i as f32).collect();
        let l = fold_loop(&resp, 10);
        assert_eq!(l[0], 0.0 + 10.0 + 20.0);
        assert_eq!(l[9], 9.0 + 19.0 + 29.0);
    }
}
