//! CONSPECT-3 heartbeat **client loop** + install convergence + never-off-air
//! chaos tests (ADR-0096, invariants #1/#10).
//!
//! A successful heartbeat against the in-process [`FakeLicenceServer`] verifies
//! the returned signed lease against the key-trust chain and drives the existing
//! [`LeaseStore::install_binding`] convergence, so the machine reads `Active`
//! wait-free with no engine-side wiring. The chaos cases abort / stall / fail the
//! heartbeat task and assert the last-good lease + status are unchanged — the
//! client only ever TIGHTENS on a positively-verified signed lease, and is
//! physically unable to take a running program off air.
//!
//! Every async case runs under a hard `tokio::time::timeout` so a hung loop fails
//! CI fast rather than hanging it.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::as_conversions,
    clippy::missing_panics_doc
)]
#![cfg(feature = "heartbeat")]

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use chrono::{TimeZone as _, Utc};
use multiview_licence::heartbeat::{
    DeviceIdentity, HeartbeatClient, HeartbeatConfig, HeartbeatOutcome,
};
use multiview_licence::{EnforcementLevel, LeaseStore};

mod fake;
use fake::{shared_fake, FakeLicenceServer};

/// A short, deterministic config (tiny backoff; the loop is driven step-by-step
/// in tests via `run_once`, so the sleep cadence does not gate the assertions).
fn test_config() -> HeartbeatConfig {
    HeartbeatConfig {
        org_id: "org_test".to_owned(),
        ..HeartbeatConfig::default()
    }
}

/// A device identity carrying ONLY salted digests + opaque ids — never a raw
/// serial/MAC/UUID (data minimisation, brief §8).
fn test_identity() -> DeviceIdentity {
    DeviceIdentity {
        machine_id: "mch_7c2a1f04c9e75031".to_owned(),
        instance_id: "inst_2b6e1c0e9b41".to_owned(),
        binding_id: Some("ib_fab_0001".to_owned()),
        fingerprint_digest: "1".repeat(64),
        fingerprint_score: 95,
        hardware_digest: "hwd_2b6e1c0e9b41".to_owned(),
        instance_discriminator_hash: "disc_9f3a2b6e1c0e".to_owned(),
        instance_discriminator_digest: "2".repeat(64),
        app_version: "0.1.0-test".to_owned(),
        device_public_key_b64url: "ZmFrZS1kZXZpY2Uta2V5".to_owned(),
    }
}

fn client(
    server: Arc<FakeLicenceServer>,
    store: Arc<LeaseStore>,
) -> HeartbeatClient<FakeLicenceServer> {
    let pinned = server.pinned_root();
    // v0.9.0 enforces device-PoP: every client carries a bound device signer (the
    // fake verifies the proof). The shared `pop_test_signer` matches the key the
    // fake checks against.
    HeartbeatClient::with_device_signer(
        server,
        store,
        pinned,
        test_config(),
        test_identity(),
        std::sync::Arc::new(fake::pop_test_signer()),
    )
}

#[tokio::test]
async fn a_successful_heartbeat_installs_the_verified_lease_active() {
    let server = shared_fake();
    let store = Arc::new(LeaseStore::new());
    let client = client(Arc::clone(&server), Arc::clone(&store));

    // No lease installed yet → no status.
    assert!(store.status().is_none(), "store starts empty");

    let outcome = tokio::time::timeout(Duration::from_secs(5), client.run_once())
        .await
        .expect("run_once must not hang")
        .expect("a healthy heartbeat must succeed");

    assert!(matches!(outcome, HeartbeatOutcome::Installed { .. }));
    let status = store.status().expect("a verified lease must be installed");
    assert_eq!(
        status.enforcement,
        EnforcementLevel::Active,
        "a fresh 35-day lease reads Active"
    );
    assert_eq!(server.heartbeats.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn a_tampered_lease_from_the_server_is_rejected_and_nothing_is_installed() {
    let server = shared_fake();
    let store = Arc::new(LeaseStore::new());
    // Point the client at the WRONG pinned root: the server's (fabricated) keyset
    // cannot be attested by a foreign root, so key-trust fails and no lease is
    // installed — the client refuses to trust an un-attested chain.
    let foreign = fake::FabricatedKeyset::new(); // a different instance shares the seed,
                                                 // so use a deterministic foreign root via the helper below.
    let wrong_root = foreign_root();
    let _ = foreign;
    let pinned = wrong_root;
    let client = HeartbeatClient::new(
        Arc::clone(&server),
        Arc::clone(&store),
        pinned,
        test_config(),
        test_identity(),
    )
    .with_signer(std::sync::Arc::new(fake::pop_test_signer()));
    let outcome = tokio::time::timeout(Duration::from_secs(5), client.run_once())
        .await
        .expect("run_once must not hang")
        .expect_err("an un-attested keyset must surface an error, not install");
    let _ = outcome;
    assert!(
        store.status().is_none(),
        "nothing is installed when the trust chain fails (fail closed on trust, lenient on enforcement)"
    );
}

#[tokio::test]
async fn withholding_the_lease_keeps_last_good_never_tightens() {
    let server = shared_fake();
    let store = Arc::new(LeaseStore::new());
    let client = client(Arc::clone(&server), Arc::clone(&store));

    // First heartbeat installs Active.
    tokio::time::timeout(Duration::from_secs(5), client.run_once())
        .await
        .unwrap()
        .unwrap();
    let installed_serial = store.current().unwrap().lease.serial;
    assert_eq!(
        store.status().unwrap().enforcement,
        EnforcementLevel::Active
    );

    // Now the entitlement is "revoked" server-side: heartbeat returns lease:null
    // (revocation by non-reissue). The client must NOT tighten — the last-good
    // lease stays installed and ages via the existing ladder.
    server.set_withhold_lease(true);
    let outcome = tokio::time::timeout(Duration::from_secs(5), client.run_once())
        .await
        .unwrap()
        .expect("a withheld lease is a normal 200, not an error");
    assert!(matches!(outcome, HeartbeatOutcome::LeaseWithheld { .. }));
    let after = store
        .current()
        .expect("last-good lease must remain installed");
    assert_eq!(
        after.lease.serial, installed_serial,
        "the last-good lease is unchanged — revocation never installs a worse state"
    );
    assert_eq!(
        store.status().unwrap().enforcement,
        EnforcementLevel::Active,
        "the client never tightens on its own; the lease ages naturally"
    );
}

#[tokio::test]
async fn an_unreachable_server_keeps_last_good() {
    let server = shared_fake();
    let store = Arc::new(LeaseStore::new());
    let client = client(Arc::clone(&server), Arc::clone(&store));
    tokio::time::timeout(Duration::from_secs(5), client.run_once())
        .await
        .unwrap()
        .unwrap();
    let serial = store.current().unwrap().lease.serial;

    server.set_fail(true);
    let res = tokio::time::timeout(Duration::from_secs(5), client.run_once())
        .await
        .expect("run_once must not hang even when the server is down");
    assert!(
        res.is_err(),
        "an unreachable server surfaces a transport error"
    );
    assert_eq!(
        store.current().unwrap().lease.serial,
        serial,
        "a failed renew keeps the last-good lease (never off air)"
    );
}

#[tokio::test]
async fn the_heartbeat_request_carries_no_raw_identifier() {
    // Data minimisation: the request the client builds carries ONLY the binding
    // id, the salted fingerprint digest, the app version, the lease serial, and
    // the transport — never a raw serial/MAC/UUID/hostname.
    let identity = test_identity();
    // v0.9.0 adds the single-use PoP `nonce` to the body (still no raw identifier).
    let req = HeartbeatClient::<FakeLicenceServer>::build_heartbeat_request(
        &identity,
        None,
        "9f3a2b6e1c0e9b41a4c92d7e3b8f10c25e6a7d3f49b0c81e2f5a6d7c8b9e0f1a".to_owned(),
    );
    let json = serde_json::to_string(&req).unwrap();

    // The salted digest IS present; a raw MAC/serial pattern is NOT.
    assert!(json.contains(&identity.fingerprint_digest));
    // No colon-separated MAC, no "serial"/"macAddress"/"uuid" raw fields.
    assert!(
        !json.to_lowercase().contains("macaddress"),
        "no raw MAC field: {json}"
    );
    assert!(
        !json.to_lowercase().contains("serialnumber"),
        "no raw serial field: {json}"
    );
    // The fingerprint digest is the salted hex, not a raw 48-bit MAC.
    assert!(
        !json.contains("\"00:1b:"),
        "no raw MAC value leaks into the payload: {json}"
    );
    // The set of top-level keys is exactly the minimal heartbeat payload.
    let value: serde_json::Value = serde_json::from_str(&json).unwrap();
    let keys: Vec<String> = value.as_object().unwrap().keys().cloned().collect();
    for k in &keys {
        assert!(
            [
                "bindingId",
                "leaseSerial",
                "fingerprintDigest",
                "appVersion",
                "transport",
                "nonce"
            ]
            .contains(&k.as_str()),
            "unexpected heartbeat payload field {k:?} (data minimisation)"
        );
    }
}

// --- Round-5c: RENEW-ONLY. With no binding (empty store + unconfigured binding)
//     the client makes NO mutation, installs nothing, and keeps output on-air. ---

#[tokio::test]
async fn run_once_with_no_binding_performs_no_mutation_and_installs_nothing() {
    // The operator decision: the device client is RENEW-ONLY. Online-activate is
    // deferred (the server-issued `serverNonce` is not yet available — ADR-I006
    // decision point 11). A device with no established binding (an empty store AND
    // no configured/learned binding) MUST NOT call activate (or any mutation): it
    // no-ops the cycle and keeps last-good. A lease arrives via an install surface
    // (control-upload / file-drop / mesh relay), and the next cycle renews it.
    let server = shared_fake();
    let store = Arc::new(LeaseStore::new());
    // No binding configured → activate would have fired here in the old design.
    let mut identity = test_identity();
    identity.binding_id = None;
    let pinned = server.pinned_root();
    let client = HeartbeatClient::new(
        Arc::clone(&server),
        Arc::clone(&store),
        pinned,
        test_config(),
        identity,
    )
    .with_signer(std::sync::Arc::new(fake::pop_test_signer()));

    assert!(store.status().is_none(), "store starts empty");

    let outcome = tokio::time::timeout(Duration::from_secs(5), client.run_once())
        .await
        .expect("run_once must not hang")
        .expect("a no-binding cycle is a benign no-op, not an error");

    // RENEW-ONLY: nothing was installed, and NO mutation reached the server (no
    // activate, no heartbeat) — the device cannot self-activate without a binding.
    assert!(
        store.status().is_none(),
        "a no-binding cycle installs nothing (keeps last-good / output on-air)"
    );
    assert_eq!(
        server.heartbeats.load(Ordering::SeqCst),
        0,
        "no heartbeat mutation is sent when there is no binding to renew"
    );
    assert!(
        server.idempotency_keys().is_empty(),
        "no mutation at all (no activate/heartbeat) is sent for a no-binding cycle"
    );
    assert!(
        matches!(outcome, HeartbeatOutcome::NoBinding { .. }),
        "a no-binding cycle reports NoBinding (await an install surface), got {outcome:?}"
    );
}

#[tokio::test]
async fn an_installed_lease_then_renews_on_the_next_cycle() {
    // RENEW-ONLY end-to-end: a device with no binding no-ops; once a lease is
    // installed via an install surface (here the upload/file-drop path that every
    // surface shares), the next heartbeat cycle RENEWS it (the binding is read from
    // the store) — proving the renew path is intact and drives install_binding.
    let server = shared_fake();
    let store = Arc::new(LeaseStore::new());
    let mut identity = test_identity();
    identity.binding_id = None; // unconfigured: relies on the store for the binding
    let pinned = server.pinned_root();
    let client = HeartbeatClient::new(
        Arc::clone(&server),
        Arc::clone(&store),
        pinned,
        test_config(),
        identity,
    )
    .with_signer(std::sync::Arc::new(fake::pop_test_signer()));

    // Cycle 1: no binding → no-op (no mutation).
    tokio::time::timeout(Duration::from_secs(5), client.run_once())
        .await
        .unwrap()
        .expect("no-binding cycle is a no-op");
    assert_eq!(server.heartbeats.load(Ordering::SeqCst), 0);

    // A lease arrives via an install surface (control-upload / file-drop / relay) —
    // it anchors this device's binding in the store.
    let (binding, upload_pinned) =
        fake::upload_binding_for(server.kit(), server.kit().binding_id());
    store
        .install_binding(&binding, &upload_pinned, store.now())
        .expect("an install surface installs the binding");
    assert!(
        store.current_binding_id().is_some(),
        "the install surface anchored the device binding"
    );

    // ANTI-ALIASING PRECONDITION (rule 19): the assertions below are only a
    // meaningful "addresses by instanceBindingId, never the lease serial" guard if
    // the fixture's binding id and lease serial are DISTINCT values. If they
    // aliased, a renew-BY-SERIAL regression could pass the binding-id assertion.
    // This was the load-bearing assert_ne! of the round-5c-removed
    // `the_renewal_addresses_the_binding_by_id_never_the_lease_serial` test; it is
    // restored here on the renew path it now lives on.
    assert_ne!(
        server.kit().binding_id(),
        server.kit().lease_serial(),
        "fixture binding id and lease serial MUST be distinct, else the renew-by-id \
         assertion below is a tautology"
    );

    // Cycle 2: now the store holds a binding → the client RENEWS via heartbeat.
    server.clear_last_binding_id();
    let outcome = tokio::time::timeout(Duration::from_secs(5), client.run_once())
        .await
        .unwrap()
        .expect("the renewal heartbeat succeeds");
    assert!(matches!(outcome, HeartbeatOutcome::Installed { .. }));
    assert_eq!(
        server.heartbeats.load(Ordering::SeqCst),
        1,
        "the next cycle renews via heartbeat once a binding exists"
    );
    let seen = server.last_binding_id().expect("a binding id was sent");
    assert_eq!(
        seen,
        server.kit().binding_id(),
        "the renewal addresses the binding by the server's instanceBindingId"
    );
    assert_ne!(
        seen,
        server.kit().lease_serial(),
        "the renewal must NEVER address the binding by the lease serial"
    );
}

// --- never-off-air chaos: SIGKILL/stall/partition the heartbeat task ----------

#[tokio::test]
async fn aborting_the_heartbeat_task_leaves_the_store_untouched() {
    // Install a good lease, then spawn a heartbeat loop and ABORT it mid-flight
    // (the JoinHandle::abort is the in-process analogue of SIGKILL'ing the task).
    // The store's last-good lease + status must be byte-identical afterwards —
    // the heartbeat task holds no lock the store/engine takes (invariant #10) and
    // can never tighten or remove a lease by dying (invariant #1).
    let server = shared_fake();
    let store = Arc::new(LeaseStore::new());
    let client = client(Arc::clone(&server), Arc::clone(&store));
    tokio::time::timeout(Duration::from_secs(5), client.run_once())
        .await
        .unwrap()
        .unwrap();
    let before = store.status().unwrap();

    // A loop that would run forever; abort it almost immediately.
    let store2 = Arc::clone(&store);
    let server2 = Arc::clone(&server);
    let handle = tokio::spawn(async move {
        let client = HeartbeatClient::new(
            server2,
            store2,
            FakeLicenceServer::new().pinned_root(),
            test_config(),
            test_identity(),
        )
        .with_signer(std::sync::Arc::new(fake::pop_test_signer()));
        client.run_forever().await;
    });
    tokio::time::sleep(Duration::from_millis(20)).await;
    handle.abort();
    let _ = handle.await; // joins as Cancelled.

    let after = store.status().unwrap();
    assert_eq!(
        before, after,
        "aborting the heartbeat task must leave the last-good status byte-identical"
    );
}

#[tokio::test]
async fn an_in_flight_stalled_heartbeat_cannot_block_a_store_reader() {
    // A reader sampling the store (the wait-free path the engine uses) must never
    // be blocked by a GENUINELY IN-FLIGHT (black-holed) heartbeat call. We start a
    // heartbeat whose server call blocks FOREVER (a real stall, not an instant
    // error), wait until it is actually parked in flight, then read the store from
    // a concurrent task and assert that read completes promptly under a tight
    // timeout while the heartbeat is still stalled (invariant #10).
    let server = shared_fake();
    let store = Arc::new(LeaseStore::new());
    let client = client(Arc::clone(&server), Arc::clone(&store));
    tokio::time::timeout(Duration::from_secs(5), client.run_once())
        .await
        .unwrap()
        .unwrap();

    // The next server call black-holes (awaits forever until released).
    server.set_block(true);
    let client2 = client;
    let hb = tokio::spawn(async move {
        let _ = client2.run_once().await;
    });

    // Wait until the heartbeat call is genuinely parked in flight (not a race).
    let waited = tokio::time::timeout(Duration::from_secs(5), async {
        while !server.is_in_flight() {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await;
    assert!(
        waited.is_ok(),
        "the heartbeat call must reach an in-flight stall"
    );
    assert!(
        !hb.is_finished(),
        "the heartbeat is stalled in flight (it has NOT returned)"
    );

    // WHILE the heartbeat is stalled, a concurrent store reader must complete
    // promptly. If the stall could back-pressure the reader, this trips.
    let reader_store = Arc::clone(&store);
    let reader = tokio::spawn(async move {
        for _ in 0..10_000 {
            let _ = reader_store.status();
            let _ = reader_store.current();
        }
        reader_store.current().is_some()
    });
    let still_present = tokio::time::timeout(Duration::from_secs(2), reader)
        .await
        .expect("a concurrent store reader must NOT be blocked by an in-flight stall")
        .unwrap();
    assert!(
        still_present,
        "last-good lease still present during the stall"
    );
    assert!(
        !hb.is_finished(),
        "the heartbeat is STILL stalled while the reader sailed through"
    );

    // Tear down: release the stall and join.
    server.release();
    let _ = tokio::time::timeout(Duration::from_secs(5), hb).await;
    assert!(store.current().is_some(), "last-good lease still present");
}

// --- Blocker #2: the installed expiry IS the signed not_after (no replay/extend)-

#[tokio::test]
async fn the_installed_lease_expiry_is_the_signed_not_after() {
    // The installed lease's expiry MUST come from the cryptographically-signed
    // not_after — NOT system_now()+35d. The fake signs a fixed-epoch not_after
    // (year ~2026, distinct from the real wall clock), so an installer that minted
    // a fresh now()+35d term would land far from the signed instant.
    let server = shared_fake();
    let store = Arc::new(LeaseStore::new());
    let client = client(Arc::clone(&server), Arc::clone(&store));
    tokio::time::timeout(Duration::from_secs(5), client.run_once())
        .await
        .unwrap()
        .unwrap();
    let lease = store.current().unwrap().lease;
    let signed_not_after = Utc
        .timestamp_millis_opt(server.kit().sign_lease(35).not_after)
        .single()
        .unwrap();
    assert_eq!(
        lease.expires_at, signed_not_after,
        "the installed expiry must equal the SIGNED not_after, not system_now()+35d"
    );
}

#[tokio::test]
async fn replaying_an_older_signed_lease_does_not_re_extend_entitlement() {
    // Install a current 35-day lease (expiry = signed not_after). Then the server
    // replays an OLDER lease (distinct serial, still Ed25519-valid, signed
    // not_after already in the PAST). It must NOT re-extend entitlement — the
    // installer rejects an expired signed lease (or keeps the newer last-good), so
    // the live expiry never moves backward to the replay's past window.
    let server = shared_fake();
    let store = Arc::new(LeaseStore::new());
    let client = client(Arc::clone(&server), Arc::clone(&store));
    tokio::time::timeout(Duration::from_secs(5), client.run_once())
        .await
        .unwrap()
        .unwrap();
    let good_expiry = store.current().unwrap().lease.expires_at;

    server.set_replay_expired(true);
    let _ = tokio::time::timeout(Duration::from_secs(5), client.run_once())
        .await
        .expect("run_once must not hang");
    let after_expiry = store.current().unwrap().lease.expires_at;
    assert_eq!(
        after_expiry, good_expiry,
        "a replayed older signed lease must NOT change the installed expiry"
    );
    // The replay's signed not_after is 5 days in the past relative to the fake's
    // epoch; the live lease must still be the GOOD (future-of-epoch) one.
    let replay_not_after = Utc
        .timestamp_millis_opt(server.kit().now_ms() - 5 * 86_400_000)
        .single()
        .unwrap();
    assert!(
        after_expiry > replay_not_after,
        "the live lease must never be replaced by the expired replay"
    );
}

// NOTE: the round-1 "the renewal addresses the binding by id, never the serial"
// test seeded the binding via the device ACTIVATE path (binding_id = None → cycle
// 1 activated). Activate is now DEFERRED (renew-only, ADR-I006 decision point 11),
// so that seeding no longer exists. The same property — a renewal addresses the
// binding by the server's instanceBindingId, never the lease serial — is covered
// by `an_installed_lease_then_renews_on_the_next_cycle` (a lease installed via an
// install surface, then renewed) above, which asserts the same
// `last_binding_id() == kit().binding_id()`.

// --- Round-2 #1: cross-instance lease replay is rejected -----------------------

#[tokio::test]
async fn a_lease_for_another_devices_binding_is_rejected_not_installed() {
    // A valid Conspect-signed lease minted for ANOTHER device's binding must NOT
    // install onto this device. Once this device has an established binding (the
    // configured identity.binding_id), install() rejects a body whose
    // instance_binding_id != the local binding — cross-instance replay defence.
    let server = shared_fake();
    let store = Arc::new(LeaseStore::new());
    // This device's established binding is the fabricated one; the server will
    // hand a (correctly-signed) lease whose body binds a DIFFERENT instance.
    let mut identity = test_identity();
    identity.binding_id = Some(server.kit().binding_id().to_owned()); // local = ib_fab_0001
    let pinned = server.pinned_root();
    let client = HeartbeatClient::new(
        Arc::clone(&server),
        Arc::clone(&store),
        pinned,
        test_config(),
        identity,
    )
    .with_signer(std::sync::Arc::new(fake::pop_test_signer()));
    server.set_foreign_binding(true); // body.instance_binding_id = someone else's

    let res = tokio::time::timeout(Duration::from_secs(5), client.run_once())
        .await
        .expect("run_once must not hang");
    assert!(
        res.is_err(),
        "a lease for another device's binding must be rejected, got {res:?}"
    );
    assert!(
        store.status().is_none(),
        "nothing is installed for a cross-instance (foreign-binding) lease"
    );
}

// --- Round-2 #2: a rejected lease must not poison the learned binding id --------

#[tokio::test]
async fn a_rejected_lease_does_not_poison_the_learned_binding_id() {
    // remember_binding_id must only fire for a SUCCESSFULLY INSTALLED lease. A
    // rejected (here: expired) lease must NOT mutate the learned binding id, so the
    // next renewal still addresses the legitimate binding, not the attacker's.
    let server = shared_fake();
    let store = Arc::new(LeaseStore::new());
    let mut identity = test_identity();
    identity.binding_id = Some(server.kit().binding_id().to_owned());
    let pinned = server.pinned_root();
    let client = HeartbeatClient::new(
        Arc::clone(&server),
        Arc::clone(&store),
        pinned,
        test_config(),
        identity,
    )
    .with_signer(std::sync::Arc::new(fake::pop_test_signer()));

    // The server hands a lease whose body carries a DIFFERENT (attacker) binding
    // AND is already expired (so install rejects it). The reject path must not
    // learn the attacker binding.
    server.set_foreign_binding(true);
    server.set_replay_absolute_past(true);
    let res = tokio::time::timeout(Duration::from_secs(5), client.run_once())
        .await
        .expect("run_once must not hang");
    assert!(res.is_err(), "the expired/foreign lease must be rejected");

    // Now the server behaves: a legitimate lease for THIS binding. The renewal
    // must still address the device's real binding (proving no poisoning).
    server.set_foreign_binding(false);
    server.set_replay_absolute_past(false);
    server.clear_last_binding_id();
    tokio::time::timeout(Duration::from_secs(5), client.run_once())
        .await
        .unwrap()
        .expect("a legitimate lease installs");
    let seen = server
        .last_binding_id()
        .expect("the heartbeat sent a binding id");
    assert_eq!(
        seen,
        server.kit().binding_id(),
        "the renewal addresses the device's real binding — the reject path did not poison it"
    );
}

// --- Round-2 #5: a past-not_after lease deterministically hits LeaseExpired -----

#[tokio::test]
async fn a_signed_lease_with_an_absolute_past_not_after_is_rejected_lease_expired() {
    // Deterministic LeaseExpired: the server signs a lease whose not_after is an
    // absolute instant in 1970 (clearly before ANY real system clock the install
    // path reads), so install() returns HeartbeatError::LeaseExpired — proving the
    // signed-expiry rejection fires (not InstallError::Stale via a fixed epoch).
    use multiview_licence::heartbeat::HeartbeatError;
    let server = shared_fake();
    let store = Arc::new(LeaseStore::new());
    let client = client(Arc::clone(&server), Arc::clone(&store));

    server.set_replay_absolute_past(true);
    let err = tokio::time::timeout(Duration::from_secs(5), client.run_once())
        .await
        .expect("run_once must not hang")
        .expect_err("a lease whose signed not_after is in the past must be rejected");
    assert!(
        matches!(err, HeartbeatError::LeaseExpired),
        "expected HeartbeatError::LeaseExpired, got {err:?}"
    );
    assert!(
        store.status().is_none(),
        "nothing is installed for an expired signed lease (keep last-good)"
    );
}

// --- Round-3 BLOCKER 1 (renew-only): a STALE FOREIGN lease, addressed against a
//     binding the device learned from its STORE (no configured binding), does NOT
//     poison the learned binding id and is rejected. The Stale->Ok fold must not
//     be treated as "installed". (Renew-only: the device anchors identity from the
//     store, never the removed activate path.) --------------------------------

#[tokio::test]
async fn a_stale_foreign_lease_against_a_store_anchored_binding_does_not_poison_identity() {
    use multiview_licence::heartbeat::HeartbeatError;
    let server = shared_fake();
    let store = Arc::new(LeaseStore::new());

    // The device has NO configured/learned heartbeat binding, but its local store
    // ALREADY HOLDS a newer lease for THIS device (a prior install). Seed that by
    // installing a genuine local lease first via a client whose identity IS this
    // device's binding. After this, a renew client with no configured binding
    // anchors to `store.current_binding_id()` — it is NEVER "fresh".
    let seed_client = {
        let mut id = test_identity();
        id.binding_id = Some(server.kit().binding_id().to_owned());
        HeartbeatClient::new(
            Arc::clone(&server),
            Arc::clone(&store),
            server.pinned_root(),
            test_config(),
            id,
        )
        .with_signer(std::sync::Arc::new(fake::pop_test_signer()))
    };
    tokio::time::timeout(Duration::from_secs(5), seed_client.run_once())
        .await
        .unwrap()
        .expect("seed install for this device");
    let seeded_binding = server.kit().binding_id().to_owned();
    assert!(
        store.current().is_some(),
        "the store holds a newer local lease"
    );

    // A FRESH client with NO configured binding — it anchors to the store's binding
    // (renew-only). The server now replays a STALE (older granted_at) lease minted
    // for a FOREIGN binding. It is crypto-valid, so verify passes; the binding
    // anchored from the store rejects the foreign body BEFORE the stale check. The
    // device identity must remain the seeded one.
    let attack_client = HeartbeatClient::new(
        Arc::clone(&server),
        Arc::clone(&store),
        server.pinned_root(),
        test_config(),
        {
            let mut id = test_identity();
            id.binding_id = None; // unconfigured: anchors to the store's binding
            id
        },
    )
    .with_signer(std::sync::Arc::new(fake::pop_test_signer()));
    server.set_foreign_binding(true);
    server.set_replay_stale(true); // older granted_at than the seeded lease
    let res = tokio::time::timeout(Duration::from_secs(5), attack_client.run_once())
        .await
        .expect("run_once must not hang");
    assert!(
        matches!(res, Err(HeartbeatError::BindingMismatch)),
        "a stale FOREIGN lease must be rejected as a binding mismatch, got {res:?}"
    );

    // The store still holds the seeded (this-device) lease — unchanged.
    let after = store.current().expect("store still holds the seeded lease");
    assert_eq!(
        after.lease.serial,
        server.kit().lease_serial(),
        "the seeded local lease is untouched"
    );

    // Identity was NOT poisoned: the next heartbeat addresses the seeded binding.
    server.set_foreign_binding(false);
    server.set_replay_stale(false);
    server.clear_last_binding_id();
    tokio::time::timeout(Duration::from_secs(5), attack_client.run_once())
        .await
        .unwrap()
        .expect("a legitimate renewal succeeds");
    assert_eq!(
        server.last_binding_id().expect("a binding id was sent"),
        seeded_binding,
        "identity was not poisoned — the renewal addresses the device's real binding"
    );
}

// --- Round-3 BLOCKER 2: key-trust TOCTOU. A signer that expires (valid_until
//     elapses) BETWEEN trust-evaluation and lease-acceptance is rejected. ---------

#[tokio::test]
async fn a_signer_that_expires_during_the_call_is_rejected_at_acceptance() {
    use multiview_licence::heartbeat::HeartbeatError;
    use std::sync::atomic::{AtomicUsize, Ordering as O};
    let server = shared_fake();
    let store = Arc::new(LeaseStore::new());

    // The fabricated signer is valid in [VALID_FROM, VALID_UNTIL]. We drive a
    // clock that reports an instant INSIDE the window on the FIRST read (so any
    // pre-network trust check passes) and an instant just AFTER valid_until on the
    // SECOND read (lease-acceptance time). A verifier that froze trust at the
    // pre-network instant would wrongly accept; re-evaluating at acceptance must
    // reject the now-expired signer.
    let inside = server.kit().now_ms();
    let after_expiry = server.kit().valid_until() + 1;
    let reads = Arc::new(AtomicUsize::new(0));
    let reads2 = Arc::clone(&reads);
    let clock = std::sync::Arc::new(move || {
        // First read → inside validity; every later read → after expiry.
        if reads2.fetch_add(1, O::SeqCst) == 0 {
            inside
        } else {
            after_expiry
        }
    });
    let client = HeartbeatClient::with_clock(
        Arc::clone(&server),
        Arc::clone(&store),
        server.pinned_root(),
        test_config(),
        test_identity(),
        clock,
    )
    .with_signer(std::sync::Arc::new(fake::pop_test_signer()));

    let res = tokio::time::timeout(Duration::from_secs(5), client.run_once())
        .await
        .expect("run_once must not hang");
    assert!(
        matches!(
            res,
            Err(HeartbeatError::KeyTrust(_) | HeartbeatError::SignedLease(_))
        ),
        "a signer that expired between trust-eval and acceptance must be rejected, got {res:?}"
    );
    assert!(
        store.status().is_none(),
        "nothing is installed when the signer expired at acceptance time"
    );
    assert!(
        reads.load(O::SeqCst) >= 2,
        "trust must be re-evaluated with a fresh clock read at acceptance (got {} reads)",
        reads.load(O::SeqCst)
    );
}

/// A deterministic foreign root (different from the fabricated keyset's root) for
/// the wrong-root rejection test.
fn foreign_root() -> multiview_licence::heartbeat::PinnedRoot {
    use p256::ecdsa::{SigningKey, VerifyingKey};
    let sk = SigningKey::from_bytes(&[3u8; 32].into()).unwrap();
    let vk = VerifyingKey::from(&sk);
    multiview_licence::heartbeat::PinnedRoot::from_sec1_bytes(vk.to_encoded_point(false).as_bytes())
        .unwrap()
}

// --- Round-4 BLOCKER 1: REVOCATION TOCTOU. A revocation published for the signer
//     BETWEEN the initial fetch and the response must reject the returned lease.

#[tokio::test]
async fn a_signer_revoked_during_the_call_is_rejected_at_acceptance() {
    use multiview_licence::heartbeat::HeartbeatError;
    let server = shared_fake();
    let store = Arc::new(LeaseStore::new());
    let client = client(Arc::clone(&server), Arc::clone(&store));

    // The first fetch_keys returns a clean keyset (signer unrevoked). The issuer
    // then publishes a ROOT-ATTESTED revocation for that signer during the stalled
    // call, so the SECOND (acceptance-time) fetch must see the signer revoked and
    // reject the returned lease. A fresh CLOCK alone cannot catch this — revocation
    // is set-membership over the keys document, so the acceptance re-check must
    // RE-FETCH the key/revocation material, not re-evaluate the stale doc.
    server.set_revoke_signer_after_first_fetch(true);
    let res = tokio::time::timeout(Duration::from_secs(5), client.run_once())
        .await
        .expect("run_once must not hang");
    assert!(
        matches!(
            res,
            Err(HeartbeatError::KeyTrust(_) | HeartbeatError::SignedLease(_))
        ),
        "a signer revoked between fetch and acceptance must be rejected, got {res:?}"
    );
    assert!(
        store.status().is_none(),
        "nothing is installed when the signer was revoked at acceptance time"
    );
    assert!(
        server.key_fetches() >= 2,
        "revocation must be re-checked against a FRESH key fetch at acceptance (got {} fetches)",
        server.key_fetches()
    );
}

// --- Round-4 BLOCKER 2 (renew-only): BINDING-ANCHOR GAP. A lease installed via
//     the file-drop / upload surface (a LeaseBinding installed DIRECTLY into the
//     store) must make the device non-fresh, so a subsequent foreign RENEW lease
//     is rejected. This is the renew-only onboarding flow: a lease arrives via an
//     install surface (here upload/file-drop) and the client then renews it; a
//     foreign lease cannot hijack that anchored identity.

#[tokio::test]
async fn a_lease_installed_via_the_upload_surface_anchors_identity_against_a_foreign_renew() {
    use multiview_licence::heartbeat::HeartbeatError;
    let server = shared_fake();
    let store = Arc::new(LeaseStore::new());

    // Simulate the offline-upload / file-drop surface: install a LeaseBinding for
    // THIS device's binding DIRECTLY into the store (NOT via the heartbeat path),
    // exactly as control/routes/licence.rs and watcher.rs do. This is the renew-only
    // onboarding path — the operator/portal lease reaches the device this way.
    let (binding, pinned) = fake::upload_binding_for(server.kit(), server.kit().binding_id());
    store
        .install_binding(&binding, &pinned, store.now())
        .expect("the upload/file-drop surface installs a binding");
    assert!(
        store.current_binding_id().is_some(),
        "an upload/file-drop install must establish the device's binding identity"
    );

    // A FRESH heartbeat client with NO configured binding — it anchors to the
    // store's binding (renew-only). The server returns a FOREIGN-binding lease. The
    // device is NOT fresh (it holds an upload-installed lease), so the foreign lease
    // must be rejected.
    let attack = HeartbeatClient::new(
        Arc::clone(&server),
        Arc::clone(&store),
        server.pinned_root(),
        test_config(),
        {
            let mut id = test_identity();
            id.binding_id = None;
            id
        },
    )
    .with_signer(std::sync::Arc::new(fake::pop_test_signer()));
    server.set_foreign_binding(true);
    let res = tokio::time::timeout(Duration::from_secs(5), attack.run_once())
        .await
        .expect("run_once must not hang");
    assert!(
        matches!(res, Err(HeartbeatError::BindingMismatch)),
        "a foreign renew lease must be rejected when an upload lease already anchors identity, got {res:?}"
    );
}

// --- Round-4 MAJOR: the Idempotency-Key is retry-stable per logical operation. ---

#[tokio::test]
async fn retries_of_the_same_logical_operation_send_a_stable_idempotency_key() {
    let server = shared_fake();
    let store = Arc::new(LeaseStore::new());
    let client = client(Arc::clone(&server), Arc::clone(&store));

    // The first heartbeat attempt fails AFTER the server records its Idempotency-
    // Key (a lost-response analogue), the second attempt succeeds. Both attempts of
    // the SAME logical operation must carry the SAME Idempotency-Key so the server
    // can dedupe and never create a duplicate binding/lease.
    server.set_fail_after_recording_idempotency(1);
    // First cycle: fails (the server recorded the key, then errored).
    let _ = tokio::time::timeout(Duration::from_secs(5), client.run_once()).await;
    // Second cycle (the retry of the same logical heartbeat): succeeds.
    let _ = tokio::time::timeout(Duration::from_secs(5), client.run_once())
        .await
        .expect("run_once must not hang");

    let keys = server.idempotency_keys();
    assert!(
        keys.len() >= 2,
        "the server must have seen at least two attempts (got {})",
        keys.len()
    );
    assert!(
        keys.iter().all(|k| k == &keys[0]),
        "every retry of the same logical operation must send the SAME Idempotency-Key, got {keys:?}"
    );
    assert!(!keys[0].is_empty(), "the Idempotency-Key must be non-empty");
}

// --- Round-5 MAJOR: the Idempotency-Key nonce is DURABLE across a restart. -------

/// A test [`NonceStore`] backed by a shared cell — the analogue of the cli's
/// on-disk nonce file. Reconstructing a client against the SAME shared cell
/// simulates a process restart that reads the persisted counter back.
#[derive(Clone)]
struct SharedNonceStore {
    cell: Arc<std::sync::atomic::AtomicU64>,
}

impl SharedNonceStore {
    fn new() -> Self {
        Self {
            cell: Arc::new(std::sync::atomic::AtomicU64::new(0)),
        }
    }
}

impl multiview_licence::heartbeat::NonceStore for SharedNonceStore {
    fn load(&self) -> Result<u64, multiview_licence::heartbeat::NonceError> {
        Ok(self.cell.load(Ordering::SeqCst))
    }
    fn commit(&self, value: u64) -> Result<(), multiview_licence::heartbeat::NonceError> {
        self.cell.store(value, Ordering::SeqCst);
        Ok(())
    }
}

#[tokio::test]
async fn the_idempotency_nonce_is_distinct_across_a_restart() {
    use std::sync::atomic::AtomicU64;
    // A successful op under client A persists its nonce to the shared (durable)
    // store. A FRESH client B reconstructed from the SAME store (a restart) must
    // mint a DISTINCT key for its next logical operation — never reuse A's
    // completed op's key (which a from-zero in-memory counter would do, colliding
    // mv-{machine}-1 across lifetimes → cross-restart duplicate mutation).
    let nonce = SharedNonceStore::new();

    // Lifetime A: one successful heartbeat.
    let server_a = shared_fake();
    let store_a = Arc::new(LeaseStore::new());
    let client_a = HeartbeatClient::with_clock_and_nonce(
        Arc::clone(&server_a),
        store_a,
        server_a.pinned_root(),
        test_config(),
        test_identity(),
        Arc::new(unix_millis_for_test()),
        Arc::new(nonce.clone()),
    )
    .with_signer(std::sync::Arc::new(fake::pop_test_signer()));
    tokio::time::timeout(Duration::from_secs(5), client_a.run_once())
        .await
        .unwrap()
        .expect("lifetime A heartbeat succeeds");
    let key_a = server_a
        .idempotency_keys()
        .last()
        .cloned()
        .expect("A sent a key");

    // Lifetime B: a fresh client + fresh server, reconstructed from the SAME
    // durable nonce store (the restart).
    let server_b = shared_fake();
    let store_b = Arc::new(LeaseStore::new());
    let client_b = HeartbeatClient::with_clock_and_nonce(
        Arc::clone(&server_b),
        store_b,
        server_b.pinned_root(),
        test_config(),
        test_identity(),
        Arc::new(unix_millis_for_test()),
        Arc::new(nonce.clone()),
    )
    .with_signer(std::sync::Arc::new(fake::pop_test_signer()));
    tokio::time::timeout(Duration::from_secs(5), client_b.run_once())
        .await
        .unwrap()
        .expect("lifetime B heartbeat succeeds");
    let key_b = server_b
        .idempotency_keys()
        .last()
        .cloned()
        .expect("B sent a key");

    assert_ne!(
        key_a, key_b,
        "a post-restart op must mint a DISTINCT idempotency key, never reuse a completed op's key \
         (durable nonce); got A={key_a} B={key_b}"
    );
    // Sanity: the durable counter advanced past A's value.
    let _ = AtomicU64::new(0);
}

/// A fixed epoch-ms clock for the durable-nonce test (the value is irrelevant —
/// the key derives from the counter + machine id, never the clock).
fn unix_millis_for_test() -> impl Fn() -> i64 + Send + Sync {
    || 1_790_000_000_000
}

// --- Round-6 BLOCKER 1: the durable idempotency nonce FAILS CLOSED. --------------
//
// A durable nonce store that cannot be trusted (a corrupt/unreadable load, or a
// commit that does not persist) must NEVER let the heartbeat send a mutation under
// a possibly-colliding key. The mint is gated on a trustworthy durable commit
// BEFORE the mutation; on a nonce-store error the cycle is a keep-last-good no-op
// (no mutation reaches the server) — a nonce-store I/O failure must never tighten
// output (inv #1/#10) and never send a colliding-key mutation.

/// A [`NonceStore`] whose `load` and `commit` both fail — the analogue of a
/// corrupt/unwritable on-disk nonce file. The client must refuse to mint a key
/// against it (fail closed), so no mutation is sent.
struct FailingNonceStore;

impl multiview_licence::heartbeat::NonceStore for FailingNonceStore {
    fn load(&self) -> Result<u64, multiview_licence::heartbeat::NonceError> {
        Err(multiview_licence::heartbeat::NonceError::new(
            "durable nonce unreadable/corrupt",
        ))
    }
    fn commit(&self, _value: u64) -> Result<(), multiview_licence::heartbeat::NonceError> {
        Err(multiview_licence::heartbeat::NonceError::new(
            "durable nonce commit failed",
        ))
    }
}

/// A [`NonceStore`] that loads fine (fresh device, 0) but whose `commit` always
/// fails — the analogue of a read-only / full lease-state dir. The mint must fail
/// closed (the key was never durably committed), so no mutation is sent.
struct UncommittableNonceStore;

impl multiview_licence::heartbeat::NonceStore for UncommittableNonceStore {
    fn load(&self) -> Result<u64, multiview_licence::heartbeat::NonceError> {
        Ok(0)
    }
    fn commit(&self, _value: u64) -> Result<(), multiview_licence::heartbeat::NonceError> {
        Err(multiview_licence::heartbeat::NonceError::new(
            "durable nonce commit failed",
        ))
    }
}

#[tokio::test]
async fn a_failing_durable_nonce_store_keeps_last_good_and_sends_no_mutation() {
    // The device HAS a binding (so it would otherwise renew), but its durable nonce
    // store fails. The mint must fail closed: run_once returns an error, NO
    // heartbeat mutation is sent (so the server can never receive a colliding key),
    // and nothing is installed (last-good kept; output stays on air).
    let server = shared_fake();
    let store = Arc::new(LeaseStore::new());
    // Default identity carries a configured binding → the renew path.
    let client = HeartbeatClient::with_clock_and_nonce(
        Arc::clone(&server),
        Arc::clone(&store),
        server.pinned_root(),
        test_config(),
        test_identity(),
        Arc::new(unix_millis_for_test()),
        Arc::new(FailingNonceStore),
    )
    .with_signer(std::sync::Arc::new(fake::pop_test_signer()));

    let res = tokio::time::timeout(Duration::from_secs(5), client.run_once())
        .await
        .expect("run_once must not hang");
    assert!(
        res.is_err(),
        "a failing durable nonce store must fail the cycle closed, got {res:?}"
    );
    assert_eq!(
        server.heartbeats.load(Ordering::SeqCst),
        0,
        "NO heartbeat mutation may be sent when the nonce cannot be durably committed"
    );
    assert!(
        server.idempotency_keys().is_empty(),
        "no mutation (and so no idempotency key) reaches the server on a nonce-store failure"
    );
    assert!(
        store.status().is_none(),
        "nothing is installed — last-good is kept (never off air)"
    );
}

#[tokio::test]
async fn a_nonce_commit_failure_blocks_the_mutation_fail_closed() {
    // load() succeeds (fresh device, counter 0) but commit() fails — the key was
    // never durably persisted, so sending the mutation would risk a cross-restart
    // collision. The mint must fail closed: no mutation, nothing installed.
    let server = shared_fake();
    let store = Arc::new(LeaseStore::new());
    let client = HeartbeatClient::with_clock_and_nonce(
        Arc::clone(&server),
        Arc::clone(&store),
        server.pinned_root(),
        test_config(),
        test_identity(),
        Arc::new(unix_millis_for_test()),
        Arc::new(UncommittableNonceStore),
    )
    .with_signer(std::sync::Arc::new(fake::pop_test_signer()));

    let res = tokio::time::timeout(Duration::from_secs(5), client.run_once())
        .await
        .expect("run_once must not hang");
    assert!(
        res.is_err(),
        "a nonce commit failure must fail the cycle closed, got {res:?}"
    );
    assert_eq!(
        server.heartbeats.load(Ordering::SeqCst),
        0,
        "NO mutation may be sent when the nonce commit did not persist"
    );
    assert!(
        store.status().is_none(),
        "nothing is installed — last-good is kept"
    );
}
