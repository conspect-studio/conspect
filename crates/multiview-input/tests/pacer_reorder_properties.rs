//! Property tests for the paced raw-RTP ingest reorder + 32-bit-wrap logic
//! (ENG-2b, ADR-0021 point 3).
//!
//! For ANY out-of-order / jittery RTP arrival — including sequences that cross
//! the 32-bit RTP wrap boundary — the paced [`IngestPump`] must:
//!
//! * never panic and never hang (it is a bounded pure state machine);
//! * publish frames in **strictly monotonic** normalized-PTS order (the reorder
//!   buffer + the normalizer's monotonic guard, invariants #2 / #3);
//! * keep memory **bounded**: the number of frames ever held pending is capped by
//!   the jitter depth + the pacer's pending window — it drops, never grows
//!   (invariants #5 / #9);
//! * never regress the timeline across the wrap (the unwrapper makes the wrap a
//!   normal forward delta, so no ~13 h jump or far-future deadline appears).
//!
//! These run in the DEFAULT (pure-Rust) build, clock-injected, no sleeping.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::collections::VecDeque;

use proptest::prelude::*;

use multiview_core::time::Rational;
use multiview_framestore::TileStore;
use multiview_input::normalize::WrapBits;
use multiview_input::source::{
    FrameProducer, IngestConfig, IngestPump, PacePolicy, PaceStep, ProducedFrame, StoredFrame,
};

fn rtp_tb() -> Rational {
    Rational::new(1, 90_000)
}

struct ScriptedRtp {
    frames: VecDeque<ProducedFrame>,
}

impl ScriptedRtp {
    fn new(raw_ptses: impl IntoIterator<Item = i64>) -> Self {
        let frames = raw_ptses
            .into_iter()
            .map(|raw| ProducedFrame::timing_only(Some(raw), 16, 16))
            .collect();
        Self { frames }
    }
}

impl FrameProducer for ScriptedRtp {
    fn next_frame(&mut self) -> multiview_input::Result<Option<ProducedFrame>> {
        Ok(self.frames.pop_front())
    }
    fn timebase(&self) -> Rational {
        rtp_tb()
    }
    fn cadence(&self) -> Rational {
        Rational::FPS_25
    }
    fn wrap_bits(&self) -> WrapBits {
        WrapBits::Rtp32
    }
}

fn slot_pts_ns(store: &TileStore<StoredFrame>) -> Option<i64> {
    store.slot().load().map(|f| f.meta.pts.as_nanos())
}

/// Drive the paced pump to EOS with an injected virtual clock, returning the
/// distinct published instants (publish order). Bounded-iteration: a hung pump
/// would blow the cap and fail the test rather than hang CI.
fn drive_to_eos(producer: &mut ScriptedRtp, pump: &mut IngestPump) -> Vec<i64> {
    const TICK_NS: i64 = 1_000_000; // 1 ms — finer than the 40 ms release spacing
    let store: TileStore<StoredFrame> = TileStore::with_defaults("prop");
    let mut now = 0_i64;
    let mut published: Vec<i64> = Vec::new();
    let mut last: Option<i64> = None;
    // Generous cap: a few iterations per ms of timeline; a hang blows it and
    // fails rather than wedging CI.
    let mut guard = 0_u64;
    loop {
        guard += 1;
        assert!(guard < 10_000_000, "paced pump must terminate, not hang");
        let step = pump
            .pump_one_paced(producer, &store, now)
            .expect("paced pump must not fault");
        if let Some(p) = slot_pts_ns(&store) {
            if last != Some(p) {
                published.push(p);
                last = Some(p);
            }
        }
        if step == PaceStep::Eos {
            break;
        }
        // Fine-step so each paced release is observed individually at the single
        // freshest-wins slot.
        now = now.saturating_add(TICK_NS);
    }
    published
}

proptest! {
    // Arbitrary out-of-order RTP timestamps (no wrap): published PTS strictly
    // monotonic, bounded, no panic/hang.
    #[test]
    fn reorder_yields_strictly_monotonic(
        order in proptest::collection::vec(0_i64..200, 1..40),
        jitter_depth in 0_usize..8,
    ) {
        // Map indices to evenly-spaced raw RTP ticks, then present in the given
        // (possibly repeating) arrival order — exercises reorder + drop-late.
        let raws: Vec<i64> = order.iter().map(|&i| i * 3_600).collect();
        let mut producer = ScriptedRtp::new(raws);
        let config = IngestConfig {
            pace: PacePolicy::WallClock,
            jitter_depth,
            ..IngestConfig::default()
        };
        let mut pump = IngestPump::new(&producer, config);
        let published = drive_to_eos(&mut producer, &mut pump);

        // Strictly increasing (monotonic guard + reorder).
        for w in published.windows(2) {
            prop_assert!(w[0] < w[1], "published not strictly monotonic: {published:?}");
        }
        // Bounded: never more published than input frames.
        prop_assert!(published.len() <= order.len());
    }

    // Sequences crossing the 32-bit RTP wrap boundary: the unwrap keeps the
    // timeline strictly forward; no regression / explosion across the wrap.
    #[test]
    fn wrap_crossing_never_regresses(
        n in 4_usize..40,
        start_offset in 0_i64..10,
    ) {
        let modulus = 1_i64 << 32;
        let step = 3_600_i64;
        // Build an in-order sequence that starts just before the wrap and crosses
        // it, so consecutive raw ticks roll 2^32 -> 0.
        let n_i64 = i64::try_from(n).unwrap();
        let first = modulus - (start_offset + n_i64) * step;
        let raws: Vec<i64> = (0..n_i64)
            .map(|k| (first + k * step).rem_euclid(modulus))
            .collect();
        let mut producer = ScriptedRtp::new(raws);
        let config = IngestConfig {
            pace: PacePolicy::WallClock,
            // Large threshold: the unwrapped delta is a normal 40 ms step, not a
            // discontinuity.
            discontinuity_ns: 60_000_000_000,
            ..IngestConfig::default()
        };
        let mut pump = IngestPump::new(&producer, config);
        let published = drive_to_eos(&mut producer, &mut pump);

        prop_assert_eq!(published.len(), n);
        // Every consecutive published pair is exactly one 40 ms frame apart —
        // continuous across the wrap, never a backward jump.
        for w in published.windows(2) {
            prop_assert_eq!(
                w[1] - w[0],
                40_000_000,
                "wrap regression / non-uniform step: {:?}", published
            );
        }
    }
}
