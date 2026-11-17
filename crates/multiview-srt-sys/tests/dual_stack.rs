//! One `[::]` listener must accept real callers over IPv6 AND IPv4 — the
//! operator requirement `FFmpeg`'s libsrt protocol cannot meet. Peers are the
//! `ffmpeg` CLI's own libsrt caller, so the handshake is independent of this
//! crate. Skips only when libsrt or the CLI is genuinely absent, or when the
//! host libsrt is below the listener security floor (then the refusal itself
//! is asserted instead). `MULTIVIEW_REQUIRE_LIBSRT_FLOOR=1` (set by the CI legs
//! that provision the pinned libsrt) turns every skip into a failure, so a
//! leg that should prove 1.5.7 can never pass by skipping.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::print_stderr
)]

use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use multiview_srt_sys::{ListenerOptions, SrtError, SrtLib, SrtListener, MIN_LISTENER_LIBSRT};

fn floor_required() -> bool {
    std::env::var_os("MULTIVIEW_REQUIRE_LIBSRT_FLOOR").is_some_and(|v| v == "1")
}

fn cli_has_srt() -> bool {
    Command::new("ffmpeg")
        .args(["-hide_banner", "-protocols"])
        .output()
        .is_ok_and(|out| {
            String::from_utf8_lossy(&out.stdout)
                .lines()
                .any(|l| l.trim() == "srt")
        })
}

/// Whether the real-socket tests can run here: libsrt loads, is at or above
/// the floor, and the `ffmpeg` CLI speaks SRT. Below the floor it proves the
/// refusal on the real library first. Panics instead of skipping when the
/// environment promised the floor.
fn ready_for_real_listener() -> bool {
    let lib = match SrtLib::shared() {
        Ok(lib) => lib,
        Err(e) => {
            assert!(!floor_required(), "libsrt must load here: {e}");
            eprintln!("SKIP: libsrt is not present ({e})");
            return false;
        }
    };
    if lib.version() < MIN_LISTENER_LIBSRT {
        assert!(
            !floor_required(),
            "libsrt {} is below the floor {MIN_LISTENER_LIBSRT}",
            lib.version()
        );
        let refused =
            SrtListener::bind("[::1]:0".parse().unwrap(), &ListenerOptions::receiver(), 1);
        assert!(
            matches!(refused, Err(SrtError::LibsrtTooOld { found, .. }) if found == lib.version()),
            "libsrt {} must refuse a listener, got {refused:?}",
            lib.version()
        );
        eprintln!(
            "SKIP: libsrt {} is below the floor (refusal verified)",
            lib.version()
        );
        return false;
    }
    if !cli_has_srt() {
        assert!(
            !floor_required(),
            "an SRT-capable ffmpeg CLI must be present here"
        );
        eprintln!("SKIP: no SRT-capable ffmpeg CLI");
        return false;
    }
    true
}

/// The integration half of the security floor: the library actually loaded is
/// at or above 1.5.7 and a real listener starts on it. Skips (or, under
/// `MULTIVIEW_REQUIRE_LIBSRT_FLOOR=1`, fails) without libsrt; below the floor
/// asserts the typed refusal on the real library instead.
#[test]
fn listener_starts_on_libsrt_at_or_above_the_floor() {
    let lib = match SrtLib::shared() {
        Ok(lib) => lib,
        Err(e) => {
            assert!(!floor_required(), "libsrt must load here: {e}");
            eprintln!("SKIP: libsrt is not present ({e})");
            return;
        }
    };
    let version = lib.version();
    eprintln!("loaded libsrt {version}");
    let bound = SrtListener::bind("[::1]:0".parse().unwrap(), &ListenerOptions::receiver(), 1);
    if version >= MIN_LISTENER_LIBSRT {
        bound.unwrap_or_else(|e| panic!("libsrt {version} must start a listener: {e}"));
    } else {
        assert!(
            !floor_required(),
            "libsrt {version} is below the floor {MIN_LISTENER_LIBSRT}"
        );
        assert!(
            matches!(bound, Err(SrtError::LibsrtTooOld { found, required }) if found == version && required == MIN_LISTENER_LIBSRT),
            "libsrt {version} must be refused, got {bound:?}"
        );
    }
}

fn send_ts_from(caller_host: &str, port: u16) -> std::process::Child {
    Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-re",
            "-f",
            "lavfi",
            "-i",
        ])
        .arg("testsrc=size=160x120:rate=30:duration=4")
        .args(["-c:v", "mpeg2video", "-f", "mpegts"])
        .arg(format!("srt://{caller_host}:{port}?mode=caller"))
        .stdin(Stdio::null())
        .spawn()
        .expect("spawn ffmpeg caller")
}

fn accept_and_read(listener: &SrtListener, caller_host: &str, port: u16) -> std::net::SocketAddr {
    let mut child = send_ts_from(caller_host, port);
    let stop = Arc::new(AtomicBool::new(false));
    let timer = {
        let stop = Arc::clone(&stop);
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_secs(8));
            stop.store(true, Ordering::Release);
        })
    };
    let flag = Arc::clone(&stop);
    let (socket, peer) = listener
        .accept(&move || flag.load(Ordering::Acquire))
        .unwrap_or_else(|e| panic!("no caller over {caller_host}: {e}"));
    let mut buf = [0_u8; 1500];
    let mut bytes = 0;
    for _ in 0..40 {
        if let Some(n) = socket.recv(&mut buf).expect("recv") {
            assert_eq!(buf[0], 0x47, "MPEG-TS sync byte");
            bytes += n;
            if bytes > 10_000 {
                break;
            }
        }
    }
    assert!(bytes > 10_000, "only {bytes} bytes over {caller_host}");
    drop(socket);
    let _ = child.kill();
    let _ = child.wait();
    stop.store(true, Ordering::Release);
    timer.join().unwrap();
    peer.expect("peer address")
}

#[test]
fn one_wildcard_listener_accepts_ipv6_and_ipv4_callers() {
    if !ready_for_real_listener() {
        return;
    }
    let port = 21_000 + u16::try_from(std::process::id() % 20_000).unwrap();
    let listener = SrtListener::bind(
        format!("[::]:{port}").parse().unwrap(),
        &ListenerOptions::receiver(),
        8,
    )
    .expect("[::] must bind with SRTO_IPV6ONLY=0");

    let v6 = accept_and_read(&listener, "[::1]", port);
    assert!(v6.is_ipv6(), "peer over [::1] was {v6}");
    let v4 = accept_and_read(&listener, "127.0.0.1", port);
    assert!(v4.is_ipv4(), "peer over 127.0.0.1 was {v4}");
}

/// SRT-05: `send` honours stop and its deadline BEFORE every chunk, not only
/// after a failed send. Against a healthy, reading caller every send would
/// succeed, so an expired deadline or a raised stop must still refuse at once.
#[test]
fn send_checks_stop_and_deadline_before_every_chunk() {
    if !ready_for_real_listener() {
        return;
    }
    let port = 23_000 + u16::try_from(std::process::id() % 20_000).unwrap();
    let listener = SrtListener::bind(
        format!("[::1]:{port}").parse().unwrap(),
        &ListenerOptions::sender(),
        2,
    )
    .unwrap();
    let mut reader = Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "quiet", "-i"])
        .arg(format!("srt://[::1]:{port}?mode=caller"))
        .args(["-f", "null", "-"])
        .stdin(Stdio::null())
        .spawn()
        .expect("spawn ffmpeg reader");
    let stop = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&stop);
    let timer = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_secs(10));
        flag.store(true, Ordering::Release);
    });
    let flag = Arc::clone(&stop);
    let (socket, _) = listener
        .accept(&move || flag.load(Ordering::Acquire))
        .expect("reader connects");
    let data = vec![0x47_u8; 1316 * 4];
    let started = std::time::Instant::now();
    let never = || false;
    socket
        .send(&data, started + Duration::from_secs(5), &never)
        .expect("a healthy peer takes a send inside its deadline");
    assert!(matches!(
        socket.send(&data, std::time::Instant::now(), &never),
        Err(multiview_srt_sys::SrtError::TimedOut)
    ));
    let stopped = || true;
    assert!(matches!(
        socket.send(
            &data,
            std::time::Instant::now() + Duration::from_secs(5),
            &stopped
        ),
        Err(multiview_srt_sys::SrtError::Interrupted)
    ));
    drop(socket);
    let _ = reader.kill();
    let _ = reader.wait();
    stop.store(true, Ordering::Release);
    timer.join().unwrap();
}
