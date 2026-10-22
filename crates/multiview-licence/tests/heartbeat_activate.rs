//! CONSPECT device ACTIVATE / enrolment (ADR-I008) — the first-contact
//! registration slice that complements the renew-only heartbeat path.
//!
//! These tests pin the PURE wire + crypto the leaf crate owns for activate
//! (mirroring `heartbeat_pop.rs` for the renew path):
//!   * `DeviceChallenge` carries the server-assigned `instanceId` (v0.16.0);
//!   * `build_activate_request` echoes that `instanceId` into
//!     `ActivateRequest.instanceId`, carries `devicePublicKey` (base64url raw-32),
//!     and NEVER carries the deprecated `serverNonce`;
//!   * the activate `Conspect-Device-PoP` proof BINDS the server-assigned
//!     `instanceId` (NOT the device's own `instance_id`) into the canonical
//!     pre-image, so on first contact the proof, the binding, and the lease share
//!     ONE server id.
//!
//! There is no live Conspect server here, so the proof is verified the only way it
//! can be locally: build it with a KNOWN device key, then INDEPENDENTLY VERIFY the
//! produced COSE_Sign1 against the matching PUBLIC key over the exact reconstructed
//! pre-image (an honest self-check — NOT live-server validation, which is the
//! ADR-I008 rule-26 follow-up the operator runs against a real org JWT + an
//! activate-registered binding).
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
    build_activate_request, canonical_pop_preimage, pop_activate_header_value, ActivateRequest,
    DeviceChallenge, DeviceIdentity, DeviceSigner,
};

/// A fixed, deterministic device signer over a known Ed25519 seed (mirrors
/// `heartbeat_pop.rs`): build a proof AND verify it against the public key.
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

/// The server-assigned durable instance id (`ib_<uuidv7>`, ADR-0015) the challenge
/// reserves with the nonce; a first-contact device enrols under THIS id.
const SERVER_INSTANCE_ID: &str = "ib_01931d4e7b2a7c009f3a2b6e1c0e9b41";
/// The device's OWN configured instance id — distinct from the server-assigned one,
/// so the test can prove the activate proof binds the SERVER id, not this one.
const DEVICE_INSTANCE_ID: &str = "inst_2b6e1c0e9b41";
const NONCE_HEX: &str = "9f3a2b6e1c0e9b41a4c92d7e3b8f10c25e6a7d3f49b0c81e2f5a6d7c8b9e0f1a";

fn enrolling_identity() -> DeviceIdentity {
    DeviceIdentity {
        machine_id: "mch_7c2a1f04c9e75031".to_owned(),
        instance_id: DEVICE_INSTANCE_ID.to_owned(),
        binding_id: None, // a fresh, un-bound device
        fingerprint_digest: "1".repeat(64),
        fingerprint_score: 95,
        hardware_digest: "hwd_2b6e1c0e9b41".to_owned(),
        instance_discriminator_hash: "disc_9f3a2b6e1c0e".to_owned(),
        instance_discriminator_digest: "2".repeat(64),
        app_version: "0.1.0-test".to_owned(),
        device_public_key_b64url: String::new(), // the signer supplies it on activate
    }
}

#[test]
fn device_challenge_deserializes_the_server_assigned_instance_id() {
    // v0.16.0 added a REQUIRED `instanceId` to DeviceChallenge. The leaf must parse
    // it (serde camelCase) — the renew path ignores it, enrolment consumes it.
    let json = format!(
        r#"{{"nonce":"{NONCE_HEX}","expiresAtMs":1784023648420,"instanceId":"{SERVER_INSTANCE_ID}"}}"#
    );
    let chal: DeviceChallenge = serde_json::from_str(&json).expect("challenge must deserialize");
    assert_eq!(chal.nonce, NONCE_HEX);
    assert_eq!(chal.expires_at_ms, 1_784_023_648_420);
    assert_eq!(
        chal.instance_id, SERVER_INSTANCE_ID,
        "the server-assigned instanceId must be parsed"
    );
}

#[test]
fn build_activate_request_echoes_the_server_instance_id_not_the_device_one() {
    // On first contact the device ECHOES the server-assigned challenge.instanceId
    // as ActivateRequest.instanceId — never its own DeviceIdentity::instance_id.
    let signer = FixedDeviceSigner::from_seed([42u8; 32]);
    let challenge = DeviceChallenge::new(
        NONCE_HEX.to_owned(),
        i64::MAX,
        SERVER_INSTANCE_ID.to_owned(),
    );
    let req: ActivateRequest = build_activate_request(
        &enrolling_identity(),
        signer.public_key_raw(),
        &challenge,
        None,
    );
    assert_eq!(
        req.instance_id, SERVER_INSTANCE_ID,
        "activate must echo the SERVER-assigned instanceId"
    );
    assert_ne!(
        req.instance_id, DEVICE_INSTANCE_ID,
        "activate must NOT send the device's own instance_id as instanceId"
    );
    assert_eq!(req.nonce, NONCE_HEX, "activate carries the challenge nonce");
}

#[test]
fn build_activate_request_carries_device_public_key_base64url_raw_32() {
    // devicePublicKey is the base64url (no-pad) of the signer's raw 32-byte Ed25519
    // public point — sourced from the SIGNER, not the (empty) identity env field.
    let signer = FixedDeviceSigner::from_seed([7u8; 32]);
    let pk_raw = signer.public_key_raw();
    let challenge = DeviceChallenge::new(
        NONCE_HEX.to_owned(),
        i64::MAX,
        SERVER_INSTANCE_ID.to_owned(),
    );
    let req = build_activate_request(&enrolling_identity(), pk_raw, &challenge, None);

    let want = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(pk_raw);
    assert_eq!(
        req.device_public_key, want,
        "devicePublicKey must be base64url(raw-32 Ed25519 public)"
    );
    // It round-trips back to the same 32 bytes.
    let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(req.device_public_key.as_bytes())
        .expect("devicePublicKey must be valid base64url");
    assert_eq!(decoded.as_slice(), pk_raw.as_slice());
}

#[test]
fn activate_request_never_serializes_the_deprecated_server_nonce() {
    // serverNonce is DEPRECATED (do not send). The serialized ActivateRequest must
    // contain no `serverNonce` key at all.
    let signer = FixedDeviceSigner::from_seed([9u8; 32]);
    let challenge = DeviceChallenge::new(
        NONCE_HEX.to_owned(),
        i64::MAX,
        SERVER_INSTANCE_ID.to_owned(),
    );
    let req = build_activate_request(
        &enrolling_identity(),
        signer.public_key_raw(),
        &challenge,
        None,
    );
    let json = serde_json::to_string(&req).expect("serialize");
    assert!(
        !json.contains("serverNonce"),
        "the deprecated serverNonce must never be serialized: {json}"
    );
    // The required camelCase fields ARE present.
    for field in [
        "machineId",
        "fingerprintDigest",
        "fingerprintScore",
        "hardwareDigest",
        "instanceId",
        "instanceDiscriminatorHash",
        "instanceDiscriminatorDigest",
        "devicePublicKey",
        "nonce",
    ] {
        assert!(
            json.contains(field),
            "activate body must carry {field}: {json}"
        );
    }
}

#[test]
fn activate_request_omits_claim_code_for_the_free_tier() {
    // Omitting claimCode auto-issues a free non-commercial licence — so a None claim
    // code must NOT serialize the key at all (not an empty string).
    let signer = FixedDeviceSigner::from_seed([11u8; 32]);
    let challenge = DeviceChallenge::new(
        NONCE_HEX.to_owned(),
        i64::MAX,
        SERVER_INSTANCE_ID.to_owned(),
    );
    let free = build_activate_request(
        &enrolling_identity(),
        signer.public_key_raw(),
        &challenge,
        None,
    );
    let json_free = serde_json::to_string(&free).expect("serialize");
    assert!(
        !json_free.contains("claimCode"),
        "an omitted claim code must not serialize claimCode: {json_free}"
    );
    // A paid claim code IS serialized when present.
    let paid = build_activate_request(
        &enrolling_identity(),
        signer.public_key_raw(),
        &challenge,
        Some("K7M3PQ"),
    );
    let json_paid = serde_json::to_string(&paid).expect("serialize");
    assert!(
        json_paid.contains("\"claimCode\":\"K7M3PQ\""),
        "a present claim code must serialize: {json_paid}"
    );
}

#[test]
fn the_activate_pop_proof_binds_the_server_instance_id() {
    // THE correctness keystone: on first contact the PoP pre-image's `instance_id`
    // must be the SERVER-assigned challenge.instanceId — so the proof, the binding,
    // and the lease share one server id. (A proof bound to the device's own
    // instance_id would not match the binding the server reserved → pop-invalid.)
    let signer = FixedDeviceSigner::from_seed([42u8; 32]);
    let htu = "https://api.conspect.studio/v0/organisations/org_test/activate";
    // The activate body bytes the transport will send (the proof binds sha256 of these).
    let challenge = DeviceChallenge::new(
        NONCE_HEX.to_owned(),
        i64::MAX,
        SERVER_INSTANCE_ID.to_owned(),
    );
    let req = build_activate_request(
        &enrolling_identity(),
        signer.public_key_raw(),
        &challenge,
        None,
    );
    let body = serde_json::to_vec(&req).expect("serialize");
    let iat: i64 = 1_790_000_000;

    // The header is base64(COSE_Sign1) over the canonical pre-image, with the
    // server-assigned instance id bound in.
    let header =
        pop_activate_header_value(&signer, htu, &body, SERVER_INSTANCE_ID, NONCE_HEX, iat).unwrap();
    let cose_bytes = base64::engine::general_purpose::STANDARD
        .decode(&header)
        .expect("standard base64");
    use coset::{CborSerializable as _, CoseSign1};
    let sign1 = CoseSign1::from_slice(&cose_bytes).expect("valid COSE_Sign1");

    // The payload IS the canonical pre-image bound to the SERVER instance id.
    let expected_server =
        canonical_pop_preimage("POST", htu, &body, SERVER_INSTANCE_ID, NONCE_HEX, iat).unwrap();
    assert_eq!(
        sign1.payload.as_deref(),
        Some(expected_server.as_slice()),
        "the activate proof must bind the SERVER-assigned instance_id"
    );
    // And NOT the device's own instance id.
    let with_device_id =
        canonical_pop_preimage("POST", htu, &body, DEVICE_INSTANCE_ID, NONCE_HEX, iat).unwrap();
    assert_ne!(
        sign1.payload.as_deref(),
        Some(with_device_id.as_slice()),
        "the activate proof must NOT bind the device's own instance_id"
    );

    // The produced COSE_Sign1 verifies against the device PUBLIC key (self-check).
    let vk = signer.verifying_key();
    sign1
        .verify_signature(b"", |sig, tbs| {
            let signature = ed25519_dalek::Signature::from_slice(sig)
                .map_err(|e| format!("bad sig bytes: {e}"))?;
            vk.verify_strict(tbs, &signature)
                .map_err(|e| format!("verify failed: {e}"))
        })
        .expect("the activate COSE_Sign1 must verify against the device public key");
}
