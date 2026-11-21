//! The right-click menu for someone in the room: a floating menu at the
//! click position (member list or stage tile) with their contact action,
//! mute and volume for you, and their stream audio's mute and volume.
//! Closes on a click outside, Escape, or picking an item.

use glance_core::PeerId;
use glance_core::contacts::StoredState;
use gpui_kit::assets::IconName;
use gpui_kit::component::slider::{Slider, SliderEvent, SliderState};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::app::GlanceApp;
use crate::theme::Palette;
use crate::ui::icon;

/// An open person menu (lives in `Room::menu`).
pub struct PersonMenu {
    pub peer: PeerId,
    /// Where the click was, in window coordinates.
    position: Point<Pixels>,
    volume: Entity<SliderState>,
    stream_volume: Entity<SliderState>,
    _subscriptions: [Subscription; 2],
}

const MENU_WIDTH: f32 = 220.;

impl GlanceApp {
    /// Open the menu for `peer` at `position` (replacing any open one).
    pub fn open_person_menu(
        &mut self,
        peer: PeerId,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if peer == self.me.id || self.room.is_none() {
            return;
        }
        let slider = |value: f32, cx: &mut Context<Self>| {
            cx.new(|_| {
                SliderState::new()
                    .min(0.)
                    .max(200.)
                    .step(1.)
                    .default_value(value * 100.)
            })
        };
        let volume = slider(self.user_volume(peer), cx);
        let stream_volume = slider(self.stream_volume(peer), cx);
        // Applied live while dragging, saved when let go.
        let subscriptions = [
            cx.subscribe(&volume, move |this, _, ev: &SliderEvent, cx| {
                let (SliderEvent::Change(v) | SliderEvent::Release(v)) = ev;
                let persist = matches!(ev, SliderEvent::Release(_));
                this.set_user_volume(peer, (v.start() / 100.).clamp(0., 2.), persist);
                cx.notify();
            }),
            cx.subscribe(&stream_volume, move |this, _, ev: &SliderEvent, cx| {
                let (SliderEvent::Change(v) | SliderEvent::Release(v)) = ev;
                let persist = matches!(ev, SliderEvent::Release(_));
                this.set_stream_volume(peer, (v.start() / 100.).clamp(0., 2.), persist);
                cx.notify();
            }),
        ];
        if let Some(room) = self.room.as_mut() {
            room.menu = Some(PersonMenu {
                peer,
                position,
                volume,
                stream_volume,
                _subscriptions: subscriptions,
            });
        }
        // So Escape reaches the app (and not, say, the chat box).
        self.focus.focus(window, cx);
        cx.notify();
    }

    /// Close the person menu; false if none was open.
    pub fn close_person_menu(&mut self) -> bool {
        self.room.as_mut().and_then(|r| r.menu.take()).is_some()
    }

    pub fn render_person_menu(
        &mut self,
        p: &Palette,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let room = self.room.as_ref()?;
        let menu = room.menu.as_ref()?;
        let peer = menu.peer;
        let (position, volume, stream_volume) = (
            menu.position,
            menu.volume.clone(),
            menu.stream_volume.clone(),
        );
        let in_live_room = room.handle.is_some();
        let Some(person) = room
            .participants
            .iter()
            .find(|x| x.profile.id == peer)
            .cloned()
        else {
            // They left.
            self.close_person_menu();
            return None;
        };
        let has_stream_audio = person.stream.as_ref().is_some_and(|s| s.has_audio);
        let muted = self.user_muted(peer);
        let stream_muted = self.stream_muted(peer);
        let vol = self.user_volume(peer);
        let stream_vol = self.stream_volume(peer);
        let short = peer.short();

        let contact_item = in_live_room.then(|| match self.book.state(&peer.0) {
            None => {
                let profile = person.profile.clone();
                menu_item(p, &format!("pm-addc-{short}"), "Add contact")
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.close_person_menu();
                        this.add_contact(profile.clone(), cx);
                        cx.notify();
                    }))
                    .into_any_element()
            }
            Some(StoredState::Incoming) => {
                menu_item(p, &format!("pm-acceptc-{short}"), "Accept contact request")
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.close_person_menu();
                        this.accept_contact(peer, cx);
                        cx.notify();
                    }))
                    .into_any_element()
            }
            Some(StoredState::Mutual) => menu_item(p, &format!("pm-msg-{short}"), "Message")
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.close_person_menu();
                    this.open_dm(peer, window, cx);
                    cx.notify();
                }))
                .into_any_element(),
            Some(StoredState::Outgoing) => {
                menu_note(p, "Contact request sent", p.text_muted).into_any_element()
            }
            Some(StoredState::Blocked) => menu_note(p, "Blocked", p.red).into_any_element(),
        });

        let panel = div()
            .id(SharedString::from(format!("person-menu-{short}")))
            .occlude()
            .w(px(MENU_WIDTH))
            .p(px(6.))
            .rounded(px(8.))
            .bg(p.bg_menu)
            .border_1()
            .border_color(p.menu_border)
            .shadow_lg()
            .flex()
            .flex_col()
            .text_sm()
            .text_color(p.text_normal)
            .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                if this.close_person_menu() {
                    cx.notify();
                }
            }))
            .when_some(contact_item, |d, item| {
                d.child(item).child(menu_separator(p))
            })
            .child(
                menu_check(p, &format!("pm-mute-{short}"), "Mute", muted).on_click(cx.listener(
                    move |this, _, _, cx| {
                        this.close_person_menu();
                        this.set_user_muted(peer, !muted);
                        cx.notify();
                    },
                )),
            )
            .child(menu_slider(p, "User volume", vol, &volume))
            .when(has_stream_audio, |d| {
                d.child(menu_separator(p))
                    .child(
                        menu_check(
                            p,
                            &format!("pm-smute-{short}"),
                            "Mute stream audio",
                            stream_muted,
                        )
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.close_person_menu();
                            this.set_stream_muted(peer, !stream_muted);
                            cx.notify();
                        })),
                    )
                    .child(menu_slider(p, "Stream volume", stream_vol, &stream_volume))
            });
        Some(
            deferred(
                anchored()
                    .position(position)
                    .snap_to_window_with_margin(px(8.))
                    .child(panel),
            )
            .with_priority(1)
            .into_any_element(),
        )
    }
}

/// A clickable menu line: highlighted in the accent colour on hover.
fn menu_item(p: &Palette, id: &str, label: &'static str) -> Stateful<Div> {
    let brand = p.brand;
    div()
        .id(SharedString::from(id.to_string()))
        .min_h(px(32.))
        .px_2()
        .py(px(6.))
        .rounded(px(4.))
        .cursor_pointer()
        .flex()
        .items_center()
        .gap_2()
        .font_weight(FontWeight::MEDIUM)
        .hover(move |s| s.bg(brand).text_color(gpui_kit::white()))
        .child(div().flex_1().child(label))
}

/// A menu line with a check box on the right.
fn menu_check(p: &Palette, id: &str, label: &'static str, checked: bool) -> Stateful<Div> {
    menu_item(p, id, label).child(
        div()
            .size(px(18.))
            .flex_shrink_0()
            .rounded(px(4.))
            .border_1()
            .border_color(if checked { p.brand } else { p.interactive })
            .when(checked, |d| d.bg(p.brand))
            .flex()
            .items_center()
            .justify_center()
            .when(checked, |d| {
                d.child(icon(IconName::Check, 14., gpui_kit::white()))
            }),
    )
}

/// A line that isn't clickable (e.g. "Contact request sent").
fn menu_note(_p: &Palette, label: &'static str, color: Hsla) -> Div {
    div()
        .min_h(px(32.))
        .px_2()
        .py(px(6.))
        .flex()
        .items_center()
        .font_weight(FontWeight::MEDIUM)
        .text_color(color)
        .child(label)
}

fn menu_separator(p: &Palette) -> Div {
    div().h(px(1.)).mx(px(4.)).my(px(4.)).bg(p.menu_border)
}

/// A label and value over a 0–200% slider.
fn menu_slider(p: &Palette, label: &'static str, value: f32, slider: &Entity<SliderState>) -> Div {
    div()
        .px_2()
        .py(px(6.))
        .flex()
        .flex_col()
        .gap_2()
        .child(
            div()
                .flex()
                .items_center()
                .text_xs()
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(p.text_muted)
                .child(div().flex_1().child(label))
                .child(format!("{:.0}%", value * 100.)),
        )
        .child(Slider::new(slider).accessibility_label(label))
}
