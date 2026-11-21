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

/// What's shown instead of the invite link until you ask to see it (the
/// link is the key to the room, so it shouldn't sit on screen in a share).
pub fn masked_link() -> String {
    "glance://join/••••••••".into()
}

/// "Copy" that turns into "Copied" for a moment.
pub fn copy_button(id: &'static str, copied: bool, cx: &mut Context<GlanceApp>) -> Button {
    let button = Button::new(id)
        .small()
        .label(if copied { "Copied" } else { "Copy" })
        .on_click(cx.listener(|this, _, _, cx| this.copy_invite_link(cx)));
    if copied {
        button.success()
    } else {
        button.primary()
    }
}

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
    /// Copy the room's invite link and say so for a couple of seconds.
    pub fn copy_invite_link(&mut self, cx: &mut Context<Self>) {
        let Some(link) = self.room.as_ref().map(|r| r.link.clone()) else {
            return;
        };
        cx.write_to_clipboard(ClipboardItem::new_string(link));
        self.link_copied = true;
        self.link_copied_gen = self.link_copied_gen.wrapping_add(1);
        let generation = self.link_copied_gen;
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(1500))
                .await;
            let _ = this.update(cx, |app, cx| {
                // A later copy restarts the timer.
                if app.link_copied_gen == generation {
                    app.link_copied = false;
                    cx.notify();
                }
            });
        })
        .detach();
        cx.notify();
    }

    /// A short message at the bottom of the window for a few seconds.
    pub fn show_toast(&mut self, text: impl Into<SharedString>, cx: &mut Context<Self>) {
        self.toast = Some(text.into());
        self.toast_gen = self.toast_gen.wrapping_add(1);
        let generation = self.toast_gen;
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_secs(3))
                .await;
            let _ = this.update(cx, |app, cx| {
                if app.toast_gen == generation {
                    app.toast = None;
                    cx.notify();
                }
            });
        })
        .detach();
        cx.notify();
    }

    pub fn render_toast(&self, p: &Palette) -> Option<AnyElement> {
        let text = self.toast.clone()?;
        Some(
            div()
                .absolute()
                .bottom(px(24.))
                .left_0()
                .right_0()
                .flex()
                .justify_center()
                .child(
                    div()
                        .px_4()
                        .py_2()
                        .rounded(px(8.))
                        .bg(p.bg_rail)
                        .border_1()
                        .border_color(p.bg_selected)
                        .shadow_lg()
                        .flex()
                        .items_center()
                        .gap_2()
                        .text_sm()
                        .text_color(p.text_header)
                        .child(icon(IconName::CopyCheck, 16., p.green))
                        .child(text),
                )
                .into_any_element(),
        )
    }

    /// The invite link: hidden until you ask to see it, with a Copy button.
    pub fn render_invite_card(
        &mut self,
        p: &Palette,
        link: String,
        copied: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let revealed = self.link_revealed;
        let hover = p.bg_hover;
        div()
            .id("invite-link")
            .mx_2()
            .mb_2()
            .p_3()
            .rounded(px(8.))
            .bg(p.bg_content)
            .flex()
            .flex_col()
            .gap_2()
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(section_label(p, "INVITE LINK"))
                    .child(
                        icon_button(
                            "invite-reveal",
                            if revealed {
                                IconName::EyeOff
                            } else {
                                IconName::Eye
                            },
                            if revealed {
                                "Hide invite link"
                            } else {
                                "Show invite link"
                            },
                            p.interactive,
                            p,
                        )
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.link_revealed = !this.link_revealed;
                            cx.notify();
                        })),
                    ),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .id("invite-link-text")
                            .flex_1()
                            .min_w_0()
                            .px_1()
                            .rounded(px(4.))
                            .cursor_pointer()
                            .hover(move |s| s.bg(hover))
                            .text_xs()
                            .text_color(if revealed {
                                p.text_normal
                            } else {
                                p.text_muted
                            })
                            .truncate()
                            .when(!revealed, |d| {
                                d.tooltip(|window, cx| {
                                    Tooltip::new("Show invite link").build(window, cx)
                                })
                            })
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.link_revealed = !this.link_revealed;
                                cx.notify();
                            }))
                            .child(if revealed { link } else { masked_link() }),
                    )
                    .child(copy_button("invite-copy-side", copied, cx)),
            )
            .into_any_element()
    }

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
            .children(self.render_person_menu(p, cx))
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
                            this.copy_invite_link(cx);
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
            .child(self.render_invite_card(p, link, copied, cx))
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
        let muted_by_you = id != self.me.id && self.user_muted(id);
        // Someone you muted still shows as talking, dimmed.
        let ring = person.speaking.then_some(if muted_by_you {
            p.green.opacity(0.4)
        } else {
            p.green
        });
        // No row tooltip while a context menu is open: it would sit on top
        // of the menu's first item.
        let menu_open = self.room.as_ref().is_some_and(|r| r.menu.is_some());
        let route = person
            .connection
            .filter(|_| !menu_open)
            .map(|c| match c.route {
                Route::Direct => format!("Direct · {} ms", c.rtt_ms),
                Route::Relayed => format!("Relayed · {} ms", c.rtt_ms),
            });
        div()
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
            .when(muted_by_you, |d| d.child(muted_by_you_icon(p, id, 14.)))
            .when(person.muted, |d| {
                d.child(icon(IconName::MicOff, 14., p.red))
            })
            .when(person.deafened, |d| {
                d.child(icon(IconName::HeadphoneOff, 14., p.red))
            })
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, ev: &MouseDownEvent, window, cx| {
                    this.open_person_menu(id, ev.position, window, cx)
                }),
            )
            .into_any_element()
    }

    fn render_stage(&mut self, _p: &Palette, cx: &mut Context<Self>) -> impl IntoElement {
        // The stage is dark in both themes (like Discord's call view), so
        // video and the control bar read the same everywhere.
        let p = &Palette::dark();
        let room = self.room.as_ref().expect("in a room");
        let chat_open = room.chat_open;
        let filmstrip_hidden = room.filmstrip_hidden;
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

        let body = match focused {
            Some(ix) => {
                let main = tiles.remove(ix);
                // Like Discord's focused stream: a toggle under the stream
                // collapses the strip of everyone else.
                let hidden = filmstrip_hidden;
                let toggle_hover = p.bg_hover;
                let toggle = div()
                    .id("toggle-filmstrip")
                    .h(px(20.))
                    .px_3()
                    .rounded_full()
                    .bg(p.bg_selected)
                    .hover(move |s| s.bg(toggle_hover))
                    .cursor_pointer()
                    .flex()
                    .items_center()
                    .gap_1()
                    .text_xs()
                    .text_color(p.text_muted)
                    .child(icon(
                        if hidden {
                            IconName::ChevronUp
                        } else {
                            IconName::ChevronDown
                        },
                        14.,
                        p.text_muted,
                    ))
                    .child(if hidden {
                        format!("Show participants ({})", tiles.len())
                    } else {
                        "Hide participants".to_string()
                    })
                    .on_click(cx.listener(|this, _, _, cx| {
                        if let Some(room) = this.room.as_mut() {
                            room.filmstrip_hidden = !room.filmstrip_hidden;
                        }
                        cx.notify();
                    }));
                let has_others = !tiles.is_empty();
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .p_2()
                    .child(div().flex_1().min_h_0().child(self.tile(p, main, true, cx)))
                    .when(has_others, |d| {
                        d.child(div().flex().justify_center().child(toggle))
                    })
                    .when(has_others && !hidden, |d| {
                        d.child(div().h(px(120.)).flex().justify_center().gap_2().children(
                            tiles.into_iter().map(|t| {
                                div().w(px(213.)).h_full().child(self.tile(p, t, false, cx))
                            }),
                        ))
                    })
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
            .overflow_hidden()
            // The same menu as right-clicking them in the member list.
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, ev: &MouseDownEvent, window, cx| {
                    this.open_person_menu(peer, ev.position, window, cx)
                }),
            );
        match kind {
            TileKind::Person(person) => {
                let speaking = person.speaking;
                let is_me = peer == self.me.id;
                let muted_by_you = !is_me && self.user_muted(peer);
                let ring = if muted_by_you {
                    p.green.opacity(0.4)
                } else {
                    p.green
                };
                let tag = name_tag(
                    p,
                    &person.profile.name,
                    person.muted,
                    person.deafened,
                    muted_by_you,
                );
                // Hover button: mute/unmute them for you.
                let group = SharedString::from(format!("ptile-{}", peer.short()));
                let hover_bar = (!is_me).then(|| {
                    div()
                        .absolute()
                        .top_2()
                        .right_2()
                        .invisible()
                        .group_hover(group.clone(), |s| s.visible())
                        .child(
                            tile_button(
                                p,
                                &format!("tmute-{}-{muted_by_you}", peer.short()),
                                if muted_by_you {
                                    IconName::Volume2
                                } else {
                                    IconName::VolumeX
                                },
                                if muted_by_you { "Unmute" } else { "Mute" },
                            )
                            .on_click(cx.listener(
                                move |this, _, _, cx| {
                                    this.set_user_muted(peer, !muted_by_you);
                                    cx.notify();
                                },
                            )),
                        )
                });
                let tile = tile.group(group);
                if let Some(cam) = room.cameras.get(&peer).cloned() {
                    return tile
                        .bg(gpui_kit::black())
                        .when(speaking, |d| d.border_2().border_color(ring))
                        .child(cam)
                        .child(tag)
                        .children(hover_bar)
                        .into_any_element();
                }
                tile.bg(crate::ui::color(person.profile.color).opacity(0.35))
                    .bg(p.bg_sidebar)
                    .when(speaking, |d| d.border_2().border_color(ring))
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(Avatar::new(&person.profile, if large { 96. } else { 64. }))
                    .child(tag)
                    .children(hover_bar)
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
                    let stream_muted = self.stream_muted(peer);
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
                        .when(!is_me && stream.has_audio, |d| {
                            d.child(
                                tile_button(
                                    p,
                                    &format!("smute-{}-{stream_muted}", peer.short()),
                                    if stream_muted {
                                        IconName::VolumeX
                                    } else {
                                        IconName::Volume2
                                    },
                                    if stream_muted {
                                        "Unmute stream audio"
                                    } else {
                                        "Mute stream audio"
                                    },
                                )
                                .on_click(cx.listener(
                                    move |this, _, _, cx| {
                                        this.set_stream_muted(peer, !stream_muted);
                                        cx.notify();
                                    },
                                )),
                            )
                        })
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
            // While live: change quality, sound or source without stopping.
            .when(live, |d| {
                d.child(
                    ctrl(
                        "ctrl-stream-settings",
                        IconName::SlidersHorizontal,
                        "Stream settings",
                        false,
                        false,
                    )
                    .on_click(cx.listener(|this, _, _, cx| this.open_stream_settings(cx))),
                )
            })
            .child(
                ctrl(
                    "ctrl-mute",
                    if self.voice.muted {
                        IconName::MicOff
                    } else {
                        IconName::Mic
                    },
                    if self.voice.muted { "Unmute" } else { "Mute" },
                    false,
                    false,
                )
                .when(self.voice.muted, |d| d.bg(p.red.opacity(0.3)))
                .on_click(cx.listener(|this, _, _, cx| {
                    this.set_muted(!this.voice.muted);
                    cx.notify();
                })),
            )
            .child(
                ctrl(
                    // The id changes with the action so the tooltip refreshes.
                    if self.voice.deafened {
                        "ctrl-undeafen"
                    } else {
                        "ctrl-deafen"
                    },
                    if self.voice.deafened {
                        IconName::HeadphoneOff
                    } else {
                        IconName::Headphones
                    },
                    if self.voice.deafened {
                        "Undeafen"
                    } else {
                        "Deafen"
                    },
                    false,
                    false,
                )
                .when(self.voice.deafened, |d| d.bg(p.red.opacity(0.3)))
                .on_click(cx.listener(|this, _, _, cx| {
                    this.set_deafened(!this.voice.deafened);
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
                    this.copy_invite_link(cx);
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
                    .children({
                        // Selection order across the whole list.
                        let mut order = 0;
                        let scope = crate::markdown_view::ChatScope::room(
                            self.room.as_ref().map_or("", |r| r.link.as_str()),
                        );
                        groups
                            .into_iter()
                            .map(|(author, msgs)| {
                                self.chat_group(p, scope, author, msgs, &mut order, cx)
                            })
                            .collect::<Vec<_>>()
                    }),
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
                        .child(div().flex_1().child(
                            Input::new(&self.chat_input).appearance(false).on_paste({
                                // Pasted files or images are sent; text pastes as usual.
                                let app = cx.entity().downgrade();
                                move |item, _, cx| {
                                    let paths = crate::files::pasted_files(item);
                                    if paths.is_empty() {
                                        return false;
                                    }
                                    let _ = app.update(cx, |app, cx| app.send_files(paths, cx));
                                    true
                                }
                            }),
                        )),
                ),
            )
    }

    fn chat_group(
        &mut self,
        p: &Palette,
        scope: crate::markdown_view::ChatScope,
        author: PeerId,
        msgs: Vec<ChatMessage>,
        order: &mut u64,
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
        let mut bodies = Vec::with_capacity(msgs.len());
        for m in msgs {
            bodies.push(match m.body {
                // Drag to select, then copy with the usual shortcut.
                ChatBody::Text(t) => {
                    let msg = self.md_cache.get(&t);
                    let view = crate::markdown_view::MessageView {
                        scope,
                        msg_id: m.id,
                        order: &mut *order,
                        revealed: &self.revealed_spoilers,
                    };
                    div()
                        .text_color(p.text_normal)
                        .child(crate::markdown_view::render_message(&msg, view, cx))
                        .into_any_element()
                }
                ChatBody::File(f) => self.file_card(p, m.id, f, cx).into_any_element(),
                ChatBody::System(_) => div().into_any_element(),
            });
        }
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
                    .children(bodies),
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
            Transfer::Unavailable => (
                format!(
                    "{} · unavailable (nobody here has it)",
                    format::human_bytes(f.size)
                ),
                None,
            ),
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
            Transfer::Unavailable => None,
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

fn name_tag(
    p: &Palette,
    name: &str,
    muted: bool,
    deafened: bool,
    muted_by_you: bool,
) -> impl IntoElement {
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
        .when(muted_by_you, |d| {
            d.child(icon(IconName::VolumeX, 14., p.muted_by_you))
        })
        .when(muted, |d| d.child(icon(IconName::MicOff, 14., p.red)))
        .when(deafened, |d| {
            d.child(icon(IconName::HeadphoneOff, 14., p.red))
        })
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

/// "Muted by you": a volume-off icon in its own colour (their own mute is a
/// red mic).
fn muted_by_you_icon(p: &Palette, id: PeerId, size: f32) -> AnyElement {
    div()
        .id(SharedString::from(format!("mbu-{}", id.short())))
        .tooltip(|window, cx| Tooltip::new("Muted by you").build(window, cx))
        .child(icon(IconName::VolumeX, size, p.muted_by_you))
        .into_any_element()
}
