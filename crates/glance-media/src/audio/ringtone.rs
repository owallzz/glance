//! Call tones, synthesized (no audio files to ship): the incoming ringtone
//! and the softer outgoing ringback the caller hears while waiting.
//!
//! Incoming: a five-note kalimba phrase in D major (D5 A5 F#5 B5, landing on
//! A5) over a soft low D, in a small warm room; 3 s per ring, the notes take
//! the first second and the rest is the room's tail fading into a pause.
//! The first ring fades in so it isn't startling.
//!
//! Outgoing: two round, quiet notes (A4 then D5) every 3 s, from the same
//! instrument and room, so both ends sound like one family.
//!
//! Both loops are rendered once, folded so the reverb tail of each ring runs
//! under the next (no click at the loop point), and cached.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};

use anyhow::Result;

use super::SAMPLE_RATE;
use super::synth::{self, Note, PLUCK, ROOM, SOFT, pitch};

/// Which call tone.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tone {
    /// Someone is calling you.
    Incoming,
    /// You're calling someone and they haven't answered yet.
    Outgoing,
}

/// Seconds per ring.
pub const PERIOD_SECS: f32 = 3.0;
/// Peak level of the incoming ringtone (-6 dBFS).
pub const INCOMING_PEAK: f32 = 0.5;
/// Peak level of the ringback (about -11 dBFS: it plays under a call).
pub const OUTGOING_PEAK: f32 = 0.28;

/// A rendered tone: a first ring (faded in) then a loop played forever.
pub struct Rendered {
    pub first: Vec<f32>,
    pub looped: Vec<f32>,
}

impl Rendered {
    /// Sample `i` of the tone from its start.
    pub fn sample(&self, i: usize) -> f32 {
        match self.first.get(i) {
            Some(s) => *s,
            None => self.looped[(i - self.first.len()) % self.looped.len()],
        }
    }
}

fn incoming_notes() -> Vec<Note> {
    use pitch::*;
    let n = |at, freq, vel| Note {
        at,
        freq,
        vel,
        timbre: PLUCK,
    };
    let bed = |at, freq, vel| Note {
        at,
        freq,
        vel,
        timbre: SOFT,
    };
    vec![
        bed(0.0, D4, 0.32),
        n(0.0, D5, 0.78),
        n(0.15, A5, 0.62),
        n(0.30, FS5, 0.66),
        n(0.45, B5, 0.6),
        bed(0.72, FS4, 0.26),
        n(0.72, A5, 0.82),
    ]
}

fn outgoing_notes() -> Vec<Note> {
    use pitch::*;
    let n = |at, freq, vel| Note {
        at,
        freq,
        vel,
        timbre: SOFT,
    };
    vec![n(0.0, A4, 0.7), n(0.2, D5, 0.62)]
}

fn render(notes: &[Note], peak: f32, fade_from: f32) -> Rendered {
    let period = (PERIOD_SECS * SAMPLE_RATE as f32) as usize;
    // Three periods hold the whole tail of one ring (RT60 ~1 s).
    let one_shot = synth::render(notes, ROOM, period * 3);
    let mut looped = synth::fold_loop(&one_shot, period);
    // The first ring has no earlier ring ringing under it; it runs into the
    // loop seamlessly because the loop's start is exactly ring 1's tail
    // plus ring 2.
    let mut first = one_shot[..period].to_vec();
    let gain = peak / synth::peak(&looped).max(1e-6);
    looped.iter_mut().for_each(|s| *s *= gain);
    for (i, s) in first.iter_mut().enumerate() {
        // Smooth rise from `fade_from` to full level over the first ring.
        let t = i as f32 / period as f32;
        let smooth = t * t * (3.0 - 2.0 * t);
        *s *= gain * (fade_from + (1.0 - fade_from) * smooth);
    }
    Rendered { first, looped }
}

static INCOMING: LazyLock<Rendered> =
    LazyLock::new(|| render(&incoming_notes(), INCOMING_PEAK, 0.35));
static OUTGOING: LazyLock<Rendered> =
    LazyLock::new(|| render(&outgoing_notes(), OUTGOING_PEAK, 0.75));

/// The rendered tone (rendered on first use, a few ms).
pub fn rendered(tone: Tone) -> &'static Rendered {
    match tone {
        Tone::Incoming => &INCOMING,
        Tone::Outgoing => &OUTGOING,
    }
}

/// Sample `i` of a tone from its start (48 kHz mono).
pub fn sample(tone: Tone, i: usize) -> f32 {
    rendered(tone).sample(i)
}

/// A tone playing on its own output stream until dropped.
pub struct Ringtone {
    stop: Arc<AtomicBool>,
}

impl Drop for Ringtone {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// How much audio is queued ahead of the device (also the stop latency).
const QUEUE: usize = SAMPLE_RATE as usize / 5;
/// Fade-out when stopped, so the tone never cuts off mid-note.
const FADE_OUT: usize = SAMPLE_RATE as usize / 25;

impl Ringtone {
    /// Ring (the incoming ringtone) on `output` (default device when `None`).
    pub fn start(output: Option<String>) -> Result<Self> {
        Self::start_tone(Tone::Incoming, output)
    }

    /// Play `tone` on `output` (default device when `None`).
    pub fn start_tone(tone: Tone, output: Option<String>) -> Result<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = stop.clone();
        let (ready_tx, ready_rx) = flume::bounded::<Result<()>>(1);
        std::thread::Builder::new()
            .name("ringtone".into())
            .spawn(move || {
                let tone = rendered(tone);
                let (mut tx, rx) = rtrb::RingBuffer::<f32>::new(QUEUE);
                let _stream = match super::device::open_output(output.as_deref(), rx) {
                    Ok(s) => {
                        let _ = ready_tx.send(Ok(()));
                        s
                    }
                    Err(e) => {
                        let _ = ready_tx.send(Err(e));
                        return;
                    }
                };
                let mut i = 0usize;
                while !stop_thread.load(Ordering::Relaxed) {
                    while tx.slots() > 0 {
                        let _ = tx.push(tone.sample(i));
                        i += 1;
                    }
                    std::thread::sleep(Duration::from_millis(20));
                }
                // Fade out from where the queue ends, then let it drain
                // before closing the stream.
                let mut faded = 0;
                let deadline = Instant::now() + Duration::from_millis(600);
                while faded < FADE_OUT && Instant::now() < deadline {
                    while tx.slots() > 0 && faded < FADE_OUT {
                        let g = 1.0 - faded as f32 / FADE_OUT as f32;
                        let _ = tx.push(tone.sample(i) * g * g);
                        i += 1;
                        faded += 1;
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
                while tx.slots() < QUEUE && Instant::now() < deadline {
                    std::thread::sleep(Duration::from_millis(10));
                }
            })?;
        ready_rx
            .recv_timeout(Duration::from_secs(5))
            .map_err(|_| anyhow::anyhow!("ringtone didn't start"))??;
        Ok(Self { stop })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SR: usize = SAMPLE_RATE as usize;

    fn rms(x: &[f32]) -> f32 {
        (x.iter().map(|s| s * s).sum::<f32>() / x.len() as f32).sqrt()
    }

    #[test]
    fn loops_have_the_right_length_and_level() {
        for (tone, peak) in [
            (Tone::Incoming, INCOMING_PEAK),
            (Tone::Outgoing, OUTGOING_PEAK),
        ] {
            let r = rendered(tone);
            assert_eq!(r.looped.len(), (PERIOD_SECS * SR as f32) as usize);
            assert_eq!(r.first.len(), r.looped.len());
            let p = synth::peak(&r.looped);
            assert!((p - peak).abs() < 1e-3, "{tone:?} peak {p}");
            assert!(synth::peak(&r.first) <= peak + 1e-3, "{tone:?} first ring");
            let dc = r.looped.iter().sum::<f32>() / r.looped.len() as f32;
            assert!(dc.abs() < 1e-3, "{tone:?} dc {dc}");
        }
        // The ringback is clearly gentler than the ringtone.
        assert!(
            rms(&rendered(Tone::Outgoing).looped) < rms(&rendered(Tone::Incoming).looped) * 0.6
        );
    }

    #[test]
    fn rings_then_pauses() {
        for tone in [Tone::Incoming, Tone::Outgoing] {
            let r = rendered(tone);
            let ring = rms(&r.looped[..SR / 2]);
            let pause = rms(&r.looped[r.looped.len() - SR / 2..]);
            assert!(ring > 0.05, "{tone:?} ring {ring}");
            assert!(pause < ring * 0.06, "{tone:?} pause {pause} vs ring {ring}");
        }
    }

    /// Largest jump between neighbouring samples, a click detector: a
    /// discontinuity shows up as a step far bigger than the waveform's own.
    fn max_step(x: impl Iterator<Item = f32>) -> f32 {
        let mut prev: Option<f32> = None;
        let mut m = 0.0f32;
        for s in x {
            if let Some(p) = prev {
                m = m.max((s - p).abs());
            }
            prev = Some(s);
        }
        m
    }

    #[test]
    fn loop_points_are_click_free() {
        for tone in [Tone::Incoming, Tone::Outgoing] {
            let r = rendered(tone);
            let n = r.looped.len();
            let typical = max_step(r.looped.iter().copied());
            // First ring -> loop, and loop end -> loop start.
            let seam1 = max_step((n - 8..n + 8).map(|i| r.sample(i)));
            let seam2 = max_step((2 * n - 8..2 * n + 8).map(|i| r.sample(i)));
            // Near the seam the tone is in its quiet pause.
            assert!(
                seam1 < typical * 0.05,
                "{tone:?} first seam {seam1} vs {typical}"
            );
            assert!(
                seam2 < typical * 0.05,
                "{tone:?} loop seam {seam2} vs {typical}"
            );
            assert!(r.first[0].abs() < 1e-4, "{tone:?} starts from silence");
        }
    }

    #[test]
    fn first_ring_fades_in() {
        let r = rendered(Tone::Incoming);
        let first = synth::peak(&r.first[..SR / 2]);
        let later = synth::peak(&r.looped[..SR / 2]);
        assert!(first < later * 0.6, "first {first} vs later {later}");
    }
}
