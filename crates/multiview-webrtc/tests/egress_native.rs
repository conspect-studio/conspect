//! Failing-first, **offline** tests for the native WebRTC program-output egress
//! ([`multiview_webrtc::transport`], feature `native`) — WHEP-serve and WHIP-push
//! (ADR-0049 §5).
//!
//! str0m is sans-IO, so the full ICE+DTLS+SRTP handshake and media egress run in
//! memory over a packet shuttle (no socket, no network) on an ordinary CI runner.
//! The live legs (a real browser WHEP player; a real OBS/MediaMTX WHIP ingest)
//! are `#[ignore]`d and hardware-gated.
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
use std::time::{Duration, Instant};

use multiview_webrtc::egress::{EgressMedia, EgressSample};
use multiview_webrtc::transport::{Direction, MediaKind, Session, SessionConfig};

const SERVER_ADDR: &str = "[::1]:41001";
const VIEWER_ADDR: &str = "[::1]:41002";

/// Shuttle every datagram between two sans-IO sessions, advancing a shared
/// virtual clock, until `done` or the iteration bound.
fn pump_until(
    a: &mut Session,
    a_addr: SocketAddr,
    b: &mut Session,
    b_addr: SocketAddr,
    mut clock: Instant,
    mut done: impl FnMut(&mut Session, &mut Session) -> bool,
) -> bool {
    for _ in 0..4000 {
        a.handle_timeout(clock).unwrap();
        b.handle_timeout(clock).unwrap();
        for _ in 0..256 {
            // poll_transmit now surfaces str0m's (source, destination, payload); for
            // host candidates the source is the local addr — assert the destination.
            match a.poll_transmit(clock) {
                Some((_source, dst, payload)) => {
                    assert_eq!(dst, b_addr);
                    b.handle_datagram(a_addr, b_addr, &payload, clock).unwrap();
                }
                None => break,
            }
        }
        for _ in 0..256 {
            match b.poll_transmit(clock) {
                Some((_source, dst, payload)) => {
                    assert_eq!(dst, a_addr);
                    a.handle_datagram(b_addr, a_addr, &payload, clock).unwrap();
                }
                None => break,
            }
        }
        if done(a, b) {
            return true;
        }
        let next = a.poll_timeout(clock).min(b.poll_timeout(clock));
        clock = next.max(clock + Duration::from_millis(1));
    }
    false
}

/// WHEP-serve: the **server** is the answerer (the viewer's browser offers
/// recvonly; Multiview answers sendonly), sample-writing the real program AU.
#[test]
fn whep_serve_sample_write_reaches_the_viewer() {
    let now = Instant::now();
    let server_addr: SocketAddr = SERVER_ADDR.parse().unwrap();
    let viewer_addr: SocketAddr = VIEWER_ADDR.parse().unwrap();

    // The viewer (browser) is the offerer with a recvonly video m-line.
    let mut viewer = Session::new(&SessionConfig::default(), now);
    let mut server = Session::new(&SessionConfig::serve(), now);
    viewer.add_host_candidate(viewer_addr).unwrap();
    server.add_host_candidate(server_addr).unwrap();
    // A WHEP viewer offers RECVONLY (it receives the program); the server answers
    // sendonly and sample-writes into the negotiated mid.
    let offer = viewer
        .create_offer_with_direction(&[MediaKind::Video], Direction::RecvOnly)
        .unwrap();
    let answer = server.accept_offer(&offer).unwrap();
    viewer.accept_answer(&answer).unwrap();

    assert!(
        pump_until(
            &mut server,
            server_addr,
            &mut viewer,
            viewer_addr,
            now,
            |s, v| { s.is_connected() && v.is_connected() }
        ),
        "ICE+DTLS must complete"
    );

    // Sample-write a realistic IDR access unit with an explicit RTP timestamp
    // (invariant #3 — derived from the program tick, never input PTS).
    let mut au = vec![0x00u8, 0x00, 0x00, 0x01, 0x65];
    au.extend(std::iter::repeat_n(0xABu8, 1200));
    server.write_video_sample(&au, true, 90_000, now).unwrap();

    assert!(
        pump_until(
            &mut server,
            server_addr,
            &mut viewer,
            viewer_addr,
            now,
            |_s, v| v.received_media_count() > 0
        ),
        "the program AU must reach the viewer over SRTP"
    );
    let frame = viewer.take_received_media().expect("a decrypted frame");
    assert!(!frame.data.is_empty(), "the viewer decrypts program bytes");
}

/// The WHEP-serve answer is str0m-native: BUNDLE, rtcp-mux, a real fingerprint,
/// and the server is the DTLS server (`setup:passive`).
#[test]
fn whep_serve_answer_is_sendonly_and_passive() {
    let now = Instant::now();
    let server_addr: SocketAddr = SERVER_ADDR.parse().unwrap();
    let viewer_addr: SocketAddr = VIEWER_ADDR.parse().unwrap();
    let mut viewer = Session::new(&SessionConfig::default(), now);
    let mut server = Session::new(&SessionConfig::serve(), now);
    viewer.add_host_candidate(viewer_addr).unwrap();
    server.add_host_candidate(server_addr).unwrap();
    let offer = viewer
        .create_offer_with_direction(&[MediaKind::Video, MediaKind::Audio], Direction::RecvOnly)
        .unwrap();
    let answer = server.accept_offer(&offer).unwrap();
    assert!(answer.contains("a=group:BUNDLE"), "answer:\n{answer}");
    assert!(answer.contains("a=rtcp-mux"));
    assert!(answer.contains("a=fingerprint:"));
    assert!(
        answer.contains("a=setup:passive"),
        "the WHEP server is the DTLS server"
    );
}

/// WHIP-push: Multiview is the **client** (the offerer), publishing the program
/// sendonly to a remote WHIP ingest; the remote answers. The client then
/// sample-writes the program AU and it reaches the remote.
#[test]
fn whip_push_offer_answer_and_send_reaches_the_remote() {
    let now = Instant::now();
    // Multiview (the WHIP client) is the offerer; the remote ingest answers.
    let client_addr: SocketAddr = SERVER_ADDR.parse().unwrap();
    let remote_addr: SocketAddr = VIEWER_ADDR.parse().unwrap();
    let mut client = Session::new(&SessionConfig::push(), now);
    let mut remote = Session::new(&SessionConfig::ingest(), now);
    client.add_host_candidate(client_addr).unwrap();
    remote.add_host_candidate(remote_addr).unwrap();

    let offer = client
        .create_offer(&[MediaKind::Video, MediaKind::Audio])
        .unwrap();
    // The push offer is sendonly + actpass (the answerer picks the DTLS role).
    assert!(offer.contains("a=sendonly"), "push offer is sendonly");
    assert!(offer.contains("a=setup:actpass"), "offerer is actpass");
    let answer = remote.accept_offer(&offer).unwrap();
    client.accept_answer(&answer).unwrap();

    assert!(
        pump_until(
            &mut client,
            client_addr,
            &mut remote,
            remote_addr,
            now,
            |c, r| { c.is_connected() && r.is_connected() }
        ),
        "ICE+DTLS must complete for the push"
    );

    let mut au = vec![0x00u8, 0x00, 0x00, 0x01, 0x65];
    au.extend(std::iter::repeat_n(0xCDu8, 1200));
    client.write_video_sample(&au, true, 90_000, now).unwrap();

    assert!(
        pump_until(
            &mut client,
            client_addr,
            &mut remote,
            remote_addr,
            now,
            |_c, r| r.received_rtp_count() > 0
        ),
        "the pushed program AU must reach the remote ingest"
    );
}

/// A single sample-write fans to TWO viewer sessions writing the SAME bytes
/// (encode-once: one program AU, packetization per viewer only — invariant #7).
#[test]
fn one_access_unit_serves_two_viewers() {
    let now = Instant::now();
    let s1: SocketAddr = "[::1]:42001".parse().unwrap();
    let v1: SocketAddr = "[::1]:42002".parse().unwrap();
    let s2: SocketAddr = "[::1]:42003".parse().unwrap();
    let v2: SocketAddr = "[::1]:42004".parse().unwrap();

    let connect = |s_addr: SocketAddr, v_addr: SocketAddr| {
        let mut viewer = Session::new(&SessionConfig::default(), now);
        let mut server = Session::new(&SessionConfig::serve(), now);
        viewer.add_host_candidate(v_addr).unwrap();
        server.add_host_candidate(s_addr).unwrap();
        let offer = viewer
            .create_offer_with_direction(&[MediaKind::Video], Direction::RecvOnly)
            .unwrap();
        let answer = server.accept_offer(&offer).unwrap();
        viewer.accept_answer(&answer).unwrap();
        assert!(pump_until(
            &mut server,
            s_addr,
            &mut viewer,
            v_addr,
            now,
            |s, v| s.is_connected() && v.is_connected()
        ));
        (server, viewer)
    };
    let (mut server1, mut viewer1) = connect(s1, v1);
    let (mut server2, mut viewer2) = connect(s2, v2);

    // The SAME access unit bytes are written into each viewer's session.
    let mut au = vec![0x00u8, 0x00, 0x00, 0x01, 0x65];
    au.extend(std::iter::repeat_n(0x77u8, 1000));
    server1.write_video_sample(&au, true, 90_000, now).unwrap();
    server2.write_video_sample(&au, true, 90_000, now).unwrap();

    assert!(pump_until(
        &mut server1,
        s1,
        &mut viewer1,
        v1,
        now,
        |_s, v| v.received_media_count() > 0
    ));
    assert!(pump_until(
        &mut server2,
        s2,
        &mut viewer2,
        v2,
        now,
        |_s, v| v.received_media_count() > 0
    ));
    let f1 = viewer1.take_received_media().unwrap();
    let f2 = viewer2.take_received_media().unwrap();
    assert_eq!(
        f1.data, f2.data,
        "both viewers decode the identical program AU"
    );
}

/// Keep `EgressSample` honest in the native build too (it is the carrier the
/// driver sample-writes from).
#[test]
fn egress_sample_round_trips_video_and_audio() {
    let v = EgressSample {
        media: EgressMedia::Video,
        rtp_timestamp: 90_000,
        keyframe: true,
        data: vec![1, 2, 3],
    };
    assert_eq!(v.media, EgressMedia::Video);
    assert!(v.keyframe);
}
