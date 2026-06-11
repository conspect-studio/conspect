//! The ingest pipeline: producer-agnostic wiring of decode -> normalize ->
//! jitter -> last-good-frame store.
//!
//! This module is **pure-Rust** and part of the default build. It owns the
//! ingest *core*: an [`IngestPump`] that takes any [`FrameProducer`] (a real
//! libav decoder behind the `ffmpeg` feature, or a synthetic producer in tests),
//! runs every produced frame through the [`PtsNormalizer`](crate::normalize)
//! (invariant #3) and an optional [`ReorderBuffer`](crate::jitter) jitter buffer,
//! and publishes the result into a [`TileStore`] — the lock-free last-good-frame
//! slot the compositor samples on each output tick (invariant #2).
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

use crate::error::Result;
use crate::jitter::ReorderBuffer;
use crate::normalize::{PtsNormalizer, WrapBits};

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
    /// Discontinuity threshold (ns) handed to the normalizer: a raw-PTS jump
    /// larger than this re-anchors the timeline.
    pub discontinuity_ns: i64,
}

impl Default for IngestConfig {
    fn default() -> Self {
        Self {
            jitter_depth: 0,
            // Match the normalizer's own ~10 s default.
            discontinuity_ns: 10_000_000_000,
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
    /// Count of frames successfully published into the store.
    published: u64,
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
        Self {
            normalizer,
            jitter,
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

        // Reorder on the RAW timeline before normalization (the normalizer
        // enforces monotonicity by arrival order, so it must see frames in
        // display order). A frame without a raw PTS cannot be ordered: flush the
        // buffer, then pass it straight through.
        let released: Vec<ProducedFrame> = match (self.jitter.as_mut(), frame.raw_pts) {
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
        };

        for frame in released {
            self.normalize_and_publish(store, frame, master_now)?;
        }
        Ok(true)
    }

    /// Normalize one frame's raw PTS and publish it into the store at the
    /// normalized instant.
    fn normalize_and_publish(
        &mut self,
        store: &TileStore<StoredFrame>,
        frame: ProducedFrame,
        master_now: MediaTime,
    ) -> Result<()> {
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
        self.publish(store, normalized, stored);
        Ok(())
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
    /// sources use [`IngestPump::pump_one`] under the supervisor + pacer instead.
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
}

impl core::fmt::Debug for IngestPump {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("IngestPump")
            .field("published", &self.published)
            .field("jitter", &self.jitter.is_some())
            .finish_non_exhaustive()
    }
}
