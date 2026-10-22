//! SRT (Secure Reliable Transport) config-schema tests: the `SourceKind::Srt`
//! ingest variant and the `Output::Srt` push-sink variant, with the typed
//! `SrtOptions` (mode, latency, `AES` encryption holding a passphrase
//! **reference**, stream id). Pure serde — TOML/JSON round-trip + validation,
//! no engine, no network, no sockets (SRT-0, ADR-0039).
//!
//! Load-bearing properties:
//!
//! * the extension is **purely additive** — a legacy `{ kind = "srt", url }`
//!   document still round-trips **byte-identical** (no schema version bump);
//! * the passphrase is a `secret_ref` (a secret-manager pointer), **never** a
//!   plaintext key in the config model;
//! * `pbkeylen ∈ {0, 16, 24, 32}` and the stream id is capped at SRT's
//!   512-byte protocol limit;
//! * IPv6-first examples lead (ADR-0042).
//!
//! Mirrors `rist_schema.rs` — RIST (ADR-0095) mirrors the SRT seam (ADR-0039).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use multiview_config::{
    Output, Source, SourceKind, SrtEncryption, SrtKeyLength, SrtMode, SrtOptions,
};
use proptest::prelude::*;

/// A minimal legacy SRT source document (no `srt` block) in canonical JSON.
const LEGACY_SOURCE_JSON: &str = r#"{"id":"srt_legacy","kind":"srt","url":"srt://[::1]:9000"}"#;

/// The same document in canonical TOML.
const LEGACY_SOURCE_TOML: &str =
    "id = \"srt_legacy\"\nkind = \"srt\"\nurl = \"srt://[::1]:9000\"\n";

/// A minimal legacy SRT push output (no `srt` block) in canonical JSON.
const LEGACY_OUTPUT_JSON: &str =
    r#"{"kind":"srt","id":"srt_out","url":"srt://[::1]:9000","codec":"h264"}"#;

/// The same output in canonical TOML.
const LEGACY_OUTPUT_TOML: &str =
    "kind = \"srt\"\nid = \"srt_out\"\nurl = \"srt://[::1]:9000\"\ncodec = \"h264\"\n";

/// Parse a minimal SRT source whose `[srt]` block body is `body` (TOML lines).
fn srt_source_with_block(body: &str) -> Source {
    let toml_str = format!(
        "id = \"srt_under_test\"\nkind = \"srt\"\nurl = \"srt://[::1]:9000\"\n\n[srt]\n{body}"
    );
    toml::from_str(&toml_str).expect("valid SRT source")
}

// ---------------------------------------------------------------------------
// SourceKind::Srt
// ---------------------------------------------------------------------------

#[test]
fn source_srt_minimal_url_only() {
    let toml_str = r#"
id = "srt_cam"
kind = "srt"
url = "srt://[2001:db8::10]:9000"
"#;
    let src: Source = toml::from_str(toml_str).expect("valid minimal SRT source");
    assert_eq!(src.id, "srt_cam");
    match &src.kind {
        SourceKind::Srt { url, srt } => {
            assert_eq!(url, "srt://[2001:db8::10]:9000");
            assert!(srt.is_none(), "no srt block ⇒ None");
        }
        other => panic!("expected Srt, got {other:?}"),
    }
}

#[test]
fn source_srt_legacy_json_roundtrips_byte_identical() {
    // ADR-0039 §2: purely additive — an existing `{ kind: "srt", url }` config
    // must still parse AND re-serialize to the exact same bytes (no schema
    // version bump). A `skip_serializing_if`-guarded `Option` is what buys this.
    let src: Source = serde_json::from_str(LEGACY_SOURCE_JSON).expect("legacy JSON parses");
    let json = serde_json::to_string(&src).expect("serialize JSON");
    assert_eq!(
        json, LEGACY_SOURCE_JSON,
        "the additive `srt` field must not change the legacy wire form"
    );
    // Internal tag (never untagged): `kind` is a top-level discriminator.
    assert!(json.contains("\"kind\":\"srt\""));
}

#[test]
fn source_srt_legacy_toml_roundtrips_byte_identical() {
    let src: Source = toml::from_str(LEGACY_SOURCE_TOML).expect("legacy TOML parses");
    assert_eq!(
        toml::to_string(&src).expect("serialize TOML"),
        LEGACY_SOURCE_TOML,
        "the additive `srt` field must not change the legacy TOML form"
    );
}

#[test]
fn source_srt_with_typed_options() {
    let toml_str = r##"
id = "srt_main"
kind = "srt"
url = "srt://[::]:9000"

[srt]
mode = "listener"
latency_ms = 200
stream_id = "#!::r=live/feed,m=publish"

[srt.encryption]
key_length = "aes256"
passphrase_ref = "op://Servers/feed/srt-passphrase"
"##;
    let src: Source = toml::from_str(toml_str).expect("valid SRT source with options");
    match &src.kind {
        SourceKind::Srt { url, srt } => {
            assert_eq!(url, "srt://[::]:9000");
            let opts = srt.as_ref().expect("options present");
            assert_eq!(opts.mode, Some(SrtMode::Listener));
            assert_eq!(opts.latency_ms, Some(200));
            assert_eq!(opts.stream_id.as_deref(), Some("#!::r=live/feed,m=publish"));
            let enc = opts.encryption.as_ref().expect("encryption present");
            assert_eq!(enc.key_length, SrtKeyLength::Aes256);
            assert_eq!(enc.passphrase_ref, "op://Servers/feed/srt-passphrase");
        }
        other => panic!("expected Srt, got {other:?}"),
    }
}

#[test]
fn source_srt_options_json_roundtrip_internal_tag() {
    let json_in = r#"{
        "id": "srt_json",
        "display_name": "Field feed",
        "kind": "srt",
        "url": "srt://[2001:db8::10]:9000",
        "srt": {
            "mode": "rendezvous",
            "latency_ms": 120
        }
    }"#;
    let original: Source = serde_json::from_str(json_in).expect("parse json");
    let json = serde_json::to_string(&original).expect("serialize json");
    let reparsed: Source = serde_json::from_str(&json).expect("re-parse json");
    assert_eq!(original, reparsed);
    // Internal tag (never untagged): `kind` is a top-level discriminator.
    assert!(json.contains("\"kind\":\"srt\""));
    // Absent optionals stay absent (skip_serializing_if).
    assert!(!json.contains("encryption"));
    assert!(!json.contains("stream_id"));
}

#[test]
fn srt_source_unknown_mode_token_rejected() {
    let err = toml::from_str::<Source>(
        "id = \"s\"\nkind = \"srt\"\nurl = \"srt://[::1]:9000\"\n\n[srt]\nmode = \"bogus\"\n",
    );
    assert!(err.is_err(), "an unknown mode token must fail parse");
}

#[test]
fn srt_source_unknown_key_length_token_rejected() {
    let err = toml::from_str::<Source>(
        "id = \"s\"\nkind = \"srt\"\nurl = \"srt://[::1]:9000\"\n\n[srt.encryption]\n\
         key_length = \"aes64\"\npassphrase_ref = \"env:X\"\n",
    );
    assert!(err.is_err(), "an unknown pbkeylen token must fail parse");
}

#[test]
fn srt_passphrase_is_reference_never_plaintext() {
    // ADR-0039 §5: the passphrase is a secret_ref (a manager pointer), NEVER a
    // plaintext key in the config model. libsrt's SRTO_PASSPHRASE is write-only;
    // config-as-code round-trips the reference only.
    let src = srt_source_with_block(
        "[srt.encryption]\nkey_length = \"aes128\"\npassphrase_ref = \"env:SRT_PASSPHRASE\"\n",
    );
    let json = serde_json::to_string(&src).expect("serialize");
    assert!(
        json.contains("env:SRT_PASSPHRASE"),
        "the secret_ref is carried"
    );
    // There is no plaintext `passphrase`/`secret`/`key` field on the model.
    assert!(
        !json.contains("\"passphrase\""),
        "no plaintext passphrase field"
    );
    assert!(!json.contains("\"secret\""));
    assert!(!json.contains("\"key\""));
}

// ---------------------------------------------------------------------------
// Output::Srt
// ---------------------------------------------------------------------------

#[test]
fn output_srt_legacy_json_roundtrips_byte_identical() {
    let out: Output = serde_json::from_str(LEGACY_OUTPUT_JSON).expect("legacy JSON parses");
    let json = serde_json::to_string(&out).expect("serialize JSON");
    assert_eq!(
        json, LEGACY_OUTPUT_JSON,
        "the additive `srt` field must not change the legacy push-sink wire form"
    );
}

#[test]
fn output_srt_legacy_toml_roundtrips_byte_identical() {
    let out: Output = toml::from_str(LEGACY_OUTPUT_TOML).expect("legacy TOML parses");
    assert_eq!(
        toml::to_string(&out).expect("serialize TOML"),
        LEGACY_OUTPUT_TOML,
        "the additive `srt` field must not change the legacy TOML form"
    );
}

#[test]
fn output_srt_deserializes_with_typed_options() {
    let toml_str = r#"
kind = "srt"
id = "srt_push"
url = "srt://[2001:db8::20]:6000"
codec = "h264"

[srt]
mode = "caller"
latency_ms = 80

[srt.encryption]
key_length = "aes192"
passphrase_ref = "env:SRT_OUT_PASSPHRASE"
"#;
    let out: Output = toml::from_str(toml_str).expect("valid SRT output");
    match &out {
        Output::Srt {
            id,
            url,
            codec,
            gpu_pin,
            audio,
            srt,
            ..
        } => {
            assert_eq!(id.as_deref(), Some("srt_push"));
            assert_eq!(url, "srt://[2001:db8::20]:6000");
            assert_eq!(codec, "h264");
            assert!(gpu_pin.is_none());
            assert!(audio.is_none());
            let opts = srt.as_ref().expect("options");
            assert_eq!(opts.mode, Some(SrtMode::Caller));
            assert_eq!(opts.latency_ms, Some(80));
            let enc = opts.encryption.as_ref().expect("encryption");
            assert_eq!(enc.key_length, SrtKeyLength::Aes192);
            assert_eq!(enc.passphrase_ref, "env:SRT_OUT_PASSPHRASE");
        }
        other => panic!("expected Srt output, got {other:?}"),
    }
}

#[test]
fn output_srt_legacy_derives_id_and_label_unchanged() {
    let out: Output = toml::from_str(LEGACY_OUTPUT_TOML).expect("valid SRT output");
    assert_eq!(out.explicit_id(), Some("srt_out"));
    assert_eq!(out.id(), "srt_out");
    assert_eq!(out.label(), "srt srt://[::1]:9000");
}

// ---------------------------------------------------------------------------
// SrtOptions model
// ---------------------------------------------------------------------------

#[test]
fn srt_options_default_is_empty() {
    let opts = SrtOptions::default();
    assert!(opts.mode.is_none());
    assert!(opts.latency_ms.is_none());
    assert!(opts.encryption.is_none());
    assert!(opts.stream_id.is_none());
}

#[test]
fn srt_key_length_bytes_are_the_pbkeylen_domain() {
    assert_eq!(SrtKeyLength::None.bytes(), 0);
    assert_eq!(SrtKeyLength::Aes128.bytes(), 16);
    assert_eq!(SrtKeyLength::Aes192.bytes(), 24);
    assert_eq!(SrtKeyLength::Aes256.bytes(), 32);
}

#[test]
fn srt_mode_tokens_are_the_libav_mode_tokens() {
    assert_eq!(SrtMode::Caller.as_libav_token(), "caller");
    assert_eq!(SrtMode::Listener.as_libav_token(), "listener");
    assert_eq!(SrtMode::Rendezvous.as_libav_token(), "rendezvous");
    assert_eq!(SrtMode::default(), SrtMode::Caller);
}

// ---------------------------------------------------------------------------
// Validation (admission time, never the hot path)
// ---------------------------------------------------------------------------

#[test]
fn srt_stream_id_at_the_512_byte_cap_accepted() {
    let id = "x".repeat(SrtOptions::STREAM_ID_MAX_BYTES);
    let src = srt_source_with_block(&format!("stream_id = {id:?}\n"));
    src.validate()
        .expect("a stream id exactly at the cap is valid");
}

#[test]
fn srt_stream_id_over_the_512_byte_cap_rejected() {
    let id = "x".repeat(SrtOptions::STREAM_ID_MAX_BYTES + 1);
    let src = srt_source_with_block(&format!("stream_id = {id:?}\n"));
    let err = src
        .validate()
        .expect_err("a stream id over the cap is rejected");
    let msg = format!("{err}");
    assert!(
        msg.contains("512"),
        "the error names the 512-byte cap: {msg}"
    );
}

#[test]
fn srt_output_stream_id_over_the_cap_rejected() {
    let id = "x".repeat(SrtOptions::STREAM_ID_MAX_BYTES + 1);
    let toml_str = format!(
        "kind = \"srt\"\nurl = \"srt://[::1]:9000\"\ncodec = \"h264\"\n\n[srt]\nstream_id = {id:?}\n"
    );
    let out: Output = toml::from_str(&toml_str).expect("parse");
    assert!(
        out.validate().is_err(),
        "the push sink enforces the same cap"
    );
}

#[test]
fn srt_encryption_requires_passphrase_ref() {
    let src =
        srt_source_with_block("[srt.encryption]\nkey_length = \"aes128\"\npassphrase_ref = \"\"\n");
    let err = src
        .validate()
        .expect_err("encryption with an empty passphrase_ref is inconsistent");
    let msg = format!("{err}");
    assert!(
        msg.contains("passphrase_ref"),
        "the error names the missing reference: {msg}"
    );
}

#[test]
fn srt_encryption_rejects_key_length_none() {
    // An `encryption` block that selects no cipher is contradictory — mirroring
    // `SrtConfig::validate`'s `Encryption` error (passphrase/key-length mismatch).
    let src = srt_source_with_block(
        "[srt.encryption]\nkey_length = \"none\"\npassphrase_ref = \"env:SRT_PASSPHRASE\"\n",
    );
    let err = src
        .validate()
        .expect_err("key_length = none inside encryption is contradictory");
    let msg = format!("{err}");
    assert!(
        msg.contains("key_length"),
        "the error names the field: {msg}"
    );
}

#[test]
fn srt_valid_options_validate_ok() {
    let src = srt_source_with_block(
        "mode = \"listener\"\nlatency_ms = 200\nstream_id = \"#!::r=live/feed,m=publish\"\n\n\
         [srt.encryption]\nkey_length = \"aes256\"\npassphrase_ref = \"env:SRT_PASSPHRASE\"\n",
    );
    src.validate()
        .expect("a fully populated SRT source validates");
}

// ---------------------------------------------------------------------------
// Properties over the protocol boundaries
// ---------------------------------------------------------------------------

proptest! {
    /// The stream-id cap is exactly SRT's 512 bytes: accepted at `len`, rejected
    /// at `len + 1`, for every length across the boundary.
    #[test]
    fn srt_stream_id_length_boundary_property(len in 0usize..600) {
        let id = "x".repeat(len);
        let src = srt_source_with_block(&format!("stream_id = {id:?}\n"));
        prop_assert_eq!(
            src.validate().is_ok(),
            len <= SrtOptions::STREAM_ID_MAX_BYTES
        );
    }

    /// `pbkeylen` is exactly `{0, 16, 24, 32}`: the four tokens parse, anything
    /// else fails parse, and every accepted value maps back into the domain.
    #[test]
    fn srt_pbkeylen_domain_property(token in "[a-z0-9_]{0,12}") {
        let parsed: Result<SrtKeyLength, _> = serde_json::from_str(&format!("{token:?}"));
        let expected = matches!(token.as_str(), "none" | "aes128" | "aes192" | "aes256");
        prop_assert_eq!(parsed.is_ok(), expected);
        if let Ok(key) = parsed {
            prop_assert!(matches!(key.bytes(), 0 | 16 | 24 | 32));
        }
    }
}

#[test]
fn srt_encryption_type_is_reachable_via_serde() {
    // `SrtEncryption` is `#[non_exhaustive]`, so the deserialize path is how a
    // caller outside this crate constructs it; confirm its fields read back.
    let enc: SrtEncryption =
        serde_json::from_str(r#"{"key_length":"aes128","passphrase_ref":"env:X"}"#)
            .expect("encryption parses");
    assert_eq!(enc.key_length, SrtKeyLength::Aes128);
    assert_eq!(enc.passphrase_ref, "env:X");
}
