//! Integration tests for the encode-once-mux-many fan-out routing model
//! (invariant #7): one encoded packet stream is routed by reference to N
//! transport sinks; the *same* packet allocation reaches every registered sink.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::sync::{Arc, Mutex};

use multiview_output::fanout::{EncodedPacket, PacketKind, PacketRouter, PacketSink, RenditionId};

/// A recording sink that captures the `Arc<EncodedPacket>` references it
/// receives, so a test can assert pointer identity (true fan-out, not a copy).
#[derive(Debug, Default)]
struct RecordingSink {
    id: String,
    received: Mutex<Vec<Arc<EncodedPacket>>>,
}

impl RecordingSink {
    fn new(id: &str) -> Arc<Self> {
        Arc::new(Self {
            id: id.to_owned(),
            received: Mutex::new(Vec::new()),
        })
    }
}

impl PacketSink for RecordingSink {
    fn sink_id(&self) -> &str {
        &self.id
    }

    fn deliver(&self, packet: &Arc<EncodedPacket>) {
        self.received
            .lock()
            .expect("test mutex poisoned")
            .push(Arc::clone(packet));
    }
}

fn key_packet(rendition: RenditionId, pts: i64) -> EncodedPacket {
    EncodedPacket {
        rendition,
        kind: PacketKind::VideoKeyframe,
        pts,
        dts: pts,
        duration: 1500,
        data: Arc::from(vec![0u8, 1, 2, 3].into_boxed_slice()),
    }
}

/// Registering N sinks under one rendition and routing a packet delivers the
/// SAME `Arc<EncodedPacket>` allocation to every sink (pointer identity), so
/// "encode once, mux many" never copies the payload per transport.
#[test]
fn fan_out_delivers_same_packet_ref_to_all_sinks() {
    let rendition = RenditionId::new("program");
    let mut router = PacketRouter::new();
    let a = RecordingSink::new("rtsp");
    let b = RecordingSink::new("hls");
    let c = RecordingSink::new("srt");
    router.register(rendition.clone(), a.clone());
    router.register(rendition.clone(), b.clone());
    router.register(rendition.clone(), c.clone());

    let packet = Arc::new(key_packet(rendition.clone(), 0));
    let delivered = router.route(&packet);
    assert_eq!(delivered, 3, "all three sinks should receive the packet");

    for sink in [&a, &b, &c] {
        let got = sink.received.lock().unwrap();
        assert_eq!(got.len(), 1);
        assert!(
            Arc::ptr_eq(&got[0], &packet),
            "sink {} got a different allocation",
            sink.id
        );
    }
}

/// Packets are routed only to sinks registered under the matching rendition;
/// a packet for one rendition never leaks to another rendition's sinks.
#[test]
fn routing_is_scoped_per_rendition() {
    let hd = RenditionId::new("hd");
    let sd = RenditionId::new("sd");
    let mut router = PacketRouter::new();
    let hd_sink = RecordingSink::new("hd-out");
    let sd_sink = RecordingSink::new("sd-out");
    router.register(hd.clone(), hd_sink.clone());
    router.register(sd.clone(), sd_sink.clone());

    let p = Arc::new(key_packet(hd.clone(), 100));
    let delivered = router.route(&p);
    assert_eq!(delivered, 1);
    assert_eq!(hd_sink.received.lock().unwrap().len(), 1);
    assert_eq!(
        sd_sink.received.lock().unwrap().len(),
        0,
        "sd sink must not receive hd packet"
    );
}

/// Routing a packet whose rendition has no registered sinks delivers to zero
/// sinks and does not error (the engine must never stall on an absent sink).
#[test]
fn routing_unknown_rendition_delivers_to_none() {
    let mut router = PacketRouter::new();
    router.register(RenditionId::new("a"), RecordingSink::new("a-out"));
    let orphan = Arc::new(key_packet(RenditionId::new("ghost"), 0));
    assert_eq!(router.route(&orphan), 0);
}

/// Deregistering a sink removes it from future fan-out.
#[test]
fn deregister_removes_sink_from_fanout() {
    let r = RenditionId::new("r");
    let mut router = PacketRouter::new();
    let keep = RecordingSink::new("keep");
    let drop = RecordingSink::new("drop");
    router.register(r.clone(), keep.clone());
    router.register(r.clone(), drop.clone());
    assert!(router.deregister(&r, "drop"));

    let p = Arc::new(key_packet(r.clone(), 0));
    assert_eq!(router.route(&p), 1);
    assert_eq!(keep.received.lock().unwrap().len(), 1);
    assert_eq!(drop.received.lock().unwrap().len(), 0);
    // Deregistering an unknown sink reports false.
    assert!(!router.deregister(&r, "nope"));
}

/// The router reports how many sinks are registered per rendition (used by the
/// engine's admission logic to know whether a rendition has any consumers).
#[test]
fn sink_count_reflects_registrations() {
    let r = RenditionId::new("r");
    let mut router = PacketRouter::new();
    assert_eq!(router.sink_count(&r), 0);
    router.register(r.clone(), RecordingSink::new("one"));
    router.register(r.clone(), RecordingSink::new("two"));
    assert_eq!(router.sink_count(&r), 2);
    assert_eq!(router.rendition_count(), 1);
}
