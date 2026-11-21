//! Three in-process peers on localhost (no relays) form a room mesh.

use std::collections::HashSet;
use std::time::Duration;

use glance_net::{Command, EndpointId, NetConfig, Node, Relay, RoomEvent, RoomHandle};
use glance_proto::{MemberState, WireProfile};
use iroh::SecretKey;

async fn node() -> Node {
    Node::spawn(NetConfig {
        secret_key: SecretKey::generate(),
        relay: Relay::Disabled,
        files_dir: None,
    })
    .await
    .expect("spawn node")
}

fn profile(name: &str) -> WireProfile {
    WireProfile {
        name: name.into(),
        color: 0x5865f2,
    }
}

/// Wait for an event matching `pred`, skipping others.
async fn expect(
    room: &RoomHandle,
    what: &str,
    mut pred: impl FnMut(&RoomEvent) -> bool,
) -> RoomEvent {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        match tokio::time::timeout(left, room.events.recv_async()).await {
            Ok(Ok(ev)) if pred(&ev) => return ev,
            Ok(Ok(_)) => continue,
            Ok(Err(_)) => panic!("event channel closed waiting for {what}"),
            Err(_) => panic!("timed out waiting for {what}"),
        }
    }
}

async fn expect_members(room: &RoomHandle, who: &str, want: &[EndpointId]) {
    let want: Vec<_> = want.iter().map(|id| (*id, None)).collect();
    expect_members_flagged(room, who, &want).await;
}

/// Like `expect_members`, also checking each one's `existing` flag (already
/// in the room when `who` arrived) where it's given.
async fn expect_members_flagged(room: &RoomHandle, who: &str, want: &[(EndpointId, Option<bool>)]) {
    let mut seen = HashSet::new();
    while !want.iter().all(|(id, _)| seen.contains(id)) {
        if let RoomEvent::MemberJoined { id, existing, .. } =
            expect(room, &format!("{who} to see all members"), |e| {
                matches!(e, RoomEvent::MemberJoined { .. })
            })
            .await
        {
            if let Some((_, Some(want_existing))) = want.iter().find(|(w, _)| *w == id) {
                assert_eq!(existing, *want_existing, "{who}: existing flag of {id}");
            }
            seen.insert(id);
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn three_peers_mesh_chat_state_and_leave() {
    let (na, nb, nc) = (node().await, node().await, node().await);
    let a = na
        .create_room(profile("Ana"), MemberState::default())
        .await
        .unwrap();
    let ticket = glance_proto::Ticket::parse(&a.link()).unwrap();

    let b = nb
        .join_room(ticket.clone(), profile("Ben"), MemberState::default())
        .await
        .unwrap();
    expect_members_flagged(&b, "b", &[(na.id(), Some(true))]).await;
    let c = nc
        .join_room(ticket, profile("Cy"), MemberState::default())
        .await
        .unwrap();

    // Everyone sees everyone (C reached B through A's Welcome list).
    // Only C sees A and B as already there.
    expect_members_flagged(&a, "a", &[(nb.id(), Some(false)), (nc.id(), Some(false))]).await;
    expect_members_flagged(&c, "c", &[(na.id(), Some(true)), (nb.id(), Some(true))]).await;
    expect_members_flagged(&b, "b", &[(nc.id(), Some(false))]).await;

    // Chat from A reaches B and C.
    a.send(Command::Chat("hi all".into()));
    for (room, who) in [(&b, "b"), (&c, "c")] {
        let ev = expect(room, &format!("chat at {who}"), |e| {
            matches!(e, RoomEvent::Chat { .. })
        })
        .await;
        assert_eq!(
            ev,
            match ev.clone() {
                RoomEvent::Chat { sent_at, .. } => RoomEvent::Chat {
                    from: na.id(),
                    sent_at,
                    text: "hi all".into()
                },
                _ => unreachable!(),
            }
        );
    }

    // B mutes; A and C see it.
    let muted = MemberState {
        muted: true,
        ..Default::default()
    };
    b.send(Command::SetState(muted.clone()));
    for (room, who) in [(&a, "a"), (&c, "c")] {
        let ev = expect(room, &format!("state at {who}"), |e| {
            matches!(e, RoomEvent::MemberUpdated { .. })
        })
        .await;
        assert_eq!(
            ev,
            RoomEvent::MemberUpdated {
                id: nb.id(),
                state: muted.clone()
            }
        );
    }

    // C leaves; A and B see it.
    c.send(Command::Leave);
    for (room, who) in [(&a, "a"), (&b, "b")] {
        let ev = expect(room, &format!("leave at {who}"), |e| {
            matches!(e, RoomEvent::MemberLeft { .. })
        })
        .await;
        assert_eq!(ev, RoomEvent::MemberLeft { id: nc.id() });
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn forged_ticket_is_rejected() {
    let (na, nd) = (node().await, node().await);
    let a = na
        .create_room(profile("Ana"), MemberState::default())
        .await
        .unwrap();
    let mut forged = a.ticket.clone();
    forged.secret[0] ^= 0xff;
    let d = nd
        .join_room(forged, profile("Mallory"), MemberState::default())
        .await
        .unwrap();
    let ev = expect(&d, "rejection", |e| matches!(e, RoomEvent::Problem(_))).await;
    let RoomEvent::Problem(msg) = ev else {
        unreachable!()
    };
    assert!(msg.contains("rejected"), "{msg}");
    // A never saw a member join.
    assert!(
        a.events
            .try_iter()
            .all(|e| !matches!(e, RoomEvent::MemberJoined { .. }))
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn chat_sent_right_after_joining_arrives() {
    let (na, nb) = (node().await, node().await);
    let a = na
        .create_room(profile("Ana"), MemberState::default())
        .await
        .unwrap();
    let b = nb
        .join_room(a.ticket.clone(), profile("Ben"), MemberState::default())
        .await
        .unwrap();
    expect_members(&b, "b", &[na.id()]).await;
    b.send(Command::Chat("hi Ana!".into()));
    let ev = expect(&a, "greeting at a", |e| matches!(e, RoomEvent::Chat { .. })).await;
    assert!(
        matches!(ev, RoomEvent::Chat { ref text, .. } if text == "hi Ana!"),
        "{ev:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn long_chat_is_cut_to_the_limit() {
    let (na, nb) = (node().await, node().await);
    let a = na
        .create_room(profile("Ana"), MemberState::default())
        .await
        .unwrap();
    let b = nb
        .join_room(a.ticket.clone(), profile("Ben"), MemberState::default())
        .await
        .unwrap();
    expect_members(&b, "b", &[na.id()]).await;
    // Multi-byte characters straddle the limit: the cut lands on a boundary.
    let long = "é".repeat(glance_proto::MAX_CHAT_LEN);
    b.send(Command::Chat(long));
    let ev = expect(&a, "long chat at a", |e| {
        matches!(e, RoomEvent::Chat { .. })
    })
    .await;
    let RoomEvent::Chat { text, .. } = ev else {
        unreachable!()
    };
    assert_eq!(text.len(), glance_proto::MAX_CHAT_LEN);
    assert!(text.chars().all(|c| c == 'é'));
}

fn live() -> MemberState {
    MemberState {
        stream: Some(glance_proto::WireStream {
            width: 64,
            height: 64,
            fps: 30,
            has_audio: false,
            source: "Screen".into(),
        }),
        ..Default::default()
    }
}

fn packet(n: u8, keyframe: bool) -> std::sync::Arc<glance_proto::VideoPacket> {
    std::sync::Arc::new(glance_proto::VideoPacket {
        ts_us: n as u64,
        keyframe,
        width: 64,
        height: 64,
        data: vec![n; 1000 + n as usize * 1000],
    })
}

#[tokio::test(flavor = "multi_thread")]
async fn video_reaches_watchers_starting_at_a_keyframe() {
    let (na, nb, nc) = (node().await, node().await, node().await);
    let a = na.create_room(profile("Ana"), live()).await.unwrap();
    let b = nb
        .join_room(a.ticket.clone(), profile("Ben"), MemberState::default())
        .await
        .unwrap();
    let c = nc
        .join_room(a.ticket.clone(), profile("Cy"), MemberState::default())
        .await
        .unwrap();
    expect_members(&b, "b", &[na.id()]).await;
    expect_members(&c, "c", &[na.id()]).await;

    b.send(Command::Watch(na.id(), glance_proto::Track::Screen));
    c.send(Command::Watch(na.id(), glance_proto::Track::Screen));
    let mut watchers = 0;
    while watchers < 2 {
        if let RoomEvent::Watchers { count: n, .. } =
            expect(&a, "watchers", |e| matches!(e, RoomEvent::Watchers { .. })).await
        {
            watchers = n;
        }
    }

    // A non-keyframe first: viewers must skip it.
    a.send(Command::Video(
        glance_proto::Track::Screen,
        packet(1, false),
    ));
    a.send(Command::Video(glance_proto::Track::Screen, packet(2, true)));
    a.send(Command::Video(
        glance_proto::Track::Screen,
        packet(3, false),
    ));

    for (room, who) in [(&b, "b"), (&c, "c")] {
        let RoomEvent::Video { from, feed, .. } = expect(room, &format!("video at {who}"), |e| {
            matches!(e, RoomEvent::Video { .. })
        })
        .await
        else {
            unreachable!()
        };
        assert_eq!(from, na.id());
        let first = tokio::time::timeout(Duration::from_secs(10), feed.0.recv_async())
            .await
            .unwrap()
            .unwrap();
        assert!(first.keyframe && first.ts_us == 2, "{who} got {first:?}");
        let second = tokio::time::timeout(Duration::from_secs(10), feed.0.recv_async())
            .await
            .unwrap()
            .unwrap();
        assert_eq!((second.ts_us, second.data.len()), (3, 4000));
    }

    // Any member's link works on its own: Cy's link names Cy as host.
    let link = glance_proto::Ticket::parse(&c.link()).unwrap();
    assert_eq!(link.host.id, nc.id());
    assert_eq!(link.room, a.ticket.room);

    b.send(Command::Unwatch(na.id(), glance_proto::Track::Screen));
    let ev = expect(&a, "watcher count drops", |e| {
        matches!(e, RoomEvent::Watchers { count: 1, .. })
    })
    .await;
    assert_eq!(
        ev,
        RoomEvent::Watchers {
            track: glance_proto::Track::Screen,
            count: 1
        }
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn voice_datagrams_reach_the_audio_sink() {
    let (na, nb) = (node().await, node().await);
    let a = na
        .create_room(profile("Ana"), MemberState::default())
        .await
        .unwrap();
    let b = nb
        .join_room(a.ticket.clone(), profile("Ben"), MemberState::default())
        .await
        .unwrap();
    expect_members(&b, "b", &[na.id()]).await;
    expect_members(&a, "a", &[nb.id()]).await;
    let (tx, rx) = flume::unbounded();
    b.send(Command::SetAudioSink(Some(tx)));
    let pkt = glance_proto::AudioDatagram {
        seq: 7,
        level: 40,
        data: vec![1, 2, 3],
        stream: false,
    };
    let bytes = bytes::Bytes::from(glance_proto::encode(&pkt).unwrap()[4..].to_vec());
    // Datagrams are unreliable: send a few.
    for _ in 0..5 {
        a.send(Command::Audio(bytes.clone()));
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let (from, got) = tokio::time::timeout(Duration::from_secs(5), rx.recv_async())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(from, na.id());
    assert_eq!(got, pkt);
}

#[tokio::test(flavor = "multi_thread")]
async fn stream_audio_goes_only_to_screen_watchers() {
    let (na, nb, nc) = (node().await, node().await, node().await);
    let a = na.create_room(profile("Ana"), live()).await.unwrap();
    let b = nb
        .join_room(a.ticket.clone(), profile("Ben"), MemberState::default())
        .await
        .unwrap();
    let c = nc
        .join_room(a.ticket.clone(), profile("Cy"), MemberState::default())
        .await
        .unwrap();
    expect_members(&b, "b", &[na.id(), nc.id()]).await;
    expect_members(&c, "c", &[na.id(), nb.id()]).await;
    expect_members(&a, "a", &[nb.id(), nc.id()]).await;
    let (btx, brx) = flume::unbounded();
    let (ctx, crx) = flume::unbounded();
    b.send(Command::SetAudioSink(Some(btx)));
    c.send(Command::SetAudioSink(Some(ctx)));
    b.send(Command::Watch(na.id(), glance_proto::Track::Screen));
    expect(&a, "watcher", |e| {
        matches!(e, RoomEvent::Watchers { count: 1, .. })
    })
    .await;
    let pkt = glance_proto::AudioDatagram {
        seq: 1,
        level: 10,
        data: vec![9; 40],
        stream: true,
    };
    let bytes = bytes::Bytes::from(glance_proto::encode(&pkt).unwrap()[4..].to_vec());
    for _ in 0..5 {
        a.send(Command::StreamAudio(bytes.clone()));
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let (from, got) = tokio::time::timeout(Duration::from_secs(5), brx.recv_async())
        .await
        .unwrap()
        .unwrap();
    assert_eq!((from, got), (na.id(), pkt));
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(crx.is_empty(), "a non-watcher got stream audio");
}

#[tokio::test(flavor = "multi_thread")]
async fn files_transfer_and_second_downloader_can_fetch_from_first() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_test_writer()
        .try_init();
    let (na, nb, nc) = (node().await, node().await, node().await);
    let a = na
        .create_room(profile("Ana"), MemberState::default())
        .await
        .unwrap();
    let b = nb
        .join_room(a.ticket.clone(), profile("Ben"), MemberState::default())
        .await
        .unwrap();
    expect_members(&b, "b", &[na.id()]).await;

    // Ana shares a 3 MB file.
    let dir = std::env::temp_dir().join(format!("glance-ft-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let src = dir.join("big.bin");
    let data: Vec<u8> = (0..3_000_000u32).map(|i| (i % 251) as u8).collect();
    std::fs::write(&src, &data).unwrap();
    let (hash, size) = na.files().add(&src).await.unwrap();
    assert_eq!(size, data.len() as u64);
    a.send(Command::OfferFile(glance_proto::WireFile {
        hash,
        name: "big.bin".into(),
        size,
        mime: "application/octet-stream".into(),
    }));
    let RoomEvent::FileOffered { from, file, .. } = expect(&b, "file offer", |e| {
        matches!(e, RoomEvent::FileOffered { .. })
    })
    .await
    else {
        unreachable!()
    };
    assert_eq!((from, file.hash), (na.id(), hash));

    // Ben downloads with progress, exports, and the bytes match.
    let mut last = 0;
    nb.files()
        .download(nb.endpoint(), hash, vec![na.id()], |p| {
            if let glance_net::files::Progress::Bytes(n) = p {
                last = n;
            }
        })
        .await
        .unwrap();
    assert!(last > 0);
    let out = dir.join("got.bin");
    nb.files().export(hash, &out).await.unwrap();
    assert_eq!(std::fs::read(&out).unwrap(), data);

    // Cy joins (so is connected to Ben), Ana goes away; Cy can still get
    // the file from Ben.
    let c = nc
        .join_room(a.ticket.clone(), profile("Cy"), MemberState::default())
        .await
        .unwrap();
    expect_members(&c, "c", &[na.id(), nb.id()]).await;
    na.shutdown().await;
    // An offer that understates the size is refused part way.
    let mut failed = false;
    let r = nc
        .files()
        .download_at_most(nc.endpoint(), hash, vec![nb.id()], 1000, |p| {
            if matches!(p, glance_net::files::Progress::Failed(_)) {
                failed = true;
            }
        })
        .await;
    assert!(r.is_err() && failed);
    nc.files()
        .download(nc.endpoint(), hash, vec![nb.id()], |_| {})
        .await
        .unwrap();
    assert!(nc.files().has(hash).await);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread")]
async fn download_of_unknown_file_fails_cleanly() {
    let (na, nb) = (node().await, node().await);
    let mut failed = false;
    let r = nb
        .files()
        .download(nb.endpoint(), [7; 32], vec![na.id()], |p| {
            if matches!(p, glance_net::files::Progress::Failed(_)) {
                failed = true;
            }
        })
        .await;
    assert!(r.is_err() && failed);
}

#[tokio::test(flavor = "multi_thread")]
async fn late_joiner_receives_chat_and_file_history() {
    let (na, nb, nc) = (node().await, node().await, node().await);
    let a = na
        .create_room(profile("Ana"), MemberState::default())
        .await
        .unwrap();
    let b = nb
        .join_room(a.ticket.clone(), profile("Ben"), MemberState::default())
        .await
        .unwrap();
    expect_members(&b, "b", &[na.id()]).await;
    b.send(Command::Chat("before you came".into()));
    expect(&a, "chat at a", |e| matches!(e, RoomEvent::Chat { .. })).await;
    a.send(Command::OfferFile(glance_proto::WireFile {
        hash: [3; 32],
        name: "notes.txt".into(),
        size: 10,
        mime: "text/plain".into(),
    }));
    expect(&b, "offer at b", |e| {
        matches!(e, RoomEvent::FileOffered { .. })
    })
    .await;

    let c = nc
        .join_room(a.ticket.clone(), profile("Cy"), MemberState::default())
        .await
        .unwrap();
    let RoomEvent::History(items) =
        expect(&c, "history", |e| matches!(e, RoomEvent::History(_))).await
    else {
        unreachable!()
    };
    assert_eq!(items.len(), 2, "{items:?}");
    assert_eq!(items[0].author, nb.id());
    assert_eq!(items[0].author_name, "Ben");
    assert!(matches!(&items[0].body, glance_proto::HistoryBody::Text(t) if t == "before you came"));
    assert!(matches!(&items[1].body, glance_proto::HistoryBody::File(f) if f.name == "notes.txt"));
    // History arrives once even though Cy connects to both Ana and Ben.
    expect_members(&c, "c", &[na.id(), nb.id()]).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        c.events
            .try_iter()
            .all(|e| !matches!(e, RoomEvent::History(_)))
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn rewatching_survives_the_old_stream_closing() {
    let (na, nb) = (node().await, node().await);
    let a = na.create_room(profile("Ana"), live()).await.unwrap();
    let b = nb
        .join_room(a.ticket.clone(), profile("Ben"), MemberState::default())
        .await
        .unwrap();
    expect_members(&b, "b", &[na.id()]).await;
    b.send(Command::Watch(na.id(), glance_proto::Track::Screen));
    expect(&a, "watcher", |e| {
        matches!(e, RoomEvent::Watchers { count: 1, .. })
    })
    .await;
    a.send(Command::Video(glance_proto::Track::Screen, packet(1, true)));
    let RoomEvent::Video { feed: first, .. } =
        expect(&b, "first feed", |e| matches!(e, RoomEvent::Video { .. })).await
    else {
        unreachable!()
    };
    // Pop-out style: drop the old feed, unwatch and watch again right away.
    drop(first);
    b.send(Command::Unwatch(na.id(), glance_proto::Track::Screen));
    b.send(Command::Watch(na.id(), glance_proto::Track::Screen));
    let RoomEvent::Video { feed: second, .. } =
        expect(&b, "second feed", |e| matches!(e, RoomEvent::Video { .. })).await
    else {
        unreachable!()
    };
    // Keep sending; the old stream's teardown must not kill the new one.
    let pump = {
        let cmd = a.commander();
        tokio::spawn(async move {
            for n in 2..40u8 {
                let _ = cmd.send(Command::Video(glance_proto::Track::Screen, packet(n, true)));
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
    };
    let mut got = 0;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while got < 10 && tokio::time::Instant::now() < deadline {
        if tokio::time::timeout(Duration::from_millis(500), second.0.recv_async())
            .await
            .is_ok_and(|r| r.is_ok())
        {
            got += 1;
        }
    }
    pump.await.unwrap();
    assert!(got >= 10, "new stream died after {got} packets");
}

#[tokio::test(flavor = "multi_thread")]
async fn screen_and_camera_tracks_are_independent() {
    use glance_proto::Track;
    let (na, nb) = (node().await, node().await);
    let state = MemberState {
        camera: true,
        ..live()
    };
    let a = na.create_room(profile("Ana"), state).await.unwrap();
    let b = nb
        .join_room(a.ticket.clone(), profile("Ben"), MemberState::default())
        .await
        .unwrap();
    expect_members(&b, "b", &[na.id()]).await;
    b.send(Command::Watch(na.id(), Track::Screen));
    b.send(Command::Watch(na.id(), Track::Camera));
    // Keep sending on both tracks until each feed has received a packet.
    let pump = {
        let cmd = a.commander();
        tokio::spawn(async move {
            for _ in 0..400 {
                let _ = cmd.send(Command::Video(Track::Screen, packet(10, true)));
                let _ = cmd.send(Command::Video(Track::Camera, packet(20, true)));
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
    };
    let mut feeds = std::collections::HashMap::new();
    while feeds.len() < 2 {
        let RoomEvent::Video { track, feed, .. } =
            expect(&b, "video feeds", |e| matches!(e, RoomEvent::Video { .. })).await
        else {
            unreachable!()
        };
        feeds.insert(track, feed);
    }
    let screen = tokio::time::timeout(Duration::from_secs(5), feeds[&Track::Screen].0.recv_async())
        .await
        .unwrap()
        .unwrap();
    let camera = tokio::time::timeout(Duration::from_secs(5), feeds[&Track::Camera].0.recv_async())
        .await
        .unwrap()
        .unwrap();
    pump.abort();
    assert_eq!((screen.ts_us, camera.ts_us), (10, 20));
    // Turning the camera off drops only camera watchers.
    a.send(Command::SetState(live()));
    let ev = expect(&a, "camera watchers cleared", |e| {
        matches!(
            e,
            RoomEvent::Watchers {
                track: Track::Camera,
                count: 0
            }
        )
    })
    .await;
    assert_eq!(
        ev,
        RoomEvent::Watchers {
            track: Track::Camera,
            count: 0
        }
    );
}
