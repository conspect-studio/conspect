//! RFC 8032 §7.1 Ed25519 known-answer tests (task #13, #228 review residual).
//!
//! The canonical `(seed → public key → signature)` vectors every Ed25519
//! implementation MUST reproduce. This pins the Conspect signature primitive
//! (`ed25519-dalek` v3, ADR-I010) to the standard, so a future dependency bump that
//! silently changed the curve arithmetic or signature encoding — which would break
//! every already-issued licence and mesh-announcement signature — fails CI here
//! instead of in the field. Raw-primitive level by design: it guards the dependency,
//! not the (in-tree) Conspect wrapper formats, which their own round-trip tests cover.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};

/// Decode a fixed-length lowercase-hex string into bytes (test-only helper; avoids a
/// `hex` dev-dependency for the frozen vectors).
fn unhex<const N: usize>(s: &str) -> [u8; N] {
    assert_eq!(s.len(), N * 2, "hex string length must be 2·N");
    let mut out = [0u8; N];
    for (i, slot) in out.iter_mut().enumerate() {
        *slot = u8::from_str_radix(&s[2 * i..2 * i + 2], 16).expect("valid hex byte");
    }
    out
}

/// Decode a variable-length lowercase-hex message (`""` → empty).
fn message_bytes(hex: &str) -> Vec<u8> {
    assert_eq!(hex.len() % 2, 0, "hex message must be even length");
    (0..hex.len() / 2)
        .map(|i| u8::from_str_radix(&hex[2 * i..2 * i + 2], 16).expect("valid hex byte"))
        .collect()
}

/// One RFC 8032 §7.1 Ed25519 test vector (all fields lowercase hex).
struct Kat {
    secret: &'static str,    // 32-byte seed
    public: &'static str,    // 32-byte public key
    message: &'static str,   // message ("" = empty)
    signature: &'static str, // 64-byte signature
}

/// RFC 8032 (`EdDSA`) §7.1 — Test Vectors for Ed25519 (TEST 1, TEST 2, TEST 3).
/// Source: <https://www.rfc-editor.org/rfc/rfc8032#section-7.1>.
const RFC8032_ED25519: &[Kat] = &[
    Kat {
        secret: "9d61b19deffd5a60ba844af492ec2cc44449c5697b326919703bac031cae7f60",
        public: "d75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511a",
        message: "",
        signature: "e5564300c360ac729086e2cc806e828a84877f1eb8e5d974d873e065224901555fb8821590a33bacc61e39701cf9b46bd25bf5f0595bbe24655141438e7a100b",
    },
    Kat {
        secret: "4ccd089b28ff96da9db6c346ec114e0f5b8a319f35aba624da8cf6ed4fb8a6fb",
        public: "3d4017c3e843895a92b70aa74d1b7ebc9c982ccf2ec4968cc0cd55f12af4660c",
        message: "72",
        signature: "92a009a9f0d4cab8720e820b5f642540a2b27b5416503f8fb3762223ebdb69da085ac1e43e15996e458f3613d0f11d8c387b2eaeb4302aeeb00d291612bb0c00",
    },
    Kat {
        secret: "c5aa8df43f9f837bedb7442f31dcb7b166d38535076f094b85ce3a2e0b4458f7",
        public: "fc51cd8e6218a1a38da47ed00230f0580816ed13ba3303ac5deb911548908025",
        message: "af82",
        signature: "6291d657deec24024827e69c3abe01a30ce548a284743a445e3680d7db5ac3ac18ff9b538d16f290ae67f760984dc6594a7c15e9716ed28dc027beceea1ec40a",
    },
];

/// Every RFC 8032 vector round-trips exactly: seed derives the RFC public key,
/// signing yields the RFC signature byte-for-byte, and that signature verifies
/// (`verify_strict`, the mode Conspect uses). A dependency bump that altered the
/// primitive would break at least one exact-bytes assertion.
#[test]
fn ed25519_matches_rfc8032_known_answer_vectors() {
    for (idx, kat) in RFC8032_ED25519.iter().enumerate() {
        let test = idx + 1;
        let seed = unhex::<32>(kat.secret);
        let expected_public = unhex::<32>(kat.public);
        let expected_sig = unhex::<64>(kat.signature);
        let message = message_bytes(kat.message);

        let sk = SigningKey::from_bytes(&seed);

        assert_eq!(
            sk.verifying_key().to_bytes(),
            expected_public,
            "TEST {test}: derived public key must match RFC 8032"
        );
        assert_eq!(
            sk.sign(&message).to_bytes(),
            expected_sig,
            "TEST {test}: signature must match RFC 8032 byte-for-byte"
        );

        let vk = VerifyingKey::from_bytes(&expected_public).expect("valid public key");
        let rfc_sig = Signature::from_bytes(&expected_sig);
        vk.verify_strict(&message, &rfc_sig)
            .unwrap_or_else(|_| panic!("TEST {test}: RFC signature must verify_strict"));
    }
}

/// The verifier rejects a tampered signature and a signature presented over a
/// different message — the accept/reject boundary Conspect relies on.
#[test]
fn ed25519_rejects_tampered_signature_and_message() {
    let kat = &RFC8032_ED25519[2]; // the 2-byte-message vector
    let expected_public = unhex::<32>(kat.public);
    let expected_sig = unhex::<64>(kat.signature);
    let message = message_bytes(kat.message);
    let vk = VerifyingKey::from_bytes(&expected_public).expect("valid public key");

    // A one-bit flip in the signature must not verify.
    let mut tampered = expected_sig;
    tampered[0] ^= 0x01;
    assert!(
        vk.verify_strict(&message, &Signature::from_bytes(&tampered))
            .is_err(),
        "a tampered signature must not verify"
    );

    // The valid signature must not verify against a modified message.
    let good_sig = Signature::from_bytes(&expected_sig);
    let mut other = message.clone();
    other.push(0xFF);
    assert!(
        vk.verify_strict(&other, &good_sig).is_err(),
        "a signature must not verify against a modified message"
    );
}
