//! Tests for the PTP / ST 2059-2 servo math: anchoring, step-vs-slew, the
//! delay-outlier guard, and convergence of noisy offset samples toward the true
//! offset. Pure-Rust default build (no NIC).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use multiview_engine::ptp::{PtpSample, PtpServo, ServoConfig};
use proptest::prelude::*;

#[test]
fn first_sample_anchors_exactly() {
    let mut servo = PtpServo::new(ServoConfig::new_default());
    assert!(!servo.is_locked());
    assert!(servo.update(PtpSample::new(12_345, 5_000)));
    assert!(servo.is_locked());
    // The first sample is applied exactly (no smoothing yet).
    assert_eq!(servo.offset_ns(), 12_345);
    assert_eq!(servo.frequency_ppb(), 0);
    assert_eq!(servo.accepted(), 1);
}

#[test]
fn small_errors_are_slewed_not_stepped() {
    let cfg = ServoConfig {
        alpha_recip: 4,
        step_threshold_ns: 1_000_000,
        delay_outlier_pct: 0, // disable the guard for this test
    };
    let mut servo = PtpServo::new(cfg);
    servo.update(PtpSample::new(0, 1000));
    // A 400 ns error (below the 1 ms step threshold) moves the estimate by
    // error/alpha = 400/4 = 100 ns.
    servo.update(PtpSample::new(400, 1000));
    assert_eq!(servo.offset_ns(), 100);
}

#[test]
fn large_discontinuity_is_stepped() {
    let cfg = ServoConfig {
        alpha_recip: 8,
        step_threshold_ns: 1_000_000,
        delay_outlier_pct: 0,
    };
    let mut servo = PtpServo::new(cfg);
    servo.update(PtpSample::new(0, 1000));
    // A 5 ms jump exceeds the 1 ms step threshold: the estimate snaps to it.
    servo.update(PtpSample::new(5_000_000, 1000));
    assert_eq!(servo.offset_ns(), 5_000_000);
    // A step resets the frequency trend.
    assert_eq!(servo.frequency_ppb(), 0);
}

#[test]
fn delay_outlier_is_rejected() {
    let cfg = ServoConfig {
        alpha_recip: 8,
        step_threshold_ns: 1_000_000,
        delay_outlier_pct: 200, // reject delays > 2x running average
    };
    let mut servo = PtpServo::new(cfg);
    servo.update(PtpSample::new(0, 10_000)); // anchor, avg delay 10us
    let before = servo.offset_ns();
    // A sample whose delay is 10x the average is a path-asymmetry spike: rejected.
    let accepted = servo.update(PtpSample::new(500_000, 100_000));
    assert!(!accepted);
    assert_eq!(
        servo.offset_ns(),
        before,
        "rejected sample must not move estimate"
    );
}

#[test]
fn master_from_local_subtracts_offset() {
    let mut servo = PtpServo::new(ServoConfig::new_default());
    servo.update(PtpSample::new(3_000, 1000)); // local is 3us ahead of master
                                               // master = local - offset.
    assert_eq!(servo.master_from_local(1_000_000), 1_000_000 - 3_000);
}

#[test]
fn delay_clamped_non_negative() {
    let s = PtpSample::new(0, -50);
    assert_eq!(s.delay_ns, 0);
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// Feeding the servo many noisy samples centred on a constant true offset
    /// makes the smoothed estimate converge to within a small band of that true
    /// offset — the whole point of a servo.
    #[test]
    fn converges_to_true_offset(
        true_offset in -2_000_000i64..2_000_000,
        // Bounded zero-mean-ish jitter samples added to the true offset.
        noise in proptest::collection::vec(-5_000i64..5_000, 200..400),
    ) {
        let cfg = ServoConfig {
            alpha_recip: 16,
            step_threshold_ns: 10_000_000, // high so jitter never triggers a step
            delay_outlier_pct: 0,
        };
        let mut servo = PtpServo::new(cfg);
        for &n in &noise {
            servo.update(PtpSample::new(true_offset.saturating_add(n), 1000));
        }
        // After hundreds of samples, the smoothed offset is within the noise band
        // of the true offset (well under the +/-5us jitter amplitude).
        let err = (servo.offset_ns() - true_offset).abs();
        prop_assert!(err <= 5_000, "estimate {} not within 5us of true {}", servo.offset_ns(), true_offset);
    }

    /// The servo never panics and the estimate stays finite (no overflow) for any
    /// sequence of arbitrary samples.
    #[test]
    fn never_panics_on_arbitrary_samples(
        samples in proptest::collection::vec(
            (any::<i64>(), any::<i64>()),
            0..200,
        ),
    ) {
        let mut servo = PtpServo::new(ServoConfig::new_default());
        for (off, del) in samples {
            servo.update(PtpSample::new(off, del));
        }
        // Reading the estimate after arbitrary input must not panic.
        let _ = servo.offset_ns();
        let _ = servo.frequency_ppb();
        let _ = servo.master_from_local(0);
    }
}
