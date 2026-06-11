//! OAuth 2.0 / **JWT** (RFC 7519) bearer-token validation — a pure, always-
//! compiled HS256 validator that is IS-10-aligned (broadcast-multiviewer brief
//! §8; AMWA NMOS IS-10).
//!
//! This is an **alternative** to the native API-key/session path: a deployment
//! configured with a [`JwtValidator`] accepts OAuth 2.0 bearer JWTs whose claims
//! are the IS-10 [`Is10Claims`] set, mapping the NMOS access grant onto the
//! crate's [`Role`](crate::Role).
//!
//! ## What this validator guarantees (security-critical)
//!
//! [`JwtValidator::validate`] performs the full check, in order, refusing the
//! token unless every step passes:
//!
//! 1. **Shape.** The token is exactly three base64url segments
//!    (`header.payload.signature`); anything else is [`JwtError::Malformed`].
//! 2. **Algorithm.** The header `alg` MUST be a supported, *signed* algorithm.
//!    **`alg=none` is rejected** ([`JwtError::UnsupportedAlgorithm`]) — the
//!    canonical JWT downgrade attack — as is any algorithm this pure validator
//!    does not implement (it never silently trusts an unverified token).
//! 3. **Signature.** The HS256 HMAC-SHA256 over the ASCII `header.payload`
//!    signing input is recomputed under the configured secret and compared to
//!    the presented signature in **constant time** ([`subtle`]). A mismatch is
//!    [`JwtError::BadSignature`]; a tampered header or payload therefore fails
//!    here before any claim is trusted.
//! 4. **Registered claims.** Issuer (`iss`), audience (`aud`), and expiry
//!    (`exp`) are validated against this resource server's policy via
//!    [`Is10Claims::validate`] ([`JwtError::Claims`] on failure).
//!
//! Only after all four pass are the decoded [`Is10Claims`] returned for the
//! caller to map to a [`Role`](crate::Role) with [`Is10Claims::role_for`].
//!
//! ## Asymmetric algorithms (RS256/ES256) and JWKS
//!
//! Production IS-10 deployments commonly sign with RS256/ES256 against the
//! authorization server's JWKS. Those algorithms need an asymmetric-crypto stack
//! and live key material; verifying them is a transport/deployment concern done
//! at the gated `nmos` boundary, exactly as the [`crate::nmos::is10`] claims
//! model documents. This pure module implements the **symmetric HS256** path so
//! the default build stays pure-Rust and dependency-light while still giving a
//! complete, *signature-verifying* (never `alg=none`-trusting) JWT validator the
//! whole crate and its tests can exercise without native crypto.
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use hmac::{Hmac, Mac};
use sha2::Sha256;
use subtle::ConstantTimeEq;

use crate::nmos::is10::{Is10Claims, Is10Error};

type HmacSha256 = Hmac<Sha256>;

/// A JWS signing algorithm this validator understands.
///
/// Intentionally tiny: only the symmetric **HS256** is implemented by the pure
/// path. [`SignatureAlgorithm::parse`] returns [`None`] for `alg=none` and for
/// asymmetric algorithms (`RS256`/`ES256`/…), so the validator can refuse them
/// explicitly rather than ever treating an unverifiable token as valid.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum SignatureAlgorithm {
    /// HMAC-SHA256 (`alg: "HS256"`).
    Hs256,
}

impl SignatureAlgorithm {
    /// Parse the JOSE header `alg` value, accepting only supported *signed*
    /// algorithms.
    ///
    /// Returns [`None`] for `"none"` (the downgrade attack) and for any
    /// algorithm this pure validator does not implement.
    #[must_use]
    pub fn parse(alg: &str) -> Option<Self> {
        match alg {
            "HS256" => Some(Self::Hs256),
            _ => None,
        }
    }

    /// The canonical JOSE `alg` string for this algorithm.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Hs256 => "HS256",
        }
    }
}

/// Why a JWT was rejected.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum JwtError {
    /// The compact serialization was not three base64url segments, or a segment
    /// was not valid base64url / valid JSON.
    #[error("malformed JWT: not three valid base64url segments")]
    Malformed,

    /// The header `alg` is unsupported — including the forbidden `alg=none` and
    /// any asymmetric algorithm this pure validator does not implement.
    #[error("unsupported or forbidden JWT algorithm {alg:?}")]
    UnsupportedAlgorithm {
        /// The `alg` value the token presented.
        alg: String,
    },

    /// The signature did not verify under the configured key.
    #[error("JWT signature verification failed")]
    BadSignature,

    /// The signature verified but a registered claim (issuer/audience/expiry)
    /// failed this resource server's policy.
    #[error("JWT claims rejected: {0}")]
    Claims(#[from] Is10Error),
}

/// The minimal JOSE header fields this validator inspects.
#[derive(Debug, serde::Deserialize)]
struct JoseHeader {
    alg: String,
}

/// A validator for OAuth 2.0 bearer **JWTs** (HS256), configured with the signing
/// secret and the issuer/audience policy this resource server enforces.
///
/// Holds only configuration; it has no I/O and never touches the engine data
/// plane, so it cannot back-pressure the engine (invariant #10).
#[derive(Clone)]
pub struct JwtValidator {
    secret: Vec<u8>,
    expected_issuer: String,
    expected_audience: String,
}

impl std::fmt::Debug for JwtValidator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never print the signing secret.
        f.debug_struct("JwtValidator")
            .field("expected_issuer", &self.expected_issuer)
            .field("expected_audience", &self.expected_audience)
            .field("secret", &"<redacted>")
            .finish()
    }
}

impl JwtValidator {
    /// Build an HS256 validator from the shared signing `secret` and the
    /// issuer/audience this resource server requires.
    #[must_use]
    pub fn new_hs256(
        secret: impl Into<Vec<u8>>,
        expected_issuer: impl Into<String>,
        expected_audience: impl Into<String>,
    ) -> Self {
        Self {
            secret: secret.into(),
            expected_issuer: expected_issuer.into(),
            expected_audience: expected_audience.into(),
        }
    }

    /// Validate a compact-serialized JWT at validation time `now` (Unix
    /// seconds), returning its decoded [`Is10Claims`] on success.
    ///
    /// Runs shape → algorithm (`alg=none` refused) → constant-time signature →
    /// registered-claim checks, in that order. See the module docs for the full
    /// guarantee.
    ///
    /// # Errors
    ///
    /// - [`JwtError::Malformed`] — not three valid base64url/JSON segments.
    /// - [`JwtError::UnsupportedAlgorithm`] — `alg=none` or an unimplemented alg.
    /// - [`JwtError::BadSignature`] — the signature did not verify.
    /// - [`JwtError::Claims`] — issuer/audience/expiry policy failed.
    pub fn validate(&self, token: &str, now: i64) -> Result<Is10Claims, JwtError> {
        // 1. Shape: exactly three dot-separated segments.
        let mut segments = token.split('.');
        let (Some(header_b64), Some(payload_b64), Some(sig_b64), None) = (
            segments.next(),
            segments.next(),
            segments.next(),
            segments.next(),
        ) else {
            return Err(JwtError::Malformed);
        };

        // Decode header + payload from base64url (no padding).
        let header_bytes = URL_SAFE_NO_PAD
            .decode(header_b64)
            .map_err(|_| JwtError::Malformed)?;
        let payload_bytes = URL_SAFE_NO_PAD
            .decode(payload_b64)
            .map_err(|_| JwtError::Malformed)?;
        let signature = URL_SAFE_NO_PAD
            .decode(sig_b64)
            .map_err(|_| JwtError::Malformed)?;

        // 2. Algorithm gate — refuse `alg=none` and any unimplemented alg BEFORE
        //    any signature/claim logic runs.
        let header: JoseHeader =
            serde_json::from_slice(&header_bytes).map_err(|_| JwtError::Malformed)?;
        let Some(SignatureAlgorithm::Hs256) = SignatureAlgorithm::parse(&header.alg) else {
            return Err(JwtError::UnsupportedAlgorithm { alg: header.alg });
        };

        // 3. Signature: recompute HS256 over the exact `header.payload` ASCII the
        //    token presented (re-encoding could differ), compare constant-time.
        //    Use the byte length of the header/payload segments to slice the
        //    original signing input without re-encoding.
        let signing_input_len = header_b64.len() + 1 + payload_b64.len();
        let signing_input = token.get(..signing_input_len).ok_or(JwtError::Malformed)?;
        let Ok(mut mac) = <HmacSha256 as Mac>::new_from_slice(&self.secret) else {
            // `new_from_slice` is infallible for HMAC (any key length); this arm
            // is defensive and keeps the method total without unwrap/expect.
            return Err(JwtError::BadSignature);
        };
        mac.update(signing_input.as_bytes());
        let expected_sig = mac.finalize().into_bytes();
        if !bool::from(expected_sig.ct_eq(&signature)) {
            return Err(JwtError::BadSignature);
        }

        // 4. Registered claims: issuer / audience / expiry.
        let claims: Is10Claims =
            serde_json::from_slice(&payload_bytes).map_err(|_| JwtError::Malformed)?;
        claims.validate(now, &self.expected_issuer, &self.expected_audience)?;
        Ok(claims)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::{JwtError, JwtValidator, SignatureAlgorithm};
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use base64::Engine as _;
    use hmac::{Hmac, Mac};
    use serde_json::json;
    use sha2::Sha256;

    type HmacSha256 = Hmac<Sha256>;

    const SECRET: &[u8] = b"unit-test-secret";

    fn sign(header: &serde_json::Value, payload: &serde_json::Value, secret: &[u8]) -> String {
        let input = format!(
            "{}.{}",
            URL_SAFE_NO_PAD.encode(serde_json::to_vec(header).unwrap()),
            URL_SAFE_NO_PAD.encode(serde_json::to_vec(payload).unwrap())
        );
        let mut mac = <HmacSha256 as Mac>::new_from_slice(secret).unwrap();
        mac.update(input.as_bytes());
        format!(
            "{input}.{}",
            URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes())
        )
    }

    fn payload(exp: i64) -> serde_json::Value {
        json!({
            "iss": "iss-x", "sub": "s", "aud": ["aud-x"], "exp": exp, "iat": 0,
            "x-nmos-api": { "version": "1.0", "access": { "query": "read" } }
        })
    }

    fn validator() -> JwtValidator {
        JwtValidator::new_hs256(SECRET.to_vec(), "iss-x", "aud-x")
    }

    #[test]
    fn happy_path_decodes_claims() {
        let token = sign(&json!({ "alg": "HS256" }), &payload(100), SECRET);
        let claims = validator().validate(&token, 50).unwrap();
        assert_eq!(claims.iss, "iss-x");
    }

    #[test]
    fn alg_none_refused() {
        let token = sign(&json!({ "alg": "none" }), &payload(100), SECRET);
        assert!(matches!(
            validator().validate(&token, 50),
            Err(JwtError::UnsupportedAlgorithm { .. })
        ));
    }

    #[test]
    fn bad_secret_fails_signature() {
        let token = sign(&json!({ "alg": "HS256" }), &payload(100), b"other");
        assert!(matches!(
            validator().validate(&token, 50),
            Err(JwtError::BadSignature)
        ));
    }

    #[test]
    fn parse_rejects_none_and_asymmetric() {
        assert_eq!(
            SignatureAlgorithm::parse("HS256"),
            Some(SignatureAlgorithm::Hs256)
        );
        assert_eq!(SignatureAlgorithm::parse("none"), None);
        assert_eq!(SignatureAlgorithm::parse("ES256"), None);
        assert_eq!(SignatureAlgorithm::Hs256.as_str(), "HS256");
    }
}
