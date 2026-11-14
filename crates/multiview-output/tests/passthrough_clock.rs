//! Passthrough clock soak and chaos tests (output-routing §12.5 / §12.10, #344 D1).
//!
//! A deterministic simulated source produces access units on its own clock
//! (with a ppm offset and bounded arrival jitter); a simulated monotonic clock
//! drives the output ticks at exactly 30000/1001 (integer nanoseconds). Every
//! test checks the invariant-#1 contract on every tick: exactly one emission,
//! stamped `dts == tick`; bridge and repeat AUs have `pts == dts`; source AUs
//! keep their source reorder `pts - dts`; the rebase offset changes only at a
//! strict IDR; a keyframe never presents before anything emitted earlier.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::too_many_lines,
    clippy::cast_possible_truncation,
    clippy::struct_excessive_bools
)]

use std::collections::VecDeque;
use std::sync::Arc;

use multiview_core::time::Rational;
use multiview_output::passthrough::{
    BridgeReason, ClockState, Emission, PassthroughClock, RepeatSource, SlotGate, TapAu,
};

const FPS: Rational = Rational::FPS_29_97;
const TB: Rational = Rational::new(1, 90_000);
/// 90 kHz units per frame at 30000/1001.
const PERIOD_90K: i64 = 3003;
/// Ticks per simulated minute at 30000/1001 (floor).
const TICKS_PER_MIN: u64 = 60 * 30_000 / 1001;

/// Output tick `t` starts at this monotonic time (exact integer ns, floor).
fn tick_ns(t: u64) -> u64 {
    u64::try_from(u128::from(t) * 1001 * 1_000_000_000 / 30_000).unwrap()
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Pattern {
    /// I P P P ... (no reordering).
    POnly,
    /// IBBP closed GOP of 12, reorder delay one frame.
    Ibbp,
}

/// Display index within a 12-frame IBBP closed GOP, by decode index.
const IBBP_DISPLAY: [i64; 12] = [0, 3, 1, 2, 6, 4, 5, 9, 7, 8, 11, 10];

struct SourceCfg {
    /// Source frame-rate offset in ppm (+ = faster than the output).
    ppm: i64,
    /// Arrival jitter bound in ns.
    jitter_ns: u64,
    /// GOP length in frames (multiple of 12 for IBBP).
    gop: i64,
    pattern: Pattern,
    /// Source AUs produced in `[start, end)` ns are lost (a live stall).
    stall: Option<(u64, u64)>,
    /// From this AU index on, `params_gen` is 2 (an IDR index).
    param_change_at: Option<i64>,
    /// No AU is produced after this time (wedged demuxer).
    wedge_after_ns: Option<u64>,
    /// AUs in this index range are held back and arrive together at the end
    /// of the range plus this many extra ns (a burst).
    burst: Option<(i64, i64, u64)>,
}

impl SourceCfg {
    fn steady(pattern: Pattern) -> Self {
        Self {
            ppm: 0,
            jitter_ns: 20_000_000,
            gop: 60,
            pattern,
            stall: None,
            param_change_at: None,
            wedge_after_ns: None,
            burst: None,
        }
    }
}

struct Source {
    cfg: SourceCfg,
    next: i64,
    rng: u64,
    last_arrival: u64,
    /// AUs produced but not yet delivered, in decode order.
    pending: VecDeque<TapAu>,
}

impl Source {
    fn new(cfg: SourceCfg) -> Self {
        Self {
            cfg,
            next: 0,
            rng: 0x9E37_79B9_7F4A_7C15,
            last_arrival: 0,
            pending: VecDeque::new(),
        }
    }

    fn jitter(&mut self) -> u64 {
        // xorshift64*: deterministic.
        self.rng ^= self.rng >> 12;
        self.rng ^= self.rng << 25;
        self.rng ^= self.rng >> 27;
        let r = self.rng.wrapping_mul(0x2545_F491_4F6C_DD1D);
        if self.cfg.jitter_ns == 0 {
            0
        } else {
            r % self.cfg.jitter_ns
        }
    }

    /// Production time of AU `i` on the source's clock (start offset 1 ms).
    fn produced_ns(&self, i: i64) -> u64 {
        let i = i128::from(i);
        let num = i * 1001 * 1_000_000_000 * 1_000_000;
        let den = 30_000 * (1_000_000 + i128::from(self.cfg.ppm));
        u64::try_from(num / den).unwrap() + 1_000_000
    }

    fn display(&self, i: i64) -> (i64, i64) {
        // (dts frames, pts frames)
        match self.cfg.pattern {
            Pattern::POnly => (i, i),
            Pattern::Ibbp => {
                let k = usize::try_from(i.rem_euclid(12)).unwrap();
                (i - 1, i - i.rem_euclid(12) + IBBP_DISPLAY[k])
            }
        }
    }

    /// Source reorder `pts - dts` (in frames) of AU `i`.
    fn reorder(&self, i: i64) -> i64 {
        let (d, p) = self.display(i);
        p - d
    }

    /// Produce every AU due by `now` into `pending`.
    fn produce_until(&mut self, now: u64) {
        loop {
            let i = self.next;
            let prod = self.produced_ns(i);
            if prod > now {
                break;
            }
            if self.cfg.wedge_after_ns.is_some_and(|w| prod >= w) {
                break;
            }
            self.next += 1;
            if self.cfg.stall.is_some_and(|(a, b)| prod >= a && prod < b) {
                continue;
            }
            let mut arrival = prod + self.jitter();
            if let Some((a, b, extra)) = self.cfg.burst {
                if i >= a && i < b {
                    arrival = self.produced_ns(b) + extra;
                }
            }
            arrival = arrival.max(self.last_arrival);
            self.last_arrival = arrival;
            let (d, p) = self.display(i);
            let gen = match self.cfg.param_change_at {
                Some(c) if i >= c => 2,
                _ => 1,
            };
            self.pending.push_back(TapAu {
                data: Arc::from(i.to_be_bytes().to_vec()),
                dts: d * PERIOD_90K,
                pts: p * PERIOD_90K,
                time_base: TB,
                strict_idr: i % self.cfg.gop == 0,
                reference: true,
                params_gen: gen,
                arrival_ns: arrival,
            });
        }
    }
}

fn index_of(data: &[u8]) -> i64 {
    i64::from_be_bytes(data.try_into().unwrap())
}

/// A gate whose answers the test controls per generation.
struct Gate {
    match_gen2: bool,
    slate_ready_gen2_from: Option<u64>,
    now_tick: u64,
}

impl Gate {
    fn open() -> Self {
        Self {
            match_gen2: true,
            slate_ready_gen2_from: Some(0),
            now_tick: 0,
        }
    }
}

impl SlotGate for Gate {
    fn matches(&self, gen: u64) -> bool {
        gen == 1 || self.match_gen2
    }
    fn matched_slate_ready(&self, gen: u64) -> bool {
        gen == 1
            || self
                .slate_ready_gen2_from
                .is_some_and(|t| self.now_tick >= t)
    }
}

/// A repeat builder that always succeeds (the bytes are a marker).
struct Repeats;
impl RepeatSource for Repeats {
    fn repeat_after(&mut self, _prev: &TapAu) -> Option<Arc<[u8]>> {
        Some(Arc::from(vec![0xEE_u8]))
    }
}

#[derive(Default)]
struct Log {
    /// (tick, emission kind: 'A' au, 'R' repeat, 'B' bridge, reason)
    kinds: Vec<(u64, char, Option<BridgeReason>)>,
    /// Ticks at which a run started.
    run_starts: Vec<u64>,
    /// Ticks at which a repeat was emitted.
    repeats: Vec<u64>,
    /// Depth samples (tick, depth) on run ticks.
    depths: Vec<(u64, usize)>,
    /// Offset per emitted AU (tick, offset, keyframe).
    offsets: Vec<(u64, i64, bool)>,
    first_au: Option<u64>,
}

impl Log {
    fn bridges_after(&self, t: u64) -> usize {
        self.kinds
            .iter()
            .filter(|(k, c, _)| *k > t && *c == 'B')
            .count()
    }
    fn first_bridge_at_or_after(&self, t: u64) -> Option<(u64, BridgeReason)> {
        self.kinds
            .iter()
            .find(|(k, c, _)| *k >= t && *c == 'B')
            .map(|(k, _, r)| (*k, r.unwrap()))
    }
    fn first_run_start_after(&self, t: u64) -> Option<u64> {
        self.run_starts.iter().copied().find(|&k| k > t)
    }
}

/// Drive `ticks` output ticks, checking the per-tick contract, and return the log.
fn run(
    clock: &mut PassthroughClock,
    src: &mut Source,
    gate: &mut Gate,
    ticks: u64,
    mut hook: impl FnMut(u64, &mut PassthroughClock, &mut Gate),
) -> Log {
    let mut log = Log::default();
    let mut repeats = Repeats;
    let mut max_pts: Option<i64> = None;
    let mut prev_was_repeat = false;
    for tick in 0..ticks {
        hook(tick, clock, gate);
        gate.now_tick = tick;
        let now = tick_ns(tick);
        src.produce_until(now);
        let pending = &mut src.pending;
        let available = std::iter::from_fn(|| {
            if pending.front().is_some_and(|a| a.arrival_ns <= now) {
                pending.pop_front()
            } else {
                None
            }
        });
        let e = clock.on_tick(tick, now, available, gate, &mut repeats);
        let t = i64::try_from(tick).unwrap();
        assert_eq!(
            e.dts(),
            t,
            "tick {tick}: exactly one emission stamped dts == tick"
        );
        match &e {
            Emission::Au {
                data,
                dts,
                pts,
                keyframe,
                run_start,
                ..
            } => {
                let i = index_of(data);
                let (sd, _) = src.display(i);
                assert_eq!(
                    pts - dts,
                    src.reorder(i),
                    "tick {tick}: source reorder kept (AU {i})"
                );
                if *run_start {
                    assert!(*keyframe, "tick {tick}: a run starts at a strict IDR");
                    log.run_starts.push(tick);
                }
                if *keyframe {
                    assert!(
                        max_pts.is_none_or(|m| *pts > m),
                        "tick {tick}: an IDR presents after everything emitted before it"
                    );
                }
                if prev_was_repeat {
                    assert!(*keyframe, "tick {tick}: a repeat is followed by the IDR");
                }
                log.offsets.push((tick, dts - sd, *keyframe));
                log.depths.push((tick, clock.last_depth()));
                log.first_au.get_or_insert(tick);
                log.kinds.push((tick, 'A', None));
                max_pts = Some(max_pts.map_or(*pts, |m| m.max(*pts)));
                prev_was_repeat = false;
            }
            Emission::Repeat { dts, pts, .. } => {
                assert_eq!((*dts, *pts), (t, t), "repeat: pts == dts == tick");
                assert!(max_pts.is_none_or(|m| *pts > m), "repeat presents last");
                log.repeats.push(tick);
                log.depths.push((tick, clock.last_depth()));
                log.kinds.push((tick, 'R', None));
                max_pts = Some(max_pts.map_or(*pts, |m| m.max(*pts)));
                prev_was_repeat = true;
            }
            Emission::Bridge { dts, pts, reason } => {
                assert_eq!((*dts, *pts), (t, t), "bridge: pts == dts == tick");
                assert!(
                    !prev_was_repeat,
                    "tick {tick}: a repeat must be followed by its IDR"
                );
                log.kinds.push((tick, 'B', Some(*reason)));
                max_pts = Some(max_pts.map_or(*pts, |m| m.max(*pts)));
                prev_was_repeat = false;
            }
        }
        assert!(clock.queued() <= 8, "jitter buffer bounded");
    }
    // The rebase offset changes only at a strict IDR (within a run or at a
    // new run's start).
    for w in log.offsets.windows(2) {
        let ((_, o0, _), (t1, o1, k1)) = (w[0], w[1]);
        if o0 != o1 {
            assert!(k1, "tick {t1}: offset changed ({o0} -> {o1}) at a non-IDR");
        }
    }
    log
}

fn steady_clock() -> PassthroughClock {
    let mut c = PassthroughClock::new(FPS);
    c.arm();
    c
}

#[test]
fn ten_minutes_one_au_per_tick_p_only() {
    let mut c = steady_clock();
    let mut src = Source::new(SourceCfg::steady(Pattern::POnly));
    let mut gate = Gate::open();
    let log = run(
        &mut c,
        &mut src,
        &mut gate,
        10 * TICKS_PER_MIN,
        |_, _, _| {},
    );
    let first = log.first_au.unwrap();
    assert_eq!(log.run_starts, vec![first], "one run");
    assert_eq!(log.bridges_after(first), 0, "no bridge once running");
    assert_eq!(log.repeats.len(), 0);
    assert_eq!(c.counters().drift_drops, 0);
    assert!(first <= 3 + 1, "the run starts after TARGET_DEPTH ticks");
    assert_eq!(c.state(), ClockState::Passthrough);
}

#[test]
fn ten_minutes_ibbp_keeps_nonzero_reorder() {
    let mut c = steady_clock();
    let mut cfg = SourceCfg::steady(Pattern::Ibbp);
    cfg.gop = 48;
    let mut src = Source::new(cfg);
    let mut gate = Gate::open();
    let log = run(
        &mut c,
        &mut src,
        &mut gate,
        10 * TICKS_PER_MIN,
        |_, _, _| {},
    );
    let first = log.first_au.unwrap();
    assert_eq!(log.run_starts, vec![first]);
    assert_eq!(log.bridges_after(first), 0);
    assert_eq!(c.counters().emitted, 10 * TICKS_PER_MIN - first);
}

#[test]
fn stall_bridges_on_the_tick_and_returns_after_30_stable_ticks_at_an_idr() {
    let mut c = steady_clock();
    let mut cfg = SourceCfg::steady(Pattern::POnly);
    // The source stops for 2 s starting at 20 s.
    let (a, b) = (20_000_000_000, 22_000_000_000);
    cfg.stall = Some((a, b));
    let mut src = Source::new(cfg);
    let mut gate = Gate::open();
    let log = run(&mut c, &mut src, &mut gate, 2 * TICKS_PER_MIN, |_, _, _| {});
    let first = log.first_au.unwrap();
    let (stall_tick, reason) = log.first_bridge_at_or_after(first).unwrap();
    assert_eq!(reason, BridgeReason::Stall);
    // The last AU before the stall was emitted on the tick right before.
    assert_eq!(
        log.kinds[usize::try_from(stall_tick - 1).unwrap()].1,
        'A',
        "no hole"
    );
    // The first AU after the stall is index i0; it is on-time from its
    // arrival; return needs 30 ticks of that and then a strict IDR.
    let resumed_at = log
        .kinds
        .iter()
        .find(|(k, c, _)| *k > stall_tick && *c == 'A')
        .map(|(k, _, _)| *k)
        .unwrap();
    let back = log.first_run_start_after(stall_tick).unwrap();
    assert_eq!(back, resumed_at);
    // The source resumed producing at b; ticks from then on.
    let resume_tick = b * 30_000 / (1001 * 1_000_000_000);
    assert!(
        back >= resume_tick + 30,
        "returns not before 30 stable ticks ({back} vs {resume_tick})"
    );
    // And only at an IDR (asserted by run_start => keyframe in `run`); the
    // return is at the first IDR after the 30 ticks (+ TARGET_DEPTH).
    assert!(
        back <= resume_tick + 30 + 60 + 3 + 2,
        "returns at the first usable IDR"
    );
    assert_eq!(c.counters().stalls, 1);
    assert_eq!(log.bridges_after(back), 0, "stable after return");
}

#[test]
fn parameter_change_bridges_and_returns_after_300_ticks_when_matched_and_ready() {
    // IDR every 60; the change happens at AU 600 (an IDR).
    let make = || {
        let mut cfg = SourceCfg::steady(Pattern::POnly);
        cfg.param_change_at = Some(600);
        Source::new(cfg)
    };
    // (1) matched and the slate ready: returns after >= 300 ticks.
    let mut c = steady_clock();
    let mut src = make();
    let mut gate = Gate::open();
    let log = run(&mut c, &mut src, &mut gate, 3 * TICKS_PER_MIN, |_, _, _| {});
    let first = log.first_au.unwrap();
    let (change_tick, reason) = log.first_bridge_at_or_after(first).unwrap();
    assert_eq!(reason, BridgeReason::ParameterChange);
    // AU 600 was due exactly on that tick (offset of the run).
    let off = log.offsets.last().map(|o| o.1).unwrap();
    assert_eq!(
        i64::try_from(change_tick).unwrap(),
        600 + off,
        "bridges on the AU's own tick"
    );
    let back = log.first_run_start_after(change_tick).unwrap();
    assert!(back >= change_tick + 300, "not before 300 stable ticks");
    assert!(
        back <= change_tick + 300 + 60 + 3 + 5,
        "at the first IDR after"
    );
    assert_eq!(c.counters().param_changes, 1);

    // (2) the new parameters do not match: never returns.
    let mut c = steady_clock();
    let mut src = make();
    let mut gate = Gate::open();
    gate.match_gen2 = false;
    let log = run(&mut c, &mut src, &mut gate, 3 * TICKS_PER_MIN, |_, _, _| {});
    let (change_tick, _) = log.first_bridge_at_or_after(log.first_au.unwrap()).unwrap();
    assert!(log.first_run_start_after(change_tick).is_none());
    assert_eq!(
        c.state(),
        ClockState::Bridging(BridgeReason::ParametersNoLongerMatch)
    );

    // (3) matched, slate ready only late: returns only after it is ready.
    let mut c = steady_clock();
    let mut src = make();
    let mut gate = Gate::open();
    let ready_at = 2 * TICKS_PER_MIN;
    gate.slate_ready_gen2_from = Some(ready_at);
    let mut seen_not_ready = false;
    let log = run(&mut c, &mut src, &mut gate, 3 * TICKS_PER_MIN, |t, c, _| {
        if t + 1 == ready_at {
            seen_not_ready = c.state() == ClockState::Bridging(BridgeReason::SlateNotReady);
        }
    });
    assert!(seen_not_ready, "readback says slate-not-ready");
    let (change_tick, _) = log.first_bridge_at_or_after(log.first_au.unwrap()).unwrap();
    let back = log.first_run_start_after(change_tick).unwrap();
    assert!(back >= ready_at);
}

/// ±200 ppm for 60 minutes.
fn drift(pattern: Pattern, ppm: i64) -> (PassthroughClock, Log) {
    let mut c = steady_clock();
    let mut cfg = SourceCfg::steady(pattern);
    cfg.ppm = ppm;
    cfg.gop = 60;
    let mut src = Source::new(cfg);
    let mut gate = Gate::open();
    let log = run(
        &mut c,
        &mut src,
        &mut gate,
        60 * TICKS_PER_MIN,
        |_, _, _| {},
    );
    (c, log)
}

fn assert_depth_bounded_after_first_correction(c: &PassthroughClock, log: &Log, first_corr: u64) {
    let _ = c;
    for &(t, d) in &log.depths {
        if t >= first_corr {
            assert!((1..=8).contains(&d), "tick {t}: depth {d} outside [1, 8]");
        }
    }
}

#[test]
fn drift_fast_p_only_corrects_by_dropping_at_idrs() {
    let (c, log) = drift(Pattern::POnly, 200);
    let n = c.counters();
    let first = log.first_au.unwrap();
    assert_eq!(log.bridges_after(first), 0, "zero bridges");
    assert!(
        (15..=30).contains(&n.drift_corrections),
        "corrections {}",
        n.drift_corrections
    );
    assert_eq!(
        n.drift_drops, n.drift_corrections,
        "P-only drops one AU per correction"
    );
    assert_eq!(log.repeats.len(), 0);
    // The first correction: the first keyframe whose offset changed.
    let first_corr = log
        .offsets
        .windows(2)
        .find(|w| w[0].1 != w[1].1)
        .map(|w| w[1].0)
        .unwrap();
    assert_depth_bounded_after_first_correction(&c, &log, first_corr);
}

#[test]
fn drift_slow_p_only_corrects_with_repeats_at_idrs() {
    let (c, log) = drift(Pattern::POnly, -200);
    let n = c.counters();
    let first = log.first_au.unwrap();
    assert_eq!(log.bridges_after(first), 0, "zero bridges");
    let r = u64::try_from(log.repeats.len()).unwrap();
    assert!((15..=30).contains(&r), "repeats {r}");
    assert_eq!(n.repeats, r);
    assert_eq!(n.drift_drops, 0);
    assert_depth_bounded_after_first_correction(&c, &log, log.repeats[0]);
}

#[test]
fn drift_fast_ibbp_corrects_by_dropping_the_reorder_suffix() {
    let (c, log) = drift(Pattern::Ibbp, 200);
    let n = c.counters();
    let first = log.first_au.unwrap();
    assert_eq!(log.bridges_after(first), 0, "zero bridges");
    assert!(
        (15..=30).contains(&n.drift_drops),
        "drops {}",
        n.drift_drops
    );
    // The IBBP drop set is the P and the B before the IDR: 2 per correction.
    assert_eq!(n.drift_drops, 2 * n.drift_corrections);
    assert!(log.repeats.is_empty(), "never a repeat with reordering");
    let first_corr = log
        .offsets
        .windows(2)
        .find(|w| w[0].1 != w[1].1)
        .map(|w| w[1].0)
        .unwrap();
    assert_depth_bounded_after_first_correction(&c, &log, first_corr);
}

#[test]
fn drift_slow_ibbp_bridges_boundedly_and_never_repeats() {
    let (c, log) = drift(Pattern::Ibbp, -200);
    let n = c.counters();
    assert!(
        log.repeats.is_empty(),
        "a B-frame source never takes a repeat"
    );
    assert_eq!(n.repeats, 0);
    // 200 ppm over 60 min is ~21.6 frames of underrun; each stall re-buffers
    // TARGET_DEPTH, so the stall count is bounded (about 21.6 / 2).
    assert!(n.stalls >= 1, "the underrun stalls");
    assert!(n.stalls <= 30, "stalls {}", n.stalls);
    // Each episode bridges for 30 ticks + up to a GOP + TARGET_DEPTH.
    assert!(
        n.bridge_ticks <= n.stalls * (30 + 60 + 3 + 3) + 10,
        "bridge ticks {}",
        n.bridge_ticks
    );
}

#[test]
fn wedged_demuxer_bridges_every_tick_for_ten_minutes() {
    let mut c = steady_clock();
    let mut cfg = SourceCfg::steady(Pattern::POnly);
    cfg.wedge_after_ns = Some(10_000_000_000);
    let mut src = Source::new(cfg);
    let mut gate = Gate::open();
    let ticks = 10 * TICKS_PER_MIN + 400;
    let started = std::time::Instant::now();
    let log = run(&mut c, &mut src, &mut gate, ticks, |_, _, _| {});
    let elapsed = started.elapsed();
    let (stall_tick, reason) = log.first_bridge_at_or_after(log.first_au.unwrap()).unwrap();
    assert_eq!(reason, BridgeReason::Stall);
    let bridged = log
        .kinds
        .iter()
        .filter(|(k, _, _)| *k >= stall_tick)
        .count();
    assert_eq!(u64::try_from(bridged).unwrap(), ticks - stall_tick);
    assert!(log
        .kinds
        .iter()
        .filter(|(k, _, _)| *k >= stall_tick)
        .all(|(_, c, _)| *c == 'B'));
    assert!(
        stall_tick + 10 * TICKS_PER_MIN <= ticks,
        "10 minutes of bridging"
    );
    // on_tick never waits: 18,000+ ticks finish far below real time.
    assert!(elapsed < std::time::Duration::from_secs(60), "{elapsed:?}");
    assert_eq!(c.queued(), 0);
}

#[test]
fn burst_beyond_jitter_bound_drops_oldest_ends_run_and_recovers() {
    let mut c = steady_clock();
    let mut cfg = SourceCfg::steady(Pattern::POnly);
    // AUs 300..320 are held and arrive together just after AU 320 is produced:
    // the run stalls at AU 300's tick; then 20 AUs land at once.
    cfg.burst = Some((300, 320, 0));
    let mut src = Source::new(cfg);
    let mut gate = Gate::open();
    let log = run(&mut c, &mut src, &mut gate, 2 * TICKS_PER_MIN, |_, _, _| {});
    let first = log.first_au.unwrap();
    let (t, reason) = log.first_bridge_at_or_after(first).unwrap();
    assert_eq!(
        reason,
        BridgeReason::Stall,
        "the gap bridges on its tick (no hole)"
    );
    let back = log.first_run_start_after(t).unwrap();
    assert!(back >= t + 30);
    assert_eq!(log.bridges_after(back), 0, "recovers");

    // A burst inside a run: 12 AUs land at once, beyond the 8-tick bound.
    let mut c = steady_clock();
    let mut gate = Gate::open();
    let mut repeats = Repeats;
    let mut i = 0_i64;
    let mk = |i: i64, arrival_ns: u64| TapAu {
        data: Arc::from(i.to_be_bytes().to_vec()),
        dts: i * PERIOD_90K,
        pts: i * PERIOD_90K,
        time_base: TB,
        strict_idr: i % 30 == 0,
        reference: true,
        params_gen: 1,
        arrival_ns,
    };
    let mut overflow_tick = None;
    for tick in 0..400_u64 {
        let now = tick_ns(tick);
        let mut batch = Vec::new();
        if tick == 200 {
            // A burst of 12 AUs ahead of time.
            for _ in 0..12 {
                batch.push(mk(i, now));
                i += 1;
            }
        } else if !(200..=212).contains(&tick) {
            batch.push(mk(i, now));
            i += 1;
        }
        let e = c.on_tick(tick, now, batch.into_iter(), &gate, &mut repeats);
        assert_eq!(e.dts(), i64::try_from(tick).unwrap());
        assert!(c.queued() <= 8);
        if overflow_tick.is_none() {
            if let Emission::Bridge {
                reason: BridgeReason::JitterOverflow,
                ..
            } = e
            {
                overflow_tick = Some(tick);
            }
        }
        gate.now_tick = tick;
    }
    assert_eq!(
        overflow_tick,
        Some(200),
        "the overflow ends the run on that tick"
    );
    assert!(c.counters().jitter_drops >= 1);
    assert_eq!(c.state(), ClockState::Passthrough, "recovers");
    assert_eq!(c.counters().runs, 2);
}

#[test]
fn repeat_source_refusal_does_not_correct_and_underrun_bridges() {
    struct Refuse;
    impl RepeatSource for Refuse {
        fn repeat_after(&mut self, _: &TapAu) -> Option<Arc<[u8]>> {
            None
        }
    }
    let mut c = steady_clock();
    let gate = Gate::open();
    let mut refuse = Refuse;
    let mk = |i: i64, arrival_ns: u64| TapAu {
        data: Arc::from(i.to_be_bytes().to_vec()),
        dts: i * PERIOD_90K,
        pts: i * PERIOD_90K,
        time_base: TB,
        strict_idr: i % 30 == 0,
        reference: true,
        params_gen: 1,
        arrival_ns,
    };
    // Deliver one AU per tick but skip delivery on every 10th tick from 100 on:
    // the source falls behind and the clock must bridge (never a hole).
    let mut i = 0;
    let mut stalled = false;
    for tick in 0..300_u64 {
        let now = tick_ns(tick);
        let batch = if tick >= 100 && tick % 10 == 0 {
            vec![]
        } else {
            let a = mk(i, now);
            i += 1;
            vec![a]
        };
        let e = c.on_tick(tick, now, batch.into_iter(), &gate, &mut refuse);
        assert_eq!(e.dts(), i64::try_from(tick).unwrap());
        assert!(!matches!(e, Emission::Repeat { .. }));
        stalled |= matches!(
            e,
            Emission::Bridge {
                reason: BridgeReason::Stall,
                ..
            }
        );
    }
    assert!(stalled);
    assert_eq!(c.counters().repeats, 0);
}
