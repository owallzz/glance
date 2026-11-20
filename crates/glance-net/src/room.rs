//! The room actor: one task per room owning membership, plus a reader and a
//! writer task per peer connection.
//!
//! Joining: the newcomer dials the ticket's host, proves it knows the room
//! secret, and gets back the member list in `Welcome`; it then dials every
//! member it doesn't know. Existing members tell each other about newcomers
//! with `Introduce` so simultaneous joins still end up fully meshed. If two
//! connections to the same peer exist, the one dialed by the smaller
//! endpoint id wins.

use std::collections::HashMap;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use glance_proto::{
    Control, EndpointAddr, EndpointId, Hello, MemberInfo, MemberState, ROOM_ALPN, Ticket,
    WireProfile, join_proof, verify_proof,
};
use iroh::Endpoint;
use iroh::endpoint::{Connection, RecvStream, SendStream};
use tokio::sync::mpsc;
use tracing::{debug, warn};

use crate::RoomSlot;
use crate::io::{read_frame, write_frame};

/// What the UI asks the room to do.
#[derive(Debug, Clone)]
pub enum Command {
    SetState(MemberState),
    SetProfile(WireProfile),
    Chat(String),
    Leave,
}

/// What happened in the room.
#[derive(Debug, Clone, PartialEq)]
pub enum RoomEvent {
    MemberJoined {
        id: EndpointId,
        profile: WireProfile,
        state: MemberState,
    },
    MemberUpdated {
        id: EndpointId,
        state: MemberState,
    },
    MemberProfile {
        id: EndpointId,
        profile: WireProfile,
    },
    MemberLeft {
        id: EndpointId,
    },
    Chat {
        from: EndpointId,
        sent_at: u64,
        text: String,
    },
    Connection {
        id: EndpointId,
        relayed: bool,
        rtt_ms: u32,
    },
    /// Something the user should know (couldn't reach the host, rejected...).
    Problem(String),
    /// The room actor stopped (after Leave).
    Closed,
}

/// Handle to a running room.
pub struct RoomHandle {
    pub ticket: Ticket,
    pub me: EndpointId,
    commands: flume::Sender<Command>,
    pub events: flume::Receiver<RoomEvent>,
}

impl RoomHandle {
    pub fn link(&self) -> String {
        self.ticket.to_link()
    }

    pub fn send(&self, cmd: Command) {
        let _ = self.commands.send(cmd);
    }
}

impl Drop for RoomHandle {
    fn drop(&mut self) {
        let _ = self.commands.send(Command::Leave);
    }
}

/// Messages into the actor.
pub(crate) enum Inbound {
    /// A connection accepted by the protocol handler.
    Incoming(Connection),
    /// Accept side: the dialer's Hello arrived.
    Hello {
        conn: Connection,
        send: SendStream,
        recv: RecvStream,
        hello: Hello,
    },
    /// Dial side: our Hello was accepted.
    Welcomed {
        conn: Connection,
        send: SendStream,
        recv: RecvStream,
        addr: EndpointAddr,
        profile: WireProfile,
        state: MemberState,
        members: Vec<MemberInfo>,
    },
    DialFailed {
        id: EndpointId,
        error: String,
        host: bool,
    },
    Frame {
        from: EndpointId,
        conn_id: usize,
        msg: Control,
    },
    PeerDown {
        id: EndpointId,
        conn_id: usize,
    },
    Cmd(Command),
}

struct Peer {
    profile: WireProfile,
    state: MemberState,
    addr: EndpointAddr,
    conn: Connection,
    dialer: EndpointId,
    tx: mpsc::UnboundedSender<Control>,
}

struct Actor {
    endpoint: Endpoint,
    me: EndpointId,
    ticket: Ticket,
    profile: WireProfile,
    state: MemberState,
    peers: HashMap<EndpointId, Peer>,
    dialing: std::collections::HashSet<EndpointId>,
    inbound: flume::Sender<Inbound>,
    events: flume::Sender<RoomEvent>,
    chat_seq: u64,
}

pub(crate) fn spawn(
    endpoint: Endpoint,
    ticket: Ticket,
    profile: WireProfile,
    state: MemberState,
    dial_host: bool,
    slot: RoomSlot,
) -> (RoomHandle, flume::Sender<Inbound>) {
    let (inbound_tx, inbound_rx) = flume::unbounded();
    let (cmd_tx, cmd_rx) = flume::unbounded();
    let (ev_tx, ev_rx) = flume::unbounded();
    let me = endpoint.id();
    let handle_ticket = ticket.clone();
    let mut actor = Actor {
        endpoint,
        me,
        ticket: ticket.clone(),
        profile,
        state,
        peers: HashMap::new(),
        dialing: Default::default(),
        inbound: inbound_tx.clone(),
        events: ev_tx,
        chat_seq: 0,
    };
    let fwd = inbound_tx.clone();
    tokio::spawn(async move {
        while let Ok(cmd) = cmd_rx.recv_async().await {
            if fwd.send(Inbound::Cmd(cmd)).is_err() {
                break;
            }
        }
    });
    tokio::spawn(async move {
        if dial_host && ticket.host.id != me {
            actor.dial(ticket.host.clone(), true);
        }
        actor.run(inbound_rx).await;
        // Free the slot if it still points at us.
        let mut s = slot.lock().expect("room slot");
        if s.as_ref().is_some_and(|tx| tx.same_channel(&actor.inbound)) {
            *s = None;
        }
    });
    let handle = RoomHandle {
        ticket: handle_ticket,
        me,
        commands: cmd_tx,
        events: ev_rx,
    };
    (handle, inbound_tx)
}

impl Actor {
    async fn run(&mut self, inbound: flume::Receiver<Inbound>) {
        let mut stats = tokio::time::interval(Duration::from_secs(2));
        stats.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                msg = inbound.recv_async() => {
                    let Ok(msg) = msg else { break };
                    if !self.handle(msg) {
                        break;
                    }
                }
                _ = stats.tick() => self.report_connections(),
            }
        }
        let _ = self.events.send(RoomEvent::Closed);
    }

    /// Returns false when the room should stop.
    fn handle(&mut self, msg: Inbound) -> bool {
        match msg {
            Inbound::Incoming(conn) => {
                let inbound = self.inbound.clone();
                tokio::spawn(async move {
                    match accept_hello(&conn).await {
                        Ok((send, recv, hello)) => {
                            let _ = inbound.send(Inbound::Hello {
                                conn,
                                send,
                                recv,
                                hello,
                            });
                        }
                        Err(e) => {
                            debug!("bad incoming room connection: {e:#}");
                            conn.close(2u32.into(), b"bad hello");
                        }
                    }
                });
            }
            Inbound::Hello {
                conn,
                send,
                recv,
                hello,
            } => self.on_hello(conn, send, recv, hello),
            Inbound::Welcomed {
                conn,
                send,
                recv,
                addr,
                profile,
                state,
                members,
            } => {
                let id = conn.remote_id();
                self.dialing.remove(&id);
                self.register(conn, send, recv, addr, profile, state, self.me, None);
                for m in members {
                    if m.addr.id != self.me && !self.peers.contains_key(&m.addr.id) {
                        self.dial(m.addr, false);
                    }
                }
            }
            Inbound::DialFailed { id, error, host } => {
                self.dialing.remove(&id);
                warn!("dial {id} failed: {error}");
                if host && self.peers.is_empty() {
                    let _ = self.events.send(RoomEvent::Problem(format!(
                        "Couldn't reach the room. The person who shared the link may have left. ({error})"
                    )));
                }
            }
            Inbound::Frame { from, conn_id, msg } => self.on_frame(from, conn_id, msg),
            Inbound::PeerDown { id, conn_id } => {
                if self
                    .peers
                    .get(&id)
                    .is_some_and(|p| p.conn.stable_id() == conn_id)
                {
                    self.peers.remove(&id);
                    let _ = self.events.send(RoomEvent::MemberLeft { id });
                }
            }
            Inbound::Cmd(cmd) => return self.on_command(cmd),
        }
        true
    }

    fn on_hello(&mut self, conn: Connection, mut send: SendStream, recv: RecvStream, hello: Hello) {
        let from = conn.remote_id();
        if hello.room != self.ticket.room
            || !verify_proof(&self.ticket.secret, hello.room, from, self.me, &hello.proof)
        {
            tokio::spawn(async move {
                let _ = write_frame(&mut send, &Control::Rejected).await;
                let _ = send.finish();
                conn.closed().await;
            });
            return;
        }
        let members = self
            .peers
            .iter()
            .filter(|(id, _)| **id != from)
            .map(|(_, p)| MemberInfo {
                addr: p.addr.clone(),
                profile: p.profile.clone(),
            })
            .collect();
        let welcome = Control::Welcome {
            profile: self.profile.clone(),
            state: self.state.clone(),
            members,
        };
        let mut addr = hello.addr.clone();
        addr.id = from;
        self.register(
            conn,
            send,
            recv,
            addr,
            hello.profile,
            hello.state,
            from,
            Some(welcome),
        );
    }

    /// Add a connected peer (both sides end up here).
    #[allow(clippy::too_many_arguments)]
    fn register(
        &mut self,
        conn: Connection,
        send: SendStream,
        recv: RecvStream,
        addr: EndpointAddr,
        profile: WireProfile,
        state: MemberState,
        dialer: EndpointId,
        first: Option<Control>,
    ) {
        let id = conn.remote_id();
        if let Some(existing) = self.peers.get(&id) {
            // Keep the connection dialed by the smaller id.
            let winner = self.me.min(id);
            if existing.dialer == winner || dialer != winner {
                // Still answer so the other side isn't left hanging, then drop it.
                if let Some(first) = first {
                    spawn_one_shot(send, first);
                }
                return;
            }
            existing.conn.close(3u32.into(), b"duplicate");
        }
        let (tx, rx) = mpsc::unbounded_channel();
        if let Some(first) = first {
            let _ = tx.send(first);
        }
        spawn_writer(send, rx);
        spawn_reader(recv, id, conn.stable_id(), self.inbound.clone());
        let is_new = !self.peers.contains_key(&id);
        // Tell everyone else about the newcomer.
        let info = MemberInfo {
            addr: addr.clone(),
            profile: profile.clone(),
        };
        for p in self.peers.values().filter(|_| is_new) {
            let _ = p.tx.send(Control::Introduce(info.clone()));
        }
        self.peers.insert(
            id,
            Peer {
                profile: profile.clone(),
                state: state.clone(),
                addr,
                conn,
                dialer,
                tx,
            },
        );
        if is_new {
            let _ = self
                .events
                .send(RoomEvent::MemberJoined { id, profile, state });
        }
    }

    fn on_frame(&mut self, from: EndpointId, conn_id: usize, msg: Control) {
        let Some(peer) = self.peers.get_mut(&from) else {
            return;
        };
        if peer.conn.stable_id() != conn_id {
            return; // stale connection
        }
        match msg {
            Control::State(state) => {
                peer.state = state.clone();
                let _ = self
                    .events
                    .send(RoomEvent::MemberUpdated { id: from, state });
            }
            Control::Profile(profile) => {
                peer.profile = profile.clone();
                let _ = self
                    .events
                    .send(RoomEvent::MemberProfile { id: from, profile });
            }
            Control::Chat { sent_at, text, .. } => {
                let _ = self.events.send(RoomEvent::Chat {
                    from,
                    sent_at,
                    text,
                });
            }
            Control::Introduce(info) => {
                let id = info.addr.id;
                if id != self.me && !self.peers.contains_key(&id) && self.me < id {
                    self.dial(info.addr, false);
                }
            }
            Control::Leave => {
                if let Some(p) = self.peers.remove(&from) {
                    p.conn.close(0u32.into(), b"left");
                }
                let _ = self.events.send(RoomEvent::MemberLeft { id: from });
            }
            Control::Ping(n) => {
                let _ = peer.tx.send(Control::Pong(n));
            }
            Control::Pong(_) | Control::Welcome { .. } | Control::Rejected => {}
        }
    }

    fn on_command(&mut self, cmd: Command) -> bool {
        match cmd {
            Command::SetState(state) => {
                self.state = state.clone();
                self.broadcast(Control::State(state));
            }
            Command::SetProfile(profile) => {
                self.profile = profile.clone();
                self.broadcast(Control::Profile(profile));
            }
            Command::Chat(text) => {
                self.chat_seq += 1;
                self.broadcast(Control::Chat {
                    id: self.chat_seq,
                    sent_at: now_ms(),
                    text,
                });
            }
            Command::Leave => {
                self.broadcast(Control::Leave);
                // Writers flush Leave and finish; close shortly after.
                let conns: Vec<Connection> = self.peers.drain().map(|(_, p)| p.conn).collect();
                tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_millis(300)).await;
                    for c in conns {
                        c.close(0u32.into(), b"left");
                    }
                });
                return false;
            }
        }
        true
    }

    fn broadcast(&self, msg: Control) {
        for p in self.peers.values() {
            let _ = p.tx.send(msg.clone());
        }
    }

    fn dial(&mut self, addr: EndpointAddr, host: bool) {
        let id = addr.id;
        if !self.dialing.insert(id) {
            return;
        }
        let endpoint = self.endpoint.clone();
        let inbound = self.inbound.clone();
        let hello = Hello {
            room: self.ticket.room,
            proof: join_proof(&self.ticket.secret, self.ticket.room, self.me, id),
            profile: self.profile.clone(),
            state: self.state.clone(),
            addr: self.endpoint.addr(),
        };
        tokio::spawn(async move {
            match dial_and_hello(&endpoint, addr.clone(), hello).await {
                Ok((conn, send, recv, profile, state, members)) => {
                    let _ = inbound.send(Inbound::Welcomed {
                        conn,
                        send,
                        recv,
                        addr,
                        profile,
                        state,
                        members,
                    });
                }
                Err(e) => {
                    let _ = inbound.send(Inbound::DialFailed {
                        id,
                        error: format!("{e:#}"),
                        host,
                    });
                }
            }
        });
    }

    fn report_connections(&self) {
        for (id, p) in &self.peers {
            let paths = p.conn.paths();
            if let Some(path) = paths.iter().find(|path| path.is_selected()) {
                let _ = self.events.send(RoomEvent::Connection {
                    id: *id,
                    relayed: path.is_relay(),
                    rtt_ms: path.rtt().as_millis().min(u32::MAX as u128) as u32,
                });
            }
        }
    }
}

async fn accept_hello(conn: &Connection) -> Result<(SendStream, RecvStream, Hello)> {
    let (send, mut recv) = tokio::time::timeout(Duration::from_secs(10), conn.accept_bi())
        .await
        .context("hello timeout")??;
    let hello: Hello = tokio::time::timeout(Duration::from_secs(10), read_frame(&mut recv))
        .await
        .context("hello timeout")??;
    Ok((send, recv, hello))
}

type Dialed = (
    Connection,
    SendStream,
    RecvStream,
    WireProfile,
    MemberState,
    Vec<MemberInfo>,
);

async fn dial_and_hello(endpoint: &Endpoint, addr: EndpointAddr, hello: Hello) -> Result<Dialed> {
    let conn = tokio::time::timeout(Duration::from_secs(20), endpoint.connect(addr, ROOM_ALPN))
        .await
        .context("timed out connecting")?
        .map_err(|e| anyhow::anyhow!("connect: {e}"))?;
    let (mut send, mut recv) = conn.open_bi().await.context("open control stream")?;
    write_frame(&mut send, &hello).await?;
    let reply: Control = tokio::time::timeout(Duration::from_secs(10), read_frame(&mut recv))
        .await
        .context("timed out waiting for welcome")??;
    match reply {
        Control::Welcome {
            profile,
            state,
            members,
        } => Ok((conn, send, recv, profile, state, members)),
        Control::Rejected => bail!("the room rejected this link"),
        other => bail!("unexpected reply {other:?}"),
    }
}

fn spawn_writer(mut send: SendStream, mut rx: mpsc::UnboundedReceiver<Control>) {
    tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            let leaving = matches!(msg, Control::Leave);
            if write_frame(&mut send, &msg).await.is_err() {
                break;
            }
            if leaving {
                let _ = send.finish();
                break;
            }
        }
    });
}

fn spawn_one_shot(mut send: SendStream, msg: Control) {
    tokio::spawn(async move {
        let _ = write_frame(&mut send, &msg).await;
        let _ = send.finish();
    });
}

fn spawn_reader(
    mut recv: RecvStream,
    from: EndpointId,
    conn_id: usize,
    inbound: flume::Sender<Inbound>,
) {
    tokio::spawn(async move {
        loop {
            match read_frame::<Control>(&mut recv).await {
                Ok(msg) => {
                    if inbound.send(Inbound::Frame { from, conn_id, msg }).is_err() {
                        return;
                    }
                }
                Err(_) => {
                    let _ = inbound.send(Inbound::PeerDown { id: from, conn_id });
                    return;
                }
            }
        }
    });
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
