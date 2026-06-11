//! Token lifecycle: issue / verify / expiry / forgery-rejection.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::time::Duration;

use multiview_preview::{AccessScope, TapKey, TapScope, TokenError, TokenIssuer};

/// A deterministic, monotonic test clock: nanoseconds since an arbitrary epoch
/// that we advance manually so expiry is exact and flake-free.
fn issuer() -> TokenIssuer {
    // 32-byte secret; the issuer must accept any non-empty key material.
    TokenIssuer::new(b"super-secret-key-material-32-byte").expect("non-empty secret")
}

fn key() -> TapKey {
    TapKey::new(TapScope::Program, "program")
}

/// Nanoseconds in a whole-second `Duration`, as `i64` (no lossy `as` cast).
fn secs_ns(secs: u64) -> i64 {
    i64::try_from(Duration::from_secs(secs).as_nanos()).expect("ttl fits i64")
}

#[test]
fn issue_then_verify_roundtrips() {
    let iss = issuer();
    let k = key();
    let now = 1_000_000_000;
    let token = iss.issue(&k, AccessScope::View, now, Duration::from_secs(30));

    let claims = iss
        .verify(token.as_str(), now + 1_000_000)
        .expect("freshly issued token must verify");
    assert_eq!(claims.tap, k, "verified tap key must match issued key");
    assert_eq!(claims.access, AccessScope::View);
    assert_eq!(claims.expires_at_nanos, now + secs_ns(30));
}

#[test]
fn verify_rejects_after_expiry() {
    let iss = issuer();
    let now = 5_000_000_000;
    let ttl = Duration::from_secs(10);
    let token = iss.issue(&key(), AccessScope::View, now, ttl);

    // One nanosecond before expiry: still valid.
    let ttl_ns = i64::try_from(ttl.as_nanos()).expect("ttl fits i64");
    let just_before = now + ttl_ns - 1;
    assert!(iss.verify(token.as_str(), just_before).is_ok());

    // At/after the expiry instant: rejected as expired (NOT as forged).
    let at_expiry = now + ttl_ns;
    let err = iss.verify(token.as_str(), at_expiry).unwrap_err();
    assert!(
        matches!(err, TokenError::Expired { .. }),
        "expected Expired, got {err:?}"
    );
    let later = at_expiry + 1_000_000_000;
    assert!(matches!(
        iss.verify(token.as_str(), later),
        Err(TokenError::Expired { .. })
    ));
}

#[test]
fn verify_rejects_forged_signature() {
    let iss = issuer();
    let now = 7_000_000_000;
    let token = iss.issue(&key(), AccessScope::View, now, Duration::from_secs(60));

    // Flip the last character of the signature region; must fail the MAC check.
    let mut bytes = token.as_str().to_owned();
    let last = bytes.pop().expect("non-empty token");
    let replacement = if last == 'A' { 'B' } else { 'A' };
    bytes.push(replacement);

    let err = iss.verify(&bytes, now + 1).unwrap_err();
    assert!(
        matches!(err, TokenError::BadSignature | TokenError::Malformed(_)),
        "tampered token must be rejected, got {err:?}"
    );
}

#[test]
fn verify_rejects_payload_tamper() {
    let iss = issuer();
    let now = 9_000_000_000;
    // Issue a View token, then try to escalate the encoded claim to Focus by
    // editing the payload while keeping the original signature.
    let token = iss.issue(&key(), AccessScope::View, now, Duration::from_secs(60));
    let s = token.as_str();
    let dot = s.find('.').expect("token has a payload.signature split");
    let (payload, sig) = s.split_at(dot);
    // Corrupt one byte of the payload; the signature no longer matches.
    let mut tampered = payload.to_owned();
    let ch = tampered.pop().expect("non-empty payload");
    tampered.push(if ch == 'x' { 'y' } else { 'x' });
    tampered.push_str(sig);

    assert!(matches!(
        iss.verify(&tampered, now + 1),
        Err(TokenError::BadSignature | TokenError::Malformed(_))
    ));
}

#[test]
fn verify_rejects_other_issuer_key() {
    let a = issuer();
    let b = TokenIssuer::new(b"a-completely-different-secret-key").expect("non-empty");
    let now = 11_000_000_000;
    let token = a.issue(&key(), AccessScope::View, now, Duration::from_secs(60));

    // The other issuer must not validate a token it did not sign.
    assert!(matches!(
        b.verify(token.as_str(), now + 1),
        Err(TokenError::BadSignature)
    ));
    // The signing issuer still accepts it.
    assert!(a.verify(token.as_str(), now + 1).is_ok());
}

#[test]
fn verify_rejects_garbage_and_empty() {
    let iss = issuer();
    let now = 1;
    for bad in ["", "no-dot-here", ".", "a.b.c.d", "®©.€"] {
        assert!(
            matches!(
                iss.verify(bad, now),
                Err(TokenError::Malformed(_) | TokenError::BadSignature)
            ),
            "garbage {bad:?} must be rejected"
        );
    }
}

#[test]
fn empty_secret_is_rejected() {
    assert!(matches!(
        TokenIssuer::new(b""),
        Err(TokenError::EmptySecret)
    ));
}

#[test]
fn token_is_scoped_to_a_single_tap() {
    let iss = issuer();
    let now = 2_000_000_000;
    let token = iss.issue(
        &TapKey::new(TapScope::Input, "cam-1"),
        AccessScope::View,
        now,
        Duration::from_secs(30),
    );
    let claims = iss.verify(token.as_str(), now + 1).expect("valid");
    // A token minted for cam-1 must not authorize cam-2.
    assert_ne!(claims.tap, TapKey::new(TapScope::Input, "cam-2"));
    assert_eq!(claims.tap, TapKey::new(TapScope::Input, "cam-1"));
}
