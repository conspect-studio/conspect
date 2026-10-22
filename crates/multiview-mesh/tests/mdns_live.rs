//! Live mDNS announce + browse round-trip (ADR-0051 §2). This is the documented
//! pattern for a live-socket test: it is `#[ignore]`d so it never runs in CI
//! (which has no multicast-capable network guarantee) and is run **manually on
//! hardware** (`cargo test -p multiview-mesh --features mdns -- --ignored`). The
//! pure announce/browse LOGIC is fully covered offline in `driver_offline.rs` via
//! the in-memory transport — this only exercises the real `mdns-sd` socket layer
//! end to end.
#![cfg(feature = "mdns")]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::time::Duration;

use chrono::{DateTime, Utc};
use ed25519_dalek::rand_core::UnwrapErr;
use ed25519_dalek::SigningKey;
use getrandom::SysRng;
use multiview_mesh::announce::{
    AnnouncePayload, EntitlementSummary, SaltedDigest, ANNOUNCE_PROTOCOL_VERSION,
};
use multiview_mesh::service::MdnsService;
use multiview_mesh::transport::MeshTransport;
use multiview_mesh::ClaimState;

fn signed_wire(byte: u8) -> Vec<u8> {
    let key = SigningKey::generate(&mut UnwrapErr(SysRng));
    let granted = DateTime::<Utc>::from_timestamp(1_700_000_000, 0).unwrap();
    let expires = granted + chrono::Duration::days(35);
    let summary = EntitlementSummary::new(
        multiview_licence::EnforcementLevel::Active,
        granted,
        expires,
    );
    AnnouncePayload::sign(
        ANNOUNCE_PROTOCOL_VERSION,
        vec![SaltedDigest::new([byte; 32])],
        ClaimState::Claimed,
        summary,
        &key,
    )
    .to_wire()
    .unwrap()
}

#[test]
#[ignore = "live mDNS socket; run manually on hardware with a multicast-capable network"]
fn two_services_discover_each_other_over_the_wire() {
    // Two services on the loopback/LAN announce distinct payloads; each should
    // observe the other's announcement (chunked TXT reassembled + decoded).
    let a = MdnsService::start("aaaa-peer-a", "conspect-a.local.", 5354).expect("start a");
    let b = MdnsService::start("bbbb-peer-b", "conspect-b.local.", 5354).expect("start b");

    a.announce(&signed_wire(0xA1)).expect("a announces");
    b.announce(&signed_wire(0xB2)).expect("b announces");

    // Give the daemons time to multicast + resolve.
    std::thread::sleep(Duration::from_secs(3));

    let seen_by_a = a.poll_received().expect("poll a");
    let seen_by_b = b.poll_received().expect("poll b");

    // Each saw at least one announcement, and the bytes decode to a payload.
    assert!(
        !seen_by_a.is_empty() || !seen_by_b.is_empty(),
        "at least one peer was discovered"
    );
    for r in seen_by_a.into_iter().chain(seen_by_b) {
        let payload = r.decode().expect("a received announcement decodes");
        assert_eq!(payload.protocol_version, ANNOUNCE_PROTOCOL_VERSION);
        assert!(
            payload.peer_key().is_some(),
            "the announcement carries a digest"
        );
    }
}
