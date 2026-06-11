//! GP-4 — the one-time pre-baked slate (the `ffmpeg` feature; ADR-0030 §4
//! "Pre-bake-once slate").
//!
//! ADR-0030's guarded passthrough splices a **pre-baked** slate into the copied
//! elementary stream on input loss, so failover costs **zero** live encode and
//! holds **zero** NVENC session. GP-4 is the baker: given the input's probed
//! coded params, it encodes **once** a short IDR-led, closed-GOP, B-free loop of
//! black / SMPTE-bars video (and, optionally, AAC tone / silence audio) and
//! returns the coded packets as a shared `Arc<[EncodedPacket]>`. After the bake
//! the encoder is released — no held session.
//!
//! These tests pin the GP-4 contract from the backlog row:
//!
//! * the bake produces **> 0** video packets;
//! * the **first** video packet is a keyframe / IDR (the closed-GOP anchor a
//!   downstream splice re-anchors on — ADR-0030 boundary 1);
//! * the cached slate is **small** (< 5 MB at 1080p — CRF/CQ-shaped, not
//!   CBR-padded);
//! * the bake is **deterministic** — a second bake of an identical spec yields
//!   the same packet count and per-packet sizes (it encodes once; the loop is
//!   replayed by offset, never re-encoded);
//! * the baked params **match** the requested params (same codec / width /
//!   height), so a downstream splice is parameter-compatible; and
//! * the optional audio bake produces AAC AUs with **>= 2 leading silence AUs**
//!   (ADR-0030 §4 audio seam) for both `Silence` and `Tone1k`.
//!
//! Licensing (LGPL-clean): the bake uses `mpeg2video` (an LGPL software codec in
//! `FFmpeg`) for video and the native libav `aac` (LGPL) for audio — never
//! x264/x265 (GPL). A separate `gpl-codecs`-gated test proves the strict-IDR
//! header path on a real H.264 bake; it never enters the default build.
#![cfg(feature = "ffmpeg")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use multiview_core::time::Rational;
use multiview_ffmpeg::EncodedPacket;
use multiview_output::slate::{
    BakedSlate, SlateAudio, SlateAudioSpec, SlateBaker, SlateKind, SlateSpec, SlateVideoCodec,
    SlateVideoSpec,
};

/// 1080p, the headline geometry the < 5 MB budget is asserted at.
const WIDTH: u32 = 1920;
const HEIGHT: u32 = 1080;

/// A 1080p black `mpeg2video` slate spec: 30 fps, a 30-frame (one second)
/// closed GOP, video-only. LGPL-clean (`mpeg2video`).
fn black_1080p_spec() -> SlateSpec {
    SlateSpec {
        kind: SlateKind::Black,
        video: SlateVideoSpec {
            codec: SlateVideoCodec::Mpeg2Video,
            width: WIDTH,
            height: HEIGHT,
            cadence: Rational::FPS_30,
            gop: 30,
        },
        audio: None,
    }
}

/// Total coded payload of every video packet in a baked slate, in bytes.
fn total_video_bytes(slate: &BakedSlate) -> usize {
    slate.video().iter().map(EncodedPacket::len).sum()
}

#[test]
fn bake_black_1080p_is_idr_led_small_and_param_matched() {
    let spec = black_1080p_spec();
    let slate = SlateBaker::bake_slate(&spec).expect("bake the 1080p black slate");

    // The bake produced coded video.
    assert!(
        !slate.video().is_empty(),
        "the slate bake produced no video packets"
    );

    // ADR-0030 boundary 1: the slate's FIRST video packet is its closed-GOP
    // keyframe / IDR — the splice re-anchor point.
    assert!(
        slate.video()[0].is_keyframe(),
        "the first slate video packet must be a keyframe (the closed-GOP IDR)"
    );

    // B-free (ADR-0030 §4): with no B-frames there is no decode reorder, so the
    // packet stream's DTS order equals its PTS order — every packet's DTS is
    // <= its own PTS and both sequences are strictly increasing in step. (A
    // B-pyramid would interleave DTS ahead of PTS and break this.)
    let pts: Vec<i64> = slate
        .video()
        .iter()
        .map(|p| p.pts().expect("slate video packet carries a PTS"))
        .collect();
    let dts: Vec<i64> = slate
        .video()
        .iter()
        .map(|p| p.dts().expect("slate video packet carries a DTS"))
        .collect();
    assert!(
        pts.windows(2).all(|w| w[1] > w[0]),
        "slate PTS must strictly increase (no reorder), got {pts:?}"
    );
    assert!(
        dts.windows(2).all(|w| w[1] > w[0]),
        "slate DTS must strictly increase (B-free, no reorder), got {dts:?}"
    );
    assert!(
        pts.iter().zip(&dts).all(|(p, d)| d <= p),
        "slate DTS must never exceed PTS (B-free), pts={pts:?} dts={dts:?}"
    );

    // The cached slate is small: CRF/CQ-shaped, well under 5 MB even at 1080p.
    // A CBR-padded bake of a multi-Mbps source would breach this (ADR-0030 §4).
    let bytes = total_video_bytes(&slate);
    assert!(
        bytes < 5 * 1024 * 1024,
        "slate video must be < 5 MB, got {bytes} bytes"
    );

    // The baked params match the requested params, so a downstream splice is
    // parameter-compatible (same codec / geometry).
    assert_eq!(
        slate.params().width,
        WIDTH,
        "baked width must match request"
    );
    assert_eq!(
        slate.params().height,
        HEIGHT,
        "baked height must match request"
    );
    assert_eq!(
        slate.params().codec,
        SlateVideoCodec::Mpeg2Video,
        "baked codec must match request"
    );

    // Video-only spec ⇒ no audio packets.
    assert!(
        slate.audio().is_none(),
        "a video-only spec must bake no audio"
    );
}

#[test]
fn bake_is_deterministic_for_an_identical_spec() {
    // GP-4 encodes ONCE: the loop is replayed downstream by offset, never
    // re-encoded. Two bakes of the same spec must therefore agree on packet
    // count and per-packet sizes (the cached bytes are stable).
    let spec = black_1080p_spec();
    let a = SlateBaker::bake_slate(&spec).expect("first bake");
    let b = SlateBaker::bake_slate(&spec).expect("second bake");

    assert_eq!(
        a.video().len(),
        b.video().len(),
        "two bakes of one spec must yield the same video packet count"
    );
    let sizes_a: Vec<usize> = a.video().iter().map(EncodedPacket::len).collect();
    let sizes_b: Vec<usize> = b.video().iter().map(EncodedPacket::len).collect();
    assert_eq!(
        sizes_a, sizes_b,
        "two bakes of one spec must yield identical per-packet sizes"
    );
}

#[test]
fn smpte_bars_bakes_a_distinct_idr_led_slate() {
    // The bars generator is a separate code path from black; it must still
    // produce an IDR-led, > 0-packet, < 5 MB slate.
    let mut spec = black_1080p_spec();
    spec.kind = SlateKind::SmpteBars;
    let slate = SlateBaker::bake_slate(&spec).expect("bake the SMPTE-bars slate");

    assert!(!slate.video().is_empty(), "bars slate produced no video");
    assert!(
        slate.video()[0].is_keyframe(),
        "the first bars-slate video packet must be a keyframe"
    );
    let bytes = total_video_bytes(&slate);
    assert!(
        bytes < 5 * 1024 * 1024,
        "bars slate must be < 5 MB, got {bytes}"
    );
}

#[test]
fn audio_bake_emits_aac_aus_with_leading_silence() {
    // ADR-0030 §4 audio seam: the slate's audio side opens with >= 2 leading
    // silence AUs (the coded-domain AAC IMDCT/TDAC transient is uncancellable),
    // for both Silence and Tone1k.
    for audio in [SlateAudio::Silence, SlateAudio::Tone1k] {
        let mut spec = black_1080p_spec();
        spec.audio = Some(SlateAudioSpec {
            sample_rate: 48_000,
            channels: 2,
            audio,
        });
        let slate = SlateBaker::bake_slate(&spec).expect("bake slate with audio");

        let aus = slate
            .audio()
            .unwrap_or_else(|| panic!("audio spec must bake audio packets ({audio:?})"));
        assert!(
            aus.len() >= 2,
            "audio slate must carry >= 2 leading silence AUs, got {} ({audio:?})",
            aus.len()
        );
        // Every audio AU carries coded payload (a real AAC frame, not empty).
        assert!(
            aus.iter().all(|p| !p.is_empty()),
            "every baked audio AU must carry coded payload ({audio:?})"
        );
    }
}
