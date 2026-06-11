//! Consent-independent local **metrics retention store** (CONSPECT engine-seam
//! S5; [ADR-0052](../../../docs/decisions/ADR-0052.md) §3,
//! conspect-account-architecture §7.2).
//!
//! ## What this is
//!
//! A rolling, **bounded**, minute-bucketed on-box record of the diagnostic
//! categories the §7.2 support context-pack needs: whole-system **utilisation**
//! (cpu / gpu busy fractions, summarised to percentiles over a window),
//! **shed-load** events from the degradation/placement controller, **per-input
//! reconnect** history, and **incident markers** (input flap, encoder
//! saturation, clock holdover). It keeps **at least seven days** at minute
//! resolution and answers windowed queries over the 1h / 24h / 7d windows the
//! bundle reports.
//!
//! ## Consent independence (the §7.2 distinction — read this)
//!
//! This store is retained **regardless of telemetry consent**. The two
//! machine→Aperim contacts are separate: the **opt-in daily telemetry pipe** is
//! governed by consent and carries anonymised analytics *outbound*; this **local
//! buffer** serves the *operator's own* support diagnostics and is never gated by
//! consent. There is deliberately **no consent input on any recording path here**
//! — turning telemetry off stops the outbound pipe, it does **not** stop or empty
//! this buffer. The buffer never leaves the machine without an explicit,
//! operator-approved data request (ADR-0053); that egress gate lives elsewhere,
//! not in this store. (See ADR-0052 §3 and the brief §7.2.)
//!
//! ## Bounded memory + writers never block (invariants #1 / #10)
//!
//! Storage is a **fixed** ring of [`RETENTION_BUCKETS`] minute-buckets allocated
//! once at construction; writing into a slot that holds an older minute simply
//! **overwrites** it (drop-oldest — data-plane rule 5), so memory is bounded
//! forever and never grows. Each bucket's event lists are capped at
//! [`MAX_EVENTS_PER_BUCKET`] (drop-oldest within a single busy minute), so even a
//! reconnect/incident *storm* cannot grow the store. Utilisation is kept as a
//! tiny per-minute **aggregate** (count + sum + min + max), not a per-sample
//! array, so the whole seven-day window is a few hundred kilobytes.
//!
//! Writes take a single short, allocation-light critical section over an internal
//! [`std::sync::Mutex`] (index + a `Vec::push`/overwrite — never held across any
//! `.await`, I/O, or callback), so a writer always makes **bounded** progress and
//! can never be back-pressured by a reader or by another writer; a poisoned guard
//! is recovered rather than propagated (this is best-effort observability that
//! must never crash or stall a caller — invariant #10). The store is fed off the
//! hot loop (a CONSPECT sampler/event task in `multiview-cli`); nothing here
//! touches the output clock.
//!
//! ## Time model
//!
//! The store is **time-source-agnostic**: every `record_*_at` /
//! `*_window`/`*_summary` method takes the relevant Unix-epoch **second** as a
//! parameter (the caller samples the wall clock once and threads it in, mirroring
//! the pure-assembler pattern in `multiview-cli::system_metrics`). That keeps the
//! store free of any clock dependency and makes rollover / windowing exhaustively
//! unit-testable without sleeping.

use std::sync::Mutex;

/// The bucket resolution in seconds: **minute** buckets.
pub const BUCKET_SECONDS: u64 = 60;

/// The number of minute-buckets retained: **7 days** exactly
/// (`7 * 24 * 60 = 10_080`). Fixed at construction, so memory is bounded forever
/// (data-plane rule 5); writing a newer minute drop-oldest-overwrites the slot
/// that held the corresponding older minute.
pub const RETENTION_BUCKETS: usize = 7 * 24 * 60;

/// The per-bucket cap on stored discrete events (reconnect / shed / incident).
///
/// Within a single minute a flapping input or a degradation storm can emit many
/// events; we keep the most recent [`MAX_EVENTS_PER_BUCKET`] per category per
/// minute (drop-oldest within the minute) so a storm can never grow the store.
/// 64/minute/category is generous for genuine support diagnostics while keeping
/// the worst-case footprint bounded.
pub const MAX_EVENTS_PER_BUCKET: usize = 64;

/// One of the §7.2 query windows the support bundle reports over.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum RetentionWindow {
    /// The last hour (3600 s).
    LastHour,
    /// The last 24 hours (86 400 s).
    LastDay,
    /// The last 7 days (604 800 s) — the full retained span.
    LastWeek,
}

impl RetentionWindow {
    /// The window length in seconds.
    #[must_use]
    pub const fn seconds(self) -> u64 {
        match self {
            RetentionWindow::LastHour => 60 * 60,
            RetentionWindow::LastDay => 24 * 60 * 60,
            RetentionWindow::LastWeek => 7 * 24 * 60 * 60,
        }
    }
}

/// Why the resource-adaptive controller shed load rather than holding/migrating.
///
/// Mirrors `multiview_telemetry::placement::SuppressReason`, the engine's
/// `multiview_engine::placement::ShedReason`, and the wire
/// `multiview_events::ShedReason` as a self-contained value (retention stays a
/// leaf — it depends on no engine/events type), so the CONSPECT feed maps the
/// wire reason onto this. `#[non_exhaustive]` so a future reason is additive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ShedReason {
    /// The overloaded pipeline is pinned to its device and cannot migrate.
    Pinned,
    /// The pipeline feeds a local display sink whose framebuffer must live on the
    /// connector-owning GPU (ADR-0044 §3), so composite may not migrate off it —
    /// the only relief is a local shed.
    DisplayBound,
    /// No materially-better home exists; moving would not cure the imbalance.
    NoBetterHome,
    /// A better home exists but cooldown / per-GPU budget forbids moving now.
    AntiStorm,
    /// The encode/egress stage could not keep up at the output cadence, so a
    /// composited frame was shed (drop-on-overload) rather than blocking the
    /// output clock (invariants #1 + #10) — the real live shed today.
    EncoderOverload,
}

impl ShedReason {
    /// The stable, lower-case label for this reason.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            ShedReason::Pinned => "pinned",
            ShedReason::DisplayBound => "display_bound",
            ShedReason::NoBetterHome => "no_better_home",
            ShedReason::AntiStorm => "anti_storm",
            ShedReason::EncoderOverload => "encoder_overload",
        }
    }
}

/// The class of an incident marker — a notable, timestamped diagnostic event.
///
/// `#[non_exhaustive]` so more marker classes can be added without a breaking
/// change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum IncidentKind {
    /// An input source flapped (rapidly lost/regained signal or reconnected).
    InputFlap,
    /// An encoder reached its saturation ceiling (e.g. NVENC session limit).
    EncoderSaturation,
    /// The output clock held over on its last-good cadence through a reference
    /// disturbance (PTP/genlock holdover).
    ClockHoldover,
}

impl IncidentKind {
    /// The stable, lower-case label for this incident kind.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            IncidentKind::InputFlap => "input_flap",
            IncidentKind::EncoderSaturation => "encoder_saturation",
            IncidentKind::ClockHoldover => "clock_holdover",
        }
    }
}

/// One utilisation sample as recorded by the caller (a single off-hot-loop
/// system-metrics sample). Fractions are `0.0..=1.0` busy ratios; an unknown GPU
/// stays `None` (never a false zero).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct UtilisationSample {
    /// Whole-system CPU busy fraction (`0.0..=1.0`).
    pub cpu_util: f64,
    /// Aggregate GPU busy fraction (`0.0..=1.0`), or `None` when unknown.
    pub gpu_util: Option<f64>,
    /// Aggregate program output rate (fps) at the sample, or `None`.
    pub program_fps: Option<f64>,
}

/// A per-minute utilisation aggregate returned from a window query (one entry per
/// in-window minute that saw at least one sample). The percentiles for the whole
/// window are computed by [`RetentionStore::utilisation_summary`].
#[derive(Debug, Clone, Copy, PartialEq)]
#[non_exhaustive]
pub struct UtilisationBucket {
    /// The minute-index (Unix seconds / 60) this aggregate covers.
    pub minute_index: u64,
    /// Number of raw samples folded into this minute.
    pub samples: u64,
    /// Mean CPU busy fraction over the minute.
    pub cpu_mean: f64,
    /// Minimum CPU busy fraction seen in the minute.
    pub cpu_min: f64,
    /// Maximum CPU busy fraction seen in the minute.
    pub cpu_max: f64,
    /// Mean GPU busy fraction over the minute when any sample carried a GPU value.
    pub gpu_mean: Option<f64>,
}

/// A summarised utilisation report over a window: total sample count and
/// percentile estimates derived from the per-minute aggregates in the window.
#[derive(Debug, Clone, Copy, PartialEq)]
#[non_exhaustive]
pub struct UtilisationSummary {
    /// Total raw samples folded across the window.
    pub samples: u64,
    /// The lowest CPU busy fraction observed in the window (the p0 floor).
    pub cpu_p0: f64,
    /// The median CPU busy fraction across the window's per-minute means.
    pub cpu_p50: f64,
    /// The 95th-percentile CPU busy fraction across the window's per-minute means.
    pub cpu_p95: f64,
    /// The highest CPU busy fraction observed in the window (the p100 ceiling).
    pub cpu_p100: f64,
}

/// A recorded per-input reconnect event.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ReconnectEvent {
    /// The Unix second the reconnect was recorded.
    pub at_unix_seconds: u64,
    /// The configured input/source id that reconnected.
    pub input_id: String,
    /// The reconnect attempt counter at this event.
    pub attempt: u32,
}

/// A recorded shed-load event from the resource-adaptive controller.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ShedEvent {
    /// The Unix second the shed was recorded.
    pub at_unix_seconds: u64,
    /// Why load was shed rather than held/migrated.
    pub reason: ShedReason,
}

/// A recorded incident marker.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct IncidentMarker {
    /// The Unix second the incident was recorded.
    pub at_unix_seconds: u64,
    /// The incident class.
    pub kind: IncidentKind,
    /// What the incident applied to (input id, "program", "system", …).
    pub subject: String,
}

/// The mutable per-minute aggregate stored in a ring slot.
#[derive(Debug, Clone, Default)]
struct Bucket {
    /// The minute-index this slot currently holds, or `None` when empty/never
    /// written. Used to detect a stale (wrapped-past) slot on read and to drop the
    /// old contents when a newer minute claims the slot.
    minute_index: Option<u64>,
    /// Folded utilisation count for the minute.
    util_count: u64,
    /// Sum of CPU fractions for the minute (for the mean).
    cpu_sum: f64,
    /// Min CPU fraction for the minute.
    cpu_min: f64,
    /// Max CPU fraction for the minute.
    cpu_max: f64,
    /// Sum of GPU fractions for the minute, with the count that carried one.
    gpu_sum: f64,
    /// Number of samples in the minute that carried a GPU value.
    gpu_count: u64,
    /// Reconnect events in the minute (bounded, drop-oldest at the cap).
    reconnects: Vec<ReconnectEvent>,
    /// Shed events in the minute (bounded).
    sheds: Vec<ShedEvent>,
    /// Incident markers in the minute (bounded).
    incidents: Vec<IncidentMarker>,
}

impl Bucket {
    /// Reset this slot to hold a fresh `minute` (drops any older contents — the
    /// drop-oldest overwrite).
    fn reset_to(&mut self, minute: u64) {
        self.minute_index = Some(minute);
        self.util_count = 0;
        self.cpu_sum = 0.0;
        self.cpu_min = 0.0;
        self.cpu_max = 0.0;
        self.gpu_sum = 0.0;
        self.gpu_count = 0;
        self.reconnects.clear();
        self.sheds.clear();
        self.incidents.clear();
    }

    /// Ensure this slot is open for `minute`, resetting it first if it currently
    /// holds a different (older, wrapped) minute.
    fn open_for(&mut self, minute: u64) {
        if self.minute_index != Some(minute) {
            self.reset_to(minute);
        }
    }

    /// Whether this slot currently holds exactly `minute`.
    fn holds(&self, minute: u64) -> bool {
        self.minute_index == Some(minute)
    }
}

/// Push onto a bounded, drop-oldest per-minute event list.
fn bounded_push<T>(list: &mut Vec<T>, item: T) {
    if list.len() >= MAX_EVENTS_PER_BUCKET {
        // Drop the oldest within this minute so a storm can never grow the bucket.
        let _ = list.remove(0);
    }
    list.push(item);
}

/// The consent-independent local metrics retention store (CONSPECT S5).
///
/// Clone-free: wrap in an [`std::sync::Arc`] to share across the feeding task and
/// any reader. All recording/query methods take `&self`.
#[derive(Debug)]
pub struct RetentionStore {
    /// The fixed ring of minute-buckets, allocated once. A single short-held
    /// mutex guards it; the critical section is index + push/overwrite only.
    buckets: Mutex<Vec<Bucket>>,
}

impl Default for RetentionStore {
    fn default() -> Self {
        Self::new()
    }
}

impl RetentionStore {
    /// Construct an empty store with its fixed-capacity ring allocated up front.
    #[must_use]
    pub fn new() -> Self {
        let mut buckets = Vec::with_capacity(RETENTION_BUCKETS);
        buckets.resize_with(RETENTION_BUCKETS, Bucket::default);
        Self {
            buckets: Mutex::new(buckets),
        }
    }

    /// The ring slot index for a minute-index (drop-oldest by modular wrap).
    fn slot(minute: u64) -> usize {
        // `RETENTION_BUCKETS` is a small positive constant; the modulus is < usize.
        let cap = u64::try_from(RETENTION_BUCKETS).unwrap_or(u64::MAX);
        usize::try_from(minute % cap).unwrap_or(0)
    }

    /// Acquire the buckets guard, recovering a poisoned lock rather than panicking
    /// (observability must never crash a caller — invariant #10).
    fn guard(&self) -> std::sync::MutexGuard<'_, Vec<Bucket>> {
        self.buckets
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Record one whole-system utilisation sample at `at_unix_seconds`.
    ///
    /// Consent-independent: there is no consent parameter; the sample is folded
    /// into its minute-bucket unconditionally (ADR-0052 §3).
    pub fn record_utilisation_at(&self, at_unix_seconds: u64, sample: UtilisationSample) {
        let minute = at_unix_seconds / BUCKET_SECONDS;
        let idx = Self::slot(minute);
        let cpu = sample.cpu_util.clamp(0.0, 1.0);
        let mut guard = self.guard();
        let Some(bucket) = guard.get_mut(idx) else {
            return;
        };
        bucket.open_for(minute);
        if bucket.util_count == 0 {
            bucket.cpu_min = cpu;
            bucket.cpu_max = cpu;
        } else {
            bucket.cpu_min = bucket.cpu_min.min(cpu);
            bucket.cpu_max = bucket.cpu_max.max(cpu);
        }
        bucket.cpu_sum += cpu;
        bucket.util_count = bucket.util_count.saturating_add(1);
        if let Some(gpu) = sample.gpu_util {
            bucket.gpu_sum += gpu.clamp(0.0, 1.0);
            bucket.gpu_count = bucket.gpu_count.saturating_add(1);
        }
    }

    /// Record a per-input reconnect at `at_unix_seconds`.
    pub fn record_reconnect_at(
        &self,
        at_unix_seconds: u64,
        input_id: impl Into<String>,
        attempt: u32,
    ) {
        let minute = at_unix_seconds / BUCKET_SECONDS;
        let idx = Self::slot(minute);
        let event = ReconnectEvent {
            at_unix_seconds,
            input_id: input_id.into(),
            attempt,
        };
        let mut guard = self.guard();
        let Some(bucket) = guard.get_mut(idx) else {
            return;
        };
        bucket.open_for(minute);
        bounded_push(&mut bucket.reconnects, event);
    }

    /// Record a shed-load event at `at_unix_seconds`.
    pub fn record_shed_at(&self, at_unix_seconds: u64, reason: ShedReason) {
        let minute = at_unix_seconds / BUCKET_SECONDS;
        let idx = Self::slot(minute);
        let event = ShedEvent {
            at_unix_seconds,
            reason,
        };
        let mut guard = self.guard();
        let Some(bucket) = guard.get_mut(idx) else {
            return;
        };
        bucket.open_for(minute);
        bounded_push(&mut bucket.sheds, event);
    }

    /// Record an incident marker at `at_unix_seconds`.
    pub fn record_incident_at(
        &self,
        at_unix_seconds: u64,
        kind: IncidentKind,
        subject: impl Into<String>,
    ) {
        let minute = at_unix_seconds / BUCKET_SECONDS;
        let idx = Self::slot(minute);
        let marker = IncidentMarker {
            at_unix_seconds,
            kind,
            subject: subject.into(),
        };
        let mut guard = self.guard();
        let Some(bucket) = guard.get_mut(idx) else {
            return;
        };
        bucket.open_for(minute);
        bounded_push(&mut bucket.incidents, marker);
    }

    /// The inclusive minute-index range `[oldest, newest]` covered by `window`
    /// ending at `now_unix_seconds`. The oldest minute is clamped so it never
    /// reaches further back than [`RETENTION_BUCKETS`] minutes (the ring would
    /// have overwritten anything older).
    fn minute_range(now_unix_seconds: u64, window: RetentionWindow) -> (u64, u64) {
        let newest = now_unix_seconds / BUCKET_SECONDS;
        let window_minutes = window.seconds() / BUCKET_SECONDS;
        let cap = u64::try_from(RETENTION_BUCKETS).unwrap_or(u64::MAX);
        // Never look back further than the ring can hold (cap - 1 minutes before
        // `newest`, since `newest` itself is one of the retained buckets).
        let max_back = cap.saturating_sub(1);
        let back = window_minutes.saturating_sub(1).min(max_back);
        let oldest = newest.saturating_sub(back);
        (oldest, newest)
    }

    /// Whether a Unix-second timestamp falls strictly within `window` ending at
    /// `now` (inclusive of `now`, exclusive of the far edge), used to filter
    /// discrete events whose own timestamp is finer than minute resolution.
    fn in_window(at: u64, now: u64, window: RetentionWindow) -> bool {
        let cutoff = now.saturating_sub(window.seconds());
        at > cutoff && at <= now
    }

    /// Return the per-minute utilisation aggregates within `window` ending at
    /// `now_unix_seconds`, in ascending minute order. Only minutes that saw at
    /// least one sample are included.
    #[must_use]
    pub fn utilisation_window(
        &self,
        now_unix_seconds: u64,
        window: RetentionWindow,
    ) -> Vec<UtilisationBucket> {
        let (oldest, newest) = Self::minute_range(now_unix_seconds, window);
        let guard = self.guard();
        let mut out = Vec::new();
        let mut minute = oldest;
        while minute <= newest {
            let idx = Self::slot(minute);
            if let Some(bucket) = guard.get(idx) {
                if bucket.holds(minute) && bucket.util_count > 0 {
                    let count_f = u64_to_f64(bucket.util_count);
                    let gpu_mean = if bucket.gpu_count > 0 {
                        Some(bucket.gpu_sum / u64_to_f64(bucket.gpu_count))
                    } else {
                        None
                    };
                    out.push(UtilisationBucket {
                        minute_index: minute,
                        samples: bucket.util_count,
                        cpu_mean: bucket.cpu_sum / count_f,
                        cpu_min: bucket.cpu_min,
                        cpu_max: bucket.cpu_max,
                        gpu_mean,
                    });
                }
            }
            minute = minute.saturating_add(1);
            if minute == 0 {
                break; // saturated; stop to avoid an infinite loop at u64::MAX
            }
        }
        out
    }

    /// Summarise utilisation over `window`: the total sample count and CPU
    /// percentile estimates. `p0`/`p100` are the global min/max observed; `p50`
    /// and `p95` are percentiles over the window's per-minute CPU means. Returns
    /// `None` when the window holds no samples.
    #[must_use]
    pub fn utilisation_summary(
        &self,
        now_unix_seconds: u64,
        window: RetentionWindow,
    ) -> Option<UtilisationSummary> {
        let buckets = self.utilisation_window(now_unix_seconds, window);
        if buckets.is_empty() {
            return None;
        }
        let mut total_samples: u64 = 0;
        let mut global_min = f64::INFINITY;
        let mut global_max = f64::NEG_INFINITY;
        let mut means: Vec<f64> = Vec::with_capacity(buckets.len());
        for bucket in &buckets {
            total_samples = total_samples.saturating_add(bucket.samples);
            global_min = global_min.min(bucket.cpu_min);
            global_max = global_max.max(bucket.cpu_max);
            means.push(bucket.cpu_mean);
        }
        means.sort_by(f64::total_cmp);
        Some(UtilisationSummary {
            samples: total_samples,
            cpu_p0: global_min,
            cpu_p50: percentile(&means, 0.50),
            cpu_p95: percentile(&means, 0.95),
            cpu_p100: global_max,
        })
    }

    /// Return reconnect events within `window` ending at `now_unix_seconds`, in
    /// ascending time order.
    #[must_use]
    pub fn reconnect_window(
        &self,
        now_unix_seconds: u64,
        window: RetentionWindow,
    ) -> Vec<ReconnectEvent> {
        self.collect_window(
            now_unix_seconds,
            window,
            |bucket| &bucket.reconnects,
            |e| e.at_unix_seconds,
        )
    }

    /// Return shed-load events within `window` ending at `now_unix_seconds`, in
    /// ascending time order.
    #[must_use]
    pub fn shed_window(&self, now_unix_seconds: u64, window: RetentionWindow) -> Vec<ShedEvent> {
        self.collect_window(
            now_unix_seconds,
            window,
            |bucket| &bucket.sheds,
            |e| e.at_unix_seconds,
        )
    }

    /// Return incident markers within `window` ending at `now_unix_seconds`, in
    /// ascending time order.
    #[must_use]
    pub fn incident_window(
        &self,
        now_unix_seconds: u64,
        window: RetentionWindow,
    ) -> Vec<IncidentMarker> {
        self.collect_window(
            now_unix_seconds,
            window,
            |bucket| &bucket.incidents,
            |e| e.at_unix_seconds,
        )
    }

    /// Shared collector for the discrete-event windows: walks the in-window
    /// minute-buckets, clones each event whose own timestamp is within the window,
    /// and returns them in ascending time order.
    fn collect_window<T, F, G>(
        &self,
        now_unix_seconds: u64,
        window: RetentionWindow,
        select: F,
        time_of: G,
    ) -> Vec<T>
    where
        T: Clone,
        F: Fn(&Bucket) -> &Vec<T>,
        G: Fn(&T) -> u64,
    {
        let (oldest, newest) = Self::minute_range(now_unix_seconds, window);
        let guard = self.guard();
        let mut out: Vec<T> = Vec::new();
        let mut minute = oldest;
        while minute <= newest {
            let idx = Self::slot(minute);
            if let Some(bucket) = guard.get(idx) {
                if bucket.holds(minute) {
                    for event in select(bucket) {
                        if Self::in_window(time_of(event), now_unix_seconds, window) {
                            out.push(event.clone());
                        }
                    }
                }
            }
            minute = minute.saturating_add(1);
            if minute == 0 {
                break;
            }
        }
        out.sort_by_key(&time_of);
        out
    }
}

/// The `quantile`-th value of an already-sorted, non-empty slice using the
/// nearest-rank method. `quantile` is clamped to `0.0..=1.0`. Returns `0.0` for
/// an empty slice (callers guard against that, but this stays total).
fn percentile(sorted: &[f64], quantile: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let q = quantile.clamp(0.0, 1.0);
    let len = sorted.len();
    // nearest-rank: rank = ceil(q * n), 1-based; index = rank - 1, clamped.
    let last = len - 1;
    let scaled = q * u64_to_f64(u64::try_from(len).unwrap_or(u64::MAX));
    let rank = f64_ceil_to_usize(scaled);
    let index = rank.saturating_sub(1).min(last);
    sorted.get(index).copied().unwrap_or(0.0)
}

/// Ceil a finite, non-negative `f64` to a `usize` without an `as` cast, saturating
/// at `usize::MAX`. Used only for small percentile ranks here.
fn f64_ceil_to_usize(value: f64) -> usize {
    if value <= 0.0 {
        return 0;
    }
    let ceiled = value.ceil();
    // ceiled is a non-negative integer-valued f64; recover it via the u64 grid.
    let as_u64 = f64_integer_to_u64(ceiled);
    usize::try_from(as_u64).unwrap_or(usize::MAX)
}

/// Convert a finite, non-negative, integer-valued `f64` (`< 2^53`) to `u64`
/// without an `as` cast, reading the IEEE-754 fields. Saturates at `u64::MAX`.
fn f64_integer_to_u64(value: f64) -> u64 {
    let truncated = value.trunc();
    if truncated <= 0.0 {
        return 0;
    }
    let bits = truncated.to_bits();
    let exponent_biased = (bits >> 52) & 0x7FF;
    let mantissa = bits & 0x000F_FFFF_FFFF_FFFF;
    let Some(exponent) = exponent_biased.checked_sub(1023) else {
        return 0;
    };
    let significand = mantissa | 0x0010_0000_0000_0000; // implicit leading 1
    if exponent >= 52 {
        significand
            .checked_shl(u32::try_from(exponent - 52).unwrap_or(u32::MAX))
            .unwrap_or(u64::MAX)
    } else {
        significand >> (52 - exponent)
    }
}

/// Lossless `u64 -> f64` widening for the magnitudes here (counts well within
/// `2^53`), avoiding an `as` cast. Mirrors the `availability` module's helper.
fn u64_to_f64(value: u64) -> f64 {
    let high = u32::try_from(value >> 32).unwrap_or(u32::MAX);
    let low = u32::try_from(value & 0xFFFF_FFFF).unwrap_or(u32::MAX);
    f64::from(high) * 4_294_967_296.0 + f64::from(low)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::float_cmp)]
    use super::*;

    fn sample(cpu: f64) -> UtilisationSample {
        UtilisationSample {
            cpu_util: cpu,
            gpu_util: Some(cpu),
            program_fps: None,
        }
    }

    #[test]
    fn window_seconds_are_the_canonical_spans() {
        assert_eq!(RetentionWindow::LastHour.seconds(), 3_600);
        assert_eq!(RetentionWindow::LastDay.seconds(), 86_400);
        assert_eq!(RetentionWindow::LastWeek.seconds(), 604_800);
    }

    #[test]
    fn empty_window_summarises_to_none() {
        let store = RetentionStore::new();
        assert!(store
            .utilisation_summary(1_000_000, RetentionWindow::LastHour)
            .is_none());
    }

    #[test]
    fn percentile_nearest_rank() {
        let data = [0.0, 0.1, 0.2, 0.3, 0.4, 0.5, 0.6, 0.7, 0.8, 0.9];
        assert_eq!(percentile(&data, 1.0), 0.9, "p100 is the max");
        assert_eq!(percentile(&data, 0.0), 0.0, "p0 is the min");
        // p50 nearest-rank of 10 elements => rank 5 => index 4 => 0.4.
        assert_eq!(percentile(&data, 0.5), 0.4);
    }

    #[test]
    fn folds_multiple_samples_into_one_minute() {
        let store = RetentionStore::new();
        let now = 100 * 24 * 60 * 60; // a minute boundary
                                      // Three samples in the minute before `now`.
        store.record_utilisation_at(now - 1, sample(0.2));
        store.record_utilisation_at(now - 2, sample(0.4));
        store.record_utilisation_at(now - 3, sample(0.6));
        let buckets = store.utilisation_window(now, RetentionWindow::LastHour);
        assert_eq!(buckets.len(), 1, "all three fold into one minute bucket");
        let b = buckets.first().expect("one bucket");
        assert_eq!(b.samples, 3);
        assert_eq!(b.cpu_min, 0.2);
        assert_eq!(b.cpu_max, 0.6);
        assert!((b.cpu_mean - 0.4).abs() < 1e-9);
    }

    #[test]
    fn event_lists_are_bounded_per_minute() {
        let store = RetentionStore::new();
        let now = 200 * 24 * 60 * 60;
        // Push more than the cap of reconnects into a single minute.
        let cap = u64::try_from(MAX_EVENTS_PER_BUCKET).unwrap_or(u64::MAX);
        for i in 0..(cap + 20) {
            store.record_reconnect_at(now - 1, "cam", u32::try_from(i).unwrap_or(0));
        }
        let week = store.reconnect_window(now, RetentionWindow::LastWeek);
        assert_eq!(
            week.len(),
            MAX_EVENTS_PER_BUCKET,
            "a busy minute is capped at MAX_EVENTS_PER_BUCKET (drop-oldest)"
        );
    }
}
