//! Independently bounded per-rendition encode workers and their typed event
//! protocol (broadcast output contract, slice 2).
//!
//! The program bakes exactly one immutable canvas per tick and offers that
//! canvas non-blockingly to **one worker per distinct video rendition key**.
//! Each worker owns its own encoder thread, its own bounded admission queue and
//! its own failure state, so a slow, wedged or reordering encoder can neither
//! stall the program clock nor another rendition (invariants #1 and #10, and
//! the R3 broadcast output contract §"Bounded packet window and interleave").
//!
//! Every worker emission is a typed [`DispatchEvent`]. A worker that has
//! *proved* it has no reorder delay publishes an exclusive [`DtsWatermark`] on
//! every event; a worker that cannot prove that publishes `None`, which the
//! window assembler must treat as "unproven" and fail closed on a bounded
//! watchdog instead of closing a window optimistically. Guessing that "the
//! call for tick N returned, so tick N is complete" is exactly the bug this
//! protocol exists to prevent: an encoder may emit zero to many packets per
//! call, may hold packets back, and may return them on later calls.

use std::sync::mpsc::{self, SyncSender, TrySendError};
use std::sync::Arc;
use std::thread::{self, JoinHandle};

use multiview_compositor::pipeline::Nv12Image;
use multiview_core::time::Rational;
use multiview_ffmpeg::EncodedPacket;

use crate::pipeline::PipelineError;

/// Bounded per-worker input depth, in baked ticks.
///
/// Deliberately small: the canvas is an `Arc` and a healthy encoder drains in
/// well under one tick. This is *not* a delivery budget — the byte/count/age
/// budgets live on the per-binding encoded-packet queues
/// ([`multiview_output::batch`]). Growing this number to hide a slow encoder is
/// the #202 bandaid the contract forbids.
pub const WORKER_QUEUE_TICKS: usize = 4;

/// One baked picture on the program's absolute rational tick.
pub struct BakedTick {
    /// Absolute program tick (0-based). Never a wall-clock or encoder ordinal.
    pub tick: u64,
    /// The immutable baked NV12 canvas for that tick. One allocation, shared by
    /// every rendition worker offered the tick.
    pub canvas: Arc<Nv12Image>,
}

/// An **exclusive** DTS completeness watermark, in nanoseconds.
///
/// `complete_before_ns` means: no packet for this stream — including packets
/// the encoder still holds for delayed/reordered output — will ever be produced
/// with a rescaled DTS strictly below this bound. The bound is exclusive, so a
/// packet whose DTS equals the bound may still arrive. A watermark is a
/// monotonic non-decreasing claim about the *stream*, never a claim about the
/// current call.
///
/// Exactness requirement: the bound is computed with the same monotone
/// floor-to-nanoseconds rational rescale the consumer uses, so under
/// [`ReorderBound::NoReorder`] `floor_ns(PTS(tick + 1))` really is a lower
/// bound on every future packet's rescaled DTS (floor is monotone, and future
/// encoder DTS values are ≥ the exact PTS of tick `tick + 1`). A producer that
/// cannot show that equality must publish `None`: a bound even one nanosecond
/// too high would let the window assembler close a window whose packet is still
/// in flight.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct DtsWatermark {
    /// Exclusive upper bound in nanoseconds on the DTS of any future packet.
    pub complete_before_ns: i128,
}

/// Whether a concrete encoder can prove it has no reorder/lookahead delay.
///
/// `NoReorder` is an assertion the *encoder factory* makes only after it has
/// verified the opened encoder's effective options (for example `bf=0` **and**
/// `delay=0`/`zerolatency=1` read back from the encoder that actually opened).
/// A plan or a requested option set is not proof. When in doubt, `Unknown`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReorderBound {
    /// The opened encoder has no reorder buffer and no lookahead: it cannot
    /// emit a packet for an earlier input after the call for a later input.
    NoReorder,
    /// No demonstrated bound. The worker never publishes a watermark.
    Unknown,
}

/// A typed emission from one rendition worker (or from the shared program
/// audio encoder), consumed by the window assembler / binding dispatcher.
#[derive(Debug, Clone)]
pub enum DispatchEvent {
    /// Packets emitted while encoding one program tick of one video rendition.
    Video {
        /// Index of this rendition in the accepted plan's distinct video keys.
        key_index: usize,
        /// The absolute program tick that was encoded.
        tick: u64,
        /// The **opened encoder's** timebase. The assembler rescales per stream
        /// with checked rational arithmetic; it never compares raw DTS across
        /// streams.
        timebase: Rational,
        /// Zero or more packets. An empty vector is normal and proves nothing.
        packets: Vec<EncodedPacket>,
        /// Proven exclusive completeness bound, or `None` when unproven.
        watermark: Option<DtsWatermark>,
    },
    /// Packets emitted while encoding one program tick of the shared audio
    /// bus / discrete tracks.
    Audio {
        /// The absolute program tick whose sample window was encoded.
        tick: u64,
        /// Per configured audio stream, indexed by stream index: the **opened**
        /// encoder's timebase for that stream. Independent tracks may run at
        /// different sample rates, so one common timebase is not assumed; a
        /// consumer that cannot validate every stream's base must reject the
        /// event rather than rescale a packet with the wrong base.
        timebases: Vec<Rational>,
        /// All packets emitted for every configured audio stream this tick.
        packets: Vec<EncodedPacket>,
        /// Per configured audio stream, indexed the same way as `timebases`: a
        /// silent or absent track must never inherit another track's proof.
        watermarks: Vec<Option<DtsWatermark>>,
    },
    /// Terminal event: the encoder was **flushed**, `packets` is the complete
    /// drained tail, and no further packet for this rendition can ever exist.
    ///
    /// This is absolute EOF completeness regardless of [`ReorderBound`]. A
    /// consumer must keep serving the already-admitted tail and drain before it
    /// finalises; it must not `close()` a queue synchronously on receipt.
    FinishVideo {
        /// Index of this rendition in the accepted plan's distinct video keys.
        key_index: usize,
        /// The drained tail, in encoder order.
        packets: Vec<EncodedPacket>,
    },
    /// Terminal audio event with the same EOF semantics as [`Self::FinishVideo`].
    FinishAudio {
        /// The drained tail, in encoder order.
        packets: Vec<EncodedPacket>,
    },
}

/// A concrete video encoder owned by one rendition worker.
///
/// Production opens one libav encoder per distinct rendition key; tests inject
/// a counter to prove that additional bindings with identical keys add no
/// encoder and no thread.
pub trait VideoEncodeWorker: Send + 'static {
    /// Encode one baked tick. May return zero to many packets, including
    /// packets for *earlier* ticks when the encoder reorders.
    ///
    /// # Errors
    /// Returns a concrete encoder failure without publishing a false watermark.
    fn encode(&mut self, item: BakedTick) -> Result<Vec<EncodedPacket>, PipelineError>;

    /// The demonstrated reorder bound of the encoder that actually opened.
    fn reorder_bound(&self) -> ReorderBound;

    /// Flush the encoder, returning every delayed packet. Called once, on the
    /// worker thread, when the program ends.
    ///
    /// # Errors
    /// Returns a concrete encoder flush failure without publishing false EOF.
    fn finish(&mut self) -> Result<Vec<EncodedPacket>, PipelineError>;

    /// Make the next encoded frame a keyframe. One-shot; the caller owns the
    /// rate limit. The default does nothing (an encoder that cannot force).
    fn force_keyframe(&mut self) {}
}

/// Why a non-blocking offer could not be accepted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OfferRejection {
    /// The worker's bounded input queue is full: this rendition is overloaded.
    /// The caller records the loss against the rendition's bindings and asks
    /// the encoder for a fresh IDR; it never waits for the program clock.
    Overloaded,
    /// The worker thread has ended (encoder failure or shutdown).
    Ended,
}

/// Monotonic watermark tracker for the shared audio encoder / any stream whose
/// completeness proof is produced outside a worker thread.
///
/// Never decreases: a tick that submitted no samples cannot withdraw a
/// previously proven bound.
#[derive(Debug, Default)]
pub struct WatermarkTracker {
    last: Option<DtsWatermark>,
}

impl WatermarkTracker {
    /// A tracker with no proven bound yet.
    #[must_use]
    pub const fn new() -> Self {
        Self { last: None }
    }

    /// Advance with a candidate exclusive bound (or `None` when unproven) and
    /// return the currently proven watermark. `None` means nothing is proven.
    pub fn advance(&mut self, candidate: Option<DtsWatermark>) -> Option<DtsWatermark> {
        if let Some(candidate) = candidate {
            let next = match self.last {
                Some(last) => DtsWatermark {
                    complete_before_ns: last.complete_before_ns.max(candidate.complete_before_ns),
                },
                None => candidate,
            };
            self.last = Some(next);
        }
        self.last
    }

    /// The currently proven watermark, if any.
    #[must_use]
    pub const fn current(&self) -> Option<DtsWatermark> {
        self.last
    }
}

/// The exclusive DTS lower bound for the tick **after** `tick`, in nanoseconds:
/// exactly `floor_ns(PTS(tick + 1))`.
///
/// Under [`ReorderBound::NoReorder`] every packet for an input at or after tick
/// `tick + 1` has an encoder DTS of at least the exact PTS of that tick, and the
/// floor rescale is monotone, so no future packet's rescaled DTS is below this
/// value. Returns `None` for a degenerate cadence, which makes the worker
/// publish no watermark at all rather than a wrong one.
#[must_use]
pub fn next_tick_lower_bound_ns(tick: u64, cadence: Rational) -> Option<DtsWatermark> {
    if cadence.num <= 0 || cadence.den <= 0 {
        return None;
    }
    let num = u128::try_from(cadence.num).ok()?;
    let den = u128::try_from(cadence.den).ok()?;
    let next_tick = u128::from(tick).checked_add(1)?;
    // floor(next_tick * den * 1e9 / num), all in u128, then narrowed.
    let ns = next_tick
        .checked_mul(den)?
        .checked_mul(1_000_000_000)?
        .checked_div(num)?;
    Some(DtsWatermark {
        complete_before_ns: i128::try_from(ns).ok()?,
    })
}

/// The exclusive DTS lower bound after a FIFO, frame-aligned audio encoder has
/// emitted its `last_pts` packet: exactly `floor_ns(last_pts + frame_samples)`.
///
/// Correct only when the next packet's PTS is exactly `last_pts +
/// frame_samples` — true for AAC-family FIFO encoders whose packets are
/// frame-aligned and whose output order is emission order. The bound is derived
/// from the emitted packet PTS, never from the submitted sample count: a
/// partially submitted frame may still produce a packet for earlier samples,
/// so `samples_submitted` is not a proof.
#[must_use]
pub fn next_frame_lower_bound_ns(
    last_pts: i64,
    frame_samples: u32,
    sample_rate: u32,
) -> Option<DtsWatermark> {
    if sample_rate == 0 || frame_samples == 0 {
        return None;
    }
    let next_pts = i128::from(last_pts).checked_add(i128::from(frame_samples))?;
    let next_pts = u128::try_from(next_pts).ok()?;
    let ns = next_pts
        .checked_mul(1_000_000_000)?
        .checked_div(u128::from(sample_rate))?;
    Some(DtsWatermark {
        complete_before_ns: i128::try_from(ns).ok()?,
    })
}

/// Bounded input and owned worker lifecycle for one distinct video key.
pub struct RenditionWorker {
    key_index: usize,
    input: SyncSender<BakedTick>,
    thread: JoinHandle<Result<(), PipelineError>>,
}

impl RenditionWorker {
    /// Spawn the encoder thread for one rendition key.
    ///
    /// `on_event` is called on the worker thread for every [`DispatchEvent`],
    /// including the terminal `FinishVideo` after `finish()` drained the
    /// encoder. The event sink is dropped only after that terminal event has
    /// been delivered, so a consuming queue observes EOF after the tail.
    ///
    /// # Errors
    /// Returns [`PipelineError::Output`] if the thread cannot be spawned.
    pub fn spawn(
        key_index: usize,
        cadence: Rational,
        timebase: Rational,
        mut encoder: Box<dyn VideoEncodeWorker>,
        mut on_event: impl FnMut(DispatchEvent) -> Result<(), PipelineError> + Send + 'static,
    ) -> Result<Self, PipelineError> {
        let (input, receiver) = mpsc::sync_channel::<BakedTick>(WORKER_QUEUE_TICKS);
        let thread = thread::Builder::new()
            .name(format!("multiview-video-rendition-{key_index}"))
            .spawn(move || {
                let proof = encoder.reorder_bound();
                let mut watermarks = WatermarkTracker::new();
                while let Ok(item) = receiver.recv() {
                    let tick = item.tick;
                    let packets = encoder.encode(item)?;
                    // A zero-reorder encoder emits packets in frame order, so a
                    // packet's PTS is its frame's tick and no LATER packet can
                    // carry an earlier or equal frame. The watermark therefore
                    // keys off the packets this event ACTUALLY EMITTED — not
                    // the submitted tick — because a delayed-but-ordered
                    // encoder (e.g. mpeg2video's fixed one-frame delay, whose
                    // drained packets are re-stamped `DTS := PTS` upstream)
                    // may emit zero packets for this tick and still be
                    // perfectly healthy. An unproven encoder claims nothing.
                    let watermark = match proof {
                        ReorderBound::NoReorder => {
                            let highest_emitted = packets
                                .iter()
                                .filter_map(EncodedPacket::pts)
                                .max()
                                .and_then(|pts| u64::try_from(pts).ok());
                            watermarks.advance(
                                highest_emitted
                                    .and_then(|emitted| next_tick_lower_bound_ns(emitted, cadence)),
                            )
                        }
                        ReorderBound::Unknown => None,
                    };
                    on_event(DispatchEvent::Video {
                        key_index,
                        tick,
                        timebase,
                        packets,
                        watermark,
                    })?;
                }
                // EOF: flush the encoder and publish the complete tail. The
                // event is terminal and absolute regardless of the reorder
                // bound, because a flushed encoder cannot emit again.
                let tail = encoder.finish()?;
                on_event(DispatchEvent::FinishVideo {
                    key_index,
                    packets: tail,
                })
            })
            .map_err(|err| PipelineError::Output {
                kind: "video-worker",
                reason: err.to_string(),
            })?;
        Ok(Self {
            key_index,
            input,
            thread,
        })
    }

    /// The rendition key index this worker encodes.
    #[must_use]
    pub const fn key_index(&self) -> usize {
        self.key_index
    }

    /// Offer one baked tick without waiting. A full queue is a truthful
    /// overload for this rendition, never a reason to stall the program clock.
    ///
    /// # Errors
    /// Returns [`OfferRejection`] when the worker is overloaded or ended.
    pub fn offer(&self, item: BakedTick) -> Result<(), OfferRejection> {
        self.input.try_send(item).map_err(|err| match err {
            TrySendError::Full(_) => OfferRejection::Overloaded,
            TrySendError::Disconnected(_) => OfferRejection::Ended,
        })
    }

    /// Offer one baked tick, waiting up to `bound` for the worker to take it.
    ///
    /// The bake consumer (OFF the hot path) uses this with a CADENCE-DERIVED
    /// bound — one hot-queue drain horizon — so that while the consumer waits
    /// inside the real-time envelope, the hot queue keeps absorbing engine
    /// ticks and NOTHING is shed (no-falter). A momentarily-full worker queue
    /// (a burst or a transient encode stall) back-pressures the consumer only
    /// within that horizon; only a worker that cannot drain even one tick
    /// within it — a genuinely wedged encoder, beyond the real-time envelope
    /// — fails explicitly, so the fatal signal stops the run and the
    /// supervisor restarts a fresh process. This is not an unbounded wait:
    /// the bound is fixed by the output cadence, not by back-pressure growing
    /// with load.
    ///
    /// # Errors
    /// Returns [`OfferRejection::Overloaded`] when the bound elapsed without a
    /// slot (wedged encoder), or [`OfferRejection::Ended`] when the worker ended.
    pub fn offer_within(
        &self,
        mut item: BakedTick,
        bound: std::time::Duration,
    ) -> Result<(), OfferRejection> {
        // `SyncSender::send_timeout` is still unstable, so the bound is a
        // 1 ms poll — coarse, but this is the OFF-hot-path consumer waiting on
        // an encoder that takes a tick span per frame anyway.
        let deadline = std::time::Instant::now() + bound;
        loop {
            match self.input.try_send(item) {
                Ok(()) => return Ok(()),
                Err(TrySendError::Full(returned)) => {
                    item = returned;
                    if std::time::Instant::now() >= deadline {
                        return Err(OfferRejection::Overloaded);
                    }
                    std::thread::sleep(std::time::Duration::from_millis(1));
                }
                Err(TrySendError::Disconnected(_)) => return Err(OfferRejection::Ended),
            }
        }
    }

    /// Stop the worker: drop the input, wait for `finish()` and the terminal
    /// event, and surface the encoder error if the thread failed.
    ///
    /// # Errors
    /// Returns the encoder's failure, or an error if the worker panicked.
    pub fn join(self) -> Result<(), PipelineError> {
        self.close_input().join()
    }

    /// Drop the input sender now, without waiting: the worker leaves its
    /// receive loop, runs `finish()` and its terminal event, and ends on its
    /// own. The returned handle may be joined later or dropped (detached) —
    /// either way no thread outlives its key with a live input.
    #[must_use]
    pub fn close_input(self) -> ClosingWorker {
        drop(self.input);
        ClosingWorker {
            thread: self.thread,
        }
    }
}

/// A [`RenditionWorker`] whose input is already closed (see
/// [`RenditionWorker::close_input`]).
pub struct ClosingWorker {
    thread: JoinHandle<Result<(), PipelineError>>,
}

impl ClosingWorker {
    /// Wait for `finish()` and the terminal event.
    ///
    /// # Errors
    /// Returns the encoder's failure, or an error if the worker panicked.
    pub fn join(self) -> Result<(), PipelineError> {
        self.thread.join().map_err(|_| PipelineError::Output {
            kind: "video-worker",
            reason: "video rendition worker panicked".to_owned(),
        })?
    }

    /// The worker thread has ended.
    #[must_use]
    pub fn is_finished(&self) -> bool {
        self.thread.is_finished()
    }
}

/// The program-audio encode worker's bounded queue depth, in ticks. Larger than
/// the video queue because an audio job is a few KB and the AAC encoder emits a
/// frame every ~21 ms, so short bursts are normal.
pub const AUDIO_WORKER_QUEUE_TICKS: usize = 8;

/// One tick of program audio to encode, owned (the bake consumer has moved on):
/// the post-loudnorm program block's interleaved samples and, per selected
/// discrete track, that source's interleaved block (or `None` = silence).
/// `frames` is the tick's exact sample budget, shared by every stream so each
/// stream's sample counter (its PTS) stays locked to the tick counter.
#[derive(Debug, Clone)]
pub struct AudioJob {
    /// The output tick this audio belongs to.
    pub tick: u64,
    /// Samples per channel in this tick.
    pub frames: usize,
    /// Interleaved post-loudnorm program samples.
    pub program: Vec<f32>,
    /// Per selected discrete track: interleaved samples, or `None` for silence.
    pub tracks: Vec<Option<Vec<f32>>>,
}

/// The encode side of the audio worker: encode one job's streams, flush at EOF.
/// Production wraps `AudioRenditionEncoder`; tests use a fake.
pub trait AudioEncodeWork: Send + 'static {
    /// Encode the program and every track of `job`, in that order.
    ///
    /// # Errors
    /// A concrete encoder failure.
    fn encode(&mut self, job: &AudioJob) -> Result<Vec<EncodedPacket>, PipelineError>;

    /// Flush every stream. Called once on the worker thread at end of run.
    ///
    /// # Errors
    /// A concrete encoder flush failure.
    fn finish(&mut self) -> Result<Vec<EncodedPacket>, PipelineError>;
}

/// The program-audio encode worker (design note
/// `docs/architecture/audio-encode-worker.md`): AAC-encodes program + discrete
/// tracks OFF the bake consumer, strictly in tick order, and hands each tick's
/// packets to `on_packets(tick, packets)` (which builds and sends the dispatch
/// event). At EOF it flushes and calls `on_finish(tail)`.
pub struct AudioEncodeWorker {
    input: SyncSender<AudioJob>,
    thread: JoinHandle<Result<(), PipelineError>>,
}

impl AudioEncodeWorker {
    /// Spawn the worker thread.
    ///
    /// # Errors
    /// Returns [`PipelineError::Output`] if the thread cannot be spawned.
    pub fn spawn(
        mut work: impl AudioEncodeWork,
        mut on_packets: impl FnMut(u64, Vec<EncodedPacket>) -> Result<(), PipelineError>
            + Send
            + 'static,
        on_finish: impl FnOnce(Vec<EncodedPacket>) -> Result<(), PipelineError> + Send + 'static,
    ) -> Result<Self, PipelineError> {
        let (input, receiver) = mpsc::sync_channel::<AudioJob>(AUDIO_WORKER_QUEUE_TICKS);
        let thread = thread::Builder::new()
            .name("multiview-audio-encode".to_owned())
            .spawn(move || {
                while let Ok(job) = receiver.recv() {
                    let packets = work.encode(&job)?;
                    on_packets(job.tick, packets)?;
                }
                let tail = work.finish()?;
                on_finish(tail)
            })
            .map_err(|err| PipelineError::Output {
                kind: "audio-worker",
                reason: err.to_string(),
            })?;
        Ok(Self { input, thread })
    }

    /// Offer one tick's audio, waiting up to `bound` for a queue slot — the
    /// same cadence-derived horizon as the video workers. Audio cannot be shed
    /// (a missing block would break the PTS-from-tick lock), so a worker that
    /// cannot drain within the horizon fails the run explicitly; it never
    /// waits without bound (invariants #1/#10).
    ///
    /// # Errors
    /// [`OfferRejection::Overloaded`] when the bound elapsed, or
    /// [`OfferRejection::Ended`] when the worker ended (its error surfaces at
    /// [`join`](Self::join)).
    pub fn offer_within(
        &self,
        mut job: AudioJob,
        bound: std::time::Duration,
    ) -> Result<(), OfferRejection> {
        let deadline = std::time::Instant::now() + bound;
        loop {
            match self.input.try_send(job) {
                Ok(()) => return Ok(()),
                Err(TrySendError::Full(returned)) => {
                    job = returned;
                    if std::time::Instant::now() >= deadline {
                        return Err(OfferRejection::Overloaded);
                    }
                    std::thread::sleep(std::time::Duration::from_millis(1));
                }
                Err(TrySendError::Disconnected(_)) => return Err(OfferRejection::Ended),
            }
        }
    }

    /// Close the input, wait for the flush and the terminal callback, and
    /// surface the worker's error.
    ///
    /// # Errors
    /// The encoder's failure, or an error if the worker panicked.
    pub fn join(self) -> Result<(), PipelineError> {
        drop(self.input);
        self.thread.join().map_err(|_| PipelineError::Output {
            kind: "audio-worker",
            reason: "audio encode worker panicked".to_owned(),
        })?
    }
}

#[cfg(test)]
mod audio_worker_tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::{AudioEncodeWork, AudioEncodeWorker, AudioJob, OfferRejection};
    use crate::pipeline::PipelineError;
    use multiview_ffmpeg::EncodedPacket;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    /// Records the (tick, frames, track count) of every job in arrival order.
    struct Recorder {
        seen: Arc<Mutex<Vec<(u64, usize, usize)>>>,
        current: u64,
        stall: Option<Arc<std::sync::Barrier>>,
    }
    impl AudioEncodeWork for Recorder {
        fn encode(&mut self, job: &AudioJob) -> Result<Vec<EncodedPacket>, PipelineError> {
            if let Some(b) = self.stall.take() {
                b.wait();
            }
            self.current = job.tick;
            self.seen
                .lock()
                .unwrap()
                .push((job.tick, job.frames, job.tracks.len()));
            Ok(Vec::new())
        }
        fn finish(&mut self) -> Result<Vec<EncodedPacket>, PipelineError> {
            self.seen.lock().unwrap().push((u64::MAX, 0, 0));
            Ok(Vec::new())
        }
    }

    fn job(tick: u64) -> AudioJob {
        AudioJob {
            tick,
            frames: 1602,
            program: vec![0.0; 1602 * 2],
            tracks: vec![Some(vec![0.0; 1602 * 2]), None],
        }
    }

    #[test]
    fn jobs_encode_strictly_in_tick_order_then_flush_once() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let dispatched = Arc::new(Mutex::new(Vec::new()));
        let d = Arc::clone(&dispatched);
        let finished = Arc::new(Mutex::new(false));
        let f = Arc::clone(&finished);
        let worker = AudioEncodeWorker::spawn(
            Recorder {
                seen: Arc::clone(&seen),
                current: 0,
                stall: None,
            },
            move |tick, _| {
                d.lock().unwrap().push(tick);
                Ok(())
            },
            move |_| {
                *f.lock().unwrap() = true;
                Ok(())
            },
        )
        .unwrap();
        for tick in 0..100 {
            worker
                .offer_within(job(tick), Duration::from_secs(5))
                .unwrap();
        }
        worker.join().unwrap();
        let seen = seen.lock().unwrap().clone();
        assert_eq!(seen.len(), 101, "100 jobs then one flush");
        for (i, (tick, frames, tracks)) in seen.iter().take(100).enumerate() {
            assert_eq!(*tick, u64::try_from(i).unwrap(), "strict tick order");
            assert_eq!((*frames, *tracks), (1602, 2));
        }
        assert_eq!(seen.last().unwrap().0, u64::MAX, "flush comes last");
        assert_eq!(*dispatched.lock().unwrap(), (0..100).collect::<Vec<_>>());
        assert!(*finished.lock().unwrap(), "terminal callback ran");
    }

    #[test]
    fn a_wedged_worker_fails_the_offer_within_the_bound_never_waits_forever() {
        let gate = Arc::new(std::sync::Barrier::new(2));
        let worker = AudioEncodeWorker::spawn(
            Recorder {
                seen: Arc::new(Mutex::new(Vec::new())),
                current: 0,
                stall: Some(Arc::clone(&gate)),
            },
            |_, _| Ok(()),
            |_| Ok(()),
        )
        .unwrap();
        // The worker takes job 0 and wedges; the queue fills behind it.
        let mut outcome = Ok(());
        let started = std::time::Instant::now();
        for tick in 0..=u64::try_from(super::AUDIO_WORKER_QUEUE_TICKS).unwrap() + 1 {
            outcome = worker.offer_within(job(tick), Duration::from_millis(50));
            if outcome.is_err() {
                break;
            }
        }
        assert_eq!(outcome, Err(OfferRejection::Overloaded));
        assert!(started.elapsed() < Duration::from_secs(2), "bounded wait");
        gate.wait(); // release the worker so it can drain and exit
        worker.join().unwrap();
    }

    /// EOF contract (review AW-1): joining while the consumer is still
    /// draining a bounded event channel must not deadlock — the worker's tail
    /// send blocks until the drainer takes it.
    #[test]
    fn join_completes_while_a_bounded_event_channel_is_drained_concurrently() {
        let (tx, rx) = std::sync::mpsc::sync_channel::<u64>(1);
        let tail_tx = tx.clone();
        let worker = AudioEncodeWorker::spawn(
            Recorder {
                seen: Arc::new(Mutex::new(Vec::new())),
                current: 0,
                stall: None,
            },
            move |tick, _| {
                tx.send(tick).map_err(|_| PipelineError::Output {
                    kind: "audio-dispatch",
                    reason: "closed".to_owned(),
                })
            },
            move |_| {
                tail_tx.send(u64::MAX).map_err(|_| PipelineError::Output {
                    kind: "audio-dispatch",
                    reason: "closed".to_owned(),
                })
            },
        )
        .unwrap();
        for tick in 0..6 {
            worker
                .offer_within(job(tick), Duration::from_secs(5))
                .unwrap();
        }
        // Join on a helper while this thread drains (the pipeline's EOF shape).
        let joiner = std::thread::spawn(move || worker.join());
        let mut got = Vec::new();
        while let Ok(v) = rx.recv_timeout(Duration::from_secs(5)) {
            got.push(v);
            if v == u64::MAX {
                break;
            }
        }
        joiner.join().unwrap().unwrap();
        assert_eq!(
            got,
            vec![0, 1, 2, 3, 4, 5, u64::MAX],
            "every tick then the tail"
        );
    }

    #[test]
    fn an_encoder_error_surfaces_at_join_and_ends_further_offers() {
        struct Failing;
        impl AudioEncodeWork for Failing {
            fn encode(&mut self, _: &AudioJob) -> Result<Vec<EncodedPacket>, PipelineError> {
                Err(PipelineError::Output {
                    kind: "encode-audio",
                    reason: "boom".to_owned(),
                })
            }
            fn finish(&mut self) -> Result<Vec<EncodedPacket>, PipelineError> {
                Ok(Vec::new())
            }
        }
        let worker = AudioEncodeWorker::spawn(Failing, |_, _| Ok(()), |_| Ok(())).unwrap();
        worker.offer_within(job(0), Duration::from_secs(1)).unwrap();
        // The thread exits on the error; later offers see Ended (or fill-then-Ended).
        let mut ended = false;
        for tick in 1..50 {
            if worker.offer_within(job(tick), Duration::from_millis(20))
                == Err(OfferRejection::Ended)
            {
                ended = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(ended, "a dead worker reports Ended");
        assert!(worker.join().is_err(), "the encoder error surfaces at join");
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::{
        next_frame_lower_bound_ns, next_tick_lower_bound_ns, BakedTick, DispatchEvent,
        OfferRejection, RenditionWorker, ReorderBound, VideoEncodeWorker, WatermarkTracker,
        WORKER_QUEUE_TICKS,
    };
    use crate::pipeline::PipelineError;
    use multiview_compositor::pipeline::Nv12Image;
    use multiview_core::time::Rational;
    use multiview_ffmpeg::EncodedPacket;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::mpsc;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    const CADENCE: Rational = Rational::new(30_000, 1001);
    const TIMEBASE: Rational = Rational::new(1, 90_000);

    fn canvas() -> Arc<Nv12Image> {
        Arc::new(
            Nv12Image::solid(
                16,
                16,
                16,
                128,
                128,
                multiview_core::color::ColorInfo::default(),
            )
            .expect("canvas"),
        )
    }

    fn packet(pts: i64) -> EncodedPacket {
        let mut raw = ffmpeg_next::codec::packet::Packet::new(16);
        raw.set_pts(Some(pts));
        EncodedPacket::from_packet(raw)
    }

    /// A zero-reorder counter encoder that emits one packet per tick.
    struct Counter {
        frames: Arc<AtomicUsize>,
        bound: ReorderBound,
    }

    impl VideoEncodeWorker for Counter {
        fn encode(&mut self, item: BakedTick) -> Result<Vec<EncodedPacket>, PipelineError> {
            self.frames.fetch_add(1, Ordering::SeqCst);
            Ok(vec![packet(i64::try_from(item.tick).unwrap_or(i64::MAX))])
        }

        fn reorder_bound(&self) -> ReorderBound {
            self.bound
        }

        fn finish(&mut self) -> Result<Vec<EncodedPacket>, PipelineError> {
            Ok(vec![packet(9_000)])
        }
    }

    /// A delayed encoder: the packet for tick N is only released on call N+1,
    /// and the last one only at `finish()`. Its bound is unprovable.
    struct Delayed {
        pending: Option<i64>,
    }

    impl VideoEncodeWorker for Delayed {
        fn encode(&mut self, item: BakedTick) -> Result<Vec<EncodedPacket>, PipelineError> {
            let previous = self.pending.take();
            self.pending = Some(i64::try_from(item.tick).unwrap_or(i64::MAX));
            Ok(previous.map(|pts| vec![packet(pts)]).unwrap_or_default())
        }

        fn reorder_bound(&self) -> ReorderBound {
            ReorderBound::Unknown
        }

        fn finish(&mut self) -> Result<Vec<EncodedPacket>, PipelineError> {
            Ok(self
                .pending
                .take()
                .map(|pts| vec![packet(pts)])
                .unwrap_or_default())
        }
    }

    /// A stalled encoder: blocks until the test releases it.
    struct Stalled {
        gate: mpsc::Receiver<()>,
    }

    impl VideoEncodeWorker for Stalled {
        fn encode(&mut self, _item: BakedTick) -> Result<Vec<EncodedPacket>, PipelineError> {
            let _ = self.gate.recv();
            Ok(Vec::new())
        }

        fn reorder_bound(&self) -> ReorderBound {
            ReorderBound::Unknown
        }

        fn finish(&mut self) -> Result<Vec<EncodedPacket>, PipelineError> {
            Ok(Vec::new())
        }
    }

    #[test]
    fn three_bindings_share_one_canvas_and_open_two_workers() {
        let frames = Arc::new(AtomicUsize::new(0));
        let opens = Arc::new(AtomicUsize::new(0));
        let events = Arc::new(Mutex::new(Vec::new()));
        let canvas = canvas();
        let mut workers = Vec::new();
        // SRT1 HEVC + SRT2/HLS H.264: two distinct keys, three bindings.
        for key_index in 0..2 {
            opens.fetch_add(1, Ordering::SeqCst);
            let sink = Arc::clone(&events);
            workers.push(
                RenditionWorker::spawn(
                    key_index,
                    CADENCE,
                    TIMEBASE,
                    Box::new(Counter {
                        frames: Arc::clone(&frames),
                        bound: ReorderBound::NoReorder,
                    }),
                    move |event| {
                        sink.lock().expect("lock").push(event);
                        Ok(())
                    },
                )
                .expect("worker"),
            );
        }
        assert_eq!(opens.load(Ordering::SeqCst), 2);
        for worker in &workers {
            worker
                .offer(BakedTick {
                    tick: 0,
                    canvas: Arc::clone(&canvas),
                })
                .expect("offer");
        }
        for worker in workers {
            worker.join().expect("join");
        }
        // One encode per distinct rendition, not per binding.
        assert_eq!(frames.load(Ordering::SeqCst), 2);
        // The single baked allocation is released once every worker is done.
        assert_eq!(Arc::strong_count(&canvas), 1);
        let events = events.lock().expect("lock");
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, DispatchEvent::Video { .. }))
                .count(),
            2
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, DispatchEvent::FinishVideo { .. }))
                .count(),
            2
        );
    }

    #[test]
    fn zero_reorder_watermark_is_exact_exclusive_and_monotonic() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&events);
        let worker = RenditionWorker::spawn(
            0,
            CADENCE,
            TIMEBASE,
            Box::new(Counter {
                frames: Arc::new(AtomicUsize::new(0)),
                bound: ReorderBound::NoReorder,
            }),
            move |event| {
                sink.lock().expect("lock").push(event);
                Ok(())
            },
        )
        .expect("worker");
        let canvas = canvas();
        for tick in 0..3 {
            worker
                .offer(BakedTick {
                    tick,
                    canvas: Arc::clone(&canvas),
                })
                .expect("offer");
        }
        worker.join().expect("join");

        // floor(1 * 1001 * 1e9 / 30000) = 33366666
        let expected = next_tick_lower_bound_ns(0, CADENCE).expect("bound");
        assert_eq!(expected.complete_before_ns, 33_366_666);
        let events = events.lock().expect("lock");
        let marks: Vec<i128> = events
            .iter()
            .filter_map(|event| match event {
                DispatchEvent::Video {
                    watermark: Some(mark),
                    ..
                } => Some(mark.complete_before_ns),
                _ => None,
            })
            .collect();
        assert_eq!(marks, vec![33_366_666, 66_733_333, 100_100_000]);
        assert!(marks.windows(2).all(|pair| pair[0] < pair[1]));
        // The terminal event is EOF completeness, not a watermark claim.
        assert!(matches!(
            events.last().expect("terminal"),
            DispatchEvent::FinishVideo { packets, .. } if packets.len() == 1
        ));
    }

    #[test]
    fn unproven_reorder_never_publishes_a_watermark_and_defers_the_tail() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&events);
        let worker = RenditionWorker::spawn(
            0,
            CADENCE,
            TIMEBASE,
            Box::new(Delayed { pending: None }),
            move |event| {
                sink.lock().expect("lock").push(event);
                Ok(())
            },
        )
        .expect("worker");
        let canvas = canvas();
        for tick in 0..3 {
            worker
                .offer(BakedTick {
                    tick,
                    canvas: Arc::clone(&canvas),
                })
                .expect("offer");
        }
        worker.join().expect("join");
        let events = events.lock().expect("lock");
        // Every intermediate event is explicitly unproven: closing a window on
        // the tick number would have been wrong for ticks 0 and 1.
        let video: Vec<&DispatchEvent> = events
            .iter()
            .filter(|event| matches!(event, DispatchEvent::Video { .. }))
            .collect();
        assert_eq!(video.len(), 3);
        for event in &video {
            match event {
                DispatchEvent::Video { watermark, .. } => assert!(watermark.is_none()),
                _ => panic!("unexpected worker event"),
            }
        }
        // Tick 0's packet is released during call 1; tick 2's only at finish.
        match video[0] {
            DispatchEvent::Video { packets, .. } => assert!(packets.is_empty()),
            _ => panic!("unexpected worker event"),
        }
        match events.last().expect("terminal") {
            DispatchEvent::FinishVideo { packets, .. } => assert_eq!(packets.len(), 1),
            _ => panic!("unexpected worker event"),
        }
    }

    #[test]
    fn overload_is_reported_promptly_without_waiting() {
        let (release, gate) = mpsc::channel();
        let worker =
            RenditionWorker::spawn(0, CADENCE, TIMEBASE, Box::new(Stalled { gate }), |_| Ok(()))
                .expect("worker");
        let canvas = canvas();
        // The stalled worker holds one item and its bounded queue holds at most
        // `WORKER_QUEUE_TICKS` more (the exact split depends on whether the
        // thread has already picked up its first item), so the next offer must
        // be rejected rather than waited on.
        let started = Instant::now();
        let mut accepted = 0_u64;
        let mut rejection = None;
        for tick in 0..=u64::try_from(WORKER_QUEUE_TICKS).expect("queue cap fits u64") {
            match worker.offer(BakedTick {
                tick,
                canvas: Arc::clone(&canvas),
            }) {
                Ok(()) => accepted = accepted.saturating_add(1),
                Err(error) => {
                    rejection = Some(error);
                    break;
                }
            }
        }
        assert_eq!(rejection, Some(OfferRejection::Overloaded));
        assert!(accepted <= u64::try_from(WORKER_QUEUE_TICKS).expect("queue cap fits u64") + 1);
        assert!(
            started.elapsed() < Duration::from_millis(250),
            "offer must never wait on a stalled encoder"
        );
        drop(release);
        // The worker unblocks on the closed gate and finishes cleanly.
        worker.join().expect("join");
    }

    #[test]
    fn worker_failure_surfaces_on_join() {
        struct Failing;
        impl VideoEncodeWorker for Failing {
            fn encode(&mut self, _item: BakedTick) -> Result<Vec<EncodedPacket>, PipelineError> {
                Err(PipelineError::Output {
                    kind: "test-encoder",
                    reason: "boom".to_owned(),
                })
            }
            fn reorder_bound(&self) -> ReorderBound {
                ReorderBound::NoReorder
            }
            fn finish(&mut self) -> Result<Vec<EncodedPacket>, PipelineError> {
                Ok(Vec::new())
            }
        }
        let worker = RenditionWorker::spawn(0, CADENCE, TIMEBASE, Box::new(Failing), |_| Ok(()))
            .expect("worker");
        worker
            .offer(BakedTick {
                tick: 0,
                canvas: canvas(),
            })
            .expect("offer");
        let error = worker.join().expect_err("encoder failure must surface");
        assert!(matches!(
            error,
            PipelineError::Output {
                kind: "test-encoder",
                ..
            }
        ));
    }

    #[test]
    fn audio_watermarks_are_per_stream_and_never_decrease() {
        // 48000 Hz, 1024-sample frames: after the packet at PTS 1024, the next
        // packet's PTS is 2048 -> floor(2048 * 1e9 / 48000) = 42_666_666 ns.
        let first = next_frame_lower_bound_ns(1024, 1024, 48_000).expect("bound");
        assert_eq!(first.complete_before_ns, 42_666_666);
        let mut tracker = WatermarkTracker::new();
        assert_eq!(tracker.current(), None);
        // A track that emitted nothing this tick withdraws nothing.
        assert_eq!(tracker.advance(None), None);
        assert_eq!(tracker.advance(Some(first)), Some(first));
        assert_eq!(tracker.advance(Some(first)), Some(first));
        let later = next_frame_lower_bound_ns(2048, 1024, 48_000).expect("bound");
        assert_eq!(later.complete_before_ns, 64_000_000);
        assert_eq!(tracker.advance(Some(later)), Some(later));
        // Out-of-order/smaller candidates never move the proof backwards.
        assert_eq!(tracker.advance(Some(first)), Some(later));
        assert_eq!(tracker.current(), Some(later));
    }

    #[test]
    fn degenerate_cadence_proves_nothing() {
        assert!(next_tick_lower_bound_ns(0, Rational::new(0, 1)).is_none());
        assert!(next_tick_lower_bound_ns(0, Rational::new(30_000, 0)).is_none());
        assert!(next_frame_lower_bound_ns(0, 1024, 0).is_none());
        assert!(next_frame_lower_bound_ns(0, 0, 48_000).is_none());
    }
}
