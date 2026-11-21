//! Microphone processing: echo cancellation, noise suppression, loudness.

use aec_rs::{Aec, AecConfig};
use nnnoiseless::DenoiseState;

use super::FRAME;

pub struct MicProcessor {
    aec: Option<Aec>,
    denoise: Option<Box<DenoiseState<'static>>>,
    i16_mic: Vec<i16>,
    i16_ref: Vec<i16>,
    i16_out: Vec<i16>,
    scratch: Vec<f32>,
    agc: Option<AutoGain>,
}

impl MicProcessor {
    pub fn new(echo_cancel: bool, noise_suppression: bool) -> Self {
        let mut p = Self {
            aec: None,
            denoise: None,
            i16_mic: vec![0; FRAME],
            i16_ref: vec![0; FRAME],
            i16_out: vec![0; FRAME],
            scratch: vec![0.0; FRAME],
            agc: None,
        };
        p.set_echo_cancel(echo_cancel);
        p.set_noise_suppression(noise_suppression);
        p
    }

    pub fn set_echo_cancel(&mut self, on: bool) {
        self.aec = on.then(|| {
            Aec::new(&AecConfig {
                frame_size: FRAME,
                // 200 ms echo tail.
                filter_length: (super::SAMPLE_RATE / 5) as i32,
                sample_rate: super::SAMPLE_RATE,
                enable_preprocess: false,
            })
        });
    }

    pub fn set_noise_suppression(&mut self, on: bool) {
        self.denoise = on.then(DenoiseState::new);
    }

    /// Turn automatic gain control on or off (off keeps the mic as it is).
    pub fn set_auto_gain(&mut self, on: bool) {
        if on != self.agc.is_some() {
            self.agc = on.then(AutoGain::new);
        }
    }

    /// Process one mic frame in place. `far_end` is what we just played
    /// (the echo reference); silence if nothing played.
    pub fn process(&mut self, mic: &mut [f32], far_end: &[f32]) {
        debug_assert_eq!(mic.len(), FRAME);
        if let Some(aec) = &self.aec {
            to_i16(mic, &mut self.i16_mic);
            to_i16(far_end, &mut self.i16_ref);
            aec.cancel_echo(&self.i16_mic, &self.i16_ref, &mut self.i16_out);
            for (m, &o) in mic.iter_mut().zip(&self.i16_out) {
                *m = o as f32 / 32768.0;
            }
        }
        // Speech probability from RNNoise, when it runs.
        let mut voice_prob = 0.0f32;
        if let Some(dn) = &mut self.denoise {
            // RNNoise works on 480-sample frames in i16 range.
            const N: usize = DenoiseState::FRAME_SIZE;
            for (chunk, out) in mic
                .as_chunks_mut::<N>()
                .0
                .iter_mut()
                .zip(self.scratch.as_chunks_mut::<N>().0.iter_mut())
            {
                for s in chunk.iter_mut() {
                    *s *= 32768.0;
                }
                voice_prob = voice_prob.max(dn.process_frame(out, chunk));
                for (s, &o) in chunk.iter_mut().zip(out.iter()) {
                    *s = o / 32768.0;
                }
            }
        }
        if let Some(agc) = &mut self.agc {
            let voice = if self.denoise.is_some() {
                voice_prob > 0.5
            } else {
                super::level_of(mic) > super::SPEAKING_LEVEL
            };
            agc.process(mic, voice);
        }
    }
}

/// Automatic gain control: brings speech to a steady, call-friendly loudness
/// whatever the mic's own gain is (other voice apps do the same by default,
/// so without it we sound quiet next to them). It adapts only while someone
/// is talking, so background noise isn't pumped up between words, and never
/// lets a frame clip.
pub struct AutoGain {
    gain_db: f32,
}

impl AutoGain {
    /// Speech level aimed for, as frame RMS.
    const TARGET_DB: f32 = -18.0;
    const MAX_GAIN_DB: f32 = 24.0;
    const MIN_GAIN_DB: f32 = -12.0;
    /// Per 20 ms frame: up 10 dB/s, down 50 dB/s.
    const RISE_DB: f32 = 0.2;
    const FALL_DB: f32 = 1.0;
    /// Peak ceiling after gain.
    const LIMIT: f32 = 0.9;

    pub fn new() -> Self {
        // Most mics are on the quiet side; start with a little boost.
        Self { gain_db: 6.0 }
    }

    pub fn gain_db(&self) -> f32 {
        self.gain_db
    }

    /// Apply gain to one frame in place; `voice` says whether it holds speech.
    pub fn process(&mut self, frame: &mut [f32], voice: bool) {
        if frame.is_empty() {
            return;
        }
        let start = db_to_gain(self.gain_db);
        let rms = (frame.iter().map(|s| s * s).sum::<f32>() / frame.len() as f32).sqrt();
        if voice && rms > 1e-4 {
            let want =
                (Self::TARGET_DB - 20.0 * rms.log10()).clamp(Self::MIN_GAIN_DB, Self::MAX_GAIN_DB);
            self.gain_db += (want - self.gain_db).clamp(-Self::FALL_DB, Self::RISE_DB);
        }
        let peak = frame.iter().fold(0.0f32, |m, s| m.max(s.abs()));
        let mut end = db_to_gain(self.gain_db);
        let mut start = start;
        if peak > 0.0 {
            let ceiling = Self::LIMIT / peak;
            if end > ceiling {
                end = ceiling;
                self.gain_db = 20.0 * end.log10();
            }
            start = start.min(ceiling);
        }
        // Ramp across the frame so gain changes don't click.
        let step = (end - start) / frame.len() as f32;
        for (i, s) in frame.iter_mut().enumerate() {
            *s *= start + step * i as f32;
        }
    }
}

impl Default for AutoGain {
    fn default() -> Self {
        Self::new()
    }
}

fn db_to_gain(db: f32) -> f32 {
    10f32.powf(db / 20.0)
}

fn to_i16(src: &[f32], dst: &mut [i16]) {
    for (d, &s) in dst.iter_mut().zip(src) {
        *d = (s.clamp(-1.0, 1.0) * 32767.0) as i16;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn noise_suppression_reduces_white_noise() {
        let mut p = MicProcessor::new(false, true);
        let mut seed = 1u32;
        let mut noise = || {
            seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
            ((seed >> 8) as f32 / (1 << 24) as f32 - 0.5) * 0.05
        };
        let silence = vec![0.0; FRAME];
        let mut levels = Vec::new();
        for _ in 0..200 {
            let mut frame: Vec<f32> = (0..FRAME).map(|_| noise()).collect();
            p.process(&mut frame, &silence);
            levels.push(super::super::level_of(&frame));
        }
        let raw: Vec<f32> = (0..FRAME).map(|_| noise()).collect();
        let raw_level = super::super::level_of(&raw);
        let settled = *levels[150..].iter().max().unwrap();
        assert!(
            settled + 10 < raw_level,
            "denoised {settled} vs raw {raw_level}"
        );
    }

    fn tone(t0: usize, amp: f32) -> Vec<f32> {
        (0..FRAME)
            .map(|i| ((t0 + i) as f32 * 220.0 * std::f32::consts::TAU / 48_000.0).sin() * amp)
            .collect()
    }

    #[test]
    fn auto_gain_brings_quiet_speech_up_without_clipping() {
        let mut agc = AutoGain::new();
        let mut last = Vec::new();
        // A quiet talker (-36 dBFS RMS), for 3 seconds.
        for n in 0..150 {
            let mut f = tone(n * FRAME, 0.022);
            agc.process(&mut f, true);
            last = f;
        }
        let level = crate::audio::level_of(&last);
        // -18 dBFS target is level 70.
        assert!((64..=74).contains(&level), "level {level}");
        // A sudden shout is limited, not clipped.
        let mut loud = tone(0, 0.8);
        agc.process(&mut loud, true);
        let peak = loud.iter().fold(0.0f32, |m, s| m.max(s.abs()));
        assert!(peak <= 0.9 + 1e-4, "peak {peak}");
    }

    #[test]
    fn auto_gain_holds_between_words() {
        let mut agc = AutoGain::new();
        let before = agc.gain_db();
        for n in 0..100 {
            let mut f = tone(n * FRAME, 0.005);
            agc.process(&mut f, false);
        }
        assert_eq!(agc.gain_db(), before);
    }

    #[test]
    fn echo_canceller_attenuates_played_audio() {
        let mut p = MicProcessor::new(true, false);
        let mut t = 0usize;
        let mut last = 100;
        for _ in 0..150 {
            let far: Vec<f32> = (0..FRAME)
                .map(|i| {
                    (((t + i) as f32) * 0.07).sin() * 0.3 + (((t + i) as f32) * 0.013).sin() * 0.2
                })
                .collect();
            // The mic hears the speaker, a little quieter.
            let mut mic: Vec<f32> = far.iter().map(|s| s * 0.5).collect();
            p.process(&mut mic, &far);
            last = super::super::level_of(&mic);
            t += FRAME;
        }
        let far_level = 75; // roughly the level of 0.5 * far
        assert!(last + 15 < far_level, "after AEC: {last}");
    }
}
