//! Adaptive (ratio-driven) resampler tests (DEV-B4 reuse target).
//!
//! The HDMI display-audio servo (`multiview-output::display::audio`) needs a
//! resampler whose ratio it can *vary* per servo tick within a clamped ±ppm
//! band — the mpv/Kodi "display-resample" technique. This is the **audio-crate
//! half** the servo reuses: the resampler lives here (the home of all
//! resampling, ADR-R005), operates on the canonical [`AudioBlock`], and is pure
//! Rust + hardware-free. These tests pin its contract.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::as_conversions,
    clippy::cast_precision_loss,
    clippy::cast_possible_wrap,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]

use multiview_audio::adaptive::{AdaptiveResampler, RatioPpm};
use multiview_audio::{AudioBlock, AudioFormat, ChannelLayout};

const FS: u32 = 48_000;

fn stereo_ramp(frames: usize) -> AudioBlock {
    let mut samples = Vec::with_capacity(frames * 2);
    for i in 0..frames {
        let v = (i as f32) / (frames.max(1) as f32);
        samples.push(v);
        samples.push(-v);
    }
    AudioBlock::from_interleaved(AudioFormat::new(FS, ChannelLayout::Stereo), samples).unwrap()
}

#[test]
fn unity_ratio_passes_frame_count_through() {
    // A 0-ppm correction is a transparent identity in frame count: N frames in,
    // N frames out (to within the one-frame fractional-phase carry).
    let mut rs = AdaptiveResampler::new(AudioFormat::new(FS, ChannelLayout::Stereo));
    rs.set_ratio(RatioPpm::ZERO);
    let out = rs.process(&stereo_ramp(1000));
    let n = out.frame_count() as i64;
    assert!(
        (n - 1000).abs() <= 1,
        "unity ratio should preserve frame count (+/-1 phase carry), got {n}"
    );
    assert_eq!(out.format().channel_count(), 2);
}

#[test]
fn positive_ppm_emits_more_frames_negative_emits_fewer() {
    // The servo speeds audio up (more output frames per input) to drain a
    // too-full FIFO, slows it down (fewer) to fill a too-empty one. A large
    // (test-only) ppm makes the sign visible over one block.
    let mut faster = AdaptiveResampler::new(AudioFormat::new(FS, ChannelLayout::Stereo));
    faster.set_ratio(RatioPpm::from_ppm(50_000.0)); // +5% — clamped at the band
    let up = faster.process(&stereo_ramp(2000)).frame_count() as i64;

    let mut slower = AdaptiveResampler::new(AudioFormat::new(FS, ChannelLayout::Stereo));
    slower.set_ratio(RatioPpm::from_ppm(-50_000.0));
    let down = slower.process(&stereo_ramp(2000)).frame_count() as i64;

    assert!(up > 2000, "positive ppm must yield MORE frames, got {up}");
    assert!(
        down < 2000,
        "negative ppm must yield FEWER frames, got {down}"
    );
}

#[test]
fn ratio_is_clamped_to_the_band() {
    // Whatever the servo asks, the applied ratio never leaves the clamped band
    // (the audible-artefact ceiling). An absurd request saturates, never
    // exceeds.
    let clamped = RatioPpm::from_ppm(10_000_000.0);
    assert!(
        clamped.ppm() <= RatioPpm::MAX_PPM + 1e-6,
        "ratio must clamp to +/- MAX_PPM, got {}",
        clamped.ppm()
    );
    let clamped_neg = RatioPpm::from_ppm(-10_000_000.0);
    assert!(clamped_neg.ppm() >= -RatioPpm::MAX_PPM - 1e-6);
}

#[test]
fn output_format_matches_input_and_is_finite() {
    let mut rs = AdaptiveResampler::new(AudioFormat::new(FS, ChannelLayout::Stereo));
    rs.set_ratio(RatioPpm::from_ppm(120.0));
    let out = rs.process(&stereo_ramp(4096));
    assert_eq!(out.format().sample_rate(), FS);
    assert_eq!(out.format().channel_count(), 2);
    assert!(
        out.interleaved().iter().all(|s| s.is_finite()),
        "resampled samples must all be finite"
    );
}

#[test]
fn long_run_tracks_the_ratio_without_unbounded_phase_drift() {
    // Over many blocks at a steady small ppm the cumulative output-frame total
    // tracks the ratio-implied total to within a single frame of fractional
    // phase — proving the resampler carries phase (no per-block rounding
    // accumulation that would desync audio over a long show).
    let mut rs = AdaptiveResampler::new(AudioFormat::new(FS, ChannelLayout::Stereo));
    let ppm = 200.0_f64;
    rs.set_ratio(RatioPpm::from_ppm(ppm));
    let blocks = 500usize;
    let per = 1024usize;
    let mut produced = 0usize;
    for _ in 0..blocks {
        produced += rs.process(&stereo_ramp(per)).frame_count();
    }
    let consumed = (blocks * per) as f64;
    let ideal = consumed * (1.0 + ppm / 1_000_000.0);
    let err = (produced as f64 - ideal).abs();
    assert!(
        err <= 2.0,
        "cumulative output should track ratio within ~1 frame phase; \
         produced {produced}, ideal {ideal:.2}, err {err:.3}"
    );
}
