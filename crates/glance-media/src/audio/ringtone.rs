//! A synthesized ringtone (no audio files to ship): two soft tones, two
//! seconds on, three off, until dropped.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::Result;

use super::SAMPLE_RATE;

pub struct Ringtone {
    stop: Arc<AtomicBool>,
}

impl Drop for Ringtone {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// Sample `i` of the ring pattern (48 kHz).
pub fn sample(i: usize) -> f32 {
    let t = i as f32 / SAMPLE_RATE as f32;
    let cycle = t % 5.0;
    if cycle > 2.0 {
        return 0.0;
    }
    // Short fades avoid clicks at the edges.
    let env = (cycle / 0.05).min(1.0) * ((2.0 - cycle) / 0.05).min(1.0);
    let tau = std::f32::consts::TAU;
    ((tau * 523.25 * t).sin() * 0.5 + (tau * 659.25 * t).sin() * 0.5) * 0.18 * env
}

impl Ringtone {
    /// Start ringing on `output` (default device when `None`).
    pub fn start(output: Option<String>) -> Result<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = stop.clone();
        let (ready_tx, ready_rx) = flume::bounded::<Result<()>>(1);
        std::thread::Builder::new()
            .name("ringtone".into())
            .spawn(move || {
                let (mut tx, rx) = rtrb::RingBuffer::<f32>::new(SAMPLE_RATE as usize / 2);
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
                        let _ = tx.push(sample(i));
                        i += 1;
                    }
                    std::thread::sleep(Duration::from_millis(50));
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

    #[test]
    fn pattern_rings_then_pauses() {
        let sr = SAMPLE_RATE as usize;
        let on: f32 = (sr / 2..sr).map(|i| sample(i).abs()).sum::<f32>() / (sr / 2) as f32;
        let off: f32 = (3 * sr..4 * sr).map(|i| sample(i).abs()).sum::<f32>();
        assert!(on > 0.02, "ringing too quiet: {on}");
        assert_eq!(off, 0.0);
        assert!((0..10 * sr).all(|i| sample(i).abs() <= 0.2));
    }
}
