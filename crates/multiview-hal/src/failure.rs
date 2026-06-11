//! The failure-learning ledger (Tier-2 gap P1c): a decaying, per-`(stage,
//! hardware)` penalty so a placement that keeps **flapping** on a piece of
//! hardware is *avoided* on the next selection — "a decode that keeps flapping
//! on a GPU → stop trying to decode it **there**" — without permanently banning
//! it (conditions change; a recovered device must become preferable again).
//!
//! ## What this is (and is not)
//!
//! This is a **pure, deterministic, wait-free-readable** data structure. It is
//! *not* the data plane and *not* the engine control loop:
//!
//! - It carries **no clock** — every mutating/reading call takes the current
//!   monotonic time in nanoseconds (`now_ns: i64`) as a parameter, so behaviour
//!   is fully reproducible in a unit test (no `Instant::now`/`Date::now` here,
//!   matching [`crate::select`]'s pure-core discipline).
//! - It is the typed seam the planner/admission path consumes: the data plane
//!   raises a [`FailureSignal`]; the engine's slow control tick folds it into the
//!   ledger via [`FailureLedger::record`]; the placement scorer consumes
//!   [`FailureLedger::penalty_for`] when ranking a candidate `(stage, hardware)`.
//!
//! ## The decay model (half-life)
//!
//! Each `(stage, hardware)` key accumulates a **penalty score** (`f64`,
//! surfaced as `f32`). A failure [`FailureLedger::record`] bumps the score by a
//! per-kind weight; between records the score decays **exponentially** with a
//! configured **half-life**:
//!
//! ```text
//! penalty(now) = score_at_last_record * 0.5 ^ ((now - last_record) / half_life)
//! ```
//!
//! So a placement that keeps failing sees its penalty **rise** (each record adds
//! to the already-present, only-partly-decayed score), and a placement that
//! stops failing sees its penalty **decay back toward 0** — self-healing, never a
//! permanent ban. The score is clamped to a finite ceiling so even a pathological
//! flood cannot produce an infinite (or NaN) penalty.
//!
//! ## How the scorer consumes it
//!
//! The scorer adds [`FailureLedger::penalty_for`] (scaled by a policy weight) to
//! a candidate's placement **cost**, so a high-penalty `(stage, hardware)` is
//! **deprioritised** but not hard-banned — unless the penalty is *extreme*
//! ([`FailureLedger::is_excluded`] crosses a hard-avoid threshold), in which case
//! that `(stage, hardware)` is dropped from this round's candidate set entirely
//! ("stop trying to decode it **there**"). Because the penalty decays, the
//! exclusion lifts on its own once the failures stop.
//!
//! ## Integration seams (where the engine wires this — wiring is NOT in this crate)
//!
//! These are the data-plane failure points that call [`FailureLedger::record`]
//! and the admission point that reads [`FailureLedger::penalty_for`]. They are
//! documented here so the (separate) engine integration knows the exact seams;
//! **this crate ships only the pure ledger**, never the wiring.
//!
//! **Producers — call [`FailureLedger::record`] (on the engine's slow control
//! tick, draining a bounded drop-oldest [`FailureSignal`] channel — inv #10,
//! never on the output-clock thread):**
//!
//! - `multiview-ffmpeg` `decode_stream` — when the `*_cuvid` / hwaccel decoder
//!   open fails (the HW→SW fallback site) on a specific CUDA ordinal: raise
//!   [`FailureSignal::decode_init_failed`] keyed to that GPU's [`HardwareId`].
//! - `multiview-ffmpeg` `decode_stream` — when a decoder *repeatedly* faults
//!   (the supervisor's consecutive-failure counter crosses its debounce
//!   threshold; a single corrupt packet must **not** penalise the GPU — bad
//!   inputs are the product): raise [`FailureSignal::decode_flapping`] with the
//!   observed `count`.
//! - `multiview-ffmpeg` hwframe pool / decoder — on a pool/device allocation that
//!   returns out-of-memory: raise [`FailureSignal::gpu_out_of_memory`].
//! - `multiview-ffmpeg` `encode` — when an NVENC encode-session open fails with
//!   the session-exhausted errno: raise [`FailureSignal::nvenc_session_exhausted`]
//!   (distinct from a generic open error).
//! - any backend — on a device-lost / reset event: raise
//!   [`FailureSignal::device_lost`].
//!
//! **Consumer — read [`FailureLedger::penalty_for`] / [`FailureLedger::is_excluded`]
//! at the admission point:**
//!
//! - `multiview-hal` `select` (the placement scorer, via the engine's
//!   `PlacementController` slow tick): when scoring each candidate `(stage,
//!   hardware)`, add `weight * penalty_for(stage, hardware, now)` to its cost and
//!   drop any candidate for which `is_excluded(stage, hardware, now)` is true.
//!
//! See [resilience-and-av](../../../docs/research/resilience-and-av.md),
//! [efficiency](../../../docs/research/efficiency.md), ADR-0035, and invariants
//! #1 (output clock — placement runs off the clock thread), #2 (last-good held
//! across a failure-driven re-select), and #9 (closed-loop degradation /
//! hysteresis — the decay *is* the hysteresis here).

use std::collections::HashMap;

use crate::capability::Stage;
use crate::load::DeviceId;

/// The hardware a [`FailureSignal`] / penalty is keyed to: a specific GPU, or
/// the always-available CPU/software tier.
///
/// This is the placement key half (the other half is the [`Stage`]). It is
/// `Hash + Eq` so `(Stage, HardwareId)` is a stable map key, reusing
/// [`DeviceId`]'s `(vendor, stable_id)` identity (the enumeration index is *not*
/// part of identity — a reorder across reboots must not change the key).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum HardwareId {
    /// A specific GPU, identified by its stable [`DeviceId`].
    Gpu(DeviceId),
    /// The CPU / software tier (always present; the universal fallback host).
    Cpu,
}

impl HardwareId {
    /// A GPU hardware id from a stable device identity.
    #[must_use]
    pub fn gpu(device: DeviceId) -> Self {
        Self::Gpu(device)
    }
}

/// A typed fault the data plane raises about a `(stage, hardware)` placement.
///
/// Each carries the [`Stage`] that faulted and the [`HardwareId`] it faulted on,
/// so the ledger can key the penalty to exactly the placement that keeps
/// flapping (and *only* that one — a decode failure on GPU1 never penalises
/// compositing on GPU1, nor decode on GPU0).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum FailureSignal {
    /// A decoder failed to **initialise** on this hardware (e.g. a `*_cuvid`
    /// open / hwaccel bind failure at admission). A heavyweight signal: the
    /// placement could not even start there.
    DecodeInitFailed {
        /// The faulting stage (always [`Stage::Decode`] for this variant; carried
        /// uniformly so all signals share the `(stage, hardware)` shape).
        stage_kind: Stage,
        /// The hardware the decode init failed on.
        hardware_id: HardwareId,
    },
    /// A decoder is **flapping** on this hardware: it has faulted `count` times
    /// in the debounce window (a *single* corrupt packet does not raise this —
    /// only a genuine repeated fault does). The repeated nature is the operator's
    /// core example: "a decode that keeps flapping on a piece of hardware".
    DecodeFlapping {
        /// The faulting stage (always [`Stage::Decode`] for this variant).
        stage_kind: Stage,
        /// The hardware the decode is flapping on.
        hardware_id: HardwareId,
        /// How many faults were observed in the debounce window (informs the
        /// penalty bump magnitude).
        count: u32,
    },
    /// A GPU allocation (frame pool / device buffer) returned out-of-memory for
    /// this stage on this hardware. Heavyweight: the stage cannot run there until
    /// VRAM frees.
    GpuOutOfMemory {
        /// The stage whose allocation went OOM.
        stage_kind: Stage,
        /// The hardware that went OOM.
        hardware_id: HardwareId,
    },
    /// An NVENC encode-session open failed because the per-system concurrent
    /// session ceiling is exhausted. Heavyweight and distinct from a generic
    /// encode error (it is a *capacity* fault, not a transient one).
    NvencSessionExhausted {
        /// The faulting stage (always [`Stage::Encode`] for this variant).
        stage_kind: Stage,
        /// The hardware whose NVENC sessions are exhausted.
        hardware_id: HardwareId,
    },
    /// The device was lost / reset (driver fault, hot-unplug, TDR). The
    /// heaviest signal: every stage on this hardware is suspect.
    DeviceLost {
        /// The stage observing the device loss.
        stage_kind: Stage,
        /// The hardware that was lost.
        hardware_id: HardwareId,
    },
}

impl FailureSignal {
    /// A decode-init-failed signal for a stage on a piece of hardware.
    ///
    /// `stage_kind` is carried explicitly (rather than hard-coded to
    /// [`Stage::Decode`]) so the constructor stays uniform across variants and a
    /// caller never has to reach into the enum to read the stage.
    #[must_use]
    pub fn decode_init_failed(hardware_id: HardwareId) -> Self {
        Self::DecodeInitFailed {
            stage_kind: Stage::Decode,
            hardware_id,
        }
    }

    /// A decode-flapping signal: `count` faults observed in the debounce window.
    #[must_use]
    pub fn decode_flapping(hardware_id: HardwareId, count: u32) -> Self {
        Self::DecodeFlapping {
            stage_kind: Stage::Decode,
            hardware_id,
            count,
        }
    }

    /// A GPU-out-of-memory signal for `stage` on a piece of hardware.
    #[must_use]
    pub fn gpu_out_of_memory(stage_kind: Stage, hardware_id: HardwareId) -> Self {
        Self::GpuOutOfMemory {
            stage_kind,
            hardware_id,
        }
    }

    /// An NVENC-session-exhausted signal for a piece of hardware.
    #[must_use]
    pub fn nvenc_session_exhausted(hardware_id: HardwareId) -> Self {
        Self::NvencSessionExhausted {
            stage_kind: Stage::Encode,
            hardware_id,
        }
    }

    /// A device-lost signal observed by `stage` on a piece of hardware.
    #[must_use]
    pub fn device_lost(stage_kind: Stage, hardware_id: HardwareId) -> Self {
        Self::DeviceLost {
            stage_kind,
            hardware_id,
        }
    }

    /// The faulting stage.
    #[must_use]
    pub const fn stage(&self) -> Stage {
        match self {
            Self::DecodeInitFailed { stage_kind, .. }
            | Self::DecodeFlapping { stage_kind, .. }
            | Self::GpuOutOfMemory { stage_kind, .. }
            | Self::NvencSessionExhausted { stage_kind, .. }
            | Self::DeviceLost { stage_kind, .. } => *stage_kind,
        }
    }

    /// The hardware the fault occurred on.
    #[must_use]
    pub const fn hardware(&self) -> &HardwareId {
        match self {
            Self::DecodeInitFailed { hardware_id, .. }
            | Self::DecodeFlapping { hardware_id, .. }
            | Self::GpuOutOfMemory { hardware_id, .. }
            | Self::NvencSessionExhausted { hardware_id, .. }
            | Self::DeviceLost { hardware_id, .. } => hardware_id,
        }
    }

    /// The penalty weight a single occurrence of this signal adds to the
    /// `(stage, hardware)` score.
    ///
    /// A capacity / hard fault (`DeviceLost`, `NvencSessionExhausted`,
    /// `GpuOutOfMemory`, `DecodeInitFailed`) is heavy — the placement could not
    /// run there at all. A flapping decode scales with the observed fault count
    /// (more faults in the window → a bigger bump), bounded so a runaway count
    /// cannot dominate the finite ceiling on its own.
    #[must_use]
    fn weight(&self) -> f64 {
        match self {
            // The device is gone: the heaviest single signal.
            Self::DeviceLost { .. } => 8.0,
            // Capacity exhausted / OOM / could-not-initialise: heavy (the
            // placement could not run there at all this attempt).
            Self::NvencSessionExhausted { .. }
            | Self::DecodeInitFailed { .. }
            | Self::GpuOutOfMemory { .. } => 4.0,
            // Flapping scales with the count, each fault worth ~1.0, capped so a
            // single signal can add at most `FLAP_WEIGHT_CAP`.
            Self::DecodeFlapping { count, .. } => {
                let count = f64::from(*count).max(1.0);
                count.min(FLAP_WEIGHT_CAP)
            }
        }
    }
}

/// The largest penalty a *single* `DecodeFlapping` signal may add (a runaway
/// fault count cannot, by itself, slam the score to the ceiling).
const FLAP_WEIGHT_CAP: f64 = 6.0;

/// The default half-life of the penalty decay, in nanoseconds (30 s).
///
/// After this much quiet time the penalty for a key halves; after a few
/// multiples it is effectively zero (self-healing). Chosen so a placement that
/// stops failing becomes preferable again within ~a minute or two, while a key
/// that keeps failing stays elevated.
pub const DEFAULT_HALF_LIFE_NS: i64 = 30_000_000_000;

/// The finite ceiling a penalty score is clamped to, so even a pathological
/// failure flood produces a large-but-finite (never infinite/NaN) penalty.
pub const PENALTY_CEILING: f32 = 100.0;

/// The default hard-avoid threshold: a `(stage, hardware)` whose live decayed
/// penalty is at or above this is **excluded** from this round's candidate set
/// ("stop trying to decode it there"). Set above a couple of heavy faults so a
/// single transient never excludes, but a sustained flap does.
pub const DEFAULT_EXCLUSION_THRESHOLD: f32 = 12.0;

/// The default maximum number of `(stage, hardware)` keys the ledger retains.
///
/// Bounded memory on a long-running daemon: when full and a brand-new key must
/// be inserted, the most-decayed (lowest live penalty) existing key is evicted
/// first. Generous relative to any real host's `stages × devices`.
pub const DEFAULT_MAX_KEYS: usize = 256;

/// Tuning for a [`FailureLedger`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LedgerConfig {
    /// Penalty decay half-life, in nanoseconds (`> 0`).
    pub half_life_ns: i64,
    /// Live-penalty value at/above which a key is hard-excluded from selection.
    pub exclusion_threshold: f32,
    /// Maximum retained keys before most-decayed eviction kicks in (`>= 1`).
    pub max_keys: usize,
}

impl LedgerConfig {
    /// The default tuning (30 s half-life, exclude at `12.0`, cap `256` keys).
    #[must_use]
    pub const fn new_default() -> Self {
        Self {
            half_life_ns: DEFAULT_HALF_LIFE_NS,
            exclusion_threshold: DEFAULT_EXCLUSION_THRESHOLD,
            max_keys: DEFAULT_MAX_KEYS,
        }
    }
}

impl Default for LedgerConfig {
    fn default() -> Self {
        Self::new_default()
    }
}

/// One key's accumulated penalty plus the time it was last updated.
#[derive(Debug, Clone, Copy, PartialEq)]
struct PenaltyEntry {
    /// The score as of `last_update_ns` (decay is applied lazily on read).
    score_at_last: f64,
    /// The monotonic time (ns) the score was last bumped.
    last_update_ns: i64,
}

/// The failure-learning ledger: a bounded, decaying penalty per `(stage,
/// hardware)`.
///
/// Single-owner (mutated + read on one thread — the engine's slow control tick,
/// off the output-clock thread), so it needs no internal locking; a snapshot can
/// be `Clone`d for read-only scoring. All time is injected (`now_ns`), so the
/// whole structure is deterministically unit-testable.
#[derive(Debug, Clone)]
pub struct FailureLedger {
    config: LedgerConfig,
    entries: HashMap<(Stage, HardwareId), PenaltyEntry>,
}

impl FailureLedger {
    /// Construct an empty ledger with the given config.
    ///
    /// A non-positive `half_life_ns` or a zero `max_keys` is repaired to the
    /// default (a degenerate config can never make the ledger panic or divide by
    /// zero).
    #[must_use]
    pub fn new(config: LedgerConfig) -> Self {
        let half_life_ns = if config.half_life_ns > 0 {
            config.half_life_ns
        } else {
            DEFAULT_HALF_LIFE_NS
        };
        let max_keys = config.max_keys.max(1);
        Self {
            config: LedgerConfig {
                half_life_ns,
                max_keys,
                ..config
            },
            entries: HashMap::new(),
        }
    }

    /// Construct an empty ledger with the default config.
    #[must_use]
    pub fn new_default() -> Self {
        Self::new(LedgerConfig::new_default())
    }

    /// The configuration in force.
    #[must_use]
    pub const fn config(&self) -> &LedgerConfig {
        &self.config
    }

    /// The number of `(stage, hardware)` keys currently retained.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the ledger holds no keys.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Record a [`FailureSignal`] at monotonic time `now_ns`: decay the affected
    /// key's existing score to *now*, add this signal's weight, clamp to the
    /// finite ceiling, and stamp the update time.
    ///
    /// A repeated failure therefore *accumulates* (the new bump rides on the
    /// only-partly-decayed previous score → the penalty **rises** with repeated
    /// failures), while quiet time between records lets it **decay** back toward
    /// zero. Inserting a brand-new key when the ledger is at `max_keys` first
    /// evicts the most-decayed existing key (bounded memory).
    pub fn record(&mut self, signal: &FailureSignal, now_ns: i64) {
        let key = (signal.stage(), signal.hardware().clone());
        let bump = signal.weight();

        let decayed = self.entries.get(&key).map_or(0.0, |entry| {
            decay(
                entry.score_at_last,
                entry.last_update_ns,
                now_ns,
                self.config.half_life_ns,
            )
        });

        // Accumulate then clamp to the finite ceiling (never infinite/NaN).
        let next = (decayed + bump).min(f64::from(PENALTY_CEILING)).max(0.0);

        if !self.entries.contains_key(&key) && self.entries.len() >= self.config.max_keys {
            self.evict_most_decayed(now_ns);
        }

        self.entries.insert(
            key,
            PenaltyEntry {
                score_at_last: next,
                last_update_ns: now_ns,
            },
        );
    }

    /// The current **decayed** penalty for a `(stage, hardware)` at time
    /// `now_ns` (`0.0` for an unknown key). Higher = more recently/repeatedly
    /// flapping; decays toward `0.0` as quiet time passes.
    ///
    /// This is the value the placement scorer adds (weighted) to a candidate's
    /// placement cost.
    #[must_use]
    pub fn penalty_for(&self, stage: Stage, hardware: &HardwareId, now_ns: i64) -> f32 {
        let key = (stage, hardware.clone());
        let value = self.entries.get(&key).map_or(0.0, |entry| {
            decay(
                entry.score_at_last,
                entry.last_update_ns,
                now_ns,
                self.config.half_life_ns,
            )
        });
        f64_to_f32_clamped(value)
    }

    /// Whether a `(stage, hardware)` is **hard-excluded** from this round's
    /// candidate set — its live decayed penalty is at or above the configured
    /// exclusion threshold ("stop trying to decode it **there**"). Because the
    /// penalty decays, the exclusion lifts on its own once the failures stop.
    #[must_use]
    pub fn is_excluded(&self, stage: Stage, hardware: &HardwareId, now_ns: i64) -> bool {
        self.penalty_for(stage, hardware, now_ns) >= self.config.exclusion_threshold
    }

    /// Drop every key whose live decayed penalty has fallen below `epsilon` at
    /// `now_ns` (housekeeping the slow tick can call to reclaim fully-healed
    /// keys). Returns how many keys were evicted.
    ///
    /// A fully-decayed key contributes `0.0` to scoring whether present or not,
    /// so evicting it is behaviour-preserving — it only reclaims memory.
    pub fn evict_decayed(&mut self, now_ns: i64, epsilon: f32) -> usize {
        let half_life = self.config.half_life_ns;
        let before = self.entries.len();
        let eps = f64::from(epsilon.max(0.0));
        self.entries.retain(|_, entry| {
            decay(entry.score_at_last, entry.last_update_ns, now_ns, half_life) >= eps
        });
        before - self.entries.len()
    }

    /// Evict the single key with the lowest live decayed penalty at `now_ns`
    /// (the most-healed key), to make room for a new insert at the cap. Keeps
    /// memory bounded while preferentially retaining the *worst* offenders.
    fn evict_most_decayed(&mut self, now_ns: i64) {
        let half_life = self.config.half_life_ns;
        let victim = self
            .entries
            .iter()
            .min_by(|(_, a), (_, b)| {
                let pa = decay(a.score_at_last, a.last_update_ns, now_ns, half_life);
                let pb = decay(b.score_at_last, b.last_update_ns, now_ns, half_life);
                pa.partial_cmp(&pb).unwrap_or(core::cmp::Ordering::Equal)
            })
            .map(|(key, _)| key.clone());
        if let Some(key) = victim {
            self.entries.remove(&key);
        }
    }
}

impl Default for FailureLedger {
    fn default() -> Self {
        Self::new_default()
    }
}

/// Exponential half-life decay of a score from `last_ns` to `now_ns`.
///
/// `score * 0.5 ^ ((now - last) / half_life)`. A non-monotonic `now` (clock
/// hiccup) or a zero/negative elapsed never *increases* the score — elapsed is
/// clamped non-negative — and a non-positive half-life is treated as the default
/// so it can never divide by zero or produce NaN/∞.
fn decay(score: f64, last_ns: i64, now_ns: i64, half_life_ns: i64) -> f64 {
    if score <= 0.0 {
        return 0.0;
    }
    let half_life = if half_life_ns > 0 {
        half_life_ns
    } else {
        DEFAULT_HALF_LIFE_NS
    };
    // Elapsed nanoseconds, clamped non-negative (a backwards clock cannot
    // resurrect a decayed penalty).
    let elapsed = now_ns.saturating_sub(last_ns).max(0);
    let elapsed = u64_to_f64(u64::try_from(elapsed).unwrap_or(0));
    let half_life = u64_to_f64(u64::try_from(half_life).unwrap_or(1).max(1));
    let exponent = elapsed / half_life;
    let factor = 0.5_f64.powf(exponent);
    (score * factor).max(0.0)
}

/// Lossless `u64 -> f64` widening (values `< 2^53`), avoiding an `as` cast.
///
/// Mirrors the helper in [`crate::capability`] / [`crate::load`]: split into
/// 32-bit halves, both of which widen exactly, then recombine.
fn u64_to_f64(value: u64) -> f64 {
    u32::try_from(value).map_or_else(
        |_| {
            let high = u32::try_from((value >> 32) & 0xFFFF_FFFF).map_or(f64::INFINITY, f64::from);
            let low = u32::try_from(value & 0xFFFF_FFFF).map_or(f64::INFINITY, f64::from);
            high * 4_294_967_296.0 + low
        },
        f64::from,
    )
}

/// Narrow a finite `f64` penalty (already in the `0.0..=PENALTY_CEILING` domain)
/// to `f32` **without an `as` cast** (the workspace lints deny `as_conversions`).
///
/// There is no `From<f64> for f32`, so — mirroring `multiview-audio`'s
/// `db_to_f32` — we round-trip through the decimal string the formatter
/// produces, which `f32::from_str` parses with standard round-to-nearest. The
/// value is small-magnitude and finite (clamped first), so this is exact to
/// `f32` precision; a non-finite intermediate maps to the ceiling (never NaN/∞
/// in a score). This runs only on the off-hot-path slow control tick, never on
/// the output clock.
fn f64_to_f32_clamped(value: f64) -> f32 {
    if !value.is_finite() {
        return PENALTY_CEILING;
    }
    let clamped = value.clamp(0.0, f64::from(PENALTY_CEILING));
    clamped
        .to_string()
        .parse::<f32>()
        .unwrap_or(PENALTY_CEILING)
        .clamp(0.0, PENALTY_CEILING)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::float_cmp)]
    use super::*;
    use crate::load::Vendor;
    use proptest::prelude::*;

    /// One second / one half-life in nanoseconds, for readable test times.
    const SECOND_NS: i64 = 1_000_000_000;

    fn gpu(index: u32) -> HardwareId {
        HardwareId::gpu(DeviceId::new(Vendor::Nvidia, format!("GPU-{index}"), index))
    }

    /// A short half-life (1 s) ledger so decay over test windows is easy to read.
    fn ledger_1s_half_life() -> FailureLedger {
        FailureLedger::new(LedgerConfig {
            half_life_ns: SECOND_NS,
            ..LedgerConfig::new_default()
        })
    }

    #[test]
    fn unknown_key_has_zero_penalty() {
        let ledger = FailureLedger::new_default();
        assert_eq!(ledger.penalty_for(Stage::Decode, &gpu(1), 0), 0.0);
        assert!(ledger.is_empty());
    }

    #[test]
    fn record_raises_penalty_above_zero() {
        let mut ledger = FailureLedger::new_default();
        ledger.record(&FailureSignal::decode_init_failed(gpu(1)), 0);
        let p = ledger.penalty_for(Stage::Decode, &gpu(1), 0);
        assert!(
            p > 0.0,
            "a recorded failure must raise the penalty, got {p}"
        );
    }

    #[test]
    fn repeated_failures_make_penalty_rise() {
        // The operator's core ask: a placement that keeps FLAPPING accumulates a
        // RISING penalty. Three flaps in quick succession must strictly increase
        // the penalty each time (each bump rides the only-partly-decayed prior).
        let mut ledger = ledger_1s_half_life();
        let hw = gpu(1);

        ledger.record(&FailureSignal::decode_flapping(hw.clone(), 3), 0);
        let p1 = ledger.penalty_for(Stage::Decode, &hw, 0);

        // A little later (well under a half-life), another flap.
        ledger.record(
            &FailureSignal::decode_flapping(hw.clone(), 3),
            SECOND_NS / 10,
        );
        let p2 = ledger.penalty_for(Stage::Decode, &hw, SECOND_NS / 10);

        ledger.record(
            &FailureSignal::decode_flapping(hw.clone(), 3),
            SECOND_NS / 5,
        );
        let p3 = ledger.penalty_for(Stage::Decode, &hw, SECOND_NS / 5);

        assert!(
            p2 > p1 && p3 > p2,
            "penalty must RISE with repeated failures: p1={p1} p2={p2} p3={p3}"
        );
    }

    #[test]
    fn penalty_halves_after_one_half_life() {
        // The decay model is a half-life: after exactly one half-life of quiet,
        // the penalty is (within fp tolerance) half what it was.
        let mut ledger = ledger_1s_half_life();
        let hw = gpu(1);
        ledger.record(&FailureSignal::decode_init_failed(hw.clone()), 0);
        let at_record = ledger.penalty_for(Stage::Decode, &hw, 0);
        let after_one = ledger.penalty_for(Stage::Decode, &hw, SECOND_NS);
        let ratio = after_one / at_record;
        assert!(
            (ratio - 0.5).abs() < 1e-3,
            "after one half-life the penalty must halve: {at_record} -> {after_one} (ratio {ratio})"
        );
    }

    #[test]
    fn penalty_decays_toward_zero_after_a_quiet_period_no_permanent_ban() {
        // NOT a permanent ban: after many half-lives of quiet, the penalty falls
        // back to ~0 and the placement is preferable again.
        let mut ledger = ledger_1s_half_life();
        let hw = gpu(1);
        ledger.record(&FailureSignal::device_lost(Stage::Decode, hw.clone()), 0);
        let hot = ledger.penalty_for(Stage::Decode, &hw, 0);
        assert!(hot > 0.0);

        // 20 half-lives later: 0.5^20 ~= 1e-6 of the original -> effectively zero.
        let quiet = ledger.penalty_for(Stage::Decode, &hw, 20 * SECOND_NS);
        assert!(
            quiet < 0.01,
            "after a long quiet period the penalty must decay toward 0 (no permanent ban): {quiet}"
        );
        assert!(
            quiet < hot,
            "the decayed penalty must be far below the hot one: {quiet} < {hot}"
        );
    }

    #[test]
    fn penalty_is_keyed_to_exact_stage_and_hardware() {
        // A decode failure on GPU1 must penalise ONLY (Decode, GPU1): never
        // (Composite, GPU1), never (Decode, GPU0).
        let mut ledger = FailureLedger::new_default();
        ledger.record(&FailureSignal::decode_init_failed(gpu(1)), 0);

        assert!(ledger.penalty_for(Stage::Decode, &gpu(1), 0) > 0.0);
        assert_eq!(
            ledger.penalty_for(Stage::Composite, &gpu(1), 0),
            0.0,
            "a decode fault must not penalise compositing on the same GPU"
        );
        assert_eq!(
            ledger.penalty_for(Stage::Decode, &gpu(0), 0),
            0.0,
            "a fault on GPU1 must not penalise GPU0"
        );
    }

    #[test]
    fn flapping_gpu_is_more_costly_than_a_clean_gpu_in_a_comparison() {
        // The headline behaviour the scorer consumes: a flapping decode on GPU1
        // makes (Decode, GPU1) strictly MORE costly than (Decode, GPU0) so the
        // planner deprioritises GPU1 for the next decode placement.
        let mut ledger = ledger_1s_half_life();
        let gpu0 = gpu(0);
        let gpu1 = gpu(1);

        // GPU1's decode flaps repeatedly; GPU0 is clean.
        for tick in 0..4 {
            ledger.record(
                &FailureSignal::decode_flapping(gpu1.clone(), 2),
                tick * (SECOND_NS / 10),
            );
        }
        let now = 4 * (SECOND_NS / 10);

        let cost_gpu0 = ledger.penalty_for(Stage::Decode, &gpu0, now);
        let cost_gpu1 = ledger.penalty_for(Stage::Decode, &gpu1, now);
        assert_eq!(cost_gpu0, 0.0, "the clean GPU carries no penalty");
        assert!(
            cost_gpu1 > cost_gpu0,
            "the flapping GPU1 decode must be more costly than the clean GPU0 decode: {cost_gpu1} > {cost_gpu0}"
        );
    }

    #[test]
    fn extreme_penalty_excludes_then_decay_lifts_the_exclusion() {
        // A sustained flap drives the penalty past the exclusion threshold ->
        // hard-avoided this round ("stop trying to decode it THERE"); after the
        // penalty decays it is no longer excluded (self-healing, not a ban).
        let mut ledger = FailureLedger::new(LedgerConfig {
            half_life_ns: SECOND_NS,
            exclusion_threshold: 12.0,
            ..LedgerConfig::new_default()
        });
        let hw = gpu(1);

        // Several heavy faults in quick succession exceed the threshold.
        for tick in 0..5 {
            ledger.record(
                &FailureSignal::decode_init_failed(hw.clone()),
                tick * (SECOND_NS / 100),
            );
        }
        let hot_now = 5 * (SECOND_NS / 100);
        assert!(
            ledger.is_excluded(Stage::Decode, &hw, hot_now),
            "a sustained flap must hard-exclude the placement, penalty={}",
            ledger.penalty_for(Stage::Decode, &hw, hot_now)
        );

        // Many half-lives later, the exclusion must have lifted.
        let later = hot_now + 20 * SECOND_NS;
        assert!(
            !ledger.is_excluded(Stage::Decode, &hw, later),
            "the exclusion must lift once the penalty decays (no permanent ban)"
        );
    }

    #[test]
    fn evict_decayed_removes_fully_healed_entries_only() {
        // A fully-decayed key contributes 0.0 whether present or not, so evicting
        // it is behaviour-preserving and reclaims memory; a still-hot key stays.
        let mut ledger = ledger_1s_half_life();
        let healed = gpu(1);
        let hot = gpu(2);

        ledger.record(&FailureSignal::decode_init_failed(healed.clone()), 0);
        // The hot key is recorded much later so it is still elevated at `now`.
        let now = 30 * SECOND_NS;
        ledger.record(&FailureSignal::decode_init_failed(hot.clone()), now);
        assert_eq!(ledger.len(), 2);

        // At `now`, the first key has decayed ~30 half-lives -> ~0.
        let evicted = ledger.evict_decayed(now, 0.01);
        assert_eq!(evicted, 1, "exactly the fully-healed key is evicted");
        assert_eq!(ledger.len(), 1);
        assert_eq!(
            ledger.penalty_for(Stage::Decode, &healed, now),
            0.0,
            "the evicted key reads as zero (unchanged behaviour)"
        );
        assert!(
            ledger.penalty_for(Stage::Decode, &hot, now) > 0.0,
            "the still-hot key is retained"
        );
    }

    #[test]
    fn bounded_map_caps_keys_evicting_the_most_decayed() {
        // The map is bounded: with a cap of 2, inserting a 3rd brand-new key
        // evicts the most-decayed existing key, never grows past the cap.
        let mut ledger = FailureLedger::new(LedgerConfig {
            half_life_ns: SECOND_NS,
            max_keys: 2,
            ..LedgerConfig::new_default()
        });

        // key A recorded earliest (will be most decayed), B later, C newest.
        ledger.record(&FailureSignal::decode_init_failed(gpu(0)), 0);
        ledger.record(&FailureSignal::decode_init_failed(gpu(1)), 10 * SECOND_NS);
        assert_eq!(ledger.len(), 2);

        // Inserting a third key at t = 20s: A (decayed ~20 half-lives) is the
        // most-decayed and must be the eviction victim. Never exceeds the cap.
        ledger.record(&FailureSignal::decode_init_failed(gpu(2)), 20 * SECOND_NS);
        assert_eq!(ledger.len(), 2, "the cap is never exceeded");
        assert_eq!(
            ledger.penalty_for(Stage::Decode, &gpu(0), 20 * SECOND_NS),
            0.0,
            "the most-decayed key (A) was the eviction victim"
        );
        assert!(
            ledger.penalty_for(Stage::Decode, &gpu(2), 20 * SECOND_NS) > 0.0,
            "the freshly-inserted key (C) is retained"
        );
    }

    #[test]
    fn penalty_never_exceeds_the_finite_ceiling() {
        // A pathological flood must clamp to a finite ceiling, never blow up to
        // infinity/NaN (which would corrupt the scorer's cost sum).
        let mut ledger = FailureLedger::new_default();
        let hw = gpu(1);
        for _ in 0..1000 {
            ledger.record(&FailureSignal::device_lost(Stage::Decode, hw.clone()), 0);
        }
        let p = ledger.penalty_for(Stage::Decode, &hw, 0);
        assert!(p.is_finite(), "penalty must stay finite under a flood: {p}");
        assert!(
            p <= PENALTY_CEILING,
            "penalty must be clamped to the ceiling: {p} <= {PENALTY_CEILING}"
        );
    }

    #[test]
    fn backwards_clock_never_resurrects_a_decayed_penalty() {
        // A non-monotonic `now` must never INCREASE a penalty (elapsed clamps to
        // zero) — a clock hiccup cannot make a healed placement look bad.
        let mut ledger = ledger_1s_half_life();
        let hw = gpu(1);
        ledger.record(
            &FailureSignal::decode_init_failed(hw.clone()),
            10 * SECOND_NS,
        );
        let at_record = ledger.penalty_for(Stage::Decode, &hw, 10 * SECOND_NS);
        // Query at an EARLIER time than the record.
        let earlier = ledger.penalty_for(Stage::Decode, &hw, 0);
        assert!(
            earlier <= at_record,
            "a backwards clock must not increase the penalty: {earlier} <= {at_record}"
        );
    }

    #[test]
    fn degenerate_config_is_repaired_not_panicking() {
        // A non-positive half-life / zero cap must be repaired, never divide by
        // zero or panic.
        let ledger = FailureLedger::new(LedgerConfig {
            half_life_ns: 0,
            exclusion_threshold: 5.0,
            max_keys: 0,
        });
        assert!(ledger.config().half_life_ns > 0);
        assert!(ledger.config().max_keys >= 1);
    }

    #[test]
    fn signal_accessors_report_stage_and_hardware() {
        let s = FailureSignal::nvenc_session_exhausted(gpu(2));
        assert_eq!(s.stage(), Stage::Encode);
        assert_eq!(s.hardware(), &gpu(2));

        let d = FailureSignal::gpu_out_of_memory(Stage::Composite, gpu(0));
        assert_eq!(d.stage(), Stage::Composite);
        assert_eq!(d.hardware(), &gpu(0));
    }

    proptest! {
        /// Pure decay is monotonic non-increasing: between two records, a later
        /// query never yields a HIGHER penalty than an earlier one.
        #[test]
        fn prop_penalty_monotonic_non_increasing_between_records(
            t0 in 0i64..1_000_000_000i64,
            gap_a in 0i64..10_000_000_000i64,
            gap_b in 0i64..10_000_000_000i64,
        ) {
            let mut ledger = ledger_1s_half_life();
            let hw = gpu(1);
            ledger.record(&FailureSignal::decode_init_failed(hw.clone()), t0);
            let (lo, hi) = if gap_a <= gap_b { (gap_a, gap_b) } else { (gap_b, gap_a) };
            let earlier = ledger.penalty_for(Stage::Decode, &hw, t0.saturating_add(lo));
            let later = ledger.penalty_for(Stage::Decode, &hw, t0.saturating_add(hi));
            prop_assert!(
                later <= earlier + 1e-4,
                "decay must be monotonic non-increasing: later={later} earlier={earlier}"
            );
        }

        /// An empty ledger is the identity for scoring: every key reads 0.0 and
        /// no key is ever excluded — i.e. wiring a failure ledger in with zero
        /// recorded failures cannot change a selection.
        #[test]
        fn prop_empty_ledger_is_scoring_identity(
            stage_pick in 0usize..3usize,
            index in 0u32..8u32,
            now in any::<i64>(),
        ) {
            let ledger = FailureLedger::new_default();
            let stage = Stage::ALL[stage_pick % 3];
            let hw = gpu(index);
            prop_assert_eq!(ledger.penalty_for(stage, &hw, now), 0.0);
            prop_assert!(!ledger.is_excluded(stage, &hw, now));
        }

        /// The penalty is always finite and within `[0, ceiling]`, for any
        /// sequence of records at any (possibly non-monotonic) times.
        #[test]
        fn prop_penalty_always_finite_and_bounded(
            times in proptest::collection::vec(any::<i64>(), 1..16),
            query in any::<i64>(),
        ) {
            let mut ledger = FailureLedger::new_default();
            let hw = gpu(1);
            for t in &times {
                ledger.record(&FailureSignal::decode_init_failed(hw.clone()), *t);
            }
            let p = ledger.penalty_for(Stage::Decode, &hw, query);
            prop_assert!(p.is_finite());
            prop_assert!((0.0..=PENALTY_CEILING).contains(&p));
        }
    }
}
