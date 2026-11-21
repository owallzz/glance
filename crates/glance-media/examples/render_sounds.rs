//! Render glance's synthesized sounds to WAV files and print an analysis
//! (peak, loudness, DC, spectrum, loop seams).
//!
//! cargo run -p glance-media --example render_sounds -- [out_dir]

use glance_media::audio::SAMPLE_RATE;
use glance_media::audio::ringtone::{self, Tone};
use glance_media::audio::sounds;

fn main() -> anyhow::Result<()> {
    let dir = std::path::PathBuf::from(
        std::env::args()
            .nth(1)
            .unwrap_or_else(|| "glance-sounds".into()),
    );
    std::fs::create_dir_all(&dir)?;
    println!(
        "{:<18} {:>6} {:>8} {:>8} {:>9} {:>8} {:>8} {:>8} {:>9}",
        "sound", "secs", "peak dB", "rms dB", "loud dB", "dc", ">5k %", ">8k %", "centroid"
    );
    for (name, tone) in [("ring_in", Tone::Incoming), ("ring_out", Tone::Outgoing)] {
        let r = ringtone::rendered(tone);
        // First ring plus three loops, as you'd hear it.
        let n = r.first.len() + 3 * r.looped.len();
        let x: Vec<f32> = (0..n).map(|i| r.sample(i)).collect();
        report(&format!("{name}"), &x);
        report(&format!("{name} (loop)"), &r.looped);
        write_wav(&dir.join(format!("{name}.wav")), &x)?;
        let l = r.looped.len();
        let seam = |at: usize| step(&x[at - 4..at + 4]);
        println!(
            "  seams: first->loop {:.5}, loop->loop {:.5}, typical max step {:.5}, last-vs-first sample {:.6}",
            seam(r.first.len()),
            seam(r.first.len() + l),
            step(&r.looped),
            (r.looped[l - 1] - r.looped[0]).abs(),
        );
    }
    for s in sounds::ALL {
        let x = sounds::samples(s);
        let name = format!("{s:?}").to_lowercase();
        report(&name, &x);
        write_wav(&dir.join(format!("{name}.wav")), &x)?;
    }
    println!("wrote {}", dir.display());
    Ok(())
}

fn db(x: f32) -> f32 {
    20.0 * x.max(1e-9).log10()
}

fn step(x: &[f32]) -> f32 {
    x.windows(2)
        .map(|w| (w[1] - w[0]).abs())
        .fold(0.0, f32::max)
}

fn report(name: &str, x: &[f32]) {
    let sr = SAMPLE_RATE as usize;
    let peak = x.iter().fold(0.0f32, |m, s| m.max(s.abs()));
    let rms = (x.iter().map(|s| s * s).sum::<f32>() / x.len() as f32).sqrt();
    // Loudest 400 ms window: roughly how loud it feels.
    let win = sr * 2 / 5;
    let loud = x
        .chunks(sr / 20)
        .enumerate()
        .map(|(k, _)| {
            let a = k * sr / 20;
            let w = &x[a..(a + win).min(x.len())];
            (w.iter().map(|s| s * s).sum::<f32>() / win as f32).sqrt()
        })
        .fold(0.0f32, f32::max);
    let dc = x.iter().sum::<f32>() / x.len() as f32;
    let (above5, above8, centroid) = spectrum(x);
    println!(
        "{name:<18} {:>6.2} {:>8.1} {:>8.1} {:>9.1} {:>8.5} {:>8.3} {:>8.4} {:>8.0}Hz",
        x.len() as f32 / sr as f32,
        db(peak),
        db(rms),
        db(loud),
        dc,
        above5 * 100.0,
        above8 * 100.0,
        centroid
    );
}

/// Energy share above 5 and 8 kHz and the spectral centroid, from
/// Hann-windowed 4096-point FFTs over the whole signal.
fn spectrum(x: &[f32]) -> (f32, f32, f32) {
    const N: usize = 4096;
    let sr = SAMPLE_RATE as f32;
    let (mut total, mut a5, mut a8, mut moment) = (0.0f64, 0.0f64, 0.0f64, 0.0f64);
    let mut start = 0;
    while start < x.len() {
        let mut re = [0.0f64; N];
        let mut im = [0.0f64; N];
        for i in 0..N {
            let s = x.get(start + i).copied().unwrap_or(0.0) as f64;
            let w = 0.5 - 0.5 * (std::f64::consts::TAU * i as f64 / N as f64).cos();
            re[i] = s * w;
        }
        fft(&mut re, &mut im);
        for k in 1..N / 2 {
            let p = re[k] * re[k] + im[k] * im[k];
            let f = k as f32 * sr / N as f32;
            total += p;
            moment += p * f as f64;
            if f > 5_000.0 {
                a5 += p;
            }
            if f > 8_000.0 {
                a8 += p;
            }
        }
        start += N / 2;
    }
    let t = total.max(1e-30);
    ((a5 / t) as f32, (a8 / t) as f32, (moment / t) as f32)
}

/// In-place radix-2 FFT.
fn fft(re: &mut [f64], im: &mut [f64]) {
    let n = re.len();
    let mut j = 0;
    for i in 1..n {
        let mut bit = n >> 1;
        while j & bit != 0 {
            j ^= bit;
            bit >>= 1;
        }
        j |= bit;
        if i < j {
            re.swap(i, j);
            im.swap(i, j);
        }
    }
    let mut len = 2;
    while len <= n {
        let ang = -std::f64::consts::TAU / len as f64;
        for i in (0..n).step_by(len) {
            for k in 0..len / 2 {
                let (s, c) = (ang * k as f64).sin_cos();
                let (a, b) = (i + k, i + k + len / 2);
                let tr = re[b] * c - im[b] * s;
                let ti = re[b] * s + im[b] * c;
                re[b] = re[a] - tr;
                im[b] = im[a] - ti;
                re[a] += tr;
                im[a] += ti;
            }
        }
        len <<= 1;
    }
}

/// 16-bit mono PCM WAV.
fn write_wav(path: &std::path::Path, x: &[f32]) -> std::io::Result<()> {
    let data_len = (x.len() * 2) as u32;
    let mut b = Vec::with_capacity(44 + x.len() * 2);
    b.extend_from_slice(b"RIFF");
    b.extend_from_slice(&(36 + data_len).to_le_bytes());
    b.extend_from_slice(b"WAVEfmt ");
    b.extend_from_slice(&16u32.to_le_bytes());
    b.extend_from_slice(&1u16.to_le_bytes()); // PCM
    b.extend_from_slice(&1u16.to_le_bytes()); // mono
    b.extend_from_slice(&SAMPLE_RATE.to_le_bytes());
    b.extend_from_slice(&(SAMPLE_RATE * 2).to_le_bytes());
    b.extend_from_slice(&2u16.to_le_bytes());
    b.extend_from_slice(&16u16.to_le_bytes());
    b.extend_from_slice(b"data");
    b.extend_from_slice(&data_len.to_le_bytes());
    for s in x {
        b.extend_from_slice(&((s.clamp(-1.0, 1.0) * 32767.0).round() as i16).to_le_bytes());
    }
    std::fs::write(path, b)
}
