//! Local lease-store + install-path tests (CONSPECT-1): a verified lease
//! installs and the store computes the ladder state at an injected `now`; a
//! tampered lease is rejected `SignatureInvalid`; a stale (older) lease is
//! rejected `Stale`; a below-threshold fingerprint is rejected
//! `FingerprintMismatch`. The store is thread-safe, in-memory, best-effort, and
//! never reads a system clock itself (the clock is injected).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::as_conversions,
    clippy::missing_panics_doc
)]

use std::sync::Arc;

use chrono::{DateTime, Duration, Utc};
use ed25519_dalek::rand_core::UnwrapErr;
use ed25519_dalek::{Signer, SigningKey};
use getrandom::SysRng;
use multiview_licence::entitlement::{
    Entitlement, EntitlementFlags, GpuLimit, HardwareClass, Tier,
};
use multiview_licence::lease::{Lease, LeaseSource};
use multiview_licence::status::EnforcementLevel;
use multiview_licence::store::{InstallError, LeaseBinding, LeaseStore};
use multiview_licence::verify::{PinnedKey, SignedLease};
use multiview_licence::{compute_ladder_state, LadderState, ACTIVATION_WINDOW_DAYS};

fn epoch() -> DateTime<Utc> {
    DateTime::from_timestamp(1_700_000_000, 0).unwrap()
}

fn keypair() -> (SigningKey, PinnedKey) {
    let mut rng = UnwrapErr(SysRng);
    let key = SigningKey::generate(&mut rng);
    let pinned = PinnedKey::from_verifying_key(&key.verifying_key());
    (key, pinned)
}

fn lease_at(serial: &str, granted: DateTime<Utc>) -> Lease {
    Lease::new_full(
        serial.to_owned(),
        granted,
        LeaseSource::Online,
        ACTIVATION_WINDOW_DAYS,
    )
}

fn sign(key: &SigningKey, lease: &Lease) -> SignedLease {
    // These fixtures build bindings with no instance_binding_id (None), so sign
    // over the lease bound to None — the binding-anchor signature contract.
    let msg = SignedLease::signing_bytes(lease, None);
    let sig = key.sign(&msg);
    SignedLease::new(lease.clone(), sig.to_bytes())
}

/// A binding carrying a strong fingerprint score (same machine) over a signed
/// lease, the standard install unit the three install paths converge on.
fn binding(key: &SigningKey, lease: &Lease, score: u8) -> LeaseBinding {
    LeaseBinding::new(
        sign(key, lease),
        Entitlement::new(
            Tier::new("studio".to_owned()),
            HardwareClass::Standard,
            HardwareClass::Standard,
            GpuLimit::Limited(2),
            lease.clone(),
            EntitlementFlags::default(),
        ),
        score,
        None,
    )
}

#[test]
fn a_verified_lease_installs_and_the_store_computes_the_ladder_state() {
    let (key, pinned) = keypair();
    let now = epoch();
    let lease = lease_at("serial-LIVE01", now);
    let store = LeaseStore::with_clock(Arc::new(move || now));

    let installed = store
        .install_binding(&binding(&key, &lease, 100), &pinned, now)
        .expect("a valid binding installs");
    assert_eq!(installed.serial, "serial-LIVE01");

    // The store now reports a status computed via the crate's ladder.
    let status = store.status().expect("a status after install");
    assert_eq!(status.tier, "studio");
    assert_eq!(status.state, LadderState::Compliant);
    assert_eq!(status.enforcement, EnforcementLevel::Active);
    assert_eq!(status.hardware_class.licensed, HardwareClass::Standard);
    assert_eq!(status.hardware_class.detected, HardwareClass::Standard);
    assert_eq!(status.gpu_limit, GpuLimit::Limited(2));
    assert_eq!(status.lease.serial, "serial-LIVE01");
    assert_eq!(status.lease.source, LeaseSource::Online);
    // The view's computed state matches the crate's compute_ladder_state directly.
    let outcome = compute_ladder_state(&store.ladder_input(now).expect("input"));
    assert_eq!(status.state, outcome.state);
}

#[test]
fn the_store_recomputes_state_as_the_clock_advances() {
    // Same installed lease, two different `now`s: compliant at grant, grace once
    // a day past expiry. The store recomputes from the injected clock each read.
    let (key, pinned) = keypair();
    let granted = epoch();
    let lease = lease_at("serial-AGE", granted);
    let store = LeaseStore::new();
    store
        .install_binding(&binding(&key, &lease, 100), &pinned, granted)
        .expect("install");

    // Compliant within term.
    assert_eq!(
        store.status_at(granted).expect("status").state,
        LadderState::Compliant
    );
    // One day past the 35-day term → grace.
    let in_grace = lease.expires_at + Duration::days(1);
    assert_eq!(
        store.status_at(in_grace).expect("status").state,
        LadderState::Grace
    );
}

#[test]
fn a_tampered_lease_is_rejected_signature_invalid() {
    let (key, pinned) = keypair();
    let now = epoch();
    let lease = lease_at("serial-OK", now);
    let mut b = binding(&key, &lease, 100);
    // Mutate the covered serial AFTER signing — the signature no longer matches.
    b.signed.lease.serial = "serial-EVIL".to_owned();

    let err = store_install_err(&b, &pinned, now);
    assert!(
        matches!(err, InstallError::SignatureInvalid),
        "tampered lease must be SignatureInvalid, got {err:?}"
    );
}

#[test]
fn a_tampered_instance_binding_id_is_rejected_and_never_anchors_identity() {
    // The binding anchor MUST come from SIGNED material: the device's durable
    // instance identity (`current_binding_id`) is recorded from
    // `binding.instance_binding_id`, so that field has to be covered by the same
    // Ed25519 signature `install_binding` verifies. Here the lease is signed
    // legitimately (binding id NONE in the signed payload) but an attacker grafts
    // an unsigned `instance_binding_id` onto the binding AFTER signing. install
    // must reject it `SignatureInvalid` and the store must NEVER anchor the forged
    // id — otherwise an attacker could poison the device identity (or DoS renewals)
    // via the offline/file-drop/relay surface with an unsigned binding id.
    let (key, pinned) = keypair();
    let now = epoch();
    let lease = lease_at("serial-ANCHOR", now);
    let mut b = binding(&key, &lease, 100);
    // Graft a binding id the signature does not cover (signed payload had None).
    b.instance_binding_id = Some("ib_attacker_9999".to_owned());

    let store = LeaseStore::new();
    let err = store
        .install_binding(&b, &pinned, now)
        .expect_err("an unsigned/forged instance_binding_id must be rejected");
    assert!(
        matches!(err, InstallError::SignatureInvalid),
        "a binding id not covered by the signature must be SignatureInvalid, got {err:?}"
    );
    assert!(
        store.current_binding_id().is_none(),
        "a rejected install must NEVER anchor a forged binding id"
    );
}

#[test]
fn a_tampered_instance_binding_id_in_a_cbor_payload_is_rejected_on_the_file_drop_relay_path() {
    // The realistic cross-instance-poisoning attack on the offline file-drop /
    // mesh-relay surfaces: those install a LeaseBinding decoded from CBOR
    // (LeaseBinding::from_bytes), NOT one built in-process. An attacker takes a
    // genuinely-signed binding and edits the CBOR `instanceBindingId` to a binding
    // they control while leaving the inner lease signature intact. The decoded
    // binding must be REJECTED at install (SignatureInvalid — the signature covers
    // the binding id) and must NEVER anchor the forged id. This exercises the
    // EXACT from_bytes→install path watcher.rs and the mesh relay take.
    let (key, pinned) = keypair();
    let now = epoch();
    let lease = lease_at("serial-CBOR", now);
    // A legitimate, fully-signed binding for THIS device's real binding id.
    let genuine = binding_with_id(&key, &lease, 100, "ib_real_device");
    let bytes = genuine
        .to_cbor()
        .expect("encode the genuine binding to CBOR");

    // The attacker decodes it and grafts a foreign binding id (the CBOR field is
    // not covered by re-signing — only the inner lease bytes are signed).
    let mut tampered = LeaseBinding::from_bytes(&bytes).expect("decode the CBOR binding");
    tampered.instance_binding_id = Some("ib_attacker_via_file".to_owned());

    let store = LeaseStore::new();
    let err = store
        .install_binding(&tampered, &pinned, now)
        .expect_err("a CBOR binding with a tampered instance_binding_id must be rejected");
    assert!(
        matches!(err, InstallError::SignatureInvalid),
        "a tampered CBOR binding id must be SignatureInvalid on the file-drop/relay path, got {err:?}"
    );
    assert!(
        store.current_binding_id().is_none(),
        "the file-drop/relay path must NEVER anchor a forged binding id"
    );

    // Control: the UNtampered decoded binding installs and anchors the real id.
    let genuine_decoded = LeaseBinding::from_bytes(&bytes).expect("decode again");
    store
        .install_binding(&genuine_decoded, &pinned, now)
        .expect("the genuine CBOR binding installs");
    assert_eq!(
        store.current_binding_id(),
        Some("ib_real_device".to_owned()),
        "the genuine signed binding id is anchored from the CBOR/file-drop path"
    );
}

#[test]
fn a_below_threshold_fingerprint_is_rejected_fingerprint_mismatch() {
    let (key, pinned) = keypair();
    let now = epoch();
    let lease = lease_at("serial-DRIFT", now);
    // Score 69 is one below FINGERPRINT_MATCH_THRESHOLD (70): a *new* machine.
    let b = binding(&key, &lease, 69);

    let err = store_install_err(&b, &pinned, now);
    assert!(
        matches!(err, InstallError::FingerprintMismatch { score: 69, .. }),
        "below-threshold fingerprint must be FingerprintMismatch, got {err:?}"
    );
}

#[test]
fn a_threshold_fingerprint_installs() {
    // Exactly the threshold (70) is the SAME machine (drift tolerated) — installs.
    let (key, pinned) = keypair();
    let now = epoch();
    let lease = lease_at("serial-EDGE", now);
    let store = LeaseStore::new();
    assert!(store
        .install_binding(&binding(&key, &lease, 70), &pinned, now)
        .is_ok());
}

#[test]
fn an_older_lease_is_rejected_stale_after_a_newer_one_installs() {
    let (key, pinned) = keypair();
    let new_granted = epoch();
    let old_granted = epoch() - Duration::days(10);
    let store = LeaseStore::new();

    // Install the newer grant first.
    store
        .install_binding(
            &binding(&key, &lease_at("serial-NEW", new_granted), 100),
            &pinned,
            new_granted,
        )
        .expect("newer installs");

    // A subsequently-presented OLDER grant must be rejected as stale (replay /
    // rollback protection): the active lease never goes backwards.
    let err = store
        .install_binding(
            &binding(&key, &lease_at("serial-OLD", old_granted), 100),
            &pinned,
            new_granted,
        )
        .expect_err("an older grant must be rejected");
    assert!(
        matches!(err, InstallError::Stale { .. }),
        "an older grant must be Stale, got {err:?}"
    );
    // The active lease is unchanged (still the newer one).
    assert_eq!(store.status().expect("status").lease.serial, "serial-NEW");
}

/// Drive an install against a fresh store and return the error it must produce.
fn store_install_err(b: &LeaseBinding, pinned: &PinnedKey, now: DateTime<Utc>) -> InstallError {
    let store = LeaseStore::new();
    store
        .install_binding(b, pinned, now)
        .expect_err("install must fail")
}

#[test]
fn the_store_retains_the_verified_fingerprint_score_for_support_context() {
    // The support-ticket context auto-attaches the fingerprint *score* (a number,
    // never a raw identifier — brief §8). The store retains the score the install
    // verified, and reports `None` before any install.
    let (key, pinned) = keypair();
    let now = epoch();
    let store = LeaseStore::with_clock(Arc::new(move || now));
    assert_eq!(
        store.fingerprint_score(),
        None,
        "no lease installed yet → no score (never a false zero)"
    );

    let lease = lease_at("serial-FP", now);
    store
        .install_binding(&binding(&key, &lease, 88), &pinned, now)
        .expect("a valid binding installs");
    assert_eq!(
        store.fingerprint_score(),
        Some(88),
        "the store retains the exact verified score the install accepted"
    );
}

#[test]
fn the_store_records_and_reports_the_instance_binding_id_from_an_install() {
    // The server-issued instanceBindingId is anchored ATOMICALLY by a genuine
    // install (the binding carries the SIGNED id; install_binding publishes it with
    // the lease), so the device's instance identity is durable (the
    // cross-instance-replay guard reads it back). `None` until a lease installs.
    let (key, pinned) = keypair();
    let now = epoch();
    let store = LeaseStore::with_clock(Arc::new(move || now));
    assert_eq!(
        store.current_binding_id(),
        None,
        "no lease installed yet → None (the device is genuinely fresh)"
    );
    let lease1 = lease_at("serial-BIND01", now);
    store
        .install_binding(
            &binding_with_id(&key, &lease1, 100, "ib_device_0001"),
            &pinned,
            now,
        )
        .expect("the first binding installs");
    assert_eq!(
        store.current_binding_id(),
        Some("ib_device_0001".to_owned()),
        "the store reports the installed instance binding id"
    );
    // A later genuine install with a NEWER grant + a different binding id (same
    // device, re-bound) overwrites it — the store is a single-value cache of the
    // current identity, published atomically with the lease.
    let lease2 = lease_at("serial-BIND02", now + Duration::seconds(1));
    store
        .install_binding(
            &binding_with_id(&key, &lease2, 100, "ib_device_0002"),
            &pinned,
            now,
        )
        .expect("the newer binding installs");
    assert_eq!(
        store.current_binding_id(),
        Some("ib_device_0002".to_owned()),
        "a newer install overwrites — the store holds the current binding identity"
    );
}

/// A binding carrying a SIGNED instance binding id (the signature covers it, so it
/// installs after the signed-anchor fix). The standard binding-aware install unit.
fn binding_with_id(key: &SigningKey, lease: &Lease, score: u8, binding_id: &str) -> LeaseBinding {
    let msg = SignedLease::signing_bytes(lease, Some(binding_id));
    let sig = key.sign(&msg);
    LeaseBinding::new(
        SignedLease::new(lease.clone(), sig.to_bytes()),
        Entitlement::new(
            Tier::new("studio".to_owned()),
            HardwareClass::Standard,
            HardwareClass::Standard,
            GpuLimit::Limited(2),
            lease.clone(),
            EntitlementFlags::default(),
        ),
        score,
        Some(binding_id.to_owned()),
    )
}

#[test]
fn a_concurrent_reader_never_observes_a_torn_install_lease_present_binding_absent() {
    // The install must publish the lease AND its binding anchor as ONE atomic
    // update: a concurrent reader must NEVER observe current().is_some() (a lease
    // is installed) while current_binding_id() is still None. That torn state would
    // make a device with a freshly-installed lease look "fresh" to the heartbeat
    // client (established_binding == None) and SKIP the BindingMismatch guard — a
    // foreign-binding lease could then install. We exercise the None→Some install
    // transition across many fresh stores with a concurrent spin-reader and assert
    // the torn (lease-present, binding-absent) state is observed ZERO times.
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::Barrier;

    let (key, pinned) = keypair();
    let now = epoch();
    let lease = lease_at("serial-ATOMIC", now);
    let torn_seen = Arc::new(AtomicBool::new(false));
    let lease_present_samples = Arc::new(AtomicU64::new(0));

    // Many independent trials so the install's None→Some window is hit repeatedly.
    for _ in 0..2_000 {
        let store = Arc::new(LeaseStore::with_clock(Arc::new(move || now)));
        let barrier = Arc::new(Barrier::new(2));

        let reader_store = Arc::clone(&store);
        let reader_barrier = Arc::clone(&barrier);
        let torn = Arc::clone(&torn_seen);
        let present = Arc::clone(&lease_present_samples);
        let reader = std::thread::spawn(move || {
            reader_barrier.wait();
            // Spin until the lease appears, sampling the (lease, binding) pair. The
            // instant the lease is visible, the binding MUST be visible too.
            for _ in 0..200_000 {
                let lease_present = reader_store.current().is_some();
                let binding_present = reader_store.current_binding_id().is_some();
                if lease_present {
                    present.fetch_add(1, Ordering::Relaxed);
                    if !binding_present {
                        torn.store(true, Ordering::SeqCst);
                    }
                    break;
                }
            }
        });

        let b = binding_with_id(&key, &lease, 100, "ib_atomic_0001");
        barrier.wait();
        store
            .install_binding(&b, &pinned, now)
            .expect("the binding installs");
        reader.join().expect("reader thread joins");
    }

    assert!(
        lease_present_samples.load(Ordering::Relaxed) > 0,
        "the reader must have observed the installed lease at least once (test is live)"
    );
    assert!(
        !torn_seen.load(Ordering::SeqCst),
        "a concurrent reader observed a TORN install (lease present but binding id absent) — \
         the install is not atomic"
    );
}
