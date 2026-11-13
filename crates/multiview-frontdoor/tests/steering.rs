//! Validated-flow steering on the real kernel (Linux): exact delivery of a
//! member's datagrams to the established socket, queue isolation under a
//! flood that overflows the listener's kernel queue, removal, and the
//! single-socket fallback.
#![cfg(target_os = "linux")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::print_stderr,
    clippy::too_many_lines
)]

use std::net::{Ipv6Addr, SocketAddr, UdpSocket};
use std::time::{Duration, Instant};

use multiview_frontdoor::flood::{freebind_v6, rotating_source};
use multiview_frontdoor::{
    kernel_drops, FrontDoor, FrontDoorConfig, ProgramSet, ReuseportGroup, StatsSnapshot,
    SteeringMode,
};

const WAIT: Duration = Duration::from_secs(10);

fn door(steering: bool) -> FrontDoor {
    FrontDoor::start(&FrontDoorConfig {
        port: 0,
        workers: 2,
        steering,
        ..FrontDoorConfig::default()
    })
    .unwrap()
}

fn wait_for(door: &FrontDoor, what: &str, ok: impl Fn(&StatsSnapshot) -> bool) -> StatsSnapshot {
    let deadline = Instant::now() + WAIT;
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

/// A 60-byte QUIC short header (any first-byte-valid payload works: the
/// counters under test are per socket, before classification).
fn short_header(tag: u8) -> Vec<u8> {
    let mut d = vec![tag; 60];
    d[0] = 0x45;
    d
}

/// Send `n` datagrams paced in chunks of 50, so the dispatcher (which drains
/// concurrently) never lets a kernel queue overflow on a loaded host.
fn send_paced(sock: &UdpSocket, to: SocketAddr, n: usize, tag: u8) {
    for chunk in 0..n.div_ceil(50) {
        for _ in 0..50.min(n - chunk * 50) {
            sock.send_to(&short_header(tag), to).unwrap();
        }
        std::thread::sleep(Duration::from_millis(2));
    }
}

#[test]
fn steering_is_active_and_falls_back_on_request() {
    let on = door(true);
    assert_eq!(on.steering(), SteeringMode::ReuseportCbpf);
    assert!(on.kernel_drops_supported());
    let off = door(false);
    assert!(
        matches!(off.steering(), SteeringMode::SingleSocket { ref reason } if reason.contains("configuration")),
        "{:?}",
        off.steering()
    );
    // The single socket still receives (and never reads an established socket).
    let port = off.local_addr().port();
    let client = UdpSocket::bind("[::1]:0").unwrap();
    send_paced(
        &client,
        SocketAddr::from((Ipv6Addr::LOCALHOST, port)),
        10,
        1,
    );
    let s = wait_for(&off, "10 datagrams", |s| s.received >= 10);
    assert_eq!(s.received_listener_socket, 10, "{s:?}");
    assert_eq!(s.received_established_socket, 0, "{s:?}");
    assert_eq!(s.steering_updates, 0, "{s:?}");
}

/// A member's datagrams (v6 and v4) are read from the established socket, all
/// of them; non-members' from the listener, all of them. After removal, the
/// former member lands on the listener.
#[test]
fn members_reach_the_established_socket_and_non_members_never_do() {
    let door = door(true);
    assert_eq!(door.steering(), SteeringMode::ReuseportCbpf);
    let port = door.local_addr().port();
    let to6 = SocketAddr::from((Ipv6Addr::LOCALHOST, port));
    let to4: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
    let member6 = UdpSocket::bind("[::1]:0").unwrap();
    let member4 = UdpSocket::bind("127.0.0.1:0").unwrap();
    let before = door.stats().snapshot();
    let established = door.quic_established();
    assert!(established.insert(member6.local_addr().unwrap()));
    assert!(established.insert(member4.local_addr().unwrap()));
    let steered = wait_for(&door, "the program with both members", |s| {
        s.steering_updates > before.steering_updates && s.steered_peers == 2
    });

    // 50 non-members: 47 on [::1], 2 on 127/8, 1 on a freebind /64 when the
    // platform allows it (else one more on [::1]).
    let mut others: Vec<(UdpSocket, SocketAddr)> = (0..47)
        .map(|_| (UdpSocket::bind("[::1]:0").unwrap(), to6))
        .collect();
    others.push((UdpSocket::bind("127.0.0.1:0").unwrap(), to4));
    others.push((UdpSocket::bind("127.0.0.2:0").unwrap(), to4));
    match freebind_v6(rotating_source("2001:db8::".parse().unwrap(), 7, 9)) {
        Ok(s) => others.push((s, to6)),
        Err(e) => {
            eprintln!("steering: freebind unsupported ({e}); a [::1] non-member instead");
            others.push((UdpSocket::bind("[::1]:0").unwrap(), to6));
        }
    }
    send_paced(&member6, to6, 500, 2);
    send_paced(&member4, to4, 500, 2);
    for (s, to) in &others {
        send_paced(s, *to, 20, 3);
    }
    let s = wait_for(&door, "2000 datagrams", |s| {
        s.received >= steered.received + 2000
    });
    let est = s.received_established_socket - steered.received_established_socket;
    let lst = s.received_listener_socket - steered.received_listener_socket;
    eprintln!(
        "steering exact delivery: established socket {est}, listener {lst}, \
         unverified {}, kernel drops {}/{}",
        s.steered_unverified, s.kernel_drops_listener, s.kernel_drops_established
    );
    assert_eq!(
        est, 1000,
        "every member datagram via the established socket: {s:?}"
    );
    assert_eq!(
        lst, 1000,
        "every non-member datagram via the listener: {s:?}"
    );
    assert_eq!(s.steered_unverified, 0, "{s:?}");
    assert_eq!(s.dropped_established_peer, 0, "{s:?}");

    // Removal: after the next program, the former members use the listener.
    established.remove(member6.local_addr().unwrap());
    established.remove(member4.local_addr().unwrap());
    let removed = wait_for(&door, "the program without members", |x| {
        x.steering_updates > s.steering_updates && x.steered_peers == 0
    });
    send_paced(&member6, to6, 100, 4);
    send_paced(&member4, to4, 100, 4);
    let after = wait_for(&door, "200 more", |x| x.received >= removed.received + 200);
    assert_eq!(
        after.received_established_socket, removed.received_established_socket,
        "{after:?}"
    );
    assert_eq!(
        after.received_listener_socket - removed.received_listener_socket,
        200,
        "{after:?}"
    );
}

/// Flood the listener with no reader until the kernel drops (synchronously:
/// a loopback `send_to` either queues the datagram or counts a drop before it
/// returns), so the next datagram for the listener queue is lost for sure.
fn overflow(listener: &UdpSocket, flooder: &UdpSocket, to: SocketAddr) -> u64 {
    let start = kernel_drops(listener).unwrap();
    for _ in 0..10_000 {
        for _ in 0..16 {
            let _ = flooder.send_to(&short_header(9), to);
        }
        let now = kernel_drops(listener).unwrap();
        if now > start {
            return now;
        }
    }
    panic!("the listener queue never overflowed");
}

/// Count the member's datagrams in `sock`'s queue (draining it).
fn drain_count(sock: &UdpSocket, member: SocketAddr) -> usize {
    sock.set_nonblocking(true).unwrap();
    let mut buf = [0u8; 2048];
    let mut n = 0;
    while let Ok((_, src)) = sock.recv_from(&mut buf) {
        n += usize::from(src == member);
    }
    n
}

/// Run the isolation scenario on a 64 KiB group: `set` decides steering.
/// Returns (member datagrams delivered, listener drops, established drops).
fn isolation_run(steer: bool) -> (usize, u64, u64) {
    let group = ReuseportGroup::bind("[::]:0".parse().unwrap(), 64 * 1024).unwrap();
    let port = group.listener().local_addr().unwrap().port();
    let to = SocketAddr::from((Ipv6Addr::LOCALHOST, port));
    let member = UdpSocket::bind("[::1]:0").unwrap();
    let member_addr = member.local_addr().unwrap();
    let flooder = UdpSocket::bind("[::1]:0").unwrap();
    let set = if steer {
        ProgramSet::build(0x5eed, [member_addr])
    } else {
        // Same group, a program that steers nobody: everything on the
        // listener, i.e. the one-queue behaviour.
        ProgramSet::empty()
    };
    group.attach(&set).unwrap();
    let mut delivered = 0usize;
    // 5 rounds of 100: before each, the listener queue is overflowing.
    for _ in 0..5 {
        overflow(group.listener(), &flooder, to);
        for _ in 0..100 {
            member.send_to(&short_header(2), to).unwrap();
        }
        delivered += drain_count(group.established(), member_addr);
    }
    // Whatever of the member's survived in the listener queue counts too.
    delivered += drain_count(group.listener(), member_addr);
    (
        delivered,
        kernel_drops(group.listener()).unwrap(),
        kernel_drops(group.established()).unwrap(),
    )
}

#[test]
fn a_flooded_listener_queue_never_costs_a_steered_member_a_datagram() {
    let (with, l_drops, e_drops) = isolation_run(true);
    let (without, l_drops_off, e_drops_off) = isolation_run(false);
    eprintln!(
        "steering isolation: with steering {with}/500 delivered (listener drops {l_drops}, \
         established drops {e_drops}); without {without}/500 (listener drops {l_drops_off}, \
         established drops {e_drops_off})"
    );
    assert!(l_drops > 0, "the flood overflowed the listener");
    assert_eq!(with, 500, "every member datagram arrived");
    assert_eq!(e_drops, 0, "the established queue dropped nothing");
    assert!(
        without < 500,
        "without steering the flood costs the member datagrams: {without}"
    );
}
