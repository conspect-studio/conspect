//! Peer-table behaviour (ADR-0051 §3, brief §9.1): discovered peers populate a
//! **bounded, untrusted** inventory keyed by salted digest; `last_seen` advances
//! on each observation; a peer that has not been seen within the staleness window
//! ages out; the table is capped (drop-oldest, invariant #10 — a flood can never
//! grow memory unbounded). A peer is never auto-trusted: `claimed`/`relaying_for_us`
//! are observed/operator flags, not auto-set.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::time::Duration;

use multiview_mesh::peer::{PeerKey, PeerObservation, PeerTable, PEER_STALE_AFTER, PEER_TABLE_CAP};
use multiview_mesh::ClaimState;

fn key(byte: u8) -> PeerKey {
    PeerKey::from_digest([byte; 32])
}

/// An observation of a peer at monotonic instant `t` (seconds), claimed or not.
fn obs(byte: u8, claim: ClaimState, t: u64) -> PeerObservation {
    PeerObservation {
        key: key(byte),
        claim_state: claim,
        observed_at: Duration::from_secs(t),
    }
}

#[test]
fn observing_a_peer_inserts_it_with_last_seen() {
    let mut table = PeerTable::new();
    table.observe(obs(0x01, ClaimState::Claimed, 10));
    let peer = table.get(&key(0x01)).expect("peer present after observe");
    assert_eq!(peer.last_seen, Duration::from_secs(10));
    assert!(peer.claimed, "claim_state Claimed surfaces as claimed=true");
    assert!(
        !peer.relaying_for_us,
        "a freshly-discovered peer is never auto-relaying (untrusted inventory)"
    );
}

#[test]
fn re_observing_advances_last_seen_and_does_not_duplicate() {
    let mut table = PeerTable::new();
    table.observe(obs(0x02, ClaimState::Unclaimed, 5));
    table.observe(obs(0x02, ClaimState::Claimed, 42));
    assert_eq!(table.len(), 1, "the same digest is one peer, not two");
    let peer = table.get(&key(0x02)).expect("peer present");
    assert_eq!(
        peer.last_seen,
        Duration::from_secs(42),
        "last_seen advances"
    );
    assert!(peer.claimed, "the claim state updates on re-observation");
}

#[test]
fn aging_removes_only_peers_past_the_staleness_window() {
    let mut table = PeerTable::new();
    table.observe(obs(0x03, ClaimState::Claimed, 100));
    table.observe(obs(0x04, ClaimState::Claimed, 100));
    // Advance to a `now` where 0x03 stays fresh but we then never see it again.
    // 0x04 keeps being seen.
    let now = Duration::from_secs(100) + PEER_STALE_AFTER + Duration::from_secs(1);
    table.observe(obs(0x04, ClaimState::Claimed, now.as_secs()));

    let removed = table.age_out(now);
    assert_eq!(removed, 1, "exactly the one un-refreshed peer ages out");
    assert!(table.get(&key(0x03)).is_none(), "the stale peer is gone");
    assert!(table.get(&key(0x04)).is_some(), "the refreshed peer stays");
}

#[test]
fn a_peer_exactly_at_the_window_is_not_yet_stale() {
    let mut table = PeerTable::new();
    table.observe(obs(0x05, ClaimState::Claimed, 0));
    // Exactly at the window boundary: still considered fresh (strictly-greater ages).
    let removed = table.age_out(PEER_STALE_AFTER);
    assert_eq!(
        removed, 0,
        "a peer exactly at the staleness window is not yet stale"
    );
    assert!(table.get(&key(0x05)).is_some());
}

#[test]
fn the_table_is_capped_drop_oldest() {
    let mut table = PeerTable::new();
    // Insert one more than the cap, each with a strictly increasing last_seen.
    for i in 0..=PEER_TABLE_CAP {
        let byte = u8::try_from(i % 256).unwrap_or(0);
        // Distinct digests: encode the index into two bytes so we exceed 256.
        let mut digest = [0_u8; 32];
        digest[0] = byte;
        digest[1] = u8::try_from((i / 256) % 256).unwrap_or(0);
        table.observe(PeerObservation {
            key: PeerKey::from_digest(digest),
            claim_state: ClaimState::Unclaimed,
            observed_at: Duration::from_secs(u64::try_from(i).unwrap_or(0)),
        });
    }
    assert_eq!(
        table.len(),
        PEER_TABLE_CAP,
        "the table is capped at PEER_TABLE_CAP; the oldest is evicted"
    );
    // The very first (oldest, last_seen=0) peer was evicted.
    let mut oldest = [0_u8; 32];
    oldest[0] = 0;
    oldest[1] = 0;
    assert!(
        table.get(&PeerKey::from_digest(oldest)).is_none(),
        "drop-oldest evicts the least-recently-seen peer"
    );
}

#[test]
fn marking_relay_is_an_explicit_operator_action() {
    // relaying_for_us is set ONLY by an explicit call (confirm-adopt semantics),
    // never by observation. A discovered peer starts untrusted.
    let mut table = PeerTable::new();
    table.observe(obs(0x06, ClaimState::Claimed, 1));
    assert!(!table.get(&key(0x06)).unwrap().relaying_for_us);
    let ok = table.set_relaying_for_us(&key(0x06), true);
    assert!(ok, "marking a known peer succeeds");
    assert!(table.get(&key(0x06)).unwrap().relaying_for_us);
    // Marking an unknown peer does nothing (no auto-insert of a trusted relayer).
    assert!(!table.set_relaying_for_us(&key(0xEE), true));
}

#[test]
fn the_peer_key_renders_as_hex_for_the_api_surface() {
    let k = PeerKey::from_digest([0xAB; 32]);
    let id = k.as_hex();
    assert_eq!(id.len(), 64, "32 bytes -> 64 hex chars");
    assert!(id.starts_with("abab"), "lowercase hex of the digest");
    assert!(
        id.chars().all(|c| c.is_ascii_hexdigit()),
        "the id is pure hex, never a raw identifier"
    );
}
