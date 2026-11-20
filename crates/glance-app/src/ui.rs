//! Small shared widgets in the glance (Discord-style) look.

use glance_core::{Presence, Profile};
use gpui_kit::assets::IconName;
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::theme::Palette;

/// 0xRRGGBB to a gpui colour.
pub fn color(rgb_u32: u32) -> Hsla {
    rgb(rgb_u32).into()
}

pub fn presence_color(p: &Palette, presence: Presence) -> Hsla {
    match presence {
        Presence::Online => p.green,
        Presence::Idle => p.yellow,
        Presence::DoNotDisturb => p.red,
        Presence::Invisible | Presence::Offline => p.gray,
    }
}

pub fn icon(name: IconName, size: f32, color: Hsla) -> Svg {
    svg()
        .path(name.path())
        .size(px(size))
        .flex_shrink_0()
        .text_color(color)
}

/// Circular avatar with initials. `ring` draws the green speaking ring;
/// `status` draws a presence dot cut out of `cutout` (the background colour).
pub struct Avatar {
    profile: Profile,
    size: f32,
    ring: Option<Hsla>,
    status: Option<(Hsla, Hsla)>,
}

impl Avatar {
    pub fn new(profile: &Profile, size: f32) -> Self {
        Self {
            profile: profile.clone(),
            size,
            ring: None,
            status: None,
        }
    }

    pub fn ring(mut self, color: Option<Hsla>) -> Self {
        self.ring = color;
        self
    }

    pub fn status(mut self, dot: Hsla, cutout: Hsla) -> Self {
        self.status = Some((dot, cutout));
        self
    }
}

impl IntoElement for Avatar {
    type Element = Div;

    fn into_element(self) -> Self::Element {
        let s = self.size;
        let dot = (s * 0.375).max(10.);
        let text_size = (s * 0.4).max(10.);
        div()
            .relative()
            .size(px(s))
            .flex_shrink_0()
            .child(
                div()
                    .size_full()
                    .rounded_full()
                    .bg(color(self.profile.color))
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_size(px(text_size))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(gpui_kit::white())
                    .when_some(self.ring, |d, ring| d.border_2().border_color(ring))
                    .child(self.profile.initials()),
            )
            .when_some(self.status, |d, (dot_color, cutout)| {
                d.child(
                    div()
                        .absolute()
                        .right(px(-2.))
                        .bottom(px(-2.))
                        .size(px(dot))
                        .rounded_full()
                        .border_3()
                        .border_color(cutout)
                        .bg(dot_color),
                )
            })
    }
}

/// Square icon button used in the user panel and headers.
pub fn icon_button(
    id: impl Into<ElementId>,
    name: IconName,
    tip: &'static str,
    fg: Hsla,
    p: &Palette,
) -> Stateful<Div> {
    let hover = p.bg_hover;
    div()
        .id(id)
        .size(px(32.))
        .rounded(px(4.))
        .flex()
        .items_center()
        .justify_center()
        .cursor_pointer()
        .hover(move |s| s.bg(hover))
        .tooltip(move |window, cx| Tooltip::new(tip).build(window, cx))
        .child(icon(name, 20., fg))
}

/// Red "LIVE" pill.
pub fn live_badge(p: &Palette) -> Div {
    div()
        .px(px(5.))
        .rounded(px(4.))
        .bg(p.red)
        .text_size(px(10.))
        .font_weight(FontWeight::BOLD)
        .text_color(gpui_kit::white())
        .child("LIVE")
}

/// Uppercase section label ("ONLINE — 3").
pub fn section_label(p: &Palette, text: impl Into<SharedString>) -> Div {
    div()
        .text_xs()
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(p.text_muted)
        .child(text.into())
}

/// A selectable pill (Discord's segmented options in Go Live).
pub fn pill(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    selected: bool,
    disabled: bool,
    p: &Palette,
) -> Stateful<Div> {
    let (bg, fg, border) = if selected {
        (p.brand.opacity(0.15), p.text_header, p.brand)
    } else {
        (p.bg_input, p.text_normal, p.bg_input)
    };
    let hover = p.bg_hover;
    div()
        .id(id)
        .px_3()
        .py_1()
        .rounded(px(4.))
        .border_1()
        .border_color(border)
        .bg(bg)
        .text_sm()
        .text_color(fg)
        .when(disabled, |d| d.opacity(0.4))
        .when(!disabled && !selected, move |d| {
            d.cursor_pointer().hover(move |s| s.bg(hover))
        })
        .child(label.into())
}
