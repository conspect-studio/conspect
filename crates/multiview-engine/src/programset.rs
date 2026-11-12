//! The **`ProgramSet`** supervisor + N independent output clocks (ADR-0030 §2,
//! MP-1).
//!
//! MP-0 introduced [`MultiviewProgram`]: one program's
//! own [`OutputClock`], [`CompositorDrive`],
//! and [`StopSignal`](crate::StopSignal), driving the protected per-tick loop. The
//! CLI run path drove **one** such program. MP-1 adds the **supervisor** so **N**
//! programs run concurrently, each on its **own** independent clock.
//!
//! ## The shape (ADR-0030 §2.1 / §2.2)
//!
//! * **One shared time *source*, N independent output *clocks*.** Every program
//!   reads the **same** read-only [`Arc<dyn TimeSource>`](crate::TimeSource) for a
//!   common monotonic reference, but each owns its **own**
//!   [`OutputClock`] + [`EngineRuntime`](crate::EngineRuntime)
//!   at its own cadence and seed. A 25 fps program and a 60 fps program advance on
//!   their own tick counters off the one shared reference — a master tick cannot
//!   serve both without resampling one (violating invariant #3). **There is no
//!   master clock**; the only shared thing is the read-only reference. This makes
//!   invariant #1's "one program stalling never stalls another" **structural**:
//!   each runtime awaits only its own pacer deadline (`runtime.rs` `run_inner`).
//! * **`Program` is the supervised actor.** Each [`Program`] runs on its **own**
//!   dedicated, named output-clock OS thread ([`crate::clock_thread`], #311,
//!   ADR-0109 — not a task on the control plane's runtime) and implements
//!   [`Actor`], so the existing
//!   [`Supervisor`] + [`RestartPolicy`]
//!   restart a *crashed* program with capped backoff while every **other** program
//!   keeps emitting. Failure is contained to one task.
//! * **`ProgramSet` is the supervisor/coordinator** — the only thing that knows
//!   there are many. It owns the shared `Arc<dyn TimeSource>`, the
//!   [`RestartPolicy`], and the `ProgramId → ProgramHandle`
//!   map. [`ProgramSet::start`] admits + spawns **without touching** other
//!   programs; [`ProgramSet::stop`] raises **only** that program's
//!   [`StopSignal`](crate::StopSignal), drains its egress, and joins. No other
//!   program's clock is touched.
//!
//! ## Per-program egress + isolation (invariant #10, per program)
//!
//! Each [`Program`] owns its own **bounded drop-oldest egress**: the per-tick
//! projection pushes the tick index onto a fixed-capacity channel consumed by a
//! **dedicated egress thread**. A wedged/slow egress consumer fills the bounded
//! queue; the push is `try_send` + drop-and-count, so the program's **own** tick
//! loop never blocks on its egress (invariant #1), and — because each program's
//! egress is its own task — a wedge on one program **cannot** back-pressure
//! another's tick loop (invariant #10). The MP-1 chaos gate proves exactly this:
//! wedge one program's egress, assert a sibling's `ticks_emitted` keeps advancing.
//!
//! The supervisor never `.await`s a program task on the data plane: it samples
//! each program's wait-free `ticks_emitted` counter (a single Acquire load) and
//! only joins a task on an explicit [`ProgramSet::stop`] / [`ProgramSet::shutdown`].

use std::collections::HashMap;
use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use multiview_compositor::pipeline::Nv12Image;
use multiview_config::ProgramSpec;

use crate::clock::{OutputClock, TimeSource};
use crate::clock_thread::ClockThread;
use crate::drive::{CompositedFrame, CompositorDrive};
use crate::error::{Error, Result};
use crate::isolation::EnginePublisher;
use crate::program::{MultiviewProgram, ProgramId};
use crate::runtime::{Pacer, RunOutcome};
use crate::supervisor::{Actor, ActorExit, RestartPolicy, Supervisor};

/// The per-program egress queue depth (bounded, drop-oldest). A wedged consumer
/// fills this; the producer (the tick loop) `try_send`s and drops-and-counts on a
/// full queue — it **never** blocks (invariants #1 + #10).
const EGRESS_QUEUE_CAP: usize = 4;

/// The per-program event-stream depth for the program's own isolated publisher.
const EVENT_CAP: usize = 8;

/// Bounded grace for joining one program's supervised task on `stop`/`shutdown`.
///
/// A healthy program returns within at most one tick + teardown of its stop being
/// raised (the run loop checks `stop` once per tick), so a real-clock program
/// joins promptly — well inside this grace. The grace exists for a program whose
/// run loop **cannot reach** its stop check (e.g. a pacer parked on a deadline its
/// time source never reaches): such a task is **aborted and detached** at the
/// grace rather than `await`ed forever, mirroring the engine's `SINK_WEDGE_GRACE`
/// teardown posture (ENG-1: bounded join, then detach a wedged peer — never a join
/// that cannot return). 2 s matches the existing egress/sink grace. Teardown is a
/// control-plane action, not the data plane, so the bounded wait gates nothing on
/// the output clock (invariants #1 + #10).
const PROGRAM_JOIN_GRACE: std::time::Duration = std::time::Duration::from_secs(2);

/// Join one supervised program's clock thread with a bounded grace, then detach.
///
/// Raises nothing itself (the caller raises `stop` first): it awaits (without
/// blocking the caller's executor) up to [`PROGRAM_JOIN_GRACE`] for the thread to
/// finish on its own (the clean path for a program that observes its stop within
/// a tick, or [`STOP_POLL`](crate::clock_thread::STOP_POLL) while it sleeps). If
/// it has not finished by then it is **detached** — teardown is bounded, never an
/// unbounded wait on a thread that cannot return. An OS thread cannot be
/// cancelled; a detached wedged clock thread owns only its own program and exits
/// as soon as its loop reaches a stop check. A panic on the thread is logged,
/// never propagated: stopping one program must never fail the supervisor.
async fn join_program_task_bounded(pid: &ProgramId, task: ClockThread<()>) {
    match tokio::time::timeout(PROGRAM_JOIN_GRACE, task.join()).await {
        Ok(Ok(())) => {}
        Ok(Err(panic)) => {
            tracing::warn!(program = %pid, %panic, "program clock thread panicked before teardown");
        }
        Err(_elapsed) => {
            tracing::warn!(
                program = %pid,
                grace_ms = PROGRAM_JOIN_GRACE.as_millis(),
                "program clock thread did not stop within grace; detached (bounded teardown)"
            );
        }
    }
}

/// One program's egress consumer: a closure run on a **dedicated thread** for each
/// emitted tick index. Production wires the real per-program output fan-out here;
/// the MP-1 chaos gate wires a deliberately-wedged consumer to prove a stuck
/// egress never stalls the program's own clock — let alone a sibling's.
type EgressConsumer = Box<dyn FnMut(u64) + Send + 'static>;

/// A caller-supplied program run: a `FnOnce` returning a boxed future that drives
/// the program's own protected loop to completion (or failure) and returns
/// [`ActorExit`]. This is the seam through which the CLI registers its full
/// ingest→composite→encode→fan-out program (its own `MultiviewProgram` drive) as a
/// first-class member of a [`ProgramSet`] — the supervisor owns its lifecycle
/// (start/stop) and samples its progress, while the heavy drive stays in the CLI.
type ExternalRun =
    Box<dyn FnOnce() -> std::pin::Pin<Box<dyn Future<Output = ActorExit> + Send>> + Send>;

/// How a [`Program`] runs under the supervisor.
enum Runner<P> {
    /// An engine-native multiview program: its own [`MultiviewProgram`] (clock,
    /// drive, stop), its own isolated [`EnginePublisher`], and its own bounded
    /// drop-oldest egress. Used by the engine's own programs and the MP-1
    /// acceptance tests. The heavy [`MultiviewProgram`] is boxed so this variant
    /// does not bloat the small [`Runner::External`] variant.
    Multiview {
        /// The protected per-program output core (clock + runtime + own stop).
        inner: Box<MultiviewProgram<P>>,
        /// This program's own isolated outbound publisher (not shared — inv #10).
        publisher: EnginePublisher<u64, ()>,
        /// The producer end of the bounded drop-oldest egress (`Option` so the run
        /// loop moves it out and drops it on return, closing the channel).
        egress_tx: Option<std::sync::mpsc::SyncSender<u64>>,
        /// The dedicated egress consumer thread (drains until the channel closes).
        egress_thread: Option<std::thread::JoinHandle<()>>,
    },
    /// A caller-supplied program run (e.g. the CLI's full `Pipeline` drive). The
    /// supervisor owns its lifecycle via the shared [`StopSignal`](crate::StopSignal)
    /// and ticks counter in the [`Program`]; this is the run future itself, `None`
    /// once a run attempt has consumed it.
    External(Option<ExternalRun>),
}

/// A self-contained, independently-supervised output **program**, ready to run on
/// one `tokio` task under a [`ProgramSet`]: one independent clock (its own
/// [`OutputClock`]) reading the set's shared time source.
///
/// A program is either **engine-native multiview** (its own [`MultiviewProgram`] +
/// isolated [`EnginePublisher`] + bounded drop-oldest egress) or an **external**
/// caller-supplied run (the seam the CLI registers its full ingest→encode→fan-out
/// `Pipeline` drive through). Either way the supervisor owns its lifecycle and
/// samples its wait-free progress; the program's loop never blocks a sibling.
///
/// `P` is the [`Pacer`] (real-time in production; a cooperative test pacer in
/// deterministic tests) — all programs in one [`ProgramSet`] share the pacer type.
pub struct Program<P> {
    /// This program's identity.
    id: ProgramId,
    /// This program's fixed output cadence.
    cadence: multiview_core::time::Rational,
    /// This program's stop handle (a clone of the run's own `StopSignal`). Raising
    /// it asks the program to finish its current tick and return.
    stop: crate::runtime::StopSignal,
    /// This program's wait-free cumulative-ticks counter (shared with its run loop).
    ticks: Arc<AtomicU64>,
    /// Frames dropped at the egress because the consumer could not keep up
    /// (multiview programs only; `0` for an external run that counts its own shed).
    egress_dropped: Arc<AtomicU64>,
    /// How this program runs (engine-native multiview, or a caller-supplied run).
    runner: Runner<P>,
}

impl<P: Pacer> Program<P> {
    /// Build a multiview program from a [`ProgramSpec`] + its already-assembled
    /// per-program clock + drive + shared time source + pacer, with a **no-op**
    /// egress consumer (the default: the program clocks + publishes, with nothing
    /// further wired downstream — used by the cadence/stop acceptance tests).
    ///
    /// The program constructs its **own** [`StopSignal`](crate::StopSignal)
    /// internally (via [`MultiviewProgram::new`]); the supervisor reads a clone for
    /// targeted stop.
    ///
    /// # Errors
    ///
    /// Propagates [`MultiviewProgram::new`]'s errors (wrong program kind, or a
    /// clock/spec cadence mismatch).
    pub fn multiview(
        spec: &ProgramSpec,
        clock: OutputClock,
        drive: CompositorDrive<Nv12Image>,
        time: Arc<dyn TimeSource>,
        pacer: P,
    ) -> Result<Self> {
        Self::multiview_with_egress(spec, clock, drive, time, pacer, |_tick| {})
    }

    /// Build a multiview program whose emitted tick indices are consumed by
    /// `egress` on a **dedicated egress thread** (bounded drop-oldest between the
    /// tick loop and the consumer).
    ///
    /// This is the seam production wires the real per-program output fan-out
    /// through, and the seam the MP-1 chaos gate wires a deliberately-wedged
    /// consumer through to prove a stuck egress never stalls the program's own
    /// clock (invariant #1) or a sibling's (invariant #10).
    ///
    /// # Errors
    ///
    /// Propagates [`MultiviewProgram::new`]'s errors (wrong program kind, or a
    /// clock/spec cadence mismatch).
    pub fn multiview_with_egress<F>(
        spec: &ProgramSpec,
        clock: OutputClock,
        drive: CompositorDrive<Nv12Image>,
        time: Arc<dyn TimeSource>,
        pacer: P,
        egress: F,
    ) -> Result<Self>
    where
        F: FnMut(u64) + Send + 'static,
    {
        let stop = crate::runtime::StopSignal::new();
        let inner = MultiviewProgram::new(spec, clock, drive, time, pacer, stop)?;
        Ok(Self::from_multiview(inner, egress))
    }

    fn from_multiview<F>(inner: MultiviewProgram<P>, egress: F) -> Self
    where
        F: FnMut(u64) + Send + 'static,
    {
        let (egress_tx, egress_rx) = std::sync::mpsc::sync_channel::<u64>(EGRESS_QUEUE_CAP);
        // Spawn the dedicated egress consumer thread NOW: it drains tick indices
        // into the consumer until every sender drops (channel close). A wedged
        // consumer blocks ONLY this thread; the bounded `try_send` on the producer
        // side means the tick loop is never blocked by it (invariants #1 + #10).
        let mut consumer: EgressConsumer = Box::new(egress);
        let id = inner.id().clone();
        let egress_thread = std::thread::Builder::new()
            .name(format!("egress-{}", id.as_str()))
            .spawn(move || {
                while let Ok(tick) = egress_rx.recv() {
                    consumer(tick);
                }
            })
            .ok();
        Self {
            id,
            cadence: inner.cadence(),
            stop: inner.stop_signal(),
            ticks: inner.ticks_counter(),
            egress_dropped: Arc::new(AtomicU64::new(0)),
            runner: Runner::Multiview {
                inner: Box::new(inner),
                publisher: EnginePublisher::new(EVENT_CAP),
                egress_tx: Some(egress_tx),
                egress_thread,
            },
        }
    }

    /// Build a multiview program anchored to an explicit tick-0 `seed_nanos`
    /// (the predecessor's, from [`ProgramSet::seed_nanos`]) with a no-op egress —
    /// the NEW side of a [`ProgramSet::start_successor`] thread handoff.
    ///
    /// # Errors
    ///
    /// See [`Program::multiview`].
    pub fn multiview_succeeding<F>(
        spec: &ProgramSpec,
        clock: OutputClock,
        drive: CompositorDrive<Nv12Image>,
        time: Arc<dyn TimeSource>,
        pacer: P,
        seed_nanos: i64,
        egress: F,
    ) -> Result<Self>
    where
        F: FnMut(u64) + Send + 'static,
    {
        let stop = crate::runtime::StopSignal::new();
        let inner = MultiviewProgram::with_seed(spec, clock, drive, time, pacer, stop, seed_nanos)?;
        Ok(Self::from_multiview(inner, egress))
    }

    /// Register a **caller-supplied** program run as a first-class member of a
    /// [`ProgramSet`] (ADR-0030 §2.2). The supervisor owns its lifecycle — `start`
    /// spawns it, [`ProgramSet::stop`] raises `stop`, [`ProgramSet::ticks_emitted`]
    /// samples `ticks` — while the actual heavy drive (e.g. the CLI's full
    /// ingest→composite→encode→fan-out `Pipeline`, which builds its own
    /// [`MultiviewProgram`] internally) lives in `run`.
    ///
    /// The caller passes the program's identity + cadence, the **same**
    /// [`StopSignal`](crate::StopSignal) its run loop checks (so `stop` reaches it),
    /// the **same** wait-free ticks counter its run loop increments (so the
    /// supervisor observes its progress), and a `FnOnce` producing the run future.
    /// `run` returns [`ActorExit::Completed`] on a clean stop (no restart) or
    /// [`ActorExit::Failed`] to ask the supervisor to restart it with capped backoff
    /// while siblings keep emitting.
    ///
    /// This is what makes "the CLI run path builds a `ProgramSet`" true and complete
    /// for the single-program legacy config (one program, id `"main"`,
    /// behaviour-identical to today) without re-homing the CLI's egress machinery
    /// into the engine.
    #[must_use]
    pub fn from_runner(
        id: ProgramId,
        cadence: multiview_core::time::Rational,
        stop: crate::runtime::StopSignal,
        ticks: Arc<AtomicU64>,
        run: impl FnOnce() -> std::pin::Pin<Box<dyn Future<Output = ActorExit> + Send>> + Send + 'static,
    ) -> Self {
        Self {
            id,
            cadence,
            stop,
            ticks,
            egress_dropped: Arc::new(AtomicU64::new(0)),
            runner: Runner::External(Some(Box::new(run))),
        }
    }

    /// This program's identity.
    #[must_use]
    pub fn id(&self) -> &ProgramId {
        &self.id
    }

    /// This program's fixed output cadence.
    #[must_use]
    pub fn cadence(&self) -> multiview_core::time::Rational {
        self.cadence
    }

    /// A clone of this program's wait-free cumulative-ticks counter, obtainable
    /// **before** the program is moved into [`ProgramSet::start`].
    ///
    /// A holder reads it Acquire from any thread to observe the program's progress
    /// — including **after** the program is stopped (the supervisor removes a
    /// stopped program from its map, but the counter `Arc` outlives it), which is
    /// how a caller proves a stopped program's clock is frozen.
    #[must_use]
    pub fn ticks_counter(&self) -> Arc<AtomicU64> {
        Arc::clone(&self.ticks)
    }

    /// Build the supervisor-side handle (stop, wait-free ticks counter, cadence,
    /// drop counter) BEFORE the program is spawned. The handle and the program
    /// share the same `Arc`-backed atomics, so the supervisor observes the running
    /// program's progress lock-free.
    fn handle(&self) -> ProgramHandle {
        ProgramHandle {
            stop: self.stop.clone(),
            ticks: Arc::clone(&self.ticks),
            egress_dropped: Arc::clone(&self.egress_dropped),
            cadence: self.cadence,
            handoff: match &self.runner {
                Runner::Multiview { inner, .. } => Some(inner.handoff()),
                Runner::External(_) => None,
            },
            seed_nanos: match &self.runner {
                Runner::Multiview { inner, .. } => Some(inner.seed_nanos()),
                Runner::External(_) => None,
            },
            task: None,
        }
    }

    /// Make this (multiview) program the **successor** of a running program on
    /// the same timeline (#311 thread handoff): it waits for the predecessor's
    /// cut and emits from exactly that tick. Build it with the predecessor's
    /// seed ([`ProgramSet::seed_nanos`], via [`Program::multiview_succeeding`]).
    fn succeed(&mut self, predecessor: Arc<crate::runtime::Handoff>) -> bool {
        match &mut self.runner {
            Runner::Multiview { inner, .. } => {
                inner.succeed(predecessor);
                true
            }
            Runner::External(_) => false,
        }
    }
}

impl<P: Pacer> Actor for Program<P> {
    fn name(&self) -> &str {
        self.id.as_str()
    }

    /// One supervised run attempt. For a multiview program: drive the protected
    /// per-tick loop **forever** (until this program's own
    /// [`StopSignal`](crate::StopSignal) is raised), pushing each emitted tick onto
    /// the bounded drop-oldest egress. For an external program: await the
    /// caller-supplied run future.
    ///
    /// A clean return (stop raised) is [`ActorExit::Completed`] — the supervisor
    /// does not restart a deliberately-stopped program. A loop error is
    /// [`ActorExit::Failed`] so the supervisor restarts it with capped backoff
    /// while siblings keep emitting.
    async fn run(&mut self) -> ActorExit {
        let id = self.id.clone();
        let dropped = Arc::clone(&self.egress_dropped);
        match &mut self.runner {
            Runner::External(run) => match run.take() {
                Some(run) => run().await,
                // Already consumed by a prior attempt (an external program is a
                // single FnOnce; a restart has nothing to re-run). Treat as complete.
                None => ActorExit::Completed,
            },
            Runner::Multiview {
                inner,
                publisher,
                egress_tx,
                ..
            } => {
                // MOVE the egress producer into the per-tick projection: on loop
                // return the sender drops, closing the channel so the egress
                // thread's `recv()` ends (`SINK_WEDGE_GRACE` posture — a wedged peer
                // is shed on channel close, never awaited).
                let publisher = publisher.clone();
                let outcome: Result<RunOutcome> = match egress_tx.take() {
                    Some(egress_tx) => {
                        // The per-tick state projection: publish the tick index to
                        // the wait-free slot AND push it onto the bounded drop-oldest
                        // egress via `try_send` — a wedged/slow consumer
                        // drops-and-counts, NEVER blocks the loop (inv #1 + #10).
                        let state_of = move |frame: &CompositedFrame| -> u64 {
                            let index = frame.tick.index;
                            if egress_tx.try_send(index).is_err() {
                                dropped.fetch_add(1, Ordering::Release);
                            }
                            index
                        };
                        inner
                            .run_with_control(
                                &publisher,
                                state_of,
                                |_frame: &CompositedFrame| None,
                                |_drive: &mut CompositorDrive<Nv12Image>| {},
                            )
                            .await
                    }
                    None => {
                        // No egress producer (already consumed by a prior attempt):
                        // run the clock + publish without egress — the clock
                        // guarantee (inv #1) is independent of any egress.
                        inner
                            .run_with_control(
                                &publisher,
                                |frame: &CompositedFrame| frame.tick.index,
                                |_frame: &CompositedFrame| None,
                                |_drive: &mut CompositorDrive<Nv12Image>| {},
                            )
                            .await
                    }
                };
                match outcome {
                    Ok(_) => ActorExit::Completed,
                    Err(error) => {
                        tracing::warn!(program = id.as_str(), %error, "program run failed; supervisor will restart");
                        ActorExit::Failed
                    }
                }
            }
        }
    }
}

impl<P> Drop for Program<P> {
    /// Best-effort egress-thread reclamation for a multiview program. If the egress
    /// producer was dropped (the run loop returned) the channel is closed, so a
    /// non-wedged consumer's `recv()` has returned and the thread is joinable
    /// promptly. The join is the drain's natural end on channel close; a
    /// permanently-wedged consumer is shed (the thread is left to the OS at process
    /// exit) rather than blocking teardown (`SINK_WEDGE_GRACE` posture). An external
    /// program owns no egress thread, so its `Drop` is trivial.
    fn drop(&mut self) {
        if let Runner::Multiview {
            egress_tx,
            egress_thread,
            ..
        } = &mut self.runner
        {
            // Drop any retained producer first so the channel closes and the
            // consumer's `recv()` ends.
            *egress_tx = None;
            if let Some(thread) = egress_thread.take() {
                // Join only if the consumer is not wedged: `is_finished` is the
                // non-blocking probe. A finished thread joins instantly; an
                // unfinished (wedged) one is shed — never block teardown on a stuck
                // sink.
                if thread.is_finished() {
                    let _ = thread.join();
                }
            }
        }
    }
}

/// The supervisor-side handle to one running program: its identity, its stop
/// signal, the wait-free cumulative-ticks counter, its cadence, and the egress
/// drop counter. Cloning the atomics with the program means the supervisor reads a
/// running program's progress without touching its hot loop.
struct ProgramHandle {
    stop: crate::runtime::StopSignal,
    ticks: Arc<AtomicU64>,
    egress_dropped: Arc<AtomicU64>,
    cadence: multiview_core::time::Rational,
    /// The program's handoff cell (multiview programs only; see
    /// [`crate::runtime::Handoff`]).
    handoff: Option<Arc<crate::runtime::Handoff>>,
    /// The program's tick-0 seed on the shared time source (multiview only).
    seed_nanos: Option<i64>,
    /// The program's dedicated clock thread; `None` only transiently during
    /// teardown.
    task: Option<ClockThread<()>>,
}

/// The **`ProgramSet`** supervisor: owns the `ProgramId → ProgramHandle` map and
/// the **one** shared read-only [`Arc<dyn TimeSource>`](crate::TimeSource) every
/// program reads, plus the [`RestartPolicy`] the per-program
/// supervisors use (ADR-0030 §2.2).
///
/// `P` is the [`Pacer`] all programs in this set share (production
/// [`RealtimePacer`](crate::RealtimePacer); cooperative test pacer in tests).
///
/// Invariant #1 (per program): each program owns an independent clock and awaits
/// only its own pacer. Invariant #10 (per program): each program has its own
/// isolated publisher + bounded drop-oldest egress; the supervisor only samples
/// wait-free atomics and never sits on a program's data plane.
pub struct ProgramSet<P> {
    /// The shared monotonic reference every program's clock reads (read-only).
    /// Retained so a future [`ProgramSet::start`] always hands the SAME reference
    /// — programs that are placed/co-located later still share one time base.
    time: Arc<dyn TimeSource>,
    /// The restart policy each per-program supervisor uses.
    policy: RestartPolicy,
    /// The running programs, keyed by identity.
    programs: HashMap<ProgramId, ProgramHandle>,
    /// Pacer-type marker: a [`ProgramSet`] runs programs that share the pacer `P`.
    _pacer: std::marker::PhantomData<fn() -> P>,
}

impl<P: Pacer + Send + 'static> ProgramSet<P> {
    /// Create an empty program set over a shared time source, using the default
    /// [`RestartPolicy`].
    #[must_use]
    pub fn new(time: Arc<dyn TimeSource>) -> Self {
        Self::with_policy(time, RestartPolicy::default_policy())
    }

    /// Create an empty program set over a shared time source with an explicit
    /// per-program [`RestartPolicy`].
    #[must_use]
    pub fn with_policy(time: Arc<dyn TimeSource>, policy: RestartPolicy) -> Self {
        Self {
            time,
            policy,
            programs: HashMap::new(),
            _pacer: std::marker::PhantomData,
        }
    }

    /// The shared monotonic time source every program in this set reads.
    #[must_use]
    pub fn time_source(&self) -> Arc<dyn TimeSource> {
        Arc::clone(&self.time)
    }

    /// Admit + start a program on its **own** dedicated output-clock thread
    /// (`clock-<id>`, #311 / ADR-0109) — **without touching** any other program
    /// (ADR-0030 §2.2). The program keeps its own independent clock; the
    /// supervisor records a handle to sample its progress and to stop it later.
    ///
    /// # Errors
    ///
    /// Returns [`Error::DuplicateProgram`] if a program with the same id is
    /// already running (ids are unique within a set), or
    /// [`Error::InvalidControlLoop`] if the OS refused to create the thread.
    pub fn start(&mut self, program: Program<P>) -> Result<()> {
        let id = program.id().clone();
        if self.programs.contains_key(&id) {
            return Err(Error::duplicate_program(id.as_str()));
        }
        let mut handle = program.handle();
        let supervisor = Supervisor::new(self.policy);
        // The per-program clock thread: the Supervisor drives the `Program` actor
        // (restarting it with capped backoff on failure while every sibling keeps
        // emitting) under `block_on` on a thread of its own, so the tick loop
        // shares no executor worker and no timer wheel with the control plane.
        // The thread owns the program; nothing here blocks the caller or any
        // other program.
        let task = crate::clock_thread::spawn(&format!("clock-{}", id.as_str()), move || {
            let _outcome = crate::clock_thread::block_on(supervisor.supervise(program));
        })
        .map_err(|error| Error::InvalidControlLoop(format!("clock thread for {id}: {error}")))?;
        handle.task = Some(task);
        self.programs.insert(id, handle);
        Ok(())
    }

    /// The tick-0 seed of the running multiview program `id` on the shared time
    /// source — what a successor is built with ([`Program::multiview_succeeding`]).
    #[must_use]
    pub fn seed_nanos(&self, id: &str) -> Option<i64> {
        let pid = ProgramId::new(id).ok()?;
        self.programs.get(&pid).and_then(|h| h.seed_nanos)
    }

    /// **Thread handoff** (#311, ADR-0109 §4): start `successor` on its own clock
    /// thread so it continues the running program `predecessor`'s exact tick
    /// timeline, then ask the predecessor to hand off. The predecessor emits
    /// every tick below the cut and stops; the successor emits from the cut on —
    /// no tick index is missed or emitted twice. The successor must have been
    /// built with the predecessor's seed and cadence. Await
    /// [`ProgramSet::stop`] on the predecessor afterwards to reap its thread.
    ///
    /// Returns the cut once the predecessor has chosen it (within a tick), or
    /// [`None`] if it did not within [`PROGRAM_JOIN_GRACE`].
    ///
    /// # Errors
    ///
    /// [`Error::DuplicateProgram`] for a duplicate successor id; an
    /// [`Error::InvalidControlLoop`] if the predecessor is not a running
    /// multiview program, the successor is not a multiview program, or the
    /// cadences or seeds differ.
    pub async fn start_successor(
        &mut self,
        predecessor: &str,
        mut successor: Program<P>,
    ) -> Result<Option<u64>> {
        let pid = ProgramId::new(predecessor)
            .map_err(|e| Error::InvalidControlLoop(format!("predecessor id: {e}")))?;
        let (handoff, seed, cadence) = match self.programs.get(&pid) {
            Some(ProgramHandle {
                handoff: Some(handoff),
                seed_nanos: Some(seed),
                cadence,
                ..
            }) => (Arc::clone(handoff), *seed, *cadence),
            _ => {
                return Err(Error::InvalidControlLoop(format!(
                    "{predecessor} is not a running multiview program"
                )))
            }
        };
        let successor_seed = match &successor.runner {
            Runner::Multiview { inner, .. } => inner.seed_nanos(),
            Runner::External(_) => seed.wrapping_add(1),
        };
        if successor.cadence != cadence || successor_seed != seed {
            return Err(Error::InvalidControlLoop(format!(
                "successor of {predecessor} must share its cadence and seed"
            )));
        }
        if !successor.succeed(Arc::clone(&handoff)) {
            return Err(Error::InvalidControlLoop(
                "a successor must be a multiview program".to_owned(),
            ));
        }
        self.start(successor)?;
        handoff.request();
        let started = std::time::Instant::now();
        while started.elapsed() < PROGRAM_JOIN_GRACE {
            if let Some(cut) = handoff.cut() {
                return Ok(Some(cut));
            }
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }
        Ok(handoff.cut())
    }

    /// Whether a program with `id` is currently in the set.
    #[must_use]
    pub fn is_running(&self, id: &str) -> bool {
        ProgramId::new(id).is_ok_and(|pid| self.programs.contains_key(&pid))
    }

    /// The ids of all currently-running programs (unordered).
    #[must_use]
    pub fn running_ids(&self) -> Vec<ProgramId> {
        self.programs.keys().cloned().collect()
    }

    /// Total ticks emitted so far by the program with `id`, or [`None`] if no such
    /// program is in the set. A single wait-free Acquire load — it never touches
    /// the program's hot loop (invariants #1 + #10).
    #[must_use]
    pub fn ticks_emitted(&self, id: &str) -> Option<u64> {
        let pid = ProgramId::new(id).ok()?;
        self.programs
            .get(&pid)
            .map(|h| h.ticks.load(Ordering::Acquire))
    }

    /// Frames shed at the egress of the program with `id` (a wedged/slow consumer),
    /// or [`None`] if no such program is in the set.
    #[must_use]
    pub fn egress_dropped(&self, id: &str) -> Option<u64> {
        let pid = ProgramId::new(id).ok()?;
        self.programs
            .get(&pid)
            .map(|h| h.egress_dropped.load(Ordering::Acquire))
    }

    /// The fixed cadence of the program with `id`, or [`None`].
    #[must_use]
    pub fn cadence(&self, id: &str) -> Option<multiview_core::time::Rational> {
        let pid = ProgramId::new(id).ok()?;
        self.programs.get(&pid).map(|h| h.cadence)
    }

    /// The number of programs currently in the set.
    #[must_use]
    pub fn len(&self) -> usize {
        self.programs.len()
    }

    /// Whether the set has no programs.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.programs.is_empty()
    }

    /// Stop **only** the program with `id`: raise its [`StopSignal`](crate::StopSignal)
    /// (its loop returns after its current tick), then join its supervised task.
    /// **No other program's clock is touched** (ADR-0030 §2.2). A no-op if no such
    /// program is in the set.
    ///
    /// Returns `true` if a program was stopped, `false` if `id` was not running.
    pub async fn stop(&mut self, id: &str) -> bool {
        let Ok(pid) = ProgramId::new(id) else {
            return false;
        };
        let Some(mut handle) = self.programs.remove(&pid) else {
            return false;
        };
        handle.stop.stop();
        if let Some(task) = handle.task.take() {
            // The program's loop returns after its current tick; the supervisor's
            // `supervise` then sees a clean `Completed` and the task ends — a join
            // bounded by one tick + teardown for any program that can observe its
            // stop. A program whose loop cannot reach its stop check (a wedged
            // pacer/clock) is aborted + detached at `PROGRAM_JOIN_GRACE` so teardown
            // is bounded, never an unbounded `await`.
            join_program_task_bounded(&pid, task).await;
        }
        true
    }

    /// Stop **every** program in the set (raise all stop signals, then join all
    /// tasks). After this returns the set is empty. Each program is stopped
    /// independently; one program's teardown never blocks another's.
    pub async fn shutdown(&mut self) {
        // Raise every stop signal FIRST so all programs wind down concurrently,
        // then join — rather than stop-and-join one at a time (which would serialize
        // teardown by one tick each).
        for handle in self.programs.values() {
            handle.stop.stop();
        }
        let drained: Vec<(ProgramId, ProgramHandle)> = self.programs.drain().collect();
        for (pid, mut handle) in drained {
            if let Some(task) = handle.task.take() {
                // Bounded join + abort-on-grace (see `join_program_task_bounded`):
                // one wedged program whose loop cannot observe stop is aborted +
                // detached rather than blocking the whole set's teardown forever.
                join_program_task_bounded(&pid, task).await;
            }
        }
    }
}
