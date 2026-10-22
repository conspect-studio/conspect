//! ELD (EDID-Like Data) parser tests (DEV-B4 / display-out §5).
//!
//! The display driver publishes the sink's parsed audio capability at
//! `/proc/asound/cardN/eld#C.P`. Our sink reads it to learn channels / rates /
//! LPCM-ness and **only emits audio while the ELD is valid** (the pipe lit). An
//! EDID-less head publishes a zero/invalid ELD — no audio path, never a panic.
//! These tests feed the parser **mock ELD bytes** (no `/proc`, no hardware) and
//! pin the capability it derives; the on-hardware leg only supplies the real
//! bytes through the same parser.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::as_conversions,
    clippy::cast_possible_truncation,
    clippy::manual_div_ceil,
    clippy::doc_markdown
)]

use multiview_output::display::audio::{parse_eld, EldCapability};

/// Build a minimal but realistic ELD v2 blob (per the HDMI/HDA ELD layout the
/// kernel emits) declaring the given monitor name + one LPCM SAD with the given
/// channel count and the 32/44.1/48 kHz rate bits.
///
/// Layout (bytes): [0]=version(2<<3), [4]=baseline_eld_len (in 4-byte words),
/// [5]=CEA_EDID_ver/mnl (monitor-name length in low nibble), header is 4 bytes
/// then a 16-byte baseline block then the monitor-name string then the SAD
/// bytes. We construct exactly that so the real kernel parser and ours agree.
fn lpcm_eld(monitor: &str, channels: u8) -> Vec<u8> {
    let mnl = monitor.len().min(16);
    // One CEA short-audio-descriptor (SAD) for LPCM (format code 1):
    //   byte0: (format<<3) | (channels-1)
    //   byte1: rate bitmap — bit0=32k, bit1=44.1k, bit2=48k
    //   byte2: LPCM bit-depth bitmap (16/20/24) for format 1
    let sad = [
        (1u8 << 3) | (channels.saturating_sub(1) & 0x07),
        0b0000_0111, // 32 + 44.1 + 48 kHz
        0b0000_0111, // 16/20/24-bit
    ];
    let sad_count = 1u8;
    let baseline_words = 4 + ((mnl + sad.len()) + 3) / 4; // 16-byte fixed + name + SADs, in words
    let mut eld = vec![0u8; 4];
    eld[0] = 2 << 3; // ELD version 2 in the high bits
    eld[2] = baseline_words as u8; // baseline_eld_len, in 4-byte words
                                   // 16-byte baseline header block:
    let mut baseline = vec![0u8; 16];
    // baseline byte 0 (= ELD byte 4): (CEA_EDID_ver << 5) | (monitor-name
    // length in the low 5 bits) — kernel GRAB_BITS(buf, 4, 0, 5)/(4, 5, 3).
    baseline[0] = (2u8 << 5) | (mnl as u8 & 0x1f);
    // baseline byte 1 (= ELD byte 5): SAD_count in the high nibble — kernel
    // GRAB_BITS(buf, 5, 4, 4); the low bits (conn_type/s_ai/hdcp) stay 0.
    baseline[1] = (sad_count & 0x0f) << 4;
    eld.extend_from_slice(&baseline);
    eld.extend_from_slice(&monitor.as_bytes()[..mnl]);
    eld.extend_from_slice(&sad);
    eld
}

#[test]
fn parses_a_valid_stereo_lpcm_eld() {
    let bytes = lpcm_eld("ACME 24", 2);
    let cap = parse_eld(&bytes).expect("a valid ELD must parse to a capability");
    assert!(cap.has_audio(), "a populated ELD has an audio path");
    assert_eq!(cap.max_channels(), 2, "stereo SAD => 2 channels");
    assert!(cap.supports_rate(48_000), "48 kHz must be advertised");
    assert!(cap.supports_lpcm(), "the SAD declares LPCM");
    assert_eq!(cap.monitor_name(), "ACME 24");
}

#[test]
fn parses_multichannel_when_declared() {
    let bytes = lpcm_eld("BIGSND", 6);
    let cap = parse_eld(&bytes).expect("valid");
    assert_eq!(cap.max_channels(), 6, "a 5.1 SAD => 6 channels");
    assert!(cap.supports_rate(48_000));
}

#[test]
fn edid_less_head_has_no_audio_path_no_panic() {
    // An EDID-less connector publishes an all-zero / empty ELD: the parser must
    // return "no audio capability", never panic. This is the documented field
    // condition (the t630 EDID-less head): video only, audio silent.
    assert!(
        parse_eld(&[]).is_none(),
        "empty ELD => no audio path (None), not a panic"
    );
    assert!(
        parse_eld(&[0u8; 4]).is_none(),
        "all-zero header => no valid baseline => no audio path"
    );
    // A truncated blob (claims a baseline longer than the bytes present) must
    // also degrade to None, never index out of bounds.
    let mut truncated = lpcm_eld("X", 2);
    truncated.truncate(6);
    assert!(
        parse_eld(&truncated).is_none(),
        "a truncated ELD must yield None, never panic"
    );
}

#[test]
fn rate_not_advertised_is_rejected() {
    let bytes = lpcm_eld("MON", 2);
    let cap = parse_eld(&bytes).expect("valid");
    // 88.2 kHz was never set in our rate bitmap.
    assert!(
        !cap.supports_rate(88_200),
        "unadvertised rate must be false"
    );
}

#[test]
fn capability_negotiates_a_supported_format() {
    let cap = parse_eld(&lpcm_eld("MON", 2)).expect("valid");
    // The sink negotiates the canonical 48 kHz stereo it already mixes; the ELD
    // supports it, so negotiation succeeds.
    let negotiated = cap.negotiate(48_000, 2);
    assert_eq!(negotiated, Some((48_000, 2)));
    // A request the ELD cannot satisfy (8 channels on a stereo sink) clamps to
    // the ELD ceiling rather than failing the whole audio path.
    let clamped = cap.negotiate(48_000, 8);
    assert_eq!(
        clamped,
        Some((48_000, 2)),
        "channel request clamps to the ELD max"
    );
}

#[test]
fn capability_is_a_plain_value() {
    // EldCapability is Clone + Eq so the sink can stash it and compare on
    // hotplug re-read (capability changed => Class-2 reconfigure).
    let a = EldCapability::lpcm(2, &[48_000], "M");
    let b = a.clone();
    assert_eq!(a, b);
}
