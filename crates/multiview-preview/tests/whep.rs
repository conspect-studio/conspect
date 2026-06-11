//! WHEP focus-session scaffold (behind the off-by-default `webrtc` feature): the
//! pure offer/answer + preview-encoder-selection logic. The real ICE/DTLS/SRTP
//! transport is a separately-gated TODO; these tests cover only the pure parts,
//! which is why the whole file compiles to nothing without the feature.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
#![cfg(feature = "webrtc")]

use multiview_preview::whep::{PreviewCodec, WhepError, WhepSession};
use multiview_preview::AccessScope;

/// A minimal-but-realistic WHEP SDP offer advertising H.264 (PT 96) and VP8
/// (PT 97) video.
const OFFER: &str = "v=0\r\n\
o=- 0 0 IN IP4 0.0.0.0\r\n\
s=-\r\n\
t=0 0\r\n\
m=video 9 UDP/TLS/RTP/SAVPF 96 97\r\n\
c=IN IP4 0.0.0.0\r\n\
a=rtpmap:96 H264/90000\r\n\
a=rtpmap:97 VP8/90000\r\n\
a=sendrecv\r\n";

#[test]
fn whep_requires_focus_access() {
    // A View token can never open a WHEP focus session (the cap on concurrent
    // focus sessions is only enforceable if Focus is granted explicitly).
    let err = WhepSession::negotiate(OFFER, AccessScope::View).unwrap_err();
    assert!(matches!(err, WhepError::AccessDenied { .. }), "got {err:?}");
}

#[test]
fn whep_selects_a_preview_codec_and_answers() {
    let session = WhepSession::negotiate(OFFER, AccessScope::Focus)
        .expect("a focus offer with H.264 negotiates");
    // H.264 is preferred for the low-latency preview encode session.
    assert_eq!(session.codec(), PreviewCodec::H264);
    // The answer is a real SDP answer naming the chosen payload type.
    let answer = session.answer_sdp();
    assert!(answer.starts_with("v=0\r\n"), "answer is SDP: {answer}");
    assert!(answer.contains("m=video"), "answer has a video m-line");
    assert!(
        answer.contains("a=rtpmap:96 H264/90000"),
        "answer names the selected H.264 payload: {answer}"
    );
    // WHEP answers are recvonly from the server's perspective wrt the browser
    // sendonly... the preview server SENDS media, so it advertises sendonly.
    assert!(answer.contains("a=sendonly"), "server sends preview media");
}

#[test]
fn whep_falls_back_to_vp8_when_no_h264() {
    let offer = OFFER.replace("a=rtpmap:96 H264/90000\r\n", "");
    let offer = offer.replace("96 97", "97");
    let session = WhepSession::negotiate(&offer, AccessScope::Focus)
        .expect("VP8-only offer still negotiates");
    assert_eq!(session.codec(), PreviewCodec::Vp8);
    assert!(session.answer_sdp().contains("VP8/90000"));
}

#[test]
fn whep_rejects_offer_without_supported_video() {
    // An audio-only / unsupported-codec offer cannot start a preview encode.
    let offer = "v=0\r\n\
o=- 0 0 IN IP4 0.0.0.0\r\n\
s=-\r\n\
t=0 0\r\n\
m=audio 9 UDP/TLS/RTP/SAVPF 111\r\n\
a=rtpmap:111 opus/48000/2\r\n";
    let err = WhepSession::negotiate(offer, AccessScope::Focus).unwrap_err();
    assert!(
        matches!(err, WhepError::NoSupportedCodec),
        "audio-only offer must be rejected, got {err:?}"
    );
}

#[test]
fn whep_rejects_malformed_offer() {
    let err = WhepSession::negotiate("not an sdp", AccessScope::Focus).unwrap_err();
    assert!(
        matches!(err, WhepError::MalformedOffer { .. }),
        "got {err:?}"
    );
}
