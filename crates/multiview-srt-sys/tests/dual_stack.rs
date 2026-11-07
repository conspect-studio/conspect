//! One `[::]` listener must accept real callers over IPv6 AND IPv4 — the
//! operator requirement `FFmpeg`'s libsrt protocol cannot meet. Peers are the
//! `ffmpeg` CLI's own libsrt caller, so the handshake is independent of this
//! crate. Skips only when libsrt or the CLI is genuinely absent.
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

use multiview_srt_sys::{ListenerOptions, SrtLib, SrtListener};

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
    let cli_has_srt = Command::new("ffmpeg")
        .args(["-hide_banner", "-protocols"])
        .output()
        .is_ok_and(|out| {
            String::from_utf8_lossy(&out.stdout)
                .lines()
                .any(|l| l.trim() == "srt")
        });
    if SrtLib::shared().is_err() || !cli_has_srt {
        eprintln!("SKIP: libsrt or an SRT-capable ffmpeg CLI is not present");
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
