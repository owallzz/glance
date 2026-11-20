//! A synthetic video source: moving colour bars at any resolution and frame
//! rate. Used by the render spike, tests, and later `glance bot`.

use std::time::{Duration, Instant};

use crate::frame::{FrameScaler, TargetSize, VideoFrame};

/// Produces frames by copying rows out of a precomputed, double-width strip,
/// so the cost is memory bandwidth (like a decoder's output), not maths.
pub struct Pattern {
    width: u32,
    height: u32,
    strip: Vec<u8>,
    t: u32,
}

impl Pattern {
    pub fn new(width: u32, height: u32, seed: u8) -> Self {
        let sw = width as usize * 2;
        let mut strip = vec![0u8; sw * 4 * 64];
        for (y, row) in strip.chunks_exact_mut(sw * 4).enumerate() {
            for (x, px) in row.chunks_exact_mut(4).enumerate() {
                let band = ((x + y) / 96) as u8;
                px.copy_from_slice(&[
                    band.wrapping_mul(37).wrapping_add(seed),
                    band.wrapping_mul(71),
                    band.wrapping_mul(113).wrapping_add(seed.wrapping_mul(3)),
                    255,
                ]);
            }
        }
        Self {
            width,
            height,
            strip,
            t: 0,
        }
    }

    pub fn next_frame(&mut self) -> VideoFrame {
        let (w, h) = (self.width as usize, self.height as usize);
        let sw = w * 2;
        let shift = (self.t as usize * 6) % w.max(1);
        let mut bgra = Vec::with_capacity(w * h * 4);
        for y in 0..h {
            let row = &self.strip[(y % 64) * sw * 4..][..sw * 4];
            bgra.extend_from_slice(&row[shift * 4..(shift + w) * 4]);
        }
        self.t = self.t.wrapping_add(1);
        VideoFrame {
            width: self.width,
            height: self.height,
            bgra,
        }
    }
}

/// Spawn a thread producing pattern frames at `fps`, scaled to `target`.
/// Stops when the receiver is dropped.
pub fn spawn_pattern(
    width: u32,
    height: u32,
    fps: u32,
    seed: u8,
    target: TargetSize,
) -> flume::Receiver<VideoFrame> {
    let (tx, rx) = flume::bounded(2);
    std::thread::Builder::new()
        .name("pattern".into())
        .spawn(move || {
            let mut pattern = Pattern::new(width, height, seed);
            let mut scaler = FrameScaler::default();
            let period = Duration::from_secs_f64(1.0 / fps.max(1) as f64);
            let mut next = Instant::now();
            loop {
                let frame = scaler.fit(pattern.next_frame(), target.get());
                if tx.send(frame).is_err() {
                    break;
                }
                next += period;
                match next.checked_duration_since(Instant::now()) {
                    Some(wait) => std::thread::sleep(wait),
                    None => next = Instant::now(),
                }
            }
        })
        .expect("spawn pattern thread");
    rx
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_have_requested_size_and_move() {
        let mut p = Pattern::new(320, 180, 0);
        let a = p.next_frame();
        let b = p.next_frame();
        assert!(a.is_valid() && b.is_valid());
        assert_eq!((a.width, a.height), (320, 180));
        assert_ne!(a.bgra, b.bgra);
    }
}
