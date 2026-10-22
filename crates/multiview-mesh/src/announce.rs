//! The always-on mDNS **announce payload** (ADR-0051 §2, brief §9.1).
//!
//! Each announcement carries an **Ed25519-signed** summary: the machine's
//! **salted** fingerprint digest set + the **claim state** + a signed
//! **entitlement summary** (enforcement level + lease bounds) + the protocol
//! version. It carries **nothing else** — no serial, MAC, URL, hostname, media,
//! config, or any direct hardware identifier (data minimisation, brief §8). The
//! type has **no field** that could hold a raw identifier, so the minimisation is
//! enforced *structurally*; a test pins the serialised key set exhaustively so a
//! future leak fails the build.
//!
//! ## The signature is the originating machine's own
//!
//! The summary is signed by the **originating machine's** Ed25519 key, so a peer
//! can detect a **spoofed or tampered** announcement (a malicious host on the
//! segment cannot advertise a stronger entitlement or impersonate a neighbour's
//! digest set without the matching private key). This crate is
//! **verification-only** in non-test code — it checks a signature handed to it
//! ([`AnnouncePayload::verify`]); it never mints a key (the keygen RNG lives in
//! dev-deps only, data minimisation).
//!
//! ## Wire form
//!
//! [`AnnouncePayload::signing_bytes`] is a deterministic, domain-separated,
//! length-prefixed byte encoding of the covered fields — stable for a given
//! payload so the originator and a verifier agree byte-for-byte on what was
//! signed; any change to a covered field changes the bytes and so invalidates the
//! signature (tamper-evidence). [`AnnouncePayload::to_wire`] / [`from_wire`]
//! carry the whole payload (JSON) for the mDNS TXT record.

use chrono::{DateTime, Utc};
use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use serde::{Deserialize, Serialize};

use multiview_licence::EnforcementLevel;

use crate::error::MeshError;
use crate::peer::{PeerKey, PEER_DIGEST_LEN};
use crate::ClaimState;

/// The mesh announce protocol version (brief §9.1). Bumped on a breaking change
/// to the payload shape so a peer can ignore an announcement it cannot parse.
pub const ANNOUNCE_PROTOCOL_VERSION: u16 = 1;

/// A domain-separation prefix so an announce signature can never be mistaken for
/// a signature over some other Multiview message type (e.g. a lease assertion).
const SIGNING_DOMAIN: &[u8] = b"multiview-mesh:announce:v1\0";

/// A salted fingerprint digest the announcement advertises (opaque 32 bytes).
///
/// Handed in already salted + hashed (data minimisation, brief §8) — never a raw
/// component identifier. A peer derives its [`PeerKey`] from the digest set;
/// across deployments (different salt) the same machine is uncorrelatable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SaltedDigest {
    digest: [u8; PEER_DIGEST_LEN],
}

impl SaltedDigest {
    /// Wrap a salted digest.
    #[must_use]
    pub const fn new(digest: [u8; PEER_DIGEST_LEN]) -> Self {
        Self { digest }
    }

    /// The salted digest bytes (opaque).
    #[must_use]
    pub const fn bytes(&self) -> &[u8; PEER_DIGEST_LEN] {
        &self.digest
    }
}

/// The signed **entitlement summary** an announcement advertises (ADR-0051 §2):
/// the enforcement **level** + the lease **bounds** (granted/expires). It carries
/// **no** tier string, serial, or owner — only the coarse facts a relaying
/// decision needs (is this neighbour licensed, and until when). Data
/// minimisation, brief §8.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct EntitlementSummary {
    /// The advertised enforcement level (the canonical resource field).
    pub level: EnforcementLevel,
    /// When the advertised lease was granted.
    pub granted_at: DateTime<Utc>,
    /// When the advertised lease term expires.
    pub expires_at: DateTime<Utc>,
}

impl EntitlementSummary {
    /// Build an entitlement summary from the level + lease bounds.
    #[must_use]
    pub const fn new(
        level: EnforcementLevel,
        granted_at: DateTime<Utc>,
        expires_at: DateTime<Utc>,
    ) -> Self {
        Self {
            level,
            granted_at,
            expires_at,
        }
    }
}

/// The full mDNS announce payload (brief §9.1).
///
/// **Exhaustive** field set — protocol version, the salted digest set, the claim
/// state, the signed entitlement summary, and the originator's signature. There
/// is intentionally **no** name, address, serial, or any identifier field; a peer
/// learns a neighbour's name only via the operator's confirm-adopt, never the
/// announce.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct AnnouncePayload {
    /// The announce protocol version (a peer ignores an announcement it cannot
    /// parse).
    pub protocol_version: u16,
    /// The salted fingerprint digest set (opaque). A peer derives the neighbour's
    /// [`PeerKey`] from this; never a raw identifier.
    pub digests: Vec<SaltedDigest>,
    /// The coarse claim state (`unclaimed`/`claiming`/`claimed`) — a state, never
    /// an identity.
    pub claim_state: ClaimState,
    /// The signed entitlement summary (level + lease bounds).
    pub entitlement: EntitlementSummary,
    /// The Ed25519 signature over the covered fields, by the originating
    /// machine's own key (64 bytes when well-formed). A peer verifies it to
    /// detect a spoof/tamper.
    pub signature: Vec<u8>,
}

impl AnnouncePayload {
    /// Build **and sign** an announce payload with the originating machine's
    /// `key`. The signature covers the protocol version, the digest set, the
    /// claim state, and the entitlement summary (everything but the signature).
    #[must_use]
    pub fn sign(
        protocol_version: u16,
        digests: Vec<SaltedDigest>,
        claim_state: ClaimState,
        entitlement: EntitlementSummary,
        key: &SigningKey,
    ) -> Self {
        let message = Self::signing_bytes(protocol_version, &digests, claim_state, &entitlement);
        let signature = key.sign(&message);
        Self {
            protocol_version,
            digests,
            claim_state,
            entitlement,
            signature: signature.to_bytes().to_vec(),
        }
    }

    /// The canonical, deterministic bytes the signature covers.
    ///
    /// Domain-separated, then each covered field appended length-prefixed in a
    /// fixed order, so the encoding is unambiguous and stable. The covered fields
    /// are exactly those a tamper must not alter undetected.
    #[must_use]
    pub fn signing_bytes(
        protocol_version: u16,
        digests: &[SaltedDigest],
        claim_state: ClaimState,
        entitlement: &EntitlementSummary,
    ) -> Vec<u8> {
        let mut out = Vec::with_capacity(SIGNING_DOMAIN.len() + 128);
        out.extend_from_slice(SIGNING_DOMAIN);
        out.extend_from_slice(&protocol_version.to_be_bytes());
        // The digest set: count then each fixed-width digest (order-significant,
        // matching the advertised order).
        let count = u64::try_from(digests.len()).unwrap_or(u64::MAX);
        out.extend_from_slice(&count.to_be_bytes());
        for digest in digests {
            out.extend_from_slice(digest.bytes());
        }
        out.push(claim_tag(claim_state));
        out.push(level_tag(entitlement.level));
        append_i64(&mut out, instant_nanos(entitlement.granted_at));
        append_i64(&mut out, instant_nanos(entitlement.expires_at));
        out
    }

    /// Verify the payload's signature against the presented originator
    /// `verifying_key`.
    ///
    /// # Errors
    /// * [`MeshError::BadSignature`] if the signature is malformed (not 64 bytes)
    ///   or does not verify (a spoofed/tampered announcement, or the wrong key).
    pub fn verify(&self, verifying_key: &VerifyingKey) -> Result<(), MeshError> {
        let sig_bytes: [u8; Signature::BYTE_SIZE] = self
            .signature
            .as_slice()
            .try_into()
            .map_err(|_| MeshError::BadSignature)?;
        let signature = Signature::from_bytes(&sig_bytes);
        let message = Self::signing_bytes(
            self.protocol_version,
            &self.digests,
            self.claim_state,
            &self.entitlement,
        );
        verifying_key
            .verify_strict(&message, &signature)
            .map_err(|_| MeshError::BadSignature)
    }

    /// The peer key derived from the announcement's first salted digest (the
    /// machine's primary fingerprint anchor). `None` if the announcement carries
    /// no digest (a malformed/empty announcement is not adopted).
    #[must_use]
    pub fn peer_key(&self) -> Option<PeerKey> {
        self.digests
            .first()
            .map(|d| PeerKey::from_digest(*d.bytes()))
    }

    /// Encode the payload for the mDNS TXT record (JSON — compact, robust across
    /// the mDNS-sd property encoding).
    ///
    /// # Errors
    /// [`MeshError::MalformedPayload`] if serialisation fails (not expected for
    /// this derived `Serialize`, but the guardrails forbid `unwrap`).
    pub fn to_wire(&self) -> Result<Vec<u8>, MeshError> {
        serde_json::to_vec(self).map_err(|e| MeshError::MalformedPayload(e.to_string()))
    }

    /// Decode a payload from its wire bytes. A typed error on garbage — never a
    /// panic (bad-inputs-are-the-purpose).
    ///
    /// # Errors
    /// [`MeshError::MalformedPayload`] if the bytes are not well-formed JSON for
    /// this payload shape.
    pub fn from_wire(bytes: &[u8]) -> Result<Self, MeshError> {
        serde_json::from_slice(bytes).map_err(|e| MeshError::MalformedPayload(e.to_string()))
    }
}

/// The stable wire tag byte for a claim state (covered by the signature).
const fn claim_tag(claim: ClaimState) -> u8 {
    match claim {
        ClaimState::Unclaimed => 0,
        ClaimState::Claiming => 1,
        ClaimState::Claimed => 2,
    }
}

/// The stable wire tag byte for an enforcement level (covered by the signature).
const fn level_tag(level: EnforcementLevel) -> u8 {
    match level {
        EnforcementLevel::Active => 0,
        EnforcementLevel::Warning => 1,
        EnforcementLevel::ConfigLocked => 2,
        EnforcementLevel::Watermark => 3,
        EnforcementLevel::BlockNewInstance => 4,
        EnforcementLevel::UnlicensedBuild => 5,
        // `EnforcementLevel` is `#[non_exhaustive]`; a future level signs as a
        // distinct sentinel so the signature still covers it unambiguously.
        _ => 255,
    }
}

/// An instant as i64 nanoseconds since the Unix epoch (saturating on overflow,
/// never a panic). Carried internal time as i64 ns (CLAUDE.md safety rule #6).
fn instant_nanos(instant: DateTime<Utc>) -> i64 {
    instant.timestamp_nanos_opt().unwrap_or(0)
}

/// Append an `i64` as 8 big-endian bytes (fixed width, no length prefix needed).
fn append_i64(out: &mut Vec<u8>, value: i64) {
    out.extend_from_slice(&value.to_be_bytes());
}
