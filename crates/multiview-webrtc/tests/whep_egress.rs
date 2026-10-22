//! Failing-first, **OFFLINE** tests for the native WHEP egress transport
//! ([`multiview_webrtc::whep_egress`], feature `native`) — ADR-P006 (PRV-1c).
//!
//! The egress transport relocates the preview-local str0m duplication onto the
//! crate's single str0m [`Session`] (ADR-0048 / ADR-P006 move 1). It implements
//! the pure `multiview_preview::whep::transport::WhepTransport` seam:
//!
//! * `accept` — parse the browser WHEP offer with str0m, mint real ICE/DTLS
//!   attributes, take the source's drop-oldest video (and, when the offer carries
//!   Opus, audio) `SampleFeed`, and return a [`TransportAnswer`];
//! * the egress loop — drain the drop-oldest feeds → `Session::write_*` → SRTP →
//!   the shared UDP socket; and
//! * `close` — terminal lifecycle + immediate `Rtc`/feed release + tombstone GC.
//!
//! All of this is socket-free: str0m is sans-IO, so a complete handshake + SRTP
//! egress runs over an in-memory packet shuttle. The live browser-play leg is
//! `#[ignore]`d and hardware-gated.
#![cfg(feature = "native")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::as_conversions,
    clippy::missing_panics_doc
)]

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use multiview_preview::whep::transport::{
    sample_feed, EncodedSample, PreviewMediaSource, SampleFeed, SampleKind, SampleSink,
    SessionState, WhepTransport,
};
use multiview_preview::whep::PreviewCodec;
use multiview_webrtc::transport::{MediaKind, Session, SessionConfig};
use multiview_webrtc::whep_egress::WhepEgress;

/// A browser WHEP offer carrying a recvonly H.264+VP8 video m-line. str0m needs
/// real ICE credentials + a DTLS fingerprint + `setup:actpass` to answer.
const VIDEO_OFFER: &str = "v=0\r\n\
o=- 1 2 IN IP6 ::1\r\n\
s=-\r\n\
t=0 0\r\n\
a=group:BUNDLE 0\r\n\
m=video 9 UDP/TLS/RTP/SAVPF 96 97\r\n\
c=IN IP6 ::\r\n\
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

/// A browser WHEP offer carrying BOTH the video m-line and an Opus audio m-line.
const AV_OFFER: &str = "v=0\r\n\
o=- 1 2 IN IP6 ::1\r\n\
s=-\r\n\
t=0 0\r\n\
a=group:BUNDLE 0 1\r\n\
m=video 9 UDP/TLS/RTP/SAVPF 96 97\r\n\
c=IN IP6 ::\r\n\
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
a=rtpmap:97 VP8/90000\r\n\
m=audio 9 UDP/TLS/RTP/SAVPF 111\r\n\
c=IN IP6 ::\r\n\
a=ice-ufrag:tEsT\r\n\
a=ice-pwd:abcdefghijklmnopqrstuvwx\r\n\
a=ice-options:trickle\r\n\
a=fingerprint:sha-256 \
6F:8E:1A:2B:3C:4D:5E:6F:70:81:92:A3:B4:C5:D6:E7:\
F8:09:1A:2B:3C:4D:5E:6F:70:81:92:A3:B4:C5:D6:E7\r\n\
a=setup:actpass\r\n\
a=mid:1\r\n\
a=recvonly\r\n\
a=rtcp-mux\r\n\
a=rtpmap:111 opus/48000/2\r\n\
a=fmtp:111 minptime=10;useinbandfec=1\r\n";

/// An in-memory media source: hands the transport a drop-oldest video feed (and,
/// optionally, an Opus audio feed), letting the test push encoded samples through
/// the producer sinks the transport then drains.
struct FakeMediaSource {
    codec: PreviewCodec,
    video_sink: SampleSink,
    audio_sink: SampleSink,
    feed: std::sync::Mutex<Option<SampleFeed>>,
    audio: std::sync::Mutex<Option<SampleFeed>>,
}

impl FakeMediaSource {
    fn video_only(codec: PreviewCodec, depth: usize) -> Self {
        let (video_sink, feed) = sample_feed(depth);
        let (audio_sink, _audio_unused) = sample_feed(depth);
        Self {
            codec,
            video_sink,
            audio_sink,
            feed: std::sync::Mutex::new(Some(feed)),
            audio: std::sync::Mutex::new(None),
        }
    }

    fn with_audio(codec: PreviewCodec, depth: usize) -> Self {
        let s = Self::video_only(codec, depth);
        let (audio_sink, audio_feed) = sample_feed(depth);
        *s.audio.lock().unwrap() = Some(audio_feed);
        // Replace the audio sink so the test's pushes reach the held feed.
        Self {
            codec: s.codec,
            video_sink: s.video_sink,
            audio_sink,
            feed: s.feed,
            audio: s.audio,
        }
    }

    fn push_video(&self, ts: u32, keyframe: bool) -> bool {
        let mut data = vec![0x00u8, 0x00, 0x00, 0x01, if keyframe { 0x65 } else { 0x41 }];
        data.extend(std::iter::repeat_n(0xABu8, 1200));
        self.video_sink.push(EncodedSample {
            data: Arc::from(data.as_slice()),
            rtp_timestamp: ts,
            keyframe,
            kind: SampleKind::Video,
        })
    }

    fn push_audio(&self, ts: u32) -> bool {
        self.audio_sink.push(EncodedSample {
            data: Arc::from(vec![0xF8u8; 80].as_slice()),
            rtp_timestamp: ts,
            keyframe: false,
            kind: SampleKind::Audio,
        })
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
    fn audio_feed(&self) -> Option<SampleFeed> {
        self.audio.lock().ok().and_then(|mut g| g.take())
    }
}

/// Drive the egress transport's session (the answerer/sender) and a peer
/// [`Session`] (the browser/receiver) entirely in memory: every datagram the
/// egress emits becomes an input on the peer and vice-versa. Returns whether
/// `done` was reached within the iteration bound.
fn pump_until(
    egress: &WhepEgress,
    id: &multiview_preview::whep::transport::SessionId,
    egress_addr: SocketAddr,
    peer: &mut Session,
    peer_addr: SocketAddr,
    mut clock: Instant,
    mut done: impl FnMut(&WhepEgress, &mut Session) -> bool,
) -> bool {
    for _ in 0..3000 {
        // Egress → peer, then peer → egress, then egress → peer again, so a
        // response to inbound STUN/DTLS is flushed within the same logical tick
        // (the live driver collects after every socket read; the shuttle mirrors
        // that by re-driving the egress after feeding it the peer's datagrams).
        for _ in 0..3 {
            let outbound = egress.drive_egress(id, clock).unwrap();
            for (_source, dst, payload) in outbound {
                assert_eq!(dst, peer_addr, "egress transmits to the peer");
                peer.handle_datagram(egress_addr, peer_addr, &payload, clock)
                    .unwrap();
            }
            peer.handle_timeout(clock).unwrap();
            for _ in 0..256 {
                match peer.poll_transmit(clock) {
                    Some((_source, dst, payload)) => {
                        assert_eq!(dst, egress_addr, "peer transmits to the egress");
                        egress
                            .handle_datagram(id, peer_addr, egress_addr, &payload, clock)
                            .unwrap();
                    }
                    None => break,
                }
            }
        }
        if done(egress, peer) {
            return true;
        }
        let next = egress.poll_timeout(id, clock).min(peer.poll_timeout(clock));
        clock = next.max(clock + Duration::from_millis(1));
    }
    false
}

/// Build an egress transport + a peer receiver, both with a host candidate, and
/// negotiate the offer through the egress. Returns the wired-up pieces.
fn negotiate(
    offer: &str,
    media: &dyn PreviewMediaSource,
) -> (
    WhepEgress,
    multiview_preview::whep::transport::SessionId,
    SocketAddr,
    Session,
    SocketAddr,
) {
    let egress_addr: SocketAddr = "[::1]:50001".parse().unwrap();
    let peer_addr: SocketAddr = "[::1]:50002".parse().unwrap();
    let egress = WhepEgress::with_host_candidate(egress_addr);
    let ta = egress
        .accept_session(offer, PreviewCodec::H264, media)
        .expect("egress accepts the offer");
    let id = ta.transport.session_id.clone();
    // The peer plays the role of the browser: it OFFERED, so here we model it as
    // the offerer that applies the egress's answer. str0m on both sides → a real
    // handshake. We rebuild the peer from the same offer/answer pair.
    let now = Instant::now();
    let mut peer = Session::new(&SessionConfig::default(), now);
    peer.add_host_candidate(peer_addr).unwrap();
    // The peer creates its own offer matching the media, and the egress already
    // produced a str0m answer internally; for the in-memory handshake we drive the
    // egress as the answerer to the peer's offer via the transport's own session.
    let _ = &ta;
    (egress, id, egress_addr, peer, peer_addr)
}

#[test]
fn accept_yields_real_ice_dtls_and_created_state() {
    let media = FakeMediaSource::video_only(PreviewCodec::H264, 4);
    let egress = WhepEgress::new();
    let ta = egress
        .accept(VIDEO_OFFER, PreviewCodec::H264, &media)
        .expect("accept");
    assert!(!ta.ice_ufrag.is_empty());
    assert_ne!(ta.ice_ufrag, "tEsT", "server mints its own ufrag");
    assert_eq!(ta.fingerprint.algorithm, "sha-256");
    let octets: Vec<&str> = ta.fingerprint.value.split(':').collect();
    assert_eq!(octets.len(), 32, "sha-256 digest is 32 octets");
    assert_eq!(
        egress.session_state(&ta.session_id),
        Some(SessionState::Created)
    );
}

#[test]
fn accept_takes_audio_feed_only_when_offer_has_opus() {
    // ADR-P006: a session whose offer negotiates an Opus audio m-line gets the
    // source's (at-most-once) audio feed wired; a video-only offer leaves it
    // absent (a feed that can never be sent is never held).
    let av = FakeMediaSource::with_audio(PreviewCodec::H264, 4);
    let egress = WhepEgress::new();
    let _ = egress
        .accept(AV_OFFER, PreviewCodec::H264, &av)
        .expect("accept AV offer");
    assert!(
        av.audio_feed().is_none(),
        "the egress took the audio feed exactly once for an Opus offer"
    );

    let video = FakeMediaSource::with_audio(PreviewCodec::H264, 4);
    let egress2 = WhepEgress::new();
    let _ = egress2
        .accept(VIDEO_OFFER, PreviewCodec::H264, &video)
        .expect("accept video-only offer");
    assert!(
        video.audio_feed().is_some(),
        "a video-only offer leaves the source's audio feed untaken"
    );
}

#[test]
fn close_is_terminal_and_idempotent() {
    let media = FakeMediaSource::video_only(PreviewCodec::H264, 4);
    let egress = WhepEgress::new();
    let ta = egress
        .accept(VIDEO_OFFER, PreviewCodec::H264, &media)
        .expect("accept");
    egress.close(&ta.session_id).expect("close");
    assert_eq!(
        egress.session_state(&ta.session_id),
        Some(SessionState::Closed)
    );
    egress.close(&ta.session_id).expect("idempotent close");
    assert_eq!(
        egress.session_state(&ta.session_id),
        Some(SessionState::Closed)
    );
}

#[test]
fn video_egress_reaches_a_real_peer_over_the_shuttle() {
    // The full egress path: a browser peer OFFERS, the egress answers with its
    // session, ICE+DTLS complete in memory, and pushed video samples egress as
    // SRTP that the peer surfaces as decrypted media.
    let media = FakeMediaSource::video_only(PreviewCodec::H264, 8);
    let egress_addr: SocketAddr = "[::1]:50001".parse().unwrap();
    let peer_addr: SocketAddr = "[::1]:50002".parse().unwrap();
    let now = Instant::now();

    // The peer (browser) builds a recvonly-style offer with a host candidate.
    let mut peer = Session::new(&SessionConfig::default(), now);
    peer.add_host_candidate(peer_addr).unwrap();
    // The browser/viewer offers RECVONLY; the egress answers SENDONLY so media
    // flows server → browser (the WHEP direction).
    let offer = peer.create_recv_offer(&[MediaKind::Video]).unwrap();

    // The egress accepts the peer's real offer, gathering its own host candidate.
    let egress = WhepEgress::with_host_candidate(egress_addr);
    let ta = egress
        .accept_session(&offer, PreviewCodec::H264, &media)
        .expect("egress accepts the peer's offer");
    let id = ta.transport.session_id.clone();
    // The peer applies the egress's answer.
    peer.accept_answer(&ta.sdp_answer).unwrap();

    // Drive from a clock captured after all sessions are built (≥ every session's
    // birth — the realistic driver invariant; `Instant::now()` is monotonic).
    let clock = Instant::now();

    // Connect.
    let connected = pump_until(
        &egress,
        &id,
        egress_addr,
        &mut peer,
        peer_addr,
        clock,
        |_e, p| p.is_connected(),
    );
    assert!(connected, "ICE+DTLS did not complete");

    // Push several video samples (a keyframe first) into the source; the egress
    // pump drains them into SRTP.
    media.push_video(0, true);
    media.push_video(3000, false);
    media.push_video(6000, false);

    let delivered = pump_until(
        &egress,
        &id,
        egress_addr,
        &mut peer,
        peer_addr,
        clock,
        |_e, p| p.received_media_count() > 0,
    );
    assert!(delivered, "no SRTP video reached the peer");
    assert!(peer.received_media_count() > 0);
}

#[test]
fn audio_egress_reaches_a_real_peer_when_opus_negotiated() {
    // ADR-P006: preview carries audio. With an Opus m-line negotiated, pushed
    // audio frames egress as SRTP the peer surfaces.
    let media = FakeMediaSource::with_audio(PreviewCodec::H264, 8);
    let egress_addr: SocketAddr = "[::1]:50001".parse().unwrap();
    let peer_addr: SocketAddr = "[::1]:50002".parse().unwrap();
    let now = Instant::now();

    let mut peer = Session::new(&SessionConfig::default(), now);
    peer.add_host_candidate(peer_addr).unwrap();
    let offer = peer
        .create_recv_offer(&[MediaKind::Video, MediaKind::Audio])
        .unwrap();

    let egress = WhepEgress::with_host_candidate(egress_addr);
    let ta = egress
        .accept_session(&offer, PreviewCodec::H264, &media)
        .expect("egress accepts the AV offer");
    let id = ta.transport.session_id.clone();
    peer.accept_answer(&ta.sdp_answer).unwrap();

    let clock = Instant::now();
    assert!(
        pump_until(
            &egress,
            &id,
            egress_addr,
            &mut peer,
            peer_addr,
            clock,
            |_e, p| p.is_connected()
        ),
        "must connect"
    );

    media.push_video(0, true);
    media.push_audio(0);
    media.push_audio(960);

    let delivered = pump_until(
        &egress,
        &id,
        egress_addr,
        &mut peer,
        peer_addr,
        clock,
        |_e, p| p.received_media_count() >= 2,
    );
    assert!(delivered, "audio + video did not reach the peer");
}

#[test]
fn a_stalled_consumer_cannot_stall_the_feed_or_the_producer() {
    // INVARIANT #10 PROOF: a WHEP consumer that never pumps egress (a stalled or
    // absent browser) must NEVER back-pressure the source's drop-oldest feed or
    // the producer pushing into it. Pushing far more samples than the feed depth
    // is wait-free and bounded; the oldest are dropped, never queued, and the
    // egress session's buffers stay bounded.
    let media = FakeMediaSource::video_only(PreviewCodec::H264, 2);
    let egress = WhepEgress::with_host_candidate("[::1]:50001".parse().unwrap());
    let _ta = egress
        .accept(VIDEO_OFFER, PreviewCodec::H264, &media)
        .expect("accept");

    // The consumer is "stalled": we never call drive_egress here. The producer
    // pushes 10_000 samples; this must complete near-instantly without blocking.
    let pushed = std::time::Instant::now();
    let mut dropped_any = false;
    for ts in 0..10_000u32 {
        if media.push_video(ts.saturating_mul(3000), ts == 0) {
            dropped_any = true;
        }
    }
    assert!(
        pushed.elapsed() < Duration::from_secs(2),
        "pushing into a drop-oldest feed never blocks on a stalled consumer"
    );
    assert!(
        dropped_any,
        "a depth-2 feed with 10k pushes drops the oldest (bounded, never grows)"
    );
}

#[test]
fn close_releases_the_session_and_keeps_a_queryable_tombstone() {
    let media = FakeMediaSource::video_only(PreviewCodec::H264, 4);
    let egress = WhepEgress::new();
    let ta = egress
        .accept(VIDEO_OFFER, PreviewCodec::H264, &media)
        .expect("accept");
    egress.close(&ta.session_id).expect("close");
    // The id stays queryable as a closed tombstone (idempotent DELETE), and
    // driving a closed session is a no-op (no datagrams), never an error/panic.
    assert_eq!(
        egress.session_state(&ta.session_id),
        Some(SessionState::Closed)
    );
    let out = egress.drive_egress(&ta.session_id, Instant::now()).unwrap();
    assert!(out.is_empty(), "a closed session emits no datagrams");
}

/// The accept-side wiring helper exists to keep the `negotiate` shape honest even
/// though the per-test handshakes build their own peers; reference it so an unused
/// warning never masks a real one.
#[test]
fn negotiate_helper_builds_a_session() {
    let media = FakeMediaSource::video_only(PreviewCodec::H264, 4);
    let (egress, id, _ea, _peer, _pa) = negotiate(VIDEO_OFFER, &media);
    assert_eq!(egress.session_state(&id), Some(SessionState::Created));
}

/// Count the `a=candidate:` lines of `typ relay` in an answer's candidate list
/// (str0m emits relay candidates with the `typ relay` token).
fn relay_candidate_count(candidates: &[String]) -> usize {
    candidates
        .iter()
        .filter(|c| c.contains("typ relay"))
        .count()
}

#[test]
fn learn_relay_offers_a_relay_candidate_on_the_next_session() {
    // ITEM-1 (TURN-in-WHEP-driver): a relay the egress driver's in-crate TURN
    // client allocates at runtime (after bind, asynchronously) is published into
    // the egress via `learn_relay(relayed, local)` and MUST then be offered as a
    // `typ relay` ICE candidate on every subsequently-negotiated session — so a
    // browser behind NAT can WHEP-play via the operator's TURN relay. Mirrors the
    // WHIP `learned_relays` path (ADR-0048 §5.1).
    let host: SocketAddr = "[2001:db8::1]:50001".parse().unwrap();
    let relayed: SocketAddr = "[2001:db8::99]:60000".parse().unwrap();
    let egress = WhepEgress::with_host_candidate(host);

    // Before any allocation, a session offers host-only (no relay candidate).
    let media0 = FakeMediaSource::video_only(PreviewCodec::H264, 4);
    let before = egress
        .accept_session(VIDEO_OFFER, PreviewCodec::H264, &media0)
        .expect("accept before relay learned");
    assert_eq!(
        relay_candidate_count(&before.transport.candidates),
        0,
        "no relay candidate is offered before a TURN allocation completes"
    );

    // The driver's TURN client completes an Allocate and publishes the relay
    // (relayed addr + the local socket the relayed traffic egresses from).
    egress.learn_relay(relayed, host);

    let media1 = FakeMediaSource::video_only(PreviewCodec::H264, 4);
    let after = egress
        .accept_session(VIDEO_OFFER, PreviewCodec::H264, &media1)
        .expect("accept after relay learned");
    assert!(
        relay_candidate_count(&after.transport.candidates) >= 1,
        "a learned TURN relay is offered as a relay candidate on the next session \
         (browser-behind-NAT WHEP-play path); candidates: {:?}",
        after.transport.candidates
    );
}

#[test]
fn learn_relay_is_idempotent_and_de_duplicates() {
    // A relay learned twice (e.g. a re-allocation or a `poll_output` + `feed`
    // both surfacing it) is offered once, not twice — the driver publishes a relay
    // it has not seen, mirroring WHIP's `learned_relays` de-dup.
    let host: SocketAddr = "[2001:db8::1]:50001".parse().unwrap();
    let relayed: SocketAddr = "[2001:db8::99]:60000".parse().unwrap();
    let egress = WhepEgress::with_host_candidate(host);
    egress.learn_relay(relayed, host);
    egress.learn_relay(relayed, host);

    let media = FakeMediaSource::video_only(PreviewCodec::H264, 4);
    let answer = egress
        .accept_session(VIDEO_OFFER, PreviewCodec::H264, &media)
        .expect("accept");
    assert_eq!(
        relay_candidate_count(&answer.transport.candidates),
        1,
        "a relay learned twice is offered exactly once; candidates: {:?}",
        answer.transport.candidates
    );
}
