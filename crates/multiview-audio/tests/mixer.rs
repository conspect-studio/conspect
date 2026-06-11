//! Mix/route model tests: the program bus + discrete tracks and the gain/route
//! matrix (per ADR-R005). This is the pure-Rust routing MODEL — decode/resample
//! live behind the off-by-default `ffmpeg` feature, so these tests operate on
//! in-memory PCM blocks.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use multiview_audio::mixer::{Mixer, RoutePoint};
use multiview_audio::{AudioBlock, AudioError, AudioFormat, ChannelLayout};

const FS: u32 = 48_000;

fn stereo_block(left: f32, right: f32, frames: usize) -> AudioBlock {
    let mut samples = Vec::with_capacity(frames * 2);
    for _ in 0..frames {
        samples.push(left);
        samples.push(right);
    }
    AudioBlock::from_interleaved(AudioFormat::new(FS, ChannelLayout::Stereo), samples).unwrap()
}

/// A mixer with two inputs routed to the program bus at unit gain sums the
/// inputs sample-for-sample.
#[test]
fn program_bus_sums_routed_inputs() {
    let mut mixer = Mixer::new(AudioFormat::new(FS, ChannelLayout::Stereo));
    let a = mixer.add_input("cam-a");
    let b = mixer.add_input("cam-b");
    mixer.route_to_program(a, 1.0);
    mixer.route_to_program(b, 1.0);

    mixer.submit(a, stereo_block(0.2, -0.1, 4)).unwrap();
    mixer.submit(b, stereo_block(0.1, 0.3, 4)).unwrap();

    let bus = mixer.mix_program().unwrap();
    assert_eq!(bus.format().channel_layout(), ChannelLayout::Stereo);
    let s = bus.interleaved();
    approx::assert_abs_diff_eq!(s[0], 0.3, epsilon = 1e-6);
    approx::assert_abs_diff_eq!(s[1], 0.2, epsilon = 1e-6);
}

/// Per-route gain scales the contribution; a -6 dB route (~0.5012) halves the
/// energy contribution of that input.
#[test]
fn route_gain_scales_contribution() {
    let mut mixer = Mixer::new(AudioFormat::new(FS, ChannelLayout::Stereo));
    let a = mixer.add_input("a");
    mixer.route_to_program(a, 0.5);
    mixer.submit(a, stereo_block(0.4, 0.4, 2)).unwrap();
    let bus = mixer.mix_program().unwrap();
    approx::assert_abs_diff_eq!(bus.interleaved()[0], 0.2, epsilon = 1e-6);
}

/// A mix that would clip is hard-limited to [-1, 1] so the bus never overflows
/// the sample domain.
#[test]
fn program_bus_clamps_to_sample_domain() {
    let mut mixer = Mixer::new(AudioFormat::new(FS, ChannelLayout::Stereo));
    let a = mixer.add_input("a");
    let b = mixer.add_input("b");
    mixer.route_to_program(a, 1.0);
    mixer.route_to_program(b, 1.0);
    mixer.submit(a, stereo_block(0.8, 0.8, 1)).unwrap();
    mixer.submit(b, stereo_block(0.8, 0.8, 1)).unwrap();
    let bus = mixer.mix_program().unwrap();
    let s = bus.interleaved();
    approx::assert_abs_diff_eq!(s[0], 1.0, epsilon = 1e-6);
    assert!(s[0] <= 1.0 && s[1] <= 1.0);
}

/// Discrete tracks are carried unaltered (ADR-R005/R006: "leave discrete tracks
/// unaltered"): the discrete track for an input equals that input's submitted
/// block, regardless of its program-bus route gain.
#[test]
fn discrete_track_is_unaltered_by_program_route() {
    let mut mixer = Mixer::new(AudioFormat::new(FS, ChannelLayout::Stereo));
    let a = mixer.add_input("a");
    mixer.route_to_program(a, 0.25); // attenuated on the bus...
    let block = stereo_block(0.6, -0.6, 3);
    mixer.submit(a, block).unwrap();

    let discrete = mixer.discrete_track(a).unwrap();
    let s = discrete.interleaved();
    approx::assert_abs_diff_eq!(s[0], 0.6, epsilon = 1e-6); // ...but discrete is untouched
    approx::assert_abs_diff_eq!(s[1], -0.6, epsilon = 1e-6);
}

/// An input with NO program route does not contribute to the bus, but still has
/// a discrete track available.
#[test]
fn unrouted_input_absent_from_bus_present_as_discrete() {
    let mut mixer = Mixer::new(AudioFormat::new(FS, ChannelLayout::Stereo));
    let a = mixer.add_input("a");
    let b = mixer.add_input("b");
    mixer.route_to_program(a, 1.0);
    // b is deliberately not routed to program.
    mixer.submit(a, stereo_block(0.2, 0.2, 1)).unwrap();
    mixer.submit(b, stereo_block(0.5, 0.5, 1)).unwrap();

    let bus = mixer.mix_program().unwrap();
    approx::assert_abs_diff_eq!(bus.interleaved()[0], 0.2, epsilon = 1e-6);
    assert!(mixer.discrete_track(b).is_some());
}

/// When an input has no fresh block (a dropout), the program bus fills silence
/// for that input rather than stalling or reusing stale audio — gap-free output
/// (invariant: the engine never blocks on an input).
#[test]
fn dropout_fills_silence_not_stall() {
    let mut mixer = Mixer::new(AudioFormat::new(FS, ChannelLayout::Stereo));
    let a = mixer.add_input("a");
    let b = mixer.add_input("b");
    mixer.route_to_program(a, 1.0);
    mixer.route_to_program(b, 1.0);
    // Only a delivers audio this tick; b dropped.
    mixer.submit(a, stereo_block(0.3, 0.3, 2)).unwrap();
    let bus = mixer.mix_program().unwrap();
    // b contributes silence => bus == a's block.
    approx::assert_abs_diff_eq!(bus.interleaved()[0], 0.3, epsilon = 1e-6);
    assert_eq!(bus.frame_count(), 2);
}

/// Submitting a block whose format mismatches the mixer is a typed error, never
/// a panic.
#[test]
fn format_mismatch_is_typed_error() {
    let mut mixer = Mixer::new(AudioFormat::new(FS, ChannelLayout::Stereo));
    let a = mixer.add_input("a");
    let mono = AudioBlock::from_interleaved(
        AudioFormat::new(FS, ChannelLayout::Mono),
        vec![0.1, 0.2, 0.3],
    )
    .unwrap();
    let err = mixer.submit(a, mono).unwrap_err();
    assert!(matches!(err, AudioError::FormatMismatch { .. }));
}

/// A route to an unknown input id is a typed error.
#[test]
fn submit_to_unknown_input_is_typed_error() {
    let mut mixer = Mixer::new(AudioFormat::new(FS, ChannelLayout::Stereo));
    let _a = mixer.add_input("a");
    let bogus = RoutePoint::input(999);
    let err = mixer.submit(bogus, stereo_block(0.1, 0.1, 1)).unwrap_err();
    assert!(matches!(err, AudioError::UnknownInput(_)));
}

/// `AudioBlock::from_interleaved` rejects a sample count that is not a whole
/// number of frames for the layout.
#[test]
fn ragged_block_rejected() {
    let err = AudioBlock::from_interleaved(
        AudioFormat::new(FS, ChannelLayout::Stereo),
        vec![0.1, 0.2, 0.3], // 3 samples, not divisible by 2 channels
    )
    .unwrap_err();
    assert!(matches!(err, AudioError::RaggedBlock { .. }));
}
