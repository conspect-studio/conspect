//! Tests for the WebRTC SDP offer/answer model. Pure (no ICE/DTLS); runs in the
//! DEFAULT build. The transport shell is behind the off-by-default `webrtc`
//! feature and is compile-verified only.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use multiview_input::webrtc::{Codec, MediaKind, SdpDirection, SessionDescription, WebRtcError};

/// A representative WebRTC ingest offer: one audio (Opus) and one video (H264 +
/// VP8) m-line, with ICE/DTLS attributes the model ignores.
const OFFER: &str = "v=0\r\n\
o=- 46117 2 IN IP4 127.0.0.1\r\n\
s=-\r\n\
t=0 0\r\n\
m=audio 9 UDP/TLS/RTP/SAVPF 111\r\n\
c=IN IP4 0.0.0.0\r\n\
a=rtpmap:111 opus/48000/2\r\n\
a=sendonly\r\n\
a=ice-ufrag:abcd\r\n\
a=fingerprint:sha-256 AA:BB\r\n\
m=video 9 UDP/TLS/RTP/SAVPF 96 98\r\n\
c=IN IP4 0.0.0.0\r\n\
a=rtpmap:96 VP8/90000\r\n\
a=rtpmap:98 H264/90000\r\n\
a=sendonly\r\n";

#[test]
fn sdp_parses_media_sections() {
    let sdp = SessionDescription::parse(OFFER).expect("valid SDP");
    assert_eq!(sdp.version, 0);
    assert_eq!(sdp.media.len(), 2);

    let audio = sdp.audio().expect("audio");
    assert_eq!(audio.kind, MediaKind::Audio);
    assert_eq!(audio.payload_types, vec![111]);
    assert_eq!(audio.direction, SdpDirection::SendOnly);
    let opus = audio.rtpmap(111).expect("opus rtpmap");
    assert_eq!(opus.encoding_name, "opus");
    assert_eq!(opus.clock_rate, 48_000);
    assert_eq!(opus.channels, Some(2));

    let video = sdp.video().expect("video");
    assert_eq!(video.payload_types, vec![96, 98]);
    assert_eq!(video.rtpmaps.len(), 2);
}

#[test]
fn sdp_negotiation_prefers_offer_order() {
    let sdp = SessionDescription::parse(OFFER).unwrap();
    // We support H264 only for video; the offer lists VP8 (96) before H264 (98),
    // so the negotiator must skip VP8 and pick H264 at PT 98.
    let answer = sdp
        .negotiate_answer(&[Codec::H264], &[Codec::OPUS])
        .expect("negotiated");

    let video = answer.video().expect("video answer");
    assert_eq!(video.payload_type, 98);
    assert_eq!(video.codec, Codec::H264);
    // The offer was sendonly, so the answer is recvonly.
    assert_eq!(video.direction, SdpDirection::RecvOnly);

    let audio = answer.audio().expect("audio answer");
    assert_eq!(audio.payload_type, 111);
    assert_eq!(audio.codec, Codec::OPUS);
}

#[test]
fn sdp_negotiation_honours_preference_when_both_supported() {
    let sdp = SessionDescription::parse(OFFER).unwrap();
    // Supporting both VP8 and H264: VP8 (96) is offered first, so it wins.
    let answer = sdp
        .negotiate_answer(&[Codec::VP8, Codec::H264], &[Codec::OPUS])
        .unwrap();
    let video = answer.video().unwrap();
    assert_eq!(video.payload_type, 96);
    assert_eq!(video.codec, Codec::VP8);
}

#[test]
fn sdp_negotiation_fails_with_no_common_codec() {
    let sdp = SessionDescription::parse(OFFER).unwrap();
    // Offer has no AV1, so negotiation fails for video.
    let av1 = Codec {
        name: "AV1",
        clock_rate: 90_000,
    };
    assert!(matches!(
        sdp.negotiate_answer(&[av1], &[Codec::OPUS]),
        Err(WebRtcError::NoCompatibleCodec("video"))
    ));
}

#[test]
fn sdp_direction_reciprocal() {
    assert_eq!(SdpDirection::SendOnly.reciprocal(), SdpDirection::RecvOnly);
    assert_eq!(SdpDirection::RecvOnly.reciprocal(), SdpDirection::SendOnly);
    assert_eq!(SdpDirection::SendRecv.reciprocal(), SdpDirection::SendRecv);
    assert_eq!(SdpDirection::Inactive.reciprocal(), SdpDirection::Inactive);
}

#[test]
fn sdp_rejects_malformed_lines() {
    assert!(matches!(
        SessionDescription::parse("v=0\r\nthisisnotvalid\r\n"),
        Err(WebRtcError::MalformedSdp(_))
    ));
}

#[test]
fn sdp_rejects_bad_numeric_field() {
    assert!(matches!(
        SessionDescription::parse("v=0\r\nm=video notaport UDP/TLS/RTP/SAVPF 96\r\n"),
        Err(WebRtcError::BadField {
            field: "m.port",
            ..
        })
    ));
}

#[test]
fn sdp_empty_offer_has_no_media() {
    let sdp = SessionDescription::parse("v=0\r\ns=-\r\n").unwrap();
    assert!(sdp.media.is_empty());
    assert!(matches!(
        sdp.negotiate_answer(&[Codec::H264], &[Codec::OPUS]),
        Err(WebRtcError::NoMedia)
    ));
}
