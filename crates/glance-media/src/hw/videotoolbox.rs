//! Apple VideoToolbox H.264 encoding (macOS).
//!
//! Frames go in as BGRA pixel buffers (the media engine converts them) and
//! come out as Constrained Baseline H.264. VideoToolbox hands back AVCC
//! (length-prefixed NAL units, parameter sets on the side); we turn that
//! into Annex-B with SPS/PPS in front of every keyframe, like the other
//! encoders, so viewers decode it with OpenH264.

use std::ffi::c_void;
use std::ptr::{null, null_mut};
use std::sync::{Mutex, OnceLock};

use anyhow::{Result, anyhow, bail};

type OSStatus = i32;
type CFTypeRef = *const c_void;
type CFStringRef = *const c_void;
type CFDictionaryRef = *const c_void;
type CFArrayRef = *const c_void;
type CFNumberRef = *const c_void;
type CFBooleanRef = *const c_void;
type CFAllocatorRef = *const c_void;
type VTCompressionSessionRef = *mut c_void;
type CVPixelBufferRef = *mut c_void;
type CVPixelBufferPoolRef = *mut c_void;
type CMSampleBufferRef = *mut c_void;
type CMBlockBufferRef = *mut c_void;
type CMFormatDescriptionRef = *mut c_void;

#[repr(C)]
#[derive(Clone, Copy)]
struct CMTime {
    value: i64,
    timescale: i32,
    flags: u32,
    epoch: i64,
}

const CM_TIME_VALID: u32 = 1;
const INVALID_TIME: CMTime = CMTime {
    value: 0,
    timescale: 0,
    flags: 0,
    epoch: 0,
};

#[repr(C)]
struct CFDictionaryKeyCallBacks {
    _private: [u8; 0],
}
#[repr(C)]
struct CFDictionaryValueCallBacks {
    _private: [u8; 0],
}

type OutputCallback = extern "C" fn(
    refcon: *mut c_void,
    frame_refcon: *mut c_void,
    status: OSStatus,
    flags: u32,
    sample: CMSampleBufferRef,
);

const CODEC_H264: u32 = u32::from_be_bytes(*b"avc1");
const PIXEL_BGRA: u32 = u32::from_be_bytes(*b"BGRA");
const CF_NUMBER_SINT32: isize = 3;
const LOCK_READ_WRITE: u64 = 0;

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    static kCFBooleanTrue: CFBooleanRef;
    static kCFBooleanFalse: CFBooleanRef;
    static kCFTypeDictionaryKeyCallBacks: CFDictionaryKeyCallBacks;
    static kCFTypeDictionaryValueCallBacks: CFDictionaryValueCallBacks;
    fn CFRelease(cf: CFTypeRef);
    fn CFNumberCreate(alloc: CFAllocatorRef, kind: isize, value: *const c_void) -> CFNumberRef;
    fn CFDictionaryCreate(
        alloc: CFAllocatorRef,
        keys: *const CFTypeRef,
        values: *const CFTypeRef,
        count: isize,
        key_cb: *const CFDictionaryKeyCallBacks,
        value_cb: *const CFDictionaryValueCallBacks,
    ) -> CFDictionaryRef;
    fn CFDictionaryGetValue(dict: CFDictionaryRef, key: CFTypeRef) -> CFTypeRef;
    fn CFArrayGetCount(array: CFArrayRef) -> isize;
    fn CFArrayGetValueAtIndex(array: CFArrayRef, index: isize) -> CFTypeRef;
}

#[link(name = "CoreVideo", kind = "framework")]
unsafe extern "C" {
    static kCVPixelBufferPixelFormatTypeKey: CFStringRef;
    static kCVPixelBufferWidthKey: CFStringRef;
    static kCVPixelBufferHeightKey: CFStringRef;
    fn CVPixelBufferPoolCreatePixelBuffer(
        alloc: CFAllocatorRef,
        pool: CVPixelBufferPoolRef,
        out: *mut CVPixelBufferRef,
    ) -> i32;
    fn CVPixelBufferLockBaseAddress(buffer: CVPixelBufferRef, flags: u64) -> i32;
    fn CVPixelBufferUnlockBaseAddress(buffer: CVPixelBufferRef, flags: u64) -> i32;
    fn CVPixelBufferGetBaseAddress(buffer: CVPixelBufferRef) -> *mut c_void;
    fn CVPixelBufferGetBytesPerRow(buffer: CVPixelBufferRef) -> usize;
    fn CVPixelBufferRelease(buffer: CVPixelBufferRef);
}

#[link(name = "CoreMedia", kind = "framework")]
unsafe extern "C" {
    static kCMSampleAttachmentKey_NotSync: CFStringRef;
    fn CMSampleBufferGetDataBuffer(sample: CMSampleBufferRef) -> CMBlockBufferRef;
    fn CMSampleBufferGetFormatDescription(sample: CMSampleBufferRef) -> CMFormatDescriptionRef;
    fn CMSampleBufferGetSampleAttachmentsArray(sample: CMSampleBufferRef, create: u8)
    -> CFArrayRef;
    fn CMBlockBufferGetDataLength(buffer: CMBlockBufferRef) -> usize;
    fn CMBlockBufferCopyDataBytes(
        buffer: CMBlockBufferRef,
        offset: usize,
        length: usize,
        dest: *mut c_void,
    ) -> OSStatus;
    fn CMVideoFormatDescriptionGetH264ParameterSetAtIndex(
        desc: CMFormatDescriptionRef,
        index: usize,
        ptr: *mut *const u8,
        size: *mut usize,
        count: *mut usize,
        nal_header_length: *mut i32,
    ) -> OSStatus;
}

#[link(name = "VideoToolbox", kind = "framework")]
unsafe extern "C" {
    static kVTVideoEncoderSpecification_EnableHardwareAcceleratedVideoEncoder: CFStringRef;
    static kVTVideoEncoderSpecification_RequireHardwareAcceleratedVideoEncoder: CFStringRef;
    static kVTVideoEncoderSpecification_EnableLowLatencyRateControl: CFStringRef;
    static kVTCompressionPropertyKey_RealTime: CFStringRef;
    static kVTCompressionPropertyKey_ProfileLevel: CFStringRef;
    static kVTCompressionPropertyKey_AllowFrameReordering: CFStringRef;
    static kVTCompressionPropertyKey_AverageBitRate: CFStringRef;
    static kVTCompressionPropertyKey_MaxKeyFrameInterval: CFStringRef;
    static kVTCompressionPropertyKey_ExpectedFrameRate: CFStringRef;
    static kVTCompressionPropertyKey_H264EntropyMode: CFStringRef;
    static kVTProfileLevel_H264_ConstrainedBaseline_AutoLevel: CFStringRef;
    static kVTProfileLevel_H264_Baseline_AutoLevel: CFStringRef;
    static kVTH264EntropyMode_CAVLC: CFStringRef;
    static kVTEncodeFrameOptionKey_ForceKeyFrame: CFStringRef;
    fn VTCompressionSessionCreate(
        alloc: CFAllocatorRef,
        width: i32,
        height: i32,
        codec: u32,
        encoder_spec: CFDictionaryRef,
        source_attrs: CFDictionaryRef,
        compressed_alloc: CFAllocatorRef,
        callback: Option<OutputCallback>,
        refcon: *mut c_void,
        out: *mut VTCompressionSessionRef,
    ) -> OSStatus;
    fn VTSessionSetProperty(session: *mut c_void, key: CFStringRef, value: CFTypeRef) -> OSStatus;
    fn VTCompressionSessionPrepareToEncodeFrames(session: VTCompressionSessionRef) -> OSStatus;
    fn VTCompressionSessionGetPixelBufferPool(
        session: VTCompressionSessionRef,
    ) -> CVPixelBufferPoolRef;
    fn VTCompressionSessionEncodeFrame(
        session: VTCompressionSessionRef,
        image: CVPixelBufferRef,
        pts: CMTime,
        duration: CMTime,
        frame_props: CFDictionaryRef,
        frame_refcon: *mut c_void,
        info_out: *mut u32,
    ) -> OSStatus;
    fn VTCompressionSessionCompleteFrames(
        session: VTCompressionSessionRef,
        until: CMTime,
    ) -> OSStatus;
    fn VTCompressionSessionInvalidate(session: VTCompressionSessionRef);
}

/// Encoded output collected by the callback (VideoToolbox's thread).
#[derive(Default)]
struct Sink {
    frames: Vec<(bool, Vec<u8>)>,
    error: Option<OSStatus>,
}

/// An owned CoreFoundation object, released on drop.
struct Cf(CFTypeRef);

impl Drop for Cf {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: we own one reference to it.
            unsafe { CFRelease(self.0) }
        }
    }
}

fn number(v: i32) -> Cf {
    // SAFETY: creates a CFNumber from a local i32.
    Cf(unsafe { CFNumberCreate(null(), CF_NUMBER_SINT32, (&v as *const i32).cast()) })
}

fn dict(pairs: &[(CFTypeRef, CFTypeRef)]) -> Cf {
    let keys: Vec<CFTypeRef> = pairs.iter().map(|p| p.0).collect();
    let values: Vec<CFTypeRef> = pairs.iter().map(|p| p.1).collect();
    // SAFETY: arrays of valid CF objects; the dictionary retains them.
    Cf(unsafe {
        CFDictionaryCreate(
            null(),
            keys.as_ptr(),
            values.as_ptr(),
            pairs.len() as isize,
            &kCFTypeDictionaryKeyCallBacks,
            &kCFTypeDictionaryValueCallBacks,
        )
    })
}

fn check(status: OSStatus, what: &str) -> Result<()> {
    if status == 0 {
        Ok(())
    } else {
        Err(anyhow!("{what} failed (OSStatus {status})"))
    }
}

pub struct VtEncoder {
    session: VTCompressionSessionRef,
    sink: Box<Mutex<Sink>>,
    width: u32,
    height: u32,
    hardware: bool,
}

// SAFETY: the session is used from one thread at a time (&mut self); the
// callback only touches the Mutex-protected sink.
unsafe impl Send for VtEncoder {}

impl VtEncoder {
    /// `require_hardware`: fail instead of using Apple's software encoder.
    pub fn new(
        width: u32,
        height: u32,
        fps: u32,
        bitrate: u32,
        require_hardware: bool,
    ) -> Result<Self> {
        let sink: Box<Mutex<Sink>> = Box::default();
        let mut session: VTCompressionSessionRef = null_mut();
        // SAFETY: CF objects are built and released here; the session gets
        // a pointer to `sink`, which lives (boxed) as long as the session.
        unsafe {
            let yes = kCFBooleanTrue;
            let spec = if require_hardware {
                dict(&[
                    (
                        kVTVideoEncoderSpecification_RequireHardwareAcceleratedVideoEncoder,
                        yes,
                    ),
                    (
                        kVTVideoEncoderSpecification_EnableLowLatencyRateControl,
                        yes,
                    ),
                ])
            } else {
                dict(&[(
                    kVTVideoEncoderSpecification_EnableHardwareAcceleratedVideoEncoder,
                    yes,
                )])
            };
            let (fmt, w, h) = (
                number(PIXEL_BGRA as i32),
                number(width as i32),
                number(height as i32),
            );
            let attrs = dict(&[
                (kCVPixelBufferPixelFormatTypeKey, fmt.0),
                (kCVPixelBufferWidthKey, w.0),
                (kCVPixelBufferHeightKey, h.0),
            ]);
            check(
                VTCompressionSessionCreate(
                    null(),
                    width as i32,
                    height as i32,
                    CODEC_H264,
                    spec.0,
                    attrs.0,
                    null(),
                    Some(on_output),
                    (&*sink as *const Mutex<Sink>).cast_mut().cast(),
                    &mut session,
                ),
                "create VideoToolbox session",
            )?;
        }
        let enc = Self {
            session,
            sink,
            width,
            height,
            hardware: require_hardware,
        };
        // SAFETY: a live session; values are CF objects alive for the call.
        unsafe {
            let profile = if require_hardware {
                // Low-latency rate control only takes constrained profiles.
                kVTProfileLevel_H264_ConstrainedBaseline_AutoLevel
            } else {
                kVTProfileLevel_H264_Baseline_AutoLevel
            };
            enc.set(kVTCompressionPropertyKey_RealTime, kCFBooleanTrue)?;
            enc.set(kVTCompressionPropertyKey_ProfileLevel, profile)?;
            enc.set(
                kVTCompressionPropertyKey_AllowFrameReordering,
                kCFBooleanFalse,
            )?;
            enc.set(
                kVTCompressionPropertyKey_H264EntropyMode,
                kVTH264EntropyMode_CAVLC,
            )
            .ok();
            let fps = fps.max(1) as i32;
            enc.set(kVTCompressionPropertyKey_ExpectedFrameRate, number(fps).0)?;
            // A keyframe every ~4 s; new viewers ask for one explicitly.
            enc.set(
                kVTCompressionPropertyKey_MaxKeyFrameInterval,
                number(fps * 4).0,
            )?;
            enc.set(
                kVTCompressionPropertyKey_AverageBitRate,
                number(bitrate.min(i32::MAX as u32) as i32).0,
            )?;
            check(
                VTCompressionSessionPrepareToEncodeFrames(enc.session),
                "prepare VideoToolbox",
            )?;
        }
        Ok(enc)
    }

    fn set(&self, key: CFStringRef, value: CFTypeRef) -> Result<()> {
        // SAFETY: a live session and CF objects alive for the call.
        check(
            unsafe { VTSessionSetProperty(self.session, key, value) },
            "set VideoToolbox property",
        )
    }

    pub fn is_hardware(&self) -> bool {
        self.hardware
    }

    pub fn set_bitrate(&mut self, bitrate: u32) -> Result<()> {
        // SAFETY: reading a framework constant.
        let key = unsafe { kVTCompressionPropertyKey_AverageBitRate };
        self.set(key, number(bitrate.min(i32::MAX as u32) as i32).0)
    }

    /// Encode one BGRA frame of the encoder's size. `None` if the encoder
    /// dropped it.
    pub fn encode(
        &mut self,
        bgra: &[u8],
        ts_us: u64,
        keyframe: bool,
    ) -> Result<Option<(bool, Vec<u8>)>> {
        let row = self.width as usize * 4;
        if bgra.len() != row * self.height as usize {
            bail!("frame buffer has the wrong size");
        }
        // SAFETY: the pixel buffer comes from the session's pool and is
        // locked while written within its reported row stride; encoding is
        // completed synchronously before the buffer is released.
        unsafe {
            let pool = VTCompressionSessionGetPixelBufferPool(self.session);
            if pool.is_null() {
                bail!("VideoToolbox has no pixel buffer pool");
            }
            let mut buf: CVPixelBufferRef = null_mut();
            let rc = CVPixelBufferPoolCreatePixelBuffer(null(), pool, &mut buf);
            if rc != 0 || buf.is_null() {
                bail!("create pixel buffer failed ({rc})");
            }
            CVPixelBufferLockBaseAddress(buf, LOCK_READ_WRITE);
            let base = CVPixelBufferGetBaseAddress(buf).cast::<u8>();
            let stride = CVPixelBufferGetBytesPerRow(buf);
            if base.is_null() || stride < row {
                CVPixelBufferUnlockBaseAddress(buf, LOCK_READ_WRITE);
                CVPixelBufferRelease(buf);
                bail!("pixel buffer is too small");
            }
            for (y, src) in bgra.chunks_exact(row).enumerate() {
                std::ptr::copy_nonoverlapping(src.as_ptr(), base.add(y * stride), row);
            }
            CVPixelBufferUnlockBaseAddress(buf, LOCK_READ_WRITE);

            let props =
                keyframe.then(|| dict(&[(kVTEncodeFrameOptionKey_ForceKeyFrame, kCFBooleanTrue)]));
            let pts = CMTime {
                value: ts_us as i64,
                timescale: 1_000_000,
                flags: CM_TIME_VALID,
                epoch: 0,
            };
            let rc = VTCompressionSessionEncodeFrame(
                self.session,
                buf,
                pts,
                INVALID_TIME,
                props.as_ref().map_or(null(), |p| p.0),
                null_mut(),
                null_mut(),
            );
            let done = VTCompressionSessionCompleteFrames(self.session, INVALID_TIME);
            CVPixelBufferRelease(buf);
            check(rc, "VideoToolbox encode")?;
            check(done, "VideoToolbox complete")?;
        }
        let mut sink = self.sink.lock().map_err(|_| anyhow!("encoder poisoned"))?;
        if let Some(status) = sink.error.take() {
            bail!("VideoToolbox encode failed (OSStatus {status})");
        }
        // One frame in, at most one out (no reordering); keep the newest.
        Ok(sink.frames.drain(..).last())
    }
}

impl Drop for VtEncoder {
    fn drop(&mut self) {
        // SAFETY: invalidate (which waits for callbacks) then release, once.
        unsafe {
            VTCompressionSessionInvalidate(self.session);
            CFRelease(self.session.cast_const());
        }
    }
}

extern "C" fn on_output(
    refcon: *mut c_void,
    _frame: *mut c_void,
    status: OSStatus,
    _flags: u32,
    sample: CMSampleBufferRef,
) {
    // SAFETY: refcon is the session's boxed sink, alive until the session
    // is invalidated; the sample buffer is valid for this call.
    let sink = unsafe { &*(refcon as *const Mutex<Sink>) };
    let Ok(mut sink) = sink.lock() else { return };
    if status != 0 {
        sink.error = Some(status);
        return;
    }
    if sample.is_null() {
        return; // dropped frame
    }
    match unsafe { annex_b(sample) } {
        Some(frame) => sink.frames.push(frame),
        None => sink.error = Some(-1),
    }
}

/// Convert one AVCC sample to Annex-B, with SPS/PPS before keyframes.
unsafe fn annex_b(sample: CMSampleBufferRef) -> Option<(bool, Vec<u8>)> {
    const START: [u8; 4] = [0, 0, 0, 1];
    // SAFETY (whole fn): CoreMedia getters on a live sample buffer; copies
    // stay within the lengths CoreMedia reports.
    unsafe {
        let mut keyframe = true;
        let attachments = CMSampleBufferGetSampleAttachmentsArray(sample, 0);
        if !attachments.is_null() && CFArrayGetCount(attachments) > 0 {
            let first = CFArrayGetValueAtIndex(attachments, 0);
            let not_sync = CFDictionaryGetValue(first, kCMSampleAttachmentKey_NotSync);
            keyframe = not_sync.is_null() || not_sync == kCFBooleanFalse;
        }
        let mut out = Vec::new();
        let mut header_len = 4i32;
        if keyframe {
            let desc = CMSampleBufferGetFormatDescription(sample);
            let mut count = 0usize;
            let (mut p, mut n) = (null(), 0usize);
            if CMVideoFormatDescriptionGetH264ParameterSetAtIndex(
                desc,
                0,
                &mut p,
                &mut n,
                &mut count,
                &mut header_len,
            ) != 0
            {
                return None;
            }
            for i in 0..count {
                if CMVideoFormatDescriptionGetH264ParameterSetAtIndex(
                    desc,
                    i,
                    &mut p,
                    &mut n,
                    null_mut(),
                    null_mut(),
                ) != 0
                {
                    return None;
                }
                out.extend_from_slice(&START);
                out.extend_from_slice(std::slice::from_raw_parts(p, n));
            }
        }
        let block = CMSampleBufferGetDataBuffer(sample);
        if block.is_null() {
            return None;
        }
        let len = CMBlockBufferGetDataLength(block);
        let mut avcc = vec![0u8; len];
        if CMBlockBufferCopyDataBytes(block, 0, len, avcc.as_mut_ptr().cast()) != 0 {
            return None;
        }
        let hl = header_len.clamp(1, 4) as usize;
        let mut at = 0;
        while at + hl <= avcc.len() {
            let n = avcc[at..at + hl]
                .iter()
                .fold(0usize, |acc, &b| (acc << 8) | b as usize);
            at += hl;
            let end = at.checked_add(n).filter(|&e| e <= avcc.len())?;
            out.extend_from_slice(&START);
            out.extend_from_slice(&avcc[at..end]);
            at = end;
        }
        Some((keyframe, out))
    }
}

/// Whether a hardware H.264 encoder session opens (cached).
pub fn available() -> bool {
    static OK: OnceLock<bool> = OnceLock::new();
    *OK.get_or_init(|| match VtEncoder::new(640, 360, 30, 1_000_000, true) {
        Ok(_) => true,
        Err(e) => {
            tracing::debug!("VideoToolbox hardware encoder unavailable: {e:#}");
            false
        }
    })
}
