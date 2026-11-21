//! Full-window overlays: the Go Live picker and the settings screen.

use glance_core::{FRAME_RATES, Resolution, StreamInfo};
use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::input::Input;
use gpui_kit::component::slider::{Slider, SliderState};
use gpui_kit::component::switch::Switch;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use glance_media::capture::{Source, SourceKind};

use crate::app::{GlanceApp, SettingsPage};
use crate::theme::Palette;
use crate::ui::{Avatar, icon, pill, presence_color, section_label};
use gpui_kit::component::Disableable;
use gpui_kit::component::Sizable;

impl GlanceApp {
    pub fn render_go_live(&mut self, p: &Palette, cx: &mut Context<Self>) -> impl IntoElement {
        let gl = self.go_live.as_ref().expect("go live open");
        let kind = if gl.windows_tab {
            SourceKind::Window
        } else {
            SourceKind::Screen
        };
        let visible: Vec<Source> = gl
            .sources
            .iter()
            .filter(|s| s.kind == kind)
            .cloned()
            .collect();
        let selected_src = visible.get(gl.source).cloned();
        let out = selected_src
            .as_ref()
            .and_then(|s| s.size)
            .map(|sz| gl.resolution.output_size(sz));
        let gpu = self.settings.hardware_encoding
            && matches!(glance_media::codec::hardware_encoder_known(), Some(Some(_)));
        let heavy = gl.resolution.needs_hardware(gl.fps) && !gpu;

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

        let source_cards: Vec<AnyElement> = visible
            .iter()
            .enumerate()
            .map(|(i, src)| {
                let selected = i == gl.source;
                let glyph = match src.kind {
                    SourceKind::Screen => IconName::Monitor,
                    SourceKind::Window => IconName::AppWindow,
                };
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
                            gl.error = None;
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
                            .child(icon(glyph, 36., p.text_muted)),
                    )
                    .child(
                        div()
                            .text_sm()
                            .truncate()
                            .text_color(if selected {
                                p.text_header
                            } else {
                                p.text_normal
                            })
                            .child(src.name.clone()),
                    )
                    .children(src.size.map(|(w, h)| {
                        div()
                            .text_xs()
                            .text_color(p.text_muted)
                            .child(format!("{w}×{h}"))
                    }))
                    .into_any_element()
            })
            .collect();

        let editing = gl.editing;
        // What the running stream shows now (editing only).
        let sharing_now = self
            .room
            .as_ref()
            .and_then(|r| r.participants.iter().find(|x| x.profile.id == self.me.id))
            .and_then(|x| x.stream.as_ref())
            .map(|s| s.source.clone())
            .unwrap_or_default();
        let picker_body: AnyElement = if gl.system_picker && editing {
            div()
                .p_4()
                .flex()
                .items_center()
                .gap_3()
                .child(icon(IconName::ScreenShare, 32., p.brand))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .flex_col()
                        .gap_1()
                        .child(
                            div()
                                .text_color(p.text_header)
                                .font_weight(FontWeight::SEMIBOLD)
                                .truncate()
                                .child(format!("Sharing {sharing_now}")),
                        )
                        .child(div().text_sm().text_color(p.text_muted).child(
                            "Pick another screen or window; viewers switch over without leaving.",
                        )),
                )
                .child(
                    Button::new("go-live-change-source")
                        .label("Change source")
                        .on_click(cx.listener(|this, _, _, cx| this.change_stream_source(cx))),
                )
                .into_any_element()
        } else if gl.system_picker {
            div()
                .p_4()
                .flex()
                .items_center()
                .gap_3()
                .child(icon(IconName::ScreenShare, 32., p.brand))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_1()
                        .child(
                            div()
                                .text_color(p.text_header)
                                .font_weight(FontWeight::SEMIBOLD)
                                .child("Your system will ask what to share"),
                        )
                        .child(div().text_sm().text_color(p.text_muted).child(
                            "After you press Go Live, pick a screen or window in the dialog that opens.",
                        )),
                )
                .into_any_element()
        } else if source_cards.is_empty() {
            div()
                .p_4()
                .text_sm()
                .text_color(p.text_muted)
                .child(if gl.windows_tab {
                    "No windows found."
                } else {
                    "No screens found. Screen capture may not be supported here."
                })
                .into_any_element()
        } else {
            div()
                .id("go-live-sources")
                .max_h(px(300.))
                .overflow_y_scroll()
                .p_4()
                .flex()
                .flex_wrap()
                .gap_3()
                .children(source_cards)
                .into_any_element()
        };

        let res_pills = Resolution::ALL.into_iter().map(|r| {
            pill(
                SharedString::from(format!("res-{}", r.label())),
                r.label(),
                r == gl.resolution,
                false,
                p,
            )
            .on_click(cx.listener(move |this, _, _, cx| {
                if let Some(gl) = this.go_live.as_mut() {
                    gl.resolution = r;
                }
                cx.notify();
            }))
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
        let audio_pill = |key: String,
                          label: String,
                          value: Option<glance_media::stream_audio::AudioSource>,
                          selected: bool| {
            pill(SharedString::from(key), label, selected, false, p).on_click(cx.listener(
                move |this, _, _, cx| {
                    if let Some(gl) = this.go_live.as_mut() {
                        gl.audio = value.clone();
                    }
                    cx.notify();
                },
            ))
        };
        let mut audio_pills = vec![audio_pill(
            "aud-none".into(),
            "No audio".into(),
            None,
            gl.audio.is_none(),
        )];
        for (i, src) in gl.audio_sources.iter().enumerate() {
            audio_pills.push(audio_pill(
                format!("aud-{i}"),
                src.label(),
                Some(src.clone()),
                gl.audio.as_ref() == Some(src),
            ));
        }
        let audio_note = match &gl.audio {
            Some(glance_media::stream_audio::AudioSource::System)
                if glance_media::stream_audio::system_includes_call_audio() =>
            {
                Some(
                    "Shares everything your computer plays, including other people's voices from this call.",
                )
            }
            Some(glance_media::stream_audio::AudioSource::System) => {
                Some("Shares everything your computer plays (except glance itself).")
            }
            // Only Linux lists individual apps.
            None if cfg!(target_os = "linux") && gl.audio_sources.len() == 1 => Some(
                "Start the app you want to share so it shows up here, or share the whole system.",
            ),
            _ => None,
        };
        let audio_supported = glance_media::stream_audio::supported();
        let summary = match (&selected_src, out, gl.system_picker) {
            (_, _, true) => format!(
                "Streams at up to {} and {} FPS. Viewers on smaller tiles get it scaled to fit.",
                gl.resolution.label(),
                gl.fps
            ),
            (Some(src), Some((w, h)), _) => format!(
                "{} will stream at {w}×{h}, {} FPS. Viewers on smaller tiles get it scaled to fit.",
                src.name, gl.fps
            ),
            (Some(src), None, _) => format!(
                "{} will stream at up to {}, {} FPS.",
                src.name,
                gl.resolution.label(),
                gl.fps
            ),
            (None, _, _) => "Pick a screen or window.".into(),
        };
        let windows_tab = gl.windows_tab;
        let system_picker = gl.system_picker;
        let error = gl.error.clone();

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
                                .child(if editing { "Stream Settings" } else { "Screen Share" }),
                        )
                        .child(close_button("go-live-close", p).on_click(cx.listener(|this, _, _, cx| {
                            this.go_live = None;
                            cx.notify();
                        }))),
                )
                .when(!system_picker, |d| {
                    d.child(
                        div()
                            .px_4()
                            .flex()
                            .gap_2()
                            .border_b_1()
                            .border_color(p.bg_selected)
                            .child(tab("tab-screens", "Screens", false, !windows_tab))
                            .child(tab("tab-windows", "Applications", true, windows_tab)),
                    )
                })
                .child(picker_body)
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
                        .child(div().text_xs().text_color(p.text_muted).child(summary))
                        .when(audio_supported, |d| {
                            d.child(
                                div()
                                    .flex()
                                    .flex_col()
                                    .gap_2()
                                    .child(div().text_xs().text_color(p.text_muted).child("SOUND"))
                                    .child(div().flex().flex_wrap().gap_1().children(audio_pills)),
                            )
                            .children(audio_note.map(|n| div().text_xs().text_color(p.text_muted).child(n)))
                        })
                        .when(heavy, |d| {
                            d.child(div().text_xs().text_color(p.yellow).child(
                                "No hardware encoder is available, so this quality may drop frames on slower computers.",
                            ))
                        })
                        .children(error.map(|e| div().text_sm().text_color(p.red).child(e))),
                )
                .child(
                    div()
                        .p_4()
                        .flex()
                        .justify_end()
                        .gap_2()
                        .bg(p.bg_sidebar)
                        .when(editing, |d| {
                            d.child(
                                Button::new("go-live-stop")
                                    .danger()
                                    .label("Stop streaming")
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.go_live = None;
                                        this.stop_streaming(cx);
                                    })),
                            )
                            .child(div().flex_1())
                        })
                        .child(Button::new("go-live-cancel").ghost().label("Cancel").on_click(cx.listener(
                            |this, _, _, cx| {
                                this.go_live = None;
                                cx.notify();
                            },
                        )))
                        .child(if editing {
                            Button::new("go-live-apply").primary().label("Apply").on_click(cx.listener(
                                |this, _, _, cx| this.apply_stream_settings(cx),
                            ))
                        } else {
                            Button::new("go-live-start").primary().label("Go Live").on_click(cx.listener(
                                |this, _, _, cx| this.start_streaming(cx),
                            ))
                        }),
                ),
        )
    }

    /// The `--demo` room has no network: show a fake stream entry.
    pub fn start_demo_stream(&mut self, cx: &mut Context<Self>) {
        let Some(gl) = self.go_live.take() else {
            return;
        };
        let me = self.me.id;
        if let Some(room) = self.room.as_mut() {
            room.live_settings = Some(crate::app::LiveSettings {
                selection: glance_media::capture::Selection::SystemPicker,
                pending_selection: None,
                resolution: gl.resolution,
                fps: gl.fps,
                audio: None,
            });
        }
        if let Some(room) = self.room.as_mut()
            && let Some(x) = room.participants.iter_mut().find(|x| x.profile.id == me)
        {
            x.stream = Some(StreamInfo {
                width: 1920,
                height: 1080,
                fps: gl.fps,
                has_audio: false,
                source: "Screen".into(),
            });
        }
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
            ($id:literal, $label:literal, $desc:expr, $field:ident) => {
                setting_row(
                    p,
                    $label,
                    $desc,
                    Switch::new($id)
                        .checked(self.settings.$field)
                        .on_click(cx.listener(|this, v: &bool, window, cx| {
                            this.settings.$field = *v;
                            this.on_settings_changed();
                            this.apply_theme(window, cx);
                            this.apply_tray(window, cx);
                            this.sync_autostart(cx);
                            cx.notify();
                        })),
                )
            };
        }
        let relay_in_use = self
            .net
            .as_ref()
            .map_or_else(|| "Offline".to_string(), |n| n.relay_label.clone());
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
            SettingsPage::Voice => {
                let devices = self.audio_devices.get_or_insert_with(|| {
                    (
                        glance_media::audio::device::input_devices(),
                        glance_media::audio::device::output_devices(),
                    )
                });
                let (inputs, outputs) = devices.clone();
                let cameras = self
                    .camera_list
                    .get_or_insert_with(glance_media::camera::list_cameras)
                    .clone();
                let camera_rows = std::iter::once((None, "First available camera".to_string()))
                    .chain(cameras.into_iter().map(|c| (Some(c.index), c.name)))
                    .enumerate()
                    .map(|(i, (value, name))| {
                        let is_sel = value == self.settings.camera_device;
                        let hover = p.bg_hover;
                        div()
                            .id(SharedString::from(format!("cam-{i}")))
                            .px_3()
                            .py_1()
                            .rounded(px(4.))
                            .cursor_pointer()
                            .truncate()
                            .text_color(if is_sel { p.text_header } else { p.text_normal })
                            .when(is_sel, |d| d.bg(p.bg_selected))
                            .hover(move |s| s.bg(hover))
                            .child(name)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.settings.camera_device = value;
                                this.save_settings();
                                cx.notify();
                            }))
                    })
                    .collect::<Vec<_>>();
                let device_list = |id: &'static str,
                                   label: &'static str,
                                   list: Vec<glance_media::audio::device::DeviceInfo>,
                                   selected: Option<String>,
                                   input: bool| {
                    let rows = std::iter::once((None, "System default".to_string()))
                        .chain(list.into_iter().map(|d| (Some(d.name.clone()), d.name)))
                        .enumerate()
                        .map(|(i, (value, name))| {
                            let is_sel = value == selected;
                            let hover = p.bg_hover;
                            div()
                                .id(SharedString::from(format!("{id}-{i}")))
                                .px_3()
                                .py_1()
                                .rounded(px(4.))
                                .cursor_pointer()
                                .truncate()
                                .text_color(if is_sel { p.text_header } else { p.text_normal })
                                .when(is_sel, |d| d.bg(p.bg_selected))
                                .hover(move |s| s.bg(hover))
                                .child(name)
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    if input {
                                        this.settings.input_device = value.clone();
                                    } else {
                                        this.settings.output_device = value.clone();
                                    }
                                    this.start_voice(cx);
                                    this.save_settings();
                                    cx.notify();
                                }))
                        });
                    div()
                        .py_4()
                        .border_b_1()
                        .border_color(p.bg_selected)
                        .flex()
                        .flex_col()
                        .gap_1()
                        .child(section_label(p, label))
                        .child(
                            div()
                                .id(id)
                                .max_h(px(180.))
                                .overflow_y_scroll()
                                .p_1()
                                .rounded(px(4.))
                                .bg(p.bg_input)
                                .children(rows),
                        )
                };
                let processing = |id: &'static str,
                                  label: &'static str,
                                  desc: &'static str,
                                  on: bool,
                                  which: Processing| {
                    setting_row(
                        p,
                        label,
                        desc,
                        Switch::new(id).checked(on).on_click(cx.listener(
                            move |this, v: &bool, _, cx| {
                                match which {
                                    Processing::EchoCancel => this.settings.echo_cancel = *v,
                                    Processing::NoiseSuppression => {
                                        this.settings.noise_suppression = *v
                                    }
                                    Processing::AutoGain => this.settings.auto_gain = *v,
                                }
                                this.apply_audio_processing();
                                this.save_settings();
                                cx.notify();
                            },
                        )),
                    )
                };
                div()
                    .flex()
                    .flex_col()
                    .child(device_list(
                        "input-devices",
                        "INPUT DEVICE",
                        inputs,
                        self.settings.input_device.clone(),
                        true,
                    ))
                    .child(device_list(
                        "output-devices",
                        "OUTPUT DEVICE",
                        outputs,
                        self.settings.output_device.clone(),
                        false,
                    ))
                    .child(
                        div()
                            .py_4()
                            .border_b_1()
                            .border_color(p.bg_selected)
                            .flex()
                            .gap_6()
                            .child(volume_slider(
                                p,
                                "INPUT VOLUME",
                                &self.input_volume_slider,
                                self.settings.input_volume,
                            ))
                            .child(volume_slider(
                                p,
                                "OUTPUT VOLUME",
                                &self.output_volume_slider,
                                self.settings.output_volume,
                            )),
                    )
                    .child(
                        div()
                            .py_4()
                            .border_b_1()
                            .border_color(p.bg_selected)
                            .flex()
                            .flex_col()
                            .gap_1()
                            .child(section_label(p, "CAMERA"))
                            .child(
                                div()
                                    .id("camera-devices")
                                    .max_h(px(140.))
                                    .overflow_y_scroll()
                                    .p_1()
                                    .rounded(px(4.))
                                    .bg(p.bg_input)
                                    .children(camera_rows),
                            ),
                    )
                    .child(processing(
                        "ns",
                        "Noise suppression",
                        "Filters background noise from your mic.",
                        self.settings.noise_suppression,
                        Processing::NoiseSuppression,
                    ))
                    .child(processing(
                        "aec",
                        "Echo cancellation",
                        "Stops others hearing themselves through your speakers.",
                        self.settings.echo_cancel,
                        Processing::EchoCancel,
                    ))
                    .child(processing(
                        "agc",
                        "Automatic gain control",
                        "Keeps your voice at a steady, clear volume however loud your mic is.",
                        self.settings.auto_gain,
                        Processing::AutoGain,
                    ))
                    .child(toggle!(
                        "sounds",
                        "Sounds",
                        "Play a chime when people join or leave, and when you mute or deafen.",
                        sounds
                    ))
                    .into_any_element()
            }
            SettingsPage::Streaming => div()
                .flex()
                .flex_col()
                .child(info_row(
                    p,
                    "Default quality",
                    format!(
                        "{} · {} FPS (the last quality you went live with)",
                        self.settings.stream_resolution.label(),
                        self.settings.stream_fps
                    )
                    .into(),
                ))
                .child(toggle!(
                    "hw-encode",
                    "Hardware video acceleration",
                    "Use the graphics card to encode your stream and decode the ones you watch when it can. Needed for 4K at 60 FPS; turn it off if video looks wrong. Applies to streams started afterwards.",
                    hardware_encoding
                ))
                .child(info_row(
                    p,
                    "Video encoder",
                    match glance_media::codec::hardware_encoder_known() {
                        _ if !self.settings.hardware_encoding => "Software H.264 (OpenH264)".into(),
                        Some(Some(name)) => format!("{name} (hardware), OpenH264 as fallback").into(),
                        Some(None) => "Software H.264 (OpenH264): no supported hardware encoder found".into(),
                        None => "Checking for a hardware encoder…".into(),
                    },
                ))
                .child(info_row(
                    p,
                    "Video decoder",
                    match glance_media::codec::hardware_decoder_known() {
                        _ if !self.settings.hardware_encoding => "Software H.264 (OpenH264)".into(),
                        Some(Some(name)) => format!("{name} (hardware), OpenH264 as fallback").into(),
                        Some(None) => "Software H.264 (OpenH264): no supported hardware decoder found".into(),
                        None => "Checking for a hardware decoder…".into(),
                    },
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
                .when(crate::tray::autostart_available(), |d| {
                    d.child(toggle!(
                        "autostart",
                        "Start glance on login",
                        if crate::net::in_flatpak() {
                            "Opens quietly in the tray when you sign in. Your desktop may ask to allow it."
                        } else {
                            "Opens quietly in the tray when you sign in."
                        },
                        autostart
                    ))
                })
                .into_any_element(),
            SettingsPage::Files => div()
                .flex()
                .flex_col()
                .child(info_row(p, "Save files to", "Downloads".into()))
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
                .child(info_row(p, "Toggle mute", keys("Shift + M")))
                .child(info_row(p, "Toggle deafen", keys("Shift + D")))
                .child(info_row(p, "Quit", keys("Q")))
                .child(info_row(p, "Close dialog / exit fullscreen", "Esc".into()))
                .into_any_element(),
            SettingsPage::Connection => div()
                .flex()
                .flex_col()
                .child(info_row(p, "Relay in use", relay_in_use.into()))
                .child(
                    div()
                        .py_4()
                        .flex()
                        .flex_col()
                        .gap_2()
                        .child(section_label(p, "CUSTOM RELAY URL"))
                        .child(div().text_xs().text_color(p.text_muted).child(
                            "Calls go directly between computers when possible. When they can't, they pass (still encrypted) through a relay. Run your own with iroh-relay and enter its URL here.",
                        ))
                        .child(
                            div()
                                .flex()
                                .gap_2()
                                .child(div().flex_1().child(Input::new(&self.relay_input)))
                                .child(Button::new("relay-save").primary().label("Save").on_click(
                                    cx.listener(|this, _, _, cx| this.save_relay(cx)),
                                )),
                        )
                        .children(
                            self.relay_note
                                .clone()
                                .map(|n| div().text_xs().text_color(p.text_muted).child(n)),
                        ),
                )
                .into_any_element(),
            SettingsPage::Updates => {
                let auto = self.updates_available().then(|| {
                    toggle!(
                        "auto-update",
                        "Check for updates automatically",
                        "Looks for a new version at startup and every 6 hours and downloads it in the background. glance never restarts on its own.",
                        auto_update
                    )
                });
                div()
                    .flex()
                    .flex_col()
                    .child(info_row(p, "Current version", self.version_label().into()))
                    .children(auto)
                    .child(self.render_update_status(p, cx))
                    .into_any_element()
            }
        };
        div()
            .max_w(px(660.))
            .child(title)
            .child(body)
            .into_any_element()
    }
}

impl GlanceApp {
    /// Apply settings that affect running systems.
    pub fn on_settings_changed(&mut self) {
        self.push_presence();
        self.apply_audio_processing();
        self.save_settings();
    }

    /// "Invite people": copy the link, or ring contacts into the room.
    pub fn render_invite(&mut self, p: &Palette, cx: &mut Context<Self>) -> impl IntoElement {
        let link = self
            .room
            .as_ref()
            .map(|r| r.link.clone())
            .unwrap_or_default();
        let copied = self.link_copied;
        let revealed = self.link_revealed;
        let in_room: Vec<glance_core::PeerId> = self
            .room
            .as_ref()
            .map(|r| r.participants.iter().map(|x| x.profile.id).collect())
            .unwrap_or_default();
        let callable: Vec<glance_core::Contact> = self
            .contacts
            .iter()
            .filter(|c| {
                c.state == glance_core::ContactState::Mutual && !in_room.contains(&c.profile.id)
            })
            .cloned()
            .collect();
        let live = self.room.as_ref().is_some_and(|r| r.handle.is_some());
        modal_backdrop("invite-backdrop").child(
            div()
                .id("invite")
                .occlude()
                .w(px(440.))
                .rounded(px(8.))
                .bg(p.bg_content)
                .p_4()
                .flex()
                .flex_col()
                .gap_3()
                .child(
                    div()
                        .flex()
                        .items_center()
                        .justify_between()
                        .child(
                            div()
                                .text_lg()
                                .font_weight(FontWeight::BOLD)
                                .text_color(p.text_header)
                                .child("Invite people"),
                        )
                        .child(close_button("invite-close", p).on_click(cx.listener(
                            |this, _, _, cx| {
                                this.invite_open = false;
                                cx.notify();
                            },
                        ))),
                )
                .child(section_label(p, "SEND A LINK"))
                .child(
                    div()
                        .id("invite-dialog-link")
                        .pl_3()
                        .pr_1()
                        .py_1()
                        .rounded(px(4.))
                        .bg(p.bg_input)
                        .cursor_pointer()
                        .flex()
                        .items_center()
                        .gap_2()
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.link_revealed = !this.link_revealed;
                            cx.notify();
                        }))
                        .child(icon(
                            if revealed {
                                IconName::EyeOff
                            } else {
                                IconName::Eye
                            },
                            16.,
                            p.interactive,
                        ))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .text_sm()
                                .text_color(if revealed {
                                    p.text_normal
                                } else {
                                    p.text_muted
                                })
                                .truncate()
                                .child(if revealed {
                                    link
                                } else {
                                    format!("{}  (click to show)", crate::room::masked_link())
                                }),
                        )
                        .child(crate::room::copy_button("invite-copy", copied, cx)),
                )
                .when(live, |d| {
                    d.child(section_label(p, "RING A CONTACT")).child(
                        div()
                            .id("invite-contacts")
                            .max_h(px(260.))
                            .overflow_y_scroll()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .when(callable.is_empty(), |d| {
                                d.child(
                                    div()
                                        .text_sm()
                                        .text_color(p.text_muted)
                                        .child("Contacts you add after calls show up here."),
                                )
                            })
                            .children(callable.into_iter().map(|c| {
                                let pid = c.profile.id;
                                let can = c.presence.can_ring();
                                div()
                                    .flex()
                                    .items_center()
                                    .gap_3()
                                    .py_1()
                                    .child(
                                        Avatar::new(&c.profile, 28.)
                                            .status(presence_color(p, c.presence), p.bg_content),
                                    )
                                    .child(
                                        div()
                                            .flex_1()
                                            .text_color(p.text_normal)
                                            .child(c.display_name().to_string()),
                                    )
                                    .child(
                                        Button::new(SharedString::from(format!(
                                            "ring-{}",
                                            pid.short()
                                        )))
                                        .small()
                                        .success()
                                        .disabled(!can)
                                        .label(if can { "Ring" } else { c.presence.label() })
                                        .on_click(
                                            cx.listener(move |this, _, _, cx| {
                                                this.call_contacts(vec![pid], cx);
                                                this.invite_open = false;
                                            }),
                                        ),
                                    )
                            })),
                    )
                }),
        )
    }

    /// "Leaving glance": a web link from chat, before it opens in the browser.
    pub fn render_leaving(&mut self, p: &Palette, cx: &mut Context<Self>) -> impl IntoElement {
        let leaving = self.leaving.as_ref().expect("leaving open");
        let (url, domain, trust) = (leaving.url.clone(), leaving.domain.clone(), leaving.trust);
        modal_backdrop("leaving-backdrop")
            .on_click(cx.listener(|this, _, _, cx| {
                this.leaving = None;
                cx.notify();
            }))
            .child(
                div()
                    .id("leaving")
                    .occlude()
                    .w(px(440.))
                    .rounded(px(8.))
                    .bg(p.bg_content)
                    .overflow_hidden()
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .p_4()
                            .flex()
                            .flex_col()
                            .gap_3()
                            .child(
                                div()
                                    .text_xl()
                                    .font_weight(FontWeight::BOLD)
                                    .text_color(p.text_header)
                                    .child("Leaving glance"),
                            )
                            .child(
                                div()
                                    .text_color(p.text_normal)
                                    .child("This link is taking you to the following website"),
                            )
                            .child(
                                div()
                                    .p_2()
                                    .rounded(px(4.))
                                    .bg(p.bg_input)
                                    .text_sm()
                                    .text_color(p.text_header)
                                    .child(url),
                            )
                            .child(
                                gpui_kit::component::checkbox::Checkbox::new("leaving-trust")
                                    .checked(trust)
                                    .label(format!("Trust {domain} links from now on"))
                                    .on_click(cx.listener(|this, checked: &bool, _, cx| {
                                        if let Some(l) = this.leaving.as_mut() {
                                            l.trust = *checked;
                                        }
                                        cx.notify();
                                    })),
                            ),
                    )
                    .child(
                        div()
                            .p_4()
                            .bg(p.bg_sidebar)
                            .flex()
                            .justify_end()
                            .gap_2()
                            .child(
                                Button::new("leaving-back")
                                    .ghost()
                                    .label("Go Back")
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.leaving = None;
                                        cx.notify();
                                    })),
                            )
                            .child(
                                Button::new("leaving-visit")
                                    .primary()
                                    .label("Visit Site")
                                    .on_click(cx.listener(|this, _, _, cx| this.visit_leaving(cx))),
                            ),
                    ),
            )
    }

    /// "Join another room?": an invite from chat while in a different room.
    pub fn render_join_confirm(&mut self, p: &Palette, cx: &mut Context<Self>) -> impl IntoElement {
        let current = self.room.as_ref().map_or("this room", |r| r.name.as_str());
        let body = format!("You'll leave {current} to join this room.");
        modal_backdrop("join-confirm-backdrop")
            .on_click(cx.listener(|this, _, _, cx| {
                this.join_confirm = None;
                cx.notify();
            }))
            .child(
                div()
                    .id("join-confirm")
                    .occlude()
                    .w(px(440.))
                    .rounded(px(8.))
                    .bg(p.bg_content)
                    .overflow_hidden()
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .p_4()
                            .flex()
                            .flex_col()
                            .gap_3()
                            .child(
                                div()
                                    .text_xl()
                                    .font_weight(FontWeight::BOLD)
                                    .text_color(p.text_header)
                                    .child("Join another room?"),
                            )
                            .child(div().text_color(p.text_normal).child(body)),
                    )
                    .child(
                        div()
                            .p_4()
                            .bg(p.bg_sidebar)
                            .flex()
                            .justify_end()
                            .gap_2()
                            .child(
                                Button::new("join-confirm-cancel")
                                    .ghost()
                                    .label("Cancel")
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.join_confirm = None;
                                        cx.notify();
                                    })),
                            )
                            .child(
                                Button::new("join-confirm-join")
                                    .primary()
                                    .label("Join")
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        if let Some(link) = this.join_confirm.take() {
                                            this.join_link(&link, cx);
                                        }
                                        cx.notify();
                                    })),
                            ),
                    ),
            )
    }

    /// "Visit Site": remember the domain if asked, then open the link.
    fn visit_leaving(&mut self, cx: &mut Context<Self>) {
        let Some(leaving) = self.leaving.take() else {
            return;
        };
        // Only ever hand http(s) to the browser.
        if crate::markdown_view::web_domain(&leaving.url).as_ref() != Some(&leaving.domain) {
            return;
        }
        if leaving.trust && !self.settings.trusted_domains.contains(&leaving.domain) {
            self.settings.trusted_domains.push(leaving.domain);
            self.settings.trusted_domains.sort();
            self.save_settings();
        }
        cx.open_url(&leaving.url);
        cx.notify();
    }

    /// Someone is calling: Join / Decline.
    pub fn render_incoming_call(
        &mut self,
        p: &Palette,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let call = self.incoming_call.as_ref().expect("incoming call");
        let caller = self.profile_of(call.from).unwrap_or(glance_core::Profile {
            id: call.from,
            name: "A contact".into(),
            color: 0x80848e,
        });
        let others: Vec<String> = call
            .members
            .iter()
            .filter(|m| **m != caller.name)
            .cloned()
            .collect();
        let subtitle = if others.is_empty() {
            "is calling you".to_string()
        } else {
            format!("is calling you · with {}", others.join(", "))
        };
        modal_backdrop("call-backdrop").child(
            div()
                .id("incoming-call")
                .occlude()
                .w(px(300.))
                .rounded(px(12.))
                .bg(p.bg_rail)
                .p_6()
                .flex()
                .flex_col()
                .items_center()
                .gap_3()
                .child(Avatar::new(&caller, 80.).ring(Some(p.green)))
                .child(
                    div()
                        .text_xl()
                        .font_weight(FontWeight::BOLD)
                        .text_color(p.text_header)
                        .child(caller.name.clone()),
                )
                .child(
                    div()
                        .text_sm()
                        .text_color(p.text_muted)
                        .text_center()
                        .child(subtitle),
                )
                .child(
                    div()
                        .pt_2()
                        .flex()
                        .gap_6()
                        .child(
                            round_button("decline-call", IconName::PhoneOff, p.red, "Decline")
                                .on_click(
                                    cx.listener(|this, _, _, cx| this.answer_call(false, cx)),
                                ),
                        )
                        .child(
                            round_button("accept-call", IconName::Phone, p.green, "Join")
                                .on_click(cx.listener(|this, _, _, cx| this.answer_call(true, cx))),
                        ),
                ),
        )
    }

    /// Status picker shown above the user panel.
    pub fn render_presence_menu(
        &mut self,
        p: &Palette,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        use glance_core::Presence;
        let item = |id: &'static str, presence: Presence, desc: &'static str| {
            let hover = p.bg_hover;
            div()
                .id(id)
                .px_3()
                .py_2()
                .rounded(px(4.))
                .cursor_pointer()
                .hover(move |s| s.bg(hover))
                .flex()
                .items_center()
                .gap_3()
                .child(
                    div()
                        .size(px(10.))
                        .rounded_full()
                        .bg(crate::ui::presence_color(p, presence)),
                )
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .child(
                            div()
                                .text_sm()
                                .text_color(p.text_header)
                                .child(presence.label()),
                        )
                        .child(div().text_xs().text_color(p.text_muted).child(desc)),
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.presence_menu = false;
                    this.set_presence(presence, cx);
                }))
        };
        div()
            .id("presence-backdrop")
            .absolute()
            .inset_0()
            .on_click(cx.listener(|this, _, _, cx| {
                this.presence_menu = false;
                cx.notify();
            }))
            .child(
                div()
                    .id("presence-menu")
                    .occlude()
                    .absolute()
                    .left(px(80.))
                    .bottom(px(60.))
                    .w(px(240.))
                    .p_2()
                    .rounded(px(8.))
                    .bg(p.bg_rail)
                    .flex()
                    .flex_col()
                    .child(item("st-online", Presence::Online, "Contacts can call you"))
                    .child(item(
                        "st-idle",
                        Presence::Idle,
                        "Away, but calls still ring",
                    ))
                    .child(item(
                        "st-dnd",
                        Presence::DoNotDisturb,
                        "Calls are silent and become missed calls",
                    ))
                    .child(item(
                        "st-invisible",
                        Presence::Invisible,
                        "Appear offline to contacts",
                    )),
            )
    }
}

fn round_button(id: &'static str, glyph: IconName, bg: Hsla, tip: &'static str) -> Stateful<Div> {
    div()
        .id(id)
        .size(px(56.))
        .rounded_full()
        .bg(bg)
        .cursor_pointer()
        .hover(|s| s.opacity(0.85))
        .flex()
        .items_center()
        .justify_center()
        .tooltip(move |window, cx| {
            gpui_kit::component::tooltip::Tooltip::new(tip).build(window, cx)
        })
        .child(icon(glyph, 26., gpui_kit::white()))
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

/// Which voice processing switch was flipped.
#[derive(Clone, Copy)]
enum Processing {
    NoiseSuppression,
    EchoCancel,
    AutoGain,
}

/// A labelled 0–200% volume slider.
fn volume_slider(
    p: &Palette,
    label: &'static str,
    state: &Entity<SliderState>,
    volume: f32,
) -> impl IntoElement {
    div()
        .flex_1()
        .flex()
        .flex_col()
        .gap_2()
        .child(
            div()
                .flex()
                .justify_between()
                .child(section_label(p, label))
                .child(
                    div()
                        .text_xs()
                        .text_color(p.text_muted)
                        .child(format!("{:.0}%", volume * 100.0)),
                ),
        )
        .child(Slider::new(state).accessibility_label(label))
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

/// A shortcut label with the platform's modifier (⌘ on macOS).
fn keys(rest: &str) -> SharedString {
    if cfg!(target_os = "macos") {
        format!("⌘ + {rest}").into()
    } else {
        format!("Ctrl + {rest}").into()
    }
}

fn info_row(p: &Palette, label: &'static str, value: SharedString) -> impl IntoElement {
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
