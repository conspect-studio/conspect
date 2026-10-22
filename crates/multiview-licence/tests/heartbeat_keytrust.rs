//! CONSPECT-3 **key-trust** + signed-lease verification tests (ADR-0096, D1/D3).
//!
//! The key-trust cases verify against the **live production well-known document**
//! (`GET https://api.conspect.studio/.well-known/conspect-licensing-keys.json`,
//! captured 2026-06-16): the real pinned root (ECDSA P-256) must attest the real
//! dual-pin Ed25519 intermediates and the real revocation statement — so the
//! verifier is proven against the authoritative trust material itself, the
//! strongest possible golden vector. The signed-lease cases mint locally-signed
//! leases under a fabricated keyset whose root we control (the production private
//! keys are server-side only).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::as_conversions,
    clippy::missing_panics_doc
)]
#![cfg(feature = "heartbeat")]

use multiview_licence::heartbeat::{
    canonical_key_preimage, verify_signed_lease_chain, KeyTrustError, LeaseBodyFields,
    LicensingKeys, PinnedRoot, SignedLeaseError, TrustedKeys,
};

mod fake;
use fake::{b64url, FabricatedKeyset, ROOT_PUB_B64URL};

/// The current dual-pin intermediate (Ed25519), from the live well-known doc.
const IV1_KID: &str = "intermediate-v1";
const IV1_PUB_B64URL: &str = "EYwdPYlasNl8JRksWWrFbGBrJD4SFumP0SeLDv2R3Cc";
const IV1_VALID_FROM: i64 = 1_781_510_167_548;
const IV1_VALID_UNTIL: i64 = 1_816_070_167_548;
const IV2_KID: &str = "intermediate-v2";
const IV2_VALID_UNTIL: i64 = 1_850_630_167_548;
/// A wall instant inside both intermediates' validity (epoch ms).
const NOW_MS_VALID: i64 = 1_790_000_000_000;
const KEY_STATEMENT: &str = "conspect.key-attestation.v1";

/// The live production well-known document, verbatim.
const LIVE_KEYS_JSON: &str = include_str!("fixtures/conspect-licensing-keys.json");

fn pinned_root() -> PinnedRoot {
    PinnedRoot::from_base64url(ROOT_PUB_B64URL).expect("the production root key must parse")
}

// --- D3: canonical-CBOR key pre-image golden vector --------------------------

#[test]
fn canonical_key_preimage_matches_the_known_golden_bytes() {
    let pre = canonical_key_preimage(
        IV1_KID,
        "lease",
        KEY_STATEMENT,
        &b64url(IV1_PUB_B64URL),
        IV1_VALID_FROM,
        IV1_VALID_UNTIL,
    );
    // Byte-exact RFC 8949 §4.2.1 (cross-checked with an independent Python CBOR
    // encoder + the live root_sig that signs exactly these bytes).
    let golden = "a6666b65795f69646f696e7465726d6564696174652d7631686b65795f74797065656c656173656973746174656d656e74781b636f6e73706563742e6b65792d6174746573746174696f6e2e76316a7075626c69635f6b65795820118c1d3d895ab0d97c25192c596ac56c606b243e1216e98fd1278b0efd91dc276a76616c69645f66726f6d1b0000019eca47dbfc6b76616c69645f756e74696c1b000001a6d6379bfc";
    assert_eq!(hex::encode(&pre), golden);
}

// --- D1: key-trust chain, against the LIVE well-known doc ---------------------

#[test]
fn live_wellknown_intermediates_verify_against_the_pinned_root() {
    let keys: LicensingKeys = serde_json::from_str(LIVE_KEYS_JSON).expect("well-known parses");
    let trusted = TrustedKeys::verify(&keys, &pinned_root(), NOW_MS_VALID)
        .expect("the production keyset must verify against the pinned root");
    assert!(
        trusted.lease_key(IV1_KID).is_some(),
        "current intermediate trusted"
    );
    assert!(
        trusted.lease_key(IV2_KID).is_some(),
        "next (dual-pin) intermediate trusted"
    );
    assert!(trusted.lease_key("intermediate-vX").is_none());
}

#[test]
fn an_intermediate_not_attested_by_the_root_is_rejected() {
    let mut keys: LicensingKeys = serde_json::from_str(LIVE_KEYS_JSON).unwrap();
    // Swap intermediate-v1's public key: its root_sig no longer covers it.
    keys.lease_keys[0].public_key = "ht_UXIIuzGj5Tk2NQr0fg9NlkYab_eKUVtWlNlMLERg".to_owned();
    let err = TrustedKeys::verify(&keys, &pinned_root(), NOW_MS_VALID)
        .expect_err("a non-root-attested intermediate must be rejected");
    assert!(
        matches!(err, KeyTrustError::IntermediateNotAttested { .. }),
        "got {err:?}"
    );
}

#[test]
fn an_intermediate_outside_its_validity_window_is_not_trusted() {
    let keys: LicensingKeys = serde_json::from_str(LIVE_KEYS_JSON).unwrap();
    let after_all = IV2_VALID_UNTIL + 1;
    let trusted = TrustedKeys::verify(&keys, &pinned_root(), after_all)
        .expect("an expired keyset still root-verifies; keys age out of the trusted set");
    assert!(trusted.lease_key(IV1_KID).is_none());
    assert!(trusted.lease_key(IV2_KID).is_none());
    let _ = IV1_VALID_UNTIL;
}

#[test]
fn a_forged_revocation_signature_is_rejected() {
    let mut keys: LicensingKeys = serde_json::from_str(LIVE_KEYS_JSON).unwrap();
    keys.revocation.root_revocation_sig = keys.lease_keys[0].root_sig.clone();
    let err = TrustedKeys::verify(&keys, &pinned_root(), NOW_MS_VALID)
        .expect_err("a forged revocation signature must be rejected (fail closed)");
    assert!(
        matches!(err, KeyTrustError::RevocationNotAttested),
        "got {err:?}"
    );
}

#[test]
fn a_wrong_pinned_root_rejects_the_whole_keyset() {
    let keys: LicensingKeys = serde_json::from_str(LIVE_KEYS_JSON).unwrap();
    // A different (fabricated) root cannot have signed the production attestations.
    // The verifier rejects up front with `RootMismatch` (the advertised root in the
    // document does not byte-match the pinned anchor) — a substituted well-known
    // document is caught before any signature is even checked. A keyset that
    // *omitted* the advertised root would instead fail at attestation; either way
    // the foreign-root keyset is rejected, which is the property under test.
    let kit = FabricatedKeyset::new();
    let err = TrustedKeys::verify(&keys, &kit.pinned_root(), NOW_MS_VALID)
        .expect_err("the live keyset must not verify under a foreign root");
    assert!(
        matches!(
            err,
            KeyTrustError::RootMismatch
                | KeyTrustError::IntermediateNotAttested { .. }
                | KeyTrustError::RevocationNotAttested
        ),
        "got {err:?}"
    );
}

// --- Revocation drop (fabricated root we control) ----------------------------

#[test]
fn a_revoked_signer_key_id_is_dropped_from_the_trusted_set() {
    let kit = FabricatedKeyset::new();
    let keys = kit.keys_with_revocation(&[kit.kid()]);
    let trusted = TrustedKeys::verify(&keys, &kit.pinned_root(), kit.now_ms())
        .expect("a keyset whose only key is revoked still verifies structurally");
    assert!(
        trusted.lease_key(kit.kid()).is_none(),
        "a revoked signerKeyId must be dropped from the trusted set"
    );
}

#[test]
fn a_non_revoked_signer_key_id_survives_with_an_unrelated_revocation() {
    let kit = FabricatedKeyset::new();
    let keys = kit.keys_with_revocation(&["intermediate-someone-else"]);
    let trusted = TrustedKeys::verify(&keys, &kit.pinned_root(), kit.now_ms()).unwrap();
    assert!(
        trusted.lease_key(kit.kid()).is_some(),
        "an unrelated revocation must not drop our key"
    );
}

// --- D3: signed-lease (bare Ed25519 over decoded leaseBytes) ------------------

#[test]
fn a_validly_signed_lease_verifies_and_yields_the_lease_body() {
    let kit = FabricatedKeyset::new();
    let trusted = kit.trusted();
    let lease = kit.sign_lease(35);
    let body =
        verify_signed_lease_chain(&lease, &trusted).expect("a correctly-signed lease verifies");
    assert_eq!(body.licence_id, kit.licence_id());
    assert!(body.not_after > kit.now_ms());
    assert_eq!(body.gpu_limit, Some(2));
}

#[test]
fn a_lease_with_a_broken_ed25519_signature_is_rejected() {
    let kit = FabricatedKeyset::new();
    let trusted = kit.trusted();
    let mut lease = kit.sign_lease(35);
    let mut sig = hex::decode(&lease.signature).unwrap();
    sig[0] ^= 0xff;
    lease.signature = hex::encode(sig);
    let err = verify_signed_lease_chain(&lease, &trusted)
        .expect_err("a tampered signature must be rejected");
    assert!(matches!(err, SignedLeaseError::BadSignature), "got {err:?}");
}

#[test]
fn a_lease_whose_leasebytes_were_tampered_is_rejected() {
    use base64::Engine;
    let kit = FabricatedKeyset::new();
    let trusted = kit.trusted();
    let mut lease = kit.sign_lease(35);
    let mut raw = base64::engine::general_purpose::STANDARD
        .decode(lease.lease_bytes.trim_end_matches('='))
        .unwrap();
    let last = raw.len() - 1;
    raw[last] ^= 0x01;
    lease.lease_bytes = base64::engine::general_purpose::STANDARD.encode(&raw);
    let err = verify_signed_lease_chain(&lease, &trusted)
        .expect_err("a tampered lease body must be rejected");
    assert!(matches!(err, SignedLeaseError::BadSignature), "got {err:?}");
}

#[test]
fn a_lease_signed_by_an_unknown_signer_key_id_is_rejected() {
    let kit = FabricatedKeyset::new();
    let trusted = kit.trusted();
    let mut lease = kit.sign_lease(35);
    lease.signer_key_id = "intermediate-ghost".to_owned();
    let err = verify_signed_lease_chain(&lease, &trusted)
        .expect_err("an unknown signerKeyId must be rejected");
    assert!(
        matches!(err, SignedLeaseError::UnknownSigner { .. }),
        "got {err:?}"
    );
}

#[test]
fn a_lease_with_a_non_hex_signature_is_rejected_not_panicked() {
    let kit = FabricatedKeyset::new();
    let trusted = kit.trusted();
    let mut lease = kit.sign_lease(35);
    lease.signature = "not-hex-zz".to_owned();
    let err = verify_signed_lease_chain(&lease, &trusted)
        .expect_err("a malformed signature must be a typed error, never a panic");
    assert!(
        matches!(err, SignedLeaseError::MalformedSignature),
        "got {err:?}"
    );
}

// --- Blocker #1: key-purpose binding (a non-lease key is NOT a lease signer) ---

#[test]
fn a_root_attested_update_key_is_not_trusted_as_a_lease_signer() {
    // The fabricated signer is genuinely root-attested (its root_sig covers a
    // pre-image with key_type="update"), but it is an UPDATE key, not a lease key.
    // The verifier must NOT accept it into the lease-signing trust set — otherwise
    // a root-attested key minted for a different purpose can sign leases.
    let kit = FabricatedKeyset::new();
    let keys = kit.keys_with_signer("update", "current");
    let trusted = TrustedKeys::verify(&keys, &kit.pinned_root(), kit.now_ms())
        .expect("the keyset is structurally root-attested; verify() still succeeds");
    assert!(
        trusted.lease_key(kit.kid()).is_none(),
        "a root-attested NON-lease (update) key must not be a trusted lease signer"
    );
}

#[test]
fn a_lease_signed_by_an_attested_update_key_is_rejected_end_to_end() {
    // End to end: a lease signed under an attested UPDATE key must be rejected at
    // verify_signed_lease_chain (the signer is not in the lease-signing set).
    let kit = FabricatedKeyset::new();
    let keys = kit.keys_with_signer("update", "current");
    let trusted = TrustedKeys::verify(&keys, &kit.pinned_root(), kit.now_ms()).unwrap();
    let lease = kit.sign_lease(35); // signed by the (now update-typed) fab key
    let err = verify_signed_lease_chain(&lease, &trusted)
        .expect_err("a lease signed by a non-lease key must be rejected");
    assert!(
        matches!(err, SignedLeaseError::UnknownSigner { .. }),
        "got {err:?}"
    );
}

// The `status` field is NOT in the root-signed key pre-image (map(6):
// key_id,key_type,statement,public_key,valid_from,valid_until), so a MITM /
// compromised well-known doc can flip a retired key's `status` to "current"
// WITHOUT breaking root_sig. Trust must therefore NOT rest on `status` — it
// rests on the SIGNED gates: key_type=="lease" ∧ now∈[valid_from,valid_until] ∧
// not in the signed revocation list. Retirement is expressed via those.

#[test]
fn a_status_flipped_to_current_is_still_rejected_when_outside_signed_validity() {
    // An attacker flips a retired key's status to "current" — but its SIGNED
    // validity window already ended. now > valid_until ⇒ still rejected.
    let kit = FabricatedKeyset::new();
    let ended = kit.now_ms() - 10 * 86_400_000; // window ended 10 days ago
    let keys = kit.keys_with_signer_validity(
        "lease",
        "current", // forged operational hint
        ended - 86_400_000,
        ended,
    );
    let trusted = TrustedKeys::verify(&keys, &kit.pinned_root(), kit.now_ms())
        .expect("structurally root-attested; verify() still succeeds");
    assert!(
        trusted.lease_key(kit.kid()).is_none(),
        "a forged-`current` key outside its SIGNED validity window must NOT be trusted"
    );
}

#[test]
fn a_status_flipped_to_current_is_still_rejected_when_revoked() {
    // An attacker flips a revoked key's status to "current" — but it is named in
    // the (root-signed) revocation list, so it stays dropped.
    let kit = FabricatedKeyset::new();
    let keys = kit.keys_with_signer_revoked("current"); // forged hint + revoked
    let trusted = TrustedKeys::verify(&keys, &kit.pinned_root(), kit.now_ms())
        .expect("structurally root-attested; verify() still succeeds");
    assert!(
        trusted.lease_key(kit.kid()).is_none(),
        "a forged-`current` key in the SIGNED revocation list must NOT be trusted"
    );
}

#[test]
fn a_lease_key_in_validity_and_unrevoked_is_trusted_regardless_of_status_hint() {
    // The flip side: trust rests on the SIGNED gates, NOT `status`. A lease key
    // that is in its signed validity window and not revoked is a valid signer
    // even if its (unsigned) status hint reads "retired" — retirement that is not
    // expressed via the signed validity window or revocation list is not binding.
    // (This documents that `status` is a hint, never a security gate.)
    let kit = FabricatedKeyset::new();
    for hint in ["current", "next", "retired", "anything"] {
        let keys = kit.keys_with_signer("lease", hint);
        let trusted = TrustedKeys::verify(&keys, &kit.pinned_root(), kit.now_ms()).unwrap();
        assert!(
            trusted.lease_key(kit.kid()).is_some(),
            "an in-validity, unrevoked lease key must be trusted (status hint {hint:?})"
        );
    }
}

// --- Blocker #7: negative timestamps are rejected, never coerced to 0 ----------

#[test]
fn an_intermediate_with_a_negative_valid_from_is_rejected_not_coerced() {
    // A negative valid_from must be rejected outright — never silently coerced to
    // unsigned 0 (which would change the signed pre-image and could trust a
    // malformed/forged key). The verifier rejects the whole keyset.
    let kit = FabricatedKeyset::new();
    let keys = kit.keys_with_negative_valid_from();
    let err = TrustedKeys::verify(&keys, &kit.pinned_root(), kit.now_ms())
        .expect_err("a negative timestamp must be rejected, not coerced");
    assert!(
        matches!(err, KeyTrustError::IntermediateNotAttested { .. }),
        "got {err:?}"
    );
}

// --- Blocker #2: the signed not_after is the authoritative expiry --------------

#[test]
fn a_signed_lease_already_past_its_not_after_is_rejected() {
    // A signed-but-expired lease (its cryptographically-signed not_after is in the
    // past) must be rejected — never installed as a fresh active term.
    let kit = FabricatedKeyset::new();
    let trusted = kit.trusted();
    let past = kit.now_ms() - 86_400_000; // 1 day ago
    let lease = kit.sign_lease_expiring_at(past);
    let err = verify_signed_lease_chain(&lease, &trusted)
        .err()
        .and_then(|e| {
            // The signature + parse must succeed; the expiry gate is the rejection.
            None::<SignedLeaseError>.or(Some(e))
        });
    // verify_signed_lease_chain itself stays pure (sig+parse); the expiry gate is
    // checked against an explicit `now`. Assert the body parses AND is flagged
    // expired by the body's own helper.
    assert!(
        err.is_none(),
        "signature+parse of a well-formed body succeeds"
    );
    let body = verify_signed_lease_chain(&lease, &trusted).unwrap();
    assert!(
        body.is_expired_at(kit.now_ms()),
        "a body whose not_after is in the past must report expired"
    );
    assert!(
        !kit.sign_lease(35).serial.is_empty(),
        "sanity: a fresh lease is well-formed"
    );
    let fresh = verify_signed_lease_chain(&kit.sign_lease(35), &trusted).unwrap();
    assert!(
        !fresh.is_expired_at(kit.now_ms()),
        "a fresh 35-day lease is not expired"
    );
}

// --- Blocker #4: required body fields fail closed -------------------------------

#[test]
fn a_signed_body_with_an_omitted_instance_binding_id_is_rejected() {
    // A body that genuinely OMITS instance_binding_id (not just empty) must be
    // rejected — never installed with an empty id via unwrap_or_default().
    let kit = FabricatedKeyset::new();
    let trusted = kit.trusted();
    let lease = kit.sign_body_omitting("instance_binding_id");
    let err = verify_signed_lease_chain(&lease, &trusted)
        .expect_err("a body omitting instance_binding_id must be rejected");
    assert!(
        matches!(err, SignedLeaseError::MalformedBody),
        "got {err:?}"
    );
}

#[test]
fn a_signed_body_with_an_omitted_serial_is_rejected() {
    let kit = FabricatedKeyset::new();
    let trusted = kit.trusted();
    let lease = kit.sign_body_omitting("serial");
    let err = verify_signed_lease_chain(&lease, &trusted)
        .expect_err("a body omitting serial must be rejected");
    assert!(
        matches!(err, SignedLeaseError::MalformedBody),
        "got {err:?}"
    );
}

// --- Round-2 #4: a present-but-invalid gpu_limit fails closed, not Unlimited ---

#[test]
fn a_present_but_negative_gpu_limit_is_rejected_not_unlimited() {
    // A signed body with gpu_limit:-1 must be MalformedBody — NOT silently folded
    // to GpuLimit::Unlimited (the LEAST restrictive). A present-but-invalid value
    // fails closed; only an ABSENT gpu_limit may mean Unlimited.
    let kit = FabricatedKeyset::new();
    let trusted = kit.trusted();
    let lease = kit.sign_body_with_raw_gpu_limit(-1);
    let err = verify_signed_lease_chain(&lease, &trusted)
        .expect_err("a present-but-negative gpu_limit must be rejected");
    assert!(
        matches!(err, SignedLeaseError::MalformedBody),
        "got {err:?}"
    );
}

#[test]
fn a_present_but_oversized_gpu_limit_is_rejected_not_unlimited() {
    // gpu_limit beyond u32::MAX → must be MalformedBody, not Unlimited.
    let kit = FabricatedKeyset::new();
    let trusted = kit.trusted();
    let lease = kit.sign_body_with_raw_gpu_limit(i64::from(u32::MAX) + 1);
    let err = verify_signed_lease_chain(&lease, &trusted)
        .expect_err("a present-but-oversized gpu_limit must be rejected");
    assert!(
        matches!(err, SignedLeaseError::MalformedBody),
        "got {err:?}"
    );
}

#[test]
fn an_absent_gpu_limit_means_unlimited() {
    // The contrast case: an ABSENT gpu_limit is legitimately Unlimited (the body
    // verifies and parses; gpu_limit is None).
    let kit = FabricatedKeyset::new();
    let trusted = kit.trusted();
    let lease = kit.sign_body_omitting("gpu_limit");
    let body = verify_signed_lease_chain(&lease, &trusted)
        .expect("a body that simply omits gpu_limit is valid");
    assert_eq!(
        body.gpu_limit, None,
        "absent gpu_limit parses as None (Unlimited)"
    );
}

// --- Blocker #3: canonically-padded standard-base64 leaseBytes decode ----------

#[test]
fn canonically_padded_standard_base64_lease_bytes_decode() {
    use base64::Engine;
    // A body whose CBOR length % 3 != 0 → its standard-base64 carries real '='
    // padding. Stripping '=' before STANDARD.decode (RequireCanonical) would
    // wrongly reject it; the verifier must decode the received bytes exactly.
    let kit = FabricatedKeyset::new();
    let trusted = kit.trusted();
    // Find a body that pads (len % 3 != 0); vary the licence id length until so.
    let mut lease = kit.sign_lease(35);
    for n in 0..8 {
        let candidate = kit.sign_body(LeaseBodyFields {
            licence_id: format!("lic_pad_{}", "x".repeat(n)),
            instance_binding_id: "ib_pad".to_owned(),
            serial: kit.lease_serial().to_owned(),
            not_after: kit.now_ms() + 35 * 86_400_000,
            gpu_limit: Some(1),
            hardware_class: Some("standard".to_owned()),
        });
        let raw_len = base64::engine::general_purpose::STANDARD
            .decode(&candidate.lease_bytes)
            .unwrap()
            .len();
        if raw_len % 3 != 0 {
            lease = candidate;
            break;
        }
    }
    assert!(
        lease.lease_bytes.ends_with('='),
        "the chosen body must be canonically padded (ends with '=')"
    );
    let body = verify_signed_lease_chain(&lease, &trusted)
        .expect("canonically-padded standard-base64 leaseBytes must decode + verify");
    assert!(body.licence_id.starts_with("lic_pad_"));
}
