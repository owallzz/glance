//! `glance bot <link> [name]`: a headless peer for testing. It joins the
//! room, greets people, echoes chat back, and toggles mute every few seconds
//! so state changes are visible. Ctrl-C to quit.

use std::time::Duration;

use anyhow::{Context, Result};
use glance_net::{Command, RoomEvent};
use glance_proto::{MemberState, Ticket, WireProfile};

use crate::net::Net;

/// `stream`: share a test pattern of this size and frame rate.
/// `tone`: speak a sine tone so others see/hear us talking.
/// `file`: offer this file to the room after joining.
pub fn run(
    link: &str,
    name: &str,
    stream: Option<(u32, u32, u32)>,
    tone: bool,
    file: Option<std::path::PathBuf>,
    ring_back: bool,
) -> Result<()> {
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
    let room = net.block_on(net.join_room(ticket, profile.clone(), initial))?;
    // Contacts: accept every request; answer rings by joining that room
    // (this process relaunches itself into the new room).
    let contacts = net.contacts();
    contacts.send(glance_net::contacts::ContactCommand::SetProfile(
        profile.clone(),
    ));
    let relations = std::cell::RefCell::new(std::collections::HashMap::new());
    let rang = std::cell::Cell::new(false);
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
    // Voice: a tone (or silence) in, mixed output discarded; we log who we
    // hear speaking so tests can check audio flows both ways.
    let commander = room.commander();
    let engine = glance_media::audio::AudioEngine::start(
        glance_media::audio::EngineConfig {
            input: if tone {
                glance_media::audio::InputKind::Tone(330.0)
            } else {
                glance_media::audio::InputKind::Silence
            },
            output: glance_media::audio::OutputKind::Null,
            echo_cancel: false,
            noise_suppression: false,
        },
        move |pkt| {
            let wire = glance_proto::AudioDatagram {
                seq: pkt.seq,
                level: pkt.level,
                data: pkt.data,
            };
            if let Ok(f) = glance_proto::encode(&wire) {
                let _ = commander.send(Command::Audio(bytes::Bytes::from(f[4..].to_vec())));
            }
        },
    )?;
    let (atx, arx) = flume::bounded::<(glance_net::EndpointId, glance_proto::AudioDatagram)>(512);
    room.send(Command::SetAudioSink(Some(atx)));
    let into_engine = engine.incoming();
    std::thread::spawn(move || {
        while let Ok((from, d)) = arx.recv() {
            let _ = into_engine.send((
                *from.as_bytes(),
                glance_media::audio::EncodedAudio {
                    seq: d.seq,
                    level: d.level,
                    data: d.data,
                },
            ));
        }
    });
    if let Some(path) = file {
        // Wait until we're connected to someone, or the offer reaches no one.
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        let mut early = Vec::new();
        while std::time::Instant::now() < deadline {
            match room.events.recv_timeout(Duration::from_millis(200)) {
                Ok(ev) => {
                    let joined = matches!(ev, RoomEvent::MemberJoined { .. });
                    early.push(ev);
                    if joined {
                        break;
                    }
                }
                Err(_) => continue,
            }
        }
        for ev in early {
            println!("{ev:?}");
        }
        let (hash, size) = net.block_on(net.add_file(path.clone()))?;
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let mime = mime_guess::from_path(&path)
            .first_or_octet_stream()
            .to_string();
        room.send(Command::OfferFile(glance_proto::WireFile {
            hash,
            name,
            size,
            mime,
        }));
        println!("offered {} ({size} bytes)", path.display());
    }
    // Bots fetch every file offered to them (into a temp dir) and say so.
    let dl_dir = std::env::temp_dir().join(format!("glance-bot-{}", std::process::id()));
    let mut heard: Vec<[u8; 32]> = Vec::new();
    let names = std::cell::RefCell::new(std::collections::HashMap::new());
    let mut muted = false;
    let tick = ticker(Duration::from_secs(if tone { 1 } else { 5 }));
    loop {
        flume::Selector::new()
            .recv(&room.events, |ev| {
                let Ok(ev) = ev else { std::process::exit(0) };
                match &ev {
                    RoomEvent::MemberJoined { id, profile, .. } => {
                        names.borrow_mut().insert(*id, profile.name.clone());
                        room.send(Command::Chat(format!("hi {}!", profile.name)));
                    }
                    RoomEvent::Chat { from, text, .. } if !text.starts_with("echo:") => {
                        let who = names.borrow().get(from).cloned().unwrap_or_default();
                        room.send(Command::Chat(format!("echo: {text} (from {who})")));
                    }
                    RoomEvent::Closed => std::process::exit(0),
                    RoomEvent::FileOffered { from, file, .. } => {
                        let (tx, rx) = flume::unbounded();
                        let dest = glance_net::files::unique_path(&dl_dir, &file.name);
                        let _ = std::fs::create_dir_all(&dl_dir);
                        net.download_file(file.hash, vec![*from], dest.clone(), tx);
                        let name = file.name.clone();
                        let cmd = room.commander();
                        let hash = file.hash;
                        std::thread::spawn(move || {
                            while let Ok(p) = rx.recv() {
                                match p {
                                    glance_net::files::Progress::Done => {
                                        let len =
                                            std::fs::metadata(&dest).map(|m| m.len()).unwrap_or(0);
                                        println!("downloaded {name} ({len} bytes)");
                                        let _ = cmd.send(Command::HaveFile(hash));
                                        let _ = cmd.send(Command::Chat(format!(
                                            "got {name} ({len} bytes)"
                                        )));
                                        break;
                                    }
                                    glance_net::files::Progress::Failed(e) => {
                                        println!("download of {name} failed: {e}");
                                        break;
                                    }
                                    _ => {}
                                }
                            }
                        });
                    }
                    RoomEvent::Watchers(n) => {
                        if let Some(b) = &broadcast {
                            b.watchers.store(*n, std::sync::atomic::Ordering::Relaxed);
                        }
                    }
                    RoomEvent::Congested => {
                        if let Some(b) = &broadcast {
                            b.congestion
                                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
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
            .recv(&contacts.events, |ev| {
                use glance_net::contacts::{ContactCommand, ContactEvent, Relation};
                let Ok(ev) = ev else { return };
                match &ev {
                    ContactEvent::RequestReceived { id, profile } => {
                        relations.borrow_mut().insert(*id, Relation::Mutual);
                        contacts.send(ContactCommand::SetRelations(relations.borrow().clone()));
                        contacts.send(ContactCommand::Accept(*id));
                        println!("accepted contact {}", profile.name);
                    }
                    ContactEvent::Ring {
                        from,
                        call_id,
                        link,
                        ..
                    } if !ring_back => {
                        println!("ringing: call {call_id}, joining {link}");
                        contacts.send(ContactCommand::Answer {
                            to: *from,
                            call_id: *call_id,
                            accepted: true,
                        });
                        std::thread::sleep(Duration::from_millis(300));
                        if let Ok(exe) = std::env::current_exe() {
                            let mut args: Vec<String> = std::env::args().skip(1).collect();
                            if args.len() > 1 {
                                args[1] = link.clone();
                            }
                            let _ = std::process::Command::new(exe).args(args).spawn();
                        }
                        std::process::exit(0);
                    }
                    // --ring-back: once a new contact leaves the call, ring
                    // them back into ours (tests the incoming-call UI).
                    ContactEvent::Presence { id, info }
                        if ring_back
                            && !rang.get()
                            && info.in_call.is_none()
                            && relations.borrow().contains_key(id) =>
                    {
                        rang.set(true);
                        println!("ringing back");
                        contacts.send(ContactCommand::Ring {
                            to: *id,
                            call_id: 42,
                            link: room.link(),
                            members: vec![name.to_string()],
                        });
                    }
                    ContactEvent::RingAnswered { accepted, .. } => {
                        println!("ring answered: {accepted}")
                    }
                    other => println!("contacts: {other:?}"),
                }
            })
            .recv(&tick, |_| {
                let now = engine.speaking_peers();
                if now != heard {
                    let who: Vec<String> = now
                        .iter()
                        .map(|k| {
                            names
                                .borrow()
                                .iter()
                                .find(|(id, _): &(&glance_net::EndpointId, &String)| {
                                    id.as_bytes() == k
                                })
                                .map(|(_, n)| n.clone())
                                .unwrap_or_else(|| "?".into())
                        })
                        .collect();
                    println!("speaking: {who:?}");
                    heard = now;
                }
                // Toggling mute would cut the tone; only bots without a tone flap.
                if tone {
                    return;
                }
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
