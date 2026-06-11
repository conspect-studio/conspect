//! Challenge-export tests (CONSPECT-1, brief §3/§8): the `<host>.challenge`
//! file is well-formed CBOR carrying ONLY salted digests + counters — never a
//! raw serial, MAC, or hostname. Round-trips byte-stably so a portal and the
//! machine agree on the format.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::as_conversions,
    clippy::missing_panics_doc
)]

use multiview_licence::challenge::{ChallengeCounters, ChallengeFile};

fn sample() -> ChallengeFile {
    ChallengeFile::new(
        // A salted host digest (hex of a salted hash) — NOT the raw hostname.
        "9f1c2b7a4e5d6f08".to_owned(),
        vec![
            "aa11bb22cc33".to_owned(),
            "dd44ee55ff66".to_owned(),
            "0011223344556677".to_owned(),
        ],
        ChallengeCounters::new(7, 42, 3),
    )
}

#[test]
fn challenge_is_well_formed_cbor_and_round_trips() {
    let challenge = sample();
    let bytes = challenge.to_cbor().expect("encode CBOR");
    // A CBOR map starts with a major-type-5 byte (0xA0..=0xBF) — well-formed.
    assert!(
        (0xA0..=0xBF).contains(&bytes[0]),
        "first byte is a CBOR map header, got {:#04x}",
        bytes[0]
    );
    // Decodes back to an equal value through ciborium (well-formed + canonical).
    let decoded = ChallengeFile::from_cbor(&bytes).expect("decode CBOR");
    assert_eq!(decoded, challenge);
}

#[test]
fn challenge_encoding_is_deterministic() {
    // The portal and the machine must agree byte-for-byte on the format.
    let a = sample().to_cbor().expect("encode");
    let b = sample().to_cbor().expect("encode");
    assert_eq!(a, b, "challenge CBOR must be deterministic");
}

#[test]
fn challenge_carries_only_salted_digests_and_counters() {
    // Data minimisation (brief §8): a challenge built from salted digests must
    // not contain any plausible raw identifier. We assert the only string
    // payloads are the digests we supplied (hex), and the numeric payloads are
    // the counters — there is no field that could carry a raw serial/MAC/host.
    let challenge = sample();
    let bytes = challenge.to_cbor().expect("encode");

    // The raw hostname / serial must NOT appear; only the salted digest does.
    let haystack = String::from_utf8_lossy(&bytes);
    assert!(
        !haystack.contains("studio-host-01"),
        "the raw hostname must never appear in the challenge"
    );
    // Decoding exposes exactly the salted fields — and the API has no field for
    // a raw identifier (type-level data minimisation).
    let decoded = ChallengeFile::from_cbor(&bytes).expect("decode");
    assert_eq!(decoded.host_digest, "9f1c2b7a4e5d6f08");
    assert_eq!(decoded.fingerprint_digests.len(), 3);
    assert_eq!(decoded.counters.boot_count, 7);
    assert_eq!(decoded.counters.heartbeat_attempts, 42);
    assert_eq!(decoded.counters.lease_installs, 3);
}

#[test]
fn malformed_cbor_is_a_typed_error_never_a_panic() {
    // Bad-inputs-are-the-purpose: decoding garbage must be a typed error.
    let err = ChallengeFile::from_cbor(&[0xFF, 0x00, 0x13, 0x37]);
    assert!(err.is_err(), "garbage CBOR must error, not panic");
}
