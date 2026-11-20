//! Home screen: rail, contacts sidebar with user panel, and the start/join area.
//! Placeholder data until rooms (M3) and contacts (M3b) land.

use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::{ActiveTheme, Sizable};
use gpui_kit::*;

use crate::theme::Palette;

#[derive(Default)]
pub struct Home;

impl Render for Home {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = Palette::current(cx);
        let font = cx.theme().font_family.clone();
        div()
            .size_full()
            .flex()
            .font_family(font)
            .text_color(p.text_normal)
            .child(rail(&p))
            .child(sidebar(&p))
            .child(content(&p))
    }
}

fn rail(p: &Palette) -> impl IntoElement {
    div()
        .w(px(72.))
        .h_full()
        .flex()
        .flex_col()
        .items_center()
        .pt_3()
        .gap_2()
        .bg(p.bg_rail)
        .child(
            div()
                .size(px(48.))
                .rounded(px(16.))
                .bg(p.brand)
                .flex()
                .items_center()
                .justify_center()
                .text_color(gpui_kit::white())
                .child(
                    svg()
                        .path(IconName::Monitor.path())
                        .size(px(26.))
                        .text_color(gpui_kit::white()),
                ),
        )
        .child(div().w(px(32.)).h(px(2.)).rounded_full().bg(p.bg_selected))
}

fn sidebar(p: &Palette) -> impl IntoElement {
    div()
        .w(px(240.))
        .h_full()
        .flex()
        .flex_col()
        .bg(p.bg_sidebar)
        .child(
            div()
                .h(px(48.))
                .px_4()
                .flex()
                .items_center()
                .border_b_1()
                .border_color(p.bg_rail)
                .text_color(p.text_header)
                .font_weight(FontWeight::SEMIBOLD)
                .child("Contacts"),
        )
        .child(
            div()
                .flex_1()
                .p_4()
                .text_sm()
                .text_color(p.text_muted)
                .child("People you add after a call show up here."),
        )
        .child(user_panel(p))
}

fn user_panel(p: &Palette) -> impl IntoElement {
    let icon_btn = |icon: IconName| {
        div()
            .size(px(32.))
            .rounded(px(4.))
            .flex()
            .items_center()
            .justify_center()
            .text_color(p.interactive)
            .hover(|s| s.bg(p.bg_hover))
            .child(
                svg()
                    .path(icon.path())
                    .size(px(20.))
                    .text_color(p.interactive),
            )
    };
    div()
        .h(px(52.))
        .px_2()
        .flex()
        .items_center()
        .gap_2()
        .bg(p.bg_user_panel)
        .child(avatar(p, "Y", p.brand, Some(p.green)))
        .child(
            div()
                .flex_1()
                .flex()
                .flex_col()
                .child(div().text_sm().text_color(p.text_header).child("You"))
                .child(div().text_xs().text_color(p.text_muted).child("Online")),
        )
        .child(icon_btn(IconName::Mic))
        .child(icon_btn(IconName::Headphones))
        .child(icon_btn(IconName::Settings))
}

/// A circular avatar with an initial and an optional status dot.
pub fn avatar(p: &Palette, initial: &str, color: Hsla, status: Option<Hsla>) -> impl IntoElement {
    div()
        .relative()
        .size(px(32.))
        .flex_shrink_0()
        .child(
            div()
                .size_full()
                .rounded_full()
                .bg(color)
                .flex()
                .items_center()
                .justify_center()
                .text_sm()
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(gpui_kit::white())
                .child(initial.to_string()),
        )
        .children(status.map(|c| {
            div()
                .absolute()
                .right(px(-2.))
                .bottom(px(-2.))
                .size(px(14.))
                .rounded_full()
                .border_3()
                .border_color(p.bg_user_panel)
                .bg(c)
        }))
}

fn content(p: &Palette) -> impl IntoElement {
    div()
        .flex_1()
        .h_full()
        .flex()
        .flex_col()
        .items_center()
        .justify_center()
        .gap_4()
        .bg(p.bg_content)
        .child(
            div()
                .text_2xl()
                .font_weight(FontWeight::BOLD)
                .text_color(p.text_header)
                .child("Share your screen in one click"),
        )
        .child(div().text_color(p.text_muted).child(
            "Start a room and send the link. Anyone in the room can go live at the same time.",
        ))
        .child(
            div()
                .flex()
                .gap_3()
                .child(
                    Button::new("start-room")
                        .primary()
                        .large()
                        .icon(IconName::Plus)
                        .label("Start a room"),
                )
                .child(
                    Button::new("join-room")
                        .large()
                        .icon(IconName::LogIn)
                        .label("Join with a link"),
                ),
        )
}
