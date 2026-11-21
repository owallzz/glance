//! NVIDIA NVDEC H.264 decoding (Linux and Windows).
//!
//! The driver's own bitstream parser (nvcuvid) finds the pictures, the GPU
//! decodes them, and each frame is copied back as NV12. Like NVENC, the
//! libraries are loaded at run time, so machines without an NVIDIA GPU just
//! get an error from [`NvdecDecoder::new`] and decode with OpenH264.

use std::collections::VecDeque;
use std::ffi::c_void;
use std::ptr::null_mut;
use std::sync::OnceLock;

use anyhow::{Context, Result, anyhow, bail};
use libloading::Library;

use super::nvdec_sys as sys;

#[cfg(target_os = "windows")]
const CUDA_LIB: &str = "nvcuda.dll";
#[cfg(not(target_os = "windows"))]
const CUDA_LIB: &str = "libcuda.so.1";
#[cfg(target_os = "windows")]
const CUVID_LIB: &str = "nvcuvid.dll";
#[cfg(not(target_os = "windows"))]
const CUVID_LIB: &str = "libnvcuvid.so.1";

type CuContext = *mut c_void;
type CuResult = sys::CUresult;

/// Driver entry points and the device's primary CUDA context, loaded once
/// per process and never unloaded.
struct Driver {
    _cuda: Library,
    _cuvid: Library,
    ctx: CuContext,
    ctx_push: unsafe extern "C" fn(CuContext) -> CuResult,
    ctx_pop: unsafe extern "C" fn(*mut CuContext) -> CuResult,
    memcpy_2d: unsafe extern "C" fn(*const sys::CUDA_MEMCPY2D) -> CuResult,
    create_parser:
        unsafe extern "C" fn(*mut sys::CUvideoparser, *mut sys::CUVIDPARSERPARAMS) -> CuResult,
    parse: unsafe extern "C" fn(sys::CUvideoparser, *mut sys::CUVIDSOURCEDATAPACKET) -> CuResult,
    destroy_parser: unsafe extern "C" fn(sys::CUvideoparser) -> CuResult,
    create_decoder:
        unsafe extern "C" fn(*mut sys::CUvideodecoder, *mut sys::CUVIDDECODECREATEINFO) -> CuResult,
    destroy_decoder: unsafe extern "C" fn(sys::CUvideodecoder) -> CuResult,
    decode_picture: unsafe extern "C" fn(sys::CUvideodecoder, *mut sys::CUVIDPICPARAMS) -> CuResult,
    map_frame: unsafe extern "C" fn(
        sys::CUvideodecoder,
        i32,
        *mut u64,
        *mut u32,
        *mut sys::CUVIDPROCPARAMS,
    ) -> CuResult,
    unmap_frame: unsafe extern "C" fn(sys::CUvideodecoder, u64) -> CuResult,
}

// SAFETY: immutable after load; the CUDA driver API is thread-safe and the
// context is pushed around every use.
unsafe impl Send for Driver {}
unsafe impl Sync for Driver {}

fn driver() -> Result<&'static Driver> {
    static DRIVER: OnceLock<std::result::Result<Driver, String>> = OnceLock::new();
    DRIVER
        .get_or_init(|| load().map_err(|e| format!("{e:#}")))
        .as_ref()
        .map_err(|e| anyhow!("{e}"))
}

fn load() -> Result<Driver> {
    // SAFETY: loading the NVIDIA driver libraries; symbols are looked up
    // with their documented signatures.
    unsafe {
        let cuda = Library::new(CUDA_LIB).context("no NVIDIA CUDA driver")?;
        let cuvid = Library::new(CUVID_LIB).context("no NVIDIA decode driver")?;
        let cu_init = *cuda.get::<unsafe extern "C" fn(u32) -> CuResult>(b"cuInit\0")?;
        let device_get =
            *cuda.get::<unsafe extern "C" fn(*mut i32, i32) -> CuResult>(b"cuDeviceGet\0")?;
        let retain = *cuda.get::<unsafe extern "C" fn(*mut CuContext, i32) -> CuResult>(
            b"cuDevicePrimaryCtxRetain\0",
        )?;
        check(cu_init(0), "cuInit")?;
        let mut device = 0;
        check(device_get(&mut device, 0), "cuDeviceGet")?;
        let mut ctx = null_mut();
        check(retain(&mut ctx, device), "cuDevicePrimaryCtxRetain")?;
        Ok(Driver {
            ctx,
            ctx_push: *cuda.get(b"cuCtxPushCurrent_v2\0")?,
            ctx_pop: *cuda.get(b"cuCtxPopCurrent_v2\0")?,
            memcpy_2d: *cuda.get(b"cuMemcpy2D_v2\0")?,
            create_parser: *cuvid.get(b"cuvidCreateVideoParser\0")?,
            parse: *cuvid.get(b"cuvidParseVideoData\0")?,
            destroy_parser: *cuvid.get(b"cuvidDestroyVideoParser\0")?,
            create_decoder: *cuvid.get(b"cuvidCreateDecoder\0")?,
            destroy_decoder: *cuvid.get(b"cuvidDestroyDecoder\0")?,
            decode_picture: *cuvid.get(b"cuvidDecodePicture\0")?,
            map_frame: *cuvid.get(b"cuvidMapVideoFrame64\0")?,
            unmap_frame: *cuvid.get(b"cuvidUnmapVideoFrame64\0")?,
            _cuda: cuda,
            _cuvid: cuvid,
        })
    }
}

fn check(rc: CuResult, what: &str) -> Result<()> {
    if rc == sys::CUDA_SUCCESS {
        Ok(())
    } else {
        Err(anyhow!("{what} failed (CUDA error {rc})"))
    }
}

/// A decoded picture, tightly packed (both strides are `width`).
pub struct Nv12Frame {
    pub width: u32,
    pub height: u32,
    pub y: Vec<u8>,
    pub uv: Vec<u8>,
}

/// What the parser callbacks work on. Boxed so its address stays fixed.
struct State {
    drv: &'static Driver,
    decoder: sys::CUvideodecoder,
    /// The stream format, kept to remake the decoder at a new output size.
    format: Option<sys::CUVIDEOFORMAT>,
    surfaces: u32,
    /// The tile the frames are shown in ((0, 0): full size).
    target: (u32, u32),
    /// Output size the decoder scales to (on the GPU).
    width: u32,
    height: u32,
    frames: VecDeque<Nv12Frame>,
    error: Option<String>,
}

pub struct NvdecDecoder {
    parser: sys::CUvideoparser,
    state: Box<State>,
}

// SAFETY: used from one thread at a time (&mut self); the CUDA context is
// pushed onto whichever thread calls in.
unsafe impl Send for NvdecDecoder {}

impl NvdecDecoder {
    pub fn new() -> Result<Self> {
        let drv = driver()?;
        let mut state = Box::new(State {
            drv,
            decoder: null_mut(),
            format: None,
            surfaces: 0,
            target: (0, 0),
            width: 0,
            height: 0,
            frames: VecDeque::new(),
            error: None,
        });
        let mut params = sys::CUVIDPARSERPARAMS {
            CodecType: sys::cudaVideoCodec_H264,
            // Raised by the sequence callback's return value.
            ulMaxNumDecodeSurfaces: 1,
            ulMaxDisplayDelay: 0,
            pUserData: (&mut *state as *mut State).cast(),
            pfnSequenceCallback: Some(on_sequence),
            pfnDecodePicture: Some(on_decode),
            pfnDisplayPicture: Some(on_display),
            ..Default::default()
        };
        let mut parser = null_mut();
        // SAFETY: params and state outlive the parser (destroyed in Drop).
        unsafe {
            check(
                (drv.create_parser)(&mut parser, &mut params),
                "cuvidCreateVideoParser",
            )?
        };
        Ok(Self { parser, state })
    }

    /// Feed one packet (a whole picture, Annex-B). Returns the newest
    /// picture it completed, scaled on the GPU to fit `target` ((0, 0) for
    /// full size). A new `target` takes effect at the next keyframe, as the
    /// decoder is remade for it.
    pub fn decode(&mut self, data: &[u8], target: (u32, u32)) -> Result<Option<Nv12Frame>> {
        let drv = self.state.drv;
        self.state.target = target;
        let resize = self
            .state
            .format
            .is_some_and(|f| output_size(&f, target) != (self.state.width, self.state.height));
        if resize && !self.state.decoder.is_null() && has_idr(data) {
            // References start over at an IDR, so a new decoder loses
            // nothing; on_decode makes it.
            unsafe {
                check((drv.ctx_push)(drv.ctx), "cuCtxPushCurrent")?;
                (drv.destroy_decoder)(self.state.decoder);
                let mut popped = null_mut();
                (drv.ctx_pop)(&mut popped);
            }
            self.state.decoder = null_mut();
        }
        let mut packet = sys::CUVIDSOURCEDATAPACKET {
            flags: sys::CUVID_PKT_ENDOFPICTURE as _,
            payload_size: data.len() as _,
            payload: data.as_ptr(),
            timestamp: 0,
        };
        // SAFETY: the context is pushed for the callbacks' CUDA calls and
        // popped again; `data` outlives the call.
        if let Some(e) = self.state.error.take() {
            bail!("{e}");
        }
        let rc = unsafe {
            check((drv.ctx_push)(drv.ctx), "cuCtxPushCurrent")?;
            let rc = (drv.parse)(self.parser, &mut packet);
            let mut popped = null_mut();
            (drv.ctx_pop)(&mut popped);
            rc
        };
        if let Some(e) = self.state.error.take() {
            bail!("{e}");
        }
        check(rc, "cuvidParseVideoData")?;
        let newest = self.state.frames.pop_back();
        self.state.frames.clear();
        Ok(newest)
    }
}

impl NvdecDecoder {
    /// Make the next decode fail, as a broken driver would (tests).
    #[cfg(test)]
    pub(crate) fn fail_next(&mut self) {
        self.state.error = Some("injected failure".into());
    }
}

impl Drop for NvdecDecoder {
    fn drop(&mut self) {
        let drv = self.state.drv;
        // SAFETY: destroying what new() and the callbacks created.
        unsafe {
            if (drv.ctx_push)(drv.ctx) == sys::CUDA_SUCCESS {
                (drv.destroy_parser)(self.parser);
                if !self.state.decoder.is_null() {
                    (drv.destroy_decoder)(self.state.decoder);
                }
                let mut popped = null_mut();
                (drv.ctx_pop)(&mut popped);
            }
        }
    }
}

/// New stream format: (re)create the decoder. Returns how many decode
/// surfaces the parser should use (0 on failure).
unsafe extern "C" fn on_sequence(user: *mut c_void, fmt: *mut sys::CUVIDEOFORMAT) -> i32 {
    // SAFETY: the parser passes back our State and a valid format.
    let (state, fmt) = unsafe { (&mut *(user as *mut State), &*fmt) };
    match state.configure(fmt) {
        Ok(surfaces) => surfaces as i32,
        Err(e) => {
            state.error = Some(format!("{e:#}"));
            0
        }
    }
}

unsafe extern "C" fn on_decode(user: *mut c_void, pic: *mut sys::CUVIDPICPARAMS) -> i32 {
    // SAFETY: as above; the picture parameters are valid for this call.
    let state = unsafe { &mut *(user as *mut State) };
    if state.decoder.is_null() {
        let Some(fmt) = state.format else {
            state.error = Some("picture before sequence header".into());
            return 0;
        };
        if let Err(e) = state.configure(&fmt) {
            state.error = Some(format!("{e:#}"));
            return 0;
        }
    }
    let rc = unsafe { (state.drv.decode_picture)(state.decoder, pic) };
    if rc != sys::CUDA_SUCCESS {
        state.error = Some(format!("cuvidDecodePicture failed (CUDA error {rc})"));
        return 0;
    }
    1
}

unsafe extern "C" fn on_display(user: *mut c_void, info: *mut sys::CUVIDPARSERDISPINFO) -> i32 {
    // SAFETY: as above. A null info means end of stream.
    let state = unsafe { &mut *(user as *mut State) };
    let Some(info) = (unsafe { info.as_ref() }) else {
        return 1;
    };
    match unsafe { state.copy_out(info) } {
        Ok(frame) => {
            state.frames.push_back(frame);
            1
        }
        Err(e) => {
            state.error = Some(format!("{e:#}"));
            0
        }
    }
}

impl State {
    fn configure(&mut self, fmt: &sys::CUVIDEOFORMAT) -> Result<u32> {
        if fmt.codec != sys::cudaVideoCodec_H264 {
            bail!("not H.264");
        }
        if fmt.chroma_format != sys::cudaVideoChromaFormat_420 || fmt.bit_depth_luma_minus8 != 0 {
            bail!("only 8-bit 4:2:0 is decoded on the GPU");
        }
        let surfaces = (fmt.min_num_decode_surfaces as u32).max(2);
        let same_stream = self.format.is_some_and(|f| {
            (
                f.coded_width,
                f.coded_height,
                f.display_area.right,
                f.display_area.bottom,
            ) == (
                fmt.coded_width,
                fmt.coded_height,
                fmt.display_area.right,
                fmt.display_area.bottom,
            )
        });
        // A repeated SPS mid-stream keeps the decoder (and its references);
        // a new output size waits for an IDR (see decode()).
        if same_stream && surfaces == self.surfaces && !self.decoder.is_null() {
            return Ok(surfaces);
        }
        let (w, h) = output_size(fmt, self.target);
        let area = &fmt.display_area;
        let (dw, dh) = display_size(fmt);
        if !self.decoder.is_null() {
            // SAFETY: the old decoder is ours and idle between pictures.
            unsafe { (self.drv.destroy_decoder)(self.decoder) };
            self.decoder = null_mut();
        }
        let mut info = sys::CUVIDDECODECREATEINFO {
            ulWidth: fmt.coded_width as _,
            ulHeight: fmt.coded_height as _,
            ulNumDecodeSurfaces: surfaces as _,
            CodecType: fmt.codec,
            ChromaFormat: fmt.chroma_format,
            ulCreationFlags: sys::cudaVideoCreate_PreferCUVID as _,
            ulMaxWidth: fmt.coded_width as _,
            ulMaxHeight: fmt.coded_height as _,
            OutputFormat: sys::cudaVideoSurfaceFormat_NV12,
            DeinterlaceMode: sys::cudaVideoDeinterlaceMode_Weave,
            ulTargetWidth: w as _,
            ulTargetHeight: h as _,
            ulNumOutputSurfaces: 2,
            ..Default::default()
        };
        info.display_area.left = area.left as i16;
        info.display_area.top = area.top as i16;
        info.display_area.right = (area.left as u32 + dw) as i16;
        info.display_area.bottom = (area.top as u32 + dh) as i16;
        let mut decoder = null_mut();
        // SAFETY: the context is current (pushed by decode()).
        unsafe {
            check(
                (self.drv.create_decoder)(&mut decoder, &mut info),
                "cuvidCreateDecoder",
            )?
        };
        self.decoder = decoder;
        self.format = Some(*fmt);
        self.surfaces = surfaces;
        (self.width, self.height) = (w, h);
        Ok(surfaces)
    }

    /// Map the decoded surface and copy it to host memory as NV12.
    unsafe fn copy_out(&mut self, info: &sys::CUVIDPARSERDISPINFO) -> Result<Nv12Frame> {
        let mut proc = sys::CUVIDPROCPARAMS {
            progressive_frame: info.progressive_frame,
            second_field: info.repeat_first_field + 1,
            top_field_first: info.top_field_first,
            unpaired_field: (info.repeat_first_field < 0) as i32,
            ..Default::default()
        };
        let (mut dev, mut pitch) = (0u64, 0u32);
        unsafe {
            check(
                (self.drv.map_frame)(
                    self.decoder,
                    info.picture_index,
                    &mut dev,
                    &mut pitch,
                    &mut proc,
                ),
                "cuvidMapVideoFrame",
            )?;
        }
        let (w, h) = (self.width as usize, self.height as usize);
        let mut y = vec![0u8; w * h];
        let mut uv = vec![0u8; w * h / 2];
        // The chroma plane follows the luma plane of the output surface.
        let uv_dev = dev + pitch as u64 * ((self.height as u64 + 1) & !1);
        let copy = |src: u64, dst: &mut [u8], rows: usize| {
            let params = sys::CUDA_MEMCPY2D {
                srcMemoryType: sys::CU_MEMORYTYPE_DEVICE,
                srcDevice: src,
                srcPitch: pitch as usize,
                dstMemoryType: sys::CU_MEMORYTYPE_HOST,
                dstHost: dst.as_mut_ptr().cast(),
                dstPitch: w,
                WidthInBytes: w,
                Height: rows,
                ..Default::default()
            };
            // SAFETY: the source is the mapped surface, the destination
            // holds `rows` rows of `w` bytes.
            check(unsafe { (self.drv.memcpy_2d)(&params) }, "cuMemcpy2D")
        };
        let result = copy(dev, &mut y, h).and_then(|()| copy(uv_dev, &mut uv, h / 2));
        unsafe { (self.drv.unmap_frame)(self.decoder, dev) };
        result?;
        Ok(Nv12Frame {
            width: self.width,
            height: self.height,
            y,
            uv,
        })
    }
}

/// The visible picture size of a stream, even.
fn display_size(fmt: &sys::CUVIDEOFORMAT) -> (u32, u32) {
    let a = &fmt.display_area;
    (
        (a.right - a.left).max(2) as u32 & !1,
        (a.bottom - a.top).max(2) as u32 & !1,
    )
}

/// What the decoder outputs for a `target` tile: the size the frame is
/// shown at, so the CPU has nothing left to scale.
fn output_size(fmt: &sys::CUVIDEOFORMAT, target: (u32, u32)) -> (u32, u32) {
    crate::codec::tile_size(display_size(fmt), target)
}

/// Whether an Annex-B packet holds an IDR slice.
fn has_idr(data: &[u8]) -> bool {
    data.windows(4)
        .any(|w| w[0] == 0 && w[1] == 0 && w[2] == 1 && w[3] & 0x1f == 5)
}

/// Whether NVDEC is usable here (cached; loads the driver once).
pub fn available() -> bool {
    static OK: OnceLock<bool> = OnceLock::new();
    *OK.get_or_init(|| match driver() {
        Ok(_) => true,
        Err(e) => {
            tracing::debug!("NVDEC unavailable: {e:#}");
            false
        }
    })
}
