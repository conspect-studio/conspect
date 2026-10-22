//! Integration tests for the **paced raw-RTP / ST-2110 ingest path** (ENG-2b,
//! ADR-0021 point 3): the [`IngestPump`] wires the wall-clock [`Pacer`] behind
//! the [`ReorderBuffer`], so a bursty / jittery / out-of-order RTP arrival is
//! reordered, normalized, then **paced to wall-clock by its normalized PTS**
//! before it enters the last-good-frame store.
//!
//! These run in the DEFAULT build (no `ffmpeg`): a synthetic [`FrameProducer`]
//! injects the arrival pattern, and the pump's clock is injected (`now_ns`), so
//! the pacing contract is pinned deterministically with **no sleeping**.
//!
//! The pacer is **ingest-side smoothing only** — it gates *when a frame enters
//! the store*, never the output tick (invariant #1). Memory stays bounded: the
//! reorder buffer drops, never grows (invariants #5 / #9).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::collections::VecDeque;

use multiview_core::time::{MediaTime, Rational};
use multiview_framestore::TileStore;
use multiview_input::normalize::WrapBits;
use multiview_input::source::{
    FrameProducer, IngestConfig, IngestPump, PacePolicy, PaceStep, ProducedFrame, StoredFrame,
};

/// A 90 kHz RTP timebase (1/90000 s per tick).
fn rtp_tb() -> Rational {
    Rational::new(1, 90_000)
}

/// A scripted RTP producer: a fixed sequence of `(raw_pts, discontinuity)` then
/// EOS, reporting an RTP 32-bit wrap width and 90 kHz timebase.
struct ScriptedRtp {
    frames: VecDeque<ProducedFrame>,
    timebase: Rational,
    cadence: Rational,
    wrap: WrapBits,
}

impl ScriptedRtp {
    fn new(
        script: impl IntoIterator<Item = (Option<i64>, bool)>,
        timebase: Rational,
        cadence: Rational,
        wrap: WrapBits,
    ) -> Self {
        let frames = script
            .into_iter()
            .map(|(raw_pts, disc)| {
                let f = ProducedFrame::timing_only(raw_pts, 16, 16);
                if disc {
                    f.with_discontinuity()
                } else {
                    f
                }
            })
            .collect();
        Self {
            frames,
            timebase,
            cadence,
            wrap,
        }
    }
}

impl FrameProducer for ScriptedRtp {
    fn next_frame(&mut self) -> multiview_input::Result<Option<ProducedFrame>> {
        Ok(self.frames.pop_front())
    }
    fn timebase(&self) -> Rational {
        self.timebase
    }
    fn cadence(&self) -> Rational {
        self.cadence
    }
    fn wrap_bits(&self) -> WrapBits {
        self.wrap
    }
}

/// The normalized PTS (ns) currently held in the store's last-good slot.
fn slot_pts_ns(store: &TileStore<StoredFrame>) -> Option<i64> {
    store.slot().load().map(|f| f.meta.pts.as_nanos())
}

/// Drive the paced pump deterministically and capture **every** published frame
/// in publish order with the virtual wall-clock instant at which it was released.
///
/// To observe each release individually (the store is a single freshest-wins
/// slot), the clock is advanced in fine 1 ms steps rather than jumping to the
/// next deadline, and the slot is sampled after each poll. The pacer's 40 ms
/// release spacing means each frame is the sole new arrival at its step.
fn drive_paced(
    producer: &mut ScriptedRtp,
    store: &TileStore<StoredFrame>,
    pump: &mut IngestPump,
    start_now_ns: i64,
) -> Vec<(i64, i64)> {
    const TICK_NS: i64 = 1_000_000; // 1 ms virtual step — finer than 40 ms spacing
    let mut now = start_now_ns;
    let mut published: Vec<(i64, i64)> = Vec::new();
    let mut last_slot: Option<i64> = None;
    let mut guard = 0_u64;
    loop {
        guard += 1;
        assert!(guard < 1_000_000, "paced driver must terminate");
        let step = pump
            .pump_one_paced(producer, store, now)
            .expect("paced pump must not fault");
        if let Some(p) = slot_pts_ns(store) {
            if last_slot != Some(p) {
                published.push((p, now));
                last_slot = Some(p);
            }
        }
        if step == PaceStep::Eos {
            break;
        }
        // Advance the virtual clock one fine tick so pending frames release one at
        // a time at their deadlines (a real ingest task wakes at WakeAt instead).
        now = now.saturating_add(TICK_NS);
    }
    published
}

#[test]
fn passthrough_policy_publishes_immediately_no_pacing() {
    // The default file/VOD policy must NOT pace: every frame lands the instant it
    // is pumped (the output clock paces emission, not ingest) — invariant #1.
    let script: Vec<(Option<i64>, bool)> = (0..5).map(|i| (Some(i * 3_600), false)).collect();
    let mut producer = ScriptedRtp::new(script, rtp_tb(), Rational::FPS_25, WrapBits::Rtp32);
    let store: TileStore<StoredFrame> = TileStore::with_defaults("passthrough");
    let config = IngestConfig {
        pace: PacePolicy::Passthrough,
        ..IngestConfig::default()
    };
    let mut pump = IngestPump::new(&producer, config);

    // pump_one (the non-pacing latch path) still works for passthrough.
    let mut count = 0_u64;
    while pump
        .pump_one(&mut producer, &store, MediaTime::ZERO)
        .expect("passthrough pump must not fault")
    {
        count += 1;
    }
    assert_eq!(count, 5);
    assert_eq!(pump.published(), 5, "all frames published immediately");
}

#[test]
fn wallclock_policy_paces_releases_by_normalized_pts() {
    // 25 fps RTP (40 ms steps). Under the wall-clock policy each frame is released
    // when the virtual wall clock reaches anchor + (pts - pts0): the i-th frame
    // (i>0) publishes at start + i*40 ms — ingest-side pacing, never instant.
    let step = 3_600_i64; // one 25 fps frame @ 90 kHz
    let script: Vec<(Option<i64>, bool)> = (0..4).map(|i| (Some(i * step), false)).collect();
    let mut producer = ScriptedRtp::new(script, rtp_tb(), Rational::FPS_25, WrapBits::Rtp32);
    let store: TileStore<StoredFrame> = TileStore::with_defaults("paced");
    let config = IngestConfig {
        pace: PacePolicy::WallClock,
        ..IngestConfig::default()
    };
    let mut pump = IngestPump::new(&producer, config);

    let start = 1_000_000_000_i64; // arbitrary virtual wall-clock origin
    let published = drive_paced(&mut producer, &store, &mut pump, start);

    // Four frames, normalized to 0/40/80/120 ms, each released at start + that
    // offset (the first anchors and releases immediately).
    let pts: Vec<i64> = published.iter().map(|(p, _)| *p).collect();
    assert_eq!(pts, vec![0, 40_000_000, 80_000_000, 120_000_000]);
    for (i, (_, at)) in published.iter().enumerate() {
        let expected_at = start + i64::try_from(i).unwrap() * 40_000_000;
        assert_eq!(
            *at, expected_at,
            "frame {i} released at the wall-clock deadline, not instantly"
        );
    }
}

#[test]
fn bursty_arrival_is_smoothed_not_flooded() {
    // All four frames arrive in one burst (the producer yields them back-to-back
    // at the SAME wall instant). The pacer must still release them spaced by
    // their normalized PTS deltas — a burst does not flood the store.
    let step = 3_600_i64;
    let script: Vec<(Option<i64>, bool)> = (0..4).map(|i| (Some(i * step), false)).collect();
    let mut producer = ScriptedRtp::new(script, rtp_tb(), Rational::FPS_25, WrapBits::Rtp32);
    let store: TileStore<StoredFrame> = TileStore::with_defaults("burst");
    let config = IngestConfig {
        pace: PacePolicy::WallClock,
        jitter_depth: 0,
        ..IngestConfig::default()
    };
    let mut pump = IngestPump::new(&producer, config);

    let start = 0_i64;
    let published = drive_paced(&mut producer, &store, &mut pump, start);
    let ats: Vec<i64> = published.iter().map(|(_, at)| *at).collect();
    // Spaced by 40 ms despite the simultaneous burst arrival.
    assert_eq!(ats, vec![0, 40_000_000, 80_000_000, 120_000_000]);
}

#[test]
fn out_of_order_rtp_reordered_then_paced_monotonic() {
    // RTP frames arrive scrambled; the reorder buffer restores PTS order, then the
    // pacer releases them in monotonic normalized-PTS order at the right instants.
    let step = 3_600_i64;
    let script: Vec<(Option<i64>, bool)> = vec![
        (Some(0), false),
        (Some(2 * step), false), // arrives before step
        (Some(step), false),
        (Some(3 * step), false),
    ];
    let mut producer = ScriptedRtp::new(script, rtp_tb(), Rational::FPS_25, WrapBits::Rtp32);
    let store: TileStore<StoredFrame> = TileStore::with_defaults("ooo");
    let config = IngestConfig {
        pace: PacePolicy::WallClock,
        jitter_depth: 1,
        ..IngestConfig::default()
    };
    let mut pump = IngestPump::new(&producer, config);

    let published = drive_paced(&mut producer, &store, &mut pump, 0);
    let pts: Vec<i64> = published.iter().map(|(p, _)| *p).collect();
    assert_eq!(
        pts,
        vec![0, 40_000_000, 80_000_000, 120_000_000],
        "reordered into monotonic PTS order before pacing"
    );
}

#[test]
fn rtp_32bit_wrap_boundary_paced_continuously() {
    // Straddle the 32-bit RTP wrap boundary: the last tick before 2^32 and the
    // first after must come out as two ADJACENT forward instants (one frame
    // apart), never a ~13.25 h backward jump or a far-future pace deadline.
    let modulus = 1_i64 << 32;
    let step = 3_600_i64; // 25 fps @ 90 kHz
    let before = modulus - step;
    let script: Vec<(Option<i64>, bool)> = vec![
        (Some(before - step), false),
        (Some(before), false),
        (Some(0), false), // wraps to 0
        (Some(step), false),
    ];
    let mut producer = ScriptedRtp::new(script, rtp_tb(), Rational::FPS_25, WrapBits::Rtp32);
    let store: TileStore<StoredFrame> = TileStore::with_defaults("wrap32");
    let config = IngestConfig {
        pace: PacePolicy::WallClock,
        // Large threshold so the (unwrapped, small) delta is not a discontinuity.
        discontinuity_ns: 60_000_000_000,
        ..IngestConfig::default()
    };
    let mut pump = IngestPump::new(&producer, config);

    let published = drive_paced(&mut producer, &store, &mut pump, 0);
    let pts: Vec<i64> = published.iter().map(|(p, _)| *p).collect();
    assert_eq!(pts.len(), 4);
    for w in pts.windows(2) {
        assert_eq!(
            w[1] - w[0],
            40_000_000,
            "uniform 40 ms across the 32-bit RTP wrap: {pts:?}"
        );
    }
    // The release instants advance by 40 ms too — the wrap never produces a
    // far-future pace deadline.
    let ats: Vec<i64> = published.iter().map(|(_, at)| *at).collect();
    for w in ats.windows(2) {
        assert_eq!(
            w[1] - w[0],
            40_000_000,
            "release spacing uniform across wrap"
        );
    }
}

#[test]
fn reorder_overflow_drops_oldest_bounded_memory() {
    // A jitter depth of 1 (capacity 2) under a flood: an old (low-PTS) frame that
    // arrives after the window has moved on is dropped — the buffer never grows.
    // We feed a descending burst then an ascending tail; the very-late ones are
    // dropped, the published set stays monotonic and bounded.
    let step = 3_600_i64;
    let script: Vec<(Option<i64>, bool)> = vec![
        (Some(10 * step), false),
        (Some(11 * step), false),
        (Some(12 * step), false),
        (Some(0), false), // way late — below the watermark, must be dropped
        (Some(13 * step), false),
    ];
    let mut producer = ScriptedRtp::new(script, rtp_tb(), Rational::FPS_25, WrapBits::Rtp32);
    let store: TileStore<StoredFrame> = TileStore::with_defaults("overflow");
    let config = IngestConfig {
        pace: PacePolicy::WallClock,
        jitter_depth: 1,
        ..IngestConfig::default()
    };
    let mut pump = IngestPump::new(&producer, config);

    let published = drive_paced(&mut producer, &store, &mut pump, 0);
    let pts: Vec<i64> = published.iter().map(|(p, _)| *p).collect();
    // Strictly increasing — the far-late frame never causes a backward step.
    for w in pts.windows(2) {
        assert!(w[0] < w[1], "published PTS strictly increasing: {pts:?}");
    }
    // The far-late frame (raw 0, arriving below the reorder watermark) is dropped,
    // so fewer frames are published than were fed (5 in) — the buffer dropped,
    // never grew (invariants #5 / #9). The published count reflects the drop.
    assert!(
        pump.published() < 5,
        "the late frame was dropped, not published: published={}",
        pump.published()
    );
    // And the surviving timeline never starts on a spurious late frame between
    // two real ones (monotonic already asserts no regression).
    assert!(!pts.is_empty(), "some frames still published: {pts:?}");
}
