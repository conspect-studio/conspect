//! Ed25519 lease/binding **verification** against a pinned public key
//! (ADR-0050 §2.5 / §3, brief §2.5).
//!
//! This module verifies that a lease assertion was signed by the licence
//! server's pinned key. It is **verification-only**: there is no key generation
//! and no RNG in non-test code — this crate checks signatures handed to it, it
//! never mints keys (data minimisation; the entropy source stays out of the
//! pure-data crate). The pinned key is a **parameter** ([`PinnedKey`]); key
//! pinning/rotation policy (O2) is an operator-confirm item and lives in the
//! caller, not here.
//!
//! # Wire format
//!
//! The signed payload is a deterministic, domain-separated, length-prefixed
//! byte encoding of the lease's covered fields ([`SignedLease::signing_bytes`]).
//! It is stable for a given lease so a portal and the machine agree on exactly
//! what was signed; any change to a covered field changes the bytes and so
//! invalidates the signature (tamper-evidence). This avoids a CBOR dependency
//! while remaining canonical.

use ed25519_dalek::{Signature, VerifyingKey};
use serde::{Deserialize, Serialize};

use crate::error::LicenceError;
use crate::lease::Lease;

/// The number of bytes in an Ed25519 verifying (public) key.
const PUBLIC_KEY_LEN: usize = 32;

/// A domain-separation prefix so a lease signature can never be mistaken for a
/// signature over some other Multiview message type.
const SIGNING_DOMAIN: &[u8] = b"multiview-licence:lease:v1\0";

/// A pinned Ed25519 verifying (public) key. Constructed from the bytes the
/// caller pins in the binary (ADR-0050 §2.5); this crate never generates one.
#[derive(Debug, Clone)]
pub struct PinnedKey {
    key: VerifyingKey,
}

impl PinnedKey {
    /// Pin an already-parsed verifying key.
    #[must_use]
    pub fn from_verifying_key(key: &VerifyingKey) -> Self {
        Self { key: *key }
    }

    /// Pin a key from its 32 raw bytes.
    ///
    /// # Errors
    /// [`LicenceError::MalformedKey`] if the bytes are not a valid Ed25519
    /// public-key point encoding.
    pub fn from_bytes(bytes: [u8; PUBLIC_KEY_LEN]) -> Result<Self, LicenceError> {
        let key = VerifyingKey::from_bytes(&bytes).map_err(|_| LicenceError::MalformedKey)?;
        Ok(Self { key })
    }

    /// Pin a key from a byte slice of unchecked length.
    ///
    /// # Errors
    /// [`LicenceError::MalformedKey`] if the slice is not exactly 32 bytes or is
    /// not a valid Ed25519 public-key point encoding.
    pub fn from_slice(bytes: &[u8]) -> Result<Self, LicenceError> {
        let arr: [u8; PUBLIC_KEY_LEN] = bytes.try_into().map_err(|_| LicenceError::MalformedKey)?;
        Self::from_bytes(arr)
    }
}

/// A lease plus the Ed25519 signature asserted over its canonical signing bytes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct SignedLease {
    /// The dated lease the signature covers.
    pub lease: Lease,
    /// The raw Ed25519 signature bytes (64 bytes when well-formed).
    pub signature: Vec<u8>,
}

impl SignedLease {
    /// Bundle a lease with its signature bytes.
    #[must_use]
    pub fn new(lease: Lease, signature: [u8; Signature::BYTE_SIZE]) -> Self {
        Self {
            lease,
            signature: signature.to_vec(),
        }
    }

    /// The canonical, deterministic bytes a signature covers for `lease` bound to
    /// `instance_binding_id`.
    ///
    /// Domain-separated then each covered field is appended length-prefixed in a
    /// fixed order, so the encoding is unambiguous and stable. The covered fields
    /// are exactly those a tamper must not be able to alter undetected: the
    /// serial, the source, the dated bounds, **and** the server-issued
    /// `instance_binding_id` the binding anchors the device identity to. The
    /// binding id is carried with a 1-byte presence tag (`0` = absent, `1` =
    /// present) before its length-prefixed bytes, so a `None` binding id signs to
    /// a value distinct from an empty-string id and a tampered (or grafted)
    /// binding id changes the bytes and invalidates the signature. Anchoring the
    /// device identity therefore always rests on signed material on every install
    /// path (offline file-drop, control-route upload, mesh relay, heartbeat).
    #[must_use]
    pub fn signing_bytes(lease: &Lease, instance_binding_id: Option<&str>) -> Vec<u8> {
        let mut out = Vec::with_capacity(SIGNING_DOMAIN.len() + 128);
        out.extend_from_slice(SIGNING_DOMAIN);
        append_field(&mut out, lease.serial.as_bytes());
        append_field(&mut out, source_tag(lease).as_bytes());
        append_i64(
            &mut out,
            lease.granted_at.timestamp_nanos_opt().unwrap_or(0),
        );
        append_i64(
            &mut out,
            lease.expires_at.timestamp_nanos_opt().unwrap_or(0),
        );
        append_i64(&mut out, lease.grace_days);
        append_i64(
            &mut out,
            lease.grace_until.timestamp_nanos_opt().unwrap_or(0),
        );
        append_i64(&mut out, lease.hard_at.timestamp_nanos_opt().unwrap_or(0));
        append_i64(
            &mut out,
            lease.next_contact_due.timestamp_nanos_opt().unwrap_or(0),
        );
        // The instance binding id: a presence tag then the length-prefixed bytes,
        // so `None` and `Some("")` are distinct signed values and any graft of the
        // anchor field changes the signed bytes (tamper-evident).
        match instance_binding_id {
            None => out.push(0),
            Some(id) => {
                out.push(1);
                append_field(&mut out, id.as_bytes());
            }
        }
        out
    }
}

/// Verify a signed lease against the pinned key, bound to `instance_binding_id`.
///
/// The signature is checked over [`SignedLease::signing_bytes`] computed with the
/// SAME `instance_binding_id` the caller will anchor — so a binding whose
/// `instance_binding_id` was grafted/tampered after signing (or is absent when
/// the signature covered one, or vice-versa) fails to verify. The device identity
/// anchor therefore always rests on signed material.
///
/// Returns a reference to the verified lease on success.
///
/// # Errors
/// - [`LicenceError::MalformedSignature`] if the signature is not 64 bytes.
/// - [`LicenceError::BadSignature`] if the signature does not verify against the
///   pinned key (tampered payload/binding id, wrong signer, or forgery).
pub fn verify_signed_lease<'a>(
    signed: &'a SignedLease,
    pinned: &PinnedKey,
    instance_binding_id: Option<&str>,
) -> Result<&'a Lease, LicenceError> {
    let sig_bytes: [u8; Signature::BYTE_SIZE] = signed
        .signature
        .as_slice()
        .try_into()
        .map_err(|_| LicenceError::MalformedSignature)?;
    let signature = Signature::from_bytes(&sig_bytes);
    let message = SignedLease::signing_bytes(&signed.lease, instance_binding_id);
    pinned
        .key
        .verify_strict(&message, &signature)
        .map_err(|_| LicenceError::BadSignature)?;
    Ok(&signed.lease)
}

/// The stable lowercase tag for a lease source (matches the serde discriminant).
fn source_tag(lease: &Lease) -> &'static str {
    use crate::lease::LeaseSource;
    match lease.source {
        LeaseSource::Online => "online",
        LeaseSource::Relay => "relay",
        LeaseSource::File => "file",
    }
}

/// Append a length-prefixed byte field (8-byte big-endian length + bytes).
fn append_field(out: &mut Vec<u8>, bytes: &[u8]) {
    let len = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(bytes);
}

/// Append an `i64` as 8 big-endian bytes (fixed width, no length prefix needed).
fn append_i64(out: &mut Vec<u8>, value: i64) {
    out.extend_from_slice(&value.to_be_bytes());
}
