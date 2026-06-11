//! The `<host>.challenge` export (CONSPECT-1, brief §3 + §8).
//!
//! The challenge file is the machine's side of the (offline-capable) claim /
//! heartbeat exchange: a compact CBOR document the operator hands to the portal.
//! It carries **only salted digests + monotonic counters** — **never** a raw
//! serial, MAC address, hostname, URL, or any direct hardware identifier (brief
//! §8 data minimisation). The type itself has no field that could hold a raw
//! identifier, so the minimisation is enforced structurally, not by convention.
//!
//! CBOR (RFC 8949) is the wire format because it is compact, canonical, and
//! self-describing, so the portal and the machine agree byte-for-byte on the
//! format (the brief requires the §2 constants and formats stay exact). Encoding
//! is deterministic for a given value.

use serde::{Deserialize, Serialize};

use crate::error::LicenceError;

/// Monotonic counters describing the machine's lifecycle, for the portal to
/// reason about activity. These are **numbers only** — they reveal nothing about
/// what the machine ingests or serves (brief §8). All `u64` so they never wrap
/// in practice and serialise unambiguously.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct ChallengeCounters {
    /// How many times the machine has booted the entitlement plane.
    pub boot_count: u64,
    /// How many heartbeat attempts have been made (successful or not).
    pub heartbeat_attempts: u64,
    /// How many leases have been installed (any of the three install paths).
    pub lease_installs: u64,
}

impl ChallengeCounters {
    /// Assemble counters. A constructor is provided because the struct is
    /// `#[non_exhaustive]` (future counters add without breaking callers).
    #[must_use]
    pub const fn new(boot_count: u64, heartbeat_attempts: u64, lease_installs: u64) -> Self {
        Self {
            boot_count,
            heartbeat_attempts,
            lease_installs,
        }
    }
}

/// The `<host>.challenge` document: salted digests + counters only.
///
/// `#[non_exhaustive]`: the format may grow salted fields without breaking
/// existing decoders. There is intentionally **no** field for a raw identifier.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct ChallengeFile {
    /// A salted, hashed digest of the host identity (hex) — **not** the raw
    /// hostname (brief §8). Two deployments with different salts cannot correlate
    /// this digest.
    pub host_digest: String,
    /// The salted hardware-component digest set (hex) the fingerprint score is
    /// computed over (brief §2.3, §8) — never raw serials/MACs.
    pub fingerprint_digests: Vec<String>,
    /// The monotonic lifecycle counters.
    pub counters: ChallengeCounters,
}

impl ChallengeFile {
    /// Assemble a challenge from its salted digests + counters.
    ///
    /// The caller (the cli, later) supplies digests it has already salted +
    /// hashed; this type never gathers raw identifiers (brief §8). A constructor
    /// is provided because the struct is `#[non_exhaustive]`.
    #[must_use]
    pub fn new(
        host_digest: String,
        fingerprint_digests: Vec<String>,
        counters: ChallengeCounters,
    ) -> Self {
        Self {
            host_digest,
            fingerprint_digests,
            counters,
        }
    }

    /// Encode the challenge as CBOR.
    ///
    /// # Errors
    /// [`LicenceError::Cbor`] if serialisation fails (not expected for this
    /// plain derived `Serialize`, but the guardrails forbid `unwrap`/`expect`).
    pub fn to_cbor(&self) -> Result<Vec<u8>, LicenceError> {
        let mut out = Vec::with_capacity(64);
        ciborium::into_writer(self, &mut out).map_err(|e| LicenceError::Cbor(e.to_string()))?;
        Ok(out)
    }

    /// Decode a challenge from CBOR bytes.
    ///
    /// # Errors
    /// [`LicenceError::Cbor`] if the bytes are not well-formed CBOR for this
    /// document shape (bad-inputs-are-the-purpose: a typed error, never a panic).
    pub fn from_cbor(bytes: &[u8]) -> Result<Self, LicenceError> {
        ciborium::from_reader(bytes).map_err(|e| LicenceError::Cbor(e.to_string()))
    }
}
