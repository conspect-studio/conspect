//! The bounded **last-good audio store** — the audio peer of the video tile
//! store ([`multiview_framestore::TileStore`]).
//!
//! A decode thread (one per source) publishes decoded+resampled
//! [`AudioBlock`]s into an [`AudioStore`]; the output clock *samples* the store
//! per tick, pulling exactly the number of frames it needs. Audio is therefore
//! **sampled, never pacing** — exactly like the video tiles (invariant #1) — and
//! the store **never back-pressures the engine** (invariant #10): the reader is
//! wait-free and a publish never blocks waiting on a reader.
//!
//! ## Gap-free by construction (ADR-R005 §4.1)
//! Unlike video, audio is a *continuous* stream: the engine pulls a contiguous
//! run of samples each tick, so a dropout must read **silence**, never a short
//! or absent block (a gap would click/desync). [`AudioStore::read`] therefore
//! always returns exactly the requested number of frames, silence-filling any
//! region that has not been written (past EOF / before the first block) or has
//! already been evicted from the bounded ring. This is the audio analogue of
//! `anullsrc` silence-fill, load-bearing for resilience invariant A
//! (resilience-and-av §4.1).
//!
//! ## Bounded — drop-oldest, never grows
//! The ring holds at most `capacity_frames` frames of interleaved PCM. A
//! producer that runs ahead of a slow reader drops the **oldest** samples beyond
//! capacity rather than growing — the data-plane "queues drop, never grow" rule.
//! A reader that has fallen behind the surviving window reads silence for the
//! evicted span and then catches the live tail.
//!
//! ## Lock-free
//! The sample window lives behind a [`multiview_framestore::SwapCell`] snapshot —
//! the same primitive the video tile store uses. The reader loads an immutable
//! snapshot without ever blocking; the writer builds the next snapshot and swaps
//! it in, and **never waits on a reader**, even one descheduled mid-read
//! (invariants #1/#10). A load only ever returns a window published into *this*
//! store. arc-swap was dropped here for vorner/arc-swap#210, which could hand one
//! store's reader another store's window (#134, ADR-T002 addendum). There is no
//! `unsafe` (this crate is `unsafe_code = forbid`) and no mutex on the read path.
//!
//! The read cursor is a single [`AtomicI64`]: the store is a single-producer /
//! single-consumer handoff (one decode thread, one output clock), and only the
//! consumer advances the cursor, so it is never torn.

use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};

use multiview_framestore::SwapCell;

use crate::error::{AudioError, Result};
use crate::format::{AudioBlock, AudioFormat};

/// The number of preallocated snapshot buffers the [`AudioStore::publish_window`]
/// **replace** path rotates through — the audio analogue of the video tile store's
/// triple-buffer (invariant #2). Three is the classic count for a single-producer /
/// single-consumer hand-off: at any instant the `SwapCell` slot holds one and the
/// reader may hold one in-flight clone, so a third is always free for the writer to
/// fill in place (reused via `Arc::get_mut`, never reallocated on the steady path —
/// rule 22 / ADR-T019 §2.2).
const WINDOW_POOL_SIZE: usize = 3;

/// An immutable snapshot of the bounded sample window.
///
/// `base_frame` is the absolute frame index of the first frame held in
/// `samples`; the window therefore covers absolute frames
/// `[base_frame, base_frame + frames)`. Snapshots are produced only by the
/// writer and are never mutated after publication, so a reader that loads one is
/// race-free.
#[derive(Debug)]
struct RingSnapshot {
    /// Absolute frame index of the first frame in `samples`.
    base_frame: i64,
    /// Interleaved PCM for `[base_frame, base_frame + frames)`. Length is always
    /// a whole multiple of the format's channel count.
    samples: Vec<f32>,
}

impl RingSnapshot {
    /// Absolute frame index just past the last buffered frame, for the given
    /// channel count.
    fn head_frame(&self, channels: usize) -> i64 {
        let frames = i64::try_from(self.samples.len() / channels.max(1)).unwrap_or(i64::MAX);
        self.base_frame.saturating_add(frames)
    }
}

/// Copy `src` interleaved samples into `dst` starting at frame offset
/// `dst_frame` (in frames; `channels` samples per frame). Bounds-checked and
/// panic-free: a copy that would run past `dst` is clamped (never indexes out of
/// range), and a negative/oversized offset places nothing.
fn copy_into(dst: &mut [f32], src: &[f32], dst_frame: i64, channels: usize) {
    if channels == 0 || src.is_empty() {
        return;
    }
    let Ok(dst_frame) = usize::try_from(dst_frame) else {
        return; // a negative offset cannot be placed.
    };
    let dst_off = dst_frame.saturating_mul(channels);
    let Some(region) = dst.get_mut(dst_off..) else {
        return; // the offset is past the destination buffer.
    };
    let run = region.len().min(src.len());
    if let (Some(d), Some(s)) = (region.get_mut(..run), src.get(..run)) {
        d.copy_from_slice(s);
    }
}

/// Overlay `src` interleaved samples — whose first frame is absolute index
/// `src_base` — into `dst`, whose first frame is absolute index `dst_base`.
///
/// Frames of `src` below `dst_base` are **skipped** (they fall outside the
/// bounded window — drop-oldest applied *during* the copy, so the caller never
/// has to allocate space for them); frames past the end of `dst` are clamped by
/// [`copy_into`]. Panic-free and bounds-checked. Reduces to a plain
/// [`copy_into`] at `src_base - dst_base` when the whole source is at or above
/// `dst_base`.
fn overlay_from(dst: &mut [f32], src: &[f32], src_base: i64, dst_base: i64, channels: usize) {
    if channels == 0 || src.is_empty() {
        return;
    }
    // Whole frames of `src` that precede the destination base and are therefore
    // dropped. Non-negative; zero when `src_base >= dst_base`.
    let skip_frames = dst_base.saturating_sub(src_base).max(0);
    let skip_samples = usize::try_from(skip_frames)
        .unwrap_or(usize::MAX)
        .saturating_mul(channels);
    let Some(src_tail) = src.get(skip_samples..) else {
        return; // the whole source precedes the window — nothing survives.
    };
    // The first surviving source frame sits `src_base + skip_frames - dst_base`
    // frames into `dst`: `src_base - dst_base` (>= 0) when nothing was skipped, or
    // exactly 0 when `skip_frames` consumed the below-base prefix.
    let dst_frame = src_base
        .saturating_add(skip_frames)
        .saturating_sub(dst_base);
    copy_into(dst, src_tail, dst_frame, channels);
}

/// A bounded, lock-free, gap-free last-good audio store for one source.
///
/// Construct with [`AudioStore::new`]; a decode thread feeds it with
/// [`publish`](AudioStore::publish) and the engine samples it with
/// [`read`](AudioStore::read).
#[derive(Debug)]
pub struct AudioStore {
    format: AudioFormat,
    /// Maximum number of frames retained in the ring (drop-oldest beyond this).
    capacity_frames: usize,
    /// The current immutable sample window. Swapped wholesale on each publish.
    window: SwapCell<Arc<RingSnapshot>>,
    /// The consumer's absolute read cursor (next frame to be read). Only the
    /// single consumer advances it.
    read_frame: AtomicI64,
    /// Whether the producer is a live **appender** ([`publish`](AudioStore::publish)).
    /// Only then may a read re-sync a cursor stranded behind the dropped window;
    /// a window-placing producer ([`publish_window`](AudioStore::publish_window))
    /// positions content deliberately, and its gaps must stay silent.
    live_append: std::sync::atomic::AtomicBool,
    /// Frames a **live** store buffers before playing, and again after running
    /// dry (0 = not a live-buffered store: plain cursor semantics).
    prebuffer_frames: i64,
    /// Whether a live-buffered store is (re)filling and reads as silence without
    /// consuming. Starts set, so the bus cannot run ahead of a late source.
    buffering: std::sync::atomic::AtomicBool,
    /// The writer-side triple-buffer pool for the allocation-free
    /// [`AudioStore::publish_window`] **replace** path (ADR-T019 §2.2). Guarded by
    /// a `Mutex` that is **uncontended in normal use** — only the single producer
    /// thread calls `publish_window`, and the wait-free reader never touches it (it
    /// reads `window` via `SwapCell`). The lock just makes the single-writer reuse
    /// of the pooled `Arc`s sound without `unsafe`. `None` until first use (the
    /// append-only `publish`/`publish_at` paths never allocate it).
    window_pool: Mutex<WindowPool>,
}

/// The writer-side reusable snapshot pool for [`AudioStore::publish_window`].
///
/// Holds the snapshots **not currently live** in the store's `window`. On each
/// `publish_window` the writer takes a free snapshot (`Arc` strong-count 1),
/// overwrites its buffer in place, swaps it into `window`, and returns the
/// displaced window here — so the steady path performs **zero** heap allocation.
/// A fresh `Arc` is allocated only when no pooled snapshot is free (the
/// never-observed-in-SPSC contention fallback); the buffers grow to the window
/// size during the first few publishes (warm-up) and are stable thereafter.
#[derive(Debug)]
struct WindowPool {
    buffers: Vec<Arc<RingSnapshot>>,
}

impl Default for WindowPool {
    fn default() -> Self {
        // Seed `WINDOW_POOL_SIZE − 1` empty buffers: together with the one the store
        // holds live in `window`, that is `WINDOW_POOL_SIZE` snapshots in
        // circulation — enough that a free buffer is always available even while the
        // reader holds an in-flight clone (the classic triple-buffer count).
        let mut buffers = Vec::with_capacity(WINDOW_POOL_SIZE);
        for _ in 1..WINDOW_POOL_SIZE {
            buffers.push(Arc::new(RingSnapshot {
                base_frame: 0,
                samples: Vec::new(),
            }));
        }
        Self { buffers }
    }
}

impl WindowPool {
    /// Take a snapshot the writer can overwrite in place — one whose `Arc`
    /// strong-count is 1 (neither the store nor the reader references it). Prefers a
    /// free pooled buffer (reused, no allocation); allocates a fresh empty snapshot
    /// only if every pooled buffer is still referenced (never observed under the
    /// single-producer/single-consumer hand-off with three buffers).
    fn take_reusable(&mut self) -> Arc<RingSnapshot> {
        if let Some(pos) = self.buffers.iter().position(|b| Arc::strong_count(b) == 1) {
            return self.buffers.swap_remove(pos);
        }
        Arc::new(RingSnapshot {
            base_frame: 0,
            samples: Vec::new(),
        })
    }

    /// Return a displaced window to the pool for future reuse. Bounded: never holds
    /// more than [`WINDOW_POOL_SIZE`] buffers (a surplus — only possible after a
    /// fresh-allocation fallback — is dropped, keeping the resident set bounded).
    fn return_buffer(&mut self, snapshot: Arc<RingSnapshot>) {
        if self.buffers.len() < WINDOW_POOL_SIZE {
            self.buffers.push(snapshot);
        }
        // else: drop it — the pool is already at its bound (the steady set is
        // restored; this only happens transiently after a contention fallback).
    }
}

impl AudioStore {
    /// A store for a **live** source, which arrives on its own clock while the
    /// bus reads on the output clock. It buffers `prebuffer_frames` before it
    /// plays and again whenever it runs dry, and a read never moves the cursor
    /// past what was written, so audio arriving after a late connect or a stall
    /// is played rather than landing behind the cursor (silent forever).
    #[must_use]
    pub fn live(format: AudioFormat, capacity_frames: usize, prebuffer_frames: usize) -> Self {
        let mut store = Self::new(format, capacity_frames);
        store.prebuffer_frames = i64::try_from(prebuffer_frames.max(1)).unwrap_or(i64::MAX);
        store
    }

    /// Create an empty store of `format` that retains at most `capacity_frames`
    /// frames of buffered audio (drop-oldest beyond that).
    ///
    /// `capacity_frames` is clamped to at least one frame so a degenerate `0`
    /// never disables buffering entirely.
    #[must_use]
    pub fn new(format: AudioFormat, capacity_frames: usize) -> Self {
        Self {
            format,
            capacity_frames: capacity_frames.max(1),
            window: SwapCell::from_pointee(RingSnapshot {
                base_frame: 0,
                samples: Vec::new(),
            }),
            read_frame: AtomicI64::new(0),
            live_append: std::sync::atomic::AtomicBool::new(false),
            prebuffer_frames: 0,
            buffering: std::sync::atomic::AtomicBool::new(true),
            window_pool: Mutex::new(WindowPool::default()),
        }
    }

    /// The store's working format (the format every published block must match
    /// and every read block is returned in).
    #[must_use]
    pub const fn format(&self) -> AudioFormat {
        self.format
    }

    /// The number of frames currently buffered in the ring (never exceeds the
    /// configured capacity). Lock-free; primarily for tests/telemetry.
    #[must_use]
    pub fn buffered_frames(&self) -> usize {
        let channels = self.format.channel_count().max(1);
        self.window.load().samples.len() / channels
    }

    /// The **allocated capacity** (in frames) of the current window's backing
    /// buffer — the size of the last transient allocation a publish made, which
    /// [`publish_at`](AudioStore::publish_at) bounds to `capacity_frames` by
    /// applying drop-oldest *before* it allocates. This exposes the transient the
    /// live length ([`buffered_frames`](AudioStore::buffered_frames)) hides:
    /// `Vec::drain` shifts elements but never reclaims capacity, so an
    /// over-allocated union span would remain visible here even after the
    /// post-merge clamp. For the bounded-memory regression test (inv #2/#5/#9).
    #[must_use]
    #[doc(hidden)]
    pub fn window_backing_capacity_frames(&self) -> usize {
        let channels = self.format.channel_count().max(1);
        self.window.load().samples.capacity() / channels
    }

    /// The reader's current **absolute** frame position — the next absolute
    /// frame index [`read`](AudioStore::read) will return. The cursor lives in
    /// the same absolute coordinate space as the writer's `base_frame`/head, so
    /// it can be aligned to absolute tick time across a re-point (RT-8a).
    ///
    /// Lock-free; only the single consumer advances it.
    #[must_use]
    pub fn read_cursor(&self) -> i64 {
        self.read_frame.load(Ordering::Relaxed)
    }

    /// Park the read cursor at an **absolute** frame position so the next
    /// [`read`](AudioStore::read) begins there.
    ///
    /// This is the re-point alignment primitive (RT-8a): when the program bus
    /// re-points onto a warm store, seeking the cursor to absolute tick time (or
    /// to the live edge, see [`seek_to_live_edge`](AudioStore::seek_to_live_edge))
    /// makes the switch sample-aligned at the seam instead of replaying evicted
    /// history from frame 0. Only the single consumer calls this.
    pub fn seek_to(&self, frame_pos: i64) {
        self.read_frame.store(frame_pos, Ordering::Relaxed);
    }

    /// Park the read cursor at the store's **live edge** — the writer's current
    /// write head (`base_frame + buffered_frames`).
    ///
    /// On a re-point onto a warm store that has been buffering drop-oldest (its
    /// `base_frame` far ahead of frame 0), a fresh cursor would read silence
    /// climbing from frame 0 through evicted history. Seeking to the live edge
    /// makes the next read start at fresh audio, so the breakaway is
    /// sample-aligned at the seam (RT-8a, decoupled-routing §5 "seek `read_frame`
    /// to the live edge"). Only the single consumer calls this.
    pub fn seek_to_live_edge(&self) {
        let channels = self.format.channel_count();
        let head = self.window.load().head_frame(channels.max(1));
        self.read_frame.store(head, Ordering::Relaxed);
    }

    /// Append a decoded block to the ring (the decode thread's write).
    ///
    /// The block's samples are *copied* into the bounded ring, so it is taken by
    /// shared reference and the caller may keep it. Non-blocking and never grows
    /// past the configured capacity: when the ring would exceed `capacity_frames`,
    /// the **oldest** frames are dropped. The absolute write head advances by the
    /// block's frame count, so reads stay contiguous across publishes.
    ///
    /// # Errors
    ///
    /// Returns [`AudioError::FormatMismatch`] if `block`'s format differs from
    /// the store's working format.
    pub fn publish(&self, block: &AudioBlock) -> Result<()> {
        self.live_append.store(true, Ordering::Relaxed);
        if block.format() != self.format {
            return Err(AudioError::FormatMismatch {
                expected_rate: self.format.sample_rate(),
                expected_channels: self.format.channel_count(),
                actual_rate: block.format().sample_rate(),
                actual_channels: block.format().channel_count(),
            });
        }
        self.publish_samples(block.interleaved())
    }

    /// Append raw interleaved samples to the live edge — the **allocation-free
    /// hot-path** sibling of [`publish`](AudioStore::publish): the caller fills a
    /// reusable buffer (e.g. via
    /// [`LoopDeck::read_into`](multiview_audio::LoopDeck::read_into)) and publishes
    /// the slice directly, with no intermediate `AudioBlock` allocation (rule 22).
    /// `samples` must be in the store's working format (interleaved `f32`, the
    /// store's channel count); a length that is not a whole number of frames is
    /// rejected so the ring never tears mid-frame.
    ///
    /// O(window) copy-on-write like [`publish`](AudioStore::publish), on the
    /// sampled decode thread — never the output clock; the reader sees a stable
    /// snapshot.
    ///
    /// # Errors
    ///
    /// Returns [`AudioError::RaggedBlock`] if `samples.len()` is not a whole
    /// multiple of the store's channel count.
    pub fn publish_samples(&self, samples: &[f32]) -> Result<()> {
        self.live_append.store(true, Ordering::Relaxed);
        let channels = self.format.channel_count();
        if channels == 0 {
            return Ok(());
        }
        if samples.len() % channels != 0 {
            return Err(AudioError::RaggedBlock {
                samples: samples.len(),
                channels,
            });
        }
        let cap_samples = self.capacity_frames.saturating_mul(channels);

        // Build the next immutable window from the current one: append the new
        // samples, then drop the oldest beyond capacity (advancing `base_frame`
        // by however many frames were evicted). This is O(window) copy-on-write
        // by design — it runs on the *sampled decode thread*, never the output
        // clock — and gives the wait-free reader a stable snapshot.
        let current = self.window.load();
        let mut next = Vec::with_capacity((current.samples.len() + samples.len()).min(cap_samples));
        next.extend_from_slice(&current.samples);
        next.extend_from_slice(samples);

        let mut base_frame = current.base_frame;
        let overflow_samples = next.len().saturating_sub(cap_samples);
        if overflow_samples > 0 {
            // Drop whole frames only (overflow is a multiple of `channels` here
            // because both `next.len()` and `cap_samples` are).
            let evicted_frames = i64::try_from(overflow_samples / channels).unwrap_or(i64::MAX);
            next.drain(0..overflow_samples);
            base_frame = base_frame.saturating_add(evicted_frames);
        }

        self.window.store(Arc::new(RingSnapshot {
            base_frame,
            samples: next,
        }));
        Ok(())
    }

    /// Write `block` at an **absolute frame index** `at` (the RTP-audio rebase
    /// seam's store entry point — ADR-T013 §4 / ADR-0033).
    ///
    /// Unlike the append-only [`publish`](AudioStore::publish), this places the
    /// block at a caller-chosen absolute index so a reordered packet lands at its
    /// **true** frame and the rebaser's anchor maps RTP time straight onto the
    /// store's absolute coordinate. An unwritten span between writes is
    /// **silence** (gap-free by construction — the new window covers the union of
    /// the old window and the placed block, with any hole left as zeroes), and a
    /// frame older than the surviving bounded window is dropped **before the merge
    /// buffer is allocated** — so the transient allocation is bounded by the store
    /// capacity, never the (potentially far larger) reorder span (drop-oldest,
    /// never grows — invariant #2/#5/#9). The placed block **overwrites** whatever
    /// occupied its frames (last write at an index wins). A negative `at` is
    /// clamped to frame `0`.
    ///
    /// This is O(window) copy-on-write like [`publish`](AudioStore::publish) and
    /// runs on the *sampled* ingest/decode thread, never the output clock — it is
    /// non-blocking and gives the wait-free reader a stable snapshot.
    ///
    /// # Errors
    ///
    /// Returns [`AudioError::FormatMismatch`] if `block`'s format differs from the
    /// store's working format.
    pub fn publish_at(&self, at: i64, block: &AudioBlock) -> Result<()> {
        if block.format() != self.format {
            return Err(AudioError::FormatMismatch {
                expected_rate: self.format.sample_rate(),
                expected_channels: self.format.channel_count(),
                actual_rate: block.format().sample_rate(),
                actual_channels: block.format().channel_count(),
            });
        }
        let channels = self.format.channel_count();
        if channels == 0 {
            return Ok(());
        }
        let incoming = block.interleaved();
        let incoming_frames = i64::try_from(incoming.len() / channels).unwrap_or(i64::MAX);
        if incoming_frames == 0 {
            return Ok(());
        }
        let at = at.max(0);
        let block_end = at.saturating_add(incoming_frames);

        let current = self.window.load();
        let old_head = current.head_frame(channels);
        // The new window covers the union of the existing window and the placed
        // block, so neither already-buffered frames nor the new block are lost
        // and any hole between them is silence.
        let union_base = current.base_frame.min(at);
        let new_head = old_head.max(block_end);
        // Apply drop-oldest UP FRONT — clamp the working base so the span never
        // exceeds `capacity_frames` *before* allocating. Frames older than
        // `new_head - capacity_frames` are dropped either way, so allocating them
        // would only be zero-then-copy-then-drain busywork. A late/reordered packet
        // whose index lands far below the head (the RTP rebaser only re-anchors on
        // jumps past its discontinuity threshold, which is much larger than this
        // capacity) would otherwise size `merged` to the ~discontinuity-wide union
        // span — a per-packet zero+memcpy amplification vector that violates "queues
        // drop, never grow" (invariant #2/#5/#9). Clamping here bounds the transient
        // allocation to `capacity_frames`, never the union span.
        let cap_frames = i64::try_from(self.capacity_frames).unwrap_or(i64::MAX);
        let new_base = union_base.max(new_head.saturating_sub(cap_frames));
        let span_frames = usize::try_from(new_head.saturating_sub(new_base)).unwrap_or(0);
        let mut merged = vec![0.0f32; span_frames.saturating_mul(channels)];

        // Overlay the existing window first, then the new block (last write wins on
        // overlap — a re-sent packet replaces the stale one at that index).
        // `overlay_from` skips any source frames below `new_base`: that is the
        // drop-oldest, now applied during the copy instead of by a post-merge
        // `drain`, so `merged` is never larger than capacity and no evicted frame is
        // ever allocated. `merged.len() <= capacity_frames * channels` by
        // construction (`new_head - new_base <= cap_frames`), so no trailing clamp
        // is needed.
        overlay_from(
            &mut merged,
            &current.samples,
            current.base_frame,
            new_base,
            channels,
        );
        overlay_from(&mut merged, incoming, at, new_base, channels);

        self.window.store(Arc::new(RingSnapshot {
            base_frame: new_base,
            samples: merged,
        }));
        Ok(())
    }

    /// **Replace** the live window with exactly `[base_frame, base_frame + n)` —
    /// the media-player audio rail's sliding-window write (ADR-T019 §2.2/§2.3).
    ///
    /// Unlike the append-only [`publish`](AudioStore::publish), this **does not
    /// merge** with the prior window: the new window *is* `samples` placed at
    /// `base_frame`, and any earlier content is dropped. That is exactly what the
    /// loop deck's fill loop needs: each block it re-derives the whole unplayed
    /// window `[cursor, H)` from the deck's *current* transport state and replaces
    /// the store window with it, so a transport transition (arm-exit / pause / stop)
    /// overwrites any stale pre-transition tail **before the bus reads it**
    /// (boundary-tight exit, true by construction). The reader's silence-fill keeps
    /// it gap-free: a `read` outside `[base_frame, base_frame + n)` reads silence,
    /// and the fill loop always covers the bus's next pull because the window starts
    /// at the live read cursor (`LOOKAHEAD ≥` one tick).
    ///
    /// **Allocation-free on the steady path** (rule 22): the snapshot comes from a
    /// triple-buffered preallocated pool ([`WINDOW_POOL_SIZE`]); a buffer whose
    /// `Arc` is no longer shared with the reader is reused in place, a fresh `Arc`
    /// is allocated only in the never-observed SPSC-contention fallback. The
    /// writer-side `Mutex` is uncontended (single producer); the wait-free reader
    /// never touches the pool. The placed window is **bounded** to `n` frames — the
    /// caller sizes it `≤ LOOKAHEAD`, never `capacity_frames`.
    ///
    /// # Errors
    ///
    /// Returns [`AudioError::RaggedBlock`] if `samples.len()` is not a whole
    /// multiple of the store's channel count (so the ring never tears mid-frame).
    pub fn publish_window(&self, base_frame: i64, samples: &[f32]) -> Result<()> {
        self.live_append.store(false, Ordering::Relaxed);
        let channels = self.format.channel_count();
        if channels == 0 {
            return Ok(());
        }
        if samples.len() % channels != 0 {
            return Err(AudioError::RaggedBlock {
                samples: samples.len(),
                channels,
            });
        }
        // Take a reusable snapshot from the pool, overwrite it in place, swap it in,
        // and return the displaced window to the pool. One take + one return per
        // publish keeps a fixed set of buffers in circulation (pool + the one live
        // in `window`); with three buffers (pool seeds two) a free one is always
        // available even while the reader holds an in-flight clone — so the steady
        // path never reallocates. The lock is uncontended (single producer); recover
        // a poisoned guard so a prior panic elsewhere never wedges the audio path.
        let mut pool = self
            .window_pool
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut snapshot = pool.take_reusable();
        // `take_reusable` returns an `Arc` whose strong-count is 1 (a free pool
        // buffer or a fresh one), so `get_mut` succeeds and the fill is in place —
        // no reallocation (the `Vec` keeps its capacity across reuse).
        if let Some(inner) = Arc::get_mut(&mut snapshot) {
            inner.base_frame = base_frame;
            inner.samples.clear();
            inner.samples.extend_from_slice(samples);
        } else {
            // Defensive (never reached in SPSC — `take_reusable` guarantees count 1):
            // build a fresh snapshot rather than corrupt a shared one.
            snapshot = Arc::new(RingSnapshot {
                base_frame,
                samples: samples.to_vec(),
            });
        }
        // Publish the new window; the displaced window returns to the pool for reuse
        // (the reader may still hold a clone of it — `take_reusable` only reuses a
        // buffer once its strong-count has fallen back to 1).
        let previous = self.window.swap(snapshot);
        pool.return_buffer(previous);
        Ok(())
    }

    /// The address of the current window's backing sample buffer (for tests:
    /// proving the triple-buffer pool reuses a *bounded* set of buffers across many
    /// `publish_window` calls rather than allocating per block — ADR-T019 §2.2).
    #[must_use]
    #[doc(hidden)]
    #[allow(clippy::as_conversions)] // reason: a pointer→usize identity cast for a test-only buffer-reuse probe; `ptr::addr`/`expose_provenance` are unstable, and no arithmetic is done on the value.
    pub fn window_backing_ptr(&self) -> usize {
        self.window.load().samples.as_ptr() as usize
    }

    /// Sample exactly `frames` contiguous frames starting at the read cursor,
    /// advancing the cursor by `frames` (the output clock's per-tick pull).
    ///
    /// Always returns a block of exactly `frames` frames in the store's format —
    /// **never short, never a gap**. Any portion of the requested span that has
    /// not been written yet (past EOF / before the first publish) or has already
    /// been evicted from the bounded ring is **silence-filled**. Wait-free: it
    /// loads one immutable snapshot and never blocks on the writer (invariant
    /// #10), so it can run on the output clock (invariant #1).
    #[must_use]
    pub fn read(&self, frames: usize) -> AudioBlock {
        let channels = self.format.channel_count();
        let mut start = self.read_frame.load(Ordering::Relaxed);
        // Re-sync a cursor that has fallen entirely behind the retained window:
        // a live decode publishes faster than real time (connect burst, segment
        // catch-up) and the bounded ring drops its oldest frames, so a cursor
        // still parked before `base` would read silence forever. Jump to one
        // read-budget behind the live edge (never into the future), so the
        // source plays again at the next read instead of staying mute.
        {
            let snapshot = self.window.load();
            let head = snapshot.head_frame(channels.max(1));
            let want = i64::try_from(frames).unwrap_or(i64::MAX);
            // ...or that has run ahead of the write head by more than the ring
            // (a reconnect restarts the producer's absolute frames, or the
            // source stalled long enough that the cursor raced past it): the
            // cursor would otherwise sit in silence until the writer caught up
            // — never, for a restarted generation. Same re-sync to the edge.
            let ahead_by = start.saturating_sub(head);
            let capacity = i64::try_from(self.capacity_frames).unwrap_or(i64::MAX);
            if self.live_append.load(Ordering::Relaxed)
                && head > snapshot.base_frame
                && (start.saturating_add(want) <= snapshot.base_frame || ahead_by > capacity)
            {
                start = head.saturating_sub(want).max(snapshot.base_frame);
            }
        }
        let want = i64::try_from(frames).unwrap_or(i64::MAX);
        // A live-buffered store: wait for the cushion, and never consume what has
        // not been written (see `live`). A stranded cursor re-syncs to one
        // cushion behind the head, not to the head itself.
        if self.prebuffer_frames > 0 {
            let snapshot = self.window.load();
            let head = snapshot.head_frame(channels.max(1));
            if start < snapshot.base_frame || start > head {
                start = head
                    .saturating_sub(self.prebuffer_frames)
                    .max(snapshot.base_frame);
            }
            let available = head.saturating_sub(start);
            if self.buffering.load(Ordering::Relaxed) {
                if available < self.prebuffer_frames {
                    self.read_frame.store(start, Ordering::Relaxed);
                    return AudioBlock::silence(self.format, frames);
                }
                self.buffering.store(false, Ordering::Relaxed);
            } else if available < want {
                self.buffering.store(true, Ordering::Relaxed);
            }
            self.read_frame
                .store(start.saturating_add(want.min(available)), Ordering::Relaxed);
        } else {
            // Advance the cursor first: the read is a pure function of `start`,
            // and only this (single) consumer touches the cursor.
            self.read_frame
                .store(start.saturating_add(want), Ordering::Relaxed);
        }

        // Start fully silent, then overlay whatever the live window provides.
        let mut out = vec![0.0f32; frames.saturating_mul(channels)];
        if channels == 0 || frames == 0 {
            // `from_interleaved` only fails on a ragged length; `out` is exact.
            return AudioBlock::from_interleaved(self.format, out)
                .unwrap_or_else(|_| AudioBlock::silence(self.format, frames));
        }

        let snapshot = self.window.load();
        let base = snapshot.base_frame;
        let head = snapshot.head_frame(channels);
        let end = start.saturating_add(want);

        // Overlap of the requested span [start, end) with the live window
        // [base, head). Everything outside the overlap stays silence.
        let copy_from = start.max(base);
        let copy_to = end.min(head);
        if copy_to > copy_from {
            // Offsets are non-negative and within their buffers by construction.
            let dst_frame = usize::try_from(copy_from.saturating_sub(start)).unwrap_or(0);
            let src_frame = usize::try_from(copy_from.saturating_sub(base)).unwrap_or(0);
            let run_frames = usize::try_from(copy_to.saturating_sub(copy_from)).unwrap_or(0);

            let dst_off = dst_frame.saturating_mul(channels);
            let src_off = src_frame.saturating_mul(channels);
            let run = run_frames.saturating_mul(channels);

            if let (Some(dst), Some(src)) = (
                out.get_mut(dst_off..dst_off.saturating_add(run)),
                snapshot.samples.get(src_off..src_off.saturating_add(run)),
            ) {
                dst.copy_from_slice(src);
            }
        }

        AudioBlock::from_interleaved(self.format, out)
            .unwrap_or_else(|_| AudioBlock::silence(self.format, frames))
    }
}

/// Lock, recovering a poisoned guard: a panic elsewhere must never wedge the
/// audio path.
fn lock<T>(q: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    q.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// How far ahead of its PTS-due time a decoded block may be published into the
/// store (the store-side cushion; the store holds 4 s).
pub const PUBLISH_LEAD: std::time::Duration = std::time::Duration::from_secs(2);

/// How much decoded audio may wait (unpublished) between the decoder and the
/// pacer. While this is full the *decoder* waits — never the engine.
pub const MAX_QUEUED_AUDIO: std::time::Duration = std::time::Duration::from_secs(20);

/// Consecutive decode errors tolerated before a connection is abandoned. A
/// single corrupt packet (an ad-splice, a truncated segment) must not drop the
/// source into a reconnect backoff.
const MAX_CONSECUTIVE_DECODE_ERRORS: u32 = 32;

/// Run a source's audio decode loop: open the file/URL at `path`, decode and
/// resample each block to the store's canonical 48 kHz / `layout` / `f32`
/// format, and publish into `store` until end-of-stream or until `stop` is
/// raised — the audio peer of the video decode thread.
///
/// The decoder is opened on THIS thread (the libav context is not `Send`). The
/// decode/publish split is described on [`paced_publish_loop`].
///
/// An open failure logs and returns: the source rides silence (the store's
/// [`read`](AudioStore::read) silence-fills), keeping the output clock
/// independent of the input (invariant #1).
#[cfg(feature = "ffmpeg")]
pub fn audio_decode_loop(
    path: impl AsRef<std::path::Path>,
    layout: crate::format::ChannelLayout,
    store: &AudioStore,
    stop: &std::sync::atomic::AtomicBool,
) {
    // Honour an already-raised stop before doing any (potentially blocking) open.
    if stop.load(Ordering::Acquire) {
        return;
    }
    let mut decoder = match crate::decode::AudioFileDecoder::open(path.as_ref(), layout) {
        Ok(decoder) => decoder,
        Err(error) => {
            tracing::warn!(%error, "audio decode loop: open failed; source rides silence");
            return;
        }
    };
    paced_publish_loop(
        || {
            decoder
                .next_decoded()
                .map(|unit| unit.map(|u| (u.block, u.pts)))
        },
        store,
        stop,
        PUBLISH_LEAD,
        MAX_QUEUED_AUDIO,
    );
}

/// Drive `next` (a decoder) on THIS thread and publish its blocks into `store`
/// from a separate **pacer** thread, paced to wall clock by PTS.
///
/// Why two threads: a live HLS read blocks inside libav for a whole segment
/// fetch (3-27 s on a slow origin). When the same thread also slept to pace
/// publication, the next fetch only *started* once playback was within `lead`
/// of the end of the previous segment — so every fetch longer than the 2 s lead
/// was heard as a gap (cnn-usa / cnn-na: ~40-65 % audible). Here the decoder
/// reads as fast as the network delivers into a bounded queue
/// (`max_queued` of audio; while it is full the decoder waits), and the pacer
/// publishes each block when it is within `lead` of its due time. A fetch stall
/// is then absorbed by everything already queued.
///
/// Pacing: anchored at the first block; a backwards PTS or a forward jump more
/// than 5 s past the lead (a discontinuity / skipped segments) re-anchors.
/// A transient decode error is skipped; only [`MAX_CONSECUTIVE_DECODE_ERRORS`]
/// in a row end the connection. Returns at end-of-stream (after the queue has
/// drained into the store) or promptly once `stop` is raised. Neither thread is
/// on the output clock, and the store is never blocked (invariants #1/#10).
pub fn paced_publish_loop(
    mut next: impl FnMut() -> Result<Option<(AudioBlock, multiview_core::time::MediaTime)>>,
    store: &AudioStore,
    stop: &std::sync::atomic::AtomicBool,
    lead: std::time::Duration,
    max_queued: std::time::Duration,
) {
    use std::collections::VecDeque;
    use std::sync::atomic::AtomicBool;
    use std::time::{Duration, Instant};

    let rate = u128::from(store.format().sample_rate().max(1));
    let max_queued_frames =
        usize::try_from(max_queued.as_nanos().saturating_mul(rate) / 1_000_000_000)
            .unwrap_or(usize::MAX)
            .max(1);
    // (queued blocks, queued frames). Only the decoder and the pacer touch it;
    // neither is the output clock.
    let queue: Mutex<(
        VecDeque<(AudioBlock, multiview_core::time::MediaTime)>,
        usize,
    )> = Mutex::new((VecDeque::new(), 0));
    let producer_done = AtomicBool::new(false);
    let fatal = AtomicBool::new(false);

    std::thread::scope(|scope| {
        // The pacer: publish each block when it is within `lead` of its due time.
        scope.spawn(|| {
            let mut pace: Option<(Instant, multiview_core::time::MediaTime)> = None;
            loop {
                if stop.load(Ordering::Acquire) || fatal.load(Ordering::Acquire) {
                    return;
                }
                let front = lock(&queue).0.front().map(|b| b.1);
                let Some(pts) = front else {
                    if producer_done.load(Ordering::Acquire) && lock(&queue).0.is_empty() {
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(5));
                    continue;
                };
                let now = Instant::now();
                let wait = match pace {
                    Some((t0, p0)) if pts >= p0 => {
                        let off = u64::try_from(pts.saturating_sub(p0).as_nanos()).unwrap_or(0);
                        let due = t0 + Duration::from_nanos(off);
                        let wait = due.saturating_duration_since(now).saturating_sub(lead);
                        if wait > Duration::from_secs(5) {
                            // A discontinuity: re-anchor rather than sleep through it.
                            pace = Some((now, pts));
                            Duration::ZERO
                        } else {
                            wait
                        }
                    }
                    // First block, or backwards PTS (wrap / discontinuity): (re-)anchor.
                    None | Some(_) => {
                        pace = Some((now, pts));
                        Duration::ZERO
                    }
                };
                if wait > Duration::ZERO {
                    std::thread::sleep(wait.min(Duration::from_millis(20)));
                    continue;
                }
                let block = {
                    let mut q = lock(&queue);
                    let block = q.0.pop_front();
                    if let Some(b) = &block {
                        q.1 = q.1.saturating_sub(b.0.frame_count());
                    }
                    block
                };
                if let Some(block) = block {
                    if let Err(error) = store.publish(&block.0) {
                        // A format mismatch is a programming error (decoder and
                        // store share the canonical format); stop, don't busy-loop.
                        tracing::error!(%error, "audio decode loop: publish rejected; stopping");
                        fatal.store(true, Ordering::Release);
                        return;
                    }
                }
            }
        });

        // The decoder: read as fast as the source delivers, bounded by the queue.
        let mut consecutive_errors: u32 = 0;
        while !stop.load(Ordering::Acquire) && !fatal.load(Ordering::Acquire) {
            if lock(&queue).1 >= max_queued_frames {
                std::thread::sleep(Duration::from_millis(10));
                continue;
            }
            match next() {
                Ok(Some(unit)) => {
                    consecutive_errors = 0;
                    let mut q = lock(&queue);
                    q.1 = q.1.saturating_add(unit.0.frame_count());
                    q.0.push_back(unit);
                }
                Ok(None) => break, // end-of-stream: the pacer drains the queue
                Err(error) => {
                    consecutive_errors = consecutive_errors.saturating_add(1);
                    if consecutive_errors >= MAX_CONSECUTIVE_DECODE_ERRORS {
                        tracing::warn!(%error, consecutive_errors, "audio decode loop: repeated decode errors; ending connection");
                        break;
                    }
                    if consecutive_errors == 1 {
                        tracing::warn!(%error, "audio decode loop: decode error; skipping");
                    }
                }
            }
        }
        producer_done.store(true, Ordering::Release);
    });
}

#[cfg(test)]
mod resync_tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use crate::format::{AudioFormat, ChannelLayout};

    const TICK: usize = 1_600;

    fn live_store() -> (AudioFormat, AudioStore) {
        let format = AudioFormat::new(48_000, ChannelLayout::Stereo);
        (format, AudioStore::live(format, 192_000, 48_000))
    }

    fn tone(format: AudioFormat) -> AudioBlock {
        AudioBlock::from_interleaved(format, vec![0.5f32; TICK * 2]).unwrap()
    }

    fn audible(block: &AudioBlock) -> bool {
        block.interleaved().iter().all(|&s| s != 0.0)
    }

    /// The bus reads every tick whether or not audio has arrived; audio then
    /// arrives at exactly real time. After the cushion fills, nothing is lost.
    #[test]
    fn audio_arriving_after_the_bus_started_is_heard_at_real_time() {
        let (format, store) = live_store();
        for _ in 0..30 {
            let _ = store.read(TICK); // 1 s of output before the source connects
        }
        let block = tone(format);
        let mut heard = 0;
        for _ in 0..300 {
            store.publish(&block).unwrap();
            if audible(&store.read(TICK)) {
                heard += 1;
            }
        }
        assert_eq!(heard, 300 - 30 + 1, "only the 1 s cushion may be silent");
    }

    /// A segment fetch stalls the source for less than the cushion: no gap.
    #[test]
    fn a_stall_shorter_than_the_cushion_is_inaudible() {
        let (format, store) = live_store();
        let block = tone(format);
        for _ in 0..45 {
            store.publish(&block).unwrap(); // 1.5 s buffered
        }
        for tick in 0..40 {
            assert!(audible(&store.read(TICK)), "gap at tick {tick}");
        }
    }

    /// After running dry the store re-buffers, then plays continuously, instead
    /// of chattering on and off one partial block at a time.
    #[test]
    fn running_dry_rebuffers_once_then_plays_without_chatter() {
        let (format, store) = live_store();
        let block = tone(format);
        for _ in 0..31 {
            store.publish(&block).unwrap();
        }
        while audible(&store.read(TICK)) {}
        let mut transitions = 0;
        let mut last = false;
        for _ in 0..120 {
            store.publish(&block).unwrap();
            let now = audible(&store.read(TICK));
            if now != last {
                transitions += 1;
            }
            last = now;
        }
        assert_eq!(
            transitions, 1,
            "silent once while re-buffering, then continuous"
        );
    }

    /// A live HLS source: each 1 s "segment" takes 0.8 s to fetch (a slow
    /// origin; real time is kept on average). The decoder must start the next
    /// fetch while the previous segment is still being paced out, so the bus
    /// never runs dry. (Regression: pacing sleeps on the decode thread deferred
    /// every fetch until playback was within the lead of the end, so each fetch
    /// longer than the lead was heard as a gap — cnn-usa / cnn-na.)
    #[test]
    fn a_slow_segment_fetch_does_not_gap_the_paced_audio() {
        use std::sync::atomic::AtomicBool;
        use std::time::{Duration, Instant};
        let format = AudioFormat::new(48_000, ChannelLayout::Stereo);
        let store = AudioStore::live(format, 192_000, 14_400); // 0.3 s cushion
        let stop = AtomicBool::new(false);
        let block_frames = 4_800; // 100 ms
        let mut n: i64 = 0;
        let next = || {
            if n % 10 == 0 {
                std::thread::sleep(Duration::from_millis(800)); // fetch the next segment
            }
            let pts = multiview_core::time::MediaTime::from_nanos(n * 100_000_000);
            n += 1;
            let block = AudioBlock::from_interleaved(format, vec![0.5f32; block_frames * 2])?;
            Ok(Some((block, pts)))
        };
        std::thread::scope(|scope| {
            scope.spawn(|| {
                paced_publish_loop(
                    next,
                    &store,
                    &stop,
                    Duration::from_millis(500),
                    Duration::from_secs(20),
                );
            });
            let start = Instant::now();
            let tick = Duration::from_nanos(1_000_000_000 / 30);
            let mut started = false;
            let mut gaps = 0;
            for i in 1..=180u32 {
                if let Some(d) = (start + tick * i).checked_duration_since(Instant::now()) {
                    std::thread::sleep(d);
                }
                let heard = audible(&store.read(TICK));
                if heard {
                    started = true;
                } else if started {
                    gaps += 1;
                }
            }
            stop.store(true, Ordering::Release);
            assert!(started, "audio never started");
            assert_eq!(gaps, 0, "silent ticks after audio started");
        });
    }

    #[test]
    fn a_cursor_behind_the_dropped_window_resyncs_to_live_audio() {
        // A live decode bursts 3 s of tone into a 1 s ring while the consumer
        // has not read yet: the first frames are evicted. The consumer's next
        // read must return the live tone, not permanent silence.
        let format = AudioFormat::new(48_000, ChannelLayout::Stereo);
        let store = AudioStore::new(format, 48_000);
        let tone = AudioBlock::from_interleaved(format, vec![0.5f32; 4_800 * 2]).unwrap();
        for _ in 0..30 {
            store.publish(&tone).unwrap();
        }
        let block = store.read(1_600);
        assert!(
            block
                .interleaved()
                .iter()
                .all(|&s| (s - 0.5).abs() < f32::EPSILON),
            "expected live tone after resync, got silence"
        );
        // A cursor far AHEAD of the head (a restarted producer generation)
        // also re-syncs instead of riding silence until the writer catches up.
        store.seek_to(10_000_000);
        let ahead = store.read(1_600);
        assert!(
            ahead
                .interleaved()
                .iter()
                .all(|&s| (s - 0.5).abs() < f32::EPSILON),
            "a cursor far ahead of the head must re-sync to live audio"
        );
        // And the following read continues contiguously (no repeat).
        let next = store.read(1_600);
        assert!(next.interleaved().iter().any(|&s| s != 0.0) || store.buffered_frames() > 0);
    }
}
