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

/// Build an NV12 frame painted solid "red" (Y=81, Cb=90, Cr=240) with a PTS —
/// strongly asymmetric chroma, so any plane loss/swap is unambiguous.
fn red_nv12(w: u32, h: u32, pts: i64) -> Video {
    let mut frame = Video::new(Pixel::NV12, w, h);
    let stride0 = frame.stride(0);
    for row in 0..usize::try_from(h).unwrap() {
        for col in 0..usize::try_from(w).unwrap() {
            frame.data_mut(0)[row * stride0 + col] = 81;
        }
    }
    let stride1 = frame.stride(1);
    for row in 0..usize::try_from(h / 2).unwrap() {
        for col in 0..usize::try_from(w / 2).unwrap() {
            frame.data_mut(1)[row * stride1 + col * 2] = 90;
            frame.data_mut(1)[row * stride1 + col * 2 + 1] = 240;
        }
    }
    frame.set_pts(Some(pts));
    frame
}

/// Mean (Cb, Cr) over an NV12 frame's interleaved chroma plane.
fn nv12_uv_means(frame: &Video) -> (f64, f64) {
    let stride1 = frame.stride(1);
    let data = frame.data(1);
    let (mut cb, mut cr, mut n) = (0_f64, 0_f64, 0_f64);
    for row in 0..usize::try_from(frame.height() / 2).unwrap() {
        for col in 0..usize::try_from(frame.width() / 2).unwrap() {
            cb += f64::from(data[row * stride1 + col * 2]);
            cr += f64::from(data[row * stride1 + col * 2 + 1]);
            n += 1.0;
        }
    }
    (cb / n, cr / n)
}

/// THE DEFECT-A ROOT-CAUSE PIN (hardware run 2026-06-11): an **identity**
/// request (same format, same geometry — NV12 -> NV12 at one size) must hand
/// back the input picture intact. libswscale's no-op converter on `FFmpeg` 7/8
/// copies the luma plane but leaves the interleaved NV12 chroma plane ZEROED
/// (Cb=Cr=0 ⇒ the saturated green/magenta the live-added tile showed), so the
/// wrapper must never route an identity request through `sws_scale`.
#[test]
fn identity_nv12_to_nv12_preserves_the_chroma_plane() {
    let spec = ScaleSpec::new(Pixel::NV12, W, H);
    let mut scaler = Scaler::new(spec, spec).expect("build identity NV12 scaler");

    let input = red_nv12(W, H, 21);
    let out = scaler.run(&input).expect("identity convert");

    assert_eq!(out.format(), Pixel::NV12, "format preserved");
    assert_eq!((out.width(), out.height()), (W, H), "geometry preserved");
    assert_eq!(out.pts(), Some(21), "PTS carried through");
    // Luma intact…
    assert_eq!(out.data(0)[0], 81, "luma plane copied");
    // …and the interleaved chroma plane intact too: red keeps Cr >> Cb. The
    // broken no-op path zeroes both means.
    let (cb, cr) = nv12_uv_means(&out);
    assert!(
        (cb - 90.0).abs() < 1.0 && (cr - 240.0).abs() < 1.0,
        "identity scale must preserve the NV12 chroma plane (got cb={cb:.1}, cr={cr:.1}; \
         the libswscale no-op path zeroes it)"
    );
}

#[test]
fn resampler_and_scaler_are_send() {
    fn assert_send<T: Send>() {}
    assert_send::<Scaler>();
    assert_send::<Resampler>();
}
