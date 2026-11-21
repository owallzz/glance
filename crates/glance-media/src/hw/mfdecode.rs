//! H.264 decoding through Media Foundation (Windows): Microsoft's decoder
//! MFT, on the GPU (DXVA through a D3D11 device) when the machine has one,
//! otherwise in software on several threads. Frames come back as NV12.

use std::mem::ManuallyDrop;

use anyhow::{Context, Result, bail};
use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
use windows::Win32::Graphics::Direct3D11::{
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_CREATE_DEVICE_VIDEO_SUPPORT, D3D11_SDK_VERSION,
    D3D11CreateDevice, ID3D11Device, ID3D11Multithread,
};
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx};
use windows::core::Interface;

use super::Nv12Frame;
use super::mediafoundation::{Mf, bool_variant, mf, set_codec};

pub struct MfDecoder {
    transform: IMFTransform,
    /// Kept alive while the decoder uses the GPU.
    _gpu: Option<(ID3D11Device, IMFDXGIDeviceManager)>,
    hardware: bool,
    output_info: MFT_OUTPUT_STREAM_INFO,
    /// The NV12 layout: allocated size (planes are this tall) and the
    /// visible part.
    coded: (u32, u32),
    visible: (u32, u32),
    stride: u32,
    newest: Option<Nv12Frame>,
    failed: Option<String>,
}

// The decoder MFT lives in the multithreaded apartment.
unsafe impl Send for MfDecoder {}

impl MfDecoder {
    /// Microsoft's H.264 decoder; `gpu` asks for DXVA decoding (it quietly
    /// stays in software if the GPU can't).
    pub fn new(gpu: bool) -> Result<Self> {
        let mf = mf()?;
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        }
        let activate = mf
            .transforms(
                MFT_CATEGORY_VIDEO_DECODER,
                MFVideoFormat_H264,
                MFVideoFormat_NV12,
                MFT_ENUM_FLAG_SYNCMFT,
            )?
            .into_iter()
            .next()
            .context("no H.264 Media Foundation decoder")?;
        let transform: IMFTransform = unsafe { activate.ActivateObject() }.context("activate")?;
        let attrs = unsafe { transform.GetAttributes() }.ok();
        if let Some(a) = &attrs {
            let _ = unsafe { a.SetUINT32(&MF_LOW_LATENCY, 1) };
        }
        if let Ok(c) = transform.cast::<ICodecAPI>() {
            set_codec(&c, &CODECAPI_AVLowLatencyMode, bool_variant());
        }
        let d3d_aware = attrs
            .as_ref()
            .is_some_and(|a| unsafe { a.GetUINT32(&MF_SA_D3D11_AWARE) }.unwrap_or(0) != 0);
        let gpu = if gpu && d3d_aware {
            match attach_gpu(mf, &transform) {
                Ok(g) => Some(g),
                Err(e) => {
                    tracing::debug!("Media Foundation decoder stays in software: {e:#}");
                    None
                }
            }
        } else {
            None
        };
        unsafe {
            let input = mf.media_type()?;
            input.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            input.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)?;
            transform
                .SetInputType(0, &input, 0)
                .context("set H.264 input type")?;
        }
        let mut dec = Self {
            transform,
            hardware: gpu.is_some(),
            _gpu: gpu,
            output_info: MFT_OUTPUT_STREAM_INFO::default(),
            coded: (0, 0),
            visible: (0, 0),
            stride: 0,
            newest: None,
            failed: None,
        };
        dec.choose_output()?;
        unsafe {
            dec.transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)?;
            dec.transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)?;
        }
        Ok(dec)
    }

    pub fn is_hardware(&self) -> bool {
        self.hardware
    }

    /// Make the next decode fail, as a broken decoder would (tests).
    #[cfg(test)]
    pub(crate) fn fail_next(&mut self) {
        self.failed = Some("injected failure".into());
    }

    /// Decode one packet (a whole picture, Annex-B); returns the newest
    /// picture that came out.
    pub fn decode(&mut self, data: &[u8]) -> Result<Option<Nv12Frame>> {
        if let Some(e) = self.failed.take() {
            bail!("{e}");
        }
        let mf = mf()?;
        let sample = unsafe {
            let buf = mf.buffer(data.len() as u32)?;
            let mut ptr = std::ptr::null_mut();
            buf.Lock(&mut ptr, None, None)?;
            std::ptr::copy_nonoverlapping(data.as_ptr(), ptr, data.len());
            buf.Unlock()?;
            buf.SetCurrentLength(data.len() as u32)?;
            let sample = mf.sample()?;
            sample.AddBuffer(&buf)?;
            sample
        };
        for attempt in 0..2 {
            match unsafe { self.transform.ProcessInput(0, &sample, 0) } {
                Ok(()) => break,
                Err(e) if e.code() == MF_E_NOTACCEPTING && attempt == 0 => self.drain(mf)?,
                Err(e) => return Err(e).context("ProcessInput"),
            }
        }
        self.drain(mf)?;
        Ok(self.newest.take())
    }

    /// Take every picture the decoder has ready.
    fn drain(&mut self, mf: &Mf) -> Result<()> {
        loop {
            let provides = self.output_info.dwFlags
                & (MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 | MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES.0)
                    as u32
                != 0;
            let sample = if provides {
                None
            } else {
                let s = mf.sample()?;
                let len = self.output_info.cbSize.max(1);
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
                Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => return Ok(()),
                Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                    // The decoder read the SPS: pick NV12 at the real size.
                    self.choose_output()?;
                    continue;
                }
                Err(e) => return Err(e).context("ProcessOutput"),
            }
            if let Some(sample) = sample {
                self.newest = Some(self.copy_out(&sample)?);
            }
        }
    }

    /// Select NV12 output and note its layout.
    fn choose_output(&mut self) -> Result<()> {
        unsafe {
            let mut i = 0;
            let ty = loop {
                let ty = self
                    .transform
                    .GetOutputAvailableType(0, i)
                    .context("decoder offers no NV12 output")?;
                if ty.GetGUID(&MF_MT_SUBTYPE)? == MFVideoFormat_NV12 {
                    break ty;
                }
                i += 1;
            };
            self.transform
                .SetOutputType(0, &ty, 0)
                .context("set NV12 output type")?;
            self.output_info = self.transform.GetOutputStreamInfo(0)?;
            let size = ty.GetUINT64(&MF_MT_FRAME_SIZE).unwrap_or(0);
            self.coded = ((size >> 32) as u32, size as u32);
            self.visible = self.coded;
            let mut area = MFVideoArea::default();
            let bytes = std::slice::from_raw_parts_mut(
                (&mut area as *mut MFVideoArea).cast::<u8>(),
                std::mem::size_of::<MFVideoArea>(),
            );
            if ty
                .GetBlob(&MF_MT_MINIMUM_DISPLAY_APERTURE, bytes, None)
                .is_ok()
                && area.Area.cx > 0
                && area.Area.cy > 0
            {
                self.visible = (area.Area.cx as u32, area.Area.cy as u32);
            }
            self.stride = ty
                .GetUINT32(&MF_MT_DEFAULT_STRIDE)
                .unwrap_or(self.coded.0)
                .max(self.coded.0);
        }
        Ok(())
    }

    /// Copy the visible part of an NV12 sample into tight planes.
    fn copy_out(&self, sample: &IMFSample) -> Result<Nv12Frame> {
        let (w, h) = (
            (self.visible.0 & !1) as usize,
            (self.visible.1 & !1) as usize,
        );
        if w == 0 || h == 0 {
            bail!("decoder output has no size");
        }
        unsafe {
            let buf = sample.GetBufferByIndex(0)?;
            // GPU (DXGI) buffers and padded system buffers have a pitch;
            // IMF2DBuffer reports it.
            let two_d = buf.cast::<IMF2DBuffer>().ok();
            let (base, pitch, locked_2d) = match &two_d {
                Some(b2) => {
                    let (mut p, mut pitch) = (std::ptr::null_mut(), 0i32);
                    b2.Lock2D(&mut p, &mut pitch)?;
                    (p as *const u8, pitch as usize, true)
                }
                None => {
                    let mut p = std::ptr::null_mut();
                    buf.Lock(&mut p, None, None)?;
                    (p as *const u8, self.stride as usize, false)
                }
            };
            let rows = (self.coded.1 as usize).max(h);
            let mut y = Vec::with_capacity(w * h);
            let mut uv = Vec::with_capacity(w * h / 2);
            for r in 0..h {
                y.extend_from_slice(std::slice::from_raw_parts(base.add(r * pitch), w));
            }
            let chroma = base.add(rows * pitch);
            for r in 0..h / 2 {
                uv.extend_from_slice(std::slice::from_raw_parts(chroma.add(r * pitch), w));
            }
            if locked_2d {
                two_d.as_ref().unwrap().Unlock2D()?;
            } else {
                buf.Unlock()?;
            }
            Ok(Nv12Frame {
                width: w as u32,
                height: h as u32,
                y,
                uv,
            })
        }
    }
}

impl Drop for MfDecoder {
    fn drop(&mut self) {
        unsafe {
            let _ = self
                .transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0);
            let _ = self
                .transform
                .ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, 0);
        }
    }
}

/// Give the decoder a D3D11 device so it decodes with DXVA.
fn attach_gpu(mf: &Mf, transform: &IMFTransform) -> Result<(ID3D11Device, IMFDXGIDeviceManager)> {
    let mut device = None;
    unsafe {
        D3D11CreateDevice(
            None,
            D3D_DRIVER_TYPE_HARDWARE,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_VIDEO_SUPPORT | D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            None,
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            None,
        )
        .context("D3D11CreateDevice")?;
    }
    let device = device.context("no D3D11 device")?;
    unsafe {
        // The decoder uses the device from its own threads.
        if let Ok(mt) = device.cast::<ID3D11Multithread>() {
            let _ = mt.SetMultithreadProtected(true);
        }
    }
    let (manager, token) = mf.dxgi_manager()?;
    unsafe {
        manager.ResetDevice(&device, token)?;
        transform
            .ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, manager.as_raw() as usize)
            .context("decoder refused the D3D11 device")?;
    }
    Ok((device, manager))
}

/// Whether Microsoft's decoder decodes on the GPU here (cached).
pub fn hardware_available() -> bool {
    static OK: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *OK.get_or_init(|| match MfDecoder::new(true) {
        Ok(d) => d.is_hardware(),
        Err(e) => {
            tracing::debug!("Media Foundation decoder unavailable: {e:#}");
            false
        }
    })
}
