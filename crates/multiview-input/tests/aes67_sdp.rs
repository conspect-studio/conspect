//! AES67 / SMPTE ST 2110-30 audio SDP parser + generator tests (RFC 4566/8866,
//! RFC 7273 reference-clock attributes). These run in the DEFAULT (pure-Rust)
//! build — SDP is text-in / text-out, no NIC, no RTP, no timestamps.
//!
//! IPv6-first (ADR-0042): the connection line carries `c=IN IP6 <group>` and the
//! examples lead with an IPv6 multicast group (`FF3x::/32` SSM range).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::as_conversions,
    clippy::cast_possible_truncation
)]

use multiview_input::st2110::sdp::{AudioSdpSession, SdpError, TsRefclk};
use multiview_input::st2110::v30::SampleDepth;
use proptest::prelude::*;

// ---------------------------------------------------------------------------
// Parse — golden vectors
// ---------------------------------------------------------------------------

#[test]
fn parse_class_a_l24_stereo_1ms() {
    let sdp = "v=0\r\n\
               o=- 1 1 IN IP6 2001:db8::1\r\n\
               s=Multiview AES67\r\n\
               c=IN IP6 ff3e::1\r\n\
               t=0 0\r\n\
               m=audio 5004 RTP/AVP 98\r\n\
               a=rtpmap:98 L24/48000/2\r\n\
               a=ptime:1\r\n\
               a=ts-refclk:ptp=IEEE1588-2008:AA-BB-CC-DD-EE-FF-00-11:0\r\n\
               a=mediaclk:direct=0\r\n";
    let sess = AudioSdpSession::parse(sdp).expect("valid Class A L24 session");
    assert_eq!(sess.port, 5004);
    assert_eq!(sess.payload_type, 98);
    assert_eq!(sess.format.channels, 2);
    assert_eq!(sess.format.depth, SampleDepth::L24);
    assert_eq!(sess.clock_rate, 48_000);
    assert_eq!(sess.ptime_ms_x1000, 1_000);
    assert_eq!(
        sess.ts_refclk,
        TsRefclk::Ptp {
            gmid: "AA-BB-CC-DD-EE-FF-00-11".to_owned(),
            domain: 0,
        }
    );
    assert_eq!(sess.mediaclk_offset, 0);
}

#[test]
fn parse_l16_mono() {
    let sdp = "m=audio 5004 RTP/AVP 97\n\
               a=rtpmap:97 L16/48000/1\n\
               a=ptime:1\n\
               a=ts-refclk:ptp=IEEE1588-2008:11-22-33-44-55-66-77-88:0\n\
               a=mediaclk:direct=0\n";
    let sess = AudioSdpSession::parse(sdp).expect("valid L16 mono session");
    assert_eq!(sess.format.channels, 1);
    assert_eq!(sess.format.depth, SampleDepth::L16);
    assert_eq!(sess.payload_type, 97);
}

#[test]
fn parse_mediaclk_nonzero_offset() {
    let sdp = "m=audio 5004 RTP/AVP 98\n\
               a=rtpmap:98 L24/48000/2\n\
               a=ptime:1\n\
               a=ts-refclk:ptp=IEEE1588-2008:DE-AD-BE-EF-00-00-00-00:1\n\
               a=mediaclk:direct=12345\n";
    let sess = AudioSdpSession::parse(sdp).expect("valid mediaclk offset");
    assert_eq!(sess.mediaclk_offset, 12_345);
    assert_eq!(
        sess.ts_refclk,
        TsRefclk::Ptp {
            gmid: "DE-AD-BE-EF-00-00-00-00".to_owned(),
            domain: 1,
        }
    );
}

#[test]
fn parse_fractional_ptime_class_b() {
    // Class B = 125 µs packet time (0.125 ms).
    let sdp = "m=audio 5004 RTP/AVP 98\n\
               a=rtpmap:98 L24/48000/8\n\
               a=ptime:0.125\n\
               a=ts-refclk:ptp=IEEE1588-2008:00-00-00-00-00-00-00-00:0\n\
               a=mediaclk:direct=0\n";
    let sess = AudioSdpSession::parse(sdp).expect("valid Class B session");
    assert_eq!(sess.ptime_ms_x1000, 125);
    assert_eq!(sess.format.channels, 8);
}

#[test]
fn parse_localmac_ts_refclk() {
    let sdp = "m=audio 5004 RTP/AVP 98\n\
               a=rtpmap:98 L24/48000/2\n\
               a=ptime:1\n\
               a=ts-refclk:localmac=00-11-22-33-44-55\n\
               a=mediaclk:direct=0\n";
    let sess = AudioSdpSession::parse(sdp).expect("valid localmac session");
    assert_eq!(
        sess.ts_refclk,
        TsRefclk::LocalMac {
            mac: [0x00, 0x11, 0x22, 0x33, 0x44, 0x55],
        }
    );
}

#[test]
fn parse_96khz() {
    let sdp = "m=audio 5004 RTP/AVP 98\n\
               a=rtpmap:98 L24/96000/2\n\
               a=ptime:1\n\
               a=ts-refclk:ptp=IEEE1588-2008:00-00-00-00-00-00-00-00:0\n\
               a=mediaclk:direct=0\n";
    let sess = AudioSdpSession::parse(sdp).expect("valid 96 kHz session");
    assert_eq!(sess.clock_rate, 96_000);
    assert_eq!(sess.format.channels, 2);
}

#[test]
fn parse_ignores_unrelated_video_section_before_audio() {
    let sdp = "m=video 5000 RTP/AVP 96\n\
               a=rtpmap:96 raw/90000\n\
               m=audio 5004 RTP/AVP 98\n\
               a=rtpmap:98 L24/48000/2\n\
               a=ptime:1\n\
               a=ts-refclk:ptp=IEEE1588-2008:00-00-00-00-00-00-00-00:0\n\
               a=mediaclk:direct=0\n";
    let sess = AudioSdpSession::parse(sdp).expect("audio section found after video");
    assert_eq!(sess.port, 5004);
    assert_eq!(sess.format.channels, 2);
}

// ---------------------------------------------------------------------------
// Generate + round-trip
// ---------------------------------------------------------------------------

#[test]
fn generate_emits_well_formed_lines() {
    let sess = AudioSdpSession {
        port: 5004,
        payload_type: 98,
        format: multiview_input::st2110::v30::Aes3Format::new(2, SampleDepth::L24)
            .expect("valid format"),
        clock_rate: 48_000,
        ptime_ms_x1000: 1_000,
        ts_refclk: TsRefclk::Ptp {
            gmid: "AA-BB-CC-DD-EE-FF-00-11".to_owned(),
            domain: 0,
        },
        mediaclk_offset: 0,
    };
    let lines = sess.generate();
    assert!(lines.iter().any(|l| l == "m=audio 5004 RTP/AVP 98"));
    assert!(lines.iter().any(|l| l == "a=rtpmap:98 L24/48000/2"));
    assert!(lines.iter().any(|l| l == "a=ptime:1"));
    assert!(lines
        .iter()
        .any(|l| l == "a=ts-refclk:ptp=IEEE1588-2008:AA-BB-CC-DD-EE-FF-00-11:0"));
    assert!(lines.iter().any(|l| l == "a=mediaclk:direct=0"));
}

#[test]
fn generate_emits_fractional_ptime() {
    let sess = AudioSdpSession {
        port: 5004,
        payload_type: 98,
        format: multiview_input::st2110::v30::Aes3Format::new(8, SampleDepth::L24)
            .expect("valid format"),
        clock_rate: 48_000,
        ptime_ms_x1000: 125,
        ts_refclk: TsRefclk::Ptp {
            gmid: "00-00-00-00-00-00-00-00".to_owned(),
            domain: 0,
        },
        mediaclk_offset: 0,
    };
    let lines = sess.generate();
    assert!(
        lines.iter().any(|l| l == "a=ptime:0.125"),
        "fractional ptime must render as 0.125, got {lines:?}"
    );
}

#[test]
fn round_trip_idempotent_ptp() {
    let original = AudioSdpSession {
        port: 5004,
        payload_type: 100,
        format: multiview_input::st2110::v30::Aes3Format::new(2, SampleDepth::L24)
            .expect("valid format"),
        clock_rate: 48_000,
        ptime_ms_x1000: 1_000,
        ts_refclk: TsRefclk::Ptp {
            gmid: "DE-AD-BE-EF-CA-FE-BA-BE".to_owned(),
            domain: 7,
        },
        mediaclk_offset: 98_765,
    };
    let reparsed = AudioSdpSession::parse(&original.generate().join("\n"))
        .expect("generated SDP must re-parse");
    assert_eq!(original, reparsed);
}

#[test]
fn round_trip_idempotent_localmac() {
    let original = AudioSdpSession {
        port: 5004,
        payload_type: 96,
        format: multiview_input::st2110::v30::Aes3Format::new(1, SampleDepth::L16)
            .expect("valid format"),
        clock_rate: 96_000,
        ptime_ms_x1000: 125,
        ts_refclk: TsRefclk::LocalMac {
            mac: [0xAB, 0xCD, 0xEF, 0x01, 0x23, 0x45],
        },
        mediaclk_offset: 0,
    };
    let reparsed = AudioSdpSession::parse(&original.generate().join("\n"))
        .expect("generated SDP must re-parse");
    assert_eq!(original, reparsed);
}

// ---------------------------------------------------------------------------
// Reject malformed
// ---------------------------------------------------------------------------

#[test]
fn reject_no_audio_section() {
    let sdp = "m=video 5000 RTP/AVP 96\na=rtpmap:96 raw/90000";
    assert!(matches!(
        AudioSdpSession::parse(sdp),
        Err(SdpError::MissingAudioSection)
    ));
}

#[test]
fn reject_missing_rtpmap() {
    let sdp = "m=audio 5004 RTP/AVP 98\na=ptime:1";
    assert!(matches!(
        AudioSdpSession::parse(sdp),
        Err(SdpError::MissingRtpmap)
    ));
}

#[test]
fn reject_zero_channels() {
    let sdp = "m=audio 5004 RTP/AVP 98\na=rtpmap:98 L24/48000/0\na=ptime:1";
    assert!(matches!(
        AudioSdpSession::parse(sdp),
        Err(SdpError::BadChannelCount(0))
    ));
}

#[test]
fn reject_unknown_codec() {
    let sdp = "m=audio 5004 RTP/AVP 98\na=rtpmap:98 OPUS/48000/2\na=ptime:1";
    assert!(matches!(
        AudioSdpSession::parse(sdp),
        Err(SdpError::UnknownCodec(_))
    ));
}

#[test]
fn reject_bad_clock_rate() {
    let sdp = "m=audio 5004 RTP/AVP 98\na=rtpmap:98 L24/44100/2\na=ptime:1";
    assert!(matches!(
        AudioSdpSession::parse(sdp),
        Err(SdpError::BadClockRate(44_100))
    ));
}

#[test]
fn reject_missing_ptime() {
    let sdp = "m=audio 5004 RTP/AVP 98\n\
               a=rtpmap:98 L24/48000/2\n\
               a=ts-refclk:ptp=IEEE1588-2008:00-00-00-00-00-00-00-00:0\n\
               a=mediaclk:direct=0\n";
    assert!(matches!(
        AudioSdpSession::parse(sdp),
        Err(SdpError::MissingPtime)
    ));
}

#[test]
fn reject_missing_ts_refclk() {
    let sdp = "m=audio 5004 RTP/AVP 98\n\
               a=rtpmap:98 L24/48000/2\n\
               a=ptime:1\n\
               a=mediaclk:direct=0\n";
    assert!(matches!(
        AudioSdpSession::parse(sdp),
        Err(SdpError::MissingTsRefclk)
    ));
}

#[test]
fn reject_malformed_ptime() {
    let sdp = "m=audio 5004 RTP/AVP 98\n\
               a=rtpmap:98 L24/48000/2\n\
               a=ptime:not-a-number\n\
               a=ts-refclk:ptp=IEEE1588-2008:00-00-00-00-00-00-00-00:0\n";
    assert!(matches!(
        AudioSdpSession::parse(sdp),
        Err(SdpError::BadPtime(_))
    ));
}

// ---------------------------------------------------------------------------
// Property: never panic on arbitrary input; generate→parse is a fixed point
// ---------------------------------------------------------------------------

proptest! {
    #[test]
    fn parse_never_panics(s in ".{0,200}") {
        // Arbitrary text must surface a typed error or a valid session — never panic.
        let _ = AudioSdpSession::parse(&s);
    }

    #[test]
    fn generate_then_parse_is_identity(
        port in any::<u16>(),
        pt in 96u8..=127,
        channels in 1u8..=16,
        l24 in any::<bool>(),
        clock_96 in any::<bool>(),
        class_b in any::<bool>(),
        domain in 0u8..=127,
        offset in any::<u32>(),
        gmid_bytes in proptest::array::uniform8(any::<u8>()),
    ) {
        let depth = if l24 { SampleDepth::L24 } else { SampleDepth::L16 };
        let clock_rate = if clock_96 { 96_000 } else { 48_000 };
        let ptime_ms_x1000 = if class_b { 125 } else { 1_000 };
        let gmid = gmid_bytes
            .iter()
            .map(|b| format!("{b:02X}"))
            .collect::<Vec<_>>()
            .join("-");
        let original = AudioSdpSession {
            port,
            payload_type: pt,
            format: multiview_input::st2110::v30::Aes3Format::new(channels, depth)
                .expect("nonzero channels"),
            clock_rate,
            ptime_ms_x1000,
            ts_refclk: TsRefclk::Ptp { gmid, domain },
            mediaclk_offset: offset,
        };
        let reparsed = AudioSdpSession::parse(&original.generate().join("\n"))
            .expect("generated SDP must re-parse");
        prop_assert_eq!(original, reparsed);
    }
}
