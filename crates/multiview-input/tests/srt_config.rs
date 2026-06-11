//! Tests for the SRT connection model and libav URL assembly. Pure (no socket);
//! runs in the DEFAULT build.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use multiview_input::srt::{KeyLength, SrtConfig, SrtError, SrtMode, StreamId};

#[test]
fn srt_caller_url_basic() {
    let cfg = SrtConfig {
        mode: SrtMode::Caller,
        host: "encoder.example".to_owned(),
        port: 9000,
        latency_ms: 200,
        ..SrtConfig::default()
    };
    let url = cfg.to_url().expect("url");
    assert_eq!(url, "srt://encoder.example:9000?mode=caller&latency=200");
}

#[test]
fn srt_modes_roundtrip() {
    for mode in [SrtMode::Caller, SrtMode::Listener, SrtMode::Rendezvous] {
        assert_eq!(SrtMode::from_token(mode.as_libav_token()).unwrap(), mode);
    }
    assert!(SrtMode::from_token("bogus").is_err());
}

#[test]
fn srt_encrypted_url_includes_key_and_passphrase() {
    // IPv6-first: a listener binds the IPv6 wildcard `::` (bracketed in the URL).
    let cfg = SrtConfig {
        mode: SrtMode::Listener,
        host: "::".to_owned(),
        port: 4200,
        key_length: KeyLength::Aes256,
        passphrase: Some("supersecretpass".to_owned()),
        latency_ms: 120,
        ..SrtConfig::default()
    };
    let url = cfg.to_url().expect("url");
    // The IPv6 host must be bracketed so the port is unambiguous.
    assert!(
        url.starts_with("srt://[::]:4200?"),
        "IPv6 host must be bracketed: {url}"
    );
    assert!(url.contains("mode=listener"));
    assert!(url.contains("pbkeylen=32"));
    assert!(url.contains("passphrase=supersecretpass"));

    // Redacted form hides the passphrase but keeps the key length.
    let redacted = cfg.to_url_redacted().expect("redacted");
    assert!(redacted.contains("pbkeylen=32"));
    assert!(redacted.contains("passphrase=***"));
    assert!(!redacted.contains("supersecretpass"));
}

#[test]
fn srt_url_brackets_ipv6_hosts_and_leaves_others_alone() {
    // A bare IPv6 literal host is bracketed; a hostname / IPv4 / already-bracketed
    // host is passed through verbatim (IPv6-first, but IPv4 still works).
    let v6 = SrtConfig {
        mode: SrtMode::Caller,
        host: "2001:db8::1".to_owned(),
        port: 9000,
        ..SrtConfig::default()
    };
    assert!(
        v6.to_url()
            .expect("v6 url")
            .starts_with("srt://[2001:db8::1]:9000?"),
        "{:?}",
        v6.to_url()
    );

    let already = SrtConfig {
        host: "[::1]".to_owned(),
        port: 9000,
        ..SrtConfig::default()
    };
    assert!(already
        .to_url()
        .expect("bracketed url")
        .starts_with("srt://[::1]:9000?"));

    let name = SrtConfig {
        host: "ingest.example.com".to_owned(),
        port: 9000,
        ..SrtConfig::default()
    };
    assert!(name
        .to_url()
        .expect("hostname url")
        .starts_with("srt://ingest.example.com:9000?"));

    // A user-supplied IPv4 host still works and is not bracketed.
    let v4 = SrtConfig {
        host: "203.0.113.7".to_owned(),
        port: 9000,
        ..SrtConfig::default()
    };
    assert!(v4
        .to_url()
        .expect("v4 url")
        .starts_with("srt://203.0.113.7:9000?"));
}

#[test]
fn srt_key_length_bytes_and_decode() {
    assert_eq!(KeyLength::None.bytes(), 0);
    assert_eq!(KeyLength::Aes128.bytes(), 16);
    assert_eq!(KeyLength::Aes192.bytes(), 24);
    assert_eq!(KeyLength::Aes256.bytes(), 32);
    assert_eq!(KeyLength::from_bytes(24).unwrap(), KeyLength::Aes192);
    assert!(KeyLength::from_bytes(20).is_err());
    assert!(KeyLength::Aes128.is_encrypted());
    assert!(!KeyLength::None.is_encrypted());
}

#[test]
fn srt_stream_id_encoded_in_url() {
    let cfg = SrtConfig {
        host: "host".to_owned(),
        port: 1234,
        stream_id: Some(StreamId::new("#!::r=live/feed,m=publish").unwrap()),
        ..SrtConfig::default()
    };
    let url = cfg.to_url().unwrap();
    // The '#', '=' and ',' that would break the query must be percent-encoded;
    // '#' -> %23, '=' -> %3D. The comma is left (valid in a query value).
    assert!(url.contains("streamid=%23!::r%3Dlive/feed,m%3Dpublish"));
}

#[test]
fn srt_stream_id_length_capped() {
    let too_long = "x".repeat(StreamId::MAX_BYTES + 1);
    assert!(matches!(
        StreamId::new(too_long),
        Err(SrtError::StreamIdTooLong(_))
    ));
    // Exactly at the cap is accepted.
    assert!(StreamId::new("y".repeat(StreamId::MAX_BYTES)).is_ok());
}

#[test]
fn srt_validation_catches_inconsistent_encryption() {
    // Encryption requested without passphrase.
    let cfg = SrtConfig {
        host: "h".to_owned(),
        port: 1,
        key_length: KeyLength::Aes128,
        passphrase: None,
        ..SrtConfig::default()
    };
    assert!(matches!(cfg.validate(), Err(SrtError::Encryption(_))));

    // Passphrase with no key length.
    let cfg2 = SrtConfig {
        host: "h".to_owned(),
        port: 1,
        key_length: KeyLength::None,
        passphrase: Some("0123456789".to_owned()),
        ..SrtConfig::default()
    };
    assert!(matches!(cfg2.validate(), Err(SrtError::Encryption(_))));

    // Too-short passphrase.
    let cfg3 = SrtConfig {
        host: "h".to_owned(),
        port: 1,
        key_length: KeyLength::Aes128,
        passphrase: Some("short".to_owned()),
        ..SrtConfig::default()
    };
    assert!(matches!(
        cfg3.validate(),
        Err(SrtError::PassphraseLength(5))
    ));
}

#[test]
fn srt_validation_requires_host_and_port() {
    let cfg = SrtConfig {
        host: String::new(),
        port: 9000,
        ..SrtConfig::default()
    };
    assert!(matches!(cfg.to_url(), Err(SrtError::Parameter(_))));

    let cfg2 = SrtConfig {
        host: "h".to_owned(),
        port: 0,
        ..SrtConfig::default()
    };
    assert!(matches!(cfg2.to_url(), Err(SrtError::Parameter(_))));
}
