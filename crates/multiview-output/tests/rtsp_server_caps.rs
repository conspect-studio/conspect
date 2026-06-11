//! CI tests for the pure RTSP-server codec/caps + timestamp-conversion seams
//! (OUT-2) — always compiled, no `GStreamer` required.
//!
//! These cover the data the feature-gated `gst-rtsp-server` pipeline consumes:
//! the codec → caps/parser/payloader/launch-line mapping ([`RtspCodec`]) and the
//! `units → nanoseconds` buffer-timestamp conversion ([`units_to_nanos`]), so the
//! contract the live serve relies on is verified without the C stack.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use multiview_output::rtsp_server::{units_to_nanos, RtspCapsError, RtspCodec};

#[test]
fn codec_resolves_h264_aliases() {
    for name in ["h264", "libx264", "h264_nvenc", "H264", "avc1"] {
        assert_eq!(
            RtspCodec::from_codec_name(name).unwrap(),
            RtspCodec::H264,
            "{name} should resolve to H264"
        );
    }
}

#[test]
fn codec_resolves_h265_aliases() {
    for name in ["h265", "hevc", "libx265", "hevc_nvenc", "HEVC"] {
        assert_eq!(
            RtspCodec::from_codec_name(name).unwrap(),
            RtspCodec::H265,
            "{name} should resolve to H265"
        );
    }
}

#[test]
fn codec_rejects_non_payloadable() {
    // The intra-only / raw codecs the file/HLS sinks use are not RTSP renditions.
    for name in ["mpeg2video", "mjpeg", "ffv1", "rawvideo"] {
        assert!(
            matches!(
                RtspCodec::from_codec_name(name),
                Err(RtspCapsError::UnsupportedCodec { .. })
            ),
            "{name} should be rejected as non-payloadable"
        );
    }
}

#[test]
fn codec_caps_and_elements_are_correct() {
    assert_eq!(
        RtspCodec::H264.appsrc_caps(),
        "video/x-h264,stream-format=byte-stream,alignment=au"
    );
    assert_eq!(RtspCodec::H264.parser_element(), "h264parse");
    assert_eq!(RtspCodec::H264.payloader_element(), "rtph264pay");

    assert_eq!(
        RtspCodec::H265.appsrc_caps(),
        "video/x-h265,stream-format=byte-stream,alignment=au"
    );
    assert_eq!(RtspCodec::H265.parser_element(), "h265parse");
    assert_eq!(RtspCodec::H265.payloader_element(), "rtph265pay");
}

#[test]
fn codec_launch_description_has_named_appsrc_and_pay0() {
    let launch = RtspCodec::H264.launch_description();
    // gst-rtsp-server requires a payloader named `pay0`; appsrc must be `src`,
    // live, format=time; config-interval=-1 repeats SPS/PPS for late joiners.
    assert!(launch.contains("appsrc name=src"));
    assert!(launch.contains("is-live=true"));
    assert!(launch.contains("format=time"));
    assert!(launch.contains("h264parse"));
    assert!(launch.contains("rtph264pay name=pay0"));
    assert!(launch.contains("config-interval=-1"));

    let h265 = RtspCodec::H265.launch_description();
    assert!(h265.contains("h265parse"));
    assert!(h265.contains("rtph265pay name=pay0"));
}

// ----------------------------------------------------------------------------
// units_to_nanos — the buffer-timestamp conversion the appsrc feed uses.
// ----------------------------------------------------------------------------

#[test]
fn units_to_nanos_90khz_timebase() {
    // 90 kHz: one whole second is 90000 units → 1e9 ns.
    assert_eq!(units_to_nanos(90_000, (1, 90_000)), Some(1_000_000_000));
    // 3000 units (one 30 fps frame at 90 kHz) → 1/30 s = 33_333_333 ns (floor).
    assert_eq!(units_to_nanos(3000, (1, 90_000)), Some(33_333_333));
    assert_eq!(units_to_nanos(0, (1, 90_000)), Some(0));
}

#[test]
fn units_to_nanos_whole_fps_timebase() {
    // (1, 30) seconds-per-unit: 30 units = 1 s.
    assert_eq!(units_to_nanos(30, (1, 30)), Some(1_000_000_000));
    assert_eq!(units_to_nanos(1, (1, 30)), Some(33_333_333));
}

#[test]
fn units_to_nanos_rejects_invalid() {
    // Negative input (should never occur — tick-restamped) → None, never wraps.
    assert_eq!(units_to_nanos(-1, (1, 90_000)), None);
    // Zero denominator → None, never a divide-by-zero panic.
    assert_eq!(units_to_nanos(100, (1, 0)), None);
}

#[test]
fn units_to_nanos_no_overflow_over_long_run() {
    // ~24 h at 90 kHz is ~7.8e9 units; the u128 intermediate must not overflow
    // before the divide (a u64 product would).
    let units = 90_000i64 * 60 * 60 * 24; // 24 h of 90 kHz ticks
    let nanos = units_to_nanos(units, (1, 90_000)).unwrap();
    assert_eq!(nanos, 86_400 * 1_000_000_000);
}
