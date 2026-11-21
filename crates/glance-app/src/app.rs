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
use gpui_kit::component::input::{InputEvent, InputState};
use gpui_kit::component::slider::{SliderEvent, SliderState};
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

#[derive(Clone, Copy, PartialEq, Eq, Default)]
pub enum SettingsPage {
    Account,
    #[default]
    Voice,
    Streaming,
    Contacts,
    Files,
    Appearance,
    Keybinds,
    Connection,
    Updates,
}

impl SettingsPage {
    pub const ALL: [SettingsPage; 9] = [
        SettingsPage::Account,
        SettingsPage::Voice,
        SettingsPage::Streaming,
        SettingsPage::Contacts,
        SettingsPage::Files,
        SettingsPage::Appearance,
        SettingsPage::Keybinds,
        SettingsPage::Connection,
        SettingsPage::Updates,
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
            SettingsPage::Updates => "Updates",
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
    /// Audio that can go with the stream (filled in shortly after opening).
    pub audio_sources: Vec<glance_media::stream_audio::AudioSource>,
    /// Chosen audio source; `None` shares no audio.
    pub audio: Option<glance_media::stream_audio::AudioSource>,
    /// Changing a running stream ("Apply") instead of starting one.
    pub editing: bool,
}

/// What our running screen share uses, so "Stream settings" can show it
/// and change only what differs.
#[derive(Clone, Debug)]
pub struct LiveSettings {
    pub selection: glance_media::capture::Selection,
    /// A source we asked to switch to that hasn't sent a frame yet.
    pub pending_selection: Option<glance_media::capture::Selection>,
    pub resolution: Resolution,
    pub fps: u32,
    pub audio: Option<glance_media::stream_audio::AudioSource>,
}

/// User settings, saved as settings.json in the data dir.
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct Settings {
    #[serde(skip)]
    pub page: SettingsPage,
    pub noise_suppression: bool,
    /// Join/leave and mute/deafen sounds.
    pub sounds: bool,
    pub echo_cancel: bool,
    /// Automatic gain control on the mic.
    pub auto_gain: bool,
    /// Mic and speaker volume, 0.0–2.0 (1.0 is 100%).
    pub input_volume: f32,
    pub output_volume: f32,
    pub tray: bool,
    pub autostart: bool,
    pub let_contacts_join: bool,
    pub auto_preview: bool,
    pub dark: bool,
    /// `None` = system default.
    pub input_device: Option<String>,
    /// Camera index (`None` = first camera).
    pub camera_device: Option<u32>,
    pub output_device: Option<String>,
    /// Custom relay server (`None` = the default public relays). Read at
    /// startup; `GLANCE_RELAY` overrides it.
    pub relay_url: Option<String>,
    /// The quality last used to Go Live (the picker starts there).
    pub stream_resolution: Resolution,
    pub stream_fps: u32,
    /// Encode (and decode) video with the GPU when there's a supported one.
    /// The name predates GPU decoding; it stays for saved settings.
    pub hardware_encoding: bool,
    /// Check GitHub Releases for updates and download them in the background.
    pub auto_update: bool,
    /// Per-person volume and local mute, by peer id.
    pub peer_audio: glance_core::voice::PeerAudio,
    /// Room chat panel width (logical px), set by dragging its edge.
    pub chat_width: f32,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            page: SettingsPage::Voice,
            noise_suppression: true,
            sounds: true,
            echo_cancel: true,
            auto_gain: true,
            input_volume: 1.0,
            output_volume: 1.0,
            tray: true,
            autostart: true,
            let_contacts_join: false,
            auto_preview: true,
            dark: true,
            input_device: None,
            output_device: None,
            camera_device: None,
            relay_url: None,
            stream_resolution: Resolution::P1080,
            stream_fps: 60,
            hardware_encoding: true,
            auto_update: true,
            peer_audio: Default::default(),
            chat_width: crate::room::CHAT_DEFAULT_WIDTH,
        }
    }
}

impl Settings {
    pub fn load(dir: &std::path::Path) -> Self {
        std::fs::read_to_string(dir.join("settings.json"))
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, dir: &std::path::Path) -> std::io::Result<()> {
        std::fs::create_dir_all(dir)?;
        let tmp = dir.join("settings.json.tmp");
        std::fs::write(
            &tmp,
            serde_json::to_vec_pretty(self).map_err(std::io::Error::other)?,
        )?;
        std::fs::rename(tmp, dir.join("settings.json"))
    }
}

impl GlanceApp {
    /// Show or hide the tray icon to match the setting.
    pub fn apply_tray(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let want = self.settings.tray && self.net.is_some();
        if !want {
            self._tray = None;
            self._tray_task = None;
            return;
        }
        if self._tray.is_some() {
            return;
        }
        let Some(net) = self.net.clone() else { return };
        let (tx, rx) = flume::unbounded();
        self._tray = crate::tray::start(&net, tx);
        let Some(tray) = &self._tray else { return };
        // A rebuilt tray starts without the update item.
        if let Some(label) = self.update.status.tray_label() {
            tray.set_update_ready(Some(label));
        }
        self._tray_task = Some(cx.spawn_in(window, async move |this, cx| {
            while let Ok(ev) = rx.recv_async().await {
                let quit = this
                    .update_in(cx, |app, window, cx| match ev {
                        crate::tray::TrayEvent::Open => {
                            window.activate_window();
                            false
                        }
                        crate::tray::TrayEvent::SetPresence(p) => {
                            app.set_presence(p, cx);
                            false
                        }
                        crate::tray::TrayEvent::RestartToUpdate => {
                            // Disabled during calls: show the window (and the pill).
                            window.activate_window();
                            app.restart_to_update(cx);
                            false
                        }
                        crate::tray::TrayEvent::Quit => true,
                    })
                    .unwrap_or(true);
                if quit {
                    let _ = cx.update(|_, cx| cx.quit());
                    break;
                }
            }
        }));
    }

    /// Requests from later launches (clicked links): bring the window up and
    /// join the linked room unless we're already in it.
    pub fn listen_for_handoffs(
        &mut self,
        rx: flume::Receiver<crate::instance::Request>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        cx.spawn_in(window, async move |this, cx| {
            while let Ok(req) = rx.recv_async().await {
                let alive = this.update_in(cx, |app, window, cx| {
                    window.activate_window();
                    if let crate::instance::Request::Join(link) = req {
                        app.join_link(&link, cx);
                    }
                    cx.notify();
                });
                if alive.is_err() {
                    break;
                }
            }
        })
        .detach();
    }

    /// Join a room from a link, unless it's the room we're already in.
    pub fn join_link(&mut self, link: &str, cx: &mut Context<Self>) {
        let Ok(ticket) = glance_proto::Ticket::parse(link) else {
            return;
        };
        if self.needs_profile {
            return;
        }
        let current = self
            .room
            .as_ref()
            .and_then(|r| glance_proto::Ticket::parse(&r.link).ok());
        if current.is_some_and(|t| t.room == ticket.room) {
            return;
        }
        self.join(ticket, cx);
    }

    /// Switch between the dark and light themes per settings.
    pub fn apply_theme(&self, window: &mut Window, cx: &mut App) {
        use gpui_kit::component::theme::{Theme, ThemeMode};
        let mode = if self.settings.dark {
            ThemeMode::Dark
        } else {
            ThemeMode::Light
        };
        if Theme::global(cx).mode != mode {
            Theme::change(mode, Some(window), cx);
        }
    }

    /// Save the custom relay URL from settings (applies on next start).
    pub fn save_relay(&mut self, cx: &mut Context<Self>) {
        let text = self.relay_input.read(cx).value().to_string();
        match parse_relay_setting(&text) {
            Ok(url) => {
                self.relay_note = Some(match url {
                    None => "Saved. glance will use the default relays after a restart.".into(),
                    Some(_) => "Saved. glance will use this relay after a restart.".into(),
                });
                self.settings.relay_url = url;
                self.save_settings();
            }
            Err(e) => self.relay_note = Some(e.into()),
        }
        cx.notify();
    }

    /// Bring start-on-login in line with the setting, once per change (the
    /// Flatpak portal may prompt, so don't ask again for unrelated toggles).
    pub fn sync_autostart(&mut self, cx: &mut Context<Self>) {
        let want = self.settings.autostart && self.net.is_some();
        if self.autostart_synced == Some(want) {
            return;
        }
        let first = self.autostart_synced.is_none();
        self.autostart_synced = Some(want);
        // Nothing to undo at startup inside Flatpak, and asking the portal
        // to drop autostart on every launch would be noise.
        if first && !want && crate::net::in_flatpak() {
            return;
        }
        let Some(refused) = crate::tray::sync_autostart(want) else {
            return;
        };
        cx.spawn(async move |this, cx| {
            if refused.recv_async().await.is_ok() {
                let _ = this.update(cx, |app, cx| {
                    app.settings.autostart = false;
                    app.autostart_synced = Some(false);
                    app.save_settings();
                    cx.notify();
                });
            }
        })
        .detach();
    }

    pub fn save_settings(&self) {
        glance_media::codec::set_hardware_decoding(self.settings.hardware_encoding);
        if let Some(net) = &self.net
            && let Err(e) = self.settings.save(&net.data_dir)
        {
            tracing::warn!("saving settings failed: {e}");
        }
    }
}

impl Room {
    /// Our running broadcast for a track.
    pub fn control(
        &self,
        track: glance_proto::Track,
    ) -> Option<&glance_media::pipeline::BroadcastControl> {
        match track {
            glance_proto::Track::Screen => self.broadcast.as_ref(),
            glance_proto::Track::Camera => self.camera_broadcast.as_ref(),
        }
    }
}

/// Everything about the room you're in.
pub struct Room {
    pub name: String,
    pub link: String,
    pub participants: Vec<Participant>,
    pub chat: Vec<ChatMessage>,
    pub chat_open: bool,
    pub focused: Option<PeerId>,
    /// Focus mode: the strip of other participants is collapsed.
    pub filmstrip_hidden: bool,
    pub watching: HashMap<PeerId, Entity<VideoView>>,
    pub started: Instant,
    /// The live network room; `None` for the `--demo` room.
    pub handle: Option<glance_net::RoomHandle>,
    /// Our running screen share, if any.
    pub broadcast: Option<glance_media::pipeline::BroadcastControl>,
    pub _broadcast_task: Option<Task<()>>,
    /// Audio shared with our screen, if any.
    pub stream_audio: Option<glance_media::stream_audio::StreamAudio>,
    /// Settings of our running screen share.
    pub live_settings: Option<LiveSettings>,
    /// Voice engine (None in the demo room or if audio failed).
    pub audio: Option<glance_media::audio::AudioEngine>,
    pub _voice_task: Option<Task<()>>,
    /// The right-click menu for someone, while it's open.
    pub menu: Option<crate::person_menu::PersonMenu>,
    /// Streams shown in their own windows.
    pub popouts: HashMap<PeerId, crate::popout::Popout>,
    /// Camera tiles (everyone's camera is shown automatically, ours too).
    pub cameras: HashMap<PeerId, Entity<VideoView>>,
    /// Our camera broadcast, if the camera is on.
    pub camera_broadcast: Option<glance_media::pipeline::BroadcastControl>,
    pub _camera_task: Option<Task<()>>,
    /// Files offered in this room.
    pub files: crate::files::FileMap,
    /// Names of history authors who may have left.
    pub history_names: HashMap<PeerId, (String, u32)>,
    /// Everyone who was in this call at some point (for call history and
    /// "add contact" afterwards).
    pub met: HashMap<PeerId, Profile>,
    /// What's needed to remember this room and save its chat.
    pub memory: crate::recent::RoomMemory,
    pub _events: Option<Task<()>>,
}

actions!(glance, [CloseOverlay, ToggleMute, ToggleDeafen]);

/// Window-level shortcuts. `Quit` is bound in main.
/// The relay field: blank means the default relays, otherwise a URL.
pub fn parse_relay_setting(text: &str) -> Result<Option<String>, &'static str> {
    let text = text.trim();
    if text.is_empty() {
        return Ok(None);
    }
    match text.parse::<iroh::RelayUrl>() {
        Ok(_) if text.starts_with("https://") || text.starts_with("http://") => {
            Ok(Some(text.to_string()))
        }
        _ => Err("That doesn't look like a relay URL (https://...)."),
    }
}

/// Shortcut keystrokes: escape, mute, deafen, quit. ⌘ on macOS, Ctrl
/// elsewhere. (A bad keystroke string panics at startup; a test parses them.)
#[cfg(target_os = "macos")]
pub const SHORTCUTS: [&str; 4] = ["escape", "cmd-shift-m", "cmd-shift-d", "cmd-q"];
#[cfg(not(target_os = "macos"))]
pub const SHORTCUTS: [&str; 4] = ["escape", "ctrl-shift-m", "ctrl-shift-d", "ctrl-q"];

pub fn bind_keys(cx: &mut App) {
    let [esc, mute, deafen, _quit] = SHORTCUTS;
    cx.bind_keys([
        KeyBinding::new(esc, CloseOverlay, Some("GlanceApp")),
        KeyBinding::new(mute, ToggleMute, Some("GlanceApp")),
        KeyBinding::new(deafen, ToggleDeafen, Some("GlanceApp")),
    ]);
}

pub struct GlanceApp {
    pub focus: FocusHandle,
    pub net: Option<std::rc::Rc<crate::net::Net>>,
    /// First run: ask for a display name before anything else.
    pub needs_profile: bool,
    pub name_input: Entity<InputState>,
    /// Settings → Connection: custom relay URL.
    pub relay_input: Entity<InputState>,
    /// Contacts home: the nickname field and whose nickname it edits.
    pub nickname_input: Entity<InputState>,
    pub editing_nickname: Option<PeerId>,
    /// Feedback under the relay field.
    pub relay_note: Option<String>,
    pub join_error: Option<String>,
    pub busy: bool,
    pub me: Profile,
    pub presence: Presence,
    /// Your mute/deafen (kept across rooms for the session).
    pub voice: glance_core::voice::SelfVoice,
    pub screen: Screen,
    pub contacts_tab: ContactsTab,
    pub contacts: Vec<Contact>,
    pub calls: Vec<CallRecord>,
    pub room: Option<Room>,
    pub join_input: Entity<InputState>,
    pub chat_input: Entity<InputState>,
    pub go_live: Option<GoLive>,
    pub settings_open: bool,
    /// The window's rounded corners this frame (zero when maximized,
    /// tiled, fullscreen or drawn by the OS). Whatever paints a background
    /// into a window corner rounds it to match.
    pub corners: Corners<Pixels>,
    pub settings: Settings,
    pub book: glance_core::contacts::ContactBook,
    pub live: crate::contacts::Live,
    pub incoming_call: Option<crate::contacts::IncomingCall>,
    /// People from the last call who aren't contacts yet.
    pub last_call: Vec<Profile>,
    pub _contacts_task: Option<Task<()>>,
    /// Status picker under your avatar.
    pub presence_menu: bool,
    /// Contact whose "more" menu is open.
    pub contact_menu: Option<PeerId>,
    /// Files sent to / received from contacts outside calls (newest first).
    pub direct_files: Vec<crate::direct::DirectFile>,
    /// Decoded chat images (thumbnails, the lightbox copy).
    pub images: crate::images::ImageCache,
    /// The image open full-window, if any.
    pub lightbox: Option<crate::images::Lightbox>,
    /// Direct messages with contacts.
    pub dms: crate::dms::Dms,
    /// The conversation open on the home screen.
    pub dm_open: Option<PeerId>,
    pub dm_input: Entity<gpui_kit::component::input::TextareaState>,
    /// The window has focus (unread counts and notifications depend on it).
    pub window_active: bool,
    /// We switched to Idle automatically (and switch back on activity).
    pub auto_idle: bool,
    /// "Invite" panel in a room.
    pub invite_open: bool,
    /// Stream shown full-window (fullscreen viewing).
    pub immersive: Option<PeerId>,
    pub _tray: Option<crate::tray::Tray>,
    pub _tray_task: Option<Task<()>>,
    /// Start-on-login state last handed to the system.
    pub autostart_synced: Option<bool>,
    pub camera_list: Option<Vec<glance_media::camera::CameraInfo>>,
    /// Audio devices, listed when the voice settings open.
    pub audio_devices: Option<(
        Vec<glance_media::audio::device::DeviceInfo>,
        Vec<glance_media::audio::device::DeviceInfo>,
    )>,
    pub link_copied: bool,
    /// Bumped on every copy, so only the latest one clears `link_copied`.
    pub link_copied_gen: u32,
    /// The invite link is masked until you click to show it.
    pub link_revealed: bool,
    /// Message at the bottom of the window ("Invite link copied").
    pub toast: Option<SharedString>,
    pub toast_gen: u32,
    /// Rooms you were in, and whether they're still going.
    pub recent: crate::recent::Recent,
    /// Mic and speaker volume sliders in the voice settings (0–200%).
    pub input_volume_slider: Entity<SliderState>,
    pub output_volume_slider: Entity<SliderState>,
    /// Updates from GitHub Releases (`update_ui.rs`).
    pub update: crate::update_ui::UpdateState,
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
        let nickname_input = cx.new(|cx| {
            InputState::new(window, cx).placeholder("Nickname (blank to use their name)")
        });
        cx.subscribe_in(
            &nickname_input,
            window,
            |this: &mut Self, _, ev: &InputEvent, _, cx| {
                if let InputEvent::PressEnter { .. } = ev {
                    this.save_nickname(cx);
                }
            },
        )
        .detach();
        let relay_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("https://relay.example.com (blank = default relays)")
        });
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
        let settings = net
            .as_ref()
            .map(|n| Settings::load(&n.data_dir))
            .unwrap_or_default();
        glance_media::codec::set_hardware_decoding(settings.hardware_encoding);
        let direct_files = net
            .as_ref()
            .map(|n| crate::direct::load(&n.data_dir))
            .unwrap_or_default();
        let dms = match (&net, demo_room) {
            (Some(n), false) => crate::dms::Dms::open(&n.data_dir),
            _ => Default::default(),
        };
        let dm_input = Self::new_dm_input(window, cx);
        let volume_slider = |value: f32, cx: &mut Context<Self>| {
            cx.new(|_| {
                SliderState::new()
                    .min(0.)
                    .max(200.)
                    .step(1.)
                    .default_value(value * 100.)
            })
        };
        let input_volume_slider = volume_slider(settings.input_volume, cx);
        let output_volume_slider = volume_slider(settings.output_volume, cx);
        cx.subscribe(&input_volume_slider, |this, _, ev: &SliderEvent, cx| {
            this.on_volume_slider(true, ev, cx)
        })
        .detach();
        cx.subscribe(&output_volume_slider, |this, _, ev: &SliderEvent, cx| {
            this.on_volume_slider(false, ev, cx)
        })
        .detach();
        let focus = cx.focus_handle();
        focus.focus(window, cx);
        let mut app = Self {
            focus,
            net,
            needs_profile,
            name_input,
            relay_input,
            relay_note: None,
            nickname_input,
            editing_nickname: None,
            join_error: None,
            busy: false,
            me,
            presence: Presence::Online,
            voice: Default::default(),
            screen: Screen::Home,
            contacts_tab: ContactsTab::Online,
            contacts,
            calls,
            room: None,
            join_input,
            chat_input,
            go_live: None,
            settings_open: false,
            corners: Corners::default(),
            settings,
            link_copied: false,
            link_copied_gen: 0,
            link_revealed: false,
            toast: None,
            toast_gen: 0,
            recent: Default::default(),
            input_volume_slider,
            output_volume_slider,
            audio_devices: None,
            camera_list: None,
            book: Default::default(),
            live: Default::default(),
            incoming_call: None,
            last_call: Vec::new(),
            _contacts_task: None,
            presence_menu: false,
            contact_menu: None,
            direct_files,
            images: Default::default(),
            lightbox: None,
            dms,
            dm_open: None,
            dm_input,
            window_active: true,
            auto_idle: false,
            invite_open: false,
            _tray: None,
            _tray_task: None,
            autostart_synced: None,
            immersive: None,
            update: Default::default(),
        };
        // While the tray icon is up, closing the window minimizes it so calls
        // keep going and contacts can still ring you.
        window.on_window_should_close(cx, |window, _cx| {
            if crate::tray::active() {
                window.minimize_window();
                false
            } else {
                true
            }
        });
        app.apply_theme(window, cx);
        app.watch_window_activation(window, cx);
        if let Some(url) = app.settings.relay_url.clone() {
            app.relay_input
                .update(cx, |s, cx| s.set_value(url, window, cx));
        }
        app.apply_tray(window, cx);
        app.sync_autostart(cx);
        if !demo_room {
            app.start_contacts(cx);
            app.watch_idle(cx);
            app.start_updater(cx);
            app.load_recent(cx);
        }
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
                let sent_at = now_ms();
                if let Some(handle) = &room.handle {
                    handle.send(glance_net::Command::Post {
                        sent_at,
                        body: glance_proto::HistoryBody::Text(text.clone()),
                    });
                }
                room.chat.push(ChatMessage {
                    id,
                    author: me,
                    sent_at,
                    body: ChatBody::Text(text),
                });
            }
            self.persist_chat();
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
            stream_audio: None,
            live_settings: None,
            audio: None,
            _voice_task: None,
            menu: None,
            popouts: HashMap::new(),
            cameras: HashMap::new(),
            camera_broadcast: None,
            _camera_task: None,
            files: Default::default(),
            history_names: HashMap::new(),
            met: HashMap::new(),
            memory: Default::default(),
            name: "Sam's room".into(),
            link: "glance://join/demo".into(),
            participants: demo::participants(),
            chat: demo::chat(),
            chat_open: true,
            focused: None,
            filmstrip_hidden: false,
            watching: HashMap::new(),
            started: Instant::now(),
        });
        self.screen = Screen::Room;
        cx.notify();
    }

    pub fn leave_room(&mut self, cx: &mut Context<Self>) {
        self.record_call_end();
        self.cancel_rings();
        self.remember_room(true);
        self.persist_chat();
        // The home screen checks the rooms again (including this one).
        self.recent.stale();
        self.invite_open = false;
        if let Some(room) = self.room.as_ref() {
            for b in [room.broadcast.as_ref(), room.camera_broadcast.as_ref()]
                .into_iter()
                .flatten()
            {
                b.stop();
            }
        }
        let _guard = self.net.as_ref().map(|n| n.enter());
        self.room = None;
        self.screen = Screen::Home;
        self.go_live = None;
        self.push_presence();
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
                handle.send(glance_net::Command::Unwatch(
                    id,
                    glance_proto::Track::Screen,
                ));
            }
            room.watching.remove(&peer);
            if room.focused == Some(peer) {
                room.focused = None;
            }
        }
        cx.notify();
    }

    pub fn set_muted(&mut self, muted: bool) {
        use glance_media::audio::sounds::Sound;
        let was_deafened = self.voice.deafened;
        self.voice.set_muted(muted);
        self.chime(match (muted, was_deafened) {
            (false, true) => Sound::Undeafen,
            (false, false) => Sound::Unmute,
            (true, _) => Sound::Mute,
        });
        self.sync_self();
    }

    pub fn set_deafened(&mut self, deafened: bool) {
        use glance_media::audio::sounds::Sound;
        self.voice.set_deafened(deafened);
        self.chime(if deafened {
            Sound::Deafen
        } else {
            Sound::Undeafen
        });
        self.sync_self();
    }

    /// Play a UI sound if sounds are on and voice is running.
    pub fn chime(&self, sound: glance_media::audio::sounds::Sound) {
        if self.settings.sounds
            && let Some(engine) = self.room.as_ref().and_then(|r| r.audio.as_ref())
        {
            engine.play(sound);
        }
    }

    /// Mirror your own state into your participant row and tell the room.
    pub fn sync_self(&mut self) {
        let (me, muted, deafened) = (self.me.id, self.voice.muted, self.voice.deafened);
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
        let unread = self.dm_total_unread();
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
            .rounded_bl(self.corners.bottom_left)
            .child(
                rail_item(
                    "rail-home",
                    home_selected,
                    p.bg_content,
                    icon(
                        IconName::Monitor,
                        26.,
                        if home_selected {
                            gpui_kit::white()
                        } else {
                            p.text_normal
                        },
                    )
                    .into_any_element(),
                )
                .when(unread > 0, |d| {
                    d.child(
                        div()
                            .absolute()
                            .right(px(10.))
                            .bottom(px(-2.))
                            .rounded_full()
                            .border_3()
                            .border_color(p.bg_rail)
                            .child(crate::dms::unread_badge(p, unread)),
                    )
                })
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _, window, cx| {
                        if this.screen != Screen::Home {
                            this.recent.home_shown();
                        }
                        this.screen = Screen::Home;
                        if let Some(peer) = this.dm_open {
                            this.open_dm(peer, window, cx);
                        }
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
        let mic_icon = if self.voice.muted {
            IconName::MicOff
        } else {
            IconName::Mic
        };
        let deaf_icon = if self.voice.deafened {
            IconName::HeadphoneOff
        } else {
            IconName::Headphones
        };
        let mic_fg = if self.voice.muted {
            p.red
        } else {
            p.interactive
        };
        let deaf_fg = if self.voice.deafened {
            p.red
        } else {
            p.interactive
        };
        div()
            .h(px(52.))
            .px_2()
            .flex()
            .items_center()
            .gap_2()
            .bg(p.bg_user_panel)
            .child(
                div()
                    .id("my-avatar")
                    .cursor_pointer()
                    .child(
                        Avatar::new(&self.me, 32.)
                            .status(presence_color(p, self.presence), p.bg_user_panel),
                    )
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.presence_menu = !this.presence_menu;
                        cx.notify();
                    })),
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
                    if self.voice.muted { "Unmute" } else { "Mute" },
                    mic_fg,
                    p,
                )
                .on_click(cx.listener(|this, _, _, cx| {
                    this.set_muted(!this.voice.muted);
                    cx.notify();
                })),
            )
            .child(
                icon_button(
                    "deafen",
                    deaf_icon,
                    if self.voice.deafened {
                        "Undeafen"
                    } else {
                        "Deafen"
                    },
                    deaf_fg,
                    p,
                )
                .on_click(cx.listener(|this, _, _, cx| {
                    this.set_deafened(!this.voice.deafened);
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
                    this.camera_list = None;
                    cx.notify();
                })),
            )
    }
}

/// Height of a one-line message box (room chat and DMs), and of the floating
/// user panel's own row.
pub const COMPOSER_HEIGHT: f32 = 56.;
/// Gap between the window's bottom edge and the message box or the floating
/// panel (and the panel's left inset).
pub const PANEL_MARGIN: f32 = 8.;
/// The "Voice Connected" part of the floating panel, above your own row.
const VOICE_PANEL_HEIGHT: f32 = 52.;

impl Render for GlanceApp {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = Palette::current(cx);
        self.corners = gpui_kit::component::window_corner_radii(window, cx);
        let font = gpui_kit::component::ActiveTheme::theme(&**cx)
            .font_family
            .clone();
        if let Some(peer) = self.immersive {
            if let Some(view) = self
                .room
                .as_ref()
                .and_then(|r| r.watching.get(&peer))
                .cloned()
            {
                return div()
                    .id("immersive")
                    .key_context("GlanceApp")
                    .track_focus(&self.focus)
                    .on_action(cx.listener(|this, _: &CloseOverlay, window, cx| {
                        this.exit_fullscreen(window, cx)
                    }))
                    .size_full()
                    .bg(gpui_kit::black())
                    .on_click(cx.listener(|this, ev: &ClickEvent, window, cx| {
                        if ev.click_count() >= 2 {
                            this.exit_fullscreen(window, cx);
                        }
                    }))
                    .child(view)
                    .into_any_element();
            }
            self.immersive = None;
        }
        let main = match (self.screen, self.room.is_some()) {
            (Screen::Room, true) => self.render_room(&p, window, cx).into_any_element(),
            _ => self.render_home(&p, window, cx).into_any_element(),
        };
        let mut root = div()
            .key_context("GlanceApp")
            .track_focus(&self.focus)
            .on_action(cx.listener(|this, _: &CloseOverlay, window, cx| {
                if this.close_person_menu() || this.close_lightbox() {
                    // Just the menu or the image.
                } else if this.immersive.is_some() {
                    this.exit_fullscreen(window, cx);
                } else if this.settings_open {
                    this.settings_open = false;
                } else {
                    this.go_live = None;
                }
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &ToggleMute, _, cx| {
                this.set_muted(!this.voice.muted);
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &ToggleDeafen, _, cx| {
                this.set_deafened(!this.voice.deafened);
                cx.notify();
            }))
            .relative()
            .size_full()
            .flex()
            .flex_col()
            .font_family(font)
            .text_color(p.text_normal)
            .bg(p.bg_rail)
            .rounded_bl(self.corners.bottom_left)
            .rounded_br(self.corners.bottom_right)
            .child(render_title_bar(&p, self.render_update_pill(&p, cx)));
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
        if self.invite_open && self.room.is_some() {
            content = content.child(self.render_invite(&p, cx));
        }
        if self.presence_menu {
            content = content.child(self.render_presence_menu(&p, cx));
        }
        if self.settings_open {
            content = content.child(self.render_settings(&p, cx));
        }
        if self.incoming_call.is_some() {
            content = content.child(self.render_incoming_call(&p, cx));
        }
        if let Some(toast) = self.render_toast(&p) {
            content = content.child(toast);
        }
        if let Some(lightbox) = self.render_lightbox(&p, window, cx) {
            content = content.child(lightbox);
        }
        self.drop_stale_images(window, cx);
        root = root.child(content);
        root.into_any_element()
    }
}

/// Discord-style title bar: a thin strip in the rail colour with the app name
/// centred. gpui-kit's `TitleBar` handles dragging, double-click to maximize,
/// the window menu, and the minimize/maximize/close buttons per platform.
fn render_title_bar(p: &Palette, update_pill: Option<AnyElement>) -> impl IntoElement {
    gpui_kit::component::TitleBar::new()
        // Linux close button (other platforms use on_window_should_close).
        .on_close_window(|_, window, _| {
            if crate::tray::active() {
                window.minimize_window();
            } else {
                window.remove_window();
            }
        })
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
                .child("glance")
                .children(update_pill),
        )
}

pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod key_tests {
    #[test]
    fn muted_peers_survive_a_restart() {
        use glance_core::PeerId;
        let dir = std::env::temp_dir().join(format!("glance-settings-test-{}", std::process::id()));
        let (a, b) = (PeerId([7; 32]), PeerId([8; 32]));
        let mut s = super::Settings::default();
        s.peer_audio.update(a, |p| {
            p.volume = 0.4;
            p.muted = true;
        });
        s.save(&dir).unwrap();
        let back = super::Settings::load(&dir);
        let _ = std::fs::remove_dir_all(&dir);
        assert!(back.peer_audio.get(a).muted);
        assert_eq!(back.peer_audio.get(a).volume, 0.4);
        assert!(!back.peer_audio.get(b).muted);
    }

    #[test]
    fn shortcuts_parse() {
        for k in super::SHORTCUTS {
            assert!(gpui_kit::Keystroke::parse(k).is_ok(), "{k}");
        }
    }

    #[test]
    fn relay_setting() {
        use super::parse_relay_setting as p;
        assert_eq!(p("  "), Ok(None));
        assert_eq!(
            p(" https://relay.example.com "),
            Ok(Some("https://relay.example.com".into()))
        );
        assert!(p("nope").is_err());
        assert!(p("ftp://x").is_err());
    }
}
