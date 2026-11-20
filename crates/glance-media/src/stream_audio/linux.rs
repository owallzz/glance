//! PipeWire capture: an application's output stream, or the default sink's
//! monitor ("entire system").

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Result, anyhow};
use pipewire as pw;
use pw::spa;
use pw::spa::pod::Pod;

use super::{AudioSource, push_mono};

/// Applications currently playing audio (PipeWire "Stream/Output/Audio"
/// nodes), excluding glance itself.
pub fn list_apps() -> Vec<super::AudioSource> {
    let result: Result<Vec<AudioSource>> = (|| {
        pw::init();
        let mainloop = pw::main_loop::MainLoopRc::new(None)?;
        let context = pw::context::ContextRc::new(&mainloop, None)?;
        let core = context.connect_rc(None)?;
        let registry = core.get_registry_rc()?;
        let found: Rc<RefCell<Vec<AudioSource>>> = Rc::default();
        let found_cb = found.clone();
        let _listener = registry
            .add_listener_local()
            .global(move |global| {
                let Some(props) = global.props else { return };
                if props.get(*pw::keys::MEDIA_CLASS) != Some("Stream/Output/Audio") {
                    return;
                }
                let name = props
                    .get(*pw::keys::APP_NAME)
                    .or_else(|| props.get(*pw::keys::NODE_NAME))
                    .unwrap_or("Application")
                    .to_string();
                if is_system_stream(&name) {
                    return;
                }
                let Some(serial) = props
                    .get(*pw::keys::OBJECT_SERIAL)
                    .and_then(|s| s.parse().ok())
                else {
                    return;
                };
                let mut f = found_cb.borrow_mut();
                if !f
                    .iter()
                    .any(|s| matches!(s, AudioSource::App { name: n, .. } if *n == name))
                {
                    f.push(AudioSource::App { id: serial, name });
                }
            })
            .register();
        // Let the registry report existing objects, then stop.
        let quit = mainloop.clone();
        let timer = mainloop.loop_().add_timer(move |_| quit.quit());
        timer
            .update_timer(Some(Duration::from_millis(300)), None)
            .into_result()?;
        mainloop.run();
        let list = found.borrow().clone();
        Ok(list)
    })();
    result.unwrap_or_default()
}

/// glance itself and desktop-shell sound effects aren't worth offering.
fn is_system_stream(name: &str) -> bool {
    const SKIP: [&str; 8] = [
        "glance",
        "mutter",
        "gnome-shell",
        "gnome shell",
        "plasmashell",
        "kwin",
        "speech-dispatcher",
        "libcanberra",
    ];
    let lower = name.to_lowercase();
    SKIP.iter().any(|s| lower.contains(s))
}

pub fn capture(source: AudioSource, tx: rtrb::Producer<f32>, stop: Arc<AtomicBool>) -> Result<()> {
    let (ready_tx, ready_rx) = flume::bounded::<Result<()>>(1);
    std::thread::Builder::new()
        .name("stream-audio-pw".into())
        .spawn(move || {
            if let Err(e) = run(source, tx, stop, &ready_tx) {
                let _ = ready_tx.try_send(Err(e));
            }
        })?;
    ready_rx
        .recv_timeout(Duration::from_secs(5))
        .map_err(|_| anyhow!("audio capture didn't start"))?
}

fn run(
    source: AudioSource,
    tx: rtrb::Producer<f32>,
    stop: Arc<AtomicBool>,
    ready: &flume::Sender<Result<()>>,
) -> Result<()> {
    pw::init();
    let mainloop = pw::main_loop::MainLoopRc::new(None)?;
    let context = pw::context::ContextRc::new(&mainloop, None)?;
    let core = context.connect_rc(None)?;
    let mut props = pw::properties::properties! {
        *pw::keys::MEDIA_TYPE => "Audio",
        *pw::keys::MEDIA_CATEGORY => "Capture",
        *pw::keys::MEDIA_ROLE => "Music",
        *pw::keys::NODE_NAME => "glance-stream-audio",
    };
    match &source {
        // The default sink's monitor: everything that plays.
        AudioSource::System => props.insert(*pw::keys::STREAM_CAPTURE_SINK, "true"),
        // Link straight to that app's output.
        AudioSource::App { id, .. } => props.insert(*pw::keys::TARGET_OBJECT, id.to_string()),
    }
    let stream = pw::stream::StreamBox::new(&core, "glance-stream-audio", props)?;
    let channels = Rc::new(RefCell::new(2usize));
    let ch_cb = channels.clone();
    let tx = RefCell::new(tx);
    let _listener = stream
        .add_local_listener_with_user_data(())
        .param_changed(move |_, _, id, param| {
            let Some(param) = param else { return };
            if id != pw::spa::param::ParamType::Format.as_raw() {
                return;
            }
            let mut info = spa::param::audio::AudioInfoRaw::default();
            if info.parse(param).is_ok() {
                *ch_cb.borrow_mut() = info.channels().max(1) as usize;
            }
        })
        .process(move |stream, _| {
            let Some(mut buffer) = stream.dequeue_buffer() else {
                return;
            };
            let datas = buffer.datas_mut();
            let Some(data) = datas.first_mut() else {
                return;
            };
            let size = data.chunk().size() as usize;
            let Some(bytes) = data.data() else { return };
            let bytes = &bytes[..size.min(bytes.len())];
            let samples: Vec<f32> = bytes
                .as_chunks::<4>()
                .0
                .iter()
                .map(|b| f32::from_le_bytes(*b))
                .collect();
            push_mono(&mut tx.borrow_mut(), &samples, *channels.borrow());
        })
        .register()?;

    let mut info = spa::param::audio::AudioInfoRaw::new();
    info.set_format(spa::param::audio::AudioFormat::F32LE);
    info.set_rate(48_000);
    info.set_channels(2);
    let obj = spa::pod::Object {
        type_: spa::utils::SpaTypes::ObjectParamFormat.as_raw(),
        id: spa::param::ParamType::EnumFormat.as_raw(),
        properties: info.into(),
    };
    let values = spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &spa::pod::Value::Object(obj),
    )
    .map_err(|e| anyhow!("audio format: {e:?}"))?
    .0
    .into_inner();
    let mut params = [Pod::from_bytes(&values).ok_or_else(|| anyhow!("bad audio format"))?];
    stream.connect(
        spa::utils::Direction::Input,
        None,
        pw::stream::StreamFlags::AUTOCONNECT
            | pw::stream::StreamFlags::MAP_BUFFERS
            | pw::stream::StreamFlags::RT_PROCESS,
        &mut params,
    )?;
    let _ = ready.send(Ok(()));
    let quit = mainloop.clone();
    let timer = mainloop.loop_().add_timer(move |_| {
        if stop.load(Ordering::Relaxed) {
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
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn shell_streams_are_skipped() {
        assert!(super::is_system_stream("Mutter"));
        assert!(super::is_system_stream("glance-voice"));
        assert!(!super::is_system_stream("Firefox"));
    }
}
