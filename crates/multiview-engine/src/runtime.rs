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

/// The maximum number of last-good **repeats** the drive loop emits in a single
/// iteration before it gives up frame-by-frame catch-up and **resyncs** the clock
/// to wall-clock in one [`OutputClock::skip_to`] step (ADR-T018).
///
/// Sustained moderate overload (the common contended-host case) falls only a few
/// ticks behind per iteration, far under this cap, so it is backfilled with a
/// **contiguous** run of last-good repeats — no PTS gap. Only a *pathological*
/// one-off time jump (a multi-second deschedule, a VM pause/migration where
/// `CLOCK_MONOTONIC` leaps) exceeds the cap; there the loop emits at most this
/// many repeats then jumps the counter to the wall-clock index, accepting one
/// bounded discontinuity rather than an unbounded burst (and never spinning).
/// 120 ticks is ~4 s at 30 fps / ~2 s at 60 fps of held last-good before resync.
pub const MAX_REPEATS_PER_TICK: u32 = 120;

/// The longest a [`RealtimePacer`] sleeps in one step before re-checking the
/// [`StopSignal`]. This deliberately bounds stop-observation latency to ~one
/// poll interval (~10 ms) **regardless of clock state** — even a deadline the
/// clock has not yet reached (a paused/jumped clock, or a CPU-contended host
/// whose wall-clock has not advanced a whole period). Because it is shorter
/// than every broadcast tick period (16.7 ms at 60 fps .. 41.7 ms at 24 fps),
/// a normal one-tick wait is intentionally split into ~2-5 of these sleeps —
/// cheap (each step is an allocation-free timer registration) and what keeps
/// shutdown prompt on an unreachable deadline (ADR-T018, the fix for the CI
/// ~37-min "hang"). The cap adds no delay of its own: each step sleeps
/// `min(remaining, cap)`, so the final step is sized to the exact time left and
/// the wait ends once the clock reaches the deadline — as with a single uncapped
/// sleep, subject only to the tokio timer's normal wake latency.
const PACER_STOP_POLL: Duration = Duration::from_millis(10);

/// How the runtime waits for a tick's wall-clock deadline.
///
/// The runtime computes the absolute deadline (on the [`TimeSource`] timeline)
/// of each tick and asks the pacer to wait for it. The pacer **must not** block
/// the executor thread; it returns once the time source has reached (or passed)
/// the deadline **or** the [`StopSignal`] is raised — whichever comes first. Two
/// implementations cover production and deterministic tests (see the module docs).
pub trait Pacer {
    /// Wait until `source.now_nanos() >= deadline_nanos`, cooperatively, **or
    /// until `stop` is raised** — whichever happens first.
    ///
    /// Honouring `stop` here is load-bearing for invariant #1's shutdown
    /// contract: the drive loop parks in this wait between ticks, so a pacer that
    /// ignored `stop` would spin/sleep on an unreachable deadline forever once the
    /// clock is frozen or merely slow (a contended host), and the loop could never
    /// observe the stop. The pacer therefore returns promptly (within
    /// `PACER_STOP_POLL` for the real pacer; a single cooperative yield for the
    /// test pacer) once `stop` is set; the caller re-checks `stop` immediately
    /// after and returns without composing an unwanted tick.
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
        stop: &StopSignal,
    ) -> impl std::future::Future<Output = ()> + Send;
}

/// Production pacer: a real [`tokio::time::sleep`] until the deadline.
///
/// Computes the remaining duration from the time source and sleeps it (in steps
/// of at most `PACER_STOP_POLL` so a raised [`StopSignal`] is observed
/// promptly); on wakeup it re-checks (a spurious early wake or accumulated
/// rounding never advances the tick before its deadline). Paused-time aware, so
/// it also works under `tokio::time::pause`.
#[derive(Debug, Clone, Copy, Default)]
pub struct RealtimePacer;

impl Pacer for RealtimePacer {
    async fn wait_until(&self, deadline_nanos: i64, source: &dyn TimeSource, stop: &StopSignal) {
        loop {
            if stop.is_stopped() {
                return;
            }
            let now = source.now_nanos();
            let remaining = deadline_nanos.saturating_sub(now);
            if remaining <= 0 {
                return;
            }
            let nanos = u64::try_from(remaining).unwrap_or(u64::MAX);
            // Cap each sleep so a stop raised mid-wait is seen within one poll
            // interval rather than only after the (possibly far) deadline. The
            // cap is below every tick period, so a normal one-tick wait is
            // deliberately split into ~2-5 short sleeps (see PACER_STOP_POLL);
            // each sleeps min(remaining, cap), so the final step is sized to the
            // exact time left and the cap adds no delay of its own.
            let poll_cap = u64::try_from(PACER_STOP_POLL.as_nanos()).unwrap_or(u64::MAX);
            tokio::time::sleep(Duration::from_nanos(nanos.min(poll_cap))).await;
        }
    }
}

/// Deterministic test pacer: a cooperative yield-spin until the (manually
/// advanced) source reaches the deadline — **or `stop` is raised**. **No real
/// sleeps.**
///
/// The test advances the [`ManualTimeSource`] (typically by exactly one tick
/// period before each step), so each `wait_until` returns after at most a couple
/// of cooperative yields — giving a contending consumer task a chance to run
/// while remaining wall-clock-free and flake-free. Checking `stop` each yield is
/// what lets a bounded test (or a real shutdown) end even when the manual clock
/// is frozen on an unreachable deadline — otherwise this spin would never exit.
///
/// [`ManualTimeSource`]: crate::clock::ManualTimeSource
#[derive(Debug, Clone, Copy, Default)]
pub struct CooperativePacer;

impl Pacer for CooperativePacer {
    async fn wait_until(&self, deadline_nanos: i64, source: &dyn TimeSource, stop: &StopSignal) {
        while source.now_nanos() < deadline_nanos {
            if stop.is_stopped() {
                return;
            }
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
    /// Cumulative count of last-good **repeats** emitted under overload (ADR-T018):
    /// ticks where the loop re-published the held last-good frame (under a fresh,
    /// strictly-increasing pts) instead of composing, to hold wall-clock cadence.
    /// Mirrors `ticks_emitted`; the overload signal for telemetry / the
    /// degradation loop (a rising rate means compose is not fitting the budget).
    frames_repeated: Arc<AtomicU64>,
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
            frames_repeated: Arc::new(AtomicU64::new(0)),
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

    /// Total last-good **repeats** emitted under overload so far (ADR-T018).
    ///
    /// Zero whenever compose keeps up with the tick budget; a rising value is the
    /// engine telling you the host is contended and the output is **holding
    /// cadence by repeating rather than slipping**. Read wait-free from any thread.
    #[must_use]
    pub fn frames_repeated(&self) -> u64 {
        self.frames_repeated.load(Ordering::Acquire)
    }

    /// A clone of the wait-free cumulative-repeats counter, for an off-thread
    /// telemetry/degradation sampler (mirrors [`EngineRuntime::ticks_counter`]).
    #[must_use]
    pub fn frames_repeated_counter(&self) -> Arc<AtomicU64> {
        Arc::clone(&self.frames_repeated)
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
        // The last fresh composite, held so the loop can re-emit it (under a fresh
        // pts) for any tick whose wall-clock deadline already passed while a slow
        // compose ran — holding 1.0× cadence instead of slipping (ADR-T018, inv
        // #1/#2/#3). `None` only before the very first frame is composed.
        let mut last_good: Option<CompositedFrame> = None;
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

            // 1. Pace to the due tick's absolute deadline (the only `.await` that
            //    gates the loop — and it gates only on the clock or a raised stop,
            //    never on an input or a consumer).
            let index = self.clock.next_index();
            let deadline = self.clock.deadline_nanos(index, self.seed_nanos);
            self.pacer
                .wait_until(deadline, self.time_source.as_ref(), stop)
                .await;
            // The pacer returns early on stop (so it cannot spin/sleep forever on an
            // unreachable deadline). Re-check here and return WITHOUT composing this
            // tick, so a stop raised mid-wait ends the loop within one poll interval
            // rather than emitting one more frame past the deadline it never met.
            if stop.is_stopped() {
                return Ok(RunOutcome {
                    ticks: emitted,
                    stop: RunStop::Stopped,
                });
            }

            // 1.5 DROP/REPEAT-TO-CADENCE (ADR-T018). If compose has fallen behind
            //     wall-clock (a CPU/GPU-contended host), re-emit the held last-good
            //     frame for each whole tick-period that has ALREADY elapsed, each
            //     under a fresh strictly-increasing pts — so the emitted tick tracks
            //     wall-clock at 1.0× rather than the loop free-running at compose
            //     speed and slipping (the frigate 84-minute-lag failure). Off the
            //     overload path the predicate below is false on the first check and
            //     this is a no-op — byte-identical to composing one fresh frame per
            //     tick. Entered only once a last-good frame exists (the first tick
            //     always composes fresh, never repeats an absent frame).
            if last_good.is_some() {
                let mut repeats: u32 = 0;
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
                    let next = self.clock.next_index();
                    let now = self.time_source.now_nanos();
                    // `next` is the freshest due tick exactly when the tick AFTER it
                    // is not yet due. Gate strictly on the exact deadline (never a
                    // rounded wall-index): on a healthy sub-period-late wake this is
                    // true immediately, so we compose `next` fresh below rather than
                    // composing it ahead of its deadline (the floor-not-nearest fix).
                    if now
                        < self
                            .clock
                            .deadline_nanos(next.saturating_add(1), self.seed_nanos)
                    {
                        break;
                    }
                    if repeats >= MAX_REPEATS_PER_TICK {
                        // Pathological one-off jump (a multi-second deschedule / VM
                        // pause where CLOCK_MONOTONIC leaps): stop emitting a per-frame
                        // burst and resync the counter to the wall-clock FLOOR tick in
                        // ONE `skip_to`, accepting one bounded discontinuity rather than
                        // unbounded catch-up. `cap_resync_floor` is O(1) and never
                        // composes a tick before its deadline (inv #1) — see its docs.
                        self.clock.skip_to(self.cap_resync_floor(now, next));
                        break;
                    }
                    // Re-emit last-good under the fresh tick: the held canvas is
                    // reused IN PLACE (only the tick/pts changes), so a repeat is not
                    // a multi-MB plane copy on the hot loop — the downstream
                    // `state_of` fan-out clones the canvas once, exactly as it does
                    // for a fresh frame (inv #3: a repeat carries a NEW pts, never a
                    // duplicate/rewound one).
                    let repeat_tick: Tick = self.clock.tick();
                    let Some(last) = last_good.as_mut() else {
                        break;
                    };
                    last.tick = repeat_tick;
                    publisher.publish_state(state_of(&*last));
                    if let Some(event) = event_of(&*last) {
                        publisher.publish_event(event);
                    }
                    emitted = emitted.saturating_add(1);
                    self.ticks_emitted.fetch_add(1, Ordering::AcqRel);
                    self.frames_repeated.fetch_add(1, Ordering::AcqRel);
                    repeats = repeats.saturating_add(1);
                }
            }

            // 2. Advance the clock for the fresh tick (pure `out_pts = f(tick)`).
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

            // Hold this fresh composite as last-good for the cadence-hold repeat
            // path above (cheap: the frame moves in; the canvas pixels are not
            // copied here).
            last_good = Some(frame);

            emitted = emitted.saturating_add(1);
            self.ticks_emitted.fetch_add(1, Ordering::AcqRel);
        }
    }

    /// The wall-clock **floor tick** for the cadence-hold cap path: the greatest
    /// index whose deadline has already passed (`deadline_nanos(idx) <= now`),
    /// clamped to `>= next`. Used after `MAX_REPEATS_PER_TICK` last-good repeats to
    /// resync the counter to wall-clock in one `skip_to` (ADR-T018), dropping the
    /// skipped ticks rather than grinding them out frame-by-frame.
    ///
    /// **O(1) and provably bounded** on the output-clock hot path — no loop whose
    /// length would depend on the cadence.
    ///
    /// The estimate is the floor against the **exact rational** period — a tick
    /// spans `den/num` seconds, so `floor(elapsed_ns / period) =
    /// elapsed_ns · num / (NANOS_PER_SEC · den)` in i128 (no overflow, no period
    /// rounding, no `/0`: the divisor is `den`, never `pts_at(1)`). Dividing by the
    /// *rounded* `pts_at(1)` would diverge from the true floor without bound as the
    /// index grows on a non-integer cadence (`idx·round(period)` vs `round(idx·period)`),
    /// so a fixed correction could not recover it and the resync would collapse to
    /// `next` — grinding the backlog. The exact-rational form has no such drift.
    ///
    /// The clock's actual `deadline_nanos(idx) = seed + pts_at(idx)` rounds `pts_at`
    /// to the nearest ns, so the floor against the *rounded* deadlines is the exact
    /// floor ±1. One O(1) up-step and one O(1) down-step (mutually exclusive)
    /// reconcile that boundary against the real `deadline_nanos`. **The ±1 step is
    /// provably sufficient** because the exact per-tick period is `>= 1 ns`: both
    /// `Canvas::validate` and `OutputClock::new` reject a cadence with a
    /// sub-nanosecond period (`fps > 1 GHz`, see `Rational::has_subnanosecond_period`),
    /// so consecutive ticks have strictly increasing rounded deadlines and the rounded
    /// floor cannot diverge from the exact floor by more than one. The result is
    /// always `>= next + 1` with `deadline_nanos(result) <= now` — it never composes
    /// a tick ahead of its deadline (inv #1) — because the caller only enters the cap
    /// path after the break predicate failed (`now >= deadline_nanos(next + 1)`), so
    /// the true floor is `>= next + 1`.
    fn cap_resync_floor(&self, now: i64, next: u64) -> u64 {
        let cadence = self.clock.cadence();
        if cadence.num <= 0 || cadence.den <= 0 {
            // Degenerate cadence (rejected by `OutputClock::new`): no skip.
            return next;
        }
        let elapsed = i128::from(now.saturating_sub(self.seed_nanos).max(0));
        let denom = 1_000_000_000_i128.saturating_mul(i128::from(cadence.den));
        let est = elapsed.saturating_mul(i128::from(cadence.num)) / denom;
        let mut floor_idx = u64::try_from(est).unwrap_or(0).max(next);
        if self
            .clock
            .deadline_nanos(floor_idx.saturating_add(1), self.seed_nanos)
            <= now
        {
            // Up-step: the next tick's (rounded) deadline has also passed.
            floor_idx = floor_idx.saturating_add(1);
        } else if floor_idx > next && self.clock.deadline_nanos(floor_idx, self.seed_nanos) > now {
            // Down-step: this tick's (rounded) deadline is still in the future.
            floor_idx = floor_idx.saturating_sub(1);
        }
        floor_idx
    }
}
