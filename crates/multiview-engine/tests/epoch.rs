//! DEV-C1 (ADR-M010): the **outbound presentation epoch** — one
//! `WallClockRef` per program anchoring the output tick counter to disciplined
//! wall-clock nanoseconds.
//!
//! Pins:
//! * the `EpochTracker` re-anchor policy: anchor once, **hold** inside the
//!   deadband, **slew** (bounded) toward a persistent error, **step** only on a
//!   gross discontinuity (the documented Class-2-like case);
//! * the `EpochSampler` anchor math is exact integer (`wall@tick0 = wall_now −
//!   (mono_mid − seed)`), with the PTP servo offset applied only on the PTP leg;
//! * ADR-T012 source precedence: PTP-if-disciplined, else the system clock,
//!   carried honestly as `ClockSource`/`ClockQuality`;
//! * invariant #1: epoch observation can never change the tick stream.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use multiview_compositor::blend::LinearRgba;
use multiview_compositor::pipeline::{CanvasColor, Nv12Image};
use multiview_core::color::ColorInfo;
use multiview_core::layout::{Canvas, Cell, FitMode, Layout};
use multiview_core::time::{MediaTime, Rational};
use multiview_core::wallclock::WallClockRef;
use multiview_engine::epoch::{
    EpochPolicy, EpochSampler, EpochSamplerConfig, EpochTracker, EpochUpdate, WallClockSampler,
    WallSample, EPOCH_RATE,
};
use multiview_engine::ptp::{LockState, ReferenceStatus};
use multiview_engine::sysref::{NtpQuery, NtpReading, SystemRefConfig};
use multiview_engine::{
    CompositorDrive, CooperativePacer, EnginePublisher, EngineRuntime, LatestState,
    ManualTimeSource, OutputClock, StopSignal, TimeSource,
};
use multiview_events::{ClockQuality, ClockSource};
use multiview_framestore::TileStore;
use proptest::prelude::*;

/// A policy with easy-to-reason-about thresholds for the tests.
fn policy() -> EpochPolicy {
    EpochPolicy {
        deadband_ns: 2_000_000,         // 2 ms
        slew_max_ns: 5_000_000,         // 5 ms per observation
        step_threshold_ns: 250_000_000, // 250 ms
    }
}

/// An epoch candidate anchored at media 0 with the canonical ns rate.
fn candidate(wall_at_anchor_ns: i64) -> WallClockRef {
    WallClockRef::new(wall_at_anchor_ns, 0, EPOCH_RATE)
}

// ---------------------------------------------------------------------------
// EpochTracker: hold / slew / step
// ---------------------------------------------------------------------------

#[test]
fn tracker_anchors_on_the_first_observation() {
    let mut t = EpochTracker::new(policy());
    assert_eq!(t.current(), None, "no epoch before the first observation");
    let c = candidate(1_750_000_000_000_000_000);
    assert_eq!(t.observe(c), EpochUpdate::Anchored);
    assert_eq!(
        t.current(),
        Some(c),
        "the first observation anchors exactly"
    );
}

#[test]
fn tracker_holds_inside_the_deadband() {
    let mut t = EpochTracker::new(policy());
    let c = candidate(1_750_000_000_000_000_000);
    let _ = t.observe(c);
    // 1.5 ms of wall jitter is inside the 2 ms deadband: the published epoch
    // must NOT move (consumers present against a stable map).
    let jittered = candidate(1_750_000_000_001_500_000);
    assert_eq!(t.observe(jittered), EpochUpdate::Held);
    assert_eq!(
        t.current(),
        Some(c),
        "the epoch must hold inside the deadband"
    );
}

#[test]
fn tracker_slews_bounded_toward_a_persistent_error() {
    let mut t = EpochTracker::new(policy());
    let _ = t.observe(candidate(1_750_000_000_000_000_000));
    // A persistent +20 ms error (above deadband, below step threshold) must be
    // chased smoothly: each observation moves the anchor by AT MOST slew_max.
    let target = candidate(1_750_000_000_020_000_000);
    assert_eq!(
        t.observe(target),
        EpochUpdate::Slewed {
            adjustment_ns: 5_000_000
        }
    );
    let after_one = t.current().expect("anchored");
    assert_eq!(
        after_one.wall_at_anchor_ns, 1_750_000_000_005_000_000,
        "exactly one slew_max step toward the candidate"
    );
    // Three more observations converge it (5+5+5+5 = 20 ms), then it holds.
    let _ = t.observe(target);
    let _ = t.observe(target);
    let _ = t.observe(target);
    assert_eq!(t.current(), Some(target), "the slew converges exactly");
    assert_eq!(t.observe(target), EpochUpdate::Held);
}

#[test]
fn tracker_steps_on_a_gross_discontinuity() {
    let mut t = EpochTracker::new(policy());
    let _ = t.observe(candidate(1_750_000_000_000_000_000));
    // A 2 s discontinuity (host clock stepped / source changed): re-anchor in
    // one step — the documented Class-2-like re-anchor.
    let stepped = candidate(1_750_000_002_000_000_000);
    assert_eq!(
        t.observe(stepped),
        EpochUpdate::Stepped {
            error_ns: 2_000_000_000
        }
    );
    assert_eq!(t.current(), Some(stepped));
}

#[test]
fn tracker_re_anchors_when_the_rate_changes() {
    let mut t = EpochTracker::new(policy());
    let _ = t.observe(candidate(1_750_000_000_000_000_000));
    // A different media rate is a different timeline: full re-anchor (step),
    // never a cross-rate slew.
    let other_rate = WallClockRef::new(1_750_000_000_000_000_000, 0, Rational::new(90_000, 1));
    assert!(matches!(t.observe(other_rate), EpochUpdate::Stepped { .. }));
    assert_eq!(t.current(), Some(other_rate));
}

#[test]
fn tracker_never_moves_the_media_anchor_on_hold_or_slew() {
    let mut t = EpochTracker::new(policy());
    let first = WallClockRef::new(1_750_000_000_000_000_000, 123_456, EPOCH_RATE);
    let _ = t.observe(first);
    // Candidates carry a different media anchor for the same affine map; the
    // tracker normalises the error at ITS anchor and adjusts only the wall leg.
    let moved = WallClockRef::new(1_750_000_000_020_123_456, 20_000_000 + 123_456, EPOCH_RATE);
    let _ = t.observe(moved);
    let cur = t.current().expect("anchored");
    assert_eq!(cur.media_at_anchor, 123_456, "media anchor is stable");
    assert_eq!(cur.rate, EPOCH_RATE, "rate is stable");
}

proptest! {
    /// Per observation the published wall mapping moves by at most `slew_max`
    /// unless the error exceeds the step threshold (then it lands exactly on
    /// the candidate). Never a panic, never an unbounded move.
    #[test]
    fn prop_tracker_moves_are_bounded(
        anchor in -1_000_000_000_000_000i64..1_000_000_000_000_000i64,
        errors in proptest::collection::vec(-400_000_000i64..400_000_000i64, 1..20),
    ) {
        let p = policy();
        let mut t = EpochTracker::new(p);
        let _ = t.observe(candidate(anchor));
        let mut last = t.current().unwrap().wall_at_anchor_ns;
        for e in errors {
            let target = candidate(last.saturating_add(e));
            let _ = t.observe(target);
            let now = t.current().unwrap().wall_at_anchor_ns;
            let moved = now - last;
            if e.abs() > p.step_threshold_ns {
                prop_assert_eq!(now, target.wall_at_anchor_ns, "gross error steps exactly");
            } else {
                prop_assert!(moved.abs() <= p.slew_max_ns, "move {} exceeds slew bound", moved);
            }
            last = now;
        }
    }
}

// ---------------------------------------------------------------------------
// EpochSampler: anchor math + ADR-T012 source/quality selection
// ---------------------------------------------------------------------------

/// A deterministic wall sampler the tests control.
struct FakeWall {
    mono_mid_ns: i64,
    wall_ns: i64,
}

impl WallClockSampler for FakeWall {
    fn sample(&mut self) -> WallSample {
        WallSample {
            mono_before_ns: self.mono_mid_ns - 1_000,
            wall_ns: self.wall_ns,
            mono_after_ns: self.mono_mid_ns + 1_000,
        }
    }
}

/// An `NtpQuery` the tests control: `Some(reading)` or unavailable.
struct FakeNtp(Option<NtpReading>);

impl NtpQuery for FakeNtp {
    fn read(&mut self) -> Option<NtpReading> {
        self.0
    }
}

/// The shared sampler fixture: the test policy, the assumed-locked system
/// classifier, and a **UTC PHC** (`ptp_utc_offset_ns: 0`) so the PTP-leg
/// tests below exercise the servo-offset math with ns-scale offsets. The
/// timescale-conversion behaviour (TAI PHC, the 37 s default, the residual
/// guard) is pinned by the dedicated *Timescale* tests, which build their own
/// configs from `EpochSamplerConfig::default()`.
fn sampler_config() -> EpochSamplerConfig {
    EpochSamplerConfig {
        policy: policy(),
        sys: SystemRefConfig::new_default(),
        ptp_utc_offset_ns: 0,
    }
}

fn ptp_status(state: LockState, offset_ns: i64) -> ReferenceStatus {
    ReferenceStatus {
        state,
        offset_ns,
        frequency_ppb: 0,
        accepted: 16,
        disciplined: state.is_disciplined(),
    }
}

#[test]
fn sampler_derives_the_anchor_exactly_from_the_system_clock() {
    // seed = 2 s on the monotonic timeline; the wall sample is taken at
    // mono 5 s, wall W. Tick 0's wall instant is exactly W - 3 s.
    let wall = FakeWall {
        mono_mid_ns: 5_000_000_000,
        wall_ns: 1_700_000_000_000_000_000,
    };
    let mut s = EpochSampler::new(2_000_000_000, wall, FakeNtp(None), sampler_config());
    let status = s.sample_once();
    assert_eq!(status.epoch.rate, EPOCH_RATE);
    assert_eq!(
        status.epoch.media_at_anchor, 0,
        "anchored at tick 0 (pts 0)"
    );
    assert_eq!(
        status.epoch.wall_at(0),
        1_700_000_000_000_000_000 - 3_000_000_000,
        "wall@tick0 = wall_now - (mono_mid - seed), exact integer"
    );
    // No PTP handle + an unavailable kernel read: the assumed-NTP-disciplined
    // system clock is the honest source (the documented deployment assumption).
    assert_eq!(status.source, ClockSource::System);
    assert_eq!(status.quality, ClockQuality::Locked);
}

#[test]
fn sampler_prefers_a_disciplined_ptp_reference_and_applies_its_offset() {
    let wall = FakeWall {
        mono_mid_ns: 5_000_000_000,
        wall_ns: 1_700_000_000_000_000_000,
    };
    let handle: LatestState<ReferenceStatus> = LatestState::new();
    // local - master = +250 ns: the local clock is ahead, so the disciplined
    // wall estimate is local - offset.
    handle.publish(ptp_status(LockState::Locked, 250));
    let mut s =
        EpochSampler::new(2_000_000_000, wall, FakeNtp(None), sampler_config()).with_ptp(handle);
    let status = s.sample_once();
    assert_eq!(status.source, ClockSource::Ptp);
    assert_eq!(status.quality, ClockQuality::Locked);
    assert_eq!(
        status.epoch.wall_at(0),
        (1_700_000_000_000_000_000 - 250) - 3_000_000_000,
        "the PTP leg disciplines the wall estimate: wall - servo offset"
    );
}

#[test]
fn sampler_keeps_ptp_through_holdover_but_not_acquiring() {
    let mk = |state: LockState| {
        let wall = FakeWall {
            mono_mid_ns: 5_000_000_000,
            wall_ns: 1_700_000_000_000_000_000,
        };
        let handle: LatestState<ReferenceStatus> = LatestState::new();
        handle.publish(ptp_status(state, 0));
        let mut s = EpochSampler::new(0, wall, FakeNtp(None), sampler_config()).with_ptp(handle);
        s.sample_once()
    };
    // Holdover is still disciplined: PTP stays selected (ADR-T012 §1).
    let h = mk(LockState::Holdover);
    assert_eq!(h.source, ClockSource::Ptp);
    assert_eq!(h.quality, ClockQuality::Holdover);
    // Acquiring is NOT disciplined: selection falls to the system clock.
    let a = mk(LockState::Acquiring);
    assert_eq!(a.source, ClockSource::System);
}

#[test]
fn sampler_reports_freerun_honestly() {
    // An operator who knows the host is undisciplined configures the assumed
    // state Freerun; with no PTP the published quality must say so.
    let wall = FakeWall {
        mono_mid_ns: 1_000_000_000,
        wall_ns: 1_700_000_000_000_000_000,
    };
    let config = EpochSamplerConfig {
        policy: policy(),
        sys: SystemRefConfig {
            est_error_tolerance_ns: 100_000,
            assumed_when_unavailable: LockState::Freerun,
        },
        ptp_utc_offset_ns: 0,
    };
    let mut s = EpochSampler::new(0, wall, FakeNtp(None), config);
    let status = s.sample_once();
    assert_eq!(status.source, ClockSource::System);
    assert_eq!(status.quality, ClockQuality::Freerun);
    // It still publishes a usable epoch — quality labels honesty, it never
    // withholds the map (a consumer free-runs drift-bounded).
    assert_eq!(status.epoch.media_at_anchor, 0);
}

// ---------------------------------------------------------------------------
// Timescale: the PTP leg publishes UTC, never TAI (review finding 1)
// ---------------------------------------------------------------------------

/// The current TAI−UTC offset in nanoseconds (37 s since 2017-01-01; sourced
/// from ptp4l's `currentUtcOffset` in a real ST 2059-2 deployment).
const TAI_UTC_NS: i64 = 37_000_000_000;

#[test]
fn sampler_converts_a_tai_phc_to_a_utc_epoch_by_default() {
    // Standard linuxptp deployment (ADR-T012): the PHC carries PTP time = TAI,
    // so the servo's steady-state `local − master` offset is ≈ −37 s (UTC −
    // TAI) plus a small residual. The published epoch is defined as UTC
    // ("ns past the Unix epoch", stamped into HLS PDT and the RTCP SR NTP
    // word), so the default config must convert TAI → UTC — never publish TAI.
    let wall = FakeWall {
        mono_mid_ns: 5_000_000_000,
        wall_ns: 1_700_000_000_000_000_000,
    };
    let handle: LatestState<ReferenceStatus> = LatestState::new();
    // local(UTC) − master(TAI) = −37 s + 250 ns servo residual.
    handle.publish(ptp_status(LockState::Locked, -TAI_UTC_NS + 250));
    let mut s = EpochSampler::new(
        2_000_000_000,
        wall,
        FakeNtp(None),
        EpochSamplerConfig::default(),
    )
    .with_ptp(handle);
    let status = s.sample_once();
    assert_eq!(status.source, ClockSource::Ptp);
    assert_eq!(status.quality, ClockQuality::Locked);
    assert_eq!(
        status.epoch.wall_at(0),
        (1_700_000_000_000_000_000 - 250) - 3_000_000_000,
        "the published epoch must be UTC (within the servo residual), not TAI: \
         wall@tick0 = (UTC wall − residual) − (mono_mid − seed)"
    );
}

#[test]
fn a_ptp_to_system_reference_transition_does_not_step_the_epoch() {
    // With a TAI PHC correctly converted to UTC, the PTP-leg estimate and the
    // system-leg estimate agree to within the servo residual — so losing PTP
    // (Freerun → selection falls to the system clock) must NOT step the
    // published epoch by 37 s. It holds (or slews within the bound).
    let wall = FakeWall {
        mono_mid_ns: 5_000_000_000,
        wall_ns: 1_700_000_000_000_000_000,
    };
    let handle: LatestState<ReferenceStatus> = LatestState::new();
    handle.publish(ptp_status(LockState::Locked, -TAI_UTC_NS));
    let mut s = EpochSampler::new(
        2_000_000_000,
        wall,
        FakeNtp(None),
        EpochSamplerConfig::default(),
    )
    .with_ptp(handle.clone());
    let first = s.sample_once();
    assert_eq!(first.source, ClockSource::Ptp);

    // The PTP reference drops out of discipline: selection falls to SYS.
    handle.publish(ptp_status(LockState::Freerun, 0));
    let second = s.sample_once();
    assert_eq!(second.source, ClockSource::System);
    assert!(
        !matches!(second.update, EpochUpdate::Stepped { .. }),
        "a ptp→system transition must not step the epoch (got {:?})",
        second.update
    );
    assert_eq!(
        second.update,
        EpochUpdate::Held,
        "the two legs agree exactly here, so the epoch holds"
    );
    let moved = second
        .epoch
        .wall_at(0)
        .saturating_sub(first.epoch.wall_at(0));
    assert!(
        moved.abs() <= policy().slew_max_ns,
        "any movement across the transition stays within the slew bound, got {moved} ns"
    );
}

#[test]
fn a_misconfigured_utc_offset_degrades_and_never_publishes_the_bogus_epoch() {
    // The PHC actually carries UTC (offset ≈ 0) but the config still assumes
    // the standard TAI PHC (37 s): the converted estimate lands 37 s off the
    // system clock. The ≥30 s residual sanity guard must refuse to publish
    // that bogus epoch — it degrades to the system leg with a non-locked
    // quality (and warns) instead.
    let wall = FakeWall {
        mono_mid_ns: 5_000_000_000,
        wall_ns: 1_700_000_000_000_000_000,
    };
    let handle: LatestState<ReferenceStatus> = LatestState::new();
    handle.publish(ptp_status(LockState::Locked, 250));
    let mut s = EpochSampler::new(
        2_000_000_000,
        wall,
        FakeNtp(None),
        EpochSamplerConfig::default(),
    )
    .with_ptp(handle);
    let status = s.sample_once();
    assert_eq!(
        status.source,
        ClockSource::System,
        "the suspect PTP leg must not be published as the epoch source"
    );
    assert_ne!(
        status.quality,
        ClockQuality::Locked,
        "a deployment with a demonstrably-wrong timescale config must not claim locked"
    );
    assert_eq!(
        status.epoch.wall_at(0),
        1_700_000_000_000_000_000 - 3_000_000_000,
        "the published epoch rides the system clock, never the 37 s-off estimate"
    );
}

#[test]
fn an_explicitly_zero_utc_offset_trusts_a_utc_phc() {
    // A deployment whose PHC genuinely carries UTC (nonstandard, e.g.
    // phc2sys keeping the PHC on UTC) sets `ptp_utc_offset_s = 0`: the PTP
    // estimate is then published as-is and stays locked — no guard trip.
    let wall = FakeWall {
        mono_mid_ns: 5_000_000_000,
        wall_ns: 1_700_000_000_000_000_000,
    };
    let handle: LatestState<ReferenceStatus> = LatestState::new();
    handle.publish(ptp_status(LockState::Locked, 250));
    let config = EpochSamplerConfig {
        ptp_utc_offset_ns: 0,
        ..EpochSamplerConfig::default()
    };
    let mut s = EpochSampler::new(2_000_000_000, wall, FakeNtp(None), config).with_ptp(handle);
    let status = s.sample_once();
    assert_eq!(status.source, ClockSource::Ptp);
    assert_eq!(status.quality, ClockQuality::Locked);
    assert_eq!(
        status.epoch.wall_at(0),
        (1_700_000_000_000_000_000 - 250) - 3_000_000_000,
        "a zero offset publishes the UTC PHC estimate directly"
    );
}

#[test]
fn sampler_holds_the_epoch_across_wall_jitter() {
    // Two samples 1 ms apart in wall terms (inside the deadband): the SECOND
    // sample must not move the published epoch.
    struct JitterWall {
        calls: u32,
    }
    impl WallClockSampler for JitterWall {
        fn sample(&mut self) -> WallSample {
            self.calls += 1;
            let wall = if self.calls == 1 {
                1_700_000_000_000_000_000
            } else {
                1_700_000_000_000_000_000 + 1_000_000 // +1 ms jitter
            };
            WallSample {
                mono_before_ns: 5_000_000_000,
                wall_ns: wall,
                mono_after_ns: 5_000_000_000,
            }
        }
    }
    let mut s = EpochSampler::new(0, JitterWall { calls: 0 }, FakeNtp(None), sampler_config());
    let first = s.sample_once();
    let second = s.sample_once();
    assert_eq!(
        first.epoch, second.epoch,
        "wall jitter inside the deadband must not move the epoch"
    );
    assert_eq!(second.update, EpochUpdate::Held);
}

// ---------------------------------------------------------------------------
// Invariant #1: epoch observation never paces the tick stream
// ---------------------------------------------------------------------------
//
// Two tests, two layers of the claim:
//
// * `epoch_observation_does_not_change_the_tick_stream` pins the STRUCTURAL
//   property only: `OutputClock` derives every PTS purely from its tick
//   index, and the sampler holds no handle into the clock, so interleaving
//   epoch samples between ticks cannot change the emitted PTS stream. On its
//   own this is a weak chaos claim — the unpaced `tick()` loop has no
//   deadline, so it cannot catch a stall or a missed schedule.
// * `epoch_churn_cannot_stall_or_skew_a_paced_engine_runtime` is the
//   behavioural gate: a REAL `EngineRuntime`, paced tick-by-tick on a
//   `ManualTimeSource`, must keep every deadline and publish the exact
//   oracle PTS while an `EpochSampler` churns full-speed on a contending OS
//   thread — sharing the run's monotonic time source and a flapping PTP
//   handle, the production topology.

/// A pathological wall source: jumps hours back and forth every sample.
struct WildWall {
    i: i64,
}

impl WallClockSampler for WildWall {
    fn sample(&mut self) -> WallSample {
        self.i += 1;
        let jump = if self.i % 2 == 0 {
            3_600_000_000_000i64
        } else {
            -7_200_000_000_000i64
        };
        WallSample {
            mono_before_ns: self.i.saturating_mul(7),
            wall_ns: 1_700_000_000_000_000_000 + jump,
            mono_after_ns: self.i.saturating_mul(7) + 2,
        }
    }
}

/// Run an output clock for `n` ticks, optionally sampling a chaotic epoch
/// observer between ticks, and collect every PTS.
fn run_clock(n: u64, observe_epoch: bool) -> Vec<MediaTime> {
    let mut clock = OutputClock::new(Rational::FPS_59_94).expect("valid cadence");
    let handle: LatestState<ReferenceStatus> = LatestState::new();
    let mut sampler = EpochSampler::new(0, WildWall { i: 0 }, FakeNtp(None), sampler_config())
        .with_ptp(handle.clone());
    let mut out = Vec::new();
    for i in 0..n {
        if observe_epoch {
            // Flap the PTP reference wildly and re-sample the epoch every tick:
            // none of this may change how many frames are emitted, nor when.
            let state = match i % 4 {
                0 => LockState::Locked,
                1 => LockState::Holdover,
                2 => LockState::Freerun,
                _ => LockState::Acquiring,
            };
            let off = i64::try_from(i).unwrap_or(0).saturating_mul(1_000_003);
            handle.publish(ptp_status(state, off));
            let _ = sampler.sample_once();
        }
        out.push(clock.tick().pts);
    }
    out
}

#[test]
fn epoch_observation_does_not_change_the_tick_stream() {
    // Structural pin only (see the section comment): identical PTS streams
    // prove the sampler cannot reach into `out_pts = f(tick)`; the paced
    // runtime test below is the deadline-keeping (stall/skew) gate.
    const TICKS: u64 = 50_000;
    let baseline = run_clock(TICKS, false);
    let observed = run_clock(TICKS, true);
    assert_eq!(
        baseline, observed,
        "out_pts = f(tick) must be identical with the epoch sampler churning vs off (inv #1)"
    );
}

/// Independent i128 oracle for `out_pts = f(tick)` (the same shape as the
/// runtime soak's), computed WITHOUT the clock's own rescale. Half away from
/// zero made explicit.
fn oracle_pts_ns(tick: i64, cadence: Rational) -> i64 {
    let numerator: i128 = i128::from(tick) * 1_000_000_000_i128 * i128::from(cadence.den);
    let denominator: i128 = i128::from(cadence.num);
    let q = numerator / denominator;
    let r = numerator % denominator;
    let rounded = if r * 2 >= denominator { q + 1 } else { q };
    i64::try_from(rounded).expect("oracle pts fits in i64")
}

/// A wall source for the churn thread: brackets with the RUN's shared
/// monotonic time source (the production `SystemWallSampler` shape) around a
/// wall reading that jumps hours back and forth every sample.
struct ChurningWall {
    time: Arc<dyn TimeSource>,
    i: i64,
}

impl WallClockSampler for ChurningWall {
    fn sample(&mut self) -> WallSample {
        self.i += 1;
        let jump = if self.i % 2 == 0 {
            3_600_000_000_000i64
        } else {
            -7_200_000_000_000i64
        };
        let mono_before_ns = self.time.now_nanos();
        let wall_ns = 1_700_000_000_000_000_000 + jump;
        let mono_after_ns = self.time.now_nanos();
        WallSample {
            mono_before_ns,
            wall_ns,
            mono_after_ns,
        }
    }
}

/// The compact per-tick snapshot the paced-runtime chaos test publishes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TickSnapshot {
    index: u64,
    pts_ns: i64,
}

#[test]
#[allow(
    // reason: like the runtime soak this is one cohesive chaos scenario
    // (build drive -> spawn churn thread -> pace+verify every tick) that
    // reads better as a single narrative than carved into helpers.
    clippy::too_many_lines
)]
fn epoch_churn_cannot_stall_or_skew_a_paced_engine_runtime() {
    // Big enough to interleave thousands of sampler iterations with the tick
    // loop; tiny canvas keeps the CPU reference compositor fast in debug.
    const TICKS: u64 = 20_000;
    let (w, h) = (32u32, 24u32);
    let cadence = Rational::FPS_59_94;

    // One never-fed store: every tile is NoSignal, yet one valid frame per
    // tick must still land on schedule.
    let mut stores = HashMap::new();
    stores.insert(
        "cam".to_owned(),
        Arc::new(TileStore::<Nv12Image>::with_defaults("cam")),
    );
    let color = ColorInfo::default().resolve_defaults(1920, 1080);
    let layout = Layout {
        name: "epoch-chaos".to_owned(),
        canvas: Canvas {
            width: w,
            height: h,
            fps_num: 60_000,
            fps_den: 1_001,
        },
        cells: vec![Cell {
            x: 0.0,
            y: 0.0,
            w: 1.0,
            h: 1.0,
            z: 0,
            fit: FitMode::Contain,
            source: Some("cam".to_owned()),
            ..Cell::default()
        }],
    };
    let drive = CompositorDrive::new(
        Arc::new(layout),
        stores,
        Nv12Image::solid(w, h, 16, 128, 128, color).expect("nosignal card"),
        CanvasColor::default(),
        LinearRgba::TRANSPARENT,
    )
    .expect("drive");

    let clock = OutputClock::new(cadence).expect("valid cadence");
    let time_source = Arc::new(ManualTimeSource::new());
    let ts_for_runtime: Arc<dyn TimeSource> = time_source.clone();
    let publisher: Arc<EnginePublisher<TickSnapshot, u64>> = Arc::new(EnginePublisher::new(64));
    let mut runtime = EngineRuntime::new(clock, drive, ts_for_runtime, CooperativePacer);
    let seed = runtime.seed_nanos();

    // ---- The churn thread: the production epoch-sampler topology (the run's
    // shared monotonic source + a wait-free PTP handle), driven full-speed
    // with a wild wall clock and a flapping PTP reference.
    let stop_churn = Arc::new(AtomicBool::new(false));
    let churn_time: Arc<dyn TimeSource> = time_source.clone();
    let churn_stop = Arc::clone(&stop_churn);
    let churn = std::thread::spawn(move || {
        let handle: LatestState<ReferenceStatus> = LatestState::new();
        let mut sampler = EpochSampler::new(
            seed,
            ChurningWall {
                time: churn_time,
                i: 0,
            },
            FakeNtp(None),
            EpochSamplerConfig::default(),
        )
        .with_ptp(handle.clone());
        let mut iterations: u64 = 0;
        while !churn_stop.load(Ordering::Acquire) {
            let state = match iterations % 4 {
                0 => LockState::Locked,
                1 => LockState::Holdover,
                2 => LockState::Freerun,
                _ => LockState::Acquiring,
            };
            let off = i64::try_from(iterations)
                .unwrap_or(0)
                .saturating_mul(1_000_003);
            handle.publish(ptp_status(state, off));
            let _ = sampler.sample_once();
            iterations = iterations.saturating_add(1);
            std::thread::yield_now();
        }
        iterations
    });

    // ---- Drive the runtime in the background; pace it tick-by-tick here.
    let stop = StopSignal::new();
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_time()
        .build()
        .expect("tokio runtime");
    let run_pub = Arc::clone(&publisher);
    let engine_join = rt.spawn(async move {
        runtime
            .run_for(
                run_pub.as_ref(),
                &stop,
                TICKS,
                |f| TickSnapshot {
                    index: f.tick.index,
                    pts_ns: f.pts().as_nanos(),
                },
                |f| Some(f.tick.index),
            )
            .await
    });

    let pts_at =
        |i: u64| -> i64 { seed + oracle_pts_ns(i64::try_from(i).expect("tick fits"), cadence) };
    for i in 0..TICKS {
        // Before tick i's deadline the runtime must be parked at exactly i
        // emitted ticks — it paces to the clock, never runs ahead, no matter
        // how hard the sampler churns. (Tick 0's deadline equals the seed,
        // which the source already meets — skip i == 0.)
        if i >= 1 {
            assert_eq!(
                publisher.state.sequence(),
                i,
                "runtime ran ahead of its deadline at tick {i} while the epoch sampler churned"
            );
        }
        time_source.set(pts_at(i));
        let started = Instant::now();
        let snap = loop {
            if let Some(snap) = publisher.state.latest() {
                if snap.index >= i {
                    break snap;
                }
            }
            assert!(
                started.elapsed() < Duration::from_secs(30),
                "runtime STALLED at tick {i} (epoch churn must never stall the engine)"
            );
            std::thread::yield_now();
        };
        assert_eq!(snap.index, i, "exactly one frame per tick, in order");
        assert_eq!(
            snap.pts_ns,
            oracle_pts_ns(i64::try_from(i).expect("tick fits"), cadence),
            "published pts must equal the independent oracle at tick {i}"
        );
    }

    // The engine ran every tick and returned cleanly — never stalled.
    let outcome = rt.block_on(engine_join).expect("join").expect("run");
    assert_eq!(outcome.ticks, TICKS, "the runtime produced every tick");
    assert_eq!(outcome.stop, multiview_engine::RunStop::Completed);

    stop_churn.store(true, Ordering::Release);
    let iterations = churn.join().expect("churn thread");
    assert!(
        iterations > 0,
        "the churn thread must actually have sampled (the chaos is real)"
    );
}
