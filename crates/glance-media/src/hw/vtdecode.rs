//! Apple VideoToolbox H.264 decoding (macOS).
//!
//! Annex-B packets are turned into AVCC sample buffers for a
//! VTDecompressionSession, which decodes on the media engine and scales the
//! picture to the tile it is shown in. Frames come back as NV12.

use std::ffi::c_void;
use std::ptr::{null, null_mut};

use anyhow::{Result, anyhow, bail};

use super::Nv12Frame;
use super::annexb::{nal_units, sps_size};

type OSStatus = i32;
type CFTypeRef = *const c_void;
type CFStringRef = *const c_void;
type CFDictionaryRef = *const c_void;
type CFNumberRef = *const c_void;
type CFBooleanRef = *const c_void;
type CFAllocatorRef = *const c_void;
type VTDecompressionSessionRef = *mut c_void;
type CVImageBufferRef = *mut c_void;
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
    image: CVImageBufferRef,
    pts: CMTime,
    duration: CMTime,
);

#[repr(C)]
struct OutputCallbackRecord {
    callback: OutputCallback,
    refcon: *mut c_void,
}

/// kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange (NV12, limited range).
const PIXEL_NV12: u32 = u32::from_be_bytes(*b"420v");
const CF_NUMBER_SINT32: isize = 3;
const LOCK_READ_ONLY: u64 = 1;
const ASSURE_MEMORY_NOW: u32 = 1;

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    static kCFBooleanTrue: CFBooleanRef;
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
}

#[link(name = "CoreVideo", kind = "framework")]
unsafe extern "C" {
    static kCVPixelBufferPixelFormatTypeKey: CFStringRef;
    static kCVPixelBufferWidthKey: CFStringRef;
    static kCVPixelBufferHeightKey: CFStringRef;
    fn CVPixelBufferLockBaseAddress(buffer: CVImageBufferRef, flags: u64) -> i32;
    fn CVPixelBufferUnlockBaseAddress(buffer: CVImageBufferRef, flags: u64) -> i32;
    fn CVPixelBufferGetWidth(buffer: CVImageBufferRef) -> usize;
    fn CVPixelBufferGetHeight(buffer: CVImageBufferRef) -> usize;
    fn CVPixelBufferGetPixelFormatType(buffer: CVImageBufferRef) -> u32;
    fn CVPixelBufferGetBaseAddressOfPlane(buffer: CVImageBufferRef, plane: usize) -> *mut c_void;
    fn CVPixelBufferGetBytesPerRowOfPlane(buffer: CVImageBufferRef, plane: usize) -> usize;
}

#[link(name = "CoreMedia", kind = "framework")]
unsafe extern "C" {
    fn CMVideoFormatDescriptionCreateFromH264ParameterSets(
        alloc: CFAllocatorRef,
        count: usize,
        pointers: *const *const u8,
        sizes: *const usize,
        nal_header_length: i32,
        out: *mut CMFormatDescriptionRef,
    ) -> OSStatus;
    fn CMBlockBufferCreateWithMemoryBlock(
        structure_alloc: CFAllocatorRef,
        memory: *mut c_void,
        block_length: usize,
        block_alloc: CFAllocatorRef,
        custom_source: *const c_void,
        offset: usize,
        data_length: usize,
        flags: u32,
        out: *mut CMBlockBufferRef,
    ) -> OSStatus;
    fn CMBlockBufferReplaceDataBytes(
        source: *const c_void,
        dest: CMBlockBufferRef,
        offset: usize,
        length: usize,
    ) -> OSStatus;
    fn CMSampleBufferCreateReady(
        alloc: CFAllocatorRef,
        data: CMBlockBufferRef,
        format: CMFormatDescriptionRef,
        samples: isize,
        timing_entries: isize,
        timing: *const c_void,
        size_entries: isize,
        sizes: *const usize,
        out: *mut CMSampleBufferRef,
    ) -> OSStatus;
}

#[link(name = "VideoToolbox", kind = "framework")]
unsafe extern "C" {
    static kVTVideoDecoderSpecification_EnableHardwareAcceleratedVideoDecoder: CFStringRef;
    fn VTDecompressionSessionCreate(
        alloc: CFAllocatorRef,
        format: CMFormatDescriptionRef,
        decoder_spec: CFDictionaryRef,
        image_attrs: CFDictionaryRef,
        callback: *const OutputCallbackRecord,
        out: *mut VTDecompressionSessionRef,
    ) -> OSStatus;
    fn VTDecompressionSessionDecodeFrame(
        session: VTDecompressionSessionRef,
        sample: CMSampleBufferRef,
        flags: u32,
        frame_refcon: *mut c_void,
        info_out: *mut u32,
    ) -> OSStatus;
    fn VTDecompressionSessionWaitForAsynchronousFrames(
        session: VTDecompressionSessionRef,
    ) -> OSStatus;
    fn VTDecompressionSessionInvalidate(session: VTDecompressionSessionRef);
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

/// What the output callback fills in.
#[derive(Default)]
struct Sink {
    frame: Option<Nv12Frame>,
    error: Option<String>,
}

pub struct VtDecoder {
    session: VTDecompressionSessionRef,
    format: Option<Cf>,
    /// SPS and PPS (without start codes) the session was made for.
    sps: Vec<u8>,
    pps: Vec<u8>,
    /// Visible size of the stream, from the SPS.
    size: (u32, u32),
    /// The tile and the size the session scales to.
    target: (u32, u32),
    out: (u32, u32),
    sink: Box<Sink>,
    failed: Option<String>,
}

// SAFETY: used from one thread at a time (&mut self); decoding is
// synchronous, so the callback has run by the time decode() returns.
unsafe impl Send for VtDecoder {}

impl VtDecoder {
    pub fn new() -> Result<Self> {
        Ok(Self {
            session: null_mut(),
            format: None,
            sps: Vec::new(),
            pps: Vec::new(),
            size: (0, 0),
            target: (0, 0),
            out: (0, 0),
            sink: Box::default(),
            failed: None,
        })
    }

    /// Make the next decode fail, as a broken decoder would (tests).
    #[cfg(test)]
    pub(crate) fn fail_next(&mut self) {
        self.failed = Some("injected failure".into());
    }

    /// Decode one packet (a whole picture, Annex-B). The picture comes back
    /// scaled to fit `target` ((0, 0): full size); a new `target` takes
    /// effect at the next keyframe.
    pub fn decode(&mut self, data: &[u8], target: (u32, u32)) -> Result<Option<Nv12Frame>> {
        if let Some(e) = self.failed.take() {
            bail!("{e}");
        }
        self.target = target;
        let mut sps = None;
        let mut pps = None;
        let mut idr = false;
        let mut avcc = Vec::with_capacity(data.len() + 16);
        for nal in nal_units(data) {
            match nal[0] & 0x1f {
                7 => sps = Some(nal),
                8 => pps = Some(nal),
                5 => {
                    idr = true;
                    avcc.extend_from_slice(&(nal.len() as u32).to_be_bytes());
                    avcc.extend_from_slice(nal);
                }
                1..=4 | 6 => {
                    avcc.extend_from_slice(&(nal.len() as u32).to_be_bytes());
                    avcc.extend_from_slice(nal);
                }
                _ => {}
            }
        }
        if let (Some(sps), Some(pps)) = (sps, pps) {
            let new_stream = sps != self.sps.as_slice() || pps != self.pps.as_slice();
            let new_size = idr && self.out_size(sps) != self.out;
            if new_stream || new_size || self.session.is_null() {
                self.open(sps, pps)?;
            }
        }
        if self.session.is_null() || avcc.is_empty() {
            // Waiting for the first keyframe's parameter sets.
            return Ok(None);
        }
        let format = self
            .format
            .as_ref()
            .map_or(null_mut(), |f| f.0 as *mut c_void);
        // SAFETY: the block and sample buffers are created, used and
        // released here; the session decodes synchronously.
        unsafe {
            let mut block = null_mut();
            check(
                CMBlockBufferCreateWithMemoryBlock(
                    null(),
                    null_mut(),
                    avcc.len(),
                    null(),
                    null(),
                    0,
                    avcc.len(),
                    ASSURE_MEMORY_NOW,
                    &mut block,
                ),
                "CMBlockBufferCreateWithMemoryBlock",
            )?;
            let block = Cf(block);
            check(
                CMBlockBufferReplaceDataBytes(
                    avcc.as_ptr().cast(),
                    block.0 as *mut c_void,
                    0,
                    avcc.len(),
                ),
                "CMBlockBufferReplaceDataBytes",
            )?;
            let mut sample = null_mut();
            let size = avcc.len();
            check(
                CMSampleBufferCreateReady(
                    null(),
                    block.0 as *mut c_void,
                    format,
                    1,
                    0,
                    null(),
                    1,
                    &size,
                    &mut sample,
                ),
                "CMSampleBufferCreateReady",
            )?;
            let sample = Cf(sample);
            let mut info = 0u32;
            check(
                VTDecompressionSessionDecodeFrame(
                    self.session,
                    sample.0 as *mut c_void,
                    0,
                    null_mut(),
                    &mut info,
                ),
                "VTDecompressionSessionDecodeFrame",
            )?;
            VTDecompressionSessionWaitForAsynchronousFrames(self.session);
        }
        if let Some(e) = self.sink.error.take() {
            bail!("{e}");
        }
        Ok(self.sink.frame.take())
    }

    /// The size frames come out at for this SPS and the current target.
    fn out_size(&self, sps: &[u8]) -> (u32, u32) {
        let size = sps_size(sps).unwrap_or(self.size);
        crate::codec::tile_size(size, self.target)
    }

    /// (Re)create the session for these parameter sets and output size.
    fn open(&mut self, sps: &[u8], pps: &[u8]) -> Result<()> {
        self.close();
        let size = sps_size(sps).ok_or_else(|| anyhow!("unreadable SPS"))?;
        let out = crate::codec::tile_size(size, self.target);
        let mut format = null_mut();
        let pointers = [sps.as_ptr(), pps.as_ptr()];
        let sizes = [sps.len(), pps.len()];
        // SAFETY: the parameter sets outlive the call; CoreMedia copies them.
        unsafe {
            check(
                CMVideoFormatDescriptionCreateFromH264ParameterSets(
                    null(),
                    2,
                    pointers.as_ptr(),
                    sizes.as_ptr(),
                    4,
                    &mut format,
                ),
                "CMVideoFormatDescriptionCreateFromH264ParameterSets",
            )?;
        }
        let format = Cf(format as CFTypeRef);
        // SAFETY: the keys are framework constants.
        let spec = unsafe {
            dict(&[(
                kVTVideoDecoderSpecification_EnableHardwareAcceleratedVideoDecoder,
                kCFBooleanTrue,
            )])
        };
        let (pixel, w, h) = (
            number(PIXEL_NV12 as i32),
            number(out.0 as i32),
            number(out.1 as i32),
        );
        // SAFETY: as above.
        let attrs = unsafe {
            dict(&[
                (kCVPixelBufferPixelFormatTypeKey, pixel.0),
                (kCVPixelBufferWidthKey, w.0),
                (kCVPixelBufferHeightKey, h.0),
            ])
        };
        let record = OutputCallbackRecord {
            callback: on_output,
            refcon: (&mut *self.sink as *mut Sink).cast(),
        };
        let mut session = null_mut();
        // SAFETY: the sink outlives the session (closed before it drops).
        unsafe {
            check(
                VTDecompressionSessionCreate(
                    null(),
                    format.0 as *mut c_void,
                    spec.0,
                    attrs.0,
                    &record,
                    &mut session,
                ),
                "VTDecompressionSessionCreate",
            )?;
        }
        self.session = session;
        self.format = Some(format);
        self.sps = sps.to_vec();
        self.pps = pps.to_vec();
        self.size = size;
        self.out = out;
        Ok(())
    }

    fn close(&mut self) {
        if !self.session.is_null() {
            // SAFETY: our session; invalidated before the last release.
            unsafe {
                VTDecompressionSessionInvalidate(self.session);
                CFRelease(self.session as CFTypeRef);
            }
            self.session = null_mut();
        }
        self.format = None;
    }
}

impl Drop for VtDecoder {
    fn drop(&mut self) {
        self.close();
    }
}

extern "C" fn on_output(
    refcon: *mut c_void,
    _frame_refcon: *mut c_void,
    status: OSStatus,
    _flags: u32,
    image: CVImageBufferRef,
    _pts: CMTime,
    _duration: CMTime,
) {
    // SAFETY: refcon is the decoder's Sink, alive while the session is.
    let sink = unsafe { &mut *(refcon as *mut Sink) };
    if status != 0 {
        sink.error = Some(format!("VideoToolbox decode failed (OSStatus {status})"));
        return;
    }
    if image.is_null() {
        // A dropped frame.
        return;
    }
    // SAFETY: a valid pixel buffer for the duration of the callback.
    match unsafe { copy_nv12(image) } {
        Ok(frame) => sink.frame = Some(frame),
        Err(e) => sink.error = Some(format!("{e:#}")),
    }
}

/// Copy a locked NV12 pixel buffer into tightly packed planes.
unsafe fn copy_nv12(image: CVImageBufferRef) -> Result<Nv12Frame> {
    unsafe {
        if CVPixelBufferGetPixelFormatType(image) != PIXEL_NV12 {
            bail!("VideoToolbox returned a pixel format other than NV12");
        }
        let (w, h) = (
            CVPixelBufferGetWidth(image) & !1,
            CVPixelBufferGetHeight(image) & !1,
        );
        check(
            CVPixelBufferLockBaseAddress(image, LOCK_READ_ONLY),
            "CVPixelBufferLockBaseAddress",
        )?;
        let plane = |i: usize, rows: usize| -> Vec<u8> {
            let base = CVPixelBufferGetBaseAddressOfPlane(image, i) as *const u8;
            let stride = CVPixelBufferGetBytesPerRowOfPlane(image, i);
            let mut out = Vec::with_capacity(w * rows);
            for r in 0..rows {
                out.extend_from_slice(std::slice::from_raw_parts(base.add(r * stride), w));
            }
            out
        };
        let y = plane(0, h);
        let uv = plane(1, h / 2);
        CVPixelBufferUnlockBaseAddress(image, LOCK_READ_ONLY);
        Ok(Nv12Frame {
            width: w as u32,
            height: h as u32,
            y,
            uv,
        })
    }
}
