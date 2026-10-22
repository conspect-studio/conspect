//! CONSPECT device REBIND + DEACTIVATE — the one-shot `rebind_once` /
//! `deactivate_once` LOOP integration (ADR-I009). These exercise the operator-invoked
//! lifecycle methods on a bound device against the in-process fake:
//!   * a healthy rebind fetches a challenge, POSTs a PoP-proofed rebind, seeds the
//!     steady-state nonce from the response, and does NOT install from the response
//!     (RebindResponse carries only a serial) — the next renew installs the lease;
//!   * a healthy deactivate fetches a challenge, POSTs a PoP-proofed deactivate, and
//!     installs NOTHING (the local last-good lease ages naturally — never off air);
//!   * every failure keeps last-good (never off air, inv #1/#10): an unreachable
//!     challenge, a 401 pop-invalid (reset + recover), a 409 in-progress (REPLAY the
//!     SAME idempotency-key, never a second charge), and an ambiguous transport (the
//!     pinned attempt replays verbatim on the same running client).
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

use std::sync::Arc;
use std::time::Duration;

use multiview_licence::heartbeat::{HeartbeatClient, HeartbeatConfig, HeartbeatOutcome};
use multiview_licence::LeaseStore;

mod fake;
use fake::{pop_test_signer, shared_fake, FakeLicenceServer};

fn lifecycle_config() -> HeartbeatConfig {
    HeartbeatConfig {
        org_id: "org_test".to_owned(),
        ..HeartbeatConfig::default()
    }
}

/// A BOUND device identity whose `binding_id` is the fake's binding (so the rebind /
/// deactivate continuity ops address the established binding the device holds).
fn bound_identity() -> multiview_licence::heartbeat::DeviceIdentity {
    multiview_licence::heartbeat::DeviceIdentity {
        machine_id: "mch_7c2a1f04c9e75031".to_owned(),
        instance_id: "inst_prod_a".to_owned(),
        binding_id: Some(fake::FakeLicenceServer::new().binding_id().to_owned()),
        fingerprint_digest: "a".repeat(64),
        fingerprint_score: 95,
        hardware_digest: "hwd_2b6e1c0e9b41".to_owned(),
        instance_discriminator_hash: "disc_9f3a2b6e1c0e".to_owned(),
        instance_discriminator_digest: "2".repeat(64),
        app_version: "0.1.0-test".to_owned(),
        device_public_key_b64url: String::new(),
    }
}

fn bound_client(
    server: Arc<FakeLicenceServer>,
    store: Arc<LeaseStore>,
) -> HeartbeatClient<FakeLicenceServer> {
    let pinned = server.pinned_root();
    HeartbeatClient::with_device_signer(
        server,
        store,
        pinned,
        lifecycle_config(),
        bound_identity(),
        Arc::new(pop_test_signer()),
    )
}

#[tokio::test]
async fn a_healthy_rebind_succeeds_and_seeds_the_next_nonce() {
    let server = shared_fake();
    let store = Arc::new(LeaseStore::new());
    let client = bound_client(Arc::clone(&server), Arc::clone(&store));

    let outcome = tokio::time::timeout(
        Duration::from_secs(5),
        client.rebind_once("lic_8d3b2a1f04c9e750"),
    )
    .await
    .expect("rebind_once must not hang")
    .expect("a healthy rebind must succeed");
    assert!(
        matches!(
            outcome,
            HeartbeatOutcome::Rebound {
                seat_consumed: false,
                ..
            }
        ),
        "a rebind consumes no new seat, got {outcome:?}"
    );

    assert!(
        server.challenge_fetches() >= 1,
        "rebind fetches a challenge"
    );
    assert_eq!(server.rebinds(), 1, "exactly one rebind call");
    assert!(
        server.last_rebind_pop_verified(),
        "the rebind PoP proof must verify against the device key"
    );
    // The rebind PoP binds the device's OWN instance id (continuity), not a server id.
    assert_eq!(
        server.last_rebind_instance_id().as_deref(),
        Some("inst_prod_a"),
        "rebind sends the device's own instance_id"
    );
    // RebindResponse carries only a serial → the client installs NOTHING from it.
    assert!(
        store.status().is_none(),
        "rebind_once must NOT install a lease from the response (only a serial)"
    );
}

#[tokio::test]
async fn after_rebind_the_next_renew_installs_the_refreshed_lease() {
    let server = shared_fake();
    let store = Arc::new(LeaseStore::new());
    let client = bound_client(Arc::clone(&server), Arc::clone(&store));

    // Rebind seeds the steady-state nonce from the response.
    let _ = client
        .rebind_once("lic_8d3b2a1f04c9e750")
        .await
        .expect("rebind");
    let challenges_after_rebind = server.challenge_fetches();

    // The next renew uses the seeded nextNonce (no extra /challenge) and installs the
    // refreshed lease via the unchanged renew chokepoint.
    let renew = tokio::time::timeout(Duration::from_secs(5), client.run_once())
        .await
        .unwrap()
        .expect("renew after rebind");
    assert!(
        matches!(renew, HeartbeatOutcome::Installed { .. }),
        "the renew after rebind installs the refreshed lease, got {renew:?}"
    );
    assert_eq!(
        server.challenge_fetches(),
        challenges_after_rebind,
        "the renew reuses the rebind nextNonce — no new /challenge"
    );
    assert!(
        store.status().is_some(),
        "the refreshed lease is installed by the renew (continuity gate cleared by the post-rebind score)"
    );
}

#[tokio::test]
async fn a_healthy_deactivate_returns_released_and_installs_nothing() {
    let server = shared_fake();
    let store = Arc::new(LeaseStore::new());
    let client = bound_client(Arc::clone(&server), Arc::clone(&store));

    let outcome = tokio::time::timeout(Duration::from_secs(5), client.deactivate_once())
        .await
        .expect("deactivate_once must not hang")
        .expect("a healthy deactivate must succeed");
    match outcome {
        HeartbeatOutcome::Deactivated {
            lifecycle_state, ..
        } => {
            assert_eq!(
                lifecycle_state, "released",
                "a deactivated binding is released"
            );
        }
        other => panic!("expected Deactivated, got {other:?}"),
    }
    assert_eq!(server.deactivates(), 1, "exactly one deactivate call");
    assert!(
        server.last_deactivate_pop_verified(),
        "the deactivate PoP proof must verify"
    );
    // Deactivate installs NOTHING and removes NOTHING — the local lease ages naturally.
    assert!(
        store.status().is_none(),
        "deactivate must not touch the store (never off air; the local lease ages out)"
    );
}

#[tokio::test]
async fn deactivate_is_idempotent_replaying_the_same_idempotency_key() {
    // A 409 "still in progress" must REPLAY the SAME idempotency-key (not drop +
    // mint fresh = a second logical op). The fake forces a lost-response on the
    // first contact, so the retry must carry the SAME key + body.
    let server = shared_fake();
    server.set_fail_after_recording_idempotency(1);
    let store = Arc::new(LeaseStore::new());
    let client = bound_client(Arc::clone(&server), Arc::clone(&store));

    // First call: the server records the key then drops the response (Transport).
    let first = client.deactivate_once().await;
    assert!(first.is_err(), "the lost-response contact fails closed");

    // Re-invoke on the SAME running client: it must replay the SAME idempotency-key.
    let _second = tokio::time::timeout(Duration::from_secs(5), client.deactivate_once())
        .await
        .expect("must not hang");
    let keys = server.recorded_idempotency_keys();
    assert!(keys.len() >= 2, "the retry made a second recorded contact");
    assert_eq!(
        keys[0], keys[1],
        "an ambiguous failure must replay the SAME idempotency-key (idempotent, no double charge): {keys:?}"
    );
}

#[tokio::test]
async fn rebind_challenge_unreachable_keeps_last_good_and_does_not_panic() {
    let server = shared_fake();
    server.set_fail_challenge(true);
    let store = Arc::new(LeaseStore::new());
    let client = bound_client(Arc::clone(&server), Arc::clone(&store));

    let result = tokio::time::timeout(
        Duration::from_secs(5),
        client.rebind_once("lic_8d3b2a1f04c9e750"),
    )
    .await
    .expect("rebind_once must not hang");
    assert!(result.is_err(), "an unreachable challenge fails closed");
    assert_eq!(server.rebinds(), 0, "no rebind without a challenge nonce");
    assert!(store.status().is_none(), "a failed rebind installs nothing");
}

#[tokio::test]
async fn rebind_rejected_pop_invalid_keeps_last_good() {
    // A 401 pop-invalid → ServerRejected: the burned nonce is dropped, keep last-good.
    let server = shared_fake();
    server.set_reject_pop(true);
    let store = Arc::new(LeaseStore::new());
    let client = bound_client(Arc::clone(&server), Arc::clone(&store));

    let result = tokio::time::timeout(
        Duration::from_secs(5),
        client.rebind_once("lic_8d3b2a1f04c9e750"),
    )
    .await
    .expect("rebind_once must not hang");
    assert!(result.is_err(), "a rejected rebind proof fails closed");
    assert!(
        store.status().is_none(),
        "a rejected rebind installs nothing"
    );
}

#[tokio::test]
async fn deactivate_rejected_pop_invalid_keeps_last_good() {
    let server = shared_fake();
    server.set_reject_pop(true);
    let store = Arc::new(LeaseStore::new());
    let client = bound_client(Arc::clone(&server), Arc::clone(&store));

    let result = tokio::time::timeout(Duration::from_secs(5), client.deactivate_once())
        .await
        .expect("deactivate_once must not hang");
    assert!(result.is_err(), "a rejected deactivate proof fails closed");
    assert!(
        store.status().is_none(),
        "a rejected deactivate touches nothing"
    );
}
