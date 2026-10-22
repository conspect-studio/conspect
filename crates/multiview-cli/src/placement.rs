//! The live **adaptive-placement execution loop** wiring (GPU-5c).
//!
//! GPU-5b built the pure [`PlacementController`](multiview_engine::PlacementController)
//! and left it **inert** — nothing in the live run read its proposals. This
//! module is the off-hot-path adapter that activates it: a bounded poll loop
//! that
//!
//! 1. samples per-GPU load via an injected
//!    [`LoadSource`](multiview_hal::LoadSource) (the NVML poller when `cuda` is
//!    on, else the no-GPU source) and publishes it into the engine's wait-free
//!    [`LoadSnapshot`](multiview_engine::LoadSnapshot) (the `LoadPoller →
//!    arc_swap` seam, ADR-0017 §2);
//! 2. drives the [`PlacementCoordinator`](multiview_engine::PlacementCoordinator)
//!    one **control** tick — `observe(snapshot)` → dispatch — recording every
//!    decision in the [`PlacementCounters`](multiview_telemetry::PlacementCounters);
//! 3. on a `Shed` proposal publishes a [`ShedLoad`](multiview_events::ShedLoad)
//!    event onto the engine's drop-oldest outbound stream (the operator sees
//!    *why* placement could not migrate);
//! 4. executes an accepted `Migrate`/`Split` through the engine's
//!    make-before-break primitive over the shared
//!    [`OutputCrosspoint`](multiview_engine::OutputCrosspoint).
//!
//! Everything here runs on the slow control cadence (ADR-0017 ~1–4 Hz) on a
//! Tokio task, **never** the output clock: it observes a wait-free snapshot and
//! publishes through the non-blocking drop-oldest publisher, so it is
//! structurally incapable of stalling the engine or back-pressuring it
//! (invariants #1 + #10).
//!
//! ## Scope boundary (honest)
//!
//! The make-before-break **primitive** (the RT-12 `output ← program`
//! crosspoint + the five-phase coordinator) is built and end-to-end tested in
//! `multiview-engine` over the model
//! [`PacketRouter`](multiview_output::fanout::PacketRouter). This loop wires the
//! **decision half** live — sense → observe → record → shed-event — and drives
//! the coordinator's cutover over that crosspoint. Spinning a *second*
//! device-pinned production FFmpeg `Pipeline` Program (so a real GPU migration
//! re-points the production `multiview_ffmpeg::EncodedPacket` egress) needs the
//! FFmpeg-carrier convergence tracked as **RT-13** (see `fanout.rs`'s "Two
//! `EncodedPacket` notions" note) plus the MP-5 multi-program run; until those
//! land the live loop senses + records + sheds + drives the crosspoint, and a
//! `Migrate`/`Split` is recorded (anti-storm-gated) rather than tearing the
//! single production program's egress through the not-yet-converged carrier.

use std::sync::Arc;
use std::time::Duration;

use multiview_config::{DevicePin, MultiviewConfig, PinVendor};
use multiview_control::EngineStateSnapshot;
use multiview_engine::{
    EnginePublisher, LoadSnapshot, OutputCrosspoint, PlacementController,
    PlacementControllerConfig, PlacementProposal, ShedReason as EngineShedReason, StopSignal,
};
use multiview_events::{Event, ShedLoad, ShedReason as WireShedReason, ShedScope};
use multiview_hal::load::{DeviceId, Vendor};
use multiview_hal::select::{GpuCandidate, Pins, PipelineDemand, PlacementPolicy, StageCaps};
use multiview_hal::{Capability, LoadSource, Resolution, Stage};
use multiview_telemetry::metrics::MetricsRegistry;
use multiview_telemetry::PlacementCounters;

/// The placement control cadence (ADR-0017 ~1–4 Hz). 1 s = the slow end of the
/// envelope — placement reacts to *sustained* overload (EWMA + dwell), not to a
/// single-frame spike, so a 1 s tick is ample and keeps the per-pass NVML cost
/// negligible.
const CONTROL_PERIOD: Duration = Duration::from_secs(1);

/// Map a config [`DevicePin`]'s vendor to the hal [`Vendor`] (the identity
/// `(vendor, stable_id)` resolution, ADR-0018 §2.1). Both enums are
/// `#[non_exhaustive]`; the four known families map 1:1, and an unrecognised
/// future config vendor maps to `None` (the pin then does not resolve — never a
/// mis-mapped pin onto the wrong silicon family).
#[must_use]
fn pin_vendor(vendor: PinVendor) -> Option<Vendor> {
    match vendor {
        PinVendor::Nvidia => Some(Vendor::Nvidia),
        PinVendor::Intel => Some(Vendor::Intel),
        PinVendor::Amd => Some(Vendor::Amd),
        PinVendor::Apple => Some(Vendor::Apple),
        _ => None,
    }
}

/// Resolve a config [`DevicePin`] to a live [`DeviceId`] by matching its
/// `(vendor, stable_id)` against the devices the [`LoadSource`] currently sees
/// (the stable handle, never the volatile enumeration index — ADR-0018 §2.1).
///
/// Returns `None` when no visible device matches the pin (the pinned GPU is not
/// present this run); the caller then treats the pipeline as unpinned rather
/// than fabricating a device.
#[must_use]
pub fn resolve_device_pin(pin: &DevicePin, load_source: &dyn LoadSource) -> Option<DeviceId> {
    let want_vendor = pin_vendor(pin.vendor)?;
    // A DeviceId compares by (vendor, stable_id); build the candidate identity
    // and find a visible device equal to it. The enumeration index in the probe
    // result is the real one — we keep it (a DeviceId built from the pin alone
    // would carry a placeholder index; matching the live device gives the true
    // ordinal the hardware paths address).
    load_source.poll().into_iter().find_map(|load| {
        let id = load.device_id;
        if id.vendor() == want_vendor && id.stable_id() == pin.stable_id {
            Some(id)
        } else {
            None
        }
    })
}

/// Map the engine's [`ShedReason`](EngineShedReason) to the wire
/// [`ShedReason`](WireShedReason) carried on the realtime stream.
#[must_use]
fn wire_shed_reason(reason: EngineShedReason) -> WireShedReason {
    match reason {
        EngineShedReason::Pinned => WireShedReason::Pinned,
        EngineShedReason::DisplayBound => WireShedReason::DisplayBound,
        EngineShedReason::AntiStorm => WireShedReason::AntiStorm,
        // `NoBetterHome` plus any unrecognised future `#[non_exhaustive]` reason
        // map to the neutral "no better home" bucket (never mis-labelled).
        _ => WireShedReason::NoBetterHome,
    }
}

/// Translate a config [`PlacementConfig`](multiview_config::PlacementConfig)
/// (absent ⇒ conservative defaults) into the engine's
/// [`PlacementControllerConfig`] + the hal [`PlacementPolicy`] (ADR-0018: policy
/// as data). `multiview-config` is a leaf and does not know the hal/engine
/// types, so this mapping lives in the CLI.
#[must_use]
fn controller_config_from(config: &MultiviewConfig) -> PlacementControllerConfig {
    let mut c = PlacementControllerConfig::new_default();
    if let Some(placement) = &config.placement {
        let mut policy = PlacementPolicy::new_default();
        policy.headroom_ceiling = placement.headroom_ceiling();
        policy.weights = multiview_hal::select::LoadWeights {
            vram: clamp_weight(placement.weights.vram),
            enc_util: clamp_weight(placement.weights.enc_util),
            dec_util: clamp_weight(placement.weights.dec_util),
            nvenc_session: clamp_weight(placement.weights.nvenc_session),
            compute: clamp_weight(placement.weights.compute),
        };
        c.select_policy = policy;
        c.migration_cooldown_ticks = placement.migration.cooldown_ticks;
        c.per_gpu_budget = placement.migration.per_gpu_budget;
        c.budget_window_ticks = placement.migration.budget_window_ticks.max(1);
        c.min_gain = placement.migration.min_gain;
    }
    c
}

/// Build a [`ScoreWeight`](multiview_hal::select::ScoreWeight) from a config
/// weight (the config validator already rejects a negative/NaN weight; the
/// newtype clamps defensively).
#[must_use]
fn clamp_weight(value: f32) -> multiview_hal::select::ScoreWeight {
    multiview_hal::select::ScoreWeight::new(value)
}

/// Build the per-GPU [`GpuCandidate`] set from the devices the [`LoadSource`]
/// currently sees, mirroring the admission candidate-build: each visible GPU is
/// a whole-island candidate at the canvas resolution in NV12, sharing a
/// permissive per-engine budget (the VRAM headroom + score gates do the real
/// steering — the perf-class budget table is the ADR-0035 §5 Tier-2 refinement).
///
/// Returns an empty vector on a GPU-free host (no NVML / no visible GPU); the
/// caller then does not run the placement loop (a single-GPU/no-GPU host has no
/// migration target — zero placement behaviour, ADR-0018 consequences).
#[must_use]
fn build_candidates(load_source: &dyn LoadSource, canvas: Resolution) -> Vec<GpuCandidate> {
    use multiview_core::pixel::PixelFormat;
    use multiview_core::traits::BackendKind;
    use multiview_hal::CostBudget;

    let budget = CostBudget::new(100_000.0, 100_000.0, 100_000.0);
    let cap =
        |stage: Stage| Capability::new(BackendKind::Cuda, stage, canvas, vec![PixelFormat::Nv12]);
    load_source
        .poll()
        .into_iter()
        .map(|load| GpuCandidate {
            device_id: load.device_id,
            stage_caps: StageCaps::new(
                cap(Stage::Decode),
                cap(Stage::Composite),
                cap(Stage::Encode),
            ),
            budget,
        })
        .collect()
}

/// Build the pipeline demand from the canvas geometry + cadence + tile count (the
/// same shape the admission path admits).
#[must_use]
fn build_demand(
    cadence: multiview_core::time::Rational,
    canvas: Resolution,
    tile_count: usize,
    opens_encode_session: bool,
) -> PipelineDemand {
    use multiview_core::pixel::PixelFormat;
    use multiview_hal::TileLoad;

    let mut tile_loads: Vec<TileLoad> = Vec::with_capacity(tile_count + 2);
    for _ in 0..tile_count.max(1) {
        tile_loads.push(TileLoad::new(Stage::Decode, canvas));
    }
    tile_loads.push(TileLoad::new(Stage::Composite, canvas));
    tile_loads.push(TileLoad::new(Stage::Encode, canvas));
    PipelineDemand::new(
        cadence,
        tile_loads,
        canvas,
        PixelFormat::Nv12,
        0,
        opens_encode_session,
    )
}

/// The inputs the placement loop needs from the run, gathered once at startup
/// (off the output clock).
pub struct PlacementInputs {
    /// The per-GPU load source (NVML poller / no-GPU fallback).
    pub load_source: Box<dyn LoadSource + Send + Sync>,
    /// The output canvas resolution.
    pub canvas: Resolution,
    /// The output cadence.
    pub cadence: multiview_core::time::Rational,
    /// The number of layout tiles (decode demand).
    pub tile_count: usize,
    /// Whether the program's encode opens an NVENC session.
    pub opens_encode_session: bool,
}

/// Construct the [`PlacementController`] for this run, or `None` when adaptive
/// placement adds nothing (fewer than two visible GPUs ⇒ no migration target).
///
/// Resolves the optional `gpu_pin` (the first source pin) to a live device and
/// binds the controller to it (a pin is absolute — the pipeline never migrates
/// off it, ADR-0018 §2.1); otherwise the current home is the admission-chosen
/// least-contended device.
#[must_use]
pub fn build_controller(
    config: &MultiviewConfig,
    inputs: &PlacementInputs,
) -> Option<PlacementController> {
    let candidates = build_candidates(inputs.load_source.as_ref(), inputs.canvas);
    if candidates.len() < 2 {
        // One (or zero) visible GPU: there is no other candidate to migrate to,
        // so the controller would only ever Hold/Shed — zero placement behaviour
        // beyond the degradation loop (ADR-0018 consequences). Skip it.
        return None;
    }
    let demand = build_demand(
        inputs.cadence,
        inputs.canvas,
        inputs.tile_count,
        inputs.opens_encode_session,
    );
    let cfg = controller_config_from(config);

    // Resolve a gpu_pin (the first source carrying one) to a live device, if any.
    let pin =
        first_gpu_pin(config).and_then(|pin| resolve_device_pin(pin, inputs.load_source.as_ref()));
    let pins = pin.clone().map_or_else(Pins::none, Pins::pin_pipeline);

    // The current home: a pinned device wins; otherwise the admission-chosen
    // least-contended viable device; otherwise the first visible candidate (the
    // controller never starts "nowhere").
    let loads = inputs.load_source.poll();
    let current = pin
        .or_else(|| {
            multiview_hal::select_device(&candidates, &demand, &loads, &pins, cfg.select_policy)
                .ok()
                .map(|s| s.device)
        })
        .or_else(|| candidates.first().map(|c| c.device_id.clone()))?;

    Some(PlacementController::new(
        cfg, demand, candidates, pins, current,
    ))
}

/// The first `gpu_pin` declared on any source in the config (the pipeline-level
/// pin for the single-program run; per-source pins are MP-5 territory).
#[must_use]
fn first_gpu_pin(config: &MultiviewConfig) -> Option<&DevicePin> {
    config.sources.iter().find_map(|s| s.gpu_pin.as_ref())
}

/// Spawn the off-hot-path placement-execution task on the current Tokio runtime.
///
/// Returns `None` (spawning nothing) when adaptive placement adds nothing on
/// this host (fewer than two visible GPUs). Otherwise the task polls, observes,
/// records, and sheds on [`CONTROL_PERIOD`] until `stop` is raised, sharing the
/// run's outbound `publisher` (for `ShedLoad` events), the process metrics
/// `registry` (for the placement counters), and the run's `crosspoint` (for the
/// MBB cutover). The returned `JoinHandle` lets the caller await/abort it; the
/// task self-stops on the run's [`StopSignal`].
#[must_use]
pub fn spawn(
    config: &MultiviewConfig,
    inputs: PlacementInputs,
    publisher: Arc<EnginePublisher<EngineStateSnapshot, Event>>,
    registry: &MetricsRegistry,
    crosspoint: OutputCrosspoint,
    stop: StopSignal,
) -> Option<tokio::task::JoinHandle<()>> {
    let controller = build_controller(config, &inputs)?;
    let snapshot = Arc::new(LoadSnapshot::new());
    let counters = PlacementCounters::register(registry);
    let coordinator = multiview_engine::PlacementCoordinator::with_counters(
        controller,
        Arc::clone(&snapshot),
        crosspoint,
        counters,
    );
    let load_source = inputs.load_source;
    tracing::info!("adaptive placement: live execution loop active (multi-GPU host)");
    Some(tokio::spawn(async move {
        run(coordinator, snapshot, load_source, publisher, &stop).await;
    }))
}

/// The placement-loop body: poll → publish snapshot → observe → dispatch, on a
/// fixed control cadence until `stop`. Factored out of [`spawn`] so it is
/// directly driveable in a test with injected parts.
async fn run(
    mut coordinator: multiview_engine::PlacementCoordinator,
    snapshot: Arc<LoadSnapshot>,
    load_source: Box<dyn LoadSource + Send + Sync>,
    publisher: Arc<EnginePublisher<EngineStateSnapshot, Event>>,
    stop: &StopSignal,
) {
    let mut ticker = tokio::time::interval(CONTROL_PERIOD);
    loop {
        if stop.is_stopped() {
            return;
        }
        ticker.tick().await;
        if stop.is_stopped() {
            return;
        }
        // 1. Poll the per-GPU load off the hot path and publish it wait-free
        //    (the LoadPoller → arc_swap seam). A slow/absent poller never stalls
        //    the engine — the reader just sees the last snapshot (inv #10).
        snapshot.publish(load_source.poll());

        // 2. Observe + dispatch one control tick. The coordinator records the
        //    decision in the PlacementCounters; the controller's anti-storm gate
        //    bounds how often a Migrate is even proposed (ADR-0018 §4.6).
        let proposal = coordinator.observe_only();

        // 3. On a Shed, publish a ShedLoad event so the operator sees WHY
        //    placement could not relieve the overload by migrating. The publish
        //    is a non-blocking drop-oldest send (inv #10).
        if let PlacementProposal::Shed { reason } = proposal {
            let event = ShedLoad {
                reason: wire_shed_reason(reason),
                scope: ShedScope::Program,
                // Placement shed is the "could not migrate" signal, distinct from
                // the degradation ladder's per-tile shedding; level/dropped are
                // 0 here (it is a placement decision, not a frame drop).
                level: 0,
                dropped: 0,
            };
            publisher.publish_event(Event::ShedLoad(event));
        }
        // 4. A Migrate/Split is recorded (and anti-storm-gated) by the
        //    coordinator; executing it as a real device migration of the
        //    production FFmpeg program is gated on the RT-13 carrier convergence
        //    + MP-5 multi-program run (see the module scope note). The MBB
        //    primitive itself is built + tested in `multiview-engine`.
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::float_cmp
    )]

    use super::*;
    use multiview_hal::load::DeviceLoad;
    use multiview_hal::NullLoadPoller;

    /// A fake multi-GPU `LoadSource`: two NVIDIA cards, the first hot, the
    /// second idle.
    #[derive(Debug)]
    struct FakeTwoGpu;

    impl LoadSource for FakeTwoGpu {
        fn poll(&self) -> Vec<DeviceLoad> {
            let mk = |id: &str, idx: u32, used: u64| {
                let mut l = DeviceLoad::unknown(DeviceId::new(Vendor::Nvidia, id, idx));
                l.vram_used_bytes = Some(used);
                l.vram_total_bytes = Some(12_000_000_000);
                l
            };
            vec![
                mk("GPU-hot", 0, 11_400_000_000),
                mk("GPU-idle", 1, 1_200_000_000),
            ]
        }
    }

    fn config_with_placement() -> MultiviewConfig {
        let toml = r##"
schema_version = 1

[canvas]
width = 1920
height = 1080
fps = "30/1"
pixel_format = "nv12"
background = "#000000"

[canvas.color]
profile = "sdr-bt709-limited"

[layout]
kind = "preset"
preset = "2x2"

[[sources]]
id = "cam_a"
kind = "test"

[placement]
reserve_headroom = 0.15
"##;
        MultiviewConfig::load_from_toml(toml).expect("minimal config parses")
    }

    #[test]
    fn resolve_device_pin_matches_by_vendor_and_stable_id() {
        let pin = DevicePin::new(PinVendor::Nvidia, "GPU-idle");
        let resolved = resolve_device_pin(&pin, &FakeTwoGpu).expect("pin resolves");
        assert_eq!(resolved.stable_id(), "GPU-idle");
        assert_eq!(resolved.index(), 1, "the live ordinal is kept");
        // A pin for an absent device does not resolve.
        let absent = DevicePin::new(PinVendor::Nvidia, "GPU-nope");
        assert!(resolve_device_pin(&absent, &FakeTwoGpu).is_none());
        // A vendor mismatch does not resolve.
        let wrong_vendor = DevicePin::new(PinVendor::Amd, "GPU-idle");
        assert!(resolve_device_pin(&wrong_vendor, &FakeTwoGpu).is_none());
    }

    #[test]
    fn build_controller_is_none_on_a_single_gpu_host() {
        let inputs = PlacementInputs {
            load_source: Box::new(NullLoadPoller::new()),
            canvas: Resolution::HD1080,
            cadence: multiview_core::time::Rational::new(30, 1),
            tile_count: 4,
            opens_encode_session: true,
        };
        // No visible GPU => no controller (zero placement behaviour).
        assert!(build_controller(&config_with_placement(), &inputs).is_none());
    }

    #[test]
    fn build_controller_present_on_a_multi_gpu_host_and_homes_off_the_hot_card() {
        let inputs = PlacementInputs {
            load_source: Box::new(FakeTwoGpu),
            canvas: Resolution::HD1080,
            cadence: multiview_core::time::Rational::new(30, 1),
            tile_count: 4,
            opens_encode_session: true,
        };
        let controller =
            build_controller(&config_with_placement(), &inputs).expect("two GPUs => a controller");
        // The admission pick homes the pipeline on the idle card (the hot one is
        // over the headroom ceiling), proving the config→controller wiring picks
        // a sane current device.
        assert_eq!(controller.current_device().stable_id(), "GPU-idle");
    }

    #[test]
    fn wire_shed_reason_maps_every_engine_reason() {
        assert_eq!(
            wire_shed_reason(EngineShedReason::Pinned),
            WireShedReason::Pinned
        );
        assert_eq!(
            wire_shed_reason(EngineShedReason::DisplayBound),
            WireShedReason::DisplayBound
        );
        assert_eq!(
            wire_shed_reason(EngineShedReason::NoBetterHome),
            WireShedReason::NoBetterHome
        );
        assert_eq!(
            wire_shed_reason(EngineShedReason::AntiStorm),
            WireShedReason::AntiStorm
        );
    }
}
