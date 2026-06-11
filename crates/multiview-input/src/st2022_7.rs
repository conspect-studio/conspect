//! SMPTE **ST 2022-7** seamless protection switching — the **hitless dual-path
//! reconstruction** algorithm (pure, property-tested).
//!
//! ST 2022-7:2019 ("Seamless Protection Switching of RTP Datagrams") sends the
//! *same* RTP stream over two independent network paths. A receiver merges the
//! two copies by **RTP sequence number**: for each sequence number it forwards
//! the first copy that arrives from *either* path and discards the duplicate, so
//! a packet lost on one path but present on the other produces **no gap** in the
//! output. Because the protection operates at the RTP-datagram level, it applies
//! to any RTP essence — ST 2110-20/-30/-40 alike (brief, verified note).
//!
//! ## The algorithm
//!
//! [`HitlessReconstructor`] keeps a small **bounded reorder window** keyed by the
//! 16-bit RTP sequence number (unwrapped to a 32-bit monotonic counter on first
//! use). Packets from path A and path B are [`push`](HitlessReconstructor::push)ed
//! as they arrive in any interleaving; the reconstructor:
//!
//! * **De-duplicates** by sequence number — the second copy of a sequence is
//!   dropped (whichever path it came from).
//! * **Reorders** within the window — packets are released in strictly
//!   increasing sequence order via [`drain`](HitlessReconstructor::drain).
//! * Is **strictly bounded** — at most `window` distinct sequence numbers are
//!   held. When a newer packet would exceed the window, the window slides
//!   forward and any not-yet-seen older sequence is declared a genuine gap
//!   (lost on *both* paths) so the output never stalls.
//!
//! ## Correctness contract (the property test)
//!
//! Given any interleaving and any per-path loss pattern of the two input copies,
//! the merged output is the **gap-minimized in-order sequence**: every sequence
//! number present on *at least one* path (and still inside the sliding window
//! when its successors arrive) appears **exactly once**, in **strictly
//! increasing** order. This is the invariant the property tests in
//! `tests/st2022_7_properties.rs` assert over thousands of random scenarios.
//!
//! ## Isolation (invariants #1 / #10)
//!
//! This is a **pure value machine** — bytes/handles in, merged handles out, no
//! sockets, no `.await`, no blocking. It feeds the last-good-frame store like any
//! other ingest path; it never paces the output clock. The two UDP receive
//! sockets that drive it live behind the off-by-default `st2110` feature.

use std::collections::BTreeMap;

/// Which of the two ST 2022-7 paths a packet arrived on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Path {
    /// The primary (first) redundant path.
    A,
    /// The secondary (second) redundant path.
    B,
}

/// The outcome of pushing one packet into the reconstructor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum PushOutcome {
    /// The packet was accepted into the reorder window (first copy of its
    /// sequence).
    Accepted,
    /// A copy of this sequence was already present (from either path); the
    /// duplicate was discarded.
    Duplicate,
    /// The packet's sequence was at or before the last-released watermark, so it
    /// arrived too late and was discarded.
    TooLate,
}

/// The hitless dual-path reconstructor (ST 2022-7).
///
/// `P` is the per-packet payload the caller wants merged (an RTP packet, a
/// decoded handle, …). Packets are keyed by their **32-bit unwrapped sequence
/// number**; the reconstructor unwraps the 16-bit RTP sequence the first time it
/// sees one and tracks wrap-around with delta arithmetic so a 24/7 stream that
/// crosses the 2^16 boundary keeps merging correctly.
#[derive(Debug)]
pub struct HitlessReconstructor<P> {
    /// Pending packets keyed by unwrapped sequence, awaiting in-order release.
    window: BTreeMap<u32, P>,
    /// Maximum number of distinct sequences held before the window slides.
    capacity: usize,
    /// Hold-back depth: [`HitlessReconstructor::drain`] only releases sequences
    /// that are at least this far below the highest buffered sequence, giving
    /// reordered earlier packets time to arrive before the watermark passes them.
    reorder_depth: u32,
    /// The highest sequence already **released or evicted** — the too-late floor.
    /// A push at or below this is genuinely too late (its slot already passed).
    /// `None` until the first packet is released/evicted. The *next* sequence to
    /// release is `released_through + 1` (or the lowest buffered key initially).
    released_through: Option<u32>,
    /// State for unwrapping the 16-bit RTP sequence to 32 bits.
    unwrap: SeqUnwrapper,
}

impl<P> HitlessReconstructor<P> {
    /// Create a reconstructor with a reorder window of `capacity` distinct
    /// sequence numbers and a hold-back **reorder depth** of `capacity / 2`.
    ///
    /// A `capacity` of zero is clamped to one. The window bounds memory: a flood
    /// on one path can never grow the buffer past `capacity` entries.
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        let capacity = capacity.max(1);
        let depth = u32::try_from(capacity / 2).unwrap_or(u32::MAX);
        Self::with_reorder_depth(capacity, depth)
    }

    /// Create a reconstructor with an explicit reorder window `capacity` and
    /// hold-back `reorder_depth`.
    ///
    /// `reorder_depth` is the number of sequence positions [`drain`] holds back
    /// below the newest buffered sequence: a larger depth tolerates deeper
    /// reordering at the cost of latency. It is clamped so it never exceeds the
    /// window capacity. A depth of `0` releases eagerly (no reorder tolerance).
    ///
    /// [`drain`]: HitlessReconstructor::drain
    #[must_use]
    pub fn with_reorder_depth(capacity: usize, reorder_depth: u32) -> Self {
        let capacity = capacity.max(1);
        let cap_u32 = u32::try_from(capacity).unwrap_or(u32::MAX);
        Self {
            window: BTreeMap::new(),
            capacity,
            reorder_depth: reorder_depth.min(cap_u32),
            released_through: None,
            unwrap: SeqUnwrapper::new(),
        }
    }

    /// The configured reorder-window capacity (distinct sequence numbers).
    #[must_use]
    pub const fn capacity(&self) -> usize {
        self.capacity
    }

    /// The number of packets currently buffered in the window.
    #[must_use]
    pub fn buffered(&self) -> usize {
        self.window.len()
    }

    /// The highest sequence number already released or evicted (the too-late
    /// floor), or [`None`] before anything has been released. A packet at or
    /// below this is rejected as [`PushOutcome::TooLate`].
    #[must_use]
    pub const fn released_through(&self) -> Option<u32> {
        self.released_through
    }

    /// The unwrapped sequence number the next [`HitlessReconstructor::drain`]
    /// would release first (the lowest buffered sequence), or [`None`] when the
    /// window is empty.
    #[must_use]
    pub fn next_release(&self) -> Option<u32> {
        self.window.keys().next().copied()
    }

    /// Push one packet arriving on `path` with the 16-bit RTP `sequence`.
    ///
    /// Unwraps the sequence to 32 bits, de-duplicates against the other path, and
    /// inserts it into the reorder window. When the window is full and the packet
    /// is newer than the oldest buffered sequence, the window slides forward
    /// (the oldest not-yet-released sequence becomes a permanent gap). Released
    /// packets are obtained by calling [`HitlessReconstructor::drain`].
    ///
    /// `_path` is accepted to keep the call site symmetric and to allow per-path
    /// metrics later; the merge logic depends only on the sequence number.
    pub fn push(&mut self, _path: Path, sequence: u16, packet: P) -> PushOutcome {
        let seq = self.unwrap.unwrap(sequence);

        // A sequence at or below the floor (already released or evicted) is
        // genuinely too late — its release slot has passed. A reordered earlier
        // packet that is still *above* the floor is accepted and slots into the
        // window so it can be released in order.
        if let Some(floor) = self.released_through {
            if seq <= floor {
                return PushOutcome::TooLate;
            }
        }
        if self.window.contains_key(&seq) {
            return PushOutcome::Duplicate;
        }

        // Slide the window if inserting would exceed capacity. We evict from the
        // front (lowest sequence) and raise the floor past it: that sequence was
        // lost on both paths (never arrived before its successors filled the
        // window), so it is a genuine, unrecoverable gap.
        while self.window.len() >= self.capacity {
            // Only slide if the incoming packet is newer than the oldest pending
            // one; otherwise the incoming packet is itself the oldest and is
            // simply dropped to preserve the bound.
            let oldest = self.window.keys().next().copied();
            match oldest {
                Some(old) if seq > old => {
                    self.window.remove(&old);
                    // The floor rises to the evicted (lost) sequence so it is
                    // never awaited again.
                    self.released_through = Some(match self.released_through {
                        Some(f) => f.max(old),
                        None => old,
                    });
                }
                _ => {
                    // Incoming is the oldest (or window empty edge): drop it to
                    // keep the buffer bounded.
                    return PushOutcome::TooLate;
                }
            }
        }

        self.window.insert(seq, packet);
        PushOutcome::Accepted
    }

    /// Release every packet that is now contiguously in order from the current
    /// watermark, **holding back** the most recent [`reorder_depth`] sequence
    /// positions so reordered earlier packets still have time to arrive.
    ///
    /// Concretely: with the highest buffered sequence `hi`, no sequence above
    /// `hi - reorder_depth` is released; below that boundary, packets are released
    /// contiguously from the watermark, advancing it past each one. A missing
    /// sequence below the boundary (a true both-path loss) stops the contiguous
    /// run for this call; the gap is only skipped when a later push slides the
    /// window past it or [`HitlessReconstructor::flush`] is called. The result is
    /// the gap-minimized in-order merged stream.
    ///
    /// [`reorder_depth`]: HitlessReconstructor::with_reorder_depth
    pub fn drain(&mut self) -> Vec<P> {
        let mut out = Vec::new();
        let Some(&hi) = self.window.keys().next_back() else {
            return out;
        };
        // Only sequences at or below this boundary are safe to release; the most
        // recent `reorder_depth` positions are held back for possible reordering.
        let boundary = hi.saturating_sub(self.reorder_depth);
        loop {
            // The next sequence to release is the contiguous successor of the
            // floor, or — before anything has been released — the lowest buffered
            // key (which becomes the start of the in-order run).
            let next = match self.released_through {
                Some(f) => f.saturating_add(1),
                None => match self.window.keys().next().copied() {
                    Some(lo) => lo,
                    None => break,
                },
            };
            if next > boundary {
                break;
            }
            match self.window.remove(&next) {
                Some(packet) => {
                    out.push(packet);
                    self.released_through = Some(next);
                }
                None => break,
            }
        }
        out
    }

    /// Force-release every buffered packet in sequence order regardless of the
    /// hold-back depth, for flush at end-of-stream. Empties the window and raises
    /// the floor past the last flushed sequence.
    ///
    /// `BTreeMap::into_values` (paired with the ascending-key iteration used to
    /// find the last key) yields entries in ascending key (sequence) order, so
    /// the flushed packets are already in strictly increasing sequence order.
    pub fn flush(&mut self) -> Vec<P> {
        let last = self.window.keys().next_back().copied();
        let drained: Vec<P> = core::mem::take(&mut self.window).into_values().collect();
        if let Some(last) = last {
            self.released_through = Some(match self.released_through {
                Some(f) => f.max(last),
                None => last,
            });
        }
        drained
    }
}

/// Unwraps a 16-bit RTP sequence number to a monotonic 32-bit counter using
/// delta-based wrap detection (per streaming-gotchas: never value-compare; use
/// the signed 16-bit delta and accumulate into a wider counter).
#[derive(Debug, Clone, Copy)]
struct SeqUnwrapper {
    /// The accumulated high bits (multiples of 2^16) added to unwrap.
    epoch: u32,
    /// The previous raw 16-bit sequence seen, to compute the delta.
    last: Option<u16>,
}

impl SeqUnwrapper {
    const fn new() -> Self {
        Self {
            epoch: 0,
            last: None,
        }
    }

    /// Unwrap `seq` against the previously-seen sequence. A backward jump larger
    /// than half the sequence space is treated as a forward wrap; a forward jump
    /// larger than half (i.e. a small backward step) does not roll the epoch
    /// back, so late/reordered packets still resolve to a sane 32-bit value.
    fn unwrap(&mut self, seq: u16) -> u32 {
        let Some(last) = self.last else {
            self.last = Some(seq);
            return self.epoch | u32::from(seq);
        };
        let delta = seq.wrapping_sub(last);
        // `delta` in the low half (0..0x8000) is a forward step; in the high half
        // it is a backward step (reorder), with the special case of crossing the
        // wrap boundary forward.
        if delta != 0 && delta < 0x8000 {
            // Forward. Detect a wrap: if the new raw value is numerically smaller
            // than the last but we stepped forward, the epoch rolled over.
            if seq < last {
                self.epoch = self.epoch.wrapping_add(0x1_0000);
            }
            self.last = Some(seq);
        }
        // For a backward (reorder) step we do not advance `last` and do not change
        // the epoch; the reordered packet resolves against the current epoch.
        self.epoch | u32::from(seq)
    }
}
