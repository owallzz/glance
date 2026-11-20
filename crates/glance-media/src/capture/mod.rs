//! Screen capture.
//!
//! - Linux on Wayland: the xdg-desktop-portal ScreenCast dialog (the OS asks
//!   which screen or window to share) and a PipeWire stream (`wayland.rs`).
//! - Linux on X11, macOS, Windows: zed-scap (`scap_backend.rs`), with our own
//!   source list.
//!
//! Every backend delivers tightly packed BGRA [`VideoFrame`]s on a bounded
//! channel; slow consumers just miss frames.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::Result;

use crate::frame::VideoFrame;

#[cfg(target_os = "linux")]
mod wayland;

#[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
mod scap_backend;

/// A screen or window the user can pick in our own picker.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Source {
    pub id: u32,
    pub name: String,
    pub kind: SourceKind,
    /// Pixel size when the platform knows it.
    pub size: Option<(u32, u32)>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SourceKind {
    Screen,
    Window,
}

/// What to capture.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Selection {
    /// Let the OS picker choose (Wayland portal).
    SystemPicker,
    /// A source from [`list_sources`].
    Source(Source),
}

/// Whether the OS shows its own picker (so we shouldn't list sources).
pub fn uses_system_picker() -> bool {
    #[cfg(target_os = "linux")]
    {
        wayland::is_wayland_session()
    }
    #[cfg(not(target_os = "linux"))]
    {
        false
    }
}

/// Screens and windows for our picker. Empty when [`uses_system_picker`].
pub fn list_sources() -> Vec<Source> {
    if uses_system_picker() {
        return Vec::new();
    }
    #[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
    {
        scap_backend::list_sources()
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    {
        Vec::new()
    }
}

/// macOS needs the Screen Recording permission; elsewhere this is `true`.
pub fn has_permission() -> bool {
    #[cfg(target_os = "macos")]
    {
        scap::has_permission()
    }
    #[cfg(not(target_os = "macos"))]
    {
        true
    }
}

/// Ask the OS for capture permission (macOS opens the prompt).
pub fn request_permission() -> bool {
    #[cfg(target_os = "macos")]
    {
        scap::request_permission()
    }
    #[cfg(not(target_os = "macos"))]
    {
        true
    }
}

/// A running capture. Dropping it (or calling [`Capture::stop`]) ends it.
pub struct Capture {
    pub frames: flume::Receiver<VideoFrame>,
    pub(crate) stop: Arc<AtomicBool>,
    /// What the user picked, once known (the portal reports it after the dialog).
    pub ended: flume::Receiver<String>,
}

impl Capture {
    pub fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }

    pub fn is_stopped(&self) -> bool {
        self.stop.load(Ordering::Relaxed)
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Shared plumbing handed to a backend thread.
pub(crate) struct Sink {
    pub frames: flume::Sender<VideoFrame>,
    pub stop: Arc<AtomicBool>,
    pub ended: flume::Sender<String>,
}

impl Sink {
    pub fn stopped(&self) -> bool {
        self.stop.load(Ordering::Relaxed)
    }

    /// Offer a frame; drop it if the consumer is behind.
    pub fn offer(&self, frame: VideoFrame) {
        let _ = self.frames.try_send(frame);
    }

    /// Report why capture ended (user stopped it in the OS, error...).
    pub fn end(&self, reason: impl Into<String>) {
        self.stop.store(true, Ordering::Relaxed);
        let _ = self.ended.try_send(reason.into());
    }
}

/// Start capturing at up to `fps` frames per second.
pub fn start(selection: Selection, fps: u32) -> Result<Capture> {
    let (frames_tx, frames_rx) = flume::bounded(2);
    let (ended_tx, ended_rx) = flume::bounded(1);
    let stop = Arc::new(AtomicBool::new(false));
    let sink = Sink {
        frames: frames_tx,
        stop: stop.clone(),
        ended: ended_tx,
    };
    match selection {
        #[cfg(target_os = "linux")]
        Selection::SystemPicker => wayland::start(sink, fps)?,
        #[cfg(not(target_os = "linux"))]
        Selection::SystemPicker => anyhow::bail!("this platform has no system capture picker"),
        #[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
        Selection::Source(src) => scap_backend::start(sink, src, fps)?,
        #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
        Selection::Source(_) => anyhow::bail!("screen capture isn't supported on this platform"),
    }
    Ok(Capture {
        frames: frames_rx,
        stop,
        ended: ended_rx,
    })
}

/// A synthetic capture (moving test pattern) for bots and tests.
pub fn test_pattern(width: u32, height: u32, fps: u32) -> Capture {
    let (frames_tx, frames_rx) = flume::bounded(2);
    let (_ended_tx, ended_rx) = flume::bounded(1);
    let stop = Arc::new(AtomicBool::new(false));
    let stop_thread = stop.clone();
    std::thread::Builder::new()
        .name("capture-pattern".into())
        .spawn(move || {
            let mut pattern = crate::pattern::Pattern::new(width, height, 90);
            let period = std::time::Duration::from_secs_f64(1.0 / fps.max(1) as f64);
            let mut next = std::time::Instant::now();
            while !stop_thread.load(Ordering::Relaxed) {
                let _ = frames_tx.try_send(pattern.next_frame());
                next += period;
                match next.checked_duration_since(std::time::Instant::now()) {
                    Some(wait) => std::thread::sleep(wait),
                    None => next = std::time::Instant::now(),
                }
            }
        })
        .expect("spawn pattern capture");
    Capture {
        frames: frames_rx,
        stop,
        ended: ended_rx,
    }
}

/// Pack rows of 4-byte pixels into a tight BGRA buffer. `order` maps output
/// B,G,R to source byte indices; alpha is forced opaque.
pub(crate) fn pack_bgra(
    src: &[u8],
    width: usize,
    height: usize,
    stride: usize,
    order: [usize; 3],
) -> Option<VideoFrame> {
    if width == 0
        || height == 0
        || stride < width * 4
        || src.len() < stride * (height - 1) + width * 4
    {
        return None;
    }
    let mut out = vec![0u8; width * height * 4];
    for y in 0..height {
        let row = &src[y * stride..y * stride + width * 4];
        let dst = &mut out[y * width * 4..(y + 1) * width * 4];
        if order == [0, 1, 2] {
            dst.copy_from_slice(row);
            for px in dst.as_chunks_mut::<4>().0 {
                px[3] = 255;
            }
        } else {
            for (d, s) in dst
                .as_chunks_mut::<4>()
                .0
                .iter_mut()
                .zip(row.as_chunks::<4>().0)
            {
                *d = [s[order[0]], s[order[1]], s[order[2]], 255];
            }
        }
    }
    Some(VideoFrame {
        width: width as u32,
        height: height as u32,
        bgra: out,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pack_handles_stride_and_channel_order() {
        // 2x2 RGBx with 12-byte stride (4 bytes padding per row).
        let src = [
            1, 2, 3, 0, 4, 5, 6, 0, 9, 9, 9, 9, //
            7, 8, 9, 0, 10, 11, 12, 0, 9, 9, 9, 9,
        ];
        let f = pack_bgra(&src, 2, 2, 12, [2, 1, 0]).unwrap();
        assert_eq!(
            f.bgra,
            vec![3, 2, 1, 255, 6, 5, 4, 255, 9, 8, 7, 255, 12, 11, 10, 255]
        );
        let same = pack_bgra(&src, 2, 2, 12, [0, 1, 2]).unwrap();
        assert_eq!(&same.bgra[..4], &[1, 2, 3, 255]);
        assert!(
            pack_bgra(&src, 4, 2, 12, [0, 1, 2]).is_none(),
            "width beyond stride"
        );
        assert!(
            pack_bgra(&src, 2, 3, 12, [0, 1, 2]).is_none(),
            "too few rows"
        );
    }
}
