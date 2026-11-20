//! Capture through zed-scap: X11, macOS (ScreenCaptureKit), Windows.

use anyhow::{Context, Result, anyhow};
use scap::capturer::{Capturer, Options};
use scap::frame::{Frame, FrameType};

use super::{Sink, Source, SourceKind, pack_bgra};

pub fn list_sources() -> Vec<Source> {
    let Ok(targets) = std::panic::catch_unwind(scap::get_all_targets) else {
        return Vec::new();
    };
    let Ok(targets) = targets else {
        return Vec::new();
    };
    targets
        .into_iter()
        .filter_map(|t| match t {
            scap::Target::Display(d) => Some(Source {
                id: d.id,
                name: if d.title.is_empty() {
                    format!("Screen {}", d.id)
                } else {
                    d.title.clone()
                },
                kind: SourceKind::Screen,
                size: display_size(&d),
            }),
            scap::Target::Window(w) if !w.title.trim().is_empty() => Some(Source {
                id: w.id,
                name: w.title.clone(),
                kind: SourceKind::Window,
                size: None,
            }),
            scap::Target::Window(_) => None,
        })
        .collect()
}

#[cfg(any(target_os = "linux", target_os = "windows"))]
fn display_size(d: &scap::Display) -> Option<(u32, u32)> {
    Some((d.width as u32, d.height as u32))
}

#[cfg(target_os = "macos")]
fn display_size(_d: &scap::Display) -> Option<(u32, u32)> {
    None
}

fn find_target(src: &Source) -> Option<scap::Target> {
    scap::get_all_targets()
        .ok()?
        .into_iter()
        .find(|t| match (t, src.kind) {
            (scap::Target::Display(d), SourceKind::Screen) => d.id == src.id,
            (scap::Target::Window(w), SourceKind::Window) => w.id == src.id,
            _ => false,
        })
}

pub fn start(sink: Sink, src: Source, fps: u32) -> Result<()> {
    let target =
        find_target(&src).ok_or_else(|| anyhow!("'{}' is no longer available", src.name))?;
    let options = Options {
        fps: fps.max(1),
        show_cursor: true,
        show_highlight: false,
        target: Some(target),
        output_type: FrameType::BGRAFrame,
        ..Default::default()
    };
    // The capturer isn't Send on every platform: build it on its thread and
    // report a build error back before returning.
    let (ready_tx, ready_rx) = flume::bounded::<Result<()>>(1);
    std::thread::Builder::new()
        .name("capture".into())
        .spawn(move || {
            let mut capturer = match Capturer::build(options) {
                Ok(c) => {
                    let _ = ready_tx.send(Ok(()));
                    c
                }
                Err(e) => {
                    let _ = ready_tx.send(Err(anyhow!("{e:?}")));
                    return;
                }
            };
            capturer.start_capture();
            while !sink.stopped() {
                match capturer.get_next_frame() {
                    Ok(frame) => {
                        if let Some(f) = convert(frame) {
                            sink.offer(f);
                        }
                    }
                    Err(e) => {
                        sink.end(format!("capture stopped: {e:#}"));
                        break;
                    }
                }
            }
            capturer.stop_capture();
        })?;
    ready_rx
        .recv()
        .map_err(|_| anyhow!("capture thread exited"))?
        .context("start capture")?;
    Ok(())
}

fn convert(frame: Frame) -> Option<crate::frame::VideoFrame> {
    // Rows may be padded; derive the stride from the buffer length.
    let pack = |data: &[u8], w: i32, h: i32, order| {
        let (w, h) = (w.max(0) as usize, h.max(0) as usize);
        let stride = data.len().checked_div(h).unwrap_or(0);
        pack_bgra(data, w, h, stride, order)
    };
    match frame {
        Frame::BGRA(f) => pack(&f.data, f.width, f.height, [0, 1, 2]),
        Frame::BGRx(f) => pack(&f.data, f.width, f.height, [0, 1, 2]),
        Frame::BGR0(f) => pack(&f.data, f.width, f.height, [0, 1, 2]),
        Frame::RGBx(f) => pack(&f.data, f.width, f.height, [2, 1, 0]),
        Frame::XBGR(f) => pack(&f.data, f.width, f.height, [1, 2, 3]),
        Frame::RGB(f) => {
            let (w, h) = (f.width.max(0) as usize, f.height.max(0) as usize);
            let mut bgra = Vec::with_capacity(w * h * 4);
            for px in f.data.as_chunks::<3>().0.iter().take(w * h) {
                bgra.extend_from_slice(&[px[2], px[1], px[0], 255]);
            }
            (bgra.len() == w * h * 4).then_some(crate::frame::VideoFrame {
                width: w as u32,
                height: h as u32,
                bgra,
            })
        }
        Frame::YUVFrame(_) => None,
    }
}
