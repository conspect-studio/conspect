//! Live in-process RTSP server playout (OUT-2) — **ignored by default**.
//!
//! This test needs the GStreamer/GLib C stack **and** the `gst-rtsp-server`,
//! `appsrc`, `h264parse`, and `rtph264pay` plugins installed, plus a free TCP
//! port. None of that is present in the GPU-free CI, so the whole file is behind
//! the off-by-default `rtsp-server` feature *and* each test is `#[ignore]`d with
//! a reason — run it on a GStreamer-equipped runner with:
//!
//! ```text
//! cargo test -p multiview-output --features rtsp-server -- --ignored
//! ```
//!
//! The full client-pull assertion (an `ffprobe`/`gst-launch rtspsrc` reading
//! `rtsp://[::1]:<port>/<mount>` and checking codec/geometry + a gap-free
//! frame run, plus a second simultaneous client proving `set_shared(true)`
//! fan-out and a slow/abandoned client not stalling the others) is the
//! GStreamer-runner acceptance for OUT-2 and is sketched in the body — it
//! requires real encoded NALs and a network peer, which this scaffold marks as
//! the deferred live half.
#![cfg(feature = "rtsp-server")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::Arc;
use std::time::{Duration, Instant};

use multiview_output::fanout::{EncodedPacket, PacketKind, PacketSink, RenditionId};
use multiview_output::rtsp_server::server::{RtspServerConfig, RtspServerHandle};
use multiview_output::rtsp_server::{BoundedPacketQueue, RtspCodec, RtspMount, RtspServerSink};

/// The server binds and accepts a real RTSP/1.0 client request sequence. The
/// client uses only `TcpStream`, so request dispatch is asserted independently
/// of any `GStreamer` client library.
///
/// Ignored by default: requires `GStreamer` + `gst-rtsp-server` plugins + a free
/// port. CI has no GStreamer-equipped leg yet (#141).
#[test]
#[ignore = "requires the GStreamer/gst-rtsp-server C stack + plugins (not in CI); run with --ignored on a gst-equipped runner"]
fn rtsp_server_serves_describe_setup_play_to_tcp_client() {
    let mount = RtspMount::new("program").unwrap();
    let config = RtspServerConfig {
        host: "[::1]".to_owned(),
        port: 8554,
        mount,
        codec: RtspCodec::H264,
        timebase: (1, 90000),
    };
    let queue: Arc<BoundedPacketQueue> = Arc::new(BoundedPacketQueue::new(8));
    let sink = RtspServerSink::with_queue("rtsp-program", Arc::clone(&queue));
    let handle = RtspServerHandle::start_with_queue(config, queue).expect("start RTSP server");
    let mut client = TcpStream::connect("[::1]:8554").unwrap();
    client
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    client
        .set_write_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    let mut cseq = 1;
    let options = send_request(&mut client, handle.served_url(), "OPTIONS", cseq, None);
    assert!(
        options.starts_with("RTSP/1.0 200"),
        "OPTIONS response: {options}"
    );
    cseq += 1;

    let describe = send_request(
        &mut client,
        handle.served_url(),
        "DESCRIBE",
        cseq,
        Some("Accept: application/sdp\r\n"),
    );
    assert!(
        describe.starts_with("RTSP/1.0 200"),
        "DESCRIBE response: {describe}"
    );
    assert!(
        describe.contains("Content-Type: application/sdp"),
        "DESCRIBE headers: {describe}"
    );
    assert!(describe.contains("m=video"), "SDP body: {describe}");
    cseq += 1;

    let setup = send_request(
        &mut client,
        handle.served_url(),
        "SETUP",
        cseq,
        Some("Transport: RTP/AVP/TCP;unicast;interleaved=0-1\r\n"),
    );
    assert!(setup.starts_with("RTSP/1.0 200"), "SETUP response: {setup}");
    let session = response_header(&setup, "Session").expect("SETUP has Session header");
    cseq += 1;

    let play = send_request(
        &mut client,
        handle.served_url(),
        "PLAY",
        cseq,
        Some(&format!("Session: {session}\r\n")),
    );
    assert!(play.starts_with("RTSP/1.0 200"), "PLAY response: {play}");
    assert!(
        response_header(&play, "RTP-Info").is_some(),
        "PLAY response has RTP-Info: {play}"
    );

    // A queued RTP payload reaches the server-side pump after PLAY. The test
    // validates client-visible RTSP setup/play; actual interleaved RTP delivery
    // requires valid SPS/PPS/IDR NAL units and remains part of the future media
    // acceptance, not this server-context regression test.
    let packet = Arc::new(EncodedPacket::new(
        RenditionId::new("program"),
        PacketKind::VideoKeyframe,
        multiview_core::encoded_packet::StreamKind::Video,
        false,
        0,
        0,
        3000,
        vec![0, 0, 0, 1, 0x67].into_boxed_slice(),
    ));
    sink.deliver(&packet);
    drop(handle);
}

fn send_request(
    client: &mut TcpStream,
    url: &str,
    method: &str,
    cseq: u32,
    headers: Option<&str>,
) -> String {
    let header = headers.unwrap_or("");
    write!(
        client,
        "{method} {url} RTSP/1.0\r\nCSeq: {cseq}\r\n{header}\r\n"
    )
    .unwrap();
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut response = Vec::new();
    let mut buf = [0_u8; 2048];
    loop {
        if let Some(header_end) = response.windows(4).position(|w| w == b"\r\n\r\n") {
            let header_end = header_end + 4;
            let head = String::from_utf8_lossy(&response[..header_end]);
            let body_length = response_header(&head, "Content-Length")
                .and_then(|value| value.parse::<usize>().ok())
                .unwrap_or(0);
            if response.len() >= header_end + body_length {
                return String::from_utf8_lossy(&response).into_owned();
            }
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {method} response: {}",
            String::from_utf8_lossy(&response)
        );
        let count = match client.read(&mut buf) {
            Ok(count) => count,
            Err(error)
                if error.kind() == std::io::ErrorKind::WouldBlock
                    || error.kind() == std::io::ErrorKind::TimedOut =>
            {
                continue
            }
            Err(error) => panic!("reading {method} response failed: {error}"),
        };
        assert_ne!(count, 0, "server closed before {method} response");
        response.extend_from_slice(&buf[..count]);
    }
}

fn response_header<'a>(response: &'a str, name: &str) -> Option<&'a str> {
    response.lines().find_map(|line| {
        line.split_once(':')
            .filter(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.trim())
    })
}
