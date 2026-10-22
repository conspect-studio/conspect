//! VP8 preview-rung tests (ADR-P006): codec identity (pure) + the real libvpx
//! encode path (gated on the `ffmpeg` feature).
//!
//! VP8 via `libvpx` (BSD, licence-clean) is the always-allowed software encoder
//! rung for WHEP preview: every browser must receive it (RFC 7742) and the
//! LGPL-clean default build may link it. Whether the **linked** `FFmpeg`
//! actually ships `libvpx` is a run-time fact, so the gated tests follow the
//! crate's availability-probe pattern (`select_encoder` / typed fallback, the
//! same honest contract as the NVENC and JPEG XS probes) — no silent skips:
//! both the present and the absent build assert their full contract.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use multiview_ffmpeg::{can_encode, candidate_encoders, VideoCodec};

#[test]
fn vp8_resolves_to_libvpx_in_the_default_build() {
    // ADR-P006: `VideoCodec` gains `Vp8`, whose licence-clean software encoder
    // is libvpx — available to the default build (BSD, no `gpl-codecs` needed).
    assert_eq!(VideoCodec::Vp8.lgpl_software_encoder(), Some("libvpx"));
    assert_eq!(VideoCodec::Vp8.gpl_software_encoder(), None);
    assert!(can_encode(VideoCodec::Vp8));

    let list = candidate_encoders(VideoCodec::Vp8);
    assert_eq!(
        list.last(),
        Some(&"libvpx"),
        "libvpx is the final (software) candidate"
    );
}

#[cfg(not(feature = "cuda"))]
#[test]
fn vp8_has_no_hardware_candidate_without_cuda() {
    assert_eq!(VideoCodec::Vp8.nvenc_encoder(), None);
    assert_eq!(candidate_encoders(VideoCodec::Vp8), vec!["libvpx"]);
}

#[cfg(feature = "ffmpeg")]
mod gated {
    use ffmpeg::format::Pixel;
    use ffmpeg::util::frame::Video;
    use ffmpeg_next as ffmpeg;
    use multiview_core::time::Rational;
    use multiview_ffmpeg::{
        preview_vp8_options, select_encoder, VideoCodec, VideoEncodeTarget, VideoEncoder,
    };

    const W: u32 = 320;
    const H: u32 = 240;
    const FPS: i64 = 15;

    fn gray_yuv420p(pts: i64) -> Video {
        let mut frame = Video::new(Pixel::YUV420P, W, H);
        for p in 0..frame.planes() {
            for byte in frame.data_mut(p).iter_mut() {
                *byte = 128;
            }
        }
        frame.set_pts(Some(pts));
        frame
    }

    fn vp8_target() -> VideoEncodeTarget {
        VideoEncodeTarget {
            codec_name: "libvpx".to_owned(),
            width: W,
            height: H,
            format: Pixel::YUV420P,
            time_base: Rational::new(1, FPS),
            bit_rate: 200_000,
            // GOP comes from the preview option set (`g`), not the target.
            gop: 0,
            cuda_device: None,
        }
    }

    /// Encode `frames` gray frames through `enc`, forcing a keyframe before the
    /// frame whose pts is `force_at` (if any), and return `(pts, is_key)` for
    /// every emitted packet (including the EOF drain).
    fn encode_collect(
        enc: &mut VideoEncoder,
        frames: i64,
        force_at: Option<i64>,
    ) -> Vec<(i64, bool)> {
        let mut out = Vec::new();
        for tick in 0..frames {
            if force_at == Some(tick) {
                enc.force_next_keyframe();
            }
            enc.send_frame(&gray_yuv420p(tick)).expect("send frame");
            while let Some(pkt) = enc.receive_packet().expect("recv") {
                out.push((pkt.pts().expect("vp8 packet pts"), pkt.is_key()));
            }
        }
        enc.send_eof().expect("eof");
        while let Some(pkt) = enc.receive_packet().expect("drain") {
            out.push((pkt.pts().expect("vp8 packet pts"), pkt.is_key()));
        }
        out
    }

    /// The availability-probe gate: on a build whose linked `FFmpeg` lacks
    /// libvpx, selection must report it (None) and the open must be a typed
    /// error — the same graceful-degradation contract as the NVENC probe.
    /// Returns `true` when libvpx is usable so the real-encode tests can run.
    fn libvpx_available() -> bool {
        let selected = select_encoder(VideoCodec::Vp8);
        let present = ffmpeg::encoder::find_by_name("libvpx").is_some();
        assert_eq!(
            selected.is_some(),
            present,
            "select_encoder must track the linked FFmpeg's libvpx registry"
        );
        if !present {
            match VideoEncoder::new_with_options(
                &vp8_target(),
                &preview_vp8_options(Rational::new(FPS, 1)),
            ) {
                Ok(_) => panic!("opening libvpx must fail when the build lacks it"),
                Err(err) => assert!(
                    err.to_string().contains("not found"),
                    "typed CodecNotFound, got: {err}"
                ),
            }
        }
        present
    }

    #[test]
    fn preview_vp8_encodes_realtime_with_2s_gop() {
        if !libvpx_available() {
            return; // the absent-build contract was fully asserted above
        }
        let options = preview_vp8_options(Rational::new(FPS, 1));
        let mut enc = VideoEncoder::new_with_options(&vp8_target(), &options).expect("open libvpx");
        assert_eq!(enc.time_base(), Rational::new(1, FPS));

        // 40 frames at 15 fps with a 2 s GOP (g=30): realtime deadline +
        // lag-in-frames=0 means 1-in-1-out, keyframes at pts 0 and 30 only.
        let packets = encode_collect(&mut enc, 40, None);
        assert_eq!(packets.len(), 40, "lag-free realtime encode is 1-in-1-out");
        let keys: Vec<i64> = packets
            .iter()
            .filter_map(|&(pts, key)| key.then_some(pts))
            .collect();
        assert!(keys.contains(&0), "stream starts with a keyframe");
        assert!(keys.contains(&30), "2 s GOP places a keyframe at pts 30");
        assert!(
            !packets.iter().any(|&(pts, key)| key && pts > 0 && pts < 30),
            "no stray keyframe inside the GOP: {packets:?}"
        );
    }

    #[test]
    fn force_next_keyframe_forces_a_vp8_keyframe_mid_gop() {
        if !libvpx_available() {
            return;
        }
        let options = preview_vp8_options(Rational::new(FPS, 1));
        let mut enc = VideoEncoder::new_with_options(&vp8_target(), &options).expect("open libvpx");

        // Force before frame 10 (mid-GOP). The PLI -> force-IDR seam of
        // ADR-0049/ADR-P006: the NEXT encoded frame must be a keyframe.
        let packets = encode_collect(&mut enc, 21, Some(10));
        let key_at = |pts: i64| {
            packets.iter().find(|&&(p, _)| p == pts).map_or_else(
                || panic!("no packet with pts {pts}: {packets:?}"),
                |&(_, k)| k,
            )
        };
        assert!(key_at(0), "first frame is a keyframe");
        assert!(key_at(10), "forced frame must be a keyframe");
        for pts in (1..10).chain(11..=20) {
            assert!(
                !key_at(pts),
                "only the forced frame may be a mid-GOP keyframe (pts {pts}): {packets:?}"
            );
        }
    }
}
