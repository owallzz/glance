//! NVIDIA NVENC H.264 encoding (Linux and Windows).
//!
//! The driver's libraries are loaded at run time, so glance starts on
//! machines without an NVIDIA GPU; [`NvencEncoder::new`] just fails there.
//! Frames go in as BGRA (NVENC converts to YUV on the GPU) and come out as
//! Constrained Baseline H.264, which every OpenH264 decoder handles.

use std::ffi::c_void;
use std::ptr::null_mut;
use std::sync::OnceLock;

use anyhow::{Context, Result, anyhow, bail};
use libloading::Library;

use super::nvenc_sys as sys;

const fn guid(d1: u32, d2: u16, d3: u16, d4: [u8; 8]) -> sys::GUID {
    sys::GUID {
        Data1: d1,
        Data2: d2,
        Data3: d3,
        Data4: d4,
    }
}

const CODEC_H264: sys::GUID = guid(
    0x6bc82762,
    0x4e63,
    0x4ca4,
    [0xaa, 0x85, 0x1e, 0x50, 0xf3, 0x21, 0xf6, 0xbf],
);
const PROFILE_BASELINE: sys::GUID = guid(
    0x0727bcaa,
    0x78c4,
    0x4c83,
    [0x8c, 0x2f, 0xef, 0x3d, 0xff, 0x26, 0x7c, 0x6a],
);
const PRESET_P3: sys::GUID = guid(
    0x36850110,
    0x3a07,
    0x441f,
    [0x94, 0xd5, 0x36, 0x70, 0x63, 0x1f, 0x91, 0xf6],
);

const API_VERSION: u32 = 12; // major 12, minor 0 (minor goes in bits 24..)
const fn struct_version(v: u32) -> u32 {
    API_VERSION | (v << 16) | (0x7 << 28)
}
const CONFIG_VER: u32 = struct_version(8) | (1 << 31);
const INITIALIZE_PARAMS_VER: u32 = struct_version(5) | (1 << 31);
const RECONFIGURE_PARAMS_VER: u32 = struct_version(1) | (1 << 31);
const PRESET_CONFIG_VER: u32 = struct_version(4) | (1 << 31);
const PIC_PARAMS_VER: u32 = struct_version(6) | (1 << 31);
const LOCK_BITSTREAM_VER: u32 = struct_version(2);
const LOCK_INPUT_BUFFER_VER: u32 = struct_version(1);
const CREATE_INPUT_BUFFER_VER: u32 = struct_version(1);
const CREATE_BITSTREAM_BUFFER_VER: u32 = struct_version(1);
const OPEN_SESSION_VER: u32 = struct_version(1);
const FUNCTION_LIST_VER: u32 = struct_version(2);
const RC_PARAMS_VER: u32 = struct_version(1);

#[cfg(target_os = "windows")]
const CUDA_LIB: &str = "nvcuda.dll";
#[cfg(not(target_os = "windows"))]
const CUDA_LIB: &str = "libcuda.so.1";
#[cfg(target_os = "windows")]
const NVENC_LIB: &str = "nvEncodeAPI64.dll";
#[cfg(not(target_os = "windows"))]
const NVENC_LIB: &str = "libnvidia-encode.so.1";

type CuContext = *mut c_void;

/// The driver entry points, loaded once per process and never unloaded.
struct Driver {
    _cuda: Library,
    _nvenc: Library,
    cu_ctx_create: unsafe extern "C" fn(*mut CuContext, u32, i32) -> i32,
    cu_ctx_destroy: unsafe extern "C" fn(CuContext) -> i32,
    cu_ctx_pop: unsafe extern "C" fn(*mut CuContext) -> i32,
    cu_device: i32,
    api: sys::NV_ENCODE_API_FUNCTION_LIST,
}

// SAFETY: the function table and library handles are immutable after load
// and the driver functions are thread-safe.
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
    // SAFETY: loading the NVIDIA driver libraries, whose initialisers are
    // safe to run; symbols are looked up with their documented signatures.
    unsafe {
        let cuda = Library::new(CUDA_LIB).context("no NVIDIA CUDA driver")?;
        let nvenc = Library::new(NVENC_LIB).context("no NVIDIA encode driver")?;
        let cu_init = *cuda.get::<unsafe extern "C" fn(u32) -> i32>(b"cuInit\0")?;
        let cu_device_get =
            *cuda.get::<unsafe extern "C" fn(*mut i32, i32) -> i32>(b"cuDeviceGet\0")?;
        let cu_ctx_create = *cuda.get(b"cuCtxCreate_v2\0")?;
        let cu_ctx_destroy = *cuda.get(b"cuCtxDestroy_v2\0")?;
        let cu_ctx_pop = *cuda.get(b"cuCtxPopCurrent_v2\0")?;
        let max_version = *nvenc.get::<unsafe extern "C" fn(*mut u32) -> sys::NVENCSTATUS>(
            b"NvEncodeAPIGetMaxSupportedVersion\0",
        )?;
        let create = *nvenc.get::<unsafe extern "C" fn(
            *mut sys::NV_ENCODE_API_FUNCTION_LIST,
        ) -> sys::NVENCSTATUS>(b"NvEncodeAPICreateInstance\0")?;

        let mut max = 0u32;
        check(max_version(&mut max), "NvEncodeAPIGetMaxSupportedVersion")?;
        // max is (major << 4) | minor.
        if max >> 4 < API_VERSION {
            bail!(
                "the NVIDIA driver supports NVENC {}.{}, glance needs {API_VERSION}.0 (driver 520 or newer)",
                max >> 4,
                max & 0xf
            );
        }
        let rc = cu_init(0);
        if rc != 0 {
            bail!("cuInit failed ({rc})");
        }
        let mut cu_device = 0;
        let rc = cu_device_get(&mut cu_device, 0);
        if rc != 0 {
            bail!("no CUDA device ({rc})");
        }
        let mut api = sys::NV_ENCODE_API_FUNCTION_LIST {
            version: FUNCTION_LIST_VER,
            ..Default::default()
        };
        check(create(&mut api), "NvEncodeAPICreateInstance")?;
        Ok(Driver {
            _cuda: cuda,
            _nvenc: nvenc,
            cu_ctx_create,
            cu_ctx_destroy,
            cu_ctx_pop,
            cu_device,
            api,
        })
    }
}

fn check(status: sys::NVENCSTATUS, what: &str) -> Result<()> {
    if status == sys::NV_ENC_SUCCESS {
        Ok(())
    } else {
        Err(anyhow!("{what} failed (NVENC status {status})"))
    }
}

macro_rules! api {
    ($drv:expr, $name:ident) => {
        $drv.api
            .$name
            .ok_or_else(|| anyhow!(concat!(stringify!($name), " missing")))?
    };
}

pub struct NvencEncoder {
    drv: &'static Driver,
    ctx: CuContext,
    session: *mut c_void,
    input: sys::NV_ENC_INPUT_PTR,
    output: sys::NV_ENC_OUTPUT_PTR,
    width: u32,
    height: u32,
    fps: u32,
    /// Kept alive (and at a fixed address) for reconfiguring.
    config: Box<sys::NV_ENC_CONFIG>,
    init: Box<sys::NV_ENC_INITIALIZE_PARAMS>,
    frame_idx: u32,
}

// SAFETY: the session is used from one thread at a time (&mut self); NVENC
// sessions aren't tied to the thread that created them.
unsafe impl Send for NvencEncoder {}

impl NvencEncoder {
    pub fn new(width: u32, height: u32, fps: u32, bitrate: u32) -> Result<Self> {
        let drv = driver()?;
        let mut ctx: CuContext = null_mut();
        // SAFETY: plain CUDA calls with out-pointers owned here.
        unsafe {
            let rc = (drv.cu_ctx_create)(&mut ctx, 0, drv.cu_device);
            if rc != 0 {
                bail!("cuCtxCreate failed ({rc})");
            }
            // Don't leave it current on this thread; NVENC binds it itself.
            let mut popped = null_mut();
            (drv.cu_ctx_pop)(&mut popped);
        }
        let mut enc = Self {
            drv,
            ctx,
            session: null_mut(),
            input: null_mut(),
            output: null_mut(),
            width,
            height,
            fps: fps.max(1),
            config: Box::default(),
            init: Box::default(),
            frame_idx: 0,
        };
        // On error, Drop releases whatever was created so far.
        enc.open(bitrate)?;
        Ok(enc)
    }

    fn open(&mut self, bitrate: u32) -> Result<()> {
        let drv = self.drv;
        // SAFETY: every struct passed is owned here, versioned, and outlives
        // the call; handles are checked before use.
        unsafe {
            let mut params = sys::NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS {
                version: OPEN_SESSION_VER,
                deviceType: sys::NV_ENC_DEVICE_TYPE_CUDA,
                device: self.ctx,
                apiVersion: API_VERSION,
                ..Default::default()
            };
            check(
                api!(drv, nvEncOpenEncodeSessionEx)(&mut params, &mut self.session),
                "open NVENC session",
            )?;

            let mut preset = sys::NV_ENC_PRESET_CONFIG {
                version: PRESET_CONFIG_VER,
                presetCfg: sys::NV_ENC_CONFIG {
                    version: CONFIG_VER,
                    ..Default::default()
                },
                ..Default::default()
            };
            check(
                api!(drv, nvEncGetEncodePresetConfigEx)(
                    self.session,
                    CODEC_H264,
                    PRESET_P3,
                    sys::NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY,
                    &mut preset,
                ),
                "get NVENC preset",
            )?;
            let mut config = preset.presetCfg;
            config.version = CONFIG_VER;
            config.profileGUID = PROFILE_BASELINE;
            // A keyframe every ~4 s (like the software encoder); new viewers
            // ask for one explicitly. No B-frames.
            config.gopLength = self.fps * 4;
            config.frameIntervalP = 1;
            config.rcParams.version = RC_PARAMS_VER;
            config.rcParams.rateControlMode = sys::NV_ENC_PARAMS_RC_CBR;
            config.rcParams.multiPass = sys::NV_ENC_MULTI_PASS_DISABLED;
            set_rate(&mut config, bitrate, self.fps);
            let h264 = &mut config.encodeCodecConfig.h264Config;
            h264.idrPeriod = config.gopLength;
            h264.set_repeatSPSPPS(1);
            h264.entropyCodingMode = sys::NV_ENC_H264_ENTROPY_CODING_MODE_CAVLC;
            h264.level = sys::NV_ENC_LEVEL_AUTOSELECT;
            h264.maxNumRefFrames = 1;
            *self.config = config;

            *self.init = sys::NV_ENC_INITIALIZE_PARAMS {
                version: INITIALIZE_PARAMS_VER,
                encodeGUID: CODEC_H264,
                presetGUID: PRESET_P3,
                encodeWidth: self.width,
                encodeHeight: self.height,
                darWidth: self.width,
                darHeight: self.height,
                frameRateNum: self.fps,
                frameRateDen: 1,
                enableEncodeAsync: 0,
                enablePTD: 1,
                encodeConfig: &mut *self.config,
                maxEncodeWidth: self.width,
                maxEncodeHeight: self.height,
                tuningInfo: sys::NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY,
                ..Default::default()
            };
            check(
                api!(drv, nvEncInitializeEncoder)(self.session, &mut *self.init),
                "initialise NVENC",
            )?;

            let mut input = sys::NV_ENC_CREATE_INPUT_BUFFER {
                version: CREATE_INPUT_BUFFER_VER,
                width: self.width,
                height: self.height,
                bufferFmt: sys::NV_ENC_BUFFER_FORMAT_ARGB,
                ..Default::default()
            };
            check(
                api!(drv, nvEncCreateInputBuffer)(self.session, &mut input),
                "create NVENC input buffer",
            )?;
            self.input = input.inputBuffer;
            let mut output = sys::NV_ENC_CREATE_BITSTREAM_BUFFER {
                version: CREATE_BITSTREAM_BUFFER_VER,
                ..Default::default()
            };
            check(
                api!(drv, nvEncCreateBitstreamBuffer)(self.session, &mut output),
                "create NVENC output buffer",
            )?;
            self.output = output.bitstreamBuffer;
        }
        Ok(())
    }

    pub fn set_bitrate(&mut self, bitrate: u32) -> Result<()> {
        let drv = self.drv;
        set_rate(&mut self.config, bitrate, self.fps);
        let mut init = *self.init;
        init.encodeConfig = &mut *self.config;
        let mut params = sys::NV_ENC_RECONFIGURE_PARAMS {
            version: RECONFIGURE_PARAMS_VER,
            reInitEncodeParams: init,
            ..Default::default()
        };
        // SAFETY: a live session and owned, versioned params.
        unsafe {
            check(
                api!(drv, nvEncReconfigureEncoder)(self.session, &mut params),
                "change NVENC bitrate",
            )
        }
    }

    /// Encode one BGRA frame of the encoder's size. Returns the Annex-B
    /// access unit and whether it is a keyframe.
    pub fn encode(&mut self, bgra: &[u8], ts_us: u64, keyframe: bool) -> Result<(bool, Vec<u8>)> {
        let drv = self.drv;
        let row = self.width as usize * 4;
        if bgra.len() != row * self.height as usize {
            bail!("frame buffer has the wrong size");
        }
        // SAFETY: the input buffer is locked while written, within the
        // pitch NVENC reports; the bitstream is locked while copied.
        unsafe {
            let mut lock = sys::NV_ENC_LOCK_INPUT_BUFFER {
                version: LOCK_INPUT_BUFFER_VER,
                inputBuffer: self.input,
                ..Default::default()
            };
            check(
                api!(drv, nvEncLockInputBuffer)(self.session, &mut lock),
                "lock NVENC input",
            )?;
            let pitch = lock.pitch as usize;
            let dst = lock.bufferDataPtr.cast::<u8>();
            if pitch < row || dst.is_null() {
                api!(drv, nvEncUnlockInputBuffer)(self.session, self.input);
                bail!("NVENC input buffer is too small");
            }
            for (y, src) in bgra.chunks_exact(row).enumerate() {
                std::ptr::copy_nonoverlapping(src.as_ptr(), dst.add(y * pitch), row);
            }
            check(
                api!(drv, nvEncUnlockInputBuffer)(self.session, self.input),
                "unlock NVENC input",
            )?;

            let mut pic = sys::NV_ENC_PIC_PARAMS {
                version: PIC_PARAMS_VER,
                inputWidth: self.width,
                inputHeight: self.height,
                inputPitch: pitch as u32,
                frameIdx: self.frame_idx,
                inputTimeStamp: ts_us,
                inputBuffer: self.input,
                outputBitstream: self.output,
                bufferFmt: sys::NV_ENC_BUFFER_FORMAT_ARGB,
                pictureStruct: sys::NV_ENC_PIC_STRUCT_FRAME,
                ..Default::default()
            };
            if keyframe {
                // (C enums are i32 on Windows, u32 elsewhere.)
                #[allow(clippy::unnecessary_cast)]
                let flags =
                    (sys::NV_ENC_PIC_FLAG_FORCEIDR | sys::NV_ENC_PIC_FLAG_OUTPUT_SPSPPS) as u32;
                pic.encodePicFlags = flags;
            }
            self.frame_idx = self.frame_idx.wrapping_add(1);
            check(
                api!(drv, nvEncEncodePicture)(self.session, &mut pic),
                "NVENC encode",
            )?;

            let mut out = sys::NV_ENC_LOCK_BITSTREAM {
                version: LOCK_BITSTREAM_VER,
                outputBitstream: self.output,
                ..Default::default()
            };
            check(
                api!(drv, nvEncLockBitstream)(self.session, &mut out),
                "lock NVENC output",
            )?;
            let data = std::slice::from_raw_parts(
                out.bitstreamBufferPtr.cast::<u8>(),
                out.bitstreamSizeInBytes as usize,
            )
            .to_vec();
            let key = matches!(
                out.pictureType,
                sys::NV_ENC_PIC_TYPE_IDR | sys::NV_ENC_PIC_TYPE_I
            );
            check(
                api!(drv, nvEncUnlockBitstream)(self.session, self.output),
                "unlock NVENC output",
            )?;
            Ok((key, data))
        }
    }
}

/// CBR at `bitrate` with about a quarter second of buffer: enough for
/// keyframes and scrolling to look right, small enough to stay real time.
fn set_rate(config: &mut sys::NV_ENC_CONFIG, bitrate: u32, _fps: u32) {
    config.rcParams.averageBitRate = bitrate;
    config.rcParams.maxBitRate = bitrate;
    config.rcParams.vbvBufferSize = bitrate / 4;
    config.rcParams.vbvInitialDelay = bitrate / 4;
}

impl Drop for NvencEncoder {
    fn drop(&mut self) {
        let api = &self.drv.api;
        // SAFETY: each handle is released once, buffers before the session
        // and the session before its CUDA context.
        unsafe {
            if !self.session.is_null() {
                if !self.input.is_null()
                    && let Some(f) = api.nvEncDestroyInputBuffer
                {
                    f(self.session, self.input);
                }
                if !self.output.is_null()
                    && let Some(f) = api.nvEncDestroyBitstreamBuffer
                {
                    f(self.session, self.output);
                }
                if let Some(f) = api.nvEncDestroyEncoder {
                    f(self.session);
                }
            }
            if !self.ctx.is_null() {
                (self.drv.cu_ctx_destroy)(self.ctx);
            }
        }
    }
}

/// Whether NVENC can open a session on this machine (cached).
pub fn available() -> bool {
    static OK: OnceLock<bool> = OnceLock::new();
    *OK.get_or_init(|| match NvencEncoder::new(640, 360, 30, 1_000_000) {
        Ok(_) => true,
        Err(e) => {
            tracing::debug!("NVENC unavailable: {e:#}");
            false
        }
    })
}
