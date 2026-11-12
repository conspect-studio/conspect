//! Output routing on the bake consumer (ADR-0108 slice 1, design §4.2-4.5, §7).
//!
//! [`OutputRouter`] runs once per Program tick on the bake consumer, right after
//! the dispatcher event drain and before the bake. It owns the take state
//! machine, the registry of encoders keyed by `(picture bus, settings)`, the
//! live open (one serialised `routing-open` thread), the linger/close (one
//! `routing-reaper` thread) and the feeding of live-opened encoders from the
//! `routing-aux` picture-bus slots.
//!
//! Isolation (invariant #10): every queue is bounded and drops when full.
//! The consumer never blocks on a live-opened worker (`offer`, never
//! `offer_within`), never joins a worker, never waits for an open. Base
//! (Program) encoders are offered exactly as before by the pipeline.
//! Nothing here runs on the output clock.

// reason: consumed by the pipeline wiring commit that follows (spec part B2).
#![allow(dead_code)]

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TryRecvError, TrySendError};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use multiview_compositor::pipeline::Nv12Image;
use multiview_core::time::{MediaTime, Rational};
use multiview_ffmpeg::EncodedPacket;
use multiview_framestore::{SwapCell, TileStore};

use crate::binding_delivery::WindowDelivery;
use crate::pipeline::PipelineError;
use crate::rendition_dispatch::{
    DispatchError, DtsTimeBase, MoveGate, MoveOutcome, MoveRefusal, RenditionDispatcher,
};
use crate::rendition_workers::{
    BakedTick, DispatchEvent, RenditionWorker, ReorderBound, VideoEncodeWorker,
};
use crate::routing_aux::{AuxFeed, AuxFeeds, PictureBus, PictureBusSlot};

/// Take mailbox capacity (the `COMMAND_CAPACITY` pattern, §4.4).
pub const TAKE_MAILBOX: usize = 64;
/// Outcome stream capacity; full = drop + count.
pub const OUTCOME_CAPACITY: usize = 64;
/// A take whose feed is not fresh is refused after this many ticks (§4.5).
pub const TAKE_PRIME_DEADLINE_TICKS: u64 = 50;
/// A live open must deliver within this many ticks of admission (§4.3).
pub const OPEN_DEADLINE_TICKS: u64 = 90;
/// At most one forced IDR per this many ticks per encoder (§4.2).
pub const FORCE_FLOOR_TICKS: u64 = 60;
/// Held ticks before a bus is reported degraded (§4.2 step 6).
pub const DEGRADED_AFTER_TICKS: u64 = 60;
/// Held ticks before every output on the bus falls back to Program.
pub const FALLBACK_AFTER_TICKS: u64 = 150;
/// A 0-subscriber encoder closes this many ticks later (10 s).
pub const LINGER_TICKS: u64 = 300;
/// Default `routing.max_encoders` (dynamic keys) and per-principal cap.
pub const DEFAULT_MAX_ENCODERS: usize = 6;
/// Default `routing.max_encoders_per_principal`.
pub const DEFAULT_MAX_PER_PRINCIPAL: usize = 2;
/// Open request queue capacity (§4.3).
pub const OPEN_QUEUE: usize = 4;
/// Reaper queue capacity; full = the handle is dropped unjoined (counted).
pub const REAPER_QUEUE: usize = 8;
/// Largest gap a live-opened worker fills with its last canvas.
pub const MAX_GAP_FILL_TICKS: u64 = 8;
/// Live-opened workers' event queue to the consumer.
const DYNAMIC_EVENT_QUEUE: usize = 64;
const NO_FORCE: u64 = u64::MAX;

/// One take request (already authorized by the control plane).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputTake {
    /// Caller's id, echoed in the outcome.
    pub operation_id: String,
    /// Output index in the run's output list.
    pub output_index: usize,
    /// Destination picture bus.
    pub bus: PictureBus,
    /// The principal the encoder is attributed to.
    pub principal: String,
}

/// The take mailbox is full (HTTP 503 `engine-busy` in the later API PR).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TakeBusy;

/// Why a take was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefuseReason {
    /// The feed was not fresh within [`TAKE_PRIME_DEADLINE_TICKS`].
    FeedNotFresh,
    /// No encoder capacity (global or per principal), or the open queue is full.
    Capacity,
    /// The destination encoder did not start, or no keyframe arrived in time.
    EncoderNotReady,
    /// The output or feed cannot be routed.
    Unroutable,
}

/// Why an output fell back to Program on its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FallbackReason {
    /// The aux picture thread failed or stopped publishing.
    AuxFailed,
    /// The output's live-opened encoder failed.
    EncoderFailed,
}

/// The result of one take (or an unrequested fallback).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TakeResult {
    /// The output now carries the requested feed from `tick`.
    Applied,
    /// A newer take for the same output replaced this one.
    Superseded,
    /// Refused; the output kept its feed.
    Refused(RefuseReason),
    /// Moved back to Program without a take.
    Fallback(FallbackReason),
}

/// One outcome on the outcome stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TakeOutcome {
    /// The take's id (empty for a fallback).
    pub operation_id: String,
    /// The output.
    pub output_index: usize,
    /// The tick the outcome was decided (Applied: the first tick of the feed).
    pub tick: u64,
    /// What happened.
    pub outcome: TakeResult,
}

/// A pending take, as seen in the snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingView {
    /// Destination bus name.
    pub bus: String,
    /// Ticks it has waited.
    pub held_ticks: u64,
}

/// One output's routing state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputRouteView {
    /// Current bus name (`program`, `preview`, `src:<id>`).
    pub bus: String,
    /// Internal encoding label.
    pub key: String,
    /// Tick the current feed started.
    pub since_tick: u64,
    /// A take in progress.
    pub pending: Option<PendingView>,
    /// The current bus is holding its last picture beyond the degraded bound.
    pub degraded: bool,
}

/// Wait-free routing state, written by the consumer only on change.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RoutingSnapshot {
    /// Per output, by index.
    pub outputs: Vec<OutputRouteView>,
    /// Clock ticks the aux thread could not take.
    pub dropped_aux_ticks: u64,
    /// Live-opened encoders running (including lingering ones).
    pub dynamic_encoders_live: usize,
    /// Opens admitted and not yet arrived.
    pub opens_in_flight: usize,
    /// Outcomes dropped because nobody read them.
    pub dropped_outcomes: u64,
    /// Workers dropped unjoined because the reaper queue was full.
    pub reaper_overflow: u64,
}

/// The control-plane handle (clone freely).
#[derive(Clone)]
pub struct OutputRoutingHandle {
    takes: SyncSender<OutputTake>,
    snapshot: Arc<SwapCell<Arc<RoutingSnapshot>>>,
    outcomes: Arc<Mutex<Option<Receiver<TakeOutcome>>>>,
}

impl std::fmt::Debug for OutputRoutingHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OutputRoutingHandle")
            .finish_non_exhaustive()
    }
}

impl OutputRoutingHandle {
    /// Enqueue a take without waiting.
    ///
    /// # Errors
    /// [`TakeBusy`] when the mailbox is full or the run ended.
    pub fn try_take(&self, take: OutputTake) -> Result<(), TakeBusy> {
        self.takes.try_send(take).map_err(|_| TakeBusy)
    }

    /// The latest routing snapshot.
    #[must_use]
    pub fn snapshot(&self) -> Arc<RoutingSnapshot> {
        self.snapshot.load()
    }

    /// The outcome stream; `Some` exactly once (one consumer).
    #[must_use]
    pub fn outcomes(&self) -> Option<Receiver<TakeOutcome>> {
        self.outcomes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
    }
}

/// A live-openable encoder: the worker contract plus the live-open seams.
pub trait LiveEncoder: VideoEncodeWorker {
    /// Accept `t0` as the first tick, as a keyframe (`starting_at`).
    fn start_at(&mut self, t0: u64);
    /// The opened encoder's time base (must be 1/cadence).
    fn time_base(&self) -> Rational;
    /// The self-contained-RAP predicate for this encoder's packets.
    fn move_gate(&self) -> MoveGate;
}

/// Opens one encoder for a rendition settings index, on the opener thread.
pub type OpenFn = Box<dyn FnMut(usize) -> Result<Box<dyn LiveEncoder>, String> + Send>;

/// Resolve a source id to its store (wait-free, sees runtime-added sources).
pub type StoreLookup = crate::routing_aux::StoreLookup;

/// A worker wrapper that forces a keyframe at the first item at or after the
/// requested tick. Base workers use it unchanged when never requested.
pub struct ForceAt {
    inner: Box<dyn VideoEncodeWorker>,
    force_at: Arc<AtomicU64>,
}

impl ForceAt {
    /// Wrap `inner`, returning the request cell.
    #[must_use]
    pub fn new(inner: Box<dyn VideoEncodeWorker>) -> (Self, Arc<AtomicU64>) {
        let force_at = Arc::new(AtomicU64::new(NO_FORCE));
        (
            Self {
                inner,
                force_at: Arc::clone(&force_at),
            },
            force_at,
        )
    }
}

fn take_force(cell: &AtomicU64, tick: u64) -> bool {
    let at = cell.load(Ordering::Acquire);
    at != NO_FORCE
        && tick >= at
        && cell
            .compare_exchange(at, NO_FORCE, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
}

impl VideoEncodeWorker for ForceAt {
    fn encode(&mut self, item: BakedTick) -> Result<Vec<EncodedPacket>, PipelineError> {
        if take_force(&self.force_at, item.tick) {
            self.inner.force_keyframe();
        }
        self.inner.encode(item)
    }
    fn reorder_bound(&self) -> ReorderBound {
        self.inner.reorder_bound()
    }
    fn finish(&mut self) -> Result<Vec<EncodedPacket>, PipelineError> {
        self.inner.finish()
    }
    fn force_keyframe(&mut self) {
        self.inner.force_keyframe();
    }
}

/// Live-opened worker: forced-IDR requests plus gap filling. A dropped offer
/// shows up as tick T+k after T; the missing ticks (at most
/// [`MAX_GAP_FILL_TICKS`]) are encoded from the last canvas so the encoder sees
/// consecutive ticks. A larger gap fails this key alone.
struct GapFill {
    inner: Box<dyn LiveEncoder>,
    force_at: Arc<AtomicU64>,
    next: Option<u64>,
    last: Option<Arc<Nv12Image>>,
}

impl VideoEncodeWorker for GapFill {
    fn encode(&mut self, item: BakedTick) -> Result<Vec<EncodedPacket>, PipelineError> {
        let mut out = Vec::new();
        if let Some(next) = self.next {
            if item.tick < next || item.tick - next > MAX_GAP_FILL_TICKS {
                return Err(PipelineError::Output {
                    kind: "routing-encode",
                    reason: format!("tick gap {next}..{}", item.tick),
                });
            }
            if let Some(last) = self.last.clone() {
                for tick in next..item.tick {
                    out.append(&mut self.inner.encode(BakedTick {
                        tick,
                        canvas: Arc::clone(&last),
                    })?);
                }
            }
        }
        if take_force(&self.force_at, item.tick) {
            self.inner.force_keyframe();
        }
        self.next = Some(item.tick.saturating_add(1));
        self.last = Some(Arc::clone(&item.canvas));
        out.append(&mut self.inner.encode(item)?);
        Ok(out)
    }
    fn reorder_bound(&self) -> ReorderBound {
        self.inner.reorder_bound()
    }
    fn finish(&mut self) -> Result<Vec<EncodedPacket>, PipelineError> {
        self.inner.finish()
    }
}

/// Static router configuration.
pub struct RouterConfig {
    /// Program cadence.
    pub cadence: Rational,
    /// Natural GOP in ticks (the base encoders' keyframe interval).
    pub gop: u32,
    /// Per output: its base (Program) key = its rendition settings index.
    pub base_keys: Vec<usize>,
    /// Per base key: the forced-IDR request cell of its worker.
    pub base_force: Vec<Arc<AtomicU64>>,
    /// `routing.max_encoders` (dynamic keys).
    pub max_encoders: usize,
    /// Per-principal cap.
    pub max_per_principal: usize,
    /// Whether an M/E exists (Preview is routable).
    pub has_preview: bool,
}

/// Aux-side wiring the router drives.
pub struct AuxWiring {
    /// Active feeds the aux thread composes.
    pub feeds: AuxFeeds,
    /// The clock's aux-send gate.
    pub active: Arc<AtomicBool>,
    /// Raised when the aux thread failed or ended.
    pub failed: Arc<AtomicBool>,
    /// Clock ticks dropped toward the aux thread.
    pub dropped: Arc<AtomicU64>,
    /// Source stores.
    pub stores: StoreLookup,
    /// The PVW source id.
    pub preview_selection: Option<Arc<SwapCell<Option<Arc<str>>>>>,
}

struct OpenRequest {
    id: u64,
    settings: usize,
}

struct OpenResult {
    id: u64,
    encoder: Result<Box<dyn LiveEncoder>, String>,
}

enum Reap {
    Worker(RenditionWorker),
    Encoder(Box<dyn LiveEncoder>),
}

struct DynamicKey {
    bus: PictureBus,
    settings: usize,
    key: usize,
    worker: Option<RenditionWorker>,
    force_at: Arc<AtomicU64>,
    principal: String,
    linger_deadline: Option<u64>,
    lingering_since: u64,
    last_fed: Option<Arc<Nv12Image>>,
    last_aux_tick: Option<u64>,
    seen_event: bool,
}

struct PendingOpen {
    bus: PictureBus,
    settings: usize,
    principal: String,
    deadline: u64,
    waiter: Option<usize>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// Waiting for the feed to be fresh.
    Priming,
    /// Waiting for the live open `id`.
    Opening(u64),
    /// The live encoder `key` runs; waiting for its first dispatched window
    /// before requesting the move (the dispatcher refuses a move whose
    /// destination window is not complete when the base window closes).
    Warming { key: usize, deadline: u64 },
    /// A dispatcher move to `key` was requested.
    Moving(usize),
}

struct Pending {
    take: OutputTake,
    started: u64,
    phase: Phase,
}

struct OutputState {
    bus: PictureBus,
    key: usize,
    since: u64,
    pending: Option<Pending>,
}

struct BusHealth {
    held: u64,
    fallen: bool,
}

/// The per-run router (lives on the bake consumer).
pub struct OutputRouter {
    cfg: RouterConfig,
    aux: AuxWiring,
    takes: Receiver<OutputTake>,
    outcomes: SyncSender<TakeOutcome>,
    snapshot: Arc<SwapCell<Arc<RoutingSnapshot>>>,
    outputs: Vec<OutputState>,
    dynamic: Vec<DynamicKey>,
    opens: HashMap<u64, PendingOpen>,
    next_open: u64,
    open_tx: SyncSender<OpenRequest>,
    open_rx: Receiver<OpenResult>,
    reap_tx: SyncSender<Reap>,
    dyn_tx: SyncSender<DispatchEvent>,
    dyn_rx: Receiver<DispatchEvent>,
    feeds: HashMap<PictureBus, AuxFeed>,
    health: HashMap<PictureBus, BusHealth>,
    last_forced: HashMap<usize, u64>,
    dropped_outcomes: u64,
    reaper_overflow: u64,
    dirty: bool,
    reaper: Option<std::thread::JoinHandle<()>>,
}

impl OutputRouter {
    /// Build the router, its handle, the opener and the reaper threads.
    ///
    /// # Errors
    /// The OS error if a helper thread cannot be spawned.
    pub fn new(
        cfg: RouterConfig,
        aux: AuxWiring,
        mut open: OpenFn,
    ) -> std::io::Result<(Self, OutputRoutingHandle)> {
        let (take_tx, takes) = mpsc::sync_channel(TAKE_MAILBOX);
        let (outcomes, outcome_rx) = mpsc::sync_channel(OUTCOME_CAPACITY);
        let (open_tx, open_req_rx) = mpsc::sync_channel::<OpenRequest>(OPEN_QUEUE);
        let (open_res_tx, open_rx) = mpsc::sync_channel::<OpenResult>(OPEN_QUEUE);
        std::thread::Builder::new()
            .name("routing-open".to_owned())
            .spawn(move || {
                while let Ok(req) = open_req_rx.recv() {
                    let encoder = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        open(req.settings)
                    }))
                    .unwrap_or_else(|_| Err("encoder open panicked".to_owned()));
                    if open_res_tx
                        .send(OpenResult {
                            id: req.id,
                            encoder,
                        })
                        .is_err()
                    {
                        return;
                    }
                }
            })?;
        let (reap_tx, reap_rx) = mpsc::sync_channel::<Reap>(REAPER_QUEUE);
        let reaper = std::thread::Builder::new()
            .name("routing-reaper".to_owned())
            .spawn(move || {
                while let Ok(item) = reap_rx.recv() {
                    match item {
                        Reap::Worker(worker) => {
                            if let Err(error) = worker.join() {
                                tracing::debug!(%error, "routing: closed encoder ended with an error");
                            }
                        }
                        Reap::Encoder(encoder) => drop(encoder),
                    }
                }
            })?;
        let (dyn_tx, dyn_rx) = mpsc::sync_channel(DYNAMIC_EVENT_QUEUE);
        let outputs = cfg
            .base_keys
            .iter()
            .map(|&key| OutputState {
                bus: PictureBus::Program,
                key,
                since: 0,
                pending: None,
            })
            .collect();
        let snapshot = Arc::new(SwapCell::from_pointee(RoutingSnapshot::default()));
        let handle = OutputRoutingHandle {
            takes: take_tx,
            snapshot: Arc::clone(&snapshot),
            outcomes: Arc::new(Mutex::new(Some(outcome_rx))),
        };
        let mut router = Self {
            cfg,
            aux,
            takes,
            outcomes,
            snapshot,
            outputs,
            dynamic: Vec::new(),
            opens: HashMap::new(),
            next_open: 0,
            open_tx,
            open_rx,
            reap_tx,
            dyn_tx,
            dyn_rx,
            feeds: HashMap::new(),
            health: HashMap::new(),
            last_forced: HashMap::new(),
            dropped_outcomes: 0,
            reaper_overflow: 0,
            dirty: true,
            reaper: Some(reaper),
        };
        router.publish_snapshot();
        Ok((router, handle))
    }

    /// One Program tick: drain dynamic events, takes, opens and dispatcher
    /// outcomes; advance every take; feed live-opened encoders. Never waits.
    ///
    /// # Errors
    /// A genuine (non-admission) dispatcher fault, exactly as the pipeline's
    /// own drain treats it.
    pub(crate) fn on_tick<D: WindowDelivery>(
        &mut self,
        tick: u64,
        pts: MediaTime,
        dispatcher: &mut RenditionDispatcher,
        delivery: &mut D,
    ) -> Result<(), DispatchError> {
        self.drain_live_events(dispatcher, delivery)?;
        self.drain_takes(tick, dispatcher);
        self.drain_opens(tick, dispatcher);
        self.drain_moves(tick, dispatcher);
        self.advance_takes(tick, pts, dispatcher);
        self.feed(tick);
        self.check_aux(tick, dispatcher);
        self.linger(tick, dispatcher);
        self.sync_feeds();
        if self.dirty || self.outputs.iter().any(|o| o.pending.is_some()) {
            self.publish_snapshot_at(tick);
        }
        Ok(())
    }

    /// Dispatch the live-opened workers' events. Called BEFORE the pipeline
    /// drains the base workers' events each tick, so a live window for tick T
    /// is staged before base window T can close (a move needs it complete).
    ///
    /// # Errors
    /// A genuine (non-admission) dispatcher fault.
    pub(crate) fn drain_live_events<D: WindowDelivery>(
        &mut self,
        dispatcher: &mut RenditionDispatcher,
        delivery: &mut D,
    ) -> Result<(), DispatchError> {
        while let Ok(event) = self.dyn_rx.try_recv() {
            if let DispatchEvent::Video { key_index, .. } = &event {
                if let Some(d) = self.dynamic.iter_mut().find(|d| d.key == *key_index) {
                    d.seen_event = true;
                }
            }
            if let Err(e) = dispatcher.dispatch(event, Instant::now(), delivery) {
                if !matches!(e, DispatchError::Admission { .. }) {
                    return Err(e);
                }
            }
        }
        Ok(())
    }

    fn emit(&mut self, operation_id: String, output_index: usize, tick: u64, outcome: TakeResult) {
        self.dirty = true;
        if self
            .outcomes
            .try_send(TakeOutcome {
                operation_id,
                output_index,
                tick,
                outcome,
            })
            .is_err()
        {
            self.dropped_outcomes = self.dropped_outcomes.saturating_add(1);
        }
    }

    fn drain_takes(&mut self, tick: u64, dispatcher: &mut RenditionDispatcher) {
        loop {
            let take = match self.takes.try_recv() {
                Ok(take) => take,
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => return,
            };
            let index = take.output_index;
            let routable = index < self.outputs.len()
                && (take.bus != PictureBus::Preview || self.cfg.has_preview);
            if !routable {
                self.emit(
                    take.operation_id,
                    index,
                    tick,
                    TakeResult::Refused(RefuseReason::Unroutable),
                );
                continue;
            }
            self.supersede(index, tick, dispatcher);
            let Some(out) = self.outputs.get_mut(index) else {
                continue;
            };
            out.pending = Some(Pending {
                take,
                started: tick,
                phase: Phase::Priming,
            });
            self.dirty = true;
        }
    }

    /// Replace a pending take: the old one is Superseded, an in-dispatcher
    /// move is cancelled by re-asserting the current key.
    fn supersede(&mut self, index: usize, tick: u64, dispatcher: &mut RenditionDispatcher) {
        let Some(old) = self.outputs.get_mut(index).and_then(|o| o.pending.take()) else {
            return;
        };
        if matches!(old.phase, Phase::Moving(_)) {
            self.cancel_dispatcher_move(index, dispatcher, tick);
        }
        if let Phase::Opening(id) = old.phase {
            if let Some(open) = self.opens.get_mut(&id) {
                open.waiter = None;
            }
        }
        self.emit(old.take.operation_id, index, tick, TakeResult::Superseded);
    }

    fn cancel_dispatcher_move(
        &self,
        index: usize,
        dispatcher: &mut RenditionDispatcher,
        tick: u64,
    ) {
        if let Some(out) = self.outputs.get(index) {
            let _ = dispatcher.rebind_video(index, out.key, tick, 1);
        }
    }

    fn drain_opens(&mut self, tick: u64, dispatcher: &mut RenditionDispatcher) {
        while let Ok(result) = self.open_rx.try_recv() {
            let Some(open) = self.opens.remove(&result.id) else {
                // Late (deadline passed): never used.
                if let Ok(encoder) = result.encoder {
                    self.reap(Reap::Encoder(encoder));
                }
                continue;
            };
            self.dirty = true;
            let waiter = open.waiter;
            let refuse = |router: &mut Self| {
                if let Some(index) = waiter {
                    router.refuse_pending(index, tick, RefuseReason::EncoderNotReady);
                }
            };
            let Ok(mut encoder) = result.encoder else {
                refuse(self);
                continue;
            };
            let tb = encoder.time_base();
            let Some(index) = waiter else {
                self.reap(Reap::Encoder(encoder));
                continue;
            };
            encoder.start_at(tick);
            let gate = encoder.move_gate();
            let Ok(key) = dispatcher.add_video_key(
                DtsTimeBase {
                    num: tb.num,
                    den: tb.den,
                },
                gate,
            ) else {
                self.reap(Reap::Encoder(encoder));
                refuse(self);
                continue;
            };
            let force_at = Arc::new(AtomicU64::new(NO_FORCE));
            let worker = GapFill {
                inner: encoder,
                force_at: Arc::clone(&force_at),
                next: None,
                last: None,
            };
            let tx = self.dyn_tx.clone();
            let spawned =
                RenditionWorker::spawn(key, self.cfg.cadence, tb, Box::new(worker), move |event| {
                    tx.send(event).map_err(|e| PipelineError::Output {
                        kind: "routing-worker",
                        reason: e.to_string(),
                    })
                });
            let Ok(worker) = spawned else {
                let _ = dispatcher.retire_video_key(key);
                refuse(self);
                continue;
            };
            self.last_forced.insert(key, tick);
            self.dynamic.push(DynamicKey {
                bus: open.bus,
                settings: open.settings,
                key,
                worker: Some(worker),
                force_at,
                principal: open.principal,
                linger_deadline: None,
                lingering_since: tick,
                last_fed: None,
                last_aux_tick: None,
                seen_event: false,
            });
            // The first offer (frame T, an IDR) is made by `feed` this same
            // tick, after the key exists. The move is requested once the key's
            // first window has been dispatched (see `Phase::Warming`).
            if let Some(p) = self.outputs.get_mut(index).and_then(|o| o.pending.as_mut()) {
                p.phase = Phase::Warming {
                    key,
                    deadline: open.deadline,
                };
            }
        }
    }

    fn refuse_pending(&mut self, index: usize, tick: u64, reason: RefuseReason) {
        if let Some(p) = self.outputs.get_mut(index).and_then(|o| o.pending.take()) {
            self.emit(
                p.take.operation_id,
                index,
                tick,
                TakeResult::Refused(reason),
            );
        }
    }

    fn drain_moves(&mut self, tick: u64, dispatcher: &mut RenditionDispatcher) {
        for outcome in dispatcher.take_move_outcomes() {
            match outcome {
                MoveOutcome::Applied {
                    output_index,
                    key,
                    tick: at,
                } => {
                    let Some(out) = self.outputs.get_mut(output_index) else {
                        continue;
                    };
                    let expected = matches!(out.pending.as_ref().map(|p| p.phase), Some(Phase::Moving(k)) if k == key);
                    if expected {
                        if let Some(p) = out.pending.take() {
                            out.bus = p.take.bus.clone();
                            out.key = key;
                            out.since = at;
                            self.emit(p.take.operation_id, output_index, at, TakeResult::Applied);
                        }
                    } else if out.key != key {
                        // A fallback move landing (no take pending for it).
                        out.key = key;
                        out.since = at;
                        self.dirty = true;
                    }
                }
                MoveOutcome::Refused {
                    output_index,
                    reason,
                } => {
                    let moving = self
                        .outputs
                        .get(output_index)
                        .and_then(|o| o.pending.as_ref())
                        .is_some_and(|p| matches!(p.phase, Phase::Moving(_)));
                    if moving {
                        let _ = reason == MoveRefusal::NoGate;
                        self.refuse_pending(output_index, tick, RefuseReason::EncoderNotReady);
                    }
                }
                MoveOutcome::Superseded { .. } => {}
                MoveOutcome::Fallback { output_index } => {
                    if let Some(out) = self.outputs.get_mut(output_index) {
                        out.bus = PictureBus::Program;
                        out.key = self
                            .cfg
                            .base_keys
                            .get(output_index)
                            .copied()
                            .unwrap_or(out.key);
                        out.since = tick;
                    }
                    self.emit(
                        String::new(),
                        output_index,
                        tick,
                        TakeResult::Fallback(FallbackReason::EncoderFailed),
                    );
                }
            }
        }
    }

    /// Lag rule and aux failure: move every output on a held bus to Program.
    fn check_aux(&mut self, tick: u64, dispatcher: &mut RenditionDispatcher) {
        let failed = self.aux.failed.load(Ordering::Acquire);
        let fallen: Vec<PictureBus> = self
            .health
            .iter_mut()
            .filter(|(_, h)| !h.fallen && (failed || h.held >= FALLBACK_AFTER_TICKS))
            .map(|(bus, h)| {
                h.fallen = true;
                bus.clone()
            })
            .collect();
        for bus in fallen {
            for index in 0..self.outputs.len() {
                let on_bus = self.outputs.get(index).is_some_and(|o| o.bus == bus);
                if !on_bus {
                    continue;
                }
                let Some(&base) = self.cfg.base_keys.get(index) else {
                    continue;
                };
                let windows = self.request_idr(base, tick);
                if dispatcher.rebind_video(index, base, tick, windows).is_ok() {
                    if let Some(out) = self.outputs.get_mut(index) {
                        out.bus = PictureBus::Program;
                    }
                    self.emit(
                        String::new(),
                        index,
                        tick,
                        TakeResult::Fallback(FallbackReason::AuxFailed),
                    );
                }
            }
        }
    }

    /// Ask `key` for an IDR at `tick` within the per-key floor; returns the
    /// number of windows the move may wait.
    fn request_idr(&mut self, key: usize, tick: u64) -> u32 {
        let last = self.last_forced.get(&key).copied();
        if last.is_none_or(|l| tick.saturating_sub(l) >= FORCE_FLOOR_TICKS) {
            let cell = self.cfg.base_force.get(key).or_else(|| {
                self.dynamic
                    .iter()
                    .find(|d| d.key == key)
                    .map(|d| &d.force_at)
            });
            if let Some(cell) = cell {
                cell.store(tick, Ordering::Release);
                self.last_forced.insert(key, tick);
                return 3;
            }
        }
        self.cfg.gop.saturating_add(3)
    }

    fn slot_fresh(&self, bus: &PictureBus, tick: u64) -> bool {
        self.feeds
            .get(bus)
            .and_then(|f| f.slot.latest.load())
            .is_some_and(|frame| frame.tick.saturating_add(1) >= tick)
    }

    fn feed_fresh(&self, bus: &PictureBus, tick: u64, pts: MediaTime) -> bool {
        let source: Option<Arc<str>> = match bus {
            PictureBus::Program => return true,
            PictureBus::Source(id) => Some(Arc::from(id.as_str())),
            PictureBus::Preview => self.aux.preview_selection.as_ref().and_then(|s| s.load()),
        };
        let live = source
            .and_then(|id| (self.aux.stores)(&id))
            .is_some_and(|store: Arc<TileStore<Nv12Image>>| store.state_at(pts).is_live());
        live && !self.aux.failed.load(Ordering::Acquire) && self.slot_fresh(bus, tick)
    }

    fn advance_takes(&mut self, tick: u64, pts: MediaTime, dispatcher: &mut RenditionDispatcher) {
        for index in 0..self.outputs.len() {
            let Some((bus, started, phase, principal)) = self.outputs.get(index).and_then(|o| {
                o.pending.as_ref().map(|p| {
                    (
                        p.take.bus.clone(),
                        p.started,
                        p.phase,
                        p.take.principal.clone(),
                    )
                })
            }) else {
                continue;
            };
            match phase {
                Phase::Moving(_) => {}
                Phase::Warming { key, deadline } => {
                    let seen = self
                        .dynamic
                        .iter()
                        .find(|d| d.key == key)
                        .map(|d| d.seen_event);
                    if tick >= deadline || seen.is_none() || dispatcher.is_key_failed(key) {
                        self.refuse_pending(index, tick, RefuseReason::EncoderNotReady);
                    } else if seen == Some(true) {
                        // The key has no subscriber yet: an IDR on it costs no
                        // viewer anything, so the warm-up force skips the floor.
                        let at = tick.saturating_add(1);
                        if let Some(d) = self.dynamic.iter().find(|d| d.key == key) {
                            d.force_at.store(at, Ordering::Release);
                        }
                        self.last_forced.insert(key, at);
                        let windows = self.cfg.gop.saturating_add(3);
                        if dispatcher.rebind_video(index, key, at, windows).is_ok() {
                            if let Some(p) =
                                self.outputs.get_mut(index).and_then(|o| o.pending.as_mut())
                            {
                                p.phase = Phase::Moving(key);
                            }
                        } else {
                            self.refuse_pending(index, tick, RefuseReason::EncoderNotReady);
                        }
                    }
                }
                Phase::Opening(id) => {
                    let late = self.opens.get(&id).is_none_or(|o| tick >= o.deadline);
                    if late {
                        // The result, if it ever comes, goes to the reaper.
                        self.opens.remove(&id);
                        self.refuse_pending(index, tick, RefuseReason::EncoderNotReady);
                    }
                }
                Phase::Priming => {
                    let current = self.outputs.get(index).map(|o| o.bus.clone());
                    if current.as_ref() == Some(&bus) {
                        if let Some(p) = self.outputs.get_mut(index).and_then(|o| o.pending.take())
                        {
                            self.cancel_dispatcher_move(index, dispatcher, tick);
                            self.emit(p.take.operation_id, index, tick, TakeResult::Applied);
                        }
                        continue;
                    }
                    if !self.feed_fresh(&bus, tick, pts) {
                        if tick.saturating_sub(started) >= TAKE_PRIME_DEADLINE_TICKS {
                            self.refuse_pending(index, tick, RefuseReason::FeedNotFresh);
                        }
                        continue;
                    }
                    self.start_move(index, &bus, &principal, tick, dispatcher);
                }
            }
        }
    }

    fn start_move(
        &mut self,
        index: usize,
        bus: &PictureBus,
        principal: &str,
        tick: u64,
        dispatcher: &mut RenditionDispatcher,
    ) {
        let Some(&settings) = self.cfg.base_keys.get(index) else {
            return;
        };
        let existing = if *bus == PictureBus::Program {
            Some(settings)
        } else {
            self.dynamic
                .iter()
                .find(|d| {
                    d.bus == *bus && d.settings == settings && !dispatcher.is_key_failed(d.key)
                })
                .map(|d| d.key)
        };
        if let Some(key) = existing {
            let windows = self.request_idr(key, tick);
            let set = |router: &mut Self, phase| {
                if let Some(p) = router
                    .outputs
                    .get_mut(index)
                    .and_then(|o| o.pending.as_mut())
                {
                    p.phase = phase;
                }
            };
            if dispatcher.rebind_video(index, key, tick, windows).is_ok() {
                set(self, Phase::Moving(key));
                // A re-take adopts the key (re-attributed, linger cleared).
                if let Some(d) = self.dynamic.iter_mut().find(|d| d.key == key) {
                    principal.clone_into(&mut d.principal);
                    d.linger_deadline = None;
                }
            } else {
                self.refuse_pending(index, tick, RefuseReason::EncoderNotReady);
            }
            return;
        }
        // Admission: global and per-principal, after closing 0-subscriber lingers.
        while !self.has_capacity(principal) {
            if !self.close_oldest_linger(dispatcher, None) {
                break;
            }
        }
        if !self.has_capacity(principal) {
            self.refuse_pending(index, tick, RefuseReason::Capacity);
            return;
        }
        let id = self.next_open;
        self.next_open = self.next_open.wrapping_add(1);
        match self.open_tx.try_send(OpenRequest { id, settings }) {
            Ok(()) => {
                self.opens.insert(
                    id,
                    PendingOpen {
                        bus: bus.clone(),
                        settings,
                        principal: principal.to_owned(),
                        deadline: tick.saturating_add(OPEN_DEADLINE_TICKS),
                        waiter: Some(index),
                    },
                );
                if let Some(p) = self.outputs.get_mut(index).and_then(|o| o.pending.as_mut()) {
                    p.phase = Phase::Opening(id);
                }
                self.dirty = true;
            }
            Err(TrySendError::Full(_) | TrySendError::Disconnected(_)) => {
                self.refuse_pending(index, tick, RefuseReason::Capacity);
            }
        }
    }

    fn has_capacity(&self, principal: &str) -> bool {
        let total = self.dynamic.len() + self.opens.len();
        let mine = self
            .dynamic
            .iter()
            .filter(|d| d.principal == principal)
            .count()
            + self
                .opens
                .values()
                .filter(|o| o.principal == principal)
                .count();
        total < self.cfg.max_encoders && mine < self.cfg.max_per_principal
    }

    fn subscribers(&self, key: usize) -> usize {
        self.outputs
            .iter()
            .filter(|o| {
                o.key == key
                    || matches!(
                        o.pending.as_ref().map(|p| p.phase),
                        Some(Phase::Moving(k) | Phase::Warming { key: k, .. }) if k == key
                    )
            })
            .count()
    }

    /// Feed every live-opened encoder its bus's frame for this tick (hold
    /// the last one when none is new), counting held ticks per bus.
    fn feed(&mut self, tick: u64) {
        let mut fresh_buses: Vec<(PictureBus, bool)> = Vec::new();
        for d in &mut self.dynamic {
            let latest = self.feeds.get(&d.bus).and_then(|f| f.slot.latest.load());
            let new = latest
                .as_ref()
                .filter(|f| d.last_aux_tick.is_none_or(|t| f.tick > t));
            let fresh = new.is_some();
            if let Some(frame) = new {
                d.last_aux_tick = Some(frame.tick);
                d.last_fed = Some(Arc::clone(&frame.frame));
            }
            fresh_buses.push((d.bus.clone(), fresh));
            let (Some(canvas), Some(worker)) = (d.last_fed.clone(), d.worker.as_ref()) else {
                continue;
            };
            // Never blocks: a full queue drops and the worker gap-fills.
            let _ = worker.offer(BakedTick { tick, canvas });
        }
        for (bus, fresh) in fresh_buses {
            let health = self.health.entry(bus).or_insert(BusHealth {
                held: 0,
                fallen: false,
            });
            let before = health.held >= DEGRADED_AFTER_TICKS;
            if fresh {
                health.held = 0;
                health.fallen = false;
            } else {
                health.held = health.held.saturating_add(1);
            }
            if before != (health.held >= DEGRADED_AFTER_TICKS) {
                self.dirty = true;
            }
        }
    }

    fn linger(&mut self, tick: u64, dispatcher: &mut RenditionDispatcher) {
        let counts: Vec<usize> = self
            .dynamic
            .iter()
            .map(|d| self.subscribers(d.key))
            .collect();
        let mut newly: Vec<(usize, String)> = Vec::new();
        for (d, &subs) in self.dynamic.iter_mut().zip(&counts) {
            if subs > 0 {
                d.linger_deadline = None;
            } else if d.linger_deadline.is_none() {
                d.linger_deadline = Some(tick.saturating_add(LINGER_TICKS));
                d.lingering_since = tick;
                newly.push((d.key, d.principal.clone()));
            }
        }
        // One 0-subscriber linger per principal: its older ones close now.
        for (key, principal) in newly {
            while self
                .dynamic
                .iter()
                .any(|d| d.key != key && d.principal == principal && d.linger_deadline.is_some())
            {
                if !self.close_oldest_linger(dispatcher, Some((&principal, key))) {
                    break;
                }
            }
        }
        let due: Vec<usize> = self
            .dynamic
            .iter()
            .filter(|d| d.linger_deadline.is_some_and(|t| tick >= t))
            .map(|d| d.key)
            .collect();
        for key in due {
            self.close(key, dispatcher);
        }
    }

    fn close_oldest_linger(
        &mut self,
        dispatcher: &mut RenditionDispatcher,
        only: Option<(&str, usize)>,
    ) -> bool {
        let oldest = self
            .dynamic
            .iter()
            .filter(|d| d.linger_deadline.is_some())
            .filter(|d| only.is_none_or(|(p, keep)| d.principal == p && d.key != keep))
            .min_by_key(|d| d.lingering_since)
            .map(|d| d.key);
        oldest.is_some_and(|key| {
            self.close(key, dispatcher);
            true
        })
    }

    /// Retire the key and hand its worker to the reaper. Never joins here.
    fn close(&mut self, key: usize, dispatcher: &mut RenditionDispatcher) {
        let _ = dispatcher.retire_video_key(key);
        if let Some(pos) = self.dynamic.iter().position(|d| d.key == key) {
            let mut d = self.dynamic.swap_remove(pos);
            if let Some(worker) = d.worker.take() {
                self.reap(Reap::Worker(worker));
            }
            self.last_forced.remove(&key);
            self.dirty = true;
        }
    }

    fn reap(&mut self, item: Reap) {
        if self.reap_tx.try_send(item).is_err() {
            // Dropped unjoined: the input closes, the thread ends on its own.
            self.reaper_overflow = self.reaper_overflow.saturating_add(1);
        }
    }

    /// Keep the aux thread's feed set equal to the buses in use; raise the
    /// clock's send gate only while one exists.
    fn sync_feeds(&mut self) {
        let mut wanted: Vec<PictureBus> = self.dynamic.iter().map(|d| d.bus.clone()).collect();
        wanted.extend(self.opens.values().map(|o| o.bus.clone()));
        wanted.extend(
            self.outputs
                .iter()
                .filter_map(|o| o.pending.as_ref().map(|p| p.take.bus.clone())),
        );
        wanted.retain(|b| *b != PictureBus::Program);
        let unchanged = wanted.iter().all(|b| self.feeds.contains_key(b))
            && self.feeds.keys().all(|b| wanted.contains(b));
        if unchanged {
            return;
        }
        self.feeds.retain(|b, _| wanted.contains(b));
        self.health.retain(|b, _| wanted.contains(b));
        for bus in wanted {
            self.feeds
                .entry(bus.clone())
                .or_insert_with(|| AuxFeed::new(bus));
        }
        let list: Vec<AuxFeed> = self.feeds.values().cloned().collect();
        let active = !list.is_empty();
        self.aux.feeds.store(Arc::new(list));
        self.aux.active.store(active, Ordering::Release);
        self.dirty = true;
    }

    fn publish_snapshot(&mut self) {
        self.publish_snapshot_at(0);
    }

    fn publish_snapshot_at(&mut self, tick: u64) {
        let outputs = self
            .outputs
            .iter()
            .map(|o| OutputRouteView {
                bus: o.bus.as_str().into_owned(),
                key: format!("{}/{}", o.bus.as_str(), o.key),
                since_tick: o.since,
                pending: o.pending.as_ref().map(|p| PendingView {
                    bus: p.take.bus.as_str().into_owned(),
                    held_ticks: tick.saturating_sub(p.started),
                }),
                degraded: self
                    .health
                    .get(&o.bus)
                    .is_some_and(|h| h.held >= DEGRADED_AFTER_TICKS),
            })
            .collect();
        let next = RoutingSnapshot {
            outputs,
            dropped_aux_ticks: self.aux.dropped.load(Ordering::Relaxed),
            dynamic_encoders_live: self.dynamic.len(),
            opens_in_flight: self.opens.len(),
            dropped_outcomes: self.dropped_outcomes,
            reaper_overflow: self.reaper_overflow,
        };
        if *self.snapshot.load() != next {
            self.snapshot.store(Arc::new(next));
        }
        self.dirty = false;
    }

    /// The picture-bus slot of an active feed (tests and diagnostics).
    #[must_use]
    pub fn slot(&self, bus: &PictureBus) -> Option<Arc<PictureBusSlot>> {
        self.feeds.get(bus).map(|f| Arc::clone(&f.slot))
    }

    /// End of run: retire every live key and hand its worker to the reaper,
    /// then let the reaper finish. Joins only the reaper, bounded by `bound`.
    pub(crate) fn shutdown(
        mut self,
        dispatcher: &mut RenditionDispatcher,
        bound: std::time::Duration,
    ) {
        let keys: Vec<usize> = self.dynamic.iter().map(|d| d.key).collect();
        for key in keys {
            self.close(key, dispatcher);
        }
        self.aux.active.store(false, Ordering::Release);
        let reaper = self.reaper.take();
        drop(self);
        if let Some(reaper) = reaper {
            let deadline = Instant::now() + bound;
            while !reaper.is_finished() && Instant::now() < deadline {
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            if reaper.is_finished() {
                let _ = reaper.join();
            }
        }
    }
}

#[cfg(test)]
#[path = "output_routing_tests.rs"]
mod tests;
