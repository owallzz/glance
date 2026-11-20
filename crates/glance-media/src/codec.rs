//! H.264 encode/decode with OpenH264 (software, every platform).
//!
//! The encoder is tuned for screen content in real time. Hardware encoders
//! (needed above 1080p60) come later behind the same types.

use std::time::Instant;

use anyhow::{Context, Result, bail};
use openh264::OpenH264API;
use openh264::decoder::Decoder;
use openh264::encoder::{
    BitRate, Complexity, Encoder, EncoderConfig, FrameRate, FrameType, IntraFramePeriod,
    RateControlMode, SpsPpsStrategy, UsageType,
};
use openh264::formats::{BgraSliceU8, YUVBuffer, YUVSource};

use crate::frame::{FrameScaler, VideoFrame};

/// One encoded access unit (Annex-B NAL units for one picture).
#[derive(Clone, Debug)]
pub struct EncodedPacket {
    pub keyframe: bool,
    /// Capture time relative to the stream start, microseconds.
    pub ts_us: u64,
    pub width: u32,
    pub height: u32,
    pub data: Vec<u8>,
}

/// A bitrate that looks good for screen content at this size and rate.
pub fn bitrate_for(width: u32, height: u32, fps: u32) -> u32 {
    let pixels = width as f64 * height as f64;
    // ~0.07 bits/pixel/frame at 30 fps; higher rates need less per frame.
    let per_frame = 0.07 * (30.0 / fps.max(1) as f64).sqrt();
    let bps = pixels * fps.max(1) as f64 * per_frame;
    (bps as u32).clamp(800_000, 40_000_000)
}

pub struct VideoEncoder {
    encoder: Encoder,
    width: u32,
    height: u32,
    yuv: YUVBuffer,
    start: Instant,
}

impl VideoEncoder {
    /// `width` and `height` must be even.
    pub fn new(width: u32, height: u32, fps: u32, bitrate_bps: u32) -> Result<Self> {
        if width == 0 || height == 0 || width % 2 == 1 || height % 2 == 1 {
            bail!("encoder size must be even and non-zero, got {width}x{height}");
        }
        let threads = std::thread::available_parallelism()
            .map(|n| n.get().min(4) as u16)
            .unwrap_or(1);
        let config = EncoderConfig::new()
            .usage_type(UsageType::ScreenContentRealTime)
            .rate_control_mode(RateControlMode::Bitrate)
            .bitrate(BitRate::from_bps(bitrate_bps))
            .max_frame_rate(FrameRate::from_hz(fps.max(1) as f32))
            .skip_frames(true)
            // Real-time first: the lowest complexity roughly halves encode
            // time at a small quality cost.
            .complexity(Complexity::Low)
            .adaptive_quantization(false)
            .background_detection(false)
            .sps_pps_strategy(SpsPpsStrategy::ConstantId)
            // A keyframe every ~4 s bounds how long a lost packet hurts;
            // new viewers ask for one explicitly.
            .intra_frame_period(IntraFramePeriod::from_num_frames(fps.max(1) * 4))
            .num_threads(threads);
        let encoder = Encoder::with_api_config(OpenH264API::from_source(), config)
            .context("create H.264 encoder")?;
        Ok(Self {
            encoder,
            width,
            height,
            yuv: YUVBuffer::new(width as usize, height as usize),
            start: Instant::now(),
        })
    }

    pub fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// Change the target bitrate on the fly (no new keyframe).
    pub fn set_bitrate(&mut self, bps: u32) {
        let mut info = openh264_sys2::SBitrateInfo {
            iLayer: openh264_sys2::SPATIAL_LAYER_ALL,
            iBitrate: bps.min(i32::MAX as u32) as i32,
        };
        // SAFETY: the encoder is initialized; SetOption copies the struct.
        unsafe {
            self.encoder.raw_api().set_option(
                openh264_sys2::ENCODER_OPTION_BITRATE,
                (&mut info as *mut openh264_sys2::SBitrateInfo).cast(),
            );
        }
    }

    /// Ask for the next frame to be a keyframe (new viewer, packet loss).
    pub fn force_keyframe(&mut self) {
        self.encoder.force_intra_frame();
    }

    /// Encode a frame of exactly the encoder's size. Returns `None` when the
    /// rate controller skipped the frame.
    pub fn encode(&mut self, frame: &VideoFrame) -> Result<Option<EncodedPacket>> {
        if (frame.width, frame.height) != (self.width, self.height) {
            bail!(
                "frame is {}x{}, encoder expects {}x{}",
                frame.width,
                frame.height,
                self.width,
                self.height
            );
        }
        let bgra = BgraSliceU8::new(&frame.bgra, (self.width as usize, self.height as usize));
        self.yuv.read_bgra8(bgra);
        let ts_us = self.start.elapsed().as_micros() as u64;
        let bitstream = self.encoder.encode(&self.yuv).context("encode frame")?;
        let keyframe = matches!(bitstream.frame_type(), FrameType::IDR | FrameType::I);
        if matches!(bitstream.frame_type(), FrameType::Skip | FrameType::Invalid) {
            return Ok(None);
        }
        let data = bitstream.to_vec();
        if data.is_empty() {
            return Ok(None);
        }
        Ok(Some(EncodedPacket {
            keyframe,
            ts_us,
            width: self.width,
            height: self.height,
            data,
        }))
    }
}

pub struct VideoDecoder {
    decoder: Decoder,
    scaler: FrameScaler,
    rgba: Vec<u8>,
}

impl VideoDecoder {
    pub fn new() -> Result<Self> {
        Ok(Self {
            decoder: Decoder::new().context("create H.264 decoder")?,
            scaler: FrameScaler::default(),
            rgba: Vec::new(),
        })
    }

    /// Decode one packet and return a BGRA frame scaled to fit `target`
    /// (or full size if `target` is unknown). `None` while the decoder is
    /// still waiting for a keyframe.
    pub fn decode(&mut self, data: &[u8], target: (u32, u32)) -> Result<Option<VideoFrame>> {
        let Some(yuv) = self.decoder.decode(data).context("decode packet")? else {
            return Ok(None);
        };
        let (w, h) = yuv.dimensions();
        self.rgba.resize(w * h * 4, 0);
        yuv.write_rgba8(&mut self.rgba);
        let mut bgra = std::mem::take(&mut self.rgba);
        for px in bgra.as_chunks_mut::<4>().0 {
            px.swap(0, 2);
        }
        let frame = VideoFrame {
            width: w as u32,
            height: h as u32,
            bgra,
        };
        let out = self.scaler.fit(frame, target);
        Ok(Some(out))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pattern::Pattern;

    #[test]
    fn encode_decode_round_trip() {
        let (w, h) = (320, 240);
        let mut enc = VideoEncoder::new(w, h, 30, bitrate_for(w, h, 30)).unwrap();
        let mut dec = VideoDecoder::new().unwrap();
        let mut pattern = Pattern::new(w, h, 7);
        let mut decoded = 0;
        let mut first_key = None;
        for i in 0..30 {
            let frame = pattern.next_frame();
            if let Some(pkt) = enc.encode(&frame).unwrap() {
                if first_key.is_none() {
                    first_key = Some(pkt.keyframe);
                }
                if let Some(out) = dec.decode(&pkt.data, (0, 0)).unwrap() {
                    assert_eq!((out.width, out.height), (w, h));
                    assert!(out.is_valid());
                    decoded += 1;
                }
            }
            if i == 15 {
                enc.force_keyframe();
            }
        }
        assert_eq!(first_key, Some(true), "stream must start with a keyframe");
        assert!(decoded >= 20, "decoded {decoded} frames");
    }

    #[test]
    fn decoder_scales_to_target() {
        let (w, h) = (640, 360);
        let mut enc = VideoEncoder::new(w, h, 30, bitrate_for(w, h, 30)).unwrap();
        let mut dec = VideoDecoder::new().unwrap();
        let mut pattern = Pattern::new(w, h, 1);
        let pkt = enc.encode(&pattern.next_frame()).unwrap().unwrap();
        let out = dec.decode(&pkt.data, (320, 320)).unwrap().unwrap();
        assert_eq!((out.width, out.height), (320, 180));
    }

    #[test]
    fn rejects_odd_sizes_and_mismatched_frames() {
        assert!(VideoEncoder::new(321, 240, 30, 1_000_000).is_err());
        let mut enc = VideoEncoder::new(320, 240, 30, 1_000_000).unwrap();
        let wrong = Pattern::new(160, 120, 0).next_frame();
        assert!(enc.encode(&wrong).is_err());
    }

    #[test]
    fn bitrate_can_change_while_encoding() {
        let (w, h) = (320, 240);
        let mut enc = VideoEncoder::new(w, h, 30, 2_000_000).unwrap();
        let mut pattern = Pattern::new(w, h, 5);
        let mut bytes = |enc: &mut VideoEncoder| -> usize {
            (0..30)
                .filter_map(|_| enc.encode(&pattern.next_frame()).unwrap())
                .map(|p| p.data.len())
                .sum()
        };
        let _warmup = bytes(&mut enc);
        let high = bytes(&mut enc);
        enc.set_bitrate(150_000);
        let _settle = bytes(&mut enc);
        let low = bytes(&mut enc);
        assert!(
            low < high,
            "lowering bitrate should shrink output: {high} -> {low}"
        );
    }

    #[test]
    fn bitrates_are_sane() {
        let b1080 = bitrate_for(1920, 1080, 30);
        assert!((3_000_000..8_000_000).contains(&b1080), "{b1080}");
        assert!(bitrate_for(3840, 2160, 60) > bitrate_for(1920, 1080, 60));
        assert!(bitrate_for(64, 64, 1) >= 800_000);
    }
}
