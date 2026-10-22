//! Failing-first tests for the **shared RTP-audio rebase seam** (ADR-T013):
//! `(rtp_timestamp, sample_rate_hz, ssrc, discontinuity) → absolute AudioStore
//! frame index on the unified timeline`. Every RTP-audio ingest (WebRTC Opus,
//! AES67 PCM, future) routes through this one type so wrap/anchor/re-anchor are
//! audited once, not re-found per transport.
//!
//! Pure, no native deps — the algorithm is the audio analogue of
//! `PtsNormalizer` (ADR-T003) and is fully exercisable in the default build.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::as_conversions
)]

use multiview_input::rtp_audio::RtpAudioRebaser;

/// Opus rides a 48 kHz RTP clock and the canonical store rate is 48 kHz, so the
/// rescale is identity: the first packet anchors at frame 0 and each subsequent
/// packet's RTP delta maps one-for-one onto store frames.
#[test]
fn opus_48k_identity_anchor_and_advance() {
    let mut r = RtpAudioRebaser::new(48_000, 48_000);
    // First packet anchors at store frame 0.
    let first = r.rebase(1_000_000, 0xAAAA, false);
    assert_eq!(first.store_frame, 0, "first packet anchors at frame 0");
    assert!(
        !first.reanchored,
        "the first packet is an anchor, not a re-anchor"
    );
    // A 20 ms Opus packet advances the RTP timestamp by 960 ticks (48k * 0.02);
    // at the identity rate that is +960 store frames.
    let second = r.rebase(1_000_960, 0xAAAA, false);
    assert_eq!(
        second.store_frame, 960,
        "a 20 ms advance maps to +960 frames"
    );
    let third = r.rebase(1_001_920, 0xAAAA, false);
    assert_eq!(third.store_frame, 1920);
}

/// The wire RTP rate is keyed per stream and rescaled to the store rate with
/// exact integer math — a 16 kHz wire clock against a 48 kHz store triples the
/// frame delta (×3), never mis-scaled by assuming 90 kHz/48 kHz.
#[test]
fn wire_rate_rescales_to_store_rate() {
    let mut r = RtpAudioRebaser::new(16_000, 48_000);
    assert_eq!(r.rebase(0, 0x1, false).store_frame, 0);
    // +160 wire ticks (10 ms at 16 kHz) -> 480 store frames (10 ms at 48 kHz).
    assert_eq!(r.rebase(160, 0x1, false).store_frame, 480);
}

/// The 32-bit RTP timestamp wraps at 2^32; the delta-based unwrap continues the
/// absolute frame index forward across the wrap rather than collapsing to ~0
/// (the soak-only bug ADR-T013 exists to prevent).
#[test]
fn rtp32_wrap_continues_forward() {
    let mut r = RtpAudioRebaser::new(48_000, 48_000);
    let near_wrap: u32 = u32::MAX - 480; // ~10 ms before the wrap.
    let anchor = r.rebase(near_wrap, 0x7, false);
    assert_eq!(anchor.store_frame, 0);
    // The next packet's raw value wraps past 2^32 (raw 480 after a +961 delta).
    // 480u32 wrapping back from (u32::MAX - 480) is a forward delta of 961.
    let after = r.rebase(480, 0x7, false);
    assert_eq!(
        after.store_frame, 961,
        "the wrap must continue forward (+961), not jump backward ~4.29e9"
    );
}

/// A new SSRC is a timeline break: the rebaser re-anchors (the new stream's
/// first packet maps to the store's current write edge supplied by the caller),
/// rather than propagating a multi-hour skip between unrelated clocks.
#[test]
fn ssrc_change_reanchors() {
    let mut r = RtpAudioRebaser::new(48_000, 48_000);
    r.rebase(5_000, 0x1, false); // anchor stream A at frame 0
    let advanced = r.rebase(5_960, 0x1, false);
    assert_eq!(advanced.store_frame, 960);
    // Stream B (new SSRC) arrives with a wildly different RTP base.
    let reanchor = r.rebase(900_000, 0x2, false);
    assert!(reanchor.reanchored, "a new SSRC must re-anchor");
    // The re-anchor continues forward from the prior frame (never backward, never
    // a multi-hour skip): the new stream's first frame is at-or-after the last.
    assert!(
        reanchor.store_frame >= 960,
        "re-anchor continues forward (>= last frame), got {}",
        reanchor.store_frame
    );
    // Subsequent stream-B packets advance from the re-anchor by their own delta.
    let b_next = r.rebase(900_960, 0x2, false);
    assert_eq!(b_next.store_frame, reanchor.store_frame + 960);
}

/// An explicit discontinuity flag (a depacketizer-detected loss / DTX gap)
/// re-anchors forward, exactly like an SSRC change — the frame index never
/// jumps backward.
#[test]
fn explicit_discontinuity_reanchors_forward() {
    let mut r = RtpAudioRebaser::new(48_000, 48_000);
    r.rebase(0, 0x9, false);
    let a = r.rebase(960, 0x9, false);
    assert_eq!(a.store_frame, 960);
    // A discontinuity with a backward-looking raw value must still go forward.
    let d = r.rebase(100, 0x9, true);
    assert!(d.reanchored, "a discontinuity flag re-anchors");
    assert!(
        d.store_frame >= 960,
        "a re-anchor never moves the frame index backward, got {}",
        d.store_frame
    );
}

/// A monotonic-clock huge jump beyond the threshold (no SSRC change, no flag)
/// is also treated as a timeline break and re-anchored forward (the audio
/// analogue of ADR-T003's ~10 s discontinuity guard).
#[test]
fn large_jump_reanchors() {
    let mut r = RtpAudioRebaser::new(48_000, 48_000);
    r.rebase(0, 0x3, false);
    r.rebase(960, 0x3, false);
    // Jump forward by ~1 hour of 48 kHz ticks (>> the threshold) with no flag.
    let jumped = r.rebase(48_000 * 3600, 0x3, false);
    assert!(jumped.reanchored, "a jump beyond the threshold re-anchors");
    assert!(jumped.store_frame >= 960);
}
