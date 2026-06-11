//! Confidence scopes: waveform, vectorscope, histogram and RGB parade as pure
//! analysis over caller-supplied sample data, producing a drawable model. Bucket
//! counts for a known ramp / known constant are checked exactly.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use multiview_overlay::scopes::{
    Histogram, Parade, ParadeChannel, RgbParade, Vectorscope, Waveform,
};

#[test]
fn histogram_buckets_a_constant_into_one_bin() {
    // 1000 samples all = 128 land in exactly one bucket.
    let samples = [128u8; 1000];
    let hist = Histogram::<256>::from_luma(&samples);
    assert_eq!(hist.bins[128], 1000);
    let total: u64 = hist.bins.iter().sum();
    assert_eq!(total, 1000);
    // Every other bin is empty.
    assert_eq!(hist.bins.iter().filter(|&&c| c > 0).count(), 1);
}

#[test]
fn histogram_of_full_ramp_is_uniform() {
    // 0..=255 exactly once each -> one count per bin.
    let samples: Vec<u8> = (0..=255u16).map(|v| u8::try_from(v).unwrap()).collect();
    let hist = Histogram::<256>::from_luma(&samples);
    for (i, &count) in hist.bins.iter().enumerate() {
        assert_eq!(count, 1, "bin {i} should have exactly one sample");
    }
    assert_eq!(hist.total(), 256);
}

#[test]
fn histogram_downscales_buckets() {
    // 64 buckets over a 256-value ramp -> 4 samples per bucket.
    let samples: Vec<u8> = (0..=255u16).map(|v| u8::try_from(v).unwrap()).collect();
    let hist = Histogram::<64>::from_luma(&samples);
    for (i, &count) in hist.bins.iter().enumerate() {
        assert_eq!(count, 4, "bin {i} should have four samples");
    }
}

#[test]
fn histogram_peak_bin_is_the_mode() {
    let mut samples = vec![10u8; 5];
    samples.extend(std::iter::repeat_n(200u8, 20));
    let hist = Histogram::<256>::from_luma(&samples);
    assert_eq!(hist.peak_bin(), Some(200));
}

#[test]
fn empty_histogram_has_no_peak() {
    let hist = Histogram::<256>::from_luma(&[]);
    assert_eq!(hist.peak_bin(), None);
    assert_eq!(hist.total(), 0);
}

#[test]
fn waveform_column_min_max_track_vertical_ramp() {
    // A 4-wide, 4-tall image where each column is a vertical ramp 0,85,170,255.
    // Row-major luma: row r, col c -> value = r * 85.
    let width = 4usize;
    let height = 4usize;
    let mut luma = vec![0u8; width * height];
    for (r, chunk) in luma.chunks_mut(width).enumerate() {
        for px in chunk.iter_mut() {
            *px = u8::try_from(r * 85).unwrap();
        }
    }
    let wf = Waveform::from_luma(&luma, width, height).unwrap();
    assert_eq!(wf.columns.len(), width);
    for col in &wf.columns {
        assert_eq!(col.min, 0);
        assert_eq!(col.max, 255);
    }
}

#[test]
fn waveform_rejects_mismatched_dimensions() {
    let luma = [0u8; 10];
    assert!(Waveform::from_luma(&luma, 4, 4).is_err());
}

#[test]
fn vectorscope_centres_on_neutral_grey() {
    // Neutral grey -> chroma at (128,128) -> all energy at the origin bin.
    let cb = [128u8; 100];
    let cr = [128u8; 100];
    let vs = Vectorscope::<64>::from_chroma(&cb, &cr).unwrap();
    let centre = vs.bin_index(128, 128);
    assert_eq!(vs.bins[centre.0][centre.1], 100);
    let total: u64 = vs.bins.iter().flatten().sum();
    assert_eq!(total, 100);
}

#[test]
fn vectorscope_rejects_length_mismatch() {
    let cb = [128u8; 10];
    let cr = [128u8; 9];
    assert!(Vectorscope::<64>::from_chroma(&cb, &cr).is_err());
}

#[test]
fn rgb_parade_has_one_histogram_per_channel() {
    // 3 pixels: red, green, blue (each fully saturated on its channel).
    let rgb = [255u8, 0, 0, 0, 255, 0, 0, 0, 255];
    let parade = RgbParade::<256>::from_rgb(&rgb).unwrap();
    assert_eq!(parade.red.bins[255], 1);
    assert_eq!(parade.red.bins[0], 2);
    assert_eq!(parade.green.bins[255], 1);
    assert_eq!(parade.blue.bins[255], 1);
}

#[test]
fn rgb_parade_rejects_non_triple_length() {
    let rgb = [0u8; 8]; // not a multiple of 3
    assert!(RgbParade::<256>::from_rgb(&rgb).is_err());
}

#[test]
fn parade_channel_labels_are_text() {
    // a11y: channels identified by text, not colour alone.
    assert_eq!(ParadeChannel::Red.label(), "R");
    assert_eq!(ParadeChannel::Green.label(), "G");
    assert_eq!(ParadeChannel::Blue.label(), "B");
}

#[test]
fn parade_kind_round_trips_through_json() {
    let p = Parade::Rgb;
    let json = serde_json::to_string(&p).unwrap();
    let back: Parade = serde_json::from_str(&json).unwrap();
    assert_eq!(p, back);
}
