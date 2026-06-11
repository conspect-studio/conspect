//! Demuxer interrupt + `rw_timeout` (GP-0, ADR-0030 §4 recovery teardown).
//!
//! A guarded passthrough that loses its live input must be able to **tear down a
//! wedged demuxer** so recovery can reconnect. Today `Demuxer::open` opens bare:
//! no `AVIOInterruptCB`, no `rw_timeout`, so a stalled live TCP/RTSP/SRT/UDP read
//! blocks `avformat_open_input` / `av_read_frame` forever and can never be
//! aborted. This suite proves [`Demuxer::open_with_interrupt`] aborts a blocked
//! read within the configured timeout instead of hanging.
//!
//! Gated behind the `ffmpeg` feature (needs libavformat). The socket path binds a
//! localhost `TcpListener` that accepts a connection then sleeps without ever
//! sending bytes — exactly the live-input wedge GP-0 must survive.
#![cfg(feature = "ffmpeg")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::io::Read;
use std::net::TcpListener;
use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};

use multiview_ffmpeg::demux::{DemuxOptions, Demuxer};

/// Bind a localhost listener and accept-then-stall: the accept loop reads nothing
/// and never writes, holding the connection open so the client's read blocks.
/// IPv6-first: binds the IPv6 loopback `[::1]` and returns a bracketed
/// `tcp://[::1]:PORT` URL (libav requires the brackets for an IPv6 literal).
fn spawn_black_hole_server() -> String {
    let listener = TcpListener::bind("[::1]:0").expect("bind IPv6 loopback ephemeral port");
    let addr = listener.local_addr().expect("local addr");
    thread::spawn(move || {
        // Accept connections and hold them open without ever sending data.
        for mut s in listener.incoming().flatten() {
            // Park the connection: block on a read that never completes so the
            // socket stays open and the peer's read stalls. Keep the stream alive
            // by holding it in this scope; drop only on process exit.
            let mut buf = [0_u8; 1];
            let _ = s.read(&mut buf);
            thread::sleep(Duration::from_secs(30));
            drop(s);
        }
    });
    // `SocketAddr`'s Display already brackets an IPv6 host (`[::1]:PORT`), so the
    // libav URL is well-formed for both families.
    format!("tcp://{addr}")
}

#[test]
fn open_with_interrupt_aborts_a_black_holed_socket_within_the_timeout() {
    let url = spawn_black_hole_server();
    let opts = DemuxOptions::new().with_rw_timeout(Duration::from_millis(500));

    let start = Instant::now();
    let result = Demuxer::open_with_interrupt(Path::new(&url), opts);
    let elapsed = start.elapsed();

    // The open must FAIL (the stream never delivers a header) and, crucially, it
    // must fail *promptly* — bounded by the timeout plus generous slack — rather
    // than blocking indefinitely on the wedged socket.
    assert!(
        result.is_err(),
        "opening a black-holed socket must not succeed"
    );
    assert!(
        elapsed < Duration::from_secs(5),
        "open must abort within the timeout, took {elapsed:?}"
    );
}

#[test]
fn open_with_interrupt_still_opens_a_healthy_local_file() {
    // The interrupt/timeout path must not regress the normal open of a finite,
    // already-available container. Generate a tiny LGPL clip and open it.
    let dir = tempfile::TempDir::new().unwrap();
    let clip = dir.path().join("av.mkv");
    let status = std::process::Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-y",
            "-f",
            "lavfi",
            "-i",
            "testsrc=size=160x120:rate=25",
            "-t",
            "1",
            "-c:v",
            "ffv1",
            "-pix_fmt",
            "yuv420p",
        ])
        .arg(&clip)
        .status()
        .expect("spawn ffmpeg CLI");
    assert!(status.success(), "ffmpeg CLI must generate the clip");

    let opts = DemuxOptions::new().with_rw_timeout(Duration::from_secs(5));
    let demux = Demuxer::open_with_interrupt(&clip, opts).expect("open healthy local file");
    let streams = demux.streams();
    assert!(
        !streams.is_empty(),
        "the clip must expose at least one stream"
    );
}

#[test]
fn read_packet_on_a_healthy_file_returns_packets_under_the_interrupt_path() {
    let dir = tempfile::TempDir::new().unwrap();
    let clip = dir.path().join("av.mkv");
    let status = std::process::Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-y",
            "-f",
            "lavfi",
            "-i",
            "testsrc=size=160x120:rate=25",
            "-t",
            "1",
            "-c:v",
            "ffv1",
            "-pix_fmt",
            "yuv420p",
        ])
        .arg(&clip)
        .status()
        .expect("spawn ffmpeg CLI");
    assert!(status.success());

    let opts = DemuxOptions::new().with_rw_timeout(Duration::from_secs(5));
    let mut demux = Demuxer::open_with_interrupt(&clip, opts).expect("open");
    let first = demux
        .read_packet()
        .expect("read without error")
        .expect("at least one packet");
    assert!(first.size() > 0, "the first packet must carry payload");
}
