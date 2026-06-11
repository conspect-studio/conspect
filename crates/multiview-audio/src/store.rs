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
//! The sample window lives behind an [`arc_swap::ArcSwap`] snapshot — the same
//! primitive the video tile store uses. The reader loads an immutable snapshot
//! wait-free; the writer builds the next snapshot and swaps it in. There is no
//! `unsafe` (this crate is `unsafe_code = forbid`) and no mutex on the read path.
//!
//! The read cursor is a single [`AtomicI64`]: the store is a single-producer /
//! single-consumer handoff (one decode thread, one output clock), and only the
//! consumer advances the cursor, so it is never torn.

use std::sync::atomic::{AtomicI64, Ordering};

use arc_swap::ArcSwap;

use crate::error::{AudioError, Result};
use crate::format::{AudioBlock, AudioFormat};

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
    window: ArcSwap<RingSnapshot>,
    /// The consumer's absolute read cursor (next frame to be read). Only the
    /// single consumer advances it.
    read_frame: AtomicI64,
}

impl AudioStore {
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
            window: ArcSwap::from_pointee(RingSnapshot {
                base_frame: 0,
                samples: Vec::new(),
            }),
            read_frame: AtomicI64::new(0),
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
        let cap_samples = self.capacity_frames.saturating_mul(channels);

        // Build the next immutable window from the current one: append the new
        // samples, then drop the oldest beyond capacity (advancing `base_frame`
        // by however many frames were evicted). This is O(window) copy-on-write
        // by design — it runs on the *sampled decode thread*, never the output
        // clock — and gives the wait-free reader a stable snapshot.
        let current = self.window.load();
        let mut next =
            Vec::with_capacity((current.samples.len() + incoming.len()).min(cap_samples));
        next.extend_from_slice(&current.samples);
        next.extend_from_slice(incoming);

        let mut base_frame = current.base_frame;
        let overflow_samples = next.len().saturating_sub(cap_samples);
        if overflow_samples > 0 {
            // Drop whole frames only (overflow is a multiple of `channels` here
            // because both `next.len()` and `cap_samples` are).
            let evicted_frames = i64::try_from(overflow_samples / channels).unwrap_or(i64::MAX);
            next.drain(0..overflow_samples);
            base_frame = base_frame.saturating_add(evicted_frames);
        }

        self.window.store(std::sync::Arc::new(RingSnapshot {
            base_frame,
            samples: next,
        }));
        Ok(())
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
        let start = self.read_frame.load(Ordering::Relaxed);
        // Advance the cursor first: the read is a pure function of `start`, and
        // only this (single) consumer touches the cursor.
        let want = i64::try_from(frames).unwrap_or(i64::MAX);
        self.read_frame
            .store(start.saturating_add(want), Ordering::Relaxed);

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

/// Run a source's audio decode loop: open the file/URL at `path`, decode and
/// resample each block to the store's canonical 48 kHz / `layout` / `f32`
/// format, and publish into `store` until end-of-stream or until `stop` is
/// raised — the audio peer of the video decode thread (and of `multiview-cli`'s
/// `synth::generator_loop`).
///
/// The decoder is **opened on this (the worker) thread** and never crosses a
/// thread boundary: the underlying libav context is not `Send`, so — exactly as
/// `multiview-cli`'s `ingest_loop` does for video — the caller spawns this loop
/// with the `Send` description (`path`, `layout`) and the loop builds the
/// non-`Send` decoder itself. The caller signals teardown via `stop`; the loop
/// polls it between blocks so a wedged or long-running source can never delay a
/// join past one decoded block. It never paces the engine (the engine samples
/// the store independently) and never blocks it (the store is non-blocking).
///
/// An open failure or a mid-stream decode error logs and ends the loop: the
/// source's tile simply rides silence (the store's [`read`](AudioStore::read)
/// yields silence past what was published), keeping the sampled track gap-free
/// (ADR-R005 §4.1) and the output clock independent of the input (invariant #1).
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
    while !stop.load(Ordering::Acquire) {
        match decoder.next_block() {
            Ok(Some(block)) => {
                if let Err(error) = store.publish(&block) {
                    // A format mismatch here is a programming error (the decoder
                    // and store share the same canonical format); log and stop
                    // rather than busy-loop.
                    tracing::error!(%error, "audio decode loop: publish rejected; stopping");
                    break;
                }
            }
            Ok(None) => break, // end-of-stream: the store rides silence-fill
            Err(error) => {
                tracing::warn!(%error, "audio decode loop: decode error; stopping source");
                break;
            }
        }
    }
}
