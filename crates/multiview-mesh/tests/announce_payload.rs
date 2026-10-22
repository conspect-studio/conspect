//! Data-minimisation is pinned by test (ADR-0051 §2, brief §8): the announce
//! payload carries **only** the allowed fields — protocol version, the salted
//! fingerprint digest set, the claim state, and a signed entitlement summary
//! (level + lease bounds + signature). It must carry **no** raw identifier
//! (serial / MAC / URL / hostname / media / config) and **no** field that could
//! hold one. This test enumerates the serialised payload's keys exhaustively so a
//! future field that leaks identity fails the build.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::collections::BTreeSet;

use chrono::{DateTime, Utc};
use ed25519_dalek::rand_core::UnwrapErr;
use ed25519_dalek::SigningKey;
use getrandom::SysRng;
use multiview_licence::EnforcementLevel;
use multiview_mesh::announce::{
    AnnouncePayload, EntitlementSummary, SaltedDigest, ANNOUNCE_PROTOCOL_VERSION,
};
use multiview_mesh::ClaimState;

fn epoch() -> DateTime<Utc> {
    DateTime::from_timestamp(1_700_000_000, 0).unwrap()
}

/// A signed announce payload built by the originating machine's own key.
fn signed_payload(key: &SigningKey, claim: ClaimState) -> AnnouncePayload {
    let granted = epoch();
    let expires = granted + chrono::Duration::days(35);
    let summary = EntitlementSummary::new(EnforcementLevel::Active, granted, expires);
    let digests = vec![SaltedDigest::new([0x11; 32]), SaltedDigest::new([0x22; 32])];
    AnnouncePayload::sign(ANNOUNCE_PROTOCOL_VERSION, digests, claim, summary, key)
}

#[test]
fn announce_payload_carries_only_the_allowed_top_level_fields() {
    let key = SigningKey::generate(&mut UnwrapErr(SysRng));
    let payload = signed_payload(&key, ClaimState::Claimed);
    let json = serde_json::to_value(&payload).expect("payload serialises");
    let obj = json.as_object().expect("payload is a JSON object");

    let keys: BTreeSet<&str> = obj.keys().map(String::as_str).collect();
    // The EXHAUSTIVE allowed set. Anything else is a data-minimisation leak.
    let allowed: BTreeSet<&str> = [
        "protocol_version",
        "digests",
        "claim_state",
        "entitlement",
        "signature",
    ]
    .into_iter()
    .collect();
    assert_eq!(
        keys, allowed,
        "the announce payload must carry ONLY {allowed:?}, got {keys:?} — a new field is a data-minimisation leak"
    );
}

#[test]
fn entitlement_summary_carries_only_level_and_lease_bounds() {
    let key = SigningKey::generate(&mut UnwrapErr(SysRng));
    let payload = signed_payload(&key, ClaimState::Claimed);
    let json = serde_json::to_value(&payload).expect("serialises");
    let summary = json["entitlement"].as_object().expect("summary object");
    let keys: BTreeSet<&str> = summary.keys().map(String::as_str).collect();
    let allowed: BTreeSet<&str> = ["level", "granted_at", "expires_at"].into_iter().collect();
    assert_eq!(
        keys, allowed,
        "the entitlement summary advertises ONLY the level + lease bounds (no tier string, no serial)"
    );
}

#[test]
fn no_raw_identifier_appears_anywhere_in_the_serialised_payload() {
    // Build a payload and assert the full serialised text contains none of the
    // identifier-shaped substrings. The salted digests are opaque bytes (hex),
    // never a serial/MAC/URL/hostname.
    let key = SigningKey::generate(&mut UnwrapErr(SysRng));
    let payload = signed_payload(&key, ClaimState::Claimed);
    let text = serde_json::to_string(&payload).expect("serialises");
    for forbidden in [
        "serial", "mac", "hostname", "rtsp://", "http://", "https://", "url", "/dev/", "tier",
    ] {
        assert!(
            !text.contains(forbidden),
            "the announce wire must not contain {forbidden:?}; got {text}"
        );
    }
}

#[test]
fn an_unclaimed_machine_advertises_no_name() {
    // An UNCLAIMED machine advertises its claim state but carries no name field at
    // all (data minimisation: a name only exists once claimed, and even then it is
    // not in the announce — the peer learns it via confirm-adopt, brief §9.1).
    let key = SigningKey::generate(&mut UnwrapErr(SysRng));
    let payload = signed_payload(&key, ClaimState::Unclaimed);
    let json = serde_json::to_value(&payload).expect("serialises");
    assert!(json.get("name").is_none(), "the announce carries no name");
    assert_eq!(json["claim_state"], "unclaimed");
}

#[test]
fn a_valid_payload_verifies_against_the_originator_key() {
    // The announce is signed by the ORIGINATING machine's key so a peer detects a
    // spoofed/tampered announcement. A genuine payload verifies.
    let key = SigningKey::generate(&mut UnwrapErr(SysRng));
    let payload = signed_payload(&key, ClaimState::Claimed);
    assert!(
        payload.verify(&key.verifying_key()).is_ok(),
        "a genuine payload verifies against the originator key"
    );
}

#[test]
fn a_tampered_payload_fails_verification() {
    // Flip the advertised enforcement level AFTER signing: the signature must no
    // longer verify (a malicious relayer/peer cannot forge a stronger entitlement).
    let key = SigningKey::generate(&mut UnwrapErr(SysRng));
    let mut payload = signed_payload(&key, ClaimState::Claimed);
    payload.entitlement.level = EnforcementLevel::Watermark;
    assert!(
        payload.verify(&key.verifying_key()).is_err(),
        "a tampered entitlement summary must fail verification"
    );
}

#[test]
fn a_wrong_key_fails_verification() {
    let key = SigningKey::generate(&mut UnwrapErr(SysRng));
    let other = SigningKey::generate(&mut UnwrapErr(SysRng));
    let payload = signed_payload(&key, ClaimState::Claimed);
    assert!(
        payload.verify(&other.verifying_key()).is_err(),
        "a payload signed by a different machine must fail verification"
    );
}

#[test]
fn the_payload_round_trips_through_its_wire_form() {
    let key = SigningKey::generate(&mut UnwrapErr(SysRng));
    let payload = signed_payload(&key, ClaimState::Claimed);
    let bytes = payload.to_wire().expect("encode");
    let back = AnnouncePayload::from_wire(&bytes).expect("decode");
    assert_eq!(
        payload, back,
        "the payload round-trips through its wire form"
    );
    // And a decoded genuine payload still verifies.
    assert!(back.verify(&key.verifying_key()).is_ok());
}

#[test]
fn garbage_wire_bytes_are_a_typed_error_never_a_panic() {
    let err = AnnouncePayload::from_wire(&[0xFF, 0x00, 0x13, 0x37]);
    assert!(
        err.is_err(),
        "garbage bytes decode to a typed error, never a panic"
    );
}
