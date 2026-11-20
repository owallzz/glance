//! Voice: capture, processing (echo cancellation, noise suppression),
//! Opus, jitter buffering, mixing and playback.
//!
//! Everything runs at 48 kHz mono in 20 ms frames. Devices are converted at
//! the edges (`device.rs`); the engine (`engine.rs`) owns the clock.

pub mod codec;
pub mod device;
pub mod dsp;
pub mod engine;
pub mod jitter;
pub mod resample;

pub use engine::{AudioEngine, EngineConfig, InputKind, OutputKind, PeerKey};

/// Internal sample rate.
pub const SAMPLE_RATE: u32 = 48_000;
/// Samples per 20 ms frame.
pub const FRAME: usize = 960;

/// One encoded 20 ms voice frame.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EncodedAudio {
    pub seq: u32,
    /// Loudness 0–100 (for speaking indicators without decoding).
    pub level: u8,
    pub data: Vec<u8>,
}

/// Loudness of a frame as 0–100 (-60 dBFS .. 0 dBFS).
pub fn level_of(frame: &[f32]) -> u8 {
    if frame.is_empty() {
        return 0;
    }
    let rms = (frame.iter().map(|s| s * s).sum::<f32>() / frame.len() as f32).sqrt();
    if rms <= 1e-6 {
        return 0;
    }
    let db = 20.0 * rms.log10();
    (((db + 60.0) / 60.0).clamp(0.0, 1.0) * 100.0) as u8
}

/// Level above which someone counts as speaking.
pub const SPEAKING_LEVEL: u8 = 22;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn levels() {
        assert_eq!(level_of(&[0.0; 960]), 0);
        assert_eq!(level_of(&[1.0; 960]), 100);
        let quiet = level_of(&[0.001; 960]); // -60 dBFS
        assert!(quiet <= 1, "{quiet}");
        let speech = level_of(&[0.1; 960]); // -20 dBFS
        assert!((60..=70).contains(&speech), "{speech}");
        assert!(speech > SPEAKING_LEVEL);
    }
}
