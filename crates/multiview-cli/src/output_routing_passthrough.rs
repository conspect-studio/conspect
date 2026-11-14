//! The router's P0 decision (output-routing §12.5, issue #344 D2b).
//!
//! A take of `{source}` onto an eligible output runs exactly as before (the
//! encoded source key is the **picture bridge**, frame-accurate at the take
//! tick). Then the router opens a **passthrough candidate** for (source,
//! output): a [`crate::passthrough_live::PassthroughLive`] key opened and
//! slate-baked on the `routing-open` thread, fed one tick per Program tick
//! like any live key, never charged to `max_encoders` or a principal (its
//! own cap is [`MAX_PASSTHROUGH_KEYS`]). Once the key's own meter passes the
//! shared match rules (`evaluate`, at least 10 s of history), the router lets
//! the key start a run and asks the dispatcher for an ordinary unforced move
//! whose gate is the run-start strict IDR. The encoded key then has no
//! subscriber and lingers out: 0 encoder sessions for that output.
//!
//! Faults: when the key bridges to its slate while an output reads it, the
//! router moves that output onto an on-demand picture bridge (the encoded
//! source key, reused or opened against the principal that patched the
//! output; at cap it stays on the slate and says so). The key is held off
//! runs while the output is away and is let back once its probe shows 30
//! (or, after a parameter change, 300) on-time ticks and it still matches;
//! the move back lands at the next run-start IDR.
//!
//! Isolation (invariants #1/#10): nothing here waits. The key's tap is read
//! only on the key's own worker thread (`try_recv`), the open runs on the
//! opener thread (`try_send`, bounded queue), the status is atomics, and a
//! key whose worker stops is closed like any live key (its outputs fall back
//! to Program through the dispatcher).

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::TrySendError;
use std::sync::Arc;

use multiview_compositor::pipeline::Nv12Image;
use multiview_output::passthrough::match_rules::evaluate_traits;
use multiview_output::passthrough::{BridgeReason, MatchMiss, SlotSpec, SourceTraits};

use super::{
    GapFill, LiveEncoder, OpenRequest, OutputRouter, PendingOpen, PictureBus, Reap, NO_FORCE,
    OPEN_DEADLINE_TICKS, WEDGED_ENCODER_TICKS,
};
use crate::passthrough_live::{PassthroughState, PassthroughStatus};
use crate::rendition_dispatch::{DtsTimeBase, RenditionDispatcher};
use crate::rendition_workers::{BakedTick, OfferRejection, RenditionWorker};

/// Passthrough keys open at once (#344 D2b). They hold no encoder session,
/// so they are not counted against `routing.max_encoders`.
pub const MAX_PASSTHROUGH_KEYS: usize = 8;

/// The readback / plan reason of an output whose medium has no P0 in this
/// slice (anything but HLS with TS segments and an H.264/HEVC slot 0).
pub const NOT_SUPPORTED_ON_MEDIUM: &str = "passthrough-not-supported-on-medium";

/// The readback reason of an output bridging on its slate because no
/// encoder session could be admitted for the picture bridge.
pub const NO_BRIDGE_SESSION: &str = "bridging on slate: no encoder session";

/// A passthrough open (key + slate bake) must arrive within this many ticks.
pub const PASS_OPEN_DEADLINE_TICKS: u64 = 300;

/// Ticks between two attempts that could not start (open queue full, no
/// dispatcher slot, a refused move, a refused bridge admission).
pub const PASS_RETRY_TICKS: u64 = 30;

/// Ticks between two refreshes of the snapshot's passthrough counters while
/// a passthrough key runs (the readback is otherwise republished on change
/// only).
const COUNTER_REFRESH_TICKS: u64 = 25;

/// Whether an output can take P0, and at what slot (§12.5 match rules).
#[derive(Debug, Clone)]
pub enum PassEligibility {
    /// Not on this medium in this slice ([`NOT_SUPPORTED_ON_MEDIUM`]).
    NotSupported,
    /// Eligible at `slot`.
    Eligible {
        /// The slot the source must match.
        slot: SlotSpec,
        /// The output's rendition settings index (its slot-0 encoder: the
        /// slate is baked with it).
        settings: usize,
    },
}

/// The source traits of a source id (wait-free; `None` with no connection).
pub type TraitsLookup = Arc<dyn Fn(&str) -> Option<SourceTraits> + Send + Sync>;

/// What the passthrough opener receives.
#[derive(Debug, Clone)]
pub struct PassRequest {
    /// The source id.
    pub source: String,
    /// The output's rendition settings index.
    pub settings: usize,
    /// The slot the key's own matcher evaluates.
    pub slot: SlotSpec,
}

/// Opens a passthrough key (subscribes the source's tap, bakes the format
/// slate) on the `routing-open` thread.
pub type PassOpenFn = Box<
    dyn FnMut(&PassRequest) -> Result<(Box<dyn LiveEncoder>, Arc<PassthroughStatus>), String>
        + Send,
>;

/// The router's passthrough wiring.
pub struct PassHooks {
    /// The opener.
    pub open: PassOpenFn,
    /// The traits lookup (plan and admission).
    pub traits: TraitsLookup,
}

/// The P0 state of one output in the readback.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PassthroughView {
    /// `off`, `opening`, `metering`, `moving`, `passthrough`, `slate`,
    /// `bridging`, `returning`, `unavailable`.
    pub state: String,
    /// Why it is not passing through (a match miss, a bridge reason, the
    /// bake failure), `None` while it is. Never names a source.
    pub reason: Option<String>,
}

/// Passthrough counters (snapshot; `/metrics` reads them).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PassthroughCounters {
    /// Passthrough keys open.
    pub keys_live: usize,
    /// Source AUs passed through (all keys, closed ones included).
    pub aus: u64,
    /// Repeat AUs emitted.
    pub repeats: u64,
    /// AUs dropped by drift corrections.
    pub drift_drops: u64,
    /// Runs ended by a stall.
    pub stalls: u64,
    /// Slate ticks emitted.
    pub slate_ticks: u64,
    /// Picture bridges moved onto after a fault.
    pub bridges: u64,
    /// Bridges that found no encoder session (the output stayed on slate).
    pub bridges_refused: u64,
    /// Moves onto a passthrough key that landed.
    pub moves_in: u64,
    /// Candidates whose open (or slate bake) failed.
    pub open_failures: u64,
}

/// The static half of the shared predicate: eligibility and the source's
/// declared traits (`evaluate_traits`, the first rules of `evaluate`).
/// The router opens a candidate only when this passes; the plan reports
/// P0 only when this and the key's measured verdict pass.
///
/// # Errors
/// The first miss as its readback string.
pub fn static_verdict(
    eligibility: &PassEligibility,
    traits: Option<SourceTraits>,
) -> Result<(), &'static str> {
    let PassEligibility::Eligible { slot, .. } = eligibility else {
        return Err(NOT_SUPPORTED_ON_MEDIUM);
    };
    let Some(traits) = traits else {
        return Err(MatchMiss::NotEnoughHistory.as_str());
    };
    evaluate_traits(&traits, slot).map_err(MatchMiss::as_str)
}

/// One open passthrough key.
struct PassKey {
    key: usize,
    worker: Option<RenditionWorker>,
    status: Arc<PassthroughStatus>,
    generation: u64,
    lost: Arc<AtomicBool>,
    seen_event: bool,
    silent: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// Waiting for the open `id`.
    Opening { id: u64, deadline: u64 },
    /// The key runs; its meter decides (or the next attempt waits).
    Metering,
    /// An unforced move onto the key was requested.
    Moving,
    /// The output reads the key.
    On,
    /// The output was moved off the key onto a picture bridge.
    Away,
    /// Waiting for the move back onto the key.
    Returning,
    /// The open failed (bake failure): no retry until the next take.
    Failed,
}

/// The on-demand picture bridge of one candidate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Bridge {
    None,
    /// Waiting for the encoder open `id`.
    Opening(u64),
    /// The bridge key runs; its first window has not been seen yet.
    Warming(usize),
    /// A forced move onto the bridge key was requested.
    Moving(usize),
    /// No session could be admitted: the output stays on the slate.
    Refused,
}

/// One output's passthrough candidate.
struct Candidate {
    output: usize,
    source: String,
    phase: Phase,
    key: Option<PassKey>,
    bridge: Bridge,
    /// The fault that took the output off the key needs the 300-tick wait.
    param_fault: bool,
    /// The next tick an attempt that could not start is retried.
    retry_at: u64,
    /// The failure text of a failed open.
    failure: Option<String>,
    /// The take that patched the output superseded the candidate: it closes
    /// as soon as no output reads its key.
    ending: bool,
    /// The readback signature last published ([`Candidate::signature`]).
    shown: u64,
}

impl Candidate {
    /// A cheap signature of what [`PassState::view`] shows (phase, bridge,
    /// the key's emitted state, reason and verdict): the snapshot is
    /// republished when it changes, never per tick otherwise.
    fn signature(&self) -> u64 {
        let phase: u64 = match self.phase {
            Phase::Opening { .. } => 1,
            Phase::Metering => 2,
            Phase::Moving => 3,
            Phase::On => 4,
            Phase::Away => 5,
            Phase::Returning => 6,
            Phase::Failed => 7,
        };
        let bridge: u64 = match self.bridge {
            Bridge::Refused => 1,
            _ => 0,
        };
        let (state, reason, verdict) = self.key.as_ref().map_or((0, 0, 0), |k| {
            (
                match k.status.state() {
                    PassthroughState::Slate => 1_u64,
                    _ => 0,
                },
                k.status.reason().map_or(0_u64, |r| {
                    u64::from(crate::passthrough_live::reason_code(r))
                }),
                match k.status.verdict() {
                    None => 0_u64,
                    Some(Ok(())) => 1,
                    Some(Err(m)) => u64::from(crate::passthrough_live::miss_code(m)),
                },
            )
        });
        phase | bridge << 4 | state << 5 | reason << 8 | verdict << 16
    }
}

/// The router's passthrough state.
pub(super) struct PassState {
    traits: Option<TraitsLookup>,
    candidates: Vec<Candidate>,
    counters: PassthroughCounters,
    /// Counters of closed keys (added to the live ones).
    closed: PassthroughCounters,
    canvas: Option<Arc<Nv12Image>>,
    last_refresh: u64,
}

impl PassState {
    pub(super) fn new(traits: Option<TraitsLookup>) -> Self {
        Self {
            traits,
            candidates: Vec::new(),
            counters: PassthroughCounters::default(),
            closed: PassthroughCounters::default(),
            canvas: Nv12Image::solid(
                2,
                2,
                16,
                128,
                128,
                multiview_core::color::ColorInfo::default(),
            )
            .ok()
            .map(Arc::new),
            last_refresh: 0,
        }
    }

    fn get(&self, output: usize) -> Option<&Candidate> {
        self.candidates.iter().find(|c| c.output == output)
    }

    fn get_mut(&mut self, output: usize) -> Option<&mut Candidate> {
        self.candidates.iter_mut().find(|c| c.output == output)
    }

    /// Whether the open result `id` is a passthrough open.
    pub(super) fn owns_open(&self, id: u64) -> bool {
        self.candidates
            .iter()
            .any(|c| matches!(c.phase, Phase::Opening { id: i, .. } if i == id))
    }

    /// A live event of a passthrough key: record it. `false` when no
    /// passthrough key has this generation and key index.
    pub(super) fn saw_event(&mut self, generation: u64, key: Option<usize>, video: bool) -> bool {
        let Some(k) = self
            .candidates
            .iter_mut()
            .filter_map(|c| c.key.as_mut())
            .find(|k| k.generation == generation && Some(k.key) == key)
        else {
            return false;
        };
        if video {
            k.seen_event = true;
        }
        k.silent = 0;
        true
    }

    /// The bus a passthrough key carries.
    pub(super) fn bus_of_key(&self, key: usize) -> Option<PictureBus> {
        self.candidates
            .iter()
            .find(|c| c.key.as_ref().is_some_and(|k| k.key == key))
            .map(|c| PictureBus::Source(c.source.clone()))
    }

    /// Whether output `output` reads its passthrough key `key`.
    pub(super) fn on_key(&self, output: usize, key: usize) -> bool {
        self.get(output)
            .and_then(|c| c.key.as_ref())
            .is_some_and(|k| k.key == key)
    }

    /// The passthrough key status of output `output` (the audio rail and
    /// tests read it).
    pub(super) fn status_of(&self, output: usize) -> Option<Arc<PassthroughStatus>> {
        self.get(output)
            .and_then(|c| c.key.as_ref())
            .map(|k| Arc::clone(&k.status))
    }

    /// The readback of output `output`.
    pub(super) fn view(&self, output: usize) -> Option<PassthroughView> {
        let c = self.get(output)?;
        let status = c.key.as_ref().map(|k| &k.status);
        let miss = || {
            status.and_then(|s| s.verdict()).map_or_else(
                || Some(MatchMiss::NotEnoughHistory.as_str().to_owned()),
                |v| v.err().map(|m| m.as_str().to_owned()),
            )
        };
        let bridge_reason = || {
            status
                .and_then(|s| s.reason())
                .map(|r| r.as_str().to_owned())
        };
        let (state, reason) = match c.phase {
            Phase::Opening { .. } => ("opening", None),
            Phase::Metering => ("metering", miss()),
            Phase::Moving => ("moving", None),
            Phase::On => {
                if c.bridge == Bridge::Refused {
                    ("slate", Some(NO_BRIDGE_SESSION.to_owned()))
                } else if status.is_some_and(|s| s.state() == PassthroughState::Slate) {
                    ("slate", bridge_reason())
                } else {
                    ("passthrough", None)
                }
            }
            Phase::Away => ("bridging", bridge_reason()),
            Phase::Returning => ("returning", None),
            Phase::Failed => ("unavailable", c.failure.clone()),
        };
        Some(PassthroughView {
            state: state.to_owned(),
            reason,
        })
    }

    /// The counters (live keys + closed ones).
    pub(super) fn counters(&self) -> PassthroughCounters {
        self.counters
    }

    fn refresh_counters(&mut self) {
        let mut c = self.closed;
        c.keys_live = 0;
        for k in self.candidates.iter().filter_map(|c| c.key.as_ref()) {
            c.keys_live += 1;
            add_status(&mut c, &k.status);
        }
        c.bridges = self.counters.bridges;
        c.bridges_refused = self.counters.bridges_refused;
        c.moves_in = self.counters.moves_in;
        c.open_failures = self.counters.open_failures;
        self.counters = c;
    }

    /// Candidates holding (or about to hold) a key: every candidate but a
    /// failed one, so the cap bounds keys, opens and retries alike.
    fn keys_live(&self) -> usize {
        self.candidates
            .iter()
            .filter(|c| c.phase != Phase::Failed)
            .count()
    }
}

fn add_status(c: &mut PassthroughCounters, s: &PassthroughStatus) {
    let load = |a: &AtomicU64| a.load(Ordering::Relaxed);
    c.aus = c.aus.saturating_add(load(&s.aus));
    c.repeats = c.repeats.saturating_add(load(&s.repeats));
    c.drift_drops = c.drift_drops.saturating_add(load(&s.drift_drops));
    c.stalls = c.stalls.saturating_add(load(&s.stalls));
    c.slate_ticks = c.slate_ticks.saturating_add(load(&s.slate_ticks));
}

/// Return ticks needed before a move back onto the key.
const fn return_ticks(param_fault: bool) -> u32 {
    if param_fault {
        multiview_output::passthrough::clock::PARAM_RETURN_TICKS
    } else {
        multiview_output::passthrough::clock::STALL_RETURN_TICKS
    }
}

impl OutputRouter {
    /// The passthrough status of output `index`'s key (tests, audio).
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn passthrough_status(&self, index: usize) -> Option<Arc<PassthroughStatus>> {
        self.pass.status_of(index)
    }

    /// The source-time delay of output `index`'s audio (#344 D2b): the
    /// key's published delay while the output reads its passthrough key and
    /// the key passes the source through, else `None`.
    pub(super) fn passthrough_audio_delay(&self, index: usize) -> Option<u32> {
        let key = self.outputs.get(index)?.key;
        if !self.pass.on_key(index, key) {
            return None;
        }
        let status = self.pass.status_of(index)?;
        matches!(
            status.state(),
            PassthroughState::Passthrough | PassthroughState::Repeat
        )
        .then(|| status.delay_ticks())
    }

    /// A new video take on output `index` towards `bus`: an existing
    /// candidate for another feed ends (its key closes once no output reads
    /// it); a re-take of the same source keeps it.
    pub(super) fn pass_take_started(&mut self, index: usize, bus: &PictureBus) {
        let Some(c) = self.pass.get_mut(index) else {
            return;
        };
        // A re-take of the same source keeps the candidate (on P0 the take
        // applies at once: the output already carries that source); a
        // failed one is retried by the next take.
        if *bus == PictureBus::Source(c.source.clone()) && c.phase != Phase::Failed {
            return;
        }
        c.ending = true;
        self.dirty = true;
    }

    /// The dispatcher fell output `index` back to Program (its key failed or
    /// retired): the candidate ends.
    pub(super) fn pass_fell_back(&mut self, index: usize, _dispatcher: &mut RenditionDispatcher) {
        if let Some(c) = self.pass.get_mut(index) {
            c.ending = true;
            self.dirty = true;
        }
    }

    /// A move landed on `key` for output `index`: a move onto the output's
    /// passthrough key or its picture bridge (no take is pending for it).
    /// `true` when handled here.
    pub(super) fn passthrough_applied(
        &mut self,
        index: usize,
        key: usize,
        at: u64,
        _dispatcher: &mut RenditionDispatcher,
    ) -> bool {
        let take_pending = self.outputs.get(index).is_some_and(|o| o.pending.is_some());
        let Some(c) = self.pass.get_mut(index) else {
            return false;
        };
        let on_pass = c.key.as_ref().is_some_and(|k| k.key == key);
        let on_bridge = matches!(c.bridge, Bridge::Moving(k) if k == key);
        if take_pending || !(on_pass || on_bridge) {
            return false;
        }
        if on_pass {
            c.phase = Phase::On;
            c.bridge = Bridge::None;
            self.pass.counters.moves_in = self.pass.counters.moves_in.saturating_add(1);
        } else {
            c.phase = Phase::Away;
            c.bridge = Bridge::None;
            // Held off runs while away: it is let back only when stable.
            if let Some(k) = c.key.as_ref() {
                k.status.allow_runs_from(u64::MAX);
            }
            self.pass.counters.bridges = self.pass.counters.bridges.saturating_add(1);
        }
        if let Some(out) = self.outputs.get_mut(index) {
            out.key = key;
            out.since = at;
        }
        self.dirty = true;
        true
    }

    /// A passthrough or bridge move of output `index` was refused. `true`
    /// when handled here.
    pub(super) fn passthrough_move_refused(&mut self, index: usize, tick: u64) -> bool {
        let take_pending = self.outputs.get(index).is_some_and(|o| o.pending.is_some());
        let Some(c) = self.pass.get_mut(index) else {
            return false;
        };
        if take_pending {
            return false;
        }
        match (c.phase, c.bridge) {
            (_, Bridge::Moving(_)) => {
                c.bridge = Bridge::None;
                c.retry_at = tick.saturating_add(PASS_RETRY_TICKS);
            }
            (Phase::Moving | Phase::Returning, _) => {
                c.phase = if c.phase == Phase::Moving {
                    Phase::Metering
                } else {
                    Phase::Away
                };
                if let Some(k) = c.key.as_ref() {
                    // A run nobody reads is no move point any more: drop it
                    // and hold runs until the next attempt.
                    k.status.allow_runs_from(u64::MAX);
                    k.status.request_rearm();
                }
                c.retry_at = tick.saturating_add(PASS_RETRY_TICKS);
            }
            _ => return false,
        }
        self.dirty = true;
        true
    }

    /// The passthrough open `id` arrived.
    pub(super) fn passthrough_opened(
        &mut self,
        id: u64,
        encoder: Result<Box<dyn LiveEncoder>, String>,
        status: Option<Arc<PassthroughStatus>>,
        tick: u64,
        dispatcher: &mut RenditionDispatcher,
    ) {
        let Some(pos) = self
            .pass
            .candidates
            .iter()
            .position(|c| matches!(c.phase, Phase::Opening { id: i, .. } if i == id))
        else {
            if let Ok(encoder) = encoder {
                self.reap(Reap::Encoder(encoder));
            }
            return;
        };
        self.dirty = true;
        let (mut encoder, status) = match (encoder, status) {
            (Ok(encoder), Some(status)) => (encoder, status),
            (Ok(encoder), None) => {
                self.reap(Reap::Encoder(encoder));
                self.pass_open_failed(pos, "passthrough key without status");
                return;
            }
            (Err(e), _) => {
                self.pass_open_failed(pos, &e);
                return;
            }
        };
        // Runs wait for the router's move request (a run start is the only
        // move point of the key).
        status.allow_runs_from(u64::MAX);
        let tb = encoder.time_base();
        encoder.start_at(tick);
        let gate = encoder.move_gate();
        let Ok(key) = dispatcher.add_video_key(
            DtsTimeBase {
                num: tb.num,
                den: tb.den,
            },
            gate,
        ) else {
            // No dispatcher slot (shared with live encoders): retry later.
            self.reap(Reap::Encoder(encoder));
            if let Some(c) = self.pass.candidates.get_mut(pos) {
                c.phase = Phase::Metering;
                c.retry_at = tick.saturating_add(PASS_RETRY_TICKS);
            }
            return;
        };
        if let Some(params) = encoder.parameter_sets() {
            let _ = dispatcher.set_parameter_sets(key, params);
        }
        if let Some(stream) = encoder.stream_params() {
            let _ = dispatcher.set_stream_params(key, stream);
        }
        let worker = GapFill {
            inner: encoder,
            force_at: Arc::new(AtomicU64::new(NO_FORCE)),
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
            if let Some(c) = self.pass.candidates.get_mut(pos) {
                c.phase = Phase::Metering;
                c.retry_at = tick.saturating_add(PASS_RETRY_TICKS);
            }
            return;
        };
        if let Some(c) = self.pass.candidates.get_mut(pos) {
            c.key = Some(PassKey {
                key,
                worker: Some(worker),
                status,
                generation,
                lost,
                seen_event: false,
                silent: 0,
            });
            c.phase = Phase::Metering;
        }
    }

    fn pass_open_failed(&mut self, pos: usize, error: &str) {
        self.pass.counters.open_failures = self.pass.counters.open_failures.saturating_add(1);
        if let Some(c) = self.pass.candidates.get_mut(pos) {
            tracing::warn!(output = c.output, %error, "routing: passthrough candidate not opened");
            c.phase = Phase::Failed;
            // The bake / open error, bounded; it names no source.
            c.failure = Some(bounded_reason(&format!("slate-bake-failed: {error}")));
        }
    }

    /// The picture bridge open of output `index` arrived (`key` registered,
    /// or `None`).
    pub(super) fn bridge_opened(&mut self, index: usize, key: Option<usize>, tick: u64) {
        if let Some(c) = self.pass.get_mut(index) {
            c.bridge = if let Some(key) = key {
                Bridge::Warming(key)
            } else {
                c.retry_at = tick.saturating_add(PASS_RETRY_TICKS);
                Bridge::None
            };
            self.dirty = true;
        }
    }

    /// Close every passthrough key whose worker's event sink dropped an
    /// event (its stream has a hole).
    pub(super) fn close_lost_passthrough(&mut self, dispatcher: &mut RenditionDispatcher) {
        let lost: Vec<usize> = self
            .pass
            .candidates
            .iter()
            .filter(|c| {
                c.key
                    .as_ref()
                    .is_some_and(|k| k.lost.load(Ordering::Acquire))
            })
            .map(|c| c.output)
            .collect();
        for output in lost {
            self.close_pass_key(output, dispatcher);
            if let Some(c) = self.pass.get_mut(output) {
                c.ending = true;
            }
        }
    }

    /// Retire output `output`'s passthrough key and reap its worker (the
    /// tap reader drops with it: the source's tap is unsubscribed).
    fn close_pass_key(&mut self, output: usize, dispatcher: &mut RenditionDispatcher) {
        let Some(k) = self.pass.get_mut(output).and_then(|c| c.key.take()) else {
            return;
        };
        let _ = dispatcher.retire_video_key(k.key);
        add_status(&mut self.pass.closed, &k.status);
        if let Some(worker) = k.worker {
            self.reap(Reap::Worker(worker.close_input()));
        }
        self.dirty = true;
    }

    /// End of run: close every passthrough key.
    pub(super) fn close_all_passthrough(&mut self, dispatcher: &mut RenditionDispatcher) {
        let outputs: Vec<usize> = self.pass.candidates.iter().map(|c| c.output).collect();
        for output in outputs {
            self.close_pass_key(output, dispatcher);
        }
        self.pass.candidates.clear();
    }

    /// Offer every passthrough key this tick (never blocks: a full queue
    /// drops and the key's `GapFill` emits the missed ticks). A key whose
    /// worker ended or went silent is closed (its outputs fall back).
    pub(super) fn feed_passthrough(&mut self, tick: u64, dispatcher: &mut RenditionDispatcher) {
        let Some(canvas) = self.pass.canvas.clone() else {
            return;
        };
        let mut dead = Vec::new();
        for c in &mut self.pass.candidates {
            let Some(k) = c.key.as_mut() else { continue };
            let Some(worker) = k.worker.as_ref() else {
                continue;
            };
            let ended = matches!(
                worker.offer(BakedTick {
                    tick,
                    canvas: Arc::clone(&canvas),
                }),
                Err(OfferRejection::Ended)
            );
            k.silent = k.silent.saturating_add(1);
            if ended || worker.is_finished() || k.silent >= WEDGED_ENCODER_TICKS {
                dead.push(c.output);
            }
        }
        for output in dead {
            self.dead_encoders = self.dead_encoders.saturating_add(1);
            self.close_pass_key(output, dispatcher);
            if let Some(c) = self.pass.get_mut(output) {
                c.ending = true;
            }
        }
        for c in &mut self.pass.candidates {
            let sig = c.signature();
            if sig != c.shown {
                c.shown = sig;
                self.dirty = true;
            }
        }
        let live = self.pass.candidates.iter().any(|c| c.key.is_some());
        if self.dirty
            || (live && tick.saturating_sub(self.pass.last_refresh) >= COUNTER_REFRESH_TICKS)
        {
            self.pass.last_refresh = tick;
            self.pass.refresh_counters();
            if self.pass.counters != self.snapshot.load().passthrough {
                self.dirty = true;
            }
        }
    }

    /// The per-tick P0 decisions (bounded: O(outputs), no waiting).
    #[allow(clippy::too_many_lines)]
    // reason: one per-candidate phase machine (open, meter, move, fault,
    // bridge, return); splitting it would scatter the single decision.
    pub(super) fn advance_passthrough(&mut self, tick: u64, dispatcher: &mut RenditionDispatcher) {
        if self.pass.traits.is_none() {
            return;
        }
        // Retire ended candidates whose key no output reads (or moves to).
        let ended: Vec<usize> = self
            .pass
            .candidates
            .iter()
            .filter(|c| c.ending)
            .map(|c| c.output)
            .collect();
        for output in ended {
            let key = self
                .pass
                .get(output)
                .and_then(|c| c.key.as_ref())
                .map(|k| k.key);
            let read = key.is_some_and(|key| {
                self.outputs
                    .iter()
                    .enumerate()
                    .any(|(i, o)| o.key == key || dispatcher.pending_video_move(i) == Some(key))
            });
            if !read {
                self.close_pass_key(output, dispatcher);
                self.pass.candidates.retain(|c| c.output != output);
                self.dirty = true;
            } else if let Some(k) = self.pass.get(output).and_then(|c| c.key.as_ref()) {
                // Never let an ending key start a new run.
                k.status.allow_runs_from(u64::MAX);
            }
        }
        // New candidates: an output settled on a source, eligible.
        for index in 0..self.outputs.len() {
            let Some(out) = self.outputs.get(index) else {
                continue;
            };
            let PictureBus::Source(source) = &out.bus else {
                continue;
            };
            if out.pending.is_some() || self.pass.get(index).is_some() {
                continue;
            }
            let source = source.clone();
            let eligibility = self.cfg.passthrough.get(index).cloned();
            let Some(PassEligibility::Eligible { slot, .. }) = eligibility else {
                continue;
            };
            let traits = self.pass.traits.as_ref().and_then(|t| t(&source));
            if evaluate_traits_ok(traits, &slot).is_err() {
                continue;
            }
            if self.pass.keys_live() >= MAX_PASSTHROUGH_KEYS {
                continue;
            }
            self.pass.candidates.push(Candidate {
                output: index,
                source: source.clone(),
                phase: Phase::Metering,
                key: None,
                bridge: Bridge::None,
                param_fault: false,
                retry_at: tick,
                failure: None,
                ending: false,
                shown: 0,
            });
            self.dirty = true;
        }
        for pos in 0..self.pass.candidates.len() {
            self.advance_candidate(pos, tick, dispatcher);
        }
    }

    #[allow(clippy::too_many_lines)]
    fn advance_candidate(&mut self, pos: usize, tick: u64, dispatcher: &mut RenditionDispatcher) {
        let Some(c) = self.pass.candidates.get(pos) else {
            return;
        };
        if c.ending {
            return;
        }
        let index = c.output;
        let (phase, bridge, retry_at) = (c.phase, c.bridge, c.retry_at);
        let key = c
            .key
            .as_ref()
            .map(|k| (k.key, Arc::clone(&k.status), k.seen_event));
        let take_pending = self.outputs.get(index).is_none_or(|o| o.pending.is_some());
        if take_pending {
            return;
        }
        let Some(PassEligibility::Eligible { slot, settings }) =
            self.cfg.passthrough.get(index).cloned()
        else {
            return;
        };
        match (phase, key) {
            (Phase::Opening { deadline, .. }, _) => {
                if tick >= deadline {
                    // The result, if it ever comes, is reaped.
                    if let Some(c) = self.pass.candidates.get_mut(pos) {
                        c.phase = Phase::Metering;
                        c.retry_at = tick.saturating_add(PASS_RETRY_TICKS);
                    }
                    self.dirty = true;
                }
            }
            (Phase::Metering, None) => {
                if tick < retry_at {
                    return;
                }
                let source = self.pass.candidates.get(pos).map(|c| c.source.clone());
                let Some(source) = source else { return };
                let id = self.next_open;
                self.next_open = self.next_open.wrapping_add(1);
                let request = OpenRequest {
                    id,
                    settings,
                    bus: PictureBus::Source(source.clone()),
                    reformat: None,
                    passthrough: Some(PassRequest {
                        source,
                        settings,
                        slot,
                    }),
                };
                let c = self.pass.candidates.get_mut(pos);
                match (self.open_tx.try_send(request), c) {
                    (Ok(()), Some(c)) => {
                        c.phase = Phase::Opening {
                            id,
                            deadline: tick.saturating_add(PASS_OPEN_DEADLINE_TICKS),
                        };
                        self.dirty = true;
                    }
                    (Err(TrySendError::Full(_) | TrySendError::Disconnected(_)), Some(c)) => {
                        c.retry_at = tick.saturating_add(PASS_RETRY_TICKS);
                    }
                    _ => {}
                }
            }
            (Phase::Metering, Some((key, status, seen))) => {
                if tick < retry_at || !seen || status.verdict() != Some(Ok(())) {
                    return;
                }
                if self.request_pass_move(index, key, &status, &slot, tick, dispatcher) {
                    if let Some(c) = self.pass.candidates.get_mut(pos) {
                        c.phase = Phase::Moving;
                    }
                }
            }
            (Phase::On, Some((_, status, _))) => {
                let slate = status.state() == PassthroughState::Slate;
                if !slate {
                    if bridge == Bridge::Refused {
                        if let Some(c) = self.pass.candidates.get_mut(pos) {
                            c.bridge = Bridge::None;
                        }
                        self.dirty = true;
                    }
                    return;
                }
                let param_fault = matches!(
                    status.reason(),
                    Some(BridgeReason::ParameterChange | BridgeReason::ParametersNoLongerMatch)
                ) || status.latest_gen() != status.run_gen();
                if let Some(c) = self.pass.candidates.get_mut(pos) {
                    if !c.param_fault && param_fault {
                        c.param_fault = true;
                    }
                }
                self.advance_bridge(pos, tick, dispatcher);
            }
            (Phase::Away, Some((key, status, _))) => {
                let param_fault = self.pass.candidates.get(pos).is_some_and(|c| c.param_fault)
                    || status.latest_gen() != status.run_gen();
                if tick < retry_at
                    || status.stable_ticks() < return_ticks(param_fault)
                    || status.verdict() != Some(Ok(()))
                {
                    return;
                }
                if self.request_pass_move(index, key, &status, &slot, tick, dispatcher) {
                    if let Some(c) = self.pass.candidates.get_mut(pos) {
                        c.phase = Phase::Returning;
                        c.param_fault = false;
                    }
                }
            }
            (Phase::Moving | Phase::Returning, Some((key, status, _))) => {
                // A move superseded without an outcome (a take re-asserted
                // the output's key): back to waiting, the run dropped.
                let on = self.outputs.get(index).is_some_and(|o| o.key == key);
                if !on && dispatcher.pending_video_move(index) != Some(key) {
                    status.allow_runs_from(u64::MAX);
                    status.request_rearm();
                    if let Some(c) = self.pass.candidates.get_mut(pos) {
                        c.phase = if phase == Phase::Moving {
                            Phase::Metering
                        } else {
                            Phase::Away
                        };
                        c.retry_at = tick.saturating_add(PASS_RETRY_TICKS);
                    }
                    self.dirty = true;
                }
            }
            (Phase::Failed, _) | (_, None) => {}
        }
    }

    /// Let the key start a run from the next tick and ask the dispatcher for
    /// an unforced move onto it at its run-start IDR. `true` when requested.
    fn request_pass_move(
        &mut self,
        index: usize,
        key: usize,
        status: &PassthroughStatus,
        slot: &SlotSpec,
        tick: u64,
        dispatcher: &mut RenditionDispatcher,
    ) -> bool {
        let at = tick.saturating_add(1);
        // The run starts at the first strict IDR at or after `at`, whose
        // decode waits the clock's target depth: within one source GOP.
        let windows = u32::try_from(slot.max_gop_ticks)
            .unwrap_or(u32::MAX)
            .saturating_add(
                u32::try_from(multiview_output::passthrough::clock::TARGET_DEPTH).unwrap_or(3),
            )
            .saturating_add(3);
        if dispatcher.rebind_video(index, key, at, windows).is_err() {
            return false;
        }
        status.request_rearm();
        status.allow_runs_from(at);
        self.dirty = true;
        true
    }

    /// The output reads its key and the key bridges on its slate: bring in
    /// the on-demand picture bridge (the encoded source key), charged to
    /// the principal that patched the output.
    #[allow(clippy::too_many_lines)]
    // reason: one bridge state machine (open, warm, move, reuse, admission).
    fn advance_bridge(&mut self, pos: usize, tick: u64, dispatcher: &mut RenditionDispatcher) {
        let Some(c) = self.pass.candidates.get(pos) else {
            return;
        };
        let (index, bridge, retry_at, source) = (c.output, c.bridge, c.retry_at, c.source.clone());
        let bus = PictureBus::Source(source);
        let Some(out) = self.outputs.get(index) else {
            return;
        };
        let (settings, reset) = (out.settings, out.reset.clone());
        let principal = out
            .principal
            .clone()
            .unwrap_or_else(|| super::BOOT_PRINCIPAL.to_owned());
        let (_, format) = self.feed_key(index, &bus);
        match bridge {
            Bridge::Opening(id) => {
                if self.opens.get(&id).is_none_or(|o| tick >= o.deadline) {
                    self.opens.remove(&id);
                    if let Some(c) = self.pass.candidates.get_mut(pos) {
                        c.bridge = Bridge::None;
                        c.retry_at = tick.saturating_add(PASS_RETRY_TICKS);
                    }
                }
            }
            Bridge::Warming(key) => {
                let seen = self
                    .dynamic
                    .iter()
                    .find(|d| d.key == key)
                    .map(|d| d.seen_event);
                match seen {
                    Some(true) => self.move_to_bridge(pos, index, key, tick, dispatcher),
                    Some(false) => {}
                    None => {
                        if let Some(c) = self.pass.candidates.get_mut(pos) {
                            c.bridge = Bridge::None;
                        }
                    }
                }
            }
            Bridge::Moving(key) => {
                let on = self.outputs.get(index).is_some_and(|o| o.key == key);
                if !on && dispatcher.pending_video_move(index) != Some(key) {
                    if let Some(c) = self.pass.candidates.get_mut(pos) {
                        c.bridge = Bridge::None;
                        c.retry_at = tick.saturating_add(PASS_RETRY_TICKS);
                    }
                }
            }
            Bridge::None | Bridge::Refused => {
                if tick < retry_at {
                    return;
                }
                // Reuse the encoded key (still lingering, or shared).
                let existing = self
                    .dynamic
                    .iter()
                    .find(|d| {
                        d.bus == bus
                            && d.format == format
                            && d.settings == settings
                            && d.reset == reset
                            && !d.reformat
                            && !dispatcher.is_key_failed(d.key)
                    })
                    .map(|d| d.key);
                if let Some(key) = existing {
                    if let Some(d) = self.dynamic.iter_mut().find(|d| d.key == key) {
                        d.linger_deadline = None;
                    }
                    let warm = self.dynamic.iter().any(|d| d.key == key && d.seen_event);
                    if warm {
                        self.move_to_bridge(pos, index, key, tick, dispatcher);
                    } else if let Some(c) = self.pass.candidates.get_mut(pos) {
                        c.bridge = Bridge::Warming(key);
                    }
                    return;
                }
                while !self.has_capacity(&principal) {
                    if !self.close_oldest_linger(dispatcher, None) {
                        break;
                    }
                }
                let refused = |router: &mut Self| {
                    if let Some(c) = router.pass.candidates.get_mut(pos) {
                        if c.bridge != Bridge::Refused {
                            router.pass.counters.bridges_refused =
                                router.pass.counters.bridges_refused.saturating_add(1);
                        }
                        c.bridge = Bridge::Refused;
                        c.retry_at = tick.saturating_add(PASS_RETRY_TICKS);
                    }
                    // The key returns by itself on the slate (D1): runs allowed.
                    if let Some(k) = router.pass.candidates.get(pos).and_then(|c| c.key.as_ref()) {
                        k.status.allow_runs_from(0);
                    }
                    router.dirty = true;
                };
                if !self.has_capacity(&principal) {
                    refused(self);
                    return;
                }
                let id = self.next_open;
                self.next_open = self.next_open.wrapping_add(1);
                match self.open_tx.try_send(OpenRequest {
                    id,
                    settings,
                    bus: bus.clone(),
                    reformat: reset.clone(),
                    passthrough: None,
                }) {
                    Ok(()) => {
                        self.opens.insert(
                            id,
                            PendingOpen {
                                bus,
                                format,
                                settings,
                                reset,
                                reformat: false,
                                principal,
                                deadline: tick.saturating_add(OPEN_DEADLINE_TICKS),
                                waiter: None,
                                bridge_for: Some(index),
                            },
                        );
                        if let Some(c) = self.pass.candidates.get_mut(pos) {
                            c.bridge = Bridge::Opening(id);
                        }
                        self.dirty = true;
                    }
                    Err(_) => refused(self),
                }
            }
        }
    }

    /// A forced move onto the warm bridge `key` (an IDR within the floor).
    fn move_to_bridge(
        &mut self,
        pos: usize,
        index: usize,
        key: usize,
        tick: u64,
        dispatcher: &mut RenditionDispatcher,
    ) {
        // A bridge key nobody reads yet (just opened, or lingering): an IDR
        // on it costs no viewer anything, so it skips the force floor like
        // a take's warm-up; a shared key keeps the floor.
        let (at, windows) = if self.subscribers(key) == 0 {
            let at = tick.saturating_add(1);
            if let Some(d) = self.dynamic.iter().find(|d| d.key == key) {
                d.force_at.store(at, Ordering::Release);
            }
            self.last_forced.insert(key, at);
            (at, self.cfg.gop.saturating_add(3))
        } else {
            (tick, self.request_idr(key, tick))
        };
        let ok = dispatcher
            .rebind_video_forced(index, key, at, windows)
            .is_ok();
        if let Some(c) = self.pass.candidates.get_mut(pos) {
            if ok {
                c.bridge = Bridge::Moving(key);
            } else {
                c.bridge = Bridge::None;
                c.retry_at = tick.saturating_add(PASS_RETRY_TICKS);
            }
        }
        self.dirty = true;
    }
}

/// Longest failure reason kept, in bytes.
const MAX_REASON_BYTES: usize = 160;

/// `text` cut to at most [`MAX_REASON_BYTES`] at a char boundary (an open
/// error is arbitrary text; `String::truncate` inside a character panics).
fn bounded_reason(text: &str) -> String {
    let mut end = text.len().min(MAX_REASON_BYTES);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text.get(..end).unwrap_or_default().to_owned()
}

fn evaluate_traits_ok(traits: Option<SourceTraits>, slot: &SlotSpec) -> Result<(), MatchMiss> {
    let Some(traits) = traits else {
        return Err(MatchMiss::NotEnoughHistory);
    };
    evaluate_traits(&traits, slot)
}
