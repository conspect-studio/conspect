//! Bounded drop-oldest audio FIFO (DEV-B4 / invariants #1 + #10).
//!
//! The display-audio sink is a *consumer*. The engine's program-audio bus pushes
//! blocks in; the sink thread drains them to the ALSA PCM. The two run on
//! independent clocks, so this FIFO decouples them — and it is the structural
//! guarantee that audio can never back-pressure the engine:
//! [`AudioFifo::push`] is a bounded in-memory copy that itself never waits,
//! and the mutex the sink wraps it in is held only across such short copies —
//! never across a PCM/device call — so the engine-side push is a bounded
//! short critical section that cannot block on the device. When full it drops
//! the **oldest** samples (so a late reader gets fresh audio, not stale
//! backlog) and records the drop count as telemetry. It never grows.
//!
//! Interleaved 32-bit float, frame-major (matching multiview-audio's
//! [`AudioBlock`](multiview_audio::AudioBlock)). Capacity and fill are measured
//! in **frames** (samples per channel) so they are channel-count independent and
//! map directly onto the servo's fill fraction.

use std::collections::VecDeque;

/// A bounded, drop-oldest, single-producer/single-consumer interleaved-float
/// audio ring.
///
/// Not internally synchronised — it lives behind the sink's own `Mutex`, which
/// is held only for these O(samples) in-memory operations (never across a
/// device call); the *property* the structure provides is that a `push` is a
/// bounded copy that itself never waits, and a full ring sheds the oldest data
/// instead of growing.
#[derive(Debug)]
pub struct AudioFifo {
    /// Interleaved samples; length is always a whole multiple of `channels`.
    buf: VecDeque<f32>,
    /// Capacity in frames (per channel).
    capacity_frames: usize,
    channels: usize,
    dropped_frames: u64,
}

impl AudioFifo {
    /// A FIFO holding at most `capacity_frames` frames of `channels`-channel
    /// interleaved float audio. `channels` is clamped to at least 1.
    #[must_use]
    pub fn new(capacity_frames: usize, channels: usize) -> Self {
        let channels = channels.max(1);
        Self {
            buf: VecDeque::with_capacity(capacity_frames.saturating_mul(channels)),
            capacity_frames,
            channels,
            dropped_frames: 0,
        }
    }

    /// Push interleaved samples (length should be a multiple of the channel
    /// count; a ragged tail is ignored). A bounded in-memory copy with no
    /// waiting of its own; when the ring would exceed capacity the **oldest**
    /// frames are dropped first, then the new samples are appended — so after
    /// the call the ring holds the most recent `capacity_frames` frames. The
    /// writer is never blocked on the device (invariants #1 + #10).
    pub fn push(&mut self, interleaved: &[f32]) {
        let ch = self.channels;
        let cap_samples = self.capacity_frames.saturating_mul(ch);
        if cap_samples == 0 {
            return;
        }
        // Whole frames only: ignore any ragged tail (never partial-frame).
        let usable = interleaved.len() - (interleaved.len() % ch);
        let incoming = interleaved.get(..usable).unwrap_or(&[]);

        // If the new data alone exceeds capacity, keep only its newest tail.
        let incoming = if incoming.len() > cap_samples {
            let drop_samples = incoming.len() - cap_samples;
            self.dropped_frames = self
                .dropped_frames
                .saturating_add(frames_u64(drop_samples / ch));
            incoming.get(drop_samples..).unwrap_or(&[])
        } else {
            incoming
        };

        // Make room for the incoming by dropping the oldest frames.
        let after = self.buf.len() + incoming.len();
        if after > cap_samples {
            let overflow = after - cap_samples;
            // Drop whole frames from the front.
            for _ in 0..overflow {
                self.buf.pop_front();
            }
            self.dropped_frames = self
                .dropped_frames
                .saturating_add(frames_u64(overflow / ch));
        }
        self.buf.extend(incoming.iter().copied());
    }

    /// Drain up to `out.len()` samples into `out`, returning the number of
    /// **real** (non-silence) samples written. Any remainder of `out` is
    /// zero-filled (silence) so a reader that asks for more than is buffered
    /// gets a full buffer of audio and never blocks waiting for the writer —
    /// the dual of the no-block-on-write rule (an underrun is silence, not a
    /// stall).
    pub fn pop_into(&mut self, out: &mut [f32]) -> usize {
        let mut written = 0usize;
        for slot in out.iter_mut() {
            match self.buf.pop_front() {
                Some(s) => {
                    *slot = s;
                    written += 1;
                }
                None => *slot = 0.0,
            }
        }
        written
    }

    /// Current fill in frames (per channel).
    #[must_use]
    pub fn fill_frames(&self) -> usize {
        self.buf.len() / self.channels
    }

    /// Current fill as a fraction of capacity (`0.0`..=`1.0`); the servo's input.
    #[must_use]
    pub fn fill_fraction(&self) -> f64 {
        if self.capacity_frames == 0 {
            return 0.0;
        }
        frames_fraction(self.fill_frames(), self.capacity_frames)
    }

    /// Total frames dropped to overflow since construction (telemetry).
    #[must_use]
    pub const fn dropped_frames(&self) -> u64 {
        self.dropped_frames
    }

    /// The channel count this FIFO carries.
    #[must_use]
    pub const fn channels(&self) -> usize {
        self.channels
    }
}

/// `fill / capacity` as an `f64` in `[0, 1]`, without an `as` cast on the hot
/// path elsewhere.
#[allow(clippy::as_conversions, clippy::cast_precision_loss)]
// reason: frame counts are tiny (< 2^53); the cast is lossless and no fallible
// `From<usize> for f64` exists.
fn frames_fraction(fill: usize, capacity: usize) -> f64 {
    (fill as f64) / (capacity.max(1) as f64)
}

/// `usize → u64` frame-count widening for the drop telemetry counter.
fn frames_u64(frames: usize) -> u64 {
    u64::try_from(frames).unwrap_or(u64::MAX)
}
