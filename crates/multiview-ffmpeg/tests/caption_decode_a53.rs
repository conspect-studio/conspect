//! Embedded CEA-608/708 caption decode (SUR-3b): drive the linked libav
//! `cc_dec` from the `AV_FRAME_DATA_A53_CC` cc-data byte sequence and assert it
//! recovers the unified [`multiview_ffmpeg::CaptionCue::Text`] shape — the
//! caption display lines plus a cue window rebased onto the nanosecond timeline
//! through the configured stream time-base (invariant #3), plus the
//! "no caption right now is normal" fail-safes (empty / malformed → no cue, no
//! panic) and the ASS text path.
//!
//! These are **hermetic**: the A53 cc-data triplets are a fixed, hand-built
//! EIA-608 byte sequence (the exact wire format an H.264/MPEG-2 decoder surfaces
//! as A53 side data), so no demux, no muxing, no GPU. The fixture-backed
//! end-to-end decode (a real `mpeg2video` bitstream → A53 side data → `cc_dec`)
//! lives in `caption_a53_frame.rs` behind the `test-fixtures` feature.
//!
//! Gated behind the `ffmpeg` feature (`cc_dec`/`ass` are already linked in the
//! LGPL `FFmpeg` 7.1 build — no new dependency, captions.md §3).
#![cfg(feature = "ffmpeg")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use multiview_core::time::Rational;
use multiview_ffmpeg::caption_decode::{CaptionDecoder, CaptionSource, CcChannel};
use multiview_ffmpeg::CaptionCue;

/// A 90 kHz (MPEG-TS) packet time-base — what a real embedded-CC video stream
/// carries, and a non-trivial rebasing (`90_000` ticks == 1 s) so a 1:1
/// tick==ns shortcut would be visibly wrong.
fn ts_time_base() -> Rational {
    Rational::new(1, 90_000)
}

/// Set the high bit of a 7-bit EIA-608 byte so the total set-bit count is odd
/// (the line-21 parity the decoder validates).
fn odd_parity(byte: u8) -> u8 {
    let low = byte & 0x7f;
    if low.count_ones() % 2 == 0 {
        low | 0x80
    } else {
        low
    }
}

/// One EIA-608 word → one A53 cc-data triplet: a `0xFC` marker (valid field-1
/// line-21) plus the two odd-parity data bytes.
fn triplet(word: (u8, u8)) -> [u8; 3] {
    [0xFC, odd_parity(word.0), odd_parity(word.1)]
}

/// The EIA-608 control/character word sequence for a pop-on caption of `text`:
/// Resume-Caption-Loading, Erase-Non-displayed-Memory, a row-15 preamble, the
/// character pairs, then End-Of-Caption (which is what makes `cc_dec` emit).
fn eia608_words(text: &str) -> Vec<(u8, u8)> {
    const RCL: (u8, u8) = (0x14, 0x20);
    const ENM: (u8, u8) = (0x14, 0x2e);
    const PAC15: (u8, u8) = (0x14, 0x70);
    const EOC: (u8, u8) = (0x14, 0x2f);
    let mut words = vec![RCL, ENM, PAC15];
    let mut bytes: Vec<u8> = text.bytes().collect();
    if bytes.len() % 2 == 1 {
        bytes.push(0x00);
    }
    for pair in bytes.chunks_exact(2) {
        words.push((pair[0], pair[1]));
    }
    words.push(EOC);
    words
}

/// Pull the single [`CaptionCue::Text`] payload (lines, start, end) out of a
/// `Vec`, asserting there is exactly one text cue.
fn one_text(cues: &[CaptionCue]) -> (Vec<String>, i64, i64) {
    assert_eq!(cues.len(), 1, "expected exactly one cue, got {cues:?}");
    match &cues[0] {
        CaptionCue::Text { start, end, text } => {
            (text.lines.clone(), start.as_nanos(), end.as_nanos())
        }
        other => panic!("expected a text cue, got {other:?}"),
    }
}

/// Drive `cc_dec` with one A53 triplet per packet (mirroring the per-video-frame
/// delivery of embedded captions), collecting every cue produced and the PTS of
/// the packet each cue arrived on.
fn decode_caption(text: &str, base_pts: i64, step: i64) -> Vec<(CaptionCue, i64)> {
    let mut dec = CaptionDecoder::for_embedded(
        CaptionSource::EmbeddedCc {
            channel: CcChannel::Cc1,
        },
        ts_time_base(),
    )
    .expect("open cc_dec");

    let mut out = Vec::new();
    for (i, word) in eia608_words(text).into_iter().enumerate() {
        let trip = triplet(word);
        let pts = base_pts + i64::try_from(i).unwrap() * step;
        let cues = dec
            .decode_bytes(&trip, Some(pts))
            .expect("decode a triplet without error");
        for cue in cues {
            out.push((cue, pts));
        }
    }
    out
}

#[test]
fn cc_dec_recovers_known_608_caption_text() {
    // One 608 word per "frame" at a 90 kHz time-base, starting at 90_000 (== 1 s).
    let cues = decode_caption("HELLO WORLD", 90_000, 3_000);
    assert_eq!(
        cues.len(),
        1,
        "exactly one cue is emitted (on the End-Of-Caption word), got {cues:?}"
    );
    let (cue, emit_pts) = &cues[0];
    let (lines, start_ns, end_ns) = one_text(std::slice::from_ref(cue));
    assert_eq!(
        lines,
        vec!["HELLO WORLD".to_owned()],
        "decoded EIA-608 caption text"
    );
    // The cue is anchored at the PTS of the End-Of-Caption packet, rebased through
    // the 90 kHz time-base. A 1:1 tick==ns shortcut would give `*emit_pts` ns
    // (tens of thousands), so 1e9-scale ns proves the rebasing went through the
    // time-base (invariant #3, non-tautological).
    assert_eq!(
        start_ns,
        emit_pts.saturating_mul(1_000_000_000) / 90_000,
        "cue start is the EOC packet PTS rebased through the 90 kHz time-base"
    );
    assert!(
        end_ns > start_ns,
        "cue has a bounded, positive on-screen window (start {start_ns}, end {end_ns})"
    );
}

#[test]
fn empty_a53_side_data_yields_no_cue_and_never_panics() {
    // A video frame with no caption bytes is the common case: it must produce no
    // cue and must not error or panic (invariants #2 last-good, #10 isolation).
    let mut dec = CaptionDecoder::for_embedded(
        CaptionSource::EmbeddedCc {
            channel: CcChannel::Cc1,
        },
        ts_time_base(),
    )
    .expect("open cc_dec");
    let cues = dec
        .decode_bytes(b"", Some(90_000))
        .expect("empty A53 payload is not an error");
    assert!(cues.is_empty(), "empty A53 side data -> no cue");
}

#[test]
fn malformed_a53_side_data_degrades_gracefully_and_keeps_decoding() {
    // A structurally-invalid A53 payload (a half triplet / nonsense bytes) must
    // degrade gracefully — never a panic, never a stall (invariant #10). `cc_dec`
    // is tolerant of junk cc-data (it validates parity / control codes), so it
    // simply produces no cue; what matters is the call *returns* and a subsequent
    // valid caption still decodes (the decoder is not wedged).
    let mut dec = CaptionDecoder::for_embedded(
        CaptionSource::EmbeddedCc {
            channel: CcChannel::Cc1,
        },
        ts_time_base(),
    )
    .expect("open cc_dec");
    let junk = dec
        .decode_bytes(&[0xFF, 0x00], Some(90_000))
        .expect("malformed A53 payload returns, never panics");
    assert!(junk.is_empty(), "junk cc-data -> no cue");

    // A full valid caption after the junk still decodes — no stall.
    let mut step_pts = 180_000;
    let mut recovered = Vec::new();
    for word in eia608_words("OK") {
        let trip = triplet(word);
        let cues = dec
            .decode_bytes(&trip, Some(step_pts))
            .expect("decode still works after malformed input");
        recovered.extend(cues);
        step_pts += 3_000;
    }
    let (lines, _, _) = one_text(&recovered);
    assert_eq!(
        lines,
        vec!["OK".to_owned()],
        "decoder recovers a valid caption after a malformed one (no stall)"
    );
}

#[test]
fn selecting_a_708_service_is_refused_honestly_not_silently_empty() {
    // The linked libav `cc_dec` (FFmpeg 7.1) decodes EIA-608 to text but does
    // **not** decode CEA-708 service blocks to text — DTVCC (cc_type 2/3) data is
    // discarded. Building an embedded-CC decoder for a 708 service must therefore
    // FAIL FAST with a descriptive error rather than open a decoder that silently
    // emits nothing forever (which would look like "no captions" while really
    // meaning "this caption form is not decodable here"). The honest refusal lets
    // the caller fall back (608 field, teletext, sidecar) instead of staring at a
    // dead, cue-less decoder (captions.md §3 row 3; ADR-R007).
    let Err(err) = CaptionDecoder::for_embedded(
        CaptionSource::EmbeddedCc {
            channel: CcChannel::Service(1),
        },
        ts_time_base(),
    ) else {
        panic!("opening a 708-service embedded-CC decoder must be refused");
    };
    let msg = err.to_string();
    assert!(
        msg.contains("708") && msg.contains("service"),
        "the refusal names the unsupported CEA-708 service path, got {msg:?}"
    );
}

#[test]
fn from_parameters_also_refuses_a_708_service_selector() {
    // The refusal must guard the shared open path, not just `for_embedded`: a
    // stream-backed `from_parameters` construction for a 708 service is equally
    // undecodable and must be rejected the same way (no silent cue-less decoder).
    let params = ffmpeg_next::codec::Parameters::new();
    let Err(err) = CaptionDecoder::from_parameters(
        CaptionSource::EmbeddedCc {
            channel: CcChannel::Service(63),
        },
        params,
        ts_time_base(),
    ) else {
        panic!("from_parameters for a 708 service must be refused too");
    };
    assert!(
        err.to_string().contains("708"),
        "the refusal names CEA-708, got {err}"
    );
}

#[test]
fn supported_608_fields_still_open_and_decode_after_the_708_guard() {
    // The 708 refusal must not regress the supported 608 fields: every CC1–CC4
    // field still opens, and the primary CC1 field still decodes a known caption.
    for channel in [
        CcChannel::Cc1,
        CcChannel::Cc2,
        CcChannel::Cc3,
        CcChannel::Cc4,
    ] {
        CaptionDecoder::for_embedded(CaptionSource::EmbeddedCc { channel }, ts_time_base())
            .unwrap_or_else(|e| panic!("608 field {channel:?} must still open: {e}"));
    }
    let cues = decode_caption("HELLO WORLD", 90_000, 3_000);
    let (lines, _, _) = one_text(&cues.iter().map(|(c, _)| c.clone()).collect::<Vec<_>>());
    assert_eq!(
        lines,
        vec!["HELLO WORLD".to_owned()],
        "the supported 608 CC1 path is unaffected by the 708 guard"
    );
}

#[test]
fn ass_packet_decodes_to_text_cue_and_strips_styling() {
    // The ASS/SSA text path (`CaptionSource::Ass`): a Dialogue event body with
    // inline override tags decodes to the markup-stripped display line. The `ass`
    // decoder consumes the event body and `strip_ass_event` removes the styling.
    let mut dec = CaptionDecoder::for_embedded(CaptionSource::Ass, Rational::new(1, 1_000))
        .expect("open ass decoder");
    // A standalone `ass` decoder takes the raw Dialogue event fields; the
    // {\an8}/{\b1} override tags are styling and must not appear in the cue text.
    let event = b"0,0,Default,,0,0,0,,{\\an8}{\\b1}HELLO ASS";
    let cues = dec
        .decode_bytes(event, Some(1_000))
        .expect("decode ass event without error");
    let (lines, start_ns, _end_ns) = one_text(&cues);
    assert_eq!(
        lines,
        vec!["HELLO ASS".to_owned()],
        "ASS override tags are stripped, leaving the display line"
    );
    assert_eq!(start_ns, 1_000_000_000, "ass cue start rebased from PTS");
}
