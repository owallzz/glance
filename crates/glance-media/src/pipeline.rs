//! The two halves of a screen share:
//!
//! - [`start_broadcast`]: capture -> scale to the chosen quality -> encode
//!   (only while someone is watching) -> hand packets to the network, plus a
//!   small local preview.
//! - [`spawn_decoder`]: packets -> decode -> scale to the tile -> frames.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;

use crate::capture::Capture;
use crate::codec::{EncodedPacket, EncoderChoice, VideoDecoder, VideoEncoder, bitrate_for};
use crate::frame::{FrameScaler, TargetSize, VideoFrame, fit_within};

/// Size to encode a `src`-sized capture at: scaled to `max_height` (if set),
/// aspect kept, never upscaled, both sides even (H.264 needs that).
pub fn output_size(src: (u32, u32), max_height: Option<u32>) -> (u32, u32) {
    let (w, h) = match max_height {
        Some(cap) => fit_within(src, (u32::MAX, cap)),
        None => src,
    };
    // Never bigger than the encoder can take (either orientation).
    let (w, h) = crate::codec::fit_encoder((w, h));
    ((w & !1).max(2), (h & !1).max(2))
}

/// Adapts the encoder bitrate to the slowest viewer: back off quickly when a
/// viewer's queue overflows, recover slowly when things are calm.
#[derive(Debug, Clone)]
pub struct RateController {
    base: u32,
    current: u32,
    last_decrease: Option<std::time::Instant>,
    last_change: std::time::Instant,
}

impl RateController {
    pub fn new(base: u32) -> Self {
        Self {
            base,
            current: base,
            last_decrease: None,
            last_change: std::time::Instant::now(),
        }
    }

    pub fn current(&self) -> u32 {
        self.current
    }

    /// A viewer fell behind. Returns the new rate if it changed.
    pub fn on_congestion(&mut self, now: std::time::Instant) -> Option<u32> {
        if self
            .last_decrease
            .is_some_and(|t| now - t < Duration::from_secs(1))
        {
            return None;
        }
        let floor = self.base / 4;
        let next = (self.current as u64 * 7 / 10) as u32;
        let next = next.max(floor);
        self.last_decrease = Some(now);
        self.last_change = now;
        (next != self.current).then(|| {
            self.current = next;
            next
        })
    }

    /// Called regularly; raises the rate after 8 calm seconds.
    pub fn on_tick(&mut self, now: std::time::Instant) -> Option<u32> {
        let calm = self
            .last_decrease
            .is_none_or(|t| now - t >= Duration::from_secs(8));
        if !calm || self.current >= self.base || now - self.last_change < Duration::from_secs(4) {
            return None;
        }
        self.current = ((self.current as u64 * 23 / 20) as u32).min(self.base);
        self.last_change = now;
        Some(self.current)
    }
}

/// Live controls for a running broadcast, shared with the UI/network.
#[derive(Clone, Default)]
pub struct BroadcastControl {
    /// Number of viewers; encoding is skipped while it's zero.
    pub watchers: Arc<AtomicUsize>,
    /// Set to make the next encoded frame a keyframe.
    pub keyframe: Arc<AtomicBool>,
    /// Captured size, packed as `(w << 32) | h` (0 until the first frame).
    pub source_size: Arc<AtomicU64>,
    /// Bumped by the network when a viewer falls behind.
    pub congestion: Arc<AtomicUsize>,
    /// Current target bitrate (for the stats overlay).
    pub bitrate: Arc<AtomicU64>,
    /// Whether the current encoder is a hardware one.
    pub hardware: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
}

impl BroadcastControl {
    pub fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }

    pub fn source_size(&self) -> (u32, u32) {
        let v = self.source_size.load(Ordering::Relaxed);
        ((v >> 32) as u32, v as u32)
    }
}

pub struct BroadcastConfig {
    pub max_height: Option<u32>,
    pub fps: u32,
    pub encoder: EncoderChoice,
}

/// Run a broadcast on its own thread. `sink` gets every encoded packet;
/// `preview` (if any) gets frames scaled to its tile. Returns the controls
/// and a receiver that yields once when the broadcast ends (with a reason).
pub fn start_broadcast(
    capture: Capture,
    config: BroadcastConfig,
    preview: Option<(flume::Sender<VideoFrame>, TargetSize)>,
    mut sink: impl FnMut(EncodedPacket) + Send + 'static,
) -> (BroadcastControl, flume::Receiver<String>) {
    let control = BroadcastControl::default();
    let ctl = control.clone();
    let (ended_tx, ended_rx) = flume::bounded(1);
    std::thread::Builder::new()
        .name("broadcast".into())
        .spawn(move || {
            let mut scaler = FrameScaler::default();
            let mut preview_scaler = FrameScaler::default();
            let mut encoder: Option<VideoEncoder> = None;
            let mut rate: Option<RateController> = None;
            let mut seen_congestion = 0usize;
            // The last captured frame: GNOME only sends frames when the screen
            // changes, so a viewer joining a still screen needs it re-encoded.
            let mut last: Option<VideoFrame> = None;
            let mut last_sent = std::time::Instant::now();
            let mut stats = Stats::default();
            let mut choice = config.encoder;
            let reason = loop {
                if ctl.stop.load(Ordering::Relaxed) {
                    break "stopped".to_string();
                }
                if let Ok(reason) = capture.ended.try_recv() {
                    break reason;
                }
                let fresh = match capture.frames.recv_timeout(Duration::from_millis(100)) {
                    Ok(f) => Some(f),
                    Err(flume::RecvTimeoutError::Timeout) => None,
                    Err(flume::RecvTimeoutError::Disconnected) => break "capture ended".into(),
                };
                let frame = match fresh {
                    Some(f) => {
                        stats.captured += 1;
                        ctl.source_size.store(
                            ((f.width as u64) << 32) | f.height as u64,
                            Ordering::Relaxed,
                        );
                        if let Some((tx, target)) = &preview {
                            send_preview(&mut preview_scaler, &f, tx, target);
                        }
                        last.insert(f)
                    }
                    // Nothing new: re-send the last frame only if a keyframe is
                    // wanted, or once a second so viewers see it's alive.
                    None => {
                        let refresh = ctl.keyframe.load(Ordering::Relaxed)
                            || last_sent.elapsed() > Duration::from_secs(1);
                        match (&last, refresh) {
                            (Some(_), true) => last.as_mut().expect("checked"),
                            _ => continue,
                        }
                    }
                };

                if ctl.watchers.load(Ordering::Relaxed) == 0 {
                    continue;
                }
                let out = output_size((frame.width, frame.height), config.max_height);
                if encoder.as_ref().is_none_or(|e| e.size() != out) {
                    let base = bitrate_for(out.0, out.1, config.fps);
                    match VideoEncoder::with_choice(out.0, out.1, config.fps, base, choice) {
                        Ok(e) => {
                            tracing::info!(
                                "encoding {}x{} with {}",
                                out.0,
                                out.1,
                                e.backend_name()
                            );
                            ctl.hardware.store(e.is_hardware(), Ordering::Relaxed);
                            encoder = Some(e);
                            rate = Some(RateController::new(base));
                            ctl.bitrate.store(base as u64, Ordering::Relaxed);
                        }
                        Err(e) => break format!("couldn't start the video encoder: {e:#}"),
                    }
                }
                let Some(enc) = encoder.as_mut() else {
                    continue;
                };
                // Follow the slowest viewer's capacity.
                if let Some(rc) = rate.as_mut() {
                    let now = std::time::Instant::now();
                    let c = ctl.congestion.load(Ordering::Relaxed);
                    let change = if c != seen_congestion {
                        seen_congestion = c;
                        rc.on_congestion(now)
                    } else {
                        rc.on_tick(now)
                    };
                    if let Some(bps) = change {
                        enc.set_bitrate(bps);
                        ctl.bitrate.store(bps as u64, Ordering::Relaxed);
                        tracing::debug!(bps, "broadcast bitrate");
                    }
                }
                if ctl.keyframe.swap(false, Ordering::Relaxed) {
                    enc.force_keyframe();
                }
                let scaled;
                let input = if out == (frame.width, frame.height) {
                    &*frame
                } else {
                    match scaler.resize_ref(frame, out) {
                        Some(f) => {
                            scaled = f;
                            &scaled
                        }
                        None => continue,
                    }
                };
                let t = std::time::Instant::now();
                let result = enc.encode(input);
                stats.encode_time += t.elapsed();
                stats.report();
                match result {
                    Ok(Some(pkt)) => {
                        stats.encoded += 1;
                        stats.bytes += pkt.data.len();
                        last_sent = std::time::Instant::now();
                        sink(pkt)
                    }
                    Ok(None) => {}
                    // A GPU encoder can fail mid-stream (driver reset, too
                    // many sessions): carry on in software.
                    Err(e) if enc.is_hardware() => {
                        tracing::warn!("hardware encoding failed, switching to software: {e:#}");
                        choice = EncoderChoice::Software;
                        encoder = None;
                        ctl.keyframe.store(true, Ordering::Relaxed);
                    }
                    Err(e) => break format!("video encoding failed: {e:#}"),
                }
            };
            capture.stop();
            let _ = ended_tx.send(reason);
        })
        .expect("spawn broadcast thread");
    (control, ended_rx)
}

/// Per-broadcast counters, logged at debug level every few seconds.
#[derive(Default)]
struct Stats {
    since: Option<std::time::Instant>,
    captured: u32,
    encoded: u32,
    bytes: usize,
    encode_time: Duration,
}

impl Stats {
    fn report(&mut self) {
        let since = *self.since.get_or_insert_with(std::time::Instant::now);
        let secs = since.elapsed().as_secs_f64();
        if secs < 3.0 {
            return;
        }
        tracing::debug!(
            captured_fps = format!("{:.1}", self.captured as f64 / secs),
            encoded_fps = format!("{:.1}", self.encoded as f64 / secs),
            encode_ms = format!(
                "{:.1}",
                self.encode_time.as_secs_f64() * 1000.0 / self.encoded.max(1) as f64
            ),
            mbps = format!("{:.2}", self.bytes as f64 * 8.0 / secs / 1e6),
            "broadcast"
        );
        *self = Stats::default();
    }
}

/// Scale a frame to the preview tile and offer it (dropped if the UI is busy).
fn send_preview(
    scaler: &mut FrameScaler,
    frame: &VideoFrame,
    tx: &flume::Sender<VideoFrame>,
    target: &TargetSize,
) {
    let (tw, th) = target.get();
    if tw == 0 || th == 0 || tx.is_full() {
        return;
    }
    let size = fit_within((frame.width, frame.height), (tw, th));
    let small = if size == (frame.width, frame.height) {
        Some(frame.clone())
    } else {
        scaler.resize_ref(frame, size)
    };
    if let Some(small) = small {
        let _ = tx.try_send(small);
    }
}

/// Decode packets from `next` (returns `None` when the stream ends) into
/// frames scaled to `target`. `on_lost_sync` is called when a packet fails
/// to decode so the caller can ask the sharer for a keyframe.
pub fn spawn_decoder(
    mut next: impl FnMut() -> Option<Vec<u8>> + Send + 'static,
    target: TargetSize,
    mut on_lost_sync: impl FnMut() + Send + 'static,
) -> flume::Receiver<VideoFrame> {
    let (tx, rx) = flume::bounded(2);
    std::thread::Builder::new()
        .name("decode".into())
        .spawn(move || {
            let Ok(mut decoder) = VideoDecoder::new() else {
                return;
            };
            while let Some(data) = next() {
                match decoder.decode(&data, target.get()) {
                    Ok(Some(frame)) => {
                        if tx.is_disconnected() {
                            break;
                        }
                        let _ = tx.try_send(frame);
                    }
                    Ok(None) => {}
                    Err(_) => on_lost_sync(),
                }
            }
        })
        .expect("spawn decode thread");
    rx
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rate_controller_backs_off_fast_and_recovers_slowly() {
        let t0 = std::time::Instant::now();
        let s = |secs: f64| t0 + Duration::from_secs_f64(secs);
        let mut rc = RateController::new(1_000_000);
        assert_eq!(rc.on_congestion(s(0.0)), Some(700_000));
        assert_eq!(rc.on_congestion(s(0.5)), None, "at most one cut per second");
        assert_eq!(rc.on_congestion(s(1.1)), Some(490_000));
        for i in 0..10 {
            rc.on_congestion(s(2.0 + i as f64 * 1.1));
        }
        assert_eq!(rc.current(), 250_000, "floor is a quarter of the base");
        assert_eq!(rc.on_tick(s(15.0)), None, "not calm yet");
        let mut t = 21.0;
        while rc.current() < 1_000_000 {
            if rc.on_tick(s(t)).is_some() {
                assert!(rc.current() <= 1_000_000);
            }
            t += 4.0;
            assert!(t < 200.0, "never recovered");
        }
    }

    #[test]
    fn output_sizes_are_even_and_capped() {
        assert_eq!(output_size((3840, 2160), Some(1080)), (1920, 1080));
        assert_eq!(output_size((1366, 768), None), (1366, 768));
        assert_eq!(output_size((1367, 769), None), (1366, 768));
        assert_eq!(output_size((5120, 1440), Some(1080)), (3840, 1080));
        // Wider than the encoder takes: scaled down, keeping the shape.
        assert_eq!(output_size((5120, 1440), None), (4096, 1152));
        assert_eq!(output_size((5120, 1440), Some(2160)), (4096, 1152));
        assert_eq!(output_size((4096, 2160), None), (3966, 2090));
        assert_eq!(output_size((3440, 1440), None), (3440, 1440));
        // Bigger than 4K's pixel count: scaled to 4K's worth.
        assert_eq!(output_size((7680, 4320), None), (3840, 2160));
        assert_eq!(output_size((5120, 2160), None), (4096, 1728));
        assert_eq!(output_size((2160, 3840), None), (2160, 3840));
        assert_eq!(output_size((1280, 720), Some(1440)), (1280, 720));
    }

    #[test]
    fn decoder_thread_round_trips_encoded_frames() {
        let mut enc = VideoEncoder::new(320, 180, 30, 1_000_000).unwrap();
        let mut pattern = crate::pattern::Pattern::new(320, 180, 3);
        let packets: Vec<Vec<u8>> = (0..10)
            .filter_map(|_| enc.encode(&pattern.next_frame()).unwrap())
            .map(|p| p.data)
            .collect();
        let mut it = packets.into_iter();
        let target = TargetSize::default();
        target.set(160, 160);
        let frames = spawn_decoder(move || it.next(), target, || {});
        let first = frames.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!((first.width, first.height), (160, 90));
    }
}
