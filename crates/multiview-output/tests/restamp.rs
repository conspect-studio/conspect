//! GP-6 Piece A — per-stream monotonic clamp+offset restamp (ADR-0030 §4
//! "Re-stamp rule (#3 for the copy path)").
//!
//! These tests pin the COPY-path invariant-#3 obligation: a guarded passthrough
//! COPIES coded packets (no encode) and must re-stamp their PTS/DTS so the muxer
//! never aborts (`av_interleaved_write_frame` rejects non-monotonic DTS;
//! mp4/mov even on *equal* DTS) AND B-frame reorder + raw deltas are preserved.
//! This is DISTINCT from the encoder path's `out_pts = f(tick)` (which would
//! collapse B-frame DTS reorder and must NEVER be used here).
//!
//! The load-bearing guard is the clamp (`last_dts + 1`), NOT any `FFmpeg` flag.
//! Pure `i64` arithmetic; always-compiled in the default build (no `ffmpeg`).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
// Justification: DTS and PTS are the irreducible domain terms of this module; the
// `raw_dts`/`raw_pts` and `emitted_dts`/`emitted_pts` pairs differ only in that
// canonical d/p suffix, which pedantic `similar_names` flags. Renaming would
// obscure exactly the quantities under test (decode vs presentation timestamps),
// so the lint is allowed here only.
#![allow(clippy::similar_names)]

use multiview_output::restamp::RestampAccumulator;

/// A monotonically-increasing input (a B-free, DTS==PTS stream) passes through
/// completely unchanged with a fresh accumulator (offset 0, no clamping fires).
#[test]
fn monotonic_input_passes_through_unchanged() {
    let mut acc = RestampAccumulator::new();
    let dtss = [0i64, 1000, 2000, 3000, 4000];
    for &d in &dtss {
        let (dts, pts) = acc.restamp(d, d);
        assert_eq!(dts, d, "monotonic dts must pass through unchanged");
        assert_eq!(pts, d, "monotonic pts must pass through unchanged");
    }
}

/// A B-pyramid GOP (dts < pts within a GOP, reorder present) emits strictly
/// increasing DTS, preserves `pts' >= dts'`, AND preserves the raw `pts - dts`
/// reorder deltas (so the decoder reorder buffer still works).
#[test]
fn b_pyramid_preserves_reorder_and_deltas() {
    let mut acc = RestampAccumulator::new();
    // A VALID B-pyramid GOP (a real coded stream always has pts >= dts — a frame
    // is never displayed before it is decoded). With a reorder delay of 2 ticks
    // the DTS lags the PTS so every frame keeps pts >= dts while the reorder gap
    // (pts - dts) is non-trivial and varies per frame:
    //   decode order:  I       P       B       B
    //   raw dts:      -2      -1       0       1   (strictly increasing)
    //   raw pts:       0       3       1       2   (display order; pts >= dts)
    //   reorder gap:   2       4       1       1
    let raw_dts = [-2i64, -1, 0, 1];
    let raw_pts = [0i64, 3, 1, 2];

    let mut emitted_dts = Vec::new();
    let mut emitted_pts = Vec::new();
    for i in 0..raw_dts.len() {
        let (dts, pts) = acc.restamp(raw_dts[i], raw_pts[i]);
        emitted_dts.push(dts);
        emitted_pts.push(pts);
    }

    // DTS strictly increasing.
    for w in emitted_dts.windows(2) {
        assert!(
            w[1] > w[0],
            "emitted DTS must be strictly increasing: {emitted_dts:?}"
        );
    }
    // pts' >= dts' at every AU.
    for i in 0..emitted_dts.len() {
        assert!(
            emitted_pts[i] >= emitted_dts[i],
            "pts' >= dts' must hold at {i}: pts={} dts={}",
            emitted_pts[i],
            emitted_dts[i]
        );
    }
    // Raw reorder deltas (pts - dts) preserved exactly.
    for i in 0..raw_dts.len() {
        let raw_delta = raw_pts[i] - raw_dts[i];
        let emitted_delta = emitted_pts[i] - emitted_dts[i];
        assert_eq!(
            emitted_delta, raw_delta,
            "raw pts-dts delta must be preserved at {i}"
        );
    }
}

/// A boundary `rebase` re-anchors `offset` so the next emitted DTS is exactly
/// `last_dts + 1`, and the run continues strictly increasing across the seam
/// while raw deltas within the new run still pass through.
#[test]
fn rebase_anchors_next_dts_and_continues_monotonic() {
    let mut acc = RestampAccumulator::new();
    // First run: input packets at raw dts 0,1,2.
    let mut last_emitted = i64::MIN;
    for d in 0..=2 {
        let (dts, _pts) = acc.restamp(d, d);
        assert!(dts > last_emitted);
        last_emitted = dts;
    }
    // last_dts is now 2 (offset 0). Now a slate run begins whose raw first DTS
    // is 500 (a totally different timeline). Rebase at that seam boundary.
    acc.rebase(500);
    // The next emitted DTS must be exactly last_dts + 1 == 3.
    let (dts0, pts0) = acc.restamp(500, 500);
    assert_eq!(
        dts0,
        last_emitted + 1,
        "first post-rebase dts == last_dts+1"
    );
    assert_eq!(pts0, dts0);
    // The rest of the slate run advances by its raw deltas (510-500 == 10).
    let (dts1, _pts1) = acc.restamp(510, 510);
    assert_eq!(dts1 - dts0, 10, "raw delta passes through post-rebase");
    assert!(dts1 > dts0);

    // Now a SECOND seam back to the original timeline (input recovery): raw dts
    // resets to 3. Rebase again; emitted dts must stay strictly increasing.
    acc.rebase(3);
    let (dts2, _pts2) = acc.restamp(3, 3);
    assert_eq!(dts2, dts1 + 1, "second seam anchors at last_dts+1");
}

/// A duplicate / equal raw DTS still emits strictly-increasing DTS via the +1
/// clamp — this is the mp4/mov equal-DTS abort guard.
#[test]
fn equal_raw_dts_still_strictly_increasing() {
    let mut acc = RestampAccumulator::new();
    let (d0, _) = acc.restamp(100, 100);
    let (d1, _) = acc.restamp(100, 100); // duplicate raw dts
    let (d2, _) = acc.restamp(100, 100); // triplicate
    assert!(d1 > d0, "equal raw dts must clamp to last_dts+1: {d0} {d1}");
    assert!(d2 > d1, "equal raw dts must clamp again: {d1} {d2}");
    assert_eq!(d1, d0 + 1);
    assert_eq!(d2, d1 + 1);
}

/// Negative leading values (an `avoid_negative_ts`-style stream that has not yet
/// had its one-shot shift) are handled: the clamp keeps DTS strictly increasing
/// and `pts' >= dts'` holds even when raw pts < raw dts after offset.
#[test]
fn negative_leading_values_handled() {
    let mut acc = RestampAccumulator::new();
    let raw = [-1000i64, -999, -998, -997];
    let mut last = i64::MIN;
    for &d in &raw {
        let (dts, pts) = acc.restamp(d, d);
        assert!(dts > last, "negative leading dts still increasing: {dts}");
        assert!(pts >= dts);
        last = dts;
    }
    // First emitted dts is the raw value (offset 0) — the leading shift is a
    // muxer one-shot (avoid_negative_ts=make_zero), not the restamp's job.
    let mut acc2 = RestampAccumulator::new();
    let (d0, _) = acc2.restamp(-1000, -1000);
    assert_eq!(d0, -1000);
}

/// `pts' = max(raw_pts + offset, dts')`: a pts below dts is lifted to dts so the
/// muxer never sees pts < dts (which it rejects), without disturbing dts.
#[test]
fn pts_never_below_dts() {
    let mut acc = RestampAccumulator::new();
    // First packet establishes last_dts = 10.
    let _ = acc.restamp(10, 10);
    // Next raw dts equals 10 (clamps to 11), raw pts is 5 (below dts') -> pts
    // must be lifted to 11.
    let (dts, pts) = acc.restamp(10, 5);
    assert_eq!(dts, 11);
    assert_eq!(pts, 11, "pts below dts' must be lifted to dts'");
}
