//! The room actor: one task per room owning membership, plus a reader and a
//! writer task per peer connection.
//!
//! Joining: the newcomer dials the ticket's host (or, when rejoining, any
//! member a probe found still in the room; the next one if that fails),
//! proves it knows the room secret, and gets back the member list in
//! `Welcome`; it then dials every
//! member it doesn't know. Existing members tell each other about newcomers
//! with `Introduce` so simultaneous joins still end up fully meshed. If two
//! connections to the same peer exist, the one dialed by the smaller
//! endpoint id wins.
//!
//! Merging: two copies of the same room (two people restarted it at once)
//! join up when one learns of the other's members (`Inbound::Merge`). It
//! connects to them under the same rule, and each side's `Introduce` and
//! `Welcome` spread the other mesh's members until everyone is connected.

use std::collections::HashMap;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use std::sync::Arc;

use glance_proto::{
    Control, EndpointAddr, EndpointId, HISTORY_LIMIT, Hello, HistoryBody, HistoryItem, MemberInfo,
    MemberState, ROOM_ALPN, Ticket, Track, UNI_VIDEO, VideoPacket, WireProfile, join_proof,
    verify_proof,
};
use iroh::Endpoint;
use iroh::endpoint::{Connection, RecvStream, SendStream};
use tokio::sync::mpsc;
use tracing::{debug, warn};

use crate::RoomSlot;
use crate::io::{read_frame, read_video, write_frame, write_video};

/// What the UI asks the room to do.
#[derive(Debug, Clone)]
pub enum Command {
    SetState(MemberState),
    SetProfile(WireProfile),
    Chat(String),
    Leave,
    /// Ask a sharer to start/stop sending a track (screen or camera).
    Watch(EndpointId, Track),
    Unwatch(EndpointId, Track),
    /// Ask a sharer for a keyframe on a track (decoder lost sync).
    RequestKeyframe(EndpointId, Track),
    /// An encoded frame of one of our tracks, fanned out to its watchers.
    Video(Track, Arc<VideoPacket>),
    /// A voice frame (already postcard-encoded `AudioDatagram`), sent to
    /// everyone as a datagram.
    Audio(bytes::Bytes),
    /// A frame of the audio shared with our screen (encoded `AudioDatagram`
    /// with `stream: true`), sent only to people watching it.
    StreamAudio(bytes::Bytes),
    /// Offer a file (already added to our store) to the room.
    OfferFile(glance_proto::WireFile),
    /// Send a chat message or file offer stamped with `sent_at` (Unix ms),
    /// so the sender's own copy matches what everyone else stores.
    Post {
        sent_at: u64,
        body: HistoryBody,
    },
    /// Tell the room we now have a file and can serve it.
    HaveFile([u8; 32]),
    /// Where incoming voice datagrams go (bypasses the UI event channel).
    SetAudioSink(Option<flume::Sender<(EndpointId, glance_proto::AudioDatagram)>>),
}

/// Shared slot for the voice sink, read by every connection's datagram task.
type AudioSink =
    Arc<std::sync::Mutex<Option<flume::Sender<(EndpointId, glance_proto::AudioDatagram)>>>>;

/// Incoming video packets from one sharer. Compares by channel identity.
#[derive(Clone, Debug)]
pub struct VideoFeed(pub flume::Receiver<VideoPacket>);

impl PartialEq for VideoFeed {
    fn eq(&self, other: &Self) -> bool {
        self.0.same_channel(&other.0)
    }
}

/// What happened in the room.
#[derive(Debug, Clone, PartialEq)]
pub enum RoomEvent {
    MemberJoined {
        id: EndpointId,
        profile: WireProfile,
        state: MemberState,
        /// Where they can be reached (remembered for rejoining).
        addr: EndpointAddr,
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
    /// A sharer started sending us video (after we asked to watch).
    Video {
        from: EndpointId,
        track: Track,
        feed: VideoFeed,
    },
    /// How many people are watching one of our tracks.
    Watchers {
        track: Track,
        count: usize,
    },
    /// Someone offered a file in chat.
    FileOffered {
        from: EndpointId,
        sent_at: u64,
        file: glance_proto::WireFile,
    },
    /// Chat and files from before we joined (oldest first).
    History(Vec<HistoryItem>),
    /// Another peer can now serve this file.
    FileProvider {
        from: EndpointId,
        hash: [u8; 32],
    },
    /// A track's encoder should produce a keyframe soon (new or lagging viewer).
    KeyframeNeeded(Track),
    /// A viewer couldn't keep up with a track (lower its bitrate).
    Congested(Track),
    /// The room actor stopped (after Leave).
    Closed,
}

/// Handle to a running room.
pub struct RoomHandle {
    pub ticket: Ticket,
    pub me: EndpointId,
    endpoint: Endpoint,
    commands: flume::Sender<Command>,
    pub events: flume::Receiver<RoomEvent>,
}

impl RoomHandle {
    /// An invite link with *us* as the host, so it keeps working after
    /// whoever created the room leaves.
    pub fn link(&self) -> String {
        Ticket {
            host: crate::invite_addr(self.endpoint.addr()),
            ..self.ticket.clone()
        }
        .to_link()
    }

    pub fn send(&self, cmd: Command) {
        let _ = self.commands.send(cmd);
    }

    /// A cloneable, thread-safe sender for commands (encoder and decoder
    /// threads use it to push video and keyframe requests).
    pub fn commander(&self) -> flume::Sender<Command> {
        self.commands.clone()
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
        history: Vec<HistoryItem>,
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
    /// Fallback from `expect_dial`.
    DialIfMissing(EndpointAddr),
    /// Members of another copy of this room (it was restarted twice at
    /// once): connect to them so the two meshes become one.
    Merge(Vec<EndpointAddr>),
    /// A viewer's video stream ended (generation identifies which one).
    WatcherGone(EndpointId, Track, u64),
    /// Someone holding the link asks whether the room is alive.
    Probe {
        from: EndpointId,
        request: glance_proto::ProbeRequest,
        reply: tokio::sync::oneshot::Sender<glance_proto::ProbeReply>,
    },
}

/// Someone watching our stream.
struct Watcher {
    tx: mpsc::Sender<Arc<VideoPacket>>,
    /// Distinguishes successive streams to the same viewer.
    generation: u64,
    /// Drop frames until the next keyframe (just joined or fell behind).
    need_key: bool,
}

/// Frames buffered per viewer before we start skipping to keyframes.
const WATCHER_QUEUE: usize = 90;

struct Peer {
    /// A connection this one replaced (duplicate dial); its in-flight
    /// frames are still accepted until it closes.
    previous_conn: Option<usize>,
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
    watchers: HashMap<(EndpointId, Track), Watcher>,
    watcher_generation: u64,
    audio_sink: AudioSink,
    history: std::collections::VecDeque<HistoryItem>,
    /// Only the first Welcome's history is used (others would duplicate it).
    got_history: bool,
    /// Members still to try if the one we dialed to get in fails.
    entry: std::collections::VecDeque<EndpointAddr>,
}

pub(crate) fn spawn(
    endpoint: Endpoint,
    ticket: Ticket,
    profile: WireProfile,
    state: MemberState,
    entry: Vec<EndpointAddr>,
    slot: RoomSlot,
) -> (RoomHandle, flume::Sender<Inbound>) {
    let (inbound_tx, inbound_rx) = flume::unbounded();
    let (cmd_tx, cmd_rx) = flume::unbounded();
    let (ev_tx, ev_rx) = flume::unbounded();
    let me = endpoint.id();
    let handle_ticket = ticket.clone();
    let endpoint_for_handle = endpoint.clone();
    let mut actor = Actor {
        endpoint,
        me,
        ticket,
        profile,
        state,
        peers: HashMap::new(),
        dialing: Default::default(),
        inbound: inbound_tx.clone(),
        events: ev_tx,
        chat_seq: 0,
        watchers: HashMap::new(),
        watcher_generation: 0,
        audio_sink: AudioSink::default(),
        history: Default::default(),
        got_history: false,
        entry: entry.into_iter().filter(|a| a.id != me).collect(),
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
        if let Some(first) = actor.entry.pop_front() {
            actor.dial(first, true);
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
        endpoint: endpoint_for_handle,
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
                history,
            } => {
                let id = conn.remote_id();
                if !self.got_history {
                    self.got_history = true;
                    if !history.is_empty() {
                        self.history.extend(history.iter().cloned());
                        let _ = self.events.send(RoomEvent::History(history));
                    }
                }
                self.dialing.remove(&id);
                self.register(conn, send, recv, addr, profile, state, self.me, None);
                // Avoid duplicate connections: only the smaller id dials. The
                // bigger side is dialed when its peer hears about us (the
                // host sends Introduce); `expect` falls back after a delay.
                for m in members {
                    self.meet(m.addr);
                }
            }
            Inbound::DialFailed { id, error, host } => {
                self.dialing.remove(&id);
                warn!("dial {id} failed: {error}");
                if host && self.peers.is_empty() {
                    // Try the next member we know of.
                    while let Some(next) = self.entry.pop_front() {
                        if !self.dialing.contains(&next.id) {
                            self.dial(next, true);
                            return true;
                        }
                    }
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
                    self.remove_peer_watchers(id);
                    let _ = self.events.send(RoomEvent::MemberLeft { id });
                }
            }
            Inbound::WatcherGone(id, track, generation) => {
                // Only if it's still the same stream (a re-watch may have
                // replaced it already).
                if self
                    .watchers
                    .get(&(id, track))
                    .is_some_and(|w| w.generation == generation)
                {
                    self.remove_watcher(id, track);
                }
            }
            Inbound::Merge(members) => {
                for addr in members {
                    self.meet(addr);
                }
            }
            Inbound::DialIfMissing(addr) => {
                if !self.peers.contains_key(&addr.id) {
                    self.dial(addr, false);
                }
            }
            Inbound::Probe {
                from,
                request,
                reply,
            } => {
                let answer = crate::probe::reply_for(&self.ticket, self.me, from, &request, || {
                    let mut members = vec![MemberInfo {
                        addr: self.endpoint.addr(),
                        profile: self.profile.clone(),
                    }];
                    members.extend(self.peers.values().map(|p| MemberInfo {
                        addr: p.addr.clone(),
                        profile: p.profile.clone(),
                    }));
                    members
                });
                let _ = reply.send(answer);
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
            history: self.history.iter().cloned().collect(),
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
        let mut previous_conn = None;
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
            // Replace gracefully: messages already sent on the old
            // connection still get read; close it a little later.
            previous_conn = Some(existing.conn.stable_id());
            let old = existing.conn.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_secs(5)).await;
                old.close(3u32.into(), b"duplicate");
            });
        }
        let (tx, rx) = mpsc::unbounded_channel();
        if let Some(first) = first {
            let _ = tx.send(first);
        }
        spawn_writer(send, rx);
        spawn_reader(recv, id, conn.stable_id(), self.inbound.clone());
        spawn_uni_acceptor(conn.clone(), id, self.inbound.clone(), self.events.clone());
        spawn_datagram_reader(conn.clone(), id, self.audio_sink.clone());
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
                previous_conn,
                profile: profile.clone(),
                state: state.clone(),
                addr,
                conn,
                dialer,
                tx,
            },
        );
        if is_new {
            let _ = self.events.send(RoomEvent::MemberJoined {
                id,
                profile,
                state,
                addr: info.addr,
            });
        }
    }

    fn on_frame(&mut self, from: EndpointId, conn_id: usize, msg: Control) {
        let Some(peer) = self.peers.get_mut(&from) else {
            return;
        };
        if peer.conn.stable_id() != conn_id && peer.previous_conn != Some(conn_id) {
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
                let (name, color) = (peer.profile.name.clone(), peer.profile.color);
                self.remember(from, name, color, sent_at, HistoryBody::Text(text.clone()));
                let _ = self.events.send(RoomEvent::Chat {
                    from,
                    sent_at,
                    text,
                });
            }
            Control::Introduce(info) => self.meet(info.addr),
            Control::Leave => {
                if let Some(p) = self.peers.remove(&from) {
                    p.conn.close(0u32.into(), b"left");
                }
                self.remove_peer_watchers(from);
                let _ = self.events.send(RoomEvent::MemberLeft { id: from });
            }
            Control::File { sent_at, file } => {
                let (name, color) = (peer.profile.name.clone(), peer.profile.color);
                self.remember(from, name, color, sent_at, HistoryBody::File(file.clone()));
                let _ = self.events.send(RoomEvent::FileOffered {
                    from,
                    sent_at,
                    file,
                });
            }
            Control::HaveFile(hash) => {
                let _ = self.events.send(RoomEvent::FileProvider { from, hash });
            }
            Control::Watch(track) => self.add_watcher(from, track),
            Control::Unwatch(track) => self.remove_watcher(from, track),
            Control::KeyframeRequest(track) => {
                if self.watchers.contains_key(&(from, track)) {
                    let _ = self.events.send(RoomEvent::KeyframeNeeded(track));
                }
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
                let stopped_screen = self.state.stream.is_some() && state.stream.is_none();
                let stopped_camera = self.state.camera && !state.camera;
                self.state = state.clone();
                self.broadcast(Control::State(state));
                for (stopped, track) in [
                    (stopped_screen, Track::Screen),
                    (stopped_camera, Track::Camera),
                ] {
                    if stopped {
                        self.watchers.retain(|(_, t), _| *t != track);
                        let _ = self.events.send(RoomEvent::Watchers { track, count: 0 });
                    }
                }
            }
            Command::Watch(id, track) => self.send_to(id, Control::Watch(track)),
            Command::Unwatch(id, track) => self.send_to(id, Control::Unwatch(track)),
            Command::RequestKeyframe(id, track) => {
                self.send_to(id, Control::KeyframeRequest(track))
            }
            Command::Video(track, pkt) => self.fan_out(track, pkt),
            Command::OfferFile(file) => self.post(now_ms(), HistoryBody::File(file)),
            Command::Post { sent_at, body } => self.post(sent_at, body),
            Command::HaveFile(hash) => self.broadcast(Control::HaveFile(hash)),
            Command::Audio(bytes) => {
                for p in self.peers.values() {
                    // Dropped if the path is congested; that's fine for voice.
                    let _ = p.conn.send_datagram(bytes.clone());
                }
            }
            Command::StreamAudio(bytes) => {
                for (id, _) in self.watchers.keys().filter(|(_, t)| *t == Track::Screen) {
                    if let Some(p) = self.peers.get(id) {
                        let _ = p.conn.send_datagram(bytes.clone());
                    }
                }
            }
            Command::SetAudioSink(sink) => *self.audio_sink.lock().expect("audio sink") = sink,
            Command::SetProfile(profile) => {
                self.profile = profile.clone();
                self.broadcast(Control::Profile(profile));
            }
            Command::Chat(text) => self.post(now_ms(), HistoryBody::Text(text)),
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

    /// Send our own chat message or file offer and keep it in the history.
    fn post(&mut self, sent_at: u64, body: HistoryBody) {
        let (me, name, color) = (self.me, self.profile.name.clone(), self.profile.color);
        self.remember(me, name, color, sent_at, body.clone());
        match body {
            HistoryBody::Text(text) => {
                self.chat_seq += 1;
                self.broadcast(Control::Chat {
                    id: self.chat_seq,
                    sent_at,
                    text,
                });
            }
            HistoryBody::File(file) => self.broadcast(Control::File { sent_at, file }),
        }
    }

    fn remember(
        &mut self,
        author: EndpointId,
        author_name: String,
        author_color: u32,
        sent_at: u64,
        body: HistoryBody,
    ) {
        self.history.push_back(HistoryItem {
            author,
            author_name,
            author_color,
            sent_at,
            body,
        });
        while self.history.len() > HISTORY_LIMIT {
            self.history.pop_front();
        }
    }

    fn send_to(&self, id: EndpointId, msg: Control) {
        if let Some(p) = self.peers.get(&id) {
            let _ = p.tx.send(msg);
        }
    }

    fn track_live(&self, track: Track) -> bool {
        match track {
            Track::Screen => self.state.stream.is_some(),
            Track::Camera => self.state.camera,
        }
    }

    fn watcher_count(&self, track: Track) -> usize {
        self.watchers.keys().filter(|(_, t)| *t == track).count()
    }

    /// A peer asked to watch a track: open a video stream to them.
    fn add_watcher(&mut self, id: EndpointId, track: Track) {
        if !self.track_live(track) || self.watchers.contains_key(&(id, track)) {
            return;
        }
        let Some(peer) = self.peers.get(&id) else {
            return;
        };
        let (tx, rx) = mpsc::channel(WATCHER_QUEUE);
        self.watcher_generation += 1;
        let generation = self.watcher_generation;
        spawn_video_writer(
            peer.conn.clone(),
            id,
            track,
            generation,
            rx,
            self.inbound.clone(),
        );
        self.watchers.insert(
            (id, track),
            Watcher {
                tx,
                generation,
                need_key: true,
            },
        );
        let count = self.watcher_count(track);
        let _ = self.events.send(RoomEvent::Watchers { track, count });
        let _ = self.events.send(RoomEvent::KeyframeNeeded(track));
    }

    fn remove_watcher(&mut self, id: EndpointId, track: Track) {
        if self.watchers.remove(&(id, track)).is_some() {
            let count = self.watcher_count(track);
            let _ = self.events.send(RoomEvent::Watchers { track, count });
        }
    }

    fn remove_peer_watchers(&mut self, id: EndpointId) {
        for track in [Track::Screen, Track::Camera] {
            self.remove_watcher(id, track);
        }
    }

    /// Send one encoded frame to every watcher of its track. Viewers that
    /// can't keep up skip to the next keyframe instead of building a backlog.
    fn fan_out(&mut self, track: Track, pkt: Arc<VideoPacket>) {
        let mut want_key = false;
        for ((_, t), w) in self.watchers.iter_mut() {
            if *t != track || (w.need_key && !pkt.keyframe) {
                continue;
            }
            match w.tx.try_send(pkt.clone()) {
                Ok(()) => w.need_key = false,
                Err(mpsc::error::TrySendError::Full(_)) => {
                    w.need_key = true;
                    want_key = true;
                }
                Err(mpsc::error::TrySendError::Closed(_)) => {}
            }
        }
        if want_key {
            let _ = self.events.send(RoomEvent::Congested(track));
            let _ = self.events.send(RoomEvent::KeyframeNeeded(track));
        }
    }

    fn broadcast(&self, msg: Control) {
        for p in self.peers.values() {
            let _ = p.tx.send(msg.clone());
        }
    }

    /// Connect to a member we just heard of, unless we know them already.
    /// Only the smaller id dials; the other side waits to be dialed.
    fn meet(&mut self, addr: EndpointAddr) {
        let id = addr.id;
        if id == self.me || self.peers.contains_key(&id) {
            return;
        }
        if self.me < id {
            self.dial(addr, false);
        } else {
            self.expect_dial(addr);
        }
    }

    /// They should dial us; if they haven't within a few seconds (e.g. their
    /// outbound path is blocked), dial them ourselves.
    fn expect_dial(&mut self, addr: EndpointAddr) {
        let inbound = self.inbound.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(4)).await;
            let _ = inbound.send(Inbound::DialIfMissing(addr));
        });
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
                Ok((conn, send, recv, profile, state, members, history)) => {
                    let _ = inbound.send(Inbound::Welcomed {
                        conn,
                        send,
                        recv,
                        addr,
                        profile,
                        state,
                        members,
                        history,
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
    Vec<HistoryItem>,
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
            history,
        } => Ok((conn, send, recv, profile, state, members, history)),
        Control::Rejected => bail!("the room rejected this link"),
        other => bail!("unexpected reply {other:?}"),
    }
}

fn spawn_writer(mut send: SendStream, mut rx: mpsc::UnboundedReceiver<Control>) {
    tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            let leaving = matches!(msg, Control::Leave);
            if write_frame(&mut send, &msg).await.is_err() {
                return;
            }
            if leaving {
                break;
            }
        }
        // Finish (not reset) so everything written still arrives.
        let _ = send.finish();
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

/// Sharer side: one unidirectional stream per watcher carrying video packets.
fn spawn_video_writer(
    conn: Connection,
    to: EndpointId,
    track: Track,
    generation: u64,
    mut rx: mpsc::Receiver<Arc<VideoPacket>>,
    inbound: flume::Sender<Inbound>,
) {
    tokio::spawn(async move {
        let result: Result<()> = async {
            let mut send = conn.open_uni().await.context("open video stream")?;
            send.write_all(&[UNI_VIDEO, track.to_byte()])
                .await
                .context("write stream kind")?;
            while let Some(pkt) = rx.recv().await {
                write_video(&mut send, &pkt).await?;
            }
            let _ = send.finish();
            Ok(())
        }
        .await;
        if let Err(e) = result {
            debug!("video stream to {to} ended: {e:#}");
            let _ = inbound.send(Inbound::WatcherGone(to, track, generation));
        }
    });
}

/// Viewer side: accept unidirectional streams from a peer and hand video
/// packets to the UI through a bounded channel.
fn spawn_uni_acceptor(
    conn: Connection,
    from: EndpointId,
    inbound: flume::Sender<Inbound>,
    events: flume::Sender<RoomEvent>,
) {
    tokio::spawn(async move {
        while let Ok(mut recv) = conn.accept_uni().await {
            let mut header = [0u8; 2];
            if recv.read_exact(&mut header).await.is_err() || header[0] != UNI_VIDEO {
                continue;
            }
            let Some(track) = Track::from_byte(header[1]) else {
                continue;
            };
            let (tx, rx) = flume::bounded(120);
            if events
                .send(RoomEvent::Video {
                    from,
                    track,
                    feed: VideoFeed(rx),
                })
                .is_err()
            {
                return;
            }
            let inbound = inbound.clone();
            tokio::spawn(async move {
                // After dropping a packet, later frames can't decode until the
                // next keyframe: skip to it, and ask for one at most once a
                // second (each request makes the sharer send a big frame).
                let mut waiting_for_key = false;
                let mut last_request: Option<tokio::time::Instant> = None;
                while let Ok(pkt) = read_video(&mut recv).await {
                    if waiting_for_key && !pkt.keyframe {
                        continue;
                    }
                    match tx.try_send(pkt) {
                        Ok(()) => waiting_for_key = false,
                        // The UI dropped the feed (stopped watching).
                        Err(flume::TrySendError::Disconnected(_)) => break,
                        Err(flume::TrySendError::Full(_)) => {
                            waiting_for_key = true;
                            let now = tokio::time::Instant::now();
                            if last_request.is_none_or(|t| now - t >= Duration::from_secs(1)) {
                                last_request = Some(now);
                                let _ = inbound
                                    .send(Inbound::Cmd(Command::RequestKeyframe(from, track)));
                            }
                        }
                    }
                }
            });
        }
    });
}

/// Read voice datagrams from a peer and hand them to the audio engine.
fn spawn_datagram_reader(conn: Connection, from: EndpointId, sink: AudioSink) {
    tokio::spawn(async move {
        while let Ok(bytes) = conn.read_datagram().await {
            let Ok(pkt) = glance_proto::decode::<glance_proto::AudioDatagram>(&bytes) else {
                continue;
            };
            let tx = sink.lock().expect("audio sink").clone();
            if let Some(tx) = tx {
                let _ = tx.try_send((from, pkt));
            }
        }
    });
}
