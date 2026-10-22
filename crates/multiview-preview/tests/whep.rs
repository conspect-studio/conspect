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

/// A browser-shaped offer carrying BOTH a video m-line (H.264 + VP8) and an
/// Opus audio m-line, bundled (the shape every real browser offer takes).
const AV_OFFER: &str = "v=0\r\n\
o=- 0 0 IN IP6 ::\r\n\
s=-\r\n\
t=0 0\r\n\
a=group:BUNDLE 0 1\r\n\
m=video 9 UDP/TLS/RTP/SAVPF 96 97\r\n\
c=IN IP6 ::\r\n\
a=mid:0\r\n\
a=rtpmap:96 H264/90000\r\n\
a=rtpmap:97 VP8/90000\r\n\
a=sendrecv\r\n\
m=audio 9 UDP/TLS/RTP/SAVPF 111\r\n\
c=IN IP6 ::\r\n\
a=mid:1\r\n\
a=rtpmap:111 opus/48000/2\r\n\
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
fn whep_answer_is_ipv6_first_browser_shaped() {
    // ADR-P006 move 2 + ADR-0042: the pure scaffold answer is honest,
    // browser-shaped SDP — IPv6-first connection lines (`c=IN IP6 ::`, never
    // `IN IP4`), a session-level BUNDLE group, a per-media mid, and rtcp-mux —
    // the same dialect browsers see, not a divergent one.
    let session = WhepSession::negotiate(OFFER, AccessScope::Focus).expect("negotiates");
    let answer = session.answer_sdp();
    assert!(
        answer.contains("c=IN IP6 ::\r\n"),
        "IPv6-first c= line (ADR-0042): {answer}"
    );
    assert!(
        !answer.contains("IN IP4"),
        "never IN IP4 anywhere in the answer (ADR-0042): {answer}"
    );
    assert!(
        answer.contains("a=group:BUNDLE 0\r\n"),
        "session-level BUNDLE group: {answer}"
    );
    assert!(answer.contains("a=mid:0\r\n"), "video mid: {answer}");
    assert!(answer.contains("a=rtcp-mux\r\n"), "rtcp-mux: {answer}");
    // BUNDLE is a SESSION-level attribute: it must precede the first m= line.
    let bundle = answer.find("a=group:BUNDLE").expect("bundle present");
    let mline = answer.find("m=video").expect("video m-line present");
    assert!(bundle < mline, "BUNDLE precedes the first m-line: {answer}");
}

#[test]
fn whep_answer_includes_opus_audio_section_when_offered() {
    // ADR-P006 move 3: an offer carrying an Opus audio m-line negotiates a
    // second, sendonly Opus audio section with its own mid, joined to the
    // BUNDLE group. Audio is Opus by definition on this seam (RFC 7874) and
    // rides the 48 kHz RTP clock RFC 7587 fixes.
    let session = WhepSession::negotiate(AV_OFFER, AccessScope::Focus).expect("negotiates");
    // The video codec-selection contract is unchanged by audio.
    assert_eq!(session.codec(), PreviewCodec::H264);
    let answer = session.answer_sdp();
    assert!(
        answer.contains("a=group:BUNDLE 0 1\r\n"),
        "audio joins the bundle: {answer}"
    );
    assert!(
        answer.contains("m=audio 9 UDP/TLS/RTP/SAVPF 111\r\n"),
        "audio m-line echoes the offered Opus payload type: {answer}"
    );
    assert!(
        answer.contains("a=rtpmap:111 opus/48000/2\r\n"),
        "Opus at the RFC 7587-fixed 48 kHz clock: {answer}"
    );
    assert!(answer.contains("a=mid:1\r\n"), "audio mid: {answer}");
    // The audio section itself is sendonly + rtcp-mux (the server transmits).
    let audio_at = answer.find("m=audio").expect("audio m-line present");
    let audio_section = &answer[audio_at..];
    assert!(
        audio_section.contains("a=sendonly"),
        "audio is sendonly: {audio_section}"
    );
    assert!(
        audio_section.contains("a=rtcp-mux"),
        "audio is rtcp-muxed: {audio_section}"
    );
}

#[test]
fn whep_answer_omits_audio_section_when_offer_has_none() {
    // ADR-P006: sessions whose offer carries no audio m-line simply leave
    // audio absent — the answer stays video-only, bundle of one.
    let session = WhepSession::negotiate(OFFER, AccessScope::Focus).expect("negotiates");
    let answer = session.answer_sdp();
    assert!(
        !answer.contains("m=audio"),
        "no audio m-line without an offered one: {answer}"
    );
    assert!(
        answer.contains("a=group:BUNDLE 0\r\n") && !answer.contains("a=group:BUNDLE 0 1"),
        "video-only bundle: {answer}"
    );
}

#[test]
fn whep_answer_skips_a_non_opus_audio_offer() {
    // A (hypothetical) audio m-line with no Opus mapping cannot be answered —
    // audio on this seam is Opus by definition (RFC 7874 / ADR-P006) — so the
    // audio section is left absent rather than mis-negotiated.
    let offer = AV_OFFER.replace("a=rtpmap:111 opus/48000/2\r\n", "a=rtpmap:0 PCMU/8000\r\n");
    let session = WhepSession::negotiate(&offer, AccessScope::Focus).expect("negotiates");
    let answer = session.answer_sdp();
    assert!(
        !answer.contains("m=audio"),
        "non-Opus audio is never answered: {answer}"
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
