//! H.264 through Media Foundation (Windows): the GPU vendor's encoder MFT
//! (AMD AMF, Intel Quick Sync, ...), fed NV12 from system memory.
//!
//! mfplat.dll is loaded at run time: Windows N editions and Windows Server
//! without the Media Foundation feature don't have it, and glance must still
//! start there (with OpenH264).

use std::collections::VecDeque;
use std::ffi::c_void;
use std::mem::ManuallyDrop;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use libloading::Library;
use windows::Win32::Foundation::VARIANT_TRUE;
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx, CoTaskMemFree};
use windows::Win32::System::Variant::{
    VARENUM, VARIANT, VARIANT_0, VARIANT_0_0, VARIANT_0_0_0, VT_BOOL, VT_UI4,
};
use windows::core::{GUID, HRESULT, Interface, PWSTR};

type StartupFn = unsafe extern "system" fn(u32, u32) -> HRESULT;
type EnumFn = unsafe extern "system" fn(
    GUID,
    u32,
    *const MFT_REGISTER_TYPE_INFO,
    *const MFT_REGISTER_TYPE_INFO,
    *mut *mut *mut c_void,
    *mut u32,
) -> HRESULT;
type CreateFn = unsafe extern "system" fn(*mut *mut c_void) -> HRESULT;
type CreateBufferFn = unsafe extern "system" fn(u32, *mut *mut c_void) -> HRESULT;

struct Mf {
    _lib: Library,
    enum_ex: EnumFn,
    create_media_type: CreateFn,
    create_sample: CreateFn,
    create_memory_buffer: CreateBufferFn,
}

/// mfplat.dll, loaded and started once.
fn mf() -> Result<&'static Mf> {
    static MF: OnceLock<Result<Mf, String>> = OnceLock::new();
    MF.get_or_init(|| unsafe {
        let load = || -> Result<Mf> {
            let lib = Library::new("mfplat.dll").context("mfplat.dll (Media Foundation)")?;
            let startup: StartupFn = *lib.get(b"MFStartup\0")?;
            let mf = Mf {
                enum_ex: *lib.get(b"MFTEnumEx\0")?,
                create_media_type: *lib.get(b"MFCreateMediaType\0")?,
                create_sample: *lib.get(b"MFCreateSample\0")?,
                create_memory_buffer: *lib.get(b"MFCreateMemoryBuffer\0")?,
                _lib: lib,
            };
            startup(MF_VERSION, MFSTARTUP_NOSOCKET)
                .ok()
                .context("MFStartup")?;
            Ok(mf)
        };
        load().map_err(|e| format!("{e:#}"))
    })
    .as_ref()
    .map_err(|e| anyhow!("{e}"))
}

impl Mf {
    fn media_type(&self) -> Result<IMFMediaType> {
        let mut p = std::ptr::null_mut();
        unsafe {
            (self.create_media_type)(&mut p).ok()?;
            Ok(IMFMediaType::from_raw(p))
        }
    }

    fn sample(&self) -> Result<IMFSample> {
        let mut p = std::ptr::null_mut();
        unsafe {
            (self.create_sample)(&mut p).ok()?;
            Ok(IMFSample::from_raw(p))
        }
    }

    fn buffer(&self, len: u32) -> Result<IMFMediaBuffer> {
        let mut p = std::ptr::null_mut();
        unsafe {
            (self.create_memory_buffer)(len, &mut p).ok()?;
            Ok(IMFMediaBuffer::from_raw(p))
        }
    }

    /// H.264 encoders that take NV12: hardware ones, or (for tests) the
    /// synchronous software ones, best first.
    fn encoders(&self, hardware: bool) -> Result<Vec<IMFActivate>> {
        let input = MFT_REGISTER_TYPE_INFO {
            guidMajorType: MFMediaType_Video,
            guidSubtype: MFVideoFormat_NV12,
        };
        let output = MFT_REGISTER_TYPE_INFO {
            guidMajorType: MFMediaType_Video,
            guidSubtype: MFVideoFormat_H264,
        };
        let kind = if hardware {
            MFT_ENUM_FLAG_HARDWARE
        } else {
            MFT_ENUM_FLAG_SYNCMFT
        };
        let flags = (kind.0 | MFT_ENUM_FLAG_SORTANDFILTER.0) as u32;
        let (mut list, mut count) = (std::ptr::null_mut(), 0u32);
        unsafe {
            (self.enum_ex)(
                MFT_CATEGORY_VIDEO_ENCODER,
                flags,
                &input,
                &output,
                &mut list,
                &mut count,
            )
            .ok()
            .context("MFTEnumEx")?;
            let mut out = Vec::new();
            for i in 0..count as usize {
                let p = *list.add(i);
                if !p.is_null() {
                    out.push(IMFActivate::from_raw(p));
                }
            }
            if !list.is_null() {
                CoTaskMemFree(Some(list as *const c_void));
            }
            Ok(out)
        }
    }
}

/// An H.264 encoder MFT, set up for low-latency CBR Baseline.
pub struct MfEncoder {
    activate: IMFActivate,
    transform: IMFTransform,
    codec: Option<ICodecAPI>,
    /// Set for asynchronous (hardware) MFTs, which say when they want
    /// input and have output through events.
    events: Option<IMFMediaEventGenerator>,
    need_input: u32,
    output_info: MFT_OUTPUT_STREAM_INFO,
    out: VecDeque<(bool, Vec<u8>)>,
    /// SPS and PPS, put in front of keyframes that come without them.
    parameter_sets: Vec<u8>,
    nv12: Vec<u8>,
    width: u32,
    height: u32,
    fps: u32,
    name: &'static str,
    hardware: bool,
}

// Encoder MFTs live in the multithreaded apartment.
unsafe impl Send for MfEncoder {}

impl MfEncoder {
    /// The first encoder MFT that opens: a hardware one, or with
    /// `hardware == false` a software one (Microsoft's, which every
    /// Windows with Media Foundation has; used by the tests).
    pub fn new(width: u32, height: u32, fps: u32, bitrate: u32, hardware: bool) -> Result<Self> {
        let mf = mf()?;
        unsafe {
            // Already initialised (either way) is fine.
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        }
        let encoders = mf.encoders(hardware)?;
        if encoders.is_empty() {
            bail!(
                "no {} H.264 Media Foundation encoder",
                if hardware { "hardware" } else { "software" }
            );
        }
        let mut last = None;
        for activate in encoders {
            let name = friendly_name(&activate);
            match Self::open(mf, activate, width, height, fps, bitrate, hardware, name) {
                Ok(e) => {
                    tracing::info!("Media Foundation encoder: {name}");
                    return Ok(e);
                }
                Err(e) => {
                    tracing::debug!("Media Foundation encoder {name} failed: {e:#}");
                    last = Some(e.context(name));
                }
            }
        }
        Err(last.expect("at least one encoder was tried"))
    }

    #[allow(clippy::too_many_arguments)]
    fn open(
        mf: &Mf,
        activate: IMFActivate,
        width: u32,
        height: u32,
        fps: u32,
        bitrate: u32,
        hardware: bool,
        name: &'static str,
    ) -> Result<Self> {
        let fps = fps.max(1);
        let transform: IMFTransform = unsafe { activate.ActivateObject() }.context("activate")?;
        let attrs = unsafe { transform.GetAttributes() }.ok();
        let is_async = attrs
            .as_ref()
            .is_some_and(|a| unsafe { a.GetUINT32(&MF_TRANSFORM_ASYNC) }.unwrap_or(0) != 0);
        if let Some(a) = &attrs {
            unsafe {
                if is_async {
                    a.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1)
                        .context("unlock async MFT")?;
                }
                let _ = a.SetUINT32(&MF_LOW_LATENCY, 1);
            }
        }
        let codec: Option<ICodecAPI> = transform.cast().ok();
        if let Some(c) = &codec {
            // Rate control and B-frames have to be set before the types.
            set_codec(c, &CODECAPI_AVLowLatencyMode, bool_variant());
            set_codec(
                c,
                &CODECAPI_AVEncCommonRateControlMode,
                u32_variant(eAVEncCommonRateControlMode_CBR.0 as u32),
            );
            set_codec(c, &CODECAPI_AVEncCommonMeanBitRate, u32_variant(bitrate));
            set_codec(c, &CODECAPI_AVEncMPVGOPSize, u32_variant(fps * 4));
            set_codec(c, &CODECAPI_AVEncMPVDefaultBPictureCount, u32_variant(0));
        }

        let size = ((width as u64) << 32) | height as u64;
        let rate = ((fps as u64) << 32) | 1;
        let square = (1u64 << 32) | 1;
        unsafe {
            let out = mf.media_type()?;
            out.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            out.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)?;
            out.SetUINT32(&MF_MT_AVG_BITRATE, bitrate)?;
            out.SetUINT64(&MF_MT_FRAME_SIZE, size)?;
            out.SetUINT64(&MF_MT_FRAME_RATE, rate)?;
            out.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, square)?;
            out.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
            out.SetUINT32(&MF_MT_MPEG2_PROFILE, eAVEncH264VProfile_Base.0 as u32)?;
            transform
                .SetOutputType(0, &out, 0)
                .context("set H.264 output type")?;

            let input = mf.media_type()?;
            input.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            input.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)?;
            input.SetUINT64(&MF_MT_FRAME_SIZE, size)?;
            input.SetUINT64(&MF_MT_FRAME_RATE, rate)?;
            input.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, square)?;
            input.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
            input.SetUINT32(&MF_MT_DEFAULT_STRIDE, width)?;
            transform
                .SetInputType(0, &input, 0)
                .context("set NV12 input type")?;

            transform.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)?;
            transform.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)?;
        }
        let output_info = unsafe { transform.GetOutputStreamInfo(0) }?;
        let events = if is_async {
            Some(transform.cast().context("async MFT without events")?)
        } else {
            None
        };
        let mut enc = Self {
            activate,
            transform,
            codec,
            events,
            need_input: 0,
            output_info,
            out: VecDeque::new(),
            parameter_sets: Vec::new(),
            nv12: Vec::new(),
            width,
            height,
            fps,
            name,
            hardware,
        };
        enc.parameter_sets = enc.sequence_header().unwrap_or_default();
        Ok(enc)
    }

    pub fn name(&self) -> &'static str {
        self.name
    }

    pub fn is_hardware(&self) -> bool {
        self.hardware
    }

    pub fn set_bitrate(&mut self, bps: u32) -> Result<()> {
        let c = self.codec.as_ref().context("encoder has no ICodecAPI")?;
        unsafe { c.SetValue(&CODECAPI_AVEncCommonMeanBitRate, &u32_variant(bps)) }
            .context("change bitrate")
    }

    /// Encode one BGRA frame. Returns `None` while an asynchronous encoder
    /// still holds the frame (it comes out with a later call).
    pub fn encode(
        &mut self,
        bgra: &[u8],
        ts_us: u64,
        keyframe: bool,
    ) -> Result<Option<(bool, Vec<u8>)>> {
        let mf = mf()?;
        let (w, h) = (self.width as usize, self.height as usize);
        let len = w * h * 3 / 2;
        self.nv12.resize(len, 0);
        let (y, uv) = self.nv12.split_at_mut(w * h);
        super::nv12::bgra_to_nv12(bgra, w, h, y, uv);
        let sample = unsafe {
            let buf = mf.buffer(len as u32)?;
            let mut ptr = std::ptr::null_mut();
            buf.Lock(&mut ptr, None, None)?;
            std::ptr::copy_nonoverlapping(self.nv12.as_ptr(), ptr, len);
            buf.Unlock()?;
            buf.SetCurrentLength(len as u32)?;
            let sample = mf.sample()?;
            sample.AddBuffer(&buf)?;
            sample.SetSampleTime(ts_us as i64 * 10)?;
            sample.SetSampleDuration(10_000_000 / self.fps as i64)?;
            sample
        };
        if keyframe && let Some(c) = &self.codec {
            set_codec(c, &CODECAPI_AVEncVideoForceKeyFrame, u32_variant(1));
        }

        if self.events.is_some() {
            self.pump(Duration::from_secs(1), |e| e.need_input > 0)?;
            if self.need_input == 0 {
                bail!("{} never asked for input", self.name);
            }
            unsafe { self.transform.ProcessInput(0, &sample, 0) }.context("ProcessInput")?;
            self.need_input -= 1;
            // Give the GPU a moment so the frame usually comes out now
            // rather than with the next call.
            self.pump(Duration::from_millis(40), |e| !e.out.is_empty())?;
        } else {
            let mut sent = false;
            for _ in 0..2 {
                match unsafe { self.transform.ProcessInput(0, &sample, 0) } {
                    Ok(()) => {
                        sent = true;
                        break;
                    }
                    Err(e) if e.code() == MF_E_NOTACCEPTING => self.drain()?,
                    Err(e) => return Err(e).context("ProcessInput"),
                }
            }
            if !sent {
                bail!("{} won't take input", self.name);
            }
            self.drain()?;
        }
        Ok(self.out.pop_front())
    }

    /// Synchronous MFT: take every output it has.
    fn drain(&mut self) -> Result<()> {
        while self.take_output()? {}
        Ok(())
    }

    /// Asynchronous MFT: handle events until `done` or `timeout`.
    fn pump(&mut self, timeout: Duration, done: impl Fn(&Self) -> bool) -> Result<()> {
        let events = self.events.clone().expect("async encoder");
        let deadline = Instant::now() + timeout;
        while !done(self) {
            match unsafe { events.GetEvent(MF_EVENT_FLAG_NO_WAIT) } {
                Ok(ev) => {
                    let kind = unsafe { ev.GetType() }?;
                    if kind == METransformNeedInput.0 as u32 {
                        self.need_input += 1;
                    } else if kind == METransformHaveOutput.0 as u32 {
                        self.take_output()?;
                    }
                }
                Err(e) if e.code() == MF_E_NO_EVENTS_AVAILABLE => {
                    if Instant::now() >= deadline {
                        break;
                    }
                    std::thread::sleep(Duration::from_micros(500));
                }
                Err(e) => return Err(e).context("encoder event"),
            }
        }
        Ok(())
    }

    /// Pull one output sample. `Ok(false)` when the MFT has nothing.
    fn take_output(&mut self) -> Result<bool> {
        let mf = mf()?;
        let provides = self.output_info.dwFlags
            & (MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 | MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES.0)
                as u32
            != 0;
        let sample = if provides {
            None
        } else {
            let len = self
                .output_info
                .cbSize
                .max(self.width * self.height)
                .max(1 << 20);
            let s = mf.sample()?;
            unsafe { s.AddBuffer(&mf.buffer(len)?)? };
            Some(s)
        };
        let mut buffers = [MFT_OUTPUT_DATA_BUFFER {
            dwStreamID: 0,
            pSample: ManuallyDrop::new(sample),
            dwStatus: 0,
            pEvents: ManuallyDrop::new(None),
        }];
        let mut status = 0;
        let result = unsafe { self.transform.ProcessOutput(0, &mut buffers, &mut status) };
        let [buffer] = buffers;
        let sample = ManuallyDrop::into_inner(buffer.pSample);
        drop(ManuallyDrop::into_inner(buffer.pEvents));
        match result {
            Ok(()) => {}
            Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => return Ok(false),
            Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                // The encoder settled its output type: take what it offers.
                unsafe {
                    let ty = self.transform.GetOutputAvailableType(0, 0)?;
                    self.transform.SetOutputType(0, &ty, 0)?;
                    self.output_info = self.transform.GetOutputStreamInfo(0)?;
                }
                if let Some(ps) = self.sequence_header() {
                    self.parameter_sets = ps;
                }
                return Ok(true);
            }
            Err(e) => return Err(e).context("ProcessOutput"),
        }
        let sample = sample.context("encoder gave no sample")?;
        let data = unsafe {
            let buf = sample.ConvertToContiguousBuffer()?;
            let (mut ptr, mut len) = (std::ptr::null_mut(), 0u32);
            buf.Lock(&mut ptr, None, Some(&mut len))?;
            let data = std::slice::from_raw_parts(ptr, len as usize).to_vec();
            buf.Unlock()?;
            data
        };
        if data.is_empty() {
            return Ok(true);
        }
        let types = nal_types(&data);
        let clean = unsafe { sample.GetUINT32(&MFSampleExtension_CleanPoint) }.unwrap_or(0) != 0;
        let key = clean || types.contains(&5);
        let data = if types.contains(&7) {
            self.parameter_sets = parameter_sets(&data);
            data
        } else if key && !self.parameter_sets.is_empty() {
            let mut with = self.parameter_sets.clone();
            with.extend_from_slice(&data);
            with
        } else {
            data
        };
        self.out.push_back((key, data));
        Ok(true)
    }

    /// SPS and PPS from the output type, when the encoder puts them there.
    fn sequence_header(&self) -> Option<Vec<u8>> {
        unsafe {
            let ty = self.transform.GetOutputCurrentType(0).ok()?;
            let (mut p, mut n) = (std::ptr::null_mut(), 0u32);
            ty.GetAllocatedBlob(&MF_MT_MPEG_SEQUENCE_HEADER, &mut p, &mut n)
                .ok()?;
            let blob = std::slice::from_raw_parts(p, n as usize).to_vec();
            CoTaskMemFree(Some(p as *const c_void));
            let ps = parameter_sets(&blob);
            (!ps.is_empty()).then_some(ps)
        }
    }
}

impl Drop for MfEncoder {
    fn drop(&mut self) {
        unsafe {
            let _ = self
                .transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0);
            if let Ok(s) = self.transform.cast::<IMFShutdown>() {
                let _ = s.Shutdown();
            }
            let _ = self.activate.ShutdownObject();
        }
    }
}

fn set_codec(c: &ICodecAPI, api: &GUID, value: VARIANT) {
    if let Err(e) = unsafe { c.SetValue(api, &value) } {
        tracing::debug!("ICodecAPI {api:?}: {e}");
    }
}

fn u32_variant(v: u32) -> VARIANT {
    variant(VT_UI4, VARIANT_0_0_0 { ulVal: v })
}

fn bool_variant() -> VARIANT {
    variant(
        VT_BOOL,
        VARIANT_0_0_0 {
            boolVal: VARIANT_TRUE,
        },
    )
}

fn variant(vt: VARENUM, value: VARIANT_0_0_0) -> VARIANT {
    VARIANT {
        Anonymous: VARIANT_0 {
            Anonymous: ManuallyDrop::new(VARIANT_0_0 {
                vt,
                wReserved1: 0,
                wReserved2: 0,
                wReserved3: 0,
                Anonymous: value,
            }),
        },
    }
}

/// The encoder's friendly name, interned so it can be a `&'static str`.
fn friendly_name(activate: &IMFActivate) -> &'static str {
    let name = unsafe {
        let (mut p, mut n) = (PWSTR::null(), 0u32);
        match activate.GetAllocatedString(&MFT_FRIENDLY_NAME_Attribute, &mut p, &mut n) {
            Ok(()) => {
                let s = p.to_string().unwrap_or_default();
                CoTaskMemFree(Some(p.0 as *const c_void));
                s
            }
            Err(_) => String::new(),
        }
    };
    let name = if name.is_empty() {
        "Media Foundation".to_string()
    } else {
        name
    };
    static NAMES: Mutex<Vec<&'static str>> = Mutex::new(Vec::new());
    let mut names = NAMES.lock().unwrap();
    if let Some(n) = names.iter().find(|n| **n == name) {
        return n;
    }
    let leaked: &'static str = Box::leak(name.into_boxed_str());
    names.push(leaked);
    leaked
}

/// The NAL unit types in an Annex-B stream.
fn nal_types(data: &[u8]) -> Vec<u8> {
    super::annexb::nal_units(data)
        .iter()
        .map(|nal| nal[0] & 0x1f)
        .collect()
}

/// The SPS and PPS NAL units of an Annex-B stream, with start codes.
fn parameter_sets(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    for nal in super::annexb::nal_units(data) {
        if matches!(nal[0] & 0x1f, 7 | 8) {
            out.extend_from_slice(&[0, 0, 0, 1]);
            out.extend_from_slice(nal);
        }
    }
    out
}

/// The hardware encoder's name, if one opens on this machine (cached).
pub fn available() -> Option<&'static str> {
    static OK: OnceLock<Option<&'static str>> = OnceLock::new();
    *OK.get_or_init(|| match MfEncoder::new(640, 360, 30, 1_000_000, true) {
        Ok(e) => Some(e.name()),
        Err(e) => {
            tracing::debug!("Media Foundation hardware encoder unavailable: {e:#}");
            None
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_nal_units_and_parameter_sets() {
        let stream = [
            0, 0, 0, 1, 0x67, 1, 2, 0, 0, 1, 0x68, 3, 0, 0, 0, 1, 0x65, 9, 9,
        ];
        assert_eq!(nal_types(&stream), vec![7, 8, 5]);
        assert_eq!(
            parameter_sets(&stream),
            vec![0, 0, 0, 1, 0x67, 1, 2, 0, 0, 0, 1, 0x68, 3]
        );
        assert!(nal_types(&[1, 2, 3]).is_empty());
    }
}
