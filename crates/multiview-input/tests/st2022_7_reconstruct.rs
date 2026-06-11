//! Concrete (golden-case) unit tests for the ST 2022-7 hitless dual-path
//! reconstructor: in-order merge, de-duplication, true-gap handling, and the
//! bounded window-slide eviction. These complement the property tests with
//! hand-checked scenarios. Pure-Rust default build.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use multiview_input::st2022_7::{HitlessReconstructor, Path, PushOutcome};

/// Two paths each missing alternate packets reconstruct the full stream with no
/// gap (the canonical ST 2022-7 hitless case): A carries 0,2,4; B carries 1,3.
#[test]
fn alternating_loss_reconstructs_fully() {
    // depth 0 so we release eagerly in this perfectly-ordered scenario.
    let mut r: HitlessReconstructor<u16> = HitlessReconstructor::with_reorder_depth(16, 0);
    let mut out = Vec::new();
    // Interleave arrivals in sequence order across the two paths.
    out.extend_push(&mut r, Path::A, 0);
    out.extend_push(&mut r, Path::B, 1);
    out.extend_push(&mut r, Path::A, 2);
    out.extend_push(&mut r, Path::B, 3);
    out.extend_push(&mut r, Path::A, 4);
    out.extend(r.flush());
    assert_eq!(out, vec![0, 1, 2, 3, 4]);
}

/// A duplicate copy on the second path is discarded (de-dup).
#[test]
fn duplicate_is_discarded() {
    let mut r: HitlessReconstructor<u16> = HitlessReconstructor::with_reorder_depth(16, 4);
    assert_eq!(r.push(Path::A, 7, 7), PushOutcome::Accepted);
    assert_eq!(r.push(Path::B, 7, 7), PushOutcome::Duplicate);
    assert_eq!(r.buffered(), 1);
    assert_eq!(r.flush(), vec![7]);
}

/// A sequence lost on BOTH paths is a true gap: the output skips it and keeps
/// flowing (no stall), in order around the hole.
#[test]
fn both_path_loss_is_a_gap_not_a_stall() {
    let mut r: HitlessReconstructor<u16> = HitlessReconstructor::with_reorder_depth(4, 0);
    let mut out = Vec::new();
    // seq 5 never arrives on either path. Window capacity 4 forces the slide
    // that declares 5 a gap once 6,7,8,9 pile up behind it.
    out.extend_push(&mut r, Path::A, 5 + 1); // 6
    out.extend_push(&mut r, Path::A, 5 + 2); // 7
    out.extend_push(&mut r, Path::A, 5 + 3); // 8
    out.extend_push(&mut r, Path::A, 5 + 4); // 9
    out.extend_push(&mut r, Path::A, 5 + 5); // 10 -> slides window, evicts 6 as it is oldest
    out.extend(r.flush());
    // 5 is never present; the stream flows 6..=10 in order with no duplicate and
    // no stall.
    for w in out.windows(2) {
        assert!(w[0] < w[1], "not increasing around the gap: {out:?}");
    }
    assert!(!out.contains(&5));
    assert!(out.contains(&6));
    assert!(out.contains(&10));
}

/// Once a sequence has been released, a very-late duplicate of it is rejected as
/// too late (its slot already passed).
#[test]
fn late_duplicate_after_release_is_too_late() {
    let mut r: HitlessReconstructor<u16> = HitlessReconstructor::with_reorder_depth(8, 0);
    assert_eq!(r.push(Path::A, 100, 100), PushOutcome::Accepted);
    let released = r.drain();
    assert_eq!(released, vec![100]);
    assert_eq!(r.released_through(), Some(100));
    // The redundant copy from path B arrives far too late.
    assert_eq!(r.push(Path::B, 100, 100), PushOutcome::TooLate);
}

/// The window never holds more than its capacity, even under a flood.
#[test]
fn window_is_strictly_bounded() {
    let mut r: HitlessReconstructor<u16> = HitlessReconstructor::new(4);
    for s in 0u16..1000 {
        r.push(Path::A, s, s);
        assert!(r.buffered() <= 4);
    }
}

/// A reordered earlier packet that arrives before its slot has been released is
/// still placed correctly in order (within the hold-back depth).
#[test]
fn reordered_earlier_packet_slots_in_order() {
    let mut r: HitlessReconstructor<u16> = HitlessReconstructor::with_reorder_depth(16, 4);
    // Arrive out of order: 2, then 1, then 0. Hold-back depth lets the earlier
    // ones slot in before release.
    assert_eq!(r.push(Path::A, 2, 2), PushOutcome::Accepted);
    assert_eq!(r.push(Path::A, 1, 1), PushOutcome::Accepted);
    assert_eq!(r.push(Path::A, 0, 0), PushOutcome::Accepted);
    let mut out = r.drain();
    out.extend(r.flush());
    assert_eq!(out, vec![0, 1, 2]);
}

/// Small helper extension: push a value and collect anything it lets us drain.
trait PushDrain {
    fn extend_push(&mut self, r: &mut HitlessReconstructor<u16>, path: Path, seq: u16);
}
impl PushDrain for Vec<u16> {
    fn extend_push(&mut self, r: &mut HitlessReconstructor<u16>, path: Path, seq: u16) {
        let _ = r.push(path, seq, seq);
        self.extend(r.drain());
    }
}
