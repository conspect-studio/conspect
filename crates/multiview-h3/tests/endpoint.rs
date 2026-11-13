//! Endpoint policy: ALPN, 0-RTT off, connection caps, the peers hook, the
//! QUIC Retry policy (slice 3B, `network.quic.retry_above_handshakes`).

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod common;

use std::io::{self, IoSliceMut};
use std::net::SocketAddr;
use std::num::NonZeroUsize;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use common::*;
use multiview_h3::{server_tls, Counters, H3Limits, PeersHook, SourceFilter};

#[test]
fn built_tls_config_is_h3_only_without_early_data() {
    let cert = cert();
    let mut tls = common::server_tls(&cert);
    tls.max_early_data_size = u32::MAX;
    tls.alpn_protocols = vec![b"moql".to_vec(), b"h3".to_vec()];
    tls.key_log = Arc::new(rustls::KeyLogFile::new());
    let built = server_tls(&tls).unwrap();
    assert_eq!(built.max_early_data_size, 0);
    assert_eq!(built.alpn_protocols, vec![b"h3".to_vec()]);
    assert!(!built.key_log.will_log("CLIENT_RANDOM"));
}

#[test]
fn tls_without_tls13_is_rejected() {
    let cert = cert();
    let tls12 = rustls::ServerConfig::builder_with_provider(provider())
        .with_protocol_versions(&[&rustls::version::TLS12])
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(vec![cert.cert.clone()], cert.key.clone_key())
        .unwrap();
    assert!(server_tls(&tls12).is_err());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn alpn_other_than_h3_fails_the_handshake() {
    let server = start(TestSite::new(), None, H3Limits::default(), None).await;
    assert!(quic_connect(&server, b"moql").await.is_err());
    assert!(quic_connect(&server, b"h3").await.is_ok());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn per_source_and_global_caps_refuse() {
    let limits = H3Limits {
        max_connections_per_source: 2,
        ..H3Limits::default()
    };
    let server = start(TestSite::new(), None, limits, None).await;
    let a = quic_connect(&server, b"h3").await.unwrap();
    let b = quic_connect(&server, b"h3").await.unwrap();
    // The cap is charged when the SERVER side of the handshake completes,
    // which can trail the client's; wait for it so the third attempt meets
    // the pre-handshake refusal deterministically.
    wait_accepted(&server, 2).await;
    assert!(
        quic_connect(&server, b"h3").await.is_err(),
        "per-source cap"
    );
    assert_eq!(
        Counters::get(&server.endpoint.counters().connections_refused),
        1
    );
    drop((a, b));

    let limits = H3Limits {
        max_connections: 1,
        ..H3Limits::default()
    };
    let server = start(TestSite::new(), None, limits, None).await;
    let held = quic_connect(&server, b"h3").await.unwrap();
    wait_accepted(&server, 1).await;
    assert!(quic_connect(&server, b"h3").await.is_err(), "global cap");
    // Closing frees the slot.
    held.close(noq::VarInt::from_u32(0), b"");
    let deadline = tokio::time::Instant::now() + TIMEOUT;
    loop {
        if quic_connect(&server, b"h3").await.is_ok() {
            break;
        }
        assert!(tokio::time::Instant::now() < deadline, "slot never freed");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Waits until the server has charged `n` connections against its caps.
async fn wait_accepted(server: &Server, n: u64) {
    let deadline = tokio::time::Instant::now() + TIMEOUT;
    while Counters::get(&server.endpoint.counters().connections_accepted) < n {
        assert!(
            tokio::time::Instant::now() < deadline,
            "server never charged the connection"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn peers_hook_sees_up_then_down() {
    let events: Arc<Mutex<Vec<(SocketAddr, bool)>>> = Arc::default();
    let hook: PeersHook = {
        let events = Arc::clone(&events);
        Arc::new(move |addr, up| events.lock().unwrap().push((addr, up)))
    };
    let server = start(TestSite::new(), None, H3Limits::default(), Some(hook)).await;
    let conn = quic_connect(&server, b"h3").await.unwrap();
    let deadline = tokio::time::Instant::now() + TIMEOUT;
    while events.lock().unwrap().is_empty() {
        assert!(tokio::time::Instant::now() < deadline);
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    conn.close(noq::VarInt::from_u32(0), b"bye");
    while events.lock().unwrap().len() < 2 {
        assert!(tokio::time::Instant::now() < deadline, "no down event");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let events = events.lock().unwrap().clone();
    assert_eq!(events.len(), 2);
    assert!(events[0].1 && !events[1].1);
    assert_eq!(events[0].0, events[1].0);
    server.endpoint.shutdown().await;
}

/// `refuse_source` refuses a matching source before any cap check, and
/// leaves a non-matching source untouched (design note §8: the forwarder
/// hop a `behind_proxy` deployment must fail closed on).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn refuse_source_refuses_the_matched_address_only() {
    let other: SocketAddr = "[2001:db8::1]:1".parse().unwrap();
    let filter: SourceFilter = Arc::new(move |addr: SocketAddr| addr.ip().is_loopback());
    let server = start_with_filter(TestSite::new(), None, H3Limits::default(), None, filter).await;

    assert!(
        quic_connect(&server, b"h3").await.is_err(),
        "a loopback source (the client in this test) is refused"
    );
    let deadline = tokio::time::Instant::now() + TIMEOUT;
    loop {
        if Counters::get(&server.endpoint.counters().sources_refused) >= 1 {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "sources_refused never incremented"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(
        Counters::get(&server.endpoint.counters().connections_accepted),
        0,
        "the refused source never reaches the cap checks"
    );

    // A filter refusing a DIFFERENT address lets the real (loopback) client
    // through normally.
    let other_filter: SourceFilter = Arc::new(move |addr: SocketAddr| addr.ip() == other.ip());
    let server = start_with_filter(
        TestSite::new(),
        None,
        H3Limits::default(),
        None,
        other_filter,
    )
    .await;
    assert!(
        quic_connect(&server, b"h3").await.is_ok(),
        "a non-matching filter admits the real client"
    );
    assert_eq!(
        Counters::get(&server.endpoint.counters().sources_refused),
        0
    );
}

// ---- QUIC Retry policy (RFC 9000 §8.1.2; design note "Slice 3 as built,
// part B"). The same intent as `multiview-frontdoor/tests/retry.rs`, which
// proves it on the vendored MoQ server; this proves it on the endpoint that
// actually serves the front door's QUIC lane since slice 2. ----

/// Limits for the Retry tests: a handshake deadline well past every wait
/// below, so stalled handshakes hold their slots for the whole test.
fn retry_limits() -> H3Limits {
    H3Limits {
        handshake_timeout: Duration::from_secs(30),
        ..H3Limits::default()
    }
}

/// A real h3 request end to end (QUIC + h3 SETTINGS + a site response).
async fn real_request(server: &Server) {
    let mut client = h3_client(server).await;
    let mut stream = tokio::time::timeout(
        TIMEOUT,
        client.send.send_request(request(http::Method::GET, "/")),
    )
    .await
    .expect("request timed out")
    .unwrap();
    stream.finish().await.unwrap();
    let response = tokio::time::timeout(TIMEOUT, stream.recv_response())
        .await
        .expect("response timed out")
        .unwrap();
    assert!(response.status().is_success(), "{}", response.status());
}

/// `Some(0)`: every unvalidated Initial is retried, yet a real client still
/// completes (noq follows the Retry transparently) and serves a request;
/// the counter moved at least once.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retry_above_zero_handshakes_still_lets_a_real_client_connect() {
    let counter = Arc::new(AtomicU64::new(0));
    let server = start_with_retry(retry_limits(), Some(0), Arc::clone(&counter)).await;
    real_request(&server).await;
    assert!(
        counter.load(Ordering::Relaxed) >= 1,
        "retry_above_handshakes = Some(0) must retry at least the one real handshake"
    );
    assert_eq!(
        Counters::get(&server.endpoint.counters().connections_accepted),
        1,
        "the token-carrying second Initial is admitted, once"
    );
}

/// `None`: no Retry is ever sent, whatever handshakes happen.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retry_disabled_by_none_never_bumps_the_counter() {
    let counter = Arc::new(AtomicU64::new(0));
    let server = start_with_retry(retry_limits(), None, Arc::clone(&counter)).await;
    real_request(&server).await;
    // A raw client sees no Retry on the wire either.
    let stalled = stalled_client(&server);
    wait_all_answered(std::slice::from_ref(&stalled), Duration::from_secs(5)).await;
    assert_eq!(stalled.1.recv_retry.load(Ordering::Relaxed), 0);
    assert_eq!(counter.load(Ordering::Relaxed), 0);
}

/// What one stalled client saw on the wire.
#[derive(Debug, Default)]
struct WireStats {
    sent_bytes: AtomicU64,
    sent_datagrams: AtomicU64,
    recv_bytes: AtomicU64,
    recv_datagrams: AtomicU64,
    /// Datagrams whose first packet is a QUIC v1 Retry (long header type 3).
    recv_retry: AtomicU64,
    /// Datagrams whose first packet is a QUIC v1 Initial (long header type
    /// 0): the server created connection state for this client.
    recv_initial: AtomicU64,
}

/// A real noq client socket that sends normally but DISCARDS every received
/// datagram (counting it first): the client keeps sending real Initials and
/// retransmitting on PTO, and never completes a handshake (a stalled peer: a
/// spoofed source, or a client that went away), seen from the server.
#[derive(Debug)]
struct StalledSocket {
    inner: Box<dyn noq::AsyncUdpSocket>,
    stats: Arc<WireStats>,
}

#[derive(Debug)]
struct CountingSender {
    inner: Pin<Box<dyn noq::UdpSender>>,
    stats: Arc<WireStats>,
}

impl noq::UdpSender for CountingSender {
    fn poll_send(
        self: Pin<&mut Self>,
        transmit: &noq::udp::Transmit<'_>,
        cx: &mut Context<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        let polled = this.inner.as_mut().poll_send(transmit, cx);
        if let Poll::Ready(Ok(())) = polled {
            let len = transmit.contents.len();
            let count = transmit
                .segment_size
                .map_or(1, |seg| len.div_ceil(seg.max(1)));
            this.stats
                .sent_bytes
                .fetch_add(u64::try_from(len).unwrap(), Ordering::Relaxed);
            this.stats
                .sent_datagrams
                .fetch_add(u64::try_from(count).unwrap(), Ordering::Relaxed);
        }
        polled
    }
}

impl noq::AsyncUdpSocket for StalledSocket {
    fn create_sender(&self) -> Pin<Box<dyn noq::UdpSender>> {
        Box::pin(CountingSender {
            inner: self.inner.create_sender(),
            stats: Arc::clone(&self.stats),
        })
    }

    fn poll_recv(
        &mut self,
        cx: &mut Context<'_>,
        bufs: &mut [IoSliceMut<'_>],
        meta: &mut [noq::udp::RecvMeta],
    ) -> Poll<io::Result<usize>> {
        // Drain until the inner socket parks (registering the waker), counting
        // and discarding everything: noq never sees a server packet.
        loop {
            match self.inner.poll_recv(cx, bufs, meta) {
                Poll::Ready(Ok(n)) => {
                    for (buf, m) in bufs.iter().zip(meta.iter()).take(n) {
                        let stride = m.stride.max(1);
                        let mut off = 0;
                        while off < m.len {
                            let end = (off + stride).min(m.len);
                            let first = buf[off];
                            self.stats.recv_datagrams.fetch_add(1, Ordering::Relaxed);
                            self.stats
                                .recv_bytes
                                .fetch_add(u64::try_from(end - off).unwrap(), Ordering::Relaxed);
                            if first & 0x80 != 0 {
                                match (first >> 4) & 0x03 {
                                    0 => self.stats.recv_initial.fetch_add(1, Ordering::Relaxed),
                                    3 => self.stats.recv_retry.fetch_add(1, Ordering::Relaxed),
                                    _ => 0,
                                };
                            }
                            off = end;
                        }
                    }
                }
                other => return other,
            }
        }
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        self.inner.local_addr()
    }

    fn max_receive_segments(&self) -> NonZeroUsize {
        self.inner.max_receive_segments()
    }

    fn may_fragment(&self) -> bool {
        self.inner.may_fragment()
    }
}

/// One stalled client: a real noq endpoint on `[::1]:0` connecting to the
/// server with ALPN `h3`. Its handshake never completes; keep the returned
/// endpoint alive for the test.
fn stalled_client(server: &Server) -> (noq::Endpoint, Arc<WireStats>) {
    let runtime: Arc<dyn noq::Runtime> = Arc::new(noq::TokioRuntime);
    let udp = std::net::UdpSocket::bind("[::1]:0").unwrap();
    let stats = Arc::new(WireStats::default());
    let socket = StalledSocket {
        inner: runtime.wrap_udp_socket(udp).unwrap(),
        stats: Arc::clone(&stats),
    };
    let endpoint = noq::Endpoint::new_with_abstract_socket(
        noq::EndpointConfig::default(),
        None,
        Box::new(socket),
        runtime,
    )
    .unwrap();
    let connecting = endpoint
        .connect_with(client_config(&server.cert, b"h3"), server.addr, "localhost")
        .unwrap();
    tokio::spawn(async move {
        let _never = connecting.await;
    });
    (endpoint, stats)
}

/// Wait until every stalled client has heard back from the server (a Retry
/// or a server Initial), or `deadline` passes.
async fn wait_all_answered(clients: &[(noq::Endpoint, Arc<WireStats>)], deadline: Duration) {
    let end = Instant::now() + deadline;
    while Instant::now() < end {
        if clients.iter().all(|(_, s)| {
            s.recv_retry.load(Ordering::Relaxed) + s.recv_initial.load(Ordering::Relaxed) > 0
        }) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Clients the server created connection state for (it sent them an
/// Initial).
fn connections_created(clients: &[(noq::Endpoint, Arc<WireStats>)]) -> usize {
    clients
        .iter()
        .filter(|(_, s)| s.recv_initial.load(Ordering::Relaxed) > 0)
        .count()
}

const STALLED: usize = 8;
const THRESHOLD: usize = 4;

/// Under load: `STALLED` real clients whose handshakes never complete hold
/// handshake slots. With `retry_above_handshakes = Some(THRESHOLD)`, at most
/// `THRESHOLD` of them get connection state (the accept loop counts slots
/// exactly, so not even one more); the rest only ever get Retries. A normal
/// client arriving now is Retried too (the counter rises past the
/// stalled-phase value) and still completes a request end to end.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stalled_handshakes_trip_the_retry_threshold_and_a_real_client_still_connects() {
    let counter = Arc::new(AtomicU64::new(0));
    let server = start_with_retry(retry_limits(), Some(THRESHOLD), Arc::clone(&counter)).await;

    let clients: Vec<_> = (0..STALLED).map(|_| stalled_client(&server)).collect();
    wait_all_answered(&clients, Duration::from_secs(5)).await;
    let created = connections_created(&clients);
    let retried_clients = clients
        .iter()
        .filter(|(_, s)| s.recv_retry.load(Ordering::Relaxed) > 0)
        .count();
    let after_stalled = counter.load(Ordering::Relaxed);
    eprintln!(
        "Some({THRESHOLD}): {STALLED} stalled clients -> {created} connections created, \
         {retried_clients} clients retried, retry counter {after_stalled}"
    );
    assert_eq!(
        created + retried_clients,
        STALLED,
        "every stalled client must be answered"
    );
    assert!(
        created <= THRESHOLD,
        "{created} connections created with threshold {THRESHOLD}: Retry must stop handshake state"
    );
    assert!(
        after_stalled >= u64::try_from(STALLED - THRESHOLD).unwrap(),
        "the stalled clients above the threshold must have been retried (counter {after_stalled})"
    );

    real_request(&server).await;
    let after_normal = counter.load(Ordering::Relaxed);
    assert!(
        after_normal > after_stalled,
        "the normal client arrived with {THRESHOLD}+ handshakes in flight and must have been \
         Retried ({after_stalled} -> {after_normal})"
    );
    drop(clients);
}

/// Control for the test above: the same stalled load with `None` never
/// retries, and every stalled client gets connection state.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stalled_handshakes_without_a_threshold_are_never_retried() {
    let counter = Arc::new(AtomicU64::new(0));
    let server = start_with_retry(retry_limits(), None, Arc::clone(&counter)).await;

    let clients: Vec<_> = (0..STALLED).map(|_| stalled_client(&server)).collect();
    wait_all_answered(&clients, Duration::from_secs(5)).await;
    let created = connections_created(&clients);
    eprintln!("None: {STALLED} stalled clients -> {created} connections created");
    assert_eq!(created, STALLED);
    real_request(&server).await;
    assert_eq!(counter.load(Ordering::Relaxed), 0);
    drop(clients);
}

/// Real Initials (from real, stalled noq clients) under `Some(0)`: every
/// one is answered with a Retry, no handshake state is created, and the
/// reply bytes stay at or below the request bytes (a Retry is far smaller
/// than a padded 1200-byte Initial), so the Retry path never amplifies.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn real_initials_under_retry_everything_are_retried_without_amplification() {
    let counter = Arc::new(AtomicU64::new(0));
    let server = start_with_retry(retry_limits(), Some(0), Arc::clone(&counter)).await;

    let clients: Vec<_> = (0..STALLED).map(|_| stalled_client(&server)).collect();
    wait_all_answered(&clients, Duration::from_secs(5)).await;
    // Let PTO retransmissions add more real Initials and Retries.
    tokio::time::sleep(Duration::from_millis(1_500)).await;
    let sum = |f: fn(&WireStats) -> u64| clients.iter().map(|(_, s)| f(s)).sum::<u64>();
    let sent_bytes = sum(|s| s.sent_bytes.load(Ordering::Relaxed));
    let sent_datagrams = sum(|s| s.sent_datagrams.load(Ordering::Relaxed));
    let recv_bytes = sum(|s| s.recv_bytes.load(Ordering::Relaxed));
    let recv_datagrams = sum(|s| s.recv_datagrams.load(Ordering::Relaxed));
    let retries = sum(|s| s.recv_retry.load(Ordering::Relaxed));
    let initials = sum(|s| s.recv_initial.load(Ordering::Relaxed));
    eprintln!(
        "Some(0): sent {sent_datagrams} datagrams / {sent_bytes} B; got back {recv_datagrams} \
         datagrams / {recv_bytes} B ({retries} Retry, {initials} Initial); counter {}",
        counter.load(Ordering::Relaxed)
    );
    assert!(retries >= 1, "real Initials must be answered with Retries");
    assert_eq!(initials, 0, "Some(0) must never create handshake state");
    assert_eq!(
        Counters::get(&server.endpoint.counters().connections_accepted),
        0
    );
    assert!(
        counter.load(Ordering::Relaxed) >= retries,
        "every Retry on the wire was counted"
    );
    assert!(
        recv_datagrams <= sent_datagrams,
        "{recv_datagrams} reply datagrams for {sent_datagrams} sent"
    );
    assert!(
        recv_bytes <= sent_bytes,
        "reply bytes {recv_bytes} exceed request bytes {sent_bytes}: the Retry path amplifies"
    );
    drop(clients);
}
