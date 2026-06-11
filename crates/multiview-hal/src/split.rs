//! The deliberate last-resort multi-GPU **split** decision (ADR-0018 §20).
//!
//! [`select_device`](crate::select::select_device) keeps a pipeline's whole
//! `decode -> composite -> encode` island on one GPU — affinity is a hard gate,
//! never a score term. Only when **no single GPU can host the whole island**
//! (every candidate failed a hard gate) does the caller's no-fit ladder reach
//! here, and only then does [`plan_split`] consider cutting the island across
//! two GPUs.
//!
//! This is the *deliberate, cost-accounted* split, never a routine tradeoff:
//! consumer `GeForce` has no peer-to-peer, so every cut pays a real
//! `GPU -> host -> GPU` copy each frame (ADR-0018 §rationale). So [`plan_split`]
//!
//! - **never fragments `composite`** — the two legal cut points keep
//!   `composite` whole;
//! - cuts at the **cheapest point that isolates the bottleneck** —
//!   [`CutPoint::DecodeThenRest`] (`decode | composite+encode`, copy the decoded
//!   source surface) when the decode engine is the bottleneck, or
//!   [`CutPoint::RestThenEncode`] (`decode+composite | encode`, copy the
//!   composited canvas) when the NVENC session ceiling is the bottleneck;
//! - **accounts the host round-trip explicitly** as a [`CrossGpuCopy`] cost;
//! - is **gated by a minimum-gain threshold** so a split is taken only when it
//!   is clearly justified — the loop prefers degrade-to-fit and holding over
//!   churning a live pipeline across the `PCIe` bus.
//!
//! It is a pure function over injected demand + load — no I/O, no native deps,
//! no clock — fully unit-testable, and on a single-GPU host it does nothing.
//!
//! See [gpu-placement-engine](../../../docs/research/gpu-placement-engine.md)
//! and ADR-0018 §20.

use crate::capability::{Resolution, Stage};
use crate::load::{DeviceId, DeviceLoad};
use crate::select::PipelineDemand;

/// Where the island is cut when a deliberate split is unavoidable (ADR-0018
/// §20). `composite` is **never** split, so there are exactly two legal cuts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum CutPoint {
    /// `decode | composite+encode` — decode runs on one GPU, composite+encode on
    /// the other. The **decoded source surface** crosses the host. Chosen when
    /// the decode engine is the bottleneck.
    DecodeThenRest,
    /// `decode+composite | encode` — decode+composite run on one GPU, encode on
    /// the other. The **composited canvas** crosses the host. Chosen when the
    /// encode engine (NVENC session ceiling) is the bottleneck.
    RestThenEncode,
}

impl CutPoint {
    /// The single pipeline stage isolated onto its own GPU by this cut (the
    /// bottleneck stage the cut relieves).
    #[must_use]
    pub const fn isolated_stage(self) -> Stage {
        match self {
            CutPoint::DecodeThenRest => Stage::Decode,
            CutPoint::RestThenEncode => Stage::Encode,
        }
    }
}

/// The explicit, modelled cost of the per-frame `GPU -> host -> GPU` copy a
/// split pays (ADR-0018 §rationale — no `GeForce` P2P).
///
/// The copy is the surface crossed at the cut point, at the output cadence,
/// expressed in megapixels/sec on **each** card it touches (the host round-trip
/// reads off the source GPU and writes onto the destination GPU). Surfacing it
/// keeps the split honest: the loop sees the price before it commits.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CrossGpuCopy {
    /// The surface crossed each frame at the cut point.
    pub surface: Resolution,
    /// The copy throughput on each card, in megapixels/sec (surface megapixels x
    /// cadence). Both the source and the destination GPU pay this.
    pub mpps_per_card: f64,
}

/// A deliberate, cost-accounted multi-GPU split (ADR-0018 §20).
///
/// Produced by [`plan_split`] only as a last resort: it names the cut point,
/// which device hosts each side, and the explicit cross-GPU copy cost. The
/// `composite` stage is always kept whole on one side.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct SplitPlan {
    /// Where the island is cut.
    pub cut: CutPoint,
    /// The GPU hosting the **isolated** stage (decode for `DecodeThenRest`,
    /// encode for `RestThenEncode`).
    pub isolated_device: DeviceId,
    /// The GPU hosting the remaining stages (which always include `composite`).
    pub remainder_device: DeviceId,
    /// The explicit per-frame host round-trip cost the split incurs.
    pub copy: CrossGpuCopy,
}

/// Why [`plan_split`] declined to propose a split — handing back to the caller's
/// no-fit ladder (degrade further, or hold last-good rather than churn).
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum SplitReject {
    /// Fewer than two distinct candidate devices were supplied — a split needs
    /// two GPUs, so there is nothing to cut across (the single-GPU-host case).
    NeedTwoDevices,
    /// No bottleneck stage could be identified from the demand + loads, so there
    /// is no cut that would relieve anything. The caller degrades instead.
    NoIdentifiableBottleneck,
    /// A split is possible but its modelled gain does not clear the minimum-gain
    /// threshold — splitting would cost more (the host round-trip) than it
    /// relieves, so the loop must not churn the live pipeline across the bus.
    BelowMinGain {
        /// The modelled gain (relieved bottleneck Mpix/s minus the copy cost).
        gain_mpps: f64,
        /// The threshold the gain had to clear.
        min_gain_mpps: f64,
    },
}

/// The split-decision tuning (ADR-0018 §20 / §consequences — policy as data).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SplitPolicy {
    /// The minimum modelled net gain (relieved bottleneck load minus the
    /// cross-GPU copy cost), in megapixels/sec, a split must clear before it is
    /// proposed. A conservative positive default biases the loop toward
    /// degrade-to-fit / holding over splitting a live pipeline.
    pub min_gain_mpps: f64,
}

impl SplitPolicy {
    /// The ADR-0018 conservative default: a split must net at least one full
    /// 1080p-at-30 stage's worth of relief (~62 Mpix/s) over its copy cost
    /// before it is taken — so a marginal split never churns the pipeline.
    #[must_use]
    pub const fn new_default() -> Self {
        Self {
            min_gain_mpps: 62.0,
        }
    }
}

impl Default for SplitPolicy {
    fn default() -> Self {
        Self::new_default()
    }
}

/// The outcome of a split decision: a deliberate [`SplitPlan`] or a
/// [`SplitReject`] sending the caller back to its degrade/hold ladder.
pub type SplitOutcome = Result<SplitPlan, SplitReject>;

/// Decide a deliberate last-resort multi-GPU split for a pipeline that **no
/// single GPU could host** (ADR-0018 §20).
///
/// This is reached only after
/// [`select_device`](crate::select::select_device) rejected and the degradation
/// ladder has already shrunk the footprint as far as it can. It is a pure
/// function over the same injected demand + load snapshots: it identifies the
/// bottleneck stage, picks the cheapest cut that isolates it (keeping
/// `composite` whole), assigns the isolated stage and the remainder to two
/// distinct devices, costs the host round-trip explicitly, and proposes the
/// split **only** when its modelled net gain clears the policy threshold.
///
/// `devices` are the distinct candidate GPUs (by stable [`DeviceId`]) the split
/// may use; `loads` are their live snapshots (matched by id; a device with no
/// snapshot still hosts a side, costed by the model). The bottleneck is taken
/// from `bottleneck` — the stage [`select_device`]'s hard gates flagged as the
/// no-fit cause (decode budget vs encode session ceiling); `None` asks
/// [`plan_split`] to infer it from the loads.
///
/// # Errors
///
/// Returns a [`SplitReject`] — never a panic — when a split is not justified:
/// [`SplitReject::NeedTwoDevices`] (a split needs two GPUs),
/// [`SplitReject::NoIdentifiableBottleneck`] (nothing to relieve), or
/// [`SplitReject::BelowMinGain`] (the split would cost more than it relieves).
pub fn plan_split(
    demand: &PipelineDemand,
    devices: &[DeviceId],
    loads: &[DeviceLoad],
    bottleneck: Option<Stage>,
    policy: SplitPolicy,
) -> SplitOutcome {
    // A split needs two *distinct* GPUs. On a single-GPU host (or with one
    // candidate) there is nothing to cut across — affinity is absolute.
    let mut distinct: Vec<&DeviceId> = Vec::new();
    for device in devices {
        if !distinct.contains(&device) {
            distinct.push(device);
        }
    }
    let (Some(first), Some(second)) = (distinct.first(), distinct.get(1)) else {
        return Err(SplitReject::NeedTwoDevices);
    };

    // Identify the bottleneck stage. Prefer the caller's hint (what the hard
    // gates flagged); otherwise infer it from the loads (the most-saturated of
    // the splittable engines). `composite` is never the cut target.
    let Some(stage) = bottleneck
        .filter(|stage| is_splittable_bottleneck(*stage))
        .or_else(|| infer_bottleneck(loads))
    else {
        return Err(SplitReject::NoIdentifiableBottleneck);
    };

    // Choose the cut that isolates the bottleneck onto its own GPU, and the
    // surface that must cross the host at that cut. `composite` stays whole on
    // the remainder side either way.
    let (cut, surface) = match stage {
        // Decode is the bottleneck: isolate decode; copy the decoded source
        // surface (the peak input tile) to the composite+encode GPU.
        Stage::Decode => (CutPoint::DecodeThenRest, demand.peak_decode_resolution()),
        // Encode (NVENC session ceiling) is the bottleneck: isolate encode; copy
        // the composited canvas (the peak output resolution) to the encode GPU.
        Stage::Encode => (CutPoint::RestThenEncode, demand.peak_output_resolution()),
        // `composite` is never split (filtered above), but keep the match total.
        Stage::Composite => return Err(SplitReject::NoIdentifiableBottleneck),
    };

    let cadence_fps = {
        let fps = demand.cadence_fps();
        if fps.is_finite() && fps > 0.0 {
            fps
        } else {
            0.0
        }
    };
    let mpps_per_card = surface.megapixels() * cadence_fps;
    let copy = CrossGpuCopy {
        surface,
        mpps_per_card,
    };

    // Model the net gain: isolating the bottleneck stage relieves that stage's
    // full load off the contended GPU; against it we charge the per-card copy
    // cost the split introduces. A split is justified only when the relief
    // clearly exceeds the price, so the loop never churns a live pipeline for a
    // marginal improvement.
    let relieved = demand.stage_load_mpps(stage);
    let gain_mpps = relieved - mpps_per_card;
    if gain_mpps < policy.min_gain_mpps {
        return Err(SplitReject::BelowMinGain {
            gain_mpps,
            min_gain_mpps: policy.min_gain_mpps,
        });
    }

    // Assign devices deterministically: the isolated bottleneck stage goes to
    // the lower-index device, the remainder (always including `composite`) to
    // the other. This is a stable, reproducible plan, not a per-frame choice.
    Ok(SplitPlan {
        cut,
        isolated_device: (*first).clone(),
        remainder_device: (*second).clone(),
        copy,
    })
}

/// Whether a caller-supplied bottleneck hint names a stage a split may isolate.
/// `composite` is never split, so a composite hint is not a usable cut target.
fn is_splittable_bottleneck(stage: Stage) -> bool {
    matches!(stage, Stage::Decode | Stage::Encode)
}

/// Infer the splittable bottleneck stage from the live loads when the caller
/// supplied no hint: pick whichever of decode / encode looks most saturated
/// across the candidate devices. Returns `None` when neither engine reports a
/// usable signal (a fully-blind probe — defer to degradation, never guess).
fn infer_bottleneck(loads: &[DeviceLoad]) -> Option<Stage> {
    let mut worst_decode: Option<f32> = None;
    let mut worst_encode: Option<f32> = None;
    for load in loads {
        if let Some(frac) = load.dec_util_frac {
            worst_decode = Some(max_opt(worst_decode, frac));
        }
        if let Some(frac) = load.enc_util_frac {
            worst_encode = Some(max_opt(worst_encode, frac));
        }
    }
    match (worst_decode, worst_encode) {
        (Some(dec), Some(enc)) => {
            if enc >= dec {
                Some(Stage::Encode)
            } else {
                Some(Stage::Decode)
            }
        }
        (Some(_), None) => Some(Stage::Decode),
        (None, Some(_)) => Some(Stage::Encode),
        (None, None) => None,
    }
}

/// The larger of an optional running max and a new sample.
fn max_opt(current: Option<f32>, value: f32) -> f32 {
    match current {
        Some(seen) => seen.max(value),
        None => value,
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::indexing_slicing,
        clippy::float_cmp
    )]
    use super::*;
    use crate::cost::TileLoad;
    use crate::load::Vendor;
    use multiview_core::pixel::PixelFormat;
    use multiview_core::time::Rational;

    fn nv(id: &str, index: u32) -> DeviceId {
        DeviceId::new(Vendor::Nvidia, id, index)
    }

    fn cadence() -> Rational {
        Rational::new(30, 1)
    }

    /// A 4K-input, 4K-output pipeline at 30 fps with a heavy decode stage and a
    /// modest encode stage — so isolating decode relieves a lot.
    fn demand_decode_heavy() -> PipelineDemand {
        PipelineDemand::new(
            cadence(),
            vec![
                // Four 4K decode tiles -> a big decode load.
                TileLoad::new(Stage::Decode, Resolution::UHD4K),
                TileLoad::new(Stage::Decode, Resolution::UHD4K),
                TileLoad::new(Stage::Decode, Resolution::UHD4K),
                TileLoad::new(Stage::Decode, Resolution::UHD4K),
                TileLoad::new(Stage::Composite, Resolution::HD1080),
                TileLoad::new(Stage::Encode, Resolution::HD1080),
            ],
            Resolution::UHD4K,
            PixelFormat::Nv12,
            1_000_000,
            true,
        )
    }

    /// A pipeline whose encode is the heavy stage (many 4K renditions), decode
    /// light — isolating encode relieves a lot; the copied surface is the 1080p
    /// canvas.
    fn demand_encode_heavy() -> PipelineDemand {
        PipelineDemand::new(
            cadence(),
            vec![
                TileLoad::new(Stage::Decode, Resolution::HD720),
                TileLoad::new(Stage::Composite, Resolution::HD1080),
                TileLoad::new(Stage::Encode, Resolution::UHD4K),
                TileLoad::new(Stage::Encode, Resolution::UHD4K),
            ],
            Resolution::HD720,
            PixelFormat::Nv12,
            1_000_000,
            true,
        )
    }

    #[test]
    fn single_device_never_splits() {
        // Affinity is absolute: with only one candidate GPU there is nothing to
        // cut across, so plan_split refuses rather than fragmenting.
        let outcome = plan_split(
            &demand_decode_heavy(),
            &[nv("GPU-0", 0)],
            &[],
            Some(Stage::Decode),
            SplitPolicy::default(),
        );
        assert_eq!(outcome, Err(SplitReject::NeedTwoDevices));
    }

    #[test]
    fn duplicate_device_ids_are_not_two_devices() {
        // Two entries for the SAME stable DeviceId is still one physical GPU:
        // not a split target.
        let outcome = plan_split(
            &demand_decode_heavy(),
            &[nv("GPU-0", 0), nv("GPU-0", 0)],
            &[],
            Some(Stage::Decode),
            SplitPolicy::default(),
        );
        assert_eq!(outcome, Err(SplitReject::NeedTwoDevices));
    }

    #[test]
    fn decode_bottleneck_cuts_decode_off_and_copies_the_source_surface() {
        // Decode is the flagged bottleneck: the cut isolates decode
        // (decode | composite+encode) and the copied surface is the peak input
        // (4K), keeping composite whole on the remainder side.
        let outcome = plan_split(
            &demand_decode_heavy(),
            &[nv("GPU-0", 0), nv("GPU-1", 1)],
            &[],
            Some(Stage::Decode),
            SplitPolicy::default(),
        )
        .expect("a heavy decode split clears the gain gate");
        assert_eq!(outcome.cut, CutPoint::DecodeThenRest);
        assert_eq!(outcome.cut.isolated_stage(), Stage::Decode);
        assert_eq!(outcome.copy.surface, Resolution::UHD4K);
        assert_eq!(outcome.isolated_device, nv("GPU-0", 0));
        assert_eq!(outcome.remainder_device, nv("GPU-1", 1));
        // The copy cost is explicitly accounted: 4K megapixels x 30 fps.
        let expected = Resolution::UHD4K.megapixels() * 30.0;
        assert!((outcome.copy.mpps_per_card - expected).abs() < 1e-6);
    }

    #[test]
    fn encode_bottleneck_cuts_encode_off_and_copies_the_canvas() {
        // Encode (NVENC session ceiling) is the bottleneck: the cut isolates
        // encode (decode+composite | encode) and the copied surface is the
        // composited canvas (the peak output resolution).
        let outcome = plan_split(
            &demand_encode_heavy(),
            &[nv("GPU-0", 0), nv("GPU-1", 1)],
            &[],
            Some(Stage::Encode),
            SplitPolicy::default(),
        )
        .expect("a heavy encode split clears the gain gate");
        assert_eq!(outcome.cut, CutPoint::RestThenEncode);
        assert_eq!(outcome.cut.isolated_stage(), Stage::Encode);
        // The canvas is the peak OUTPUT resolution, not the (720p) input.
        assert_eq!(outcome.copy.surface, Resolution::UHD4K);
    }

    #[test]
    fn composite_is_never_a_cut_target() {
        // Even if the caller hints composite, a split never fragments composite:
        // with no other usable bottleneck signal it must reject, not cut
        // composite.
        let outcome = plan_split(
            &demand_decode_heavy(),
            &[nv("GPU-0", 0), nv("GPU-1", 1)],
            &[], // no loads -> nothing to infer
            Some(Stage::Composite),
            SplitPolicy::default(),
        );
        assert_eq!(outcome, Err(SplitReject::NoIdentifiableBottleneck));
    }

    #[test]
    fn marginal_split_is_rejected_below_min_gain() {
        // A pipeline whose decode load barely exceeds the copy cost: the net
        // gain is below the threshold, so the split must NOT be taken (the loop
        // would churn the live pipeline for nothing). One 1080p decode tile
        // relieves ~62 Mpix/s but copying the 1080p source costs ~62 Mpix/s too
        // -> ~0 net gain, well below the default 62 Mpix/s floor.
        let demand = PipelineDemand::new(
            cadence(),
            vec![
                TileLoad::new(Stage::Decode, Resolution::HD1080),
                TileLoad::new(Stage::Composite, Resolution::HD1080),
                TileLoad::new(Stage::Encode, Resolution::HD1080),
            ],
            Resolution::HD1080,
            PixelFormat::Nv12,
            1_000_000,
            true,
        );
        let outcome = plan_split(
            &demand,
            &[nv("GPU-0", 0), nv("GPU-1", 1)],
            &[],
            Some(Stage::Decode),
            SplitPolicy::default(),
        );
        match outcome {
            Err(SplitReject::BelowMinGain {
                gain_mpps,
                min_gain_mpps,
            }) => {
                assert!(gain_mpps < min_gain_mpps);
                assert_eq!(min_gain_mpps, 62.0);
            }
            other => panic!("expected BelowMinGain, got {other:?}"),
        }
    }

    #[test]
    fn inferred_bottleneck_from_loads_when_no_hint() {
        // No caller hint: the most-saturated of decode/encode across the loads
        // decides the cut. Here encode util (0.95) dominates decode (0.2), so
        // the cut isolates encode.
        let mut hot_enc = DeviceLoad::unknown(nv("GPU-0", 0));
        hot_enc.enc_util_frac = Some(0.95);
        hot_enc.dec_util_frac = Some(0.2);
        let outcome = plan_split(
            &demand_encode_heavy(),
            &[nv("GPU-0", 0), nv("GPU-1", 1)],
            &[hot_enc],
            None,
            SplitPolicy::default(),
        )
        .expect("encode-heavy demand clears the gain gate");
        assert_eq!(outcome.cut, CutPoint::RestThenEncode);
    }

    #[test]
    fn fully_blind_loads_with_no_hint_defer_to_degradation() {
        // No hint and no usable load signal at all (a blind probe): plan_split
        // must not guess a cut — it defers to the degradation ladder.
        let outcome = plan_split(
            &demand_decode_heavy(),
            &[nv("GPU-0", 0), nv("GPU-1", 1)],
            &[DeviceLoad::unknown(nv("GPU-0", 0))],
            None,
            SplitPolicy::default(),
        );
        assert_eq!(outcome, Err(SplitReject::NoIdentifiableBottleneck));
    }
}
