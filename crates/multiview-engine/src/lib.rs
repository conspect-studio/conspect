//! # multiview-engine
//!
//! The **protected output core**: the fixed-cadence output clock, the compositor
//! drive loop, the actor supervisor, the engine→outside isolation wiring, and
//! the admission/degradation control loop. The library target is
//! `multiview_engine`.
//!
//! This crate is where Multiview's two load-bearing invariants live, and it treats
//! them as hard rules:
//!
//! * **Invariant #1 (output-clock).** One monotonic [`OutputClock`] emits exactly
//!   one valid, correctly-timestamped frame **per tick, forever**, independent of
//!   any input: `out_pts = f(tick)` via [`multiview_core::time::MediaTime::from_tick`],
//!   computed from the integer tick counter (never float-accumulated, never
//!   derived from an input). The [`CompositorDrive`] loop samples each tile's
//!   last-good frame without blocking and composites one frame per tick even when
//!   every input is absent. See [`clock`] and [`drive`].
//! * **Invariant #10 (isolation).** The engine publishes to control/preview
//!   **only** through a wait-free [`LatestState`] (newest-wins single slot,
//!   atomic store) and a bounded **drop-oldest** [`EventStream`] (built on
//!   `tokio::sync::broadcast`, `send` never blocks on a slow receiver); the
//!   publish side never `.await`s a consumer and never holds a lock a consumer
//!   can hold. A subscriber that never reads — or one that crashes, or one that
//!   holds every lock it can reach — cannot back-pressure the engine. See
//!   [`isolation`].
//! * **Integrated runtime (invariant #1 + #10).** [`EngineRuntime`] owns the
//!   per-tick driver loop: it paces to the wall-clock deadline via an injected
//!   [`TimeSource`] + [`Pacer`], advances the clock, composes one frame, and
//!   publishes state + an event — forever, never awaiting an input or a
//!   consumer. See [`runtime`].
//!
//! It also realizes:
//!
//! * **Invariant #2 (resilience).** The [`Supervisor`] restarts failed input/
//!   output actors with bounded backoff; a crashing actor never takes down the
//!   output clock. See [`supervisor`].
//! * **Invariant #9 (resource-adaptive degradation).** The [`ControlLoop`] runs
//!   sense→estimate→plan→apply over the `multiview-hal` cost model and hysteresis
//!   ladder, shedding cheapest-impact-first, tile-by-tile, before program output
//!   is touched. See [`degrade`].
//! * **Monitoring/alarm engine (ADR-MV001).** Content-aware [`probe`]s
//!   (black/freeze/format) read **sampled** last-good frames and report
//!   instantaneous conditions; the X.733 [`alarm`] engine applies
//!   dwell-up/dwell-down hysteresis, latch and acknowledge, rolls severities up
//!   probe→tile→group→system, evaluates Boolean virtual alarms, and maps a
//!   sustained alarm to a returned penalty-box layout action. Like the rest of
//!   the engine these are **pure state machines over an injected
//!   [`MediaTime`](multiview_core::time::MediaTime)** — best-effort samplers that
//!   never block, never `.await`, and so cannot stall the output clock or
//!   back-pressure the engine (invariants #1 + #10). See [`probe`] and [`alarm`].
//! * **Tally / salvo operator surface (M11, ADR-MV001).** The [`tally`] arbiter
//!   aggregates many external tally buses into one resolved per-tile state under a
//!   configurable profile and conflict policy (with a virtual [`tally::gpio`]
//!   GPI/GPO model); the [`salvo`] engine commits an atomic batch of changes on
//!   arm → take; the [`scheduler`] fires salvo/command actions on time and event
//!   triggers; [`heads`] resolves a multi-head wall; and [`cycle`] drives
//!   round-robin and freeze/reference-still tiles. All are **pure value machines
//!   over an injected [`MediaTime`](multiview_core::time::MediaTime)** that *return*
//!   commands/state for the engine to apply at a frame boundary — they never reach
//!   into or block the tick loop (invariants #1 + #10).
//!
//! * **PTP / ST 2059-2 servo (M12, gated transport).** The [`ptp`] module is the
//!   pure servo *math*: it filters noisy `(offset, delay)` PTP measurements into a
//!   smoothed disciplined-reference estimate. It is decisively **separate from the
//!   output clock** — `out_pts = f(tick)` is untouched, so a jittering/absent
//!   grandmaster can neither stall nor speed up the tick loop (invariant #1). The
//!   PTP NIC/PHC binding is behind the off-by-default `ptp` feature and is
//!   compile-verified only. See [`ptp`].
//!
//! * **High availability (M9, gated transport).** The [`ha`] module is the pure
//!   active/standby + N+1 model: a heartbeat health-check [`HaStateMachine`] over
//!   an injected [`MediaTime`](multiview_core::time::MediaTime) (on a miss-threshold
//!   the standby promotes), a deterministic failover [`Cluster`] /
//!   [`FailoverPolicy`] that elects exactly one output driver (no split-brain,
//!   fenced by an [`Epoch`]), and a serializable state-replication model
//!   ([`ha::repl`]). The HA machinery is decisively **separate from the output
//!   clock**: on the active node it only *samples* peer heartbeats and *returns* a
//!   decision, so a flapping/partitioned cluster can neither stall the tick loop
//!   (invariant #1) nor back-pressure the engine (invariant #10), and promotion is
//!   make-before-break so output never falters. The peer-socket / replication wire
//!   transport is behind the off-by-default `cluster` feature and compile-verified
//!   only. See [`ha`].
//!
//! The default build is pure Rust (no GPU, no FFmpeg): the drive loop runs the
//! CPU reference compositor from `multiview-compositor`, and the clock is driven by
//! an **injected** [`TimeSource`] so the whole engine is deterministic in tests.
#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod alarm;
pub mod clock;
pub mod cycle;
pub mod degrade;
pub mod drive;
pub mod error;
pub mod ha;
pub mod heads;
pub mod isolation;
pub mod placement;
pub mod probe;
pub mod program;
pub mod programset;
pub mod ptp;
pub mod route;
pub mod runtime;
pub mod salvo;
pub mod scheduler;
pub mod slate;
pub mod supervisor;
pub mod sysref;
pub mod tally;

pub use alarm::{
    black_config_from_kind, engine_zone, freeze_config_from_kind, AlarmHysteresis,
    AlarmStateMachine, AlarmTransition, BoolOp, PenaltyAction, PenaltyBox, PenaltyConfig,
    PenaltyState, Phase, RollupNode, VirtualAlarm,
};
pub use clock::{ManualTimeSource, MonotonicTimeSource, OutputClock, Tick, TimeSource};
pub use cycle::{FreezeTile, RoundRobin};
pub use degrade::{ControlLoop, ControlStep};
pub use drive::{CompositedFrame, CompositorDrive};
pub use error::{Error, Result};
pub use ha::{
    Cluster, Epoch, FailoverDecision, FailoverPolicy, HaNode, HaStateMachine, Heartbeat,
    HeartbeatConfig, NodeId, NodeRole, PeerHealth, Priority,
};
pub use heads::{HeadBinding, HeadPlacement, WallComposition};
pub use isolation::{
    event_stream, EnginePublisher, EventStream, EventSubscription, LatestState, RecvError,
    SeqEvent, TryRecvError,
};
pub use placement::{
    MigrationPlan, PlacementController, PlacementControllerConfig, PlacementProposal, ShedReason,
};
pub use probe::{
    BlackConfig, BlackProbe, DetectionZone, ExpectedFormat, FormatAxis, FormatMismatch,
    FormatProbe, FreezeConfig, FreezeProbe, LumaView, LumaViewError, ProbeObservation,
};
pub use program::{MultiviewProgram, ProgramId, ProgramKind};
pub use programset::{Program, ProgramSet};
pub use ptp::{PtpSample, PtpServo, ServoConfig};
pub use route::{resolve_selector, RouteApplier, RouteIntent, RouteResolution};
pub use runtime::{
    CooperativePacer, EngineRuntime, Pacer, RealtimePacer, RunOutcome, RunStop, StopSignal,
};
pub use salvo::{Salvo, SalvoBatch, SalvoChange, SalvoPhase};
pub use scheduler::{EventKind, ScheduledAction, Scheduler, TriggerEvent};
pub use slate::{failover_slate_image, FailoverSlate};
#[cfg(feature = "ffmpeg")]
pub use slate::{output_slate_audio, output_slate_kind};
pub use supervisor::{
    Actor, ActorExit, RestartDecision, RestartPolicy, StopReason, SupervisionOutcome, Supervisor,
};
pub use sysref::{
    classify_system, NtpClockState, NtpQuery, NtpReading, NtpStatusFlags, ReferenceSelector,
    SelectedReference, SystemRefConfig, SystemRefTracker,
};
pub use tally::{
    BitMapping, ConflictPolicy, Edge, GpiPoint, GpoPoint, LatchPolicy, Polarity, TallyArbiter,
    TallyFact, TallyProfile,
};
