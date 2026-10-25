//! SW-0 acceptance tests: switcher core types and exact-rational timing.
//!
//! Pins [ADR-T015](../../../docs/decisions/ADR-T015.md) (integer frame counts at
//! the exact rational cadence, half-up ms conversion with a 1-frame floor, and
//! `progress = f(tick)`) plus the descriptor serde discipline of
//! [ADR-0055](../../../docs/decisions/ADR-0055.md) /
//! [ADR-0056](../../../docs/decisions/ADR-0056.md) (`#[non_exhaustive]`,
//! internally tagged, never untagged).
//!
//! Integration tests do not inherit `clippy.toml`'s test relaxations, so this
//! file opts out of the panic-bearing lints explicitly (CLAUDE.md §A.1).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use multiview_core::switcher::{
    kernel, progress, ArmedSet, DveDirection, FrameCount, KeySource, KeyType, KeyerDescriptor,
    KeyerMode, KeyerTransition, PixelRect, Progress, SourceRef, TapPoint, TransitionDescriptor,
    WipePattern,
};
use multiview_core::time::Rational;
use proptest::prelude::*;
use serde_json::{json, Value};

/// The 29.97 NTSC cadence as an exact rational — never a float fps.
const NTSC_30: Rational = Rational::FPS_29_97;
/// The 59.94 NTSC cadence as an exact rational.
const NTSC_60: Rational = Rational::FPS_59_94;

// ---------------------------------------------------------------------------
// FrameCount: integer frames at the exact rational cadence (ADR-T015 §1/§2)
// ---------------------------------------------------------------------------

#[test]
fn ms_to_frames_matches_the_adr_t015_normative_vectors() {
    // The worked examples of ADR-T015 §2 are normative test vectors.
    assert_eq!(FrameCount::from_ms(1000, NTSC_60).get(), 60);
    assert_eq!(FrameCount::from_ms(250, NTSC_60).get(), 15);
    assert_eq!(FrameCount::from_ms(1000, NTSC_30).get(), 30);
    // 20 ms at 25/1 is exactly 0.5 frames: round half up.
    assert_eq!(FrameCount::from_ms(20, Rational::FPS_25).get(), 1);
    // 8 ms at 25/1 is 0.2 frames: floored to 0, then clamped to the 1-frame floor.
    assert_eq!(FrameCount::from_ms(8, Rational::FPS_25).get(), 1);
}

#[test]
fn ms_to_frames_never_yields_a_zero_frame_no_op() {
    assert_eq!(FrameCount::from_ms(0, NTSC_30).get(), 1);
    assert_eq!(FrameCount::from_ms(0, Rational::FPS_60).get(), 1);
    assert_eq!(FrameCount::from_ms(1, NTSC_30).get(), 1);
    // A degenerate cadence cannot convert; the 1-frame floor is the safe answer.
    assert_eq!(FrameCount::from_ms(1000, Rational::new(0, 1)).get(), 1);
    assert_eq!(FrameCount::from_ms(1000, Rational::new(30_000, 0)).get(), 1);
    assert_eq!(
        FrameCount::from_ms(1000, Rational::new(-30_000, 1_001)).get(),
        1
    );
}

#[test]
fn frame_count_echoes_the_quantised_milliseconds() {
    // The ADR-T015 §2 "exact duration" column, rounded to whole milliseconds.
    assert_eq!(FrameCount::try_from(60).unwrap().to_ms(NTSC_60), 1001);
    assert_eq!(FrameCount::try_from(15).unwrap().to_ms(NTSC_60), 250); // 250.25 ms
    assert_eq!(FrameCount::try_from(30).unwrap().to_ms(NTSC_30), 1001);
    assert_eq!(FrameCount::try_from(1).unwrap().to_ms(Rational::FPS_25), 40);
    // Degenerate cadence: no wall-clock answer.
    assert_eq!(FrameCount::ONE.to_ms(Rational::new(0, 1)), 0);
}

#[test]
fn a_frame_count_is_never_zero() {
    // The minimum-1-frame rule is enforced by the type, not by callers.
    assert_eq!(
        FrameCount::try_from(0),
        Err(multiview_core::switcher::FrameCountError::Zero)
    );
    assert_eq!(FrameCount::try_from(1).unwrap(), FrameCount::ONE);
    assert_eq!(FrameCount::try_from(30).unwrap().get(), 30);
    assert!(FrameCount::ONE.is_one());
}

#[test]
fn frame_count_serialises_as_a_bare_integer_and_rejects_zero() {
    let thirty = FrameCount::try_from(30).unwrap();
    assert_eq!(serde_json::to_value(thirty).unwrap(), json!(30));
    assert_eq!(
        serde_json::from_value::<FrameCount>(json!(30)).unwrap(),
        thirty
    );
    assert!(serde_json::from_value::<FrameCount>(json!(0)).is_err());
    assert!(serde_json::from_value::<FrameCount>(json!({"frames": 30})).is_err());
}

#[test]
fn duration_is_in_wall_terms_only_through_the_cadence() {
    // ADR-T015 Consequences: frame counts are the invariant under a cadence
    // change; a 30-frame mix is 1.001 s at 30000/1001 and 0.6 s at 50/1.
    let thirty = FrameCount::try_from(30).unwrap();
    assert_eq!(thirty.to_ms(NTSC_30), 1001);
    assert_eq!(thirty.to_ms(Rational::FPS_50), 600);
}

proptest! {
    /// ms → frames → ms round trip stays inside half a frame period plus half a
    /// millisecond at 30000/1001. The lower bound excludes the 1-frame floor
    /// region, where the clamp deliberately widens the error.
    #[test]
    fn prop_ms_round_trip_bounded_at_ntsc30(ms in 17_u32..=10_000_000) {
        let frames = FrameCount::from_ms(ms, NTSC_30);
        let back = frames.to_ms(NTSC_30);
        let delta = (i128::from(back) - i128::from(ms)).abs();
        // |Δ|·2·num ≤ 1000·den + num  ⇔  |Δ| ≤ period/2 + 1/2
        let lhs = delta * 2 * i128::from(NTSC_30.num);
        let rhs = 1000 * i128::from(NTSC_30.den) + i128::from(NTSC_30.num);
        prop_assert!(lhs <= rhs, "ms={ms} frames={} back={back}", frames.get());
    }

    /// Every conversion yields at least one frame, whatever the input.
    #[test]
    fn prop_ms_conversion_is_always_at_least_one_frame(ms in any::<u32>()) {
        prop_assert!(FrameCount::from_ms(ms, NTSC_30).get() >= 1);
        prop_assert!(FrameCount::from_ms(ms, Rational::FPS_25).get() >= 1);
    }
}

// ---------------------------------------------------------------------------
// progress(tick_index, start_tick, duration_frames): pure exact rational
// ---------------------------------------------------------------------------

#[test]
fn progress_hits_exactly_zero_and_one() {
    let duration = FrameCount::try_from(30).unwrap();
    let start = 100_u64;

    assert_eq!(progress(0, start, duration), Progress::ZERO);
    assert_eq!(progress(start, start, duration), Progress::ZERO);
    assert_eq!(progress(start + 1, start, duration).numerator(), 1);
    assert_eq!(progress(start + 1, start, duration).denominator(), 30);
    assert_eq!(progress(start + 30, start, duration), Progress::COMPLETE);
    assert_eq!(progress(start + 31, start, duration), Progress::COMPLETE);
    assert_eq!(progress(u64::MAX, start, duration), Progress::COMPLETE);
}

#[test]
fn progress_is_reduced_and_ordered_by_value() {
    let six = FrameCount::try_from(6).unwrap();
    let half = Progress::new(3, six); // 1/2
    let third = Progress::new(2, six); // 1/3
    assert!(third < half);
    assert!(Progress::ZERO < third);
    assert!(half < Progress::COMPLETE);
    // Equality is by value: 1/2 from a 6-frame window equals 1/2 from a 4-frame one.
    let four = FrameCount::try_from(4).unwrap();
    assert_eq!(half, Progress::new(2, four));
    assert_eq!(half.numerator(), 1);
    assert_eq!(half.denominator(), 2);
    // Integer wire form (ADR-T015 §3): basis points, never a float.
    assert_eq!(Progress::ZERO.to_basis_points(), 0);
    assert_eq!(half.to_basis_points(), 5_000);
    assert_eq!(third.to_basis_points(), 3_333);
    assert_eq!(Progress::COMPLETE.to_basis_points(), 10_000);
    assert!(Progress::ZERO.is_zero());
    assert!(Progress::COMPLETE.is_complete());
    assert!(!half.is_zero() && !half.is_complete());
}

#[test]
fn a_one_frame_duration_completes_on_the_next_tick() {
    // `progress = clamp((tick − start) / d, 0, 1)` (ADR-0055 decision 1), so the
    // degenerate one-frame window reads 0 at its start tick and exactly 1 at the
    // following tick.
    let duration = FrameCount::ONE;
    assert_eq!(progress(9, 10, duration), Progress::ZERO);
    assert_eq!(progress(10, 10, duration), Progress::ZERO);
    assert_eq!(progress(11, 10, duration), Progress::COMPLETE);
    assert_eq!(progress(12, 10, duration), Progress::COMPLETE);
}

proptest! {
    /// Progress is monotonic in the tick index, and strictly increasing inside
    /// the open window.
    #[test]
    fn prop_progress_is_monotonic(
        start in 0_u64..1_000_000,
        duration in 1_u32..10_000,
        a in 0_u64..2_000_000,
        b in 0_u64..2_000_000,
    ) {
        let d = FrameCount::try_from(duration).unwrap();
        let (lo, hi) = if a <= b { (a, b) } else { (b, a) };
        let (lo_p, hi_p) = (progress(lo, start, d), progress(hi, start, d));
        prop_assert!(lo_p <= hi_p);
        if lo > start && hi > lo && hi - start < u64::from(duration) {
            prop_assert!(lo_p < hi_p, "lo={lo} hi={hi} start={start} d={duration}");
        }
    }

    /// Progress is a total order, and it is order-isomorphic to the tick order:
    /// permuting the input ticks cannot change the sorted progress sequence.
    #[test]
    fn prop_progress_is_a_total_order_under_tick_permutation(
        start in 0_u64..100_000,
        duration in 1_u32..2_000,
        ticks in prop::collection::vec(0_u64..250_000, 1..40),
    ) {
        let d = FrameCount::try_from(duration).unwrap();
        let progresses: Vec<Progress> =
            ticks.iter().map(|&t| progress(t, start, d)).collect();

        // Trichotomy: exactly one of <, ==, > holds for every pair.
        for &a in &progresses {
            for &b in &progresses {
                let exactly_one = usize::from(a < b) + usize::from(a == b) + usize::from(a > b);
                prop_assert_eq!(exactly_one, 1);
            }
        }

        // Sort the same multiset in tick order; the induced progress sequence is
        // already sorted (monotonicity), so any input permutation sorts equal.
        let mut by_tick = ticks.clone();
        by_tick.sort_unstable();
        let seq: Vec<Progress> = by_tick.iter().map(|&t| progress(t, start, d)).collect();
        let mut sorted = progresses.clone();
        sorted.sort_unstable();
        prop_assert_eq!(seq, sorted);
    }
}

// ---------------------------------------------------------------------------
// The time-math surface is float-free; one bridge exists at the kernel boundary
// ---------------------------------------------------------------------------

/// The timing module is the time-math API surface, and it must contain no
/// floating-point type at all. `kernel.rs` is the single, deliberate exception
/// (the per-tile opacity handed to the compositor), so this guard also proves
/// that bridge still exists.
#[test]
fn the_time_math_source_contains_no_floating_point_types() {
    const TIMING: &str = include_str!("../src/switcher/timing.rs");
    const MOD: &str = include_str!("../src/switcher/mod.rs");
    const KERNEL: &str = include_str!("../src/switcher/kernel.rs");

    assert!(
        !TIMING.contains("f32") && !TIMING.contains("f64"),
        "switcher/timing.rs is the time-math surface and must stay float-free"
    );
    assert!(
        !MOD.contains("f32") && !MOD.contains("f64"),
        "switcher/mod.rs must not name floating-point types"
    );
    assert!(
        KERNEL.contains("f32"),
        "switcher/kernel.rs is the one documented float bridge"
    );
}

#[test]
fn the_kernel_bridge_converts_progress_for_the_compositor() {
    // Bit-exact comparisons keep this free of float equality lints.
    assert_eq!(kernel::opacity(Progress::ZERO).to_bits(), 0.0_f32.to_bits());
    assert_eq!(
        kernel::opacity(Progress::COMPLETE).to_bits(),
        1.0_f32.to_bits()
    );
    let six = FrameCount::try_from(6).unwrap();
    assert_eq!(
        kernel::opacity(Progress::new(3, six)).to_bits(),
        0.5_f32.to_bits()
    );
}

// ---------------------------------------------------------------------------
// Descriptors: internally tagged, never untagged, non_exhaustive by construction
// ---------------------------------------------------------------------------

/// Exhaustive match plus a wildcard: if `TransitionDescriptor` ever lost its
/// `#[non_exhaustive]` attribute, the wildcard arm would become unreachable and
/// `clippy --all-targets -- -D warnings` would fail. The guard is compile-time.
fn transition_label(descriptor: &TransitionDescriptor) -> &'static str {
    match descriptor {
        TransitionDescriptor::Cut => "cut",
        TransitionDescriptor::Mix { .. } => "mix",
        TransitionDescriptor::Dip { .. } => "dip",
        TransitionDescriptor::Wipe { .. } => "wipe",
        TransitionDescriptor::DvePush { .. } => "dve_push",
        TransitionDescriptor::DveSqueeze { .. } => "dve_squeeze",
        TransitionDescriptor::Stinger { .. } => "stinger",
        _ => "future",
    }
}

/// Compile-time `#[non_exhaustive]` guard for [`KeyType`] (see above).
fn key_type_label(key_type: &KeyType) -> &'static str {
    match key_type {
        KeyType::Luma { .. } => "luma",
        KeyType::Linear => "linear",
        KeyType::DvePip { .. } => "dve_pip",
        _ => "future",
    }
}

/// Compile-time `#[non_exhaustive]` guard for [`TapPoint`] (see above).
fn tap_label(point: TapPoint) -> &'static str {
    match point {
        TapPoint::Program => "program",
        TapPoint::Dirty => "dirty",
        TapPoint::Clean => "clean",
        TapPoint::Preview => "preview",
        TapPoint::Me { .. } => "me",
        TapPoint::Aux { .. } => "aux",
        _ => "future",
    }
}

/// Compile-time `#[non_exhaustive]` guard for [`KeyerMode`] (see above).
fn keyer_mode_label(mode: KeyerMode) -> &'static str {
    match mode {
        KeyerMode::Cut => "cut",
        KeyerMode::Auto => "auto",
        KeyerMode::Tie => "tie",
        _ => "future",
    }
}

/// Compile-time `#[non_exhaustive]` guard for [`WipePattern`] (see above).
fn wipe_label(pattern: WipePattern) -> &'static str {
    match pattern {
        WipePattern::LinearEdge => "linear_edge",
        WipePattern::Box => "box",
        WipePattern::Circle => "circle",
        _ => "future",
    }
}

/// Compile-time `#[non_exhaustive]` guard for [`DveDirection`] (see above).
fn dve_label(direction: DveDirection) -> &'static str {
    match direction {
        DveDirection::Left => "left",
        DveDirection::Right => "right",
        DveDirection::Up => "up",
        DveDirection::Down => "down",
        _ => "future",
    }
}

#[test]
fn descriptor_enums_are_non_exhaustive() {
    assert_eq!(transition_label(&TransitionDescriptor::Cut), "cut");
    assert_eq!(key_type_label(&KeyType::Linear), "linear");
    assert_eq!(tap_label(TapPoint::Program), "program");
    assert_eq!(keyer_mode_label(KeyerMode::Cut), "cut");
    assert_eq!(wipe_label(WipePattern::Box), "box");
    assert_eq!(dve_label(DveDirection::Left), "left");
}

#[test]
fn transition_descriptors_round_trip_with_an_internal_tag() {
    let cases: [Value; 7] = [
        json!({"kind": "cut"}),
        json!({"kind": "mix", "rate": 30}),
        json!({
            "kind": "dip",
            "rate": 20,
            "dip_source": "bars",
            "switch_point": {"num": 1, "den": 2}
        }),
        json!({
            "kind": "wipe",
            "pattern": "linear_edge",
            "rate": 30,
            "softness": 0.0,
            "border_width": 0.0,
            "position_xy": [0.5, 0.5],
            "reverse": false,
            "flip_flop": false
        }),
        json!({"kind": "dve_push", "direction": "left", "rate": 15}),
        json!({"kind": "dve_squeeze", "direction": "up", "rate": 15}),
        json!({
            "kind": "stinger",
            "player": "stinger-a",
            "pre_roll": 5,
            "clip_duration": 40,
            "trigger_point": 10,
            "mix_rate": 20
        }),
    ];

    for case in cases {
        // Internal tagging: the tag and the payload fields share one level. An
        // adjacently tagged shape (a nested `content` object) would not have
        // deserialised; an untagged one could not carry the tag at all.
        let parsed: TransitionDescriptor = serde_json::from_value(case.clone()).unwrap();
        assert_eq!(serde_json::to_value(&parsed).unwrap(), case);
        assert_eq!(
            case["kind"],
            Value::String(transition_label(&parsed).to_owned())
        );
    }
}

#[test]
fn a_missing_tag_is_rejected() {
    assert!(serde_json::from_value::<TransitionDescriptor>(json!({"rate": 30})).is_err());
    assert!(serde_json::from_value::<TapPoint>(json!({"index": 0})).is_err());
    assert!(serde_json::from_value::<KeyType>(json!({"clip": 0.0})).is_err());
}

#[test]
fn dip_switch_point_defaults_to_one_half() {
    let parsed: TransitionDescriptor =
        serde_json::from_value(json!({"kind": "dip", "rate": 20, "dip_source": "bars"})).unwrap();
    if let TransitionDescriptor::Dip { switch_point, .. } = parsed {
        assert_eq!(switch_point, Rational::new(1, 2));
    } else {
        panic!("expected a dip descriptor");
    }
}

#[test]
fn luma_key_defaults_are_neutral() {
    let key: KeyType = serde_json::from_value(json!({"kind": "luma"})).unwrap();
    if let KeyType::Luma { clip, gain, invert } = key {
        assert_eq!(clip.to_bits(), 0.0_f32.to_bits());
        assert_eq!(gain.to_bits(), 1.0_f32.to_bits());
        assert!(!invert);
    } else {
        panic!("expected a luma key");
    }
}

#[test]
fn dve_pip_key_carries_an_integer_pixel_rect() {
    let doc = json!({"kind": "dve_pip", "rect": {"x": 10, "y": -5, "width": 640, "height": 360}});
    let key: KeyType = serde_json::from_value(doc.clone()).unwrap();
    assert_eq!(serde_json::to_value(&key).unwrap(), doc);
    if let KeyType::DvePip { rect } = key {
        assert_eq!(rect, PixelRect::new(10, -5, 640, 360));
    } else {
        panic!("expected a dve_pip key");
    }
}

#[test]
fn key_source_omits_the_optional_key_and_round_trips() {
    let doc = json!({"fill": "cam1", "premultiplied": false});
    let source: KeySource = serde_json::from_value(doc.clone()).unwrap();
    assert_eq!(source.fill, SourceRef::new("cam1"));
    assert_eq!(source.fill.as_str(), "cam1");
    assert!(source.key.is_none());
    assert!(!source.premultiplied);
    assert_eq!(serde_json::to_value(&source).unwrap(), doc);

    let paired: KeySource =
        serde_json::from_value(json!({"fill": "cam1", "key": "cam1-key", "premultiplied": true}))
            .unwrap();
    assert_eq!(paired.key, Some(SourceRef::new("cam1-key")));
    assert!(paired.premultiplied);
}

#[test]
fn keyer_descriptor_flattens_its_key_type() {
    let doc = json!({
        "z": 2,
        "source": {"fill": "cam1", "premultiplied": false},
        "kind": "luma",
        "clip": 0.25,
        "gain": 1.0,
        "invert": true,
        "garbage_matte": {"x": 0, "y": 0, "width": 1920, "height": 1080},
        "on_air": true,
        "transition": {"mode": "auto", "rate": 30}
    });
    let parsed: KeyerDescriptor = serde_json::from_value(doc.clone()).unwrap();
    assert_eq!(serde_json::to_value(&parsed).unwrap(), doc);
    assert!(parsed.on_air);
    assert_eq!(parsed.z, 2);
    assert_eq!(parsed.garbage_matte, Some(PixelRect::new(0, 0, 1920, 1080)));
    assert_eq!(parsed.transition.mode, KeyerMode::Auto);
    assert_eq!(
        parsed.transition.rate,
        Some(FrameCount::try_from(30).unwrap())
    );
}

#[test]
fn keyer_transition_names_its_mode() {
    assert_eq!(KeyerTransition::default().mode, KeyerMode::Cut);
    assert_eq!(KeyerTransition::default().rate, None);
    let auto = KeyerTransition::auto(FrameCount::try_from(25).unwrap());
    assert_eq!(auto.mode, KeyerMode::Auto);
    assert_eq!(auto.rate, Some(FrameCount::try_from(25).unwrap()));
    assert_eq!(KeyerTransition::tie().mode, KeyerMode::Tie);
    assert_eq!(KeyerTransition::cut().mode, KeyerMode::Cut);
}

#[test]
fn armed_set_defaults_to_nothing_armed() {
    let set: ArmedSet = serde_json::from_value(json!({})).unwrap();
    assert!(set.is_empty());

    let doc = json!({"background": true, "keys": [1, 2]});
    let set: ArmedSet = serde_json::from_value(doc.clone()).unwrap();
    assert_eq!(serde_json::to_value(&set).unwrap(), doc);
    assert!(!set.is_empty());
    assert!(ArmedSet::default().with_background().background);
    assert_eq!(ArmedSet::default().with_key(1).keys, vec![1]);
}

#[test]
fn tap_points_are_internally_tagged() {
    let cases = [
        (json!({"tap": "program"}), TapPoint::Program),
        (json!({"tap": "dirty"}), TapPoint::Dirty),
        (json!({"tap": "clean"}), TapPoint::Clean),
        (json!({"tap": "preview"}), TapPoint::Preview),
        (json!({"tap": "me", "index": 0}), TapPoint::Me { index: 0 }),
        (
            json!({"tap": "aux", "index": 3}),
            TapPoint::Aux { index: 3 },
        ),
    ];
    for (doc, expected) in cases {
        let parsed: TapPoint = serde_json::from_value(doc.clone()).unwrap();
        assert_eq!(parsed, expected);
        assert_eq!(serde_json::to_value(parsed).unwrap(), doc);
    }
}
