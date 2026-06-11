//! Content-aware probe tests (ADR-MV001): black/freeze/format over a synthetic
//! NV12 luma view, plus an end-to-end probe → X.733 state-machine chain showing a
//! black fault raises after its dwell and clears after recovery, with no sleeps.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use multiview_core::alarm::{AlarmId, AlarmKind, AlarmScope, PerceivedSeverity};
use multiview_core::color::{ColorInfo, ColorPrimaries, ColorRange};
use multiview_core::frame::FrameMeta;
use multiview_core::pixel::PixelFormat;
use multiview_core::time::MediaTime;
use multiview_engine::alarm::state::{AlarmHysteresis, AlarmStateMachine, AlarmTransition};
use multiview_engine::probe::{
    BlackConfig, BlackProbe, DetectionZone, ExpectedFormat, FormatAxis, FormatProbe, FreezeConfig,
    FreezeProbe, LumaView,
};
use proptest::prelude::*;

const W: u32 = 32;
const H: u32 = 18;

/// `u32 -> usize` without an `as` cast (banned even in tests by
/// `clippy::as_conversions`).
fn px(n: u32) -> usize {
    usize::try_from(n).expect("frame index fits usize")
}

/// A tightly-packed `W x H` luma plane filled with a constant value.
fn flat(value: u8) -> Vec<u8> {
    vec![value; px(W * H)]
}

fn ms(n: i64) -> MediaTime {
    MediaTime::from_nanos(n.saturating_mul(1_000_000))
}

#[test]
fn luma_view_rejects_bad_geometry() {
    let buf = flat(50);
    assert!(LumaView::new(&buf, 0, H, W).is_err()); // zero width
    assert!(LumaView::new(&buf, W, H, W - 1).is_err()); // stride < width
    assert!(LumaView::new(&buf, W, H + 1, W).is_err()); // buffer too small
    assert!(LumaView::packed(&buf, W, H).is_ok());
}

#[test]
fn mean_luma_is_exact_for_flat_field() {
    let buf = flat(120);
    let view = LumaView::packed(&buf, W, H).unwrap();
    assert!((view.mean_luma(DetectionZone::FULL) - 120.0).abs() < 1e-9);
}

#[test]
fn black_probe_fires_below_threshold_only() {
    let probe = BlackProbe::new(BlackConfig::with_threshold(16.0));

    let dark = flat(8);
    let dark_view = LumaView::packed(&dark, W, H).unwrap();
    let obs = probe.detect(&dark_view);
    assert_eq!(obs.kind, AlarmKind::Black);
    assert!(obs.condition_present);
    assert!((obs.measured - 8.0).abs() < 1e-9);

    let bright = flat(200);
    let bright_view = LumaView::packed(&bright, W, H).unwrap();
    assert!(!probe.detect(&bright_view).condition_present);

    // Exactly at threshold counts as black (<=).
    let edge = flat(16);
    let edge_view = LumaView::packed(&edge, W, H).unwrap();
    assert!(probe.detect(&edge_view).condition_present);
}

#[test]
fn black_probe_detection_zone_ignores_outside_pixels() {
    // Left half black, right half bright. A zone over only the right half is NOT
    // black; a zone over only the left half IS black.
    let mut buf = vec![0_u8; px(W * H)];
    for y in 0..H {
        for x in 0..W {
            let v = if x < W / 2 { 0 } else { 235 };
            buf[px(y * W + x)] = v;
        }
    }
    let view = LumaView::packed(&buf, W, H).unwrap();
    let probe = BlackProbe::new(BlackConfig::with_threshold(16.0));

    let right = DetectionZone::new(0.5, 0.0, 0.5, 1.0).unwrap();
    assert!(!probe.config().zone.eq(&right));
    let right_probe = BlackProbe::new(BlackConfig::with_threshold(16.0).with_zone(right));
    assert!(!right_probe.detect(&view).condition_present);

    let left = DetectionZone::new(0.0, 0.0, 0.5, 1.0).unwrap();
    let left_probe = BlackProbe::new(BlackConfig::with_threshold(16.0).with_zone(left));
    assert!(left_probe.detect(&view).condition_present);
}

#[test]
fn freeze_probe_detects_identical_frames_and_clears_on_change() {
    let probe = FreezeProbe::new(FreezeConfig::default());
    let a = flat(100);
    let b = flat(100);
    let va = LumaView::packed(&a, W, H).unwrap();
    let vb = LumaView::packed(&b, W, H).unwrap();
    let obs = probe.detect(&va, &vb);
    assert_eq!(obs.kind, AlarmKind::Freeze);
    assert!(obs.condition_present, "identical frames must read frozen");
    assert!(obs.measured.abs() < 1e-9);

    // Change every pixel substantially: not frozen.
    let c = flat(200);
    let vc = LumaView::packed(&c, W, H).unwrap();
    let obs2 = probe.detect(&vc, &vb);
    assert!(!obs2.condition_present);
    assert!((obs2.measured - 1.0).abs() < 1e-9);
}

#[test]
fn freeze_probe_tolerates_small_noise() {
    // One pixel changes by 1 level; tolerance 2 means it is still "unchanged".
    let prev = flat(100);
    let mut cur = flat(100);
    cur[0] = 101;
    let vprev = LumaView::packed(&prev, W, H).unwrap();
    let vcur = LumaView::packed(&cur, W, H).unwrap();
    let probe = FreezeProbe::new(FreezeConfig::default()); // tolerance 2, thr 0.1%
    assert!(probe.detect(&vcur, &vprev).condition_present);
}

#[test]
fn freeze_probe_geometry_change_is_not_frozen() {
    let prev = flat(100);
    let cur = vec![100_u8; px(W * (H + 2))];
    let vprev = LumaView::packed(&prev, W, H).unwrap();
    let vcur = LumaView::packed(&cur, W, H + 2).unwrap();
    let probe = FreezeProbe::new(FreezeConfig::default());
    let obs = probe.detect(&vcur, &vprev);
    assert!(!obs.condition_present, "a resize must never read as freeze");
    assert!((obs.measured - 1.0).abs() < 1e-9);
}

#[test]
fn format_probe_flags_only_constrained_changed_axes() {
    let expected = ExpectedFormat::with_size(1920, 1080).with_color(ColorInfo {
        primaries: ColorPrimaries::Bt709,
        transfer: multiview_core::color::TransferCharacteristic::Bt709,
        matrix: multiview_core::color::MatrixCoefficients::Bt709,
        range: ColorRange::Limited,
    });
    let probe = FormatProbe::new(expected);

    // Matching frame: clean.
    let good = FrameMeta {
        pts: MediaTime::ZERO,
        width: 1920,
        height: 1080,
        format: PixelFormat::Nv12,
        color: ColorInfo {
            primaries: ColorPrimaries::Bt709,
            transfer: multiview_core::color::TransferCharacteristic::Bt709,
            matrix: multiview_core::color::MatrixCoefficients::Bt709,
            range: ColorRange::Limited,
        },
    };
    assert!(probe.compare(&good).is_clean());
    assert!(!probe.detect(&good).condition_present);

    // SD/PAL-ish frame with full range: width, height, primaries, range differ.
    let bad = FrameMeta {
        pts: MediaTime::ZERO,
        width: 720,
        height: 576,
        format: PixelFormat::Nv12,
        color: ColorInfo {
            primaries: ColorPrimaries::Bt601_625,
            transfer: multiview_core::color::TransferCharacteristic::Bt709,
            matrix: multiview_core::color::MatrixCoefficients::Bt709,
            range: ColorRange::Full,
        },
    };
    let mm = probe.compare(&bad);
    assert!(mm.any());
    assert!(mm.contains(FormatAxis::Width));
    assert!(mm.contains(FormatAxis::Height));
    assert!(mm.contains(FormatAxis::Primaries));
    assert!(mm.contains(FormatAxis::Range));
    assert!(!mm.contains(FormatAxis::Transfer));
    assert_eq!(mm.count(), 4);
    let obs = probe.detect(&bad);
    assert_eq!(obs.kind, AlarmKind::FormatMismatch);
    assert!(obs.condition_present);
    assert!((obs.measured - 4.0).abs() < 1e-9);
}

#[test]
fn format_probe_unspecified_axis_is_dont_care() {
    // Expect only a size; an unsignalled (Unspecified) source color is fine.
    let probe = FormatProbe::new(ExpectedFormat::with_size(1280, 720));
    let frame = FrameMeta {
        pts: MediaTime::ZERO,
        width: 1280,
        height: 720,
        format: PixelFormat::Nv12,
        color: ColorInfo::default(), // all axes Unspecified
    };
    assert!(probe.compare(&frame).is_clean());
}

// ---- End-to-end synthetic fault: a black fault drives the probe, whose
// observations drive the X.733 machine; the alarm raises after dwell-up and
// clears after dwell-down. No sleeps, fully deterministic. ----
#[test]
fn black_fault_raises_after_dwell_and_clears_after_recovery() {
    let probe = BlackProbe::new(BlackConfig::with_threshold(16.0));
    let mut machine = AlarmStateMachine::new(
        AlarmId::new("black-tile-0"),
        AlarmKind::Black,
        AlarmScope::Tile { index: 0 },
        PerceivedSeverity::Major,
        AlarmHysteresis::new(ms(100), ms(100)),
    );

    let dark = flat(4);
    let bright = flat(200);
    let dark_view = LumaView::packed(&dark, W, H).unwrap();
    let bright_view = LumaView::packed(&bright, W, H).unwrap();

    // t=0..=90 ms: black present, dwelling up, not yet raised.
    let mut raised = false;
    for t in [0_i64, 30, 60, 90] {
        let obs = probe.detect(&dark_view);
        if machine.observe(obs.condition_present, ms(t)) == AlarmTransition::Raised {
            raised = true;
        }
    }
    assert!(!raised, "must not raise before dwell-up");
    assert!(!machine.is_active());

    // t=120 ms: still black, dwell-up satisfied -> raised.
    let obs = probe.detect(&dark_view);
    assert_eq!(
        machine.observe(obs.condition_present, ms(120)),
        AlarmTransition::Raised
    );
    assert!(machine.is_active());

    // Recovery: bright frames; clears after dwell-down.
    let obs = probe.detect(&bright_view);
    assert_eq!(
        machine.observe(obs.condition_present, ms(200)),
        AlarmTransition::None
    );
    assert!(machine.is_active(), "still in clearing dwell");
    let obs = probe.detect(&bright_view);
    assert_eq!(
        machine.observe(obs.condition_present, ms(300)),
        AlarmTransition::Cleared
    );
    assert!(!machine.is_active());
}

// Property: a flat field of value v reads black iff v <= threshold.
proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    fn black_threshold_is_exact(value in 0_u8..=255, thr in 0_u8..=255) {
        let buf = flat(value);
        let view = LumaView::packed(&buf, W, H).unwrap();
        let probe = BlackProbe::new(BlackConfig::with_threshold(f64::from(thr)));
        let present = probe.detect(&view).condition_present;
        prop_assert_eq!(present, f64::from(value) <= f64::from(thr));
    }

    #[test]
    fn identical_frames_always_freeze(value in 0_u8..=255) {
        let a = flat(value);
        let b = flat(value);
        let va = LumaView::packed(&a, W, H).unwrap();
        let vb = LumaView::packed(&b, W, H).unwrap();
        let probe = FreezeProbe::new(FreezeConfig::default());
        prop_assert!(probe.detect(&va, &vb).condition_present);
    }
}
