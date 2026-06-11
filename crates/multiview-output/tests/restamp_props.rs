//! GP-6 Piece A — property tests for the per-stream monotonic clamp+offset
//! restamp (ADR-0030 §4 "Re-stamp rule (#3 for the copy path)").
//!
//! The load-bearing invariant: across ANY input sequence interleaved with ANY
//! number of `rebase` re-anchors, the emitted DTS is ALWAYS strictly increasing
//! and `pts' >= dts'` always holds — so `av_interleaved_write_frame` never
//! aborts on non-monotonic / equal DTS, and no muxer ever sees pts < dts.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use multiview_output::restamp::RestampAccumulator;
use proptest::prelude::*;

/// One operation against the accumulator: either restamp a raw (dts, pts) pair,
/// or rebase the offset at a seam boundary with a raw dts anchor.
#[derive(Debug, Clone)]
enum Op {
    Restamp { raw_dts: i64, raw_pts_delta: i64 },
    Rebase { raw_dts: i64 },
}

fn op_strategy() -> impl Strategy<Value = Op> {
    prop_oneof![
        // Bound the raw range well inside i64 so no checked op can overflow even
        // after offset shifts; reorder delta is a non-negative pts-dts gap.
        (-1_000_000_000i64..1_000_000_000, 0i64..1000).prop_map(|(raw_dts, raw_pts_delta)| {
            Op::Restamp {
                raw_dts,
                raw_pts_delta,
            }
        }),
        (-1_000_000_000i64..1_000_000_000).prop_map(|raw_dts| Op::Rebase { raw_dts }),
    ]
}

proptest! {
    #[test]
    fn emitted_dts_always_strictly_increasing(ops in proptest::collection::vec(op_strategy(), 1..400)) {
        let mut acc = RestampAccumulator::new();
        let mut last: Option<i64> = None;
        for op in ops {
            match op {
                Op::Restamp { raw_dts, raw_pts_delta } => {
                    let raw_pts = raw_dts.saturating_add(raw_pts_delta);
                    let (dts, pts) = acc.restamp(raw_dts, raw_pts);
                    if let Some(prev) = last {
                        prop_assert!(dts > prev, "emitted DTS must be strictly increasing: prev={prev} dts={dts}");
                    }
                    prop_assert!(pts >= dts, "pts' >= dts' must always hold: pts={pts} dts={dts}");
                    last = Some(dts);
                }
                Op::Rebase { raw_dts } => {
                    acc.rebase(raw_dts);
                    // A rebase emits nothing; the NEXT restamp is anchored at
                    // last_dts+1 and is checked by the strictly-increasing rule.
                }
            }
        }
    }
}
