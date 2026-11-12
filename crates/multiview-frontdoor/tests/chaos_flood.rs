//! Chaos (invariants #1 and #10, issue #287): flood the front-door socket while
//! the REAL engine tick loop (`EngineRuntime` + `RealtimePacer` +
//! `MonotonicTimeSource`, the production pacing path) runs on a separate
//! "program" runtime, exactly the topology slice 1 creates.
//!
//! Three runs, same host, same flood:
//! * `idle`: front door up, no flood;
//! * `flood_door_on`: front door up, an established MoQ viewer subscribed, and a
//!   multi-source flood (garbage, supported-version Initial shapes,
//!   unsupported-version Initials, STUN, RTP, short headers) from up to 200
//!   distinct loopback sources (127.0.0.2-201, distinct source keys);
//! * `flood_door_off`: no front door at all, the same flood at the same port.
//!
//! Asserted bounds (stated in the PR):
//! * frame count: every run emits `elapsed / period` ticks within 2 (the clock
//!   holds cadence: one frame per tick, repeats included), and no run repeats;
//! * tick lateness under flood with the door on is no worse than with the door
//!   off (or idle, whichever is worse) by more than [`P99_BOUND_NS`] at p99, and
//!   its worst tick is no worse than theirs by more than one period;
//! * the established viewer receives at least 95% of the groups published while
//!   the flood runs;
//! * memory: RSS growth during the flood stays under [`RSS_BOUND_KB`], and the
//!   per-source table never exceeds its cap.
//!
//! Duration per run: `FRONTDOOR_CHAOS_SECS` (default 6 s). Flood rate:
//! `FRONTDOOR_CHAOS_PPS` (default 20 000 datagrams/s, sized for a 2-core CI
//! runner where the flood threads share the cores with the clock; the PR evidence
//! runs 60 000 pps for 30 s on the 56-core build host).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::too_many_lines,
    clippy::as_conversions,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss,
    clippy::print_stderr,
    clippy::doc_markdown,
    clippy::default_trait_access
)]

use std::collections::HashMap;
use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use moq_tokio::moq_net;
use multiview_compositor::blend::LinearRgba;
use multiview_compositor::pipeline::{CanvasColor, Nv12Image};
use multiview_core::color::ColorInfo;
use multiview_core::layout::{Canvas, Cell, FitMode, Layout};
use multiview_core::time::Rational;
use multiview_engine::{
    CompositedFrame, CompositorDrive, EnginePublisher, EngineRuntime, MonotonicTimeSource,
    OutputClock, RealtimePacer, StopSignal, TimeSource,
};
use multiview_framestore::TileStore;
use multiview_frontdoor::{FrontDoor, FrontDoorConfig};

/// 30 fps: an exact 33.333 ms period expressed as a rational.
const FPS: (i64, i64) = (30, 1);
/// p99 tick lateness with the door on may exceed the door-off run by at most this.
const P99_BOUND_NS: i64 = 4_000_000;
/// RSS growth bound during the flood.
const RSS_BOUND_KB: u64 = 64 * 1024;

fn secs() -> u64 {
    std::env::var("FRONTDOOR_CHAOS_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(6)
}

fn pps() -> u64 {
    std::env::var("FRONTDOOR_CHAOS_PPS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(20_000)
}

fn rss_kb() -> u64 {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("VmRSS:"))
                .and_then(|l| l.split_whitespace().nth(1))
                .and_then(|v| v.parse().ok())
        })
        .unwrap_or(0)
}

fn build_drive() -> CompositorDrive<Nv12Image> {
    let mut stores = HashMap::new();
    stores.insert(
        "cam-a".to_owned(),
        Arc::new(TileStore::<Nv12Image>::with_defaults("cam-a")),
    );
    CompositorDrive::new(
        Arc::new(Layout {
            name: "chaos".to_owned(),
            canvas: Canvas {
                width: 64,
                height: 36,
                fps_num: 30,
                fps_den: 1,
            },
            cells: vec![Cell {
                x: 0.0,
                y: 0.0,
                w: 1.0,
                h: 1.0,
                z: 0,
                fit: FitMode::Contain,
                source: Some("cam-a".to_owned()),
                ..Cell::default()
            }],
        }),
        stores,
        Nv12Image::solid(
            64,
            36,
            16,
            128,
            128,
            ColorInfo::default().resolve_defaults(1920, 1080),
        )
        .unwrap(),
        CanvasColor::default(),
        LinearRgba::TRANSPARENT,
    )
    .unwrap()
}

#[derive(Debug, Clone, Copy)]
struct ClockRun {
    ticks: u64,
    expected: u64,
    repeats: u64,
    p50_ns: i64,
    p99_ns: i64,
    max_ns: i64,
    late_over_2ms: usize,
}

/// Run the real engine tick loop for `duration` on a 2-worker program runtime
/// and measure each tick's lateness against its wall-clock deadline.
fn run_clock(duration: Duration) -> ClockRun {
    let program_rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .thread_name("program")
        .enable_all()
        .build()
        .unwrap();
    program_rt.block_on(async move {
        let time: Arc<dyn TimeSource> = Arc::new(MonotonicTimeSource::new());
        let clock = OutputClock::new(Rational::new(FPS.0, FPS.1)).unwrap();
        let mut runtime =
            EngineRuntime::new(clock, build_drive(), Arc::clone(&time), RealtimePacer);
        let seed = runtime.seed_nanos();
        let period_ns = 1_000_000_000i64 * FPS.1 / FPS.0;
        let publisher: Arc<EnginePublisher<u64, u64>> = Arc::new(EnginePublisher::new(8));
        let lateness = Arc::new(Mutex::new(Vec::<i64>::with_capacity(4096)));
        let lat = Arc::clone(&lateness);
        let ts = Arc::clone(&time);
        let stop = StopSignal::new();
        let stop2 = stop.clone();
        let repeats = runtime.frames_repeated_counter();
        let started = Instant::now();
        let stopper = tokio::spawn(async move {
            tokio::time::sleep(duration).await;
            stop2.stop();
        });
        runtime
            .run(
                &publisher,
                &stop,
                move |f: &CompositedFrame| {
                    // Deadline of this tick on the time source the pacer uses.
                    let index = i64::try_from(f.tick.index).unwrap_or(i64::MAX);
                    let deadline = seed + (index * 1_000_000_000 * FPS.1) / FPS.0;
                    let late = ts.now_nanos() - deadline;
                    if let Ok(mut l) = lat.lock() {
                        l.push(late);
                    }
                    f.tick.index
                },
                |_| None,
            )
            .await
            .unwrap();
        let elapsed = started.elapsed();
        stopper.await.unwrap();
        let raw = lateness.lock().unwrap().clone();
        let mut worst: Vec<(usize, i64)> = raw.iter().copied().enumerate().collect();
        worst.sort_by_key(|&(_, v)| std::cmp::Reverse(v));
        eprintln!(
            "chaos worst ticks (index, ms): {:?}",
            worst
                .iter()
                .take(5)
                .map(|&(i, v)| (i, v / 1_000_000))
                .collect::<Vec<_>>()
        );
        let mut l = raw;
        l.sort_unstable();
        let pick = |q: f64| l[((l.len() as f64 - 1.0) * q) as usize];
        ClockRun {
            ticks: l.len() as u64,
            expected: (elapsed.as_nanos() as i64 / period_ns) as u64,
            repeats: repeats.load(Ordering::Relaxed),
            p50_ns: pick(0.50),
            p99_ns: pick(0.99),
            max_ns: *l.last().unwrap(),
            late_over_2ms: l.iter().filter(|&&x| x > 2_000_000).count(),
        }
    })
}

/// The flood: `threads` sender threads, each cycling through its own set of
/// loopback sources (127.0.0.2-201) and six packet shapes, paced to `pps`
/// datagrams per second in total.
struct Flood {
    stop: Arc<AtomicBool>,
    sent: Arc<AtomicU64>,
    threads: Vec<std::thread::JoinHandle<()>>,
}

impl Flood {
    fn start(target_port: u16, pps: u64) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let sent = Arc::new(AtomicU64::new(0));
        let threads = (0..4u8)
            .map(|t| {
                let stop = Arc::clone(&stop);
                let sent = Arc::clone(&sent);
                std::thread::spawn(move || {
                    let sockets: Vec<UdpSocket> = (0..50u8)
                        .filter_map(|i| {
                            UdpSocket::bind((format!("127.0.0.{}", 2 + t * 50 + i), 0)).ok()
                        })
                        .collect();
                    assert!(!sockets.is_empty(), "loopback sources");
                    let target: SocketAddr = format!("127.0.0.1:{target_port}").parse().unwrap();
                    let shapes = shapes();
                    let per_thread = pps / 4;
                    let batch = 64u64;
                    let interval = Duration::from_nanos(1_000_000_000 * batch / per_thread.max(1));
                    let mut next = Instant::now();
                    let mut n = 0usize;
                    while !stop.load(Ordering::Relaxed) {
                        for _ in 0..batch {
                            let s = &sockets[n % sockets.len()];
                            let shape = &shapes[n % shapes.len()];
                            let _ = s.send_to(shape, target);
                            n = n.wrapping_add(1);
                        }
                        sent.fetch_add(batch, Ordering::Relaxed);
                        next += interval;
                        let now = Instant::now();
                        if next > now {
                            std::thread::sleep(next - now);
                        } else if now - next > Duration::from_millis(100) {
                            next = now;
                        }
                    }
                })
            })
            .collect();
        Self {
            stop,
            sent,
            threads,
        }
    }

    fn stop(self) -> u64 {
        self.stop.store(true, Ordering::Relaxed);
        for t in self.threads {
            let _ = t.join();
        }
        self.sent.load(Ordering::Relaxed)
    }
}

fn shapes() -> Vec<Vec<u8>> {
    let mut seed = 0x9e37_79b9_u32;
    let mut rand = |len: usize| -> Vec<u8> {
        (0..len)
            .map(|_| {
                seed ^= seed << 13;
                seed ^= seed >> 17;
                seed ^= seed << 5;
                seed.to_le_bytes()[0]
            })
            .collect()
    };
    // Garbage: reserved first byte (RFC 9443 drop range) and a random tail.
    let mut garbage = rand(300);
    garbage[0] = 0x0a;
    // Supported-version Initial shape (random keys: noq must allocate nothing).
    let mut initial = rand(1200);
    initial[0] = 0xc3;
    initial[1..5].copy_from_slice(&1u32.to_be_bytes());
    initial[5] = 8;
    initial[14] = 8;
    // Unsupported-version Initial (Version Negotiation trigger).
    let mut vn = rand(1200);
    vn[0] = 0xc0;
    vn[1..5].copy_from_slice(&0x5a5a_5a5au32.to_be_bytes());
    vn[5] = 8;
    vn[14] = 8;
    // Well-formed STUN Binding Request with a bogus USERNAME.
    let mut stun = vec![0u8; 20];
    stun[1] = 0x01;
    stun[4..8].copy_from_slice(&[0x21, 0x12, 0xA4, 0x42]);
    stun.extend_from_slice(&rand(12)[..]);
    stun.extend_from_slice(&[0x00, 0x06, 0x00, 0x08]);
    stun.extend_from_slice(b"abcd:efg");
    let body = u16::try_from(stun.len() - 20).unwrap();
    stun[2..4].copy_from_slice(&body.to_be_bytes());
    // RTP.
    let mut rtp = rand(1100);
    rtp[0] = 0x80;
    rtp[1] = 96;
    // Short header with a random CID (stateless-reset trigger).
    let mut short = rand(60);
    short[0] = 0x45;
    vec![garbage, initial, vn, stun, rtp, short]
}

/// A MoQ server on the door's QUIC lane that publishes one group every 20 ms,
/// and a subscriber (on its own client runtime, from [::1], a different source
/// key than the flood) that counts what it receives.
struct Viewer {
    published: Arc<AtomicU64>,
    received: Arc<AtomicU64>,
    _client_rt: tokio::runtime::Runtime,
}

fn start_viewer(door: &mut FrontDoor) -> Viewer {
    let lane = door.take_quic_lane().unwrap();
    let established = door.quic_established();
    let port = door.local_addr().port();
    let published = Arc::new(AtomicU64::new(0));
    let pub2 = Arc::clone(&published);
    door.handle().spawn(async move {
        let origin = moq_tokio::origin::spawn();
        let broadcast = origin.create_broadcast("live").unwrap();
        broadcast.announce(Default::default()).unwrap();
        let track = broadcast.create_track("video", None).unwrap();
        let mut listen = moq_tokio::listen::Config::default();
        listen.tls.generate = vec!["localhost".into()];
        let mut server = moq_tokio::server::Config::default();
        server.listen = listen;
        server.socket = Some(moq_tokio::noq::Injected {
            socket: Box::new(lane),
            endpoint: multiview_frontdoor::quic::endpoint_config(),
        });
        let mut listener = server.init().unwrap().listen().await.unwrap();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_millis(20));
            loop {
                tick.tick().await;
                let Ok(mut group) = track.append_group() else {
                    break;
                };
                let _ = group.write_frame(moq_net::Timestamp::ZERO, b"frame".as_ref());
                let _ = group.finish();
                pub2.fetch_add(1, Ordering::Relaxed);
            }
        });
        while let Some(request) = listener.accept().await {
            let remote = request.remote_addr();
            let Ok(session) = request.with_publisher(&origin).ok().await else {
                continue;
            };
            if let Some(remote) = remote {
                established.insert(remote);
            }
            tokio::spawn(async move {
                let _ = session.closed().await;
            });
        }
        drop(broadcast);
    });

    let received = Arc::new(AtomicU64::new(0));
    let recv2 = Arc::clone(&received);
    let client_rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .unwrap();
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    client_rt.spawn(async move {
        let sub_origin = moq_tokio::origin::spawn();
        let consumer = sub_origin.consume();
        let mut announced = consumer.announced();
        let mut cfg = moq_tokio::connect::Config::default();
        cfg.tls.insecure = Some(true);
        cfg.bind = Some("[::1]:0".parse().unwrap());
        let client = cfg
            .init(Default::default())
            .unwrap()
            .with_subscriber(sub_origin);
        let url: url::Url = format!("https://[::1]:{port}").parse().unwrap();
        let _conn = client
            .with_reconnect(false)
            .connect(url)
            .established()
            .await
            .unwrap();
        announced.next().await.unwrap();
        let broadcast = consumer.request_broadcast("live").await.unwrap();
        let mut track = broadcast
            .track("video")
            .unwrap()
            .subscribe(moq_net::track::Subscription::default().with_max_age(Duration::from_secs(5)))
            .await
            .unwrap();
        let _ = ready_tx.send(());
        while let Ok(Some(mut group)) = track.recv_group().await {
            if let Ok(Some(_)) = group.read_frame().await {
                recv2.fetch_add(1, Ordering::Relaxed);
            }
        }
    });
    ready_rx
        .recv_timeout(Duration::from_secs(15))
        .expect("viewer established before the flood");
    Viewer {
        published,
        received,
        _client_rt: client_rt,
    }
}

fn report(name: &str, r: &ClockRun) {
    eprintln!(
        "chaos {name}: ticks {} (expected {}) repeats {} lateness p50 {:.3} ms p99 {:.3} ms max {:.3} ms late>2ms {}",
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

#[test]
fn flood_does_not_slip_the_output_clock_or_starve_an_established_viewer() {
    let duration = Duration::from_secs(secs());
    let pps = pps();
    let period_ns = 1_000_000_000i64 * FPS.1 / FPS.0;

    // 1. Front door on, no flood.
    let mut door = FrontDoor::start(&FrontDoorConfig {
        port: 0,
        workers: 2,
        ..FrontDoorConfig::default()
    })
    .unwrap();
    let port = door.local_addr().port();
    let viewer = start_viewer(&mut door);
    // A WebRTC-lane consumer that drains as fast as it can (the str0m driver's
    // stand-in in the default build; the real driver is exercised in
    // tests/one_socket.rs), so the flood's STUN/RTP shapes really cross the lane.
    let rtc_seen = Arc::new(AtomicU64::new(0));
    {
        let mut lane = door.take_rtc_lane().unwrap();
        let seen = Arc::clone(&rtc_seen);
        door.handle().spawn(async move {
            while lane.recv().await.is_some() {
                seen.fetch_add(1, Ordering::Relaxed);
            }
        });
    }
    let idle = run_clock(duration);
    report("idle (door on, no flood)", &idle);

    // 2. Front door on, flood, established viewer.
    let rss_before = rss_kb();
    let published_before = viewer.published.load(Ordering::Relaxed);
    let received_before = viewer.received.load(Ordering::Relaxed);
    let flood = Flood::start(port, pps);
    // Both flood runs start the flood 1 s before the clock: the comparison is
    // the steady state under flood, not thread start-up racing the first sends.
    std::thread::sleep(Duration::from_secs(1));
    let on = run_clock(duration);
    let sent_on = flood.stop();
    std::thread::sleep(Duration::from_millis(500));
    let published = viewer.published.load(Ordering::Relaxed) - published_before;
    let received = viewer.received.load(Ordering::Relaxed) - received_before;
    let rss_after = rss_kb();
    let stats = door.stats().snapshot();
    report("flood, door on", &on);
    eprintln!(
        "chaos flood door on: sent {sent_on} ({} pps), viewer {received}/{published} groups, \
         rss {rss_before} -> {rss_after} kB, rtc lane drained {}, door {stats:?}",
        sent_on / duration.as_secs().max(1),
        rtc_seen.load(Ordering::Relaxed)
    );
    drop(viewer);
    drop(door);

    // 3. No front door, same flood at the same (now closed) port.
    let flood = Flood::start(port, pps);
    std::thread::sleep(Duration::from_secs(1));
    let off = run_clock(duration);
    let sent_off = flood.stop();
    report("flood, door off", &off);
    eprintln!("chaos flood door off: sent {sent_off}");

    for (name, r) in [("idle", &idle), ("door on", &on), ("door off", &off)] {
        assert_cadence(name, r);
    }
    assert!(
        on.p99_ns <= off.p99_ns.max(idle.p99_ns) + P99_BOUND_NS,
        "p99 lateness with the door on ({:.3} ms) exceeds door off ({:.3} ms) by more than {} ms",
        on.p99_ns as f64 / 1e6,
        off.p99_ns as f64 / 1e6,
        P99_BOUND_NS / 1_000_000
    );
    assert!(
        on.max_ns <= off.max_ns.max(idle.max_ns) + period_ns,
        "worst tick with the door on is more than a period worse: on {on:?} off {off:?}"
    );
    assert!(
        received * 100 >= published * 95,
        "established viewer got {received}/{published} groups under flood"
    );
    assert!(
        rss_after.saturating_sub(rss_before) < RSS_BOUND_KB,
        "RSS grew {rss_before} -> {rss_after} kB under flood"
    );
    assert!(stats.tracked_sources <= 16_384);
    assert!(
        stats.to_rtc > 256,
        "the WebRTC lane carried flood traffic: {stats:?}"
    );
    assert!(stats.received > 0 && stats.dropped_classify + stats.dropped_per_source > 0);
}
