//! Contacts between in-process peers (who first met in a room, as in the app).

use std::collections::HashMap;
use std::time::Duration;

use glance_net::contacts::{ContactCommand, ContactEvent, ContactsHandle, Relation};
use glance_net::{NetConfig, Node, Relay, RoomEvent};
use glance_proto::{MemberState, PresenceInfo, WirePresence, WireProfile};
use iroh::SecretKey;

async fn node(name: &str) -> Node {
    let n = Node::spawn(NetConfig {
        secret_key: SecretKey::generate(),
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

/// Put two nodes in a room so their endpoints know each other's address.
async fn meet(a: &Node, b: &Node) -> (glance_net::RoomHandle, glance_net::RoomHandle) {
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
    (ra, rb)
}

fn rel(id: iroh::PublicKey, r: Relation) -> HashMap<iroh::PublicKey, Relation> {
    HashMap::from([(id, r)])
}

#[tokio::test(flavor = "multi_thread")]
async fn request_accept_presence_ring_and_join() {
    let (a, b) = (node("Ana").await, node("Ben").await);
    let _rooms = meet(&a, &b).await;
    let (ca, cb) = (a.contacts().clone(), b.contacts().clone());

    // Ana asks; Ben accepts.
    ca.send(ContactCommand::SetRelations(rel(
        b.id(),
        Relation::Outgoing,
    )));
    ca.send(ContactCommand::SendRequest(b.id()));
    let ev = expect(&cb, "request", |e| {
        matches!(e, ContactEvent::RequestReceived { .. })
    })
    .await;
    assert_eq!(
        ev,
        ContactEvent::RequestReceived {
            id: a.id(),
            profile: profile("Ana")
        }
    );
    cb.send(ContactCommand::SetRelations(rel(a.id(), Relation::Mutual)));
    cb.send(ContactCommand::Accept(a.id()));
    let ev = expect(&ca, "accepted", |e| {
        matches!(e, ContactEvent::Accepted { .. })
    })
    .await;
    assert_eq!(
        ev,
        ContactEvent::Accepted {
            id: b.id(),
            profile: profile("Ben")
        }
    );
    ca.send(ContactCommand::SetRelations(rel(b.id(), Relation::Mutual)));
    // Ana's connection to Ben predates the acceptance: Ben must now show online.
    let ev = expect(&ca, "ben online", |e| {
        matches!(e, ContactEvent::Online { .. })
    })
    .await;
    assert!(matches!(ev, ContactEvent::Online { id, .. } if id == b.id()));

    // Presence flows between mutual contacts.
    let info = PresenceInfo {
        presence: WirePresence::Online,
        in_call: Some(2),
        joinable: true,
    };
    ca.send(ContactCommand::SetPresence(Some(info.clone())));
    let ev = expect(&cb, "presence", |e| {
        matches!(e, ContactEvent::Presence { .. })
    })
    .await;
    assert_eq!(ev, ContactEvent::Presence { id: a.id(), info });

    // Ring and answer.
    ca.send(ContactCommand::Ring {
        to: b.id(),
        call_id: 5,
        link: "glance://join/abc".into(),
        members: vec!["Ana".into()],
    });
    let ev = expect(&cb, "ring", |e| matches!(e, ContactEvent::Ring { .. })).await;
    assert_eq!(
        ev,
        ContactEvent::Ring {
            from: a.id(),
            call_id: 5,
            link: "glance://join/abc".into(),
            members: vec!["Ana".into()]
        }
    );
    cb.send(ContactCommand::Answer {
        to: a.id(),
        call_id: 5,
        accepted: false,
    });
    let ev = expect(&ca, "answer", |e| {
        matches!(e, ContactEvent::RingAnswered { .. })
    })
    .await;
    assert_eq!(
        ev,
        ContactEvent::RingAnswered {
            from: b.id(),
            call_id: 5,
            accepted: false
        }
    );

    // Ben asks to join Ana's call.
    cb.send(ContactCommand::JoinRequest(a.id()));
    expect(&ca, "join request", |e| {
        matches!(e, ContactEvent::JoinRequested(_))
    })
    .await;
    ca.send(ContactCommand::JoinReply {
        to: b.id(),
        link: Some("glance://join/abc".into()),
    });
    let ev = expect(&cb, "ticket", |e| {
        matches!(e, ContactEvent::JoinTicket { .. })
    })
    .await;
    assert_eq!(
        ev,
        ContactEvent::JoinTicket {
            from: a.id(),
            link: Some("glance://join/abc".into())
        }
    );

    // Ana removes Ben.
    ca.send(ContactCommand::Remove(b.id()));
    let ev = expect(&cb, "removed", |e| matches!(e, ContactEvent::Removed(_))).await;
    assert_eq!(ev, ContactEvent::Removed(a.id()));
}

#[tokio::test(flavor = "multi_thread")]
async fn strangers_cant_ring_and_blocked_peers_are_refused() {
    let (a, b) = (node("Ana").await, node("Ben").await);
    let _rooms = meet(&a, &b).await;
    let (ca, cb) = (a.contacts().clone(), b.contacts().clone());

    // Not contacts: a ring is ignored.
    ca.send(ContactCommand::Ring {
        to: b.id(),
        call_id: 1,
        link: "x".into(),
        members: vec![],
    });
    // Ben blocks Ana: her request never arrives.
    cb.send(ContactCommand::SetRelations(rel(a.id(), Relation::Blocked)));
    tokio::time::sleep(Duration::from_millis(300)).await;
    ca.send(ContactCommand::SendRequest(b.id()));
    tokio::time::sleep(Duration::from_secs(1)).await;
    let got: Vec<ContactEvent> = cb.events.try_iter().collect();
    assert!(
        got.iter().all(|e| !matches!(
            e,
            ContactEvent::Ring { .. } | ContactEvent::RequestReceived { .. }
        )),
        "{got:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn files_go_to_mutual_contacts_outside_calls() {
    let (a, b) = (node("Ana").await, node("Ben").await);
    let rooms = meet(&a, &b).await;
    let (ca, cb) = (a.contacts().clone(), b.contacts().clone());
    ca.send(ContactCommand::SetRelations(rel(b.id(), Relation::Mutual)));
    cb.send(ContactCommand::SetRelations(rel(a.id(), Relation::Mutual)));
    // Leave the room: the file goes over the contacts link alone.
    drop(rooms);

    let dir = std::env::temp_dir().join(format!("glance-cfile-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let src = dir.join("notes.txt");
    std::fs::write(&src, b"hello from ana, outside a call").unwrap();
    let (hash, size) = a.files().add(&src).await.unwrap();
    let file = glance_proto::WireFile {
        hash,
        name: "notes.txt".into(),
        size,
        mime: "text/plain".into(),
    };
    ca.send(ContactCommand::SendFile {
        to: b.id(),
        file: file.clone(),
    });
    let ev = expect(&cb, "file", |e| {
        matches!(e, ContactEvent::FileReceived { .. })
    })
    .await;
    assert_eq!(
        ev,
        ContactEvent::FileReceived {
            from: a.id(),
            file: file.clone()
        }
    );
    b.files()
        .download(b.endpoint(), hash, vec![a.id()], |_| {})
        .await
        .unwrap();
    let out = dir.join("got.txt");
    b.files().export(hash, &out).await.unwrap();
    assert_eq!(std::fs::read(&out).unwrap(), std::fs::read(&src).unwrap());
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread")]
async fn messages_right_after_acceptance_are_not_dropped() {
    let (a, b) = (node("Ana").await, node("Ben").await);
    let _rooms = meet(&a, &b).await;
    let (ca, cb) = (a.contacts().clone(), b.contacts().clone());
    ca.send(ContactCommand::SetRelations(rel(
        b.id(),
        Relation::Outgoing,
    )));
    ca.send(ContactCommand::SendRequest(b.id()));
    expect(&cb, "request", |e| {
        matches!(e, ContactEvent::RequestReceived { .. })
    })
    .await;
    // Ben accepts and immediately sends a file; Ana's app hasn't updated its
    // relations yet.
    cb.send(ContactCommand::SetRelations(rel(a.id(), Relation::Mutual)));
    cb.send(ContactCommand::Accept(a.id()));
    let file = glance_proto::WireFile {
        hash: [3; 32],
        name: "hi.txt".into(),
        size: 2,
        mime: "text/plain".into(),
    };
    cb.send(ContactCommand::SendFile {
        to: a.id(),
        file: file.clone(),
    });
    let ev = expect(&ca, "file", |e| {
        matches!(e, ContactEvent::FileReceived { .. })
    })
    .await;
    assert_eq!(ev, ContactEvent::FileReceived { from: b.id(), file });
}
