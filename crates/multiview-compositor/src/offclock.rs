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
    /// The scene identity the caller composed this job for (layout
    /// generation, source bindings, transition step). A late CPU-fallback
    /// result is handed to a later frame only when its scene key equals that
    /// frame's (it then differs only by the source frames sampled one tick
    /// earlier); a staged-then-rolled-back scene never reaches the output.
    pub scene: u64,
    /// Caller metadata handed back with an adopted result.
    pub meta: Option<JobMeta>,
}

impl FrameJob {
    /// An owned job from borrowed tiles (copies the tile images: the
    /// off-clock synchronous path only, never the clock).
    #[must_use]
    pub fn from_tiles(
        canvas_w: u32,
        canvas_h: u32,
        canvas: CanvasColor,
        background: LinearRgba,
        tiles: &[Tile<'_>],
    ) -> Self {
        Self {
            canvas_w,
            canvas_h,
            canvas,
            background,
            tiles: tiles
                .iter()
                .map(|t| SharedTile {
                    image: Arc::new(t.image.clone()),
                    dst_x: t.dst_x,
                    dst_y: t.dst_y,
                    dst_w: t.dst_w,
                    dst_h: t.dst_h,
                    opacity: t.opacity,
                    mask: t.mask,
                })
                .collect(),
            scene: 0,
            meta: None,
        }
    }

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

/// Opaque per-job data the caller attaches to a [`FrameJob`] (the drive's
/// source states and times for that frame). An adopted late result hands
/// back the metadata of the job that produced its pixels, never the current
/// frame's (issue #415 review MC-R2-1).
#[derive(Clone)]
pub struct JobMeta(pub Arc<dyn std::any::Any + Send + Sync>);

impl std::fmt::Debug for JobMeta {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("JobMeta")
    }
}

/// A composited frame and, when it is an adopted late result, the metadata
/// of the job that produced it.
#[derive(Debug)]
pub struct JobOutput {
    /// The pixels.
    pub image: Nv12Image,
    /// `Some` only for an adopted earlier job: its [`FrameJob::meta`].
    pub adopted: Option<Option<JobMeta>>,
}

impl JobOutput {
    /// A frame composed for the job it was returned for.
    #[must_use]
    pub fn fresh(image: Nv12Image) -> Self {
        Self {
            image,
            adopted: None,
        }
    }
}

/// The outcome of one [`Worker::run`].
#[derive(Debug)]
pub enum Outcome {
    /// The job finished within the wait.
    Done(Result<Nv12Image>),
    /// An adopting worker returned an EARLIER job's frame (same scene and
    /// generation), with that job's metadata.
    Adopted(Nv12Image, Option<JobMeta>),
    /// The job panicked (caught on the worker); the message.
    Panicked(String),
    /// The wait elapsed with the job still running (the worker is now busy).
    TimedOut,
    /// An earlier job is still running, for this long; nothing was handed
    /// over and nothing was waited for.
    Busy(Duration),
}

/// The identity of a running / finished job for adoption.
#[derive(Debug, Clone)]
struct JobTag {
    seq: u64,
    scene: u64,
    generation: u64,
    meta: Option<JobMeta>,
}

#[derive(Debug, Default)]
struct Slot {
    job: Option<(u64, Arc<FrameJob>)>,
    /// The last finished job and its outcome.
    result: Option<(JobTag, Outcome)>,
    /// The job now running.
    running: Option<JobTag>,
    /// When the job now running (or queued) was handed over.
    busy_since: Option<Instant>,
    seq: u64,
    /// Bumped by [`Worker::invalidate`]: results of older generations are
    /// never adopted.
    generation: u64,
    stop: bool,
    /// The composer built on the worker thread (or why it failed), taken
    /// once by the spawner.
    init: Option<Result<()>>,
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

/// A prewarmed worker thread that BUILDS and owns one [`FrameComposer`]
/// (see the module docs). The composer is constructed on the worker thread
/// and dropped there; the handle keeps only a [`std::sync::Weak`] to read its
/// counters, so no path ever destroys it on the caller's (clock) thread.
pub struct Worker<C: FrameComposer> {
    shared: Arc<Shared>,
    /// Whether a frame finds the worker busy waits (within its own budget)
    /// for the running job and takes its result (the CPU fallback), instead
    /// of failing fast and discarding it (the GPU).
    adopt_late: bool,
    composer: std::sync::Weak<C>,
}

impl<C: FrameComposer> std::fmt::Debug for Worker<C> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Worker")
            .field("adopt_late", &self.adopt_late)
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

/// How long a spawner waits for a worker to BUILD its composer (10 s: a
/// wgpu device request is a driver round trip, normally milliseconds). On
/// timeout the spawn fails and the worker drops whatever it builds later on
/// its own thread.
pub const WORKER_INIT_WAIT: Duration = Duration::from_secs(10);

impl<C: FrameComposer> Worker<C> {
    /// Spawn the worker thread `name`, which builds its composer with `make`.
    ///
    /// # Errors
    ///
    /// [`Error::GpuRuntime`] when the thread cannot be spawned (then `make`
    /// never runs: no device exists), when `make` fails or panics (anything it
    /// built is dropped on the worker), or when it does not finish within
    /// `init_wait`. There is no inline fallback: a composer never runs or
    /// drops on the caller's thread.
    pub fn spawn_with<F>(
        name: &str,
        make: F,
        options: WorkerOptions,
        init_wait: Duration,
    ) -> Result<Self>
    where
        F: FnOnce() -> Result<Arc<C>> + Send + 'static,
    {
        let shared = Arc::new(Shared::default());
        let thread_shared = Arc::clone(&shared);
        let (weak_tx, weak_rx) = std::sync::mpsc::channel::<std::sync::Weak<C>>();
        let WorkerOptions {
            adopt_late,
            panic_marks,
            permit,
        } = options;
        if spawn_should_fail() {
            return Err(Error::GpuRuntime(format!(
                "could not spawn compose worker {name} (injected)"
            )));
        }
        std::thread::Builder::new()
            .name(name.to_owned())
            .spawn(move || {
                // The admission lease lives exactly as long as this thread.
                let _permit = permit;
                let built = match std::panic::catch_unwind(AssertUnwindSafe(make)) {
                    Ok(result) => result,
                    Err(payload) => Err(Error::GpuRuntime(format!(
                        "compose worker init panicked: {}",
                        panic_message(payload.as_ref())
                    ))),
                };
                let mut slot = thread_shared.lock();
                match built {
                    Ok(composer) if !slot.stop => {
                        let _ = weak_tx.send(Arc::downgrade(&composer));
                        slot.init = Some(Ok(()));
                        drop(slot);
                        thread_shared.job_done.notify_all();
                        let marks = panic_marks.as_deref().and_then(std::sync::OnceLock::get);
                        worker_loop(&thread_shared, composer, marks.map(Arc::as_ref));
                    }
                    // The spawner gave up waiting: drop it here.
                    Ok(composer) => {
                        drop(slot);
                        drop(composer);
                    }
                    Err(e) => {
                        slot.init = Some(Err(e));
                        drop(slot);
                        thread_shared.job_done.notify_all();
                    }
                }
            })
            .map_err(|e| {
                Error::GpuRuntime(format!("could not spawn compose worker {name}: {e}"))
            })?;
        let due = Instant::now().checked_add(init_wait);
        let mut slot = shared.lock();
        loop {
            if let Some(init) = slot.init.take() {
                init?;
                break;
            }
            let now = Instant::now();
            let remaining = match due {
                Some(due) if due > now => due - now,
                _ => {
                    slot.stop = true;
                    return Err(Error::GpuRuntime(format!(
                        "compose worker {name} did not build its composer within {} ms",
                        init_wait.as_millis()
                    )));
                }
            };
            slot = match shared.job_done.wait_timeout(slot, remaining) {
                Ok((guard, _)) => guard,
                Err(poisoned) => poisoned.into_inner().0,
            };
        }
        drop(slot);
        let composer = weak_rx
            .try_recv()
            .map_err(|_| Error::GpuRuntime(format!("compose worker {name} lost its composer")))?;
        Ok(Self {
            shared,
            adopt_late,
            composer,
        })
    }

    /// Run `f` on the composer, if it is still alive (counters, settings).
    /// The temporary strong reference is released before returning; the
    /// composer is only ever dropped by the worker, which outlives every
    /// handle method call (it exits only after the handle's drop).
    pub fn with_composer<R>(&self, f: impl FnOnce(&C) -> R) -> Option<R> {
        self.composer.upgrade().map(|c| f(&c))
    }

    /// Make every running or finished job's result non-adoptable (the GPU
    /// delivered a newer frame, review MC-R2-2).
    pub fn invalidate(&self) {
        let mut slot = self.shared.lock();
        slot.generation = slot.generation.wrapping_add(1);
    }

    /// Hand `job` to the worker and wait for it at most `wait`. Never blocks
    /// longer than `wait` (plus a lock held only for slot bookkeeping, never
    /// while a job runs).
    ///
    /// While an earlier job still runs, nothing is handed over. A worker
    /// without `adopt_late` returns [`Outcome::Busy`] at once and later
    /// discards that job's result. An adopting worker (the CPU fallback)
    /// waits, still at most `wait`, for the running job and returns its
    /// frame as [`Outcome::Adopted`] (with that job's metadata) when its
    /// [`FrameJob::scene`] equals `job`'s and no [`Worker::invalidate`]
    /// happened since it was handed over; a result that finished between
    /// two calls is adopted under the same rule. A fallback slower than the
    /// clock's wait therefore still delivers frames; a rolled-back scene, or
    /// a frame older than one the GPU already delivered, never reaches the
    /// output.
    #[must_use]
    pub fn run(&self, job: &Arc<FrameJob>, wait: Duration) -> Outcome {
        let due = Instant::now().checked_add(wait);
        let mut slot = self.shared.lock();
        if let Some(since) = slot.busy_since {
            let adoptable = self.adopt_late
                && slot
                    .running
                    .as_ref()
                    .is_some_and(|t| t.scene == job.scene && t.generation == slot.generation);
            if !adoptable {
                return Outcome::Busy(since.elapsed());
            }
            let running = slot.running.as_ref().map_or(0, |t| t.seq);
            let (outcome, guard) = self.wait_for(slot, running, due);
            slot = guard;
            match outcome {
                Some((tag, Outcome::Done(Ok(image)))) if tag.generation == slot.generation => {
                    return Outcome::Adopted(image, tag.meta);
                }
                // Finished, but not a frame / invalidated meanwhile.
                Some(_) => {}
                None => return Outcome::Busy(since.elapsed()),
            }
        }
        // A finished result of an abandoned job: adopt it when allowed (same
        // scene and generation, a frame), else it is stale.
        if let Some((tag, outcome)) = slot.result.take() {
            if self.adopt_late && tag.scene == job.scene && tag.generation == slot.generation {
                if let Outcome::Done(Ok(image)) = outcome {
                    return Outcome::Adopted(image, tag.meta);
                }
            }
        }
        slot.seq = slot.seq.wrapping_add(1);
        let seq = slot.seq;
        slot.job = Some((seq, Arc::clone(job)));
        slot.running = Some(JobTag {
            seq,
            scene: job.scene,
            generation: slot.generation,
            meta: job.meta.clone(),
        });
        slot.busy_since = Some(Instant::now());
        self.shared.job_ready.notify_one();
        match self.wait_for(slot, seq, due).0 {
            Some((_, outcome)) => outcome,
            None => Outcome::TimedOut,
        }
    }

    /// Wait until `due` for the result of job `seq`; `None` on timeout.
    fn wait_for<'a>(
        &self,
        mut slot: MutexGuard<'a, Slot>,
        seq: u64,
        due: Option<Instant>,
    ) -> (Option<(JobTag, Outcome)>, MutexGuard<'a, Slot>) {
        loop {
            if slot.result.as_ref().is_some_and(|(t, _)| t.seq == seq) {
                if let Some(result) = slot.result.take() {
                    return (Some(result), slot);
                }
            }
            let now = Instant::now();
            let remaining = match due {
                Some(due) if due > now => due - now,
                Some(_) => return (None, slot),
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

impl<C: FrameComposer> Drop for Worker<C> {
    fn drop(&mut self) {
        // Stop the worker. It drops its composer on its own thread when it
        // leaves its loop and is never joined, so a composer stuck in a hung
        // device call wedges only that detached thread (issue #415).
        let mut slot = self.shared.lock();
        slot.stop = true;
        drop(slot);
        self.shared.job_ready.notify_one();
    }
}

/// How a [`Worker`] treats late results, panics and admission.
#[derive(Debug, Default)]
pub struct WorkerOptions {
    /// Adopt a running / finished job's result for a later frame of the same
    /// scene (see [`Worker::run`]).
    pub adopt_late: bool,
    /// Mark this health lost when a job panics (on the worker itself); set
    /// by the composer's builder once the device exists.
    pub panic_marks: Option<Arc<std::sync::OnceLock<Arc<GpuHealth>>>>,
    /// An admission lease released when the LAST worker thread holding it
    /// exits (not when a handle drops): a wedged worker keeps holding it.
    pub permit: Option<Arc<AdmissionPermit>>,
}

/// A process-wide cap on live GPU backends (issue #415 review BR-2).
///
/// A GPU wedged in a driver call keeps its detached worker, the compositor and
/// one [`FrameJob`] alive until the device returns; a CPU fallback job keeps
/// its worker busy until it finishes. Every GPU backend takes one permit
/// before its device exists, shared by both of its worker threads and held
/// until both exit, so repeated construct / wedge / tear-down cycles cannot
/// accumulate threads, devices and memory without bound: once the cap is
/// reached, new GPU backends are refused (the selector uses the CPU
/// reference). Never waits.
#[derive(Debug)]
pub struct Admission {
    live: AtomicU64,
    cap: u64,
}

impl Admission {
    /// An admission pool of `cap` live GPU backends.
    #[must_use]
    pub const fn new(cap: u64) -> Self {
        Self {
            live: AtomicU64::new(0),
            cap,
        }
    }

    /// Take a permit, or `None` at the cap (never waits).
    #[must_use]
    pub fn try_acquire(self: &Arc<Self>) -> Option<AdmissionPermit> {
        let mut live = self.live.load(Ordering::Acquire);
        loop {
            if live >= self.cap {
                return None;
            }
            match self.live.compare_exchange_weak(
                live,
                live.saturating_add(1),
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return Some(AdmissionPermit(Arc::clone(self))),
                Err(now) => live = now,
            }
        }
    }

    /// Live permits.
    #[must_use]
    pub fn live(&self) -> u64 {
        self.live.load(Ordering::Acquire)
    }
}

/// One [`Admission`] permit; released on drop.
#[derive(Debug)]
pub struct AdmissionPermit(Arc<Admission>);

impl Drop for AdmissionPermit {
    fn drop(&mut self) {
        self.0.live.fetch_sub(1, Ordering::AcqRel);
    }
}

/// The most GPU backends alive in one process, wedged ones included (each is
/// one GPU worker and one CPU-fallback worker). A host runs a handful of
/// programs; 8 leaves headroom for migrations while bounding what a
/// repeatedly hanging device can pin.
pub const MAX_LIVE_GPU_WORKERS: u64 = 8;

/// The process-wide GPU backend admission pool.
#[must_use]
pub fn gpu_admission() -> Arc<Admission> {
    static POOL: std::sync::OnceLock<Arc<Admission>> = std::sync::OnceLock::new();
    Arc::clone(POOL.get_or_init(|| Arc::new(Admission::new(MAX_LIVE_GPU_WORKERS))))
}

#[cfg(test)]
thread_local! {
    static FAIL_SPAWN: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

/// Test seam: make the next `n` worker spawns on this thread fail.
#[cfg(test)]
pub(crate) fn fail_spawns_on_this_thread(n: u32) {
    FAIL_SPAWN.with(|f| f.set(n));
}

/// Test seam: skip `skip` spawns on this thread, then fail the next one.
#[cfg(test)]
pub(crate) fn fail_spawn_after(skip: u32) {
    FAIL_SPAWN.with(|f| f.set(skip.saturating_add(1) | (1 << 31)));
}

#[cfg(test)]
fn spawn_should_fail() -> bool {
    FAIL_SPAWN.with(|f| {
        let v = f.get();
        if v & (1 << 31) != 0 {
            let skip = (v & !(1 << 31)).saturating_sub(1);
            if skip == 0 {
                f.set(1);
                return spawn_should_fail_plain(f);
            }
            f.set(skip | (1 << 31));
            return false;
        }
        spawn_should_fail_plain(f)
    })
}

#[cfg(test)]
fn spawn_should_fail_plain(f: &std::cell::Cell<u32>) -> bool {
    let v = f.get();
    if v > 0 {
        f.set(v - 1);
        true
    } else {
        false
    }
}

#[cfg(not(test))]
const fn spawn_should_fail() -> bool {
    false
}

fn worker_loop<C: FrameComposer>(
    shared: &Shared,
    composer: Arc<C>,
    panic_marks: Option<&GpuHealth>,
) {
    loop {
        let mut slot = shared.lock();
        let (seq, job) = loop {
            if slot.stop {
                drop(slot);
                // Teardown happens here, on the worker thread.
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
        // A panic out of a GPU call loses the device for good, recorded here
        // on the worker: whether or not the clock still waits for this job.
        if let (Outcome::Panicked(why), Some(health)) = (&outcome, panic_marks) {
            health.mark_lost(&format!("GPU call panicked: {why}"));
        }
        let mut slot = shared.lock();
        if let Some(tag) = slot.running.take().filter(|t| t.seq == seq) {
            slot.result = Some((tag, outcome));
        }
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
    /// Build a GPU backend whose composer `make` constructs ON its worker
    /// thread, admitted against `admission` before anything is built.
    ///
    /// # Errors
    ///
    /// [`Error::GpuRuntime`] when the admission cap is reached or a worker
    /// thread cannot be spawned; `make`'s own error (e.g. no adapter). The
    /// caller then uses the CPU reference. Whatever `make` built is dropped
    /// on its worker thread, never on the caller's.
    pub fn build<F>(admission: &Arc<Admission>, make: F) -> Result<Self>
    where
        F: FnOnce() -> Result<Arc<G>> + Send + 'static,
    {
        let Some(permit) = admission.try_acquire() else {
            return Err(Error::GpuRuntime(format!(
                "GPU worker admission cap reached ({} live)",
                admission.live()
            )));
        };
        let permit = Arc::new(permit);
        let marks: Arc<std::sync::OnceLock<Arc<GpuHealth>>> = Arc::default();
        let marks_out = Arc::clone(&marks);
        // The GPU worker builds the device on its own thread.
        let gpu = Worker::spawn_with(
            GPU_WORKER,
            move || {
                let gpu = make()?;
                let _ = marks_out.set(gpu.health());
                Ok(gpu)
            },
            WorkerOptions {
                adopt_late: false,
                panic_marks: Some(Arc::clone(&marks)),
                permit: Some(Arc::clone(&permit)),
            },
            WORKER_INIT_WAIT,
        )?;
        let health = marks
            .get()
            .map(Arc::clone)
            .ok_or_else(|| Error::GpuRuntime("GPU worker built no health".to_owned()))?;
        let cost_nanos = gpu.with_composer(GpuComposer::cpu_fallback_cost).flatten();
        // If this fails, `gpu` drops here: its worker stops and drops the
        // device on its own thread.
        let cpu = Worker::spawn_with(
            CPU_WORKER,
            move || {
                Ok(Arc::new(CpuFallback {
                    cpu: crate::backend::CpuBackend::default(),
                    cost_nanos,
                }))
            },
            WorkerOptions {
                adopt_late: true,
                panic_marks: None,
                permit: Some(permit),
            },
            WORKER_INIT_WAIT,
        )?;
        Ok(Self {
            gpu,
            cpu,
            health,
            period_nanos: AtomicU64::new(0),
        })
    }

    /// Run `f` on the GPU composer (counters, tests); `None` once dropped.
    pub fn with_gpu<R>(&self, f: impl FnOnce(&G) -> R) -> Option<R> {
        self.gpu.with_composer(f)
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
        let _ = self.with_gpu(|g| g.set_frame_period(period));
    }

    fn period(&self) -> Duration {
        Duration::from_nanos(self.period_nanos.load(Ordering::Relaxed))
    }

    /// Every [`Error::GpuDeadline`] this backend returned to its caller,
    /// counted once at the return boundary (each is one engine repeat). The
    /// readback gate's own count (which also includes misses of abandoned
    /// jobs) stays a separate diagnostic.
    #[must_use]
    pub fn deadline_misses(&self) -> u64 {
        self.health.deadline_returns()
    }

    /// Readbacks the #395 gate reported as missed (diagnostic).
    #[must_use]
    pub fn readback_gate_misses(&self) -> u64 {
        self.with_gpu(GpuComposer::gate_misses).unwrap_or(0)
    }

    /// Frames dispatched to the CPU fallback.
    #[must_use]
    pub fn cpu_fallback_frames(&self) -> u64 {
        self.health.cpu_fallback_frames()
    }

    fn clock_miss(why: String) -> Error {
        Error::GpuDeadline(why)
    }

    /// Count every [`Error::GpuDeadline`] handed back to the caller.
    fn counted<T>(&self, result: Result<T>) -> Result<T> {
        if matches!(result, Err(Error::GpuDeadline(_))) {
            self.health.record_deadline_return();
        }
        result
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
    pub fn composite_job(&self, job: &Arc<FrameJob>) -> Result<JobOutput> {
        let result = self.composite_job_uncounted(job);
        self.counted(result)
    }

    fn composite_job_uncounted(&self, job: &Arc<FrameJob>) -> Result<JobOutput> {
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
            Outcome::Done(Ok(image)) => {
                // A newer frame than any CPU job still in flight.
                self.cpu.invalidate();
                Ok(JobOutput::fresh(image))
            }
            Outcome::Adopted(image, _) => Ok(JobOutput::fresh(image)),
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
            Outcome::Done(Err(other)) => Err(other),
            Outcome::TimedOut => Err(Self::clock_miss(format!(
                "GPU frame not ready within the clock's {} us wait",
                wait.as_micros()
            ))),
            Outcome::Busy(held) if held >= STALL_HOLD => self.cpu_frame(
                job,
                start,
                budget,
                &format!("GPU busy for {} ms", held.as_millis()),
            ),
            Outcome::Busy(held) => Err(Self::clock_miss(format!(
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
    ) -> Result<JobOutput> {
        let remaining = budget.saturating_sub(start.elapsed());
        let output = match self.cpu.run(job, remaining) {
            Outcome::Done(Ok(image)) => JobOutput::fresh(image),
            Outcome::Adopted(image, meta) => JobOutput {
                image,
                adopted: Some(meta),
            },
            Outcome::Done(Err(err)) => return Err(err),
            Outcome::Panicked(why) => {
                return Err(Self::clock_miss(format!("CPU fallback panicked: {why}")));
            }
            Outcome::TimedOut => {
                return Err(Self::clock_miss(format!(
                    "CPU fallback not ready within the clock's {} us wait",
                    remaining.as_micros()
                )));
            }
            Outcome::Busy(held) => {
                return Err(Self::clock_miss(format!(
                    "CPU fallback still busy with an earlier frame after {} ms",
                    held.as_millis()
                )));
            }
        };
        let frames = self.health.record_cpu_fallback();
        if frames.is_power_of_two() {
            tracing::warn!(
                reason = why,
                fallback_frames = frames,
                "GPU composite unavailable; composited on the CPU reference (off the clock thread)"
            );
        }
        Ok(output)
    }

    /// The synchronous off-clock path ([`crate::backend::RunBackend::composite`])
    /// for tests, benches and one-shot renders: the same as
    /// [`Self::composite_job`] but waiting without a clock budget (the
    /// unclocked bounds still apply), and an adopted result is never
    /// returned (a caller of this path asked for exactly these tiles).
    ///
    /// # Errors
    ///
    /// As [`Self::composite_job`].
    pub fn composite_inline(
        &self,
        canvas_w: u32,
        canvas_h: u32,
        canvas: CanvasColor,
        background: LinearRgba,
        tiles: &[Tile<'_>],
    ) -> Result<Nv12Image> {
        let job = Arc::new(FrameJob::from_tiles(
            canvas_w, canvas_h, canvas, background, tiles,
        ));
        let result = self.composite_inline_uncounted(&job);
        self.counted(result)
    }

    fn composite_inline_uncounted(&self, job: &Arc<FrameJob>) -> Result<Nv12Image> {
        let on_cpu = |why: &str| -> Result<Nv12Image> {
            let image = match self.cpu.run(job, UNCLOCKED_FRAME_BUDGET) {
                Outcome::Done(result) => result?,
                Outcome::Panicked(why) => {
                    return Err(Error::GpuRuntime(format!("CPU fallback panicked: {why}")))
                }
                Outcome::Adopted(..) | Outcome::TimedOut | Outcome::Busy(_) => {
                    return Err(Error::GpuDeadline("CPU fallback not ready".to_owned()))
                }
            };
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
        match self.gpu.run(job, UNCLOCKED_FRAME_BUDGET) {
            Outcome::Done(Ok(image)) => {
                self.cpu.invalidate();
                Ok(image)
            }
            Outcome::Done(Err(Error::GpuLost(why))) => {
                self.lost(&why);
                on_cpu(&why)
            }
            Outcome::Panicked(why) => {
                self.lost(&format!("GPU call panicked: {why}"));
                on_cpu(&why)
            }
            Outcome::Done(Err(err)) if is_gpu_failure(&err) => on_cpu(&err.to_string()),
            Outcome::Done(Err(other)) => Err(other),
            Outcome::Adopted(..) | Outcome::TimedOut => Err(Error::GpuDeadline(
                "GPU frame not ready within the unclocked budget".to_owned(),
            )),
            Outcome::Busy(held) if held >= STALL_HOLD => {
                on_cpu(&format!("GPU busy for {} ms", held.as_millis()))
            }
            Outcome::Busy(held) => Err(Error::GpuDeadline(format!(
                "GPU still busy with an earlier frame after {} ms",
                held.as_millis()
            ))),
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
        panic_after_block: AtomicBool,
        /// Jobs that ENTERED `compose_tiles` (the start acknowledgement).
        started: AtomicU64,
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
            self.started.fetch_add(1, Ordering::AcqRel);
            assert!(!self.panic.load(Ordering::Acquire), "probe panic");
            while self.block.load(Ordering::Acquire) {
                std::thread::sleep(Duration::from_millis(1));
            }
            assert!(
                !self.panic_after_block.load(Ordering::Acquire),
                "probe late panic"
            );
            crate::pipeline::composite(canvas_w, canvas_h, canvas, background, tiles)
        }
    }

    impl Drop for Probe {
        fn drop(&mut self) {
            *self.dropped_on.lock().unwrap() = std::thread::current().name().map(str::to_owned);
        }
    }

    /// Wait (bounded) until `n` jobs have entered the probe: a timed-out
    /// job may still be queued on a loaded runner (review MC-R3-1).
    fn await_started(probe: &Probe, n: u64) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while probe.started.load(Ordering::Acquire) < n {
            assert!(Instant::now() < deadline, "probe job never started");
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    fn spawn_probe(name: &str, probe: &Arc<Probe>, options: WorkerOptions) -> Worker<Probe> {
        let probe = Arc::clone(probe);
        Worker::spawn_with(name, move || Ok(probe), options, Duration::from_secs(10)).unwrap()
    }

    fn adopting() -> WorkerOptions {
        WorkerOptions {
            adopt_late: true,
            ..WorkerOptions::default()
        }
    }

    fn job() -> Arc<FrameJob> {
        Arc::new(FrameJob {
            canvas_w: 16,
            canvas_h: 16,
            canvas: CanvasColor::default(),
            background: LinearRgba::TRANSPARENT,
            tiles: Vec::new(),
            scene: 0,
            meta: None,
        })
    }

    fn job_in(scene: u64) -> Arc<FrameJob> {
        Arc::new(FrameJob {
            scene,
            ..(*job()).clone()
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
        let worker = spawn_probe("probe-a", &probe, WorkerOptions::default());
        assert!(matches!(
            worker.run(&job(), Duration::from_secs(5)),
            Outcome::Done(Ok(_))
        ));
        probe.block.store(true, Ordering::Release);
        assert!(matches!(
            worker.run(&job(), Duration::from_millis(5)),
            Outcome::TimedOut
        ));
        await_started(&probe, 2);
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
        let worker = spawn_probe("probe-d", &probe, adopting());
        block.store(true, Ordering::Release);
        assert!(matches!(
            worker.run(&job(), Duration::from_millis(5)),
            Outcome::TimedOut
        ));
        await_started(&probe, 1);
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
            Outcome::Adopted(..)
        ));
        assert_eq!(probe.runs.load(Ordering::Relaxed), runs);
        unblock.join().unwrap();
    }

    #[test]
    fn a_panicking_job_is_reported_and_the_worker_survives() {
        let probe = Arc::new(Probe::default());
        let worker = spawn_probe("probe-b", &probe, WorkerOptions::default());
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
        let worker = spawn_probe("probe-c", &probe, WorkerOptions::default());
        block.store(true, Ordering::Release);
        assert!(matches!(
            worker.run(&job(), Duration::from_millis(5)),
            Outcome::TimedOut
        ));
        await_started(&probe, 1);
        drop(probe);
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

    /// Review MC-2 / INV-3: a CPU result that finished BETWEEN two frames
    /// (after one frame's wait, before the next call) is adopted by the next
    /// frame of the same scene instead of being discarded.
    #[test]
    fn a_result_finished_between_frames_is_adopted_in_the_same_scene() {
        let probe = Arc::new(Probe::default());
        let block = Arc::clone(&probe.block);
        let worker = spawn_probe("probe-e", &probe, adopting());
        block.store(true, Ordering::Release);
        assert!(matches!(
            worker.run(&job_in(7), Duration::from_millis(5)),
            Outcome::TimedOut
        ));
        await_started(&probe, 1);
        block.store(false, Ordering::Release);
        while worker.is_busy() {
            std::thread::sleep(Duration::from_millis(1));
        }
        let runs = probe.runs.load(Ordering::Relaxed);
        // Same scene: adopted at once, no new job.
        assert!(matches!(
            worker.run(&job_in(7), Duration::from_millis(0)),
            Outcome::Adopted(..)
        ));
        assert_eq!(probe.runs.load(Ordering::Relaxed), runs);
    }

    /// Review MC-1: a late result of a DIFFERENT scene (e.g. a staged take
    /// that was rolled back) is never handed to a later frame, neither while
    /// it runs nor after it finished.
    #[test]
    fn a_late_result_of_another_scene_is_never_adopted() {
        let probe = Arc::new(Probe::default());
        let block = Arc::clone(&probe.block);
        let worker = spawn_probe("probe-f", &probe, adopting());
        block.store(true, Ordering::Release);
        assert!(matches!(
            worker.run(&job_in(1), Duration::from_millis(5)),
            Outcome::TimedOut
        ));
        await_started(&probe, 1);
        // Running, other scene: busy at once, no wait for it.
        assert!(matches!(
            worker.run(&job_in(2), Duration::from_secs(5)),
            Outcome::Busy(_)
        ));
        block.store(false, Ordering::Release);
        while worker.is_busy() {
            std::thread::sleep(Duration::from_millis(1));
        }
        let runs = probe.runs.load(Ordering::Relaxed);
        // Finished, other scene: discarded, scene 2 composes afresh.
        assert!(matches!(
            worker.run(&job_in(2), Duration::from_secs(5)),
            Outcome::Done(Ok(_))
        ));
        assert_eq!(probe.runs.load(Ordering::Relaxed), runs + 1);
    }

    /// Review MC-4 / INV-4: a GPU call that panics AFTER the clock gave up on
    /// it still marks the device lost (recorded on the worker).
    #[test]
    fn a_late_panic_marks_the_device_lost_on_the_worker() {
        let probe = Arc::new(Probe::default());
        let block = Arc::clone(&probe.block);
        let health = Arc::new(GpuHealth::default());
        let marks: Arc<std::sync::OnceLock<Arc<GpuHealth>>> = Arc::default();
        marks.set(Arc::clone(&health)).unwrap();
        let worker = spawn_probe(
            "probe-g",
            &probe,
            WorkerOptions {
                panic_marks: Some(marks),
                ..WorkerOptions::default()
            },
        );
        block.store(true, Ordering::Release);
        assert!(matches!(
            worker.run(&job(), Duration::from_millis(5)),
            Outcome::TimedOut
        ));
        await_started(&probe, 1);
        // The blocked call ends in a panic nobody waits for.
        probe.panic_after_block.store(true, Ordering::Release);
        block.store(false, Ordering::Release);
        let deadline = Instant::now() + Duration::from_secs(10);
        while !health.is_lost() {
            assert!(Instant::now() < deadline, "late panic never marked lost");
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    /// Review MC-5 / BR-1 / INV-1: a worker that cannot be spawned is an
    /// error, never an inline fallback onto the caller's thread.
    #[test]
    fn spawn_failure_is_an_error_never_inline() {
        // The GPU worker cannot be spawned: the builder never runs, so no
        // device ever exists on (or is dropped on) the caller.
        fail_spawns_on_this_thread(1);
        let built = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&built);
        let pool = Arc::new(Admission::new(4));
        let backend = OffClockGpu::<crate::readback::SimulatedGpu>::build(&pool, move || {
            flag.store(true, Ordering::Release);
            Ok(crate::readback::SimulatedGpu::new(Duration::from_millis(1)))
        });
        fail_spawns_on_this_thread(0);
        assert!(matches!(backend, Err(Error::GpuRuntime(_))));
        assert!(!built.load(Ordering::Acquire), "no device was built");
        assert_eq!(pool.live(), 0, "the refused backend holds no lease");
    }

    /// Review BR-1 / INV-1 round 2: the CPU worker cannot be spawned AFTER
    /// the GPU device was built: the device is dropped on its own worker
    /// thread, never on the caller.
    #[test]
    fn a_failed_second_spawn_drops_the_device_on_its_worker() {
        fail_spawn_after(1);
        let pool = Arc::new(Admission::new(4));
        let (tx, rx) = std::sync::mpsc::channel();
        let backend = OffClockGpu::<crate::readback::SimulatedGpu>::build(&pool, move || {
            let sim = crate::readback::SimulatedGpu::new(Duration::from_millis(1));
            let _ = tx.send(crate::readback::SimulatedGpuControl::new(&sim));
            Ok(sim)
        });
        fail_spawns_on_this_thread(0);
        assert!(matches!(backend, Err(Error::GpuRuntime(_))));
        let control = rx.recv_timeout(Duration::from_secs(10)).unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        let dropped_on = loop {
            if let Some(name) = control.dropped_on() {
                break name;
            }
            assert!(Instant::now() < deadline, "device never dropped");
            std::thread::sleep(Duration::from_millis(2));
        };
        assert_eq!(dropped_on, GPU_WORKER);
        while pool.live() != 0 {
            assert!(Instant::now() < deadline, "lease never released");
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    /// Review MC-R2-2: a CPU result that was still running when the GPU
    /// delivered a newer frame is never adopted later.
    #[test]
    fn a_cpu_result_older_than_a_gpu_frame_is_never_adopted() {
        let probe = Arc::new(Probe::default());
        let block = Arc::clone(&probe.block);
        let worker = spawn_probe("probe-j", &probe, adopting());
        block.store(true, Ordering::Release);
        assert!(matches!(
            worker.run(&job_in(3), Duration::from_millis(5)),
            Outcome::TimedOut
        ));
        await_started(&probe, 1);
        // The GPU recovered and delivered: invalidate.
        worker.invalidate();
        // Still running: not adoptable, fail fast.
        assert!(matches!(
            worker.run(&job_in(3), Duration::from_secs(5)),
            Outcome::Busy(_)
        ));
        block.store(false, Ordering::Release);
        while worker.is_busy() {
            std::thread::sleep(Duration::from_millis(1));
        }
        let runs = probe.runs.load(Ordering::Relaxed);
        // Finished but stale: discarded, a fresh job runs.
        assert!(matches!(
            worker.run(&job_in(3), Duration::from_secs(5)),
            Outcome::Done(Ok(_))
        ));
        assert_eq!(probe.runs.load(Ordering::Relaxed), runs + 1);
    }

    /// Review MC-R2-1: an adopted result carries the metadata of the job
    /// that produced it, not of the frame that adopted it.
    #[test]
    fn an_adopted_result_carries_its_own_jobs_metadata() {
        let probe = Arc::new(Probe::default());
        let block = Arc::clone(&probe.block);
        let worker = spawn_probe("probe-k", &probe, adopting());
        let with_meta = |tag: u32| {
            Arc::new(FrameJob {
                scene: 9,
                meta: Some(JobMeta(Arc::new(tag))),
                ..(*job()).clone()
            })
        };
        block.store(true, Ordering::Release);
        assert!(matches!(
            worker.run(&with_meta(1), Duration::from_millis(5)),
            Outcome::TimedOut
        ));
        await_started(&probe, 1);
        block.store(false, Ordering::Release);
        match worker.run(&with_meta(2), Duration::from_secs(5)) {
            Outcome::Adopted(_, Some(JobMeta(meta))) => {
                assert_eq!(meta.downcast_ref::<u32>(), Some(&1));
            }
            other => panic!("expected an adopted frame, got {other:?}"),
        }
    }

    /// Review BR-2: admission caps live GPU workers per process and a permit
    /// is held by the worker THREAD (a wedged worker keeps it).
    #[test]
    fn admission_caps_live_workers_until_their_threads_exit() {
        let pool = Arc::new(Admission::new(1));
        let probe = Arc::new(Probe::default());
        let block = Arc::clone(&probe.block);
        let permit = Arc::new(pool.try_acquire().unwrap());
        assert!(pool.try_acquire().is_none(), "cap of one");
        let worker = spawn_probe(
            "probe-i",
            &probe,
            WorkerOptions {
                permit: Some(permit),
                ..WorkerOptions::default()
            },
        );
        block.store(true, Ordering::Release);
        assert!(matches!(
            worker.run(&job(), Duration::from_millis(5)),
            Outcome::TimedOut
        ));
        await_started(&probe, 1);
        drop(probe);
        drop(worker);
        // The handle is gone but the thread is wedged: still at the cap.
        std::thread::sleep(Duration::from_millis(20));
        assert_eq!(pool.live(), 1);
        assert!(pool.try_acquire().is_none());
        block.store(false, Ordering::Release);
        let deadline = Instant::now() + Duration::from_secs(10);
        while pool.live() != 0 {
            assert!(Instant::now() < deadline, "permit never released");
            std::thread::sleep(Duration::from_millis(2));
        }
        assert!(pool.try_acquire().is_some());
    }
}
