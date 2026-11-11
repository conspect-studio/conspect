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

use crate::clock::Tick;
use crate::isolation::{event_stream, EventStream, EventSubscription, LatestState};
use crate::switcher::{SwitcherProjection, SwitcherState};

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

/// An operator verb, already authorized and reference-validated on admission.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SwitcherVerb {
    /// Arm a new preview source, without changing on-air output.
    SelectPreview(SourceRef),
    /// Punch a source directly onto on-air PGM at the next frame boundary.
    SelectProgram(SourceRef),
    /// Atomically take PVW to PGM at the next frame boundary.
    Cut,
    /// Begin an exact-duration automatic take on the next frame boundary.
    Auto(FrameCount),
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
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SwitcherSnapshot {
    /// Output tick on which this snapshot was published.
    pub tick: u64,
    /// Current PGM source id.
    pub program: SourceRef,
    /// Current PVW source id.
    pub preview: SourceRef,
    /// Whether an automatic transition is in progress.
    pub in_flight: bool,
    /// Number of completed takes, monotonic within one runtime.
    pub completion_count: u64,
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
        let handle = Self {
            tx,
            snapshots,
            outcomes,
            scenes: LatestState::new(),
            readiness: LatestState::new(),
            applied_tick: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            composed_count: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            auto_capable: Arc::new(std::sync::atomic::AtomicBool::new(false)),
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
            pending_take: None,
            last_tick: 0,
            staged_success: Vec::with_capacity(MAX_COMMANDS_PER_TICK),
            spare_success: Vec::with_capacity(MAX_COMMANDS_PER_TICK),
            before_staged: None,
        };
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

    /// Subscribe to correlated, drop-oldest applied outcomes. A lagged reader
    /// must re-fetch state and operation status; it never stalls the output tick.
    #[must_use]
    pub fn outcomes(&self) -> EventSubscription<SwitcherOutcome> {
        self.outcomes.subscribe()
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
    pending_take: Option<(SwitcherCommand, u64)>,
    last_tick: u64,
    staged_success: Vec<String>,
    spare_success: Vec<String>,
    before_staged: Option<Staged>,
}

/// Everything a rejected tick must restore (review SW2-03): the machine and
/// the AUTO bookkeeping that lives beside it.
#[derive(Debug)]
struct Staged {
    machine: SwitcherState,
    active_auto: Option<String>,
    active_auto_scenes: Option<Arc<ScenePair>>,
}

impl SwitcherTickDriver {
    /// Install the shared, already allocated source stores before the clock
    /// starts. Only `is_primed` is sampled on a tick; ingest remains off-clock.
    pub fn set_prime_stores(&mut self, stores: HashMap<String, Arc<TileStore<Nv12Image>>>) {
        self.primed_stores = Some(stores);
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
        let stores = self.primed_stores.as_ref()?;
        Some(
            stores
                .get(source)
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
            });
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

    /// The source a take verb puts on Program.
    fn incoming<'a>(&'a self, verb: &'a SwitcherVerb) -> &'a str {
        match verb {
            SwitcherVerb::SelectProgram(source) => source.as_str(),
            _ => self.machine.preview().as_str(),
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
                validate_mix(&pair.program, &pair.preview)
                    .map_err(|e| e.to_string())
                    .and_then(|()| {
                        self.machine
                            .set_transition(TransitionDescriptor::Mix { rate: *rate })
                            .map_err(|e| e.to_string())
                    })
                    .and_then(|()| self.machine.auto(tick).map_err(|e| e.to_string()))
            }
            SwitcherVerb::SelectPreview(_) => Err("not a take".into()),
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
            self.active_auto_scenes = scenes;
            self.active_auto = Some(operation_id);
            return;
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
        if self
            .pending_take
            .as_ref()
            .is_some_and(|(_, deadline)| tick.index >= *deadline)
        {
            if let Some((command, _)) = self.pending_take.take() {
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

    /// Authoritative post-command selected sources for the same output tick.
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
        // An AUTO in flight can complete (swap buses) on this tick: snapshot
        // first so a rejected frame restores the whole flight (SW2-03).
        if self.machine.is_in_flight() {
            self.stage();
        }
        // SW2-02: never complete an AUTO onto a source without a fresh frame.
        if let Some((start, rate)) = self.machine.auto_window() {
            let terminal = i128::from(tick.index) - start >= i128::from(rate.get());
            let preview = self.machine.preview().as_str().to_owned();
            if terminal && self.fresh_at(&preview, tick) == Some(false) {
                self.machine.abort();
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
        if let Some((command, deadline)) = self.pending_take.take() {
            let incoming = self.incoming(&command.verb).to_owned();
            if self.fresh_at(&incoming, tick) == Some(true) {
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
                self.pending_take = Some((command, deadline));
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
            } else if let SwitcherVerb::SelectPreview(source) = &command.verb {
                if !scenes
                    .as_ref()
                    .is_some_and(|pair| scene_admits_source(&pair.preview, source.as_str()))
                {
                    Err("preview source is absent from the active scene".into())
                } else if source == self.machine.program() {
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
                if source == self.machine.program() {
                    Err("source is already on program".into())
                } else if scenes
                    .as_ref()
                    .is_some_and(|pair| scene_admits_source(&pair.program, source.as_str()))
                {
                    self.gate_take(&command, scenes.as_ref(), tick)
                } else {
                    Err("program source is absent from the active scene".into())
                }
            } else if self.machine.preview() == self.machine.program() {
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
        projection
    }

    /// Apply a take now if its incoming source is fresh, else hold it (one slot,
    /// bounded deadline). `Ok(None)` means held.
    fn gate_take(
        &mut self,
        command: &SwitcherCommand,
        scenes: Option<&Arc<ScenePair>>,
        tick: Tick,
    ) -> Result<Option<SwitcherProjection>, String> {
        let incoming = self.incoming(&command.verb).to_owned();
        match self.fresh_at(&incoming, tick) {
            Some(true) => self.apply_take(&command.verb, scenes, tick).map(Some),
            Some(false) => {
                self.pending_take = Some((
                    command.clone(),
                    tick.index.saturating_add(TAKE_PRIME_DEADLINE_TICKS),
                ));
                Ok(None)
            }
            None => Err("switcher source prime provider is unavailable".into()),
        }
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
        self.applied_tick
            .store(tick.index, std::sync::atomic::Ordering::Release);
        let changed = self.snapshots.latest().is_none_or(|previous| {
            previous.program != *self.machine.program()
                || previous.preview != *self.machine.preview()
                || previous.in_flight != self.machine.is_in_flight()
                || previous.completion_count != self.machine.completion_count()
        });
        if changed {
            self.snapshots.publish(SwitcherSnapshot {
                tick: tick.index,
                program: self.machine.program().clone(),
                preview: self.machine.preview().clone(),
                in_flight: self.machine.is_in_flight(),
                completion_count: self.machine.completion_count(),
            });
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
        if let Some(previous) = self.before_staged.take() {
            self.machine = previous.machine;
            self.active_auto = previous.active_auto;
            self.active_auto_scenes = previous.active_auto_scenes;
        }
        // A rejected frame aborts any AUTO: Program stays on the source the
        // restored machine names, which the hook re-binds.
        if self.machine.is_in_flight() {
            self.machine.abort();
        }
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
