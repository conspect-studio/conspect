//! The pure, deterministic affinity-gated least-loaded placement policy
//! (ADR-0018).
//!
//! [`select_device`] answers the governing question: given a pipeline's
//! demand, the candidate GPUs, the live [`DeviceLoad`] snapshots, and any
//! operator pins, **which single GPU should host the whole
//! `decode -> composite -> encode` island?** It is a pure function — no I/O, no
//! native deps, no clock — so it is fully unit-testable with synthetic loads,
//! holding the same "pure core, feature-gated hardware seam" discipline as the
//! rest of the crate.
//!
//! The four ordered stages (ADR-0018 §2):
//!
//! 1. **Pins win (hard, first).** An operator pin to a stable [`DeviceId`] is
//!    honoured unconditionally if that device is a candidate; an unsatisfiable
//!    pin is surfaced as a [`RejectReason::PinUnsatisfiable`], never silently
//!    relocated.
//! 2. **Hard gates build the candidate set — affinity is a GATE.** A GPU is a
//!    candidate only if it can host the *whole* island: capability
//!    ([`Capability::supports`]), the per-engine Mpix/s cost budget (the
//!    existing [`Planner::admit`], applied per GPU), predicted pool bytes `<=`
//!    free VRAM, and — for NVENC encode placement — the discovered per-system
//!    concurrent-session ceiling (tracked, never hard-coded). A GPU failing any
//!    hard gate is dropped. We never split a pipeline here; a split is the
//!    caller's explicit last-resort path.
//! 3. **Score the survivors** by a dominant-resource (DRF-style) load model,
//!    lower = least-loaded. VRAM carries the highest weight; an **unknown**
//!    vendor term drops out and its weight is redistributed to the known terms
//!    — never fabricated. A GPU whose dominant resource exceeds the configured
//!    headroom ceiling is rejected/degraded rather than chosen.
//! 4. Ties (within an epsilon band) break deterministically: lowest stable
//!    [`DeviceId`] index.
//!
//! See [gpu-placement-engine](../../../docs/research/gpu-placement-engine.md)
//! and ADR-0018.

use crate::capability::{Capability, Resolution, Stage};
use crate::cost::{CostBudget, TileLoad};
use crate::load::{DeviceId, DeviceLoad};
use crate::planner::Planner;
use multiview_core::pixel::PixelFormat;

/// The default headroom ceiling: a GPU whose dominant-resource share would
/// exceed this after admitting the pipeline is rejected (ADR-0018 §2.5,
/// ~0.85 — keep room for fluctuating external load).
pub const DEFAULT_HEADROOM_CEILING: f32 = 0.85;

/// A single scoring weight, validated non-negative and finite.
///
/// Weights are config data (ADR-0018 §2.3 — "not magic constants"); the newtype
/// guarantees a weight can never be NaN/negative and silently corrupt the
/// dominant-resource `max`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ScoreWeight(f32);

impl ScoreWeight {
    /// Construct a weight, clamping a non-finite or negative input to `0.0`.
    #[must_use]
    pub fn new(value: f32) -> Self {
        if value.is_finite() && value >= 0.0 {
            Self(value)
        } else {
            Self(0.0)
        }
    }

    /// The weight value (`>= 0.0`, finite).
    #[must_use]
    pub const fn get(self) -> f32 {
        self.0
    }
}

/// The per-resource weights for the dominant-resource load score (ADR-0018
/// §2.3).
///
/// VRAM is the highest weight (a hard OOM wall, trustworthy on every vendor).
/// Any resource whose live signal is **unknown** for a candidate is dropped
/// from that candidate's score and its weight is redistributed across the
/// known terms — so a blind vendor ranks on what it has, never on a fabricated
/// metric.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LoadWeights {
    /// VRAM used-fraction weight (primary; highest).
    pub vram: ScoreWeight,
    /// Encoder-ASIC busy-fraction weight.
    pub enc_util: ScoreWeight,
    /// Decoder-ASIC busy-fraction weight.
    pub dec_util: ScoreWeight,
    /// NVENC session used-fraction weight (sessions / discovered ceiling).
    pub nvenc_session: ScoreWeight,
    /// Compute / compositor-pressure busy-fraction weight.
    pub compute: ScoreWeight,
}

impl LoadWeights {
    /// The ADR-0018 default weighting: VRAM dominant, the others lighter.
    #[must_use]
    pub fn new_default() -> Self {
        Self {
            vram: ScoreWeight::new(1.0),
            enc_util: ScoreWeight::new(0.6),
            dec_util: ScoreWeight::new(0.6),
            nvenc_session: ScoreWeight::new(0.5),
            compute: ScoreWeight::new(0.4),
        }
    }
}

impl Default for LoadWeights {
    fn default() -> Self {
        Self::new_default()
    }
}

/// Operator pin overrides — the always-wins stage (ADR-0018 §2.1).
///
/// A pin binds a pipeline (or a stage of it) to a stable [`DeviceId`]. For the
/// whole-island placement [`select_device`] performs, the relevant pin is the
/// pipeline pin; it is honoured unconditionally when the device is a candidate,
/// and surfaced as unsatisfiable otherwise (never silently relocated).
#[derive(Debug, Clone, Default)]
pub struct Pins {
    pipeline: Option<DeviceId>,
}

impl Pins {
    /// No pins — the auto policy runs.
    #[must_use]
    pub fn none() -> Self {
        Self::default()
    }

    /// Pin the whole pipeline island to a device.
    #[must_use]
    pub fn pin_pipeline(device: DeviceId) -> Self {
        Self {
            pipeline: Some(device),
        }
    }

    /// The pinned device for the whole pipeline, if any.
    #[must_use]
    pub const fn pipeline(&self) -> Option<&DeviceId> {
        self.pipeline.as_ref()
    }
}

/// Why [`select_device`] declined to place a pipeline on any single GPU.
///
/// A reject is the signal for the caller's no-fit ladder (degrade-to-fit, then
/// the deliberate last-resort split, ADR-0018 §3) — `select_device` itself
/// never splits a pipeline.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum RejectReason {
    /// No GPU could host the whole island (every candidate failed a hard gate).
    NoCandidateFitsWholePipeline,
    /// Every gate-passing GPU exceeded the headroom ceiling — degrade or split.
    AllOverHeadroomCeiling,
    /// An operator pin named a device that is not a viable candidate.
    PinUnsatisfiable {
        /// The device the operator pinned.
        device: DeviceId,
    },
    /// No candidate GPUs were supplied at all.
    NoCandidates,
    /// The pipeline feeds a GPU-resident display sink whose scanout locality
    /// (the connector-owning GPUs, ADR-0044 §3) is not among the viable
    /// candidates — so composite cannot be placed where the framebuffer must
    /// live. Surfaced rather than silently placed off the scanout GPU (which
    /// would force the per-frame GPU→host→GPU copy ADR-0018 forbids).
    SinkLocalityUnsatisfied,
}

/// A chosen placement: the winning device plus its computed load score.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct Selection {
    /// The GPU chosen to host the whole pipeline island.
    pub device: DeviceId,
    /// The composite load score that won (lower = least loaded). For a pin this
    /// is the pinned device's score (or `0.0` if its load is entirely unknown).
    pub score: f32,
    /// Whether this placement was forced by an operator pin (vs the auto
    /// policy).
    pub pinned: bool,
}

/// The outcome of a placement decision: a single-GPU [`Selection`] or a
/// [`RejectReason`] handing off to the caller's no-fit ladder.
pub type SelectOutcome = Result<Selection, RejectReason>;

/// The demand a pipeline imposes, used to gate and score candidate GPUs.
///
/// This is the input shape [`select_device`] reasons over: the per-stage tile
/// loads (already costed in the cost model), the largest tile resolution + its
/// pixel format (for the capability gate), the predicted GPU-pool byte
/// footprint (for the VRAM gate), the output cadence, and — for encode
/// placement — whether the pipeline opens an NVENC encode session.
#[derive(Debug, Clone)]
pub struct PipelineDemand {
    cadence: multiview_core::time::Rational,
    loads: Vec<TileLoad>,
    peak_resolution: Resolution,
    format: PixelFormat,
    predicted_pool_bytes: u64,
    opens_encode_session: bool,
    /// The scanout-locality constraint (ADR-0044 §3): when non-empty, the
    /// pipeline feeds a GPU-resident display sink and its composite **must** be
    /// placed on one of these connector-owning GPUs. Empty = no display sink, no
    /// constraint (every existing call path).
    sink_localities: Vec<DeviceId>,
}

impl PipelineDemand {
    /// Construct a pipeline demand.
    ///
    /// `loads` are the per-stage tile/rendition loads (the same the planner
    /// admits); `peak_resolution`/`format` gate capability; `predicted_pool_bytes`
    /// is the VRAM footprint the per-device pools will allocate;
    /// `opens_encode_session` is `true` when the pipeline's encode stage opens
    /// an NVENC session (so the session-ceiling gate applies).
    #[must_use]
    pub fn new(
        cadence: multiview_core::time::Rational,
        loads: Vec<TileLoad>,
        peak_resolution: Resolution,
        format: PixelFormat,
        predicted_pool_bytes: u64,
        opens_encode_session: bool,
    ) -> Self {
        Self {
            cadence,
            loads,
            peak_resolution,
            format,
            predicted_pool_bytes,
            opens_encode_session,
            sink_localities: Vec::new(),
        }
    }

    /// Constrain this demand to a **scanout locality**: the set of
    /// connector-owning GPUs a local display sink ([ADR-0044](../../../docs/decisions/ADR-0044.md)
    /// §3) requires the composite to be co-located with.
    ///
    /// A display [`crate::scanout::ScanoutInventory::locality_for`] produces this
    /// set from the sink's connectors. With it set, [`select_device`] rejects
    /// every candidate not in the set ([`RejectReason::SinkLocalityUnsatisfied`])
    /// — composite cannot be placed off the GPU the scanout framebuffer must live
    /// on. An empty locality (the default) imposes no constraint. On a single-GPU
    /// display host the set is `{that GPU}`, so the constraint is trivially
    /// satisfied yet still modelled.
    #[must_use]
    pub fn with_sink_locality(mut self, localities: Vec<DeviceId>) -> Self {
        self.sink_localities = localities;
        self
    }

    /// The scanout-locality constraint, if this pipeline feeds a display sink
    /// (empty otherwise).
    #[must_use]
    pub fn sink_localities(&self) -> &[DeviceId] {
        &self.sink_localities
    }

    /// Whether `device` satisfies this demand's scanout-locality constraint: an
    /// empty constraint admits every device; a non-empty one admits only its
    /// members (the connector-owning GPUs).
    #[must_use]
    pub fn satisfies_sink_locality(&self, device: &DeviceId) -> bool {
        self.sink_localities.is_empty() || self.sink_localities.contains(device)
    }

    /// The plan this demand presents to a candidate GPU's [`Planner`].
    fn plan(&self) -> crate::planner::Plan {
        crate::planner::Plan::new(self.cadence, self.loads.clone())
    }

    /// The output cadence in frames/sec (`0.0` for a degenerate rational).
    ///
    /// Used by the deliberate-split cost accounting
    /// ([`crate::split::plan_split`]) to size the per-frame cross-GPU copy.
    #[must_use]
    pub fn cadence_fps(&self) -> f64 {
        if self.cadence.is_valid() {
            self.cadence.as_f64()
        } else {
            0.0
        }
    }

    /// The total load a single [`Stage`] imposes, in megapixels/sec at the
    /// pipeline cadence (the same per-stage sum the [`Planner`] budgets).
    ///
    /// The split decision ([`crate::split::plan_split`]) uses this to model how
    /// much load isolating a bottleneck stage onto its own GPU relieves.
    #[must_use]
    pub fn stage_load_mpps(&self, stage: Stage) -> f64 {
        self.plan().stage_load_mpps(stage)
    }

    /// The largest **decode**-stage tile resolution — the source surface that
    /// would cross the host on a `decode | composite+encode` split. Falls back
    /// to the overall peak resolution when the demand carries no decode tile.
    #[must_use]
    pub fn peak_decode_resolution(&self) -> Resolution {
        self.peak_stage_resolution(Stage::Decode)
            .unwrap_or(self.peak_resolution)
    }

    /// The largest **encode**-stage (rendition) resolution — the composited
    /// canvas that would cross the host on a `decode+composite | encode` split.
    /// Falls back to the overall peak resolution when the demand carries no
    /// encode tile.
    #[must_use]
    pub fn peak_output_resolution(&self) -> Resolution {
        self.peak_stage_resolution(Stage::Encode)
            .unwrap_or(self.peak_resolution)
    }

    /// The largest tile resolution on a given stage, if the demand has any tile
    /// for that stage. `Resolution` orders by pixel area, so `max` is the peak.
    fn peak_stage_resolution(&self, stage: Stage) -> Option<Resolution> {
        self.loads
            .iter()
            .filter(|load| load.stage == stage)
            .map(|load| load.resolution)
            .max()
    }
}

/// A candidate GPU for placement: its identity, the capabilities relevant to
/// each pipeline stage, and its per-engine cost budget.
///
/// The caller assembles candidates from the
/// [`crate::registry::BackendRegistry`] and [`crate::probe`]; `select_device`
/// treats them as opaque whole-island hosts (affinity gate — never split across
/// two).
#[derive(Debug, Clone)]
pub struct GpuCandidate {
    /// Stable identity (the placement + pin key).
    pub device_id: DeviceId,
    /// The capability for each pipeline stage on this GPU. A stage absent from
    /// the map means the GPU cannot host that stage (capability-gate fail).
    pub stage_caps: StageCaps,
    /// The per-engine Mpix/s cost budget for this GPU.
    pub budget: CostBudget,
}

/// Per-stage capability descriptors for a candidate GPU.
#[derive(Debug, Clone)]
pub struct StageCaps {
    decode: Capability,
    composite: Capability,
    encode: Capability,
}

impl StageCaps {
    /// Construct per-stage capabilities for a candidate GPU.
    #[must_use]
    pub fn new(decode: Capability, composite: Capability, encode: Capability) -> Self {
        Self {
            decode,
            composite,
            encode,
        }
    }

    /// The capability for a stage.
    #[must_use]
    pub const fn for_stage(&self, stage: Stage) -> &Capability {
        match stage {
            Stage::Decode => &self.decode,
            Stage::Composite => &self.composite,
            Stage::Encode => &self.encode,
        }
    }
}

/// The placement policy configuration: scoring weights, the headroom ceiling,
/// and the discovered NVENC concurrent-session ceiling.
///
/// Bundling these keeps [`select_device`]'s signature readable and groups the
/// config data ADR-0018 §2.3 treats as data, not magic constants. The NVENC
/// ceiling is the per-system, runtime-**discovered** number (ADR-0017 §1.1) the
/// engine supplies from its own session bookkeeping — `None` until discovered,
/// in which case the session gate stays inert (and the soft session score term
/// is omitted), exactly the "seed conservatively, discover by attempt-and-handle"
/// posture. It is never hard-coded here.
#[derive(Debug, Clone, Copy)]
pub struct PlacementPolicy {
    /// Per-resource scoring weights.
    pub weights: LoadWeights,
    /// Dominant-resource headroom ceiling (`0.0..=1.0`); a GPU whose worst
    /// resource exceeds it after admitting the pipeline is rejected.
    pub headroom_ceiling: f32,
    /// The discovered per-system NVENC concurrent-encode-session ceiling, if
    /// known. `None` leaves the session gate inert.
    pub nvenc_system_ceiling: Option<u32>,
}

impl PlacementPolicy {
    /// The ADR-0018 defaults with no discovered NVENC ceiling yet.
    #[must_use]
    pub fn new_default() -> Self {
        Self {
            weights: LoadWeights::new_default(),
            headroom_ceiling: DEFAULT_HEADROOM_CEILING,
            nvenc_system_ceiling: None,
        }
    }

    /// This policy with a discovered NVENC system ceiling set.
    #[must_use]
    pub const fn with_nvenc_ceiling(mut self, ceiling: u32) -> Self {
        self.nvenc_system_ceiling = Some(ceiling);
        self
    }
}

impl Default for PlacementPolicy {
    fn default() -> Self {
        Self::new_default()
    }
}

/// Choose the single GPU to host a pipeline's whole `decode -> composite ->
/// encode` island (ADR-0018 §2).
///
/// `candidates` are the GPUs that *could* host the pipeline; `demand` is the
/// work; `loads` are the live per-device snapshots (matched to candidates by
/// stable [`DeviceId`]; a candidate with no snapshot scores on the cost model
/// only); `pins` is the operator override; `policy` carries the scoring
/// weights, the headroom ceiling, and the discovered NVENC system ceiling.
///
/// **This function never splits a pipeline across GPUs** — affinity is a hard
/// gate, and a split is the caller's explicit last-resort path.
///
/// # Errors
///
/// Returns a [`RejectReason`] (the `Err` arm of [`SelectOutcome`]) — never a
/// panic — when no single GPU can host the whole pipeline: [`RejectReason::NoCandidates`]
/// (none supplied), [`RejectReason::SinkLocalityUnsatisfied`] (the pipeline
/// feeds a display sink but no candidate owns its connector — ADR-0044 §3),
/// [`RejectReason::PinUnsatisfiable`] (an operator pin names a non-viable
/// device), [`RejectReason::NoCandidateFitsWholePipeline`] (every candidate
/// failed a hard gate), or [`RejectReason::AllOverHeadroomCeiling`] (gate-passing
/// candidates all exceed the headroom ceiling). Each hands off to the caller's
/// no-fit ladder (degrade-to-fit, then the deliberate split).
pub fn select_device(
    candidates: &[GpuCandidate],
    demand: &PipelineDemand,
    loads: &[DeviceLoad],
    pins: &Pins,
    policy: PlacementPolicy,
) -> SelectOutcome {
    let weights = policy.weights;
    let headroom_ceiling = policy.headroom_ceiling;
    if candidates.is_empty() {
        return Err(RejectReason::NoCandidates);
    }

    // Stage 0 — scanout affinity (ADR-0044 §3, a HARD gate, checked first). A
    // display-bound pipeline's composite must live on a connector-owning GPU; a
    // candidate outside the locality set is not a legal host *at any load*.
    // Filtering here (before the load/cost gates) lets the locality cause be
    // surfaced distinctly: if no candidate owns the display, that is the reason,
    // not a generic no-fit.
    let local: Vec<&GpuCandidate> = candidates
        .iter()
        .filter(|candidate| demand.satisfies_sink_locality(&candidate.device_id))
        .collect();
    if local.is_empty() {
        // There were candidates, but none owns the display the sink targets.
        return Err(RejectReason::SinkLocalityUnsatisfied);
    }

    // Stage 2 (computed first so a pin can be checked against it): the set of
    // GPUs that pass every hard gate as a whole-island host. Only the
    // locality-satisfying candidates are considered.
    let viable: Vec<&GpuCandidate> = local
        .iter()
        .copied()
        .filter(|candidate| {
            passes_hard_gates(
                candidate,
                demand,
                find_load(loads, &candidate.device_id),
                policy.nvenc_system_ceiling,
            )
        })
        .collect();

    // Stage 1: pins win, but only onto a viable (whole-pipeline-capable) GPU.
    if let Some(pinned) = pins.pipeline() {
        return match viable.iter().find(|c| &c.device_id == pinned) {
            Some(candidate) => {
                let score = score_candidate(
                    candidate,
                    find_load(loads, &candidate.device_id),
                    weights,
                    policy.nvenc_system_ceiling,
                )
                .map_or(0.0, |scored| scored.score);
                Ok(Selection {
                    device: candidate.device_id.clone(),
                    score,
                    pinned: true,
                })
            }
            None => Err(RejectReason::PinUnsatisfiable {
                device: pinned.clone(),
            }),
        };
    }

    if viable.is_empty() {
        return Err(RejectReason::NoCandidateFitsWholePipeline);
    }

    // Stage 3: score the survivors; reject any over the headroom ceiling.
    let ceiling = if headroom_ceiling.is_finite() {
        headroom_ceiling.clamp(0.0, 1.0)
    } else {
        DEFAULT_HEADROOM_CEILING
    };

    let mut best: Option<Scored> = None;
    let mut blind_fallback: Option<&GpuCandidate> = None;
    for candidate in &viable {
        let scored = score_candidate(
            candidate,
            find_load(loads, &candidate.device_id),
            weights,
            policy.nvenc_system_ceiling,
        );
        let Some(scored) = scored else {
            // Fully blind (no live term): admitted by the cost-model hard gates
            // already. Keep the lowest-index such candidate as the cost-model
            // fallback (ADR-0018 §5: a blind vendor -> cost-model placement,
            // never blocked).
            blind_fallback = Some(lower_index_candidate(blind_fallback, candidate));
            continue;
        };
        // The dominant resource must leave headroom; a GPU whose worst resource
        // is over the ceiling is not chosen (degrade/split instead).
        if scored.dominant_frac > ceiling {
            continue;
        }
        best = Some(match best {
            None => scored,
            Some(current) => pick_better(current, scored),
        });
    }

    if let Some(scored) = best {
        return Ok(Selection {
            device: scored.device.clone(),
            score: scored.score,
            pinned: false,
        });
    }

    // No scored survivor. Prefer a cost-model (blind) placement before
    // rejecting: a blind-but-gate-passing GPU is a valid cost-model home and a
    // blind vendor must never block placement (ADR-0018 §5).
    if let Some(candidate) = blind_fallback {
        return Ok(Selection {
            device: candidate.device_id.clone(),
            score: 0.0,
            pinned: false,
        });
    }

    // Every scored candidate sat over the headroom ceiling (and none was a
    // blind cost-model fallback) — reject so the caller degrades or splits,
    // rather than silently packing a near-full GPU.
    Err(RejectReason::AllOverHeadroomCeiling)
}

/// Keep the lower-enumeration-index of an existing fallback and a new
/// candidate (deterministic blind-fallback selection).
fn lower_index_candidate<'a>(
    existing: Option<&'a GpuCandidate>,
    candidate: &'a GpuCandidate,
) -> &'a GpuCandidate {
    match existing {
        Some(current) if current.device_id.index() <= candidate.device_id.index() => current,
        _ => candidate,
    }
}

/// A candidate's computed score and the dominant-resource share that produced
/// it.
#[derive(Debug, Clone)]
struct Scored {
    device: DeviceId,
    /// The blended composite score (lower = least loaded).
    score: f32,
    /// The single most-saturated resource share (DRF dominant), for the
    /// headroom-ceiling gate.
    dominant_frac: f32,
}

/// Pick the lower-scored of two candidates; ties (within an epsilon band) break
/// to the lower stable device index for determinism (ADR-0018 §2.3).
fn pick_better(current: Scored, other: Scored) -> Scored {
    const EPSILON: f32 = 1e-4;
    if other.score + EPSILON < current.score {
        return other;
    }
    if current.score + EPSILON < other.score {
        return current;
    }
    // Tie: lower enumeration index wins (deterministic).
    if other.device.index() < current.device.index() {
        other
    } else {
        current
    }
}

/// Locate the live load snapshot for a device, if one was supplied.
fn find_load<'a>(loads: &'a [DeviceLoad], device: &DeviceId) -> Option<&'a DeviceLoad> {
    loads.iter().find(|load| &load.device_id == device)
}

/// Whether a candidate passes **every** hard gate as a whole-island host
/// (ADR-0018 §2.2): capability, per-engine cost budget, free VRAM, NVENC
/// session ceiling.
fn passes_hard_gates(
    candidate: &GpuCandidate,
    demand: &PipelineDemand,
    load: Option<&DeviceLoad>,
    nvenc_system_ceiling: Option<u32>,
) -> bool {
    // 1. Capability gate — every stage must support the peak tile.
    for stage in Stage::ALL {
        if !candidate
            .stage_caps
            .for_stage(stage)
            .supports(demand.peak_resolution, demand.format)
        {
            return false;
        }
    }

    // 2. Cost-budget gate — the plan must fit every engine's Mpix/s budget.
    //    Reuse the existing per-GPU Planner::admit (the existing admission
    //    check, applied per candidate). A malformed budget fails the gate
    //    rather than panicking.
    let Ok(planner) = Planner::new(candidate.budget) else {
        return false;
    };
    if planner.admit(&demand.plan()).is_err() {
        return false;
    }

    // 3. VRAM gate — predicted pool bytes must fit in free VRAM, where known.
    //    Unknown free VRAM (blind vendor) does NOT fail the gate: the cost
    //    model + degradation ladder are the fallback (ADR-0018 §5), so we admit
    //    on capability + budget and let scoring/headroom steer.
    if let Some(free) = load.and_then(DeviceLoad::vram_free_bytes) {
        if demand.predicted_pool_bytes > free {
            return false;
        }
    }

    // 4. NVENC session-ceiling gate — only when the pipeline opens an encode
    //    session, the vendor reports its current session count, and the engine
    //    has discovered the per-system ceiling. Multiview tracks its own count
    //    (the snapshot); the ceiling is discovered at runtime, never hard-coded.
    //    With no discovered ceiling the gate stays inert (ADR-0017 §5.6).
    if demand.opens_encode_session {
        if let (Some(used), Some(ceiling)) = (
            load.and_then(|l| l.nvenc_session_count),
            nvenc_system_ceiling,
        ) {
            if used.saturating_add(1) > ceiling {
                return false;
            }
        }
    }

    true
}

/// Score a single candidate by the dominant-resource (DRF) load model with
/// weight redistribution over the known terms (ADR-0018 §2.3).
///
/// Returns `None` only if the candidate has *no* known load term at all (a
/// fully-blind vendor with no snapshot): such a candidate is scored by the cost
/// model elsewhere and is not ranked here. Otherwise returns the blended score
/// plus the single dominant-resource share for the headroom gate.
fn score_candidate(
    candidate: &GpuCandidate,
    load: Option<&DeviceLoad>,
    weights: LoadWeights,
    nvenc_system_ceiling: Option<u32>,
) -> Option<Scored> {
    let load = load?;

    // Collect the (weight, used_fraction) of each KNOWN resource. An unknown
    // term is simply not pushed — its weight is excluded from the
    // normalisation, which is exactly "redistribute weight to known terms"
    // (the share each known term contributes grows because the denominator
    // shrinks). No metric is fabricated.
    let mut terms: Vec<(f32, f32)> = Vec::new();
    if let Some(frac) = load.vram_used_frac() {
        terms.push((weights.vram.get(), clamp_unit(frac)));
    }
    if let Some(frac) = load.enc_util_frac {
        terms.push((weights.enc_util.get(), clamp_unit(frac)));
    }
    if let Some(frac) = load.dec_util_frac {
        terms.push((weights.dec_util.get(), clamp_unit(frac)));
    }
    if let Some(frac) = nvenc_session_frac(load, nvenc_system_ceiling) {
        terms.push((weights.nvenc_session.get(), clamp_unit(frac)));
    }
    if let Some(frac) = load.effective_compute_frac() {
        terms.push((weights.compute.get(), clamp_unit(frac)));
    }

    if terms.is_empty() {
        // Fully blind: no live term to rank on. Let cost-model placement
        // (the caller) handle it; not scored here.
        return None;
    }

    let total_weight: f32 = terms.iter().map(|(w, _)| *w).sum();
    // Weighted blend over the known terms, normalised by the known weight mass
    // (the redistribution). If every weight is zero, fall back to an unweighted
    // mean so a config of all-zero weights still produces a finite score.
    let blended = if total_weight > 0.0 {
        let weighted: f32 = terms.iter().map(|(w, f)| w * f).sum();
        weighted / total_weight
    } else {
        let sum: f32 = terms.iter().map(|(_, f)| f).sum();
        let count = u16::try_from(terms.len()).map_or(1.0_f32, f32::from);
        sum / count
    };

    // The dominant (most-saturated) resource share — the DRF `max`, used for
    // the headroom-ceiling gate independent of the blend.
    let dominant = terms.iter().map(|(_, f)| *f).fold(0.0_f32, f32::max);

    let _ = candidate;
    Some(Scored {
        device: load.device_id.clone(),
        score: blended,
        dominant_frac: dominant,
    })
}

/// The NVENC session used-fraction (`sessions / discovered_ceiling`), if both
/// the count and a ceiling are known. Returns `None` (term drops out) when the
/// ceiling is undiscovered, so a missing ceiling never fabricates a fraction.
fn nvenc_session_frac(load: &DeviceLoad, ceiling: Option<u32>) -> Option<f32> {
    let count = load.nvenc_session_count?;
    let ceiling = ceiling?;
    if ceiling == 0 {
        return None;
    }
    let count_f = u16::try_from(count.min(ceiling)).map_or(1.0_f32, f32::from);
    let ceiling_f = u16::try_from(ceiling).map_or(1.0_f32, f32::from);
    Some((count_f / ceiling_f).clamp(0.0, 1.0))
}

/// Clamp a fraction to `0.0..=1.0` (a transient sample can briefly exceed it).
fn clamp_unit(value: f32) -> f32 {
    if value.is_finite() {
        value.clamp(0.0, 1.0)
    } else {
        // A non-finite sample is treated as fully saturated (conservative): it
        // never lets a bad reading look idle.
        1.0
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::float_cmp)]
    use super::*;
    use crate::load::Vendor;
    use multiview_core::time::Rational;

    fn nv(id: &str, index: u32) -> DeviceId {
        DeviceId::new(Vendor::Nvidia, id, index)
    }

    fn cadence() -> Rational {
        // 30 fps exact.
        Rational::new(30, 1)
    }

    /// A capability that supports HD1080 NV12 on every stage.
    fn cap(stage: Stage) -> Capability {
        Capability::new(
            multiview_core::traits::BackendKind::Cuda,
            stage,
            Resolution::UHD4K,
            vec![PixelFormat::Nv12],
        )
    }

    fn full_caps() -> StageCaps {
        StageCaps::new(
            cap(Stage::Decode),
            cap(Stage::Composite),
            cap(Stage::Encode),
        )
    }

    /// A generous budget that admits a small 1080p pipeline at 30 fps.
    fn generous_budget() -> CostBudget {
        // 1080p @ 30fps ~= 62 Mpix/s per stage; give plenty of room.
        CostBudget::new(1000.0, 1000.0, 1000.0)
    }

    /// A 1080p single-tile pipeline at 30 fps, small VRAM footprint, opens an
    /// encode session.
    fn demand_1080p(pool_bytes: u64) -> PipelineDemand {
        PipelineDemand::new(
            cadence(),
            vec![
                TileLoad::new(Stage::Decode, Resolution::HD1080),
                TileLoad::new(Stage::Composite, Resolution::HD1080),
                TileLoad::new(Stage::Encode, Resolution::HD1080),
            ],
            Resolution::HD1080,
            PixelFormat::Nv12,
            pool_bytes,
            true,
        )
    }

    fn candidate(id: &str, index: u32) -> GpuCandidate {
        GpuCandidate {
            device_id: nv(id, index),
            stage_caps: full_caps(),
            budget: generous_budget(),
        }
    }

    /// A load snapshot with a given VRAM used/total and optional enc/dec util.
    fn load_vram(
        id: &str,
        index: u32,
        used: u64,
        total: u64,
        enc: Option<f32>,
        dec: Option<f32>,
    ) -> DeviceLoad {
        let mut load = DeviceLoad::unknown(nv(id, index));
        load.vram_used_bytes = Some(used);
        load.vram_total_bytes = Some(total);
        load.enc_util_frac = enc;
        load.dec_util_frac = dec;
        load
    }

    #[test]
    fn no_candidates_rejects() {
        let outcome = select_device(
            &[],
            &demand_1080p(1_000_000),
            &[],
            &Pins::none(),
            PlacementPolicy::default(),
        );
        assert_eq!(outcome, Err(RejectReason::NoCandidates));
    }

    #[test]
    fn least_loaded_picks_the_min_score_candidate() {
        // GPU-a is busier (75% VRAM); GPU-b is idle (10% VRAM). b must win.
        let candidates = vec![candidate("GPU-a", 0), candidate("GPU-b", 1)];
        let loads = vec![
            load_vram(
                "GPU-a",
                0,
                9_000_000_000,
                12_000_000_000,
                Some(0.8),
                Some(0.2),
            ),
            load_vram(
                "GPU-b",
                1,
                1_200_000_000,
                12_000_000_000,
                Some(0.1),
                Some(0.1),
            ),
        ];
        let outcome = select_device(
            &candidates,
            &demand_1080p(1_000_000),
            &loads,
            &Pins::none(),
            PlacementPolicy::default(),
        )
        .expect("a fits");
        assert_eq!(outcome.device, nv("GPU-b", 1));
        assert!(!outcome.pinned);
    }

    #[test]
    fn ties_break_to_lower_index_deterministically() {
        // Two identical loads: the lower stable index must win, every time.
        let candidates = vec![candidate("GPU-hi", 3), candidate("GPU-lo", 1)];
        let loads = vec![
            load_vram(
                "GPU-hi",
                3,
                3_000_000_000,
                12_000_000_000,
                Some(0.3),
                Some(0.3),
            ),
            load_vram(
                "GPU-lo",
                1,
                3_000_000_000,
                12_000_000_000,
                Some(0.3),
                Some(0.3),
            ),
        ];
        let outcome = select_device(
            &candidates,
            &demand_1080p(1_000_000),
            &loads,
            &Pins::none(),
            PlacementPolicy::default(),
        )
        .expect("fits");
        assert_eq!(
            outcome.device,
            nv("GPU-lo", 1),
            "lowest index breaks the tie"
        );
    }

    #[test]
    fn pin_overrides_even_a_busier_gpu() {
        // GPU-a is far busier than GPU-b, but the operator pinned GPU-a.
        let candidates = vec![candidate("GPU-a", 0), candidate("GPU-b", 1)];
        let loads = vec![
            load_vram(
                "GPU-a",
                0,
                8_000_000_000,
                12_000_000_000,
                Some(0.7),
                Some(0.3),
            ),
            load_vram(
                "GPU-b",
                1,
                1_000_000_000,
                12_000_000_000,
                Some(0.1),
                Some(0.1),
            ),
        ];
        let outcome = select_device(
            &candidates,
            &demand_1080p(1_000_000),
            &loads,
            &Pins::pin_pipeline(nv("GPU-a", 0)),
            PlacementPolicy::default(),
        )
        .expect("pinned device is viable");
        assert_eq!(outcome.device, nv("GPU-a", 0));
        assert!(outcome.pinned, "pin wins over least-loaded");
    }

    #[test]
    fn unsatisfiable_pin_rejects_rather_than_relocating() {
        // The pinned GPU cannot host the pipeline (its decode caps are too
        // small): the pin must surface unsatisfiable, NOT silently relocate.
        let mut weak = candidate("GPU-weak", 0);
        weak.stage_caps = StageCaps::new(
            Capability::new(
                multiview_core::traits::BackendKind::Cuda,
                Stage::Decode,
                Resolution::HD720, // too small for the 1080p tile
                vec![PixelFormat::Nv12],
            ),
            cap(Stage::Composite),
            cap(Stage::Encode),
        );
        let candidates = vec![weak, candidate("GPU-ok", 1)];
        let loads = vec![
            load_vram(
                "GPU-weak",
                0,
                1_000_000_000,
                12_000_000_000,
                Some(0.1),
                Some(0.1),
            ),
            load_vram(
                "GPU-ok",
                1,
                1_000_000_000,
                12_000_000_000,
                Some(0.1),
                Some(0.1),
            ),
        ];
        let outcome = select_device(
            &candidates,
            &demand_1080p(1_000_000),
            &loads,
            &Pins::pin_pipeline(nv("GPU-weak", 0)),
            PlacementPolicy::default(),
        );
        assert_eq!(
            outcome,
            Err(RejectReason::PinUnsatisfiable {
                device: nv("GPU-weak", 0)
            })
        );
    }

    #[test]
    fn affinity_gate_drops_a_gpu_that_cannot_host_the_whole_pipeline() {
        // GPU-noenc has no encode capability (format mismatch on encode): it
        // cannot host the WHOLE pipeline, so it is dropped from candidates even
        // though it is the least loaded. GPU-ok wins.
        let mut noenc = candidate("GPU-noenc", 0);
        noenc.stage_caps = StageCaps::new(
            cap(Stage::Decode),
            cap(Stage::Composite),
            Capability::new(
                multiview_core::traits::BackendKind::Cuda,
                Stage::Encode,
                Resolution::UHD4K,
                vec![PixelFormat::Rgba], // does NOT accept NV12 -> encode gate fails
            ),
        );
        let candidates = vec![noenc, candidate("GPU-ok", 1)];
        let loads = vec![
            // noenc is the idlest, but it cannot encode NV12.
            load_vram(
                "GPU-noenc",
                0,
                100_000_000,
                12_000_000_000,
                Some(0.0),
                Some(0.0),
            ),
            load_vram(
                "GPU-ok",
                1,
                4_000_000_000,
                12_000_000_000,
                Some(0.4),
                Some(0.4),
            ),
        ];
        let outcome = select_device(
            &candidates,
            &demand_1080p(1_000_000),
            &loads,
            &Pins::none(),
            PlacementPolicy::default(),
        )
        .expect("GPU-ok fits the whole pipeline");
        assert_eq!(
            outcome.device,
            nv("GPU-ok", 1),
            "affinity gate dropped the idlest GPU because it cannot host the whole island"
        );
    }

    #[test]
    fn vram_gate_rejects_a_gpu_without_room_for_the_pool() {
        // GPU-tight has only 500 MB free; the pipeline pool needs 2 GB. It is
        // dropped; GPU-roomy (with room) wins even though it is busier.
        let candidates = vec![candidate("GPU-tight", 0), candidate("GPU-roomy", 1)];
        let loads = vec![
            // 11.5 GB of 12 GB used -> 500 MB free.
            load_vram(
                "GPU-tight",
                0,
                11_500_000_000,
                12_000_000_000,
                Some(0.1),
                Some(0.1),
            ),
            // 4 GB used -> 8 GB free.
            load_vram(
                "GPU-roomy",
                1,
                4_000_000_000,
                12_000_000_000,
                Some(0.4),
                Some(0.4),
            ),
        ];
        let outcome = select_device(
            &candidates,
            &demand_1080p(2_000_000_000), // needs 2 GB
            &loads,
            &Pins::none(),
            PlacementPolicy::default(),
        )
        .expect("GPU-roomy has room");
        assert_eq!(outcome.device, nv("GPU-roomy", 1));
    }

    #[test]
    fn nvenc_session_ceiling_rejects_a_full_encoder_gpu() {
        // GPU-full is at the discovered NVENC ceiling (8/8): admitting one more
        // session would exceed it, so it is gated out. GPU-spare (1/8) wins —
        // even though it is otherwise the busier-VRAM... here equal VRAM, so the
        // session ceiling is the deciding hard gate. The ceiling is the
        // discovered per-system number supplied via the policy, never hard-coded.
        let candidates = vec![candidate("GPU-full", 0), candidate("GPU-spare", 1)];

        let mut full = load_vram(
            "GPU-full",
            0,
            1_000_000_000,
            12_000_000_000,
            Some(0.5),
            Some(0.1),
        );
        full.nvenc_session_count = Some(8);
        let mut spare = load_vram(
            "GPU-spare",
            1,
            1_000_000_000,
            12_000_000_000,
            Some(0.5),
            Some(0.1),
        );
        spare.nvenc_session_count = Some(1);

        let policy = PlacementPolicy::default().with_nvenc_ceiling(8);
        let outcome = select_device(
            &candidates,
            &demand_1080p(1_000_000), // opens an encode session
            &[full, spare],
            &Pins::none(),
            policy,
        )
        .expect("GPU-spare has a free session");
        assert_eq!(
            outcome.device,
            nv("GPU-spare", 1),
            "the full-encoder GPU is gated out by the discovered session ceiling"
        );
    }

    #[test]
    fn nvenc_ceiling_inert_when_undiscovered() {
        // With NO discovered ceiling, the session gate must stay inert: a GPU
        // reporting many sessions is NOT rejected (we cannot know it is full),
        // so the least-loaded-by-VRAM choice stands. This proves the ceiling is
        // never fabricated/hard-coded.
        let candidates = vec![candidate("GPU-busy-enc", 0)];
        let mut load = load_vram(
            "GPU-busy-enc",
            0,
            2_000_000_000,
            12_000_000_000,
            Some(0.3),
            Some(0.3),
        );
        load.nvenc_session_count = Some(99);
        let outcome = select_device(
            &candidates,
            &demand_1080p(1_000_000),
            &[load],
            &Pins::none(),
            PlacementPolicy::default(), // nvenc_system_ceiling = None
        )
        .expect("inert session gate admits");
        assert_eq!(outcome.device, nv("GPU-busy-enc", 0));
    }

    #[test]
    fn no_fit_rejects_for_the_split_ladder() {
        // The only candidate cannot host the pipeline (encode caps too small):
        // select_device must REJECT (handing off to the no-fit/split ladder),
        // never split here.
        let mut weak = candidate("GPU-weak", 0);
        weak.stage_caps = StageCaps::new(
            cap(Stage::Decode),
            cap(Stage::Composite),
            Capability::new(
                multiview_core::traits::BackendKind::Cuda,
                Stage::Encode,
                Resolution::HD720, // 1080p won't fit
                vec![PixelFormat::Nv12],
            ),
        );
        let loads = vec![load_vram(
            "GPU-weak",
            0,
            1_000_000_000,
            12_000_000_000,
            Some(0.1),
            Some(0.1),
        )];
        let outcome = select_device(
            &[weak],
            &demand_1080p(1_000_000),
            &loads,
            &Pins::none(),
            PlacementPolicy::default(),
        );
        assert_eq!(outcome, Err(RejectReason::NoCandidateFitsWholePipeline));
    }

    #[test]
    fn over_headroom_ceiling_rejects_rather_than_packing() {
        // The only viable GPU is 95% VRAM-used — over the 0.85 ceiling. Even
        // though it passes the hard gates, scoring must reject it (degrade or
        // split), never pack a near-full GPU.
        let candidates = vec![candidate("GPU-hot", 0)];
        let loads = vec![load_vram(
            "GPU-hot",
            0,
            11_400_000_000,
            12_000_000_000, // 95% used
            Some(0.5),
            Some(0.5),
        )];
        let outcome = select_device(
            &candidates,
            &demand_1080p(1_000_000),
            &loads,
            &Pins::none(),
            PlacementPolicy::default(),
        );
        assert_eq!(outcome, Err(RejectReason::AllOverHeadroomCeiling));
    }

    #[test]
    fn unknown_vendor_term_redistributes_weight_without_fabrication() {
        // GPU-blind exposes ONLY VRAM (enc/dec util unknown); GPU-full exposes
        // all terms. With identical VRAM, the blind GPU's score must rest purely
        // on its known VRAM term (weight redistributed), and a HIGH unknown's
        // enc/dec on the other GPU must make the other GPU score worse — proving
        // the unknown term dropped out rather than being read as 0.0 (which
        // would have made the blind GPU look artificially idle/equal).
        let candidates = vec![candidate("GPU-blind", 0), candidate("GPU-busy", 1)];
        // Same VRAM (50%). GPU-blind: enc/dec unknown. GPU-busy: enc/dec 0.9.
        let blind = load_vram("GPU-blind", 0, 6_000_000_000, 12_000_000_000, None, None);
        let busy = load_vram(
            "GPU-busy",
            1,
            6_000_000_000,
            12_000_000_000,
            Some(0.9),
            Some(0.9),
        );

        let blind_score =
            score_candidate(&candidates[0], Some(&blind), LoadWeights::default(), None)
                .expect("VRAM known")
                .score;
        let busy_score = score_candidate(&candidates[1], Some(&busy), LoadWeights::default(), None)
            .expect("all known")
            .score;

        // Blind GPU ranks on VRAM alone (50%); busy GPU's high enc/dec pulls its
        // blended score above 50%. So the blind GPU wins — the unknown terms
        // were dropped, not fabricated as 0.0.
        assert!(
            blind_score < busy_score,
            "blind={blind_score} busy={busy_score}: unknown terms must drop out, not read as 0.0"
        );
        assert!(
            (blind_score - 0.5).abs() < 1e-4,
            "blind score is pure VRAM (0.5), got {blind_score}"
        );

        let outcome = select_device(
            &candidates,
            &demand_1080p(1_000_000),
            &[blind, busy],
            &Pins::none(),
            PlacementPolicy::default(),
        )
        .expect("both viable");
        assert_eq!(outcome.device, nv("GPU-blind", 0));
    }

    #[test]
    fn fully_blind_candidate_is_not_scored_here() {
        // A candidate with NO known load term at all is not ranked by the score
        // model (it is left to cost-model placement). With one fully-blind and
        // one scored candidate, the scored one is chosen.
        let candidates = vec![candidate("GPU-dark", 0), candidate("GPU-seen", 1)];
        let dark = DeviceLoad::unknown(nv("GPU-dark", 0)); // all None
        let seen = load_vram(
            "GPU-seen",
            1,
            2_000_000_000,
            12_000_000_000,
            Some(0.2),
            Some(0.2),
        );
        let outcome = select_device(
            &candidates,
            &demand_1080p(1_000_000),
            &[dark, seen],
            &Pins::none(),
            PlacementPolicy::default(),
        )
        .expect("the seen GPU is scored");
        assert_eq!(outcome.device, nv("GPU-seen", 1));
    }

    // ----- DEV-B2: sink-locality constraint (ADR-0044 §3 / display-out.md §3) -----

    #[test]
    fn sink_locality_pins_composite_to_the_display_gpu() {
        // GPU-display owns the connector; GPU-idle is idler but owns no connector.
        // A display sink declares locality on GPU-display, so the composite MUST
        // land there even though GPU-idle would otherwise win on load.
        let candidates = vec![candidate("GPU-display", 0), candidate("GPU-idle", 1)];
        let loads = vec![
            // GPU-display is busier (60%)...
            load_vram(
                "GPU-display",
                0,
                7_200_000_000,
                12_000_000_000,
                Some(0.5),
                Some(0.5),
            ),
            // ...GPU-idle is idle (10%) but cannot scan out the display.
            load_vram(
                "GPU-idle",
                1,
                1_200_000_000,
                12_000_000_000,
                Some(0.1),
                Some(0.1),
            ),
        ];
        let demand = demand_1080p(1_000_000).with_sink_locality(vec![nv("GPU-display", 0)]);
        let outcome = select_device(
            &candidates,
            &demand,
            &loads,
            &Pins::none(),
            PlacementPolicy::default(),
        )
        .expect("the display GPU hosts the locked composite");
        assert_eq!(
            outcome.device,
            nv("GPU-display", 0),
            "sink locality pins composite to the connector-owning GPU"
        );
    }

    #[test]
    fn sink_locality_rejects_when_the_display_gpu_is_not_a_candidate() {
        // The locality names a GPU that is not in the candidate set at all (it was
        // gated out / never offered). select_device must REJECT with the dedicated
        // SinkLocalityUnsatisfied reason — never silently place composite on a GPU
        // that owns no connector (which would force the GPU->host->GPU copy).
        let candidates = vec![candidate("GPU-render-only", 0)];
        let loads = vec![load_vram(
            "GPU-render-only",
            0,
            1_000_000_000,
            12_000_000_000,
            Some(0.1),
            Some(0.1),
        )];
        let demand = demand_1080p(1_000_000).with_sink_locality(vec![nv("GPU-has-the-display", 9)]);
        let outcome = select_device(
            &candidates,
            &demand,
            &loads,
            &Pins::none(),
            PlacementPolicy::default(),
        );
        assert_eq!(outcome, Err(RejectReason::SinkLocalityUnsatisfied));
    }

    #[test]
    fn empty_sink_locality_imposes_no_constraint() {
        // The default (no display sink) carries an empty locality set, which must
        // impose NO constraint: the idlest GPU wins exactly as before.
        let candidates = vec![candidate("GPU-a", 0), candidate("GPU-b", 1)];
        let loads = vec![
            load_vram(
                "GPU-a",
                0,
                9_000_000_000,
                12_000_000_000,
                Some(0.8),
                Some(0.2),
            ),
            load_vram(
                "GPU-b",
                1,
                1_200_000_000,
                12_000_000_000,
                Some(0.1),
                Some(0.1),
            ),
        ];
        // demand_1080p carries an empty locality by default.
        let outcome = select_device(
            &candidates,
            &demand_1080p(1_000_000),
            &loads,
            &Pins::none(),
            PlacementPolicy::default(),
        )
        .expect("no locality -> least-loaded wins");
        assert_eq!(outcome.device, nv("GPU-b", 1));
    }

    #[test]
    fn sink_locality_with_multiple_owners_admits_either() {
        // Two connectors on two GPUs -> locality = {GPU-x, GPU-y}. Composite may
        // legally live on EITHER (a same-GPU scanout); the idler of the two wins,
        // and a third render-only GPU is rejected by the locality gate.
        let candidates = vec![
            candidate("GPU-x", 0),
            candidate("GPU-y", 1),
            candidate("GPU-render", 2),
        ];
        let loads = vec![
            load_vram(
                "GPU-x",
                0,
                8_000_000_000,
                12_000_000_000,
                Some(0.6),
                Some(0.6),
            ),
            // GPU-y is the idler of the two display GPUs.
            load_vram(
                "GPU-y",
                1,
                1_500_000_000,
                12_000_000_000,
                Some(0.1),
                Some(0.1),
            ),
            // GPU-render is the idlest overall but owns no connector -> gated out.
            load_vram(
                "GPU-render",
                2,
                300_000_000,
                12_000_000_000,
                Some(0.0),
                Some(0.0),
            ),
        ];
        let demand =
            demand_1080p(1_000_000).with_sink_locality(vec![nv("GPU-x", 0), nv("GPU-y", 1)]);
        let outcome = select_device(
            &candidates,
            &demand,
            &loads,
            &Pins::none(),
            PlacementPolicy::default(),
        )
        .expect("either display GPU is admissible");
        assert_eq!(
            outcome.device,
            nv("GPU-y", 1),
            "the idler display-owning GPU wins; the render-only GPU is gated out"
        );
    }
}
