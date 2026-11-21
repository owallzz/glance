//! Fullscreen (immersive) viewing and popping a stream out into its own window.
//!
//! The pop-out window has no title bar: the video fills it, dragging the
//! video moves the window and double-clicking toggles fullscreen (as in the
//! main window). Moving the mouse shows floating controls (window buttons,
//! fullscreen, back to the main window, stream volume) that hide again, with
//! the cursor, after [`AutoHide::DELAY`] without movement. On Linux the window
//! is client-decorated: gpui-kit's window frame resizes from the shadow
//! around it, and [`resize_zones`] adds a wider grab band just inside.
//! macOS and Windows resize natively.

use std::time::{Duration, Instant};

use glance_core::PeerId;
use gpui_kit::assets::IconName;
use gpui_kit::component::TitleBar;
use gpui_kit::component::slider::{Slider, SliderEvent, SliderState};
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::app::GlanceApp;
use crate::theme::Palette;
use crate::ui::{icon, live_badge};
use crate::video::VideoView;

actions!(
    popout,
    [
        PopoutToggleFullscreen,
        PopoutEscape,
        PopoutToggleMute,
        PopoutClose
    ]
);

/// Pop-out shortcuts: fullscreen (two keys), Esc, mute, close.
#[cfg(target_os = "macos")]
pub const KEYS: [&str; 5] = ["f", "f11", "escape", "m", "cmd-w"];
#[cfg(not(target_os = "macos"))]
pub const KEYS: [&str; 5] = ["f", "f11", "escape", "m", "ctrl-w"];

pub fn bind_keys(cx: &mut App) {
    let [f, f11, esc, mute, close] = KEYS;
    let ctx = Some("Popout");
    cx.bind_keys([
        KeyBinding::new(f, PopoutToggleFullscreen, ctx),
        KeyBinding::new(f11, PopoutToggleFullscreen, ctx),
        KeyBinding::new(esc, PopoutEscape, ctx),
        KeyBinding::new(mute, PopoutToggleMute, ctx),
        KeyBinding::new(close, PopoutClose, ctx),
    ]);
}

/// A stream shown in its own window (kept in `RoomUi::popouts`).
pub struct Popout {
    /// Where the stream's frames go.
    pub video: Entity<VideoView>,
    pub window: AnyWindowHandle,
}

/// Show/hide state of the floating controls.
///
/// Mouse movement shows them; they hide [`AutoHide::DELAY`] after the last
/// movement, unless the pointer rests on them (`pinned`). At most one timer
/// runs at a time: movement while it runs only moves the deadline, and the
/// timer re-arms itself for the remaining time, so a burst of movement costs
/// one timer and an idle window none.
#[derive(Debug)]
pub struct AutoHide {
    visible: bool,
    pinned: bool,
    armed: bool,
    last_activity: Instant,
}

/// What to do when the hide timer fires.
#[derive(Debug, PartialEq, Eq)]
pub enum TimerOutcome {
    /// Hide the controls (and the cursor) now.
    Hide,
    /// The mouse moved meanwhile: wait this much longer.
    Wait(Duration),
    /// Nothing to hide (already hidden, or pinned); the timer ends.
    Stop,
}

impl AutoHide {
    pub const DELAY: Duration = Duration::from_secs(2);

    pub fn new(now: Instant) -> Self {
        Self {
            visible: false,
            pinned: false,
            armed: false,
            last_activity: now,
        }
    }

    pub fn visible(&self) -> bool {
        self.visible
    }

    /// The mouse moved: show the controls. Returns a delay when a hide timer
    /// has to be started (none is running yet).
    pub fn activity(&mut self, now: Instant) -> Option<Duration> {
        self.visible = true;
        self.last_activity = now;
        self.arm()
    }

    /// The pointer entered (`true`) or left a control. Controls never hide
    /// while pinned; leaving restarts the countdown.
    pub fn set_pinned(&mut self, pinned: bool, now: Instant) -> Option<Duration> {
        self.pinned = pinned;
        if pinned || !self.visible {
            return None;
        }
        self.last_activity = now;
        self.arm()
    }

    /// The hide timer fired. `busy` means the user is in the middle of
    /// something with the controls (dragging the volume slider), which gpui
    /// reports as "not hovered": count it as activity.
    pub fn timer_fired(&mut self, now: Instant, busy: bool) -> TimerOutcome {
        self.armed = false;
        if !self.visible || self.pinned {
            return TimerOutcome::Stop;
        }
        if busy {
            self.last_activity = now;
        }
        let due = self.last_activity + Self::DELAY;
        if now >= due {
            self.visible = false;
            TimerOutcome::Hide
        } else {
            self.armed = true;
            TimerOutcome::Wait(due - now)
        }
    }

    /// The pointer left the window: hide right away. Returns whether that
    /// changed anything.
    pub fn hide_now(&mut self) -> bool {
        let was = self.visible;
        self.visible = false;
        self.pinned = false;
        was
    }

    fn arm(&mut self) -> Option<Duration> {
        if self.armed || self.pinned {
            return None;
        }
        self.armed = true;
        Some(Self::DELAY)
    }
}

/// Initial pop-out size (logical pixels) for a stream of `w`×`h`: as large
/// as fits in 960×600 at the stream's aspect ratio (16:9 when unknown,
/// extreme ratios clamped so the window stays usable).
pub fn popout_size(stream: Option<(u32, u32)>) -> (f32, f32) {
    const MAX_W: f32 = 960.;
    const MAX_H: f32 = 600.;
    let aspect = match stream {
        Some((w, h)) if w > 0 && h > 0 => (w as f32 / h as f32).clamp(0.5, 4.0),
        _ => 16. / 9.,
    };
    if MAX_W / aspect <= MAX_H {
        (MAX_W, (MAX_W / aspect).round())
    } else {
        ((MAX_H * aspect).round(), MAX_H)
    }
}

/// New stream volume and the remembered "unmuted" volume after pressing mute.
/// Unmuting goes back to the last audible volume.
pub fn toggle_mute(volume: f32, restore: f32) -> (f32, f32) {
    if volume > 0.0 {
        (0.0, volume)
    } else {
        (if restore > 0.0 { restore } else { 1.0 }, restore)
    }
}

/// Width of the extra resize band inside the window edge, and the size of
/// the corner squares.
const RESIZE_BAND: f32 = 6.;
const RESIZE_CORNER: f32 = 12.;

/// Resize handles for a client-decorated window: the edges that aren't
/// tiled (snapped to a screen edge or another window), and the corners
/// between two such edges. Corners come last so they sit on top.
pub fn resize_zones(tiling: Tiling) -> Vec<ResizeEdge> {
    let mut zones = Vec::with_capacity(8);
    for (edge, tiled) in [
        (ResizeEdge::Top, tiling.top),
        (ResizeEdge::Bottom, tiling.bottom),
        (ResizeEdge::Left, tiling.left),
        (ResizeEdge::Right, tiling.right),
    ] {
        if !tiled {
            zones.push(edge);
        }
    }
    for (edge, a, b) in [
        (ResizeEdge::TopLeft, tiling.top, tiling.left),
        (ResizeEdge::TopRight, tiling.top, tiling.right),
        (ResizeEdge::BottomLeft, tiling.bottom, tiling.left),
        (ResizeEdge::BottomRight, tiling.bottom, tiling.right),
    ] {
        if !a && !b {
            zones.push(edge);
        }
    }
    zones
}

pub fn resize_cursor(edge: ResizeEdge) -> CursorStyle {
    match edge {
        ResizeEdge::Top | ResizeEdge::Bottom => CursorStyle::ResizeUpDown,
        ResizeEdge::Left | ResizeEdge::Right => CursorStyle::ResizeLeftRight,
        ResizeEdge::TopLeft | ResizeEdge::BottomRight => CursorStyle::ResizeUpLeftDownRight,
        ResizeEdge::TopRight | ResizeEdge::BottomLeft => CursorStyle::ResizeUpRightDownLeft,
    }
}

fn resize_zone(edge: ResizeEdge) -> Div {
    let (band, corner) = (px(RESIZE_BAND), px(RESIZE_CORNER));
    let d = div().absolute().cursor(resize_cursor(edge));
    let d = match edge {
        ResizeEdge::Top => d.top_0().left(corner).right(corner).h(band),
        ResizeEdge::Bottom => d.bottom_0().left(corner).right(corner).h(band),
        ResizeEdge::Left => d.left_0().top(corner).bottom(corner).w(band),
        ResizeEdge::Right => d.right_0().top(corner).bottom(corner).w(band),
        ResizeEdge::TopLeft => d.top_0().left_0().size(corner),
        ResizeEdge::TopRight => d.top_0().right_0().size(corner),
        ResizeEdge::BottomLeft => d.bottom_0().left_0().size(corner),
        ResizeEdge::BottomRight => d.bottom_0().right_0().size(corner),
    };
    d.on_mouse_down(MouseButton::Left, move |_, window, cx| {
        cx.stop_propagation();
        window.start_window_resize(edge);
    })
}

/// Hide the mouse cursor until it moves.
///
/// gpui has no direct call for this, but hides the pointer (until the next
/// movement, on every platform) when a key that types a character is
/// pressed. A synthetic key that types nothing and has no bindings triggers
/// exactly that and nothing else; the pop-out has no text input to receive it.
fn hide_cursor(window: &mut Window, cx: &mut App) {
    window.dispatch_keystroke(
        Keystroke {
            modifiers: Modifiers::default(),
            key: "f24".into(),
            key_char: Some(String::new()),
        },
        cx,
    );
}

/// What the overlay shows about the stream, copied from the app when it changes.
#[derive(Clone, Debug, PartialEq)]
pub struct PopoutInfo {
    pub name: SharedString,
    pub source: SharedString,
    pub has_audio: bool,
    /// Stream audio volume, 0.0–2.0.
    pub volume: f32,
}

impl GlanceApp {
    fn popout_info(&self, peer: PeerId) -> Option<PopoutInfo> {
        let person = self
            .room
            .as_ref()?
            .participants
            .iter()
            .find(|p| p.profile.id == peer)?;
        let stream = person.stream.as_ref()?;
        Some(PopoutInfo {
            name: person.profile.name.clone().into(),
            source: stream.source.clone().into(),
            has_audio: stream.has_audio,
            volume: self.stream_volume(peer),
        })
    }
}

/// The root of a pop-out window: the video, with controls floating on top.
pub struct PopoutView {
    peer: PeerId,
    app: WeakEntity<GlanceApp>,
    window_id: WindowId,
    pub video: Entity<VideoView>,
    info: PopoutInfo,
    focus: FocusHandle,
    chrome: AutoHide,
    hide_timer: Option<Task<()>>,
    /// Where a left press on the video started; moving from there starts a
    /// window move (a plain click or double-click doesn't).
    press: Option<Point<Pixels>>,
    volume_slider: Entity<SliderState>,
    /// Volume to go back to when unmuting.
    restore_volume: f32,
    _subscriptions: Vec<Subscription>,
}

impl PopoutView {
    fn new(
        peer: PeerId,
        app: &Entity<GlanceApp>,
        info: PopoutInfo,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let video = cx.new(|_| {
            let mut v = VideoView::empty(glance_media::TargetSize::default());
            v.show_stats = false;
            v
        });
        let volume_slider = cx.new(|_| {
            SliderState::new()
                .min(0.)
                .max(200.)
                .step(1.)
                .default_value(info.volume * 100.)
        });
        let window_id = window.window_handle().window_id();
        let subscriptions = vec![
            cx.subscribe(&volume_slider, |this, _, ev: &SliderEvent, cx| {
                let (SliderEvent::Change(v) | SliderEvent::Release(v)) = ev;
                this.set_volume((v.start() / 100.).clamp(0., 2.), cx);
            }),
            cx.observe_in(app, window, |this, app, window, cx| {
                this.sync(&app, window, cx)
            }),
        ];
        // However the window goes away, the stream goes back to the main window.
        let weak = app.downgrade();
        cx.on_release(move |_, cx| {
            if let Some(app) = weak.upgrade() {
                app.update(cx, |app, cx| app.popout_closed(peer, window_id, cx));
            }
        })
        .detach();
        let weak = app.downgrade();
        window.on_window_should_close(cx, move |_, cx| {
            if let Some(app) = weak.upgrade() {
                app.update(cx, |app, cx| app.popout_closed(peer, window_id, cx));
            }
            true
        });
        let focus = cx.focus_handle();
        focus.focus(window, cx);
        let restore_volume = if info.volume > 0. { info.volume } else { 1. };
        let mut view = Self {
            peer,
            app: app.downgrade(),
            window_id,
            video,
            info,
            focus,
            chrome: AutoHide::new(Instant::now()),
            hide_timer: None,
            press: None,
            volume_slider,
            restore_volume,
            _subscriptions: subscriptions,
        };
        // Show the controls once so people see what's there.
        view.activity(window, cx);
        view
    }

    /// The app changed: close if the stream is gone, else refresh what we show.
    fn sync(&mut self, app: &Entity<GlanceApp>, window: &mut Window, cx: &mut Context<Self>) {
        let (ours, info) = {
            let app = app.read(cx);
            let ours = app
                .room
                .as_ref()
                .and_then(|r| r.popouts.get(&self.peer))
                .is_some_and(|p| p.window.window_id() == self.window_id);
            (ours, app.popout_info(self.peer))
        };
        // Left the room, popped back in from the main window, or the stream ended.
        let Some(info) = info.filter(|_| ours) else {
            let (peer, id) = (self.peer, self.window_id);
            app.update(cx, |app, cx| app.popout_closed(peer, id, cx));
            window.remove_window();
            return;
        };
        if info == self.info {
            return;
        }
        if info.volume != self.info.volume {
            let value = info.volume * 100.;
            self.volume_slider
                .update(cx, |s, cx| s.set_value(value, window, cx));
        }
        if info.volume > 0. {
            self.restore_volume = info.volume;
        }
        self.info = info;
        cx.notify();
    }

    fn set_volume(&mut self, volume: f32, cx: &mut Context<Self>) {
        let peer = self.peer;
        self.info.volume = volume;
        if let Some(app) = self.app.upgrade() {
            app.update(cx, |app, cx| {
                app.set_stream_volume(peer, volume);
                cx.notify();
            });
        }
        cx.notify();
    }

    fn toggle_mute(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.info.has_audio {
            return;
        }
        let (volume, restore) = toggle_mute(self.info.volume, self.restore_volume);
        self.restore_volume = restore;
        self.volume_slider
            .update(cx, |s, cx| s.set_value(volume * 100., window, cx));
        self.set_volume(volume, cx);
    }

    fn close(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (peer, id) = (self.peer, self.window_id);
        if let Some(app) = self.app.upgrade() {
            app.update(cx, |app, cx| app.popout_closed(peer, id, cx));
        }
        window.remove_window();
    }

    /// Esc only leaves fullscreen; closing is Ctrl/Cmd+W or the buttons, so a
    /// stray Esc never sends the stream back.
    fn escape(&mut self, window: &mut Window) {
        if window.is_fullscreen() {
            window.toggle_fullscreen();
        }
    }

    /// Mouse movement: show the controls, and start the hide timer if none runs.
    fn activity(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let was_visible = self.chrome.visible();
        if let Some(delay) = self.chrome.activity(Instant::now()) {
            self.arm_hide_timer(delay, window, cx);
        }
        if !was_visible {
            cx.notify();
        }
    }

    fn set_pinned(&mut self, pinned: bool, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(delay) = self.chrome.set_pinned(pinned, Instant::now()) {
            self.arm_hide_timer(delay, window, cx);
        }
    }

    fn arm_hide_timer(&mut self, delay: Duration, window: &mut Window, cx: &mut Context<Self>) {
        self.hide_timer = Some(cx.spawn_in(window, async move |this, cx| {
            let mut delay = delay;
            loop {
                cx.background_executor().timer(delay).await;
                let next = this.update_in(cx, |view, window, cx| {
                    match view
                        .chrome
                        .timer_fired(Instant::now(), cx.has_active_drag())
                    {
                        TimerOutcome::Hide => {
                            // Never hide the pointer over another window.
                            // Deferred: the fake keystroke redraws the
                            // window, which renders this view, and it's
                            // still leased here (double-lease panic).
                            if window.is_window_hovered() {
                                window.defer(cx, hide_cursor);
                            }
                            cx.notify();
                            None
                        }
                        TimerOutcome::Wait(d) => Some(d),
                        TimerOutcome::Stop => None,
                    }
                });
                match next {
                    Ok(Some(d)) => delay = d,
                    _ => break,
                }
            }
        }));
    }

    /// The video surface: drag to move the window, double-click for fullscreen.
    fn render_surface(&self, cx: &mut Context<Self>) -> Stateful<Div> {
        div()
            .id("popout-surface")
            .absolute()
            .inset_0()
            // Windows moves windows from caption hit tests, not start_window_move.
            .when(cfg!(target_os = "windows"), |d| {
                d.window_control_area(WindowControlArea::Drag)
            })
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, ev: &MouseDownEvent, window, cx| {
                    if ev.click_count >= 2 {
                        this.press = None;
                        window.toggle_fullscreen();
                        // Handled: on Windows this also stops the native
                        // double-click-to-maximize on the caption area.
                        cx.stop_propagation();
                    } else {
                        this.press = Some(ev.position);
                    }
                }),
            )
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _, _, _| this.press = None),
            )
            .on_mouse_move(cx.listener(|this, ev: &MouseMoveEvent, window, _| {
                let Some(start) = this.press else { return };
                if ev.pressed_button != Some(MouseButton::Left) {
                    this.press = None;
                    return;
                }
                let d = ev.position - start;
                if f32::from(d.x).abs() + f32::from(d.y).abs() >= 4. {
                    this.press = None;
                    window.start_window_move();
                }
            }))
            .on_mouse_down(MouseButton::Right, |ev: &MouseDownEvent, window, _| {
                if cfg!(target_os = "linux")
                    && matches!(window.window_decorations(), Decorations::Client { .. })
                    && window.window_controls().window_menu
                {
                    window.show_window_menu(ev.position);
                }
            })
            .child(self.video.clone())
    }

    fn render_controls(
        &self,
        p: &Palette,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let fullscreen = window.is_fullscreen();
        let stats = self.video.read(cx).show_stats;
        // macOS keeps its traffic lights; a Linux window manager that draws
        // its own title bar (server decorations) has the buttons there.
        let own_window_buttons = !fullscreen
            && (cfg!(target_os = "windows")
                || (cfg!(target_os = "linux")
                    && matches!(window.window_decorations(), Decorations::Client { .. })));
        let supported = window.window_controls();
        let red = p.red;
        let mut bar =
            div()
                .id("popout-controls")
                .absolute()
                .top(px(RESIZE_CORNER))
                .right(px(RESIZE_CORNER))
                .flex()
                .items_center()
                .gap_1()
                .p_1()
                .rounded(px(8.))
                .bg(gpui_kit::black().opacity(0.6))
                .occlude()
                .hover_listener_mode(HoverListenerMode::InputModalityIndependent)
                .on_hover(cx.listener(|this, hovered: &bool, window, cx| {
                    this.set_pinned(*hovered, window, cx)
                }))
                .child(
                    overlay_button(
                        "popout-stats",
                        IconName::Activity,
                        if stats {
                            "Hide stream stats"
                        } else {
                            "Show stream stats"
                        },
                    )
                    .when(stats, |b| b.bg(gpui_kit::white().opacity(0.2)))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.video.update(cx, |v, cx| {
                            v.show_stats = !v.show_stats;
                            cx.notify();
                        });
                        cx.notify();
                    })),
                )
                .child(
                    overlay_button(
                        "popout-popin",
                        IconName::PictureInPicture2,
                        "Back to main window",
                    )
                    .on_click(cx.listener(|this, _, window, cx| this.close(window, cx))),
                )
                .child(
                    overlay_button(
                        "popout-fullscreen",
                        if fullscreen {
                            IconName::Shrink
                        } else {
                            IconName::Expand
                        },
                        if fullscreen {
                            "Exit fullscreen (Esc)"
                        } else {
                            "Fullscreen (F)"
                        },
                    )
                    .on_click(|_, window, _| window.toggle_fullscreen()),
                );
        if own_window_buttons {
            bar = bar.child(
                div()
                    .w(px(1.))
                    .h(px(20.))
                    .mx_1()
                    .bg(gpui_kit::white().opacity(0.25)),
            );
            if supported.minimize && window.is_minimizable() {
                bar = bar.child(window_button(
                    overlay_button("popout-minimize", IconName::Minus, "Minimize"),
                    WindowControlArea::Min,
                    |window, _| window.minimize_window(),
                ));
            }
            if supported.maximize && window.is_resizable() {
                let maximized = window.is_maximized();
                bar = bar.child(window_button(
                    overlay_button(
                        "popout-maximize",
                        if maximized {
                            IconName::Copy
                        } else {
                            IconName::Square
                        },
                        if maximized { "Restore" } else { "Maximize" },
                    ),
                    WindowControlArea::Max,
                    |window, _| window.zoom_window(),
                ));
            }
            let close = overlay_button("popout-close", IconName::X, "Close (back to main window)")
                .hover(move |s| s.bg(red));
            let close = if cfg!(target_os = "windows") {
                close.window_control_area(WindowControlArea::Close)
            } else {
                close.on_click(cx.listener(|this, _, window, cx| this.close(window, cx)))
            };
            bar = bar.child(close);
        }
        bar
    }

    fn render_info_bar(&self, p: &Palette, cx: &mut Context<Self>) -> Stateful<Div> {
        let info = &self.info;
        let muted = info.volume <= 0.;
        div()
            .id("popout-info")
            .absolute()
            .bottom(px(RESIZE_CORNER))
            .left(px(RESIZE_CORNER))
            .right(px(RESIZE_CORNER))
            .flex()
            .items_center()
            .gap_2()
            .px_3()
            .py_2()
            .rounded(px(8.))
            .bg(gpui_kit::black().opacity(0.6))
            .occlude()
            .hover_listener_mode(HoverListenerMode::InputModalityIndependent)
            .on_hover(
                cx.listener(|this, hovered: &bool, window, cx| {
                    this.set_pinned(*hovered, window, cx)
                }),
            )
            .child(live_badge(p))
            .child(
                div()
                    .flex_shrink_0()
                    .text_sm()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(gpui_kit::white())
                    .child(info.name.clone()),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_xs()
                    .text_color(gpui_kit::white().opacity(0.7))
                    .child(info.source.clone()),
            )
            .when(info.has_audio, |d| {
                d.child(
                    overlay_button(
                        "popout-mute",
                        if muted {
                            IconName::VolumeX
                        } else {
                            IconName::Volume2
                        },
                        if muted {
                            "Unmute stream (M)"
                        } else {
                            "Mute stream (M)"
                        },
                    )
                    .on_click(cx.listener(|this, _, window, cx| this.toggle_mute(window, cx))),
                )
                .child(
                    div().w(px(96.)).flex_shrink_0().child(
                        Slider::new(&self.volume_slider).accessibility_label("Stream volume"),
                    ),
                )
                .child(
                    div()
                        .w(px(36.))
                        .flex_shrink_0()
                        .text_xs()
                        .text_color(gpui_kit::white().opacity(0.8))
                        .child(format!("{:.0}%", info.volume * 100.)),
                )
            })
    }
}

/// Hides the controls when the pointer leaves the window.
///
/// Not an `on_hover`/`on_mouse_exit` on the root: those go through the root's
/// hitbox, which the occluding control bars cover (so hovering a bar would
/// count as leaving), and they also turn false on key presses and during a
/// slider drag. A raw `MouseExitEvent` is the platform telling us the pointer
/// really left. An exit with a button held (a drag or a window move under way)
/// is ignored.
fn window_exit_watcher(view: WeakEntity<PopoutView>) -> impl IntoElement {
    canvas(
        |_, _, _| (),
        move |_, _, window, _| {
            let view = view.clone();
            window.on_mouse_event(move |ev: &MouseExitEvent, phase, _, cx| {
                if phase != DispatchPhase::Bubble
                    || ev.pressed_button.is_some()
                    || cx.has_active_drag()
                {
                    return;
                }
                let _ = view.update(cx, |this, cx| {
                    if this.chrome.hide_now() {
                        cx.notify();
                    }
                });
            });
        },
    )
    .absolute()
    .size_full()
}

/// Round translucent button for the overlay (the video is dark in both themes).
fn overlay_button(id: &'static str, glyph: IconName, tip: &'static str) -> Stateful<Div> {
    div()
        .id(id)
        .size(px(28.))
        .flex_shrink_0()
        .rounded(px(6.))
        .hover(|s| s.bg(gpui_kit::white().opacity(0.15)))
        .cursor_pointer()
        .flex()
        .items_center()
        .justify_center()
        .tooltip(move |window, cx| Tooltip::new(tip).build(window, cx))
        .child(icon(glyph, 16., gpui_kit::white()))
}

/// A minimize/maximize button: Windows handles these natively from the hit
/// test (like the title bar's), elsewhere we click through to `action`.
fn window_button(
    button: Stateful<Div>,
    area: WindowControlArea,
    action: fn(&mut Window, &mut App),
) -> Stateful<Div> {
    if cfg!(target_os = "windows") {
        button.window_control_area(area)
    } else {
        button.on_click(move |_, window, cx| {
            cx.stop_propagation();
            action(window, cx);
        })
    }
}

impl Render for PopoutView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = Palette::current(cx);
        let visible = self.chrome.visible();
        let zones = match window.window_decorations() {
            Decorations::Client { tiling }
                if cfg!(target_os = "linux")
                    && window.is_resizable()
                    && !window.is_fullscreen()
                    && !window.is_maximized() =>
            {
                resize_zones(tiling)
            }
            _ => Vec::new(),
        };
        div()
            .id("popout")
            .key_context("Popout")
            .track_focus(&self.focus)
            .on_action(
                cx.listener(|_, _: &PopoutToggleFullscreen, window, _| window.toggle_fullscreen()),
            )
            .on_action(cx.listener(|this, _: &PopoutEscape, window, _| this.escape(window)))
            .on_action(
                cx.listener(|this, _: &PopoutToggleMute, window, cx| this.toggle_mute(window, cx)),
            )
            .on_action(cx.listener(|this, _: &PopoutClose, window, cx| this.close(window, cx)))
            .relative()
            .size_full()
            .overflow_hidden()
            .bg(gpui_kit::black())
            .on_mouse_move(
                cx.listener(|this, _: &MouseMoveEvent, window, cx| this.activity(window, cx)),
            )
            .child(self.render_surface(cx))
            .child(window_exit_watcher(cx.entity().downgrade()))
            .when(visible, |d| {
                d.child(self.render_controls(&p, window, cx))
                    .child(self.render_info_bar(&p, cx))
            })
            .children(zones.into_iter().map(resize_zone))
    }
}

impl GlanceApp {
    /// Fill the window (and go OS-fullscreen) with one stream.
    pub fn enter_fullscreen(&mut self, peer: PeerId, window: &mut Window, cx: &mut Context<Self>) {
        self.immersive = Some(peer);
        // Keyboard shortcuts (Esc) follow focus; make sure we have it.
        self.focus.focus(window, cx);
        if !window.is_fullscreen() {
            window.toggle_fullscreen();
        }
        cx.notify();
    }

    pub fn exit_fullscreen(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.immersive.take().is_some() && window.is_fullscreen() {
            window.toggle_fullscreen();
        }
        cx.notify();
    }

    /// Move a stream into its own window.
    pub fn pop_out(&mut self, peer: PeerId, cx: &mut Context<Self>) {
        let me = self.me.id;
        if peer == me || self.room.as_ref().is_none_or(|r| r.handle.is_none()) {
            return;
        }
        // Already out: bring that window forward.
        if let Some(window) = self
            .room
            .as_ref()
            .and_then(|r| r.popouts.get(&peer))
            .map(|p| p.window)
        {
            let _ = window.update(cx, |_, window, _| window.activate_window());
            return;
        }
        let Some(info) = self.popout_info(peer) else {
            return;
        };
        let stream_size = self
            .room
            .as_ref()
            .and_then(|r| r.participants.iter().find(|p| p.profile.id == peer))
            .and_then(|p| p.stream.as_ref())
            .map(|s| (s.width, s.height));
        let (w, h) = popout_size(stream_size);
        let title = format!("{}'s screen — glance", info.name);
        // Stop the in-window copy; the pop-out gets its own fresh stream.
        self.stop_watching(peer, cx);
        let app = cx.entity();
        // No title bar: the video is the drag area and the controls float
        // on top (see the module docs).
        let options = WindowOptions {
            titlebar: Some(TitlebarOptions {
                title: Some(title.into()),
                ..TitleBar::title_bar_options()
            }),
            window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
                None,
                size(px(w), px(h)),
                cx,
            ))),
            window_min_size: Some(size(px(320.), px(180.))),
            window_decorations: Some(WindowDecorations::Client),
            app_id: Some("glance".into()),
            ..TitleBar::window_options()
        };
        let opened = gpui_kit::open_window(options, cx, |window, cx| {
            cx.new(|cx| PopoutView::new(peer, &app, info, window, cx))
        });
        let (window, view) = match opened {
            Ok(opened) => opened,
            Err(e) => {
                tracing::warn!("pop-out window failed: {e:#}");
                self.watch(peer, cx);
                return;
            }
        };
        let video = view.read(cx).video.clone();
        if let Some(room) = self.room.as_mut() {
            room.popouts.insert(peer, Popout { video, window });
        }
        tracing::info!("popped out; requesting stream");
        self.request_stream(peer);
        cx.notify();
    }

    /// Bring a popped-out stream back into the main window and close its window.
    pub fn pop_in(&mut self, peer: PeerId, cx: &mut Context<Self>) {
        let Some(window) = self
            .room
            .as_ref()
            .and_then(|r| r.popouts.get(&peer))
            .map(|p| p.window)
        else {
            return;
        };
        self.popout_closed(peer, window.window_id(), cx);
        cx.defer(move |cx| {
            let _ = window.update(cx, |_, window, _| window.remove_window());
        });
    }

    /// The pop-out `window` for `peer` closed (or is closing): show the
    /// stream in the main window again. Does nothing for a window that is no
    /// longer the peer's pop-out, so every close path can call it.
    pub fn popout_closed(&mut self, peer: PeerId, window: WindowId, cx: &mut Context<Self>) {
        let Some(room) = self.room.as_mut() else {
            return;
        };
        if !room
            .popouts
            .get(&peer)
            .is_some_and(|p| p.window.window_id() == window)
        {
            return;
        }
        tracing::info!("pop in");
        room.popouts.remove(&peer);
        let live = room
            .participants
            .iter()
            .any(|p| p.profile.id == peer && p.stream.is_some());
        if let Some(h) = room.handle.as_ref()
            && let Ok(id) = glance_net::EndpointId::from_bytes(&peer.0)
        {
            h.send(glance_net::Command::Unwatch(
                id,
                glance_proto::Track::Screen,
            ));
        }
        if live {
            self.watch(peer, cx);
        }
        cx.notify();
    }

    fn request_stream(&self, peer: PeerId) {
        if let Some(h) = self.room.as_ref().and_then(|r| r.handle.as_ref())
            && let Ok(id) = glance_net::EndpointId::from_bytes(&peer.0)
        {
            h.send(glance_net::Command::Watch(id, glance_proto::Track::Screen));
        }
    }
}

#[cfg(test)]
mod tests {
    // Not `super::*`: the gpui_kit glob would shadow `#[test]` with GPUI's test macro.
    use super::{
        AutoHide, KEYS, TimerOutcome, popout_size, resize_cursor, resize_zones, toggle_mute,
    };
    use gpui_kit::{CursorStyle, ResizeEdge, Tiling};
    use std::time::{Duration, Instant};

    const SEC: Duration = Duration::from_secs(1);

    #[test]
    fn controls_hide_two_seconds_after_the_last_movement() {
        let t0 = Instant::now();
        let mut c = AutoHide::new(t0);
        assert!(!c.visible());
        // First movement shows and arms one timer.
        assert_eq!(c.activity(t0), Some(AutoHide::DELAY));
        assert!(c.visible());
        // More movement while it runs starts no new timer.
        assert_eq!(c.activity(t0 + SEC), None);
        assert_eq!(c.activity(t0 + SEC + SEC / 2), None);
        // The timer fires 2 s after the first move: wait out the rest.
        assert_eq!(
            c.timer_fired(t0 + 2 * SEC, false),
            TimerOutcome::Wait(SEC + SEC / 2)
        );
        assert!(c.visible());
        // Moving now doesn't double the timer (it re-armed itself).
        assert_eq!(c.activity(t0 + 2 * SEC), None);
        assert_eq!(
            c.timer_fired(t0 + 3 * SEC + SEC / 2, false),
            TimerOutcome::Wait(SEC / 2)
        );
        assert_eq!(c.timer_fired(t0 + 4 * SEC, false), TimerOutcome::Hide);
        assert!(!c.visible());
        // Next movement shows again and arms a fresh timer.
        assert_eq!(c.activity(t0 + 10 * SEC), Some(AutoHide::DELAY));
        assert!(c.visible());
    }

    #[test]
    fn controls_stay_while_the_pointer_rests_on_them() {
        let t0 = Instant::now();
        let mut c = AutoHide::new(t0);
        assert!(c.activity(t0).is_some());
        assert_eq!(c.set_pinned(true, t0 + SEC), None);
        // The running timer ends without hiding.
        assert_eq!(c.timer_fired(t0 + 5 * SEC, false), TimerOutcome::Stop);
        assert!(c.visible());
        // Movement over the controls arms nothing while pinned.
        assert_eq!(c.activity(t0 + 6 * SEC), None);
        // Leaving them restarts the countdown from now.
        assert_eq!(c.set_pinned(false, t0 + 7 * SEC), Some(AutoHide::DELAY));
        assert_eq!(c.timer_fired(t0 + 8 * SEC, false), TimerOutcome::Wait(SEC));
        assert_eq!(c.timer_fired(t0 + 9 * SEC, false), TimerOutcome::Hide);
    }

    #[test]
    fn leaving_the_window_hides_at_once_and_stops_the_timer() {
        let t0 = Instant::now();
        let mut c = AutoHide::new(t0);
        assert!(c.activity(t0).is_some());
        assert!(c.set_pinned(true, t0).is_none());
        assert!(c.hide_now());
        assert!(!c.hide_now(), "already hidden");
        // The control's own hover-out arriving after the window's doesn't re-show.
        assert_eq!(c.set_pinned(false, t0), None);
        assert!(!c.visible());
        assert_eq!(c.timer_fired(t0 + 2 * SEC, false), TimerOutcome::Stop);
        assert_eq!(c.activity(t0 + 3 * SEC), Some(AutoHide::DELAY));
    }

    #[test]
    fn controls_stay_while_the_volume_slider_is_dragged() {
        let t0 = Instant::now();
        let mut c = AutoHide::new(t0);
        assert!(c.activity(t0).is_some());
        // Pressing the slider starts a gpui drag, which reads as unhovered.
        assert_eq!(c.set_pinned(true, t0 + SEC / 2), None);
        // The first timer still runs, so no second one starts.
        assert_eq!(c.set_pinned(false, t0 + SEC), None);
        // The timer fires mid-drag: no hiding, wait a full delay.
        assert_eq!(
            c.timer_fired(t0 + 3 * SEC, true),
            TimerOutcome::Wait(AutoHide::DELAY)
        );
        assert!(c.visible());
        // Drag released (pointer off the bar): the countdown runs out.
        assert_eq!(c.timer_fired(t0 + 5 * SEC, false), TimerOutcome::Hide);
    }

    #[test]
    fn initial_size_follows_the_stream_aspect() {
        assert_eq!(popout_size(Some((1920, 1080))), (960., 540.));
        assert_eq!(popout_size(None), (960., 540.));
        assert_eq!(popout_size(Some((0, 0))), (960., 540.));
        // 4:3 and portrait are height-limited.
        assert_eq!(popout_size(Some((1600, 1200))), (800., 600.));
        assert_eq!(popout_size(Some((1080, 1920))), (338., 600.));
        // Super-ultrawide is width-limited; absurd ratios are clamped.
        assert_eq!(popout_size(Some((5120, 1440))), (960., 270.));
        assert_eq!(popout_size(Some((10000, 10))), (960., 240.));
    }

    #[test]
    fn mute_toggles_back_to_the_last_volume() {
        assert_eq!(toggle_mute(0.7, 1.0), (0.0, 0.7));
        assert_eq!(toggle_mute(0.0, 0.7), (0.7, 0.7));
        // Muted with nothing to restore: back to 100%.
        assert_eq!(toggle_mute(0.0, 0.0), (1.0, 0.0));
    }

    #[test]
    fn resize_zones_skip_tiled_edges() {
        let all = resize_zones(Tiling::default());
        assert_eq!(all.len(), 8);
        // Corners last, so they sit on top of the edge strips.
        assert!(matches!(all[4..], [ResizeEdge::TopLeft, ..]));
        assert!(resize_zones(Tiling::tiled()).is_empty());
        // Snapped to the left half: no left edge, no left corners.
        let left = resize_zones(Tiling {
            left: true,
            top: true,
            bottom: true,
            right: false,
        });
        assert_eq!(left, vec![ResizeEdge::Right]);
        let top = resize_zones(Tiling {
            top: true,
            ..Tiling::default()
        });
        assert_eq!(
            top,
            vec![
                ResizeEdge::Bottom,
                ResizeEdge::Left,
                ResizeEdge::Right,
                ResizeEdge::BottomLeft,
                ResizeEdge::BottomRight,
            ]
        );
    }

    #[test]
    fn resize_cursors_point_along_the_edge() {
        assert_eq!(resize_cursor(ResizeEdge::Top), CursorStyle::ResizeUpDown);
        assert_eq!(
            resize_cursor(ResizeEdge::Right),
            CursorStyle::ResizeLeftRight
        );
        assert_eq!(
            resize_cursor(ResizeEdge::BottomRight),
            CursorStyle::ResizeUpLeftDownRight
        );
        assert_eq!(
            resize_cursor(ResizeEdge::BottomLeft),
            CursorStyle::ResizeUpRightDownLeft
        );
    }

    #[test]
    fn popout_keys_parse() {
        for k in KEYS {
            assert!(gpui_kit::Keystroke::parse(k).is_ok(), "{k}");
        }
    }
}
