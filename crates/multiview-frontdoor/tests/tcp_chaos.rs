//! Chaos for the TCP door (invariants #1 and #10, issue #290): attack the TCP
//! door while the REAL engine tick loop (`EngineRuntime` + `RealtimePacer`)
//! runs on a separate program runtime.
//!
//! The attacker runs in-process, so its CPU competes with the clock; the test
//! therefore compares like with like (the `chaos_flood.rs` pattern, #321,
//! #284). Three runs, the same paced attack generator in two of them:
//! * `idle`: the TCP door up, no attack;
//! * `attack_door_on`: the attack against the TCP door: an accept/connect
//!   flood from many loopback sources (most connections idle, a first-byte
//!   slowloris), TLS slowloris (`0x16` then stall), half-open TLS (a full
//!   `ClientHello` then nothing), stuck ICE-TCP readers (never read while the
//!   consumer pushes megabytes at them), oversized probation frames and
//!   garbage first bytes;
//! * `attack_door_off`: the door shut down and the same port served by a
//!   trivial baseline (a plain `TcpListener` on its own 2-worker runtime that
//!   reads and discards each connection with a 5 s deadline), the same attack.
//!
//! The attack is paced to `FRONTDOOR_TCP_CHAOS_CPS` connection attempts per
//! second in total (default 2000: far above the 512 cap's turnover, so every
//! attack kind runs and the held population stays far above the cap).
//!
//! Asserted (counts and design bounds, not tight percentiles):
//! * cadence: each run emits `elapsed / period` ticks within 2; `idle` has no
//!   repeats and the door-on run at most one more than the door-off run;
//! * lateness: door-on p99 no worse than the worse of door-off and idle by
//!   more than [`P99_BOUND_NS`], worst tick no worse by more than one period;
//! * the door's `open_connections` gauge never exceeds `max_connections`;
//! * RSS growth during the door-on attack stays under [`RSS_BOUND_KB`];
//! * Linux: the process fd count returns to its baseline (+ slack) after the
//!   attack stops (every refused / timed-out connection was really closed);
//! * every door defence fired, stuck ICE-TCP writers included.
//!
//! Duration per run: `FRONTDOOR_TCP_CHAOS_SECS` (default 4 s).
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
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpSocket;

const P99_BOUND_NS: i64 = 4_000_000;
const RSS_BOUND_KB: u64 = 64 * 1024;
const MAX_CONNECTIONS: usize = 512;
const FD_SLACK: usize = 32;
/// Attack tasks: six kinds x four lanes.
const ATTACK_TASKS: u64 = 24;

fn secs() -> u64 {
    std::env::var("FRONTDOOR_TCP_CHAOS_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(4)
}

fn cps() -> u64 {
    std::env::var("FRONTDOOR_TCP_CHAOS_CPS")
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|&v: &u64| v > 0)
        .unwrap_or(2000)
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

fn assert_ticks(name: &str, r: &ClockRun) {
    assert!(
        r.ticks + 2 >= r.expected && r.ticks <= r.expected + 2,
        "{name}: one frame per tick: {r:?}"
    );
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

fn start_attack(target: SocketAddr, cps: u64) -> Attack {
    let stop = Arc::new(AtomicBool::new(false));
    let connects = Arc::new(AtomicU64::new(0));
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .thread_name("attack")
        .enable_all()
        .build()
        .unwrap();
    let hello = Arc::new(client_hello());
    // Each task paces its own attempts so the total is `cps` per second.
    let per_task = Duration::from_micros(1_000_000 * ATTACK_TASKS / cps);
    for kind in 0..6u8 {
        for lane in 0..4u8 {
            let stop = Arc::clone(&stop);
            let connects = Arc::clone(&connects);
            let hello = Arc::clone(&hello);
            rt.spawn(async move {
                let mut n = 0u32;
                let mut held = std::collections::VecDeque::new();
                let mut pace = tokio::time::interval(per_task);
                pace.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                while !stop.load(Ordering::Relaxed) {
                    pace.tick().await;
                    n = n.wrapping_add(1);
                    let src = 2 + u8::try_from(
                        (u32::from(kind) * 40 + u32::from(lane) * 10 + n % 10) % 250,
                    )
                    .unwrap();
                    let rcvbuf = (kind == 3).then_some(4096);
                    let Some(mut s) = connect_from(src, target, rcvbuf).await else {
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
                }
            });
        }
    }
    Attack { stop, connects, rt }
}

impl Attack {
    /// Stop, tear down, and return the number of connects.
    fn stop(self) -> u64 {
        self.stop.store(true, Ordering::Relaxed);
        let connects = self.connects.load(Ordering::Relaxed);
        self.rt.shutdown_timeout(Duration::from_secs(2));
        connects
    }
}

/// The door-off baseline: a dumb server on `port` with its own 2-worker
/// runtime, reading and discarding each connection with a 5 s deadline.
struct Baseline {
    rt: tokio::runtime::Runtime,
    accepted: Arc<AtomicU64>,
}

fn start_baseline(port: u16) -> Baseline {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .thread_name("baseline")
        .enable_all()
        .build()
        .unwrap();
    let accepted = Arc::new(AtomicU64::new(0));
    let listener = rt.block_on(async {
        let sock = TcpSocket::new_v6().unwrap();
        sock.set_reuseaddr(true).unwrap();
        // `[::]` is dual-stack on Linux and macOS by default (bindv6only=0).
        sock.bind(SocketAddr::new("::".parse().unwrap(), port))
            .unwrap();
        sock.listen(1024).unwrap()
    });
    let count = Arc::clone(&accepted);
    rt.spawn(async move {
        loop {
            let Ok((mut s, _)) = listener.accept().await else {
                continue;
            };
            count.fetch_add(1, Ordering::Relaxed);
            tokio::spawn(async move {
                let mut buf = [0u8; 4096];
                let _ = tokio::time::timeout(Duration::from_secs(5), async {
                    while matches!(s.read(&mut buf).await, Ok(n) if n > 0) {}
                })
                .await;
            });
        }
    });
    Baseline { rt, accepted }
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

    let cps = cps();
    let port = addr.port();

    // 1. Idle: door up, no attack.
    let idle = run_clock(duration);
    report("idle", &idle);
    let fds_before = fd_count();
    let rss_before = rss_kb();

    // 2. Attack, door on.
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
    let attack = start_attack(target, cps);
    std::thread::sleep(Duration::from_secs(1));
    let on = run_clock(duration);
    let connects_on = attack.stop();
    let rss_after = rss_kb();
    report("attack_door_on", &on);

    // After the attack: every connection closes (deadlines, stuck close).
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
    for _ in 0..100 {
        if stats.tcp_open_connections.load(Ordering::Relaxed) == 0 {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    sampling.store(false, Ordering::Relaxed);
    sampler.join().unwrap();
    let snap = stats.snapshot();
    eprintln!(
        "tcp chaos door on: {connects_on} connects ({cps} cps paced), max open {} (cap {MAX_CONNECTIONS}), rss {rss_before} -> {rss_after} kB, fds {fds_before:?} -> {fds_after:?}, door {snap:?}",
        max_open.load(Ordering::Relaxed)
    );
    door.shutdown_blocking();

    // 3. Attack, door off: the same port served by the dumb baseline.
    let baseline = start_baseline(port);
    let attack = start_attack(target, cps);
    std::thread::sleep(Duration::from_secs(1));
    let off = run_clock(duration);
    let connects_off = attack.stop();
    report("attack_door_off", &off);
    eprintln!(
        "tcp chaos door off: {connects_off} connects, baseline accepted {}",
        baseline.accepted.load(Ordering::Relaxed)
    );
    baseline.rt.shutdown_timeout(Duration::from_secs(2));

    for (name, r) in [
        ("idle", &idle),
        ("attack_door_on", &on),
        ("attack_door_off", &off),
    ] {
        assert_ticks(name, r);
    }
    assert_eq!(
        idle.repeats, 0,
        "idle: no tick fell behind compose: {idle:?}"
    );
    assert!(
        on.repeats <= off.repeats + 1,
        "repeats with the door on ({}) exceed door off ({}) by more than 1",
        on.repeats,
        off.repeats
    );
    assert!(
        on.p99_ns <= off.p99_ns.max(idle.p99_ns) + P99_BOUND_NS,
        "p99 with the door on ({:.3} ms) exceeds max(door off {:.3} ms, idle {:.3} ms) by more than {} ms",
        on.p99_ns as f64 / 1e6,
        off.p99_ns as f64 / 1e6,
        idle.p99_ns as f64 / 1e6,
        P99_BOUND_NS / 1_000_000
    );
    assert!(
        on.max_ns <= off.max_ns.max(idle.max_ns) + period_ns,
        "worst tick with the door on is more than a period worse: on {on:?} off {off:?} idle {idle:?}"
    );
    assert!(
        max_open.load(Ordering::Relaxed) <= MAX_CONNECTIONS as u64,
        "open connections exceeded the cap"
    );
    assert!(connects_on > 1000, "the attack really ran: {connects_on}");
    assert!(
        connects_off > 1000,
        "the baseline attack really ran: {connects_off}"
    );
    assert!(snap.tcp_first_byte_timeout > 0, "{snap:?}");
    assert!(snap.tcp_tls_handshake_timeout > 0, "{snap:?}");
    assert!(snap.tcp_classified_close > 0, "{snap:?}");
    assert!(snap.ice_tcp_rejected > 0, "{snap:?}");
    assert!(
        snap.ice_tcp_closed_stuck > 0,
        "stuck writers closed: {snap:?}"
    );
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
}
