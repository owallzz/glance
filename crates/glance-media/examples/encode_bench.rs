//! Encode speed: `cargo run --release -p glance-media --example
//! encode_bench -- [WxH] [frames] [--sw|--hw] [--pattern]`. Encodes a scrolling, busy pattern (like
//! scrolling a web page) and prints the average time per frame.
use glance_media::VideoFrame;
use glance_media::codec::{EncoderChoice, VideoEncoder, bitrate_for};

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (w, h) = args
        .first()
        .and_then(|s| s.split_once('x'))
        .and_then(|(a, b)| Some((a.parse().ok()?, b.parse().ok()?)))
        .unwrap_or((2560u32, 1440u32));
    let frames: u32 = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(120);
    let choice = if args.iter().any(|a| a == "--sw") {
        EncoderChoice::Software
    } else if args.iter().any(|a| a == "--hw") {
        EncoderChoice::Hardware
    } else {
        EncoderChoice::Auto
    };
    let mut enc = VideoEncoder::with_choice(w, h, 60, bitrate_for(w, h, 60), choice)?;
    println!("encoder: {}", enc.backend_name());
    // A text-like texture that scrolls 8 px per frame.
    let tex: Vec<u8> = (0..(w * (h + 1024)) as usize)
        .map(|i| {
            let (x, y) = (i % w as usize, i / w as usize);
            if (x / 7 + y / 13) % 5 == 0 || (x * 31 + y * 17) % 97 < 9 {
                30
            } else {
                230
            }
        })
        .collect();
    let mut total = std::time::Duration::ZERO;
    let mut bytes = 0usize;
    let mut packets = Vec::new();
    let use_pattern = args.iter().any(|a| a == "--pattern");
    let mut pattern = glance_media::pattern::Pattern::new(w, h, 60);
    for n in 0..frames {
        let frame = if use_pattern {
            pattern.next_frame()
        } else {
            let off = ((n * 8) % 1024) as usize * w as usize;
            let mut bgra = Vec::with_capacity((w * h * 4) as usize);
            for &v in &tex[off..off + (w * h) as usize] {
                bgra.extend_from_slice(&[v, v, v, 255]);
            }
            VideoFrame {
                width: w,
                height: h,
                bgra,
            }
        };
        let t = std::time::Instant::now();
        if let Some(p) = enc.encode(&frame)? {
            bytes += p.data.len();
            packets.push(p.data);
        }
        total += t.elapsed();
    }
    let ms = total.as_secs_f64() * 1000.0 / frames as f64;
    println!("  (BGRA->I420 alone: {:.1} ms)", conversion_ms(w, h));
    // Viewer side: decode and scale to a 1080p-sized tile, in software and
    // with the GPU decoder (if there is one).
    let mut dec = glance_media::codec::VideoDecoder::new()?;
    let t = std::time::Instant::now();
    for p in &packets {
        let _ = dec.decode(p, (1920, 1080))?;
    }
    let scaled = t.elapsed();
    let mut hw = glance_media::codec::VideoDecoder::auto()?;
    if hw.is_hardware() {
        let n = packets.len().max(1) as f64;
        let t = std::time::Instant::now();
        for p in &packets {
            let _ = hw.decode(p, (1920, 1080))?;
        }
        let tile = t.elapsed().as_secs_f64() * 1000.0 / n;
        let t = std::time::Instant::now();
        for p in &packets {
            let _ = hw.decode(p, (0, 0))?;
        }
        let full = t.elapsed().as_secs_f64() * 1000.0 / n;
        println!(
            "  {}: decode+scale to 1080p tile {tile:.1} ms/frame, full size {full:.1} ms/frame",
            hw.backend_name()
        );
    }
    let mut raw = openh264::decoder::Decoder::new()?;
    let (mut t_dec, mut t_rgb) = (std::time::Duration::ZERO, std::time::Duration::ZERO);
    let mut rgba = Vec::new();
    for p in &packets {
        let t0 = std::time::Instant::now();
        let yuv = raw.decode(p)?;
        t_dec += t0.elapsed();
        if let Some(yuv) = yuv {
            use openh264::formats::YUVSource;
            let (w, h) = yuv.dimensions();
            rgba.resize(w * h * 4, 0);
            let t1 = std::time::Instant::now();
            yuv.write_rgba8(&mut rgba);
            t_rgb += t1.elapsed();
        }
    }
    let n = packets.len().max(1) as f64;
    println!(
        "  of which decode {:.1} ms, YUV->RGBA {:.1} ms",
        t_dec.as_secs_f64() * 1000.0 / n,
        t_rgb.as_secs_f64() * 1000.0 / n
    );
    println!(
        "  decode+scale to 1080p tile: {:.1} ms/frame",
        scaled.as_secs_f64() * 1000.0 / packets.len().max(1) as f64
    );
    println!(
        "{w}x{h}: {ms:.1} ms/frame ({:.0} fps max), {:.1} Mbps at 60 fps",
        1000.0 / ms,
        bytes as f64 * 8.0 / frames as f64 * 60.0 / 1e6
    );
    Ok(())
}

#[allow(dead_code)]
fn conversion_ms(w: u32, h: u32) -> f64 {
    use openh264::formats::{BgraSliceU8, YUVBuffer};
    let bgra = vec![128u8; (w * h * 4) as usize];
    let mut yuv = YUVBuffer::new(w as usize, h as usize);
    let t = std::time::Instant::now();
    for _ in 0..20 {
        yuv.read_bgra8(BgraSliceU8::new(&bgra, (w as usize, h as usize)));
    }
    t.elapsed().as_secs_f64() * 1000.0 / 20.0
}
