//! Property tests for the loudness/DSP invariants.
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
    clippy::cast_sign_loss
)]

use std::f64::consts::PI;

use multiview_audio::loudness::LoudnessMeter;
use multiview_audio::{AudioFormat, ChannelLayout};
use proptest::prelude::*;

const FS: u32 = 48_000;

fn sine(amp: f64, freq: f64, seconds: f64) -> Vec<f32> {
    let n = (f64::from(FS) * seconds).round() as usize;
    let w = 2.0 * PI * freq / f64::from(FS);
    let mut v = Vec::with_capacity(n * 2);
    for i in 0..n {
        let s = (amp * (w * i as f64).sin()) as f32;
        v.push(s);
        v.push(s);
    }
    v
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(48))]

    /// Loudness is linear in dBFS: scaling the input by `g` dB shifts the
    /// integrated loudness by exactly `g` LU (the meter is LTI), independent of
    /// the carrier frequency or absolute level.
    #[test]
    fn integrated_loudness_tracks_gain_in_db(
        gain_db in -40.0f64..=0.0,
        freq in 200.0f64..=4000.0,
    ) {
        let mut reference = LoudnessMeter::new(AudioFormat::new(FS, ChannelLayout::Stereo)).unwrap();
        reference.push_interleaved(&sine(0.5, freq, 3.0)).unwrap();
        let l_ref = reference.integrated().unwrap();

        let amp = 0.5 * 10f64.powf(gain_db / 20.0);
        let mut scaled = LoudnessMeter::new(AudioFormat::new(FS, ChannelLayout::Stereo)).unwrap();
        scaled.push_interleaved(&sine(amp, freq, 3.0)).unwrap();
        let l_scaled = scaled.integrated().unwrap();

        prop_assert!((l_scaled - (l_ref + gain_db)).abs() < 0.1,
            "freq={freq} gain={gain_db}: ref={l_ref} scaled={l_scaled}");
    }

    /// Pushing a signal in two halves yields the same integrated loudness as
    /// pushing it in one call (streaming accumulation is consistent).
    #[test]
    fn streaming_chunks_match_single_push(amp in 0.05f64..=0.9) {
        let whole = sine(amp, 1000.0, 4.0);
        let mut one = LoudnessMeter::new(AudioFormat::new(FS, ChannelLayout::Stereo)).unwrap();
        one.push_interleaved(&whole).unwrap();
        let l_one = one.integrated().unwrap();

        let mut split = LoudnessMeter::new(AudioFormat::new(FS, ChannelLayout::Stereo)).unwrap();
        let mid = (whole.len() / 4) * 2; // keep frame alignment (stereo)
        split.push_interleaved(&whole[..mid]).unwrap();
        split.push_interleaved(&whole[mid..]).unwrap();
        let l_split = split.integrated().unwrap();

        prop_assert!((l_one - l_split).abs() < 0.05,
            "single={l_one} split={l_split}");
    }

    /// True-peak is never below the raw sample peak (oversampling can only find
    /// equal-or-higher peaks).
    #[test]
    fn true_peak_at_least_sample_peak(amp in 0.01f64..=1.0, freq in 500.0f64..=20000.0) {
        let s = sine(amp, freq, 0.5);
        let sample_peak = s.iter().fold(0.0f32, |m, &x| m.max(x.abs()));
        let mut meter = LoudnessMeter::new(AudioFormat::new(FS, ChannelLayout::Stereo)).unwrap();
        meter.push_interleaved(&s).unwrap();
        if let Some(tp_db) = meter.true_peak_dbtp() {
            let tp_lin = 10f64.powf(tp_db / 20.0);
            prop_assert!(tp_lin + 1e-3 >= f64::from(sample_peak),
                "tp_lin={tp_lin} sample_peak={sample_peak}");
        }
    }
}
