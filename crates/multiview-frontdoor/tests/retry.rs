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

use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use multiview_frontdoor::flood::{datagram, Rng, Shape};
use multiview_frontdoor::{FrontDoor, FrontDoorConfig};

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

/// A flood of supported-version Initial shapes with random keys (never
/// completing a handshake) does not make the server send Retries larger
/// than the Initials that triggered them: the amplification bound already
/// enforced for VN/reset triggers (`tests/amplification.rs`) also holds for
/// the Retry packet itself, with an aggressive retry threshold.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_flood_of_initials_under_an_aggressive_retry_threshold_never_amplifies() {
    let mut door = FrontDoor::start(&config()).unwrap();
    let addr: SocketAddr = format!("[::1]:{}", door.local_addr().port())
        .parse()
        .unwrap();
    let counter = Arc::new(AtomicU64::new(0));
    start_moq(&mut door, Some(0), Arc::clone(&counter));
    tokio::time::sleep(Duration::from_millis(200)).await;

    let sock = UdpSocket::bind("[::1]:0").unwrap();
    sock.set_nonblocking(true).unwrap();
    sock.set_read_timeout(Some(Duration::from_millis(50))).ok();
    let mut rng = Rng::new(0x51);
    let mut sent_bytes = 0u64;
    let mut reply_bytes = 0u64;
    let mut buf = [0u8; 2048];
    for _ in 0..2_000 {
        let d = datagram(Shape::Initial, &mut rng);
        if sock.send_to(&d, addr).is_ok() {
            sent_bytes += u64::try_from(d.len()).unwrap_or(u64::MAX);
        }
    }
    let end = std::time::Instant::now() + Duration::from_millis(500);
    while std::time::Instant::now() < end {
        while let Ok((n, _)) = sock.recv_from(&mut buf) {
            reply_bytes += u64::try_from(n).unwrap_or(u64::MAX);
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        reply_bytes < sent_bytes,
        "retry/response bytes {reply_bytes} must stay below the {sent_bytes} B sent \
         (no amplification even with retry_above_handshakes = Some(0))"
    );
}
