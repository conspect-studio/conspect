//! Chaos for the TCP door (invariants #1 and #10, issue #290): attack the TCP
//! door while the REAL engine tick loop (`EngineRuntime` + `RealtimePacer`)
//! runs on a separate program runtime.
//!
//! Two runs, same door: `idle` (TCP door up, no attack) and `attack`: an
//! accept/connect flood from many loopback sources (most connections idle, a
//! first-byte slowloris), TLS slowloris (`0x16` then stall), half-open TLS (a
//! full `ClientHello` then nothing), stuck ICE-TCP readers (never read while
//! the consumer pushes megabytes at them), oversized probation frames and
//! garbage first bytes.
//!
//! Asserted (counts and design bounds, not tight percentiles; #321):
//! * cadence: each run emits `elapsed / period` ticks within 2, no repeats;
//! * lateness under attack: p99 no worse than idle by more than
//!   [`P99_BOUND_NS`], worst tick no worse by more than one period;
//! * the door's `open_connections` gauge never exceeds `max_connections`;
//! * RSS growth during the attack stays under [`RSS_BOUND_KB`];
//! * Linux: the process fd count returns to its baseline (+ slack) after the
//!   attack stops (every refused / timed-out connection was really closed).
//!
//! Duration per run: `FRONTDOOR_TCP_CHAOS_SECS` (default 4 s), sized for a
//! 2-core CI runner together with the attack threads.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::too_many_lines,
    clippy::as_conversions,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_precision_loss,
    clippy::print_stderr
)]

#[path = "common/clock.rs"]
mod clock;
mod common;

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use clock::{rss_kb, run_clock, ClockRun, FPS};
use common::{binding_request, frame, site_config, FixedSite};
use multiview_frontdoor::{FrontDoor, FrontDoorConfig, SiteService, TcpDoorConfig, TlsRouting};
use tokio::io::AsyncWriteExt;
use tokio::net::TcpSocket;

const P99_BOUND_NS: i64 = 4_000_000;
const RSS_BOUND_KB: u64 = 64 * 1024;
const MAX_CONNECTIONS: usize = 512;
const FD_SLACK: usize = 32;

fn secs() -> u64 {
    std::env::var("FRONTDOOR_TCP_CHAOS_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(4)
}

fn fd_count() -> Option<usize> {
    std::fs::read_dir("/proc/self/fd").ok().map(Iterator::count)
}

fn report(name: &str, r: &ClockRun) {
    eprintln!(
        "tcp chaos {name}: ticks {} (expected {}) repeats {} p50 {:.3} ms p99 {:.3} ms max {:.3} ms late>2ms {}",
        r.ticks,
        r.expected,
        r.repeats,
        r.p50_ns as f64 / 1e6,
        r.p99_ns as f64 / 1e6,
        r.max_ns as f64 / 1e6,
        r.late_over_2ms
    );
}

fn assert_cadence(name: &str, r: &ClockRun) {
    assert!(
        r.ticks + 2 >= r.expected && r.ticks <= r.expected + 2,
        "{name}: one frame per tick: {r:?}"
    );
    assert_eq!(r.repeats, 0, "{name}: no tick fell behind compose: {r:?}");
}

/// A raw TLS 1.3 `ClientHello` (bytes only; never completed).
fn client_hello() -> Vec<u8> {
    let cfg = common::client_config(&[b"http/1.1"]);
    let mut conn = rustls::ClientConnection::new(
        cfg,
        rustls::pki_types::ServerName::try_from("localhost").unwrap(),
    )
    .unwrap();
    let mut out = Vec::new();
    while conn.wants_write() {
        conn.write_tls(&mut out).unwrap();
    }
    out
}

/// One connection from loopback source `127.0.0.<src>` (Linux routes 127/8),
/// falling back to 127.0.0.1.
async fn connect_from(
    src: u8,
    target: SocketAddr,
    rcvbuf: Option<u32>,
) -> Option<tokio::net::TcpStream> {
    let sock = TcpSocket::new_v4().ok()?;
    if let Some(size) = rcvbuf {
        let _ = sock.set_recv_buffer_size(size);
    }
    let local: SocketAddr = format!("127.0.0.{src}:0").parse().ok()?;
    if sock.bind(local).is_err() {
        let _ = sock.bind("127.0.0.1:0".parse().ok()?);
    }
    tokio::time::timeout(Duration::from_secs(1), sock.connect(target))
        .await
        .ok()?
        .ok()
}

struct Attack {
    stop: Arc<AtomicBool>,
    connects: Arc<AtomicU64>,
    rt: tokio::runtime::Runtime,
}

fn start_attack(target: SocketAddr) -> Attack {
    let stop = Arc::new(AtomicBool::new(false));
    let connects = Arc::new(AtomicU64::new(0));
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .thread_name("attack")
        .enable_all()
        .build()
        .unwrap();
    let hello = Arc::new(client_hello());
    for kind in 0..6u8 {
        for lane in 0..4u8 {
            let stop = Arc::clone(&stop);
            let connects = Arc::clone(&connects);
            let hello = Arc::clone(&hello);
            rt.spawn(async move {
                let mut n = 0u32;
                let mut held = std::collections::VecDeque::new();
                while !stop.load(Ordering::Relaxed) {
                    n = n.wrapping_add(1);
                    let src = 2 + u8::try_from(
                        (u32::from(kind) * 40 + u32::from(lane) * 10 + n % 10) % 250,
                    )
                    .unwrap();
                    let rcvbuf = (kind == 3).then_some(4096);
                    let Some(mut s) = connect_from(src, target, rcvbuf).await else {
                        tokio::time::sleep(Duration::from_millis(5)).await;
                        continue;
                    };
                    connects.fetch_add(1, Ordering::Relaxed);
                    let _ = match kind {
                        // Connect flood / first-byte slowloris: send nothing.
                        0 => Ok(()),
                        // TLS slowloris: one byte of a record, then stall.
                        1 => s.write_all(&[0x16, 0x03, 0x01]).await,
                        // Half-open TLS: a full ClientHello, then nothing.
                        2 => s.write_all(&hello).await,
                        // Stuck ICE-TCP reader: Binding Request, never read.
                        3 => s.write_all(&frame(&binding_request("door:stuck"))).await,
                        // Oversized probation frame after a valid first frame.
                        4 => {
                            let mut b = frame(&binding_request("door:big"));
                            b.extend_from_slice(&4000u16.to_be_bytes());
                            b.extend_from_slice(&[0u8; 4000]);
                            s.write_all(&b).await
                        }
                        // Garbage first bytes.
                        _ => s.write_all(b"\x7fGARBAGE\x00\x00\xff\xff").await,
                    };
                    held.push_back(s);
                    // Hold up to 256 per task (the slowloris population, far
                    // above the door's cap); the door's deadlines close them.
                    if held.len() > 256 {
                        held.pop_front();
                    }
                    if n % 8 == 0 {
                        tokio::time::sleep(Duration::from_millis(5)).await;
                    }
                }
            });
        }
    }
    Attack { stop, connects, rt }
}

#[test]
fn tcp_attack_does_not_slip_the_output_clock_and_stays_bounded() {
    let duration = Duration::from_secs(secs());
    let period_ns = 1_000_000_000i64 * FPS.1 / FPS.0;

    let mut door = FrontDoor::start(&FrontDoorConfig {
        port: 0,
        workers: 2,
        ..FrontDoorConfig::default()
    })
    .unwrap();
    let site: Arc<dyn SiteService> = Arc::new(FixedSite::default());
    let addr = door
        .start_tcp(
            TcpDoorConfig {
                listen: "[::]:0".parse().unwrap(),
                max_connections: MAX_CONNECTIONS,
                first_byte_timeout: Duration::from_millis(1000),
                tls_handshake_timeout: Duration::from_millis(1500),
                ice_probation: Duration::from_millis(1500),
                ice_write_timeout: Duration::from_millis(1000),
                ..TcpDoorConfig::default()
            },
            TlsRouting {
                site: Some(site_config()),
                acme: None,
            },
            Some(site),
        )
        .unwrap();
    let target = SocketAddr::new("127.0.0.1".parse().unwrap(), addr.port());
    // The consumer answers every Binding Request (ending probation) and then
    // pushes 16 KiB frames at the last 128 peers it saw: stuck readers
    // overflow (bounded by OUT_QUEUE_BYTES each, 32 MiB in total here).
    let mut lane = door.take_ice_tcp_lane().unwrap();
    let sender = lane.sender();
    door.handle().spawn(async move {
        let mut peers = std::collections::VecDeque::new();
        let push = vec![0x80u8; 16 * 1024];
        let mut tick = tokio::time::interval(Duration::from_millis(10));
        loop {
            tokio::select! {
                f = lane.recv() => {
                    let Some(f) = f else { break };
                    let _ = sender.send(f.peer, b"\x01\x01ok");
                    peers.push_back(f.peer);
                    if peers.len() > 128 { peers.pop_front(); }
                }
                _ = tick.tick() => {
                    for p in &peers {
                        for _ in 0..8 { let _ = sender.send(*p, &push); }
                    }
                }
            }
        }
    });
    let stats = door.stats();

    // 1. Idle.
    let idle = run_clock(duration);
    report("idle", &idle);
    let fds_before = fd_count();
    let rss_before = rss_kb();

    // 2. Attack.
    let max_open = Arc::new(AtomicU64::new(0));
    let sampling = Arc::new(AtomicBool::new(true));
    let sampler = {
        let max_open = Arc::clone(&max_open);
        let sampling = Arc::clone(&sampling);
        let stats = Arc::clone(&stats);
        std::thread::spawn(move || {
            while sampling.load(Ordering::Relaxed) {
                let open = stats.tcp_open_connections.load(Ordering::Relaxed);
                max_open.fetch_max(open, Ordering::Relaxed);
                std::thread::sleep(Duration::from_millis(5));
            }
        })
    };
    let attack = start_attack(target);
    std::thread::sleep(Duration::from_secs(1));
    let on = run_clock(duration);
    attack.stop.store(true, Ordering::Relaxed);
    let connects = attack.connects.load(Ordering::Relaxed);
    attack.rt.shutdown_timeout(Duration::from_secs(2));
    let rss_after = rss_kb();
    report("attack", &on);

    // 3. After the attack: every connection closes (deadlines, stuck close).
    let mut fds_after = fd_count();
    if let Some(before) = fds_before {
        for _ in 0..100 {
            if fds_after.is_some_and(|n| n <= before + FD_SLACK) {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
            fds_after = fd_count();
        }
    }
    sampling.store(false, Ordering::Relaxed);
    sampler.join().unwrap();
    let snap = stats.snapshot();
    eprintln!(
        "tcp chaos: {connects} connects, max open {} (cap {MAX_CONNECTIONS}), rss {rss_before} -> {rss_after} kB, fds {fds_before:?} -> {fds_after:?}, door {snap:?}",
        max_open.load(Ordering::Relaxed)
    );

    assert_cadence("idle", &idle);
    assert_cadence("attack", &on);
    assert!(
        on.p99_ns <= idle.p99_ns + P99_BOUND_NS,
        "p99 under attack {:.3} ms vs idle {:.3} ms",
        on.p99_ns as f64 / 1e6,
        idle.p99_ns as f64 / 1e6
    );
    assert!(
        on.max_ns <= idle.max_ns + period_ns,
        "worst tick under attack more than a period worse: {on:?} vs {idle:?}"
    );
    assert!(
        max_open.load(Ordering::Relaxed) <= MAX_CONNECTIONS as u64,
        "open connections exceeded the cap"
    );
    assert!(connects > 1000, "the attack really ran: {connects}");
    assert!(snap.tcp_first_byte_timeout > 0, "{snap:?}");
    assert!(snap.tcp_tls_handshake_timeout > 0, "{snap:?}");
    assert!(snap.tcp_classified_close > 0, "{snap:?}");
    assert!(snap.ice_tcp_rejected > 0, "{snap:?}");
    assert!(
        rss_after.saturating_sub(rss_before) < RSS_BOUND_KB,
        "RSS grew {rss_before} -> {rss_after} kB"
    );
    if let (Some(before), Some(after)) = (fds_before, fds_after) {
        assert!(
            after <= before + FD_SLACK,
            "fds did not return to baseline: {before} -> {after}"
        );
    }
    assert_eq!(snap.tcp_open_connections, 0, "{snap:?}");
    door.shutdown_blocking();
}
