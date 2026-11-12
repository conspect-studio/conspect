//! ICE-TCP at the sans-IO session seam (issue #290, slice 4): a session that
//! gathers only a **passive** TCP host candidate and a peer with only an
//! **active** TCP host candidate complete ICE + DTLS and carry SRTP media
//! through an in-memory shuttle tagged [`Transport::Tcp`] — no socket. Every
//! transmit is routed as TCP (never UDP), and `validated_peer()` never reports
//! a TCP path (the front door's established set is the UDP socket's).
#![cfg(feature = "native")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::missing_panics_doc
)]

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use multiview_webrtc::transport::{
    resolve_local_destination, tcp_passive_candidates, MediaKind, Session, SessionConfig, Transport,
};

/// Shuttle every routed transmit from one peer into the other as TCP, until
/// `done` or the bound. Each side's `destination` on receipt is the address
/// the frame was sent to (the TCP connection's local end).
fn pump_tcp(
    a: &mut Session,
    b: &mut Session,
    mut clock: Instant,
    mut done: impl FnMut(&mut Session, &mut Session) -> bool,
) -> bool {
    for _ in 0..2000 {
        a.handle_timeout(clock).unwrap();
        b.handle_timeout(clock).unwrap();
        while let Some(o) = a.poll_transmit_routed(clock) {
            assert_eq!(o.transport, Transport::Tcp, "a TCP-only session sends TCP");
            b.handle_datagram_proto(Transport::Tcp, o.source, o.destination, &o.payload, clock)
                .unwrap();
        }
        while let Some(o) = b.poll_transmit_routed(clock) {
            assert_eq!(o.transport, Transport::Tcp, "a TCP-only session sends TCP");
            a.handle_datagram_proto(Transport::Tcp, o.source, o.destination, &o.payload, clock)
                .unwrap();
        }
        if done(a, b) {
            return true;
        }
        let next = a.poll_timeout(clock).min(b.poll_timeout(clock));
        clock = next.max(clock + Duration::from_millis(1));
    }
    false
}

fn connect(server_addr: &str, client_addr: &str) {
    let now = Instant::now();
    let server: SocketAddr = server_addr.parse().unwrap();
    let client: SocketAddr = client_addr.parse().unwrap();
    // The client publishes (offerer, active); the server ingests (passive).
    let mut publisher = Session::new(&SessionConfig::default(), now);
    publisher.add_tcp_active_candidate(client).unwrap();
    let mut ingest = Session::new(&SessionConfig::ingest(), now);
    ingest.add_tcp_passive_candidate(server).unwrap();
    let offer = publisher.create_offer(&[MediaKind::Video]).unwrap();
    assert!(offer.contains("tcptype active"), "{offer}");
    let answer = ingest.accept_offer(&offer).unwrap();
    assert!(
        answer.contains(&format!(" {} typ host tcptype passive", server.port())),
        "{answer}"
    );
    publisher.accept_answer(&answer).unwrap();

    assert!(
        pump_tcp(&mut publisher, &mut ingest, now, |a, b| a.is_connected()
            && b.is_connected()),
        "ICE+DTLS over ICE-TCP"
    );
    let mut au = vec![0x00u8, 0x00, 0x00, 0x01, 0x65];
    au.extend(std::iter::repeat_n(0xABu8, 1200));
    let mut ts = 0u32;
    let delivered = pump_tcp(&mut publisher, &mut ingest, now, |a, b| {
        ts = ts.wrapping_add(3000);
        let _ = a.write_video_sample(&au, true, ts, Instant::now());
        b.received_rtp_count() >= 5
    });
    assert!(delivered, "SRTP media flowed over ICE-TCP");
    assert_eq!(
        ingest.validated_peer(),
        None,
        "a TCP path is never UDP-validated"
    );
    assert_eq!(publisher.validated_peer(), None);
}

#[test]
fn passive_and_active_tcp_sessions_connect_and_carry_media_ipv6() {
    connect("[::1]:443", "[::1]:50123");
}

#[test]
fn passive_and_active_tcp_sessions_connect_and_carry_media_ipv4() {
    connect("127.0.0.1:443", "127.0.0.1:50123");
}

/// A session with both a UDP and a passive TCP candidate on the same
/// `addr:port` still prefers (and validates) UDP with a UDP peer, and the
/// UDP-only peer's transmits stay UDP.
#[test]
fn udp_peer_ignores_the_tcp_candidate_and_validates_udp() {
    let now = Instant::now();
    let server: SocketAddr = "[::1]:443".parse().unwrap();
    let client: SocketAddr = "[::1]:50124".parse().unwrap();
    let mut publisher = Session::new(&SessionConfig::default(), now);
    publisher.add_host_candidate(client).unwrap();
    let mut ingest = Session::new(&SessionConfig::ingest(), now);
    ingest.add_host_candidate(server).unwrap();
    ingest.add_tcp_passive_candidate(server).unwrap();
    let offer = publisher.create_offer(&[MediaKind::Video]).unwrap();
    let answer = ingest.accept_offer(&offer).unwrap();
    let udp_line = answer.lines().position(|l| l.contains(" udp ")).unwrap();
    let tcp_line = answer
        .lines()
        .position(|l| l.contains("tcptype passive"))
        .unwrap();
    assert!(udp_line < tcp_line, "UDP candidate first: {answer}");
    publisher.accept_answer(&answer).unwrap();
    let mut clock = now;
    for _ in 0..2000 {
        publisher.handle_timeout(clock).unwrap();
        ingest.handle_timeout(clock).unwrap();
        while let Some(o) = publisher.poll_transmit_routed(clock) {
            assert_eq!(o.transport, Transport::Udp);
            ingest
                .handle_datagram(o.source, o.destination, &o.payload, clock)
                .unwrap();
        }
        while let Some(o) = ingest.poll_transmit_routed(clock) {
            assert_eq!(o.transport, Transport::Udp, "no TCP transmit to a UDP peer");
            publisher
                .handle_datagram(o.source, o.destination, &o.payload, clock)
                .unwrap();
        }
        if publisher.is_connected() && ingest.is_connected() && publisher.validated_peer().is_some()
        {
            break;
        }
        clock = publisher
            .poll_timeout(clock)
            .min(ingest.poll_timeout(clock))
            .max(clock + Duration::from_millis(1));
    }
    assert!(ingest.is_connected());
    assert_eq!(publisher.validated_peer(), Some(server));
}

#[test]
fn tcp_candidates_one_per_concrete_host_ip() {
    let hosts: Vec<SocketAddr> = vec![
        "[::]:8189".parse().unwrap(),
        "[2001:db8::1]:8189".parse().unwrap(),
        "192.0.2.1:8189".parse().unwrap(),
        "[2001:db8::1]:9000".parse().unwrap(),
    ];
    assert_eq!(
        tcp_passive_candidates(&hosts, None),
        Vec::<SocketAddr>::new()
    );
    assert_eq!(
        tcp_passive_candidates(&hosts, Some(443)),
        vec![
            "[2001:db8::1]:443".parse::<SocketAddr>().unwrap(),
            "192.0.2.1:443".parse().unwrap(),
        ]
    );
}

/// Review M1 (PR 337): NAT 1:1 (`network.advertise`). The server session's
/// only candidates are the advertised public passive TCP ones; an IPv4 active
/// client's frames arrive on a dual-stack listener whose accepted socket
/// reports the private, IPv4-mapped local address `[::ffff:10.0.0.5]:443`.
/// Resolved through `resolve_local_destination` exactly as the unified
/// driver does, the destination must be the IPv4 candidate: ICE+DTLS connect.
#[test]
fn ipv4_active_client_connects_behind_nat_1to1_with_a_mapped_local() {
    let now = Instant::now();
    let public_v6: SocketAddr = "[2001:db8::10]:443".parse().unwrap();
    let public_v4: SocketAddr = "203.0.113.10:443".parse().unwrap();
    let mapped_local: SocketAddr = "[::ffff:10.0.0.5]:443".parse().unwrap();
    let tcp_candidates = [public_v6, public_v4];
    let client: SocketAddr = "198.51.100.7:50125".parse().unwrap();

    let mut publisher = Session::new(&SessionConfig::default(), now);
    publisher.add_tcp_active_candidate(client).unwrap();
    let mut ingest = Session::new(&SessionConfig::ingest(), now);
    for c in tcp_candidates {
        ingest.add_tcp_passive_candidate(c).unwrap();
    }
    let offer = publisher.create_offer(&[MediaKind::Video]).unwrap();
    let answer = ingest.accept_offer(&offer).unwrap();
    assert!(
        answer.contains("203.0.113.10 443 typ host tcptype passive"),
        "{answer}"
    );
    publisher.accept_answer(&answer).unwrap();

    let mut clock = now;
    let mut to_server = 0usize;
    let mut connected = false;
    for _ in 0..2000 {
        publisher.handle_timeout(clock).unwrap();
        ingest.handle_timeout(clock).unwrap();
        while let Some(o) = publisher.poll_transmit_routed(clock) {
            assert_eq!(o.transport, Transport::Tcp);
            // The door reports the accepted socket's local end, not the
            // advertised address the client dialled.
            let dst = resolve_local_destination(mapped_local, &tcp_candidates);
            assert_eq!(
                dst, public_v4,
                "a mapped v4 arrival resolves to the v4 candidate"
            );
            to_server += 1;
            ingest
                .handle_datagram_proto(Transport::Tcp, o.source, dst, &o.payload, clock)
                .unwrap();
        }
        while let Some(o) = ingest.poll_transmit_routed(clock) {
            assert_eq!(o.transport, Transport::Tcp);
            publisher
                .handle_datagram_proto(Transport::Tcp, o.source, o.destination, &o.payload, clock)
                .unwrap();
        }
        if publisher.is_connected() && ingest.is_connected() {
            connected = true;
            break;
        }
        clock = publisher
            .poll_timeout(clock)
            .min(ingest.poll_timeout(clock))
            .max(clock + Duration::from_millis(1));
    }
    assert!(to_server > 0, "the client sent frames to the server");
    assert!(connected, "ICE+DTLS over ICE-TCP behind NAT 1:1");
}
