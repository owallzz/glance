//! Wayland capture: the xdg-desktop-portal ScreenCast dialog picks the
//! screen or window, then frames arrive over a PipeWire stream.

use std::cell::RefCell;
use std::os::fd::OwnedFd;
use std::rc::Rc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use ashpd::desktop::PersistMode;
use ashpd::desktop::screencast::{CursorMode, Screencast, SelectSourcesOptions, SourceType};
use pipewire as pw;
use pw::spa;
use pw::spa::param::video::{VideoFormat, VideoInfoRaw};
use pw::spa::pod::Pod;

use super::{Sink, pack_bgra};

pub fn is_wayland_session() -> bool {
    std::env::var_os("WAYLAND_DISPLAY").is_some()
        || std::env::var("XDG_SESSION_TYPE").is_ok_and(|t| t == "wayland")
}

/// Run the portal dialog and the PipeWire loop on a dedicated thread.
pub fn start(sink: Sink, fps: u32) -> Result<()> {
    std::thread::Builder::new()
        .name("capture-wayland".into())
        .spawn(move || {
            if let Err(e) = run(&sink, fps) {
                sink.end(format!("{e:#}"));
            } else if !sink.stopped() {
                sink.end("sharing ended");
            }
        })?;
    Ok(())
}

fn run(sink: &Sink, fps: u32) -> Result<()> {
    // Testing aid: GLANCE_PW_NODE=<id> skips the portal dialog and reads an
    // existing PipeWire video node from the default PipeWire daemon.
    if let Some(node) = std::env::var("GLANCE_PW_NODE")
        .ok()
        .and_then(|v| v.parse().ok())
    {
        return run_pipewire(sink, node, None, fps);
    }
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    // Keep the proxy and session alive for the whole capture; dropping the
    // session tells the compositor sharing stopped.
    let (_proxy, _session, node_id, fd) = rt.block_on(open_portal())?;
    if sink.stopped() {
        return Ok(());
    }
    run_pipewire(sink, node_id, Some(fd), fps)
}

async fn open_portal() -> Result<(
    Screencast,
    ashpd::desktop::Session<Screencast>,
    u32,
    OwnedFd,
)> {
    let proxy = Screencast::new()
        .await
        .context("screen sharing portal unavailable")?;
    let session = proxy.create_session(Default::default()).await?;
    proxy
        .select_sources(
            &session,
            SelectSourcesOptions::default()
                .set_cursor_mode(CursorMode::EMBEDDED)
                .set_sources(SourceType::MONITOR | SourceType::WINDOW)
                .set_multiple(false)
                .set_persist_mode(PersistMode::DoNot),
        )
        .await?;
    let response = proxy
        .start(&session, None, Default::default())
        .await?
        .response()
        .map_err(|e| anyhow!("screen sharing was cancelled ({e})"))?;
    let stream = response
        .streams()
        .first()
        .ok_or_else(|| anyhow!("no screen or window was chosen"))?;
    let node_id = stream.pipe_wire_node_id();
    let fd = proxy
        .open_pipe_wire_remote(&session, Default::default())
        .await?;
    Ok((proxy, session, node_id, fd))
}

struct State {
    format: VideoInfoRaw,
    last: Option<Instant>,
}

fn run_pipewire(sink: &Sink, node_id: u32, fd: Option<OwnedFd>, fps: u32) -> Result<()> {
    pw::init();
    let mainloop = pw::main_loop::MainLoopRc::new(None)?;
    let context = pw::context::ContextRc::new(&mainloop, None)?;
    let core = match fd {
        Some(fd) => context.connect_fd_rc(fd, None)?,
        None => context.connect_rc(None)?,
    };
    let stream = pw::stream::StreamBox::new(
        &core,
        "glance-screen",
        pw::properties::properties! {
            *pw::keys::MEDIA_TYPE => "Video",
            *pw::keys::MEDIA_CATEGORY => "Capture",
            *pw::keys::MEDIA_ROLE => "Screen",
        },
    )?;

    let min_interval = Duration::from_secs_f64(1.0 / fps.max(1) as f64) * 9 / 10;
    let frames = sink.frames.clone();
    let ended = Rc::new(RefCell::new(None::<String>));
    let ended_cb = ended.clone();
    let quit = mainloop.clone();
    let _listener = stream
        .add_local_listener_with_user_data(State {
            format: Default::default(),
            last: None,
        })
        .state_changed(move |_, _, _old, new| {
            if let pw::stream::StreamState::Error(e) = new {
                *ended_cb.borrow_mut() = Some(format!("screen stream failed: {e}"));
                quit.quit();
            }
        })
        .param_changed(|_, state, id, param| {
            let Some(param) = param else { return };
            if id != pw::spa::param::ParamType::Format.as_raw() {
                return;
            }
            if let Ok((media_type, media_subtype)) =
                pw::spa::param::format_utils::parse_format(param)
                && media_type == pw::spa::param::format::MediaType::Video
                && media_subtype == pw::spa::param::format::MediaSubtype::Raw
            {
                let _ = state.format.parse(param);
            }
        })
        .process(move |stream, state| {
            let Some(mut buffer) = stream.dequeue_buffer() else {
                return;
            };
            let now = Instant::now();
            if state.last.is_some_and(|t| now - t < min_interval) {
                return;
            }
            let order = match state.format.format() {
                VideoFormat::BGRx | VideoFormat::BGRA => [0, 1, 2],
                VideoFormat::RGBx | VideoFormat::RGBA => [2, 1, 0],
                _ => return,
            };
            let size = state.format.size();
            let datas = buffer.datas_mut();
            let Some(data) = datas.first_mut() else {
                return;
            };
            let chunk = data.chunk();
            let (offset, stride) = (chunk.offset() as usize, chunk.stride().max(0) as usize);
            let Some(bytes) = data.data() else { return };
            let Some(bytes) = bytes.get(offset..) else {
                return;
            };
            let stride = if stride == 0 {
                size.width as usize * 4
            } else {
                stride
            };
            if let Some(frame) = pack_bgra(
                bytes,
                size.width as usize,
                size.height as usize,
                stride,
                order,
            ) {
                state.last = Some(now);
                let _ = frames.try_send(frame);
            }
        })
        .register()?;

    let format = format_pod()?;
    let mut params = [Pod::from_bytes(&format).ok_or_else(|| anyhow!("bad format pod"))?];
    stream.connect(
        spa::utils::Direction::Input,
        Some(node_id),
        pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS,
        &mut params,
    )?;

    // Poll the stop flag so the UI can end sharing.
    let stop = sink.stop.clone();
    let quit = mainloop.clone();
    let timer = mainloop.loop_().add_timer(move |_| {
        if stop.load(std::sync::atomic::Ordering::Relaxed) {
            quit.quit();
        }
    });
    timer
        .update_timer(
            Some(Duration::from_millis(100)),
            Some(Duration::from_millis(100)),
        )
        .into_result()?;

    mainloop.run();
    let _ = stream.disconnect();
    if let Some(reason) = ended.borrow_mut().take() {
        sink.end(reason);
    }
    Ok(())
}

/// Accept 32-bit RGB formats at any size and frame rate (no DMA-BUF
/// modifiers, so the compositor hands us mappable memory).
fn format_pod() -> Result<Vec<u8>> {
    let obj = pw::spa::pod::object!(
        pw::spa::utils::SpaTypes::ObjectParamFormat,
        pw::spa::param::ParamType::EnumFormat,
        pw::spa::pod::property!(
            pw::spa::param::format::FormatProperties::MediaType,
            Id,
            pw::spa::param::format::MediaType::Video
        ),
        pw::spa::pod::property!(
            pw::spa::param::format::FormatProperties::MediaSubtype,
            Id,
            pw::spa::param::format::MediaSubtype::Raw
        ),
        pw::spa::pod::property!(
            pw::spa::param::format::FormatProperties::VideoFormat,
            Choice,
            Enum,
            Id,
            VideoFormat::BGRx,
            VideoFormat::BGRx,
            VideoFormat::BGRA,
            VideoFormat::RGBx,
            VideoFormat::RGBA,
        ),
        pw::spa::pod::property!(
            pw::spa::param::format::FormatProperties::VideoSize,
            Choice,
            Range,
            Rectangle,
            pw::spa::utils::Rectangle {
                width: 1920,
                height: 1080
            },
            pw::spa::utils::Rectangle {
                width: 1,
                height: 1
            },
            pw::spa::utils::Rectangle {
                width: 8192,
                height: 8192
            }
        ),
        pw::spa::pod::property!(
            pw::spa::param::format::FormatProperties::VideoFramerate,
            Choice,
            Range,
            Fraction,
            pw::spa::utils::Fraction { num: 60, denom: 1 },
            pw::spa::utils::Fraction { num: 0, denom: 1 },
            pw::spa::utils::Fraction {
                num: 1000,
                denom: 1
            }
        ),
    );
    Ok(pw::spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &pw::spa::pod::Value::Object(obj),
    )
    .map_err(|e| anyhow!("serialize format: {e:?}"))?
    .0
    .into_inner())
}
