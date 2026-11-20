//! Video tiles: show a stream of decoded BGRA frames in gpui.
//!
//! Each new frame becomes a fresh `RenderImage`; the previous one is removed
//! from the GPU sprite atlas on the next paint so textures don't pile up.
//! The tile publishes its on-screen size in physical pixels through a
//! [`TargetSize`], and the producer scales frames down to it, so upload cost
//! follows the tile size rather than the stream resolution.

use std::sync::Arc;
use std::time::{Duration, Instant};

use glance_media::{TargetSize, VideoFrame};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use image::{Frame, RgbaImage};

fn to_render_image(frame: VideoFrame) -> Option<Arc<RenderImage>> {
    let buf = RgbaImage::from_raw(frame.width, frame.height, frame.bgra)?;
    Some(Arc::new(RenderImage::new([Frame::new(buf)])))
}

/// A view that shows the latest frame from a channel.
pub struct VideoView {
    current: Option<Arc<RenderImage>>,
    current_size: (u32, u32),
    stale: Vec<Arc<RenderImage>>,
    target: TargetSize,
    stats: FpsCounter,
    _feed: Task<()>,
}

impl VideoView {
    /// Show frames from `rx`. `target` is written with this tile's size so
    /// the producer can scale to it. Frames that arrive faster than we paint
    /// replace each other; only the newest is uploaded.
    pub fn new(
        rx: flume::Receiver<VideoFrame>,
        target: TargetSize,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut view = Self::empty(target);
        view.set_source(rx, cx);
        view
    }

    /// A tile with no frames yet (waiting for a stream to start).
    pub fn empty(target: TargetSize) -> Self {
        Self {
            current: None,
            current_size: (0, 0),
            stale: Vec::new(),
            target,
            stats: FpsCounter::default(),
            _feed: Task::ready(()),
        }
    }

    pub fn target(&self) -> TargetSize {
        self.target.clone()
    }

    /// Show frames from `rx`, replacing any previous source.
    pub fn set_source(&mut self, rx: flume::Receiver<VideoFrame>, cx: &mut Context<Self>) {
        self._feed = cx.spawn(async move |this, cx| {
            while let Ok(mut frame) = rx.recv_async().await {
                while let Ok(newer) = rx.try_recv() {
                    frame = newer;
                }
                let size = (frame.width, frame.height);
                let Some(image) = to_render_image(frame) else {
                    continue;
                };
                let alive = this.update(cx, |view, cx| {
                    if let Some(old) = view.current.replace(image) {
                        view.stale.push(old);
                    }
                    view.current_size = size;
                    view.stats.tick();
                    cx.notify();
                });
                if alive.is_err() {
                    break;
                }
            }
        });
    }

    pub fn fps(&self) -> f32 {
        self.stats.fps
    }

    pub fn frame_size(&self) -> (u32, u32) {
        self.current_size
    }
}

impl Render for VideoView {
    fn render(&mut self, window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        for old in self.stale.drain(..) {
            window.drop_image(old).ok();
        }
        let target = self.target.clone();
        let fps = self.stats.fps;
        let (fw, fh) = self.current_size;
        div()
            .relative()
            .size_full()
            .bg(gpui_kit::black())
            .child(
                // Measures the tile during layout and publishes its size.
                canvas(
                    move |bounds, window, _| {
                        let scale = window.scale_factor();
                        let w = (f32::from(bounds.size.width) * scale).round() as u32;
                        let h = (f32::from(bounds.size.height) * scale).round() as u32;
                        target.set(w, h);
                    },
                    |_, _, _, _| {},
                )
                .absolute()
                .size_full(),
            )
            .when(self.current.is_none(), |d| {
                d.flex().items_center().justify_center().child(
                    div()
                        .text_sm()
                        .text_color(gpui_kit::white().opacity(0.6))
                        .child("Connecting to stream…"),
                )
            })
            .children(self.current.clone().map(|image| {
                img(ImageSource::Render(image))
                    .size_full()
                    .object_fit(ObjectFit::Contain)
            }))
            .child(
                div()
                    .absolute()
                    .top_2()
                    .left_2()
                    .px_2()
                    .rounded(px(4.))
                    .bg(gpui_kit::black().opacity(0.6))
                    .text_xs()
                    .text_color(gpui_kit::white())
                    .child(format!("{fw}×{fh} · {fps:.0} fps")),
            )
    }
}

/// Counts events per second over a sliding one-second window.
#[derive(Default)]
pub struct FpsCounter {
    window_start: Option<Instant>,
    count: u32,
    pub fps: f32,
}

impl FpsCounter {
    pub fn tick(&mut self) {
        let now = Instant::now();
        let start = *self.window_start.get_or_insert(now);
        self.count += 1;
        let elapsed = now - start;
        if elapsed >= Duration::from_secs(1) {
            self.fps = self.count as f32 / elapsed.as_secs_f32();
            tracing::debug!(fps = self.fps, "video tile");
            self.count = 0;
            self.window_start = Some(now);
        }
    }
}

#[cfg(test)]
mod tests {
    // Not `super::*`: the gpui_kit glob would shadow `#[test]` with GPUI's test macro.
    use super::{VideoFrame, to_render_image};

    #[test]
    fn valid_frame_converts_and_bad_frame_is_rejected() {
        let ok = VideoFrame {
            width: 4,
            height: 2,
            bgra: vec![0; 4 * 2 * 4],
        };
        assert!(to_render_image(ok).is_some());
        let bad = VideoFrame {
            width: 10,
            height: 10,
            bgra: vec![0; 10],
        };
        assert!(to_render_image(bad).is_none());
    }
}
