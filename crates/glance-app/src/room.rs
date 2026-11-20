//! Room screen: participant sidebar, the stage (grid / focus + filmstrip),
//! the call control bar, and the chat panel with file cards.

use glance_core::{ChatBody, ChatMessage, FileOffer, Participant, PeerId, Route, Transfer, format};
use gpui_kit::assets::IconName;
use gpui_kit::component::Sizable;
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::input::Input;
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::app::GlanceApp;
use crate::theme::Palette;
use crate::ui::{Avatar, icon, icon_button, live_badge, section_label};

/// What a stage tile shows.
#[derive(Clone)]
enum TileKind {
    /// Avatar card for someone (camera tiles render the same until M8).
    Person(Participant),
    /// Someone's screen share.
    Stream(Participant),
}

impl TileKind {
    fn peer(&self) -> PeerId {
        match self {
            TileKind::Person(p) | TileKind::Stream(p) => p.profile.id,
        }
    }

    fn key(&self) -> String {
        match self {
            TileKind::Person(p) => format!("person-{}", p.profile.id.short()),
            TileKind::Stream(p) => format!("stream-{}", p.profile.id.short()),
        }
    }
}

impl GlanceApp {
    pub fn render_room(
        &mut self,
        p: &Palette,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let chat_open = self.room.as_ref().is_some_and(|r| r.chat_open);
        div()
            .id("room-view")
            .flex_1()
            .min_w_0()
            .h_full()
            .flex()
            // Drop files anywhere in the room to send them.
            .on_drop(cx.listener(|this, paths: &ExternalPaths, _, cx| {
                this.send_files(paths.paths().to_vec(), cx);
            }))
            .child(self.render_room_sidebar(p, cx))
            .child(self.render_stage(p, cx))
            .when(chat_open, |d| d.child(self.render_chat(p, cx)))
    }

    fn render_room_sidebar(&mut self, p: &Palette, cx: &mut Context<Self>) -> impl IntoElement {
        let room = self.room.as_ref().expect("in a room");
        let name = room.name.clone();
        let link = room.link.clone();
        let people = room.participants.clone();
        let elapsed = format::duration(room.started.elapsed().as_secs());
        let copied = self.link_copied;
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
                    .justify_between()
                    .border_b_1()
                    .border_color(p.bg_rail)
                    .child(
                        div()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(p.text_header)
                            .truncate()
                            .child(name),
                    )
                    .child(
                        icon_button(
                            "invite",
                            IconName::UserPlus,
                            "Invite people",
                            p.interactive,
                            p,
                        )
                        .on_click(cx.listener(move |this, _, _, cx| {
                            cx.write_to_clipboard(ClipboardItem::new_string(link.clone()));
                            this.link_copied = true;
                            this.invite_open = true;
                            cx.notify();
                        })),
                    ),
            )
            .child(
                div()
                    .id("room-people")
                    .flex_1()
                    .overflow_y_scroll()
                    .p_2()
                    .flex()
                    .flex_col()
                    .gap(px(2.))
                    .child(
                        div()
                            .px_2()
                            .py_1()
                            .flex()
                            .items_center()
                            .gap_1()
                            .child(icon(IconName::Volume2, 18., p.text_muted))
                            .child(section_label(p, format!("IN THE ROOM — {}", people.len()))),
                    )
                    .children(
                        people
                            .into_iter()
                            .map(|person| self.person_row(p, person, cx)),
                    ),
            )
            // Invite link card
            .child(
                div()
                    .mx_2()
                    .mb_2()
                    .p_3()
                    .rounded(px(8.))
                    .bg(p.bg_content)
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(
                        div()
                            .text_xs()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(p.text_muted)
                            .child(if copied {
                                "INVITE LINK COPIED"
                            } else {
                                "INVITE LINK"
                            }),
                    )
                    .child(
                        div().text_xs().text_color(p.text_normal).truncate().child(
                            self.room
                                .as_ref()
                                .map(|r| r.link.clone())
                                .unwrap_or_default(),
                        ),
                    ),
            )
            // Voice connected panel
            .child(
                div()
                    .px_2()
                    .py_2()
                    .flex()
                    .items_center()
                    .gap_2()
                    .border_t_1()
                    .border_color(p.bg_rail)
                    .bg(p.bg_user_panel)
                    .child(
                        div()
                            .flex_1()
                            .flex()
                            .flex_col()
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap_1()
                                    .text_sm()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(p.green)
                                    .child(icon(IconName::Signal, 16., p.green))
                                    .child("Voice Connected"),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(p.text_muted)
                                    .child(format!("Direct · {elapsed}")),
                            ),
                    )
                    .child(
                        icon_button(
                            "disconnect",
                            IconName::PhoneOff,
                            "Disconnect",
                            p.interactive,
                            p,
                        )
                        .on_click(cx.listener(|this, _, _, cx| this.leave_room(cx))),
                    ),
            )
            .child(self.render_user_panel(p, cx))
    }

    fn person_row(
        &mut self,
        p: &Palette,
        person: Participant,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let id = person.profile.id;
        let hover = p.bg_hover;
        let ring = person.speaking.then_some(p.green);
        let route = person.connection.map(|c| match c.route {
            Route::Direct => format!("Direct · {} ms", c.rtt_ms),
            Route::Relayed => format!("Relayed · {} ms", c.rtt_ms),
        });
        let row = div()
            .id(SharedString::from(format!("person-{}", id.short())))
            .h(px(34.))
            .pl(px(20.))
            .pr_2()
            .rounded(px(4.))
            .flex()
            .items_center()
            .gap_2()
            .cursor_pointer()
            .hover(move |s| s.bg(hover))
            .when_some(route, |d, route| {
                d.tooltip(move |window, cx| Tooltip::new(route.clone()).build(window, cx))
            })
            .on_click(cx.listener(move |this, _, _, cx| {
                this.watch(id, cx);
                if let Some(room) = this.room.as_mut()
                    && room.watching.contains_key(&id)
                {
                    room.focused = Some(id);
                }
                cx.notify();
            }))
            .child(Avatar::new(&person.profile, 24.).ring(ring))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_sm()
                    .truncate()
                    .text_color(if person.speaking {
                        p.text_header
                    } else {
                        p.text_muted
                    })
                    .child(person.profile.name.clone()),
            )
            .when(person.stream.is_some(), |d| d.child(live_badge(p)))
            .when(person.camera, |d| {
                d.child(icon(IconName::Video, 14., p.text_muted))
            })
            .when(person.muted, |d| {
                d.child(icon(IconName::MicOff, 14., p.red))
            })
            .when(person.deafened, |d| {
                d.child(icon(IconName::HeadphoneOff, 14., p.red))
            })
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, _, _, cx| {
                    if let Some(room) = this.room.as_mut() {
                        room.volume_open = if room.volume_open == Some(id) {
                            None
                        } else {
                            Some(id)
                        };
                    }
                    cx.notify();
                }),
            );
        let volume_open = self
            .room
            .as_ref()
            .is_some_and(|r| r.volume_open == Some(id))
            && id != self.me.id;
        // "Add contact" for people you aren't connected with yet.
        let contact_state = self.book.state(&id.0);
        let in_live_room = self.room.as_ref().is_some_and(|r| r.handle.is_some());
        let person_profile = person.profile.clone();
        let contact_action = div().ml(px(20.)).mr_2().mb_1().when(in_live_room, |d| {
            d.child(match contact_state {
                None => Button::new(SharedString::from(format!("addc-{}", id.short())))
                    .small()
                    .primary()
                    .label("Add contact")
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.add_contact(person_profile.clone(), cx)
                    }))
                    .into_any_element(),
                Some(glance_core::contacts::StoredState::Outgoing) => div()
                    .text_xs()
                    .text_color(p.text_muted)
                    .child("Contact request sent")
                    .into_any_element(),
                Some(glance_core::contacts::StoredState::Incoming) => {
                    Button::new(SharedString::from(format!("acceptc-{}", id.short())))
                        .small()
                        .success()
                        .label("Accept contact request")
                        .on_click(cx.listener(move |this, _, _, cx| this.accept_contact(id, cx)))
                        .into_any_element()
                }
                Some(glance_core::contacts::StoredState::Mutual) => div()
                    .text_xs()
                    .text_color(p.text_muted)
                    .child("In your contacts")
                    .into_any_element(),
                Some(glance_core::contacts::StoredState::Blocked) => div()
                    .text_xs()
                    .text_color(p.red)
                    .child("Blocked")
                    .into_any_element(),
            })
        });
        let vol = self.user_volume(id);
        let step = |delta: f32, label: &'static str, key: &str| {
            div()
                .id(SharedString::from(format!("{key}-{}", id.short())))
                .size(px(24.))
                .rounded(px(4.))
                .bg(p.bg_selected)
                .hover(|s| s.opacity(0.8))
                .cursor_pointer()
                .flex()
                .items_center()
                .justify_center()
                .text_color(p.text_header)
                .child(label)
                .on_click(cx.listener(move |this, _, _, cx| {
                    let v = this.user_volume(id);
                    this.set_user_volume(id, ((v + delta) * 10.0).round() / 10.0);
                    cx.notify();
                }))
        };
        div()
            .flex()
            .flex_col()
            .child(row)
            .when(volume_open, |d| {
                d.child(
                    div()
                        .ml(px(20.))
                        .mr_2()
                        .mb_1()
                        .p_2()
                        .rounded(px(4.))
                        .bg(p.bg_rail)
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(
                            div()
                                .flex_1()
                                .text_xs()
                                .text_color(p.text_muted)
                                .child(format!("User volume {:.0}%", vol * 100.0)),
                        )
                        .child(step(-0.1, "−", "vol-down"))
                        .child(step(0.1, "+", "vol-up")),
                )
                .child(contact_action)
            })
            .into_any_element()
    }

    fn render_stage(&mut self, p: &Palette, cx: &mut Context<Self>) -> impl IntoElement {
        let room = self.room.as_ref().expect("in a room");
        let chat_open = room.chat_open;
        let name = room.name.clone();
        let mut tiles: Vec<TileKind> = Vec::new();
        for person in &room.participants {
            if person.stream.is_some() {
                tiles.push(TileKind::Stream(person.clone()));
            }
        }
        for person in &room.participants {
            tiles.push(TileKind::Person(person.clone()));
        }
        let focused = room.focused.and_then(|id| {
            tiles
                .iter()
                .position(|t| matches!(t, TileKind::Stream(_)) && t.peer() == id)
        });

        let body =
            match focused {
                Some(ix) => {
                    let main = tiles.remove(ix);
                    div()
                        .flex_1()
                        .min_h_0()
                        .flex()
                        .flex_col()
                        .gap_2()
                        .p_2()
                        .child(div().flex_1().min_h_0().child(self.tile(p, main, true, cx)))
                        .child(div().h(px(120.)).flex().justify_center().gap_2().children(
                            tiles.into_iter().map(|t| {
                                div().w(px(213.)).h_full().child(self.tile(p, t, false, cx))
                            }),
                        ))
                }
                None => {
                    let n = tiles.len().max(1);
                    let cols = (n as f32).sqrt().ceil() as usize;
                    let rows: Vec<Vec<TileKind>> = tiles.chunks(cols).map(|c| c.to_vec()).collect();
                    div()
                        .flex_1()
                        .min_h_0()
                        .p_2()
                        .flex()
                        .flex_col()
                        .gap_2()
                        .children(rows.into_iter().map(|row| {
                            div()
                                .flex_1()
                                .min_h_0()
                                .min_w_0()
                                .flex()
                                .justify_center()
                                .gap_2()
                                .children(row.into_iter().map(|t| {
                                    div()
                                        .flex_1()
                                        .min_w_0()
                                        .max_w(px(900.))
                                        .h_full()
                                        .child(self.tile(p, t, false, cx))
                                }))
                        }))
                }
            };

        div()
            .flex_1()
            .min_w_0()
            .h_full()
            .flex()
            .flex_col()
            .bg(p.bg_stage)
            .child(
                div()
                    .h(px(48.))
                    .px_4()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(gpui_kit::white())
                            .child(icon(IconName::Volume2, 20., p.text_muted))
                            .child(name),
                    )
                    .child(
                        icon_button(
                            "toggle-chat",
                            IconName::MessageSquare,
                            if chat_open { "Hide chat" } else { "Show chat" },
                            if chat_open {
                                gpui_kit::white()
                            } else {
                                p.interactive
                            },
                            p,
                        )
                        .on_click(cx.listener(|this, _, _, cx| {
                            if let Some(room) = this.room.as_mut() {
                                room.chat_open = !room.chat_open;
                            }
                            cx.notify();
                        })),
                    ),
            )
            .child(body)
            .child(self.render_controls(p, cx))
    }

    fn tile(
        &mut self,
        p: &Palette,
        kind: TileKind,
        large: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let peer = kind.peer();
        let key = kind.key();
        let room = self.room.as_ref().expect("in a room");
        let tile = div()
            .id(SharedString::from(key))
            .relative()
            .size_full()
            .rounded(px(8.))
            .overflow_hidden();
        match kind {
            TileKind::Person(person) => {
                let speaking = person.speaking;
                if let Some(cam) = room.cameras.get(&peer).cloned() {
                    return tile
                        .bg(gpui_kit::black())
                        .when(speaking, |d| d.border_2().border_color(p.green))
                        .child(cam)
                        .child(name_tag(p, &person.profile.name, person.muted))
                        .into_any_element();
                }
                tile.bg(crate::ui::color(person.profile.color).opacity(0.35))
                    .bg(p.bg_sidebar)
                    .when(speaking, |d| d.border_2().border_color(p.green))
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(Avatar::new(&person.profile, if large { 96. } else { 64. }))
                    .child(name_tag(p, &person.profile.name, person.muted))
                    .into_any_element()
            }
            TileKind::Stream(person) => {
                let stream = person.stream.clone().expect("stream tile");
                let is_me = peer == self.me.id;
                if room.popouts.contains_key(&peer) {
                    return tile
                        .bg(p.bg_sidebar)
                        .flex()
                        .flex_col()
                        .items_center()
                        .justify_center()
                        .gap_2()
                        .child(icon(IconName::ExternalLink, 28., p.text_muted))
                        .child(div().text_sm().text_color(p.text_muted).child("Popped out"))
                        .child(
                            div()
                                .id(SharedString::from(format!("popin-{}", peer.short())))
                                .px_3()
                                .py_1()
                                .rounded(px(4.))
                                .bg(p.bg_selected)
                                .cursor_pointer()
                                .text_sm()
                                .text_color(p.text_header)
                                .child("Bring back")
                                .on_click(cx.listener(move |this, _, _, cx| this.pop_in(peer, cx))),
                        )
                        .into_any_element();
                }
                if let Some(view) = room.watching.get(&peer).cloned() {
                    // Controls show while the pointer is over the tile.
                    let group = SharedString::from(format!("tile-{}", peer.short()));
                    let hover_bar = div()
                        .absolute()
                        .top_2()
                        .right_2()
                        .flex()
                        .gap_1()
                        .invisible()
                        .group_hover(group.clone(), |s| s.visible())
                        .child(
                            tile_button(
                                p,
                                &format!("focus-{}", peer.short()),
                                IconName::Maximize2,
                                if large { "Exit focus" } else { "Focus" },
                            )
                            .on_click(cx.listener(
                                move |this, _, _, cx| {
                                    if let Some(room) = this.room.as_mut() {
                                        room.focused = if room.focused == Some(peer) {
                                            None
                                        } else {
                                            Some(peer)
                                        };
                                    }
                                    cx.notify();
                                },
                            )),
                        )
                        .child(
                            tile_button(
                                p,
                                &format!("full-{}", peer.short()),
                                IconName::Expand,
                                "Fullscreen",
                            )
                            .on_click(cx.listener(
                                move |this, _, window, cx| this.enter_fullscreen(peer, window, cx),
                            )),
                        )
                        .when(!is_me, |d| {
                            d.child(
                                tile_button(
                                    p,
                                    &format!("pop-{}", peer.short()),
                                    IconName::ExternalLink,
                                    "Pop out",
                                )
                                .on_click(
                                    cx.listener(move |this, _, _, cx| this.pop_out(peer, cx)),
                                ),
                            )
                        })
                        .child(
                            tile_button(
                                p,
                                &format!("stop-{}", peer.short()),
                                IconName::X,
                                if is_me {
                                    "Hide preview"
                                } else {
                                    "Stop watching"
                                },
                            )
                            .on_click(
                                cx.listener(move |this, _, _, cx| this.stop_watching(peer, cx)),
                            ),
                        );
                    tile.group(group)
                        .child(view)
                        .child(
                            div()
                                .absolute()
                                .bottom_2()
                                .left_2()
                                .flex()
                                .gap_1()
                                .items_center()
                                .child(live_badge(p))
                                .child(name_tag_inline(p, &person.profile.name)),
                        )
                        .child(hover_bar)
                        .on_click(cx.listener(move |this, ev: &ClickEvent, _, cx| {
                            if ev.click_count() >= 2
                                && let Some(room) = this.room.as_mut()
                            {
                                room.focused = if room.focused == Some(peer) {
                                    None
                                } else {
                                    Some(peer)
                                };
                                cx.notify();
                            }
                        }))
                        .into_any_element()
                } else {
                    let brand = p.brand;
                    let brand_hover = p.brand_hover;
                    tile.bg(p.bg_sidebar)
                        .flex()
                        .flex_col()
                        .items_center()
                        .justify_center()
                        .gap_3()
                        .child(Avatar::new(&person.profile, 48.))
                        .child(
                            div()
                                .max_w_full()
                                .px_2()
                                .truncate()
                                .text_color(p.text_header)
                                .font_weight(FontWeight::SEMIBOLD)
                                .child(format!(
                                    "{} is streaming {}",
                                    person.profile.name, stream.source
                                )),
                        )
                        .child(div().text_xs().text_color(p.text_muted).child(format!(
                            "{}×{} · {} fps{}",
                            stream.width,
                            stream.height,
                            stream.fps,
                            if stream.has_audio { " · audio" } else { "" }
                        )))
                        .child(
                            div()
                                .id(SharedString::from(format!("watch-{}", peer.short())))
                                .px_4()
                                .py_2()
                                .rounded(px(4.))
                                .bg(brand)
                                .hover(move |s| s.bg(brand_hover))
                                .cursor_pointer()
                                .text_sm()
                                .font_weight(FontWeight::MEDIUM)
                                .text_color(gpui_kit::white())
                                .child("Watch Stream")
                                .on_click(cx.listener(move |this, _, _, cx| this.watch(peer, cx))),
                        )
                        .child(div().absolute().top_2().left_2().child(live_badge(p)))
                        .into_any_element()
                }
            }
        }
    }

    fn render_controls(&mut self, p: &Palette, cx: &mut Context<Self>) -> impl IntoElement {
        let me = self.me.id;
        let live = self
            .room
            .as_ref()
            .and_then(|r| r.participants.iter().find(|x| x.profile.id == me))
            .is_some_and(|x| x.stream.is_some());
        let camera = self
            .room
            .as_ref()
            .and_then(|r| r.participants.iter().find(|x| x.profile.id == me))
            .is_some_and(|x| x.camera);
        let ctrl =
            |id: &'static str, glyph: IconName, tip: &'static str, active: bool, danger: bool| {
                let (bg, hover) = if danger {
                    (p.red, p.red_hover)
                } else if active {
                    (gpui_kit::white(), gpui_kit::white().opacity(0.85))
                } else {
                    (p.bg_selected, p.bg_hover)
                };
                let fg = if active && !danger {
                    gpui_kit::black()
                } else {
                    gpui_kit::white()
                };
                div()
                    .id(id)
                    .w(px(if danger { 64. } else { 56. }))
                    .h(px(48.))
                    .rounded(px(if danger { 24. } else { 12. }))
                    .bg(bg)
                    .hover(move |s| s.bg(hover))
                    .cursor_pointer()
                    .flex()
                    .items_center()
                    .justify_center()
                    .tooltip(move |window, cx| Tooltip::new(tip).build(window, cx))
                    .child(icon(glyph, 24., fg))
            };
        div()
            .h(px(80.))
            .flex()
            .items_center()
            .justify_center()
            .gap_3()
            .child(
                ctrl(
                    "ctrl-camera",
                    if camera {
                        IconName::Video
                    } else {
                        IconName::VideoOff
                    },
                    if camera {
                        "Turn off camera"
                    } else {
                        "Turn on camera"
                    },
                    camera,
                    false,
                )
                .on_click(cx.listener(move |this, _, _, cx| this.toggle_camera(cx))),
            )
            .child(
                ctrl(
                    "ctrl-share",
                    if live {
                        IconName::ScreenShareOff
                    } else {
                        IconName::ScreenShare
                    },
                    if live {
                        "Stop streaming"
                    } else {
                        "Share your screen"
                    },
                    live,
                    false,
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    if live {
                        this.stop_streaming(cx);
                    } else {
                        this.open_go_live(cx);
                    }
                    cx.notify();
                })),
            )
            .child(
                ctrl(
                    "ctrl-mute",
                    if self.muted {
                        IconName::MicOff
                    } else {
                        IconName::Mic
                    },
                    if self.muted { "Unmute" } else { "Mute" },
                    false,
                    false,
                )
                .when(self.muted, |d| d.bg(p.red.opacity(0.3)))
                .on_click(cx.listener(|this, _, _, cx| {
                    this.set_muted(!this.muted);
                    cx.notify();
                })),
            )
            .child(
                ctrl(
                    "ctrl-invite",
                    IconName::UserPlus,
                    "Invite people",
                    false,
                    false,
                )
                .on_click(cx.listener(|this, _, _, cx| {
                    if let Some(link) = this.room.as_ref().map(|r| r.link.clone()) {
                        cx.write_to_clipboard(ClipboardItem::new_string(link));
                        this.link_copied = true;
                    }
                    this.invite_open = true;
                    cx.notify();
                })),
            )
            .child(
                ctrl("ctrl-leave", IconName::PhoneOff, "Disconnect", false, true)
                    .on_click(cx.listener(|this, _, _, cx| this.leave_room(cx))),
            )
    }

    fn render_chat(&mut self, p: &Palette, cx: &mut Context<Self>) -> impl IntoElement {
        let messages = self
            .room
            .as_ref()
            .map(|r| r.chat.clone())
            .unwrap_or_default();
        let mut groups: Vec<(PeerId, Vec<ChatMessage>)> = Vec::new();
        for m in messages {
            let system = matches!(m.body, ChatBody::System(_));
            // Consecutive messages from one author share a header; system
            // lines always stand alone.
            match groups.last_mut() {
                Some((author, list))
                    if *author == m.author
                        && !system
                        && !matches!(list[0].body, ChatBody::System(_)) =>
                {
                    list.push(m)
                }
                _ => groups.push((m.author, vec![m])),
            }
        }
        div()
            .w(px(360.))
            .h_full()
            .flex_shrink_0()
            .flex()
            .flex_col()
            .bg(p.bg_content)
            .border_l_1()
            .border_color(p.bg_rail)
            .child(
                div()
                    .h(px(48.))
                    .px_4()
                    .flex()
                    .items_center()
                    .gap_2()
                    .border_b_1()
                    .border_color(p.bg_rail)
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(p.text_header)
                    .child(icon(IconName::MessageSquare, 20., p.text_muted))
                    .child("Room chat"),
            )
            .child(
                div()
                    .id("chat-scroll")
                    .flex_1()
                    .overflow_y_scroll()
                    .py_2()
                    .flex()
                    .flex_col()
                    .gap_3()
                    .children(
                        groups
                            .into_iter()
                            .map(|(author, msgs)| self.chat_group(p, author, msgs, cx)),
                    ),
            )
            .child(
                div().p_4().child(
                    div()
                        .rounded(px(8.))
                        .bg(p.bg_hover)
                        .flex()
                        .items_center()
                        .gap_2()
                        .px_2()
                        .child(
                            icon_button(
                                "attach",
                                IconName::CirclePlus,
                                "Send a file",
                                p.interactive,
                                p,
                            )
                            .on_click(cx.listener(|this, _, _, cx| this.pick_files_to_send(cx))),
                        )
                        .child(
                            div()
                                .flex_1()
                                .child(Input::new(&self.chat_input).appearance(false)),
                        ),
                ),
            )
    }

    fn chat_group(
        &mut self,
        p: &Palette,
        author: PeerId,
        msgs: Vec<ChatMessage>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        if let Some(ChatBody::System(text)) = msgs.first().map(|m| m.body.clone()) {
            return div()
                .px_4()
                .flex()
                .items_center()
                .gap_2()
                .text_sm()
                .text_color(p.text_muted)
                .child(icon(IconName::Radio, 14., p.red))
                .child(text)
                .into_any_element();
        }
        let profile = self.profile_of(author);
        let name = profile
            .as_ref()
            .map(|p| p.name.clone())
            .unwrap_or_else(|| author.short());
        let color = profile
            .as_ref()
            .map(|p| crate::ui::color(p.color))
            .unwrap_or(p.text_header);
        div()
            .px_4()
            .flex()
            .gap_3()
            .children(profile.as_ref().map(|pr| Avatar::new(pr, 40.)))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(
                        div()
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(color)
                            .child(name),
                    )
                    .children(msgs.into_iter().map(|m| match m.body {
                        ChatBody::Text(t) => {
                            div().text_color(p.text_normal).child(t).into_any_element()
                        }
                        ChatBody::File(f) => self.file_card(p, m.id, f, cx).into_any_element(),
                        ChatBody::System(_) => div().into_any_element(),
                    })),
            )
            .into_any_element()
    }

    fn file_card(
        &mut self,
        p: &Palette,
        msg_id: u64,
        f: FileOffer,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let glyph = if f.is_image() {
            IconName::FileImage
        } else if f.mime.starts_with("video/") {
            IconName::FilePlay
        } else {
            IconName::FileText
        };
        let (status, progress) = match &f.transfer {
            Transfer::Available => (format::human_bytes(f.size), None),
            Transfer::Sent => (format!("{} · sent", format::human_bytes(f.size)), None),
            Transfer::Done => (
                format!("{} · saved to Downloads", format::human_bytes(f.size)),
                None,
            ),
            Transfer::Failed(e) => (format!("Failed: {e}"), None),
            Transfer::Downloading {
                done,
                bytes_per_sec,
            } => {
                let eta = format::eta_secs(f.size.saturating_sub(*done), *bytes_per_sec)
                    .map(format::duration)
                    .unwrap_or_else(|| "—".into());
                (
                    format!(
                        "{} of {} · {}/s · {eta} left",
                        format::human_bytes(*done),
                        format::human_bytes(f.size),
                        format::human_bytes(*bytes_per_sec)
                    ),
                    Some(*done as f32 / f.size.max(1) as f32),
                )
            }
        };
        let hash = f.hash;
        let done = f.transfer == Transfer::Done;
        let path = self.file_path(&hash);
        let action = match &f.transfer {
            Transfer::Available | Transfer::Failed(_) => Some((IconName::Download, "Download")),
            Transfer::Downloading { .. } => Some((IconName::X, "Cancel")),
            Transfer::Done => Some((IconName::FolderOpen, "Show in folder")),
            Transfer::Sent => path
                .as_ref()
                .map(|_| (IconName::FolderOpen, "Show in folder")),
        };
        let sent = f.transfer == Transfer::Sent;
        // Inline preview for images we have on disk.
        let preview = path
            .clone()
            .filter(|_| f.is_image() && (done || sent) && f.size <= crate::files::AUTO_PREVIEW_MAX);
        div()
            .max_w(px(300.))
            .p_3()
            .rounded(px(8.))
            .border_1()
            .border_color(p.bg_rail)
            .bg(p.bg_sidebar)
            .flex()
            .flex_col()
            .gap_2()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .child(icon(glyph, 30., p.brand))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .child(div().text_color(p.brand).truncate().child(f.name.clone()))
                            .child(div().text_xs().text_color(p.text_muted).child(status)),
                    )
                    .children(action.map(|(glyph, tip)| {
                        icon_button(
                            // The id includes the action so the tooltip refreshes when it changes.
                            SharedString::from(format!("file-{msg_id}-{tip}")),
                            glyph,
                            tip,
                            p.interactive,
                            p,
                        )
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if done || sent {
                                this.reveal_file(hash, cx);
                            } else {
                                this.toggle_download(hash, cx);
                            }
                        }))
                    })),
            )
            .children(progress.map(|frac| {
                div()
                    .h(px(4.))
                    .w_full()
                    .rounded_full()
                    .bg(p.bg_selected)
                    .child(div().h_full().w(relative(frac)).rounded_full().bg(p.brand))
            }))
            .children(preview.map(|path| {
                img(path)
                    .max_w(px(276.))
                    .max_h(px(200.))
                    .rounded(px(4.))
                    .object_fit(ObjectFit::Contain)
            }))
            .into_any_element()
    }
}

fn name_tag(p: &Palette, name: &str, muted: bool) -> impl IntoElement {
    div()
        .absolute()
        .bottom_2()
        .left_2()
        .flex()
        .items_center()
        .gap_1()
        .px_2()
        .py(px(2.))
        .rounded(px(4.))
        .bg(gpui_kit::black().opacity(0.5))
        .text_sm()
        .text_color(gpui_kit::white())
        .when(muted, |d| d.child(icon(IconName::MicOff, 14., p.red)))
        .child(name.to_string())
}

fn name_tag_inline(_p: &Palette, name: &str) -> impl IntoElement {
    div()
        .px_2()
        .py(px(2.))
        .rounded(px(4.))
        .bg(gpui_kit::black().opacity(0.5))
        .text_sm()
        .text_color(gpui_kit::white())
        .child(name.to_string())
}

fn tile_button(_p: &Palette, id: &str, glyph: IconName, tip: &'static str) -> Stateful<Div> {
    div()
        .id(SharedString::from(id.to_string()))
        .size(px(32.))
        .rounded(px(6.))
        .bg(gpui_kit::black().opacity(0.6))
        .hover(|s| s.bg(gpui_kit::black().opacity(0.85)))
        .cursor_pointer()
        .flex()
        .items_center()
        .justify_center()
        .tooltip(move |window, cx| Tooltip::new(tip).build(window, cx))
        .child(icon(glyph, 18., gpui_kit::white()))
}
