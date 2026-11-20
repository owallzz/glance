//! Lists shareable audio sources and captures one for a few seconds.
//! `cargo run -p glance-media --example stream_audio_probe -- [name-substring]`
//! (no argument captures the whole system). `--self-tone` (Linux) also
//! plays a tone from this process into a sink named `glance-test-sink`:
//! "entire system" must not pick it up (max_level stays 0).

use glance_media::stream_audio::{self, AudioSource};

fn main() -> anyhow::Result<()> {
    let sources = stream_audio::list_sources();
    for s in &sources {
        println!("source: {s:?}");
    }
    #[cfg(target_os = "linux")]
    if std::env::args().any(|a| a == "--self-tone") {
        std::thread::spawn(self_tone);
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
    let pick = std::env::args().nth(1).filter(|a| !a.starts_with("--"));
    let source = match pick {
        Some(name) => sources
            .into_iter()
            .find(|s| matches!(s, AudioSource::App { name: n, .. } if n.contains(&name)))
            .ok_or_else(|| anyhow::anyhow!("no app matching {name}"))?,
        None => AudioSource::System,
    };
    println!("capturing {}", source.label());
    let stats = std::sync::Arc::new(std::sync::Mutex::new((0u32, 0u8, 0usize)));
    let s2 = stats.clone();
    let cap = stream_audio::start(source, move |pkt| {
        let mut s = s2.lock().unwrap();
        s.0 += 1;
        s.1 = s.1.max(pkt.level);
        s.2 += pkt.data.len();
    })?;
    std::thread::sleep(std::time::Duration::from_secs(3));
    drop(cap);
    let (frames, max_level, bytes) = *stats.lock().unwrap();
    println!(
        "frames={frames} max_level={max_level} kbps={:.0}",
        bytes as f64 * 8.0 / 3.0 / 1000.0
    );
    Ok(())
}

/// Play a quiet 440 Hz tone from this process into `glance-test-sink`.
#[cfg(target_os = "linux")]
fn self_tone() {
    use pipewire as pw;
    use pw::spa;
    let run = || -> anyhow::Result<()> {
        pw::init();
        let mainloop = pw::main_loop::MainLoopRc::new(None)?;
        let context = pw::context::ContextRc::new(&mainloop, None)?;
        let core = context.connect_rc(None)?;
        let stream = pw::stream::StreamBox::new(
            &core,
            "probe-self-tone",
            pw::properties::properties! {
                *pw::keys::MEDIA_TYPE => "Audio",
                *pw::keys::MEDIA_CATEGORY => "Playback",
                *pw::keys::TARGET_OBJECT => "glance-test-sink",
                *pw::keys::APP_NAME => "ProbeSelf",
            },
        )?;
        let _l = stream
            .add_local_listener_with_user_data(0f32)
            .process(|stream, phase| {
                let Some(mut buffer) = stream.dequeue_buffer() else {
                    return;
                };
                let data = &mut buffer.datas_mut()[0];
                let mut n = 0;
                if let Some(bytes) = data.data() {
                    for frame in bytes.as_chunks_mut::<8>().0 {
                        *phase = (*phase + 440.0 / 48_000.0).fract();
                        let v = (*phase * std::f32::consts::TAU).sin() * 0.3;
                        frame[..4].copy_from_slice(&v.to_le_bytes());
                        frame[4..].copy_from_slice(&v.to_le_bytes());
                        n += 1;
                    }
                }
                let chunk = data.chunk_mut();
                *chunk.offset_mut() = 0;
                *chunk.stride_mut() = 8;
                *chunk.size_mut() = n * 8;
            })
            .register()?;
        let mut info = spa::param::audio::AudioInfoRaw::new();
        info.set_format(spa::param::audio::AudioFormat::F32LE);
        info.set_rate(48_000);
        info.set_channels(2);
        let values = spa::pod::serialize::PodSerializer::serialize(
            std::io::Cursor::new(Vec::new()),
            &spa::pod::Value::Object(spa::pod::Object {
                type_: spa::utils::SpaTypes::ObjectParamFormat.as_raw(),
                id: spa::param::ParamType::EnumFormat.as_raw(),
                properties: info.into(),
            }),
        )
        .map_err(|e| anyhow::anyhow!("{e:?}"))?
        .0
        .into_inner();
        let mut params = [spa::pod::Pod::from_bytes(&values).expect("pod")];
        stream.connect(
            spa::utils::Direction::Output,
            None,
            pw::stream::StreamFlags::AUTOCONNECT
                | pw::stream::StreamFlags::MAP_BUFFERS
                | pw::stream::StreamFlags::RT_PROCESS,
            &mut params,
        )?;
        mainloop.run();
        Ok(())
    };
    if let Err(e) = run() {
        eprintln!("self tone: {e:#}");
    }
}
