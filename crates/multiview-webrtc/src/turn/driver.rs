//! The shared sans-IO **TURN relay driver** (ADR-0048 §5.1).
//!
//! Both endpoint drivers — the WHIP-ingest endpoint ([`crate::transport`], async
//! tokio over the shared UDP socket) and the WHEP-egress preview driver (in the
//! cli, a sync socket loop) — must run their configured TURN clients over their
//! own socket and harvest the allocated relay address so str0m can offer it as a
//! relay candidate. [`TurnRelayDriver`] wraps one [`TurnClient`] per configured
//! TURN server with the build / pump / feed / harvest steps both endpoints need,
//! so the [`TurnClient`] (the shared machinery) is reused and the driving glue
//! lives in one place — never re-implemented or duplicated per endpoint.
//!
//! It is **pure / sans-IO**: it never owns a socket. The caller pulls outbound
//! datagrams with [`TurnRelayDriver::poll_transmit`] and sends them on its own
//! socket; pushes received datagrams in with [`TurnRelayDriver::feed`]; and reads
//! the newly-learned relays with [`TurnRelayDriver::take_new_relays`]. This keeps
//! it offline-testable against the in-process fake TURN server, and lets the sync
//! (WHEP) and async (WHIP) endpoints drive it the same way.

use std::net::SocketAddr;
use std::time::Instant;

use crate::config::{EndpointConfig, IceServerKind};
use crate::turn::{TurnClient, TurnEvent, TurnOutput};

/// One inbound TURN Data indication unwrapped back into the application datagram
/// the relay forwarded (defect C). The driver reports the `relay` it arrived on so
/// the caller feeds str0m a `Receive` whose destination is the relay candidate's
/// address (str0m matches the local relay candidate by `addr == destination`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelayedDatagram {
    /// The far peer the data originated from (XOR-PEER-ADDRESS).
    pub peer: SocketAddr,
    /// The decapsulated application payload (the inner SRTP/STUN bytes).
    pub payload: Vec<u8>,
    /// The relay address the data arrived on — the local candidate str0m matches.
    pub relay: SocketAddr,
}

/// A driven TURN client wrapping the shared sans-IO [`TurnClient`].
struct DrivenClient {
    client: TurnClient,
}

/// Runs the configured TURN clients sans-IO and harvests their relay candidates.
///
/// Build with [`Self::from_config`]; drive with [`Self::poll_transmit`] +
/// [`Self::feed`]; collect learned relays with [`Self::take_new_relays`]. Empty
/// when no TURN server is configured (the common self-hosted / port-forwarded
/// case), in which case every method is a cheap no-op.
pub struct TurnRelayDriver {
    clients: Vec<DrivenClient>,
    /// Relays learned since the last [`Self::take_new_relays`] drain, de-duped
    /// against everything already handed out.
    pending_relays: Vec<SocketAddr>,
    /// Every relay ever harvested (de-dup set; a re-allocation or a relay seen via
    /// both `feed` and `poll_transmit` is published once).
    known_relays: Vec<SocketAddr>,
}

impl std::fmt::Debug for TurnRelayDriver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TurnRelayDriver")
            .field("clients", &self.clients.len())
            .field("known_relays", &self.known_relays.len())
            .finish_non_exhaustive()
    }
}

impl TurnRelayDriver {
    /// Build a [`TurnClient`] per configured TURN server (ADR-0048 §5.1). The
    /// per-allocation credential is resolved `now` (ephemeral REST derives a
    /// time-limited username/password from the wall clock; long-term uses the
    /// static pair). STUN servers need no client here (str0m's server-reflexive
    /// candidates are gathered from the bound/advertised addresses). Empty when no
    /// TURN server is configured.
    #[must_use]
    pub fn from_config(config: &EndpointConfig, now: Instant) -> Self {
        // A wall-clock seconds value for the ephemeral-REST expiry derivation. The
        // monotonic `now` is the driver's tick clock; the REST username's expiry
        // is a unix time, so use the system clock here (a credential-derivation
        // detail — not a media-timeline clock, so this is not invariant-#3
        // territory).
        let now_unix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        let _ = now;
        let clients = config
            .ice_servers
            .iter()
            .filter(|s| s.kind == IceServerKind::Turn)
            .filter_map(|server| {
                let credential = server.credentials.as_ref()?.resolve(now_unix);
                Some(DrivenClient {
                    client: TurnClient::new(server.addr, credential),
                })
            })
            .collect();
        Self {
            clients,
            pending_relays: Vec::new(),
            known_relays: Vec::new(),
        }
    }

    /// The number of driven TURN clients (one per configured TURN server).
    #[must_use]
    pub fn client_count(&self) -> usize {
        self.clients.len()
    }

    /// Whether there are no TURN clients to drive (no TURN server configured).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.clients.is_empty()
    }

    /// Pull the next outbound datagram the driver wants sent (to a TURN server),
    /// or `None` when every client is idle/waiting. The caller sends it on its own
    /// socket. Each call drains at most one datagram across the clients; call in a
    /// `while let Some(..)` to flush a tick. Also harvests any relay a client has
    /// reached via a `poll_output`-driven transition.
    #[must_use]
    pub fn poll_transmit(&mut self, now: Instant) -> Option<(SocketAddr, Vec<u8>)> {
        // One `poll_output` per client per call: the caller drives this in a
        // `while let Some(..)`, so a client with several queued requests is fully
        // flushed across successive calls (the client serializes one request at a
        // time anyway). After each poll, harvest any relay the transition reached.
        for driven in &mut self.clients {
            let out = driven.client.poll_output(now);
            Self::harvest(
                &mut self.pending_relays,
                &mut self.known_relays,
                driven.client.relay(),
            );
            if let TurnOutput::Transmit {
                destination,
                payload,
            } = out
            {
                return Some((destination, payload));
            }
        }
        None
    }

    /// Feed a received datagram into the client that owns `src` (a configured TURN
    /// server). Returns `true` if a TURN client consumed it (the datagram was a
    /// TURN reply / relayed-data indication); `false` means it was **not** from a
    /// TURN server and the caller should route it to the media path. A successful
    /// `Allocate` (or a relay reached as a side effect) is harvested.
    pub fn feed(&mut self, src: SocketAddr, payload: &[u8], now: Instant) -> bool {
        for driven in &mut self.clients {
            if driven.client.server_addr() != src {
                continue;
            }
            if let Ok(Some(TurnEvent::Allocated(relay))) = driven.client.handle_input(payload, now)
            {
                Self::push_relay(&mut self.pending_relays, &mut self.known_relays, relay);
            }
            // Also catch a relay reached without an explicit `Allocated` event
            // surfacing here (e.g. it was already learned on a prior pass).
            Self::harvest(
                &mut self.pending_relays,
                &mut self.known_relays,
                driven.client.relay(),
            );
            return true;
        }
        false
    }

    /// Drain and return the relays learned since the last call (each relay is
    /// returned exactly once over the driver's life). The caller registers each as
    /// a str0m relay candidate (WHIP: per future negotiation; WHEP:
    /// `WhepEgress::learn_relay`).
    #[must_use]
    pub fn take_new_relays(&mut self) -> Vec<SocketAddr> {
        std::mem::take(&mut self.pending_relays)
    }

    /// Every relay currently allocated across the driven clients (one per client
    /// that has reached `Allocated`). The driver routes a session transmit whose
    /// str0m `source` matches one of these through that relay's TURN server.
    #[must_use]
    pub fn relays(&self) -> Vec<SocketAddr> {
        self.clients
            .iter()
            .filter_map(|c| c.client.relay())
            .collect()
    }

    /// Whether `addr` is an allocated relay address (the str0m `Transmit::source`
    /// for a relay-routed datagram, i.e. a relay candidate's base).
    #[must_use]
    pub fn is_relay(&self, addr: SocketAddr) -> bool {
        self.clients.iter().any(|c| c.client.relay() == Some(addr))
    }

    /// Frame an outbound application datagram for the relay `relay` toward `peer`
    /// (defect C / ADR-0048 §5.1): the str0m `Transmit::source` was `relay` and its
    /// `destination` was `peer`, so the bytes must ride a TURN Send indication to
    /// the relay's server. Returns `(turn_server, wire_bytes)` to send on the
    /// shared socket, or `None` if `relay` is not an allocated relay of any client.
    ///
    /// A permission for `peer` is ensured first (queued if absent) so the relay
    /// accepts the peer's return traffic (RFC 8656 §9 access control). `now` is the
    /// driver tick used to schedule any queued `CreatePermission`.
    #[must_use]
    pub fn frame_for_relay(
        &mut self,
        relay: SocketAddr,
        peer: SocketAddr,
        payload: &[u8],
        now: Instant,
    ) -> Option<(SocketAddr, Vec<u8>)> {
        for driven in &mut self.clients {
            if driven.client.relay() != Some(relay) {
                continue;
            }
            // Ensure the relay will accept traffic to/from this peer.
            if !driven.client.has_permission(&peer) {
                driven.client.create_permission(peer, now);
            }
            let wire = driven.client.wrap_send(peer, payload);
            return Some((driven.client.server_addr(), wire));
        }
        None
    }

    /// Try to unwrap an inbound datagram from the TURN server `src` as a relayed
    /// **Data indication** (defect C). Returns the decapsulated
    /// [`RelayedDatagram`] (peer + payload + the relay it arrived on) when `src` is
    /// a configured TURN server whose client is allocated and the datagram is a
    /// Data indication; `None` otherwise (the caller then routes `src`'s datagram
    /// through the ordinary [`Self::feed`] / media path).
    #[must_use]
    pub fn try_unwrap_relayed(
        &self,
        src: SocketAddr,
        payload: &[u8],
        now: Instant,
    ) -> Option<RelayedDatagram> {
        let _ = now;
        for driven in &self.clients {
            if driven.client.server_addr() != src {
                continue;
            }
            let relay = driven.client.relay()?;
            let (peer, data) = driven.client.unwrap_data(payload)?;
            return Some(RelayedDatagram {
                peer,
                payload: data,
                relay,
            });
        }
        None
    }

    /// Harvest `relay` (if any) into the pending+known sets, de-duped.
    fn harvest(
        pending: &mut Vec<SocketAddr>,
        known: &mut Vec<SocketAddr>,
        relay: Option<SocketAddr>,
    ) {
        if let Some(relay) = relay {
            Self::push_relay(pending, known, relay);
        }
    }

    /// Record `relay` as newly learned unless it was already published.
    fn push_relay(pending: &mut Vec<SocketAddr>, known: &mut Vec<SocketAddr>, relay: SocketAddr) {
        if known.contains(&relay) {
            return;
        }
        known.push(relay);
        pending.push(relay);
    }
}
