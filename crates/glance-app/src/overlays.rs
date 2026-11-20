//! Full-window overlays: the Go Live picker and the settings screen.

use glance_core::{FRAME_RATES, Resolution, StreamInfo};
use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::switch::Switch;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::app::{GlanceApp, GoLive, SettingsPage};
use crate::theme::Palette;
use crate::ui::{Avatar, icon, pill, section_label};

impl Default for GoLive {
    fn default() -> Self {
        Self {
            windows_tab: false,
            source: 0,
            resolution: Resolution::P1080,
            fps: 60,
            audio: true,
        }
    }
}

/// Placeholder capture sources until M4 enumerates real ones.
const SCREENS: [(&str, u32, u32); 2] = [("Screen 1", 3840, 2160), ("Screen 2", 2560, 1440)];
const WINDOWS: [(&str, u32, u32); 3] = [
    ("Terminal", 1920, 1080),
    ("Browser", 2560, 1440),
    ("Game", 5120, 1440),
];

impl GlanceApp {
    pub fn render_go_live(&mut self, p: &Palette, cx: &mut Context<Self>) -> impl IntoElement {
        let gl = self.go_live.as_ref().expect("go live open");
        let sources: &[(&str, u32, u32)] = if gl.windows_tab { &WINDOWS } else { &SCREENS };
        let (src_name, src_w, src_h) = sources[gl.source.min(sources.len() - 1)];
        let out = gl.resolution.output_size((src_w, src_h));
        // No encoder probing yet (M10); assume hardware is present.
        let hw_available = true;

        let tab = |id: &'static str, label: &'static str, windows: bool, selected: bool| {
            div()
                .id(id)
                .px_3()
                .py_1()
                .cursor_pointer()
                .border_b_2()
                .border_color(if selected {
                    p.brand
                } else {
                    gpui_kit::transparent_black()
                })
                .text_color(if selected {
                    p.text_header
                } else {
                    p.text_muted
                })
                .child(label)
                .on_click(cx.listener(move |this, _, _, cx| {
                    if let Some(gl) = this.go_live.as_mut() {
                        gl.windows_tab = windows;
                        gl.source = 0;
                    }
                    cx.notify();
                }))
        };

        let source_cards = sources.iter().enumerate().map(|(i, (name, w, h))| {
            let selected = i == gl.source;
            div()
                .id(SharedString::from(format!("src-{i}")))
                .w(px(200.))
                .flex()
                .flex_col()
                .gap_1()
                .cursor_pointer()
                .on_click(cx.listener(move |this, _, _, cx| {
                    if let Some(gl) = this.go_live.as_mut() {
                        gl.source = i;
                    }
                    cx.notify();
                }))
                .child(
                    div()
                        .h(px(112.))
                        .rounded(px(6.))
                        .border_2()
                        .border_color(if selected { p.brand } else { p.bg_rail })
                        .bg(p.bg_rail)
                        .flex()
                        .items_center()
                        .justify_center()
                        .child(icon(IconName::Monitor, 36., p.text_muted)),
                )
                .child(
                    div()
                        .text_sm()
                        .text_color(if selected {
                            p.text_header
                        } else {
                            p.text_normal
                        })
                        .child(name.to_string()),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(p.text_muted)
                        .child(format!("{w}×{h}")),
                )
        });

        let res_pills = Resolution::ALL.into_iter().map(|r| {
            let disabled = r.needs_hardware(gl.fps) && !hw_available;
            pill(
                SharedString::from(format!("res-{}", r.label())),
                r.label(),
                r == gl.resolution,
                disabled,
                p,
            )
            .when(!disabled, |d| {
                d.on_click(cx.listener(move |this, _, _, cx| {
                    if let Some(gl) = this.go_live.as_mut() {
                        gl.resolution = r;
                    }
                    cx.notify();
                }))
            })
        });
        let fps_pills = FRAME_RATES.into_iter().map(|f| {
            pill(
                SharedString::from(format!("fps-{f}")),
                format!("{f} FPS"),
                f == gl.fps,
                false,
                p,
            )
            .on_click(cx.listener(move |this, _, _, cx| {
                if let Some(gl) = this.go_live.as_mut() {
                    gl.fps = f;
                }
                cx.notify();
            }))
        });
        let audio = gl.audio;
        let windows_tab = gl.windows_tab;

        modal_backdrop("go-live-backdrop").child(
            div()
                .id("go-live")
                .occlude()
                .w(px(720.))
                .rounded(px(8.))
                .bg(p.bg_content)
                .flex()
                .flex_col()
                .overflow_hidden()
                .child(
                    div()
                        .p_4()
                        .flex()
                        .items_center()
                        .justify_between()
                        .child(
                            div()
                                .text_xl()
                                .font_weight(FontWeight::BOLD)
                                .text_color(p.text_header)
                                .child("Screen Share"),
                        )
                        .child(close_button("go-live-close", p).on_click(cx.listener(|this, _, _, cx| {
                            this.go_live = None;
                            cx.notify();
                        }))),
                )
                .child(
                    div()
                        .px_4()
                        .flex()
                        .gap_2()
                        .border_b_1()
                        .border_color(p.bg_selected)
                        .child(tab("tab-screens", "Screens", false, !windows_tab))
                        .child(tab("tab-windows", "Applications", true, windows_tab)),
                )
                .child(div().p_4().flex().flex_wrap().gap_3().children(source_cards))
                .child(
                    div()
                        .p_4()
                        .flex()
                        .flex_col()
                        .gap_3()
                        .bg(p.bg_sidebar)
                        .child(section_label(p, "STREAM QUALITY"))
                        .child(
                            div()
                                .flex()
                                .flex_wrap()
                                .gap_6()
                                .child(
                                    div()
                                        .flex()
                                        .flex_col()
                                        .gap_2()
                                        .child(div().text_xs().text_color(p.text_muted).child("RESOLUTION"))
                                        .child(div().flex().flex_wrap().gap_1().children(res_pills)),
                                )
                                .child(
                                    div()
                                        .flex()
                                        .flex_col()
                                        .gap_2()
                                        .child(div().text_xs().text_color(p.text_muted).child("FRAME RATE"))
                                        .child(div().flex().flex_wrap().gap_1().children(fps_pills)),
                                ),
                        )
                        .child(div().text_xs().text_color(p.text_muted).child(format!(
                            "{src_name} will stream at {}×{}. Viewers on smaller tiles get it scaled to fit.",
                            out.0, out.1
                        )))
                        .child(
                            Switch::new("share-audio")
                                .checked(audio)
                                .label("Also share audio")
                                .on_click(cx.listener(|this, checked: &bool, _, cx| {
                                    if let Some(gl) = this.go_live.as_mut() {
                                        gl.audio = *checked;
                                    }
                                    cx.notify();
                                })),
                        ),
                )
                .child(
                    div()
                        .p_4()
                        .flex()
                        .justify_end()
                        .gap_2()
                        .bg(p.bg_sidebar)
                        .child(Button::new("go-live-cancel").ghost().label("Cancel").on_click(cx.listener(
                            |this, _, _, cx| {
                                this.go_live = None;
                                cx.notify();
                            },
                        )))
                        .child(Button::new("go-live-start").primary().label("Go Live").on_click(cx.listener(
                            move |this, _, _, cx| {
                                this.start_streaming(src_name, out, cx);
                            },
                        ))),
                ),
        )
    }

    fn start_streaming(&mut self, source: &str, size: (u32, u32), cx: &mut Context<Self>) {
        let Some(gl) = self.go_live.take() else {
            return;
        };
        let me = self.me.id;
        if let Some(room) = self.room.as_mut()
            && let Some(x) = room.participants.iter_mut().find(|x| x.profile.id == me)
        {
            x.stream = Some(StreamInfo {
                width: size.0,
                height: size.1,
                fps: gl.fps,
                has_audio: gl.audio,
                source: source.to_string(),
            });
        }
        self.push_state();
        cx.notify();
    }

    pub fn render_settings(&mut self, p: &Palette, cx: &mut Context<Self>) -> impl IntoElement {
        let page = self.settings.page;
        let nav = SettingsPage::ALL.into_iter().map(|pg| {
            let selected = pg == page;
            let hover = p.bg_hover;
            div()
                .id(SharedString::from(format!("settings-{}", pg.label())))
                .px_3()
                .py(px(6.))
                .rounded(px(4.))
                .cursor_pointer()
                .text_color(if selected {
                    p.text_header
                } else {
                    p.text_muted
                })
                .when(selected, |d| d.bg(p.bg_selected))
                .hover(move |s| s.bg(hover))
                .child(pg.label())
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.settings.page = pg;
                    cx.notify();
                }))
        });

        div()
            .id("settings")
            .occlude()
            .absolute()
            .inset_0()
            .flex()
            .bg(p.bg_content)
            .child(
                div().flex_1().flex().justify_end().bg(p.bg_sidebar).child(
                    div()
                        .w(px(218.))
                        .pt_16()
                        .pr_2()
                        .flex()
                        .flex_col()
                        .gap(px(2.))
                        .child(div().px_3().pb_1().child(section_label(p, "USER SETTINGS")))
                        .children(nav),
                ),
            )
            .child(
                div()
                    .id("settings-content")
                    .flex_basis(px(800.))
                    .flex_shrink(1.)
                    .overflow_y_scroll()
                    .pt_16()
                    .px_10()
                    .child(self.settings_page(p, page, cx)),
            )
            .child(
                div().flex_1().pt_16().pl_4().child(
                    div()
                        .flex()
                        .flex_col()
                        .items_center()
                        .gap_1()
                        .child(close_button("settings-close", p).on_click(cx.listener(
                            |this, _, _, cx| {
                                this.settings_open = false;
                                cx.notify();
                            },
                        )))
                        .child(div().text_xs().text_color(p.text_muted).child("ESC")),
                ),
            )
    }

    fn settings_page(
        &mut self,
        p: &Palette,
        page: SettingsPage,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let title = div()
            .pb_5()
            .text_xl()
            .font_weight(FontWeight::BOLD)
            .text_color(p.text_header)
            .child(page.label());
        macro_rules! toggle {
            ($id:literal, $label:literal, $desc:literal, $field:ident) => {
                setting_row(
                    p,
                    $label,
                    $desc,
                    Switch::new($id)
                        .checked(self.settings.$field)
                        .on_click(cx.listener(|this, v: &bool, _, cx| {
                            this.settings.$field = *v;
                            cx.notify();
                        })),
                )
            };
        }
        let body = match page {
            SettingsPage::Account => div()
                .p_4()
                .rounded(px(8.))
                .bg(p.bg_rail)
                .flex()
                .items_center()
                .gap_4()
                .child(Avatar::new(&self.me, 80.))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .child(
                            div()
                                .text_xl()
                                .font_weight(FontWeight::BOLD)
                                .text_color(p.text_header)
                                .child(self.me.name.clone()),
                        )
                        .child(div().text_sm().text_color(p.text_muted).child(format!(
                            "Contact ID {} · shared with people you call",
                            self.me.id.short()
                        ))),
                )
                .into_any_element(),
            SettingsPage::Voice => div()
                .flex()
                .flex_col()
                .child(info_row(p, "Input device", "Default — System microphone"))
                .child(info_row(p, "Output device", "Default — System speakers"))
                .child(info_row(p, "Camera", "Default camera"))
                .child(toggle!(
                    "ns",
                    "Noise suppression",
                    "Filters background noise from your mic.",
                    noise_suppression
                ))
                .child(toggle!(
                    "aec",
                    "Echo cancellation",
                    "Stops others hearing themselves through your speakers.",
                    echo_cancel
                ))
                .into_any_element(),
            SettingsPage::Streaming => div()
                .flex()
                .flex_col()
                .child(info_row(p, "Default quality", "1080p · 60 FPS"))
                .child(info_row(
                    p,
                    "Video encoder",
                    "Detected in M10 (hardware preferred, software fallback)",
                ))
                .into_any_element(),
            SettingsPage::Contacts => div()
                .flex()
                .flex_col()
                .child(toggle!(
                    "join",
                    "Let contacts join my calls",
                    "Contacts see a Join button while you're in a call.",
                    let_contacts_join
                ))
                .child(toggle!(
                    "tray",
                    "Keep running in the tray",
                    "Closing the window keeps glance reachable so contacts can ring you.",
                    tray
                ))
                .child(toggle!(
                    "autostart",
                    "Start glance on login",
                    "Opens quietly in the tray when you sign in.",
                    autostart
                ))
                .into_any_element(),
            SettingsPage::Files => div()
                .flex()
                .flex_col()
                .child(info_row(p, "Save files to", "Downloads"))
                .child(toggle!(
                    "preview",
                    "Preview images automatically",
                    "Images and short videos under 25 MB load inline.",
                    auto_preview
                ))
                .into_any_element(),
            SettingsPage::Appearance => div()
                .flex()
                .flex_col()
                .child(toggle!(
                    "dark",
                    "Dark theme",
                    "Turn off for the light theme.",
                    dark
                ))
                .into_any_element(),
            SettingsPage::Keybinds => div()
                .flex()
                .flex_col()
                .child(info_row(p, "Toggle mute", "Ctrl + Shift + M"))
                .child(info_row(p, "Toggle deafen", "Ctrl + Shift + D"))
                .child(info_row(p, "Quit", "Ctrl + Q"))
                .into_any_element(),
            SettingsPage::Connection => div()
                .flex()
                .flex_col()
                .child(info_row(p, "Relay", "Default public relays"))
                .child(info_row(p, "Custom relay URL", "Not set"))
                .into_any_element(),
        };
        div()
            .max_w(px(660.))
            .child(title)
            .child(body)
            .into_any_element()
    }
}

fn modal_backdrop(id: &'static str) -> Stateful<Div> {
    div()
        .id(id)
        .occlude()
        .absolute()
        .inset_0()
        .bg(gpui_kit::black().opacity(0.7))
        .flex()
        .items_center()
        .justify_center()
}

fn close_button(id: &'static str, p: &Palette) -> Stateful<Div> {
    let hover = p.bg_hover;
    div()
        .id(id)
        .size(px(36.))
        .rounded_full()
        .border_2()
        .border_color(p.interactive)
        .cursor_pointer()
        .hover(move |s| s.bg(hover))
        .flex()
        .items_center()
        .justify_center()
        .child(icon(IconName::X, 18., p.interactive))
}

fn setting_row(
    p: &Palette,
    label: &'static str,
    desc: &'static str,
    control: impl IntoElement,
) -> impl IntoElement {
    div()
        .py_4()
        .border_b_1()
        .border_color(p.bg_selected)
        .flex()
        .items_center()
        .gap_4()
        .child(
            div()
                .flex_1()
                .flex()
                .flex_col()
                .gap_1()
                .child(
                    div()
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(p.text_header)
                        .child(label),
                )
                .child(div().text_sm().text_color(p.text_muted).child(desc)),
        )
        .child(control)
}

fn info_row(p: &Palette, label: &'static str, value: &'static str) -> impl IntoElement {
    div()
        .py_4()
        .border_b_1()
        .border_color(p.bg_selected)
        .flex()
        .flex_col()
        .gap_1()
        .child(section_label(p, label.to_uppercase()))
        .child(
            div()
                .px_3()
                .py_2()
                .rounded(px(4.))
                .bg(p.bg_input)
                .text_color(p.text_normal)
                .child(value),
        )
}
