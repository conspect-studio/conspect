//! IN-3 — NDI ingest receive→NV12 conversion + the `NdiProducer` `FrameProducer`
//! adapter, exercised over a deterministic injected fake receiver (no SDK).
//!
//! These tests are gated to the off-by-default `ndi` feature (the only feature
//! that compiles the `ndi` module + its `multiview-ndi-sys` runtime-load seam).
//! They are pure CPU logic — the NDI runtime is never required — so they run in
//! CI under `cargo test -p multiview-input --features ndi`. The live receive from
//! a real NDI sender lives in a separate `#[ignore]`d test (no NDI network here).
#![cfg(feature = "ndi")]
#![allow(
    // reason: integration tests assert on known-good values and unwrap fixtures;
    // the strict workspace lints are relaxed for `tests/` per CLAUDE.md.
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]

use multiview_core::time::MediaTime;
use multiview_framestore::{NoSignalPolicy, TileStore, TileThresholds};
use multiview_input::ndi::convert::{bgra_to_nv12, uyvy_to_nv12, ReceivedVideoFrame};
use multiview_input::ndi::receiver::{FakeNdiReceiver, NdiRecvFourCc, ReceivedFrame};
use multiview_input::ndi::NdiProducer;
use multiview_input::normalize::WrapBits;
use multiview_input::source::{FrameProducer, IngestConfig, IngestPump};

/// A 2x2 UYVY frame: two UYVY groups (one per row), `U Y0 V Y1` per group.
/// Row 0: U=100, Y0=10, V=200, Y1=20. Row 1: U=110, Y0=30, V=210, Y1=40.
fn uyvy_2x2() -> ReceivedVideoFrame {
    let data = vec![
        100, 10, 200, 20, // row 0
        110, 30, 210, 40, // row 1
    ];
    ReceivedVideoFrame::new(2, 2, NdiRecvFourCc::Uyvy, 2 * 2, data).unwrap()
}

#[test]
fn uyvy_to_nv12_geometry_and_luma() {
    let frame = uyvy_2x2();
    let nv12 = uyvy_to_nv12(&frame).unwrap();
    // NV12 planes: Y is width*height, UV is width*height/2.
    assert_eq!(nv12.y_plane().len(), 4);
    assert_eq!(nv12.uv_plane().len(), 2);
    // Y is taken verbatim from the UYVY luma bytes (no resample).
    assert_eq!(nv12.y_plane(), &[10, 20, 30, 40]);
    // 4:2:2 → 4:2:0: the two vertically-stacked chroma rows are averaged into the
    // single NV12 chroma row. U = (100+110)/2 = 105, V = (200+210)/2 = 205.
    assert_eq!(nv12.uv_plane(), &[105, 205]);
}

#[test]
fn uyvy_stride_with_padding_is_respected() {
    // A row stride wider than width*2 (padding bytes after the packed pixels):
    // the converter must read only the first width*2 bytes of each row.
    let stride = 6u32; // width*2 = 4, plus 2 pad bytes/row
    let data = vec![
        100, 10, 200, 20, 0xEE, 0xEE, // row 0 + padding
        110, 30, 210, 40, 0xEE, 0xEE, // row 1 + padding
    ];
    let frame = ReceivedVideoFrame::new(2, 2, NdiRecvFourCc::Uyvy, stride, data).unwrap();
    let nv12 = uyvy_to_nv12(&frame).unwrap();
    // Padding must not leak into the luma plane.
    assert_eq!(nv12.y_plane(), &[10, 20, 30, 40]);
    assert_eq!(nv12.uv_plane(), &[105, 205]);
}

#[test]
fn bgra_to_nv12_white_and_black_extremes() {
    // 2x2 BGRA: top-left white, the rest black. White → high luma, black → Y≈16.
    let data = vec![
        255, 255, 255, 255, /* white */ 0, 0, 0, 255, // row 0
        0, 0, 0, 255, 0, 0, 0, 255, // row 1
    ];
    let frame = ReceivedVideoFrame::new(2, 2, NdiRecvFourCc::Bgra, 2 * 4, data).unwrap();
    let nv12 = bgra_to_nv12(&frame).unwrap();
    assert_eq!(nv12.y_plane().len(), 4);
    assert_eq!(nv12.uv_plane().len(), 2);
    // BT.709 limited-range: white ≈ 235, black ≈ 16.
    assert!(
        nv12.y_plane()[0] > 200,
        "white luma {} too low",
        nv12.y_plane()[0]
    );
    assert_eq!(nv12.y_plane()[1], 16, "black luma must be the 16 floor");
    assert_eq!(nv12.y_plane()[3], 16);
}

#[test]
fn received_frame_rejects_short_buffer_no_panic() {
    // A buffer too short for stride*height is a typed refusal, never a panic.
    let err = ReceivedVideoFrame::new(2, 2, NdiRecvFourCc::Uyvy, 4, vec![0u8; 3]);
    assert!(err.is_err());
}

#[test]
fn received_frame_rejects_odd_dimensions() {
    let err = ReceivedVideoFrame::new(3, 2, NdiRecvFourCc::Uyvy, 6, vec![0u8; 12]);
    assert!(err.is_err());
}

#[test]
fn producer_samples_and_yields_nv12_frames() {
    // The fake receiver hands two UYVY frames then None (no signal). The producer
    // converts each to an NV12 ProducedFrame whose meta geometry matches.
    let recv = FakeNdiReceiver::with_frames(vec![
        ReceivedFrame::Video(uyvy_2x2()),
        ReceivedFrame::Video(uyvy_2x2()),
    ]);
    let mut producer = NdiProducer::new(Box::new(recv));
    let f0 = producer.next_frame().unwrap().expect("first frame");
    assert_eq!(f0.meta.width, 2);
    assert_eq!(f0.meta.height, 2);
    assert_eq!(f0.pixels.len(), 6, "NV12 2x2 is 4 (Y) + 2 (UV)");
    let f1 = producer.next_frame().unwrap();
    assert!(f1.is_some());
    // Drained: a fake with no more frames returns None (clean no-signal), never
    // blocks.
    let f2 = producer.next_frame().unwrap();
    assert!(f2.is_none());
}

#[test]
fn producer_skips_empty_polls_without_blocking() {
    // NDI receive returns "no frame this tick" frequently (None timeout). The
    // producer must surface that as Ok(None) — it never spins or blocks.
    let recv = FakeNdiReceiver::with_frames(vec![ReceivedFrame::None]);
    let mut producer = NdiProducer::new(Box::new(recv));
    assert!(producer.next_frame().unwrap().is_none());
}

#[test]
fn producer_drives_the_ingest_pump_into_last_good_store() {
    // The full IN-2 shape: NdiProducer → IngestPump → TileStore last-good slot.
    let recv = FakeNdiReceiver::with_frames(vec![
        ReceivedFrame::Video(uyvy_2x2()),
        ReceivedFrame::Video(uyvy_2x2()),
    ]);
    let mut producer = NdiProducer::new(Box::new(recv));
    let store: TileStore<multiview_input::source::StoredFrame> = TileStore::new(
        "ndi-test",
        TileThresholds::default(),
        NoSignalPolicy::HoldForever,
    );
    let mut pump = IngestPump::new(&producer, IngestConfig::default());
    let published = pump
        .run_to_end(&mut producer, &store, MediaTime::ZERO)
        .unwrap();
    assert_eq!(
        published, 2,
        "both sampled frames reach the last-good store"
    );
    let stored = store.read_at(MediaTime::from_nanos(i64::MAX));
    assert!(
        stored.frame().is_some(),
        "the store holds the last-good frame"
    );
}

#[test]
fn producer_timebase_is_ndi_100ns_and_wrap_is_none() {
    // NDI timecode is a 64-bit monotonic value in 100 ns units: timebase 1/1e7,
    // and no fixed-width wrap (WrapBits::None), so the normalizer passes it
    // through as a continuous timeline.
    let recv = FakeNdiReceiver::with_frames(vec![]);
    let producer = NdiProducer::new(Box::new(recv));
    let tb = producer.timebase();
    assert_eq!(tb.num, 1);
    assert_eq!(tb.den, 10_000_000);
    assert!(matches!(producer.wrap_bits(), WrapBits::None));
}
