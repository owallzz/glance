//! iroh networking: the endpoint, the room mesh, and (later) media streams,
//! the contacts protocol and file transfer.
//!
//! [`Node`] owns the iroh endpoint and routes incoming connections to the
//! active room. [`Node::create_room`] / [`Node::join_room`] return a
//! [`RoomHandle`]: send it [`Command`]s and read [`RoomEvent`]s.

mod io;
pub mod room;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
use glance_proto::{MemberState, ROOM_ALPN, RoomId, Ticket, WireProfile};
use iroh::endpoint::{Connection, RelayMode, presets};
use iroh::protocol::{AcceptError, ProtocolHandler, Router};
use iroh::{Endpoint, EndpointAddr, RelayMap, RelayUrl, SecretKey, TransportAddr};

pub use glance_proto::EndpointId;
pub use room::{Command, RoomEvent, RoomHandle, VideoFeed};

/// Which relays the endpoint uses when a direct path isn't possible.
#[derive(Clone, Debug, Default)]
pub enum Relay {
    /// n0's public relays and address lookup.
    #[default]
    Default,
    /// Your own `iroh-relay` server.
    Custom(RelayUrl),
    /// No relays and no address lookup: direct addresses only (tests, LAN).
    Disabled,
}

pub struct NetConfig {
    pub secret_key: SecretKey,
    pub relay: Relay,
}

/// The slot the protocol handler hands incoming room connections to.
type RoomSlot = Arc<Mutex<Option<flume::Sender<room::Inbound>>>>;

#[derive(Clone)]
pub struct Node {
    router: Router,
    room_slot: RoomSlot,
}

#[derive(Debug, Clone)]
struct RoomProtocol(RoomSlot);

impl ProtocolHandler for RoomProtocol {
    async fn accept(&self, connection: Connection) -> Result<(), AcceptError> {
        let tx = self.0.lock().expect("room slot").clone();
        match tx {
            Some(tx) if tx.send(room::Inbound::Incoming(connection.clone())).is_ok() => {}
            _ => connection.close(1u32.into(), b"no room"),
        }
        Ok(())
    }
}

/// The address to put in an invite link. With relays, the relay URL is
/// enough (address lookup and hole punching find direct paths), which keeps
/// links short. Without relays, keep a couple of the most useful direct
/// addresses (private/public IPv4 first, then global IPv6).
pub fn invite_addr(addr: EndpointAddr) -> EndpointAddr {
    let relays: Vec<TransportAddr> = addr
        .addrs
        .iter()
        .filter(|a| matches!(a, TransportAddr::Relay(_)))
        .cloned()
        .collect();
    if !relays.is_empty() {
        return EndpointAddr::from_parts(addr.id, relays.into_iter().take(1));
    }
    let mut ips: Vec<std::net::SocketAddr> = addr.ip_addrs().copied().collect();
    ips.sort_by_key(|sa| match sa.ip() {
        std::net::IpAddr::V4(v4) if v4.is_private() => 0,
        std::net::IpAddr::V4(v4) if !v4.is_loopback() && !v4.is_link_local() => 1,
        std::net::IpAddr::V6(v6) if (v6.segments()[0] & 0xe000) == 0x2000 => 2,
        std::net::IpAddr::V4(_) => 3,
        std::net::IpAddr::V6(_) => 4,
    });
    EndpointAddr::from_parts(addr.id, ips.into_iter().take(2).map(TransportAddr::Ip))
}

impl Node {
    pub async fn spawn(config: NetConfig) -> Result<Self> {
        let builder = match &config.relay {
            Relay::Default => Endpoint::builder(presets::N0),
            Relay::Custom(url) => Endpoint::builder(presets::N0)
                .relay_mode(RelayMode::Custom(RelayMap::from(url.clone()))),
            Relay::Disabled => Endpoint::builder(presets::Minimal).relay_mode(RelayMode::Disabled),
        };
        let endpoint = builder
            .secret_key(config.secret_key)
            .bind()
            .await
            .map_err(|e| anyhow::anyhow!("bind endpoint: {e}"))?;
        let room_slot: RoomSlot = Arc::default();
        let router = Router::builder(endpoint)
            .accept(ROOM_ALPN, RoomProtocol(room_slot.clone()))
            .spawn();
        Ok(Self { router, room_slot })
    }

    pub fn endpoint(&self) -> &Endpoint {
        self.router.endpoint()
    }

    pub fn id(&self) -> EndpointId {
        self.endpoint().id()
    }

    /// Wait (bounded) until the endpoint has a relay or direct address, so
    /// tickets made right after start are dialable.
    pub async fn wait_online(&self, timeout: Duration) {
        let _ = tokio::time::timeout(timeout, self.endpoint().online()).await;
    }

    /// Start a new room; its ticket is in the returned handle.
    pub async fn create_room(
        &self,
        profile: WireProfile,
        state: MemberState,
    ) -> Result<RoomHandle> {
        if !matches!(self.endpoint().addr().addrs.len(), 1..) {
            self.wait_online(Duration::from_secs(5)).await;
        }
        let ticket = Ticket {
            room: RoomId(rand::random()),
            secret: rand::random(),
            host: invite_addr(self.endpoint().addr()),
        };
        Ok(self.start_room(ticket, profile, state, false))
    }

    /// Join the room a ticket points to.
    pub async fn join_room(
        &self,
        ticket: Ticket,
        profile: WireProfile,
        state: MemberState,
    ) -> Result<RoomHandle> {
        Ok(self.start_room(ticket, profile, state, true))
    }

    fn start_room(
        &self,
        ticket: Ticket,
        profile: WireProfile,
        state: MemberState,
        dial_host: bool,
    ) -> RoomHandle {
        let (handle, inbound_tx) = room::spawn(
            self.endpoint().clone(),
            ticket,
            profile,
            state,
            dial_host,
            self.room_slot.clone(),
        );
        *self.room_slot.lock().expect("room slot") = Some(inbound_tx);
        handle
    }

    pub async fn shutdown(&self) {
        let _ = self.router.shutdown().await;
    }
}
