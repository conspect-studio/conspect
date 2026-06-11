//! The WHEP **transport seam** (behind the off-by-default `webrtc` feature): the
//! `WhepTransport` trait + SDP offer/answer glue + session lifecycle + the
//! bounded, drop-oldest sample feed.
//!
//! These cover the **seam**, exercised with an in-memory fake transport — no
//! socket, no UDP/STUN, no DTLS certificate, so they run in CI. The **live**
//! str0m ICE/DTLS/SRTP path needs real network reachability + certificates and
//! lives behind a further build gate + an env-gated loopback test (see PRV-1
//! notes); it is intentionally not exercised here.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
#![cfg(feature = "webrtc")]

use std::sync::Arc;

use multiview_preview::whep::transport::{
    sample_feed, DtlsFingerprint, DtlsSetup, EncodedSample, PreviewMediaSource, SampleFeed,
    SampleSink, SessionHandle, SessionId, SessionState, TransportAnswer, WhepTransport,
};
use multiview_preview::whep::{PreviewCodec, WhepSession};
use multiview_preview::AccessScope;

const OFFER: &str = "v=0\r\n\
o=- 0 0 IN IP4 0.0.0.0\r\n\
s=-\r\n\
t=0 0\r\n\
m=video 9 UDP/TLS/RTP/SAVPF 96 97\r\n\
c=IN IP4 0.0.0.0\r\n\
a=rtpmap:96 H264/90000\r\n\
a=rtpmap:97 VP8/90000\r\n\
a=sendrecv\r\n";

/// An in-memory media source: hands out the producer end so the test can feed
/// samples, and the consumer end (the `SampleFeed`) to the transport.
struct FakeMediaSource {
    codec: PreviewCodec,
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
        // The transport takes the feed exactly once; subsequent calls would be a
        // programming error, but for the fake we just create a fresh empty one.
        self.feed
            .lock()
            .ok()
            .and_then(|mut g| g.take())
            .unwrap_or_else(|| sample_feed(1).1)
    }
}

/// An in-memory transport: assigns deterministic, **non-placeholder** ICE/DTLS
/// attributes and tracks the per-session lifecycle. It holds only the media
/// `SampleFeed` (drop-oldest) — never an engine handle.
struct FakeTransport {
    handle: std::sync::Mutex<Option<SessionHandle>>,
    // The feed the transport drained from the media source, kept to prove the
    // transport reads samples (and holds nothing the engine awaits).
    held_feed: std::sync::Mutex<Option<SampleFeed>>,
}

impl FakeTransport {
    fn new() -> Self {
        Self {
            handle: std::sync::Mutex::new(None),
            held_feed: std::sync::Mutex::new(None),
        }
    }
    fn handle(&self) -> SessionHandle {
        self.handle
            .lock()
            .unwrap()
            .clone()
            .expect("accept was called")
    }
    fn held_feed_buffered(&self) -> usize {
        self.held_feed
            .lock()
            .unwrap()
            .as_ref()
            .map_or(0, SampleFeed::buffered)
    }
}

impl WhepTransport for FakeTransport {
    fn accept(
        &self,
        offer: &str,
        _codec: PreviewCodec,
        media: &dyn PreviewMediaSource,
    ) -> Result<TransportAnswer, multiview_preview::whep::WhepError> {
        assert!(offer.contains("v=0"), "transport sees the offer");
        let id = SessionId::new("fake-session-1");
        let handle = SessionHandle::new(id.clone());
        *self.handle.lock().unwrap() = Some(handle);
        *self.held_feed.lock().unwrap() = Some(media.feed());
        Ok(TransportAnswer {
            session_id: id,
            ice_ufrag: "Fk9aZ".to_owned(),
            ice_pwd: "s3cretIcePasswordValue0123456789".to_owned(),
            fingerprint: DtlsFingerprint {
                algorithm: "sha-256".to_owned(),
                value: "AB:CD:EF:01:23:45:67:89:AB:CD:EF:01:23:45:67:89:\
                        AB:CD:EF:01:23:45:67:89:AB:CD:EF:01:23:45:67:89"
                    .to_owned(),
            },
            setup: DtlsSetup::Passive,
            candidates: vec!["1 1 UDP 2122260223 192.0.2.1 50000 typ host".to_owned()],
        })
    }

    fn close(&self, id: &SessionId) -> Result<(), multiview_preview::whep::WhepError> {
        if let Some(h) = self.handle.lock().unwrap().as_ref() {
            if h.id() == id {
                h.close();
            }
        }
        Ok(())
    }
}

#[test]
fn negotiate_then_accept_yields_non_placeholder_answer() {
    // (a): negotiate selects the codec (pure), the transport supplies real
    // ICE/DTLS attributes, and the assembled answer carries them — NOT the
    // 0.0.0.0 / no-ICE placeholders of the codec-only scaffold.
    let session = WhepSession::negotiate(OFFER, AccessScope::Focus).expect("focus offer");
    assert_eq!(session.codec(), PreviewCodec::H264);

    let transport = FakeTransport::new();
    let media = FakeMediaSource::new(PreviewCodec::H264, 2);
    let ta = transport
        .accept(OFFER, session.codec(), &media)
        .expect("transport accepts");

    let answer = session.build_answer(&ta);

    // Codec negotiation is preserved.
    assert!(
        answer.contains("a=rtpmap:96 H264/90000"),
        "answer: {answer}"
    );
    assert!(answer.contains("a=sendonly"), "server sends preview");
    // Transport-supplied attributes are present and non-placeholder.
    assert!(answer.contains("a=ice-ufrag:Fk9aZ"), "ufrag: {answer}");
    assert!(
        answer.contains("a=ice-pwd:s3cretIcePasswordValue0123456789"),
        "pwd: {answer}"
    );
    assert!(
        answer.contains("a=fingerprint:sha-256 AB:CD:EF"),
        "fingerprint: {answer}"
    );
    assert!(answer.contains("a=setup:passive"), "setup: {answer}");
    assert!(
        answer.contains("a=candidate:1 1 UDP 2122260223 192.0.2.1 50000 typ host"),
        "candidate: {answer}"
    );
    // The transport-folded answer is strictly richer than the codec-only
    // scaffold `negotiate` produces: the placeholder answer has no ICE/DTLS
    // attributes at all, so it can never establish a session.
    let placeholder = session.answer_sdp();
    assert!(
        !placeholder.contains("a=ice-ufrag"),
        "scaffold: {placeholder}"
    );
    assert!(
        !placeholder.contains("a=fingerprint"),
        "scaffold: {placeholder}"
    );
    assert!(
        !placeholder.contains("a=candidate"),
        "scaffold: {placeholder}"
    );
    assert_ne!(answer, placeholder, "transport answer != placeholder");
}

#[test]
fn session_lifecycle_runs_created_to_closed() {
    // (b): create -> connecting -> connected -> closed, all driven through the
    // transport seam's lifecycle state machine.
    let transport = FakeTransport::new();
    let media = FakeMediaSource::new(PreviewCodec::H264, 2);
    let ta = transport.accept(OFFER, PreviewCodec::H264, &media).unwrap();

    let handle = transport.handle();
    assert_eq!(handle.state(), SessionState::Created);
    handle.advance_to(SessionState::Connecting).unwrap();
    handle.advance_to(SessionState::Connected).unwrap();
    assert_eq!(handle.state(), SessionState::Connected);

    transport.close(&ta.session_id).unwrap();
    assert_eq!(handle.state(), SessionState::Closed);
    assert!(handle.state().is_closed());
    // Closing again is idempotent (WHEP DELETE may be retried / RTCP-timeout).
    transport.close(&ta.session_id).unwrap();
    assert_eq!(handle.state(), SessionState::Closed);
}

#[test]
fn sample_feed_drops_oldest_never_blocks() {
    // (c) + inv #10: the encoder pushes faster than the transport drains; the
    // feed drops the OLDEST and the push call never blocks (it is synchronous
    // and returns a bool). This is the structural no-back-pressure guarantee.
    let (sink, feed) = sample_feed(2);
    let sample = |ts: u32| EncodedSample {
        data: Arc::from(ts.to_le_bytes().as_slice()),
        rtp_timestamp: ts,
        keyframe: ts == 0,
    };

    // Push 5 samples into a depth-2 ring with NO draining: the producer never
    // blocks, the ring stays bounded, and the 3 oldest are dropped.
    let mut evictions = 0u32;
    for ts in 0..5 {
        if sink.push(sample(ts)) {
            evictions += 1;
        }
    }
    assert_eq!(feed.buffered(), 2, "ring stays bounded at depth");
    assert_eq!(evictions, 3, "3 oldest evicted");
    assert_eq!(feed.dropped(), 3);
    // The two survivors are the NEWEST two (drop-oldest, not drop-newest).
    assert_eq!(feed.pop().unwrap().rtp_timestamp, 3);
    assert_eq!(feed.pop().unwrap().rtp_timestamp, 4);
}

#[test]
fn transport_holds_only_the_media_feed_not_the_engine() {
    // inv #10: after accept, the transport holds the media SampleFeed (a
    // drop-oldest preview tap) and nothing else. We feed it via the media
    // source's producer end and confirm the transport can drain it — proving
    // the only coupling is the lossy feed, never an engine publish handle.
    let transport = FakeTransport::new();
    let media = FakeMediaSource::new(PreviewCodec::H264, 3);
    transport.accept(OFFER, PreviewCodec::H264, &media).unwrap();

    // Producer pushes; this must never block even though no egress task drains.
    // Depth-3 ring, only 2 samples: no eviction, so the lag flag is false.
    for ts in 0..2 {
        let lagging = media.sink.push(EncodedSample {
            data: Arc::from([ts].as_slice()),
            rtp_timestamp: u32::from(ts),
            keyframe: ts == 0,
        });
        assert!(!lagging, "depth-3 ring with 2 samples never evicts");
    }
    assert_eq!(
        transport.held_feed_buffered(),
        2,
        "transport reads the lossy media feed and nothing else"
    );
}
