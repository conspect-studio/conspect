//! The Conspect **device-licensing heartbeat client** (CONSPECT-3, ADR-0096) —
//! feature `heartbeat`, OFF by default.
//!
//! This module is the device→server licensing client: it fetches the published
//! key-trust material, verifies the **pinned-root → attested-intermediate** chain
//! (D1), verifies a server lease's **bare-Ed25519** signature over the
//! deterministic-CBOR lease body (D3), and drives the existing
//! [`LeaseStore::install_binding`](crate::store::LeaseStore::install_binding)
//! convergence so the rest of the machine (S1/S2/S3, the control routes, the web
//! screens) reads the renewed lease with no extra wiring.
//!
//! # Renew-only (online-activate deferred)
//!
//! The client is **renew-only**: its job is to RENEW a lease the device already
//! holds. Device-side online-activate is **deferred** (ADR-I006 decision point 11)
//! — the Conspect server does not yet issue the `serverNonce` (the per-instance
//! lease-chain freshness anchor; part of the device-credential mechanism the spec
//! marks "deferred to ADR-0036"), so the device cannot mint a valid activation
//! request today. Onboarding therefore does **not** go through this client: the
//! operator activates in the Conspect portal, and the signed lease reaches the
//! device via the three existing install surfaces — control-upload, the offline
//! file-drop watcher, and the mesh relay — all of which feed `install_binding`.
//! With no lease/binding yet, [`HeartbeatClient::run_once`] makes **no** server
//! call ([`HeartbeatOutcome::NoBinding`]) and waits for one of those surfaces.
//!
//! # Never off air (invariants #1 / #10)
//!
//! The client only ever **tightens** on a positively-verified signed lease.
//! Every failure mode — an unreachable server, a failed renew, a malformed
//! response, a suspect clock, **and a deliberately withheld lease** (revocation
//! by non-reissue: a `200` with `lease: null`) — leaves the last-good lease in
//! place to age via the existing ladder. There is no push kill verb and no code
//! path here that removes or downgrades a lease on its own. The
//! [`HeartbeatClient`] holds only an [`Arc<LeaseStore>`](crate::store::LeaseStore),
//! the pinned root, the server handle, and config — **never** an engine handle,
//! channel, or lock the data plane takes — so it is physically unable to
//! back-pressure or stall the engine. Its sole side effect is `install_binding`.
//!
//! # Trust posture (fail closed on trust, lenient on enforcement)
//!
//! Trust material is verified strictly: an un-attested intermediate, a forged
//! revocation list, or a bad lease signature is **rejected** (never trusted). But
//! a *rejection* never degrades the machine — it simply withholds the next lease,
//! and the previous lease ages normally. The two postures compose: we are
//! paranoid about *what we trust*, lenient about *what we enforce*.
//!
//! # Auth (today)
//!
//! Requests carry the account-JWT `Authorization: Bearer` chain (the
//! device proof-of-possession request-signing wire format is deferred
//! server-side, ADR-0096 D2). The
//! [`DeviceIdentity::device_public_key_b64url`] is captured and stored on the
//! binding but does **not** authenticate requests yet. Every mutation carries a
//! required `Idempotency-Key` (a retry replays, never re-issues).
//!
//! The live HTTP transport lives at the cli/app boundary (it owns `reqwest`); the
//! testable crypto + the loop live here behind the [`LicenceServer`] seam, so this
//! leaf crate never opens a socket of its own.

use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use ed25519_dalek::{Signature as EdSignature, VerifyingKey as EdVerifyingKey};
use p256::ecdsa::{signature::Verifier as _, Signature as P256Signature, VerifyingKey as P256Key};
use serde::{Deserialize, Serialize};

use crate::constants::ACTIVATION_WINDOW_DAYS;
use crate::entitlement::{Entitlement, EntitlementFlags, GpuLimit, HardwareClass, Tier};
use crate::lease::Lease;
use crate::store::{system_now, LeaseBinding, LeaseStore};
use crate::verify::{PinnedKey, SignedLease};

// ===========================================================================
// Canonical CBOR (RFC 8949 §4.2.1) — the key pre-image + revocation pre-image.
// ===========================================================================
//
// A tiny, total, hand-rolled canonical encoder for the fixed-shape attestation
// pre-images. We do NOT rely on a serde-CBOR codec's map-key ordering: `ciborium`
// preserves insertion order rather than sorting keys, so a canonical guarantee
// must be ours. The pre-image maps are fixed: their keys, written in the
// well-known `key_pre_image` order, are already in RFC 8949 §4.2.1 canonical
// order (shortest-encoded-key first, then bytewise) — verified byte-exact against
// the live `root_sig` that signs exactly these bytes (see the golden-vector test).

/// Append a CBOR head (major type + argument) using the shortest encoding
/// (preferred serialization, RFC 8949 §4.2.1). `n` is the unsigned argument.
fn cbor_head(out: &mut Vec<u8>, major: u8, n: u64) {
    let mt = major << 5;
    if n < 24 {
        out.push(mt | u8::try_from(n).unwrap_or(0));
    } else if let Ok(b) = u8::try_from(n) {
        out.push(mt | 0x18);
        out.push(b);
    } else if let Ok(b) = u16::try_from(n) {
        out.push(mt | 0x19);
        out.extend_from_slice(&b.to_be_bytes());
    } else if let Ok(b) = u32::try_from(n) {
        out.push(mt | 0x1a);
        out.extend_from_slice(&b.to_be_bytes());
    } else {
        out.push(mt | 0x1b);
        out.extend_from_slice(&n.to_be_bytes());
    }
}

/// The CBOR head argument for a `len` (a collection length / byte count). A
/// length beyond `u64` cannot occur for these small fixed pre-images; it clamps
/// to `u64::MAX` rather than `as`-truncating (total + panic-free).
fn len_arg(len: usize) -> u64 {
    u64::try_from(len).unwrap_or(u64::MAX)
}

/// Append a CBOR text string (major 3).
fn cbor_tstr(out: &mut Vec<u8>, s: &str) {
    cbor_head(out, 3, len_arg(s.len()));
    out.extend_from_slice(s.as_bytes());
}

/// Append a CBOR byte string (major 2).
fn cbor_bstr(out: &mut Vec<u8>, b: &[u8]) {
    cbor_head(out, 2, len_arg(b.len()));
    out.extend_from_slice(b);
}

/// Append a CBOR unsigned integer (major 0). The pre-image times are epoch-ms
/// (always non-negative); a negative input clamps to 0 (it cannot occur for a
/// valid timestamp and is never silently mis-encoded as a different number).
fn cbor_uint(out: &mut Vec<u8>, n: i64) {
    cbor_head(out, 0, u64::try_from(n).unwrap_or(0));
}

/// The deterministic-CBOR **key pre-image** the well-known `root_sig` covers
/// (ADR-0096 D3): a `map(6)` over `[key_id, key_type, statement,
/// public_key(bstr), valid_from(uint), valid_until(uint)]` in that (canonical)
/// order. `public_key` is the **raw 32-byte Ed25519 point** (a CBOR byte string),
/// the times are epoch-milliseconds.
///
/// Exposed (crate-public via re-export) so tests can build the exact pre-image a
/// fabricated root signs.
#[must_use]
pub fn canonical_key_preimage(
    key_id: &str,
    key_type: &str,
    statement: &str,
    public_key: &[u8],
    valid_from: i64,
    valid_until: i64,
) -> Vec<u8> {
    let mut out = Vec::with_capacity(160);
    cbor_head(&mut out, 5, 6); // map(6)
    cbor_tstr(&mut out, "key_id");
    cbor_tstr(&mut out, key_id);
    cbor_tstr(&mut out, "key_type");
    cbor_tstr(&mut out, key_type);
    cbor_tstr(&mut out, "statement");
    cbor_tstr(&mut out, statement);
    cbor_tstr(&mut out, "public_key");
    cbor_bstr(&mut out, public_key);
    cbor_tstr(&mut out, "valid_from");
    cbor_uint(&mut out, valid_from);
    cbor_tstr(&mut out, "valid_until");
    cbor_uint(&mut out, valid_until);
    out
}

/// The deterministic-CBOR **revocation pre-image** the `root_revocation_sig`
/// covers (ADR-0096 D3): a `map(3)` over `[issued_at(uint), statement,
/// revoked_key_ids(array of tstr)]` in that (canonical) order.
#[must_use]
pub fn canonical_revocation_preimage(
    issued_at: i64,
    statement: &str,
    revoked_key_ids: &[String],
) -> Vec<u8> {
    let mut out = Vec::with_capacity(64);
    cbor_head(&mut out, 5, 3); // map(3)
    cbor_tstr(&mut out, "issued_at");
    cbor_uint(&mut out, issued_at);
    cbor_tstr(&mut out, "statement");
    cbor_tstr(&mut out, statement);
    cbor_tstr(&mut out, "revoked_key_ids");
    cbor_head(&mut out, 4, len_arg(revoked_key_ids.len())); // array(n)
    for id in revoked_key_ids {
        cbor_tstr(&mut out, id);
    }
    out
}

// ===========================================================================
// Device proof-of-possession (PoP) — the canonical pre-image + COSE_Sign1 proof
// (CONSPECT-3 D2, ADR-I007; Conspect API v0.9.0 enforces device-PoP).
// ===========================================================================
//
// The `Conspect-Device-PoP` header on each device-mutating op is a base64
// COSE_Sign1 the device signs over the canonical PoP pre-image
// `htm | htu | sha256(body) | instance_id | nonce | iat` with its Ed25519 device
// key. The server recomputes the pre-image from the actual request, verifies the
// COSE_Sign1 against the bound device key (continuity), checks the iat ±60s
// leeway, and burns the single-use nonce. This module owns the PURE crypto: the
// byte-exact pre-image (a deterministic-CBOR map, hand-rolled like the key
// pre-image) and the COSE_Sign1 envelope. Key GENERATION + durable PERSISTENCE is
// the cli's (it does the I/O + the only RNG); the device key reaches this module
// through the [`DeviceSigner`] seam (Ed25519 signing is deterministic — RFC 8032 —
// so this stays no-RNG in non-test code).

/// A device proof-of-possession failure. None of these tighten the machine — the
/// caller skips this heartbeat cycle and keeps last-good (never off air, inv
/// #1/#10).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum PopError {
    /// The challenge nonce was not 32 bytes of lower-case hex (`^[0-9a-f]{64}$`) —
    /// a malformed/absent nonce, never a silently truncated pre-image.
    #[error("device-PoP nonce is not 64 lower-case hex: {0}")]
    Nonce(String),
    /// The `COSE_Sign1` proof could not be assembled/serialised (not expected for a
    /// well-formed pre-image, but the guardrails forbid `unwrap`/`expect`).
    #[error("device-PoP COSE_Sign1 could not be built: {0}")]
    Cose(String),
}

/// The device key seam: the bound Ed25519 device keypair the PoP proof is signed
/// with. The cli implements this over a generated + durably-persisted keypair (the
/// I/O + RNG live there); the leaf crate only ever **signs** (Ed25519 signing is
/// deterministic — RFC 8032 — so no RNG enters non-test code) and reads the public
/// point. A test backs it with a fixed seed so it can sign a proof AND verify it.
pub trait DeviceSigner: Send + Sync {
    /// The raw 32-byte Ed25519 public point — the device key the server has bound
    /// (its base64url is `devicePublicKey`; its RFC 7638 thumbprint is the lease
    /// `cnf_jkt`).
    fn public_key_raw(&self) -> [u8; 32];
    /// A deterministic Ed25519 signature (64 bytes) over `message` (RFC 8032 — no
    /// RNG). `message` is the COSE `Sig_structure` (`Signature1`) the library hands
    /// us; we never sign anything else.
    fn sign(&self, message: &[u8]) -> [u8; 64];
}

/// Decode a `^[0-9a-f]{64}$` nonce to its 32 raw bytes, rejecting anything else
/// (fail closed — never a truncated/zero-padded pre-image). Lower-case only, to
/// match the server's canonical form exactly.
fn nonce_hex_to_raw(nonce_hex: &str) -> Result<[u8; 32], PopError> {
    if nonce_hex.len() != 64 || !nonce_hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(PopError::Nonce(format!(
            "expected 64 hex chars, got {} chars",
            nonce_hex.len()
        )));
    }
    // Lower-case only (the wire is lower-case hex); an upper-case digit is rejected
    // so the device and server agree byte-for-byte.
    if nonce_hex.bytes().any(|b| b.is_ascii_uppercase()) {
        return Err(PopError::Nonce("nonce must be lower-case hex".to_owned()));
    }
    let bytes = hex::decode(nonce_hex).map_err(|e| PopError::Nonce(e.to_string()))?;
    let mut out = [0u8; 32];
    if bytes.len() != 32 {
        return Err(PopError::Nonce(format!(
            "decoded to {} bytes, expected 32",
            bytes.len()
        )));
    }
    out.copy_from_slice(&bytes);
    Ok(out)
}

/// The SHA-256 of the request body — the `sha256(body)` term of the pre-image.
fn sha256_body(body: &[u8]) -> [u8; 32] {
    use sha2::{Digest as _, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(body);
    hasher.finalize().into()
}

/// The deterministic-CBOR **device-PoP pre-image** the server recomputes and the
/// `COSE_Sign1` signs over (ADR-I007): a `map(6)` over
/// `htm | htu | sha256(body) | instance_id | nonce | iat` in that order. `htm` is
/// the upper-case HTTP method, `htu` the full request URI (no query); `sha256_body`
/// and `nonce` are CBOR **byte strings** (raw 32 bytes each — the nonce decoded
/// from its 64-hex form), `iat` an unsigned int (epoch **seconds** — the server
/// checks ±60s).
///
/// # Errors
/// [`PopError::Nonce`] if `nonce_hex` is not 64 lower-case hex.
pub fn canonical_pop_preimage(
    htm: &str,
    htu: &str,
    body: &[u8],
    instance_id: &str,
    nonce_hex: &str,
    iat: i64,
) -> Result<Vec<u8>, PopError> {
    let nonce_raw = nonce_hex_to_raw(nonce_hex)?;
    let body_hash = sha256_body(body);
    let mut out = Vec::with_capacity(160);
    cbor_head(&mut out, 5, 6); // map(6)
    cbor_tstr(&mut out, "htm");
    cbor_tstr(&mut out, htm);
    cbor_tstr(&mut out, "htu");
    cbor_tstr(&mut out, htu);
    cbor_tstr(&mut out, "sha256_body");
    cbor_bstr(&mut out, &body_hash);
    cbor_tstr(&mut out, "instance_id");
    cbor_tstr(&mut out, instance_id);
    cbor_tstr(&mut out, "nonce");
    cbor_bstr(&mut out, &nonce_raw);
    cbor_tstr(&mut out, "iat");
    cbor_uint(&mut out, iat);
    Ok(out)
}

/// Build the `Conspect-Device-PoP` header value: a **standard-base64** (RFC 4648
/// §4) `COSE_Sign1` the `signer` signs over the [`canonical_pop_preimage`]. The
/// protected header pins `alg = EdDSA`; the payload is the pre-image (attached), so
/// the server recomputes the same pre-image and verifies the signature against the
/// bound device key. The result is the untagged 4-element `COSE_Sign1` array.
///
/// # Errors
/// [`PopError::Nonce`] if `nonce_hex` is malformed; [`PopError::Cose`] if the
/// `COSE_Sign1` fails to serialise (not expected for a well-formed pre-image).
pub fn pop_header_value(
    signer: &dyn DeviceSigner,
    htm: &str,
    htu: &str,
    body: &[u8],
    instance_id: &str,
    nonce_hex: &str,
    iat: i64,
) -> Result<String, PopError> {
    use coset::{iana, CborSerializable as _, CoseSign1Builder, HeaderBuilder};

    let preimage = canonical_pop_preimage(htm, htu, body, instance_id, nonce_hex, iat)?;
    let protected = HeaderBuilder::new()
        .algorithm(iana::Algorithm::EdDSA)
        .build();
    // `create_signature` hands the closure the COSE Sig_structure ("Signature1")
    // bytes; the device key signs exactly those. The empty AAD matches the server's
    // recompute (no external_aad).
    let sign1 = CoseSign1Builder::new()
        .protected(protected)
        .payload(preimage)
        .create_signature(b"", |tbs| signer.sign(tbs).to_vec())
        .build();
    let bytes = sign1.to_vec().map_err(|e| PopError::Cose(e.to_string()))?;
    Ok(base64::engine::general_purpose::STANDARD.encode(bytes))
}

// ===========================================================================
// The published well-known key-trust document.
// ===========================================================================

/// The well-known licensing-keys document
/// (`GET /.well-known/conspect-licensing-keys.json`). Deserialised as received;
/// only the fields the verifier needs are modelled (`#[serde(default)]` on the
/// rest keeps it tolerant of additive server changes — but the cryptographic
/// fields are required).
#[derive(Debug, Clone, Deserialize)]
#[non_exhaustive]
pub struct LicensingKeys {
    /// The pinned trust anchor descriptor (its key bytes are compared to the
    /// build-pinned [`PinnedRoot`]; a mismatch is rejected).
    pub root: RootDescriptor,
    /// The attestation contract (statements + encoding); the verifier reads the
    /// statement strings from here.
    pub attestation_contract: AttestationContract,
    /// The dual-pin lease-signing intermediates.
    pub lease_keys: Vec<IntermediateKey>,
    /// The revocation list + its root signature.
    pub revocation: Revocation,
}

/// The root descriptor in the well-known document.
#[derive(Debug, Clone, Deserialize)]
#[non_exhaustive]
pub struct RootDescriptor {
    /// The root key id (informational).
    #[serde(default)]
    pub kid: String,
    /// The base64url-encoded uncompressed P-256 point.
    pub public_key: String,
}

/// The attestation contract block.
#[derive(Debug, Clone, Deserialize)]
#[non_exhaustive]
pub struct AttestationContract {
    /// The statement string carried in each key pre-image.
    pub key_statement: String,
    /// The statement string carried in the revocation pre-image.
    pub revocation_statement: String,
}

/// One dual-pin intermediate key in the well-known document.
#[derive(Debug, Clone, Deserialize)]
#[non_exhaustive]
pub struct IntermediateKey {
    /// The key id leases reference via `signerKeyId`.
    pub kid: String,
    /// The key kind (`"lease"`).
    pub key_type: String,
    /// The base64url-encoded raw 32-byte Ed25519 public key.
    pub public_key: String,
    /// The validity start (epoch milliseconds).
    pub valid_from: i64,
    /// The validity end (epoch milliseconds).
    pub valid_until: i64,
    /// `"current"` or `"next"` (both accepted within validity — dual-pin).
    #[serde(default)]
    pub status: String,
    /// The base64url-encoded ECDSA-P256 `root_sig` (raw r||s) over the key
    /// pre-image.
    pub root_sig: String,
}

/// The revocation block.
#[derive(Debug, Clone, Deserialize)]
#[non_exhaustive]
pub struct Revocation {
    /// The revocation statement (must match `attestation_contract`).
    #[serde(default)]
    pub statement: String,
    /// When the revocation list was issued (epoch milliseconds).
    pub issued_at: i64,
    /// The revoked key ids (their leases/keys are dropped from trust).
    #[serde(default)]
    pub revoked_key_ids: Vec<String>,
    /// The base64url-encoded ECDSA-P256 signature over the revocation pre-image.
    pub root_revocation_sig: String,
}

// ===========================================================================
// Key trust: pinned root → attested intermediates → dual-pin + revocation.
// ===========================================================================

/// The build-pinned root verifying key (ECDSA P-256). Obtained out-of-band and
/// pinned in the binary/config; this crate **verifies against** it, never mints
/// it.
#[derive(Debug, Clone)]
pub struct PinnedRoot {
    key: P256Key,
    /// The SEC1 uncompressed encoding, retained so we can reject a well-known
    /// document whose advertised root does not byte-match the pinned anchor.
    sec1: Vec<u8>,
}

impl PinnedRoot {
    /// Pin a root from a SEC1 point encoding (uncompressed `0x04||X||Y`, 65
    /// bytes; a compressed point is also accepted by the parser).
    ///
    /// # Errors
    /// [`KeyTrustError::MalformedRoot`] if the bytes are not a valid P-256 point.
    pub fn from_sec1_bytes(bytes: &[u8]) -> Result<Self, KeyTrustError> {
        let key = P256Key::from_sec1_bytes(bytes).map_err(|_| KeyTrustError::MalformedRoot)?;
        Ok(Self {
            key,
            sec1: bytes.to_vec(),
        })
    }

    /// Pin a root from the base64url-encoded uncompressed point the well-known
    /// document publishes (`base64url-uncompressed-p256-point`).
    ///
    /// # Errors
    /// [`KeyTrustError::MalformedRoot`] on a bad base64url string or point.
    pub fn from_base64url(s: &str) -> Result<Self, KeyTrustError> {
        let bytes = b64url(s).ok_or(KeyTrustError::MalformedRoot)?;
        Self::from_sec1_bytes(&bytes)
    }

    /// Verify an ECDSA-P256/SHA-256 signature (raw r||s) over `message`.
    fn verify(&self, message: &[u8], raw_sig: &[u8]) -> bool {
        match P256Signature::from_slice(raw_sig) {
            Ok(sig) => self.key.verify(message, &sig).is_ok(),
            Err(_) => false,
        }
    }
}

/// The verified, in-validity set of lease-signing keys: the only keys a server
/// lease may be signed under. Built by [`TrustedKeys::verify`]; resolving an
/// unknown or revoked or out-of-validity `signerKeyId` yields `None`.
#[derive(Debug, Clone)]
pub struct TrustedKeys {
    keys: Vec<(String, EdVerifyingKey)>,
}

impl TrustedKeys {
    /// Verify a well-known document against the pinned root and return the set of
    /// trusted lease-signing keys valid at `now_ms` (epoch milliseconds).
    ///
    /// Steps (all must pass, else the whole keyset is rejected):
    /// 1. the document's advertised root byte-matches the pinned anchor;
    /// 2. the revocation list is itself root-attested (fail closed: a forged
    ///    revocation signature is rejected, never silently honoured/ignored);
    /// 3. each intermediate's `root_sig` verifies over its canonical key
    ///    pre-image against the pinned root.
    ///
    /// Then the trusted set is those attested intermediates that are **within
    /// their validity window** at `now_ms` and **not** named in the (verified)
    /// revocation list. Both `current` and `next` status keys are accepted
    /// (dual-pin), so a key rotation never strands a fielded build.
    ///
    /// # Errors
    /// [`KeyTrustError`] when the pinned root mismatches, the revocation list is
    /// not root-attested, or any intermediate is not root-attested.
    pub fn verify(
        keys: &LicensingKeys,
        pinned: &PinnedRoot,
        now_ms: i64,
    ) -> Result<Self, KeyTrustError> {
        // 1. The advertised root must be the pinned anchor (defends against a
        //    substituted well-known document with a self-consistent foreign root).
        let advertised = b64url(&keys.root.public_key).ok_or(KeyTrustError::MalformedRoot)?;
        let advertised_root =
            PinnedRoot::from_sec1_bytes(&advertised).map_err(|_| KeyTrustError::MalformedRoot)?;
        if advertised_root.sec1 != pinned.sec1 {
            return Err(KeyTrustError::RootMismatch);
        }

        // 2. The revocation list must be root-attested before we honour it.
        let rev_pre = canonical_revocation_preimage(
            keys.revocation.issued_at,
            &keys.attestation_contract.revocation_statement,
            &keys.revocation.revoked_key_ids,
        );
        let rev_sig = b64url(&keys.revocation.root_revocation_sig)
            .ok_or(KeyTrustError::RevocationNotAttested)?;
        if !pinned.verify(&rev_pre, &rev_sig) {
            return Err(KeyTrustError::RevocationNotAttested);
        }
        let revoked = &keys.revocation.revoked_key_ids;

        // 3. Each intermediate's root_sig must verify over its canonical pre-image.
        let mut trusted = Vec::new();
        for ik in &keys.lease_keys {
            let pubkey =
                b64url(&ik.public_key).ok_or_else(|| KeyTrustError::IntermediateNotAttested {
                    kid: ik.kid.clone(),
                })?;
            // Reject a non-sensical (negative) validity window up front: a negative
            // epoch-ms cannot be a real Conspect timestamp, and the canonical-CBOR
            // uint encoder would otherwise coerce it (a forged/garbled key must not
            // be silently normalised into something the root could appear to sign).
            if ik.valid_from < 0 || ik.valid_until < 0 {
                return Err(KeyTrustError::IntermediateNotAttested {
                    kid: ik.kid.clone(),
                });
            }
            let pre = canonical_key_preimage(
                &ik.kid,
                &ik.key_type,
                &keys.attestation_contract.key_statement,
                &pubkey,
                ik.valid_from,
                ik.valid_until,
            );
            let sig =
                b64url(&ik.root_sig).ok_or_else(|| KeyTrustError::IntermediateNotAttested {
                    kid: ik.kid.clone(),
                })?;
            if !pinned.verify(&pre, &sig) {
                return Err(KeyTrustError::IntermediateNotAttested {
                    kid: ik.kid.clone(),
                });
            }
            // Attested — but attestation is PURPOSE-BOUND. Only a key whose
            // root-signed pre-image declared `key_type == "lease"` is a lease
            // signer; a root-attested key minted for any other purpose (e.g. an
            // update/signing key) must NOT be accepted to sign leases. `key_type`
            // IS in the signed pre-image, so this gate is cryptographically bound.
            // A SKIP, not a hard reject: an unrelated non-lease key in the document
            // does not poison the whole keyset, it is simply never a lease signer.
            if ik.key_type != "lease" {
                continue;
            }
            // The trust decision rests ONLY on signed fields. `ik.status` is NOT in
            // the root-signed pre-image (a MITM / compromised well-known doc can
            // flip a retired key's status to "current" without breaking root_sig),
            // so it is a non-binding operational hint and MUST NOT gate trust.
            // Retirement is expressed via the SIGNED revocation list and the SIGNED
            // validity window — both checked below.
            if revoked.iter().any(|r| r == &ik.kid) {
                continue;
            }
            if now_ms < ik.valid_from || now_ms > ik.valid_until {
                continue;
            }
            let arr: [u8; 32] = match pubkey.as_slice().try_into() {
                Ok(a) => a,
                Err(_) => {
                    return Err(KeyTrustError::IntermediateNotAttested {
                        kid: ik.kid.clone(),
                    })
                }
            };
            let vk = EdVerifyingKey::from_bytes(&arr).map_err(|_| {
                KeyTrustError::IntermediateNotAttested {
                    kid: ik.kid.clone(),
                }
            })?;
            trusted.push((ik.kid.clone(), vk));
        }
        Ok(Self { keys: trusted })
    }

    /// The trusted Ed25519 verifying key for `signer_key_id`, if it is attested,
    /// in-validity, and not revoked.
    #[must_use]
    pub fn lease_key(&self, signer_key_id: &str) -> Option<&EdVerifyingKey> {
        self.keys
            .iter()
            .find(|(kid, _)| kid == signer_key_id)
            .map(|(_, vk)| vk)
    }
}

/// Why the key-trust chain was rejected. Every variant means "do not trust this
/// material" — never "tighten enforcement"; a rejection withholds the next lease,
/// it never downgrades the machine.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum KeyTrustError {
    /// The pinned root bytes are not a valid P-256 point.
    #[error("the pinned root key is malformed")]
    MalformedRoot,
    /// The well-known document advertises a root that is not the pinned anchor.
    #[error("the advertised root key does not match the pinned anchor")]
    RootMismatch,
    /// The revocation list's signature does not verify against the pinned root
    /// (fail closed — we never honour an unsigned revocation list).
    #[error("the revocation list is not root-attested")]
    RevocationNotAttested,
    /// An intermediate's `root_sig` does not verify over its canonical pre-image.
    #[error("intermediate {kid} is not root-attested")]
    IntermediateNotAttested {
        /// The offending intermediate's key id.
        kid: String,
    },
}

// ===========================================================================
// The signed server lease (bare Ed25519 over deterministic-CBOR leaseBytes).
// ===========================================================================

/// A signed lease as returned by `heartbeat` (the authoritative renewal artifact).
/// The `leaseBytes` are the authoritative signed body; the scalar fields are a
/// convenience subset. `licenceId`/`instanceBindingId` are optional on the
/// envelope (the install path reads them from the signed body, not these mirrors).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerLease {
    /// The signer-minted lease serial (`UUIDv7`).
    pub serial: String,
    /// The licence this lease was issued against (envelope mirror; authoritative
    /// value is in the signed body).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub licence_id: Option<String>,
    /// The instance binding the lease is bound to (envelope mirror; authoritative
    /// value is in the signed body).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instance_binding_id: Option<String>,
    /// The lease expiry (epoch milliseconds) — the convenience subset of the
    /// signed `not_after`.
    pub not_after: i64,
    /// The bare Ed25519 signature, lower-case hex (64 bytes / 128 hex chars).
    pub signature: String,
    /// The dual-pin intermediate key id the lease was signed under.
    pub signer_key_id: String,
    /// The exact canonical-CBOR lease body the signature covers,
    /// **STANDARD-base64** (RFC 4648 §4, NOT base64url).
    pub lease_bytes: String,
}

/// The authoritative fields parsed out of the signed CBOR lease body — the
/// offline-enforcement inputs (`gpu_limit`, `hardware_class`, `not_after`, the
/// chain identity). Only the fields the install path needs are modelled.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct LeaseBody {
    /// The licence id (from the signed body).
    pub licence_id: String,
    /// The instance binding id (from the signed body).
    pub instance_binding_id: String,
    /// The lease serial (from the signed body).
    pub serial: String,
    /// The lease expiry (epoch milliseconds, from the signed body).
    pub not_after: i64,
    /// The GPU allowance carried in the signed body (`None` if absent).
    pub gpu_limit: Option<u32>,
    /// The licensed hardware class string carried in the signed body.
    pub hardware_class: Option<String>,
}

impl LeaseBody {
    /// Whether the signed lease is already expired at `now_ms` (epoch ms): its
    /// cryptographically-signed `not_after` is at or before `now_ms`. The
    /// installer rejects such a lease (a signed-but-expired or replayed-old lease
    /// must never become a fresh active term).
    #[must_use]
    pub fn is_expired_at(&self, now_ms: i64) -> bool {
        self.not_after <= now_ms
    }
}

/// The test/fabrication mirror of [`LeaseBody`] with a deterministic-CBOR encoder
/// (so a fake server can mint a body and sign exactly the bytes the verifier
/// re-checks). Field order is the canonical map order used on the wire.
#[derive(Debug, Clone)]
pub struct LeaseBodyFields {
    /// The licence id.
    pub licence_id: String,
    /// The instance binding id.
    pub instance_binding_id: String,
    /// The lease serial.
    pub serial: String,
    /// The lease expiry (epoch milliseconds).
    pub not_after: i64,
    /// The GPU allowance.
    pub gpu_limit: Option<u32>,
    /// The licensed hardware class.
    pub hardware_class: Option<String>,
}

impl LeaseBodyFields {
    /// Encode this body as deterministic CBOR (canonical map order). The optional
    /// fields are omitted when `None`, so the map count adapts; the present keys
    /// are written in `[gpu_limit, hardware_class, instance_binding_id,
    /// licence_id, not_after, serial]` — i.e. the keys sorted in RFC 8949 §4.2.1
    /// canonical order (all are length-distinct or bytewise-ordered text keys).
    #[must_use]
    pub fn to_canonical_cbor(&self) -> Vec<u8> {
        // Collect present (key, value) pairs, sort by canonical key order, emit.
        // This keeps the encoder honest about canonicality regardless of the
        // struct field order.
        let mut pairs: Vec<(&str, CborVal)> = Vec::with_capacity(6);
        if let Some(g) = self.gpu_limit {
            pairs.push(("gpu_limit", CborVal::Uint(i64::from(g))));
        }
        if let Some(hc) = self.hardware_class.clone() {
            pairs.push(("hardware_class", CborVal::Text(hc)));
        }
        pairs.push((
            "instance_binding_id",
            CborVal::Text(self.instance_binding_id.clone()),
        ));
        pairs.push(("licence_id", CborVal::Text(self.licence_id.clone())));
        pairs.push(("not_after", CborVal::Uint(self.not_after)));
        pairs.push(("serial", CborVal::Text(self.serial.clone())));
        pairs.sort_by(|a, b| canonical_key_cmp(a.0, b.0));

        let mut out = Vec::with_capacity(128);
        cbor_head(&mut out, 5, len_arg(pairs.len()));
        for (k, v) in &pairs {
            cbor_tstr(&mut out, k);
            match v {
                CborVal::Uint(n) => cbor_uint(&mut out, *n),
                CborVal::Text(s) => cbor_tstr(&mut out, s),
            }
        }
        out
    }
}

/// A CBOR scalar value for the canonical lease-body encoder.
enum CborVal {
    /// An unsigned integer (major 0).
    Uint(i64),
    /// A text string (major 3).
    Text(String),
}

/// Compare two text map keys by RFC 8949 §4.2.1 order: shorter encoded key first
/// (for text strings of equal short length that means by length then bytewise),
/// then bytewise on the encoded bytes.
fn canonical_key_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    a.len()
        .cmp(&b.len())
        .then_with(|| a.as_bytes().cmp(b.as_bytes()))
}

/// Verify a [`ServerLease`] end to end: resolve `signer_key_id` to a trusted
/// intermediate, then verify the **bare Ed25519** signature over the
/// **standard-base64-decoded** `lease_bytes`, then parse the CBOR body.
///
/// # Errors
/// [`SignedLeaseError`] for an unknown signer, a malformed signature/body, a
/// signature that does not verify, or a body missing required fields.
pub fn verify_signed_lease_chain(
    lease: &ServerLease,
    trusted: &TrustedKeys,
) -> Result<LeaseBody, SignedLeaseError> {
    let vk =
        trusted
            .lease_key(&lease.signer_key_id)
            .ok_or_else(|| SignedLeaseError::UnknownSigner {
                kid: lease.signer_key_id.clone(),
            })?;
    let sig_bytes =
        hex::decode(lease.signature.trim()).map_err(|_| SignedLeaseError::MalformedSignature)?;
    let sig_arr: [u8; 64] = sig_bytes
        .as_slice()
        .try_into()
        .map_err(|_| SignedLeaseError::MalformedSignature)?;
    let signature = EdSignature::from_bytes(&sig_arr);
    // Decode the received STANDARD-base64 (RFC 4648 §4) EXACTLY — the signature
    // covers these bytes. Do NOT strip '=' first: a canonically-padded body (CBOR
    // length % 3 != 0) carries real padding, and stripping it makes the input
    // non-canonical so `STANDARD` (RequireCanonical) would wrongly reject a valid
    // lease. `.trim()` only removes incidental surrounding whitespace.
    let body_bytes = base64::engine::general_purpose::STANDARD
        .decode(lease.lease_bytes.trim())
        .map_err(|_| SignedLeaseError::MalformedBody)?;
    vk.verify_strict(&body_bytes, &signature)
        .map_err(|_| SignedLeaseError::BadSignature)?;
    parse_lease_body(&body_bytes)
}

/// Parse the signed CBOR lease body into the fields the install path needs. A
/// missing required field is a typed error (never a panic).
fn parse_lease_body(bytes: &[u8]) -> Result<LeaseBody, SignedLeaseError> {
    let value: ciborium::value::Value =
        ciborium::from_reader(bytes).map_err(|_| SignedLeaseError::MalformedBody)?;
    let map = value.as_map().ok_or(SignedLeaseError::MalformedBody)?;
    let get = |key: &str| -> Option<&ciborium::value::Value> {
        map.iter()
            .find(|(k, _)| k.as_text() == Some(key))
            .map(|(_, v)| v)
    };
    // A REQUIRED text field: must be present AND non-empty. Fail closed — a
    // signed-but-malformed body (a field omitted, or present-but-empty) is
    // rejected, never installed with an empty id that could mis-bind enforcement.
    let required_text = |key: &str| -> Result<String, SignedLeaseError> {
        match get(key).and_then(ciborium::value::Value::as_text) {
            Some(s) if !s.is_empty() => Ok(s.to_owned()),
            _ => Err(SignedLeaseError::MalformedBody),
        }
    };
    let integer = |key: &str| -> Option<i64> {
        get(key)
            .and_then(ciborium::value::Value::as_integer)
            .and_then(|i| i64::try_from(i).ok())
    };
    let licence_id = required_text("licence_id")?;
    let instance_binding_id = required_text("instance_binding_id")?;
    let serial = required_text("serial")?;
    let not_after = integer("not_after").ok_or(SignedLeaseError::MalformedBody)?;
    // gpu_limit fails CLOSED: ABSENT means Unlimited, but a value that is PRESENT
    // and out of range for a u32 GPU count (negative, non-integer, or > u32::MAX)
    // is MalformedBody — never silently folded to `Unlimited` (the LEAST
    // restrictive), which would let a malformed-but-signed lease grant unlimited
    // GPUs. `as_conversions` is denied, so the bound is an explicit `u32::try_from`.
    let gpu_limit = match get("gpu_limit") {
        None => None,
        Some(value) => Some(
            value
                .as_integer()
                .and_then(|i| i64::try_from(i).ok())
                .and_then(|n| u32::try_from(n).ok())
                .ok_or(SignedLeaseError::MalformedBody)?,
        ),
    };
    let hardware_class = get("hardware_class")
        .and_then(ciborium::value::Value::as_text)
        .map(str::to_owned);
    Ok(LeaseBody {
        licence_id,
        instance_binding_id,
        serial,
        not_after,
        gpu_limit,
        hardware_class,
    })
}

/// Why a signed lease was rejected. Like [`KeyTrustError`], a rejection only ever
/// **withholds** the next lease — it never tightens the machine.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum SignedLeaseError {
    /// The lease's `signerKeyId` is not in the trusted set (unknown, revoked, or
    /// out of validity).
    #[error("lease signer key id {kid} is not trusted")]
    UnknownSigner {
        /// The unrecognised signer key id.
        kid: String,
    },
    /// The signature field is not 64 bytes of lower-case hex.
    #[error("the lease signature is malformed")]
    MalformedSignature,
    /// The `leaseBytes` are not valid standard-base64 / CBOR.
    #[error("the signed lease body is malformed")]
    MalformedBody,
    /// The Ed25519 signature did not verify over the signed body (tamper or
    /// wrong key).
    #[error("the lease signature did not verify")]
    BadSignature,
}

// ===========================================================================
// Wire requests/responses (verbatim field names, Conspect /v0 v0.6.1).
// ===========================================================================

/// The 9-rung enforcement-ladder state, returned as **data** inside a `200`
/// (never a control verb). The client records it but never acts on it to tighten
/// — enforcement is driven by the local lease ladder ([`crate::ladder`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum EnforcementState {
    /// Within the lease term.
    Compliant,
    /// In the grace window.
    Grace,
    /// Soft-lapsed (data only).
    LapsedSoft,
    /// Hard-lapsed (data only).
    LapsedHard,
    /// An evaluation/trial grant.
    Evaluation,
    /// Administratively on hold.
    OnHold,
    /// The detected hardware class does not match the licensed class.
    ClassMismatch,
    /// More GPUs in use than licensed.
    OverGpu,
    /// The entitlement is revoked (lease withheld — revocation by non-reissue).
    Revoked,
}

// NOTE: device-side online-activate is DEFERRED (ADR-I006 decision point 11).
// The activation request carried a server-issued `serverNonce` (the per-instance
// lease-chain freshness anchor) which the Conspect server does not yet issue (it
// is part of the device-credential mechanism the spec marks "deferred to
// ADR-0036 §Deferred / not yet available"), so the device cannot obtain a valid
// activate nonce today — a real server `422`s the empty value. Per rule 6 (never
// ship a stub/scaffold), the `ActivateRequest`/`ActivateResponse` wire types and
// the activate call path are NOT shipped. Onboarding is via the operator/portal:
// the operator activates in the Conspect portal and the signed lease reaches the
// device through the three existing install surfaces (control-upload, the
// offline file-drop watcher, the mesh relay) that all feed
// `LeaseStore::install_binding`; this client's job is to RENEW that lease. The
// activate slice is re-added when the server-nonce issuance flow lands.

/// The channel a heartbeat arrives on — a **closed** enum over the exact set the
/// Conspect `/v0` wire accepts (`direct`/`relay`/`file`). It is the full,
/// exhaustive vocabulary by design: modelling it as a closed enum (not an open
/// `String`) makes an out-of-enum value structurally impossible to send, so a
/// future server `422` for an unknown transport label cannot occur. The device
/// phone-home always reports [`Transport::Direct`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Transport {
    /// A direct device→server heartbeat (the phone-home channel).
    #[default]
    Direct,
    /// Relayed through a local-mesh peer (Conspect mesh relay).
    Relay,
    /// Carried via an offline file drop.
    File,
}

/// The `POST /organisations/{orgId}/heartbeat` request body (verbatim field
/// names) — the minimal licensing keep-alive: the binding id, the lease serial
/// head of the chain, the salted fingerprint digest, the app version, the
/// transport, and the single-use PoP `nonce`. **No** raw identifier, **no**
/// telemetry (heartbeat ≠ telemetry). `Deserialize` so the in-process fake (and
/// any body-inspecting test) can parse the exact serialised bytes back.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct HeartbeatRequest {
    /// The instance binding id (the seat-consuming, lease-bearing unit).
    pub binding_id: String,
    /// The serial of the lease the device currently holds (head of its chain),
    /// or `null` when none is held yet.
    pub lease_serial: Option<String>,
    /// The device's salted hardware-fingerprint digest (lower-case hex).
    pub fingerprint_digest: String,
    /// The engine version reporting in.
    pub app_version: String,
    /// The channel the heartbeat arrived on (a closed enum — never an open
    /// string, so an out-of-vocabulary value cannot be sent).
    pub transport: Transport,
    /// The single-use device-PoP challenge nonce (lower-case hex) bound into the
    /// signed PoP pre-image — the `nextNonce` from the prior response, or a fresh
    /// `GET /challenge` at cold start (ADR-I007). v0.9.0 requires it on every
    /// heartbeat; the matching `Conspect-Device-PoP` header carries the proof.
    pub nonce: String,
}

/// A `GET /v0/devices/licence/challenge` response (ADR-I007): a freshly-minted
/// single-use device-PoP challenge nonce + its short expiry. The client fetches
/// one only at cold start / when it has no usable `nextNonce`; steady-state it
/// reuses the prior heartbeat response's `nextNonce` (RFC 9449 DPoP-nonce style).
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct DeviceChallenge {
    /// The single-use challenge nonce (`^[0-9a-f]{64}$`, 32 bytes / 64 lower-case
    /// hex). Signed inside the PoP pre-image; burned on the first successful proof.
    pub nonce: String,
    /// When the nonce expires (epoch milliseconds) — issued + ~120 s. A proof
    /// presented after this is rejected; the client fetches a fresh challenge.
    pub expires_at_ms: i64,
    /// The **server-assigned** durable instance id (`ib_<uuidv7>`, ADR-0015)
    /// reserved with this nonce (REQUIRED since Conspect v0.16.0). A **first-contact**
    /// device (no prior binding) echoes it as the activate `instanceId` and binds it
    /// into the PoP pre-image's `instance_id`, so the proof, the binding, and the
    /// lease share ONE server id (ADR-I008). A **renewing** device already knows its
    /// durable id and signs the PoP over THAT, ignoring this value — so it is
    /// load-bearing only on the enrolment path.
    pub instance_id: String,
}

impl DeviceChallenge {
    /// Assemble a challenge (the in-process fake + the cli transport build it
    /// explicitly; the type is `#[non_exhaustive]`).
    #[must_use]
    pub fn new(nonce: String, expires_at_ms: i64, instance_id: String) -> Self {
        Self {
            nonce,
            expires_at_ms,
            instance_id,
        }
    }
}

/// The `POST …/heartbeat` response.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct HeartbeatResponse {
    /// The freshly-signed 35-day lease, or `null` when the entitlement is revoked
    /// (the running output continues by ageing — never off air).
    #[serde(default)]
    pub lease: Option<ServerLease>,
    /// The enforcement-ladder rung (data inside the 200).
    pub enforcement_state: EnforcementState,
    /// When the next monthly heartbeat is due (epoch milliseconds). The loop
    /// sleeps to this instant.
    pub next_due: i64,
    /// The NEXT single-use device-PoP challenge nonce (RFC 9449 DPoP-nonce style,
    /// ADR-I007): signed on the following heartbeat so the steady-state hot path
    /// needs no extra `GET /challenge` round-trip. Single-use; burned on the next
    /// successful proof. `#[serde(default)]` so a server that omits it (or an older
    /// server) leaves it empty — the client then fetches a fresh `/challenge` next
    /// cycle (fail closed: a missing nextNonce never reuses a prior one).
    #[serde(default)]
    pub next_nonce: String,
}

impl HeartbeatResponse {
    /// Assemble a heartbeat response. A constructor is provided because the type
    /// is `#[non_exhaustive]` (a versioned wire response): the in-process fake
    /// server + the cli's transport build it explicitly.
    #[must_use]
    pub fn new(
        lease: Option<ServerLease>,
        enforcement_state: EnforcementState,
        next_due: i64,
        next_nonce: String,
    ) -> Self {
        Self {
            lease,
            enforcement_state,
            next_due,
            next_nonce,
        }
    }
}

/// A `POST /organisations/{orgId}/activate` request — the **first-contact
/// enrolment** body (ADR-I008). Distinct from the renew [`HeartbeatRequest`]: it
/// carries the full device-identity triple, the device-PoP **public** key (set HERE
/// and only here — the registration path where the server stores it and derives the
/// lease `cnf_jkt`), and the single-use challenge nonce. The request MUST also carry
/// the `Conspect-Device-PoP` header (a proof over the same nonce, bound to the
/// server-assigned `instance_id`).
///
/// Serialised camelCase; the byte-exact required set (Conspect v0.9.0 → v0.38.0).
/// The whole device-facing wire — [`HeartbeatRequest`], [`HeartbeatResponse`],
/// `ActivateRequest`, [`ActivateResponse`], and [`DeviceChallenge`] — is byte-stable
/// across Conspect API v0.16.0 → v0.38.0 (the v0.16.0 `DeviceChallenge.instanceId`
/// addition is the lone delta from the v0.9.0 baseline; this merged activate +
/// heartbeat + device-PoP client stays field-for-field aligned through v0.38.0).
/// Verified 2026-06-20 against the live Conspect reference (api.conspect.studio,
/// v0.38.0). The deprecated `serverNonce` is **never** a field (do not send it; the
/// device proves liveness via the PoP challenge nonce, not by supplying the lease nonce).
/// `Deserialize` is derived too (mirroring [`HeartbeatRequest`]) so a server-side
/// or test consumer can parse the exact serialised bytes back.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct ActivateRequest {
    /// The registered machine id this instance runs on.
    pub machine_id: String,
    /// The salted hardware-fingerprint digest (lower-case hex SHA-256), carried in
    /// the signed lease body. Raw serials/MACs never leave the machine.
    pub fingerprint_digest: String,
    /// The weighted fingerprint match score (0–100). Activation requires ≥ 70
    /// (`REBIND_THRESHOLD`); a lower score is a server `422`.
    pub fingerprint_score: u8,
    /// The salted hardware digest shared by co-located sibling instances on one
    /// machine — part of the seat-key identity triple.
    pub hardware_digest: String,
    /// The **server-assigned** instance id (`ib_<uuidv7>`) echoed from the
    /// [`DeviceChallenge::instance_id`] — the seat-consuming, lease-bearing unit the
    /// activation creates the binding under. Bound into the PoP pre-image too, so
    /// the proof, the binding, and the lease share one server id.
    pub instance_id: String,
    /// The instance discriminator hash — part of the seat-key identity triple.
    pub instance_discriminator_hash: String,
    /// The instance discriminator digest (lower-case hex SHA-256).
    pub instance_discriminator_digest: String,
    /// The instance's Ed25519 device proof-of-possession **public** key (base64url
    /// of the raw 32-byte point) — sourced from the persisted device key's public
    /// half, NOT the legacy `MULTIVIEW_LICENCE_DEVICE_KEY` env string. The server
    /// verifies the `COSE_Sign1` proof against this key before binding, then embeds
    /// its RFC 7638 thumbprint as the lease `cnf_jkt` (holder-of-key).
    pub device_public_key: String,
    /// The single-use device-PoP challenge nonce (lower-case hex) from
    /// `GET /challenge`. Bound into the signed PoP pre-image; burned on success.
    pub nonce: String,
    /// The 6-character single-use claim code from a paid order. **Omitted**
    /// (`None`, not serialised) to auto-issue a free non-commercial licence — the
    /// default self-host path.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub claim_code: Option<String>,
}

/// A `POST /organisations/{orgId}/activate` response (ADR-I008): the signed lease
/// the activation issued (when issued), the enforcement-ladder rung as data, and the
/// NEXT single-use device-PoP challenge nonce (DPoP-nonce style) that seeds the
/// steady-state heartbeat — so the device transitions activate → renew with no extra
/// `GET /challenge` round-trip.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct ActivateResponse {
    /// The freshly-signed lease (an `ActivationLease` — a superset of
    /// `HeartbeatLease` that [`ServerLease`] deserialises directly), or `null` when
    /// none is issued (the device stays last-good).
    #[serde(default)]
    pub lease: Option<ServerLease>,
    /// The enforcement-ladder rung (data inside the 200) — `compliant` on a fresh
    /// activation.
    pub enforcement_state: EnforcementState,
    /// The NEXT single-use device-PoP challenge nonce — signed on the following
    /// heartbeat so the steady-state hot path needs no extra `GET /challenge`.
    /// `#[serde(default)]` so a server that omits it leaves it empty (the client then
    /// cold-starts a fresh `/challenge` next cycle — fail closed, never reuse).
    #[serde(default)]
    pub next_nonce: String,
}

impl ActivateResponse {
    /// Assemble an activate response (the type is `#[non_exhaustive]`; the
    /// in-process fake + the cli transport build it explicitly).
    #[must_use]
    pub fn new(
        lease: Option<ServerLease>,
        enforcement_state: EnforcementState,
        next_nonce: String,
    ) -> Self {
        Self {
            lease,
            enforcement_state,
            next_nonce,
        }
    }
}

/// Assemble the [`ActivateRequest`] for a **first-contact** enrolment (ADR-I008):
/// the device-identity triple from `identity`, the **server-assigned**
/// `instance_id` + the challenge `nonce` echoed from `challenge`, and
/// `device_public_key` = base64url(no-pad) of the signer's raw 32-byte public key
/// (`public_key_raw`) — the authoritative device key, never the legacy
/// `MULTIVIEW_LICENCE_DEVICE_KEY` env string. `claim_code` is `None` for the free
/// tier (omitted from the wire), `Some(code)` for a paid redemption. Pure + total
/// (mirrors `build_heartbeat_request_for`); the caller serialises it once and signs
/// the PoP over those exact bytes.
#[must_use]
pub fn build_activate_request(
    identity: &DeviceIdentity,
    device_public_key_raw: [u8; 32],
    challenge: &DeviceChallenge,
    claim_code: Option<&str>,
) -> ActivateRequest {
    ActivateRequest {
        machine_id: identity.machine_id.clone(),
        fingerprint_digest: identity.fingerprint_digest.clone(),
        fingerprint_score: identity.fingerprint_score,
        hardware_digest: identity.hardware_digest.clone(),
        // Echo the SERVER-assigned instance id — never the device's own
        // `identity.instance_id`.
        instance_id: challenge.instance_id.clone(),
        instance_discriminator_hash: identity.instance_discriminator_hash.clone(),
        instance_discriminator_digest: identity.instance_discriminator_digest.clone(),
        device_public_key: base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(device_public_key_raw),
        nonce: challenge.nonce.clone(),
        claim_code: claim_code.map(ToOwned::to_owned),
    }
}

/// Build the `Conspect-Device-PoP` header for an **activate** request: the base64
/// `COSE_Sign1` over the canonical pre-image (`htm | htu | sha256(body) |
/// instance_id | nonce | iat`) with `htm = "POST"`. Identical machinery to
/// [`pop_header_value`]; the load-bearing difference (ADR-I008) is that `instance_id`
/// is the **server-assigned** [`DeviceChallenge::instance_id`] (echoed into the
/// request), so the proof binds the binding the server reserved. `body` is the exact
/// serialised [`ActivateRequest`] bytes the transport sends; `iat` is epoch seconds.
///
/// # Errors
/// [`PopError`] on a COSE/nonce failure (the caller maps it to a fail-closed
/// `HeartbeatError::Pop` — no activate is sent without a valid proof).
pub fn pop_activate_header_value(
    signer: &dyn DeviceSigner,
    htu: &str,
    body: &[u8],
    instance_id: &str,
    nonce_hex: &str,
    iat: i64,
) -> Result<String, PopError> {
    pop_header_value(signer, "POST", htu, body, instance_id, nonce_hex, iat)
}

// ===========================================================================
// Device lifecycle (ADR-I009): REBIND + DEACTIVATE wire shapes + PoP wrappers.
//
// CONTINUITY ops on an ALREADY-BOUND instance — distinct from first-contact
// activate: the PoP binds the device's OWN durable `instance_id` (NOT a server-
// assigned challenge id), and neither request carries `devicePublicKey` (the
// server verifies the STORED bound key). Byte-exact to Conspect OpenAPI v0.46.0.
// ===========================================================================

/// A `POST /organisations/{orgId}/rebind` request — the **rebind** body (ADR-I009).
/// Reactivates the device's **already-bound** instance after a lawful hardware
/// change (the fingerprint-drift self-heal); it reactivates the SAME binding (no
/// new seat) and refreshes the lease. A **continuity** op: the request carries the
/// device's OWN `binding_id`/`instance_id` (NOT a server-assigned challenge id) and
/// **no** `devicePublicKey` (the server verifies the binding's STORED Ed25519 key).
/// Serialised camelCase; the byte-exact required set (Conspect v0.46.0).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct RebindRequest {
    /// The licence the instance is bound to (the seat source).
    pub licence_id: String,
    /// The instance binding to rebind — the device's OWN seat-consuming, lease-bearing
    /// unit (ADR-0015). The rebind reactivates THIS row; it never mints a second seat.
    pub binding_id: String,
    /// The device's OWN ADR-0015 instance id carried inside the signed lease body.
    /// Bound into the PoP pre-image too (continuity — NOT a challenge id).
    pub instance_id: String,
    /// The instance discriminator hash — must match the binding on record (a divergent
    /// discriminator is a fingerprint-mismatch: a foreign instance cannot rebind here).
    pub instance_discriminator_hash: String,
    /// The salted fingerprint digest (lower-case hex SHA-256) carried in the lease body.
    pub fingerprint_digest: String,
    /// The new fingerprint's match score against the binding (0–100). Below 70 is the
    /// self-heal trigger; the score is recorded + surfaced, never used to invent a refusal.
    pub fp_score: u8,
    /// The single-use device-PoP challenge nonce (lower-case hex). Bound into the signed
    /// PoP pre-image and burned on a successful proof (ADR-0042).
    pub nonce: String,
}

/// A `POST /organisations/{orgId}/rebind` response (ADR-I009): the rebind result.
/// **Carries only a `lease_serial`** (a `UUIDv7`) — NOT an embedded signed lease
/// envelope (contrast [`ActivateResponse::lease`]/[`HeartbeatResponse::lease`]) — so
/// the device cannot install from it; it seeds the steady-state nonce from
/// `next_nonce` and the **next renew** installs the refreshed lease.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct RebindResponse {
    /// Whether a fresh lease was issued. `false` when the entitlement is revoked (the
    /// signer withholds the re-issue — never off air; the current rung is still returned).
    pub rebound: bool,
    /// The signed lease serial (a `UUIDv7`) when issued, or `null` when withheld.
    /// **REQUIRED + nullable** in the wire (v0.46.0). An `Option` field, so serde treats
    /// an absent key identically to an explicit `null` (both → `None`) — operationally
    /// identical for the device, which never installs from the response either way (it
    /// re-renews to get the refreshed lease). The device only reads it for the operator's
    /// outcome report. (The load-bearing presence check is on the non-nullable fields +
    /// `next_nonce`, which serde DOES reject when absent.)
    pub lease_serial: Option<String>,
    /// The new lease's expiry (Unix epoch ms), or `null` when withheld. REQUIRED +
    /// nullable, same handling as `lease_serial` (absent or `null` → `None`).
    pub not_after: Option<i64>,
    /// The enforcement-ladder rung as DATA (always returned, never a kill).
    pub enforcement_state: EnforcementState,
    /// Rebinds charged against the 3-free-per-licence-per-AEST-year budget so far this
    /// year, including this one (ADR-0050 §7). Server-tracked; the device only reports it.
    pub rebinds_this_year: i64,
    /// Always `false`: a rebind reactivates the SAME binding and consumes NO new seat.
    pub seat_consumed: bool,
    /// The new fingerprint match score the device reported (0–100), echoed back.
    pub fp_score: u8,
    /// The NEXT single-use device-PoP challenge nonce (DPoP-nonce style) — seeds the
    /// following heartbeat. **REQUIRED** in the wire (v0.46.0): NO `#[serde(default)]`,
    /// so a response that omits `nextNonce` is malformed and rejected (a missing nonce
    /// would silently strand the next renew otherwise).
    pub next_nonce: String,
}

impl RebindResponse {
    /// Assemble a rebind response (the type is `#[non_exhaustive]`; the in-process
    /// fake + the cli transport build it explicitly).
    #[must_use]
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        rebound: bool,
        lease_serial: Option<String>,
        not_after: Option<i64>,
        enforcement_state: EnforcementState,
        rebinds_this_year: i64,
        seat_consumed: bool,
        fp_score: u8,
        next_nonce: String,
    ) -> Self {
        Self {
            rebound,
            lease_serial,
            not_after,
            enforcement_state,
            rebinds_this_year,
            seat_consumed,
            fp_score,
            next_nonce,
        }
    }
}

/// A `POST /organisations/{orgId}/deactivate` request — the **deactivate** body
/// (ADR-I009): decommission the device's binding and return its seat. A
/// **continuity** op (the server verifies the binding's STORED key); it carries only
/// the device's OWN `binding_id` + the challenge nonce, and **no** `devicePublicKey`.
/// The release is idempotent and never errors a running output.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct DeactivateRequest {
    /// The device's OWN seat-consuming instance binding to decommission.
    pub binding_id: String,
    /// The single-use device-PoP challenge nonce (lower-case hex), bound into the
    /// signed PoP pre-image and burned on a successful proof (ADR-0042).
    pub nonce: String,
}

/// A `POST /organisations/{orgId}/deactivate` response — a **device-local
/// projection** of the wire `InstanceBinding` the 200 returns (there is no
/// `DeactivateResponse` schema upstream). The device acts only on the seat-return
/// confirmation, so it parses the load-bearing subset (`id`, `lifecycleState`,
/// `enforcementState`); serde drops the rest of the `InstanceBinding` payload
/// (`#[non_exhaustive]`, no `deny_unknown_fields`). A successful release yields
/// `lifecycle_state == "released"`.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct DeactivateResponse {
    /// The binding's server-minted id (the seat that was returned).
    pub id: String,
    /// The binding lifecycle state — `released` after a successful deactivate. Typed as
    /// a `String` (not a closed enum) for forward-compat: a new upstream lifecycle value
    /// must not break parsing of a seat-return the device only inspects for `released`.
    pub lifecycle_state: String,
    /// The enforcement-ladder rung snapshotted onto the binding (data, never a verb).
    pub enforcement_state: EnforcementState,
}

impl DeactivateResponse {
    /// Assemble a deactivate response projection (the type is `#[non_exhaustive]`;
    /// the in-process fake + the cli transport build it explicitly).
    #[must_use]
    pub fn new(id: String, lifecycle_state: String, enforcement_state: EnforcementState) -> Self {
        Self {
            id,
            lifecycle_state,
            enforcement_state,
        }
    }
}

/// Assemble the [`RebindRequest`] for the device's **already-bound** instance with an
/// **explicitly-resolved `binding_id`** (ADR-I009). The caller resolves the binding via
/// the same chain renew + deactivate use (learned binding → `store.current_binding_id()`
/// → configured `identity.binding_id`) and passes it here — so a STORE-LEARNED binding
/// (not in the static identity config) is rebound under the CORRECT id, never the
/// `instance_id` fallback. The `instance_id` stays the device's OWN durable id
/// (continuity — bound into the PoP pre-image). Pure + total; the caller serialises once
/// and signs the PoP over those exact bytes.
#[must_use]
pub fn build_rebind_request_for(
    identity: &DeviceIdentity,
    licence_id: &str,
    binding_id: &str,
    challenge: &DeviceChallenge,
) -> RebindRequest {
    RebindRequest {
        licence_id: licence_id.to_owned(),
        binding_id: binding_id.to_owned(),
        instance_id: identity.instance_id.clone(),
        instance_discriminator_hash: identity.instance_discriminator_hash.clone(),
        fingerprint_digest: identity.fingerprint_digest.clone(),
        fp_score: identity.fingerprint_score,
        nonce: challenge.nonce.clone(),
    }
}

/// Assemble the [`RebindRequest`] resolving the `binding_id` from the identity alone
/// (`identity.binding_id`, falling back to `identity.instance_id`). This is the pure
/// builder the wire tests exercise; the live client path uses
/// [`build_rebind_request_for`] with the binding resolved via the store chain
/// (`rebind_once`), so a store-learned binding is never lost. Mirrors
/// [`build_activate_request`].
#[must_use]
pub fn build_rebind_request(
    identity: &DeviceIdentity,
    licence_id: &str,
    challenge: &DeviceChallenge,
) -> RebindRequest {
    let binding_id = identity
        .binding_id
        .clone()
        .unwrap_or_else(|| identity.instance_id.clone());
    build_rebind_request_for(identity, licence_id, &binding_id, challenge)
}

/// Assemble the [`DeactivateRequest`] for the device's binding (ADR-I009): the
/// device's OWN `binding_id` + the challenge `nonce`. Pure + total.
#[must_use]
pub fn build_deactivate_request(binding_id: &str, nonce: &str) -> DeactivateRequest {
    DeactivateRequest {
        binding_id: binding_id.to_owned(),
        nonce: nonce.to_owned(),
    }
}

/// Build the `Conspect-Device-PoP` header for a **rebind** request (ADR-I009): the
/// base64 `COSE_Sign1` over the canonical pre-image with `htm = "POST"`. Identical
/// machinery to [`pop_header_value`]; `instance_id` is the device's **OWN** durable
/// id (continuity — the server verifies the STORED key). `body` is the exact
/// serialised [`RebindRequest`] bytes the transport sends; `iat` is epoch seconds.
///
/// # Errors
/// [`PopError`] on a COSE/nonce failure (the caller maps it to a fail-closed
/// `HeartbeatError::Pop` — no rebind is sent without a valid proof).
pub fn pop_rebind_header_value(
    signer: &dyn DeviceSigner,
    htu: &str,
    body: &[u8],
    instance_id: &str,
    nonce_hex: &str,
    iat: i64,
) -> Result<String, PopError> {
    pop_header_value(signer, "POST", htu, body, instance_id, nonce_hex, iat)
}

/// Build the `Conspect-Device-PoP` header for a **deactivate** request (ADR-I009):
/// the base64 `COSE_Sign1` over the canonical pre-image with `htm = "POST"` and the
/// device's **OWN** durable `instance_id` bound in (continuity). `body` is the exact
/// serialised [`DeactivateRequest`] bytes; `iat` is epoch seconds.
///
/// # Errors
/// [`PopError`] on a COSE/nonce failure (the caller maps it to a fail-closed
/// `HeartbeatError::Pop` — no deactivate is sent without a valid proof).
pub fn pop_deactivate_header_value(
    signer: &dyn DeviceSigner,
    htu: &str,
    body: &[u8],
    instance_id: &str,
    nonce_hex: &str,
    iat: i64,
) -> Result<String, PopError> {
    pop_header_value(signer, "POST", htu, body, instance_id, nonce_hex, iat)
}

// ===========================================================================
// The LicenceServer seam + the HeartbeatClient loop.
// ===========================================================================

/// The licence-server seam. Implemented by the real HTTP transport (at the
/// cli/app boundary, which owns `reqwest`) and by an in-process fake for tests.
///
/// Uses native `async fn` in trait with a `Send` future (the house pattern —
/// mirrors `multiview-control`'s `ZowietekTransport`), so the loop can be
/// `tokio::spawn`'d. The client is generic over the server (`HeartbeatClient<S>`)
/// rather than `dyn`-dispatched: the spawn point holds exactly one concrete
/// server, so generics are alloc-free and the seam stays clean.
pub trait LicenceServer: Send + Sync {
    /// Fetch the published well-known key-trust document.
    fn fetch_keys(
        &self,
    ) -> impl std::future::Future<Output = Result<LicensingKeys, HeartbeatError>> + Send;

    /// `GET /v0/devices/licence/challenge?orgId={org}` — a fresh single-use
    /// device-PoP challenge nonce (ADR-I007). Consulted only at cold start / when
    /// no usable `nextNonce` is held; steady-state the prior response's `nextNonce`
    /// is reused. A transport failure here keeps last-good (the caller skips the
    /// cycle, never off air).
    fn fetch_challenge(
        &self,
        org: &str,
    ) -> impl std::future::Future<Output = Result<DeviceChallenge, HeartbeatError>> + Send;

    /// `POST /organisations/{org}/heartbeat` with a required `Idempotency-Key` and
    /// the required `Conspect-Device-PoP` header (`pop_header`, a base64 `COSE_Sign1`
    /// over the canonical pre-image — ADR-I007).
    ///
    /// `body` is the EXACT JSON bytes the leaf crate serialised the request to and
    /// computed `sha256(body)` over for the PoP pre-image — the transport sends
    /// these bytes **verbatim** (content-type `application/json`), so the device and
    /// the server hash byte-for-byte the same body (no re-serialisation drift). The
    /// body carries the matching single-use `nonce`.
    ///
    /// # Error-mapping contract (implementors MUST uphold — the burned-nonce boundary)
    ///
    /// [`run_once`](HeartbeatClient::run_once) keys its retry decision entirely off
    /// **which** [`HeartbeatError`] variant this returns, because the single-use PoP
    /// nonce in `body` is burned the instant the server SEES the request. So the
    /// variant MUST encode whether the request reached the server:
    ///
    /// * [`HeartbeatError::Transport`] — an **AMBIGUOUS** failure: **no response was
    ///   received** (connection/TLS/timeout/DNS, or a `5xx`). This is the **only**
    ///   variant `run_once` REPLAYS verbatim, so it MUST be reserved for genuine
    ///   no-contact / lost-response failures (the server may or may not have
    ///   committed). A pre-send or purely-local error that never reached the server
    ///   (e.g. failing to build the request) MUST map here, never to
    ///   [`Malformed`](HeartbeatError::Malformed) — mislabelling it `Malformed` would
    ///   make `run_once` drop a nonce the server never saw.
    /// * [`HeartbeatError::ServerRejected`] — a **DEFINITIVE** received non-2xx the
    ///   device cannot fix by resending the same bytes (`401 pop-invalid`/
    ///   `pop-required`, `409` idempotency/body-mismatch).
    /// * [`HeartbeatError::Malformed`] — a **DEFINITIVE** failure that MUST be emitted
    ///   **only after a RECEIVED `2xx`** whose body would not parse. A `2xx` means the
    ///   server processed the request and burned the nonce, so `run_once` treats this
    ///   exactly like `ServerRejected` (drop the pinned attempt + nonce, fresh
    ///   `/challenge` next cycle). **Do NOT** return `Malformed` for any pre-send,
    ///   no-response, or otherwise un-contacted error.
    ///
    /// The shipped `ConspectHttpServer` transport (in `multiview-cli`, at the cli/app
    /// boundary) and the in-process test fake uphold this: the body is decoded only
    /// after `status.is_success()`. This is a trait-level invariant the type cannot
    /// enforce — a second transport MUST honour it (see the
    /// [`Malformed`](HeartbeatError::Malformed) docs for the rationale).
    fn heartbeat(
        &self,
        org: &str,
        body: Vec<u8>,
        idempotency_key: &str,
        pop_header: &str,
    ) -> impl std::future::Future<Output = Result<HeartbeatResponse, HeartbeatError>> + Send;

    /// `POST /organisations/{org}/activate` — the **first-contact enrolment** call
    /// (ADR-I008), with a required `Idempotency-Key` and the required
    /// `Conspect-Device-PoP` header. `body` is the EXACT serialised
    /// [`ActivateRequest`] bytes the PoP signed over (sent verbatim — no drift); it
    /// carries `devicePublicKey` + the server-assigned `instanceId` + the challenge
    /// `nonce`.
    ///
    /// The **identical error-mapping contract** as [`heartbeat`](Self::heartbeat)
    /// applies (the burned-nonce boundary is the same single-use challenge nonce):
    /// [`HeartbeatError::Transport`] = ambiguous (no response / `5xx`, replay the
    /// pinned attempt verbatim); [`HeartbeatError::ServerRejected`] = a definitive
    /// received non-2xx (`401 pop-invalid`/`422` low score); and
    /// [`HeartbeatError::Malformed`] only after a received `2xx` whose body would not
    /// parse. The shipped transport decodes the body only after `status.is_success()`,
    /// upholding it.
    fn activate(
        &self,
        org: &str,
        body: Vec<u8>,
        idempotency_key: &str,
        pop_header: &str,
    ) -> impl std::future::Future<Output = Result<ActivateResponse, HeartbeatError>> + Send;

    /// `POST /organisations/{org}/rebind` — the **device REBIND** lifecycle call
    /// (ADR-I009), with a required `Idempotency-Key` and the required
    /// `Conspect-Device-PoP` header. `body` is the EXACT serialised [`RebindRequest`]
    /// bytes the PoP signed over (sent verbatim — no drift); it carries the device's
    /// own binding/instance id + the challenge `nonce`, and **no** `devicePublicKey`
    /// (continuity — the server verifies the STORED bound key).
    ///
    /// # Error-mapping contract (implementors MUST uphold — the lifecycle refinement)
    ///
    /// The same burned-nonce boundary as [`heartbeat`](Self::heartbeat), **with one
    /// deliberate refinement for the lifecycle verb**:
    /// [`HeartbeatError::Transport`] = AMBIGUOUS (no response / `5xx`) — replay the
    /// pinned attempt verbatim; **and a `409` MUST also map to
    /// [`Transport`](HeartbeatError::Transport)**, not
    /// [`ServerRejected`](HeartbeatError::ServerRejected). The live `409` is overloaded
    /// (rebinds-exhausted / discriminator-mismatch / no-live-instance / the same
    /// Idempotency-Key still in progress) with **no JSON body to disambiguate**;
    /// replaying the SAME idempotency-key + body is the only choice that is correct for
    /// all of them — the server dedups, so an exhausted/mismatch re-POST returns the
    /// same `409` deterministically with **no second rebind charge**, and an in-progress
    /// request completes. Mapping `409` to `ServerRejected` (drop + mint a fresh key)
    /// would **double-charge the scarce 3-free-per-year budget** on an ambiguous
    /// failure. [`HeartbeatError::ServerRejected`] = the OTHER definitive received
    /// non-2xx the device cannot fix by resending the same bytes (`401`
    /// `pop-invalid`/`pop-required`, `403`, `404`, `422`). [`HeartbeatError::Malformed`]
    /// = only after a received `2xx` whose body would not parse. The shipped
    /// `ConspectHttpServer` lifecycle transport (`post_raw_json_lifecycle`) decodes the
    /// body only after `status.is_success()` and maps `409 → Transport`, upholding this.
    fn rebind(
        &self,
        org: &str,
        body: Vec<u8>,
        idempotency_key: &str,
        pop_header: &str,
    ) -> impl std::future::Future<Output = Result<RebindResponse, HeartbeatError>> + Send;

    /// `POST /organisations/{org}/deactivate` — the **device DEACTIVATE** lifecycle
    /// call (ADR-I009): decommission the binding and return its seat. Required
    /// `Idempotency-Key` + `Conspect-Device-PoP`; `body` is the EXACT serialised
    /// [`DeactivateRequest`] bytes (the device's own binding id + the challenge nonce,
    /// **no** `devicePublicKey` — continuity). The release is idempotent and the
    /// **identical lifecycle error-mapping contract** as [`rebind`](Self::rebind)
    /// applies (`409 → Transport`/replay-same-key; `401`/`403`/`404`/`422` →
    /// `ServerRejected`; `2xx`-bad-body → `Malformed`). On a 200 the wire returns an
    /// `InstanceBinding`; the device parses it into the [`DeactivateResponse`]
    /// projection. Deactivate installs **nothing** — the local last-good lease ages out
    /// naturally (never off air).
    fn deactivate(
        &self,
        org: &str,
        body: Vec<u8>,
        idempotency_key: &str,
        pop_header: &str,
    ) -> impl std::future::Future<Output = Result<DeactivateResponse, HeartbeatError>> + Send;
}

/// A failure talking to the licence server. None of these tighten the machine —
/// the caller keeps the last-good lease and lets it age (never off air).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum HeartbeatError {
    /// An **ambiguous** failure where **no HTTP response was received** — a
    /// connection error, TLS handshake failure, timeout, DNS failure, or a `5xx`
    /// (the server may or may not have processed the request). On a heartbeat
    /// mutation the device does NOT know whether the server received + committed it,
    /// so a pinned attempt is REPLAYED verbatim next cycle (same Idempotency-Key +
    /// same body + same single-use nonce) — the server dedupes, never a duplicate
    /// lease, never a stranding mismatch (ADR-I007 §8).
    #[error("licence-server transport error: {0}")]
    Transport(String),
    /// A **definitive** server rejection where an HTTP RESPONSE WAS received with a
    /// status the device cannot fix by replaying the same bytes — `401`
    /// `pop-invalid`/`pop-required` (the single-use PoP nonce was SEEN + burned) or
    /// `409` idempotency/body-mismatch. The device KNOWS the server processed +
    /// rejected this attempt, so the pinned attempt is DROPPED and the burned nonce
    /// discarded → the next cycle fetches a FRESH `/challenge` and signs a FRESH
    /// proof (recovery). The device **key is unchanged** (only the nonce burned).
    /// Keeps last-good, never off air (ADR-I007 §8, round 3).
    #[error("licence-server rejected the request (definitive, response received): {0}")]
    ServerRejected(String),
    /// A **definitive** failure: a `2xx` was RECEIVED but its body could not be
    /// parsed — so the server **processed the request and BURNED the single-use PoP
    /// nonce**, exactly as a [`ServerRejected`](Self::ServerRejected) `4xx` does. When
    /// this arrives from the [`heartbeat`](LicenceServer::heartbeat) seam it is
    /// treated as a definitive rejection: [`run_once`](HeartbeatClient::run_once)
    /// DROPS the pinned attempt + the burned nonce and the next cycle fetches a FRESH
    /// `/challenge` — it is **never** replayed (a burned nonce replayed loops
    /// `pop-invalid` forever and strands renewal). Keeps last-good, never off air
    /// (ADR-I007 §8, round 4).
    ///
    /// **The contract that makes the burned-nonce inference sound:** the live
    /// transport (`ConspectHttpServer::post_raw_json`, at the cli/app boundary)
    /// decodes the body **only after** `status.is_success()` has already passed, so
    /// any `Malformed` it returns is, by construction, a received-2xx-with-bad-body —
    /// never a no-response/`5xx` failure (those are
    /// [`Transport`](Self::Transport), the ONLY ambiguous case that replays). The
    /// in-process fake server upholds the same contract. The one OTHER construction
    /// site — `run_once` mapping a request-**body-serialise** failure to `Malformed`
    /// — returns BEFORE any send, so it can never reach the step-4 retry arm and is
    /// not a received contact.
    #[error("malformed licence-server response: {0}")]
    Malformed(String),
    /// The key-trust chain failed to verify (fail closed on trust).
    #[error("key trust failed: {0}")]
    KeyTrust(#[from] KeyTrustError),
    /// The returned signed lease failed to verify.
    #[error("signed lease verification failed: {0}")]
    SignedLease(#[from] SignedLeaseError),
    /// The signed lease is already expired (its cryptographically-signed
    /// `not_after` is at or before now) — a stale or replayed signed lease is
    /// rejected, never installed as a fresh term. The last-good lease is kept.
    #[error("signed lease is already expired (not_after in the past); keeping last-good")]
    LeaseExpired,
    /// The signed lease's `instance_binding_id` does not match this device's
    /// established binding — a valid lease minted for ANOTHER device cannot be
    /// replayed onto this one (cross-instance replay defence). The last-good lease
    /// is kept; nothing is installed.
    #[error("signed lease binds a different instance; refusing cross-instance install")]
    BindingMismatch,
    /// The device's salted hardware fingerprint does not match the lease's machine
    /// (the store's fingerprint-continuity gate rejected the install). The
    /// last-good lease is kept; nothing is installed.
    #[error("device fingerprint does not match; keeping last-good")]
    FingerprintMismatch,
    /// The durable idempotency-nonce store could not be trusted (a present-but-
    /// corrupt load, or a commit that did not persist), so the retry-stable key
    /// could not be durably minted. The mutation is NOT sent this cycle (it could
    /// carry a colliding key across a restart); the last-good lease is kept and the
    /// cycle retries. A nonce-store I/O failure never tightens output (inv #1/#10).
    #[error("durable idempotency nonce unavailable; not sending a mutation: {0}")]
    NonceStore(#[from] NonceError),
    /// The device proof-of-possession could not be built (a malformed/absent PoP
    /// challenge nonce, or a `COSE_Sign1` assembly failure). The heartbeat mutation
    /// is NOT sent this cycle; the last-good lease is kept and the cycle retries
    /// (it fetches a fresh challenge next time). A PoP failure never tightens output
    /// (inv #1/#10) — this is the v0.9.0 enforced-PoP fail-closed path.
    #[error("device proof-of-possession unavailable; not sending a mutation: {0}")]
    Pop(#[from] PopError),
}

/// The device identity material — salted digests + opaque ids only (data
/// minimisation, brief §8). The cli assembles this from the fingerprint
/// subsystem; this crate never gathers raw identifiers.
///
/// The RENEW (heartbeat) path uses `machine_id` (the idempotency-key anchor),
/// `binding_id`, `fingerprint_digest`/`fingerprint_score`, and `app_version`. The
/// remaining fields (`instance_id`, `hardware_digest`, the discriminator
/// hash/digest, `device_public_key_b64url`) are the device-credential material
/// that device-side **online-activate** carried; activate is DEFERRED (ADR-I006
/// decision point 11 — the server does not yet issue the `serverNonce`). They are
/// retained on the identity (the cli's `MULTIVIEW_LICENCE_*` config contract) so
/// the activate slice re-adds without re-plumbing the device config when the
/// server-nonce flow lands — **forward-compat**, not sent today.
#[derive(Debug, Clone)]
pub struct DeviceIdentity {
    /// The registered machine id (the idempotency-key anchor; sent on renew).
    pub machine_id: String,
    /// The salted hardware-fingerprint digest (lower-case hex SHA-256; sent on
    /// renew).
    pub fingerprint_digest: String,
    /// The weighted fingerprint score (0–100; stamped on the install seal).
    pub fingerprint_score: u8,
    /// The instance binding id, once known (the renewal addresses the binding by
    /// this id, learned from a prior signed lease body / install surface).
    pub binding_id: Option<String>,
    /// The engine app version (sent on renew).
    pub app_version: String,
    /// The instance id (seat-consuming, lease-bearing unit). **Forward-compat:**
    /// device-credential material for the deferred activate slice; not sent today.
    pub instance_id: String,
    /// The salted hardware digest (sibling-instance grouping). **Forward-compat:**
    /// device-credential material for the deferred activate slice; not sent today.
    pub hardware_digest: String,
    /// The instance discriminator hash. **Forward-compat:** device-credential
    /// material for the deferred activate slice; not sent today.
    pub instance_discriminator_hash: String,
    /// The instance discriminator digest (lower-case hex SHA-256).
    /// **Forward-compat:** device-credential material for the deferred activate
    /// slice; not sent today.
    pub instance_discriminator_digest: String,
    /// The device Ed25519 proof-of-possession public key, base64url (captured +
    /// stored). **Forward-compat:** device-credential material for the deferred
    /// activate slice + the deferred device-PoP request-signing (ADR-0096 D2); not
    /// sent today.
    pub device_public_key_b64url: String,
}

/// The heartbeat-client configuration.
#[derive(Debug, Clone)]
pub struct HeartbeatConfig {
    /// The organisation id the device heartbeats against (`{orgId}`). **Config
    /// driven** (ADR-0096 O4): the operator sets it explicitly (the free
    /// auto-issue default org is an external-doc residual), with a clearly-named
    /// placeholder default rather than a hard-coded guess.
    pub org_id: String,
    /// The Conspect API base URL (e.g. `https://api.conspect.studio/v0`), trailing
    /// slash trimmed. Carried here so the device-PoP `htu` the loop signs is the
    /// REAL request URI the cli's transport POSTs to (ADR-I007) — the signed `htu`
    /// and the actual URL must agree byte-for-byte, so both derive from this base +
    /// `org_id`. Empty when the heartbeat is unconfigured (no PoP `htu` is built —
    /// the renew path is not reached without a configured server).
    pub api_base: String,
    /// The minimum sleep between contacts when the server does not dictate a
    /// `nextDue` (or on the backoff floor).
    pub min_interval: Duration,
    /// The backoff cap after repeated failures.
    pub max_backoff: Duration,
    /// Whether **first-contact device ACTIVATE / enrolment** is enabled (ADR-I008).
    /// `false` keeps the renew-only behaviour: an un-bound device makes no server
    /// call and returns [`HeartbeatOutcome::NoBinding`]. `true` lets a fresh device
    /// enrol online (`fetch_challenge` → `activate` → install → renew). Off by
    /// default; the cli turns it on only when the device-identity triple + the
    /// activate config are present.
    pub enable_activate: bool,
    /// The optional 6-character paid claim code sent on ACTIVATE (ADR-I008). `None`
    /// (the default) **omits** `claimCode` from the activate body, auto-issuing a
    /// free non-commercial licence; `Some(code)` redeems a paid entitlement. Unused
    /// on the renew path.
    pub claim_code: Option<String>,
}

impl Default for HeartbeatConfig {
    fn default() -> Self {
        Self {
            // A clearly-named placeholder (NOT a guessed real org id): the cli
            // surfaces this as a config field; an unset value means "no free
            // default configured" (ADR-0096 O4).
            org_id: "org-unset".to_owned(),
            api_base: String::new(),
            min_interval: Duration::from_secs(60),
            max_backoff: Duration::from_secs(3600),
            // Renew-only by default — activate is opt-in (the cli enables it when
            // the device-identity triple is configured).
            enable_activate: false,
            // No paid claim code → the free non-commercial auto-issue path.
            claim_code: None,
        }
    }
}

/// The outcome of one heartbeat cycle (for tests + logging).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum HeartbeatOutcome {
    /// A verified lease was installed; carries the lease serial and `nextDue`.
    Installed {
        /// The installed lease serial.
        serial: String,
        /// When the next heartbeat is due (epoch milliseconds).
        next_due: i64,
    },
    /// A **first-contact ACTIVATE / enrolment** succeeded (ADR-I008): the device
    /// registered (its `devicePublicKey` is now bound + `cnf_jkt` set server-side),
    /// the issued lease was installed, and the binding is learned — the next cycle
    /// RENEWS via the heartbeat path (seeded by the activate response `nextNonce`).
    Activated {
        /// The installed lease serial.
        serial: String,
        /// When the next heartbeat is due (epoch milliseconds).
        next_due: i64,
    },
    /// A **device REBIND** succeeded (ADR-I009): the device's already-bound instance
    /// was reactivated against refreshed hardware (the SAME binding — no new seat). The
    /// `RebindResponse` carries only a serial (no embedded signed lease), so nothing is
    /// installed here; the steady-state nonce is seeded from the response `nextNonce`
    /// and the **next renew** installs the refreshed lease. Carries the result fields
    /// the operator reads (the rebind budget consumed; the seat assertion).
    Rebound {
        /// Whether a fresh lease was issued (`false` on a withheld re-issue / revoked).
        rebound: bool,
        /// The refreshed lease serial when issued, or `None` when withheld.
        lease_serial: Option<String>,
        /// Rebinds charged against the 3-free-per-AEST-year budget this year (incl. this).
        rebinds_this_year: i64,
        /// Always `false` — a rebind consumes no new seat (surfaced so a caller asserts it).
        seat_consumed: bool,
    },
    /// A **device DEACTIVATE** succeeded (ADR-I009): the binding's seat was returned
    /// server-side (idempotent). Nothing is installed or removed locally — the
    /// last-good lease ages out via the offline ladder (never off air). Carries the
    /// binding id + its post-release lifecycle state (`released`).
    Deactivated {
        /// The binding id that was decommissioned.
        binding_id: String,
        /// The binding's lifecycle state after the release (`released`).
        lifecycle_state: String,
    },
    /// The server withheld the lease (`lease: null`, revocation by non-reissue):
    /// the last-good lease is kept and ages naturally. Carries the rung + nextDue.
    LeaseWithheld {
        /// The enforcement rung the server reported.
        state: EnforcementState,
        /// When the next heartbeat is due (epoch milliseconds).
        next_due: i64,
    },
    /// No binding is established yet (an empty store **and** no configured/learned
    /// binding), so there is nothing to RENEW. The client is renew-only —
    /// device-side online-activate is deferred (ADR-I006 decision point 11) — so it
    /// makes **no** server call this cycle and installs nothing (keeps last-good,
    /// output stays on air). A lease arrives via an install surface (control-upload
    /// / offline file-drop / mesh relay), and the next cycle renews it. Carries the
    /// `nextDue` the loop sleeps to before re-checking.
    NoBinding {
        /// When to re-check for an installed binding to renew (epoch milliseconds).
        next_due: i64,
    },
}

/// A wall-clock seam returning epoch milliseconds. The production default is
/// [`unix_millis_now`]; tests inject a controllable clock to drive time-sensitive
/// key-trust re-evaluation (signer validity-window / revocation at lease-acceptance
/// time) deterministically.
pub type NowMs = Arc<dyn Fn() -> i64 + Send + Sync>;

/// The result of an [`HeartbeatClient::install`] attempt — distinguishes a GENUINE
/// install (the lease entered the store) from the benign STALE no-op (the store
/// already held a newer lease and kept it). The caller learns the device binding
/// id ONLY on a genuine install; a stale no-op installed nothing, so learning its
/// binding would poison the device identity with a non-installed (possibly foreign)
/// lease.
enum InstallOutcome {
    /// The verified lease was installed into the store.
    Installed {
        /// The installed lease serial.
        serial: String,
    },
    /// The store already held a newer lease (a benign no-op — never off air).
    /// Nothing was installed; the binding id is NOT learned from this.
    StaleNoop {
        /// The (not-installed) incoming lease serial.
        serial: String,
    },
}

/// The device heartbeat client: drives the verified-lease install convergence and
/// nothing else. Holds only the server handle, the shared lease store, the pinned
/// root, config, and identity — **no engine handle** (invariant #10).
pub struct HeartbeatClient<S: LicenceServer> {
    server: Arc<S>,
    store: Arc<LeaseStore>,
    pinned: PinnedRoot,
    config: HeartbeatConfig,
    identity: DeviceIdentity,
    /// The server-issued `instanceBindingId` learned from a verified lease body
    /// (or the configured identity, or — via `store.current_binding_id()` — an
    /// install surface). Renewals address the binding by THIS id — **never** the
    /// lease serial (a different object). Control-plane only; the loop is the sole
    /// writer/reader, so a plain `Mutex` is correct (no hot path).
    learned_binding_id: std::sync::Mutex<Option<String>>,
    /// The wall-clock seam (epoch ms). Read FRESH at key-trust evaluation AND
    /// again at lease-acceptance, so a signer that expires (or is revoked) during
    /// an arbitrarily-stalled network call is rejected at acceptance (no TOCTOU).
    now_ms: NowMs,
    /// The retry-stable `Idempotency-Key` state for the CURRENT logical operation.
    /// A key is minted ONCE per logical operation and reused on every retry; it
    /// rotates ONLY after a successful contact (install / stale-no-op / withheld),
    /// so a lost-response retry replays the SAME key (the server dedupes — never a
    /// duplicate binding/lease) while a genuinely-new operation gets a fresh key.
    /// Derived from a monotonic per-client counter + the device identity — NEVER
    /// the wall clock (a fresh-per-call wall-clock key defeats dedup). Control-plane
    /// only; the loop is the sole accessor, so a plain `Mutex` is correct.
    idempotency: std::sync::Mutex<IdempotencyState>,
    /// The durable backing for the idempotency mint counter, so the per-operation
    /// nonce survives a restart (a post-restart op never reuses a prior lifetime's
    /// key). A SEAM: the cli supplies a file-backed impl; the in-memory default is
    /// used otherwise.
    nonce_store: SharedNonceStore,
    /// The bound Ed25519 device key for the v0.9.0 device-PoP proof (ADR-I007). A
    /// SEAM: the cli supplies a generated + durably-persisted keypair; signing is
    /// deterministic (RFC 8032 — no RNG in the leaf crate). `None` only for the
    /// pre-PoP constructors retained for the existing lease/trust/idempotency tests
    /// (those never reach the heartbeat mutation against a real server); a `None`
    /// signer on the renew path is a `PopError` (fail closed — keep last-good),
    /// never a heartbeat without a proof.
    device_signer: Option<Arc<dyn DeviceSigner>>,
    /// The held single-use device-PoP challenge nonce for the NEXT heartbeat — the
    /// `nextNonce` from the prior response (RFC 9449 DPoP-nonce style), or freshly
    /// fetched at cold start. Control-plane only; the loop is the sole accessor, so
    /// a plain `Mutex` is correct (no hot path). `None` until the first
    /// `/challenge` / response, and cleared when the server rejects it.
    pop_nonce: std::sync::Mutex<Option<PopNonce>>,
    /// The in-flight heartbeat attempt, pinned across retries (ADR-I007 retry
    /// coupling). The Idempotency-Key, the EXACT serialised body bytes (which carry
    /// the PoP challenge nonce), and the `COSE_Sign1` proof are ONE immutable unit: a
    /// retry of an ambiguous/failed contact replays this verbatim (so a lost-response
    /// retry presents the SAME key with the SAME body — never the same key with a fresh
    /// nonce, which a strict server rejects as an idempotency body-mismatch and
    /// could strand the client). It is set when a NEW logical operation is built and
    /// cleared ONLY on a successful contact (then the next cycle mints afresh).
    ///
    /// **Verb-keyed (ADR-I009, money-path defence).** The pin is keyed by
    /// [`AttemptVerb`] in PER-VERB slots so the verbs CANNOT contaminate each other's
    /// pending state: the automatic renew loop (`run_once`) only ever consumes/clears
    /// the `Renew`/`Activate` slot, and the operator-invoked one-shot ops only ever
    /// consume/clear their own `Rebind`/`Deactivate` slot. An ambiguous `/rebind`'s pin
    /// therefore PERSISTS (same idempotency-key/body/nonce) until the OPERATOR's rebind
    /// retry replays it verbatim — the background heartbeat loop is physically unable to
    /// consume it (which would let a definitive `/heartbeat` rejection clear the rebind
    /// pin → a fresh key on the operator's retry → a SECOND charge against the scarce
    /// 3-free-per-AEST-year budget). Control-plane only.
    pending: std::sync::Mutex<PendingSlots>,
}

/// Which logical operation a [`PendingAttempt`] belongs to (ADR-I009). The pin is
/// keyed by this so a verb only ever replays/clears its OWN attempt — the automatic
/// renew loop can never touch an operator-invoked rebind/deactivate pin, and vice
/// versa (the no-cross-verb-replay + no-double-charge guarantee).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AttemptVerb {
    /// The automatic renew (`POST …/heartbeat`) path.
    Renew,
    /// The automatic first-contact enrolment (`POST …/activate`) path.
    Activate,
    /// The operator-invoked one-shot rebind (`POST …/rebind`) path.
    Rebind,
    /// The operator-invoked one-shot deactivate (`POST …/deactivate`) path.
    Deactivate,
}

/// Per-verb pinned-attempt slots (ADR-I009). ONE slot PER VERB so no two verbs ever
/// share pending state: an in-flight operator rebind/deactivate pin coexists with the
/// always-running renew loop's pin without either overwriting the other, and clearing
/// one verb's pin never touches another's. Renew and Activate get **separate** slots
/// (r3 fix): although a device is normally either bound→renew or unbound→activate, the
/// binding state can change between cycles (an ambiguous activate's pin persists, then a
/// lease arrives via an install surface so the next cycle renews), and a shared slot
/// would let a stale activate attempt be posted by the renew path. Separate slots make
/// that structurally impossible.
#[derive(Debug, Default)]
struct PendingSlots {
    /// The automatic renew (`POST …/heartbeat`) path's pin.
    renew: Option<PendingAttempt>,
    /// The automatic first-contact activate (`POST …/activate`) path's pin.
    activate: Option<PendingAttempt>,
    /// The operator-invoked rebind path's pin.
    rebind: Option<PendingAttempt>,
    /// The operator-invoked deactivate path's pin.
    deactivate: Option<PendingAttempt>,
}

impl PendingSlots {
    /// A `&mut` to the slot a given verb owns — one slot per verb (no sharing).
    fn slot_mut(&mut self, verb: AttemptVerb) -> &mut Option<PendingAttempt> {
        match verb {
            AttemptVerb::Renew => &mut self.renew,
            AttemptVerb::Activate => &mut self.activate,
            AttemptVerb::Rebind => &mut self.rebind,
            AttemptVerb::Deactivate => &mut self.deactivate,
        }
    }
}

/// A held device-PoP challenge nonce + the instant it expires (epoch ms), so the
/// client can discard an expired nonce before signing (the server's ~120 s TTL).
#[derive(Debug, Clone)]
struct PopNonce {
    /// The single-use challenge nonce (lower-case hex).
    nonce: String,
    /// When it expires (epoch ms); `0` when unknown (a `nextNonce` carries no
    /// explicit expiry — it is used once on the next cycle, well within the TTL).
    expires_at_ms: i64,
}

/// The pinned in-flight heartbeat attempt — the Idempotency-Key, the body bytes,
/// and the `COSE_Sign1` proof as ONE retry unit (ADR-I007). A retry of a
/// failed/ambiguous contact replays exactly these bytes; only a successful contact
/// clears it so the next cycle builds a fresh attempt. The PoP challenge nonce is
/// not stored separately — it is already embedded in `body` (and hashed into
/// `pop_header`), so pinning the body pins the nonce.
#[derive(Debug, Clone)]
struct PendingAttempt {
    /// Which logical operation this attempt belongs to (ADR-I009) — the pin is keyed
    /// by this so a verb only ever replays/clears its OWN attempt.
    verb: AttemptVerb,
    /// The retry-stable Idempotency-Key (also tracked in [`IdempotencyState`]).
    idempotency_key: String,
    /// The EXACT JSON body bytes the transport POSTs verbatim (the PoP signed
    /// `sha256` of these; carries the single-use nonce).
    body: Vec<u8>,
    /// The base64 `COSE_Sign1` `Conspect-Device-PoP` header for this body.
    pop_header: String,
}

/// A durable backing store for the idempotency-key **mint counter**, so the
/// monotonic per-operation nonce survives a process restart and a post-restart
/// operation never reuses a prior lifetime's key (cross-restart duplicate-mutation
/// defence). It is a SEAM (like the clock): the leaf crate does no I/O, so the cli
/// implements it on a small file beside the lease state; tests back it with a
/// shared cell to simulate a restart.
///
/// **Fail closed (round-6 panel).** Both operations return a [`Result`]: a store
/// that cannot be *trusted* (a present-but-corrupt/unreadable `load`, or a `commit`
/// that does not durably persist) **must** surface an error rather than a silent
/// fallback. The mint is gated on a trustworthy `load` + a successful durable
/// `commit` BEFORE the mutation is sent, so a store failure makes the heartbeat
/// keep last-good and send nothing (never a colliding-key mutation, never off air).
/// An **absent** durable value is NOT an error: a fresh device legitimately starts
/// at `0` (`load` returns `Ok(0)`); only a *present-but-untrustworthy* value errors.
pub trait NonceStore: Send + Sync {
    /// The highest committed mint counter, or `Ok(0)` when nothing is persisted yet
    /// (a fresh device). Returns [`Err`] when a value IS present but cannot be
    /// trusted (corrupt/unreadable) — never a silent `0` that would reset the
    /// high-water and re-mint a colliding key after a restart.
    ///
    /// # Errors
    /// [`NonceError`] when a present durable value cannot be read/parsed.
    fn load(&self) -> Result<u64, NonceError>;
    /// Persist `value` as the new high-water mint counter (called at mint time,
    /// before the mutation). Returns [`Err`] when the value was not durably
    /// persisted, so the caller can refuse to send a possibly-colliding mutation.
    ///
    /// # Errors
    /// [`NonceError`] when the value could not be durably written.
    fn commit(&self, value: u64) -> Result<(), NonceError>;
}

/// A durable-nonce store failure (a present-but-corrupt load, or a commit that did
/// not persist). It is an opaque message: the I/O detail lives at the cli boundary;
/// the leaf crate only needs "the durable store could not be trusted" so the mint
/// fails closed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("durable idempotency nonce store failure: {0}")]
pub struct NonceError(String);

impl NonceError {
    /// Build a [`NonceError`] from a human-readable cause (the cli passes the I/O
    /// error text).
    #[must_use]
    pub fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

/// The default in-process [`NonceStore`]: holds the counter in memory only.
///
/// This survives nothing across a restart, so a deployment that needs
/// cross-restart idempotency (the cli) supplies a durable, file-backed
/// implementation. The in-memory default is used only where no durable store is
/// wired (e.g. a process that never restarts within an idempotency window). Its
/// operations are infallible (an in-memory cell never fails), so they return `Ok`.
#[derive(Debug, Default)]
pub struct InMemoryNonceStore {
    counter: std::sync::atomic::AtomicU64,
}

impl NonceStore for InMemoryNonceStore {
    fn load(&self) -> Result<u64, NonceError> {
        Ok(self.counter.load(std::sync::atomic::Ordering::SeqCst))
    }
    fn commit(&self, value: u64) -> Result<(), NonceError> {
        self.counter
            .store(value, std::sync::atomic::Ordering::SeqCst);
        Ok(())
    }
}

/// A shareable [`NonceStore`] handle (the cli's file-backed impl, or the in-memory
/// default).
pub type SharedNonceStore = Arc<dyn NonceStore>;

/// The retry-stable idempotency-key state for [`HeartbeatClient`]. The `counter`
/// only advances when a fresh key is minted, so each logical operation owns one
/// stable key across its retries. The counter is seeded from (and committed to) a
/// durable [`NonceStore`] so it does not reset on restart.
///
/// **The in-flight `current` key is PER-VERB (ADR-I009, money-path defence).** A
/// single shared `current` would let one verb's freshly-minted key be handed to a
/// DIFFERENT verb's next build (the renew loop reusing a pending rebind's key, posting
/// a renew body under the rebind's idempotency-key) — a cross-verb idempotency
/// collision and a budget-charge hazard. Keying `current` by [`AttemptVerb`] (the same
/// per-verb slots as [`PendingSlots`]) means each verb's retry reuses only ITS OWN
/// in-flight key, and the always-monotonic `counter` still guarantees every minted key
/// is globally unique. (The verb-keyed [`PendingAttempt`] is the primary retry-stability
/// mechanism; `current` is the pre-pin mint cache, now correctly per-verb.)
#[derive(Debug, Default)]
struct IdempotencyState {
    /// Monotonic per-client mint counter (advances once per logical operation, across
    /// ALL verbs — every minted key is globally unique + monotonic).
    counter: u64,
    /// The in-flight key for the automatic renew path.
    current_renew: Option<String>,
    /// The in-flight key for the automatic activate path.
    current_activate: Option<String>,
    /// The in-flight key for the operator-invoked rebind path.
    current_rebind: Option<String>,
    /// The in-flight key for the operator-invoked deactivate path.
    current_deactivate: Option<String>,
}

impl IdempotencyState {
    /// A `&mut` to the in-flight-key slot a given verb owns — one slot per verb (no
    /// sharing), matching [`PendingSlots`] (ADR-I009 r3).
    fn current_mut(&mut self, verb: AttemptVerb) -> &mut Option<String> {
        match verb {
            AttemptVerb::Renew => &mut self.current_renew,
            AttemptVerb::Activate => &mut self.current_activate,
            AttemptVerb::Rebind => &mut self.current_rebind,
            AttemptVerb::Deactivate => &mut self.current_deactivate,
        }
    }
}

impl<S: LicenceServer> HeartbeatClient<S> {
    /// Assemble a heartbeat client with the production wall clock
    /// ([`unix_millis_now`]).
    #[must_use]
    pub fn new(
        server: Arc<S>,
        store: Arc<LeaseStore>,
        pinned: PinnedRoot,
        config: HeartbeatConfig,
        identity: DeviceIdentity,
    ) -> Self {
        Self::with_clock(
            server,
            store,
            pinned,
            config,
            identity,
            Arc::new(unix_millis_now),
        )
    }

    /// Assemble a heartbeat client with the production wall clock and a durable
    /// [`NonceStore`] for the idempotency mint counter (the cli supplies a
    /// file-backed store so the per-operation nonce survives a restart).
    #[must_use]
    pub fn with_nonce(
        server: Arc<S>,
        store: Arc<LeaseStore>,
        pinned: PinnedRoot,
        config: HeartbeatConfig,
        identity: DeviceIdentity,
        nonce_store: SharedNonceStore,
    ) -> Self {
        Self::with_clock_and_nonce(
            server,
            store,
            pinned,
            config,
            identity,
            Arc::new(unix_millis_now),
            nonce_store,
        )
    }

    /// Assemble a heartbeat client reading "now" (epoch ms) from `now_ms` — tests
    /// inject a controllable clock to exercise the key-trust re-evaluation at
    /// lease-acceptance time (the validity-window / revocation TOCTOU). The
    /// idempotency mint counter uses the in-memory default [`InMemoryNonceStore`]
    /// (no cross-restart durability — use [`HeartbeatClient::with_clock_and_nonce`]
    /// for that).
    #[must_use]
    pub fn with_clock(
        server: Arc<S>,
        store: Arc<LeaseStore>,
        pinned: PinnedRoot,
        config: HeartbeatConfig,
        identity: DeviceIdentity,
        now_ms: NowMs,
    ) -> Self {
        Self::with_clock_and_nonce(
            server,
            store,
            pinned,
            config,
            identity,
            now_ms,
            Arc::new(InMemoryNonceStore::default()),
        )
    }

    /// Assemble a heartbeat client with both the wall-clock seam and a durable
    /// [`NonceStore`] for the idempotency mint counter. The cli supplies a
    /// file-backed `nonce_store` so the per-operation nonce survives a restart and
    /// a post-restart operation never reuses a prior lifetime's key.
    #[must_use]
    pub fn with_clock_and_nonce(
        server: Arc<S>,
        store: Arc<LeaseStore>,
        pinned: PinnedRoot,
        config: HeartbeatConfig,
        identity: DeviceIdentity,
        now_ms: NowMs,
        nonce_store: SharedNonceStore,
    ) -> Self {
        Self::assemble(
            server,
            store,
            pinned,
            config,
            identity,
            now_ms,
            nonce_store,
            None,
        )
    }

    /// Assemble a heartbeat client with a bound device-PoP signer (ADR-I007) — the
    /// production constructor under v0.9.0 enforced PoP. The cli supplies a
    /// generated + durably-persisted Ed25519 keypair (the I/O + the only RNG live
    /// at the cli boundary); the loop signs the `COSE_Sign1` proof with it on every
    /// heartbeat. Uses the production wall clock + the given durable nonce store.
    #[must_use]
    pub fn with_device_signer(
        server: Arc<S>,
        store: Arc<LeaseStore>,
        pinned: PinnedRoot,
        config: HeartbeatConfig,
        identity: DeviceIdentity,
        device_signer: Arc<dyn DeviceSigner>,
    ) -> Self {
        Self::assemble(
            server,
            store,
            pinned,
            config,
            identity,
            Arc::new(unix_millis_now),
            Arc::new(InMemoryNonceStore::default()),
            Some(device_signer),
        )
    }

    /// Assemble a heartbeat client with BOTH a durable nonce store AND a bound
    /// device-PoP signer — the cli's production wiring (a file-backed nonce store
    /// so the idempotency key survives a restart, and the persisted device keypair
    /// for the PoP proof).
    #[must_use]
    pub fn with_nonce_and_signer(
        server: Arc<S>,
        store: Arc<LeaseStore>,
        pinned: PinnedRoot,
        config: HeartbeatConfig,
        identity: DeviceIdentity,
        nonce_store: SharedNonceStore,
        device_signer: Arc<dyn DeviceSigner>,
    ) -> Self {
        Self::assemble(
            server,
            store,
            pinned,
            config,
            identity,
            Arc::new(unix_millis_now),
            nonce_store,
            Some(device_signer),
        )
    }

    /// Attach (or replace) the bound device-PoP signer on an already-constructed
    /// client (a chainable builder). The production constructors
    /// ([`with_device_signer`](Self::with_device_signer) /
    /// [`with_nonce_and_signer`](Self::with_nonce_and_signer)) set it directly; this
    /// lets a caller layer a signer onto a clock-/nonce-injecting constructor
    /// (e.g. `with_clock(..).with_signer(..)`).
    #[must_use]
    pub fn with_signer(mut self, device_signer: Arc<dyn DeviceSigner>) -> Self {
        self.device_signer = Some(device_signer);
        self
    }

    /// The single struct-initialising constructor every other delegates to.
    #[must_use]
    #[allow(clippy::too_many_arguments)] // a wide assemble seam fed by the named constructors above; the public API stays narrow.
    fn assemble(
        server: Arc<S>,
        store: Arc<LeaseStore>,
        pinned: PinnedRoot,
        config: HeartbeatConfig,
        identity: DeviceIdentity,
        now_ms: NowMs,
        nonce_store: SharedNonceStore,
        device_signer: Option<Arc<dyn DeviceSigner>>,
    ) -> Self {
        // NOT seeded from `identity.binding_id` (ADR-I009): `learned_binding_id` holds
        // ONLY genuinely server-learned ids, so `resolve_binding_id` applies the honest
        // precedence (genuinely-learned → store → configured) and a stale configured id
        // never short-circuits a fresher learned/installed binding. The configured value
        // is the FINAL fallback in `resolve_binding_id` (and the renew/no-binding path).
        let learned_binding_id = std::sync::Mutex::new(None);
        Self {
            server,
            store,
            pinned,
            config,
            identity,
            learned_binding_id,
            now_ms,
            idempotency: std::sync::Mutex::new(IdempotencyState::default()),
            nonce_store,
            device_signer,
            pop_nonce: std::sync::Mutex::new(None),
            pending: std::sync::Mutex::new(PendingSlots::default()),
        }
    }

    /// The `Idempotency-Key` for the CURRENT logical operation — minted once and
    /// REPLAYED on every retry until a success rotates it. Derived from a
    /// monotonic per-client counter + the device machine id (stable, never the
    /// wall clock), so a lost-response retry carries the SAME key (the server
    /// dedupes) while a genuinely-new operation gets a distinct key. A poisoned
    /// lock recovers (it is an in-process invariant, not a durable-store failure).
    ///
    /// **Fails closed (round-6 panel).** The mint is gated on a trustworthy durable
    /// state: it reads the durable high-water (`load`) and durably persists the new
    /// high-water (`commit`) BEFORE returning the key. If `load` is untrustworthy or
    /// `commit` does not persist, it returns [`HeartbeatError::NonceStore`] and
    /// advances NOTHING — so `run_once` sends no mutation this cycle (a key that was
    /// not durably committed could collide across a restart). A retry next cycle
    /// re-attempts the mint cleanly from the unchanged counter base.
    ///
    /// # Errors
    /// [`HeartbeatError::NonceStore`] when the durable nonce store cannot be trusted
    /// (a present-but-corrupt load, or a non-persisting commit).
    fn idempotency_key(&self, verb: AttemptVerb) -> Result<String, HeartbeatError> {
        let mut guard = match self.idempotency.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        // Reuse only THIS verb's in-flight key (ADR-I009) — never another verb's, so a
        // pending rebind's key is never handed to a building renew (no cross-verb
        // idempotency collision / budget hazard).
        if let Some(existing) = guard.current_mut(verb).clone() {
            return Ok(existing);
        }
        // Read the durable high-water FIRST — a present-but-corrupt value fails
        // closed (we never reset to 0 and re-mint a colliding key after a restart).
        let durable = self.nonce_store.load()?;
        // Mint a NEW counter strictly above BOTH this process's in-memory high-water
        // AND the durable high-water — so a restart (in-memory resets to 0, durable
        // persists) never reuses a prior lifetime's value. `saturating_add` (not
        // `wrapping_add`) so the monotonic guarantee never wraps back to a reused
        // low value. The counter is GLOBAL across verbs, so every minted key is unique.
        let counter = guard.counter.max(durable).saturating_add(1);
        // Commit the new high-water DURABLY before exposing the key: only on a
        // successful persist do we advance in-process state and return. A commit
        // failure leaves `guard` untouched and propagates, so the cycle sends no
        // mutation and a retry re-mints cleanly (no un-persisted, possibly-colliding
        // key ever reaches the server).
        self.nonce_store.commit(counter)?;
        guard.counter = counter;
        let key = format!("mv-{}-{counter}", self.identity.machine_id);
        *guard.current_mut(verb) = Some(key.clone());
        Ok(key)
    }

    /// Rotate **`verb`'s** idempotency key after a SUCCESSFUL contact, so the NEXT
    /// operation of that verb mints a fresh key. Called only on a positive outcome
    /// (install / stale-no-op / withheld lease) — never on a failure, so a failure's
    /// retry replays the same key. Clears ONLY that verb's slot (ADR-I009): a renew
    /// success never rotates a pending rebind/deactivate key. A poisoned lock recovers
    /// by clearing the inner value.
    fn rotate_idempotency(&self, verb: AttemptVerb) {
        match self.idempotency.lock() {
            Ok(mut g) => *g.current_mut(verb) = None,
            Err(poisoned) => *poisoned.into_inner().current_mut(verb) = None,
        }
    }

    /// The pinned in-flight attempt to REPLAY on a retry, or `None` for a fresh
    /// logical operation. A poisoned lock recovers by reading the inner value.
    /// The pinned attempt **for `verb`**, if any (ADR-I009 verb-keyed pin). A verb
    /// only ever sees its OWN slot — so the renew loop never replays a rebind/
    /// deactivate attempt (no cross-verb replay) and an operator op never replays a
    /// renew attempt.
    fn pinned_attempt(&self, verb: AttemptVerb) -> Option<PendingAttempt> {
        let mut guard = match self.pending.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        let slot = guard.slot_mut(verb);
        // Defence in depth: a slot only ever returns an attempt whose `.verb` matches
        // the requested verb. With one slot per verb this always holds, but the explicit
        // guard means a wrong-verb attempt is NEVER replayed/posted (it is dropped so the
        // caller mints a fresh attempt for the requested verb) — no cross-verb body/proof
        // can reach the wrong endpoint (ADR-I009 r3).
        match slot {
            Some(attempt) if attempt.verb == verb => Some(attempt.clone()),
            Some(_) => {
                *slot = None;
                None
            }
            None => None,
        }
    }

    /// Pin the attempt in ITS verb's slot so any retry of an ambiguous/failed contact
    /// replays the SAME `{Idempotency-Key, body, nonce, proof}` (ADR-I007 retry
    /// coupling). The verb is read off the attempt itself ([`PendingAttempt::verb`]).
    fn pin_attempt(&self, attempt: PendingAttempt) {
        let verb = attempt.verb;
        let mut guard = match self.pending.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        *guard.slot_mut(verb) = Some(attempt);
    }

    /// Clear the pinned attempt **for `verb`** AND rotate the Idempotency-Key on a
    /// SUCCESSFUL contact — they are one retry unit, so the next op of that verb builds
    /// a wholly fresh attempt (fresh nonce + body + key). Clears ONLY that verb's slot
    /// (ADR-I009): a renew success never clears a pending rebind/deactivate, and vice
    /// versa.
    fn clear_pending(&self, verb: AttemptVerb) {
        {
            let mut guard = match self.pending.lock() {
                Ok(g) => g,
                Err(poisoned) => poisoned.into_inner(),
            };
            *guard.slot_mut(verb) = None;
        }
        self.rotate_idempotency(verb);
    }

    /// Recover from a DEFINITIVE server rejection of `verb` (`ServerRejected` — 401
    /// pop-invalid/pop-required or 409 body-mismatch; ADR-I007 §8, round 3). The
    /// single-use PoP nonce was SEEN + burned server-side, so:
    /// 1. drop the pinned attempt **for that verb** + rotate the Idempotency-Key
    ///    ([`clear_pending`]) — the burned attempt must NEVER be replayed (a verbatim
    ///    replay loops pop-invalid forever and strands the op); a DIFFERENT verb's pin
    ///    is left UNTOUCHED (ADR-I009);
    /// 2. clear any held PoP nonce so the next cycle COLD-STARTS a fresh
    ///    `GET /challenge` and signs a FRESH proof.
    ///
    /// The device **key is untouched** — only the single-use nonce is burned. Keeps
    /// last-good (never off air): the cycle still returns an error, the loop backs
    /// off, and the NEXT cycle recovers cleanly.
    fn reset_on_rejection(&self, verb: AttemptVerb) {
        self.clear_pending(verb);
        // Drop the held nextNonce — it is either the just-burned one or stale; the
        // next cycle must fetch a fresh challenge, never present a burned nonce. The
        // nonce is a single shared DPoP-nonce (the server burns whichever it saw), so
        // a definitive rejection of ANY verb invalidates the held nonce — clearing it
        // is correct regardless of verb (the next op re-fetches a fresh challenge).
        match self.pop_nonce.lock() {
            Ok(mut g) => *g = None,
            Err(poisoned) => *poisoned.into_inner() = None,
        }
    }

    /// Obtain the single-use device-PoP challenge nonce for THIS heartbeat:
    /// steady-state the held `nextNonce` (RFC 9449 DPoP-nonce style); cold start (no
    /// held nonce, or a held nonce already expired) a fresh `GET /challenge`.
    ///
    /// **Fail closed (ADR-I007).** A `/challenge` transport failure (or a malformed
    /// challenge) propagates as [`HeartbeatError`]; `run_once` then sends NO
    /// heartbeat mutation this cycle and keeps last-good (never off air). The held
    /// nonce is consumed (taken) so a failed cycle re-fetches cleanly next time
    /// rather than re-presenting a possibly-burned nonce.
    async fn obtain_pop_nonce(&self, org: &str) -> Result<String, HeartbeatError> {
        let now_ms = (self.now_ms)();
        // Take any held nonce; an expired one (server ~120 s TTL) is discarded.
        let held = {
            let mut guard = match self.pop_nonce.lock() {
                Ok(g) => g,
                Err(poisoned) => poisoned.into_inner(),
            };
            match guard.take() {
                Some(p) if p.expires_at_ms == 0 || p.expires_at_ms > now_ms => Some(p.nonce),
                // Expired or none: fall through to a fresh fetch.
                _ => None,
            }
        };
        if let Some(nonce) = held {
            return Ok(nonce);
        }
        // Cold start / expired: fetch a fresh challenge. A transport failure here is
        // a normal keep-last-good outcome (the loop backs off and retries).
        let challenge = self.server.fetch_challenge(org).await?;
        // Expiry-check the FRESH challenge too (not only a held nonce): a clock skew
        // or a slow round-trip can hand back an already-expired nonce, and signing +
        // POSTing it just earns a `pop-invalid`. Fail closed instead — skip this
        // cycle, keep last-good (never off air); the next cycle fetches afresh. A
        // re-read of `now` (not the pre-fetch sample) accounts for the round-trip.
        let now_after = (self.now_ms)();
        if challenge.expires_at_ms != 0 && challenge.expires_at_ms <= now_after {
            return Err(HeartbeatError::Pop(PopError::Nonce(format!(
                "fresh device-PoP challenge already expired (expiresAtMs {} <= now {now_after})",
                challenge.expires_at_ms
            ))));
        }
        Ok(challenge.nonce)
    }

    /// Remember the server-issued `nextNonce` for the NEXT heartbeat (steady-state
    /// DPoP-nonce). An empty `next_nonce` (a server that omitted it) clears the
    /// held nonce so the next cycle cold-starts a fresh `/challenge` — never reuse
    /// a prior, already-burned nonce.
    fn remember_next_nonce(&self, next_nonce: &str) {
        let mut guard = match self.pop_nonce.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        *guard = if next_nonce.is_empty() {
            None
        } else {
            Some(PopNonce {
                nonce: next_nonce.to_owned(),
                // A nextNonce carries no explicit expiry; it is used once on the
                // immediately-following cycle, well within the server TTL.
                expires_at_ms: 0,
            })
        };
    }

    /// Build the `Conspect-Device-PoP` header for a heartbeat: the base64 `COSE_Sign1`
    /// over the canonical pre-image (`htm | htu | sha256(body) | instance_id | nonce
    /// | iat`). `body` is the EXACT serialized request body the transport sends, so
    /// the device and server hash the same bytes; `iat` is the current epoch seconds.
    ///
    /// **Fail closed.** A missing device signer, or a COSE/nonce error, is a
    /// [`HeartbeatError::Pop`] — `run_once` sends no mutation this cycle (never off
    /// air). The renew path is never taken without a proof.
    fn build_pop_header(&self, body: &[u8], nonce_hex: &str) -> Result<String, HeartbeatError> {
        let signer = self
            .device_signer
            .as_ref()
            .ok_or_else(|| PopError::Cose("no device signer configured".to_owned()))?;
        let htu = format!(
            "{}/organisations/{}/heartbeat",
            self.api_base_for_pop(),
            self.config.org_id
        );
        let iat = (self.now_ms)() / 1000; // epoch seconds (server checks ±60s)
        let header = pop_header_value(
            signer.as_ref(),
            "POST",
            &htu,
            body,
            &self.identity.instance_id,
            nonce_hex,
            iat,
        )?;
        Ok(header)
    }

    /// The API base the PoP `htu` is built from. The leaf crate does not own the
    /// live URL (the cli's transport does), so the `htu` it signs must match the URL
    /// the transport actually POSTs to — they agree because both derive it from the
    /// same `org_id` against the same base. The base is carried on the config so the
    /// signed `htu` is the real request URI (not a placeholder).
    fn api_base_for_pop(&self) -> &str {
        self.config.api_base.as_str()
    }

    /// The **genuinely server-learned** binding id (set by
    /// [`remember_binding_id`](Self::remember_binding_id) on a real install/activate),
    /// or `None` before any learn. This is NOT seeded from the configured
    /// `identity.binding_id` — so the resolution chains can apply the honest precedence
    /// *genuinely-learned → store → configured* (ADR-I009). NEVER the lease serial.
    fn binding_id(&self) -> Option<String> {
        self.learned_binding_id.lock().ok().and_then(|g| g.clone())
    }

    /// Resolve the binding id for a continuity operation — the **honest precedence**
    /// (ADR-I009): a genuinely **server-learned** binding
    /// ([`binding_id`](Self::binding_id)) first, then the **store's installed-lease**
    /// binding ([`current_binding_id`](crate::store::LeaseStore::current_binding_id)),
    /// then the **configured** `identity.binding_id`, then `None`. Shared by the renew
    /// resolution and the rebind/deactivate lifecycle ops so all three agree.
    ///
    /// The key correctness point (the iso-lens finding): a **stale configured** id must
    /// NOT short-circuit a fresher server-learned or installed-store binding. Because
    /// `learned_binding_id` is no longer seeded from config, learned holds only real
    /// server ids; and the store (the installed lease) outranks a configured guess. A
    /// configured-only device (no learn, no installed lease) still resolves via the
    /// final `identity.binding_id` arm, so the renew/no-binding contract is unchanged.
    fn resolve_binding_id(&self) -> Option<String> {
        self.binding_id()
            .or_else(|| self.store.current_binding_id())
            .or_else(|| self.identity.binding_id.clone())
    }

    /// Record the server-issued `instanceBindingId` from a verified lease body so
    /// the next renewal addresses the binding by id.
    fn remember_binding_id(&self, binding_id: &str) {
        if let Ok(mut g) = self.learned_binding_id.lock() {
            *g = Some(binding_id.to_owned());
        }
    }

    /// Build the minimal heartbeat request from the identity (data minimisation:
    /// only the binding id, lease serial, salted digest, app version, transport, and
    /// the single-use PoP `nonce`). `lease_serial` is the head of the device's
    /// current lease chain, if any.
    #[must_use]
    pub fn build_heartbeat_request(
        identity: &DeviceIdentity,
        lease_serial: Option<String>,
        nonce: String,
    ) -> HeartbeatRequest {
        HeartbeatRequest {
            binding_id: identity.binding_id.clone().unwrap_or_default(),
            lease_serial,
            fingerprint_digest: identity.fingerprint_digest.clone(),
            app_version: identity.app_version.clone(),
            transport: Transport::Direct,
            nonce,
        }
    }

    /// Run one RENEW cycle. The client is **renew-only**: device-side
    /// online-activate is deferred (ADR-I006 decision point 11 — the server does
    /// not yet issue the `serverNonce`). So:
    ///
    /// * **No established binding** (an empty store **and** no configured/learned
    ///   binding) → there is nothing to renew. The client makes **no** server call,
    ///   installs nothing, keeps last-good, and returns
    ///   [`HeartbeatOutcome::NoBinding`]. A lease arrives via an install surface
    ///   (control-upload / offline file-drop / mesh relay), and a later cycle
    ///   renews it.
    /// * **A binding exists** → fetch + verify the key-trust chain, send the
    ///   heartbeat, verify the returned signed lease, and drive
    ///   `store.install_binding`. On any failure the store is left untouched (never
    ///   off air).
    ///
    /// # Retry contract — replay only when it is unknown whether the server received it
    ///
    /// A heartbeat carries a **single-use** PoP nonce. Whether a failed cycle replays
    /// the pinned attempt verbatim or recovers with a fresh nonce turns on ONE
    /// question: *did the server SEE this request?* (step 4 below).
    ///
    /// * **AMBIGUOUS — replay (lost-after-commit).** A
    ///   [`Transport`](HeartbeatError::Transport) error means **no response was
    ///   received** (connection/TLS/timeout/DNS, or a `5xx`): the device cannot know
    ///   whether the server committed the mutation, so the pinned
    ///   `{Idempotency-Key, body, nonce, proof}` stays pinned and is REPLAYED
    ///   byte-for-byte next cycle (the server dedupes on the stable key — never a
    ///   duplicate lease, never an idempotency body-mismatch). This is the **only**
    ///   case that replays.
    /// * **DEFINITIVE — reset (the nonce is burned).** A RECEIVED response that the
    ///   device cannot fix by resending the same bytes is definitive, so the pinned
    ///   attempt + the burned nonce are DROPPED (`reset_on_rejection`) and the next
    ///   cycle fetches a fresh `/challenge` and signs a fresh proof. Two flavours map
    ///   here identically:
    ///   * [`ServerRejected`](HeartbeatError::ServerRejected) — a received `4xx`
    ///     (`401 pop-invalid`/`pop-required`, `409` idempotency/body-mismatch);
    ///   * [`Malformed`](HeartbeatError::Malformed) — a received **`2xx`** whose body
    ///     would not parse. **A `2xx` means the server processed the request and
    ///     burned the nonce**, so a `Malformed` returned from the
    ///     [`heartbeat`](LicenceServer::heartbeat) seam is just as definitive as a
    ///     `4xx` and must never be replayed. (The
    ///     [`Malformed`](HeartbeatError::Malformed) docs state why a seam-returned
    ///     `Malformed` is always a received 2xx; the body-serialise `Malformed` built
    ///     locally below returns before any send and never reaches the step-4 arm.)
    ///
    /// **Invariant:** a single-use nonce the server has SEEN is burned — replay only
    /// when it is unknown whether the server received the request.
    ///
    /// Either way the store is untouched and the loop keeps last-good and backs off
    /// (never off air, inv #1/#10).
    ///
    /// # Errors
    /// [`HeartbeatError`] on transport / trust / verification failure. The caller
    /// (the loop) treats every error as "keep last-good and back off".
    pub async fn run_once(&self) -> Result<HeartbeatOutcome, HeartbeatError> {
        // 1. Resolve the binding to RENEW *before* any network call. The binding is
        //    addressed by the server-issued instanceBindingId — NEVER the lease
        //    serial (a different object). `established_binding` is this device's
        //    locally-anchored identity: the configured/learned heartbeat binding,
        //    OR — when neither is set but a local lease already exists — the store's
        //    current lease binding. A device that already holds a lease is NEVER
        //    "fresh": a foreign-binding lease is rejected because it has an identity
        //    to violate.
        // Resolve the binding to RENEW with the honest precedence (ADR-I009):
        // genuinely-learned → store's installed lease → configured `identity.binding_id`.
        // (The configured arm replaces the old `learned_binding_id` config pre-seed, so a
        // configured-only device still renews; a stale configured no longer outranks a
        // fresher learned/installed binding — which previously caused a BindingMismatch.)
        let established_binding = self.resolve_binding_id();

        // No established binding: either ENROL (first-contact activate, ADR-I008) or
        // — renew-only — no-op. With `enable_activate` the device registers online:
        // it fetches a challenge, signs an activate proof bound to the SERVER-assigned
        // instanceId, installs the issued lease, and learns the binding (the next
        // cycle then renews). Without it, the renew-only contract holds: no server
        // call, keep last-good (output on air), `NoBinding` — a lease arrives via an
        // install surface and a later cycle renews it. BOTH keep last-good on every
        // failure (never off air, inv #1/#10).
        let Some(established_binding) = established_binding else {
            if self.config.enable_activate {
                return self.activate_once().await;
            }
            let now_ms = (self.now_ms)();
            return Ok(HeartbeatOutcome::NoBinding {
                next_due: next_due_default(now_ms),
            });
        };

        // 2. Fetch + verify the key-trust chain (fail closed on trust). This is a
        //    pre-network fast-fail only; trust is RE-EVALUATED at lease-acceptance
        //    time (step 4) against a FRESHLY RE-FETCHED key document, so a signer
        //    that is revoked OR whose validity window elapses DURING a stalled call
        //    cannot validate the returned lease (no TOCTOU).
        let keys = self.server.fetch_keys().await?;
        let now_ms = (self.now_ms)();
        let _ = TrustedKeys::verify(&keys, &self.pinned, now_ms)?;

        // 3. Build (or REPLAY) the in-flight attempt as ONE retry unit (ADR-I007).
        //    A retry of a prior failed/ambiguous contact replays the SAME
        //    {Idempotency-Key, body bytes, PoP nonce, COSE_Sign1 proof} verbatim —
        //    so the server sees the same key with the SAME body (it dedupes, never a
        //    duplicate lease, never an idempotency body-mismatch). Only a genuinely
        //    NEW logical operation (no pinned attempt) builds fresh: it obtains the
        //    single-use challenge nonce (held `nextNonce`, or a cold-start `GET
        //    /challenge`), serialises the body ONCE, signs the `Conspect-Device-PoP`
        //    proof over `sha256(body)` + the canonical pre-image, and mints the
        //    durable Idempotency-Key. EVERY build step fails closed BEFORE the
        //    mutation (a PoP/challenge/nonce-store failure `?`-propagates and sends
        //    nothing) — keep last-good, retry next cycle, never off air (inv #1/#10).
        let attempt = if let Some(pinned) = self.pinned_attempt(AttemptVerb::Renew) {
            // A retry of an in-flight RENEW attempt — replay it verbatim. (Verb-keyed:
            // this NEVER returns a pending rebind/deactivate attempt — ADR-I009.)
            pinned
        } else {
            // A genuinely-new logical operation — build a fresh attempt + pin it.
            let held_serial = self.store.current().map(|e| e.lease.serial);
            let pop_nonce = self.obtain_pop_nonce(&self.config.org_id).await?;
            let req = Self::build_heartbeat_request_for(
                &self.identity,
                held_serial,
                Some(established_binding.clone()),
                pop_nonce,
            );
            // Serialise ONCE — the transport sends THESE bytes verbatim and the PoP
            // signs `sha256` of THESE bytes (device + server hash the same body).
            let body = serde_json::to_vec(&req)
                .map_err(|e| HeartbeatError::Malformed(format!("heartbeat body serialise: {e}")))?;
            let pop_header = self.build_pop_header(&body, &req.nonce)?;
            // Mint the durable Idempotency-Key LAST (after the fallible PoP build, so
            // a PoP failure never burns a counter), then PIN the whole unit so any
            // retry replays it byte-for-byte.
            let idempotency_key = self.idempotency_key(AttemptVerb::Renew)?;
            let attempt = PendingAttempt {
                verb: AttemptVerb::Renew,
                idempotency_key,
                body,
                pop_header,
            };
            self.pin_attempt(attempt.clone());
            attempt
        };

        // 4. RENEW the held lease by sending the pinned attempt. STATUS-AWARE retry
        //    (ADR-I007 §8, round 3):
        //    * AMBIGUOUS failure (`Transport` — NO response: conn/timeout/DNS/5xx):
        //      the server may or may not have committed it, so KEEP the attempt
        //      pinned; the next cycle REPLAYS it verbatim (same key + body + nonce)
        //      and the server dedupes.
        //    * DEFINITIVE rejection (`ServerRejected` — a response WAS received,
        //      401 pop-invalid/pop-required or 409 body-mismatch): the single-use
        //      nonce was SEEN + burned, so the attempt MUST NOT be replayed. Drop
        //      the pinned attempt + the burned nonce (`reset_on_rejection`) so the
        //      next cycle fetches a FRESH `/challenge` and signs a FRESH proof
        //      (recovery). The device key is untouched.
        //    Both keep last-good, back off, never panic (never off air, inv #1/#10).
        let resp = match self
            .server
            .heartbeat(
                &self.config.org_id,
                attempt.body.clone(),
                &attempt.idempotency_key,
                &attempt.pop_header,
            )
            .await
        {
            Ok(resp) => resp,
            Err(err @ (HeartbeatError::ServerRejected(_) | HeartbeatError::Malformed(_))) => {
                // INVARIANT: a single-use PoP nonce the server has SEEN is burned — never
                // replay it. Both arms here are a RECEIVED contact: a definitive 4xx
                // (ServerRejected) OR a 2xx whose body would not parse (Malformed).
                //
                // This rests on the LicenceServer::heartbeat error-mapping CONTRACT (see the
                // trait doc): a transport MUST emit Malformed ONLY after a received 2xx (so a
                // Malformed here ALWAYS means the server processed the request + burned the
                // nonce), and MUST route any pre-send / no-response / local error to Transport
                // (the ambiguous, replay-only arm below) — never to Malformed. If a future
                // transport mislabelled an un-contacted error as Malformed, this arm would
                // wrongly drop a nonce the server never saw. The shipped ConspectHttpServer +
                // the test fake decode the body only after status.is_success(), upholding it.
                //
                // Drop the pinned RENEW attempt + burned nonce so the next cycle recovers
                // with a fresh challenge (a fresh idempotency-keyed unit; the lease
                // re-renews safely). Verb-keyed: a pending rebind/deactivate is UNTOUCHED.
                self.reset_on_rejection(AttemptVerb::Renew);
                return Err(err);
            }
            // Ambiguous (no response / 5xx → Transport, per the trait contract): the server
            // may or may not have committed it, so leave the attempt pinned to replay verbatim.
            Err(err) => return Err(err),
        };
        let (lease, state, next_due, next_nonce) = (
            resp.lease,
            resp.enforcement_state,
            resp.next_due,
            resp.next_nonce,
        );
        // The contact succeeded → this logical operation is DONE. Clear the pinned RENEW
        // attempt + rotate the Idempotency-Key so the next cycle is a fresh unit, and
        // remember the server's `nextNonce` for it (steady-state DPoP-nonce; an empty
        // one cold-starts next cycle). The success-clear happens here for the
        // withheld-lease early return below AND every install outcome.
        self.clear_pending(AttemptVerb::Renew);
        self.remember_next_nonce(&next_nonce);

        // 5. A withheld lease (revocation by non-reissue) is a normal outcome:
        //    keep last-good, never tighten. The contact already succeeded, so
        //    `clear_pending` above rotated the key + cleared the attempt.
        let Some(server_lease) = lease else {
            return Ok(HeartbeatOutcome::LeaseWithheld { state, next_due });
        };

        // 4. Verify the returned signed lease against the trusted intermediates,
        //    then install it ANCHORED TO THIS DEVICE'S IDENTITY. Only after a
        //    SUCCESSFUL install do we learn the binding id — a rejected
        //    (expired/stale/cross-instance) lease must never mutate the learned
        //    identity (no reject-path poisoning).
        // Re-establish trust at lease ACCEPTANCE time against FRESHLY RE-FETCHED
        // key/revocation material — NOT the pre-network document. Revocation is
        // set-membership over the signed key document, so a fresh clock against the
        // STALE document cannot observe a signer added to the revocation list
        // during an arbitrarily-stalled call; only a re-fetch can. The re-fetched
        // document is itself fully re-verified (`TrustedKeys::verify` re-checks the
        // root match, the root-attested revocation signature, every intermediate's
        // `root_sig`, the signed validity window at the fresh `now()`, and the
        // revocation set), so BOTH a newly-revoked signer AND an elapsed validity
        // window are caught. A signer that is no longer trusted is dropped from the
        // re-fetched `trusted` set, so `verify_signed_lease_chain` cannot resolve
        // `signerKeyId` and rejects the lease (no TOCTOU). The re-fetch fails closed
        // on a transport error (keep last-good, never off air).
        let accept_now_ms = (self.now_ms)();
        let fresh_keys = self.server.fetch_keys().await?;
        let trusted = TrustedKeys::verify(&fresh_keys, &self.pinned, accept_now_ms)?;
        let body = verify_signed_lease_chain(&server_lease, &trusted)?;
        // Install ANCHORED to this device's identity. `remember_binding_id` fires
        // ONLY on a GENUINE install — never on the stale no-op (a Stale outcome
        // means the store kept a newer lease; nothing was installed, so learning a
        // binding from it would poison identity with a non-installed lease). An
        // install rejection (expired/cross-instance/fingerprint) propagates via `?`;
        // the retry unit was ALREADY cleared/rotated on the successful contact above
        // (the server committed the mutation under that key + returned this lease;
        // re-presenting the same key would just return the same rejectable lease), so
        // the next cycle is a genuinely-new heartbeat.
        let serial = match self.install(&server_lease, &body, Some(established_binding.as_str()))? {
            InstallOutcome::Installed { serial } => {
                self.remember_binding_id(&body.instance_binding_id);
                serial
            }
            // The store already holds a newer lease — a benign no-op, never off
            // air. Do NOT learn the binding (nothing was installed).
            InstallOutcome::StaleNoop { serial } => serial,
        };
        Ok(HeartbeatOutcome::Installed { serial, next_due })
    }

    /// One **first-contact ACTIVATE / enrolment** cycle (ADR-I008). Taken by
    /// [`run_once`](Self::run_once) only when there is NO established binding AND
    /// `enable_activate` is set. It registers the device online:
    ///
    /// 1. Fetch a fresh `GET /challenge` — activate ALWAYS needs one, both for the
    ///    single-use nonce AND the **server-assigned `instanceId`** the device enrols
    ///    under (there is no held nonce on first contact). Fail closed on an
    ///    unreachable / already-expired challenge (keep last-good, never off air).
    /// 2. Build (or REPLAY) the in-flight attempt as ONE retry unit — exactly the
    ///    heartbeat discipline: a pinned attempt replays its `{Idempotency-Key, body,
    ///    nonce, proof}` verbatim; a new op echoes the server `instanceId` into
    ///    [`ActivateRequest`] + the PoP pre-image, serialises ONCE, signs the proof
    ///    over those bytes, mints the durable key LAST, and pins the unit.
    /// 3. `POST /activate` with the same STATUS-AWARE retry: a `ServerRejected`/
    ///    `Malformed` (a received contact that burned the nonce) drops the pinned
    ///    attempt + nonce and recovers next cycle; an ambiguous `Transport` keeps it
    ///    pinned to replay verbatim. Both keep last-good, never panic.
    /// 4. On a positively-verified [`ActivateResponse`] lease: re-verify trust against
    ///    a freshly re-fetched key document, install ANCHORED via the body's own
    ///    `instance_binding_id` (the server-issued binding — `install` is called with
    ///    `None` since this is the first binding), learn the binding ONLY on a genuine
    ///    install, and seed the steady-state nonce from the response `nextNonce` so
    ///    the next cycle RENEWS with no extra `/challenge`.
    async fn activate_once(&self) -> Result<HeartbeatOutcome, HeartbeatError> {
        // 1. Pre-network key-trust fast-fail (re-verified at acceptance below).
        let keys = self.server.fetch_keys().await?;
        let now_ms = (self.now_ms)();
        let _ = TrustedKeys::verify(&keys, &self.pinned, now_ms)?;

        // 2. Build or REPLAY the in-flight activate attempt as one retry unit.
        let attempt = if let Some(pinned) = self.pinned_attempt(AttemptVerb::Activate) {
            pinned
        } else {
            // Fetch a fresh challenge — activate needs the nonce AND the
            // server-assigned instanceId. Expiry-checked like the renew path: a
            // round-trip-stale nonce is a fail-closed skip, not a doomed POST.
            let challenge = self.server.fetch_challenge(&self.config.org_id).await?;
            let now_after = (self.now_ms)();
            if challenge.expires_at_ms != 0 && challenge.expires_at_ms <= now_after {
                return Err(HeartbeatError::Pop(PopError::Nonce(format!(
                    "fresh activate challenge already expired (expiresAtMs {} <= now {now_after})",
                    challenge.expires_at_ms
                ))));
            }
            // Echo the server instanceId into the request AND bind it into the proof.
            let signer = self
                .device_signer
                .as_ref()
                .ok_or_else(|| PopError::Cose("no device signer configured".to_owned()))?;
            let req = build_activate_request(
                &self.identity,
                signer.public_key_raw(),
                &challenge,
                self.config.claim_code.as_deref(),
            );
            // Serialise ONCE — the transport sends THESE bytes verbatim and the PoP
            // signs `sha256` of THESE bytes.
            let body = serde_json::to_vec(&req)
                .map_err(|e| HeartbeatError::Malformed(format!("activate body serialise: {e}")))?;
            let pop_header =
                self.build_activate_pop_header(&body, &challenge.instance_id, &req.nonce)?;
            // Mint the durable Idempotency-Key LAST (after the fallible PoP build),
            // then pin the whole unit for verbatim replay.
            let idempotency_key = self.idempotency_key(AttemptVerb::Activate)?;
            let attempt = PendingAttempt {
                verb: AttemptVerb::Activate,
                idempotency_key,
                body,
                pop_header,
            };
            self.pin_attempt(attempt.clone());
            attempt
        };

        // 3. POST the pinned attempt with the status-aware retry contract.
        let resp = match self
            .server
            .activate(
                &self.config.org_id,
                attempt.body.clone(),
                &attempt.idempotency_key,
                &attempt.pop_header,
            )
            .await
        {
            Ok(resp) => resp,
            Err(err @ (HeartbeatError::ServerRejected(_) | HeartbeatError::Malformed(_))) => {
                // A RECEIVED contact burned the single-use nonce — drop the pinned
                // attempt + nonce so the next cycle re-enrols with a fresh challenge.
                self.reset_on_rejection(AttemptVerb::Activate);
                return Err(err);
            }
            // Ambiguous (no response / 5xx → Transport): leave the attempt pinned to
            // replay verbatim next cycle (the server dedupes on the Idempotency-Key).
            Err(err) => return Err(err),
        };
        let (lease, _state, next_nonce) = (resp.lease, resp.enforcement_state, resp.next_nonce);
        // The contact succeeded → this logical operation is DONE. Clear/rotate the
        // attempt + remember the server's nextNonce (seeds the steady-state renew).
        self.clear_pending(AttemptVerb::Activate);
        self.remember_next_nonce(&next_nonce);

        // A null activate lease is a fail-closed keep-last-good (the device is not
        // enrolled this cycle but never crashed): nothing to install, no binding to
        // learn. Re-check on the no-binding cadence; a later cycle re-enrols.
        let now_ms = (self.now_ms)();
        let next_due = next_due_default(now_ms);
        let Some(server_lease) = lease else {
            return Ok(HeartbeatOutcome::NoBinding { next_due });
        };

        // 4. Verify the issued lease against FRESHLY RE-FETCHED trust, then install
        //    anchored to the server-issued binding from the SIGNED body. `install` is
        //    called with `None` (this is the FIRST binding — there is nothing to
        //    cross-check against yet); the body's own `instance_binding_id` becomes
        //    the learned anchor on a genuine install.
        let accept_now_ms = (self.now_ms)();
        let fresh_keys = self.server.fetch_keys().await?;
        let trusted = TrustedKeys::verify(&fresh_keys, &self.pinned, accept_now_ms)?;
        let body = verify_signed_lease_chain(&server_lease, &trusted)?;
        let serial = match self.install(&server_lease, &body, None)? {
            InstallOutcome::Installed { serial } => {
                self.remember_binding_id(&body.instance_binding_id);
                serial
            }
            InstallOutcome::StaleNoop { serial } => serial,
        };
        Ok(HeartbeatOutcome::Activated { serial, next_due })
    }

    /// Build the `Conspect-Device-PoP` header for an ACTIVATE request: the base64
    /// `COSE_Sign1` over the canonical pre-image with the `/activate` `htu` and the
    /// **server-assigned** `instance_id` bound in (ADR-I008). Mirrors
    /// [`build_pop_header`](Self::build_pop_header) but for the activate URL +
    /// instance id. Fail closed (a missing signer / COSE error is a
    /// [`HeartbeatError::Pop`] — no activate is sent without a proof).
    fn build_activate_pop_header(
        &self,
        body: &[u8],
        server_instance_id: &str,
        nonce_hex: &str,
    ) -> Result<String, HeartbeatError> {
        let signer = self
            .device_signer
            .as_ref()
            .ok_or_else(|| PopError::Cose("no device signer configured".to_owned()))?;
        let htu = format!(
            "{}/organisations/{}/activate",
            self.api_base_for_pop(),
            self.config.org_id
        );
        let iat = (self.now_ms)() / 1000; // epoch seconds (server checks ±60s)
        let header = pop_activate_header_value(
            signer.as_ref(),
            &htu,
            body,
            server_instance_id,
            nonce_hex,
            iat,
        )?;
        Ok(header)
    }

    /// One **device REBIND** lifecycle cycle (ADR-I009). Operator-invoked (NOT a
    /// `run_once`/`run_forever` branch — auto-rebind would silently spend the scarce
    /// 3-free-per-AEST-year budget). It reactivates the device's already-bound
    /// instance against refreshed hardware:
    ///
    /// 1. Pre-network key-trust fast-fail.
    /// 2. Build (or REPLAY) the pinned attempt as ONE retry unit — fetch a fresh
    ///    `/challenge` for the single-use nonce, build the [`RebindRequest`] over the
    ///    device's OWN binding/instance id (continuity), serialise ONCE, sign the PoP
    ///    over those bytes (binding the device's own `instance_id`), mint the durable
    ///    key LAST, pin.
    /// 3. `POST /rebind` with the status-aware retry: `ServerRejected`/`Malformed`
    ///    (a received contact that burned the nonce) drops the pinned attempt + nonce
    ///    and recovers; an ambiguous `Transport` (incl. a `409` per the lifecycle
    ///    contract) keeps it pinned to replay verbatim — so a re-invocation on this
    ///    SAME running client replays the SAME idempotency-key (no double charge).
    /// 4. On a positively-decoded [`RebindResponse`]: clear/rotate the attempt and seed
    ///    the steady-state nonce from `nextNonce`. The response carries only a serial
    ///    (no embedded signed lease), so NOTHING is installed here — the next renew
    ///    installs the refreshed lease via the unchanged chokepoint. Keeps last-good on
    ///    every failure (never off air, inv #1/#10).
    ///
    /// # Errors
    /// [`HeartbeatError`] on transport / trust / PoP failure — every one keeps
    /// last-good; nothing is installed or removed.
    pub async fn rebind_once(&self, licence_id: &str) -> Result<HeartbeatOutcome, HeartbeatError> {
        // The binding to rebind: the SAME `resolve_binding_id` chain as renew + deactivate
        // — genuinely-learned → store's installed lease → configured (ADR-I009). A device
        // whose binding was LEARNED from the server (or is in the store) rebinds under THAT
        // id; a stale configured id never short-circuits it, and `instance_id` is never a
        // bindingId fallback. Fail closed if none is known (nothing to rebind).
        let binding_id = self.resolve_binding_id().ok_or_else(|| {
            HeartbeatError::Pop(PopError::Cose(
                "no established binding to rebind".to_owned(),
            ))
        })?;

        // 1. Pre-network key-trust fast-fail.
        let keys = self.server.fetch_keys().await?;
        let now_ms = (self.now_ms)();
        let _ = TrustedKeys::verify(&keys, &self.pinned, now_ms)?;

        // 2. Build or REPLAY the in-flight REBIND attempt as one retry unit. Verb-keyed
        //    (ADR-I009): this slot is touched ONLY by rebind — the automatic renew loop
        //    can never consume or clear it, so an ambiguous rebind's pin PERSISTS until
        //    the OPERATOR's rebind retry replays it verbatim (no double charge).
        let attempt = if let Some(pinned) = self.pinned_attempt(AttemptVerb::Rebind) {
            pinned
        } else {
            let nonce = self.obtain_pop_nonce(&self.config.org_id).await?;
            let challenge = DeviceChallenge::new(nonce, 0, String::new());
            let req = build_rebind_request_for(&self.identity, licence_id, &binding_id, &challenge);
            let body = serde_json::to_vec(&req)
                .map_err(|e| HeartbeatError::Malformed(format!("rebind body serialise: {e}")))?;
            let pop_header = self.build_lifecycle_pop_header("rebind", &body, &req.nonce)?;
            let idempotency_key = self.idempotency_key(AttemptVerb::Rebind)?;
            let attempt = PendingAttempt {
                verb: AttemptVerb::Rebind,
                idempotency_key,
                body,
                pop_header,
            };
            self.pin_attempt(attempt.clone());
            attempt
        };

        // 3. POST the pinned attempt with the lifecycle status-aware retry contract.
        let resp = match self
            .server
            .rebind(
                &self.config.org_id,
                attempt.body.clone(),
                &attempt.idempotency_key,
                &attempt.pop_header,
            )
            .await
        {
            Ok(resp) => resp,
            Err(err @ (HeartbeatError::ServerRejected(_) | HeartbeatError::Malformed(_))) => {
                // A RECEIVED definitive contact burned the nonce — drop the REBIND pin +
                // recover. Verb-keyed: a pending renew/deactivate is UNTOUCHED.
                self.reset_on_rejection(AttemptVerb::Rebind);
                return Err(err);
            }
            // Ambiguous (Transport, incl. a 409 per the lifecycle contract): keep the
            // attempt pinned so a re-invocation on this client replays it verbatim
            // (the server dedups on the Idempotency-Key — no second rebind charge).
            Err(err) => return Err(err),
        };

        // 4. The contact succeeded → clear/rotate the REBIND pin + seed the steady-state
        //    nonce. The response carries only a serial, so NOTHING is installed here (the
        //    next renew installs the refreshed lease).
        self.clear_pending(AttemptVerb::Rebind);
        self.remember_next_nonce(&resp.next_nonce);
        Ok(HeartbeatOutcome::Rebound {
            rebound: resp.rebound,
            lease_serial: resp.lease_serial,
            rebinds_this_year: resp.rebinds_this_year,
            seat_consumed: resp.seat_consumed,
        })
    }

    /// One **device DEACTIVATE** lifecycle cycle (ADR-I009). Operator-invoked. It
    /// returns the binding's seat server-side (idempotent) and installs **nothing**
    /// locally — the last-good lease ages out via the offline ladder (never off air).
    /// Same four-stage shape as [`rebind_once`](Self::rebind_once) minus any install:
    /// fetch challenge → build [`DeactivateRequest`] (the device's own binding id +
    /// nonce) → sign the PoP (binding the device's own `instance_id`) → `POST
    /// /deactivate` with the lifecycle retry → on a `2xx` clear/rotate the attempt and
    /// return. It does NOT seed a renew nonce (there is nothing to renew after a
    /// surrender) and does NOT touch the store or engine.
    ///
    /// # Errors
    /// [`HeartbeatError`] on transport / trust / PoP failure — every one keeps
    /// last-good; nothing is installed or removed.
    pub async fn deactivate_once(&self) -> Result<HeartbeatOutcome, HeartbeatError> {
        // The binding to decommission: the SAME `resolve_binding_id` chain as renew +
        // rebind — genuinely-learned → store's installed lease → configured (ADR-I009);
        // a stale configured id never short-circuits a fresher learned/installed binding.
        // Fail closed if none is known (nothing to deactivate).
        let binding_id = self.resolve_binding_id().ok_or_else(|| {
            HeartbeatError::Pop(PopError::Cose(
                "no established binding to deactivate".to_owned(),
            ))
        })?;

        // 1. Pre-network key-trust fast-fail.
        let keys = self.server.fetch_keys().await?;
        let now_ms = (self.now_ms)();
        let _ = TrustedKeys::verify(&keys, &self.pinned, now_ms)?;

        // 2. Build or REPLAY the in-flight DEACTIVATE attempt as one retry unit.
        //    Verb-keyed (ADR-I009): touched ONLY by deactivate — the renew loop can
        //    never consume or clear it (no cross-verb replay, idempotent surrender).
        let attempt = if let Some(pinned) = self.pinned_attempt(AttemptVerb::Deactivate) {
            pinned
        } else {
            let nonce = self.obtain_pop_nonce(&self.config.org_id).await?;
            let req = build_deactivate_request(&binding_id, &nonce);
            let body = serde_json::to_vec(&req).map_err(|e| {
                HeartbeatError::Malformed(format!("deactivate body serialise: {e}"))
            })?;
            let pop_header = self.build_lifecycle_pop_header("deactivate", &body, &req.nonce)?;
            let idempotency_key = self.idempotency_key(AttemptVerb::Deactivate)?;
            let attempt = PendingAttempt {
                verb: AttemptVerb::Deactivate,
                idempotency_key,
                body,
                pop_header,
            };
            self.pin_attempt(attempt.clone());
            attempt
        };

        // 3. POST with the lifecycle status-aware retry contract.
        let resp = match self
            .server
            .deactivate(
                &self.config.org_id,
                attempt.body.clone(),
                &attempt.idempotency_key,
                &attempt.pop_header,
            )
            .await
        {
            Ok(resp) => resp,
            Err(err @ (HeartbeatError::ServerRejected(_) | HeartbeatError::Malformed(_))) => {
                // Verb-keyed: drops only the DEACTIVATE pin; renew/rebind are UNTOUCHED.
                self.reset_on_rejection(AttemptVerb::Deactivate);
                return Err(err);
            }
            Err(err) => return Err(err),
        };

        // 4. The contact succeeded → clear/rotate the DEACTIVATE pin. Drop the held
        //    nonce too (there is no renew after a surrender). Install NOTHING — the
        //    local lease ages out.
        self.clear_pending(AttemptVerb::Deactivate);
        self.remember_next_nonce(""); // clear: no steady-state renew after surrender
        Ok(HeartbeatOutcome::Deactivated {
            binding_id: resp.id,
            lifecycle_state: resp.lifecycle_state,
        })
    }

    /// Build the `Conspect-Device-PoP` header for a **rebind**/**deactivate** request
    /// (ADR-I009): the base64 `COSE_Sign1` over the canonical pre-image with the verb's
    /// `htu` and the device's **OWN** durable `instance_id` bound in (continuity).
    /// `verb` is `"rebind"` or `"deactivate"`. Fail closed (a missing signer / COSE
    /// error is a [`HeartbeatError::Pop`] — no lifecycle op is sent without a proof).
    fn build_lifecycle_pop_header(
        &self,
        verb: &str,
        body: &[u8],
        nonce_hex: &str,
    ) -> Result<String, HeartbeatError> {
        let signer = self
            .device_signer
            .as_ref()
            .ok_or_else(|| PopError::Cose("no device signer configured".to_owned()))?;
        let htu = format!(
            "{}/organisations/{}/{verb}",
            self.api_base_for_pop(),
            self.config.org_id
        );
        let iat = (self.now_ms)() / 1000; // epoch seconds (server checks ±60s)
        let header = pop_header_value(
            signer.as_ref(),
            "POST",
            &htu,
            body,
            &self.identity.instance_id,
            nonce_hex,
            iat,
        )?;
        Ok(header)
    }

    /// Translate a verified server lease into a [`LeaseBinding`] and drive
    /// [`LeaseStore::install_binding`]. The binding is signed with the crate's
    /// own pinned-key envelope so the single install convergence (shared by the
    /// file-drop and mesh-relay paths) re-verifies it uniformly; the
    /// authoritative Conspect signature was already checked in step 4.
    ///
    /// `established_binding` is this device's locally-anchored instance binding
    /// (configured, or learned from a prior install / the store's current lease).
    /// On the renew-only path this is **always** `Some` (a binding must exist for
    /// a renew to run); a returned body whose `instance_binding_id` differs is
    /// rejected as a cross-instance replay (a valid lease minted for another device
    /// must not install here). The `Option` is retained so the deferred activate
    /// slice — which establishes the first binding from the signed body — can pass
    /// `None` without re-shaping this method; the fingerprint-strong stamp is only
    /// applied to a binding-matched body.
    ///
    /// Returns an [`InstallOutcome`] distinguishing a GENUINE install from the
    /// benign stale no-op (the store already held a newer lease). The caller learns
    /// the binding id ONLY on a genuine install — never on the stale no-op (which
    /// installed nothing, so learning its binding would poison identity).
    ///
    /// # Errors
    /// [`HeartbeatError::BindingMismatch`] for a cross-instance lease;
    /// [`HeartbeatError::LeaseExpired`] for an expired/replayed signed lease;
    /// [`HeartbeatError::SignedLease`] if the local re-verification (which cannot
    /// fail for a body we just signed) rejects it.
    fn install(
        &self,
        server: &ServerLease,
        body: &LeaseBody,
        established_binding: Option<&str>,
    ) -> Result<InstallOutcome, HeartbeatError> {
        // CROSS-INSTANCE REPLAY DEFENCE: this device has an established binding, so
        // a returned lease MUST bind that same instance. A valid Conspect-signed
        // lease minted for another device's binding is refused here (and never
        // reaches the fingerprint-strong stamp below). The `None` arm exists for the
        // deferred activate slice (a first binding establishes from the body); on
        // the renew path `established_binding` is always `Some`.
        if let Some(local) = established_binding {
            if body.instance_binding_id != local {
                return Err(HeartbeatError::BindingMismatch);
            }
        }
        let granted_at = system_now();
        // The installed lease's expiry IS the cryptographically-signed `not_after`
        // (NOT system_now()+35d): a short-lived or replayed-old signed lease must
        // never become a fresh 35-day term. An already-expired signed lease is
        // rejected (keep last-good, never off air).
        let now_ms = (self.now_ms)();
        let lease = Lease::new_online_expiring_at(
            body.serial.clone(),
            body.not_after,
            now_ms,
            ACTIVATION_WINDOW_DAYS,
        )
        .ok_or(HeartbeatError::LeaseExpired)?;
        let gpu_limit = body
            .gpu_limit
            .map_or(GpuLimit::Unlimited, GpuLimit::Limited);
        let hardware = parse_hardware_class(body.hardware_class.as_deref());
        let entitlement = Entitlement::new(
            Tier::new(body.licence_id.clone()),
            hardware,
            hardware,
            gpu_limit,
            lease,
            EntitlementFlags::default(),
        );
        // Re-sign the install envelope with an ephemeral key the store verifies
        // against the matching pinned key. The Conspect signature is already
        // verified; this envelope is the crate's internal install contract. Stamp
        // the device's ACTUAL local fingerprint score (NOT an unconditional
        // STRONG): the store's fingerprint-continuity gate then does real work —
        // a machine whose salted fingerprint does not match (score below the
        // threshold) is rejected rather than silently installed.
        let (binding, install_pinned) = seal_for_install(
            &entitlement,
            self.identity.fingerprint_score,
            &body.instance_binding_id,
        );
        match self
            .store
            .install_binding(&binding, &install_pinned, granted_at)
        {
            Ok(installed) => {
                let _ = server;
                // The store recorded the device's instance binding id ATOMICALLY
                // inside `install_binding` (the single binding-anchor chokepoint
                // every install surface converges on), so the renew anchor reads it
                // back without a second write here. Recording happens ONLY on a
                // genuine install — the stale no-op below installs nothing.
                Ok(InstallOutcome::Installed {
                    serial: installed.serial,
                })
            }
            // A stale grant means the store already holds a NEWER lease — a benign
            // no-op, never off air. This is NOT an install: the caller must NOT
            // learn the binding from it (the Stale->Ok fold was the round-3 poison).
            Err(crate::store::InstallError::Stale { .. }) => Ok(InstallOutcome::StaleNoop {
                serial: body.serial.clone(),
            }),
            // The device's fingerprint did not clear the store's continuity gate
            // (the score we stamped is below threshold) — a real keep-last-good
            // outcome now that the score is the device's actual one, not a
            // hardcoded STRONG.
            Err(crate::store::InstallError::FingerprintMismatch { .. }) => {
                Err(HeartbeatError::FingerprintMismatch)
            }
            // The envelope signature cannot fail for a binding we just sealed with
            // the matching pinned key; surface as a verification error, not a panic.
            Err(crate::store::InstallError::SignatureInvalid) => {
                Err(HeartbeatError::SignedLease(SignedLeaseError::BadSignature))
            }
        }
    }

    /// Run the renew loop forever: sleep to the server-dictated `nextDue` (or, with
    /// no binding to renew yet, the no-binding re-check interval / backoff floor),
    /// run a cycle, repeat. Best-effort and cancellation-safe — abort it at any
    /// await point and the store's last-good lease is untouched.
    pub async fn run_forever(&self) {
        let mut backoff = self.config.min_interval;
        loop {
            match self.run_once().await {
                Ok(
                    HeartbeatOutcome::Installed { next_due, .. }
                    | HeartbeatOutcome::Activated { next_due, .. }
                    | HeartbeatOutcome::LeaseWithheld { next_due, .. }
                    | HeartbeatOutcome::NoBinding { next_due },
                ) => {
                    backoff = self.config.min_interval;
                    let sleep = sleep_until_due(next_due, self.config.min_interval);
                    tokio::time::sleep(sleep).await;
                }
                // The one-shot lifecycle outcomes (ADR-I009) are NEVER produced by
                // `run_once` — they only ever come from `rebind_once`/`deactivate_once`,
                // which the operator drives directly, not the renew loop. This arm is a
                // defensive no-op (re-check on the min interval) so the match stays
                // exhaustive without inventing a renew cadence for a one-shot result.
                Ok(HeartbeatOutcome::Rebound { .. } | HeartbeatOutcome::Deactivated { .. }) => {
                    backoff = self.config.min_interval;
                    tokio::time::sleep(self.config.min_interval).await;
                }
                Err(err) => {
                    tracing::info!(
                        %err,
                        "heartbeat cycle failed — keeping last-good lease, backing off (never off air)"
                    );
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(self.config.max_backoff);
                }
            }
        }
    }

    fn build_heartbeat_request_for(
        identity: &DeviceIdentity,
        held_serial: Option<String>,
        binding_id: Option<String>,
        nonce: String,
    ) -> HeartbeatRequest {
        HeartbeatRequest {
            binding_id: binding_id.unwrap_or_default(),
            lease_serial: held_serial,
            fingerprint_digest: identity.fingerprint_digest.clone(),
            app_version: identity.app_version.clone(),
            transport: Transport::Direct,
            nonce,
        }
    }
}

/// Decode a base64url (no-pad, tolerant of padding) string to bytes; `None` on
/// any non-base64url input (total, panic-free).
fn b64url(s: &str) -> Option<Vec<u8>> {
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(s.trim_end_matches('='))
        .ok()
}

/// Map a hardware-class string from the signed body onto the typed class. An
/// unknown/absent class defaults to `Standard` (the conservative, broadest tier).
fn parse_hardware_class(s: Option<&str>) -> HardwareClass {
    match s {
        Some("datacenter") => HardwareClass::Datacenter,
        Some("edge") => HardwareClass::Edge,
        _ => HardwareClass::Standard,
    }
}

/// "Now" in epoch milliseconds, from the host wall clock. Off the engine hot loop
/// (the heartbeat task is control-plane). A pre-epoch clock saturates to 0 (the
/// trust check then treats all keys as not-yet-valid — fail closed, never a
/// crash, and never an unwarranted trust).
fn unix_millis_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|d| i64::try_from(d.as_millis()).ok())
        .unwrap_or(0)
}

/// The default next-due (epoch ms) when no server-dictated `nextDue` applies — the
/// no-binding re-check interval (no lease to renew yet): `now + 30 days`.
fn next_due_default(now_ms: i64) -> i64 {
    now_ms.saturating_add(30 * 86_400_000)
}

/// How long to sleep until `next_due` (epoch ms), floored at `min` so a stale /
/// past due never spins.
fn sleep_until_due(next_due: i64, min: Duration) -> Duration {
    let now = unix_millis_now();
    let delta_ms = next_due.saturating_sub(now);
    if delta_ms <= 0 {
        return min;
    }
    let secs = u64::try_from(delta_ms / 1000).unwrap_or(0);
    Duration::from_secs(secs).max(min)
}

/// Seal a verified entitlement into the crate's own [`LeaseBinding`] +
/// [`PinnedKey`] install envelope. The Conspect signature is already verified;
/// this envelope is the internal contract the single [`LeaseStore::install_binding`]
/// convergence re-checks uniformly across all producers (file-drop, mesh relay,
/// heartbeat). The signing key is ephemeral and per-call — it never leaves this
/// function and authenticates nothing externally.
fn seal_for_install(
    entitlement: &Entitlement,
    fingerprint_score: u8,
    instance_binding_id: &str,
) -> (LeaseBinding, PinnedKey) {
    use ed25519_dalek::{Signer as _, SigningKey};
    // A deterministic per-process signer derived from nothing secret — its only
    // job is to satisfy the store's internal re-verification of a binding this
    // task itself produced (the authoritative external trust was checked above).
    // It is NOT an identity key and grants no authority.
    let envelope_signer = SigningKey::from_bytes(&INSTALL_ENVELOPE_SEED);
    let pinned = PinnedKey::from_verifying_key(&envelope_signer.verifying_key());
    // Sign over the lease BOUND to the binding id, so the store's re-verification
    // covers the anchor the binding carries (a grafted id would not verify).
    let msg = SignedLease::signing_bytes(&entitlement.lease, Some(instance_binding_id));
    let sig = envelope_signer.sign(&msg);
    let signed_lease = SignedLease::new(entitlement.lease.clone(), sig.to_bytes());
    // The device's ACTUAL local fingerprint score — NOT an unconditional STRONG.
    // The store's fingerprint-continuity gate then genuinely rejects a machine
    // whose salted fingerprint does not match (score below threshold). The
    // verified binding id rides along so `install_binding` records the device's
    // instance identity atomically (the single binding-anchor chokepoint).
    let binding = LeaseBinding::new(
        signed_lease,
        entitlement.clone(),
        fingerprint_score,
        Some(instance_binding_id.to_owned()),
    );
    (binding, pinned)
}

/// A fixed seed for the ephemeral install-envelope signer (see
/// [`seal_for_install`]). Not a secret and not an identity — it only closes the
/// store's internal binding re-verification for a binding this task produced.
const INSTALL_ENVELOPE_SEED: [u8; 32] = [0x6d; 32];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_key_cmp_orders_shortest_then_bytewise() {
        // Shorter encoded key sorts first (RFC 8949 §4.2.1): "serial" (6) before
        // "not_after" (9).
        assert_eq!(canonical_key_cmp("a", "bb"), std::cmp::Ordering::Less);
        assert_eq!(
            canonical_key_cmp("serial", "not_after"),
            std::cmp::Ordering::Less
        );
        assert_eq!(
            canonical_key_cmp("not_after", "serial"),
            std::cmp::Ordering::Greater
        );
        // Equal length → bytewise: "licence_id" vs "not_after_x" differ in length
        // so length wins; same-length keys fall back to bytewise order.
        assert_eq!(canonical_key_cmp("aaa", "aab"), std::cmp::Ordering::Less);
        assert_eq!(
            canonical_key_cmp("gpu_limit", "gpu_limit"),
            std::cmp::Ordering::Equal
        );
    }

    #[test]
    fn cbor_head_uses_shortest_encoding() {
        let mut a = Vec::new();
        cbor_head(&mut a, 0, 10);
        assert_eq!(a, vec![0x0a]);
        let mut b = Vec::new();
        cbor_head(&mut b, 0, 1_000_000);
        assert_eq!(b[0], 0x1a); // 4-byte argument
    }

    #[test]
    fn b64url_round_trips_and_rejects_garbage() {
        assert!(b64url("aGVsbG8").is_some());
        assert!(b64url("not base64!").is_none());
    }

    #[test]
    fn transport_serialises_to_exactly_the_three_wire_values() {
        // The transport is a CLOSED enum: it can only ever serialise to one of the
        // three Conspect-accepted channel labels, so an out-of-enum value can never
        // be put on the wire (a future 422 is structurally impossible).
        assert_eq!(
            serde_json::to_string(&Transport::Direct).expect("serialise"),
            "\"direct\""
        );
        assert_eq!(
            serde_json::to_string(&Transport::Relay).expect("serialise"),
            "\"relay\""
        );
        assert_eq!(
            serde_json::to_string(&Transport::File).expect("serialise"),
            "\"file\""
        );
    }

    #[test]
    fn transport_default_is_direct() {
        // The phone-home channel: a direct device→server heartbeat.
        assert_eq!(Transport::default(), Transport::Direct);
    }

    #[test]
    fn an_unknown_transport_label_does_not_deserialise() {
        // A value outside the closed set is rejected — the enum is the full,
        // exhaustive wire vocabulary, never an open string.
        assert!(serde_json::from_str::<Transport>("\"webrtc\"").is_err());
        assert!(serde_json::from_str::<Transport>("\"direct\"").is_ok());
    }
}
