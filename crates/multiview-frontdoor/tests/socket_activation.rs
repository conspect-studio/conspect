//! Socket activation (slice 6, issue #292; design note section 9).
//!
//! 1. `real_systemd_socket_activate_*`: this test binary re-runs itself under
//!    the real `systemd-socket-activate` (TCP door port, redirect port, then
//!    the UDP port through a nested activator), so the sockets arrive through `LISTEN_FDS`/`LISTEN_PID` exactly as
//!    `conspect.socket` passes them. The child adopts them with
//!    `ListenSockets::from_env`, reports what it inherited, and the parent
//!    proves each one serves: a TCP connection is accepted and classified, the
//!    port-80 redirect answers 301, a UDP datagram reaches the dispatcher.
//! 2. `a_restart_keeps_the_sockets`: a "service manager" (this test) holds the
//!    listening sockets; door A runs on duplicates, shuts down, and while no
//!    engine runs a client connects (the kernel completes the handshake from
//!    the backlog) and sends a datagram; door B adopts the same sockets and
//!    serves that queued connection and reads that datagram. Nothing is
//!    refused during the gap.
//! 3. `without_activation_the_door_binds_itself`: no inherited sockets, the
//!    same calls bind their own (the non-activated path is unchanged).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::print_stdout
)]

use std::io::{BufRead as _, Read as _, Write as _};
use std::net::{IpAddr, Ipv6Addr, SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::time::{Duration, Instant};

use multiview_frontdoor::{
    FrontDoor, FrontDoorConfig, ListenSockets, RedirectConfig, TcpDoorConfig, TlsRouting,
};

const CHILD_ENV: &str = "FRONTDOOR_ACTIVATION_CHILD";

fn any(port: u16) -> SocketAddr {
    SocketAddr::new(IpAddr::V6(Ipv6Addr::UNSPECIFIED), port)
}

fn free_port() -> u16 {
    // A port free for both TCP and UDP on [::].
    loop {
        let t = TcpListener::bind("[::]:0").unwrap();
        let port = t.local_addr().unwrap().port();
        if UdpSocket::bind(any(port)).is_ok() {
            return port;
        }
    }
}

/// Start a door on whatever `inherited` holds (binding the rest), the way
/// `multiview run` does.
fn start_door(
    mut inherited: ListenSockets,
    https: u16,
    redirect: u16,
) -> (FrontDoor, bool, bool, bool) {
    let udp = inherited.take_udp_all(any(https));
    let udp_inherited = !udp.is_empty();
    let mut door = FrontDoor::start_with(
        &FrontDoorConfig {
            port: https,
            workers: 1,
            ..FrontDoorConfig::default()
        },
        udp,
    )
    .unwrap();
    let tcp = inherited.take_tcp(any(https));
    let tcp_inherited = tcp.is_some();
    door.start_tcp_with(
        TcpDoorConfig {
            listen: any(https),
            first_byte_timeout: Duration::from_millis(500),
            ..TcpDoorConfig::default()
        },
        TlsRouting::default(),
        None,
        tcp,
    )
    .unwrap();
    let r = inherited.take_tcp(any(redirect));
    let redirect_inherited = r.is_some();
    door.start_redirect_with(
        RedirectConfig::new(any(redirect), "door.example".to_owned(), 443),
        r,
    )
    .unwrap();
    assert!(inherited.is_empty(), "every inherited socket was claimed");
    (door, udp_inherited, tcp_inherited, redirect_inherited)
}

/// The child half of the real-systemd test: runs only when re-executed by
/// `real_systemd_socket_activate_hands_tcp_redirect_and_udp_to_the_door`.
#[test]
fn activation_child() {
    let Some(ports) = std::env::var_os(CHILD_ENV) else {
        return;
    };
    let ports = ports.into_string().unwrap();
    let mut parts = ports.splitn(3, ',');
    let https: u16 = parts.next().unwrap().parse().unwrap();
    let redirect: u16 = parts.next().unwrap().parse().unwrap();
    let stop = std::path::PathBuf::from(parts.next().unwrap());
    // As `main` does: adopt before any other thread reads the environment
    // (the harness runs this test alone, `--test-threads=1`).
    let pid_matched = std::env::var("LISTEN_PID").ok() == Some(std::process::id().to_string());
    let inherited = ListenSockets::from_env();
    let env_cleared = std::env::var_os("LISTEN_FDS").is_none();
    let (door, udp, tcp, redirect_inherited) = start_door(inherited, https, redirect);
    println!(
        "READY pid_matched={pid_matched} udp={udp} tcp={tcp} redirect={redirect_inherited} env_cleared={env_cleared}"
    );
    std::io::stdout().flush().unwrap();
    // Serve until the parent has probed every socket and creates the stop
    // file (the activator gives the service /dev/null as stdin), then report
    // the counters.
    let deadline = Instant::now() + Duration::from_secs(120);
    while !stop.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    let stats = door.stats().snapshot();
    println!(
        "STATS received={} tcp_accepted={} redirect_served={}",
        stats.received, stats.tcp_accepted, stats.redirect_served
    );
    std::io::stdout().flush().unwrap();
    door.shutdown_blocking();
}

#[cfg(target_os = "linux")]
#[test]
fn real_systemd_socket_activate_hands_tcp_redirect_and_udp_to_the_door() {
    use std::process::{Command, Stdio};
    let tool = ["/usr/bin/systemd-socket-activate", "/bin/systemd-socket-activate"]
        .into_iter()
        .find(|p| std::path::Path::new(p).exists())
        .expect("systemd-socket-activate is installed (package `systemd`)");
    let https = free_port();
    let redirect = free_port();
    let exe = std::env::current_exe().unwrap();
    // systemd-socket-activate binds the sockets itself (as root does for
    // conspect.socket) and execs the command with LISTEN_FDS=3 and
    // LISTEN_PID = the exec'd pid. `-d` makes the following -l datagram.
    let stop = std::env::temp_dir().join(format!("fd-activation-stop-{}-{https}", std::process::id()));
    let _ = std::fs::remove_file(&stop);
    // `--datagram` applies to every --listen of one invocation, so the TCP
    // sockets come from an outer activator and the UDP one from an inner
    // activator, which passes the outer's descriptors through: the engine
    // receives fds 3, 4 (TCP) and 5 (UDP), as from conspect.socket.
    let mut child = Command::new(tool)
        .arg(format!("--listen=[::]:{https}"))
        .arg(format!("--listen=[::]:{redirect}"))
        .arg(tool)
        .arg("--datagram")
        .arg(format!("--listen=[::]:{https}"))
        .arg(format!(
            "--setenv={CHILD_ENV}={https},{redirect},{}",
            stop.display()
        ))
        .arg(exe)
        .args(["--exact", "activation_child", "--nocapture", "--test-threads=1"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    // systemd-socket-activate (like an `Accept=no` socket unit) execs the
    // service on the first activity. That first request is sent before any
    // engine exists: the kernel queues it, and the activated engine answers
    // it (the restart/boot property the socket unit gives mda001).
    let mut first = loop {
        match TcpStream::connect(("::1", redirect)) {
            Ok(s) => break s,
            Err(_) => std::thread::sleep(Duration::from_millis(20)),
        }
    };
    first.write_all(b"GET /first HTTP/1.1\r\n\r\n").unwrap();
    first.set_read_timeout(Some(Duration::from_secs(60))).unwrap();
    let mut out = std::io::BufReader::new(child.stdout.take().unwrap());
    let mut ready = String::new();
    let deadline = Instant::now() + Duration::from_secs(60);
    while !ready.contains("READY ") {
        ready.clear();
        assert!(Instant::now() < deadline, "child never became ready");
        assert!(out.read_line(&mut ready).unwrap() > 0, "child exited before READY");
    }
    assert!(
        ready
            .trim_end()
            .ends_with("READY pid_matched=true udp=true tcp=true redirect=true env_cleared=true"),
        "{ready}"
    );

    let mut first_body = String::new();
    first.read_to_string(&mut first_body).unwrap();
    assert!(
        first_body.contains("Location: https://door.example/first\r\n"),
        "{first_body}"
    );

    // The redirect: a real 301 from the inherited port-80 socket.
    let mut s = TcpStream::connect(("::1", redirect)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    s.write_all(b"GET /x HTTP/1.1\r\nHost: evil\r\n\r\n").unwrap();
    let mut body = String::new();
    s.read_to_string(&mut body).unwrap();
    assert!(body.starts_with("HTTP/1.1 301"), "{body}");
    assert!(body.contains("Location: https://door.example/x\r\n"), "{body}");

    // The TCP door: accepted and classified (a plaintext byte is closed).
    let mut t = TcpStream::connect(("::1", https)).unwrap();
    t.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    t.write_all(b"G").unwrap();
    let mut rest = Vec::new();
    let _ = t.read_to_end(&mut rest);
    assert!(rest.is_empty(), "plaintext gets no bytes");
    // IPv4 reaches the same inherited dual-stack socket.
    TcpStream::connect(("127.0.0.1", https)).unwrap();

    // The UDP door: a STUN-shaped datagram reaches the dispatcher.
    let u = UdpSocket::bind("[::1]:0").unwrap();
    let mut stun = vec![0u8, 1, 0, 0, 0x21, 0x12, 0xa4, 0x42];
    stun.extend_from_slice(&[7u8; 12]);
    for _ in 0..5 {
        u.send_to(&stun, ("::1", https)).unwrap();
    }
    std::thread::sleep(Duration::from_millis(300));

    std::fs::write(&stop, b"").unwrap();
    let mut stats = String::new();
    while !stats.contains("STATS ") {
        stats.clear();
        assert!(out.read_line(&mut stats).unwrap() > 0, "child exited before STATS");
    }
    let field = |name: &str| -> u64 {
        stats
            .split_whitespace()
            .find_map(|kv| kv.strip_prefix(&format!("{name}=")))
            .unwrap()
            .parse()
            .unwrap()
    };
    assert!(field("received") >= 1, "{stats}");
    assert!(field("tcp_accepted") >= 2, "{stats}");
    assert_eq!(field("redirect_served"), 2, "{stats}");
    assert!(child.wait().unwrap().success());
    let _ = std::fs::remove_file(&stop);
}

/// The service manager's copy (kept across "restarts") and a duplicate for
/// one engine run.
fn dup(l: &(TcpListener, TcpListener, UdpSocket)) -> ListenSockets {
    let mut s = ListenSockets::empty();
    s.push_tcp(l.0.try_clone().unwrap());
    s.push_tcp(l.1.try_clone().unwrap());
    s.push_udp(l.2.try_clone().unwrap());
    s
}

fn manager_sockets(https: u16, redirect: u16) -> (TcpListener, TcpListener, UdpSocket) {
    use socket2::{Domain, Protocol, Socket, Type};
    let tcp = |port| {
        let s = Socket::new(Domain::IPV6, Type::STREAM, Some(Protocol::TCP)).unwrap();
        s.set_only_v6(false).unwrap();
        s.bind(&any(port).into()).unwrap();
        s.listen(128).unwrap();
        TcpListener::from(s)
    };
    let u = Socket::new(Domain::IPV6, Type::DGRAM, Some(Protocol::UDP)).unwrap();
    u.set_only_v6(false).unwrap();
    u.bind(&any(https).into()).unwrap();
    (tcp(https), tcp(redirect), UdpSocket::from(u))
}

#[test]
fn a_restart_keeps_the_sockets() {
    let https = free_port();
    let redirect = free_port();
    let manager = manager_sockets(https, redirect);

    // Engine run A.
    let (a, udp, tcp, r) = start_door(dup(&manager), https, redirect);
    assert!(udp && tcp && r);
    a.shutdown_blocking();

    // The gap: no engine. The kernel still owns the listening sockets, so a
    // connect completes from the backlog (never ECONNREFUSED) and a datagram
    // waits in the receive buffer.
    let mut queued = TcpStream::connect(("::1", redirect)).expect("connect during the restart gap");
    queued
        .write_all(b"GET /during-restart HTTP/1.1\r\n\r\n")
        .unwrap();
    queued
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let u = UdpSocket::bind("[::1]:0").unwrap();
    let mut stun = vec![0u8, 1, 0, 0, 0x21, 0x12, 0xa4, 0x42];
    stun.extend_from_slice(&[9u8; 12]);
    u.send_to(&stun, ("::1", https)).unwrap();
    TcpStream::connect(("::1", https)).expect("TCP door port accepts during the gap");

    // Engine run B adopts the same sockets and serves what queued.
    let (b, ..) = start_door(dup(&manager), https, redirect);
    let mut body = String::new();
    queued.read_to_string(&mut body).unwrap();
    assert!(
        body.contains("Location: https://door.example/during-restart\r\n"),
        "{body}"
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    while b.stats().snapshot().received == 0 {
        assert!(Instant::now() < deadline, "the queued datagram was never read");
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(b.stats().snapshot().tcp_accepted >= 1);
    b.shutdown_blocking();
}

#[test]
fn without_activation_the_door_binds_itself() {
    let https = free_port();
    let redirect = free_port();
    let (door, udp, tcp, r) = start_door(ListenSockets::empty(), https, redirect);
    assert!(!udp && !tcp && !r);
    assert_eq!(door.local_addr().port(), https);
    assert_eq!(door.tcp_local_addr().unwrap().port(), https);
    assert_eq!(door.redirect_local_addr().unwrap().port(), redirect);
    door.shutdown_blocking();
}

/// `conspect.socket` passes UDP 443 as a `ReusePort=yes` pair (two
/// `ListenDatagram=` lines, listener first). The door adopts the pair and
/// keeps slice 3a's validated-flow steering: an established peer's datagrams
/// are read from the second socket, everyone else's from the first.
#[cfg(target_os = "linux")]
#[test]
fn an_inherited_reuseport_pair_keeps_validated_flow_steering() {
    use multiview_frontdoor::{ReuseportGroup, SteeringMode};
    // The "service manager": a reuseport pair bound in order, as systemd
    // binds the two ListenDatagram= lines.
    let group = ReuseportGroup::bind(any(0), 0).unwrap();
    let port = group.listener().local_addr().unwrap().port();
    let (listener, established, _) = group.into_parts();
    let mut inherited = ListenSockets::empty();
    inherited.push_udp(listener);
    inherited.push_udp(established);
    let pair = inherited.take_udp_all(any(port));
    assert_eq!(pair.len(), 2);
    let door = FrontDoor::start_with(
        &FrontDoorConfig {
            port,
            workers: 1,
            ..FrontDoorConfig::default()
        },
        pair,
    )
    .unwrap();
    assert_eq!(door.steering(), SteeringMode::ReuseportCbpf);

    let member = UdpSocket::bind("[::1]:0").unwrap();
    let other = UdpSocket::bind("[::1]:0").unwrap();
    let before = door.stats().snapshot();
    assert!(door.quic_established().insert(member.local_addr().unwrap()));
    let deadline = Instant::now() + Duration::from_secs(10);
    while door.stats().snapshot().steered_peers != 1
        || door.stats().snapshot().steering_updates <= before.steering_updates
    {
        assert!(Instant::now() < deadline, "the steering program never attached");
        std::thread::sleep(Duration::from_millis(5));
    }
    let mut short = vec![7u8; 60];
    short[0] = 0x45;
    for _ in 0..50 {
        member.send_to(&short, ("::1", port)).unwrap();
        other.send_to(&short, ("::1", port)).unwrap();
        std::thread::sleep(Duration::from_millis(1));
    }
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let s = door.stats().snapshot();
        if s.received >= before.received + 100 {
            assert_eq!(s.received_established_socket - before.received_established_socket, 50, "{s:?}");
            assert_eq!(s.received_listener_socket - before.received_listener_socket, 50, "{s:?}");
            break;
        }
        assert!(Instant::now() < deadline, "datagrams never arrived: {s:?}");
        std::thread::sleep(Duration::from_millis(5));
    }
    door.shutdown_blocking();
}
