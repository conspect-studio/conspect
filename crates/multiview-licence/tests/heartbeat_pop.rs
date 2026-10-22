//! CONSPECT-3 device proof-of-possession (PoP) — the byte-exact COSE_Sign1 +
//! canonical pre-image (ADR-I007, Conspect API v0.9.0 device-PoP wire).
//!
//! These tests pin the PURE crypto the leaf crate owns: the canonical PoP
//! pre-image `htm | htu | sha256(body) | instance_id | nonce | iat` (a
//! deterministic-CBOR map, mirroring `canonical_key_preimage`), and the
//! `Conspect-Device-PoP` header value — a base64 COSE_Sign1 the device signs over
//! that pre-image with its Ed25519 device key.
//!
//! There is no live Conspect server here, so the proof is verified the only way it
//! can be locally: we build it with a KNOWN device key, then INDEPENDENTLY VERIFY
//! the produced COSE_Sign1's signature against the matching PUBLIC key over the
//! exact reconstructed pre-image (an honest self-check — NOT a claim of live-server
//! validation, which is the ADR-I007 rule-26 follow-up the operator runs).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::as_conversions,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::doc_markdown,
    clippy::items_after_statements,
    clippy::missing_panics_doc
)]
#![cfg(feature = "heartbeat")]

use base64::Engine as _;
use ed25519_dalek::{Signer as _, SigningKey, VerifyingKey};
use multiview_licence::heartbeat::{
    canonical_pop_preimage, pop_header_value, DeviceSigner, PopError,
};

/// A fixed, deterministic device signer over a known Ed25519 seed — lets a test
/// build a PoP proof AND verify it against the matching public key. The
/// production `DeviceSigner` is the cli's persisted keypair; this stands in for it.
struct FixedDeviceSigner {
    key: SigningKey,
}

impl FixedDeviceSigner {
    fn from_seed(seed: [u8; 32]) -> Self {
        Self {
            key: SigningKey::from_bytes(&seed),
        }
    }
    fn verifying_key(&self) -> VerifyingKey {
        self.key.verifying_key()
    }
}

impl DeviceSigner for FixedDeviceSigner {
    fn public_key_raw(&self) -> [u8; 32] {
        self.key.verifying_key().to_bytes()
    }
    fn sign(&self, message: &[u8]) -> [u8; 64] {
        self.key.sign(message).to_bytes()
    }
}

/// A small hand-rolled CBOR reader for asserting the pre-image's exact structure
/// (we re-derive the bytes the encoder MUST produce; this is the byte-exact pin,
/// independent of the encoder under test).
fn cbor_head(major: u8, n: u64) -> Vec<u8> {
    let mt = major << 5;
    if n < 24 {
        vec![mt | (n as u8)]
    } else if let Ok(b) = u8::try_from(n) {
        vec![mt | 0x18, b]
    } else if let Ok(b) = u16::try_from(n) {
        let mut v = vec![mt | 0x19];
        v.extend_from_slice(&b.to_be_bytes());
        v
    } else if let Ok(b) = u32::try_from(n) {
        let mut v = vec![mt | 0x1a];
        v.extend_from_slice(&b.to_be_bytes());
        v
    } else {
        let mut v = vec![mt | 0x1b];
        v.extend_from_slice(&n.to_be_bytes());
        v
    }
}
fn tstr(s: &str) -> Vec<u8> {
    let mut v = cbor_head(3, s.len() as u64);
    v.extend_from_slice(s.as_bytes());
    v
}
fn bstr(b: &[u8]) -> Vec<u8> {
    let mut v = cbor_head(2, b.len() as u64);
    v.extend_from_slice(b);
    v
}
fn uint(n: u64) -> Vec<u8> {
    cbor_head(0, n)
}

fn sha256(bytes: &[u8]) -> [u8; 32] {
    use sha2::{Digest as _, Sha256};
    let mut h = Sha256::new();
    h.update(bytes);
    h.finalize().into()
}

// A valid 64-char (32-byte) lower-case-hex nonce. NOTE: the spec's own DeviceChallenge
// `example` value is 65 chars — it violates its own `^[0-9a-f]{64}$` pattern (the spec
// examples are illustrative/unreliable, like the `g1gg…` header example). We use a
// genuinely-64-char value so the test exercises the real wire shape.
const NONCE_HEX: &str = "9f3a2b6e1c0e9b41a4c92d7e3b8f10c25e6a7d3f49b0c81e2f5a6d7c8b9e0f1a";

#[test]
fn the_pop_pre_image_is_a_byte_exact_canonical_cbor_map() {
    // The pre-image is a deterministic-CBOR map(6) over the spec's field order:
    // htm | htu | sha256(body) | instance_id | nonce | iat (ADR-I007).
    let htm = "POST";
    let htu = "https://api.conspect.studio/v0/organisations/org_test/heartbeat";
    let body = br#"{"bindingId":"ib_fab_0001"}"#;
    let instance_id = "inst_2b6e1c0e9b41";
    let nonce_raw = hex_to_32(NONCE_HEX);
    let iat: i64 = 1_790_000_000;

    let got = canonical_pop_preimage(htm, htu, body, instance_id, NONCE_HEX, iat).unwrap();

    // Re-derive the exact bytes the encoder MUST emit (map(6), canonical fields).
    let mut want = Vec::new();
    want.extend_from_slice(&cbor_head(5, 6)); // map(6)
    want.extend_from_slice(&tstr("htm"));
    want.extend_from_slice(&tstr(htm));
    want.extend_from_slice(&tstr("htu"));
    want.extend_from_slice(&tstr(htu));
    want.extend_from_slice(&tstr("sha256_body"));
    want.extend_from_slice(&bstr(&sha256(body)));
    want.extend_from_slice(&tstr("instance_id"));
    want.extend_from_slice(&tstr(instance_id));
    want.extend_from_slice(&tstr("nonce"));
    want.extend_from_slice(&bstr(&nonce_raw));
    want.extend_from_slice(&tstr("iat"));
    want.extend_from_slice(&uint(iat as u64));

    assert_eq!(
        got, want,
        "the PoP pre-image must be the byte-exact canonical CBOR the server recomputes"
    );
}

#[test]
fn the_pop_pre_image_carries_the_raw_32_byte_nonce_not_the_hex_text() {
    // The nonce in the pre-image is the 32 RAW bytes decoded from the 64-hex
    // challenge, as a CBOR byte string — never the 64-char hex text.
    let pre = canonical_pop_preimage(
        "POST",
        "https://h/v0/x",
        b"{}",
        "inst_x",
        NONCE_HEX,
        1_790_000_000,
    )
    .unwrap();
    let raw = hex_to_32(NONCE_HEX);
    // The 32 raw bytes appear; the 64-char hex string does not.
    assert!(
        windows_contains(&pre, &raw),
        "the raw 32-byte nonce must be embedded"
    );
    assert!(
        !windows_contains(&pre, NONCE_HEX.as_bytes()),
        "the hex TEXT of the nonce must NOT be embedded (it is decoded to raw bytes)"
    );
}

#[test]
fn a_non_hex_nonce_is_rejected_not_silently_mis_encoded() {
    // A nonce that is not 64 lower-case hex is a hard error (fail closed) — never a
    // silently truncated/zero-padded pre-image.
    let err = canonical_pop_preimage("POST", "https://h/v0/x", b"{}", "inst_x", "not-hex", 1).err();
    assert!(matches!(err, Some(PopError::Nonce(_))));

    let err_short =
        canonical_pop_preimage("POST", "https://h/v0/x", b"{}", "inst_x", "abcd", 1).err();
    assert!(matches!(err_short, Some(PopError::Nonce(_))));
}

#[test]
fn the_pop_header_is_a_self_verifiable_cose_sign1_over_the_pre_image() {
    // The header value is base64(COSE_Sign1). Decode it, and verify the COSE
    // signature against the device PUBLIC key over the reconstructed pre-image —
    // exactly the check an independent verifier (the server) performs. This is an
    // honest self-check, NOT live-server validation (ADR-I007 rule-26 follow-up).
    let signer = FixedDeviceSigner::from_seed([42u8; 32]);
    let htm = "POST";
    let htu = "https://api.conspect.studio/v0/organisations/org_test/heartbeat";
    let body = br#"{"bindingId":"ib_fab_0001","nonce":"..."}"#;
    let instance_id = "inst_2b6e1c0e9b41";
    let iat: i64 = 1_790_000_000;

    let header = pop_header_value(&signer, htm, htu, body, instance_id, NONCE_HEX, iat).unwrap();

    // The header decodes as STANDARD base64 (RFC 4648 §4).
    let cose_bytes = base64::engine::general_purpose::STANDARD
        .decode(&header)
        .expect("the PoP header must be standard base64");

    // It parses as an (untagged) COSE_Sign1.
    use coset::{CborSerializable as _, CoseSign1};
    let sign1 = CoseSign1::from_slice(&cose_bytes).expect("a valid COSE_Sign1");

    // The expected pre-image (what the server recomputes from the request).
    let expected_pre = canonical_pop_preimage(htm, htu, body, instance_id, NONCE_HEX, iat).unwrap();
    // The COSE payload IS the pre-image (attached).
    assert_eq!(
        sign1.payload.as_deref(),
        Some(expected_pre.as_slice()),
        "the COSE_Sign1 payload must be the canonical pre-image"
    );

    // Verify the signature over the COSE Sig_structure against the device key.
    let vk = signer.verifying_key();
    sign1
        .verify_signature(b"", |sig, tbs| {
            let signature = ed25519_dalek::Signature::from_slice(sig)
                .map_err(|e| format!("bad sig bytes: {e}"))?;
            vk.verify_strict(tbs, &signature)
                .map_err(|e| format!("sig verify failed: {e}"))
        })
        .expect("the produced COSE_Sign1 must verify against the device public key");
}

#[test]
fn the_pop_header_protected_alg_is_eddsa() {
    // The protected header pins alg = EdDSA (-8) so the server selects Ed25519.
    let signer = FixedDeviceSigner::from_seed([7u8; 32]);
    let header = pop_header_value(
        &signer,
        "POST",
        "https://h/v0/x",
        b"{}",
        "inst_x",
        NONCE_HEX,
        1_790_000_000,
    )
    .unwrap();
    let cose_bytes = base64::engine::general_purpose::STANDARD
        .decode(&header)
        .unwrap();
    use coset::iana;
    use coset::{Algorithm, CborSerializable as _, CoseSign1};
    let sign1 = CoseSign1::from_slice(&cose_bytes).unwrap();
    assert_eq!(
        sign1.protected.header.alg,
        Some(Algorithm::Assigned(iana::Algorithm::EdDSA)),
        "the protected header must pin alg = EdDSA (-8)"
    );
}

#[test]
fn a_changed_body_changes_the_proof_no_signature_reuse() {
    // The body hash is bound into the pre-image, so two different bodies produce
    // different proofs — a captured proof cannot be replayed onto a different body.
    let signer = FixedDeviceSigner::from_seed([9u8; 32]);
    let a = pop_header_value(
        &signer,
        "POST",
        "https://h/v0/x",
        br#"{"a":1}"#,
        "inst_x",
        NONCE_HEX,
        1_790_000_000,
    )
    .unwrap();
    let b = pop_header_value(
        &signer,
        "POST",
        "https://h/v0/x",
        br#"{"a":2}"#,
        "inst_x",
        NONCE_HEX,
        1_790_000_000,
    )
    .unwrap();
    assert_ne!(a, b, "a different body must yield a different PoP proof");
}

// --- helpers -------------------------------------------------------------------

fn hex_to_32(s: &str) -> [u8; 32] {
    let bytes = hex::decode(s).expect("valid hex");
    let mut out = [0u8; 32];
    out.copy_from_slice(&bytes);
    out
}

fn windows_contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}
