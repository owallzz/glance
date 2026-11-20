//! Audio devices via cpal, converted to/from 48 kHz mono through lock-free
//! ring buffers so the audio callbacks never block.

use anyhow::{Context, Result, anyhow};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{SampleFormat, Stream, StreamConfig};

use super::SAMPLE_RATE;
use super::resample::{Resampler, downmix};

/// A device name for settings pickers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeviceInfo {
    pub name: String,
    pub is_default: bool,
}

fn device_name(d: &cpal::Device) -> String {
    d.description()
        .map(|desc| desc.name().to_string())
        .unwrap_or_else(|_| "Unknown device".into())
}

pub fn input_devices() -> Vec<DeviceInfo> {
    let host = cpal::default_host();
    let default = host.default_input_device().map(|d| device_name(&d));
    host.input_devices()
        .map(|it| {
            it.map(|d| {
                let name = device_name(&d);
                DeviceInfo {
                    is_default: Some(&name) == default.as_ref(),
                    name,
                }
            })
            .collect()
        })
        .unwrap_or_default()
}

pub fn output_devices() -> Vec<DeviceInfo> {
    let host = cpal::default_host();
    let default = host.default_output_device().map(|d| device_name(&d));
    host.output_devices()
        .map(|it| {
            it.map(|d| {
                let name = device_name(&d);
                DeviceInfo {
                    is_default: Some(&name) == default.as_ref(),
                    name,
                }
            })
            .collect()
        })
        .unwrap_or_default()
}

fn find_input(name: Option<&str>) -> Result<cpal::Device> {
    let host = cpal::default_host();
    if let Some(name) = name
        && let Ok(mut it) = host.input_devices()
        && let Some(d) = it.find(|d| device_name(d) == name)
    {
        return Ok(d);
    }
    host.default_input_device()
        .ok_or_else(|| anyhow!("no microphone found"))
}

fn find_output(name: Option<&str>) -> Result<cpal::Device> {
    let host = cpal::default_host();
    if let Some(name) = name
        && let Ok(mut it) = host.output_devices()
        && let Some(d) = it.find(|d| device_name(d) == name)
    {
        return Ok(d);
    }
    host.default_output_device()
        .ok_or_else(|| anyhow!("no speakers or headphones found"))
}

/// Open the microphone; 48 kHz mono samples are pushed into `tx`.
pub fn open_input(name: Option<&str>, mut tx: rtrb::Producer<f32>) -> Result<Stream> {
    let device = find_input(name)?;
    let supported = device.default_input_config().context("microphone config")?;
    let format = supported.sample_format();
    let config: StreamConfig = supported.into();
    let channels = config.channels as usize;
    let mut resampler = Resampler::new(config.sample_rate, SAMPLE_RATE);
    let mut mono = Vec::with_capacity(4096);
    let mut resampled = Vec::with_capacity(4096);
    let mut push = move |interleaved: &[f32]| {
        mono.clear();
        resampled.clear();
        downmix(interleaved, channels, &mut mono);
        resampler.process(&mono, &mut resampled);
        for &s in &resampled {
            if tx.push(s).is_err() {
                break; // engine is behind; drop
            }
        }
    };
    let err = |e| tracing::warn!("microphone error: {e}");
    let stream = match format {
        SampleFormat::F32 => {
            device.build_input_stream::<f32, _, _>(config, move |d, _| push(d), err, None)?
        }
        SampleFormat::I16 => {
            let mut buf = Vec::new();
            device.build_input_stream::<i16, _, _>(
                config,
                move |d, _| {
                    buf.clear();
                    buf.extend(d.iter().map(|&s| s as f32 / 32768.0));
                    push(&buf);
                },
                err,
                None,
            )?
        }
        SampleFormat::U16 => {
            let mut buf = Vec::new();
            device.build_input_stream::<u16, _, _>(
                config,
                move |d, _| {
                    buf.clear();
                    buf.extend(d.iter().map(|&s| (s as f32 - 32768.0) / 32768.0));
                    push(&buf);
                },
                err,
                None,
            )?
        }
        other => return Err(anyhow!("unsupported microphone sample format {other:?}")),
    };
    stream.play().context("start microphone")?;
    Ok(stream)
}

/// Open the speakers; 48 kHz mono samples are pulled from `rx` (silence on
/// underrun).
pub fn open_output(name: Option<&str>, mut rx: rtrb::Consumer<f32>) -> Result<Stream> {
    let device = find_output(name)?;
    let supported = device.default_output_config().context("speaker config")?;
    let format = supported.sample_format();
    let config: StreamConfig = supported.into();
    let channels = config.channels as usize;
    let mut resampler = Resampler::new(SAMPLE_RATE, config.sample_rate);
    let mut src = Vec::with_capacity(4096);
    let mut converted: Vec<f32> = Vec::with_capacity(8192);
    let mut fill = move |out_frames: usize, write: &mut dyn FnMut(usize, f32)| {
        // Pull enough 48 kHz input to produce `out_frames` device frames.
        while converted.len() < out_frames {
            src.clear();
            let want = ((out_frames - converted.len()) as f64 * SAMPLE_RATE as f64
                / config.sample_rate as f64)
                .ceil() as usize
                + 2;
            for _ in 0..want {
                src.push(rx.pop().unwrap_or(0.0));
            }
            resampler.process(&src, &mut converted);
        }
        for (i, s) in converted.drain(..out_frames).enumerate() {
            for c in 0..channels {
                write(i * channels + c, s);
            }
        }
    };
    let err = |e| tracing::warn!("speaker error: {e}");
    let stream = match format {
        SampleFormat::F32 => device.build_output_stream::<f32, _, _>(
            config,
            move |d: &mut [f32], _| fill(d.len() / channels, &mut |i, s| d[i] = s),
            err,
            None,
        )?,
        SampleFormat::I16 => device.build_output_stream::<i16, _, _>(
            config,
            move |d: &mut [i16], _| {
                fill(d.len() / channels, &mut |i, s| {
                    d[i] = (s.clamp(-1.0, 1.0) * 32767.0) as i16
                })
            },
            err,
            None,
        )?,
        SampleFormat::U16 => device.build_output_stream::<u16, _, _>(
            config,
            move |d: &mut [u16], _| {
                fill(d.len() / channels, &mut |i, s| {
                    d[i] = ((s.clamp(-1.0, 1.0) + 1.0) * 32767.5) as u16
                })
            },
            err,
            None,
        )?,
        other => return Err(anyhow!("unsupported speaker sample format {other:?}")),
    };
    stream.play().context("start speakers")?;
    Ok(stream)
}
