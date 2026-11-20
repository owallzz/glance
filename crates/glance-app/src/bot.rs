//! `glance bot <link> [name]`: a headless peer for testing. It joins the
//! room, greets people, echoes chat back, and toggles mute every few seconds
//! so state changes are visible. Ctrl-C to quit.

use std::time::Duration;

use anyhow::{Context, Result};
use glance_net::{Command, RoomEvent};
use glance_proto::{MemberState, Ticket, WireProfile};

use crate::net::Net;

/// `stream`: share a test pattern of this size and frame rate.
pub fn run(link: &str, name: &str, stream: Option<(u32, u32, u32)>) -> Result<()> {
    let ticket = Ticket::parse(link).context("parse room link")?;
    // A fresh identity each run, so a bot never collides with your app.
    let net = Net::start_ephemeral()?;
    let profile = WireProfile {
        name: name.to_string(),
        color: 0x80848e,
    };
    let live = stream.map(|(w, h, fps)| glance_proto::WireStream {
        width: w,
        height: h,
        fps,
        has_audio: false,
        source: "Test pattern".into(),
    });
    let initial = MemberState {
        stream: live.clone(),
        ..Default::default()
    };
    let room = net.block_on(net.join_room(ticket, profile, initial))?;
    println!("bot joined as {name}; me = {}", room.me.fmt_short());
    // Share a moving test pattern through the real encoder and network path.
    let broadcast = stream.map(|(w, h, fps)| {
        let commander = room.commander();
        let (control, _ended) = glance_media::pipeline::start_broadcast(
            glance_media::capture::test_pattern(w, h, fps),
            glance_media::pipeline::BroadcastConfig {
                max_height: None,
                fps,
            },
            None,
            move |pkt| {
                let _ = commander.send(Command::Video(std::sync::Arc::new(
                    glance_proto::VideoPacket {
                        ts_us: pkt.ts_us,
                        keyframe: pkt.keyframe,
                        width: pkt.width,
                        height: pkt.height,
                        data: pkt.data,
                    },
                )));
            },
        );
        control
    });
    let mut names = std::collections::HashMap::new();
    let mut muted = false;
    let tick = ticker(Duration::from_secs(5));
    loop {
        flume::Selector::new()
            .recv(&room.events, |ev| {
                let Ok(ev) = ev else { std::process::exit(0) };
                match &ev {
                    RoomEvent::MemberJoined { id, profile, .. } => {
                        names.insert(*id, profile.name.clone());
                        room.send(Command::Chat(format!("hi {}!", profile.name)));
                    }
                    RoomEvent::Chat { from, text, .. } if !text.starts_with("echo:") => {
                        let who = names.get(from).cloned().unwrap_or_default();
                        room.send(Command::Chat(format!("echo: {text} (from {who})")));
                    }
                    RoomEvent::Closed => std::process::exit(0),
                    RoomEvent::Watchers(n) => {
                        if let Some(b) = &broadcast {
                            b.watchers.store(*n, std::sync::atomic::Ordering::Relaxed);
                        }
                    }
                    RoomEvent::KeyframeNeeded => {
                        if let Some(b) = &broadcast {
                            b.keyframe.store(true, std::sync::atomic::Ordering::Relaxed);
                        }
                    }
                    _ => {}
                }
                println!("{ev:?}");
            })
            .recv(&tick, |_| {
                muted = !muted;
                room.send(Command::SetState(MemberState {
                    muted,
                    stream: live.clone(),
                    ..Default::default()
                }));
            })
            .wait();
    }
}

fn ticker(period: Duration) -> flume::Receiver<()> {
    let (tx, rx) = flume::bounded(1);
    std::thread::spawn(move || {
        while tx.send(()).is_ok() {
            std::thread::sleep(period);
        }
    });
    rx
}
