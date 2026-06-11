//! Hardware-frame lifecycle scaffold tests.
//!
//! These run on a GPU-free machine: they assert the RAII handles are `Send`,
//! that `libav_name` mappings are stable, and that creating a device context
//! on a host with no GPU/driver fails with a **typed error** rather than
//! panicking (the leak-free, panic-safe contract). A real surface allocation is
//! GPU-only and lives in the GPU-tagged test tier, not here.
#![cfg(feature = "ffmpeg")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use ffmpeg_next as ffmpeg;
use multiview_ffmpeg::{HwDeviceContext, HwDeviceKind, HwFramesContext};

#[test]
fn device_kind_names_are_stable() {
    assert_eq!(HwDeviceKind::Cuda.libav_name(), "cuda");
    assert_eq!(HwDeviceKind::Vaapi.libav_name(), "vaapi");
    assert_eq!(HwDeviceKind::Qsv.libav_name(), "qsv");
    assert_eq!(HwDeviceKind::VideoToolbox.libav_name(), "videotoolbox");
}

#[test]
fn creating_a_device_without_a_gpu_is_a_typed_error_not_a_panic() {
    // On the GPU-free CI host, creating any hardware device must surface a
    // typed error. It must NOT panic and must NOT leak — the function owns and
    // frees any partially-allocated ref internally.
    //
    // We accept either outcome defensively: an error (no device/driver — the
    // expected CI path) OR success (if a runner genuinely has the device). The
    // load-bearing assertion is "no panic, typed Result".
    for kind in [
        HwDeviceKind::Cuda,
        HwDeviceKind::Vaapi,
        HwDeviceKind::Qsv,
        HwDeviceKind::VideoToolbox,
    ] {
        match HwDeviceContext::create(kind, None) {
            Ok(device) => {
                // If a device DID open, the kind round-trips and we can drop it
                // cleanly (the Drop releases the AVBufferRef).
                assert_eq!(device.kind(), kind);
                assert!(!device.as_raw().is_null(), "live device has a non-null ref");
                drop(device);
            }
            Err(err) => {
                // Typed error rendered without panicking — the CI-green path.
                let rendered = err.to_string();
                assert!(!rendered.is_empty(), "error renders to a message");
            }
        }
    }
}

#[test]
fn hw_handles_are_send() {
    fn assert_send<T: Send>() {}
    assert_send::<HwDeviceContext>();
    assert_send::<HwFramesContext>();
}

/// Build an unopened H.264 video-encoder context (its software codec id is fine
/// — `attach_to_encoder` only writes `hw_device_ctx`, it does not open). Returns
/// `None` if no H.264 encoder is in this build.
fn unopened_video_encoder() -> Option<ffmpeg::codec::encoder::video::Video> {
    let codec = ffmpeg::encoder::find(ffmpeg::codec::Id::H264)?;
    let enc = ffmpeg::codec::context::Context::new_with_codec(codec)
        .encoder()
        .video()
        .ok()?;
    Some(enc)
}

#[test]
fn attach_to_encoder_on_a_built_unopened_ctx_is_a_typed_result_not_a_panic() {
    // P1a: the encoder hw-device bind seam. On a GPU-free host the CUDA device
    // create returns a typed error (the GPU-free CI path); on a real CUDA box it
    // succeeds and `attach_to_encoder` writes `hw_device_ctx` on the still-
    // unopened encoder context. Either way: a typed `Result`, never a panic —
    // and the bind, when it runs, happens BEFORE the encoder is opened.
    let Some(mut enc) = unopened_video_encoder() else {
        // No H.264 encoder in this build — nothing to bind; not a failure.
        return;
    };
    match HwDeviceContext::create(HwDeviceKind::Cuda, Some("0")) {
        Ok(device) => {
            // A real CUDA device: attaching to the unopened encoder must succeed
            // and take a NEW owning ref (device keeps its own).
            device
                .attach_to_encoder(&mut enc)
                .expect("attach to unopened encoder ctx");
            assert!(!device.as_raw().is_null(), "device keeps its own ref");
            drop(enc);
            drop(device);
        }
        Err(err) => {
            // GPU-free CI: the create is a typed error, rendered without a panic.
            assert!(!err.to_string().is_empty(), "error renders to a message");
        }
    }
}
