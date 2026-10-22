//! The ingest pipeline: producer-agnostic wiring of decode -> normalize ->
//! jitter -> last-good-frame store.
//!
//! This module is **pure-Rust** and part of the default build. It owns the
//! ingest *core*: an [`IngestPump`] that takes any [`FrameProducer`] (a real
//! libav decoder behind the `ffmpeg` feature, or a synthetic producer in tests),
//! runs every produced frame through the [`PtsNormalizer`](crate::normalize)
//! (invariant #3), an optional [`ReorderBuffer`](crate::jitter) jitter buffer,
//! and — for live raw-RTP / ST-2110 sources — an optional wall-clock
//! [`Pacer`](crate::pacer) ([`PacePolicy::WallClock`], ADR-0021 point 3) that
//! smooths bursty / jittery arrival *into the store*, then publishes the result
//! into a [`TileStore`] — the lock-free last-good-frame slot the compositor
//! samples on each output tick (invariant #2).
//!
//! ## Pacing is ingest-side only (invariant #1)
//! The pacer sits **behind** the reorder buffer and feeds on the *normalized*
//! PTS; it gates *when a frame enters the store*, never the output tick. The
//! output clock re-stamps from its own tick counter, so a paused, bursting, or
//! wrapping input never paces or stalls the program output. The pacer is
//! clock-injected ([`IngestPump::pump_one_paced`] takes `now_ns`), so the paced
//! path is deterministically testable without sleeping, and it never blocks the
//! engine — it returns a [`PaceStep`] telling the ingest task when to next poll.
//!
//! **Inputs are sampled, never pacing.** The pump only ever *writes* into the
//! store; it never blocks on a reader and never back-pressures the engine. A
//! bursting producer overwrites the single slot (newest wins) and a slow one is
//! held — bounded memory either way.
//!
//! ## Why a producer trait
//! The actual demux/decode lives behind the off-by-default `ffmpeg` feature and
//! is owned by `multiview-ffmpeg` (the only crate allowed raw FFI). By depending on
//! a small [`FrameProducer`] trait here, the timing/resilience logic — the part
//! that must be exhaustively tested — stays pure-Rust and testable with a
//! synthetic producer, while the libav adapter (the `libav` module, behind the
//! `ffmpeg` feature) merely implements the trait.

use multiview_core::color::ColorInfo;
use multiview_core::frame::FrameMeta;
use multiview_core::pixel::PixelFormat;
use multiview_core::time::{MediaTime, Rational};
use multiview_framestore::TileStore;

use std::collections::VecDeque;

use crate::error::Result;
use crate::jitter::ReorderBuffer;
use crate::normalize::{PtsNormalizer, WrapBits};
use crate::pacer::{Pacer, PacerConfig, Release};

/// One decoded frame handed from a [`FrameProducer`] to the [`IngestPump`].
///
/// Carries the decoded pixels as an owned host buffer plus the pure-Rust
/// [`FrameMeta`] describing them. The `pts` inside `meta` at this stage is still
/// the **raw input** presentation timestamp (the producer's own timeline); the
/// pump replaces it with the normalized, strictly-monotonic internal-timeline
/// instant before publishing (invariants #1/#3).
///
/// `raw_pts` is the producer's raw timestamp in *its declared timebase ticks*,
/// or `None` when the source did not provide one (`AV_NOPTS_VALUE`), in which
/// case the normalizer synthesizes a value from the declared cadence (genpts
/// fallback).
#[derive(Debug, Clone)]
pub struct ProducedFrame {
    /// The decoded pixels (NV12 / P010 host bytes), if the producer carries
    /// them. Synthetic producers may leave this empty; the timing pipeline does
    /// not inspect the bytes.
    pub pixels: Vec<u8>,
    /// Raw presentation timestamp in the producer's timebase ticks, or `None`.
    pub raw_pts: Option<i64>,
    /// Whether this frame begins a new timeline segment (an `EXT-X-DISCONTINUITY`
    /// tag, a TS discontinuity indicator, or a producer-detected reset). The pump
    /// re-anchors the normalizer on it (invariant #3).
    pub discontinuity: bool,
    /// Geometry, pixel format, and color of the decoded frame. Its `pts` field
    /// is ignored here (the pump overwrites it with the normalized instant).
    pub meta: FrameMeta,
}

impl ProducedFrame {
    /// Construct a produced frame with no pixel payload (synthetic / timing-only).
    #[must_use]
    pub fn timing_only(raw_pts: Option<i64>, width: u32, height: u32) -> Self {
        Self {
            pixels: Vec::new(),
            raw_pts,
            discontinuity: false,
            meta: FrameMeta {
                pts: MediaTime::ZERO,
                width,
                height,
                format: PixelFormat::Nv12,
                color: ColorInfo::default(),
            },
        }
    }

    /// Mark this frame as the start of a new timeline segment.
    #[must_use]
    pub const fn with_discontinuity(mut self) -> Self {
        self.discontinuity = true;
        self
    }
}

/// The payload published into the [`TileStore`]: a decoded frame plus its
/// normalized, internal-timeline metadata.
///
/// This is what the compositor would sample on an output tick. The `meta.pts` is
/// the normalized, strictly-monotonic nanosecond instant (NOT the raw input
/// timestamp).
#[derive(Debug, Clone)]
pub struct StoredFrame {
    /// The decoded pixels (may be empty for timing-only synthetic producers).
    pub pixels: Vec<u8>,
    /// Normalized metadata: `meta.pts` is the internal-timeline instant.
    pub meta: FrameMeta,
}

/// A source of decoded frames the [`IngestPump`] pulls from.
///
/// Implementations decode independently of the output clock (invariant #1):
/// they are *sampled*, never pacing. The libav adapter (the `libav` module,
/// behind the `ffmpeg` feature) implements this over `multiview-ffmpeg`'s safe
/// demux/decode wrappers; tests implement it synthetically.
pub trait FrameProducer {
    /// Pull the next decoded frame.
    ///
    /// Returns `Ok(Some(frame))` for a frame, `Ok(None)` at clean end-of-stream,
    /// or an [`Error`](crate::Error) for a fault the supervisor should react to (reconnect).
    ///
    /// # Errors
    /// Returns an [`Error`](crate::Error) when the underlying producer faults (e.g. a demux /
    /// decode error). The caller treats this as a connection fault and applies
    /// the supervised-reconnect backoff rather than crashing the engine.
    fn next_frame(&mut self) -> Result<Option<ProducedFrame>>;

    /// The producer's declared input timebase (seconds per raw tick), used to
    /// rebase raw timestamps onto the internal nanosecond timeline.
    fn timebase(&self) -> Rational;

    /// The producer's declared cadence (fps) for the genpts fallback when a
    /// frame has no PTS.
    fn cadence(&self) -> Rational;

    /// The timestamp wrap width of the producer's stream (33-bit MPEG-TS, 32-bit
    /// RTP, or none for a container with a 64-bit monotonic timestamp).
    fn wrap_bits(&self) -> WrapBits;
}

/// Ingest-side pacing policy (ADR-0021 point 3 / invariant #4).
///
/// The pacer is **ingest-side smoothing only**: it gates *when a frame enters the
/// last-good-frame store*, never the output tick (the output clock re-stamps from
/// its own tick counter — invariant #1). It exists for live raw-RTP / ST-2110
/// (and HLS) sources, where a connect burst or a jittery network would otherwise
/// flood the store; a file / VOD source is read as fast as it decodes and the
/// output clock alone paces emission.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum PacePolicy {
    /// No ingest pacing: every released frame is published the instant it is
    /// pumped. Correct for files / VOD-as-file (`-re` is *not* used) and for the
    /// latch-on-tick file path, where a producer that runs ahead is harmless (the
    /// bounded store drops oldest). This is the default.
    #[default]
    Passthrough,
    /// Pace each released frame to the wall clock by its **normalized** PTS
    /// (`release = anchor_wall + (pts - pts0)`), behind the reorder buffer. The
    /// canonical raw-RTP / ST-2110 / live-HLS ingest policy (ADR-0021 point 3):
    /// the connect burst is smoothed and per-source memory stays bounded.
    WallClock,
}

/// Tuning for the ingest pump.
///
/// A plain configuration record. Intentionally *not* `#[non_exhaustive]` so
/// callers and tests can build it directly with `..IngestConfig::default()`.
#[derive(Debug, Clone, Copy)]
pub struct IngestConfig {
    /// Reorder window depth (frames held back to absorb out-of-order arrival).
    /// `0` disables reordering (publish straight through), which is correct for
    /// sources known to deliver in display order (files, all-intra test
    /// patterns). The underlying buffer is bounded at `depth + 1` and drops,
    /// never grows: a frame older than the release watermark is rejected, and the
    /// oldest is released the instant a `depth + 1`-th frame arrives.
    pub jitter_depth: usize,
    /// Discontinuity threshold (ns) handed to the normalizer **and** the pacer: a
    /// raw-PTS jump larger than this re-anchors the timeline (and re-anchors the
    /// pacer so a discontinuity never schedules a far-future release).
    pub discontinuity_ns: i64,
    /// Ingest-side pacing policy. [`PacePolicy::Passthrough`] (the default) is the
    /// file / latch-on-tick path; [`PacePolicy::WallClock`] is the raw-RTP /
    /// ST-2110 / live path that smooths arrival via the wall-clock [`Pacer`].
    pub pace: PacePolicy,
    /// Upper bound on frames the pacer may hold *pending* (normalized but not yet
    /// due for release). Bounded so a flood can never grow memory (invariants
    /// #5 / #9): at the cap the oldest pending frame is published early to make
    /// room rather than the queue growing. Ignored under
    /// [`PacePolicy::Passthrough`].
    pub pace_pending_max: usize,
}

impl Default for IngestConfig {
    fn default() -> Self {
        Self {
            jitter_depth: 0,
            // Match the normalizer's own ~10 s default.
            discontinuity_ns: 10_000_000_000,
            pace: PacePolicy::Passthrough,
            // A few frames of pending headroom is ample for wall-clock pacing; the
            // pacer releases in PTS order so this rarely fills. Bounded regardless.
            pace_pending_max: 8,
        }
    }
}

/// Drives a [`FrameProducer`] into a [`TileStore`], normalizing timestamps and
/// (optionally) reordering frames along the way.
///
/// Construct one per input. The pump holds the per-input [`PtsNormalizer`] and
/// jitter buffer; the [`TileStore`] is shared (the compositor reads it). Pulling
/// is **non-blocking writes only** — the pump never awaits the store's reader.
pub struct IngestPump {
    normalizer: PtsNormalizer,
    /// Reorders incoming frames by their **raw** PTS *before* normalization, so
    /// the normalizer (which assumes display order and enforces a monotonic
    /// guard by arrival) sees an in-order stream. Keyed by raw PTS; frames with
    /// no raw PTS bypass it (they cannot be ordered).
    jitter: Option<ReorderBuffer<ProducedFrame>>,
    /// Wall-clock ingest pacer, present only under [`PacePolicy::WallClock`]. It
    /// sits **behind** the reorder buffer and feeds on the *normalized* PTS
    /// (ADR-0021 point 3), gating *when* a frame enters the store — never the
    /// output tick (invariant #1).
    pacer: Option<Pacer>,
    /// Frames normalized but not yet due for release, ordered by release deadline
    /// (which equals normalized-PTS order). **Bounded** by `pace_pending_max`:
    /// drops never grow — at the cap the oldest is published early to make room
    /// (invariants #5 / #9).
    pending: VecDeque<PendingFrame>,
    /// The bound on `pending` under wall-clock pacing.
    pending_max: usize,
    /// Count of frames successfully published into the store.
    published: u64,
}

/// A frame normalized and held by the pacer until its wall-clock release deadline.
#[derive(Debug)]
struct PendingFrame {
    /// Wall-clock instant (ns) at which this frame becomes due for release.
    deadline_ns: i64,
    /// The normalized internal-timeline instant the frame is published at.
    at: MediaTime,
    /// The store payload (normalized metadata + pixels).
    frame: StoredFrame,
}

/// The result of one [`IngestPump::pump_one_paced`] poll on the wall-clock path.
///
/// The caller (an ingest task running off the engine thread) uses this to decide
/// when to next poll: it never blocks the engine (invariants #1 / #10). This is a
/// closed set of poll outcomes — deliberately exhaustive so a caller's `match`
/// must handle every wake case (no silent fall-through into a wildcard).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaceStep {
    /// A frame is held pending; the caller should next poll at or after this
    /// wall-clock instant (ns), when the earliest pending frame becomes due.
    WakeAt(i64),
    /// Nothing is pending and the producer yielded no frame this poll (it had
    /// nothing ready); the caller should re-poll on its own cadence.
    Pending,
    /// The producer reached clean end-of-stream and the pacer is drained.
    Eos,
}

impl IngestPump {
    /// Build a pump for a producer with the given configuration.
    #[must_use]
    pub fn new<P: FrameProducer + ?Sized>(producer: &P, config: IngestConfig) -> Self {
        let normalizer = PtsNormalizer::new(
            producer.wrap_bits(),
            producer.timebase(),
            producer.cadence(),
        )
        .with_discontinuity_ns(config.discontinuity_ns);
        let jitter = if config.jitter_depth > 0 {
            // Hold up to `depth` frames; capacity `depth + 1` so a just-arrived
            // frame can coexist with the window before the oldest is released.
            Some(ReorderBuffer::new(config.jitter_depth.saturating_add(1)))
        } else {
            None
        };
        let pacer = match config.pace {
            PacePolicy::WallClock => Some(Pacer::new(PacerConfig {
                discontinuity_ns: config.discontinuity_ns,
                ..PacerConfig::default()
            })),
            PacePolicy::Passthrough => None,
        };
        Self {
            normalizer,
            jitter,
            pacer,
            pending: VecDeque::new(),
            pending_max: config.pace_pending_max.max(1),
            published: 0,
        }
    }

    /// The number of frames published into the store so far.
    #[must_use]
    pub const fn published(&self) -> u64 {
        self.published
    }

    /// Pull and process exactly one frame from `producer`, publishing it into
    /// `store` (possibly via the jitter buffer).
    ///
    /// Returns `Ok(true)` if the producer yielded a frame (whether or not it was
    /// released this call — a buffered frame returns `true`), `Ok(false)` at
    /// clean end-of-stream.
    ///
    /// `master_now` is the master monotonic clock reading used only to anchor the
    /// very first frame; thereafter the timeline advances by *input* deltas.
    ///
    /// # Errors
    /// Propagates a producer fault (the supervisor reconnects) or a normalizer
    /// error (degenerate timebase).
    pub fn pump_one<P: FrameProducer + ?Sized>(
        &mut self,
        producer: &mut P,
        store: &TileStore<StoredFrame>,
        master_now: MediaTime,
    ) -> Result<bool> {
        let Some(frame) = producer.next_frame()? else {
            // Clean EOS: flush any frames still held in the jitter buffer so the
            // store ends on the freshest available frame.
            let drained = self.drain_jitter();
            for frame in drained {
                self.normalize_and_publish(store, frame, master_now)?;
            }
            return Ok(false);
        };

        let released = self.reorder_step(frame);
        for frame in released {
            self.normalize_and_publish(store, frame, master_now)?;
        }
        Ok(true)
    }

    /// Push one freshly produced frame through the reorder buffer (on its **raw**
    /// PTS, before normalization) and return whatever frames are now released in
    /// raw-PTS order.
    ///
    /// The normalizer enforces monotonicity by arrival order, so it must see
    /// frames in display order — the reorder happens here, ahead of it. A frame
    /// without a raw PTS cannot be ordered: the buffer is flushed (to keep order)
    /// and the frame passed straight through. With no jitter buffer, the frame is
    /// returned as-is. The buffer is **bounded** and drops, never grows.
    fn reorder_step(&mut self, frame: ProducedFrame) -> Vec<ProducedFrame> {
        match (self.jitter.as_mut(), frame.raw_pts) {
            (Some(buffer), Some(raw)) => {
                // Key the reorder heap by the raw PTS (ticks). Within the bounded
                // window, wrap cannot occur, so a raw-tick key orders correctly.
                let depth = buffer.capacity().saturating_sub(1);
                let _ = buffer.push(MediaTime::from_nanos(raw), frame);
                let mut out: Vec<ProducedFrame> = Vec::new();
                while buffer.len() > depth {
                    match buffer.pop() {
                        Some((_, f)) => out.push(f),
                        None => break,
                    }
                }
                out
            }
            // No jitter, or a frame with no raw PTS: flush anything buffered (to
            // keep order), then this frame.
            (maybe_buffer, _) => {
                let mut out = Self::drain_buffer(maybe_buffer);
                out.push(frame);
                out
            }
        }
    }

    /// Pull and process exactly one frame on the **wall-clock paced** raw-RTP /
    /// ST-2110 ingest path (ADR-0021 point 3, [`PacePolicy::WallClock`]).
    ///
    /// The flow is reorder (raw PTS) → normalize → **pace by normalized PTS** →
    /// publish: a frame whose wall-clock deadline has not yet arrived is held in
    /// the bounded `pending` queue and published on a later poll once `now_ns`
    /// reaches it. This smooths a connect burst or jittery arrival *into the
    /// store* — it never paces the output tick (invariant #1). Every poll first
    /// flushes any pending frames already due at `now_ns`.
    ///
    /// `now_ns` is the caller's (injected) wall-clock reading. The returned
    /// [`PaceStep`] tells the caller when to next poll; it never blocks the engine
    /// (invariants #1 / #10). Memory is bounded: the reorder buffer and the
    /// pending queue both drop, never grow (invariants #5 / #9).
    ///
    /// This method requires the pump to have been built with
    /// [`PacePolicy::WallClock`]; called on a passthrough pump it degenerates to
    /// an immediate publish (the pacer is absent) and never holds frames.
    ///
    /// The normalized timeline is anchored **source-relative** (first frame at 0);
    /// the pacer owns the wall-clock relationship, mapping each normalized PTS
    /// onto `now_ns` via `anchor_wall + (pts - pts0)`. So the stored `meta.pts` is
    /// a stable source-relative instant and `now_ns` only sets the release clock.
    ///
    /// # Errors
    /// Propagates a producer fault (the supervisor reconnects) or a normalizer
    /// error (degenerate timebase).
    pub fn pump_one_paced<P: FrameProducer + ?Sized>(
        &mut self,
        producer: &mut P,
        store: &TileStore<StoredFrame>,
        now_ns: i64,
    ) -> Result<PaceStep> {
        // (1) Release every pending frame already due at `now_ns`, in deadline
        // (== normalized-PTS) order.
        self.release_due(store, now_ns);

        // (2) Pull one frame from the producer. On the live paced path,
        // `Ok(None)` means **nothing ready this poll** (a live RTP source never
        // cleanly ends — a real end is a fault the supervisor handles), so we do
        // NOT flush: we let the clock drain the pending queue across polls.
        let Some(frame) = producer.next_frame()? else {
            // Nothing ready this poll. Release whatever is now due; if frames are
            // still pending, keep waiting on the pacer.
            self.release_due(store, now_ns);
            if let Some(next) = self.pending.front() {
                return Ok(PaceStep::WakeAt(next.deadline_ns));
            }
            // Nothing pending either: the reorder buffer may still hold frames it
            // never released (it only releases past its depth) — flush that tail
            // through pace so it is not lost, then report the wake state.
            let drained = self.drain_jitter();
            for frame in drained {
                self.normalize_and_pace(store, frame, now_ns)?;
            }
            self.release_due(store, now_ns);
            return Ok(match self.pending.front() {
                Some(next) => PaceStep::WakeAt(next.deadline_ns),
                None => PaceStep::Eos,
            });
        };

        // (3) Reorder on the raw timeline, then normalize + pace each released
        // frame.
        let released = self.reorder_step(frame);
        for frame in released {
            self.normalize_and_pace(store, frame, now_ns)?;
        }

        // (4) Release anything that became due as a result, then report when the
        // caller should next poll.
        self.release_due(store, now_ns);
        Ok(match self.pending.front() {
            Some(next) => PaceStep::WakeAt(next.deadline_ns),
            None => PaceStep::Pending,
        })
    }

    /// Normalize one frame, then either publish it immediately (no pacer) or hand
    /// it to the pacer, which decides immediate release vs. a future deadline.
    fn normalize_and_pace(
        &mut self,
        store: &TileStore<StoredFrame>,
        frame: ProducedFrame,
        now_ns: i64,
    ) -> Result<()> {
        let discontinuity = frame.discontinuity;
        // Anchor the normalized timeline source-relative (first frame at 0); the
        // pacer — not the normalizer — owns the wall-clock relationship, mapping
        // the normalized PTS onto `now_ns` (ADR-0021 point 3). This keeps the
        // stored `meta.pts` a stable source-relative instant regardless of when
        // the source connected.
        let (at, stored) = self.normalize_one(frame, MediaTime::ZERO)?;
        let Some(pacer) = self.pacer.as_mut() else {
            // No pacer (passthrough): publish immediately.
            self.publish(store, at, stored);
            return Ok(());
        };
        if discontinuity {
            pacer.mark_discontinuity();
        }
        match pacer.submit(at, now_ns) {
            Release::Now => self.publish(store, at, stored),
            Release::At(deadline_ns) => self.push_pending(store, deadline_ns, at, stored),
        }
        Ok(())
    }

    /// Enqueue a not-yet-due frame in deadline order. **Bounded**: at capacity the
    /// oldest pending frame is published early to make room (drop-oldest into the
    /// store rather than growing memory — invariants #5 / #9).
    fn push_pending(
        &mut self,
        store: &TileStore<StoredFrame>,
        deadline_ns: i64,
        at: MediaTime,
        frame: StoredFrame,
    ) {
        while self.pending.len() >= self.pending_max {
            // Publish the oldest pending frame now rather than grow unbounded. The
            // store itself is a single slot, so this is the freshest-wins drop.
            if let Some(old) = self.pending.pop_front() {
                self.publish(store, old.at, old.frame);
            } else {
                break;
            }
        }
        // Deadlines (== normalized PTS) are non-decreasing because the pacer feeds
        // on the monotonic normalized timeline, so push-back keeps order.
        self.pending.push_back(PendingFrame {
            deadline_ns,
            at,
            frame,
        });
    }

    /// Publish every pending frame whose deadline is at or before `now_ns`, in
    /// order.
    fn release_due(&mut self, store: &TileStore<StoredFrame>, now_ns: i64) {
        while let Some(front) = self.pending.front() {
            if front.deadline_ns > now_ns {
                break;
            }
            if let Some(due) = self.pending.pop_front() {
                self.publish(store, due.at, due.frame);
            } else {
                break;
            }
        }
    }

    /// Normalize one frame's raw PTS and publish it into the store at the
    /// normalized instant (the unpaced latch path).
    fn normalize_and_publish(
        &mut self,
        store: &TileStore<StoredFrame>,
        frame: ProducedFrame,
        master_now: MediaTime,
    ) -> Result<()> {
        let (normalized, stored) = self.normalize_one(frame, master_now)?;
        self.publish(store, normalized, stored);
        Ok(())
    }

    /// Normalize one frame's raw PTS into the internal timeline, returning the
    /// normalized instant and the store payload. Marks the normalizer's
    /// discontinuity flag when the frame begins a new segment. Does **not**
    /// publish or pace — the caller decides.
    fn normalize_one(
        &mut self,
        frame: ProducedFrame,
        master_now: MediaTime,
    ) -> Result<(MediaTime, StoredFrame)> {
        if frame.discontinuity {
            self.normalizer.mark_discontinuity();
        }
        let normalized = self
            .normalizer
            .normalize(frame.raw_pts, master_now.as_nanos())?;
        let mut meta = frame.meta;
        meta.pts = normalized;
        let stored = StoredFrame {
            pixels: frame.pixels,
            meta,
        };
        Ok((normalized, stored))
    }

    /// Drain every frame still held in the jitter buffer in raw-PTS order. A
    /// no-op (empty) when there is no jitter buffer.
    fn drain_jitter(&mut self) -> Vec<ProducedFrame> {
        Self::drain_buffer(self.jitter.as_mut())
    }

    /// Drain a (possibly absent) reorder buffer fully in raw-PTS order.
    fn drain_buffer(buffer: Option<&mut ReorderBuffer<ProducedFrame>>) -> Vec<ProducedFrame> {
        let Some(buffer) = buffer else {
            return Vec::new();
        };
        let mut drained: Vec<ProducedFrame> = Vec::new();
        while let Some((_, frame)) = buffer.pop() {
            drained.push(frame);
        }
        drained
    }

    /// Publish one frame into the store at instant `at`, bumping the count.
    fn publish(&mut self, store: &TileStore<StoredFrame>, at: MediaTime, frame: StoredFrame) {
        store.publish(frame, at);
        self.published = self.published.saturating_add(1);
    }

    /// Run the producer to clean end-of-stream, publishing every frame into the
    /// store. Returns the number of frames published.
    ///
    /// This is the file / VOD-as-file drive loop (no pacing — a file is read as
    /// fast as it decodes; the engine's output clock paces emission). Live
    /// raw-RTP / ST-2110 sources use [`IngestPump::pump_one_paced`] (with
    /// [`PacePolicy::WallClock`]) under the supervisor instead, so their connect
    /// burst is wall-clock-smoothed before it reaches the store.
    ///
    /// # Errors
    /// Propagates the first producer/normalizer fault encountered.
    pub fn run_to_end<P: FrameProducer + ?Sized>(
        &mut self,
        producer: &mut P,
        store: &TileStore<StoredFrame>,
        master_now: MediaTime,
    ) -> Result<u64> {
        while self.pump_one(producer, store, master_now)? {}
        Ok(self.published)
    }

    /// Run the producer to clean end-of-stream on the **wall-clock paced**
    /// raw-RTP / ST-2110 ingest path (ADR-0021 point 3), driving the injected
    /// [`PaceClock`] forward through each [`PaceStep::WakeAt`] so pending frames
    /// release at their wall-clock deadlines. Returns the number of frames
    /// published.
    ///
    /// This is the supervised live-loop body: it samples the producer, reorders +
    /// normalizes + paces each frame, and sleeps (via the clock) until the next
    /// release is due — **never** blocking the engine (the clock is the *ingest*
    /// task's wall clock; the output clock is unaffected, invariant #1). The clock
    /// seam makes it deterministic and sleep-free in tests.
    ///
    /// Requires [`PacePolicy::WallClock`]; on a passthrough pump it degenerates to
    /// an immediate publish per frame (the pacer is absent).
    ///
    /// # Errors
    /// Propagates the first producer/normalizer fault encountered.
    pub fn run_paced_to_end<P, C>(
        &mut self,
        producer: &mut P,
        store: &TileStore<StoredFrame>,
        clock: &mut C,
    ) -> Result<u64>
    where
        P: FrameProducer + ?Sized,
        C: PaceClock + ?Sized,
    {
        loop {
            let now = clock.now_ns();
            match self.pump_one_paced(producer, store, now)? {
                PaceStep::Eos => break,
                PaceStep::WakeAt(deadline) => clock.sleep_until(deadline),
                // Nothing ready and nothing pending: yield to the clock briefly so
                // the loop re-polls without spinning (a real source's next packet
                // arrives off-clock).
                PaceStep::Pending => clock.idle(),
            }
        }
        Ok(self.published)
    }
}

/// The ingest task's wall clock for the paced raw-RTP / ST-2110 drive loop.
///
/// Injected into [`IngestPump::run_paced_to_end`] so the pacing loop is
/// deterministic and sleep-free in tests (a virtual clock that jumps to each
/// deadline) and real in production (a monotonic clock + a bounded, interruptible
/// thread sleep). It is the *ingest* task's clock — it never paces or blocks the
/// output clock (invariant #1).
pub trait PaceClock {
    /// The current wall-clock reading in nanoseconds.
    fn now_ns(&mut self) -> i64;

    /// Block this ingest task until the wall clock reaches `deadline_ns` (a frame
    /// is held pending until then). Production sleeps the difference; a virtual
    /// test clock jumps. Must be interruptible/bounded in production so a stop
    /// signal is honored.
    fn sleep_until(&mut self, deadline_ns: i64);

    /// Yield briefly when nothing is ready and nothing is pending (avoid a busy
    /// spin). Production naps a few ms; a virtual test clock advances a nominal
    /// step.
    fn idle(&mut self);
}

impl core::fmt::Debug for IngestPump {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("IngestPump")
            .field("published", &self.published)
            .field("jitter", &self.jitter.is_some())
            .field("paced", &self.pacer.is_some())
            .field("pending", &self.pending.len())
            .finish_non_exhaustive()
    }
}
