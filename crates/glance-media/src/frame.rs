//! Decoded video frames and scaling them to the size they are shown at.
//!
//! Uploading a frame to the GPU costs memory bandwidth in proportion to its
//! pixel count, so frames are scaled down to the on-screen size of the tile
//! that shows them before they reach the UI. A 4K stream in a 640px tile then
//! costs what a 640px stream costs; fullscreen on a 4K display still gets
//! every pixel.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use fast_image_resize::images::{Image, ImageRef};
use fast_image_resize::{FilterType, PixelType, ResizeAlg, ResizeOptions, Resizer};

/// One frame, tightly packed BGRA (`width * 4` bytes per row).
#[derive(Clone)]
pub struct VideoFrame {
    pub width: u32,
    pub height: u32,
    pub bgra: Vec<u8>,
}

impl VideoFrame {
    pub fn is_valid(&self) -> bool {
        self.bgra.len() == self.width as usize * self.height as usize * 4
    }
}

/// The largest size, preserving `src`'s aspect ratio, that fits in `bounds`.
/// Never upscales: if `src` already fits, it is returned unchanged.
pub fn fit_within(src: (u32, u32), bounds: (u32, u32)) -> (u32, u32) {
    let (sw, sh) = src;
    let (bw, bh) = bounds;
    if sw == 0 || sh == 0 || bw == 0 || bh == 0 {
        return (0, 0);
    }
    if sw <= bw && sh <= bh {
        return src;
    }
    let scale = (bw as f64 / sw as f64).min(bh as f64 / sh as f64);
    let w = ((sw as f64 * scale).round() as u32).clamp(1, bw);
    let h = ((sh as f64 * scale).round() as u32).clamp(1, bh);
    (w, h)
}

/// The on-screen size (physical pixels) a tile currently shows its video at.
/// The UI writes it during layout; the decode thread reads it per frame.
/// Zero means "unknown / not visible".
#[derive(Clone, Default)]
pub struct TargetSize(Arc<AtomicU64>);

impl TargetSize {
    pub fn set(&self, width: u32, height: u32) {
        self.0
            .store(((width as u64) << 32) | height as u64, Ordering::Relaxed);
    }

    pub fn get(&self) -> (u32, u32) {
        let v = self.0.load(Ordering::Relaxed);
        ((v >> 32) as u32, v as u32)
    }
}

/// Scales frames down to a [`TargetSize`] with SIMD bilinear filtering.
pub struct FrameScaler {
    resizer: Resizer,
    options: ResizeOptions,
}

impl Default for FrameScaler {
    fn default() -> Self {
        Self {
            resizer: Resizer::new(),
            options: ResizeOptions::new().resize_alg(ResizeAlg::Convolution(FilterType::Bilinear)),
        }
    }
}

impl FrameScaler {
    /// Return `frame` scaled to fit `target`, or unchanged when it already
    /// fits or the target is unknown. Channel order is preserved, so BGRA in
    /// gives BGRA out.
    pub fn fit(&mut self, frame: VideoFrame, target: (u32, u32)) -> VideoFrame {
        if target.0 == 0 || target.1 == 0 {
            return frame;
        }
        let (w, h) = fit_within((frame.width, frame.height), target);
        if (w, h) == (frame.width, frame.height) || w == 0 {
            return frame;
        }
        let Ok(src) = ImageRef::new(frame.width, frame.height, &frame.bgra, PixelType::U8x4) else {
            return frame;
        };
        let mut dst = Image::new(w, h, PixelType::U8x4);
        if self.resizer.resize(&src, &mut dst, &self.options).is_err() {
            return frame;
        }
        VideoFrame {
            width: w,
            height: h,
            bgra: dst.into_vec(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fit_keeps_aspect_and_never_upscales() {
        assert_eq!(fit_within((3840, 2160), (1280, 1280)), (1280, 720));
        assert_eq!(fit_within((3840, 2160), (640, 720)), (640, 360));
        assert_eq!(fit_within((1280, 720), (3840, 2160)), (1280, 720));
        assert_eq!(fit_within((5120, 1440), (1920, 1080)), (1920, 540));
        assert_eq!(fit_within((100, 100), (0, 50)), (0, 0));
    }

    #[test]
    fn target_size_round_trips() {
        let t = TargetSize::default();
        assert_eq!(t.get(), (0, 0));
        t.set(7680, 4320);
        assert_eq!(t.get(), (7680, 4320));
    }

    #[test]
    fn scaler_downscales_and_passes_through() {
        let mut s = FrameScaler::default();
        let f = VideoFrame {
            width: 400,
            height: 200,
            bgra: vec![128; 400 * 200 * 4],
        };
        let out = s.fit(f.clone(), (100, 100));
        assert_eq!((out.width, out.height), (100, 50));
        assert!(out.is_valid());
        assert_eq!(out.bgra[0], 128);
        let same = s.fit(f, (0, 0));
        assert_eq!((same.width, same.height), (400, 200));
    }
}
