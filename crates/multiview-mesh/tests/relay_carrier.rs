//! The relay carrier model (ADR-0051 §4, brief §9.2): the relayer is a **dumb
//! carrier**. It carries an end-to-end-signed lease binding (the FILE-exchange
//! artefact) between an offline originator and the licence server; it lacks both
//! keys, so it can neither read past the signed envelope nor forge/alter the
//! assertion. A tampered relayed payload fails the originator/server signature
//! check and is rejected. The relay queue is bounded drop-oldest (invariant #10).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use chrono::{DateTime, Utc};
use ed25519_dalek::rand_core::UnwrapErr;
use ed25519_dalek::{Signer, SigningKey};
use getrandom::SysRng;
use multiview_licence::entitlement::{
    Entitlement, EntitlementFlags, GpuLimit, HardwareClass, Tier,
};
use multiview_licence::lease::{Lease, LeaseSource};
use multiview_licence::store::{LeaseBinding, LeaseStore};
use multiview_licence::verify::{PinnedKey, SignedLease};
use multiview_licence::ACTIVATION_WINDOW_DAYS;
use multiview_mesh::peer::PeerKey;
use multiview_mesh::relay::{RelayConfig, RelayQueue, RelayedBinding, RELAY_QUEUE_CAP};

fn epoch() -> DateTime<Utc> {
    DateTime::from_timestamp(1_700_000_000, 0).unwrap()
}

/// A server-signed lease binding (the artefact the carrier passes through).
fn server_binding(server: &SigningKey, serial: &str, granted: DateTime<Utc>) -> LeaseBinding {
    let lease = Lease::new_full(
        serial.to_owned(),
        granted,
        LeaseSource::Relay,
        ACTIVATION_WINDOW_DAYS,
    );
    let sig = server.sign(&SignedLease::signing_bytes(&lease, None));
    LeaseBinding::new(
        SignedLease::new(lease.clone(), sig.to_bytes()),
        Entitlement::new(
            Tier::new("studio".to_owned()),
            HardwareClass::Standard,
            HardwareClass::Standard,
            GpuLimit::Limited(2),
            lease,
            EntitlementFlags::default(),
        ),
        100,
        None,
    )
}

fn origin(byte: u8) -> PeerKey {
    PeerKey::from_digest([byte; 32])
}

#[test]
fn a_relayed_binding_installs_at_the_destination_using_the_server_key() {
    // The destination machine pins the SERVER key (not the relayer's). The carrier
    // forwarded the server-signed binding verbatim; it installs because the
    // server's signature verifies — the relayer's identity is irrelevant.
    let server = SigningKey::generate(&mut UnwrapErr(SysRng));
    let pinned = PinnedKey::from_verifying_key(&server.verifying_key());
    let now = epoch();
    let carried = RelayedBinding::new(origin(0x09), server_binding(&server, "serial-RLY", now));

    let store = LeaseStore::with_clock(std::sync::Arc::new(move || now));
    let lease = store
        .install_binding(carried.binding(), &pinned, now)
        .expect("a server-signed relayed binding installs end-to-end");
    assert_eq!(lease.serial, "serial-RLY");
    assert_eq!(
        lease.source,
        LeaseSource::Relay,
        "the relayed grant is audited as Relay"
    );
}

#[test]
fn a_relayer_cannot_forge_an_assertion_it_lacks_the_server_key() {
    // The relayer signs with ITS OWN key (a forgery attempt). The destination
    // pins the SERVER key, so the forged binding fails verification — the relayer
    // is a dumb carrier with no authority.
    let server = SigningKey::generate(&mut UnwrapErr(SysRng));
    let relayer = SigningKey::generate(&mut UnwrapErr(SysRng)); // the malicious relayer's key
    let pinned = PinnedKey::from_verifying_key(&server.verifying_key());
    let now = epoch();

    // The relayer mints a binding with its own key (not the server's).
    let forged = server_binding(&relayer, "serial-FORGED", now);
    let carried = RelayedBinding::new(origin(0x0A), forged);

    let store = LeaseStore::with_clock(std::sync::Arc::new(move || now));
    let result = store.install_binding(carried.binding(), &pinned, now);
    assert!(
        result.is_err(),
        "a binding the relayer signed (not the server) must be rejected"
    );
}

#[test]
fn a_tampered_relayed_binding_is_rejected() {
    // The carrier (or a man-in-the-middle) flips a covered field after the server
    // signed it. The signature no longer matches → rejected at the destination.
    let server = SigningKey::generate(&mut UnwrapErr(SysRng));
    let pinned = PinnedKey::from_verifying_key(&server.verifying_key());
    let now = epoch();
    let mut binding = server_binding(&server, "serial-OK", now);
    // Tamper the signed serial after signing.
    binding.signed.lease.serial = "serial-EVIL".to_owned();
    let carried = RelayedBinding::new(origin(0x0B), binding);

    let store = LeaseStore::with_clock(std::sync::Arc::new(move || now));
    assert!(
        store
            .install_binding(carried.binding(), &pinned, now)
            .is_err(),
        "a tampered relayed binding fails the server signature check"
    );
}

#[test]
fn the_relay_queue_is_bounded_drop_oldest() {
    // A relayer enqueues at most RELAY_QUEUE_CAP neighbour requests; beyond that
    // the OLDEST is dropped (never grows — invariant #10). The newest survive.
    let server = SigningKey::generate(&mut UnwrapErr(SysRng));
    let now = epoch();
    let mut queue = RelayQueue::new();
    for i in 0..=RELAY_QUEUE_CAP {
        let serial = format!("serial-{i}");
        let carried = RelayedBinding::new(
            origin(u8::try_from(i % 256).unwrap_or(0)),
            server_binding(&server, &serial, now),
        );
        queue.push(carried);
    }
    assert_eq!(
        queue.len(),
        RELAY_QUEUE_CAP,
        "the queue never exceeds the cap"
    );
    // The oldest (serial-0) was dropped; the newest is retained.
    let drained: Vec<String> = queue
        .drain()
        .map(|c| c.binding().signed.lease.serial.clone())
        .collect();
    assert!(
        !drained.contains(&"serial-0".to_owned()),
        "the oldest was dropped"
    );
    assert!(
        drained.contains(&format!("serial-{RELAY_QUEUE_CAP}")),
        "the newest is retained"
    );
}

#[test]
fn relay_is_opt_out_by_default() {
    // A machine must explicitly opt IN to relay for neighbours (brief §9.2). The
    // default is decline — no neighbour traffic is carried unless enabled.
    let cfg = RelayConfig::default();
    assert!(
        !cfg.enabled,
        "relay is opt-out by default (a machine declines)"
    );
}

#[test]
fn the_carrier_exposes_the_origin_peer_for_audit_but_not_the_lease_contents() {
    // The carrier surfaces WHO it is relaying for (the origin peer digest, for the
    // operator's Mesh screen) but does not interpret the binding — it is opaque
    // bytes the carrier forwards.
    let server = SigningKey::generate(&mut UnwrapErr(SysRng));
    let now = epoch();
    let carried = RelayedBinding::new(origin(0x0C), server_binding(&server, "serial-AUD", now));
    assert_eq!(carried.origin(), &origin(0x0C));
}
