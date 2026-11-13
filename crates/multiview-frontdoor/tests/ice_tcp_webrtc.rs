//! Slice 4 acceptance (issue #290): WebRTC over ICE-TCP with no UDP at all.
//!
//! A real front door runs the UDP socket AND the TCP door on ephemeral ports;
//! a `UnifiedEndpoint` (WHIP ingest) is driven by
//! `run_shared_with_tcp(udp lane, Some(ice-tcp lane))` on the front-door
//! runtime. The client is a str0m `Session` with NO UDP candidate and NO UDP
//! socket: it connects a `TcpStream` to the door's TCP port, gathers an active
//! TCP host candidate at the stream's local address, negotiates through the
//! WHIP handle, frames its transmits (RFC 4571) onto the stream and deframes
//! inbound frames into `handle_datagram_proto(Tcp, ..)`. ICE + DTLS connect and
//! SRTP media reaches the ingest ring, over IPv4 and IPv6 loopback, while the
//! door's UDP socket sees zero datagrams.
#![cfg(feature = "webrtc")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::too_many_lines
)]

use std::io::{ErrorKind, Read, Write};
use std::net::{IpAddr, SocketAddr, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use multiview_frontdoor::tcp::ice_tcp::{Deframer, MAX_FRAME};
use multiview_frontdoor::{FrontDoor, FrontDoorConfig, TcpDoorConfig, TlsRouting};
use multiview_webrtc::config::EndpointConfig;
use multiview_webrtc::transport::{MediaKind, Session, SessionConfig, Transport, UnifiedEndpoint};

const TIMEOUT: Duration = Duration::from_secs(15);

fn webrtc_over_ice_tcp(loopback: IpAddr) {
    let mut door = FrontDoor::start(&FrontDoorConfig {
        port: 0,
        workers: 2,
        recv_buffer_bytes: 0,
        ..FrontDoorConfig::default()
    })
    .unwrap();
    let tcp = door
        .start_tcp(
            TcpDoorConfig {
                listen: "[::]:0".parse().unwrap(),
                ..TcpDoorConfig::default()
            },
            TlsRouting::default(),
            None,
        )
        .unwrap();
    let rtc_lane = door.take_rtc_lane().unwrap();
    let ice_lane = door.take_ice_tcp_lane().unwrap();
    assert_eq!(ice_lane.local_port(), tcp.port());
    let cfg = EndpointConfig {
        udp_port: door.local_addr().port(),
        advertised_addresses: vec![loopback],
        ice_tcp_port: Some(ice_lane.local_port()),
        ..EndpointConfig::default()
    };
    let builder =
        UnifiedEndpoint::with_shared_socket(cfg, rtc_lane.send_socket().unwrap()).unwrap();
    assert_eq!(
        builder.tcp_candidates(),
        &[SocketAddr::new(loopback, tcp.port())]
    );
    let (builder, whip) = builder.with_ingest();
    let endpoint = builder.build();
    let stop = Arc::new(AtomicBool::new(false));
    let stop2 = Arc::clone(&stop);
    door.handle().spawn(async move {
        let _ = endpoint
            .run_shared_with_tcp(rtc_lane, Some(ice_lane), stop2)
            .await;
    });

    // The TCP-only client.
    let server = SocketAddr::new(loopback, tcp.port());
    let mut stream = TcpStream::connect(server).unwrap();
    stream.set_nodelay(true).unwrap();
    stream.set_nonblocking(true).unwrap();
    let local = stream.local_addr().unwrap();
    let now = Instant::now();
    let mut publisher = Session::new(&SessionConfig::default(), now);
    publisher.add_tcp_active_candidate(local).unwrap();
    let offer = publisher.create_offer(&[MediaKind::Video]).unwrap();
    assert!(
        !offer.contains(" udp "),
        "no UDP candidate offered: {offer}"
    );
    let negotiated = whip.negotiate("cam-1", &offer, false).unwrap();
    let answer = &negotiated.answer_sdp;
    assert!(
        answer.contains(&format!(
            "{loopback} {} typ host tcptype passive",
            tcp.port()
        )),
        "the answer advertises the door's passive TCP candidate: {answer}"
    );
    publisher.accept_answer(answer).unwrap();

    let mut deframer = Deframer::new(MAX_FRAME);
    let mut frames = Vec::new();
    let mut buf = vec![0u8; 64 * 1024];
    let mut au = vec![0x00u8, 0x00, 0x00, 0x01, 0x65];
    au.extend(std::iter::repeat_n(0xABu8, 900));
    let mut ts = 0u32;
    let mut last_au = Instant::now();
    let mut rtp = 0usize;
    let deadline = Instant::now() + TIMEOUT;
    while Instant::now() < deadline && rtp < 10 {
        let now = Instant::now();
        loop {
            match stream.read(&mut buf) {
                Ok(0) => panic!("the door closed the ICE-TCP connection"),
                Ok(n) => deframer.feed(&buf[..n], &mut frames).unwrap(),
                Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                Err(e) => panic!("read: {e}"),
            }
        }
        for f in frames.drain(..) {
            publisher
                .handle_datagram_proto(Transport::Tcp, server, local, &f, now)
                .unwrap();
        }
        publisher.handle_timeout(now).unwrap();
        if publisher.is_connected() && now.duration_since(last_au) > Duration::from_millis(30) {
            ts = ts.wrapping_add(3000);
            let _ = publisher.write_video_sample(&au, true, ts, now);
            last_au = now;
        }
        while let Some(o) = publisher.poll_transmit_routed(now) {
            assert_eq!(o.transport, Transport::Tcp, "the client only has TCP");
            let mut wire = u16::try_from(o.payload.len())
                .unwrap()
                .to_be_bytes()
                .to_vec();
            wire.extend_from_slice(&o.payload);
            // Loopback with small volumes: a blocking write never stalls long.
            stream.set_nonblocking(false).unwrap();
            stream.write_all(&wire).unwrap();
            stream.set_nonblocking(true).unwrap();
        }
        while negotiated.ring.pop().is_some() {
            rtp += 1;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    assert!(publisher.is_connected(), "ICE+DTLS completed over ICE-TCP");
    assert!(
        rtp >= 10,
        "media reached the ingest ring over ICE-TCP: {rtp}"
    );
    let stats = door.stats().snapshot();
    assert_eq!(
        stats.received, 0,
        "no UDP datagram reached the door: {stats:?}"
    );
    assert_eq!(stats.ice_tcp_accepted, 1, "{stats:?}");
    assert!(stats.ice_tcp_frames_in >= 10, "{stats:?}");
    // A TCP path never enters the UDP established set.
    assert_eq!(door.rtc_established().len(), 0);
    stop.store(true, Ordering::Release);
    drop(door);
}

#[test]
fn webrtc_over_ice_tcp_with_no_udp() {
    webrtc_over_ice_tcp("::1".parse().unwrap());
    webrtc_over_ice_tcp("127.0.0.1".parse().unwrap());
}
