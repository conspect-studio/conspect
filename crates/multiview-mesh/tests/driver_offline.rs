//! The announce + browse driver step, exercised **offline** with an in-memory
//! fake transport (ADR-0051 §5, brief §13.2): the pure logic is testable without
//! a socket. A received announcement folds into the UNTRUSTED inventory (digest +
//! claim state, never auto-relaying); a peer that stops being heard ages out;
//! discovery is always-on (there is no off path). The mDNS socket is isolated
//! behind the `MeshTransport` trait, so this test needs no network.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::sync::Mutex;
use std::time::Duration;

use chrono::{DateTime, Utc};
use ed25519_dalek::rand_core::UnwrapErr;
use ed25519_dalek::SigningKey;
use getrandom::SysRng;
use multiview_mesh::announce::{
    AnnouncePayload, EntitlementSummary, SaltedDigest, ANNOUNCE_PROTOCOL_VERSION,
};
use multiview_mesh::driver::announce_browse_step;
use multiview_mesh::peer::{PeerKey, PEER_STALE_AFTER};
use multiview_mesh::transport::{MeshTransport, ReceivedAnnouncement};
use multiview_mesh::{ClaimState, MeshError, MeshRole, MeshState};

fn epoch() -> DateTime<Utc> {
    DateTime::from_timestamp(1_700_000_000, 0).unwrap()
}

/// A signed announce for a peer whose primary digest is `[byte;32]`.
fn neighbour_wire(byte: u8, claim: ClaimState) -> Vec<u8> {
    let key = SigningKey::generate(&mut UnwrapErr(SysRng));
    let granted = epoch();
    let expires = granted + chrono::Duration::days(35);
    let summary = EntitlementSummary::new(
        multiview_licence::EnforcementLevel::Active,
        granted,
        expires,
    );
    let payload = AnnouncePayload::sign(
        ANNOUNCE_PROTOCOL_VERSION,
        vec![SaltedDigest::new([byte; 32])],
        claim,
        summary,
        &key,
    );
    payload.to_wire().expect("encode")
}

/// An in-memory fake transport: records announced wire, replays a fixed set of
/// received announcements once (drained on poll). No socket.
#[derive(Default)]
struct FakeTransport {
    announced: Mutex<Vec<Vec<u8>>>,
    inbox: Mutex<Vec<ReceivedAnnouncement>>,
}

impl FakeTransport {
    fn with_inbox(received: Vec<Vec<u8>>) -> Self {
        Self {
            announced: Mutex::new(Vec::new()),
            inbox: Mutex::new(
                received
                    .into_iter()
                    .map(ReceivedAnnouncement::new)
                    .collect(),
            ),
        }
    }

    fn announced_count(&self) -> usize {
        self.announced.lock().unwrap().len()
    }
}

impl MeshTransport for FakeTransport {
    fn announce(&self, wire: &[u8]) -> Result<(), MeshError> {
        self.announced.lock().unwrap().push(wire.to_vec());
        Ok(())
    }

    fn poll_received(&self) -> Result<Vec<ReceivedAnnouncement>, MeshError> {
        Ok(std::mem::take(&mut *self.inbox.lock().unwrap()))
    }
}

#[test]
fn a_round_announces_and_folds_received_peers_untrusted() {
    let transport = FakeTransport::with_inbox(vec![
        neighbour_wire(0xA1, ClaimState::Claimed),
        neighbour_wire(0xA2, ClaimState::Unclaimed),
    ]);
    let state = MeshState::new();
    let now = Duration::from_secs(10);

    let folded = announce_browse_step(&transport, &state, b"my-announce", now);
    assert_eq!(
        folded, 2,
        "both received announcements fold into the inventory"
    );
    assert_eq!(transport.announced_count(), 1, "the round announced once");

    let peers = state.peers();
    assert_eq!(peers.len(), 2);
    // Every discovered peer is UNTRUSTED: relaying_for_us is false (never auto).
    for peer in &peers {
        assert!(
            !peer.relaying_for_us,
            "a discovered peer is never auto-relayed (untrusted inventory)"
        );
    }
    // The claimed neighbour surfaces claimed=true.
    let claimed = state
        .peers()
        .into_iter()
        .find(|p| p.key == PeerKey::from_digest([0xA1; 32]));
    assert!(claimed.expect("0xA1 present").claimed);
}

#[test]
fn a_garbage_announcement_is_skipped_never_panics() {
    let transport = FakeTransport::with_inbox(vec![
        vec![0xFF, 0x00, 0x13, 0x37],
        neighbour_wire(0xB1, ClaimState::Claimed),
    ]);
    let state = MeshState::new();
    let folded = announce_browse_step(&transport, &state, b"x", Duration::from_secs(1));
    assert_eq!(
        folded, 1,
        "the garbage announcement is skipped; the valid one folds"
    );
    assert_eq!(state.peers().len(), 1);
}

#[test]
fn a_peer_that_stops_being_heard_ages_out_over_rounds() {
    let state = MeshState::new();
    // Round 1: hear a peer at t=0.
    let t0 = FakeTransport::with_inbox(vec![neighbour_wire(0xC1, ClaimState::Claimed)]);
    announce_browse_step(&t0, &state, b"x", Duration::from_secs(0));
    assert_eq!(state.peers().len(), 1);

    // Round 2: hear NOTHING, well past the staleness window → the peer ages out.
    let t1 = FakeTransport::with_inbox(vec![]);
    let later = PEER_STALE_AFTER + Duration::from_secs(1);
    announce_browse_step(&t1, &state, b"x", later);
    assert!(state.peers().is_empty(), "the un-refreshed peer aged out");
}

#[test]
fn status_reports_always_on_discovery_and_the_peer_count() {
    let transport = FakeTransport::with_inbox(vec![neighbour_wire(0xD1, ClaimState::Claimed)]);
    let state = MeshState::new();
    announce_browse_step(&transport, &state, b"x", Duration::from_secs(1));

    let status = state.status();
    // Discovery is ALWAYS-ON — the status can only ever report this; there is no
    // field or method to disable it.
    let json = serde_json::to_value(&status).expect("serialises");
    assert_eq!(json["discovery"], "always_on");
    assert_eq!(json["peers_count"], 1);
    assert_eq!(json["relay_enabled"], false, "relay is opt-out by default");
    assert_eq!(json["role"]["kind"], "direct", "online, no opt-in → direct");
}

#[test]
fn enabling_relay_changes_the_role_to_relay() {
    let state = MeshState::new();
    assert_eq!(state.role(), MeshRole::Direct);
    let now_enabled = state.set_relay_enabled(true);
    assert!(now_enabled);
    assert_eq!(state.role(), MeshRole::Relay, "online + opted-in → relay");
    // The status mirrors the toggle.
    assert!(state.status().relay_enabled);
}

#[test]
fn adopting_an_unknown_peer_is_refused_but_a_discovered_one_is_adopted() {
    let transport = FakeTransport::with_inbox(vec![neighbour_wire(0xE1, ClaimState::Claimed)]);
    let state = MeshState::new();
    announce_browse_step(&transport, &state, b"x", Duration::from_secs(1));

    // Adopting an unknown peer is refused (untrusted inventory + confirm-adopt:
    // you can only adopt a peer you have actually discovered).
    assert!(
        !state.adopt_relay(Some(PeerKey::from_digest([0xFF; 32]))),
        "an unknown peer cannot be adopted"
    );
    // Adopting the discovered peer succeeds and marks it relaying_for_us.
    let discovered = PeerKey::from_digest([0xE1; 32]);
    assert!(state.adopt_relay(Some(discovered.clone())));
    let peer = state
        .peers()
        .into_iter()
        .find(|p| p.key == discovered)
        .expect("present");
    assert!(
        peer.relaying_for_us,
        "the adopted peer is marked relaying_for_us"
    );
}
