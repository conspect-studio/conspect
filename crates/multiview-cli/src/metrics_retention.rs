//! The CONSPECT **local-metrics retention feed** (engine-seam S5; ADR-0052 §3,
//! conspect-account-architecture §7.2): an off-hot-loop subscriber that mirrors
//! live engine [`Event`]s into the consent-independent
//! [`RetentionStore`](multiview_telemetry::retention::RetentionStore).
//!
//! ## Why this lives here
//!
//! The retention *store* is a pure leaf in `multiview-telemetry` (no engine/event
//! dependency). The *mapping* from the engine's outbound event vocabulary onto
//! the store's typed `record_*` calls needs both `multiview_engine` (the
//! broadcast) and `multiview_events` (the `Event` enum), so it lives in the cli
//! beside the [`system_metrics`](crate::system_metrics) sampler — the run wires
//! both off-hot-loop tasks onto the **same** outbound publisher.
//!
//! ## Consent independence
//!
//! Nothing on this path checks telemetry consent: the store records every live
//! event it classifies, regardless of the opt-in daily telemetry pipe's state
//! (ADR-0052 §3). Consent governs the *outbound* analytics pipe; this is the
//! operator's *own* local diagnostics buffer.
//!
//! ## Isolation (invariant #10) — the load-bearing property
//!
//! Exactly like the engine→control alarm ingest, this task **only ever reads**
//! the engine's drop-oldest broadcast ([`EventSubscription`]). It never sends on
//! a path the engine awaits and never blocks the engine's publish. When it falls
//! behind, the broadcast reports [`RecvError::Lagged`] and it **resubscribes at
//! the head** (lagged-skip), dropping missed events rather than back-pressuring.
//! The retention store's writes are themselves non-blocking and bounded, so the
//! whole feed is incapable of stalling the engine.
//!
//! ## Live coverage (honest)
//!
//! The feed wires every §7.2 category that has a **real live producer** on the
//! engine event stream today:
//!
//! * **Utilisation** ← [`Event::SystemMetrics`] (the always-running
//!   [`system_metrics`](crate::system_metrics) sampler).
//! * **Per-input reconnect history** ← [`Event::TileState`] transitions *into*
//!   [`LifecycleState::Reconnecting`] for a bound input (the tile state machine
//!   the run publishes).
//! * **Incident markers** ← active [`Event::AlarmRaised`]/[`Event::AlarmUpdated`]
//!   transitions (mapped by [`AlarmKind`]) and active
//!   [`Event::HealthWarningRaised`] warnings (mapped by subsystem).
//! * **Shed-load** ← [`Event::ShedLoad`] (mapped by [`WireShedReason`]). The live
//!   producer today is the pipeline's **encode/egress drop-on-overload** shed
//!   ([`WireShedReason::EncoderOverload`]): when the bake/encode consumer cannot
//!   keep up at cadence, the hot loop sheds-and-counts a composited frame rather
//!   than blocking the output clock (invariants #1 + #10), and emits a
//!   change-driven `shed.load` through the same drop-oldest publisher. The
//!   degradation/placement controller's `Pinned`/`DisplayBound`/`NoBetterHome`/
//!   `AntiStorm` reasons share the same event shape and map here too; those
//!   reasons have no run-path producer yet (the controller is a pure proposer not
//!   yet wired into the run loop), so they are reachable but unexercised today. We
//!   do **not** fabricate any shed — only a real drop emits one.

use multiview_core::alarm::{AlarmKind, AlarmRecord, AlarmScope};
use multiview_engine::{EventSubscription, RecvError};
use multiview_events::{Event, LifecycleState, ShedReason as WireShedReason, SystemMetrics};
use multiview_telemetry::retention::{IncidentKind, RetentionStore, ShedReason, UtilisationSample};

/// A classified retention update derived from one engine [`Event`] — the pure
/// unit of behaviour the feed loop applies to the store.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum RetentionUpdate {
    /// A whole-system utilisation sample.
    Utilisation(UtilisationSample),
    /// A per-input reconnect occurrence.
    Reconnect {
        /// The configured input/source id that reconnected.
        input_id: String,
        /// The reconnect attempt counter, when the source reported one.
        attempt: u32,
    },
    /// A load-shed occurrence (live producer: [`Event::ShedLoad`] — see the
    /// module-level coverage note).
    Shed {
        /// Why load was shed.
        reason: ShedReason,
    },
    /// A notable incident marker.
    Incident {
        /// The incident class.
        kind: IncidentKind,
        /// What the incident applied to (input id / subsystem / "system").
        subject: String,
    },
}

/// Classify an engine [`Event`] into a [`RetentionUpdate`], or [`None`] when the
/// event carries no §7.2 retention category. Pure and total — exhaustively
/// unit-testable with no async, no sockets, no clock.
#[must_use]
pub fn classify(event: &Event) -> Option<RetentionUpdate> {
    match event {
        Event::SystemMetrics(metrics) => {
            Some(RetentionUpdate::Utilisation(utilisation_from(metrics)))
        }
        Event::TileState(tile) => {
            // A reconnect is a transition INTO Reconnecting for a bound input.
            // Other lifecycle moves (Live<->Stale, ->NoSignal) are not reconnects.
            if tile.to == LifecycleState::Reconnecting {
                tile.input
                    .as_ref()
                    .map(|input_id| RetentionUpdate::Reconnect {
                        input_id: input_id.clone(),
                        // The wire `TileState` carries no attempt counter; the
                        // occurrence itself is the history entry. Use 1 (a single
                        // recorded reconnect occurrence).
                        attempt: 1,
                    })
            } else {
                None
            }
        }
        // Only an ACTIVE alarm raise/update marks an incident occurrence; a clear
        // or ack is not an incident.
        Event::AlarmRaised(t) | Event::AlarmUpdated(t) => incident_from_alarm(&t.record),
        // An active health warning (e.g. encoder/compositor saturation-class) is
        // an incident; a clear is not.
        Event::HealthWarningRaised(w) if w.active => Some(RetentionUpdate::Incident {
            kind: incident_kind_for_subsystem(&w.subsystem),
            subject: w.subsystem.clone(),
        }),
        // A shed-load decision is the §7.2 shed category's live producer: map the
        // wire reason onto the store reason faithfully (no fabrication).
        Event::ShedLoad(shed) => Some(RetentionUpdate::Shed {
            reason: shed_reason_from_wire(shed.reason),
        }),
        _ => None,
    }
}

/// Map the wire [`WireShedReason`] onto the retention store's [`ShedReason`].
///
/// A faithful 1:1 mapping — the store mirrors the same reason vocabulary. The
/// wire enum is `#[non_exhaustive]`, so a future reason the store does not yet
/// know falls back to [`ShedReason::NoBetterHome`] (the conservative "load could
/// not be relieved by moving" default) rather than dropping the shed or
/// panicking — the occurrence is still recorded.
fn shed_reason_from_wire(reason: WireShedReason) -> ShedReason {
    match reason {
        WireShedReason::Pinned => ShedReason::Pinned,
        WireShedReason::DisplayBound => ShedReason::DisplayBound,
        WireShedReason::AntiStorm => ShedReason::AntiStorm,
        WireShedReason::EncoderOverload => ShedReason::EncoderOverload,
        // `WireShedReason::NoBetterHome` maps here. `WireShedReason` is
        // `#[non_exhaustive]`, so the wildcard also covers any future reason —
        // recorded as a generic local shed ("load could not be relieved by
        // moving") rather than losing the occurrence or panicking. The two share
        // a body, so a single wildcard arm is the clippy-clean expression.
        _ => ShedReason::NoBetterHome,
    }
}

/// Map a live [`SystemMetrics`] sample onto a [`UtilisationSample`]: cpu busy
/// fraction, the max GPU compute fraction across the device list (the aggregate
/// "how busy is the busiest GPU"), and the program rate. `f32 -> f64` widening is
/// lossless.
fn utilisation_from(metrics: &SystemMetrics) -> UtilisationSample {
    let gpu_util = metrics
        .gpus
        .iter()
        .map(|g| f64::from(g.compute_util))
        .fold(None::<f64>, |acc, v| {
            Some(acc.map_or(v, |cur: f64| cur.max(v)))
        });
    UtilisationSample {
        cpu_util: f64::from(metrics.cpu_util),
        gpu_util,
        program_fps: metrics.program_fps.map(f64::from),
    }
}

/// Map an [`AlarmRecord`] to an incident marker when its kind/severity makes it a
/// notable incident occurrence; [`None`] for cleared alarms or kinds we do not
/// track as retention incidents.
fn incident_from_alarm(record: &AlarmRecord) -> Option<RetentionUpdate> {
    if !record.is_active() {
        return None;
    }
    let kind = incident_kind_for_alarm(record.kind)?;
    Some(RetentionUpdate::Incident {
        kind,
        subject: alarm_subject(&record.scope),
    })
}

/// Map an [`AlarmKind`] to a retention [`IncidentKind`]. Signal/format/freeze
/// faults on a source are input-flap incidents; the audio/caption probe faults
/// are not retention incidents (they ride the alarm store, not this diagnostics
/// buffer). Returns [`None`] for kinds we do not retain as incident markers.
fn incident_kind_for_alarm(kind: AlarmKind) -> Option<IncidentKind> {
    match kind {
        AlarmKind::SignalLoss
        | AlarmKind::Freeze
        | AlarmKind::Black
        | AlarmKind::FormatMismatch => Some(IncidentKind::InputFlap),
        // The audio/caption probe faults (`Silence`, `OverLevel`, `Clip`,
        // `PhaseInvert`, `LoudnessViolation`, `CaptionLoss`) are NOT retained as
        // diagnostics incidents — they ride the alarm store, not this buffer. The
        // wildcard also covers any future non-exhaustive `AlarmKind`; we never
        // fabricate an incident for a kind we do not explicitly map.
        _ => None,
    }
}

/// The incident subject string for an alarm scope (the input/probe id, the
/// tile/group name, or "system").
fn alarm_subject(scope: &AlarmScope) -> String {
    match scope {
        AlarmScope::Probe { id } => id.clone(),
        AlarmScope::Tile { index } => format!("tile-{index}"),
        AlarmScope::Group { name } => name.clone(),
        AlarmScope::System => "system".to_owned(),
        // `AlarmScope` is non-exhaustive: an unknown future scope falls back to a
        // stable generic subject rather than panicking.
        _ => "unknown".to_owned(),
    }
}

/// Choose the incident kind for a health-warning subsystem string. Encoder
/// subsystems are saturation incidents; clock/reference subsystems are holdover
/// incidents; everything else is an input flap (the broadest "something went
/// wrong with a source/path" marker).
fn incident_kind_for_subsystem(subsystem: &str) -> IncidentKind {
    let lower = subsystem.to_ascii_lowercase();
    if lower.contains("encode") || lower.contains("encoder") {
        IncidentKind::EncoderSaturation
    } else if lower.contains("clock") || lower.contains("genlock") || lower.contains("ptp") {
        IncidentKind::ClockHoldover
    } else {
        IncidentKind::InputFlap
    }
}

/// Apply a classified [`RetentionUpdate`] to `store` at `now_unix_seconds`.
pub fn apply(update: RetentionUpdate, store: &RetentionStore, now_unix_seconds: u64) {
    match update {
        RetentionUpdate::Utilisation(sample) => {
            store.record_utilisation_at(now_unix_seconds, sample);
        }
        RetentionUpdate::Reconnect { input_id, attempt } => {
            store.record_reconnect_at(now_unix_seconds, input_id, attempt);
        }
        RetentionUpdate::Shed { reason } => {
            store.record_shed_at(now_unix_seconds, reason);
        }
        RetentionUpdate::Incident { kind, subject } => {
            store.record_incident_at(now_unix_seconds, kind, subject);
        }
    }
}

/// The outcome of pumping one step of the retention feed loop (mirrors the alarm
/// ingest's step shape so the control flow is testable without a live broadcast).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IngestStep {
    /// A classified event was applied to the store.
    Applied,
    /// A non-retention event (or a resumed/duplicate) was skipped.
    Skipped,
    /// This subscriber lagged; it resubscribed at the head (lagged-skip). The
    /// engine was never back-pressured (invariant #10).
    Lagged,
    /// The engine is gone (every publish handle dropped); the loop should stop.
    Closed,
}

/// Receive one event and record it into `store` at `now_unix_seconds`, returning
/// the step outcome.
///
/// On [`RecvError::Lagged`] this resubscribes at the head and returns
/// [`IngestStep::Lagged`] — it never propagates back-pressure (invariant #10). On
/// an unclassified event it returns [`IngestStep::Skipped`]; on a classified one
/// it records into the store and returns [`IngestStep::Applied`].
pub async fn ingest_step(
    sub: &mut EventSubscription<Event>,
    store: &RetentionStore,
    now_unix_seconds: u64,
) -> IngestStep {
    match sub.recv().await {
        Ok(seq_event) => match classify(&seq_event.event) {
            Some(update) => {
                apply(update, store, now_unix_seconds);
                IngestStep::Applied
            }
            None => IngestStep::Skipped,
        },
        Err(RecvError::Lagged(missed)) => {
            tracing::debug!(
                missed,
                "metrics-retention feed lagged; resubscribing at head"
            );
            *sub = sub.resubscribe();
            IngestStep::Lagged
        }
        Err(RecvError::Closed) => IngestStep::Closed,
    }
}

/// Run the local-metrics retention feed to completion.
///
/// Drains classified engine events into `store` until the engine is gone. This is
/// the long-lived off-hot-loop task the run spawns at startup; it owns one engine
/// subscription and a shared retention store, and times every recorded event with
/// the current Unix-epoch second (the wall clock sampled per event). It can never
/// block the engine (it only reads the drop-oldest broadcast and lagged-skips).
pub async fn run_metrics_retention(
    mut sub: EventSubscription<Event>,
    store: std::sync::Arc<RetentionStore>,
) {
    loop {
        let now = now_unix_seconds();
        match ingest_step(&mut sub, store.as_ref(), now).await {
            IngestStep::Closed => break,
            IngestStep::Applied | IngestStep::Skipped | IngestStep::Lagged => {}
        }
    }
}

/// The current Unix-epoch second from the wall clock, or `0` if the clock is
/// before the epoch (never panics). Public so the run wiring can stamp a final
/// retention summary against the same clock the feed uses.
#[must_use]
pub fn now_unix_seconds() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;
    use multiview_events::{GpuMetrics, GpuVendor};

    fn gpu(compute: f32) -> GpuMetrics {
        GpuMetrics {
            id: "g".to_owned(),
            vendor: GpuVendor::Nvidia,
            name: None,
            compute_util: compute,
            mem_used_bytes: 0,
            mem_total_bytes: 0,
            encoder_util: None,
            decoder_util: None,
            encoder_sessions: None,
            encoder_session_ceiling: None,
            self_compute_util: None,
            self_encoder_util: None,
            self_decoder_util: None,
            self_mem_used_bytes: None,
            self_encoder_sessions: None,
        }
    }

    #[test]
    fn gpu_util_is_the_max_over_devices() {
        let metrics = SystemMetrics {
            cpu_util: 0.1,
            mem_used_bytes: None,
            mem_total_bytes: None,
            self_cpu_util: None,
            self_mem_used_bytes: None,
            gpus: vec![gpu(0.2), gpu(0.7), gpu(0.5)],
            program_fps: None,
            sampled_hz: 1,
        };
        let sample = utilisation_from(&metrics);
        assert_eq!(sample.gpu_util, Some(0.7_f32.into()));
    }

    #[test]
    fn gpu_free_metrics_have_no_gpu_util() {
        let metrics = SystemMetrics {
            cpu_util: 0.1,
            mem_used_bytes: None,
            mem_total_bytes: None,
            self_cpu_util: None,
            self_mem_used_bytes: None,
            gpus: vec![],
            program_fps: None,
            sampled_hz: 1,
        };
        assert_eq!(utilisation_from(&metrics).gpu_util, None);
    }

    #[test]
    fn subsystem_maps_to_incident_kind() {
        assert_eq!(
            incident_kind_for_subsystem("encode"),
            IncidentKind::EncoderSaturation
        );
        assert_eq!(
            incident_kind_for_subsystem("output clock"),
            IncidentKind::ClockHoldover
        );
        assert_eq!(
            incident_kind_for_subsystem("compositor"),
            IncidentKind::InputFlap
        );
    }
}
