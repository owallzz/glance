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
use crate::theme::Palette;
use crate::ui::{Avatar, icon, icon_button, presence_color, section_label};

impl GlanceApp {
    pub fn render_home(
        &mut self,
        p: &Palette,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        div()
            .flex_1()
            .min_w_0()
            .h_full()
            .flex()
            .child(self.render_home_sidebar(p, cx))
            .child(self.render_contacts_page(p, cx))
            .child(self.render_active_now(p))
    }

    fn render_home_sidebar(&mut self, p: &Palette, cx: &mut Context<Self>) -> impl IntoElement {
        let mut favorites: Vec<Contact> = self
            .contacts
            .iter()
            .filter(|c| c.state == ContactState::Mutual)
            .cloned()
            .collect();
        favorites.sort_by_key(|c| (!c.favorite, !c.presence.is_online()));
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
                    .child(sidebar_row(p, IconName::Users, "Contacts", true))
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
                        div()
                            .id(SharedString::from(format!("side-{}", c.profile.id.short())))
                            .h(px(42.))
                            .px_2()
                            .rounded(px(4.))
                            .flex()
                            .items_center()
                            .gap_3()
                            .cursor_pointer()
                            .hover(move |s| s.bg(hover))
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
        let actions = match c.state {
            ContactState::Incoming => div()
                .flex()
                .gap_2()
                .child(action(IconName::Check, "Accept", "accept"))
                .child(action(IconName::X, "Ignore", "ignore")),
            ContactState::Outgoing => div().flex().child(action(IconName::X, "Cancel", "cancel")),
            _ => div()
                .flex()
                .gap_2()
                .when(c.joinable && c.in_call.is_some(), |d| {
                    d.child(
                        Button::new(SharedString::from(format!("join-{id}")))
                            .success()
                            .small()
                            .label("Join")
                            .on_click(cx.listener(|this, _, _, cx| this.start_room(cx))),
                    )
                })
                .child(
                    action(IconName::Phone, "Call", "call")
                        .on_click(cx.listener(|this, _, _, cx| this.start_room(cx))),
                )
                .child(action(IconName::Paperclip, "Send file", "file"))
                .child(action(IconName::EllipsisVertical, "More", "more")),
        };
        div()
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
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
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
                            .when(c.favorite, |d| d.child(icon(IconName::Star, 12., p.yellow))),
                    )
                    .child(div().text_sm().text_color(p.text_muted).child(status)),
            )
            .child(actions)
            .into_any_element()
    }

    fn render_active_now(&self, p: &Palette) -> impl IntoElement {
        let in_calls: Vec<&Contact> = self
            .contacts
            .iter()
            .filter(|c| c.in_call.is_some())
            .collect();
        div()
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
