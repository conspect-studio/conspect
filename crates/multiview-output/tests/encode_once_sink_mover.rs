//! RT-12 — the **encode-once-mux-many proof** (invariant #7) + the runtime
//! sink-mover (ADR-0034 / RT-12 backlog row, hard acceptance gate #6).
//!
//! The fan-out is `RenditionId → N sinks` sharing **one** encode. This test is
//! the load-bearing proof the design demanded: a call-count **spy** on
//! `encode_frame` asserts that
//!
//! * two outputs (sinks) on **one** rendition call `encode_frame` **exactly once
//!   per tick** (encode once, mux many — never per sink);
//! * moving a sink to an **already-encoding** (warm) rendition spawns **zero**
//!   new encodes (the target was already paying for its single encode);
//! * moving a sink to a **cold** rendition (no prior sinks) spawns **exactly
//!   one** new encode (the first consumer turns the encode on).
//!
//! The driver under test ([`EncodeOnceDriver`]) models exactly the engine's
//! encode-once rule: per tick, encode each rendition that currently has ≥1 sink
//! **once**, then fan the single packet to that rendition's sinks. A rendition
//! with no sinks is never encoded (admission: no consumers ⇒ no encode).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use multiview_output::fanout::{
    EncodeOnceDriver, EncodedPacket, PacketKind, PacketSink, RenditionEncoder, RenditionId,
};

/// A spy encoder: counts how many times `encode_frame` is called, per rendition
/// and in total, so a test can prove "encode once per rendition per tick".
#[derive(Debug, Default)]
struct SpyEncoder {
    total: AtomicUsize,
    per_rendition: Mutex<HashMap<String, usize>>,
}

impl SpyEncoder {
    fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    fn total_calls(&self) -> usize {
        self.total.load(Ordering::SeqCst)
    }

    fn calls_for(&self, rendition: &str) -> usize {
        self.per_rendition
            .lock()
            .unwrap()
            .get(rendition)
            .copied()
            .unwrap_or(0)
    }
}

impl RenditionEncoder for SpyEncoder {
    fn encode_frame(&self, rendition: &RenditionId, tick: u64) -> EncodedPacket {
        self.total.fetch_add(1, Ordering::SeqCst);
        *self
            .per_rendition
            .lock()
            .unwrap()
            .entry(rendition.as_str().to_owned())
            .or_insert(0) += 1;
        EncodedPacket {
            rendition: rendition.clone(),
            kind: PacketKind::VideoKeyframe,
            pts: i64::try_from(tick).unwrap_or(i64::MAX),
            dts: i64::try_from(tick).unwrap_or(i64::MAX),
            duration: 1500,
            data: Arc::from(vec![0u8, 1, 2, 3].into_boxed_slice()),
        }
    }
}

/// A recording sink that captures the `Arc<EncodedPacket>` references it gets,
/// so a test can assert pointer identity (true fan-out, not a per-sink copy).
#[derive(Debug)]
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

    fn count(&self) -> usize {
        self.received.lock().unwrap().len()
    }
}

impl PacketSink for RecordingSink {
    fn sink_id(&self) -> &str {
        &self.id
    }

    fn deliver(&self, packet: &Arc<EncodedPacket>) {
        self.received.lock().unwrap().push(Arc::clone(packet));
    }
}

/// Two outputs on ONE rendition call `encode_frame` EXACTLY ONCE per tick: the
/// canvas is encoded once and the *same* packet is fanned to both sinks
/// (invariant #7 — encode once, mux many).
#[test]
fn two_sinks_on_one_rendition_encode_exactly_once_per_tick() {
    let encoder = SpyEncoder::new();
    let mut driver = EncodeOnceDriver::new();
    let r = RenditionId::new("program");
    let rtsp = RecordingSink::new("rtsp");
    let hls = RecordingSink::new("hls");
    driver.register(r.clone(), rtsp.clone());
    driver.register(r.clone(), hls.clone());

    // Drive five ticks.
    for tick in 0..5 {
        driver.tick(tick, encoder.as_ref());
    }

    assert_eq!(
        encoder.total_calls(),
        5,
        "exactly one encode per tick for a single rendition with two sinks"
    );
    assert_eq!(encoder.calls_for("program"), 5);
    // Both sinks received every tick's packet (mux many).
    assert_eq!(rtsp.count(), 5, "rtsp sink got every tick");
    assert_eq!(hls.count(), 5, "hls sink got every tick");

    // Pointer identity: per tick, both sinks hold the SAME allocation (one encode).
    let rtsp_pkts = rtsp.received.lock().unwrap();
    let hls_pkts = hls.received.lock().unwrap();
    for (a, b) in rtsp_pkts.iter().zip(hls_pkts.iter()) {
        assert!(
            Arc::ptr_eq(a, b),
            "both sinks must share one encoded allocation per tick (encode once)"
        );
    }
}

/// Moving a sink to an ALREADY-ENCODING (warm) rendition spawns ZERO new
/// encodes: the target rendition already pays for its single encode, so adding a
/// consumer changes only the fan-out width, never the encode count.
#[test]
fn move_to_warm_rendition_spawns_zero_new_encodes() {
    let encoder = SpyEncoder::new();
    let mut driver = EncodeOnceDriver::new();
    let hd = RenditionId::new("hd");
    let sd = RenditionId::new("sd");
    let a = RecordingSink::new("a");
    let b = RecordingSink::new("b");
    // hd has one sink; sd already has one sink (so sd is WARM — already encoding).
    driver.register(hd.clone(), a.clone());
    driver.register(sd.clone(), b.clone());

    // One steady tick: both renditions encode once each ⇒ 2 encodes.
    driver.tick(0, encoder.as_ref());
    assert_eq!(
        encoder.total_calls(),
        2,
        "two warm renditions ⇒ two encodes"
    );
    let baseline = encoder.calls_for("sd");
    assert_eq!(baseline, 1);

    // Move sink `a` from hd → sd (sd is already encoding).
    assert!(driver.move_sink("a", &hd, sd.clone()), "the sink must move");

    // Next tick: sd still encodes ONCE (now feeding two sinks); hd has no sinks
    // left, so it is NOT encoded. Zero NEW encodes spawned by the warm move —
    // sd's per-tick encode count is unchanged.
    let total_before = encoder.total_calls();
    driver.tick(1, encoder.as_ref());
    assert_eq!(
        encoder.calls_for("sd"),
        baseline + 1,
        "warm rendition still encodes exactly once on the next tick (no extra)"
    );
    assert_eq!(
        encoder.total_calls() - total_before,
        1,
        "the warm move spawns ZERO new encodes — only sd (now 2 sinks) encodes; \
         hd (0 sinks) is not encoded"
    );
    // Both sinks now live under sd and share the one packet.
    assert_eq!(a.received.lock().unwrap().len(), 2);
    assert_eq!(b.received.lock().unwrap().len(), 2);
}

/// Moving a sink to a COLD rendition (one with no prior sinks, hence not being
/// encoded) spawns EXACTLY ONE new encode: the first consumer turns the encode
/// on.
#[test]
fn move_to_cold_rendition_spawns_exactly_one_new_encode() {
    let encoder = SpyEncoder::new();
    let mut driver = EncodeOnceDriver::new();
    let hd = RenditionId::new("hd");
    let cold = RenditionId::new("cold"); // never registered ⇒ not encoded
    let a = RecordingSink::new("a");
    driver.register(hd.clone(), a.clone());

    // Steady tick: only hd encodes (cold has no sinks).
    driver.tick(0, encoder.as_ref());
    assert_eq!(encoder.total_calls(), 1, "only hd encodes; cold is dark");
    assert_eq!(encoder.calls_for("cold"), 0, "cold rendition not encoded");

    // Move sink `a` from hd → cold.
    assert!(
        driver.move_sink("a", &hd, cold.clone()),
        "the sink must move"
    );

    // Next tick: cold now has a consumer ⇒ encodes EXACTLY ONCE; hd has no sinks
    // ⇒ not encoded. Exactly one new encode total for this tick.
    let total_before = encoder.total_calls();
    driver.tick(1, encoder.as_ref());
    assert_eq!(
        encoder.calls_for("cold"),
        1,
        "the cold target spawns exactly one new encode once it has a consumer"
    );
    assert_eq!(
        encoder.total_calls() - total_before,
        1,
        "exactly one encode this tick (cold), since hd is now dark"
    );
    assert_eq!(
        a.received.lock().unwrap().len(),
        2,
        "sink fed across the move"
    );
}

/// `move_sink` from an unknown source rendition (or unknown sink) is a no-op
/// returning `false` — it never errors and never spawns an encode.
#[test]
fn move_sink_unknown_is_a_noop() {
    let mut driver = EncodeOnceDriver::new();
    let r = RenditionId::new("r");
    driver.register(r.clone(), RecordingSink::new("present"));
    assert!(
        !driver.move_sink("absent", &r, RenditionId::new("target")),
        "moving an absent sink reports false"
    );
    assert!(
        !driver.move_sink(
            "present",
            &RenditionId::new("nowhere"),
            RenditionId::new("target")
        ),
        "moving from an absent source rendition reports false"
    );
}
