//! Shared test scaffolding for the CONSPECT-3 heartbeat tests: a fabricated,
//! root-attested keyset (whose root + intermediate private keys we control, so
//! we can mint a valid signed lease and exercise the revocation contract) and an
//! in-process [`FakeLicenceServer`] that returns locally-signed leases.
//!
//! This is `tests/fake/mod.rs` (a submodule, not a top-level `tests/*.rs`) so it
//! is shared by the integration tests without being compiled as its own test
//! binary.
// A shared test-helper module included by multiple integration-test crates; not
// every crate exercises every helper, so `dead_code` / `unreachable_pub` are
// expected and silenced here (test-only scaffolding, never product code).
#![allow(
    dead_code,
    unreachable_pub,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::as_conversions,
    clippy::cast_possible_wrap,
    clippy::cast_possible_truncation,
    clippy::doc_markdown,
    clippy::needless_pass_by_value,
    clippy::unused_self,
    clippy::missing_panics_doc
)]

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use base64::Engine as _;
use ed25519_dalek::{Signer as _, SigningKey as EdKey};
// ed25519-dalek 3.x uses `signature` 3.0 while `p256` 0.13 still uses `signature`
// 2.2.0, so their two `Signer` traits are DISTINCT types: the ed25519 `Signer as _`
// above no longer covers a p256 key. Import p256's own re-exported `signature::Signer`
// (2.2.0) for `SigningKey::sign` below. Both are `use … as _`, and each `.sign()`
// resolves by its receiver type (ed25519 vs p256 key), so there is no ambiguity.
// `Signature` is `P256Sig` (ADR-I010).
use p256::ecdsa::signature::Signer as _;
use p256::ecdsa::{Signature as P256Sig, SigningKey as P256Key};

use ed25519_dalek::{Verifier as _, VerifyingKey as EdVerifyingKey};
use multiview_licence::heartbeat::{
    canonical_key_preimage, canonical_revocation_preimage, ActivateRequest, ActivateResponse,
    DeactivateRequest, DeactivateResponse, DeviceChallenge, DeviceSigner, EnforcementState,
    HeartbeatError, HeartbeatRequest, HeartbeatResponse, LeaseBodyFields, LicenceServer,
    LicensingKeys, PinnedRoot, RebindRequest, RebindResponse, ServerLease, TrustedKeys,
};

/// The fixed device-key seed the PoP loop tests sign with, so the fake can verify
/// the proof against the matching public key.
pub const POP_DEVICE_SEED: [u8; 32] = [0x5a; 32];

/// A deterministic device signer over [`POP_DEVICE_SEED`] for the PoP loop tests.
pub struct PopTestSigner {
    key: EdKey,
}

/// Build the shared PoP test signer (the client signs with this; the fake verifies
/// the proof against its public key).
pub fn pop_test_signer() -> PopTestSigner {
    PopTestSigner {
        key: EdKey::from_bytes(&POP_DEVICE_SEED),
    }
}

impl DeviceSigner for PopTestSigner {
    fn public_key_raw(&self) -> [u8; 32] {
        self.key.verifying_key().to_bytes()
    }
    fn sign(&self, message: &[u8]) -> [u8; 64] {
        self.key.sign(message).to_bytes()
    }
}

/// The live production root verifying key (ECDSA P-256, base64url uncompressed),
/// re-exported for the key-trust tests.
pub const ROOT_PUB_B64URL: &str =
    "BN4f6BIHOFZmFqXp9YM1U65bJTGpOOob1I9X8C_FpfJOWanTCs4Z3c-l8C1wqH4g8Rl01VNkQNC78XixViLiwRY";

/// Decode a base64url (no-pad) string, the well-known encoding for key material.
pub fn b64url(s: &str) -> Vec<u8> {
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(s.trim_end_matches('='))
        .unwrap()
}

const FAB_KID: &str = "intermediate-fab";
const FAB_LICENCE: &str = "lic_fab_0001";
const FAB_BINDING: &str = "ib_fab_0001";
const FAB_NOW_MS: i64 = 1_790_000_000_000;
const FAB_VALID_FROM: i64 = 1_781_510_167_548;
const FAB_VALID_UNTIL: i64 = 1_850_630_167_548;
const FAB_ISSUED_AT: i64 = 1_781_570_867_301;

/// A fabricated keyset rooted at a P-256 key we control.
pub struct FabricatedKeyset {
    root: P256Key,
    intermediate: EdKey,
}

impl FabricatedKeyset {
    pub fn new() -> Self {
        Self {
            root: P256Key::from_bytes(&[7u8; 32].into()).expect("test root key"),
            intermediate: EdKey::from_bytes(&[9u8; 32]),
        }
    }

    pub fn now_ms(&self) -> i64 {
        FAB_NOW_MS
    }
    /// The signer's signed `valid_until` (epoch ms) — the dual-pin intermediate's
    /// validity-window end. The TOCTOU test advances the clock just past this.
    pub fn valid_until(&self) -> i64 {
        FAB_VALID_UNTIL
    }
    pub fn kid(&self) -> &'static str {
        FAB_KID
    }
    pub fn licence_id(&self) -> &'static str {
        FAB_LICENCE
    }

    pub fn pinned_root(&self) -> PinnedRoot {
        let vk = p256::ecdsa::VerifyingKey::from(&self.root);
        PinnedRoot::from_sec1_bytes(vk.to_encoded_point(false).as_bytes()).expect("fab root parse")
    }

    fn root_sign(&self, msg: &[u8]) -> String {
        let sig: P256Sig = self.root.sign(msg);
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(sig.to_bytes())
    }

    fn intermediate_pub_b64url(&self) -> String {
        base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(self.intermediate.verifying_key().to_bytes())
    }

    pub fn keys(&self) -> LicensingKeys {
        self.keys_with_revocation(&[])
    }

    /// Build a well-formed, root-attested well-known doc with `revoked` listed in
    /// the (root-signed) revocation set.
    pub fn keys_with_revocation(&self, revoked: &[&str]) -> LicensingKeys {
        self.keys_full(revoked, "lease", "current", FAB_VALID_FROM, FAB_VALID_UNTIL)
    }

    /// Build a root-attested well-known doc whose sole `lease_keys` entry carries
    /// the given `key_type` + `status` — its `root_sig` is computed over the
    /// pre-image that INCLUDES that `key_type`, so it is genuinely root-attested
    /// (the attack the verifier must still reject: a root-attested NON-lease key
    /// presented as a lease signer). `status` is the UNSIGNED operational hint.
    pub fn keys_with_signer(&self, key_type: &str, status: &str) -> LicensingKeys {
        self.keys_full(&[], key_type, status, FAB_VALID_FROM, FAB_VALID_UNTIL)
    }

    /// Like [`keys_with_signer`] but with an explicit (signed) validity window —
    /// to prove that forging the UNSIGNED `status` to "current" does NOT rescue a
    /// key whose SIGNED validity window excludes `now`.
    pub fn keys_with_signer_validity(
        &self,
        key_type: &str,
        status: &str,
        valid_from: i64,
        valid_until: i64,
    ) -> LicensingKeys {
        self.keys_full(&[], key_type, status, valid_from, valid_until)
    }

    /// A keyset whose sole lease key has the given `status` but is named in the
    /// (root-signed) revocation list — to prove the SIGNED revocation list still
    /// drops it even when `status` is forged to "current".
    pub fn keys_with_signer_revoked(&self, status: &str) -> LicensingKeys {
        self.keys_full(&[FAB_KID], "lease", status, FAB_VALID_FROM, FAB_VALID_UNTIL)
    }

    /// Build a keyset whose intermediate carries a NEGATIVE `valid_from`. The
    /// root_sig is computed over the pre-image with that negative value, so the
    /// entry is structurally root-attested — the rejection must come from the
    /// negative-timestamp gate, not a signature mismatch.
    pub fn keys_with_negative_valid_from(&self) -> LicensingKeys {
        let pubkey = self.intermediate.verifying_key().to_bytes().to_vec();
        let pre = canonical_key_preimage(
            FAB_KID,
            "lease",
            "conspect.key-attestation.v1",
            &pubkey,
            -1,
            FAB_VALID_UNTIL,
        );
        let root_sig = self.root_sign(&pre);
        let rev_pre =
            canonical_revocation_preimage(FAB_ISSUED_AT, "conspect.key-revocation.v1", &[]);
        let rev_sig = self.root_sign(&rev_pre);
        let root_pub = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(
            p256::ecdsa::VerifyingKey::from(&self.root)
                .to_encoded_point(false)
                .as_bytes(),
        );
        let json = serde_json::json!({
            "version": 1,
            "root": { "kid": "root", "algorithm": "ecdsa-p256-sha256", "public_key": root_pub,
                "public_key_encoding": "base64url-uncompressed-p256-point" },
            "attestation_contract": {
                "key_statement": "conspect.key-attestation.v1",
                "revocation_statement": "conspect.key-revocation.v1",
                "encoding": "deterministic-cbor-rfc8949-section-4.2.1",
                "key_pre_image": ["key_id","key_type","statement","public_key","valid_from","valid_until"],
                "revocation_pre_image": ["issued_at","statement","revoked_key_ids"],
                "field_order": "canonical", "signature": "ecdsa-p256-sha256-raw-r-s-base64url",
                "public_key_encoding": "raw-32-byte-ed25519-point", "time_unit": "epoch-milliseconds" },
            "lease_keys": [{ "kid": FAB_KID, "key_type": "lease", "algorithm": "ed25519",
                "public_key": self.intermediate_pub_b64url(), "valid_from": -1,
                "valid_until": FAB_VALID_UNTIL, "status": "current", "root_sig": root_sig }],
            "update_keys": [],
            "revocation": { "statement": "conspect.key-revocation.v1", "issued_at": FAB_ISSUED_AT,
                "revoked_key_ids": [], "root_revocation_sig": rev_sig }
        });
        serde_json::from_value(json).expect("fabricated keyset must parse")
    }

    fn keys_full(
        &self,
        revoked: &[&str],
        key_type: &str,
        status: &str,
        valid_from: i64,
        valid_until: i64,
    ) -> LicensingKeys {
        let pubkey = self.intermediate.verifying_key().to_bytes().to_vec();
        // The root_sig covers the pre-image that includes key_type + the validity
        // window, so the entry is genuinely root-attested for THIS key_type and
        // window (mirrors how Conspect signs every intermediate). `status` is NOT
        // in the pre-image — it is the unsigned, forgeable operational hint.
        let pre = canonical_key_preimage(
            FAB_KID,
            key_type,
            "conspect.key-attestation.v1",
            &pubkey,
            valid_from,
            valid_until,
        );
        let root_sig = self.root_sign(&pre);
        let revoked_owned: Vec<String> = revoked.iter().map(|s| (*s).to_owned()).collect();
        let rev_pre = canonical_revocation_preimage(
            FAB_ISSUED_AT,
            "conspect.key-revocation.v1",
            &revoked_owned,
        );
        let rev_sig = self.root_sign(&rev_pre);
        let root_pub = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(
            p256::ecdsa::VerifyingKey::from(&self.root)
                .to_encoded_point(false)
                .as_bytes(),
        );

        let json = serde_json::json!({
            "version": 1,
            "root": {
                "kid": "root",
                "algorithm": "ecdsa-p256-sha256",
                "public_key": root_pub,
                "public_key_encoding": "base64url-uncompressed-p256-point"
            },
            "attestation_contract": {
                "key_statement": "conspect.key-attestation.v1",
                "revocation_statement": "conspect.key-revocation.v1",
                "encoding": "deterministic-cbor-rfc8949-section-4.2.1",
                "key_pre_image": ["key_id","key_type","statement","public_key","valid_from","valid_until"],
                "revocation_pre_image": ["issued_at","statement","revoked_key_ids"],
                "field_order": "canonical",
                "signature": "ecdsa-p256-sha256-raw-r-s-base64url",
                "public_key_encoding": "raw-32-byte-ed25519-point",
                "time_unit": "epoch-milliseconds"
            },
            "lease_keys": [{
                "kid": FAB_KID,
                "key_type": key_type,
                "algorithm": "ed25519",
                "public_key": self.intermediate_pub_b64url(),
                "valid_from": valid_from,
                "valid_until": valid_until,
                "status": status,
                "root_sig": root_sig
            }],
            "update_keys": [],
            "revocation": {
                "statement": "conspect.key-revocation.v1",
                "issued_at": FAB_ISSUED_AT,
                "revoked_key_ids": revoked_owned,
                "root_revocation_sig": rev_sig
            }
        });
        serde_json::from_value(json).expect("fabricated keyset must parse")
    }

    pub fn trusted(&self) -> TrustedKeys {
        TrustedKeys::verify(&self.keys(), &self.pinned_root(), self.now_ms())
            .expect("the fabricated keyset must verify against its own root")
    }

    /// The default fabricated lease serial (a UUIDv7).
    pub fn lease_serial(&self) -> &'static str {
        "01931d4e-7b2a-7c00-9f3a-2b6e1c0e9b41"
    }
    pub fn binding_id(&self) -> &'static str {
        FAB_BINDING
    }

    /// Mint a signed lease (bare Ed25519 over a canonical-CBOR body), `term_days`
    /// out from `now_ms`.
    pub fn sign_lease(&self, term_days: i64) -> ServerLease {
        self.sign_lease_expiring_at(self.now_ms() + term_days * 86_400_000)
    }

    /// Mint a signed lease whose signed `not_after` is exactly `not_after_ms` —
    /// the cryptographically-signed expiry the installer MUST honour (so a past
    /// `not_after` exercises the expiry/replay rejection).
    pub fn sign_lease_expiring_at(&self, not_after_ms: i64) -> ServerLease {
        self.sign_lease_for(FAB_BINDING, not_after_ms)
    }

    /// A foreign instance binding id — a DIFFERENT device's binding, for the
    /// cross-instance replay test.
    pub fn foreign_binding_id(&self) -> &'static str {
        "ib_other_device_9999"
    }

    /// Mint a correctly-signed lease whose body binds `binding_id` and expires at
    /// `not_after_ms`. Lets a test mint a lease for ANOTHER device (foreign
    /// binding) and/or with an absolute-past expiry — both still cryptographically
    /// valid, so the rejection must come from the install-time identity/expiry
    /// gates, not the signature.
    pub fn sign_lease_for(&self, binding_id: &str, not_after_ms: i64) -> ServerLease {
        self.sign_body(LeaseBodyFields {
            licence_id: FAB_LICENCE.to_owned(),
            instance_binding_id: binding_id.to_owned(),
            serial: self.lease_serial().to_owned(),
            not_after: not_after_ms,
            gpu_limit: Some(2),
            hardware_class: Some("standard".to_owned()),
        })
    }

    /// Mint a signed lease with a DISTINCT (older) serial expiring at
    /// `not_after_ms` — the replay-attack lease (a different serial than the
    /// current one, so installing it is not a serial-collision no-op).
    pub fn sign_old_lease(&self, not_after_ms: i64) -> ServerLease {
        self.sign_body(LeaseBodyFields {
            licence_id: FAB_LICENCE.to_owned(),
            instance_binding_id: FAB_BINDING.to_owned(),
            serial: "01930000-0000-7000-8000-000000000001".to_owned(),
            not_after: not_after_ms,
            gpu_limit: Some(2),
            hardware_class: Some("standard".to_owned()),
        })
    }

    /// Sign an arbitrary lease body — exactly the bytes the verifier re-checks.
    /// The signed `not_after` mirror field on the envelope matches the body.
    pub fn sign_body(&self, body: LeaseBodyFields) -> ServerLease {
        let lease_bytes = body.to_canonical_cbor();
        let sig = self.intermediate.sign(&lease_bytes);
        ServerLease {
            serial: body.serial.clone(),
            licence_id: Some(body.licence_id.clone()),
            instance_binding_id: Some(body.instance_binding_id.clone()),
            not_after: body.not_after,
            signature: hex::encode(sig.to_bytes()),
            signer_key_id: FAB_KID.to_owned(),
            lease_bytes: base64::engine::general_purpose::STANDARD.encode(&lease_bytes),
        }
    }

    /// Sign a canonical-CBOR lease body that genuinely OMITS `omit` (a required
    /// field) — to prove the verifier fails closed on a missing field rather than
    /// defaulting it to empty. Builds the map by hand (canonical key order).
    ///
    /// The `licence_id` value is padded so the total CBOR length is a multiple of
    /// 3 (its standard-base64 has NO '=' padding) — this ISOLATES the missing-field
    /// rejection from the base64-padding decode path, so the test proves blocker #4
    /// independent of blocker #3.
    pub fn sign_body_omitting(&self, omit: &str) -> ServerLease {
        let not_after = self.now_ms() + 35 * 86_400_000;
        // Search a licence_id length that makes the encoded body len % 3 == 0.
        for pad in 0..3 {
            let licence_id = format!("{FAB_LICENCE}{}", "x".repeat(pad));
            let mut pairs: Vec<(&str, CborScalar)> = vec![
                (
                    "instance_binding_id",
                    CborScalar::Text(FAB_BINDING.to_owned()),
                ),
                ("licence_id", CborScalar::Text(licence_id.clone())),
                ("not_after", CborScalar::Uint(not_after)),
                ("serial", CborScalar::Text(self.lease_serial().to_owned())),
            ];
            pairs.retain(|(k, _)| *k != omit);
            pairs.sort_by(|a, b| {
                a.0.len()
                    .cmp(&b.0.len())
                    .then(a.0.as_bytes().cmp(b.0.as_bytes()))
            });
            let mut bytes = Vec::new();
            cbor_map_head(&mut bytes, pairs.len());
            for (k, v) in &pairs {
                cbor_text(&mut bytes, k);
                match v {
                    CborScalar::Text(s) => cbor_text(&mut bytes, s),
                    CborScalar::Uint(n) => cbor_uint(&mut bytes, *n),
                }
            }
            if bytes.len() % 3 != 0 {
                continue; // would base64-pad; try a different licence_id length
            }
            let sig = self.intermediate.sign(&bytes);
            let encoded = base64::engine::general_purpose::STANDARD.encode(&bytes);
            assert!(!encoded.ends_with('='), "chosen body must be unpadded");
            return ServerLease {
                serial: self.lease_serial().to_owned(),
                licence_id: Some(licence_id),
                instance_binding_id: Some(FAB_BINDING.to_owned()),
                not_after,
                signature: hex::encode(sig.to_bytes()),
                signer_key_id: FAB_KID.to_owned(),
                lease_bytes: encoded,
            };
        }
        panic!("could not build an unpadded omitting body for {omit}");
    }

    /// Sign a body whose `gpu_limit` is the RAW signed integer `raw` (which may be
    /// negative, or larger than `u32::MAX`) — so the verifier's gpu_limit handling
    /// is exercised with a present-but-out-of-range value. All required fields are
    /// present + valid; only gpu_limit is hostile. The CBOR integer is emitted
    /// canonically (major 1 for negatives, the 8-byte form for oversized).
    pub fn sign_body_with_raw_gpu_limit(&self, raw: i64) -> ServerLease {
        let not_after = self.now_ms() + 35 * 86_400_000;
        // Canonical key order: gpu_limit, instance_binding_id, licence_id,
        // not_after, serial (by length then bytewise).
        let mut bytes = Vec::new();
        cbor_map_head(&mut bytes, 5);
        cbor_text(&mut bytes, "gpu_limit");
        cbor_int(&mut bytes, raw);
        cbor_text(&mut bytes, "instance_binding_id");
        cbor_text(&mut bytes, FAB_BINDING);
        cbor_text(&mut bytes, "licence_id");
        cbor_text(&mut bytes, FAB_LICENCE);
        cbor_text(&mut bytes, "not_after");
        cbor_uint(&mut bytes, not_after);
        cbor_text(&mut bytes, "serial");
        cbor_text(&mut bytes, self.lease_serial());
        let sig = self.intermediate.sign(&bytes);
        ServerLease {
            serial: self.lease_serial().to_owned(),
            licence_id: Some(FAB_LICENCE.to_owned()),
            instance_binding_id: Some(FAB_BINDING.to_owned()),
            not_after,
            signature: hex::encode(sig.to_bytes()),
            signer_key_id: FAB_KID.to_owned(),
            lease_bytes: base64::engine::general_purpose::STANDARD.encode(&bytes),
        }
    }
}

/// A CBOR scalar for the hand-built omitting encoder.
enum CborScalar {
    Text(String),
    Uint(i64),
}

/// Encode a (possibly negative) CBOR integer canonically: major 0 for `n >= 0`,
/// major 1 (value `-1 - n`) for negatives.
fn cbor_int(out: &mut Vec<u8>, n: i64) {
    if n >= 0 {
        cbor_head(out, 0, n.unsigned_abs());
    } else {
        cbor_head(out, 1, (-1 - n).unsigned_abs());
    }
}

/// Minimal CBOR helpers for `sign_body_omitting` (RFC 8949 §4.2.1 shortest form).
fn cbor_map_head(out: &mut Vec<u8>, n: usize) {
    cbor_head(out, 5, n as u64);
}
fn cbor_text(out: &mut Vec<u8>, s: &str) {
    cbor_head(out, 3, s.len() as u64);
    out.extend_from_slice(s.as_bytes());
}
fn cbor_uint(out: &mut Vec<u8>, n: i64) {
    cbor_head(out, 0, u64::try_from(n).unwrap_or(0));
}
fn cbor_head(out: &mut Vec<u8>, major: u8, n: u64) {
    let mt = major << 5;
    if n < 24 {
        out.push(mt | (n as u8));
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

/// An in-process [`LicenceServer`]: hands out the fabricated keyset and a
/// locally-signed lease, with knobs to drive the never-off-air cases (return
/// `lease: null` like a revoked entitlement; stall; or fail every call).
pub struct FakeLicenceServer {
    kit: FabricatedKeyset,
    /// When true, `heartbeat` returns `lease: null` + a `revoked` state (the
    /// revocation-by-non-reissue case): the client must keep last-good, never
    /// tighten on its own.
    withhold_lease: AtomicBool,
    /// When true, every call returns a transport error (server unreachable).
    fail: AtomicBool,
    /// When true, every call **blocks in-flight forever** (awaits `unblock`) — a
    /// real stalled transport (black hole), NOT an instant error. Used to prove a
    /// stalled heartbeat call cannot block a concurrent store reader / the output
    /// clock (invariant #10).
    block: AtomicBool,
    /// Notified to release any in-flight blocked call (so the test can tear down).
    unblock: tokio::sync::Notify,
    /// Set true once a blocked call is actually in flight (the reader waits for
    /// this so it observes a genuinely concurrent stall, not a race).
    in_flight: AtomicBool,
    /// When true, `heartbeat` replays an OLDER, already-expired (but still
    /// Ed25519-valid) signed lease — the replay-attack case: the installer must
    /// not re-extend entitlement from the replay instant.
    replay_expired: AtomicBool,
    /// When true, the returned (correctly-signed) lease binds a DIFFERENT device's
    /// instance binding id — the cross-instance replay case.
    foreign_binding: AtomicBool,
    /// When true, the returned (correctly-signed) lease's `not_after` is an
    /// absolute instant in 1970 — clearly past ANY real clock, so install() takes
    /// the `LeaseExpired` path deterministically.
    replay_absolute_past: AtomicBool,
    /// When true, the returned (correctly-signed) lease has an OLDER `granted_at`
    /// (a smaller, but still-future, `not_after`) than a normal lease — so the
    /// store rejects it as `Stale`. Still future, so it clears the expiry gate and
    /// reaches the store's staleness check (the Stale->Ok fold path).
    replay_stale: AtomicBool,
    /// The `bindingId` the most recent heartbeat request carried (so a test can
    /// assert the renewal addresses the binding by id, never the lease serial).
    last_binding_id: std::sync::Mutex<Option<String>>,
    /// When true, the signer is unrevoked on the FIRST `fetch_keys` but REVOKED on
    /// every later fetch — the revocation-TOCTOU case: a revocation published
    /// between the initial fetch and the acceptance re-fetch must reject the lease.
    revoke_signer_after_first_fetch: AtomicBool,
    /// Counts `fetch_keys` calls (the revocation-TOCTOU test asserts a re-fetch).
    key_fetches: AtomicU64,
    /// Every `Idempotency-Key` the server has received, in order (the idempotency
    /// test asserts retries of the same logical op reuse one key).
    idempotency_keys: std::sync::Mutex<Vec<String>>,
    /// Every heartbeat request BODY the server received, in order — so the
    /// retry-coupling test can assert a lost-response retry replays the SAME body
    /// bytes (same PoP nonce), not a freshly-nonce'd body under the same key.
    bodies: std::sync::Mutex<Vec<Vec<u8>>>,
    /// When > 0, the next N heartbeat calls record their Idempotency-Key and then
    /// return a transport error (a lost-response analogue) — decremented per call.
    fail_after_recording: AtomicU64,
    /// Counts heartbeat calls (the loop test asserts it advances).
    pub heartbeats: AtomicU64,
    /// The `nextDue` (epoch ms) the server returns — the loop sleeps to it.
    next_due_ms: AtomicU64,
    // --- device-PoP (ADR-I007) ---
    /// Counts `fetch_challenge` calls (the loop test asserts cold-start fetches one
    /// and steady-state fetches none).
    challenge_fetches: AtomicU64,
    /// A monotonic counter to mint distinct challenge nonces.
    nonce_counter: AtomicU64,
    /// The most recent challenge nonce this server ISSUED (via `fetch_challenge`).
    last_issued_nonce: std::sync::Mutex<Option<String>>,
    /// The `nonce` field the most recent heartbeat REQUEST body carried.
    last_request_nonce: std::sync::Mutex<Option<String>>,
    /// The `nextNonce` the most recent heartbeat RESPONSE returned.
    last_next_nonce: std::sync::Mutex<Option<String>>,
    /// Whether the most recent heartbeat's `Conspect-Device-PoP` proof VERIFIED
    /// against the device public key over the recomputed pre-image.
    last_pop_verified: AtomicBool,
    /// When true, `fetch_challenge` returns a transport error (cold start can't get
    /// a nonce → the client fails closed and keeps last-good).
    fail_challenge: AtomicBool,
    /// When true, `fetch_challenge` returns a challenge whose `expiresAtMs` is
    /// already in the PAST — the client must NOT sign/send an expired fresh nonce.
    challenge_expired: AtomicBool,
    /// When true, the next heartbeat response carries an EMPTY `nextNonce` (the
    /// client must cold-start a fresh `/challenge` next cycle, never reuse a nonce).
    drop_next_nonce: AtomicBool,
    /// When true, `heartbeat` returns a `pop-invalid`-style transport error (401)
    /// regardless of the proof — the server-rejected-nonce case.
    reject_pop: AtomicBool,
    /// When true, `heartbeat` returns a `Malformed` error AFTER verifying the proof —
    /// modelling a 2xx response whose body would not parse (`post_raw_json` emits
    /// `Malformed` only after a 2xx). The server has SEEN + processed the request (the
    /// nonce is burned), so the client must drop the pinned attempt and recover with a
    /// fresh `/challenge`, never replay the burned nonce.
    malformed_2xx: AtomicBool,
    // --- device ACTIVATE / enrolment (ADR-I008) ---
    /// Counts `activate` calls (the enrolment loop test asserts exactly one).
    activates: AtomicU64,
    /// The server-assigned durable instance id (`ib_<uuidv7>`) the most recent
    /// `fetch_challenge` issued — a first-contact device must echo it on activate.
    last_issued_instance_id: std::sync::Mutex<Option<String>>,
    /// The `instanceId` the most recent activate REQUEST carried (the test asserts
    /// it equals the server-assigned id).
    last_activate_instance_id: std::sync::Mutex<Option<String>>,
    /// The `devicePublicKey` the most recent activate request carried.
    last_activate_device_public_key: std::sync::Mutex<Option<String>>,
    /// Whether the most recent activate's `Conspect-Device-PoP` proof VERIFIED
    /// against the device public key over the recomputed pre-image — and bound the
    /// SERVER-assigned instance id.
    last_activate_pop_verified: AtomicBool,
    /// The instance binding id a successful `activate` bound the lease to — so the
    /// subsequent renew (`heartbeat`) serves a lease bound to the SAME id (otherwise
    /// the renew would `BindingMismatch` against the activate-learned binding).
    activated_binding: std::sync::Mutex<Option<String>>,
    // --- device REBIND + DEACTIVATE lifecycle (ADR-I009) ---
    /// Counts `rebind` calls (the lifecycle loop test asserts exactly one).
    rebinds: AtomicU64,
    /// Counts `deactivate` calls.
    deactivates: AtomicU64,
    /// The `instanceId` the most recent rebind REQUEST carried (the test asserts it is
    /// the device's OWN id — continuity, not a server-assigned challenge id).
    last_rebind_instance_id: std::sync::Mutex<Option<String>>,
    /// The `bindingId` the most recent rebind REQUEST carried (the test asserts it is
    /// the device's resolved binding — store-learned id, not the instance_id fallback).
    last_rebind_binding_id: std::sync::Mutex<Option<String>>,
    /// Whether the most recent rebind's `Conspect-Device-PoP` proof VERIFIED against the
    /// device key over the recomputed pre-image (binding the device's own instance id).
    last_rebind_pop_verified: AtomicBool,
    /// Whether the most recent deactivate's `Conspect-Device-PoP` proof VERIFIED.
    last_deactivate_pop_verified: AtomicBool,
    /// The `bindingId` the most recent deactivate request carried.
    last_deactivate_binding_id: std::sync::Mutex<Option<String>>,
}

impl FakeLicenceServer {
    pub fn new() -> Self {
        Self {
            kit: FabricatedKeyset::new(),
            withhold_lease: AtomicBool::new(false),
            fail: AtomicBool::new(false),
            block: AtomicBool::new(false),
            unblock: tokio::sync::Notify::new(),
            in_flight: AtomicBool::new(false),
            replay_expired: AtomicBool::new(false),
            foreign_binding: AtomicBool::new(false),
            replay_absolute_past: AtomicBool::new(false),
            replay_stale: AtomicBool::new(false),
            last_binding_id: std::sync::Mutex::new(None),
            revoke_signer_after_first_fetch: AtomicBool::new(false),
            key_fetches: AtomicU64::new(0),
            idempotency_keys: std::sync::Mutex::new(Vec::new()),
            bodies: std::sync::Mutex::new(Vec::new()),
            fail_after_recording: AtomicU64::new(0),
            heartbeats: AtomicU64::new(0),
            next_due_ms: AtomicU64::new(FAB_NOW_MS as u64 + 30 * 86_400_000),
            challenge_fetches: AtomicU64::new(0),
            challenge_expired: AtomicBool::new(false),
            nonce_counter: AtomicU64::new(0),
            last_issued_nonce: std::sync::Mutex::new(None),
            last_request_nonce: std::sync::Mutex::new(None),
            last_next_nonce: std::sync::Mutex::new(None),
            last_pop_verified: AtomicBool::new(false),
            fail_challenge: AtomicBool::new(false),
            drop_next_nonce: AtomicBool::new(false),
            reject_pop: AtomicBool::new(false),
            malformed_2xx: AtomicBool::new(false),
            activates: AtomicU64::new(0),
            last_issued_instance_id: std::sync::Mutex::new(None),
            last_activate_instance_id: std::sync::Mutex::new(None),
            last_activate_device_public_key: std::sync::Mutex::new(None),
            last_activate_pop_verified: AtomicBool::new(false),
            activated_binding: std::sync::Mutex::new(None),
            rebinds: AtomicU64::new(0),
            deactivates: AtomicU64::new(0),
            last_rebind_instance_id: std::sync::Mutex::new(None),
            last_rebind_binding_id: std::sync::Mutex::new(None),
            last_rebind_pop_verified: AtomicBool::new(false),
            last_deactivate_pop_verified: AtomicBool::new(false),
            last_deactivate_binding_id: std::sync::Mutex::new(None),
        }
    }

    pub fn kit(&self) -> &FabricatedKeyset {
        &self.kit
    }
    pub fn pinned_root(&self) -> PinnedRoot {
        self.kit.pinned_root()
    }
    /// The server's established binding id (the lease the renew path serves binds it) —
    /// so a BOUND device identity for the rebind/deactivate continuity ops addresses
    /// the binding the fake actually holds (ADR-I009).
    pub fn binding_id(&self) -> &'static str {
        self.kit.binding_id()
    }
    pub fn set_withhold_lease(&self, on: bool) {
        self.withhold_lease.store(on, Ordering::SeqCst);
    }
    pub fn set_fail(&self, on: bool) {
        self.fail.store(on, Ordering::SeqCst);
    }
    /// Make every server call BLOCK in-flight forever (until [`release`]) — a real
    /// stalled transport for the isolation gate.
    pub fn set_block(&self, on: bool) {
        self.block.store(on, Ordering::SeqCst);
    }
    /// True once a blocked call is genuinely in flight (await this before probing
    /// concurrency so the stall is real, not a race).
    pub fn is_in_flight(&self) -> bool {
        self.in_flight.load(Ordering::SeqCst)
    }
    /// Release any in-flight blocked call (test teardown).
    pub fn release(&self) {
        self.block.store(false, Ordering::SeqCst);
        self.unblock.notify_waiters();
    }
    pub fn set_next_due_ms(&self, ms: u64) {
        self.next_due_ms.store(ms, Ordering::SeqCst);
    }
    /// Make `heartbeat` replay an OLDER, already-expired (but still-signed) lease.
    pub fn set_replay_expired(&self, on: bool) {
        self.replay_expired.store(on, Ordering::SeqCst);
    }
    /// Make the returned lease bind a DIFFERENT device's instance binding id.
    pub fn set_foreign_binding(&self, on: bool) {
        self.foreign_binding.store(on, Ordering::SeqCst);
    }
    /// Make the returned lease's signed `not_after` an absolute 1970 instant
    /// (deterministically past the real clock install() reads → `LeaseExpired`).
    pub fn set_replay_absolute_past(&self, on: bool) {
        self.replay_absolute_past.store(on, Ordering::SeqCst);
    }
    /// Make the returned lease STALE (older granted_at than a normal lease) yet
    /// still future — so it clears the expiry gate and the store rejects it as
    /// `Stale` (exercising the Stale->Ok fold / no-poison path).
    pub fn set_replay_stale(&self, on: bool) {
        self.replay_stale.store(on, Ordering::SeqCst);
    }

    // --- device-PoP (ADR-I007) knobs + accessors ---
    /// How many times `fetch_challenge` has been called.
    pub fn challenge_fetches(&self) -> u64 {
        self.challenge_fetches.load(Ordering::SeqCst)
    }
    /// The most recent challenge nonce this server issued (via `fetch_challenge`).
    pub fn last_issued_nonce(&self) -> String {
        self.last_issued_nonce
            .lock()
            .expect("poisoned")
            .clone()
            .unwrap_or_default()
    }
    /// The `nonce` field the most recent heartbeat request body carried.
    pub fn last_request_nonce(&self) -> Option<String> {
        self.last_request_nonce.lock().expect("poisoned").clone()
    }
    /// The `nextNonce` the most recent heartbeat response returned.
    pub fn last_next_nonce(&self) -> Option<String> {
        self.last_next_nonce.lock().expect("poisoned").clone()
    }
    /// Whether the most recent heartbeat's PoP proof verified.
    pub fn last_pop_verified(&self) -> bool {
        self.last_pop_verified.load(Ordering::SeqCst)
    }
    /// Make `fetch_challenge` fail (cold start can't get a nonce).
    pub fn set_fail_challenge(&self, on: bool) {
        self.fail_challenge.store(on, Ordering::SeqCst);
    }
    /// Make `fetch_challenge` issue an already-expired challenge (`expiresAtMs` in
    /// the past) — the client must not sign/send it.
    pub fn set_challenge_expired(&self, on: bool) {
        self.challenge_expired.store(on, Ordering::SeqCst);
    }
    /// Make the next heartbeat response carry an EMPTY `nextNonce`.
    pub fn set_drop_next_nonce(&self, on: bool) {
        self.drop_next_nonce.store(on, Ordering::SeqCst);
    }
    /// Make `heartbeat` return a `Malformed` error AFTER verifying the proof (a 2xx
    /// whose body would not parse — a RECEIVED contact that burns the nonce).
    pub fn set_malformed_2xx(&self, on: bool) {
        self.malformed_2xx.store(on, Ordering::SeqCst);
    }
    /// Make `heartbeat`/`activate` reject the proof as `pop-invalid` (401).
    pub fn set_reject_pop(&self, on: bool) {
        self.reject_pop.store(on, Ordering::SeqCst);
    }

    // --- device ACTIVATE / enrolment (ADR-I008) accessors ---
    /// How many times `activate` has been called.
    pub fn activates(&self) -> u64 {
        self.activates.load(Ordering::SeqCst)
    }
    /// The server-assigned instance id the most recent challenge issued.
    pub fn last_issued_instance_id(&self) -> String {
        self.last_issued_instance_id
            .lock()
            .expect("poisoned")
            .clone()
            .unwrap_or_default()
    }
    /// The `instanceId` the most recent activate request carried.
    pub fn last_activate_instance_id(&self) -> Option<String> {
        self.last_activate_instance_id
            .lock()
            .expect("poisoned")
            .clone()
    }
    /// The `devicePublicKey` the most recent activate request carried.
    pub fn last_activate_device_public_key(&self) -> Option<String> {
        self.last_activate_device_public_key
            .lock()
            .expect("poisoned")
            .clone()
    }
    /// Whether the most recent activate PoP proof verified (and bound the
    /// server-assigned instance id).
    pub fn last_activate_pop_verified(&self) -> bool {
        self.last_activate_pop_verified.load(Ordering::SeqCst)
    }
    /// Count `rebind` calls (ADR-I009).
    pub fn rebinds(&self) -> u64 {
        self.rebinds.load(Ordering::SeqCst)
    }
    /// Count `deactivate` calls (ADR-I009).
    pub fn deactivates(&self) -> u64 {
        self.deactivates.load(Ordering::SeqCst)
    }
    /// The `instanceId` the most recent rebind request carried (the device's OWN id).
    /// The `bindingId` the most recent rebind request carried (the device's resolved
    /// binding — store-learned id, not the instance_id fallback).
    pub fn last_rebind_binding_id(&self) -> Option<String> {
        self.last_rebind_binding_id
            .lock()
            .expect("poisoned")
            .clone()
    }
    pub fn last_rebind_instance_id(&self) -> Option<String> {
        self.last_rebind_instance_id
            .lock()
            .expect("poisoned")
            .clone()
    }
    /// Whether the most recent rebind PoP proof verified (bound the device's own id).
    pub fn last_rebind_pop_verified(&self) -> bool {
        self.last_rebind_pop_verified.load(Ordering::SeqCst)
    }
    /// Whether the most recent deactivate PoP proof verified.
    pub fn last_deactivate_pop_verified(&self) -> bool {
        self.last_deactivate_pop_verified.load(Ordering::SeqCst)
    }
    /// The `bindingId` the most recent deactivate request carried.
    pub fn last_deactivate_binding_id(&self) -> Option<String> {
        self.last_deactivate_binding_id
            .lock()
            .expect("poisoned")
            .clone()
    }
    /// The Idempotency-Keys recorded across all mutations, in order — so a test can
    /// assert a retry replayed the SAME key (idempotency stability, ADR-I009).
    pub fn recorded_idempotency_keys(&self) -> Vec<String> {
        self.idempotency_keys.lock().expect("poisoned").clone()
    }
    /// Mint the server-assigned durable instance id reserved with a challenge nonce
    /// (deterministic per counter), shaped like `ib_<hex>`.
    fn mint_instance_id(&self) -> String {
        let n = self.nonce_counter.load(Ordering::SeqCst);
        format!("ib_{n:032x}")
    }
    /// Mint a fresh, distinct 64-hex challenge nonce (deterministic per counter).
    fn mint_nonce(&self) -> String {
        let n = self.nonce_counter.fetch_add(1, Ordering::SeqCst);
        // 32 bytes: a fixed prefix + the counter in the tail, rendered lower-case hex.
        let mut bytes = [0u8; 32];
        bytes[24..32].copy_from_slice(&n.to_be_bytes());
        hex::encode(bytes)
    }
    /// Verify a presented `Conspect-Device-PoP` header against the device public key
    /// — exactly the check the server performs. Returns true iff:
    ///   1. the COSE_Sign1 signature is valid for the bound device key over the
    ///      payload (proves the device key signed THIS pre-image), AND
    ///   2. the signed payload BINDS this request — it contains `sha256(body)` and
    ///      the raw 32-byte challenge nonce (so a proof cannot be replayed onto a
    ///      different body/nonce).
    fn verify_pop(&self, pop_header: &str, body: &[u8], nonce_hex: &str) -> bool {
        use coset::{CborSerializable as _, CoseSign1};
        let Ok(cose_bytes) = base64::engine::general_purpose::STANDARD.decode(pop_header) else {
            return false;
        };
        let Ok(sign1) = CoseSign1::from_slice(&cose_bytes) else {
            return false;
        };
        let vk = EdVerifyingKey::from_bytes(&pop_test_signer().public_key_raw()).expect("vk");
        let sig_ok = sign1
            .verify_signature(b"", |sig, tbs| {
                let signature = match ed25519_dalek::Signature::from_slice(sig) {
                    Ok(s) => s,
                    Err(e) => return Err(format!("bad sig bytes: {e}")),
                };
                vk.verify(tbs, &signature)
                    .map_err(|e| format!("verify: {e}"))
            })
            .is_ok();
        if !sig_ok {
            return false;
        }
        let Some(payload) = sign1.payload.as_deref() else {
            return false;
        };
        // The signed payload must bind THIS body + THIS nonce.
        let body_hash = {
            use sha2::{Digest as _, Sha256};
            let mut h = Sha256::new();
            h.update(body);
            h.finalize()
        };
        let Ok(nonce_raw) = hex::decode(nonce_hex) else {
            return false;
        };
        windows_contains(payload, &body_hash) && windows_contains(payload, &nonce_raw)
    }

    /// Like [`verify_pop`] but ALSO requires the signed payload to bind a specific
    /// `instance_id` (text) — the activate first-contact check: the proof must be
    /// bound to the SERVER-assigned instanceId the server reserved with the nonce.
    fn verify_activate_pop(
        &self,
        pop_header: &str,
        body: &[u8],
        nonce_hex: &str,
        instance_id: &str,
    ) -> bool {
        use coset::{CborSerializable as _, CoseSign1};
        if !self.verify_pop(pop_header, body, nonce_hex) {
            return false;
        }
        let Ok(cose_bytes) = base64::engine::general_purpose::STANDARD.decode(pop_header) else {
            return false;
        };
        let Ok(sign1) = CoseSign1::from_slice(&cose_bytes) else {
            return false;
        };
        let Some(payload) = sign1.payload.as_deref() else {
            return false;
        };
        // The instance id is a CBOR text string in the pre-image — assert its bytes
        // appear (the canonical encoder emits it as a tstr).
        windows_contains(payload, instance_id.as_bytes())
    }

    /// The binding id the current knobs select for the served lease body. A
    /// successful `activate` records its server-assigned binding so the subsequent
    /// renew serves a lease bound to the SAME id (no `BindingMismatch`); absent that,
    /// the static fabricated binding (the pre-bound renew tests use it directly).
    fn served_binding(&self) -> String {
        if self.foreign_binding.load(Ordering::SeqCst) {
            self.kit.foreign_binding_id().to_owned()
        } else if let Some(b) = self.activated_binding.lock().expect("poisoned").clone() {
            b
        } else {
            self.kit.binding_id().to_owned()
        }
    }
    /// The signed lease the current knobs select (foreign binding and/or an
    /// absolute-past expiry), else a healthy 35-day lease for this device.
    fn served_lease(&self) -> ServerLease {
        let not_after = if self.replay_absolute_past.load(Ordering::SeqCst) {
            1_000_000 // 1970-01-01T00:16:40Z — before any real clock
        } else if self.replay_stale.load(Ordering::SeqCst) {
            // A SHORTER (still-future) term → an OLDER granted_at than the normal
            // 35-day lease (granted_at = not_after - 35d). fab_now+20d is still
            // far past the real clock, so it clears the expiry gate but is stale
            // vs a lease granted at fab_now.
            self.kit.now_ms() + 20 * 86_400_000
        } else {
            self.kit.now_ms() + 35 * 86_400_000
        };
        self.kit.sign_lease_for(&self.served_binding(), not_after)
    }
    /// The bindingId the most recent heartbeat request carried.
    pub fn last_binding_id(&self) -> Option<String> {
        self.last_binding_id.lock().expect("poisoned").clone()
    }
    pub fn clear_last_binding_id(&self) {
        *self.last_binding_id.lock().expect("poisoned") = None;
    }
    /// Make the signer unrevoked on the first `fetch_keys` but revoked on every
    /// later fetch (the revocation-TOCTOU case).
    pub fn set_revoke_signer_after_first_fetch(&self, on: bool) {
        self.revoke_signer_after_first_fetch
            .store(on, Ordering::SeqCst);
    }
    /// How many times `fetch_keys` has been called.
    pub fn key_fetches(&self) -> u64 {
        self.key_fetches.load(Ordering::SeqCst)
    }
    /// Every `Idempotency-Key` the server received, in order.
    pub fn idempotency_keys(&self) -> Vec<String> {
        self.idempotency_keys.lock().expect("poisoned").clone()
    }
    /// Every heartbeat request body the server received, in order.
    pub fn bodies(&self) -> Vec<Vec<u8>> {
        self.bodies.lock().expect("poisoned").clone()
    }
    /// Make the next `n` heartbeat calls record their Idempotency-Key, then return
    /// a transport error (a lost-response analogue) — for the retry-stability test.
    pub fn set_fail_after_recording_idempotency(&self, n: u64) {
        self.fail_after_recording.store(n, Ordering::SeqCst);
    }

    /// Record the `Idempotency-Key` this mutation carried, then — if the
    /// `fail_after_recording` knob is still positive — decrement it and return a
    /// transport error (a lost-response analogue: the server DID process/record
    /// the key, but the device never saw the response). The retry of the same
    /// logical operation must carry the SAME key (the idempotency-stability test).
    fn record_idempotency_then_maybe_fail(&self, idempotency_key: &str) -> Option<HeartbeatError> {
        self.idempotency_keys
            .lock()
            .expect("poisoned")
            .push(idempotency_key.to_owned());
        // `fetch_update` decrements only while positive, so the failure fires for
        // exactly the first N recorded mutations and not after.
        let took = self
            .fail_after_recording
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| {
                if n > 0 {
                    Some(n - 1)
                } else {
                    None
                }
            })
            .is_ok();
        took.then(|| HeartbeatError::Transport("fake lost-response after recording".to_owned()))
    }

    /// Block in-flight (mark in_flight, await the unblock) when the block knob is
    /// set — the shared body each server method runs first.
    async fn maybe_block(&self) {
        if self.block.load(Ordering::SeqCst) {
            self.in_flight.store(true, Ordering::SeqCst);
            // Park until released. `notify_waiters` only wakes current waiters, so
            // register interest, then re-check the flag to avoid a lost wakeup.
            while self.block.load(Ordering::SeqCst) {
                let fut = self.unblock.notified();
                if !self.block.load(Ordering::SeqCst) {
                    break;
                }
                fut.await;
            }
            self.in_flight.store(false, Ordering::SeqCst);
        }
    }
}

impl LicenceServer for FakeLicenceServer {
    async fn fetch_keys(&self) -> Result<LicensingKeys, HeartbeatError> {
        self.maybe_block().await;
        if self.fail.load(Ordering::SeqCst) {
            return Err(HeartbeatError::Transport("fake offline".to_owned()));
        }
        // Count this fetch. The revocation-TOCTOU knob serves a clean keyset on
        // the FIRST fetch (signer unrevoked) and a keyset listing the signer in
        // the (root-signed) revocation set on every LATER fetch — so a re-fetch at
        // lease-acceptance observes the revocation, while reusing the stale doc
        // would not. The first fetch is `0`; the acceptance re-fetch is `1`+.
        let n = self.key_fetches.fetch_add(1, Ordering::SeqCst);
        if self.revoke_signer_after_first_fetch.load(Ordering::SeqCst) && n >= 1 {
            return Ok(self.kit.keys_with_signer_revoked("current"));
        }
        Ok(self.kit.keys())
    }

    async fn fetch_challenge(&self, _org: &str) -> Result<DeviceChallenge, HeartbeatError> {
        self.maybe_block().await;
        self.challenge_fetches.fetch_add(1, Ordering::SeqCst);
        if self.fail_challenge.load(Ordering::SeqCst) || self.fail.load(Ordering::SeqCst) {
            return Err(HeartbeatError::Transport(
                "fake /challenge offline".to_owned(),
            ));
        }
        let nonce = self.mint_nonce();
        *self.last_issued_nonce.lock().expect("poisoned") = Some(nonce.clone());
        // The server-assigned durable instance id reserved with this nonce (v0.16.0):
        // a first-contact device echoes it on activate. Minted BEFORE the nonce
        // counter advances again so it pairs with this challenge.
        let instance_id = self.mint_instance_id();
        *self.last_issued_instance_id.lock().expect("poisoned") = Some(instance_id.clone());
        // Normally issued generously-future (the client reads the real wall clock,
        // so i64::MAX is never treated as expired). The `challenge_expired` knob
        // returns a clearly-past expiry (1970) so the client's fresh-nonce expiry
        // check rejects it.
        let expires_at_ms = if self.challenge_expired.load(Ordering::SeqCst) {
            1_000_000 // 1970-01-01T00:16:40Z — before any real clock
        } else {
            i64::MAX
        };
        Ok(DeviceChallenge::new(nonce, expires_at_ms, instance_id))
    }

    async fn heartbeat(
        &self,
        _org: &str,
        body: Vec<u8>,
        idempotency_key: &str,
        pop_header: &str,
    ) -> Result<HeartbeatResponse, HeartbeatError> {
        self.maybe_block().await;
        // Record the EXACT body bytes BEFORE any maybe-fail, so a lost-response
        // attempt's body is captured and the retry-coupling test can compare it to
        // the retry's body.
        self.bodies.lock().expect("poisoned").push(body.clone());
        if let Some(err) = self.record_idempotency_then_maybe_fail(idempotency_key) {
            return Err(err);
        }
        if self.fail.load(Ordering::SeqCst) {
            return Err(HeartbeatError::Transport("fake offline".to_owned()));
        }
        // Parse the EXACT body bytes the transport sent (the same bytes the PoP
        // signed over) to read the request fields.
        let req: HeartbeatRequest = match serde_json::from_slice(&body) {
            Ok(r) => r,
            Err(e) => return Err(HeartbeatError::Malformed(format!("fake body parse: {e}"))),
        };
        // Record + VERIFY the device-PoP proof against the device key over the
        // recomputed pre-image (sha256 of THESE body bytes + the request nonce).
        *self.last_request_nonce.lock().expect("poisoned") = Some(req.nonce.clone());
        let verified = self.verify_pop(pop_header, &body, &req.nonce);
        self.last_pop_verified.store(verified, Ordering::SeqCst);
        // `pop-invalid` (401): the server rejects the proof — a DEFINITIVE rejection
        // (an HTTP response WAS received). Either the knob forces it, or the proof
        // genuinely failed to verify (a real server would 401 it too). Surfaced as
        // `ServerRejected` so the client drops the burned nonce + recovers with a
        // fresh /challenge next cycle (round-3) — NOT a `Transport` (ambiguous) error
        // that would replay the burned nonce forever.
        if self.reject_pop.load(Ordering::SeqCst) || !verified {
            return Err(HeartbeatError::ServerRejected(
                "fake heartbeat returned HTTP 401 pop-invalid".to_owned(),
            ));
        }
        // A 2xx whose body would not parse: the server PROCESSED the request (the nonce
        // verified + recorded above is burned) but the client cannot read the lease —
        // `post_raw_json` surfaces this as `Malformed`. A RECEIVED contact, so the client
        // must recover with a fresh /challenge (round-4), never replay the burned nonce.
        if self.malformed_2xx.load(Ordering::SeqCst) {
            return Err(HeartbeatError::Malformed(
                "fake heartbeat returned HTTP 200 with an unparseable body".to_owned(),
            ));
        }
        // Record the bindingId the client addressed us by (the renewal must use
        // the server's instanceBindingId, never the lease serial).
        *self.last_binding_id.lock().expect("poisoned") = Some(req.binding_id.clone());
        self.heartbeats.fetch_add(1, Ordering::SeqCst);
        let (lease, state) = if self.withhold_lease.load(Ordering::SeqCst) {
            (None, EnforcementState::Revoked)
        } else if self.replay_expired.load(Ordering::SeqCst) {
            // A replay: an OLDER lease with a DISTINCT (older) serial, still
            // Ed25519-valid, whose signed not_after is already in the past
            // (expired 5 days ago). A distinct serial means the install is NOT a
            // benign serial-collision no-op — the ONLY thing that can keep it from
            // re-extending entitlement is honouring the signed (past) not_after.
            let past = self.kit.now_ms() - 5 * 86_400_000;
            (
                Some(self.kit.sign_old_lease(past)),
                EnforcementState::Compliant,
            )
        } else {
            // The knob-selected lease (foreign binding and/or absolute-past expiry,
            // else a healthy 35-day lease for this device).
            (Some(self.served_lease()), EnforcementState::Compliant)
        };
        // Mint the NEXT single-use nonce (DPoP-nonce style) — UNLESS the knob drops
        // it, forcing the client to cold-start a fresh /challenge next cycle.
        let next_nonce = if self.drop_next_nonce.load(Ordering::SeqCst) {
            String::new()
        } else {
            self.mint_nonce()
        };
        *self.last_next_nonce.lock().expect("poisoned") = if next_nonce.is_empty() {
            None
        } else {
            Some(next_nonce.clone())
        };
        Ok(HeartbeatResponse::new(
            lease,
            state,
            self.next_due_ms.load(Ordering::SeqCst) as i64,
            next_nonce,
        ))
    }

    async fn activate(
        &self,
        _org: &str,
        body: Vec<u8>,
        idempotency_key: &str,
        pop_header: &str,
    ) -> Result<ActivateResponse, HeartbeatError> {
        self.maybe_block().await;
        self.bodies.lock().expect("poisoned").push(body.clone());
        if let Some(err) = self.record_idempotency_then_maybe_fail(idempotency_key) {
            return Err(err);
        }
        if self.fail.load(Ordering::SeqCst) {
            return Err(HeartbeatError::Transport("fake offline".to_owned()));
        }
        // Parse the EXACT activate body bytes (the same bytes the PoP signed over).
        let req: ActivateRequest = match serde_json::from_slice(&body) {
            Ok(r) => r,
            Err(e) => {
                return Err(HeartbeatError::Malformed(format!(
                    "fake activate parse: {e}"
                )))
            }
        };
        self.activates.fetch_add(1, Ordering::SeqCst);
        *self.last_activate_instance_id.lock().expect("poisoned") = Some(req.instance_id.clone());
        *self
            .last_activate_device_public_key
            .lock()
            .expect("poisoned") = Some(req.device_public_key.clone());
        // VERIFY the activate proof binds THIS body + THIS nonce + the SERVER-assigned
        // instance id (the device must echo + bind the id the challenge reserved).
        let expected_instance = self.last_issued_instance_id();
        let verified = self.verify_activate_pop(pop_header, &body, &req.nonce, &expected_instance);
        self.last_activate_pop_verified
            .store(verified, Ordering::SeqCst);
        // `pop-invalid` (401): the knob forces it, or the proof genuinely failed (a
        // real server 401s it) — a DEFINITIVE rejection (a response WAS received).
        if self.reject_pop.load(Ordering::SeqCst)
            || !verified
            || req.instance_id != expected_instance
        {
            return Err(HeartbeatError::ServerRejected(
                "fake activate returned HTTP 401 pop-invalid".to_owned(),
            ));
        }
        // The signed lease the activation issues (an ActivationLease — a superset of
        // HeartbeatLease that the ServerLease type already deserializes), bound to the
        // server-assigned instance binding id. Record that binding so the subsequent
        // renew (heartbeat) serves a lease bound to the SAME id.
        *self.activated_binding.lock().expect("poisoned") = Some(expected_instance.clone());
        let lease = self
            .kit
            .sign_lease_for(&expected_instance, self.kit.now_ms() + 35 * 86_400_000);
        let next_nonce = self.mint_nonce();
        *self.last_next_nonce.lock().expect("poisoned") = Some(next_nonce.clone());
        Ok(ActivateResponse::new(
            Some(lease),
            EnforcementState::Compliant,
            next_nonce,
        ))
    }

    async fn rebind(
        &self,
        _org: &str,
        body: Vec<u8>,
        idempotency_key: &str,
        pop_header: &str,
    ) -> Result<RebindResponse, HeartbeatError> {
        self.maybe_block().await;
        self.bodies.lock().expect("poisoned").push(body.clone());
        if let Some(err) = self.record_idempotency_then_maybe_fail(idempotency_key) {
            return Err(err);
        }
        if self.fail.load(Ordering::SeqCst) {
            return Err(HeartbeatError::Transport("fake offline".to_owned()));
        }
        // Parse the EXACT rebind body bytes (the same bytes the PoP signed over).
        let req: RebindRequest = match serde_json::from_slice(&body) {
            Ok(r) => r,
            Err(e) => return Err(HeartbeatError::Malformed(format!("fake rebind parse: {e}"))),
        };
        self.rebinds.fetch_add(1, Ordering::SeqCst);
        *self.last_rebind_instance_id.lock().expect("poisoned") = Some(req.instance_id.clone());
        *self.last_rebind_binding_id.lock().expect("poisoned") = Some(req.binding_id.clone());
        // VERIFY the proof binds THIS body + THIS nonce + the device's OWN instance id
        // (continuity — NOT a server-assigned challenge id).
        let verified = self.verify_activate_pop(pop_header, &body, &req.nonce, &req.instance_id);
        self.last_rebind_pop_verified
            .store(verified, Ordering::SeqCst);
        if self.reject_pop.load(Ordering::SeqCst) || !verified {
            return Err(HeartbeatError::ServerRejected(
                "fake rebind returned HTTP 401 pop-invalid".to_owned(),
            ));
        }
        if self.malformed_2xx.load(Ordering::SeqCst) {
            return Err(HeartbeatError::Malformed(
                "fake rebind returned HTTP 200 with an unparseable body".to_owned(),
            ));
        }
        // A successful rebind reactivates the SAME binding (no new seat) and records it
        // so the SUBSEQUENT renew serves a lease bound to the SAME id.
        let bound = req.binding_id.clone();
        *self.activated_binding.lock().expect("poisoned") = Some(bound);
        // The response carries only a serial (NOT an embedded lease) + the next nonce.
        let next_nonce = if self.drop_next_nonce.load(Ordering::SeqCst) {
            String::new()
        } else {
            self.mint_nonce()
        };
        *self.last_next_nonce.lock().expect("poisoned") = if next_nonce.is_empty() {
            None
        } else {
            Some(next_nonce.clone())
        };
        Ok(RebindResponse::new(
            true,
            Some(self.kit.lease_serial().to_owned()),
            Some(self.kit.now_ms() + 35 * 86_400_000),
            EnforcementState::Compliant,
            1,
            false,
            req.fp_score,
            next_nonce,
        ))
    }

    async fn deactivate(
        &self,
        _org: &str,
        body: Vec<u8>,
        idempotency_key: &str,
        pop_header: &str,
    ) -> Result<DeactivateResponse, HeartbeatError> {
        self.maybe_block().await;
        self.bodies.lock().expect("poisoned").push(body.clone());
        if let Some(err) = self.record_idempotency_then_maybe_fail(idempotency_key) {
            return Err(err);
        }
        if self.fail.load(Ordering::SeqCst) {
            return Err(HeartbeatError::Transport("fake offline".to_owned()));
        }
        let req: DeactivateRequest = match serde_json::from_slice(&body) {
            Ok(r) => r,
            Err(e) => {
                return Err(HeartbeatError::Malformed(format!(
                    "fake deactivate parse: {e}"
                )))
            }
        };
        self.deactivates.fetch_add(1, Ordering::SeqCst);
        *self.last_deactivate_binding_id.lock().expect("poisoned") = Some(req.binding_id.clone());
        // VERIFY the proof binds THIS body + THIS nonce + the device's OWN instance id.
        // The deactivate body has no instance_id field, so the device signs over its
        // configured identity instance_id (continuity); the loop test uses inst_prod_a.
        let verified = self.verify_activate_pop(pop_header, &body, &req.nonce, "inst_prod_a");
        self.last_deactivate_pop_verified
            .store(verified, Ordering::SeqCst);
        if self.reject_pop.load(Ordering::SeqCst) || !verified {
            return Err(HeartbeatError::ServerRejected(
                "fake deactivate returned HTTP 401 pop-invalid".to_owned(),
            ));
        }
        if self.malformed_2xx.load(Ordering::SeqCst) {
            return Err(HeartbeatError::Malformed(
                "fake deactivate returned HTTP 200 with an unparseable body".to_owned(),
            ));
        }
        // The 200 returns an InstanceBinding whose lifecycleState is `released`.
        Ok(DeactivateResponse::new(
            req.binding_id,
            "released".to_owned(),
            EnforcementState::Revoked,
        ))
    }
}

/// Whether `haystack` contains the contiguous `needle` (for asserting the PoP
/// payload binds a body hash / nonce).
fn windows_contains(haystack: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty() && haystack.windows(needle.len()).any(|w| w == needle)
}

/// Shareable handle for the loop test.
pub fn shared_fake() -> Arc<FakeLicenceServer> {
    Arc::new(FakeLicenceServer::new())
}

/// Build a [`LeaseBinding`] + the [`PinnedKey`] that verifies it, for the
/// **offline-upload / file-drop** install surface — the path
/// `multiview-control`'s `install_lease` route and the `LeaseDirectoryWatcher`
/// take, which call [`LeaseStore::install_binding`] DIRECTLY (not via the
/// heartbeat client). The returned binding carries `instance_binding_id` so a
/// store that records the binding anchor on install makes `current_binding_id()`
/// reflect this device's identity — exactly the chokepoint the binding-anchor fix
/// establishes. The lease expiry is anchored well into the future (the kit's
/// epoch + 35d) so the install clears the expiry gate.
pub fn upload_binding_for(
    kit: &FabricatedKeyset,
    instance_binding_id: &str,
) -> (
    multiview_licence::store::LeaseBinding,
    multiview_licence::verify::PinnedKey,
) {
    use ed25519_dalek::{Signer as _, SigningKey};
    use multiview_licence::entitlement::{Entitlement, EntitlementFlags, GpuLimit, HardwareClass};
    use multiview_licence::lease::Lease;
    use multiview_licence::store::LeaseBinding;
    use multiview_licence::verify::{PinnedKey, SignedLease};
    use multiview_licence::{Tier, ACTIVATION_WINDOW_DAYS};

    // A dated lease whose expiry is the kit's signed `not_after` (epoch + 35d) —
    // far past any real clock, so the store's expiry gate passes and the grant is
    // newer than an empty store.
    let not_after = kit.now_ms() + 35 * 86_400_000;
    let lease = Lease::new_online_expiring_at(
        kit.lease_serial().to_owned(),
        not_after,
        kit.now_ms(),
        ACTIVATION_WINDOW_DAYS,
    )
    .expect("a future not_after yields a lease");
    let entitlement = Entitlement::new(
        Tier::new(kit.licence_id().to_owned()),
        HardwareClass::Standard,
        HardwareClass::Standard,
        GpuLimit::Limited(2),
        lease,
        EntitlementFlags::default(),
    );
    // Sign the install envelope with a deterministic key the caller pins — exactly
    // the seal/verify contract `LeaseStore::install_binding` re-checks for every
    // producer. (This stands in for the offline-lease issuer's signature; the
    // store verifies the binding against the pinned key handed alongside it.)
    let envelope_signer = SigningKey::from_bytes(&[0x6d; 32]);
    let pinned = PinnedKey::from_verifying_key(&envelope_signer.verifying_key());
    // Sign over the lease BOUND to the binding id this binding carries, so the
    // store's signature check covers the anchor (matches seal_for_install).
    let msg = SignedLease::signing_bytes(&entitlement.lease, Some(instance_binding_id));
    let sig = envelope_signer.sign(&msg);
    let signed_lease = SignedLease::new(entitlement.lease.clone(), sig.to_bytes());
    let binding = LeaseBinding::new(
        signed_lease,
        entitlement,
        100,
        Some(instance_binding_id.to_owned()),
    );
    (binding, pinned)
}
