//! Amplification caps (design note section 3, "Stateless replies"; issue
//! #289): a real noq endpoint (vendored MoQ server) on the QUIC lane, one real
//! client socket sending for 2 s at 5 000 datagrams/s each of:
//! (a) 1200-byte unsupported-version long headers (Version Negotiation
//!     triggers), (b) supported-version Initials with random content, and
//! (c) short headers with well-formed random 8-byte CIDs (stateless-reset
//!     triggers),
//! counting every reply datagram and byte. Per shape: reply bytes < request
//! bytes, and reply datagrams within the budget the design gives over the run
//! plus its burst (VN: 200/s x secs + 20; reset: secs / 0.2 s + 1; Initial:
//! the handshake budget 200/s x secs + 20).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::print_stderr,
    clippy::default_trait_access,
    clippy::cast_precision_loss,
    clippy::as_conversions,
    clippy::doc_markdown
)]

use std::net::{SocketAddr, UdpSocket};
use std::time::{Duration, Instant};

use multiview_frontdoor::flood::{datagram, Rng, Shape};
use multiview_frontdoor::{FrontDoor, FrontDoorConfig};

const RATE: u64 = 5_000;
const SECS: u64 = 2;

fn start_server(door: &mut FrontDoor) {
    let lane = door.take_quic_lane().unwrap();
    door.handle().spawn(async move {
        let origin = moq_tokio::origin::spawn();
        let mut listen = moq_tokio::listen::Config::default();
        listen.tls.generate = vec!["localhost".into()];
        let mut server = moq_tokio::server::Config::default();
        server.listen = listen;
        server.socket = Some(moq_tokio::noq::Injected {
            socket: Box::new(lane),
            endpoint: multiview_frontdoor::quic::endpoint_config(),
            incoming: Some(multiview_frontdoor::quic::incoming_caps()),
            retry_above_handshakes: None,
            retry_counter: None,
        });
        let mut listener = server.init().unwrap().listen().await.unwrap();
        while let Some(request) = listener.accept().await {
            let _ = request.with_publisher(&origin).ok().await;
        }
    });
}

#[derive(Debug, Default, Clone, Copy)]
struct Tally {
    requests: u64,
    request_bytes: u64,
    replies: u64,
    reply_bytes: u64,
    largest_reply: usize,
}

/// Send `shape` at [`RATE`] for [`SECS`] from a fresh socket, reading every
/// reply (and for 500 ms after).
fn blast(shape: Shape, to: SocketAddr) -> Tally {
    let sock = UdpSocket::bind("[::1]:0").unwrap();
    sock.set_nonblocking(true).unwrap();
    let mut rng = Rng::new(0xa3 ^ shape as u64);
    let mut t = Tally::default();
    let mut buf = [0u8; 2048];
    let mut read = |t: &mut Tally| {
        while let Ok((n, _)) = sock.recv_from(&mut buf) {
            t.replies += 1;
            t.reply_bytes += n as u64;
            t.largest_reply = t.largest_reply.max(n);
        }
    };
    let start = Instant::now();
    let per_tick = RATE / 100;
    let mut tick = 0u64;
    while start.elapsed() < Duration::from_secs(SECS) {
        for _ in 0..per_tick {
            let d = datagram(shape, &mut rng);
            if sock.send_to(&d, to).is_ok() {
                t.requests += 1;
                t.request_bytes += d.len() as u64;
            }
        }
        read(&mut t);
        tick += 1;
        let next = start + Duration::from_millis(10 * tick);
        if let Some(wait) = next.checked_duration_since(Instant::now()) {
            std::thread::sleep(wait);
        }
    }
    let end = Instant::now() + Duration::from_millis(500);
    while Instant::now() < end {
        read(&mut t);
        std::thread::sleep(Duration::from_millis(5));
    }
    t
}

#[test]
fn stateless_replies_stay_within_their_budgets_and_never_amplify() {
    let mut door = FrontDoor::start(&FrontDoorConfig {
        port: 0,
        workers: 2,
        ..FrontDoorConfig::default()
    })
    .unwrap();
    start_server(&mut door);
    std::thread::sleep(Duration::from_millis(300));
    let to: SocketAddr = format!("[::1]:{}", door.local_addr().port())
        .parse()
        .unwrap();
    // Budgets over the run (the run lasts SECS plus the read tail; one
    // extra second covers the tail and scheduling).
    let secs = SECS + 1;
    let vn_bound = 200 * secs + 20;
    let initial_bound = 200 * secs + 20;
    let reset_bound = secs * 5 + 1;
    for (shape, bound, name) in [
        (Shape::VersionNegotiation, vn_bound, "version negotiation"),
        (Shape::Initial, initial_bound, "random initial"),
        (Shape::Short, reset_bound, "stateless reset"),
    ] {
        let before = door.stats().snapshot();
        let t = blast(shape, to);
        let after = door.stats().snapshot();
        eprintln!(
            "amplification {name}: {} requests ({} B) -> {} replies ({} B, largest {} B), \
             ratio {:.4} datagrams {:.4} bytes; bound {bound}; door to_quic +{} dropped vn +{} \
             initial +{} per-source +{}",
            t.requests,
            t.request_bytes,
            t.replies,
            t.reply_bytes,
            t.largest_reply,
            t.replies as f64 / t.requests.max(1) as f64,
            t.reply_bytes as f64 / t.request_bytes.max(1) as f64,
            after.to_quic - before.to_quic,
            after.dropped_vn - before.dropped_vn,
            after.dropped_initial - before.dropped_initial,
            after.dropped_per_source - before.dropped_per_source,
        );
        assert!(
            t.requests >= RATE * SECS * 9 / 10,
            "{name}: sent {}",
            t.requests
        );
        assert!(
            t.reply_bytes < t.request_bytes,
            "{name}: reply bytes {} >= request bytes {}",
            t.reply_bytes,
            t.request_bytes
        );
        assert!(
            t.replies <= bound,
            "{name}: {} replies over the budget {bound}",
            t.replies
        );
        // No single reply larger than 3x its trigger (RFC 9000 section 8.1).
        let trigger = datagram(shape, &mut Rng::new(1)).len();
        assert!(
            t.largest_reply <= 3 * trigger,
            "{name}: reply of {} B to a {trigger} B trigger",
            t.largest_reply
        );
    }
}
