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

use std::sync::Arc;

use multiview_output::fanout::{EncodedPacket, PacketKind, PacketSink, RenditionId};
use multiview_output::rtsp_server::server::{RtspServerConfig, RtspServerHandle};
use multiview_output::rtsp_server::{BoundedPacketQueue, RtspCodec, RtspMount, RtspServerSink};

/// The server binds, exposes the configured mount URL, and the engine-side
/// `RtspServerSink` feeds the same bounded queue the serving side drains —
/// without the producer ever blocking.
///
/// Ignored by default: requires GStreamer + `gst-rtsp-server` plugins + a free
/// port.
#[test]
#[ignore = "requires the GStreamer/gst-rtsp-server C stack + plugins (not in CI); run with --ignored on a gst-equipped runner"]
fn rtsp_server_binds_and_feeds_from_sink() {
    let mount = RtspMount::new("program").unwrap();
    let config = RtspServerConfig {
        // IPv6-first: bind the IPv6 loopback (bracketed in the served URL).
        host: "[::1]".to_owned(),
        port: 8554,
        mount,
        codec: RtspCodec::H264,
        // 90 kHz RTP-friendly timebase: pts/duration are in 1/90000 s units.
        timebase: (1, 90000),
    };
    let expected_url = config.served_url();

    let queue: Arc<BoundedPacketQueue> = Arc::new(BoundedPacketQueue::new(8));
    let sink = RtspServerSink::with_queue("rtsp-program", Arc::clone(&queue));

    let handle =
        RtspServerHandle::start_with_queue(config, queue).expect("rtsp server should start");
    assert_eq!(handle.served_url(), expected_url);

    // The engine routes encoded packets to the sink; the serving side drains the
    // shared queue. A real run feeds tick-stamped NALs; here we prove the seam is
    // wired (deliver enqueues into the queue the server pops from).
    let packet = Arc::new(EncodedPacket {
        rendition: RenditionId::new("program"),
        kind: PacketKind::VideoKeyframe,
        pts: 0,
        dts: 0,
        duration: 3000,
        data: Arc::from(vec![0u8, 0, 0, 1, 0x67].into_boxed_slice()),
    });
    sink.deliver(&packet);
    assert_eq!(sink.queued(), 1);

    // Live client pull (ffprobe rtsp://… + a second simultaneous client +
    // slow-client isolation) is the GStreamer-runner acceptance — deferred here.
    drop(handle);
}
