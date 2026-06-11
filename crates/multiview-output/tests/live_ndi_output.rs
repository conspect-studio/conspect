//! Live end-to-end NDI **output** test (NDI-L1 / OUT-4): drive the real
//! [`SdkNdiApi`] through the safe [`NdiOutput`] sink — accept the license, create a
//! sender, and publish a sequence of NV12 canvases (each converted NV12→UYVY at
//! the host-copy boundary and sent through `NDIlib_send_send_video_v2`). This
//! proves the whole output path against the real licensed SDK: license gate →
//! `SdkNdiApi` → `NdiSender` → SDK.
//!
//! `#[cfg(feature = "ndi-bindings")]` — needs the build-time `bindgen` over the
//! licensed SDK header. `#[ignore]` — needs a resolvable NDI runtime at run time.
//! Run on the SDK-equipped box:
//!
//! ```text
//! cargo test -p multiview-output --features ndi-bindings --test live_ndi_output -- --ignored --nocapture
//! ```
#![cfg(feature = "ndi-bindings")]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use multiview_output::ndi::license::LicenseAcceptance;
use multiview_output::ndi::{NdiCapability, NdiLicense, NdiOutput, Nv12Canvas, SdkNdiApi};

#[test]
#[ignore = "requires a resolvable NDI runtime (libndi_advanced.so.6 / libndi.so.6)"]
fn live_ndi_output_publishes_nv12_canvases() {
    let capability = NdiCapability::load().expect("an NDI runtime should load on this host");
    let license = NdiLicense::accept(LicenseAcceptance {
        accepted_by: "live-test".to_owned(),
        accepted_at: "2026-06-07T00:00:00Z".to_owned(),
    })
    .expect("a complete acceptance record yields an accepted license");

    let mut output = NdiOutput::new(
        license,
        SdkNdiApi::new(capability),
        "Multiview NDI-L1 Output",
    )
    .expect("the sender should be created");
    assert!(output.is_open());
    assert_eq!(output.name(), "Multiview NDI-L1 Output");

    // A small NV12 canvas: Y = w*h, interleaved UV = w*h/2.
    let (w, h) = (64u32, 64u32);
    let y_plane: Vec<u8> = (0..w * h).map(|i| u8::try_from(i % 256).unwrap()).collect();
    let uv_plane: Vec<u8> = (0..w * h / 2)
        .map(|i| u8::try_from((i % 128) + 64).unwrap())
        .collect();
    let canvas = Nv12Canvas::new(w, h, &y_plane, &uv_plane).expect("valid NV12 geometry");

    // 10 ms of planar-float (FLTP) program audio: 48 kHz, 2 ch, 480 samples.
    let audio = vec![0.5f32; 2 * 480];

    // Publish 30 frames at 30000/1001 with tick-derived timecodes (inv #3),
    // interleaving video + audio through the same NDI source.
    for tick in 0..30i64 {
        let tc = tick * 33_366;
        output
            .send_canvas(&canvas, tc, 30_000, 1_001)
            .expect("each NV12 canvas should convert + send");
        output
            .send_audio_planar(48_000, 2, 480, tc, &audio)
            .expect("each audio chunk should send");
    }
    println!("published 30 NV12 canvases + FLTP audio through the live NDI output ({w}x{h})");

    output.close();
    assert!(!output.is_open());
    // A send after close is a typed refusal, never a panic (inv #1).
    assert!(output.send_canvas(&canvas, 0, 30_000, 1_001).is_err());
}
