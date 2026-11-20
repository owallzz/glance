//! `glance --spike [tiles] [width] [height] [fps]`: render N test-pattern
//! tiles to measure the video path (M1). Prints painted fps per tile to stderr.

use gpui_kit::*;

use glance_media::TargetSize;
use glance_media::pattern::spawn_pattern;

use crate::video::VideoView;

pub struct Spike {
    tiles: Vec<Entity<VideoView>>,
    _report: Task<()>,
}

impl Spike {
    pub fn new(n: usize, width: u32, height: u32, fps: u32, cx: &mut Context<Self>) -> Self {
        let tiles: Vec<_> = (0..n)
            .map(|i| {
                let target = TargetSize::default();
                let rx = spawn_pattern(width, height, fps, (i * 40) as u8, target.clone());
                cx.new(|cx| VideoView::new(rx, target, cx))
            })
            .collect();
        let report = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(std::time::Duration::from_secs(2))
                    .await;
                let Ok(line) = this.update(cx, |spike, cx| {
                    spike
                        .tiles
                        .iter()
                        .map(|t| {
                            let v = t.read(cx);
                            let (w, h) = v.frame_size();
                            format!("{w}x{h}@{:.1}", v.fps())
                        })
                        .collect::<Vec<_>>()
                        .join(" ")
                }) else {
                    break;
                };
                eprintln!("spike fps: {line}");
            }
        });
        Self {
            tiles,
            _report: report,
        }
    }
}

impl Render for Spike {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        let cols = (self.tiles.len() as f32).sqrt().ceil().max(1.) as usize;
        let rows = self.tiles.len().div_ceil(cols).max(1);
        div()
            .size_full()
            .bg(gpui_kit::black())
            .flex()
            .flex_col()
            .gap_1()
            .children(self.tiles.chunks(cols).map(|row| {
                div()
                    .flex()
                    .flex_1()
                    .gap_1()
                    .h(relative(1.0 / rows as f32))
                    .children(row.iter().map(|t| div().flex_1().h_full().child(t.clone())))
            }))
    }
}
