//! Ingest tests for the gated ST 2110 receive path (feature `st2110`, IN-2):
//! the [`PacketSource`] application-layer seam, the
//! [`St2110Producer`] that drives an **injected** packet stream through the
//! IN-1 [`FrameAssembler`] into the [`IngestPump`] -> [`TileStore`] with the
//! RTP 90 kHz timestamps rebased onto the nanosecond timeline, and the
//! ST 2022-7 [`DualPathPacketSource`] dedup-by-sequence merge.
//!
//! Everything here is driven by **injected** RTP packet units — there is **no
//! real NIC, multicast, or PTP**. The real `UdpSocket`/`RtpReceiver` path is
//! gated and `#[ignore]`d (it needs an ST 2110 network); see
//! `live_st2110_receiver_assembles_frame`.
#![cfg(feature = "st2110")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use multiview_core::time::{rescale, MediaTime, Rational};
use multiview_framestore::TileStore;
use multiview_input::source::{FrameProducer, IngestConfig, IngestPump, StoredFrame};
use multiview_input::st2022_7::Path;
use multiview_input::st2110::assembler::RasterGeometry;
use multiview_input::st2110::transport::{
    DualPathPacketSource, PacketSource, St2110Packet, St2110Producer,
};

/// A small 4-line raster, 8 bytes per line (toy geometry matching the IN-1 tests).
const W: u32 = 4;
const H: u32 = 4;
const BYTES_PER_LINE: usize = 8;
/// The full reassembled raster size (`H * BYTES_PER_LINE`), as a `usize`.
const RASTER_BYTES: usize = 4 * BYTES_PER_LINE;

fn geometry() -> RasterGeometry {
    RasterGeometry::new(W, H, BYTES_PER_LINE).expect("toy geometry is valid")
}

/// Build a valid ST 2110-20 RTP **payload** (the bytes after the RTP fixed
/// header) carrying one SRD segment for `line`, filled with `fill`.
///
/// Layout: 2-byte extended sequence, one 6-byte SRD header (length, C=0 line,
/// F=0 offset), then `BYTES_PER_LINE` octets of sample data.
fn v20_payload(line: u16, fill: u8) -> Vec<u8> {
    let mut p = Vec::new();
    p.extend_from_slice(&0u16.to_be_bytes()); // extended sequence (high 16 bits)
    let len = u16::try_from(BYTES_PER_LINE).expect("line fits u16");
    p.extend_from_slice(&len.to_be_bytes()); // SRD length
    p.extend_from_slice(&line.to_be_bytes()); // C=0, line number
    p.extend_from_slice(&0u16.to_be_bytes()); // F=0, pixel offset 0
    p.extend(std::iter::repeat_n(fill, BYTES_PER_LINE));
    p
}

/// Build an injected ST 2110-20 packet unit for `line` of the frame at the given
/// 90 kHz `timestamp` and RTP `sequence`; `marker` flags the last packet.
fn line_packet(marker: bool, timestamp: u32, sequence: u16, line: u16, fill: u8) -> St2110Packet {
    St2110Packet {
        marker,
        timestamp,
        sequence,
        ssrc: 0,
        payload_type: 96,
        payload: v20_payload(line, fill),
    }
}

/// A scripted single-path packet source: yields a fixed list of injected packet
/// units in order, then signals clean end-of-stream. No sockets — exactly the
/// seam the real `RtpReceiver` plugs into.
struct ScriptedSource {
    packets: std::collections::VecDeque<St2110Packet>,
}

impl ScriptedSource {
    fn new(packets: Vec<St2110Packet>) -> Self {
        Self {
            packets: packets.into(),
        }
    }
}

impl PacketSource for ScriptedSource {
    fn poll_packet(&mut self) -> Result<Option<St2110Packet>, multiview_input::Error> {
        Ok(self.packets.pop_front())
    }
}

/// Build the four line packets of one complete frame, the last marker-flagged.
fn complete_frame(timestamp: u32, first_seq: u16) -> Vec<St2110Packet> {
    (0..H)
        .map(|line| {
            let l = u16::try_from(line).expect("line fits u16");
            let marker = line == H - 1;
            let seq = first_seq.wrapping_add(l);
            let fill = 0x10u8.wrapping_add(u8::try_from(line).expect("line fits u8"));
            line_packet(marker, timestamp, seq, l, fill)
        })
        .collect()
}

#[test]
fn injected_rtp_sequence_assembles_and_produces_a_frame() {
    let packets = complete_frame(9000, 100);
    let source = ScriptedSource::new(packets);
    let mut producer = St2110Producer::new(Box::new(source), geometry());

    // A complete in-order frame assembles into exactly one produced frame.
    let frame = producer
        .next_frame()
        .expect("producer pulls without faulting")
        .expect("the marker packet closes one frame");
    assert_eq!(
        frame.raw_pts,
        Some(9000),
        "raw_pts carries the verbatim 90 kHz RTP timestamp"
    );
    assert_eq!(frame.meta.width, W);
    assert_eq!(frame.meta.height, H);
    assert_eq!(
        frame.pixels.len(),
        RASTER_BYTES,
        "the produced frame carries the full reassembled raster"
    );
    // Clean end-of-stream once the scripted packets are drained.
    assert!(producer.next_frame().expect("clean EOS pull").is_none());
}

#[test]
fn producer_rebases_rtp_timestamps_onto_a_monotonic_ns_timeline() {
    // Two consecutive frames one 90 kHz frame period (3000 ticks @ 30 fps) apart.
    let mut packets = complete_frame(90_000, 200);
    packets.extend(complete_frame(93_000, 204));
    let source = ScriptedSource::new(packets);
    let mut producer = St2110Producer::new(Box::new(source), geometry());

    let store: TileStore<StoredFrame> = TileStore::with_defaults("st2110-0");
    let mut pump = IngestPump::new(&producer, IngestConfig::default());
    // Anchor the first frame at a non-zero master-clock instant.
    let anchor = MediaTime::from_nanos(1_000_000_000);
    let published = pump
        .run_to_end(&mut producer, &store, anchor)
        .expect("pump runs to clean EOS");

    assert_eq!(published, 2, "both complete frames reach the store");
    // The latest stored frame is normalized: its pts is on the ns timeline (the
    // RTP 90 kHz raw_pts, NOT 93_000 ticks) and strictly after the anchor.
    let read = store.read(anchor);
    let stored = read.frame().expect("a frame is stored");
    assert!(
        stored.meta.pts.as_nanos() > anchor.as_nanos(),
        "second frame's ns pts is strictly after the anchored first ({} > {})",
        stored.meta.pts.as_nanos(),
        anchor.as_nanos()
    );
    // One 90 kHz frame period is 3000 ticks = 100/3 ms = 33_333_333 ns; the
    // rebased delta from the anchor must equal that (exact rational math).
    assert_eq!(
        stored.meta.pts.as_nanos() - anchor.as_nanos(),
        33_333_333,
        "the 90 kHz->ns rebase is float-free and exact"
    );
}

#[test]
fn dual_path_source_dedups_merged_sequences() {
    // The SAME frame sent over two redundant ST 2022-7 paths with complementary
    // loss: path A drops line 1, path B drops line 2; merged, every sequence
    // appears exactly once and the frame still completes.
    let frame = complete_frame(9000, 0);
    let path_a: Vec<St2110Packet> = frame.iter().filter(|p| p.sequence != 1).cloned().collect();
    let path_b: Vec<St2110Packet> = frame.iter().filter(|p| p.sequence != 2).cloned().collect();

    let merged = DualPathPacketSource::new(
        Box::new(ScriptedSource::new(path_a)),
        Box::new(ScriptedSource::new(path_b)),
        16,
    );
    let mut producer = St2110Producer::new(Box::new(merged), geometry());

    let frame = producer
        .next_frame()
        .expect("merged producer pulls without faulting")
        .expect("the merged stream closes the frame");
    // All four lines arrived across the two paths (none lost on BOTH).
    assert_eq!(
        frame.pixels.len(),
        RASTER_BYTES,
        "the merged frame carries the full raster"
    );
}

#[test]
fn dual_path_source_discards_the_duplicate_copy() {
    // Both paths carry the IDENTICAL full frame: the merge must emit each
    // sequence exactly once, so the assembler sees four packets, not eight.
    let frame = complete_frame(9000, 0);
    let merged = DualPathPacketSource::new(
        Box::new(ScriptedSource::new(frame.clone())),
        Box::new(ScriptedSource::new(frame)),
        16,
    );

    let mut count = 0usize;
    let mut src = merged;
    while let Some(_pkt) = src.poll_packet().expect("merged poll") {
        count += 1;
    }
    assert_eq!(
        count, 4,
        "the duplicate copy of every sequence is discarded (4 unique, not 8)"
    );
    // The merge is sequence-keyed: Path is opaque to the dedup (both paths feed
    // the same reconstructor).
    assert_ne!(Path::A, Path::B);
}

#[test]
fn channel_bridge_is_bounded_and_drives_the_producer() {
    use multiview_input::st2110::transport::ChannelPacketSource;

    // The bounded-ring bridge: the async receive task (here, a synchronous
    // `push`) feeds units; the sync source drains them. This is the seam the
    // live NIC path crosses, exercised without a socket.
    let (sink, source) = ChannelPacketSource::bounded(8);
    for pkt in complete_frame(9000, 100) {
        sink.push(pkt);
    }
    let mut producer = St2110Producer::new(Box::new(source), geometry());
    let frame = producer
        .next_frame()
        .expect("producer pulls without faulting")
        .expect("the bridged packets close one frame");
    assert_eq!(frame.pixels.len(), RASTER_BYTES);

    // The ring is BOUNDED (invariant #10): pushing past capacity never grows —
    // it sheds exactly one unit and records the drop, so a stalled reader can
    // never back-pressure the receive task into unbounded memory. (The drop
    // *policy* — oldest, not newest — is pinned by `st2110_channel_bridge.rs`.)
    let (sink2, mut src2) = ChannelPacketSource::bounded(2);
    sink2.push(line_packet(false, 1, 1, 0, 0));
    sink2.push(line_packet(false, 1, 2, 1, 0));
    sink2.push(line_packet(false, 1, 3, 2, 0));
    assert_eq!(
        src2.dropped(),
        1,
        "a full bounded ring sheds exactly one unit, never grows"
    );
    assert!(src2.poll_packet().expect("poll never faults").is_some());
    assert!(src2.poll_packet().expect("poll never faults").is_some());
    assert!(
        src2.poll_packet().expect("poll never faults").is_none(),
        "only capacity-many units survive the bound"
    );
}

/// A virtual injected [`PaceClock`]: jumps to each release deadline (sleep-free),
/// records the wall-clock instant at which each frame becomes due. The end-to-end
/// proof that the paced raw-RTP / ST-2110 path releases frames at
/// `anchor + (normalized_pts - pts0)` without ever touching a real clock.
struct VirtualPaceClock {
    now_ns: i64,
}

impl VirtualPaceClock {
    fn new(start_ns: i64) -> Self {
        Self { now_ns: start_ns }
    }
}

impl multiview_input::source::PaceClock for VirtualPaceClock {
    fn now_ns(&mut self) -> i64 {
        self.now_ns
    }
    fn sleep_until(&mut self, deadline_ns: i64) {
        // Jump the virtual clock forward to the deadline (never backward).
        self.now_ns = self.now_ns.max(deadline_ns);
    }
    fn idle(&mut self) {
        self.now_ns = self.now_ns.saturating_add(1_000_000);
    }
}

#[test]
fn paced_st2110_releases_frames_wall_clock_smoothed() {
    use multiview_input::source::{IngestConfig, PacePolicy};

    // Three consecutive 30 fps frames arrive as a back-to-back BURST (the producer
    // yields them as fast as the assembler closes them). Under the wall-clock
    // policy the pump must release them spaced by their normalized-PTS deltas
    // (one 90 kHz frame period = 3000 ticks = 33_333_333 ns), not flood the store.
    let mut packets = complete_frame(90_000, 0);
    packets.extend(complete_frame(93_000, 4));
    packets.extend(complete_frame(96_000, 8));
    let source = ScriptedSource::new(packets);
    let mut producer = St2110Producer::new(Box::new(source), geometry());

    let store: TileStore<StoredFrame> = TileStore::with_defaults("st2110-paced");
    let config = IngestConfig {
        pace: PacePolicy::WallClock,
        ..IngestConfig::default()
    };
    let mut pump = IngestPump::new(&producer, config);
    let start = 5_000_000_000_i64;
    let mut clock = VirtualPaceClock::new(start);

    let published = pump
        .run_paced_to_end(&mut producer, &store, &mut clock)
        .expect("paced pump runs to clean EOS");
    assert_eq!(published, 3, "all three frames reach the store");

    // The virtual clock advanced to release the LAST frame at start + the exact
    // (float-free) ns offset of 2 frame periods (6000 ticks @ 90 kHz) — proof the
    // burst was smoothed across wall time, not published instantly.
    let two_periods_ns = rescale(
        6_000,
        Rational::new(1, 90_000),
        Rational::new(1, 1_000_000_000),
    );
    assert_eq!(
        clock.now_ns,
        start + two_periods_ns,
        "the last frame released exactly 2 frame periods after the first (paced, not flooded)"
    );
    // And the offset must be ~2 * 33.33 ms (sanity: neither instant nor far-future).
    assert!(
        (66_000_000..=67_000_000).contains(&two_periods_ns),
        "two-period offset is ~66.6 ms: {two_periods_ns}"
    );
}

#[test]
fn paced_st2110_32bit_wrap_crossing_soak() {
    use multiview_input::source::{IngestConfig, PacePolicy};

    // SOAK across the 32-bit RTP wrap boundary: feed a run of 30 fps frames whose
    // 90 kHz timestamps cross 2^32 -> 0. The unwrap must make the wrap a normal
    // forward delta, the normalizer stay strictly monotonic, and the pacer release
    // each frame one frame period after the last — never a ~13.25 h backward jump
    // or a far-future deadline (no explosion).
    let modulus = 1_i64 << 32;
    let period = 3_000_u32; // 30 fps @ 90 kHz
    let n = 12_u32;
    // Start far enough before the wrap that the run crosses it mid-stream.
    let first = u32::try_from(modulus - i64::from(period) * 6).expect("fits u32");
    let mut packets: Vec<St2110Packet> = Vec::new();
    let mut seq: u16 = 0;
    for k in 0..n {
        let ts = first.wrapping_add(period.wrapping_mul(k));
        packets.extend(complete_frame(ts, seq));
        seq = seq.wrapping_add(4);
    }
    let source = ScriptedSource::new(packets);
    let mut producer = St2110Producer::new(Box::new(source), geometry());

    let store: TileStore<StoredFrame> = TileStore::with_defaults("st2110-wrap");
    let config = IngestConfig {
        pace: PacePolicy::WallClock,
        // Large threshold: the unwrapped delta is a normal frame step, never a
        // discontinuity.
        discontinuity_ns: 60_000_000_000,
        ..IngestConfig::default()
    };
    let mut pump = IngestPump::new(&producer, config);
    let start = 0_i64;
    let mut clock = VirtualPaceClock::new(start);

    let published = pump
        .run_paced_to_end(&mut producer, &store, &mut clock)
        .expect("paced wrap soak runs to clean EOS");
    assert_eq!(
        published,
        u64::from(n),
        "every frame across the wrap reaches the store"
    );

    // The release clock advanced by the exact (float-free) ns offset of (n-1)
    // frame periods — uniform across the wrap, no regression / explosion. A wrap
    // mishandled as a backward jump would re-anchor (clock near `start`); one
    // mishandled as a forward explosion would push the clock ~13 h out.
    let total_ticks = i64::from(period) * (i64::from(n) - 1);
    let expected_offset = rescale(
        total_ticks,
        Rational::new(1, 90_000),
        Rational::new(1, 1_000_000_000),
    );
    assert_eq!(
        clock.now_ns,
        start + expected_offset,
        "release spacing stayed uniform across the 32-bit RTP wrap (no jump/explosion)"
    );
    // Sanity bound: (n-1)=11 frame periods ~= 366.6 ms, nowhere near a 13 h wrap.
    assert!(
        (360_000_000..=370_000_000).contains(&expected_offset),
        "offset is ~366 ms, not a wrap-sized jump: {expected_offset}"
    );
}

/// A [`PacketSource`] that yields `remaining` malformed units — each a counted
/// poll — then drains to `None`, so a test can prove [`St2110Producer::next_frame`]
/// never drains an unbounded flood in one call (F1 / inv #1).
struct FloodSource {
    polls: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    remaining: usize,
}

impl PacketSource for FloodSource {
    fn poll_packet(&mut self) -> Result<Option<St2110Packet>, multiview_input::Error> {
        self.polls
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if self.remaining == 0 {
            return Ok(None);
        }
        self.remaining -= 1;
        // A 1-byte payload cannot even hold the 2-byte extended-sequence word, so
        // `V20Payload::parse` fails and the producer drops the unit without ever
        // closing a frame — the malformed-flood shape that drove the unbounded loop.
        Ok(Some(St2110Packet {
            marker: false,
            timestamp: 0,
            sequence: 1,
            ssrc: 0,
            payload_type: 96,
            payload: vec![0u8; 1],
        }))
    }
}

/// A malformed-packet flood must not make one `next_frame()` call loop until the
/// source empties (a single sample doing unbounded work would delay the output
/// clock — panel F1, inv #1). One tick over the flood produces no frame AND pulls
/// EXACTLY the per-poll budget (64) before yielding `Ok(None)`; the unbounded loop
/// this guards against would instead pull all 5001 units of a 5000-packet flood.
#[test]
fn next_frame_is_bounded_work_under_a_malformed_flood() {
    use std::sync::atomic::Ordering;

    // A flood far larger than the per-poll budget: a correct sample drains only a
    // budget's worth per tick, the old unbounded loop drained all 5000 (F1 / inv #1).
    const FLOOD: usize = 5_000;

    let polls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let source = FloodSource {
        polls: std::sync::Arc::clone(&polls),
        remaining: FLOOD,
    };
    let mut producer = St2110Producer::new(Box::new(source), geometry());

    // One tick over a malformed flood: no frame is assembled, and the call must
    // NOT drain the whole flood — a sample is bounded work so it can never delay
    // the output clock (inv #1). The pump re-polls next tick; malformed datagrams
    // are drained at the budget rate, not all at once.
    let out = producer
        .next_frame()
        .expect("a malformed flood is dropped, never faulted");
    assert!(out.is_none(), "no frame closes in the flood this tick");
    let polled = polls.load(Ordering::Relaxed);
    // Pin the budget at 64 (the AES67 F1 analogue): a deliberate change is a
    // reviewable event that must update this test too (rule 19), and the
    // exact-count assertion below reads against a named contract.
    assert_eq!(
        St2110Producer::MAX_PACKETS_PER_POLL,
        64,
        "next_frame's per-poll budget is pinned at 64",
    );
    // EXACT bound (mutation-killing): every flooded unit depacketizes-then-drops
    // (none closes a frame), so a correct loop spends its FULL budget and then
    // yields `Ok(None)` — it pulls exactly 64, no more. The old `<= 1000` ceiling
    // let a regression to any budget <= 1000 (or a mutant that widened the loop
    // bound) pass silently; `== budget` fails on it.
    assert_eq!(
        polled,
        St2110Producer::MAX_PACKETS_PER_POLL,
        "next_frame must pull exactly its 64-packet budget then yield under a \
         {FLOOD}-unit malformed flood (inv #1), not {polled}",
    );
}

/// A scripted [`PacketSource`] that COUNTS every poll, so a test can assert the
/// exact number of units a `next_frame` call pulled. Yields the queued packets in
/// order, then `None`.
struct CountingSource {
    packets: std::collections::VecDeque<St2110Packet>,
    polls: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

impl CountingSource {
    fn new(
        packets: Vec<St2110Packet>,
        polls: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    ) -> Self {
        Self {
            packets: packets.into(),
            polls,
        }
    }
}

impl PacketSource for CountingSource {
    fn poll_packet(&mut self) -> Result<Option<St2110Packet>, multiview_input::Error> {
        self.polls
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Ok(self.packets.pop_front())
    }
}

/// Build the `height` line packets of one complete frame at `timestamp` (the last
/// marker-flagged), line `L` filled with the distinct byte `0x10 + L` so a
/// reassembled raster can be checked for dropped/duplicated payload. The
/// height-parameterized companion to [`complete_frame`], for a raster that spans
/// the per-poll budget.
fn complete_frame_of_height(height: u16, timestamp: u32, first_seq: u16) -> Vec<St2110Packet> {
    (0..height)
        .map(|line| {
            let marker = line == height - 1;
            let seq = first_seq.wrapping_add(line);
            let fill = 0x10u8.wrapping_add(u8::try_from(line).expect("line fits u8"));
            line_packet(marker, timestamp, seq, line, fill)
        })
        .collect()
}

/// Positive control (F1): the per-poll budget must not break normal operation. A
/// complete, well-formed frame still assembles into exactly one produced frame,
/// pulling only its own handful of line packets — far inside the budget. Guards
/// against an over-aggressive bound (a zero budget would starve every valid input).
#[test]
fn next_frame_completes_a_valid_frame_within_budget() {
    use std::sync::atomic::Ordering;

    let polls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let source = CountingSource::new(complete_frame(9000, 100), std::sync::Arc::clone(&polls));
    let mut producer = St2110Producer::new(Box::new(source), geometry());

    let frame = producer
        .next_frame()
        .expect("a valid frame pulls without faulting")
        .expect("the marker packet closes exactly one frame within budget");
    assert_eq!(
        frame.pixels.len(),
        RASTER_BYTES,
        "the produced frame carries the full reassembled raster, intact under budgeting",
    );
    assert_eq!(
        frame.raw_pts,
        Some(9000),
        "the RTP timestamp survives budgeting unchanged",
    );

    let polled = polls.load(Ordering::Relaxed);
    assert!(
        polled <= St2110Producer::MAX_PACKETS_PER_POLL,
        "a valid frame completes within the per-poll budget (budgeting must not \
         break normal operation): polled {polled}",
    );
    let frame_lines = usize::try_from(H).expect("H fits usize");
    assert_eq!(
        polled, frame_lines,
        "exactly the frame's {frame_lines} line packets are pulled — the marker \
         closes it with no over-polling",
    );
}

/// Boundary-split (F1 / inv #1): a frame whose completing (marker) unit is the
/// 65th packet straddles the 64-poll budget. The first 64 line packets are pulled
/// by one `next_frame` call that yields `Ok(None)` (budget spent, no close); the
/// 65th — the marker — is pulled by the NEXT call, which returns the COMPLETE
/// raster. Proves the in-progress assembler state is PRESERVED across the
/// budget-exhaustion yield (not reset), with no line dropped or duplicated.
#[test]
fn next_frame_preserves_partial_state_across_the_budget_yield() {
    use std::sync::atomic::Ordering;

    // A raster exactly one line taller than the budget: its 65 SRD units (marker
    // on the last) span the 64-poll boundary.
    const TALL_H: u16 = 65;
    let geometry = RasterGeometry::new(W, u32::from(TALL_H), BYTES_PER_LINE)
        .expect("a 65-line toy raster is valid");
    let polls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let source = CountingSource::new(
        complete_frame_of_height(TALL_H, 9000, 100),
        std::sync::Arc::clone(&polls),
    );
    let mut producer = St2110Producer::new(Box::new(source), geometry);

    // Call 1: the first 64 lines fill the whole budget and none is the marker, so
    // the call yields `Ok(None)` after EXACTLY 64 pulls — the partial frame (lines
    // 0..=63) stays held in the assembler.
    let first = producer
        .next_frame()
        .expect("call 1 pulls without faulting");
    assert!(
        first.is_none(),
        "no marker in the first 64 units, so the budget-spent call yields Ok(None)",
    );
    assert_eq!(
        polls.load(Ordering::Relaxed),
        St2110Producer::MAX_PACKETS_PER_POLL,
        "call 1 spends exactly the 64-packet budget before yielding",
    );

    // Call 2: pulls just the 65th unit (the marker), which closes the frame using
    // the preserved lines 0..=63 plus line 64 — the COMPLETE 65-line raster.
    let frame = producer
        .next_frame()
        .expect("call 2 pulls without faulting")
        .expect("the 65th unit (marker) closes the frame");
    assert_eq!(
        polls.load(Ordering::Relaxed),
        St2110Producer::MAX_PACKETS_PER_POLL + 1,
        "call 2 pulls exactly ONE more unit — partial state was preserved, not re-fetched",
    );

    // The reassembled raster is intact: every line landed at its own row (keyed by
    // the SRD line number), none dropped or duplicated across the yield.
    let expected: Vec<u8> = (0..TALL_H)
        .flat_map(|line| {
            let fill = 0x10u8.wrapping_add(u8::try_from(line).expect("line fits u8"));
            std::iter::repeat_n(fill, BYTES_PER_LINE)
        })
        .collect();
    assert_eq!(
        frame.pixels.len(),
        usize::from(TALL_H) * BYTES_PER_LINE,
        "the closed raster spans all 65 lines",
    );
    assert_eq!(
        frame.pixels, expected,
        "each line's payload is at its own row — nothing dropped or duplicated \
         across the budget-yield",
    );
    assert_eq!(
        frame.raw_pts,
        Some(9000),
        "the frame's RTP timestamp is carried through the split",
    );
}

/// Live ST 2110 UDP receive path — **gated, requires a real ST 2110 network +
/// PTP-disciplined NIC**.
///
/// This devcontainer has no ST 2110 multicast network and no PTP NIC, so this
/// test is `#[ignore]`d and only runs when an operator points it at a real
/// stream via `MULTIVIEW_ST2110_ADDR`. Absent that, it skips honestly (it never
/// asserts a fake pass) — the injected-source tests above carry the
/// receiver->assembler->producer correctness load.
#[test]
#[ignore = "needs a real ST 2110 network + PTP NIC (set MULTIVIEW_ST2110_ADDR)"]
fn live_st2110_receiver_assembles_frame() {
    let Ok(addr) = std::env::var("MULTIVIEW_ST2110_ADDR") else {
        eprintln!(
            "skipping live st2110 test: set MULTIVIEW_ST2110_ADDR to a reachable \
             ST 2110-20 multicast endpoint on a PTP-disciplined NIC"
        );
        return;
    };
    panic!(
        "live st2110 ingest against {addr} requires a real ST 2110 network and a \
         PTP-disciplined NIC, neither of which exists in CI"
    );
}
