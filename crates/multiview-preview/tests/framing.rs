//! MJPEG `multipart/x-mixed-replace` framing model + snapshot data model.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use multiview_preview::{
    JpegEncoder, JpegError, MjpegStream, Snapshot, StubJpegEncoder, ThumbnailPlan,
};

#[test]
fn mjpeg_content_type_carries_the_boundary() {
    let stream = MjpegStream::new();
    let ct = stream.content_type();
    assert!(
        ct.starts_with("multipart/x-mixed-replace; boundary="),
        "content type must declare the multipart boundary: {ct}"
    );
    // The declared boundary token must be the one the parts actually use.
    let declared = ct
        .split("boundary=")
        .nth(1)
        .expect("boundary token present");
    assert_eq!(declared, stream.boundary());
    assert!(!stream.boundary().is_empty());
}

#[test]
fn mjpeg_part_framing_is_well_formed() {
    let stream = MjpegStream::new();
    let jpeg = vec![0xFFu8, 0xD8, 0x00, 0x01, 0x02, 0xFF, 0xD9];
    let part = stream.frame_part(&jpeg);

    let text = String::from_utf8_lossy(&part);
    // Each part opens with the boundary delimiter, declares the JPEG content
    // type and its length, then a blank line, then the raw bytes, then CRLF.
    assert!(text.starts_with(&format!("\r\n--{}\r\n", stream.boundary())));
    assert!(text.contains("Content-Type: image/jpeg\r\n"));
    assert!(text.contains(&format!("Content-Length: {}\r\n", jpeg.len())));
    // Header/body separator and a trailing CRLF after the payload.
    assert!(part.ends_with(b"\r\n"));

    // The exact JPEG bytes survive byte-for-byte inside the part.
    let needle = jpeg.as_slice();
    assert!(
        part.windows(needle.len()).any(|w| w == needle),
        "the raw JPEG payload must appear verbatim in the part"
    );
    // The declared Content-Length matches the real payload size.
    assert!(text.contains(&format!("Content-Length: {}", needle.len())));
}

#[test]
fn snapshot_carries_dimensions_and_bytes() {
    let snap = Snapshot::new(320, 180, vec![0xFF, 0xD8, 0xFF, 0xD9]);
    assert_eq!(snap.width(), 320);
    assert_eq!(snap.height(), 180);
    assert_eq!(snap.content_type(), "image/jpeg");
    assert_eq!(snap.bytes(), &[0xFF, 0xD8, 0xFF, 0xD9]);
    // A stable ETag derived from the bytes lets the HTTP layer do conditional
    // GETs (the brief's grid-thumbnail polling path).
    assert!(!snap.etag().is_empty());
    let same = Snapshot::new(320, 180, vec![0xFF, 0xD8, 0xFF, 0xD9]);
    assert_eq!(
        snap.etag(),
        same.etag(),
        "identical bytes => identical ETag"
    );
    let other = Snapshot::new(320, 180, vec![0xFF, 0xD8, 0x00, 0xD9]);
    assert_ne!(
        snap.etag(),
        other.etag(),
        "different bytes => different ETag"
    );
}

#[test]
fn thumbnail_plan_clamps_fps_and_dimension() {
    // Requested 999 fps / 4096px is clamped to the preview caps (cheap-by-design).
    let plan = ThumbnailPlan::clamped(999, 4096);
    assert!(plan.fps() >= 1 && plan.fps() <= ThumbnailPlan::MAX_FPS);
    assert!(plan.max_dim() <= ThumbnailPlan::MAX_DIM);
    // A zero request is floored to the minimum, never zero (avoid div-by-zero
    // pacing and degenerate sizing).
    let floored = ThumbnailPlan::clamped(0, 0);
    assert!(floored.fps() >= 1);
    assert!(floored.max_dim() >= 1);
}

#[test]
fn stub_jpeg_encoder_is_a_total_function() {
    // The default encoder is a documented stub: it returns a syntactically valid
    // minimal JPEG (SOI..EOI) rather than panicking or doing real DCT work, so
    // the framing/transport layer is testable without a native codec.
    let enc = StubJpegEncoder;
    let out = enc
        .encode_nv12(&[0u8; 16], 4, 2, 80)
        .expect("stub encodes any well-formed NV12 plane");
    assert_eq!(&out[..2], &[0xFF, 0xD8], "JPEG SOI marker");
    assert_eq!(&out[out.len() - 2..], &[0xFF, 0xD9], "JPEG EOI marker");
}

#[test]
fn stub_jpeg_encoder_rejects_inconsistent_buffer() {
    let enc = StubJpegEncoder;
    // NV12 needs width*height*3/2 bytes; a short buffer must be a typed error,
    // never a panic / out-of-bounds index.
    let err = enc.encode_nv12(&[0u8; 3], 4, 2, 80).unwrap_err();
    assert!(
        matches!(err, JpegError::BufferTooSmall { .. }),
        "got {err:?}"
    );
    // Odd dimensions are invalid for NV12 (2x2 chroma subsampling).
    assert!(matches!(
        enc.encode_nv12(&[0u8; 100], 3, 2, 80),
        Err(JpegError::OddDimensions { .. })
    ));
    // Quality must be 1..=100.
    assert!(matches!(
        enc.encode_nv12(&[0u8; 24], 4, 2, 0),
        Err(JpegError::InvalidQuality(0))
    ));
}
