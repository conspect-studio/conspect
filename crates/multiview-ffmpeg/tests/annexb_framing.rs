//! Pure Annex-B framing-normalization tests (no `ffmpeg` feature needed).
//!
//! The WHIP ingest path (ADR-T014) feeds the packet-fed H.264 decoder whatever
//! the RTP depacketizer emits: start-code-framed Annex-B, AVCC length-prefixed
//! access units, a raw STAP-A aggregation payload, or one bare NAL with no
//! framing at all. `multiview_ffmpeg::to_annexb` must normalize all of them to
//! start-code framing — pure byte logic, unit-testable in the default build.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::borrow::Cow;

use multiview_ffmpeg::to_annexb;

#[test]
fn annexb_input_passes_through_borrowed() {
    // Already start-code framed (4-byte and 3-byte variants): the hot path must
    // not copy — a borrowed Cow proves zero-copy passthrough.
    let four = [
        0x00, 0x00, 0x00, 0x01, 0x67, 0xAA, 0x00, 0x00, 0x01, 0x68, 0xBB,
    ];
    assert!(
        matches!(to_annexb(&four), Cow::Borrowed(b) if b == four),
        "4-byte start-code AU must pass through borrowed"
    );

    let three = [0x00, 0x00, 0x01, 0x65, 0x12, 0x34];
    assert!(
        matches!(to_annexb(&three), Cow::Borrowed(b) if b == three),
        "3-byte start-code AU must pass through borrowed"
    );
}

#[test]
fn bare_nal_gets_a_start_code_prefix() {
    // A single NAL with no framing — exactly what the RTP depacketizer emits for
    // a single-NAL packet or a reassembled FU-A (ADR-T014).
    let sps = [0x67, 0x42, 0xC0, 0x1E];
    let out = to_annexb(&sps);
    assert_eq!(
        out.as_ref(),
        &[0x00, 0x00, 0x00, 0x01, 0x67, 0x42, 0xC0, 0x1E],
        "bare NAL must be wrapped with one start code"
    );
}

#[test]
fn avcc_length_prefixed_au_converts_to_start_codes() {
    // Two NALs with 4-byte big-endian length prefixes (avcC framing).
    let avcc = [
        0x00, 0x00, 0x00, 0x02, 0x67, 0xAA, // SPS, len 2
        0x00, 0x00, 0x00, 0x03, 0x65, 0x11, 0x22, // IDR, len 3
    ];
    let out = to_annexb(&avcc);
    assert_eq!(
        out.as_ref(),
        &[
            0x00, 0x00, 0x00, 0x01, 0x67, 0xAA, //
            0x00, 0x00, 0x00, 0x01, 0x65, 0x11, 0x22,
        ],
        "AVCC lengths must become start codes"
    );
}

#[test]
fn stap_a_payload_is_deaggregated() {
    // RFC 6184 STAP-A: one aggregation NAL (type 24) carrying SPS + PPS with
    // 16-bit lengths — the depacketizer surfaces the raw payload (ADR-T014).
    let stap = [
        0x78, // STAP-A header (nri=3, type 24)
        0x00, 0x02, 0x67, 0xAA, // SPS, len 2
        0x00, 0x03, 0x68, 0xBB, 0xCC, // PPS, len 3
    ];
    let out = to_annexb(&stap);
    assert_eq!(
        out.as_ref(),
        &[
            0x00, 0x00, 0x00, 0x01, 0x67, 0xAA, //
            0x00, 0x00, 0x00, 0x01, 0x68, 0xBB, 0xCC,
        ],
        "STAP-A must split into start-code framed NALs"
    );
}

#[test]
fn malformed_inputs_degrade_to_bare_nal_wrapping_never_panic() {
    // A truncated STAP-A (length runs past the buffer) cannot be split; the
    // conservative total fallback is a single start-code wrap (the decoder skips
    // an unknown/garbage NAL safely). Never a panic, never dropped bytes.
    let truncated_stap = [0x78, 0x00, 0x09, 0x67];
    let out = to_annexb(&truncated_stap);
    assert_eq!(out[..4], [0x00, 0x00, 0x00, 0x01], "fallback wraps once");
    assert_eq!(&out[4..], &truncated_stap, "payload preserved verbatim");

    // A bare NAL whose leading bytes look nothing like a valid AVCC walk.
    let bare = [0x65, 0x88, 0x84];
    let out = to_annexb(&bare);
    assert_eq!(out[..4], [0x00, 0x00, 0x00, 0x01]);
    assert_eq!(&out[4..], &bare);
}

#[test]
fn empty_input_yields_empty_output() {
    let out = to_annexb(&[]);
    assert!(
        out.is_empty(),
        "empty AU must stay empty (and not allocate)"
    );
    assert!(matches!(out, Cow::Borrowed(_)));
}
