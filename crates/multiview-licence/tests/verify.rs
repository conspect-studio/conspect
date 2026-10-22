//! Ed25519 lease-verification tests (ADR-0050 §2.5 / §3): a signed lease
//! assertion verifies against a PINNED public key; a tampered payload or the
//! wrong key is rejected with a typed error.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::as_conversions,
    clippy::missing_panics_doc
)]

use ed25519_dalek::rand_core::UnwrapErr;
use ed25519_dalek::{Signer, SigningKey};
use getrandom::SysRng;
use multiview_licence::lease::{Lease, LeaseSource};
use multiview_licence::verify::{verify_signed_lease, PinnedKey, SignedLease};
use multiview_licence::{LicenceError, ACTIVATION_WINDOW_DAYS};

fn epoch() -> chrono::DateTime<chrono::Utc> {
    chrono::DateTime::from_timestamp(1_700_000_000, 0).unwrap()
}

fn sample_lease() -> Lease {
    Lease::new_full(
        "serial-ABCDEF".to_owned(),
        epoch(),
        LeaseSource::Online,
        ACTIVATION_WINDOW_DAYS,
    )
}

/// Sign a lease with `key` (no instance binding id) and return the wire-form
/// signed lease. The binding-id-bound signing variant is exercised by the store
/// tests; these focus on the lease-signature mechanics.
fn sign_with(key: &SigningKey, lease: &Lease) -> SignedLease {
    let msg = SignedLease::signing_bytes(lease, None);
    let sig = key.sign(&msg);
    SignedLease::new(lease.clone(), sig.to_bytes())
}

#[test]
fn valid_signature_against_pinned_key_verifies() {
    let mut rng = UnwrapErr(SysRng);
    let key = SigningKey::generate(&mut rng);
    let pinned = PinnedKey::from_verifying_key(&key.verifying_key());
    let lease = sample_lease();
    let signed = sign_with(&key, &lease);

    let out = verify_signed_lease(&signed, &pinned, None).expect("valid lease must verify");
    assert_eq!(out.serial, "serial-ABCDEF");
}

#[test]
fn tampered_payload_is_rejected() {
    let mut rng = UnwrapErr(SysRng);
    let key = SigningKey::generate(&mut rng);
    let pinned = PinnedKey::from_verifying_key(&key.verifying_key());
    let lease = sample_lease();
    let mut signed = sign_with(&key, &lease);

    // Mutate the payload AFTER signing — the signature no longer covers it.
    signed.lease.serial = "serial-EVIL00".to_owned();

    let err =
        verify_signed_lease(&signed, &pinned, None).expect_err("tampered lease must be rejected");
    assert!(matches!(err, LicenceError::BadSignature));
}

#[test]
fn wrong_key_is_rejected() {
    let mut rng = UnwrapErr(SysRng);
    let real_signing_key = SigningKey::generate(&mut rng);
    let other = SigningKey::generate(&mut rng);
    // Pin the OTHER key — the signature was made by `real_signing_key`.
    let pinned = PinnedKey::from_verifying_key(&other.verifying_key());
    let lease = sample_lease();
    let signed = sign_with(&real_signing_key, &lease);

    let err = verify_signed_lease(&signed, &pinned, None).expect_err("wrong key must be rejected");
    assert!(matches!(err, LicenceError::BadSignature));
}

#[test]
fn malformed_signature_length_is_a_typed_error() {
    let mut rng = UnwrapErr(SysRng);
    let key = SigningKey::generate(&mut rng);
    let pinned = PinnedKey::from_verifying_key(&key.verifying_key());
    let lease = sample_lease();
    let mut signed = sign_with(&key, &lease);
    // Truncate the signature bytes — must be a typed error, never a panic.
    signed.signature.truncate(10);

    let err = verify_signed_lease(&signed, &pinned, None).expect_err("short sig must error");
    assert!(matches!(
        err,
        LicenceError::MalformedSignature | LicenceError::BadSignature
    ));
}

#[test]
fn pinned_key_from_bytes_roundtrips() {
    let mut rng = UnwrapErr(SysRng);
    let key = SigningKey::generate(&mut rng);
    let vk = key.verifying_key();
    let pinned = PinnedKey::from_bytes(vk.to_bytes()).expect("32-byte key must parse");
    let lease = sample_lease();
    let signed = sign_with(&key, &lease);
    assert!(verify_signed_lease(&signed, &pinned, None).is_ok());
}

#[test]
fn pinned_key_rejects_wrong_length() {
    let err = PinnedKey::from_slice(&[0_u8; 16]).expect_err("16 bytes is not a key");
    assert!(matches!(err, LicenceError::MalformedKey));
}

#[test]
fn signing_bytes_are_deterministic() {
    // Canonical signing bytes must be stable for the same lease + binding id (so a
    // portal and the machine agree on what was signed).
    let lease = sample_lease();
    let a = SignedLease::signing_bytes(&lease, None);
    let b = SignedLease::signing_bytes(&lease.clone(), None);
    assert_eq!(a, b);
    // A different serial yields different signing bytes (the field is covered).
    let mut other = lease.clone();
    other.serial = "serial-DIFFER".to_owned();
    assert_ne!(a, SignedLease::signing_bytes(&other, None));
    // The instance binding id is COVERED: None, Some(""), and Some("x") are three
    // distinct signed values, so a grafted/changed/absent binding id is
    // tamper-evident (the binding-anchor signature contract).
    let none = SignedLease::signing_bytes(&lease, None);
    let empty = SignedLease::signing_bytes(&lease, Some(""));
    let some = SignedLease::signing_bytes(&lease, Some("ib_x"));
    assert_ne!(none, empty, "None must sign distinctly from Some(\"\")");
    assert_ne!(none, some, "the binding id is covered by the signing bytes");
    assert_ne!(empty, some, "different binding ids sign differently");
}
