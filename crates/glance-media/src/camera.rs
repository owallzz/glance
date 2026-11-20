//! Webcam capture via nokhwa (V4L2 on Linux, AVFoundation on macOS, Media
//! Foundation on Windows). Frames come out as BGRA like screen capture, so
//! the same broadcast pipeline encodes and sends them.

use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use anyhow::{Result, anyhow};
use nokhwa::pixel_format::RgbFormat;
use nokhwa::utils::{
    ApiBackend, CameraFormat, CameraIndex, FrameFormat, RequestedFormat, RequestedFormatType,
    Resolution,
};

use crate::capture::{Capture, Sink};
use crate::frame::VideoFrame;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CameraInfo {
    pub index: u32,
    pub name: String,
}

/// Cameras on this machine (listing doesn't turn any camera on).
pub fn list_cameras() -> Vec<CameraInfo> {
    let Ok(cams) = std::panic::catch_unwind(|| nokhwa::query(ApiBackend::Auto)) else {
        return Vec::new();
    };
    cams.unwrap_or_default()
        .into_iter()
        .enumerate()
        .map(|(i, c)| CameraInfo {
            index: match c.index() {
                CameraIndex::Index(n) => *n,
                CameraIndex::String(_) => i as u32,
            },
            name: c.human_name(),
        })
        .collect()
}

/// Convert packed RGB to BGRA.
pub fn rgb_to_bgra(rgb: &[u8], width: u32, height: u32) -> Option<VideoFrame> {
    let n = width as usize * height as usize;
    if rgb.len() < n * 3 {
        return None;
    }
    let mut bgra = Vec::with_capacity(n * 4);
    for px in rgb.as_chunks::<3>().0.iter().take(n) {
        bgra.extend_from_slice(&[px[2], px[1], px[0], 255]);
    }
    Some(VideoFrame {
        width,
        height,
        bgra,
    })
}

/// Start a camera (`None` = the first one) at about `width`×`height`@`fps`.
pub fn start(index: Option<u32>, width: u32, height: u32, fps: u32) -> Result<Capture> {
    let (frames_tx, frames_rx) = flume::bounded(2);
    let (ended_tx, ended_rx) = flume::bounded(1);
    let stop = Arc::new(AtomicBool::new(false));
    let sink = Sink {
        frames: frames_tx,
        stop: stop.clone(),
        ended: ended_tx,
    };
    let (ready_tx, ready_rx) = flume::bounded::<Result<()>>(1);
    // nokhwa cameras aren't Send: open and read on one thread.
    std::thread::Builder::new()
        .name("camera".into())
        .spawn(move || {
            let requested = RequestedFormat::new::<RgbFormat>(RequestedFormatType::Closest(
                CameraFormat::new(Resolution::new(width, height), FrameFormat::MJPEG, fps),
            ));
            let mut camera =
                match nokhwa::Camera::new(CameraIndex::Index(index.unwrap_or(0)), requested)
                    .and_then(|mut c| c.open_stream().map(|_| c))
                {
                    Ok(c) => {
                        let _ = ready_tx.send(Ok(()));
                        c
                    }
                    Err(e) => {
                        let _ = ready_tx.send(Err(anyhow!("couldn't open the camera: {e}")));
                        return;
                    }
                };
            while !sink.stopped() {
                match camera.frame() {
                    Ok(buf) => {
                        let res = buf.resolution();
                        if let Ok(img) = buf.decode_image::<RgbFormat>()
                            && let Some(frame) =
                                rgb_to_bgra(img.as_raw(), res.width(), res.height())
                        {
                            sink.offer(frame);
                        }
                    }
                    Err(e) => {
                        sink.end(format!("camera stopped: {e}"));
                        break;
                    }
                }
            }
            let _ = camera.stop_stream();
        })?;
    ready_rx
        .recv_timeout(std::time::Duration::from_secs(10))
        .map_err(|_| anyhow!("the camera didn't start"))??;
    Ok(Capture {
        frames: frames_rx,
        stop,
        ended: ended_rx,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rgb_converts_to_bgra() {
        let f = rgb_to_bgra(&[1, 2, 3, 4, 5, 6], 2, 1).unwrap();
        assert_eq!(f.bgra, vec![3, 2, 1, 255, 6, 5, 4, 255]);
        assert!(rgb_to_bgra(&[1, 2], 2, 1).is_none());
    }
}
