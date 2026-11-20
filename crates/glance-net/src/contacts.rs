//! The contacts actor: always running (not tied to a room). Keeps a
//! connection to each online mutual contact for presence and rings, and
//! handles contact requests from people you've been in a room with.
//!
//! Policy: blocked peers are refused at connect; peers that aren't contacts
//! may only send `Request` (and the initial `Hello`).

use std::collections::HashMap;
use std::time::Duration;

use anyhow::{Context, Result};
use glance_proto::{
    CONTACTS_ALPN, ContactMsg, EndpointAddr, EndpointId, PresenceInfo, WireProfile,
};
use iroh::Endpoint;
use iroh::endpoint::{Connection, RecvStream, SendStream};
use tokio::sync::mpsc;
use tracing::debug;

use crate::io::{read_frame, write_frame};

/// How we relate to a peer (the app's contact book decides).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Relation {
    Mutual,
    /// We asked them.
    Outgoing,
    /// They asked us.
    Incoming,
    Blocked,
}

#[derive(Debug, Clone)]
pub enum ContactCommand {
    SetProfile(WireProfile),
    /// `None` = invisible (contacts see us offline; no presence sent).
    SetPresence(Option<PresenceInfo>),
    /// Replace the relation table (from the contact book).
    SetRelations(HashMap<EndpointId, Relation>),
    /// Dial mutual contacts we aren't connected to.
    ConnectAll,
    SendRequest(EndpointId),
    Accept(EndpointId),
    Remove(EndpointId),
    Ring {
        to: EndpointId,
        call_id: u64,
        link: String,
        members: Vec<String>,
    },
    CancelRing {
        to: EndpointId,
        call_id: u64,
    },
    Answer {
        to: EndpointId,
        call_id: u64,
        accepted: bool,
    },
    JoinRequest(EndpointId),
    JoinReply {
        to: EndpointId,
        link: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContactEvent {
    /// A contact connected (presence is `None` for non-mutual peers).
    Online {
        id: EndpointId,
        profile: WireProfile,
        presence: Option<PresenceInfo>,
    },
    Offline(EndpointId),
    Presence {
        id: EndpointId,
        info: PresenceInfo,
    },
    RequestReceived {
        id: EndpointId,
        profile: WireProfile,
    },
    Accepted {
        id: EndpointId,
        profile: WireProfile,
    },
    Removed(EndpointId),
    Ring {
        from: EndpointId,
        call_id: u64,
        link: String,
        members: Vec<String>,
    },
    RingCancelled {
        from: EndpointId,
        call_id: u64,
    },
    RingAnswered {
        from: EndpointId,
        call_id: u64,
        accepted: bool,
    },
    JoinRequested(EndpointId),
    JoinTicket {
        from: EndpointId,
        link: Option<String>,
    },
    /// A request or ring couldn't be delivered (peer unreachable).
    Undeliverable {
        to: EndpointId,
        what: String,
    },
}

pub(crate) enum Inbound {
    Incoming(Connection),
    Connected {
        conn: Connection,
        send: SendStream,
        recv: RecvStream,
        profile: WireProfile,
        presence: Option<PresenceInfo>,
        dialer: EndpointId,
    },
    DialFailed {
        id: EndpointId,
    },
    Msg {
        from: EndpointId,
        conn_id: usize,
        msg: ContactMsg,
    },
    Down {
        id: EndpointId,
        conn_id: usize,
    },
    Cmd(ContactCommand),
}

struct Peer {
    conn: Connection,
    dialer: EndpointId,
    tx: mpsc::UnboundedSender<ContactMsg>,
    profile: WireProfile,
}

struct Actor {
    endpoint: Endpoint,
    me: EndpointId,
    profile: WireProfile,
    presence: Option<PresenceInfo>,
    relations: HashMap<EndpointId, Relation>,
    peers: HashMap<EndpointId, Peer>,
    /// Messages waiting for a dial to finish.
    pending: HashMap<EndpointId, Vec<ContactMsg>>,
    inbound: flume::Sender<Inbound>,
    events: flume::Sender<ContactEvent>,
}

#[derive(Clone)]
pub struct ContactsHandle {
    commands: flume::Sender<Inbound>,
    pub events: flume::Receiver<ContactEvent>,
}

impl ContactsHandle {
    pub fn send(&self, cmd: ContactCommand) {
        let _ = self.commands.send(Inbound::Cmd(cmd));
    }
}

pub(crate) fn spawn(endpoint: Endpoint) -> (ContactsHandle, flume::Sender<Inbound>) {
    let (in_tx, in_rx) = flume::unbounded();
    let (ev_tx, ev_rx) = flume::unbounded();
    let me = endpoint.id();
    let mut actor = Actor {
        endpoint,
        me,
        profile: WireProfile {
            name: String::new(),
            color: 0,
        },
        presence: None,
        relations: HashMap::new(),
        peers: HashMap::new(),
        pending: HashMap::new(),
        inbound: in_tx.clone(),
        events: ev_tx,
    };
    tokio::spawn(async move {
        let mut retry = tokio::time::interval(Duration::from_secs(30));
        retry.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                msg = in_rx.recv_async() => {
                    let Ok(msg) = msg else { break };
                    actor.handle(msg);
                }
                _ = retry.tick() => actor.connect_all(),
            }
        }
    });
    (
        ContactsHandle {
            commands: in_tx.clone(),
            events: ev_rx,
        },
        in_tx,
    )
}

impl Actor {
    fn relation(&self, id: &EndpointId) -> Option<Relation> {
        self.relations.get(id).copied()
    }

    fn handle(&mut self, msg: Inbound) {
        match msg {
            Inbound::Incoming(conn) => {
                if self.relation(&conn.remote_id()) == Some(Relation::Blocked) {
                    conn.close(4u32.into(), b"blocked");
                    return;
                }
                let inbound = self.inbound.clone();
                tokio::spawn(async move {
                    let result: Result<(SendStream, RecvStream, ContactMsg)> = async {
                        let (send, mut recv) =
                            tokio::time::timeout(Duration::from_secs(10), conn.accept_bi())
                                .await
                                .context("hello timeout")??;
                        let hello =
                            tokio::time::timeout(Duration::from_secs(10), read_frame(&mut recv))
                                .await
                                .context("hello timeout")??;
                        Ok((send, recv, hello))
                    }
                    .await;
                    match result {
                        Ok((send, recv, ContactMsg::Hello { profile, presence })) => {
                            let dialer = conn.remote_id();
                            let _ = inbound.send(Inbound::Connected {
                                conn,
                                send,
                                recv,
                                profile,
                                presence,
                                dialer,
                            });
                        }
                        _ => conn.close(2u32.into(), b"bad hello"),
                    }
                });
            }
            Inbound::Connected {
                conn,
                send,
                recv,
                profile,
                presence,
                dialer,
            } => self.on_connected(conn, send, recv, profile, presence, dialer),
            Inbound::DialFailed { id } => {
                for m in self.pending.remove(&id).unwrap_or_default() {
                    let what = match m {
                        ContactMsg::Request { .. } => "contact request",
                        ContactMsg::Ring { .. } => "call",
                        ContactMsg::JoinRequest => "join request",
                        _ => continue,
                    };
                    let _ = self.events.send(ContactEvent::Undeliverable {
                        to: id,
                        what: what.into(),
                    });
                }
            }
            Inbound::Msg { from, conn_id, msg } => {
                if self
                    .peers
                    .get(&from)
                    .is_some_and(|p| p.conn.stable_id() == conn_id)
                {
                    self.on_msg(from, msg);
                }
            }
            Inbound::Down { id, conn_id } => {
                if self
                    .peers
                    .get(&id)
                    .is_some_and(|p| p.conn.stable_id() == conn_id)
                {
                    self.peers.remove(&id);
                    let _ = self.events.send(ContactEvent::Offline(id));
                }
            }
            Inbound::Cmd(cmd) => self.on_command(cmd),
        }
    }

    fn hello(&self, to: &EndpointId) -> ContactMsg {
        ContactMsg::Hello {
            profile: self.profile.clone(),
            presence: match self.relation(to) {
                Some(Relation::Mutual) => self.presence.clone(),
                _ => None,
            },
        }
    }

    fn on_connected(
        &mut self,
        conn: Connection,
        mut send: SendStream,
        recv: RecvStream,
        profile: WireProfile,
        presence: Option<PresenceInfo>,
        dialer: EndpointId,
    ) {
        let id = conn.remote_id();
        if self.relation(&id) == Some(Relation::Blocked) {
            conn.close(4u32.into(), b"blocked");
            return;
        }
        if let Some(existing) = self.peers.get(&id) {
            let winner = self.me.min(id);
            if existing.dialer == winner || dialer != winner {
                conn.close(3u32.into(), b"duplicate");
                return;
            }
            existing.conn.close(3u32.into(), b"duplicate");
        }
        let (tx, mut rx) = mpsc::unbounded_channel::<ContactMsg>();
        // The acceptor answers the dialer's Hello with its own.
        if dialer == id {
            let _ = tx.send(self.hello(&id));
        }
        for m in self.pending.remove(&id).unwrap_or_default() {
            let _ = tx.send(m);
        }
        tokio::spawn(async move {
            while let Some(msg) = rx.recv().await {
                if write_frame(&mut send, &msg).await.is_err() {
                    break;
                }
            }
        });
        let conn_id = conn.stable_id();
        let inbound = self.inbound.clone();
        let mut recv = recv;
        tokio::spawn(async move {
            while let Ok(msg) = read_frame::<ContactMsg>(&mut recv).await {
                if inbound
                    .send(Inbound::Msg {
                        from: id,
                        conn_id,
                        msg,
                    })
                    .is_err()
                {
                    return;
                }
            }
            let _ = inbound.send(Inbound::Down { id, conn_id });
        });
        let mutual = self.relation(&id) == Some(Relation::Mutual);
        self.peers.insert(
            id,
            Peer {
                conn,
                dialer,
                tx,
                profile: profile.clone(),
            },
        );
        if mutual || dialer == id {
            let _ = self.events.send(ContactEvent::Online {
                id,
                profile,
                presence: if mutual { presence } else { None },
            });
        }
    }

    fn on_msg(&mut self, from: EndpointId, msg: ContactMsg) {
        let rel = self.relation(&from);
        let mutual = rel == Some(Relation::Mutual);
        match msg {
            ContactMsg::Hello { profile, presence } => {
                // The acceptor's reply to our Hello.
                if let Some(p) = self.peers.get_mut(&from) {
                    p.profile = profile.clone();
                }
                let _ = self.events.send(ContactEvent::Online {
                    id: from,
                    profile,
                    presence: if mutual { presence } else { None },
                });
            }
            ContactMsg::Request { profile } => match rel {
                Some(Relation::Blocked) => {}
                // We'd already asked them: treat as acceptance.
                Some(Relation::Outgoing) => {
                    let _ = self
                        .events
                        .send(ContactEvent::Accepted { id: from, profile });
                }
                _ => {
                    let _ = self
                        .events
                        .send(ContactEvent::RequestReceived { id: from, profile });
                }
            },
            ContactMsg::Accept { profile } if rel == Some(Relation::Outgoing) => {
                let _ = self
                    .events
                    .send(ContactEvent::Accepted { id: from, profile });
            }
            ContactMsg::Remove if rel.is_some() && rel != Some(Relation::Blocked) => {
                let _ = self.events.send(ContactEvent::Removed(from));
            }
            ContactMsg::Presence(info) if mutual => {
                let _ = self.events.send(ContactEvent::Presence { id: from, info });
            }
            ContactMsg::Ring {
                call_id,
                link,
                members,
            } if mutual => {
                let _ = self.events.send(ContactEvent::Ring {
                    from,
                    call_id,
                    link,
                    members,
                });
            }
            ContactMsg::CancelRing { call_id } if mutual => {
                let _ = self
                    .events
                    .send(ContactEvent::RingCancelled { from, call_id });
            }
            ContactMsg::RingAnswer { call_id, accepted } if mutual => {
                let _ = self.events.send(ContactEvent::RingAnswered {
                    from,
                    call_id,
                    accepted,
                });
            }
            ContactMsg::JoinRequest if mutual => {
                let _ = self.events.send(ContactEvent::JoinRequested(from));
            }
            ContactMsg::JoinTicket { link } if mutual => {
                let _ = self.events.send(ContactEvent::JoinTicket { from, link });
            }
            other => debug!("ignoring {other:?} from {from} (relation {rel:?})"),
        }
    }

    /// Send to a peer, dialing first if we aren't connected.
    fn deliver(&mut self, to: EndpointId, msg: ContactMsg) {
        if let Some(p) = self.peers.get(&to) {
            let _ = p.tx.send(msg);
            return;
        }
        let first = !self.pending.contains_key(&to);
        self.pending.entry(to).or_default().push(msg);
        if first {
            self.dial(to);
        }
    }

    fn dial(&mut self, to: EndpointId) {
        let endpoint = self.endpoint.clone();
        let inbound = self.inbound.clone();
        let hello = self.hello(&to);
        let me = self.me;
        tokio::spawn(async move {
            let result: Result<(Connection, SendStream, RecvStream, ContactMsg)> = async {
                let conn = tokio::time::timeout(
                    Duration::from_secs(15),
                    endpoint.connect(EndpointAddr::new(to), CONTACTS_ALPN),
                )
                .await
                .context("timed out")?
                .map_err(|e| anyhow::anyhow!("connect: {e}"))?;
                let (mut send, mut recv) = conn.open_bi().await?;
                write_frame(&mut send, &hello).await?;
                let reply = tokio::time::timeout(Duration::from_secs(10), read_frame(&mut recv))
                    .await
                    .context("no hello reply")??;
                Ok((conn, send, recv, reply))
            }
            .await;
            match result {
                Ok((conn, send, recv, ContactMsg::Hello { profile, presence })) => {
                    let _ = inbound.send(Inbound::Connected {
                        conn,
                        send,
                        recv,
                        profile,
                        presence,
                        dialer: me,
                    });
                }
                _ => {
                    let _ = inbound.send(Inbound::DialFailed { id: to });
                }
            }
        });
    }

    fn connect_all(&mut self) {
        let targets: Vec<EndpointId> = self
            .relations
            .iter()
            .filter(|(id, r)| {
                **r == Relation::Mutual
                    && !self.peers.contains_key(*id)
                    && !self.pending.contains_key(*id)
            })
            .map(|(id, _)| *id)
            .collect();
        for id in targets {
            self.pending.insert(id, Vec::new());
            self.dial(id);
        }
    }

    fn on_command(&mut self, cmd: ContactCommand) {
        match cmd {
            ContactCommand::SetProfile(p) => self.profile = p,
            ContactCommand::SetPresence(p) => {
                self.presence = p.clone();
                if let Some(info) = p {
                    for (id, peer) in &self.peers {
                        if self.relation(id) == Some(Relation::Mutual) {
                            let _ = peer.tx.send(ContactMsg::Presence(info.clone()));
                        }
                    }
                }
            }
            ContactCommand::SetRelations(r) => {
                let newly_mutual: Vec<EndpointId> = self
                    .peers
                    .keys()
                    .filter(|id| {
                        r.get(*id) == Some(&Relation::Mutual)
                            && self.relation(id) != Some(Relation::Mutual)
                    })
                    .copied()
                    .collect();
                self.relations = r;
                // Peers we're already connected to that just became contacts:
                // report them online and share our status.
                for id in newly_mutual {
                    if let Some(peer) = self.peers.get(&id) {
                        if let Some(info) = self.presence.clone() {
                            let _ = peer.tx.send(ContactMsg::Presence(info));
                        }
                        let _ = self.events.send(ContactEvent::Online {
                            id,
                            profile: peer.profile.clone(),
                            presence: None,
                        });
                    }
                }
                // Drop connections to anyone now blocked.
                let blocked: Vec<EndpointId> = self
                    .peers
                    .keys()
                    .filter(|id| self.relation(id) == Some(Relation::Blocked))
                    .copied()
                    .collect();
                for id in blocked {
                    if let Some(p) = self.peers.remove(&id) {
                        p.conn.close(4u32.into(), b"blocked");
                    }
                }
            }
            ContactCommand::ConnectAll => self.connect_all(),
            ContactCommand::SendRequest(to) => {
                let profile = self.profile.clone();
                self.deliver(to, ContactMsg::Request { profile });
            }
            ContactCommand::Accept(to) => {
                let profile = self.profile.clone();
                self.deliver(to, ContactMsg::Accept { profile });
                if let Some(info) = self.presence.clone() {
                    self.deliver(to, ContactMsg::Presence(info));
                }
            }
            ContactCommand::Remove(to) => self.deliver(to, ContactMsg::Remove),
            ContactCommand::Ring {
                to,
                call_id,
                link,
                members,
            } => self.deliver(
                to,
                ContactMsg::Ring {
                    call_id,
                    link,
                    members,
                },
            ),
            ContactCommand::CancelRing { to, call_id } => {
                if let Some(p) = self.peers.get(&to) {
                    let _ = p.tx.send(ContactMsg::CancelRing { call_id });
                }
            }
            ContactCommand::Answer {
                to,
                call_id,
                accepted,
            } => self.deliver(to, ContactMsg::RingAnswer { call_id, accepted }),
            ContactCommand::JoinRequest(to) => self.deliver(to, ContactMsg::JoinRequest),
            ContactCommand::JoinReply { to, link } => {
                self.deliver(to, ContactMsg::JoinTicket { link })
            }
        }
    }
}
