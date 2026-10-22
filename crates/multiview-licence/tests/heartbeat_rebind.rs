//! CONSPECT device REBIND + DEACTIVATE lifecycle (ADR-I009) — the PURE wire +
//! crypto the leaf crate owns (mirroring `heartbeat_activate.rs` for activate).
//!
//! These tests pin:
//!   * `RebindRequest` / `DeactivateRequest` serialise camelCase, carry the
//!     required fields, and NEVER carry `devicePublicKey` (continuity — the server
//!     verifies the STORED bound key; `devicePublicKey` is activate-only);
//!   * `build_rebind_request` / `build_deactivate_request` carry the device's OWN
//!     `instance_id` / `binding_id` (NOT a server-assigned challenge id) + the
//!     challenge nonce;
//!   * the rebind / deactivate `Conspect-Device-PoP` proof BINDS the device's OWN
//!     `instance_id` into the canonical pre-image (continuity), and verifies against
//!     the device public key;
//!   * the responses deserialise (`RebindResponse` carries only `leaseSerial`, no
//!     embedded signed lease; the deactivate response is a device-local projection
//!     of `InstanceBinding` with `lifecycleState`).
//!
//! There is no live Conspect server here, so the proof is verified the only way it
//! can be locally: build it with a KNOWN device key, then INDEPENDENTLY VERIFY the
//! produced COSE_Sign1 against the matching PUBLIC key over the exact reconstructed
//! pre-image (an honest self-check — NOT live-server validation, the ADR-I009
//! rule-26 follow-up the operator runs against a real org JWT + a bound instance).
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
    build_deactivate_request, build_rebind_request, canonical_pop_preimage,
    pop_deactivate_header_value, pop_rebind_header_value, DeactivateRequest, DeviceChallenge,
    DeviceIdentity, DeviceSigner, RebindRequest, RebindResponse,
};

/// A fixed, deterministic device signer over a known Ed25519 seed (mirrors
/// `heartbeat_activate.rs`): build a proof AND verify it against the public key.
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

/// The device's OWN established binding id (the seat-consuming, lease-bearing unit
/// it already holds — rebind/deactivate act on THIS, continuity).
const DEVICE_BINDING_ID: &str = "ib_01931d4e7b2a7c009f3a2b6e1c0e9b41";
/// The device's OWN durable instance id (ADR-0015) — the continuity id the proof
/// binds, distinct from any server-assigned challenge id.
const DEVICE_INSTANCE_ID: &str = "inst_prod_a";
/// The licence the binding draws its seat from (rebind needs it; deactivate does not).
const LICENCE_ID: &str = "lic_8d3b2a1f04c9e750";
/// A server-assigned challenge instanceId — present on the challenge but IGNORED by
/// the continuity ops (the test proves the proof does NOT bind this).
const CHALLENGE_INSTANCE_ID: &str = "ib_server_assigned_should_be_ignored";
const NONCE_HEX: &str = "9f3a2b6e1c0e9b41a4c92d7e3b8f10c25e6a7d3f49b0c81e2f5a6d7c8b9e0f1a";

/// An ALREADY-BOUND device identity (it holds `DEVICE_BINDING_ID`) — the rebind /
/// deactivate case. The fingerprint score is the POST-rebind self-match (≥ 70) the
/// new hardware reports against the refreshed binding (ADR-I009 §3 handoff).
fn bound_identity() -> DeviceIdentity {
    DeviceIdentity {
        machine_id: "mch_7c2a1f04c9e75031".to_owned(),
        instance_id: DEVICE_INSTANCE_ID.to_owned(),
        binding_id: Some(DEVICE_BINDING_ID.to_owned()),
        fingerprint_digest: "a".repeat(64),
        fingerprint_score: 95,
        hardware_digest: "hwd_2b6e1c0e9b41".to_owned(),
        instance_discriminator_hash: "disc_9f3a2b6e1c0e".to_owned(),
        instance_discriminator_digest: "2".repeat(64),
        app_version: "0.1.0-test".to_owned(),
        device_public_key_b64url: String::new(),
    }
}

fn challenge() -> DeviceChallenge {
    DeviceChallenge::new(
        NONCE_HEX.to_owned(),
        i64::MAX,
        CHALLENGE_INSTANCE_ID.to_owned(),
    )
}

#[test]
fn build_rebind_request_carries_the_devices_own_ids_and_the_nonce() {
    // Rebind is a CONTINUITY op: it sends the device's OWN binding/instance id +
    // licence id + the refreshed fingerprint material + the challenge nonce.
    let req: RebindRequest = build_rebind_request(&bound_identity(), LICENCE_ID, &challenge());
    assert_eq!(
        req.binding_id, DEVICE_BINDING_ID,
        "rebind sends the device's own binding"
    );
    assert_eq!(
        req.instance_id, DEVICE_INSTANCE_ID,
        "rebind sends the device's OWN instance_id (continuity), not a challenge id"
    );
    assert_ne!(
        req.instance_id, CHALLENGE_INSTANCE_ID,
        "rebind must NOT send the server-assigned challenge instanceId"
    );
    assert_eq!(req.licence_id, LICENCE_ID);
    assert_eq!(
        req.fp_score, 95,
        "the refreshed fingerprint score is reported"
    );
    assert_eq!(req.nonce, NONCE_HEX, "rebind carries the challenge nonce");
}

#[test]
fn rebind_request_never_serializes_device_public_key() {
    // Continuity: the server verifies the STORED bound key, so RebindRequest carries
    // NO devicePublicKey (that field belongs to activate only).
    let req = build_rebind_request(&bound_identity(), LICENCE_ID, &challenge());
    let json = serde_json::to_string(&req).expect("serialize");
    assert!(
        !json.contains("devicePublicKey"),
        "rebind must NOT carry devicePublicKey (continuity): {json}"
    );
    // The required camelCase fields ARE present.
    for field in [
        "licenceId",
        "bindingId",
        "instanceId",
        "instanceDiscriminatorHash",
        "fingerprintDigest",
        "fpScore",
        "nonce",
    ] {
        assert!(
            json.contains(field),
            "rebind body must carry {field}: {json}"
        );
    }
}

#[test]
fn build_deactivate_request_carries_only_binding_and_nonce() {
    let req: DeactivateRequest = build_deactivate_request(DEVICE_BINDING_ID, NONCE_HEX);
    assert_eq!(req.binding_id, DEVICE_BINDING_ID);
    assert_eq!(req.nonce, NONCE_HEX);
    let json = serde_json::to_string(&req).expect("serialize");
    assert!(
        !json.contains("devicePublicKey"),
        "deactivate must NOT carry devicePublicKey (continuity): {json}"
    );
    assert!(json.contains("bindingId") && json.contains("nonce"));
}

#[test]
fn the_rebind_pop_proof_binds_the_devices_own_instance_id() {
    // THE correctness keystone for rebind: the PoP pre-image's `instance_id` must be
    // the device's OWN durable id (continuity) — NOT the challenge's server-assigned
    // id (which is for first-contact activate). A proof bound to the wrong id is
    // pop-invalid against the existing binding.
    let signer = FixedDeviceSigner::from_seed([42u8; 32]);
    let htu = "https://api.conspect.studio/v0/organisations/org_test/rebind";
    let req = build_rebind_request(&bound_identity(), LICENCE_ID, &challenge());
    let body = serde_json::to_vec(&req).expect("serialize");
    let iat: i64 = 1_790_000_000;

    let header = pop_rebind_header_value(&signer, htu, &body, DEVICE_INSTANCE_ID, NONCE_HEX, iat)
        .expect("rebind pop header");
    let cose_bytes = base64::engine::general_purpose::STANDARD
        .decode(&header)
        .expect("standard base64");
    use coset::{CborSerializable as _, CoseSign1};
    let sign1 = CoseSign1::from_slice(&cose_bytes).expect("valid COSE_Sign1");

    let expected_own =
        canonical_pop_preimage("POST", htu, &body, DEVICE_INSTANCE_ID, NONCE_HEX, iat).unwrap();
    assert_eq!(
        sign1.payload.as_deref(),
        Some(expected_own.as_slice()),
        "the rebind proof must bind the device's OWN instance_id"
    );
    let with_challenge_id =
        canonical_pop_preimage("POST", htu, &body, CHALLENGE_INSTANCE_ID, NONCE_HEX, iat).unwrap();
    assert_ne!(
        sign1.payload.as_deref(),
        Some(with_challenge_id.as_slice()),
        "the rebind proof must NOT bind the server-assigned challenge instanceId"
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
        .expect("the rebind COSE_Sign1 must verify against the device public key");
}

#[test]
fn the_deactivate_pop_proof_binds_the_devices_own_instance_id_and_verifies() {
    let signer = FixedDeviceSigner::from_seed([7u8; 32]);
    let htu = "https://api.conspect.studio/v0/organisations/org_test/deactivate";
    let req = build_deactivate_request(DEVICE_BINDING_ID, NONCE_HEX);
    let body = serde_json::to_vec(&req).expect("serialize");
    let iat: i64 = 1_790_000_000;

    let header =
        pop_deactivate_header_value(&signer, htu, &body, DEVICE_INSTANCE_ID, NONCE_HEX, iat)
            .expect("deactivate pop header");
    let cose_bytes = base64::engine::general_purpose::STANDARD
        .decode(&header)
        .expect("standard base64");
    use coset::{CborSerializable as _, CoseSign1};
    let sign1 = CoseSign1::from_slice(&cose_bytes).expect("valid COSE_Sign1");

    let expected_own =
        canonical_pop_preimage("POST", htu, &body, DEVICE_INSTANCE_ID, NONCE_HEX, iat).unwrap();
    assert_eq!(
        sign1.payload.as_deref(),
        Some(expected_own.as_slice()),
        "the deactivate proof must bind the device's OWN instance_id"
    );

    let vk = signer.verifying_key();
    sign1
        .verify_signature(b"", |sig, tbs| {
            let signature = ed25519_dalek::Signature::from_slice(sig)
                .map_err(|e| format!("bad sig bytes: {e}"))?;
            vk.verify_strict(tbs, &signature)
                .map_err(|e| format!("verify failed: {e}"))
        })
        .expect("the deactivate COSE_Sign1 must verify against the device public key");
}

#[test]
fn rebind_response_deserializes_only_a_lease_serial_no_embedded_lease() {
    // The live RebindResponse carries leaseSerial (string|null) — NOT an embedded
    // signed lease envelope. So the client cannot install from the response; it
    // seeds nextNonce and the next renew installs the refreshed lease (ADR-I009 §1).
    let json = r#"{
        "rebound": true,
        "leaseSerial": "01931d4e-7b2a-7c00-9f3a-2b6e1c0e9b41",
        "notAfter": 1784023648420,
        "enforcementState": "compliant",
        "rebindsThisYear": 1,
        "seatConsumed": false,
        "fpScore": 95,
        "nextNonce": "9f3a2b6e1c0e9b41a4c92d7e3b8f10c25e6a7d3f49b0c81e2f5a6d7c8b9e0f1a2"
    }"#;
    let resp: RebindResponse = serde_json::from_str(json).expect("RebindResponse must deserialize");
    assert!(resp.rebound);
    assert_eq!(
        resp.lease_serial.as_deref(),
        Some("01931d4e-7b2a-7c00-9f3a-2b6e1c0e9b41")
    );
    assert_eq!(resp.not_after, Some(1_784_023_648_420));
    assert_eq!(resp.rebinds_this_year, 1);
    assert!(
        !resp.seat_consumed,
        "a rebind consumes NO new seat (always false)"
    );
    assert_eq!(
        resp.next_nonce, "9f3a2b6e1c0e9b41a4c92d7e3b8f10c25e6a7d3f49b0c81e2f5a6d7c8b9e0f1a2",
        "the nextNonce seeds steady-state renew"
    );
}

#[test]
fn rebind_response_rejects_a_missing_required_field() {
    // v0.46.0 marks ALL RebindResponse fields REQUIRED. The load-bearing safety property
    // is that a response missing a NON-nullable required field is REJECTED, not silently
    // defaulted — most critically `nextNonce` (a missing nonce would silently strand the
    // next renew) and the scalar flags. Each omission below must fail to deserialize.
    // (leaseSerial/notAfter are required-BUT-nullable; absence is treated identically to
    // an explicit null — `None`, a withheld re-issue the device never installs from — so
    // those are covered by the null-tolerance test below, not a presence assertion.)
    let cases: &[(&str, &str)] = &[
        (
            "nextNonce",
            r#"{"rebound":true,"leaseSerial":"s","notAfter":1,"enforcementState":"compliant","rebindsThisYear":1,"seatConsumed":false,"fpScore":95}"#,
        ),
        (
            "rebound",
            r#"{"leaseSerial":"s","notAfter":1,"enforcementState":"compliant","rebindsThisYear":1,"seatConsumed":false,"fpScore":95,"nextNonce":"ab"}"#,
        ),
        (
            "seatConsumed",
            r#"{"rebound":true,"leaseSerial":"s","notAfter":1,"enforcementState":"compliant","rebindsThisYear":1,"fpScore":95,"nextNonce":"ab"}"#,
        ),
        (
            "rebindsThisYear",
            r#"{"rebound":true,"leaseSerial":"s","notAfter":1,"enforcementState":"compliant","seatConsumed":false,"fpScore":95,"nextNonce":"ab"}"#,
        ),
        (
            "enforcementState",
            r#"{"rebound":true,"leaseSerial":"s","notAfter":1,"rebindsThisYear":1,"seatConsumed":false,"fpScore":95,"nextNonce":"ab"}"#,
        ),
        (
            "fpScore",
            r#"{"rebound":true,"leaseSerial":"s","notAfter":1,"enforcementState":"compliant","rebindsThisYear":1,"seatConsumed":false,"nextNonce":"ab"}"#,
        ),
    ];
    for (missing, json) in cases {
        assert!(
            serde_json::from_str::<RebindResponse>(json).is_err(),
            "a RebindResponse missing the required `{missing}` must be rejected, not defaulted: {json}"
        );
    }
}

#[test]
fn rebind_response_tolerates_a_withheld_reissue_null_lease_serial() {
    // A revoked entitlement: rebound=false, leaseSerial=null, notAfter=null — the
    // signer withholds the re-issue (never off air). Must still deserialise.
    let json = r#"{
        "rebound": false,
        "leaseSerial": null,
        "notAfter": null,
        "enforcementState": "revoked",
        "rebindsThisYear": 2,
        "seatConsumed": false,
        "fpScore": 40,
        "nextNonce": ""
    }"#;
    let resp: RebindResponse =
        serde_json::from_str(json).expect("must deserialize a withheld rebind");
    assert!(!resp.rebound);
    assert_eq!(resp.lease_serial, None);
    assert_eq!(resp.not_after, None);
}
