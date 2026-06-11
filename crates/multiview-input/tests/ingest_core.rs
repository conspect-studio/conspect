//! Integration tests for the pure-Rust ingest core: the producer-agnostic
//! [`IngestPump`] that wires decode → normalize → jitter → last-good-frame store.
//!
//! These run in the DEFAULT build (no `ffmpeg`): they drive the pump with a
//! synthetic [`FrameProducer`] so the timing/resilience contract is pinned
//! without any native dependency. The pump MUST publish frames into the store
//! with strictly-monotonic, normalized nanosecond PTS (invariants #1/#2/#3) and
//! MUST never lose the last-good frame.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::collections::VecDeque;

use multiview_core::time::{MediaTime, Rational};
use multiview_framestore::{TileRead, TileStore};
use multiview_input::normalize::WrapBits;
use multiview_input::source::{
    FrameProducer, IngestConfig, IngestPump, ProducedFrame, StoredFrame,
};

/// The 90 kHz MPEG-TS timebase (1/90000 s per tick).
fn ts_tb() -> Rational {
    Rational::new(1, 90_000)
}

/// A scripted producer that hands out a fixed sequence of frames, then EOS.
///
/// Each scripted entry is `(raw_pts, discontinuity)`. The producer reports a
/// fixed timebase / cadence / wrap width so the pump builds a deterministic
/// normalizer.
struct ScriptedProducer {
    frames: VecDeque<ProducedFrame>,
    timebase: Rational,
    cadence: Rational,
    wrap: WrapBits,
    /// If set, [`FrameProducer::next_frame`] returns this error once the script
    /// is exhausted instead of clean EOS (to exercise fault propagation).
    fault_at_end: bool,
}

impl ScriptedProducer {
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
            fault_at_end: false,
        }
    }

    fn with_fault_at_end(mut self) -> Self {
        self.fault_at_end = true;
        self
    }
}

impl FrameProducer for ScriptedProducer {
    fn next_frame(&mut self) -> multiview_input::Result<Option<ProducedFrame>> {
        match self.frames.pop_front() {
            Some(f) => Ok(Some(f)),
            None if self.fault_at_end => {
                Err(multiview_input::Error::Ingest("scripted fault".to_owned()))
            }
            None => Ok(None),
        }
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

/// The normalized PTS (ns) of whatever frame currently occupies the store's
/// last-good slot, independent of the staleness ladder. Reading the slot
/// directly (rather than `read(now)`) lets a test inspect the published instant
/// without having to track a `now` against the freshness thresholds.
fn slot_pts_ns(store: &TileStore<StoredFrame>) -> Option<i64> {
    store.slot().load().map(|f| f.meta.pts.as_nanos())
}

#[test]
fn run_to_end_publishes_every_frame_with_monotonic_ns_pts() {
    // 5 frames at 25 fps on a 90 kHz timebase: raw PTS 0, 3600, 7200, ... ticks.
    let script: Vec<(Option<i64>, bool)> = (0..5).map(|i| (Some(i * 3_600), false)).collect();
    let mut producer = ScriptedProducer::new(script, ts_tb(), Rational::FPS_25, WrapBits::None);

    // Capture the published instants by reading after each pump step.
    let store: TileStore<StoredFrame> = TileStore::with_defaults("cam-1");
    let mut pump = IngestPump::new(&producer, IngestConfig::default());

    let anchor = MediaTime::from_nanos(1_000_000_000); // master_now = 1 s
    let mut seen: Vec<i64> = Vec::new();

    while pump
        .pump_one(&mut producer, &store, anchor)
        .expect("pump must not fault on a clean script")
    {
        let pts = slot_pts_ns(&store).expect("a frame must be held after a pump step");
        seen.push(pts);
    }

    assert_eq!(pump.published(), 5, "all five frames published");
    assert_eq!(seen.len(), 5, "saw a published frame after each pump step");

    // First frame anchors to master_now (1 s); each subsequent frame advances by
    // exactly one 25 fps period (40 ms) since raw deltas are 3600 ticks @ 90 kHz.
    assert_eq!(seen[0], 1_000_000_000, "first frame anchors to master_now");
    assert_eq!(seen[1], 1_040_000_000, "+40 ms");
    assert_eq!(seen[4], 1_160_000_000, "+160 ms");

    // Strictly monotonic: never a backwards or duplicate normalized PTS.
    for w in seen.windows(2) {
        assert!(
            w[1] > w[0],
            "normalized PTS must strictly increase: {seen:?}"
        );
    }
}

#[test]
fn missing_pts_uses_genpts_cadence_fallback() {
    // No raw PTS at all: the normalizer synthesizes one frame period per frame
    // from the declared cadence (30 fps → 33.333… ms steps).
    let script: Vec<(Option<i64>, bool)> = vec![(None, false), (None, false), (None, false)];
    let mut producer = ScriptedProducer::new(script, ts_tb(), Rational::FPS_30, WrapBits::None);

    let store: TileStore<StoredFrame> = TileStore::with_defaults("genpts");
    let mut pump = IngestPump::new(&producer, IngestConfig::default());

    let mut seen: Vec<i64> = Vec::new();
    while pump
        .pump_one(&mut producer, &store, MediaTime::ZERO)
        .expect("genpts pump must not fault")
    {
        seen.push(slot_pts_ns(&store).expect("frame held"));
    }

    assert_eq!(seen.len(), 3, "three synthesized frames");
    // 30 fps period in ns = 1e9/30 = 33_333_333 (floor). The first frame anchors
    // at master_now = 0, then advances one period at a time.
    let period = 33_333_333_i64;
    assert_eq!(seen[0], 0, "first synthesized frame anchors at master_now");
    assert_eq!(seen[1], period, "second frame one period later");
    assert!(
        seen[2] > seen[1] && seen[2] <= 2 * period + 1,
        "third advances another period: {seen:?}"
    );
    for w in seen.windows(2) {
        assert!(w[1] > w[0], "monotonic genpts: {seen:?}");
    }
}

#[test]
fn explicit_discontinuity_reanchors_forward_not_backward() {
    // Frame 0 at raw 0; frame 1 marked discontinuous with a raw PTS that jumps
    // BACKWARD by an hour. A naive pass-through would emit a backwards PTS; the
    // pump must re-anchor forward by one frame period instead (invariant #3).
    let big_back = -(3_600_i64 * 90_000); // -1 hour @ 90 kHz, in ticks
    let script: Vec<(Option<i64>, bool)> = vec![
        (Some(0), false),
        (Some(big_back), true),
        (Some(big_back + 3_600), false),
    ];
    let mut producer = ScriptedProducer::new(script, ts_tb(), Rational::FPS_25, WrapBits::None);

    let store: TileStore<StoredFrame> = TileStore::with_defaults("disc");
    let mut pump = IngestPump::new(&producer, IngestConfig::default());

    let mut seen: Vec<i64> = Vec::new();
    while pump
        .pump_one(&mut producer, &store, MediaTime::ZERO)
        .expect("discontinuity pump must not fault")
    {
        seen.push(slot_pts_ns(&store).expect("frame held"));
    }

    assert_eq!(seen.len(), 3);
    // Re-anchor continues forward from the last emitted instant by one 40 ms
    // period, NOT a backwards hour.
    assert_eq!(seen[0], 0, "anchor");
    assert_eq!(seen[1], 40_000_000, "re-anchored forward one 25 fps period");
    assert!(
        seen[2] > seen[1],
        "post-discontinuity stays forward: {seen:?}"
    );
}

#[test]
fn ts_wrap_is_unwrapped_into_continuous_ns() {
    // Straddle the 33-bit MPEG-TS wrap boundary: the last value before 2^33 and
    // the first after it must come out as two adjacent forward instants, never a
    // ~26.5 h backwards jump.
    let modulus = 1_i64 << 33;
    let step = 3_600_i64; // one 25 fps frame @ 90 kHz
    let before = modulus - step; // last tick before wrap
    let after = 0; // wraps to 0
    let script: Vec<(Option<i64>, bool)> = vec![
        (Some(before - step), false),
        (Some(before), false),
        (Some(after), false),
    ];
    let mut producer = ScriptedProducer::new(script, ts_tb(), Rational::FPS_25, WrapBits::Mpeg33);

    let store: TileStore<StoredFrame> = TileStore::with_defaults("wrap");
    // Use a large discontinuity threshold so the wrapped delta (which the
    // unwrapper makes small) is NOT mistaken for a discontinuity.
    let config = IngestConfig {
        discontinuity_ns: 60_000_000_000,
        ..IngestConfig::default()
    };
    let mut pump = IngestPump::new(&producer, config);

    let mut seen: Vec<i64> = Vec::new();
    while pump
        .pump_one(&mut producer, &store, MediaTime::ZERO)
        .expect("wrap pump must not fault")
    {
        seen.push(slot_pts_ns(&store).expect("frame held"));
    }

    assert_eq!(seen.len(), 3);
    // Each step is exactly one 40 ms frame across the wrap boundary.
    assert_eq!(seen[1] - seen[0], 40_000_000, "uniform across pre-wrap");
    assert_eq!(
        seen[2] - seen[1],
        40_000_000,
        "uniform ACROSS the wrap boundary"
    );
}

#[test]
fn jitter_buffer_releases_in_pts_order_and_stays_monotonic() {
    // Frames arrive out of order (swap two), normalized PTS still increasing in
    // wall order. With a jitter window they must be released in PTS order.
    let script: Vec<(Option<i64>, bool)> = vec![
        (Some(0), false),
        (Some(7_200), false), // arrives before 3600
        (Some(3_600), false),
        (Some(10_800), false),
    ];
    let mut producer = ScriptedProducer::new(script, ts_tb(), Rational::FPS_25, WrapBits::None);

    let store: TileStore<StoredFrame> = TileStore::with_defaults("jitter");
    // Reorder depth 1: hold one frame back so a late lower-PTS frame can slot
    // into place before its higher-PTS neighbour is released.
    let config = IngestConfig {
        jitter_depth: 1,
        ..IngestConfig::default()
    };
    let mut pump = IngestPump::new(&producer, config);

    // Record the published instant after every pump step (a step may release 0 or
    // 1 frame; we sample whatever is now held when it changes).
    let mut last_held: Option<i64> = None;
    let mut releases: Vec<i64> = Vec::new();
    while pump
        .pump_one(&mut producer, &store, MediaTime::ZERO)
        .expect("jitter pump must not fault")
    {
        if let Some(p) = slot_pts_ns(&store) {
            if last_held != Some(p) {
                releases.push(p);
                last_held = Some(p);
            }
        }
    }
    // The final EOS flush releases the last held frame; capture it too.
    if let Some(p) = slot_pts_ns(&store) {
        if last_held != Some(p) {
            releases.push(p);
        }
    }

    // Releases must come out in PTS order despite the scrambled arrival order:
    // 0, 40 ms, 80 ms, 120 ms — the 40 ms frame slotted ahead of the 80 ms one.
    assert_eq!(
        releases,
        vec![0, 40_000_000, 80_000_000, 120_000_000],
        "jitter buffer released frames in PTS order: {releases:?}"
    );
    assert_eq!(pump.published(), 4, "all four frames eventually published");
}

#[test]
fn clean_eos_returns_false_without_fault() {
    let mut producer = ScriptedProducer::new(
        vec![(Some(0), false)],
        ts_tb(),
        Rational::FPS_25,
        WrapBits::None,
    );
    let store: TileStore<StoredFrame> = TileStore::with_defaults("eos");
    let mut pump = IngestPump::new(&producer, IngestConfig::default());

    assert!(pump
        .pump_one(&mut producer, &store, MediaTime::ZERO)
        .expect("first pump"));
    // Script exhausted: a clean EOS is Ok(false), NOT an error.
    assert!(!pump
        .pump_one(&mut producer, &store, MediaTime::ZERO)
        .expect("clean EOS must not be an error"));
}

#[test]
fn producer_fault_propagates_as_error_not_panic() {
    // A producer that faults at end-of-script must surface a typed error the
    // supervisor reconnects on — never a panic on the ingest hot path.
    let mut producer = ScriptedProducer::new(
        vec![(Some(0), false)],
        ts_tb(),
        Rational::FPS_25,
        WrapBits::None,
    )
    .with_fault_at_end();
    let store: TileStore<StoredFrame> = TileStore::with_defaults("fault");
    let mut pump = IngestPump::new(&producer, IngestConfig::default());

    assert!(pump
        .pump_one(&mut producer, &store, MediaTime::ZERO)
        .expect("first pump ok"));
    match pump.pump_one(&mut producer, &store, MediaTime::ZERO) {
        Err(multiview_input::Error::Ingest(msg)) => assert!(msg.contains("scripted fault")),
        other => panic!("expected an Ingest fault, got {other:?}"),
    }
}

#[test]
fn store_holds_last_good_frame_after_eos() {
    // After ingest ends, the store must still hand back the last-good frame (the
    // compositor keeps showing it until the staleness ladder degrades it).
    let mut producer = ScriptedProducer::new(
        vec![(Some(0), false), (Some(3_600), false)],
        ts_tb(),
        Rational::FPS_25,
        WrapBits::None,
    );
    let store: TileStore<StoredFrame> = TileStore::with_defaults("hold");
    let mut pump = IngestPump::new(&producer, IngestConfig::default());

    let published = pump
        .run_to_end(&mut producer, &store, MediaTime::ZERO)
        .expect("run to end");
    assert_eq!(published, 2);

    // Read at the last published instant (40 ms): still Fresh, last-good held.
    let read = store.read(MediaTime::from_nanos(40_000_000));
    match read {
        TileRead::Fresh { frame } => {
            assert_eq!(frame.meta.pts.as_nanos(), 40_000_000, "last-good frame PTS");
            assert_eq!(frame.meta.width, 16, "decoded geometry preserved");
        }
        other => panic!("expected Fresh last-good frame, got {other:?}"),
    }
}
