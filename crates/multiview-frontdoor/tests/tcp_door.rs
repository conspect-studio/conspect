//! The TCP door (slice 4, issue #290): TLS h1 and ICE-TCP on one port, the
//! ICE-TCP gates and probation, behind-a-proxy mode, the redirect, the
//! deadlines and the caps. Unprivileged: ephemeral ports on `[::1]` and
//! 127.0.0.1.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::too_many_lines,
    clippy::print_stderr
)]

mod common;

use std::net::SocketAddr;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use common::{binding_request, closed_within, frame, https_get, site_config, FixedSite};
use multiview_frontdoor::{
    FrontDoor, FrontDoorConfig, RedirectConfig, SendOutcome, SiteService, TcpDoorConfig,
    TlsRouting, TrustedProxies,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpSocket, TcpStream};

fn udp_door() -> FrontDoor {
    FrontDoor::start(&FrontDoorConfig {
        port: 0,
        workers: 2,
        ..FrontDoorConfig::default()
    })
    .unwrap()
}

fn fast(listen: &str) -> TcpDoorConfig {
    TcpDoorConfig {
        listen: listen.parse().unwrap(),
        first_byte_timeout: Duration::from_millis(500),
        tls_handshake_timeout: Duration::from_millis(800),
        ice_probation: Duration::from_millis(700),
        ice_write_timeout: Duration::from_millis(700),
        ..TcpDoorConfig::default()
    }
}

struct Door {
    fd: FrontDoor,
    addr: SocketAddr,
    site: Arc<FixedSite>,
}

fn start(cfg: TcpDoorConfig) -> Door {
    let mut door = udp_door();
    let site = Arc::new(FixedSite::default());
    let dyn_site: Arc<dyn SiteService> = site.clone();
    let addr = door
        .start_tcp(
            cfg,
            TlsRouting {
                site: Some(site_config()),
                acme: None,
            },
            Some(dyn_site),
        )
        .unwrap();
    Door {
        fd: door,
        addr,
        site,
    }
}

fn client_rt() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap()
}

fn loopback(addr: SocketAddr, v6: bool) -> SocketAddr {
    let ip = if v6 { "::1" } else { "127.0.0.1" };
    SocketAddr::new(ip.parse().unwrap(), addr.port())
}

/// A test consumer on the door's runtime: answers every lane frame with
/// `b"\x01\x01reply"` (a STUN-shaped reply), echoing the peer.
fn answering_consumer(door: &mut FrontDoor) -> multiview_frontdoor::IceTcpSender {
    let mut lane = door.take_ice_tcp_lane().unwrap();
    let sender = lane.sender();
    let reply = sender.clone();
    door.handle().spawn(async move {
        while let Some(f) = lane.recv().await {
            assert_ne!(f.payload.len(), 0);
            let mut answer = b"\x01\x01".to_vec();
            answer.extend_from_slice(&f.payload);
            let _ = reply.send(f.peer, &answer);
        }
    });
    sender
}

#[test]
fn tls_h1_and_ice_tcp_on_one_port() {
    for v6 in [true, false] {
        let mut d = start(fast("[::]:0"));
        let mut lane = d.fd.take_ice_tcp_lane().unwrap();
        let sender = lane.sender();
        let addr = loopback(d.addr, v6);
        let rt = client_rt();
        rt.block_on(async {
            let resp = https_get(addr, "localhost").await.unwrap();
            assert!(resp.starts_with("HTTP/1.1 200"), "{resp}");
            assert_eq!(d.site.served.load(Ordering::Relaxed), 1);

            let mut ice = TcpStream::connect(addr).await.unwrap();
            let req = binding_request("door:client");
            ice.write_all(&frame(&req)).await.unwrap();
            let got = tokio::time::timeout(Duration::from_secs(3), lane.recv())
                .await
                .unwrap()
                .unwrap();
            assert_eq!(got.payload, req);
            assert_eq!(
                got.peer,
                multiview_frontdoor::classify::canonical(ice.local_addr().unwrap())
            );
            assert_eq!(got.local.port(), addr.port());
            assert_eq!(got.local.is_ipv6(), v6, "local is unmapped: {}", got.local);
            assert_eq!(
                sender.send(got.peer, b"\x01\x01answer"),
                SendOutcome::Queued
            );
            let mut len = [0u8; 2];
            ice.read_exact(&mut len).await.unwrap();
            let mut body = vec![0u8; usize::from(u16::from_be_bytes(len))];
            ice.read_exact(&mut body).await.unwrap();
            assert_eq!(body, b"\x01\x01answer");
            // Out of probation: a frame larger than 1535 B flows now.
            let big = vec![0x80u8; 4000];
            ice.write_all(&frame(&big)).await.unwrap();
            let got = tokio::time::timeout(Duration::from_secs(3), lane.recv())
                .await
                .unwrap()
                .unwrap();
            assert_eq!(got.payload.len(), 4000);
            // The consumer can drop it.
            assert!(sender.close(got.peer));
            assert!(closed_within(&mut ice, Duration::from_secs(2)).await);
        });
        let s = d.fd.stats().snapshot();
        assert_eq!(s.ice_tcp_accepted, 1, "{s:?}");
        assert_eq!(s.tcp_tls_site, 1, "{s:?}");
        d.fd.shutdown_blocking();
    }
}

#[test]
fn ice_tcp_without_an_answer_expires_in_probation() {
    let mut d = start(fast("[::1]:0"));
    let mut lane = d.fd.take_ice_tcp_lane().unwrap();
    let rt = client_rt();
    rt.block_on(async {
        let mut ice = TcpStream::connect(d.addr).await.unwrap();
        ice.write_all(&frame(&binding_request("a:b")))
            .await
            .unwrap();
        assert!(lane.recv().await.is_some());
        let t = std::time::Instant::now();
        assert!(closed_within(&mut ice, Duration::from_secs(3)).await);
        assert!(
            t.elapsed() < Duration::from_millis(1500),
            "{:?}",
            t.elapsed()
        );
    });
    assert_eq!(d.fd.stats().snapshot().ice_tcp_probation_expired, 1);
    d.fd.shutdown_blocking();
}

#[test]
fn ice_tcp_first_frame_and_probation_gates() {
    let mut d = start(fast("[::1]:0"));
    let _lane = d.fd.take_ice_tcp_lane().unwrap();
    let rt = client_rt();
    let addr = d.addr;
    let check = |bytes: Vec<u8>| async move {
        let mut s = TcpStream::connect(addr).await.unwrap();
        let _ = s.write_all(&bytes).await;
        closed_within(&mut s, Duration::from_secs(2)).await
    };
    rt.block_on(async {
        // First frame 1536 B: closed at classification.
        let mut big = 1536u16.to_be_bytes().to_vec();
        big.extend_from_slice(&[0; 1536]);
        assert!(check(big).await, "1536");
        // Non-STUN first frame.
        assert!(check(frame(&[0x80; 40])).await, "non-STUN");
        // A Binding Request without ':' in USERNAME.
        assert!(check(frame(&binding_request("nocolon"))).await, "username");
        // Garbage first byte.
        assert!(check(b"GET / HTTP/1.1\r\n\r\n".to_vec()).await, "G");
        // Zero-length frame after a valid first frame.
        let mut zero = frame(&binding_request("a:b"));
        zero.extend_from_slice(&[0, 0]);
        assert!(check(zero).await, "zero-length");
        // Probation: a non-STUN frame before any answer.
        let mut rtp = frame(&binding_request("a:b"));
        rtp.extend_from_slice(&frame(&[0x80; 100]));
        assert!(check(rtp).await, "probation non-STUN");
        // Probation: a 1536 B frame before any answer.
        let mut long = frame(&binding_request("a:b"));
        long.extend_from_slice(&1536u16.to_be_bytes());
        long.extend_from_slice(&[0; 1536]);
        assert!(check(long).await, "probation size");
        // Probation: a 9th frame.
        let mut many = Vec::new();
        for _ in 0..9 {
            many.extend_from_slice(&frame(&binding_request("a:b")));
        }
        assert!(check(many).await, "probation count");
    });
    let s = d.fd.stats().snapshot();
    assert_eq!(
        s.tcp_classified_close, 2,
        "1536 and G close at classification: {s:?}"
    );
    assert_eq!(s.ice_tcp_rejected, 6, "{s:?}");
    d.fd.shutdown_blocking();
}

#[test]
fn ice_tcp_precheck_and_no_consumer() {
    // No consumer: closed right after the first frame.
    let d = start(fast("[::1]:0"));
    let rt = client_rt();
    rt.block_on(async {
        let mut s = TcpStream::connect(d.addr).await.unwrap();
        s.write_all(&frame(&binding_request("a:b"))).await.unwrap();
        let t = std::time::Instant::now();
        assert!(closed_within(&mut s, Duration::from_secs(2)).await);
        assert!(t.elapsed() < Duration::from_millis(400));
    });
    d.fd.shutdown_blocking();
    // A pre-check that refuses the ufrag.
    let mut d = start(fast("[::1]:0"));
    let _lane = d.fd.take_ice_tcp_lane().unwrap();
    d.fd.set_ice_tcp_username_check(Some(Arc::new(|u: &str| u.starts_with("live:"))))
        .unwrap();
    rt.block_on(async {
        let mut s = TcpStream::connect(d.addr).await.unwrap();
        s.write_all(&frame(&binding_request("dead:b")))
            .await
            .unwrap();
        assert!(closed_within(&mut s, Duration::from_millis(400)).await);
    });
    assert_eq!(d.fd.stats().snapshot().ice_tcp_accepted, 0);
    d.fd.shutdown_blocking();
}

#[test]
fn behind_proxy_fails_closed_and_uses_the_header_address() {
    let mut cfg = fast("[::1]:0");
    cfg.proxy = Some(TrustedProxies::new([("::1".parse().unwrap(), 128)]));
    let d = start(cfg);
    let rt = client_rt();
    let addr = d.addr;
    rt.block_on(async {
        // (a) trusted peer without a PROXY header: TLS ClientHello, then GET.
        let err = https_get(addr, "localhost").await;
        assert!(
            err.map_or(true, |r| r.is_empty()),
            "no response without PROXY v2"
        );
        let mut s = TcpStream::connect(addr).await.unwrap();
        s.write_all(b"GET / HTTP/1.1\r\nHost: x\r\n\r\n")
            .await
            .unwrap();
        assert!(closed_within(&mut s, Duration::from_secs(2)).await);
        // (c) trusted peer with a valid TCP6 header.
        let client: SocketAddr = "[2001:db8::42]:40000".parse().unwrap();
        let header = multiview_frontdoor::tcp::proxy_v2::encode(client, addr, &[]);
        let connector = tokio_rustls::TlsConnector::from(common::client_config(&[b"http/1.1"]));
        let mut tcp = TcpStream::connect(addr).await.unwrap();
        tcp.write_all(&header).await.unwrap();
        let mut tls = connector
            .connect(
                rustls::pki_types::ServerName::try_from("localhost").unwrap(),
                tcp,
            )
            .await
            .unwrap();
        tls.write_all(b"GET / HTTP/1.1\r\n\r\n").await.unwrap();
        let mut out = Vec::new();
        let _ = tls.read_to_end(&mut out).await;
        assert!(out.starts_with(b"HTTP/1.1 200"));
    });
    assert_eq!(
        d.site.served.load(Ordering::Relaxed),
        1,
        "the site saw only the PROXY connection"
    );
    assert_eq!(
        d.site.peers.lock().unwrap().as_slice(),
        &["[2001:db8::42]:40000".parse::<SocketAddr>().unwrap()]
    );
    assert!(d.fd.stats().snapshot().tcp_proxy_rejected >= 2);
    d.fd.shutdown_blocking();

    // (b) an untrusted TCP peer with a valid header.
    let mut cfg = fast("[::]:0");
    cfg.proxy = Some(TrustedProxies::new([("2001:db8::1".parse().unwrap(), 128)]));
    let d = start(cfg);
    rt.block_on(async {
        let addr = loopback(d.addr, false);
        let header = multiview_frontdoor::tcp::proxy_v2::encode(
            "198.51.100.1:1".parse().unwrap(),
            addr,
            &[],
        );
        let mut s = TcpStream::connect(addr).await.unwrap();
        let _ = s.write_all(&header).await;
        assert!(closed_within(&mut s, Duration::from_millis(400)).await);
    });
    assert_eq!(d.site.served.load(Ordering::Relaxed), 0);
    d.fd.shutdown_blocking();
}

#[test]
fn redirect_ignores_the_host_header() {
    let mut door = udp_door();
    let addr = door
        .start_redirect(RedirectConfig::new(
            "[::1]:0".parse().unwrap(),
            "mda001.example.net".to_owned(),
            443,
        ))
        .unwrap();
    client_rt().block_on(async {
        let mut s = TcpStream::connect(addr).await.unwrap();
        s.write_all(b"GET /a?b=1 HTTP/1.1\r\nHost: evil.example\r\n\r\n")
            .await
            .unwrap();
        let mut out = String::new();
        s.read_to_string(&mut out).await.unwrap();
        assert!(
            out.starts_with("HTTP/1.1 301 Moved Permanently\r\n"),
            "{out}"
        );
        assert!(
            out.contains("\r\nLocation: https://mda001.example.net/a?b=1\r\n"),
            "{out}"
        );
        assert!(!out.contains("evil"));
    });
    assert_eq!(door.stats().snapshot().redirect_served, 1);
    door.shutdown_blocking();
}

#[test]
fn deadlines_close_silent_and_stalled_clients() {
    let d = start(fast("[::1]:0"));
    client_rt().block_on(async {
        // Silent: closed at the first-byte deadline (500 ms).
        let mut s = TcpStream::connect(d.addr).await.unwrap();
        let t = std::time::Instant::now();
        assert!(closed_within(&mut s, Duration::from_secs(3)).await);
        let e = t.elapsed();
        assert!(
            e >= Duration::from_millis(400) && e < Duration::from_millis(1500),
            "{e:?}"
        );
        // 0x16 + 10 bytes then stall: closed at the handshake deadline (800 ms).
        let mut s = TcpStream::connect(d.addr).await.unwrap();
        s.write_all(&[0x16, 3, 1, 0, 200, 1, 0, 0, 196, 3, 3])
            .await
            .unwrap();
        let t = std::time::Instant::now();
        assert!(closed_within(&mut s, Duration::from_secs(3)).await);
        let e = t.elapsed();
        assert!(
            e >= Duration::from_millis(600) && e < Duration::from_millis(2000),
            "{e:?}"
        );
    });
    let s = d.fd.stats().snapshot();
    assert_eq!(s.tcp_first_byte_timeout, 1, "{s:?}");
    assert_eq!(s.tcp_tls_handshake_timeout, 1, "{s:?}");
    d.fd.shutdown_blocking();
}

#[test]
fn per_source_cap_refuses_only_that_source() {
    let mut cfg = fast("[::]:0");
    cfg.max_per_source = 3;
    cfg.first_byte_timeout = Duration::from_secs(5);
    let d = start(cfg);
    client_rt().block_on(async {
        let v4 = loopback(d.addr, false);
        let mut held = Vec::new();
        for _ in 0..3 {
            held.push(TcpStream::connect(v4).await.unwrap());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
        let mut extra = TcpStream::connect(v4).await.unwrap();
        assert!(closed_within(&mut extra, Duration::from_millis(500)).await);
        // Another source (IPv6 loopback) is admitted and served.
        let resp = https_get(loopback(d.addr, true), "localhost")
            .await
            .unwrap();
        assert!(resp.starts_with("HTTP/1.1 200"), "{resp}");
        drop(held);
    });
    let s = d.fd.stats().snapshot();
    assert_eq!(s.tcp_refused_per_source, 1, "{s:?}");
    d.fd.shutdown_blocking();
}

#[test]
fn stuck_ice_tcp_reader_is_bounded_and_closed() {
    let mut d = start(fast("[::1]:0"));
    let mut lane = d.fd.take_ice_tcp_lane().unwrap();
    let sender = lane.sender();
    client_rt().block_on(async {
        // A tiny receive buffer so the kernel queue fills quickly.
        let sock = TcpSocket::new_v6().unwrap();
        sock.set_recv_buffer_size(4096).unwrap();
        let mut stuck = sock.connect(d.addr).await.unwrap();
        stuck
            .write_all(&frame(&binding_request("a:b")))
            .await
            .unwrap();
        let peer = lane.recv().await.unwrap().peer;
        // A healthy peer alongside.
        let mut healthy = TcpStream::connect(d.addr).await.unwrap();
        healthy
            .write_all(&frame(&binding_request("c:d")))
            .await
            .unwrap();
        let healthy_peer = lane.recv().await.unwrap().peer;
        let payload = vec![0x80u8; 16 * 1024];
        let t = std::time::Instant::now();
        let mut max_queued = 0;
        let mut closed = false;
        while t.elapsed() < Duration::from_secs(4) {
            for _ in 0..64 {
                if sender.send(peer, &payload) == SendOutcome::NoConnection {
                    closed = true;
                }
            }
            max_queued = max_queued.max(sender.queued(peer));
            assert!(sender.queued_bytes(peer) <= 256 * 1024 + 2 * 16 * 1024);
            let _ = sender.send(healthy_peer, b"\x01\x01hi");
            if closed {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(closed, "stuck peer closed within the bound");
        assert!(max_queued <= 256, "{max_queued}");
        // The healthy connection still works.
        let mut len = [0u8; 2];
        healthy.read_exact(&mut len).await.unwrap();
        assert_eq!(
            sender.send(healthy_peer, b"\x01\x01ok"),
            SendOutcome::Queued
        );
        assert_eq!(sender.connections(), 1);
        drop(stuck);
    });
    let s = d.fd.stats().snapshot();
    assert!(s.ice_tcp_dropped_out > 0, "{s:?}");
    assert!(s.ice_tcp_closed_stuck >= 1, "{s:?}");
    d.fd.shutdown_blocking();
}

/// A peer that keeps draining a trickle (512 bytes every 50 ms, a tiny
/// receive buffer) while the consumer pushes 16 KiB frames continuously never
/// catches up: the writer closes it as stuck within 3 x the write timeout,
/// even though every write makes some progress (F1, PR #328).
#[test]
fn trickling_ice_tcp_reader_is_closed_as_stuck() {
    trickle_case(16 * 1024);
}

/// The same trickle with 512-byte frames: every single write completes well
/// within the write timeout (512 bytes per 50 ms), so only the writer's
/// backlog rule (queue at or over its high-water mark for the write timeout)
/// can close it.
#[test]
fn trickling_ice_tcp_reader_with_small_frames_is_closed_as_stuck() {
    trickle_case(512);
}

fn trickle_case(frame_len: usize) {
    let write_timeout = Duration::from_millis(300);
    let mut d = start(TcpDoorConfig {
        ice_write_timeout: write_timeout,
        ..fast("[::1]:0")
    });
    let mut lane = d.fd.take_ice_tcp_lane().unwrap();
    let sender = lane.sender();
    let stats = d.fd.stats();
    let elapsed = client_rt().block_on(async {
        let sock = TcpSocket::new_v6().unwrap();
        sock.set_recv_buffer_size(4096).unwrap();
        let trickle = sock.connect(d.addr).await.unwrap();
        let (mut rd, mut wr) = trickle.into_split();
        wr.write_all(&frame(&binding_request("a:b"))).await.unwrap();
        let peer = lane.recv().await.unwrap().peer;
        // The trickle: 512 bytes every 50 ms until EOF or error.
        let drained = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let reader = tokio::spawn({
            let drained = Arc::clone(&drained);
            async move {
                let mut buf = [0u8; 512];
                loop {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    match rd.read(&mut buf).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => drained.fetch_add(n, Ordering::Relaxed),
                    };
                }
            }
        });
        let payload = vec![0x80u8; frame_len];
        let t = std::time::Instant::now();
        let mut closed_after = None;
        while t.elapsed() < Duration::from_secs(5) {
            if sender.send(peer, &payload) == SendOutcome::NoConnection {
                closed_after = Some(t.elapsed());
                break;
            }
            if frame_len >= 4096 {
                tokio::time::sleep(Duration::from_millis(2)).await;
            } else {
                tokio::task::yield_now().await;
            }
        }
        // The peer only sees the FIN after draining what the kernel already
        // buffered, at trickle speed: stop reading instead of waiting for it.
        reader.abort();
        let drained = drained.load(Ordering::Relaxed);
        eprintln!(
            "trickle ({frame_len} B frames): peer drained {drained} bytes, closed after {closed_after:?}"
        );
        assert!(drained > 0, "the trickle really read");
        drop(wr);
        closed_after.expect("the trickling peer was closed")
    });
    assert!(
        elapsed <= write_timeout * 3,
        "closed after {elapsed:?}, bound {:?}",
        write_timeout * 3
    );
    let gone = (0..100).any(|_| {
        if stats.tcp_open_connections.load(Ordering::Relaxed) == 0 {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
        false
    });
    let s = stats.snapshot();
    assert!(gone, "the open-connections gauge returns to 0: {s:?}");
    assert!(s.ice_tcp_closed_stuck >= 1, "{s:?}");
    d.fd.shutdown_blocking();
}

#[test]
fn answering_consumer_round_trip_over_ipv4_mapped() {
    let mut d = start(fast("[::]:0"));
    let _sender = answering_consumer(&mut d.fd);
    client_rt().block_on(async {
        let mut s = TcpStream::connect(loopback(d.addr, false)).await.unwrap();
        let req = binding_request("x:y");
        s.write_all(&frame(&req)).await.unwrap();
        let mut len = [0u8; 2];
        s.read_exact(&mut len).await.unwrap();
        let mut body = vec![0u8; usize::from(u16::from_be_bytes(len))];
        s.read_exact(&mut body).await.unwrap();
        assert_eq!(&body[..2], b"\x01\x01");
        assert_eq!(&body[2..], &req[..]);
    });
    d.fd.shutdown_blocking();
}

#[test]
fn ipv4_unspecified_is_never_bound() {
    let mut door = udp_door();
    let err = door.start_tcp(fast("0.0.0.0:0"), TlsRouting::default(), None);
    assert!(err.is_err());
    door.shutdown_blocking();
}
