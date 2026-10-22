//! SAP **socket transport** loopback validation (ADR-0041 §3/§5, `st2110`
//! feature).
//!
//! The listener binds an IPv6-first UDP socket, receives datagrams, parses each,
//! and folds announcements into the wait-free session table off the output-clock
//! data plane (inv #1/#10); the announcer builds + sends RFC 2974 packets. These
//! tests exercise the real sockets over **unicast loopback** (the multicast group
//! join is the live-network path, not reachable in this devcontainer). A
//! malformed datagram must be skipped without killing the receive loop, and a
//! spoofed inbound deletion must never withdraw a tracked session.
#![cfg(feature = "st2110")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]

use std::net::{IpAddr, Ipv6Addr, SocketAddr};
use std::time::Duration;

use multiview_input::sap::announce::{announcement, deletion, stable_hash, AnnounceSchedule};
use multiview_input::sap::transport::{AnnouncedSession, SapAnnouncer, SapListener};

fn loopback6(port: u16) -> SocketAddr {
    SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), port)
}

/// Poll up to ~2 s for an async effect to land, so the tests never hang.
async fn wait_for(pred: impl Fn() -> bool) -> bool {
    for _ in 0..200 {
        if pred() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    pred()
}

#[tokio::test]
async fn loopback_announce_is_received_and_folded_into_the_table() {
    let listener = SapListener::bind(loopback6(0)).await.unwrap();
    let addr = listener.local_addr().unwrap();
    let table = listener.table();
    tokio::spawn(listener.run());

    let announcer = SapAnnouncer::bind(loopback6(0)).await.unwrap();
    let sdp = b"v=0\r\no=- 1 1 IN IP6 ff3e::1\r\ns=multiview\r\n".to_vec();
    let hash = stable_hash(&sdp);
    let pkt = announcement(hash, IpAddr::V6(Ipv6Addr::LOCALHOST), sdp.clone());
    announcer.send_to(&pkt, addr).await.unwrap();

    assert!(
        wait_for(|| table.len() == 1).await,
        "the announcement is received, parsed and discovered"
    );
    let inv = table.inventory();
    assert_eq!(inv[0].sdp, sdp);
    assert_eq!(inv[0].key.msg_id_hash, hash);
}

#[tokio::test]
async fn malformed_datagram_is_skipped_and_the_loop_survives() {
    let listener = SapListener::bind(loopback6(0)).await.unwrap();
    let addr = listener.local_addr().unwrap();
    let table = listener.table();
    tokio::spawn(listener.run());

    // Raw socket to inject garbage the SapAnnouncer would never build.
    let raw = tokio::net::UdpSocket::bind(loopback6(0)).await.unwrap();
    raw.send_to(&[0xFF, 0x00, 0x00, 0x01], addr).await.unwrap(); // bad version, too short
    raw.send_to(&[], addr).await.unwrap(); // empty datagram

    // A valid announcement after the garbage must still be discovered → the loop
    // survived the malformed inputs.
    let announcer = SapAnnouncer::bind(loopback6(0)).await.unwrap();
    let sdp = b"v=0 valid-after-garbage".to_vec();
    let pkt = announcement(
        stable_hash(&sdp),
        IpAddr::V6(Ipv6Addr::LOCALHOST),
        sdp.clone(),
    );
    announcer.send_to(&pkt, addr).await.unwrap();

    assert!(
        wait_for(|| table.len() == 1).await,
        "the valid announcement lands after garbage — the receive loop never died"
    );
}

#[tokio::test]
async fn inbound_deletion_never_withdraws_a_tracked_session_over_the_wire() {
    let listener = SapListener::bind(loopback6(0)).await.unwrap();
    let addr = listener.local_addr().unwrap();
    let table = listener.table();
    tokio::spawn(listener.run());

    let announcer = SapAnnouncer::bind(loopback6(0)).await.unwrap();
    let sdp = b"v=0 tracked".to_vec();
    let hash = stable_hash(&sdp);
    let origin = IpAddr::V6(Ipv6Addr::LOCALHOST);
    announcer
        .send_to(&announcement(hash, origin, sdp.clone()), addr)
        .await
        .unwrap();
    assert!(wait_for(|| table.len() == 1).await, "session is tracked");

    // A spoofed deletion must be ignored (ADR-0041 §8 hijack guard).
    announcer
        .send_to(&deletion(hash, origin, sdp.clone()), addr)
        .await
        .unwrap();
    // Give the loop time to (not) act on it, then assert it survived.
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(
        table.len(),
        1,
        "the tracked session survives a spoofed inbound deletion"
    );
}

#[tokio::test]
async fn listener_binds_ipv6_unspecified_dual_stack() {
    // ADR-0042 IPv6-first: bind [::] rather than 0.0.0.0.
    let listener = SapListener::bind(SocketAddr::new(IpAddr::V6(Ipv6Addr::UNSPECIFIED), 0))
        .await
        .unwrap();
    assert!(
        listener.local_addr().unwrap().is_ipv6(),
        "the listener binds an IPv6 socket"
    );
}

#[tokio::test]
async fn a_datagram_flood_is_rate_limited_before_the_expensive_fold() {
    use multiview_input::sap::SapSessionTable;
    use std::sync::Arc;

    // A high-capacity table so `len()` reflects the number of *folds* (not the
    // default 256 cap), and a TIGHT rate limit with a window far longer than the
    // test, so exactly `burst` datagrams are folded regardless of send timing —
    // fully deterministic (panel F4).
    let table = Arc::new(SapSessionTable::with_limits(100_000, 100_000));
    let listener = SapListener::bind(loopback6(0))
        .await
        .unwrap()
        .with_table(Arc::clone(&table))
        .with_rate_limit(8, Duration::from_secs(30));
    let addr = listener.local_addr().unwrap();
    tokio::spawn(listener.run());

    // Flood 200 DISTINCT valid announcements (distinct hashes) as fast as we can.
    let announcer = SapAnnouncer::bind(loopback6(0)).await.unwrap();
    let origin = IpAddr::V6(Ipv6Addr::LOCALHOST);
    for i in 0..200u32 {
        let sdp = format!("v=0\r\no=- {i} {i} IN IP6 ff3e::1\r\ns=flood-{i}\r\n").into_bytes();
        let pkt = announcement(stable_hash(&sdp), origin, sdp);
        announcer.send_to(&pkt, addr).await.unwrap();
    }

    // Give the listener time to drain the socket and fold whatever the limiter
    // admits. Only the first `burst` (8) can enter the expensive fold path; the
    // rest are dropped cheaply BEFORE the O(n) RCU clone (inv #10).
    let bounded = wait_for(|| !table.is_empty()).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let folded = table.len();
    assert!(bounded && folded >= 1, "at least the burst is folded");
    assert!(
        folded <= 8,
        "a same-window flood folds at most the rate-limit burst (8), got {folded} of 200 — \
         the expensive fold is gated BEFORE it runs (F4 / inv #10)"
    );
}

#[tokio::test]
async fn announcer_run_loop_emits_the_first_cycle_immediately() {
    let listener = SapListener::bind(loopback6(0)).await.unwrap();
    let addr = listener.local_addr().unwrap();
    let table = listener.table();
    tokio::spawn(listener.run());

    let announcer = SapAnnouncer::bind(loopback6(0)).await.unwrap();
    let sdp = b"v=0 scheduled".to_vec();
    let session = AnnouncedSession {
        hash: stable_hash(&sdp),
        origin: IpAddr::V6(Ipv6Addr::LOCALHOST),
        sdp: sdp.clone(),
        dest: addr,
    };
    // A 30 s cadence means the first announcement is sent immediately, before the
    // first (jittered) sleep — so the table populates without waiting a cycle.
    tokio::spawn(announcer.run(
        vec![session],
        AnnounceSchedule::new(Duration::from_secs(30)),
    ));

    assert!(
        wait_for(|| table.len() == 1).await,
        "the announce run loop emits its first cycle immediately"
    );
}

#[tokio::test(start_paused = true)]
async fn purge_runs_on_its_own_timer_even_when_no_datagrams_arrive() {
    use multiview_input::sap::announce::announcement;
    use multiview_input::sap::SapSessionTable;
    use std::sync::Arc;

    // P2-F3: a shared table pre-seeded with one session at t=0; the listener then
    // runs with NO datagram ever arriving. Before the fix, purge runs only AFTER
    // recv_from returns, so a parked receive (announcements stopped) blocks it
    // forever and the expired session is never reaped.
    let table = Arc::new(SapSessionTable::with_limits(16, 16));
    let sdp = b"v=0\r\no=- 1 1 IN IP6 ff3e::1\r\ns=stale\r\n".to_vec();
    let pkt = announcement(stable_hash(&sdp), IpAddr::V6(Ipv6Addr::LOCALHOST), sdp);
    table.observe(&pkt, Duration::ZERO);
    assert_eq!(table.len(), 1, "seeded one session at t=0");

    let listener = SapListener::bind(loopback6(0))
        .await
        .unwrap()
        .with_table(Arc::clone(&table));
    tokio::spawn(listener.run());

    // Let the listener reach its select! and arm the purge interval at t=0
    // (before the clock jumps), so the fast-forward crosses a real tick deadline.
    for _ in 0..8 {
        tokio::task::yield_now().await;
    }

    // Fast-forward past the 1 h purge floor WITHOUT sending any datagram, in steps
    // so each fired purge tick is processed. The receive loop is parked in
    // recv_from, so only an INDEPENDENT purge timer can reap the expired session
    // (P2-F3).
    let mut purged = false;
    for _ in 0..200 {
        tokio::time::advance(Duration::from_secs(60)).await;
        tokio::task::yield_now().await;
        if table.is_empty() {
            purged = true;
            break;
        }
    }
    assert!(
        purged,
        "the expired session is purged on the independent purge timer with no \
         datagram ever received (P2-F3)"
    );
}

#[tokio::test(start_paused = true)]
async fn missed_purge_ticks_collapse_to_one_reap_not_a_burst() {
    use multiview_input::sap::transport::PURGE_INTERVAL;
    use multiview_input::sap::SapSessionTable;
    use std::sync::Arc;

    // I1 (#154): when the receive branch keeps the loop busy across several purge
    // periods, the purge interval must fire ONCE on return, not burst
    // one redundant O(n) reap scan per missed tick (amplifying the very work F4
    // gated). Run the listener with no datagrams, jump the paused clock past N
    // purge intervals in ONE step, and count actual reap scans via the table.
    let table = Arc::new(SapSessionTable::with_limits(16, 16));
    let listener = SapListener::bind(loopback6(0))
        .await
        .unwrap()
        .with_table(Arc::clone(&table));
    tokio::spawn(listener.run());

    // Let run() reach its select! and consume the immediate first purge tick at
    // t=0, BEFORE the clock jumps.
    for _ in 0..8 {
        tokio::task::yield_now().await;
    }
    assert_eq!(
        table.purge_count(),
        0,
        "no reap scan before the clock advances"
    );

    // Jump past 5 purge intervals in a SINGLE step — a stall that missed 5 ticks.
    tokio::time::advance(PURGE_INTERVAL * 5 + Duration::from_millis(1)).await;
    for _ in 0..8 {
        tokio::task::yield_now().await;
    }

    // Skip: the 5 missed ticks collapse to ONE reap. Burst (the default, the bug)
    // runs 5 redundant O(n) scans back-to-back.
    assert_eq!(
        table.purge_count(),
        1,
        "missed purge ticks must Skip to a single reap, not Burst N redundant O(n) \
         scans (I1)"
    );
}

#[tokio::test]
async fn announcer_threads_the_interface_index_to_v6_multicast_egress() {
    // P2-F9: IPv6 multicast egress is interface-scoped (RFC 4291): a scoped /
    // link-local announce dest needs an explicit egress interface (IPV6_MULTICAST_IF),
    // not the OS default. Proven exactly like the F6 RX join — thread a BOGUS
    // interface index through and observe the OS reject it; if the index were
    // ignored (hardcoded 0), configuring egress would spuriously succeed.
    use multiview_input::st2110::transport::MulticastInterface;

    // Unspecified (OS default, index 0) configures cleanly on a v6 socket.
    let default = SapAnnouncer::bind(loopback6(0))
        .await
        .unwrap()
        .with_interface(MulticastInterface::Unspecified);
    default
        .configure_multicast_egress()
        .expect("the unspecified egress interface configures on the OS default");

    // A bogus interface index reaches the OS and fails the egress config.
    let bogus = SapAnnouncer::bind(loopback6(0))
        .await
        .unwrap()
        .with_interface(MulticastInterface::Index(0xFFFF_FFF0));
    assert!(
        bogus.configure_multicast_egress().is_err(),
        "a bogus egress interface index must reach the OS and fail (the index is \
         plumbed to IPV6_MULTICAST_IF, not hardcoded 0) (P2-F9)"
    );
}

#[tokio::test(start_paused = true)]
async fn announcer_run_applies_the_configured_multicast_egress_interface() {
    use multiview_input::st2110::transport::MulticastInterface;

    // S2 (#156): SapAnnouncer::run must apply the configured IPv6 egress interface
    // during socket setup. F9 wired configure_multicast_egress but NOTHING on the
    // run path called it, so a configured interface was silently ignored and
    // announces fell to the OS default. Proven with a BOGUS index: run() must apply
    // it, fail, and END — without the call run() announces forever on the OS default
    // and the timeout elapses (the RED).
    let announcer = SapAnnouncer::bind(loopback6(0))
        .await
        .unwrap()
        .with_interface(MulticastInterface::Index(0xFFFF_FFF0));
    let sdp = b"v=0 egress".to_vec();
    let session = AnnouncedSession {
        hash: stable_hash(&sdp),
        origin: IpAddr::V6(Ipv6Addr::LOCALHOST),
        sdp,
        dest: loopback6(9999),
    };
    let ran = tokio::time::timeout(
        Duration::from_secs(60),
        announcer.run(
            vec![session],
            AnnounceSchedule::new(Duration::from_secs(30)),
        ),
    )
    .await;
    assert!(
        ran.is_ok(),
        "run() must apply the configured egress interface and END on a bogus index, \
         not announce forever on the OS default (S2)"
    );
}
