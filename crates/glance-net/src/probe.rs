//! Room liveness probes: "is room X still going, and who's in it?"
//!
//! A prober dials a member it remembers on [`ROOM_PROBE_ALPN`], sends one
//! [`ProbeRequest`] (proving it holds the room's link) and reads one
//! [`ProbeReply`]. Only a peer currently in that room answers with its member
//! list; everyone else says `NotHere`. Peers from before probes existed don't
//! speak the ALPN, so the connection fails and the room reads as unknown.

use std::collections::HashSet;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use glance_proto::{
    EndpointAddr, MemberInfo, ProbeReply, ProbeRequest, ROOM_PROBE_ALPN, Ticket, probe_proof,
    verify_probe_proof,
};
use iroh::Endpoint;
use iroh::endpoint::Connection;
use iroh::protocol::{AcceptError, ProtocolHandler};

use crate::RoomSlot;
use crate::io::{read_frame, write_frame};

/// What probing a room found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RoomStatus {
    /// Someone answered from inside the room; these are its members
    /// (including whoever answered).
    Alive(Vec<MemberInfo>),
    /// Everyone we asked answered that they aren't in it (or there was no
    /// one else to ask).
    Empty,
    /// Not everyone could be reached (offline, slow, or too old to answer
    /// probes), and nobody who answered is in it. The room may still be
    /// going with the ones we couldn't reach, so it must not be started
    /// again from scratch.
    Unknown,
}

/// Accept side: answer probes for the room we're in.
#[derive(Debug, Clone)]
pub(crate) struct ProbeProtocol(pub(crate) RoomSlot);

impl ProtocolHandler for ProbeProtocol {
    async fn accept(&self, connection: Connection) -> Result<(), AcceptError> {
        if let Err(e) = answer(&self.0, &connection).await {
            tracing::debug!("room probe from {}: {e:#}", connection.remote_id());
        }
        Ok(())
    }
}

async fn answer(slot: &RoomSlot, conn: &Connection) -> Result<()> {
    let (mut send, mut recv) = tokio::time::timeout(Duration::from_secs(5), conn.accept_bi())
        .await
        .context("probe timeout")??;
    let request: ProbeRequest = tokio::time::timeout(Duration::from_secs(5), read_frame(&mut recv))
        .await
        .context("probe timeout")??;
    let room = slot.lock().expect("room slot").clone();
    let reply = match room {
        Some(room) => {
            let (tx, rx) = tokio::sync::oneshot::channel();
            let asked = room.send(crate::room::Inbound::Probe {
                from: conn.remote_id(),
                request,
                reply: tx,
            });
            match asked {
                Ok(()) => tokio::time::timeout(Duration::from_secs(2), rx)
                    .await
                    .ok()
                    .and_then(|r| r.ok())
                    .unwrap_or(ProbeReply::NotHere),
                Err(_) => ProbeReply::NotHere,
            }
        }
        None => ProbeReply::NotHere,
    };
    write_frame(&mut send, &reply).await?;
    let _ = send.finish();
    // The prober closes once it has read the reply.
    let _ = tokio::time::timeout(Duration::from_secs(3), conn.closed()).await;
    Ok(())
}

/// The actor's answer to a probe for the room it runs.
pub(crate) fn reply_for(
    ticket: &Ticket,
    me: iroh::EndpointId,
    from: iroh::EndpointId,
    request: &ProbeRequest,
    members: impl FnOnce() -> Vec<MemberInfo>,
) -> ProbeReply {
    if request.room != ticket.room {
        return ProbeReply::NotHere;
    }
    if !verify_probe_proof(&ticket.secret, ticket.room, from, me, &request.proof) {
        return ProbeReply::Rejected;
    }
    ProbeReply::Members {
        proof: probe_proof(&ticket.secret, ticket.room, me, from),
        members: members(),
    }
}

/// How one probe went.
#[derive(Debug)]
enum Outcome {
    Alive(Vec<MemberInfo>),
    NotHere,
}

/// Ask one peer about the room.
async fn probe_one(endpoint: &Endpoint, ticket: &Ticket, addr: EndpointAddr) -> Result<Outcome> {
    let to = addr.id;
    let conn = endpoint
        .connect(addr, ROOM_PROBE_ALPN)
        .await
        .map_err(|e| anyhow::anyhow!("connect: {e}"))?;
    let result = async {
        let (mut send, mut recv) = conn.open_bi().await.context("open probe stream")?;
        let me = endpoint.id();
        write_frame(
            &mut send,
            &ProbeRequest {
                room: ticket.room,
                proof: probe_proof(&ticket.secret, ticket.room, me, to),
            },
        )
        .await?;
        let _ = send.finish();
        let reply: ProbeReply = read_frame(&mut recv).await?;
        match reply {
            ProbeReply::Members { proof, members } => {
                if !verify_probe_proof(&ticket.secret, ticket.room, to, me, &proof) {
                    bail!("probe answer without a valid proof");
                }
                Ok(Outcome::Alive(members))
            }
            ProbeReply::NotHere => Ok(Outcome::NotHere),
            ProbeReply::Rejected => bail!("probe rejected"),
        }
    }
    .await;
    conn.close(0u32.into(), b"done");
    result
}

/// Ask every candidate at once; the first one inside the room wins. Each
/// probe gives up after `timeout`. The room only reads as empty when every
/// candidate answered that it isn't in it: someone who left (often the
/// link's creator) answers at once, while members still inside may just be
/// slow to reach, and starting the room again next to them would split it.
pub async fn probe(
    endpoint: &Endpoint,
    ticket: &Ticket,
    candidates: Vec<EndpointAddr>,
    timeout: Duration,
) -> RoomStatus {
    let me = endpoint.id();
    let mut seen = HashSet::new();
    let mut tasks = tokio::task::JoinSet::new();
    for addr in candidates {
        if addr.id == me || !seen.insert(addr.id) {
            continue;
        }
        let (endpoint, ticket) = (endpoint.clone(), ticket.clone());
        tasks.spawn(async move {
            tokio::time::timeout(timeout, probe_one(&endpoint, &ticket, addr))
                .await
                .map_err(|_| anyhow::anyhow!("probe timed out"))
                .and_then(|r| r)
        });
    }
    // Nobody else was ever in it: as far as we can tell, it's empty.
    let mut all_answered = true;
    while let Some(done) = tasks.join_next().await {
        match done {
            Ok(Ok(Outcome::Alive(members))) => return RoomStatus::Alive(members),
            Ok(Ok(Outcome::NotHere)) => {}
            Ok(Err(e)) => {
                tracing::debug!("room probe failed: {e:#}");
                all_answered = false;
            }
            Err(_) => all_answered = false,
        }
    }
    if all_answered {
        RoomStatus::Empty
    } else {
        RoomStatus::Unknown
    }
}
