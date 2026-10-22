//! Shared relay-aware datagram I/O for the unified endpoint driver (defect C,
//! ADR-0048 §5.1) — **feature `native`**.
//!
//! str0m's [`Transmit::source`](str0m::net::Transmit) tells us how a datagram must
//! leave the host: if the source is one of our bound local sockets, send it
//! directly; if it is an allocated TURN **relay** address, frame it as a TURN Send
//! indication to that relay's server (RFC 8656 §10). Inbound, a datagram from a
//! configured TURN server may be a relayed **Data indication** that must be
//! decapsulated back into the application bytes and fed to str0m as arriving on the
//! relay candidate's address. These two helpers centralise that routing so every
//! role on the single shared socket frames relay media identically — the gap the
//! box-validation named (the relay was advertised but media never traversed it).

use std::net::SocketAddr;

use tokio::net::UdpSocket;

use crate::turn::TurnRelayDriver;

/// Send one `(source, destination, payload)` the session emitted, routing it
/// through the TURN relay when `source` is an allocated relay address (defect C).
///
/// * `source` is one of our bound local sockets → send `payload` straight to
///   `destination` (the direct/host path).
/// * `source` is an allocated TURN relay → frame `payload` as a TURN Send
///   indication toward `destination` (the peer) and send that to the relay's TURN
///   server. A permission for the peer is ensured by [`TurnRelayDriver`] first.
///
/// Non-blocking; a send error is dropped (str0m / the peer recover via retransmit,
/// and the driver never blocks on a peer — invariant #10).
pub(crate) async fn send_routed(
    socket: &UdpSocket,
    turn: &mut TurnRelayDriver,
    source: SocketAddr,
    destination: SocketAddr,
    payload: &[u8],
    now: std::time::Instant,
) {
    if turn.is_relay(source) {
        if let Some((server, wire)) = turn.frame_for_relay(source, destination, payload, now) {
            let _ = socket.send_to(&wire, server).await;
            return;
        }
        // The relay vanished between poll and send (a re-alloc / teardown race):
        // drop this datagram — the peer's ICE keepalive re-drives the path.
        return;
    }
    let _ = socket.send_to(payload, destination).await;
}

/// The two ways an inbound datagram from `src` can be consumed before it reaches
/// the media demux: as a TURN-server control reply (consumed by the relay driver),
/// or as a relayed Data indication that decapsulates to an application datagram for
/// a session.
pub(crate) enum Inbound {
    /// `src` is a TURN server and the datagram was a control reply (Allocate /
    /// Refresh / `CreatePermission` response) the relay driver consumed — there is
    /// nothing to route to a session.
    TurnControl,
    /// `src` is a TURN server and the datagram was a relayed **Data indication**:
    /// route `payload` to the sessions as arriving from `peer` on `relay`.
    Relayed {
        /// The far peer the relayed data came from.
        peer: SocketAddr,
        /// The local relay address the data arrived on (the candidate str0m
        /// matches by `addr == destination`).
        relay: SocketAddr,
        /// The decapsulated application payload.
        payload: Vec<u8>,
    },
    /// Not from a TURN server: route `src`'s datagram to the media demux directly.
    Media,
}

/// Classify an inbound datagram from `src`: a relayed Data indication is
/// decapsulated (defect C); any other datagram from a configured TURN server feeds
/// the relay driver (Allocate/Refresh/permission replies); everything else is
/// ordinary media for the session demux.
#[must_use]
pub(crate) fn classify_inbound(
    turn: &mut TurnRelayDriver,
    src: SocketAddr,
    payload: &[u8],
    now: std::time::Instant,
) -> Inbound {
    // A relayed Data indication first (it carries media we must decapsulate).
    if let Some(relayed) = turn.try_unwrap_relayed(src, payload, now) {
        return Inbound::Relayed {
            peer: relayed.peer,
            relay: relayed.relay,
            payload: relayed.payload,
        };
    }
    // Otherwise, a TURN-server control reply is consumed by the driver; a
    // non-TURN datagram is left for the media path.
    if turn.feed(src, payload, now) {
        Inbound::TurnControl
    } else {
        Inbound::Media
    }
}
