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
    DispatchError, DtsTimeBase, MoveGate, MoveOutcome, MoveRefusal, ParameterSets,
    RenditionDispatcher,
};
use crate::rendition_workers::{
    BakedTick, ClosingWorker, DispatchEvent, OfferRejection, RenditionWorker, ReorderBound,
    VideoEncodeWorker,
};
pub use crate::routing_audio::AudioChoice;
use crate::routing_audio::{AudioTakeProgress, AudioTakeStart};
use crate::routing_aux::{AuxFeed, AuxFeeds, AuxFormat, PictureBus, PictureBusSlot};

/// Longest accepted `principal` / `operation_id`, in bytes.
pub const MAX_TAKE_ID_BYTES: usize = 128;
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
/// Sized to cover a full consumer substitution gap (`FRAME_SUBSTITUTION_LIMIT`
/// = 8 in the pipeline) on top of a full worker queue (`WORKER_QUEUE_TICKS` = 4)
/// and the offers dropped meanwhile, so a transient stall never fails a key.
pub const MAX_GAP_FILL_TICKS: u64 = 24;
/// Operation-id prefixes of a start-up (boot) take — `boot:<output index>`
/// and the audio-only `boot:audio:<output index>` — defined once in the
/// control plane, whose outcome relay never persists a boot outcome
/// ([`multiview_control::output_routing::is_boot_take`]). Every boot id is
/// keyed on the OUTPUT INDEX, never parsed from the config id (PR #352 round
/// 2): a config id (an HLS one is a path) can be arbitrarily long.
pub use multiview_control::output_routing::{BOOT_AUDIO_TAKE_PREFIX, BOOT_TAKE_PREFIX};

/// Whether `operation_id` is a video (or combined) boot take — the kind that
/// enters the bounded retry window. An audio-only boot take
/// (`boot:audio:<id>`) is a boot take too (never persisted) but has no fresh
/// gate, so it is never retried.
fn is_boot_video_take(operation_id: &str) -> bool {
    multiview_control::output_routing::is_boot_take(operation_id)
        && !operation_id.starts_with(BOOT_AUDIO_TAKE_PREFIX)
}
/// The principal a boot take is charged to.
pub const BOOT_PRINCIPAL: &str = "system:boot";
/// A refused boot take is re-issued at most once per this many seconds …
pub const BOOT_RETRY_SECS: i64 = 2;
/// … for at most this long after the run's first tick; then the output stays
/// on Program (logged).
pub const BOOT_WINDOW_SECS: i64 = 30;
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
    /// Destination picture bus (`None`: an audio-only take, the video is
    /// untouched).
    pub bus: Option<PictureBus>,
    /// The output's new audio choice (`None`: keep the current one).
    pub audio: Option<AudioChoice>,
    /// The principal the encoder is attributed to.
    pub principal: String,
}

/// The take was not enqueued: the mailbox is full or the run ended (HTTP 503
/// `engine-busy` in the later API PR), or the take's `principal` /
/// `operation_id` exceeds [`MAX_TAKE_ID_BYTES`].
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
    /// The feed's encoder configuration differs from the output's and the
    /// output's container cannot take it in band (output-routing.md §4.2).
    FormatMismatch,
}

impl RefuseReason {
    /// Operator-facing explanation (never names a group or shared encoder).
    #[must_use]
    pub const fn operator_message(self) -> &'static str {
        match self {
            Self::FeedNotFresh => "This feed has no current picture; the take was refused",
            Self::Capacity => "No encoder capacity is available; the take was refused",
            Self::EncoderNotReady => {
                "This feed's encoder did not become ready; the take was refused"
            }
            Self::Unroutable => "This feed cannot be routed to the output; the take was refused",
            Self::FormatMismatch => {
                "This feed's encoder settings differ from the output's; the take was refused \
                 to avoid a corrupt stream"
            }
        }
    }
}

/// Why an output fell back to Program on its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FallbackReason {
    /// The aux picture thread failed or stopped publishing.
    AuxFailed,
    /// The output's live-opened encoder failed, ended or wedged.
    EncoderFailed,
}

impl FallbackReason {
    /// Operator-facing explanation (never names a group or shared encoder).
    #[must_use]
    pub const fn operator_message(self) -> &'static str {
        match self {
            Self::AuxFailed => {
                "This feed stopped delivering pictures; the output is back on Program"
            }
            Self::EncoderFailed => {
                "The encoder for this feed stopped; the output is back on Program"
            }
        }
    }
}

/// A live key whose worker produced no event for this many consecutive
/// offered ticks is wedged (design §9 chaos 5) and is closed: its outputs
/// fall back to Program at a RAP.
pub const WEDGED_ENCODER_TICKS: u32 = 90;

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
    /// The output's bus after this outcome (for `Applied`, the taken bus):
    /// lets the control plane persist an applied take from the outcome alone.
    pub bus: PictureBus,
    /// The output's audio choice after this outcome (`None` for a run
    /// without program audio), for persistence.
    pub audio: Option<AudioChoice>,
}

/// A pending take, as seen in the snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingView {
    /// Destination bus name (the current bus for an audio-only take).
    pub bus: String,
    /// The take's audio choice label, when it carries one.
    pub audio: Option<String>,
    /// Ticks it has waited.
    pub held_ticks: u64,
}

/// One output's routing state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputRouteView {
    /// Current bus name (`program`, `preview`, `clean`, `src:<id>`).
    pub bus: String,
    /// Internal encoding label.
    pub key: String,
    /// Tick the current feed started.
    pub since_tick: u64,
    /// A take in progress.
    pub pending: Option<PendingView>,
    /// The current bus is holding its last picture beyond the degraded bound.
    pub degraded: bool,
    /// The audio the output carries (once scheduled changes landed):
    /// `program`, `none`, `src:<id>`, `track:<name>`, `preview`.
    pub audio: String,
    /// The output's audio choice: `follow`, `program`, `none`, `src:<id>`,
    /// `track:<name>`.
    pub audio_choice: String,
    /// Whether the output runs a private encoded audio bus now (a take that
    /// changes its audio then costs no new audio encode).
    pub private_audio: bool,
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
    /// Live-opened encoders closed because their worker ended (panic, error)
    /// or produced nothing for [`WEDGED_ENCODER_TICKS`] offered ticks.
    pub dead_encoders: u64,
    /// Live-opened workers' events dropped because the consumer queue was
    /// full (the key then fails alone) or the key was already closed.
    pub dropped_live_events: u64,
    /// Applied moves whose cut RAP carried different parameter sets than the
    /// output's previous feed (each reported a discontinuity to that output).
    pub parameter_set_changes: u64,
    /// Video encoder keys the dispatcher has ever registered (base + live).
    pub video_keys: usize,
    /// Clock ticks handed to the aux thread (0 while nothing is routed).
    pub aux_ticks_sent: u64,
    /// Per-output private encoded audio buses running (+1 audio encode
    /// each, CPU).
    pub audio_buses_live: usize,
    /// Routed-audio counters (opened, failed, refused, faded back).
    pub audio: crate::routing_audio::AudioRoutingCounters,
    /// Live-opened encoder keys as `(feed name, settings index)`, sorted,
    /// lingering ones included and failed ones excluded: a take onto one of
    /// these reuses it (+0 encoders). The feed name is the bus name for the
    /// canvas format and `bus@WxH/mode/anchor` for an output format (#342):
    /// a different format is a different key (+1).
    pub live_keys: Vec<(String, usize)>,
    /// Start-up (boot) takes still inside their retry window (#329).
    pub boot_takes_pending: usize,
    /// Start-up takes given up: the output stayed on Program.
    pub boot_takes_given_up: u64,
    /// Pending dispatcher moves the router could not cancel by re-asserting
    /// the output's key (it was already failed/retired); the move was
    /// cancelled directly instead (#330 item 4).
    pub move_cancel_failures: u64,
}

/// The control-plane handle (clone freely).
///
/// Trust boundary: `OutputTake::principal` is trusted exactly as given — it
/// is the identity encoder capacity is charged to and attributed to. The
/// control plane must pass the authenticated key id only (never a
/// client-supplied string) and must have authorized the take (§10) before
/// calling [`Self::try_take`]. Both strings are length-bounded here.
#[derive(Clone)]
pub struct OutputRoutingHandle {
    #[cfg(test)]
    pub(crate) audio_tap: crate::routing_audio::AudioTapCell,
    takes: SyncSender<OutputTake>,
    snapshot: Arc<SwapCell<Arc<RoutingSnapshot>>>,
    outcomes: Arc<Mutex<Option<Receiver<TakeOutcome>>>>,
    outputs: Arc<[RunOutput]>,
    has_preview: bool,
}

/// One routable position of the run (the router's output index).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunOutput {
    /// The output's config id (`None` for an internal sink, e.g. the
    /// `program.ts` anchor).
    pub id: Option<String>,
    /// The base (Program) encoder settings index (names its codec settings
    /// AND its base format, #342).
    pub settings: usize,
    /// The format a routed source/PVW is composed at for this output (#342;
    /// `None` = the canvas with the default fit).
    pub feed_format: Option<AuxFormat>,
    /// Whether slice 1 can cut this output cleanly (its encoder family has
    /// the parameter-set RAP check; VAAPI/VideoToolbox do not, §4.2).
    pub routable: bool,
    /// Whether the output's binding has a program-audio slot, the slot a
    /// routed audio feed rides (§5.5). `false`: audio takes are refused.
    pub program_audio: bool,
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
    /// [`TakeBusy`] when the mailbox is full or the run ended, or an id is
    /// longer than [`MAX_TAKE_ID_BYTES`].
    pub fn try_take(&self, take: OutputTake) -> Result<(), TakeBusy> {
        if take.principal.len() > MAX_TAKE_ID_BYTES || take.operation_id.len() > MAX_TAKE_ID_BYTES {
            return Err(TakeBusy);
        }
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

    /// Attach the run's output map (router index → config id, settings).
    #[must_use]
    pub fn with_outputs(mut self, outputs: Vec<RunOutput>) -> Self {
        self.outputs = outputs.into();
        self
    }

    /// The run's output map, by router index.
    #[must_use]
    pub fn outputs(&self) -> &[RunOutput] {
        &self.outputs
    }

    /// Whether the run has an M/E (Preview is routable).
    #[must_use]
    pub const fn has_preview(&self) -> bool {
        self.has_preview
    }

    /// The router index of the output whose config id is `id`.
    #[must_use]
    pub fn index_of(&self, id: &str) -> Option<usize> {
        self.outputs
            .iter()
            .position(|o| o.id.as_deref() == Some(id))
    }
}

/// Where a run publishes its [`OutputRoutingHandle`] for the control plane.
pub type OutputRoutingSlot = Arc<SwapCell<Option<Arc<OutputRoutingHandle>>>>;

/// An empty [`OutputRoutingSlot`].
#[must_use]
pub fn output_routing_slot() -> OutputRoutingSlot {
    Arc::new(SwapCell::empty())
}

/// A live-openable encoder: the worker contract plus the live-open seams.
pub trait LiveEncoder: VideoEncodeWorker {
    /// Accept `t0` as the first tick, as a keyframe (`starting_at`).
    fn start_at(&mut self, t0: u64);
    /// The opened encoder's time base (must be 1/cadence).
    fn time_base(&self) -> Rational;
    /// The self-contained-RAP predicate for this encoder's packets.
    fn move_gate(&self) -> MoveGate;
    /// The in-band parameter-set extractor for this encoder's RAPs (compared
    /// at a move; `None` = not compared, a plain splice).
    fn parameter_sets(&self) -> Option<ParameterSets> {
        None
    }
}

/// Opens one encoder for a rendition settings index, on the opener thread.
pub type OpenFn = Box<dyn FnMut(usize) -> Result<Box<dyn LiveEncoder>, String> + Send>;

/// An opener that also learns the bus (#342): a Program/Clean key at a
/// non-canvas format converts on its own worker.
pub type BusOpenFn =
    Box<dyn FnMut(usize, &PictureBus) -> Result<Box<dyn LiveEncoder>, String> + Send>;

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
        let force_at = force_cell();
        (Self::with_cell(inner, Arc::clone(&force_at)), force_at)
    }

    /// Wrap `inner` around an existing request cell (from [`force_cell`]).
    #[must_use]
    pub fn with_cell(inner: Box<dyn VideoEncodeWorker>, force_at: Arc<AtomicU64>) -> Self {
        Self { inner, force_at }
    }
}

/// A fresh, unset forced-IDR request cell.
#[must_use]
pub fn force_cell() -> Arc<AtomicU64> {
    Arc::new(AtomicU64::new(NO_FORCE))
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
                    // The forced IDR belongs to the first ENCODED tick >= its
                    // tick, fill ticks included: a dropped offer for the
                    // forced tick must not push the keyframe past the rebind.
                    if take_force(&self.force_at, tick) {
                        self.inner.force_keyframe();
                    }
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
    /// Per output: the format a routed source/PVW is composed at for it
    /// (#342; `None` or a missing entry = the canvas with the default fit).
    /// Program/Clean ignore it: their key's format is the output's base key
    /// (its settings index), converted on the key's worker by the opener.
    pub formats: Vec<Option<AuxFormat>>,
    /// The start-up take retry policy (#329).
    pub boot_retry: BootRetryPolicy,
}

/// How a refused start-up (boot) take is retried: re-issued at most once per
/// `retry_secs`, for at most `window_secs` after the run's first tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BootRetryPolicy {
    /// Minimum seconds between two issues of one output's boot take.
    pub retry_secs: i64,
    /// The window after the first tick; then the output stays on Program.
    pub window_secs: i64,
}

impl Default for BootRetryPolicy {
    fn default() -> Self {
        Self {
            retry_secs: BOOT_RETRY_SECS,
            window_secs: BOOT_WINDOW_SECS,
        }
    }
}

/// One picture feed: a bus at an output format (`None` = the canvas).
pub type FeedKey = (PictureBus, Option<AuxFormat>);

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
    /// Clock ticks handed to the aux thread.
    pub sent: Arc<AtomicU64>,
    /// Source stores.
    pub stores: StoreLookup,
    /// The PVW source id.
    pub preview_selection: Option<Arc<SwapCell<Option<Arc<str>>>>>,
}

/// What the opener receives: the rendition settings index (which already
/// names (format, codec settings)) plus the picture bus, so a Program/Clean
/// key at a non-canvas format wraps its encoder in a `FormatConvert`.
pub struct OpenRequest {
    id: u64,
    /// The rendition settings index (= the output's base key).
    pub settings: usize,
    /// The bus the encoder is fed.
    pub bus: PictureBus,
}

struct OpenResult {
    id: u64,
    encoder: Result<Box<dyn LiveEncoder>, String>,
}

enum Reap {
    /// A worker whose input is already closed: it finishes on its own.
    Worker(ClosingWorker),
    Encoder(Box<dyn LiveEncoder>),
}

struct DynamicKey {
    bus: PictureBus,
    format: Option<AuxFormat>,
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
    /// Set by the worker's event sink when it had to drop an event (the
    /// consumer's queue was full): the key's window stream has a hole, so
    /// the key fails alone at the next drain.
    lost: Arc<AtomicBool>,
    /// Router-unique id of this key's worker. Every event the worker sends is
    /// tagged with it, so a late event of a closed worker whose dispatcher
    /// slot was reused is dropped and counted, never applied to the new key.
    generation: u64,
    /// Consecutive ticks this key was offered a frame with no event from its
    /// worker since; reset by every event. `WEDGED_ENCODER_TICKS` = wedged.
    silent: u32,
    /// Consecutive offers to this key's worker refused (`Overloaded`); reset
    /// by an accepted offer. Counted per key, never per bus: a healthy
    /// sibling key on the same bus must not hide a starved one.
    /// `DEGRADED_AFTER_TICKS` marks its outputs degraded;
    /// `FALLBACK_AFTER_TICKS` closes the key (its outputs fall back).
    starved: u64,
    /// A substitution offer was refused this tick: the surviving tick's
    /// accepted offer must not reset `starved` (a key that refuses every
    /// substitution and accepts every survivor is still starved).
    refused_this_tick: bool,
    /// Whether the last published snapshot showed this key degraded.
    reported_degraded: bool,
}

struct PendingOpen {
    bus: PictureBus,
    format: Option<AuxFormat>,
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
    /// An audio-only take's crossfade is scheduled; refused at `deadline`.
    AudioMoving { deadline: u64 },
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

/// A boot take being carried through its retry window (#329): one per output
/// at most, so the state is O(outputs).
struct BootRetry {
    take: OutputTake,
    /// The tick it was last issued (entered the take state machine).
    issued: u64,
    /// When refused: the tick it may be re-issued.
    due: Option<u64>,
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
    dyn_tx: SyncSender<(u64, DispatchEvent)>,
    dyn_rx: Receiver<(u64, DispatchEvent)>,
    next_generation: u64,
    feeds: HashMap<FeedKey, AuxFeed>,
    /// Whether a Clean feed is active (cached so `publish_clean` costs one
    /// bool load when nothing selects Clean).
    clean_active: bool,
    /// The last published `live_keys` (recomputed only while keys exist).
    live_keys: Vec<(String, usize)>,
    health: HashMap<FeedKey, BusHealth>,
    last_forced: HashMap<usize, u64>,
    dropped_outcomes: u64,
    reaper_overflow: u64,
    /// Live keys closed because their worker ended or wedged.
    dead_encoders: u64,
    /// Events dropped by live-opened workers' sinks (shared with them).
    dyn_dropped: Arc<AtomicU64>,
    parameter_set_changes: u64,
    move_cancel_failures: u64,
    video_keys: usize,
    /// Items handed to the reaper this run (0 = nothing to wait for at the end).
    reaped: u64,
    dirty: bool,
    reaper: Option<std::thread::JoinHandle<()>>,
    /// Routed outputs' audio (`audio = follow`); `None` for a run without
    /// program audio.
    audio: Option<crate::routing_audio::RoutedAudio>,
    #[cfg(test)]
    audio_tap: crate::routing_audio::AudioTapCell,
    /// Start-up (boot) takes (#329).
    boot: BootTakes,
    /// The run's first tick (the boot retry window's origin).
    first_tick: Option<u64>,
}

/// The router's start-up (boot) take state, O(outputs).
#[derive(Default)]
struct BootTakes {
    /// Per output: its boot take while inside the retry window.
    slots: Vec<Option<BootRetry>>,
    /// Boot takes given up (refused for good, or the window ended).
    given_up: u64,
    /// The audio of given-up combined boot takes, re-issued alone on the
    /// next tick (at most one per output).
    audio_after: Vec<OutputTake>,
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
        Self::new_with_bus(cfg, aux, Box::new(move |settings, _bus| open(settings)))
    }

    /// [`Self::new`] with an opener that also learns the fed bus (#342).
    ///
    /// # Errors
    /// The OS error if a helper thread cannot be spawned.
    pub fn new_with_bus(
        cfg: RouterConfig,
        aux: AuxWiring,
        mut open: BusOpenFn,
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
                        open(req.settings, &req.bus)
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
        #[cfg(test)]
        let audio_tap = crate::routing_audio::AudioTapCell::default();
        let handle = OutputRoutingHandle {
            #[cfg(test)]
            audio_tap: Arc::clone(&audio_tap),
            takes: take_tx,
            snapshot: Arc::clone(&snapshot),
            outcomes: Arc::new(Mutex::new(Some(outcome_rx))),
            outputs: Arc::from(Vec::new()),
            has_preview: cfg.has_preview,
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
            next_generation: 0,
            feeds: HashMap::new(),
            clean_active: false,
            live_keys: Vec::new(),
            health: HashMap::new(),
            last_forced: HashMap::new(),
            dropped_outcomes: 0,
            reaper_overflow: 0,
            dead_encoders: 0,
            dyn_dropped: Arc::new(AtomicU64::new(0)),
            parameter_set_changes: 0,
            move_cancel_failures: 0,
            video_keys: 0,
            reaped: 0,
            dirty: true,
            reaper: Some(reaper),
            audio: None,
            #[cfg(test)]
            audio_tap,
            boot: BootTakes::default(),
            first_tick: None,
        };
        router.publish_snapshot();
        Ok((router, handle))
    }

    /// Install the routed-audio side (a run with program audio): private
    /// buses open with `config` (the program encoder's settings) and must
    /// produce `program_config` (its `AudioSpecificConfig`) byte for byte.
    pub(crate) fn enable_audio(
        &mut self,
        config: multiview_output::AudioEncodeConfig,
        program_config: Option<Vec<u8>>,
        route_gain: crate::routing_audio::RouteGain,
    ) {
        self.audio = Some(crate::routing_audio::RoutedAudio::new(
            self.outputs.len(),
            config,
            self.cfg.cadence,
            program_config,
            route_gain,
            self.aux.preview_selection.clone(),
        ));
        #[cfg(test)]
        if let Some(audio) = self.audio.as_mut() {
            audio.tap = Some(Arc::clone(&self.audio_tap));
        }
    }

    /// Test seam: inject the private-bus encode thread's per-job hook.
    #[cfg(test)]
    pub(crate) fn set_audio_worker_hook(&mut self, hook: crate::routing_audio::WorkerHook) {
        if let Some(audio) = self.audio.as_mut() {
            audio.worker_hook = Some(hook);
        }
    }

    /// Test observation: the block handed to the shared program encoder.
    #[cfg(test)]
    pub(crate) fn tap_program_audio(
        &self,
        tick: u64,
        program: &multiview_audio::format::AudioBlock,
    ) {
        if let Some(audio) = self.audio.as_ref() {
            audio.tap_program(tick, program);
        }
    }

    /// Whether any output has a private audio bus (no take = `false`, and
    /// the consumer then does no routed-audio work at all).
    pub(crate) fn has_private_audio(&self) -> bool {
        self.audio.as_ref().is_some_and(|a| a.private_buses() > 0)
    }

    /// Feed this tick's program block and per-source clean blocks to every
    /// private audio bus (bake consumer, right after the program bus ticked).
    /// Never blocks. A run without a take does nothing here.
    pub(crate) fn feed_audio(
        &mut self,
        tick: u64,
        program: &multiview_audio::format::AudioBlock,
        inputs: &[(&str, &multiview_audio::format::AudioBlock)],
        dispatcher: &mut RenditionDispatcher,
    ) {
        if let Some(audio) = self.audio.as_mut() {
            let before = (audio.private_buses(), audio.counters());
            audio.feed(tick, program, inputs, dispatcher);
            if (audio.private_buses(), audio.counters()) != before {
                self.dirty = true;
            }
        }
    }

    /// Publish this tick's Clean picture: the Program canvas BEFORE the
    /// overlay bake. Call on the bake consumer before [`Self::on_tick`] for
    /// the same tick, so a Clean encoder is fed frame `tick` that tick. Does
    /// nothing (one bool load) unless some take or encoder uses Clean; never
    /// copies the picture (one `Arc` clone into a wait-free slot).
    pub(crate) fn publish_clean(&mut self, tick: u64, canvas: &Arc<Nv12Image>) {
        if !self.clean_active {
            return;
        }
        for (key, feed) in &self.feeds {
            if key.0 != PictureBus::Clean {
                continue;
            }
            feed.slot
                .latest
                .store(Some(Arc::new(crate::routing_aux::AuxFrame {
                    tick,
                    generation: 0,
                    frame: Arc::clone(canvas),
                })));
            feed.slot.composed.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// The output's picture format (`None` = the canvas).
    fn format_of(&self, index: usize) -> Option<AuxFormat> {
        self.cfg.formats.get(index).copied().flatten()
    }

    /// The feed an output at `index` reads for `bus`: Program and Clean are
    /// the canvas picture (a non-canvas format is converted on the key's
    /// worker), a source/PVW is composed at the output's format.
    fn feed_key(&self, index: usize, bus: &PictureBus) -> FeedKey {
        let format = if aux_composed(bus) {
            self.format_of(index)
        } else {
            None
        };
        (bus.clone(), format)
    }

    /// Substitute every tick in `ticks` on the Clean bus with its own last
    /// pre-bake frame, exactly like Program's base-worker substitution:
    /// call on the bake consumer BEFORE [`Self::publish_clean`]/[`Self::on_tick`]
    /// for the surviving tick, with the SAME range the base-worker
    /// substitution loop computes, when a consumer stall sheds ticks from
    /// the hot queue. Without this, a Clean live encoder would see only
    /// surviving ticks and rely on its own `GapFill` (bounded by
    /// [`MAX_GAP_FILL_TICKS`]) to absorb a consumer gap — which, combined
    /// with any dropped offers, can exceed that bound and fail the key.
    ///
    /// For each Clean dynamic key with a last-fed canvas, offers
    /// `BakedTick { tick, canvas: last_fed }` (non-blocking, like
    /// [`Self::feed`]) for every tick in `ticks`, in increasing order, and
    /// stops at the first refused offer (counted as starvation; the rest of
    /// the gap is filled by the key's own `GapFill`, whose bound covers a
    /// full substitution gap plus a full queue). `feed()` then offers only
    /// the surviving tick. Does nothing (one bool check) unless Clean is
    /// active. Never blocks.
    pub(crate) fn substitute_clean(&mut self, ticks: std::ops::Range<u64>) {
        if !self.clean_active {
            return;
        }
        for d in &mut self.dynamic {
            if d.bus != PictureBus::Clean {
                continue;
            }
            let Some(worker) = d.worker.as_ref() else {
                continue;
            };
            let Some(canvas) = d.last_fed.clone() else {
                continue;
            };
            // Stop at the first refused offer: the ticks after it stay
            // visible as a gap the key's own `GapFill` fills (bounded) once
            // its queue drains, and the refusal counts toward starvation.
            for tick in ticks.clone() {
                match worker.offer(BakedTick {
                    tick,
                    canvas: Arc::clone(&canvas),
                }) {
                    Ok(()) => {}
                    Err(OfferRejection::Overloaded) => {
                        d.starved = d.starved.saturating_add(1);
                        d.refused_this_tick = true;
                        break;
                    }
                    // `feed` closes an ended worker on this same tick.
                    Err(OfferRejection::Ended) => break,
                }
            }
        }
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
        let keys = dispatcher.video_key_count();
        if keys != self.video_keys {
            self.video_keys = keys;
            self.dirty = true;
        }
        self.first_tick.get_or_insert(tick);
        self.drain_takes(tick, dispatcher);
        self.retry_boot_takes(tick);
        self.issue_boot_audio_after(tick, dispatcher);
        self.drain_opens(tick, dispatcher);
        self.expire_orphan_opens(tick);
        self.drain_moves(tick, dispatcher);
        self.advance_takes(tick, pts, dispatcher);
        self.feed(tick, dispatcher);
        self.check_aux(tick, dispatcher);
        self.linger(tick, dispatcher);
        self.sync_feeds();
        self.sync_live_keys(dispatcher);
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
        // A key whose sink dropped an event has a hole: it fails alone (its
        // subscribers fall back to Program through the dispatcher).
        let lost: Vec<usize> = self
            .dynamic
            .iter()
            .filter(|d| d.lost.load(Ordering::Acquire))
            .map(|d| d.key)
            .collect();
        for key in lost {
            self.close(key, dispatcher);
        }
        if let Some(audio) = self.audio.as_mut() {
            let before = (audio.private_buses(), audio.counters());
            audio.drain(dispatcher, delivery)?;
            // A bus the dispatcher failed (stall, watchdog) closed here: the
            // readback must say so.
            if (audio.private_buses(), audio.counters()) != before {
                self.dirty = true;
            }
        }
        while let Ok((generation, event)) = self.dyn_rx.try_recv() {
            // Accept only an event whose generation AND key index both name
            // the same live key: a generation alone could match another live
            // key while the event's key index names a reused slot.
            let key_index = match &event {
                DispatchEvent::Video { key_index, .. }
                | DispatchEvent::FinishVideo { key_index, .. } => Some(*key_index),
                DispatchEvent::Audio { .. } | DispatchEvent::FinishAudio { .. } => None,
            };
            let Some(d) = self
                .dynamic
                .iter_mut()
                .find(|d| d.generation == generation && Some(d.key) == key_index)
            else {
                // A closed worker's late event (its slot may be reused), or an
                // event whose key index does not match its worker's key.
                self.dyn_dropped.fetch_add(1, Ordering::Relaxed);
                continue;
            };
            if matches!(event, DispatchEvent::Video { .. }) {
                d.seen_event = true;
            }
            d.silent = 0;
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
        if is_boot_video_take(&operation_id) {
            self.on_boot_outcome(&operation_id, output_index, tick, outcome);
        }
        // Outcomes are rare control events (not per tick); the clone is the
        // output's current bus name at most.
        let bus = self
            .outputs
            .get(output_index)
            .map_or(PictureBus::Program, |o| o.bus.clone());
        let audio = self.audio.as_ref().map(|a| a.choice(output_index));
        if self
            .outcomes
            .try_send(TakeOutcome {
                operation_id,
                output_index,
                tick,
                outcome,
                bus,
                audio,
            })
            .is_err()
        {
            self.dropped_outcomes = self.dropped_outcomes.saturating_add(1);
        }
    }

    /// Ticks in `secs` seconds at the Program cadence (rounded up, at least 1).
    fn ticks_for(&self, secs: i64) -> u64 {
        let (num, den) = (self.cfg.cadence.num, self.cfg.cadence.den);
        if num <= 0 || den <= 0 {
            return 1;
        }
        let ticks = secs.saturating_mul(num).saturating_add(den - 1) / den;
        u64::try_from(ticks).unwrap_or(1).max(1)
    }

    /// A boot take's outcome: applied or superseded ends it; a refusal the
    /// gate may lift later (not fresh, capacity, encoder not ready) schedules
    /// a re-issue, no sooner than [`BOOT_RETRY_SECS`] after the last one; any
    /// other refusal gives up. The persisted feed is never touched here: the
    /// operator's intent stays, the readback shows the live bus.
    fn on_boot_outcome(
        &mut self,
        operation_id: &str,
        index: usize,
        tick: u64,
        outcome: TakeResult,
    ) {
        let retry_ticks = self.ticks_for(self.cfg.boot_retry.retry_secs);
        let Some(slot) = self.boot.slots.get_mut(index).filter(|slot| {
            slot.as_ref()
                .is_some_and(|b| b.take.operation_id == operation_id)
        }) else {
            return;
        };
        match outcome {
            TakeResult::Refused(
                RefuseReason::FeedNotFresh | RefuseReason::Capacity | RefuseReason::EncoderNotReady,
            ) => {
                if let Some(boot) = slot.as_mut() {
                    boot.due = Some(tick.max(boot.issued.saturating_add(retry_ticks)));
                }
            }
            TakeResult::Refused(reason) => {
                if let Some(boot) = slot.take() {
                    self.boot.given_up = self.boot.given_up.saturating_add(1);
                    self.boot_audio_alone(&boot.take);
                    tracing::warn!(
                        output = index,
                        feed = %boot.take.bus.as_ref().map_or_else(|| "-".into(), PictureBus::as_str),
                        reason = reason.operator_message(),
                        "routing: the start-up take cannot be carried; the output stays on Program"
                    );
                }
            }
            TakeResult::Applied | TakeResult::Superseded | TakeResult::Fallback(_) => {
                *slot = None;
            }
        }
    }

    /// A given-up boot take that also carried a persisted audio feed (a
    /// combined take): its audio is still applied, alone (audio-only takes
    /// have no fresh gate), so the video's refusal never drops it. Keyed on
    /// the OUTPUT INDEX (PR #352 round 2), never by re-parsing the give-up
    /// take's `operation_id`: a config id can be arbitrarily long (an HLS
    /// config id is its path), so the audio id is minted fresh from the
    /// index, always well under `MAX_TAKE_ID_BYTES`.
    fn boot_audio_alone(&mut self, take: &OutputTake) {
        let Some(audio) = take.audio.clone() else {
            return;
        };
        if self.boot.audio_after.len() >= self.outputs.len().max(1) {
            return;
        }
        self.boot.audio_after.push(OutputTake {
            operation_id: format!("{BOOT_AUDIO_TAKE_PREFIX}{}", take.output_index),
            output_index: take.output_index,
            bus: None,
            audio: Some(audio),
            principal: take.principal.clone(),
        });
    }

    /// Start the audio-only takes [`Self::boot_audio_alone`] queued, on an
    /// output with no take in flight (an operator take wins).
    fn issue_boot_audio_after(&mut self, tick: u64, dispatcher: &mut RenditionDispatcher) {
        for take in std::mem::take(&mut self.boot.audio_after) {
            let index = take.output_index;
            let Some(out) = self.outputs.get_mut(index) else {
                continue;
            };
            if out.pending.is_some() {
                continue;
            }
            out.pending = Some(Pending {
                take,
                started: tick,
                phase: Phase::Priming,
            });
            self.dirty = true;
            self.start_audio_take(index, tick, dispatcher);
        }
    }

    /// Re-issue due boot takes inside the window; give up after it. Bounded:
    /// one check per output, only while a boot take is still unresolved.
    fn retry_boot_takes(&mut self, tick: u64) {
        let window = self.ticks_for(self.cfg.boot_retry.window_secs);
        let first = self.first_tick.unwrap_or(tick);
        for index in 0..self.boot.slots.len() {
            let Some(due) = self
                .boot
                .slots
                .get(index)
                .and_then(|b| b.as_ref())
                .and_then(|b| b.due)
            else {
                continue;
            };
            if tick < due || self.outputs.get(index).is_none_or(|o| o.pending.is_some()) {
                continue;
            }
            if tick.saturating_sub(first) > window {
                if let Some(boot) = self.boot.slots.get_mut(index).and_then(Option::take) {
                    self.boot.given_up = self.boot.given_up.saturating_add(1);
                    self.boot_audio_alone(&boot.take);
                    self.dirty = true;
                    tracing::warn!(
                        output = index,
                        feed = %boot.take.bus.as_ref().map_or_else(|| "-".into(), PictureBus::as_str),
                        window_secs = self.cfg.boot_retry.window_secs,
                        "routing: the start-up feed never became takeable; the output stays on \
                         Program (the configured feed is kept)"
                    );
                }
                continue;
            }
            let Some(boot) = self.boot.slots.get_mut(index).and_then(Option::as_mut) else {
                continue;
            };
            boot.issued = tick;
            boot.due = None;
            let take = boot.take.clone();
            if let Some(out) = self.outputs.get_mut(index) {
                out.pending = Some(Pending {
                    take,
                    started: tick,
                    phase: Phase::Priming,
                });
                self.dirty = true;
            }
        }
    }

    fn drain_takes(&mut self, tick: u64, dispatcher: &mut RenditionDispatcher) {
        loop {
            let take = match self.takes.try_recv() {
                Ok(take) => take,
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => return,
            };
            let index = take.output_index;
            // A boot take enters its retry window; an operator take on the
            // same output replaces the boot intent for good. One slot per
            // output, allocated on the first take (bounded by the outputs).
            if self.boot.slots.len() < self.outputs.len() {
                self.boot.slots.resize_with(self.outputs.len(), || None);
            }
            // Any later take on the output replaces a queued boot audio too.
            self.boot.audio_after.retain(|b| b.output_index != index);
            if let Some(slot) = self.boot.slots.get_mut(index) {
                *slot = is_boot_video_take(&take.operation_id).then(|| BootRetry {
                    take: take.clone(),
                    issued: tick,
                    due: None,
                });
            }
            let needs_switcher = matches!(take.bus, Some(PictureBus::Preview | PictureBus::Clean));
            // Audio rides the output's program-audio slot (§5.5): a run
            // without program audio, or an output without that slot (NDI,
            // AES67, display), cannot take an audio choice.
            let audio_ok = take.audio.is_none()
                || (self.audio.is_some() && dispatcher.has_program_audio(index));
            let routable = index < self.outputs.len()
                && (!needs_switcher || self.cfg.has_preview)
                && (take.bus.is_some() || take.audio.is_some())
                && audio_ok;
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
            let audio_only = take.bus.is_none();
            let Some(out) = self.outputs.get_mut(index) else {
                continue;
            };
            out.pending = Some(Pending {
                take,
                started: tick,
                phase: Phase::Priming,
            });
            self.dirty = true;
            if audio_only {
                // Adopted at this drain tick: no fresh gate for audio (a
                // source without audio is silence, §5.1).
                self.start_audio_take(index, tick, dispatcher);
            }
        }
    }

    /// Start the pending take's audio change on the output's current video
    /// bus (an audio-only take, or a combined take whose video is already
    /// on air).
    fn start_audio_take(&mut self, index: usize, tick: u64, dispatcher: &mut RenditionDispatcher) {
        let Some((choice, bus)) = self.outputs.get(index).and_then(|o| {
            o.pending
                .as_ref()
                .and_then(|p| p.take.audio.clone())
                .map(|c| (c, o.bus.clone()))
        }) else {
            return;
        };
        let started = self.audio.as_mut().map_or(AudioTakeStart::Refused, |a| {
            a.take_audio(index, choice, &bus, tick, dispatcher)
        });
        match started {
            AudioTakeStart::Unchanged => {
                if let Some(p) = self.outputs.get_mut(index).and_then(|o| o.pending.take()) {
                    self.emit(p.take.operation_id, index, tick, TakeResult::Applied);
                }
            }
            AudioTakeStart::Refused => {
                self.refuse_pending(index, tick, RefuseReason::EncoderNotReady, dispatcher);
            }
            AudioTakeStart::Started => {
                if let Some(p) = self.outputs.get_mut(index).and_then(|o| o.pending.as_mut()) {
                    p.phase = Phase::AudioMoving {
                        deadline: tick.saturating_add(TAKE_PRIME_DEADLINE_TICKS),
                    };
                }
            }
        }
    }

    /// The pending take ended without applying: undo its audio (restore the
    /// choice; audio already scheduled fades back).
    fn abandon_audio(
        &mut self,
        index: usize,
        phase: Phase,
        tick: u64,
        dispatcher: &mut RenditionDispatcher,
    ) {
        let bus = self
            .outputs
            .get(index)
            .map_or(PictureBus::Program, |o| o.bus.clone());
        if let Some(audio) = self.audio.as_mut() {
            if matches!(phase, Phase::AudioMoving { .. }) {
                audio.finish_take(index, false, &bus, tick, dispatcher);
            } else {
                audio.video_refused(index, tick, dispatcher);
            }
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
        self.abandon_audio(index, old.phase, tick, dispatcher);
        if let Phase::Opening(id) = old.phase {
            if let Some(open) = self.opens.get_mut(&id) {
                open.waiter = None;
            }
        }
        self.emit(old.take.operation_id, index, tick, TakeResult::Superseded);
    }

    /// [`Self::supersede`] only a take whose dispatcher move is in flight
    /// (`Moving`): a fallback replaces that move. A take still priming,
    /// opening or warming stays valid from the output's new feed.
    fn supersede_move(&mut self, index: usize, tick: u64, dispatcher: &mut RenditionDispatcher) {
        let moving = self
            .outputs
            .get(index)
            .and_then(|o| o.pending.as_ref())
            .is_some_and(|p| matches!(p.phase, Phase::Moving(_)));
        if moving {
            self.supersede(index, tick, dispatcher);
        }
    }

    /// Cancel the output's pending dispatcher move by re-asserting its
    /// current key. When that key is already failed/retired (its fallback is
    /// on the way), the rebind is refused and the old move would stay
    /// pending and could land later with no take (#330 item 4): cancel it
    /// directly instead, and count it.
    fn cancel_dispatcher_move(
        &mut self,
        index: usize,
        dispatcher: &mut RenditionDispatcher,
        tick: u64,
    ) {
        let Some(key) = self.outputs.get(index).map(|o| o.key) else {
            return;
        };
        if let Err(error) = dispatcher.rebind_video(index, key, tick, 1) {
            self.move_cancel_failures = self.move_cancel_failures.saturating_add(1);
            self.dirty = true;
            let cancelled = dispatcher.cancel_video_move(index);
            tracing::warn!(
                output = index,
                key,
                ?error,
                cancelled,
                "routing: could not re-assert the current key; pending move cancelled directly"
            );
        }
    }

    /// The picture bus a video key carries (`None`: unknown, e.g. closed).
    fn bus_of_key(&self, key: usize) -> Option<PictureBus> {
        if self.cfg.base_keys.contains(&key) {
            return Some(PictureBus::Program);
        }
        self.dynamic
            .iter()
            .find(|d| d.key == key)
            .map(|d| d.bus.clone())
    }

    /// A pending take whose dispatcher move vanished without an Applied or
    /// Refused (a newer rebind replaced it): settle it as Superseded so it
    /// never strands (#330 item 3, defensive).
    fn settle_stranded(&mut self, index: usize, tick: u64, dispatcher: &mut RenditionDispatcher) {
        let stranded = self
            .outputs
            .get(index)
            .and_then(|o| o.pending.as_ref())
            .and_then(|p| match p.phase {
                Phase::Moving(k) => Some(k),
                _ => None,
            })
            .is_some_and(|k| dispatcher.pending_video_move(index) != Some(k));
        if !stranded {
            return;
        }
        if let Some(old) = self.outputs.get_mut(index).and_then(|o| o.pending.take()) {
            self.abandon_audio(index, old.phase, tick, dispatcher);
            self.emit(old.take.operation_id, index, tick, TakeResult::Superseded);
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
            let refuse = |router: &mut Self, dispatcher: &mut RenditionDispatcher| {
                if let Some(index) = waiter {
                    router.refuse_pending(index, tick, RefuseReason::EncoderNotReady, dispatcher);
                }
            };
            let Ok(mut encoder) = result.encoder else {
                refuse(self, dispatcher);
                continue;
            };
            let tb = encoder.time_base();
            // Register the key only while the take still waits for exactly
            // this open within its deadline; otherwise the encoder never
            // takes a dispatcher slot.
            let waiting = waiter.filter(|&index| {
                self.outputs
                    .get(index)
                    .and_then(|o| o.pending.as_ref())
                    .is_some_and(|p| p.phase == Phase::Opening(result.id))
            });
            let Some(index) = waiting.filter(|_| tick < open.deadline) else {
                self.reap(Reap::Encoder(encoder));
                if waiting.is_some() {
                    refuse(self, dispatcher);
                }
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
                refuse(self, dispatcher);
                continue;
            };
            if let Some(params) = encoder.parameter_sets() {
                let _ = dispatcher.set_parameter_sets(key, params);
            }
            let force_at = Arc::new(AtomicU64::new(NO_FORCE));
            let worker = GapFill {
                inner: encoder,
                force_at: Arc::clone(&force_at),
                next: None,
                last: None,
            };
            let lost = Arc::new(AtomicBool::new(false));
            let generation = self.next_generation;
            self.next_generation = self.next_generation.wrapping_add(1);
            let spawned =
                self.spawn_live_worker(key, generation, tb, Box::new(worker), Arc::clone(&lost));
            let Ok(worker) = spawned else {
                let _ = dispatcher.retire_video_key(key);
                refuse(self, dispatcher);
                continue;
            };
            self.last_forced.insert(key, tick);
            self.dynamic.push(DynamicKey {
                bus: open.bus,
                format: open.format,
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
                lost,
                generation,
                silent: 0,
                starved: 0,
                refused_this_tick: false,
                reported_degraded: false,
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

    /// An open whose take was superseded (no waiter) and whose result never
    /// arrived releases its capacity at its deadline, like a waited one
    /// (#330: otherwise a wedged open leaked an encoder session forever).
    /// A result arriving later goes to the reaper (`drain_opens`).
    fn expire_orphan_opens(&mut self, tick: u64) {
        let before = self.opens.len();
        self.opens
            .retain(|_, o| o.waiter.is_some() || tick < o.deadline);
        if self.opens.len() != before {
            self.dirty = true;
        }
    }

    /// Spawn a live-opened worker whose event sink never blocks: a full
    /// consumer queue drops the event, counts it and marks the key `lost`
    /// (every later event of that worker is dropped too, so the dispatcher
    /// never sees a stream with a hole); a closed queue ends the worker.
    fn spawn_live_worker(
        &self,
        key: usize,
        generation: u64,
        tb: Rational,
        worker: Box<dyn VideoEncodeWorker>,
        lost: Arc<AtomicBool>,
    ) -> Result<RenditionWorker, PipelineError> {
        let tx = self.dyn_tx.clone();
        let dropped = Arc::clone(&self.dyn_dropped);
        RenditionWorker::spawn(key, self.cfg.cadence, tb, worker, move |event| {
            if lost.load(Ordering::Acquire) {
                dropped.fetch_add(1, Ordering::Relaxed);
                return Ok(());
            }
            match tx.try_send((generation, event)) {
                Ok(()) => Ok(()),
                Err(TrySendError::Full(_)) => {
                    lost.store(true, Ordering::Release);
                    dropped.fetch_add(1, Ordering::Relaxed);
                    Ok(())
                }
                Err(TrySendError::Disconnected(_)) => Err(PipelineError::Output {
                    kind: "routing-worker",
                    reason: "routing consumer ended".to_owned(),
                }),
            }
        })
    }

    fn refuse_pending(
        &mut self,
        index: usize,
        tick: u64,
        reason: RefuseReason,
        dispatcher: &mut RenditionDispatcher,
    ) {
        let phase = self
            .outputs
            .get(index)
            .and_then(|o| o.pending.as_ref())
            .map(|p| p.phase);
        if let Some(phase) = phase {
            self.abandon_audio(index, phase, tick, dispatcher);
        }
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
        let mut superseded: Vec<usize> = Vec::new();
        for outcome in dispatcher.take_move_outcomes() {
            match outcome {
                MoveOutcome::Applied {
                    output_index,
                    key,
                    tick: at,
                    parameter_sets_changed,
                } => {
                    if parameter_sets_changed {
                        self.parameter_set_changes = self.parameter_set_changes.saturating_add(1);
                        self.dirty = true;
                    }
                    let Some(out) = self.outputs.get_mut(output_index) else {
                        continue;
                    };
                    let expected = matches!(out.pending.as_ref().map(|p| p.phase), Some(Phase::Moving(k)) if k == key);
                    if expected {
                        if let Some(p) = out.pending.take() {
                            if let Some(bus) = p.take.bus.clone() {
                                out.bus = bus;
                            }
                            out.key = key;
                            out.since = at;
                            if let Some(audio) = self.audio.as_mut() {
                                audio.video_applied(output_index);
                            }
                            self.emit(p.take.operation_id, output_index, at, TakeResult::Applied);
                        }
                    } else if out.key != key {
                        // A fallback (or a stale, uncancelled) move landing
                        // with no take pending for it: the bus follows the
                        // key, so `key` and `bus` never disagree (#330 item
                        // 4). An unknown key (already closed) is left to its
                        // own Fallback.
                        if let Some(bus) = self.bus_of_key(key) {
                            if let Some(out) = self.outputs.get_mut(output_index) {
                                out.bus = bus;
                                out.key = key;
                                out.since = at;
                            }
                            self.dirty = true;
                        }
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
                        let reason = match reason {
                            MoveRefusal::FormatMismatch => RefuseReason::FormatMismatch,
                            MoveRefusal::EncoderNotReady | MoveRefusal::NoGate => {
                                RefuseReason::EncoderNotReady
                            }
                        };
                        self.refuse_pending(output_index, tick, reason, dispatcher);
                    }
                }
                MoveOutcome::Superseded { output_index } => superseded.push(output_index),
                MoveOutcome::AudioFallback { .. } => {}
                MoveOutcome::AudioApplied { .. } | MoveOutcome::AudioRefused { .. } => {
                    if let Some(audio) = self.audio.as_mut() {
                        audio.on_outcome(&outcome, dispatcher);
                    }
                    self.dirty = true;
                }
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
                    // A take whose dispatcher move is in flight is settled
                    // (Superseded, its audio restored) and the move cancelled,
                    // before the audio falls back (#330 item 3).
                    self.supersede_move(output_index, tick, dispatcher);
                    if let Some(audio) = self.audio.as_mut() {
                        audio.fallback(output_index, &PictureBus::Program, tick, None, dispatcher);
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
        // Decided after the whole batch: an Applied/Refused later in it has
        // already settled the take.
        for index in superseded {
            self.settle_stranded(index, tick, dispatcher);
        }
    }

    /// Lag rule and aux failure: move every output on a held bus to Program.
    /// The aux thread failing moves only the buses it composes: Clean is fed
    /// by the bake consumer itself and stays.
    fn check_aux(&mut self, tick: u64, dispatcher: &mut RenditionDispatcher) {
        let failed = self.aux.failed.load(Ordering::Acquire);
        let fallen: Vec<FeedKey> = self
            .health
            .iter_mut()
            .filter(|((bus, _), h)| {
                let aux_failed = failed && aux_composed(bus);
                !h.fallen && (aux_failed || h.held >= FALLBACK_AFTER_TICKS)
            })
            .map(|(bus, h)| {
                h.fallen = true;
                bus.clone()
            })
            .collect();
        for (bus, format) in fallen {
            for index in 0..self.outputs.len() {
                let on_bus = self.outputs.get(index).is_some_and(|o| o.bus == bus)
                    && self.feed_key(index, &bus).1 == format;
                if !on_bus {
                    continue;
                }
                let Some(&base) = self.cfg.base_keys.get(index) else {
                    continue;
                };
                // A take whose dispatcher move is in flight is settled first
                // (Superseded, audio restored): the forced rebind replaces
                // that move, which would otherwise strand it forever (#330
                // item 3).
                self.supersede_move(index, tick, dispatcher);
                let windows = self.request_idr(base, tick);
                // A fallback: the routed feed is gone, so it is never refused
                // for a format mismatch (an out-of-band output takes a loss).
                if dispatcher
                    .rebind_video_forced(index, base, tick, windows)
                    .is_ok()
                {
                    if let Some(out) = self.outputs.get_mut(index) {
                        out.bus = PictureBus::Program;
                    }
                    if let Some(audio) = self.audio.as_mut() {
                        audio.fallback(index, &PictureBus::Program, tick, Some(base), dispatcher);
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

    fn slot_fresh(&self, feed: &FeedKey, tick: u64) -> bool {
        self.feeds
            .get(feed)
            .and_then(|f| f.slot.latest.load())
            .is_some_and(|frame| frame.tick.saturating_add(1) >= tick)
    }

    fn feed_fresh(&self, feed: &FeedKey, tick: u64, pts: MediaTime) -> bool {
        let bus = &feed.0;
        let source: Option<Arc<str>> = match bus {
            // Program and Clean are always fresh (§4.5): both come from the
            // bake consumer's own Program canvas.
            PictureBus::Program | PictureBus::Clean => return true,
            PictureBus::Source(id) => Some(Arc::from(id.as_str())),
            PictureBus::Preview => self.aux.preview_selection.as_ref().and_then(|s| s.load()),
        };
        let live = source
            .and_then(|id| (self.aux.stores)(&id))
            .is_some_and(|store: Arc<TileStore<Nv12Image>>| store.state_at(pts).is_live());
        live && !self.aux.failed.load(Ordering::Acquire) && self.slot_fresh(feed, tick)
    }

    fn advance_takes(&mut self, tick: u64, pts: MediaTime, dispatcher: &mut RenditionDispatcher) {
        for index in 0..self.outputs.len() {
            let Some((bus, choice, started, phase, principal)) =
                self.outputs.get(index).and_then(|o| {
                    o.pending.as_ref().map(|p| {
                        (
                            p.take.bus.clone(),
                            p.take.audio.clone(),
                            p.started,
                            p.phase,
                            p.take.principal.clone(),
                        )
                    })
                })
            else {
                continue;
            };
            if let Phase::AudioMoving { deadline } = phase {
                self.advance_audio_take(index, tick, deadline, dispatcher);
                continue;
            }
            let Some(bus) = bus else {
                continue;
            };
            match phase {
                Phase::Moving(_) | Phase::AudioMoving { .. } => {}
                Phase::Warming { key, deadline } => {
                    let seen = self
                        .dynamic
                        .iter()
                        .find(|d| d.key == key)
                        .map(|d| d.seen_event);
                    if tick >= deadline || seen.is_none() || dispatcher.is_key_failed(key) {
                        self.refuse_pending(index, tick, RefuseReason::EncoderNotReady, dispatcher);
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
                            if let Some(audio) = self.audio.as_mut() {
                                audio.follow(index, &bus, choice, at, Some(key), dispatcher);
                            }
                        } else {
                            self.refuse_pending(
                                index,
                                tick,
                                RefuseReason::EncoderNotReady,
                                dispatcher,
                            );
                        }
                    }
                }
                Phase::Opening(id) => {
                    let late = self.opens.get(&id).is_none_or(|o| tick >= o.deadline);
                    if late {
                        // The result, if it ever comes, goes to the reaper.
                        self.opens.remove(&id);
                        self.refuse_pending(index, tick, RefuseReason::EncoderNotReady, dispatcher);
                    }
                }
                Phase::Priming => {
                    let current = self.outputs.get(index).map(|o| o.bus.clone());
                    if current.as_ref() == Some(&bus) {
                        self.cancel_dispatcher_move(index, dispatcher, tick);
                        if choice.is_some() {
                            // The video is already on air: the audio alone.
                            self.start_audio_take(index, tick, dispatcher);
                        } else if let Some(p) =
                            self.outputs.get_mut(index).and_then(|o| o.pending.take())
                        {
                            self.emit(p.take.operation_id, index, tick, TakeResult::Applied);
                        }
                        continue;
                    }
                    let feed = self.feed_key(index, &bus);
                    if !self.feed_fresh(&feed, tick, pts) {
                        if tick.saturating_sub(started) >= TAKE_PRIME_DEADLINE_TICKS {
                            self.refuse_pending(
                                index,
                                tick,
                                RefuseReason::FeedNotFresh,
                                dispatcher,
                            );
                        }
                        continue;
                    }
                    if self.wait_for_shared_open(index, &bus, started, tick, dispatcher) {
                        continue;
                    }
                    self.start_move(index, &bus, choice.as_ref(), &principal, tick, dispatcher);
                }
            }
        }
    }

    /// Encode once per (bus, settings) (§4.3): while an open for the same
    /// key is in flight, the take waits for it and then reuses its encoder
    /// (#330: two takes opened two encoders). Returns `true` while waiting;
    /// the wait is bounded by [`OPEN_DEADLINE_TICKS`].
    fn wait_for_shared_open(
        &mut self,
        index: usize,
        bus: &PictureBus,
        started: u64,
        tick: u64,
        dispatcher: &mut RenditionDispatcher,
    ) -> bool {
        let settings = self.cfg.base_keys.get(index).copied();
        let in_flight = self
            .opens
            .values()
            .any(|o| o.bus == *bus && Some(o.settings) == settings);
        if in_flight && tick.saturating_sub(started) >= OPEN_DEADLINE_TICKS {
            self.refuse_pending(index, tick, RefuseReason::EncoderNotReady, dispatcher);
        }
        in_flight
    }

    /// An audio-only take in flight: applied once on air (the tick whose
    /// block holds `G`), refused when its bus fails or at `deadline`.
    fn advance_audio_take(
        &mut self,
        index: usize,
        tick: u64,
        deadline: u64,
        dispatcher: &mut RenditionDispatcher,
    ) {
        let progress = self
            .audio
            .as_ref()
            .map_or(AudioTakeProgress::Failed, |a| a.take_progress(index));
        match progress {
            AudioTakeProgress::Done(g) => {
                let bus = self
                    .outputs
                    .get(index)
                    .map_or(PictureBus::Program, |o| o.bus.clone());
                let at = self.audio.as_mut().map_or(tick, |a| {
                    a.finish_take(index, true, &bus, tick, dispatcher);
                    a.tick_of_sample(g)
                });
                if let Some(p) = self.outputs.get_mut(index).and_then(|o| o.pending.take()) {
                    self.emit(p.take.operation_id, index, at, TakeResult::Applied);
                }
            }
            AudioTakeProgress::Failed => {
                self.refuse_pending(index, tick, RefuseReason::EncoderNotReady, dispatcher);
            }
            AudioTakeProgress::Waiting => {
                if tick >= deadline {
                    self.refuse_pending(index, tick, RefuseReason::EncoderNotReady, dispatcher);
                }
            }
        }
    }

    fn start_move(
        &mut self,
        index: usize,
        bus: &PictureBus,
        choice: Option<&AudioChoice>,
        principal: &str,
        tick: u64,
        dispatcher: &mut RenditionDispatcher,
    ) {
        let Some(&settings) = self.cfg.base_keys.get(index) else {
            return;
        };
        // The settings index already names (format, codec settings): the
        // base key of the output. Its feed is (bus, format) for a routed
        // source/PVW, the canvas picture for Clean.
        let (_, format) = self.feed_key(index, bus);
        // Take admission: open the output's private audio bus before the
        // video move (§5.2), so it is warm at the grid boundary G.
        if let Some(audio) = self.audio.as_mut() {
            audio.admit(index, bus, choice, tick, dispatcher);
        }
        let existing = if *bus == PictureBus::Program {
            Some(settings)
        } else {
            self.dynamic
                .iter()
                .find(|d| {
                    d.bus == *bus
                        && d.format == format
                        && d.settings == settings
                        && !dispatcher.is_key_failed(d.key)
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
            // Another output (not this one) already subscribes the key.
            let held_by_other = self.outputs.iter().enumerate().any(|(i, o)| {
                i != index
                    && (o.key == key
                        || matches!(
                            o.pending.as_ref().map(|p| p.phase),
                            Some(Phase::Moving(k) | Phase::Warming { key: k, .. }) if k == key
                        ))
            });
            if dispatcher.rebind_video(index, key, tick, windows).is_ok() {
                set(self, Phase::Moving(key));
                if let Some(audio) = self.audio.as_mut() {
                    audio.follow(index, bus, choice.cloned(), tick, Some(key), dispatcher);
                }
                // A re-take of a lingering key adopts it (re-attributed,
                // linger cleared). A key another output still subscribes
                // stays with its holder: the newcomer rides it at +0 and is
                // never able to move the holder's attribution or cap.
                if let Some(d) = self.dynamic.iter_mut().find(|d| d.key == key) {
                    if !held_by_other {
                        principal.clone_into(&mut d.principal);
                    }
                    d.linger_deadline = None;
                }
            } else {
                self.refuse_pending(index, tick, RefuseReason::EncoderNotReady, dispatcher);
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
            self.refuse_pending(index, tick, RefuseReason::Capacity, dispatcher);
            return;
        }
        let id = self.next_open;
        self.next_open = self.next_open.wrapping_add(1);
        match self.open_tx.try_send(OpenRequest {
            id,
            settings,
            bus: bus.clone(),
        }) {
            Ok(()) => {
                self.opens.insert(
                    id,
                    PendingOpen {
                        bus: bus.clone(),
                        format,
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
                self.refuse_pending(index, tick, RefuseReason::Capacity, dispatcher);
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
    /// the last one when none is new), counting held ticks per bus (picture
    /// freshness) and refused offers per KEY (`DynamicKey::starved`): a live
    /// encoder that never drains its input queue degrades and its key is
    /// closed (outputs fall back) even though its picture bus (Clean
    /// included) is published fresh every tick by the bake consumer.
    fn feed(&mut self, tick: u64, dispatcher: &mut RenditionDispatcher) {
        let mut fresh_buses: Vec<(FeedKey, bool)> = Vec::new();
        let mut dead: Vec<usize> = Vec::new();
        for d in &mut self.dynamic {
            let feed_key = (d.bus.clone(), d.format);
            let latest = self.feeds.get(&feed_key).and_then(|f| f.slot.latest.load());
            let new = latest
                .as_ref()
                .filter(|f| d.last_aux_tick.is_none_or(|t| f.tick > t));
            let fresh = new.is_some();
            if let Some(frame) = new {
                d.last_aux_tick = Some(frame.tick);
                d.last_fed = Some(Arc::clone(&frame.frame));
            }
            let (Some(canvas), Some(worker)) = (d.last_fed.clone(), d.worker.as_ref()) else {
                // No canvas to offer yet: nothing was dropped, so this is
                // not (yet) a held tick for the bus beyond slot freshness.
                fresh_buses.push((feed_key, fresh));
                continue;
            };
            // Never blocks: a full queue drops and the worker gap-fills.
            let offer_result = worker.offer(BakedTick { tick, canvas });
            let ended = matches!(offer_result, Err(OfferRejection::Ended));
            // The bus's health is its picture's freshness; this KEY's
            // encoder refusing frames is counted on the key itself, so a
            // healthy sibling key on the same bus cannot reset it.
            let refused_before = std::mem::take(&mut d.refused_this_tick);
            match offer_result {
                Ok(()) if !refused_before => d.starved = 0,
                Err(OfferRejection::Overloaded) => d.starved = d.starved.saturating_add(1),
                Ok(()) | Err(OfferRejection::Ended) => {}
            }
            // Wherever `starved` moved (here or in `substitute_clean`), a
            // change of the degraded state republishes the snapshot.
            if d.reported_degraded != (d.starved >= DEGRADED_AFTER_TICKS) {
                d.reported_degraded = !d.reported_degraded;
                self.dirty = true;
            }
            fresh_buses.push((feed_key, fresh));
            d.silent = d.silent.saturating_add(1);
            // A panicked/exited worker, or one that emits nothing while fed
            // (wedged), would freeze its outputs forever: close the key so
            // they fall back to Program (Fallback(EncoderFailed)).
            // A key starved for FALLBACK_AFTER_TICKS (it never drains its
            // queue) is closed the same way.
            if ended
                || worker.is_finished()
                || d.silent >= WEDGED_ENCODER_TICKS
                || d.starved >= FALLBACK_AFTER_TICKS
            {
                dead.push(d.key);
            }
        }
        for key in dead {
            self.dead_encoders = self.dead_encoders.saturating_add(1);
            self.close(key, dispatcher);
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

    /// Retire the key, close its worker's input and hand the closing worker
    /// to the reaper. Never joins here; the input is closed BEFORE the
    /// handoff, so a worker the reaper cannot take (queue full, shutdown
    /// grace over) still leaves its loop, finishes and ends on its own.
    fn close(&mut self, key: usize, dispatcher: &mut RenditionDispatcher) {
        let _ = dispatcher.retire_video_key(key);
        if let Some(pos) = self.dynamic.iter().position(|d| d.key == key) {
            let mut d = self.dynamic.swap_remove(pos);
            if let Some(worker) = d.worker.take() {
                self.reap(Reap::Worker(worker.close_input()));
            }
            self.last_forced.remove(&key);
            self.dirty = true;
        }
    }

    fn reap(&mut self, item: Reap) {
        self.reaped = self.reaped.saturating_add(1);
        if self.reap_tx.try_send(item).is_err() {
            // Dropped unjoined: the input closes, the thread ends on its own.
            self.reaper_overflow = self.reaper_overflow.saturating_add(1);
        }
    }

    /// Keep the feed set equal to the buses in use.
    ///
    /// Bounded (#342 review A2): every feed is wanted by a live key, an open
    /// or a pending take, so the set holds at most `max_encoders` (live keys
    /// plus opens, which `has_capacity` caps) plus one per output with a
    /// pending take, whatever the formats. There is no separate
    /// `routing.max_aux_canvases` setting to enforce. The aux thread is handed
    /// (and the clock's send gate raised for) only the buses it composes
    /// (Preview, sources): routing only Clean never wakes it.
    fn sync_feeds(&mut self) {
        let mut wanted: Vec<FeedKey> = self
            .dynamic
            .iter()
            .map(|d| (d.bus.clone(), d.format))
            .collect();
        wanted.extend(self.opens.values().map(|o| (o.bus.clone(), o.format)));
        let pending: Vec<FeedKey> = self
            .outputs
            .iter()
            .enumerate()
            .filter_map(|(i, o)| {
                o.pending
                    .as_ref()
                    .and_then(|p| p.take.bus.as_ref())
                    .map(|bus| self.feed_key(i, bus))
            })
            .collect();
        wanted.extend(pending);
        wanted.retain(|(b, _)| *b != PictureBus::Program);
        wanted.sort_unstable_by_key(feed_order);
        wanted.dedup();
        let unchanged = wanted.iter().all(|b| self.feeds.contains_key(b))
            && self.feeds.keys().all(|b| wanted.contains(b));
        if unchanged {
            return;
        }
        self.feeds.retain(|b, _| wanted.contains(b));
        self.health.retain(|b, _| wanted.contains(b));
        for key in wanted {
            let (bus, format) = key.clone();
            self.feeds
                .entry(key)
                .or_insert_with(|| AuxFeed::with_format(bus, format));
        }
        self.clean_active = self.feeds.keys().any(|(b, _)| *b == PictureBus::Clean);
        let list: Vec<AuxFeed> = self
            .feeds
            .values()
            .filter(|f| aux_composed(&f.bus))
            .cloned()
            .collect();
        let active = !list.is_empty();
        self.aux.feeds.store(Arc::new(list));
        self.aux.active.store(active, Ordering::Release);
        self.dirty = true;
    }

    /// Recompute `live_keys` (only while live keys exist or were published).
    fn sync_live_keys(&mut self, dispatcher: &RenditionDispatcher) {
        if self.dynamic.is_empty() && self.live_keys.is_empty() {
            return;
        }
        let mut keys: Vec<(String, usize)> = self
            .dynamic
            .iter()
            .filter(|d| !dispatcher.is_key_failed(d.key))
            .map(|d| (crate::routing_aux::feed_name(&d.bus, d.format), d.settings))
            .collect();
        keys.sort_unstable();
        keys.dedup();
        if keys != self.live_keys {
            self.live_keys = keys;
            self.dirty = true;
        }
    }

    fn publish_snapshot(&mut self) {
        self.publish_snapshot_at(0);
    }

    fn publish_snapshot_at(&mut self, tick: u64) {
        let outputs = self
            .outputs
            .iter()
            .enumerate()
            .map(|(i, o)| OutputRouteView {
                bus: o.bus.as_str().into_owned(),
                key: format!("{}/{}", o.bus.as_str(), o.key),
                since_tick: o.since,
                pending: o.pending.as_ref().map(|p| PendingView {
                    bus: p.take.bus.as_ref().unwrap_or(&o.bus).as_str().into_owned(),
                    audio: p.take.audio.as_ref().map(AudioChoice::label),
                    held_ticks: tick.saturating_sub(p.started),
                }),
                degraded: self
                    .health
                    .get(&(
                        o.bus.clone(),
                        if aux_composed(&o.bus) {
                            self.cfg.formats.get(i).copied().flatten()
                        } else {
                            None
                        },
                    ))
                    .is_some_and(|h| h.held >= DEGRADED_AFTER_TICKS)
                    || self
                        .dynamic
                        .iter()
                        .any(|d| d.key == o.key && d.starved >= DEGRADED_AFTER_TICKS),
                audio: self
                    .audio
                    .as_ref()
                    .map_or_else(|| "program".to_owned(), |a| a.carried_label(i)),
                audio_choice: self
                    .audio
                    .as_ref()
                    .map_or_else(|| "follow".to_owned(), |a| a.choice(i).label()),
                private_audio: self.audio.as_ref().is_some_and(|a| a.has_private_bus(i)),
            })
            .collect();
        let next = RoutingSnapshot {
            outputs,
            dropped_aux_ticks: self.aux.dropped.load(Ordering::Relaxed),
            dynamic_encoders_live: self.dynamic.len(),
            opens_in_flight: self.opens.len(),
            dropped_outcomes: self.dropped_outcomes,
            reaper_overflow: self.reaper_overflow,
            dead_encoders: self.dead_encoders,
            dropped_live_events: self.dyn_dropped.load(Ordering::Relaxed),
            parameter_set_changes: self.parameter_set_changes,
            video_keys: self.video_keys,
            aux_ticks_sent: self.aux.sent.load(Ordering::Relaxed),
            audio_buses_live: self
                .audio
                .as_ref()
                .map_or(0, crate::routing_audio::RoutedAudio::private_buses),
            audio: self
                .audio
                .as_ref()
                .map(crate::routing_audio::RoutedAudio::counters)
                .unwrap_or_default(),
            live_keys: self.live_keys.clone(),
            boot_takes_pending: self.boot.slots.iter().filter(|b| b.is_some()).count(),
            boot_takes_given_up: self.boot.given_up,
            move_cancel_failures: self.move_cancel_failures,
        };
        if *self.snapshot.load() != next {
            self.snapshot.store(Arc::new(next));
        }
        self.dirty = false;
    }

    /// The picture-bus slot of an active canvas-format feed (tests and
    /// diagnostics).
    #[must_use]
    pub fn slot(&self, bus: &PictureBus) -> Option<Arc<PictureBusSlot>> {
        self.slot_at(bus, None)
    }

    /// The slot of the active feed of `bus` at `format` (#342).
    #[must_use]
    pub fn slot_at(
        &self,
        bus: &PictureBus,
        format: Option<AuxFormat>,
    ) -> Option<Arc<PictureBusSlot>> {
        self.feeds
            .get(&(bus.clone(), format))
            .map(|f| Arc::clone(&f.slot))
    }

    /// Active feeds (bus, format), for tests and diagnostics.
    #[must_use]
    pub fn feed_keys(&self) -> Vec<FeedKey> {
        let mut keys: Vec<FeedKey> = self.feeds.keys().cloned().collect();
        keys.sort_unstable_by_key(feed_order);
        keys
    }

    /// Live dynamic keys as (bus, format, settings, dispatcher key).
    #[must_use]
    pub fn dynamic_keys(&self) -> Vec<(PictureBus, Option<AuxFormat>, usize, usize)> {
        self.dynamic
            .iter()
            .map(|d| (d.bus.clone(), d.format, d.settings, d.key))
            .collect()
    }

    /// End of run: retire every live key and hand its worker to the reaper,
    /// then let the reaper finish. Joins only the reaper, bounded by `bound`,
    /// and only when something was handed to it: a run without a live open
    /// returns at once (its end-of-run timing is unchanged).
    pub(crate) fn shutdown(
        mut self,
        dispatcher: &mut RenditionDispatcher,
        bound: std::time::Duration,
    ) {
        let keys: Vec<usize> = self.dynamic.iter().map(|d| d.key).collect();
        for key in keys {
            self.close(key, dispatcher);
        }
        if let Some(audio) = self.audio.as_mut() {
            audio.shutdown(dispatcher);
        }
        self.aux.active.store(false, Ordering::Release);
        // Drain what the closing workers already queued (bounded): their keys
        // are retired, so the events are only counted, never dispatched.
        for _ in 0..DYNAMIC_EVENT_QUEUE.saturating_mul(2) {
            if self.dyn_rx.try_recv().is_err() {
                break;
            }
            self.dyn_dropped.fetch_add(1, Ordering::Relaxed);
        }
        self.video_keys = dispatcher.video_key_count();
        self.live_keys.clear();
        self.publish_snapshot();
        let reaper = self.reaper.take();
        let handed_over = self.reaped;
        drop(self);
        if handed_over == 0 {
            return;
        }
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

/// A total order over feed keys (dedup and stable listings).
fn feed_order(key: &FeedKey) -> (String, Option<(u32, u32, String)>) {
    (
        key.0.as_str().into_owned(),
        key.1.map(|f| {
            (
                f.width,
                f.height,
                format!("{:?}/{:?}", f.fit.mode, f.fit.anchor),
            )
        }),
    )
}

/// Whether the `routing-aux` thread composes `bus` (Program and Clean come
/// from the bake consumer).
fn aux_composed(bus: &PictureBus) -> bool {
    matches!(bus, PictureBus::Preview | PictureBus::Source(_))
}

#[cfg(test)]
#[path = "output_routing_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "output_routing_pipeline_tests.rs"]
mod pipeline_tests;
