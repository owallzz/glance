//! macOS: ScreenCaptureKit's audio of the whole display, excluding glance's
//! own process (so viewers never hear the call played back to them).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Result, anyhow};
use screencapturekit::cm_sample_buffer::CMSampleBuffer;
use screencapturekit::sc_content_filter::{InitParams, SCContentFilter};
use screencapturekit::sc_error_handler::StreamErrorHandler;
use screencapturekit::sc_output_handler::{SCStreamOutputType, StreamOutput};
use screencapturekit::sc_shareable_content::SCShareableContent;
use screencapturekit::sc_stream::SCStream;
use screencapturekit::sc_stream_configuration::SCStreamConfiguration;

use super::AudioSource;

struct Errors;

impl StreamErrorHandler for Errors {
    fn on_error(&self) {
        tracing::warn!("screen audio capture stopped with an error");
    }
}

/// Receives audio buffers on ScreenCaptureKit's queue.
struct AudioOut {
    tx: Mutex<rtrb::Producer<f32>>,
    stop: Arc<AtomicBool>,
}

impl StreamOutput for AudioOut {
    fn did_output_sample_buffer(&self, sample: CMSampleBuffer, kind: SCStreamOutputType) {
        if !matches!(kind, SCStreamOutputType::Audio) || self.stop.load(Ordering::Relaxed) {
            return;
        }
        // A panic here would cross into Objective-C and abort the app.
        let buffers = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            sample.sys_ref.get_av_audio_buffer_list()
        }));
        let Ok(buffers) = buffers else { return };
        // Float32, one buffer per channel (non-interleaved): average them.
        let planes: Vec<Vec<f32>> = buffers
            .iter()
            .map(|b| {
                b.data
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .map(|c| f32::from_ne_bytes(*c))
                    .collect()
            })
            .collect();
        let Some(len) = planes.iter().map(|p| p.len()).min() else {
            return;
        };
        let inv = 1.0 / planes.len().max(1) as f32;
        let Ok(mut tx) = self.tx.lock() else { return };
        for i in 0..len {
            let s: f32 = planes.iter().map(|p| p[i]).sum::<f32>() * inv;
            if tx.push(s).is_err() {
                break;
            }
        }
    }
}

/// Video frames we don't use (the stream needs a screen output).
struct Ignore;

impl StreamOutput for Ignore {
    fn did_output_sample_buffer(&self, _: CMSampleBuffer, _: SCStreamOutputType) {}
}

pub fn capture(_source: AudioSource, tx: rtrb::Producer<f32>, stop: Arc<AtomicBool>) -> Result<()> {
    let (ready_tx, ready_rx) = flume::bounded::<Result<()>>(1);
    std::thread::Builder::new()
        .name("stream-audio-sck".into())
        .spawn(move || {
            let mut content = SCShareableContent::current();
            let Some(display) = content.displays.pop() else {
                let _ = ready_tx.send(Err(anyhow!("no display to capture audio from")));
                return;
            };
            let filter = SCContentFilter::new(InitParams::Display(display));
            let config = SCStreamConfiguration {
                // Audio is what we want; keep the (ignored) video tiny.
                width: 2,
                height: 2,
                captures_audio: true,
                sample_rate: 48_000,
                channel_count: 2,
                excludes_current_process_audio: true,
                ..Default::default()
            };
            let mut stream = SCStream::new(filter, config, Errors);
            stream.add_output(Ignore, SCStreamOutputType::Screen);
            stream.add_output(
                AudioOut {
                    tx: Mutex::new(tx),
                    stop: stop.clone(),
                },
                SCStreamOutputType::Audio,
            );
            if let Err(e) = stream.start_capture() {
                let _ = ready_tx.send(Err(anyhow!("screen audio capture failed: {e}")));
                return;
            }
            let _ = ready_tx.send(Ok(()));
            while !stop.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(100));
            }
            let _ = stream.stop_capture();
        })?;
    ready_rx
        .recv_timeout(Duration::from_secs(5))
        .map_err(|_| anyhow!("audio capture didn't start"))?
}
