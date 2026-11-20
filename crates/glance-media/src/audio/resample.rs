//! Tiny streaming converters between device formats and 48 kHz mono.

/// Streaming linear-interpolation resampler for one channel.
pub struct Resampler {
    ratio: f64,
    pos: f64,
    prev: f32,
}

impl Resampler {
    pub fn new(from_hz: u32, to_hz: u32) -> Self {
        Self {
            ratio: from_hz as f64 / to_hz as f64,
            pos: 0.0,
            prev: 0.0,
        }
    }

    pub fn is_identity(&self) -> bool {
        (self.ratio - 1.0).abs() < 1e-9
    }

    /// Resample `input`, appending to `out`. Keeps state across calls.
    pub fn process(&mut self, input: &[f32], out: &mut Vec<f32>) {
        if self.is_identity() {
            out.extend_from_slice(input);
            return;
        }
        // `pos` is the fractional read position, where -1.0..0.0 means
        // between the previous call's last sample (`prev`) and input[0].
        while self.pos < input.len() as f64 - 1.0 + 1e-9 || self.pos < 0.0 {
            let i = self.pos.floor();
            let frac = (self.pos - i) as f32;
            let (a, b) = if i < 0.0 {
                (self.prev, input.first().copied().unwrap_or(self.prev))
            } else {
                let i = i as usize;
                if i + 1 >= input.len() {
                    break;
                }
                (input[i], input[i + 1])
            };
            out.push(a + (b - a) * frac);
            self.pos += self.ratio;
        }
        if let Some(&last) = input.last() {
            self.prev = last;
            self.pos -= input.len() as f64;
        }
    }
}

/// Average interleaved channels into mono.
pub fn downmix(interleaved: &[f32], channels: usize, out: &mut Vec<f32>) {
    if channels <= 1 {
        out.extend_from_slice(interleaved);
        return;
    }
    let inv = 1.0 / channels as f32;
    for frame in interleaved.chunks_exact(channels) {
        out.push(frame.iter().sum::<f32>() * inv);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_passes_through() {
        let mut r = Resampler::new(48_000, 48_000);
        let mut out = Vec::new();
        r.process(&[0.1, 0.2, 0.3], &mut out);
        assert_eq!(out, vec![0.1, 0.2, 0.3]);
    }

    #[test]
    fn rate_conversion_keeps_duration() {
        for (from, to) in [
            (44_100, 48_000),
            (48_000, 44_100),
            (16_000, 48_000),
            (96_000, 48_000),
        ] {
            let mut r = Resampler::new(from, to);
            let mut out = Vec::new();
            // One second in 10 ms chunks.
            let chunk = from as usize / 100;
            for c in 0..100 {
                let input: Vec<f32> = (0..chunk)
                    .map(|i| ((c * chunk + i) as f32 * 0.01).sin())
                    .collect();
                r.process(&input, &mut out);
            }
            let diff = (out.len() as i64 - to as i64).abs();
            assert!(diff <= 4, "{from}->{to}: got {} samples", out.len());
        }
    }

    #[test]
    fn downmix_averages() {
        let mut out = Vec::new();
        downmix(&[1.0, 0.0, 0.5, 0.5], 2, &mut out);
        assert_eq!(out, vec![0.5, 0.5]);
    }
}
