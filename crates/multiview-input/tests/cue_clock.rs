//! Pure cue-clock projection must never consume video timestamps.
#![allow(clippy::unwrap_used, clippy::expect_used)]
use multiview_core::time::Rational;
use multiview_input::normalize::{PtsNormalizer, WrapBits};

const TS: Rational = Rational::new(1, 90_000);
const MOD: i64 = 1 << 33;
fn normalizer(tb: Rational) -> PtsNormalizer {
    PtsNormalizer::new(WrapBits::Mpeg33, tb, Rational::FPS_25)
}

#[test]
fn future_projection_does_not_change_video() {
    let mut n = normalizer(TS);
    let mut control = normalizer(TS);
    n.normalize(Some(90_000), 5_000_000_000).unwrap();
    control.normalize(Some(90_000), 5_000_000_000).unwrap();
    let snapshot = n.mapping_snapshot().unwrap();
    assert_eq!(
        snapshot
            .project(90_000 * 61, TS, WrapBits::Mpeg33)
            .unwrap()
            .as_nanos(),
        65_000_000_000
    );
    for raw in [93_600, 97_200, 100_800] {
        assert_eq!(
            n.normalize(Some(raw), 0).unwrap(),
            control.normalize(Some(raw), 0).unwrap()
        );
    }
}

#[test]
fn projects_both_directions_across_wrap_and_rejects_half_modulus() {
    let mut n = normalizer(TS);
    n.normalize(Some(MOD - 9_000), 1_000_000_000).unwrap();
    let s = n.mapping_snapshot().unwrap();
    assert_eq!(
        s.project(9_000, TS, WrapBits::Mpeg33).unwrap().as_nanos(),
        1_200_000_000
    );
    assert!(s
        .project((MOD - 9_000 + MOD / 2) % MOD, TS, WrapBits::Mpeg33)
        .is_none());
    n.normalize(Some(9_000), 0).unwrap();
    assert_eq!(
        n.mapping_snapshot()
            .unwrap()
            .project(MOD - 9_000, TS, WrapBits::Mpeg33)
            .unwrap()
            .as_nanos(),
        1_000_000_000
    );
}

#[test]
fn event_uses_its_own_timebase_without_rounding_anchor_to_event_ticks() {
    let mut n = normalizer(Rational::new(1, 1_000));
    n.normalize(Some(1_001), 2_000_000_000).unwrap();
    let s = n.mapping_snapshot().unwrap();
    assert_eq!(
        s.project(180_000, TS, WrapBits::Mpeg33).unwrap().as_nanos(),
        2_999_000_000
    );
    assert_eq!(
        s.project(2_000, Rational::new(1, 1_000), WrapBits::None)
            .unwrap()
            .as_nanos(),
        2_999_000_000
    );
    let mut fractional = normalizer(Rational::new(1, 3));
    fractional.normalize(Some(1), 0).unwrap();
    assert_eq!(
        fractional
            .mapping_snapshot()
            .unwrap()
            .project(1, Rational::new(1, 2), WrapBits::None)
            .unwrap()
            .as_nanos(),
        166_666_667
    );
}

#[test]
fn needs_valid_real_anchor() {
    let mut n = normalizer(TS);
    assert!(n.mapping_snapshot().is_none());
    n.normalize(None, 0).unwrap();
    n.normalize(None, 0).unwrap();
    assert!(n.mapping_snapshot().is_none());
    n.normalize(Some(90_000), 0).unwrap();
    assert!(n.mapping_snapshot().is_some());
    for tb in [
        Rational::new(0, 1),
        Rational::new(-1, 90_000),
        Rational::new(1, 0),
    ] {
        let mut invalid = normalizer(tb);
        let _ = invalid.normalize(Some(0), 0);
        assert!(invalid.mapping_snapshot().is_none());
    }
}

#[test]
fn discontinuity_invalidates_until_real_reanchor() {
    let mut n = normalizer(TS);
    n.normalize(Some(90_000), 0).unwrap();
    n.mark_discontinuity();
    assert!(n.mapping_snapshot().is_none());
    n.normalize(None, 0).unwrap();
    assert!(n.mapping_snapshot().is_none());
    let reanchor = n.normalize(Some(10), 0).unwrap();
    assert_eq!(
        n.mapping_snapshot()
            .unwrap()
            .project(10, TS, WrapBits::Mpeg33),
        Some(reanchor)
    );
    let jump = n.normalize(Some(90_000 * 60), 0).unwrap();
    assert_eq!(
        n.mapping_snapshot()
            .unwrap()
            .project(90_000 * 60, TS, WrapBits::Mpeg33),
        Some(jump)
    );
}

#[test]
fn generation_exposes_implicit_and_explicit_discontinuities_not_wraps() {
    let mut n = normalizer(TS);
    assert_eq!(n.mapping_generation(), Some(0));
    n.normalize(Some(MOD - 9_000), 0).unwrap();
    let pending = n.mapping_snapshot().unwrap();
    n.normalize(Some(9_000), 0).unwrap();
    assert_eq!(
        n.mapping_snapshot().unwrap().generation(),
        pending.generation()
    );
    n.normalize(Some(90_000 * 60), 0).unwrap();
    assert_eq!(n.mapping_generation(), Some(pending.generation() + 1));
    assert_ne!(
        n.mapping_snapshot().unwrap().generation(),
        pending.generation()
    );
    n.mark_discontinuity();
    let marked = n.mapping_generation();
    assert_eq!(marked, Some(pending.generation() + 2));
    assert!(n.mapping_snapshot().is_none());
    n.normalize(Some(0), 0).unwrap();
    assert_eq!(n.mapping_generation(), marked);
    assert_eq!(Some(n.mapping_snapshot().unwrap().generation()), marked);
}

#[test]
fn rejects_invalid_events_and_checked_overflow() {
    let mut n = normalizer(TS);
    n.normalize(Some(0), i64::MAX - 1).unwrap();
    let s = n.mapping_snapshot().unwrap();
    assert!(s.project(1, TS, WrapBits::None).is_none());
    assert!(s.project(MOD, TS, WrapBits::Mpeg33).is_none());
    assert!(s.project(-1, TS, WrapBits::Mpeg33).is_none());
    for tb in [
        Rational::new(0, 1),
        Rational::new(1, 0),
        Rational::new(1, -1),
        Rational::new(i64::MAX, 1),
    ] {
        assert!(s.project(i64::MAX, tb, WrapBits::None).is_none());
    }
    let mut invalid =
        PtsNormalizer::new(WrapBits::None, Rational::new(i64::MAX, 1), Rational::FPS_25);
    invalid.normalize(Some(i64::MAX), 0).unwrap();
    assert!(invalid.mapping_snapshot().is_none());
}
