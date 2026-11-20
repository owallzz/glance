//! The root view: app state, the left rail, and which screen/overlay shows.

use std::collections::HashMap;
use std::time::Instant;

use glance_core::{
    CallRecord, ChatBody, ChatMessage, Contact, Participant, PeerId, Presence, Profile, Resolution,
    demo,
};
use glance_media::TargetSize;
use glance_media::pattern::spawn_pattern;
use gpui_kit::assets::IconName;
use gpui_kit::component::input::InputState;
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::theme::Palette;
use crate::ui::{Avatar, icon, icon_button, presence_color};
use crate::video::VideoView;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Screen {
    Home,
    Room,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ContactsTab {
    Online,
    All,
    Pending,
    Blocked,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum SettingsPage {
    Account,
    Voice,
    Streaming,
    Contacts,
    Files,
    Appearance,
    Keybinds,
    Connection,
}

impl SettingsPage {
    pub const ALL: [SettingsPage; 8] = [
        SettingsPage::Account,
        SettingsPage::Voice,
        SettingsPage::Streaming,
        SettingsPage::Contacts,
        SettingsPage::Files,
        SettingsPage::Appearance,
        SettingsPage::Keybinds,
        SettingsPage::Connection,
    ];

    pub fn label(self) -> &'static str {
        match self {
            SettingsPage::Account => "My Profile",
            SettingsPage::Voice => "Voice & Video",
            SettingsPage::Streaming => "Streaming",
            SettingsPage::Contacts => "Contacts & Privacy",
            SettingsPage::Files => "File Sharing",
            SettingsPage::Appearance => "Appearance",
            SettingsPage::Keybinds => "Keybinds",
            SettingsPage::Connection => "Connection",
        }
    }
}

/// Go Live picker state.
pub struct GoLive {
    pub windows_tab: bool,
    /// Index into the screens or windows of `sources` (per tab).
    pub source: usize,
    /// Real capture sources (empty when the OS shows its own picker).
    pub sources: Vec<glance_media::capture::Source>,
    /// Wayland: the system dialog chooses what to share.
    pub system_picker: bool,
    pub resolution: Resolution,
    pub fps: u32,
    pub error: Option<String>,
}

/// Settings toggles (persisted in M11).
pub struct Settings {
    pub page: SettingsPage,
    pub noise_suppression: bool,
    pub echo_cancel: bool,
    pub tray: bool,
    pub autostart: bool,
    pub let_contacts_join: bool,
    pub auto_preview: bool,
    pub dark: bool,
    /// `None` = system default.
    pub input_device: Option<String>,
    pub output_device: Option<String>,
}

/// Everything about the room you're in.
pub struct Room {
    pub name: String,
    pub link: String,
    pub participants: Vec<Participant>,
    pub chat: Vec<ChatMessage>,
    pub chat_open: bool,
    pub focused: Option<PeerId>,
    pub watching: HashMap<PeerId, Entity<VideoView>>,
    pub started: Instant,
    /// The live network room; `None` for the `--demo` room.
    pub handle: Option<glance_net::RoomHandle>,
    /// Our running screen share, if any.
    pub broadcast: Option<glance_media::pipeline::BroadcastControl>,
    pub _broadcast_task: Option<Task<()>>,
    /// Voice engine (None in the demo room or if audio failed).
    pub audio: Option<glance_media::audio::AudioEngine>,
    pub _voice_task: Option<Task<()>>,
    /// Participant whose volume control is open in the sidebar.
    pub volume_open: Option<PeerId>,
    /// Files offered in this room.
    pub files: crate::files::FileMap,
    /// Names of history authors who may have left.
    pub history_names: HashMap<PeerId, (String, u32)>,
    pub _events: Option<Task<()>>,
}

actions!(glance, [CloseOverlay, ToggleMute, ToggleDeafen]);

/// Window-level shortcuts. `Quit` is bound in main.
pub fn bind_keys(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("escape", CloseOverlay, Some("GlanceApp")),
        KeyBinding::new("ctrl-shift-m", ToggleMute, Some("GlanceApp")),
        KeyBinding::new("ctrl-shift-d", ToggleDeafen, Some("GlanceApp")),
    ]);
}

pub struct GlanceApp {
    focus: FocusHandle,
    pub net: Option<std::rc::Rc<crate::net::Net>>,
    /// First run: ask for a display name before anything else.
    pub needs_profile: bool,
    pub name_input: Entity<InputState>,
    pub join_error: Option<String>,
    pub busy: bool,
    pub me: Profile,
    pub presence: Presence,
    pub muted: bool,
    pub deafened: bool,
    pub screen: Screen,
    pub contacts_tab: ContactsTab,
    pub contacts: Vec<Contact>,
    pub calls: Vec<CallRecord>,
    pub room: Option<Room>,
    pub join_input: Entity<InputState>,
    pub chat_input: Entity<InputState>,
    pub go_live: Option<GoLive>,
    pub settings_open: bool,
    pub settings: Settings,
    /// Per-user volume (kept across rooms for this session).
    pub volumes: HashMap<PeerId, f32>,
    /// Audio devices, listed when the voice settings open.
    pub audio_devices: Option<(
        Vec<glance_media::audio::device::DeviceInfo>,
        Vec<glance_media::audio::device::DeviceInfo>,
    )>,
    pub link_copied: bool,
}

impl GlanceApp {
    pub fn new(
        demo_room: bool,
        net: Option<std::rc::Rc<crate::net::Net>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let name_input = cx.new(|cx| InputState::new(window, cx).placeholder("Your name"));
        cx.subscribe_in(&name_input, window, Self::on_name_input)
            .detach();
        let join_input =
            cx.new(|cx| InputState::new(window, cx).placeholder("Paste a glance link or code"));
        let chat_input = cx.new(|cx| InputState::new(window, cx).placeholder("Message the room"));
        cx.subscribe_in(&chat_input, window, Self::on_chat_input)
            .detach();
        cx.subscribe_in(&join_input, window, Self::on_join_input)
            .detach();
        let (me, needs_profile) = match (&net, demo_room) {
            (Some(net), false) => {
                let id = net.me();
                match crate::net::load_profile(&net.data_dir) {
                    Some(saved) => (
                        Profile {
                            id,
                            name: saved.name,
                            color: saved.color,
                        },
                        false,
                    ),
                    None => (
                        Profile {
                            id,
                            name: String::new(),
                            color: crate::net::default_color(&id),
                        },
                        true,
                    ),
                }
            }
            _ => (demo::me(), false),
        };
        let (contacts, calls) = if demo_room || net.is_none() {
            (demo::contacts(), demo::recent_calls())
        } else {
            (Vec::new(), Vec::new())
        };
        let focus = cx.focus_handle();
        focus.focus(window, cx);
        let mut app = Self {
            focus,
            net,
            needs_profile,
            name_input,
            join_error: None,
            busy: false,
            me,
            presence: Presence::Online,
            muted: false,
            deafened: false,
            screen: Screen::Home,
            contacts_tab: ContactsTab::Online,
            contacts,
            calls,
            room: None,
            join_input,
            chat_input,
            go_live: None,
            settings_open: false,
            settings: Settings {
                page: SettingsPage::Voice,
                noise_suppression: true,
                echo_cancel: true,
                tray: true,
                autostart: true,
                let_contacts_join: false,
                auto_preview: true,
                dark: true,
                input_device: None,
                output_device: None,
            },
            link_copied: false,
            volumes: HashMap::new(),
            audio_devices: None,
        };
        if demo_room {
            app.start_demo_room(cx);
        }
        app
    }

    fn on_chat_input(
        &mut self,
        input: &Entity<InputState>,
        event: &gpui_kit::component::input::InputEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let gpui_kit::component::input::InputEvent::PressEnter { shift: false, .. } = event {
            let text = input.read(cx).value().trim().to_string();
            if text.is_empty() {
                return;
            }
            let me = self.me.id;
            if let Some(room) = self.room.as_mut() {
                let id = room.chat.last().map_or(1, |m| m.id + 1);
                if let Some(handle) = &room.handle {
                    handle.send(glance_net::Command::Chat(text.clone()));
                }
                room.chat.push(ChatMessage {
                    id,
                    author: me,
                    sent_at: now_ms(),
                    body: ChatBody::Text(text),
                });
            }
            input.update(cx, |state, cx| state.set_value("", window, cx));
            cx.notify();
        }
    }

    /// The `--demo` room: fake participants, streams and chat.
    pub fn start_demo_room(&mut self, cx: &mut Context<Self>) {
        self.room = Some(Room {
            handle: None,
            _events: None,
            broadcast: None,
            _broadcast_task: None,
            audio: None,
            _voice_task: None,
            volume_open: None,
            files: Default::default(),
            history_names: HashMap::new(),
            name: "Sam's room".into(),
            link: "glance://join/demo".into(),
            participants: demo::participants(),
            chat: demo::chat(),
            chat_open: true,
            focused: None,
            watching: HashMap::new(),
            started: Instant::now(),
        });
        self.screen = Screen::Room;
        cx.notify();
    }

    pub fn leave_room(&mut self, cx: &mut Context<Self>) {
        if let Some(b) = self.room.as_ref().and_then(|r| r.broadcast.as_ref()) {
            b.stop();
        }
        let _guard = self.net.as_ref().map(|n| n.enter());
        self.room = None;
        self.screen = Screen::Home;
        self.go_live = None;
        cx.notify();
    }

    /// Start watching a participant's stream. In the UI shell the stream is
    /// a test pattern at the advertised resolution.
    pub fn watch(&mut self, peer: PeerId, cx: &mut Context<Self>) {
        let Some(room) = self.room.as_mut() else {
            return;
        };
        if room.watching.contains_key(&peer) {
            return;
        }
        if room.handle.is_some() {
            self.watch_live(peer, cx);
            return;
        }
        let Some(stream) = room
            .participants
            .iter()
            .find(|p| p.profile.id == peer)
            .and_then(|p| p.stream.clone())
        else {
            return;
        };
        let target = TargetSize::default();
        let rx = spawn_pattern(
            stream.width,
            stream.height,
            stream.fps,
            peer.0[0].wrapping_mul(50),
            target.clone(),
        );
        let view = cx.new(|cx| VideoView::new(rx, target, cx));
        room.watching.insert(peer, view);
        cx.notify();
    }

    pub fn stop_watching(&mut self, peer: PeerId, cx: &mut Context<Self>) {
        let me = self.me.id;
        if let Some(room) = self.room.as_mut() {
            if peer != me
                && let Some(handle) = &room.handle
                && let Ok(id) = glance_net::EndpointId::from_bytes(&peer.0)
            {
                handle.send(glance_net::Command::Unwatch(id));
            }
            room.watching.remove(&peer);
            if room.focused == Some(peer) {
                room.focused = None;
            }
        }
        cx.notify();
    }

    pub fn set_muted(&mut self, muted: bool) {
        self.muted = muted;
        if !muted {
            self.deafened = false;
        }
        self.sync_self();
    }

    pub fn set_deafened(&mut self, deafened: bool) {
        self.deafened = deafened;
        self.muted = deafened || self.muted;
        self.sync_self();
    }

    /// Mirror your own state into your participant row and tell the room.
    pub fn sync_self(&mut self) {
        let (me, muted, deafened) = (self.me.id, self.muted, self.deafened);
        if let Some(room) = self.room.as_mut()
            && let Some(p) = room.participants.iter_mut().find(|p| p.profile.id == me)
        {
            p.muted = muted;
            p.deafened = deafened;
        }
        self.push_state();
        self.sync_voice();
    }

    pub fn profile_of(&self, id: PeerId) -> Option<Profile> {
        if id == self.me.id {
            return Some(self.me.clone());
        }
        self.room
            .as_ref()
            .and_then(|r| r.participants.iter().find(|p| p.profile.id == id))
            .map(|p| p.profile.clone())
            .or_else(|| {
                self.contacts
                    .iter()
                    .find(|c| c.profile.id == id)
                    .map(|c| c.profile.clone())
            })
            .or_else(|| {
                let (name, color) = self.room.as_ref()?.history_names.get(&id)?.clone();
                Some(Profile { id, name, color })
            })
    }

    fn render_rail(&mut self, p: &Palette, cx: &mut Context<Self>) -> impl IntoElement {
        let home_selected = self.screen == Screen::Home;
        let rail_item = |id: &'static str, selected: bool, bg: Hsla, child: AnyElement| {
            let hover_bg = p.brand;
            div()
                .relative()
                .w_full()
                .flex()
                .justify_center()
                // Selection pill on the left edge, like Discord.
                .child(
                    div()
                        .absolute()
                        .left_0()
                        .top(px(if selected { 4. } else { 20. }))
                        .w(px(4.))
                        .h(px(if selected { 40. } else { 8. }))
                        .rounded_r(px(4.))
                        .bg(gpui_kit::white())
                        .when(!selected, |d| d.opacity(0.)),
                )
                .child(
                    div()
                        .id(id)
                        .size(px(48.))
                        .rounded(px(if selected { 16. } else { 24. }))
                        .bg(if selected { p.brand } else { bg })
                        .flex()
                        .items_center()
                        .justify_center()
                        .cursor_pointer()
                        .hover(move |s| s.bg(hover_bg).rounded(px(16.)))
                        .child(child),
                )
        };
        let mut rail = div()
            .w(px(72.))
            .h_full()
            .flex()
            .flex_col()
            .items_center()
            .pt_3()
            .gap_2()
            .bg(p.bg_rail)
            .child(
                rail_item(
                    "rail-home",
                    home_selected,
                    p.bg_content,
                    icon(IconName::Monitor, 26., gpui_kit::white()).into_any_element(),
                )
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _, _, cx| {
                        this.screen = Screen::Home;
                        cx.notify();
                    }),
                ),
            )
            .child(div().w(px(32.)).h(px(2.)).rounded_full().bg(p.bg_selected));
        if let Some(room) = &self.room {
            let initials = room.name.chars().next().unwrap_or('R').to_string();
            rail = rail.child(
                rail_item(
                    "rail-room",
                    !home_selected,
                    p.bg_content,
                    div()
                        .text_color(gpui_kit::white())
                        .font_weight(FontWeight::SEMIBOLD)
                        .child(initials)
                        .into_any_element(),
                )
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _, _, cx| {
                        this.screen = Screen::Room;
                        cx.notify();
                    }),
                ),
            );
        }
        let green = p.green;
        rail.child(
            div()
                .id("rail-new-room")
                .size(px(48.))
                .rounded(px(24.))
                .bg(p.bg_content)
                .flex()
                .items_center()
                .justify_center()
                .cursor_pointer()
                .hover(move |s| s.bg(green).rounded(px(16.)))
                .tooltip(|window, cx| Tooltip::new("Start a room").build(window, cx))
                .on_click(cx.listener(|this, _, _, cx| {
                    if this.room.is_none() {
                        this.start_room(cx);
                    } else {
                        this.screen = Screen::Room;
                        cx.notify();
                    }
                }))
                .child(icon(IconName::Plus, 24., p.green)),
        )
    }

    /// Bottom-left panel: your avatar, status, mute/deafen/settings.
    pub fn render_user_panel(&mut self, p: &Palette, cx: &mut Context<Self>) -> impl IntoElement {
        let mic_icon = if self.muted {
            IconName::MicOff
        } else {
            IconName::Mic
        };
        let deaf_icon = if self.deafened {
            IconName::HeadphoneOff
        } else {
            IconName::Headphones
        };
        let mic_fg = if self.muted { p.red } else { p.interactive };
        let deaf_fg = if self.deafened { p.red } else { p.interactive };
        div()
            .h(px(52.))
            .px_2()
            .flex()
            .items_center()
            .gap_2()
            .bg(p.bg_user_panel)
            .child(
                Avatar::new(&self.me, 32.)
                    .status(presence_color(p, self.presence), p.bg_user_panel),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .text_sm()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(p.text_header)
                            .truncate()
                            .child(self.me.name.clone()),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(p.text_muted)
                            .child(self.presence.label()),
                    ),
            )
            .child(
                icon_button(
                    "mute",
                    mic_icon,
                    if self.muted { "Unmute" } else { "Mute" },
                    mic_fg,
                    p,
                )
                .on_click(cx.listener(|this, _, _, cx| {
                    this.set_muted(!this.muted);
                    cx.notify();
                })),
            )
            .child(
                icon_button(
                    "deafen",
                    deaf_icon,
                    if self.deafened { "Undeafen" } else { "Deafen" },
                    deaf_fg,
                    p,
                )
                .on_click(cx.listener(|this, _, _, cx| {
                    this.set_deafened(!this.deafened);
                    cx.notify();
                })),
            )
            .child(
                icon_button(
                    "settings",
                    IconName::Settings,
                    "User Settings",
                    p.interactive,
                    p,
                )
                .on_click(cx.listener(|this, _, _, cx| {
                    this.settings_open = true;
                    this.audio_devices = None; // re-list devices on open
                    cx.notify();
                })),
            )
    }
}

impl Render for GlanceApp {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = Palette::current(cx);
        let font = gpui_kit::component::ActiveTheme::theme(&**cx)
            .font_family
            .clone();
        let main = match (self.screen, self.room.is_some()) {
            (Screen::Room, true) => self.render_room(&p, window, cx).into_any_element(),
            _ => self.render_home(&p, window, cx).into_any_element(),
        };
        let mut root = div()
            .key_context("GlanceApp")
            .track_focus(&self.focus)
            .on_action(cx.listener(|this, _: &CloseOverlay, _, cx| {
                if this.settings_open {
                    this.settings_open = false;
                } else {
                    this.go_live = None;
                }
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &ToggleMute, _, cx| {
                this.set_muted(!this.muted);
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &ToggleDeafen, _, cx| {
                this.set_deafened(!this.deafened);
                cx.notify();
            }))
            .relative()
            .size_full()
            .flex()
            .flex_col()
            .font_family(font)
            .text_color(p.text_normal)
            .bg(p.bg_rail)
            .child(render_title_bar(&p));
        // Overlays live below the title bar so the window can always be
        // dragged, minimized and closed.
        let mut content = div()
            .relative()
            .flex_1()
            .min_h_0()
            .flex()
            .bg(p.bg_content)
            .child(self.render_rail(&p, cx))
            .child(main);
        if self.go_live.is_some() {
            content = content.child(self.render_go_live(&p, cx));
        }
        if self.settings_open {
            content = content.child(self.render_settings(&p, cx));
        }
        root = root.child(content);
        root
    }
}

/// Discord-style title bar: a thin strip in the rail colour with the app name
/// centred. gpui-kit's `TitleBar` handles dragging, double-click to maximize,
/// the window menu, and the minimize/maximize/close buttons per platform.
fn render_title_bar(p: &Palette) -> impl IntoElement {
    gpui_kit::component::TitleBar::new()
        .h(px(32.))
        .bg(p.bg_rail)
        .border_b_0()
        .child(
            div()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .gap_2()
                .text_sm()
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(p.text_muted)
                .child(icon(IconName::Monitor, 14., p.text_muted))
                .child("glance"),
        )
}

pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
