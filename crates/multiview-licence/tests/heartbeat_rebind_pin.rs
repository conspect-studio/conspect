//! CONSPECT device REBIND/DEACTIVATE — the VERB-KEYED pinned-attempt money-path
//! defence (ADR-I009, round 2). The 3-lens panel found that a SINGLE untyped pin slot
//! shared across run_once/rebind_once/deactivate_once let the verbs contaminate each
//! other's pending state — so the automatic renew loop could consume a pinned rebind
//! attempt (posting a rebind body to /heartbeat), and a definitive /heartbeat rejection
//! could CLEAR the rebind pin → the operator's rebind retry mints a FRESH idempotency-
//! key → a SECOND charge against the scarce 3-free-per-AEST-year budget if the first
//! /rebind committed but its response was lost.
//!
//! These tests pin the fix: the pin is keyed by VERB in per-verb slots, so:
//!   * an ambiguous rebind's pin PERSISTS across a full background renew cycle (the
//!     renew never consumes or clears it), and the operator's rebind retry replays the
//!     SAME idempotency-key (no double charge);
//!   * no cross-verb replay — a renew never posts a rebind/deactivate body and vice
//!     versa (the wrong-verb body would fail to parse at the wrong endpoint).
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

fn config() -> HeartbeatConfig {
    HeartbeatConfig {
        org_id: "org_test".to_owned(),
        ..HeartbeatConfig::default()
    }
}

/// A BOUND device whose binding matches the fake's served binding — so BOTH the renew
/// path (run_once) AND the lifecycle ops (rebind/deactivate) have a binding to act on.
fn bound_identity() -> multiview_licence::heartbeat::DeviceIdentity {
    multiview_licence::heartbeat::DeviceIdentity {
        machine_id: "mch_7c2a1f04c9e75031".to_owned(),
        instance_id: "inst_prod_a".to_owned(),
        binding_id: Some(FakeLicenceServer::new().binding_id().to_owned()),
        fingerprint_digest: "a".repeat(64),
        fingerprint_score: 95,
        hardware_digest: "hwd_2b6e1c0e9b41".to_owned(),
        instance_discriminator_hash: "disc_9f3a2b6e1c0e".to_owned(),
        instance_discriminator_digest: "2".repeat(64),
        app_version: "0.1.0-test".to_owned(),
        device_public_key_b64url: String::new(),
    }
}

fn client(
    server: Arc<FakeLicenceServer>,
    store: Arc<LeaseStore>,
) -> HeartbeatClient<FakeLicenceServer> {
    let pinned = server.pinned_root();
    HeartbeatClient::with_device_signer(
        server,
        store,
        pinned,
        config(),
        bound_identity(),
        Arc::new(pop_test_signer()),
    )
}

/// THE MONEY-PATH GATE: an ambiguous /rebind leaves a rebind pin; a full background
/// renew cycle runs on the SAME client and does NOT consume or clear the rebind pin;
/// the operator's rebind retry replays the SAME idempotency-key (no second charge).
///
/// Must FAIL against the shared-pin implementation (the renew would consume/clear the
/// rebind pin → the rebind retry mints a fresh key) and PASS with the verb-keyed pin.
#[tokio::test]
async fn an_ambiguous_rebind_pin_survives_the_renew_loop_and_replays_same_key() {
    let server = shared_fake();
    let store = Arc::new(LeaseStore::new());
    let client = client(Arc::clone(&server), Arc::clone(&store));

    // 1. The first /rebind is an ambiguous lost-response (the fake RECORDS the key +
    //    body, then drops the response → Transport) → the rebind pin PERSISTS. (The fake
    //    records the idempotency-key on entry but only bumps `rebinds()` on a fully
    //    successful contact, so the recorded-keys log is the ground truth here.)
    server.set_fail_after_recording_idempotency(1);
    let first = client.rebind_once("lic_test").await;
    assert!(first.is_err(), "the lost-response rebind fails closed");
    let rebind_key_1 = {
        let keys = server.recorded_idempotency_keys();
        assert_eq!(
            keys.len(),
            1,
            "the /rebind reached the server (key recorded): {keys:?}"
        );
        keys[0].clone()
    };

    // 2. A FULL background renew cycle runs on the SAME client. It must NOT consume the
    //    pinned rebind attempt (post a rebind body to /heartbeat) and must NOT clear the
    //    rebind pin. It renews normally via /heartbeat. (If it had replayed the rebind
    //    body, the fake /heartbeat would fail to parse it as a HeartbeatRequest → the
    //    renew would error instead of Installed.)
    server.set_fail_after_recording_idempotency(0); // the renew contacts cleanly now
    let renew = tokio::time::timeout(Duration::from_secs(5), client.run_once())
        .await
        .expect("run_once must not hang")
        .expect("the renew must succeed via /heartbeat, not choke on a rebind body");
    assert!(
        matches!(renew, HeartbeatOutcome::Installed { .. }),
        "the renew installs a lease via /heartbeat, got {renew:?}"
    );
    assert_eq!(
        server.rebinds(),
        0,
        "the renew loop must NOT post the pinned rebind attempt to /rebind (no successful rebind)"
    );
    assert!(
        server.heartbeats.load(std::sync::atomic::Ordering::SeqCst) >= 1,
        "the renew posted a heartbeat"
    );
    // The renew minted + recorded its OWN (distinct) idempotency-key on /heartbeat.
    let after_renew = server.recorded_idempotency_keys();
    assert_eq!(
        after_renew.len(),
        2,
        "rebind + renew recorded: {after_renew:?}"
    );
    assert_ne!(
        after_renew[1], rebind_key_1,
        "the renew used its OWN fresh key, not the pinned rebind key: {after_renew:?}"
    );

    // 3. The operator's rebind RETRY on the same client replays the SAME idempotency-key
    //    (the persisted pin) — so if the first /rebind actually committed, the server
    //    dedups and the rebind budget is charged ONCE (no double charge).
    let _second = tokio::time::timeout(Duration::from_secs(5), client.rebind_once("lic_test"))
        .await
        .expect("must not hang");
    let rebind_keys = server.recorded_idempotency_keys();
    // Recorded order: [rebind (ambiguous), renew (heartbeat), rebind (retry)].
    let rebind_key_2 = rebind_keys.last().expect("a retry was recorded").clone();
    assert_eq!(
        rebind_key_1, rebind_key_2,
        "the rebind retry MUST replay the SAME idempotency-key (no double charge); keys={rebind_keys:?}"
    );
}

/// NO CROSS-VERB REPLAY: a pinned rebind attempt is never replayed by the renew path,
/// and a pinned deactivate attempt is never replayed by the renew path. The fake's
/// /heartbeat endpoint parses the body as a HeartbeatRequest, so if the renew wrongly
/// replayed a rebind/deactivate body it would either fail to parse (Malformed) or post
/// to the wrong verb — both observable. Here we assert the renew posts its OWN body and
/// the lifecycle pins are independent.
#[tokio::test]
async fn pins_do_not_cross_replay_between_verbs() {
    let server = shared_fake();
    let store = Arc::new(LeaseStore::new());
    let client = client(Arc::clone(&server), Arc::clone(&store));

    // Leave an ambiguous DEACTIVATE pin (recorded on entry; `deactivates()` not bumped
    // on the lost-response contact).
    server.set_fail_after_recording_idempotency(1);
    let _ = client.deactivate_once().await;
    let deact_key_1 = {
        let keys = server.recorded_idempotency_keys();
        assert_eq!(
            keys.len(),
            1,
            "the /deactivate reached the server: {keys:?}"
        );
        keys[0].clone()
    };
    assert_eq!(
        server.deactivates(),
        0,
        "the lost-response deactivate did not complete (counter not bumped)"
    );

    // A renew cycle runs cleanly — it must post a HEARTBEAT body to /heartbeat (parsed
    // as a HeartbeatRequest by the fake), NOT the pinned deactivate body. If it replayed
    // the deactivate body, the fake /heartbeat would reject it (Malformed parse) and the
    // renew would fail; assert it SUCCEEDS.
    server.set_fail_after_recording_idempotency(0);
    let renew = tokio::time::timeout(Duration::from_secs(5), client.run_once())
        .await
        .expect("no hang")
        .expect("the renew posts its OWN heartbeat body, never the pinned deactivate body");
    assert!(matches!(renew, HeartbeatOutcome::Installed { .. }));
    assert_eq!(
        server.deactivates(),
        0,
        "the renew loop must NOT post the pinned deactivate attempt to /deactivate"
    );

    // The deactivate pin still persists → the operator's deactivate retry replays the
    // SAME idempotency-key (the pin survived the renew's success — verb-keyed) and now
    // succeeds (the fake completes it).
    let retry = tokio::time::timeout(Duration::from_secs(5), client.deactivate_once())
        .await
        .expect("no hang");
    assert!(
        retry.is_ok(),
        "the deactivate retry replays the persisted pin and succeeds: {retry:?}"
    );
    assert_eq!(
        server.deactivates(),
        1,
        "the deactivate retry reached + completed /deactivate"
    );
    let keys = server.recorded_idempotency_keys();
    let deact_key_2 = keys.last().expect("retry recorded").clone();
    assert_eq!(
        deact_key_1, deact_key_2,
        "the deactivate retry replays the SAME idempotency-key (verb-keyed pin survived): {keys:?}"
    );
}

/// AUTO-SLOT VERB GATE (ADR-I009 r3): an ACTIVATE pin must NOT be replayed/posted by a
/// subsequent RENEW. Renew and Activate are both auto-path verbs; if binding state
/// changes between cycles (an unbound device's ambiguous ACTIVATE pin persists, then a
/// lease arrives via an install surface so the next cycle RENEWS), the renew must NOT
/// post the stale activate body to /heartbeat — it must mint a FRESH renew attempt.
///
/// Must FAIL against the blind-auto-slot-return src (it returns the activate pin to the
/// renew path → posts an ActivateRequest body to /heartbeat → the fake /heartbeat parse
/// fails → the renew errors) and PASS after the auto-slot is verb-gated.
#[tokio::test]
async fn an_activate_pin_is_not_replayed_by_a_subsequent_renew() {
    let server = shared_fake();
    let store = Arc::new(LeaseStore::new());
    // An UNBOUND device with activate ENABLED — the first cycle takes the ACTIVATE path.
    let pinned = server.pinned_root();
    let client = HeartbeatClient::with_device_signer(
        Arc::clone(&server),
        Arc::clone(&store),
        pinned,
        HeartbeatConfig {
            org_id: "org_test".to_owned(),
            enable_activate: true,
            ..HeartbeatConfig::default()
        },
        multiview_licence::heartbeat::DeviceIdentity {
            binding_id: None, // unbound → activate path
            ..bound_identity()
        },
        Arc::new(pop_test_signer()),
    );

    // 1. An ambiguous ACTIVATE (lost response) → the activate attempt is pinned in the
    //    shared `auto` slot.
    server.set_fail_after_recording_idempotency(1);
    let act = client.run_once().await;
    assert!(act.is_err(), "the lost-response activate fails closed");
    assert!(
        !server.recorded_idempotency_keys().is_empty(),
        "the activate reached the server"
    );

    // 2. A binding arrives via an install surface (the store now resolves a binding), so
    //    the NEXT cycle takes the RENEW path. Seed a verified lease into the store.
    let (binding, install_pinned) = fake::upload_binding_for(server.kit(), server.binding_id());
    store
        .install_binding(
            &binding,
            &install_pinned,
            multiview_licence::store::system_now(),
        )
        .expect("seed a binding so run_once renews");

    // 3. The RENEW cycle must NOT replay the pinned ACTIVATE body to /activate (or post an
    //    activate body to /heartbeat). It mints a FRESH renew attempt and renews cleanly.
    server.set_fail_after_recording_idempotency(0);
    let renew = tokio::time::timeout(Duration::from_secs(5), client.run_once())
        .await
        .expect("no hang")
        .expect("the renew must mint its OWN heartbeat attempt, not replay the activate pin");
    assert!(
        matches!(renew, HeartbeatOutcome::Installed { .. }),
        "the renew installs via /heartbeat, got {renew:?}"
    );
    assert_eq!(
        server.activates(),
        0,
        "the renew must NOT post the pinned activate attempt to /activate"
    );
    assert!(
        server.heartbeats.load(std::sync::atomic::Ordering::SeqCst) >= 1,
        "the renew posted a heartbeat (its own body, parsed OK)"
    );
}

/// A renew rejection must NOT clear a pending rebind pin (verb-scoped reset). An
/// ambiguous rebind leaves a pin; a renew that is DEFINITIVELY rejected (401 pop-invalid)
/// calls reset_on_rejection for the RENEW verb only — the rebind pin survives, so the
/// operator's rebind retry still replays the same key.
#[tokio::test]
async fn a_renew_rejection_does_not_clear_the_rebind_pin() {
    let server = shared_fake();
    let store = Arc::new(LeaseStore::new());
    let client = client(Arc::clone(&server), Arc::clone(&store));

    // Ambiguous rebind → rebind pin persists.
    server.set_fail_after_recording_idempotency(1);
    let _ = client.rebind_once("lic_test").await;
    let rebind_key_1 = server.recorded_idempotency_keys()[0].clone();

    // A renew that is DEFINITIVELY rejected (401 pop-invalid) — reset_on_rejection fires
    // for the RENEW verb. It must NOT clear the rebind pin.
    server.set_fail_after_recording_idempotency(0);
    server.set_reject_pop(true);
    let renew = client.run_once().await;
    assert!(renew.is_err(), "the renew is definitively rejected");
    server.set_reject_pop(false);

    // The operator's rebind retry still replays the SAME key (rebind pin survived the
    // renew's reset_on_rejection).
    let _ = tokio::time::timeout(Duration::from_secs(5), client.rebind_once("lic_test"))
        .await
        .expect("no hang");
    let keys = server.recorded_idempotency_keys();
    let rebind_key_2 = keys.last().expect("a rebind retry recorded").clone();
    assert_eq!(
        rebind_key_1, rebind_key_2,
        "a renew rejection must NOT clear the rebind pin; keys={keys:?}"
    );
}

/// WIRE (ADR-I009 r4): rebind_once must resolve the bindingId via the SAME chain as
/// renew + deactivate (configured/learned binding_id → store.current_binding_id() →
/// identity.binding_id), NOT from identity.binding_id with an instance_id fallback. A
/// device whose binding was LEARNED from the server (in the store, not the static
/// identity config) must rebind under the CORRECT store-learned bindingId.
///
/// Must FAIL against the identity+instance_id src (the request's bindingId would be the
/// device's instance_id) and PASS after rebind_once uses the store chain.
#[tokio::test]
async fn rebind_uses_the_store_learned_binding_not_the_instance_id() {
    let server = shared_fake();
    let store = Arc::new(LeaseStore::new());
    // An UNBOUND identity (binding_id: None) with a DISTINCT instance_id — so the only
    // correct binding source is the store-learned one.
    let pinned = server.pinned_root();
    let client = HeartbeatClient::with_device_signer(
        Arc::clone(&server),
        Arc::clone(&store),
        pinned,
        config(),
        multiview_licence::heartbeat::DeviceIdentity {
            binding_id: None, // NOT configured — must come from the store
            instance_id: "inst_prod_a".to_owned(),
            ..bound_identity()
        },
        Arc::new(pop_test_signer()),
    );

    // The binding is LEARNED from the server: seed a verified lease into the store so
    // store.current_binding_id() returns the fake's binding (an `ib_…`, NOT inst_prod_a).
    let store_binding = server.binding_id().to_owned();
    let (binding, install_pinned) = fake::upload_binding_for(server.kit(), &store_binding);
    store
        .install_binding(
            &binding,
            &install_pinned,
            multiview_licence::store::system_now(),
        )
        .expect("seed the store-learned binding");
    assert_eq!(
        store.current_binding_id().as_deref(),
        Some(store_binding.as_str()),
        "precondition: the store has the learned binding"
    );

    // Rebind: the request's bindingId MUST be the store-learned binding, NOT inst_prod_a.
    let _ = tokio::time::timeout(Duration::from_secs(5), client.rebind_once("lic_test"))
        .await
        .expect("no hang")
        .expect("a healthy rebind");
    assert_eq!(
        server.last_rebind_binding_id().as_deref(),
        Some(store_binding.as_str()),
        "rebind must target the store-learned bindingId"
    );
    assert_ne!(
        server.last_rebind_binding_id().as_deref(),
        Some("inst_prod_a"),
        "rebind must NOT fall back to the device's instance_id as the bindingId"
    );
}

/// PRECEDENCE — a genuinely-learned/store binding beats a PRESENT-but-stale configured id
/// (ADR-I009). The resolution order is genuinely-learned → store → configured; a stale
/// CONFIGURED `identity.binding_id` must NOT short-circuit a fresher learned/store binding.
/// This was a REAL bug (not a false positive): `learned_binding_id` used to be SEEDED from
/// `identity.binding_id`, so a stale configured id won — and even broke the renew path with
/// a `BindingMismatch`. The fix stops the pre-seed (learned holds only real server ids) and
/// shares `resolve_binding_id()` across renew/rebind/deactivate.
///
/// (a) A device with a PRESENT-but-stale configured `identity.binding_id` AND a fresher
/// store binding (`ib_fab_0001`): a renew RESOLVES the store binding (not the stale
/// configured — pre-fix this was a `BindingMismatch`) and LEARNS it; the subsequent rebind
/// then uses the learned binding, never the stale configured one.
#[tokio::test]
async fn rebind_prefers_the_learned_binding_over_a_present_stale_configured_id() {
    let server = shared_fake();
    let store = Arc::new(LeaseStore::new());
    let store_binding = server.binding_id().to_owned(); // ib_fab_0001
    let stale_configured = "ib_stale_configured_0000".to_owned();
    assert_ne!(
        stale_configured, store_binding,
        "the configured id must be DIFFERENT"
    );

    // The device has a PRESENT-but-stale configured binding id; the real binding is in
    // the store (seeded), and a renew will LEARN it.
    let (binding, install_pinned) = fake::upload_binding_for(server.kit(), &store_binding);
    store
        .install_binding(
            &binding,
            &install_pinned,
            multiview_licence::store::system_now(),
        )
        .expect("seed the store binding");
    let pinned = server.pinned_root();
    let client = HeartbeatClient::with_device_signer(
        Arc::clone(&server),
        Arc::clone(&store),
        pinned,
        config(),
        multiview_licence::heartbeat::DeviceIdentity {
            binding_id: Some(stale_configured.clone()), // PRESENT but stale
            instance_id: "inst_prod_a".to_owned(),
            ..bound_identity()
        },
        Arc::new(pop_test_signer()),
    );

    // A renew resolves the store binding (the stale configured does NOT win) and LEARNS it.
    let renew = tokio::time::timeout(Duration::from_secs(5), client.run_once())
        .await
        .expect("no hang")
        .expect("renew");
    assert!(matches!(renew, HeartbeatOutcome::Installed { .. }));
    assert_eq!(
        server.last_binding_id().as_deref(),
        Some(store_binding.as_str()),
        "the renew addressed the store binding, not the stale configured id"
    );

    // Now learned is set. A rebind must use the LEARNED binding, NOT the stale configured.
    let _ = tokio::time::timeout(Duration::from_secs(5), client.rebind_once("lic_test"))
        .await
        .expect("no hang")
        .expect("rebind");
    assert_eq!(
        server.last_rebind_binding_id().as_deref(),
        Some(store_binding.as_str()),
        "rebind must use the learned binding"
    );
    assert_ne!(
        server.last_rebind_binding_id().as_deref(),
        Some(stale_configured.as_str()),
        "a PRESENT-but-stale configured id must NOT short-circuit the learned/store binding"
    );
}

/// PRECEDENCE — (b) learned None + store present + a DIFFERENT present-configured id →
/// the STORE binding wins (configured is last in the chain). Pins store > configured.
#[tokio::test]
async fn rebind_prefers_the_store_binding_over_a_present_stale_configured_id() {
    let server = shared_fake();
    let store = Arc::new(LeaseStore::new());
    let store_binding = server.binding_id().to_owned(); // ib_fab_0001
    let stale_configured = "ib_stale_configured_0000".to_owned();
    assert_ne!(stale_configured, store_binding);

    // Seed ONLY the store (the client never activates/renews, so learned stays None).
    let (binding, install_pinned) = fake::upload_binding_for(server.kit(), &store_binding);
    store
        .install_binding(
            &binding,
            &install_pinned,
            multiview_licence::store::system_now(),
        )
        .expect("seed the store binding");
    assert_eq!(
        store.current_binding_id().as_deref(),
        Some(store_binding.as_str()),
        "precondition: the store has the binding anchor"
    );
    let pinned = server.pinned_root();
    let client = HeartbeatClient::with_device_signer(
        Arc::clone(&server),
        Arc::clone(&store),
        pinned,
        config(),
        multiview_licence::heartbeat::DeviceIdentity {
            binding_id: Some(stale_configured.clone()), // PRESENT but stale
            instance_id: "inst_prod_a".to_owned(),
            ..bound_identity()
        },
        Arc::new(pop_test_signer()),
    );

    // A rebind WITHOUT any prior learn must resolve via the store (NOT the configured id).
    let _ = tokio::time::timeout(Duration::from_secs(5), client.rebind_once("lic_test"))
        .await
        .expect("no hang")
        .expect("rebind");
    assert_eq!(
        server.last_rebind_binding_id().as_deref(),
        Some(store_binding.as_str()),
        "rebind must use the STORE binding (store > configured)"
    );
    assert_ne!(
        server.last_rebind_binding_id().as_deref(),
        Some(stale_configured.as_str()),
        "a PRESENT-but-stale configured id must NOT short-circuit the store binding"
    );
}
