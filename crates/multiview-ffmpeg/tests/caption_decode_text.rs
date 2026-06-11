//! Native **text** caption decode (SUR-3 phase 2/3): drive the linked libav
//! text-subtitle decoders (`subrip`, `webvtt`, `mov_text`) directly from a
//! caption packet and assert they produce the unified
//! [`multiview_ffmpeg::CaptionCue::Text`] shape — the markup-stripped display
//! lines plus a cue window whose start is rebased onto the nanosecond timeline
//! through the configured stream time-base (invariant #3).
//!
//! No network, no muxing: the `SubRip` / `WebVTT` / `mov_text` decoders consume
//! the cue **body** as packet payload and take timing from the packet PTS, so a
//! fixed byte sequence is a complete, deterministic fixture (captions.md §9 —
//! "TDD on controlled inputs, never live broadcast"). The DVB-sub (bitmap) path
//! is proven separately in `demux_subtitle.rs`.
//!
//! Gated behind the `ffmpeg` feature (these text decoders are already linked in
//! the LGPL `FFmpeg` 7.1 build — no new dependency, captions.md §3).
#![cfg(feature = "ffmpeg")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use multiview_core::time::Rational;
use multiview_ffmpeg::caption_decode::{CaptionDecoder, CaptionSource};
use multiview_ffmpeg::CaptionCue;

/// The default on-screen hold (ns) a libav text decoder falls back to when it
/// reports no explicit cue window — mirrors `caption_decode::DEFAULT_HOLD_NS`
/// (4 s). The standalone, demux-less decode path here has no `pkt_timebase`, so
/// the decoders surface `end_display_time == 0` and this bounded hold closes the
/// cue (proving an open-ended caption cannot linger forever).
const DEFAULT_HOLD_NS: i64 = 4_000_000_000;

/// A 1 kHz (millisecond) packet time-base — the simplest rebasing to read.
fn ms_time_base() -> Rational {
    Rational::new(1, 1_000)
}

/// Pull the single [`CaptionCue::Text`] payload (lines, start, end) out of a
/// decode result, asserting there is exactly one text cue.
fn one_text(cues: &[CaptionCue]) -> (Vec<String>, i64, i64) {
    assert_eq!(cues.len(), 1, "expected exactly one cue, got {cues:?}");
    match &cues[0] {
        CaptionCue::Text { start, end, text } => {
            (text.lines.clone(), start.as_nanos(), end.as_nanos())
        }
        other => panic!("expected a text cue, got {other:?}"),
    }
}

#[test]
fn subrip_packet_decodes_to_text_cue_with_rebased_start() {
    let mut dec = CaptionDecoder::for_embedded(CaptionSource::SubRip, ms_time_base())
        .expect("open subrip decoder");
    // The `subrip` decoder consumes the cue body; PTS = 1000 ms anchors it.
    let cues = dec
        .decode_bytes_for_window(b"HELLO WORLD", Some(1_000), 2_000)
        .expect("decode without error");
    let (lines, start_ns, end_ns) = one_text(&cues);
    assert_eq!(lines, vec!["HELLO WORLD".to_owned()]);
    // 1000 ms @ 1 kHz time-base -> 1e9 ns (start rebased through the time-base).
    assert_eq!(start_ns, 1_000_000_000, "start rebased from PTS");
    // No pkt_timebase on the standalone context -> no explicit window -> the
    // bounded default hold closes the cue.
    assert_eq!(
        end_ns,
        1_000_000_000 + DEFAULT_HOLD_NS,
        "bounded hold applied"
    );
}

#[test]
fn subrip_start_is_rebased_through_a_non_trivial_time_base() {
    // 90 kHz time-base: 90_000 ticks == 1 s. A 1:1 tick==ns shortcut would give
    // 90_000 ns, so a correct 1e9 ns proves the rebasing goes through the
    // time-base (invariant #3, non-tautological).
    let mut dec = CaptionDecoder::for_embedded(CaptionSource::SubRip, Rational::new(1, 90_000))
        .expect("open subrip @90k");
    let cues = dec.decode_bytes(b"NINETY K", Some(90_000)).expect("decode");
    let (lines, start_ns, _end_ns) = one_text(&cues);
    assert_eq!(lines, vec!["NINETY K".to_owned()]);
    assert_eq!(start_ns, 1_000_000_000, "90_000 @ 90 kHz == 1 s == 1e9 ns");
}

#[test]
fn webvtt_packet_decodes_to_text_cue_and_strips_inline_markup() {
    let mut dec = CaptionDecoder::for_embedded(CaptionSource::WebVtt, ms_time_base())
        .expect("open webvtt decoder");
    // WebVTT inline tags (`<b>…</b>`) must be stripped: the decoder emits ASS and
    // `strip_ass_event` removes the markup, leaving the plain display line.
    let cues = dec
        .decode_bytes(b"Hello, <b>world</b>", Some(1_000))
        .expect("decode without error");
    let (lines, start_ns, _end_ns) = one_text(&cues);
    assert_eq!(
        lines,
        vec!["Hello, world".to_owned()],
        "inline WebVTT markup is stripped"
    );
    assert_eq!(start_ns, 1_000_000_000);
}

#[test]
fn mov_text_tx3g_packet_decodes_to_text_cue() {
    let mut dec = CaptionDecoder::for_embedded(CaptionSource::MovText, ms_time_base())
        .expect("open mov_text decoder");
    // A tx3g/mov_text sample is a 2-byte big-endian length prefix + UTF-8 text.
    let text = b"CAPTION";
    let len = u16::try_from(text.len()).expect("len fits u16");
    let mut packet = Vec::with_capacity(text.len() + 2);
    packet.extend_from_slice(&len.to_be_bytes());
    packet.extend_from_slice(text);
    let cues = dec
        .decode_bytes(&packet, Some(1_000))
        .expect("decode without error");
    let (lines, start_ns, _end_ns) = one_text(&cues);
    assert_eq!(lines, vec!["CAPTION".to_owned()]);
    assert_eq!(start_ns, 1_000_000_000);
}

#[test]
fn empty_packet_yields_no_cue_and_never_panics() {
    // Captions are intermittent: an empty caption packet must produce no cue and
    // must not error or panic (invariants #2 last-good, #10 isolation).
    let mut dec = CaptionDecoder::for_embedded(CaptionSource::SubRip, ms_time_base())
        .expect("open subrip decoder");
    let cues = dec
        .decode_bytes(b"", Some(1_000))
        .expect("empty is not an error");
    assert!(cues.is_empty(), "empty packet -> no cue");
}

#[test]
fn malformed_packet_degrades_gracefully_and_never_panics() {
    // A structurally-invalid caption packet must degrade gracefully — never a
    // panic and never a stall (invariant #10). Invalid UTF-8 fed to `subrip` is
    // rejected by the decoder with a typed error; the wrapper surfaces it as a
    // `Result` rather than unwinding across the FFI boundary.
    let mut dec = CaptionDecoder::for_embedded(CaptionSource::SubRip, ms_time_base())
        .expect("open subrip decoder");
    let result = dec.decode_bytes(&[0xFF, 0x00, 0x01, 0xFE], Some(1_000));
    // The bytes are not valid UTF-8, so `subrip` reports a decode error; what
    // matters is that the call *returns* (Ok-with-or-without-a-cue or Err) and
    // never panics. A subsequent valid packet still decodes — the decoder is not
    // wedged by the bad input (no stall).
    assert!(
        result.is_err(),
        "invalid-UTF-8 subrip packet is a typed error"
    );
    let recovered = dec
        .decode_bytes(b"AFTER", Some(2_000))
        .expect("decoder still works after a bad packet");
    let (lines, _, _) = one_text(&recovered);
    assert_eq!(lines, vec!["AFTER".to_owned()], "no stall after bad input");
}
