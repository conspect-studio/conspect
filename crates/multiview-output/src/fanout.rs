//! Encode-once-mux-many fan-out routing model (invariant #7).
//!
//! Multiview composites once and **encodes the canvas once per rendition**, then
//! fans the *same* coded packets to every transport (RTSP, HLS, NDI, RTMP/SRT
//! push). A separate encode exists only when codec, resolution, or bitrate
//! differ — never per tile.
//!
//! This module is the pure-Rust **routing/registration data model**: a
//! [`PacketRouter`] maps each [`RenditionId`] to a set of registered
//! [`PacketSink`]s and, on [`PacketRouter::route`], hands every sink an
//! `Arc<EncodedPacket>` — the *same* allocation, by reference. The concrete
//! sinks (the RTSP server, the LL-HLS origin, the NDI sender, the push clients)
//! are feature-gated and implement [`PacketSink`]; they are out of scope here.
//!
//! ## Two `EncodedPacket` notions — what "encode once" means in practice
//!
//! Inv #7 is **"encode the canvas once per rendition, then fan the same coded
//! packets to N muxers."** There are two concrete carriers for "the coded
//! packet", and they are *not* the same allocation strategy:
//!
//! * This module's [`EncodedPacket`] wraps the payload in `Arc<[u8]>` and is
//!   handed out as one `Arc<EncodedPacket>` — a **single allocation shared by
//!   reference** across every sink. The [`EncodeOnceDriver`] +
//!   [`RenditionEncoder`] here drive exactly that: one
//!   [`RenditionEncoder::encode_frame`] call per rendition-with-sinks per tick,
//!   then a by-reference fan-out (see the call-count proof in
//!   `tests/encode_once_sink_mover.rs`).
//! * The **production** fan-out (the `FFmpeg` path, `multiview-cli`'s pipeline)
//!   carries a `multiview_ffmpeg::EncodedPacket` (an `AVPacket` wrapper). The
//!   *encode* still happens **once** per rendition; the per-muxer hand-off is a
//!   ref-counted `av_packet_ref` + per-sink `av_packet_make_writable` copy
//!   (`to_owned_packet`), **not** one shared `Arc<[u8]>`, because each muxer
//!   rescales timestamps in place and must not alias another sink's packet.
//!
//! So inv #7's load-bearing claim — **encode once, mux many** — holds in both:
//! the *encoder* runs once per rendition per tick regardless. The two types do
//! **not** share one allocation today, and converging them (backing the
//! production `multiview_ffmpeg::EncodedPacket` with this module's
//! `Arc<[u8]>` carrier) would require changes inside `multiview-ffmpeg`, which is
//! out of scope for RT-12. This doc therefore states the reality honestly rather
//! than overclaiming a single shared buffer for the `FFmpeg` path; the type
//! convergence is tracked as the RT-13 follow-up.
//!
//! ## Isolation (invariant #10)
//!
//! [`PacketSink::deliver`] is a synchronous, non-blocking hand-off: an
//! implementation MUST enqueue the reference into its own bounded, drop-oldest
//! buffer and return immediately. It must never block, `.await`, or
//! back-pressure the caller — the engine drives [`PacketRouter::route`] on the
//! output clock and can never be allowed to stall on a slow consumer.
use std::collections::HashMap;
use std::sync::Arc;

/// Stable identifier for an output rendition (one encode at a given
/// codec/resolution/bitrate).
///
/// Renditions are the unit of fan-out: all sinks registered under the same
/// [`RenditionId`] receive that rendition's packets.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RenditionId(String);

impl RenditionId {
    /// Construct a rendition id from any string-like value.
    #[must_use]
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// Borrow the underlying identifier string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for RenditionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// The kind of coded payload an [`EncodedPacket`] carries.
///
/// The router treats every kind identically (it fans references out
/// unconditionally); the kind is metadata for sinks that need it (e.g. a
/// segmenter that starts a new CMAF part on a keyframe).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum PacketKind {
    /// A video keyframe (IDR) — a valid segment/part boundary.
    VideoKeyframe,
    /// A non-keyframe video packet (P/B frame).
    VideoDelta,
    /// An audio packet.
    Audio,
}

impl PacketKind {
    /// Whether this packet is a keyframe (a valid segment boundary).
    #[must_use]
    pub const fn is_keyframe(self) -> bool {
        matches!(self, Self::VideoKeyframe)
    }
}

/// One encoded packet emitted by a rendition's encoder.
///
/// Carries timestamps **already re-stamped from the output tick counter**
/// (invariant #3 — raw input PTS never reaches a muxer) and the coded payload
/// as a shared, immutable byte slice. Wrapped in an [`Arc`] by the caller so a
/// single allocation fans out to many sinks without copying.
///
/// This is the **routing-model** carrier (`Arc<[u8]>`, truly shared by
/// reference). It is distinct from the production `multiview_ffmpeg::EncodedPacket`
/// (an `AVPacket` wrapper, copied per muxer via `to_owned_packet`); see the
/// module-level "Two `EncodedPacket` notions" note for the honest reconciliation
/// of what "encode once" means in each.
#[derive(Debug, Clone)]
pub struct EncodedPacket {
    /// The rendition that produced this packet.
    pub rendition: RenditionId,
    /// The payload kind.
    pub kind: PacketKind,
    /// Presentation timestamp, in the output timebase (re-stamped from the
    /// tick counter).
    pub pts: i64,
    /// Decode timestamp, in the output timebase.
    pub dts: i64,
    /// Packet duration, in the output timebase.
    pub duration: i64,
    /// Coded payload. Shared and immutable so the same bytes serve every sink.
    pub data: Arc<[u8]>,
}

/// A transport that consumes a rendition's encoded packets.
///
/// Implementors are the feature-gated servers/clients (RTSP, HLS origin, NDI,
/// RTMP/SRT). [`PacketSink::deliver`] is a **non-blocking** hand-off (see the
/// module-level isolation note).
pub trait PacketSink: Send + Sync {
    /// Stable sink identifier, unique within a rendition.
    fn sink_id(&self) -> &str;

    /// Deliver a packet reference. MUST NOT block or back-pressure the caller:
    /// enqueue the reference into a bounded drop-oldest buffer and return.
    fn deliver(&self, packet: &Arc<EncodedPacket>);
}

/// Routes a single encoded packet stream to many transport sinks, grouped by
/// rendition (the encode-once-mux-many fan-out, invariant #7).
#[derive(Default)]
pub struct PacketRouter {
    renditions: HashMap<RenditionId, Vec<Arc<dyn PacketSink>>>,
}

impl PacketRouter {
    /// Create an empty router.
    #[must_use]
    pub fn new() -> Self {
        Self {
            renditions: HashMap::new(),
        }
    }

    /// Register a sink under a rendition. If a sink with the same
    /// [`PacketSink::sink_id`] is already registered under that rendition it is
    /// replaced (re-registration is idempotent in identity).
    pub fn register(&mut self, rendition: RenditionId, sink: Arc<dyn PacketSink>) {
        let sinks = self.renditions.entry(rendition).or_default();
        let id = sink.sink_id().to_owned();
        if let Some(existing) = sinks.iter_mut().find(|s| s.sink_id() == id) {
            *existing = sink;
        } else {
            sinks.push(sink);
        }
    }

    /// Deregister the sink with `sink_id` from `rendition`. Returns `true` if a
    /// sink was removed.
    pub fn deregister(&mut self, rendition: &RenditionId, sink_id: &str) -> bool {
        let Some(sinks) = self.renditions.get_mut(rendition) else {
            return false;
        };
        let before = sinks.len();
        sinks.retain(|s| s.sink_id() != sink_id);
        let removed = sinks.len() != before;
        if sinks.is_empty() {
            self.renditions.remove(rendition);
        }
        removed
    }

    /// **Move** the sink with `sink_id` from rendition `from` to rendition `to`,
    /// re-pointing which rendition's packets that sink receives (the runtime
    /// sink-mover, ADR-0034 / RT-12). Returns `true` if the sink moved.
    ///
    /// This is a pure **routing-table** move: the same [`Arc<dyn PacketSink>`]
    /// (the sink keeps its identity, its bounded buffer, its connection) is
    /// re-keyed under `to`. It **does not encode** — encoding is the
    /// [`EncodeOnceDriver`]'s job, which encodes each rendition that has ≥1 sink
    /// **once** per tick. So if `to` is already encoding (warm) the move spawns
    /// **zero** new encodes (the target already pays for its single encode); if
    /// `to` was cold (no prior sinks) the driver will encode it once on the next
    /// tick (one new encode), and if `from` is left with no sinks it stops being
    /// encoded — encode-once-mux-many is preserved (invariant #7).
    ///
    /// Returns `false` (a no-op) when `from` has no such sink, never erroring —
    /// the engine drives this at a frame boundary and must not stall. Moving a
    /// sink onto a rendition that already holds a sink with the same id replaces
    /// it there (mirroring [`PacketRouter::register`]'s identity-idempotence).
    /// `from == to` with a present sink is a no-op that still reports `true`.
    pub fn move_sink(&mut self, sink_id: &str, from: &RenditionId, to: RenditionId) -> bool {
        // Take the sink out of `from` by identity; bail if it is not there.
        let Some(sinks) = self.renditions.get_mut(from) else {
            return false;
        };
        let Some(pos) = sinks.iter().position(|s| s.sink_id() == sink_id) else {
            return false;
        };
        let sink = sinks.swap_remove(pos);
        if sinks.is_empty() {
            self.renditions.remove(from);
        }
        // Re-register the SAME sink under `to` (identity-idempotent: replaces a
        // same-id sink already there). No encode is spawned here.
        self.register(to, sink);
        true
    }

    /// Route a packet to every sink registered under its rendition, handing
    /// each the **same** `Arc<EncodedPacket>` allocation. Returns the number of
    /// sinks the packet was delivered to (`0` if the rendition has none).
    ///
    /// This never fails and never blocks: an absent rendition or a slow sink
    /// must not stall the engine (invariants #1/#10).
    // reason: `route`'s purpose is the side effect (delivering to sinks); the
    // returned sink count is a diagnostic the engine may legitimately ignore,
    // so it must NOT be `#[must_use]`.
    #[allow(clippy::must_use_candidate)]
    pub fn route(&self, packet: &Arc<EncodedPacket>) -> usize {
        let Some(sinks) = self.renditions.get(&packet.rendition) else {
            return 0;
        };
        for sink in sinks {
            sink.deliver(packet);
        }
        sinks.len()
    }

    /// Number of sinks registered under `rendition`.
    #[must_use]
    pub fn sink_count(&self, rendition: &RenditionId) -> usize {
        self.renditions.get(rendition).map_or(0, Vec::len)
    }

    /// Number of distinct renditions with at least one registered sink.
    #[must_use]
    pub fn rendition_count(&self) -> usize {
        self.renditions.len()
    }

    /// The renditions that currently have **at least one** registered sink — the
    /// set the [`EncodeOnceDriver`] encodes (once each) per tick. A rendition
    /// with no sinks is never encoded (no consumers ⇒ no encode), so the encode
    /// budget tracks demand (invariant #7 + admission).
    #[must_use]
    pub fn active_renditions(&self) -> Vec<RenditionId> {
        self.renditions.keys().cloned().collect()
    }
}

/// Encodes one rendition's canvas frame into a coded [`EncodedPacket`].
///
/// The engine implements this over its real per-rendition encoder; the
/// [`EncodeOnceDriver`] calls [`RenditionEncoder::encode_frame`] **exactly once
/// per rendition-with-sinks per tick** — the operational definition of inv #7's
/// "encode the canvas once per rendition". The returned packet is then fanned by
/// reference to every sink under that rendition (encode once, mux many).
pub trait RenditionEncoder {
    /// Encode rendition `rendition`'s frame for output tick `tick`, returning the
    /// coded packet (timestamps already re-stamped from the tick counter, inv
    /// #3). Called at most once per rendition per tick by the driver.
    fn encode_frame(&self, rendition: &RenditionId, tick: u64) -> EncodedPacket;
}

/// Drives the **encode-once-mux-many** loop over a [`PacketRouter`] (invariant
/// #7): per output tick, encode each rendition that currently has ≥1 sink
/// **exactly once**, then fan the single packet by reference to that rendition's
/// sinks.
///
/// This is the in-crate model of the engine's output drive. It exists so the
/// encode-once contract is **provable in `multiview-output` alone**: a
/// call-count spy on [`RenditionEncoder::encode_frame`] asserts one encode per
/// rendition per tick, that a runtime [`EncodeOnceDriver::move_sink`] onto an
/// already-encoding (warm) rendition spawns **zero** new encodes, and that a
/// move onto a cold rendition spawns **exactly one** (the first consumer turns
/// the encode on). See `tests/encode_once_sink_mover.rs`.
#[derive(Default)]
pub struct EncodeOnceDriver {
    router: PacketRouter,
}

impl EncodeOnceDriver {
    /// Create an empty driver (no renditions, no sinks).
    #[must_use]
    pub fn new() -> Self {
        Self {
            router: PacketRouter::new(),
        }
    }

    /// Register `sink` under `rendition` (see [`PacketRouter::register`]). The
    /// first sink under a rendition makes that rendition *active* — it will be
    /// encoded once per tick from the next [`EncodeOnceDriver::tick`].
    pub fn register(&mut self, rendition: RenditionId, sink: Arc<dyn PacketSink>) {
        self.router.register(rendition, sink);
    }

    /// Deregister the sink with `sink_id` from `rendition` (see
    /// [`PacketRouter::deregister`]). When the last sink under a rendition is
    /// removed, that rendition stops being encoded. Returns `true` if removed.
    pub fn deregister(&mut self, rendition: &RenditionId, sink_id: &str) -> bool {
        self.router.deregister(rendition, sink_id)
    }

    /// Move the sink with `sink_id` from `from` to `to` (see
    /// [`PacketRouter::move_sink`]) — the runtime sink-mover. A move onto a warm
    /// (already-active) rendition spawns **zero** new encodes; a move onto a cold
    /// rendition makes it active (one new encode on the next tick). Returns
    /// `true` if the sink moved.
    pub fn move_sink(&mut self, sink_id: &str, from: &RenditionId, to: RenditionId) -> bool {
        self.router.move_sink(sink_id, from, to)
    }

    /// Borrow the underlying [`PacketRouter`] (for sink-count / rendition-count
    /// inspection).
    #[must_use]
    pub const fn router(&self) -> &PacketRouter {
        &self.router
    }

    /// Drive **one output tick**: encode each active rendition (≥1 sink)
    /// **exactly once** via `encoder`, then fan the single packet by reference to
    /// that rendition's sinks (encode once, mux many — invariant #7).
    ///
    /// A rendition with no sinks is **not** encoded. Returns the number of
    /// renditions encoded this tick (the per-tick encode count) — a diagnostic
    /// the caller may ignore.
    // reason: the per-tick encode count is a diagnostic; the load-bearing effect
    // is the side effect (encode + fan-out), so this is NOT `#[must_use]`.
    #[allow(clippy::must_use_candidate)]
    pub fn tick<E: RenditionEncoder + ?Sized>(&self, tick: u64, encoder: &E) -> usize {
        let active = self.router.active_renditions();
        let encode_count = active.len();
        for rendition in active {
            // EXACTLY ONE encode per active rendition this tick (inv #7).
            let packet = Arc::new(encoder.encode_frame(&rendition, tick));
            // Fan the single allocation by reference to every sink (mux many).
            self.router.route(&packet);
        }
        encode_count
    }
}
