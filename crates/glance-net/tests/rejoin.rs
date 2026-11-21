//! Rooms outlive whoever created them: probing a remembered room and
//! rejoining it through a member who's still there.

use std::collections::HashSet;
use std::time::Duration;

use glance_net::{Command, EndpointId, NetConfig, Node, Relay, RoomEvent, RoomHandle, RoomStatus};
use glance_proto::{HistoryBody, MemberState, RoomId, Ticket, WireProfile};
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
async fn room_outlives_its_host_and_can_be_probed_and_rejoined() {
    let (na, nb, nc) = (node().await, node().await, node().await);
    let a = na
        .create_room(profile("Ana"), MemberState::default())
        .await
        .unwrap();
    // The link as it was shared: Ana is the host.
    let ticket = Ticket::parse(&a.link()).unwrap();
    let b = nb
        .join_room(ticket.clone(), profile("Ben"), MemberState::default())
        .await
        .unwrap();
    expect_members(&b, "b", &[na.id()]).await;
    b.send(Command::Chat("still here".into()));
    expect(&a, "chat at a", |e| matches!(e, RoomEvent::Chat { .. })).await;

    // Ana leaves; Ben stays.
    a.send(Command::Leave);
    expect(
        &b,
        "ana leaving",
        |e| matches!(e, RoomEvent::MemberLeft { id } if *id == na.id()),
    )
    .await;
    drop(a);
    let (a_addr, b_addr) = (na.endpoint().addr(), nb.endpoint().addr());
    let timeout = Duration::from_secs(5);

    // Only Ana known: she answers that she isn't in it any more.
    let mut status = RoomStatus::Unknown;
    for _ in 0..50 {
        status = nc.probe_room(&ticket, vec![], timeout).await;
        if status == RoomStatus::Empty {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(status, RoomStatus::Empty);

    // Knowing Ben too: alive, one member.
    let status = nc
        .probe_room(&ticket, vec![a_addr.clone(), b_addr.clone()], timeout)
        .await;
    let RoomStatus::Alive(members) = status else {
        panic!("expected the room alive, got {status:?}")
    };
    assert_eq!(members.len(), 1);
    assert_eq!(members[0].addr.id, nb.id());
    assert_eq!(members[0].profile.name, "Ben");

    // A wrong secret is refused (reads as unknown), another room as empty.
    let mut forged = ticket.clone();
    forged.secret[0] ^= 0xff;
    forged.host = b_addr.clone();
    assert_eq!(
        nc.probe_room(&forged, vec![], timeout).await,
        RoomStatus::Unknown
    );
    let mut other = ticket.clone();
    other.room = RoomId([9; 8]);
    other.host = b_addr.clone();
    assert_eq!(
        nc.probe_room(&other, vec![], timeout).await,
        RoomStatus::Empty
    );

    // Cy rejoins with the stored ticket: Ana's dial fails, Ben lets him in.
    let c = nc
        .join_room_via(
            ticket.clone(),
            vec![a_addr, members[0].addr.clone()],
            profile("Cy"),
            MemberState::default(),
        )
        .await
        .unwrap();
    // History comes with Ben's welcome, before he shows up as a member.
    let RoomEvent::History(items) =
        expect(&c, "history", |e| matches!(e, RoomEvent::History(_))).await
    else {
        unreachable!()
    };
    assert!(matches!(&items[0].body, HistoryBody::Text(t) if t == "still here"));
    expect_members(&c, "c", &[nb.id()]).await;
    expect_members(&b, "b", &[nc.id()]).await;

    // And a probe now sees both of them.
    let RoomStatus::Alive(members) = na.probe_room(&ticket, vec![b_addr], timeout).await else {
        panic!("room should be alive")
    };
    assert_eq!(members.len(), 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn hosting_a_room_again_lets_old_links_find_it() {
    let (na, nb) = (node().await, node().await);
    let a = na
        .create_room(profile("Ana"), MemberState::default())
        .await
        .unwrap();
    let ticket = Ticket::parse(&a.link()).unwrap();
    a.send(Command::Leave);
    drop(a);
    // Ana was alone in it: nobody else to ask, so it reads as empty.
    assert_eq!(
        na.probe_room(&ticket, vec![], Duration::from_secs(5)).await,
        RoomStatus::Empty
    );
    // Ben starts it again with the same link; Ana probes via Ben.
    let b = nb
        .host_room(ticket.clone(), profile("Ben"), MemberState::default())
        .await
        .unwrap();
    let b_addr = nb.endpoint().addr();
    let RoomStatus::Alive(members) = na
        .probe_room(&ticket, vec![b_addr.clone()], Duration::from_secs(5))
        .await
    else {
        panic!("room should be alive")
    };
    assert_eq!(members.len(), 1);
    let a = na
        .join_room_via(ticket, vec![b_addr], profile("Ana"), MemberState::default())
        .await
        .unwrap();
    expect_members(&a, "a", &[nb.id()]).await;
    expect_members(&b, "b", &[na.id()]).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn own_posts_keep_their_timestamp() {
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
    a.send(Command::Post {
        sent_at: 1234,
        body: HistoryBody::Text("stamped".into()),
    });
    let ev = expect(&b, "chat at b", |e| matches!(e, RoomEvent::Chat { .. })).await;
    assert!(
        matches!(ev, RoomEvent::Chat { sent_at: 1234, ref text, .. } if text == "stamped"),
        "{ev:?}"
    );
}
