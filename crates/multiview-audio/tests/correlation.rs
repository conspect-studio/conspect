//! Phase-correlation, goniometer and surround-downmix tests with synthetic
//! stereo/surround signals of known correlation.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
// reason: synthetic-signal generation needs index<->float casts that are exact
// for the small ranges used here; test-only.
#![allow(
    clippy::as_conversions,
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::float_cmp,
    clippy::many_single_char_names
)]

use std::f64::consts::PI;

use multiview_audio::correlation::{CorrelationMeter, GonioPoint, SurroundDownmix};

const FS: u32 = 48_000;

fn sine(freq: f64, amp: f64, phase: f64, n: usize) -> Vec<f64> {
    let w = 2.0 * PI * freq / f64::from(FS);
    (0..n).map(|i| amp * (w * i as f64 + phase).sin()).collect()
}

#[test]
fn mono_identical_channels_correlate_plus_one() {
    let mut m = CorrelationMeter::new();
    let s = sine(1_000.0, 0.5, 0.0, FS as usize / 4);
    for &x in &s {
        m.push(x, x); // L == R
    }
    approx::assert_abs_diff_eq!(m.correlation(), 1.0, epsilon = 1e-3);
}

#[test]
fn antiphase_channels_correlate_minus_one() {
    let mut m = CorrelationMeter::new();
    let s = sine(1_000.0, 0.5, 0.0, FS as usize / 4);
    for &x in &s {
        m.push(x, -x); // L == -R (mono collapse risk)
    }
    approx::assert_abs_diff_eq!(m.correlation(), -1.0, epsilon = 1e-3);
}

#[test]
fn decorrelated_channels_correlate_near_zero() {
    let mut m = CorrelationMeter::new();
    // Two different frequencies are (over a long window) uncorrelated.
    let l = sine(1_000.0, 0.5, 0.0, FS as usize);
    let r = sine(1_700.0, 0.5, 0.0, FS as usize);
    for i in 0..l.len() {
        m.push(l[i], r[i]);
    }
    assert!(
        m.correlation().abs() < 0.1,
        "uncorrelated tones should be near 0, got {}",
        m.correlation()
    );
}

#[test]
fn silence_correlation_is_defined_and_zero() {
    let mut m = CorrelationMeter::new();
    for _ in 0..FS {
        m.push(0.0, 0.0);
    }
    // No energy => correlation is reported as 0 (not NaN).
    assert!(m.correlation().is_finite());
    approx::assert_abs_diff_eq!(m.correlation(), 0.0, epsilon = 1e-9);
}

#[test]
fn goniometer_maps_mono_to_vertical_axis() {
    // In the standard 45deg-rotated Lissajous, an in-phase (mono) signal lies on
    // the vertical (M) axis: x ~ 0, y != 0.
    let pt = GonioPoint::from_lr(0.7, 0.7);
    approx::assert_abs_diff_eq!(pt.x, 0.0, epsilon = 1e-9);
    assert!(pt.y.abs() > 0.1);
}

#[test]
fn goniometer_maps_antiphase_to_horizontal_axis() {
    // Antiphase (S-only) lies on the horizontal (S) axis: y ~ 0, x != 0.
    let pt = GonioPoint::from_lr(0.7, -0.7);
    approx::assert_abs_diff_eq!(pt.y, 0.0, epsilon = 1e-9);
    assert!(pt.x.abs() > 0.1);
}

#[test]
fn lo_ro_downmix_follows_bs775() {
    // ITU-R BS.775 Lo/Ro: Lo = L + -3dB*C + -3dB*Ls ; Ro = R + -3dB*C + -3dB*Rs.
    // Feed only C => both Lo and Ro get -3 dB of C (~0.7071).
    let dm = SurroundDownmix::default();
    // L,R,C,LFE,Ls,Rs
    let (lo, ro) = dm.lo_ro(&[0.0, 0.0, 1.0, 0.5, 0.0, 0.0]);
    let g = 10f64.powf(-3.0 / 20.0);
    approx::assert_abs_diff_eq!(lo, g, epsilon = 1e-3);
    approx::assert_abs_diff_eq!(ro, g, epsilon = 1e-3);
    // LFE is excluded from the stereo downmix.
}

#[test]
fn lt_rt_downmix_is_phase_matrixed() {
    // ITU-R BS.775 Lt/Rt (matrix surround): surrounds are summed in antiphase
    // (Lt gets -Ls-Rs term region) so a pure mono-surround feed largely cancels
    // between Lt and Rt difference, encoding it for matrix decode.
    let dm = SurroundDownmix::default();
    // Feed only Ls.
    let (lt, rt) = dm.lt_rt(&[0.0, 0.0, 0.0, 0.0, 1.0, 0.0]);
    // Lt and Rt carry the surround with opposite sign (matrix encode).
    assert!(
        (lt - rt).abs() > 0.5,
        "Lt/Rt must phase-matrix the surround, got lt={lt} rt={rt}"
    );
}

#[test]
fn downmix_left_only_passes_to_lo_left() {
    let dm = SurroundDownmix::default();
    let (lo, ro) = dm.lo_ro(&[1.0, 0.0, 0.0, 0.0, 0.0, 0.0]);
    approx::assert_abs_diff_eq!(lo, 1.0, epsilon = 1e-9);
    approx::assert_abs_diff_eq!(ro, 0.0, epsilon = 1e-9);
}
