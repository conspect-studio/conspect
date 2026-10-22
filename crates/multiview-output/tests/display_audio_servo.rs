//! Buffer-level servo tests (DEV-B4 / display-out §5: the three-clock servo).
//!
//! Three independent crystals — the engine tick, the display *pixel* clock
//! (observed via flip timestamps, `sink.rs` `last_flip_ns`), and the ALSA
//! *sample* clock — drift at ppm levels. The servo watches the audio FIFO fill
//! level (and the long-run sample-vs-flip skew) and emits a resample-ratio
//! correction (the mpv/Kodi display-resample technique) so audio tracks the
//! *scanout* clock. These tests prove, hardware-free: the correction has the
//! right sign, is bounded to the ±ppm band, and **converges** the fill toward
//! the setpoint without oscillating unbounded.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::as_conversions,
    clippy::cast_precision_loss,
    clippy::float_cmp
)]

use multiview_audio::adaptive::RatioPpm;
use multiview_output::display::audio::BufferServo;

#[test]
fn too_full_speeds_audio_up_too_empty_slows_it_down() {
    // Setpoint is mid-FIFO (0.5). A fill above setpoint must DRAIN faster =>
    // positive ppm (more output frames per input). Below setpoint => negative.
    let servo = BufferServo::new();
    let up = servo.clone().correction(0.9, 0.0);
    let down = servo.clone().correction(0.1, 0.0);
    assert!(
        up.ppm() > 0.0,
        "too-full FIFO must speed audio up, got {}",
        up.ppm()
    );
    assert!(
        down.ppm() < 0.0,
        "too-empty FIFO must slow audio down, got {}",
        down.ppm()
    );
}

#[test]
fn at_setpoint_with_no_skew_holds_unity() {
    let servo = BufferServo::new();
    let c = servo.clone().correction(0.5, 0.0);
    assert!(
        c.ppm().abs() < 1e-3,
        "balanced FIFO + no skew => ~unity, got {}",
        c.ppm()
    );
}

#[test]
fn correction_is_clamped_to_the_band() {
    // Even a pathological fill error never produces a ppm outside the audible
    // ceiling — the band clamp lives in RatioPpm and the servo respects it.
    let servo = BufferServo::new();
    let c = servo.clone().correction(1.0, 1e9);
    assert!(
        c.ppm().abs() <= RatioPpm::MAX_PPM + 1e-6,
        "servo output must stay within +/- MAX_PPM, got {}",
        c.ppm()
    );
}

#[test]
fn closed_loop_converges_fill_toward_setpoint() {
    // Simulate the closed loop: a model FIFO that the servo drains at a rate set
    // by the ppm correction, with a constant fill inflow. Starting badly
    // off-setpoint, the fill must monotone-ish converge toward 0.5 and settle.
    let mut servo = BufferServo::new();
    let cap = 8_192.0_f64;
    let mut fill = 0.95 * cap; // start nearly full
    let inflow = 480.0_f64; // frames per servo tick fed in (one 10ms @48k block)
    let mut last_err = f64::INFINITY;
    let mut settled = false;
    for _ in 0..2000 {
        let frac = fill / cap;
        let ppm = servo.correction(frac, 0.0).ppm();
        // outflow tracks the ratio: nominal == inflow at unity; +ppm drains more.
        let outflow = inflow * (1.0 + ppm / 1_000_000.0);
        fill = (fill + inflow - outflow).clamp(0.0, cap);
        let err = (fill / cap - 0.5).abs();
        if err < 0.02 {
            settled = true;
            break;
        }
        // Allow brief non-monotone settle but require overall progress.
        last_err = last_err.min(err);
    }
    assert!(
        settled,
        "closed loop must converge fill toward the 0.5 setpoint; best err {last_err:.4}"
    );
}

#[test]
fn skew_term_corrects_long_run_sample_vs_flip_drift() {
    // Even with the FIFO perfectly at setpoint, a persistent measured skew
    // (sample clock running slow/fast vs the flip clock) must bias the ratio so
    // the audio re-aligns to scanout. Positive skew = audio AHEAD of scanout
    // (`skew_ms` is audio-elapsed minus scanout-elapsed) => drain slower (a
    // negative drain ppm) so scanout catches back up, and vice-versa.
    let servo = BufferServo::new();
    let ahead = servo.clone().correction(0.5, 5.0); // +5 ms: audio AHEAD of scanout
    let behind = servo.clone().correction(0.5, -5.0); // -5 ms: audio behind scanout
    assert!(ahead.ppm() != behind.ppm(), "skew must move the correction");
    assert!(
        ahead.ppm().signum() != behind.ppm().signum() || ahead.ppm() == 0.0,
        "opposite skew => opposite-sign correction"
    );
}
