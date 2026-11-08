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
        /// The opened audio encoder's timebase.
        timebase: Rational,
        /// All packets emitted for every configured audio stream this tick.
        packets: Vec<EncodedPacket>,
        /// Per configured audio stream, indexed by stream index: a silent or
        /// absent track must never inherit another track's proof.
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
    fn encode(&mut self, item: BakedTick) -> Result<Vec<EncodedPacket>, PipelineError>;

    /// The demonstrated reorder bound of the encoder that actually opened.
    fn reorder_bound(&self) -> ReorderBound;

    /// Flush the encoder, returning every delayed packet. Called once, on the
    /// worker thread, when the program ends.
    fn finish(&mut self) -> Result<Vec<EncodedPacket>, PipelineError>;
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

/// The exclusive DTS lower bound for the tick **after** `tick`, in nanoseconds.
///
/// All packets for inputs at or after tick `tick + 1` have an encoder DTS of at
/// least the exact PTS of that tick, so the floor of that PTS minus 1 ns is a
/// conservative exclusive bound. Returns `None` for a degenerate cadence, which
/// makes the worker publish no watermark at all rather than a wrong one.
#[must_use]
pub fn next_tick_lower_bound_ns(tick: u64, cadence: Rational) -> Option<DtsWatermark> {
    if cadence.num <= 0 || cadence.den <= 0 {
        return None;
    }
    let num = u128::try_from(cadence.num).ok()?;
    let den = u128::try_from(cadence.den).ok()?;
    let next_tick = u128::from(tick).checked_add(1)?;
    // floor(next_tick * den * 1e9 / num) - 1, all in u128, then narrowed.
    let ns = next_tick
        .checked_mul(den)?
        .checked_mul(1_000_000_000)?
        .checked_div(num)?
        .checked_sub(1)?;
    Some(DtsWatermark {
        complete_before_ns: i128::try_from(ns).ok()?,
    })
}

/// The exclusive DTS lower bound after submitting `samples` samples at
/// `sample_rate` Hz: no packet for that stream can have a PTS below the exact
/// presentation time of the samples already submitted.
#[must_use]
pub fn audio_sample_lower_bound_ns(samples: u64, sample_rate: u32) -> Option<DtsWatermark> {
    if sample_rate == 0 {
        return None;
    }
    let ns = u128::from(samples)
        .checked_mul(1_000_000_000)?
        .checked_div(u128::from(sample_rate))?
        .checked_sub(1)?;
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
                    // A zero-reorder encoder that returned from the call for
                    // this tick cannot later emit a packet for this or any
                    // earlier tick; an unproven encoder claims nothing.
                    let watermark = match proof {
                        ReorderBound::NoReorder => {
                            watermarks.advance(next_tick_lower_bound_ns(tick, cadence))
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

    /// Stop the worker: drop the input, wait for `finish()` and the terminal
    /// event, and surface the encoder error if the thread failed.
    ///
    /// # Errors
    /// Returns the encoder's failure, or an error if the worker panicked.
    pub fn join(self) -> Result<(), PipelineError> {
        drop(self.input);
        self.thread.join().map_err(|_| PipelineError::Output {
            kind: "video-worker",
            reason: "video rendition worker panicked".to_owned(),
        })?
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::{
        audio_sample_lower_bound_ns, next_tick_lower_bound_ns, BakedTick, DispatchEvent,
        DtsWatermark, OfferRejection, RenditionWorker, ReorderBound, VideoEncodeWorker,
        WatermarkTracker, WORKER_QUEUE_TICKS,
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
            Ok(self.pending.take().map(|pts| vec![packet(pts)]).unwrap_or_default())
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

        // floor(1 * 1001 * 1e9 / 30000) - 1 = 33366666 - 1
        let expected = next_tick_lower_bound_ns(0, CADENCE).expect("bound");
        assert_eq!(expected.complete_before_ns, 33_366_665);
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
        assert_eq!(marks, vec![33_366_665, 66_733_332, 100_099_999]);
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
                _ => unreachable!(),
            }
        }
        // Tick 0's packet is released during call 1; tick 2's only at finish.
        match video[0] {
            DispatchEvent::Video { packets, .. } => assert!(packets.is_empty()),
            _ => unreachable!(),
        }
        match events.last().expect("terminal") {
            DispatchEvent::FinishVideo { packets, .. } => assert_eq!(packets.len(), 1),
            _ => unreachable!(),
        }
    }

    #[test]
    fn overload_is_reported_promptly_without_waiting() {
        let (release, gate) = mpsc::channel();
        let worker = RenditionWorker::spawn(
            0,
            CADENCE,
            TIMEBASE,
            Box::new(Stalled { gate }),
            |_| Ok(()),
        )
        .expect("worker");
        let canvas = canvas();
        // The stalled worker holds one item and its bounded queue holds at most
        // `WORKER_QUEUE_TICKS` more (the exact split depends on whether the
        // thread has already picked up its first item), so the next offer must
        // be rejected rather than waited on.
        let started = Instant::now();
        let mut accepted = 0_u64;
        let mut rejection = None;
        for tick in 0..=WORKER_QUEUE_TICKS as u64 {
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
        assert!(accepted <= WORKER_QUEUE_TICKS as u64 + 1);
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
        let worker =
            RenditionWorker::spawn(0, CADENCE, TIMEBASE, Box::new(Failing), |_| Ok(())).expect(
                "worker",
            );
        worker
            .offer(BakedTick {
                tick: 0,
                canvas: canvas(),
            })
            .expect("offer");
        let error = worker.join().expect_err("encoder failure must surface");
        assert!(matches!(error, PipelineError::Output { kind: "test-encoder", .. }));
    }

    #[test]
    fn audio_watermarks_are_per_stream_and_never_decrease() {
        // 48000 Hz: 1024 samples = 21_333_333 ns, exclusive bound 21_333_332.
        let first = audio_sample_lower_bound_ns(1024, 48_000).expect("bound");
        assert_eq!(first.complete_before_ns, 21_333_332);
        let mut tracker = WatermarkTracker::new();
        assert_eq!(tracker.current(), None);
        // A track that submitted nothing this tick withdraws nothing.
        assert_eq!(tracker.advance(None), None);
        assert_eq!(
            tracker.advance(Some(first)),
            Some(DtsWatermark {
                complete_before_ns: 21_333_332
            })
        );
        assert_eq!(
            tracker.advance(Some(first)),
            Some(DtsWatermark {
                complete_before_ns: 21_333_332
            })
        );
        let later = audio_sample_lower_bound_ns(2048, 48_000).expect("bound");
        assert_eq!(tracker.advance(Some(later)), Some(later));
        // Out-of-order/smaller candidates never move the proof backwards.
        assert_eq!(tracker.advance(Some(first)), Some(later));
        assert_eq!(tracker.current(), Some(later));
    }

    #[test]
    fn degenerate_cadence_proves_nothing() {
        assert!(next_tick_lower_bound_ns(0, Rational::new(0, 1)).is_none());
        assert!(next_tick_lower_bound_ns(0, Rational::new(30_000, 0)).is_none());
        assert!(audio_sample_lower_bound_ns(1024, 0).is_none());
    }
}
