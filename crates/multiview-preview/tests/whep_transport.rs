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
    SampleKind, SampleSink, SessionHandle, SessionId, SessionState, TransportAnswer, WhepTransport,
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

/// An in-memory media source: hands out the producer ends so the test can feed
/// samples, and the consumer ends (the `SampleFeed`s) to the transport. Audio
/// (Opus by definition on this seam, ADR-P006) is optional — `new` builds a
/// video-only source whose `audio_feed` is `None`; `with_audio` adds an Opus
/// feed handed out at most once, mirroring the `feed()` take-once contract.
struct FakeMediaSource {
    codec: PreviewCodec,
    sink: SampleSink,
    feed: std::sync::Mutex<Option<SampleFeed>>,
    audio_sink: Option<SampleSink>,
    audio: std::sync::Mutex<Option<SampleFeed>>,
}

impl FakeMediaSource {
    fn new(codec: PreviewCodec, depth: usize) -> Self {
        let (sink, feed) = sample_feed(depth);
        Self {
            codec,
            sink,
            feed: std::sync::Mutex::new(Some(feed)),
            audio_sink: None,
            audio: std::sync::Mutex::new(None),
        }
    }

    fn with_audio(codec: PreviewCodec, depth: usize, audio_depth: usize) -> Self {
        let mut source = Self::new(codec, depth);
        let (audio_sink, audio_feed) = sample_feed(audio_depth);
        source.audio_sink = Some(audio_sink);
        source.audio = std::sync::Mutex::new(Some(audio_feed));
        source
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
    fn audio_feed(&self) -> Option<SampleFeed> {
        // At most once, like `feed()`: once taken, audio reads as absent.
        self.audio.lock().ok().and_then(|mut g| g.take())
    }
}

/// An in-memory transport: assigns deterministic, **non-placeholder** ICE/DTLS
/// attributes and tracks the per-session lifecycle. It holds only the media
/// `SampleFeed` (drop-oldest) — never an engine handle.
struct FakeTransport {
    handle: std::sync::Mutex<Option<SessionHandle>>,
    // The feeds the transport drained from the media source, kept to prove the
    // transport reads samples (and holds nothing the engine awaits).
    held_feed: std::sync::Mutex<Option<SampleFeed>>,
    held_audio_feed: std::sync::Mutex<Option<SampleFeed>>,
}

impl FakeTransport {
    fn new() -> Self {
        Self {
            handle: std::sync::Mutex::new(None),
            held_feed: std::sync::Mutex::new(None),
            held_audio_feed: std::sync::Mutex::new(None),
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
    fn held_audio_feed_buffered(&self) -> usize {
        self.held_audio_feed
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
        // The audio feed is optional (ADR-P006): the fake takes it when the
        // source has one, at most once, exactly like the video feed.
        *self.held_audio_feed.lock().unwrap() = media.audio_feed();
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
        kind: SampleKind::Video,
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
            kind: SampleKind::Video,
        });
        assert!(!lagging, "depth-3 ring with 2 samples never evicts");
    }
    assert_eq!(
        transport.held_feed_buffered(),
        2,
        "transport reads the lossy media feed and nothing else"
    );
}

#[test]
fn sample_kind_pins_the_per_kind_rtp_clocks() {
    // ADR-P006 move 3: video samples ride the 90 kHz RTP clock every video
    // payload advertises; audio (Opus by definition on this seam) rides the
    // 48 kHz clock RFC 7587 fixes for Opus.
    assert_eq!(SampleKind::Video.rtp_clock_hz(), 90_000);
    assert_eq!(SampleKind::Audio.rtp_clock_hz(), 48_000);
}

#[test]
fn one_feed_carries_video_and_audio_samples_in_order() {
    // The audio seam reuses the proven drop-oldest feed (ADR-P006 rejected a
    // parallel AudioFeed): one ring carries both kinds, tagged per sample, and
    // pops in push order.
    let (sink, feed) = sample_feed(4);
    let push = |kind: SampleKind, ts: u32, key: bool| {
        let _ = sink.push(EncodedSample {
            data: Arc::from([0u8].as_slice()),
            rtp_timestamp: ts,
            keyframe: key,
            kind,
        });
    };
    push(SampleKind::Video, 0, true);
    push(SampleKind::Audio, 960, false); // one 20 ms Opus frame at 48 kHz
    push(SampleKind::Video, 3_000, false); // one 30 fps video frame at 90 kHz

    let first = feed.pop().expect("first sample");
    assert_eq!(first.kind, SampleKind::Video);
    assert!(first.keyframe);
    let second = feed.pop().expect("second sample");
    assert_eq!(second.kind, SampleKind::Audio);
    assert_eq!(
        second.rtp_timestamp, 960,
        "audio timestamps are 48 kHz units"
    );
    let third = feed.pop().expect("third sample");
    assert_eq!(third.kind, SampleKind::Video);
    assert_eq!(third.rtp_timestamp, 3_000);
    assert!(feed.pop().is_none());
}

#[test]
fn audio_feed_defaults_to_absent() {
    // ADR-P006: the audio feed is OPTIONAL per source — scopes with no audio
    // source simply leave it absent — so the trait defaults to `None` and a
    // video-only source needs no override.
    struct VideoOnly;
    impl PreviewMediaSource for VideoOnly {
        fn codec(&self) -> PreviewCodec {
            PreviewCodec::H264
        }
        fn feed(&self) -> SampleFeed {
            sample_feed(1).1
        }
    }
    assert!(
        VideoOnly.audio_feed().is_none(),
        "the default audio feed is absent"
    );
}

#[test]
fn transport_takes_the_audio_feed_at_most_once_when_present() {
    // ADR-P006: `audio_feed` is called at most once per session, like `feed()`.
    // The fake transport takes it at accept; the producer can then push Opus
    // samples (48 kHz clock) through the same bounded drop-oldest seam.
    let transport = FakeTransport::new();
    let media = FakeMediaSource::with_audio(PreviewCodec::H264, 2, 3);
    transport.accept(OFFER, PreviewCodec::H264, &media).unwrap();

    assert!(
        media.audio_feed().is_none(),
        "the transport took the audio feed exactly once"
    );
    let lagging = media.audio_sink.as_ref().unwrap().push(EncodedSample {
        data: Arc::from([7u8].as_slice()),
        rtp_timestamp: 960, // one 20 ms Opus frame at 48 kHz
        keyframe: false,
        kind: SampleKind::Audio,
    });
    assert!(!lagging, "depth-3 audio ring with 1 sample never evicts");
    assert_eq!(
        transport.held_audio_feed_buffered(),
        1,
        "the transport drains the held audio feed"
    );
}

#[test]
fn folded_answer_stays_ipv6_first_and_bundled() {
    // ADR-P006 move 2 + ADR-0042: the transport-folded answer keeps the honest
    // browser shape — IPv6-first c= lines (never IN IP4), session-level
    // BUNDLE, per-media mid, rtcp-mux — alongside the real ICE/DTLS lines.
    let session = WhepSession::negotiate(OFFER, AccessScope::Focus).expect("focus offer");
    let transport = FakeTransport::new();
    let media = FakeMediaSource::new(PreviewCodec::H264, 2);
    let ta = transport
        .accept(OFFER, session.codec(), &media)
        .expect("transport accepts");
    let answer = session.build_answer(&ta);
    assert!(answer.contains("c=IN IP6 ::\r\n"), "answer: {answer}");
    assert!(
        !answer.contains("IN IP4"),
        "never IN IP4 (ADR-0042): {answer}"
    );
    assert!(answer.contains("a=group:BUNDLE 0\r\n"), "answer: {answer}");
    assert!(answer.contains("a=mid:0\r\n"), "answer: {answer}");
    assert!(answer.contains("a=rtcp-mux\r\n"), "answer: {answer}");
}
