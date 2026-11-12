//! Chaos (invariants #1 and #10, issues #287, #289, #321): flood the
//! front-door socket while the REAL engine tick loop (`EngineRuntime` +
//! `RealtimePacer` + `MonotonicTimeSource`, the production pacing path) runs on
//! a separate "program" runtime, exactly the topology slice 1 creates.
//!
//! Arms, same host:
//! * `idle`: front door up (steering as the platform allows), no flood;
//! * `flood door on`, once per flood kind, with an established MoQ viewer:
//!   (i) one source, (ii) many IPv4 loopback sources (127.0.0.2.., distinct
//!   source keys), (iii) rotating IPv6 /64s (Linux freebind; skipped with a
//!   printed note where unsupported). Every kind sends every shape of
//!   `multiview_frontdoor::flood` (garbage, supported-version Initial,
//!   unsupported-version Initial, short header with a random CID, STUN with a
//!   bogus USERNAME, RTP);
//! * `flood door off`: no front door at all, the many-source flood at the
//!   same port.
//!
//! Asserted per arm (absolute, never relative to another arm, issue #321):
//! * one frame per tick: `elapsed / period` ticks within 2;
//! * output PTS: every tick's PTS is exactly `MediaTime::from_tick(index)` and
//!   indices are consecutive, so the PTS delta is exactly one period;
//! * 0 unexplained repeats or slips: a repeat (or a tick later than a period)
//!   is explained only when the stall witness saw the whole process stop over
//!   its `[deadline, emit]` interval (`tests/common/mod.rs`); the former
//!   p99/max comparisons between arms are gone (percentile comparisons, the
//!   class of assertion #321 retired);
//! * door on: the established viewer receives at least 95% of the groups
//!   published while the flood runs; RSS growth stays under [`RSS_BOUND_KB`];
//!   with steering active (Linux) the viewer's datagrams arrive through the
//!   established socket (`received_established_socket` grows and nothing read
//!   there is unverified: the viewer is the only steered peer).
//!
//! Duration per arm: `FRONTDOOR_CHAOS_SECS` (default 3 s). Flood rate:
//! `FRONTDOOR_CHAOS_PPS` (default 20 000 datagrams/s, sized for a 2-core CI
//! runner).
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

mod common;

use std::net::{Ipv6Addr, SocketAddr};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use moq_tokio::moq_net;
use multiview_frontdoor::flood::{self, Flood, FloodConfig, Shape, Sources};
use multiview_frontdoor::{FrontDoor, FrontDoorConfig, SteeringMode};

use common::{assert_cadence, report, run_clock, ClockRun};

/// RSS growth bound during the floods.
const RSS_BOUND_KB: u64 = 64 * 1024;

fn secs() -> u64 {
    std::env::var("FRONTDOOR_CHAOS_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(3)
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

fn start_flood(port: u16, pps: u64, sources: Sources) -> std::io::Result<Flood> {
    Flood::start(FloodConfig {
        target: SocketAddr::from((Ipv6Addr::LOCALHOST, port)),
        pps,
        threads: 4,
        sources,
        shapes: Shape::ALL.to_vec(),
    })
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
            incoming: Some(multiview_frontdoor::quic::incoming_caps()),
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

struct ArmResult {
    name: String,
    run: ClockRun,
    received: u64,
    published: u64,
}

#[test]
fn flood_does_not_slip_the_output_clock_or_starve_an_established_viewer() {
    let duration = Duration::from_secs(secs());
    let pps = pps();

    // 1. Front door on, no flood.
    let mut door = FrontDoor::start(&FrontDoorConfig {
        port: 0,
        workers: 2,
        ..FrontDoorConfig::default()
    })
    .unwrap();
    let steering = door.steering() == SteeringMode::ReuseportCbpf;
    eprintln!("chaos: steering {:?}", door.steering());
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
    let idle = run_clock(duration, |_| {});
    report("idle (door on, no flood)", &idle);

    // 2. Front door on, one arm per flood kind, established viewer.
    let rss_before = rss_kb();
    let mut kinds: Vec<(&str, Sources)> = vec![
        ("one source", Sources::One("[::1]:0".parse().unwrap())),
        ("many v4 sources", Sources::ManyV4Loopback),
    ];
    if flood::rotating_v6_supported() {
        kinds.push((
            "rotating v6 /64s",
            Sources::RotatingV6 {
                prefix: "2001:db8::".parse().unwrap(),
            },
        ));
    } else {
        eprintln!("chaos: rotating IPv6 /64 flood skipped (IPV6_FREEBIND unsupported here)");
    }
    let mut arms = Vec::new();
    let mut stats = door.stats().snapshot();
    for (kind, sources) in kinds {
        let published_before = viewer.published.load(Ordering::Relaxed);
        let received_before = viewer.received.load(Ordering::Relaxed);
        let before = door.stats().snapshot();
        let flood = start_flood(port, pps, sources).unwrap();
        // The flood starts 1 s before the clock: steady state, not thread
        // start-up racing the first sends.
        std::thread::sleep(Duration::from_secs(1));
        let run = run_clock(duration, |_| {});
        let sent = flood.stop();
        std::thread::sleep(Duration::from_millis(500));
        let published = viewer.published.load(Ordering::Relaxed) - published_before;
        let received = viewer.received.load(Ordering::Relaxed) - received_before;
        stats = door.stats().snapshot();
        let name = format!("flood door on, {kind}");
        report(&name, &run);
        eprintln!(
            "chaos {name}: sent {sent} ({} pps), viewer {received}/{published} groups, \
             established socket +{} listener socket +{} unverified +{} kernel drops \
             listener {} established {}, rtc lane drained {}, door {stats:?}",
            sent / (duration.as_secs() + 1).max(1),
            stats.received_established_socket - before.received_established_socket,
            stats.received_listener_socket - before.received_listener_socket,
            stats.steered_unverified - before.steered_unverified,
            stats.kernel_drops_listener,
            stats.kernel_drops_established,
            rtc_seen.load(Ordering::Relaxed)
        );
        if steering {
            assert!(
                stats.received_established_socket > before.received_established_socket,
                "{name}: the viewer's datagrams came through the established socket: {stats:?}"
            );
            assert_eq!(
                stats.steered_unverified, before.steered_unverified,
                "{name}: nothing but the viewer was read from the established socket"
            );
        }
        arms.push(ArmResult {
            name,
            run,
            received,
            published,
        });
    }
    let rss_after = rss_kb();
    drop(viewer);
    door.shutdown_blocking();
    // The door-off baseline really has no front door: the port is free again.
    let probe = std::net::UdpSocket::bind(("::", port));
    assert!(probe.is_ok(), "front door released UDP {port}: {probe:?}");
    drop(probe);

    // 3. No front door, the many-source flood at the same (now closed) port.
    let flood = start_flood(port, pps, Sources::ManyV4Loopback).unwrap();
    std::thread::sleep(Duration::from_secs(1));
    let off = run_clock(duration, |_| {});
    let sent_off = flood.stop();
    report("flood, door off", &off);
    eprintln!("chaos flood door off: sent {sent_off}");

    assert_cadence("idle", &idle);
    assert_cadence("door off", &off);
    for arm in &arms {
        assert_cadence(&arm.name, &arm.run);
        assert!(
            arm.received * 100 >= arm.published * 95,
            "{}: established viewer got {}/{} groups under flood",
            arm.name,
            arm.received,
            arm.published
        );
    }
    assert!(
        rss_after.saturating_sub(rss_before) < RSS_BOUND_KB,
        "RSS grew {rss_before} -> {rss_after} kB under flood"
    );
    assert!(
        stats.to_rtc > 256,
        "the WebRTC lane carried flood traffic: {stats:?}"
    );
    assert!(stats.received > 0 && stats.dropped_classify + stats.dropped_per_source > 0);
}
