//! WASAPI loopback of the default output device (via cpal).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

use super::{AudioSource, push_mono};
use crate::audio::resample::Resampler;

pub fn capture(
    _source: AudioSource,
    mut tx: rtrb::Producer<f32>,
    stop: Arc<AtomicBool>,
) -> Result<()> {
    let (ready_tx, ready_rx) = flume::bounded::<Result<()>>(1);
    std::thread::Builder::new()
        .name("stream-audio-wasapi".into())
        .spawn(move || {
            let result: Result<cpal::Stream> = (|| {
                let host = cpal::default_host();
                let device = host
                    .default_output_device()
                    .ok_or_else(|| anyhow!("no output device"))?;
                let supported = device.default_output_config().context("output config")?;
                let config: cpal::StreamConfig = supported.into();
                let channels = config.channels as usize;
                let mut resampler = Resampler::new(config.sample_rate, 48_000);
                let mut mono = Vec::new();
                let mut out = Vec::new();
                // cpal captures an output device in loopback when you build an
                // input stream on it (WASAPI).
                let stream = device.build_input_stream::<f32, _, _>(
                    config,
                    move |data, _| {
                        mono.clear();
                        out.clear();
                        crate::audio::resample::downmix(data, channels, &mut mono);
                        resampler.process(&mono, &mut out);
                        push_mono(&mut tx, &out, 1);
                    },
                    |e| tracing::warn!("loopback error: {e}"),
                    None,
                )?;
                stream.play()?;
                Ok(stream)
            })();
            match result {
                Ok(stream) => {
                    let _ = ready_tx.send(Ok(()));
                    while !stop.load(Ordering::Relaxed) {
                        std::thread::sleep(Duration::from_millis(100));
                    }
                    drop(stream);
                }
                Err(e) => {
                    let _ = ready_tx.send(Err(e));
                }
            }
        })?;
    ready_rx
        .recv_timeout(Duration::from_secs(5))
        .map_err(|_| anyhow!("audio capture didn't start"))?
}
