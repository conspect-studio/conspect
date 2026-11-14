//! Downstream keyer (DSK) runtime on the Program drive (ADR-0113, design note
//! `docs/architecture/switcher-keyers.md` §2-§4 and §9, slice K2a).
//!
//! The [`SwitcherTickDriver`](crate::switcher_control::SwitcherTickDriver)
//! owns one [`KeyerRuntime`] on the output clock. Everything in it is fixed
//! size and allocated at attach: at most [`MAX_DSK`] slots, each with a
//! `Copy` [`KeyerFlags`], a two-`Arc` last-good pair and two optional
//! operation ids that are *moved* in from the command, never cloned per tick.
//! Per tick the runtime:
//!
//! 1. resolves held commands and completes ramps ([`KeyerRuntime::tick_pending`]),
//! 2. applies keyer verbs from the shared mailbox (`command`), and
//! 3. after the background has been staged, resolves the gate, the loss
//!    policy, the last-good pair, the weight and the tally into a fixed
//!    `[Option<KeyerDraw>; MAX_DSK]` the frame hook hands to the drive
//!    ([`KeyerRuntime::resolve`]).
//!
//! Store reads are the drive's own wait-free `read_at` (one per source per
//! keyer that is on, ramping or riding a tied MIX). A keyer that is off reads
//! nothing. Events are queued in a fixed array and published only on a
//! committed frame through a bounded drop-oldest [`EventStream`] (invariant
//! #10); a rejected frame restores the `Copy` flags staged at the start of the
//! tick and refuses the operations that moved on it.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use multiview_compositor::key::{KeyKind, KeyMask};
use multiview_compositor::pipeline::Nv12Image;
use multiview_config::{DownstreamKeyerConfig, KeyerLossPolicy, KeyerLossTrigger, KeyerType};
use multiview_core::switcher::{kernel, FrameCount, Progress};
use multiview_core::traits::SourceState;
use multiview_framestore::{TileRead, TileStore};

use crate::clock::Tick;
use crate::isolation::EventStream;
use crate::switcher_control::{SwitcherOutcome, SwitcherVerb, TAKE_PRIME_DEADLINE_TICKS};

/// Downstream keyers per program (ADR-0113 §9 hard limit).
pub const MAX_DSK: usize = 2;
/// Keyer events queued per tick before a commit; more are counted as dropped.
pub const MAX_QUEUED_KEYER_EVENTS: usize = 8;
/// Entries of one tally set (design note §9: room for 6 keyers × 2 + 2 buses).
pub const TALLY_SLOTS: usize = 14;

/// Typed refusal: a keyer source had no fresh frame before the deadline.
pub const REFUSE_KEYER_NO_FRESH_FRAME: &str =
    "keyer source has no fresh frame before the bounded take deadline";
/// Typed refusal: a second command for a keyer while one is held.
pub const REFUSE_KEYER_PENDING: &str =
    "keyer-pending: a command for this keyer is already held waiting for a fresh fill and key";
/// Typed refusal: the keyer index names no defined keyer.
pub const REFUSE_KEYER_UNKNOWN: &str =
    "unknown-keyer: no downstream keyer is defined at this index";
/// Typed refusal: a keyer AUTO while that keyer is already ramping.
pub const REFUSE_KEYER_RAMPING: &str =
    "keyer-ramping: the keyer is in an AUTO; wait for it to finish or CUT it";
/// Typed refusal: a keyer AUTO was interrupted by a keyer CUT.
pub const REFUSE_KEYER_AUTO_INTERRUPTED: &str = "keyer AUTO interrupted by a keyer CUT";
/// Typed refusal: a keyer source reached its loss trigger during an AUTO.
pub const REFUSE_KEYER_SOURCE_LOST: &str =
    "source-lost: a keyer source reached its loss trigger; the keyer was taken off air";
/// Typed refusal: a held keyer command expired while Program repeated.
pub const REFUSE_KEYER_REPEAT_DEADLINE: &str =
    "keyer take deadline expired while program repeated last-good";

/// Slack when comparing a summed cost with the budget (shared with config).
const BUDGET_EPSILON: f64 = 1e-9;

/// The key parameters of one keyer, `Copy` so a placement carries them.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct KeyerParams {
    /// Matte derivation.
    pub kind: KeyKind,
    /// Invert the matte.
    pub invert: bool,
    /// Mask in fill-source pixels.
    pub mask: Option<KeyMask>,
}

/// A destination rectangle in canvas pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyerFly {
    /// Left column.
    pub x: u32,
    /// Top row.
    pub y: u32,
    /// Width.
    pub w: u32,
    /// Height.
    pub h: u32,
}

/// What a keyer does at its loss trigger (ADR-R011 §3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LossPolicy {
    /// Take the keyer off air; it stays off until an operator acts.
    DropToOffAir,
    /// Keep drawing the frozen pair and report `stale`.
    HoldLastGood,
}

/// The source state at or past which the loss policy fires.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LossTrigger {
    /// Stale or worse.
    Stale,
    /// Reconnecting or worse (the default).
    Reconnecting,
    /// No signal.
    NoSignal,
}

impl LossTrigger {
    const fn rank(self) -> u8 {
        match self {
            Self::Stale => 1,
            Self::Reconnecting => 2,
            Self::NoSignal => 3,
        }
    }
}

/// A local order on source states (`SourceState` is `non_exhaustive` and has
/// no `Ord`): `Live` < `Stale` < `Reconnecting` < `NoSignal`; anything unknown ranks
/// as no signal.
fn rank(state: SourceState) -> u8 {
    if state == SourceState::Live {
        0
    } else if state == SourceState::Stale {
        1
    } else if state == SourceState::Reconnecting {
        2
    } else {
        3
    }
}

/// One defined downstream keyer, resolved at attach: the stores are held, so
/// a tick never looks a source up by id.
#[derive(Debug, Clone)]
pub struct KeyerDef {
    /// Authorizable object id, `<program>/<id>` (e.g. `main/dsk1`).
    pub object_id: Arc<str>,
    /// Fill source id (for the control plane's tally resolution and demand).
    pub fill_id: Arc<str>,
    /// Key source id, if any.
    pub key_id: Option<Arc<str>>,
    /// The fill's frame store.
    pub fill: Arc<TileStore<Nv12Image>>,
    /// The key's frame store; `None` is a self key.
    pub key: Option<Arc<TileStore<Nv12Image>>>,
    /// Key parameters.
    pub params: KeyerParams,
    /// Destination rectangle; `None` is the full canvas.
    pub fly: Option<KeyerFly>,
    /// Admitted source envelope `(w, h)`; a larger frame is a non-live read.
    pub max_source: (u32, u32),
    /// AUTO rate.
    pub rate: FrameCount,
    /// Tied at definition.
    pub tie: bool,
    /// Goes on at boot through the fresh gate.
    pub on_at_start: bool,
    /// Draw order among DSKs (ascending).
    pub z: i32,
    /// Loss policy.
    pub loss: LossPolicy,
    /// Loss trigger.
    pub trigger: LossTrigger,
    /// Admission cost in canvas folds (ADR-0113 §9).
    pub cost: f64,
}

impl KeyerDef {
    /// Resolve a validated config entry against the run's stores.
    ///
    /// # Errors
    /// A fill or key with no frame store in this run, a refused-by-name
    /// feature (`chroma`, `premultiplied`) or a zero rate. A validated
    /// document only fails on a missing store.
    pub fn from_config<S: std::hash::BuildHasher>(
        cfg: &DownstreamKeyerConfig,
        stores: &std::collections::HashMap<String, Arc<TileStore<Nv12Image>>, S>,
        canvas_w: u32,
        canvas_h: u32,
    ) -> Result<Self, String> {
        let name = format!("{}/{}", cfg.program, cfg.id);
        let kind = match cfg.key_type {
            KeyerType::Linear => KeyKind::Linear,
            KeyerType::Luma => KeyKind::Luma {
                clip: cfg.clip,
                gain: cfg.gain,
            },
            _ => {
                return Err(format!(
                    "downstream keyer {name}: this key type is not supported"
                ))
            }
        };
        if cfg.premultiplied {
            return Err(format!(
                "downstream keyer {name}: premultiplied fills are not supported"
            ));
        }
        let store = |role: &str, id: &str| {
            stores.get(id).cloned().ok_or_else(|| {
                format!(
                    "downstream keyer {name}: {role} source {id:?} has no frame store in this run"
                )
            })
        };
        let fill = store("fill", &cfg.fill)?;
        let key = match &cfg.key {
            Some(id) => Some(store("key", id)?),
            None => None,
        };
        let rate = FrameCount::try_from(cfg.rate)
            .map_err(|_| format!("downstream keyer {name}: rate must be at least one frame"))?;
        let source = cfg.max_source_or_canvas(canvas_w, canvas_h);
        Ok(Self {
            object_id: Arc::from(name.as_str()),
            fill_id: Arc::from(cfg.fill.as_str()),
            key_id: cfg.key.as_deref().map(Arc::from),
            fill,
            key,
            params: KeyerParams {
                kind,
                invert: cfg.invert,
                mask: cfg.mask.map(|m| KeyMask {
                    x: m.x,
                    y: m.y,
                    w: m.w,
                    h: m.h,
                }),
            },
            fly: cfg.fly.map(|f| KeyerFly {
                x: f.x,
                y: f.y,
                w: f.w,
                h: f.h,
            }),
            max_source: (source.w, source.h),
            rate,
            tie: cfg.tie,
            on_at_start: cfg.on_at_start,
            z: cfg.z,
            loss: match cfg.loss {
                KeyerLossPolicy::HoldLastGood => LossPolicy::HoldLastGood,
                _ => LossPolicy::DropToOffAir,
            },
            trigger: match cfg.loss_trigger {
                KeyerLossTrigger::Stale => LossTrigger::Stale,
                KeyerLossTrigger::NoSignal => LossTrigger::NoSignal,
                _ => LossTrigger::Reconnecting,
            },
            cost: cfg.cost(canvas_w, canvas_h),
        })
    }
}

/// Reserve the defined keyers against `budget` at definition (ADR-0113 §9)
/// and build their runtime. Called at boot in K2 (and by `keyer_set` in K4),
/// never on the clock.
///
/// # Errors
/// More than [`MAX_DSK`] keyers, a non-finite budget, or a summed cost over
/// the budget.
pub fn admit(
    defs: Vec<KeyerDef>,
    canvas_w: u32,
    canvas_h: u32,
    budget: f32,
) -> Result<KeyerRuntime, String> {
    if defs.len() > MAX_DSK {
        return Err(format!("a program has at most {MAX_DSK} downstream keyers"));
    }
    if !budget.is_finite() || budget < 0.0 {
        return Err(format!("keyer budget {budget} must be finite and >= 0"));
    }
    let total: f64 = defs.iter().map(|d| d.cost).sum();
    if total > f64::from(budget) + BUDGET_EPSILON {
        return Err(format!(
            "downstream keyers cost {total:.3} canvas folds, over the keyer budget {budget}"
        ));
    }
    let mut runtime = KeyerRuntime {
        canvas: (canvas_w, canvas_h),
        reserved_cost: total,
        budget,
        ..KeyerRuntime::default()
    };
    for (index, def) in defs.into_iter().enumerate() {
        let flags = KeyerFlags {
            tie: def.tie,
            pending: def.on_at_start.then_some(Pending {
                action: PendingAction::Cut,
                deadline: None,
                boot: true,
            }),
            ..KeyerFlags::default()
        };
        if let Some(slot) = runtime.slots.get_mut(index) {
            *slot = Some(KeyerSlot {
                def,
                flags,
                last_good: None,
                pending_op: None,
                ramp_op: None,
            });
        }
    }
    // Draw order: ascending z (validation made z unique per program).
    let mut order = [0_usize, 1];
    order.sort_by_key(|&i| {
        runtime
            .slots
            .get(i)
            .and_then(Option::as_ref)
            .map_or(i32::MAX, |s| s.def.z)
    });
    runtime.draw_order = order;
    Ok(runtime)
}

/// An in-flight keyer AUTO: weight `f(tick − start)` in integer frames.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ramp {
    /// Tick the ramp started on (weight 0 going on, 1 going off).
    pub start: u64,
    /// Duration in frames.
    pub rate: FrameCount,
    /// Direction.
    pub to_on: bool,
}

/// What a held command does once its sources are fresh.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PendingAction {
    /// Go on at once.
    Cut,
    /// Start an AUTO on at this rate.
    Auto(FrameCount),
}

/// A command held in the keyer's own pending slot (ADR-0113 §6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pending {
    /// What it does when fresh.
    pub action: PendingAction,
    /// Refusal tick; `None` for the boot `on_at_start` intent.
    pub deadline: Option<u64>,
    /// The op-less `on_at_start` intent (an operator command supersedes it).
    pub boot: bool,
}

/// The `Copy` state of one keyer; staged per tick for reject atomicity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct KeyerFlags {
    /// On air (weight 1 when idle).
    pub on_air: bool,
    /// Tied to the next background transition.
    pub tie: bool,
    /// The keyer's own AUTO, if one is in flight.
    pub ramp: Option<Ramp>,
    /// A held command.
    pub pending: Option<Pending>,
    /// `hold_last_good` is drawing a frozen pair past the trigger.
    pub stale: bool,
    /// Participation in the background AUTO in flight, fixed when it starts
    /// (design note §3.2): `Some(true)` going on, `Some(false)` going off.
    /// Only participants flip on the terminal tick; a source loss under
    /// `drop_to_off_air` or an operator keyer CUT/AUTO cancels it, so a
    /// recovery never re-keys the keyer without an operator command.
    pub flight: Option<bool>,
}

/// A keyer's `(fill, key)` last-good pair.
type Pair = (Arc<Nv12Image>, Option<Arc<Nv12Image>>);

#[derive(Debug)]
struct KeyerSlot {
    def: KeyerDef,
    flags: KeyerFlags,
    /// `(fill, key)` replaced only on a tick where both reads are live.
    last_good: Option<Pair>,
    pending_op: Option<String>,
    ramp_op: Option<String>,
}

impl KeyerSlot {
    fn release(&mut self) {
        self.last_good = None;
        self.flags.stale = false;
    }
}

/// One keyer's contribution to this tick's composite: two `Arc` clones and
/// `Copy` parameters, no heap.
#[derive(Debug, Clone)]
pub struct KeyerDraw {
    /// Keyer index (config order).
    pub keyer: u8,
    /// Fill picture.
    pub fill: Arc<Nv12Image>,
    /// Key picture, frozen together with the fill.
    pub key: Option<Arc<Nv12Image>>,
    /// Destination rectangle in canvas pixels.
    pub rect: KeyerFly,
    /// On-air weight in `[0, 1]` (the tile opacity).
    pub weight: f32,
    /// Key parameters.
    pub params: KeyerParams,
}

/// Why a keyer changed state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyerReason {
    /// An operator verb (CUT, AUTO terminal).
    Operator,
    /// A tied keyer followed a background take.
    Tie,
    /// The boot `on_at_start` intent.
    OnAtStart,
    /// A source reached the loss trigger.
    SourceLost,
    /// A stale keyer's sources are live again.
    SourceRecovered,
}

/// A keyer state change, published only on a committed frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyerEvent {
    /// Tick of the change.
    pub tick: u64,
    /// Keyer index (config order).
    pub keyer: u8,
    /// On air after the change.
    pub on_air: bool,
    /// Stale after the change.
    pub stale: bool,
    /// Why.
    pub reason: KeyerReason,
}

/// The snapshot form of an in-flight AUTO (clients compute the weight).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RampSnapshot {
    /// Start tick.
    pub start_tick: u64,
    /// Duration in frames.
    pub rate: u32,
    /// Direction.
    pub to_on: bool,
}

/// One keyer row of the switcher snapshot; changes on state changes only.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(clippy::struct_excessive_bools)]
// reason: four independent wire flags of one snapshot row (design note §8).
pub struct KeyerSnapshot {
    /// On air.
    pub on_air: bool,
    /// Tied.
    pub tie: bool,
    /// In-flight AUTO.
    pub ramp: Option<RampSnapshot>,
    /// Frozen past the trigger (`hold_last_good`).
    pub stale: bool,
    /// An operator command is held.
    pub pending: bool,
}

/// A role contributing to a bus (design note §9, published as roles: the
/// Program and Preview sources have no stable store index in the drive; the
/// control plane resolves roles to source ids off the clock).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TallyRole {
    /// The Program (background) source.
    Program,
    /// The Preview source.
    Preview,
    /// The fill of keyer `n`.
    Fill(u8),
    /// The key of keyer `n`.
    Key(u8),
}

/// A fixed-size tally set (no heap; `Copy`, `Eq` for change detection).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TallySet {
    roles: [Option<TallyRole>; TALLY_SLOTS],
}

impl TallySet {
    fn insert(&mut self, role: TallyRole) {
        if self.contains(role) {
            return;
        }
        if let Some(free) = self.roles.iter_mut().find(|r| r.is_none()) {
            *free = Some(role);
        }
    }

    /// Whether `role` is in the set.
    #[must_use]
    pub fn contains(&self, role: TallyRole) -> bool {
        self.roles.contains(&Some(role))
    }

    /// The roles, in insertion order.
    pub fn roles(&self) -> impl Iterator<Item = TallyRole> + '_ {
        self.roles.iter().flatten().copied()
    }
}

/// Engine tally from the composed tile list.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct KeyerTally {
    /// Contributing to Program this tick.
    pub program: TallySet,
    /// Preview plus the fill and key of tied keyers going on.
    pub preview: TallySet,
}

/// Keyer counters, readable off the clock.
#[derive(Debug, Default)]
pub struct KeyerCounters {
    oversize_frames: AtomicU64,
    events_dropped: AtomicU64,
    source_lost: AtomicU64,
}

impl KeyerCounters {
    /// Fill or key frames above `max_source`, treated as non-live.
    #[must_use]
    pub fn oversize_frames(&self) -> u64 {
        self.oversize_frames.load(Ordering::Relaxed)
    }

    /// Keyer events not queued because the per-tick queue was full.
    #[must_use]
    pub fn events_dropped(&self) -> u64 {
        self.events_dropped.load(Ordering::Relaxed)
    }

    /// Keyers taken off air by `drop_to_off_air`.
    #[must_use]
    pub fn source_lost(&self) -> u64 {
        self.source_lost.load(Ordering::Relaxed)
    }
}

/// A keyer verb, decoded from [`SwitcherVerb`] (engine-internal; K4 maps
/// keyer ids to indices).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyerAction {
    /// Flip at the next tick.
    Cut(bool),
    /// Ramp over `rate` (or the keyer's own rate).
    Auto(bool, Option<FrameCount>),
    /// Set the tie flag.
    Tie(bool),
}

impl KeyerAction {
    /// The keyer index and action of a keyer verb, `None` for any other verb.
    #[must_use]
    pub fn of(verb: &SwitcherVerb) -> Option<(u8, Self)> {
        match *verb {
            SwitcherVerb::KeyerCut { keyer, on } => Some((keyer, Self::Cut(on))),
            SwitcherVerb::KeyerAuto { keyer, on, rate } => Some((keyer, Self::Auto(on, rate))),
            SwitcherVerb::KeyerTie { keyer, tie } => Some((keyer, Self::Tie(tie))),
            _ => None,
        }
    }
}

/// Where keyer operation ids go: applied ops join the driver's staged
/// success list (confirmed or refused with the frame), refusals publish at
/// once.
pub(crate) struct OpSink<'a> {
    pub(crate) success: &'a mut Vec<String>,
    pub(crate) outcomes: &'a EventStream<SwitcherOutcome>,
    pub(crate) tick: u64,
}

impl OpSink<'_> {
    fn applied(&mut self, operation_id: String) {
        self.success.push(operation_id);
    }

    fn refused(&self, operation_id: String, reason: &str) {
        refuse(self.outcomes, operation_id, self.tick, reason);
    }
}

fn refuse(outcomes: &EventStream<SwitcherOutcome>, operation_id: String, tick: u64, reason: &str) {
    outcomes.publish(SwitcherOutcome {
        operation_id,
        tick,
        completed: false,
        error: Some(reason.to_owned()),
    });
}

/// A read's frame when it is fresh and inside the envelope.
fn fresh_fit(read: &TileRead<Nv12Image>, max: (u32, u32)) -> Option<&Arc<Nv12Image>> {
    let frame = read.frame()?;
    (read.state().is_live() && frame.width() <= max.0 && frame.height() <= max.1).then_some(frame)
}

/// Whether a read is fresh but oversize (counted, treated as non-live).
fn oversize(read: &TileRead<Nv12Image>, max: (u32, u32)) -> bool {
    read.state().is_live()
        && read
            .frame()
            .is_some_and(|f| f.width() > max.0 || f.height() > max.1)
}

/// The fresh gate: fill and key both live and inside the envelope now.
fn live_now(def: &KeyerDef, tick: Tick) -> bool {
    let fill = def.fill.read_at(tick.pts);
    if fresh_fit(&fill, def.max_source).is_none() {
        return false;
    }
    def.key.as_ref().is_none_or(|key| {
        let read = key.read_at(tick.pts);
        fresh_fit(&read, def.max_source).is_some()
    })
}

fn queue_event(
    events: &mut [Option<KeyerEvent>; MAX_QUEUED_KEYER_EVENTS],
    counters: &KeyerCounters,
    event: KeyerEvent,
) {
    match events.iter_mut().find(|e| e.is_none()) {
        Some(free) => *free = Some(event),
        None => {
            counters.events_dropped.fetch_add(1, Ordering::Relaxed);
        }
    }
}

fn event_of(tick: u64, keyer: u8, flags: KeyerFlags, reason: KeyerReason) -> KeyerEvent {
    KeyerEvent {
        tick,
        keyer,
        on_air: flags.on_air,
        stale: flags.stale,
        reason,
    }
}

/// A tied keyer's weights while a background AUTO is in flight (ADR-0113 §4,
/// design note §3.2): the weight a keyer going **on** draws, and the weight a
/// keyer going **off** draws. MIX: `p` and `1 − p`. DIP: the incoming
/// contribution `i` and `1 − d`, from `dip_weights`. `Copy`, per tick.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TieEnvelope {
    /// Weight of a tied keyer going on.
    pub going_on: f32,
    /// Weight of a tied keyer going off.
    pub going_off: f32,
}

impl TieEnvelope {
    /// The MIX envelope at progress `p`: `p` going on, `1 − p` going off.
    #[must_use]
    pub fn mix(p: f32) -> Self {
        Self {
            going_on: p,
            going_off: 1.0 - p,
        }
    }
}

/// The on-air weight for this tick (direction-aware for a tied AUTO).
fn weight_of(flags: KeyerFlags, tick: Tick, auto_p: Option<TieEnvelope>) -> f32 {
    if let Some(ramp) = flags.ramp {
        let elapsed = u32::try_from(tick.index.saturating_sub(ramp.start)).unwrap_or(u32::MAX);
        let p = kernel::opacity(Progress::new(elapsed, ramp.rate));
        return if ramp.to_on { p } else { 1.0 - p };
    }
    if let (Some(to_on), Some(envelope)) = (flags.flight, auto_p) {
        // The direction fixed when the background AUTO started, never
        // re-derived from `on_air` (review F1).
        return if to_on {
            envelope.going_on
        } else {
            envelope.going_off
        };
    }
    if flags.on_air {
        1.0
    } else {
        0.0
    }
}

fn index_u8(index: usize) -> u8 {
    u8::try_from(index).unwrap_or(u8::MAX)
}

/// The keyer state the switcher tick driver owns (fixed size, attach-time).
#[derive(Debug)]
pub struct KeyerRuntime {
    slots: [Option<KeyerSlot>; MAX_DSK],
    /// Slot indices in ascending z.
    draw_order: [usize; MAX_DSK],
    /// Flags at the start of this tick, restored by a rejected frame.
    staged: [KeyerFlags; MAX_DSK],
    /// Last-good pairs at the start of this tick (refcounts only), restored
    /// by a rejected frame (review F2).
    staged_pairs: [Option<Pair>; MAX_DSK],
    /// The tally at the start of this tick, restored by a rejected frame.
    staged_tally: KeyerTally,
    events: [Option<KeyerEvent>; MAX_QUEUED_KEYER_EVENTS],
    tally: KeyerTally,
    canvas: (u32, u32),
    counters: Arc<KeyerCounters>,
    reserved_cost: f64,
    budget: f32,
}

impl Default for KeyerRuntime {
    /// No keyers: every per-tick call is a no-op.
    fn default() -> Self {
        Self {
            slots: Default::default(),
            draw_order: [0, 1],
            staged: [KeyerFlags::default(); MAX_DSK],
            staged_pairs: [const { None }; MAX_DSK],
            staged_tally: KeyerTally::default(),
            events: [None; MAX_QUEUED_KEYER_EVENTS],
            tally: KeyerTally::default(),
            canvas: (0, 0),
            counters: Arc::new(KeyerCounters::default()),
            reserved_cost: 0.0,
            budget: 0.0,
        }
    }
}

impl KeyerRuntime {
    /// Whether no keyer is defined.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.slots.iter().all(Option::is_none)
    }

    /// The number of defined keyers.
    #[must_use]
    pub fn len(&self) -> usize {
        self.slots.iter().flatten().count()
    }

    /// The reserved cost in canvas folds and the budget it was admitted
    /// against.
    #[must_use]
    pub fn reservation(&self) -> (f64, f32) {
        (self.reserved_cost, self.budget)
    }

    /// The definition of keyer `index`, if any.
    #[must_use]
    pub fn def(&self, index: usize) -> Option<&KeyerDef> {
        self.slots.get(index)?.as_ref().map(|s| &s.def)
    }

    /// The current flags of keyer `index`, if any.
    #[must_use]
    pub fn flags(&self, index: usize) -> Option<KeyerFlags> {
        self.slots.get(index)?.as_ref().map(|s| s.flags)
    }

    /// The last-good pair of keyer `index` (for `Arc::ptr_eq` assertions).
    #[must_use]
    pub fn last_good(&self, index: usize) -> Option<Pair> {
        self.slots.get(index)?.as_ref()?.last_good.clone()
    }

    pub(crate) fn set_counters(&mut self, counters: Arc<KeyerCounters>) {
        self.counters = counters;
    }

    /// Stage the `Copy` flags, the last-good pairs (refcount increments, no
    /// allocation) and the tally a rejected frame restores. Once per tick.
    pub(crate) fn begin_tick(&mut self) {
        for ((staged, pair), slot) in self
            .staged
            .iter_mut()
            .zip(self.staged_pairs.iter_mut())
            .zip(self.slots.iter())
        {
            *staged = slot.as_ref().map(|s| s.flags).unwrap_or_default();
            *pair = slot.as_ref().and_then(|s| s.last_good.clone());
        }
        self.staged_tally = self.tally;
    }

    /// Complete ramps whose terminal tick is now and resolve held commands
    /// (fresh: apply; deadline: refuse). Runs before the mailbox drain.
    pub(crate) fn tick_pending(&mut self, tick: Tick, sink: &mut OpSink<'_>) {
        for (index, entry) in self.slots.iter_mut().enumerate() {
            let Some(slot) = entry.as_mut() else {
                continue;
            };
            let keyer = index_u8(index);
            if let Some(ramp) = slot.flags.ramp {
                if tick.index.saturating_sub(ramp.start) >= u64::from(ramp.rate.get()) {
                    slot.flags.ramp = None;
                    slot.flags.on_air = ramp.to_on;
                    if !ramp.to_on {
                        slot.release();
                    }
                    if let Some(op) = slot.ramp_op.take() {
                        sink.applied(op);
                    }
                    queue_event(
                        &mut self.events,
                        &self.counters,
                        event_of(tick.index, keyer, slot.flags, KeyerReason::Operator),
                    );
                }
            }
            let Some(pending) = slot.flags.pending else {
                continue;
            };
            if live_now(&slot.def, tick) {
                slot.flags.pending = None;
                // A held command that lands takes the keyer out of a tied
                // transition: the operator's verb wins.
                slot.flags.flight = None;
                let op = slot.pending_op.take();
                match pending.action {
                    PendingAction::Cut => {
                        slot.flags.on_air = true;
                        let reason = if pending.boot {
                            KeyerReason::OnAtStart
                        } else {
                            KeyerReason::Operator
                        };
                        queue_event(
                            &mut self.events,
                            &self.counters,
                            event_of(tick.index, keyer, slot.flags, reason),
                        );
                        if let Some(op) = op {
                            sink.applied(op);
                        }
                    }
                    PendingAction::Auto(rate) => {
                        slot.flags.ramp = Some(Ramp {
                            start: tick.index,
                            rate,
                            to_on: true,
                        });
                        slot.ramp_op = op;
                    }
                }
            } else if pending.deadline.is_some_and(|d| tick.index >= d) {
                slot.flags.pending = None;
                if let Some(op) = slot.pending_op.take() {
                    sink.refused(op, REFUSE_KEYER_NO_FRESH_FRAME);
                }
            }
        }
    }

    /// Apply one keyer verb at this tick. Never touches the background
    /// machine or the held background take (ADR-0113 §6).
    pub(crate) fn command(
        &mut self,
        keyer: u8,
        action: KeyerAction,
        operation_id: String,
        tick: Tick,
        sink: &mut OpSink<'_>,
    ) {
        let Some(slot) = self
            .slots
            .get_mut(usize::from(keyer))
            .and_then(Option::as_mut)
        else {
            sink.refused(operation_id, REFUSE_KEYER_UNKNOWN);
            return;
        };
        if slot.flags.pending.is_some() {
            if slot.pending_op.is_some() {
                sink.refused(operation_id, REFUSE_KEYER_PENDING);
                return;
            }
            // The op-less boot intent: the operator's command supersedes it.
            slot.flags.pending = None;
        }
        match action {
            KeyerAction::Tie(tie) => {
                slot.flags.tie = tie;
                if !tie && slot.flags.flight.take().is_some() && !slot.flags.on_air {
                    // Untied mid-flight: it keeps its state (off stays off).
                    slot.release();
                }
                sink.applied(operation_id);
            }
            KeyerAction::Cut(on) => {
                // An operator CUT takes the keyer out of a tied transition.
                slot.flags.flight = None;
                let interrupted = slot.flags.ramp.take().is_some();
                if let Some(old) = slot.ramp_op.take() {
                    sink.refused(old, REFUSE_KEYER_AUTO_INTERRUPTED);
                }
                let was = slot.flags.on_air;
                if !on {
                    // Going off is never gated.
                    slot.flags.on_air = false;
                    slot.release();
                } else if was || interrupted || live_now(&slot.def, tick) {
                    // On air or mid-ramp, the keyer already holds a pair.
                    slot.flags.on_air = true;
                } else {
                    slot.flags.pending = Some(Pending {
                        action: PendingAction::Cut,
                        deadline: Some(tick.index.saturating_add(TAKE_PRIME_DEADLINE_TICKS)),
                        boot: false,
                    });
                    slot.pending_op = Some(operation_id);
                    return;
                }
                if was != slot.flags.on_air || interrupted {
                    queue_event(
                        &mut self.events,
                        &self.counters,
                        event_of(tick.index, keyer, slot.flags, KeyerReason::Operator),
                    );
                }
                sink.applied(operation_id);
            }
            KeyerAction::Auto(on, rate) => {
                if slot.flags.ramp.is_some() {
                    sink.refused(operation_id, REFUSE_KEYER_RAMPING);
                    return;
                }
                // An operator AUTO takes the keyer out of a tied transition,
                // the no-op toward the current state included (it stays put).
                slot.flags.flight = None;
                if on == slot.flags.on_air {
                    // Already there: an applied no-op, never a second ramp.
                    sink.applied(operation_id);
                    return;
                }
                let rate = rate.unwrap_or(slot.def.rate);
                if on && !live_now(&slot.def, tick) {
                    slot.flags.pending = Some(Pending {
                        action: PendingAction::Auto(rate),
                        deadline: Some(tick.index.saturating_add(TAKE_PRIME_DEADLINE_TICKS)),
                        boot: false,
                    });
                    slot.pending_op = Some(operation_id);
                    return;
                }
                slot.flags.ramp = Some(Ramp {
                    start: tick.index,
                    rate,
                    to_on: on,
                });
                slot.ramp_op = Some(operation_id);
            }
        }
    }

    /// Whether every tied keyer that a background take would put on has a
    /// fresh fill and key now (a CUT or AUTO is held until it does).
    pub(crate) fn tied_going_on_fresh(&self, tick: Tick) -> bool {
        self.slots
            .iter()
            .flatten()
            .filter(|s| s.flags.tie && !s.flags.on_air && s.flags.ramp.is_none())
            .all(|s| live_now(&s.def, tick))
    }

    /// A background AUTO started on this tick: fix each tied keyer's
    /// participation and direction (design note §3.2, review F1). A keyer in
    /// its own AUTO does not take part.
    pub(crate) fn begin_tied_transition(&mut self) {
        for slot in self.slots.iter_mut().flatten() {
            slot.flags.flight =
                (slot.flags.tie && slot.flags.ramp.is_none()).then_some(!slot.flags.on_air);
        }
    }

    /// A background take completed on this tick (the AUTO terminal tick, or
    /// a CUT): every participant lands in the direction fixed at the start.
    /// A keyer whose participation was cancelled (source lost, operator
    /// verb) or that was tied during the flight does not move. A CUT outside
    /// an AUTO calls [`Self::begin_tied_transition`] first.
    pub(crate) fn complete_tied(&mut self, tick: Tick) {
        for (index, entry) in self.slots.iter_mut().enumerate() {
            let Some(slot) = entry.as_mut() else {
                continue;
            };
            let Some(to_on) = slot.flags.flight.take() else {
                continue;
            };
            slot.flags.on_air = to_on;
            if !to_on {
                slot.release();
            }
            queue_event(
                &mut self.events,
                &self.counters,
                event_of(tick.index, index_u8(index), slot.flags, KeyerReason::Tie),
            );
        }
    }

    /// The background AUTO ended without completing (aborted, interrupted
    /// by a punch, or a rejected frame): participants keep their state. A
    /// keyer that was going on and is still off releases its pair.
    pub(crate) fn cancel_tied(&mut self) {
        for slot in self.slots.iter_mut().flatten() {
            if slot.flags.flight.take().is_some() && !slot.flags.on_air && slot.flags.ramp.is_none()
            {
                slot.release();
            }
        }
    }

    /// After a rejected frame: bring the committed draws in line with the
    /// restored state where a background AUTO was cancelled. A keyer neither
    /// in its own AUTO nor in a tied one draws weight 1 on air and nothing
    /// off; its pictures are the committed pair, untouched.
    pub(crate) fn settle_draws(&self, draws: &mut [Option<KeyerDraw>; MAX_DSK]) {
        for entry in draws.iter_mut() {
            let Some(draw) = entry.as_mut() else {
                continue;
            };
            let Some(flags) = self.flags(usize::from(draw.keyer)) else {
                *entry = None;
                continue;
            };
            if flags.ramp.is_some() || flags.flight.is_some() {
                continue;
            }
            if flags.on_air {
                draw.weight = 1.0;
            } else {
                *entry = None;
            }
        }
    }

    /// A repeat tick: a held command past its deadline is refused; nothing
    /// is applied on a stale picture.
    pub(crate) fn expire_on_repeat(&mut self, tick: u64, outcomes: &EventStream<SwitcherOutcome>) {
        for slot in self.slots.iter_mut().flatten() {
            let Some(pending) = slot.flags.pending else {
                continue;
            };
            if pending.deadline.is_some_and(|d| tick >= d) {
                slot.flags.pending = None;
                if let Some(op) = slot.pending_op.take() {
                    refuse(outcomes, op, tick, REFUSE_KEYER_REPEAT_DEADLINE);
                }
            }
        }
    }

    /// Resolve this tick's keyer draws (in ascending z), the loss policy, the
    /// last-good pair and the tally. `auto_p` is the background AUTO's tie
    /// envelope while one is in flight. Wait-free: atomic loads, `Arc`
    /// refcounts and fixed arrays only.
    #[allow(clippy::too_many_lines)]
    // reason: one linear per-keyer pass (read, pair, loss, weight, tally);
    // splitting it would thread the same borrowed slot through helpers.
    pub(crate) fn resolve(
        &mut self,
        tick: Tick,
        auto_p: Option<TieEnvelope>,
        out: &mut [Option<KeyerDraw>; MAX_DSK],
        outcomes: &EventStream<SwitcherOutcome>,
    ) {
        for draw in out.iter_mut() {
            *draw = None;
        }
        let mut program = TallySet::default();
        program.insert(TallyRole::Program);
        if auto_p.is_some() {
            program.insert(TallyRole::Preview);
        }
        let mut preview = TallySet::default();
        preview.insert(TallyRole::Preview);
        let mut next = out.iter_mut();
        let canvas = self.canvas;
        for &index in &self.draw_order {
            let Some(slot) = self.slots.get_mut(index).and_then(Option::as_mut) else {
                continue;
            };
            let keyer = index_u8(index);
            let has_key = slot.def.key.is_some();
            if slot.flags.tie && !slot.flags.on_air && slot.flags.ramp.is_none() {
                preview.insert(TallyRole::Fill(keyer));
                if has_key {
                    preview.insert(TallyRole::Key(keyer));
                }
            }
            let weight = weight_of(slot.flags, tick, auto_p);
            // A tied participant is engaged for the whole background AUTO,
            // weight 0 included (a DIP before its switch point): its loss
            // policy applies on every tick of the flight (review F1).
            let engaged =
                slot.flags.on_air || slot.flags.ramp.is_some() || slot.flags.flight.is_some();
            if !engaged && weight <= 0.0 {
                // Off and not in a transition: reads nothing.
                continue;
            }
            let max = slot.def.max_source;
            let fill_read = slot.def.fill.read_at(tick.pts);
            let key_read = slot.def.key.as_ref().map(|k| k.read_at(tick.pts));
            let fill_live = fresh_fit(&fill_read, max);
            let key_live = match &key_read {
                Some(read) => fresh_fit(read, max).map(Some),
                None => Some(None),
            };
            let mut worst = rank(fill_read.state());
            for read in std::iter::once(&fill_read).chain(key_read.as_ref()) {
                if oversize(read, max) {
                    self.counters
                        .oversize_frames
                        .fetch_add(1, Ordering::Relaxed);
                    // Oversize is a non-live read: it ranks as stale.
                    worst = worst.max(1);
                }
            }
            if let Some(read) = &key_read {
                worst = worst.max(rank(read.state()));
            }
            if let (Some(fill), Some(key)) = (fill_live, key_live) {
                slot.last_good = Some((Arc::clone(fill), key.cloned()));
                if slot.flags.stale {
                    slot.flags.stale = false;
                    queue_event(
                        &mut self.events,
                        &self.counters,
                        event_of(tick.index, keyer, slot.flags, KeyerReason::SourceRecovered),
                    );
                }
            } else if worst >= slot.def.trigger.rank() {
                let loss = slot.def.loss;
                match loss {
                    LossPolicy::DropToOffAir if engaged => {
                        slot.flags.on_air = false;
                        slot.flags.ramp = None;
                        // Loss cancels tied participation: the terminal tick
                        // never puts it back on (review F1).
                        slot.flags.flight = None;
                        slot.release();
                        if let Some(op) = slot.ramp_op.take() {
                            refuse(outcomes, op, tick.index, REFUSE_KEYER_SOURCE_LOST);
                        }
                        self.counters.source_lost.fetch_add(1, Ordering::Relaxed);
                        queue_event(
                            &mut self.events,
                            &self.counters,
                            event_of(tick.index, keyer, slot.flags, KeyerReason::SourceLost),
                        );
                        continue;
                    }
                    LossPolicy::HoldLastGood if !slot.flags.stale => {
                        slot.flags.stale = true;
                        queue_event(
                            &mut self.events,
                            &self.counters,
                            event_of(tick.index, keyer, slot.flags, KeyerReason::SourceLost),
                        );
                    }
                    _ => {}
                }
            }
            if weight <= 0.0 || weight.is_nan() {
                continue;
            }
            let Some((fill, key)) = &slot.last_good else {
                // No pair yet: the keyer contributes nothing (never a slate).
                continue;
            };
            let Some(target) = next.next() else {
                break;
            };
            program.insert(TallyRole::Fill(keyer));
            if key.is_some() {
                program.insert(TallyRole::Key(keyer));
            }
            *target = Some(KeyerDraw {
                keyer,
                fill: Arc::clone(fill),
                key: key.clone(),
                rect: slot.def.fly.unwrap_or(KeyerFly {
                    x: 0,
                    y: 0,
                    w: canvas.0,
                    h: canvas.1,
                }),
                weight: weight.min(1.0),
                params: slot.def.params,
            });
        }
        self.tally = KeyerTally { program, preview };
    }

    /// Publish this tick's queued events (a committed frame only).
    pub(crate) fn commit(&mut self, stream: &EventStream<KeyerEvent>) {
        for entry in &mut self.events {
            if let Some(event) = entry.take() {
                stream.publish(event);
            }
        }
    }

    /// A rejected frame: restore the flags staged at the start of the tick,
    /// refuse the operations that moved on it, drop queued events. An
    /// unchanged in-flight ramp or held command from an earlier tick
    /// survives; one that started, ended or changed this tick is refused
    /// (its op either sits in the driver's staged list, which the driver
    /// refuses, or in this slot, refused here). The boot intent survives.
    /// Every last-good pair and the tally go back to their tick-start values
    /// (review F2): a staged off never loses the committed pair.
    pub(crate) fn reject(
        &mut self,
        tick: u64,
        reason: &str,
        outcomes: &EventStream<SwitcherOutcome>,
    ) {
        for ((slot, staged), pair) in self
            .slots
            .iter_mut()
            .zip(self.staged.iter())
            .zip(self.staged_pairs.iter_mut())
        {
            let Some(slot) = slot.as_mut() else {
                continue;
            };
            slot.last_good = pair.take();
            let current = slot.flags;
            slot.flags = *staged;
            if staged.ramp != current.ramp {
                slot.flags.ramp = None;
                if let Some(op) = slot.ramp_op.take() {
                    refuse(outcomes, op, tick, reason);
                }
            }
            if staged.pending != current.pending {
                slot.flags.pending = staged.pending.filter(|p| p.boot);
                if let Some(op) = slot.pending_op.take() {
                    refuse(outcomes, op, tick, reason);
                }
            }
            // A ramp on that started or ended this tick was refused above
            // and left the keyer off: it holds no pair. A keyer committed
            // on air keeps its committed pair.
            if !slot.flags.on_air && slot.flags.ramp.is_none() && slot.flags.flight.is_none() {
                slot.release();
            }
        }
        self.tally = self.staged_tally;
        self.events.fill(None);
    }

    /// The snapshot rows (change-only publication by the driver).
    #[must_use]
    pub fn snapshot(&self) -> [Option<KeyerSnapshot>; MAX_DSK] {
        let mut rows = [None; MAX_DSK];
        for (row, slot) in rows.iter_mut().zip(self.slots.iter()) {
            *row = slot.as_ref().map(|s| KeyerSnapshot {
                on_air: s.flags.on_air,
                tie: s.flags.tie,
                ramp: s.flags.ramp.map(|r| RampSnapshot {
                    start_tick: r.start,
                    rate: r.rate.get(),
                    to_on: r.to_on,
                }),
                stale: s.flags.stale,
                pending: s.pending_op.is_some(),
            });
        }
        rows
    }

    /// The tally of the last resolved tick.
    #[must_use]
    pub fn tally(&self) -> KeyerTally {
        self.tally
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loss_ranks_follow_the_failure_ladder() {
        assert!(rank(SourceState::Live) < rank(SourceState::Stale));
        assert!(rank(SourceState::Stale) < rank(SourceState::Reconnecting));
        assert!(rank(SourceState::Reconnecting) < rank(SourceState::NoSignal));
        assert_eq!(
            LossTrigger::Reconnecting.rank(),
            rank(SourceState::Reconnecting)
        );
    }

    #[test]
    fn a_ramp_weight_is_k_over_n_and_direction_aware() {
        let n = FrameCount::try_from(4).unwrap();
        let on = KeyerFlags {
            ramp: Some(Ramp {
                start: 10,
                rate: n,
                to_on: true,
            }),
            ..KeyerFlags::default()
        };
        let off = KeyerFlags {
            on_air: true,
            ramp: Some(Ramp {
                start: 10,
                rate: n,
                to_on: false,
            }),
            ..KeyerFlags::default()
        };
        for k in 0..4_u64 {
            let tick = Tick {
                index: 10 + k,
                pts: multiview_core::time::MediaTime::ZERO,
            };
            let expected = kernel::opacity(Progress::new(u32::try_from(k).unwrap(), n));
            assert!((weight_of(on, tick, None) - expected).abs() < 1e-6);
            assert!((weight_of(off, tick, None) - (1.0 - expected)).abs() < 1e-6);
        }
        // Review F1 changed this expectation: the tied direction is the one
        // fixed when the background AUTO started (`flight`), no longer
        // re-derived from `tie` and `on_air` each tick. A tied keyer that is
        // not a participant (its participation was cancelled by a loss)
        // draws its resting weight, never the envelope.
        let tied = KeyerFlags {
            tie: true,
            flight: Some(true),
            ..KeyerFlags::default()
        };
        let tick = Tick {
            index: 0,
            pts: multiview_core::time::MediaTime::ZERO,
        };
        assert!((weight_of(tied, tick, Some(TieEnvelope::mix(0.25))) - 0.25).abs() < 1e-6);
        let tied_on = KeyerFlags {
            on_air: true,
            flight: Some(false),
            ..tied
        };
        assert!((weight_of(tied_on, tick, Some(TieEnvelope::mix(0.25))) - 0.75).abs() < 1e-6);
        assert!((weight_of(tied_on, tick, None) - 1.0).abs() < 1e-6);
        let cancelled = KeyerFlags {
            flight: None,
            ..tied
        };
        assert!(weight_of(cancelled, tick, Some(TieEnvelope::mix(0.25))).abs() < 1e-6);
    }

    #[test]
    fn tally_sets_are_fixed_and_deduplicated() {
        let mut set = TallySet::default();
        set.insert(TallyRole::Fill(0));
        set.insert(TallyRole::Fill(0));
        set.insert(TallyRole::Key(1));
        assert_eq!(set.roles().count(), 2);
        assert!(set.contains(TallyRole::Key(1)));
    }

    #[test]
    fn admission_refuses_three_keyers_and_an_over_budget_document() {
        assert!(admit(Vec::new(), 64, 64, 3.0).unwrap().is_empty());
        let err = admit(Vec::new(), 64, 64, f32::NAN).unwrap_err();
        assert!(err.contains("finite"), "{err}");
    }
}
