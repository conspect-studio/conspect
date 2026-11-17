//! Bounded, nonblocking control-to-tick handoff for the first M/E.
//!
//! The data plane is the sole [`SwitcherTickDriver`] owner. No REST task can
//! lock its state or block its clock; admitted intents use `try_send`, and the
//! driver performs a fixed maximum of `try_recv` calls on each output tick.

use multiview_core::layout::Layout;
use multiview_core::switcher::{FrameCount, SourceRef, TransitionDescriptor};
use std::sync::Arc;

use crate::switcher_plan::validate_mix;
use multiview_compositor::pipeline::Nv12Image;
use multiview_framestore::TileStore;
use std::collections::HashMap;
use tokio::sync::mpsc;

use crate::clean_gate::{CleanDskGate, DskReservation, TickRefusal};
use crate::clock::Tick;
use crate::isolation::{event_stream, EventStream, EventSubscription, LatestState};
use crate::keyer::{
    KeyerAction, KeyerCounters, KeyerDef, KeyerDefinition, KeyerDefinitions, KeyerDraw, KeyerEvent,
    KeyerReadback, KeyerRuntime, KeyerSnapshot, KeyerTally, NextTake, OpSink, TieEnvelope,
    MAX_KEYERS, REFUSE_KEYER_CLEAN, REFUSE_KEYER_GATE_CONTENDED, REFUSE_KEYER_SET_DEADLINE,
};
use crate::patch_set::{ArmState, SetArm, SetRefusal, SET_ADOPT_DEADLINE_TICKS};
use crate::switcher::{SwitcherProjection, SwitcherState};
use crate::switcher_inputs::{InputsReadback, MixerInputs, INPUT_SLOTS, MAX_PATCH_MEMBERS};

/// Maximum command applications in one output frame.
pub const MAX_COMMANDS_PER_TICK: usize = 16;

/// Does this authored scene admit `source` on its bus?
///
/// A scene with exactly one full-canvas, fully opaque cell is the first slice's
/// source-agnostic bus canvas: the M/E machine's selection decides which source
/// fills it and the frame hook rebinds that one cell on every change, so the
/// cell's placeholder source is not a membership rule. Every other (multi-cell
/// or windowed) scene keeps the strict membership check: a source that is not
/// authored into the scene is refused with a typed error rather than silently
/// selected.
#[allow(clippy::float_cmp)]
// reason: these are authored layout fractions compared against their exact
// canonical values (0.0/1.0) — an approximate comparison would admit a
// windowed scene as "full canvas" and change which sources are admissible.
fn scene_admits_source(scene: &Layout, source: &str) -> bool {
    if scene
        .cells
        .iter()
        .any(|cell| cell.source.as_deref() == Some(source))
    {
        return true;
    }
    match scene.cells.as_slice() {
        [cell] => {
            cell.x == 0.0 && cell.y == 0.0 && cell.w == 1.0 && cell.h == 1.0 && cell.opacity == 1.0
        }
        _ => false,
    }
}
/// Maximum queued commands, including unresponsive data-plane scenarios.
pub const COMMAND_CAPACITY: usize = 64;

/// Output ticks a take may wait for its incoming source's first fresh frame
/// before it is refused (ADR-0104). The clock never waits; the take does.
pub const TAKE_PRIME_DEADLINE_TICKS: u64 = 50;
/// Ticks after admission (the clock's last composed tick when the control
/// plane built the verb) within which a queued `keyer_set` must be drained;
/// drained at or after its deadline tick it is refused and never applies
/// (review H4, mirroring [`TAKE_PRIME_DEADLINE_TICKS`]). Once the clock has
/// composed the deadline tick, the command can no longer apply, so the
/// control plane's settlement always reaches a terminal answer.
pub const KEYER_SET_DEADLINE_TICKS: u64 = TAKE_PRIME_DEADLINE_TICKS;

/// Typed refusal: the incoming source had no fresh frame before the deadline.
pub const REFUSE_NO_FRESH_FRAME: &str =
    "incoming source was not primed with a fresh frame before the bounded take deadline; Program is unchanged";
/// Typed refusal: the AUTO's incoming source lost its fresh frame before the
/// terminal tick (review SW2-02). Program keeps the outgoing source.
pub const REFUSE_LOST_DURING_AUTO: &str =
    "incoming source lost its fresh frame during AUTO; the take was aborted and Program kept the outgoing source";
/// Typed refusal: Preview already carries the Program source (#247).
pub const REFUSE_SAME_SOURCE: &str =
    "preview and program carry the same source; select a different preview source";

/// Typed refusal: a patch member targets the input on PGM under `protect`.
pub const REFUSE_ON_AIR_PROTECTED: &str =
    "on-air-protected: the patch re-patches the input on Program; retry with on_air=allow to cut Program";
/// Typed refusal: a patch member targets the PGM or PVW input during an AUTO.
pub const REFUSE_PATCH_DURING_AUTO: &str =
    "on-air-protected: the patch targets the Program or Preview input while an AUTO is in flight";
/// Typed refusal: a select names a raw source on an M/E with numbered inputs.
pub const REFUSE_NOT_AN_INPUT: &str =
    "not-on-an-input: this M/E selects numbered inputs; select an input, not a source";
/// Typed refusal: the transition kind or one of its options is not rendered.
pub const REFUSE_UNSUPPORTED_TRANSITION: &str =
    "unsupported-transition: this engine renders cut, mix, dip to a colour and wipe";

/// Is `descriptor` a transition the Program drive renders (ADR-0112)?
#[must_use]
pub fn is_rendered_transition(descriptor: &TransitionDescriptor) -> bool {
    descriptor.is_rendered()
}

/// Typed refusal: an `arm` names a key that is not a defined upstream
/// keyer of this M/E (K3).
pub const REFUSE_ARM_UNKNOWN_KEY: &str =
    "unknown-keyer: an armed key is not a defined upstream keyer of this M/E";

/// Typed refusal: a select names an undeclared or unpatched input.
pub const REFUSE_UNKNOWN_INPUT: &str = "unknown-slot: the input is not declared or not patched";

/// A set of mixer-input crosspoints applied at one tick, all or none
/// (ADR-0110 §12.8). Bounded by the control plane to 64 members.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PatchSet {
    /// `(input number, new source)`; `None` unpatches the input.
    pub members: Vec<(u16, Option<SourceRef>)>,
    /// `on_air = "allow"`: a member may re-patch the input on PGM (a cut).
    pub allow_on_air: bool,
}

/// An operator verb, already authorized and reference-validated on admission.
#[derive(Debug, Clone, PartialEq)]
pub enum SwitcherVerb {
    /// Arm a new preview source, without changing on-air output. With numbered
    /// inputs installed this must be an input reference (`in:<n>`).
    SelectPreview(SourceRef),
    /// Punch a source directly onto on-air PGM at the next frame boundary.
    /// With numbered inputs installed this must be an input reference.
    SelectProgram(SourceRef),
    /// Atomically take PVW to PGM at the next frame boundary.
    Cut,
    /// Begin an exact-duration automatic take on the next frame boundary,
    /// in the selected transition kind (ADR-0112 §3).
    Auto(FrameCount),
    /// Select the transition the next AUTO uses (ADR-0112 §3). Refused
    /// during a take and for a kind this engine does not render.
    SetTransition(TransitionDescriptor),
    /// Re-patch numbered mixer inputs at one tick (all members or none).
    Patch(PatchSet),
    /// Downstream keyer `keyer` (config order) on or off at the next tick
    /// (ADR-0113 §5). Going on passes the fresh gate; going off never waits.
    /// Engine-internal: the control plane maps keyer ids to indices (K4).
    KeyerCut {
        /// Keyer index.
        keyer: u8,
        /// Target state.
        on: bool,
    },
    /// Ramp downstream keyer `keyer` on or off over `rate` frames (its own
    /// configured rate when `None`).
    KeyerAuto {
        /// Keyer index.
        keyer: u8,
        /// Target state.
        on: bool,
        /// Ramp duration.
        rate: Option<FrameCount>,
    },
    /// Tie (or untie) downstream keyer `keyer` to the next background take.
    KeyerTie {
        /// Keyer index.
        keyer: u8,
        /// Tied.
        tie: bool,
    },
    /// Define or change downstream keyer `keyer` at one tick (ADR-0113 §10,
    /// K4): a class-1 change whose definition and stores the control plane
    /// resolved and admitted off the clock. A free index defines a new keyer.
    KeyerSet {
        /// Keyer index (a free slot defines a new keyer).
        keyer: u8,
        /// The resolved definition (moved in; never cloned per tick).
        def: KeyerDefinition,
        /// The committed definitions after this change, built off the clock
        /// ([`SwitcherControl::keyer_set_verb`]); the tick checks it by
        /// pointer identity and publishes it, allocating nothing (review
        /// H6). A stale one (the base changed) refuses the verb.
        readback: KeyerReadback,
        /// `on_air = "allow"`: the fill or key of a keyer on air may change.
        allow_on_air: bool,
        /// Drained at or after this tick, the verb is refused
        /// ([`REFUSE_KEYER_SET_DEADLINE`]) and never applies (review H4).
        deadline: u64,
    },
    /// Select the elements the next background transition moves (ADR-0113
    /// §4, slice K3): `{background} ∪ {USK keys}`. Refused during a take,
    /// for an empty set, or for a key that is not a defined upstream keyer.
    /// Built off the clock; the tick only moves the `Arc` in.
    Arm(Arc<multiview_core::switcher::ArmedSet>),
    /// A verb the control plane authorized against a specific committed
    /// arm set and keyer definitions (review AUTHZ-1/-2): drained, it is
    /// refused ([`REFUSE_ARM_CHANGED`] / [`crate::keyer::REFUSE_KEYER_DEFINITION_CHANGED`])
    /// when either version moved since, so a take or keyer cut never
    /// toggles a keyer the caller was not authorized for. Built (and boxed)
    /// off the clock; the tick only compares two integers and moves the
    /// inner verb out.
    Bound(Box<BoundVerb>),
    /// The mixer half of an atomic multi-crosspoint set (ADR-0110 §12.8,
    /// #308): validated and primed here, then applied only at the adoption
    /// tick the router proposes through `arm`, never on its own. Held in its
    /// own slot, so an armed set never blocks a Cut or AUTO.
    PatchArmed {
        /// The mixer-input members.
        set: PatchSet,
        /// The handshake cell shared with the router.
        arm: Arc<SetArm>,
    },
}

/// The payload of [`SwitcherVerb::Bound`].
#[derive(Debug, Clone, PartialEq)]
pub struct BoundVerb {
    /// The verb itself (never another `Bound`).
    pub verb: SwitcherVerb,
    /// The arm-set version the caller was authorized against
    /// ([`SwitcherSnapshot::arm_version`]), or `None` when it does not
    /// matter (a keyer verb).
    pub arm_version: Option<u64>,
    /// The keyer definitions version the caller was authorized against
    /// ([`KeyerDefinitions::version`]), or `None`.
    pub defs_version: Option<u64>,
}

/// Typed refusal: a take authorized against an arm set that changed before
/// its tick (review AUTHZ-1). Nothing changes; the caller re-plans.
pub const REFUSE_ARM_CHANGED: &str =
    "arm-changed: the armed set changed since this take was authorized; re-plan it";
/// Typed refusal: `arm` while a take is held for a fresh source (review
/// K3-INV-1: the held take keeps the set it was admitted with).
pub const REFUSE_ARM_TAKE_HELD: &str =
    "take-held: a take is held waiting for a fresh frame; arm after it lands or is refused";

/// Ticks past [`SET_ADOPT_DEADLINE_TICKS`] the switcher waits for a ready
/// set's router half before it refuses the set `deadline` itself: the
/// router admits the set later than the clock (it runs on the bake
/// consumer) and owns the set's first cause, so its refusal normally lands
/// first.
pub const SET_SWITCHER_GRACE_TICKS: u64 = 16;

/// Typed refusal: a second armed set while one is held.
pub const REFUSE_SET_ARMED: &str =
    "engine-busy: a salvo is already armed on this M/E; retry when it settled";

/// The arm-set and keyer-definitions versions a [`SwitcherVerb::Bound`]
/// verb was authorized against (`None`: not bound on that axis). `Copy`, so
/// holding it costs nothing on the clock.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Bound {
    arm_version: Option<u64>,
    defs_version: Option<u64>,
}

/// The single held take (a CUT/AUTO/punch/patch waiting for a fresh frame).
/// Review round 2 (MEDIA-3, AUTHZ-3, K3-BOUND-1): it keeps the versions it
/// was authorized against and is refused, never retargeted, when either
/// moved before it lands.
#[derive(Debug)]
struct HeldTake {
    command: SwitcherCommand,
    deadline: u64,
    bound: Bound,
}

/// The armed mixer half of a set (one slot, separate from the held take).
#[derive(Debug)]
struct ArmedSet {
    operation_id: String,
    set: PatchSet,
    arm: Arc<SetArm>,
    /// Refused `feed-not-fresh` when still not ready at this tick.
    deadline: u64,
    /// The tick whose drain applied the patch (commit/reject resolves it).
    applied_at: Option<u64>,
}

/// One correlated command; `operation_id` is opaque and never interpreted by
/// the data plane. The caller owns idempotency before enqueue.
#[derive(Debug, Clone)]
pub struct SwitcherCommand {
    /// Stable operation identifier returned by the 202 response.
    pub operation_id: String,
    /// The scene generation validated at API admission. Rechecked at the tick.
    pub scene_version: u64,
    /// The operator action.
    pub verb: SwitcherVerb,
}

/// Engine-owned bus selection projected at a real output frame boundary.
#[derive(Debug, Clone, PartialEq)]
pub struct SwitcherSnapshot {
    /// Output tick on which this snapshot was published.
    pub tick: u64,
    /// Current PGM source id (resolved through the input table, if any).
    pub program: SourceRef,
    /// Current PVW source id (resolved through the input table, if any).
    pub preview: SourceRef,
    /// Current PGM input number when numbered inputs are installed.
    pub program_input: Option<u16>,
    /// Current PVW input number when numbered inputs are installed.
    pub preview_input: Option<u16>,
    /// Bumped once per applied patch; 0 without inputs or before any patch.
    pub inputs_version: u64,
    /// Whether an automatic transition is in progress.
    pub in_flight: bool,
    /// Number of completed takes, monotonic within one runtime.
    pub completion_count: u64,
    /// The operator's selected transition, as the machine holds it after
    /// this tick (ADR-0112 §4). An AUTO never changes it: the flight renders
    /// its own descriptor (the selection at the AUTO's rate).
    pub transition: TransitionDescriptor,
    /// Keyer rows by runtime slot: DSKs, then USKs (ADR-0113 §8). Ramps
    /// are descriptors (start, rate, direction), never per-tick weights.
    pub keyers: [Option<KeyerSnapshot>; MAX_KEYERS],
    /// What the next background transition moves (the `arm` set, K3).
    pub next: NextTake,
    /// Bumped once per applied `arm` (K3, review AUTHZ-1): a take binds to
    /// it.
    pub arm_version: u64,
    /// Engine tally as roles (design note §9, deviation: roles, not store
    /// indices; the control plane resolves them to source ids).
    pub tally: KeyerTally,
}

/// Definitive command result after the engine has processed it at a tick.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SwitcherOutcome {
    /// Operation identifier correlated with the accepted 202.
    pub operation_id: String,
    /// Exact tick on which the engine applied or rejected the command.
    pub tick: u64,
    /// `true` means the operation reached its terminal applied frame, not merely armed.
    pub completed: bool,
    /// `None` means applied; otherwise typed machine or geometry refusal.
    pub error: Option<String>,
}

/// A cloneable control-plane handle; it cannot mutate the machine directly.
#[derive(Debug, Clone)]
pub struct SwitcherControl {
    tx: mpsc::Sender<SwitcherCommand>,
    snapshots: LatestState<SwitcherSnapshot>,
    outcomes: EventStream<SwitcherOutcome>,
    scenes: LatestState<ScenePair>,
    readiness: LatestState<ProviderStatus>,
    applied_tick: Arc<std::sync::atomic::AtomicU64>,
    composed_count: Arc<std::sync::atomic::AtomicU64>,
    auto_capable: Arc<std::sync::atomic::AtomicBool>,
    inputs: LatestState<InputsReadback>,
    keyer_events: EventStream<KeyerEvent>,
    keyer_counters: Arc<KeyerCounters>,
    keyer_defs: LatestState<KeyerDefinitions>,
    /// The run's frame stores, published once at attach so `keyer_set` can
    /// resolve a definition off the clock (K4).
    stores: LatestState<HashMap<String, Arc<TileStore<Nv12Image>>>>,
    /// The Clean / DSK admission point (review H3): counted DSK and Clean
    /// reservations plus the committed "defined" bit, shared with the
    /// routing handle and the router (K2a deviation until K2b).
    clean_gate: Arc<CleanDskGate>,
    /// The `keyer` demand consumer set (ADR-0113 §4), installed at attach so
    /// a committed `keyer_set` republishes the fill/key leases off the clock.
    keyer_demand: Arc<std::sync::OnceLock<crate::ConsumerSet<Nv12Image>>>,
}

/// Actual paired-bus frame capability, published after the rendering provider
/// has produced a frame at this generation and pinned signal format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProviderStatus {
    /// Scene version whose two buses were composited.
    pub scene_version: u64,
    /// Real output tick whose frame is available, not an optimistic counter.
    pub last_tick: u64,
    /// Canvas width.
    pub width: u32,
    /// Canvas height.
    pub height: u32,
    /// Exact output fps numerator.
    pub fps_num: i64,
    /// Exact output fps denominator.
    pub fps_den: i64,
}

/// Versioned, prevalidated render scenes. A command never references mutable
/// control-side scene geometry; the engine resolves this immutable pair at arm.
#[derive(Debug, Clone)]
pub struct ScenePair {
    /// Monotonic authored-scene generation.
    pub version: u64,
    /// Current on-air scene geometry.
    pub program: Arc<Layout>,
    /// Armed preview scene geometry.
    pub preview: Arc<Layout>,
}

impl SwitcherControl {
    /// Construct a bounded mailbox and engine-owned driver; initial state is not
    /// reported as applied until the first output tick calls `drain_at_tick`.
    #[must_use]
    pub fn new(machine: SwitcherState) -> (Self, SwitcherTickDriver) {
        let (tx, rx) = mpsc::channel(COMMAND_CAPACITY);
        let snapshots = LatestState::new();
        let (outcomes, _) = event_stream(256);
        let (keyer_events, _) = event_stream(256);
        let handle = Self {
            tx,
            snapshots,
            outcomes,
            scenes: LatestState::new(),
            readiness: LatestState::new(),
            applied_tick: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            composed_count: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            auto_capable: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            inputs: LatestState::new(),
            keyer_events,
            keyer_counters: Arc::new(KeyerCounters::default()),
            keyer_defs: LatestState::new(),
            stores: LatestState::new(),
            clean_gate: CleanDskGate::new(),
            keyer_demand: Arc::new(std::sync::OnceLock::new()),
        };
        let driver = SwitcherTickDriver {
            rx,
            machine,
            snapshots: handle.snapshots.clone(),
            outcomes: handle.outcomes.clone(),
            scenes: handle.scenes.clone(),
            readiness: handle.readiness.clone(),
            applied_tick: Arc::clone(&handle.applied_tick),
            composed_count: Arc::clone(&handle.composed_count),
            auto_capable: Arc::clone(&handle.auto_capable),
            active_auto: None,
            active_auto_scenes: None,
            primed_stores: None,
            live_stores: HashMap::new(),
            pending_take: None,
            drain_bound: Bound::default(),
            armed: None,
            last_tick: 0,
            staged_success: Vec::with_capacity(MAX_COMMANDS_PER_TICK),
            spare_success: Vec::with_capacity(MAX_COMMANDS_PER_TICK),
            before_staged: None,
            before_arm: None,
            inputs: None,
            inputs_version: 0,
            published_inputs_version: 0,
            inputs_readback: handle.inputs.clone(),
            keyers: Box::default(),
            keyer_events: handle.keyer_events.clone(),
            keyer_counters: Arc::clone(&handle.keyer_counters),
            keyer_defs: handle.keyer_defs.clone(),
            published_def_version: 0,
            staged_readback: None,
            definition_builds: 0,
            stores: handle.stores.clone(),
            clean_gate: Arc::clone(&handle.clean_gate),
            staged_dsk: None,
            arm_version: 0,
        };
        let mut driver = driver;
        driver.keyers.set_next(NextTake::of(driver.machine.armed()));
        (handle, driver)
    }

    /// Admit without waiting; a full/closed queue is an explicit refusal and
    /// MUST NOT receive a 202 response.
    ///
    /// # Errors
    /// Returns the command unchanged if the bounded mailbox is full or closed.
    pub fn try_send(&self, command: SwitcherCommand) -> Result<(), SwitcherCommand> {
        self.tx
            .try_send(command)
            .map_err(tokio::sync::mpsc::error::TrySendError::into_inner)
    }

    /// Whether the engine side of the mailbox is gone (the run ended): no
    /// queued command can apply any more.
    #[must_use]
    pub fn is_closed(&self) -> bool {
        self.tx.is_closed()
    }

    /// Publish an immutable pair after validating its layout geometry. This is
    /// control-side only; a later edit cannot mutate a previously published pair.
    ///
    /// # Errors
    /// Refuses invalid source scenes; an AUTO additionally validates the exact
    /// pair captured at the engine tick before arming.
    pub fn set_scenes(&self, pair: ScenePair) -> Result<(), String> {
        pair.program.validate().map_err(|e| e.to_string())?;
        pair.preview.validate().map_err(|e| e.to_string())?;
        self.readiness.publish(ProviderStatus {
            scene_version: pair.version,
            last_tick: 0,
            width: 0,
            height: 0,
            fps_num: 0,
            fps_den: 0,
        });
        self.scenes.publish(pair);
        Ok(())
    }

    /// Most recently applied scene (change-only publication) with the freshest
    /// real frame tick from a cheap atomic load; never a predicted state.
    #[must_use]
    pub fn snapshot(&self) -> Option<std::sync::Arc<SwitcherSnapshot>> {
        self.snapshots.latest()
    }

    /// Number of committed frames, independent of skipped tick indices.
    #[must_use]
    pub fn composed_count(&self) -> u64 {
        self.composed_count
            .load(std::sync::atomic::Ordering::Acquire)
    }

    /// Last freshly published tick, independent of change-only state snapshots.
    #[must_use]
    pub fn applied_tick(&self) -> u64 {
        self.applied_tick.load(std::sync::atomic::Ordering::Acquire)
    }

    /// The shared cell behind [`Self::applied_tick`]: the router reads the
    /// clock's last composed tick from it (wait-free) to pick a set's
    /// adoption tick (#308).
    #[must_use]
    pub fn applied_tick_cell(&self) -> Arc<std::sync::atomic::AtomicU64> {
        Arc::clone(&self.applied_tick)
    }

    /// Real paired-bus publication status, or `None` before the first frame.
    /// An invalidated status (`width=0`) never admits a command.
    #[must_use]
    pub fn provider_status(&self) -> Option<ProviderStatus> {
        let status = *self.readiness.latest()?;
        if status.width == 0 || status.height == 0 || status.fps_num <= 0 || status.fps_den <= 0 {
            return None;
        }
        let scene = self.scenes.latest()?;
        (status.scene_version == scene.version).then_some(status)
    }

    /// `true` only after the actual paired compositor published frames.
    #[must_use]
    pub fn is_ready(&self) -> bool {
        self.provider_status().is_some()
    }

    /// `true` only once the frame hook that renders the AUTO dissolve inside
    /// the Program drive is installed (ADR-0104 §3); never at config load.
    #[must_use]
    pub fn supports_auto(&self) -> bool {
        self.auto_capable.load(std::sync::atomic::Ordering::Acquire)
    }

    /// Scene generation presently available for command admission. This is
    /// descriptive only: the caller must verify a real bus frame provider is
    /// attached before returning 202; a scene snapshot alone is not capability.
    #[must_use]
    pub fn scene_version(&self) -> Option<u64> {
        self.scenes.latest().map(|pair| pair.version)
    }

    /// The engine's live mixer-input table, published once at install and then
    /// only on a committed patch tick. `None` when the M/E has no inputs.
    #[must_use]
    pub fn inputs(&self) -> Option<Arc<InputsReadback>> {
        self.inputs.latest()
    }

    /// Subscribe to correlated, drop-oldest applied outcomes. A lagged reader
    /// must re-fetch state and operation status; it never stalls the output tick.
    #[must_use]
    pub fn outcomes(&self) -> EventSubscription<SwitcherOutcome> {
        self.outcomes.subscribe()
    }

    /// Subscribe to downstream keyer state changes (`switcher.keyer`,
    /// ADR-0113 §4). Bounded drop-oldest: published only on committed state
    /// changes, never per ramp tick; a lagged reader re-fetches the snapshot.
    #[must_use]
    pub fn keyer_events(&self) -> EventSubscription<KeyerEvent> {
        self.keyer_events.subscribe()
    }

    /// Keyer counters (oversize frames, dropped events, source-lost drops).
    #[must_use]
    pub fn keyer_counters(&self) -> Arc<KeyerCounters> {
        Arc::clone(&self.keyer_counters)
    }

    /// The committed downstream keyer definitions (K4): published at attach
    /// and then only on a committed `keyer_set` tick. `None` when no keyer
    /// runtime was installed (no M/E keyers on this run).
    #[must_use]
    pub fn keyer_definitions(&self) -> Option<Arc<KeyerDefinitions>> {
        self.keyer_defs.latest()
    }

    /// Resolve an accepted keyer definition against this run's frame stores
    /// and canvas, off the clock (K4 `keyer_set`). The result is moved into
    /// [`SwitcherVerb::KeyerSet`].
    ///
    /// # Errors
    /// No keyer runtime on this run, or a fill or key without a frame store.
    pub fn resolve_keyer(
        &self,
        config: &multiview_config::DownstreamKeyerConfig,
    ) -> Result<KeyerDefinition, String> {
        let defs = self
            .keyer_defs
            .latest()
            .ok_or_else(|| "this run has no downstream keyer runtime".to_owned())?;
        let stores = self
            .stores
            .latest()
            .ok_or_else(|| "this run has no frame stores attached".to_owned())?;
        let (w, h) = defs.canvas;
        KeyerDef::from_config(config, &stores, w, h).map(|def| KeyerDefinition(Arc::new(def)))
    }

    /// Resolve an upstream keyer definition (K3) of M/E `me` on `program`
    /// against this run's frame stores and canvas, off the clock.
    ///
    /// # Errors
    /// As [`Self::resolve_keyer`].
    pub fn resolve_usk(
        &self,
        config: &multiview_config::UpstreamKeyerConfig,
        me: &str,
        program: &str,
    ) -> Result<KeyerDefinition, String> {
        let defs = self
            .keyer_defs
            .latest()
            .ok_or_else(|| "this run has no keyer runtime".to_owned())?;
        let stores = self
            .stores
            .latest()
            .ok_or_else(|| "this run has no frame stores attached".to_owned())?;
        let (w, h) = defs.canvas;
        KeyerDef::from_usk(config, me, program, &stores, w, h)
            .map(|def| KeyerDefinition(Arc::new(def)))
    }

    /// Build the [`SwitcherVerb::KeyerSet`] for `def` at `keyer`, admitted
    /// against the committed definitions `base`: prepares the readback the
    /// committing tick publishes (off the clock, review H6) and stamps the
    /// deadline tick (the last composed tick + [`KEYER_SET_DEADLINE_TICKS`],
    /// review H4).
    #[must_use]
    pub fn keyer_set_verb(
        &self,
        base: &KeyerDefinitions,
        keyer: u8,
        def: KeyerDefinition,
        allow_on_air: bool,
    ) -> SwitcherVerb {
        let readback = KeyerReadback(Arc::new(base.with_definition(keyer, &def.0)));
        SwitcherVerb::KeyerSet {
            keyer,
            def,
            readback,
            allow_on_air,
            deadline: self.applied_tick().saturating_add(KEYER_SET_DEADLINE_TICKS),
        }
    }

    /// The shared Clean / DSK admission point (review H3). Its "defined"
    /// bit is set at attach (declared DSKs) and by the commit of a tick that
    /// defined a keyer. The control plane holds a DSK reservation from
    /// before it enqueues a `keyer_set` defining a new keyer until that
    /// command settles. The router holds one Clean reservation per output on
    /// or taking Clean.
    #[must_use]
    pub fn clean_gate(&self) -> Arc<CleanDskGate> {
        Arc::clone(&self.clean_gate)
    }

    /// Install the run's `keyer` demand set (once, at attach).
    pub fn set_keyer_demand(&self, set: crate::ConsumerSet<Nv12Image>) {
        let _ = self.keyer_demand.set(set);
    }

    /// The run's `keyer` demand set, if installed.
    #[must_use]
    pub fn keyer_demand(&self) -> Option<&crate::ConsumerSet<Nv12Image>> {
        self.keyer_demand.get()
    }
}

/// Sole output-clock owner of the M/E state; do not share with REST handlers.
#[derive(Debug)]
pub struct SwitcherTickDriver {
    rx: mpsc::Receiver<SwitcherCommand>,
    machine: SwitcherState,
    snapshots: LatestState<SwitcherSnapshot>,
    outcomes: EventStream<SwitcherOutcome>,
    scenes: LatestState<ScenePair>,
    active_auto: Option<String>,
    active_auto_scenes: Option<Arc<ScenePair>>,
    readiness: LatestState<ProviderStatus>,
    applied_tick: Arc<std::sync::atomic::AtomicU64>,
    composed_count: Arc<std::sync::atomic::AtomicU64>,
    auto_capable: Arc<std::sync::atomic::AtomicBool>,
    primed_stores: Option<HashMap<String, Arc<TileStore<Nv12Image>>>>,
    /// The Program drive's current store set (issue #464), owned by the
    /// driver and resynced by the frame hook only when the drive's store
    /// generation changes, so a source added live can be primed, patched,
    /// taken and monitored. Consulted before the boot map: a source removed
    /// and re-added live has a NEW store here. No shared state is read.
    live_stores: HashMap<String, Arc<TileStore<Nv12Image>>>,
    pending_take: Option<HeldTake>,
    /// The versions the command being drained was bound to (review round
    /// 2 K3-BOUND-1): a take held by the fresh gate keeps them.
    drain_bound: Bound,
    /// The armed mixer half of an atomic set (#308), if any.
    armed: Option<ArmedSet>,
    last_tick: u64,
    staged_success: Vec<String>,
    spare_success: Vec<String>,
    before_staged: Option<Staged>,
    /// The arm set and version before this tick's first `arm`, when nothing
    /// else staged the full machine (review K3-R3-2): an `arm` only moves an
    /// `Arc`, so restoring it needs no `SwitcherState` clone on the clock.
    before_arm: Option<(Arc<multiview_core::switcher::ArmedSet>, u64)>,
    /// Numbered mixer inputs (§12.2); `None` = PGM/PVW select sources by id.
    inputs: Option<MixerInputs>,
    inputs_version: u64,
    published_inputs_version: u64,
    inputs_readback: LatestState<InputsReadback>,
    /// Downstream keyers (ADR-0113); empty unless [`Self::set_keyers`].
    /// Boxed: built once at attach, it keeps the fixed keyer state out of
    /// the driver (and so out of the output loop's future).
    keyers: Box<KeyerRuntime>,
    keyer_events: EventStream<KeyerEvent>,
    keyer_counters: Arc<KeyerCounters>,
    keyer_defs: LatestState<KeyerDefinitions>,
    published_def_version: u64,
    /// The readback of this tick's applied `keyer_set` (published at the
    /// commit, dropped by a reject).
    staged_readback: Option<Arc<KeyerDefinitions>>,
    /// Odometer: `KeyerDefinitions` readbacks the clock built itself (an
    /// allocation on the clock); review H6 pins it at the attach build.
    definition_builds: u64,
    stores: LatestState<HashMap<String, Arc<TileStore<Nv12Image>>>>,
    clean_gate: Arc<CleanDskGate>,
    /// Bumped per applied `arm` (restored by a reject with the machine).
    arm_version: u64,
    /// The tick backstop's DSK reservation for this tick's applied
    /// definition of a new keyer: held until the commit marks the gate
    /// defined, dropped by a reject.
    staged_dsk: Option<DskReservation>,
}

/// Everything a rejected tick must restore (review SW2-03): the machine and
/// the AUTO bookkeeping that lives beside it.
#[derive(Debug)]
struct Staged {
    machine: SwitcherState,
    active_auto: Option<String>,
    active_auto_scenes: Option<Arc<ScenePair>>,
    /// The input table before this tick's first patch, captured lazily (only
    /// on a patch tick, so the steady state never clones the table).
    inputs: Option<(MixerInputs, u64)>,
    /// The arm version before this tick.
    arm_version: u64,
}

impl SwitcherTickDriver {
    /// Install the admitted downstream keyers before the clock starts (like
    /// [`Self::set_inputs`]). Build them with [`crate::keyer::admit`].
    pub fn set_keyers(&mut self, mut keyers: KeyerRuntime) {
        keyers.set_counters(Arc::clone(&self.keyer_counters));
        *self.keyers = keyers;
        // K3: the machine holds one key per USK slot; the keyer runtime
        // owns their on-air state and the machine mirrors it.
        let _ = self.machine.set_key_count(crate::keyer::MAX_USK);
        self.keyers.set_next(NextTake::of(self.machine.armed()));
        // The attach-time definitions (K4 readback); later only on a
        // committed `keyer_set` tick.
        self.keyer_defs.publish(self.keyers.definitions());
        self.definition_builds += 1;
        self.published_def_version = self.keyers.def_version();
        if self.keyers.any_dsk() {
            self.clean_gate.set_defined();
        }
    }

    /// Odometer: committed-definition readbacks built on the clock (one at
    /// attach; a committed `keyer_set` publishes the readback the control
    /// plane prepared off the clock, so it never moves).
    #[must_use]
    pub fn definition_builds(&self) -> u64 {
        self.definition_builds
    }

    /// What the next background take moves (the machine's arm set).
    #[must_use]
    pub fn next_take(&self) -> NextTake {
        NextTake::of(self.machine.armed())
    }

    /// The keyer runtime (read-only: flags, last-good pair, tally).
    #[must_use]
    pub fn keyers(&self) -> &KeyerRuntime {
        &self.keyers
    }

    /// Resolve this tick's downstream keyer draws into `out` (ascending z),
    /// applying the fresh gate, the loss policy and the last-good pair, and
    /// recording the tally. `auto_p` is the background AUTO's tie envelope
    /// while one is in flight (MIX: `p` going on, `1 − p` going off; DIP: `i`
    /// going on, `1 − d` going off). `incoming_drawn` says whether the
    /// transition's incoming tile contributes to this frame (the PVW role's
    /// PROGRAM tally). Called by the frame hook after the background is
    /// staged. Wait-free; allocation-free.
    pub fn resolve_keyers(
        &mut self,
        tick: Tick,
        auto_p: Option<TieEnvelope>,
        incoming_drawn: bool,
        out: &mut [Option<KeyerDraw>; MAX_KEYERS],
    ) {
        self.keyers
            .resolve(tick, auto_p, incoming_drawn, out, &self.outcomes);
    }

    /// After [`Self::reject_composed_tick`]: rebuild the installed keyer
    /// draws as the committed resting state (an on-air keyer draws its
    /// restored last-good pair at weight 1, even where the committed tick
    /// drew it at weight 0; review round 2 G1).
    pub fn settle_keyer_draws(&self, draws: &mut [Option<KeyerDraw>; MAX_KEYERS]) {
        self.keyers.settle_draws(draws);
    }

    /// Install the numbered mixer-input table before the clock starts (like
    /// [`Self::set_prime_stores`]). The machine's PGM/PVW must then be input
    /// references (`in:<n>`). Publishes the initial readback.
    pub fn set_inputs(&mut self, inputs: MixerInputs) {
        self.inputs_readback
            .publish(inputs.readback(self.last_tick, self.inputs_version));
        self.published_inputs_version = self.inputs_version;
        self.inputs = Some(inputs);
    }

    /// Resolve a machine ref to its source through the input table (identity
    /// without a table). Allocation-free.
    fn resolve<'a>(&'a self, reference: &'a SourceRef) -> &'a SourceRef {
        match &self.inputs {
            Some(table) => table.resolve(reference),
            None => reference,
        }
    }

    /// The SOURCES on PGM and PVW for this tick: the machine's input refs
    /// resolved through the table (identity without a table). Everything that
    /// touches a store or a cell uses these, never [`Self::selections`].
    #[must_use]
    pub fn resolved_selections(&self) -> (&SourceRef, &SourceRef) {
        (
            self.resolve(self.machine.program()),
            self.resolve(self.machine.preview()),
        )
    }

    /// With inputs installed, a select must name a declared, patched input;
    /// a raw source is refused (the control plane translates).
    fn select_error(&self, verb: &SwitcherVerb) -> Option<&'static str> {
        let table = self.inputs.as_ref()?;
        let (SwitcherVerb::SelectPreview(target) | SwitcherVerb::SelectProgram(target)) = verb
        else {
            return None;
        };
        match target.as_input() {
            None => Some(REFUSE_NOT_AN_INPUT),
            Some(number) if table.source(number).is_none() => Some(REFUSE_UNKNOWN_INPUT),
            Some(_) => None,
        }
    }

    /// Validate a patch against the live table and buses. On success returns
    /// the new PGM source when the patch changes it (it must be fresh first).
    fn validate_patch<'a>(&self, set: &'a PatchSet) -> Result<Option<&'a SourceRef>, String> {
        let Some(table) = self.inputs.as_ref() else {
            return Err("unknown-slot: this M/E declares no numbered inputs".into());
        };
        if set.members.is_empty() || set.members.len() > MAX_PATCH_MEMBERS {
            return Err(format!(
                "a patch carries 1..={MAX_PATCH_MEMBERS} crosspoints"
            ));
        }
        let program_input = self.machine.program().as_input();
        let preview_input = self.machine.preview().as_input();
        // Fixed-size duplicate guard: no allocation on the clock.
        let mut seen = [false; INPUT_SLOTS];
        let mut new_program = None;
        for (number, source) in &set.members {
            if !table.is_declared(*number) {
                return Err(format!(
                    "unknown-slot: mixer input {number} is not declared"
                ));
            }
            // `is_declared` bounded the number to 1..=64 above.
            let Some(slot) = seen.get_mut(usize::from(*number)) else {
                return Err(format!(
                    "unknown-slot: mixer input {number} is not declared"
                ));
            };
            if *slot {
                return Err(format!("mixer input {number} is patched twice in one set"));
            }
            *slot = true;
            let on_program = program_input == Some(*number);
            let on_bus = on_program || preview_input == Some(*number);
            match source {
                Some(source) => {
                    let known = self.store_for(source.as_str()).is_some();
                    if !known {
                        return Err(format!(
                            "unknown-source: {:?} has no frame store in this run",
                            source.as_str()
                        ));
                    }
                }
                None if on_bus => {
                    return Err(format!(
                        "mixer input {number} is on Program or Preview and cannot be unpatched"
                    ));
                }
                None => {}
            }
            if on_bus && self.machine.is_in_flight() {
                return Err(REFUSE_PATCH_DURING_AUTO.into());
            }
            if on_program {
                if !set.allow_on_air {
                    return Err(REFUSE_ON_AIR_PROTECTED.into());
                }
                if source.as_ref() != table.source(*number) {
                    new_program = source.as_ref();
                }
            }
        }
        Ok(new_program)
    }

    /// Apply a validated patch: every member lands in this drain, the table
    /// is captured first so a rejected frame restores it. Cloning a
    /// `SourceRef` (a `String`) happens only here, on a patch tick, never in
    /// the steady state.
    fn apply_patch(&mut self, set: &PatchSet) {
        self.stage_inputs();
        if let Some(table) = self.inputs.as_mut() {
            for (number, source) in &set.members {
                table.set(*number, source.clone());
            }
            self.inputs_version = self.inputs_version.saturating_add(1);
        }
    }

    /// Validate, then apply now, hold for the new PGM source's first fresh
    /// frame (the single held-take slot), or refuse.
    fn gate_patch(
        &mut self,
        command: &SwitcherCommand,
        set: &PatchSet,
        tick: Tick,
        held: bool,
    ) -> Result<bool, String> {
        let fresh = match self.validate_patch(set)? {
            None => Some(true),
            Some(source) => self.fresh_at(source.as_str(), tick),
        };
        match fresh {
            Some(true) => {
                self.apply_patch(set);
                Ok(true)
            }
            Some(false) => {
                if !held {
                    self.pending_take = Some(HeldTake {
                        command: command.clone(),
                        deadline: tick.index.saturating_add(TAKE_PRIME_DEADLINE_TICKS),
                        bound: self.drain_bound,
                    });
                }
                Ok(false)
            }
            None => Err("switcher source prime provider is unavailable".into()),
        }
    }

    /// Install the shared, already allocated source stores before the clock
    /// starts. Only `is_primed` is sampled on a tick; ingest remains off-clock.
    pub fn set_prime_stores(&mut self, stores: HashMap<String, Arc<TileStore<Nv12Image>>>) {
        // Shared with the control plane once, at attach (K4 `keyer_set`).
        self.stores.publish(stores.clone());
        self.primed_stores = Some(stores);
    }

    /// Resync the drive's current store set (issue #464). Called by the frame
    /// hook on the clock only when the drive's store generation changed, i.e.
    /// on an operator-paced live add/remove; the steady state never calls it.
    /// Bounded by the drive's store count (the drain bounds that).
    pub fn sync_live_stores(&mut self, stores: &HashMap<String, Arc<TileStore<Nv12Image>>>) {
        self.live_stores.clone_from(stores);
        // K4 `keyer_set` resolves fills and keys off the clock from the
        // shared cell: republish the same view `store_for` uses (the drive's
        // current set first, then the boot map), so a live-added source can
        // be keyed and a re-added id resolves its new store. Operator-paced
        // like the resync itself; the publish never waits on a reader.
        if let Some(boot) = self.primed_stores.as_ref() {
            let mut view = boot.clone();
            view.extend(
                self.live_stores
                    .iter()
                    .map(|(id, store)| (id.clone(), Arc::clone(store))),
            );
            self.stores.publish(view);
        }
    }

    /// The drive's current store for `source`, if any (the PVW monitor reads
    /// this first).
    #[must_use]
    pub fn live_store(&self, source: &str) -> Option<&Arc<TileStore<Nv12Image>>> {
        self.live_stores.get(source)
    }

    /// The store for `source`: the drive's current set first, then the boot
    /// map. `None` when no prime map is installed.
    fn store_for(&self, source: &str) -> Option<&Arc<TileStore<Nv12Image>>> {
        let stores = self.primed_stores.as_ref()?;
        self.live_stores.get(source).or_else(|| stores.get(source))
    }

    /// Advertise AUTO. Called only by the frame hook that renders the
    /// dissolve inside the Program drive (ADR-0104 §3).
    pub fn advertise_auto(&self) {
        self.auto_capable
            .store(true, std::sync::atomic::Ordering::Release);
    }

    /// Whether `source` has a fresh frame at this tick, by the compositor's own
    /// latch-and-classify rule (ADR-0104 §2). `None` when no store map is
    /// installed. Wait-free: atomic loads and a ring snapshot, no allocation.
    fn fresh_at(&self, source: &str, tick: Tick) -> Option<bool> {
        self.primed_stores.as_ref()?;
        Some(
            self.store_for(source)
                .is_some_and(|store| store.state_at(tick.pts).is_live()),
        )
    }

    /// Snapshot the state a rejected tick restores, once per tick.
    fn stage(&mut self) {
        if self.before_staged.is_none() {
            self.before_staged = Some(Staged {
                machine: self.machine.clone(),
                active_auto: self.active_auto.clone(),
                active_auto_scenes: self.active_auto_scenes.clone(),
                inputs: None,
                arm_version: self.arm_version,
            });
        }
    }

    /// Capture the input table for restore before this tick's first patch.
    fn stage_inputs(&mut self) {
        self.stage();
        if let (Some(staged), Some(table)) = (self.before_staged.as_mut(), self.inputs.as_ref()) {
            if staged.inputs.is_none() {
                staged.inputs = Some((table.clone(), self.inputs_version));
            }
        }
    }

    /// The running AUTO got its terminal outcome on this tick: a rejected
    /// frame restores the buses but must not resurrect or refuse it again.
    fn forget_staged_auto(&mut self) {
        if let Some(staged) = self.before_staged.as_mut() {
            staged.active_auto = None;
            staged.active_auto_scenes = None;
            staged.machine.abort();
        }
    }

    /// The source a take verb puts on Program (resolved through the table).
    fn incoming<'a>(&'a self, verb: &'a SwitcherVerb) -> &'a str {
        match verb {
            SwitcherVerb::SelectProgram(source) => self.resolve(source).as_str(),
            _ => self.resolve(self.machine.preview()).as_str(),
        }
    }

    /// Apply a take whose incoming source has a fresh frame now.
    fn apply_take(
        &mut self,
        verb: &SwitcherVerb,
        scenes: Option<&Arc<ScenePair>>,
        tick: Tick,
    ) -> Result<SwitcherProjection, String> {
        match verb {
            SwitcherVerb::SelectProgram(source) => Ok(self.machine.punch(source.clone())),
            SwitcherVerb::Cut => self.machine.cut().map_err(|e| e.to_string()),
            SwitcherVerb::Auto(rate) => {
                if self.machine.is_in_flight() {
                    return Err("cannot start a second AUTO during an active take".into());
                }
                let Some(pair) = scenes else {
                    return Err("no active PGM/PVW scene provider".into());
                };
                let descriptor = self.machine.transition_with_rate(*rate);
                if !is_rendered_transition(&descriptor) {
                    return Err(REFUSE_UNSUPPORTED_TRANSITION.into());
                }
                // The flight renders its own descriptor (the selection at the
                // AUTO's rate); the selection itself is unchanged (ADR-0112 §4).
                validate_mix(&pair.program, &pair.preview)
                    .map_err(|e| e.to_string())
                    .and_then(|()| {
                        self.machine
                            .auto_with(descriptor, tick)
                            .map_err(|e| e.to_string())
                    })
            }
            SwitcherVerb::SelectPreview(_)
            | SwitcherVerb::Patch(_)
            | SwitcherVerb::PatchArmed { .. }
            | SwitcherVerb::SetTransition(_)
            | SwitcherVerb::KeyerCut { .. }
            | SwitcherVerb::KeyerAuto { .. }
            | SwitcherVerb::KeyerTie { .. }
            | SwitcherVerb::KeyerSet { .. }
            | SwitcherVerb::Arm(_)
            | SwitcherVerb::Bound(_) => Err("not a take".into()),
        }
    }

    /// Stage a successful take result: AUTO stays active until completion; an
    /// on-air verb interrupts an active AUTO.
    fn stage_take(
        &mut self,
        operation_id: String,
        verb: &SwitcherVerb,
        next: SwitcherProjection,
        scenes: Option<Arc<ScenePair>>,
        tick: Tick,
    ) {
        if matches!(verb, SwitcherVerb::Auto(_)) && !next.completed {
            // Tied DSKs take part in this flight in a direction fixed now
            // (review F1).
            self.keyers.begin_tied_transition();
            self.active_auto_scenes = scenes;
            self.active_auto = Some(operation_id);
            return;
        }
        if matches!(verb, SwitcherVerb::Cut) && next.completed {
            // ADR-0113 §5: tied DSKs flip on the CUT tick. A CUT during an
            // AUTO completes that flight's participants; otherwise every
            // tied keyer takes part from its current state.
            if self.active_auto.is_none() {
                self.keyers.begin_tied_transition();
            }
            self.keyers.complete_tied(tick);
        }
        if matches!(verb, SwitcherVerb::SelectProgram(_)) && self.active_auto.is_some() {
            // A punch ends the flight without a take: tied keyers stay put.
            self.keyers.cancel_tied();
        }
        if matches!(verb, SwitcherVerb::Cut | SwitcherVerb::SelectProgram(_)) {
            if let Some(old) = self.active_auto.take() {
                self.active_auto_scenes = None;
                self.forget_staged_auto();
                self.publish_outcome(
                    old,
                    tick.index,
                    false,
                    Some("AUTO interrupted by a new on-air operation".into()),
                );
            }
        }
        self.staged_success.push(operation_id);
    }

    /// Immutable scene pair pinned for the duration of the active AUTO, if any.
    /// The compositor must render this pair rather than a newer authored pair
    /// until the take completes or is interrupted.
    #[must_use]
    pub fn scene_pair(&self) -> Option<Arc<ScenePair>> {
        self.active_auto_scenes
            .clone()
            .or_else(|| self.scenes.latest())
    }

    /// Set capability only after paired real program/preview canvases were
    /// published for this tick; never at config load or before first frame.
    pub fn publish_provider_status(&self, status: ProviderStatus) {
        self.readiness.publish(status);
    }

    /// Invalidate on detach/render failure; no stale last-good capability can
    /// authorize a fresh command after the real provider stops.
    pub fn revoke_provider_status(&self) {
        self.readiness.publish(ProviderStatus {
            scene_version: 0,
            last_tick: 0,
            width: 0,
            height: 0,
            fps_num: 0,
            fps_den: 0,
        });
    }

    /// On a repeat tick the protected engine re-emits the previous canvas;
    /// never drain or apply commands on that stale picture. A held Cut may
    /// expire off the clock as a refusal; successful takes wait for a fresh tick.
    pub fn repeat_tick(&mut self, tick: Tick) {
        self.last_tick = tick.index;
        self.keyers.expire_on_repeat(tick.index, &self.outcomes);
        // A repeated tick never composes the armed patch: a proposal at or
        // before it is missed and goes back to the router.
        if let Some(armed) = self.armed.as_ref() {
            if armed.arm.state() == ArmState::Proposed && armed.arm.tick() <= tick.index {
                let _ = armed.arm.unpropose();
            }
        }
        // Sustained repeats (no fresh drain) still bound the armed set: its
        // deadlines expire here too (review C-2). `Applying`/`Committed`
        // belong to the composed-tick path and are never touched here.
        self.expire_armed(tick);
        if self
            .pending_take
            .as_ref()
            .is_some_and(|held| tick.index >= held.deadline)
        {
            if let Some(HeldTake { command, .. }) = self.pending_take.take() {
                self.publish_outcome(
                    command.operation_id,
                    tick.index,
                    false,
                    Some("take deadline expired while program repeated last-good".into()),
                );
            }
        }
    }

    /// The active AUTO's anchor tick and duration (ADR-0104 §3/§4), if any.
    #[must_use]
    pub fn auto_window(&self) -> Option<(i128, FrameCount)> {
        self.machine.auto_window()
    }

    /// The descriptor the Program drive renders: the in-flight AUTO's own
    /// descriptor during a take, else the selection.
    #[must_use]
    pub fn transition(&self) -> &TransitionDescriptor {
        self.machine.active_transition()
    }

    /// The operator's selected transition (ADR-0112 §4), unchanged by AUTO.
    #[must_use]
    pub fn selected_transition(&self) -> &TransitionDescriptor {
        self.machine.transition()
    }

    /// Authoritative post-command machine selections for the same output
    /// tick: input refs (`in:<n>`) when numbered inputs are installed, else
    /// sources. Use [`Self::resolved_selections`] to touch a store or cell.
    #[must_use]
    pub fn selections(&self) -> (&SourceRef, &SourceRef) {
        (self.machine.program(), self.machine.preview())
    }

    /// Confirm or reject a pending selection only AFTER a freshly composited
    /// program frame has actually been published. During repeat/catch-up the
    /// caller does not invoke this method; no stale canvas gets a false receipt.
    pub fn confirm_fresh_frame(&mut self, tick: Tick, composed: bool) {
        if composed {
            self.commit_composed_tick(tick);
        } else {
            self.reject_composed_tick(tick, "program frame did not composite");
        }
    }

    /// Process at most 16 queued commands, then advance the exact-tick machine;
    /// always produces one snapshot even if a client wedges or a command fails.
    /// The caller composes the resulting PGM/PVW scene on this same `tick`.
    #[allow(clippy::too_many_lines)]
    // reason: one flat, linear per-tick drain: the pending-take prime deadline,
    // then the bounded command match (SelectPreview/SelectProgram/Cut/Auto) and
    // its staged/held/failed arms. Splitting it would move the all-or-nothing
    // per-tick staging state (`before_staged`/`staged_success`) behind one-use
    // helpers and hide the single-tick atomicity this test set pins.
    pub fn drain_at_tick(&mut self, tick: Tick) -> SwitcherProjection {
        self.last_tick = tick.index;
        // Keyers: stage the Copy flags a rejected frame restores, then finish
        // ramps and resolve held keyer commands (ADR-0113 §6).
        self.keyers.begin_tick();
        // Review round 2 (MEDIA-2): a held USK cut never lands mid-transition.
        self.keyers.set_in_transition(self.machine.is_in_flight());
        self.keyers.tick_pending(
            tick,
            &mut OpSink {
                success: &mut self.staged_success,
                outcomes: &self.outcomes,
                tick: tick.index,
            },
        );
        // An AUTO in flight can complete (swap buses) on this tick: snapshot
        // first so a rejected frame restores the whole flight (SW2-03).
        if self.machine.is_in_flight() {
            self.stage();
        }
        // SW2-02: never complete an AUTO onto a source without a fresh frame.
        if let Some((start, rate)) = self.machine.auto_window() {
            let terminal = i128::from(tick.index) - start >= i128::from(rate.get());
            let (_, preview) = self.resolved_selections();
            // A key-only transition (K3) puts no incoming picture on air.
            let background = self.machine.armed().background;
            if terminal && background && self.fresh_at(preview.as_str(), tick) == Some(false) {
                self.machine.abort();
                // Aborted: tied keyers keep their state.
                self.keyers.cancel_tied();
                if let Some(operation_id) = self.active_auto.take() {
                    self.active_auto_scenes = None;
                    // Already terminal: a rejected frame must not refuse it twice.
                    self.forget_staged_auto();
                    self.publish_outcome(
                        operation_id,
                        tick.index,
                        false,
                        Some(REFUSE_LOST_DURING_AUTO.into()),
                    );
                }
            }
        }
        let mut projection = self.machine.sample(tick);
        if projection.completed {
            // The AUTO terminal tick: tied participants land (their envelope
            // is at its end value). An aborted AUTO cancelled them above.
            self.keyers.complete_tied(tick);
        }
        if let Some(HeldTake {
            command,
            deadline,
            bound,
        }) = self.pending_take.take()
        {
            if let Some(reason) = self.bound_refusal(bound) {
                // Review round 2 (K3-BOUND-1): the arm set or a keyer
                // definition moved while the take was held (a `keyer_set`
                // re-pointing an armed, off-air USK is legal meanwhile): the
                // take is refused, never applied to keys it was not
                // authorized for.
                self.publish_outcome(command.operation_id, tick.index, false, Some(reason.into()));
            } else if let SwitcherVerb::Patch(set) = &command.verb {
                // A held patch re-validates against the live table: nothing
                // else could change it while it held the only slot.
                match self.gate_patch(&command, set, tick, true) {
                    Ok(true) => self.staged_success.push(command.operation_id),
                    Ok(false) if tick.index >= deadline => self.publish_outcome(
                        command.operation_id,
                        tick.index,
                        false,
                        Some(REFUSE_NO_FRESH_FRAME.into()),
                    ),
                    Ok(false) => {
                        self.pending_take = Some(HeldTake {
                            command,
                            deadline,
                            bound,
                        });
                    }
                    Err(err) => {
                        self.publish_outcome(command.operation_id, tick.index, false, Some(err));
                    }
                }
            } else if self.take_fresh(&command.verb, tick) == Some(true) {
                self.stage();
                let scenes = self.scenes.latest();
                match self.apply_take(&command.verb, scenes.as_ref(), tick) {
                    Ok(next) => {
                        projection = next;
                        self.stage_take(command.operation_id, &command.verb, next, scenes, tick);
                    }
                    Err(err) => {
                        self.publish_outcome(command.operation_id, tick.index, false, Some(err));
                    }
                }
            } else if tick.index >= deadline {
                self.publish_outcome(
                    command.operation_id,
                    tick.index,
                    false,
                    Some(REFUSE_NO_FRESH_FRAME.into()),
                );
            } else {
                self.pending_take = Some(HeldTake {
                    command,
                    deadline,
                    bound,
                });
            }
        }
        if projection.completed {
            if let Some(operation_id) = self.active_auto.take() {
                self.active_auto_scenes = None;
                self.staged_success.push(operation_id);
            }
        }
        for _ in 0..MAX_COMMANDS_PER_TICK {
            let Ok(command) = self.rx.try_recv() else {
                break;
            };
            // Review AUTHZ-1/-2: a bound verb is applied only against the
            // arm set and keyer definitions it was authorized for.
            self.drain_bound = Bound::default();
            let command = match command.verb {
                SwitcherVerb::Bound(bound) => {
                    let BoundVerb {
                        verb,
                        arm_version,
                        defs_version,
                    } = *bound;
                    let versions = Bound {
                        arm_version,
                        defs_version,
                    };
                    let refusal = if let Some(reason) = self.bound_refusal(versions) {
                        Some(reason)
                    } else if matches!(verb, SwitcherVerb::Bound(_)) {
                        Some("invalid: a bound verb cannot be nested")
                    } else {
                        None
                    };
                    if let Some(reason) = refusal {
                        self.publish_outcome(
                            command.operation_id,
                            tick.index,
                            false,
                            Some(reason.into()),
                        );
                        continue;
                    }
                    // Kept by a take or keyer cut this drain holds.
                    self.drain_bound = versions;
                    SwitcherCommand {
                        operation_id: command.operation_id,
                        scene_version: command.scene_version,
                        verb,
                    }
                }
                _ => command,
            };
            // Keyer verbs first: they reference no scene geometry, and a held
            // background take never blocks a DSK (ADR-0113 §6).
            if let SwitcherVerb::KeyerSet {
                keyer,
                def,
                readback,
                allow_on_air,
                deadline,
            } = command.verb
            {
                if tick.index >= deadline {
                    // Expired in the mailbox: never applied (review H4).
                    self.publish_outcome(
                        command.operation_id,
                        tick.index,
                        false,
                        Some(REFUSE_KEYER_SET_DEADLINE.into()),
                    );
                    continue;
                }
                // Backstop (review H3): a new keyer is defined only under a
                // DSK reservation taken here, at the tick, so it cannot land
                // while an output carries Clean or has a take to it pending,
                // whatever the caller did. At most `MAX_TICK_ATTEMPTS` CAS
                // attempts, then refused as contention (review J2: never
                // spins on the clock); no allocation.
                // Clean is pre-DSK Program: only a new DSK excludes it (a
                // USK belongs to the M/E scene, K3).
                let new_keyer = self.keyers.def(usize::from(keyer)).is_none()
                    && def.0.family == crate::keyer::KeyerFamily::Dsk;
                let reservation = if new_keyer {
                    match self.clean_gate.tick_reserve_dsk() {
                        Ok(reservation) => Some(reservation),
                        Err(refusal) => {
                            let reason = match refusal {
                                TickRefusal::Excluded => REFUSE_KEYER_CLEAN,
                                TickRefusal::Contended => REFUSE_KEYER_GATE_CONTENDED,
                            };
                            self.publish_outcome(
                                command.operation_id,
                                tick.index,
                                false,
                                Some(reason.into()),
                            );
                            continue;
                        }
                    }
                } else {
                    None
                };
                let before = self.keyers.def_version();
                self.keyers.define(
                    keyer,
                    def.0,
                    &readback.0,
                    allow_on_air,
                    command.operation_id,
                    tick,
                    &mut OpSink {
                        success: &mut self.staged_success,
                        outcomes: &self.outcomes,
                        tick: tick.index,
                    },
                );
                if self.keyers.def_version() != before {
                    // Applied: its prepared readback is what the commit
                    // publishes (a move; a later set this tick replaces it).
                    self.staged_readback = Some(readback.0);
                    // One held reservation excludes Clean until the commit
                    // marks the gate defined; a second new keyer this tick
                    // keeps the first one (never a gap between them).
                    if self.staged_dsk.is_none() {
                        self.staged_dsk = reservation;
                    }
                }
                continue;
            }
            if let SwitcherVerb::Arm(armed) = command.verb {
                // No scene geometry: the arm set names elements only.
                let result = if self.pending_take.is_some() {
                    // Review K3-INV-1: a held take keeps the set it was
                    // admitted with; a re-arm would retarget it.
                    Err(REFUSE_ARM_TAKE_HELD.to_owned())
                } else if armed.keys.iter().any(|&k| !self.keyers.usk_defined(k)) {
                    Err(REFUSE_ARM_UNKNOWN_KEY.to_owned())
                } else {
                    // Allocation-free staging (review K3-R3-2): a full stage
                    // already covers the arm; otherwise keep only the
                    // previous set (an `Arc` clone) and version.
                    if self.before_staged.is_none() && self.before_arm.is_none() {
                        self.before_arm =
                            Some((Arc::clone(self.machine.armed_shared()), self.arm_version));
                    }
                    self.machine.arm_shared(armed).map_err(|e| e.to_string())
                };
                match result {
                    Ok(()) => {
                        self.keyers.set_next(NextTake::of(self.machine.armed()));
                        self.arm_version = self.arm_version.wrapping_add(1);
                        self.staged_success.push(command.operation_id);
                    }
                    Err(error) => {
                        self.publish_outcome(command.operation_id, tick.index, false, Some(error));
                    }
                }
                continue;
            }
            if let Some((keyer, action)) = KeyerAction::of(&command.verb) {
                self.keyers.set_in_transition(self.machine.is_in_flight());
                self.keyers.command(
                    keyer,
                    action,
                    self.drain_bound.defs_version,
                    command.operation_id,
                    tick,
                    &mut OpSink {
                        success: &mut self.staged_success,
                        outcomes: &self.outcomes,
                        tick: tick.index,
                    },
                );
                continue;
            }
            if let SwitcherVerb::PatchArmed { .. } = &command.verb {
                self.admit_armed(command, tick);
                continue;
            }
            let scenes = self.scenes.latest();
            self.stage();
            let take = matches!(
                command.verb,
                SwitcherVerb::SelectProgram(_) | SwitcherVerb::Cut | SwitcherVerb::Auto(_)
            );
            let result: Result<Option<SwitcherProjection>, String> = if scenes
                .as_ref()
                .is_some_and(|pair| pair.version != command.scene_version)
            {
                Err("scene version changed before the frame boundary; re-plan the operation".into())
            } else if scenes.is_none() {
                Err("no active PGM/PVW scene provider".into())
            } else if self.pending_take.is_some() {
                // One held take at a time; a PVW change would retarget it.
                Err("a take is already held waiting for source prime".into())
            } else if let SwitcherVerb::Patch(set) = &command.verb {
                match self.gate_patch(&command, set, tick, false) {
                    // Applied: the projection is unchanged by a patch.
                    Ok(true) => Ok(Some(projection)),
                    Ok(false) => Ok(None),
                    Err(err) => Err(err),
                }
            } else if let SwitcherVerb::SetTransition(descriptor) = &command.verb {
                if is_rendered_transition(descriptor) {
                    // Cloning the descriptor allocates only for a dip/wipe
                    // carrying a source id, and only on this command's tick.
                    self.machine
                        .set_transition(descriptor.clone())
                        .map(|()| Some(projection))
                        .map_err(|e| e.to_string())
                } else {
                    Err(REFUSE_UNSUPPORTED_TRANSITION.into())
                }
            } else if let Some(error) = self.select_error(&command.verb) {
                Err(error.into())
            } else if let SwitcherVerb::SelectPreview(source) = &command.verb {
                if !scenes.as_ref().is_some_and(|pair| {
                    scene_admits_source(&pair.preview, self.resolve(source).as_str())
                }) {
                    Err("preview source is absent from the active scene".into())
                } else if self.resolve(source) == self.resolved_selections().0 {
                    // #247 compares the pictures: the resolved SOURCES.
                    Err(REFUSE_SAME_SOURCE.into())
                } else {
                    self.machine
                        .set_preview(source.clone())
                        .map(|()| Some(self.machine.sample(tick)))
                        .map_err(|e| e.to_string())
                }
            } else if matches!(command.verb, SwitcherVerb::Auto(_)) && self.machine.is_in_flight() {
                Err("cannot start a second AUTO during an active take".into())
            } else if let SwitcherVerb::SelectProgram(source) = &command.verb {
                if source == self.machine.program()
                    || self.resolve(source) == self.resolved_selections().0
                {
                    // The picture, not the reference: another input carrying
                    // the Program source is the same source on Program.
                    Err("source is already on program".into())
                } else if self.inputs.is_some() && source == self.machine.preview() {
                    // #341 review A-1: punching the Preview input would put one
                    // input on both buses (CUT/AUTO then refuse, flip-flop
                    // cannot swap). Select another input, or CUT.
                    Err(REFUSE_SAME_SOURCE.into())
                } else if scenes.as_ref().is_some_and(|pair| {
                    scene_admits_source(&pair.program, self.resolve(source).as_str())
                }) {
                    self.gate_take(&command, scenes.as_ref(), tick)
                } else {
                    Err("program source is absent from the active scene".into())
                }
            } else if self.machine.armed().background
                && self.resolved_selections().0 == self.resolved_selections().1
            {
                Err(REFUSE_SAME_SOURCE.into())
            } else {
                self.gate_take(&command, scenes.as_ref(), tick)
            };
            match result {
                // Held: never claim success before the incoming source is fresh.
                Ok(None) => {}
                Ok(Some(next)) => {
                    projection = next;
                    if take {
                        self.stage_take(command.operation_id, &command.verb, next, scenes, tick);
                    } else {
                        self.staged_success.push(command.operation_id);
                    }
                }
                Err(error) => {
                    self.publish_outcome(command.operation_id, tick.index, false, Some(error));
                }
            }
        }
        // After the commands: a Cut on this tick is seen by the armed set's
        // re-validation at its adoption tick.
        self.drive_armed(tick);
        projection
    }

    /// Admit the mixer half of a set into the armed slot (validated now; a
    /// failure refuses the whole set through its arm).
    fn admit_armed(&mut self, command: SwitcherCommand, tick: Tick) {
        let SwitcherVerb::PatchArmed { set, arm } = command.verb else {
            return;
        };
        let scenes = self.scenes.latest();
        let error = if self.armed.is_some() {
            Some((SetRefusal::Busy, REFUSE_SET_ARMED.to_owned()))
        } else if scenes.as_ref().map(|pair| pair.version) != Some(command.scene_version) {
            Some((
                SetRefusal::Invalid,
                "scene version changed before the frame boundary; re-plan the operation".into(),
            ))
        } else {
            self.validate_patch(&set)
                .err()
                .map(|e| (SetRefusal::from_switcher_error(&e), e))
        };
        if let Some((reason, error)) = error {
            let _ = arm.refuse(reason);
            self.publish_outcome(command.operation_id, tick.index, false, Some(error));
            return;
        }
        self.armed = Some(ArmedSet {
            operation_id: command.operation_id,
            set,
            arm,
            deadline: tick.index.saturating_add(SET_ADOPT_DEADLINE_TICKS),
            applied_at: None,
        });
    }

    /// Refuse the armed set (first terminal write wins) and free the slot.
    fn refuse_armed(&mut self, tick: Tick, reason: SetRefusal, error: String) {
        if let Some(armed) = self.armed.take() {
            let _ = armed.arm.refuse(reason);
            let error = match armed.arm.reason() {
                Some(won) if won != reason => format!("refused:{}", won.code()),
                _ => error,
            };
            self.publish_outcome(armed.operation_id, tick.index, false, Some(error));
        }
    }

    /// Deadline expiry of the armed slot without a fresh drain (a repeat
    /// tick): O(1), never validates or applies. A router refusal frees the
    /// slot; a `Pending` set expires `feed-not-fresh` at its deadline, a
    /// `SwitcherReady` one `deadline` at the grace backstop. `Applying` and
    /// `Committed` are owned by the commit/reject path and left alone.
    fn expire_armed(&mut self, tick: Tick) {
        let Some(armed) = self.armed.as_ref() else {
            return;
        };
        if armed.applied_at.is_some() {
            return;
        }
        let deadline = armed.deadline;
        match armed.arm.state() {
            ArmState::Refused => {
                let code = armed.arm.reason().map_or("invalid", SetRefusal::code);
                self.refuse_armed(tick, SetRefusal::Invalid, format!("refused:{code}"));
            }
            ArmState::Pending if tick.index >= deadline => {
                self.refuse_armed(tick, SetRefusal::FeedNotFresh, REFUSE_NO_FRESH_FRAME.into());
            }
            ArmState::SwitcherReady
                if tick.index >= deadline.saturating_add(SET_SWITCHER_GRACE_TICKS) =>
            {
                self.refuse_armed(
                    tick,
                    SetRefusal::Deadline,
                    "refused:deadline: the set was not adopted in time".into(),
                );
            }
            _ => {}
        }
    }

    /// One fresh drain of the armed slot: O(1) except on the adoption tick
    /// (one validation, O(members)). Never waits.
    #[allow(clippy::too_many_lines)]
    // reason: one flat match over the handshake states; each arm is the
    // switcher's single move for that state.
    fn drive_armed(&mut self, tick: Tick) {
        let Some(armed) = self.armed.as_mut() else {
            return;
        };
        // A patch applied on a drain that was neither committed nor rejected
        // stayed in the table: it was composed, so it is committed there.
        if let Some(at) = armed.applied_at.take() {
            if armed.arm.commit(at) {
                if let Some(armed) = self.armed.take() {
                    self.publish_outcome(armed.operation_id, at, true, None);
                }
                return;
            }
        }
        let arm = Arc::clone(&armed.arm);
        let deadline = armed.deadline;
        match arm.state() {
            // Applying is resolved by this tick's commit/reject only.
            ArmState::Committed | ArmState::Applying => {
                if arm.state() == ArmState::Committed {
                    self.armed = None;
                }
            }
            ArmState::Refused => {
                // The router refused the set (superseded, capacity, an
                // output member's cause): nothing was applied here.
                let code = arm.reason().map_or("invalid", SetRefusal::code);
                self.refuse_armed(tick, SetRefusal::Invalid, format!("refused:{code}"));
            }
            ArmState::Pending => match self.armed_ready(tick) {
                Ok(true) => {
                    let _ = arm.switcher_ready();
                }
                Ok(false) if tick.index >= deadline => {
                    self.refuse_armed(tick, SetRefusal::FeedNotFresh, REFUSE_NO_FRESH_FRAME.into());
                }
                Ok(false) => {}
                Err((reason, error)) => self.refuse_armed(tick, reason, error),
            },
            ArmState::SwitcherReady => {
                if tick.index >= deadline.saturating_add(SET_SWITCHER_GRACE_TICKS) {
                    self.refuse_armed(
                        tick,
                        SetRefusal::Deadline,
                        "refused:deadline: the set was not adopted in time".into(),
                    );
                }
            }
            ArmState::Proposed => {
                let at = arm.tick();
                if at > tick.index {
                    return;
                }
                if at < tick.index {
                    // Missed (a skipped or late drain): back to the router.
                    let _ = arm.unpropose();
                    if tick.index >= deadline.saturating_add(SET_SWITCHER_GRACE_TICKS) {
                        self.refuse_armed(
                            tick,
                            SetRefusal::Deadline,
                            "refused:deadline: the set was not adopted in time".into(),
                        );
                    }
                    return;
                }
                // The adoption tick: re-validate (a Cut may have moved PGM).
                match self.armed_ready(tick) {
                    // Claimed before the table changes: a refusal racing in
                    // from the router either wins first (nothing applied) or
                    // loses (the patch lands and commits at T).
                    Ok(true) if arm.begin_apply(at) => {
                        if let Some(set) = self.armed.as_ref().map(|a| a.set.clone()) {
                            self.apply_patch(&set);
                        }
                        if let Some(armed) = self.armed.as_mut() {
                            armed.applied_at = Some(tick.index);
                        }
                    }
                    // The router refused it meanwhile: settled next drain.
                    Ok(true) => {}
                    // The new PGM source lost its fresh frame: re-propose.
                    Ok(false) => {
                        let _ = arm.unpropose();
                    }
                    Err((reason, error)) => self.refuse_armed(tick, reason, error),
                }
            }
        }
    }

    /// Whether the armed set validates now and its new Program source (if
    /// any) has a fresh frame; a validation failure is its typed refusal.
    fn armed_ready(&self, tick: Tick) -> Result<bool, (SetRefusal, String)> {
        let Some(armed) = self.armed.as_ref() else {
            return Ok(false);
        };
        match self.validate_patch(&armed.set) {
            Err(error) => Err((SetRefusal::from_switcher_error(&error), error)),
            Ok(None) => Ok(true),
            Ok(Some(source)) => match self.fresh_at(source.as_str(), tick) {
                Some(fresh) => Ok(fresh),
                None => Err((
                    SetRefusal::Invalid,
                    "switcher source prime provider is unavailable".into(),
                )),
            },
        }
    }

    /// Apply a take now if its incoming source is fresh, else hold it (one slot,
    /// bounded deadline). `Ok(None)` means held.
    fn gate_take(
        &mut self,
        command: &SwitcherCommand,
        scenes: Option<&Arc<ScenePair>>,
        tick: Tick,
    ) -> Result<Option<SwitcherProjection>, String> {
        match self.take_fresh(&command.verb, tick) {
            Some(true) => self.apply_take(&command.verb, scenes, tick).map(Some),
            Some(false) => {
                self.pending_take = Some(HeldTake {
                    command: command.clone(),
                    deadline: tick.index.saturating_add(TAKE_PRIME_DEADLINE_TICKS),
                    bound: self.drain_bound,
                });
                Ok(None)
            }
            None => Err("switcher source prime provider is unavailable".into()),
        }
    }

    /// The typed refusal of a verb bound to versions that moved (review
    /// AUTHZ-1/-2; round 2 K3-BOUND-1 for a held one): two integer compares.
    fn bound_refusal(&self, bound: Bound) -> Option<&'static str> {
        if bound.arm_version.is_some_and(|v| v != self.arm_version) {
            Some(REFUSE_ARM_CHANGED)
        } else if bound
            .defs_version
            .is_some_and(|v| v != self.keyers.def_version())
        {
            Some(crate::keyer::REFUSE_KEYER_DEFINITION_CHANGED)
        } else {
            None
        }
    }

    /// The take's fresh gate: the incoming source, and for a CUT or AUTO
    /// also the fill and key of every tied keyer it would put on air
    /// (ADR-0113 §6). `None` when no store map is installed.
    fn take_fresh(&self, verb: &SwitcherVerb, tick: Tick) -> Option<bool> {
        let incoming = self.fresh_at(self.incoming(verb), tick)?;
        // A key-only CUT or AUTO (K3) puts no background on air: only its
        // keys going on are gated.
        let incoming = incoming
            || (matches!(verb, SwitcherVerb::Cut | SwitcherVerb::Auto(_))
                && !self.machine.armed().background);
        // A CUT during the background AUTO completes that flight: it puts on
        // only the flight's surviving going-on participants (review round 2
        // G2), the same set `stage_take` lands.
        let in_flight = matches!(verb, SwitcherVerb::Cut) && self.active_auto.is_some();
        let tied = !matches!(verb, SwitcherVerb::Cut | SwitcherVerb::Auto(_))
            || self.keyers.tied_going_on_fresh(tick, in_flight);
        Some(incoming && tied)
    }

    /// End the public snapshot stream at this tick if the output loop is about
    /// to stop; state remains the last actually published frame.
    #[must_use]
    pub fn last_observed_tick(&self) -> u64 {
        self.last_tick
    }

    /// Confirm the staged scene was actually composited and published at this
    /// tick before exposing state or an applied receipt. Repeat frames never
    /// call this; failure calls `reject_composed_tick` instead.
    pub fn commit_composed_tick(&mut self, tick: Tick) {
        self.before_staged = None;
        self.before_arm = None;
        // The armed patch composed on this tick: the set is committed at T
        // (its outcome rides this tick's applied receipts).
        if self
            .armed
            .as_ref()
            .is_some_and(|a| a.applied_at == Some(tick.index) && a.arm.commit(tick.index))
        {
            if let Some(armed) = self.armed.take() {
                self.staged_success.push(armed.operation_id);
            }
        }
        // The machine mirrors the runtime's USK states (a loss or a key cut
        // can differ from its toggle). Fixed array, no allocation.
        for slot in crate::keyer::KeyerFamily::Usk.slots() {
            if let (Some(key), Some(flags)) =
                (crate::keyer::usk_key_index(slot), self.keyers.flags(slot))
            {
                self.machine.set_key_on_air(key, flags.on_air);
            }
        }
        self.applied_tick
            .store(tick.index, std::sync::atomic::Ordering::Release);
        let (program, preview) = self.resolved_selections();
        let (program_input, preview_input) = if self.inputs.is_some() {
            (
                self.machine.program().as_input(),
                self.machine.preview().as_input(),
            )
        } else {
            (None, None)
        };
        // Copy rows: no allocation unless the snapshot actually changed.
        let keyers = self.keyers.snapshot();
        let tally = self.keyers.tally();
        let next = NextTake::of(self.machine.armed());
        let changed = self.snapshots.latest().is_none_or(|previous| {
            previous.program != *program
                || previous.preview != *preview
                || previous.program_input != program_input
                || previous.preview_input != preview_input
                || previous.inputs_version != self.inputs_version
                || previous.in_flight != self.machine.is_in_flight()
                || previous.completion_count != self.machine.completion_count()
                // A comparison, never a clone: the descriptor is cloned only
                // below, on a tick whose published state actually changed.
                || previous.transition != *self.machine.transition()
                || previous.keyers != keyers
                || previous.next != next
                || previous.arm_version != self.arm_version
                || previous.tally != tally
        });
        if changed {
            self.snapshots.publish(SwitcherSnapshot {
                tick: tick.index,
                program: program.clone(),
                preview: preview.clone(),
                program_input,
                preview_input,
                inputs_version: self.inputs_version,
                in_flight: self.machine.is_in_flight(),
                completion_count: self.machine.completion_count(),
                transition: self.machine.transition().clone(),
                keyers,
                next,
                arm_version: self.arm_version,
                tally,
            });
        }
        // Definitions only on a committed `keyer_set` tick, before the
        // tick's keyer events, so an event relay always resolves a keyer
        // index against its definition. The readback was built off the clock
        // with the request (review H6): publishing it moves an `Arc`.
        if self.keyers.def_version() != self.published_def_version {
            if let Some(readback) = self.staged_readback.take() {
                self.keyer_defs.publish_arc(readback);
            } else {
                // Unreachable (every applied set carries its readback);
                // fail-safe: build it here and count the build.
                self.keyer_defs.publish(self.keyers.definitions());
                self.definition_builds += 1;
            }
            self.published_def_version = self.keyers.def_version();
            // Before this tick's outcomes: once a caller sees `applied`, the
            // gate already refuses Clean for good.
            if self.keyers.any_dsk() {
                self.clean_gate.set_defined();
            }
        }
        self.staged_readback = None;
        self.staged_dsk = None;
        self.keyers.commit(&self.keyer_events);
        // Readback only on a committed patch tick (allocates; never steady state).
        if self.inputs_version != self.published_inputs_version {
            if let Some(table) = self.inputs.as_ref() {
                self.inputs_readback
                    .publish(table.readback(tick.index, self.inputs_version));
            }
            self.published_inputs_version = self.inputs_version;
        }
        self.composed_count
            .fetch_add(1, std::sync::atomic::Ordering::Release);
        self.spare_success.clear();
        std::mem::swap(&mut self.staged_success, &mut self.spare_success);
        while let Some(operation_id) = self.spare_success.pop() {
            self.publish_outcome(operation_id, tick.index, true, None);
        }
    }

    /// Never claim on-air success when the bus frame could not be rendered.
    pub fn reject_composed_tick(&mut self, tick: Tick, reason: &str) {
        // The AUTO admitted or running on this tick, if any: it gets exactly
        // one terminal refusal, never a later second outcome (SW2-03).
        let staged_auto = self.active_auto.take();
        // Keyers: restore the tick-start flags, refuse ops held in the slots
        // that moved this tick, drop queued events. Applied keyer ops sit in
        // `staged_success` and are refused below with everything else.
        self.keyers.reject(tick.index, reason, &self.outcomes);
        self.staged_readback = None;
        // The restored definitions have no new keyer: its reservation goes.
        self.staged_dsk = None;
        // The armed patch applied on this tick is restored with the table
        // below and goes back to the router (never committed).
        if let Some(armed) = self.armed.as_mut() {
            if armed.applied_at.take() == Some(tick.index) {
                let _ = armed.arm.abort_apply();
            }
        }
        let before_arm = self.before_arm.take();
        if let Some(previous) = self.before_staged.take() {
            self.machine = previous.machine;
            self.active_auto = previous.active_auto;
            self.active_auto_scenes = previous.active_auto_scenes;
            self.arm_version = previous.arm_version;
            if let Some((table, version)) = previous.inputs {
                self.inputs = Some(table);
                self.inputs_version = version;
            }
        }
        // An `arm` staged without a full stage (review K3-R3-2). A full stage
        // taken later in the same tick captured the machine AFTER that arm,
        // so the arm set and version are restored last, over it.
        if let Some((armed, version)) = before_arm {
            self.machine.restore_armed(armed);
            self.arm_version = version;
        }
        // A rejected frame aborts any AUTO: Program stays on the source the
        // restored machine names, which the hook re-binds.
        if self.machine.is_in_flight() {
            self.machine.abort();
        }
        // ... and with it any tied keyer participation: they keep their
        // committed state.
        self.keyers.cancel_tied();
        // The restored machine's arm set (an `arm` this tick is undone).
        self.keyers.set_next(NextTake::of(self.machine.armed()));
        let restored_auto = self.active_auto.take();
        self.active_auto_scenes = None;
        self.spare_success.clear();
        std::mem::swap(&mut self.staged_success, &mut self.spare_success);
        for operation_id in [staged_auto, restored_auto].into_iter().flatten() {
            if !self.spare_success.contains(&operation_id) {
                self.spare_success.push(operation_id);
            }
        }
        self.spare_success.dedup();
        while let Some(operation_id) = self.spare_success.pop() {
            self.publish_outcome(operation_id, tick.index, false, Some(reason.to_owned()));
        }
        self.revoke_provider_status();
    }

    fn publish_outcome(
        &self,
        operation_id: String,
        tick: u64,
        completed: bool,
        error: Option<String>,
    ) {
        self.outcomes.publish(SwitcherOutcome {
            operation_id,
            tick,
            completed,
            error,
        });
    }
}
