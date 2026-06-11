//! The **native str0m `WhepTransport`** (PRV-1b) — gated behind the off-by-default
//! `webrtc-native` feature.
//!
//! These tests exercise the CI-runnable seam of the native transport. str0m is a
//! *sans-IO* WebRTC library, so the whole SDP offer→answer negotiation —
//! parsing the browser offer, building the `Rtc`, accepting the offer, and
//! producing an answer that carries **real** str0m-minted ICE ufrag/pwd + a real
//! self-signed DTLS-certificate fingerprint — runs with **no socket** and is
//! therefore fully tested here in CI. The live packet exchange — the DTLS
//! handshake, SRTP media egress, and an ffprobe check — needs a real UDP
//! socket/peer and is **NOT** exercised here: the env-gated `#[ignore]`d loopback
//! test at the bottom (`MULTIVIEW_WHEP_LOOPBACK=1`) only confirms socket bind +
//! host-candidate gathering; completing the handshake/SRTP/ffprobe is PRV-1c.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
#![cfg(feature = "webrtc-native")]

use std::sync::Arc;

use multiview_preview::whep::native::{parse_answer_attributes, Str0mWhepTransport};
use multiview_preview::whep::transport::{
    sample_feed, EncodedSample, PreviewMediaSource, SampleFeed, SampleSink, SessionState,
    WhepTransport,
};
use multiview_preview::whep::{PreviewCodec, WhepSession};
use multiview_preview::AccessScope;

/// A realistic browser WHEP offer: a recvonly video m-line with H.264 + VP8, ICE
/// credentials, a DTLS fingerprint, and `setup:actpass` — the minimum str0m needs
/// to accept the offer and answer it.
const BROWSER_OFFER: &str = "v=0\r\n\
o=- 4611731400430051336 2 IN IP4 127.0.0.1\r\n\
s=-\r\n\
t=0 0\r\n\
a=group:BUNDLE 0\r\n\
a=msid-semantic: WMS\r\n\
m=video 9 UDP/TLS/RTP/SAVPF 96 97\r\n\
c=IN IP4 0.0.0.0\r\n\
a=rtcp:9 IN IP4 0.0.0.0\r\n\
a=ice-ufrag:tEsT\r\n\
a=ice-pwd:abcdefghijklmnopqrstuvwx\r\n\
a=ice-options:trickle\r\n\
a=fingerprint:sha-256 \
6F:8E:1A:2B:3C:4D:5E:6F:70:81:92:A3:B4:C5:D6:E7:\
F8:09:1A:2B:3C:4D:5E:6F:70:81:92:A3:B4:C5:D6:E7\r\n\
a=setup:actpass\r\n\
a=mid:0\r\n\
a=recvonly\r\n\
a=rtcp-mux\r\n\
a=rtpmap:96 H264/90000\r\n\
a=fmtp:96 level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f\r\n\
a=rtpmap:97 VP8/90000\r\n";

/// An in-memory media source for the native transport: hands out a `SampleFeed`
/// the transport drains; lets the test feed samples via the producer `SampleSink`.
struct FakeMediaSource {
    codec: PreviewCodec,
    #[allow(dead_code)]
    sink: SampleSink,
    feed: std::sync::Mutex<Option<SampleFeed>>,
}

impl FakeMediaSource {
    fn new(codec: PreviewCodec, depth: usize) -> Self {
        let (sink, feed) = sample_feed(depth);
        Self {
            codec,
            sink,
            feed: std::sync::Mutex::new(Some(feed)),
        }
    }
}

impl PreviewMediaSource for FakeMediaSource {
    fn codec(&self) -> PreviewCodec {
        self.codec
    }
    fn feed(&self) -> SampleFeed {
        self.feed
            .lock()
            .ok()
            .and_then(|mut g| g.take())
            .unwrap_or_else(|| sample_feed(1).1)
    }
}

#[test]
fn native_accept_yields_real_str0m_ice_and_dtls_attributes() {
    // The pure half — negotiate selects the codec.
    let session = WhepSession::negotiate(BROWSER_OFFER, AccessScope::Focus).expect("focus offer");
    assert_eq!(session.codec(), PreviewCodec::H264);

    // The native str0m transport accepts the SAME offer and returns a
    // TransportAnswer whose ICE/DTLS attributes are minted by str0m — real,
    // non-placeholder, and distinct from the codec-only scaffold's 0.0.0.0 lines.
    let transport = Str0mWhepTransport::new();
    let media = FakeMediaSource::new(PreviewCodec::H264, 2);
    let ta = transport
        .accept(BROWSER_OFFER, session.codec(), &media)
        .expect("native transport accepts the offer");

    // str0m mints fresh ICE credentials per session, never the offer's values.
    assert!(!ta.ice_ufrag.is_empty(), "ufrag present");
    assert!(!ta.ice_pwd.is_empty(), "pwd present");
    assert_ne!(
        ta.ice_ufrag, "tEsT",
        "answer ufrag is the server's, not the offer's"
    );
    // The DTLS fingerprint is a real self-signed cert digest: sha-256, 32 bytes
    // → 32 colon-separated upper-case hex octets.
    assert_eq!(ta.fingerprint.algorithm, "sha-256");
    let octets: Vec<&str> = ta.fingerprint.value.split(':').collect();
    assert_eq!(
        octets.len(),
        32,
        "sha-256 digest is 32 octets: {}",
        ta.fingerprint.value
    );
    assert!(
        octets
            .iter()
            .all(|o| o.len() == 2 && o.chars().all(|c| c.is_ascii_hexdigit())),
        "each octet is 2 hex digits: {}",
        ta.fingerprint.value
    );

    // The assembled WHEP answer folds the real attributes in (not placeholders).
    let answer = session.build_answer(&ta);
    assert!(
        answer.contains(&format!("a=ice-ufrag:{}", ta.ice_ufrag)),
        "answer: {answer}"
    );
    assert!(
        answer.contains("a=fingerprint:sha-256 "),
        "answer: {answer}"
    );
    assert!(
        answer.contains("a=setup:passive"),
        "server answers passive: {answer}"
    );
    assert!(
        answer.contains("a=rtpmap:96 H264/90000"),
        "codec preserved: {answer}"
    );
}

#[test]
fn native_session_lifecycle_created_then_closed() {
    let transport = Str0mWhepTransport::new();
    let media = FakeMediaSource::new(PreviewCodec::H264, 2);
    let ta = transport
        .accept(BROWSER_OFFER, PreviewCodec::H264, &media)
        .expect("accept");

    // The freshly accepted session is in Created until ICE/DTLS drives it on.
    let state = transport.session_state(&ta.session_id);
    assert_eq!(state, Some(SessionState::Created));

    // Close tears it down; the handle goes terminal and close is idempotent.
    transport.close(&ta.session_id).expect("close");
    assert_eq!(
        transport.session_state(&ta.session_id),
        Some(SessionState::Closed)
    );
    transport.close(&ta.session_id).expect("idempotent close");
    assert_eq!(
        transport.session_state(&ta.session_id),
        Some(SessionState::Closed)
    );
}

#[test]
fn native_rejects_audio_only_offer_without_a_supported_codec() {
    // An offer that str0m parses but which carries no video we can preview-encode
    // must be rejected, never panic.
    const AUDIO_ONLY: &str = "v=0\r\n\
o=- 1 2 IN IP4 127.0.0.1\r\n\
s=-\r\n\
t=0 0\r\n\
a=group:BUNDLE 0\r\n\
m=audio 9 UDP/TLS/RTP/SAVPF 111\r\n\
c=IN IP4 0.0.0.0\r\n\
a=ice-ufrag:tEsT\r\n\
a=ice-pwd:abcdefghijklmnopqrstuvwx\r\n\
a=fingerprint:sha-256 \
6F:8E:1A:2B:3C:4D:5E:6F:70:81:92:A3:B4:C5:D6:E7:\
F8:09:1A:2B:3C:4D:5E:6F:70:81:92:A3:B4:C5:D6:E7\r\n\
a=setup:actpass\r\n\
a=mid:0\r\n\
a=recvonly\r\n\
a=rtpmap:111 opus/48000/2\r\n";
    let transport = Str0mWhepTransport::new();
    let media = FakeMediaSource::new(PreviewCodec::H264, 2);
    let err = transport.accept(AUDIO_ONLY, PreviewCodec::H264, &media);
    assert!(err.is_err(), "audio-only offer has no preview video codec");
}

#[test]
fn parse_answer_attributes_extracts_the_ice_dtls_lines() {
    // The pure SDP-munging seam: given an SDP answer string (the shape str0m
    // produces), extract exactly the ICE ufrag/pwd, the DTLS fingerprint
    // (algorithm + colon-hex value), the setup role, and the candidate lines.
    const ANSWER: &str = "v=0\r\n\
o=- 0 0 IN IP4 0.0.0.0\r\n\
s=-\r\n\
t=0 0\r\n\
m=video 9 UDP/TLS/RTP/SAVPF 96\r\n\
c=IN IP4 0.0.0.0\r\n\
a=ice-ufrag:Sv3R\r\n\
a=ice-pwd:serverPasswordValue0123456789ab\r\n\
a=fingerprint:sha-256 AA:BB:CC:DD\r\n\
a=setup:passive\r\n\
a=candidate:1 1 udp 2122260223 192.0.2.1 50000 typ host\r\n\
a=candidate:2 1 udp 2122194687 192.0.2.2 50001 typ host\r\n\
a=rtpmap:96 H264/90000\r\n\
a=sendonly\r\n";
    let attrs = parse_answer_attributes(ANSWER).expect("answer is parseable");
    assert_eq!(attrs.ice_ufrag, "Sv3R");
    assert_eq!(attrs.ice_pwd, "serverPasswordValue0123456789ab");
    assert_eq!(attrs.fingerprint.algorithm, "sha-256");
    assert_eq!(attrs.fingerprint.value, "AA:BB:CC:DD");
    assert_eq!(attrs.setup.as_str(), "passive");
    assert_eq!(attrs.candidates.len(), 2);
    assert_eq!(
        attrs.candidates[0],
        "1 1 udp 2122260223 192.0.2.1 50000 typ host"
    );
}

#[test]
fn parse_answer_attributes_rejects_an_answer_missing_ice() {
    // An answer with no ICE credentials cannot establish a session — reject it
    // (a MalformedOffer-class transport error), never silently produce a
    // placeholder.
    const NO_ICE: &str = "v=0\r\n\
o=- 0 0 IN IP4 0.0.0.0\r\n\
s=-\r\n\
t=0 0\r\n\
m=video 9 UDP/TLS/RTP/SAVPF 96\r\n\
a=fingerprint:sha-256 AA:BB\r\n\
a=setup:passive\r\n";
    assert!(parse_answer_attributes(NO_ICE).is_err());
}

/// Loopback socket bring-up (env-gated). HONEST SCOPE: this verifies only that a
/// socket-bound native transport **binds** a loopback UDP socket and **gathers at
/// least one host candidate** when accepting an offer — it does NOT complete a
/// DTLS handshake, does NOT exchange SRTP, and does NOT call `drive_egress_once`
/// (all of which need a real peer). The full handshake + SampleFeed→SRTP egress +
/// ffprobe check are the remaining slice (PRV-1c).
///
/// `#[ignore]`d by default and additionally gated on `MULTIVIEW_WHEP_LOOPBACK=1`
/// so it never runs in CI. Run it explicitly with:
///
/// ```text
/// MULTIVIEW_WHEP_LOOPBACK=1 cargo test -p multiview-preview \
///   --features webrtc-native -- --ignored native_loopback_dtls_srtp
/// ```
#[test]
#[ignore = "needs a real UDP socket + DTLS peer; set MULTIVIEW_WHEP_LOOPBACK=1"]
fn native_loopback_dtls_srtp() {
    if std::env::var("MULTIVIEW_WHEP_LOOPBACK").ok().as_deref() != Some("1") {
        // Not requested: do nothing (the #[ignore] already excludes CI; this is
        // the belt-and-braces env gate so an explicit `--ignored` run on a box
        // without loopback intent is still a no-op).
        return;
    }
    // Drive the native transport's UDP egress against a local loopback receiver,
    // pushing one keyframe sample through the bounded feed and confirming the
    // transport binds a socket and gathers at least one host candidate.
    let transport = Str0mWhepTransport::bind_loopback().expect("bind a loopback socket");
    let media = FakeMediaSource::new(PreviewCodec::H264, 2);
    let ta = transport
        .accept(BROWSER_OFFER, PreviewCodec::H264, &media)
        .expect("accept over a bound socket");
    assert!(
        !ta.candidates.is_empty(),
        "a bound transport gathers at least one host candidate"
    );
    let _ = media.sink.push(EncodedSample {
        data: Arc::from([0u8, 1, 2, 3].as_slice()),
        rtp_timestamp: 0,
        keyframe: true,
    });
    transport.close(&ta.session_id).expect("close");
}
