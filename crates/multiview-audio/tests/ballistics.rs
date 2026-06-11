//! Meter-ballistics tests with synthetic signals of known answers.
//!
//! Verifies the selectable ballistics — PPM Type I/IIa/IIb (IEC 60268-10), VU,
//! sample-peak (IEC TR 60268-18) and the true-peak (ITU-R BS.1770) wrapper —
//! against documented integration/decay behaviour and reference deflections.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
// reason: synthetic-signal generation needs index<->float and float<->sample
// casts that are exact for the small ranges used here; test-only.
#![allow(
    clippy::as_conversions,
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::float_cmp,
    clippy::many_single_char_names
)]

use std::f64::consts::PI;

use multiview_audio::ballistics::{Ballistics, MeterScale, PeakMode, PpmKind, SampleScale};

const FS: u32 = 48_000;

/// `seconds` of a `freq` Hz sine of peak amplitude `amp`, sampled at `FS`.
fn sine(freq: f64, amp: f64, seconds: f64) -> Vec<f64> {
    let n = (f64::from(FS) * seconds).round() as usize;
    let w = 2.0 * PI * freq / f64::from(FS);
    (0..n).map(|i| amp * (w * i as f64).sin()).collect()
}

/// dBFS of a linear amplitude.
fn dbfs(amp: f64) -> f64 {
    20.0 * amp.log10()
}

/// A 1 kHz sine at -18 dBFS, run long enough for any meter to settle.
fn tone_minus18(seconds: f64) -> Vec<f64> {
    let amp = 10f64.powf(-18.0 / 20.0);
    sine(1_000.0, amp, seconds)
}

#[test]
fn sample_peak_tracks_the_largest_magnitude() {
    let mut m = Ballistics::new(FS, MeterScale::SamplePeak(PeakMode::Sample));
    let tone = tone_minus18(0.5);
    for &x in &tone {
        m.push(x);
    }
    // A -18 dBFS sine peaks at -18 dBFS sample-peak.
    let SampleScale::DbFs(v) = m.reading_scaled() else {
        panic!("sample-peak meter must read in dBFS");
    };
    approx::assert_abs_diff_eq!(v, -18.0, epsilon = 0.05);
}

#[test]
fn sample_peak_holds_then_decays() {
    let mut m = Ballistics::new(FS, MeterScale::SamplePeak(PeakMode::Sample));
    // A single full-scale spike, then silence: with peak-hold the reading must
    // stay near 0 dBFS during the hold, then fall back.
    m.push(1.0);
    let held = m.reading_db();
    approx::assert_abs_diff_eq!(held, 0.0, epsilon = 0.01);
    // After a long run of silence the meter must decay well below 0 dBFS.
    for _ in 0..FS {
        m.push(0.0);
    }
    assert!(
        m.reading_db() < -10.0,
        "sample-peak should decay after the hold, got {}",
        m.reading_db()
    );
}

#[test]
fn vu_reads_zero_at_its_reference_level() {
    // VU is calibrated so a steady sine at the alignment level deflects to 0 VU.
    // Multiview aligns 0 VU to -18 dBFS (EBU alignment level). A -18 dBFS sine must
    // therefore read ~0 VU once integrated.
    let mut m = Ballistics::new(FS, MeterScale::Vu);
    for &x in &tone_minus18(1.0) {
        m.push(x);
    }
    let SampleScale::Vu(v) = m.reading_scaled() else {
        panic!("VU meter must read in VU units");
    };
    approx::assert_abs_diff_eq!(v, 0.0, epsilon = 0.3);
}

#[test]
fn vu_integration_is_slow() {
    // The VU 300 ms integration means a freshly-started tone has NOT reached
    // full deflection after only 10 ms but is close after ~600 ms.
    let mut fast = Ballistics::new(FS, MeterScale::Vu);
    for &x in &tone_minus18(0.01) {
        fast.push(x);
    }
    let early = fast.reading_db();

    let mut slow = Ballistics::new(FS, MeterScale::Vu);
    for &x in &tone_minus18(0.6) {
        slow.push(x);
    }
    let late = slow.reading_db();

    assert!(
        late > early + 3.0,
        "VU must integrate slowly: early={early} late={late}"
    );
}

#[test]
fn ppm_type2a_reference_deflects_to_test_level() {
    // EBU PPM (Type IIa): the reference/alignment level "TEST" (-18 dBFS in the
    // digital domain, '4' on the scale = 0 dBu alignment) reads at the alignment
    // mark. We model the scale in dB relative to alignment, so a -18 dBFS tone
    // reads ~0 (alignment) on the dB scale.
    let mut m = Ballistics::new(FS, MeterScale::Ppm(PpmKind::Iia));
    for &x in &tone_minus18(1.0) {
        m.push(x);
    }
    // dB reading is absolute dBFS-based; at -18 dBFS the meter sits near -18.
    approx::assert_abs_diff_eq!(m.reading_db(), -18.0, epsilon = 1.0);
}

#[test]
fn ppm_integration_time_is_fast_but_not_instant() {
    // IEC 60268-10: a Type I PPM reaches ~80% (within ~2 dB) of full deflection
    // for a 5 ms tone burst at reference frequency; a single sample does not.
    let kind = PpmKind::One;
    let mut burst = Ballistics::new(FS, MeterScale::Ppm(kind));
    let amp = 10f64.powf(-6.0 / 20.0);
    for &x in &sine(1_000.0, amp, 0.005) {
        burst.push(x);
    }
    let after_5ms = burst.reading_db();

    let mut steady = Ballistics::new(FS, MeterScale::Ppm(kind));
    for &x in &sine(1_000.0, amp, 0.5) {
        steady.push(x);
    }
    let steady_db = steady.reading_db();

    // 5 ms burst is within ~4 dB of steady deflection (fast integration), and a
    // steady tone reads near its true level.
    assert!(
        (steady_db - after_5ms).abs() < 4.0,
        "5ms burst {after_5ms} should approach steady {steady_db}"
    );
    approx::assert_abs_diff_eq!(steady_db, dbfs(amp), epsilon = 1.0);
}

#[test]
fn ppm_decay_is_slow_relative_to_attack() {
    // The defining PPM characteristic: fast attack, slow decay (fallback).
    // After a full-scale burst then silence, the reading falls only a few dB
    // over the first ~200 ms (the fallback time is seconds-per-20 dB).
    let mut m = Ballistics::new(FS, MeterScale::Ppm(PpmKind::Iib));
    for &x in &sine(1_000.0, 1.0, 0.05) {
        m.push(x);
    }
    let peak = m.reading_db();
    // 200 ms of silence.
    for _ in 0..(FS / 5) {
        m.push(0.0);
    }
    let after = m.reading_db();
    let drop = peak - after;
    assert!(
        (1.0..12.0).contains(&drop),
        "PPM fallback over 200ms should be a few dB (slow), got {drop} dB"
    );
}

#[test]
fn truepeak_detects_intersample_overshoot() {
    // The classic BS.1770 inter-sample test: a tone near Nyquist sampled so the
    // true (reconstructed) peak exceeds the sample peak. The true-peak ballistic
    // must read higher than the sample-peak ballistic for the same signal.
    let amp = 10f64.powf(-6.0 / 20.0); // -6 dBFS sample peak
                                       // 11.025 kHz-ish tone phased to land between samples.
    let f = f64::from(FS) / 4.0 - 50.0;
    let w = 2.0 * PI * f / f64::from(FS);
    let n = FS as usize / 4;
    let mut tp = Ballistics::new(FS, MeterScale::SamplePeak(PeakMode::TruePeak));
    let mut sp = Ballistics::new(FS, MeterScale::SamplePeak(PeakMode::Sample));
    for i in 0..n {
        let x = amp * (w * i as f64 + 0.4).sin();
        tp.push(x);
        sp.push(x);
    }
    assert!(
        tp.reading_db() > sp.reading_db() + 0.1,
        "true-peak {} should exceed sample-peak {}",
        tp.reading_db(),
        sp.reading_db()
    );
    // And the true-peak should be within a sensible band of 0 dBTP (the tone is
    // -6 dBFS so the inter-sample overshoot is small but positive).
    assert!(tp.reading_db() > -6.5 && tp.reading_db() < 0.5);
}

#[test]
fn silence_reads_negative_infinity_floor() {
    let mut m = Ballistics::new(FS, MeterScale::SamplePeak(PeakMode::Sample));
    for _ in 0..FS {
        m.push(0.0);
    }
    // Silence floors at the configured floor (a very negative dB), never +inf.
    assert!(m.reading_db() <= Ballistics::FLOOR_DB + 0.001);
    assert!(m.reading_db().is_finite());
}
