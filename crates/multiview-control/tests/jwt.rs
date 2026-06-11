//! OAuth 2.0 / JWT (RFC 7519) authentication tests for the pure validator.
//!
//! These exercise the pure, always-compiled HS256 JWT validator: a well-formed
//! token with a correct signature, audience, expiry, and issuer is accepted and
//! its claims map to a [`Role`]; tokens with a bad signature, wrong audience,
//! expired `exp`, wrong issuer, or the forbidden `alg=none` are rejected. No
//! network, no JWKS — the signing key is supplied directly (IS-10-aligned
//! claims model).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use hmac::{Hmac, Mac};
use multiview_control::{
    Is10Claims, JwtError, JwtValidator, NmosAccess, NmosApiClaim, Role, SignatureAlgorithm,
};
use serde_json::json;
use sha2::Sha256;
use std::collections::BTreeMap;

type HmacSha256 = Hmac<Sha256>;

const SECRET: &[u8] = b"shared-hmac-signing-secret-32bytes!";
const ISSUER: &str = "https://auth.facility.example";
const AUDIENCE: &str = "multiview";
const NOW: i64 = 1_800_000_000;

/// Base64url-encode without padding (JWT segment encoding).
fn b64(bytes: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(bytes)
}

/// Mint a signed compact JWT from a header + payload JSON, signing with HS256
/// over the `header.payload` ASCII using `secret`.
fn sign_hs256(header: &serde_json::Value, payload: &serde_json::Value, secret: &[u8]) -> String {
    let signing_input = format!(
        "{}.{}",
        b64(serde_json::to_vec(header).unwrap().as_slice()),
        b64(serde_json::to_vec(payload).unwrap().as_slice())
    );
    let mut mac = <HmacSha256 as Mac>::new_from_slice(secret).unwrap();
    mac.update(signing_input.as_bytes());
    let sig = mac.finalize().into_bytes();
    format!("{signing_input}.{}", b64(&sig))
}

/// A valid IS-10-style claims payload as JSON.
fn claims_json(exp: i64) -> serde_json::Value {
    json!({
        "iss": ISSUER,
        "sub": "operator-7",
        "aud": [AUDIENCE],
        "exp": exp,
        "iat": NOW - 600,
        "x-nmos-api": { "version": "1.0", "access": { "connection": "write" } }
    })
}

fn validator() -> JwtValidator {
    JwtValidator::new_hs256(SECRET.to_vec(), ISSUER, AUDIENCE)
}

#[test]
fn valid_hs256_token_is_accepted_and_claims_decode() {
    let token = sign_hs256(
        &json!({ "alg": "HS256", "typ": "JWT" }),
        &claims_json(2_000_000_000),
        SECRET,
    );
    let claims: Is10Claims = validator()
        .validate(&token, NOW)
        .expect("a correctly signed, current, in-audience token must validate");
    assert_eq!(claims.iss, ISSUER);
    assert_eq!(claims.sub, "operator-7");
    // The NMOS write grant maps to Operator.
    assert_eq!(claims.role_for("connection").unwrap(), Role::Operator);
}

#[test]
fn bad_signature_is_rejected() {
    // Signed with a DIFFERENT secret than the validator trusts.
    let token = sign_hs256(
        &json!({ "alg": "HS256", "typ": "JWT" }),
        &claims_json(2_000_000_000),
        b"attacker-controlled-other-secret",
    );
    let err = validator().validate(&token, NOW).unwrap_err();
    assert!(matches!(err, JwtError::BadSignature), "{err:?}");
}

#[test]
fn tampered_payload_breaks_the_signature() {
    let token = sign_hs256(
        &json!({ "alg": "HS256", "typ": "JWT" }),
        &claims_json(2_000_000_000),
        SECRET,
    );
    // Swap the payload segment for a forged one (escalating to admin-like grant)
    // while keeping the original signature — must fail signature verification.
    let mut parts: Vec<&str> = token.split('.').collect();
    let forged = b64(serde_json::to_vec(&json!({
        "iss": ISSUER, "sub": "attacker", "aud": [AUDIENCE], "exp": 2_000_000_000i64, "iat": NOW,
        "x-nmos-api": { "version": "1.0", "access": { "*": "write" } }
    }))
    .unwrap()
    .as_slice());
    parts[1] = &forged;
    let forged_token = parts.join(".");
    let err = validator().validate(&forged_token, NOW).unwrap_err();
    assert!(matches!(err, JwtError::BadSignature), "{err:?}");
}

#[test]
fn alg_none_is_rejected_even_with_empty_signature() {
    // The classic "alg":"none" downgrade: header claims no algorithm and the
    // signature segment is empty. MUST be rejected regardless of claims.
    let signing_input = format!(
        "{}.{}",
        b64(serde_json::to_vec(&json!({ "alg": "none", "typ": "JWT" }))
            .unwrap()
            .as_slice()),
        b64(serde_json::to_vec(&claims_json(2_000_000_000))
            .unwrap()
            .as_slice())
    );
    let token = format!("{signing_input}."); // empty signature
    let err = validator().validate(&token, NOW).unwrap_err();
    assert!(
        matches!(err, JwtError::UnsupportedAlgorithm { .. }),
        "{err:?}"
    );
}

#[test]
fn alg_none_with_forged_signature_is_still_rejected() {
    // Even if a "none" token carries a bogus non-empty signature segment, the
    // algorithm itself must be refused before any signature logic runs.
    let token = sign_hs256(
        &json!({ "alg": "none", "typ": "JWT" }),
        &claims_json(2_000_000_000),
        SECRET,
    );
    let err = validator().validate(&token, NOW).unwrap_err();
    assert!(
        matches!(err, JwtError::UnsupportedAlgorithm { .. }),
        "{err:?}"
    );
}

#[test]
fn wrong_audience_is_rejected() {
    let payload = json!({
        "iss": ISSUER, "sub": "x", "aud": ["some-other-service"], "exp": 2_000_000_000i64,
        "iat": NOW, "x-nmos-api": { "version": "1.0", "access": { "connection": "read" } }
    });
    let token = sign_hs256(&json!({ "alg": "HS256", "typ": "JWT" }), &payload, SECRET);
    let err = validator().validate(&token, NOW).unwrap_err();
    assert!(matches!(err, JwtError::Claims(_)), "{err:?}");
}

#[test]
fn expired_token_is_rejected() {
    // exp in the past relative to NOW.
    let token = sign_hs256(
        &json!({ "alg": "HS256", "typ": "JWT" }),
        &claims_json(NOW - 1),
        SECRET,
    );
    let err = validator().validate(&token, NOW).unwrap_err();
    assert!(matches!(err, JwtError::Claims(_)), "{err:?}");
}

#[test]
fn wrong_issuer_is_rejected() {
    let payload = json!({
        "iss": "https://evil.example", "sub": "x", "aud": [AUDIENCE], "exp": 2_000_000_000i64,
        "iat": NOW, "x-nmos-api": { "version": "1.0", "access": { "connection": "read" } }
    });
    let token = sign_hs256(&json!({ "alg": "HS256", "typ": "JWT" }), &payload, SECRET);
    let err = validator().validate(&token, NOW).unwrap_err();
    assert!(matches!(err, JwtError::Claims(_)), "{err:?}");
}

#[test]
fn malformed_token_shapes_are_rejected() {
    let v = validator();
    // Not three dot-separated segments.
    assert!(matches!(
        v.validate("only.two", NOW),
        Err(JwtError::Malformed)
    ));
    assert!(matches!(
        v.validate("a.b.c.d", NOW),
        Err(JwtError::Malformed)
    ));
    // Non-base64url payload segment.
    assert!(matches!(
        v.validate("aGVhZGVy.!!!notb64!!!.c2ln", NOW),
        Err(JwtError::Malformed)
    ));
}

#[test]
fn read_only_grant_maps_to_viewer_role() {
    let mut access = BTreeMap::new();
    access.insert("query".to_owned(), NmosAccess::Read);
    let claims = Is10Claims {
        iss: ISSUER.to_owned(),
        sub: "viewer".to_owned(),
        aud: vec![AUDIENCE.to_owned()],
        exp: 2_000_000_000,
        iat: NOW,
        x_nmos_api: NmosApiClaim {
            version: "1.0".to_owned(),
            access,
        },
    };
    assert_eq!(claims.role_for("query").unwrap(), Role::Viewer);
}

#[test]
fn signature_algorithm_parses_only_supported_symmetric_alg() {
    assert_eq!(
        SignatureAlgorithm::parse("HS256"),
        Some(SignatureAlgorithm::Hs256)
    );
    // alg=none and asymmetric algs are not handled by the pure symmetric path.
    assert_eq!(SignatureAlgorithm::parse("none"), None);
    assert_eq!(SignatureAlgorithm::parse("RS256"), None);
}
