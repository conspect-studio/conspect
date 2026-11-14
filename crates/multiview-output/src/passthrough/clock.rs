//! The passthrough clock (output-routing §12.5, ADR-0110 decision 5).
//!
//! One [`PassthroughClock`] per passthrough slot, owned by that output's clock
//! thread. It is single-threaded, takes no locks, never waits and allocates
//! nothing per tick (the jitter buffer is preallocated; an emitted AU is an
//! `Arc` clone). On every tick [`PassthroughClock::on_tick`] returns exactly one
//! [`Emission`]:
//!
//! * [`Emission::Au`]: the queued source AU whose rebased DTS is this tick;
//! * [`Emission::Repeat`]: a synthesised skip picture (drift correction for a
//!   slow source, bytes from the caller's [`RepeatSource`]);
//! * [`Emission::Bridge`]: the caller emits its bridge AU (picture bridge or the
//!   [`SlateLoop`] slate) for this tick.
//!
//! ## Time
//!
//! The output time base is `1/fps`, so a rebased DTS **is** the tick index.
//! A source timestamp converts exactly (checked i128) **relative to a
//! per-stream epoch** (the raw DTS of the first AU, re-anchored after a
//! malformed AU, a time-base change or a DTS off the current epoch's grid):
//! `(ts - epoch) * tb.num * fps.num / (tb.den * fps.den)` must divide evenly
//! (else the PTS / AU is off grid: never emitted, a stall at its tick), and
//! its magnitude must stay within [`MAX_ABS_TICKS`] (else malformed). AUs of
//! an older epoch never mix with a run of a newer one. A run fixes one integer
//! `offset`: `dts' = dts_ticks + offset`, `pts' = pts_ticks + offset + extra`,
//! so raw deltas and the B-frame reorder pass through unchanged. Every
//! offset/rebase sum is checked; an overflow ends the run as malformed.
//!
//! ## Presentation delay across bridges
//!
//! After a run with B-frames, pictures already emitted may present after the
//! tick on which the run ends. The bridge then carries one constant delay
//! (`pts = tick + bridge_delay`, `bridge_delay = max(0, max_emitted_pts + 1 -
//! t)`), fixed when the run ends; a slate is forward-predicted, so a constant
//! delay is a valid stream. A new run adds the smallest constant `extra` to its
//! PTS' so its IDR presents after everything emitted before it (bounded by
//! [`MAX_PRESENTATION_DELAY`]; beyond it the slot stays bridged until
//! [`PassthroughClock::reset_timeline`] at a signalled discontinuity).
//!
//! ## The jitter buffer
//!
//! A `VecDeque` of at most [`JITTER_MAX`] AUs (8 ticks at one AU per tick);
//! a new AU beyond that drops the **oldest** (counted). A drop inside a run
//! ends the run (a dropped reference AU breaks decode). A run starts with
//! [`TARGET_DEPTH`] ticks of depth: its first IDR gets `dts' = max(tick,
//! arrival_tick + TARGET_DEPTH)`, bridge AUs filling the wait.
//!
//! **Depth** at tick `t` is the number of queued AUs with `dts' >= t`, counted
//! before this tick's emission (so the AU due now counts; healthy is 3).
//!
//! ## Bridging and return
//!
//! A run ends, on the same tick, on: a stall (no AU with `dts' == t`, or an
//! off-grid AU at the head), a late AU (`dts' < t`), a parameter change (the
//! due AU's `params_gen` differs from the run's), a jitter-buffer drop, or a
//! malformed source (non-monotonic DTS, time-base change, unrepresentable
//! timestamp). Return needs, in order: the slot armed; enough consecutive
//! **on-time** ticks (30 after a stall-class fault, 300 after a parameter
//! change, none for the first run after [`PassthroughClock::arm`]); the slot
//! gate's `matches(gen)` and `matched_slate_ready(gen)`; and a strict IDR at the
//! head of the queue. An IDR whose `params_gen` differs from the previous
//! run's always needs the 300-tick wait, even when the change happened inside
//! a stall.
//!
//! **On-time tick** (precise): the clock keeps a *probe* over arriving AUs.
//! The first AU after a re-anchor fixes a virtual offset that would put it
//! [`TARGET_DEPTH`] ticks after its arrival tick. Each later AU must continue
//! the source grid (on grid, DTS exactly one output tick after the previous
//! AU's, same `params_gen`), else the probe re-anchors on it and the count
//! restarts at zero. Tick `t` is on-time when the newest probed AU's virtual
//! rebased DTS is `>= t` (a run started at the anchor would have depth >= 1 at
//! `t`); a tick that is not on-time drops the probe and resets the count. So a
//! stalled, gapped, VFR, re-timed or parameter-changing source never counts.
//!
//! ## Cadence drift
//!
//! * Too deep (depth > [`DEEP_DEPTH`]): when a strict IDR is queued with at
//!   least one AU before it, the drop set is the decode-order suffix before the
//!   IDR starting at the queued AU with the maximum PTS among those after the
//!   previous strict IDR (for a source without B-frames, just the AU before the
//!   IDR). With `m` = its size, the IDR and everything after it shift by `-m`
//!   ticks (a new run at that IDR), only if every remaining AU before the IDR,
//!   emitted or queued, presents before the IDR's new PTS. Otherwise it waits
//!   for the next IDR.
//! * Too shallow: when the AU due now is a strict IDR (not a run start) and it
//!   is the only queued AU (depth <= 1), emit a [`Emission::Repeat`] on this
//!   tick and the IDR on the next (offset + 1). Never for a run with reordering
//!   (any `pts' != dts'`): its underrun stalls and bridges. When the
//!   [`RepeatSource`] returns `None` the IDR is emitted on time and no
//!   correction happens; an actual underrun then stalls and bridges on its
//!   own tick (bridging pre-emptively would only cost the 30-tick return).
//! * One correction per IDR at most.

use std::collections::VecDeque;
use std::sync::Arc;

use multiview_core::time::Rational;

use super::tap::{TapAu, MAX_AU_BYTES};

/// Target jitter-buffer depth at a run start, in ticks.
pub const TARGET_DEPTH: u64 = 3;

/// Jitter-buffer bound in AUs (one AU per tick: 8 ticks).
pub const JITTER_MAX: usize = 8;

/// Depth above which a fast source is corrected at the next IDR.
pub const DEEP_DEPTH: usize = 5;

/// Consecutive on-time ticks needed to return after a stall-class fault.
pub const STALL_RETURN_TICKS: u32 = 30;

/// Consecutive on-time ticks with unchanged parameters needed after a
/// parameter change.
pub const PARAM_RETURN_TICKS: u32 = 300;

/// Largest converted timestamp magnitude, in output ticks (2^62). Anything
/// larger is a malformed source; with this bound every offset / rebase sum
/// below stays far inside `i128` (|ticks| <= 2^62, ticks <= 2^64).
pub const MAX_ABS_TICKS: i128 = 1 << 62;

/// Largest constant presentation delay (PTS - DTS shift, in ticks) a run or a
/// bridge may carry after a B-frame run ended mid-GOP. Beyond it the clock
/// stays bridged ([`BridgeReason::PresentationDelay`]) until the caller resets
/// the timeline at a signalled discontinuity
/// ([`PassthroughClock::reset_timeline`]).
pub const MAX_PRESENTATION_DELAY: i128 = 8;

/// Most AUs the clock takes from its reader per tick. Anything beyond stays
/// in the reader's bounded channel for the next tick.
pub const MAX_DRAIN_PER_TICK: usize = 2 * JITTER_MAX;

/// Why a tick is bridged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BridgeReason {
    /// The slot is not in passthrough (never armed, or disarmed).
    NotArmed,
    /// Armed, waiting for a usable strict IDR.
    AwaitingIdr,
    /// A run is about to start; its first IDR waits [`TARGET_DEPTH`] ticks.
    Starting,
    /// No AU for the tick (or an off-grid AU at the head).
    Stall,
    /// An AU arrived after its tick.
    Late,
    /// The jitter buffer overflowed and dropped an AU.
    JitterOverflow,
    /// The source's parameter sets changed.
    ParameterChange,
    /// After a parameter change: the new parameters no longer match the slot.
    ParametersNoLongerMatch,
    /// The matched slate for the source's parameters is not ready.
    SlateNotReady,
    /// The source's timestamps are malformed.
    Malformed,
    /// A new run would need a presentation delay above
    /// [`MAX_PRESENTATION_DELAY`]; waits for [`PassthroughClock::reset_timeline`].
    PresentationDelay,
}

impl BridgeReason {
    /// Kebab-case reason for the readback.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NotArmed => "not-armed",
            Self::AwaitingIdr => "awaiting-idr",
            Self::Starting => "starting",
            Self::Stall => "stall",
            Self::Late => "late",
            Self::JitterOverflow => "jitter-overflow",
            Self::ParameterChange => "parameter-change",
            Self::ParametersNoLongerMatch => "parameters-no-longer-match",
            Self::SlateNotReady => "slate-not-ready",
            Self::Malformed => "malformed",
            Self::PresentationDelay => "presentation-delay",
        }
    }
}

/// What the clock emits on one tick. Exactly one per [`PassthroughClock::on_tick`].
#[derive(Debug, Clone)]
pub enum Emission {
    /// A source access unit, rebased.
    Au {
        /// The AU bytes, unchanged.
        data: Arc<[u8]>,
        /// Rebased DTS in output ticks (equals the tick).
        dts: i64,
        /// Rebased PTS in output ticks.
        pts: i64,
        /// Whether the AU is a strict IDR.
        keyframe: bool,
        /// The AU's parameter-set generation.
        params_gen: u64,
        /// The first AU of a run entered from a bridge.
        run_start: bool,
    },
    /// A synthesised repeat (skip) picture; `dts == tick`, `pts == tick +
    /// the run's constant presentation delay` (0 for a run entered without
    /// one).
    Repeat {
        /// The repeat AU bytes from the [`RepeatSource`].
        data: Arc<[u8]>,
        /// DTS (the tick).
        dts: i64,
        /// PTS.
        pts: i64,
    },
    /// The caller emits its bridge AU for this tick: `dts == tick`,
    /// `pts == tick + delay` with a delay that is constant for the whole bridge
    /// (non-zero only after a B-frame run, so a bridge picture never presents
    /// at or before a picture already emitted). A bridge AU must carry no
    /// reordering (the slate is forward-predicted).
    Bridge {
        /// DTS (the tick).
        dts: i64,
        /// PTS (the tick plus the bridge's presentation delay).
        pts: i64,
        /// Why.
        reason: BridgeReason,
    },
}

impl Emission {
    /// The emission's DTS.
    #[must_use]
    pub fn dts(&self) -> i64 {
        match self {
            Self::Au { dts, .. } | Self::Repeat { dts, .. } | Self::Bridge { dts, .. } => *dts,
        }
    }
}

/// Synthesises the repeat AU after `prev` (the last emitted AU). `None` when
/// unsupported (the clock then does not correct; see the module docs).
///
/// The implementation must build from the parameter sets of `prev.params_gen`
/// (for H.264: `multiview_ffmpeg::h264_repeat::RepeatAuBuilder`, built once per
/// generation); an HEVC source returns `None` in slice D (its underrun stalls
/// and bridges).
pub trait RepeatSource {
    /// The repeat AU's bytes, valid after `prev`.
    fn repeat_after(&mut self, prev: &TapAu) -> Option<Arc<[u8]>>;
}

/// The slot-side conditions for (re)entering passthrough.
pub trait SlotGate {
    /// Whether parameter generation `gen` matches the slot (§12.5 match rules).
    fn matches(&self, gen: u64) -> bool;
    /// Whether a matched slate baked from generation `gen` is ready.
    fn matched_slate_ready(&self, gen: u64) -> bool;
}

/// Clock counters.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ClockCounters {
    /// Source AUs emitted.
    pub emitted: u64,
    /// Repeat AUs emitted.
    pub repeats: u64,
    /// AUs dropped by too-deep drift corrections.
    pub drift_drops: u64,
    /// Too-deep drift corrections (each drops `>= 1` AU).
    pub drift_corrections: u64,
    /// Late AUs discarded.
    pub late: u64,
    /// Runs ended by a stall.
    pub stalls: u64,
    /// Runs ended by a parameter change.
    pub param_changes: u64,
    /// AUs dropped by the jitter-buffer bound.
    pub jitter_drops: u64,
    /// Runs ended by a malformed source.
    pub malformed: u64,
    /// Ticks emitted as bridge.
    pub bridge_ticks: u64,
    /// Runs started from a bridge.
    pub runs: u64,
}

/// Readback of the clock's path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClockState {
    /// Passing source AUs through (path P0).
    Passthrough,
    /// Bridging, with the reason.
    Bridging(BridgeReason),
}

/// One queued AU with its exact source-tick conversion.
#[derive(Debug, Clone)]
struct Entry {
    au: TapAu,
    /// Monotonic arrival sequence number.
    seq: u64,
    /// The timestamp epoch generation `dts_t` / `pts_t` are relative to.
    epoch: u64,
    /// DTS in output ticks, `None` when off grid.
    dts_t: Option<i128>,
    /// PTS in output ticks, `None` when off grid.
    pts_t: Option<i128>,
    /// Arrival tick on the output clock.
    arrival_tick: i128,
}

/// The bridge-side stability probe (see "On-time tick" in the module docs).
#[derive(Debug, Clone, Copy)]
struct Probe {
    virtual_offset: i128,
    last_dts_t: i128,
    gen: u64,
    epoch: u64,
}

/// One exact conversion result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Conv {
    /// On the output grid.
    Tick(i128),
    /// Not a whole number of output ticks.
    OffGrid,
    /// Arithmetic overflow, a degenerate rate / time base, or beyond
    /// [`MAX_ABS_TICKS`]: a malformed source.
    Overflow,
}

#[derive(Debug, Clone, Copy)]
enum State {
    Bridge {
        reason: BridgeReason,
        /// On-time ticks needed before a return.
        required: u32,
        /// Whether the return also needs the parameters re-validated.
        param_fault: bool,
    },
    Run {
        offset: i128,
        start_tick: i128,
        gen: u64,
        /// The timestamp epoch generation of the run's AUs.
        epoch: u64,
        /// The run's constant presentation delay (added to every PTS'), so
        /// its first IDR presents after everything emitted before it.
        extra: i128,
        /// A pending new run at a drift-corrected IDR: (IDR seq, new offset).
        rebase: Option<(u64, i128)>,
        /// Any emitted `pts' != dts'` in this run.
        reorder: bool,
        /// Max emitted PTS' in this run.
        max_pts: Option<i128>,
        /// Seq of the last IDR a correction was applied or considered at.
        corrected_seq: Option<u64>,
        /// Whether the next emitted AU is the run's first.
        first_pending: bool,
    },
}

/// The per-slot passthrough clock. See the [module docs](self).
#[derive(Debug)]
pub struct PassthroughClock {
    fps: Rational,
    armed: bool,
    state: State,
    queue: VecDeque<Entry>,
    next_seq: u64,
    /// Last AU received (raw dts, time base) for the monotonic check.
    last_rx: Option<(i64, Rational)>,
    probe: Option<Probe>,
    stable: u32,
    /// A fault detected while ingesting, applied at this tick's decision.
    pending_fault: Option<BridgeReason>,
    last_emitted: Option<TapAu>,
    /// Depth used by the last run decision (0 when bridged).
    last_depth: usize,
    /// Parameter generation of the most recent run (reset by `arm`).
    last_run_gen: Option<u64>,
    /// The source timestamp epoch (raw DTS) and its generation: converted
    /// ticks are `(ts - epoch) * tb * fps` (R1-M1: an absolute source
    /// timestamp need not be a whole number of frame periods).
    epoch_ts: Option<i64>,
    epoch_gen: u64,
    /// The bridge's presentation delay (never decreases while bridged).
    bridge_delay: i128,
    /// The highest PTS' emitted so far (every emission kind).
    max_emitted_pts: Option<i128>,
    counters: ClockCounters,
}

impl PassthroughClock {
    /// A disarmed clock for an output at `fps` (exact rational).
    #[must_use]
    pub fn new(fps: Rational) -> Self {
        Self {
            fps,
            armed: false,
            state: State::Bridge {
                reason: BridgeReason::NotArmed,
                required: 0,
                param_fault: false,
            },
            queue: VecDeque::with_capacity(JITTER_MAX + 1),
            next_seq: 0,
            last_rx: None,
            probe: None,
            stable: 0,
            pending_fault: None,
            last_emitted: None,
            last_depth: 0,
            last_run_gen: None,
            epoch_ts: None,
            epoch_gen: 0,
            bridge_delay: 0,
            max_emitted_pts: None,
            counters: ClockCounters::default(),
        }
    }

    /// The current bridge presentation delay in ticks.
    #[must_use]
    pub fn bridge_delay(&self) -> i64 {
        i64::try_from(self.bridge_delay).unwrap_or(i64::MAX)
    }

    /// Forget the presentation history: the next bridge and run start with
    /// no presentation delay. Call ONLY when the output signals a timeline
    /// discontinuity at the next emitted IDR (an HLS `EXT-X-DISCONTINUITY`
    /// / TS `discontinuity_indicator`), e.g. when [`BridgeReason::PresentationDelay`]
    /// holds the slot bridged.
    pub fn reset_timeline(&mut self) {
        self.bridge_delay = 0;
        self.max_emitted_pts = None;
    }

    /// Take the slot into passthrough: the first strict IDR that passes the
    /// gate starts a run (no stability wait for the first run).
    pub fn arm(&mut self) {
        if self.armed {
            return;
        }
        self.armed = true;
        self.last_run_gen = None;
        self.state = State::Bridge {
            reason: BridgeReason::AwaitingIdr,
            required: 0,
            param_fault: false,
        };
    }

    /// Take the slot out of passthrough (bridges from the next tick).
    pub fn disarm(&mut self) {
        self.armed = false;
        self.state = State::Bridge {
            reason: BridgeReason::NotArmed,
            required: 0,
            param_fault: false,
        };
        self.queue.clear();
    }

    /// Whether the slot is armed.
    #[must_use]
    pub fn is_armed(&self) -> bool {
        self.armed
    }

    /// The counters.
    #[must_use]
    pub fn counters(&self) -> ClockCounters {
        self.counters
    }

    /// Queued AUs (bounded by [`JITTER_MAX`]).
    #[must_use]
    pub fn queued(&self) -> usize {
        self.queue.len()
    }

    /// The depth (module docs) the last tick's run decision saw; 0 when the
    /// last tick was decided in the bridge state.
    #[must_use]
    pub fn last_depth(&self) -> usize {
        self.last_depth
    }

    /// Consecutive on-time ticks observed by the bridge probe.
    #[must_use]
    pub fn stable_ticks(&self) -> u32 {
        self.stable
    }

    /// The path readback.
    #[must_use]
    pub fn state(&self) -> ClockState {
        match self.state {
            State::Bridge { reason, .. } => ClockState::Bridging(reason),
            State::Run {
                first_pending: true,
                ..
            } => ClockState::Bridging(BridgeReason::Starting),
            State::Run { .. } => ClockState::Passthrough,
        }
    }

    /// Depth at `tick`: queued on-grid AUs with `dts' >= tick` (0 while bridged).
    #[must_use]
    pub fn depth(&self, tick: u64) -> usize {
        let t = i128::from(tick);
        match self.state {
            State::Run {
                offset,
                rebase,
                epoch,
                ..
            } => self.depth_at(t, offset, rebase, epoch),
            State::Bridge { .. } => 0,
        }
    }

    /// One output tick: drain up to [`MAX_DRAIN_PER_TICK`] AUs from `aus`
    /// (non-blocking: pass the reader's `drain()`), then emit exactly one
    /// [`Emission`] stamped for `tick`. `now_ns` is this tick's time on the
    /// shared monotonic clock (it places each AU's `arrival_ns` on the tick
    /// grid).
    ///
    /// A `tick` above `i64::MAX` saturates the emitted stamps; that is
    /// unreachable (2^63 ticks is about 4.8e9 years at 60 fps).
    pub fn on_tick(
        &mut self,
        tick: u64,
        now_ns: u64,
        aus: impl Iterator<Item = TapAu>,
        gate: &dyn SlotGate,
        repeat: &mut dyn RepeatSource,
    ) -> Emission {
        let t = i128::from(tick);
        self.last_depth = 0;
        for au in aus.take(MAX_DRAIN_PER_TICK) {
            self.ingest(t, now_ns, au);
        }
        let emission = self.decide(t, gate, repeat);
        let pts = match &emission {
            Emission::Bridge { pts, .. } => {
                self.counters.bridge_ticks = self.counters.bridge_ticks.saturating_add(1);
                *pts
            }
            Emission::Au { pts, .. } | Emission::Repeat { pts, .. } => *pts,
        };
        let pts = i128::from(pts);
        self.max_emitted_pts = Some(self.max_emitted_pts.map_or(pts, |m| m.max(pts)));
        emission
    }

    /// Exact output ticks of a source-relative timestamp `rel` (time base
    /// `tb`). See [`Conv`].
    fn convert(&self, rel: i128, tb: Rational) -> Conv {
        if tb.num <= 0 || tb.den <= 0 || self.fps.num <= 0 || self.fps.den <= 0 {
            return Conv::Overflow;
        }
        let Some(num) = rel
            .checked_mul(i128::from(tb.num))
            .and_then(|v| v.checked_mul(i128::from(self.fps.num)))
        else {
            return Conv::Overflow;
        };
        let Some(den) = i128::from(tb.den).checked_mul(i128::from(self.fps.den)) else {
            return Conv::Overflow;
        };
        if num % den != 0 {
            return Conv::OffGrid;
        }
        let ticks = num / den;
        if ticks.checked_abs().is_none_or(|a| a > MAX_ABS_TICKS) {
            return Conv::Overflow;
        }
        Conv::Tick(ticks)
    }

    /// Output ticks of `ts` (relative to a zero epoch) in time base `tb`, or
    /// `None` when off grid, overflowing or degenerate.
    #[cfg(test)]
    fn to_ticks(&self, ts: i64, tb: Rational) -> Option<i128> {
        match self.convert(i128::from(ts), tb) {
            Conv::Tick(t) => Some(t),
            Conv::OffGrid | Conv::Overflow => None,
        }
    }

    /// The output tick on which an AU that arrived at `arrival_ns` landed,
    /// given that `tick` began at `now_ns`. Saturating: an absurd age lands
    /// at tick 0.
    fn arrival_tick(&self, t: i128, now_ns: u64, arrival_ns: u64) -> i128 {
        let ago_ns = i128::from(now_ns.saturating_sub(arrival_ns));
        let num = i128::from(self.fps.num.max(1));
        let den = i128::from(self.fps.den.max(1)).saturating_mul(1_000_000_000);
        let ago_ticks = ago_ns.checked_mul(num).map_or(t, |v| v / den);
        t.saturating_sub(ago_ticks).max(0)
    }

    /// Start a new timestamp epoch at raw DTS `dts`.
    fn re_epoch(&mut self, dts: i64) {
        self.epoch_ts = Some(dts);
        self.epoch_gen = self.epoch_gen.wrapping_add(1);
    }

    /// A malformed AU: drop the queue, end a run, restart the epoch on it.
    fn malformed(&mut self, dts: i64) {
        self.queue.clear();
        if matches!(self.state, State::Run { .. }) {
            self.pending_fault = Some(BridgeReason::Malformed);
        }
        self.probe = None;
        self.stable = 0;
        self.re_epoch(dts);
    }

    fn ingest(&mut self, t: i128, now_ns: u64, au: TapAu) {
        // R1-A5: a byte budget per AU (the tap enforces it too).
        if au.data.len() > MAX_AU_BYTES {
            if matches!(self.state, State::Run { .. }) {
                self.pending_fault = Some(BridgeReason::Malformed);
            }
            return;
        }
        let malformed = match self.last_rx {
            Some((dts, tb)) => tb != au.time_base || au.dts <= dts,
            None => false,
        };
        self.last_rx = Some((au.dts, au.time_base));
        if malformed {
            self.malformed(au.dts);
            return;
        }
        let epoch = if let Some(e) = self.epoch_ts {
            e
        } else {
            self.re_epoch(au.dts);
            au.dts
        };
        let rel = |ts: i64, e: i64| i128::from(ts) - i128::from(e);
        let dts_t = match self.convert(rel(au.dts, epoch), au.time_base) {
            Conv::Tick(d) => d,
            // Off the current epoch's grid (a timestamp jump): a new epoch
            // starts at this AU. In a run it is foreign (a stall when due).
            Conv::OffGrid => {
                self.re_epoch(au.dts);
                0
            }
            Conv::Overflow => {
                self.malformed(au.dts);
                return;
            }
        };
        let epoch = self.epoch_ts.unwrap_or(au.dts);
        let pts_t = match self.convert(rel(au.pts, epoch), au.time_base) {
            Conv::Tick(p) => Some(p),
            Conv::OffGrid => None,
            Conv::Overflow => {
                self.malformed(au.dts);
                return;
            }
        };
        let arrival_tick = self.arrival_tick(t, now_ns, au.arrival_ns);
        let entry = Entry {
            seq: self.next_seq,
            epoch: self.epoch_gen,
            dts_t: Some(dts_t),
            pts_t,
            arrival_tick,
            au,
        };
        self.next_seq = self.next_seq.wrapping_add(1);
        self.update_probe(&entry);

        match self.state {
            State::Bridge { .. } => {
                // Bridged: only a strict IDR can start a run, and only at the
                // head. Anything before one is useless; drop it on arrival.
                if self.queue.is_empty() && !entry.au.strict_idr {
                    return;
                }
                self.push_bounded(entry);
            }
            State::Run { .. } => self.push_bounded(entry),
        }
    }

    fn push_bounded(&mut self, entry: Entry) {
        self.queue.push_back(entry);
        while self.queue.len() > JITTER_MAX {
            self.queue.pop_front();
            self.counters.jitter_drops = self.counters.jitter_drops.saturating_add(1);
            if matches!(self.state, State::Run { .. }) && self.pending_fault.is_none() {
                self.pending_fault = Some(BridgeReason::JitterOverflow);
            }
        }
    }

    fn update_probe(&mut self, e: &Entry) {
        let continues = match (self.probe, e.dts_t) {
            (Some(p), Some(d)) => {
                p.epoch == e.epoch && d == p.last_dts_t + 1 && p.gen == e.au.params_gen
            }
            _ => false,
        };
        match (continues, e.dts_t) {
            (true, Some(d)) => {
                if let Some(p) = self.probe.as_mut() {
                    p.last_dts_t = d;
                }
            }
            (_, Some(d)) => {
                let Some(virtual_offset) = e
                    .arrival_tick
                    .checked_add(i128::from(TARGET_DEPTH))
                    .and_then(|v| v.checked_sub(d))
                else {
                    self.probe = None;
                    self.stable = 0;
                    return;
                };
                self.probe = Some(Probe {
                    virtual_offset,
                    last_dts_t: d,
                    gen: e.au.params_gen,
                    epoch: e.epoch,
                });
                self.stable = 0;
            }
            (_, None) => {
                self.probe = None;
                self.stable = 0;
            }
        }
    }

    /// Advance the on-time count for tick `t`.
    fn tick_probe(&mut self, t: i128) {
        match self.probe {
            Some(p)
                if p.last_dts_t
                    .checked_add(p.virtual_offset)
                    .is_some_and(|v| v >= t) =>
            {
                self.stable = self.stable.saturating_add(1);
            }
            _ => {
                self.probe = None;
                self.stable = 0;
            }
        }
    }

    /// A bridge emission for tick `t`: `pts = t + bridge_delay`.
    fn bridge(&self, t: i128, reason: BridgeReason) -> Emission {
        let tick = i64::try_from(t).unwrap_or(i64::MAX);
        let pts = t
            .checked_add(self.bridge_delay)
            .and_then(|p| i64::try_from(p).ok())
            .unwrap_or(i64::MAX);
        Emission::Bridge {
            dts: tick,
            pts,
            reason,
        }
    }

    /// End the current run with `reason` and bridge this tick. The bridge's
    /// presentation delay is fixed here, once, for the whole bridge: the
    /// smallest that keeps every bridge picture presenting after everything
    /// already emitted (R1-M2; 0 after a run without reordering).
    fn end_run(&mut self, t: i128, reason: BridgeReason) -> Emission {
        let (required, param_fault) = match reason {
            BridgeReason::ParameterChange => {
                self.counters.param_changes = self.counters.param_changes.saturating_add(1);
                (PARAM_RETURN_TICKS, true)
            }
            BridgeReason::Stall => {
                self.counters.stalls = self.counters.stalls.saturating_add(1);
                (STALL_RETURN_TICKS, false)
            }
            BridgeReason::Malformed => {
                self.counters.malformed = self.counters.malformed.saturating_add(1);
                (STALL_RETURN_TICKS, false)
            }
            _ => (STALL_RETURN_TICKS, false),
        };
        self.state = State::Bridge {
            reason,
            required,
            param_fault,
        };
        self.probe = None;
        self.stable = 0;
        self.pending_fault = None;
        self.bridge_delay = self
            .max_emitted_pts
            .and_then(|m| m.checked_add(1))
            .and_then(|m| m.checked_sub(t))
            .map_or(0, |d| d.max(0));
        self.bridge(t, reason)
    }

    fn decide(&mut self, t: i128, gate: &dyn SlotGate, repeat: &mut dyn RepeatSource) -> Emission {
        match self.state {
            State::Bridge { .. } => self.decide_bridge(t, gate, repeat),
            State::Run { .. } => self.decide_run(t, repeat),
        }
    }

    #[allow(clippy::too_many_lines)]
    fn decide_bridge(
        &mut self,
        t: i128,
        gate: &dyn SlotGate,
        repeat: &mut dyn RepeatSource,
    ) -> Emission {
        self.pending_fault = None;
        self.tick_probe(t);
        let State::Bridge {
            reason,
            required,
            param_fault,
        } = self.state
        else {
            return self.bridge(t, BridgeReason::NotArmed);
        };
        if !self.armed {
            self.queue.clear();
            return self.bridge(t, BridgeReason::NotArmed);
        }
        let mut shown = reason;
        while let Some(head) = self.queue.front() {
            let (Some(d), Some(p)) = (head.dts_t, head.pts_t) else {
                self.queue.pop_front();
                continue;
            };
            if !head.au.strict_idr {
                self.queue.pop_front();
                continue;
            }
            let gen = head.au.params_gen;
            // New parameters since the last run (a change hidden inside a
            // stall included) need the parameter-change wait and re-check.
            let gen_changed = self.last_run_gen.is_some_and(|g| g != gen);
            let required = if gen_changed {
                required.max(PARAM_RETURN_TICKS)
            } else {
                required
            };
            let param_fault = param_fault || gen_changed;
            if self.stable < required {
                self.queue.pop_front();
                continue;
            }
            if !gate.matches(gen) {
                if param_fault {
                    shown = BridgeReason::ParametersNoLongerMatch;
                }
                self.queue.pop_front();
                continue;
            }
            if !gate.matched_slate_ready(gen) {
                shown = BridgeReason::SlateNotReady;
                self.queue.pop_front();
                continue;
            }
            // Start a run at this IDR: its first AU decodes at `start`.
            let start = t.max(head.arrival_tick.saturating_add(i128::from(TARGET_DEPTH)));
            let Some(offset) = start.checked_sub(d) else {
                self.queue.pop_front();
                continue;
            };
            // R1-M2: the IDR must present after everything already emitted
            // and after the starting bridges still to come (t..start, each
            // presenting at tick + bridge_delay). A run after a B-frame run's
            // bridge takes the smallest constant delay that achieves it.
            let floor = if start > t {
                start
                    .checked_sub(1)
                    .and_then(|v| v.checked_add(self.bridge_delay))
            } else {
                None
            }
            .max(self.max_emitted_pts);
            let Some(idr_pts) = p.checked_add(offset) else {
                self.queue.pop_front();
                continue;
            };
            let extra = floor
                .and_then(|f| f.checked_add(1))
                .and_then(|f| f.checked_sub(idr_pts))
                .map_or(0, |e| e.max(0));
            if extra > MAX_PRESENTATION_DELAY {
                shown = BridgeReason::PresentationDelay;
                self.queue.pop_front();
                continue;
            }
            self.state = State::Run {
                offset,
                start_tick: start,
                gen,
                epoch: head.epoch,
                extra,
                rebase: None,
                reorder: false,
                max_pts: None,
                corrected_seq: None,
                first_pending: true,
            };
            self.counters.runs = self.counters.runs.saturating_add(1);
            self.last_run_gen = Some(gen);
            return self.decide_run(t, repeat);
        }
        if let State::Bridge { reason: r, .. } = &mut self.state {
            if matches!(
                shown,
                BridgeReason::ParametersNoLongerMatch
                    | BridgeReason::SlateNotReady
                    | BridgeReason::PresentationDelay
            ) {
                *r = shown;
            }
            shown = *r;
        }
        self.bridge(t, shown)
    }

    #[allow(clippy::too_many_lines)]
    fn decide_run(&mut self, t: i128, repeat: &mut dyn RepeatSource) -> Emission {
        if let Some(fault) = self.pending_fault.take() {
            return self.end_run(t, fault);
        }
        let State::Run {
            mut offset,
            start_tick,
            gen,
            epoch,
            extra,
            mut rebase,
            reorder,
            max_pts,
            mut corrected_seq,
            first_pending,
        } = self.state
        else {
            return self.bridge(t, BridgeReason::NotArmed);
        };
        if t < start_tick {
            return self.bridge(t, BridgeReason::Starting);
        }

        // Late AUs (dts' < t) are discarded and end the run. An AU from
        // another timestamp epoch (a jump) is foreign: it ends the run when
        // it reaches the head (below).
        let mut late = 0_u64;
        while let Some(head) = self.queue.front() {
            if head.epoch != epoch {
                break;
            }
            match head
                .dts_t
                .and_then(|d| d.checked_add(offset_for(offset, rebase, head.seq)))
            {
                Some(d) if d < t => {
                    self.queue.pop_front();
                    late += 1;
                }
                _ => break,
            }
        }
        if late > 0 {
            self.counters.late = self.counters.late.saturating_add(late);
            return self.end_run(t, BridgeReason::Late);
        }

        let depth = self.depth_at(t, offset, rebase, epoch);
        self.last_depth = depth;

        // Too deep: drop before a queued IDR (one correction per IDR).
        if depth > DEEP_DEPTH && rebase.is_none() && !first_pending {
            if let Some((idr_seq, new_offset, k, i)) =
                self.plan_deep(t, offset, epoch, extra, max_pts, corrected_seq)
            {
                let m = i - k;
                self.queue.drain(k..i);
                rebase = Some((idr_seq, new_offset));
                corrected_seq = Some(idr_seq);
                self.counters.drift_drops = self
                    .counters
                    .drift_drops
                    .saturating_add(u64::try_from(m).unwrap_or(u64::MAX));
                self.counters.drift_corrections = self.counters.drift_corrections.saturating_add(1);
            }
        }

        let Some(head) = self.queue.front() else {
            return self.end_run(t, BridgeReason::Stall);
        };
        if head.epoch != epoch {
            return self.end_run(t, BridgeReason::Stall);
        }
        let head_off = offset_for(offset, rebase, head.seq);
        let (Some(hd), Some(hp)) = (head.dts_t, head.pts_t) else {
            self.queue.pop_front();
            return self.end_run(t, BridgeReason::Stall);
        };
        let Some(due) = hd.checked_add(head_off) else {
            return self.end_run(t, BridgeReason::Malformed);
        };
        if due > t {
            return self.end_run(t, BridgeReason::Stall);
        }
        if head.au.params_gen != gen {
            return self.end_run(t, BridgeReason::ParameterChange);
        }

        // Too shallow: repeat before an IDR that is the only queued AU.
        if head.au.strict_idr
            && !first_pending
            && depth <= 1
            && !reorder
            && rebase.is_none()
            && corrected_seq != Some(head.seq)
        {
            let head_seq = head.seq;
            corrected_seq = Some(head_seq);
            let bytes = self
                .last_emitted
                .as_ref()
                .and_then(|prev| repeat.repeat_after(prev));
            if let (Some(data), Some(next_offset), Some(rpts)) =
                (bytes, offset.checked_add(1), t.checked_add(extra))
            {
                offset = next_offset;
                self.state = State::Run {
                    offset,
                    start_tick,
                    gen,
                    epoch,
                    extra,
                    rebase,
                    reorder,
                    max_pts: max_pts.max(Some(rpts)),
                    corrected_seq,
                    first_pending,
                };
                self.counters.repeats = self.counters.repeats.saturating_add(1);
                let tick = i64::try_from(t).unwrap_or(i64::MAX);
                let Ok(rpts) = i64::try_from(rpts) else {
                    return self.end_run(t, BridgeReason::Malformed);
                };
                return Emission::Repeat {
                    data,
                    dts: tick,
                    pts: rpts,
                };
            }
            // No repeat available: keep passing through; a real underrun
            // then stalls and bridges on its own tick.
        }

        let Some(entry) = self.queue.pop_front() else {
            return self.end_run(t, BridgeReason::Stall);
        };
        if let Some((seq, new_off)) = rebase {
            if entry.seq >= seq {
                offset = new_off;
                rebase = None;
            }
        }
        let (Some(dts), Some(pts)) = (
            hd.checked_add(offset),
            hp.checked_add(offset).and_then(|p| p.checked_add(extra)),
        ) else {
            return self.end_run(t, BridgeReason::Malformed);
        };
        let (Ok(dts64), Ok(pts64)) = (i64::try_from(dts), i64::try_from(pts)) else {
            return self.end_run(t, BridgeReason::Malformed);
        };
        if pts < dts {
            // A presentation before decode is a malformed source.
            return self.end_run(t, BridgeReason::Malformed);
        }
        self.state = State::Run {
            offset,
            start_tick,
            gen,
            epoch,
            extra,
            rebase,
            reorder: reorder || pts != dts.saturating_add(extra),
            max_pts: max_pts.max(Some(pts)),
            corrected_seq,
            first_pending: false,
        };
        self.counters.emitted = self.counters.emitted.saturating_add(1);
        let emission = Emission::Au {
            data: Arc::clone(&entry.au.data),
            dts: dts64,
            pts: pts64,
            keyframe: entry.au.strict_idr,
            params_gen: entry.au.params_gen,
            run_start: first_pending,
        };
        self.last_emitted = Some(entry.au);
        emission
    }

    fn depth_at(&self, t: i128, offset: i128, rebase: Option<(u64, i128)>, epoch: u64) -> usize {
        self.queue
            .iter()
            .filter(|e| {
                e.epoch == epoch
                    && e.dts_t
                        .and_then(|d| d.checked_add(offset_for(offset, rebase, e.seq)))
                        .is_some_and(|d| d >= t)
            })
            .count()
    }

    /// Plan a too-deep correction: `(idr_seq, new_offset, k, i)` with the drop
    /// set at queue indices `k..i` and the IDR at `i`. Every queued AU it
    /// looks at must be of the run's epoch; all arithmetic is checked (an
    /// overflow plans nothing).
    #[allow(clippy::many_single_char_names)] // t, k, i, m, p as in the docs
    fn plan_deep(
        &self,
        t: i128,
        offset: i128,
        epoch: u64,
        extra: i128,
        max_emitted_pts: Option<i128>,
        corrected_seq: Option<u64>,
    ) -> Option<(u64, i128, usize, usize)> {
        // The first queued strict IDR with at least one AU before it.
        let i = self
            .queue
            .iter()
            .enumerate()
            .skip(1)
            .find(|(_, e)| e.au.strict_idr)
            .map(|(i, _)| i)?;
        if self.queue.iter().take(i + 1).any(|e| e.epoch != epoch) {
            return None;
        }
        let idr = self.queue.get(i)?;
        if corrected_seq == Some(idr.seq) {
            return None;
        }
        // Only AUs after the previous strict IDR (queued) are candidates.
        let start = self
            .queue
            .iter()
            .take(i)
            .rposition(|e| e.au.strict_idr)
            .map_or(0, |p| p + 1);
        // The queued candidate with the maximum PTS (the earliest on a tie).
        let mut k = None;
        let mut k_pts = i128::MIN;
        for (j, e) in self.queue.iter().enumerate().take(i).skip(start) {
            let p = e.pts_t?;
            if k.is_none() || p > k_pts {
                k = Some(j);
                k_pts = p;
            }
        }
        let k = k?;
        let m = i128::try_from(i - k).ok()?;
        let new_offset = offset.checked_sub(m)?;
        let idr_decode_at = idr.dts_t?.checked_add(new_offset)?;
        let idr_present_at = idr.pts_t?.checked_add(new_offset)?.checked_add(extra)?;
        if idr_decode_at < t {
            return None;
        }
        // Every remaining AU before the IDR (emitted or queued) must present
        // before the IDR's new PTS.
        let mut remaining_max = max_emitted_pts;
        for e in self.queue.iter().take(k) {
            let p = e.pts_t?.checked_add(offset)?.checked_add(extra)?;
            remaining_max = remaining_max.max(Some(p));
        }
        if remaining_max.is_some_and(|p| p >= idr_present_at) {
            return None;
        }
        // The IDR takes the first dropped AU's tick: grid stays contiguous.
        let first_dropped = self.queue.get(k)?.dts_t?.checked_add(offset)?;
        if first_dropped != idr_decode_at {
            return None;
        }
        Some((idr.seq, new_offset, k, i))
    }
}

fn offset_for(offset: i128, rebase: Option<(u64, i128)>, seq: u64) -> i128 {
    match rebase {
        Some((s, o)) if seq >= s => o,
        _ => offset,
    }
}

/// One slate bridge AU for a tick.
#[derive(Debug, Clone)]
pub struct SlateAu {
    /// The slate packet bytes.
    pub data: Arc<[u8]>,
    /// DTS (the tick).
    pub dts: i64,
    /// PTS (the tick).
    pub pts: i64,
    /// The loop's first packet (the slate's IDR).
    pub keyframe: bool,
}

/// Loops a baked slate's IDR-led GOP at the output clock, `dts = pts = tick`.
///
/// The slate GOP must carry no reordering (a baked slate is all-forward
/// predicted). **`bridge_starts_at_idr`:** entering the slate bridge
/// ([`SlateLoop::enter`]) always restarts at the slate's IDR, so a bridge
/// entered mid-loop never starts on a P picture.
#[derive(Debug, Clone)]
pub struct SlateLoop {
    packets: Arc<[Arc<[u8]>]>,
    pos: usize,
}

impl SlateLoop {
    /// A loop over `packets` (the first one is the IDR). `None` when empty.
    #[must_use]
    pub fn new(packets: Arc<[Arc<[u8]>]>) -> Option<Self> {
        if packets.is_empty() {
            return None;
        }
        Some(Self { packets, pos: 0 })
    }

    /// Enter the slate bridge: the next packet is the slate's IDR.
    pub fn enter(&mut self) {
        self.pos = 0;
    }

    /// The slate AU for a bridge emission (`dts`, `pts` from
    /// [`Emission::Bridge`]: the bridge's constant presentation delay is
    /// honoured), advancing the loop. `None` when `pts < dts`.
    pub fn next_at(&mut self, dts: i64, pts: i64) -> Option<SlateAu> {
        if pts < dts {
            return None;
        }
        let data = Arc::clone(self.packets.get(self.pos)?);
        let keyframe = self.pos == 0;
        self.pos = (self.pos + 1) % self.packets.len();
        Some(SlateAu {
            data,
            dts,
            pts,
            keyframe,
        })
    }

    /// The slate AU for `tick` (no presentation delay), advancing the loop.
    pub fn next_for_tick(&mut self, tick: u64) -> Option<SlateAu> {
        let data = Arc::clone(self.packets.get(self.pos)?);
        let keyframe = self.pos == 0;
        self.pos = (self.pos + 1) % self.packets.len();
        let tick = i64::try_from(tick).ok()?;
        Some(SlateAu {
            data,
            dts: tick,
            pts: tick,
            keyframe,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Open;
    impl SlotGate for Open {
        fn matches(&self, _: u64) -> bool {
            true
        }
        fn matched_slate_ready(&self, _: u64) -> bool {
            true
        }
    }
    struct NoRepeat;
    impl RepeatSource for NoRepeat {
        fn repeat_after(&mut self, _: &TapAu) -> Option<Arc<[u8]>> {
            None
        }
    }

    fn au(i: i64, idr: bool) -> TapAu {
        TapAu {
            data: Arc::from(vec![u8::try_from(i % 256).unwrap()]),
            dts: i * 3003,
            pts: i * 3003,
            time_base: Rational::new(1, 90_000),
            strict_idr: idr,
            reference: true,
            params_gen: 1,
            arrival_ns: 0,
        }
    }

    #[test]
    fn exact_grid_conversion() {
        let c = PassthroughClock::new(Rational::FPS_29_97);
        let tb = Rational::new(1, 90_000);
        assert_eq!(c.to_ticks(3003 * 7, tb), Some(7));
        assert_eq!(c.to_ticks(3000, tb), None, "off grid");
        assert_eq!(c.to_ticks(-3003, tb), Some(-1));
        assert_eq!(c.to_ticks(1, Rational::new(0, 1)), None);
        assert_eq!(
            c.to_ticks(i64::MAX, Rational::new(i64::MAX, 1)),
            None,
            "overflow is off grid"
        );
    }

    #[test]
    fn run_waits_target_depth_then_one_per_tick() {
        let mut c = PassthroughClock::new(Rational::FPS_29_97);
        c.arm();
        let mut out = Vec::new();
        for tick in 0..20_u64 {
            let i = i64::try_from(tick).unwrap();
            let e = c.on_tick(
                tick,
                0,
                std::iter::once(au(i, i % 10 == 0)),
                &Open,
                &mut NoRepeat,
            );
            assert_eq!(e.dts(), i, "one emission stamped for the tick");
            out.push(e);
        }
        // IDR arrives at tick 0 and is emitted at tick 3 (TARGET_DEPTH).
        for e in &out[..3] {
            assert!(matches!(
                e,
                Emission::Bridge {
                    reason: BridgeReason::Starting,
                    ..
                }
            ));
        }
        assert!(matches!(
            out[3],
            Emission::Au {
                run_start: true,
                keyframe: true,
                ..
            }
        ));
        for e in &out[3..] {
            assert!(matches!(e, Emission::Au { .. }));
        }
        assert_eq!(c.depth(20), 3);
    }

    #[test]
    fn off_grid_head_is_a_stall() {
        let mut c = PassthroughClock::new(Rational::FPS_29_97);
        c.arm();
        for tick in 0..10_u64 {
            let i = i64::try_from(tick).unwrap();
            c.on_tick(
                tick,
                0,
                std::iter::once(au(i, i == 0)),
                &Open,
                &mut NoRepeat,
            );
        }
        let mut bad = au(10, false);
        bad.dts += 1;
        bad.pts += 1;
        let mut stalled_at = None;
        for tick in 10..20_u64 {
            let i = i64::try_from(tick).unwrap();
            let a = if tick == 10 {
                bad.clone()
            } else {
                au(i, false)
            };
            let e = c.on_tick(tick, 0, std::iter::once(a), &Open, &mut NoRepeat);
            if stalled_at.is_none() && matches!(e, Emission::Bridge { .. }) {
                stalled_at = Some(tick);
            }
        }
        // AU 10 is due at tick 13 (offset 3).
        assert_eq!(stalled_at, Some(13));
        assert_eq!(c.counters().stalls, 1);
    }

    #[test]
    fn non_monotonic_source_is_malformed() {
        let mut c = PassthroughClock::new(Rational::FPS_29_97);
        c.arm();
        for tick in 0..8_u64 {
            let i = i64::try_from(tick).unwrap();
            c.on_tick(
                tick,
                0,
                std::iter::once(au(i, i == 0)),
                &Open,
                &mut NoRepeat,
            );
        }
        let e = c.on_tick(8, 0, std::iter::once(au(2, false)), &Open, &mut NoRepeat);
        assert!(matches!(
            e,
            Emission::Bridge {
                reason: BridgeReason::Malformed,
                ..
            }
        ));
        assert_eq!(c.counters().malformed, 1);
    }

    #[test]
    fn non_strict_keyframe_never_starts_a_run() {
        let mut c = PassthroughClock::new(Rational::FPS_29_97);
        c.arm();
        for tick in 0..50_u64 {
            let i = i64::try_from(tick).unwrap();
            let e = c.on_tick(tick, 0, std::iter::once(au(i, false)), &Open, &mut NoRepeat);
            assert!(matches!(e, Emission::Bridge { .. }));
        }
        assert_eq!(c.queued(), 0, "useless AUs are not kept while bridged");
    }

    #[test]
    fn disarmed_clock_bridges() {
        let mut c = PassthroughClock::new(Rational::FPS_29_97);
        let e = c.on_tick(0, 0, std::iter::once(au(0, true)), &Open, &mut NoRepeat);
        assert!(matches!(
            e,
            Emission::Bridge {
                reason: BridgeReason::NotArmed,
                ..
            }
        ));
        assert_eq!(c.state(), ClockState::Bridging(BridgeReason::NotArmed));
    }

    #[test]
    fn slate_loop_restarts_at_idr() {
        let pkts: Arc<[Arc<[u8]>]> = (0..4_u8).map(|i| Arc::from(vec![i])).collect();
        let mut s = SlateLoop::new(pkts).unwrap();
        let a = s.next_for_tick(10).unwrap();
        assert!(a.keyframe);
        assert_eq!((a.dts, a.pts), (10, 10));
        let b = s.next_for_tick(11).unwrap();
        assert!(!b.keyframe);
        s.enter();
        let c = s.next_for_tick(12).unwrap();
        assert!(c.keyframe, "a re-entered bridge starts at the slate IDR");
        for k in 13..16 {
            s.next_for_tick(k).unwrap();
        }
        assert!(s.next_for_tick(16).unwrap().keyframe, "loops");
        assert!(SlateLoop::new(Arc::from(Vec::<Arc<[u8]>>::new())).is_none());
    }

    fn au_at(i: i64, epoch: i64, idr: bool) -> TapAu {
        let mut a = au(i, idr);
        a.dts = epoch + i * 3003;
        a.pts = epoch + i * 3003;
        a
    }

    fn drive(
        c: &mut PassthroughClock,
        ticks: std::ops::Range<u64>,
        mut f: impl FnMut(u64) -> Option<TapAu>,
    ) -> Vec<Emission> {
        ticks
            .map(|tick| c.on_tick(tick, 0, f(tick).into_iter(), &Open, &mut NoRepeat))
            .collect()
    }

    #[test]
    fn a_shifted_epoch_cfr_stream_starts_a_run() {
        // R1-M1: absolute DTS 126000 + n*3003 is not a multiple of 3003.
        for epoch in [126_000_i64, -126_000, 1, -1, 7_777_777] {
            let mut c = PassthroughClock::new(Rational::FPS_29_97);
            c.arm();
            let out = drive(&mut c, 0..20, |t| {
                let i = i64::try_from(t).unwrap();
                Some(au_at(i, epoch, i % 10 == 0))
            });
            assert!(
                out.iter().skip(3).all(|e| matches!(e, Emission::Au { .. })),
                "epoch {epoch}: runs from tick 3"
            );
            for (t, e) in out.iter().enumerate() {
                assert_eq!(e.dts(), i64::try_from(t).unwrap());
            }
        }
    }

    /// Decode-order IBBP: (dts index, pts index, idr). GOP of 6 pictures.
    fn ibbp(n: i64) -> (i64, i64, bool) {
        // decode order per GOP: I0 P3 B1 B2 P6? keep within GOP: I(0) P(3) B(1) B(2) P(5) B(4)
        let g = n / 6;
        let k = n % 6;
        let pts = [0, 3, 1, 2, 5, 4][usize::try_from(k).unwrap()];
        (n, g * 6 + pts + 1, k == 0)
    }

    fn assert_monotone(out: &[Emission]) {
        let mut seen = std::collections::HashSet::new();
        let mut last_dts = None;
        for e in out {
            let pts = match e {
                Emission::Au { pts, .. }
                | Emission::Repeat { pts, .. }
                | Emission::Bridge { pts, .. } => *pts,
            };
            assert!(seen.insert(pts), "duplicate PTS {pts} in {out:?}");
            assert!(pts >= e.dts(), "pts before dts");
            if let Some(l) = last_dts {
                assert!(e.dts() > l, "dts strictly increasing");
            }
            last_dts = Some(e.dts());
        }
    }

    #[test]
    fn b_frame_stalls_and_parameter_changes_never_collide_with_bridge_pts() {
        // R1-M2: stall mid-GOP on IBBP, then a return, then a parameter
        // change mid-GOP, then a return; every emission's PTS is unique.
        for stall_at in [9_u64, 10, 11, 12] {
            let mut c = PassthroughClock::new(Rational::FPS_29_97);
            c.arm();
            let mk = |n: i64, gen: u64| {
                let (d, p, idr) = ibbp(n);
                let mut a = au(d, idr);
                a.pts = p * 3003;
                a.params_gen = gen;
                a
            };
            let mut out = Vec::new();
            let mut n = 0_i64;
            for tick in 0..400_u64 {
                // Stall: nothing for 3 ticks; the source then skips ahead.
                let gen = if tick >= 200 { 2 } else { 1 };
                let a = if (stall_at..stall_at + 3).contains(&tick) {
                    n += 1;
                    None
                } else {
                    let a = mk(n, gen);
                    n += 1;
                    Some(a)
                };
                out.push(c.on_tick(tick, 0, a.into_iter(), &Open, &mut NoRepeat));
            }
            assert!(c.counters().stalls >= 1);
            assert!(c.counters().param_changes >= 1 || c.counters().runs >= 2);
            assert!(c.counters().runs >= 2, "returned after the stall");
            assert!(
                out.iter()
                    .any(|e| matches!(e, Emission::Bridge { dts, pts, .. } if pts > dts)),
                "a delayed bridge was exercised"
            );
            assert_monotone(&out);
        }
    }

    #[test]
    fn extreme_timestamps_fail_closed_never_panic() {
        // R1-A4.
        let mut c = PassthroughClock::new(Rational::new(4, 1));
        c.arm();
        let mut a = au(0, true);
        a.time_base = Rational::new(1 << 62, 1);
        a.dts = i64::MIN;
        a.pts = i64::MIN;
        let e = c.on_tick(0, 0, std::iter::once(a), &Open, &mut NoRepeat);
        assert!(matches!(e, Emission::Bridge { .. }));
        let mut c = PassthroughClock::new(Rational::new(2, 1));
        c.arm();
        let mut out = Vec::new();
        for (tick, (dts, pts)) in [
            (i64::MIN, i64::MAX),
            (i64::MIN + 1, i64::MAX),
            (0, 0),
            (1, 1),
        ]
        .into_iter()
        .enumerate()
        {
            let mut a = au(0, true);
            a.time_base = Rational::new(i64::MAX, 1);
            a.dts = dts;
            a.pts = pts;
            out.push(c.on_tick(
                u64::try_from(tick).unwrap(),
                u64::MAX,
                std::iter::once(a),
                &Open,
                &mut NoRepeat,
            ));
        }
        assert!(out.iter().all(|e| matches!(e, Emission::Bridge { .. })));
        // Converted magnitude beyond 2^62 ticks is malformed (never emitted).
        let c = PassthroughClock::new(Rational::new(1, 1));
        assert_eq!(c.to_ticks(i64::MAX, Rational::new(1, 1)), None);
        assert_eq!(c.to_ticks(1 << 62, Rational::new(1, 1)), Some(1 << 62));
    }

    #[test]
    fn an_oversize_au_ends_a_run() {
        // R1-A5.
        let mut c = PassthroughClock::new(Rational::FPS_29_97);
        c.arm();
        let out = drive(&mut c, 0..10, |t| {
            let i = i64::try_from(t).unwrap();
            Some(au(i, i == 0))
        });
        assert!(matches!(out[9], Emission::Au { .. }));
        let mut big = au(10, false);
        big.data = Arc::from(vec![0_u8; MAX_AU_BYTES + 1]);
        let e = c.on_tick(10, 0, std::iter::once(big), &Open, &mut NoRepeat);
        assert!(matches!(
            e,
            Emission::Bridge {
                reason: BridgeReason::Malformed,
                ..
            }
        ));
    }

    #[test]
    fn slate_honours_the_bridge_delay() {
        let pkts: Arc<[Arc<[u8]>]> = (0..2_u8).map(|i| Arc::from(vec![i])).collect();
        let mut s = SlateLoop::new(pkts).unwrap();
        let a = s.next_at(10, 12).unwrap();
        assert_eq!((a.dts, a.pts, a.keyframe), (10, 12, true));
        assert!(s.next_at(11, 10).is_none());
    }
}
