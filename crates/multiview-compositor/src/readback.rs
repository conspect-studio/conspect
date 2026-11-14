//! The **bounded GPU readback** state machine (issue #395, ADR-0109 §9).
//!
//! The GPU compositor composites synchronously on the output-clock thread:
//! submit, then wait for the NV12 planes to map back to the host. A hung or slow
//! GPU must never stall Program (invariant #1), so the wait is **bounded**:
//!
//! * **One bounded wait per frame, measured from submit.** [`ReadbackGate::wait`]
//!   polls the in-flight readback until it is ready or [`ReadbackGate::deadline`]
//!   has elapsed since submit. A miss returns [`Error::GpuDeadline`] — the
//!   engine then repeats last-good under this tick (counted), it does not
//!   re-composite on the CPU (the budget is already spent).
//! * **Fail fast while stalled.** The missed readback is kept in a stall slot
//!   (its buffers are mid-map and must not be reused). Every later frame first
//!   calls [`ReadbackGate::admit`], a non-blocking check: while the readback is
//!   outstanding for less than [`STALL_HOLD`] it returns
//!   [`Error::GpuDeadline`] at once (no wait, no GPU work); after
//!   [`STALL_HOLD`] it returns [`Error::GpuRuntime`] so the run backend
//!   composites the frame on the CPU reference (the picture keeps moving on a
//!   GPU that never answers).
//! * **Automatic recovery.** Once the stalled readback completes (or fails), it
//!   is released and the frame composes on the GPU again.
//!
//! The machine is backend-independent ([`PendingReadback`]) so it is tested
//! GPU-free: the wgpu compositor plugs in its buffer maps, and the
//! [`SimulatedGpu`] chaos seam plugs in a fake device whose readback completes
//! `latency` after submit, or never while hung.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use crate::blend::LinearRgba;
use crate::error::{Error, Result};
use crate::pipeline::{CanvasColor, Nv12Image, Tile};

/// The largest readback wait one frame may spend (10 ms).
///
/// The measured 1080p GPU round trip on the reference server is about 4 ms
/// (ADR-0109 §8), so 10 ms is 2.5× healthy headroom while staying well inside
/// a 25–30 fps period. Shorter periods use half the period instead
/// ([`readback_deadline`]).
pub const MAX_READBACK_DEADLINE: Duration = Duration::from_millis(10);

/// The readback deadline of a compositor whose output frame period was never
/// set (2 s): an off-clock caller (a test, a one-shot render) still never waits
/// forever, but a cold pipeline or a software rasteriser is not held to the
/// output-clock budget. The engine always sets the period
/// ([`ReadbackGate::set_frame_period`]) before the clock composes, so Program
/// runs under [`readback_deadline`], never this.
pub const UNCLOCKED_READBACK_DEADLINE: Duration = Duration::from_secs(2);

/// How long a stalled readback makes frames fail fast (repeat last-good)
/// before the run backend starts compositing them on the CPU reference
/// (500 ms: long enough to ride out a transient driver hiccup without paying
/// the CPU composite, short enough that a dead GPU freezes the picture for at
/// most half a second).
pub const STALL_HOLD: Duration = Duration::from_millis(500);

/// The readback deadline for an output `period`: `min(10 ms, period / 2)`.
///
/// Half a period leaves the other half for wake + hook + project, so a miss
/// never makes the tick itself late by more than half a period and never
/// cascades into catch-up repeats (ADR-T018). A zero (unknown) period yields
/// [`MAX_READBACK_DEADLINE`].
#[must_use]
pub fn readback_deadline(period: Duration) -> Duration {
    if period.is_zero() {
        return MAX_READBACK_DEADLINE;
    }
    MAX_READBACK_DEADLINE.min(period / 2)
}

/// One poll of an in-flight readback.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadbackPoll {
    /// Every plane is mapped and can be read.
    Ready,
    /// Still in flight.
    Pending,
    /// The readback failed (a map error, a closed callback channel, or a
    /// non-timeout device poll error).
    Failed(String),
}

/// An in-flight GPU readback the [`ReadbackGate`] can poll.
pub trait PendingReadback {
    /// Drive the device and report the readback state. `wait` bounds how long
    /// this call may block (`None` or zero: do not block at all).
    fn poll(&mut self, wait: Option<Duration>) -> ReadbackPoll;
    /// Whether any part of the readback is still outstanding (a buffer whose
    /// map has not resolved yet must not be reused).
    fn outstanding(&self) -> bool;
    /// Release what is mapped (unmap), discarding the data.
    fn release(&mut self);
}

/// A missed readback held until it resolves.
#[derive(Debug)]
struct Stall<P> {
    pending: P,
    since: Instant,
}

/// The per-compositor stall/deadline state machine (see the module docs).
#[derive(Debug)]
pub struct ReadbackGate<P> {
    deadline_nanos: AtomicU64,
    stall_hold: Duration,
    stall: Mutex<Option<Stall<P>>>,
    misses: AtomicU64,
    stalls: AtomicU64,
}

impl<P> Default for ReadbackGate<P> {
    fn default() -> Self {
        Self::new(UNCLOCKED_READBACK_DEADLINE, STALL_HOLD)
    }
}

/// `Duration` → saturating `u64` nanoseconds.
fn nanos_u64(d: Duration) -> u64 {
    u64::try_from(d.as_nanos()).unwrap_or(u64::MAX)
}

impl<P> ReadbackGate<P> {
    /// A gate with an explicit `deadline` and `stall_hold`.
    #[must_use]
    pub fn new(deadline: Duration, stall_hold: Duration) -> Self {
        Self {
            deadline_nanos: AtomicU64::new(nanos_u64(deadline)),
            stall_hold,
            stall: Mutex::new(None),
            misses: AtomicU64::new(0),
            stalls: AtomicU64::new(0),
        }
    }

    /// The current per-frame readback deadline.
    #[must_use]
    pub fn deadline(&self) -> Duration {
        Duration::from_nanos(self.deadline_nanos.load(Ordering::Relaxed))
    }

    /// Set the deadline from the output frame `period` ([`readback_deadline`]).
    pub fn set_frame_period(&self, period: Duration) {
        self.deadline_nanos
            .store(nanos_u64(readback_deadline(period)), Ordering::Relaxed);
    }

    /// Frames that returned [`Error::GpuDeadline`] (a miss or a fail-fast while
    /// stalled).
    #[must_use]
    pub fn misses(&self) -> u64 {
        self.misses.load(Ordering::Relaxed)
    }

    /// Whether a missed readback is still held.
    #[must_use]
    pub fn is_stalled(&self) -> bool {
        self.lock().is_some()
    }

    /// Lock the stall slot, recovering a poisoned lock (the slot holds plain
    /// state; a panic elsewhere never leaves it half-written).
    fn lock(&self) -> MutexGuard<'_, Option<Stall<P>>> {
        self.stall
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn miss(&self, why: String) -> Error {
        self.misses.fetch_add(1, Ordering::Relaxed);
        Error::GpuDeadline(why)
    }
}

impl<P: PendingReadback> ReadbackGate<P> {
    /// The non-blocking pre-frame check. `Ok(())` when there is no stall (or it
    /// just resolved: the held readback is released and the GPU is used
    /// again). While the stalled readback is outstanding it returns
    /// [`Error::GpuDeadline`] immediately, or [`Error::GpuRuntime`] once it has
    /// been outstanding for the stall hold (the caller composites on the CPU).
    ///
    /// # Errors
    ///
    /// As above; never blocks.
    pub fn admit(&self) -> Result<()> {
        let mut slot = self.lock();
        let Some(stall) = slot.as_mut() else {
            return Ok(());
        };
        let resolved = match stall.pending.poll(None) {
            ReadbackPoll::Ready => true,
            ReadbackPoll::Failed(_) => !stall.pending.outstanding(),
            ReadbackPoll::Pending => false,
        };
        let held = stall.since.elapsed();
        if resolved {
            stall.pending.release();
            *slot = None;
            if self.stalls.load(Ordering::Relaxed).is_power_of_two() {
                tracing::info!(
                    stalled_ms = held.as_millis(),
                    "GPU readback recovered after {} ms",
                    held.as_millis()
                );
            }
            return Ok(());
        }
        drop(slot);
        if held >= self.stall_hold {
            return Err(Error::GpuRuntime(format!(
                "GPU stalled for {} ms (readback still outstanding)",
                held.as_millis()
            )));
        }
        Err(self.miss(format!(
            "readback still outstanding after {} ms",
            held.as_millis()
        )))
    }

    /// Wait, bounded by the deadline measured from `submitted`, for the fresh
    /// readback `pending`. Returns it ready to read.
    ///
    /// # Errors
    ///
    /// [`Error::GpuDeadline`] on a miss (the readback is held as the stall);
    /// [`Error::GpuRuntime`] when the readback failed (held as the stall only
    /// while part of it is still outstanding, else released).
    pub fn wait(&self, mut pending: P, submitted: Instant) -> Result<P> {
        let deadline = self.deadline();
        let due = submitted.checked_add(deadline).unwrap_or(submitted);
        loop {
            let remaining = due.saturating_duration_since(Instant::now());
            match pending.poll(Some(remaining)) {
                ReadbackPoll::Ready => return Ok(pending),
                ReadbackPoll::Failed(why) => {
                    if pending.outstanding() {
                        self.enter_stall(pending);
                    } else {
                        pending.release();
                    }
                    return Err(Error::GpuRuntime(why));
                }
                ReadbackPoll::Pending => {
                    if Instant::now() >= due {
                        self.enter_stall(pending);
                        return Err(self.miss(format!(
                            "readback missed its {} us deadline",
                            deadline.as_micros()
                        )));
                    }
                }
            }
        }
    }

    fn enter_stall(&self, pending: P) {
        let stalls = self
            .stalls
            .fetch_add(1, Ordering::Relaxed)
            .saturating_add(1);
        // A transition, never per tick; a GPU flapping between slow and
        // healthy is rate-limited to power-of-two stall counts.
        if stalls.is_power_of_two() {
            tracing::warn!(
                deadline_us = self.deadline().as_micros(),
                stalls,
                "GPU readback missed its deadline; repeating last-good until it completes"
            );
        }
        let mut slot = self.lock();
        if let Some(mut old) = slot.take() {
            // Unreachable in practice (admit clears the slot before any new
            // submit); never leak a mapped buffer if it happens.
            old.pending.release();
        }
        *slot = Some(Stall {
            pending,
            since: Instant::now(),
        });
    }
}

// --- the GPU-free chaos seam ---------------------------------------------------

/// The fake device shared by a [`SimulatedGpu`] and its control handle.
#[derive(Debug, Default)]
struct SimDevice {
    latency_nanos: AtomicU64,
    hung: AtomicBool,
}

/// A simulated in-flight readback: ready `latency` after submit unless the
/// device is hung.
#[derive(Debug)]
struct SimPending {
    device: Arc<SimDevice>,
    ready_at: Instant,
}

impl PendingReadback for SimPending {
    fn poll(&mut self, wait: Option<Duration>) -> ReadbackPoll {
        let ready =
            |p: &Self| !p.device.hung.load(Ordering::Acquire) && Instant::now() >= p.ready_at;
        if ready(self) {
            return ReadbackPoll::Ready;
        }
        let wait = wait.unwrap_or(Duration::ZERO);
        if wait.is_zero() {
            return ReadbackPoll::Pending;
        }
        // A blocked device wait: up to `wait`, or until the latency elapses.
        let sleep = if self.device.hung.load(Ordering::Acquire) {
            wait
        } else {
            wait.min(self.ready_at.saturating_duration_since(Instant::now()))
        };
        std::thread::sleep(sleep);
        if ready(self) {
            ReadbackPoll::Ready
        } else {
            ReadbackPoll::Pending
        }
    }

    fn outstanding(&self) -> bool {
        true
    }

    fn release(&mut self) {}
}

/// A **test seam** (never constructed in production): composes on the CPU
/// reference but routes every frame through the same [`ReadbackGate`] as the
/// GPU compositor, with a fake device controlled by [`SimulatedGpuControl`].
#[doc(hidden)]
#[derive(Debug)]
pub struct SimulatedGpu {
    device: Arc<SimDevice>,
    gate: ReadbackGate<SimPending>,
    gpu_frames: AtomicU64,
    cpu_fallback_frames: AtomicU64,
}

impl SimulatedGpu {
    /// A simulated GPU whose readback completes `latency` after submit.
    #[must_use]
    pub fn new(latency: Duration) -> Arc<Self> {
        let device = Arc::new(SimDevice::default());
        device
            .latency_nanos
            .store(nanos_u64(latency), Ordering::Relaxed);
        Arc::new(Self {
            device,
            gate: ReadbackGate::default(),
            gpu_frames: AtomicU64::new(0),
            cpu_fallback_frames: AtomicU64::new(0),
        })
    }

    /// Set the readback deadline from the output frame `period`.
    pub fn set_frame_period(&self, period: Duration) {
        self.gate.set_frame_period(period);
    }

    /// Frames that returned [`Error::GpuDeadline`].
    #[must_use]
    pub fn deadline_misses(&self) -> u64 {
        self.gate.misses()
    }

    /// Frames delivered through the simulated GPU.
    #[must_use]
    pub fn gpu_frames(&self) -> u64 {
        self.gpu_frames.load(Ordering::Relaxed)
    }

    /// Frames the run backend re-composited on the CPU.
    #[must_use]
    pub fn cpu_fallback_frames(&self) -> u64 {
        self.cpu_fallback_frames.load(Ordering::Relaxed)
    }

    pub(crate) fn record_cpu_fallback(&self) -> u64 {
        self.cpu_fallback_frames
            .fetch_add(1, Ordering::Relaxed)
            .saturating_add(1)
    }

    /// One "GPU" composite: admit, compose (CPU reference), "submit", then the
    /// bounded readback wait.
    ///
    /// # Errors
    ///
    /// The composite's own errors, [`Error::GpuDeadline`], [`Error::GpuRuntime`].
    pub fn composite(
        &self,
        canvas_w: u32,
        canvas_h: u32,
        canvas: CanvasColor,
        background: LinearRgba,
        tiles: &[Tile<'_>],
    ) -> Result<Nv12Image> {
        self.gate.admit()?;
        let image = crate::pipeline::composite(canvas_w, canvas_h, canvas, background, tiles)?;
        let submitted = Instant::now();
        let latency = Duration::from_nanos(self.device.latency_nanos.load(Ordering::Relaxed));
        let pending = SimPending {
            device: Arc::clone(&self.device),
            ready_at: submitted.checked_add(latency).unwrap_or(submitted),
        };
        self.gate.wait(pending, submitted)?;
        self.gpu_frames.fetch_add(1, Ordering::Relaxed);
        Ok(image)
    }
}

/// Test control for a [`SimulatedGpu`]: change latency, hang and release the
/// fake device, read its counters.
#[doc(hidden)]
#[derive(Debug, Clone)]
pub struct SimulatedGpuControl(pub(crate) Arc<SimulatedGpu>);

impl SimulatedGpuControl {
    /// Readbacks submitted from now on complete `latency` after submit.
    pub fn set_latency(&self, latency: Duration) {
        self.0
            .device
            .latency_nanos
            .store(nanos_u64(latency), Ordering::Relaxed);
    }

    /// Hang the device: no readback completes until [`Self::release`].
    pub fn hang(&self) {
        self.0.device.hung.store(true, Ordering::Release);
    }

    /// Un-hang the device.
    pub fn release(&self) {
        self.0.device.hung.store(false, Ordering::Release);
    }

    /// Frames delivered through the simulated GPU.
    #[must_use]
    pub fn gpu_frames(&self) -> u64 {
        self.0.gpu_frames()
    }

    /// Frames re-composited on the CPU after a stall hold.
    #[must_use]
    pub fn cpu_fallback_frames(&self) -> u64 {
        self.0.cpu_fallback_frames()
    }

    /// Frames that returned [`Error::GpuDeadline`].
    #[must_use]
    pub fn deadline_misses(&self) -> u64 {
        self.0.gate.misses()
    }

    /// Whether a missed readback is still held.
    #[must_use]
    pub fn is_stalled(&self) -> bool {
        self.0.gate.is_stalled()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;

    // Generous slack for a loaded CI runner: only design bounds are asserted.
    const SLACK: Duration = Duration::from_millis(400);

    fn sim(latency: Duration, hold: Duration) -> (ReadbackGate<SimPending>, Arc<SimDevice>) {
        let device = Arc::new(SimDevice::default());
        device
            .latency_nanos
            .store(nanos_u64(latency), Ordering::Relaxed);
        (ReadbackGate::new(Duration::from_millis(10), hold), device)
    }

    fn submit(gate: &ReadbackGate<SimPending>, device: &Arc<SimDevice>) -> Result<SimPending> {
        gate.admit()?;
        let submitted = Instant::now();
        let latency = Duration::from_nanos(device.latency_nanos.load(Ordering::Relaxed));
        gate.wait(
            SimPending {
                device: Arc::clone(device),
                ready_at: submitted + latency,
            },
            submitted,
        )
    }

    #[test]
    fn deadline_is_half_a_period_capped_at_10ms() {
        let ns = |n: u64| Duration::from_nanos(n);
        assert_eq!(readback_deadline(ns(33_366_667)), MAX_READBACK_DEADLINE);
        assert_eq!(readback_deadline(ns(40_000_000)), MAX_READBACK_DEADLINE);
        assert_eq!(readback_deadline(ns(20_000_000)), MAX_READBACK_DEADLINE);
        assert_eq!(readback_deadline(ns(16_666_667)), ns(8_333_333));
        assert_eq!(readback_deadline(ns(8_333_333)), ns(4_166_666));
        assert_eq!(readback_deadline(Duration::ZERO), MAX_READBACK_DEADLINE);
        let gate: ReadbackGate<SimPending> = ReadbackGate::default();
        assert_eq!(gate.deadline(), UNCLOCKED_READBACK_DEADLINE);
        gate.set_frame_period(ns(16_666_667));
        assert_eq!(gate.deadline(), ns(8_333_333));
    }

    #[test]
    fn on_time_readback_is_ok() {
        let (gate, device) = sim(Duration::from_millis(1), STALL_HOLD);
        for _ in 0..5 {
            submit(&gate, &device).unwrap();
        }
        assert_eq!(gate.misses(), 0);
        assert!(!gate.is_stalled());
    }

    #[test]
    fn slow_readback_misses_within_the_deadline_then_recovers() {
        let (gate, device) = sim(Duration::from_millis(60), STALL_HOLD);
        let t = Instant::now();
        let err = submit(&gate, &device).unwrap_err();
        let took = t.elapsed();
        assert!(matches!(err, Error::GpuDeadline(_)), "{err:?}");
        assert!(
            took >= Duration::from_millis(10),
            "returned before the deadline: {took:?}"
        );
        assert!(took < Duration::from_millis(10) + SLACK, "{took:?}");
        assert!(gate.is_stalled());
        assert_eq!(gate.misses(), 1);
        // Outstanding: the next frame fails fast, without waiting.
        let t = Instant::now();
        let err = submit(&gate, &device).unwrap_err();
        assert!(matches!(err, Error::GpuDeadline(_)), "{err:?}");
        assert!(
            t.elapsed() < Duration::from_millis(10),
            "fail-fast waited {:?}",
            t.elapsed()
        );
        assert_eq!(gate.misses(), 2);
        // Once the slow readback lands, the next frame recovers by itself.
        device.latency_nanos.store(1_000_000, Ordering::Relaxed);
        std::thread::sleep(Duration::from_millis(70));
        submit(&gate, &device).unwrap();
        assert!(!gate.is_stalled());
        assert_eq!(gate.misses(), 2);
    }

    #[test]
    fn hung_device_holds_then_hands_frames_to_the_cpu_then_recovers() {
        let hold = Duration::from_millis(50);
        let (gate, device) = sim(Duration::from_millis(1), hold);
        device.hung.store(true, Ordering::Release);
        assert!(matches!(submit(&gate, &device), Err(Error::GpuDeadline(_))));
        assert!(matches!(submit(&gate, &device), Err(Error::GpuDeadline(_))));
        std::thread::sleep(hold);
        let t = Instant::now();
        let err = submit(&gate, &device).unwrap_err();
        assert!(matches!(err, Error::GpuRuntime(_)), "{err:?}");
        assert!(
            t.elapsed() < Duration::from_millis(10),
            "stall hold waited {:?}",
            t.elapsed()
        );
        assert_eq!(
            gate.misses(),
            2,
            "a CPU-fallback frame is not a deadline miss"
        );
        device.hung.store(false, Ordering::Release);
        submit(&gate, &device).unwrap();
        assert!(!gate.is_stalled());
    }

    #[test]
    fn simulated_backend_counts_cpu_fallback_and_recovers() {
        let (backend, control) =
            crate::backend::RunBackend::simulated_gpu(Duration::from_millis(1));
        backend.set_frame_period(Duration::from_millis(40));
        let canvas = CanvasColor::default();
        let run = || backend.composite(16, 16, canvas, LinearRgba::TRANSPARENT, &[]);
        run().unwrap();
        assert_eq!(control.gpu_frames(), 1);
        control.hang();
        assert!(matches!(run(), Err(Error::GpuDeadline(_))));
        std::thread::sleep(STALL_HOLD);
        // Past the hold: the run backend composites THIS frame on the CPU.
        run().unwrap();
        assert_eq!(control.cpu_fallback_frames(), 1);
        assert_eq!(backend.gpu_cpu_fallback_frames(), 1);
        assert_eq!(backend.gpu_deadline_misses(), 1);
        control.release();
        run().unwrap();
        assert_eq!(control.gpu_frames(), 2);
        assert!(!control.is_stalled());
    }
}
