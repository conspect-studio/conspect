//! RIST (Reliable Internet Stream Transport, VSF `TR-06`) config-schema tests:
//! the `SourceKind::Rist` ingest variant and the `Output::Rist` push-sink
//! variant, with the typed `RistOptions` (profile, buffer, `pkt_size`, `PSK`
//! encryption, bonding peers). Pure serde — TOML/JSON round-trip + validation,
//! no engine, no network.
//!
//! Mirrors the SRT seam (ADR-0095 mirrors ADR-0039). Load-bearing properties:
//! the `PSK` is a `secret_ref` (never a plaintext key in the config); a
//! non-empty `bonding` list is rejected at validate time on the Tier-0 build
//! (honest capability reporting, never a silent single-link); IPv6-first
//! examples lead.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use multiview_config::{
    Output, RistAesBits, RistEncryption, RistOptions, RistProfile, Source, SourceKind,
};

// ---------------------------------------------------------------------------
// SourceKind::Rist
// ---------------------------------------------------------------------------

#[test]
fn source_rist_minimal_url_only() {
    let toml_str = r#"
id = "rist_cam"
kind = "rist"
url = "rist://[::1]:5000"
"#;
    let src: Source = toml::from_str(toml_str).expect("valid minimal RIST source");
    assert_eq!(src.id, "rist_cam");
    match &src.kind {
        SourceKind::Rist { url, rist } => {
            assert_eq!(url, "rist://[::1]:5000");
            assert!(rist.is_none(), "no options block ⇒ None");
        }
        other => panic!("expected Rist, got {other:?}"),
    }
}

#[test]
fn source_rist_with_typed_options() {
    let toml_str = r#"
id = "rist_main"
kind = "rist"
url = "rist://[::]:5000"

[rist]
profile = "main"
buffer_ms = 1000
pkt_size = 1316

[rist.encryption]
aes_bits = "aes256"
secret_ref = "op://Servers/feed/rist-psk"
"#;
    let src: Source = toml::from_str(toml_str).expect("valid RIST source with options");
    match &src.kind {
        SourceKind::Rist { url, rist } => {
            assert_eq!(url, "rist://[::]:5000");
            let opts = rist.as_ref().expect("options present");
            assert_eq!(opts.profile, Some(RistProfile::Main));
            assert_eq!(opts.buffer_ms, Some(1000));
            assert_eq!(opts.pkt_size, Some(1316));
            let enc = opts.encryption.as_ref().expect("encryption present");
            assert_eq!(enc.aes_bits, RistAesBits::Aes256);
            assert_eq!(enc.secret_ref, "op://Servers/feed/rist-psk");
            assert!(opts.bonding.is_empty(), "no bonding ⇒ empty");
        }
        other => panic!("expected Rist, got {other:?}"),
    }
}

#[test]
fn source_rist_roundtrip_json_internal_tag() {
    let json_in = r#"{
        "id": "rist_json",
        "display_name": "Field feed",
        "kind": "rist",
        "url": "rist://[2001:db8::10]:5000",
        "rist": {
            "profile": "simple",
            "buffer_ms": 500
        }
    }"#;
    let original: Source = serde_json::from_str(json_in).expect("parse json");
    let json = serde_json::to_string(&original).expect("serialize json");
    let reparsed: Source = serde_json::from_str(&json).expect("re-parse json");
    assert_eq!(original, reparsed);
    // Internal tag (never untagged): `kind` is a top-level discriminator.
    assert!(json.contains("\"kind\":\"rist\""));
    // Absent optionals stay absent (skip_serializing_if).
    assert!(!json.contains("pkt_size"));
    assert!(!json.contains("encryption"));
    assert!(!json.contains("bonding"));
}

#[test]
fn source_rist_psk_secret_is_reference_never_plaintext() {
    // The PSK passphrase is a secret_ref (a manager pointer), NEVER a plaintext
    // key in the config model — a serialized RIST source must carry the ref, not
    // a key.
    let toml_str = r#"
id = "rist_enc"
kind = "rist"
url = "rist://[::1]:5000"

[rist.encryption]
aes_bits = "aes128"
secret_ref = "env:RIST_PSK"
"#;
    let src: Source = toml::from_str(toml_str).expect("parse");
    let json = serde_json::to_string(&src).expect("serialize");
    assert!(json.contains("env:RIST_PSK"), "the secret_ref is carried");
    // There is no `secret`/`passphrase`/`key` plaintext field on the model.
    assert!(!json.contains("\"secret\""));
    assert!(!json.contains("passphrase"));
    assert!(!json.contains("\"key\""));
}

// ---------------------------------------------------------------------------
// Output::Rist
// ---------------------------------------------------------------------------

#[test]
fn output_rist_deserializes() {
    let toml_str = r#"
kind = "rist"
id = "rist_out"
url = "rist://[2001:db8::20]:6000"
codec = "h264"

[rist]
profile = "main"
buffer_ms = 700
"#;
    let out: Output = toml::from_str(toml_str).expect("valid RIST output");
    match &out {
        Output::Rist {
            id,
            url,
            codec,
            gpu_pin,
            audio,
            rist,
            ..
        } => {
            assert_eq!(id.as_deref(), Some("rist_out"));
            assert_eq!(url, "rist://[2001:db8::20]:6000");
            assert_eq!(codec, "h264");
            assert!(gpu_pin.is_none());
            assert!(audio.is_none());
            let opts = rist.as_ref().expect("options");
            assert_eq!(opts.profile, Some(RistProfile::Main));
            assert_eq!(opts.buffer_ms, Some(700));
        }
        other => panic!("expected Rist output, got {other:?}"),
    }
}

#[test]
fn output_rist_accessors_and_label() {
    let toml_str = r#"
kind = "rist"
id = "rist1"
url = "rist://[::1]:6000"
codec = "h264"
"#;
    let out: Output = toml::from_str(toml_str).expect("valid RIST output");
    assert_eq!(out.explicit_id(), Some("rist1"));
    assert_eq!(out.id(), "rist1");
    assert!(out.gpu_pin().is_none());
    assert!(out.audio().is_none());
    assert_eq!(out.label(), "rist rist://[::1]:6000");
}

#[test]
fn output_rist_id_derives_from_label_when_absent() {
    let toml_str = r#"
kind = "rist"
url = "rist://[::1]:6000"
codec = "h264"
"#;
    let out: Output = toml::from_str(toml_str).expect("valid RIST output");
    assert_eq!(out.explicit_id(), None);
    assert_eq!(out.id(), "rist rist://[::1]:6000");
}

#[test]
fn output_rist_empty_codec_rejected() {
    let toml_str = r#"
kind = "rist"
url = "rist://[::1]:6000"
codec = ""
"#;
    let out: Output = toml::from_str(toml_str).expect("parse");
    assert!(out.validate().is_err(), "empty codec is rejected");
}

// ---------------------------------------------------------------------------
// Validation — bonding rejection (Tier-0 honest capability reporting) +
// encryption consistency.
// ---------------------------------------------------------------------------

#[test]
fn rist_bonding_rejected_on_tier0_build() {
    // ADR-0095 §4: a non-empty bonding list is the Tier-2 direct-FFI feature; on
    // the Tier-0 (FFmpeg `rist://`) build it must be rejected with a clear error,
    // never silently single-linked.
    let toml_str = r#"
id = "rist_bonded"
kind = "rist"
url = "rist://[::1]:5000"

[rist]
profile = "simple"

[[rist.bonding]]
url = "rist://[2001:db8::2]:5000"
"#;
    let src: Source = toml::from_str(toml_str).expect("parse");
    let err = src
        .validate()
        .expect_err("bonding must be rejected on Tier-0");
    let msg = format!("{err}");
    assert!(msg.contains("bonding"), "the error names bonding: {msg}");
}

#[test]
fn rist_encryption_requires_secret_ref() {
    // An encryption block with an empty secret_ref is inconsistent.
    let toml_str = r#"
id = "rist_badenc"
kind = "rist"
url = "rist://[::1]:5000"

[rist.encryption]
aes_bits = "aes128"
secret_ref = ""
"#;
    let src: Source = toml::from_str(toml_str).expect("parse");
    assert!(
        src.validate().is_err(),
        "an empty secret_ref is rejected (encryption requested with no key reference)"
    );
}

#[test]
fn rist_single_link_validates_ok() {
    let toml_str = r#"
id = "rist_ok"
kind = "rist"
url = "rist://[::1]:5000"

[rist]
profile = "main"
buffer_ms = 1000

[rist.encryption]
aes_bits = "aes256"
secret_ref = "env:RIST_PSK"
"#;
    let src: Source = toml::from_str(toml_str).expect("parse");
    src.validate()
        .expect("a single-link encrypted RIST source validates");
}

#[test]
fn rist_options_default_is_empty() {
    let opts = RistOptions::default();
    assert!(opts.profile.is_none());
    assert!(opts.buffer_ms.is_none());
    assert!(opts.pkt_size.is_none());
    assert!(opts.encryption.is_none());
    assert!(opts.bonding.is_empty());
}

#[test]
fn rist_encryption_aes_bits_maps_to_ffmpeg_token() {
    // The FFmpeg `librist` protocol takes `encryption=128|256`.
    assert_eq!(RistAesBits::Aes128.bits(), 128);
    assert_eq!(RistAesBits::Aes256.bits(), 256);
    // `RistEncryption` is constructed via the deserialize path (it is
    // `#[non_exhaustive]`); confirm its fields are readable.
    let toml_str = r#"
id = "enc"
kind = "rist"
url = "rist://[::1]:5000"

[rist.encryption]
aes_bits = "aes256"
secret_ref = "env:X"
"#;
    let src: Source = toml::from_str(toml_str).expect("parse");
    let SourceKind::Rist {
        rist: Some(opts), ..
    } = &src.kind
    else {
        panic!("expected Rist with options");
    };
    let enc: &RistEncryption = opts.encryption.as_ref().expect("encryption");
    assert_eq!(enc.aes_bits, RistAesBits::Aes256);
    assert_eq!(enc.secret_ref, "env:X");
}
