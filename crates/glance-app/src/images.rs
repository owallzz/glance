//! Images in chat: thumbnails for inline previews and the full-window
//! lightbox. Images are decoded (and scaled down) on a background thread
//! once per path and size, then kept as `RenderImage`s; nothing is decoded
//! while painting.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use gpui_kit::assets::IconName;
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::*;
use image::{DynamicImage, Frame, ImageDecoder, ImageReader};

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
/// Decoded thumbnails kept; older ones are dropped (and decoded again if
/// they scroll back into view).
const KEEP: usize = 64;

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
    /// Ready thumbnails, oldest first (for eviction).
    order: VecDeque<PathBuf>,
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
    let mut decoder = ImageReader::open(path)?
        .with_guessed_format()?
        .into_decoder()?;
    let orientation = decoder.orientation()?;
    let mut img = DynamicImage::from_decoder(decoder)?;
    img.apply_orientation(orientation);
    let (width, height) = (img.width(), img.height());
    if width > max.0 || height > max.1 {
        img = img.resize(max.0, max.1, image::imageops::FilterType::Triangle);
    }
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
            Some(Slot::Ready(d)) => return Some(d.clone()),
            Some(_) => return None,
            None => {}
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
                // Closed or evicted meanwhile: don't keep it.
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
                    app.images.order.push_back(key.0.clone());
                    while app.images.order.len() > KEEP {
                        if let Some(old) = app.images.order.pop_front()
                            && let Some(Slot::Ready(d)) =
                                app.images.slots.remove(&(old, Size::Thumb))
                        {
                            app.images.stale.push(d.image);
                        }
                    }
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

    /// Remove images we no longer show from the GPU atlas.
    pub fn drop_stale_images(&mut self, window: &mut Window, cx: &mut App) {
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
    use super::fit_within;

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
