//! RIST (Reliable Internet Stream Transport, VSF `TR-06`) URL-lowering tests:
//! the pure function that turns a base `rist://host:port` URL + typed
//! [`multiview_config::RistOptions`] + a resolved PSK into the
//! `rist://…?rist_profile=…&buffer_size=…&encryption=…&secret=…&pkt_size=…`
//! `AVIO` URL the libav `librist` demuxer/muxer opens (ADR-0095 Tier-0).
//!
//! Mirrors the SRT `to_url` / `to_url_redacted` seam. Load-bearing: the
//! resolved PSK appears verbatim in `to_url` but is `***`-redacted in
//! `to_url_redacted` (so it never reaches a log); IPv6 literals stay bracketed;
//! a non-empty bonding list is rejected (Tier-0 single-link only).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use multiview_config::{lower_rist_url, RistOptions, RistProfile, RistUrlError};

#[test]
fn rist_minimal_passes_base_url_through() {
    // No options ⇒ the base URL is opened verbatim (FFmpeg defaults apply).
    let opts = RistOptions::default();
    let url = lower_rist_url("rist://[::1]:5000", &opts, None, false).expect("url");
    assert_eq!(url, "rist://[::1]:5000");
}

#[test]
fn rist_lowers_profile_buffer_pkt_size() {
    let toml_str = r#"
profile = "main"
buffer_ms = 1000
pkt_size = 1316
"#;
    let opts: RistOptions = toml::from_str(toml_str).expect("opts");
    let url = lower_rist_url("rist://[::]:5000", &opts, None, false).expect("url");
    assert!(
        url.starts_with("rist://[::]:5000?"),
        "base preserved: {url}"
    );
    assert!(url.contains("rist_profile=1"), "main ⇒ 1: {url}");
    assert!(url.contains("buffer_size=1000"), "{url}");
    assert!(url.contains("pkt_size=1316"), "{url}");
    // No encryption block ⇒ no secret/encryption tokens.
    assert!(!url.contains("encryption="), "{url}");
    assert!(!url.contains("secret="), "{url}");
}

#[test]
fn rist_simple_profile_token_is_zero() {
    let mut opts = RistOptions::default();
    opts.profile = Some(RistProfile::Simple);
    let url = lower_rist_url("rist://host.example:5000", &opts, None, false).expect("url");
    assert!(url.contains("rist_profile=0"), "simple ⇒ 0: {url}");
}

#[test]
fn rist_psk_secret_is_included_then_redacted() {
    let toml_str = r#"
profile = "main"

[encryption]
aes_bits = "aes256"
secret_ref = "env:RIST_PSK"
"#;
    let opts: RistOptions = toml::from_str(toml_str).expect("opts");
    // The caller resolves the secret_ref → plaintext and passes it in.
    let url = lower_rist_url(
        "rist://[2001:db8::1]:5000",
        &opts,
        Some("super-secret-passphrase"),
        false,
    )
    .expect("url");
    assert!(url.contains("encryption=256"), "aes256 ⇒ 256: {url}");
    assert!(
        url.contains("secret=super-secret-passphrase"),
        "the resolved PSK is included verbatim for libav: {url}"
    );

    // The redacted form hides the secret but keeps the cipher length.
    let redacted = lower_rist_url(
        "rist://[2001:db8::1]:5000",
        &opts,
        Some("super-secret-passphrase"),
        true,
    )
    .expect("redacted");
    assert!(redacted.contains("encryption=256"), "{redacted}");
    assert!(redacted.contains("secret=***"), "{redacted}");
    assert!(
        !redacted.contains("super-secret-passphrase"),
        "the plaintext PSK must never reach the redacted (loggable) URL: {redacted}"
    );
}

#[test]
fn rist_encryption_without_resolved_secret_is_an_error() {
    let toml_str = r#"
[encryption]
aes_bits = "aes128"
secret_ref = "env:RIST_PSK"
"#;
    let opts: RistOptions = toml::from_str(toml_str).expect("opts");
    // Encryption configured but the caller could not resolve the secret_ref.
    let err = lower_rist_url("rist://[::1]:5000", &opts, None, false)
        .expect_err("encryption requested with no resolved secret must error");
    assert!(matches!(err, RistUrlError::UnresolvedSecret));
}

#[test]
fn rist_bonding_is_rejected_tier0() {
    let toml_str = r#"
profile = "simple"

[[bonding]]
url = "rist://[2001:db8::2]:5000"
"#;
    let opts: RistOptions = toml::from_str(toml_str).expect("opts");
    let err = lower_rist_url("rist://[::1]:5000", &opts, None, false)
        .expect_err("bonding is unreachable on the Tier-0 FFmpeg path");
    assert!(matches!(err, RistUrlError::BondingUnsupported));
}

#[test]
fn rist_base_url_keeps_existing_query_with_ampersand() {
    // A base URL that already carries a query gets the lowered options appended
    // with `&`, not a second `?`.
    let toml_str = r"
buffer_ms = 500
";
    let opts: RistOptions = toml::from_str(toml_str).expect("opts");
    let url = lower_rist_url("rist://[::1]:5000?weight=5", &opts, None, false).expect("url");
    assert!(url.starts_with("rist://[::1]:5000?weight=5&"), "{url}");
    assert!(url.contains("buffer_size=500"), "{url}");
    assert_eq!(url.matches('?').count(), 1, "exactly one ? in {url}");
}
