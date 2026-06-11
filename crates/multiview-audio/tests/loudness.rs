//! BS.1770 / EBU R128 loudness metering tests with synthetic signals of known
//! loudness. These verify the K-weighting filter, mean-square, channel
//! weighting, gating, and the M/S/I/LRA + true-peak math against documented
//! reference values.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
// reason: synthetic-signal generation needs index<->float and float<->sample
// casts that are exact for the small ranges used here, plus short DSP locals
// (n, w, v, s); test-only.
#![allow(
    clippy::as_conversions,
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::float_cmp,
    clippy::many_single_char_names
)]

use std::f64::consts::PI;

use multiview_audio::loudness::{
    k_weight_coeffs, LoudnessMeter, ABSOLUTE_GATE_LUFS, RELATIVE_GATE_OFFSET_LU,
};
use multiview_audio::{AudioFormat, ChannelLayout};

const FS: u32 = 48_000;

/// Generate `seconds` of a `freq` Hz sine of peak amplitude `amp`, sampled at
/// `FS`, replicated across every channel of `layout`. Returns interleaved f32.
fn sine_interleaved(layout: ChannelLayout, freq: f64, amp: f64, seconds: f64) -> Vec<f32> {
    let ch = layout.channel_count();
    let n = (f64::from(FS) * seconds).round() as usize;
    let mut out = Vec::with_capacity(n * ch);
    let w = 2.0 * PI * freq / f64::from(FS);
    for i in 0..n {
        let s = (amp * (w * i as f64).sin()) as f32;
        for _ in 0..ch {
            out.push(s);
        }
    }
    out
}

fn meter(layout: ChannelLayout) -> LoudnessMeter {
    LoudnessMeter::new(AudioFormat::new(FS, layout)).unwrap()
}

/// The K-weighting biquad coefficients computed for 48 kHz must match the
/// documented BS.1770-4 reference constants to high precision. This pins the
/// analog-prototype + bilinear-transform implementation.
#[test]
fn k_weight_coeffs_match_bs1770_reference_at_48k() {
    let (stage1, stage2) = k_weight_coeffs(48_000);

    // Stage 1: high-shelf "pre-filter" (head model).
    approx::assert_abs_diff_eq!(stage1.b0, 1.535_124_859_586_97, epsilon = 1e-6);
    approx::assert_abs_diff_eq!(stage1.b1, -2.691_696_189_406_38, epsilon = 1e-6);
    approx::assert_abs_diff_eq!(stage1.b2, 1.198_392_810_852_85, epsilon = 1e-6);
    approx::assert_abs_diff_eq!(stage1.a1, -1.690_659_293_182_41, epsilon = 1e-6);
    approx::assert_abs_diff_eq!(stage1.a2, 0.732_480_774_215_85, epsilon = 1e-6);

    // Stage 2: RLB high-pass.
    approx::assert_abs_diff_eq!(stage2.b0, 1.0, epsilon = 1e-6);
    approx::assert_abs_diff_eq!(stage2.b1, -2.0, epsilon = 1e-6);
    approx::assert_abs_diff_eq!(stage2.b2, 1.0, epsilon = 1e-6);
    approx::assert_abs_diff_eq!(stage2.a1, -1.990_047_454_833_98, epsilon = 1e-6);
    approx::assert_abs_diff_eq!(stage2.a2, 0.990_072_250_366_21, epsilon = 1e-6);
}

/// A full-scale (0 dBFS) 1 kHz sine on both channels of a stereo signal reads
/// approximately 0.0 LUFS. The BS.1770-4 K-weighting has ~+0.70 dB of gain at
/// 1 kHz (`|H_k(1 kHz)| ≈ 1.0836`), so the summed weighted mean-square is ~1.17
/// and `-0.691 + 10·log10(1.17) ≈ +0.0` LUFS. (Verified against the standard's
/// own reference coefficients; the common "≈ -0.69" intuition assumes a unity
/// 1 kHz gain that the real filter does not have.)
#[test]
fn stereo_full_scale_1khz_sine_reads_near_zero_lufs() {
    let mut m = meter(ChannelLayout::Stereo);
    let s = sine_interleaved(ChannelLayout::Stereo, 1000.0, 1.0, 3.0);
    m.push_interleaved(&s).unwrap();
    let i = m
        .integrated()
        .expect("integrated loudness over a loud 3 s signal");
    approx::assert_abs_diff_eq!(i, 0.0, epsilon = 0.3);
}

/// A mono full-scale 1 kHz sine reads ~3.01 LU below the stereo case (one
/// channel instead of two summed), i.e. about -3.00 LUFS.
#[test]
fn mono_full_scale_1khz_sine_reads_near_minus_300_lufs() {
    let mut m = meter(ChannelLayout::Mono);
    let s = sine_interleaved(ChannelLayout::Mono, 1000.0, 1.0, 3.0);
    m.push_interleaved(&s).unwrap();
    let i = m.integrated().expect("integrated loudness");
    approx::assert_abs_diff_eq!(i, -3.00, epsilon = 0.3);
}

/// The meter is linear/time-invariant: scaling the input by -20 dB shifts the
/// reported loudness by exactly -20 LU, independent of the absolute calibration
/// or the exact K-weight gain at 1 kHz.
#[test]
fn loudness_is_linear_in_dbfs() {
    let mut full = meter(ChannelLayout::Stereo);
    full.push_interleaved(&sine_interleaved(ChannelLayout::Stereo, 1000.0, 1.0, 3.0))
        .unwrap();
    let l_full = full.integrated().unwrap();

    let amp = 10f64.powf(-20.0 / 20.0); // -20 dBFS
    let mut quiet = meter(ChannelLayout::Stereo);
    quiet
        .push_interleaved(&sine_interleaved(ChannelLayout::Stereo, 1000.0, amp, 3.0))
        .unwrap();
    let l_quiet = quiet.integrated().unwrap();

    approx::assert_abs_diff_eq!(l_full - l_quiet, 20.0, epsilon = 0.05);
}

/// A -23 dBFS stereo 1 kHz sine reads ~23 LU below the full-scale (≈0 LUFS)
/// anchor, i.e. approximately -23.0 LUFS — coinciding with the EBU R128 program
/// target, since a -23 dBFS 1 kHz tone is the canonical R128 alignment signal.
#[test]
fn minus_23_dbfs_stereo_sine_reads_near_minus_23_lufs() {
    let amp = 10f64.powf(-23.0 / 20.0);
    let mut m = meter(ChannelLayout::Stereo);
    m.push_interleaved(&sine_interleaved(ChannelLayout::Stereo, 1000.0, amp, 4.0))
        .unwrap();
    let i = m.integrated().unwrap();
    approx::assert_abs_diff_eq!(i, -23.0, epsilon = 0.4);
}

/// Pure silence yields no integrated loudness (every block is below the
/// absolute -70 LUFS gate), reported as `None` (conceptually -inf).
#[test]
fn silence_is_gated_to_none() {
    let mut m = meter(ChannelLayout::Stereo);
    let silence = vec![0.0f32; (FS as usize) * 2 * 3]; // 3 s stereo
    m.push_interleaved(&silence).unwrap();
    assert_eq!(m.integrated(), None);
    assert_eq!(m.momentary(), None);
    assert_eq!(m.short_term(), None);
}

/// Gating excludes quiet passages: a signal that is loud for half its duration
/// and silent for the other half integrates to the LOUD level (the silent half
/// is gated out), NOT ~3 dB lower as a naive linear average would give.
#[test]
fn gating_excludes_quiet_passages() {
    let mut loud_only = meter(ChannelLayout::Stereo);
    loud_only
        .push_interleaved(&sine_interleaved(ChannelLayout::Stereo, 1000.0, 0.5, 4.0))
        .unwrap();
    let baseline = loud_only.integrated().unwrap();

    // Same loud tone, then an equal stretch of silence appended.
    let mut mixed = meter(ChannelLayout::Stereo);
    mixed
        .push_interleaved(&sine_interleaved(ChannelLayout::Stereo, 1000.0, 0.5, 4.0))
        .unwrap();
    mixed
        .push_interleaved(&vec![0.0f32; (FS as usize) * 2 * 4])
        .unwrap();
    let gated = mixed.integrated().unwrap();

    // Without gating the average would drop ~3 LU; gating keeps it ~equal.
    approx::assert_abs_diff_eq!(gated, baseline, epsilon = 0.2);
    assert!(
        gated > baseline - 1.0,
        "gating must NOT pull the integrated loudness down toward the silence \
         (baseline={baseline}, gated={gated})"
    );
}

/// Momentary (400 ms) and short-term (3 s) windows of a steady tone converge to
/// the same value as the integrated loudness (a stationary signal).
#[test]
fn momentary_short_term_integrated_agree_for_steady_tone() {
    let mut m = meter(ChannelLayout::Stereo);
    m.push_interleaved(&sine_interleaved(ChannelLayout::Stereo, 1000.0, 0.5, 5.0))
        .unwrap();
    let mom = m.momentary().unwrap();
    let st = m.short_term().unwrap();
    let int = m.integrated().unwrap();
    approx::assert_abs_diff_eq!(mom, st, epsilon = 0.1);
    approx::assert_abs_diff_eq!(st, int, epsilon = 0.1);
}

/// Loudness Range (LRA) of a perfectly steady tone is ~0 LU; a tone that steps
/// between two levels has an LRA spanning that step.
#[test]
fn lra_zero_for_steady_then_spans_a_level_step() {
    let mut steady = meter(ChannelLayout::Stereo);
    steady
        .push_interleaved(&sine_interleaved(ChannelLayout::Stereo, 1000.0, 0.5, 10.0))
        .unwrap();
    let lra_steady = steady.loudness_range().unwrap();
    assert!(
        lra_steady < 1.0,
        "steady tone LRA should be ~0, got {lra_steady}"
    );

    let mut stepped = meter(ChannelLayout::Stereo);
    // 10 s loud, 10 s ~10 dB quieter.
    stepped
        .push_interleaved(&sine_interleaved(ChannelLayout::Stereo, 1000.0, 0.5, 10.0))
        .unwrap();
    let quiet_amp = 0.5 * 10f64.powf(-10.0 / 20.0);
    stepped
        .push_interleaved(&sine_interleaved(
            ChannelLayout::Stereo,
            1000.0,
            quiet_amp,
            10.0,
        ))
        .unwrap();
    let lra_stepped = stepped.loudness_range().unwrap();
    assert!(
        lra_stepped > 5.0,
        "a 10 LU level step should produce a sizeable LRA, got {lra_stepped}"
    );
}

/// True-peak of a 0 dBFS sine is ~0 dBTP, and crucially a near-Nyquist tone
/// sampled near its zero-crossings has an inter-sample peak ABOVE its sample
/// peak — oversampling must reveal that (sample-peak underestimates).
#[test]
fn true_peak_detects_inter_sample_overshoot() {
    // 0 dBFS 1 kHz sine: TP ~ 0 dBTP.
    let mut m = meter(ChannelLayout::Stereo);
    m.push_interleaved(&sine_interleaved(ChannelLayout::Stereo, 1000.0, 1.0, 1.0))
        .unwrap();
    let tp = m.true_peak_dbtp().unwrap();
    approx::assert_abs_diff_eq!(tp, 0.0, epsilon = 0.5);

    // A worst-case near-Nyquist tone: sampled phase misses the true peak, so the
    // sample peak is well below 0 dBFS but the inter-sample (true) peak is ~0.
    let n = FS as usize;
    let mut worst = Vec::with_capacity(n * 2);
    // 12 kHz (= FS/4) sampled at a phase that lands samples at +/- sqrt(2)/2.
    let w = 2.0 * PI * 12_000.0 / f64::from(FS);
    let phase = PI / 4.0;
    let mut sample_peak = 0.0f64;
    for i in 0..n {
        let v = (w * i as f64 + phase).sin();
        sample_peak = sample_peak.max(v.abs());
        let s = v as f32;
        worst.push(s);
        worst.push(s);
    }
    let mut wm = meter(ChannelLayout::Stereo);
    wm.push_interleaved(&worst).unwrap();
    let true_peak_lin = 10f64.powf(wm.true_peak_dbtp().unwrap() / 20.0);
    assert!(
        true_peak_lin > sample_peak + 0.05,
        "oversampled true-peak ({true_peak_lin}) must exceed the raw sample peak \
         ({sample_peak}) for a near-Nyquist tone"
    );
}

/// Sanity: the documented gating constants are the BS.1770 values.
#[test]
fn gating_constants_are_standard() {
    approx::assert_abs_diff_eq!(ABSOLUTE_GATE_LUFS, -70.0, epsilon = 1e-12);
    approx::assert_abs_diff_eq!(RELATIVE_GATE_OFFSET_LU, -10.0, epsilon = 1e-12);
}

/// 5.1 surround channel weighting: the surround channels carry the documented
/// ~+1.5 dB weight (factor 1.41) and the LFE is excluded entirely. Putting the
/// same tone on the surrounds vs the fronts must yield a slightly higher
/// reading for the surrounds.
#[test]
fn surround_channels_weighted_above_front_lfe_excluded() {
    use multiview_audio::loudness::channel_weight;
    // Front L/R/C unity, surround +1.5 dB (~1.41 linear), LFE excluded (0).
    approx::assert_abs_diff_eq!(
        channel_weight(ChannelLayout::FivePointOne, 0),
        1.0,
        epsilon = 1e-9
    );
    let lfe = channel_weight(ChannelLayout::FivePointOne, 3);
    approx::assert_abs_diff_eq!(lfe, 0.0, epsilon = 1e-9);
    let ls = channel_weight(ChannelLayout::FivePointOne, 4);
    approx::assert_abs_diff_eq!(ls, 1.41, epsilon = 0.01);
}
