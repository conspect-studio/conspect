//! OUT-1 — RTSP egress via the ADR-0006 **sidecar baseline**: publish the
//! already-encoded program to a listening RTSP endpoint (e.g. `MediaMTX`) over the
//! existing `PushProtocol::Rtsp` push path.
//!
//! The decision OUT-1 locks: an in-process `gst-rtsp-server` is OUT-2's job; the
//! immediate, native-light RTSP egress is the **publish hop** — libav RTSP
//! ANNOUNCE/RECORD to a sidecar — which needs **zero new sink code**, only a typed
//! way to derive the publish URL from a configured base + mount. This test pins
//! that pure seam (`RtspPublishTarget`): base/mount validation and checked URL
//! construction, always compiled and always run in CI (no network, no `ffmpeg`).
//!
//! The genuine network push (a real datagram round-trip to a `MediaMTX` listener)
//! is `#[ignore]`d-with-reason below: CI has no sidecar. That mirrors how
//! `push_and_gpl.rs` cannot complete a live RTMP/RTSP push in CI.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use multiview_output::rtsp::{RtspPublishError, RtspPublishTarget};

#[test]
fn target_builds_a_publish_url_from_base_and_mount() {
    // The MediaMTX default base + a multiview mount → the libav RTSP publish URL.
    let target = RtspPublishTarget::new("rtsp://127.0.0.1:8554", "program").unwrap();
    assert_eq!(target.publish_url(), "rtsp://127.0.0.1:8554/program");
    assert_eq!(target.mount(), "program");
}

#[test]
fn default_base_is_the_mediamtx_loopback() {
    // ADR-0006 sidecar baseline: the default peer is a local MediaMTX on 8554.
    // IPv6-first (operator directive): the default loopback is `[::1]`, not v4.
    assert_eq!(RtspPublishTarget::DEFAULT_BASE, "rtsp://[::1]:8554");
    let target = RtspPublishTarget::with_default_base("cam1").unwrap();
    assert_eq!(target.publish_url(), "rtsp://[::1]:8554/cam1");
}

#[test]
fn an_ipv6_bracketed_base_builds_a_publish_url() {
    // A bracketed IPv6 authority (`[host]:port`) must build cleanly: the inner
    // `:` of the address is not a path separator, so the mount still appends once.
    let target = RtspPublishTarget::new("rtsp://[::1]:8554", "program").unwrap();
    assert_eq!(target.publish_url(), "rtsp://[::1]:8554/program");
    assert_eq!(target.mount(), "program");

    // A global IPv6 literal works the same way.
    let g = RtspPublishTarget::new("rtsp://[2001:db8::1]:8554", "studio/program").unwrap();
    assert_eq!(g.publish_url(), "rtsp://[2001:db8::1]:8554/studio/program");
}

#[test]
fn a_trailing_slash_on_the_base_does_not_double_up() {
    // The builder must join base+mount with exactly one separator regardless of a
    // trailing slash on the base or a leading slash on the mount.
    let a = RtspPublishTarget::new("rtsp://host:8554/", "live").unwrap();
    let b = RtspPublishTarget::new("rtsp://host:8554", "/live").unwrap();
    let c = RtspPublishTarget::new("rtsp://host:8554/", "/live").unwrap();
    assert_eq!(a.publish_url(), "rtsp://host:8554/live");
    assert_eq!(b.publish_url(), "rtsp://host:8554/live");
    assert_eq!(c.publish_url(), "rtsp://host:8554/live");
}

#[test]
fn a_nested_mount_path_is_preserved() {
    // Mounts may be multi-segment (MediaMTX paths) — interior slashes survive.
    let target = RtspPublishTarget::new("rtsp://127.0.0.1:8554", "studio/program").unwrap();
    assert_eq!(target.publish_url(), "rtsp://127.0.0.1:8554/studio/program");
    assert_eq!(target.mount(), "studio/program");
}

#[test]
fn a_non_rtsp_base_scheme_is_rejected() {
    // The push protocol is fixed to RTSP; an http/rtmp base is a config error, not
    // a silently-wrong URL.
    match RtspPublishTarget::new("http://127.0.0.1:8554", "program") {
        Err(RtspPublishError::NotRtspScheme { base }) => {
            assert_eq!(base, "http://127.0.0.1:8554");
        }
        other => panic!("expected NotRtspScheme, got {other:?}"),
    }
}

#[test]
fn an_rtsps_base_scheme_is_accepted() {
    // TLS RTSP (rtsps://) is still an RTSP publish target.
    let target = RtspPublishTarget::new("rtsps://127.0.0.1:8555", "secure").unwrap();
    assert_eq!(target.publish_url(), "rtsps://127.0.0.1:8555/secure");
}

#[test]
fn an_empty_base_is_rejected() {
    match RtspPublishTarget::new("", "program") {
        Err(RtspPublishError::EmptyBase) => {}
        other => panic!("expected EmptyBase, got {other:?}"),
    }
}

#[test]
fn an_empty_mount_is_rejected() {
    // An empty mount (or one that is only slashes) has no RTSP path to publish to.
    match RtspPublishTarget::new("rtsp://127.0.0.1:8554", "") {
        Err(RtspPublishError::EmptyMount) => {}
        other => panic!("expected EmptyMount, got {other:?}"),
    }
    match RtspPublishTarget::new("rtsp://127.0.0.1:8554", "///") {
        Err(RtspPublishError::EmptyMount) => {}
        other => panic!("expected EmptyMount for an all-slash mount, got {other:?}"),
    }
}

#[test]
fn a_scheme_only_base_with_no_authority_is_rejected() {
    // A bare `rtsp://` has the scheme but no host:port; joining a mount onto it
    // would yield a host-less `rtsp:/program` URL, so the typed builder rejects it
    // rather than handing a silently-wrong target to libav.
    match RtspPublishTarget::new("rtsp://", "program") {
        Err(RtspPublishError::MissingAuthority { base }) => {
            assert_eq!(base, "rtsp://");
        }
        other => panic!("expected MissingAuthority, got {other:?}"),
    }
    // A scheme followed only by a slash is equally authority-less.
    match RtspPublishTarget::new("rtsps:///", "program") {
        Err(RtspPublishError::MissingAuthority { base }) => {
            assert_eq!(base, "rtsps:///");
        }
        other => panic!("expected MissingAuthority, got {other:?}"),
    }
}

#[test]
fn a_base_with_a_path_is_rejected() {
    // The base is host[:port] only; a path belongs in the mount, so a base that
    // already carries a path is ambiguous and rejected (no silent concatenation).
    match RtspPublishTarget::new("rtsp://127.0.0.1:8554/already", "program") {
        Err(RtspPublishError::BaseHasPath { base }) => {
            assert_eq!(base, "rtsp://127.0.0.1:8554/already");
        }
        other => panic!("expected BaseHasPath, got {other:?}"),
    }
}

#[test]
fn whitespace_in_a_mount_is_rejected() {
    // A mount with whitespace would produce an invalid RTSP URL; reject it rather
    // than emit a broken target.
    match RtspPublishTarget::new("rtsp://127.0.0.1:8554", "bad mount") {
        Err(RtspPublishError::InvalidMount { mount }) => {
            assert_eq!(mount, "bad mount");
        }
        other => panic!("expected InvalidMount, got {other:?}"),
    }
}

/// Under the `ffmpeg` feature the target couples to the existing push transport:
/// it names the same `rtsp` libav muxer the OUT-1 baseline reuses (no new sink
/// code — invariant #7: the SAME one-encode stream is muxed to RTSP).
#[cfg(feature = "ffmpeg")]
#[test]
fn target_selects_the_rtsp_push_protocol_and_muxer() {
    use multiview_output::sink::PushProtocol;

    let target = RtspPublishTarget::new("rtsp://127.0.0.1:8554", "program").unwrap();
    assert_eq!(target.protocol(), PushProtocol::Rtsp);
    assert_eq!(target.protocol().muxer_name(), "rtsp");
    // The pure assertion the work-schedule calls out as always-CI.
    assert_eq!(PushProtocol::Rtsp.muxer_name(), "rtsp");
}

/// LIVE-ONLY (sidecar-gated): the genuine RTSP publish hop to a listening
/// `MediaMTX` endpoint, then an `ffprobe` re-read of the served stream. `#[ignore]`d
/// because CI has no RTSP sidecar (network + extra process); run on a host that
/// has a `MediaMTX` (or `ffmpeg -rtsp_flags listen`) peer on 8554 via:
///
/// ```text
/// cargo test -p multiview-output --features ffmpeg -- --ignored rtsp_live
/// ```
///
/// This is the ONLY place the live network push is exercised; it never runs
/// unattended (no faked network pass — the seam above is what CI proves).
#[cfg(feature = "ffmpeg")]
#[test]
#[ignore = "requires a listening RTSP sidecar (MediaMTX / ffmpeg -rtsp_flags listen) on the publish base; absent in CI"]
fn rtsp_live_push_reaches_a_listening_sidecar() {
    use ffmpeg_next::format::Pixel;
    use ffmpeg_next::util::frame::Video;
    use multiview_core::color::ColorInfo;
    use multiview_core::frame::FrameMeta;
    use multiview_core::pixel::PixelFormat;
    use multiview_core::time::{MediaTime, Rational};
    use multiview_ffmpeg::DecodedVideoFrame;
    use multiview_output::sink::{EncodeConfig, PushProtocol, PushSink, VideoFrameSource};
    use multiview_output::Result;

    const W: u32 = 320;
    const H: u32 = 240;
    const FRAMES: u32 = 50;

    struct GrayNv12Source {
        remaining: u32,
    }
    impl VideoFrameSource for GrayNv12Source {
        fn next_frame(&mut self) -> Result<Option<DecodedVideoFrame>> {
            if self.remaining == 0 {
                return Ok(None);
            }
            self.remaining -= 1;
            let mut frame = Video::new(Pixel::NV12, W, H);
            for p in 0..frame.planes() {
                for byte in frame.data_mut(p).iter_mut() {
                    *byte = 128;
                }
            }
            let meta = FrameMeta {
                pts: MediaTime::ZERO,
                width: W,
                height: H,
                format: PixelFormat::Nv12,
                color: ColorInfo::default(),
            };
            Ok(Some(DecodedVideoFrame {
                frame,
                meta,
                raw_pts: None,
                a53_cc: None,
            }))
        }
    }

    let base =
        std::env::var("MULTIVIEW_RTSP_BASE").unwrap_or_else(|_| "rtsp://[::1]:8554".to_owned());
    let target = RtspPublishTarget::new(&base, "multiview_out1").unwrap();
    assert_eq!(target.protocol(), PushProtocol::Rtsp);

    let config = EncodeConfig {
        codec_name: "mpeg2video".to_owned(),
        width: W,
        height: H,
        format: Pixel::YUV420P,
        cadence: Rational::new(25, 1),
        gop: 25,
        bit_rate: 1_500_000,
        audio: None,
        cuda_ordinal: None,
    };
    let sink = PushSink::new(config, target.protocol(), target.publish_url());
    let mut source = GrayNv12Source { remaining: FRAMES };
    let stats = sink
        .run(&mut source)
        .expect("rtsp push run against the sidecar");
    assert_eq!(stats.packets, u64::from(FRAMES));
    assert!(stats.keyframes >= 1);
}
