//! H.264 encode/decode with OpenH264 (software, every platform).
//!
//! The encoder is tuned for screen content in real time and splits big
//! pictures into slices so several cores share the work.

use std::time::Instant;

use anyhow::{Context, Result, bail};
use openh264::decoder::Decoder;
use openh264::formats::{BgraSliceU8, YUVBuffer, YUVSource};

use crate::frame::{VideoFrame, fit_within};

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

/// The largest picture OpenH264 encodes (H.264 level 5.2), either way up.
pub const MAX_ENCODE: (u32, u32) = (3840, 2160);

pub struct VideoEncoder {
    raw: RawEncoder,
    width: u32,
    height: u32,
    yuv: YUVBuffer,
    start: Instant,
}

impl VideoEncoder {
    /// `width` and `height` must be even and within [`MAX_ENCODE`].
    pub fn new(width: u32, height: u32, fps: u32, bitrate_bps: u32) -> Result<Self> {
        if width == 0 || height == 0 || width % 2 == 1 || height % 2 == 1 {
            bail!("encoder size must be even and non-zero, got {width}x{height}");
        }
        let (long, short) = (width.max(height), width.min(height));
        if long > MAX_ENCODE.0 || short > MAX_ENCODE.1 {
            bail!("{width}x{height} is larger than software encoding supports (3840x2160)");
        }
        // OpenH264 encodes slices in parallel (at most 4 threads). Measured
        // on a 24-thread desktop: 1440p 21 -> 9 ms/frame, 4K 45 -> 22 ms.
        let cores = std::thread::available_parallelism().map_or(1, |n| n.get());
        let wanted = if width as u64 * height as u64 <= 1280 * 720 {
            2
        } else {
            4
        };
        let threads = wanted.min(cores.saturating_sub(1).max(1)) as u16;
        // Testing aid: GLANCE_H264_THREADS=1 forces single-threaded encoding.
        let threads = std::env::var("GLANCE_H264_THREADS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(threads);
        let raw = RawEncoder::new(width, height, fps, bitrate_bps, threads)?;
        Ok(Self {
            raw,
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
        self.raw.set_option(
            openh264_sys2::ENCODER_OPTION_BITRATE,
            (&mut info as *mut openh264_sys2::SBitrateInfo).cast(),
        );
    }

    /// Ask for the next frame to be a keyframe (new viewer, packet loss).
    pub fn force_keyframe(&mut self) {
        self.raw.force_intra_frame();
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
        let Some((keyframe, data)) = self.raw.encode(&self.yuv, ts_us / 1000)? else {
            return Ok(None);
        };
        Ok(Some(EncodedPacket {
            keyframe,
            ts_us,
            width: self.width,
            height: self.height,
            data,
        }))
    }
}

/// A thin wrapper over the OpenH264 C API. The `openh264` crate always
/// configures one slice, and OpenH264 only uses threads across slices, so
/// big pictures were encoded on one core; here we set one slice per thread.
struct RawEncoder {
    ptr: *mut openh264_sys2::ISVCEncoder,
    info: openh264_sys2::SFrameBSInfo,
}

// SAFETY: the encoder is only used from one thread at a time (&mut self);
// OpenH264 encoders aren't tied to the thread that created them.
unsafe impl Send for RawEncoder {}

impl RawEncoder {
    fn new(width: u32, height: u32, fps: u32, bitrate: u32, threads: u16) -> Result<Self> {
        use openh264_sys2 as sys;
        let mut ptr: *mut sys::ISVCEncoder = std::ptr::null_mut();
        // SAFETY: creates an encoder object; checked for null below.
        let rc = unsafe { sys::source::APILoader::WelsCreateSVCEncoder(&mut ptr) };
        if rc != 0 || ptr.is_null() {
            bail!("create H.264 encoder failed ({rc})");
        }
        let enc = Self {
            ptr,
            info: sys::SFrameBSInfo::default(),
        };
        let fps = fps.max(1) as f32;
        let bitrate = bitrate.min(i32::MAX as u32) as i32;
        let mut p = sys::SEncParamExt::default();
        // SAFETY: the vtable entries come from a live encoder; the params
        // struct is plain data owned here.
        unsafe {
            let vt = &**enc.ptr;
            // Quiet unless asked (it warns about skipped frames constantly).
            // (The cast is a no-op on some targets, not on others.)
            #[allow(clippy::unnecessary_cast)]
            let mut level: i32 = if std::env::var_os("GLANCE_H264_LOG").is_some() {
                sys::WELS_LOG_DEBUG as i32
            } else {
                sys::WELS_LOG_QUIET as i32
            };
            (vt.SetOption.context("encoder vtable")?)(
                enc.ptr,
                sys::ENCODER_OPTION_TRACE_LEVEL,
                (&mut level as *mut i32).cast(),
            );
            (vt.GetDefaultParams.context("encoder vtable")?)(enc.ptr, &mut p);
            p.iUsageType = sys::SCREEN_CONTENT_REAL_TIME;
            p.iPicWidth = width as i32;
            p.iPicHeight = height as i32;
            p.iTargetBitrate = bitrate;
            p.iMaxBitrate = sys::UNSPECIFIED_BIT_RATE as i32;
            p.iRCMode = sys::RC_BITRATE_MODE;
            p.fMaxFrameRate = fps;
            p.bEnableFrameSkip = true;
            // Real-time first: the lowest complexity roughly halves encode
            // time at a small quality cost.
            p.iComplexityMode = sys::LOW_COMPLEXITY;
            p.bEnableAdaptiveQuant = false;
            p.bEnableBackgroundDetection = false;
            p.bEnableSceneChangeDetect = true;
            p.bEnableDenoise = false;
            p.eSpsPpsIdStrategy = sys::CONSTANT_ID;
            // A keyframe every ~4 s bounds how long a lost packet hurts;
            // new viewers ask for one explicitly.
            p.uiIntraPeriod = (fps as u32) * 4;
            p.iMultipleThreadIdc = threads;
            p.iSpatialLayerNum = 1;
            p.iTemporalLayerNum = 1;
            let layer = &mut p.sSpatialLayers[0];
            layer.iVideoWidth = width as i32;
            layer.iVideoHeight = height as i32;
            layer.fFrameRate = fps;
            layer.iSpatialBitrate = bitrate;
            layer.iMaxSpatialBitrate = sys::UNSPECIFIED_BIT_RATE as i32;
            if threads > 1 {
                layer.sSliceArgument.uiSliceMode = sys::SM_FIXEDSLCNUM_SLICE;
                layer.sSliceArgument.uiSliceNum = threads as u32;
            } else {
                layer.sSliceArgument.uiSliceMode = sys::SM_SINGLE_SLICE;
                layer.sSliceArgument.uiSliceNum = 1;
            }
            let rc = (vt.InitializeExt.context("encoder vtable")?)(enc.ptr, &p);
            if rc != 0 {
                bail!("H.264 encoder rejected {width}x{height} ({rc})");
            }
            let mut fmt = sys::videoFormatI420;
            (vt.SetOption.context("encoder vtable")?)(
                enc.ptr,
                sys::ENCODER_OPTION_DATAFORMAT,
                (&mut fmt as *mut i32).cast(),
            );
        }
        Ok(enc)
    }

    fn set_option(&mut self, id: openh264_sys2::ENCODER_OPTION, value: *mut std::ffi::c_void) {
        // SAFETY: live encoder; callers pass the struct type `id` expects.
        unsafe {
            if let Some(f) = (**self.ptr).SetOption {
                f(self.ptr, id, value);
            }
        }
    }

    fn force_intra_frame(&mut self) {
        // SAFETY: live, initialized encoder.
        unsafe {
            if let Some(f) = (**self.ptr).ForceIntraFrame {
                f(self.ptr, true);
            }
        }
    }

    /// Encode one I420 picture: `(keyframe, Annex-B bytes)`, or `None` if
    /// the rate controller skipped it.
    fn encode(&mut self, yuv: &YUVBuffer, ts_ms: u64) -> Result<Option<(bool, Vec<u8>)>> {
        use openh264_sys2 as sys;
        let (w, h) = yuv.dimensions_i32();
        let (sy, su, sv) = yuv.strides_i32();
        let pic = sys::SSourcePicture {
            iColorFormat: sys::videoFormatI420,
            iStride: [sy, su, sv, 0],
            // The encoder only reads the planes.
            pData: [
                yuv.y().as_ptr().cast_mut(),
                yuv.u().as_ptr().cast_mut(),
                yuv.v().as_ptr().cast_mut(),
                std::ptr::null_mut(),
            ],
            iPicWidth: w,
            iPicHeight: h,
            uiTimeStamp: ts_ms as i64,
            bPsnrY: false,
            bPsnrU: false,
            bPsnrV: false,
        };
        // SAFETY: live encoder; `pic` points into `yuv`, which outlives the
        // call; `info` is ours and filled by the encoder.
        let rc = unsafe {
            let f = (**self.ptr).EncodeFrame.context("encoder vtable")?;
            f(self.ptr, &pic, &mut self.info)
        };
        if rc != 0 {
            bail!("encode frame failed ({rc})");
        }
        let kind = self.info.eFrameType;
        if kind == sys::videoFrameTypeSkip || kind == sys::videoFrameTypeInvalid {
            return Ok(None);
        }
        let mut data = Vec::with_capacity(self.info.iFrameSizeInBytes.max(0) as usize);
        for layer in &self.info.sLayerInfo[..self.info.iLayerNum.clamp(0, 128) as usize] {
            let nals = layer.iNalCount.max(0) as usize;
            // SAFETY: the encoder reports `iNalCount` lengths and a buffer
            // holding all of them back to back, valid until the next call.
            let len: usize = unsafe { std::slice::from_raw_parts(layer.pNalLengthInByte, nals) }
                .iter()
                .map(|&n| n.max(0) as usize)
                .sum();
            if len > 0 {
                data.extend_from_slice(unsafe { std::slice::from_raw_parts(layer.pBsBuf, len) });
            }
        }
        if data.is_empty() {
            return Ok(None);
        }
        let keyframe = kind == sys::videoFrameTypeIDR || kind == sys::videoFrameTypeI;
        Ok(Some((keyframe, data)))
    }
}

impl Drop for RawEncoder {
    fn drop(&mut self) {
        // SAFETY: tear down the encoder we created, exactly once.
        unsafe {
            if let Some(f) = (**self.ptr).Uninitialize {
                f(self.ptr);
            }
            openh264_sys2::source::APILoader::WelsDestroySVCEncoder(self.ptr);
        }
    }
}

pub struct VideoDecoder {
    decoder: Decoder,
    planes: PlaneScaler,
}

impl VideoDecoder {
    pub fn new() -> Result<Self> {
        Ok(Self {
            decoder: Decoder::new().context("create H.264 decoder")?,
            planes: PlaneScaler::default(),
        })
    }

    /// Decode one packet and return a BGRA frame scaled to fit `target`
    /// (or full size if `target` is unknown). `None` while the decoder is
    /// still waiting for a keyframe.
    ///
    /// When the tile is smaller than the video, the Y/U/V planes are scaled
    /// first and only the tile-sized picture is converted to BGRA: a 4K
    /// stream in a 1080p tile costs about a third of converting it whole.
    pub fn decode(&mut self, data: &[u8], target: (u32, u32)) -> Result<Option<VideoFrame>> {
        let Some(yuv) = self.decoder.decode(data).context("decode packet")? else {
            return Ok(None);
        };
        let (w, h) = yuv.dimensions();
        let out = if target.0 == 0 || target.1 == 0 {
            (w as u32, h as u32)
        } else {
            fit_within((w as u32, h as u32), target)
        };
        // Even sizes, and a width the SIMD converter likes (multiple of 8).
        let out = ((out.0 & !7).max(8), (out.1 & !1).max(2));
        let mut rgba;
        let (ow, oh) = if out.0 as usize >= w || out.1 as usize >= h {
            rgba = vec![0u8; w * h * 4];
            yuv.write_rgba8(&mut rgba);
            (w as u32, h as u32)
        } else {
            let (y, u, v) = self.planes.scale(&yuv, out)?;
            let mut bgra = vec![0u8; out.0 as usize * out.1 as usize * 4];
            i420_to_bgra(y, u, v, (out.0 as usize, out.1 as usize), &mut bgra);
            return Ok(Some(VideoFrame {
                width: out.0,
                height: out.1,
                bgra,
            }));
        };
        for px in rgba.as_chunks_mut::<4>().0 {
            px.swap(0, 2);
        }
        Ok(Some(VideoFrame {
            width: ow,
            height: oh,
            bgra: rgba,
        }))
    }
}

/// Scales I420 planes (bilinear) into reusable buffers.
#[derive(Default)]
struct PlaneScaler {
    resizer: fast_image_resize::Resizer,
    y: Vec<u8>,
    u: Vec<u8>,
    v: Vec<u8>,
}

impl PlaneScaler {
    /// Scaled, tightly packed planes (`out` and half of it for chroma).
    fn scale(&mut self, yuv: &impl YUVSource, out: (u32, u32)) -> Result<(&[u8], &[u8], &[u8])> {
        use fast_image_resize::images::{Image, ImageRef};
        use fast_image_resize::{FilterType, PixelType, ResizeAlg, ResizeOptions};
        let (w, h) = yuv.dimensions();
        let (sy, su, sv) = yuv.strides();
        let (ow, oh) = (out.0 as usize, out.1 as usize);
        // Box filtering is cheap and looks right for the big ratios here.
        let alg = ResizeOptions::new().resize_alg(ResizeAlg::Convolution(FilterType::Box));
        let mut plane = |src: &[u8],
                         stride: usize,
                         (pw, ph): (usize, usize),
                         (dw, dh): (usize, usize),
                         dst: &mut Vec<u8>|
         -> Result<()> {
            // Rows are `stride` bytes apart; view the plane that wide and
            // crop to the visible part.
            let rows = src.len() / stride;
            let src = ImageRef::new(
                stride as u32,
                rows as u32,
                &src[..rows * stride],
                PixelType::U8,
            )
            .map_err(|e| anyhow::anyhow!("plane view: {e:?}"))?;
            dst.resize(dw * dh, 0);
            let mut img = Image::from_slice_u8(dw as u32, dh as u32, dst, PixelType::U8)
                .map_err(|e| anyhow::anyhow!("plane buffer: {e:?}"))?;
            self.resizer
                .resize(&src, &mut img, &alg.crop(0.0, 0.0, pw as f64, ph as f64))
                .map_err(|e| anyhow::anyhow!("scale plane: {e:?}"))?;
            Ok(())
        };
        plane(yuv.y(), sy, (w, h), (ow, oh), &mut self.y)?;
        plane(yuv.u(), su, (w / 2, h / 2), (ow / 2, oh / 2), &mut self.u)?;
        plane(yuv.v(), sv, (w / 2, h / 2), (ow / 2, oh / 2), &mut self.v)?;
        Ok((&self.y, &self.u, &self.v))
    }
}

/// Limited-range BT.601 I420 (tightly packed) to BGRA, the same maths as
/// OpenH264's converter in 8.8 fixed point.
fn i420_to_bgra(y: &[u8], u: &[u8], v: &[u8], (w, h): (usize, usize), out: &mut [u8]) {
    let cw = w / 2;
    for row in 0..h {
        let yrow = &y[row * w..][..w];
        let urow = &u[(row / 2) * cw..][..cw];
        let vrow = &v[(row / 2) * cw..][..cw];
        let orow = &mut out[row * w * 4..][..w * 4];
        // Two pixels share one chroma sample.
        let pairs = orow.as_chunks_mut::<8>().0.iter_mut();
        for (((pair, ys), &cu), &cv) in pairs.zip(yrow.as_chunks::<2>().0).zip(urow).zip(vrow) {
            let d = cu as i32 - 128;
            let e = cv as i32 - 128;
            let (rb, rg, rr) = (516 * d + 128, 128 - 100 * d - 208 * e, 409 * e + 128);
            for (k, &yv) in ys.iter().enumerate() {
                let c = (yv as i32 - 16) * 298;
                pair[k * 4] = ((c + rb) >> 8).clamp(0, 255) as u8;
                pair[k * 4 + 1] = ((c + rg) >> 8).clamp(0, 255) as u8;
                pair[k * 4 + 2] = ((c + rr) >> 8).clamp(0, 255) as u8;
                pair[k * 4 + 3] = 255;
            }
        }
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
    fn multi_slice_hd_stream_decodes() {
        // 1080p uses several slices (threads); the decoder must reassemble
        // whole pictures, and the picture must match what went in.
        let (w, h) = (1920, 1080);
        let mut enc = VideoEncoder::new(w, h, 30, bitrate_for(w, h, 30)).unwrap();
        let mut dec = VideoDecoder::new().unwrap();
        let mut pattern = Pattern::new(w, h, 5);
        let mut last = None;
        for _ in 0..6 {
            let frame = pattern.next_frame();
            if let Some(pkt) = enc.encode(&frame).unwrap()
                && let Some(out) = dec.decode(&pkt.data, (0, 0)).unwrap()
            {
                last = Some((frame, out));
            }
        }
        let (input, out) = last.expect("decoded a frame");
        assert_eq!((out.width, out.height), (w, h));
        // Compare a few pixels top, middle and bottom (each slice) loosely.
        for y in [10, h / 2, h - 10] {
            let i = ((y * w + w / 2) * 4) as usize;
            for c in 0..3 {
                let d = (input.bgra[i + c] as i32 - out.bgra[i + c] as i32).abs();
                assert!(d < 60, "pixel at row {y} channel {c} off by {d}");
            }
        }
    }

    #[test]
    fn scaled_decode_keeps_colours() {
        // A flat colour must come out the same whether the decoder converts
        // full size or scales the planes first.
        let (w, h) = (1280, 720);
        let mut enc = VideoEncoder::new(w, h, 30, bitrate_for(w, h, 30)).unwrap();
        let mut full = VideoDecoder::new().unwrap();
        let mut small = VideoDecoder::new().unwrap();
        let colour = [40u8, 120, 200, 255]; // BGRA
        let frame = VideoFrame {
            width: w,
            height: h,
            bgra: colour.repeat((w * h) as usize),
        };
        let (mut a, mut b) = (None, None);
        for _ in 0..3 {
            if let Some(pkt) = enc.encode(&frame).unwrap() {
                a = full.decode(&pkt.data, (0, 0)).unwrap().or(a);
                b = small.decode(&pkt.data, (640, 360)).unwrap().or(b);
            }
        }
        let (a, b) = (a.unwrap(), b.unwrap());
        assert_eq!((b.width, b.height), (640, 360));
        let mid = |f: &VideoFrame| {
            let i = (((f.height / 2) * f.width + f.width / 2) * 4) as usize;
            [f.bgra[i], f.bgra[i + 1], f.bgra[i + 2]]
        };
        for (c, (x, y)) in mid(&a).iter().zip(mid(&b)).enumerate() {
            assert!((*x as i32 - y as i32).abs() <= 3, "channel {c}: {x} vs {y}");
            assert!(
                (*x as i32 - colour[c] as i32).abs() <= 8,
                "channel {c}: {x} vs input"
            );
        }
    }

    #[test]
    fn huge_frames_are_refused_not_crashed() {
        assert!(VideoEncoder::new(5120, 1440, 30, 10_000_000).is_err());
        assert!(VideoEncoder::new(3840, 2160, 30, 10_000_000).is_ok());
        assert!(VideoEncoder::new(2160, 3840, 30, 10_000_000).is_ok());
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
