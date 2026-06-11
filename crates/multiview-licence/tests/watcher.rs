//! Lease-directory-watcher tests (CONSPECT-1, brief §2/§8): a binding file
//! dropped into the watched directory is picked up on the next poll, verified
//! against the pinned key, and installed into the store; an invalid/tampered
//! file WARNs and is ignored (never crash, never stall — bad-inputs-are-the-
//! purpose). The directory is config-overridable (a tempdir here).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::as_conversions,
    clippy::map_unwrap_or,
    clippy::missing_panics_doc
)]

use std::sync::Arc;

use chrono::{DateTime, Utc};
use ed25519_dalek::{Signer, SigningKey};
use multiview_licence::entitlement::{
    Entitlement, EntitlementFlags, GpuLimit, HardwareClass, Tier,
};
use multiview_licence::lease::{Lease, LeaseSource};
use multiview_licence::store::{LeaseBinding, LeaseStore};
use multiview_licence::verify::{PinnedKey, SignedLease};
use multiview_licence::watcher::{LeaseDirectoryWatcher, PollOutcome};
use multiview_licence::ACTIVATION_WINDOW_DAYS;
use rand_core::OsRng;

fn epoch() -> DateTime<Utc> {
    DateTime::from_timestamp(1_700_000_000, 0).unwrap()
}

fn keypair() -> (SigningKey, PinnedKey) {
    let mut rng = OsRng;
    let key = SigningKey::generate(&mut rng);
    let pinned = PinnedKey::from_verifying_key(&key.verifying_key());
    (key, pinned)
}

fn binding(key: &SigningKey, serial: &str, granted: DateTime<Utc>) -> LeaseBinding {
    let lease = Lease::new_full(
        serial.to_owned(),
        granted,
        LeaseSource::File,
        ACTIVATION_WINDOW_DAYS,
    );
    let msg = SignedLease::signing_bytes(&lease);
    let sig = key.sign(&msg);
    LeaseBinding::new(
        SignedLease::new(lease.clone(), sig.to_bytes()),
        Entitlement::new(
            Tier::new("studio".to_owned()),
            HardwareClass::Standard,
            HardwareClass::Standard,
            GpuLimit::Unlimited,
            lease,
            EntitlementFlags::default(),
        ),
        100,
    )
}

/// A unique temp directory for one test (no external tempfile dependency).
fn temp_dir(tag: &str) -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let dir = std::env::temp_dir().join(format!("conspect-watch-{tag}-{nanos}"));
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

#[test]
fn a_dropped_valid_binding_is_picked_up_and_activated() {
    let (key, pinned) = keypair();
    let now = epoch();
    let dir = temp_dir("valid");
    let store = Arc::new(LeaseStore::with_clock(Arc::new(move || now)));
    let watcher = LeaseDirectoryWatcher::new(dir.clone(), pinned, Arc::clone(&store));

    // No file yet → an empty poll, store still empty.
    assert!(matches!(watcher.poll_once(now), PollOutcome::NothingNew));
    assert!(store.status().is_none(), "no lease before any file");

    // Drop a valid binding file into the watched directory.
    let b = binding(&key, "serial-DROP01", now);
    let path = dir.join("studio-host-01.binding");
    std::fs::write(&path, b.to_cbor().expect("encode binding")).expect("write");

    // The next poll picks it up and activates it.
    let outcome = watcher.poll_once(now);
    assert!(
        matches!(outcome, PollOutcome::Installed { .. }),
        "a valid dropped file must activate, got {outcome:?}"
    );
    let status = store.status().expect("a lease after the drop");
    assert_eq!(status.lease.serial, "serial-DROP01");

    // Re-polling the same (unchanged) file does not re-install — idempotent.
    assert!(matches!(watcher.poll_once(now), PollOutcome::NothingNew));

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_tampered_dropped_file_is_warned_and_ignored_never_crashes() {
    let (key, pinned) = keypair();
    let now = epoch();
    let dir = temp_dir("tampered");
    let store = Arc::new(LeaseStore::new());
    let watcher = LeaseDirectoryWatcher::new(dir.clone(), pinned, Arc::clone(&store));

    // A binding whose covered serial was mutated after signing.
    let mut b = binding(&key, "serial-GOOD", now);
    b.signed.lease.serial = "serial-TAMPERED".to_owned();
    let path = dir.join("evil.binding");
    std::fs::write(&path, b.to_cbor().expect("encode")).expect("write");

    // The poll rejects it (does NOT crash / stall) and the store stays empty.
    let outcome = watcher.poll_once(now);
    assert!(
        matches!(outcome, PollOutcome::Rejected { .. }),
        "a tampered file must be rejected, got {outcome:?}"
    );
    assert!(
        store.status().is_none(),
        "a rejected file must never activate a lease"
    );

    // Garbage (not even CBOR) is likewise rejected, never a panic.
    std::fs::write(dir.join("garbage.binding"), [0xFF, 0x00, 0x42]).expect("write garbage");
    let outcome = watcher.poll_once(now);
    assert!(
        matches!(outcome, PollOutcome::Rejected { .. }),
        "garbage must be rejected, got {outcome:?}"
    );
    assert!(store.status().is_none());

    let _ = std::fs::remove_dir_all(&dir);
}
