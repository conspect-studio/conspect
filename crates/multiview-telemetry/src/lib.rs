//! # multiview-telemetry
//!
//! Observability primitives for the Multiview engine: a `tracing` subscriber
//! builder, a small dependency-free metrics registry, and pure health-state
//! types for the `/livez` and `/readyz` probes.
//!
//! This crate is **pure Rust with no native deps and no GPU**, so it builds in
//! the default CI-green feature set. It owns the observability *model*; it does
//! not run an HTTP server (that is `multiview-control`'s job) and it never installs
//! a process-global tracing subscriber implicitly.
//!
//! Telemetry is best-effort and must never back-pressure or crash the engine
//! (invariant #10): metric handles are lock-free atomics on the read/update
//! path, and every fallible lock recovers a poisoned guard instead of
//! propagating a panic.
//!
//! See the observability brief (core-engine §15) and ADR-R009 for the metric
//! taxonomy and the liveness/readiness split.
//!
//! ## Modules
//!
//! * [`availability`] — [`availability::AvailabilityCounters`]: pure G.826-style
//!   uptime / alarm-second / error-second / severely-errored-second accounting
//!   with a derived availability ratio.
//! * [`clock`] — clock-layer servo telemetry (DEV-C4; ADR-R012, implementing the
//!   ADR-M010 sync-acceptance gate): the disciplined-reference servo offset (ns)
//!   / frequency (ppb) gauges ([`clock::ClockServoGauges`]) labelled by `source`,
//!   the display-audio buffer servo gauges ([`clock::AudioServoGauges`]) labelled
//!   by `sink`, and the documented soak pass thresholds
//!   ([`clock::PTP_OFFSET_P99_MAX_NS`] / [`clock::CHRONY_OFFSET_P99_MAX_NS`]) as
//!   exact integer nanoseconds (invariant #3).
//! * [`soak`] — the pure, dependency-free acceptance-soak verdict analyzer
//!   (DEV-C4; ADR-R012): nearest-rank p99 `|offset|`
//!   ([`soak::p99_abs_offset_ns`]), the per-leg [`soak::evaluate_offset`], the
//!   invariant-#1 [`soak::cadence_uninterrupted`] chaos assertion, and the
//!   aggregate [`soak::SoakReport`] PASS/FAIL. The same code CI exercises is what
//!   a hardware soak is judged by (`cargo xtask soak-report`).
//! * [`health`] — [`health::HealthState`] with readiness gates for the probes.
//! * [`log_capture`] — the resource-scoped structured log producer (ADR-0060):
//!   the bounded drop-oldest [`log_capture::LogRing`] and the
//!   [`log_capture::LogCaptureLayer`] `tracing` layer that mirrors every event
//!   (ours and the libav bridge's) into it with its `resource_id` attribution,
//!   feeding the control-plane `GET /api/v1/logs` tail and `Topic::Logs`.
//! * [`metrics`] — [`metrics::MetricsRegistry`] with counters, gauges,
//!   histograms, and bounded-cardinality [`metrics::Labels`].
//! * [`gpu`] — per-GPU load gauges ([`gpu::GpuGauges`]) keyed by a bounded
//!   `{gpu, vendor}` label, the whole-system [`gpu::CpuGauge`], and the pure
//!   std-only [`gpu::CpuSampler`] (`/proc/stat`). Unknown vendor metrics are
//!   **not registered** (ADR-0017 §4.1 — "n/a", never a false zero).
//! * [`placement`] — placement/migration/split decision counters
//!   ([`placement::PlacementCounters`]) for the GPU work-placement loop
//!   (ADR-0018 — "every adaptation logged"), labelled by a bounded
//!   `outcome`/`reason` vocabulary.
//! * [`retention`] — the **consent-independent local metrics retention store**
//!   ([`retention::RetentionStore`], CONSPECT engine-seam S5; ADR-0052 §3): a
//!   rolling, bounded, minute-bucketed ≥7-day record of the §7.2 support
//!   categories (utilisation percentiles, shed-load, per-input reconnects,
//!   incident markers), retained **regardless of telemetry consent**.
//! * [`tracing_init`] — [`tracing_init::SubscriberBuilder`] for an
//!   `EnvFilter`-based subscriber.
//! * [`syslog`] — a **pure** RFC 5424 message formatter (always compiled); the
//!   UDP/TCP sender is behind the off-by-default `syslog` feature.
//! * `snmp` *(feature `snmp`)* — the SNMP trap path: the Multiview enterprise
//!   MIB/OID model, a **pure, golden-tested** ASN.1 BER encoder and SNMPv2-Trap
//!   PDU/message builder, X.733 raise/clear mapping, and a compile-only UDP
//!   `TrapSender`. Behind the off-by-default `snmp` feature (named as a plain
//!   code span: the module is absent from the default doc build).
#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod availability;
pub mod clock;
pub mod error;
pub mod gpu;
pub mod health;
pub mod log_capture;
pub mod metrics;
pub mod placement;
pub mod retention;
pub mod rist;
#[cfg(feature = "snmp")]
pub mod snmp;
pub mod soak;
pub mod syslog;
pub mod tracing_init;

pub use availability::{AvailabilityCounters, AvailabilitySnapshot};
pub use clock::{
    AudioServoGauges, ClockServoGauges, ClockSourceLabel, CHRONY_OFFSET_P99_MAX_NS,
    PTP_OFFSET_P99_MAX_NS, SOAK_WINDOW_SECS,
};
pub use error::{Result, TelemetryError};
pub use gpu::{CpuGauge, CpuSampler, GpuGauges, GpuLabels, VendorExposes};
pub use health::{GateId, HealthState, Liveness, Readiness};
pub use log_capture::{LogCaptureLayer, LogFilter, LogLevel, LogRecord, LogResourceKind, LogRing};
pub use metrics::{
    Counter, Gauge, Histogram, HistogramSnapshot, Labels, MetricKind, MetricsRegistry,
    SeriesDescriptor,
};
pub use placement::{PlacementCounters, SuppressReason};
pub use retention::{
    IncidentKind, IncidentMarker, ReconnectEvent, RetentionStore, RetentionWindow, ShedEvent,
    ShedReason, UtilisationBucket, UtilisationSample, UtilisationSummary, BUCKET_SECONDS,
    MAX_EVENTS_PER_BUCKET, RETENTION_BUCKETS,
};
pub use rist::{RistLinkAssessment, RistLinkGauges, RistLinkRole, RistLinkSample};
pub use syslog::{Facility, SdElement, Severity, SyslogMessage};
pub use tracing_init::{Output, SubscriberBuilder};
