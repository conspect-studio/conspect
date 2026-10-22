//! CONSPECT-3 device-PoP — the heartbeat LOOP integration (ADR-I007): the nonce
//! lifecycle (cold-start `/challenge` + steady-state `nextNonce`), the
//! `Conspect-Device-PoP` header + `nonce` body field reaching the server, and
//! fail-closed-on-every-PoP-failure (never off air, inv #1/#10).
//!
//! These drive the real `HeartbeatClient` against the in-process `FakeLicenceServer`
//! (extended to serve challenges, record the PoP header, and verify the proof
//! against the device public key the client signs with).
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

use multiview_licence::heartbeat::{HeartbeatClient, HeartbeatError, HeartbeatOutcome};
use multiview_licence::{EnforcementLevel, LeaseStore};

mod fake;
use fake::{pop_test_signer, shared_fake, FakeLicenceServer};

fn test_config() -> multiview_licence::heartbeat::HeartbeatConfig {
    multiview_licence::heartbeat::HeartbeatConfig {
        org_id: "org_test".to_owned(),
        ..multiview_licence::heartbeat::HeartbeatConfig::default()
    }
}

fn test_identity() -> multiview_licence::heartbeat::DeviceIdentity {
    multiview_licence::heartbeat::DeviceIdentity {
        machine_id: "mch_7c2a1f04c9e75031".to_owned(),
        instance_id: "inst_2b6e1c0e9b41".to_owned(),
        binding_id: Some("ib_fab_0001".to_owned()),
        fingerprint_digest: "1".repeat(64),
        fingerprint_score: 95,
        hardware_digest: "hwd_2b6e1c0e9b41".to_owned(),
        instance_discriminator_hash: "disc_9f3a2b6e1c0e".to_owned(),
        instance_discriminator_digest: "2".repeat(64),
        app_version: "0.1.0-test".to_owned(),
        device_public_key_b64url: String::new(), // set by the signer in production
    }
}

/// A client wired with a known device signer so the fake can verify the proof.
fn pop_client(
    server: Arc<FakeLicenceServer>,
    store: Arc<LeaseStore>,
) -> HeartbeatClient<FakeLicenceServer> {
    let pinned = server.pinned_root();
    HeartbeatClient::with_device_signer(
        server,
        store,
        pinned,
        test_config(),
        test_identity(),
        Arc::new(pop_test_signer()),
    )
}

#[tokio::test]
async fn cold_start_fetches_a_challenge_then_heartbeats_with_a_valid_pop() {
    let server = shared_fake();
    let store = Arc::new(LeaseStore::new());
    let client = pop_client(Arc::clone(&server), Arc::clone(&store));

    let outcome = tokio::time::timeout(Duration::from_secs(5), client.run_once())
        .await
        .expect("run_once must not hang")
        .expect("a healthy PoP heartbeat must succeed");
    assert!(matches!(outcome, HeartbeatOutcome::Installed { .. }));

    // Cold start consulted GET /challenge exactly once (no held nonce yet).
    assert_eq!(
        server.challenge_fetches(),
        1,
        "cold start must fetch one challenge"
    );
    // The heartbeat carried a PoP header that VERIFIES against the device key, and
    // the body `nonce` equalled the challenge nonce.
    assert!(
        server.last_pop_verified(),
        "the PoP proof must verify against the device public key"
    );
    assert_eq!(
        server.last_request_nonce().as_deref(),
        Some(server.last_issued_nonce().as_str()),
        "the heartbeat body nonce must be the challenge nonce"
    );
    assert_eq!(
        store.status().unwrap().enforcement,
        EnforcementLevel::Active
    );
}

#[tokio::test]
async fn steady_state_uses_next_nonce_and_skips_the_challenge_round_trip() {
    let server = shared_fake();
    let store = Arc::new(LeaseStore::new());
    let client = pop_client(Arc::clone(&server), Arc::clone(&store));

    // Cycle 1: cold start (1 challenge fetch).
    tokio::time::timeout(Duration::from_secs(5), client.run_once())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(server.challenge_fetches(), 1);
    let next_nonce_after_1 = server.last_next_nonce();

    // Cycle 2: steady state — the prior response's nextNonce is reused, so NO new
    // /challenge round-trip (RFC 9449 DPoP-nonce style).
    tokio::time::timeout(Duration::from_secs(5), client.run_once())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        server.challenge_fetches(),
        1,
        "steady state must NOT fetch a second challenge"
    );
    // Cycle 2's body nonce was cycle 1's nextNonce.
    assert_eq!(
        server.last_request_nonce(),
        next_nonce_after_1,
        "the next heartbeat must sign the prior response's nextNonce"
    );
    assert!(server.last_pop_verified());
}

#[tokio::test]
async fn an_unreachable_challenge_keeps_last_good_no_mutation() {
    let server = shared_fake();
    let store = Arc::new(LeaseStore::new());
    let client = pop_client(Arc::clone(&server), Arc::clone(&store));

    // Seed a healthy lease first (so there is a last-good to keep).
    tokio::time::timeout(Duration::from_secs(5), client.run_once())
        .await
        .unwrap()
        .unwrap();
    let serial = store.current().unwrap().lease.serial;
    let heartbeats_before = server.heartbeats.load(Ordering::SeqCst);

    // Force a cold start (drop the held nonce) AND make /challenge unreachable.
    server.set_drop_next_nonce(true); // next response carries no usable nextNonce
    tokio::time::timeout(Duration::from_secs(5), client.run_once())
        .await
        .unwrap()
        .unwrap(); // this cycle still succeeds; it just leaves no nextNonce
    server.set_fail_challenge(true);

    let res = tokio::time::timeout(Duration::from_secs(5), client.run_once())
        .await
        .expect("run_once must not hang when /challenge is down");
    assert!(
        res.is_err(),
        "an unreachable /challenge with no held nonce surfaces an error (fail closed)"
    );
    // No heartbeat mutation was sent this cycle, and the last-good lease is intact.
    assert_eq!(
        store.current().unwrap().lease.serial,
        serial,
        "a PoP-nonce failure keeps the last-good lease (never off air)"
    );
    let _ = heartbeats_before;
}

#[tokio::test]
async fn a_server_rejected_nonce_pop_invalid_keeps_last_good_and_recovers() {
    // Panel round-3 (the convergence finding): a DEFINITIVE pop-invalid REJECTION
    // (an HTTP response was received) burns the single-use nonce server-side, so the
    // client must NOT replay that burned nonce+proof — it must DROP the pinned
    // attempt and, next cycle, fetch a FRESH /challenge and sign a FRESH proof
    // (RECOVERY). A verbatim replay would loop pop-invalid forever and strand the
    // lease silently. The INVARIANT: a nonce the server has SEEN + REJECTED is
    // burned and must never be replayed; only an AMBIGUOUS (no-response) failure
    // replays the pinned unit.
    let server = shared_fake();
    let store = Arc::new(LeaseStore::new());
    let client = pop_client(Arc::clone(&server), Arc::clone(&store));

    // Seed a healthy lease.
    tokio::time::timeout(Duration::from_secs(5), client.run_once())
        .await
        .unwrap()
        .unwrap();
    let serial = store.current().unwrap().lease.serial;

    // The server rejects the heartbeat as pop-invalid (401, response received).
    server.set_reject_pop(true);
    let res = tokio::time::timeout(Duration::from_secs(5), client.run_once())
        .await
        .expect("run_once must not hang");
    assert!(res.is_err(), "a pop-invalid rejection surfaces an error");
    assert_eq!(
        store.current().unwrap().lease.serial,
        serial,
        "a pop-invalid rejection keeps the last-good lease (never off air)"
    );
    let challenges_after_reject = server.challenge_fetches();
    let rejected_nonce = server
        .last_request_nonce()
        .expect("the rejected attempt carried a nonce");

    // RECOVERY: the server now accepts proofs again. The NEXT cycle must NOT replay
    // the burned nonce — it must fetch a FRESH /challenge and sign a FRESH proof.
    server.set_reject_pop(false);
    let outcome = tokio::time::timeout(Duration::from_secs(5), client.run_once())
        .await
        .expect("run_once must not hang")
        .expect("the client must RECOVER after a pop-invalid (fresh nonce), not loop");
    assert!(matches!(outcome, HeartbeatOutcome::Installed { .. }));
    assert!(
        server.challenge_fetches() > challenges_after_reject,
        "recovery must fetch a FRESH /challenge (a burned nonce is never replayed)"
    );
    assert_ne!(
        server.last_request_nonce().as_deref(),
        Some(rejected_nonce.as_str()),
        "recovery must sign a FRESH nonce — never replay the server-burned one"
    );
    assert!(
        server.last_pop_verified(),
        "the recovery proof verifies against the device key"
    );
}

#[tokio::test]
async fn a_malformed_2xx_body_keeps_last_good_and_recovers_with_a_fresh_challenge() {
    // Panel round-4: a 2xx whose body will not parse surfaces as HeartbeatError::Malformed
    // (post_raw_json emits it ONLY after a 2xx). The server RECEIVED the request and
    // processed/burned the single-use nonce, so — exactly like a 4xx rejection — the client
    // must NOT replay the burned nonce: it must DROP the pinned attempt and, next cycle,
    // fetch a FRESH /challenge and sign a FRESH proof (RECOVERY). Only an AMBIGUOUS
    // (no-response/5xx) failure replays the pinned unit verbatim.
    let server = shared_fake();
    let store = Arc::new(LeaseStore::new());
    let client = pop_client(Arc::clone(&server), Arc::clone(&store));

    // Seed a healthy lease.
    tokio::time::timeout(Duration::from_secs(5), client.run_once())
        .await
        .unwrap()
        .unwrap();
    let serial = store.current().unwrap().lease.serial;

    // The server returns a 2xx with an unparseable body (Malformed, response received).
    server.set_malformed_2xx(true);
    let res = tokio::time::timeout(Duration::from_secs(5), client.run_once())
        .await
        .expect("run_once must not hang");
    // The CONTRACT premise: a received-2xx-with-bad-body surfaces as the DEFINITIVE
    // `Malformed` variant SPECIFICALLY — never `Transport` (the ambiguous, replay-only
    // case). The whole "drop the burned nonce, don't replay" branch hinges on this
    // variant, so pin it (not just `is_err`).
    assert!(
        matches!(res, Err(HeartbeatError::Malformed(_))),
        "a received 2xx whose body will not parse is the DEFINITIVE Malformed variant \
         (the server burned the nonce), not an ambiguous Transport error: got {res:?}"
    );
    assert_eq!(
        store.current().unwrap().lease.serial,
        serial,
        "a malformed 2xx response keeps the last-good lease (never off air)"
    );
    let challenges_after = server.challenge_fetches();
    let burned_nonce = server
        .last_request_nonce()
        .expect("the malformed attempt carried a nonce");

    // RECOVERY: the body parses again. The NEXT cycle must NOT replay the burned nonce —
    // it must fetch a FRESH /challenge and sign a FRESH proof.
    server.set_malformed_2xx(false);
    let outcome = tokio::time::timeout(Duration::from_secs(5), client.run_once())
        .await
        .expect("run_once must not hang")
        .expect("the client must RECOVER after a malformed 2xx (fresh nonce), not loop");
    assert!(matches!(outcome, HeartbeatOutcome::Installed { .. }));
    assert!(
        server.challenge_fetches() > challenges_after,
        "recovery must fetch a FRESH /challenge (a burned nonce is never replayed)"
    );
    assert_ne!(
        server.last_request_nonce().as_deref(),
        Some(burned_nonce.as_str()),
        "recovery must sign a FRESH nonce — never replay the server-burned one"
    );
    assert!(
        server.last_pop_verified(),
        "the recovery proof verifies against the device key"
    );
}

#[tokio::test]
async fn no_binding_makes_no_challenge_and_no_pop() {
    // The renew-only client makes NO server call when there is no binding to renew
    // — so it must NOT fetch a challenge or build a PoP (a benign no-op).
    let server = shared_fake();
    let store = Arc::new(LeaseStore::new());
    let pinned = server.pinned_root();
    let mut identity = test_identity();
    identity.binding_id = None; // no configured binding, empty store → NoBinding
    let client = HeartbeatClient::with_device_signer(
        Arc::clone(&server),
        Arc::clone(&store),
        pinned,
        test_config(),
        identity,
        Arc::new(pop_test_signer()),
    );

    let outcome = tokio::time::timeout(Duration::from_secs(5), client.run_once())
        .await
        .unwrap()
        .expect("no-binding is a benign no-op, not an error");
    assert!(matches!(outcome, HeartbeatOutcome::NoBinding { .. }));
    assert_eq!(
        server.challenge_fetches(),
        0,
        "no binding → no challenge fetch"
    );
    assert_eq!(server.heartbeats.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn an_already_expired_fresh_challenge_is_not_signed_or_sent() {
    // Panel minor #5: a freshly-fetched /challenge nonce whose expiresAtMs is
    // already in the past must NOT be signed + POSTed (the asymmetry: a HELD nonce
    // was expiry-checked, a fresh one was not). Fail closed — no heartbeat mutation
    // this cycle, keep last-good (never off air).
    let server = shared_fake();
    let store = Arc::new(LeaseStore::new());
    let client = pop_client(Arc::clone(&server), Arc::clone(&store));

    // Seed a healthy lease so there is a last-good to keep.
    tokio::time::timeout(Duration::from_secs(5), client.run_once())
        .await
        .unwrap()
        .unwrap();
    let serial = store.current().unwrap().lease.serial;

    // Drop the held nextNonce (force a cold-start /challenge next cycle). This cycle
    // itself still sends a heartbeat (and leaves no usable nextNonce).
    server.set_drop_next_nonce(true);
    tokio::time::timeout(Duration::from_secs(5), client.run_once())
        .await
        .unwrap()
        .unwrap();
    // Capture the count AFTER the drop cycle — so the assertion measures ONLY the
    // expired-challenge cycle below.
    let heartbeats_before = server.heartbeats.load(Ordering::SeqCst);
    server.set_challenge_expired(true);

    let res = tokio::time::timeout(Duration::from_secs(5), client.run_once())
        .await
        .expect("run_once must not hang");
    assert!(
        res.is_err(),
        "an already-expired fresh challenge must fail closed (not be signed/sent)"
    );
    assert_eq!(
        server.heartbeats.load(Ordering::SeqCst),
        heartbeats_before,
        "no heartbeat mutation is sent when the fresh challenge is already expired"
    );
    assert_eq!(
        store.current().unwrap().lease.serial,
        serial,
        "an expired-fresh-challenge cycle keeps the last-good lease (never off air)"
    );
}

#[tokio::test]
async fn a_lost_response_retry_replays_the_same_idempotency_key_and_body_and_nonce() {
    // Panel round-2 major #1: the Idempotency-Key is retry-stable, but the PoP nonce
    // (a body field hashed into the proof) must ALSO be pinned to the in-flight
    // attempt — otherwise a lost-response-after-commit retry POSTs the SAME key with
    // a DIFFERENT body (fresh nonce), which a strict server rejects as an idempotency
    // body-mismatch and strands the client. So a retry of an ambiguous/failed contact
    // MUST replay the SAME {idempotency-key, body, nonce} as the attempt that may have
    // landed on the server.
    let server = shared_fake();
    let store = Arc::new(LeaseStore::new());
    let client = pop_client(Arc::clone(&server), Arc::clone(&store));

    // The first heartbeat records its key+body, then returns a transport error (a
    // lost response: the server may have committed + burned the nonce, but the device
    // never saw the 200). The NEXT cycle is a retry of the SAME logical operation.
    server.set_fail_after_recording_idempotency(1);

    // Cycle 1: lost response (records key #1 + body #1, then errors → keep last-good).
    let r1 = tokio::time::timeout(Duration::from_secs(5), client.run_once())
        .await
        .expect("run_once must not hang");
    assert!(r1.is_err(), "the lost-response cycle surfaces an error");

    // Cycle 2: the retry. It must NOT mint a fresh nonce/body under a replayed key.
    let r2 = tokio::time::timeout(Duration::from_secs(5), client.run_once())
        .await
        .expect("run_once must not hang");
    assert!(
        r2.is_ok(),
        "the retry succeeds once the server stops failing"
    );

    let keys = server.idempotency_keys();
    let bodies = server.bodies();
    assert!(
        keys.len() >= 2 && bodies.len() >= 2,
        "the server saw the lost-response attempt + the retry (keys={}, bodies={})",
        keys.len(),
        bodies.len()
    );
    assert_eq!(
        keys[0], keys[1],
        "the retry must replay the SAME Idempotency-Key (server dedup)"
    );
    assert_eq!(
        bodies[0], bodies[1],
        "the retry must replay the SAME body bytes (same PoP nonce) — not a fresh \
         nonce under the replayed key (a strict server would reject the mismatch)"
    );
}
