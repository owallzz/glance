//! The voice engine: one thread that every 20 ms takes a mic frame,
//! processes and encodes it, pulls a frame from each peer's jitter buffer,
//! mixes, and hands the mix to the speakers.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;

use super::codec::{VoiceDecoder, VoiceEncoder};
use super::dsp::MicProcessor;
use super::jitter::{JitterBuffer, Pop};
use super::{EncodedAudio, FRAME, SPEAKING_LEVEL, level_of};

/// A peer's identity (its 32-byte key).
pub type PeerKey = [u8; 32];

#[derive(Clone, Debug)]
pub enum InputKind {
    /// A microphone (by name, or the default).
    Device(Option<String>),
    /// A sine tone (bots/tests).
    Tone(f32),
    Silence,
}

#[derive(Clone, Debug)]
pub enum OutputKind {
    Device(Option<String>),
    /// Mix and discard (bots/tests).
    Null,
}

#[derive(Clone, Debug)]
pub struct EngineConfig {
    pub input: InputKind,
    pub output: OutputKind,
    pub echo_cancel: bool,
    pub noise_suppression: bool,
    /// Automatic gain control on the mic.
    pub auto_gain: bool,
}

/// Settings the UI can change while the engine runs.
#[derive(Default)]
struct Shared {
    muted: AtomicBool,
    deafened: AtomicBool,
    stop: AtomicBool,
    my_level: AtomicU8,
    sent: AtomicU32,
    volumes: Mutex<HashMap<PeerKey, f32>>,
    /// Peers silenced for us only (kept apart from `volumes` so unmuting
    /// brings back their volume).
    muted_peers: Mutex<HashSet<PeerKey>>,
    /// Level of each peer's packets as sent (they're talking).
    levels: Mutex<HashMap<PeerKey, (u8, Instant)>>,
    /// Level of each peer in what we actually play (after volume, local
    /// mute and deafen).
    heard: Mutex<HashMap<PeerKey, (u8, Instant)>>,
    removed: Mutex<Vec<PeerKey>>,
    /// New (echo cancel, noise suppression, auto gain) settings to apply.
    dsp: Mutex<Option<(bool, bool, bool)>>,
    /// Mic and speaker volume as f32 bits (1.0 is normal).
    input_gain: AtomicU32,
    output_gain: AtomicU32,
    /// UI sounds waiting to be mixed in (played even when deafened).
    effects: Mutex<std::collections::VecDeque<f32>>,
}

pub struct AudioEngine {
    shared: Arc<Shared>,
    incoming: flume::Sender<(PeerKey, EncodedAudio)>,
    /// Why the engine stopped or degraded (device errors), if it did.
    pub problems: flume::Receiver<String>,
}

impl AudioEngine {
    /// Start the engine. `outgoing` receives each encoded mic frame.
    pub fn start(
        config: EngineConfig,
        outgoing: impl FnMut(EncodedAudio) + Send + 'static,
    ) -> Result<Self> {
        let shared = Arc::new(Shared {
            input_gain: AtomicU32::new(1.0f32.to_bits()),
            output_gain: AtomicU32::new(1.0f32.to_bits()),
            ..Default::default()
        });
        let (in_tx, in_rx) = flume::unbounded();
        let (prob_tx, prob_rx) = flume::bounded(4);
        let (ready_tx, ready_rx) = flume::bounded::<Result<()>>(1);
        let sh = shared.clone();
        std::thread::Builder::new()
            .name("voice".into())
            .spawn(move || run(config, sh, in_rx, outgoing, prob_tx, ready_tx))?;
        ready_rx
            .recv_timeout(Duration::from_secs(10))
            .map_err(|_| anyhow::anyhow!("audio engine didn't start"))??;
        Ok(Self {
            shared,
            incoming: in_tx,
            problems: prob_rx,
        })
    }

    /// Where the network delivers peers' voice packets.
    pub fn incoming(&self) -> flume::Sender<(PeerKey, EncodedAudio)> {
        self.incoming.clone()
    }

    pub fn set_muted(&self, muted: bool) {
        self.shared.muted.store(muted, Ordering::Relaxed);
    }

    /// Play a UI sound through the speakers (mixed with voices).
    pub fn play(&self, sound: super::sounds::Sound) {
        let samples = super::sounds::samples(sound);
        let mut q = self.shared.effects.lock().unwrap();
        // Overlap with anything still playing rather than queueing behind it.
        for (i, s) in samples.into_iter().enumerate() {
            match q.get_mut(i) {
                Some(slot) => *slot += s,
                None => q.push_back(s),
            }
        }
    }

    pub fn set_deafened(&self, deafened: bool) {
        self.shared.deafened.store(deafened, Ordering::Relaxed);
    }

    /// 0.0–2.0, 1.0 is normal.
    pub fn set_peer_volume(&self, peer: PeerKey, volume: f32) {
        self.shared
            .volumes
            .lock()
            .unwrap()
            .insert(peer, volume.clamp(0.0, 2.0));
    }

    /// Silence a peer for us only. Their volume is kept for when they're
    /// unmuted, and they still show as speaking.
    pub fn set_peer_muted(&self, peer: PeerKey, muted: bool) {
        let mut set = self.shared.muted_peers.lock().unwrap();
        if muted {
            set.insert(peer);
        } else {
            set.remove(&peer);
        }
    }

    pub fn peer_muted(&self, peer: &PeerKey) -> bool {
        self.shared.muted_peers.lock().unwrap().contains(peer)
    }

    pub fn peer_volume(&self, peer: &PeerKey) -> f32 {
        self.shared
            .volumes
            .lock()
            .unwrap()
            .get(peer)
            .copied()
            .unwrap_or(1.0)
    }

    /// Mic volume, 0.0–2.0 (applied after processing, so it also scales
    /// what auto gain produces).
    pub fn set_input_volume(&self, volume: f32) {
        self.shared
            .input_gain
            .store(volume.clamp(0.0, 2.0).to_bits(), Ordering::Relaxed);
    }

    /// Speaker volume for everyone you hear, 0.0–2.0.
    pub fn set_output_volume(&self, volume: f32) {
        self.shared
            .output_gain
            .store(volume.clamp(0.0, 2.0).to_bits(), Ordering::Relaxed);
    }

    pub fn set_processing(&self, echo_cancel: bool, noise_suppression: bool, auto_gain: bool) {
        *self.shared.dsp.lock().unwrap() = Some((echo_cancel, noise_suppression, auto_gain));
    }

    pub fn remove_peer(&self, peer: PeerKey) {
        self.shared.removed.lock().unwrap().push(peer);
        self.shared.levels.lock().unwrap().remove(&peer);
        self.shared.heard.lock().unwrap().remove(&peer);
    }

    /// Whether we're currently speaking (unmuted and loud enough).
    pub fn me_speaking(&self) -> bool {
        !self.shared.muted.load(Ordering::Relaxed)
            && self.shared.my_level.load(Ordering::Relaxed) > SPEAKING_LEVEL
    }

    /// Peers speaking in the last 300 ms (whether or not we play them).
    pub fn speaking_peers(&self) -> Vec<PeerKey> {
        recent_loud(&self.shared.levels.lock().unwrap())
    }

    /// Peers loud enough in what reaches the speakers in the last 300 ms:
    /// like [`Self::speaking_peers`] minus locally muted (or zero volume)
    /// peers, and nobody while deafened.
    pub fn audible_peers(&self) -> Vec<PeerKey> {
        recent_loud(&self.shared.heard.lock().unwrap())
    }

    pub fn frames_sent(&self) -> u32 {
        self.shared.sent.load(Ordering::Relaxed)
    }
}

fn recent_loud(levels: &HashMap<PeerKey, (u8, Instant)>) -> Vec<PeerKey> {
    let now = Instant::now();
    let mut out: Vec<PeerKey> = levels
        .iter()
        .filter(|(_, (lvl, at))| *lvl > SPEAKING_LEVEL && now - *at < Duration::from_millis(300))
        .map(|(k, _)| *k)
        .collect();
    out.sort_unstable();
    out
}

impl Drop for AudioEngine {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Relaxed);
    }
}

struct Peer {
    jitter: JitterBuffer,
    decoder: VoiceDecoder,
}

fn run(
    config: EngineConfig,
    shared: Arc<Shared>,
    incoming: flume::Receiver<(PeerKey, EncodedAudio)>,
    mut outgoing: impl FnMut(EncodedAudio),
    problems: flume::Sender<String>,
    ready: flume::Sender<Result<()>>,
) {
    // Rings hold ~0.5 s; the engine keeps playback ~40–100 ms ahead.
    let (mic_tx, mut mic_rx) = rtrb::RingBuffer::<f32>::new(FRAME * 25);
    let (spk_tx, spk_rx) = rtrb::RingBuffer::<f32>::new(FRAME * 25);
    let mut spk_tx = spk_tx;

    // Streams aren't Send everywhere: open them on this thread.
    let mut _mic_stream = None;
    if let InputKind::Device(name) = &config.input {
        match super::device::open_input(name.as_deref(), mic_tx) {
            Ok(s) => _mic_stream = Some(s),
            Err(e) => {
                let _ = problems.try_send(format!("Microphone unavailable: {e:#}"));
            }
        }
    }
    let mut _spk_stream = None;
    if let OutputKind::Device(name) = &config.output {
        match super::device::open_output(name.as_deref(), spk_rx) {
            Ok(s) => _spk_stream = Some(s),
            Err(e) => {
                let _ = problems.try_send(format!("Speakers unavailable: {e:#}"));
            }
        }
    }
    let mic_live = _mic_stream.is_some();
    let spk_live = _spk_stream.is_some();

    let mut encoder = match VoiceEncoder::new(32_000) {
        Ok(e) => e,
        Err(e) => {
            let _ = ready.send(Err(e));
            return;
        }
    };
    let _ = ready.send(Ok(()));
    let mut dsp = MicProcessor::new(config.echo_cancel, config.noise_suppression);
    dsp.set_auto_gain(config.auto_gain);
    let mut peers: HashMap<PeerKey, Peer> = HashMap::new();
    let mut mic = vec![0.0f32; FRAME];
    let mut far_end = vec![0.0f32; FRAME];
    let mut mix = vec![0.0f32; FRAME];
    let mut decoded = vec![0.0f32; FRAME];
    let mut seq: u32 = 0;
    let mut tone_t: usize = 0;
    let frame_dur = Duration::from_millis(20);
    let mut next_tick = Instant::now();

    while !shared.stop.load(Ordering::Relaxed) {
        // ---- clock: the mic when we have one, otherwise a 20 ms timer ----
        if mic_live {
            while mic_rx.slots() < FRAME {
                if shared.stop.load(Ordering::Relaxed) {
                    return;
                }
                std::thread::sleep(Duration::from_millis(2));
            }
            for s in mic.iter_mut() {
                *s = mic_rx.pop().unwrap_or(0.0);
            }
        } else {
            next_tick += frame_dur;
            if let Some(wait) = next_tick.checked_duration_since(Instant::now()) {
                std::thread::sleep(wait);
            } else {
                next_tick = Instant::now();
            }
            match &config.input {
                InputKind::Tone(hz) => {
                    for (i, s) in mic.iter_mut().enumerate() {
                        *s = ((tone_t + i) as f32 * hz * std::f32::consts::TAU / 48_000.0).sin()
                            * 0.3;
                    }
                    tone_t += FRAME;
                }
                _ => mic.fill(0.0),
            }
        }

        if let Some((aec, ns, agc)) = shared.dsp.lock().unwrap().take() {
            dsp.set_auto_gain(agc);
            dsp.set_echo_cancel(aec);
            dsp.set_noise_suppression(ns);
        }
        for key in shared.removed.lock().unwrap().drain(..) {
            peers.remove(&key);
        }

        // ---- send ----
        if !matches!(config.input, InputKind::Tone(_)) {
            dsp.process(&mut mic, &far_end);
            let gain = f32::from_bits(shared.input_gain.load(Ordering::Relaxed));
            if gain != 1.0 {
                for s in mic.iter_mut() {
                    *s = (*s * gain).clamp(-1.0, 1.0);
                }
            }
        }
        let level = level_of(&mic);
        shared.my_level.store(level, Ordering::Relaxed);
        if !shared.muted.load(Ordering::Relaxed) {
            match encoder.encode(&mic) {
                Ok(data) => {
                    outgoing(EncodedAudio { seq, level, data });
                    seq = seq.wrapping_add(1);
                    shared.sent.fetch_add(1, Ordering::Relaxed);
                }
                Err(e) => tracing::warn!("voice encode failed: {e:#}"),
            }
        }

        // ---- receive ----
        while let Ok((key, pkt)) = incoming.try_recv() {
            let peer = match peers.entry(key) {
                std::collections::hash_map::Entry::Occupied(o) => o.into_mut(),
                std::collections::hash_map::Entry::Vacant(v) => match VoiceDecoder::new() {
                    Ok(decoder) => v.insert(Peer {
                        jitter: JitterBuffer::default(),
                        decoder,
                    }),
                    Err(_) => continue,
                },
            };
            shared
                .levels
                .lock()
                .unwrap()
                .insert(key, (pkt.level, Instant::now()));
            peer.jitter.insert(pkt.seq, pkt.data);
        }

        // ---- mix ----
        mix.fill(0.0);
        let volumes = shared.volumes.lock().unwrap().clone();
        let muted_peers = shared.muted_peers.lock().unwrap().clone();
        let deafened = shared.deafened.load(Ordering::Relaxed);
        let master = f32::from_bits(shared.output_gain.load(Ordering::Relaxed));
        let mut heard = Vec::new();
        for (key, peer) in peers.iter_mut() {
            let ok = match peer.jitter.pop() {
                Pop::Packet(data) => peer.decoder.decode(&data, &mut decoded).is_ok(),
                Pop::Lost { next: Some(next) } => peer.decoder.recover(&next, &mut decoded).is_ok(),
                Pop::Lost { next: None } => peer.decoder.conceal(&mut decoded).is_ok(),
                Pop::Empty => false,
            };
            // Muted peers are still decoded so their decoder and jitter
            // buffer stay in step for when they're unmuted.
            if ok && !deafened && !muted_peers.contains(key) {
                let vol = volumes.get(key).copied().unwrap_or(1.0) * master;
                if vol > 0.0 {
                    for (m, d) in mix.iter_mut().zip(&decoded) {
                        *m += d * vol;
                    }
                    for d in decoded.iter_mut() {
                        *d *= vol;
                    }
                    heard.push((*key, level_of(&decoded)));
                }
            }
        }
        if !heard.is_empty() {
            let now = Instant::now();
            let mut h = shared.heard.lock().unwrap();
            for (key, level) in heard {
                h.insert(key, (level, now));
            }
        }
        for m in mix.iter_mut() {
            // Soft clip so several loud speakers don't distort harshly.
            *m = m.tanh();
        }

        // ---- play ----
        // (Deafened: no peer was mixed in above; sounds still play.)
        {
            let mut fx = shared.effects.lock().unwrap();
            if !fx.is_empty() {
                for m in mix.iter_mut() {
                    match fx.pop_front() {
                        Some(s) => *m = (*m + s).clamp(-1.0, 1.0),
                        None => break,
                    }
                }
            }
        }
        // (Sounds are in the echo canceller's reference, so the mic doesn't
        // send them back.)
        far_end.copy_from_slice(&mix);
        if spk_live {
            let queued = spk_tx.buffer().capacity() - spk_tx.slots();
            // Keep 40–120 ms queued: skip a frame when far ahead (clock
            // drift), add one when nearly empty.
            let copies = if queued > FRAME * 6 {
                0
            } else if queued < FRAME * 2 {
                2
            } else {
                1
            };
            for _ in 0..copies {
                for &s in &mix {
                    if spk_tx.push(s).is_err() {
                        break;
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two engines with synthetic I/O talk to each other through channels.
    #[test]
    fn tone_from_one_engine_is_heard_by_another() {
        let (b_in_tx, b_in_rx) = flume::unbounded::<(PeerKey, EncodedAudio)>();
        let a = AudioEngine::start(
            EngineConfig {
                input: InputKind::Tone(440.0),
                output: OutputKind::Null,
                echo_cancel: false,
                noise_suppression: false,
                auto_gain: false,
            },
            move |pkt| {
                let _ = b_in_tx.send(([1; 32], pkt));
            },
        )
        .unwrap();
        let b = AudioEngine::start(
            EngineConfig {
                input: InputKind::Silence,
                output: OutputKind::Null,
                echo_cancel: true,
                noise_suppression: true,
                auto_gain: true,
            },
            |_| {},
        )
        .unwrap();
        let fwd = b.incoming();
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let pump_stop = stop.clone();
        let pump = std::thread::spawn(move || {
            while !pump_stop.load(std::sync::atomic::Ordering::Relaxed) {
                if let Ok(m) = b_in_rx.recv_timeout(Duration::from_millis(50)) {
                    let _ = fwd.send(m);
                }
            }
        });
        // Poll instead of sleeping a fixed time: slow CI machines (shared macOS
        // VMs) run the engine threads late.
        let wait_until = |cond: &dyn Fn() -> bool| {
            let deadline = Instant::now() + Duration::from_secs(5);
            while !cond() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(20));
            }
        };
        wait_until(&|| a.frames_sent() >= 20 && b.speaking_peers() == vec![[1; 32]]);
        assert!(a.frames_sent() >= 20, "sent {}", a.frames_sent());
        assert_eq!(b.speaking_peers(), vec![[1; 32]]);
        assert!(!b.me_speaking());
        a.set_muted(true);
        wait_until(&|| b.speaking_peers().is_empty());
        assert!(b.speaking_peers().is_empty(), "muted peer still speaking");
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        pump.join().unwrap();
    }

    fn synthetic(input: InputKind) -> EngineConfig {
        EngineConfig {
            input,
            output: OutputKind::Null,
            echo_cancel: false,
            noise_suppression: false,
            auto_gain: false,
        }
    }

    fn wait_until(cond: &dyn Fn() -> bool) -> bool {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !cond() {
            if Instant::now() > deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        true
    }

    /// Two tone engines talk into a listener; muting one locally silences
    /// it in the mix while the other is still played, and unmuting brings
    /// back the volume it had.
    #[test]
    fn locally_muted_peer_is_silent_and_unmuting_restores_volume() {
        let listener = AudioEngine::start(synthetic(InputKind::Silence), |_| {}).unwrap();
        let (alice, bob) = ([1u8; 32], [2u8; 32]);
        let mut talkers = Vec::new();
        for (key, hz) in [(alice, 440.0), (bob, 660.0)] {
            let into = listener.incoming();
            talkers.push(
                AudioEngine::start(synthetic(InputKind::Tone(hz)), move |pkt| {
                    let _ = into.send((key, pkt));
                })
                .unwrap(),
            );
        }
        assert!(
            wait_until(&|| listener.audible_peers() == vec![alice, bob]),
            "both should be heard, got {:?}",
            listener.audible_peers()
        );

        listener.set_peer_volume(alice, 0.5);
        listener.set_peer_muted(alice, true);
        assert!(listener.peer_muted(&alice));
        assert!(
            wait_until(&|| listener.audible_peers() == vec![bob]),
            "muted peer still heard: {:?}",
            listener.audible_peers()
        );
        // Still shown as talking, just not played.
        assert_eq!(listener.speaking_peers(), vec![alice, bob]);

        listener.set_peer_muted(alice, false);
        assert_eq!(listener.peer_volume(&alice), 0.5);
        assert!(
            wait_until(&|| listener.audible_peers() == vec![alice, bob]),
            "unmuted peer not heard: {:?}",
            listener.audible_peers()
        );

        // Deafened: nobody is played (voice and stream audio alike).
        listener.set_deafened(true);
        assert!(wait_until(&|| listener.audible_peers().is_empty()));
        assert_eq!(listener.speaking_peers(), vec![alice, bob]);
        listener.set_deafened(false);
        assert!(wait_until(&|| listener.audible_peers() == vec![alice, bob]));
        drop(talkers);
    }
}
