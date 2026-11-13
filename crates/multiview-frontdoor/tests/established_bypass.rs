//! Issue #325 acceptance: the established-flow bypass is keyed on a source
//! address a spoofer can forge, so
//! * a supported-version Initial from an established address still pays the
//!   handshake (Initial) and global budgets, while the established viewer's
//!   short headers are still delivered;
//! * a QUIC Version Negotiation shape in the RTP range (first byte 128-191,
//!   zero version) from a non-established source never reaches the WebRTC
//!   consumer, while ordinary RTP (0x80) still does, and an established
//!   peer's datagram of the same shape is not touched.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::print_stderr,
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::as_conversions,
    clippy::type_complexity
)]

use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use multiview_frontdoor::flood::{datagram, Rng, Shape};
use multiview_frontdoor::{FrontDoor, FrontDoorConfig, NewFlowLimits, StatsSnapshot};

fn door() -> FrontDoor {
    FrontDoor::start(&FrontDoorConfig {
        port: 0,
        workers: 2,
        ..FrontDoorConfig::default()
    })
    .unwrap()
}

fn wait_for(door: &FrontDoor, what: &str, ok: impl Fn(&StatsSnapshot) -> bool) -> StatsSnapshot {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let s = door.stats().snapshot();
        if ok(&s) {
            return s;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {what}: {s:?}"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn initials_from_an_established_address_pay_the_handshake_budget() {
    let mut door = door();
    let port = door.local_addr().port();
    // A QUIC consumer that counts short headers (first byte 64-127) from the
    // viewer's address.
    let mut lane = door.take_quic_lane().unwrap();
    let viewer = UdpSocket::bind("[::1]:0").unwrap();
    let viewer_addr = viewer.local_addr().unwrap();
    let shorts = Arc::new(AtomicU64::new(0));
    {
        let shorts = Arc::clone(&shorts);
        door.handle().spawn(async move {
            use noq::AsyncUdpSocket;
            let mut storage = vec![vec![0u8; 1500]; 32];
            let mut metas = vec![noq::udp::RecvMeta::default(); 32];
            loop {
                let n = std::future::poll_fn(|cx| {
                    let mut bufs: Vec<std::io::IoSliceMut<'_>> = storage
                        .iter_mut()
                        .map(|b| std::io::IoSliceMut::new(b))
                        .collect();
                    lane.poll_recv(cx, &mut bufs, &mut metas)
                })
                .await;
                let Ok(n) = n else { break };
                for (m, b) in metas.iter().zip(&storage).take(n) {
                    if (64..128).contains(&b[0])
                        && multiview_frontdoor::classify::canonical(m.addr) == viewer_addr
                    {
                        shorts.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
        });
    }
    assert!(door.quic_established().insert(viewer_addr));
    // Let the steering program include the viewer (if steering is active).
    std::thread::sleep(Duration::from_millis(100));

    let budget_pps = u64::from(NewFlowLimits::default().initial_pps);
    let rate = 10 * budget_pps; // 2 000 Initials/s
    let mut rng = Rng::new(325);
    let before = door.stats().snapshot();
    let start = Instant::now();
    let (mut initials, mut short_sent) = (0u64, 0u64);
    for tick in 1..=100u64 {
        // 10 ms ticks for 1 s: 20 Initials and 2 short headers per tick.
        for _ in 0..rate / 100 {
            viewer
                .send_to(&datagram(Shape::Initial, &mut rng), ("::1", port))
                .unwrap();
            initials += 1;
        }
        for _ in 0..2 {
            viewer
                .send_to(&datagram(Shape::Short, &mut rng), ("::1", port))
                .unwrap();
            short_sent += 1;
        }
        if let Some(wait) =
            (start + Duration::from_millis(10 * tick)).checked_duration_since(Instant::now())
        {
            std::thread::sleep(wait);
        }
    }
    let elapsed = start.elapsed();
    let s = wait_for(&door, "every datagram read", |s| {
        s.received >= before.received + initials + short_sent
    });
    std::thread::sleep(Duration::from_millis(200));
    let dropped = s.dropped_initial - before.dropped_initial;
    // The budget over the run: burst (initial_pps / 10) + rate x elapsed.
    let budget = budget_pps / 10 + (budget_pps as f64 * elapsed.as_secs_f64()).ceil() as u64;
    eprintln!(
        "#325 initials: sent {initials} in {:.3} s, dropped_initial {dropped}, budget {budget}, \
         shorts delivered {}/{short_sent}, dropped_established_peer {}",
        elapsed.as_secs_f64(),
        shorts.load(Ordering::Relaxed),
        s.dropped_established_peer
    );
    assert!(
        dropped >= initials - budget,
        "Initials from an established address are held to the handshake budget: \
         dropped {dropped} of {initials}, budget {budget}"
    );
    assert!(dropped < initials, "the budget admits some: {dropped}");
    assert_eq!(
        shorts.load(Ordering::Relaxed),
        short_sent,
        "the established viewer's short headers are all delivered: {s:?}"
    );
    assert_eq!(s.dropped_per_source, before.dropped_per_source, "{s:?}");
}

#[test]
fn vn_shaped_rtp_from_an_unvalidated_source_never_reaches_the_rtc_consumer() {
    let mut door = door();
    let port = door.local_addr().port();
    let mut lane = door.take_rtc_lane().unwrap();
    let seen: Arc<Mutex<Vec<(SocketAddr, Vec<u8>)>>> = Arc::default();
    {
        let seen = Arc::clone(&seen);
        door.handle().spawn(async move {
            while let Some(d) = lane.recv().await {
                seen.lock()
                    .unwrap()
                    .push((multiview_frontdoor::classify::canonical(d.src), d.payload));
            }
        });
    }
    let stranger = UdpSocket::bind("[::1]:0").unwrap();
    let peer = UdpSocket::bind("[::1]:0").unwrap();
    let peer_addr = peer.local_addr().unwrap();
    assert!(door.rtc_established().insert(peer_addr));
    std::thread::sleep(Duration::from_millis(100));
    let vn_shape = |first: u8| {
        let mut d = vec![0u8; 60];
        d[0] = first;
        d[5] = 8;
        d
    };
    let mut rtp = vec![0x5au8; 172];
    rtp[0] = 0x80;
    rtp[1] = 96;
    for first in 128..=143u8 {
        stranger.send_to(&vn_shape(first), ("::1", port)).unwrap();
    }
    stranger.send_to(&rtp, ("::1", port)).unwrap();
    peer.send_to(&vn_shape(0x80), ("::1", port)).unwrap();
    peer.send_to(&rtp, ("::1", port)).unwrap();
    wait_for(&door, "19 datagrams", |s| s.received >= 19);
    std::thread::sleep(Duration::from_millis(200));
    let seen = seen.lock().unwrap().clone();
    let stranger_addr = stranger.local_addr().unwrap();
    let from_stranger: Vec<&Vec<u8>> = seen
        .iter()
        .filter(|(a, _)| *a == stranger_addr)
        .map(|(_, p)| p)
        .collect();
    assert_eq!(
        from_stranger,
        vec![&rtp],
        "only the ordinary RTP from the stranger"
    );
    let from_peer = seen.iter().filter(|(a, _)| *a == peer_addr).count();
    assert_eq!(
        from_peer, 2,
        "an established peer's datagrams are never touched"
    );
}
