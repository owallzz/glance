//! Images in chat: thumbnails for inline previews and the full-window
//! lightbox. Images are decoded (and scaled down) on a background thread
//! once per path and size, then kept as `RenderImage`s; nothing is decoded
//! while painting.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use gpui_kit::assets::IconName;
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::*;
use image::metadata::Orientation;
use image::{DynamicImage, Frame, ImageDecoder, ImageReader, Limits};

use crate::app::GlanceApp;
use crate::theme::Palette;
use crate::ui::icon;

/// Largest inline preview, in logical px (aspect ratio kept).
pub const THUMB_MAX_W: f32 = 400.;
pub const THUMB_MAX_H: f32 = 300.;

/// How big thumbnails are decoded, in physical px (2× the preview, for
/// high-DPI screens).
const THUMB_DECODE: (u32, u32) = (800, 600);
/// How big the lightbox copy is decoded, in physical px.
const LARGE_DECODE: (u32, u32) = (2560, 1600);
/// Decoded thumbnails kept; beyond that, the least recently shown ones are
/// dropped (and decoded again if they scroll back into view). Ones on screen
/// are never dropped, so this is a soft limit.
const KEEP: usize = 64;
/// Thumbnail decodes running at once (each can briefly need ~100 MB).
const MAX_DECODES: usize = 2;
/// Largest image we decode at all, per side, and the most memory the
/// decoder may use. Images come from peers: a tiny compressed file can
/// claim huge dimensions.
const MAX_SIDE: u32 = 12_000;
const MAX_ALLOC: u64 = 192 << 20;

/// The thumbnails to drop so at most `keep` remain: the least recently used
/// first, never one used in `frame` (the render that just happened).
fn evictions(last_used: &HashMap<PathBuf, u64>, frame: u64, keep: usize) -> Vec<PathBuf> {
    let excess = last_used.len().saturating_sub(keep);
    if excess == 0 {
        return Vec::new();
    }
    let mut old: Vec<(&PathBuf, u64)> = last_used
        .iter()
        .filter(|(_, at)| **at < frame)
        .map(|(p, at)| (p, *at))
        .collect();
    old.sort_by_key(|(_, at)| *at);
    old.into_iter()
        .take(excess)
        .map(|(p, _)| p.clone())
        .collect()
}

/// `width`×`height` scaled down (never up) to fit in `max_w`×`max_h`,
/// keeping the aspect ratio.
pub fn fit_within(width: f32, height: f32, max_w: f32, max_h: f32) -> (f32, f32) {
    if width <= 0. || height <= 0. || max_w <= 0. || max_h <= 0. {
        return (0., 0.);
    }
    let scale = (max_w / width).min(max_h / height).min(1.);
    (
        (width * scale).round().max(1.),
        (height * scale).round().max(1.),
    )
}

/// A decoded image and the size of the original (after EXIF rotation).
#[derive(Clone)]
pub struct Decoded {
    pub image: Arc<RenderImage>,
    pub width: u32,
    pub height: u32,
}

enum Slot {
    Loading,
    Ready(Decoded),
    Failed,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum Size {
    Thumb,
    Large,
}

/// Decoded images by path and size.
#[derive(Default)]
pub struct ImageCache {
    slots: HashMap<(PathBuf, Size), Slot>,
    /// Ready thumbnails and the render they were last shown in.
    last_used: HashMap<PathBuf, u64>,
    /// Renders so far.
    frame: u64,
    /// Thumbnail decodes running.
    decoding: usize,
    /// Images no longer shown, removed from the GPU atlas on the next paint.
    stale: Vec<Arc<RenderImage>>,
}

/// The lightbox: which image is open.
pub struct Lightbox {
    pub path: PathBuf,
    pub name: String,
}

/// Decode `path` and scale it to fit `max` (physical px), as BGRA.
fn decode(path: &Path, max: (u32, u32)) -> anyhow::Result<Decoded> {
    let mut reader = ImageReader::open(path)?.with_guessed_format()?;
    let mut limits = Limits::default();
    limits.max_image_width = Some(MAX_SIDE);
    limits.max_image_height = Some(MAX_SIDE);
    limits.max_alloc = Some(MAX_ALLOC);
    reader.limits(limits);
    let mut decoder = reader.into_decoder()?;
    let (w, h) = decoder.dimensions();
    anyhow::ensure!(w <= MAX_SIDE && h <= MAX_SIDE, "image too large ({w}x{h})");
    let orientation = decoder.orientation()?;
    let mut img = DynamicImage::from_decoder(decoder)?;
    // Scale before rotating, so the full-size picture isn't copied.
    let turned = matches!(
        orientation,
        Orientation::Rotate90
            | Orientation::Rotate270
            | Orientation::Rotate90FlipH
            | Orientation::Rotate270FlipH
    );
    let (mw, mh) = if turned { (max.1, max.0) } else { max };
    if w > mw || h > mh {
        img = if w >= mw * 2 || h >= mh * 2 {
            // Far bigger: the box filter needs no big intermediate buffer.
            img.thumbnail(mw, mh)
        } else {
            img.resize(mw, mh, image::imageops::FilterType::Triangle)
        };
    }
    img.apply_orientation(orientation);
    let (width, height) = if turned { (h, w) } else { (w, h) };
    let mut data = img.into_rgba8();
    for px in data.pixels_mut() {
        px.0.swap(0, 2);
    }
    Ok(Decoded {
        image: Arc::new(RenderImage::new([Frame::new(data)])),
        width,
        height,
    })
}

impl GlanceApp {
    /// The thumbnail of `path`, starting a decode if there's none yet.
    /// `None` while decoding or if it can't be decoded.
    pub fn thumbnail(&mut self, path: &Path, cx: &mut Context<Self>) -> Option<Decoded> {
        self.decoded(path, Size::Thumb, cx)
    }

    fn decoded(&mut self, path: &Path, size: Size, cx: &mut Context<Self>) -> Option<Decoded> {
        let key = (path.to_path_buf(), size);
        match self.images.slots.get(&key) {
            Some(Slot::Ready(d)) => {
                let d = d.clone();
                if size == Size::Thumb {
                    self.images.last_used.insert(key.0, self.images.frame);
                }
                return Some(d);
            }
            Some(_) => return None,
            None => {}
        }
        if size == Size::Thumb {
            // Busy: asked again on the render after one finishes.
            if self.images.decoding >= MAX_DECODES {
                return None;
            }
            self.images.decoding += 1;
        }
        self.images.slots.insert(key.clone(), Slot::Loading);
        let max = match size {
            Size::Thumb => THUMB_DECODE,
            Size::Large => LARGE_DECODE,
        };
        let owned = path.to_path_buf();
        let job = cx
            .background_executor()
            .spawn(async move { decode(&owned, max) });
        cx.spawn(async move |this, cx| {
            let result = job.await;
            let _ = this.update(cx, |app, cx| {
                if key.1 == Size::Thumb {
                    app.images.decoding -= 1;
                }
                // Closed meanwhile: don't keep it.
                if !app.images.slots.contains_key(&key) {
                    return;
                }
                let slot = match result {
                    Ok(d) => Slot::Ready(d),
                    Err(e) => {
                        tracing::debug!("can't preview {}: {e:#}", key.0.display());
                        Slot::Failed
                    }
                };
                if matches!(slot, Slot::Ready(_)) && key.1 == Size::Thumb {
                    // Counts as shown now: it's drawn on the next render.
                    app.images.last_used.insert(key.0.clone(), app.images.frame);
                }
                app.images.slots.insert(key, slot);
                cx.notify();
            });
        })
        .detach();
        None
    }

    /// The clickable inline preview of the image at `path`, at most `max_w`
    /// wide (and [`THUMB_MAX_W`]×[`THUMB_MAX_H`]). `None` until it's decoded,
    /// or if it can't be.
    pub fn image_preview(
        &mut self,
        id: impl Into<ElementId>,
        path: &Path,
        name: &str,
        max_w: f32,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let d = self.thumbnail(path, cx)?;
        let (w, h) = fit_within(
            d.width as f32,
            d.height as f32,
            max_w.min(THUMB_MAX_W),
            THUMB_MAX_H,
        );
        let (path, name) = (path.to_path_buf(), name.to_string());
        Some(
            div()
                .id(id)
                .w(px(w))
                .h(px(h))
                .flex_shrink_0()
                .rounded(px(4.))
                .overflow_hidden()
                .cursor_pointer()
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.open_lightbox(path.clone(), name.clone(), cx)
                }))
                .child(
                    img(ImageSource::Render(d.image))
                        .size_full()
                        .object_fit(ObjectFit::Contain),
                )
                .into_any_element(),
        )
    }

    /// At the end of a render: drop the thumbnails over the limit that
    /// weren't shown in it, and remove images we no longer show from the
    /// GPU atlas.
    pub fn drop_stale_images(&mut self, window: &mut Window, cx: &mut App) {
        let images = &mut self.images;
        for path in evictions(&images.last_used, images.frame, KEEP) {
            images.last_used.remove(&path);
            if let Some(Slot::Ready(d)) = images.slots.remove(&(path, Size::Thumb)) {
                images.stale.push(d.image);
            }
        }
        images.frame += 1;
        for image in self.images.stale.drain(..) {
            let _ = window.drop_image(image);
        }
        let _ = cx;
    }

    pub fn open_lightbox(&mut self, path: PathBuf, name: String, cx: &mut Context<Self>) {
        self.close_lightbox();
        self.lightbox = Some(Lightbox { path, name });
        cx.notify();
    }

    /// Close the lightbox; true if it was open.
    pub fn close_lightbox(&mut self) -> bool {
        let Some(lb) = self.lightbox.take() else {
            return false;
        };
        if let Some(Slot::Ready(d)) = self.images.slots.remove(&(lb.path, Size::Large)) {
            self.images.stale.push(d.image);
        }
        true
    }

    /// The full-window image viewer: the image as large as fits, on a dark
    /// backdrop. Click outside it, press Escape or the X to close.
    pub fn render_lightbox(
        &mut self,
        p: &Palette,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let lb = self.lightbox.as_ref()?;
        let (path, name) = (lb.path.clone(), lb.name.clone());
        let large = self.decoded(&path, Size::Large, cx);
        let failed = matches!(
            self.images.slots.get(&(path.clone(), Size::Large)),
            Some(Slot::Failed)
        );
        let view = window.viewport_size();
        let (max_w, max_h) = (f32::from(view.width) - 96., f32::from(view.height) - 160.);
        let picture = match large {
            Some(d) => {
                let (w, h) = fit_within(d.width as f32, d.height as f32, max_w, max_h);
                div()
                    .id("lightbox-image")
                    // Clicking the picture itself doesn't close it.
                    .on_click(|_, _, cx| cx.stop_propagation())
                    .child(
                        img(ImageSource::Render(d.image))
                            .w(px(w))
                            .h(px(h))
                            .rounded(px(4.))
                            .object_fit(ObjectFit::Contain),
                    )
                    .into_any_element()
            }
            None => div()
                .text_color(gpui_kit::white().opacity(0.7))
                .child(if failed {
                    "Can't show this image"
                } else {
                    "Loading…"
                })
                .into_any_element(),
        };
        let button = |id: &'static str, glyph: IconName, tip: &'static str| {
            div()
                .id(id)
                .size(px(36.))
                .rounded_full()
                .flex()
                .items_center()
                .justify_center()
                .cursor_pointer()
                .bg(gpui_kit::black().opacity(0.4))
                .hover(|s| s.bg(gpui_kit::black().opacity(0.8)))
                .tooltip(move |window, cx| Tooltip::new(tip).build(window, cx))
                .child(icon(glyph, 20., gpui_kit::white()))
        };
        let open_path = path.clone();
        let reveal_path = path.clone();
        let bar = div()
            .absolute()
            .top_3()
            .left_4()
            .right_4()
            .flex()
            .items_center()
            .gap_2()
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_color(gpui_kit::white())
                    .font_weight(FontWeight::MEDIUM)
                    .child(name),
            )
            .child(
                button("lightbox-open", IconName::ExternalLink, "Open").on_click(cx.listener(
                    move |_, _, _, cx| {
                        cx.stop_propagation();
                        cx.open_with_system(&open_path);
                    },
                )),
            )
            .child(
                button("lightbox-reveal", IconName::FolderOpen, "Show in folder").on_click(
                    cx.listener(move |_, _, _, cx| {
                        cx.stop_propagation();
                        cx.reveal_path(&reveal_path);
                    }),
                ),
            )
            .child(
                button("lightbox-close", IconName::X, "Close").on_click(cx.listener(
                    |this, _, _, cx| {
                        cx.stop_propagation();
                        this.close_lightbox();
                        cx.notify();
                    },
                )),
            );
        Some(
            div()
                .id("lightbox")
                .occlude()
                .absolute()
                .inset_0()
                .bg(p.lightbox_backdrop)
                .flex()
                .items_center()
                .justify_center()
                .on_click(cx.listener(|this, _, _, cx| {
                    this.close_lightbox();
                    cx.notify();
                }))
                .child(picture)
                .child(bar)
                .into_any_element(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::{MAX_SIDE, decode, evictions, fit_within};
    use std::collections::HashMap;
    use std::path::PathBuf;

    #[test]
    fn decode_scales_down_and_refuses_huge_images() {
        let dir = std::env::temp_dir().join(format!("glance-images-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // A photo-sized picture comes back fitted, with its real size.
        let ok = dir.join("ok.png");
        image::GrayImage::new(3000, 1000).save(&ok).unwrap();
        let d = decode(&ok, (800, 600)).unwrap();
        assert_eq!((d.width, d.height), (3000, 1000));
        assert_eq!(d.image.size(0).width.0, 800);
        // A tiny file claiming enormous dimensions isn't decoded.
        let huge = dir.join("huge.png");
        image::GrayImage::new(MAX_SIDE + 1, 1).save(&huge).unwrap();
        assert!(decode(&huge, (800, 600)).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn eviction_keeps_what_is_on_screen() {
        let used: HashMap<PathBuf, u64> = [("a", 1), ("b", 5), ("c", 3), ("d", 9), ("e", 9)]
            .into_iter()
            .map(|(p, f)| (PathBuf::from(p), f))
            .collect();
        // Under the limit: nothing goes.
        assert!(evictions(&used, 9, 5).is_empty());
        // Over: least recently shown first.
        let mut out = evictions(&used, 9, 3);
        out.sort();
        assert_eq!(out, [PathBuf::from("a"), PathBuf::from("c")]);
        // Everything shown in this render stays, even over the limit.
        let mut out = evictions(&used, 9, 0);
        out.sort();
        assert_eq!(
            out,
            [PathBuf::from("a"), PathBuf::from("b"), PathBuf::from("c")]
        );
    }

    #[test]
    fn thumbnails_fit_and_keep_their_shape() {
        // Wide: limited by width.
        assert_eq!(fit_within(1600., 900., 400., 300.), (400., 225.));
        // Tall: limited by height.
        assert_eq!(fit_within(1000., 2000., 400., 300.), (150., 300.));
        // Small images aren't blown up.
        assert_eq!(fit_within(120., 80., 400., 300.), (120., 80.));
        // Exactly the box.
        assert_eq!(fit_within(800., 600., 400., 300.), (400., 300.));
        // Extreme strips stay at least a pixel.
        assert_eq!(fit_within(10000., 1., 400., 300.), (400., 1.));
        // Degenerate input.
        assert_eq!(fit_within(0., 10., 400., 300.), (0., 0.));
        assert_eq!(fit_within(10., 10., 0., 300.), (0., 0.));
    }
}
