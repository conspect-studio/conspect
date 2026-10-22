//! Unit tests for the `multiview node` bootstrap logic (DEV-B5, [ADR-0045]):
//! the node runtime config parse/validate, the keypair-bound enrollment
//! request + display pairing code, the node clock-mode policy, and the
//! presentation frame chooser (§8). Every unit here is **pure** and
//! software-testable — no DRM, no ALSA, no network. The live ingest →
//! hardware-decode → scanout path and the real `POST /devices/enroll` HTTP are
//! hardware/network follow-ons (rule 26), deliberately out of this slice.
//!
//! [ADR-0045]: https://github.com/aperim/multiview/blob/main/docs/decisions/ADR-0045.md
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::float_cmp
)]

use base64::Engine as _;
use multiview_cli::node::{
    pairing_code, ClockMode, EnrollmentRequest, NodeRuntimeConfig, PresentChoice,
    PresentationChooser, VblankPrediction,
};
use multiview_core::time::Rational;
use multiview_core::wallclock::WallClockRef;

// ---------------------------------------------------------------------------
// NodeRuntimeConfig — the node's own bootstrap config (TOML).
// ---------------------------------------------------------------------------

const MINIMAL_NODE_TOML: &str = r#"
controller = "https://[fd00:db8::1]:8443"
enrollment_token = "tok-abcdef0123456789"
"#;

#[test]
fn node_config_parses_minimal_document_with_defaults() {
    let cfg = NodeRuntimeConfig::parse(MINIMAL_NODE_TOML).expect("minimal node config parses");
    assert_eq!(cfg.controller, "https://[fd00:db8::1]:8443");
    assert_eq!(cfg.enrollment_token, "tok-abcdef0123456789");
    // Defaults (ADR-0045): identity dir defaults to the lease-state dir; the
    // link offset defaults into the §8 100–300 ms band; clock mode is OFF.
    assert_eq!(cfg.clock_mode, ClockMode::Default);
    assert!(
        (100..=300).contains(&cfg.link_offset_ms),
        "default link offset must sit in the §8 100-300 ms band, got {}",
        cfg.link_offset_ms
    );
    assert!(
        !cfg.identity_dir.is_empty(),
        "identity_dir must default to a non-empty lease-state dir"
    );
    cfg.validate().expect("a minimal config validates");
}

#[test]
fn node_config_parses_full_document() {
    let toml = r#"
controller = "https://[fd00:db8::1]:8443"
enrollment_token = "tok-abcdef0123456789"
identity_dir = "/var/lib/multiview"
display_name = "Lobby left"
link_offset_ms = 200
clock_mode = "display_locked"
program_stream = "srt://[fd00:db8::1]:9000"
"#;
    let cfg = NodeRuntimeConfig::parse(toml).expect("full node config parses");
    assert_eq!(cfg.identity_dir, "/var/lib/multiview");
    assert_eq!(cfg.display_name.as_deref(), Some("Lobby left"));
    assert_eq!(cfg.link_offset_ms, 200);
    assert_eq!(cfg.clock_mode, ClockMode::DisplayLocked);
    assert_eq!(
        cfg.program_stream.as_deref(),
        Some("srt://[fd00:db8::1]:9000")
    );
}

#[test]
fn node_config_rejects_unknown_field() {
    let toml = r#"
controller = "https://[fd00:db8::1]:8443"
enrollment_token = "tok-abcdef0123456789"
bogus_field = true
"#;
    assert!(
        NodeRuntimeConfig::parse(toml).is_err(),
        "an unknown field must be rejected (deny_unknown_fields), not silently dropped"
    );
}

#[test]
fn node_config_validate_rejects_empty_controller() {
    let cfg = NodeRuntimeConfig::parse(
        r#"
controller = ""
enrollment_token = "tok-abcdef0123456789"
"#,
    )
    .expect("parses");
    assert!(
        cfg.validate().is_err(),
        "an empty controller endpoint must fail validation"
    );
}

#[test]
fn node_config_validate_rejects_empty_token() {
    let cfg = NodeRuntimeConfig::parse(
        r#"
controller = "https://[fd00:db8::1]:8443"
enrollment_token = ""
"#,
    )
    .expect("parses");
    assert!(
        cfg.validate().is_err(),
        "an empty enrollment token must fail validation"
    );
}

#[test]
fn node_config_default_clock_mode_is_off_per_adr_0045() {
    // ADR-0045 §7: the display-locked clock is node-only and default OFF.
    let cfg = NodeRuntimeConfig::parse(MINIMAL_NODE_TOML).expect("parses");
    assert_eq!(
        cfg.clock_mode,
        ClockMode::Default,
        "the display-locked clock mode must be OFF by default (ADR-0045 §7)"
    );
}

// ---------------------------------------------------------------------------
// Pairing code — the six-character code shown on the attached display.
// ---------------------------------------------------------------------------

#[test]
fn pairing_code_is_six_chars_and_deterministic() {
    let key = [7u8; 32];
    let a = pairing_code(&key);
    let b = pairing_code(&key);
    assert_eq!(a.as_str().chars().count(), 6, "pairing code is six chars");
    assert_eq!(a, b, "the same key always yields the same display code");
}

#[test]
fn pairing_code_differs_for_different_keys() {
    let a = pairing_code(&[1u8; 32]);
    let b = pairing_code(&[2u8; 32]);
    assert_ne!(
        a, b,
        "distinct device keys must show distinct pairing codes (collision would mis-pair)"
    );
}

#[test]
fn pairing_code_uses_unambiguous_crockford_alphabet() {
    // Crockford base32 excludes I, L, O, U to avoid display ambiguity.
    let code = pairing_code(&[42u8; 32]);
    for ch in code.as_str().chars() {
        assert!(
            ch.is_ascii_uppercase() || ch.is_ascii_digit(),
            "pairing code char {ch:?} must be an uppercase letter or digit"
        );
        assert!(
            !matches!(ch, 'I' | 'L' | 'O' | 'U'),
            "pairing code must avoid the ambiguous Crockford chars I/L/O/U, got {ch:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// EnrollmentRequest — the keypair-bound enrollment body (no HTTP here).
// ---------------------------------------------------------------------------

#[test]
fn enrollment_request_carries_base64url_raw_public_key() {
    let key = [9u8; 32];
    let req = EnrollmentRequest::build(&key, "tok-abcdef0123456789", Some("Lobby left"));
    // ADR-0045 / ADR-I008: devicePublicKey is base64url of the RAW 32-byte point.
    let expected = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(key);
    assert_eq!(req.device_public_key, expected);
    assert_eq!(req.enrollment_token, "tok-abcdef0123456789");
    assert_eq!(req.display_name.as_deref(), Some("Lobby left"));
}

#[test]
fn enrollment_request_serializes_camel_case() {
    let req = EnrollmentRequest::build(&[3u8; 32], "tok-x0123456789abcd", None);
    let json = serde_json::to_string(&req).expect("serializes");
    assert!(
        json.contains("\"devicePublicKey\""),
        "wire field must be camelCase devicePublicKey, got {json}"
    );
    assert!(
        json.contains("\"enrollmentToken\""),
        "wire field must be camelCase enrollmentToken, got {json}"
    );
    assert!(
        !json.contains("displayName"),
        "an absent display name must be omitted from the wire body, got {json}"
    );
}

// ---------------------------------------------------------------------------
// ClockMode policy — ADR-0045 §7 (node-only; never on a local multiview).
// ---------------------------------------------------------------------------

#[test]
fn display_locked_clock_is_rejected_for_a_local_multiview() {
    // §7: display-locked is valid only on a dedicated node where the panel is
    // the terminal output. A local multiview's clock also serves encoders +
    // network outputs, so locking it to a monitor would break invariant #1.
    assert!(
        ClockMode::DisplayLocked
            .validate_for_node(/* is_dedicated_node = */ false)
            .is_err(),
        "display-locked clock must be rejected when the host also serves other outputs"
    );
    assert!(
        ClockMode::DisplayLocked.validate_for_node(true).is_ok(),
        "display-locked clock is permitted on a dedicated display node"
    );
}

#[test]
fn default_clock_mode_is_always_valid() {
    assert!(ClockMode::Default.validate_for_node(false).is_ok());
    assert!(ClockMode::Default.validate_for_node(true).is_ok());
}

// ---------------------------------------------------------------------------
// PresentationChooser — ADR-0045 §8 pure frame choice.
// ---------------------------------------------------------------------------

/// A 90 kHz program epoch anchored so that media PTS 0 maps to wall ns 0, at a
/// 1:1 media rate (90000 ticks/s). Link offset is applied by the chooser.
fn epoch_90k() -> WallClockRef {
    WallClockRef::new(0, 0, Rational::new(90_000, 1))
}

#[test]
fn chooser_presents_the_frame_nearest_the_predicted_vblank() {
    // Three decoded frames at 90 kHz PTS: 0, 3000 (33.3 ms), 6000 (66.7 ms).
    // With zero link offset their wall instants are 0, 33.3 ms, 66.7 ms.
    // Predict the next vblank at 35 ms wall — the 3000-PTS frame is nearest.
    let chooser = PresentationChooser::new(epoch_90k(), /* link_offset_ms = */ 0);
    let queue = [0_i64, 3000, 6000];
    let predicted = VblankPrediction::at_wall_ns(35_000_000);
    match chooser.choose(&queue, predicted) {
        PresentChoice::Present { pts } => assert_eq!(pts, 3000),
        other => panic!("expected Present{{pts:3000}}, got {other:?}"),
    }
}

#[test]
fn chooser_applies_the_link_offset_uniformly() {
    // Same queue, but a 100 ms link offset shifts every frame's target instant
    // later by 100 ms. The vblank at 135 ms now lands on the 3000-PTS frame
    // (33.3 ms + 100 ms = 133.3 ms), proving the offset is added, not ignored.
    let chooser = PresentationChooser::new(epoch_90k(), 100);
    let queue = [0_i64, 3000, 6000];
    let predicted = VblankPrediction::at_wall_ns(135_000_000);
    match chooser.choose(&queue, predicted) {
        PresentChoice::Present { pts } => assert_eq!(pts, 3000),
        other => panic!("expected Present{{pts:3000}} under a 100 ms link offset, got {other:?}"),
    }
}

#[test]
fn chooser_repeats_when_the_next_frame_is_still_in_the_future() {
    // The earliest queued frame's target instant (with offset) is later than
    // the predicted vblank => present nothing new, repeat the current frame
    // (ADR-0045 §8: "repeat the last frame if the next is early").
    let chooser = PresentationChooser::new(epoch_90k(), 0);
    let queue = [9000_i64, 12000]; // 100 ms, 133 ms
    let predicted = VblankPrediction::at_wall_ns(50_000_000); // 50 ms
    assert_eq!(
        chooser.choose(&queue, predicted),
        PresentChoice::Repeat,
        "a vblank before the earliest frame's deadline must repeat, never present early"
    );
}

#[test]
fn chooser_with_empty_queue_repeats_last_good() {
    // No decoded frames available (a starved feed) => hold last-good, never
    // block, never go black (invariant #2 inherited by the node).
    let chooser = PresentationChooser::new(epoch_90k(), 0);
    let queue: [i64; 0] = [];
    assert_eq!(
        chooser.choose(&queue, VblankPrediction::at_wall_ns(1_000_000)),
        PresentChoice::Repeat,
        "an empty decode queue must repeat last-good, not panic or block"
    );
}

#[test]
fn chooser_drops_stale_frames_and_presents_the_freshest_due_one() {
    // Frames at 0, 33.3, 66.7 ms; vblank at 70 ms. Frames 0 and 3000 are both
    // behind the vblank — the chooser presents the freshest one that is at-or-
    // before the vblank (6000 = 66.7 ms), dropping the older two (§8: "drop if
    // late"). This proves we never present a stale frame when a newer due one
    // exists.
    let chooser = PresentationChooser::new(epoch_90k(), 0);
    let queue = [0_i64, 3000, 6000];
    let predicted = VblankPrediction::at_wall_ns(70_000_000);
    match chooser.choose(&queue, predicted) {
        PresentChoice::Present { pts } => assert_eq!(
            pts, 6000,
            "must present the freshest at-or-before-vblank frame, dropping older ones"
        ),
        other => panic!("expected Present{{pts:6000}}, got {other:?}"),
    }
}
