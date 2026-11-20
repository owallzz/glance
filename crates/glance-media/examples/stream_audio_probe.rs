//! Lists shareable audio sources and captures one for a few seconds.
//! `cargo run -p glance-media --example stream_audio_probe -- [name-substring]`
//! (no argument captures the whole system).

use glance_media::stream_audio::{self, AudioSource};

fn main() -> anyhow::Result<()> {
    let sources = stream_audio::list_sources();
    for s in &sources {
        println!("source: {s:?}");
    }
    let pick = std::env::args().nth(1);
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
