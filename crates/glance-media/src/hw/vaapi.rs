//! VA-API H.264 encoding (Linux: AMD and Intel GPUs).
//!
//! libva is loaded at run time. Each `/dev/dri/renderD*` node is tried until
//! one offers a Constrained Baseline encoder. Frames are converted from BGRA
//! to NV12 on the CPU (in parallel) and uploaded. Drivers that take
//! "packed headers" (Mesa does, and then writes none itself) get SPS, PPS
//! and slice headers from [`super::h264_headers`].
//! P-frames only, one reference: the simplest stream every decoder takes.

use std::ffi::{c_char, c_int, c_uint, c_void};
use std::os::fd::AsRawFd;
use std::ptr::null_mut;
use std::sync::OnceLock;

use anyhow::{Context, Result, anyhow, bail};
use libloading::Library;

use super::h264_headers::StreamHeaders;
use super::va_sys as sys;

type Dpy = sys::VADisplay;
type Status = sys::VAStatus;
type MessageCallback = Option<unsafe extern "C" fn(*mut c_void, *const c_char)>;

/// libva entry points, loaded once and never unloaded.
struct Va {
    _va: Library,
    _drm: Library,
    get_display_drm: unsafe extern "C" fn(c_int) -> Dpy,
    initialize: unsafe extern "C" fn(Dpy, *mut c_int, *mut c_int) -> Status,
    terminate: unsafe extern "C" fn(Dpy) -> Status,
    set_info_callback: unsafe extern "C" fn(Dpy, MessageCallback, *mut c_void) -> MessageCallback,
    set_error_callback: unsafe extern "C" fn(Dpy, MessageCallback, *mut c_void) -> MessageCallback,
    max_num_entrypoints: unsafe extern "C" fn(Dpy) -> c_int,
    query_config_entrypoints:
        unsafe extern "C" fn(Dpy, sys::VAProfile, *mut sys::VAEntrypoint, *mut c_int) -> Status,
    get_config_attributes: unsafe extern "C" fn(
        Dpy,
        sys::VAProfile,
        sys::VAEntrypoint,
        *mut sys::VAConfigAttrib,
        c_int,
    ) -> Status,
    create_config: unsafe extern "C" fn(
        Dpy,
        sys::VAProfile,
        sys::VAEntrypoint,
        *mut sys::VAConfigAttrib,
        c_int,
        *mut sys::VAConfigID,
    ) -> Status,
    destroy_config: unsafe extern "C" fn(Dpy, sys::VAConfigID) -> Status,
    create_surfaces: unsafe extern "C" fn(
        Dpy,
        c_uint,
        c_uint,
        c_uint,
        *mut sys::VASurfaceID,
        c_uint,
        *mut c_void,
        c_uint,
    ) -> Status,
    destroy_surfaces: unsafe extern "C" fn(Dpy, *mut sys::VASurfaceID, c_int) -> Status,
    create_context: unsafe extern "C" fn(
        Dpy,
        sys::VAConfigID,
        c_int,
        c_int,
        c_int,
        *mut sys::VASurfaceID,
        c_int,
        *mut sys::VAContextID,
    ) -> Status,
    destroy_context: unsafe extern "C" fn(Dpy, sys::VAContextID) -> Status,
    create_buffer: unsafe extern "C" fn(
        Dpy,
        sys::VAContextID,
        sys::VABufferType,
        c_uint,
        c_uint,
        *mut c_void,
        *mut sys::VABufferID,
    ) -> Status,
    destroy_buffer: unsafe extern "C" fn(Dpy, sys::VABufferID) -> Status,
    map_buffer: unsafe extern "C" fn(Dpy, sys::VABufferID, *mut *mut c_void) -> Status,
    unmap_buffer: unsafe extern "C" fn(Dpy, sys::VABufferID) -> Status,
    begin_picture: unsafe extern "C" fn(Dpy, sys::VAContextID, sys::VASurfaceID) -> Status,
    render_picture:
        unsafe extern "C" fn(Dpy, sys::VAContextID, *mut sys::VABufferID, c_int) -> Status,
    end_picture: unsafe extern "C" fn(Dpy, sys::VAContextID) -> Status,
    sync_surface: unsafe extern "C" fn(Dpy, sys::VASurfaceID) -> Status,
    derive_image: unsafe extern "C" fn(Dpy, sys::VASurfaceID, *mut sys::VAImage) -> Status,
    create_image: unsafe extern "C" fn(
        Dpy,
        *mut sys::VAImageFormat,
        c_int,
        c_int,
        *mut sys::VAImage,
    ) -> Status,
    put_image: unsafe extern "C" fn(
        Dpy,
        sys::VASurfaceID,
        sys::VAImageID,
        c_int,
        c_int,
        c_uint,
        c_uint,
        c_int,
        c_int,
        c_uint,
        c_uint,
    ) -> Status,
    destroy_image: unsafe extern "C" fn(Dpy, sys::VAImageID) -> Status,
}

// SAFETY: immutable function pointers into libva, which is thread-safe.
unsafe impl Send for Va {}
unsafe impl Sync for Va {}

fn va() -> Result<&'static Va> {
    static VA: OnceLock<std::result::Result<Va, String>> = OnceLock::new();
    VA.get_or_init(|| load().map_err(|e| format!("{e:#}")))
        .as_ref()
        .map_err(|e| anyhow!("{e}"))
}

fn load() -> Result<Va> {
    // SAFETY: loading libva and looking symbols up with their documented
    // C signatures.
    unsafe {
        let lib = Library::new("libva.so.2").context("libva is not installed")?;
        let drm = Library::new("libva-drm.so.2").context("libva-drm is not installed")?;
        macro_rules! sym {
            ($l:expr, $n:literal) => {
                *$l.get(concat!($n, "\0").as_bytes())
                    .context(concat!("libva lacks ", $n))?
            };
        }
        Ok(Va {
            get_display_drm: sym!(drm, "vaGetDisplayDRM"),
            initialize: sym!(lib, "vaInitialize"),
            terminate: sym!(lib, "vaTerminate"),
            set_info_callback: sym!(lib, "vaSetInfoCallback"),
            set_error_callback: sym!(lib, "vaSetErrorCallback"),
            max_num_entrypoints: sym!(lib, "vaMaxNumEntrypoints"),
            query_config_entrypoints: sym!(lib, "vaQueryConfigEntrypoints"),
            get_config_attributes: sym!(lib, "vaGetConfigAttributes"),
            create_config: sym!(lib, "vaCreateConfig"),
            destroy_config: sym!(lib, "vaDestroyConfig"),
            create_surfaces: sym!(lib, "vaCreateSurfaces"),
            destroy_surfaces: sym!(lib, "vaDestroySurfaces"),
            create_context: sym!(lib, "vaCreateContext"),
            destroy_context: sym!(lib, "vaDestroyContext"),
            create_buffer: sym!(lib, "vaCreateBuffer"),
            destroy_buffer: sym!(lib, "vaDestroyBuffer"),
            map_buffer: sym!(lib, "vaMapBuffer"),
            unmap_buffer: sym!(lib, "vaUnmapBuffer"),
            begin_picture: sym!(lib, "vaBeginPicture"),
            render_picture: sym!(lib, "vaRenderPicture"),
            end_picture: sym!(lib, "vaEndPicture"),
            sync_surface: sym!(lib, "vaSyncSurface"),
            derive_image: sym!(lib, "vaDeriveImage"),
            create_image: sym!(lib, "vaCreateImage"),
            put_image: sym!(lib, "vaPutImage"),
            destroy_image: sym!(lib, "vaDestroyImage"),
            _va: lib,
            _drm: drm,
        })
    }
}

fn check(status: Status, what: &str) -> Result<()> {
    if status == sys::VA_STATUS_SUCCESS as Status {
        Ok(())
    } else {
        Err(anyhow!("{what} failed (VA status {status:#x})"))
    }
}

const PROFILE: sys::VAProfile = sys::VAProfileH264ConstrainedBaseline;
/// Level 5.2: enough for 4K60.
const LEVEL_IDC: u8 = 52;
/// frame_num and POC lsb both wrap at 256 (log2 max = 8).
const LOG2_MAX: u32 = 8;
const SLICE_P: u8 = 0;
const SLICE_I: u8 = 2;

/// An initialised VA display on one render node.
struct Display {
    va: &'static Va,
    dpy: Dpy,
    _node: std::fs::File,
    entrypoint: sys::VAEntrypoint,
}

impl Drop for Display {
    fn drop(&mut self) {
        // SAFETY: terminate the display we initialised, once.
        unsafe {
            (self.va.terminate)(self.dpy);
        }
    }
}

/// The first render node whose driver encodes Constrained Baseline H.264.
fn open_display() -> Result<Display> {
    let va = va()?;
    let mut last_err = anyhow!("no GPU render nodes");
    for n in 128..136 {
        let path = format!("/dev/dri/renderD{n}");
        let Ok(node) = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
        else {
            continue;
        };
        // SAFETY: libva calls on a display created from an open fd that the
        // Display keeps alive; out-pointers are owned here.
        unsafe {
            let dpy = (va.get_display_drm)(node.as_raw_fd());
            if dpy.is_null() {
                continue;
            }
            // libva prints "libva info: ..." to stderr by default.
            (va.set_info_callback)(dpy, None, null_mut());
            (va.set_error_callback)(dpy, None, null_mut());
            let (mut major, mut minor) = (0, 0);
            if (va.initialize)(dpy, &mut major, &mut minor) != 0 {
                (va.terminate)(dpy);
                continue;
            }
            let max = (va.max_num_entrypoints)(dpy).max(1) as usize;
            let mut eps = vec![0 as sys::VAEntrypoint; max];
            let mut count = 0;
            let rc = (va.query_config_entrypoints)(dpy, PROFILE, eps.as_mut_ptr(), &mut count);
            eps.truncate(count.max(0) as usize);
            let entrypoint = [sys::VAEntrypointEncSlice, sys::VAEntrypointEncSliceLP]
                .into_iter()
                .find(|e| rc == 0 && eps.contains(e));
            match entrypoint {
                Some(entrypoint) => {
                    return Ok(Display {
                        va,
                        dpy,
                        _node: node,
                        entrypoint,
                    });
                }
                None => {
                    last_err = anyhow!("{path} has no H.264 encoder");
                    (va.terminate)(dpy);
                }
            }
        }
    }
    Err(last_err)
}

pub struct VaapiEncoder {
    disp: Display,
    config: sys::VAConfigID,
    context: sys::VAContextID,
    /// [input, reconstructed A, reconstructed B]
    surfaces: [sys::VASurfaceID; 3],
    coded: sys::VABufferID,
    /// A separate upload image when the driver can't map surfaces directly.
    upload: Option<sys::VAImage>,
    width: u32,
    height: u32,
    mb_w: u32,
    mb_h: u32,
    fps: u32,
    bitrate: u32,
    rc_changed: bool,
    gop: u32,
    /// Frames since the last IDR (0 = this one is an IDR).
    since_idr: u32,
    idr_pic_id: u16,
    /// Which reconstructed surface (1 or 2) the next frame writes.
    recon: usize,
    force_idr: bool,
    nv12: Vec<u8>,
    /// Which headers we write (VA_ENC_PACKED_HEADER_* bits).
    packed: u32,
    headers: StreamHeaders,
}

// SAFETY: used from one thread at a time (&mut self); VA handles aren't
// tied to the creating thread.
unsafe impl Send for VaapiEncoder {}

impl VaapiEncoder {
    pub fn new(width: u32, height: u32, fps: u32, bitrate: u32) -> Result<Self> {
        let disp = open_display()?;
        let va = disp.va;
        let (mb_w, mb_h) = (width.div_ceil(16), height.div_ceil(16));
        let mut enc = Self {
            disp,
            config: sys::VA_INVALID_ID,
            context: sys::VA_INVALID_ID,
            surfaces: [sys::VA_INVALID_SURFACE; 3],
            coded: sys::VA_INVALID_ID,
            upload: None,
            width,
            height,
            mb_w,
            mb_h,
            fps: fps.max(1),
            bitrate,
            rc_changed: false,
            gop: fps.max(1) * 4,
            since_idr: 0,
            idr_pic_id: 0,
            recon: 1,
            force_idr: true,
            nv12: Vec::new(),
            packed: 0,
            headers: StreamHeaders {
                width,
                height,
                level_idc: LEVEL_IDC,
                log2_max: LOG2_MAX,
            },
        };
        let dpy = enc.disp.dpy;
        // SAFETY: libva calls on our display with owned out-pointers; on
        // error, Drop releases what was created.
        unsafe {
            let mut attrs = [
                sys::VAConfigAttrib {
                    type_: sys::VAConfigAttribRTFormat,
                    value: 0,
                },
                sys::VAConfigAttrib {
                    type_: sys::VAConfigAttribRateControl,
                    value: 0,
                },
                sys::VAConfigAttrib {
                    type_: sys::VAConfigAttribEncPackedHeaders,
                    value: 0,
                },
            ];
            check(
                (va.get_config_attributes)(
                    dpy,
                    PROFILE,
                    enc.disp.entrypoint,
                    attrs.as_mut_ptr(),
                    attrs.len() as c_int,
                ),
                "query VA encoder",
            )?;
            if attrs[0].value & sys::VA_RT_FORMAT_YUV420 == 0 {
                bail!("VA encoder doesn't take YUV 4:2:0");
            }
            let rc = attrs[1].value;
            let rc_mode = if rc != sys::VA_ATTRIB_NOT_SUPPORTED && rc & sys::VA_RC_CBR != 0 {
                sys::VA_RC_CBR
            } else if rc != sys::VA_ATTRIB_NOT_SUPPORTED && rc & sys::VA_RC_VBR != 0 {
                sys::VA_RC_VBR
            } else {
                bail!("VA encoder has no bitrate control");
            };
            attrs[0].value = sys::VA_RT_FORMAT_YUV420;
            attrs[1].value = rc_mode;
            let wanted = sys::VA_ENC_PACKED_HEADER_SEQUENCE
                | sys::VA_ENC_PACKED_HEADER_PICTURE
                | sys::VA_ENC_PACKED_HEADER_SLICE;
            enc.packed = match attrs[2].value {
                sys::VA_ATTRIB_NOT_SUPPORTED => 0,
                v => v & wanted,
            };
            attrs[2].value = enc.packed;
            let count = if enc.packed == 0 { 2 } else { 3 };
            check(
                (va.create_config)(
                    dpy,
                    PROFILE,
                    enc.disp.entrypoint,
                    attrs.as_mut_ptr(),
                    count,
                    &mut enc.config,
                ),
                "create VA config",
            )?;
            let (aw, ah) = (mb_w * 16, mb_h * 16);
            check(
                (va.create_surfaces)(
                    dpy,
                    sys::VA_RT_FORMAT_YUV420,
                    aw,
                    ah,
                    enc.surfaces.as_mut_ptr(),
                    3,
                    null_mut(),
                    0,
                ),
                "create VA surfaces",
            )?;
            check(
                (va.create_context)(
                    dpy,
                    enc.config,
                    aw as c_int,
                    ah as c_int,
                    sys::VA_PROGRESSIVE as c_int,
                    enc.surfaces.as_mut_ptr(),
                    3,
                    &mut enc.context,
                ),
                "create VA context",
            )?;
            // Room for an incompressible frame.
            let coded_size = (aw * ah * 3 / 2).max(1 << 20);
            check(
                (va.create_buffer)(
                    dpy,
                    enc.context,
                    sys::VAEncCodedBufferType,
                    coded_size,
                    1,
                    null_mut(),
                    &mut enc.coded,
                ),
                "create VA output buffer",
            )?;
            // Prefer writing straight into the input surface.
            let mut probe = sys::VAImage::default();
            let direct = (va.derive_image)(dpy, enc.surfaces[0], &mut probe) == 0;
            if direct {
                let nv12 = probe.format.fourcc == sys::VA_FOURCC_NV12;
                (va.destroy_image)(dpy, probe.image_id);
                if !nv12 {
                    bail!("VA surfaces aren't NV12");
                }
            } else {
                let mut fmt = sys::VAImageFormat {
                    fourcc: sys::VA_FOURCC_NV12,
                    byte_order: 1, // VA_LSB_FIRST
                    bits_per_pixel: 12,
                    ..Default::default()
                };
                let mut img = sys::VAImage::default();
                check(
                    (va.create_image)(dpy, &mut fmt, aw as c_int, ah as c_int, &mut img),
                    "create VA upload image",
                )?;
                enc.upload = Some(img);
            }
        }
        Ok(enc)
    }

    pub fn set_bitrate(&mut self, bitrate: u32) {
        self.bitrate = bitrate;
        self.rc_changed = true;
    }

    /// Encode one BGRA frame of the encoder's size: `(keyframe, Annex-B)`.
    pub fn encode(&mut self, bgra: &[u8], keyframe: bool) -> Result<(bool, Vec<u8>)> {
        if bgra.len() != self.width as usize * self.height as usize * 4 {
            bail!("frame buffer has the wrong size");
        }
        if keyframe || self.force_idr || self.since_idr >= self.gop {
            self.since_idr = 0;
            self.force_idr = false;
        }
        let idr = self.since_idr == 0;
        self.upload_frame(bgra)?;
        let va = self.disp.va;
        let dpy = self.disp.dpy;
        let mut buffers = Vec::with_capacity(8);
        let result = (|| -> Result<Vec<u8>> {
            if idr || self.rc_changed {
                self.rc_changed = false;
                if idr {
                    let seq = self.sequence_params();
                    buffers.push(self.buffer(sys::VAEncSequenceParameterBufferType, &seq)?);
                }
                for misc in self.misc_params() {
                    buffers.push(self.raw_buffer(sys::VAEncMiscParameterBufferType, &misc)?);
                }
            }
            let (pic, slice) = self.picture_params(idr);
            buffers.push(self.buffer(sys::VAEncPictureParameterBufferType, &pic)?);
            if idr && self.packed & sys::VA_ENC_PACKED_HEADER_SEQUENCE != 0 {
                let sps = self.headers.sps();
                let bits = sps.len() as u32 * 8;
                self.packed_header(&mut buffers, sys::VAEncPackedHeaderSequence, &sps, bits)?;
            }
            if idr && self.packed & sys::VA_ENC_PACKED_HEADER_PICTURE != 0 {
                let pps = self.headers.pps();
                let bits = pps.len() as u32 * 8;
                self.packed_header(&mut buffers, sys::VAEncPackedHeaderPicture, &pps, bits)?;
            }
            if self.packed & sys::VA_ENC_PACKED_HEADER_SLICE != 0 {
                let (hdr, bits) = self.headers.slice_header(
                    idr,
                    pic.frame_num as u32,
                    slice.idr_pic_id as u32,
                    slice.pic_order_cnt_lsb as u32,
                );
                self.packed_header(&mut buffers, sys::VAEncPackedHeaderSlice, &hdr, bits)?;
            }
            buffers.push(self.buffer(sys::VAEncSliceParameterBufferType, &slice)?);
            // SAFETY: a live context; buffers were created on it and stay
            // alive until after the picture completes.
            unsafe {
                check(
                    (va.begin_picture)(dpy, self.context, self.surfaces[0]),
                    "VA begin picture",
                )?;
                check(
                    (va.render_picture)(
                        dpy,
                        self.context,
                        buffers.as_mut_ptr(),
                        buffers.len() as c_int,
                    ),
                    "VA render picture",
                )?;
                check((va.end_picture)(dpy, self.context), "VA end picture")?;
                check((va.sync_surface)(dpy, self.surfaces[0]), "VA sync")?;
            }
            self.read_coded()
        })();
        for b in buffers {
            // SAFETY: buffers we created, destroyed once.
            unsafe {
                (va.destroy_buffer)(dpy, b);
            }
        }
        let data = result?;
        self.since_idr += 1;
        if idr {
            self.idr_pic_id = self.idr_pic_id.wrapping_add(1);
        }
        // The picture just written becomes the reference for the next one.
        self.recon = if self.recon == 1 { 2 } else { 1 };
        Ok((idr, data))
    }

    /// BGRA -> NV12 into the input surface.
    fn upload_frame(&mut self, bgra: &[u8]) -> Result<()> {
        let va = self.disp.va;
        let dpy = self.disp.dpy;
        let (w, h) = (self.width as usize, self.height as usize);
        // SAFETY: the mapped image is written within its reported planes
        // (offsets, pitches, data_size), then unmapped before use.
        unsafe {
            let (img, derived) = match self.upload {
                Some(img) => (img, false),
                None => {
                    let mut img = sys::VAImage::default();
                    check(
                        (va.derive_image)(dpy, self.surfaces[0], &mut img),
                        "map VA surface",
                    )?;
                    (img, true)
                }
            };
            let mut ptr = null_mut();
            let mapped = check((va.map_buffer)(dpy, img.buf, &mut ptr), "map VA image");
            if mapped.is_ok() && !ptr.is_null() {
                let (y_off, uv_off) = (img.offsets[0] as usize, img.offsets[1] as usize);
                let (y_pitch, uv_pitch) = (img.pitches[0] as usize, img.pitches[1] as usize);
                let size = img.data_size as usize;
                let fits = y_off + y_pitch * h <= size
                    && uv_off + uv_pitch * (h / 2) <= size
                    && y_pitch >= w
                    && uv_pitch >= w
                    && y_off + y_pitch * h <= uv_off;
                if fits {
                    let all = std::slice::from_raw_parts_mut(ptr.cast::<u8>(), size);
                    let (head, uv) = all.split_at_mut(uv_off);
                    let y = &mut head[y_off..];
                    // Write a local copy, then one sequential pass into
                    // (often uncached) GPU memory.
                    self.nv12.resize(w * h * 3 / 2, 0);
                    let (ly, luv) = self.nv12.split_at_mut(w * h);
                    super::nv12::bgra_to_nv12(bgra, w, h, ly, luv);
                    for (dst, src) in y.chunks_mut(y_pitch).zip(ly.chunks_exact(w)) {
                        dst[..w].copy_from_slice(src);
                    }
                    for (dst, src) in uv.chunks_mut(uv_pitch).zip(luv.chunks_exact(w)) {
                        dst[..w].copy_from_slice(src);
                    }
                }
                (va.unmap_buffer)(dpy, img.buf);
                if !fits {
                    if derived {
                        (va.destroy_image)(dpy, img.image_id);
                    }
                    bail!("VA image layout is unexpected");
                }
            }
            if derived {
                (va.destroy_image)(dpy, img.image_id);
            } else if mapped.is_ok() {
                let (aw, ah) = (self.mb_w * 16, self.mb_h * 16);
                check(
                    (va.put_image)(
                        dpy,
                        self.surfaces[0],
                        img.image_id,
                        0,
                        0,
                        aw,
                        ah,
                        0,
                        0,
                        aw,
                        ah,
                    ),
                    "upload VA image",
                )?;
            }
            mapped
        }
    }

    fn sequence_params(&self) -> sys::VAEncSequenceParameterBufferH264 {
        let mut seq = sys::VAEncSequenceParameterBufferH264 {
            seq_parameter_set_id: 0,
            level_idc: LEVEL_IDC,
            intra_period: self.gop,
            intra_idr_period: self.gop,
            ip_period: 1,
            bits_per_second: self.bitrate,
            max_num_ref_frames: 1,
            picture_width_in_mbs: self.mb_w as u16,
            picture_height_in_mbs: self.mb_h as u16,
            ..Default::default()
        };
        // SAFETY: writing bitfields of a plain-data union.
        unsafe {
            let f = &mut seq.seq_fields.bits;
            f.set_chroma_format_idc(1);
            f.set_frame_mbs_only_flag(1);
            f.set_direct_8x8_inference_flag(1);
            f.set_log2_max_frame_num_minus4(LOG2_MAX - 4);
            f.set_pic_order_cnt_type(0);
            f.set_log2_max_pic_order_cnt_lsb_minus4(LOG2_MAX - 4);
        }
        let (crop_r, crop_b) = (self.mb_w * 16 - self.width, self.mb_h * 16 - self.height);
        if crop_r > 0 || crop_b > 0 {
            // In units of two pixels for 4:2:0.
            seq.frame_cropping_flag = 1;
            seq.frame_crop_right_offset = crop_r / 2;
            seq.frame_crop_bottom_offset = crop_b / 2;
        }
        seq
    }

    /// Rate control, frame rate and HRD as misc-parameter buffers.
    fn misc_params(&self) -> Vec<Vec<u8>> {
        let mut rc = sys::VAEncMiscParameterRateControl {
            bits_per_second: self.bitrate,
            target_percentage: 100,
            window_size: 1000,
            initial_qp: 26,
            ..Default::default()
        };
        // SAFETY: writing a bitfield of a plain-data union.
        unsafe { rc.rc_flags.bits.set_reset(1) };
        let fps = sys::VAEncMiscParameterFrameRate {
            framerate: self.fps,
            ..Default::default()
        };
        let hrd = sys::VAEncMiscParameterHRD {
            buffer_size: self.bitrate / 4,
            initial_buffer_fullness: self.bitrate / 8,
            ..Default::default()
        };
        vec![
            misc(sys::VAEncMiscParameterTypeRateControl, &rc),
            misc(sys::VAEncMiscParameterTypeFrameRate, &fps),
            misc(sys::VAEncMiscParameterTypeHRD, &hrd),
        ]
    }

    fn picture_params(
        &self,
        idr: bool,
    ) -> (
        sys::VAEncPictureParameterBufferH264,
        sys::VAEncSliceParameterBufferH264,
    ) {
        let max = 1u32 << LOG2_MAX;
        let frame_num = self.since_idr % max;
        let poc = (self.since_idr * 2) % max;
        let invalid = sys::VAPictureH264 {
            picture_id: sys::VA_INVALID_SURFACE,
            flags: sys::VA_PICTURE_H264_INVALID,
            ..Default::default()
        };
        let current = sys::VAPictureH264 {
            picture_id: self.surfaces[self.recon],
            frame_idx: frame_num,
            flags: 0,
            TopFieldOrderCnt: poc as i32,
            BottomFieldOrderCnt: poc as i32,
            ..Default::default()
        };
        let prev_frame = self.since_idr.saturating_sub(1);
        let reference = sys::VAPictureH264 {
            picture_id: self.surfaces[if self.recon == 1 { 2 } else { 1 }],
            frame_idx: prev_frame % max,
            flags: sys::VA_PICTURE_H264_SHORT_TERM_REFERENCE,
            TopFieldOrderCnt: ((prev_frame * 2) % max) as i32,
            BottomFieldOrderCnt: ((prev_frame * 2) % max) as i32,
            ..Default::default()
        };
        let mut pic = sys::VAEncPictureParameterBufferH264 {
            CurrPic: current,
            ReferenceFrames: [invalid; 16],
            coded_buf: self.coded,
            frame_num: frame_num as u16,
            pic_init_qp: 26,
            ..Default::default()
        };
        if !idr {
            pic.ReferenceFrames[0] = reference;
        }
        // SAFETY: writing bitfields of a plain-data union.
        unsafe {
            let f = &mut pic.pic_fields.bits;
            f.set_idr_pic_flag(idr as u32);
            f.set_reference_pic_flag(1);
            f.set_entropy_coding_mode_flag(0);
            f.set_deblocking_filter_control_present_flag(1);
        }
        let mut slice = sys::VAEncSliceParameterBufferH264 {
            macroblock_address: 0,
            num_macroblocks: self.mb_w * self.mb_h,
            macroblock_info: sys::VA_INVALID_ID,
            slice_type: if idr { SLICE_I } else { SLICE_P },
            idr_pic_id: self.idr_pic_id,
            pic_order_cnt_lsb: poc as u16,
            RefPicList0: [invalid; 32],
            RefPicList1: [invalid; 32],
            ..Default::default()
        };
        if !idr {
            slice.RefPicList0[0] = reference;
        }
        (pic, slice)
    }

    /// A packed header: its description, then its bytes (already escaped).
    fn packed_header(
        &self,
        buffers: &mut Vec<sys::VABufferID>,
        kind: sys::VAEncPackedHeaderType,
        data: &[u8],
        bits: u32,
    ) -> Result<()> {
        let param = sys::VAEncPackedHeaderParameterBuffer {
            type_: kind,
            bit_length: bits,
            has_emulation_bytes: 1,
            ..Default::default()
        };
        buffers.push(self.buffer(sys::VAEncPackedHeaderParameterBufferType, &param)?);
        buffers.push(self.raw_buffer(sys::VAEncPackedHeaderDataBufferType, data)?);
        Ok(())
    }

    fn buffer<T>(&self, kind: sys::VABufferType, value: &T) -> Result<sys::VABufferID> {
        // SAFETY: viewing a plain-data struct as bytes for libva to copy.
        let bytes =
            unsafe { std::slice::from_raw_parts((value as *const T).cast::<u8>(), size_of::<T>()) };
        self.raw_buffer(kind, bytes)
    }

    fn raw_buffer(&self, kind: sys::VABufferType, bytes: &[u8]) -> Result<sys::VABufferID> {
        let mut id = sys::VA_INVALID_ID;
        // SAFETY: libva copies `bytes` into a new buffer on our context.
        unsafe {
            check(
                (self.disp.va.create_buffer)(
                    self.disp.dpy,
                    self.context,
                    kind,
                    bytes.len() as c_uint,
                    1,
                    bytes.as_ptr().cast_mut().cast(),
                    &mut id,
                ),
                "create VA buffer",
            )?;
        }
        Ok(id)
    }

    fn read_coded(&self) -> Result<Vec<u8>> {
        let (va, dpy) = (self.disp.va, self.disp.dpy);
        let mut out = Vec::new();
        // SAFETY: the coded buffer is mapped while its segment list is
        // walked; each segment reports its own size.
        unsafe {
            let mut seg: *mut c_void = null_mut();
            check((va.map_buffer)(dpy, self.coded, &mut seg), "map VA output")?;
            let mut overflow = false;
            while !seg.is_null() {
                let s = &*(seg as *const sys::VACodedBufferSegment);
                if s.status & sys::VA_CODED_BUF_STATUS_SLICE_OVERFLOW_MASK != 0 {
                    overflow = true;
                }
                if !s.buf.is_null() && s.size > 0 {
                    out.extend_from_slice(std::slice::from_raw_parts(
                        s.buf.cast::<u8>(),
                        s.size as usize,
                    ));
                }
                seg = s.next;
            }
            (va.unmap_buffer)(dpy, self.coded);
            if overflow {
                bail!("VA output buffer overflowed");
            }
        }
        Ok(out)
    }
}

/// A misc-parameter buffer: the type tag followed by the payload.
fn misc<T>(kind: sys::VAEncMiscParameterType, payload: &T) -> Vec<u8> {
    let mut out = kind.to_ne_bytes().to_vec();
    // SAFETY: viewing a plain-data struct as bytes.
    out.extend_from_slice(unsafe {
        std::slice::from_raw_parts((payload as *const T).cast::<u8>(), size_of::<T>())
    });
    out
}

impl Drop for VaapiEncoder {
    fn drop(&mut self) {
        let (va, dpy) = (self.disp.va, self.disp.dpy);
        // SAFETY: each handle is released once, before the display (which
        // Display's own Drop terminates afterwards).
        unsafe {
            if let Some(img) = self.upload {
                (va.destroy_image)(dpy, img.image_id);
            }
            if self.coded != sys::VA_INVALID_ID {
                (va.destroy_buffer)(dpy, self.coded);
            }
            if self.context != sys::VA_INVALID_ID {
                (va.destroy_context)(dpy, self.context);
            }
            if self.surfaces[0] != sys::VA_INVALID_SURFACE {
                (va.destroy_surfaces)(dpy, self.surfaces.as_mut_ptr(), 3);
            }
            if self.config != sys::VA_INVALID_ID {
                (va.destroy_config)(dpy, self.config);
            }
        }
    }
}

/// Whether a VA-API H.264 encoder opens on this machine (cached).
pub fn available() -> bool {
    static OK: OnceLock<bool> = OnceLock::new();
    *OK.get_or_init(|| match VaapiEncoder::new(640, 360, 30, 1_000_000) {
        Ok(_) => true,
        Err(e) => {
            tracing::debug!("VA-API encoder unavailable: {e:#}");
            false
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Runs only where a VA-API encoder exists (not in CI). Uses an odd
    /// macroblock size (1366x768 -> cropping) on purpose.
    #[test]
    fn vaapi_stream_decodes_with_openh264() {
        if !available() {
            eprintln!("no VA-API encoder; skipping");
            return;
        }
        let (w, h) = (1366u32, 768u32);
        let mut enc = VaapiEncoder::new(w, h, 30, 4_000_000).unwrap();
        let mut dec = crate::codec::VideoDecoder::new().unwrap();
        let mut pattern = crate::pattern::Pattern::new(w, h, 2);
        let (mut keys, mut last) = (Vec::new(), None);
        for i in 0..12 {
            let frame = pattern.next_frame();
            let (key, data) = enc.encode(&frame.bgra, i == 6).unwrap();
            keys.push(key);
            if let Some(out) = dec.decode(&data, (0, 0)).unwrap() {
                last = Some((frame, out));
            }
            if i == 8 {
                enc.set_bitrate(1_000_000);
            }
        }
        assert_eq!(keys.iter().filter(|k| **k).count(), 2, "{keys:?}");
        assert!(keys[0] && keys[6]);
        let (input, out) = last.expect("decoded");
        assert_eq!((out.width, out.height), (w, h));
        for y in [5, h / 2, h - 5] {
            let i = ((y * w + w / 2) * 4) as usize;
            for c in 0..3 {
                let d = (input.bgra[i + c] as i32 - out.bgra[i + c] as i32).abs();
                assert!(d < 60, "row {y} channel {c} off by {d}");
            }
        }
    }
}
