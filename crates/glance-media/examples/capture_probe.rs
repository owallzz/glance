//! Capture for a few seconds and report frame size, rate and encode cost.
//!
//!     cargo run --release -p glance-media --example capture_probe [seconds] [max_height]
//!
//! On Wayland this opens the system share dialog (or set GLANCE_PW_NODE to
//! read an existing PipeWire node). Elsewhere it captures the first screen.

use std::time::{Duration, Instant};

use glance_media::capture::{self, Selection, SourceKind};
use glance_media::codec::{VideoEncoder, bitrate_for};
use glance_media::frame::FrameScaler;
use glance_media::pipeline::output_size;

fn main() -> anyhow::Result<()> {
    let secs: u64 = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(5);
    let max_h: Option<u32> = std::env::args().nth(2).and_then(|s| s.parse().ok());
    let selection = if capture::uses_system_picker() {
        Selection::SystemPicker
    } else {
        let src = capture::list_sources()
            .into_iter()
            .find(|s| s.kind == SourceKind::Screen)
            .ok_or_else(|| anyhow::anyhow!("no screens"))?;
        println!("capturing {}", src.name);
        Selection::Source(src)
    };
    let cap = capture::start(selection, 60)?;
    let mut frames = 0u32;
    let mut first = None;
    let mut size = (0, 0);
    let mut encoder: Option<VideoEncoder> = None;
    let mut scaler = FrameScaler::default();
    let mut encode_time = Duration::ZERO;
    let mut bytes = 0usize;
    let deadline = Instant::now() + Duration::from_secs(secs + 30);
    while Instant::now() < deadline {
        if let Ok(reason) = cap.ended.try_recv() {
            println!("ended: {reason}");
            break;
        }
        let Ok(f) = cap.frames.recv_timeout(Duration::from_millis(500)) else {
            continue;
        };
        let start = *first.get_or_insert_with(Instant::now);
        if start.elapsed() > Duration::from_secs(secs) {
            break;
        }
        frames += 1;
        size = (f.width, f.height);
        let out = output_size(size, max_h);
        let enc = match &mut encoder {
            Some(e) if e.size() == out => e,
            _ => encoder.insert(VideoEncoder::new(
                out.0,
                out.1,
                60,
                bitrate_for(out.0, out.1, 60),
            )?),
        };
        let t = Instant::now();
        let input = if out == size {
            f
        } else {
            scaler.resize_ref(&f, out).unwrap()
        };
        if let Some(p) = enc.encode(&input)? {
            bytes += p.data.len();
        }
        encode_time += t.elapsed();
    }
    cap.stop();
    let elapsed = first
        .map(|t| t.elapsed().as_secs_f64())
        .unwrap_or(0.0)
        .max(0.001);
    println!(
        "frames={frames} size={}x{} capture_fps={:.1} avg_scale+encode_ms={:.1} bitrate_mbps={:.2}",
        size.0,
        size.1,
        frames as f64 / elapsed,
        encode_time.as_secs_f64() * 1000.0 / frames.max(1) as f64,
        bytes as f64 * 8.0 / elapsed / 1e6
    );
    Ok(())
}
