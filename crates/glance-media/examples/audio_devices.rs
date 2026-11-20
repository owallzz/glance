//! Prints the audio devices glance offers in settings (and, with `--raw`,
//! everything the audio host reports).
use cpal::traits::{DeviceTrait, HostTrait};

fn main() {
    let host = cpal::default_host();
    println!("host: {:?}", host.id());
    if std::env::args().any(|a| a == "--raw") {
        for d in host.devices().into_iter().flatten() {
            if let Ok(desc) = d.description() {
                println!(
                    "raw: {:40} dir={:?} driver={:?} addr={:?}",
                    desc.name(),
                    desc.direction(),
                    desc.driver(),
                    desc.address()
                );
            }
        }
    }
    for d in glance_media::audio::device::input_devices() {
        println!(
            "in : {}{}",
            d.name,
            if d.is_default { " (default)" } else { "" }
        );
    }
    for d in glance_media::audio::device::output_devices() {
        println!(
            "out: {}{}",
            d.name,
            if d.is_default { " (default)" } else { "" }
        );
    }
}
