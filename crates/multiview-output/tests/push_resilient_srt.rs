//! Resilient push over a **real** libsrt loopback (the `ffmpeg` feature).
//!
//! Drives [`PacketMuxSink::run_push_resilient`] with a paced live encoder and
//! real `ffmpeg` CLI SRT peers, proving: sequential callers on one listener,
//! simultaneous callers, caller-mode reconnect, the never-block drop policy with
//! no peer attached, and bounded teardown while waiting in `srt_accept`.
#![cfg(feature = "ffmpeg")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU16, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use ffmpeg_next::format::Pixel;
use ffmpeg_next::util::frame::Video;
use multiview_core::time::Rational;
use multiview_ffmpeg::{EncodedPacket, StreamCodecParameters, VideoEncodeTarget, VideoEncoder};
use multiview_output::push_link::{PushLinkState, PushLinkStatus};
use multiview_output::sink::{MuxStream, PacketMuxSink, PacketSource, PushProtocol, PushRunReport};
use multiview_output::Result;

const W: u32 = 160;
const H: u32 = 120;
const FPS: i64 = 30;

static NEXT_PORT: AtomicU16 = AtomicU16::new(0);

/// A distinct port per test (the harness runs tests concurrently).
fn port() -> u16 {
    let base = 20_000 + u16::try_from(std::process::id() % 20_000).unwrap();
    base + NEXT_PORT.fetch_add(1, Ordering::Relaxed)
}

/// A live packet source: encodes a frame per tick until `stop`.
struct LiveSource {
    encoder: VideoEncoder,
    tick: i64,
    pending: std::collections::VecDeque<EncodedPacket>,
    stop: Arc<AtomicBool>,
    pace: Option<Duration>,
}

impl LiveSource {
    fn new(stop: Arc<AtomicBool>, pace: Option<Duration>) -> Self {
        let encoder = VideoEncoder::new(&VideoEncodeTarget {
            codec_name: "mpeg2video".to_owned(),
            width: W,
            height: H,
            format: Pixel::YUV420P,
            time_base: Rational::new(1, FPS),
            bit_rate: 500_000,
            gop: 10,
            cuda_device: None,
        })
        .expect("encoder");
        Self {
            encoder,
            tick: 0,
            pending: std::collections::VecDeque::new(),
            stop,
            pace,
        }
    }

    fn params(&self) -> (StreamCodecParameters, Rational) {
        (
            StreamCodecParameters::from_encoder(&self.encoder),
            self.encoder.time_base(),
        )
    }
}

impl PacketSource for LiveSource {
    fn next_packet(&mut self) -> Result<Option<EncodedPacket>> {
        loop {
            if self.stop.load(Ordering::Acquire) {
                return Ok(None);
            }
            if let Some(packet) = self.pending.pop_front() {
                return Ok(Some(packet));
            }
            if let Some(pace) = self.pace {
                std::thread::sleep(pace);
            }
            let mut frame = Video::new(Pixel::YUV420P, W, H);
            let shade = u8::try_from(self.tick.rem_euclid(200)).unwrap();
            for plane in 0..3 {
                frame.data_mut(plane).fill(shade);
            }
            frame.set_pts(Some(self.tick));
            self.tick += 1;
            self.encoder.send_frame(&frame).expect("send");
            while let Some(p) = self.encoder.receive_packet().expect("recv") {
                self.pending.push_back(EncodedPacket::from_packet(p));
            }
        }
    }
}

/// A running push under test.
struct Run {
    stop: Arc<AtomicBool>,
    link: Arc<PushLinkStatus>,
    handle: std::thread::JoinHandle<PushRunReport>,
}

fn start(url: String, listener: bool, pace: Option<Duration>) -> Run {
    let stop = Arc::new(AtomicBool::new(false));
    let link = Arc::new(PushLinkStatus::new("srt", listener, url.clone()));
    let handle = {
        let (stop, link) = (Arc::clone(&stop), Arc::clone(&link));
        std::thread::spawn(move || {
            let mut source = LiveSource::new(stop, pace);
            let (params, tb) = source.params();
            PacketMuxSink::push(PushProtocol::Srt, url)
                .run_push_resilient(&mut source, MuxStream::new(&params, tb), None, &[], &link)
                .expect("resilient push")
        })
    };
    Run { stop, link, handle }
}

fn paced() -> Option<Duration> {
    Some(Duration::from_millis(33))
}

fn wait_for(what: &str, timeout: Duration, mut cond: impl FnMut() -> bool) {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if cond() {
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("timed out waiting for {what}");
}

/// A real SRT caller that decodes `frames` video frames, then disconnects.
fn caller(port: u16, frames: u32) -> Child {
    caller_over("127.0.0.1", port, frames)
}

fn caller_over(host: &str, port: u16, frames: u32) -> Child {
    caller_over_query(host, port, frames, "")
}

fn caller_over_query(host: &str, port: u16, frames: u32, query: &str) -> Child {
    Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-i"])
        .arg(format!("srt://{host}:{port}?mode=caller{query}"))
        .args(["-frames:v", &frames.to_string(), "-f", "null", "-"])
        .stdin(Stdio::null())
        .spawn()
        .expect("spawn ffmpeg caller")
}

fn assert_decoded(mut child: Child, who: &str) {
    let deadline = Instant::now() + Duration::from_secs(25);
    loop {
        if let Some(status) = child.try_wait().expect("wait") {
            assert!(status.success(), "{who}: caller failed: {status}");
            return;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            panic!("{who}: caller did not decode frames in time");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn listener_serves_two_sequential_callers() {
    let port = port();
    let run = start(
        format!("srt://127.0.0.1:{port}?mode=listener"),
        true,
        paced(),
    );
    wait_for("listening", Duration::from_secs(5), || {
        run.link.snapshot().state == PushLinkState::Listening
    });
    assert_decoded(caller(port, 20), "first");
    wait_for("first caller detach", Duration::from_secs(15), || {
        run.link.clients() == 0
    });
    assert_decoded(caller(port, 20), "second");
    let snap = run.link.snapshot();
    assert!(snap.sessions >= 2, "two sessions: {snap:?}");
    assert!(snap.written > 0, "packets reached peers: {snap:?}");
    run.stop.store(true, Ordering::Release);
    run.handle.join().unwrap();
}

/// The operator requirement: `[::]` serves IPv6 and IPv4 callers at once, and
/// the status reports both real peer addresses.
#[test]
fn wildcard_listener_is_dual_stack_and_reports_real_peers() {
    let port = port();
    let run = start(format!("srt://[::]:{port}?mode=listener"), true, paced());
    wait_for("listening", Duration::from_secs(5), || {
        run.link.snapshot().state == PushLinkState::Listening
    });
    assert!(run.link.snapshot().last_error.is_none());
    let v6 = caller_over("[::1]", port, 120);
    let v4 = caller_over("127.0.0.1", port, 120);
    wait_for("both attached", Duration::from_secs(10), || {
        run.link.clients() == 2
    });
    let peers = run.link.snapshot().peers;
    assert!(peers.iter().any(|p| p.starts_with("[::1]:")), "{peers:?}");
    assert!(
        peers.iter().any(|p| p.starts_with("127.0.0.1:")),
        "{peers:?}"
    );
    assert_decoded(v6, "ipv6 caller");
    assert_decoded(v4, "ipv4 caller");
    run.stop.store(true, Ordering::Release);
    run.handle.join().unwrap();
}

/// `0.0.0.0` is what production configs say today; on the native path it binds
/// v4-mapped, which must still take IPv4 callers.
#[test]
fn explicit_ipv4_wildcard_listener_still_serves_ipv4_callers() {
    let port = port();
    let run = start(format!("srt://0.0.0.0:{port}?mode=listener"), true, paced());
    wait_for("listening", Duration::from_secs(5), || {
        run.link.snapshot().state == PushLinkState::Listening
    });
    assert_decoded(caller_over("127.0.0.1", port, 20), "ipv4 caller");
    let snap = run.link.snapshot();
    assert_eq!(snap.sessions, 1, "exactly the one caller: {snap:?}");
    assert!(snap.written > 0, "packets reached the caller: {snap:?}");
    run.stop.store(true, Ordering::Release);
    run.handle.join().unwrap();
}

/// Chaos (R3, invariants #1/#10): callers connecting and vanishing without a
/// clean close must never slow the producer, and every session must be released.
#[test]
fn caller_churn_never_slows_the_producer() {
    let port = port();
    let run = start(format!("srt://[::]:{port}?mode=listener"), true, paced());
    wait_for("listening", Duration::from_secs(5), || {
        run.link.snapshot().state == PushLinkState::Listening
    });
    let started = Instant::now();
    let mut sessions = 0;
    for round in 0..6_u64 {
        let mut child = caller_over("[::1]", port, 100_000);
        wait_for("attach", Duration::from_secs(10), || {
            run.link.snapshot().sessions > round
        });
        std::thread::sleep(Duration::from_millis(300));
        child.kill().expect("kill caller"); // no SRT shutdown handshake
        child.wait().expect("reap caller");
        sessions += 1;
    }
    let elapsed = started.elapsed();
    let snap = run.link.snapshot();
    assert_eq!(snap.sessions, sessions, "{snap:?}");
    run.stop.store(true, Ordering::Release);
    let report = run.handle.join().unwrap();
    // A 33 ms pace yields ~30 packets/s; a stalled producer would fall far short.
    let expected = u64::try_from(elapsed.as_millis()).unwrap() / 33;
    assert!(
        report.received * 10 >= expected * 7,
        "producer stalled: {} packets in {elapsed:?} (expected ~{expected})",
        report.received
    );
    assert_eq!(run.link.clients(), 0, "every killed session is released");
}

#[test]
fn listener_serves_two_simultaneous_callers() {
    let port = port();
    let run = start(
        format!("srt://127.0.0.1:{port}?mode=listener"),
        true,
        paced(),
    );
    wait_for("listening", Duration::from_secs(5), || {
        run.link.snapshot().state == PushLinkState::Listening
    });
    let first = caller(port, 120);
    wait_for("first attach", Duration::from_secs(10), || {
        run.link.clients() == 1
    });
    let second = caller(port, 30);
    wait_for("both attached", Duration::from_secs(10), || {
        run.link.clients() == 2
    });
    assert_decoded(second, "second");
    assert_decoded(first, "first");
    run.stop.store(true, Ordering::Release);
    run.handle.join().unwrap();
}

#[test]
fn caller_reconnects_after_the_peer_goes_away() {
    let port = port();
    let peer = |frames: u32| {
        Command::new("ffmpeg")
            .args(["-hide_banner", "-loglevel", "error", "-i"])
            .arg(format!("srt://127.0.0.1:{port}?mode=listener"))
            .args(["-frames:v", &frames.to_string(), "-f", "null", "-"])
            .stdin(Stdio::null())
            .spawn()
            .expect("spawn ffmpeg listener peer")
    };
    // The push starts with NO peer: it must retry rather than die.
    let run = start(
        format!("srt://127.0.0.1:{port}?mode=caller"),
        false,
        paced(),
    );
    std::thread::sleep(Duration::from_millis(700));
    assert_decoded(peer(20), "first peer");
    wait_for("detach", Duration::from_secs(15), || {
        run.link.clients() == 0
    });
    assert_decoded(peer(20), "second peer");
    assert!(run.link.snapshot().sessions >= 2);
    run.stop.store(true, Ordering::Release);
    run.handle.join().unwrap();
}

#[test]
fn no_peer_never_blocks_the_producer_and_counts_drops() {
    let port = port();
    // Unpaced: the source runs flat out; any blocking would stall it.
    let run = start(format!("srt://127.0.0.1:{port}?mode=listener"), true, None);
    wait_for("500 drops", Duration::from_secs(20), || {
        run.link.dropped() >= 500
    });
    assert_eq!(run.link.clients(), 0);
    run.stop.store(true, Ordering::Release);
    let report = run.handle.join().unwrap();
    assert_eq!(report.dropped, report.received, "all dropped: {report:?}");
}

#[test]
fn teardown_is_bounded_while_waiting_for_a_caller() {
    let port = port();
    let run = start(
        format!("srt://127.0.0.1:{port}?mode=listener"),
        true,
        paced(),
    );
    wait_for("listening", Duration::from_secs(5), || {
        run.link.snapshot().state == PushLinkState::Listening
    });
    std::thread::sleep(Duration::from_millis(300));
    let started = Instant::now();
    run.stop.store(true, Ordering::Release);
    run.handle.join().unwrap();
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "teardown took {:?}",
        started.elapsed()
    );
    assert_eq!(run.link.snapshot().state, PushLinkState::Idle);
}

/// Encryption options must really reach the native socket: the right passphrase
/// decodes, and a caller with the wrong one never attaches.
#[test]
fn encrypted_listener_admits_only_the_right_passphrase() {
    let port = port();
    let secret = "correct-horse-battery";
    let run = start(
        format!("srt://[::]:{port}?mode=listener&pbkeylen=32&passphrase={secret}"),
        true,
        paced(),
    );
    wait_for("listening", Duration::from_secs(5), || {
        run.link.snapshot().state == PushLinkState::Listening
    });
    let wrong = Command::new("timeout")
        .args(["6", "ffmpeg", "-hide_banner", "-loglevel", "quiet", "-i"])
        .arg(format!(
            "srt://[::1]:{port}?mode=caller&pbkeylen=32&passphrase=wrong-passphrase-x"
        ))
        .args(["-frames:v", "5", "-f", "null", "-"])
        .stdin(Stdio::null())
        .status()
        .expect("spawn wrong caller");
    assert!(!wrong.success(), "a wrong passphrase must not decode");
    assert_eq!(run.link.snapshot().sessions, 0, "wrong passphrase attached");
    assert_decoded(
        caller_over_query(
            "[::1]",
            port,
            20,
            &format!("&pbkeylen=32&passphrase={secret}"),
        ),
        "right passphrase",
    );
    run.stop.store(true, Ordering::Release);
    run.handle.join().unwrap();
}

/// `stop` must stay bounded with a caller attached that has stopped reading:
/// the writer is mid-send to a peer that is alive but silent.
#[test]
fn teardown_is_bounded_with_a_frozen_caller_attached() {
    let port = port();
    let run = start(format!("srt://[::]:{port}?mode=listener"), true, None);
    wait_for("listening", Duration::from_secs(5), || {
        run.link.snapshot().state == PushLinkState::Listening
    });
    let mut child = caller_over("[::1]", port, 100_000);
    wait_for("attach", Duration::from_secs(10), || {
        run.link.clients() == 1
    });
    let pid = child.id().to_string();
    let frozen = Command::new("kill").args(["-STOP", &pid]).status().unwrap();
    assert!(frozen.success());
    std::thread::sleep(Duration::from_secs(1));
    let started = Instant::now();
    run.stop.store(true, Ordering::Release);
    run.handle.join().unwrap();
    let elapsed = started.elapsed();
    child.kill().expect("kill caller");
    child.wait().expect("reap caller");
    assert!(
        elapsed < Duration::from_secs(2),
        "teardown took {elapsed:?}"
    );
}
