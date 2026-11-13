//! QUIC Retry policy (`network.quic.retry_above_handshakes`, RFC 9000
//! section 8.1.2; SPEC-B §1). Feature `webrtc` is NOT required: the QUIC
//! lane alone is enough to exercise the vendored server's retry path.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::doc_markdown,
    clippy::default_trait_access
)]

mod common;

use std::io::{self, IoSliceMut};
use std::net::SocketAddr;
use std::num::NonZeroUsize;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use multiview_frontdoor::{FrontDoor, FrontDoorConfig};
use noq::udp::RecvMeta;

const TIMEOUT: Duration = Duration::from_secs(15);

fn config() -> FrontDoorConfig {
    FrontDoorConfig {
        port: 0,
        workers: 2,
        recv_buffer_bytes: 0,
        ..FrontDoorConfig::default()
    }
}

/// Start a MoQ server on the QUIC lane with `retry_above_handshakes` and a
/// retry counter, publishing one track with one frame so a real client has
/// something to subscribe to.
fn start_moq(
    door: &mut FrontDoor,
    retry_above_handshakes: Option<usize>,
    retry_counter: Arc<AtomicU64>,
) -> tokio::task::JoinHandle<anyhow::Result<()>> {
    let lane = door.take_quic_lane().unwrap();
    door.handle().spawn(async move {
        let origin = moq_tokio::origin::spawn();
        let broadcast = origin.create_broadcast("test").unwrap();
        broadcast.announce(Default::default()).unwrap();
        let track = broadcast.create_track("video", None).unwrap();
        let mut group = track.append_group().unwrap();
        group
            .write_frame(moq_tokio::moq_net::Timestamp::ZERO, b"hello".as_ref())
            .unwrap();
        group.finish().unwrap();

        let mut listen = moq_tokio::listen::Config::default();
        listen.tls.generate = vec!["localhost".into()];
        let mut server = moq_tokio::server::Config::default();
        server.listen = listen;
        server.socket = Some(moq_tokio::noq::Injected {
            socket: Box::new(lane),
            endpoint: multiview_frontdoor::quic::endpoint_config(),
            incoming: Some(multiview_frontdoor::quic::incoming_caps()),
            retry_above_handshakes,
            retry_counter: Some(retry_counter),
        });
        let mut listener = server.init()?.listen().await?;
        let request = listener.accept().await.expect("an incoming session");
        let session = request.with_publisher(&origin).ok().await?;
        let _keep = (broadcast, track);
        let _ = session.closed().await;
        Ok(())
    })
}

/// A real MoQ subscriber: connect over WebTransport, subscribe and read the
/// frame — proving a client still connects end to end through a Retry round
/// trip (the client's QUIC stack validates the token transparently).
async fn moq_subscribe(server: SocketAddr) {
    let sub_origin = moq_tokio::origin::spawn();
    let consumer = sub_origin.consume();
    let mut announcements = consumer.announced();
    let mut client_config = moq_tokio::connect::Config::default();
    client_config.tls.insecure = Some(true);
    client_config.bind = Some("[::1]:0".parse().unwrap());
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
    assert_eq!(frame.payload.as_ref(), b"hello");
}

/// `Some(0)`: every unvalidated Initial is retried, yet a real client still
/// connects (the round trip is transparent to the client), and the retry
/// counter is bumped at least once.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retry_above_zero_handshakes_still_lets_a_real_client_connect() {
    let mut door = FrontDoor::start(&config()).unwrap();
    let addr: SocketAddr = format!("[::1]:{}", door.local_addr().port())
        .parse()
        .unwrap();
    let counter = Arc::new(AtomicU64::new(0));
    start_moq(&mut door, Some(0), Arc::clone(&counter));
    tokio::time::sleep(Duration::from_millis(200)).await;
    moq_subscribe(addr).await;
    assert!(
        counter.load(Ordering::Relaxed) >= 1,
        "retry_above_handshakes = Some(0) must retry at least the one real handshake"
    );
}

/// `None`: the original behaviour, byte for byte — the counter never moves,
/// whatever handshakes happen.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retry_disabled_by_none_never_bumps_the_counter() {
    let mut door = FrontDoor::start(&config()).unwrap();
    let addr: SocketAddr = format!("[::1]:{}", door.local_addr().port())
        .parse()
        .unwrap();
    let counter = Arc::new(AtomicU64::new(0));
    start_moq(&mut door, None, Arc::clone(&counter));
    tokio::time::sleep(Duration::from_millis(200)).await;
    moq_subscribe(addr).await;
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
/// datagram (counting it first). The client keeps sending real Initials and
/// retransmitting on PTO, and never completes a handshake: a stalled peer
/// (a spoofed source, or a client that went away), seen from the server.
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
        meta: &mut [RecvMeta],
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

/// One stalled client: a real noq endpoint on `[::1]:0` connecting to
/// `server` with ALPN `h3`. Its handshake task runs in the background and
/// never completes; keep the returned endpoint alive for the test.
fn stalled_client(server: SocketAddr) -> (noq::Endpoint, Arc<WireStats>) {
    let runtime = noq::default_runtime().unwrap();
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
    let tls = Arc::unwrap_or_clone(common::client_config(&[b"h3"]));
    let quic: noq::crypto::rustls::QuicClientConfig = tls.try_into().unwrap();
    let connecting = endpoint
        .connect_with(noq::ClientConfig::new(Arc::new(quic)), server, "localhost")
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
/// Initial): one noq connection per such client.
fn connections_created(clients: &[(noq::Endpoint, Arc<WireStats>)]) -> usize {
    clients
        .iter()
        .filter(|(_, s)| s.recv_initial.load(Ordering::Relaxed) > 0)
        .count()
}

const STALLED: usize = 8;
const THRESHOLD: usize = 4;

/// Under load: `STALLED` real clients whose handshakes never complete fill
/// the server's in-flight count (the vendored accept pipeline holds each
/// one until the setup deadline). With `retry_above_handshakes =
/// Some(THRESHOLD)`, at most `THRESHOLD` of them get connection state; the
/// rest only ever get Retries (no state). A normal client arriving now is
/// Retried too (the counter rises past the stalled-phase value) and still
/// completes end to end.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stalled_handshakes_trip_the_retry_threshold_and_a_real_client_still_connects() {
    let mut door = FrontDoor::start(&config()).unwrap();
    let addr: SocketAddr = format!("[::1]:{}", door.local_addr().port())
        .parse()
        .unwrap();
    let counter = Arc::new(AtomicU64::new(0));
    start_moq(&mut door, Some(THRESHOLD), Arc::clone(&counter));
    tokio::time::sleep(Duration::from_millis(200)).await;

    let clients: Vec<_> = (0..STALLED).map(|_| stalled_client(addr)).collect();
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
        created <= THRESHOLD + 1,
        "{created} connections created with threshold {THRESHOLD}: Retry must stop handshake state"
    );
    assert!(
        after_stalled >= u64::try_from(STALLED - THRESHOLD).unwrap(),
        "the stalled clients above the threshold must have been retried (counter {after_stalled})"
    );

    moq_subscribe(addr).await;
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
    let mut door = FrontDoor::start(&config()).unwrap();
    let addr: SocketAddr = format!("[::1]:{}", door.local_addr().port())
        .parse()
        .unwrap();
    let counter = Arc::new(AtomicU64::new(0));
    start_moq(&mut door, None, Arc::clone(&counter));
    tokio::time::sleep(Duration::from_millis(200)).await;

    let clients: Vec<_> = (0..STALLED).map(|_| stalled_client(addr)).collect();
    wait_all_answered(&clients, Duration::from_secs(5)).await;
    let created = connections_created(&clients);
    eprintln!("None: {STALLED} stalled clients -> {created} connections created");
    assert_eq!(created, STALLED);
    moq_subscribe(addr).await;
    assert_eq!(counter.load(Ordering::Relaxed), 0);
    drop(clients);
}

/// Real Initials (from real, stalled noq clients) under `Some(0)`: every
/// one is answered with a Retry, and the reply bytes stay at or below the
/// request bytes (a Retry is far smaller than a padded 1200-byte Initial),
/// so the Retry path never amplifies.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn real_initials_under_retry_everything_are_retried_without_amplification() {
    let mut door = FrontDoor::start(&config()).unwrap();
    let addr: SocketAddr = format!("[::1]:{}", door.local_addr().port())
        .parse()
        .unwrap();
    let counter = Arc::new(AtomicU64::new(0));
    start_moq(&mut door, Some(0), Arc::clone(&counter));
    tokio::time::sleep(Duration::from_millis(200)).await;

    let clients: Vec<_> = (0..STALLED).map(|_| stalled_client(addr)).collect();
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
