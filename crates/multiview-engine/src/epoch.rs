//! The **outbound presentation epoch** (ADR-M010, DEV-C1): one
//! [`WallClockRef`] per program, anchoring the output tick counter to
//! disciplined wall-clock nanoseconds so downstream presenters (display
//! nodes, RTCP SR stamping, HLS `EXT-X-PROGRAM-DATE-TIME`) share a single
//! tick↔wall map.
//!
//! ## What the epoch is
//!
//! The output clock pins `out_pts = f(tick)` on the run's monotonic
//! [`TimeSource`] timeline: tick `i` is due at
//! `seed + pts_at(i)` (see [`EngineRuntime::seed_nanos`]). The epoch is the
//! **observation** of where that timeline sits on the disciplined wall clock:
//!
//! ```text
//! wall@tick0 = wall_now − (mono_now − seed)
//! epoch      = WallClockRef { wall_at_anchor: wall@tick0, media_at_anchor: 0, rate: 1 GHz }
//! ```
//!
//! with media units = **output-PTS nanoseconds** ([`EPOCH_RATE`]), so any
//! consumer maps a packet PTS to a wall instant with
//! [`WallClockRef::wall_at`] exactly (and back with `media_at`). All
//! arithmetic is exact integer (invariant #3) — never float.
//!
//! ## Invariant #1: an observation, never a pacer
//!
//! Nothing in this module is called by — or calls into — the output-clock
//! loop. The [`EpochSampler`] runs on a low-rate (~1 Hz) control-plane task,
//! reads the immutable `seed` value, the shared monotonic [`TimeSource`]
//! (a wait-free atomic/`Instant` read), the wall clock, and the wait-free
//! [`LatestState`] PTP snapshot; it writes only its own published value. A
//! drifting, stepping, flapping, or absent reference changes **only the
//! published map and its quality label** — it can neither stall, speed up,
//! nor skip a tick (`epoch_observation_does_not_change_the_tick_stream`
//! pins this behaviourally).
//!
//! ## Re-anchor policy (the [`EpochTracker`])
//!
//! Consumers present frames against the epoch, so the published map must be
//! *stable*: it **anchors** on the first observation, **holds** while the
//! re-derived candidate stays inside a small deadband (wall-read jitter),
//! **slews** by a bounded step per observation toward a persistent error
//! (smooth — no visible step while consumers present), and **steps** only on
//! a gross discontinuity (host clock stepped / reference changed) — the
//! Class-2-like re-anchor ADR-M010 documents (a consumer sees it as a new
//! map, exactly like a `D`-change discontinuity in wall-clock-sync §3).
//!
//! ## Source selection (ADR-T012)
//!
//! The wall estimate follows the pinned reference ladder: **PTP while
//! disciplined** (Locked or Holdover — the servo's `local − master` offset is
//! subtracted from the system reading, then the timescale conversion below),
//! else the **system clock** (read directly: chrony/NTP already disciplines
//! `CLOCK_REALTIME`; the kernel discipline state is classified honestly via
//! [`sysref`](crate::sysref)). The published [`ClockSource`]/[`ClockQuality`]
//! labels say which leg is live — never an over-claim.
//!
//! ## Timescale: the published epoch is **UTC**, the PHC carries **TAI**
//!
//! The epoch's wall leg is defined as **UTC nanoseconds past the Unix
//! epoch** — it is stamped into HLS `EXT-X-PROGRAM-DATE-TIME` (RFC 8216:
//! UTC, `Z`) and the RTCP SR NTP word (the UTC-aligned NTP era). Under the
//! standard linuxptp deployment ADR-T012 names (ptp4l on the ST 2059-2
//! profile + phc2sys) the PHC carries **PTP time = TAI**, so the servo's
//! steady-state `local − master` offset is ≈ **−37 s** (UTC − TAI) plus the
//! servo residual. Subtracting the raw offset alone would publish TAI into
//! UTC-defined surfaces; the PTP leg therefore additionally subtracts the
//! configured TAI−UTC offset ([`EpochSamplerConfig::ptp_utc_offset_ns`],
//! default 37 s — `[timing] ptp_utc_offset_s`, sourced from ptp4l's
//! `currentUtcOffset` in deployment) so the published epoch is **always
//! UTC**, and a ptp↔system reference transition does not step the epoch by
//! the TAI−UTC difference. A post-conversion residual of
//! [`UTC_RESIDUAL_GUARD_NS`] or more (a misconfigured offset, or a PHC on an
//! unexpected timescale) trips a sanity guard: the bogus PTP estimate is
//! **never published** — the sampler degrades to the system leg with a
//! non-locked quality and warns. All conversion math is exact integer
//! (invariant #3).

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use multiview_core::time::Rational;
use multiview_core::wallclock::WallClockRef;
use multiview_events::{ClockQuality, ClockSource};
use multiview_overlay::clock::RefSource;

use crate::clock::TimeSource;
use crate::isolation::LatestState;
use crate::ptp::{LockState, ReferenceStatus};
use crate::sysref::{NtpQuery, ReferenceSelector, SystemRefConfig, SystemRefTracker};

#[cfg(doc)]
use crate::runtime::EngineRuntime;

/// The epoch media timebase: **output-PTS nanoseconds** (1 GHz ticks). With
/// this rate `wall_at`/`media_at` round-trip exactly, and any container
/// timebase rescales into it exactly.
pub const EPOCH_RATE: Rational = Rational::new(1_000_000_000, 1);

/// The default PTP-leg timescale conversion: the current TAI−UTC offset
/// (37 s since 2017-01-01) in integer nanoseconds. In deployment this is
/// `[timing] ptp_utc_offset_s`, confirmed against ptp4l's `currentUtcOffset`.
pub const DEFAULT_PTP_UTC_OFFSET_NS: i64 = 37_000_000_000;

/// The post-conversion residual sanity bound (30 s, integer ns): if the
/// UTC-converted PTP estimate still disagrees with the system clock by this
/// much, the timescale configuration is wrong (e.g. the 37 s default applied
/// to a PHC that carries UTC, or `0` applied to a TAI PHC) — the estimate is
/// never published; the sampler degrades to the system leg and warns.
pub const UTC_RESIDUAL_GUARD_NS: i64 = 30_000_000_000;

/// Tuning for the [`EpochTracker`] re-anchor policy. All thresholds are
/// integer nanoseconds (invariant #3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EpochPolicy {
    /// Candidate errors at or below this magnitude are wall-read jitter: the
    /// published epoch **holds** (consumers keep a stable map).
    pub deadband_ns: i64,
    /// The maximum the anchor may move per observation while **slewing**
    /// toward a persistent error (smooth re-anchor, no visible step).
    pub slew_max_ns: i64,
    /// Errors beyond this magnitude are a gross discontinuity (host clock
    /// stepped / reference changed): the epoch **steps** to the candidate in
    /// one observation — the documented Class-2-like re-anchor.
    pub step_threshold_ns: i64,
}

impl EpochPolicy {
    /// The defaults: 2 ms deadband (chrony-class wall jitter), 5 ms/update
    /// slew bound (≤ 5 ms/s at the 1 Hz sampling cadence), 250 ms step
    /// threshold.
    #[must_use]
    pub const fn new_default() -> Self {
        Self {
            deadband_ns: 2_000_000,
            slew_max_ns: 5_000_000,
            step_threshold_ns: 250_000_000,
        }
    }
}

impl Default for EpochPolicy {
    fn default() -> Self {
        Self::new_default()
    }
}

/// What one [`EpochTracker::observe`] did to the published epoch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum EpochUpdate {
    /// The first observation anchored the epoch exactly.
    Anchored,
    /// The candidate was inside the deadband: the published epoch held.
    Held,
    /// The anchor slewed by the (bounded) `adjustment_ns` toward the
    /// candidate.
    Slewed {
        /// The signed bounded adjustment applied to the anchor's wall leg.
        adjustment_ns: i64,
    },
    /// A gross discontinuity (or a media-rate change): the epoch stepped to
    /// the candidate in one observation.
    Stepped {
        /// The signed error the step absorbed (`0` for a pure rate change at
        /// an equal wall mapping).
        error_ns: i64,
    },
}

/// The hold / slew / step re-anchor state machine over candidate epochs.
///
/// Pure and deterministic: feed re-derived candidates with
/// [`EpochTracker::observe`]; read the published map with
/// [`EpochTracker::current`]. Same-rate candidates differ from the current
/// epoch by a constant wall error (two affine maps with one rate), which is
/// measured at the current anchor's media position — the media anchor and
/// rate never move on hold/slew, only the wall leg does.
#[derive(Debug, Clone)]
pub struct EpochTracker {
    policy: EpochPolicy,
    current: Option<WallClockRef>,
}

impl EpochTracker {
    /// A tracker with the given policy and no epoch yet.
    #[must_use]
    pub const fn new(policy: EpochPolicy) -> Self {
        Self {
            policy,
            current: None,
        }
    }

    /// The currently-published epoch (`None` before the first observation).
    #[must_use]
    pub const fn current(&self) -> Option<WallClockRef> {
        self.current
    }

    /// Fold one re-derived candidate into the published epoch per the
    /// hold/slew/step policy (see the type docs).
    pub fn observe(&mut self, candidate: WallClockRef) -> EpochUpdate {
        let Some(cur) = self.current else {
            self.current = Some(candidate);
            return EpochUpdate::Anchored;
        };
        if cur.rate != candidate.rate {
            // A different media rate is a different timeline: full re-anchor.
            let error_ns = candidate
                .wall_at(cur.media_at_anchor)
                .saturating_sub(cur.wall_at_anchor_ns);
            self.current = Some(candidate);
            return EpochUpdate::Stepped { error_ns };
        }
        // Same-rate affine maps differ by a constant: evaluate the candidate
        // at OUR anchor's media position and compare wall legs.
        let error_ns = candidate
            .wall_at(cur.media_at_anchor)
            .saturating_sub(cur.wall_at_anchor_ns);
        if error_ns.saturating_abs() <= self.policy.deadband_ns {
            return EpochUpdate::Held;
        }
        if error_ns.saturating_abs() > self.policy.step_threshold_ns {
            // Gross discontinuity: land exactly on the candidate, preserving
            // OUR media anchor (the maps are equal; only the form changes).
            self.current = Some(WallClockRef::new(
                cur.wall_at_anchor_ns.saturating_add(error_ns),
                cur.media_at_anchor,
                cur.rate,
            ));
            return EpochUpdate::Stepped { error_ns };
        }
        let adjustment_ns = error_ns.clamp(-self.policy.slew_max_ns, self.policy.slew_max_ns);
        self.current = Some(WallClockRef::new(
            cur.wall_at_anchor_ns.saturating_add(adjustment_ns),
            cur.media_at_anchor,
            cur.rate,
        ));
        EpochUpdate::Slewed { adjustment_ns }
    }
}

/// One paired wall/monotonic reading: the wall instant bracketed by two
/// monotonic reads (the same sandwich shape as
/// `PhcReading`), so the wall value is bound
/// to the monotonic midpoint with the read latency as its uncertainty.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WallSample {
    /// Monotonic ns (the run's [`TimeSource`] timeline) read *before* the
    /// wall clock.
    pub mono_before_ns: i64,
    /// The wall-clock instant (`CLOCK_REALTIME`), integer ns past the Unix
    /// epoch.
    pub wall_ns: i64,
    /// Monotonic ns read *after* the wall clock.
    pub mono_after_ns: i64,
}

impl WallSample {
    /// The monotonic instant the wall reading is bound to (the bracket
    /// midpoint, computed without overflow).
    #[must_use]
    pub const fn mono_mid_ns(&self) -> i64 {
        let span = self.mono_after_ns.saturating_sub(self.mono_before_ns);
        self.mono_before_ns.saturating_add(span.div_euclid(2))
    }
}

/// The injectable source of paired wall/monotonic readings.
///
/// Production wires [`SystemWallSampler`] (the OS `CLOCK_REALTIME` bracketed
/// by the run's monotonic [`TimeSource`]); tests inject a fake so the whole
/// epoch derivation is deterministic.
pub trait WallClockSampler: Send {
    /// Take one paired reading.
    fn sample(&mut self) -> WallSample;
}

/// The production wall sampler: `SystemTime::now()` (the chrony/NTP-
/// disciplined `CLOCK_REALTIME`) bracketed by the run's monotonic source.
pub struct SystemWallSampler {
    time: Arc<dyn TimeSource>,
}

impl SystemWallSampler {
    /// A sampler over the run's monotonic `time` source (the same instance
    /// the output clock's deadlines are measured on, so the bracket is on the
    /// right timeline).
    #[must_use]
    pub fn new(time: Arc<dyn TimeSource>) -> Self {
        Self { time }
    }
}

impl std::fmt::Debug for SystemWallSampler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SystemWallSampler").finish_non_exhaustive()
    }
}

/// Read `CLOCK_REALTIME` as integer ns past the Unix epoch (negative for a
/// pre-1970 clock, saturating at the i64 extremes).
#[must_use]
fn unix_now_ns() -> i64 {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(since) => i64::try_from(since.as_nanos()).unwrap_or(i64::MAX),
        Err(before) => i64::try_from(before.duration().as_nanos())
            .unwrap_or(i64::MAX)
            .saturating_neg(),
    }
}

impl WallClockSampler for SystemWallSampler {
    fn sample(&mut self) -> WallSample {
        let mono_before_ns = self.time.now_nanos();
        let wall_ns = unix_now_ns();
        let mono_after_ns = self.time.now_nanos();
        WallSample {
            mono_before_ns,
            wall_ns,
            mono_after_ns,
        }
    }
}

/// The run anchor an [`EpochSampler`] derives the epoch against: the run's
/// monotonic [`TimeSource`] plus the seed instant tick 0 is anchored to
/// (`EngineRuntime::seed_nanos`). Published by the run path into a shared
/// slot once the clock seeds; the sampler task picks it up lazily.
#[derive(Clone)]
pub struct EpochAnchor {
    time: Arc<dyn TimeSource>,
    seed_nanos: i64,
}

impl EpochAnchor {
    /// Bind the run's monotonic source and tick-0 seed instant.
    #[must_use]
    pub fn new(time: Arc<dyn TimeSource>, seed_nanos: i64) -> Self {
        Self { time, seed_nanos }
    }

    /// The run's monotonic time source (shared, read-only).
    #[must_use]
    pub fn time(&self) -> Arc<dyn TimeSource> {
        Arc::clone(&self.time)
    }

    /// The monotonic instant tick 0 is anchored to.
    #[must_use]
    pub const fn seed_nanos(&self) -> i64 {
        self.seed_nanos
    }
}

impl std::fmt::Debug for EpochAnchor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EpochAnchor")
            .field("seed_nanos", &self.seed_nanos)
            .finish_non_exhaustive()
    }
}

/// Tuning for the [`EpochSampler`]: the re-anchor policy, the system
/// discipline classifier configuration, and the PTP timescale conversion.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EpochSamplerConfig {
    /// The hold/slew/step re-anchor policy.
    pub policy: EpochPolicy,
    /// The system NTP/chrony discipline classifier tuning (incl. the
    /// assumed-when-unavailable state). The epoch default is the
    /// **conservative** [`SystemRefConfig::new_conservative`]: with no kernel
    /// reading available the machine-readable feed reports `freerun`, never
    /// an assumed lock (the on-screen badge keeps its own deployment-
    /// assumption default — this field affects only the epoch path).
    pub sys: SystemRefConfig,
    /// The PTP-leg timescale conversion (TAI−UTC, integer ns), subtracted
    /// from the PHC-derived estimate so the published epoch is always UTC —
    /// see the module-level *Timescale* section. Default
    /// [`DEFAULT_PTP_UTC_OFFSET_NS`] (37 s); `0` for a PHC that carries UTC.
    pub ptp_utc_offset_ns: i64,
}

impl EpochSamplerConfig {
    /// The epoch-path defaults: the standard re-anchor policy, the
    /// conservative (never-assume-locked) system classifier, and the current
    /// TAI−UTC timescale conversion.
    #[must_use]
    pub const fn new_default() -> Self {
        Self {
            policy: EpochPolicy::new_default(),
            sys: SystemRefConfig::new_conservative(),
            ptp_utc_offset_ns: DEFAULT_PTP_UTC_OFFSET_NS,
        }
    }
}

impl Default for EpochSamplerConfig {
    fn default() -> Self {
        Self::new_default()
    }
}

/// One published epoch snapshot: the map plus the honest source/quality
/// labels (the [`multiview_events::TimingStatus`] payload's clock fields).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EpochStatus {
    /// The published tick↔wall map (media units = output-PTS ns,
    /// [`EPOCH_RATE`]).
    pub epoch: WallClockRef,
    /// Which reference disciplines the wall estimate (ADR-T012 precedence).
    pub source: ClockSource,
    /// The discipline quality of that reference (honest — never over-claimed).
    pub quality: ClockQuality,
    /// What this observation did to the published map.
    pub update: EpochUpdate,
}

/// Map the selected reference source onto the wire [`ClockSource`]: PTP is
/// PTP; the NTP-disciplined and undisciplined system clock both ride the
/// `system` label (the quality field carries the discipline state).
#[must_use]
pub fn clock_source_of(source: RefSource) -> ClockSource {
    match source {
        RefSource::Ptp => ClockSource::Ptp,
        _ => ClockSource::System,
    }
}

/// Map the servo lock-state lifecycle onto the wire [`ClockQuality`] (1:1).
#[must_use]
pub fn clock_quality_of(state: LockState) -> ClockQuality {
    match state {
        LockState::Locked => ClockQuality::Locked,
        LockState::Holdover => ClockQuality::Holdover,
        LockState::Acquiring => ClockQuality::Acquiring,
        LockState::Freerun => ClockQuality::Freerun,
    }
}

/// Derives the per-program outbound epoch: one wall/monotonic sample at a
/// time, folded through the ADR-T012 reference selection and the
/// [`EpochTracker`] re-anchor policy.
///
/// Drive [`EpochSampler::sample_once`] from a low-rate off-hot-path task
/// (~1 Hz). It reads the PTP reference (when wired) through a wait-free
/// [`LatestState`] snapshot and never touches the output clock — see the
/// module docs for the invariant-#1 argument.
pub struct EpochSampler<W: WallClockSampler, Q: NtpQuery> {
    seed_nanos: i64,
    wall: W,
    sys_query: Q,
    sys: SystemRefTracker,
    ptp: Option<LatestState<ReferenceStatus>>,
    selector: ReferenceSelector,
    tracker: EpochTracker,
    /// The PTP-leg TAI−UTC timescale conversion (integer ns) — see the
    /// module-level *Timescale* section.
    ptp_utc_offset_ns: i64,
    /// Whether the UTC-residual sanity guard is currently tripped (latched so
    /// the warning fires once per excursion, not once per ~1 Hz sample).
    utc_guard_tripped: bool,
}

impl<W: WallClockSampler, Q: NtpQuery> EpochSampler<W, Q> {
    /// Build a sampler for a run whose tick 0 is anchored at `seed_nanos` on
    /// the same monotonic timeline `wall`'s bracket reads use.
    #[must_use]
    pub fn new(seed_nanos: i64, wall: W, sys_query: Q, config: EpochSamplerConfig) -> Self {
        Self {
            seed_nanos,
            wall,
            sys_query,
            sys: SystemRefTracker::new(config.sys),
            ptp: None,
            selector: ReferenceSelector,
            tracker: EpochTracker::new(config.policy),
            ptp_utc_offset_ns: config.ptp_utc_offset_ns,
            utc_guard_tripped: false,
        }
    }

    /// Attach the PTP reference snapshot handle (filled by the `ptp`-feature
    /// `PhcSampler`). While that reference is
    /// disciplined it outranks the system clock (ADR-T012 §1).
    #[must_use]
    pub fn with_ptp(mut self, handle: LatestState<ReferenceStatus>) -> Self {
        self.ptp = Some(handle);
        self
    }

    /// The currently-published epoch (`None` before the first sample).
    #[must_use]
    pub const fn current(&self) -> Option<WallClockRef> {
        self.tracker.current()
    }

    /// Take one wall/monotonic sample, select the disciplined reference, fold
    /// the re-derived candidate through the re-anchor policy, and return the
    /// published snapshot.
    ///
    /// Never fails and never blocks beyond the two clock reads: an absent PTP
    /// handle, an unpublished PTP snapshot, or an unavailable kernel NTP read
    /// all degrade to the honest next rung of the ADR-T012 ladder.
    pub fn sample_once(&mut self) -> EpochStatus {
        let sample = self.wall.sample();
        // The system NTP/chrony discipline (live read or the honest assumed
        // state) and the latest PTP snapshot (Freerun default when absent).
        let sys_state = self.sys.sample(&mut self.sys_query);
        let sys_offset_ns = self.sys.offset_ns();
        let ptp_status = self
            .ptp
            .as_ref()
            .and_then(LatestState::latest)
            .map_or_else(default_ptp_status, |s| *s);
        let selected = self.selector.select(sys_state, sys_offset_ns, &ptp_status);

        // The disciplined wall estimate at the sample instant: the PTP leg
        // subtracts the servo's `local − master` offset (master = local −
        // offset, the PHC timescale = TAI) and then the configured TAI−UTC
        // offset so the published epoch is UTC (module *Timescale* section);
        // the system leg reads CLOCK_REALTIME directly — chrony/NTP already
        // disciplines it, and the kernel residual is what the discipline loop
        // is converging, not a correction to re-apply.
        let (wall_disciplined_ns, source, state) = match selected.source {
            RefSource::Ptp => {
                let utc_estimate_ns = sample
                    .wall_ns
                    .saturating_sub(selected.offset_ns)
                    .saturating_sub(self.ptp_utc_offset_ns);
                // Post-conversion sanity guard: a healthy conversion leaves
                // the PTP estimate within servo-residual distance of the
                // (UTC) system clock; ≥30 s means the timescale config is
                // wrong — never publish that estimate.
                let residual_ns = utc_estimate_ns.saturating_sub(sample.wall_ns);
                if residual_ns.saturating_abs() >= UTC_RESIDUAL_GUARD_NS {
                    if !self.utc_guard_tripped {
                        self.utc_guard_tripped = true;
                        tracing::warn!(
                            residual_ns,
                            ptp_utc_offset_ns = self.ptp_utc_offset_ns,
                            "epoch: the UTC-converted PTP estimate disagrees with the system \
                             clock by >=30 s — [timing] ptp_utc_offset_s is misconfigured for \
                             this PHC's timescale (check ptp4l currentUtcOffset); publishing \
                             the system-clock epoch with a degraded quality instead"
                        );
                    }
                    // Degrade to the system leg; never claim Locked while the
                    // deployment's timescale configuration is demonstrably
                    // wrong (cap at Holdover; weaker states stay as they are).
                    let capped = match sys_state {
                        LockState::Locked => LockState::Holdover,
                        other => other,
                    };
                    (sample.wall_ns, RefSource::System, capped)
                } else {
                    if self.utc_guard_tripped {
                        self.utc_guard_tripped = false;
                        tracing::info!(
                            "epoch: the PTP timescale residual is back in bounds; \
                             the PTP leg resumes disciplining the published epoch"
                        );
                    }
                    (utc_estimate_ns, RefSource::Ptp, selected.state)
                }
            }
            other => (sample.wall_ns, other, selected.state),
        };
        // wall@tick0 = wall_now − (mono_mid − seed); media anchor 0 = pts 0.
        let elapsed_since_seed = sample.mono_mid_ns().saturating_sub(self.seed_nanos);
        let candidate = WallClockRef::new(
            wall_disciplined_ns.saturating_sub(elapsed_since_seed),
            0,
            EPOCH_RATE,
        );
        let update = self.tracker.observe(candidate);
        // `observe` always anchors on the first sample, so `current` is Some.
        let epoch = self.tracker.current().unwrap_or(candidate);
        EpochStatus {
            epoch,
            source: clock_source_of(source),
            quality: clock_quality_of(state),
            update,
        }
    }
}

impl<W: WallClockSampler, Q: NtpQuery> std::fmt::Debug for EpochSampler<W, Q> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EpochSampler")
            .field("seed_nanos", &self.seed_nanos)
            .field("current", &self.tracker.current())
            .finish_non_exhaustive()
    }
}

/// The PTP snapshot used when no handle is attached or nothing was published
/// yet: an honest never-referenced Freerun (selection falls to the system
/// clock).
fn default_ptp_status() -> ReferenceStatus {
    ReferenceStatus {
        state: LockState::Freerun,
        offset_ns: 0,
        frequency_ppb: 0,
        accepted: 0,
        disciplined: false,
    }
}
