//! Opus voice codec, 48 kHz mono, 20 ms frames.

use anyhow::{Context, Result};
use opus::{Application, Bitrate, Channels, Decoder, Encoder};

use super::FRAME;

pub struct VoiceEncoder {
    enc: Encoder,
    buf: Vec<u8>,
}

impl VoiceEncoder {
    pub fn new(bitrate: i32) -> Result<Self> {
        let mut enc = Encoder::new(super::SAMPLE_RATE, Channels::Mono, Application::Voip)
            .context("opus encoder")?;
        enc.set_bitrate(Bitrate::Bits(bitrate))?;
        // In-band FEC lets the receiver rebuild a lost frame from the next one.
        enc.set_inband_fec(true)?;
        enc.set_packet_loss_perc(10)?;
        // Discontinuous transmission: near-empty packets during silence.
        enc.set_dtx(true)?;
        Ok(Self {
            enc,
            buf: vec![0; 1500],
        })
    }

    /// Encode one 20 ms frame.
    pub fn encode(&mut self, frame: &[f32]) -> Result<Vec<u8>> {
        debug_assert_eq!(frame.len(), FRAME);
        let n = self
            .enc
            .encode_float(frame, &mut self.buf)
            .context("opus encode")?;
        Ok(self.buf[..n].to_vec())
    }
}

pub struct VoiceDecoder {
    dec: Decoder,
}

impl VoiceDecoder {
    pub fn new() -> Result<Self> {
        Ok(Self {
            dec: Decoder::new(super::SAMPLE_RATE, Channels::Mono).context("opus decoder")?,
        })
    }

    /// Decode a packet into `out` (one frame).
    pub fn decode(&mut self, data: &[u8], out: &mut [f32]) -> Result<()> {
        let n = self.dec.decode_float(data, out, false)?;
        out[n..].fill(0.0);
        Ok(())
    }

    /// Rebuild a lost frame from the *next* packet's FEC data.
    pub fn recover(&mut self, next: &[u8], out: &mut [f32]) -> Result<()> {
        let n = self.dec.decode_float(next, out, true)?;
        out[n..].fill(0.0);
        Ok(())
    }

    /// Conceal a lost frame with no data at all.
    pub fn conceal(&mut self, out: &mut [f32]) -> Result<()> {
        let n = self.dec.decode_float(&[], out, false)?;
        out[n..].fill(0.0);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(t0: usize) -> Vec<f32> {
        (0..FRAME)
            .map(|i| ((t0 + i) as f32 * 440.0 * std::f32::consts::TAU / 48_000.0).sin() * 0.3)
            .collect()
    }

    #[test]
    fn round_trip_keeps_energy() {
        let mut enc = VoiceEncoder::new(32_000).unwrap();
        let mut dec = VoiceDecoder::new().unwrap();
        let mut out = vec![0.0; FRAME];
        let mut last_level = 0;
        for k in 0..20 {
            let pkt = enc.encode(&tone(k * FRAME)).unwrap();
            assert!(pkt.len() < 400);
            dec.decode(&pkt, &mut out).unwrap();
            last_level = super::super::level_of(&out);
        }
        assert!(last_level > 50, "decoded level {last_level}");
    }

    #[test]
    fn loss_is_concealed() {
        let mut enc = VoiceEncoder::new(32_000).unwrap();
        let mut dec = VoiceDecoder::new().unwrap();
        let mut out = vec![0.0; FRAME];
        let pkts: Vec<_> = (0..10)
            .map(|k| enc.encode(&tone(k * FRAME)).unwrap())
            .collect();
        for p in &pkts[..5] {
            dec.decode(p, &mut out).unwrap();
        }
        dec.recover(&pkts[6], &mut out).unwrap();
        assert!(super::super::level_of(&out) > 30);
        dec.conceal(&mut out).unwrap();
    }
}
