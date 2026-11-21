//! Direct messages between in-process contacts: delivery and acks, queueing
//! while the contact is offline, the contact rules, and 0.1.0 peers.

use std::collections::HashMap;
use std::time::Duration;

use glance_net::contacts::{ContactCommand, ContactEvent, ContactsHandle, DmFailure, Relation};
use glance_net::{NetConfig, Node, Relay, RoomEvent};
use glance_proto::{ContactMsg, MemberState, WireDm, WireProfile};
use iroh::SecretKey;

async fn node_with(key: SecretKey, name: &str) -> Node {
    let n = Node::spawn(NetConfig {
        secret_key: key,
        relay: Relay::Disabled,
        files_dir: None,
    })
    .await
    .unwrap();
    n.contacts().send(ContactCommand::SetProfile(profile(name)));
    n
}

fn profile(name: &str) -> WireProfile {
    WireProfile {
        name: name.into(),
        color: 7,
    }
}

fn dm(id: u64, text: &str) -> WireDm {
    WireDm {
        id,
        sent_at: id,
        text: text.into(),
    }
}

async fn expect(
    h: &ContactsHandle,
    what: &str,
    mut pred: impl FnMut(&ContactEvent) -> bool,
) -> ContactEvent {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        match tokio::time::timeout(left, h.events.recv_async()).await {
            Ok(Ok(ev)) if pred(&ev) => return ev,
            Ok(Ok(_)) => continue,
            _ => panic!("timed out waiting for {what}"),
        }
    }
}

/// Events that arrive within `wait` (to check that something didn't).
async fn collect(h: &ContactsHandle, wait: Duration) -> Vec<ContactEvent> {
    tokio::time::sleep(wait).await;
    h.events.try_iter().collect()
}

/// Put two nodes in a room so their endpoints know each other's address.
async fn meet(a: &Node, b: &Node) {
    let ra = a
        .create_room(profile("A"), MemberState::default())
        .await
        .unwrap();
    let rb = b
        .join_room(ra.ticket.clone(), profile("B"), MemberState::default())
        .await
        .unwrap();
    loop {
        if let Ok(Ok(RoomEvent::MemberJoined { .. })) =
            tokio::time::timeout(Duration::from_secs(10), rb.events.recv_async()).await
        {
            break;
        }
    }
}

fn rel(id: iroh::PublicKey, r: Relation) -> HashMap<iroh::PublicKey, Relation> {
    HashMap::from([(id, r)])
}

#[tokio::test(flavor = "multi_thread")]
async fn messages_are_acked_queued_offline_and_delivered_on_reconnect() {
    let ben_key = SecretKey::generate();
    let a = node_with(SecretKey::generate(), "Ana").await;
    let b = node_with(ben_key.clone(), "Ben").await;
    meet(&a, &b).await;
    let (ca, cb) = (a.contacts().clone(), b.contacts().clone());
    ca.send(ContactCommand::SetRelations(rel(b.id(), Relation::Mutual)));
    cb.send(ContactCommand::SetRelations(rel(a.id(), Relation::Mutual)));

    // Ana writes; Ben gets it and his app acks it automatically.
    ca.send(ContactCommand::SendDm {
        to: b.id(),
        dm: dm(1, "hi Ben"),
    });
    let ev = expect(&cb, "dm", |e| matches!(e, ContactEvent::DmReceived { .. })).await;
    assert_eq!(
        ev,
        ContactEvent::DmReceived {
            from: a.id(),
            dm: dm(1, "hi Ben")
        }
    );
    let ev = expect(&ca, "ack", |e| matches!(e, ContactEvent::DmAcked { .. })).await;
    assert_eq!(ev, ContactEvent::DmAcked { from: b.id(), id: 1 });

    // And back.
    cb.send(ContactCommand::SendDm {
        to: a.id(),
        dm: dm(2, "hey Ana"),
    });
    expect(&ca, "reply", |e| {
        matches!(e, ContactEvent::DmReceived { dm, .. } if dm.id == 2)
    })
    .await;
    expect(&cb, "reply ack", |e| {
        matches!(e, ContactEvent::DmAcked { id: 2, .. })
    })
    .await;

    // Ben goes offline: Ana's message can't go out.
    let ben_addr = b.endpoint().addr();
    b.shutdown().await;
    drop(b);
    expect(&ca, "ben offline", |e| matches!(e, ContactEvent::Offline(_))).await;
    ca.send(ContactCommand::SendDm {
        to: ben_addr.id,
        dm: dm(3, "are you there?"),
    });
    let ev = expect(&ca, "failed", |e| matches!(e, ContactEvent::DmFailed { .. })).await;
    assert_eq!(
        ev,
        ContactEvent::DmFailed {
            to: ben_addr.id,
            id: 3,
            reason: DmFailure::Offline
        }
    );

    // Ben comes back (same identity). Once he's online, Ana's app resends
    // what's queued, plus a duplicate of an old one whose ack it "lost".
    let b = node_with(ben_key, "Ben").await;
    let cb = b.contacts().clone();
    cb.send(ContactCommand::SetRelations(rel(a.id(), Relation::Mutual)));
    // No relays or discovery in tests: tell each side the other's new address.
    let lookup = iroh::address_lookup::MemoryLookup::new();
    lookup.add_endpoint_info(a.endpoint().addr());
    lookup.add_endpoint_info(b.endpoint().addr());
    a.endpoint().address_lookup().unwrap().add(lookup.clone());
    b.endpoint().address_lookup().unwrap().add(lookup);
    cb.send(ContactCommand::ConnectAll);
    expect(&ca, "ben back", |e| matches!(e, ContactEvent::Online { .. })).await;
    ca.send(ContactCommand::SendDm {
        to: b.id(),
        dm: dm(3, "are you there?"),
    });
    expect(&cb, "queued dm", |e| {
        matches!(e, ContactEvent::DmReceived { dm, .. } if dm.id == 3)
    })
    .await;
    expect(&ca, "queued ack", |e| {
        matches!(e, ContactEvent::DmAcked { id: 3, .. })
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn strangers_cant_message_and_blocked_peers_are_dropped() {
    let a = node_with(SecretKey::generate(), "Ana").await;
    let b = node_with(SecretKey::generate(), "Ben").await;
    meet(&a, &b).await;
    let (ca, cb) = (a.contacts().clone(), b.contacts().clone());

    // Ana's own app refuses to message a non-contact.
    ca.send(ContactCommand::SendDm {
        to: b.id(),
        dm: dm(1, "hello?"),
    });
    let ev = expect(&ca, "refused", |e| matches!(e, ContactEvent::DmFailed { .. })).await;
    assert_eq!(
        ev,
        ContactEvent::DmFailed {
            to: b.id(),
            id: 1,
            reason: DmFailure::NotContact
        }
    );

    // Ana thinks they're contacts, Ben doesn't: Ben ignores the message
    // and doesn't ack it.
    ca.send(ContactCommand::SetRelations(rel(b.id(), Relation::Mutual)));
    ca.send(ContactCommand::SendDm {
        to: b.id(),
        dm: dm(2, "let me in"),
    });
    let got = collect(&cb, Duration::from_secs(2)).await;
    assert!(
        got.iter()
            .all(|e| !matches!(e, ContactEvent::DmReceived { .. })),
        "{got:?}"
    );
    let got = collect(&ca, Duration::from_millis(100)).await;
    assert!(
        got.iter().all(|e| !matches!(e, ContactEvent::DmAcked { .. })),
        "{got:?}"
    );

    // Ben blocks Ana: the connection is dropped and nothing gets through.
    cb.send(ContactCommand::SetRelations(rel(a.id(), Relation::Blocked)));
    tokio::time::sleep(Duration::from_millis(300)).await;
    ca.send(ContactCommand::SendDm {
        to: b.id(),
        dm: dm(3, "blocked?"),
    });
    let got = collect(&cb, Duration::from_secs(2)).await;
    assert!(
        got.iter()
            .all(|e| !matches!(e, ContactEvent::DmReceived { .. })),
        "{got:?}"
    );
}

/// A 0.1.0 peer only speaks `glance/contacts/1`: it still connects, and
/// messages to it fail as unsupported instead of breaking the link.
#[tokio::test(flavor = "multi_thread")]
async fn old_peers_still_connect_but_get_no_messages() {
    let b = node_with(SecretKey::generate(), "Ben").await;
    let cb = b.contacts().clone();
    let old = iroh::Endpoint::builder(iroh::endpoint::presets::Minimal)
        .relay_mode(iroh::RelayMode::Disabled)
        .alpns(vec![glance_proto::CONTACTS_ALPN_V1.to_vec()])
        .bind()
        .await
        .unwrap();
    cb.send(ContactCommand::SetRelations(rel(old.id(), Relation::Mutual)));
    tokio::time::sleep(Duration::from_millis(200)).await;

    let conn = old
        .connect(b.endpoint().addr(), glance_proto::CONTACTS_ALPN_V1)
        .await
        .unwrap();
    let (mut send, mut recv) = conn.open_bi().await.unwrap();
    let hello = ContactMsg::Hello {
        profile: profile("Old"),
        presence: None,
    };
    send.write_all(&glance_proto::encode(&hello).unwrap())
        .await
        .unwrap();
    // Ben answers with his Hello, in the version 1 encoding.
    let mut len = [0u8; 4];
    recv.read_exact(&mut len).await.unwrap();
    let mut body = vec![0u8; glance_proto::frame::body_len(len).unwrap()];
    recv.read_exact(&mut body).await.unwrap();
    assert!(matches!(
        glance_proto::decode::<ContactMsg>(&body).unwrap(),
        ContactMsg::Hello { .. }
    ));
    expect(&cb, "old peer online", |e| {
        matches!(e, ContactEvent::Online { .. })
    })
    .await;

    cb.send(ContactCommand::SendDm {
        to: old.id(),
        dm: dm(5, "hi"),
    });
    let ev = expect(&cb, "unsupported", |e| {
        matches!(e, ContactEvent::DmFailed { .. })
    })
    .await;
    assert_eq!(
        ev,
        ContactEvent::DmFailed {
            to: old.id(),
            id: 5,
            reason: DmFailure::Unsupported
        }
    );
    // The link is still up (nothing undecodable was sent).
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(conn.close_reason().is_none());
}
