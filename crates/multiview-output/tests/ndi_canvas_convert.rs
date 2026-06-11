//! OUT-4: the pure canvas→NDI host-copy seam — NV12 (4:2:0 semi-planar) canvas →
//! UYVY (4:2:2 packed) host buffer + the `NdiVideoFrame` send-descriptor.
//!
//! All of this is pure, SDK-free logic (geometry/stride/FourCC mapping + the
//! line-stride pack), so it runs in CI under the `ndi` feature without any
//! proprietary runtime. The actual send to a real receiver stays in `ndi_live.rs`.
#![cfg(feature = "ndi")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use multiview_output::ndi::convert::{nv12_to_uyvy, Nv12Canvas};
use multiview_output::ndi::license::LicenseAcceptance;
use multiview_output::ndi::{FakeNdiApi, NdiFourCc, NdiLicense, NdiOutput, NdiSendError};

fn accepted() -> NdiLicense {
    NdiLicense::accept(LicenseAcceptance {
        accepted_by: "ops".to_owned(),
        accepted_at: "2026-06-06T00:00:00Z".to_owned(),
    })
    .unwrap()
}

/// A tiny 4x2 NV12 canvas with a horizontal luma ramp and a constant chroma so
/// the conversion is exactly predictable. Y plane is `w*h` bytes (tightly packed,
/// stride == width); UV plane is `w*h/2` interleaved Cb,Cr (one pair per 2x2).
fn ramp_canvas() -> (u32, u32, Vec<u8>, Vec<u8>) {
    let (w, h) = (4u32, 2u32);
    // Y: row-major, value = 10*x + row marker so we can prove ordering.
    let y = vec![
        0, 10, 20, 30, // row 0
        40, 50, 60, 70, // row 1
    ];
    // UV: 2 chroma columns x 1 chroma row = 2 pairs => 4 bytes. (Cb=100, Cr=200)
    let uv = vec![100, 200, 100, 200];
    (w, h, y, uv)
}

#[test]
fn canvas_validates_plane_lengths_and_even_dims() {
    let (w, h, y, uv) = ramp_canvas();
    // Good geometry constructs.
    assert!(Nv12Canvas::new(w, h, &y, &uv).is_ok());

    // Odd width is refused with a typed error (never a panic).
    let err = Nv12Canvas::new(3, 2, &y, &uv).expect_err("odd width refused");
    assert!(matches!(err, NdiSendError::InvalidFrame { .. }));

    // A short Y plane is refused.
    let short_y = vec![0u8; 4];
    let err = Nv12Canvas::new(w, h, &short_y, &uv).expect_err("short Y refused");
    assert!(matches!(err, NdiSendError::InvalidFrame { .. }));

    // A short UV plane is refused.
    let short_uv = vec![0u8; 2];
    let err = Nv12Canvas::new(w, h, &y, &short_uv).expect_err("short UV refused");
    assert!(matches!(err, NdiSendError::InvalidFrame { .. }));
}

#[test]
fn nv12_to_uyvy_packs_in_u_y0_v_y1_order_with_correct_geometry() {
    let (w, h, y, uv) = ramp_canvas();
    let canvas = Nv12Canvas::new(w, h, &y, &uv).expect("valid canvas");
    let uyvy = nv12_to_uyvy(&canvas);

    // UYVY is 2 bytes/pixel: stride = width*2, total = width*2*height.
    let (wz, hz) = (usize::try_from(w).unwrap(), usize::try_from(h).unwrap());
    assert_eq!(uyvy.len(), wz * 2 * hz);

    // Row 0 covers Y = [0,10,20,30] with the single chroma pair (Cb=100, Cr=200)
    // replicated across the two 2-pixel groups: U,Y0,V,Y1, U,Y2,V,Y3.
    assert_eq!(&uyvy[0..8], &[100, 0, 200, 10, 100, 20, 200, 30]);
    // Row 1 (vertical chroma replication into the 2nd luma row, same chroma row):
    // Y = [40,50,60,70].
    assert_eq!(&uyvy[8..16], &[100, 40, 200, 50, 100, 60, 200, 70]);
}

#[test]
fn nv12_to_uyvy_advances_chroma_rows_for_a_multi_chroma_row_canvas() {
    // A 4x4 canvas has TWO chroma rows (rows 0-1 share chroma row 0; rows 2-3
    // share chroma row 1). Distinct chroma per row proves the chroma-row-stride
    // math (`chroma_row_base = (row/2) * (width/2) * 2`) advances — a single
    // chroma row (the 4x2 case) cannot catch a chroma-row regression.
    let (w, h) = (4u32, 4u32);
    // Y: a unique value per pixel so luma ordering is unambiguous.
    let y: Vec<u8> = (0u8..16).collect();
    // UV: chroma row 0 = (Cb=10, Cr=20); chroma row 1 = (Cb=30, Cr=40); two pairs
    // per chroma row (width/2). 2 chroma rows x 2 pairs x 2 bytes = 8 = w*h/2.
    let uv = vec![10, 20, 10, 20, 30, 40, 30, 40];
    let canvas = Nv12Canvas::new(w, h, &y, &uv).expect("valid 4x4 canvas");
    let uyvy = nv12_to_uyvy(&canvas);
    assert_eq!(uyvy.len(), 4 * 2 * 4);

    // Rows 0 and 1 use chroma row 0 (Cb=10, Cr=20).
    assert_eq!(
        &uyvy[0..8],
        &[10, 0, 20, 1, 10, 2, 20, 3],
        "row 0 / chroma row 0"
    );
    assert_eq!(
        &uyvy[8..16],
        &[10, 4, 20, 5, 10, 6, 20, 7],
        "row 1 / chroma row 0"
    );
    // Rows 2 and 3 MUST use chroma row 1 (Cb=30, Cr=40) — the stride advanced.
    assert_eq!(
        &uyvy[16..24],
        &[30, 8, 40, 9, 30, 10, 40, 11],
        "row 2 must use the SECOND chroma row"
    );
    assert_eq!(
        &uyvy[24..32],
        &[30, 12, 40, 13, 30, 14, 40, 15],
        "row 3 must use the SECOND chroma row"
    );
}

#[test]
fn to_uyvy_frame_builds_a_valid_send_descriptor() {
    let (w, h, y, uv) = ramp_canvas();
    let canvas = Nv12Canvas::new(w, h, &y, &uv).expect("valid canvas");
    let uyvy = nv12_to_uyvy(&canvas);

    let frame = canvas
        .to_uyvy_frame(333_667, 30_000, 1001, &uyvy)
        .expect("descriptor builds");

    assert_eq!(frame.width, w);
    assert_eq!(frame.height, h);
    assert_eq!(frame.fourcc, NdiFourCc::Uyvy);
    assert_eq!(frame.stride, w * 2);
    assert_eq!(frame.frame_rate_n, 30_000);
    assert_eq!(frame.frame_rate_d, 1001);
    assert_eq!(frame.timecode, 333_667);
    // The descriptor must already pass the SDK-foot-gun validation.
    frame
        .validate()
        .expect("descriptor is internally consistent");
}

#[test]
fn to_uyvy_frame_rejects_a_mismatched_buffer() {
    let (w, h, y, uv) = ramp_canvas();
    let canvas = Nv12Canvas::new(w, h, &y, &uv).expect("valid canvas");
    // A buffer one row short of the packed UYVY size is refused, not silently sent.
    let (wz, hz) = (usize::try_from(w).unwrap(), usize::try_from(h).unwrap());
    let short = vec![0u8; wz * 2 * hz - 1];
    let err = canvas
        .to_uyvy_frame(0, 30, 1, &short)
        .expect_err("short UYVY buffer refused");
    assert!(matches!(err, NdiSendError::InvalidFrame { .. }));
}

#[test]
fn to_uyvy_frame_rejects_a_zero_frame_rate_denominator() {
    let (w, h, y, uv) = ramp_canvas();
    let canvas = Nv12Canvas::new(w, h, &y, &uv).expect("valid canvas");
    let uyvy = nv12_to_uyvy(&canvas);
    let err = canvas
        .to_uyvy_frame(0, 30, 0, &uyvy)
        .expect_err("zero fps denominator refused");
    assert!(matches!(err, NdiSendError::InvalidFrame { .. }));
}

#[test]
fn send_canvas_publishes_the_converted_frame_with_tick_timecode() {
    let mut out = NdiOutput::new(accepted(), FakeNdiApi::new(), "MV").unwrap();
    let (w, h, y, uv) = ramp_canvas();
    let canvas = Nv12Canvas::new(w, h, &y, &uv).expect("valid canvas");
    // Three ticks; timecode derived from the tick counter (invariant #3).
    for tick in 0..3i64 {
        let tc = tick * 333_667;
        out.send_canvas(&canvas, tc, 30_000, 1001)
            .expect("canvas send ok");
    }
    let sent = &out.api().sent;
    assert_eq!(sent.len(), 3);
    // Each recorded frame carries the canvas geometry + UYVY FourCC + the exact
    // tick-derived timecode, in order.
    assert!(sent
        .iter()
        .all(|s| s.0 == w && s.1 == h && s.2 == NdiFourCc::Uyvy));
    assert_eq!(
        sent.iter().map(|s| s.3).collect::<Vec<_>>(),
        vec![0, 333_667, 667_334]
    );
}

#[test]
fn send_canvas_after_close_is_a_typed_closed_error() {
    let mut out = NdiOutput::new(accepted(), FakeNdiApi::new(), "MV").unwrap();
    let (w, h, y, uv) = ramp_canvas();
    let canvas = Nv12Canvas::new(w, h, &y, &uv).expect("valid canvas");
    out.close();
    let err = out
        .send_canvas(&canvas, 0, 30, 1)
        .expect_err("closed sender refuses a canvas send");
    assert!(matches!(err, NdiSendError::Closed));
    assert!(out.api().sent.is_empty());
}
