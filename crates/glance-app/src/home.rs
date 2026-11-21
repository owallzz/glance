//! Home screen: contacts sidebar, the contacts page (tabs) with start/join,
//! and the "Active now" column.

use glance_core::{Contact, ContactState, format};
use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::input::Input;
use gpui_kit::component::{Disableable, Sizable};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::app::{ContactsTab, GlanceApp};
use crate::dms::unread_badge;
use crate::theme::Palette;
use crate::ui::{Avatar, icon, icon_button, presence_color, section_label};

impl GlanceApp {
    pub fn render_home(
        &mut self,
        p: &Palette,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let page = match self.dm_open {
            Some(peer) if self.contacts.iter().any(|c| c.profile.id == peer) => {
                self.render_dm(p, peer, cx)
            }
            _ => div()
                .flex_1()
                .min_w_0()
                .h_full()
                .flex()
                .child(self.render_contacts_page(p, cx))
                .child(self.render_active_now(p, cx))
                .into_any_element(),
        };
        div()
            .flex_1()
            .min_w_0()
            .h_full()
            .flex()
            .child(self.render_home_sidebar(p, cx))
            .child(page)
    }

    fn render_home_sidebar(&mut self, p: &Palette, cx: &mut Context<Self>) -> impl IntoElement {
        let mut favorites: Vec<Contact> = self
            .contacts
            .iter()
            .filter(|c| c.state == ContactState::Mutual)
            .cloned()
            .collect();
        favorites.sort_by_key(|c| (!c.favorite, !c.presence.is_online()));
        let conversations = self.dm_list();
        let open = self.dm_open;
        // The open DM is highlighted in DIRECT MESSAGES; its contact row only
        // stands in while the conversation has no row there yet (no messages).
        let open_in_dms = open.is_some_and(|o| conversations.iter().any(|(pr, _, _)| pr.id == o));
        let contact_selected = move |pid| open == Some(pid) && !open_in_dms;
        div()
            .w(px(240.))
            .h_full()
            .flex()
            .flex_col()
            .bg(p.bg_sidebar)
            .child(
                div()
                    .h(px(48.))
                    .px_2()
                    .flex()
                    .items_center()
                    .border_b_1()
                    .border_color(p.bg_rail)
                    .child(
                        div()
                            .w_full()
                            .h(px(28.))
                            .px_2()
                            .rounded(px(4.))
                            .bg(p.bg_rail)
                            .flex()
                            .items_center()
                            .text_sm()
                            .text_color(p.text_muted)
                            .child("Find a contact"),
                    ),
            )
            .child(
                div()
                    .id("home-sidebar-list")
                    .flex_1()
                    .overflow_y_scroll()
                    .p_2()
                    .flex()
                    .flex_col()
                    .gap(px(2.))
                    .child(
                        div()
                            .id("side-contacts")
                            .cursor_pointer()
                            .child(sidebar_row(p, IconName::Users, "Contacts", open.is_none()))
                            .on_click(cx.listener(|this, _, _, cx| this.close_dm(cx))),
                    )
                    .when(!conversations.is_empty(), |d| {
                        d.child(
                            div()
                                .pt_4()
                                .pb_1()
                                .px_2()
                                .child(section_label(p, "DIRECT MESSAGES")),
                        )
                    })
                    .children(conversations.into_iter().map(|(profile, preview, unread)| {
                        let hover = p.bg_hover;
                        let pid = profile.id;
                        let selected = open == Some(pid);
                        div()
                            .id(SharedString::from(format!("dm-side-{}", pid.short())))
                            .h(px(42.))
                            .px_2()
                            .rounded(px(4.))
                            .flex()
                            .items_center()
                            .gap_3()
                            .cursor_pointer()
                            .when(selected, |d| d.bg(p.bg_selected))
                            .hover(move |s| s.bg(hover))
                            .on_click(
                                cx.listener(move |this, _, window, cx| {
                                    this.open_dm(pid, window, cx)
                                }),
                            )
                            .child(Avatar::new(&profile, 32.))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .flex()
                                    .flex_col()
                                    .child(
                                        div()
                                            .text_color(if unread > 0 || selected {
                                                p.text_header
                                            } else {
                                                p.text_normal
                                            })
                                            .when(unread > 0, |d| {
                                                d.font_weight(FontWeight::SEMIBOLD)
                                            })
                                            .truncate()
                                            .child(profile.name.clone()),
                                    )
                                    .child(
                                        div()
                                            .text_xs()
                                            .text_color(p.text_muted)
                                            .truncate()
                                            .child(preview),
                                    ),
                            )
                            .when(unread > 0, |d| d.child(unread_badge(p, unread)))
                    }))
                    .child(
                        div()
                            .pt_4()
                            .pb_1()
                            .px_2()
                            .child(section_label(p, "CONTACTS")),
                    )
                    .children(favorites.into_iter().map(|c| {
                        let hover = p.bg_hover;
                        let sub = match c.in_call {
                            Some(n) => format!("In a call ({n})"),
                            None => c.presence.label().to_string(),
                        };
                        let pid = c.profile.id;
                        let unread = self.dms.unread(pid);
                        div()
                            .id(SharedString::from(format!("side-{}", c.profile.id.short())))
                            .h(px(42.))
                            .px_2()
                            .rounded(px(4.))
                            .flex()
                            .items_center()
                            .gap_3()
                            .cursor_pointer()
                            .when(contact_selected(pid), |d| d.bg(p.bg_selected))
                            .hover(move |s| s.bg(hover))
                            .on_click(
                                cx.listener(move |this, _, window, cx| {
                                    this.open_dm(pid, window, cx)
                                }),
                            )
                            .when(!c.presence.is_online(), |d| d.opacity(0.5))
                            .child(
                                Avatar::new(&c.profile, 32.)
                                    .status(presence_color(p, c.presence), p.bg_sidebar),
                            )
                            .child(
                                div()
                                    .min_w_0()
                                    .flex()
                                    .flex_col()
                                    .child(
                                        div()
                                            .text_color(p.text_normal)
                                            .truncate()
                                            .child(c.display_name().to_string()),
                                    )
                                    .child(div().text_xs().text_color(p.text_muted).child(sub)),
                            )
                            .child(div().flex_1())
                            .when(unread > 0, |d| d.child(unread_badge(p, unread)))
                    })),
            )
            .child(self.render_user_panel(p, cx))
    }

    fn render_contacts_page(&mut self, p: &Palette, cx: &mut Context<Self>) -> impl IntoElement {
        let pending = self
            .contacts
            .iter()
            .filter(|c| matches!(c.state, ContactState::Incoming | ContactState::Outgoing))
            .count();
        let tab = |this: &Self, id: &'static str, t: ContactsTab, label: String| {
            let selected = this.contacts_tab == t;
            let hover = p.bg_hover;
            div()
                .id(id)
                .px_2()
                .py(px(2.))
                .rounded(px(4.))
                .cursor_pointer()
                .text_color(if selected {
                    p.text_header
                } else {
                    p.text_muted
                })
                .when(selected, |d| d.bg(p.bg_selected))
                .hover(move |s| s.bg(hover))
                .child(label)
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.contacts_tab = t;
                    cx.notify();
                }))
        };
        let visible: Vec<Contact> = self
            .contacts
            .iter()
            .filter(|c| match self.contacts_tab {
                ContactsTab::Online => c.state == ContactState::Mutual && c.presence.is_online(),
                ContactsTab::All => c.state == ContactState::Mutual,
                ContactsTab::Pending => {
                    matches!(c.state, ContactState::Incoming | ContactState::Outgoing)
                }
                ContactsTab::Blocked => c.state == ContactState::Blocked,
            })
            .cloned()
            .collect();
        let title = match self.contacts_tab {
            ContactsTab::Online => format!("ONLINE — {}", visible.len()),
            ContactsTab::All => format!("ALL CONTACTS — {}", visible.len()),
            ContactsTab::Pending => format!("PENDING — {}", visible.len()),
            ContactsTab::Blocked => format!("BLOCKED — {}", visible.len()),
        };

        div()
            .flex_1()
            .min_w_0()
            .h_full()
            .flex()
            .flex_col()
            .bg(p.bg_content)
            // Header with tabs
            .child(
                div()
                    .h(px(48.))
                    .px_4()
                    .flex()
                    .items_center()
                    .gap_4()
                    .border_b_1()
                    .border_color(p.bg_rail)
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(p.text_header)
                            .child(icon(IconName::Users, 20., p.text_muted))
                            .child("Contacts"),
                    )
                    .child(div().w(px(1.)).h(px(24.)).bg(p.bg_selected))
                    .child(tab(self, "tab-online", ContactsTab::Online, "Online".into()))
                    .child(tab(self, "tab-all", ContactsTab::All, "All".into()))
                    .child(tab(
                        self,
                        "tab-pending",
                        ContactsTab::Pending,
                        if pending > 0 { format!("Pending ({pending})") } else { "Pending".into() },
                    ))
                    .child(tab(self, "tab-blocked", ContactsTab::Blocked, "Blocked".into())),
            )
            // Start / join
            .child(
                div()
                    .px_6()
                    .pt_5()
                    .pb_4()
                    .flex()
                    .flex_col()
                    .gap_3()
                    .border_b_1()
                    .border_color(p.bg_selected)
                    .when(self.needs_profile, |d| {
                        d.child(
                            div()
                                .p_4()
                                .mb_2()
                                .rounded(px(8.))
                                .bg(p.bg_sidebar)
                                .flex()
                                .flex_col()
                                .gap_2()
                                .child(
                                    div()
                                        .font_weight(FontWeight::BOLD)
                                        .text_color(p.text_header)
                                        .child("Welcome to glance! What should people call you?"),
                                )
                                .child(
                                    div()
                                        .flex()
                                        .gap_2()
                                        .child(div().flex_1().child(Input::new(&self.name_input)))
                                        .child(
                                            Button::new("save-name")
                                                .primary()
                                                .label("Continue")
                                                .on_click(cx.listener(|this, _, _, cx| this.save_name(cx))),
                                        ),
                                ),
                        )
                    })
                    .child(
                        div()
                            .font_weight(FontWeight::BOLD)
                            .text_color(p.text_header)
                            .child("START A CALL"),
                    )
                    .child(
                        div()
                            .text_sm()
                            .text_color(p.text_muted)
                            .child("Start a room and the invite link is copied for you. Or paste a link someone sent you."),
                    )
                    .child(
                        div()
                            .flex()
                            .gap_2()
                            .child(
                                Button::new("start-room")
                                    .success()
                                    .disabled(self.needs_profile || self.busy)
                                    .loading(self.busy)
                                    .icon(IconName::Plus)
                                    .label("Start a room")
                                    .on_click(cx.listener(|this, _, _, cx| this.start_room(cx))),
                            )
                            .child(div().flex_1().child(Input::new(&self.join_input)))
                            .child(
                                Button::new("join-room")
                                    .primary()
                                    .disabled(self.needs_profile || self.busy)
                                    .label("Join")
                                    .on_click(cx.listener(|this, _, window, cx| this.join_from_input(window, cx))),
                            ),
                    )
                    .children(self.join_error.clone().map(|e| {
                        div().text_sm().text_color(p.red).child(e)
                    })),
            )
            .children(self.render_call_ended(p, cx))
            // List
            .child(
                div()
                    .id("contacts-list")
                    .flex_1()
                    .overflow_y_scroll()
                    .px_6()
                    .pt_4()
                    .child(div().pb_2().child(section_label(p, title)))
                    .children(visible.into_iter().map(|c| self.contact_row(p, c, cx)))
                    .when(self.contacts_tab == ContactsTab::Blocked, |d| {
                        d.child(
                            div()
                                .pt_8()
                                .text_center()
                                .text_color(p.text_muted)
                                .child("You haven't blocked anyone."),
                        )
                    }),
            )
    }

    fn contact_row(&mut self, p: &Palette, c: Contact, cx: &mut Context<Self>) -> AnyElement {
        let id = c.profile.id.short();
        let hover = p.bg_hover;
        let status = match (c.state, c.in_call) {
            (ContactState::Incoming, _) => "Incoming contact request".to_string(),
            (ContactState::Outgoing, _) => "Outgoing contact request".to_string(),
            (_, Some(n)) => format!("In a call with {n} people"),
            _ => c.presence.label().to_string(),
        };
        let action = |name: IconName, tip: &'static str, suffix: &str| {
            icon_button(
                SharedString::from(format!("{suffix}-{id}")),
                name,
                tip,
                p.interactive,
                p,
            )
            .size(px(36.))
            .rounded_full()
            .bg(p.bg_sidebar)
        };
        let pid = c.profile.id;
        let can_ring = c.presence.can_ring();
        let online = c.presence.is_online();
        let actions = match c.state {
            ContactState::Incoming => div()
                .flex()
                .gap_2()
                .child(
                    action(IconName::Check, "Accept", "accept")
                        .on_click(cx.listener(move |this, _, _, cx| this.accept_contact(pid, cx))),
                )
                .child(
                    action(IconName::X, "Ignore", "ignore")
                        .on_click(cx.listener(move |this, _, _, cx| this.remove_contact(pid, cx))),
                ),
            ContactState::Outgoing => div().flex().child(
                action(IconName::X, "Cancel request", "cancel")
                    .on_click(cx.listener(move |this, _, _, cx| this.remove_contact(pid, cx))),
            ),
            ContactState::Blocked => div().flex().child(
                Button::new(SharedString::from(format!("unblock-{id}")))
                    .small()
                    .label("Unblock")
                    .on_click(cx.listener(move |this, _, _, cx| this.unblock_contact(pid, cx))),
            ),
            ContactState::Mutual => div()
                .flex()
                .gap_2()
                .when(c.joinable && c.in_call.is_some(), |d| {
                    d.child(
                        Button::new(SharedString::from(format!("join-{id}")))
                            .success()
                            .small()
                            .label("Join")
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.request_join(pid);
                                cx.notify();
                            })),
                    )
                })
                .child(
                    action(IconName::MessageSquare, "Message", "message").on_click(
                        cx.listener(move |this, _, window, cx| this.open_dm(pid, window, cx)),
                    ),
                )
                .when(can_ring, |d| {
                    d.child(action(IconName::Phone, "Call", "call").on_click(
                        cx.listener(move |this, _, _, cx| this.call_contacts(vec![pid], cx)),
                    ))
                })
                .when(online, |d| {
                    d.child(
                        action(IconName::FileText, "Send file", "sendfile").on_click(
                            cx.listener(move |this, _, _, cx| this.send_file_to_contact(pid, cx)),
                        ),
                    )
                })
                .child(
                    action(IconName::EllipsisVertical, "More", "more").on_click(cx.listener(
                        move |this, _, _, cx| {
                            this.contact_menu = if this.contact_menu == Some(pid) {
                                None
                            } else {
                                Some(pid)
                            };
                            cx.notify();
                        },
                    )),
                ),
        };
        let menu_open = self.contact_menu == Some(pid);
        let mutual = c.state == ContactState::Mutual;
        let unread = self.dms.unread(pid);
        let favorite = c.favorite;
        let menu_item = |key: &str, label: &'static str, danger: bool| {
            let hover = p.bg_hover;
            div()
                .id(SharedString::from(format!("{key}-{id}")))
                .px_3()
                .py_1()
                .rounded(px(4.))
                .cursor_pointer()
                .text_sm()
                .text_color(if danger { p.red } else { p.text_normal })
                .hover(move |s| s.bg(hover))
                .child(label)
        };
        let menu = div()
            .ml(px(52.))
            .mb_2()
            .p_1()
            .rounded(px(6.))
            .bg(p.bg_rail)
            .flex()
            .gap_1()
            .child(menu_item("msg", "Message", false).on_click(cx.listener(
                move |this, _, window, cx| {
                    this.contact_menu = None;
                    this.open_dm(pid, window, cx);
                },
            )))
            .child(
                menu_item(
                    "fav",
                    if favorite { "Unfavorite" } else { "Favorite" },
                    false,
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.toggle_favorite(pid, cx);
                    this.contact_menu = None;
                })),
            )
            .child(menu_item("nick", "Nickname", false).on_click(cx.listener(
                move |this, _, window, cx| {
                    this.contact_menu = None;
                    this.edit_nickname(pid, window, cx);
                },
            )))
            .child(
                menu_item("rm", "Remove contact", true).on_click(cx.listener(
                    move |this, _, _, cx| {
                        this.remove_contact(pid, cx);
                        this.contact_menu = None;
                    },
                )),
            )
            .child(menu_item("block", "Block", true).on_click(cx.listener(
                move |this, _, _, cx| {
                    this.block_contact(pid, cx);
                    this.contact_menu = None;
                },
            )));
        let row = div()
            .id(SharedString::from(format!("contact-{id}")))
            .h(px(62.))
            .px_2()
            .border_t_1()
            .border_color(p.bg_selected)
            .rounded(px(8.))
            .flex()
            .items_center()
            .gap_3()
            .hover(move |s| s.bg(hover))
            .child(Avatar::new(&c.profile, 32.).status(presence_color(p, c.presence), p.bg_content))
            .child(
                div()
                    .id(SharedString::from(format!("contact-open-{id}")))
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .justify_center()
                    .flex()
                    .flex_col()
                    .when(mutual, |d| {
                        d.cursor_pointer().on_click(
                            cx.listener(move |this, _, window, cx| this.open_dm(pid, window, cx)),
                        )
                    })
                    .child(
                        div()
                            .flex()
                            .gap_1()
                            .items_center()
                            .child(
                                div()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(p.text_header)
                                    .child(c.display_name().to_string()),
                            )
                            .when(c.favorite, |d| d.child(icon(IconName::Star, 12., p.yellow)))
                            .when(unread > 0, |d| d.child(unread_badge(p, unread))),
                    )
                    .child(div().text_sm().text_color(p.text_muted).child(status)),
            )
            .child(actions);
        let editing = self.editing_nickname == Some(pid);
        let nickname_row = div()
            .ml(px(52.))
            .mb_2()
            .flex()
            .gap_2()
            .items_center()
            .child(
                div()
                    .flex_1()
                    .child(Input::new(&self.nickname_input).small()),
            )
            .child(
                Button::new(SharedString::from(format!("nick-save-{id}")))
                    .small()
                    .primary()
                    .label("Save")
                    .on_click(cx.listener(|this, _, _, cx| this.save_nickname(cx))),
            )
            .child(
                Button::new(SharedString::from(format!("nick-cancel-{id}")))
                    .small()
                    .ghost()
                    .label("Cancel")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.editing_nickname = None;
                        cx.notify();
                    })),
            );
        div()
            .flex()
            .flex_col()
            .child(row)
            .when(menu_open, |d| d.child(menu))
            .when(editing, |d| d.child(nickname_row))
            .into_any_element()
    }

    /// After a call: offer to add the people you met.
    fn render_call_ended(&mut self, p: &Palette, cx: &mut Context<Self>) -> Option<AnyElement> {
        if self.last_call.is_empty() {
            return None;
        }
        let people = self.last_call.clone();
        Some(
            div()
                .mx_6()
                .mt_4()
                .p_4()
                .rounded(px(8.))
                .bg(p.bg_sidebar)
                .flex()
                .flex_col()
                .gap_2()
                .child(
                    div()
                        .flex()
                        .justify_between()
                        .child(
                            div()
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(p.text_header)
                                .child("Call ended. Add the people you talked to?"),
                        )
                        .child(
                            icon_button("dismiss-call", IconName::X, "Dismiss", p.interactive, p)
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.last_call.clear();
                                    cx.notify();
                                })),
                        ),
                )
                .children(people.into_iter().map(|person| {
                    let pid = person.id;
                    let sent = self.book.state(&pid.0).is_some();
                    div()
                        .flex()
                        .items_center()
                        .gap_3()
                        .child(Avatar::new(&person, 28.))
                        .child(
                            div()
                                .flex_1()
                                .text_color(p.text_normal)
                                .child(person.name.clone()),
                        )
                        .child(
                            Button::new(SharedString::from(format!("add-{}", pid.short())))
                                .small()
                                .primary()
                                .disabled(sent)
                                .label(if sent { "Request sent" } else { "Add contact" })
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.add_contact(person.clone(), cx)
                                })),
                        )
                }))
                .into_any_element(),
        )
    }

    fn render_active_now(&mut self, p: &Palette, cx: &mut Context<Self>) -> impl IntoElement {
        let files: Vec<AnyElement> = (0..self.direct_files.len())
            .map(|i| self.direct_file_card(p, i, cx))
            .collect();
        let recent_rooms = self.render_recent_rooms(p, cx);
        let in_calls: Vec<&Contact> = self
            .contacts
            .iter()
            .filter(|c| c.in_call.is_some())
            .collect();
        div()
            .id("active-now")
            .overflow_y_scroll()
            .w(px(320.))
            .h_full()
            .flex_shrink_0()
            .border_l_1()
            .border_color(p.bg_selected)
            .p_4()
            .flex()
            .flex_col()
            .gap_4()
            .child(
                div()
                    .text_xl()
                    .font_weight(FontWeight::BOLD)
                    .text_color(p.text_header)
                    .child("Active Now"),
            )
            .children(in_calls.into_iter().map(|c| {
                div()
                    .p_4()
                    .rounded(px(8.))
                    .bg(p.bg_sidebar)
                    .flex()
                    .items_center()
                    .gap_3()
                    .child(Avatar::new(&c.profile, 32.).status(p.green, p.bg_sidebar))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .child(
                                div()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(p.text_header)
                                    .child(c.display_name().to_string()),
                            )
                            .child(
                                div().text_sm().text_color(p.text_muted).child(format!(
                                    "In a call · {} people",
                                    c.in_call.unwrap_or(0)
                                )),
                            ),
                    )
            }))
            .children(recent_rooms)
            .when(!files.is_empty(), |d| {
                d.child(div().pt_2().child(section_label(p, "FILES")))
                    .children(files)
            })
            .child(div().pt_2().child(section_label(p, "RECENT CALLS")))
            .children(self.calls.iter().map(|call| {
                let names: Vec<String> = call
                    .with
                    .iter()
                    .filter_map(|id| self.profile_of(*id).map(|p| p.name))
                    .collect();
                let (glyph, color, sub) = if call.missed {
                    (IconName::PhoneMissed, p.red, "Missed call".to_string())
                } else {
                    (
                        IconName::PhoneCall,
                        p.green,
                        format::duration(call.duration_secs),
                    )
                };
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .child(icon(glyph, 18., color))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .child(div().text_color(p.text_normal).child(names.join(", ")))
                            .child(div().text_xs().text_color(p.text_muted).child(sub)),
                    )
            }))
    }
}

fn sidebar_row(
    p: &Palette,
    glyph: IconName,
    label: &'static str,
    selected: bool,
) -> impl IntoElement {
    div()
        .h(px(42.))
        .px_3()
        .rounded(px(4.))
        .flex()
        .items_center()
        .gap_3()
        .when(selected, |d| d.bg(p.bg_selected))
        .text_color(if selected {
            p.text_header
        } else {
            p.text_muted
        })
        .font_weight(FontWeight::MEDIUM)
        .child(icon(
            glyph,
            22.,
            if selected {
                p.text_header
            } else {
                p.text_muted
            },
        ))
        .child(label)
}
