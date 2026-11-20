//! "Entire system" audio on Windows: WASAPI process loopback of everything
//! except glance's own process tree (Windows 10 2004+), so viewers don't
//! hear the call echoed back; on older systems, loopback of the default
//! output device via cpal (which includes the call).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

use super::{AudioSource, push_mono};
use crate::audio::resample::Resampler;

/// Set once a capture had to fall back to device loopback (Windows older
/// than 10 2004), which includes glance's own audio.
static FELL_BACK: AtomicBool = AtomicBool::new(false);

/// Whether "Entire system" can leave glance's own audio out here. Assumed
/// until a capture shows otherwise.
pub fn process_loopback_supported() -> bool {
    !FELL_BACK.load(Ordering::Relaxed)
}

pub fn capture(source: AudioSource, tx: rtrb::Producer<f32>, stop: Arc<AtomicBool>) -> Result<()> {
    // The capture thread reports once: started, or failed (handing the ring
    // back so device loopback can use it instead).
    let (started_tx, started_rx) = flume::bounded::<process_loopback::Started>(1);
    let stop2 = stop.clone();
    std::thread::Builder::new()
        .name("stream-audio-process".into())
        .spawn(move || process_loopback::run(tx, &stop2, &started_tx))?;
    match started_rx.recv_timeout(Duration::from_secs(5)) {
        Ok(process_loopback::Started::Yes) => Ok(()),
        Ok(process_loopback::Started::No(e, tx)) => {
            tracing::info!("process loopback unavailable ({e:#}); using device loopback");
            FELL_BACK.store(true, Ordering::Relaxed);
            device_loopback(source, tx, stop)
        }
        Err(_) => Err(anyhow!("audio capture didn't start")),
    }
}

fn device_loopback(
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

mod process_loopback {
    use std::sync::atomic::{AtomicBool, Ordering};

    use ::windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
    use ::windows::Win32::Media::Audio::*;
    use ::windows::Win32::System::Com::StructuredStorage::{
        PROPVARIANT, PROPVARIANT_0, PROPVARIANT_0_0, PROPVARIANT_0_0_0,
    };
    use ::windows::Win32::System::Com::{
        BLOB, COINIT_MULTITHREADED, CoInitializeEx, CoUninitialize,
    };
    use ::windows::Win32::System::Threading::{
        CreateEventW, GetCurrentProcessId, WaitForSingleObject,
    };
    use ::windows::Win32::System::Variant::VT_BLOB;
    use ::windows::core::{IUnknown, Interface, Ref, implement};
    use anyhow::{Result, anyhow};

    use super::super::push_mono;

    #[implement(IActivateAudioInterfaceCompletionHandler)]
    struct Activated(std::sync::mpsc::SyncSender<()>);

    impl IActivateAudioInterfaceCompletionHandler_Impl for Activated_Impl {
        fn ActivateCompleted(
            &self,
            _op: Ref<IActivateAudioInterfaceAsyncOperation>,
        ) -> ::windows::core::Result<()> {
            let _ = self.0.try_send(());
            Ok(())
        }
    }

    struct Event(HANDLE);
    impl Drop for Event {
        fn drop(&mut self) {
            // SAFETY: closing the event we created, once.
            unsafe {
                let _ = CloseHandle(self.0);
            }
        }
    }

    struct Com;
    impl Drop for Com {
        fn drop(&mut self) {
            // SAFETY: balances the CoInitializeEx on this thread.
            unsafe { CoUninitialize() }
        }
    }

    pub enum Started {
        Yes,
        /// Not available; the ring buffer comes back for a fallback.
        No(anyhow::Error, rtrb::Producer<f32>),
    }

    /// Capture until `stop`, reporting once on `started`.
    pub fn run(mut tx: rtrb::Producer<f32>, stop: &AtomicBool, started: &flume::Sender<Started>) {
        // SAFETY: COM on this thread for the lifetime of `_com`.
        let _com = match unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.ok() {
            Ok(()) => Com,
            Err(e) => {
                let _ = started.send(Started::No(e.into(), tx));
                return;
            }
        };
        let (client, capture, event) = match start() {
            Ok(v) => v,
            Err(e) => {
                let _ = started.send(Started::No(e, tx));
                return;
            }
        };
        let _ = started.send(Started::Yes);
        let mut samples = Vec::new();
        while !stop.load(Ordering::Relaxed) {
            // SAFETY: waiting on our event; buffers are read between
            // GetBuffer and ReleaseBuffer within the reported frame count.
            unsafe {
                if WaitForSingleObject(event.0, 100) != WAIT_OBJECT_0 {
                    continue;
                }
                loop {
                    let Ok(n) = capture.GetNextPacketSize() else {
                        break;
                    };
                    if n == 0 {
                        break;
                    }
                    let (mut data, mut frames, mut flags) = (std::ptr::null_mut(), 0u32, 0u32);
                    if capture
                        .GetBuffer(&mut data, &mut frames, &mut flags, None, None)
                        .is_err()
                    {
                        break;
                    }
                    samples.clear();
                    if flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0 || data.is_null() {
                        samples.resize(frames as usize * 2, 0.0);
                    } else {
                        samples.extend_from_slice(std::slice::from_raw_parts(
                            data.cast::<f32>(),
                            frames as usize * 2,
                        ));
                    }
                    let _ = capture.ReleaseBuffer(frames);
                    push_mono(&mut tx, &samples, 2);
                }
            }
        }
        // SAFETY: stopping the client we started.
        unsafe {
            let _ = client.Stop();
        }
    }

    fn start() -> Result<(IAudioClient, IAudioCaptureClient, Event)> {
        // SAFETY: COM calls with parameters that outlive them; the
        // activation params blob points at a local that lives until the
        // activation has completed.
        unsafe {
            let mut params = AUDIOCLIENT_ACTIVATION_PARAMS {
                ActivationType: AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK,
                Anonymous: AUDIOCLIENT_ACTIVATION_PARAMS_0 {
                    ProcessLoopbackParams: AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS {
                        TargetProcessId: GetCurrentProcessId(),
                        ProcessLoopbackMode: PROCESS_LOOPBACK_MODE_EXCLUDE_TARGET_PROCESS_TREE,
                    },
                },
            };
            let prop = PROPVARIANT {
                Anonymous: PROPVARIANT_0 {
                    Anonymous: std::mem::ManuallyDrop::new(PROPVARIANT_0_0 {
                        vt: VT_BLOB,
                        wReserved1: 0,
                        wReserved2: 0,
                        wReserved3: 0,
                        Anonymous: PROPVARIANT_0_0_0 {
                            blob: BLOB {
                                cbSize: size_of::<AUDIOCLIENT_ACTIVATION_PARAMS>() as u32,
                                pBlobData: (&mut params as *mut AUDIOCLIENT_ACTIVATION_PARAMS)
                                    .cast(),
                            },
                        },
                    }),
                },
            };
            let (done_tx, done_rx) = std::sync::mpsc::sync_channel(1);
            let handler: IActivateAudioInterfaceCompletionHandler = Activated(done_tx).into();
            let op = ActivateAudioInterfaceAsync(
                VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK,
                &IAudioClient::IID,
                Some(&prop),
                &handler,
            )?;
            done_rx
                .recv_timeout(std::time::Duration::from_secs(5))
                .map_err(|_| anyhow!("process loopback activation timed out"))?;
            let mut hr = ::windows::core::HRESULT(0);
            let mut unknown: Option<IUnknown> = None;
            op.GetActivateResult(&mut hr, &mut unknown)?;
            hr.ok()?;
            let client: IAudioClient = unknown.ok_or_else(|| anyhow!("no audio client"))?.cast()?;
            // Process loopback has no mix format; ask for 48 kHz stereo float
            // and let Windows convert.
            let format = WAVEFORMATEX {
                wFormatTag: 3, // WAVE_FORMAT_IEEE_FLOAT
                nChannels: 2,
                nSamplesPerSec: 48_000,
                nAvgBytesPerSec: 48_000 * 8,
                nBlockAlign: 8,
                wBitsPerSample: 32,
                cbSize: 0,
            };
            client.Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                AUDCLNT_STREAMFLAGS_LOOPBACK
                    | AUDCLNT_STREAMFLAGS_EVENTCALLBACK
                    | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM
                    | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY,
                200_000, // 20 ms, in 100 ns units
                0,
                &format,
                None,
            )?;
            let event = Event(CreateEventW(None, false, false, None)?);
            client.SetEventHandle(event.0)?;
            let capture: IAudioCaptureClient = client.GetService()?;
            client.Start()?;
            Ok((client, capture, event))
        }
    }
}
