//! Perf-class weighting: a *real* per-GPU [`CostBudget`] (Tier-2 gap P1b).
//!
//! Placement scoring ([`crate::select::select_device`]) gates each candidate GPU
//! through the per-engine cost budget ([`crate::cost::CostBudget`]) via the
//! existing [`crate::planner::Planner::admit`]. That gate is only as good as the
//! budget it is handed: when every candidate is given a uniform, sky-high budget
//! (the placeholder `CostBudget::new(100_000, 100_000, 100_000)`), a 2016 Pascal
//! **Quadro P2000** and an Ada **RTX 4060** score identically on the load
//! fractions alone — and a too-weak GPU can be picked for a stage it cannot
//! sustain, risking the output clock (invariant #1).
//!
//! [`PerfClass`] closes that gap. It models, per pipeline [`Stage`], the
//! sustainable throughput ceiling (in **megapixels/sec**) of a specific GPU, and
//! turns it into a per-stage [`CostBudget`] via [`PerfClass::stage_budgets`]. No
//! new gate is added: the existing [`Planner::admit`] budget check *becomes* the
//! cadence-sustain check the moment the budget it compares against is real. A 4K30
//! composite demand (~249 Mpix/s) then fits the 4060's budget but **exceeds** the
//! P2000's — so the P2000 is gated out for that stage and a stronger home (or the
//! CPU fallback / degrade-to-fit ladder) is chosen instead. The clock never
//! blocks on a GPU that cannot keep cadence.
//!
//! ## Signal priority (most → least trustworthy)
//!
//! [`PerfClass::for_device`] resolves a class from the signals a probe can supply,
//! in this fixed order:
//!
//! 1. **NVML `num_cores() × max_clock_info(Graphics)`** scaled linearly from one
//!    calibrated anchor GPU (see [`anchor`]). This is the most direct sustainable
//!    -throughput proxy we can read at runtime; a device with more shader cores
//!    running faster sustains proportionally more per-tile work.
//! 2. **Architecture / CUDA compute-capability** → a coarse per-generation tier
//!    ([`arch_tier`]) when cores×clock is unavailable but the silicon generation
//!    is known.
//! 3. **Name-substring** match against [`ARCH_TABLE`] (case-insensitive) — the
//!    last resort that still recognises a *specific* known card by its marketing
//!    name (covers the box's "RTX 4060" and "P2000").
//! 4. **[`DEFAULT_PERF_CLASS`]** — a conservative, *finite* (~1080p60 composite)
//!    fallback for a GPU we recognise by none of the above. It is **never
//!    infinite**: an unknown GPU is never trusted to sustain an arbitrarily heavy
//!    stage (the inv-#1 guard).
//!
//! A [`PerfClass::cpu`] software tier (low ceilings) makes a CPU/software target
//! representable in the same model, so software fallback for any stage is costed —
//! possible, but expensive.
//!
//! All figures here are **pure data**; no GPU, no I/O, no native deps. The live
//! probe (`cuda`-gated, in [`crate::load`]) merely *populates the signals*
//! ([`PerfSignals`]); the CLI integration derives the [`PerfClass`] per candidate
//! and stores its [`stage_budgets`](PerfClass::stage_budgets) as that candidate's
//! [`GpuCandidate::budget`](crate::select::GpuCandidate::budget).
//!
//! See [efficiency §1,§4](../../../docs/research/efficiency.md), ADR-0035, and
//! invariants #1/#6/#9.

use crate::capability::Stage;
use crate::cost::CostBudget;

/// The per-stage sustainable-throughput ceilings of one GPU (or the CPU
/// software tier), in **megapixels/sec**.
///
/// Each field is the most decode/composite/encode load that engine can keep up
/// with at the output cadence. [`stage_budgets`](PerfClass::stage_budgets)
/// projects this into the existing per-stage [`CostBudget`] the planner gates on,
/// so a perf class is "the real budget for this device".
///
/// Every ceiling is finite and `>= 0.0` by construction — there is deliberately
/// no way to build an *infinite* perf class, because an unbounded budget would
/// re-introduce the inert-gate bug this module exists to fix.
#[derive(Debug, Clone, Copy, PartialEq)]
// Justification: the per-stage fields intentionally mirror `CostBudget`'s
// canonical `{decode,composite,encode}_mpps` naming (invariant #6 — every engine
// budget is megapixels/sec); `stage_budgets` maps them one-to-one. The shared
// `_mpps` unit suffix is the domain convention here, not an accidental
// repetition, so the `struct_field_names` postfix lint is suppressed for this
// one struct (root cause is naming convention, deliberately kept aligned).
#[allow(clippy::struct_field_names)]
pub struct PerfClass {
    /// Sustainable decode (NVDEC / VAAPI / software) throughput ceiling, Mpix/s.
    decode_mpps: f64,
    /// Sustainable composite (GPU compositor) throughput ceiling, Mpix/s.
    composite_mpps: f64,
    /// Sustainable encode (NVENC / VAAPI / x264) throughput ceiling, Mpix/s.
    encode_mpps: f64,
}

impl PerfClass {
    /// Construct a perf class from explicit per-stage ceilings.
    ///
    /// Each ceiling is sanitised to a finite, non-negative value (a non-finite or
    /// negative input collapses to `0.0`) so a [`PerfClass`] can never carry an
    /// infinite or NaN budget into the gate.
    #[must_use]
    pub fn new(decode_mpps: f64, composite_mpps: f64, encode_mpps: f64) -> Self {
        Self {
            decode_mpps: finite_non_negative(decode_mpps),
            composite_mpps: finite_non_negative(composite_mpps),
            encode_mpps: finite_non_negative(encode_mpps),
        }
    }

    /// The decode-stage throughput ceiling, Mpix/s.
    #[must_use]
    pub const fn decode_mpps_ceiling(&self) -> f64 {
        self.decode_mpps
    }

    /// The composite-stage throughput ceiling, Mpix/s.
    #[must_use]
    pub const fn composite_mpps_ceiling(&self) -> f64 {
        self.composite_mpps
    }

    /// The encode-stage throughput ceiling, Mpix/s.
    #[must_use]
    pub const fn encode_mpps_ceiling(&self) -> f64 {
        self.encode_mpps
    }

    /// The ceiling for a single [`Stage`], Mpix/s.
    #[must_use]
    pub const fn ceiling_for(&self, stage: Stage) -> f64 {
        match stage {
            Stage::Decode => self.decode_mpps,
            Stage::Composite => self.composite_mpps,
            Stage::Encode => self.encode_mpps,
        }
    }

    /// Project this perf class into the per-stage [`CostBudget`] the placement
    /// scorer gates on.
    ///
    /// This is the seam that makes [`crate::planner::Planner::admit`] a real
    /// cadence-sustain check: the budget it compares the demand against is now
    /// this device's actual ceilings, not a uniform placeholder.
    #[must_use]
    pub const fn stage_budgets(&self) -> CostBudget {
        CostBudget::new(self.decode_mpps, self.composite_mpps, self.encode_mpps)
    }

    /// The CPU / software-backend tier: low, finite ceilings.
    ///
    /// Software decode/composite/encode is genuinely possible (the always-on
    /// `software` backend) but expensive — so a CPU target is *representable* in
    /// the placement model yet ranks far below any real GPU. Its composite ceiling
    /// sits below [`DEFAULT_PERF_CLASS`] so a software fallback is always the
    /// least-preferred home for a heavy stage.
    #[must_use]
    pub fn cpu() -> Self {
        Self::new(CPU_DECODE, CPU_COMPOSITE, CPU_ENCODE)
    }

    /// Resolve a perf class from the available device signals, following the
    /// documented signal priority (NVML cores×clock → architecture → name table →
    /// [`DEFAULT_PERF_CLASS`]).
    #[must_use]
    pub fn for_device(signals: &PerfSignals) -> Self {
        // Priority 1: NVML cores x clock scaled from the calibrated anchor.
        if let (Some(cores), Some(clock_mhz)) = (signals.num_cores, signals.max_graphics_clock_mhz)
        {
            if cores > 0 && clock_mhz > 0 {
                return anchor::scaled(cores, clock_mhz);
            }
        }

        // Priority 2: architecture / compute-capability -> coarse generation tier.
        if let Some(arch) = signals.architecture.as_deref() {
            if let Some(class) = arch_tier(arch) {
                return class;
            }
        }

        // Priority 3: name-substring match against the per-card table.
        if let Some(name) = signals.name.as_deref() {
            if let Some(class) = name_table_lookup(name) {
                return class;
            }
        }

        // Priority 4: the conservative, finite default.
        DEFAULT_PERF_CLASS
    }
}

/// Clamp an input ceiling to a finite, non-negative value.
fn finite_non_negative(value: f64) -> f64 {
    if value.is_finite() && value >= 0.0 {
        value
    } else {
        0.0
    }
}

/// The signals a probe can supply about a device, in priority order of
/// trustworthiness (see [`PerfClass::for_device`]).
///
/// Every field is optional: a probe fills what the platform exposes and leaves
/// the rest `None` (the honest unknown, never a fabricated value), exactly as
/// [`crate::load::DeviceLoad`] does for live load.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PerfSignals {
    /// Marketing/device name (e.g. `"NVIDIA GeForce RTX 4060"`), for the
    /// name-substring table.
    pub name: Option<String>,
    /// Shader-core count (NVML `num_cores()`), for the cores×clock proxy.
    pub num_cores: Option<u32>,
    /// Max graphics-domain clock in MHz (NVML `max_clock_info(Graphics)`), for
    /// the cores×clock proxy.
    pub max_graphics_clock_mhz: Option<u32>,
    /// Architecture / generation string (e.g. `"Ada"`, `"Pascal"`), for the
    /// coarse per-generation tier.
    pub architecture: Option<String>,
}

impl PerfSignals {
    /// Signals carrying only a device name (the name-table path).
    #[must_use]
    pub fn from_name(name: impl Into<String>) -> Self {
        Self {
            name: Some(name.into()),
            ..Self::default()
        }
    }

    /// Signals as an NVML probe would supply them: name, cores, max graphics
    /// clock (MHz), and an architecture string, each optional.
    #[must_use]
    pub fn from_nvml(
        name: Option<&str>,
        num_cores: Option<u32>,
        max_graphics_clock_mhz: Option<u32>,
        architecture: Option<&str>,
    ) -> Self {
        Self {
            name: name.map(str::to_owned),
            num_cores,
            max_graphics_clock_mhz,
            architecture: architecture.map(str::to_owned),
        }
    }
}

/// The calibrated anchor used to scale the NVML cores×clock signal.
///
/// We calibrate from **one** real GPU and scale every other probeable device
/// linearly by its `cores × clock` product. The anchor is the box's
/// **NVIDIA `GeForce` RTX 4060** (Ada Lovelace):
///
/// - **3072** CUDA cores (NVML `num_cores()`),
/// - **~2460 MHz** boost graphics clock (NVML `max_clock_info(Graphics)`),
/// - so the anchor perf index is `3072 × 2460 = 7_557_120`.
///
/// Its estimated sustainable ceilings (the "easily does a 2×2 4K30 multiview
/// with headroom" product bar — a 2×2 4K canvas composites to one 3840×2160
/// surface, ingests four ~4K sources, and encodes one canvas per rendition):
///
/// - composite **1200** Mpix/s (≈4.8× a single 4K30 composite's ~249 Mpix/s),
/// - decode **2400** Mpix/s (comfortably four 4K30 NVDEC streams ≈ 995 Mpix/s),
/// - encode **1200** Mpix/s (one 4K30 NVENC canvas ≈ 249 Mpix/s, room for
///   several renditions).
///
/// Scaling is linear in the perf index: a device at `0.2×` the anchor index gets
/// `0.2×` each ceiling. A 2016 Pascal **P2000** (1024 cores × ~1480 MHz =
/// `1_515_520`, ≈ `0.2006×` the anchor) thus scales to a composite ceiling of
/// `1200 × 0.2006 ≈ 240.7` Mpix/s — *below* the 4K30 composite demand of
/// ~248.8 Mpix/s, which is exactly the inv-#1 outcome we require.
pub mod anchor {
    use super::PerfClass;

    /// Anchor shader-core count (RTX 4060).
    pub const CORES: u32 = 3072;
    /// Anchor boost graphics clock, MHz (RTX 4060).
    pub const CLOCK_MHZ: u32 = 2460;
    /// Anchor perf index (`CORES × CLOCK_MHZ`).
    pub const INDEX: f64 = 7_557_120.0;

    /// Anchor sustainable decode ceiling, Mpix/s.
    pub const DECODE_CEILING: f64 = 2400.0;
    /// Anchor sustainable composite ceiling, Mpix/s.
    pub const COMPOSITE_CEILING: f64 = 1200.0;
    /// Anchor sustainable encode ceiling, Mpix/s.
    pub const ENCODE_CEILING: f64 = 1200.0;

    /// The anchor device's own perf class (the RTX 4060 calibration point).
    #[must_use]
    pub fn class() -> PerfClass {
        PerfClass::new(DECODE_CEILING, COMPOSITE_CEILING, ENCODE_CEILING)
    }

    /// Scale the anchor ceilings linearly by `cores × clock_mhz / INDEX`.
    ///
    /// Both inputs are `> 0` (the caller guards that); the product is widened
    /// losslessly to `f64` for the ratio (`u32 × u32 < 2^64`, and well within
    /// `f64`'s 2^53 integer-exact bound for realistic cores/clocks).
    #[must_use]
    pub fn scaled(num_cores: u32, clock_mhz: u32) -> PerfClass {
        let index = f64::from(num_cores) * f64::from(clock_mhz);
        let ratio = index / INDEX;
        PerfClass::new(
            DECODE_CEILING * ratio,
            COMPOSITE_CEILING * ratio,
            ENCODE_CEILING * ratio,
        )
    }
}

/// The conservative, **finite** default for a GPU we recognise by no other
/// signal: a ~1080p60-composite tier.
///
/// 1080p60 composite ≈ `2.0736 Mpix × 60 = 124.4` Mpix/s; the default composite
/// ceiling (130) sits just above that and well **below** a 4K30 composite (~249
/// Mpix/s). The crucial property is finiteness: an unknown GPU is given a real,
/// bounded budget so the gate still cadence-checks it — it is never trusted with
/// an unbounded budget (the inv-#1 guard against the old uniform-100_000 bug).
pub const DEFAULT_PERF_CLASS: PerfClass = PerfClass {
    decode_mpps: DEFAULT_DECODE,
    composite_mpps: DEFAULT_COMPOSITE,
    encode_mpps: DEFAULT_ENCODE,
};

const DEFAULT_DECODE: f64 = 260.0;
const DEFAULT_COMPOSITE: f64 = 130.0;
const DEFAULT_ENCODE: f64 = 130.0;

const CPU_DECODE: f64 = 50.0;
const CPU_COMPOSITE: f64 = 30.0;
const CPU_ENCODE: f64 = 30.0;

/// Per-card name-substring perf classes (case-insensitive lookup).
///
/// The last-resort recognition path: a card we know by its marketing name even
/// when NVML cores×clock and the architecture string are unavailable. Entries are
/// ordered most-specific first so a longer marketing string is preferred (e.g. a
/// future `"RTX 4060 Ti"` would precede `"RTX 4060"` if added). The two the test
/// box runs — the Ada **RTX 4060** and the Pascal **P2000** — are both listed; the
/// P2000's composite ceiling is deliberately below a 4K30 composite (inv #1).
///
/// Matching is by ASCII-case-insensitive substring, so `"NVIDIA GeForce RTX
/// 4060"`, `"rtx 4060"`, and `"RTX 4060"` all hit the same entry.
pub const ARCH_TABLE: &[(&str, PerfClass)] = &[
    // Ada RTX 4060 — the calibration anchor; ceilings match `anchor::class()`.
    (
        "rtx 4060",
        PerfClass {
            decode_mpps: anchor::DECODE_CEILING,
            composite_mpps: anchor::COMPOSITE_CEILING,
            encode_mpps: anchor::ENCODE_CEILING,
        },
    ),
    // Pascal Quadro P2000 — 2016 silicon. Composite ceiling (200) is BELOW a
    // 4K30 composite (~249 Mpix/s): the inv-#1 outcome. Consistent with its
    // cores×clock scaling (≈ 0.2× the anchor → ≈ 240 composite) but rounded
    // conservatively below the 4K30 line.
    (
        "p2000",
        PerfClass {
            decode_mpps: 480.0,
            composite_mpps: 200.0,
            encode_mpps: 240.0,
        },
    ),
];

/// Look a device name up in [`ARCH_TABLE`] by ASCII-case-insensitive substring.
fn name_table_lookup(name: &str) -> Option<PerfClass> {
    let lower = name.to_ascii_lowercase();
    ARCH_TABLE
        .iter()
        .find(|(needle, _)| lower.contains(needle))
        .map(|(_, class)| *class)
}

/// Map an architecture / generation string to a coarse per-generation perf tier.
///
/// Matching is ASCII-case-insensitive substring against the known generation
/// names (the NVML [`DeviceArchitecture`] spellings). Pre-Volta generations
/// (Kepler/Maxwell/Pascal) sit **below** a 4K30 composite; Volta and newer sit
/// above, monotonically increasing with generation. An unrecognised string yields
/// `None` so the caller falls through to the name table / default.
///
/// [`DeviceArchitecture`]: https://docs.rs/nvml-wrapper
fn arch_tier(arch: &str) -> Option<PerfClass> {
    let lower = arch.to_ascii_lowercase();
    ARCH_GEN_TIERS
        .iter()
        .find(|(needle, _)| lower.contains(needle))
        .map(|(_, class)| *class)
}

/// Coarse per-generation perf tiers, ordered weakest -> strongest (composite
/// ceilings are monotonic across the table). Pre-Volta generations
/// (Kepler/Maxwell/Pascal) sit below a 4K30 composite (~249 Mpix/s); Volta and
/// newer sit above. Pascal's composite (200) being below 4K30 is the inv-#1 fact
/// for the P2000's generation.
const ARCH_GEN_TIERS: &[(&str, PerfClass)] = &[
    ("kepler", tier(120.0, 60.0, 60.0)),
    ("maxwell", tier(180.0, 90.0, 90.0)),
    ("pascal", tier(480.0, 200.0, 240.0)),
    ("volta", tier(700.0, 350.0, 350.0)),
    ("turing", tier(900.0, 480.0, 480.0)),
    ("ampere", tier(1100.0, 600.0, 600.0)),
    ("ada", tier(2400.0, 1200.0, 1200.0)),
    ("hopper", tier(3000.0, 1600.0, 1600.0)),
];

/// `const` perf-class constructor for the coarse tier table (the public
/// [`PerfClass::new`] sanitiser is not `const`; tier figures are already finite
/// and non-negative by construction).
const fn tier(decode: f64, composite: f64, encode: f64) -> PerfClass {
    PerfClass {
        decode_mpps: decode,
        composite_mpps: composite,
        encode_mpps: encode,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::float_cmp)]
    use super::*;
    use crate::capability::{Resolution, Stage};
    use crate::cost::TileLoad;
    use crate::planner::{Plan, Planner};
    use multiview_core::time::Rational;

    fn composite_4k30() -> f64 {
        Plan::new(
            Rational::new(30, 1),
            vec![TileLoad::new(Stage::Composite, Resolution::UHD4K)],
        )
        .stage_load_mpps(Stage::Composite)
    }

    #[test]
    fn new_sanitises_non_finite_and_negative_to_zero() {
        let pc = PerfClass::new(f64::INFINITY, -5.0, f64::NAN);
        assert_eq!(pc.decode_mpps_ceiling(), 0.0);
        assert_eq!(pc.composite_mpps_ceiling(), 0.0);
        assert_eq!(pc.encode_mpps_ceiling(), 0.0);
    }

    #[test]
    fn stage_budgets_mirror_the_ceilings() {
        let pc = PerfClass::new(100.0, 200.0, 300.0);
        let b = pc.stage_budgets();
        assert_eq!(b.decode_mpps, 100.0);
        assert_eq!(b.composite_mpps, 200.0);
        assert_eq!(b.encode_mpps, 300.0);
        assert_eq!(b.for_stage(Stage::Composite), 200.0);
        assert_eq!(pc.ceiling_for(Stage::Encode), 300.0);
    }

    #[test]
    fn anchor_scaled_at_index_one_is_the_anchor() {
        let scaled = anchor::scaled(anchor::CORES, anchor::CLOCK_MHZ);
        assert_eq!(scaled, anchor::class());
        // And the anchor's composite clears a 4K30 composite comfortably.
        assert!(scaled.composite_mpps_ceiling() > composite_4k30());
    }

    #[test]
    fn no_usable_signals_yield_the_conservative_default() {
        let pc = PerfClass::for_device(&PerfSignals::default());
        assert_eq!(pc, DEFAULT_PERF_CLASS);
        // Zero cores / zero clock must NOT take the cores x clock path.
        let zeroed = PerfClass::for_device(&PerfSignals::from_nvml(None, Some(0), Some(0), None));
        assert_eq!(zeroed, DEFAULT_PERF_CLASS);
    }

    #[test]
    fn name_table_path_is_substring_and_case_insensitive() {
        assert_eq!(
            name_table_lookup("NVIDIA GeForce RTX 4060"),
            Some(anchor::class())
        );
        assert!(name_table_lookup("quadro p2000").is_some());
        assert!(name_table_lookup("totally unknown").is_none());
    }

    #[test]
    fn signal_priority_cores_clock_beats_a_misleading_name() {
        // A weak P2000 name but a strong cores x clock: the cores x clock signal
        // (priority 1) must win, proving the order.
        let pc = PerfClass::for_device(&PerfSignals::from_nvml(
            Some("Quadro P2000"),
            Some(anchor::CORES),
            Some(anchor::CLOCK_MHZ),
            Some("Pascal"),
        ));
        assert_eq!(pc, anchor::class());
    }

    #[test]
    fn arch_tier_is_monotonic_and_pascal_is_below_4k30() {
        let pascal = arch_tier("Pascal").expect("known gen");
        let ada = arch_tier("Ada").expect("known gen");
        assert!(pascal.composite_mpps_ceiling() < composite_4k30());
        assert!(ada.composite_mpps_ceiling() > pascal.composite_mpps_ceiling());
        assert!(arch_tier("Unobtainium").is_none());
    }

    #[test]
    fn cpu_tier_is_weakest_but_positive() {
        let cpu = PerfClass::cpu();
        assert!(cpu.composite_mpps_ceiling() > 0.0);
        assert!(cpu.composite_mpps_ceiling() < DEFAULT_PERF_CLASS.composite_mpps_ceiling());
    }

    #[test]
    fn p2000_budget_gate_rejects_4k30_composite_but_4060_admits() {
        // The headline inv-#1 proof, end to end through the REAL planner gate.
        let plan = Plan::new(
            Rational::new(30, 1),
            vec![TileLoad::new(Stage::Composite, Resolution::UHD4K)],
        );
        let p2000 =
            Planner::new(PerfClass::for_device(&PerfSignals::from_name("P2000")).stage_budgets())
                .expect("valid budget");
        let rtx = Planner::new(
            PerfClass::for_device(&PerfSignals::from_name("RTX 4060")).stage_budgets(),
        )
        .expect("valid budget");
        assert!(
            p2000.admit(&plan).is_err(),
            "P2000 must fail the 4K30 composite budget gate"
        );
        assert!(
            rtx.admit(&plan).is_ok(),
            "RTX 4060 must pass the 4K30 composite budget gate"
        );
    }
}
