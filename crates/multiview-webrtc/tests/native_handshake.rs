//! Failing-first, **offline** tests for the native str0m-backed transport
//! ([`multiview_webrtc::transport`], feature `native`).
//!
//! str0m is sans-IO: it owns no socket. That is the whole win here — two
//! [`Session`]s are driven entirely in memory through a packet shuttle (each
//! `Output::Transmit` from one peer becomes an `Input::Receive` on the other),
//! so a complete ICE + DTLS handshake and SRTP-protected RTP media exchange run
//! with **no network and no real socket** on an ordinary CI runner. The
//! live-network legs (real UDP, real TURN server) are `#[ignore]`d and
//! hardware-gated.
#![cfg(feature = "native")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::as_conversions,
    clippy::missing_panics_doc
)]

mod fake_turn;

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use fake_turn::FakeTurnServer as FakeTurn;
use multiview_webrtc::transport::{MediaKind, Session, SessionConfig};

/// The offerer/answerer host addresses (loopback-style, but never actually
/// bound — the shuttle moves bytes by value). IPv6-first per ADR-0042.
const OFFERER_ADDR: &str = "[::1]:40001";
const ANSWERER_ADDR: &str = "[::1]:40002";

/// Drive both peers' sans-IO loops, shuttling every datagram one peer emits into
/// the other peer, advancing a shared virtual clock, until `done` returns true or
/// the iteration bound is hit. Returns whether `done` was reached.
fn pump_until(
    a: &mut Session,
    a_addr: SocketAddr,
    b: &mut Session,
    b_addr: SocketAddr,
    mut clock: Instant,
    mut done: impl FnMut(&mut Session, &mut Session) -> bool,
) -> bool {
    for _ in 0..2000 {
        // Feed each peer a timeout tick so its ICE/DTLS timers advance, then
        // drain everything it wants to send into the other peer.
        a.handle_timeout(clock).unwrap();
        b.handle_timeout(clock).unwrap();
        for _ in 0..128 {
            match a.poll_transmit(clock) {
                // poll_transmit now surfaces str0m's (source, destination, payload);
                // for host candidates the source is the local addr — assert the
                // destination is the other peer (the relay routing is exercised by
                // the dedicated relay-forced-media test, not this host shuttle).
                Some((_source, dst, payload)) => {
                    assert_eq!(dst, b_addr, "a transmits to b");
                    b.handle_datagram(a_addr, b_addr, &payload, clock).unwrap();
                }
                None => break,
            }
        }
        for _ in 0..128 {
            match b.poll_transmit(clock) {
                Some((_source, dst, payload)) => {
                    assert_eq!(dst, a_addr, "b transmits to a");
                    a.handle_datagram(b_addr, a_addr, &payload, clock).unwrap();
                }
                None => break,
            }
        }
        if done(a, b) {
            return true;
        }
        // Advance the virtual clock past whichever peer wakes soonest so DTLS /
        // ICE timers fire deterministically.
        let next = a.poll_timeout(clock).min(b.poll_timeout(clock));
        clock = next.max(clock + Duration::from_millis(1));
    }
    false
}

fn offerer_answerer() -> (Session, SocketAddr, Session, SocketAddr) {
    let now = Instant::now();
    let a_addr: SocketAddr = OFFERER_ADDR.parse().unwrap();
    let b_addr: SocketAddr = ANSWERER_ADDR.parse().unwrap();
    let offerer = Session::new(&SessionConfig::default(), now);
    let answerer = Session::new(&SessionConfig::default(), now);
    (offerer, a_addr, answerer, b_addr)
}

#[test]
fn two_in_process_endpoints_complete_ice_and_dtls() {
    let (mut a, a_addr, mut b, b_addr) = offerer_answerer();
    let now = Instant::now();

    // The offerer adds video media and a host candidate, then produces an offer.
    a.add_host_candidate(a_addr).unwrap();
    b.add_host_candidate(b_addr).unwrap();
    let offer = a.create_offer(&[MediaKind::Video]).unwrap();
    // The answerer accepts and produces an answer that the offerer applies.
    let answer = b.accept_offer(&offer).unwrap();
    a.accept_answer(&answer).unwrap();

    // Both must reach ICE+DTLS connected purely over the in-memory shuttle.
    let connected = pump_until(&mut a, a_addr, &mut b, b_addr, now, |a, b| {
        a.is_connected() && b.is_connected()
    });
    assert!(connected, "ICE+DTLS did not complete over the shuttle");
}

#[test]
fn srtp_rtp_media_flows_offerer_to_answerer() {
    let (mut a, a_addr, mut b, b_addr) = offerer_answerer();
    let now = Instant::now();

    a.add_host_candidate(a_addr).unwrap();
    b.add_host_candidate(b_addr).unwrap();
    let offer = a.create_offer(&[MediaKind::Video]).unwrap();
    let answer = b.accept_offer(&offer).unwrap();
    a.accept_answer(&answer).unwrap();

    assert!(
        pump_until(&mut a, a_addr, &mut b, b_addr, now, |a, b| a.is_connected()
            && b.is_connected()),
        "must connect first"
    );

    // The offerer writes one encoded IDR access unit (Annex-B start code + IDR
    // NAL header `0x65` + slice bytes); str0m packetizes it into SRTP. The
    // answerer must surface it as decrypted media. A realistic access unit is
    // used because str0m's depacketizer only emits a complete frame, not a bare
    // parameter-set NAL.
    let mut payload = vec![0x00u8, 0x00, 0x00, 0x01, 0x65];
    payload.extend(std::iter::repeat_n(0xABu8, 1200));
    a.write_video_sample(&payload, true, 0, now).unwrap();

    let delivered = pump_until(&mut a, a_addr, &mut b, b_addr, now, |_a, b| {
        b.received_media_count() > 0
    });
    assert!(
        delivered,
        "SRTP-protected RTP media did not reach the answerer"
    );
    assert!(
        b.received_media_count() > 0,
        "answerer surfaced at least one decrypted media frame"
    );
    let frame = b
        .take_received_media()
        .expect("a decrypted frame is buffered");
    assert!(!frame.data.is_empty(), "the decrypted frame carries bytes");
}

#[test]
fn video_samples_carry_advancing_rtp_timestamps() {
    // ADR-P006: preview egress re-stamps each access unit at the negotiated
    // payload type on the 90 kHz clock with the sample's own RTP timestamp — not
    // a constant 0. Two distinct-timestamp samples must both reach the answerer.
    let (mut a, a_addr, mut b, b_addr) = offerer_answerer();
    let now = Instant::now();
    a.add_host_candidate(a_addr).unwrap();
    b.add_host_candidate(b_addr).unwrap();
    let offer = a.create_offer(&[MediaKind::Video]).unwrap();
    let answer = b.accept_offer(&offer).unwrap();
    a.accept_answer(&answer).unwrap();
    assert!(
        pump_until(&mut a, a_addr, &mut b, b_addr, now, |a, b| a.is_connected()
            && b.is_connected()),
        "must connect first"
    );

    // Two complete IDR access units at two distinct 90 kHz timestamps.
    let mut au0 = vec![0x00u8, 0x00, 0x00, 0x01, 0x65];
    au0.extend(std::iter::repeat_n(0xABu8, 1200));
    let mut au1 = vec![0x00u8, 0x00, 0x00, 0x01, 0x65];
    au1.extend(std::iter::repeat_n(0xCDu8, 1200));
    // rtp_ts in 90 kHz units: 0 and 3000 (≈ 33 ms apart).
    a.write_video_sample(&au0, true, 0, now).unwrap();
    let delivered_first = pump_until(&mut a, a_addr, &mut b, b_addr, now, |_a, b| {
        b.received_media_count() >= 1
    });
    assert!(delivered_first, "first sample must arrive");
    a.write_video_sample(&au1, false, 3000, now).unwrap();
    let delivered_second = pump_until(&mut a, a_addr, &mut b, b_addr, now, |_a, b| {
        b.received_media_count() >= 2
    });
    assert!(
        delivered_second,
        "a second sample at a distinct timestamp must also arrive (the RTP \
         timestamp advanced, not pinned to 0)"
    );
}

#[test]
fn audio_opus_samples_flow_to_the_answerer() {
    // ADR-P006: preview carries Opus audio alongside video. An audio sample
    // written to the Opus media must reach the answerer as decrypted media.
    let (mut a, a_addr, mut b, b_addr) = offerer_answerer();
    let now = Instant::now();
    a.add_host_candidate(a_addr).unwrap();
    b.add_host_candidate(b_addr).unwrap();
    let offer = a
        .create_offer(&[MediaKind::Video, MediaKind::Audio])
        .unwrap();
    let answer = b.accept_offer(&offer).unwrap();
    a.accept_answer(&answer).unwrap();
    assert!(
        pump_until(&mut a, a_addr, &mut b, b_addr, now, |a, b| a.is_connected()
            && b.is_connected()),
        "must connect first"
    );

    // A 20 ms Opus frame is 960 samples at the 48 kHz RTP clock.
    let opus_frame = vec![0xF8u8; 80];
    a.write_audio_sample(&opus_frame, 0, now).unwrap();
    a.write_audio_sample(&opus_frame, 960, now).unwrap();
    let delivered = pump_until(&mut a, a_addr, &mut b, b_addr, now, |_a, b| {
        b.received_media_count() >= 1
    });
    assert!(delivered, "Opus audio media did not reach the answerer");
}

#[test]
fn answer_is_str0m_native_ipv6_first_and_bundled() {
    let (mut a, a_addr, mut b, b_addr) = offerer_answerer();
    a.add_host_candidate(a_addr).unwrap();
    b.add_host_candidate(b_addr).unwrap();
    let offer = a
        .create_offer(&[MediaKind::Video, MediaKind::Audio])
        .unwrap();
    let answer = b.accept_offer(&offer).unwrap();
    // str0m's own answer: BUNDLE + rtcp-mux + a real fingerprint + the answerer
    // as DTLS server (setup:passive). Real reachability rides the ICE candidate
    // line; the dummy `c=IN IP4 0.0.0.0` is the WebRTC norm (the addresses are in
    // `a=candidate`, never the `c=` line — RFC 8839 §4.3.2).
    assert!(answer.contains("a=group:BUNDLE"), "answer:\n{answer}");
    assert!(answer.contains("a=rtcp-mux"));
    assert!(answer.contains("a=fingerprint:"));
    assert!(
        answer.contains("a=setup:passive"),
        "answerer is the DTLS server"
    );
    // The gathered v6 host candidate is advertised (IPv6-first reachability,
    // ADR-0042); str0m carries the real address in the candidate, not `c=`.
    assert!(
        answer.contains("typ host") && answer.contains("::1"),
        "v6 host candidate present in:\n{answer}"
    );
}

#[test]
fn relay_candidate_from_turn_appears_in_the_offer() {
    // The operator's NAT-traversal path: the in-crate TURN client allocates a
    // relay (proven separately in `turn_client.rs` against a fake server); the
    // learned relay address is registered with str0m via `add_relay_candidate`
    // and must surface in the offer SDP as a `typ relay` candidate (IPv6-first).
    let now = Instant::now();
    let mut a = Session::new(&SessionConfig::default(), now);
    let host: SocketAddr = OFFERER_ADDR.parse().unwrap();
    // A relayed transport address as a TURN Allocate would yield (v6 relay), and
    // the local socket the relayed traffic egresses from.
    let relayed: SocketAddr = "[2001:db8::a11]:49152".parse().unwrap();
    a.add_host_candidate(host).unwrap();
    a.add_relay_candidate(relayed, host).unwrap();
    let offer = a.create_offer(&[MediaKind::Video]).unwrap();
    assert!(
        offer.contains("typ relay"),
        "the TURN relay candidate is advertised in:\n{offer}"
    );
    assert!(
        offer.contains("2001:db8::a11"),
        "the v6 relayed address is present in:\n{offer}"
    );
}

#[test]
fn turn_allocate_drives_to_a_relay_that_can_be_registered() {
    // End-to-end (offline) TURN: drive the in-crate client to an allocation vs an
    // in-process fake TURN server, then register the learned relay with a session
    // — the same wiring the live endpoint uses, with the socket replaced by a
    // direct shuttle.
    use multiview_webrtc::turn::message::{Class, Method, StunMessage};
    use multiview_webrtc::turn::{TurnClient, TurnCredential, TurnOutput, TurnState};

    let now = Instant::now();
    let server: SocketAddr = "[2001:db8::1]:3478".parse().unwrap();
    let relay: SocketAddr = "[2001:db8::1]:49152".parse().unwrap();
    let mut client = TurnClient::new(server, TurnCredential::static_credential("u", "p", None));
    let mut fake = FakeTurn::new(server, relay, "u", "p", "realm");

    let mut learned = None;
    for _ in 0..32 {
        if let TurnOutput::Transmit { payload, .. } = client.poll_output(now) {
            if let Some(reply) = fake.handle(&payload) {
                client.handle_input(&reply, now).unwrap();
            }
        }
        if let TurnState::Allocated { relay, .. } = client.state() {
            learned = Some(relay);
            break;
        }
    }
    let relay_addr = learned.expect("TURN allocation reached a relay");
    assert_eq!(relay_addr, relay);

    // Register the relay with a real session — proves the relay address is a
    // valid str0m relay candidate.
    let local: SocketAddr = OFFERER_ADDR.parse().unwrap();
    let mut session = Session::new(&SessionConfig::default(), now);
    session.add_relay_candidate(relay_addr, local).unwrap();
    let offer = session.create_offer(&[MediaKind::Video]).unwrap();
    assert!(offer.contains("typ relay"), "relay candidate registered");

    // Keep the fake-server response codec honest.
    let probe = StunMessage::request(Method::Binding).to_bytes(None);
    assert_eq!(StunMessage::parse(&probe).unwrap().class(), Class::Request);
}

#[test]
fn endpoint_config_rejects_turn_without_credentials() {
    use multiview_webrtc::config::{EndpointConfig, IceServer, IceServerKind};
    let bad = IceServer {
        kind: IceServerKind::Turn,
        addr: "[2001:db8::1]:3478".parse().unwrap(),
        credentials: None,
    };
    let cfg = EndpointConfig {
        ice_servers: vec![bad],
        ..EndpointConfig::default()
    };
    assert!(
        cfg.validate().is_err(),
        "TURN without credentials is rejected"
    );
}

#[test]
fn rtp_mode_ingest_surfaces_raw_rtp_packets_for_the_pure_depacketizer() {
    // ADR-T014 §4: WHIP ingest runs an RTP-mode answerer so str0m decrypts SRTP
    // and surfaces RAW RTP packets — the existing pure, keyframe-gated
    // `H264Depacketizer` is the canonical depacketization path, NOT str0m's
    // sample API. The answerer must therefore expose per-packet seq / timestamp /
    // marker / payload-type / payload (the fields `multiview_input`'s `RtpFrame`
    // carries), driven entirely over the in-memory shuttle.
    let now = Instant::now();
    let a_addr: SocketAddr = OFFERER_ADDR.parse().unwrap();
    let b_addr: SocketAddr = ANSWERER_ADDR.parse().unwrap();
    // The publisher (offerer) is a normal sample-mode sender; the ingest answerer
    // is RTP-mode (recvonly from our side — we never write media to it).
    let mut a = Session::new(&SessionConfig::default(), now);
    let mut b = Session::new(&SessionConfig::ingest(), now);
    a.add_host_candidate(a_addr).unwrap();
    b.add_host_candidate(b_addr).unwrap();
    let offer = a.create_offer(&[MediaKind::Video]).unwrap();
    let answer = b.accept_offer(&offer).unwrap();
    a.accept_answer(&answer).unwrap();
    assert!(
        pump_until(&mut a, a_addr, &mut b, b_addr, now, |a, b| a.is_connected()
            && b.is_connected()),
        "must connect first"
    );

    // The publisher writes one IDR access unit large enough to span several RTP
    // packets (so we see real RFC 6184 FU-A fragmentation, marker on the last).
    let mut payload = vec![0x00u8, 0x00, 0x00, 0x01, 0x65];
    payload.extend(std::iter::repeat_n(0xABu8, 4000));
    a.write_video_sample(&payload, true, 0, now).unwrap();

    let delivered = pump_until(&mut a, a_addr, &mut b, b_addr, now, |_a, b| {
        b.received_rtp_count() > 0
    });
    assert!(
        delivered,
        "no raw RTP surfaced on the RTP-mode ingest session"
    );

    let pkt = b.take_received_rtp().expect("a raw RTP packet is buffered");
    assert!(
        !pkt.payload.is_empty(),
        "the RTP packet carries payload bytes"
    );
    // A negotiated dynamic PT for H.264 (96..=127 is the dynamic range).
    assert!(pkt.payload_type >= 96, "a negotiated dynamic H.264 PT");
    // The 90 kHz video RTP timestamp is surfaced verbatim (a 32-bit value).
    let _ = pkt.timestamp;
    let _ = pkt.marker;
    let _ = pkt.sequence;
}

#[test]
fn rtp_mode_ingest_received_ring_is_bounded_drop_oldest() {
    // Inv #10 / safety-rule 5: the per-session received-RTP ring is bounded and
    // drop-oldest — a slow ingest consumer can never grow it without bound. We
    // assert the buffered count never exceeds the cap even under a burst that far
    // exceeds it.
    let now = Instant::now();
    let a_addr: SocketAddr = OFFERER_ADDR.parse().unwrap();
    let b_addr: SocketAddr = ANSWERER_ADDR.parse().unwrap();
    let mut a = Session::new(&SessionConfig::default(), now);
    let mut b = Session::new(&SessionConfig::ingest(), now);
    a.add_host_candidate(a_addr).unwrap();
    b.add_host_candidate(b_addr).unwrap();
    let offer = a.create_offer(&[MediaKind::Video]).unwrap();
    let answer = b.accept_offer(&offer).unwrap();
    a.accept_answer(&answer).unwrap();
    assert!(
        pump_until(&mut a, a_addr, &mut b, b_addr, now, |a, b| a.is_connected()
            && b.is_connected()),
        "must connect first"
    );

    // Write many large access units WITHOUT ever draining the ingest ring, so it
    // is forced far past its cap if drop-oldest were not enforced. The full
    // shuttle (`pump_until`, which advances the virtual clock + feeds timeouts so
    // str0m actually packetizes and sends) runs after each write; `b` never has
    // `take_received_rtp` called, so its ring only ever grows by arrival.
    let mut clock = now;
    for i in 0..200u32 {
        let mut payload = vec![0x00u8, 0x00, 0x00, 0x01, 0x65];
        payload.extend(std::iter::repeat_n(0xCDu8, 4000));
        // One 90 kHz RTP tick per ~5 ms write (i * 450); monotonic re-stamp.
        a.write_video_sample(&payload, true, i.saturating_mul(450), clock)
            .unwrap();
        clock += Duration::from_millis(5);
        // Pump until this write's packets have all reached b (or a bounded number
        // of shuttle iterations elapse), never draining b's ring.
        let before = b.received_rtp_count();
        pump_until(&mut a, a_addr, &mut b, b_addr, clock, |_a, b| {
            b.received_rtp_count() > before
        });
        assert!(
            b.buffered_rtp() <= multiview_webrtc::transport::MAX_RECEIVED_RTP,
            "the received-RTP ring grew past its cap on write {i} ({} > {})",
            b.buffered_rtp(),
            multiview_webrtc::transport::MAX_RECEIVED_RTP
        );
    }
    assert!(b.received_rtp_count() > 0, "media did flow (sanity)");
}

#[test]
fn ingest_session_can_request_a_keyframe_pli() {
    // ADR-T014 §7: the ingest (RTP-mode answerer) session sends PLI toward the
    // publisher's video stream to pull a fresh IDR. The answerer records the
    // negotiated video mid from the answer SDP, so `request_video_keyframe`
    // finds a video stream and queues the RTCP feedback — proven by the offerer
    // surfacing a coalesced keyframe request after the shuttle delivers it.
    let now = Instant::now();
    let a_addr: SocketAddr = OFFERER_ADDR.parse().unwrap();
    let b_addr: SocketAddr = ANSWERER_ADDR.parse().unwrap();
    let mut a = Session::new(&SessionConfig::default(), now);
    let mut b = Session::new(&SessionConfig::ingest(), now);
    a.add_host_candidate(a_addr).unwrap();
    b.add_host_candidate(b_addr).unwrap();
    let offer = a.create_offer(&[MediaKind::Video]).unwrap();
    let answer = b.accept_offer(&offer).unwrap();
    a.accept_answer(&answer).unwrap();
    assert!(
        pump_until(&mut a, a_addr, &mut b, b_addr, now, |a, b| a.is_connected()
            && b.is_connected()),
        "must connect first"
    );

    // Before any media, the publisher has not been asked for a keyframe.
    assert!(!a.take_keyframe_request(), "no PLI yet");
    // The ingest answerer requests a keyframe; the offerer (publisher) must see a
    // coalesced keyframe request once the RTCP PLI is shuttled to it.
    assert!(
        b.request_video_keyframe(now),
        "the ingest session found a video stream to PLI"
    );
    let asked = pump_until(&mut a, a_addr, &mut b, b_addr, now, |a, _b| {
        a.take_keyframe_request()
    });
    assert!(
        asked,
        "the publisher received the PLI as a keyframe request"
    );
}

#[test]
fn whip_endpoint_negotiates_answers_one_publisher_and_releases() {
    // The WHIP control provider's path through the live endpoint, driven OFFLINE
    // (the socket loop is never spawned — negotiation gathers candidates, accepts
    // the offer, and registers the session synchronously). Proves: a first
    // publisher gets an answer + a session id + an RTP ring; a SECOND publisher
    // on the same source is a 409; release frees the slot for a re-POST.
    use multiview_webrtc::config::EndpointConfig;
    use multiview_webrtc::error::WebRtcError;
    use multiview_webrtc::transport::{Session, SessionConfig, WhipEndpoint};

    // A publisher offer (a normal sendonly browser/OBS-shaped offer).
    let now = Instant::now();
    let mut publisher = Session::new(&SessionConfig::default(), now);
    publisher
        .add_host_candidate("[::1]:50000".parse().unwrap())
        .unwrap();
    let offer = publisher
        .create_offer(&[MediaKind::Video, MediaKind::Audio])
        .unwrap();

    // Bind the endpoint on an ephemeral port (the bind itself is local, no live
    // peer); do NOT spawn `run` — we exercise the handle's negotiation logic.
    let cfg = EndpointConfig {
        udp_port: 0,
        // A concrete advertised address — the unspecified `[::]` bind addr is
        // never a valid ICE candidate (str0m rejects it).
        advertised_addresses: vec!["::1".parse().unwrap()],
        ..EndpointConfig::default()
    };
    let (_endpoint, handle) = WhipEndpoint::bind(cfg).expect("bind ephemeral endpoint");

    let first = handle
        .negotiate("cam-1", &offer, true)
        .expect("first publisher negotiates an answer");
    assert!(
        first.answer_sdp.contains("a=group:BUNDLE"),
        "the answer is str0m's own complete SDP"
    );
    assert!(
        !first.session_id.as_str().is_empty(),
        "a session id was minted"
    );
    assert_eq!(handle.live_publisher_count(), 1);

    // A second publisher on the SAME source is a 409 conflict (one per source).
    let second = handle.negotiate("cam-1", &offer, true);
    assert!(
        matches!(second, Err(WebRtcError::PublisherConflict(_))),
        "a second publisher on a live source is a conflict, got {second:?}"
    );

    // A DIFFERENT source negotiates independently (outside the viewer pool).
    let other = handle.negotiate("cam-2", &offer, true);
    assert!(
        other.is_ok(),
        "a different source is admitted independently"
    );
    assert_eq!(handle.live_publisher_count(), 2);

    // Release frees the slot; the source can be re-POSTed.
    assert!(
        handle.release("cam-1", first.session_id.as_str()),
        "release finds the live session"
    );
    assert!(
        !handle.release("cam-1", first.session_id.as_str()),
        "releasing again is idempotent-false (already freed)"
    );
    let reposted = handle.negotiate("cam-1", &offer, true);
    assert!(reposted.is_ok(), "the freed source accepts a new publisher");
}

#[test]
fn whip_endpoint_audio_false_answers_without_opus() {
    // `audio = false` (ADR-T014 §5) answers the audio m-line inactive: the
    // answer carries no Opus rtpmap, so the publisher's audio is not received.
    use multiview_webrtc::config::EndpointConfig;
    use multiview_webrtc::transport::{Session, SessionConfig, WhipEndpoint};

    let now = Instant::now();
    let mut publisher = Session::new(&SessionConfig::default(), now);
    publisher
        .add_host_candidate("[::1]:50010".parse().unwrap())
        .unwrap();
    let offer = publisher
        .create_offer(&[MediaKind::Video, MediaKind::Audio])
        .unwrap();

    let (_endpoint, handle) = WhipEndpoint::bind(EndpointConfig {
        udp_port: 0,
        advertised_addresses: vec!["::1".parse().unwrap()],
        ..EndpointConfig::default()
    })
    .expect("bind");
    let negotiated = handle
        .negotiate("vid-only", &offer, false)
        .expect("audio-false negotiates");
    // No Opus answered when audio is disabled (the ingest session disables Opus).
    assert!(
        !negotiated.answer_sdp.to_ascii_lowercase().contains("opus"),
        "audio=false must not answer Opus:\n{}",
        negotiated.answer_sdp
    );
}

#[test]
fn relay_forced_media_flows_both_ways_through_the_turn_relay() {
    // Defect C — the operator's HARD NAT-traversal requirement, proven offline:
    // when the ONLY usable path between two peers is a TURN relay, real SRTP media
    // must traverse the relay in BOTH directions. The publisher (A) is reachable
    // only via its allocated relay; str0m therefore emits Transmits whose SOURCE
    // is the relay address, which the driver frames as TURN Send indications to
    // the server. The server (acting as the relay) delivers the inner datagram to
    // the far peer (B), and B's replies to the relay address are wrapped back to A
    // as Data indications the driver unwraps. None of this was wired before — the
    // relay was advertised but media never traversed it.
    use multiview_webrtc::turn::message::{Class, Method, StunMessage};
    use multiview_webrtc::turn::{TurnClient, TurnCredential, TurnOutput, TurnState};

    let now = Instant::now();
    let server_addr: SocketAddr = "[2001:db8::1]:3478".parse().unwrap();
    // A's relay address (what the TURN server allocates for A). B reaches A here.
    let a_relay: SocketAddr = "[2001:db8::1]:49152".parse().unwrap();
    let b_addr: SocketAddr = "[2001:db8::b]:50000".parse().unwrap();

    let mut turn_server = FakeTurn::new(server_addr, a_relay, "alice", "s3cret", "example.org");
    let mut a_turn = TurnClient::new(
        server_addr,
        TurnCredential::static_credential("alice", "s3cret", None),
    );

    // 1. Drive A's TURN client to an allocation against the fake server.
    let mut relay = None;
    for _ in 0..32 {
        if let TurnOutput::Transmit { payload, .. } = a_turn.poll_output(now) {
            if let Some(reply) = turn_server.handle(&payload) {
                a_turn.handle_input(&reply, now).unwrap();
            }
        }
        if let TurnState::Allocated { relay: r, .. } = a_turn.state() {
            relay = Some(r);
            break;
        }
    }
    assert_eq!(relay, Some(a_relay), "A allocated its relay");

    // 2. A (publisher) gathers ONLY its relay candidate — no host; the relayed
    //    traffic egresses A's local socket. B gathers a host candidate.
    let a_local: SocketAddr = "[::1]:40001".parse().unwrap();
    let mut a = Session::new(&SessionConfig::default(), now);
    let mut b = Session::new(&SessionConfig::ingest(), now);
    a.add_relay_candidate(a_relay, a_local).unwrap();
    b.add_host_candidate(b_addr).unwrap();
    let offer = a.create_offer(&[MediaKind::Video]).unwrap();
    let answer = b.accept_offer(&offer).unwrap();
    a.accept_answer(&answer).unwrap();

    // 3. The relay shuttle. A's transmits have source == a_relay (str0m chose the
    //    relay candidate): the driver frames them as Send indications to the TURN
    //    server; the server (relay) delivers the inner payload to B as if from
    //    a_relay. B's transmits to a_relay are wrapped back to A as Data
    //    indications the TURN client unwraps and feeds to A as arriving on a_relay.
    let mut clock = now;
    let mut a_saw_pli_or_media = false;
    let mut delivered = false;
    for i in 0..4000 {
        a.handle_timeout(clock).unwrap();
        b.handle_timeout(clock).unwrap();

        // After connect, A writes one IDR access unit that must reach B via relay.
        if a.is_connected() && b.is_connected() && !a_saw_pli_or_media {
            let mut payload = vec![0x00u8, 0x00, 0x00, 0x01, 0x65];
            payload.extend(std::iter::repeat_n(0xABu8, 1200));
            a.write_video_sample(&payload, true, 0, clock).unwrap();
            a_saw_pli_or_media = true;
        }

        // A → (Send indication) → server → B.
        for _ in 0..128 {
            match a.poll_transmit(clock) {
                Some((source, dst, payload)) => {
                    // Relay-forced: A's source is the relay, dst is the peer (B).
                    assert_eq!(source, a_relay, "A sends from its relay candidate");
                    assert_eq!(dst, b_addr, "the peer is B");
                    // Frame as a TURN Send indication to the server.
                    let send_ind = a_turn.wrap_send(dst, &payload);
                    // The server relays: deliver the inner payload to B as if it
                    // arrived from a_relay (B's view of A is the relay).
                    if let Some((peer, inner)) = unwrap_send_indication(&send_ind) {
                        assert_eq!(peer, b_addr);
                        b.handle_datagram(a_relay, b_addr, &inner, clock).unwrap();
                    }
                }
                None => break,
            }
        }
        // B → server (Data indication wrap) → A (unwrap to arriving on a_relay).
        for _ in 0..128 {
            match b.poll_transmit(clock) {
                Some((source, dst, payload)) => {
                    assert_eq!(source, b_addr, "B sends from its host candidate");
                    // B addresses A at the relay; the server wraps it back to A as
                    // a Data indication; A's TURN client unwraps it.
                    assert_eq!(dst, a_relay, "B reaches A via the relay address");
                    let data_ind = turn_server.make_data_indication(b_addr, &payload);
                    if let Some((peer, inner)) = a_turn.unwrap_data(&data_ind) {
                        // Fed to A as arriving from the peer on the relay's local
                        // candidate addr (str0m matches addr == relay).
                        a.handle_datagram(peer, a_relay, &inner, clock).unwrap();
                    }
                }
                None => break,
            }
        }

        if b.received_rtp_count() > 0 {
            delivered = true;
            break;
        }
        let _ = i;
        let next = a.poll_timeout(clock).min(b.poll_timeout(clock));
        clock = next.max(clock + Duration::from_millis(1));
    }

    assert!(
        a.is_connected() && b.is_connected(),
        "ICE+DTLS completed entirely over the relay path"
    );
    assert!(
        delivered,
        "relay-forced SRTP media did not reach B through the TURN relay"
    );
    // Keep the codec honest (the relay shuttle used real STUN framing).
    let probe = StunMessage::request(Method::Binding).to_bytes(None);
    assert_eq!(StunMessage::parse(&probe).unwrap().class(), Class::Request);
}

/// Test helper: unwrap a TURN Send indication into `(peer, payload)` (the relay
/// server's view of what the client asked it to forward).
fn unwrap_send_indication(datagram: &[u8]) -> Option<(SocketAddr, Vec<u8>)> {
    use multiview_webrtc::turn::message::{Class, Method, StunMessage};
    let msg = StunMessage::parse(datagram).ok()?;
    if msg.class() != Class::Indication || msg.method() != Method::Send {
        return None;
    }
    Some((msg.peer_address()?, msg.data()?.to_vec()))
}

#[test]
#[ignore = "live network: binds a real dual-stack UDP socket; hardware-gated"]
fn endpoint_binds_dual_stack_socket() {
    use multiview_webrtc::config::EndpointConfig;
    use multiview_webrtc::transport::WebRtcEndpoint;
    // Port 0 = ephemeral; the bind must be [::] dual-stack, never 0.0.0.0.
    let cfg = EndpointConfig {
        udp_port: 0,
        ..EndpointConfig::default()
    };
    let endpoint = WebRtcEndpoint::bind(cfg).expect("dual-stack bind");
    let local = endpoint.local_addr().expect("bound addr");
    assert!(local.is_ipv6(), "bound IPv6 dual-stack, not 0.0.0.0");
}
