//! The `NdiOutput` sink seam over a fake API table (no SDK): the drive loop,
//! tick-restamped timecode, frame validation, create-failure, and graceful close.
//!
//! Actually sending to a real NDI receiver needs the proprietary runtime + a live
//! NDI network and is gated behind the `ndi_live.rs` ignored-by-default test.
#![cfg(feature = "ndi")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use multiview_output::ndi::license::LicenseAcceptance;
use multiview_output::ndi::{
    FakeNdiApi, NdiFourCc, NdiLicense, NdiOutput, NdiSendError, NdiVideoFrame,
};

fn accepted() -> NdiLicense {
    NdiLicense::accept(LicenseAcceptance {
        accepted_by: "ops".to_owned(),
        accepted_at: "2026-06-06T00:00:00Z".to_owned(),
    })
    .unwrap()
}

fn uyvy_frame(width: u32, height: u32, timecode: i64, buf: &[u8]) -> NdiVideoFrame<'_> {
    NdiVideoFrame {
        width,
        height,
        stride: width * 2,
        fourcc: NdiFourCc::Uyvy,
        frame_rate_n: 30_000,
        frame_rate_d: 1001,
        timecode,
        data: buf,
    }
}

#[test]
fn drives_frames_with_tick_restamped_timecode() {
    let mut out = NdiOutput::new(accepted(), FakeNdiApi::new(), "OUT").unwrap();
    let buf = vec![0u8; 4 * 2 * 2]; // 4x2 UYVY = stride 8 * height 2, in bytes
    for tick in 0..3i64 {
        let tc = tick * 333_667; // 100ns units, monotonic from the tick counter
        out.send(&uyvy_frame(4, 2, tc, &buf)).expect("send ok");
    }
    let sent = &out.api().sent;
    assert_eq!(sent.len(), 3);
    // Timecodes are exactly what we stamped, in order (never raw input PTS).
    assert_eq!(
        sent.iter().map(|s| s.3).collect::<Vec<_>>(),
        vec![0, 333_667, 667_334]
    );
    assert!(sent.iter().all(|s| s.2 == NdiFourCc::Uyvy));
}

#[test]
fn invalid_frame_is_rejected_not_panicked() {
    let mut out = NdiOutput::new(accepted(), FakeNdiApi::new(), "OUT").unwrap();
    // Buffer too small for stride*height.
    let tiny = vec![0u8; 3];
    let frame = uyvy_frame(4, 2, 0, &tiny);
    let err = out.send(&frame).expect_err("short buffer must be refused");
    assert!(matches!(err, NdiSendError::InvalidFrame { .. }));
    assert!(out.api().sent.is_empty());
}

#[test]
fn create_failure_does_not_open_a_sender() {
    let err = NdiOutput::new(accepted(), FakeNdiApi::failing_create("name taken"), "OUT")
        .expect_err("create failure must surface");
    assert!(matches!(err, NdiSendError::CreateFailed { .. }));
}

#[test]
fn send_after_close_is_typed_closed_error() {
    let buf = vec![0u8; 16];
    let mut out = NdiOutput::new(accepted(), FakeNdiApi::new(), "OUT").unwrap();
    out.close();
    assert!(!out.is_open());
    let err = out.send(&uyvy_frame(4, 2, 0, &buf)).expect_err("closed");
    assert!(matches!(err, NdiSendError::Closed));
    // Close is idempotent.
    out.close();
}

#[test]
fn fourcc_tags_are_the_on_wire_bytes() {
    assert_eq!(&NdiFourCc::Uyvy.tag(), b"UYVY");
    assert_eq!(&NdiFourCc::P216.tag(), b"P216");
    assert_eq!(&NdiFourCc::Bgra.tag(), b"BGRA");
}

#[test]
fn drives_planar_audio_with_tick_restamped_timecode() {
    let mut out = NdiOutput::new(accepted(), FakeNdiApi::new(), "OUT").unwrap();
    // 2 channels × 4 samples of planar f32.
    let audio = vec![0.25f32; 2 * 4];
    for tick in 0..3i64 {
        let tc = tick * 333_667;
        out.send_audio_planar(48_000, 2, 4, tc, &audio)
            .expect("audio send ok");
    }
    let sent = &out.api().sent_audio;
    assert_eq!(sent.len(), 3);
    // (sample_rate, channels, samples, timecode) recorded exactly, in order.
    assert_eq!(sent[0], (48_000, 2, 4, 0));
    assert_eq!(
        sent.iter().map(|a| a.3).collect::<Vec<_>>(),
        vec![0, 333_667, 667_334]
    );
}

#[test]
fn invalid_audio_is_rejected_not_panicked() {
    let mut out = NdiOutput::new(accepted(), FakeNdiApi::new(), "OUT").unwrap();
    // Buffer too short for channels*samples (need 8, have 3).
    let tiny = vec![0.0f32; 3];
    let err = out
        .send_audio_planar(48_000, 2, 4, 0, &tiny)
        .expect_err("short audio buffer must be refused");
    assert!(matches!(err, NdiSendError::InvalidFrame { .. }));
    // Zero geometry is likewise a typed refusal.
    let err = out
        .send_audio_planar(48_000, 0, 4, 0, &tiny)
        .expect_err("zero channels must be refused");
    assert!(matches!(err, NdiSendError::InvalidFrame { .. }));
    assert!(out.api().sent_audio.is_empty());
}

#[test]
fn audio_after_close_is_typed_closed_error() {
    let mut out = NdiOutput::new(accepted(), FakeNdiApi::new(), "OUT").unwrap();
    out.close();
    let audio = vec![0.0f32; 8];
    let err = out
        .send_audio_planar(48_000, 2, 4, 0, &audio)
        .expect_err("closed");
    assert!(matches!(err, NdiSendError::Closed));
}
