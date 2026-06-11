//! CI seam tests for the in-process RTSP server (OUT-2) — the **pure-Rust**,
//! always-compiled parts that do **not** need the GStreamer/GLib C stack.
//!
//! The actual `gst-rtsp-server` pipeline + serving live behind the off-by-default
//! `rtsp-server` feature and a live, ignored-by-default playout test
//! (`rtsp_server_playout.rs`). These tests exercise the typed seam that feeds it:
//!
//! * the bounded **drop-oldest** packet queue (a slow/absent client must never
//!   stall the engine — invariants #1/#10): it drops the oldest, never grows past
//!   its capacity, and `push` never blocks;
//! * the [`RtspServerSink`] `PacketSink` hand-off: `deliver` enqueues a packet
//!   reference and returns immediately (no `.await`, no block);
//! * the [`RtspMount`] URL construction (`rtsp://host:port/mount`), panic-free.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::sync::Arc;

use multiview_output::fanout::{EncodedPacket, PacketKind, PacketSink, RenditionId};
use multiview_output::rtsp_server::{
    BoundedPacketQueue, RtspMount, RtspMountError, RtspServerSink,
};

fn packet(pts: i64, byte: u8) -> Arc<EncodedPacket> {
    Arc::new(EncodedPacket {
        rendition: RenditionId::new("program"),
        kind: if pts == 0 {
            PacketKind::VideoKeyframe
        } else {
            PacketKind::VideoDelta
        },
        pts,
        dts: pts,
        duration: 1500,
        data: Arc::from(vec![byte].into_boxed_slice()),
    })
}

// ----------------------------------------------------------------------------
// Bounded drop-oldest queue (inv #1/#10): a slow consumer sheds the OLDEST,
// never grows past capacity, and the producer never blocks.
// ----------------------------------------------------------------------------

#[test]
fn queue_drops_oldest_when_consumer_is_slow() {
    // Capacity 3, no consumer draining: pushing 5 keeps only the newest 3.
    let queue = BoundedPacketQueue::new(3);
    for pts in 0..5 {
        let dropped = queue.push(&packet(pts, 0));
        // The first 3 fit; pushes 4 and 5 each evict one oldest.
        if pts < 3 {
            assert!(!dropped, "push {pts} should fit without dropping");
        } else {
            assert!(dropped, "push {pts} should drop the oldest");
        }
    }

    // Never grew past capacity.
    assert_eq!(queue.len(), 3, "queue must never grow past its capacity");

    // The retained packets are the NEWEST three (pts 2,3,4) in FIFO order — the
    // oldest were shed, not the newest (a fresh client must get current frames).
    let drained: Vec<i64> = std::iter::from_fn(|| queue.pop().map(|p| p.pts)).collect();
    assert_eq!(drained, vec![2, 3, 4]);
    assert_eq!(queue.len(), 0);
    assert!(
        queue.pop().is_none(),
        "drained queue yields None, never blocks"
    );
}

#[test]
fn queue_push_never_blocks_under_sustained_overflow() {
    // A producer that pushes far more than capacity must return from every push
    // (drop-oldest, never block) — the output-clock analogue: the engine drives
    // route() on the tick and can never stall on a full sink buffer.
    let queue = BoundedPacketQueue::new(2);
    let mut total_dropped = 0usize;
    for pts in 0..1000 {
        if queue.push(&packet(pts, 0)) {
            total_dropped += 1;
        }
        // Invariant: bounded at all times.
        assert!(queue.len() <= 2, "queue exceeded capacity at push {pts}");
    }
    // All but the final `capacity` pushes evicted an older packet.
    assert_eq!(total_dropped, 1000 - 2);
    assert_eq!(queue.len(), 2);
}

#[test]
fn queue_capacity_zero_is_rejected() {
    // A zero-capacity queue could never hold a packet; reject it up front rather
    // than silently dropping everything.
    assert!(BoundedPacketQueue::try_new(0).is_none());
    assert!(BoundedPacketQueue::try_new(1).is_some());
}

// ----------------------------------------------------------------------------
// RtspServerSink: PacketSink — `deliver` is a non-blocking hand-off into the
// bounded queue.
// ----------------------------------------------------------------------------

#[test]
fn sink_deliver_enqueues_without_blocking() {
    let sink = RtspServerSink::new("rtsp-program", 4);
    assert_eq!(sink.sink_id(), "rtsp-program");
    assert_eq!(sink.queued(), 0);

    sink.deliver(&packet(0, 0));
    sink.deliver(&packet(1, 1));
    assert_eq!(
        sink.queued(),
        2,
        "deliver must enqueue into the bounded buffer"
    );
}

#[test]
fn sink_deliver_sheds_oldest_when_buffer_full() {
    // A slow/absent RTSP client whose buffer is full: deliver keeps shedding the
    // oldest and never blocks the caller (the engine on the output clock).
    let sink = RtspServerSink::new("slow-client", 2);
    for pts in 0..10 {
        sink.deliver(&packet(pts, 0));
        assert!(sink.queued() <= 2, "sink buffer exceeded capacity at {pts}");
    }
    assert_eq!(sink.queued(), 2);
    // The retained packets are the newest two (8, 9).
    let pts: Vec<i64> = sink.drain_for_test().iter().map(|p| p.pts).collect();
    assert_eq!(pts, vec![8, 9]);
}

#[test]
fn sink_fan_out_shares_packet_allocation() {
    // The router hands the SAME Arc to the sink; deliver must keep the reference
    // (encode-once, inv #7), not copy the payload.
    let sink = RtspServerSink::new("program", 4);
    let p = packet(0, 7);
    sink.deliver(&p);
    let drained = sink.drain_for_test();
    assert_eq!(drained.len(), 1);
    assert!(
        Arc::ptr_eq(&drained[0], &p),
        "sink should retain the same allocation, not a copy"
    );
}

// ----------------------------------------------------------------------------
// RtspMount — mount path + served URL construction, panic-free.
// ----------------------------------------------------------------------------

#[test]
fn mount_builds_served_url() {
    // IPv6-first: a bracketed IPv6 loopback host interpolates into the served URL.
    let mount = RtspMount::new("program").unwrap();
    assert_eq!(mount.path(), "/program");
    assert_eq!(mount.served_url("[::1]", 8554), "rtsp://[::1]:8554/program");
}

#[test]
fn mount_normalizes_surrounding_slashes() {
    // Leading/trailing slashes are normalized to exactly one leading slash; the
    // gst-rtsp-server mount-point path is always rooted.
    let mount = RtspMount::new("//multiview/program//").unwrap();
    assert_eq!(mount.path(), "/multiview/program");
    assert_eq!(
        mount.served_url("example.test", 5540),
        "rtsp://example.test:5540/multiview/program"
    );
}

#[test]
fn mount_rejects_empty_and_whitespace() {
    assert_eq!(RtspMount::new(""), Err(RtspMountError::Empty));
    assert_eq!(RtspMount::new("///"), Err(RtspMountError::Empty));
    // A whitespace-only mount is non-empty after slash-trimming, so it is the
    // more precise `InvalidPath` (it contains whitespace), not `Empty`.
    assert!(matches!(
        RtspMount::new("   "),
        Err(RtspMountError::InvalidPath { .. })
    ));
    assert!(matches!(
        RtspMount::new("has space"),
        Err(RtspMountError::InvalidPath { .. })
    ));
    assert!(matches!(
        RtspMount::new("ctl\u{0007}char"),
        Err(RtspMountError::InvalidPath { .. })
    ));
}
