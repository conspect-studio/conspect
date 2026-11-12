//! Slice 1 acceptance (issue #287): ONE front-door UDP socket serves a real
//! MoQ/WebTransport session (vendored moq-tokio server, QUIC lane injected) and a
//! real WebRTC session (str0m `UnifiedEndpoint` WHIP ingest, driven from the
//! WebRTC lane) at the same time, on IPv4 and on IPv6 loopback, unprivileged
//! port. Also: every first byte 0..=255 sent at the live socket creates no QUIC
//! connection and reaches no WebRTC session (the S4 sweep), and the established
//! path is written only after validation.
#![cfg(feature = "webrtc")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::too_many_lines,
    clippy::doc_markdown,
    clippy::default_trait_access
)]

use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use moq_tokio::moq_net;
use multiview_frontdoor::{FrontDoor, FrontDoorConfig};
use multiview_webrtc::config::EndpointConfig;
use multiview_webrtc::transport::{MediaKind, Session, SessionConfig, UnifiedEndpoint};

const TIMEOUT: Duration = Duration::from_secs(15);

fn config() -> FrontDoorConfig {
    FrontDoorConfig {
        port: 0,
        workers: 2,
        recv_buffer_bytes: 0,
        ..FrontDoorConfig::default()
    }
}

/// Start a MoQ server on the QUIC lane (inside the front-door runtime) that
/// publishes one track with one frame, and return its accept task.
fn start_moq(door: &mut FrontDoor) -> tokio::task::JoinHandle<anyhow::Result<()>> {
    let lane = door.take_quic_lane().unwrap();
    let established = door.quic_established();
    door.handle().spawn(async move {
        let origin = moq_tokio::origin::spawn();
        let broadcast = origin.create_broadcast("test").unwrap();
        broadcast.announce(Default::default()).unwrap();
        let track = broadcast.create_track("video", None).unwrap();
        let mut group = track.append_group().unwrap();
        group
            .write_frame(moq_net::Timestamp::ZERO, b"hello".as_ref())
            .unwrap();
        group.finish().unwrap();

        let mut listen = moq_tokio::listen::Config::default();
        listen.tls.generate = vec!["localhost".into()];
        let mut server = moq_tokio::server::Config::default();
        server.listen = listen;
        server.socket = Some(moq_tokio::noq::Injected {
            socket: Box::new(lane),
            endpoint: multiview_frontdoor::quic::endpoint_config(),
        });
        let mut listener = server.init()?.listen().await?;
        let request = listener.accept().await.expect("an incoming session");
        let remote = request.remote_addr();
        let session = request.with_publisher(&origin).ok().await?;
        if let Some(remote) = remote {
            established.insert(remote);
        }
        let _keep = (broadcast, track);
        let _ = session.closed().await;
        Ok(())
    })
}

/// A real MoQ subscriber: connect over WebTransport (`https://`), subscribe and
/// read the frame.
async fn moq_subscribe(server: SocketAddr, bind: &str) -> Vec<u8> {
    let sub_origin = moq_tokio::origin::spawn();
    let consumer = sub_origin.consume();
    let mut announcements = consumer.announced();
    let mut client_config = moq_tokio::connect::Config::default();
    client_config.tls.insecure = Some(true);
    client_config.bind = Some(bind.parse().unwrap());
    let client = client_config
        .init(Default::default())
        .unwrap()
        .with_subscriber(sub_origin);
    let host = match server {
        SocketAddr::V4(v4) => v4.ip().to_string(),
        SocketAddr::V6(v6) => format!("[{}]", v6.ip()),
    };
    let url: url::Url = format!("https://{host}:{}", server.port()).parse().unwrap();
    let connection = tokio::time::timeout(
        TIMEOUT,
        client
            .clone()
            .with_reconnect(false)
            .connect(url)
            .established(),
    )
    .await
    .expect("MoQ connect timed out")
    .expect("MoQ connect failed");
    tokio::time::timeout(TIMEOUT, announcements.next())
        .await
        .expect("announce timed out")
        .expect("origin closed");
    let broadcast = tokio::time::timeout(TIMEOUT, consumer.request_broadcast("test"))
        .await
        .expect("broadcast timed out")
        .expect("broadcast resolves");
    let mut track = broadcast
        .track("video")
        .unwrap()
        .subscribe(None)
        .await
        .unwrap();
    let mut group = tokio::time::timeout(TIMEOUT, track.recv_group())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let frame = tokio::time::timeout(TIMEOUT, group.read_frame())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    drop(connection);
    frame.payload.to_vec()
}

/// Start the WebRTC endpoint (WHIP ingest lane) on the WebRTC lane.
fn start_rtc(
    door: &mut FrontDoor,
    advertise: std::net::IpAddr,
) -> (multiview_webrtc::transport::WhipHandle, Arc<AtomicBool>) {
    let lane = door.take_rtc_lane().unwrap();
    let send = lane.send_socket().unwrap();
    let cfg = EndpointConfig {
        udp_port: door.local_addr().port(),
        advertised_addresses: vec![advertise],
        ..EndpointConfig::default()
    };
    let builder = UnifiedEndpoint::with_shared_socket(cfg, send).unwrap();
    let (builder, whip) = builder.with_ingest();
    let endpoint = builder.build();
    let stop = Arc::new(AtomicBool::new(false));
    let stop2 = Arc::clone(&stop);
    door.handle().spawn(async move {
        let _ = endpoint.run_shared(lane, stop2).await;
    });
    (whip, stop)
}

/// A real str0m WHIP publisher on its own UDP socket: negotiate against the
/// endpoint, connect over ICE/DTLS through the shared socket, and send IDR
/// access units until the endpoint's ring has RTP. Returns RTP packets seen.
fn publish_rtc(whip: &multiview_webrtc::transport::WhipHandle, bind: SocketAddr) -> usize {
    let sock = UdpSocket::bind(bind).unwrap();
    sock.set_nonblocking(true).unwrap();
    let local = sock.local_addr().unwrap();
    let now = Instant::now();
    let mut publisher = Session::new(&SessionConfig::default(), now);
    publisher.add_host_candidate(local).unwrap();
    let offer = publisher.create_offer(&[MediaKind::Video]).unwrap();
    let negotiated = whip.negotiate("cam-1", &offer, false).unwrap();
    publisher.accept_answer(&negotiated.answer_sdp).unwrap();

    let mut au = vec![0x00u8, 0x00, 0x00, 0x01, 0x65];
    au.extend(std::iter::repeat_n(0xABu8, 900));
    let deadline = Instant::now() + TIMEOUT;
    let mut buf = [0u8; 2048];
    let mut rtp = 0usize;
    let mut ts = 0u32;
    let mut last_au = Instant::now();
    while Instant::now() < deadline && rtp < 5 {
        let now = Instant::now();
        while let Ok((n, src)) = sock.recv_from(&mut buf) {
            publisher
                .handle_datagram(src, local, &buf[..n], now)
                .unwrap();
        }
        publisher.handle_timeout(now).unwrap();
        if publisher.is_connected() && now.duration_since(last_au) > Duration::from_millis(30) {
            ts = ts.wrapping_add(3000);
            let _ = publisher.write_video_sample(&au, true, ts, now);
            last_au = now;
        }
        while let Some((_src, dst, payload)) = publisher.poll_transmit(now) {
            let _ = sock.send_to(&payload, dst);
        }
        while negotiated.ring.pop().is_some() {
            rtp += 1;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    assert!(
        publisher.is_connected(),
        "ICE+DTLS completed through the shared socket"
    );
    rtp
}

fn both_on_one_socket(loopback: &str, advertise: std::net::IpAddr) {
    let mut door = FrontDoor::start(&config()).unwrap();
    let port = door.local_addr().port();
    assert!(door.local_addr().is_ipv6(), "bound [::] dual-stack");
    let server: SocketAddr = format!("{loopback}:{port}").parse().unwrap();
    let client_bind = if server.is_ipv4() {
        "127.0.0.1:0"
    } else {
        "[::1]:0"
    };

    let moq = start_moq(&mut door);
    let (whip, stop) = start_rtc(&mut door, advertise);

    // The test's own runtime plays the clients: neither depends on the
    // front-door runtime for anything but the server side.
    let client_rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let moq_client = client_rt.spawn(moq_subscribe(server, client_bind));
    let rtc_bind: SocketAddr = client_bind.parse().unwrap();
    let rtp = publish_rtc(&whip, rtc_bind);
    let payload = client_rt.block_on(moq_client).unwrap();

    assert_eq!(payload, b"hello", "the MoQ frame arrived over WebTransport");
    assert!(
        rtp >= 5,
        "WebRTC media reached the ingest ring: {rtp} RTP packets"
    );
    let stats = door.stats().snapshot();
    assert!(stats.to_quic > 0, "{stats:?}");
    assert!(stats.to_rtc > 0, "{stats:?}");
    // Both consumers marked their validated peer.
    assert_eq!(door.quic_established().len(), 1, "one validated QUIC peer");
    let _ = &moq;
    stop.store(true, Ordering::Release);
    drop(door);
}

#[test]
fn moq_and_webrtc_share_one_socket_on_ipv6_loopback() {
    both_on_one_socket("[::1]", "::1".parse().unwrap());
}

#[test]
fn moq_and_webrtc_share_one_socket_on_ipv4_loopback() {
    both_on_one_socket("127.0.0.1", "127.0.0.1".parse().unwrap());
}

/// The S4 sweep on the live socket: one 1300-byte datagram per first byte
/// 0..=255 (random tail) from one source, with a real TLS MoQ server on the QUIC
/// lane and a live WHIP session on the WebRTC lane. No QUIC session may be
/// accepted and the WebRTC session must surface nothing.
#[test]
fn every_first_byte_on_the_live_socket_misdelivers_nothing() {
    let mut door = FrontDoor::start(&config()).unwrap();
    let port = door.local_addr().port();
    let lane = door.take_quic_lane().unwrap();
    let (accepted_tx, mut accepted_rx) = tokio::sync::mpsc::channel::<()>(4);
    door.handle().spawn(async move {
        let mut listen = moq_tokio::listen::Config::default();
        listen.tls.generate = vec!["localhost".into()];
        let mut server = moq_tokio::server::Config::default();
        server.listen = listen;
        server.socket = Some(moq_tokio::noq::Injected {
            socket: Box::new(lane),
            endpoint: multiview_frontdoor::quic::endpoint_config(),
        });
        let mut listener = server.init()?.listen().await?;
        while listener.accept().await.is_some() {
            let _ = accepted_tx.try_send(());
        }
        anyhow::Ok(())
    });
    let (whip, stop) = start_rtc(&mut door, "::1".parse().unwrap());
    let now = Instant::now();
    let mut publisher = Session::new(&SessionConfig::default(), now);
    publisher
        .add_host_candidate("[::1]:1".parse().unwrap())
        .unwrap();
    let offer = publisher.create_offer(&[MediaKind::Video]).unwrap();
    let negotiated = whip.negotiate("cam-1", &offer, false).unwrap();
    std::thread::sleep(Duration::from_millis(200));

    let sender = UdpSocket::bind("[::1]:0").unwrap();
    let mut seed = 0x1234_5678_u32;
    for byte in 0..=u8::MAX {
        let mut d = vec![0u8; 1300];
        for b in &mut d {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            *b = seed.to_le_bytes()[0];
        }
        d[0] = byte;
        // Make the supported-version long headers parse, so noq really sees
        // Initial-shaped datagrams (and must still create nothing).
        if byte >= 192 {
            d[1..5].copy_from_slice(&1u32.to_be_bytes());
            d[5] = 8;
            d[14] = 8;
        }
        sender.send_to(&d, ("::1", port)).unwrap();
        std::thread::sleep(Duration::from_millis(1));
    }
    std::thread::sleep(Duration::from_millis(1000));
    let stats = door.stats().snapshot();
    assert_eq!(stats.received, 256, "{stats:?}");
    assert!(
        accepted_rx.try_recv().is_err(),
        "no QUIC session from garbage"
    );
    assert_eq!(negotiated.ring.pop(), None, "no RTP surfaced from garbage");
    assert_eq!(door.rtc_established().len(), 0, "nothing validated");
    assert_eq!(door.quic_established().len(), 0, "nothing validated");
    // 4-19 are dropped before any lane.
    assert!(stats.dropped_classify >= 16, "{stats:?}");
    assert_eq!(
        stats.to_quic + stats.to_rtc + stats.dropped_classify + stats.dropped_per_source,
        256,
        "{stats:?}"
    );
    stop.store(true, Ordering::Release);
}
