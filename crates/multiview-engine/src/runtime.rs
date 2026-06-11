//! The integrated engine **runtime driver loop** (invariant #1, with #10).
//!
//! [`EngineRuntime`] is the one component that *owns the tick loop*. Given the
//! fixed-cadence [`OutputClock`], the [`CompositorDrive`], the outbound
//! [`EnginePublisher`] (invariant #10 isolation), and an injected
//! [`TimeSource`], it does — forever, until stopped — exactly this per tick:
//!
//! 1. **Pace to wall-clock.** Compute the absolute deadline of the next tick
//!    (`seed + pts_at(index)`; derived from the integer tick counter, never
//!    accumulated) and wait until the [`TimeSource`] reaches it via the injected
//!    [`Pacer`]. Because deadlines are absolute, OS sleep jitter cannot cause
//!    cumulative drift (ADR-T001).
//! 2. **Advance the clock.** [`OutputClock::tick`] yields the next [`Tick`] with
//!    its exact `out_pts = f(tick)`.
//! 3. **Sample + compose one frame.** [`CompositorDrive::compose`] reads each
//!    tile's last-good frame (or `NoSignal` slate) *without blocking* and
//!    produces exactly one valid composited frame for the tick.
//! 4. **Publish outward.** The composited frame's state snapshot goes to the
//!    wait-free latest-state slot, and a per-tick event to the drop-oldest
//!    broadcast. Both publish paths are physically incapable of being blocked by
//!    a consumer (invariant #10).
//!
//! The loop **never `.await`s an input or a consumer**: inputs are sampled from
//! lock-free stores, and publishing is non-blocking. The only thing it awaits is
//! the *pacer* (the wall-clock deadline) — which depends on nothing but the tick
//! counter and the injected time source. That is precisely what makes the output
//! "one frame per tick, on schedule, forever, independent of inputs and clients."
//!
//! ## Pacing seam
//!
//! Pacing is injected via the [`Pacer`] trait so the same loop runs two ways:
//!
//! * Production wires [`RealtimePacer`] over a [`MonotonicTimeSource`](crate::clock::MonotonicTimeSource): the wait
//!   becomes a real [`tokio::time::sleep`] until the deadline.
//! * Tests wire [`CooperativePacer`] over a [`ManualTimeSource`](crate::clock::ManualTimeSource): the wait
//!   becomes a cooperative [`tokio::task::yield_now`] spin until the (manually
//!   advanced) source reaches the deadline — **no real sleeps, fully
//!   deterministic**.
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use multiview_compositor::pipeline::Nv12Image;

use crate::clock::{OutputClock, Tick, TimeSource};
use crate::drive::{CompositedFrame, CompositorDrive};
use crate::isolation::EnginePublisher;

/// How the runtime waits for a tick's wall-clock deadline.
///
/// The runtime computes the absolute deadline (on the [`TimeSource`] timeline)
/// of each tick and asks the pacer to wait for it. The pacer **must not** block
/// the executor thread; it returns once the time source has reached (or passed)
/// the deadline. Two implementations cover production and deterministic tests
/// (see the module docs).
pub trait Pacer {
    /// Wait until `source.now_nanos() >= deadline_nanos`, cooperatively.
    ///
    /// The returned future is **`Send`** so an [`EngineRuntime`] driving this
    /// pacer can run on a `tokio::spawn`ed task — exactly what MP-1's
    /// [`ProgramSet`](crate::ProgramSet) needs to run N programs concurrently, each
    /// on its own task. Both production ([`RealtimePacer`]) and test
    /// ([`CooperativePacer`]) waits are `Send` (a `tokio::time::sleep` /
    /// `yield_now` over a `Send + Sync` [`TimeSource`]).
    fn wait_until(
        &self,
        deadline_nanos: i64,
        source: &dyn TimeSource,
    ) -> impl std::future::Future<Output = ()> + Send;
}

/// Production pacer: a real [`tokio::time::sleep`] until the deadline.
///
/// Computes the remaining duration from the time source and sleeps it; on wakeup
/// it re-checks (a spurious early wake or accumulated rounding never advances the
/// tick before its deadline). Paused-time aware, so it also works under
/// `tokio::time::pause`.
#[derive(Debug, Clone, Copy, Default)]
pub struct RealtimePacer;

impl Pacer for RealtimePacer {
    async fn wait_until(&self, deadline_nanos: i64, source: &dyn TimeSource) {
        loop {
            let now = source.now_nanos();
            let remaining = deadline_nanos.saturating_sub(now);
            if remaining <= 0 {
                return;
            }
            let nanos = u64::try_from(remaining).unwrap_or(u64::MAX);
            tokio::time::sleep(Duration::from_nanos(nanos)).await;
        }
    }
}

/// Deterministic test pacer: a cooperative yield-spin until the (manually
/// advanced) source reaches the deadline. **No real sleeps.**
///
/// The test advances the [`ManualTimeSource`] (typically by exactly one tick
/// period before each step), so each `wait_until` returns after at most a couple
/// of cooperative yields — giving a contending consumer task a chance to run
/// while remaining wall-clock-free and flake-free.
///
/// [`ManualTimeSource`]: crate::clock::ManualTimeSource
#[derive(Debug, Clone, Copy, Default)]
pub struct CooperativePacer;

impl Pacer for CooperativePacer {
    async fn wait_until(&self, deadline_nanos: i64, source: &dyn TimeSource) {
        while source.now_nanos() < deadline_nanos {
            tokio::task::yield_now().await;
        }
    }
}

/// A cancellation handle for an [`EngineRuntime`].
///
/// Cloneable and `Send`/`Sync`: hand a clone to a controller (or a signal
/// handler) and call [`StopSignal::stop`] to ask the runtime to finish its
/// current tick and return. The runtime checks the flag once per tick, so
/// stopping is prompt and never interrupts a frame mid-composite.
#[derive(Debug, Clone, Default)]
pub struct StopSignal {
    stopped: Arc<AtomicBool>,
}

impl StopSignal {
    /// Create a fresh, not-yet-stopped signal.
    #[must_use]
    pub fn new() -> Self {
        Self {
            stopped: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Request the runtime to stop after its current tick.
    pub fn stop(&self) {
        self.stopped.store(true, Ordering::Release);
    }

    /// Whether a stop has been requested.
    #[must_use]
    pub fn is_stopped(&self) -> bool {
        self.stopped.load(Ordering::Acquire)
    }
}

/// What stopped the runtime loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunStop {
    /// The [`StopSignal`] was raised.
    Stopped,
    /// The requested fixed tick budget was reached (used by bounded test runs).
    Completed,
}

/// The result of a runtime run: how many ticks were emitted and why it stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RunOutcome {
    /// The number of ticks emitted (frames composited + published).
    pub ticks: u64,
    /// Why the loop returned.
    pub stop: RunStop,
}

/// A do-nothing per-tick control hook, used by [`EngineRuntime::run`] /
/// [`EngineRuntime::run_for`] (the paths with no management plane attached).
fn no_control_hook() -> impl FnMut(&mut CompositorDrive<Nv12Image>) {
    |_drive: &mut CompositorDrive<Nv12Image>| {}
}

/// The integrated engine runtime: owns the clock, the drive loop, the outbound
/// isolation publisher, the time source, and the pacer, and runs the per-tick
/// loop (invariant #1).
///
/// The state-snapshot type `S` and event type `E` are whatever the engine wants
/// to surface to control/preview; [`EngineRuntime::run`] derives both from each
/// tick's [`CompositedFrame`] via the supplied projection closures, so the
/// runtime stays agnostic to the wire types while still publishing once per tick.
pub struct EngineRuntime<P> {
    clock: OutputClock,
    drive: CompositorDrive<Nv12Image>,
    time_source: Arc<dyn TimeSource>,
    pacer: P,
    /// The seed instant (on the time-source timeline) tick 0 is anchored to.
    seed_nanos: i64,
    /// Cumulative count of ticks emitted across all `run`/`run_for` calls.
    ticks_emitted: Arc<AtomicU64>,
}

impl<P: Pacer> EngineRuntime<P> {
    /// Build a runtime. The `seed` is read from `time_source` at construction:
    /// tick `i` is due at `seed + pts_at(i)`.
    #[must_use]
    pub fn new(
        clock: OutputClock,
        drive: CompositorDrive<Nv12Image>,
        time_source: Arc<dyn TimeSource>,
        pacer: P,
    ) -> Self {
        let seed_nanos = time_source.now_nanos();
        Self {
            clock,
            drive,
            time_source,
            pacer,
            seed_nanos,
            ticks_emitted: Arc::new(AtomicU64::new(0)),
        }
    }

    /// The fixed output cadence of the underlying clock.
    #[must_use]
    pub fn cadence(&self) -> multiview_core::time::Rational {
        self.clock.cadence()
    }

    /// The seed instant (time-source nanoseconds) tick 0 is anchored to.
    #[must_use]
    pub const fn seed_nanos(&self) -> i64 {
        self.seed_nanos
    }

    /// The absolute deadline (time-source nanoseconds) of the next tick to emit.
    #[must_use]
    pub fn next_deadline_nanos(&self) -> i64 {
        self.clock
            .deadline_nanos(self.clock.next_index(), self.seed_nanos)
    }

    /// Total ticks emitted by this runtime so far.
    #[must_use]
    pub fn ticks_emitted(&self) -> u64 {
        self.ticks_emitted.load(Ordering::Acquire)
    }

    /// A clone of the wait-free cumulative-ticks counter.
    ///
    /// The runtime increments this every tick (`fetch_add`, Release). A holder of
    /// the clone reads it Acquire from **another task/thread** without locking or
    /// blocking the tick loop — exactly what MP-1's [`ProgramSet`](crate::ProgramSet)
    /// supervisor samples to prove a sibling program's clock keeps advancing while
    /// this one runs on its own task (invariants #1 + #10, per program).
    #[must_use]
    pub fn ticks_counter(&self) -> Arc<AtomicU64> {
        Arc::clone(&self.ticks_emitted)
    }

    /// Run the tick loop **forever**, until `stop` is raised.
    ///
    /// Per tick: pace to the wall-clock deadline (via the pacer + time source),
    /// advance the clock, compose exactly one frame, and publish a derived state
    /// snapshot + event through the (non-blocking) `publisher`. Never awaits an
    /// input or a consumer.
    ///
    /// `state_of` projects each tick's [`CompositedFrame`] into the wire state
    /// type `S`, published to the wait-free latest slot every tick. `event_of`
    /// projects it into an *optional* event `E`: it is published to the
    /// drop-oldest broadcast only when it returns `Some`, so events stay sparse
    /// (state-change driven), never one-per-tick. Both run on the hot loop, so
    /// keep them cheap and panic-free.
    ///
    /// # Errors
    ///
    /// Returns [`crate::Error::Canvas`] only if the compositor rejects the
    /// (structurally fixed) canvas geometry — input health is never an error.
    pub async fn run<S, E, FS, FE>(
        &mut self,
        publisher: &EnginePublisher<S, E>,
        stop: &StopSignal,
        mut state_of: FS,
        mut event_of: FE,
    ) -> crate::Result<RunOutcome>
    where
        FS: FnMut(&CompositedFrame) -> S,
        FE: FnMut(&CompositedFrame) -> Option<E>,
    {
        let mut no_control = no_control_hook();
        self.run_inner(
            publisher,
            stop,
            None,
            &mut state_of,
            &mut event_of,
            &mut no_control,
        )
        .await
    }

    /// Run the tick loop for at most `max_ticks` ticks (or until `stop`), for
    /// bounded soak/integration tests. Otherwise identical to [`EngineRuntime::run`].
    ///
    /// # Errors
    ///
    /// See [`EngineRuntime::run`].
    pub async fn run_for<S, E, FS, FE>(
        &mut self,
        publisher: &EnginePublisher<S, E>,
        stop: &StopSignal,
        max_ticks: u64,
        mut state_of: FS,
        mut event_of: FE,
    ) -> crate::Result<RunOutcome>
    where
        FS: FnMut(&CompositedFrame) -> S,
        FE: FnMut(&CompositedFrame) -> Option<E>,
    {
        let mut no_control = no_control_hook();
        self.run_inner(
            publisher,
            stop,
            Some(max_ticks),
            &mut state_of,
            &mut event_of,
            &mut no_control,
        )
        .await
    }

    /// Run **forever** (until `stop`) while applying control-plane
    /// reconfiguration at each frame boundary via `control`.
    ///
    /// `control` is invoked once per tick, between the clock advance and the
    /// compose, with `&mut` access to the [`CompositorDrive`] — the seam through
    /// which the management plane drains its non-blocking command queue and
    /// applies hot swaps ([`CompositorDrive::set_layout`] /
    /// [`CompositorDrive::insert_store`]). It **must not block, await, or hold a
    /// client-fillable lock**: it runs on the output-clock loop, so a stall there
    /// would falter program output (invariants #1 + #10). Otherwise identical to
    /// [`EngineRuntime::run`].
    ///
    /// # Errors
    ///
    /// See [`EngineRuntime::run`].
    pub async fn run_with_control<S, E, FS, FE, FC>(
        &mut self,
        publisher: &EnginePublisher<S, E>,
        stop: &StopSignal,
        mut state_of: FS,
        mut event_of: FE,
        mut control: FC,
    ) -> crate::Result<RunOutcome>
    where
        FS: FnMut(&CompositedFrame) -> S,
        FE: FnMut(&CompositedFrame) -> Option<E>,
        FC: FnMut(&mut CompositorDrive<Nv12Image>),
    {
        self.run_inner(
            publisher,
            stop,
            None,
            &mut state_of,
            &mut event_of,
            &mut control,
        )
        .await
    }

    /// Bounded (`max_ticks`) counterpart of [`EngineRuntime::run_with_control`],
    /// for deterministic integration tests of the control seam.
    ///
    /// # Errors
    ///
    /// See [`EngineRuntime::run`].
    pub async fn run_for_with_control<S, E, FS, FE, FC>(
        &mut self,
        publisher: &EnginePublisher<S, E>,
        stop: &StopSignal,
        max_ticks: u64,
        mut state_of: FS,
        mut event_of: FE,
        mut control: FC,
    ) -> crate::Result<RunOutcome>
    where
        FS: FnMut(&CompositedFrame) -> S,
        FE: FnMut(&CompositedFrame) -> Option<E>,
        FC: FnMut(&mut CompositorDrive<Nv12Image>),
    {
        self.run_inner(
            publisher,
            stop,
            Some(max_ticks),
            &mut state_of,
            &mut event_of,
            &mut control,
        )
        .await
    }

    async fn run_inner<S, E, FS, FE, FC>(
        &mut self,
        publisher: &EnginePublisher<S, E>,
        stop: &StopSignal,
        max_ticks: Option<u64>,
        state_of: &mut FS,
        event_of: &mut FE,
        control: &mut FC,
    ) -> crate::Result<RunOutcome>
    where
        FS: FnMut(&CompositedFrame) -> S,
        FE: FnMut(&CompositedFrame) -> Option<E>,
        FC: FnMut(&mut CompositorDrive<Nv12Image>),
    {
        let mut emitted: u64 = 0;
        loop {
            if stop.is_stopped() {
                return Ok(RunOutcome {
                    ticks: emitted,
                    stop: RunStop::Stopped,
                });
            }
            if let Some(max) = max_ticks {
                if emitted >= max {
                    return Ok(RunOutcome {
                        ticks: emitted,
                        stop: RunStop::Completed,
                    });
                }
            }

            // 1. Pace to this tick's absolute deadline (the only `.await` that
            //    gates the loop — and it gates only on the clock, never on an
            //    input or a consumer).
            let index = self.clock.next_index();
            let deadline = self.clock.deadline_nanos(index, self.seed_nanos);
            self.pacer
                .wait_until(deadline, self.time_source.as_ref())
                .await;

            // 2. Advance the clock (pure `out_pts = f(tick)`).
            let tick: Tick = self.clock.tick();

            // 2.5 Apply any pending control-plane reconfiguration AT THE FRAME
            //     BOUNDARY (between ticks), before composing this tick — so a
            //     hot layout/source swap takes effect on the very frame it lands.
            //     The hook MUST be non-blocking and allocation-light (it drains a
            //     non-blocking queue and applies O(1) `set_layout`/`insert_store`);
            //     it never awaits and never holds a lock a client can fill, so the
            //     output clock cannot be stalled by control (invariants #1 + #10).
            control(&mut self.drive);

            // 3. Sample + compose exactly one frame (lock-free, never blocks on
            //    an input).
            let frame = self.drive.compose(tick)?;

            // 4. Publish outward — wait-free state slot + non-blocking drop-oldest
            //    event stream. Neither can be back-pressured by a consumer.
            publisher.publish_state(state_of(&frame));
            // Events are sparse: publish only when the projection yields one, so
            // the drop-oldest broadcast carries state changes, not a per-tick
            // flood. The state slot above still refreshes every tick.
            if let Some(event) = event_of(&frame) {
                publisher.publish_event(event);
            }

            emitted = emitted.saturating_add(1);
            self.ticks_emitted.fetch_add(1, Ordering::AcqRel);
        }
    }
}
