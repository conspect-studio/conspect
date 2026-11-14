//! **GPU work off the output-clock thread** (issue #415, ADR-0109 §9).
//!
//! #395 bounded the GPU *readback* wait, but four GPU hazards stayed on the
//! clock thread: a device-lost `Device::poll` panic (wgpu 30
//! `handle_error_fatal`), a driver-side `queue.submit` / `write_texture` that
//! does not return, the CPU-fallback composite, and teardown (`wgpu::Queue`
//! drop waits for the device to idle). This module moves all of them off it:
//!
//! * [`Worker`]: one **prewarmed** named thread per job kind with a one-slot
//!   handoff (a `Mutex` + `Condvar`; no per-tick thread or channel). The clock
//!   hands a frame job over and waits for it **at most** a bounded time. A job
//!   still running when the clock gives up makes the worker *busy*: later
//!   frames fail fast at once (never wait), and its late result is discarded
//!   (never emitted under another tick). A panic inside a job is caught on the
//!   worker (`catch_unwind`) and reported, never unwound into the clock.
//! * [`OffClockGpu`]: a GPU backend (the wgpu compositor, or the GPU-free
//!   [`crate::readback::SimulatedGpu`] chaos seam) driven through a GPU worker
//!   with a CPU-reference fallback worker. A lost device (callback, uncaptured
//!   device error, or a panic out of a device call) is permanent: every later
//!   frame is composited on the CPU fallback worker, counted. The GPU
//!   composer is only ever dropped on its own worker thread, and the worker
//!   is never joined, so a hung GPU can never wedge the clock thread or
//!   program teardown.

use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use crate::blend::LinearRgba;
use crate::error::{Error, Result};
use crate::mask::WipeMask;
use crate::pipeline::{CanvasColor, Nv12Image, Tile};
use crate::readback::{GpuHealth, STALL_HOLD, UNCLOCKED_READBACK_DEADLINE};

/// The longest the clock waits for one GPU frame job (upload + submit +
/// bounded readback) on a clocked output: 15 ms. The readback inside the job
/// keeps its own `min(10 ms, period / 2)` deadline from submit (#395).
pub const MAX_GPU_JOB_WAIT: Duration = Duration::from_millis(15);

/// The whole per-frame compose budget of a compositor whose frame period was
/// never set (an off-clock caller: a test, a one-shot render): twice the
/// unclocked readback deadline, so the job's own 2 s readback bound fires
/// first.
pub const UNCLOCKED_FRAME_BUDGET: Duration = Duration::from_secs(4);

/// The clock's whole compose budget for one frame (GPU attempt + CPU
/// fallback), measured from the composite call: three quarters of the frame
/// `period`, leaving a quarter for wake, hook and projection.
/// [`UNCLOCKED_FRAME_BUDGET`] for a zero (never set) period.
#[must_use]
pub fn frame_budget(period: Duration) -> Duration {
    if period.is_zero() {
        return UNCLOCKED_FRAME_BUDGET;
    }
    period.saturating_mul(3) / 4
}

/// One tile of an off-clock frame job: an owned `Arc` of the source image (no
/// pixel copy) plus its placement, the owned twin of [`Tile`].
#[derive(Debug, Clone)]
pub struct SharedTile {
    /// The source image.
    pub image: Arc<Nv12Image>,
    /// Destination x (pixels).
    pub dst_x: u32,
    /// Destination y (pixels).
    pub dst_y: u32,
    /// Destination width (pixels).
    pub dst_w: u32,
    /// Destination height (pixels).
    pub dst_h: u32,
    /// Uniform tile opacity in `[0, 1]`.
    pub opacity: f32,
    /// Optional wipe mask.
    pub mask: Option<WipeMask>,
}

/// One frame to composite off the clock thread (owned, `Send`).
#[derive(Debug, Clone)]
pub struct FrameJob {
    /// Canvas width (pixels).
    pub canvas_w: u32,
    /// Canvas height (pixels).
    pub canvas_h: u32,
    /// Canvas color.
    pub canvas: CanvasColor,
    /// Background (linear canvas gamut).
    pub background: LinearRgba,
    /// Back-to-front tiles.
    pub tiles: Vec<SharedTile>,
}

impl FrameJob {
    /// The job's tiles as borrowed [`Tile`]s.
    #[must_use]
    pub fn borrowed_tiles(&self) -> Vec<Tile<'_>> {
        self.tiles
            .iter()
            .map(|t| {
                let tile = Tile::scaled(&t.image, t.dst_x, t.dst_y, t.dst_w, t.dst_h, t.opacity);
                match t.mask {
                    Some(mask) => tile.with_mask(mask),
                    None => tile,
                }
            })
            .collect()
    }

    /// Composite this job through `composer`.
    fn run<C: FrameComposer + ?Sized>(&self, composer: &C) -> Result<Nv12Image> {
        let tiles = self.borrowed_tiles();
        composer.compose_tiles(
            self.canvas_w,
            self.canvas_h,
            self.canvas,
            self.background,
            &tiles,
        )
    }
}

/// Something that composites one frame (`Send + Sync`, shared with a worker).
pub trait FrameComposer: Send + Sync + 'static {
    /// Composite `tiles` exactly like [`crate::pipeline::composite`].
    ///
    /// # Errors
    ///
    /// The compositor's own errors.
    fn compose_tiles(
        &self,
        canvas_w: u32,
        canvas_h: u32,
        canvas: CanvasColor,
        background: LinearRgba,
        tiles: &[Tile<'_>],
    ) -> Result<Nv12Image>;
}

/// A GPU compositor [`OffClockGpu`] can drive.
pub trait GpuComposer: FrameComposer {
    /// Bound the GPU readback by the output frame period (#395).
    fn set_frame_period(&self, period: Duration);
    /// Readbacks the #395 gate reported as missed.
    fn gate_misses(&self) -> u64;
    /// The device health the driver callbacks write to.
    fn health(&self) -> Arc<GpuHealth>;
    /// Extra cost each CPU-fallback frame pays (chaos seam only).
    fn cpu_fallback_cost(&self) -> Option<Arc<AtomicU64>> {
        None
    }
}

/// The CPU reference fallback, run on its own worker.
#[derive(Debug, Default)]
pub(crate) struct CpuFallback {
    cpu: crate::backend::CpuBackend,
    cost_nanos: Option<Arc<AtomicU64>>,
}

impl FrameComposer for CpuFallback {
    fn compose_tiles(
        &self,
        canvas_w: u32,
        canvas_h: u32,
        canvas: CanvasColor,
        background: LinearRgba,
        tiles: &[Tile<'_>],
    ) -> Result<Nv12Image> {
        if let Some(cost) = &self.cost_nanos {
            let nanos = cost.load(Ordering::Relaxed);
            if nanos > 0 {
                std::thread::sleep(Duration::from_nanos(nanos));
            }
        }
        self.cpu
            .composite_persistent(canvas_w, canvas_h, canvas, background, tiles)
    }
}

/// The outcome of one [`Worker::run`].
#[derive(Debug)]
pub enum Outcome {
    /// The job finished within the wait.
    Done(Result<Nv12Image>),
    /// The job panicked (caught on the worker); the message.
    Panicked(String),
    /// The wait elapsed with the job still running (the worker is now busy).
    TimedOut,
    /// An earlier job is still running, for this long; nothing was handed
    /// over and nothing was waited for.
    Busy(Duration),
}

#[derive(Debug, Default)]
struct Slot {
    job: Option<(u64, Arc<FrameJob>)>,
    result: Option<(u64, Outcome)>,
    /// When the job now running (or queued) was handed over.
    busy_since: Option<Instant>,
    seq: u64,
    stop: bool,
}

#[derive(Debug, Default)]
struct Shared {
    slot: Mutex<Slot>,
    job_ready: Condvar,
    job_done: Condvar,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, Slot> {
        self.slot.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// A prewarmed worker thread owning one [`FrameComposer`] (see the module
/// docs). The clock side keeps an `Arc` only to read the composer's counters;
/// on drop it is handed back to the worker so the composer is dropped there.
pub struct Worker<C: FrameComposer> {
    shared: Arc<Shared>,
    /// Whether a frame finds the worker busy waits (within its own budget)
    /// for the running job and takes its result (the CPU fallback), instead
    /// of failing fast and discarding it (the GPU).
    adopt_late: bool,
    /// `None` only during drop.
    composer: Option<Arc<C>>,
    /// The retire channel: the clock's `Arc` travels here on drop.
    retire: Arc<Mutex<Option<Arc<C>>>>,
    /// `false` when the thread could not be spawned: jobs then run inline.
    threaded: bool,
}

impl<C: FrameComposer> std::fmt::Debug for Worker<C> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Worker")
            .field("threaded", &self.threaded)
            .finish_non_exhaustive()
    }
}

/// The text of a caught panic payload.
fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_owned()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "non-string panic payload".to_owned()
    }
}

/// Run `f` and turn a panic into [`Outcome::Panicked`].
fn guarded(f: impl FnOnce() -> Result<Nv12Image>) -> Outcome {
    match std::panic::catch_unwind(AssertUnwindSafe(f)) {
        Ok(result) => Outcome::Done(result),
        Err(payload) => Outcome::Panicked(panic_message(payload.as_ref())),
    }
}

impl<C: FrameComposer> Worker<C> {
    /// Spawn the worker thread `name` owning `composer`. If the thread cannot
    /// be spawned the worker runs jobs inline on the caller (logged once).
    pub fn spawn(name: &str, composer: Arc<C>) -> Self {
        Self::spawn_with(name, composer, false)
    }

    /// As [`Worker::spawn`]; `adopt_late` makes a busy worker's running job
    /// adoptable (see [`Worker::run`]).
    pub fn spawn_with(name: &str, composer: Arc<C>, adopt_late: bool) -> Self {
        let shared = Arc::new(Shared::default());
        let retire: Arc<Mutex<Option<Arc<C>>>> = Arc::new(Mutex::new(None));
        let thread_shared = Arc::clone(&shared);
        let thread_composer = Arc::clone(&composer);
        let thread_retire = Arc::clone(&retire);
        let spawned = std::thread::Builder::new()
            .name(name.to_owned())
            .spawn(move || worker_loop(&thread_shared, thread_composer, &thread_retire));
        let threaded = match spawned {
            Ok(_detached) => true,
            Err(e) => {
                tracing::warn!(error = %e, worker = name, "could not spawn compose worker; composing inline");
                false
            }
        };
        Self {
            shared,
            adopt_late,
            composer: Some(composer),
            retire,
            threaded,
        }
    }

    /// The composer (for reading its counters).
    ///
    /// # Panics
    ///
    /// Never: the composer is only taken during drop.
    #[must_use]
    pub fn composer(&self) -> &C {
        match self.composer.as_deref() {
            Some(c) => c,
            None => unreachable_composer(),
        }
    }

    /// Hand `job` to the worker and wait for it at most `wait`. Never blocks
    /// longer than `wait` (plus a lock held only for slot bookkeeping).
    ///
    /// While an earlier job still runs, nothing is handed over: a worker
    /// spawned without `adopt_late` returns [`Outcome::Busy`] at once (and
    /// the late result is later discarded); one spawned with it waits, still
    /// at most `wait`, for that running job and returns ITS result (a frame
    /// composed from the previous tick's inputs), or [`Outcome::Busy`] if it
    /// does not finish in time. That keeps a fallback slower than one tick
    /// delivering every other frame instead of none.
    #[must_use]
    pub fn run(&self, job: &Arc<FrameJob>, wait: Duration) -> Outcome {
        if !self.threaded {
            let composer = self.composer();
            return guarded(|| job.run(composer));
        }
        let due = Instant::now().checked_add(wait);
        let mut slot = self.shared.lock();
        if let Some(since) = slot.busy_since {
            if !self.adopt_late {
                return Outcome::Busy(since.elapsed());
            }
            let running = slot.seq;
            return match self.wait_for(slot, running, due) {
                Some(outcome) => outcome,
                None => Outcome::Busy(since.elapsed()),
            };
        }
        // A late result of an abandoned job is stale: drop it.
        slot.result = None;
        slot.seq = slot.seq.wrapping_add(1);
        let seq = slot.seq;
        slot.job = Some((seq, Arc::clone(job)));
        slot.busy_since = Some(Instant::now());
        self.shared.job_ready.notify_one();
        self.wait_for(slot, seq, due).unwrap_or(Outcome::TimedOut)
    }

    /// Wait until `due` for the result of job `seq`; `None` on timeout.
    fn wait_for(
        &self,
        mut slot: MutexGuard<'_, Slot>,
        seq: u64,
        due: Option<Instant>,
    ) -> Option<Outcome> {
        loop {
            if slot.result.as_ref().is_some_and(|(s, _)| *s == seq) {
                if let Some((_, outcome)) = slot.result.take() {
                    return Some(outcome);
                }
            }
            let now = Instant::now();
            let remaining = match due {
                Some(due) if due > now => due - now,
                Some(_) => return None,
                None => Duration::from_secs(3600),
            };
            slot = match self.shared.job_done.wait_timeout(slot, remaining) {
                Ok((guard, _)) => guard,
                Err(poisoned) => poisoned.into_inner().0,
            };
        }
    }

    /// Whether a job is running on the worker now.
    #[must_use]
    pub fn is_busy(&self) -> bool {
        self.shared.lock().busy_since.is_some()
    }
}

#[cold]
fn unreachable_composer() -> ! {
    // The composer is only `None` inside `Drop::drop`, which never calls
    // `composer()`. Kept as an abort-free typed path for the lint set.
    #[allow(clippy::panic)] // reason: provably unreachable (see above)
    {
        panic!("compose worker used after drop")
    }
}

impl<C: FrameComposer> Drop for Worker<C> {
    fn drop(&mut self) {
        let Some(composer) = self.composer.take() else {
            return;
        };
        if !self.threaded {
            return;
        }
        // Hand our `Arc` to the worker, then stop it. The worker drops every
        // `Arc` it holds on its own thread when it leaves its loop; it is never
        // joined, so a composer stuck in a hung device call wedges only that
        // detached thread (issue #415).
        *self.retire.lock().unwrap_or_else(PoisonError::into_inner) = Some(composer);
        let mut slot = self.shared.lock();
        slot.stop = true;
        drop(slot);
        self.shared.job_ready.notify_one();
    }
}

fn worker_loop<C: FrameComposer>(
    shared: &Shared,
    composer: Arc<C>,
    retire: &Mutex<Option<Arc<C>>>,
) {
    loop {
        let mut slot = shared.lock();
        let (seq, job) = loop {
            if slot.stop {
                drop(slot);
                // Teardown happens here, on the worker thread: our `Arc`, and
                // the clock side's (handed over on drop).
                let retired = retire.lock().unwrap_or_else(PoisonError::into_inner).take();
                drop(retired);
                drop(composer);
                return;
            }
            if let Some(job) = slot.job.take() {
                break job;
            }
            slot = shared
                .job_ready
                .wait(slot)
                .unwrap_or_else(PoisonError::into_inner);
        };
        drop(slot);
        let outcome = guarded(|| job.run(composer.as_ref()));
        drop(job);
        let mut slot = shared.lock();
        slot.result = Some((seq, outcome));
        slot.busy_since = None;
        drop(slot);
        shared.job_done.notify_all();
    }
}

/// A GPU backend composited off the clock thread with a CPU fallback worker
/// (see the module docs).
pub struct OffClockGpu<G: GpuComposer> {
    gpu: Worker<G>,
    cpu: Worker<CpuFallback>,
    health: Arc<GpuHealth>,
    period_nanos: AtomicU64,
}

impl<G: GpuComposer> std::fmt::Debug for OffClockGpu<G> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OffClockGpu")
            .field("lost", &self.health.is_lost())
            .finish_non_exhaustive()
    }
}

/// The two worker thread names (one GPU worker and one CPU-fallback worker
/// per GPU backend).
const GPU_WORKER: &str = "gpu-compose";
const CPU_WORKER: &str = "gpu-cpu-fallback";

impl<G: GpuComposer> OffClockGpu<G> {
    /// Drive `gpu` through a prewarmed GPU worker with a CPU fallback worker.
    /// The worker owns `gpu` from here on; a caller may keep only a `Weak`.
    pub fn new(gpu: Arc<G>) -> Self {
        let health = gpu.health();
        let cpu = CpuFallback {
            cpu: crate::backend::CpuBackend::default(),
            cost_nanos: gpu.cpu_fallback_cost(),
        };
        Self {
            gpu: Worker::spawn(GPU_WORKER, gpu),
            cpu: Worker::spawn_with(CPU_WORKER, Arc::new(cpu), true),
            health,
            period_nanos: AtomicU64::new(0),
        }
    }

    /// The GPU composer (counters, tests).
    #[must_use]
    pub fn gpu(&self) -> &G {
        self.gpu.composer()
    }

    /// The device health.
    #[must_use]
    pub fn health(&self) -> &GpuHealth {
        &self.health
    }

    /// Set the output frame period: bounds the readback (#395) and the
    /// clock's per-frame wait.
    pub fn set_frame_period(&self, period: Duration) {
        self.period_nanos.store(
            u64::try_from(period.as_nanos()).unwrap_or(u64::MAX),
            Ordering::Relaxed,
        );
        self.gpu().set_frame_period(period);
    }

    fn period(&self) -> Duration {
        Duration::from_nanos(self.period_nanos.load(Ordering::Relaxed))
    }

    /// Every [`Error::GpuDeadline`] this backend returned (readback gate +
    /// clock-side waits).
    #[must_use]
    pub fn deadline_misses(&self) -> u64 {
        self.gpu()
            .gate_misses()
            .saturating_add(self.health.clock_misses())
    }

    /// Frames dispatched to the CPU fallback.
    #[must_use]
    pub fn cpu_fallback_frames(&self) -> u64 {
        self.health.cpu_fallback_frames()
    }

    fn clock_miss(&self, why: String) -> Error {
        self.health.record_clock_miss();
        Error::GpuDeadline(why)
    }

    fn lost(&self, reason: &str) {
        if self.health.mark_lost(reason) {
            tracing::error!(
                reason,
                "GPU device lost; compositing every later frame on the CPU reference"
            );
        }
    }

    /// The clock path: composite `job` with every wait bounded by
    /// [`frame_budget`] (see the module docs).
    ///
    /// # Errors
    ///
    /// [`Error::GpuDeadline`] when no frame is ready within the budget (the
    /// engine repeats last-good under the tick), or a request error the CPU
    /// reference would reject too.
    pub fn composite_job(&self, job: &Arc<FrameJob>) -> Result<Nv12Image> {
        let start = Instant::now();
        let period = self.period();
        let budget = frame_budget(period);
        if self.health.is_lost() {
            return self.cpu_frame(job, start, budget, "GPU device lost");
        }
        let wait = if period.is_zero() {
            budget
        } else {
            MAX_GPU_JOB_WAIT.min(budget)
        };
        match self.gpu.run(job, wait) {
            Outcome::Done(Ok(image)) => Ok(image),
            Outcome::Done(Err(err @ Error::GpuDeadline(_))) => Err(err),
            Outcome::Done(Err(Error::GpuLost(why))) => {
                self.lost(&why);
                self.cpu_frame(job, start, budget, &why)
            }
            Outcome::Panicked(why) => {
                self.lost(&format!("GPU call panicked: {why}"));
                self.cpu_frame(job, start, budget, &why)
            }
            Outcome::Done(Err(err)) if is_gpu_failure(&err) => {
                self.cpu_frame(job, start, budget, &err.to_string())
            }
            Outcome::Done(other) => other,
            Outcome::TimedOut => Err(self.clock_miss(format!(
                "GPU frame not ready within the clock's {} us wait",
                wait.as_micros()
            ))),
            Outcome::Busy(held) if held >= STALL_HOLD => self.cpu_frame(
                job,
                start,
                budget,
                &format!("GPU busy for {} ms", held.as_millis()),
            ),
            Outcome::Busy(held) => Err(self.clock_miss(format!(
                "GPU still busy with an earlier frame after {} ms",
                held.as_millis()
            ))),
        }
    }

    /// Composite `job` on the CPU fallback worker within what is left of
    /// `budget`.
    fn cpu_frame(
        &self,
        job: &Arc<FrameJob>,
        start: Instant,
        budget: Duration,
        why: &str,
    ) -> Result<Nv12Image> {
        let remaining = budget.saturating_sub(start.elapsed());
        match self.cpu.run(job, remaining) {
            Outcome::Done(Ok(image)) => {
                let frames = self.health.record_cpu_fallback();
                if frames.is_power_of_two() {
                    tracing::warn!(
                        reason = why,
                        fallback_frames = frames,
                        "GPU composite unavailable; composited on the CPU reference (off the clock thread)"
                    );
                }
                Ok(image)
            }
            Outcome::Done(Err(err)) => Err(err),
            Outcome::Panicked(why) => Err(self.clock_miss(format!("CPU fallback panicked: {why}"))),
            Outcome::TimedOut => Err(self.clock_miss(format!(
                "CPU fallback not ready within the clock's {} us wait",
                remaining.as_micros()
            ))),
            Outcome::Busy(held) => Err(self.clock_miss(format!(
                "CPU fallback still busy with an earlier frame after {} ms",
                held.as_millis()
            ))),
        }
    }

    /// The synchronous off-clock path ([`crate::backend::RunBackend::composite`]):
    /// composite on the caller's thread with the same lost-device handling,
    /// panics caught. Used by tests, benches and one-shot renders; the engine's
    /// output clock uses [`Self::composite_job`].
    ///
    /// # Errors
    ///
    /// As [`Self::composite_job`], without the clock-side waits.
    pub fn composite_inline(
        &self,
        canvas_w: u32,
        canvas_h: u32,
        canvas: CanvasColor,
        background: LinearRgba,
        tiles: &[Tile<'_>],
    ) -> Result<Nv12Image> {
        let on_cpu = |why: &str| {
            let image = self
                .cpu
                .composer()
                .compose_tiles(canvas_w, canvas_h, canvas, background, tiles)?;
            let frames = self.health.record_cpu_fallback();
            if frames.is_power_of_two() {
                tracing::warn!(
                    reason = why,
                    fallback_frames = frames,
                    "GPU composite unavailable; composited this frame on the CPU reference"
                );
            }
            Ok(image)
        };
        if self.health.is_lost() {
            return on_cpu("GPU device lost");
        }
        let gpu = self.gpu();
        match guarded(|| gpu.compose_tiles(canvas_w, canvas_h, canvas, background, tiles)) {
            Outcome::Done(Err(Error::GpuLost(why))) => {
                self.lost(&why);
                on_cpu(&why)
            }
            Outcome::Panicked(why) => {
                self.lost(&format!("GPU call panicked: {why}"));
                on_cpu(&why)
            }
            Outcome::Done(Err(err)) if is_gpu_failure(&err) => on_cpu(&err.to_string()),
            Outcome::Done(other) => other,
            Outcome::TimedOut | Outcome::Busy(_) => {
                Err(Error::GpuDeadline("unreachable inline outcome".to_owned()))
            }
        }
    }
}

/// A GPU failure the CPU reference re-composites (one frame): a runtime
/// failure (submit / map / a readback stalled past the hold) or a fixed GPU
/// limit. A deadline miss is not one (the budget is spent), and request
/// errors (geometry, colour) would fail on the CPU too.
fn is_gpu_failure(err: &Error) -> bool {
    match err {
        Error::GpuRuntime(_) => true,
        #[cfg(feature = "wgpu")]
        Error::GpuLimit(_) => true,
        _ => false,
    }
}

/// The unclocked default used by [`frame_budget`] is twice the readback's.
const _: () =
    assert!(UNCLOCKED_FRAME_BUDGET.as_nanos() == 2 * UNCLOCKED_READBACK_DEADLINE.as_nanos());

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;

    /// A composer whose job blocks while `block` is set, or panics.
    #[derive(Default)]
    struct Probe {
        block: Arc<AtomicBool>,
        panic: AtomicBool,
        runs: AtomicU64,
        dropped_on: Arc<Mutex<Option<String>>>,
    }

    impl FrameComposer for Probe {
        fn compose_tiles(
            &self,
            canvas_w: u32,
            canvas_h: u32,
            canvas: CanvasColor,
            background: LinearRgba,
            tiles: &[Tile<'_>],
        ) -> Result<Nv12Image> {
            self.runs.fetch_add(1, Ordering::Relaxed);
            assert!(!self.panic.load(Ordering::Acquire), "probe panic");
            while self.block.load(Ordering::Acquire) {
                std::thread::sleep(Duration::from_millis(1));
            }
            crate::pipeline::composite(canvas_w, canvas_h, canvas, background, tiles)
        }
    }

    impl Drop for Probe {
        fn drop(&mut self) {
            *self.dropped_on.lock().unwrap() = std::thread::current().name().map(str::to_owned);
        }
    }

    fn job() -> Arc<FrameJob> {
        Arc::new(FrameJob {
            canvas_w: 16,
            canvas_h: 16,
            canvas: CanvasColor::default(),
            background: LinearRgba::TRANSPARENT,
            tiles: Vec::new(),
        })
    }

    #[test]
    fn budget_is_three_quarters_of_a_period() {
        assert_eq!(
            frame_budget(Duration::from_millis(40)),
            Duration::from_millis(30)
        );
        assert_eq!(frame_budget(Duration::ZERO), UNCLOCKED_FRAME_BUDGET);
    }

    #[test]
    fn done_timed_out_busy_and_late_results_are_discarded() {
        let probe = Arc::new(Probe::default());
        let worker = Worker::spawn("probe-a", Arc::clone(&probe));
        assert!(matches!(
            worker.run(&job(), Duration::from_secs(5)),
            Outcome::Done(Ok(_))
        ));
        probe.block.store(true, Ordering::Release);
        assert!(matches!(
            worker.run(&job(), Duration::from_millis(5)),
            Outcome::TimedOut
        ));
        // Busy: returns at once and hands nothing over.
        let runs = probe.runs.load(Ordering::Relaxed);
        let t = Instant::now();
        assert!(matches!(
            worker.run(&job(), Duration::from_secs(5)),
            Outcome::Busy(_)
        ));
        assert!(t.elapsed() < Duration::from_secs(2), "busy must not wait");
        assert_eq!(probe.runs.load(Ordering::Relaxed), runs);
        probe.block.store(false, Ordering::Release);
        while worker.is_busy() {
            std::thread::sleep(Duration::from_millis(1));
        }
        // The abandoned job's late result is never returned for a new job:
        // the new job runs (runs + 1) and returns its own result.
        assert!(matches!(
            worker.run(&job(), Duration::from_secs(5)),
            Outcome::Done(Ok(_))
        ));
        assert_eq!(probe.runs.load(Ordering::Relaxed), runs + 1);
    }

    #[test]
    fn an_adopting_worker_hands_the_running_job_result_to_the_next_frame() {
        let probe = Arc::new(Probe::default());
        let block = Arc::clone(&probe.block);
        let worker = Worker::spawn_with("probe-d", Arc::clone(&probe), true);
        block.store(true, Ordering::Release);
        assert!(matches!(
            worker.run(&job(), Duration::from_millis(5)),
            Outcome::TimedOut
        ));
        // Still running: the next frame waits its own bound, then gives up.
        assert!(matches!(
            worker.run(&job(), Duration::from_millis(5)),
            Outcome::Busy(_)
        ));
        let runs = probe.runs.load(Ordering::Relaxed);
        let release = Arc::clone(&block);
        let unblock = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(20));
            release.store(false, Ordering::Release);
        });
        // Finishes within this frame's wait: its result is taken, and no
        // second job was handed over.
        assert!(matches!(
            worker.run(&job(), Duration::from_secs(5)),
            Outcome::Done(Ok(_))
        ));
        assert_eq!(probe.runs.load(Ordering::Relaxed), runs);
        unblock.join().unwrap();
    }

    #[test]
    fn a_panicking_job_is_reported_and_the_worker_survives() {
        let probe = Arc::new(Probe::default());
        let worker = Worker::spawn("probe-b", Arc::clone(&probe));
        probe.panic.store(true, Ordering::Release);
        let outcome = worker.run(&job(), Duration::from_secs(5));
        assert!(
            matches!(&outcome, Outcome::Panicked(m) if m.contains("probe panic")),
            "{outcome:?}"
        );
        probe.panic.store(false, Ordering::Release);
        assert!(matches!(
            worker.run(&job(), Duration::from_secs(5)),
            Outcome::Done(Ok(_))
        ));
    }

    #[test]
    fn drop_never_joins_a_hung_job_and_drops_the_composer_on_the_worker() {
        let probe = Arc::new(Probe::default());
        let dropped_on = Arc::clone(&probe.dropped_on);
        let block = Arc::clone(&probe.block);
        let worker = Worker::spawn("probe-c", probe);
        block.store(true, Ordering::Release);
        assert!(matches!(
            worker.run(&job(), Duration::from_millis(5)),
            Outcome::TimedOut
        ));
        let t = Instant::now();
        drop(worker);
        assert!(t.elapsed() < Duration::from_millis(500), "drop waited");
        assert!(dropped_on.lock().unwrap().is_none());
        block.store(false, Ordering::Release);
        let deadline = Instant::now() + Duration::from_secs(10);
        while dropped_on.lock().unwrap().is_none() {
            assert!(Instant::now() < deadline, "composer never dropped");
            std::thread::sleep(Duration::from_millis(2));
        }
        assert_eq!(dropped_on.lock().unwrap().as_deref(), Some("probe-c"));
    }
}
