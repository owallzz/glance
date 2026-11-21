//! The two halves of a screen share:
//!
//! - [`start_broadcast`]: capture -> scale to the chosen quality -> encode
//!   (only while someone is watching) -> hand packets to the network, plus a
//!   small local preview. Quality and the captured source can change while
//!   it runs ([`BroadcastControl::reconfigure`],
//!   [`BroadcastControl::replace_capture`]).
//! - [`spawn_decoder`]: packets -> decode -> scale to the tile -> frames.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

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

    /// The same controller for a new base rate (new size or frame rate):
    /// keeps how far it has backed off, so a congested viewer isn't
    /// flooded right after a quality change.
    pub fn rebased(&self, base: u32) -> Self {
        let ratio = self.current as f64 / self.base.max(1) as f64;
        Self {
            base,
            current: ((base as f64 * ratio) as u32).clamp(base / 4, base),
            last_decrease: self.last_decrease,
            last_change: self.last_change,
        }
    }
}

/// Encoders are remade (each starting with a keyframe) at most this often,
/// so repeated quality changes or a window being resized can't flood
/// viewers with keyframes. In between, frames are scaled to the current
/// encoder's size.
const MIN_REBUILD_GAP: Duration = Duration::from_secs(1);

/// A change to a running broadcast, applied at a frame boundary.
pub enum BroadcastCommand {
    /// New quality. A different output size or frame rate remakes the
    /// encoder, so viewers get a keyframe with the new SPS/PPS.
    Reconfigure { max_height: Option<u32>, fps: u32 },
    /// Switch to another capture. The current one keeps streaming until the
    /// new one delivers its first frame (a picker may be open meanwhile).
    /// `label` comes back in the [`BroadcastNotice`].
    ReplaceCapture { capture: Capture, label: String },
}

/// What happened to a [`BroadcastCommand::ReplaceCapture`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BroadcastNotice {
    /// The new source is live.
    SourceChanged(String),
    /// The new source never started (picker cancelled...); the old one
    /// carries on.
    SourceFailed { label: String, reason: String },
}

/// Live controls for a running broadcast, shared with the UI/network.
#[derive(Clone)]
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
    /// Size the stream goes out at, packed like `source_size` (0 until the
    /// first frame). Follows quality changes and source switches.
    pub output_size: Arc<AtomicU64>,
    /// Frame rate in effect.
    pub fps: Arc<AtomicU32>,
    stop: Arc<AtomicBool>,
    commands: flume::Sender<BroadcastCommand>,
    notices: flume::Receiver<BroadcastNotice>,
}

impl BroadcastControl {
    pub fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }

    pub fn source_size(&self) -> (u32, u32) {
        unpack(self.source_size.load(Ordering::Relaxed))
    }

    pub fn output_size(&self) -> (u32, u32) {
        unpack(self.output_size.load(Ordering::Relaxed))
    }

    /// Change the quality without stopping (see [`BroadcastCommand`]).
    pub fn reconfigure(&self, max_height: Option<u32>, fps: u32) {
        let _ = self
            .commands
            .send(BroadcastCommand::Reconfigure { max_height, fps });
    }

    /// Swap the captured source without stopping (see [`BroadcastCommand`]).
    pub fn replace_capture(&self, capture: Capture, label: impl Into<String>) {
        let _ = self.commands.send(BroadcastCommand::ReplaceCapture {
            capture,
            label: label.into(),
        });
    }

    /// The next notice about a source switch, if any.
    pub fn try_notice(&self) -> Option<BroadcastNotice> {
        self.notices.try_recv().ok()
    }
}

fn pack((w, h): (u32, u32)) -> u64 {
    ((w as u64) << 32) | h as u64
}

fn unpack(v: u64) -> (u32, u32) {
    ((v >> 32) as u32, v as u32)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
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
    let (commands_tx, commands) = flume::unbounded();
    let (notices_tx, notices) = flume::bounded(8);
    let control = BroadcastControl {
        watchers: Default::default(),
        keyframe: Default::default(),
        source_size: Default::default(),
        congestion: Default::default(),
        bitrate: Default::default(),
        hardware: Default::default(),
        output_size: Default::default(),
        fps: Arc::new(AtomicU32::new(config.fps)),
        stop: Default::default(),
        commands: commands_tx,
        notices,
    };
    let ctl = control.clone();
    let (ended_tx, ended_rx) = flume::bounded(1);
    std::thread::Builder::new()
        .name("broadcast".into())
        .spawn(move || {
            let mut capture = capture;
            let mut config = config;
            // A capture waiting for its first frame to take over.
            let mut pending: Option<(Capture, String)> = None;
            // The current capture ended while a replacement is pending.
            let mut capture_dead = false;
            let mut scaler = FrameScaler::default();
            let mut preview_scaler = FrameScaler::default();
            let mut encoder: Option<VideoEncoder> = None;
            let mut encoder_fps = 0;
            let mut last_rebuild: Option<Instant> = None;
            let mut rate: Option<RateController> = None;
            let mut seen_congestion = 0usize;
            // The last captured frame: GNOME only sends frames when the screen
            // changes, so a viewer joining a still screen needs it re-encoded.
            let mut last: Option<VideoFrame> = None;
            let mut last_sent = Instant::now();
            // Re-encode the last frame now (the quality changed).
            let mut refresh_now = false;
            let mut stats = Stats::default();
            let mut choice = config.encoder;
            let reason = loop {
                if ctl.stop.load(Ordering::Relaxed) {
                    break "stopped".to_string();
                }
                for cmd in commands.try_iter() {
                    match cmd {
                        BroadcastCommand::Reconfigure { max_height, fps } => {
                            tracing::info!(?max_height, fps, "broadcast reconfigured");
                            config.max_height = max_height;
                            config.fps = fps.max(1);
                            capture.set_fps(config.fps);
                            if let Some((p, _)) = &pending {
                                p.set_fps(config.fps);
                            }
                            ctl.fps.store(config.fps, Ordering::Relaxed);
                            refresh_now = true;
                        }
                        BroadcastCommand::ReplaceCapture { capture: c, label } => {
                            c.set_fps(config.fps);
                            // A replacement still pending is dropped (and so
                            // stopped).
                            pending = Some((c, label));
                        }
                    }
                }

                let mut fresh = None;
                if let Some((p, _)) = &pending {
                    let failed = match p.ended.try_recv() {
                        Ok(reason) => Some(reason),
                        Err(_) if p.frames.is_disconnected() => Some("capture ended".into()),
                        Err(_) => None,
                    };
                    if let Some(reason) = failed {
                        let (_, label) = pending.take().expect("checked");
                        tracing::info!("new source {label:?} didn't start: {reason}");
                        let _ = notices_tx.try_send(BroadcastNotice::SourceFailed {
                            label,
                            reason: reason.clone(),
                        });
                        if capture_dead {
                            break reason;
                        }
                    } else if let Ok(f) = p.frames.try_recv() {
                        let (p, label) = pending.take().expect("checked");
                        tracing::info!("switched source to {label:?}");
                        std::mem::replace(&mut capture, p).stop();
                        capture_dead = false;
                        let _ = notices_tx.try_send(BroadcastNotice::SourceChanged(label));
                        fresh = Some(f);
                    }
                }
                if fresh.is_none() && !capture_dead {
                    if let Ok(reason) = capture.ended.try_recv() {
                        if pending.is_none() {
                            break reason;
                        }
                        capture_dead = true;
                    } else {
                        match capture.frames.recv_timeout(Duration::from_millis(100)) {
                            Ok(f) => fresh = Some(f),
                            Err(flume::RecvTimeoutError::Timeout) => {}
                            Err(flume::RecvTimeoutError::Disconnected) => {
                                if pending.is_none() {
                                    break "capture ended".into();
                                }
                                capture_dead = true;
                            }
                        }
                    }
                }
                if fresh.is_none() && capture_dead {
                    // Hold the last picture until the new source starts.
                    std::thread::sleep(Duration::from_millis(20));
                }
                let frame = match fresh {
                    Some(f) => {
                        stats.captured += 1;
                        ctl.source_size
                            .store(pack((f.width, f.height)), Ordering::Relaxed);
                        if let Some((tx, target)) = &preview {
                            send_preview(&mut preview_scaler, &f, tx, target);
                        }
                        last.insert(f)
                    }
                    // Nothing new: re-send the last frame only if a keyframe is
                    // wanted, the quality changed, or once a second so viewers
                    // see it's alive.
                    None => {
                        let refresh = ctl.keyframe.load(Ordering::Relaxed)
                            || refresh_now
                            || last_sent.elapsed() > Duration::from_secs(1);
                        match (&mut last, refresh) {
                            (Some(f), true) => f,
                            _ => continue,
                        }
                    }
                };
                refresh_now = false;

                let out = output_size((frame.width, frame.height), config.max_height);
                ctl.output_size.store(pack(out), Ordering::Relaxed);
                if ctl.watchers.load(Ordering::Relaxed) == 0 {
                    continue;
                }
                let stale = encoder
                    .as_ref()
                    .is_none_or(|e| e.size() != out || encoder_fps != config.fps);
                let may_rebuild = encoder.is_none()
                    || last_rebuild.is_none_or(|t| t.elapsed() >= MIN_REBUILD_GAP);
                if stale && may_rebuild {
                    let base = bitrate_for(out.0, out.1, config.fps);
                    match VideoEncoder::with_choice(out.0, out.1, config.fps, base, choice) {
                        Ok(e) => {
                            tracing::info!(
                                "encoding {}x{}@{} with {}",
                                out.0,
                                out.1,
                                config.fps,
                                e.backend_name()
                            );
                            ctl.hardware.store(e.is_hardware(), Ordering::Relaxed);
                            encoder = Some(e);
                            encoder_fps = config.fps;
                            last_rebuild = Some(Instant::now());
                            let rc = match &rate {
                                Some(old) => old.rebased(base),
                                None => RateController::new(base),
                            };
                            ctl.bitrate.store(rc.current() as u64, Ordering::Relaxed);
                            rate = Some(rc);
                            // A new encoder starts with a keyframe (with
                            // SPS/PPS), which answers any pending request.
                            ctl.keyframe.store(false, Ordering::Relaxed);
                        }
                        Err(e) => break format!("couldn't start the video encoder: {e:#}"),
                    }
                }
                let Some(enc) = encoder.as_mut() else {
                    continue;
                };
                // Follow the slowest viewer's capacity.
                if let Some(rc) = rate.as_mut() {
                    let now = Instant::now();
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
                // Normally `out`; the previous size until a rebuild is due.
                let size = enc.size();
                let scaled;
                let input = if size == (frame.width, frame.height) {
                    &*frame
                } else {
                    match scaler.resize_ref(frame, size) {
                        Some(f) => {
                            scaled = f;
                            &scaled
                        }
                        None => continue,
                    }
                };
                let t = Instant::now();
                let result = enc.encode(input);
                stats.encode_time += t.elapsed();
                stats.report();
                match result {
                    Ok(Some(pkt)) => {
                        stats.encoded += 1;
                        stats.bytes += pkt.data.len();
                        last_sent = Instant::now();
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
            drop(pending);
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
            let Ok(mut decoder) = VideoDecoder::auto() else {
                return;
            };
            tracing::info!("decoding with {}", decoder.backend_name());
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
    fn rebased_rate_keeps_the_back_off() {
        let mut rc = RateController::new(1_000_000);
        rc.on_congestion(Instant::now());
        let r = rc.rebased(4_000_000);
        assert_eq!(r.current(), 2_800_000);
        assert_eq!(RateController::new(1_000).rebased(8_000).current(), 8_000);
    }

    /// Runs a software-encoded broadcast and decodes what it sends with
    /// OpenH264, as a viewer would.
    struct Viewer {
        /// Packets with when the broadcaster sent them.
        packets: flume::Receiver<(Instant, EncodedPacket)>,
        decoder: VideoDecoder,
    }

    impl Viewer {
        fn start(capture: Capture, config: BroadcastConfig) -> (BroadcastControl, Self) {
            let (tx, packets) = flume::unbounded();
            let (control, _ended) = start_broadcast(capture, config, None, move |p| {
                let _ = tx.send((Instant::now(), p));
            });
            control.watchers.store(1, Ordering::Relaxed);
            let viewer = Viewer {
                packets,
                decoder: VideoDecoder::new().unwrap(),
            };
            (control, viewer)
        }

        /// Decode until a frame of `size` comes out; returns how many
        /// keyframes were seen on the way.
        fn until(&mut self, size: (u32, u32)) -> usize {
            let deadline = Instant::now() + Duration::from_secs(10);
            let mut keyframes = 0;
            loop {
                let left = deadline.saturating_duration_since(Instant::now());
                let (_, pkt) = self
                    .packets
                    .recv_timeout(left)
                    .unwrap_or_else(|_| panic!("no {size:?} frame in time"));
                keyframes += pkt.keyframe as usize;
                assert_eq!(
                    pkt.keyframe,
                    has_sps(&pkt.data),
                    "every keyframe carries SPS/PPS, nothing else does"
                );
                let frame = self
                    .decoder
                    .decode(&pkt.data, (0, 0))
                    .expect("decodes across the change");
                if let Some(f) = frame
                    && (f.width, f.height) == size
                {
                    assert_eq!((pkt.width, pkt.height), size);
                    return keyframes;
                }
            }
        }
    }

    /// Whether an Annex-B packet contains an SPS NAL unit.
    fn has_sps(data: &[u8]) -> bool {
        data.windows(4)
            .any(|w| w[..3] == [0, 0, 1] && w[3] & 0x1f == 7)
    }

    fn software(max_height: Option<u32>, fps: u32) -> BroadcastConfig {
        BroadcastConfig {
            max_height,
            fps,
            encoder: EncoderChoice::Software,
        }
    }

    #[test]
    fn quality_changes_mid_stream_and_viewers_keep_decoding() {
        let capture = crate::capture::test_pattern(1920, 1080, 30);
        let (control, mut viewer) = Viewer::start(capture, software(Some(720), 30));
        viewer.until((1280, 720));
        assert_eq!(control.output_size(), (1280, 720));
        // Up to the source's full size, and a new frame rate.
        control.reconfigure(None, 24);
        assert!(
            viewer.until((1920, 1080)) >= 1,
            "the change starts with a keyframe"
        );
        assert_eq!(control.output_size(), (1920, 1080));
        assert_eq!(control.fps.load(Ordering::Relaxed), 24);
        // And back down.
        control.reconfigure(Some(720), 30);
        viewer.until((1280, 720));
        // The stream carries on at the new size after the change.
        let mut more = 0;
        while more < 10 {
            let (_, pkt) = viewer.packets.recv_timeout(Duration::from_secs(5)).unwrap();
            assert_eq!((pkt.width, pkt.height), (1280, 720));
            if viewer.decoder.decode(&pkt.data, (0, 0)).unwrap().is_some() {
                more += 1;
            }
        }
        control.stop();
    }

    #[test]
    fn quality_changes_are_rate_limited() {
        let capture = crate::capture::test_pattern(1280, 720, 30);
        let (control, mut viewer) = Viewer::start(capture, software(None, 30));
        viewer.until((1280, 720));
        // A burst of changes: at most one new encoder (keyframe) a second.
        let t0 = Instant::now();
        for i in 0..20 {
            let h = if i % 2 == 0 { Some(360) } else { Some(540) };
            control.reconfigure(h, 30);
            std::thread::sleep(Duration::from_millis(50));
        }
        let mut keyframes = vec![];
        while t0.elapsed() < Duration::from_millis(2500) {
            if let Ok((sent, p)) = viewer.packets.recv_timeout(Duration::from_millis(100)) {
                // Every packet decodes, whatever size it's at.
                viewer.decoder.decode(&p.data, (0, 0)).unwrap();
                if p.keyframe {
                    keyframes.push(sent - t0);
                }
            }
        }
        assert!(
            (1..=3).contains(&keyframes.len()),
            "keyframes at {keyframes:?}"
        );
        for pair in keyframes.windows(2) {
            assert!(
                pair[1] - pair[0] >= Duration::from_millis(900),
                "{keyframes:?}"
            );
        }
        // It settles on the last request.
        viewer.until((960, 540));
        control.stop();
    }

    #[test]
    fn capture_is_swapped_without_stopping() {
        let capture = crate::capture::test_pattern(640, 360, 30);
        let (control, mut viewer) = Viewer::start(capture, software(None, 30));
        viewer.until((640, 360));
        // A source that never starts (a cancelled picker): the stream goes on.
        let (sink, broken) = crate::capture::channel(30);
        sink.end("screen sharing was cancelled");
        control.replace_capture(broken, "Nothing");
        let deadline = Instant::now() + Duration::from_secs(5);
        let notice = loop {
            if let Some(n) = control.try_notice() {
                break n;
            }
            assert!(Instant::now() < deadline, "no notice");
            std::thread::sleep(Duration::from_millis(20));
        };
        assert_eq!(
            notice,
            BroadcastNotice::SourceFailed {
                label: "Nothing".into(),
                reason: "screen sharing was cancelled".into()
            }
        );
        viewer.until((640, 360));
        // A real one takes over at its own size.
        control.replace_capture(crate::capture::test_pattern(960, 540, 30), "Other");
        viewer.until((960, 540));
        assert_eq!(
            control.try_notice(),
            Some(BroadcastNotice::SourceChanged("Other".into()))
        );
        assert_eq!(control.source_size(), (960, 540));
        control.stop();
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
