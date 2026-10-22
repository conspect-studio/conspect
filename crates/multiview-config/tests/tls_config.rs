//! TLS-0 (ADR-W029): the `[control.tls]` static-cert schema parses, is
//! `mode`-tagged (never `untagged`), and validates cert/key paths fail-closed.
//!
//! The runtime rustls termination lives in `multiview-control`; this crate only
//! models + validates the knobs (mirroring the `[control.limits]` split). These
//! tests pin the wire shape the operator writes and the fail-closed validation
//! `MultiviewConfig::validate` enforces at config load.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use multiview_config::{ControlConfig, TlsConfig};

/// A `[control.tls]` table with `mode = "static"` + `cert_file`/`key_file`
/// deserializes to the `Static` variant with both paths, carried on
/// `ControlConfig::tls`.
#[test]
fn static_mode_parses_with_cert_and_key() {
    let control: ControlConfig = serde_json::from_str(
        r#"{
            "listen": "[::1]:8080",
            "tls": { "mode": "static", "cert_file": "/etc/mv/cert.pem", "key_file": "/etc/mv/key.pem" }
        }"#,
    )
    .expect("a static-mode tls table must deserialize");

    match control.tls {
        Some(TlsConfig::Static {
            cert_file,
            key_file,
        }) => {
            assert_eq!(cert_file, std::path::PathBuf::from("/etc/mv/cert.pem"));
            assert_eq!(key_file, std::path::PathBuf::from("/etc/mv/key.pem"));
        }
        other => panic!("expected Some(TlsConfig::Static {{ .. }}), got {other:?}"),
    }
}

/// A `[control]` section with no `[control.tls]` leaves `tls` absent — the
/// default plain-HTTP posture (no native TLS dep, no cert required).
#[test]
fn absent_tls_is_none() {
    let control: ControlConfig =
        serde_json::from_str(r#"{ "listen": "[::]:8080" }"#).expect("a control without tls parses");
    assert!(
        control.tls.is_none(),
        "no [control.tls] ⇒ plain HTTP (tls == None)"
    );
}

/// The union is `mode`-tagged, not `untagged`: an unknown `mode` fails to parse
/// (validated by construction — a typo can never silently pick a variant).
#[test]
fn unknown_mode_fails_to_parse() {
    let parsed: Result<TlsConfig, _> =
        serde_json::from_str(r#"{ "mode": "acme-dns01", "cert_file": "/c", "key_file": "/k" }"#);
    assert!(
        parsed.is_err(),
        "an unknown tls mode must fail to parse (mode-tagged, never untagged)"
    );
}

/// `static` mode requires BOTH a `cert_file` and a `key_file`: a missing field
/// fails to parse (no all-optional silent-empty config).
#[test]
fn static_mode_requires_cert_and_key() {
    let no_key: Result<TlsConfig, _> =
        serde_json::from_str(r#"{ "mode": "static", "cert_file": "/c" }"#);
    assert!(
        no_key.is_err(),
        "static mode without a key_file must fail to parse"
    );
    let no_cert: Result<TlsConfig, _> =
        serde_json::from_str(r#"{ "mode": "static", "key_file": "/k" }"#);
    assert!(
        no_cert.is_err(),
        "static mode without a cert_file must fail to parse"
    );
}

/// An empty `cert_file` path is rejected by validation (fail-closed at config
/// load — an empty path would never load a certificate).
#[test]
fn empty_cert_path_fails_validation() {
    let tls = TlsConfig::Static {
        cert_file: std::path::PathBuf::new(),
        key_file: std::path::PathBuf::from("/etc/mv/key.pem"),
    };
    assert!(
        tls.validate().is_err(),
        "an empty cert_file path must fail config-load validation"
    );
}

/// An empty `key_file` path is rejected by validation (fail-closed at config load).
#[test]
fn empty_key_path_fails_validation() {
    let tls = TlsConfig::Static {
        cert_file: std::path::PathBuf::from("/etc/mv/cert.pem"),
        key_file: std::path::PathBuf::new(),
    };
    assert!(
        tls.validate().is_err(),
        "an empty key_file path must fail config-load validation"
    );
}

/// A fully-specified static config validates OK — the happy path is accepted.
#[test]
fn a_complete_static_config_validates() {
    let tls = TlsConfig::Static {
        cert_file: std::path::PathBuf::from("/etc/mv/cert.pem"),
        key_file: std::path::PathBuf::from("/etc/mv/key.pem"),
    };
    tls.validate()
        .expect("a static config with both paths present must validate");
}
