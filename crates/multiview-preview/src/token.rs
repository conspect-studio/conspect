//! Short-lived **signed access tokens** for preview taps (HMAC-SHA256).
//!
//! Preview transports (MJPEG, snapshot, and — behind the off-by-default
//! `webrtc` feature — WHEP) are authenticated/authorized by the control plane.
//! A [`TokenIssuer`] mints a compact, self-describing bearer token that:
//!
//! * names exactly **one** [`TapKey`] (`scope` + entity id) and one
//!   [`AccessScope`] (view vs focus), so a token for `input/cam-1` can never be
//!   replayed against `input/cam-2` or escalated to a focus session;
//! * carries an absolute **expiry** (nanoseconds on the same monotonic timeline
//!   as the engine clock) so a leaked token is useless within seconds; and
//! * is **HMAC-SHA256 signed** over the canonical claim string, so a forged or
//!   tampered token is rejected (the MAC is verified in constant time via
//!   [`hmac`]'s `verify_slice`).
//!
//! The wire form is `payload.signature`, both lower-case hex. Hex (rather than a
//! base64 crate) keeps the codec dependency-free, total, and free of any
//! `indexing`/`as`-cast guardrail hazards. The `payload` is the canonical claim
//! string `v1|<scope>|<id>|<access>|<expires_ns>`; the `signature` is the hex of
//! `HMAC-SHA256(secret, payload_bytes)`.
//!
//! The clock is **injected** (the caller passes `now_nanos`), so issue/verify
//! are pure functions of their inputs and the tests are exact and flake-free.
use hmac::{Hmac, KeyInit, Mac};
use sha2::Sha256;
use std::time::Duration;
use thiserror::Error;

type HmacSha256 = Hmac<Sha256>;

/// Which family of pipeline entity a preview tap observes (see the preview
/// brief §1: the three scopes).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum TapScope {
    /// An individual input source (on-air tile slot or off-air cue decoder).
    Input,
    /// The composed program canvas (pre-encode downscale tap).
    Program,
    /// A single real encoded output / rendition (return-feed monitor).
    Output,
}

impl TapScope {
    /// The stable wire token for this scope (used in the signed claim string).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Input => "input",
            Self::Program => "program",
            Self::Output => "output",
        }
    }

    /// Parse a scope from its wire token.
    fn parse(s: &str) -> Option<Self> {
        match s {
            "input" => Some(Self::Input),
            "program" => Some(Self::Program),
            "output" => Some(Self::Output),
            _ => None,
        }
    }
}

/// Identifies one preview tap: a [`TapScope`] plus the entity id within it
/// (e.g. an input source id, an output id, or the singleton `"program"`).
///
/// This is the key the [`crate::TapRegistry`] refcounts on and the subject a
/// signed token authorizes.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TapKey {
    scope: TapScope,
    id: String,
}

impl TapKey {
    /// Build a tap key for `scope` and entity `id`.
    #[must_use]
    pub fn new(scope: TapScope, id: impl Into<String>) -> Self {
        Self {
            scope,
            id: id.into(),
        }
    }

    /// The scope family of this tap.
    #[must_use]
    pub const fn scope(&self) -> TapScope {
        self.scope
    }

    /// The entity id within the scope.
    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }
}

/// What a token authorizes the bearer to do with the tap.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum AccessScope {
    /// Cheap, default: snapshot / MJPEG thumbnail viewing.
    View,
    /// On-demand, capped: a low-latency focus session (WHEP). Strictly a
    /// superset of [`AccessScope::View`] in capability, granted separately so
    /// the cap on concurrent focus sessions is enforceable.
    Focus,
}

impl AccessScope {
    /// The stable wire token for this access level.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::View => "view",
            Self::Focus => "focus",
        }
    }

    fn parse(s: &str) -> Option<Self> {
        match s {
            "view" => Some(Self::View),
            "focus" => Some(Self::Focus),
            _ => None,
        }
    }
}

/// The verified claims carried by a valid token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenClaims {
    /// The single tap this token authorizes.
    pub tap: TapKey,
    /// The access level granted.
    pub access: AccessScope,
    /// Absolute expiry instant (nanoseconds on the engine timeline). The token
    /// is invalid once `now_nanos >= expires_at_nanos`.
    pub expires_at_nanos: i64,
}

/// A minted, signed bearer token in its `payload.signature` wire form.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreviewToken(String);

impl PreviewToken {
    /// The token as its wire string (`payload.signature`, lower-case hex).
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Consume the token into its owned wire string.
    #[must_use]
    pub fn into_string(self) -> String {
        self.0
    }
}

impl std::fmt::Display for PreviewToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Failures from issuing or verifying a preview token.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum TokenError {
    /// The issuer was constructed with empty secret key material.
    #[error("preview token secret must be non-empty")]
    EmptySecret,

    /// The token string is not well-formed (missing the `.` split, bad hex, or
    /// an unparseable claim field).
    #[error("malformed preview token: {0}")]
    Malformed(&'static str),

    /// The HMAC signature did not match — the token was forged or tampered with
    /// (or signed by a different secret).
    #[error("preview token signature is invalid")]
    BadSignature,

    /// The token's signature was valid but it has expired.
    #[error("preview token expired {by_nanos}ns ago")]
    Expired {
        /// How long ago (nanoseconds) the token expired.
        by_nanos: i64,
    },
}

/// Mints and verifies short-lived HMAC-SHA256 preview tokens.
///
/// Hold one per deployment, seeded from a securely-provisioned secret. Cloning
/// is cheap; all clones share the same signing key and therefore validate each
/// other's tokens.
///
/// The keyed MAC is built once at construction (HMAC accepts a key of any
/// length, so this is the one place the construction is performed) and cloned
/// per sign/verify — so the hot path needs no `unwrap`/`expect` and no fallible
/// re-keying.
#[derive(Clone)]
pub struct TokenIssuer {
    mac: HmacSha256,
}

impl std::fmt::Debug for TokenIssuer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never leak key material in logs/Debug output.
        f.debug_struct("TokenIssuer")
            .field("key", &"<redacted>")
            .finish()
    }
}

impl TokenIssuer {
    /// Build an issuer from `secret` key material.
    ///
    /// # Errors
    ///
    /// Returns [`TokenError::EmptySecret`] if `secret` is empty. (HMAC itself
    /// accepts any non-empty key length; we require non-empty so a misconfigured
    /// deployment with no secret cannot silently sign with an empty key.)
    pub fn new(secret: &[u8]) -> Result<Self, TokenError> {
        if secret.is_empty() {
            return Err(TokenError::EmptySecret);
        }
        // HMAC's `new_from_slice` is infallible for any key length; map the
        // impossible error to `EmptySecret` rather than `unwrap`/`expect` so the
        // function stays total under the no-panic guardrail.
        let mac = HmacSha256::new_from_slice(secret).map_err(|_| TokenError::EmptySecret)?;
        Ok(Self { mac })
    }

    /// Issue a token authorizing `access` to `tap`, valid from `now_nanos` for
    /// `ttl`.
    ///
    /// The expiry is `now_nanos + ttl` saturating into `i64`, so an absurd TTL
    /// can never overflow into a past instant.
    #[must_use]
    pub fn issue(
        &self,
        tap: &TapKey,
        access: AccessScope,
        now_nanos: i64,
        ttl: Duration,
    ) -> PreviewToken {
        let ttl_nanos = i64::try_from(ttl.as_nanos()).unwrap_or(i64::MAX);
        let expires_at_nanos = now_nanos.saturating_add(ttl_nanos);
        let payload = canonical_payload(tap, access, expires_at_nanos);
        let sig = self.sign(payload.as_bytes());
        let mut wire = encode_hex(payload.as_bytes());
        wire.push('.');
        wire.push_str(&encode_hex(&sig));
        PreviewToken(wire)
    }

    /// Verify a token string against the current `now_nanos`, returning its
    /// claims if the signature matches and it has not expired.
    ///
    /// # Errors
    ///
    /// * [`TokenError::Malformed`] — the wire form / hex / claim string is not
    ///   well-formed.
    /// * [`TokenError::BadSignature`] — the HMAC did not verify (forged,
    ///   tampered, or signed with a different secret).
    /// * [`TokenError::Expired`] — the signature is valid but the token has
    ///   expired as of `now_nanos`.
    pub fn verify(&self, token: &str, now_nanos: i64) -> Result<TokenClaims, TokenError> {
        let (payload_hex, sig_hex) = token
            .split_once('.')
            .ok_or(TokenError::Malformed("missing payload.signature separator"))?;
        let payload_bytes =
            decode_hex(payload_hex).ok_or(TokenError::Malformed("payload is not valid hex"))?;
        let sig_bytes =
            decode_hex(sig_hex).ok_or(TokenError::Malformed("signature is not valid hex"))?;

        // Constant-time MAC verification: reject forged/tampered tokens before
        // trusting any field of the payload.
        let mut mac = self.mac();
        mac.update(&payload_bytes);
        mac.verify_slice(&sig_bytes)
            .map_err(|_| TokenError::BadSignature)?;

        // Signature is authentic; the payload is trustworthy. Parse it.
        let payload = std::str::from_utf8(&payload_bytes)
            .map_err(|_| TokenError::Malformed("payload is not valid UTF-8"))?;
        let claims = parse_payload(payload)?;

        if now_nanos >= claims.expires_at_nanos {
            return Err(TokenError::Expired {
                by_nanos: now_nanos.saturating_sub(claims.expires_at_nanos),
            });
        }
        Ok(claims)
    }

    /// A fresh keyed MAC instance, cloned from the one built at construction
    /// (so no fallible re-keying happens on the hot path).
    fn mac(&self) -> HmacSha256 {
        self.mac.clone()
    }

    fn sign(&self, payload: &[u8]) -> Vec<u8> {
        let mut mac = self.mac();
        mac.update(payload);
        mac.finalize().into_bytes().to_vec()
    }
}

/// Build the canonical claim string that is signed and verified verbatim.
fn canonical_payload(tap: &TapKey, access: AccessScope, expires_at_nanos: i64) -> String {
    // The id is hex-encoded inside the payload too (then the whole payload is
    // hex-encoded on the wire), so a `|` or `.` in an id can never break the
    // field/segment framing.
    format!(
        "v1|{}|{}|{}|{}",
        tap.scope().as_str(),
        encode_hex(tap.id().as_bytes()),
        access.as_str(),
        expires_at_nanos,
    )
}

/// Parse a verified canonical claim string back into [`TokenClaims`].
fn parse_payload(payload: &str) -> Result<TokenClaims, TokenError> {
    let mut parts = payload.split('|');
    let version = parts.next().ok_or(TokenError::Malformed("empty claim"))?;
    if version != "v1" {
        return Err(TokenError::Malformed("unsupported claim version"));
    }
    let scope_str = parts.next().ok_or(TokenError::Malformed("missing scope"))?;
    let id_hex = parts.next().ok_or(TokenError::Malformed("missing id"))?;
    let access_str = parts
        .next()
        .ok_or(TokenError::Malformed("missing access"))?;
    let exp_str = parts
        .next()
        .ok_or(TokenError::Malformed("missing expiry"))?;
    if parts.next().is_some() {
        return Err(TokenError::Malformed("trailing claim fields"));
    }

    let scope = TapScope::parse(scope_str).ok_or(TokenError::Malformed("unknown scope"))?;
    let id_bytes = decode_hex(id_hex).ok_or(TokenError::Malformed("id is not valid hex"))?;
    let id = String::from_utf8(id_bytes).map_err(|_| TokenError::Malformed("id is not UTF-8"))?;
    let access = AccessScope::parse(access_str).ok_or(TokenError::Malformed("unknown access"))?;
    let expires_at_nanos = exp_str
        .parse::<i64>()
        .map_err(|_| TokenError::Malformed("expiry is not an integer"))?;

    Ok(TokenClaims {
        tap: TapKey::new(scope, id),
        access,
        expires_at_nanos,
    })
}

/// Lower-case hex encode (no `as` casts, total).
fn encode_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len().saturating_mul(2));
    for &b in bytes {
        let hi = usize::from(b >> 4);
        let lo = usize::from(b & 0x0f);
        if let (Some(&h), Some(&l)) = (HEX.get(hi), HEX.get(lo)) {
            out.push(char::from(h));
            out.push(char::from(l));
        }
    }
    out
}

/// Decode lower/upper-case hex; returns [`None`] on any non-hex input or an odd
/// length. Total and index-safe.
fn decode_hex(s: &str) -> Option<Vec<u8>> {
    let bytes = s.as_bytes();
    if bytes.len() % 2 != 0 {
        return None;
    }
    let mut out = Vec::with_capacity(bytes.len() / 2);
    let mut chunks = bytes.chunks_exact(2);
    for pair in &mut chunks {
        let hi = hex_val(*pair.first()?)?;
        let lo = hex_val(*pair.get(1)?)?;
        out.push((hi << 4) | lo);
    }
    Some(out)
}

/// Map one hex ASCII byte to its nibble value, or [`None`].
fn hex_val(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}
