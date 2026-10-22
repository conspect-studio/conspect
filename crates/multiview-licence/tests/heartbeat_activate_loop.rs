//! CONSPECT device ACTIVATE / enrolment — the `run_once` first-contact LOOP
//! integration (ADR-I008): a fresh, un-bound device with activate ENABLED enrols
//! online (fetch challenge → activate with a PoP proof bound to the server-assigned
//! instanceId → install the signed lease → learn the binding), then transitions to
//! the renew path (the activate response's `nextNonce` seeds steady state). Every
//! failure mode keeps last-good (never off air, inv #1/#10); with activate DISABLED
//! a fresh device is still `NoBinding` (renew-only, unchanged).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::as_conversions,
    clippy::doc_markdown,
    clippy::missing_panics_doc
)]
#![cfg(feature = "heartbeat")]

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use multiview_licence::heartbeat::{HeartbeatClient, HeartbeatConfig, HeartbeatOutcome};
use multiview_licence::{EnforcementLevel, LeaseStore};

mod fake;
use fake::{pop_test_signer, shared_fake, FakeLicenceServer};

/// A config with activate ENABLED (a fresh device enrols rather than no-op'ing).
fn activate_config() -> HeartbeatConfig {
    HeartbeatConfig {
        org_id: "org_test".to_owned(),
        enable_activate: true,
        ..HeartbeatConfig::default()
    }
}

/// A fresh, UN-BOUND device identity (no binding_id) — the enrolment case.
fn fresh_identity() -> multiview_licence::heartbeat::DeviceIdentity {
    multiview_licence::heartbeat::DeviceIdentity {
        machine_id: "mch_7c2a1f04c9e75031".to_owned(),
        instance_id: "inst_2b6e1c0e9b41".to_owned(),
        binding_id: None,
        fingerprint_digest: "1".repeat(64),
        fingerprint_score: 95,
        hardware_digest: "hwd_2b6e1c0e9b41".to_owned(),
        instance_discriminator_hash: "disc_9f3a2b6e1c0e".to_owned(),
        instance_discriminator_digest: "2".repeat(64),
        app_version: "0.1.0-test".to_owned(),
        device_public_key_b64url: String::new(),
    }
}

fn enrolling_client(
    server: Arc<FakeLicenceServer>,
    store: Arc<LeaseStore>,
) -> HeartbeatClient<FakeLicenceServer> {
    let pinned = server.pinned_root();
    HeartbeatClient::with_device_signer(
        server,
        store,
        pinned,
        activate_config(),
        fresh_identity(),
        Arc::new(pop_test_signer()),
    )
}

#[tokio::test]
async fn a_fresh_device_with_activate_enabled_enrols_and_installs_a_lease() {
    let server = shared_fake();
    let store = Arc::new(LeaseStore::new());
    let client = enrolling_client(Arc::clone(&server), Arc::clone(&store));

    // No binding yet, activate enabled → the device ACTIVATES (not NoBinding).
    let outcome = tokio::time::timeout(Duration::from_secs(5), client.run_once())
        .await
        .expect("run_once must not hang")
        .expect("a healthy first-contact activate must succeed");
    assert!(
        matches!(outcome, HeartbeatOutcome::Activated { .. }),
        "a fresh device must ACTIVATE, got {outcome:?}"
    );

    // It fetched a challenge and POSTed an activate whose PoP proof verified, binding
    // the SERVER-assigned instanceId.
    assert!(
        server.challenge_fetches() >= 1,
        "activate fetches a challenge"
    );
    assert_eq!(server.activates(), 1, "exactly one activate call");
    assert!(
        server.last_activate_pop_verified(),
        "the activate PoP proof must verify against the device key"
    );
    assert_eq!(
        server.last_activate_instance_id().as_deref(),
        Some(server.last_issued_instance_id().as_str()),
        "activate must echo the server-assigned instanceId"
    );
    assert!(
        server.last_activate_device_public_key().is_some(),
        "activate must carry devicePublicKey"
    );

    // A lease was installed and the device is now licensed-active.
    assert_eq!(
        store.status().unwrap().enforcement,
        EnforcementLevel::Active
    );
}

#[tokio::test]
async fn after_activate_the_device_renews_seeded_by_the_activate_next_nonce() {
    let server = shared_fake();
    let store = Arc::new(LeaseStore::new());
    let client = enrolling_client(Arc::clone(&server), Arc::clone(&store));

    // Cycle 1: activate (learns the binding + seeds nextNonce from the response).
    let first = tokio::time::timeout(Duration::from_secs(5), client.run_once())
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(first, HeartbeatOutcome::Activated { .. }));
    let challenges_after_activate = server.challenge_fetches();

    // Cycle 2: now there IS a binding → it RENEWS (heartbeat), and the activate
    // response's nextNonce is reused, so NO extra /challenge round-trip.
    let second = tokio::time::timeout(Duration::from_secs(5), client.run_once())
        .await
        .unwrap()
        .unwrap();
    assert!(
        matches!(second, HeartbeatOutcome::Installed { .. }),
        "the second cycle must RENEW via heartbeat, got {second:?}"
    );
    assert_eq!(
        server.heartbeats.load(Ordering::SeqCst),
        1,
        "the second cycle is a heartbeat renew"
    );
    assert_eq!(
        server.challenge_fetches(),
        challenges_after_activate,
        "steady-state renewal reuses the activate nextNonce — no new /challenge"
    );
}

#[tokio::test]
async fn activate_disabled_keeps_the_renew_only_no_binding_behaviour() {
    // With activate DISABLED (the default), a fresh un-bound device makes NO server
    // call and returns NoBinding (renew-only) — keep last-good, never off air.
    let server = shared_fake();
    let store = Arc::new(LeaseStore::new());
    let pinned = server.pinned_root();
    let client = HeartbeatClient::with_device_signer(
        Arc::clone(&server),
        Arc::clone(&store),
        pinned,
        HeartbeatConfig {
            org_id: "org_test".to_owned(),
            enable_activate: false,
            ..HeartbeatConfig::default()
        },
        fresh_identity(),
        Arc::new(pop_test_signer()),
    );
    let outcome = tokio::time::timeout(Duration::from_secs(5), client.run_once())
        .await
        .unwrap()
        .unwrap();
    assert!(
        matches!(outcome, HeartbeatOutcome::NoBinding { .. }),
        "renew-only: a fresh device with activate disabled is NoBinding, got {outcome:?}"
    );
    assert_eq!(server.activates(), 0, "no activate call when disabled");
    assert_eq!(
        server.challenge_fetches(),
        0,
        "no challenge fetch when disabled"
    );
    assert_eq!(
        server.heartbeats.load(Ordering::SeqCst),
        0,
        "no heartbeat call when disabled"
    );
}

#[tokio::test]
async fn activate_challenge_unreachable_keeps_last_good_and_does_not_panic() {
    // The challenge GET fails → no activate this cycle, keep last-good (never off
    // air). The device stays un-licensed-honest, NOT crashed.
    let server = shared_fake();
    server.set_fail_challenge(true);
    let store = Arc::new(LeaseStore::new());
    let client = enrolling_client(Arc::clone(&server), Arc::clone(&store));

    let result = tokio::time::timeout(Duration::from_secs(5), client.run_once())
        .await
        .expect("run_once must not hang");
    assert!(result.is_err(), "an unreachable challenge fails closed");
    assert_eq!(
        server.activates(),
        0,
        "no activate without a challenge nonce"
    );
    // Nothing was installed — the fresh device holds no lease (keep last-good: an
    // empty store stays empty, never a crash, never a forced tighten).
    assert!(
        store.status().is_none(),
        "a failed enrolment must install no lease"
    );
}

#[tokio::test]
async fn activate_rejected_pop_invalid_keeps_last_good() {
    // The server rejects the activate proof (401 pop-invalid) → ServerRejected, the
    // burned nonce is dropped, keep last-good, recover next cycle. Never off air.
    let server = shared_fake();
    server.set_reject_pop(true);
    let store = Arc::new(LeaseStore::new());
    let client = enrolling_client(Arc::clone(&server), Arc::clone(&store));

    let result = tokio::time::timeout(Duration::from_secs(5), client.run_once())
        .await
        .expect("run_once must not hang");
    assert!(result.is_err(), "a rejected activate proof fails closed");
    // The device is NOT licensed (nothing installed), but is not crashed.
    assert!(
        store.status().is_none(),
        "a rejected activate must install no lease"
    );
}
