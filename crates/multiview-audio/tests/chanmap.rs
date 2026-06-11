//! Channel mapping / shuffle / de-embed matrix tests.
//!
//! The matrix routes an arbitrary number of input channels (16+ embedded
//! channels) to an output channel set, with optional gain per crosspoint, for
//! de-embedding, shuffling, and downmix. Identity, swap, sum and de-embed are
//! verified against hand-computed answers.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
// reason: synthetic channel indices <-> float are exact for the small ranges
// used here; test-only.
#![allow(clippy::as_conversions, clippy::cast_precision_loss, clippy::float_cmp)]

use multiview_audio::chanmap::ChannelMatrix;

#[test]
fn identity_matrix_passes_channels_through() {
    let m = ChannelMatrix::identity(4);
    let out = m.apply(&[1.0, 2.0, 3.0, 4.0]).unwrap();
    assert_eq!(out, vec![1.0, 2.0, 3.0, 4.0]);
}

#[test]
fn swap_shuffles_two_channels() {
    // 2->2 swap: out0 = in1, out1 = in0.
    let m = ChannelMatrix::from_routes(2, 2, &[(1, 0, 1.0), (0, 1, 1.0)]).unwrap();
    let out = m.apply(&[0.25, 0.75]).unwrap();
    assert_eq!(out, vec![0.75, 0.25]);
}

#[test]
fn de_embed_selects_a_pair_from_sixteen() {
    // 16 embedded channels in; pull channels 8 and 9 to a stereo pair out.
    let m = ChannelMatrix::from_routes(16, 2, &[(8, 0, 1.0), (9, 1, 1.0)]).unwrap();
    let mut input = vec![0.0f32; 16];
    input[8] = 0.5;
    input[9] = -0.5;
    let out = m.apply(&input).unwrap();
    assert_eq!(out, vec![0.5, -0.5]);
}

#[test]
fn sum_mixes_with_gain() {
    // Mono fold-down of a stereo pair at -6 dB each.
    let g = 0.5f32; // ~ -6 dB
    let m = ChannelMatrix::from_routes(2, 1, &[(0, 0, g), (1, 0, g)]).unwrap();
    let out = m.apply(&[1.0, 1.0]).unwrap();
    assert_eq!(out, vec![1.0]);
}

#[test]
fn supports_sixteen_plus_output_channels() {
    let m = ChannelMatrix::identity(16);
    assert_eq!(m.inputs(), 16);
    assert_eq!(m.outputs(), 16);
    let input: Vec<f32> = (0..16).map(|i| i as f32).collect();
    let out = m.apply(&input).unwrap();
    assert_eq!(out.len(), 16);
    assert_eq!(out[15], 15.0);
}

#[test]
fn wrong_input_length_is_an_error() {
    let m = ChannelMatrix::identity(4);
    assert!(m.apply(&[1.0, 2.0]).is_err());
}

#[test]
fn out_of_range_route_is_rejected_at_build() {
    // Output index 5 does not exist in a 2-output matrix.
    assert!(ChannelMatrix::from_routes(2, 2, &[(0, 5, 1.0)]).is_err());
    assert!(ChannelMatrix::from_routes(2, 2, &[(9, 0, 1.0)]).is_err());
}

#[test]
fn apply_interleaved_processes_every_frame() {
    let m = ChannelMatrix::from_routes(2, 2, &[(1, 0, 1.0), (0, 1, 1.0)]).unwrap();
    // Two frames of stereo, frame-major.
    let out = m.apply_interleaved(&[1.0, 2.0, 3.0, 4.0]).unwrap();
    assert_eq!(out, vec![2.0, 1.0, 4.0, 3.0]);
}
