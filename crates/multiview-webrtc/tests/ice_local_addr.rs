//! Failing-first, **offline** tests for box-validation defect #3: the
//! [`UnifiedEndpoint`] fed str0m the **unspecified** bind address (`[::]:PORT`)
//! as every inbound datagram's local destination, so str0m discarded every
//! inbound STUN binding-request ("Discarding STUN request on unknown
//! interface") because none of its gathered local candidates is `[::]` — ICE
//! never reached Connected and zero frames flowed (host AND relay paths).
//!
//! The fix presents str0m the **concrete** local address each datagram actually
//! arrived on. These tests prove, entirely in memory (no real socket), that:
//!
//! * the pure resolver maps a concrete arrival address (PKTINFO) onto the
//!   candidate str0m gathered — never the unspecified bind addr; and
//! * a full ICE+DTLS handshake completes when the answerer is fed the resolved
//!   **concrete** destination, but does NOT complete when fed the **unspecified**
//!   `[::]` destination (the old, broken behaviour) — i.e. the STUN checks are
//!   accepted vs. discarded.
#![cfg(feature = "native")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::as_conversions,
    clippy::missing_panics_doc
)]

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use multiview_webrtc::transport::{resolve_local_destination, MediaKind, Session, SessionConfig};

/// The answerer's single gathered concrete host candidate (what str0m knows as a
/// local candidate — the `advertised_addresses` entry, IPv6-first per ADR-0042).
const ANSWERER_CANDIDATE: &str = "[2001:db8::a]:7000";
/// The unspecified dual-stack bind address the OLD code fed str0m as the
/// destination of every inbound datagram (defect #3).
const UNSPECIFIED_LOCAL: &str = "[::]:7000";
/// The offerer's host candidate (the remote peer, e.g. a browser).
const OFFERER_CANDIDATE: &str = "[2001:db8::b]:6000";

// ---------------------------------------------------------------------------
// 1. The pure resolver: concrete arrival addr -> a gathered local candidate.
// ---------------------------------------------------------------------------

#[test]
fn resolver_returns_an_exact_candidate_match_unchanged() {
    let cand: SocketAddr = ANSWERER_CANDIDATE.parse().unwrap();
    let arrival: SocketAddr = ANSWERER_CANDIDATE.parse().unwrap();
    // A datagram whose PKTINFO destination IS a gathered candidate is fed verbatim.
    assert_eq!(resolve_local_destination(arrival, &[cand]), cand);
}

#[test]
fn resolver_never_returns_the_unspecified_bind_addr() {
    // The whole defect: the unspecified `[::]` arrival must be resolved to a
    // concrete gathered candidate, NEVER passed through (str0m discards `[::]`).
    let cand: SocketAddr = ANSWERER_CANDIDATE.parse().unwrap();
    let unspecified: SocketAddr = UNSPECIFIED_LOCAL.parse().unwrap();
    let resolved = resolve_local_destination(unspecified, &[cand]);
    assert!(
        !resolved.ip().is_unspecified(),
        "resolver leaked the unspecified bind addr str0m discards: {resolved}"
    );
    assert_eq!(resolved, cand, "resolved to the only gathered candidate");
}

#[test]
fn resolver_maps_a_nat_private_arrival_to_the_same_family_candidate() {
    // NAT 1:1 / Docker: PKTINFO reports the PRIVATE interface IP, but str0m only
    // knows the PUBLIC `advertised_addresses` candidate. The resolver maps a
    // non-matching arrival to the gathered candidate of the same IP family, so the
    // STUN check str0m runs against its (public) candidate is accepted.
    let public_v6: SocketAddr = ANSWERER_CANDIDATE.parse().unwrap();
    let private_v6: SocketAddr = "[fd00::99]:7000".parse().unwrap();
    assert_eq!(
        resolve_local_destination(private_v6, &[public_v6]),
        public_v6
    );
}

#[test]
fn resolver_prefers_a_same_family_candidate_over_a_cross_family_one() {
    // With both a v6 and a v4 candidate gathered, a v4-mapped arrival resolves to
    // the v4 candidate (family match), not the v6 one.
    let v6: SocketAddr = ANSWERER_CANDIDATE.parse().unwrap();
    let v4: SocketAddr = "203.0.113.7:7000".parse().unwrap();
    let v4_arrival: SocketAddr = "192.0.2.50:7000".parse().unwrap();
    assert_eq!(resolve_local_destination(v4_arrival, &[v6, v4]), v4);
}

// ---------------------------------------------------------------------------
// 2. str0m-level handshake: concrete destination connects; `[::]` is discarded.
// ---------------------------------------------------------------------------

/// Drive a full ICE+DTLS handshake between an offerer and an answerer, where the
/// answerer is bound to an unspecified socket: every datagram the offerer sends
/// "arrives" on the answerer's unspecified socket, and the driver resolves the
/// local destination via `resolve_local` before feeding str0m. Returns whether
/// both peers reached Connected within the iteration bound.
fn handshake_with_resolver(resolve_local: impl Fn(SocketAddr) -> SocketAddr) -> bool {
    let now = Instant::now();
    let a_addr: SocketAddr = OFFERER_CANDIDATE.parse().unwrap();
    let b_cand: SocketAddr = ANSWERER_CANDIDATE.parse().unwrap();

    let mut a = Session::new(&SessionConfig::default(), now);
    let mut b = Session::new(&SessionConfig::default(), now);
    // Each peer gathers ONLY its concrete candidate (the box skips `[::]`).
    a.add_host_candidate(a_addr).unwrap();
    b.add_host_candidate(b_cand).unwrap();
    let offer = a.create_offer(&[MediaKind::Video]).unwrap();
    let answer = b.accept_offer(&offer).unwrap();
    a.accept_answer(&answer).unwrap();

    let mut clock = now;
    for _ in 0..2000 {
        a.handle_timeout(clock).unwrap();
        b.handle_timeout(clock).unwrap();
        // a -> b. b is "bound" on the unspecified socket; the datagram arrives on
        // it and the driver resolves the concrete local destination str0m knows.
        for _ in 0..128 {
            match a.poll_transmit(clock) {
                Some((_src, dst, payload)) => {
                    assert_eq!(dst, b_cand, "offerer targets the answerer candidate");
                    let local = resolve_local(dst);
                    b.handle_datagram(a_addr, local, &payload, clock).unwrap();
                }
                None => break,
            }
        }
        // b -> a (the offerer is a normal concrete-bound peer in this shuttle).
        for _ in 0..128 {
            match b.poll_transmit(clock) {
                Some((_src, dst, payload)) => {
                    a.handle_datagram(b_cand, a_addr, &payload, clock).unwrap();
                    let _ = dst;
                }
                None => break,
            }
        }
        if a.is_connected() && b.is_connected() {
            return true;
        }
        let next = a.poll_timeout(clock).min(b.poll_timeout(clock));
        clock = next.max(clock + Duration::from_millis(1));
    }
    false
}

#[test]
fn ice_completes_when_the_answerer_is_fed_the_resolved_concrete_destination() {
    let cand: SocketAddr = ANSWERER_CANDIDATE.parse().unwrap();
    // The driver resolves the unspecified arrival to the gathered candidate.
    let connected = handshake_with_resolver(|dst| resolve_local_destination(dst, &[cand]));
    assert!(
        connected,
        "ICE+DTLS must complete once str0m is fed the concrete local destination"
    );
}

#[test]
fn ice_fails_when_the_answerer_is_fed_the_unspecified_bind_addr() {
    // The OLD behaviour (defect #3): feed str0m the unspecified `[::]` bind addr
    // as the local destination. str0m discards every inbound STUN binding-request
    // ("unknown interface"), so ICE can NEVER complete. This regression-guards the
    // fix: if someone reverts to passing `local_addr` ([::]), this goes green and
    // the test fails — exactly the broken state box-validation hit.
    let unspecified: SocketAddr = UNSPECIFIED_LOCAL.parse().unwrap();
    let connected = handshake_with_resolver(|_dst| unspecified);
    assert!(
        !connected,
        "feeding str0m the unspecified [::] destination must NOT connect \
         (STUN is discarded as 'unknown interface') — this is defect #3"
    );
}
