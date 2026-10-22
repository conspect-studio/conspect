//! # multiview-hal
//!
//! Hardware-abstraction layer for the **Multiview** engine: the pure-Rust
//! capability + cost + planner model that feeds invariants #6
//! (decode-at-display-resolution, budgeted in megapixels/sec) and #9
//! (resource-adaptive degradation).
//!
//! It has these cooperating pieces:
//!
//! - [`capability`] — [`Capability`] descriptors: per `(BackendKind, Stage)`,
//!   the max resolution, accepted pixel formats, and decode-resize support.
//! - [`registry`] — the [`BackendRegistry`]: register/query capabilities by
//!   `(stage, kind)`, seeded with the always-available software tier.
//! - [`cost`] — the [`CostBudget`] and per-tile [`TileLoad`], all in
//!   **megapixels/sec** (never stream count).
//! - [`planner`] — the [`Planner`]: admission control (does a [`Plan`] fit
//!   every engine budget?) plus the hysteresis-controlled, cheapest-impact-first
//!   [`degradation`] ladder (no flapping).
//! - [`load`] — the live per-device [`load::DeviceLoad`] model + the injectable
//!   [`load::LoadProbe`] vendor seam and the off-hot-path [`load::LoadPoller`]
//!   (ADR-0017). The pure model always compiles; vendor probes (NVML via the
//!   runtime-loaded `nvml-wrapper`, Linux sysfs) are feature-gated.
//! - [`select`] — the pure, deterministic affinity-gated least-loaded
//!   [`select::select_device`] placement policy (ADR-0018): pins win, hard gates
//!   build the whole-pipeline candidate set, survivors are scored by a
//!   dominant-resource load model.
//! - [`split`] — the deliberate last-resort multi-GPU [`split::plan_split`]
//!   decision (ADR-0018 §20): reached only when no single GPU can host the whole
//!   island, it cuts at the cheapest point (never fragmenting `composite`),
//!   accounts the cross-GPU host round-trip explicitly, and is gated by a
//!   minimum-gain threshold so a live pipeline is never churned for a marginal
//!   improvement.
//! - [`scanout`] — KMS scanout discovery ([`scanout::ScanoutInventory`],
//!   DEV-B2 / ADR-0044 §3): per DRM card node, the display connectors it owns
//!   and the GPU behind it, so a local display sink's composite can be pinned to
//!   the connector-owning GPU ([`select::PipelineDemand::with_sink_locality`]).
//!   The owning [`load::DeviceId`] is **reconciled** against the canonical device
//!   set by PCI bus id and reused verbatim ([`scanout::ScanoutInventory::from_drm_cards`]),
//!   so a scanout locality matches placement's identities for the same physical
//!   GPU — never a re-derived id that would silently miss. The pure
//!   reconciliation + the injectable [`scanout::ScanoutProbe`] seam always
//!   compile and are CI-tested; the real DRM enumeration
//!   ([`scanout::DrmScanoutProbe`]) is feature-gated behind `display-kms` and is
//!   empty on a GPU-less host.
//! - [`failure`] — the failure-learning ledger ([`failure::FailureLedger`],
//!   Tier-2 gap P1c): a pure, decaying per-`(Stage, HardwareId)` penalty the
//!   data plane bumps with typed [`failure::FailureSignal`]s, so a placement
//!   that keeps *flapping* on a piece of hardware is deprioritised (or, when
//!   extreme, excluded) on the next selection — and decays back toward zero when
//!   it stops failing (never a permanent ban). The scorer adds the penalty to a
//!   candidate's cost.
//!
//! Hardware probing ([`probe`]) is the only seam that touches vendors. It
//! follows the three-layer model (core-engine §6.2): the injectable
//! [`probe::DeviceProbe`] decides device *presence* (environment detection, no
//! native SDK), and the feature-gated backend crates later refine the resulting
//! [`Capability`] with true vendor caps queries. The native paths live behind
//! off-by-default features (`cuda`, `vaapi`, `qsv`, `videotoolbox`); the default
//! build is pure Rust with no native deps and every hardware probe reports
//! *unavailable*. With a feature on but no device present (CI), the probe still
//! reports *unavailable* cleanly — never a panic. The library target is
//! `multiview_hal`.
//!
//! See [core-engine §6](../../docs/research/core-engine.md),
//! [efficiency](../../docs/research/efficiency.md), ADR-0003, and ADR-0004.
#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod capability;
pub mod composite_probe;
pub mod cost;
pub mod degradation;
pub mod error;
pub mod failure;
pub mod load;
pub mod perf;
pub mod planner;
pub mod probe;
pub mod registry;
pub mod scanout;
pub mod select;
pub mod split;

pub use capability::{Capability, Resolution, Stage};
pub use composite_probe::{
    composite_mismatch, hardware_present, AdapterClass, AdapterDeviceType, AdapterReport,
    CompositeMismatch,
};
pub use cost::{CostBudget, TileLoad};
pub use degradation::{
    actions_at_level, DegradationAction, Hysteresis, HysteresisConfig, LadderMove, MAX_LEVEL,
};
pub use error::{Error, Result};
pub use failure::{
    FailureLedger, FailureSignal, HardwareId, LedgerConfig, DEFAULT_EXCLUSION_THRESHOLD,
    DEFAULT_HALF_LIFE_NS, DEFAULT_MAX_KEYS, PENALTY_CEILING,
};
#[cfg(feature = "cuda")]
pub use load::NvmlLoadPoller;
#[cfg(any(feature = "vaapi", feature = "qsv"))]
pub use load::SysfsLoadPoller;
pub use load::{
    normalize_pci_bus_id, DeviceId, DeviceLoad, GpuTargetInfo, LoadPoller, LoadProbe, LoadSample,
    LoadSource, NullLoadPoller, PollInterval, SelfShare, Vendor,
};
pub use perf::{PerfClass, PerfSignals, ARCH_TABLE, DEFAULT_PERF_CLASS};
pub use planner::{Admission, Plan, Planner, StageUsage};
pub use probe::{
    detect, software_capability, DeviceCaps, DeviceProbe, EnvProbe, HardwareKind, ProbeOutcome,
    StageSupport,
};
pub use registry::BackendRegistry;
pub use scanout::{
    CardNode, ConnectionStatus, Connector, ConnectorId, DrmCardDescriptor, DrmScanoutProbe,
    ScanoutInventory, ScanoutProbe,
};
pub use select::{
    select_device, GpuCandidate, LoadWeights, Pins, PipelineDemand, PlacementPolicy, RejectReason,
    ScoreWeight, SelectOutcome, Selection, StageCaps,
};
pub use split::{
    plan_split, CrossGpuCopy, CutPoint, SplitOutcome, SplitPlan, SplitPolicy, SplitReject,
};

// Re-export the shared enums the HAL describes assignments with, so downstream
// crates can name them through `multiview_hal` without a direct `multiview_core`
// dependency where convenient.
pub use multiview_core::traits::BackendKind;
