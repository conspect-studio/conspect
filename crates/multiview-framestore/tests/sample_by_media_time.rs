#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
//! Latch-on-tick sampling: the output clock samples each tile by **output media
//! time**, selecting the frame whose source-relative `media_time` is
//! *nearest-but-not-after* the requested instant (streaming-gotchas §1).
//!
//! This is the regression guard for the "file/VOD tile races ultra-fast then
//! freezes" bug: when the output loop runs slower than real-time, the ingest
//! thread (which decodes ahead) had published many frames into the store, and a
//! `latest-wins` read sampled the newest one — so the tile's content raced far
//! ahead of the output's own media clock. Sampling by media time makes the tile
//! advance 1:1 with output time regardless of how fast frames were produced.

use multiview_core::time::MediaTime;
use multiview_core::traits::SourceState;
use multiview_framestore::{NoSignalPolicy, TileStore, TileThresholds};

/// Helper: a store that holds frames forever (so selection, not the failure
/// ladder, is what we are asserting) with very generous freshness thresholds.
fn store() -> TileStore<u32> {
    TileStore::new("t", TileThresholds::default(), NoSignalPolicy::HoldForever)
}

#[test]
fn read_at_selects_nearest_but_not_after() {
    let s = store();
    // Three frames stamped on the OUTPUT media timeline (ms): 0, 40, 80.
    s.publish(10_u32, MediaTime::from_nanos(0));
    s.publish(20_u32, MediaTime::from_nanos(40_000_000));
    s.publish(30_u32, MediaTime::from_nanos(80_000_000));

    // At output time 0 we must see frame@0, NOT the latest (frame@80).
    let r0 = s.read_at(MediaTime::from_nanos(0));
    assert_eq!(r0.frame().map(|f| **f), Some(10), "t=0 must latch frame@0");

    // Between 40 and 80 we latch frame@40 (nearest-but-not-after).
    let r1 = s.read_at(MediaTime::from_nanos(60_000_000));
    assert_eq!(r1.frame().map(|f| **f), Some(20), "t=60ms latches frame@40");

    // At/after the newest we latch the newest.
    let r2 = s.read_at(MediaTime::from_nanos(80_000_000));
    assert_eq!(r2.frame().map(|f| **f), Some(30), "t=80ms latches frame@80");
}

#[test]
fn read_at_does_not_race_ahead_when_producer_runs_far_ahead() {
    // The bug scenario in miniature: the producer has published 0..=250 frames
    // (the file decoded ahead), each stamped at i*40ms of OUTPUT media time,
    // BUT the consumer's clock has only reached output time 1.0s (== frame 25).
    let s = store();
    for i in 0..=250_i64 {
        let at = MediaTime::from_nanos(i.saturating_mul(40_000_000));
        let val = u32::try_from(i).unwrap();
        s.publish(val, at);
    }

    // The output clock is at 1.0s. We MUST sample frame 25 (1.0s / 40ms), not
    // the newest frame 250 — that newest-wins read is exactly the race.
    let now = MediaTime::from_nanos(1_000_000_000);
    let r = s.read_at(now);
    assert_eq!(
        r.frame().map(|f| **f),
        Some(25),
        "output@1.0s must latch source frame 25, not race to the newest (250)"
    );
}

#[test]
fn read_at_holds_last_frame_when_output_passes_the_end() {
    // A finite source played out: frames 0..=9 at 40ms steps. Once output time
    // passes the last frame, the tile holds the last-good frame (freeze), it
    // never goes blank and never rewinds.
    let s = store();
    for i in 0..=9_i64 {
        let at = MediaTime::from_nanos(i.saturating_mul(40_000_000));
        s.publish(u32::try_from(i).unwrap(), at);
    }
    let way_past = MediaTime::from_nanos(10_000_000_000);
    let r = s.read_at(way_past);
    assert_eq!(r.frame().map(|f| **f), Some(9), "holds the last frame");
}

#[test]
fn read_at_before_first_frame_uses_earliest() {
    // If the only frames available are stamped slightly after `now` (e.g. the
    // first frame arrives a touch late, or a static primed frame stamped at a
    // non-zero instant), the tile shows the earliest frame rather than a slate —
    // a tile should not flash NO_SIGNAL just because its first frame's stamp is
    // marginally ahead of the very first tick.
    let s = store();
    s.publish(7_u32, MediaTime::from_nanos(5_000_000));
    let r = s.read_at(MediaTime::from_nanos(0));
    assert_eq!(
        r.frame().map(|f| **f),
        Some(7),
        "earliest frame, not a slate"
    );
}

#[test]
fn is_primed_flips_false_to_true_on_first_publish() {
    // The startup prime-wait uses `is_primed` to tell a tile that has decoded its
    // first frame from one still cold. A fresh store has published nothing, so it
    // is NOT primed; the very first publish flips it primed, forever.
    let s = store();
    assert!(
        !s.is_primed(),
        "a store with no published frame must read NOT primed (cold tile)"
    );
    s.publish(42_u32, MediaTime::from_nanos(0));
    assert!(
        s.is_primed(),
        "the first published frame must mark the tile primed"
    );
    // Newest-wins publishes keep it primed (it never reverts to cold).
    s.publish(43_u32, MediaTime::from_nanos(40_000_000));
    assert!(s.is_primed(), "a primed store stays primed");
}

#[test]
fn state_at_classifies_on_the_latched_frame_not_the_newest() {
    // FRESHNESS DIVERGENCE regression (multiview-framestore + multiview-engine).
    //
    // An ahead-decoding VOD source decodes far past the output clock: it has
    // already published a FUTURE-stamped frame into the ring (its newest frame's
    // source-media stamp sits well ahead of `now`), while the picture the output
    // clock is actually latched onto is an old frame.
    //
    // `state()` classifies on `elapsed_since_frame` — the lag of the NEWEST
    // published frame. With a future stamp `>= now`, that lag saturates to 0, so
    // `state()` reports LIVE even though the LATCHED picture has frozen and aged.
    // `state_at(now)` must instead classify on the LATCHED frame's lag
    // (`now - selected.at`, the exact rule `read_at` uses), so a tile whose shown
    // picture has aged past `nosignal` reports NO_SIGNAL.
    let s = store();
    // Frame@0, then the producer raced ahead and published a frame stamped 100s
    // (a long source gap / ahead-decode). The output clock is at 20s: it latches
    // frame@0 (20s stale), but the newest stamp (100s) is >= now.
    s.publish(10_u32, MediaTime::from_nanos(0));
    s.publish(20_u32, MediaTime::from_nanos(100_000_000_000));

    let twenty_sec = MediaTime::from_nanos(20_000_000_000);
    // The producer-liveness view: the newest frame is "fresh" (future stamp) ->
    // LIVE. This is what the buggy `sample_states` used, and it is misleading.
    assert_eq!(
        s.state(twenty_sec),
        SourceState::Live,
        "state() classifies on the newest (future-stamped) frame -> LIVE"
    );
    // The correct latched-picture view: the frame on screen is frame@0, 20s old
    // -> past the 10s nosignal threshold -> NO_SIGNAL.
    assert_eq!(
        s.state_at(twenty_sec),
        SourceState::NoSignal,
        "state_at() classifies on the latched (shown) frame -> NO_SIGNAL"
    );
    // state_at MUST agree exactly with read_at's own ladder (shared logic).
    assert_eq!(
        s.state_at(twenty_sec),
        s.read_at(twenty_sec).state(),
        "state_at and read_at must classify on the identical latched lag"
    );

    // A second instant where the latched frame IS current keeps reporting LIVE:
    // at output 0.0s the latched frame is frame@0 (lag 0).
    assert_eq!(
        s.state_at(MediaTime::ZERO),
        SourceState::Live,
        "at output 0.0s the latched frame is current -> LIVE"
    );
}

#[test]
fn state_at_ages_a_finite_clip_that_output_runs_past() {
    // The ascending-clip case the engine sees most: a finite VOD published at
    // i*40ms for i in 0..=250 (0..10.0s). Once the output clock runs past the
    // clip's last frame, the latched picture freezes on frame@10.0s and ages.
    let s = store();
    for i in 0..=250_i64 {
        let at = MediaTime::from_nanos(i.saturating_mul(40_000_000));
        s.publish(u32::try_from(i).unwrap(), at);
    }
    // At 1.0s the latched frame is frame@1.0s (lag 0) -> LIVE.
    assert_eq!(
        s.state_at(MediaTime::from_nanos(1_000_000_000)),
        SourceState::Live,
        "output 1.0s latches a current frame -> LIVE"
    );
    // At 20.0s the latched frame is the last (10.0s), 10s behind -> NO_SIGNAL,
    // matching read_at exactly.
    let twenty_sec = MediaTime::from_nanos(20_000_000_000);
    assert_eq!(
        s.state_at(twenty_sec),
        SourceState::NoSignal,
        "output 20.0s, last frame 10s behind -> NO_SIGNAL"
    );
    assert_eq!(s.state_at(twenty_sec), s.read_at(twenty_sec).state());
}

#[test]
fn state_at_on_an_empty_store_is_no_signal() {
    // A store that has never been published reports NO_SIGNAL for any instant
    // (mirrors `state()` and `read_at` on an empty ring).
    let s = store();
    assert_eq!(s.state_at(MediaTime::ZERO), SourceState::NoSignal);
    assert_eq!(
        s.state_at(MediaTime::from_nanos(5_000_000_000)),
        SourceState::NoSignal
    );
}

#[test]
fn ring_is_bounded_drop_oldest() {
    // The ring is bounded: publishing far more than its capacity drops the
    // OLDEST entries (newest wins for memory), so sampling an old instant falls
    // back to the earliest *retained* frame rather than growing without bound.
    let s = store();
    let cap = TileStore::<u32>::RING_CAPACITY;
    let total = cap.saturating_add(50);
    for i in 0..total {
        let at = MediaTime::from_nanos(i64::try_from(i).unwrap().saturating_mul(40_000_000));
        s.publish(u32::try_from(i).unwrap(), at);
    }
    // Sampling t=0 (long since evicted) must not panic and must yield the
    // earliest retained frame, which is `total - cap`.
    let earliest_retained = u32::try_from(total - cap).unwrap();
    let r = s.read_at(MediaTime::from_nanos(0));
    assert_eq!(r.frame().map(|f| **f), Some(earliest_retained));
    // Sampling the newest instant still yields the newest frame.
    let newest_at = MediaTime::from_nanos(i64::try_from(total - 1).unwrap() * 40_000_000);
    let r2 = s.read_at(newest_at);
    assert_eq!(
        r2.frame().map(|f| **f),
        Some(u32::try_from(total - 1).unwrap())
    );
}

#[test]
fn steady_state_pacing_prunes_consumed_frames() {
    // BOUNDED-MEMORY guard (invariant #9). In the shipping path the producer is
    // paced ~1:1 with the output clock (multiview-cli `PtsWallClock`), so the
    // output latches each frame at ~the instant it was published and advances
    // past every prior frame. Output `now` is monotonic (inv #1/#3), so a frame
    // older than the current latch can NEVER be selected again — retaining it is
    // pure dead weight. The ring must prune those consumed past frames instead of
    // sitting pinned at a full RING_CAPACITY-deep backlog (~3.2 GB/tile at 2160p).
    let s = store();
    let cap = TileStore::<u32>::RING_CAPACITY;
    // Run well past the ceiling so a count-only bound would stay pinned at `cap`.
    let iters = cap.saturating_mul(4);
    for i in 0..iters {
        let at = MediaTime::from_nanos(i64::try_from(i).unwrap().saturating_mul(40_000_000));
        s.publish(u32::try_from(i).unwrap(), at);
        // The output clock latches this frame on this tick (1:1 pacing). This
        // also proves the just-latched frame is never pruned (a prune bound that
        // dropped the boundary frame would break this read-back).
        let r = s.read_at(at);
        assert_eq!(
            r.frame().map(|f| **f),
            Some(u32::try_from(i).unwrap()),
            "output@i must latch the frame it just published"
        );
    }
    // After steady-state pacing the reader has consumed every past frame, so the
    // ring holds only a tiny trailing window (the latched frame + at most a
    // handful), NOT ~RING_CAPACITY frames.
    let retained = s.retained_frames();
    assert!(
        retained <= 4,
        "steady-state ring must prune consumed frames; retained {retained} of cap {cap}"
    );
}

#[test]
fn prune_keeps_the_latched_boundary_frame() {
    // The prune drops entries STRICTLY older than the latch watermark; the frame
    // AT the watermark (the one the output clock is currently latched onto) must
    // be KEPT — an off-by-one that evicted the boundary would make the tile jump
    // off its held frame the instant the producer publishes again.
    let s = store();
    s.publish(10_u32, MediaTime::from_nanos(0));
    s.publish(20_u32, MediaTime::from_nanos(40_000_000));
    // Latch frame@40ms -> watermark = 40ms.
    assert_eq!(
        s.read_at(MediaTime::from_nanos(40_000_000))
            .frame()
            .map(|f| **f),
        Some(20)
    );
    // A further forward publish triggers the prune with watermark = 40ms.
    s.publish(30_u32, MediaTime::from_nanos(80_000_000));
    // Re-reading at the latched instant must STILL yield frame@40ms: it sits at
    // the watermark and must survive the prune (frame@0, strictly older, is gone).
    assert_eq!(
        s.read_at(MediaTime::from_nanos(40_000_000))
            .frame()
            .map(|f| **f),
        Some(20),
        "the frame at the watermark (latched) must survive the prune"
    );
}

#[test]
fn backwards_stamp_reanchors_so_a_reconnect_is_not_pruned() {
    // Reconnect / source-generation change (bad inputs are the purpose): a
    // producer that had been latched far along its timeline (advancing the prune
    // watermark) reconnects and re-stamps from a LOW media time. The watermark is
    // keyed on the tile-global publish `seq`, and the re-anchored generation's
    // frames carry strictly higher `seq`s than the old generation — so the old
    // latch can never prune the fresh low-stamped frames and the tile does not go
    // blank after a reconnect. (A media-time watermark would have pruned frame@0
    // as "older than the 10s latch"; keying on the monotonic publish seq is what
    // prevents that, including under the cross-generation read/reconnect race —
    // see `tile::cross_generation_watermark_race`.)
    let s = store();
    s.publish(1_u32, MediaTime::from_nanos(0));
    s.publish(2_u32, MediaTime::from_nanos(10_000_000_000));
    let _ = s.read_at(MediaTime::from_nanos(10_000_000_000)); // watermark -> seq of frame@10s
                                                              // New generation: a backwards stamp re-anchors at t=0, then a normal forward
                                                              // publish continues it.
    s.publish(100_u32, MediaTime::from_nanos(0));
    s.publish(101_u32, MediaTime::from_nanos(40_000_000));
    // The re-anchored generation's first frame must survive that forward publish:
    // its seq outranks the old 10s latch, so the prune keeps it.
    assert_eq!(
        s.read_at(MediaTime::from_nanos(0)).frame().map(|f| **f),
        Some(100),
        "re-anchored generation must survive the next publish (seq outranks the old latch)"
    );
    assert_eq!(
        s.read_at(MediaTime::from_nanos(40_000_000))
            .frame()
            .map(|f| **f),
        Some(101)
    );
}

#[test]
fn state_at_alone_bounds_the_ring_for_a_monitored_uncomposited_tile() {
    // A tile that is monitored (state sampled) but not currently composited never
    // calls `read_at`, yet its producer keeps publishing. `state_at` must advance
    // the prune watermark too, so such a tile's ring stays bounded (invariant #9)
    // instead of filling to RING_CAPACITY.
    let s = store();
    let cap = TileStore::<u32>::RING_CAPACITY;
    let iters = cap.saturating_mul(4);
    for i in 0..iters {
        let at = MediaTime::from_nanos(i64::try_from(i).unwrap().saturating_mul(40_000_000));
        s.publish(u32::try_from(i).unwrap(), at);
        let _ = s.state_at(at); // monitored, not composited: only state_at is called
    }
    let retained = s.retained_frames();
    assert!(
        retained <= 4,
        "state_at must bound the ring too; retained {retained} of cap {cap}"
    );
}

#[test]
fn retained_frames_reports_actual_ring_occupancy() {
    // Pins the introspection accessor's contract: it reports the TRUE number of
    // frames held in the media-time ring (the bounded-memory tests above rely on
    // it). A fresh store holds none; publishing three frames with no read to
    // advance the watermark retains all three (nothing consumed to prune).
    let s = store();
    assert_eq!(s.retained_frames(), 0, "empty ring holds no frames");
    s.publish(1_u32, MediaTime::from_nanos(0));
    s.publish(2_u32, MediaTime::from_nanos(40_000_000));
    s.publish(3_u32, MediaTime::from_nanos(80_000_000));
    assert_eq!(
        s.retained_frames(),
        3,
        "three published, none consumed -> all three retained"
    );
}

#[test]
fn equal_timestamp_publish_stays_forward_and_still_prunes() {
    // An equal media-time stamp (a duplicate, or a monotonic-guard-clamped PTS)
    // is NOT a discontinuity: it must be treated as a forward publish, so it
    // keeps pruning consumed frames and does NOT reset the watermark the way a
    // genuine backwards (re-anchor) stamp does. If `at == tail` re-anchored, a
    // duplicate stamp would defeat pruning and let the ring re-accumulate the
    // frames the output clock already passed.
    let s = store();
    s.publish(1_u32, MediaTime::from_nanos(0));
    s.publish(2_u32, MediaTime::from_nanos(40_000_000));
    s.publish(3_u32, MediaTime::from_nanos(80_000_000));
    // Advance the latch watermark to 80ms (the newest instant).
    assert_eq!(
        s.read_at(MediaTime::from_nanos(80_000_000))
            .frame()
            .map(|f| **f),
        Some(3)
    );
    // Publish a fourth frame stamped at the SAME instant as the tail (80ms).
    s.publish(4_u32, MediaTime::from_nanos(80_000_000));
    // Forward semantics: the watermark still pruned the consumed frames (@0, @40)
    // — only the two @80ms frames remain, not all four (which is what a spurious
    // re-anchor on `==` would leave).
    assert_eq!(
        s.retained_frames(),
        2,
        "equal-stamp publish must stay forward (keep pruning), not re-anchor"
    );
    // Newest-wins at the shared instant.
    assert_eq!(
        s.read_at(MediaTime::from_nanos(80_000_000))
            .frame()
            .map(|f| **f),
        Some(4),
        "the newer frame at the shared instant wins"
    );
}
