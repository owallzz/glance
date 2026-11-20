//! Capturing the audio that goes with a screen share.
//!
//! - Linux: PipeWire. Either one application's output (excludes glance's
//!   own call audio, so viewers don't hear themselves) or the whole system
//!   (the default output's monitor).
//! - Windows: WASAPI loopback of the default output (via cpal).
//! - macOS: not available yet (needs a ScreenCaptureKit audio stream).
//!
//! Captured audio is 48 kHz mono f32, encoded with Opus for music and sent
//! only to people watching the screen.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::Result;

use crate::audio::EncodedAudio;
#[cfg(any(target_os = "linux", target_os = "windows"))]
use crate::audio::{FRAME, level_of};

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "windows")]
mod windows;

/// Where shared audio comes from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AudioSource {
    /// Everything the computer plays (may include call audio on Linux).
    System,
    /// One application's audio (Linux: a PipeWire stream node).
    App { id: u32, name: String },
}

impl AudioSource {
    pub fn label(&self) -> String {
        match self {
            AudioSource::System => "Entire system".into(),
            AudioSource::App { name, .. } => name.clone(),
        }
    }
}

/// The audio-engine key for a peer's shared stream audio (distinct from
/// their voice, so it gets its own decoder and volume).
pub fn stream_key(peer: [u8; 32]) -> [u8; 32] {
    peer.map(|b| !b)
}

/// Whether this platform can share audio at all.
pub fn supported() -> bool {
    cfg!(any(target_os = "linux", target_os = "windows"))
}

/// Sources to offer (apps currently playing sound come first on Linux).
pub fn list_sources() -> Vec<AudioSource> {
    #[cfg(target_os = "linux")]
    {
        let mut v = linux::list_apps();
        v.push(AudioSource::System);
        v
    }
    #[cfg(target_os = "windows")]
    {
        vec![AudioSource::System]
    }
    #[cfg(not(any(target_os = "linux", target_os = "windows")))]
    {
        Vec::new()
    }
}

/// A running audio share; dropping it stops capture.
pub struct StreamAudio {
    stop: Arc<AtomicBool>,
}

impl Drop for StreamAudio {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// Start capturing `source`; each 20 ms Opus frame goes to `sink`.
pub fn start(
    source: AudioSource,
    sink: impl FnMut(EncodedAudio) + Send + 'static,
) -> Result<StreamAudio> {
    #[cfg(not(any(target_os = "linux", target_os = "windows")))]
    {
        let _ = (source, sink);
        anyhow::bail!("sharing audio isn't available on this platform yet");
    }
    #[cfg(any(target_os = "linux", target_os = "windows"))]
    start_supported(source, sink)
}

#[cfg(any(target_os = "linux", target_os = "windows"))]
fn start_supported(
    source: AudioSource,
    sink: impl FnMut(EncodedAudio) + Send + 'static,
) -> Result<StreamAudio> {
    let stop = Arc::new(AtomicBool::new(false));
    let (tx, rx) = rtrb::RingBuffer::<f32>::new(48_000);
    #[cfg(target_os = "linux")]
    linux::capture(source, tx, stop.clone())?;
    #[cfg(target_os = "windows")]
    windows::capture(source, tx, stop.clone())?;
    let stop_enc = stop.clone();
    std::thread::Builder::new()
        .name("stream-audio".into())
        .spawn(move || encode_loop(rx, stop_enc, sink))?;
    Ok(StreamAudio { stop })
}

#[cfg(any(target_os = "linux", target_os = "windows"))]
fn encode_loop(
    mut rx: rtrb::Consumer<f32>,
    stop: Arc<AtomicBool>,
    mut sink: impl FnMut(EncodedAudio),
) {
    let Ok(mut enc) = opus::Encoder::new(48_000, opus::Channels::Mono, opus::Application::Audio)
    else {
        return;
    };
    let _ = enc.set_bitrate(opus::Bitrate::Bits(64_000));
    let _ = enc.set_inband_fec(true);
    let mut frame = vec![0.0f32; FRAME];
    let mut out = vec![0u8; 1500];
    let mut seq = 0u32;
    while !stop.load(Ordering::Relaxed) {
        if rx.slots() < FRAME {
            std::thread::sleep(std::time::Duration::from_millis(5));
            continue;
        }
        for s in frame.iter_mut() {
            *s = rx.pop().unwrap_or(0.0);
        }
        if let Ok(n) = enc.encode_float(&frame, &mut out) {
            sink(EncodedAudio {
                seq,
                level: level_of(&frame),
                data: out[..n].to_vec(),
            });
            seq = seq.wrapping_add(1);
        }
    }
}

/// Push interleaved samples as mono into the ring (dropping if full).
#[cfg_attr(not(any(target_os = "linux", target_os = "windows")), allow(dead_code))]
pub(crate) fn push_mono(tx: &mut rtrb::Producer<f32>, interleaved: &[f32], channels: usize) {
    let ch = channels.max(1);
    let inv = 1.0 / ch as f32;
    for frame in interleaved.chunks_exact(ch) {
        if tx.push(frame.iter().sum::<f32>() * inv).is_err() {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stereo_is_downmixed() {
        let (mut tx, mut rx) = rtrb::RingBuffer::<f32>::new(8);
        push_mono(&mut tx, &[1.0, 0.0, 0.5, 0.5], 2);
        assert_eq!(rx.pop(), Ok(0.5));
        assert_eq!(rx.pop(), Ok(0.5));
        assert!(rx.pop().is_err());
    }

    #[test]
    fn labels() {
        assert_eq!(AudioSource::System.label(), "Entire system");
        assert_eq!(
            AudioSource::App {
                id: 3,
                name: "Firefox".into()
            }
            .label(),
            "Firefox"
        );
    }
}
