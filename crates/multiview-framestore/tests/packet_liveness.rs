#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
//! Tests for the GP-5 `PacketLiveness` watchdog (ADR-0030 §4 "three signals").
//!
//! The watchdog is the copy-vs-splice primitive for guarded passthrough: a
//! `TileStore` minus the frame ring. It tracks PACKET liveness (arrival
//! instants + max-DTS advance), never decoded-picture age — nothing is decoded
//! on the copy path. Every method takes an injected `now_ns`, so the whole
//! ladder is deterministic with no real clock and no sleeps.
//!
//! The three signals exercised here:
//!  1. **hard death** — elapsed since last packet `>= splice` (or an explicit
//!     Eof/error flag) ⇒ splice.
//!  2. **slow-loris / stutter** — gaps in `[hold, splice)` ride LIVE/STALE with
//!     widened tolerance and NEVER splice (no slate-flapping).
//!  3. **stalled PTS** — bytes flowing but max-DTS frozen past `pts_stall` ⇒
//!     splice.
//!
//! Plus: a stale-read race fails safe (older stamp ⇒ larger elapsed ⇒ biases to
//! SPLICE, never a false-LIVE) and video/audio instances classify
//! independently.

use multiview_framestore::{PacketLiveness, PacketLivenessState, PacketLivenessThresholds};

use proptest::prelude::*;

/// Frame interval for a 50 fps program: T = 20 ms.
const FRAME_NS: i64 = 20_000_000;
/// Segment duration: 2 s.
const SEGMENT_NS: i64 = 2_000_000_000;

/// Thresholds derived from `(frame_interval, segment_duration)` per ADR-0030:
/// `STALE = 2·T`, `SPLICE = max(4·T, 150 ms)`, `NO_SIGNAL = max(2·Sd, 3 s)`,
/// `pts_stall = splice + 2·T`.
fn thresholds() -> PacketLivenessThresholds {
    PacketLivenessThresholds::from_frame_and_segment(FRAME_NS, SEGMENT_NS)
        .expect("derived thresholds are valid")
}

/// The derived constants, computed independently from the ADR formula so a
/// divergence flags a real bug.
const STALE_NS: i64 = 2 * FRAME_NS; // 40 ms
const SPLICE_NS: i64 = 150_000_000; // max(4·20ms=80ms, 150ms) = 150 ms
const PTS_STALL_NS: i64 = SPLICE_NS + 2 * FRAME_NS; // 190 ms
const NOSIGNAL_NS: i64 = 2 * SEGMENT_NS; // max(4 s, 3 s) = 4 s

#[test]
fn derived_thresholds_match_adr_formula() {
    let t = thresholds();
    assert_eq!(t.stale_ns(), STALE_NS, "STALE = 2·T");
    assert_eq!(t.splice_ns(), SPLICE_NS, "SPLICE = max(4·T, 150 ms)");
    assert_eq!(t.nosignal_ns(), NOSIGNAL_NS, "NO_SIGNAL = max(2·Sd, 3 s)");
    assert_eq!(t.pts_stall_ns(), PTS_STALL_NS, "pts_stall = splice + 2·T");
}

#[test]
fn segment_floor_applies_when_2sd_below_3s() {
    // Short segments (0.5 s): 2·Sd = 1 s < 3 s ⇒ NO_SIGNAL floors at 3 s.
    let t = PacketLivenessThresholds::from_frame_and_segment(FRAME_NS, 500_000_000).expect("valid");
    assert_eq!(t.nosignal_ns(), 3_000_000_000);
}

#[test]
fn frame_floor_applies_when_4t_above_150ms() {
    // Slow 10 fps program: T = 100 ms ⇒ 4·T = 400 ms > 150 ms ⇒ SPLICE = 400 ms.
    let t =
        PacketLivenessThresholds::from_frame_and_segment(100_000_000, SEGMENT_NS).expect("valid");
    assert_eq!(t.splice_ns(), 400_000_000);
    assert_eq!(t.pts_stall_ns(), 400_000_000 + 2 * 100_000_000);
}

/// Signal 1, healthy: a fresh packet stamp ⇒ LIVE / no splice.
#[test]
fn fresh_packet_is_live_no_splice() {
    let w = PacketLiveness::new(thresholds());
    w.record_packet(1_000, 0);
    // Read a hair later (well within hold/stale).
    let now = 1_000 + STALE_NS / 2;
    assert_eq!(w.classify(now), PacketLivenessState::Live);
    assert!(!w.should_splice(now));
}

/// Signal 1, hard death by elapsed: advance `now` past `splice` with no new
/// packet ⇒ `should_splice` = true.
#[test]
fn hard_death_by_elapsed_splices() {
    let w = PacketLiveness::new(thresholds());
    w.record_packet(0, 0);
    let now = SPLICE_NS; // elapsed == splice threshold ⇒ splice (>=)
    assert!(w.should_splice(now));
    assert_eq!(w.classify(now), PacketLivenessState::Splice);
}

/// Signal 1, hard death by explicit flag: an Eof/error the caller sets ⇒ splice
/// immediately, even with a fresh packet stamp.
#[test]
fn hard_death_by_eof_flag_splices_immediately() {
    let w = PacketLiveness::new(thresholds());
    w.record_packet(0, 0);
    let now = 1; // freshly stamped — would be LIVE on elapsed alone
    assert!(!w.should_splice(now));
    w.mark_eof();
    assert!(
        w.should_splice(now),
        "Eof forces splice regardless of elapsed"
    );
    assert_eq!(w.classify(now), PacketLivenessState::Splice);
}

/// Signal 1, hard death by explicit error flag.
#[test]
fn hard_death_by_error_flag_splices_immediately() {
    let w = PacketLiveness::new(thresholds());
    w.record_packet(0, 0);
    w.mark_error();
    assert!(w.should_splice(1));
    assert_eq!(w.classify(1), PacketLivenessState::Splice);
}

/// Signal 2, slow-loris / stutter: packets keep arriving with gaps in
/// `[hold, splice)`. The watchdog rides LIVE/STALE and NEVER splices — proven
/// across several ticks so there is no slate-flapping.
#[test]
fn slow_loris_rides_stale_never_splices() {
    let w = PacketLiveness::new(thresholds());
    // Gap chosen strictly inside [stale, splice): bigger than `hold`/stale but
    // smaller than `splice`, and DTS advancing so the stall signal stays clear.
    let gap = (STALE_NS + SPLICE_NS) / 2; // ~95 ms, in [40 ms, 150 ms)
    let mut now = 0_i64;
    let mut dts = 0_i64;
    w.record_packet(now, dts);
    for tick in 0..20 {
        // Sample just before the next packet (max elapsed within the gap).
        let sample = now + gap - 1;
        let state = w.classify(sample);
        assert!(
            !w.should_splice(sample),
            "tick {tick}: stutter inside [stale, splice) must NOT splice (no flapping)"
        );
        // It is allowed to ride STALE (widened tolerance), never Splice.
        assert_ne!(state, PacketLivenessState::Splice, "tick {tick}");
        // Next packet arrives, advancing DTS — resets the packet clock.
        now += gap;
        dts += FRAME_NS;
        w.record_packet(now, dts);
    }
}

/// Signal 3, stalled PTS: bytes keep flowing (packet clock fresh) but the
/// max-seen DTS never advances ⇒ once `last_advancing_dts_at_ns` elapsed
/// reaches `pts_stall`, splice — even though packets are arriving.
#[test]
fn stalled_dts_splices_even_with_flowing_bytes() {
    let w = PacketLiveness::new(thresholds());
    // First advancing packet at t=0, dts=0.
    w.record_packet(0, 0);
    // From here, a frozen encoder loops the SAME dts while bytes keep flowing.
    // Stamp a packet every 10 ms so the packet clock is always fresh.
    let mut now = 0_i64;
    let mut last_no_splice = true;
    while now < PTS_STALL_NS + 50_000_000 {
        now += 10_000_000;
        w.record_packet(now, 0); // dts frozen at 0 — does NOT advance
        let before_stall = now < PTS_STALL_NS;
        if before_stall {
            assert!(
                !w.should_splice(now),
                "bytes flowing, dts elapsed {now} < pts_stall {PTS_STALL_NS}: no splice yet"
            );
        } else {
            // Past pts_stall: stalled-PTS signal fires despite fresh packets.
            assert!(
                w.should_splice(now),
                "dts elapsed {now} >= pts_stall {PTS_STALL_NS}: must splice"
            );
            assert_eq!(w.classify(now), PacketLivenessState::Splice);
            last_no_splice = false;
        }
    }
    assert!(
        !last_no_splice,
        "the stall must have eventually triggered a splice"
    );
}

/// A non-advancing DTS (e.g. B-frame reorder dipping below max) must NOT reset
/// the advancing-DTS clock: only a strict advance of the max-seen DTS counts.
#[test]
fn non_advancing_dts_does_not_reset_stall_clock() {
    let w = PacketLiveness::new(thresholds());
    w.record_packet(0, 100); // max dts = 100
                             // A later packet with a LOWER dts (reorder) at t past pts_stall, never
                             // advancing the max — the stall clock keeps running from t=0.
    let now = PTS_STALL_NS + 1;
    w.record_packet(now, 50); // dts 50 < max 100 ⇒ not advancing
    assert!(
        w.should_splice(now),
        "max-dts frozen since t=0 ⇒ stall fires even though a packet just arrived"
    );
}

/// Stale-read race / clock skew: a `now` EARLIER than the last stamp (a reader
/// observing an out-of-date `now`, or a backwards clock blip) must FAIL SAFE —
/// it must never report a *false* LIVE that it would not also report with the
/// true (later) now. The watchdog clamps negative elapsed to 0 ⇒ LIVE only when
/// genuinely fresh, and a STALER stamp (smaller last-seen) only ever yields a
/// LARGER elapsed ⇒ biases toward splice.
#[test]
fn stale_read_fails_safe_never_false_live() {
    let w = PacketLiveness::new(thresholds());
    // Last packet genuinely long ago (past splice).
    w.record_packet(0, 0);
    // A reader's `now` skewed backwards to before the stamp: elapsed clamps to 0
    // ⇒ LIVE. That is the SAFE direction (never claims live when it is actually
    // dead): with the true now it is dead/splice; the only way skew changes the
    // answer is the harmless backwards case. Critically, an OLDER observed stamp
    // (the wait-free hazard) yields a larger elapsed, never a smaller one.
    let true_now = SPLICE_NS + 100;
    assert!(w.should_splice(true_now), "true now is past splice");
    // Now model the wait-free hazard directly: had the reader observed an even
    // older stamp than the real one, elapsed only grows, so splice still holds.
    let w_old = PacketLiveness::new(thresholds());
    w_old.record_packet(-1_000_000, 0); // a staler observed stamp
    assert!(
        w_old.should_splice(true_now),
        "an older observed stamp ⇒ larger elapsed ⇒ still splices (fail-safe)"
    );
}

/// Two INDEPENDENT instances (video + audio) classify independently: audio
/// dying must not flip video to splice, and vice versa. The caller holds two.
#[test]
fn video_and_audio_classify_independently() {
    let video = PacketLiveness::new(thresholds());
    let audio = PacketLiveness::new(thresholds());
    // Video healthy and fresh; audio long dead.
    let now = SPLICE_NS + 1;
    video.record_packet(now - 1, 0);
    audio.record_packet(0, 0);
    assert!(!video.should_splice(now), "fresh video must stay live");
    assert!(audio.should_splice(now), "dead audio must splice");
    // Reverse: audio fresh, video dead.
    let v2 = PacketLiveness::new(thresholds());
    let a2 = PacketLiveness::new(thresholds());
    v2.record_packet(0, 0);
    a2.record_packet(now - 1, 0);
    assert!(v2.should_splice(now));
    assert!(!a2.should_splice(now));
}

proptest! {
    /// `should_splice` is MONOTONE in elapsed: for fixed thresholds and a fixed
    /// last-packet/last-advancing stamp, once it returns true for some `now` it
    /// stays true for every later `now`. More elapsed never flips
    /// splice → no-splice (the anti-flapping safety property).
    #[test]
    fn should_splice_is_monotone_in_now(
        stamp in -1_000_000_000_i64..1_000_000_000,
        dts in 0_i64..1_000,
        now0 in 0_i64..10_000_000_000,
        delta in 0_i64..10_000_000_000,
    ) {
        let w = PacketLiveness::new(thresholds());
        w.record_packet(stamp, dts);
        let later = now0.saturating_add(delta);
        if w.should_splice(now0) {
            prop_assert!(
                w.should_splice(later),
                "splice must not flip back to no-splice as elapsed grows"
            );
        }
    }

    /// Classification agrees with `should_splice`: the `Splice` state is exactly
    /// the should-splice decision (no third interpretation).
    #[test]
    fn classify_splice_iff_should_splice(
        stamp in -1_000_000_000_i64..1_000_000_000,
        now in 0_i64..10_000_000_000,
    ) {
        let w = PacketLiveness::new(thresholds());
        w.record_packet(stamp, 0);
        let splicing = w.classify(now) == PacketLivenessState::Splice;
        prop_assert_eq!(splicing, w.should_splice(now));
    }
}
