//! PTP / SMPTE **ST 2059-2** servo **math** — a pure, disciplined-reference-clock
//! estimator (always compiled, unit + property tested).
//!
//! ST 2110-10 mandates the **ST 2059-2 profile of IEEE 1588-2008 (PTP)** as the
//! media-reference timing for uncompressed-over-IP. A PTP receiver exchanges
//! Sync / Follow-Up / Delay-Req / Delay-Resp messages to measure, for each
//! exchange, the **offset** of the local clock from the grandmaster and the
//! **mean path delay**. Those raw measurements are noisy (network jitter,
//! timestamp granularity); a **servo** filters them into a smooth estimate of the
//! grandmaster time and the local frequency error so a disciplined reference can
//! be derived.
//!
//! This module is the **servo math only** — the filtering and convergence logic
//! over injected `(offset, delay)` samples. It is a pure value machine: samples
//! in, a refined offset/frequency estimate out, no sockets, no clocks of its own,
//! no `.await`. The actual hardware PTP NIC / PHC (PTP Hardware Clock) binding
//! lives behind the off-by-default **`ptp`** feature in the `phc` submodule
//! (compiled only with that feature enabled) and is compile-verified only (this
//! environment has no PTP NIC).
//!
//! ## Invariant #1 is preserved: PTP does **not** pace the output clock
//!
//! This is load-bearing. The Multiview output clock
//! ([`OutputClock`](crate::clock::OutputClock)) remains the **single pacing
//! authority**: `out_pts = f(tick)` is computed purely from the integer tick
//! counter and the fixed cadence, exactly as before. The PTP servo disciplines a
//! **separate reference estimate** — used to *timestamp ingest in the
//! grandmaster's timebase* and to align ST 2110 senders/receivers — and it can
//! neither stall nor speed up the tick loop. A drifting, jittering, or absent
//! grandmaster changes only the reference estimate this servo reports; it does
//! **not** change how many frames the output emits, nor when, because the runtime
//! paces on its own monotonic [`TimeSource`](crate::clock::TimeSource), never on
//! the PTP estimate. (If a deployment later chooses to *seed* the output clock's
//! origin from PTP at startup, that is a one-time anchor, not per-tick pacing —
//! the cadence stays the exact rational.)
//!
//! All servo arithmetic is in **integer nanoseconds** (offsets) and **parts per
//! billion** (frequency), never float fps — consistent with invariant #3.

/// One PTP measurement: the instantaneous master↔local **offset** and the
/// measured **mean path delay**, both in nanoseconds.
///
/// Offset is `local_clock - master_clock`: a **positive** offset means the local
/// clock is *ahead* of the grandmaster and must be slowed / stepped back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PtpSample {
    /// Measured offset of the local clock from the master, in nanoseconds
    /// (`local - master`).
    pub offset_ns: i64,
    /// Measured mean path delay, in nanoseconds (non-negative).
    pub delay_ns: i64,
}

impl PtpSample {
    /// Construct a sample, clamping a negative path delay to zero (delay is
    /// physically non-negative; a negative measurement is timestamp noise).
    #[must_use]
    pub const fn new(offset_ns: i64, delay_ns: i64) -> Self {
        Self {
            offset_ns,
            delay_ns: if delay_ns < 0 { 0 } else { delay_ns },
        }
    }
}

/// Tuning for the [`PtpServo`].
///
/// The servo is a first-order IIR (exponential) filter on the offset plus a
/// frequency estimate derived from the smoothed-offset trend. The smoothing
/// strength is expressed as an integer reciprocal weight `alpha_recip`: each new
/// sample contributes `1 / alpha_recip` of the correction, so a larger value is
/// heavier smoothing (slower but steadier convergence). A `step_threshold_ns`
/// bounds how far a single sample may pull the estimate: a measurement further
/// off than this is treated as a **step** (e.g. a grandmaster change) and applied
/// in full rather than filtered, mirroring the PTP servo "step vs slew" rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ServoConfig {
    /// Reciprocal smoothing weight for the offset filter (`>= 1`). Larger =
    /// heavier smoothing.
    pub alpha_recip: i64,
    /// Offsets whose magnitude exceeds this are applied as a hard step rather
    /// than slewed.
    pub step_threshold_ns: i64,
    /// An outlier guard: samples whose measured delay exceeds this many times the
    /// running median-ish average delay are ignored as path-asymmetry spikes
    /// (expressed as a percentage, e.g. `400` = 4×). Zero disables the guard.
    pub delay_outlier_pct: i64,
}

impl ServoConfig {
    /// A sane default: weight 8 (each sample contributes 1/8 of the correction),
    /// a 1 ms step threshold, and a 4× delay-outlier guard.
    #[must_use]
    pub const fn new_default() -> Self {
        Self {
            alpha_recip: 8,
            step_threshold_ns: 1_000_000,
            delay_outlier_pct: 400,
        }
    }
}

impl Default for ServoConfig {
    fn default() -> Self {
        Self::new_default()
    }
}

/// The PTP/ST 2059-2 servo: filters noisy `(offset, delay)` samples into a
/// smoothed offset estimate and a frequency-error estimate (parts per billion).
///
/// Pure and deterministic: feed it [`PtpSample`]s with [`PtpServo::update`] and
/// read [`PtpServo::offset_ns`] / [`PtpServo::frequency_ppb`]. It owns no clock
/// and performs no I/O.
#[derive(Debug, Clone)]
pub struct PtpServo {
    config: ServoConfig,
    /// The current smoothed offset estimate (ns).
    offset_ns: i64,
    /// The current frequency-error estimate (parts per billion). Positive means
    /// the local clock runs fast relative to the master.
    frequency_ppb: i64,
    /// Running average path delay (ns), for the outlier guard.
    avg_delay_ns: i64,
    /// Whether any sample has been applied yet (the first sample anchors the
    /// estimate exactly).
    locked: bool,
    /// Count of samples accepted (for diagnostics / lock detection).
    accepted: u64,
}

impl PtpServo {
    /// Create a servo with the given tuning. The estimate starts at zero offset
    /// and zero frequency error until the first sample anchors it.
    #[must_use]
    pub fn new(config: ServoConfig) -> Self {
        let alpha_recip = config.alpha_recip.max(1);
        Self {
            config: ServoConfig {
                alpha_recip,
                ..config
            },
            offset_ns: 0,
            frequency_ppb: 0,
            avg_delay_ns: 0,
            locked: false,
            accepted: 0,
        }
    }

    /// The current smoothed offset estimate (`local - master`), in nanoseconds.
    #[must_use]
    pub const fn offset_ns(&self) -> i64 {
        self.offset_ns
    }

    /// The current frequency-error estimate, in parts per billion (positive =
    /// local clock fast).
    #[must_use]
    pub const fn frequency_ppb(&self) -> i64 {
        self.frequency_ppb
    }

    /// The running average path delay (ns).
    #[must_use]
    pub const fn avg_delay_ns(&self) -> i64 {
        self.avg_delay_ns
    }

    /// The number of samples accepted so far.
    #[must_use]
    pub const fn accepted(&self) -> u64 {
        self.accepted
    }

    /// Whether the servo has anchored on at least one sample.
    #[must_use]
    pub const fn is_locked(&self) -> bool {
        self.locked
    }

    /// The estimated master time corresponding to a `local_ns` reading: subtract
    /// the offset (`local - master = offset`, so `master = local - offset`).
    #[must_use]
    pub const fn master_from_local(&self, local_ns: i64) -> i64 {
        local_ns.saturating_sub(self.offset_ns)
    }

    /// Feed one measurement into the servo, returning whether it was **accepted**
    /// (`true`) or rejected as a delay outlier (`false`).
    ///
    /// The first accepted sample anchors the offset exactly. Subsequent samples
    /// are either applied as a hard step (when their offset is further than
    /// [`ServoConfig::step_threshold_ns`] from the current estimate) or
    /// exponentially smoothed toward the new measurement. The frequency estimate
    /// tracks the smoothed change in offset.
    pub fn update(&mut self, sample: PtpSample) -> bool {
        // Outlier guard: reject obviously asymmetric delay spikes once we have a
        // running average to compare against.
        if self.locked && self.config.delay_outlier_pct > 0 && self.avg_delay_ns > 0 {
            // limit = avg_delay * pct / 100, computed in i128 to avoid overflow.
            let limit = i128::from(self.avg_delay_ns)
                .saturating_mul(i128::from(self.config.delay_outlier_pct))
                / 100;
            if i128::from(sample.delay_ns) > limit {
                return false;
            }
        }

        // Update the running average delay (same exponential weight as the
        // offset filter).
        if self.locked {
            let recip = self.config.alpha_recip.max(1);
            let delta = sample.delay_ns.saturating_sub(self.avg_delay_ns);
            self.avg_delay_ns = self.avg_delay_ns.saturating_add(delta.div_euclid(recip));
        } else {
            self.avg_delay_ns = sample.delay_ns;
        }

        if !self.locked {
            // First sample: anchor exactly.
            self.offset_ns = sample.offset_ns;
            self.frequency_ppb = 0;
            self.locked = true;
            self.accepted = self.accepted.saturating_add(1);
            return true;
        }

        let error = sample.offset_ns.saturating_sub(self.offset_ns);
        // `saturating_abs` avoids the `i64::MIN.abs()` overflow panic.
        if error.saturating_abs() > self.config.step_threshold_ns {
            // Large discontinuity: step the estimate (grandmaster change / reset).
            self.offset_ns = sample.offset_ns;
            // A step invalidates the frequency trend; reset it.
            self.frequency_ppb = 0;
        } else {
            // Slew: move 1/alpha of the way toward the new measurement.
            let recip = self.config.alpha_recip.max(1);
            let correction = error.div_euclid(recip);
            self.offset_ns = self.offset_ns.saturating_add(correction);
            // Frequency error tracks the per-sample drift of the smoothed offset,
            // also exponentially smoothed. `correction` is the smoothed offset
            // delta per sample; fold it into the frequency estimate.
            let freq_delta = correction.saturating_sub(self.frequency_ppb);
            self.frequency_ppb = self
                .frequency_ppb
                .saturating_add(freq_delta.div_euclid(recip));
        }
        self.accepted = self.accepted.saturating_add(1);
        true
    }
}

/// The lock / clock-class state of the disciplined reference, as surfaced on the
/// wall-clock badge.
///
/// This is the ST 2059 / IEEE 1588 receiver lifecycle reduced to the four states
/// an operator needs to read: the reference is either absent, being acquired,
/// locked, or coasting (holdover). It is a strict superset of what the on-screen
/// badge shows and is computed purely from the servo's offset estimate plus the
/// arrival timing of injected samples — it owns no clock and never paces output.
///
/// `Acquiring` reports as *not yet disciplined* on purpose (honest badge): a
/// single fresh sample is not a lock.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum LockState {
    /// No usable reference: either nothing has been sampled yet, or the reference
    /// was lost and the holdover window has since expired. The clock free-runs on
    /// its local oscillator.
    #[default]
    Freerun,
    /// Samples are arriving and the servo is converging, but not enough
    /// consecutive in-tolerance samples have been seen to declare a lock.
    Acquiring,
    /// Disciplined and locked: the offset estimate is within tolerance and fresh
    /// samples keep arriving.
    Locked,
    /// The reference stopped delivering fresh samples (or drifted out of
    /// tolerance) while locked; the clock coasts on the last good estimate until
    /// the holdover window expires (then -> `Freerun`).
    Holdover,
}

impl LockState {
    /// A short, lower-case textual label (accessibility — the badge reads without
    /// colour).
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Freerun => "freerun",
            Self::Acquiring => "acquiring",
            Self::Locked => "locked",
            Self::Holdover => "holdover",
        }
    }

    /// Whether the reference is currently disciplined — i.e. the offset estimate
    /// is trustworthy to reference accuracy. True for `Locked` and `Holdover`
    /// (coasting on a recent good lock); false for `Freerun` and `Acquiring`.
    #[must_use]
    pub const fn is_disciplined(self) -> bool {
        matches!(self, Self::Locked | Self::Holdover)
    }
}

/// Tuning for the [`ReferenceTracker`]: the servo math plus the lock-state
/// machine thresholds.
///
/// All timing thresholds are in **integer nanoseconds** against the monotonic
/// timestamp the caller passes with each sample/tick — never float seconds
/// (invariant #3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReferenceConfig {
    /// Tuning for the underlying offset/frequency servo.
    pub servo: ServoConfig,
    /// Maximum magnitude of `|sample.offset - estimate|` (ns) for a sample to
    /// count as **in tolerance** toward a lock. A sample that the servo accepts
    /// but that lands outside this band resets the consecutive-in-tolerance
    /// counter (acquisition restarts).
    pub lock_tolerance_ns: i64,
    /// How many consecutive in-tolerance accepted samples are required to declare
    /// a lock (`Acquiring` -> `Locked`). Clamped to at least 1.
    pub lock_samples: u32,
    /// If no accepted sample arrives within this many ns of the last accepted
    /// sample, a `Locked`/`Acquiring` reference is considered to have lost its
    /// feed: `Locked` drops to `Holdover`. Must be positive to be meaningful.
    pub stale_after_ns: i64,
    /// Total ns from the last accepted sample after which a `Holdover` reference
    /// is abandoned (`Holdover` -> `Freerun`). Should exceed `stale_after_ns`.
    pub holdover_window_ns: i64,
}

impl ReferenceConfig {
    /// A sane default: the default servo, a 50 us lock tolerance, lock after 8
    /// consecutive in-tolerance samples, stale after 2 s without a sample, and a
    /// 10 s holdover window.
    #[must_use]
    pub const fn new_default() -> Self {
        Self {
            servo: ServoConfig::new_default(),
            lock_tolerance_ns: 50_000,
            lock_samples: 8,
            stale_after_ns: 2_000_000_000,
            holdover_window_ns: 10_000_000_000,
        }
    }
}

impl Default for ReferenceConfig {
    fn default() -> Self {
        Self::new_default()
    }
}

/// A wait-free-friendly snapshot of the reference tracker's state, suitable for
/// publishing to the wall-clock badge (control / preview / overlay) without ever
/// pacing the engine.
///
/// `Copy` and small so it can be stored in a single-slot latest-state cell and
/// read by any consumer at any time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReferenceStatus {
    /// The current lock/clock-class state.
    pub state: LockState,
    /// The current smoothed offset estimate (`local - master`), in ns.
    pub offset_ns: i64,
    /// The current frequency-error estimate, in parts per billion.
    pub frequency_ppb: i64,
    /// Number of samples accepted by the servo so far.
    pub accepted: u64,
    /// Whether the reference is currently disciplined (`Locked` or `Holdover`).
    pub disciplined: bool,
}

/// The PTP **reference tracker**: the offset servo plus the lock-state /
/// clock-class state machine that feeds the wall-clock reference badge.
///
/// Pure and deterministic over **injected** measurements: callers feed accepted
/// PTP samples with [`ReferenceTracker::observe`] (carrying a monotonic sample
/// timestamp in ns) and advance time-only with [`ReferenceTracker::tick`] (to let
/// staleness / holdover expiry fire when no sample arrives). The hardware PHC read
/// that produces those samples lives behind the off-by-default `ptp` feature (the
/// `phc` module); this state machine never reads a clock or performs I/O and is
/// fully unit-tested without a NIC.
///
/// **Invariant #1:** the tracker only *informs* the reference estimate / badge —
/// it can neither stall nor speed up the output clock. It exposes no method the
/// output clock calls and holds no reference to it (see the module docs).
#[derive(Debug, Clone)]
pub struct ReferenceTracker {
    config: ReferenceConfig,
    servo: PtpServo,
    state: LockState,
    /// Consecutive accepted in-tolerance samples since the last reset.
    in_tolerance_streak: u32,
    /// Monotonic timestamp (ns) of the last accepted sample, if any.
    last_sample_ns: Option<i64>,
    /// The latest monotonic timestamp the tracker has observed (sample or tick),
    /// so staleness is measured against "now", not only against sample arrivals.
    now_ns: i64,
}

impl ReferenceTracker {
    /// Build a tracker with the given tuning. It starts in [`LockState::Freerun`]
    /// with an un-anchored servo.
    #[must_use]
    pub fn new(config: ReferenceConfig) -> Self {
        Self {
            servo: PtpServo::new(config.servo),
            config,
            state: LockState::Freerun,
            in_tolerance_streak: 0,
            last_sample_ns: None,
            now_ns: 0,
        }
    }

    /// The current lock/clock-class state.
    #[must_use]
    pub const fn state(&self) -> LockState {
        self.state
    }

    /// The servo's current smoothed offset estimate (`local - master`), in ns.
    #[must_use]
    pub const fn servo_offset_ns(&self) -> i64 {
        self.servo.offset_ns()
    }

    /// The servo's current frequency-error estimate, in parts per billion.
    #[must_use]
    pub const fn servo_frequency_ppb(&self) -> i64 {
        self.servo.frequency_ppb()
    }

    /// The number of samples the servo has accepted so far.
    #[must_use]
    pub const fn accepted(&self) -> u64 {
        self.servo.accepted()
    }

    /// The estimated master time for a `local_ns` monotonic reading. Meaningful
    /// only while [`LockState::is_disciplined`]; returns the local reading
    /// unchanged (zero offset) before the first lock.
    #[must_use]
    pub const fn master_from_local(&self, local_ns: i64) -> i64 {
        self.servo.master_from_local(local_ns)
    }

    /// A consistent point-in-time snapshot for the wall-clock badge.
    #[must_use]
    pub const fn status(&self) -> ReferenceStatus {
        ReferenceStatus {
            state: self.state,
            offset_ns: self.servo.offset_ns(),
            frequency_ppb: self.servo.frequency_ppb(),
            accepted: self.servo.accepted(),
            disciplined: self.state.is_disciplined(),
        }
    }

    /// Borrow the underlying servo (for finer metrics).
    #[must_use]
    pub const fn servo(&self) -> &PtpServo {
        &self.servo
    }

    /// Feed one PTP measurement, captured at monotonic time `now_ns`.
    ///
    /// Returns whether the servo **accepted** the sample (`false` if it was
    /// rejected as a delay outlier — a rejected sample advances neither the lock
    /// streak nor the staleness clock). On acceptance the lock-state machine is
    /// re-evaluated:
    ///
    /// * an in-tolerance sample (`|offset - estimate| <= lock_tolerance_ns`)
    ///   extends the streak; `lock_samples` of them in a row reach `Locked`;
    /// * an out-of-tolerance (but servo-accepted) sample resets the streak, so
    ///   acquisition restarts;
    /// * any accepted sample refreshes the staleness clock and pulls a coasting
    ///   `Holdover`/`Freerun` reference back into `Acquiring`.
    pub fn observe(&mut self, sample: PtpSample, now_ns: i64) -> bool {
        self.now_ns = now_ns.max(self.now_ns);
        // Measure tolerance against the estimate *before* this sample is folded
        // in, mirroring how a PTP servo classifies the incoming measurement.
        let estimate_before = self.servo.offset_ns();
        let accepted = self.servo.update(sample);
        if !accepted {
            // Rejected outlier: no state change, no staleness refresh.
            return false;
        }
        self.last_sample_ns = Some(now_ns);

        let error = sample.offset_ns.saturating_sub(estimate_before);
        let in_tolerance = error.saturating_abs() <= self.config.lock_tolerance_ns;

        if in_tolerance {
            self.in_tolerance_streak = self.in_tolerance_streak.saturating_add(1);
        } else {
            // A real but out-of-band measurement: acquisition restarts. A fresh
            // out-of-tolerance sample still counts as "1" toward a new streak so
            // a steady but slightly-off reference can still eventually lock.
            self.in_tolerance_streak = 0;
        }

        let needed = self.config.lock_samples.max(1);
        self.state = if self.in_tolerance_streak >= needed {
            LockState::Locked
        } else {
            // Either still climbing the streak, or just reset it: Acquiring.
            LockState::Acquiring
        };
        true
    }

    /// Advance time without a new sample, to `now_ns`. Lets staleness fire when a
    /// reference stops delivering: a `Locked`/`Acquiring` reference whose last
    /// accepted sample is older than `stale_after_ns` drops to `Holdover`; a
    /// `Holdover` reference past `holdover_window_ns` is abandoned to `Freerun`.
    ///
    /// `now_ns` is clamped to be non-decreasing, so an out-of-order tick can never
    /// make the reference appear *fresher* than it is.
    pub fn tick(&mut self, now_ns: i64) {
        self.now_ns = now_ns.max(self.now_ns);
        let Some(last) = self.last_sample_ns else {
            // Never had a sample: stays Freerun regardless of elapsed time.
            return;
        };
        let age = self.now_ns.saturating_sub(last);

        if age >= self.config.holdover_window_ns && self.config.holdover_window_ns > 0 {
            // Coasted past the holdover window: abandon the reference. The servo
            // estimate is intentionally left as-is (the badge reads Freerun, so it
            // is no longer presented as disciplined).
            self.state = LockState::Freerun;
            self.in_tolerance_streak = 0;
            return;
        }

        if age >= self.config.stale_after_ns && self.config.stale_after_ns > 0 {
            // Stale feed: a disciplined or acquiring reference coasts on its last
            // good estimate. Freerun stays Freerun. The lock is no longer
            // continuous, so the in-tolerance streak resets — a sample arriving
            // later must re-run acquisition rather than instantly re-lock.
            match self.state {
                LockState::Locked | LockState::Acquiring => {
                    self.state = LockState::Holdover;
                    self.in_tolerance_streak = 0;
                }
                LockState::Holdover | LockState::Freerun => {}
            }
        }
    }
}

/// Hardware PTP binding — **feature `ptp`, compile-verified only**.
///
/// Reading the host PTP Hardware Clock (PHC) and driving its frequency requires a
/// real PTP-capable NIC and OS clock-discipline syscalls, neither of which exist
/// in this environment. The binding therefore lives behind the off-by-default
/// `ptp` feature and is compile-verified only; its correctness rests on the pure,
/// fully-tested [`PtpServo`] above. Crucially, even with `ptp` enabled the servo
/// disciplines a **separate reference clock** — the output clock's
/// `out_pts = f(tick)` is untouched (see the module docs).
#[cfg(feature = "ptp")]
pub mod phc {
    use super::{
        LockState, PtpSample, PtpServo, ReferenceConfig, ReferenceStatus, ReferenceTracker,
        ServoConfig,
    };
    use crate::isolation::LatestState;

    /// A disciplined-reference clock driven by the PTP servo from sampled
    /// `(offset, delay)` measurements.
    ///
    /// In a real deployment the samples come from a PTP receiver reading the host
    /// PHC; here the type is constructed and exercised purely through its servo,
    /// so it is meaningful to compile-check but cannot be runtime-verified without
    /// a NIC.
    #[derive(Debug, Clone)]
    pub struct DisciplinedReference {
        servo: PtpServo,
    }

    impl DisciplinedReference {
        /// Build a disciplined reference with the given servo tuning.
        #[must_use]
        pub fn new(config: ServoConfig) -> Self {
            Self {
                servo: PtpServo::new(config),
            }
        }

        /// Apply a PTP measurement (in a real build, read from the PHC); returns
        /// whether the sample was accepted.
        pub fn apply(&mut self, sample: PtpSample) -> bool {
            self.servo.update(sample)
        }

        /// The estimated grandmaster time for a local monotonic reading.
        #[must_use]
        pub const fn master_from_local(&self, local_ns: i64) -> i64 {
            self.servo.master_from_local(local_ns)
        }

        /// Borrow the underlying servo (for metrics).
        #[must_use]
        pub const fn servo(&self) -> &PtpServo {
            &self.servo
        }
    }

    /// One paired reading: the PHC instant and the bracketing system-clock
    /// instants from a sys → phc → sys "sandwich", all in nanoseconds.
    ///
    /// The two system reads bracket the single PHC read, so the system clock's
    /// instant *at the moment of* the PHC read is estimated as the midpoint and
    /// the read's own latency bounds the timestamp uncertainty (used as the path
    /// delay). This is a read-side estimate; no PTP ioctl is required.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct PhcReading {
        /// The system (`CLOCK_REALTIME`) instant read *before* the PHC, in ns.
        pub sys_before_ns: i64,
        /// The PHC (grandmaster-disciplined) instant, in ns.
        pub phc_ns: i64,
        /// The system instant read *after* the PHC, in ns.
        pub sys_after_ns: i64,
    }

    impl PhcReading {
        /// The estimated offset of the **local** (system) clock from the **master**
        /// (PHC), as `local - master` (the [`PtpSample`] convention): the system
        /// midpoint minus the PHC instant.
        #[must_use]
        pub const fn offset_ns(&self) -> i64 {
            // midpoint = sys_before + (sys_after - sys_before)/2, computed without
            // overflowing on large epoch nanoseconds.
            let span = self.sys_after_ns.saturating_sub(self.sys_before_ns);
            let midpoint = self.sys_before_ns.saturating_add(span.div_euclid(2));
            midpoint.saturating_sub(self.phc_ns)
        }

        /// The read-latency bound used as the path delay: half the system-bracket
        /// span (non-negative).
        #[must_use]
        pub const fn delay_ns(&self) -> i64 {
            let span = self.sys_after_ns.saturating_sub(self.sys_before_ns);
            if span < 0 {
                0
            } else {
                span.div_euclid(2)
            }
        }

        /// Convert this reading into a [`PtpSample`] for the servo/tracker.
        #[must_use]
        pub const fn to_sample(&self) -> PtpSample {
            PtpSample::new(self.offset_ns(), self.delay_ns())
        }
    }

    /// The injectable source of PHC-vs-system readings.
    ///
    /// Production wires [`RealPhcSource`] (reads `/dev/ptpN` on Linux); tests wire
    /// a fake whose readings they control, so the whole sampling-and-discipline
    /// path is deterministically exercised without a NIC. Implementors are
    /// `Send` so a sampler can own one on a dedicated off-hot-path thread.
    pub trait PhcSource: Send {
        /// Take one paired PHC/system reading.
        ///
        /// # Errors
        ///
        /// Returns [`PhcError`] if the underlying clock read fails (e.g. the PHC
        /// device disappeared); the sampler treats this as a missing sample (it
        /// lets the reference go stale -> holdover), never a panic.
        fn read(&mut self) -> Result<PhcReading, PhcError>;
    }

    /// A failure to read the PHC.
    #[derive(Debug, thiserror::Error)]
    pub enum PhcError {
        /// The PHC device could not be opened.
        #[error("opening PHC device {path}: {source}")]
        Open {
            /// The device path that failed to open.
            path: String,
            /// The underlying I/O error.
            #[source]
            source: std::io::Error,
        },
        /// A clock read failed.
        #[error("reading PHC clock: {0}")]
        Read(String),
        /// This platform cannot read a PHC (non-Linux).
        #[error("PHC reads are only supported on Linux")]
        Unsupported,
    }

    /// Samples a [`PhcSource`] on demand, feeds each reading into a
    /// [`ReferenceTracker`], and publishes the resulting [`ReferenceStatus`] into a
    /// wait-free latest-state slot for the wall-clock badge.
    ///
    /// **Invariant #1 / #10:** the sampler is the only thing that *reads* the PHC;
    /// it publishes through [`LatestState`] (a single wait-free atomic store) and
    /// never touches the output clock. A missing/failed read does not stall
    /// anything — it simply lets the tracker's staleness/holdover logic fire on the
    /// next [`PhcSampler::tick`]. Drive it from a dedicated sampled thread (like the
    /// HAL load poller), never from the per-tick output loop.
    pub struct PhcSampler<S: PhcSource> {
        source: S,
        tracker: ReferenceTracker,
        status: LatestState<ReferenceStatus>,
    }

    impl<S: PhcSource> PhcSampler<S> {
        /// Build a sampler over the given PHC source and tracker tuning.
        #[must_use]
        pub fn new(source: S, config: ReferenceConfig) -> Self {
            let tracker = ReferenceTracker::new(config);
            let status = LatestState::new();
            // Publish the initial Freerun snapshot so a reader sees a value
            // immediately rather than `None`.
            status.publish(tracker.status());
            Self {
                source,
                tracker,
                status,
            }
        }

        /// A clone of the published-status handle, for the badge consumer to read.
        #[must_use]
        pub fn status_handle(&self) -> LatestState<ReferenceStatus> {
            self.status.clone()
        }

        /// The current tracker state (for diagnostics).
        #[must_use]
        pub const fn state(&self) -> LockState {
            self.tracker.state()
        }

        /// Take one reading (at monotonic time `now_ns`), feed it to the tracker,
        /// and publish the updated status. On a read error, advance staleness only
        /// (the reference coasts then drops per the tracker's holdover logic).
        ///
        /// Returns the published [`ReferenceStatus`].
        pub fn sample_once(&mut self, now_ns: i64) -> ReferenceStatus {
            match self.source.read() {
                Ok(reading) => {
                    self.tracker.observe(reading.to_sample(), now_ns);
                }
                Err(_) => {
                    // No fresh sample this round: let staleness/holdover fire.
                    self.tracker.tick(now_ns);
                }
            }
            let status = self.tracker.status();
            self.status.publish(status);
            status
        }

        /// Advance time without taking a reading (lets staleness/holdover fire),
        /// publishing and returning the updated status.
        pub fn tick(&mut self, now_ns: i64) -> ReferenceStatus {
            self.tracker.tick(now_ns);
            let status = self.tracker.status();
            self.status.publish(status);
            status
        }
    }

    /// The real Linux PHC source: opens `/dev/ptpN` and reads it via the dynamic
    /// POSIX clock id derived from the file descriptor.
    ///
    /// Reading a `/dev/ptpN` device as a clock works by turning its file
    /// descriptor into a dynamic `clockid_t` (`FD_TO_CLOCKID`) and calling
    /// `clock_gettime` on it. We use [`rustix::time::clock_gettime_dynamic`] for
    /// that mapping + syscall, so **no `unsafe` is needed in this crate** (the
    /// engine is `forbid(unsafe_code)`); `rustix` owns the FFI. The system bracket
    /// reads use the safe [`rustix::time::clock_gettime`] on `CLOCK_REALTIME`.
    ///
    /// Compile-verified only here (no PTP NIC); the live read is gated in tests.
    #[cfg(target_os = "linux")]
    #[derive(Debug)]
    pub struct RealPhcSource {
        dev: std::fs::File,
        path: String,
    }

    #[cfg(target_os = "linux")]
    impl RealPhcSource {
        /// Open a PHC device (e.g. `/dev/ptp0`) for reading.
        ///
        /// # Errors
        ///
        /// Returns [`PhcError::Open`] if the device cannot be opened (absent NIC,
        /// permissions).
        pub fn open(path: impl Into<String>) -> Result<Self, PhcError> {
            let path = path.into();
            let dev = std::fs::File::open(&path).map_err(|source| PhcError::Open {
                path: path.clone(),
                source,
            })?;
            Ok(Self { dev, path })
        }

        /// The device path this source reads.
        #[must_use]
        pub fn path(&self) -> &str {
            &self.path
        }

        /// Read one clock instant (ns) from a dynamic clock by file descriptor.
        fn read_dynamic(&self) -> Result<i64, PhcError> {
            use rustix::time::{clock_gettime_dynamic, DynamicClockId};
            use std::os::fd::AsFd as _;
            let ts = clock_gettime_dynamic(DynamicClockId::Dynamic(self.dev.as_fd()))
                .map_err(|e| PhcError::Read(format!("PHC clock_gettime: {e}")))?;
            Ok(timespec_to_ns(ts.tv_sec, ts.tv_nsec))
        }

        /// Read the system `CLOCK_REALTIME` instant (ns).
        fn read_system() -> i64 {
            use rustix::time::{clock_gettime, ClockId};
            let ts = clock_gettime(ClockId::Realtime);
            timespec_to_ns(ts.tv_sec, ts.tv_nsec)
        }
    }

    /// Combine a `timespec`'s seconds + nanoseconds into a single i64 nanosecond
    /// count, saturating rather than overflowing on a far-future epoch.
    #[cfg(target_os = "linux")]
    const fn timespec_to_ns(secs: i64, nanos: i64) -> i64 {
        secs.saturating_mul(1_000_000_000).saturating_add(nanos)
    }

    #[cfg(target_os = "linux")]
    impl PhcSource for RealPhcSource {
        fn read(&mut self) -> Result<PhcReading, PhcError> {
            // sys -> phc -> sys sandwich: the two system reads bracket the PHC read
            // so the system instant at the PHC read is the midpoint and the bracket
            // span bounds the uncertainty (used as path delay).
            let sys_before = Self::read_system();
            let phc = self.read_dynamic()?;
            let sys_after = Self::read_system();
            Ok(PhcReading {
                sys_before_ns: sys_before,
                phc_ns: phc,
                sys_after_ns: sys_after,
            })
        }
    }

    /// On non-Linux targets the `ptp` feature still compiles, but no real PHC can
    /// be read: a constructed source always reports [`PhcError::Unsupported`].
    #[cfg(not(target_os = "linux"))]
    #[derive(Debug, Default)]
    #[non_exhaustive]
    pub struct RealPhcSource;

    #[cfg(not(target_os = "linux"))]
    impl RealPhcSource {
        /// Construct the (non-functional) source on a non-Linux target. Accepts the
        /// path for API parity with the Linux build but never opens it.
        pub fn open(_path: impl Into<String>) -> Result<Self, PhcError> {
            Err(PhcError::Unsupported)
        }
    }

    #[cfg(not(target_os = "linux"))]
    impl PhcSource for RealPhcSource {
        fn read(&mut self) -> Result<PhcReading, PhcError> {
            Err(PhcError::Unsupported)
        }
    }
}
