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
                dn.process_frame(out, chunk);
                for (s, &o) in chunk.iter_mut().zip(out.iter()) {
                    *s = o / 32768.0;
                }
            }
        }
    }
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
