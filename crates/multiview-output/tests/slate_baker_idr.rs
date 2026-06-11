//! GP-4 strict-IDR proof on a real H.264 bake (the `gpl-codecs` feature).
//!
//! The default GP-4 test (`slate_baker.rs`) bakes `mpeg2video` and proves the
//! anchor is a keyframe via `EncodedPacket::is_keyframe`. `mpeg2video` is not a
//! NAL codec, so GP-1's strict-IDR header classifier (`multiview_ffmpeg::is_idr`)
//! does not apply to it. This test bakes a real **H.264** slate and verifies the
//! first VIDEO access unit is a strict IDR through that very classifier — the
//! exact gate ADR-0030 boundary 2 re-anchors on (distinct from
//! `AV_PKT_FLAG_KEY`, which also flags CRA / recovery-point I-frames).
//!
//! H.264's only software encoder in this build is `libx264`, which is GPL, so
//! this test lives behind the `gpl-codecs` feature and **never** enters the
//! default LGPL-clean build (`#![cfg(feature = "gpl-codecs")]`).
#![cfg(feature = "gpl-codecs")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use multiview_core::time::Rational;
use multiview_ffmpeg::idr::{is_idr, CodecKind, NalFraming};
use multiview_output::slate::{SlateBaker, SlateKind, SlateSpec, SlateVideoCodec, SlateVideoSpec};

#[test]
fn h264_slate_first_au_is_a_strict_idr() {
    let spec = SlateSpec {
        kind: SlateKind::Black,
        video: SlateVideoSpec {
            codec: SlateVideoCodec::H264,
            width: 1280,
            height: 720,
            cadence: Rational::FPS_30,
            gop: 30,
        },
        audio: None,
    };
    let slate = SlateBaker::bake_slate(&spec).expect("bake the H.264 slate");

    assert!(!slate.video().is_empty(), "H.264 bake produced no video");

    // The first AU's coded bytes must classify as a STRICT IDR (H.264 nal==5),
    // not merely AV_PKT_FLAG_KEY. libx264 emits Annex-B start codes by default.
    let first = &slate.video()[0];
    let bytes = first
        .to_owned_packet()
        .data()
        .map(<[u8]>::to_vec)
        .expect("first slate AU carries coded bytes");
    assert!(
        is_idr(&bytes, CodecKind::H264, NalFraming::AnnexB),
        "the first H.264 slate AU must be a strict IDR (nal==5)"
    );
}
