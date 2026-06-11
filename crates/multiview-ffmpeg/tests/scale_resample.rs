//! libswscale / libswresample wrapper tests on synthetic frames.
//!
//! These need no media file — they build frames in memory — but still gate on
//! the `ffmpeg` feature because they exercise the native conversion contexts.
#![cfg(feature = "ffmpeg")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use ffmpeg::format::{Pixel, Sample};
use ffmpeg::util::frame::{Audio, Video};
use ffmpeg::ChannelLayout;
use ffmpeg_next as ffmpeg;
use multiview_ffmpeg::{ResampleSpec, Resampler, ScaleSpec, Scaler};

const W: u32 = 64;
const H: u32 = 48;

/// Build a YUV420P frame filled with a flat mid-gray and a carried PTS.
fn yuv420p_frame(pts: i64) -> Video {
    let mut frame = Video::new(Pixel::YUV420P, W, H);
    // Plane 0 = luma; planes 1,2 = chroma. Fill deterministically so the
    // conversion has real data to move (not an empty buffer).
    for p in 0..frame.planes() {
        // Mid value for every plane: luma gray and neutral chroma both sit at
        // 128 for 8-bit, which is all the conversion needs to move real data.
        for byte in frame.data_mut(p).iter_mut() {
            *byte = 128_u8;
        }
    }
    frame.set_pts(Some(pts));
    frame
}

#[test]
fn scaler_converts_yuv420p_to_nv12_preserving_size_and_pts() {
    let mut scaler = Scaler::new(
        ScaleSpec::new(Pixel::YUV420P, W, H),
        ScaleSpec::new(Pixel::NV12, W, H),
    )
    .expect("build YUV420P->NV12 scaler");

    let input = yuv420p_frame(7);
    let out = scaler.run(&input).expect("scale");

    assert_eq!(out.format(), Pixel::NV12, "output is NV12");
    assert_eq!(out.width(), W, "width preserved");
    assert_eq!(out.height(), H, "height preserved");
    // NV12 is semi-planar: exactly two planes (Y, interleaved UV).
    assert_eq!(
        out.planes(),
        2,
        "NV12 has a luma + interleaved-chroma plane"
    );
    // The source PTS is carried through unchanged (input time — the caller
    // rebases it; the scaler must not drop or invent it).
    assert_eq!(out.pts(), Some(7), "PTS carried through the scaler");
}

#[test]
fn scaler_rescales_to_a_smaller_canvas() {
    let mut scaler = Scaler::new(
        ScaleSpec::new(Pixel::YUV420P, W, H),
        ScaleSpec::new(Pixel::NV12, W / 2, H / 2),
    )
    .expect("build downscaling scaler");

    let out = scaler.run(&yuv420p_frame(0)).expect("scale");
    assert_eq!(out.width(), W / 2, "downscaled width");
    assert_eq!(out.height(), H / 2, "downscaled height");
}

#[test]
fn scaler_rejects_a_mismatched_input_frame() {
    let mut scaler = Scaler::new(
        ScaleSpec::new(Pixel::YUV420P, W, H),
        ScaleSpec::new(Pixel::NV12, W, H),
    )
    .unwrap();

    // Feed a frame with the wrong geometry: must be a typed error, never a
    // silent mis-scale or a panic.
    let wrong = Video::new(Pixel::YUV420P, W * 2, H);
    match scaler.run(&wrong) {
        Ok(_) => panic!("mismatched input must be rejected"),
        Err(err) => assert!(
            err.to_string().contains("conversion context"),
            "typed FrameMismatch error, got: {err}"
        ),
    }
}

/// Build a packed-S16 stereo audio frame of `samples` frames at `rate`.
fn s16_stereo(samples: usize, rate: u32, pts: i64) -> Audio {
    let mut frame = Audio::new(
        Sample::I16(ffmpeg::format::sample::Type::Packed),
        samples,
        ChannelLayout::STEREO,
    );
    frame.set_rate(rate);
    // Fill the single packed plane with a ramp so resampling has signal.
    let data = frame.data_mut(0);
    for (i, byte) in data.iter_mut().enumerate() {
        *byte = u8::try_from(i % 251).unwrap_or(0);
    }
    frame.set_pts(Some(pts));
    frame
}

#[test]
fn resampler_changes_rate_and_format() {
    // 48 kHz packed S16 stereo -> 44.1 kHz planar FLTP stereo.
    let mut resampler = Resampler::new(
        ResampleSpec::new(
            Sample::I16(ffmpeg::format::sample::Type::Packed),
            ChannelLayout::STEREO,
            48_000,
        ),
        ResampleSpec::new(
            Sample::F32(ffmpeg::format::sample::Type::Planar),
            ChannelLayout::STEREO,
            44_100,
        ),
    )
    .expect("build resampler");

    let input = s16_stereo(1024, 48_000, 11);
    let out = resampler.run(&input).expect("resample");

    assert_eq!(
        out.format(),
        Sample::F32(ffmpeg::format::sample::Type::Planar),
        "output sample format converted"
    );
    assert_eq!(out.rate(), 44_100, "output rate converted");
    assert!(out.samples() > 0, "produced output samples");
}

#[test]
fn resampler_and_scaler_are_send() {
    fn assert_send<T: Send>() {}
    assert_send::<Scaler>();
    assert_send::<Resampler>();
}
