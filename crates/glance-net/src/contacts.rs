//! The contacts actor: always running (not tied to a room). Keeps a
//! connection to each online mutual contact for presence and rings, and
//! handles contact requests from people you've been in a room with.
//!
//! Policy: blocked peers are refused at connect; peers that aren't contacts
//! may only send `Request` (and the initial `Hello`).
//!
//! Direct messages need a `glance/contacts/2` connection on both ends. The
//! actor doesn't keep them: the app persists its outbox and resends whatever
//! wasn't acked when the contact comes online (receivers dedupe by id).
//! It does track what's in flight: a message that isn't acked within
//! [`DM_ACK_TIMEOUT`] (or whose connection drops) is reported as failed, so
//! the app queues it again instead of showing "Sending…" forever.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use glance_proto::{
    CONTACTS_ALPN, CONTACTS_ALPN_V1, ContactMsg, EndpointAddr, EndpointId, MAX_DM_LEN,
    PresenceInfo, WireDm, WireProfile, truncate_utf8,
};
use iroh::Endpoint;
use iroh::endpoint::{ConnectOptions, Connection, RecvStream, SendStream};
use tokio::sync::mpsc;
use tracing::debug;

use crate::io::{read_frame, write_frame};

/// How long a sent direct message may wait for its ack before we report it
/// as failed (the receiver may have dropped it, e.g. because it didn't see
/// us as a contact yet).
pub const DM_ACK_TIMEOUT: Duration = Duration::from_secs(15);
/// How often unacked messages are checked (only while there are any).
const DM_CHECK_EVERY: Duration = Duration::from_secs(5);

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
    /// Send a file (already in our store) to a mutual contact.
    SendFile {
        to: EndpointId,
        file: glance_proto::WireFile,
    },
    /// Send a direct message to a mutual contact (dialing them if needed).
    /// Answered by [`ContactEvent::DmAcked`] or [`ContactEvent::DmFailed`].
    SendDm {
        to: EndpointId,
        dm: WireDm,
    },
}

/// Why a direct message didn't go out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DmFailure {
    /// It was sent but no ack came back (the connection dropped or the ack
    /// timed out): it may have arrived. Resend when they come online.
    Offline,
    /// We couldn't reach them, so it was never sent. Resend when they come
    /// online.
    Unreachable,
    /// Their glance predates direct messages (`glance/contacts/1`).
    Unsupported,
    /// They aren't a mutual contact.
    NotContact,
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
    /// A mutual contact sent us a file.
    FileReceived {
        from: EndpointId,
        file: glance_proto::WireFile,
    },
    /// A request or ring couldn't be delivered (peer unreachable).
    Undeliverable {
        to: EndpointId,
        what: String,
    },
    /// A mutual contact sent us a direct message (already acked; the same
    /// id may arrive again if our ack got lost).
    DmReceived {
        from: EndpointId,
        dm: WireDm,
    },
    /// A contact confirmed they got our message.
    DmAcked {
        from: EndpointId,
        id: u64,
    },
    /// Our message couldn't be sent now.
    DmFailed {
        to: EndpointId,
        id: u64,
        reason: DmFailure,
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
    /// The connection speaks `glance/contacts/2` (direct messages).
    dms: bool,
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
    /// Direct messages sent but not acked yet, by (contact, message id).
    unacked: HashMap<(EndpointId, u64), Instant>,
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
        unacked: HashMap::new(),
        inbound: in_tx.clone(),
        events: ev_tx,
    };
    tokio::spawn(async move {
        let mut retry = tokio::time::interval(Duration::from_secs(30));
        retry.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        // Only polled while messages wait for an ack.
        let mut dm_check = tokio::time::interval(DM_CHECK_EVERY);
        dm_check.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                msg = in_rx.recv_async() => {
                    let Ok(msg) = msg else { break };
                    actor.handle(msg);
                }
                _ = retry.tick() => actor.connect_all(),
                _ = dm_check.tick(), if !actor.unacked.is_empty() => actor.expire_dms(),
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
                        ContactMsg::Dm(dm) => {
                            self.dm_failed(id, dm.id, DmFailure::Unreachable);
                            continue;
                        }
                        ContactMsg::Request { .. } => "contact request",
                        ContactMsg::Ring { .. } => "call",
                        ContactMsg::JoinRequest => "join request",
                        ContactMsg::File(_) => "file",
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
                    // Whatever we sent on that connection without an ack may
                    // never have arrived.
                    let lost: Vec<u64> = self
                        .unacked
                        .keys()
                        .filter(|(to, _)| *to == id)
                        .map(|(_, dm)| *dm)
                        .collect();
                    for dm in lost {
                        self.dm_failed(id, dm, DmFailure::Offline);
                    }
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
        let dms = conn.alpn() == CONTACTS_ALPN;
        for m in self.pending.remove(&id).unwrap_or_default() {
            match m {
                ContactMsg::Dm(dm) if !dms => {
                    self.dm_failed(id, dm.id, DmFailure::Unsupported);
                }
                m => {
                    let _ = tx.send(m);
                }
            }
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
                dms,
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
                // Mutual from this moment, so whatever they send right behind
                // the acceptance (their status, a file) isn't dropped while
                // the app catches up with SetRelations.
                self.relations.insert(from, Relation::Mutual);
                let _ = self
                    .events
                    .send(ContactEvent::Accepted { id: from, profile });
                if let Some(peer) = self.peers.get(&from) {
                    if let Some(info) = self.presence.clone() {
                        let _ = peer.tx.send(ContactMsg::Presence(info));
                    }
                    let _ = self.events.send(ContactEvent::Online {
                        id: from,
                        profile: peer.profile.clone(),
                        presence: None,
                    });
                }
            }
            ContactMsg::Remove if rel.is_some() && rel != Some(Relation::Blocked) => {
                let _ = self.events.send(ContactEvent::Removed(from));
            }
            ContactMsg::Presence(info) if mutual => {
                let _ = self.events.send(ContactEvent::Presence { id: from, info });
            }
            ContactMsg::File(file) if mutual => {
                let _ = self.events.send(ContactEvent::FileReceived { from, file });
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
            ContactMsg::Dm(dm) if mutual && dm.text.len() <= MAX_DM_LEN => {
                if let Some(p) = self.peers.get(&from) {
                    let _ = p.tx.send(ContactMsg::DmAck { id: dm.id });
                }
                let _ = self.events.send(ContactEvent::DmReceived { from, dm });
            }
            ContactMsg::DmAck { id } if mutual => {
                self.unacked.remove(&(from, id));
                let _ = self.events.send(ContactEvent::DmAcked { from, id });
            }
            other => debug!("ignoring {other:?} from {from} (relation {rel:?})"),
        }
    }

    /// Report a direct message as not sent (and stop waiting for its ack).
    fn dm_failed(&mut self, to: EndpointId, id: u64, reason: DmFailure) {
        self.unacked.remove(&(to, id));
        let _ = self.events.send(ContactEvent::DmFailed { to, id, reason });
    }

    /// Give up on direct messages that weren't acked in time: the app queues
    /// them again and resends when the contact next comes online.
    fn expire_dms(&mut self) {
        let now = Instant::now();
        let late: Vec<(EndpointId, u64)> = self
            .unacked
            .iter()
            .filter(|(_, sent)| now.duration_since(**sent) >= DM_ACK_TIMEOUT)
            .map(|(key, _)| *key)
            .collect();
        for (to, id) in late {
            self.dm_failed(to, id, DmFailure::Offline);
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
                // Offer both versions; the acceptor picks the newest it has.
                let conn = tokio::time::timeout(Duration::from_secs(15), async {
                    endpoint
                        .connect_with_opts(
                            EndpointAddr::new(to),
                            CONTACTS_ALPN,
                            ConnectOptions::new()
                                .with_additional_alpns(vec![CONTACTS_ALPN_V1.to_vec()]),
                        )
                        .await
                        .map_err(|e| anyhow::anyhow!("connect: {e}"))?
                        .await
                        .map_err(|e| anyhow::anyhow!("connect: {e}"))
                })
                .await
                .context("timed out")??;
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
                // report them online and share our status. The Hello reads
                // as "online" on their side too, so they resend any message
                // we dropped while they weren't a contact yet.
                for id in newly_mutual {
                    let hello = self.hello(&id);
                    if let Some(peer) = self.peers.get(&id) {
                        let _ = peer.tx.send(hello);
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
            ContactCommand::SendFile { to, file } => self.deliver(to, ContactMsg::File(file)),
            ContactCommand::SendDm { to, mut dm } => {
                let fail = |reason| ContactEvent::DmFailed {
                    to,
                    id: dm.id,
                    reason,
                };
                if self.relation(&to) != Some(Relation::Mutual) {
                    let _ = self.events.send(fail(DmFailure::NotContact));
                    return;
                }
                match self.peers.get(&to) {
                    Some(p) if !p.dms => {
                        let _ = self.events.send(fail(DmFailure::Unsupported));
                    }
                    _ => {
                        truncate_utf8(&mut dm.text, MAX_DM_LEN);
                        self.unacked.insert((to, dm.id), Instant::now());
                        self.deliver(to, ContactMsg::Dm(dm));
                    }
                }
            }
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
