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
    let mut seen = HashSet::new();
    while !want.iter().all(|id| seen.contains(id)) {
        if let RoomEvent::MemberJoined { id, .. } =
            expect(room, &format!("{who} to see all members"), |e| {
                matches!(e, RoomEvent::MemberJoined { .. })
            })
            .await
        {
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
    expect_members(&b, "b", &[na.id()]).await;
    let c = nc
        .join_room(ticket, profile("Cy"), MemberState::default())
        .await
        .unwrap();

    // Everyone sees everyone (C reached B through A's Welcome list).
    expect_members(&a, "a", &[nb.id(), nc.id()]).await;
    expect_members(&c, "c", &[na.id(), nb.id()]).await;
    expect_members(&b, "b", &[nc.id()]).await;

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
